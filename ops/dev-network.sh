#!/usr/bin/env bash
# Dev network: `suzerain run` in standalone mode on this machine (one
# binary, two co-located processes — control plane + agent-hosting child),
# logs streamed, Ctrl-C shuts everything down (daemons, drivers, VMs).
#
# State lives in .dev/ (persistent across runs, gitignored).
# Web UI: http://127.0.0.1:8485 (dev port, avoids clashing with a real instance).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
DEV=.dev
export SUZERAIN_HOME="$PWD/$DEV/suzerain"
WORK="$DEV/run"
mkdir -p "$SUZERAIN_HOME" "$WORK"

# Provider keys for agent runs, if present locally.
[[ -f .env ]] && { set -a; source .env; set +a; }
export PATH="$HOME/.local/share/mise/shims:$PATH"
export SOPS_AGE_KEY_FILE="${SOPS_AGE_KEY_FILE:-$SUZERAIN_HOME/age-keys.txt}"
# The dev store's age identity lives in the fleet home; create it if absent
# so the sops-CLI synthesis below (and suzerain itself) can use it.
[[ -f "$SOPS_AGE_KEY_FILE" ]] || age-keygen -o "$SOPS_AGE_KEY_FILE" 2>/dev/null || true

say() { echo -e "\033[36m[dev-network]\033[0m $*"; }

say "building (debug)…"
cargo build -p suzerain -p suzerain-cli --quiet
SUZ=./target/debug/suz
SUZERAIN=./target/debug/suzerain

# Web UI on the dev port (no clash with a real instance on 8484).
if [[ ! -f "$SUZERAIN_HOME/suzerain.toml" ]]; then
  printf '[web]\nport = 8485\n' > "$SUZERAIN_HOME/suzerain.toml"
fi
export SUZERAIN_API_URL="http://127.0.0.1:8485"

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
  # SIGTERM (not SIGKILL): lets suzerain's shutdown handler run, which is
  # what tears down the co-located agent-worker child and its VMs — see
  # docs/UNIFIED-AGENT-API-DESIGN.md's SIGTERM-handling note.
  for pid in "${PIDS[@]:-}"; do kill -TERM "$pid" 2>/dev/null || true; done
  sleep 2
  for pid in "${PIDS[@]:-}"; do kill -9 "$pid" 2>/dev/null || true; done
  exit 0
}
trap cleanup INT TERM EXIT

say "starting suzerain (standalone mode: control plane + agent-hosting)…"
"$SUZERAIN" run > "$WORK/suzerain.log" 2>&1 &
PIDS+=($!)
# Wait for a REAL listener, not just the socket file (a stale socket from a
# previous run passes -S before the new process binds → connection refused).
for i in $(seq 1 30); do
  if SID=$($SUZ id 2>/dev/null); then break; fi
  if ! kill -0 "${PIDS[0]}" 2>/dev/null; then
    say "suzerain failed to start; log follows:"
    cat "$WORK/suzerain.log"
    exit 1
  fi
  sleep 1
done
[[ -n "${SID:-}" ]] || { say "suzerain never became ready"; exit 1; }
say "suzerain endpoint: $SID"

say "streaming logs (ctrl-c to stop) — web ui: http://127.0.0.1:8485"
tail -n +1 -F "$WORK/suzerain.log" 2>/dev/null &
PIDS+=($!)

wait
