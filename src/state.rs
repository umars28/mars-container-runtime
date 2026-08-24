use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use nix::unistd::Pid;
use serde::{Deserialize, Serialize};

use crate::cgroup::v2::Cgroup;
use crate::error::{Error, IoContext, Result};
use crate::paths::Layout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Creating,
    Created,
    Running,
    Stopped,
    Paused,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Status::Creating => "creating",
            Status::Created => "created",
            Status::Running => "running",
            Status::Stopped => "stopped",
            Status::Paused => "paused",
        };
        f.write_str(text)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persisted {
    pub oci_version: String,
    pub id: String,
    pub pid: i32,
    pub start_time: u64,
    pub bundle: PathBuf,
    pub rootfs: PathBuf,
    pub cgroup: PathBuf,
    pub created: String,
    pub rootless: bool,
    #[serde(default)]
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OciState {
    #[serde(rename = "ociVersion")]
    pub version: String,
    pub id: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    pub bundle: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub struct Container {
    pub layout: Layout,
    pub state: Persisted,
}

impl Container {
    pub fn load(layout: &Layout, id: &str) -> Result<Self> {
        let path = layout.state_file(id);

        if !path.is_file() {
            return Err(Error::NotFound(id.to_string()));
        }

        let text = fs::read_to_string(&path).ctx(format!("read {}", path.display()))?;
        let state: Persisted = serde_json::from_str(&text)?;

        Ok(Self {
            layout: layout.clone(),
            state,
        })
    }

    pub fn save(layout: &Layout, state: &Persisted) -> Result<()> {
        let dir = layout.container_dir(&state.id);
        fs::create_dir_all(&dir).ctx(format!("create {}", dir.display()))?;

        let path = layout.state_file(&state.id);
        let staged = dir.join("state.json.new");

        let text = serde_json::to_string_pretty(state)?;
        fs::write(&staged, &text).ctx(format!("write {}", staged.display()))?;
        fs::rename(&staged, &path).ctx(format!("rename {} into place", staged.display()))?;

        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.state.id
    }

    pub fn pid(&self) -> Pid {
        Pid::from_raw(self.state.pid)
    }

    pub fn exec_fifo(&self) -> PathBuf {
        self.layout.exec_fifo(self.id())
    }

    pub fn cgroup(&self) -> Cgroup {
        Cgroup::attach(&self.state.cgroup)
    }

    pub fn alive(&self) -> bool {
        matches!(
            start_time(self.pid()),
            Ok(observed) if observed == self.state.start_time
        )
    }

    pub fn status(&self) -> Status {
        if !self.alive() {
            return Status::Stopped;
        }

        if self.cgroup().is_frozen() {
            return Status::Paused;
        }

        if self.exec_fifo().exists() {
            Status::Created
        } else {
            Status::Running
        }
    }

    pub fn oci_state(&self) -> OciState {
        let status = self.status();

        OciState {
            version: self.state.oci_version.clone(),
            id: self.state.id.clone(),
            status,
            pid: match status {
                Status::Stopped => None,
                _ => Some(self.state.pid),
            },
            bundle: self.state.bundle.clone(),
            annotations: if self.state.annotations.is_empty() {
                None
            } else {
                Some(self.state.annotations.clone())
            },
        }
    }

    pub fn require(&self, expected: &[Status]) -> Result<Status> {
        let actual = self.status();

        if expected.contains(&actual) {
            return Ok(actual);
        }

        let expected = expected
            .iter()
            .map(Status::to_string)
            .collect::<Vec<_>>()
            .join(" or ");

        Err(Error::BadState {
            id: self.state.id.clone(),
            actual: actual.to_string(),
            expected,
        })
    }

    pub fn remove(&self) -> Result<()> {
        let dir = self.layout.container_dir(self.id());

        if dir.exists() {
            fs::remove_dir_all(&dir).ctx(format!("remove {}", dir.display()))?;
        }

        Ok(())
    }
}

pub fn list(layout: &Layout) -> Result<Vec<Container>> {
    let root = layout.root();

    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();

    for entry in fs::read_dir(root).ctx(format!("read {}", root.display()))? {
        let entry = entry.ctx(format!("read an entry of {}", root.display()))?;

        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };

        if let Ok(container) = Container::load(layout, &id) {
            found.push(container);
        }
    }

    found.sort_by(|a, b| a.state.created.cmp(&b.state.created));

    Ok(found)
}

pub fn start_time(pid: Pid) -> Result<u64> {
    let path = format!("/proc/{}/stat", pid.as_raw());
    let text = fs::read_to_string(&path).ctx(format!("read {path}"))?;

    parse_start_time(&text).ok_or_else(|| Error::Invalid(format!("cannot parse {path}")))
}

pub fn parse_start_time(stat: &str) -> Option<u64> {
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(19)?.parse().ok()
}

pub fn now_rfc3339() -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    let secs = elapsed.as_secs() as i64;
    let (year, month, day, hour, minute, second) = civil_from_unix(secs);

    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:09}Z",
        elapsed.subsec_nanos()
    )
}

fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };

    (
        if m <= 2 { y + 1 } else { y },
        m as u32,
        d as u32,
        (rem / 3_600) as u32,
        (rem % 3_600 / 60) as u32,
        (rem % 60) as u32,
    )
}

pub fn write_pid_file(path: Option<&Path>, pid: Pid) -> Result<()> {
    let Some(path) = path else { return Ok(()) };

    fs::write(path, pid.as_raw().to_string()).ctx(format!("write pid file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_time_is_field_22_of_proc_stat() {
        let stat = "4242 (bash) S 1 4242 4242 0 -1 4194304 3273 12175 0 1 12 6 27 12 20 0 1 0 \
                    918273 12345 678 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(918_273));
    }

    #[test]
    fn a_command_name_containing_spaces_and_parens_does_not_shift_the_fields() {
        let stat = "77 (my (odd) name) S 1 77 77 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 555 1 2 3";
        assert_eq!(parse_start_time(stat), Some(555));
    }

    #[test]
    fn our_own_start_time_is_readable_and_stable() {
        let mine = start_time(nix::unistd::getpid()).unwrap();
        assert!(mine > 0);
        assert_eq!(mine, start_time(nix::unistd::getpid()).unwrap());
    }

    #[test]
    fn a_status_serialises_to_the_lowercase_name_the_spec_uses() {
        assert_eq!(
            serde_json::to_string(&Status::Created).unwrap(),
            "\"created\""
        );
        assert_eq!(Status::Stopped.to_string(), "stopped");
    }

    #[test]
    fn a_stopped_container_reports_no_pid() {
        let state = OciState {
            version: "1.0.2".into(),
            id: "x".into(),
            status: Status::Stopped,
            pid: None,
            bundle: PathBuf::from("/b"),
            annotations: None,
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("pid"), "{json}");
        assert!(json.contains("\"ociVersion\":\"1.0.2\""), "{json}");
    }

    #[test]
    fn timestamps_are_rfc3339_in_utc() {
        let stamp = now_rfc3339();
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(
            stamp.len(),
            "2026-08-24T09:13:07.872476000Z".len(),
            "{stamp}"
        );
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
    }

    #[test]
    fn the_epoch_converts_to_the_expected_civil_date() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        assert_eq!(civil_from_unix(1_772_000_000), (2026, 2, 25, 6, 13, 20));
    }
}
