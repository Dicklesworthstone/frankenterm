#!/usr/bin/env bash
set -euo pipefail

# ZERO-RCH e2e conformance for ft-7h5da.5.9/.5.10/.5.11/.5.13.
# This validates the production wiring and golden matrix without compiling.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUNTIME_RS="${ROOT_DIR}/crates/frankenterm-core/src/runtime.rs"
INBOUND_RS="${ROOT_DIR}/crates/frankenterm-core/src/connector_inbound_bridge.rs"
OUTBOUND_RS="${ROOT_DIR}/crates/frankenterm-core/src/connector_outbound_bridge.rs"
POLICY_RS="${ROOT_DIR}/crates/frankenterm-core/src/policy.rs"
CONFORMANCE_RS="${ROOT_DIR}/crates/frankenterm-core/tests/connector_bridge_wiring_conformance.rs"
GOLDEN_JSON="${ROOT_DIR}/crates/frankenterm-core/tests/golden/connector_bridge_wiring/conformance_matrix.json"
SELF_SCRIPT="${ROOT_DIR}/tests/e2e/test_connector_bridge_wiring_conformance.sh"

require_cmd() {
  local name="$1"
  if ! command -v "${name}" >/dev/null 2>&1; then
    echo "required command missing: ${name}" >&2
    exit 1
  fi
}

require_file() {
  local path="$1"
  if [[ ! -f "${path}" ]]; then
    echo "required file missing: ${path}" >&2
    exit 1
  fi
}

assert_contains() {
  local path="$1"
  local label="$2"
  local needle="$3"
  if ! rg -F --quiet -- "${needle}" "${path}"; then
    echo "missing ${label}: ${needle}" >&2
    echo "file: ${path}" >&2
    exit 1
  fi
}

assert_all() {
  local path="$1"
  local label="$2"
  shift 2
  local needle
  for needle in "$@"; do
    assert_contains "${path}" "${label}" "${needle}"
  done
}

require_cmd jq
require_cmd rg

for path in \
  "${RUNTIME_RS}" \
  "${INBOUND_RS}" \
  "${OUTBOUND_RS}" \
  "${POLICY_RS}" \
  "${CONFORMANCE_RS}" \
  "${GOLDEN_JSON}" \
  "${SELF_SCRIPT}"; do
  require_file "${path}"
done

if rg --quiet -n -e '(^|[;&|[:space:]])(cargo|rch)([[:space:]]|$)' "${SELF_SCRIPT}"; then
  echo "forbidden Cargo/RCH invocation found in zero-RCH connector bridge e2e script" >&2
  exit 1
fi

assert_all "${RUNTIME_RS}" "ft-7h5da.5.9 inbound production caller" \
  "ConnectorInboundBridge::new(" \
  "pub fn route_connector_signal" \
  "route_connector_signal_through_bridge" \
  "guard.route_signal(signal)"

assert_all "${RUNTIME_RS}" "ft-7h5da.5.9 inbound fail closed" \
  "ConnectorBridgeError::BridgeUnavailable" \
  "runtime was not configured with an event bus" \
  "connector inbound bridge lock poisoned"

assert_all "${INBOUND_RS}" "ft-7h5da.5.9 inbound no silent drop" \
  "ConnectorBridgeError::UnknownKindRejected" \
  "ConnectorBridgeError::PrivacyRejected" \
  "ConnectorBridgeError::PrivacyQuarantined" \
  "ConnectorBridgeError::ClassificationFailed" \
  "let delivered = self.event_bus.publish(event);" \
  "delivered_count: delivered" \
  "events_published"

assert_all "${RUNTIME_RS}" "ft-7h5da.5.10 outbound production caller" \
  "ConnectorOutboundBridge::new(" \
  "spawn_connector_outbound_task" \
  "OutboundEvent::from_runtime_event(" \
  "bridge.process_event(&outbound_event)" \
  "bridge.drain_actions()" \
  "dispatch_connector_outbound_action"

assert_all "${OUTBOUND_RS}" "ft-7h5da.5.10 governor fail-closed" \
  "use crate::connector_governor::GovernorVerdict;" \
  "governor.evaluate(&action, now_ms)" \
  "!matches!(&governor_decision.verdict, GovernorVerdict::Allow)" \
  "actions_blocked_governor" \
  "ConnectorDispatchDenialEnvelope::new(" \
  "connector.governor_denied"

assert_all "${RUNTIME_RS}" "ft-7h5da.5.10 outbound end-to-end feedback" \
  "route_connector_operation_through_mesh" \
  "record_action_success(action, now_ms)" \
  "record_action_failure(action, err.to_string(), kind, now_ms)" \
  "ConnectorErrorKind::ServiceUnavailable" \
  "ConnectorErrorKind::Permanent"

assert_all "${POLICY_RS}" "ft-7h5da.5.11 lifecycle production boundary" \
  "pub fn run_connector_lifecycle_intent" \
  "self.lifecycle_manager_mut().execute(intent, now_ms)" \
  "connector lifecycle intent executed via production boundary" \
  "connector lifecycle intent failed at production boundary"

assert_all "${POLICY_RS}" "ft-7h5da.5.11 mesh production boundary" \
  "pub fn route_connector_operation_through_mesh" \
  ".route(&routing_request, now_ms)" \
  "ConnectorOperationDispatchError::Denied" \
  "ConnectorOperationDispatchError::from_mesh_error" \
  "ConnectorOperationDispatchError::from_host_runtime_error" \
  "record_failure(crate::connector_mesh::MeshFailureEvent" \
  ".release_connector(&routing_decision.host_id)" \
  "connector operation routed through production mesh boundary"

assert_all "${CONFORMANCE_RS}" "ft-7h5da.5.13 conformance harness" \
  "connector_bridge_contract_matrix_matches_golden" \
  "assert_contract_matrix_all_true" \
  "assert_golden_json" \
  "outbound_dispatch_consults_governor_and_fails_closed" \
  "lifecycle_routes_through_policy_engine_boundary"

jq -e '
  length == 4
  and ([.[].bead] | sort == [
    "ft-7h5da.5.10",
    "ft-7h5da.5.11",
    "ft-7h5da.5.13",
    "ft-7h5da.5.9"
  ])
  and all(.[]; .production_caller_wired == true)
  and all(.[]; .fail_closed == true)
  and all(.[]; .no_silent_drop == true)
  and any(.[]; .bead == "ft-7h5da.5.10" and (.contract | contains("host-runtime feedback")))
  and any(.[]; .bead == "ft-7h5da.5.11" and (.bridge == "lifecycle_mesh"))
  and any(.[]; .bead == "ft-7h5da.5.13" and (.bridge == "conformance"))
' "${GOLDEN_JSON}" >/dev/null

echo "all_checks_passed: connector bridge wiring golden matrix and zero-RCH e2e conformance"
