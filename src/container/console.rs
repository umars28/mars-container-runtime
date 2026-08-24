use std::io::IoSlice;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use nix::mount::{MsFlags, mount};
use nix::pty::{OpenptyResult, openpty};
use nix::sys::socket::{
    AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr, connect, sendmsg, socket,
};
use nix::unistd::{dup2_stderr, dup2_stdin, dup2_stdout, setsid};

use crate::error::{Error, IoContext, NixContext, Result};

pub fn open_socket(socket_path: &Path) -> Result<OwnedFd> {
    let stream = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .ctx("create a socket for the console handover")?;

    let address = UnixAddr::new(socket_path).ctx(format!(
        "resolve the console socket {}",
        socket_path.display()
    ))?;

    connect(stream.as_raw_fd(), &address).ctx(format!(
        "connect to the console socket {}; the caller must already be listening on it",
        socket_path.display()
    ))?;

    Ok(stream)
}

pub fn setup(socket: &OwnedFd) -> Result<()> {
    let OpenptyResult { master, slave } = openpty(None, None).ctx(
        "openpty inside the container; this must run after pivot_root so the slave is allocated \
         from the container's own devpts instance and has a name there",
    )?;

    let name = pty_name(&master)?;
    send_master(socket, &master, &name)?;
    drop(master);

    tracing::debug!(console = %name, "pty allocated inside the container, master sent to the caller");

    bind_console(&name)?;
    adopt(slave)?;

    Ok(())
}

fn pty_name(master: &OwnedFd) -> Result<String> {
    let mut buffer = [0_u8; 128];

    let rc = unsafe {
        libc::ptsname_r(
            master.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len() - 1,
        )
    };

    if rc != 0 {
        return Err(Error::Invalid(
            "ptsname_r failed for the pty master".to_string(),
        ));
    }

    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);

    Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

fn send_master(socket: &OwnedFd, master: &OwnedFd, name: &str) -> Result<()> {
    let fds = [master.as_raw_fd()];
    let control = [ControlMessage::ScmRights(&fds)];

    sendmsg::<UnixAddr>(
        socket.as_raw_fd(),
        &[IoSlice::new(name.as_bytes())],
        &control,
        MsgFlags::empty(),
        None,
    )
    .ctx("send the pty master over SCM_RIGHTS")?;

    Ok(())
}

fn bind_console(slave_path: &str) -> Result<()> {
    let target = Path::new("/dev/console");

    if !target.exists() {
        std::fs::File::create(target).ctx("create /dev/console as a bind target")?;
    }

    mount(
        Some(Path::new(slave_path)),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .ctx(format!(
        "bind {slave_path} onto /dev/console so anything opening the container's console reaches \
         the pty"
    ))
}

pub fn adopt(slave: OwnedFd) -> Result<()> {
    setsid().ctx("setsid to become a session leader before claiming the terminal")?;

    let rc = unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0) };

    if rc != 0 {
        return Err(Error::Invalid(
            "ioctl(TIOCSCTTY) failed; the process could not claim the pty as its controlling \
             terminal"
                .to_string(),
        ));
    }

    dup2_stdin(&slave).ctx("dup the pty onto stdin")?;
    dup2_stdout(&slave).ctx("dup the pty onto stdout")?;
    dup2_stderr(&slave).ctx("dup the pty onto stderr")?;

    if slave.as_raw_fd() > 2 {
        drop(slave);
    } else {
        std::mem::forget(slave);
    }

    Ok(())
}
