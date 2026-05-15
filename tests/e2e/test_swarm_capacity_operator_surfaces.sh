#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_OPERATOR_BEAD_ID:-ft-b94bx.6}"
RUN_ID="${SWARM_CAPACITY_OPERATOR_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_operator/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
SOURCE_FILE="${ROOT_DIR}/crates/frankenterm/src/main.rs"
MCP_RESOURCE_FILE="${ROOT_DIR}/crates/frankenterm-core/src/mcp_resources.rs"
DOC_FILE="${ROOT_DIR}/docs/robot-contracts/swarm-capacity.md"
FIXTURE_DIR="${ROOT_DIR}/crates/frankenterm/tests/fixtures/golden_artifacts/swarm_capacity_operator"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_operator_surfaces.sh [--run-rust-proof]

Static fixture and privacy checks run locally. --run-rust-proof uses rch and
refuses local Cargo fallback.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-rust-proof)
      RUN_RUST_PROOF=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

emit_event() {
  local item_id="$1"
  local step="$2"
  local outcome="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local selected_worker="${7:-}"
  local cargo_reached="${8:-false}"
  local rustc_reached="${9:-false}"
  local test_execution_reached="${10:-false}"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg item_id "${item_id}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path#"${ROOT_DIR}/"}" \
    --arg selected_worker "${selected_worker}" \
    --argjson cargo_reached "${cargo_reached}" \
    --argjson rustc_reached "${rustc_reached}" \
    --argjson test_execution_reached "${test_execution_reached}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      run_id: $run_id,
      item_id: $item_id,
      step: $step,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      selected_worker: ($selected_worker | if . == "" then null else . end),
      cargo_reached: $cargo_reached,
      rustc_reached: $rustc_reached,
      test_execution_reached: $test_execution_reached
    }' >>"${LOG_FILE}"
}

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    emit_event "${command_name}" "preflight" "failed" "capacity.operator.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
    exit 1
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${label}" "preflight" "failed" "capacity.operator.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
}

emit_event "suite" "start" "running" "capacity.operator.started" "none" "${LOG_FILE}"

require_command jq
require_file "${SOURCE_FILE}" "source"
require_file "${MCP_RESOURCE_FILE}" "mcp_resource_source"
require_file "${DOC_FILE}" "doc"

for surface in status plan explain; do
  require_file "${FIXTURE_DIR}/${surface}.json" "${surface}_json_fixture"
  require_file "${FIXTURE_DIR}/${surface}.toon" "${surface}_toon_fixture"
  jq empty "${FIXTURE_DIR}/${surface}.json"
  if [[ "$(jq -r '.contract_id' "${FIXTURE_DIR}/${surface}.json")" != "ft.robot.swarm_capacity.operator.v1" ]]; then
    emit_event "${surface}" "fixture_contract" "failed" "capacity.operator.fixture_contract_invalid" "fixture_contract_invalid" "${FIXTURE_DIR}/${surface}.json"
    exit 1
  fi
  if ! grep -q "surface: ${surface}" "${FIXTURE_DIR}/${surface}.toon"; then
    emit_event "${surface}" "toon_surface" "failed" "capacity.operator.toon_surface_missing" "toon_surface_missing" "${FIXTURE_DIR}/${surface}.toon"
    exit 1
  fi
  emit_event "${surface}" "fixture_contract" "passed" "capacity.operator.fixture_present" "none" "${FIXTURE_DIR}/${surface}.json"
done

for needle in \
  "ft robot swarm-capacity status" \
  "ft robot swarm-capacity plan --add-panes N" \
  "wa://swarm-capacity/current" \
  "wa://swarm-capacity/runs/{run_id}" \
  "raw_pane_content_stored=false"; do
  if ! grep -q "${needle}" "${DOC_FILE}"; then
    emit_event "doc" "doc_contract" "failed" "capacity.operator.doc_missing" "doc_missing" "${DOC_FILE}"
    exit 1
  fi
done
emit_event "doc" "doc_contract" "passed" "capacity.operator.doc_present" "none" "${DOC_FILE}"

for needle in \
  "RobotSwarmCapacityCommands" \
  "build_robot_swarm_capacity_plan_payload" \
  "robot_swarm_capacity_sha256_prefixed" \
  "WaSwarmCapacityCurrentResource" \
  "WaSwarmCapacityRunTemplateResource"; do
  if ! grep -q "${needle}" "${SOURCE_FILE}" "${MCP_RESOURCE_FILE}"; then
    emit_event "source" "source_contract" "failed" "capacity.operator.source_missing" "source_missing" "${LOG_FILE}"
    exit 1
  fi
done
emit_event "source" "source_contract" "passed" "capacity.operator.source_present" "none" "${SOURCE_FILE}"

privacy_pattern="$(printf '%s|%s|%s' 'PROMPT_''BODY:' 'sk-''proj-ft-b94bx-''private-token' 'Cookie: ft_session''=pri''vate')"
privacy_hits="$(grep -E "${privacy_pattern}" "${DOC_FILE}" "${FIXTURE_DIR}"/*.json "${FIXTURE_DIR}"/*.toon || true)"
if [[ -n "${privacy_hits}" ]]; then
  emit_event "privacy" "sentinel_scan" "failed" "capacity.operator.privacy_fixture_leak" "privacy_violation" "${LOG_FILE}"
  printf '%s\n' "${privacy_hits}" >&2
  exit 1
fi
emit_event "privacy" "sentinel_scan" "passed" "capacity.operator.no_raw_content" "none" "${LOG_FILE}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  RCH_CANONICAL_PROJECT_ROOT="${RCH_CANONICAL_PROJECT_ROOT:-/data/projects}"
  RCH_ALIAS_PROJECT_ROOT="${RCH_ALIAS_PROJECT_ROOT:-/dp}"
  export RCH_CANONICAL_PROJECT_ROOT RCH_ALIAS_PROJECT_ROOT
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_operator" "${ROOT_DIR}"
  ensure_rch_ready

  RCH_LOG="${ARTIFACT_ROOT}/swarm_capacity_operator_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-operator-${RUN_ID}"
  emit_event "rust_proof" "cargo_test_start" "running" "capacity.operator.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm --bin ft --no-default-features --features subprocess-bridge robot_swarm_capacity_ -- --nocapture
  )
  rch_rc=$?
  set -e

  rch_meta="${RCH_LOG}.rch_meta.json"
  selected_worker="$(jq -r '.selected_worker_id // .worker_id // .selected_worker // empty' "${rch_meta}" 2>/dev/null || true)"
  cargo_reached="false"
  rustc_reached="false"
  test_reached="false"
  if grep -Eq 'Compiling|Finished|Running|test result' "${RCH_LOG}"; then
    cargo_reached="true"
  fi
  if grep -Eq 'rustc|Compiling|Finished' "${RCH_LOG}"; then
    rustc_reached="true"
  fi
  if grep -Eq 'running [0-9]+ tests|test result: ok' "${RCH_LOG}"; then
    test_reached="true"
  fi
  if [[ "${rch_rc}" -ne 0 ]]; then
    emit_event "rust_proof" "cargo_test_finish" "failed" "capacity.operator.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "cargo_test_finish" "passed" "capacity.operator.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "cargo_test_skip" "skipped" "capacity.operator.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

emit_event "suite" "finish" "passed" "capacity.operator.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
