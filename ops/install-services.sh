#!/usr/bin/env bash
# Install suzerain/castellan as user services (systemd on Linux, launchd on macOS).
# Usage: ops/install-services.sh [suzerain|castellan|both]  (default: both)
set -euo pipefail

WHAT="${1:-both}"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Build + install binaries if missing.
for bin in suzerain castellan suz; do
  if [[ ! -x "$BIN_DIR/$bin" ]]; then
    echo "building $bin → $BIN_DIR"
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    for b in suzerain castellan suz; do
      install -m 755 "$REPO_ROOT/target/release/$b" "$BIN_DIR/$b"
    done
    break
  fi
done

OS="$(uname -s)"
case "$OS" in
  Linux)
    mkdir -p "$HOME/.config/systemd/user"
    for svc in suzerain castellan; do
      if [[ "$WHAT" == "both" || "$WHAT" == "$svc" ]]; then
        sed "s|%h/.local/bin|$BIN_DIR|g" "$REPO_ROOT/ops/systemd/$svc.service" \
          > "$HOME/.config/systemd/user/$svc.service"
        systemctl --user daemon-reload
        systemctl --user enable --now "$svc.service"
        echo "installed + started systemd user service: $svc"
      fi
    done
    ;;
  Darwin)
    mkdir -p "$HOME/Library/LaunchAgents"
    for svc in controlplane castellan; do
      if [[ "$WHAT" == "both" || ("$WHAT" == "suzerain" && "$svc" == "controlplane") || "$WHAT" == "$svc" ]]; then
        plist="$REPO_ROOT/ops/launchd/com.suzerain.$svc.plist"
        dest="$HOME/Library/LaunchAgents/com.suzerain.$svc.plist"
        sed -e "s|BIN_DIR|$BIN_DIR|g" -e "s|HOME_DIR|$HOME|g" "$plist" > "$dest"
        launchctl unload "$dest" 2>/dev/null || true
        launchctl load "$dest"
        echo "installed + loaded launchd agent: com.suzerain.$svc"
      fi
    done
    ;;
  *)
    echo "unsupported OS: $OS" >&2
    exit 1
    ;;
esac
