use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::sched::{CloneFlags, unshare};
use nix::unistd::{ForkResult, chdir, execve, fork, sethostname};

use crate::bundle::{self, Bundle};
use crate::error::{Error, Result};
use crate::rootfs;
use crate::sync::{Channel, Message};

use super::signal;

pub fn intermediate(
    bundle: &Bundle,
    rootfs_path: &Path,
    unshare_flags: CloneFlags,
    cgroup_ns: bool,
    channel: &Channel,
) -> ! {
    if let Err(error) = stage_one(bundle, rootfs_path, unshare_flags, cgroup_ns, channel) {
        let _ = channel.send(&Message::Failed(error.to_string()));
    }

    std::process::exit(1);
}

fn stage_one(
    bundle: &Bundle,
    rootfs_path: &Path,
    unshare_flags: CloneFlags,
    cgroup_ns: bool,
    channel: &Channel,
) -> Result<()> {
    unshare(unshare_flags)?;

    if unshare_flags.contains(CloneFlags::CLONE_NEWUSER) {
        channel.send(&Message::RequestUserMapping)?;
        channel.expect("UserMappingDone")?;
    }

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            channel.send(&Message::InitPid(child.as_raw()))?;
            std::process::exit(0);
        }
        ForkResult::Child => {
            if let Err(error) =
                container_init(bundle, rootfs_path, unshare_flags, cgroup_ns, channel)
            {
                let _ = channel.send(&Message::Failed(error.to_string()));
            }
            std::process::exit(1);
        }
    }
}

fn container_init(
    bundle: &Bundle,
    rootfs_path: &Path,
    unshare_flags: CloneFlags,
    cgroup_ns: bool,
    channel: &Channel,
) -> Result<()> {
    channel.expect("CgroupApplied")?;

    if cgroup_ns {
        unshare(CloneFlags::CLONE_NEWCGROUP)?;
    }

    if unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
        rootfs::pivot::make_root_private()?;
        rootfs::pivot::make_mount_point(rootfs_path)?;
        rootfs::mounts::apply(rootfs_path, &bundle.mounts())?;
        rootfs::devices::create(rootfs_path, &bundle.devices())?;
        rootfs::pivot::pivot(rootfs_path)?;
    }

    if let Some(hostname) = bundle.hostname() {
        sethostname(hostname)?;
    }

    let env = bundle.env();
    let argv = bundle.argv()?;
    let program = bundle::resolve_executable(&argv[0], &env)?;

    channel.send(&Message::InitReady)?;
    channel.expect("Start")?;

    match bundle.cwd() {
        Some(cwd) if !cwd.as_os_str().is_empty() => chdir(&cwd)?,
        _ => chdir("/")?,
    }

    signal::unblock_all()?;

    let program = CString::new(program.as_os_str().as_bytes())
        .map_err(|_| Error::NulByte(program.display().to_string()))?;
    let argv = bundle::to_cstrings(&argv)?;
    let env = bundle::to_cstrings(&env)?;

    execve(&program, &argv, &env)?;
    unreachable!()
}
