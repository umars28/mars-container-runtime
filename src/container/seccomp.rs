use libseccomp::{
    ScmpAction, ScmpArch, ScmpArgCompare, ScmpCompareOp, ScmpFilterAttr, ScmpFilterContext,
    ScmpSyscall,
};
use oci_spec::runtime::{
    Arch, LinuxSeccomp, LinuxSeccompAction, LinuxSeccompFilterFlag, LinuxSeccompOperator,
};

use crate::error::{Error, Result};

pub fn apply(spec: &LinuxSeccomp) -> Result<usize> {
    let default = action(spec.default_action(), spec.default_errno_ret())?;
    let mut filter = ScmpFilterContext::new(default)
        .map_err(|error| Error::Invalid(format!("create a seccomp filter: {error}")))?;

    for arch in spec.architectures().iter().flatten() {
        let mapped = architecture(*arch)?;

        filter.add_arch(mapped).map_err(|error| {
            Error::Invalid(format!("add seccomp architecture {arch:?}: {error}"))
        })?;
    }

    filter
        .set_filter_attr(ScmpFilterAttr::CtlNnp, 0)
        .map_err(|error| {
            Error::Invalid(format!(
                "clear the seccomp filter's no-new-privs bit: {error}; libseccomp sets it on load \
                 by default, which would silently contradict process.noNewPrivileges"
            ))
        })?;

    for flag in spec.flags().iter().flatten() {
        let attribute = match flag {
            LinuxSeccompFilterFlag::SeccompFilterFlagLog => ScmpFilterAttr::CtlLog,
            LinuxSeccompFilterFlag::SeccompFilterFlagTsync => ScmpFilterAttr::CtlTsync,
            LinuxSeccompFilterFlag::SeccompFilterFlagSpecAllow => ScmpFilterAttr::CtlSsb,
            LinuxSeccompFilterFlag::SeccompFilterFlagWaitKillableRecv => {
                ScmpFilterAttr::CtlWaitkill
            }
        };

        filter
            .set_filter_attr(attribute, 1)
            .map_err(|error| Error::Invalid(format!("set seccomp flag {flag:?}: {error}")))?;
    }

    let mut rules = 0;

    for rule in spec.syscalls().iter().flatten() {
        let verdict = action(rule.action(), rule.errno_ret())?;

        for name in rule.names() {
            let syscall = match ScmpSyscall::from_name(name) {
                Ok(syscall) => syscall,
                Err(_) => {
                    tracing::debug!(
                        syscall = %name,
                        "the spec names a syscall this kernel's libseccomp does not know; skipping \
                         the rule rather than failing the container"
                    );
                    continue;
                }
            };

            let comparators = rule
                .args()
                .iter()
                .flatten()
                .map(comparator)
                .collect::<Result<Vec<_>>>()?;

            if comparators.is_empty() {
                filter.add_rule(verdict, syscall)
            } else {
                filter.add_rule_conditional(verdict, syscall, &comparators)
            }
            .map_err(|error| Error::Invalid(format!("add a seccomp rule for {name}: {error}")))?;

            rules += 1;
        }
    }

    filter
        .load()
        .map_err(|error| Error::Invalid(format!("load the seccomp filter: {error}")))?;

    Ok(rules)
}

fn action(requested: LinuxSeccompAction, errno: Option<u32>) -> Result<ScmpAction> {
    Ok(match requested {
        LinuxSeccompAction::ScmpActKill | LinuxSeccompAction::ScmpActKillThread => {
            ScmpAction::KillThread
        }
        LinuxSeccompAction::ScmpActKillProcess => ScmpAction::KillProcess,
        LinuxSeccompAction::ScmpActTrap => ScmpAction::Trap,
        LinuxSeccompAction::ScmpActErrno => {
            ScmpAction::Errno(errno.unwrap_or(libc::EPERM as u32) as i32)
        }
        LinuxSeccompAction::ScmpActTrace => ScmpAction::Trace(errno.unwrap_or(0) as u16),
        LinuxSeccompAction::ScmpActAllow => ScmpAction::Allow,
        LinuxSeccompAction::ScmpActLog => ScmpAction::Log,
        LinuxSeccompAction::ScmpActNotify => {
            return Err(Error::OutOfScope(
                "SCMP_ACT_NOTIFY, which needs a listener process to receive the notification fd",
            ));
        }
    })
}

fn architecture(arch: Arch) -> Result<ScmpArch> {
    Ok(match arch {
        Arch::ScmpArchNative => ScmpArch::Native,
        Arch::ScmpArchX86 => ScmpArch::X86,
        Arch::ScmpArchX86_64 => ScmpArch::X8664,
        Arch::ScmpArchX32 => ScmpArch::X32,
        Arch::ScmpArchArm => ScmpArch::Arm,
        Arch::ScmpArchAarch64 => ScmpArch::Aarch64,
        Arch::ScmpArchMips => ScmpArch::Mips,
        Arch::ScmpArchMips64 => ScmpArch::Mips64,
        Arch::ScmpArchMips64n32 => ScmpArch::Mips64N32,
        Arch::ScmpArchMipsel => ScmpArch::Mipsel,
        Arch::ScmpArchMipsel64 => ScmpArch::Mipsel64,
        Arch::ScmpArchMipsel64n32 => ScmpArch::Mipsel64N32,
        Arch::ScmpArchPpc => ScmpArch::Ppc,
        Arch::ScmpArchPpc64 => ScmpArch::Ppc64,
        Arch::ScmpArchPpc64le => ScmpArch::Ppc64Le,
        Arch::ScmpArchS390 => ScmpArch::S390,
        Arch::ScmpArchS390x => ScmpArch::S390X,
        Arch::ScmpArchParisc => ScmpArch::Parisc,
        Arch::ScmpArchParisc64 => ScmpArch::Parisc64,
        Arch::ScmpArchRiscv64 => ScmpArch::Riscv64,
        other => {
            return Err(Error::Invalid(format!(
                "seccomp architecture {other:?} has no libseccomp equivalent"
            )));
        }
    })
}

fn comparator(arg: &oci_spec::runtime::LinuxSeccompArg) -> Result<ScmpArgCompare> {
    let index = u32::try_from(arg.index()).map_err(|_| {
        Error::Invalid(format!("seccomp arg index {} is out of range", arg.index()))
    })?;

    if index > 5 {
        return Err(Error::Invalid(format!(
            "seccomp arg index {index} is out of range; a syscall has at most six arguments"
        )));
    }

    let op = match arg.op() {
        LinuxSeccompOperator::ScmpCmpNe => ScmpCompareOp::NotEqual,
        LinuxSeccompOperator::ScmpCmpLt => ScmpCompareOp::Less,
        LinuxSeccompOperator::ScmpCmpLe => ScmpCompareOp::LessOrEqual,
        LinuxSeccompOperator::ScmpCmpEq => ScmpCompareOp::Equal,
        LinuxSeccompOperator::ScmpCmpGe => ScmpCompareOp::GreaterEqual,
        LinuxSeccompOperator::ScmpCmpGt => ScmpCompareOp::Greater,
        LinuxSeccompOperator::ScmpCmpMaskedEq => {
            ScmpCompareOp::MaskedEqual(arg.value_two().unwrap_or(0))
        }
    };

    Ok(ScmpArgCompare::new(index, op, arg.value()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_map_onto_libseccomp() {
        assert_eq!(
            action(LinuxSeccompAction::ScmpActAllow, None).unwrap(),
            ScmpAction::Allow
        );
        assert_eq!(
            action(LinuxSeccompAction::ScmpActErrno, Some(38)).unwrap(),
            ScmpAction::Errno(38)
        );
        assert_eq!(
            action(LinuxSeccompAction::ScmpActKillProcess, None).unwrap(),
            ScmpAction::KillProcess
        );
    }

    #[test]
    fn errno_defaults_to_eperm_when_the_spec_omits_it() {
        assert_eq!(
            action(LinuxSeccompAction::ScmpActErrno, None).unwrap(),
            ScmpAction::Errno(libc::EPERM)
        );
    }

    #[test]
    fn notify_is_out_of_scope_rather_than_silently_allowed() {
        assert!(matches!(
            action(LinuxSeccompAction::ScmpActNotify, None),
            Err(Error::OutOfScope(_))
        ));
    }

    #[test]
    fn this_host_architecture_is_mappable() {
        architecture(Arch::ScmpArchAarch64).unwrap();
        architecture(Arch::ScmpArchX86_64).unwrap();
        architecture(Arch::ScmpArchNative).unwrap();
    }

    #[test]
    fn a_syscall_name_this_kernel_knows_resolves() {
        ScmpSyscall::from_name("write").unwrap();
        ScmpSyscall::from_name("chmod").unwrap();
        assert!(ScmpSyscall::from_name("definitely_not_a_syscall").is_err());
    }
}
