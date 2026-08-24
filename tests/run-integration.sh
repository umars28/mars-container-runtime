#!/usr/bin/env bash
set -uo pipefail

if [[ -z "${MARS:-}" ]]; then
  for candidate in target/debug/mars target/release/mars \
    "${CARGO_TARGET_DIR:-}/debug/mars" /var/tmp/mars-target/debug/mars; do
    [[ -x "$candidate" ]] && MARS=$candidate && break
  done
fi
MARS="${MARS:-target/debug/mars}"
WORK="${WORK:-/tmp/mars-it}"
IMAGE="${IMAGE:-alpine:3.20}"

PASS=0
FAIL=0

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: this needs Linux; namespaces and cgroups do not exist elsewhere" >&2
  exit 1
fi

if [[ "$EUID" -ne 0 ]]; then
  echo "error: needs root for mount/pivot_root; use sudo -E" >&2
  exit 1
fi

if [[ ! -x "$MARS" ]]; then
  echo "error: $MARS not found; run cargo build first" >&2
  exit 1
fi

ok() {
  PASS=$((PASS + 1))
  printf '  ok   %s\n' "$1"
}

no() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
  printf '       expected: %s\n' "$2"
  printf '       actual:   %s\n' "$3"
}

check() {
  local name=$1 expected=$2 actual=$3
  if [[ "$expected" == "$actual" ]]; then
    ok "$name"
  else
    no "$name" "$expected" "$actual"
  fi
}

check_prefix() {
  local name=$1 prefix=$2 actual=$3
  if [[ "$actual" == "$prefix"* ]]; then
    ok "$name ($actual)"
  else
    no "$name" "starts with $prefix" "$actual"
  fi
}

check_lt() {
  local name=$1 limit=$2 actual=$3
  if [[ "$actual" =~ ^[0-9]+$ ]] && ((actual < limit)); then
    ok "$name ($actual < $limit)"
  else
    no "$name" "a number below $limit" "$actual"
  fi
}

bundle() {
  local dir=$1 jq_filter=$2
  rm -rf "$dir"
  mkdir -p "$dir"
  "$MARS" spec --bundle "$dir"
  cp -a "$WORK/rootfs" "$dir/rootfs"
  jq "$jq_filter | .process.terminal = false | .root.readonly = false" "$dir/config.json" >"$dir/c.tmp"
  mv "$dir/c.tmp" "$dir/config.json"
}

run_in() {
  local dir=$1
  shift
  (cd "$dir" && "$MARS" run "$@")
}

overlay_bundle() {
  local dir=$1 lower=$2 jq_filter=$3
  rm -rf "$dir"
  mkdir -p "$dir/diff" "$dir/work" "$dir/merged"
  "$MARS" spec --bundle "$dir"
  jq --arg lower "$lower" "$jq_filter
     | .root.path = \"merged\"
     | .process.terminal = false
     | .root.readonly = false
     | .annotations[\"dev.mars.overlay.lowerdir\"] = \$lower
     | .annotations[\"dev.mars.overlay.upperdir\"] = \"diff\"
     | .annotations[\"dev.mars.overlay.workdir\"] = \"work\"" \
    "$dir/config.json" >"$dir/c.tmp"
  mv "$dir/c.tmp" "$dir/config.json"
}

check_error() {
  local name=$1 needle=$2 output=$3
  if grep -qi -- "$needle" <<<"$output"; then
    ok "$name"
  else
    no "$name" "an error mentioning $needle" "${output:-<no output>}"
  fi
}

rejects() {
  local name=$1 dir=$2 needle=$3
  local output status
  output=$(run_in "$dir" "$(basename "$dir")" 2>&1)
  status=$?
  if [[ "$status" -eq 0 ]]; then
    no "$name" "a non-zero exit and an explanation" "exit 0"
  else
    check_error "$name" "$needle" "$output"
  fi
}

echo "clearing any state left by an earlier run"
for id in $("$MARS" list -q 2>/dev/null); do "$MARS" delete --force "$id" 2>/dev/null; done
find /sys/fs/cgroup/mars -mindepth 1 -maxdepth 1 -type d -exec rmdir {} + 2>/dev/null
rm -rf /run/mars

echo "preparing shared rootfs from $IMAGE"
rm -rf "$WORK"
mkdir -p "$WORK"
"$(dirname "$0")/../scripts/make-rootfs.sh" "$WORK/rootfs" >/dev/null

HOST_HOSTNAME=$(hostname)
HOST_MOUNTS=$(wc -l </proc/mounts)

echo
echo "pid namespace"
bundle "$WORK/pid" '.process.args = ["/bin/sh","-c","echo $$; ls -d /proc/[0-9]* | wc -l"]'
mapfile -t out < <(run_in "$WORK/pid" it-pid)
check "init process is PID 1" "1" "${out[0]:-<none>}"
check_lt "sees only its own processes" 5 "${out[1]:-<none>}"

echo
echo "uts namespace"
bundle "$WORK/uts" '.process.args = ["/bin/sh","-c","hostname"]'
guest_hostname=$(run_in "$WORK/uts" it-uts)
check "hostname is the spec hostname" "mars" "$guest_hostname"
if [[ "$guest_hostname" != "$HOST_HOSTNAME" ]]; then
  ok "hostname differs from the host ($HOST_HOSTNAME)"
else
  no "hostname differs from the host" "not $HOST_HOSTNAME" "$guest_hostname"
fi

echo
echo "network namespace"
bundle "$WORK/net" '.process.args = ["/bin/sh","-c","ip -o link | awk -F\": \" \"{print \\$2}\" | tr \"\\n\" \",\""]'
guest_links=$(run_in "$WORK/net" it-net)
check "only loopback exists" "lo," "$guest_links"

echo
echo "mount namespace and pivot_root"
bundle "$WORK/mnt" '.process.args = ["/bin/sh","-c","wc -l < /proc/mounts; grep -c Users /proc/mounts; cat /etc/alpine-release"]'
mapfile -t mnt < <(run_in "$WORK/mnt" it-mnt)
if [[ "${mnt[0]:-999}" -lt "$HOST_MOUNTS" ]]; then
  ok "fewer mounts than the host (${mnt[0]} < $HOST_MOUNTS)"
else
  no "fewer mounts than the host" "< $HOST_MOUNTS" "${mnt[0]:-<none>}"
fi
check "no host virtiofs mount leaked in" "0" "${mnt[1]:-<none>}"
check_prefix "root is the alpine rootfs" "3.20" "${mnt[2]:-<none>}"

echo
echo "exit code propagation"
bundle "$WORK/exit" '.process.args = ["/bin/sh","-c","exit 42"]'
run_in "$WORK/exit" it-exit
check "non-zero exit propagates" "42" "$?"

await_init() {
  local pid_file=$1
  for _ in $(seq 100); do
    [[ -s "$pid_file" ]] && return 0
    sleep 0.1
  done
  return 1
}

echo
echo "signal forwarding to a PID 1 that installs a handler"
bundle "$WORK/sigtrap" '.process.args = ["/bin/sh","-c","trap \"exit 42\" TERM; while true; do sleep 0.2; done"]'
rm -f "$WORK/sigtrap.pid"
env -C "$WORK/sigtrap" "$MARS" run --pid-file "$WORK/sigtrap.pid" it-sigtrap &
runtime_pid=$!
if await_init "$WORK/sigtrap.pid"; then
  ok "pid file written by the runtime"
  sleep 0.3
  kill -TERM "$runtime_pid"
  wait "$runtime_pid"
  check "handler ran, its exit code propagates" "42" "$?"
else
  no "pid file written by the runtime" "a pid" "nothing"
  kill -KILL "$runtime_pid" 2>/dev/null
fi

echo
echo "PID 1 without a handler ignores SIGTERM from an ancestor namespace"
bundle "$WORK/sigbare" '.process.args = ["/bin/sleep","30"]'
rm -f "$WORK/sigbare.pid"
env -C "$WORK/sigbare" "$MARS" run --pid-file "$WORK/sigbare.pid" it-sigbare &
runtime_pid=$!
if await_init "$WORK/sigbare.pid"; then
  init_pid=$(cat "$WORK/sigbare.pid")
  check "init pid names a live process" "0" "$(
    kill -0 "$init_pid" 2>/dev/null
    echo $?
  )"

  sleep 0.3
  kill -TERM "$runtime_pid"
  sleep 0.7
  if kill -0 "$init_pid" 2>/dev/null; then
    ok "SIGTERM was discarded by the kernel, container still running"
  else
    no "SIGTERM discarded for a handler-less PID 1" "still running" "gone"
  fi

  kill -KILL "$init_pid"
  wait "$runtime_pid"
  check "SIGKILL is forcibly delivered, reported as 128+9" "137" "$?"
else
  no "pid file written by the runtime" "a pid" "nothing"
  kill -KILL "$runtime_pid" 2>/dev/null
fi

echo
echo "device nodes in a fresh /dev tmpfs"
bundle "$WORK/dev" '.process.args = ["/bin/sh","-c","echo x > /dev/null && echo null-ok; head -c 4 /dev/zero | wc -c; [ -L /dev/stdout ] && echo stdout-symlink-ok"]'
mapfile -t dev < <(run_in "$WORK/dev" it-dev)
check "/dev/null is writable" "null-ok" "${dev[0]:-<none>}"
check "/dev/zero reads" "4" "${dev[1]:-<none>}"
check "/dev/stdout is a symlink into /proc/self/fd" "stdout-symlink-ok" "${dev[2]:-<none>}"

echo
echo "cgroup placement and cleanup"
bundle "$WORK/cg" '.process.args = ["/bin/sleep","10"]'
rm -f "$WORK/cg.pid"
env -C "$WORK/cg" "$MARS" run --pid-file "$WORK/cg.pid" it-cg &
runtime_pid=$!
if await_init "$WORK/cg.pid"; then
  init_pid=$(cat "$WORK/cg.pid")
  cg=/sys/fs/cgroup/mars/it-cg

  if [[ -d "$cg" ]]; then
    ok "cgroup created at $cg"
    check "init pid is in cgroup.procs" "$init_pid" "$(head -1 "$cg/cgroup.procs")"
    check "memory controller is delegated here" "0" "$(
      grep -qw memory "$cg/../cgroup.subtree_control"
      echo $?
    )"
  else
    no "cgroup created at $cg" "a directory" "missing"
  fi

  kill -KILL "$init_pid"
  wait "$runtime_pid"

  if [[ ! -d "$cg" ]]; then
    ok "the container cgroup is removed when it exits"
  else
    no "the container cgroup is removed when it exits" "gone" "$(ls "$cg")"
  fi
else
  no "pid file written by the runtime" "a pid" "nothing"
  kill -KILL "$runtime_pid" 2>/dev/null
fi

echo
echo "cgroup namespace"
bundle "$WORK/cgns" '.process.args = ["/bin/sh","-c","cat /proc/self/cgroup"]'
cgns=$(run_in "$WORK/cgns" it-cgns)
check "container sees its own cgroup as the root" "0::/" "$cgns"

echo
echo "memory.max and OOM kill"
bundle "$WORK/oom" '.linux.resources.memory.limit = 33554432
  | .process.args = ["/usr/bin/awk","BEGIN{s=\"\";while(1){s = s sprintf(\"%1000000s\",\"\")}}"]'
oom_log=$(run_in "$WORK/oom" it-oom 2>&1)
check "OOM kill is reported as 128+9" "137" "$?"
if grep -q "OOM killed" <<<"$oom_log"; then
  ok "runtime names memory.max as the cause, read from memory.events"
else
  no "runtime explains the OOM kill" "a warning naming memory.max" "$oom_log"
fi

echo
echo "cpu.max quota causes throttling"
bundle "$WORK/cpu" '.linux.resources.cpu.quota = 10000
  | .linux.resources.cpu.period = 100000
  | .process.args = ["/bin/sh","-c","i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done"]'
cpu_log=$(run_in "$WORK/cpu" it-cpu 2>&1)
if grep -q "CPU throttled" <<<"$cpu_log"; then
  ok "runtime reports throttling, read from cpu.stat"
else
  no "runtime reports throttling" "a message naming cpu.max" "$cpu_log"
fi

echo
echo "pids.max blocks fork"
bundle "$WORK/pids" '.linux.resources.pids.limit = 5
  | .process.args = ["/bin/sh","-c","for i in 1 2 3 4 5 6 7 8 9 10; do sleep 3 & done; echo all-forked"]'
pids_log=$(run_in "$WORK/pids" it-pids 2>&1)
if grep -qi "can.t fork" <<<"$pids_log"; then
  ok "fork rejected with EAGAIN once pids.max is reached"
else
  no "fork rejected once pids.max is reached" "a fork failure" "$pids_log"
fi

echo
echo "zombie reaping is the application's job, not the runtime's"
bundle "$WORK/zombie" '.process.args = ["/bin/sh","-c","{ sleep 0.3; } & exec sleep 4"]'
rm -f "$WORK/zombie.pid"
env -C "$WORK/zombie" "$MARS" run --pid-file "$WORK/zombie.pid" it-zombie &
runtime_pid=$!
if await_init "$WORK/zombie.pid"; then
  init_pid=$(cat "$WORK/zombie.pid")
  sleep 1.2
  zombies=$(ps -o stat= --ppid "$init_pid" 2>/dev/null | grep -c '^Z')
  if [[ "$zombies" -ge 1 ]]; then
    ok "PID 1 is the application after execve, so orphans stay unreaped ($zombies zombie)"
  else
    no "orphans stay unreaped under a non-init PID 1" "at least one zombie" "$zombies"
  fi
  kill -KILL "$init_pid"
  wait "$runtime_pid"
else
  no "pid file written by the runtime" "a pid" "nothing"
  kill -KILL "$runtime_pid" 2>/dev/null
fi

echo
echo "config.json validation"
bundle "$WORK/badhost" '.linux.namespaces = [{"type":"pid"},{"type":"mount"}] | .hostname = "guest"'
rejects "hostname without a UTS namespace is refused" "$WORK/badhost" "UTS namespace"

bundle "$WORK/badcwd" '.process.cwd = "relative/dir"'
rejects "a relative process.cwd is refused" "$WORK/badcwd" "not an absolute path"

bundle "$WORK/baddup" '.linux.namespaces += [{"type":"pid"}]'
rejects "a namespace listed twice is refused" "$WORK/baddup" "more than once"

bundle "$WORK/badver" '.ociVersion = "2.0.0"'
rejects "a runtime-spec major version we do not implement is refused" "$WORK/badver" "major version"

bundle "$WORK/badenv" '.process.env += ["NOT_A_PAIR"]'
rejects "a malformed env entry is refused" "$WORK/badenv" "KEY=VALUE"

echo
echo "preparing overlay lower layers"
LOWER="$WORK/lower"
mkdir -p "$LOWER"
cp -a "$WORK/rootfs" "$LOWER/base"
mkdir -p "$LOWER/top"
echo "written by the top layer" >"$LOWER/top/from-top.txt"
mkdir -p "$LOWER/top/etc"
mknod "$LOWER/top/etc/hostname" c 0 0
echo "  base $(du -sh "$LOWER/base" | cut -f1), top adds from-top.txt and whiteouts /etc/hostname"

echo
echo "overlay rootfs assembled from two lower layers"
overlay_bundle "$WORK/ovl" "$LOWER/top:$LOWER/base" \
  '.process.args = ["/bin/sh","-c","cat /from-top.txt; cat /etc/alpine-release; [ -e /etc/hostname ] && echo whiteout-FAILED || echo whiteout-ok; echo from-container > /scratch.txt; rm -f /etc/alpine-release; echo done"]'
mapfile -t ovl < <(run_in "$WORK/ovl" ovl)
check "top layer file is visible" "written by the top layer" "${ovl[0]:-<none>}"
check_prefix "base layer file is visible under it" "3.20" "${ovl[1]:-<none>}"
check "a whiteout in the top layer hides the base layer file" "whiteout-ok" "${ovl[2]:-<none>}"
check "container ran to completion" "done" "${ovl[3]:-<none>}"

check "a container write lands in upperdir" "from-container" "$(cat "$WORK/ovl/diff/scratch.txt" 2>/dev/null)"
check "a container delete becomes a whiteout device in upperdir" "character special file" \
  "$(stat -c %F "$WORK/ovl/diff/etc/alpine-release" 2>/dev/null)"
check "the lower layer was not written to" "0" \
  "$(find "$LOWER" -name scratch.txt | wc -l)"
check "the deleted file still exists in the lower layer" "0" "$(
  test -f "$LOWER/base/etc/alpine-release"
  echo $?
)"
if mountpoint -q "$WORK/ovl/merged"; then
  no "the overlay mount does not leak onto the host" "not a mountpoint" "still mounted"
else
  ok "the overlay mount does not leak onto the host"
fi

echo
echo "overlay is read-only when no upperdir is given"
overlay_bundle "$WORK/ovlro" "$LOWER/top:$LOWER/base" \
  '.process.args = ["/bin/sh","-c","touch /nope 2>&1 || true"]'
jq 'del(.annotations["dev.mars.overlay.upperdir"], .annotations["dev.mars.overlay.workdir"])' \
  "$WORK/ovlro/config.json" >"$WORK/ovlro/c.tmp"
mv "$WORK/ovlro/c.tmp" "$WORK/ovlro/config.json"
ro_out=$(run_in "$WORK/ovlro" ovlro 2>&1)
check_error "writing to a read-only overlay fails inside the container" "read-only" "$ro_out"

echo
echo "overlay option string over one page, the reason overlay2 uses short symlinks"
PAGE="$WORK/page"
LONG="layer-with-a-deliberately-long-name-to-blow-past-the-single-page-mount-option-limit"
rm -rf "$PAGE"
mkdir -p "$PAGE"
for i in $(seq -w 1 40); do
  mkdir -p "$PAGE/$LONG-$i"
  echo "layer $i" >"$PAGE/$LONG-$i/f$i"
done
page_lower=""
for i in $(seq -w 40 -1 1); do page_lower+="$PAGE/$LONG-$i:"; done
page_lower+="$LOWER/base"
if ((${#page_lower} > 4096)); then
  ok "the absolute option string really is over 4096 bytes (${#page_lower})"
else
  no "the absolute option string is over 4096 bytes" "> 4096" "${#page_lower}"
fi

overlay_bundle "$WORK/ovlpage" "$page_lower" \
  '.process.args = ["/bin/sh","-c","cat /f01 /f40"]'
mapfile -t page < <(run_in "$WORK/ovlpage" ovlpage 2>/dev/null)
check "the bottom-most long-named layer is readable" "layer 01" "${page[0]:-<none>}"
check "the top-most long-named layer is readable" "layer 40" "${page[1]:-<none>}"

echo
echo "overlay misconfiguration is diagnosed by the runtime, not left to the kernel"
overlay_bundle "$WORK/ovlnest" "$LOWER/top:$LOWER/base" '.process.args = ["/bin/true"]'
jq '.annotations["dev.mars.overlay.workdir"] = "diff/work"' \
  "$WORK/ovlnest/config.json" >"$WORK/ovlnest/c.tmp"
mv "$WORK/ovlnest/c.tmp" "$WORK/ovlnest/config.json"
rejects "workdir nested inside upperdir is named as such" "$WORK/ovlnest" "inside upperdir"

overlay_bundle "$WORK/ovlxdev" "$LOWER/top:$LOWER/base" '.process.args = ["/bin/true"]'
mount -t tmpfs mars-xdev "$WORK/ovlxdev/work"
jq '.' "$WORK/ovlxdev/config.json" >/dev/null
rejects "workdir on another filesystem is named as such" "$WORK/ovlxdev" "same filesystem\|filesystem boundary"
umount "$WORK/ovlxdev/work"

overlay_bundle "$WORK/ovlone" "$LOWER/base" '.process.args = ["/bin/true"]'
jq 'del(.annotations["dev.mars.overlay.upperdir"], .annotations["dev.mars.overlay.workdir"])' \
  "$WORK/ovlone/config.json" >"$WORK/ovlone/c.tmp"
mv "$WORK/ovlone/c.tmp" "$WORK/ovlone/config.json"
rejects "a single-layer read-only overlay is named as such" "$WORK/ovlone" "at least 2 lower layers"

overlay_bundle "$WORK/ovlnons" "$LOWER/top:$LOWER/base" \
  '.process.args = ["/bin/true"] | .linux.namespaces = [{"type":"pid"}] | del(.hostname)'
rejects "an overlay without a mount namespace is refused" "$WORK/ovlnons" "mount namespace"

echo
echo "the create/start split: create parks the init, start releases it"
ROOT="$WORK/root"
rm -rf "$ROOT"
M=("$MARS" --root "$ROOT")
bundle "$WORK/lc" '.process.args = ["/bin/sh","-c","echo started > /ran.txt; sleep 30"]'

env -C "$WORK/lc" "${M[@]}" create lc
check "create returns without starting the process" "0" "$?"
check "state is created, not running" "created" "$(env -C "$WORK/lc" "${M[@]}" state lc | jq -r .status)"
check "the user process has not run yet" "1" "$(
  test -f "$WORK/lc/rootfs/ran.txt"
  echo $?
)"
if [[ -p "$ROOT/lc/exec.fifo" ]]; then
  ok "the exec fifo exists and is a fifo"
else
  no "the exec fifo exists" "a fifo" "$(ls -l "$ROOT/lc/exec.fifo" 2>&1)"
fi
check "state.json records the pid" "$(cat "$ROOT/lc/init.pid" 2>/dev/null || jq -r .pid "$ROOT/lc/state.json")" \
  "$(jq -r .pid "$ROOT/lc/state.json")"

"${M[@]}" start lc
sleep 0.4
check "state is running after start" "running" "$("${M[@]}" state lc | jq -r .status)"
check "the user process ran" "started" "$(cat "$WORK/lc/rootfs/ran.txt" 2>/dev/null)"
check "the exec fifo is gone, so start cannot run twice" "1" "$(
  test -e "$ROOT/lc/exec.fifo"
  echo $?
)"
"${M[@]}" start lc 2>/dev/null
check "starting an already-started container is refused" "1" "$?"

echo
echo "state is derived from the kernel, not from what the runtime wrote down"
init_pid=$(jq -r .pid "$ROOT/lc/state.json")
check "state reports the OCI shape" "1.0.2 lc running" \
  "$("${M[@]}" state lc | jq -r '"\(.ociVersion) \(.id) \(.status)"')"
check "list shows the container" "1" "$("${M[@]}" list -q | grep -c '^lc$')"
kill -KILL "$init_pid"
sleep 0.4
check "status becomes stopped once the pid is gone" "stopped" "$("${M[@]}" state lc | jq -r .status)"
check "a stopped container reports no pid" "null" "$("${M[@]}" state lc | jq -r '.pid // "null"')"

echo
echo "delete refuses a live container and cleans up a dead one"
"${M[@]}" delete lc
check "delete removes a stopped container" "0" "$?"
check "the state directory is gone" "1" "$(
  test -d "$ROOT/lc"
  echo $?
)"

bundle "$WORK/force" '.process.args = ["/bin/sleep","30"]'
env -C "$WORK/force" "${M[@]}" create force
"${M[@]}" start force
sleep 0.3
"${M[@]}" delete force 2>/dev/null
check "delete refuses a running container" "1" "$?"
"${M[@]}" delete --force force
check "delete --force kills it first" "0" "$?"
check "the cgroup is gone after delete" "1" "$(
  test -d /sys/fs/cgroup/mars/force
  echo $?
)"

echo
echo "kill takes signal names and numbers, and refuses a stopped container"
bundle "$WORK/sig" '.process.args = ["/bin/sh","-c","trap \"exit 42\" USR1; while true; do sleep 0.2; done"]'
env -C "$WORK/sig" "${M[@]}" create sig
"${M[@]}" start sig
sleep 0.4
"${M[@]}" kill sig USR1
check "kill by bare name works" "0" "$?"
sleep 0.5
check "the container acted on the signal" "stopped" "$("${M[@]}" state sig | jq -r .status)"
"${M[@]}" kill sig 9 2>/dev/null
check "kill on a stopped container is refused" "1" "$?"
"${M[@]}" delete sig

echo
echo "pause and resume use cgroup.freeze"
bundle "$WORK/frz" '.process.args = ["/bin/sleep","30"]'
env -C "$WORK/frz" "${M[@]}" create frz
"${M[@]}" start frz
sleep 0.3
"${M[@]}" pause frz
check "status is paused" "paused" "$("${M[@]}" state frz | jq -r .status)"
check "the kernel agrees the cgroup is frozen" "frozen 1" \
  "$(grep '^frozen' /sys/fs/cgroup/mars/frz/cgroup.events)"
"${M[@]}" resume frz
check "status is running again" "running" "$("${M[@]}" state frz | jq -r .status)"
"${M[@]}" delete --force frz

echo
echo "exec joins the running container's namespaces"
bundle "$WORK/ex" '.process.args = ["/bin/sleep","30"]'
env -C "$WORK/ex" "${M[@]}" create ex
"${M[@]}" start ex
sleep 0.4
init_pid=$("${M[@]}" state ex | jq -r .pid)

mapfile -t ex < <("${M[@]}" exec ex -- /bin/sh -c 'echo $$; hostname; cat /proc/self/cgroup; cat /etc/alpine-release')
check "the exec process is inside the container pid namespace" "2" "${ex[0]:-<none>}"
check "it sees the container hostname" "mars" "${ex[1]:-<none>}"
check "it sees the container cgroup as its root" "0::/" "${ex[2]:-<none>}"
check_prefix "it sees the container rootfs" "3.20" "${ex[3]:-<none>}"

check "the namespaces really are the container's" "0" "$(
  a=$(readlink "/proc/$init_pid/ns/mnt")
  b=$("${M[@]}" exec ex -- /bin/sh -c 'readlink /proc/self/ns/mnt')
  [[ "$a" == "$b" ]]
  echo $?
)"

"${M[@]}" exec ex -- /bin/sh -c 'exit 42'
check "the exec exit code propagates" "42" "$?"
check "--env and --cwd are honoured" "/etc bar" \
  "$("${M[@]}" exec ex -e FOO=bar --cwd /etc -- /bin/sh -c 'echo $(pwd) $FOO')"

rm -f "$WORK/ex.pid"
"${M[@]}" exec -d --pid-file "$WORK/ex.pid" ex -- /bin/sleep 5
sleep 0.4
exec_pid=$(cat "$WORK/ex.pid" 2>/dev/null)
check "the exec pid file lands on the host, not in the container" "0" "$(
  test -n "$exec_pid"
  echo $?
)"
check "the exec process joined the container cgroup" "1" \
  "$(grep -c "^${exec_pid:-none}$" /sys/fs/cgroup/mars/ex/cgroup.procs)"
check "the runtime itself did not join the cgroup" "2" \
  "$(wc -l < /sys/fs/cgroup/mars/ex/cgroup.procs)"

"${M[@]}" delete --force ex
"${M[@]}" exec ex -- /bin/true 2>/dev/null
check "exec into a deleted container is refused" "1" "$?"

echo
echo "ps and events read the cgroup"
bundle "$WORK/ps" '.process.args = ["/bin/sleep","30"]'
env -C "$WORK/ps" "${M[@]}" create ps1
"${M[@]}" start ps1
sleep 0.3
check "ps -f json lists the init pid" "1" \
  "$("${M[@]}" ps ps1 -f json | jq 'length')"
check "events --stats reports a pid count" "1" \
  "$("${M[@]}" events --stats ps1 | jq '.data.pids.current')"
"${M[@]}" delete --force ps1

echo
echo "process.user, rlimits and oomScoreAdj are applied"
bundle "$WORK/usr" '.process.args = ["/bin/sh","-c","id -u; id -g; ulimit -n; cat /proc/self/oom_score_adj"]
  | .process.user.uid = 405
  | .process.user.gid = 100
  | .process.rlimits = [{"type":"RLIMIT_NOFILE","hard":512,"soft":512}]
  | .process.oomScoreAdj = 123'
mapfile -t usr < <(run_in "$WORK/usr" usr)
check "uid is the one the spec asked for" "405" "${usr[0]:-<none>}"
check "gid is the one the spec asked for" "100" "${usr[1]:-<none>}"
check "RLIMIT_NOFILE was applied" "512" "${usr[2]:-<none>}"
check "oom_score_adj was applied" "123" "${usr[3]:-<none>}"

echo
echo "capabilities are reduced to the set in the spec"
bundle "$WORK/cap" '.process.args = ["/bin/sh","-c","grep CapEff /proc/self/status; grep CapBnd /proc/self/status"]
  | .process.capabilities.bounding = ["CAP_KILL","CAP_CHOWN"]
  | .process.capabilities.effective = ["CAP_KILL"]
  | .process.capabilities.permitted = ["CAP_KILL"]
  | .process.capabilities.inheritable = []
  | .process.capabilities.ambient = []'
mapfile -t cap < <(run_in "$WORK/cap" cap)
check "the effective set is exactly CAP_KILL" "CapEff:	0000000000000020" "${cap[0]:-<none>}"
check "the bounding set is exactly CAP_KILL and CAP_CHOWN" "CapBnd:	0000000000000021" "${cap[1]:-<none>}"

echo
echo "the five lifecycle hooks run where the spec says they do"
HOOK="$WORK/hook.sh"
cat >"$HOOK" <<'HOOKEOF'
#!/bin/sh
state=$(cat)
id=$(echo "$state" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
status=$(echo "$state" | sed -n 's/.*"status":"\([^"]*\)".*/\1/p')
printf '%s id=%s status=%s uts=%s alpine=%s\n' \
  "$1" "$id" "$status" "$(readlink /proc/self/ns/uts)" \
  "$(test -f /etc/alpine-release && echo yes || echo no)" >>"$HOOKLOG"
HOOKEOF
chmod +x "$HOOK"
HOOKLOG="$WORK/hooks.log"
export HOOKLOG
rm -f "$HOOKLOG"

bundle "$WORK/hk" '.process.args = ["/bin/true"]'
jq --arg hook "$HOOK" --arg log "$HOOKLOG" \
  '.hooks.createRuntime   = [{"path":$hook,"args":["h","createRuntime"],  "env":["HOOKLOG=\($log)"]}]
   | .hooks.createContainer = [{"path":$hook,"args":["h","createContainer"],"env":["HOOKLOG=\($log)"]}]
   | .hooks.poststart       = [{"path":$hook,"args":["h","poststart"],      "env":["HOOKLOG=\($log)"]}]
   | .hooks.poststop        = [{"path":$hook,"args":["h","poststop"],       "env":["HOOKLOG=\($log)"]}]
   | .hooks.startContainer  = [{"path":"/bin/sh","args":["sh","-c","cat /etc/alpine-release > /startContainer.txt"]}]' \
  "$WORK/hk/config.json" >"$WORK/hk/c.tmp"
mv "$WORK/hk/c.tmp" "$WORK/hk/config.json"

HOST_UTS=$(readlink /proc/self/ns/uts)
run_in "$WORK/hk" hk
check "the container ran with hooks configured" "0" "$?"

check "four host-side hooks ran, in order" "createRuntime createContainer poststart poststop" \
  "$(awk '{print $1}' "$HOOKLOG" | tr '\n' ' ' | sed 's/ $//')"
check "createRuntime runs in the runtime's UTS namespace" "uts=$HOST_UTS" \
  "$(awk '/^createRuntime/ {print $4}' "$HOOKLOG")"
if [[ "$(awk '/^createContainer/ {print $4}' "$HOOKLOG")" != "uts=$HOST_UTS" ]]; then
  ok "createContainer runs in the container's UTS namespace"
else
  no "createContainer runs in the container's namespaces" "a different uts ns" "$HOST_UTS"
fi
check "createContainer runs before pivot_root, so the rootfs is still the host's" "alpine=no" \
  "$(awk '/^createContainer/ {print $5}' "$HOOKLOG")"
check_prefix "startContainer runs after pivot_root, inside the container rootfs" "3.20" \
  "$(cat "$WORK/hk/rootfs/startContainer.txt" 2>/dev/null)"
check "poststart sees the container running" "status=running" \
  "$(awk '/^poststart/ {print $3}' "$HOOKLOG")"
check "poststop sees it stopped" "status=stopped" \
  "$(awk '/^poststop/ {print $3}' "$HOOKLOG")"

bundle "$WORK/hkfail" '.process.args = ["/bin/true"]'
jq --arg hook /bin/false '.hooks.createRuntime = [{"path":$hook,"args":["false"]}]' \
  "$WORK/hkfail/config.json" >"$WORK/hkfail/c.tmp"
mv "$WORK/hkfail/c.tmp" "$WORK/hkfail/config.json"
rejects "a failing createRuntime hook aborts create" "$WORK/hkfail" "createRuntime hook 0"

bundle "$WORK/hkpost" '.process.args = ["/bin/true"]'
jq --arg hook /bin/false '.hooks.poststop = [{"path":$hook,"args":["false"]}]' \
  "$WORK/hkpost/config.json" >"$WORK/hkpost/c.tmp"
mv "$WORK/hkpost/c.tmp" "$WORK/hkpost/config.json"
run_in "$WORK/hkpost" hkpost >/dev/null 2>&1
check "a failing poststop hook does not fail the operation" "0" "$?"

echo
echo "a console socket receives the pty master over SCM_RIGHTS"
if command -v python3 >/dev/null; then
  bundle "$WORK/tty" '.process.args = ["/bin/sh","-c","tty; test -t 1 && echo stdout-is-a-tty"]'
  jq '.process.terminal = true' "$WORK/tty/config.json" >"$WORK/tty/c.tmp"
  mv "$WORK/tty/c.tmp" "$WORK/tty/config.json"

  rm -f "$WORK/tty.out"
  python3 "$(dirname "$0")/recvtty.py" "$WORK/console.sock" "$WORK/tty.out" >/dev/null 2>&1 &
  receiver=$!
  sleep 1
  env -C "$WORK/tty" "${M[@]}" run -d --console-socket "$WORK/console.sock" ttytest >/dev/null 2>&1
  sleep 1.5
  wait "$receiver" 2>/dev/null
  tty_out=$(tr -d '\r' <"$WORK/tty.out" 2>/dev/null)
  check "the container's stdin names a pty inside its own devpts" "/dev/pts/0" \
    "$(sed -n 1p <<<"$tty_out")"
  check "stdout is a terminal" "stdout-is-a-tty" "$(sed -n 2p <<<"$tty_out")"
  "${M[@]}" delete --force ttytest 2>/dev/null
else
  no "console socket test" "python3" "not installed"
fi

echo
echo "a create that cannot work fails at create and leaves nothing behind"
bundle "$WORK/nobin" '.process.args = ["/nonexistent-binary"]'
before=$(find /sys/fs/cgroup/mars -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
out=$(env -C "$WORK/nobin" "${M[@]}" create nobin 2>&1)
status=$?
check "create fails rather than deferring the error to start" "1" "$status"
check_error "the missing executable is named" "not found in the container PATH" "$out"
check "no cgroup is left behind" "$before" \
  "$(find /sys/fs/cgroup/mars -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
check "no state directory is left behind" "1" "$(
  test -d "$ROOT/nobin"
  echo $?
)"

echo
echo "read-only rootfs, maskedPaths and readonlyPaths"
bundle "$WORK/ro" '.process.args = ["/bin/sh","-c","touch /nope 2>&1 | head -1; cat /proc/kcore | head -c 8; echo kcore-empty; echo x > /proc/sys/kernel/hostname 2>&1 | head -1"]'
jq '.root.readonly = true' "$WORK/ro/config.json" >"$WORK/ro/c.tmp"
mv "$WORK/ro/c.tmp" "$WORK/ro/config.json"
mapfile -t ro < <(run_in "$WORK/ro" ro 2>&1)
check_error "the rootfs is read-only" "read-only file system" "${ro[0]:-<none>}"
check "a masked path reads as empty" "kcore-empty" "${ro[1]:-<none>}"
check_error "a readonly path cannot be written" "read-only file system" "${ro[2]:-<none>}"

bundle "$WORK/rw" '.process.args = ["/bin/sh","-c","touch /yes && echo writable"]'
check "the rootfs is writable when the spec says so" "writable" "$(run_in "$WORK/rw" rw)"

echo
echo "seccomp gives each rule its own errno"
bundle "$WORK/sec" '.process.args = ["/bin/sh","-c","chmod 700 /tmp 2>&1 | head -1; mkdir /tmp/d 2>&1 | head -1"]
  | .linux.seccomp = {"defaultAction":"SCMP_ACT_ALLOW",
      "syscalls":[{"names":["chmod","fchmodat"],"action":"SCMP_ACT_ERRNO","errnoRet":1},
                  {"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO","errnoRet":38}]}'
mapfile -t sec < <(run_in "$WORK/sec" sec 2>&1)
check_error "the first rule returns its own errno (EPERM)" "operation not permitted" "${sec[0]:-<none>}"
check_error "the second rule returns a different errno (ENOSYS)" "not implemented" "${sec[1]:-<none>}"

bundle "$WORK/secnnp" '.process.args = ["/bin/sh","-c","grep NoNewPrivs /proc/self/status"]
  | .process.noNewPrivileges = false
  | .linux.seccomp = {"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mount"],"action":"SCMP_ACT_ERRNO"}]}'
check "loading a filter does not turn on no_new_privs behind the spec's back" "NoNewPrivs:	0"   "$(run_in "$WORK/secnnp" secnnp)"

echo
echo "a user namespace with id mappings"
HOST_USERNS=$(readlink /proc/self/ns/user)
bundle "$WORK/uns" '.process.args = ["/bin/sh","-c","id -u; cat /proc/self/uid_map; readlink /proc/self/ns/user; echo x > /dev/null && echo devnull-works"]
  | .linux.namespaces += [{"type":"user"}]
  | .linux.uidMappings = [{"containerID":0,"hostID":100000,"size":65536}]
  | .linux.gidMappings = [{"containerID":0,"hostID":100000,"size":65536}]'
mapfile -t uns < <(run_in "$WORK/uns" uns 2>&1)
check "the process is root inside the namespace" "0" "${uns[0]:-<none>}"
check "the mapping the spec asked for is in place" "         0     100000      65536" "${uns[1]:-<none>}"
if [[ "${uns[2]:-}" != "$HOST_USERNS" && -n "${uns[2]:-}" ]]; then
  ok "the user namespace differs from the host's (${uns[2]})"
else
  no "the user namespace differs from the host's" "not $HOST_USERNS" "${uns[2]:-<none>}"
fi
check "device nodes work, bind-mounted because mknod is refused in a user namespace" "devnull-works" "${uns[3]:-<none>}"

bundle "$WORK/unsbad" '.process.args = ["/bin/true"]
  | .linux.namespaces += [{"type":"user"}]
  | .linux.uidMappings = [{"containerID":0,"hostID":100000,"size":1}]'
rejects "a user namespace with only a uid map is refused" "$WORK/unsbad" "gidMappings"

echo
echo "the time namespace, which docker asks for by default"
bundle "$WORK/tns" '.process.args = ["/bin/sh","-c","readlink /proc/self/ns/time"]
  | .linux.namespaces += [{"type":"time"}]'
host_time_ns=$(readlink /proc/self/ns/time)
guest_time_ns=$(run_in "$WORK/tns" tns 2>&1)
if [[ -n "$guest_time_ns" && "$guest_time_ns" != "$host_time_ns" ]]; then
  ok "a time namespace is created ($guest_time_ns)"
else
  no "a time namespace is created" "not $host_time_ns" "${guest_time_ns:-<none>}"
fi

echo
echo "delete --force is idempotent, because containerd calls it during cleanup"
"${M[@]}" delete --force never-existed
check "deleting a container that was never created succeeds" "0" "$?"
"${M[@]}" delete never-existed 2>/dev/null
check "deleting it without --force still reports it missing" "1" "$?"

echo
echo "the startup trace is valid OTLP that a collector accepts"
if command -v python3 >/dev/null; then
  bundle "$WORK/otlp" '.process.args = ["/bin/true"]'
  rm -f "$WORK/otlp.out"
  python3 "$(dirname "$0")/otlp-capture.py" 4319 "$WORK/otlp.out" &
  receiver=$!
  sleep 1
  MARS_OTLP_ENDPOINT=127.0.0.1:4319 run_in "$WORK/otlp" otlp >/dev/null 2>&1
  wait "$receiver" 2>/dev/null
  if [[ -s "$WORK/otlp.out" ]]; then
    span_count=$(sed -n 1p "$WORK/otlp.out")
    trace_id=$(sed -n 2p "$WORK/otlp.out")
    names=$(sed -n 3p "$WORK/otlp.out")
    check "the collector received a parseable OTLP document" "0" "$(
      [[ "$span_count" -gt 5 ]]
      echo $?
    )"
    check "the trace id is 16 bytes of hex" "32" "${#trace_id}"
    check_error "the parent measures the fork" "wait.initpid" "$names"
    check_error "the child's own phases are folded in" "init.pivot_root" "$names"
    check_error "each namespace unshare is timed separately" "intermediate.unshare.net" "$names"
  else
    no "the collector received an OTLP document" "a payload" "nothing"
  fi
else
  no "OTLP export test" "python3" "not installed"
fi

echo
echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
