#!/usr/bin/env bash
# End-to-end lifecycle test against a real suzerain stack (standalone mode:
# one binary, control plane + co-located agent hosting).
# Runs locally and in CI (ops/.github/workflows/e2e.yml).
#
# Requires: workspace built (target/debug), qemu, node, sops, age-keygen.
# Secrets:  KIMI_API_KEY (LLM provider key for the test agent).
# Env:      SUZERAIN_HOME overrides the state dir.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
SUZERAIN_HOME="${SUZERAIN_HOME:-/tmp/suz-e2e}"
WORK="${E2E_WORK:-/tmp/suz-e2e-work}"
SUZ=./target/debug/suz
SUZERAIN=./target/debug/suzerain

if [[ -z "${KIMI_API_KEY:-}" ]]; then
  echo "KIMI_API_KEY not set — skipping e2e (set it to run the full lifecycle)"
  exit 0
fi

cleanup() {
  SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent destroy e2e-agent >/dev/null 2>&1 || true
  # SIGTERM (not SIGKILL): lets suzerain's shutdown handler run, which is
  # what tears down the co-located agent-hosting child and its VMs.
  pkill -TERM -f "target/debug/suzerain run" >/dev/null 2>&1 || true
  sleep 2
  pkill -9 -f "target/debug/suzerain run" >/dev/null 2>&1 || true
  pkill -9 -f gondolin-driver >/dev/null 2>&1 || true
  pkill -9 -f qemu-system >/dev/null 2>&1 || true
  rm -rf "$SUZERAIN_HOME" "$WORK"
}
trap cleanup EXIT
cleanup >/dev/null 2>&1 || true
mkdir -p "$SUZERAIN_HOME" "$WORK"

say() { echo "=== $* ==="; }
dump_diagnostics() {
  echo "--- suzerain.log (tail) ---" >&2
  tail -n 200 "$WORK/suzerain.log" 2>/dev/null >&2 || true
  echo "--- agent central log(s) (tail) ---" >&2
  for f in "$SUZERAIN_HOME"/logs/*.jsonl; do
    [[ -e "$f" ]] || continue
    echo "-- $f --" >&2
    tail -n 100 "$f" >&2 || true
  done
}
fail() {
  echo "E2E FAILED: $*" >&2
  dump_diagnostics
  exit 1
}

# ── Secrets store (sops/age) ─────────────────────────────────────────────
say "secrets store"
export SOPS_AGE_KEY_FILE="$WORK/keys.txt"
age-keygen -o "$SOPS_AGE_KEY_FILE" 2>/dev/null
printf 'providers:\n  kimi-coding: "%s"\n' "$KIMI_API_KEY" > "$WORK/plain.yaml"
sops --encrypt --age "$(age-keygen -y "$SOPS_AGE_KEY_FILE")" \
  --input-type yaml --output-type yaml "$WORK/plain.yaml" > "$SUZERAIN_HOME/secrets.sops.yaml"
rm "$WORK/plain.yaml"

# ── Boot standalone (control plane + co-located agent hosting) ──────────
say "suzerain up (standalone mode)"
# Operator allow list for the shell-session probe (iroh operator channel).
PROBE=./target/debug/examples/shell-probe
[[ -x "$PROBE" ]] || cargo build -p suzy --example shell-probe
PROBE_ID=$("$PROBE" --print-id --key-file "$WORK/probe.key")
printf '[operator]\nallow = ["%s"]\n' "$PROBE_ID" > "$SUZERAIN_HOME/suzerain.toml"
SUZERAIN_HOME="$SUZERAIN_HOME" nohup "$SUZERAIN" run > "$WORK/suzerain.log" 2>&1 &
for i in $(seq 1 30); do
  SID=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ id 2>/dev/null) && break
  sleep 1
done
[[ -n "${SID:-}" ]] || fail "suzerain id"
echo "suzerain: $SID"

# ── Co-located agent host comes online automatically (no enroll step) ────
say "agent host online"
for i in $(seq 1 30); do
  SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ daemon list 2>/dev/null | grep -q online && break
  sleep 1
done
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ daemon list | grep -q online || fail "agent host never came online"

# ── Create agent ─────────────────────────────────────────────────────────
say "agent create"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent create --manifest examples/researcher.toml | tee "$WORK/create.out"
grep -q "created researcher-1" "$WORK/create.out" || fail "create"

# ── Ask (provider auth via sliced secrets) ───────────────────────────────
say "agent ask"
OUT=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent ask researcher-1 "Reply with exactly: e2e-ok" | tail -1)
echo "answer: $OUT"
grep -q "e2e-ok" <<< "$OUT" || fail "ask: $OUT"

# ── Shell session probe (microVM → driver → agent host → control plane → client) ──
say "shell session probe"
"$PROBE" --key-file "$WORK/probe.key" "$SID" researcher-1 e2e-shell-ok || fail "shell probe"

# ── Auto-suspend + transparent wake ──────────────────────────────────────
# (Sessions rotate on every suspend by design, so this checks wake
# correctness — not in-context memory. History continuity is covered by
# the central-log check below.)
say "auto-suspend + transparent wake"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent config researcher-1 --auto-suspend 15s > /dev/null
for i in $(seq 1 24); do
  SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent list 2>/dev/null | grep -q "sleeping" && break
  sleep 5
done
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent list | grep -q sleeping || fail "agent never auto-suspended"
OUT=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent ask researcher-1 "Reply with exactly: e2e-woke" | tail -1)
echo "answer: $OUT"
grep -q "e2e-woke" <<< "$OUT" || fail "transparent wake: $OUT"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent config researcher-1 --auto-suspend default > /dev/null

# ── Web UI test (secrets add-provider flow) ──────────────────────────────
say "web ui test"
if command -v node >/dev/null 2>&1; then
  node tools/ui-test/ui-test.mjs http://127.0.0.1:8484 || fail "web ui test"
else
  echo "node missing — skipping ui test"
fi

# ── Central logs ─────────────────────────────────────────────────────────
say "central logs"
N=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent logs researcher-1 --tail 500 | grep -c message_end || true)
[[ "$N" -gt 0 ]] || fail "no events in central log"

say "destroy"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent destroy researcher-1 > /dev/null
say "E2E PASSED"
