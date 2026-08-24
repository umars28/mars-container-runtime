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

## Still to come

- `OOMKilled`: exit 137, `memory.events`, and why the limit lives in `memory.max` (phase 2)
- CPU throttling at low utilisation: `cpu.max` is a quota, not a share (phase 2)
- `permission denied` on a mount at mode `0777`, caused by user namespace uid remapping (phase 5)
- zombie processes accumulating, because after `execve` PID 1 is the application, not an init
  (phase 2)
