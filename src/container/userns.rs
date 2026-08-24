use std::fs;
use std::path::PathBuf;
use std::process::Command;

use nix::unistd::Pid;
use oci_spec::runtime::LinuxIdMapping;

use crate::error::{Error, IoContext, Result};

pub struct Mappings {
    pub uid: Vec<LinuxIdMapping>,
    pub gid: Vec<LinuxIdMapping>,
}

impl Mappings {
    pub fn is_empty(&self) -> bool {
        self.uid.is_empty() && self.gid.is_empty()
    }
}

pub fn write(pid: Pid, mappings: &Mappings, privileged: bool) -> Result<()> {
    if mappings.uid.is_empty() || mappings.gid.is_empty() {
        return Err(Error::Invalid(
            "a user namespace needs both linux.uidMappings and linux.gidMappings; the kernel \
             leaves an unmapped namespace with no usable ids at all"
                .to_string(),
        ));
    }

    deny_setgroups(pid, privileged)?;

    write_one(pid, "uid_map", &mappings.uid, privileged, "newuidmap")?;
    write_one(pid, "gid_map", &mappings.gid, privileged, "newgidmap")?;

    Ok(())
}

fn deny_setgroups(pid: Pid, privileged: bool) -> Result<()> {
    if privileged {
        return Ok(());
    }

    let path = format!("/proc/{}/setgroups", pid.as_raw());

    fs::write(&path, "deny").ctx(format!(
        "write deny to {path}; without it an unprivileged process cannot write gid_map, because \
         dropping a group could grant access that a negative group permission was denying"
    ))
}

fn write_one(
    pid: Pid,
    file: &str,
    mappings: &[LinuxIdMapping],
    privileged: bool,
    helper: &str,
) -> Result<()> {
    let path = format!("/proc/{}/{file}", pid.as_raw());
    let body = render(mappings);

    match fs::write(&path, &body) {
        Ok(()) => {
            tracing::debug!(target = %path, mappings = mappings.len(), "id mapping written directly");
            return Ok(());
        }
        Err(error) if privileged => {
            return Err(Error::Io {
                context: format!("write {body:?} to {path}"),
                source: error,
            });
        }
        Err(error) => {
            tracing::debug!(
                "writing {path} directly failed ({error}); falling back to {helper}, which is \
                 setuid and consults /etc/subuid"
            );
        }
    }

    run_helper(helper, pid, mappings)
}

fn run_helper(helper: &str, pid: Pid, mappings: &[LinuxIdMapping]) -> Result<()> {
    let binary = locate(helper).ok_or_else(|| {
        Error::Invalid(format!(
            "{helper} is not installed, and this process may not write more than one id mapping \
             directly; install the uidmap package or reduce the mapping to a single line for your \
             own id"
        ))
    })?;

    let mut command = Command::new(&binary);
    command.arg(pid.as_raw().to_string());

    for mapping in mappings {
        command.arg(mapping.container_id().to_string());
        command.arg(mapping.host_id().to_string());
        command.arg(mapping.size().to_string());
    }

    let output = command.output().ctx(format!("run {}", binary.display()))?;

    if output.status.success() {
        tracing::debug!(helper = %binary.display(), "id mapping written via the setuid helper");
        return Ok(());
    }

    Err(Error::Invalid(format!(
        "{} exited with {}: {}; the ranges must be inside what /etc/subuid and /etc/subgid grant \
         this user",
        binary.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn locate(helper: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin"]
        .iter()
        .map(|dir| PathBuf::from(dir).join(helper))
        .find(|candidate| candidate.is_file())
}

pub fn render(mappings: &[LinuxIdMapping]) -> String {
    let mut out = String::new();

    for mapping in mappings {
        out.push_str(&format!(
            "{} {} {}\n",
            mapping.container_id(),
            mapping.host_id(),
            mapping.size()
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::LinuxIdMappingBuilder;

    fn mapping(container: u32, host: u32, size: u32) -> LinuxIdMapping {
        LinuxIdMappingBuilder::default()
            .container_id(container)
            .host_id(host)
            .size(size)
            .build()
            .unwrap()
    }

    #[test]
    fn mappings_render_in_the_kernels_three_column_format() {
        let rendered = render(&[mapping(0, 1000, 1), mapping(1, 100000, 65536)]);
        assert_eq!(rendered, "0 1000 1\n1 100000 65536\n");
    }

    #[test]
    fn an_empty_mapping_list_renders_to_nothing() {
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn a_user_namespace_without_both_maps_is_rejected() {
        let error = write(
            Pid::from_raw(1),
            &Mappings {
                uid: vec![mapping(0, 1000, 1)],
                gid: Vec::new(),
            },
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("gidMappings"), "{error}");
    }

    #[test]
    fn the_helpers_are_looked_up_on_the_usual_paths() {
        assert!(locate("sh").is_some());
        assert!(locate("definitely-not-a-real-helper").is_none());
    }
}
