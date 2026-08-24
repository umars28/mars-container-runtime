# Phase 5 — hardening and user namespaces

Everything up to here made a container *isolated*. None of it made the container *safe*: it ran as
real root, on a writable rootfs, with every capability and every syscall the kernel offers. This
phase closes that, and adds the one namespace that changes what "root" means.

Capabilities were pulled forward into [phase 4](04-lifecycle.md#capabilities-and-an-ordering-the-kernel-enforces)
because the validation suite depended on them. What is left: read-only rootfs, masked and read-only
paths, seccomp, and user namespaces.

## Read-only rootfs, and the ordering that broke it twice

Three mount operations, each with an ordering constraint that only shows up when you get it wrong.

**`maskedPaths`** hides a path completely. For a file, bind `/dev/null` over it; for a directory,
mount an empty read-only `tmpfs`. That is how `/proc/kcore` — a mapping of all physical memory —
stops being readable inside a container:

```sh
$ cat /proc/kcore | head -c 8
$              # nothing, because /dev/null is bound over it
```

**`readonlyPaths`** cannot simply "remount that path read-only", because a path is not a mount. It
has to be made one first, by bind-mounting it onto itself, and only then remounted with `MS_RDONLY`:

```rust
mount(Some(path), path, None, MS_BIND | MS_REC, None)?;
mount(Some(path), path, None, MS_BIND | MS_REC | MS_REMOUNT | MS_RDONLY | …, None)?;
```

**`root.readonly`** remounts `/` with `MS_BIND | MS_REMOUNT | MS_RDONLY` after everything else,
because nothing that needs to write to the rootfs can run after it.

The first thing this broke was my own test suite. `mars spec` generates `"readonly": true` — as
`runc spec` does — and until this phase the field was parsed and ignored. Turning it on made every
test that wrote into the container fail at once. That is the correct behaviour and the tests were
wrong: the OCI default really is a read-only rootfs, and anything that needs to write needs a volume
or an explicit `readonly: false`.

The second break was subtler and came from Docker. The container came up, then:

```
mars: container init failed: write sysctl net.ipv4.ping_group_range=0 2147483647 to
/proc/sys/net/ipv4/ping_group_range: Read-only file system (os error 30)
```

`readonlyPaths` in the default Docker spec contains `/proc/sys`. `linux.sysctl` contains
`net.ipv4.ping_group_range`. I was applying the sysctls *after* the hardening, so the runtime made
`/proc/sys` read-only and then tried to write to it. Sysctls have to go first — but still after
`pivot_root`, because before it `/proc/sys` is the **host's**, and writing there would change the
host's kernel settings.

There is exactly one window: after `pivot_root`, before `readonlyPaths`.

## Seccomp, and a filter that turns on a flag behind your back

The filter itself is mechanical: translate `defaultAction`, add the architectures, translate each
rule's action and argument comparators, load. The OCI action set maps cleanly onto libseccomp, with
one refusal — `SCMP_ACT_NOTIFY` needs a listener process to receive the notification fd, which is a
whole subsystem, so `mars` reports it as out of scope rather than silently allowing the syscall.

What it looks like working, with two rules that return *different* errnos:

```sh
$ chmod 700 /tmp
chmod: /tmp: Operation not permitted      # errnoRet: 1
$ mkdir /tmp/d
mkdir: can't create directory '/tmp/d': Function not implemented   # errnoRet: 38
```

The interesting part is what loading a filter does that nobody asked for. After adding seccomp, nine
validation tests that had been passing started failing on a single assertion:

```
not ok 292 - has expected noNewPrivileges
```

`seccomp(2)` requires the caller to hold `CAP_SYS_ADMIN` **or** to have `no_new_privs` set — the
kernel will not let an unprivileged process install a filter that a later setuid binary could be
tricked by. libseccomp handles that for you by setting `no_new_privs` itself as part of
`seccomp_load()`. Which means: loading any filter silently sets a process flag that the OCI spec has
its own field for, and `runtimetest` compares that field against reality.

So the filter has to be told not to:

```rust
filter.set_filter_attr(ScmpFilterAttr::CtlNnp, 0)?;
```

And once the runtime owns that flag, it owns the ordering too, because the requirement does not go
away:

- **`noNewPrivileges: false`** — the load needs `CAP_SYS_ADMIN`, so seccomp must be applied
  **before** capabilities are dropped.
- **`noNewPrivileges: true`** — set the flag first, then drop capabilities, then load the filter
  **last**, so the smallest possible number of syscalls runs under it. Fewer syscalls after the load
  means a profile can be tighter.

`runc` splits it the same way, with a comment explaining each branch. Getting it wrong in either
direction produces `EACCES` from `seccomp(2)` with nothing pointing at capabilities.

## A user namespace makes you nobody first

This is the namespace the three-level fork chain from [phase 1](01-isolation.md) was built for, and
it took the longest to get right.

`uid_map` and `gid_map` must be written from **outside** the namespace by a process with privilege
over it — a process cannot map itself. So the intermediate unshares `CLONE_NEWUSER`, asks the parent
over the socketpair, and waits:

```
intermediate:  unshare(CLONE_NEWUSER)
               ─── "map me" ──►
parent:                          write /proc/<intermediate>/setgroups  = "deny"
                                 write /proc/<intermediate>/uid_map
                                 write /proc/<intermediate>/gid_map
               ◄── "mapped" ───
```

`setgroups=deny` comes first, and only for an unprivileged runtime. Without it the kernel refuses to
let an unprivileged process write `gid_map` at all — because dropping a group can *grant* access
where a negative group permission was denying it, and an unprivileged user must not be able to
engineer that.

For more than one mapping line, or ranges outside your own id, writing directly needs `CAP_SETUID` in
the parent namespace. `mars` tries the direct write and falls back to `newuidmap`/`newgidmap`, the
setuid helpers from the `uidmap` package, which check the ranges against `/etc/subuid` and
`/etc/subgid`. Get the range wrong and the helper says so:

```
newuidmap: uid range [0-65536) -> [100000-165536) not allowed
```

### Two things that fail with errors pointing somewhere else

**`EOVERFLOW` from `mkdir`.** With a mapping of container 0..65535 → host 100000..165535, the
container came up as far as the rootfs and then:

```
mars: container init failed: create /var/tmp/hard/rootfs/dev/pts:
      Value too large for defined data type (os error 75)
```

Nothing in that message is about user namespaces. The cause: **creating a user namespace does not
move you into your own mapping.** The intermediate was running as host uid 0, and host uid 0 is not
inside `[100000, 165536)`, so from inside the namespace its own uid was unmappable — it read as the
overflow id, `65534`, and every file operation failed with `EOVERFLOW`.

The namespace creator holds full capabilities in the new namespace regardless of its uid, so it can
fix this itself, and must, before touching a filesystem:

```rust
setresgid(Gid::from_raw(0), …)?;
setresuid(Uid::from_raw(0), …)?;
```

`runc` does the same thing in `nsexec.c`, with the comment "become root in the namespace proper". It
is one line and nothing works without it.

**`mknod` is never permitted.** With that fixed, the next failure was:

```
mknod /var/tmp/h1/rootfs/dev/null SFlag(S_IFCHR) 1:3: EPERM
```

A process in a non-initial user namespace cannot create a device node, no matter what capabilities it
holds there — the kernel requires `CAP_MKNOD` in the *initial* namespace, because a device node is a
handle on real hardware. This is not a permission that can be delegated.

So the nodes have to come from the host, by bind mount:

```rust
match mknod(target, kind, mode, makedev(major, minor)) {
    Err(Errno::EPERM | Errno::EOVERFLOW) => return bind(target, &host),
```

`mars` binds unconditionally when the spec declares a user namespace, and falls back to binding on
`EPERM` otherwise — which is what `runc` does, and the reason rootless Podman containers have a
`/dev/null` owned by `nobody`:

```sh
$ stat -c %u /dev/null      # inside the container
65534
$ echo x > /dev/null && echo works
works
```

The ownership looks broken and is not. `/dev/null` belongs to host uid 0, which is outside the
mapping, so it displays as the overflow id. The device works because permission on a character
device does not depend on the owner being mappable.

## `!flag` dropped a namespace, quietly

The best bug of this phase, found because Docker asks for a namespace `nix` has never heard of.

`mars` unshares the cgroup namespace separately from the others — that constraint is from
[phase 2](02-cgroups.md#an-ordering-constraint-that-is-easy-to-get-wrong) — so it removes that one
flag from the set:

```rust
let unshare_flags = requested & !CloneFlags::CLONE_NEWCGROUP;
```

That line silently deletes `CLONE_NEWTIME`. `bitflags`' `Not` is defined as
`Self::from_bits_truncate(!self.bits())`, and truncation keeps only bits the type has *named*. `nix`
0.31 has no `CLONE_NEWTIME` constant, so `!CLONE_NEWCGROUP` is a mask of every flag nix knows minus
one — and `0x80` is not in it. The `&` then clears it.

There was no error. `mars` accepted `{"type": "time"}`, reported success, and produced a container in
the host's time namespace:

```sh
$ readlink /proc/self/ns/time      # inside the container
time:[4026531834]
$ readlink /proc/self/ns/time      # on the host
time:[4026531834]                  # the same one
```

The fix is to mask on raw bits instead, with a named helper so it cannot be written the wrong way
again:

```rust
pub fn without(flags: CloneFlags, unwanted: CloneFlags) -> CloneFlags {
    CloneFlags::from_bits_retain(flags.bits() & !unwanted.bits())
}
```

and a test that pins both halves — that the helper keeps the bit, and that the naive expression
loses it:

```rust
assert_eq!(without(requested, CLONE_NEWCGROUP).bits() & CLONE_NEWTIME, CLONE_NEWTIME);
assert_eq!((requested & !CLONE_NEWCGROUP).bits() & CLONE_NEWTIME, 0,
           "this is the trap the helper exists to avoid");
```

The general shape is worth remembering: **a typed bitflag wrapper is only as complete as the crate's
constant list**, and the kernel adds flags faster than bindings do. `unshare(2)` accepts an `int`; the
type system's opinion about which bits are meaningful is not the kernel's.

## Validation results

| | passed | failed | inconclusive |
|---|---|---|---|
| **mars 0.1.0** | **26** | 18 | 14 |
| runc 1.5.1 | 22 | 20 | 16 |

Five tests moved from failing to passing with this phase: `linux_masked_paths`,
`linux_readonly_paths`, `root_readonly_true`, `linux_ns_nopath` and `linux_uid_mappings` — the last
two being the user namespace ones. There is now **no test that `runc` passes and `mars` fails**.

The remaining differences, honestly labelled:

| test | mars | runc | |
|---|---|---|---|
| `linux_devices` | pass | inconclusive | the `mknod` umask fix from phase 4 |
| `delete` | pass | fail | `delete --force` is idempotent |
| `linux_mount_label`, `linux_process_apparmor_profile` | pass | inconclusive | **mars ignores these fields** — not a win |
| `linux_seccomp` | fail | fail | see below |

`linux_seccomp` is the one test where implementing the feature made `mars` fail it. The test blocks
`getcwd` with an errno rule, then runs `runtimetest` inside the container — and `runtimetest` calls
`getcwd` to check `process.cwd`. The validator cannot run under the filter it just installed. `runc`
fails it with byte-identical output, which is the clearest evidence available that `mars` is now
behaving like a runtime that implements seccomp rather than one that ignores it.

## Not yet done

Rootless in the full sense — `mars` invoked by a non-root user with no `sudo` anywhere — needs more
than the user namespace: a cgroup delegated under `user.slice`, `fuse-overlayfs` or `userxattr` for
the overlay's whiteouts, and `slirp4netns` for networking. The user namespace machinery works and is
exercised, but `mars` still expects to be started with privilege.

`SCMP_ACT_NOTIFY` is refused rather than implemented. AppArmor and SELinux labels are parsed and
ignored, which is why two validation rows above carry a caveat instead of credit. The cgroup device
allowlist needs an eBPF program on cgroup v2 and is absent, which `runc` also fails here.
