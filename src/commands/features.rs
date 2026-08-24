use serde_json::json;

use crate::OCI_VERSION;
use crate::error::Result;

pub fn run() -> Result<()> {
    let features = json!({
        "ociVersionMin": "1.0.0",
        "ociVersionMax": OCI_VERSION,
        "hooks": [
            "prestart",
            "createRuntime",
            "createContainer",
            "startContainer",
            "poststart",
            "poststop"
        ],
        "mountOptions": [
            "async", "atime", "bind", "defaults", "dev", "diratime", "dirsync", "exec",
            "mand", "noatime", "nodev", "nodiratime", "noexec", "nomand", "norelatime",
            "nostrictatime", "nosuid", "private", "rbind", "relatime", "remount", "ro",
            "rprivate", "rshared", "rslave", "runbindable", "rw", "shared", "slave",
            "strictatime", "suid", "sync", "unbindable"
        ],
        "linux": {
            "namespaces": ["cgroup", "ipc", "mount", "network", "pid", "user", "uts"],
            "capabilities": null,
            "cgroup": {
                "v1": false,
                "v2": true,
                "systemd": false,
                "systemdUser": false,
                "rdma": false
            },
            "seccomp": null,
            "apparmor": { "enabled": false },
            "selinux": { "enabled": false },
            "intelRdt": { "enabled": false },
            "mountExtensions": {
                "idmap": { "enabled": false }
            }
        },
        "annotations": {
            "dev.mars.overlay.lowerdir": "colon-separated lower layers, topmost first",
            "dev.mars.overlay.upperdir": "writable layer; omit for a read-only rootfs",
            "dev.mars.overlay.workdir": "overlayfs scratch space, same filesystem as upperdir"
        }
    });

    println!("{}", serde_json::to_string_pretty(&features)?);

    Ok(())
}
