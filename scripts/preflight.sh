#!/usr/bin/env bash
set -uo pipefail

# Checks whether this host can actually run mars. Safe to run as a normal user,
# though a few checks need root to be conclusive.

PASS=0
WARN=0
FAIL=0

ok() {
  PASS=$((PASS + 1))
  printf '  ok    %s\n' "$1"
}
warn() {
  WARN=$((WARN + 1))
  printf '  warn  %s\n' "$1"
  printf '        %s\n' "$2"
}
no() {
  FAIL=$((FAIL + 1))
  printf '  FAIL  %s\n' "$1"
  printf '        %s\n' "$2"
}

echo "host"
echo "  $(uname -srm)"
[[ -r /etc/os-release ]] && echo "  $(. /etc/os-release; echo "$PRETTY_NAME")"
echo

echo "can this machine create namespaces at all?"
if [[ "$(uname -s)" != Linux ]]; then
  no "running on Linux" "namespaces and cgroups do not exist on $(uname -s)"
else
  ok "running on Linux"
fi

# systemd-detect-virt prints "none" *and* exits non-zero when there is nothing to report,
# so the output has to be taken on its own and the exit status ignored.
virt=$(systemd-detect-virt 2>/dev/null | head -1)
container=$(systemd-detect-virt -c 2>/dev/null | head -1)
virt=${virt:-unknown}
container=${container:-none}

if [[ "$container" != none ]]; then
  no "not already inside a container ($container)" \
    "OpenVZ, LXC and most 'cheap VPS' plans share the host kernel and block pivot_root and
        cgroup delegation. mars needs a KVM/Xen VM or bare metal. Ask the provider for KVM."
else
  ok "not inside a container (virtualisation: $virt)"
fi

if unshare --mount --pid --fork true 2>/dev/null; then
  ok "unshare(CLONE_NEWNS|CLONE_NEWPID) works here"
elif [[ "$EUID" -ne 0 ]]; then
  warn "unshare needs privilege as this user" "re-run with sudo to be sure"
else
  no "unshare(CLONE_NEWNS|CLONE_NEWPID) works" \
    "the kernel refused. Check for a seccomp/AppArmor policy on the host, or a provider
        that blocks namespaces."
fi
echo

echo "cgroup v2"
fstype=$(stat -fc %T /sys/fs/cgroup 2>/dev/null || echo missing)
if [[ "$fstype" == cgroup2fs ]]; then
  ok "/sys/fs/cgroup is a pure cgroup v2 hierarchy"
else
  no "/sys/fs/cgroup is cgroup2fs (found: $fstype)" \
    "mars has no cgroup v1 driver. On a hybrid host, boot with
        systemd.unified_cgroup_hierarchy=1 and reboot."
fi

if [[ -r /sys/fs/cgroup/cgroup.controllers ]]; then
  available=$(cat /sys/fs/cgroup/cgroup.controllers)
  echo "        available: $available"
  missing=""
  for c in memory cpu pids; do
    grep -qw "$c" <<<"$available" || missing="$missing $c"
  done
  if [[ -z "$missing" ]]; then
    ok "the controllers mars writes are compiled in"
  else
    no "controllers$missing are available" \
      "resource limits for those will fail. This is a kernel build or a nested-cgroup problem."
  fi

  subtree=$(cat /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || echo "")
  echo "        delegated: ${subtree:-<none>}"
  if [[ -n "$subtree" ]]; then
    ok "the root cgroup delegates controllers to children"
  else
    warn "the root cgroup delegates nothing yet" \
      "mars enables what it needs on the way down, which needs root. Fine if you run it as root."
  fi
fi
echo

echo "filesystem"
if grep -qw overlay /proc/filesystems; then
  ok "overlayfs is available"
else
  warn "overlayfs is available" "the overlay rootfs feature will not work; a plain rootfs still will"
fi

if command -v setfattr >/dev/null; then
  ok "setfattr present (needed by scripts/oci-bundle.sh for opaque whiteouts)"
else
  warn "setfattr present" "apt install attr — only needed to build layered test bundles"
fi
echo

echo "user namespaces, for the rootless path"
max_userns=$(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo 0)
if [[ "$max_userns" -gt 0 ]]; then
  ok "user namespaces are permitted (max_user_namespaces=$max_userns)"
else
  warn "user namespaces are permitted" \
    "max_user_namespaces=0. Everything except the user-namespace tests still works."
fi

if [[ -r /etc/subuid ]] && grep -q "^$(id -un):" /etc/subuid 2>/dev/null; then
  ok "/etc/subuid has a range for $(id -un): $(grep "^$(id -un):" /etc/subuid)"
else
  warn "/etc/subuid has a range for $(id -un)" \
    "needed only for multi-range id mappings via newuidmap. usermod --add-subuids 100000-165535 $(id -un)"
fi
echo

echo "build dependencies"
for tool in cargo rustc; do
  if command -v "$tool" >/dev/null; then
    ok "$tool ($($tool --version 2>/dev/null | head -1))"
  else
    no "$tool present" "install via https://rustup.rs — the distro package is often too old"
  fi
done

if pkg-config --exists libseccomp 2>/dev/null; then
  ok "libseccomp headers ($(pkg-config --modversion libseccomp))"
else
  no "libseccomp headers present" "apt install libseccomp-dev pkg-config"
fi

for tool in jq; do
  command -v "$tool" >/dev/null && ok "$tool present" ||
    warn "$tool present" "apt install $tool — needed by the test suite and scripts"
done
echo

echo "optional, for the Docker drop-in"
if command -v docker >/dev/null; then
  driver=$(docker info --format '{{.CgroupDriver}}' 2>/dev/null | head -1)
  [[ -z "$driver" ]] && driver=$(sudo -n docker info --format '{{.CgroupDriver}}' 2>/dev/null | head -1)
  driver=${driver:-unknown}
  if [[ "$driver" == cgroupfs ]]; then
    ok "docker present, cgroup driver is cgroupfs"
  elif [[ "$driver" == systemd ]]; then
    warn "docker's cgroup driver is systemd" \
      "run scripts/install-docker-runtime.sh, which switches it to cgroupfs"
  else
    warn "docker present but its cgroup driver could not be read" "try again with sudo"
  fi
else
  warn "docker present" "only needed for the drop-in demo and to build test rootfs images"
fi

echo
printf '%d ok, %d warnings, %d blocking\n' "$PASS" "$WARN" "$FAIL"

if [[ "$FAIL" -gt 0 ]]; then
  echo
  echo "Blocking problems above must be fixed first; mars will not start otherwise."
  exit 1
fi

echo "This host can run mars. Warnings only limit optional features."
