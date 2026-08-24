use std::fs;
use std::path::{Component, Path, PathBuf};

use nix::mount::{MsFlags, mount};
use oci_spec::runtime::Mount as SpecMount;

use crate::error::{IoContext, NixContext, Result};

#[derive(Debug)]
pub struct Options {
    pub flags: MsFlags,
    pub data: String,
}

pub fn parse_options(options: &[String]) -> Options {
    let mut flags = MsFlags::empty();
    let mut data: Vec<&str> = Vec::new();

    for option in options {
        match option.as_str() {
            "defaults" => {}
            "ro" => flags |= MsFlags::MS_RDONLY,
            "rw" => flags &= !MsFlags::MS_RDONLY,
            "suid" => flags &= !MsFlags::MS_NOSUID,
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "dev" => flags &= !MsFlags::MS_NODEV,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "exec" => flags &= !MsFlags::MS_NOEXEC,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
            "async" => flags &= !MsFlags::MS_SYNCHRONOUS,
            "dirsync" => flags |= MsFlags::MS_DIRSYNC,
            "remount" => flags |= MsFlags::MS_REMOUNT,
            "mand" => flags |= MsFlags::MS_MANDLOCK,
            "nomand" => flags &= !MsFlags::MS_MANDLOCK,
            "atime" => flags &= !MsFlags::MS_NOATIME,
            "noatime" => flags |= MsFlags::MS_NOATIME,
            "diratime" => flags &= !MsFlags::MS_NODIRATIME,
            "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
            "relatime" => flags |= MsFlags::MS_RELATIME,
            "norelatime" => flags &= !MsFlags::MS_RELATIME,
            "strictatime" => flags |= MsFlags::MS_STRICTATIME,
            "nostrictatime" => flags &= !MsFlags::MS_STRICTATIME,
            "bind" => flags |= MsFlags::MS_BIND,
            "rbind" => flags |= MsFlags::MS_BIND | MsFlags::MS_REC,
            "unbindable" => flags |= MsFlags::MS_UNBINDABLE,
            "runbindable" => flags |= MsFlags::MS_UNBINDABLE | MsFlags::MS_REC,
            "private" => flags |= MsFlags::MS_PRIVATE,
            "rprivate" => flags |= MsFlags::MS_PRIVATE | MsFlags::MS_REC,
            "shared" => flags |= MsFlags::MS_SHARED,
            "rshared" => flags |= MsFlags::MS_SHARED | MsFlags::MS_REC,
            "slave" => flags |= MsFlags::MS_SLAVE,
            "rslave" => flags |= MsFlags::MS_SLAVE | MsFlags::MS_REC,
            other => data.push(other),
        }
    }

    Options {
        flags,
        data: data.join(","),
    }
}

pub fn resolve(rootfs: &Path, destination: &Path) -> PathBuf {
    let mut out = rootfs.to_path_buf();

    for component in destination.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if out != rootfs {
                    out.pop();
                }
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }

    out
}

pub fn unified_cgroup_host() -> bool {
    nix::sys::statfs::statfs("/sys/fs/cgroup")
        .map(|stat| stat.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC)
        .unwrap_or(false)
}

pub fn effective_fstype(requested: &str, unified_host: bool) -> &str {
    if requested == "cgroup" && unified_host {
        "cgroup2"
    } else {
        requested
    }
}

pub fn apply(rootfs: &Path, mounts: &[SpecMount]) -> Result<()> {
    let unified = unified_cgroup_host();

    for spec_mount in mounts {
        apply_one(rootfs, spec_mount, unified)?;
    }

    Ok(())
}

fn apply_one(rootfs: &Path, spec_mount: &SpecMount, unified_host: bool) -> Result<()> {
    let destination = resolve(rootfs, spec_mount.destination());
    let requested = spec_mount.typ().clone().unwrap_or_else(|| "none".into());
    let fstype = effective_fstype(&requested, unified_host).to_string();
    let source = spec_mount
        .source()
        .clone()
        .unwrap_or_else(|| PathBuf::from("none"));

    let options = parse_options(spec_mount.options().as_deref().unwrap_or(&[]));
    let bind = options.flags.contains(MsFlags::MS_BIND);

    if bind && source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).ctx(format!("create {}", parent.display()))?;
        }
        if !destination.exists() {
            fs::File::create(&destination).ctx(format!("create {}", destination.display()))?;
        }
    } else {
        fs::create_dir_all(&destination).ctx(format!("create {}", destination.display()))?;
    }

    let data = if options.data.is_empty() {
        None
    } else {
        Some(options.data.as_str())
    };

    mount(
        Some(source.as_path()),
        destination.as_path(),
        Some(fstype.as_str()),
        options.flags,
        data,
    )
    .ctx(format!(
        "mount {} type={} at {} flags={:?} data={:?}",
        source.display(),
        fstype,
        spec_mount.destination().display(),
        options.flags,
        options.data,
    ))?;

    if fstype == "devpts" {
        link_ptmx(rootfs)?;
    }

    Ok(())
}

fn link_ptmx(rootfs: &Path) -> Result<()> {
    let ptmx = rootfs.join("dev/ptmx");

    if ptmx.symlink_metadata().is_ok() {
        fs::remove_file(&ptmx).ctx(format!("remove {}", ptmx.display()))?;
    }

    std::os::unix::fs::symlink("pts/ptmx", &ptmx)
        .ctx(format!("symlink {} -> pts/ptmx", ptmx.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flag_options_and_keeps_data() {
        let parsed = parse_options(&[
            "nosuid".into(),
            "noexec".into(),
            "nodev".into(),
            "mode=1777".into(),
            "size=65536k".into(),
        ]);

        assert!(parsed.flags.contains(MsFlags::MS_NOSUID));
        assert!(parsed.flags.contains(MsFlags::MS_NOEXEC));
        assert!(parsed.flags.contains(MsFlags::MS_NODEV));
        assert_eq!(parsed.data, "mode=1777,size=65536k");
    }

    #[test]
    fn later_options_override_earlier_ones() {
        let parsed = parse_options(&["ro".into(), "rw".into()]);
        assert!(!parsed.flags.contains(MsFlags::MS_RDONLY));

        let parsed = parse_options(&["rw".into(), "ro".into()]);
        assert!(parsed.flags.contains(MsFlags::MS_RDONLY));
    }

    #[test]
    fn rbind_implies_recursive() {
        let parsed = parse_options(&["rbind".into()]);
        assert!(parsed.flags.contains(MsFlags::MS_BIND));
        assert!(parsed.flags.contains(MsFlags::MS_REC));
    }

    #[test]
    fn legacy_cgroup_is_translated_on_unified_hosts() {
        assert_eq!(effective_fstype("cgroup", true), "cgroup2");
        assert_eq!(effective_fstype("cgroup", false), "cgroup");
        assert_eq!(effective_fstype("cgroup2", true), "cgroup2");
        assert_eq!(effective_fstype("proc", true), "proc");
        assert_eq!(effective_fstype("tmpfs", true), "tmpfs");
    }

    #[test]
    fn destination_cannot_escape_rootfs() {
        let rootfs = Path::new("/tmp/rootfs");

        assert_eq!(
            resolve(rootfs, Path::new("/proc")),
            PathBuf::from("/tmp/rootfs/proc")
        );
        assert_eq!(
            resolve(rootfs, Path::new("/../../etc/shadow")),
            PathBuf::from("/tmp/rootfs/etc/shadow")
        );
        assert_eq!(
            resolve(rootfs, Path::new("/a/../../../b")),
            PathBuf::from("/tmp/rootfs/b")
        );
    }
}
