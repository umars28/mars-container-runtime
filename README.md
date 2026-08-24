# mars

An OCI-compliant container runtime written from scratch in Rust — the layer that Docker and
Kubernetes sit on top of, built to understand it rather than to replace it.

`mars` implements the [OCI runtime-spec](https://github.com/opencontainers/runtime-spec): it takes
a filesystem bundle and a `config.json`, then uses Linux namespaces, cgroup v2, and OverlayFS to
turn it into an isolated process. The goal is to be a drop-in `--runtime` for Docker.

> **Status: early but usable.** The full OCI lifecycle works — `create`, `start`, `state`, `kill`,
> `delete`, `exec`, `ps`, `pause`, `resume`, `events`, `update` — with namespaces, cgroup v2 limits,
> an OverlayFS rootfs assembled from image layers, capabilities, the five lifecycle hooks, and a
> console socket for `-it`. It ties `runc` on the OCI validation suite — 22 passed each, on the same
> host and the same test binaries. Still missing: seccomp, rootless, and read-only paths. See
> [Roadmap](#roadmap).

## Why build this when runc exists?

This is not a runc competitor and is not meant for production. It exists because container
failures in production happen in the layer that Docker hides:

- a pod is `OOMKilled` with exit `137` — which process sent `SIGKILL`, and why is the limit in
  `memory.max` rather than `memory.limit_in_bytes`?
- a volume mount gives `permission denied` even at mode `0777` — because a user namespace remapped
  the uid
- a container ignores `SIGTERM` — because PID 1 has no default signal handlers
- zombie processes pile up — because PID 1 never reaped its orphans
- CPU throttles while utilisation looks low — because `cpu.max` is a quota, not a share
- a fix applied with `exec` survives a restart but not a recreate — because it lived in the
  OverlayFS upper layer, which *is* the container

Reading the documentation does not build a mental model for these. Writing the runtime does.

The verifiable outputs are the point: passing the OCI validation suite, running as
`docker run --runtime=mars`, and [`docs/failure-modes.md`](docs/failure-modes.md) reproducing each
failure above with evidence read straight from the kernel. Seven are written up so far, including two
that surprised me:

**An OOM kill does not always produce exit 137.** The kernel picks its victim by badness score, so it
often kills a child rather than PID 1 — and the container then exits `0` while having lost a process.
Nothing watching exit codes can see it.

**A mount option string over 4096 bytes is truncated, not rejected.** Enough OverlayFS layers and the
kernel silently cuts the `lowerdir=` list mid-path, then reports `ENOENT` on the mount *source* — an
error that names neither the truncation nor the layer count. This is why
`/var/lib/docker/overlay2/l/` is full of short symlinks.

The validation numbers are only meaningful next to a reference, so every run is paired with `runc`
1.5.1 on the same host, same rootfs, same test binaries:

| | passed | failed | inconclusive |
|---|---|---|---|
| mars 0.1.0 | 22 | 21 | 15 |
| runc 1.5.1 | 22 | 20 | 16 |

`runtime-tools` 0.9.0 is partly stale — both runtimes fail the same 15 tests for the same
environmental reasons, written up per-test in [`docs/04-lifecycle.md`](docs/04-lifecycle.md#validation-results).
Two of the rows where mars passes and runc does not are mars *ignoring* `mountLabel` and
`apparmorProfile` rather than implementing them, which is noted there too.

## Design notes

**The three-level fork chain is forced by the kernel, not a style choice.**

```
mars create
  │  socketpair(AF_UNIX)
  ├─ fork() ─────────► [intermediate]
  │                      unshare(NEWUSER|NEWNS|NEWPID|NEWUTS|NEWIPC|NEWNET)
  │  ◄── "map me" ────   send pid, then block
  │  write /proc/<pid>/uid_map, gid_map, setgroups=deny
  │  ─── "mapped" ──►
  │                      fork() ──────────► [container init, PID 1]
  │                        exit                mount rootfs, pivot_root
  │                                            set capabilities, seccomp, no_new_privs
  │  ◄── "ready" ─────────────────────────────  block on exec fifo
mars start ───── "go" ─────────────────────────► execve(user process)
```

Two constraints produce that shape:

1. `unshare(CLONE_NEWPID)` does **not** move the caller into the new PID namespace. The next
   `fork()` is what becomes PID 1.
2. `uid_map` must be written from **outside** the user namespace by a privileged process, so the
   parent and child need two-way synchronisation — a process cannot map itself.

**Rust instead of Go, deliberately.** `runc` cannot call `setns(2)` reliably from Go, because Go is
multi-threaded by the time `main` runs and `setns` refuses to move a multi-threaded process into a new
mount or user namespace. It works around this with a C constructor (`libcontainer/nsenter`) that runs
before the Go runtime starts. Rust stays single-threaded until we fork, so `mars exec` calls `setns`
directly — and asserts the property rather than assuming it, by counting `/proc/self/task` first.

**Telemetry lives only in the parent.** The OTLP exporter runs a background thread, and after
`fork()` only async-signal-safe work is legal in the child. The child reports timings over a pipe;
the parent turns them into spans.

**The cgroup driver is hand-written** against `cgroupfs` rather than using a crate. Delegating that
away would delegate away the main thing this project is for.

**The overlay rootfs is a documented extension, not a spec feature.** The OCI runtime-spec has no
field for image layers — assembling them is the image-spec's job, which containerd and Docker do
before the runtime is ever called. `mars` reads three `dev.mars.overlay.*` annotations instead, so
the `config.json` stays valid for any other runtime, which will simply ignore them. The overlay is
mounted inside the container's mount namespace, so it never appears on the host and needs no
teardown.

## Scope

Implemented or planned:

- namespaces: mount, pid, uts, ipc, user, cgroup, net (isolated, no CNI)
- `pivot_root`, standard mounts, `maskedPaths` / `readonlyPaths`
- cgroup v2 unified: `memory.max`, `cpu.max`, `pids.max`, `cpuset`, `io.max`
- OverlayFS rootfs assembly
- full lifecycle: `create`, `start`, `state`, `kill`, `delete`, `exec`, plus console socket and hooks
- capabilities, seccomp, `no_new_privs`, rootless via user namespaces
- Docker drop-in runtime, OpenTelemetry traces of the startup path

Deliberately out of scope: image pulling from registries (that is containerd's job), CNI
networking, checkpoint/restore, CRI, cgroup v1, and the systemd cgroup driver.

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| 0 | Dev VM, toolchain, CLI surface, `spec` generation | done |
| 1 | Namespaces, `pivot_root`, PID 1 duties — [writeup](docs/01-isolation.md) | done |
| 2 | cgroup v2 driver, OOMKilled reproduction — [writeup](docs/02-cgroups.md) | done |
| 3 | OverlayFS rootfs, `config.json` validation — [writeup](docs/03-overlayfs.md) | done |
| 4 | Full lifecycle, `exec`, hooks, console, OCI validation — [writeup](docs/04-lifecycle.md) | done |
| 5 | Seccomp, rootless, read-only paths | not started |
| 6 | Docker drop-in, OpenTelemetry to Tempo | not started |
| 7 | Per-phase writeups, CI | not started |

## Development

Linux-only: it needs namespaces, cgroups, and `libseccomp`. It does not build on macOS. A
[Lima](https://lima-vm.io) VM definition is included so the environment is reproducible.

```sh
limactl start --name=mars-dev ./lima/mars-dev.yaml
limactl shell mars-dev

cargo build
cargo test
sudo -E ./tests/run-integration.sh
```

The integration suite asserts against kernel state — `/proc`, `/proc/mounts`, `ip -o link`, wait
statuses — rather than trusting what the runtime reports about itself. To drive it by hand:

```sh
mars spec --bundle /tmp/demo
./scripts/make-rootfs.sh /tmp/demo/rootfs
cd /tmp/demo && sudo mars run demo
```

For a layered rootfs, `scripts/oci-bundle.sh` unpacks a Docker image into one directory per layer,
converts the `.wh.` markers in the layer tarballs into real OverlayFS whiteouts, and writes a
`config.json` whose `process` comes from the image config:

```sh
sudo -E ./scripts/oci-bundle.sh -i alpine:3.20 /tmp/layered
cd /tmp/layered && sudo mars run demo
find /tmp/layered/diff -mindepth 1     # everything the container wrote
```

`-F` builds a throwaway multi-layer image first, so the bundle has several lower layers and a real
whiteout to look at.

Verified environment: Ubuntu 24.04, kernel 6.8, aarch64, pure cgroup v2 (`cgroup2fs`) with
`cpu cpuset io memory pids` controllers, unprivileged user namespaces enabled.

## License

Copyright 2026 Umar Sabirin. Licensed under the Apache License, Version 2.0 — see
[LICENSE](LICENSE).
