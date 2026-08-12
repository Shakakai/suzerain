#!/usr/bin/env bash
# Dev network: suzerain + one castellan on this machine, logs interleaved,
# Ctrl-C shuts everything down (daemons, drivers, VMs).
#
# State lives in .dev/ (persistent across runs, gitignored).
# Web UI: http://127.0.0.1:8485 (dev port, avoids clashing with a real instance).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
DEV=.dev
export SUZERAIN_HOME="$PWD/$DEV/suzerain"
export CASTELLAN_HOME="$PWD/$DEV/castellan"
WORK="$DEV/run"
mkdir -p "$SUZERAIN_HOME" "$CASTELLAN_HOME" "$WORK"

# Provider keys for agent runs, if present locally.
[[ -f .env ]] && { set -a; source .env; set +a; }
export PATH="$HOME/.local/share/mise/shims:$PATH"
export SOPS_AGE_KEY_FILE="${SOPS_AGE_KEY_FILE:-$HOME/.config/sops/age/keys.txt}"

say() { echo -e "\033[36m[dev-network]\033[0m $*"; }

say "building (debug)…"
cargo build -p suzerain -p castellan -p suzerain-cli --quiet
SUZ=./target/debug/suz
SUZERAIN=./target/debug/suzerain
CASTELLAN=./target/debug/castellan

# Web UI on the dev port (no clash with a real instance on 8484).
if [[ ! -f "$SUZERAIN_HOME/config.toml" ]]; then
  printf '[web]\nport = 8485\n' > "$SUZERAIN_HOME/config.toml"
fi

# Secrets: reuse the real store if present; else synthesize one from .env.
if [[ ! -f "$SUZERAIN_HOME/secrets.age" && ! -f "$SUZERAIN_HOME/secrets.sops.yaml" ]]; then
  if [[ -f "$HOME/.local/share/suzerain/secrets.age" ]]; then
    cp "$HOME/.local/share/suzerain/secrets.age" "$SUZERAIN_HOME/secrets.age"
    say "copied secrets.age from ~/.local/share/suzerain"
  elif [[ -n "${KIMI_API_KEY:-}" && -f "$SOPS_AGE_KEY_FILE" ]]; then
    printf 'providers:\n  kimi-coding: "%s"\n' "$KIMI_API_KEY" > "$WORK/plain.yaml"
    sops --encrypt --age "$(age-keygen -y "$SOPS_AGE_KEY_FILE")" \
      --input-type yaml --output-type yaml "$WORK/plain.yaml" > "$SUZERAIN_HOME/secrets.sops.yaml"
    rm "$WORK/plain.yaml"
    say "synthesized secrets store from .env (KIMI_API_KEY)"
  else
    say "WARNING: no secrets store; agents will have no provider keys"
  fi
fi

PIDS=()
cleanup() {
  say "shutting down…"
  trap - INT TERM EXIT
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  sleep 1
  for pid in "${PIDS[@]:-}"; do kill -9 "$pid" 2>/dev/null || true; done
  # VM/driver children of the dev castellan die via stdin close (driver) or
  # their parent; sweep leftovers just in case.
  pkill -f "$CASTELLAN_HOME" 2>/dev/null || true
  exit 0
}
trap cleanup INT TERM EXIT

say "starting suzerain…"
"$SUZERAIN" run > "$WORK/suzerain.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do [[ -S "$SUZERAIN_HOME/suzerain.sock" ]] && break; sleep 1; done
SID=$($SUZ id)
say "suzerain endpoint: $SID"

if ! $SUZ daemon list 2>/dev/null | grep -q .; then
  CID=$("$CASTELLAN" init --suzerain "$SID" | grep "endpoint id" | head -1 | awk '{print $NF}')
  $SUZ daemon approve "$CID" > /dev/null
  say "castellan approved: ${CID:0:8}…"
else
  say "castellan already enrolled"
fi

say "starting castellan…"
"$CASTELLAN" run > "$WORK/castellan.log" 2>&1 &
PIDS+=($!)

say "streaming logs (ctrl-c to stop) — web ui: http://127.0.0.1:8485"
tail -n +1 -F "$WORK/suzerain.log" 2>/dev/null | sed -u $'s/^/\033[35m[suz]\033[0m  /' &
PIDS+=($!)
tail -n +1 -F "$WORK/castellan.log" 2>/dev/null | sed -u $'s/^/\033[33m[cast]\033[0m /' &
PIDS+=($!)

wait
