# Phase 2 — cgroup v2

Namespaces control what a process can *see*. They do nothing about what it can *consume*: a
container from phase 1 could exhaust the host's memory and CPU while being perfectly isolated.
Cgroups are the other half.

The driver here is written by hand against `cgroupfs` rather than using a crate. Delegating it away
would delegate away the point.

## v2 is not v1 with new filenames

Under v1 each controller had its own hierarchy and its own mount:
`/sys/fs/cgroup/memory/...`, `/sys/fs/cgroup/cpu/...`. A process could sit in a different place in
each one. That flexibility turned out to be a mistake — controllers that need to cooperate (memory
reclaim and I/O writeback, for instance) could not agree on which cgroup a page belonged to.

v2 has one hierarchy. A process is in exactly one cgroup, and controllers are switched on per
subtree. Two rules follow from that, and both are load-bearing:

**Controllers must be enabled from above.** A cgroup cannot decide to use the memory controller;
its *parent* must list `memory` in `cgroup.subtree_control`. So creating `/mars/<id>` means walking
down from the root, enabling controllers at each level as we go:

```
/sys/fs/cgroup/cgroup.subtree_control       ← must contain memory, pids, cpu … for /mars to use them
/sys/fs/cgroup/mars/cgroup.subtree_control  ← must contain them for /mars/<id> to use them
/sys/fs/cgroup/mars/<id>/                   ← the leaf: processes live here, subtree_control stays empty
```

`mars` reads `cgroup.controllers` at each level and only enables what the kernel says is available,
skipping anything already enabled — writing to a systemd-managed root cgroup when nothing needs
changing is a good way to break a host.

**No internal processes.** A cgroup that has children cannot also contain processes, with the root
cgroup exempted. This is why the layout above has a dedicated leaf. Putting the container's process
in `/mars` instead of `/mars/<id>` would make it impossible to ever create a sibling container.

On this host the root cgroup already delegates what we need, which is worth knowing before
debugging a delegation error that does not exist:

```sh
$ cat /sys/fs/cgroup/cgroup.controllers      # what the kernel supports here
cpuset cpu io memory hugetlb pids rdma misc
$ cat /sys/fs/cgroup/cgroup.subtree_control  # what children may actually use
cpuset cpu io memory pids
```

## An ordering constraint that is easy to get wrong

The cgroup namespace makes a container see its own cgroup as `/`. Without it, a process inside the
container reads `/proc/self/cgroup` and learns its full path on the host — an information leak, and
a source of confusion for anything that tries to find its own limits.

The catch: `unshare(CLONE_NEWCGROUP)` freezes the namespace root at *whatever cgroup the process is
in at that moment*. Phase 1 unshared every namespace at once, in the intermediate, before the
container had been placed anywhere. The result would be a container whose cgroup root is the
runtime's cgroup — wrong tree, wrong view, and `/sys/fs/cgroup` inside the container showing the
host's layout.

So the cgroup namespace has to be unshared separately, after placement:

```
intermediate:  unshare(flags & ~CLONE_NEWCGROUP)      ← everything except the cgroup namespace
               fork() ─────────► init
runtime:       write init pid to /mars/<id>/cgroup.procs
               ─── CgroupApplied ──►
init:                                unshare(CLONE_NEWCGROUP)   ← now the root is /mars/<id>
                                     mount /sys/fs/cgroup, pivot_root, …
```

This is what the `& ~CLONE_NEWCGROUP` in runc's `nsexec.c` is for. Verified:

```
  ok   container sees its own cgroup as the root      # /proc/self/cgroup reads "0::/"
```

## OCI values are v1 values, and three of them need translating

The OCI spec's resource fields were designed around cgroup v1. A v2 driver cannot copy them
straight through.

**`memory.swap` changed meaning.** v1's `memory.memsw.limit_in_bytes` was memory *plus* swap. v2's
`memory.swap.max` is swap *only*. Passing the spec value through unchanged gives a container far
more swap than asked for:

```rust
(s, Some(l)) if l > 0 && s > l => (s - l).to_string(),
```

**`cpu.shares` became `cpu.weight`, on a different scale.** v1 shares run 2..262144 with a default
of 1024; v2 weights run 1..10000 with a default of 100. The mapping is linear, and `1024 → 39` is
the number to recognise in a `cpu.weight` file:

```rust
1 + ((shares - 2) * 9_999) / 262_142
```

**`blkio.weight` became `io.weight`**, 10..1000 mapped onto 1..10000, same shape.

Getting these wrong produces no error — just a container with quietly wrong limits, which is worse.
Each conversion has a unit test pinning the boundary values.

## `cpu.max` is a quota, not a share

Two different mechanisms are easy to confuse because both are "CPU limits":

- `cpu.weight` — *relative*. Only matters under contention. A container with a low weight still gets
  the whole CPU when nothing else wants it.
- `cpu.max` — *absolute*. `"10000 100000"` means 10ms of CPU per 100ms period. When the quota is
  spent, every task in the cgroup is stopped until the next period begins, **even if the machine is
  idle**.

The second one is behind most "CPU throttling at low utilisation" reports. Average utilisation over
a minute looks like 10%, because 10% is exactly the ceiling — meanwhile the application is stalled
for 90ms out of every 100ms. `cpu.stat` is where the truth is:

```
nr_periods    how many periods elapsed
nr_throttled  how many of them ended with the cgroup stopped
throttled_usec  total time spent stopped
```

`mars` reads it when the container exits and says so, rather than leaving it to be discovered later.

## exit 137, and why the runtime should explain it

A container that exceeds `memory.max` is killed by the kernel OOM killer. It shows up as exit
`137 = 128 + 9` — killed by signal 9 — which is indistinguishable from any other SIGKILL. `docker
inspect` has an `OOMKilled` flag for exactly this reason: the exit code alone cannot tell you.

The evidence lives in `memory.events`, and only while the cgroup still exists:

```
low 0
high 0
max 45              ← the cgroup hit its limit 45 times, and reclaim handled it
oom 1               ← once, reclaim could not free anything
oom_kill 1          ← so the kernel killed a process
oom_group_kill 0    ← it killed one victim, not the whole cgroup
```

`mars` reads it after the container exits and before it removes the cgroup:

```
WARN container was OOM killed: a process exceeded memory.max
     exit_code=137 oom_kill=1 max_events=35
```

That `max=35` is the interesting number. The container was hitting the limit repeatedly and
surviving — the OOM kill was the end of a long fight, not a sudden event. A memory limit 20% higher
might have avoided it entirely. Read from the `137` alone, none of that is visible.

Note that `oom` and `oom_kill` differ. `oom` counts the times reclaim failed; `oom_kill` counts
processes actually killed. A cgroup can register `oom` events without anything dying.

And the reason to read this file rather than trust the exit code: **an OOM kill does not always
produce a 137.** The kernel picks its victim by badness score, which is usually the process that was
allocating — often a child rather than PID 1. Kill the child, and PID 1 carries on and exits
cleanly:

```
WARN container was OOM killed: a process exceeded memory.max
     exit_code=0 oom_kill=1 max_events=45
```

A container that reports success while having lost a process to the OOM killer is invisible to
anything watching exit codes, which is everything. `memory.events` is the only place it shows up.
Written up in [failure-modes.md](failure-modes.md#the-variant-that-reports-success).

## One bug found by running it

Mounting a fresh `tmpfs` at `/dev` gives the container an **empty** `/dev`. The OCI spec's default
`linux.devices` is an empty array, and the runtime is expected to create a standard set anyway —
something no document states outright. Symptom:

```
/bin/sh: can't open '/dev/null': No such file or directory
```

Almost every real workload writes to `/dev/null`. `mars` now creates `null`, `zero`, `full`, `tty`,
`random`, and `urandom` with `mknod`, plus the `/dev/fd`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr`
symlinks into `/proc/self/fd`, before `pivot_root` — and after it, since `mknod` needs `CAP_MKNOD`,
which is exactly why rootless containers bind-mount these from the host instead. That comes in
phase 5.

## Verified behaviour

```
device nodes in a fresh /dev tmpfs
  ok   /dev/null is writable
  ok   /dev/zero reads
  ok   /dev/stdout is a symlink into /proc/self/fd
cgroup placement and cleanup
  ok   cgroup created at /sys/fs/cgroup/mars/it-cg
  ok   init pid is in cgroup.procs
  ok   memory controller is delegated here
  ok   cgroup tree removed when the container exits
cgroup namespace
  ok   container sees its own cgroup as the root
memory.max and OOM kill
  ok   OOM kill is reported as 128+9
  ok   runtime names memory.max as the cause, read from memory.events
cpu.max quota causes throttling
  ok   runtime reports throttling, read from cpu.stat
pids.max blocks fork
  ok   fork rejected with EAGAIN once pids.max is reached
zombie reaping is the application's job, not the runtime's
  ok   PID 1 is the application after execve, so orphans stay unreaped (1 zombie)

27 passed, 0 failed
```

## Not yet done

Limits are written once, at creation. `mars update` does not exist yet, so a running container's
limits cannot be changed.

`pause`/`resume` would be `cgroup.freeze`, which is a single file write — but it needs the state
tracking from phase 4 to be useful, since there is nothing to pause once `run` has returned.

The rootfs is still whatever directory the bundle points at, used directly. OverlayFS is phase 3.
