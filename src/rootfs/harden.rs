use std::path::Path;

use nix::mount::{MsFlags, mount};

use crate::error::{NixContext, Result};

pub fn mask_paths(paths: &[String]) -> Result<()> {
    for path in paths {
        mask_one(Path::new(path))?;
    }

    Ok(())
}

fn mask_one(path: &Path) -> Result<()> {
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };

    if metadata.is_dir() {
        return mount(
            Some("tmpfs"),
            path,
            Some("tmpfs"),
            MsFlags::MS_RDONLY,
            Some("size=0k"),
        )
        .ctx(format!(
            "mask the directory {} with an empty read-only tmpfs",
            path.display()
        ));
    }

    mount(
        Some("/dev/null"),
        path,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .ctx(format!(
        "mask {} by binding /dev/null over it",
        path.display()
    ))
}

pub fn readonly_paths(paths: &[String]) -> Result<()> {
    for path in paths {
        readonly_one(Path::new(path))?;
    }

    Ok(())
}

fn readonly_one(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Ok(());
    }

    mount(
        Some(path),
        path,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .ctx(format!(
        "bind {} onto itself; a path cannot be remounted read-only unless it is a mount point of \
         its own",
        path.display()
    ))?;

    mount(
        Some(path),
        path,
        None::<&str>,
        MsFlags::MS_BIND
            | MsFlags::MS_REC
            | MsFlags::MS_REMOUNT
            | MsFlags::MS_RDONLY
            | MsFlags::MS_NOSUID
            | MsFlags::MS_NODEV
            | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .ctx(format!("remount {} read-only", path.display()))
}

pub fn readonly_rootfs() -> Result<()> {
    mount(
        None::<&Path>,
        "/",
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .ctx("remount / read-only for root.readonly")
}

pub fn remount_readonly_mounts(rootfs_mounts: &[oci_spec::runtime::Mount]) -> Result<()> {
    for spec_mount in rootfs_mounts {
        let options = super::mounts::parse_options(spec_mount.options().as_deref().unwrap_or(&[]));

        if !options.flags.contains(MsFlags::MS_RDONLY) {
            continue;
        }

        let fstype = spec_mount.typ().clone().unwrap_or_default();

        if fstype != "tmpfs" {
            continue;
        }

        let destination = spec_mount.destination();

        mount(
            None::<&Path>,
            destination,
            None::<&str>,
            options.flags | MsFlags::MS_REMOUNT,
            None::<&str>,
        )
        .ctx(format!(
            "remount the tmpfs at {} read-only; it had to be writable while the runtime populated \
             it",
            destination.display()
        ))?;
    }

    Ok(())
}
