use std::fs;

use nix::sys::resource::{Resource, setrlimit};
use nix::sys::stat::{Mode, umask};
use nix::unistd::{Gid, Uid, setgid, setgroups, setuid};
use oci_spec::runtime::{PosixRlimit, PosixRlimitType, Process};

use crate::error::{Error, IoContext, NixContext, Result};

pub fn apply_limits(process: &Process) -> Result<()> {
    for rlimit in process.rlimits().iter().flatten() {
        let resource = map_resource(rlimit.typ()).ok_or(Error::OutOfScope(
            "a POSIX rlimit this platform does not expose through nix",
        ))?;

        setrlimit(resource, rlimit.soft(), rlimit.hard()).ctx(format!(
            "setrlimit({:?}, soft={}, hard={})",
            rlimit.typ(),
            rlimit.soft(),
            rlimit.hard()
        ))?;
    }

    Ok(())
}

pub fn apply_oom_score_adj(process: &Process) -> Result<()> {
    let Some(score) = process.oom_score_adj() else {
        return Ok(());
    };

    fs::write("/proc/self/oom_score_adj", score.to_string()).ctx(format!(
        "write oom_score_adj={score}; a negative value needs CAP_SYS_RESOURCE, so this must \
         happen before dropping privileges"
    ))
}

pub fn apply_umask(process: &Process) {
    if let Some(mask) = process.user().umask() {
        umask(Mode::from_bits_truncate(mask));
    }
}

pub fn drop_privileges(process: &Process) -> Result<()> {
    let user = process.user();
    let leaving_root = nix::unistd::geteuid().is_root() && user.uid() != 0;

    let groups: Vec<Gid> = user
        .additional_gids()
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(Gid::from_raw)
        .collect();

    setgroups(&groups).ctx(format!(
        "setgroups({:?}); the supplementary groups must be set while still privileged",
        user.additional_gids().clone().unwrap_or_default()
    ))?;

    if leaving_root {
        super::capabilities::keep_caps_across_setuid(true)?;
    }

    let gid = Gid::from_raw(user.gid());
    setgid(gid).ctx(format!("setgid({gid})"))?;

    let uid = Uid::from_raw(user.uid());
    setuid(uid).ctx(format!(
        "setuid({uid}); this comes last because after it we can no longer change groups"
    ))?;

    if leaving_root {
        super::capabilities::keep_caps_across_setuid(false)?;
    }

    Ok(())
}

fn map_resource(kind: PosixRlimitType) -> Option<Resource> {
    Some(match kind {
        PosixRlimitType::RlimitCpu => Resource::RLIMIT_CPU,
        PosixRlimitType::RlimitFsize => Resource::RLIMIT_FSIZE,
        PosixRlimitType::RlimitData => Resource::RLIMIT_DATA,
        PosixRlimitType::RlimitStack => Resource::RLIMIT_STACK,
        PosixRlimitType::RlimitCore => Resource::RLIMIT_CORE,
        PosixRlimitType::RlimitRss => Resource::RLIMIT_RSS,
        PosixRlimitType::RlimitNproc => Resource::RLIMIT_NPROC,
        PosixRlimitType::RlimitNofile => Resource::RLIMIT_NOFILE,
        PosixRlimitType::RlimitMemlock => Resource::RLIMIT_MEMLOCK,
        PosixRlimitType::RlimitAs => Resource::RLIMIT_AS,
        PosixRlimitType::RlimitLocks => Resource::RLIMIT_LOCKS,
        PosixRlimitType::RlimitSigpending => Resource::RLIMIT_SIGPENDING,
        PosixRlimitType::RlimitMsgqueue => Resource::RLIMIT_MSGQUEUE,
        PosixRlimitType::RlimitNice => Resource::RLIMIT_NICE,
        PosixRlimitType::RlimitRtprio => Resource::RLIMIT_RTPRIO,
        PosixRlimitType::RlimitRttime => Resource::RLIMIT_RTTIME,
    })
}

pub fn rlimit_names(rlimits: &[PosixRlimit]) -> Vec<String> {
    rlimits
        .iter()
        .map(|rlimit| format!("{:?}", rlimit.typ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rlimit_type_the_spec_defines_maps_onto_a_resource() {
        for kind in [
            PosixRlimitType::RlimitCpu,
            PosixRlimitType::RlimitFsize,
            PosixRlimitType::RlimitData,
            PosixRlimitType::RlimitStack,
            PosixRlimitType::RlimitCore,
            PosixRlimitType::RlimitRss,
            PosixRlimitType::RlimitNproc,
            PosixRlimitType::RlimitNofile,
            PosixRlimitType::RlimitMemlock,
            PosixRlimitType::RlimitAs,
            PosixRlimitType::RlimitLocks,
            PosixRlimitType::RlimitSigpending,
            PosixRlimitType::RlimitMsgqueue,
            PosixRlimitType::RlimitNice,
            PosixRlimitType::RlimitRtprio,
            PosixRlimitType::RlimitRttime,
        ] {
            assert!(map_resource(kind).is_some(), "{kind:?} has no mapping");
        }
    }
}
