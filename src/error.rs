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

    #[error("config.json is missing required field {0}")]
    SpecField(&'static str),

    #[error("config.json is invalid: {0}")]
    Invalid(String),

    #[error("overlay rootfs: {0}")]
    Overlay(String),

    #[error("rootfs {0} does not exist or is not a directory")]
    RootfsMissing(PathBuf),

    #[error("executable {0:?} not found in the container PATH")]
    ExecutableNotFound(String),

    #[error("{0:?} contains a nul byte and cannot be passed to execve")]
    NulByte(String),

    #[error("container init failed: {0}")]
    InitFailed(String),

    #[error("synchronisation error: {0}")]
    Sync(String),

    #[error("synchronisation channel closed before the container reported readiness")]
    SyncClosed,

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

    #[error("{context}: {source}")]
    Nix {
        context: String,
        #[source]
        source: nix::Error,
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

pub trait NixContext<T> {
    fn ctx(self, context: impl Into<String>) -> Result<T>;
}

impl<T> NixContext<T> for std::result::Result<T, nix::Error> {
    fn ctx(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|source| Error::Nix {
            context: context.into(),
            source,
        })
    }
}
