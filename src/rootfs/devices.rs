use std::path::Path;

use nix::sys::stat::{Mode, SFlag, makedev, mknod};
use oci_spec::runtime::{LinuxDevice, LinuxDeviceType};

use crate::error::{IoContext, NixContext, Result};

struct DefaultDevice {
    path: &'static str,
    major: u64,
    minor: u64,
    mode: u32,
}

const DEFAULT_DEVICES: [DefaultDevice; 6] = [
    DefaultDevice {
        path: "dev/null",
        major: 1,
        minor: 3,
        mode: 0o666,
    },
    DefaultDevice {
        path: "dev/zero",
        major: 1,
        minor: 5,
        mode: 0o666,
    },
    DefaultDevice {
        path: "dev/full",
        major: 1,
        minor: 7,
        mode: 0o666,
    },
    DefaultDevice {
        path: "dev/tty",
        major: 5,
        minor: 0,
        mode: 0o666,
    },
    DefaultDevice {
        path: "dev/random",
        major: 1,
        minor: 8,
        mode: 0o666,
    },
    DefaultDevice {
        path: "dev/urandom",
        major: 1,
        minor: 9,
        mode: 0o666,
    },
];

const SYMLINKS: [(&str, &str); 4] = [
    ("/proc/self/fd", "dev/fd"),
    ("/proc/self/fd/0", "dev/stdin"),
    ("/proc/self/fd/1", "dev/stdout"),
    ("/proc/self/fd/2", "dev/stderr"),
];

pub fn create(rootfs: &Path, extra: &[LinuxDevice]) -> Result<()> {
    for device in DEFAULT_DEVICES {
        node(
            &rootfs.join(device.path),
            SFlag::S_IFCHR,
            device.mode,
            device.major,
            device.minor,
        )?;
    }

    for (source, link) in SYMLINKS {
        let target = rootfs.join(link);

        if target.symlink_metadata().is_ok() {
            continue;
        }

        std::os::unix::fs::symlink(source, &target)
            .ctx(format!("symlink {} -> {source}", target.display()))?;
    }

    for device in extra {
        let target = crate::rootfs::mounts::resolve(rootfs, device.path());

        node(
            &target,
            sflag_for(device.typ()),
            device.file_mode().unwrap_or(0o666),
            device.major() as u64,
            device.minor() as u64,
        )?;
    }

    Ok(())
}

fn sflag_for(kind: LinuxDeviceType) -> SFlag {
    match kind {
        LinuxDeviceType::B => SFlag::S_IFBLK,
        LinuxDeviceType::P => SFlag::S_IFIFO,
        _ => SFlag::S_IFCHR,
    }
}

fn node(target: &Path, kind: SFlag, mode: u32, major: u64, minor: u64) -> Result<()> {
    if target.symlink_metadata().is_ok() {
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ctx(format!("create {}", parent.display()))?;
    }

    mknod(
        target,
        kind,
        Mode::from_bits_truncate(mode),
        makedev(major, minor),
    )
    .ctx(format!(
        "mknod {} {:?} {major}:{minor}",
        target.display(),
        kind
    ))
}
