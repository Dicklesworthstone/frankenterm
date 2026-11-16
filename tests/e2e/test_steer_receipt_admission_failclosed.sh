#!/usr/bin/env bash
# Perfect e2e contract for the SteeringReceipt admission flow (W5.x / ft-7h5da.6.x).
#
# Asserts the full plan -> gate -> admit path is FAIL-CLOSED:
#   * steer_run_gate fails closed on unverifiable bindings (a contract the
#     receipt captured but the caller did not supply for revalidation is denied,
#     never executed on an unconfirmed binding);
#   * receipt_admits_action is PLAN-HASH-SCOPED (admits only the exact plan hash
#     the receipt bound; does not require / consult the live mission);
#   * no replay (TTL is_expired gate) and no forgery (receipt_id is
#     content-addressed; validate() recomputes + compares it, and the on-disk
#     store id is path-traversal-guarded);
#   * SteerRunGate::Valid is reachable ONLY after every guard (default-deny).
#
# ZERO RCH / ZERO cargo: pure static analysis (grep/awk over committed source).
# Exit 0 IS the proof; it does not require frankenterm-core to compile, so it
# locks the W5.x fixes in even under the dep-upgrade meta-blocker. Bead: ft-utp88.
#
# Exit codes: 0 = all invariants hold; 1 = an invariant failed; 2 = missing input.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || { echo "FATAL: cannot cd to repo root"; exit 2; }

RUN="crates/frankenterm-core/src/steer_run.rs"
RECEIPT="crates/frankenterm-core/src/steering.rs"
STORE="crates/frankenterm-core/src/steer_receipt_store.rs"
CONF_CHAIN="crates/frankenterm-core/tests/conformance_steering_receipt_chain.rs"
CONF_PLAN="crates/frankenterm-core/tests/conformance_steer_plan_golden.rs"
GOLDEN="crates/frankenterm-core/tests/golden/steer_plan_scenarios.json"
E2E="crates/frankenterm/tests/steer_plan_e2e.rs"

for f in "$RUN" "$RECEIPT" "$STORE"; do
  [[ -f "$f" ]] || { echo "FATAL: missing required input: $f"; exit 2; }
done

# Slice the two gate function bodies (top-level `pub fn ... { ... }`, ends at ^}).
GATE_BODY="$(awk '/pub fn steer_run_gate/{f=1} f{print} f&&/^}/{exit}' "$RUN")"
ADMIT_BODY="$(awk '/pub fn receipt_admits_action/{f=1} f{print} f&&/^}/{exit}' "$RUN")"
VALIDATE_BODY="$(awk '/pub fn validate/{f=1} f{print} f&&/^    }/{exit}' "$RECEIPT")"

PASS=0
FAIL=0
pass() { printf 'PASS  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL  %s\n' "$1"; FAIL=$((FAIL + 1)); }
has()  { grep -q -- "$1" <<<"$2"; }

# Returns 0 if SteerRunGate::Valid is the LAST meaningful (non-blank, non-brace)
# line of the supplied fn body — i.e. success is only reached after every guard.
valid_is_last() {
  local body="$1"
  local last
  last="$(grep -vE '^\s*$|^\s*}\s*$' <<<"$body" | tail -1 | tr -d ' ')"
  [[ "$last" == "SteerRunGate::Valid" ]]
}

echo "=== SteeringReceipt admission fail-closed contract (ft-utp88 / W5.x) ==="
echo ""

[[ -n "$GATE_BODY" && -n "$ADMIT_BODY" && -n "$VALIDATE_BODY" ]] \
  && pass "S0 function bodies sliced non-vacuously (gate/admit/validate)" \
  || fail "S0 could not slice gate/admit/validate bodies (anchor drift)"

# --- S1: steer_run_gate is fail-closed; Valid only after every guard ---
if has 'receipt.validate()' "$GATE_BODY" \
  && has 'is_expired' "$GATE_BODY" \
  && has '"envelope.admit"' "$GATE_BODY" \
  && has 'UnverifiableBinding' "$GATE_BODY" \
  && has 'HashMismatch' "$GATE_BODY" \
  && [[ "$(grep -c 'SteerRunGate::Valid' <<<"$GATE_BODY")" -eq 1 ]] \
  && valid_is_last "$GATE_BODY"; then
  pass "S1 steer_run_gate runs validate->expired->admit->binding->hash guards; Valid is the sole, final success"
else
  fail "S1 steer_run_gate fail-closed ordering broken (a guard missing, or Valid not gated behind all checks)"
fi

# --- S2: UnverifiableBinding — captured-but-unsupplied binding is DENIED ---
if has 'mission_contract_hash.is_some() && live_mission.is_none()' "$GATE_BODY" \
  && has 'tx_contract_hash.is_some() && live_tx_hash.is_none()' "$GATE_BODY" \
  && has 'contract: "mission"' "$GATE_BODY" \
  && has 'contract: "tx"' "$GATE_BODY"; then
  pass "S2 unverifiable binding fails closed (captured mission/tx hash + no live value -> UnverifiableBinding)"
else
  fail "S2 unverifiable-binding deny missing — a forgotten live binding could bypass drift detection"
fi

# --- S3: receipt_admits_action is PLAN-HASH-SCOPED (and mission-independent) ---
ADMIT_SIG="$(awk '/pub fn receipt_admits_action/{f=1} f{print} f&&/\{/{exit}' "$RUN")"
if has 'action_plan_hash' "$ADMIT_SIG" \
  && ! has 'live_mission' "$ADMIT_SIG" \
  && has '!receipt.matches_tx_hash(action_plan_hash)' "$ADMIT_BODY" \
  && has 'HashMismatch' "$ADMIT_BODY" \
  && [[ "$(grep -c 'SteerRunGate::Valid' <<<"$ADMIT_BODY")" -eq 1 ]] \
  && valid_is_last "$ADMIT_BODY"; then
  pass "S3 receipt_admits_action admits ONLY the exact bound plan hash (scoped; no live_mission required); else HashMismatch"
else
  fail "S3 receipt_admits_action not plan-hash-scoped (must gate on matches_tx_hash(action_plan_hash); Valid last)"
fi

# --- S4: envelope-verdict anti-forgery gate in BOTH entry points ---
if has 'envelope_verdict != "envelope.admit"' "$GATE_BODY" \
  && has 'NotAdmitted' "$GATE_BODY" \
  && has 'envelope_verdict != "envelope.admit"' "$ADMIT_BODY" \
  && has 'NotAdmitted' "$ADMIT_BODY"; then
  pass "S4 both gates reject any receipt whose envelope verdict is not 'envelope.admit' (NotAdmitted)"
else
  fail "S4 a non-admit envelope verdict could slip through (forged/denied receipt admitting an action)"
fi

# --- S5: forgery — receipt_id is content-addressed; validate recomputes+compares ---
if has 'self.compute_id()' "$VALIDATE_BODY" \
  && has 'self.receipt_id != expected' "$VALIDATE_BODY" \
  && has 'ReceiptIdMismatch' "$VALIDATE_BODY"; then
  pass "S5 forgery fails closed: validate() recomputes the content-addressed id and rejects ReceiptIdMismatch"
else
  fail "S5 receipt validate() does not re-derive+compare the content id — a tampered binding could pass"
fi

# --- S6: replay — TTL is_expired gate in BOTH entry points ---
if has 'is_expired' "$GATE_BODY" && has 'SteerRunGate::Expired' "$GATE_BODY" \
  && has 'is_expired' "$ADMIT_BODY" && has 'SteerRunGate::Expired' "$ADMIT_BODY"; then
  pass "S6 replay fails closed: both gates reject expired receipts (TTL is_expired -> Expired)"
else
  fail "S6 TTL/expiry gate missing from a gate entry point (stale receipt replay possible)"
fi

# --- S7: on-disk store id is path-traversal-guarded ---
IDGUARD="$(awk '/pub fn is_valid_receipt_id/{f=1} f{print} f&&/^}/{exit}' "$STORE")"
PATHFN="$(awk '/pub fn receipt_path/{f=1} f{print} f&&/^}/{exit}' "$STORE")"
if has 'strip_prefix("steer:")' "$IDGUARD" \
  && has 'len() == 32' "$IDGUARD" \
  && has 'is_valid_receipt_id' "$PATHFN" \
  && has 'InvalidInput' "$PATHFN"; then
  pass "S7 store rejects forged/traversal receipt ids (steer:+32hex guard; receipt_path errors on invalid id)"
else
  fail "S7 receipt-store id guard weakened (path traversal / arbitrary-id write possible)"
fi

# --- S8: behavioral coverage exists (runnable proof complementing this static lock) ---
missing=""
for f in "$CONF_CHAIN" "$CONF_PLAN" "$GOLDEN" "$E2E"; do
  [[ -f "$f" ]] || missing="$missing $f"
done
if [[ -z "$missing" ]]; then
  pass "S8 behavioral coverage present (receipt-chain + plan-golden conformance, golden scenarios, plan e2e)"
else
  fail "S8 missing behavioral coverage file(s):$missing"
fi

echo ""
echo "=== Summary: ${PASS} passed, ${FAIL} failed ==="
if [[ "$FAIL" -ne 0 ]]; then
  exit 1
fi
exit 0
