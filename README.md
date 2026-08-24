# mars

An OCI-compliant container runtime written from scratch in Rust — the layer that Docker and
Kubernetes sit on top of, built to understand it rather than to replace it.

`mars` implements the [OCI runtime-spec](https://github.com/opencontainers/runtime-spec): it takes
a filesystem bundle and a `config.json`, then uses Linux namespaces, cgroup v2, and OverlayFS to
turn it into an isolated process. The goal is to be a drop-in `--runtime` for Docker.

> **Status: early.** Environment and CLI surface are in place. The isolation core, cgroup driver,
> and lifecycle are not implemented yet. See [Roadmap](#roadmap) for exactly what works today.

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

Reading the documentation does not build a mental model for these. Writing the runtime does.

The verifiable outputs are the point: passing the OCI validation suite, running as
`docker run --runtime=mars`, and [`docs/failure-modes.md`](docs/) reproducing each failure above
with evidence read straight from the kernel.

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
multi-threaded by the time `main` runs and namespaces are per-thread. It works around this with a C
constructor (`libcontainer/nsenter`) that runs before the Go runtime starts. Rust stays
single-threaded until we fork, so `unshare`/`setns` can be called directly.

**Telemetry lives only in the parent.** The OTLP exporter runs a background thread, and after
`fork()` only async-signal-safe work is legal in the child. The child reports timings over a pipe;
the parent turns them into spans.

**The cgroup driver is hand-written** against `cgroupfs` rather than using a crate. Delegating that
away would delegate away the main thing this project is for.

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
| 1 | Namespaces, `pivot_root`, PID 1 duties | not started |
| 2 | cgroup v2 driver, OOMKilled reproduction | not started |
| 3 | OverlayFS, OCI bundle parsing | not started |
| 4 | Full lifecycle, passes OCI validation | not started |
| 5 | Capabilities, seccomp, rootless | not started |
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
./target/debug/mars spec --bundle /tmp/demo
```

Verified environment: Ubuntu 24.04, kernel 6.8, aarch64, pure cgroup v2 (`cgroup2fs`) with
`cpu cpuset io memory pids` controllers, unprivileged user namespaces enabled.

## License

Apache-2.0
