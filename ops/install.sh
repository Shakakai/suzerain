#!/usr/bin/env bash
# Suzerain one-line installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Shakakai/suzerain/main/ops/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- suz                   # one component
#   curl -fsSL .../install.sh | bash -s -- --version v0.1.3 --no-service suzerain
#   curl -fsSL .../install.sh | bash -s -- --control-only        # reduced deps
#
# Components: suzerain suz suzerain-mcp (default: all). There is no
# separate `castellan` binary — `suzerain` is the one binary for every
# fleet role (docs/UNIFIED-AGENT-API-DESIGN.md §6 step 2); the Gondolin
# driver ships inside suzerain's own release archive.
# Resolves the latest GitHub release (including prereleases) unless --version
# is given, downloads the per-component archives for this platform, verifies
# SHA256 checksums, installs binaries to ~/.local/bin, the gondolin driver to
# ~/.local/share/suzerain/driver (the shared fleet home), and (unless
# --no-service) enables a systemd user service (Linux) or launchd agent
# (macOS) for suzerain.
#
# Install modes (docs/UNIFIED-AGENT-API-DESIGN.md §4.1.1): `suzerain run`
# defaults to `standalone` mode (control plane + co-located agent hosting)
# and also supports `--mode agent`/`--mode control`. Standalone/agent modes
# need the Gondolin runtime (node, qemu, the driver bundle) to host agent
# VMs; `control` mode never hosts a VM locally and needs none of that.
# Plain `install.sh` is the **full** path (installs the runtime).
# `--control-only` is the **reduced-deps** path: skips node/qemu/KVM checks
# and the driver bundle entirely, for a dedicated registry-only host that
# will only ever run `suzerain run --mode control`.
set -euo pipefail

REPO="Shakakai/suzerain"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
SUZERAIN_HOME="${SUZERAIN_HOME:-$HOME/.local/share/suzerain}"
VERSION=""
WITH_SERVICE=1
CONTROL_ONLY=0
EXPLICIT=0
COMPONENTS=()

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: install.sh [options] [component...]

components: suzerain | suz | suzerain-mcp | suzy | all (default: all)
            (suzy = desktop UI, opt-in: not part of "all")
options:
  --version vX.Y.Z   install a specific release (default: latest, incl. prereleases)
  --bin-dir DIR      binary install location (default: ~/.local/bin)
  --no-service       do not install/enable the systemd or launchd service
  --control-only     skip the Gondolin runtime (node/qemu/KVM/driver bundle) —
                      for a dedicated `suzerain run --mode control` registry
                      host that never hosts agent VMs locally (default: full,
                      i.e. install the runtime too)
  -h, --help         show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --bin-dir) BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
    --no-service) WITH_SERVICE=0; shift ;;
    --control-only) CONTROL_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    all) COMPONENTS=(suzerain suz suzerain-mcp); EXPLICIT=1; shift ;;
    suzerain|suz|suzerain-mcp|suzy) COMPONENTS+=("$1"); EXPLICIT=1; shift ;;
    castellan) die "there is no separate 'castellan' component anymore — it's part of 'suzerain' now (see --help)" ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done
[ ${#COMPONENTS[@]} -eq 0 ] && COMPONENTS=(suzerain suz suzerain-mcp)

has() { command -v "$1" >/dev/null 2>&1; }

# ── platform ────────────────────────────────────────────────────────────────
OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS-$ARCH" in
  Linux-x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  Darwin-arm64)  TARGET="aarch64-apple-darwin" ;;
  *) die "unsupported platform $OS-$ARCH (supported: linux x86_64, macOS arm64)" ;;
esac

has curl || die "curl is required"

sha256() {
  if has sha256sum; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# ── resolve release ─────────────────────────────────────────────────────────
if [ -z "$VERSION" ]; then
  info "Resolving latest release of $REPO"
  # The releases API lists newest first and includes prereleases (unlike the
  # /releases/latest redirect, which skips them).
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases" \
    | grep -m1 '"tag_name"' | cut -d'"' -f4)"
  [ -n "$VERSION" ] || die "no releases found for $REPO"
fi
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
info "Installing $VERSION ($TARGET): ${COMPONENTS[*]}"

# ── download + verify ───────────────────────────────────────────────────────
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
BASE="https://github.com/$REPO/releases/download/$VERSION"

(cd "$TMP" && curl -fsSLO "$BASE/SHA256SUMS.txt") \
  || die "release $VERSION not found (or has no SHA256SUMS.txt)"

INSTALLED=()
for comp in "${COMPONENTS[@]}"; do
  archive="${comp}-${VERSION#v}-${TARGET}.tar.gz"
  info "Downloading $archive"
  if ! (cd "$TMP" && curl -fsSLO "$BASE/$archive"); then
    # Component not shipped in this release: fatal when explicitly
    # requested, skippable when part of the default "all" set.
    [ "$EXPLICIT" = 1 ] && die "missing asset: $archive"
    warn "$archive not in release $VERSION — skipping $comp"
    continue
  fi
  want="$(awk -v f="$archive" '$2 == f {print $1}' "$TMP/SHA256SUMS.txt")"
  [ -n "$want" ] || die "$archive not listed in SHA256SUMS.txt"
  got="$(sha256 "$TMP/$archive")"
  [ "$want" = "$got" ] || die "checksum mismatch for $archive"
  mkdir -p "$TMP/x"
  tar -C "$TMP/x" -xzf "$TMP/$archive"
  INSTALLED+=("$comp")
done
[ ${#INSTALLED[@]} -gt 0 ] || die "no components could be installed"
COMPONENTS=("${INSTALLED[@]}")

# ── install binaries ────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
for comp in "${COMPONENTS[@]}"; do
  src="$TMP/x/${comp}-${VERSION#v}-${TARGET}/bin/$comp"
  [ -f "$src" ] || die "archive for $comp is malformed (no bin/$comp)"
  install -m 755 "$src" "$BIN_DIR/$comp"
  info "Installed $BIN_DIR/$comp"
done

# ── Gondolin runtime: driver bundle + host dependencies ───────────────────
# Needed by `mode = standalone` (default) and `mode = agent` — either can
# host an agent VM locally. NOT needed by a `--control-only` install
# (`mode = control` never boots a VM). The driver bundle lives inside
# suzerain's own archive (extracted above), so this only runs if that
# archive was actually installed.
apt_install() {
  if has apt-get; then
    info "Installing via apt: $*"
    sudo apt-get update -qq && sudo apt-get install -y "$@"
  else
    return 1
  fi
}

install_gondolin_runtime() {
  local extracted="$TMP/x/suzerain-${VERSION#v}-$TARGET"

  # gondolin-driver sidecar (JS + platform-specific node_modules).
  info "Installing gondolin driver → $SUZERAIN_HOME/driver"
  mkdir -p "$SUZERAIN_HOME"
  rm -rf "$SUZERAIN_HOME/driver"
  cp -R "$extracted/driver" "$SUZERAIN_HOME/driver"

  # node (driver host process).
  if ! has node; then
    warn "node not found — required by the gondolin driver"
    if [ "$OS" = "Darwin" ] && has brew; then brew install node
    else apt_install nodejs || warn "install Node.js >= 22 manually: https://nodejs.org"; fi
  fi
  if has node; then
    node_major="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
    [ "$node_major" -ge 22 ] 2>/dev/null \
      || warn "node $(node --version) is older than the supported >= 22"
  fi

  # qemu (microVM backend).
  if ! has qemu-system-x86_64 && ! has qemu-system-aarch64; then
    warn "qemu not found — required to boot Gondolin microVMs"
    if [ "$OS" = "Darwin" ] && has brew; then brew install qemu
    else apt_install qemu-system-x86 || warn "install QEMU manually (e.g. apt install qemu-system-x86)"; fi
  fi

  # /dev/kvm access on Linux.
  if [ "$OS" = "Linux" ]; then
    if [ ! -e /dev/kvm ]; then
      warn "/dev/kvm missing — VMs need KVM (enable nested virtualization on VMs)"
    elif [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
      warn "/dev/kvm not accessible by $(id -un). Fix: sudo usermod -aG kvm $(id -un) (then re-login), or: sudo chmod o+rw /dev/kvm"
    fi
  fi

  # Guest VM images (~600MB) auto-download to ~/.cache/gondolin on first boot.
}

if [ "$CONTROL_ONLY" = 1 ]; then
  info "Control-only install: skipping the Gondolin runtime (node/qemu/driver bundle)"
elif [ -d "$TMP/x/suzerain-${VERSION#v}-$TARGET/driver" ]; then
  install_gondolin_runtime
else
  warn "suzerain archive has no driver bundle — skipping Gondolin runtime setup"
fi

# ── service ─────────────────────────────────────────────────────────────────
install_service() {
  comp="$1"
  stage="$TMP/x/${comp}-${VERSION#v}-${TARGET}/services"
  [ -d "$stage" ] || return 0
  if [ "$OS" = "Linux" ] && has systemctl; then
    mkdir -p "$HOME/.config/systemd/user"
    for unit in "$stage"/*.service; do
      dest="$HOME/.config/systemd/user/$(basename "$unit")"
      sed "s|%h/.local/bin|$BIN_DIR|g" "$unit" > "$dest"
      systemctl --user daemon-reload
      systemctl --user enable --now "$(basename "$unit")" \
        && info "Enabled systemd user service: $(basename "$unit")" \
        || warn "could not enable $(basename "$unit") (no user systemd session?)"
    done
  elif [ "$OS" = "Darwin" ] && has launchctl; then
    mkdir -p "$HOME/Library/LaunchAgents" "$SUZERAIN_HOME"
    for plist in "$stage"/*.plist; do
      dest="$HOME/Library/LaunchAgents/$(basename "$plist")"
      sed -e "s|BIN_DIR|$BIN_DIR|g" -e "s|HOME_DIR|$HOME|g" "$plist" > "$dest"
      launchctl unload "$dest" 2>/dev/null || true
      launchctl load "$dest" \
        && info "Loaded launchd agent: $(basename "$plist")" \
        || warn "could not load $(basename "$plist")"
    done
  else
    warn "no supported service manager; run $comp manually: $BIN_DIR/$comp run"
  fi
}

if [ "$WITH_SERVICE" = 1 ]; then
  for comp in "${COMPONENTS[@]}"; do
    [ "$comp" = "suzerain" ] && install_service "$comp"
  done
fi

# ── wrap up ─────────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH — add it (e.g. export PATH=\"$BIN_DIR:\$PATH\")" ;;
esac

info "Done. Installed versions:"
for comp in "${COMPONENTS[@]}"; do
  printf '  %-14s %s\n' "$comp" "$("$BIN_DIR/$comp" --version 2>/dev/null || echo '?')"
done
