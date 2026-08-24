use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("container {0} does not exist")]
    NotFound(String),

    #[error("container {0} already exists")]
    AlreadyExists(String),

    #[error("invalid container id {0:?}: must be non-empty and match [a-zA-Z0-9._+-]+")]
    InvalidId(String),

    #[error("container {id} is {actual}, expected {expected}")]
    BadState {
        id: String,
        actual: String,
        expected: String,
    },

    #[error("bundle {0} has no config.json")]
    MissingConfig(PathBuf),

    #[error("{0} is deliberately out of scope for mars")]
    OutOfScope(&'static str),

    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),

    #[error("cgroup v2 unified hierarchy not found at /sys/fs/cgroup")]
    NoCgroupV2,

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Syscall(#[from] nix::Error),

    #[error(transparent)]
    Spec(#[from] oci_spec::OciSpecError),
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait IoContext<T> {
    fn ctx(self, context: impl Into<String>) -> Result<T>;
}

impl<T> IoContext<T> for std::result::Result<T, std::io::Error> {
    fn ctx(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context.into(),
            source,
        })
    }
}
