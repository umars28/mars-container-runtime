# Phase 4 — the full lifecycle

Phases 1–3 built one command: `run`, which set a container up and supervised it in a single process.
That is not how anything drives a runtime. Docker calls `create`, then `start`, then `state` on a
timer, then `kill`, then `delete` — five separate processes, minutes apart, with the container alive
in between. This phase is that split, plus `exec`, the console socket, and the five lifecycle hooks.

## `create` returns, and the container keeps waiting

The whole difficulty of the split is in one sentence: **`mars create` has to exit while the container
init stays alive, fully set up, not yet running the user's program.**

Init cannot wait on a pipe or socket held by `create`, because those close when `create` exits. It
needs something a *later, unrelated* process can reach. That is a named pipe:

```
mars create                                   mars start (minutes later)
  mkfifo /run/mars/<id>/exec.fifo
  open(fifo, O_PATH)   ← does not count as an opener
  fork ──► init
             … namespaces, rootfs, pivot_root, caps …
           ◄── "ready"
  write state.json
  exit                                        open(fifo, O_RDONLY)  ← unblocks init
             open("/proc/self/fd/N", O_WRONLY)
             write("0")  ─────────────────────► read() == "0"
             execve(user process)              unlink(fifo)
```

Three details make it work.

**`O_PATH` for the parent's handle.** A fifo's `open(O_WRONLY)` blocks until a reader arrives — that
blocking *is* the parking mechanism. If `create` opened the fifo normally it would either block
itself or count as an opener and release init early. `O_PATH` opens the file without opening it for
I/O: it is a handle to the filesystem object, nothing more.

**Init re-opens through `/proc/self/fd/N`.** By the time init parks, it has already called
`pivot_root`. `/run/mars/<id>/exec.fifo` does not exist in the container's filesystem any more. But
the inherited fd does, and `/proc/self/fd/N` is a kernel-resolved magic symlink to the object behind
it — reopening through it works from inside a different mount namespace. This is why the container
must have `/proc` mounted; `runc` has the same requirement for the same reason.

**The fifo is mode 0622, not 0600.** I first created it mode 0000, reasoning that only the runtime
should ever touch it. Then capabilities landed and every container broke: by the time init parks it
has dropped `CAP_DAC_OVERRIDE`, and a process without that capability cannot open a mode-0000 file
even as uid 0. The directory (`/run/mars/<id>`, root-owned) is what provides the protection; the fifo
itself has to be openable by whatever user the container runs as. `runc` uses 0622 for exactly this.

Asking for 0622 is not the same as getting it. `mkfifo` masks its mode argument with the umask, the
same trap as `mknod` [below](#mknod-masks-its-mode-with-the-umask) — so with the usual 022 the fifo
came out 0600, and containers running as a non-root user failed at the one place that gives no useful
context:

```
mars: container init failed: reopen the exec fifo through /proc/self/fd/3; this needs /proc
mounted inside the container: EACCES: Permission denied
```

The error names `/proc`, which was mounted and fine. The problem was two `w` bits, three functions
away. Both `mkfifo` and `mknod` now get an explicit `chmod` afterwards.

The read on the other side is not just a wakeup, it is a health check. If init dies between `create`
and `start`, the fifo's write end closes and `start`'s read returns **0 bytes** instead of `"0"`:

```
the container init closed the exec fifo without writing; it died between create and start
```

## Status is derived, never stored

`state.json` holds the pid, the bundle path, the cgroup path, the creation time — and **not the
status**. Status is computed on every query:

```
process gone?              → stopped
cgroup.events says frozen? → paused
exec.fifo still present?   → created
otherwise                  → running
```

Storing a status invites the classic bug: a file that says `running` for a container that died an
hour ago. Nothing writes to `state.json` when a container crashes, so anything derived from a stored
field is a guess. `created` versus `running` falls out of whether the fifo has been consumed, which
is the same fact `start` acts on.

**"Process gone" is not `kill(pid, 0)`.** Pids are recycled. A container that died an hour ago may
have had its pid reused by an unrelated process, and `kill(pid, 0)` would happily report it alive —
so `mars state` would say `running`, and `mars kill` would signal a stranger. So `state.json` also
records field 22 of `/proc/<pid>/stat`, the process start time in clock ticks since boot, and
liveness means *the pid exists **and** its start time matches*.

Parsing that field has its own trap. Field 2 is the executable name in parentheses, and it can
contain both spaces and parentheses:

```
77 (my (odd) name) S 1 77 77 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 555 …
                                                                    ^ starttime
```

Splitting on whitespace from the left gives the wrong field. The parser has to find the **last**
`)` and count from there.

## `exec`, and why this project is in Rust

`mars exec` opens `/proc/<init>/ns/*`, calls `setns(2)` for each, then forks. The fork is required
because `setns` into a pid namespace does not move the caller — same rule that forces the three-level
chain in [phase 1](01-isolation.md).

The join order is fixed and load-bearing:

```
user → ipc → uts → net → pid → cgroup → mnt
```

**Mount last.** Once we are in the container's mount namespace, `/proc/<init>/ns/...` resolves inside
the container, and the fds we have not opened yet are unreachable. So every namespace fd is opened
before the first `setns`, and the mount namespace is entered last.

And the reason this file is short instead of being a C shim: **`setns(2)` refuses to move a
multi-threaded process into a new mount or user namespace.** Go's runtime is already multi-threaded
when `main` starts, which is why `runc` cannot do this in Go and ships `libcontainer/nsenter` — a C
constructor that runs before the Go runtime initialises. Rust stays single-threaded until we fork, so
`setns` is just a function call. `mars` asserts the property rather than assuming it:

```
the runtime has 5 threads, but setns(2) refuses to move a multi-threaded process into a new
mount or user namespace; nothing in mars may start a thread before exec
```

That check is also why the OpenTelemetry work in phase 6 has to keep its exporter thread out of any
process that will call `setns`.

### Two bugs that only exist because of the namespace switch

Both were found by running it, both had the same shape, and neither produced an error.

**The pid file landed inside the container.** `mars exec -d --pid-file /tmp/x.pid` exited 0 and wrote
nothing to `/tmp/x.pid`. The write happens after the fork, which is after `setns(CLONE_NEWNS)` — so
`/tmp/x.pid` was created in the *container's* `/tmp`:

```
$ mars exec -d --pid-file /tmp/probe.pid pf -- /bin/sleep 5
$ cat /tmp/probe.pid
cat: /tmp/probe.pid: No such file or directory
$ mars exec pf -- /bin/cat /tmp/probe.pid
12074
```

**The cgroup path stopped resolving.** The exec'd process must join the container's cgroup, or it
escapes every limit the container has. But `/sys/fs/cgroup/mars/<id>/cgroup.procs` is a host path,
and after entering the cgroup and mount namespaces it is gone.

The fix for both is the same, and it is the general lesson: **an open file descriptor survives a
namespace change; a path does not.** Open `cgroup.procs` and the pid file *before* the first `setns`,
write to the fds afterwards.

That also fixed a correctness problem in my first attempt, which put the runtime's *own* pid into the
container's cgroup before forking and let the child inherit it. It works, but it means `mars exec`
itself runs under the container's `memory.max` and can be OOM-killed while setting up. Now the parent
stays outside and writes only the child's pid into the fd, with a socketpair handshake so the child
does not `execve` before it has been placed.

## Hooks land in three different worlds

The spec puts the five hooks in specific places, and the differences are the entire point of having
five. Running the same script from each one shows it:

```
createRuntime   uts=uts:[4026531838]  alpine=no    ← runtime namespace, host rootfs
createContainer uts=uts:[4026532411]  alpine=no    ← container namespaces, host rootfs
startContainer  (wrote release=3.20.10)            ← container namespaces, container rootfs
poststart       uts=uts:[4026531838]  status=running
poststop        uts=uts:[4026531838]  status=stopped
```

`createRuntime` sees the host's UTS namespace. `createContainer` sees a *different* one — it runs
inside the container's namespaces — but `/etc/alpine-release` is still absent, because the spec
requires it to run **before `pivot_root`**. `startContainer` runs after, so the container's rootfs is
in place. That is why a hook that needs to write into the image goes in `startContainer` and a hook
that needs to configure a namespace from outside goes in `createRuntime`.

Failure handling differs too, and the spec is explicit: `createRuntime`, `createContainer` and
`startContainer` failures MUST abort the operation; `poststart` and `poststop` failures MUST only be
logged. So:

```
WARN poststop hook 0 (/bin/false) failed: exited with exit status: 1; the spec says poststop
     hook failures must not abort the operation
```

Hooks receive the container state as JSON on stdin, and honour their `timeout`, which is enforced by
polling `try_wait` and killing the process — otherwise one hanging hook hangs `create` forever.

## The console socket allocates its pty inside the container

`process.terminal: true` means the runtime must create a pty and pass the **master** end to whoever
called it, over a unix socket, as an `SCM_RIGHTS` message. The container gets the **slave** as its
stdin, stdout and stderr, after `setsid()` and `ioctl(TIOCSCTTY)`.

My first implementation created the pty in the runtime, before the fork. Everything worked except one
thing:

```
$ tty
not a tty
$ test -t 1 && echo yes
yes
```

Both are true at once, which is a good sign that the fd is fine and the *name* is not. `test -t 1`
asks the kernel whether the fd is a terminal. `tty` calls `ttyname(3)`, which resolves
`/proc/self/fd/0` and then stats the result — and the answer, `/dev/pts/0`, referred to the **host's**
devpts. The container mounts its own instance at `/dev/pts` (the default spec passes `newinstance`),
which starts empty:

```
$ ls /dev/pts/
ptmx
```

Bind-mounting the slave onto `/dev/console` makes `/dev/console` work, but does not give the pty a
name under `/dev/pts`, so `ttyname` still fails. Docker's containers do not have this problem because
Docker's own spec omits `newinstance`, sharing the host's devpts — but that is Docker's choice, not
something the runtime can rely on.

`runc`, on the same bundle, prints `/dev/pts/0`. The difference is *where* the pty is allocated:
`runc` passes the console *socket* fd into the init process and calls `openpty` **after
`pivot_root`**, so the slave comes from the container's own devpts and has a name there. `mars` now
does the same, and the two runtimes produce identical output on the identical bundle:

```
########## runc            ########## mars
  /dev/pts/0                 /dev/pts/0
  stdout-is-a-tty            stdout-is-a-tty
  0     ptmx                 0     ptmx
```

## Capabilities, and an ordering the kernel enforces

Capabilities were meant to be phase 5. They came forward because nearly every test in the validation
suite runs `runtimetest` inside the container, and `runtimetest` checks all five capability sets — so
109 of the 111 failures in a typical test were one missing feature.

Getting the order right took three attempts, because the kernel enforces invariants that produce
`EPERM` with no explanation of which one was violated.

**Effective must stay a subset of permitted.** Setting permitted to a small set while effective is
still the full root set fails. The `caps` crate writes one set per call, so the order has to be
effective (shrink) → permitted (shrink) → inheritable. Writing permitted first gives:

```
set the Permitted capability set: caps error: capset failure: Operation not permitted
```

**Bounding is dropped before `setuid`, the rest after.** Dropping from the bounding set needs
`CAP_SETPCAP` in the effective set, so it has to happen while still privileged. But a transition from
uid 0 to a non-zero uid clears every capability set, so the other four have to be applied *after*
`setuid` — otherwise the work is thrown away.

**And that transition needs `PR_SET_KEEPCAPS`.** Without it, `setuid` away from root drops the
permitted set to empty and re-adding to permitted is impossible (permitted can only shrink). So:
`prctl(PR_SET_KEEPCAPS, 1)` → `setgroups` → `setgid` → `setuid` → `prctl(PR_SET_KEEPCAPS, 0)` → apply
the four sets.

One more trap, this one mine. I built kernel capability names by uppercasing the enum variant:
`AuditWrite` → `CAP_AUDITWRITE`. The kernel spells it `CAP_AUDIT_WRITE`. Rather than write a
CamelCase-to-SNAKE_CASE converter and hope, `mars` asks `serde` for the name, which is the same
mapping the spec's own JSON uses:

```rust
match serde_json::to_value(name)? {
    serde_json::Value::String(text) => Ok(text),
```

## `mknod` masks its mode with the umask

The validation suite found this one:

```
not ok 292 - "/dev/test1" (linux.devices[0]) has the expected permissions
```

`mknod(path, S_IFCHR, 0o666, dev)` does not produce a 0666 device node. Like `open` and `mkdir`, the
mode argument is masked with the process umask, so with the usual 022 the node comes out 0644. Every
device `mars` created had been subtly wrong since phase 2 — and the phase 2 test only checked that
`/dev/null` was *writable*, which it was, for the owner.

The fix is an explicit `chmod` after the `mknod`. `linux.devices[].uid` and `gid` were being ignored
entirely; they are applied now too.

## Validation results

`opencontainers/runtime-tools` 0.9.0, 58 test binaries, on Ubuntu 24.04 / kernel 6.8 / aarch64. The
suite is only meaningful next to a reference, so every run below is paired with `runc` 1.5.1 on the
same host, same rootfs, same test binaries:

| | passed | failed | inconclusive |
|---|---|---|---|
| mars 0.1.0 | 22 | 21 | 15 |
| runc 1.5.1 | 22 | 20 | 16 |

"Inconclusive" means the test binary produced no TAP assertions at all — it failed to run in this
environment. 15 of those are the same for both runtimes, which is what the plan anticipated when it
called this suite "relatively stagnant". `pidfile` is a clear example: it starts `true`, then waits
for the container to report `running`, then kills it. `true` has already exited, so the wait times out
and the kill correctly fails on a stopped container. Both runtimes fail it; neither is wrong.

Where the two runtimes differ:

| test | mars | runc | why |
|---|---|---|---|
| `linux_devices` | pass | inconclusive | the `mknod` umask bug above, now fixed |
| `delete`, `linux_seccomp` | pass | fail | |
| `linux_mount_label`, `linux_process_apparmor_profile` | pass | inconclusive | **mars ignores these fields rather than implementing them** — not a win |
| `linux_masked_paths`, `linux_readonly_paths`, `root_readonly_true` | fail | pass | phase 5 |
| `linux_ns_nopath`, `linux_uid_mappings` | fail | pass | user namespaces, phase 5 |

Every remaining mars-only failure is a phase 5 item, and
[phase 5](05-hardening.md#validation-results) closes all of them. Two rows deserve a caveat rather
than credit:
`mars` accepts `mountLabel` and `apparmorProfile` and does nothing with them, while `runc` tries and
fails on a host with no SELinux or AppArmor policy loaded. Passing by ignoring a field is not passing.

One test went the other way on purpose. `misc_props` starts a container whose `process.args` names a
binary that is not in the bundle, and expects `create` to succeed. It used to pass, because
`resolve_executable` returned any path containing a `/` unchecked and the failure surfaced later at
`execve` — where no caller is watching. `mars` now stats the program during `create`:

```
$ mars create nobin
mars: container init failed: executable "/nonexistent-binary" not found in the container PATH
```

`runc` does the same (`stat /nonexistent-binary: no such file or directory`) and fails the same test.
Reporting a missing program at `create` is worth one test.

The check only applies when the runtime owns the rootfs. If the mount namespace is *joined* rather
than created, `mars` never calls `pivot_root`, so the filesystem the program will be looked up in is
not one it set up — and `linux_ns_path` exercises exactly that, joining a namespace made by
`unshare --mount` on the host. There the lookup is left to `execve`.

## Not yet done

Seccomp, read-only paths and user namespaces land in [phase 5](05-hardening.md);
`--preserve-fds` and `--no-pivot` are still rejected outright. `linux.devices` cgroup rules (the
device allowlist) need eBPF on cgroup v2 and are not implemented, which is why the
`linux_cgroups_devices` test fails — `runc` fails it here too.

`update` writes new limits but does not re-read `config.json`, so a limit set to "unlimited" and back
goes through `mars update --memory -1`, not by editing the bundle.

Everything in `docs/failure-modes.md` about signals still applies: `mars kill` sends the signal, and
whether the container reacts is up to whatever the image put at PID 1.
