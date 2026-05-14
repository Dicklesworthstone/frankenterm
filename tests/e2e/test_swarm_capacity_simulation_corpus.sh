#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_SIMULATION_CORPUS_BEAD_ID:-ft-b94bx.4}"
RUN_ID="${SWARM_CAPACITY_SIMULATION_CORPUS_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_simulation_corpus/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
FIXTURE_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/swarm_capacity_simulation_corpus/high_scale.v1.jsonl"
DOC_FILE="${ROOT_DIR}/docs/swarm-capacity-simulation-corpus.md"
TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/swarm_capacity_simulation_corpus.rs"
RUN_RUST_PROOF=0

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

usage() {
  cat <<'USAGE'
Usage: bash tests/e2e/test_swarm_capacity_simulation_corpus.sh [--run-rust-proof]

Static JSONL smoke checks run locally. --run-rust-proof uses rch and refuses
local Cargo fallback.
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
  local scenario_id="$1"
  local domain="$2"
  local step="$3"
  local outcome="$4"
  local admission_action="$5"
  local capacity_units="$6"
  local evidence_state="$7"
  local reason_code="$8"
  local error_code="$9"
  local artifact_path="${10}"
  local selected_worker="${11:-}"
  local cargo_reached="${12:-false}"
  local rustc_reached="${13:-false}"
  local test_execution_reached="${14:-false}"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg scenario_id "${scenario_id}" \
    --arg domain "${domain}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg admission_action "${admission_action}" \
    --argjson capacity_units "${capacity_units}" \
    --arg evidence_state "${evidence_state}" \
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
      scenario_id: $scenario_id,
      domain: $domain,
      step: $step,
      outcome: $outcome,
      admission_action: $admission_action,
      capacity_units: $capacity_units,
      evidence_state: $evidence_state,
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
    emit_event "${command_name}" "static" "preflight" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
    exit 1
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    emit_event "${label}" "static" "preflight" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.artifact_missing" "missing_artifact" "${path}"
    exit 1
  fi
}

emit_event "suite" "e2e_jsonl" "start" "running" "mixed" 0 "mixed" "capacity.simulation_corpus.started" "none" "${LOG_FILE}"

require_command jq
require_file "${FIXTURE_FILE}" "fixture"
require_file "${DOC_FILE}" "doc"
require_file "${TEST_FILE}" "test"

jq empty "${FIXTURE_FILE}"
emit_event "fixture" "static" "jq_empty" "passed" "unavailable" 0 "measured" "capacity.simulation_corpus.fixture_jsonl" "none" "${FIXTURE_FILE}"

row_count="$(jq -s 'length' "${FIXTURE_FILE}")"
if [[ "${row_count}" -ne 4 ]]; then
  emit_event "fixture" "static" "row_count" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.row_count_invalid" "row_count_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "static" "row_count" "passed" "unavailable" 0 "measured" "capacity.simulation_corpus.four_scenarios" "none" "${FIXTURE_FILE}"

if ! jq -e -s '[.[].pane_count] == [50, 100, 200, 500]' "${FIXTURE_FILE}" >/dev/null; then
  emit_event "fixture" "static" "scale_coverage" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.scale_missing" "scale_missing" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "static" "scale_coverage" "passed" "unavailable" 0 "simulated" "capacity.simulation_corpus.scale_50_100_200_500" "none" "${FIXTURE_FILE}"

for feature in idle_tails build_bursts rate_limits blocker_cascades context_rotations render_resize_storms; do
  if ! jq -e -s --arg feature "${feature}" 'any(.[]; .features | index($feature))' "${FIXTURE_FILE}" >/dev/null; then
    emit_event "${feature}" "static" "feature_coverage" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.feature_missing" "feature_missing" "${FIXTURE_FILE}"
    exit 1
  fi
  emit_event "${feature}" "feature" "feature_coverage" "present" "unavailable" 0 "simulated" "capacity.simulation_corpus.feature_present" "none" "${FIXTURE_FILE}"
done

bad_hashes="$(jq -s '[.[] | select((.content_hash | test("^sha256:[0-9a-f]{64}$") | not))] | length' "${FIXTURE_FILE}")"
if [[ "${bad_hashes}" -ne 0 ]]; then
  emit_event "fixture" "static" "content_hash_shape" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.bad_hash" "bad_hash" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "static" "content_hash_shape" "passed" "unavailable" 0 "simulated" "capacity.simulation_corpus.hash_shape" "none" "${FIXTURE_FILE}"

bad_summary="$(jq -s '
  [.[] | select(
    ((.workload_mix | map(.pane_count) | add) != .pane_count) or
    ((.workload_mix | map(.pane_count * .requested_units_per_pane) | add) != .expected_summary.total_requested_units) or
    ((.expected_summary.admitted_units + .expected_summary.deferred_units + .expected_summary.throttled_units + .expected_summary.shed_units) != .expected_summary.total_requested_units) or
    ((.expected_summary.admitted_panes + .expected_summary.deferred_panes + .expected_summary.throttled_panes + .expected_summary.shed_panes) != .pane_count)
  )] | length
' "${FIXTURE_FILE}")"
if [[ "${bad_summary}" -ne 0 ]]; then
  emit_event "fixture" "static" "summary_consistency" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.summary_invalid" "summary_invalid" "${FIXTURE_FILE}"
  exit 1
fi
emit_event "fixture" "static" "summary_consistency" "passed" "unavailable" 0 "simulated" "capacity.simulation_corpus.summary_consistent" "none" "${FIXTURE_FILE}"

while IFS=$'\t' read -r scenario_id pane_count bottleneck; do
  emit_event "${scenario_id}" "scenario" "scenario_emit" "present" "mixed" "${pane_count}" "simulated" "capacity.simulation_corpus.${bottleneck}" "none" "${FIXTURE_FILE}"
done < <(jq -r '[.scenario_id, (.pane_count | tostring), .expected_bottleneck] | @tsv' "${FIXTURE_FILE}")

while IFS= read -r decision; do
  scenario_id="$(jq -r '.scenario_id' <<<"${decision}")"
  step_id="$(jq -r '.step_id' <<<"${decision}")"
  action="$(jq -r '.admission_action' <<<"${decision}")"
  capacity_units="$(jq -r '.capacity_units' <<<"${decision}")"
  evidence_state="$(jq -r '.evidence_state' <<<"${decision}")"
  reason_code="$(jq -r '.reason_code' <<<"${decision}")"
  emit_event "${scenario_id}" "decision_trace" "${step_id}" "passed" "${action}" "${capacity_units}" "${evidence_state}" "${reason_code}" "none" "${FIXTURE_FILE}"
done < <(jq -c '. as $scenario | .decision_trace[] | . + {scenario_id: $scenario.scenario_id}' "${FIXTURE_FILE}")

privacy_hits="$(grep -E 'Bearer ft-b94bx-private-token|Cookie: ft_session=private|PROMPT_BODY:|raw pane excerpt with secret|sk-proj-' \
  "${FIXTURE_FILE}" "${DOC_FILE}" "${TEST_FILE}" || true)"
if [[ -n "${privacy_hits}" ]]; then
  emit_event "privacy" "static" "sentinel_scan" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.privacy_raw_content_leak" "privacy_violation" "${LOG_FILE}"
  exit 1
fi
emit_event "privacy" "static" "sentinel_scan" "passed" "unavailable" 0 "measured" "capacity.simulation_corpus.no_raw_content" "none" "${LOG_FILE}"

if [[ "${RUN_RUST_PROOF}" -eq 1 ]]; then
  # shellcheck source=tests/e2e/lib_rch_guards.sh
  RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
  source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
  rch_init "${ARTIFACT_ROOT}" "${RUN_ID}" "swarm_capacity_simulation_corpus" "${ROOT_DIR}"
  rch_preflight_phase=1
  emit_rch_preflight_failure_row() {
    local rc="$?"
    if [[ "${rch_preflight_phase}" == "1" && "${rc}" -ne 0 ]]; then
      local worker_selection_artifact="${ARTIFACT_ROOT}/swarm_capacity_simulation_corpus_${RUN_ID}.rch_worker_selection.json"
      emit_event "rust_proof" "rch" "preflight" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.rch_preflight_blocked" "rch_preflight_failed" "${worker_selection_artifact}" "" false false false
    fi
  }
  trap emit_rch_preflight_failure_row EXIT
  ensure_rch_ready
  rch_preflight_phase=0
  trap - EXIT

  RCH_LOG="${ARTIFACT_ROOT}/swarm_capacity_simulation_corpus_${RUN_ID}.cargo_test.log"
  SAFE_BEAD_ID="${BEAD_ID//[^[:alnum:]]/-}"
  TARGET_DIR="/tmp/${SAFE_BEAD_ID}-simulation-corpus-${RUN_ID}"
  emit_event "rust_proof" "rch" "cargo_test_start" "running" "mixed" 0 "mixed" "capacity.simulation_corpus.rch_started" "none" "${RCH_LOG}" "" false false false

  set +e
  (
    run_rch_cargo_logged "${RCH_LOG}" \
      env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test -j 1 -p frankenterm-core --test swarm_capacity_simulation_corpus --no-default-features -- --nocapture
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
    emit_event "rust_proof" "rch" "cargo_test_finish" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.rch_failed" "cargo_test_failed" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
    exit "${rch_rc}"
  fi
  emit_event "rust_proof" "rch" "cargo_test_finish" "passed" "measured" 0 "measured" "capacity.simulation_corpus.rch_passed" "none" "${RCH_LOG}" "${selected_worker}" "${cargo_reached}" "${rustc_reached}" "${test_reached}"
else
  emit_event "rust_proof" "rch" "cargo_test_skip" "skipped" "unavailable" 0 "unavailable" "capacity.simulation_corpus.rch_not_requested" "none" "${LOG_FILE}" "" false false false
fi

event_count="$(jq -s 'length' "${LOG_FILE}")"
if [[ "${event_count}" -lt 24 ]]; then
  emit_event "suite" "e2e_jsonl" "event_count" "failed" "unavailable" 0 "unavailable" "capacity.simulation_corpus.too_few_events" "event_count_low" "${LOG_FILE}"
  exit 1
fi

emit_event "suite" "e2e_jsonl" "finish" "passed" "mixed" 0 "mixed" "capacity.simulation_corpus.completed" "none" "${LOG_FILE}"
printf '%s\n' "${LOG_FILE}"
