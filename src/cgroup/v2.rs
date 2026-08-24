use std::fs;
use std::path::{Component, Path, PathBuf};

use nix::unistd::Pid;
use oci_spec::runtime::LinuxResources;

use crate::cgroup::events;
use crate::error::{Error, IoContext, Result};

pub const MOUNT_POINT: &str = "/sys/fs/cgroup";
pub const DEFAULT_PARENT: &str = "mars";

const WANTED: [&str; 5] = ["memory", "pids", "cpu", "cpuset", "io"];

#[derive(Debug)]
pub struct Cgroup {
    path: PathBuf,
    relative: PathBuf,
}

pub fn is_unified() -> bool {
    nix::sys::statfs::statfs(MOUNT_POINT)
        .map(|stat| stat.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC)
        .unwrap_or(false)
}

pub fn relative_path(cgroups_path: Option<&Path>, id: &str) -> PathBuf {
    match cgroups_path {
        Some(path) if !path.as_os_str().is_empty() => normalise(path),
        _ => PathBuf::from(DEFAULT_PARENT).join(id),
    }
}

fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();

    for component in path.components() {
        if let Component::Normal(part) = component {
            out.push(part);
        }
    }

    out
}

impl Cgroup {
    pub fn create(relative: &Path) -> Result<Self> {
        if !is_unified() {
            return Err(Error::NoCgroupV2);
        }

        let mut current = PathBuf::from(MOUNT_POINT);
        let parts: Vec<_> = relative.components().collect();

        for (index, component) in parts.iter().enumerate() {
            let Component::Normal(part) = component else {
                continue;
            };

            let is_leaf = index + 1 == parts.len();
            delegate(&current)?;

            current.push(part);

            if !current.is_dir() {
                fs::create_dir(&current).ctx(format!("create cgroup {}", current.display()))?;
            }

            if is_leaf {
                break;
            }
        }

        Ok(Self {
            path: current,
            relative: relative.to_path_buf(),
        })
    }

    pub fn attach(relative: &Path) -> Self {
        Self {
            path: Path::new(MOUNT_POINT).join(relative),
            relative: relative.to_path_buf(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    pub fn procs(&self) -> Result<Vec<Pid>> {
        let text = self.read("cgroup.procs")?;

        Ok(text
            .lines()
            .filter_map(|line| line.trim().parse::<i32>().ok())
            .map(Pid::from_raw)
            .collect())
    }

    pub fn freeze(&self, frozen: bool) -> Result<()> {
        self.write("cgroup.freeze", if frozen { "1" } else { "0" })?;

        for _ in 0..500 {
            if self.is_frozen() == frozen {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        Err(Error::Invalid(format!(
            "{} did not reach frozen={frozen} within 5s",
            self.path.display()
        )))
    }

    pub fn is_frozen(&self) -> bool {
        self.read("cgroup.events")
            .map(|text| {
                events::parse_flat_keyed(&text)
                    .get("frozen")
                    .copied()
                    .unwrap_or(0)
                    == 1
            })
            .unwrap_or(false)
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }

    pub fn add_process(&self, pid: Pid) -> Result<()> {
        self.write("cgroup.procs", &pid.as_raw().to_string())
    }

    pub fn destroy(&self) -> Result<()> {
        for attempt in 0..50 {
            match fs::remove_dir(&self.path) {
                Ok(()) => break,
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) && attempt < 49 => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(Error::Io {
                        context: format!("remove cgroup {}", self.path.display()),
                        source: error,
                    });
                }
            }
        }

        if let Some(parent) = self.path.parent() {
            if parent != Path::new(MOUNT_POINT) {
                let _ = fs::remove_dir(parent);
            }
        }

        Ok(())
    }

    pub fn write(&self, file: &str, value: &str) -> Result<()> {
        let target = self.path.join(file);
        fs::write(&target, value).ctx(format!("write {:?} to {}", value, target.display()))
    }

    pub fn read(&self, file: &str) -> Result<String> {
        let target = self.path.join(file);
        fs::read_to_string(&target).ctx(format!("read {}", target.display()))
    }

    pub fn apply(&self, resources: &LinuxResources) -> Result<()> {
        if let Some(memory) = resources.memory() {
            let limit = memory.limit();

            if let Some(bytes) = limit {
                self.write("memory.max", &limit_value(bytes))?;
            }
            if let Some(bytes) = memory.reservation() {
                self.write("memory.low", &limit_value(bytes))?;
            }
            if let Some(swap) = memory.swap() {
                self.write("memory.swap.max", &swap_value(swap, limit))?;
            }
        }

        if let Some(cpu) = resources.cpu() {
            let period = cpu.period().unwrap_or(100_000);

            if let Some(quota) = cpu.quota() {
                self.write("cpu.max", &cpu_max_value(quota, period))?;
            }
            if let Some(shares) = cpu.shares() {
                if shares != 0 {
                    self.write("cpu.weight", &shares_to_weight(shares).to_string())?;
                }
            }
            if let Some(cpus) = cpu.cpus() {
                if !cpus.is_empty() {
                    self.write("cpuset.cpus", cpus)?;
                }
            }
            if let Some(mems) = cpu.mems() {
                if !mems.is_empty() {
                    self.write("cpuset.mems", mems)?;
                }
            }
        }

        if let Some(pids) = resources.pids() {
            self.write("pids.max", &limit_value(pids.limit()))?;
        }

        if let Some(block_io) = resources.block_io() {
            if let Some(weight) = block_io.weight() {
                if weight != 0 {
                    self.write("io.weight", &blkio_to_io_weight(weight).to_string())?;
                }
            }

            for line in io_max_lines(block_io) {
                self.write("io.max", &line)?;
            }
        }

        if let Some(unified) = resources.unified() {
            for (file, value) in unified {
                self.write(file, value)?;
            }
        }

        Ok(())
    }
}

fn delegate(cgroup: &Path) -> Result<()> {
    let available = read_list(&cgroup.join("cgroup.controllers"))?;
    let enabled = read_list(&cgroup.join("cgroup.subtree_control")).unwrap_or_default();

    let missing: Vec<String> = WANTED
        .iter()
        .filter(|wanted| {
            available.iter().any(|have| have == *wanted)
                && !enabled.iter().any(|have| have == *wanted)
        })
        .map(|wanted| format!("+{wanted}"))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let target = cgroup.join("cgroup.subtree_control");
    fs::write(&target, missing.join(" ")).ctx(format!(
        "enable controllers {:?} in {}",
        missing,
        target.display()
    ))
}

fn read_list(path: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).ctx(format!("read {}", path.display()))?;
    Ok(text.split_whitespace().map(str::to_string).collect())
}

pub fn limit_value(bytes: i64) -> String {
    if bytes <= 0 {
        "max".to_string()
    } else {
        bytes.to_string()
    }
}

pub fn swap_value(swap: i64, limit: Option<i64>) -> String {
    match (swap, limit) {
        (s, _) if s <= 0 => "max".to_string(),
        (s, Some(l)) if l > 0 && s > l => (s - l).to_string(),
        (_, Some(l)) if l > 0 => "0".to_string(),
        (s, _) => s.to_string(),
    }
}

pub fn cpu_max_value(quota: i64, period: u64) -> String {
    if quota <= 0 {
        format!("max {period}")
    } else {
        format!("{quota} {period}")
    }
}

pub fn shares_to_weight(shares: u64) -> u64 {
    if shares == 0 {
        return 100;
    }

    let shares = shares.clamp(2, 262_144);
    1 + ((shares - 2) * 9_999) / 262_142
}

pub fn blkio_to_io_weight(weight: u16) -> u64 {
    let weight = u64::from(weight).clamp(10, 1_000);
    1 + (weight - 10) * 9_999 / 990
}

fn io_max_lines(block_io: &oci_spec::runtime::LinuxBlockIo) -> Vec<String> {
    let mut lines = Vec::new();

    let groups: [(&str, &Option<Vec<oci_spec::runtime::LinuxThrottleDevice>>); 4] = [
        ("rbps", block_io.throttle_read_bps_device()),
        ("wbps", block_io.throttle_write_bps_device()),
        ("riops", block_io.throttle_read_iops_device()),
        ("wiops", block_io.throttle_write_iops_device()),
    ];

    for (key, devices) in groups {
        for device in devices.iter().flatten() {
            lines.push(format!(
                "{}:{} {}={}",
                device.major(),
                device.minor(),
                key,
                device.rate()
            ));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_becomes_max() {
        assert_eq!(limit_value(-1), "max");
        assert_eq!(limit_value(0), "max");
        assert_eq!(limit_value(134_217_728), "134217728");
    }

    #[test]
    fn swap_is_converted_from_v1_total_to_v2_swap_only() {
        assert_eq!(swap_value(200, Some(100)), "100");
        assert_eq!(swap_value(100, Some(100)), "0");
        assert_eq!(swap_value(-1, Some(100)), "max");
        assert_eq!(swap_value(200, None), "200");
    }

    #[test]
    fn cpu_max_is_quota_and_period() {
        assert_eq!(cpu_max_value(50_000, 100_000), "50000 100000");
        assert_eq!(cpu_max_value(-1, 100_000), "max 100000");
    }

    #[test]
    fn shares_map_onto_the_v2_weight_range() {
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(1024), 39);
        assert_eq!(shares_to_weight(262_144), 10_000);
    }

    #[test]
    fn blkio_weight_maps_onto_the_v2_io_weight_range() {
        assert_eq!(blkio_to_io_weight(10), 1);
        assert_eq!(blkio_to_io_weight(1000), 10_000);
    }

    #[test]
    fn default_path_is_used_when_the_spec_is_silent() {
        assert_eq!(relative_path(None, "abc"), PathBuf::from("mars/abc"));
        assert_eq!(
            relative_path(Some(Path::new("")), "abc"),
            PathBuf::from("mars/abc")
        );
    }

    #[test]
    fn spec_path_is_made_relative_and_cannot_escape() {
        assert_eq!(
            relative_path(Some(Path::new("/system.slice/demo")), "abc"),
            PathBuf::from("system.slice/demo")
        );
        assert_eq!(
            relative_path(Some(Path::new("/../../escape")), "abc"),
            PathBuf::from("escape")
        );
    }
}
