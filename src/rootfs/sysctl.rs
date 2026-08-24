use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, IoContext, Result};

const BASE: &str = "/proc/sys";

pub fn apply(settings: &HashMap<String, String>) -> Result<()> {
    for (key, value) in settings {
        let target = resolve(key)?;

        fs::write(&target, value).ctx(format!(
            "write sysctl {key}={value} to {}; the namespace that owns this knob must belong to \
             the container",
            target.display()
        ))?;
    }

    Ok(())
}

pub fn resolve(key: &str) -> Result<PathBuf> {
    if key.is_empty() {
        return Err(Error::Invalid("an empty sysctl key".to_string()));
    }

    let relative = PathBuf::from(key.replace('.', "/"));

    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(Error::Invalid(format!(
                    "sysctl key {key:?} resolves outside {BASE}"
                )));
            }
        }
    }

    Ok(Path::new(BASE).join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_become_path_separators() {
        assert_eq!(
            resolve("net.ipv4.ip_forward").unwrap(),
            PathBuf::from("/proc/sys/net/ipv4/ip_forward")
        );
        assert_eq!(
            resolve("kernel.domainname").unwrap(),
            PathBuf::from("/proc/sys/kernel/domainname")
        );
    }

    #[test]
    fn a_key_cannot_escape_proc_sys() {
        assert!(resolve("...kernel.hostname").is_err());
        assert!(resolve("").is_err());
        assert!(resolve("/absolute.thing").is_err());
    }
}
