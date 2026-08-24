use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, sigprocmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::error::{Error, Result};

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

pub fn parse(name: &str) -> Result<Signal> {
    let trimmed = name.trim();

    if let Ok(number) = trimmed.parse::<i32>() {
        return Signal::try_from(number)
            .map_err(|_| Error::Invalid(format!("{number} is not a signal number")));
    }

    let upper = trimmed.to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());

    Signal::iterator()
        .find(|candidate| candidate.as_str().strip_prefix("SIG") == Some(bare))
        .ok_or_else(|| Error::Invalid(format!("{name:?} is not a known signal")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_accepted_with_or_without_the_sig_prefix() {
        assert_eq!(parse("SIGTERM").unwrap(), Signal::SIGTERM);
        assert_eq!(parse("TERM").unwrap(), Signal::SIGTERM);
        assert_eq!(parse("term").unwrap(), Signal::SIGTERM);
        assert_eq!(parse(" KILL ").unwrap(), Signal::SIGKILL);
    }

    #[test]
    fn numbers_are_accepted_because_that_is_what_docker_sends() {
        assert_eq!(parse("9").unwrap(), Signal::SIGKILL);
        assert_eq!(parse("15").unwrap(), Signal::SIGTERM);
    }

    #[test]
    fn nonsense_is_rejected_by_name() {
        assert!(parse("SIGNOPE").is_err());
        assert!(parse("0").is_err());
        assert!(parse("999").is_err());
    }
}
