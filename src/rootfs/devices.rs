use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::stat::{Mode, SFlag, makedev, mknod};
use nix::unistd::{Gid, Uid, chown};
use oci_spec::runtime::{LinuxDevice, LinuxDeviceType};

use crate::error::{Error, IoContext, NixContext, Result};

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

pub fn create(rootfs: &Path, extra: &[LinuxDevice], user_namespace: bool) -> Result<()> {
    for device in DEFAULT_DEVICES {
        node(
            &rootfs.join(device.path),
            Path::new("/").join(device.path),
            SFlag::S_IFCHR,
            device.mode,
            device.major,
            device.minor,
            user_namespace,
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
            device.path().clone(),
            sflag_for(device.typ()),
            device.file_mode().unwrap_or(0o666),
            device.major() as u64,
            device.minor() as u64,
            user_namespace,
        )?;

        if device.uid().is_some() || device.gid().is_some() {
            chown(
                &target,
                device.uid().map(Uid::from_raw),
                device.gid().map(Gid::from_raw),
            )
            .ctx(format!(
                "chown {} to {:?}:{:?}",
                target.display(),
                device.uid(),
                device.gid()
            ))?;
        }
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

fn node(
    target: &Path,
    host: PathBuf,
    kind: SFlag,
    mode: u32,
    major: u64,
    minor: u64,
    user_namespace: bool,
) -> Result<()> {
    if target.symlink_metadata().is_ok() {
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ctx(format!("create {}", parent.display()))?;
    }

    if user_namespace {
        return bind(target, &host);
    }

    match mknod(
        target,
        kind,
        Mode::from_bits_truncate(mode),
        makedev(major, minor),
    ) {
        Ok(()) => {}
        Err(Errno::EPERM | Errno::EOVERFLOW) => return bind(target, &host),
        Err(source) => {
            return Err(Error::Nix {
                context: format!("mknod {} {kind:?} {major}:{minor}", target.display()),
                source,
            });
        }
    }

    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode)).ctx(format!(
        "chmod {} to {mode:04o}; mknod(2) masks its mode argument with the umask, so the node \
         would otherwise get {:04o}",
        target.display(),
        mode & !0o022
    ))
}

fn bind(target: &Path, host: &Path) -> Result<()> {
    if !host.exists() {
        return Err(Error::Invalid(format!(
            "cannot provide {} because the host has no {}",
            target.display(),
            host.display()
        )));
    }

    if !target.exists() {
        std::fs::File::create(target)
            .ctx(format!("create {} as a bind target", target.display()))?;
    }

    nix::mount::mount(
        Some(host),
        target,
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .ctx(format!(
        "bind {} onto {}; a user namespace may not call mknod(2) for a device at all, so the node \
         has to come from the host",
        host.display(),
        target.display()
    ))
}
