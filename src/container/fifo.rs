use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use nix::unistd::{mkfifo, read, write};

use crate::error::{Error, IoContext, NixContext, Result};

const HANDSHAKE: u8 = b'0';
const MODE: u32 = 0o622;

pub fn create(path: &Path) -> Result<OwnedFd> {
    if path.exists() {
        std::fs::remove_file(path).ctx(format!("remove a stale {}", path.display()))?;
    }

    mkfifo(path, Mode::from_bits_truncate(MODE)).ctx(format!(
        "mkfifo {}; mode {MODE:04o} so a container init that runs as a non-root user and has \
         dropped CAP_DAC_OVERRIDE can still open it for writing, with the root-owned state \
         directory providing the real protection",
        path.display()
    ))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(MODE)).ctx(format!(
        "chmod {} to {MODE:04o}; mkfifo(3) masks its mode with the umask, which would leave it \
         {:04o} and unopenable by an unprivileged container",
        path.display(),
        MODE & !0o022
    ))?;

    open(path, OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty()).ctx(format!(
        "open {} with O_PATH to keep a handle that does not count as an opener",
        path.display()
    ))
}

pub fn park(handle: &OwnedFd) -> Result<()> {
    let magic = format!("/proc/self/fd/{}", handle.as_raw_fd());

    let writer = open(magic.as_str(), OFlag::O_WRONLY, Mode::empty()).ctx(format!(
        "reopen the exec fifo through {magic}; this needs /proc mounted inside the container"
    ))?;

    write(&writer, &[HANDSHAKE]).ctx("announce readiness on the exec fifo")?;

    Ok(())
}

pub fn release(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(Error::Invalid(format!(
            "{} is gone, so the container was already started",
            path.display()
        )));
    }

    let reader = open(path, OFlag::O_RDONLY, Mode::empty()).ctx(format!(
        "open the exec fifo {} to start the container",
        path.display()
    ))?;

    let mut buffer = [0_u8; 1];
    let read_bytes = read(&reader, &mut buffer).ctx("read the exec fifo handshake")?;

    if read_bytes == 0 {
        return Err(Error::InitFailed(
            "the container init closed the exec fifo without writing; it died between create and \
             start"
                .to_string(),
        ));
    }

    if buffer[0] != HANDSHAKE {
        return Err(Error::Sync(format!(
            "unexpected byte {:#04x} on the exec fifo",
            buffer[0]
        )));
    }

    std::fs::remove_file(path).ctx(format!(
        "remove {} so start cannot run twice",
        path.display()
    ))
}
