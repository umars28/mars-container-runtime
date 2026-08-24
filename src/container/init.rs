use std::ffi::CString;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;

use nix::fcntl::{OFlag, open};
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::stat::Mode;
use nix::unistd::{ForkResult, chdir, execve, fork, sethostname};

use crate::bundle;
use crate::error::{Error, NixContext, Result};
use crate::namespace;
use crate::rootfs;
use crate::state::Status;
use crate::sync::{Channel, Message};

use super::{Plan, capabilities, console, fifo, hooks, process, signal};

pub fn intermediate(plan: &Plan, channel: &Channel) -> ! {
    if let Err(error) = stage_one(plan, channel) {
        let _ = channel.send(&Message::Failed(error.to_string()));
    }

    std::process::exit(1);
}

fn stage_one(plan: &Plan, channel: &Channel) -> Result<()> {
    join_existing(plan)?;

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
                eprintln!("mars: container init failed: {error}");
            }
            std::process::exit(1);
        }
    }
}

fn join_existing(plan: &Plan) -> Result<()> {
    for (kind, path) in &plan.join {
        let flag = namespace::flag_for(*kind)?;

        let handle = open(path, OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty())
            .ctx(format!("open {} to join it", path.display()))?;

        setns(handle.as_fd(), flag).ctx(format!(
            "setns into the {kind:?} namespace at {}",
            path.display()
        ))?;
    }

    Ok(())
}

fn container_init(plan: &Plan, channel: &Channel) -> Result<()> {
    let host_pid = match channel.expect("CgroupApplied")? {
        Message::CgroupApplied(pid) => pid,
        other => {
            return Err(Error::Sync(format!(
                "expected CgroupApplied, got {other:?}"
            )));
        }
    };

    if plan.cgroup_ns {
        unshare(CloneFlags::CLONE_NEWCGROUP)?;
    }

    let mut state = plan.state.clone();
    state.pid = Some(host_pid);

    if plan.unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
        rootfs::pivot::make_root_private()?;

        match &plan.overlay {
            Some(layers) => rootfs::overlay::mount_at(&plan.rootfs, layers)?,
            None => rootfs::pivot::make_mount_point(&plan.rootfs)?,
        }

        rootfs::mounts::apply(&plan.rootfs, &plan.bundle.mounts())?;
        rootfs::devices::create(&plan.rootfs, &plan.bundle.devices())?;

        hooks::run(plan.bundle.hooks(), hooks::Phase::CreateContainer, &state)?;

        rootfs::pivot::pivot(&plan.rootfs)?;
        rootfs::pivot::set_propagation(plan.bundle.rootfs_propagation())?;

        if let Some(socket) = &plan.console {
            console::setup(socket)?;
        }
    } else {
        hooks::run(plan.bundle.hooks(), hooks::Phase::CreateContainer, &state)?;
    }

    if let Some(hostname) = plan.bundle.hostname() {
        sethostname(hostname)?;
    }

    if let Some(domainname) = plan.bundle.domainname() {
        set_domainname(domainname)?;
    }

    rootfs::sysctl::apply(&plan.bundle.sysctl())?;

    let env = plan.bundle.env();
    let argv = plan.bundle.argv()?;
    let program = bundle::resolve_executable(
        &argv[0],
        &env,
        plan.unshare_flags.contains(CloneFlags::CLONE_NEWNS),
    )?;

    let program = CString::new(program.as_os_str().as_bytes())
        .map_err(|_| Error::NulByte(program.display().to_string()))?;
    let argv = bundle::to_cstrings(&argv)?;
    let env = bundle::to_cstrings(&env)?;

    if let Some(spec) = plan.bundle.process() {
        process::apply_limits(spec)?;
        process::apply_oom_score_adj(spec)?;
        process::apply_umask(spec);
    }

    match plan.bundle.cwd() {
        Some(cwd) if !cwd.as_os_str().is_empty() => chdir(&cwd)?,
        _ => chdir("/")?,
    }

    if let Some(spec) = plan.bundle.process() {
        if let Some(caps) = spec.capabilities() {
            capabilities::drop_bounding(caps)?;
        }

        process::drop_privileges(spec)?;

        if let Some(caps) = spec.capabilities() {
            capabilities::apply(caps)?;
        }

        if spec.no_new_privileges().unwrap_or(false) {
            capabilities::set_no_new_privs()?;
        }
    }

    channel.send(&Message::InitReady)?;

    fifo::park(&plan.fifo)?;

    state.status = Status::Running;
    hooks::run(plan.bundle.hooks(), hooks::Phase::StartContainer, &state)?;

    signal::unblock_all()?;

    execve(&program, &argv, &env)?;
    unreachable!()
}

fn set_domainname(name: &str) -> Result<()> {
    let rc = unsafe { libc::setdomainname(name.as_ptr().cast(), name.len()) };

    if rc != 0 {
        return Err(Error::Nix {
            context: format!("setdomainname({name:?})"),
            source: nix::Error::last(),
        });
    }

    Ok(())
}
