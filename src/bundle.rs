use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use oci_spec::runtime::{LinuxDevice, LinuxNamespace, LinuxResources, Mount, Spec};

use crate::error::{Error, IoContext, Result};

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

        Ok(Self { dir, spec })
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

pub fn resolve_executable(program: &str, env: &[String]) -> Result<PathBuf> {
    if program.contains('/') {
        return Ok(PathBuf::from(program));
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
    fn absolute_program_is_returned_verbatim() {
        let resolved = resolve_executable("/bin/sh", &[]).unwrap();
        assert_eq!(resolved, PathBuf::from("/bin/sh"));
    }

    #[test]
    fn relative_program_with_slash_is_not_searched() {
        let resolved = resolve_executable("./run.sh", &[]).unwrap();
        assert_eq!(resolved, PathBuf::from("./run.sh"));
    }

    #[test]
    fn missing_program_reports_the_name() {
        let error = resolve_executable("definitely-not-a-real-binary", &[]).unwrap_err();
        assert!(
            matches!(error, Error::ExecutableNotFound(name) if name == "definitely-not-a-real-binary")
        );
    }

    #[test]
    fn path_comes_from_spec_env_when_present() {
        let env = vec!["HOME=/root".to_string(), "PATH=/sbin:/bin".to_string()];
        let error = resolve_executable("nope-not-here", &env).unwrap_err();
        assert!(matches!(error, Error::ExecutableNotFound(_)));
    }

    #[test]
    fn rejects_nul_bytes_in_argv() {
        let error = to_cstrings(&["ok".to_string(), "b\0ad".to_string()]).unwrap_err();
        assert!(matches!(error, Error::NulByte(_)));
    }
}
