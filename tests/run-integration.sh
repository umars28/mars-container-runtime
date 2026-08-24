#!/usr/bin/env bash
set -uo pipefail

MARS="${MARS:-/var/tmp/mars-target/debug/mars}"
WORK="${WORK:-/tmp/mars-it}"
IMAGE="${IMAGE:-alpine:3.20}"

PASS=0
FAIL=0

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: run this inside the mars-dev VM" >&2
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
  jq "$jq_filter | .process.terminal = false" "$dir/config.json" >"$dir/c.tmp"
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

  if [[ ! -d /sys/fs/cgroup/mars ]]; then
    ok "cgroup tree removed when the container exits"
  else
    no "cgroup tree removed when the container exits" "gone" "$(ls /sys/fs/cgroup/mars)"
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
echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
