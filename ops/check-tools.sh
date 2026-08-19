#!/usr/bin/env bash
# Verify that all tools required for development are installed.
#
# Required tools cause a non-zero exit with install instructions.
# Optional tools (only needed by specific tasks) print a warning.
#
# Run via `mise run setup` (which runs `mise install` first, so mise-managed
# tools from mise.toml — rust, node, sops, age — should already be present).
set -uo pipefail

os="$(uname -s)"

pkg_hint() { # $1 = linux hint, $2 = mac hint
  if [ "$os" = "Darwin" ]; then
    echo "$2"
  else
    echo "$1"
  fi
}

missing=()
require() { # $1 = command, $2 = install hint
  if ! command -v "$1" >/dev/null 2>&1; then
    missing+=("  ✗ $1 — install with: $2")
  fi
}

warn_only=()
optional() { # $1 = command, $2 = what needs it, $3 = install hint
  if ! command -v "$1" >/dev/null 2>&1; then
    warn_only+=("  ! $1 — needed by $2 — install with: $3")
  fi
}

# ── Required: mise-managed toolchains (installed by `mise install`) ──────
require cargo       "mise install (managed via mise.toml)"
require node        "mise install (managed via mise.toml)"
require npm         "mise install (managed via mise.toml)"
require sops        "mise install (managed via mise.toml)"
require age-keygen  "mise install (managed via mise.toml) — or $(pkg_hint 'sudo apt install age' 'brew install age')"

# ── Required: system tools ───────────────────────────────────────────────
# cc is provisioned by ops/ensure-cc.sh (runs before this script in setup).
require cc          "$(pkg_hint 'sudo apt install gcc' 'xcode-select --install') (or re-run setup — ops/ensure-cc.sh installs a zig-based cc)"
require rsync       "$(pkg_hint 'sudo apt install rsync' 'brew install rsync') (used by mise run package)"
require curl        "$(pkg_hint 'sudo apt install curl' 'brew install curl')"

# ── Optional: only needed by specific tasks ──────────────────────────────
if ! command -v qemu-system-x86_64 >/dev/null 2>&1 && ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
  warn_only+=("  ! qemu-system — needed by castellan VMs / ops/e2e.sh — install with: $(pkg_hint 'sudo apt install qemu-system' 'brew install qemu')")
fi
if ! command -v google-chrome >/dev/null 2>&1 && ! command -v chromium >/dev/null 2>&1 \
   && ! command -v chromium-browser >/dev/null 2>&1 && ! command -v chrome >/dev/null 2>&1; then
  warn_only+=("  ! chrome/chromium — needed by mise run ui:test — install with: $(pkg_hint 'sudo apt install chromium-browser' 'brew install --cask google-chrome')")
fi
if [ "$os" = "Linux" ]; then
  optional systemctl "mise run install:services" "$(pkg_hint 'sudo apt install systemd' '')"
fi

if [ "${#warn_only[@]}" -gt 0 ]; then
  echo "[check-tools] optional tools missing (only needed by specific tasks):"
  printf '%s\n' "${warn_only[@]}"
fi

if [ "${#missing[@]}" -gt 0 ]; then
  echo "[check-tools] ERROR: required tools are missing:" >&2
  printf '%s\n' "${missing[@]}" >&2
  echo "[check-tools] install the tools above and re-run: mise run setup" >&2
  exit 1
fi

echo "[check-tools] all required tools present"
