# Phase 1 — Isolation core

What a container actually is, at the syscall level: a process that was started with a different view
of the system. No new kind of object exists in the kernel. This phase builds that view.

## The fork chain, and why it has three levels

```
mars run                              ← the runtime process
  │  prctl(PR_SET_CHILD_SUBREAPER, 1)
  │  sigprocmask(SIG_BLOCK, {TERM,INT,QUIT,HUP,USR1,USR2,WINCH,CHLD})
  │  socketpair(AF_UNIX, SOCK_SEQPACKET)
  │
  ├─ fork() ───────────► intermediate
  │                        unshare(NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS|NEWCGROUP)
  │                        │
  │                        ├─ fork() ───────────► container init, PID 1
  │  ◄── InitPid(pid) ─────┤                        mount /, pivot_root
  │      exit(0)  ─────────┘                        sethostname
  │                                                 resolve argv[0] in container PATH
  │  ◄── InitReady ───────────────────────────────  block on the sync socket
  │  ─── Start ──────────────────────────────────►  sigprocmask(SIG_SETMASK, {})
  │                                                 execve(argv)
  │  sigwait() loop: forward signals, reap children
```

Two kernel rules force this shape.

**`unshare(CLONE_NEWPID)` does not move the caller into the new PID namespace.** It only sets
`pid_ns_for_children`. If it moved the caller, the caller's own PID would change underneath it,
which would break every pid the process was already holding. So a `fork()` is mandatory, and *that
child* is PID 1.

**A process cannot write its own `uid_map`.** Setting up a user namespace requires a privileged
process *outside* the namespace to write `/proc/<pid>/uid_map` and `/proc/<pid>/setgroups`. That
means the parent and child must synchronise: child unshares and blocks, parent writes the maps,
parent releases the child. Hence a socket, not just a fork. (The mapping itself lands in phase 5;
the protocol is already in place, and `mars` reports `RequestUserMapping` as unimplemented rather
than silently producing a broken namespace.)

## Reparenting: why `PR_SET_CHILD_SUBREAPER`

The intermediate exits as soon as it has reported the init pid. That orphans the container init —
and an orphan is reparented away from the runtime, normally to host PID 1. A runtime that cannot
`waitpid()` on its own container cannot report its exit code, which would make every later phase
(OOM kills reported as 137, `docker stop` exit codes) impossible.

`runc` solves this in C, using `clone(CLONE_PARENT)` so the init is born as a sibling — a direct
child of the original runtime process.

`mars` uses `prctl(PR_SET_CHILD_SUBREAPER, 1)` instead. A subreaper adopts orphaned descendants
rather than letting them travel to PID 1. One syscall, no C, and the same result. This is the same
mechanism systemd and the containerd shims use.

## Signals: block before you fork

The runtime blocks its forwardable signals *before* forking, then consumes them with `sigwait()`.
Blocking first is not tidiness — it closes a race. If SIGCHLD arrived after the fork but before the
wait loop started, a handler-based design would drop it and the runtime would hang forever waiting
for a child that had already exited. A blocked signal stays pending and is still there when
`sigwait()` finally asks.

The cost of that choice is a trap: **the signal mask survives `execve`.** A container process would
inherit a mask with SIGTERM blocked and would then be unkillable by normal means, for reasons
nothing in the container could explain. So the container init calls
`sigprocmask(SIG_SETMASK, empty)` immediately before `execve`.

## `pivot_root`, not `chroot`

`chroot` moves a process's root directory. The old root is still mounted, still reachable through
any file descriptor opened before the call, and escapable with a well-known dozen-line program.
`pivot_root` moves the *mount* itself, and the old root can then be unmounted — after which there
is nothing left to escape to.

Three details matter, in order:

**1. Make `/` private first.** A new mount namespace starts as a copy of its parent, and by default
mount events propagate back. Without `mount(NULL, "/", NULL, MS_PRIVATE|MS_REC, NULL)`, every mount
the container makes appears on the host, and every unmount tears down host mounts. This one line is
the difference between isolation and vandalism.

**2. The new root must be a mount point.** `pivot_root` requires it. A plain directory is not one,
so the rootfs is bind-mounted onto itself.

**3. `pivot_root(".", ".")`.** Passing the same path twice looks like a mistake and is not. The old
root gets stacked on top of the new root at `.`, so no scratch directory has to be created inside
the container's filesystem — which matters because that filesystem may be read-only. The old root
is then made private and detached through a file descriptor captured beforehand:

```
oldroot = open("/", O_DIRECTORY)
newroot = open(rootfs, O_DIRECTORY)
fchdir(newroot)
pivot_root(".", ".")
fchdir(oldroot)                              ← the fd still resolves, the path no longer would
mount(NULL, ".", NULL, MS_PRIVATE|MS_REC, NULL)
umount2(".", MNT_DETACH)
chdir("/")
```

The `MS_PRIVATE` before the unmount is the subtle one: without it, detaching the old root can
propagate the unmount outward and unmount things on the host.

## Two things the kernel taught us during this phase

Both are written up in [failure-modes.md](failure-modes.md):

- mounting `/sys/fs/cgroup` fails with `EPERM` on a modern host, and the reason is not permissions
- `SIGTERM` sent to a container's PID 1 is silently discarded, and this is correct behaviour

## Verified behaviour

`tests/run-integration.sh`, run as root inside the dev VM. Every assertion reads kernel state —
`/proc`, `/proc/mounts`, `ip -o link`, wait statuses — rather than trusting anything the runtime
prints about itself.

```
pid namespace
  ok   init process is PID 1
  ok   sees only its own processes (3 < 5)
uts namespace
  ok   hostname is the spec hostname
  ok   hostname differs from the host (lima-mars-dev)
network namespace
  ok   only loopback exists
mount namespace and pivot_root
  ok   fewer mounts than the host (8 < 26)
  ok   no host virtiofs mount leaked in
  ok   root is the alpine rootfs (3.20.10)
exit code propagation
  ok   non-zero exit propagates
signal forwarding to a PID 1 that installs a handler
  ok   pid file written by the runtime
  ok   handler ran, its exit code propagates
PID 1 without a handler ignores SIGTERM from an ancestor namespace
  ok   init pid names a live process
  ok   SIGTERM was discarded by the kernel, container still running
  ok   SIGKILL is forcibly delivered, reported as 128+9

14 passed, 0 failed
```

## Not yet done

`create` and `start` are still separate unimplemented commands: splitting them needs an on-disk
FIFO and a state file, because the runtime process that created the container is gone by the time
someone starts it. That is phase 4. This phase implements `run`, where one process sees the whole
lifecycle and the sync socket is enough.

No cgroups yet, so a container can still exhaust the host's memory and CPU — namespaces control
*visibility*, not *consumption*. That is phase 2.
