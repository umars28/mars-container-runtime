use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const STATE_FILE: &str = "state.json";
pub const EXEC_FIFO: &str = "exec.fifo";
pub const PID_FILE: &str = "init.pid";

#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn state_file(&self, id: &str) -> PathBuf {
        self.container_dir(id).join(STATE_FILE)
    }

    pub fn exec_fifo(&self, id: &str) -> PathBuf {
        self.container_dir(id).join(EXEC_FIFO)
    }

    pub fn pid_file(&self, id: &str) -> PathBuf {
        self.container_dir(id).join(PID_FILE)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.state_file(id).is_file()
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));

    if ok {
        Ok(())
    } else {
        Err(Error::InvalidId(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_docker_style_ids() {
        validate_id("a1b2c3d4e5f6").unwrap();
        validate_id("my-container_01.v2+x").unwrap();
    }

    #[test]
    fn rejects_path_traversal() {
        for bad in ["", ".", "..", "a/b", "../etc/passwd", "with space", "n\0ul"] {
            assert!(validate_id(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn layout_places_state_under_id() {
        let l = Layout::new("/run/mars");
        assert_eq!(
            l.state_file("abc"),
            PathBuf::from("/run/mars/abc/state.json")
        );
        assert_eq!(l.exec_fifo("abc"), PathBuf::from("/run/mars/abc/exec.fifo"));
    }
}
