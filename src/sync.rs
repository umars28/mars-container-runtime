use std::os::fd::OwnedFd;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const MAX_MESSAGE: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    RequestUserMapping,
    UserMappingDone,
    InitPid(i32),
    CgroupApplied,
    InitReady,
    Start,
    Failed(String),
}

pub struct Channel {
    fd: OwnedFd,
}

pub fn pair() -> Result<(Channel, Channel)> {
    let (a, b) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?;

    Ok((Channel { fd: a }, Channel { fd: b }))
}

impl Channel {
    pub fn send(&self, message: &Message) -> Result<()> {
        let buf = serde_json::to_vec(message)?;
        nix::unistd::write(&self.fd, &buf)?;
        Ok(())
    }

    pub fn recv(&self) -> Result<Message> {
        let mut buf = [0u8; MAX_MESSAGE];
        let n = nix::unistd::read(&self.fd, &mut buf)?;

        if n == 0 {
            return Err(Error::SyncClosed);
        }

        Ok(serde_json::from_slice(&buf[..n])?)
    }

    pub fn expect(&self, want: &'static str) -> Result<Message> {
        match self.recv()? {
            Message::Failed(reason) => Err(Error::InitFailed(reason)),
            other => Ok(other),
        }
        .and_then(|m| match (&m, want) {
            (Message::UserMappingDone, "UserMappingDone")
            | (Message::InitPid(_), "InitPid")
            | (Message::CgroupApplied, "CgroupApplied")
            | (Message::InitReady, "InitReady")
            | (Message::Start, "Start")
            | (Message::RequestUserMapping, "RequestUserMapping") => Ok(m),
            _ => Err(Error::Sync(format!("expected {want}, got {m:?}"))),
        })
    }
}
