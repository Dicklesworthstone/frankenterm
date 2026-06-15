#!/usr/bin/env bash
# Golden + e2e contract test for the MCP/robot read-path SECURITY surface.
#
# Locks in the security fixes:
#   * ft-ps9fu — wa://swarm/scent (and wa.state) read paths redact pane metadata
#     AND enforce read policy (PolicyEngine authorize) before serving.
#   * ft-cdxrr / read-policy — the read-path authorization gate is fail-closed
#     (a denied pane read returns an error, not silently-served data).
#   * Robot/MCP Contract Doctor verdict is HONEST: no green-overclaim
#     (pass_with_tracked_exceptions, local cargo is not proof, tracked
#     exceptions are explicitly not green claims and each owns a tracking bead).
#
# ZERO RCH / ZERO cargo: pure static analysis (grep/jq over committed source,
# the read-path redaction matrix, and the golden Contract-Doctor verdict). A
# clean exit 0 IS the proof — it does not require frankenterm-core to compile.
# Bead: ft-spjcx.
#
# Exit codes: 0 = all invariants hold; 1 = an invariant failed; 2 = missing input.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || { echo "FATAL: cannot cd to repo root"; exit 2; }

MCP_TOOLS="crates/frankenterm-core/src/mcp_tools.rs"
MCP_RES="crates/frankenterm-core/src/mcp_resources.rs"
WEB_HANDLERS="crates/frankenterm-core/src/web/handlers.rs"
MATRIX="docs/security/read-path-redaction-matrix.md"
GOLDEN="fixtures/robot-contract-doctor/golden/verdict.v1.json"

for f in "$MCP_TOOLS" "$MCP_RES" "$WEB_HANDLERS" "$MATRIX" "$GOLDEN"; do
  [[ -f "$f" ]] || { echo "FATAL: missing required input: $f"; exit 2; }
done
command -v jq >/dev/null 2>&1 || { echo "FATAL: jq not found"; exit 2; }

PASS=0
FAIL=0
pass() { printf 'PASS  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL  %s\n' "$1"; FAIL=$((FAIL + 1)); }

echo "=== MCP/robot read-path redaction + Contract-Doctor honesty contract (ft-spjcx) ==="
echo ""

# --- R1: wa.state redacts pane title + cwd + ignore_reason (ft-yj375/ft-ps9fu parity) ---
# Slice the redact_mcp_pane_state_fields helper body and require all three fields.
r1_body="$(awk '/fn redact_mcp_pane_state_fields/{f=1} f{print} f&&/^}/{exit}' "$MCP_TOOLS")"
if grep -q 'REDACTOR.redact' <<<"$r1_body" \
  && grep -q 'state.title' <<<"$r1_body" \
  && grep -q 'state.cwd' <<<"$r1_body" \
  && grep -q 'ignore_reason' <<<"$r1_body"; then
  pass "R1 wa.state redact_mcp_pane_state_fields redacts title + cwd + ignore_reason"
else
  fail "R1 wa.state metadata redaction regressed (title/cwd/ignore_reason must run through Redactor)"
fi

# --- R2: ft-ps9fu wa://swarm/scent read path enforces read policy (fail-closed) ---
# (a) the read input is a PolicyEngine ReadOutput input for an MCP actor;
# (b) the gate authorizes each pane AND returns Err on a denying decision;
# (c) the gate is actually invoked from the swarm-scent load path.
if grep -Eq 'ActionKind::ReadOutput' "$MCP_RES" \
  && grep -Eq 'ActorKind::Mcp' "$MCP_RES" \
  && grep -q 'fn swarm_scent_read_policy_input' "$MCP_RES"; then
  pass "R2a wa://swarm/scent builds a ReadOutput/Mcp PolicyInput"
else
  fail "R2a wa://swarm/scent must build PolicyInput(ReadOutput, Mcp) (ft-ps9fu read-policy)"
fi
authz_body="$(awk '/fn authorize_swarm_scent_panes/{f=1} f{print} f&&/^}/{exit}' "$MCP_RES")"
if grep -q 'engine.authorize' <<<"$authz_body" \
  && grep -Eq 'return Err|\?;' <<<"$authz_body" \
  && grep -q 'swarm_scent_policy_error' <<<"$authz_body"; then
  pass "R2b authorize_swarm_scent_panes is fail-closed (denied read returns Err)"
else
  fail "R2b swarm/scent authorize gate must reject on policy denial, not serve (ft-ps9fu/ft-cdxrr)"
fi
# def + at least one call site
authz_refs="$(grep -c 'authorize_swarm_scent_panes' "$MCP_RES")"
if [[ "$authz_refs" -ge 2 ]]; then
  pass "R2c swarm/scent authorize gate is invoked on the load path (refs=$authz_refs)"
else
  fail "R2c authorize_swarm_scent_panes is defined but never called (refs=$authz_refs) — gate is dead"
fi

# --- R3: ft-ps9fu wa://swarm/scent serialized fields are redacted ---
red_body="$(awk '/fn redact_swarm_scent_report/{f=1} f{print} f&&/^}/{exit}' "$MCP_RES")"
if grep -q 'redact_optional_string(&mut agent.cwd' <<<"$red_body" \
  && grep -q 'redact_optional_string(&mut agent.session_id' <<<"$red_body"; then
  pass "R3 wa://swarm/scent redacts agent cwd + session_id (no raw pane metadata leak)"
else
  fail "R3 wa://swarm/scent must redact agent.cwd + agent.session_id (ft-ps9fu leak fields)"
fi

# --- R4: GET /panes defense-in-depth parity (title + cwd) ---
fr_body="$(awk '/fn from_record/{f=1} f{print} f&&/^    }/{exit}' "$WEB_HANDLERS")"
if grep -q 'redactor.redact(&t)' <<<"$fr_body" && grep -q 'redactor.redact(&c)' <<<"$fr_body"; then
  pass "R4 GET /panes PaneView::from_record redacts title + cwd"
else
  fail "R4 GET /panes must redact title + cwd (read-path parity)"
fi

# --- R5: redaction matrix discipline (wa.state ✓ recorded; no untracked leak) ---
if grep -Eq '\| `wa\.state` \|.*\| ✓ \|' "$MATRIX"; then
  pass "R5a read-path matrix records wa.state as redacting (✓)"
else
  fail "R5a read-path matrix lost the wa.state ✓ row (matrix must not under-record the landed fix)"
fi
# Any data row marked ✗ / LEAK must carry a tracking bead on the same line.
leak_untracked="$(grep -nE '\| (✗|LEAK) \|' "$MATRIX" | grep -viE 'ft-[a-z0-9]' || true)"
if [[ -z "$leak_untracked" ]]; then
  pass "R5b no untracked ✗/LEAK row in the read-path matrix (no silent leak)"
else
  fail "R5b untracked leak row(s) in matrix: $leak_untracked"
fi

# --- R6: Contract-Doctor verdict honesty — NO green-overclaim ---
if jq -e '
  .overall_status == "pass_with_tracked_exceptions"
  and .contract.local_cargo_counts_as_proof == false
  and .contract.tracked_exceptions_are_not_green_claims == true
' "$GOLDEN" >/dev/null; then
  pass "R6a golden verdict is honest (pass_with_tracked_exceptions; local cargo NOT proof; exceptions != green)"
else
  fail "R6a golden verdict honesty flags regressed (would permit a green-overclaim)"
fi
# Every tracked exception must own a non-empty tracking bead (no anonymous 'pass').
if jq -e '
  (.tracked_exceptions | length) > 0
  and all(.tracked_exceptions[]; (.tracking_bead // "") | test("^ft-[a-z0-9.]+$"))
  and all(.tracked_exceptions[]; .status == "tracked_exception")
' "$GOLDEN" >/dev/null; then
  pass "R6b every tracked exception is owned (tracking_bead + status=tracked_exception)"
else
  fail "R6b a tracked exception lacks a tracking bead/status — that is a silent green-overclaim"
fi

# --- R7: honest claim boundaries are active (verdict disclaims what it does not prove) ---
# Every claim_boundaries entry must be `true` (an active disclaimer), and the set
# must include the policy-audit-completeness disclaimer relevant to ft-cdxrr-class
# (policy/classification) gaps — so the verdict never silently implies full
# policy/classification enforcement.
if jq -e '
  (.claim_boundaries | length) > 0
  and all(.claim_boundaries[]; . == true)
  and (.claim_boundaries.does_not_claim_all_policy_audit_rows_complete == true)
' "$GOLDEN" >/dev/null; then
  pass "R7 verdict claim_boundaries are all active disclaimers (incl. policy-audit completeness)"
else
  fail "R7 verdict claim_boundaries weakened — an inactive disclaimer permits an enforcement overclaim"
fi

echo ""
echo "=== Summary: ${PASS} passed, ${FAIL} failed ==="
if [[ "$FAIL" -ne 0 ]]; then
  exit 1
fi
exit 0
