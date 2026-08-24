# Phase 6 — Docker drop-in, and where cold start actually goes

Two claims to make good on. First: `mars` can replace `runc` under a real Docker daemon, not just
pass a test suite. Second: the startup path is instrumented well enough to answer "why does starting
a container take this long", with a number rather than a guess.

## Docker calls the runtime, and tells you nothing when it fails

Registering the runtime is two lines of `daemon.json`:

```json
{
  "runtimes": { "mars": { "path": "/usr/local/bin/mars" } },
  "exec-opts": ["native.cgroupdriver=cgroupfs"]
}
```

The second line is not optional. Docker defaults to the **systemd** cgroup driver, which asks systemd
over dbus to create a scope unit and delegate a cgroup into it. `mars` writes to `cgroupfs` directly.
Run both and they disagree about who owns `/sys/fs/cgroup/<container>` — a conflict the plan flagged
back in phase 2 and which is why the driver is out of scope here.

Then the first attempt:

```
docker: Error response from daemon: failed to create task for container: failed to create shim
task: OCI runtime create failed: /usr/local/bin/mars did not terminate successfully: exit status 1
```

That is the entire diagnostic. `containerd` captures the runtime's stderr into a log the error does
not name, and "exit status 1" is all that reaches the CLI. The way out is a wrapper that records both
the arguments and the stderr — `scripts/install-docker-runtime.sh` installs one with `TRACE=1`:

```sh
#!/bin/sh
printf 'argv: %s %s\n' "$0" "$*" >>/var/tmp/mars-docker.log
/usr/local/bin/mars "$@" 2>>/var/tmp/mars-docker.log
```

Which immediately shows exactly how Docker drives a runtime:

```
cwd=/run/containerd/io.containerd.runtime.v2.task/moby/2d8a5acd…
argv: mars --root /var/run/docker/runtime-runc/moby
           --log …/log.json --log-format json
           create --bundle /run/containerd/io.containerd.runtime.v2.task/moby/2d8a5acd…
                  --pid-file …/init.pid 2d8a5acd…
```

`create`, then `start`, then `state` on a timer, then `kill`, then `delete --force` — the split from
[phase 4](04-lifecycle.md) is exactly what this interface needs. Three incompatibilities showed up,
in order.

### `time` is not optional any more

```
mars: time namespace is deliberately out of scope for mars
```

Docker 29 puts a time namespace in every spec it generates:

```json
[{"type":"mount"},{"type":"network"},{"type":"time"},{"type":"uts"},
 {"type":"pid"},{"type":"ipc"},{"type":"cgroup"}]
```

The plan had listed the time namespace as out of scope. A drop-in runtime does not get that choice.
It is `unshare(CLONE_NEWTIME)` with the same semantics as `CLONE_NEWPID` — the caller is not moved,
the first child is — which the existing fork chain already satisfies. `linux.timeOffsets`, if present,
goes to `/proc/self/timens_offsets`, and can only be written while the namespace has no other member,
which is why the intermediate writes it immediately after unsharing rather than leaving it to the init.

Implementing it exposed the `bitflags` truncation bug written up in
[phase 5](05-hardening.md#flag-dropped-a-namespace-quietly) — the flag was accepted, then silently
dropped, and the container ran in the host's time namespace with no error at all.

### `delete --force` has to be idempotent

```
mars: container 2d8a5acd… does not exist
exit=1
```

`containerd` calls `delete --force` during cleanup, including for containers whose creation already
failed. `runc` returns 0 there:

```sh
$ runc delete --force does-not-exist; echo $?
0
```

Reporting "does not exist" as a failure turns every failed create into two errors, and the second one
is the one the user sees. Now `--force` on a missing container logs at debug and exits 0. Without
`--force` it is still an error, because then the caller is asserting the container exists.

### Sysctls before hardening

The third was not a Docker-specific bug, just a bug that only Docker's spec triggered — `/proc/sys`
in `readonlyPaths` plus a `net.ipv4.ping_group_range` sysctl. Written up in
[phase 5](05-hardening.md#read-only-rootfs-and-the-ordering-that-broke-it-twice).

### Working

```sh
$ docker run --rm --runtime=mars alpine:3.20 echo hello-from-mars
hello-from-mars

$ docker run --rm -it --runtime=mars alpine:3.20 sh -c 'tty; id -u; hostname'
/dev/pts/0
0
49656ce0cb05

$ docker exec m1 sh -c 'echo exec-ok; ls -d /proc/[0-9]* | wc -l'
exec-ok
4

$ docker run --rm --runtime=mars --memory=64m alpine:3.20 cat /sys/fs/cgroup/memory.max
67108864
```

And the one that exercises signal forwarding end to end, since a container that ignores `SIGTERM`
takes the full grace period:

```sh
$ docker run -d --runtime=mars alpine:3.20 sh -c 'trap "echo got-sigterm; exit 0" TERM; …'
$ time docker stop m2
real    0m0.289s
$ docker logs m2
got-sigterm
```

0.289s, not 10s. The signal arrived, the handler ran, the container exited on its own.

## Telemetry that cannot use the OpenTelemetry SDK

The plan called for `opentelemetry-otlp`. That turned out to be the wrong choice, for a reason that
is specific to this program: **the SDK's exporter runs on a background thread, and this process must
stay single-threaded.**

Two separate constraints say so. After `fork()`, only async-signal-safe work is legal in the child, so
a runtime that forks must not have a thread pool running. And `setns(2)` refuses to move a
multi-threaded process into a new mount or user namespace, which is the whole reason
[`exec` works in Rust](04-lifecycle.md#exec-and-why-this-project-is-in-rust) without a C shim. Linking
an async runtime in and hoping it stays idle is not a guarantee; the check in `mars exec` would start
failing and the fix would be to remove the SDK anyway.

So the exporter here is about 120 lines: build the OTLP/HTTP JSON document, open a TCP socket, write
one POST, read the status, exit. No threads, no async runtime, nothing running at `fork()` time. The
trace is sent **after** the container's process has started, from the parent, once the forking is over.

Timings are collected as plain durations. The runtime's own phases come from an `Instant`; the
intermediate and the init each keep their own recorder and hand the phase list back over the existing
socketpair — the intermediate with `InitPid`, the init with `InitReady`. The parent shifts them onto
its own clock and emits one span per phase.

Getting that shift right took two attempts. The child's recorder started when the child began, which
included the time it spent *blocked* waiting for the parent's go-ahead — so its spans appeared before
the parent's `createRuntime` hook, which is impossible. Starting the child's recorder after the
handshake instead makes the offset exact.

Real Tempo accepts it:

```sh
$ curl -s localhost:3200/metrics | grep receiver_accepted
tempo_receiver_accepted_spans{receiver="tempo/otlp_receiver",transport="http"} 50
$ curl -s localhost:3200/api/traces/4c0eb6fc778c7325f323c57019eac033 | head -c 120
{"batches":[{"resource":{"attributes":[{"key":"service.version", …
```

For checking the payload without a Tempo instance, `scripts/otlp-echo.py` prints the span tree it
receives.

## Where container cold start actually goes

This is what the instrumentation was for.

```
trace 10a2ae966c82f09af3c6d2282991b79c  25 spans  11892us total
       1us    446us  bundle
     448us      9us  plan
     458us    279us  cgroup                     ← create the cgroup, write the limits
     737us     60us  fork
     747us     13us  intermediate.join
     767us    124us  intermediate.unshare.mnt
     892us      4us  intermediate.unshare.uts
     896us    103us  intermediate.unshare.ipc
     999us      8us  intermediate.unshare.pid
    1012us    895us  intermediate.unshare.net    ← creating a network namespace is not free
    1908us     63us  intermediate.fork.init
     800us   1471us  wait.initpid
    2271us     86us  reap.intermediate
    2358us   7065us  cgroup.attach               ← 59% of the whole thing
    9423us      1us  hooks.createRuntime
    9424us   1240us  init
   10664us    102us  state
    9438us    492us  init.rootfs.mount
    9931us    232us  init.pivot_root
   10164us    162us  init.harden
   10327us     64us  init.identity
   10391us    118us  init.security
```

**Writing one pid into `cgroup.procs` is the single most expensive step in starting a container.**
Across three runs it was 6.6ms, 9.5ms and 14.7ms, out of totals of 11.9ms, 14.2ms and 18.5ms — between
55% and 79% of cold start. Creating the cgroup and writing every limit into it took 279µs; *moving one
process into it* took twenty-five times longer.

Measured directly, away from `mars`, the shape becomes clear:

```sh
$ # move the same process in and out of a freshly created cgroup, five times
  move into mars-probe: 11268us     ← first
  move back to root:      770us
  move into mars-probe:   702us     ← same cgroup, second time
  move back to root:      639us
  move into mars-probe:   615us
```

The **first** migration into a new cgroup costs ~11ms; every later one costs ~650µs. So it is
first-touch cost, not per-write cost — the kernel allocating and initialising per-cgroup controller
state, and taking `cgroup_threadgroup_rwsem` as a writer, which needs an RCU grace period across
every CPU. Both are paid once per cgroup.

Which is exactly the wrong shape for containers, because **every container gets a fresh cgroup**. The
one code path that could amortise the cost never gets to.

Two consequences worth carrying to production:

- A "container startup is slow" investigation that looks at image pulls, rootfs assembly, or the
  runtime binary is looking in the wrong place for the first ~10ms. `rootfs.mount` +
  `pivot_root` + hardening together are under 900µs here.
- The second most expensive item is `unshare(CLONE_NEWNET)` at ~900µs, an order of magnitude above
  every other namespace. That is the kernel building a fresh network stack — its own loopback device,
  its own routing tables, its own per-namespace state. It is also why `--network=host` visibly
  speeds up short-lived containers.

Neither of these is something I would have guessed. The `cgroup.attach` number in particular I first
assumed was an instrumentation bug, which is why the direct measurement above exists.

## Not yet done

The trace covers `create` and `run`. `exec` is not instrumented, deliberately — it is the one command
where a stray thread would break `setns`, so nothing is added to its path until there is a reason.

There is no metrics or logs export, only traces. `mars events --stats` already reads the numbers a
metrics exporter would need; wiring it to Prometheus would be a small addition and is not here.
