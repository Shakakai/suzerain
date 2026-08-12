#!/usr/bin/env bash
# End-to-end lifecycle test against a real suzerain+castellan stack.
# Runs locally and in CI (ops/.github/workflows/e2e.yml).
#
# Requires: workspace built (target/debug), qemu, node, sops, age-keygen.
# Secrets:  KIMI_API_KEY (LLM provider key for the test agent).
# Env:      SUZERAIN_HOME / CASTELLAN_HOME override state dirs.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
SUZERAIN_HOME="${SUZERAIN_HOME:-/tmp/suz-e2e}"
CASTELLAN_HOME="${CASTELLAN_HOME:-/tmp/cast-e2e}"
WORK="${E2E_WORK:-/tmp/suz-e2e-work}"
SUZ=./target/debug/suz
CASTELLAN=./target/debug/castellan
SUZERAIN=./target/debug/suzerain

if [[ -z "${KIMI_API_KEY:-}" ]]; then
  echo "KIMI_API_KEY not set — skipping e2e (set it to run the full lifecycle)"
  exit 0
fi

cleanup() {
  $SUZ agent destroy e2e-agent >/dev/null 2>&1 || true
  pkill -f "suzerain run" >/dev/null 2>&1 || true
  pkill -f "castellan run" >/dev/null 2>&1 || true
  pkill -f gondolin-driver >/dev/null 2>&1 || true
  pkill -f qemu-system >/dev/null 2>&1 || true
  rm -rf "$SUZERAIN_HOME" "$CASTELLAN_HOME" "$WORK"
}
trap cleanup EXIT
cleanup >/dev/null 2>&1 || true
mkdir -p "$SUZERAIN_HOME" "$WORK"

say() { echo "=== $* ==="; }
fail() { echo "E2E FAILED: $*" >&2; exit 1; }

# ── Secrets store (sops/age) ─────────────────────────────────────────────
say "secrets store"
export SOPS_AGE_KEY_FILE="$WORK/keys.txt"
age-keygen -o "$SOPS_AGE_KEY_FILE" 2>/dev/null
printf 'providers:\n  kimi-coding: "%s"\n' "$KIMI_API_KEY" > "$WORK/plain.yaml"
sops --encrypt --age "$(age-keygen -y "$SOPS_AGE_KEY_FILE")" \
  --input-type yaml --output-type yaml "$WORK/plain.yaml" > "$SUZERAIN_HOME/secrets.sops.yaml"
rm "$WORK/plain.yaml"

# ── Boot control plane ───────────────────────────────────────────────────
say "suzerain up"
SUZERAIN_HOME="$SUZERAIN_HOME" nohup "$SUZERAIN" run > "$WORK/suzerain.log" 2>&1 &
for i in $(seq 1 30); do [[ -S "$SUZERAIN_HOME/suzerain.sock" ]] && break; sleep 1; done
SID=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ id) || fail "suzerain id"
echo "suzerain: $SID"

# ── Enroll daemon ────────────────────────────────────────────────────────
say "castellan enroll"
INIT_OUT=$(CASTELLAN_HOME="$CASTELLAN_HOME" "$CASTELLAN" init --suzerain "$SID" 2>/dev/null)
CID=$(head -1 <<< "$INIT_OUT" | awk '{print $NF}')
[[ -n "$CID" ]] || fail "castellan init"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ daemon approve "$CID" > /dev/null
CASTELLAN_HOME="$CASTELLAN_HOME" nohup "$CASTELLAN" run > "$WORK/castellan.log" 2>&1 &
for i in $(seq 1 30); do
  SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ daemon list 2>/dev/null | grep -q online && break
  sleep 1
done
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ daemon list | grep -q online || fail "daemon never came online"

# ── Create agent ─────────────────────────────────────────────────────────
say "agent create"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent create --manifest examples/researcher.toml | tee "$WORK/create.out"
grep -q "created researcher-1" "$WORK/create.out" || fail "create"

# ── Ask (provider auth via sliced secrets) ───────────────────────────────
say "agent ask"
OUT=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent ask researcher-1 "Reply with exactly: e2e-ok" | tail -1)
echo "answer: $OUT"
grep -q "e2e-ok" <<< "$OUT" || fail "ask: $OUT"

# ── Memory across stop/start ─────────────────────────────────────────────
say "stop/start memory"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent ask researcher-1 "Remember codeword E2E-1. Reply: noted" > /dev/null
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent stop researcher-1 > /dev/null
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent start researcher-1 > /dev/null
OUT=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent ask researcher-1 "Codeword? Just the codeword." | tail -1)
grep -q "E2E-1" <<< "$OUT" || fail "memory after restart: $OUT"

# ── Suspend + restore ────────────────────────────────────────────────────
say "suspend/restore memory"
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent suspend researcher-1 > /dev/null
SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent restore researcher-1 > /dev/null
OUT=$(SUZERAIN_HOME="$SUZERAIN_HOME" $SUZ agent ask researcher-1 "Codeword? Just the codeword." | tail -1)
grep -q "E2E-1" <<< "$OUT" || fail "memory after restore: $OUT"

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
