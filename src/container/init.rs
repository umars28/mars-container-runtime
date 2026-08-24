use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

use nix::sched::{CloneFlags, unshare};
use nix::unistd::{ForkResult, chdir, execve, fork, sethostname};

use crate::bundle;
use crate::error::{Error, Result};
use crate::rootfs;
use crate::sync::{Channel, Message};

use super::Plan;
use super::signal;

pub fn intermediate(plan: &Plan, channel: &Channel) -> ! {
    if let Err(error) = stage_one(plan, channel) {
        let _ = channel.send(&Message::Failed(error.to_string()));
    }

    std::process::exit(1);
}

fn stage_one(plan: &Plan, channel: &Channel) -> Result<()> {
    unshare(plan.unshare_flags)?;

    if plan.unshare_flags.contains(CloneFlags::CLONE_NEWUSER) {
        channel.send(&Message::RequestUserMapping)?;
        channel.expect("UserMappingDone")?;
    }

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            channel.send(&Message::InitPid(child.as_raw()))?;
            std::process::exit(0);
        }
        ForkResult::Child => {
            if let Err(error) = container_init(plan, channel) {
                let _ = channel.send(&Message::Failed(error.to_string()));
            }
            std::process::exit(1);
        }
    }
}

fn container_init(plan: &Plan, channel: &Channel) -> Result<()> {
    channel.expect("CgroupApplied")?;

    if plan.cgroup_ns {
        unshare(CloneFlags::CLONE_NEWCGROUP)?;
    }

    if plan.unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
        rootfs::pivot::make_root_private()?;

        match &plan.overlay {
            Some(layers) => rootfs::overlay::mount_at(&plan.rootfs, layers)?,
            None => rootfs::pivot::make_mount_point(&plan.rootfs)?,
        }

        rootfs::mounts::apply(&plan.rootfs, &plan.bundle.mounts())?;
        rootfs::devices::create(&plan.rootfs, &plan.bundle.devices())?;
        rootfs::pivot::pivot(&plan.rootfs)?;
    }

    if let Some(hostname) = plan.bundle.hostname() {
        sethostname(hostname)?;
    }

    let env = plan.bundle.env();
    let argv = plan.bundle.argv()?;
    let program = bundle::resolve_executable(&argv[0], &env)?;

    channel.send(&Message::InitReady)?;
    channel.expect("Start")?;

    match plan.bundle.cwd() {
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
