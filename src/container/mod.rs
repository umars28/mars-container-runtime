pub mod init;
pub mod signal;

use std::fs;
use std::path::{Path, PathBuf};

use nix::sched::CloneFlags;
use nix::sys::prctl;
use nix::sys::signal::SigSet;
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid, fork};

use crate::bundle::Bundle;
use crate::cgroup::events;
use crate::cgroup::v2::{self, Cgroup};
use crate::error::{Error, IoContext, Result};
use crate::namespace;
use crate::rootfs::overlay::{self, Layers};
use crate::sync::{self, Channel, Message};

pub struct RunOptions {
    pub id: String,
    pub pid_file: Option<PathBuf>,
}

pub struct Plan<'a> {
    pub bundle: &'a Bundle,
    pub rootfs: PathBuf,
    pub overlay: Option<Layers>,
    pub unshare_flags: CloneFlags,
    pub cgroup_ns: bool,
}

impl<'a> Plan<'a> {
    pub fn build(bundle: &'a Bundle) -> Result<Self> {
        let requested = namespace::clone_flags(&bundle.namespaces())?;
        let cgroup_ns = requested.contains(CloneFlags::CLONE_NEWCGROUP);
        let unshare_flags = requested & !CloneFlags::CLONE_NEWCGROUP;

        let rootfs = bundle.rootfs()?;
        let overlay = bundle.overlay()?;

        if let Some(layers) = &overlay {
            if !unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
                return Err(Error::Overlay(
                    "an overlay rootfs needs a mount namespace, otherwise the assembled \
                     filesystem would stay visible on the host after the container exits"
                        .to_string(),
                ));
            }

            overlay::prepare(layers)?;
            prepare_merged(&rootfs)?;

            tracing::debug!(
                lower = layers.lower.len(),
                readonly = layers.is_readonly(),
                merged = %rootfs.display(),
                "overlay rootfs prepared"
            );
        }

        if !rootfs.is_dir() {
            return Err(Error::RootfsMissing(rootfs));
        }

        Ok(Self {
            bundle,
            rootfs,
            overlay,
            unshare_flags,
            cgroup_ns,
        })
    }
}

fn prepare_merged(rootfs: &Path) -> Result<()> {
    fs::create_dir_all(rootfs).ctx(format!("create overlay mountpoint {}", rootfs.display()))?;

    let mut entries =
        fs::read_dir(rootfs).ctx(format!("inspect overlay mountpoint {}", rootfs.display()))?;

    if entries.next().is_some() {
        return Err(Error::Overlay(format!(
            "root.path {} is not empty; the overlay is mounted over it, so anything already \
             there would be hidden for the life of the container",
            rootfs.display()
        )));
    }

    Ok(())
}

pub fn run(bundle: &Bundle, options: &RunOptions) -> Result<u8> {
    let plan = Plan::build(bundle)?;

    let relative = v2::relative_path(bundle.cgroups_path().as_deref(), &options.id);
    let cgroup = Cgroup::create(&relative)?;
    tracing::debug!(cgroup = %cgroup.path().display(), "cgroup created");

    if let Some(resources) = bundle.resources() {
        if let Err(error) = cgroup.apply(&resources) {
            let _ = cgroup.destroy();
            return Err(error);
        }
    }

    prctl::set_child_subreaper(true)?;
    let blocked = signal::block_forwardable()?;

    let (parent, child) = sync::pair()?;

    match unsafe { fork() }? {
        ForkResult::Child => {
            drop(parent);
            init::intermediate(&plan, &child)
        }
        ForkResult::Parent {
            child: intermediate,
        } => {
            drop(child);

            let outcome = supervise(&parent, intermediate, options, &blocked, &cgroup);
            explain_exit(&cgroup, &outcome);
            let _ = cgroup.destroy();

            outcome
        }
    }
}

fn supervise(
    channel: &Channel,
    intermediate: Pid,
    options: &RunOptions,
    blocked: &SigSet,
    cgroup: &Cgroup,
) -> Result<u8> {
    let init = match channel.recv()? {
        Message::InitPid(pid) => Pid::from_raw(pid),
        Message::RequestUserMapping => {
            return Err(Error::Unimplemented("user namespace uid/gid mapping"));
        }
        Message::Failed(reason) => return Err(Error::InitFailed(reason)),
        other => return Err(Error::Sync(format!("expected InitPid, got {other:?}"))),
    };

    waitpid(intermediate, None)?;

    cgroup.add_process(init)?;
    channel.send(&Message::CgroupApplied)?;

    match channel.recv()? {
        Message::InitReady => {}
        Message::Failed(reason) => return Err(Error::InitFailed(reason)),
        other => return Err(Error::Sync(format!("expected InitReady, got {other:?}"))),
    }

    if let Some(path) = &options.pid_file {
        fs::write(path, init.as_raw().to_string())
            .ctx(format!("write pid file {}", path.display()))?;
    }

    tracing::debug!(init = init.as_raw(), "container init ready, starting");
    channel.send(&Message::Start)?;

    signal::supervise(blocked, init)
}

fn explain_exit(cgroup: &Cgroup, outcome: &Result<u8>) {
    let Ok(code) = outcome else { return };

    if let Ok(memory) = events::memory_events(cgroup) {
        if memory.oom_kill > 0 {
            tracing::warn!(
                exit_code = code,
                oom_kill = memory.oom_kill,
                max_events = memory.max,
                "container was OOM killed: a process exceeded memory.max"
            );
        }
    }

    if let Ok(cpu) = events::cpu_stat(cgroup) {
        if cpu.nr_throttled > 0 {
            tracing::info!(
                nr_periods = cpu.nr_periods,
                nr_throttled = cpu.nr_throttled,
                throttled_usec = cpu.throttled_usec,
                "container was CPU throttled by cpu.max"
            );
        }
    }
}
