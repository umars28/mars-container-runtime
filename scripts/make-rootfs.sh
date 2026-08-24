#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:-alpine:3.20}"
OUT="${1:-rootfs}"
TMP_NAME="mars-rootfs-$$"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: run this inside the mars-dev VM, not on macOS" >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker is required to export a rootfs" >&2
  exit 1
fi

DOCKER=(docker)
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER=(sudo docker)
  else
    echo "error: cannot reach the docker daemon as $(id -un) or via sudo" >&2
    exit 1
  fi
fi

if [[ -e "$OUT" ]]; then
  echo "error: $OUT already exists, remove it first" >&2
  exit 1
fi

cleanup() {
  "${DOCKER[@]}" rm -f "$TMP_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

"${DOCKER[@]}" pull -q "$IMAGE"
"${DOCKER[@]}" create --name "$TMP_NAME" "$IMAGE" /bin/true >/dev/null

mkdir -p "$OUT"
"${DOCKER[@]}" export "$TMP_NAME" | tar -C "$OUT" -xf -

echo "rootfs ready: $OUT ($(du -sh "$OUT" | cut -f1), $(find "$OUT" -type f | wc -l) files)"
echo "next: mars spec --bundle $(dirname "$(readlink -f "$OUT")")"
