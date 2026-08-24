use nix::sched::CloneFlags;
use oci_spec::runtime::{LinuxNamespace, LinuxNamespaceType};

use crate::error::{Error, Result};

pub fn clone_flags(namespaces: &[LinuxNamespace]) -> Result<CloneFlags> {
    let mut flags = CloneFlags::empty();

    for ns in namespaces {
        if ns.path().is_some() {
            return Err(Error::Unimplemented(
                "joining an existing namespace by path",
            ));
        }
        flags |= flag_for(ns.typ())?;
    }

    Ok(flags)
}

pub fn flag_for(kind: LinuxNamespaceType) -> Result<CloneFlags> {
    Ok(match kind {
        LinuxNamespaceType::Mount => CloneFlags::CLONE_NEWNS,
        LinuxNamespaceType::Pid => CloneFlags::CLONE_NEWPID,
        LinuxNamespaceType::Network => CloneFlags::CLONE_NEWNET,
        LinuxNamespaceType::Ipc => CloneFlags::CLONE_NEWIPC,
        LinuxNamespaceType::Uts => CloneFlags::CLONE_NEWUTS,
        LinuxNamespaceType::User => CloneFlags::CLONE_NEWUSER,
        LinuxNamespaceType::Cgroup => CloneFlags::CLONE_NEWCGROUP,
        LinuxNamespaceType::Time => return Err(Error::OutOfScope("time namespace")),
    })
}
