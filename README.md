# mars

An OCI-compliant container runtime written from scratch in Rust — the layer that Docker and
Kubernetes sit on top of, built to understand it rather than to replace it.

`mars` implements the [OCI runtime-spec](https://github.com/opencontainers/runtime-spec): it takes a
filesystem bundle and a `config.json` and uses Linux namespaces, cgroup v2, OverlayFS, capabilities
and seccomp to turn it into an isolated process.

```sh
$ docker run --rm --runtime=mars --memory=64m alpine:3.20 cat /sys/fs/cgroup/memory.max
67108864
```

That is Docker, using `mars` instead of `runc`, enforcing a memory limit through a cgroup that `mars`
created and wrote itself.

| | |
|---|---|
| OCI validation suite | **26 passed**, next to `runc` 1.5.1's 22 on the same host — [details](docs/05-hardening.md#validation-results) |
| Integration suite | 128 assertions, all reading kernel state rather than the runtime's own claims |
| Docker drop-in | `run`, `run -it`, `exec`, `stop`, `--memory` — [details](docs/06-docker-and-tracing.md) |
| Startup tracing | OTLP spans per phase, accepted by Tempo |
| Not implemented | rootless without privilege, CNI networking, image pulling, cgroup v1, systemd cgroup driver |

## Why build this when runc exists?

This is not a runc competitor and is not meant for production. It exists because container failures in
production happen in the layer that Docker hides, and reading about that layer does not build a model
of it. [`docs/failure-modes.md`](docs/failure-modes.md) reproduces nine of them with evidence read
from the kernel:

- a pod is `OOMKilled` with exit `137` — and `memory.events` says how close it had been, for how long
- a container ignores `SIGTERM` for the full grace period — because PID 1 gets no default handlers
- zombies pile up until `fork()` fails — because PID 1 was never written to be an init
- CPU throttles at 10% utilisation — because `cpu.max` is a quota, not a share
- a fix applied with `exec` survives a restart but not a recreate — because it lived in the OverlayFS
  upper layer, which *is* the container
- a rootless bind mount is unwritable at mode `0777` — because the uid has no mapping and reads as
  `nobody`
- `EPERM` mounting `/sys/fs/cgroup` — because the bundle asked for cgroup **v1** on a v2 host

Three findings surprised me enough to state outright:

**An OOM kill does not reliably produce exit 137.** The kernel picks its victim by badness score,
usually the allocating process rather than PID 1. Kill a child and PID 1 carries on: the container
exits `0` having lost a process, with `oom_kill=1` in `memory.events` and nothing else to show for it.
Kubernetes only marks a pod `OOMKilled` when PID 1 dies of signal 9, so this case restarts nothing and
alerts nobody.

**A mount option string over 4096 bytes is truncated, not rejected.** Enough OverlayFS layers and the
kernel silently cuts the `lowerdir=` list mid-path, then reports `ENOENT` against the mount *source* —
an error naming neither the truncation nor the layer count. This is what the short symlinks in
`/var/lib/docker/overlay2/l/` are for.

**Writing one pid to `cgroup.procs` is the most expensive step in starting a container** — 55% to 79%
of cold start, measured. Creating the cgroup and writing every limit takes 279µs; moving one process
into it takes 7–15ms, because the first migration into a fresh cgroup pays for per-cgroup controller
setup and an RCU grace period across every CPU. Every container gets a fresh cgroup, so nothing is
ever amortised.

```
trace 10a2ae966c82f09af3c6d2282991b79c  25 spans  11892us total
     458us    279us  cgroup                      create it, write every limit
    1012us    895us  intermediate.unshare.net    a whole network stack
    2358us   7065us  cgroup.attach               write one pid to one file
    9438us    492us  init.rootfs.mount
    9931us    232us  init.pivot_root
```

## Design notes

**The three-level fork chain is forced by the kernel, not a style choice.**

```
mars create
  │  socketpair(AF_UNIX)
  ├─ fork() ─────────► [intermediate]
  │                      unshare(NEWUSER|NEWNS|NEWPID|NEWUTS|NEWIPC|NEWNET|NEWTIME)
  │  ◄── "map me" ────
  │  write /proc/<pid>/setgroups, uid_map, gid_map
  │  ─── "mapped" ──►   setresuid(0) — a new user namespace leaves you unmapped
  │                      fork() ──────────► [container init, PID 1]
  │                        exit                mounts, pivot_root, devices
  │  write pid to cgroup.procs                 caps, seccomp, no_new_privs
  │  ◄── "ready" ─────────────────────────────  block on exec.fifo
mars create returns; the container stays alive, waiting
mars start ──── opens the fifo ─────────────────► execve(user process)
```

1. `unshare(CLONE_NEWPID)` does **not** move the caller into the new PID namespace. The next `fork()`
   is what becomes PID 1. Same for `CLONE_NEWTIME`.
2. `uid_map` must be written from **outside** the user namespace by a privileged process — a process
   cannot map itself, so the two ends need two-way synchronisation.
3. `create` has to return while the container stays alive, so the wait has to be on something a later,
   unrelated process can reach. That is a fifo, opened `O_PATH` by the parent so it does not count as
   an opener, and reopened by the init through `/proc/self/fd/N` because the path is gone after
   `pivot_root`.

**Rust instead of Go, deliberately.** `setns(2)` refuses to move a multi-threaded process into a new
mount or user namespace. Go is already multi-threaded when `main` starts, which is why `runc` ships a C
constructor (`libcontainer/nsenter`) that runs before the Go runtime initialises. Rust stays
single-threaded until we fork, so `mars exec` calls `setns` directly — and asserts the property by
counting `/proc/self/task` rather than assuming it.

**The telemetry exporter is hand-written for the same reason.** The OpenTelemetry SDK runs its exporter
on a background thread. A process that forks must not have one — only async-signal-safe work is legal
in the child — and a process that calls `setns` must not either. So the exporter is ~120 lines that
build OTLP/HTTP JSON and write one POST: no threads, nothing running at `fork()` time.

**The cgroup driver is hand-written** against `cgroupfs` rather than using a crate. Delegating it would
delegate away the main thing this project is for.

**The overlay rootfs is a documented extension, not a spec feature.** The runtime-spec has no field for
image layers — that is the image-spec's job, done by containerd or Docker before the runtime is called.
`mars` reads three `dev.mars.overlay.*` annotations instead, so the `config.json` stays valid for any
other runtime, which will ignore them.

## Scope

Implemented: mount/pid/uts/ipc/user/cgroup/net/time namespaces, `pivot_root`, standard mounts and
device nodes, OverlayFS rootfs assembly, cgroup v2 (`memory`, `cpu`, `pids`, `cpuset`, `io`) written
directly to `cgroupfs`, the full lifecycle (`create`, `start`, `state`, `kill`, `delete`, `exec`,
`list`, `ps`, `pause`, `resume`, `events`, `update`, `spec`, `features`), console socket over
`SCM_RIGHTS`, all five lifecycle hooks, capabilities, seccomp, `no_new_privs`, read-only rootfs,
`maskedPaths`/`readonlyPaths`, sysctls, rlimits, `oomScoreAdj`, user namespaces with a `newuidmap`
fallback, Docker drop-in, OTLP trace export.

Deliberately out of scope: image pulling from registries (containerd's job), CNI networking,
checkpoint/restore, CRI, cgroup v1, the systemd cgroup driver, SELinux and AppArmor labels (parsed and
ignored), `SCMP_ACT_NOTIFY`.

Not finished: rootless without any privilege. The user namespace machinery works and is tested, but
`mars` still expects to be started with privilege; a fully rootless run also needs a delegated cgroup
under `user.slice`, `fuse-overlayfs` or `userxattr` for whiteouts, and `slirp4netns` for networking.

## Roadmap

| Phase | Scope | |
|---|---|---|
| 0 | Dev VM, toolchain, CLI surface, `spec` generation | done |
| 1 | Namespaces, `pivot_root`, PID 1 duties | [writeup](docs/01-isolation.md) |
| 2 | cgroup v2 driver, OOMKilled reproduction | [writeup](docs/02-cgroups.md) |
| 3 | OverlayFS rootfs, `config.json` validation | [writeup](docs/03-overlayfs.md) |
| 4 | Full lifecycle, `exec`, hooks, console socket | [writeup](docs/04-lifecycle.md) |
| 5 | Capabilities, seccomp, read-only paths, user namespaces | [writeup](docs/05-hardening.md) |
| 6 | Docker drop-in, OTLP tracing of the startup path | [writeup](docs/06-docker-and-tracing.md) |
| 7 | Writeups, failure modes, CI | done |

## Running it on your own machine

Check the host first — most of what can go wrong is the host, not the build:

```sh
./scripts/preflight.sh
```

The one that stops people: **a VPS that is itself a container.** OpenVZ, LXC and most budget plans
share the provider's kernel, which blocks `pivot_root` and cgroup delegation. `systemd-detect-virt -c`
naming anything other than `none` means `mars` cannot run there and no amount of `sudo` changes it —
you need KVM, Xen, or bare metal. The other common blocker is a hybrid cgroup hierarchy; `mars` has no
v1 driver, so `/sys/fs/cgroup` must be `cgroup2fs`.

On a Debian or Ubuntu host that passes preflight:

```sh
sudo apt-get install -y build-essential pkg-config libseccomp-dev jq attr uidmap
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # the distro rustc is often too old
cargo build --release
sudo install -m 0755 target/release/mars /usr/local/bin/mars
```

## Development

Linux-only: it needs namespaces, cgroups, and `libseccomp`. It does not build on macOS. A
[Lima](https://lima-vm.io) VM definition is included so the environment is reproducible.

```sh
limactl start --name=mars-dev ./lima/mars-dev.yaml
limactl shell mars-dev

cargo build
cargo test --lib
sudo -E ./tests/run-integration.sh
```

The integration suite asserts against kernel state — `/proc`, `/proc/mounts`, `/sys/fs/cgroup`,
`ip -o link`, wait statuses — rather than trusting what the runtime reports about itself.

Driving it by hand:

```sh
mars spec --bundle /tmp/demo
./scripts/make-rootfs.sh /tmp/demo/rootfs
cd /tmp/demo && sudo mars run demo
```

A layered rootfs, with the `.wh.` markers from the image tarballs converted into real OverlayFS
whiteouts and `process`/`env`/`cwd` taken from the image config:

```sh
sudo -E ./scripts/oci-bundle.sh -i alpine:3.20 /tmp/layered
cd /tmp/layered && sudo mars run demo
find /tmp/layered/diff -mindepth 1        # everything the container wrote
```

The OCI validation suite, against `mars` or `runc`:

```sh
git clone --depth 1 https://github.com/opencontainers/runtime-tools.git /var/tmp/runtime-tools
cd /var/tmp/runtime-tools && make runtimetest validation-executables
tar czf rootfs-$(go env GOARCH).tar.gz -C /tmp/demo/rootfs .

sudo -E ./scripts/run-validation.sh
sudo -E RUNTIME=runc ./scripts/run-validation.sh
```

As a Docker runtime:

```sh
sudo ./scripts/install-docker-runtime.sh          # TRACE=1 also logs how Docker calls it
docker run --rm -it --runtime=mars alpine:3.20 sh
```

Startup traces, without needing a collector:

```sh
./scripts/otlp-echo.py 4318 &
MARS_OTLP_ENDPOINT=127.0.0.1:4318 sudo -E mars run demo
```

Verified environment: Ubuntu 24.04, kernel 6.8, aarch64, pure cgroup v2 (`cgroup2fs`) with
`cpu cpuset io memory pids` delegated, unprivileged user namespaces enabled, and `runc` 1.5.1 plus
Docker 29.7.2 alongside for comparison.

## License

Copyright 2026 Umar Sabirin. Licensed under the Apache License, Version 2.0 — see
[LICENSE](LICENSE).
