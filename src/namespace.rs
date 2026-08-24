use std::path::PathBuf;

use nix::sched::CloneFlags;
use oci_spec::runtime::{LinuxNamespace, LinuxNamespaceType};

use crate::error::{Error, Result};

pub const JOIN_ORDER: [LinuxNamespaceType; 7] = [
    LinuxNamespaceType::User,
    LinuxNamespaceType::Ipc,
    LinuxNamespaceType::Uts,
    LinuxNamespaceType::Network,
    LinuxNamespaceType::Pid,
    LinuxNamespaceType::Cgroup,
    LinuxNamespaceType::Mount,
];

#[derive(Debug, Clone)]
pub struct Layout {
    pub create: CloneFlags,
    pub join: Vec<(LinuxNamespaceType, PathBuf)>,
}

pub fn layout(namespaces: &[LinuxNamespace]) -> Result<Layout> {
    let mut create = CloneFlags::empty();
    let mut join = Vec::new();

    for namespace in namespaces {
        let flag = flag_for(namespace.typ())?;

        match namespace.path() {
            Some(path) if !path.as_os_str().is_empty() => {
                if !path.exists() {
                    return Err(Error::Invalid(format!(
                        "namespace {:?} points at {}, which does not exist",
                        namespace.typ(),
                        path.display()
                    )));
                }
                join.push((namespace.typ(), path.clone()));
            }
            _ => create |= flag,
        }
    }

    join.sort_by_key(|(kind, _)| {
        JOIN_ORDER
            .iter()
            .position(|candidate| candidate == kind)
            .unwrap_or(usize::MAX)
    });

    Ok(Layout { create, join })
}

pub fn clone_flags(namespaces: &[LinuxNamespace]) -> Result<CloneFlags> {
    Ok(layout(namespaces)?.create)
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

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::LinuxNamespaceBuilder;

    fn namespace(kind: LinuxNamespaceType, path: Option<&str>) -> LinuxNamespace {
        let mut builder = LinuxNamespaceBuilder::default();
        builder = builder.typ(kind);

        if let Some(path) = path {
            builder = builder.path(PathBuf::from(path));
        }

        builder.build().unwrap()
    }

    #[test]
    fn namespaces_without_a_path_are_created() {
        let found = layout(&[
            namespace(LinuxNamespaceType::Pid, None),
            namespace(LinuxNamespaceType::Mount, None),
        ])
        .unwrap();

        assert_eq!(
            found.create,
            CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS
        );
        assert!(found.join.is_empty());
    }

    #[test]
    fn an_empty_path_means_create_not_join() {
        let found = layout(&[namespace(LinuxNamespaceType::Uts, Some(""))]).unwrap();

        assert_eq!(found.create, CloneFlags::CLONE_NEWUTS);
        assert!(found.join.is_empty());
    }

    #[test]
    fn namespaces_with_a_path_are_joined_and_do_not_appear_in_the_clone_flags() {
        let found = layout(&[
            namespace(LinuxNamespaceType::Pid, Some("/proc/self/ns/pid")),
            namespace(LinuxNamespaceType::Mount, None),
        ])
        .unwrap();

        assert_eq!(found.create, CloneFlags::CLONE_NEWNS);
        assert_eq!(found.join.len(), 1);
        assert_eq!(found.join[0].0, LinuxNamespaceType::Pid);
    }

    #[test]
    fn joins_are_ordered_user_first_and_mount_last() {
        let found = layout(&[
            namespace(LinuxNamespaceType::Mount, Some("/proc/self/ns/mnt")),
            namespace(LinuxNamespaceType::Pid, Some("/proc/self/ns/pid")),
            namespace(LinuxNamespaceType::User, Some("/proc/self/ns/user")),
        ])
        .unwrap();

        let order: Vec<_> = found.join.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            order,
            vec![
                LinuxNamespaceType::User,
                LinuxNamespaceType::Pid,
                LinuxNamespaceType::Mount
            ]
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_reported_rather_than_passed_to_setns() {
        let error = layout(&[namespace(
            LinuxNamespaceType::Pid,
            Some("/proc/self/ns/nope"),
        )])
        .unwrap_err();

        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn the_time_namespace_stays_out_of_scope() {
        assert!(layout(&[namespace(LinuxNamespaceType::Time, None)]).is_err());
    }
}
