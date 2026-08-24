# Failure modes

Production container problems, reproduced deliberately, with the evidence read from the kernel
rather than inferred from a runtime's error message.

Each entry follows the same shape: the symptom as it appears in production, the reproduction, what
the kernel says, and the actual cause.

---

## `docker stop` hangs for ten seconds, then the container dies anyway

**Symptom in production.** `docker stop` or a Kubernetes pod deletion appears to do nothing. After
exactly the grace period the container disappears. Application shutdown hooks never ran, in-flight
requests were dropped, and the logs contain no shutdown message.

**Reproduction.** Two containers, identical except for whether PID 1 installs a signal handler.

```sh
# PID 1 with no handler
jq '.process.args = ["/bin/sleep","30"]' config.json | sponge config.json
mars run --pid-file /tmp/bare.pid demo &
kill -TERM $!                       # ask the runtime to forward SIGTERM
sleep 1
kill -0 "$(cat /tmp/bare.pid)" && echo "still alive"
```

```
still alive
```

```sh
# PID 1 with a handler
jq '.process.args = ["/bin/sh","-c","trap \"exit 42\" TERM; while true; do sleep 0.2; done"]' \
  config.json | sponge config.json
mars run demo &
kill -TERM $!
wait $!; echo "exit=$?"
```

```
exit=42
```

**What the kernel says.** From `man 7 pid_namespaces`:

> a process in an ancestor namespace can — subject to the usual permission checks — send signals to
> the "init" process of a child PID namespace only if the "init" process has established a handler
> for that signal. […] SIGKILL or SIGSTOP are treated exceptionally: these signals are forcibly
> delivered when sent from an ancestor PID namespace.

**Cause.** PID 1 of a namespace does not get default signal actions. The kernel discards any signal
with no installed handler, deliberately, so that a stray `kill` cannot destroy a namespace's init.
The runtime forwarded SIGTERM correctly; the kernel dropped it on the floor.

So the ten second wait is not a timeout being hit — it is `docker stop` sending SIGTERM, the kernel
discarding it, and Docker then falling back to SIGKILL, which *is* forcibly delivered.

**What this means when it is your service.** `ENTRYPOINT ["python", "app.py"]` makes the
interpreter PID 1. If it never registers a SIGTERM handler, it cannot shut down gracefully — no
amount of increasing `terminationGracePeriodSeconds` helps, because nothing is waiting on that
grace period. Either handle the signal in the application, or run a real init as PID 1
(`docker run --init`, or `tini`) so signals reach your process as a normal child with normal
default actions.

**Evidence of correct translation.** `SIGKILL` from an ancestor namespace is delivered, and the
runtime reports it the way a shell does:

```
  ok   SIGKILL is forcibly delivered, reported as 128+9
```

`137 = 128 + 9`. The same arithmetic behind every `OOMKilled` exit code.

---

## Mounting `/sys/fs/cgroup` fails with `EPERM`, and it is not a permission problem

**Symptom in production.** An older image, or an older runtime, or a hand-written OCI bundle fails
to start on a current host. The error is `EPERM` on a mount, while running as root with full
capabilities. Everything about the message points at permissions, and permissions are fine.

**Reproduction.** The default OCI spec asks for the legacy cgroup filesystem:

```json
{ "destination": "/sys/fs/cgroup", "type": "cgroup",
  "options": ["nosuid","noexec","nodev","relatime","ro"] }
```

Run it on Ubuntu 24.04 with kernel 6.8:

```
mars: container init failed: mount cgroup type=cgroup at /sys/fs/cgroup
      flags=MsFlags(MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC | MS_RELATIME)
      data="": EPERM: Operation not permitted
```

**What the kernel says.** The host is running a pure cgroup v2 unified hierarchy:

```sh
$ stat -fc %T /sys/fs/cgroup
cgroup2fs
$ cat /sys/fs/cgroup/cgroup.controllers
cpuset cpu io memory hugetlb pids rdma misc
```

`cgroup2fs`, not `tmpfs` with v1 hierarchies underneath. There is no v1 hierarchy to join, and the
kernel refuses to create one.

**Cause.** `type: "cgroup"` names the *v1* filesystem. On a unified host the correct filesystem is
`cgroup2`, which is a different fstype with different semantics — one hierarchy for all controllers
instead of one mount per controller. The kernel reports this refusal as `EPERM`, which is
misleading: nothing about the request was a permissions question.

**The fix, and what real runtimes do.** Detect the host's hierarchy and translate:

```rust
pub fn unified_cgroup_host() -> bool {
    nix::sys::statfs::statfs("/sys/fs/cgroup")
        .map(|stat| stat.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC)
        .unwrap_or(false)
}

pub fn effective_fstype(requested: &str, unified_host: bool) -> &str {
    if requested == "cgroup" && unified_host { "cgroup2" } else { requested }
}
```

`runc` does the same thing. This is why a bundle that names `cgroup` still works under Docker on a
v2 host: the runtime quietly rewrote the request. A hand-rolled bundle gets no such courtesy.

**Debugging lesson.** The original error was just `EPERM: Operation not permitted` with no
indication of *which* of the seven mounts failed. Adding the mount target, fstype, flags, and data
to the error message turned a guessing game into a one-line diagnosis. For a runtime whose purpose
is troubleshooting, an error that does not name the syscall and its arguments is a bug in its own
right.

---

## `OOMKilled` with exit 137, and no idea how close you were

**Symptom in production.** A pod restarts. `kubectl describe` says `OOMKilled`, exit code 137. The
application logged nothing — no exception, no shutdown. Raising the memory limit fixes it, but by
how much is a guess, and the same pod ran fine for weeks.

**Reproduction.** A 32 MiB limit and a process that allocates without bound:

```sh
jq '.linux.resources.memory.limit = 33554432
  | .process.args = ["/usr/bin/awk","BEGIN{s=\"\";while(1){s = s sprintf(\"%1000000s\",\"\")}}"]' \
  config.json | sponge config.json

mars run oomtest; echo "exit=$?"
```

```
WARN container was OOM killed: a process exceeded memory.max
     exit_code=137 oom_kill=1 max_events=35
exit=137
```

**What the kernel says.** `memory.events`, in the container's cgroup, while it still exists:

```
low 0
high 0
max 45
oom 1
oom_kill 1
oom_group_kill 0
```

`memory.peak` in the same cgroup reads `33554432` — exactly the limit, to the byte. The container
did not overshoot; it was held at the ceiling until the kernel gave up.

**Cause.** `137 = 128 + 9`: killed by signal 9. The application logged nothing because SIGKILL
cannot be caught — there is no handler, no unwinding, no last log line. This is why the exit code
alone can never tell you it was an OOM kill rather than any other `kill -9`; `docker inspect` carries
a separate `OOMKilled` boolean precisely because 137 is ambiguous.

**The number that actually matters is `max`.** It counts how many times the cgroup reached its limit
and *survived*, because reclaim managed to free something. Here: 35 times. The container had been
fighting its limit continuously, and the kill was the end of that fight rather than a sudden spike.

That distinction changes the fix:

- `max` high, `oom_kill` 1 — chronic pressure. The limit is genuinely too low; raise it.
- `max` 0 or 1, `oom_kill` 1 — one allocation blew straight through the ceiling. Raising the limit
  a little will not help. Look for an unbounded allocation.

`oom` and `oom_kill` are also not the same counter. `oom` counts reclaim failures; `oom_kill` counts
processes actually killed. A cgroup can accumulate `oom` events with nothing dying.

**Why this evidence is usually gone.** The cgroup is removed when the container exits, taking
`memory.events` with it. By the time a human looks, only the `137` survives. A runtime that reads it
before cleanup — as above — is the difference between "OOMKilled, again" and "chronic pressure, the
limit is 20% too low".

### The variant that reports success

Change one thing — make PID 1 a shell that outlives the allocation — and the same OOM kill produces
a *zero* exit code:

```sh
jq '.linux.resources.memory.limit = 33554432
  | .process.args = ["/bin/sh","-c","awk \"BEGIN{s=\\\"\\\";for(i=0;i<28;i++){s = s sprintf(\\\"%1000000s\\\",\\\"\\\")}}\"; sleep 3"]' \
  config.json | sponge config.json
```

```
Killed
WARN container was OOM killed: a process exceeded memory.max
     exit_code=0 oom_kill=1 max_events=45
```

`oom_kill=1` — a process really was killed. `exit_code=0` — the container reports success.

The OOM killer picks its victim by badness score, largely proportional to memory used. That is
usually the process doing the allocating, which is often *not* PID 1. Here `awk` was killed, `sh`
carried on to `sleep 3`, and exited cleanly.

Everything downstream reads the exit code. Kubernetes marks a pod `OOMKilled` when PID 1 dies of
signal 9; when a child dies instead, the pod is `Completed` and nothing restarts. This is the
mechanism behind "a worker disappeared and no alert fired" — a Celery worker, a forked request
handler, a build step in a wrapper script.

Which is why `mars` reports OOM kills from `memory.events` rather than inferring them from the exit
code. The exit code cannot see this case at all.

To make the whole container die together, cgroup v2 offers `memory.oom.group`: set it to `1` and the
kernel kills every process in the cgroup as a unit, so the failure is at least visible. The
`oom_group_kill` counter above is how you confirm it fired.

---

## CPU throttling while utilisation looks low

**Symptom in production.** Latency is bad. CPU utilisation graphs sit at 25%. Adding replicas does
not help much, and the node is not busy.

**Reproduction.** A quota of 10% of one core, running a tight loop:

```sh
jq '.linux.resources.cpu.quota = 10000 | .linux.resources.cpu.period = 100000' \
  config.json | sponge config.json
```

```
INFO container was CPU throttled by cpu.max
     nr_periods=37 nr_throttled=37 throttled_usec=3314146
```

**Cause.** `cpu.max` is `"quota period"` — here 10ms of CPU per 100ms window. Once the quota is
spent, every task in the cgroup is **stopped until the next period begins, even on a completely idle
machine.** All 37 periods ended throttled, with 3.3 seconds spent frozen out of 3.7 seconds of wall
clock — the workload was stopped roughly 90% of the time, which is the arithmetic complement of the
10% quota.

Average utilisation reads 10% because 10% is the ceiling, not because the application is idle. The
metric and the symptom are the same number seen from opposite sides.

**The distinction to hold onto.** Two things both get called "CPU limits":

| | file | behaviour |
|---|---|---|
| shares / weight | `cpu.weight` | *relative*, only bites under contention; an idle machine gives you everything |
| quota | `cpu.max` | *absolute*, bites regardless of what else is running |

In Kubernetes, `requests.cpu` becomes `cpu.weight` and `limits.cpu` becomes `cpu.max`. Which is why
removing a CPU *limit* can improve latency dramatically while removing a CPU *request* does nothing
until the node is contended.

**Where to look.** `cpu.stat`, specifically `nr_throttled` against `nr_periods`. A ratio near 1
means the workload is quota-bound, whatever the utilisation graph says.

---

## Zombie processes pile up, and the runtime is not at fault

**Symptom in production.** `ps` inside a long-running container fills with `Z` state processes.
Eventually `fork()` starts failing with `EAGAIN` because the pid limit is reached, and the
application cannot start subprocesses any more.

**Reproduction.** A PID 1 that is an ordinary program, with an orphan below it:

```sh
jq '.process.args = ["/bin/sh","-c","{ sleep 0.3; } & exec sleep 4"]' config.json | sponge config.json
mars run --pid-file /tmp/z.pid zombietest &
sleep 1.2
ps -o stat= --ppid "$(cat /tmp/z.pid)" | grep -c '^Z'
```

```
1
```

**Cause.** `exec sleep 4` replaces the shell, so PID 1 *is* `sleep`. When the backgrounded child
exits, its parent is PID 1 — and PID 1 of a namespace inherits every orphan in it. `sleep` never
calls `wait()`, so the child's exit status is never collected and the process table entry stays
forever.

Nothing here is the runtime's job. After `execve` the runtime is not in the container at all; PID 1
is the application. A container's init duties — reaping orphans, forwarding signals — belong to
whatever the image chose to put at PID 1.

**Note this is the same root cause as the SIGTERM case above.** Both are the consequence of an
application being PID 1 when it was never written to be an init. `docker run --init` and Kubernetes'
`shareProcessNamespace` pause container both exist to insert a real init — `tini` reaps orphans and
forwards signals in about 200 lines.

---

## Still to come

- `permission denied` on a mount at mode `0777`, caused by user namespace uid remapping (phase 5)
- writes that vanish on restart, because the OverlayFS upper layer was not where you thought
  (phase 3)
