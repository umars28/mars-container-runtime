# Phase 3 — OverlayFS and bundle validation

Phases 1 and 2 took a directory and turned it into an isolated, resource-limited process. The
directory was a plain copy of a filesystem, used in place. That is not how any real container works.

A real container's rootfs is a stack: read-only image layers shared between every container from
that image, and one writable layer per container. This phase builds that stack, and validates the
`config.json` that describes it.

## Layering is not the runtime's job, which is why this needs explaining

The OCI **runtime**-spec has no field for image layers. `root.path` is a single directory and the
spec assumes something else already assembled it. That something else is the **image**-spec's
territory: containerd's snapshotter, Docker's `overlay2` graphdriver, podman's `c/storage`. They
unpack layers, stack them, and hand the runtime a finished mountpoint.

So a strictly conformant runtime never touches OverlayFS. `runc` does not — `crun` and `runc` accept
a `type: "overlay"` entry in `mounts`, but that mounts an overlay somewhere *inside* the container,
not as the container's root.

Assembling it anyway is the whole point here, so `mars` reads three annotations. Annotations are the
spec's own extension mechanism, so this stays valid `config.json` that any other runtime will parse
(and ignore):

```json
{
  "root": { "path": "merged" },
  "annotations": {
    "dev.mars.overlay.lowerdir": "layers/03:layers/02:layers/01:layers/00",
    "dev.mars.overlay.upperdir": "diff",
    "dev.mars.overlay.workdir": "work"
  }
}
```

Relative paths resolve against the bundle directory. Omitting `upperdir` and `workdir` gives a
read-only container.

The mount happens **inside the container's mount namespace**, after `unshare(CLONE_NEWNS)` and
before `pivot_root`. Two consequences, both wanted:

- the assembled filesystem never appears on the host, and disappears when the last process in the
  namespace exits — no leaked mountpoint to clean up, no `umount` in an error path
- `upperdir` is an ordinary host directory, so writes are inspectable from outside while the
  container runs and after it exits

`mars` therefore refuses an overlay rootfs when the spec declares no mount namespace, rather than
mounting it on the host:

```
an overlay rootfs needs a mount namespace, otherwise the assembled filesystem would stay
visible on the host after the container exits
```

## `lowerdir` is written top layer first, and the image lists them bottom first

```
lowerdir=layers/03:layers/02:layers/01:layers/00
         ^^^^^^^^^                     ^^^^^^^^^
         highest priority              the base image
```

An OCI image manifest lists its layers in build order — base first. OverlayFS wants the opposite.
Getting this backwards produces no error at all: the container boots, and quietly runs the *first*
version of every file that was later modified. A `RUN apt-get upgrade` in the Dockerfile silently
does nothing.

`scripts/oci-bundle.sh` reverses the manifest order for exactly this reason, and prints what it
wrote so the ordering is visible rather than assumed:

```
  layers/00    9.1M
  layers/01    8.0K
  layers/02     16K  1 whiteout(s) converted
  layers/03    8.0K  1 whiteout(s) converted

bundle ready: /var/tmp/mars-layered
  layers   4 (lowerdir is written topmost-first: layers/03:layers/02:layers/01:layers/00)
```

## Deletion is a character device

OverlayFS never modifies a lower layer. Deleting a file that exists only below creates a **whiteout**
in the upper layer: a character device with major and minor both `0`.

```sh
$ mars run demo   # the container ran: rm /etc/alpine-release
$ stat -c '%n %F' diff/etc/alpine-release
diff/etc/alpine-release character special file
$ ls diff/etc/alpine-release
c--------- 2 root root 0, 0 diff/etc/alpine-release
$ ls layers/00/etc/alpine-release
layers/00/etc/alpine-release          # still there, untouched
```

Removing a whole directory sets `trusted.overlay.opaque=y` on the replacement directory instead, so
the lower layer's contents stop showing through.

Two things follow that matter in production:

1. **Deleting files inside a container frees no disk space.** The bytes are in a lower layer that
   nothing rewrote. `du` on the merged view shrinks; the disk does not.
2. `trusted.*` xattrs need `CAP_SYS_ADMIN`, so a rootless container cannot write a whiteout at all
   unless the overlay is mounted with `userxattr`, which moves them to `user.overlay.*`. That is
   phase 5's problem, and it is why rootless podman defaults to `fuse-overlayfs`.

Image layer tarballs use a *different* convention — a regular file named `.wh.<name>` — which the
snapshotter converts. `scripts/oci-bundle.sh` does that conversion so the layers it unpacks behave
like real ones:

```sh
if [[ "$name" == ".wh..wh..opq" ]]; then
  setfattr -n trusted.overlay.opaque -v y "$parent"
else
  mknod "$parent/${name#.wh.}" c 0 0
fi
```

## `workdir`, and why the kernel will not tell you what is wrong

`workdir` is scratch space OverlayFS uses to stage copy-up and rename operations atomically. It has
three requirements, and violating any of them produces the same useless error.

The kernel returns `EINVAL` and puts the real reason in the ring buffer, where no runtime looks:

```sh
$ mount -t overlay overlay -o lowerdir=…,upperdir=/x/upper,workdir=/x/upper/work /x/merged
mount: /x/merged: wrong fs type, bad option, bad superblock on overlay,
       missing codepage or helper program, or other error.
$ dmesg | tail -1
overlayfs: workdir and upperdir must be separate subtrees
```

```sh
$ mount -t overlay overlay -o lowerdir=…,upperdir=/x/upper,workdir=/x/tmpwork /x/merged
mount: /x/merged: wrong fs type, bad option, bad superblock on overlay, …
$ dmesg | tail -1
overlayfs: workdir and upperdir must reside under the same mount
```

The third one is worse, because the errno actively misleads:

```sh
$ mount -t overlay overlay -o lowerdir=/x/upper,upperdir=/x/upper,workdir=/x/work /x/merged
mount: /x/merged: mount(2) system call failed: Too many levels of symbolic links.
$ dmesg | tail -1
overlayfs: conflicting lowerdir path
```

`ELOOP` — "too many levels of symbolic links" — for a configuration error involving no symlinks at
all. Anyone debugging that from the errno alone is looking in the wrong place.

So `mars` checks these before calling `mount`, and says which one failed:

```
overlay rootfs: workdir /b/diff/work is inside upperdir /b/diff; the container would see the
runtime's scratch directory as part of its own filesystem

overlay rootfs: workdir /b/work is on device 46 but upperdir /b/diff is on device 254; overlayfs
renames files between the two and cannot cross a filesystem boundary
```

The same-filesystem rule is a `st_dev` comparison, not a heuristic — `rename(2)` cannot cross a
filesystem boundary, and copy-up depends on being able to rename the staged file into place.

One more that only shows up on a read-only overlay:

```
$ mount -t overlay overlay -o lowerdir=/x/lower /x/merged
$ dmesg | tail -1
overlayfs: at least 2 lowerdir are needed while upperdir nonexistent
```

With no `upperdir`, a single `lowerdir` has nothing to merge, and the legacy `lowerdir=` option
refuses it. Kernel 6.8 still enforces this. `mars` reports it as a configuration problem rather than
forwarding an `EINVAL`.

## The mount option string is capped at one page, and the kernel truncates it silently

This is the sharpest thing in this phase. `mount(2)`'s `data` argument is copied by
`copy_mount_options`, which copies **at most one page** — 4096 bytes. Longer strings are not
rejected. They are cut off.

41 layers with long directory names, at 4223 bytes of absolute paths:

```sh
$ mount -t overlay overlay -o "lowerdir=$ABS,upperdir=$B/diff,workdir=$B/work" $B/merged
mount: /var/tmp/mars-long/merged: special device overlay does not exist.
$ dmesg | tail -1
overlayfs: failed to resolve '/var/tmp/mars-long/layer-with-a-deliberately-long-dire': -2
```

Look at the path in that message. The real directory is
`layer-with-a-deliberately-long-directory-name-to-blow-the-page-limit-xxxxxxxxxxxxxxxxxxxx-07`; the
kernel saw it cut off mid-word at the 4096-byte boundary. And the error it reports is `ENOENT` on
**`overlay`, the source** — which is not a real path and was never the problem. Nothing in that
output points at "you have too many layers."

This is why `/var/lib/docker/overlay2` looks the way it does. Those `l/BSHXFN2…` symlinks are not
obfuscation; they are Docker keeping the option string short enough to survive the page limit.

`mars` does the equivalent — it measures the string, and if it is over the limit it finds the
deepest common parent of every layer, `chdir`s there, and passes relative paths:

```
DEBUG overlay rootfs prepared lower=41 readonly=false merged=/tmp/mars-it/ovlpage/merged
DEBUG overlay option string exceeded one page, using paths relative to a common parent
      base=/tmp/mars-it bytes=3742
```

4223 bytes becomes 3742 and the mount succeeds. If even the relative form does not fit, that is
reported as a configuration error naming the byte count, rather than being handed to the kernel to
truncate.

There is a second escaping trap in the same string. `:` separates lower layers and `,` separates
mount options. The kernel accepts `\:` for a literal colon, but there is **no escape for a comma** —
a layer directory with a comma in its name silently becomes two half-parsed options. `mars` rejects
it rather than passing it through:

```
overlay rootfs: "/layers/a,b" contains a comma; overlayfs separates mount options with commas
and provides no way to escape one
```

## Validating `config.json` instead of discovering it later

The other half of this phase. A bad `config.json` used to surface as a syscall failure deep in the
init process, after three forks, with the real cause several layers below the error. Now it fails
before the first `fork()`.

The checks that come straight from the spec's own MUSTs:

| Check | Why the spec requires it |
|---|---|
| `ociVersion` is `1.x` | a `2.x` spec may mean something different by the same field |
| `root.path` present and non-empty | there is nothing to run without it |
| `process.args` non-empty, `args[0]` non-empty | `execve` needs a program |
| `process.cwd` absolute | a relative `cwd` has no defined starting point inside the container |
| `process.env` entries contain `=` | `execve` takes `KEY=VALUE`; a bare word is silently dropped |
| no namespace type listed twice | the spec makes duplicates an error |
| `hostname` only with a UTS namespace | otherwise `sethostname` renames the **host** |
| mount destinations absolute | resolved against the rootfs, so a relative path is ambiguous |
| `rootfsPropagation` is one of the four legal values | a typo would otherwise be ignored |
| no duplicate `rlimits`, no soft limit above its hard limit | the spec makes both errors |
| `solaris`/`windows`/`vm`/`zos` absent | out of scope, and silently ignoring them would be worse |

The hostname rule is the one worth dwelling on. Without a UTS namespace, `sethostname(2)` from a
privileged process succeeds — and renames the machine the runtime is running on. It is not a
theoretical concern; it is a one-word omission in a `config.json` away.

```
$ mars run badhost
mars: config.json is invalid: hostname is set but the container has no UTS namespace,
      so setting it would rename the host
```

## Verified behaviour

```
config.json validation
  ok   hostname without a UTS namespace is refused
  ok   a relative process.cwd is refused
  ok   a namespace listed twice is refused
  ok   a runtime-spec major version we do not implement is refused
  ok   a malformed env entry is refused

overlay rootfs assembled from two lower layers
  ok   top layer file is visible
  ok   base layer file is visible under it (3.20.10)
  ok   a whiteout in the top layer hides the base layer file
  ok   container ran to completion
  ok   a container write lands in upperdir
  ok   a container delete becomes a whiteout device in upperdir
  ok   the lower layer was not written to
  ok   the deleted file still exists in the lower layer
  ok   the overlay mount does not leak onto the host

overlay is read-only when no upperdir is given
  ok   writing to a read-only overlay fails inside the container

overlay option string over one page, the reason overlay2 uses short symlinks
  ok   the absolute option string really is over 4096 bytes (4223)
  ok   the bottom-most long-named layer is readable
  ok   the top-most long-named layer is readable

overlay misconfiguration is diagnosed by the runtime, not left to the kernel
  ok   workdir nested inside upperdir is named as such
  ok   workdir on another filesystem is named as such
  ok   a single-layer read-only overlay is named as such
  ok   an overlay without a mount namespace is refused

49 passed, 0 failed
```

## Not yet done

`root.readonly` is parsed and ignored; remounting the rootfs read-only is phase 5, along with
`maskedPaths` and `readonlyPaths` (validated here, not yet applied).

Rootless overlay needs the `userxattr` mount option so whiteouts land in `user.overlay.*` instead of
`trusted.overlay.*`, and a kernel new enough to accept unprivileged overlay mounts inside a user
namespace. Also phase 5.

`metacopy` and `redirect_dir` are left at the kernel's defaults. Both change copy-up behaviour in
ways worth understanding — `metacopy=on` copies only metadata on a `chown`, which makes the upper
layer much smaller and makes layers non-portable between kernels — but neither is needed to run a
container.

Layer *extraction* stays in `scripts/oci-bundle.sh` rather than the runtime. Pulling images and
unpacking layers is image-spec work, and out of scope on purpose; the script exists so the overlay
code can be tested against layers with real whiteouts in them.
