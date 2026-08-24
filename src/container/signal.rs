use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, sigprocmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::error::Result;

const FORWARDED: [Signal; 7] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGHUP,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGWINCH,
];

pub fn block_forwardable() -> Result<SigSet> {
    let mut set = SigSet::empty();

    for signal in FORWARDED {
        set.add(signal);
    }
    set.add(Signal::SIGCHLD);

    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&set), None)?;
    Ok(set)
}

pub fn unblock_all() -> Result<()> {
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)?;
    Ok(())
}

pub fn supervise(blocked: &SigSet, init: Pid) -> Result<u8> {
    loop {
        let signal = blocked.wait()?;

        if signal != Signal::SIGCHLD {
            let _ = kill(init, signal);
            continue;
        }

        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) if pid == init => return Ok(code as u8),
                Ok(WaitStatus::Signaled(pid, terminator, _)) if pid == init => {
                    return Ok((128 + terminator as i32) as u8);
                }
                Ok(WaitStatus::StillAlive) => break,
                Ok(_) => continue,
                Err(nix::Error::ECHILD) => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
}
