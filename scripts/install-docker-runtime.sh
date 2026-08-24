#!/usr/bin/env bash
set -euo pipefail

BINARY="${BINARY:-/usr/local/bin/mars}"
SOURCE="${SOURCE:-/var/tmp/mars-target/debug/mars}"
DAEMON_JSON="${DAEMON_JSON:-/etc/docker/daemon.json}"
TRACE="${TRACE:-0}"

if [[ "$EUID" -ne 0 ]]; then
  echo "error: writing $DAEMON_JSON and restarting docker needs root; use sudo" >&2
  exit 1
fi

command -v jq >/dev/null || {
  echo "error: jq is required" >&2
  exit 1
}

[[ -x "$SOURCE" ]] || {
  echo "error: $SOURCE not found; run cargo build first" >&2
  exit 1
}

rm -f "$BINARY"
cp "$SOURCE" "$BINARY"
echo "installed $BINARY"

RUNTIME_PATH="$BINARY"

if [[ "$TRACE" == "1" ]]; then
  RUNTIME_PATH=/usr/local/bin/mars-trace
  cat >"$RUNTIME_PATH" <<'EOF'
#!/bin/sh
log=/var/tmp/mars-docker.log
printf '=== %s cwd=%s\nargv: %s %s\n' "$(date -Ins)" "$(pwd)" "$0" "$*" >>"$log"
/usr/local/bin/mars "$@" 2>>"$log"
rc=$?
echo "exit=$rc" >>"$log"
exit $rc
EOF
  chmod +x "$RUNTIME_PATH"
  echo "installed $RUNTIME_PATH; every invocation and its stderr goes to /var/tmp/mars-docker.log"
fi

mkdir -p "$(dirname "$DAEMON_JSON")"
[[ -f "$DAEMON_JSON" ]] || echo '{}' >"$DAEMON_JSON"

jq --arg path "$RUNTIME_PATH" '
  .runtimes.mars.path = $path
  | .["exec-opts"] = ((.["exec-opts"] // [])
      - ["native.cgroupdriver=systemd"]
      + ["native.cgroupdriver=cgroupfs"] | unique)
' "$DAEMON_JSON" >"$DAEMON_JSON.new"
mv "$DAEMON_JSON.new" "$DAEMON_JSON"

cat "$DAEMON_JSON"

systemctl restart docker
echo "waiting for docker"
for _ in $(seq 60); do
  docker info >/dev/null 2>&1 && break
  sleep 1
done

docker info 2>/dev/null | grep -E "Cgroup Driver|Runtimes"

cat <<EOF

mars is registered. The cgroup driver had to change: docker defaults to systemd,
which delegates cgroup creation to systemd via dbus, and mars writes to cgroupfs
directly. Leaving it on systemd makes docker and mars disagree about who owns
/sys/fs/cgroup/<container>.

Try it:
  docker run --rm --runtime=mars alpine:3.20 echo ok
  docker run --rm -it --runtime=mars alpine:3.20 sh
  docker run --rm --runtime=mars --memory=32m alpine:3.20 cat /sys/fs/cgroup/memory.max
EOF
