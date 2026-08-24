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
echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
