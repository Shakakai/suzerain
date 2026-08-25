#!/usr/bin/env bash
# Install suzerain as a user service (systemd on Linux, launchd on macOS).
# One binary, one service — `suzerain run` defaults to standalone mode
# (control plane + co-located agent-hosting); edit [role].mode in
# suzerain.toml (or the unit's ExecStart/ProgramArguments --mode arg) for a
# dedicated control-only or agent-only host.
# Usage: ops/install-services.sh
set -euo pipefail

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Build + install binaries if missing.
if [[ ! -x "$BIN_DIR/suzerain" ]]; then
  echo "building suzerain, suz → $BIN_DIR"
  cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
  for b in suzerain suz; do
    install -m 755 "$REPO_ROOT/target/release/$b" "$BIN_DIR/$b"
  done
fi

OS="$(uname -s)"
case "$OS" in
  Linux)
    mkdir -p "$HOME/.config/systemd/user"
    sed "s|%h/.local/bin|$BIN_DIR|g" "$REPO_ROOT/ops/systemd/suzerain.service" \
      > "$HOME/.config/systemd/user/suzerain.service"
    systemctl --user daemon-reload
    systemctl --user enable --now suzerain.service
    echo "installed + started systemd user service: suzerain"
    ;;
  Darwin)
    mkdir -p "$HOME/Library/LaunchAgents"
    plist="$REPO_ROOT/ops/launchd/com.suzerain.controlplane.plist"
    dest="$HOME/Library/LaunchAgents/com.suzerain.controlplane.plist"
    sed -e "s|BIN_DIR|$BIN_DIR|g" -e "s|HOME_DIR|$HOME|g" "$plist" > "$dest"
    launchctl unload "$dest" 2>/dev/null || true
    launchctl load "$dest"
    echo "installed + loaded launchd agent: com.suzerain.controlplane"
    ;;
  *)
    echo "unsupported OS: $OS" >&2
    exit 1
    ;;
esac
