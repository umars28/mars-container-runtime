pub mod validate;

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use oci_spec::runtime::{LinuxDevice, LinuxNamespace, LinuxResources, Mount, Spec};

use crate::error::{Error, IoContext, Result};
use crate::rootfs::overlay::{self, Layers};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct Bundle {
    pub dir: PathBuf,
    pub spec: Spec,
}

impl Bundle {
    pub fn load(dir: &Path) -> Result<Self> {
        let dir = fs::canonicalize(dir).ctx(format!("resolve bundle {}", dir.display()))?;
        let config = dir.join("config.json");

        if !config.is_file() {
            return Err(Error::MissingConfig(dir));
        }

        let text = fs::read_to_string(&config).ctx(format!("read {}", config.display()))?;
        let spec: Spec = serde_json::from_str(&text)?;

        validate::spec(&spec)?;

        Ok(Self { dir, spec })
    }

    pub fn annotations(&self) -> HashMap<String, String> {
        self.spec.annotations().clone().unwrap_or_default()
    }

    pub fn overlay(&self) -> Result<Option<Layers>> {
        overlay::from_annotations(&self.annotations(), &self.dir)
    }

    pub fn rootfs(&self) -> Result<PathBuf> {
        let root = self.spec.root().as_ref().ok_or(Error::SpecField("root"))?;

        let path = root.path();

        Ok(if path.is_absolute() {
            path.clone()
        } else {
            self.dir.join(path)
        })
    }

    pub fn namespaces(&self) -> Vec<LinuxNamespace> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.namespaces().clone())
            .unwrap_or_default()
    }

    pub fn mounts(&self) -> Vec<Mount> {
        self.spec.mounts().clone().unwrap_or_default()
    }

    pub fn devices(&self) -> Vec<LinuxDevice> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.devices().clone())
            .unwrap_or_default()
    }

    pub fn resources(&self) -> Option<LinuxResources> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.resources().clone())
    }

    pub fn cgroups_path(&self) -> Option<PathBuf> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.cgroups_path().clone())
    }

    pub fn hooks(&self) -> Option<&oci_spec::runtime::Hooks> {
        self.spec.hooks().as_ref()
    }

    pub fn terminal(&self) -> bool {
        self.spec
            .process()
            .as_ref()
            .and_then(|process| process.terminal())
            .unwrap_or(false)
    }

    pub fn readonly_rootfs(&self) -> bool {
        self.spec
            .root()
            .as_ref()
            .and_then(|root| root.readonly())
            .unwrap_or(false)
    }

    pub fn masked_paths(&self) -> Vec<String> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.masked_paths().clone())
            .unwrap_or_default()
    }

    pub fn readonly_paths(&self) -> Vec<String> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.readonly_paths().clone())
            .unwrap_or_default()
    }

    pub fn seccomp(&self) -> Option<&oci_spec::runtime::LinuxSeccomp> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.seccomp().as_ref())
    }

    pub fn id_mappings(&self) -> crate::container::userns::Mappings {
        let linux = self.spec.linux().as_ref();

        crate::container::userns::Mappings {
            uid: linux
                .and_then(|linux| linux.uid_mappings().clone())
                .unwrap_or_default(),
            gid: linux
                .and_then(|linux| linux.gid_mappings().clone())
                .unwrap_or_default(),
        }
    }

    pub fn process(&self) -> Option<&oci_spec::runtime::Process> {
        self.spec.process().as_ref()
    }

    pub fn time_offsets(&self) -> HashMap<String, oci_spec::runtime::LinuxTimeOffset> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.time_offsets().clone())
            .unwrap_or_default()
    }

    pub fn sysctl(&self) -> HashMap<String, String> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.sysctl().clone())
            .unwrap_or_default()
    }

    pub fn rootfs_propagation(&self) -> Option<&str> {
        self.spec
            .linux()
            .as_ref()
            .and_then(|linux| linux.rootfs_propagation().as_deref())
    }

    pub fn domainname(&self) -> Option<&str> {
        self.spec.domainname().as_deref()
    }

    pub fn hostname(&self) -> Option<&str> {
        self.spec.hostname().as_deref()
    }

    pub fn cwd(&self) -> Option<PathBuf> {
        self.spec
            .process()
            .as_ref()
            .map(|process| process.cwd().clone())
    }

    pub fn env(&self) -> Vec<String> {
        self.spec
            .process()
            .as_ref()
            .and_then(|process| process.env().clone())
            .unwrap_or_default()
    }

    pub fn argv(&self) -> Result<Vec<String>> {
        let process = self
            .spec
            .process()
            .as_ref()
            .ok_or(Error::SpecField("process"))?;

        let args = process.args().clone().unwrap_or_default();

        if args.is_empty() {
            return Err(Error::SpecField("process.args"));
        }

        Ok(args)
    }
}

pub fn resolve_executable(program: &str, env: &[String], own_rootfs: bool) -> Result<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);

        if own_rootfs && !path.is_file() {
            return Err(Error::ExecutableNotFound(program.to_string()));
        }

        return Ok(path);
    }

    let search = env
        .iter()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .unwrap_or(DEFAULT_PATH);

    for dir in search.split(':').filter(|dir| !dir.is_empty()) {
        let candidate = Path::new(dir).join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(Error::ExecutableNotFound(program.to_string()))
}

pub fn to_cstrings(values: &[String]) -> Result<Vec<CString>> {
    values
        .iter()
        .map(|value| CString::new(value.as_str()).map_err(|_| Error::NulByte(value.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_path_is_used_as_given_when_it_exists() {
        let resolved = resolve_executable("/bin/sh", &[], true).unwrap();
        assert_eq!(resolved, PathBuf::from("/bin/sh"));
    }

    #[test]
    fn an_explicit_path_that_does_not_exist_fails_here_rather_than_at_execve() {
        let error = resolve_executable("/definitely/not/here", &[], true).unwrap_err();
        assert!(matches!(error, Error::ExecutableNotFound(name) if name == "/definitely/not/here"));

        let error = resolve_executable("./nope.sh", &[], true).unwrap_err();
        assert!(matches!(error, Error::ExecutableNotFound(_)));
    }

    #[test]
    fn a_joined_mount_namespace_is_not_ours_to_validate() {
        let resolved = resolve_executable("/definitely/not/here", &[], false).unwrap();
        assert_eq!(resolved, PathBuf::from("/definitely/not/here"));
    }

    #[test]
    fn missing_program_reports_the_name() {
        let error = resolve_executable("definitely-not-a-real-binary", &[], true).unwrap_err();
        assert!(
            matches!(error, Error::ExecutableNotFound(name) if name == "definitely-not-a-real-binary")
        );
    }

    #[test]
    fn path_comes_from_spec_env_when_present() {
        let env = vec!["HOME=/root".to_string(), "PATH=/sbin:/bin".to_string()];
        let error = resolve_executable("nope-not-here", &env, true).unwrap_err();
        assert!(matches!(error, Error::ExecutableNotFound(_)));
    }

    #[test]
    fn rejects_nul_bytes_in_argv() {
        let error = to_cstrings(&["ok".to_string(), "b\0ad".to_string()]).unwrap_err();
        assert!(matches!(error, Error::NulByte(_)));
    }
}
