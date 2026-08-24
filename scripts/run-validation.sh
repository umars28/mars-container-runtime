#!/usr/bin/env bash
set -uo pipefail

TOOLS="${TOOLS:-/var/tmp/runtime-tools}"
RUNTIME="${RUNTIME:-mars}"
ONLY="${1:-}"

if [[ ! -d "$TOOLS" ]]; then
  cat >&2 <<EOF
error: $TOOLS not found. Set it up with:

  git clone --depth 1 https://github.com/opencontainers/runtime-tools.git $TOOLS
  cd $TOOLS && make runtimetest validation-executables
  tar czf rootfs-\$(go env GOARCH).tar.gz -C <a rootfs> .
EOF
  exit 1
fi

if [[ "$EUID" -ne 0 ]]; then
  echo "error: the validation suite creates containers; use sudo -E" >&2
  exit 1
fi

command -v "$RUNTIME" >/dev/null || {
  echo "error: $RUNTIME is not on PATH; install the built binary first" >&2
  exit 1
}

cd "$TOOLS" || exit 1

ARCH=$(go env GOARCH 2>/dev/null || echo unknown)
[[ -f "rootfs-$ARCH.tar.gz" ]] || {
  echo "error: rootfs-$ARCH.tar.gz missing in $TOOLS" >&2
  exit 1
}

mapfile -t TESTS < <(find ./validation -name '*.t' | sort)
[[ ${#TESTS[@]} -gt 0 ]] || {
  echo "error: no validation executables built in $TOOLS" >&2
  exit 1
}

OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

declare -a GREEN=() RED=() EMPTY=()

for test in "${TESTS[@]}"; do
  name=$(basename "$test" .t)
  [[ -n "$ONLY" && "$name" != *"$ONLY"* ]] && continue

  log="$OUT/$name.tap"
  timeout 60 env RUNTIME="$RUNTIME" "$test" >"$log" 2>&1
  rc=$?

  assertions=$(grep -cE '^(ok|not ok) ' "$log")
  failed=$(grep -cE '^not ok ' "$log")

  if [[ "$assertions" -eq 0 ]]; then
    EMPTY+=("$name")
    printf '  ????  %-44s no assertions (rc=%s)\n' "$name" "$rc"
  elif [[ "$failed" -eq 0 && "$rc" -eq 0 ]]; then
    GREEN+=("$name")
    printf '  pass  %-44s %s/%s\n' "$name" "$assertions" "$assertions"
  else
    RED+=("$name")
    printf '  FAIL  %-44s %s/%s failed (rc=%s)\n' "$name" "$failed" "$assertions" "$rc"
    grep -E '^not ok ' "$log" | sed 's/^/          /' | head -4
  fi
done

echo
printf 'runtime-tools %s against %s: %d passed, %d failed, %d inconclusive\n' \
  "$(cat VERSION)" "$RUNTIME" "${#GREEN[@]}" "${#RED[@]}" "${#EMPTY[@]}"

if [[ ${#RED[@]} -gt 0 ]]; then
  echo
  echo "failing:"
  printf '  %s\n' "${RED[@]}"
fi

if [[ ${#EMPTY[@]} -gt 0 ]]; then
  echo
  echo "inconclusive (the test itself did not run):"
  printf '  %s\n' "${EMPTY[@]}"
fi

[[ ${#RED[@]} -eq 0 ]]
