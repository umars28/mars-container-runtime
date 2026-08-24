use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use nix::fcntl::{OFlag, open};
use nix::sched::{CloneFlags, setns};
use nix::sys::stat::Mode;
use nix::unistd::{ForkResult, Gid, Uid, chdir, execve, fork, setgid, setgroups, setuid};

use crate::bundle;
use crate::error::{Error, IoContext, NixContext, Result};
use crate::paths::Layout;
use crate::state::{Container, Status};
use crate::sync::{self, Message};

use super::{console, signal};

const JOIN_ORDER: [(&str, CloneFlags); 7] = [
    ("user", CloneFlags::CLONE_NEWUSER),
    ("ipc", CloneFlags::CLONE_NEWIPC),
    ("uts", CloneFlags::CLONE_NEWUTS),
    ("net", CloneFlags::CLONE_NEWNET),
    ("pid", CloneFlags::CLONE_NEWPID),
    ("cgroup", CloneFlags::CLONE_NEWCGROUP),
    ("mnt", CloneFlags::CLONE_NEWNS),
];

pub struct ExecOptions {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub process: Option<PathBuf>,
    pub user: Option<String>,
    pub additional_gids: Vec<u32>,
    pub console_socket: Option<PathBuf>,
    pub tty: bool,
    pub detach: bool,
    pub pid_file: Option<PathBuf>,
    pub ignore_paused: bool,
}

struct Target {
    argv: Vec<String>,
    env: Vec<String>,
    cwd: PathBuf,
    uid: Option<Uid>,
    gid: Option<Gid>,
    groups: Vec<Gid>,
    terminal: bool,
}

pub fn exec(layout: &Layout, id: &str, options: &ExecOptions) -> Result<u8> {
    let container = Container::load(layout, id)?;
    let status = container.status();

    match status {
        Status::Stopped | Status::Creating => {
            return Err(Error::BadState {
                id: id.to_string(),
                actual: status.to_string(),
                expected: "created or running".to_string(),
            });
        }
        Status::Paused if !options.ignore_paused => {
            return Err(Error::BadState {
                id: id.to_string(),
                actual: "paused".to_string(),
                expected: "running; pass --ignore-paused to exec anyway".to_string(),
            });
        }
        _ => {}
    }

    let target = resolve_target(&container, options)?;

    match (&options.console_socket, target.terminal) {
        (None, true) => {
            return Err(Error::Invalid(
                "the exec process asks for a terminal but no --console-socket was given"
                    .to_string(),
            ));
        }
        (Some(_), false) => {
            return Err(Error::Invalid(
                "--console-socket was given but the exec process does not ask for a terminal"
                    .to_string(),
            ));
        }
        _ => {}
    }

    single_threaded()?;

    let init = container.pid();
    let joins = collect(init)?;

    if joins.is_empty() {
        tracing::debug!("the container shares every namespace with the runtime, nothing to join");
    }

    let console = match &options.console_socket {
        Some(path) => Some(console::open_socket(path)?),
        None => None,
    };

    let procs = open_cgroup_procs(&container)?;

    let mut pid_file = match options.pid_file.as_deref() {
        Some(path) => Some(File::create(path).ctx(format!(
            "create the pid file {} before joining the container namespaces; afterwards this path \
             would resolve inside the container",
            path.display()
        ))?),
        None => None,
    };

    for (name, flag, handle) in &joins {
        setns(handle.as_fd(), *flag).ctx(format!(
            "setns into the container's {name} namespace (/proc/{}/ns/{name})",
            init.as_raw()
        ))?;
    }

    let blocked = signal::block_forwardable()?;
    let (parent, child) = sync::pair()?;

    match unsafe { fork() }? {
        ForkResult::Child => {
            drop(parent);

            let error = match child
                .expect("CgroupApplied")
                .and_then(|_| enter(&target, console))
            {
                Err(error) => error,
                Ok(()) => unreachable!(),
            };

            eprintln!("mars: exec failed: {error}");
            std::process::exit(126);
        }
        ForkResult::Parent { child: pid } => {
            drop(child);

            place(&procs, pid)?;
            parent.send(&Message::CgroupApplied(pid.as_raw()))?;

            if let Some(handle) = &mut pid_file {
                handle
                    .write_all(pid.as_raw().to_string().as_bytes())
                    .ctx("write the exec pid file")?;
            }

            if options.detach {
                return Ok(0);
            }

            signal::supervise(&blocked, pid)
        }
    }
}

fn open_cgroup_procs(container: &Container) -> Result<File> {
    let cgroup = container.cgroup();
    let path = cgroup.path().join("cgroup.procs");

    OpenOptions::new()
        .write(true)
        .open(&path)
        .ctx(format!("open {} before joining the container namespaces; once we are inside the cgroup namespace this path no longer resolves", path.display()))
}

fn place(procs: &File, pid: nix::unistd::Pid) -> Result<()> {
    let mut handle = procs;

    handle
        .write_all(pid.as_raw().to_string().as_bytes())
        .ctx("write the exec process into the container's cgroup")
}

fn enter(target: &Target, console: Option<OwnedFd>) -> Result<()> {
    chdir("/").ctx("chdir to / after entering the container's mount namespace")?;

    if let Some(socket) = console {
        console::setup(&socket)?;
    }

    if let Some(gid) = target.gid {
        if !target.groups.is_empty() {
            setgroups(&target.groups).ctx("set the supplementary groups for the exec process")?;
        }
        setgid(gid).ctx(format!("setgid({gid})"))?;
    }

    if let Some(uid) = target.uid {
        setuid(uid).ctx(format!("setuid({uid})"))?;
    }

    chdir(&target.cwd).ctx(format!("chdir to {}", target.cwd.display()))?;

    let program = bundle::resolve_executable(&target.argv[0], &target.env, true)?;
    let program = CString::new(program.as_os_str().as_bytes())
        .map_err(|_| Error::NulByte(program.display().to_string()))?;
    let argv = bundle::to_cstrings(&target.argv)?;
    let env = bundle::to_cstrings(&target.env)?;

    signal::unblock_all()?;

    execve(&program, &argv, &env)?;
    unreachable!()
}

fn collect(init: nix::unistd::Pid) -> Result<Vec<(&'static str, CloneFlags, OwnedFd)>> {
    let mut joins = Vec::new();

    for (name, flag) in JOIN_ORDER {
        let theirs = PathBuf::from(format!("/proc/{}/ns/{name}", init.as_raw()));

        if !theirs.exists() {
            continue;
        }

        let ours = fs::read_link(format!("/proc/self/ns/{name}")).ok();
        let target = fs::read_link(&theirs).ok();

        if ours.is_some() && ours == target {
            continue;
        }

        let handle = open(&theirs, OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty())
            .ctx(format!("open {}", theirs.display()))?;

        joins.push((name, flag, handle));
    }

    Ok(joins)
}

fn single_threaded() -> Result<()> {
    let threads = fs::read_dir("/proc/self/task")
        .ctx("count our own threads")?
        .count();

    if threads <= 1 {
        return Ok(());
    }

    Err(Error::Invalid(format!(
        "the runtime has {threads} threads, but setns(2) refuses to move a multi-threaded process \
         into a new mount or user namespace; nothing in mars may start a thread before exec"
    )))
}

fn resolve_target(container: &Container, options: &ExecOptions) -> Result<Target> {
    let spec = bundle::Bundle::load(&container.state.bundle).ok();

    let process = match &options.process {
        Some(path) => {
            let text = fs::read_to_string(path).ctx(format!("read {}", path.display()))?;
            Some(serde_json::from_str::<oci_spec::runtime::Process>(&text)?)
        }
        None => None,
    };

    let argv = match (&process, options.argv.is_empty()) {
        (Some(process), _) => process.args().clone().unwrap_or_default(),
        (None, false) => options.argv.clone(),
        (None, true) => return Err(Error::SpecField("process.args")),
    };

    if argv.is_empty() {
        return Err(Error::SpecField("process.args"));
    }

    let mut env = match &process {
        Some(process) => process.env().clone().unwrap_or_default(),
        None => spec.as_ref().map(bundle::Bundle::env).unwrap_or_default(),
    };

    for entry in &options.env {
        if !entry.contains('=') {
            return Err(Error::Invalid(format!(
                "--env {entry:?} is not in KEY=VALUE form"
            )));
        }
        env.push(entry.clone());
    }

    let cwd = options
        .cwd
        .clone()
        .or_else(|| process.as_ref().map(|process| process.cwd().clone()))
        .or_else(|| spec.as_ref().and_then(bundle::Bundle::cwd))
        .filter(|cwd| !cwd.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/"));

    let (uid, gid) = match (&options.user, &process) {
        (Some(text), _) => parse_user(text)?,
        (None, Some(process)) => (
            Some(Uid::from_raw(process.user().uid())),
            Some(Gid::from_raw(process.user().gid())),
        ),
        (None, None) => (None, None),
    };

    let mut groups: Vec<Gid> = options
        .additional_gids
        .iter()
        .copied()
        .map(Gid::from_raw)
        .collect();

    if groups.is_empty() {
        if let Some(process) = &process {
            groups = process
                .user()
                .additional_gids()
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Gid::from_raw)
                .collect();
        }
    }

    let terminal = match &process {
        Some(process) => process.terminal().unwrap_or(false),
        None => options.tty,
    };

    Ok(Target {
        argv,
        env,
        cwd,
        uid,
        gid,
        groups,
        terminal,
    })
}

fn parse_user(text: &str) -> Result<(Option<Uid>, Option<Gid>)> {
    let (uid, gid) = match text.split_once(':') {
        Some((uid, gid)) => (uid, Some(gid)),
        None => (text, None),
    };

    let uid = uid
        .parse::<u32>()
        .map_err(|_| Error::Invalid(format!("--user {text:?} must be UID[:GID] as numbers")))?;

    let gid = match gid {
        Some(gid) => Some(Gid::from_raw(gid.parse::<u32>().map_err(|_| {
            Error::Invalid(format!("--user {text:?} must be UID[:GID] as numbers"))
        })?)),
        None => None,
    };

    Ok((Some(Uid::from_raw(uid)), gid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_mount_namespace_is_joined_last() {
        let last = JOIN_ORDER.last().unwrap();
        assert_eq!(last.0, "mnt");
    }

    #[test]
    fn the_user_namespace_is_joined_first() {
        assert_eq!(JOIN_ORDER[0].0, "user");
    }

    #[test]
    fn the_pid_namespace_is_joined_before_the_mount_namespace() {
        let position = |name| JOIN_ORDER.iter().position(|(n, _)| *n == name).unwrap();
        assert!(position("pid") < position("mnt"));
    }

    #[test]
    fn a_user_is_parsed_with_an_optional_group() {
        assert_eq!(parse_user("0").unwrap(), (Some(Uid::from_raw(0)), None));
        assert_eq!(
            parse_user("1000:1000").unwrap(),
            (Some(Uid::from_raw(1000)), Some(Gid::from_raw(1000)))
        );
        assert!(parse_user("root").is_err());
        assert!(parse_user("1000:staff").is_err());
    }

    #[test]
    fn the_guard_notices_extra_threads() {
        let extra = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(200)));
        let error = single_threaded().unwrap_err();

        assert!(error.to_string().contains("setns(2) refuses"), "{error}");
        extra.join().unwrap();
    }
}
