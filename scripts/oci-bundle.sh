#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: oci-bundle.sh [-i IMAGE] [-F] BUNDLE_DIR

Lay out an OCI bundle whose rootfs is an overlay assembled from the image's
own layers, with process/env/cwd taken from the image config.

  -i IMAGE  docker image to unpack (default: alpine:3.20)
  -F        build a throwaway multi-layer fixture image first, so the bundle
            has several lower layers and a real whiteout to look at

Layout produced:
  BUNDLE_DIR/config.json
  BUNDLE_DIR/layers/00..NN   one directory per image layer, base first
  BUNDLE_DIR/diff            upperdir, empty until the container writes
  BUNDLE_DIR/work            overlayfs scratch space
  BUNDLE_DIR/merged          root.path, the overlay mountpoint
EOF
  exit 2
}

IMAGE="${IMAGE:-alpine:3.20}"
FIXTURE=0

while getopts ":i:F" opt; do
  case "$opt" in
    i) IMAGE=$OPTARG ;;
    F) FIXTURE=1 ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))

[[ $# -eq 1 ]] || usage
BUNDLE=$1

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: this needs Linux" >&2
  exit 1
fi

for tool in docker jq tar; do
  command -v "$tool" >/dev/null || {
    echo "error: $tool is required" >&2
    exit 1
  }
done

DOCKER=(docker)
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER=(sudo docker)
  else
    echo "error: cannot reach the docker daemon as $(id -un) or via sudo" >&2
    exit 1
  fi
fi

MARS="${MARS:-/var/tmp/mars-target/debug/mars}"
[[ -x "$MARS" ]] || {
  echo "error: $MARS not found; run cargo build first" >&2
  exit 1
}

if [[ "$EUID" -ne 0 ]]; then
  echo "error: needs root to mknod overlayfs whiteouts; use sudo -E" >&2
  exit 1
fi

WHITEOUTS=1
command -v setfattr >/dev/null || {
  echo "warning: setfattr missing (apt install attr); opaque directories will be skipped" >&2
  WHITEOUTS=0
}

TMP=$(mktemp -d /var/tmp/mars-bundle.XXXXXX)
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

if [[ "$FIXTURE" -eq 1 ]]; then
  IMAGE="mars-fixture:multi-layer"
  echo "building fixture image $IMAGE"
  cat >"$TMP/Dockerfile" <<'EOF'
FROM alpine:3.20
RUN echo "written by layer 2" >/layer2.txt
RUN rm -f /etc/motd && mkdir -p /gone && echo x >/gone/file
RUN rm -rf /gone && echo "written by layer 4" >/layer4.txt
EOF
  "${DOCKER[@]}" build -q -t "$IMAGE" "$TMP" >/dev/null
else
  "${DOCKER[@]}" pull -q "$IMAGE" >/dev/null
fi

echo "unpacking $IMAGE"
"${DOCKER[@]}" save "$IMAGE" -o "$TMP/image.tar"
mkdir -p "$TMP/image"
tar -C "$TMP/image" -xf "$TMP/image.tar"

MANIFEST="$TMP/image/manifest.json"
[[ -f "$MANIFEST" ]] || {
  echo "error: no manifest.json in the saved image" >&2
  exit 1
}

mapfile -t LAYERS < <(jq -r '.[0].Layers[]' "$MANIFEST")
CONFIG_BLOB="$TMP/image/$(jq -r '.[0].Config' "$MANIFEST")"

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/layers" "$BUNDLE/diff" "$BUNDLE/work" "$BUNDLE/merged"

index=0
for layer in "${LAYERS[@]}"; do
  dir=$(printf '%s/layers/%02d' "$BUNDLE" "$index")
  mkdir -p "$dir"
  tar -C "$dir" -xf "$TMP/image/$layer"

  converted=0
  while IFS= read -r -d '' marker; do
    parent=$(dirname "$marker")
    name=$(basename "$marker")
    rm -f "$marker"

    if [[ "$name" == ".wh..wh..opq" ]]; then
      [[ "$WHITEOUTS" -eq 1 ]] && setfattr -n trusted.overlay.opaque -v y "$parent"
    else
      mknod "$parent/${name#.wh.}" c 0 0
    fi
    converted=$((converted + 1))
  done < <(find "$dir" -name '.wh.*' -print0)

  printf '  layers/%02d  %6s  %s\n' \
    "$index" "$(du -sh "$dir" | cut -f1)" \
    "$([[ $converted -gt 0 ]] && echo "$converted whiteout(s) converted" || echo "")"
  index=$((index + 1))
done

IMAGE_CONFIG=$(jq '.config // .Config // {}' "$CONFIG_BLOB")

ARGS=$(jq -c '((.Entrypoint // []) + (.Cmd // [])) | if length == 0 then ["/bin/sh"] else . end' <<<"$IMAGE_CONFIG")
ENV=$(jq -c '.Env // ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]' <<<"$IMAGE_CONFIG")
CWD=$(jq -r 'if (.WorkingDir // "") == "" then "/" else .WorkingDir end' <<<"$IMAGE_CONFIG")

LOWER=""
for ((i = index - 1; i >= 0; i--)); do
  LOWER+="$(printf 'layers/%02d' "$i")"
  ((i > 0)) && LOWER+=":"
done

"$MARS" spec --bundle "$BUNDLE"

jq \
  --argjson args "$ARGS" \
  --argjson env "$ENV" \
  --arg cwd "$CWD" \
  --arg lower "$LOWER" \
  '.root.path = "merged"
   | .process.args = $args
   | .process.env = $env
   | .process.cwd = $cwd
   | .process.terminal = false
   | .annotations["dev.mars.overlay.lowerdir"] = $lower
   | .annotations["dev.mars.overlay.upperdir"] = "diff"
   | .annotations["dev.mars.overlay.workdir"] = "work"' \
  "$BUNDLE/config.json" >"$BUNDLE/config.json.tmp"
mv "$BUNDLE/config.json.tmp" "$BUNDLE/config.json"

echo
echo "bundle ready: $BUNDLE"
echo "  layers   $index (lowerdir is written topmost-first: $LOWER)"
echo "  argv     $ARGS"
echo "  cwd      $CWD"
echo
echo "run it:   (cd $BUNDLE && $MARS run demo)"
echo "inspect:  find $BUNDLE/diff -mindepth 1"
