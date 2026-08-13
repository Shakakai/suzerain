#!/usr/bin/env bash
# Suzerain one-line installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Shakakai/suzerain/main/ops/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- castellan            # one component
#   curl -fsSL .../install.sh | bash -s -- --version v0.1.3 --no-service suzerain
#
# Components: suzerain castellan suz suzerain-mcp (default: all).
# Resolves the latest GitHub release (including prereleases) unless --version
# is given, downloads the per-component archives for this platform, verifies
# SHA256 checksums, installs binaries to ~/.local/bin, the gondolin driver to
# ~/.local/share/castellan/driver, and (unless --no-service) enables systemd
# user services (Linux) or launchd agents (macOS) for suzerain/castellan.
set -euo pipefail

REPO="Shakakai/suzerain"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
CASTELLAN_HOME="${CASTELLAN_HOME:-$HOME/.local/share/castellan}"
SUZERAIN_HOME="${SUZERAIN_HOME:-$HOME/.local/share/suzerain}"
VERSION=""
WITH_SERVICE=1
COMPONENTS=()

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: install.sh [options] [component...]

components: suzerain | castellan | suz | suzerain-mcp | all (default: all)
options:
  --version vX.Y.Z   install a specific release (default: latest, incl. prereleases)
  --bin-dir DIR      binary install location (default: ~/.local/bin)
  --no-service       do not install/enable systemd or launchd services
  -h, --help         show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --bin-dir) BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
    --no-service) WITH_SERVICE=0; shift ;;
    -h|--help) usage; exit 0 ;;
    all) COMPONENTS=(suzerain castellan suz suzerain-mcp); shift ;;
    suzerain|castellan|suz|suzerain-mcp) COMPONENTS+=("$1"); shift ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done
[ ${#COMPONENTS[@]} -eq 0 ] && COMPONENTS=(suzerain castellan suz suzerain-mcp)

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

for comp in "${COMPONENTS[@]}"; do
  archive="${comp}-${VERSION#v}-${TARGET}.tar.gz"
  info "Downloading $archive"
  (cd "$TMP" && curl -fsSLO "$BASE/$archive") || die "missing asset: $archive"
  want="$(awk -v f="$archive" '$2 == f {print $1}' "$TMP/SHA256SUMS.txt")"
  [ -n "$want" ] || die "$archive not listed in SHA256SUMS.txt"
  got="$(sha256 "$TMP/$archive")"
  [ "$want" = "$got" ] || die "checksum mismatch for $archive"
  mkdir -p "$TMP/x"
  tar -C "$TMP/x" -xzf "$TMP/$archive"
done

# ── install binaries ────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
for comp in "${COMPONENTS[@]}"; do
  src="$TMP/x/${comp}-${VERSION#v}-${TARGET}/bin/$comp"
  [ -f "$src" ] || die "archive for $comp is malformed (no bin/$comp)"
  install -m 755 "$src" "$BIN_DIR/$comp"
  info "Installed $BIN_DIR/$comp"
done

# ── castellan runtime: driver bundle + host dependencies ───────────────────
apt_install() {
  if has apt-get; then
    info "Installing via apt: $*"
    sudo apt-get update -qq && sudo apt-get install -y "$@"
  else
    return 1
  fi
}

for comp in "${COMPONENTS[@]}"; do
  [ "$comp" = "castellan" ] || continue

  # gondolin-driver sidecar (JS + platform-specific node_modules).
  info "Installing gondolin driver → $CASTELLAN_HOME/driver"
  mkdir -p "$CASTELLAN_HOME"
  rm -rf "$CASTELLAN_HOME/driver"
  cp -R "$TMP/x/castellan-${VERSION#v}-$TARGET/driver" "$CASTELLAN_HOME/driver"

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
done

# ── services ────────────────────────────────────────────────────────────────
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
    mkdir -p "$HOME/Library/LaunchAgents" "$HOME/.local/share/suzerain" "$CASTELLAN_HOME"
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
    case "$comp" in suzerain|castellan) install_service "$comp" ;; esac
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
