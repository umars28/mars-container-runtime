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

if [[ -e "$OUT" ]]; then
  echo "error: $OUT already exists, remove it first" >&2
  exit 1
fi

cleanup() {
  docker rm -f "$TMP_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker pull -q "$IMAGE"
docker create --name "$TMP_NAME" "$IMAGE" /bin/true >/dev/null

mkdir -p "$OUT"
docker export "$TMP_NAME" | tar -C "$OUT" -xf -

echo "rootfs ready: $OUT ($(du -sh "$OUT" | cut -f1), $(find "$OUT" -type f | wc -l) files)"
echo "next: mars spec --bundle $(dirname "$(readlink -f "$OUT")")"
