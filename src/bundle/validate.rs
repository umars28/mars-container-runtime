use std::collections::HashSet;
use std::path::Path;

use oci_spec::runtime::{LinuxNamespaceType, Spec};

use crate::error::{Error, Result};

const SUPPORTED_MAJOR: u64 = 1;

const PROPAGATIONS: [&str; 4] = ["shared", "slave", "private", "unbindable"];

pub fn spec(spec: &Spec) -> Result<()> {
    platform(spec)?;
    version(spec.version())?;

    let root = spec.root().as_ref().ok_or(Error::SpecField("root"))?;

    if root.path().as_os_str().is_empty() {
        return Err(Error::Invalid("root.path is empty".to_string()));
    }

    let linux = spec.linux().as_ref().ok_or(Error::SpecField("linux"))?;

    let namespaces = namespace_types(spec)?;
    process(spec)?;
    mounts(spec)?;

    if spec.hostname().is_some() && !namespaces.contains(&LinuxNamespaceType::Uts) {
        return Err(Error::Invalid(
            "hostname is set but the container has no UTS namespace, so setting it would rename \
             the host"
                .to_string(),
        ));
    }

    if spec.domainname().is_some() && !namespaces.contains(&LinuxNamespaceType::Uts) {
        return Err(Error::Invalid(
            "domainname is set but the container has no UTS namespace".to_string(),
        ));
    }

    if let Some(propagation) = linux.rootfs_propagation() {
        if !PROPAGATIONS.contains(&propagation.as_str()) {
            return Err(Error::Invalid(format!(
                "linux.rootfsPropagation is {propagation:?}, expected one of {}",
                PROPAGATIONS.join(", ")
            )));
        }
    }

    for (field, paths) in [
        ("linux.maskedPaths", linux.masked_paths()),
        ("linux.readonlyPaths", linux.readonly_paths()),
    ] {
        for path in paths.iter().flatten() {
            if !Path::new(path).is_absolute() {
                return Err(Error::Invalid(format!(
                    "{field} entry {path:?} is not an absolute path"
                )));
            }
        }
    }

    Ok(())
}

fn platform(spec: &Spec) -> Result<()> {
    let foreign = [
        ("solaris", spec.solaris().is_some()),
        ("windows", spec.windows().is_some()),
        ("vm", spec.vm().is_some()),
        ("zos", spec.zos().is_some()),
    ];

    for (name, present) in foreign {
        if present {
            return Err(Error::OutOfScope(match name {
                "solaris" => "the solaris section of config.json",
                "windows" => "the windows section of config.json",
                "vm" => "the vm section of config.json",
                _ => "the zos section of config.json",
            }));
        }
    }

    Ok(())
}

pub fn version(version: &str) -> Result<()> {
    let numeric = version.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<&str> = numeric.split('.').collect();

    if parts.len() != 3 || parts.iter().any(|part| part.parse::<u64>().is_err()) {
        return Err(Error::Invalid(format!(
            "ociVersion {version:?} is not a major.minor.patch version"
        )));
    }

    let major: u64 = parts[0].parse().unwrap();

    if major != SUPPORTED_MAJOR {
        return Err(Error::Invalid(format!(
            "ociVersion {version:?} declares runtime-spec major version {major}; mars implements \
             {SUPPORTED_MAJOR}.x"
        )));
    }

    Ok(())
}

fn namespace_types(spec: &Spec) -> Result<HashSet<LinuxNamespaceType>> {
    let mut seen = HashSet::new();

    let namespaces = spec
        .linux()
        .as_ref()
        .and_then(|linux| linux.namespaces().as_ref());

    for namespace in namespaces.into_iter().flatten() {
        if !seen.insert(namespace.typ()) {
            return Err(Error::Invalid(format!(
                "namespace type {:?} is listed more than once",
                namespace.typ()
            )));
        }
    }

    Ok(seen)
}

fn process(spec: &Spec) -> Result<()> {
    let Some(process) = spec.process().as_ref() else {
        return Ok(());
    };

    let args = process.args().as_deref().unwrap_or(&[]);

    if args.is_empty() {
        return Err(Error::SpecField("process.args"));
    }

    if args[0].is_empty() {
        return Err(Error::Invalid(
            "process.args[0] is empty; there is no program to execute".to_string(),
        ));
    }

    if !process.cwd().is_absolute() {
        return Err(Error::Invalid(format!(
            "process.cwd {:?} is not an absolute path",
            process.cwd().display()
        )));
    }

    for entry in process.env().iter().flatten() {
        if !entry.contains('=') {
            return Err(Error::Invalid(format!(
                "process.env entry {entry:?} is not in KEY=VALUE form"
            )));
        }
    }

    let mut limits = Vec::new();

    for rlimit in process.rlimits().iter().flatten() {
        if limits.contains(&rlimit.typ()) {
            return Err(Error::Invalid(format!(
                "process.rlimits lists {:?} more than once",
                rlimit.typ()
            )));
        }
        limits.push(rlimit.typ());

        if rlimit.soft() > rlimit.hard() {
            return Err(Error::Invalid(format!(
                "process.rlimits {:?} has a soft limit ({}) above its hard limit ({})",
                rlimit.typ(),
                rlimit.soft(),
                rlimit.hard()
            )));
        }
    }

    Ok(())
}

fn mounts(spec: &Spec) -> Result<()> {
    for mount in spec.mounts().iter().flatten() {
        if !mount.destination().is_absolute() {
            return Err(Error::Invalid(format!(
                "mount destination {:?} is not an absolute path",
                mount.destination().display()
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::{LinuxBuilder, LinuxNamespaceBuilder, MountBuilder, ProcessBuilder};
    use std::path::PathBuf;

    fn valid() -> Spec {
        let mut spec = Spec::default();
        spec.set_version("1.0.2".to_string());
        spec
    }

    #[test]
    fn the_generated_default_spec_is_valid() {
        spec(&valid()).unwrap();
    }

    #[test]
    fn accepts_the_1_x_series_and_ignores_prerelease_suffixes() {
        version("1.0.0").unwrap();
        version("1.2.0").unwrap();
        version("1.0.2-dev").unwrap();
    }

    #[test]
    fn rejects_other_major_versions_and_malformed_strings() {
        assert!(version("2.0.0").is_err());
        assert!(version("0.9.0").is_err());
        assert!(version("1.0").is_err());
        assert!(version("").is_err());
        assert!(version("one.two.three").is_err());
    }

    #[test]
    fn duplicate_namespace_types_are_rejected() {
        let mut spec = valid();
        let namespaces = vec![
            LinuxNamespaceBuilder::default()
                .typ(LinuxNamespaceType::Pid)
                .build()
                .unwrap(),
            LinuxNamespaceBuilder::default()
                .typ(LinuxNamespaceType::Pid)
                .build()
                .unwrap(),
        ];
        spec.set_linux(Some(
            LinuxBuilder::default()
                .namespaces(namespaces)
                .build()
                .unwrap(),
        ));

        let error = spec_err(&spec);
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn hostname_without_a_uts_namespace_would_rename_the_host() {
        let mut spec = valid();
        spec.set_hostname(Some("guest".to_string()));
        spec.set_linux(Some(
            LinuxBuilder::default().namespaces(vec![]).build().unwrap(),
        ));

        let error = spec_err(&spec);
        assert!(error.contains("UTS namespace"), "{error}");
    }

    #[test]
    fn relative_cwd_is_rejected() {
        let mut spec = valid();
        spec.set_process(Some(
            ProcessBuilder::default()
                .cwd(PathBuf::from("relative/dir"))
                .args(vec!["/bin/sh".to_string()])
                .build()
                .unwrap(),
        ));

        let error = spec_err(&spec);
        assert!(error.contains("not an absolute path"), "{error}");
    }

    #[test]
    fn empty_args_leaves_nothing_to_execute() {
        let mut spec = valid();
        spec.set_process(Some(
            ProcessBuilder::default()
                .cwd(PathBuf::from("/"))
                .args(Vec::<String>::new())
                .build()
                .unwrap(),
        ));

        assert!(matches!(
            super::spec(&spec),
            Err(Error::SpecField("process.args"))
        ));
    }

    #[test]
    fn env_entries_must_be_key_value_pairs() {
        let mut spec = valid();
        spec.set_process(Some(
            ProcessBuilder::default()
                .cwd(PathBuf::from("/"))
                .args(vec!["/bin/sh".to_string()])
                .env(vec!["PATH=/bin".to_string(), "BROKEN".to_string()])
                .build()
                .unwrap(),
        ));

        let error = spec_err(&spec);
        assert!(error.contains("KEY=VALUE"), "{error}");
    }

    #[test]
    fn relative_mount_destinations_are_rejected() {
        let mut spec = valid();
        spec.set_mounts(Some(vec![
            MountBuilder::default()
                .destination(PathBuf::from("proc"))
                .typ("proc".to_string())
                .build()
                .unwrap(),
        ]));

        let error = spec_err(&spec);
        assert!(error.contains("not an absolute path"), "{error}");
    }

    #[test]
    fn an_unknown_rootfs_propagation_is_rejected() {
        let mut spec = valid();
        spec.set_linux(Some(
            LinuxBuilder::default()
                .rootfs_propagation("recursive".to_string())
                .build()
                .unwrap(),
        ));

        let error = spec_err(&spec);
        assert!(error.contains("rootfsPropagation"), "{error}");
    }

    #[test]
    fn a_missing_root_is_reported_as_a_missing_field() {
        let mut spec = valid();
        spec.set_root(None);

        assert!(matches!(super::spec(&spec), Err(Error::SpecField("root"))));
    }

    fn spec_err(candidate: &Spec) -> String {
        super::spec(candidate).unwrap_err().to_string()
    }
}
