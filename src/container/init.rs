use std::ffi::CString;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;

use nix::fcntl::{OFlag, open};
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::stat::Mode;
use nix::unistd::{ForkResult, Gid, Uid, chdir, execve, fork, sethostname, setresgid, setresuid};

use crate::bundle;
use crate::error::{Error, IoContext, NixContext, Result};
use crate::namespace;
use crate::rootfs;
use crate::state::Status;
use crate::sync::{Channel, Message};
use crate::telemetry;

use super::{Plan, capabilities, console, fifo, hooks, process, seccomp, signal};

pub fn intermediate(plan: &Plan, channel: &Channel) -> ! {
    if let Err(error) = stage_one(plan, channel) {
        let _ = channel.send(&Message::Failed(error.to_string()));
    }

    std::process::exit(1);
}

fn stage_one(plan: &Plan, channel: &Channel) -> Result<()> {
    let mut clock = telemetry::Recorder::new();

    clock.begin("join");
    join_existing(plan)?;

    for (name, flag) in unshare_order() {
        if !plan.unshare_flags.contains(flag) {
            continue;
        }

        clock.begin(&format!("unshare.{name}"));
        unshare(flag)?;

        if name == "time" {
            write_time_offsets(&plan.bundle.time_offsets())?;
        }
    }

    if plan.unshare_flags.contains(CloneFlags::CLONE_NEWUSER) {
        clock.begin("unshare.user.map");
        channel.send(&Message::RequestUserMapping)?;
        channel.expect("UserMappingDone")?;
        become_root_in_namespace()?;
    }

    clock.begin("fork.init");

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            channel.send(&Message::InitPid(child.as_raw(), clock.take()))?;
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

fn unshare_order() -> [(&'static str, CloneFlags); 7] {
    [
        ("user", CloneFlags::CLONE_NEWUSER),
        ("mnt", CloneFlags::CLONE_NEWNS),
        ("uts", CloneFlags::CLONE_NEWUTS),
        ("ipc", CloneFlags::CLONE_NEWIPC),
        ("pid", CloneFlags::CLONE_NEWPID),
        ("net", CloneFlags::CLONE_NEWNET),
        (
            "time",
            CloneFlags::from_bits_retain(namespace::CLONE_NEWTIME),
        ),
    ]
}

fn write_time_offsets(
    offsets: &std::collections::HashMap<String, oci_spec::runtime::LinuxTimeOffset>,
) -> Result<()> {
    if offsets.is_empty() {
        return Ok(());
    }

    let mut body = String::new();

    for (clock, offset) in offsets {
        body.push_str(&format!(
            "{clock} {} {}\n",
            offset.secs().unwrap_or(0),
            offset.nanosecs().unwrap_or(0)
        ));
    }

    std::fs::write("/proc/self/timens_offsets", &body).ctx(format!(
        "write {body:?} to /proc/self/timens_offsets; the offsets can only be set before the \
         namespace has any other member, which is why this happens here and not in the init"
    ))
}

fn become_root_in_namespace() -> Result<()> {
    setresgid(Gid::from_raw(0), Gid::from_raw(0), Gid::from_raw(0)).ctx(
        "setresgid(0,0,0) inside the new user namespace; creating one leaves the caller's own id \
         unmapped, so it reads as the overflow id and every file operation fails with EOVERFLOW \
         until we move into the mapping",
    )?;

    setresuid(Uid::from_raw(0), Uid::from_raw(0), Uid::from_raw(0))
        .ctx("setresuid(0,0,0) inside the new user namespace")?;

    Ok(())
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

    let mut clock = telemetry::Recorder::new();
    clock.begin("unshare.cgroup");

    if plan.cgroup_ns {
        unshare(CloneFlags::CLONE_NEWCGROUP)?;
    }

    let mut state = plan.state.clone();
    state.pid = Some(host_pid);

    if plan.unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
        clock.begin("rootfs.mount");
        rootfs::pivot::make_root_private()?;

        match &plan.overlay {
            Some(layers) => rootfs::overlay::mount_at(&plan.rootfs, layers)?,
            None => rootfs::pivot::make_mount_point(&plan.rootfs)?,
        }

        rootfs::mounts::apply(&plan.rootfs, &plan.bundle.mounts())?;
        rootfs::devices::create(
            &plan.rootfs,
            &plan.bundle.devices(),
            plan.unshare_flags.contains(CloneFlags::CLONE_NEWUSER) || !plan.join.is_empty(),
        )?;

        clock.begin("hooks.createContainer");
        hooks::run(plan.bundle.hooks(), hooks::Phase::CreateContainer, &state)?;

        clock.begin("pivot_root");
        rootfs::pivot::pivot(&plan.rootfs)?;
        rootfs::pivot::set_propagation(plan.bundle.rootfs_propagation())?;

        if let Some(socket) = &plan.console {
            console::setup(socket)?;
        }
    } else {
        hooks::run(plan.bundle.hooks(), hooks::Phase::CreateContainer, &state)?;
    }

    clock.begin("identity");

    if let Some(hostname) = plan.bundle.hostname() {
        sethostname(hostname)?;
    }

    if let Some(domainname) = plan.bundle.domainname() {
        set_domainname(domainname)?;
    }

    rootfs::sysctl::apply(&plan.bundle.sysctl())?;

    if plan.unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
        clock.begin("harden");
        rootfs::harden::mask_paths(&plan.bundle.masked_paths())?;
        rootfs::harden::readonly_paths(&plan.bundle.readonly_paths())?;
        rootfs::harden::remount_readonly_mounts(&plan.bundle.mounts())?;

        if plan.bundle.readonly_rootfs() {
            rootfs::harden::readonly_rootfs()?;
        }
    }

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

    let no_new_privs = plan
        .bundle
        .process()
        .and_then(|spec| spec.no_new_privileges())
        .unwrap_or(false);

    clock.begin("security");

    if !no_new_privs {
        if let Some(spec) = plan.bundle.seccomp() {
            let rules = seccomp::apply(spec)?;
            tracing::debug!(
                rules,
                "seccomp loaded before dropping capabilities, because without no_new_privs the \
                 seccomp(2) call needs CAP_SYS_ADMIN"
            );
        }
    }

    if let Some(spec) = plan.bundle.process() {
        if let Some(caps) = spec.capabilities() {
            capabilities::drop_bounding(caps)?;
        }

        process::drop_privileges(spec)?;

        if let Some(caps) = spec.capabilities() {
            capabilities::apply(caps)?;
        }

        if no_new_privs {
            capabilities::set_no_new_privs()?;
        }
    }

    if no_new_privs {
        if let Some(spec) = plan.bundle.seccomp() {
            let rules = seccomp::apply(spec)?;
            tracing::debug!(
                rules,
                "seccomp loaded last, so as few syscalls as possible run under the filter"
            );
        }
    }

    channel.send(&Message::InitReady(clock.take()))?;

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
