pub mod capabilities;
pub mod console;
pub mod exec;
pub mod fifo;
pub mod hooks;
pub mod init;
pub mod process;
pub mod seccomp;
pub mod signal;
pub mod userns;

use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use nix::sched::CloneFlags;
use nix::sys::prctl;
use nix::sys::signal::{Signal, kill as send_signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork};

use crate::OCI_VERSION;
use crate::bundle::Bundle;
use crate::cgroup::events;
use crate::cgroup::v2::{self, Cgroup};
use crate::error::{Error, IoContext, Result};
use crate::namespace;
use crate::paths::Layout;
use crate::rootfs::overlay::{self, Layers};
use crate::state::{self, Container, OciState, Persisted, Status};
use crate::sync::{self, Channel, Message};
use crate::telemetry;

pub struct CreateOptions {
    pub id: String,
    pub pid_file: Option<PathBuf>,
    pub console_socket: Option<PathBuf>,
    pub rootless: bool,
}

pub struct Plan<'a> {
    pub bundle: &'a Bundle,
    pub rootfs: PathBuf,
    pub overlay: Option<Layers>,
    pub unshare_flags: CloneFlags,
    pub join: Vec<(oci_spec::runtime::LinuxNamespaceType, PathBuf)>,
    pub cgroup_ns: bool,
    pub fifo: OwnedFd,
    pub console: Option<OwnedFd>,
    pub state: OciState,
}

pub fn create(
    layout: &Layout,
    bundle: &Bundle,
    options: &CreateOptions,
    clock: &mut telemetry::Recorder,
) -> Result<()> {
    let layout = layout.clone();

    if layout.exists(&options.id) {
        let existing = Container::load(&layout, &options.id)?;

        if existing.status() != Status::Stopped {
            return Err(Error::AlreadyExists(options.id.clone()));
        }

        return Err(Error::AlreadyExists(format!(
            "{}; it is stopped but not deleted, run `mars delete {}` first",
            options.id, options.id
        )));
    }

    let dir = layout.container_dir(&options.id);
    fs::create_dir_all(&dir).ctx(format!("create {}", dir.display()))?;

    match build(&layout, bundle, options, clock) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_dir_all(&dir);
            Err(error)
        }
    }
}

fn build(
    layout: &Layout,
    bundle: &Bundle,
    options: &CreateOptions,
    clock: &mut telemetry::Recorder,
) -> Result<()> {
    let terminal = bundle.terminal();

    match (&options.console_socket, terminal) {
        (None, true) => {
            return Err(Error::Invalid(
                "process.terminal is true but no --console-socket was given, so there is nowhere \
                 to send the pty master"
                    .to_string(),
            ));
        }
        (Some(_), false) => {
            return Err(Error::Invalid(
                "--console-socket was given but process.terminal is false".to_string(),
            ));
        }
        _ => {}
    }

    clock.begin("plan");
    let namespaces = namespace::layout(&bundle.namespaces())?;
    let cgroup_ns = namespaces.create.contains(CloneFlags::CLONE_NEWCGROUP);
    let unshare_flags = namespace::without(namespaces.create, CloneFlags::CLONE_NEWCGROUP);

    let rootfs = bundle.rootfs()?;
    let overlay_layers = bundle.overlay()?;

    if let Some(layers) = &overlay_layers {
        if !unshare_flags.contains(CloneFlags::CLONE_NEWNS) {
            return Err(Error::Overlay(
                "an overlay rootfs needs a mount namespace, otherwise the assembled filesystem \
                 would stay visible on the host after the container exits"
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

    clock.begin("cgroup");
    let relative = v2::relative_path(bundle.cgroups_path().as_deref(), &options.id);
    let cgroup = Cgroup::create(&relative)?;
    tracing::debug!(cgroup = %cgroup.path().display(), "cgroup created");

    if let Some(resources) = bundle.resources() {
        if let Err(error) = cgroup.apply(&resources) {
            let _ = cgroup.destroy();
            return Err(error);
        }
    }

    let outcome = spawn(
        layout,
        bundle,
        options,
        rootfs,
        overlay_layers,
        unshare_flags,
        namespaces.join,
        cgroup_ns,
        &cgroup,
        clock,
    );

    if outcome.is_err() {
        let _ = cgroup.destroy();
    }

    outcome
}

#[allow(clippy::too_many_arguments)]
fn spawn(
    layout: &Layout,
    bundle: &Bundle,
    options: &CreateOptions,
    rootfs: PathBuf,
    overlay_layers: Option<Layers>,
    unshare_flags: CloneFlags,
    join: Vec<(oci_spec::runtime::LinuxNamespaceType, PathBuf)>,
    cgroup_ns: bool,
    cgroup: &Cgroup,
    clock: &mut telemetry::Recorder,
) -> Result<()> {
    let fifo = fifo::create(&layout.exec_fifo(&options.id))?;

    let console = match &options.console_socket {
        Some(path) => Some(console::open_socket(path)?),
        None => None,
    };

    let state = OciState {
        version: OCI_VERSION.to_string(),
        id: options.id.clone(),
        status: Status::Creating,
        pid: None,
        bundle: bundle.dir.clone(),
        annotations: {
            let annotations = bundle.annotations();
            if annotations.is_empty() {
                None
            } else {
                Some(annotations)
            }
        },
    };

    let plan = Plan {
        bundle,
        rootfs,
        overlay: overlay_layers,
        unshare_flags,
        join,
        cgroup_ns,
        fifo,
        console,
        state,
    };

    prctl::set_child_subreaper(true)?;
    let _blocked = signal::block_forwardable()?;

    let (parent, child) = sync::pair()?;
    clock.begin("fork");
    let forked_at = clock.elapsed_us();

    match unsafe { fork() }? {
        ForkResult::Child => {
            drop(parent);
            init::intermediate(&plan, &child)
        }
        ForkResult::Parent {
            child: intermediate,
        } => {
            drop(child);

            match handshake(
                &parent,
                intermediate,
                &plan,
                cgroup,
                layout,
                options,
                clock,
                forked_at,
            ) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = fs::remove_file(layout.exec_fifo(&options.id));
                    teardown(cgroup);
                    Err(error)
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handshake(
    channel: &Channel,
    intermediate: Pid,
    plan: &Plan,
    cgroup: &Cgroup,
    layout: &Layout,
    options: &CreateOptions,
    clock: &mut telemetry::Recorder,
    forked_at: u64,
) -> Result<()> {
    clock.begin("wait.initpid");
    let mut message = channel.recv()?;

    if matches!(message, Message::RequestUserMapping) {
        let mappings = plan.bundle.id_mappings();
        let privileged = nix::unistd::geteuid().is_root();

        userns::write(intermediate, &mappings, privileged)?;
        channel.send(&Message::UserMappingDone)?;

        tracing::debug!(
            intermediate = intermediate.as_raw(),
            uid = mappings.uid.len(),
            gid = mappings.gid.len(),
            "id mappings written from outside the user namespace"
        );

        message = channel.recv()?;
    }

    let init = match message {
        Message::InitPid(pid, phases) => {
            clock.absorb("intermediate", forked_at, phases);
            Pid::from_raw(pid)
        }
        Message::Failed(reason) => return Err(Error::InitFailed(reason)),
        other => return Err(Error::Sync(format!("expected InitPid, got {other:?}"))),
    };

    clock.begin("reap.intermediate");
    waitpid(intermediate, None)?;

    clock.begin("cgroup.attach");
    cgroup.add_process(init)?;

    let mut state = plan.state.clone();
    state.pid = Some(init.as_raw());

    clock.begin("hooks.createRuntime");
    hooks::run(plan.bundle.hooks(), hooks::Phase::Prestart, &state)?;
    hooks::run(plan.bundle.hooks(), hooks::Phase::CreateRuntime, &state)?;

    clock.begin("init");
    let init_started = clock.elapsed_us();
    channel.send(&Message::CgroupApplied(init.as_raw()))?;

    let phases = match channel.expect("InitReady")? {
        Message::InitReady(phases) => phases,
        other => return Err(Error::Sync(format!("expected InitReady, got {other:?}"))),
    };

    clock.begin("state");

    let persisted = Persisted {
        oci_version: OCI_VERSION.to_string(),
        id: options.id.clone(),
        pid: init.as_raw(),
        start_time: state::start_time(init)?,
        bundle: plan.bundle.dir.clone(),
        rootfs: plan.rootfs.clone(),
        cgroup: cgroup.relative().to_path_buf(),
        created: state::now_rfc3339(),
        rootless: options.rootless,
        annotations: plan.bundle.annotations(),
    };

    Container::save(layout, &persisted)?;
    state::write_pid_file(options.pid_file.as_deref(), init)?;

    clock.end();
    clock.absorb("init", init_started, phases);

    tracing::debug!(init = init.as_raw(), id = %options.id, "container created");

    Ok(())
}

pub fn start(layout: &Layout, id: &str) -> Result<()> {
    let container = Container::load(layout, id)?;
    container.require(&[Status::Created])?;

    fifo::release(&container.exec_fifo())?;

    hooks::run(
        container_hooks(&container)?.as_ref(),
        hooks::Phase::Poststart,
        &container.oci_state(),
    )?;

    Ok(())
}

pub fn run(
    layout: &Layout,
    bundle: &Bundle,
    options: &CreateOptions,
    detach: bool,
    keep: bool,
    clock: &mut telemetry::Recorder,
    exported: &dyn Fn(&mut telemetry::Recorder),
) -> Result<u8> {
    let blocked = signal::block_forwardable()?;

    create(layout, bundle, options, clock)?;

    let container = Container::load(layout, &options.id)?;
    let init = container.pid();

    fifo::release(&container.exec_fifo())?;

    hooks::run(
        bundle.hooks(),
        hooks::Phase::Poststart,
        &container.oci_state(),
    )?;

    exported(clock);

    if detach {
        return Ok(0);
    }

    let cgroup = container.cgroup();
    let outcome = signal::supervise(&blocked, init);

    explain_exit(&cgroup, &outcome);

    hooks::run(
        bundle.hooks(),
        hooks::Phase::Poststop,
        &container.oci_state(),
    )?;

    let _ = cgroup.destroy();

    if !keep {
        let _ = container.remove();
    }

    outcome
}

pub fn kill(layout: &Layout, id: &str, signal: Signal, all: bool) -> Result<()> {
    let container = Container::load(layout, id)?;
    container.require(&[Status::Created, Status::Running, Status::Paused])?;

    if !all {
        nix::sys::signal::kill(container.pid(), signal)?;
        return Ok(());
    }

    let cgroup = container.cgroup();
    let frozen = cgroup.is_frozen();

    if !frozen {
        cgroup.freeze(true)?;
    }

    let result = cgroup.procs().and_then(|pids| {
        for pid in pids {
            nix::sys::signal::kill(pid, signal)?;
        }
        Ok(())
    });

    if !frozen {
        cgroup.freeze(false)?;
    }

    result
}

pub fn delete(layout: &Layout, id: &str, force: bool) -> Result<()> {
    let container = match Container::load(layout, id) {
        Ok(container) => container,
        Err(Error::NotFound(_)) if force => {
            tracing::debug!(
                id,
                "delete --force on a container that is already gone; containerd calls this during \
                 cleanup and expects it to succeed"
            );
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let status = container.status();

    if status != Status::Stopped {
        if !force {
            return Err(Error::BadState {
                id: id.to_string(),
                actual: status.to_string(),
                expected: "stopped; pass --force to kill it first".to_string(),
            });
        }

        let cgroup = container.cgroup();

        if cgroup.is_frozen() {
            cgroup.freeze(false)?;
        }

        for pid in cgroup.procs().unwrap_or_default() {
            let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
        }

        for _ in 0..500 {
            if !container.alive() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let hooks_spec = container_hooks(&container).unwrap_or(None);
    let _ = container.cgroup().destroy();

    hooks::run(
        hooks_spec.as_ref(),
        hooks::Phase::Poststop,
        &container.oci_state(),
    )?;

    container.remove()
}

pub fn pause(layout: &Layout, id: &str, paused: bool) -> Result<()> {
    let container = Container::load(layout, id)?;

    let expected: &[Status] = if paused {
        &[Status::Created, Status::Running]
    } else {
        &[Status::Paused]
    };

    container.require(expected)?;
    container.cgroup().freeze(paused)
}

pub fn update(
    layout: &Layout,
    id: &str,
    resources: &oci_spec::runtime::LinuxResources,
) -> Result<()> {
    let container = Container::load(layout, id)?;
    container.require(&[Status::Created, Status::Running, Status::Paused])?;

    container.cgroup().apply(resources)
}

fn container_hooks(container: &Container) -> Result<Option<oci_spec::runtime::Hooks>> {
    match Bundle::load(&container.state.bundle) {
        Ok(bundle) => Ok(bundle.hooks().cloned()),
        Err(error) => {
            tracing::debug!(
                bundle = %container.state.bundle.display(),
                "cannot reload the bundle to look for hooks: {error}"
            );
            Ok(None)
        }
    }
}

fn teardown(cgroup: &Cgroup) {
    for pid in cgroup.procs().unwrap_or_default() {
        let _ = send_signal(pid, Signal::SIGKILL);
    }

    for _ in 0..250 {
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(_) => continue,
            }
        }

        if cgroup.procs().map(|procs| procs.is_empty()).unwrap_or(true) {
            break;
        }

        std::thread::sleep(std::time::Duration::from_millis(20));
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
