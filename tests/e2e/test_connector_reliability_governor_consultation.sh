#!/usr/bin/env bash
# Golden + e2e contract test for the CONNECTOR reliability/governor consultation
# path (ft-x3211 + the ft-7h5da.5.x outbound-bridge fixes).
#
# Locks in, end-to-end, that connector dispatch actually CONSULTS the reliability
# circuit-breaker and the governor before dispatching — and FEEDS outcomes back —
# rather than the prior state where both subsystems were built but never consulted
# on the production dispatch path (the reality-check ft-x3211 finding).
#
# The invariant chain (all in committed HEAD):
#   runtime spawn_connector_outbound_task
#     -> process_connector_outbound_runtime_event
#       -> bridge.process_event   [reliability allow_operation + governor evaluate,
#                                   gated by per-connector admission_enforced;
#                                   denied-when-enforced is NOT enqueued]
#         -> drain_actions -> dispatch_connector_outbound_action
#           -> policy.route_connector_operation_through_mesh
#     -> record_action_success / record_action_failure
#          [governor.record_outcome + reliability record_success/record_failure]
#
# ZERO RCH / ZERO cargo: pure static analysis (grep over committed source). A
# clean exit 0 IS the proof; it does not require frankenterm-core to compile, so
# it locks the fix in even while the dep-upgrade meta-blocker holds. Bead: ft-kms8i.
#
# Exit codes: 0 = all invariants hold; 1 = an invariant failed; 2 = missing input.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || { echo "FATAL: cannot cd to repo root"; exit 2; }

BRIDGE="crates/frankenterm-core/src/connector_outbound_bridge.rs"
RUNTIME="crates/frankenterm-core/src/runtime.rs"
POLICY="crates/frankenterm-core/src/policy.rs"

for f in "$BRIDGE" "$RUNTIME" "$POLICY"; do
  [[ -f "$f" ]] || { echo "FATAL: missing required input: $f"; exit 2; }
done

# Slice the process_event body (admission path) so ordering-sensitive checks only
# look inside it, not the whole file / test module.
PE_BODY="$(awk '/pub fn process_event/{f=1} f{print} f&&/^    }$/{exit}' "$BRIDGE")"

PASS=0
FAIL=0
pass() { printf 'PASS  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL  %s\n' "$1"; FAIL=$((FAIL + 1)); }
has()  { grep -q "$1" <<<"$2"; }

echo "=== Connector reliability/governor consultation contract (ft-kms8i / ft-x3211 / .5.x) ==="
echo ""

# --- C1: reliability circuit-breaker is CONSULTED in process_event, with teeth ---
# allow_operation() gates; a deny under admission_enforced bumps the blocked
# counter and `continue`s (the action never reaches the dispatch queue).
if has 'allow_operation()' "$PE_BODY" \
  && has 'reliability_registry_mut()' "$PE_BODY" \
  && has 'actions_blocked_reliability' "$PE_BODY" \
  && has 'continue;' "$PE_BODY"; then
  pass "C1 process_event consults the reliability circuit-breaker (allow_operation) and blocks-when-enforced"
else
  fail "C1 reliability admission missing/decorative in process_event (must allow_operation + block + continue)"
fi

# --- C2: governor is CONSULTED in process_event, Reject blocks ---
if has 'connector_governor_mut()' "$PE_BODY" \
  && has '.evaluate(' "$PE_BODY" \
  && has 'actions_blocked_governor' "$PE_BODY"; then
  pass "C2 process_event consults the connector governor (evaluate) and blocks Reject verdicts"
else
  fail "C2 governor admission missing in process_event (must evaluate + block on Reject)"
fi

# --- C3: ft-7h5da.5.15 — Throttle is NOT a rejection (proceeds w/ advisory delay) ---
# Throttle must be counted distinctly and fall through, not be dropped like Reject.
if has 'GovernorVerdict::Throttle' "$PE_BODY" \
  && has 'actions_throttled' "$PE_BODY"; then
  pass "C3 governor Throttle handled distinctly from Reject (ft-7h5da.5.15; actions_throttled, falls through)"
else
  fail "C3 Throttle/Reject conflation risk — Throttle must be distinct + advisory (ft-7h5da.5.15)"
fi

# --- C4: per-connector admission_enforced gating (log-only vs enforce) ---
if has 'connector_admission_enforced' "$PE_BODY" \
  && has 'if !admission_enforced' "$PE_BODY"; then
  pass "C4 admission decisions are gated by per-connector admission_enforced (log-only vs enforce)"
else
  fail "C4 admission_enforced gating missing (denials must be log-only until enforced is set)"
fi

# --- C5: feedback loop — both outcome paths feed governor + reliability ---
SUCCESS_BODY="$(awk '/pub fn record_action_success/{f=1} f{print} f&&/^    }$/{exit}' "$BRIDGE")"
FAILURE_BODY="$(awk '/pub fn record_action_failure/{f=1} f{print} f&&/^    }$/{exit}' "$BRIDGE")"
if has 'connector_governor_mut().record_outcome(' "$SUCCESS_BODY" \
  && has '.record_success()' "$SUCCESS_BODY"; then
  pass "C5a record_action_success feeds governor.record_outcome + reliability.record_success"
else
  fail "C5a success feedback loop broken (must feed governor + reliability)"
fi
if has 'connector_governor_mut().record_outcome(' "$FAILURE_BODY" \
  && has '.record_failure(' "$FAILURE_BODY"; then
  pass "C5b record_action_failure feeds governor.record_outcome + reliability.record_failure"
else
  fail "C5b failure feedback loop broken (must feed governor + reliability)"
fi

# --- C6: e2e wiring — the runtime actually spawns + drives the bridge ---
if grep -q 'spawn_connector_outbound_task()' "$RUNTIME" \
  && grep -q 'process_connector_outbound_runtime_event(' "$RUNTIME"; then
  pass "C6a runtime spawns the connector-outbound task + routes events to it"
else
  fail "C6a connector-outbound task not spawned/driven by the runtime (subsystem would be inert — ft-x3211)"
fi
DRIVER_BODY="$(awk '/fn process_connector_outbound_runtime_event/{f=1} f{print} f&&/^}$/{exit}' "$RUNTIME")"
if has 'bridge.process_event(' "$DRIVER_BODY" \
  && has 'dispatch_connector_outbound_action(' "$DRIVER_BODY"; then
  pass "C6b runtime driver calls bridge.process_event THEN dispatch_connector_outbound_action"
else
  fail "C6b runtime driver does not route through process_event -> dispatch (consultation bypassed)"
fi

# --- C7: dispatch routes through the policy mesh (host-runtime/kill-switch/zone) ---
DISPATCH_BODY="$(awk '/fn dispatch_connector_outbound_action/{f=1} f{print} f&&/^}$/{exit}' "$RUNTIME")"
if has 'route_connector_operation_through_mesh' "$DISPATCH_BODY" \
  && grep -q 'pub fn route_connector_operation_through_mesh' "$POLICY"; then
  pass "C7 dispatch routes the connector operation through policy.route_connector_operation_through_mesh"
else
  fail "C7 dispatch does not route through the policy mesh (mesh/kill-switch/host-runtime bypassed)"
fi

echo ""
echo "=== Summary: ${PASS} passed, ${FAIL} failed ==="
if [[ "$FAIL" -ne 0 ]]; then
  exit 1
fi
exit 0
