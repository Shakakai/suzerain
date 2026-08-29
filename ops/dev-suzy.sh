#!/usr/bin/env bash
# Dev wiring: boot `suzerain run` (standalone mode) and `suzy`, pre-wired
# so Suzy already has a workspace connected to this suzerain the moment its
# window opens — no manual "add workspace" dialog, no manual `suz operator
# approve`.
#
# How the wiring works (docs/SUZY.md §6.4 has the full protocol):
#   1. `suzy --print-operator-id` — Suzy's own iroh identity is persisted
#      at $SUZY_HOME/iroh.key on first use; this prints its public half
#      without opening the GUI.
#   2. `suzerain run --operator <that id>` — allowlists Suzy's id on the
#      operator channel *before* Suzy ever dials in, and prints the
#      control plane's own endpoint id at startup.
#   3. `suzy --add-workspace <name> <that id>` — writes the workspace entry
#      into $SUZY_HOME/config.toml directly (again, no GUI), so step 4
#      opens with the workspace already present.
#   4. `suzy` — GUI opens, workspace already connected.
#
# State lives in .dev/ (persistent across runs, gitignored): a separate
# SUZERAIN_HOME/SUZY_HOME pair from ops/dev-network.sh's, so this can run
# alongside (or instead of) that script without clobbering either.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
DEV=.dev
export SUZERAIN_HOME="$PWD/$DEV/suzerain-suzy"
export SUZY_HOME="$PWD/$DEV/suzy"
WORK="$DEV/run-suzy"
mkdir -p "$SUZERAIN_HOME" "$SUZY_HOME" "$WORK"

[[ -f .env ]] && { set -a; source .env; set +a; }
export PATH="$HOME/.local/share/mise/shims:$PATH"
export SOPS_AGE_KEY_FILE="${SOPS_AGE_KEY_FILE:-$SUZERAIN_HOME/age-keys.txt}"

say() { echo -e "\033[35m[dev-suzy]\033[0m $*"; }

say "building (debug)…"
cargo build -p suzerain -p suzerain-cli -p suzy --quiet
SUZ=./target/debug/suz
SUZERAIN=./target/debug/suzerain
SUZY=./target/debug/suzy

# Web UI on its own dev port (avoids clashing with ops/dev-network.sh's
# 8485 or a real instance's 8484).
if [[ ! -f "$SUZERAIN_HOME/suzerain.toml" ]]; then
  printf '[web]\nport = 8486\n' > "$SUZERAIN_HOME/suzerain.toml"
fi
export SUZERAIN_API_URL="http://127.0.0.1:8486"

# Secrets: reuse the real store if present; else synthesize one from .env
# (same fallback chain as ops/dev-network.sh) — except the age identity is
# only generated fresh in the branches that don't copy a real secrets.age,
# since a copied ciphertext needs *its* matching identity, not a new one
# (dev-network.sh generates the identity unconditionally before this check,
# which would mismatch a copied secrets.age the same way — not hit there in
# practice, but avoided here on purpose).
if [[ ! -f "$SUZERAIN_HOME/secrets.age" && ! -f "$SUZERAIN_HOME/secrets.sops.yaml" ]]; then
  if [[ -f "$HOME/.local/share/suzerain/secrets.age" && -f "$HOME/.local/share/suzerain/age-keys.txt" ]]; then
    cp "$HOME/.local/share/suzerain/secrets.age" "$SUZERAIN_HOME/secrets.age"
    cp "$HOME/.local/share/suzerain/age-keys.txt" "$SOPS_AGE_KEY_FILE"
    say "copied secrets.age + matching age identity from ~/.local/share/suzerain"
  elif [[ -n "${KIMI_API_KEY:-}" ]]; then
    [[ -f "$SOPS_AGE_KEY_FILE" ]] || age-keygen -o "$SOPS_AGE_KEY_FILE" 2>/dev/null || true
    printf 'providers:\n  kimi-coding: "%s"\n' "$KIMI_API_KEY" > "$WORK/plain.yaml"
    sops --encrypt --age "$(age-keygen -y "$SOPS_AGE_KEY_FILE")" \
      --input-type yaml --output-type yaml "$WORK/plain.yaml" > "$SUZERAIN_HOME/secrets.sops.yaml"
    rm "$WORK/plain.yaml"
    say "synthesized secrets store from .env (KIMI_API_KEY)"
  else
    say "WARNING: no secrets store; agents will have no provider keys"
  fi
fi

say "reading Suzy's operator identity ($SUZY_HOME/iroh.key)…"
SUZY_ID=$("$SUZY" --print-operator-id)
say "suzy operator id: $SUZY_ID"

PIDS=()
cleanup() {
  say "shutting down…"
  trap - INT TERM EXIT
  for pid in "${PIDS[@]:-}"; do kill -TERM "$pid" 2>/dev/null || true; done
  sleep 2
  for pid in "${PIDS[@]:-}"; do kill -9 "$pid" 2>/dev/null || true; done
  exit 0
}
trap cleanup INT TERM EXIT

say "starting suzerain (standalone mode), pre-approving suzy's operator id…"
"$SUZERAIN" run --operator "$SUZY_ID" > "$WORK/suzerain.log" 2>&1 &
PIDS+=($!)
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

say "wiring suzy's workspace to ${SID}…"
"$SUZY" --add-workspace dev "$SID"

say "launching suzy — the 'dev' workspace should already be connected"
"$SUZY" > "$WORK/suzy.log" 2>&1 &
PIDS+=($!)

say "streaming suzerain logs (ctrl-c to stop everything) — web ui: http://127.0.0.1:8486"
tail -n +1 -F "$WORK/suzerain.log" 2>/dev/null &
PIDS+=($!)

wait
