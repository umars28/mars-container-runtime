use std::path::Path;

use nix::fcntl::{OFlag, open};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sys::stat::Mode;
use nix::unistd::{chdir, fchdir, pivot_root};

use crate::error::{NixContext, Result};

pub fn make_root_private() -> Result<()> {
    mount(
        None::<&Path>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .ctx("remount / as MS_PRIVATE|MS_REC to stop propagation to the host")?;

    Ok(())
}

pub fn make_mount_point(rootfs: &Path) -> Result<()> {
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .ctx(format!(
        "bind {} onto itself so pivot_root has a mount point",
        rootfs.display()
    ))?;

    Ok(())
}

pub fn pivot(rootfs: &Path) -> Result<()> {
    let oldroot = open(
        "/",
        OFlag::O_DIRECTORY | OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ctx("open / to keep a handle on the old root")?;

    let newroot = open(
        rootfs,
        OFlag::O_DIRECTORY | OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ctx(format!("open rootfs {}", rootfs.display()))?;

    fchdir(&newroot).ctx("chdir into the new root")?;
    pivot_root(".", ".").ctx("pivot_root(\".\", \".\")")?;

    fchdir(&oldroot).ctx("chdir back to the old root via its fd")?;
    mount(
        None::<&Path>,
        ".",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .ctx("mark the old root private before detaching it")?;

    umount2(".", MntFlags::MNT_DETACH).ctx("detach the old root")?;
    chdir("/").ctx("chdir to the new /")?;

    Ok(())
}

pub fn set_propagation(requested: Option<&str>) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };

    let flag = match requested {
        "shared" => MsFlags::MS_SHARED,
        "slave" => MsFlags::MS_SLAVE,
        "private" => MsFlags::MS_PRIVATE,
        "unbindable" => MsFlags::MS_UNBINDABLE,
        other => {
            return Err(crate::error::Error::Invalid(format!(
                "linux.rootfsPropagation {other:?} is not one of shared, slave, private, unbindable"
            )));
        }
    };

    mount(
        None::<&Path>,
        "/",
        None::<&str>,
        flag | MsFlags::MS_REC,
        None::<&str>,
    )
    .ctx(format!("apply rootfsPropagation={requested} to /"))
}
