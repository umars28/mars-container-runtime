pub mod init;
pub mod signal;

use std::fs;
use std::path::PathBuf;

use nix::sys::prctl;
use nix::sys::signal::SigSet;
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid, fork};

use crate::bundle::Bundle;
use crate::error::{Error, IoContext, Result};
use crate::namespace;
use crate::sync::{self, Channel, Message};

pub struct RunOptions {
    pub pid_file: Option<PathBuf>,
}

pub fn run(bundle: &Bundle, options: &RunOptions) -> Result<u8> {
    let rootfs = bundle.rootfs()?;

    if !rootfs.is_dir() {
        return Err(Error::RootfsMissing(rootfs));
    }

    let flags = namespace::clone_flags(&bundle.namespaces())?;

    prctl::set_child_subreaper(true)?;
    let blocked = signal::block_forwardable()?;

    let (parent, child) = sync::pair()?;

    match unsafe { fork() }? {
        ForkResult::Child => {
            drop(parent);
            init::intermediate(bundle, &rootfs, flags, &child)
        }
        ForkResult::Parent {
            child: intermediate,
        } => {
            drop(child);
            supervise(&parent, intermediate, options, &blocked)
        }
    }
}

fn supervise(
    channel: &Channel,
    intermediate: Pid,
    options: &RunOptions,
    blocked: &SigSet,
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
