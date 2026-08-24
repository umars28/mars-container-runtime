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
echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
