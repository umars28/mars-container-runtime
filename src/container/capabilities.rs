use std::collections::HashSet;
use std::str::FromStr;

use caps::{CapSet, Capability, CapsHashSet};
use oci_spec::runtime::{Capability as SpecCapability, LinuxCapabilities};

use crate::error::{Error, Result};

pub fn parse(names: &Option<HashSet<SpecCapability>>) -> Result<CapsHashSet> {
    let mut set = CapsHashSet::new();

    for name in names.iter().flatten() {
        let canonical = spec_name(name)?;

        let parsed = Capability::from_str(&canonical).map_err(|_| {
            Error::Invalid(format!(
                "process.capabilities lists {canonical}, which this kernel's capability set does \
                 not contain"
            ))
        })?;

        set.insert(parsed);
    }

    Ok(set)
}

pub fn spec_name(name: &SpecCapability) -> Result<String> {
    match serde_json::to_value(name)? {
        serde_json::Value::String(text) => Ok(text),
        other => Err(Error::Invalid(format!(
            "capability {other} did not serialise to a name"
        ))),
    }
}

pub fn drop_bounding(spec: &LinuxCapabilities) -> Result<()> {
    let wanted = parse(spec.bounding())?;
    let current = caps::read(None, CapSet::Bounding)
        .map_err(|error| Error::Invalid(format!("read the bounding capability set: {error}")))?;

    for capability in current.difference(&wanted) {
        caps::drop(None, CapSet::Bounding, *capability).map_err(|error| {
            Error::Invalid(format!(
                "drop {capability} from the bounding set: {error}; this needs CAP_SETPCAP and must \
                 happen before setuid"
            ))
        })?;
    }

    Ok(())
}

pub fn apply(spec: &LinuxCapabilities) -> Result<()> {
    let permitted = parse(spec.permitted())?;
    let effective = parse(spec.effective())?;
    let inheritable = parse(spec.inheritable())?;
    let ambient = parse(spec.ambient())?;

    if !effective.is_subset(&permitted) {
        return Err(Error::Invalid(format!(
            "process.capabilities.effective has {:?} which is not in permitted; the kernel              requires effective to be a subset of permitted",
            effective.difference(&permitted).collect::<Vec<_>>()
        )));
    }

    set(CapSet::Effective, &effective)?;
    set(CapSet::Permitted, &permitted)?;
    set(CapSet::Inheritable, &inheritable)?;

    caps::clear(None, CapSet::Ambient)
        .map_err(|error| Error::Invalid(format!("clear the ambient capability set: {error}")))?;

    for capability in &ambient {
        if !permitted.contains(capability) || !inheritable.contains(capability) {
            return Err(Error::Invalid(format!(
                "{capability} is in process.capabilities.ambient but not in both permitted and \
                 inheritable; the kernel refuses to raise it"
            )));
        }

        caps::raise(None, CapSet::Ambient, *capability)
            .map_err(|error| Error::Invalid(format!("raise {capability} into ambient: {error}")))?;
    }

    Ok(())
}

fn set(which: CapSet, value: &CapsHashSet) -> Result<()> {
    caps::set(None, which, value)
        .map_err(|error| Error::Invalid(format!("set the {which:?} capability set: {error}")))
}

pub fn effective_now() -> Result<HashSet<String>> {
    let current = caps::read(None, CapSet::Effective)
        .map_err(|error| Error::Invalid(format!("read the effective capability set: {error}")))?;

    Ok(current
        .into_iter()
        .map(|capability| capability.to_string())
        .collect())
}

pub fn keep_caps_across_setuid(keep: bool) -> Result<()> {
    let rc = unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, i32::from(keep), 0, 0, 0) };

    if rc != 0 {
        return Err(Error::Invalid(format!(
            "prctl(PR_SET_KEEPCAPS, {}): {}; without it a transition away from uid 0 clears every              capability set",
            i32::from(keep),
            nix::Error::last()
        )));
    }

    Ok(())
}

pub fn set_no_new_privs() -> Result<()> {
    nix::sys::prctl::set_no_new_privs()
        .map_err(|error| Error::Invalid(format!("prctl(PR_SET_NO_NEW_PRIVS): {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_names_use_the_kernels_snake_case_spelling() {
        assert_eq!(
            spec_name(&SpecCapability::AuditWrite).unwrap(),
            "CAP_AUDIT_WRITE"
        );
        assert_eq!(
            spec_name(&SpecCapability::NetBindService).unwrap(),
            "CAP_NET_BIND_SERVICE"
        );
        assert_eq!(spec_name(&SpecCapability::Kill).unwrap(), "CAP_KILL");
        assert_eq!(
            spec_name(&SpecCapability::CheckpointRestore).unwrap(),
            "CAP_CHECKPOINT_RESTORE"
        );
    }

    #[test]
    fn spec_names_map_onto_kernel_capabilities() {
        let names = Some(HashSet::from([
            SpecCapability::AuditWrite,
            SpecCapability::Kill,
            SpecCapability::NetBindService,
            SpecCapability::SysAdmin,
            SpecCapability::DacOverride,
        ]));

        let parsed = parse(&names).unwrap();

        assert!(parsed.contains(&Capability::CAP_AUDIT_WRITE));
        assert!(parsed.contains(&Capability::CAP_KILL));
        assert!(parsed.contains(&Capability::CAP_NET_BIND_SERVICE));
        assert!(parsed.contains(&Capability::CAP_SYS_ADMIN));
        assert!(parsed.contains(&Capability::CAP_DAC_OVERRIDE));
        assert_eq!(parsed.len(), 5);
    }

    #[test]
    fn an_absent_capability_list_is_an_empty_set_not_an_error() {
        assert!(parse(&None).unwrap().is_empty());
        assert!(parse(&Some(HashSet::new())).unwrap().is_empty());
    }

    #[test]
    fn every_capability_the_spec_can_name_is_parseable() {
        let all = [
            SpecCapability::AuditControl,
            SpecCapability::AuditRead,
            SpecCapability::AuditWrite,
            SpecCapability::BlockSuspend,
            SpecCapability::Bpf,
            SpecCapability::CheckpointRestore,
            SpecCapability::Chown,
            SpecCapability::DacOverride,
            SpecCapability::DacReadSearch,
            SpecCapability::Fowner,
            SpecCapability::Fsetid,
            SpecCapability::IpcLock,
            SpecCapability::IpcOwner,
            SpecCapability::Kill,
            SpecCapability::Lease,
            SpecCapability::LinuxImmutable,
            SpecCapability::MacAdmin,
            SpecCapability::MacOverride,
            SpecCapability::Mknod,
            SpecCapability::NetAdmin,
            SpecCapability::NetBindService,
            SpecCapability::NetBroadcast,
            SpecCapability::NetRaw,
            SpecCapability::Perfmon,
            SpecCapability::Setgid,
            SpecCapability::Setfcap,
            SpecCapability::Setpcap,
            SpecCapability::Setuid,
            SpecCapability::SysAdmin,
            SpecCapability::SysBoot,
            SpecCapability::SysChroot,
            SpecCapability::SysModule,
            SpecCapability::SysNice,
            SpecCapability::SysPacct,
            SpecCapability::SysPtrace,
            SpecCapability::SysRawio,
            SpecCapability::SysResource,
            SpecCapability::SysTime,
            SpecCapability::SysTtyConfig,
            SpecCapability::Syslog,
            SpecCapability::WakeAlarm,
        ];

        for capability in all {
            parse(&Some(HashSet::from([capability])))
                .unwrap_or_else(|error| panic!("{capability:?} did not parse: {error}"));
        }
    }
}
