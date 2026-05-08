#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_tkpgy_4_2_blast_radius_controller"
CORRELATION_ID="ft-tkpgy.4.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_tkpgy_4_2_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_tkpgy_4_2_${RUN_ID}.stdout.log"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-tkpgy-4-2-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${FT_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "${REQUESTED_CARGO_TARGET_DIR}" && "${REQUESTED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${REQUESTED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "ars.blast_radius.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logs and shared rch metadata" >&2
  exit 1
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "tkpgy_4_2"
ensure_rch_ready

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ARS token-bucket blast-radius controller verification"

emit_log \
  "passed" \
  "preflight_rch_workers" \
  "rch_shared_guard_passed" \
  "none" \
  "$(basename "$(rch_probe_log_path)")" \
  "shared rch guard reported reachable workers"

emit_log \
  "passed" \
  "preflight_rch_remote_smoke" \
  "rch_remote_smoke_passed" \
  "none" \
  "$(basename "$(rch_smoke_log_path)")" \
  "shared rch guard verified fail-closed remote cargo execution"

TEST_FILTERS=(
  "decide_fallback_on_blast_radius_limit"
  "swarm_blast_radius_allows_exactly_five_of_fifty"
  "intercept_stats_render_prometheus_includes_ars_rate_limited_metric"
  "rate_replenishes_over_time"
)

: >"${STDOUT_FILE}"
for test_filter in "${TEST_FILTERS[@]}"; do
  step_log="${LOG_DIR}/ft_tkpgy_4_2_${RUN_ID}.${test_filter}.stdout.log"
  emit_log "running" "cargo_test" "none" "none" "$(basename "${STDOUT_FILE}")" \
    "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} cargo test -p frankenterm-core ${test_filter} -- --nocapture"

  set +e
  run_rch_cargo_logged "${step_log}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core "${test_filter}" -- --nocapture
  status=$?
  set -e
  tee -a "${STDOUT_FILE}" <"${step_log}"

  if [[ ${status} -ne 0 ]]; then
    emit_log "failed" "cargo_test" "test_failure" "cargo_test_failed" "$(basename "${STDOUT_FILE}")" "exit=${status}"
    exit ${status}
  fi

done

required_markers=(
  "decide_fallback_on_blast_radius_limit ... ok"
  "swarm_blast_radius_allows_exactly_five_of_fifty ... ok"
  "intercept_stats_render_prometheus_includes_ars_rate_limited_metric ... ok"
  "rate_replenishes_over_time ... ok"
)

for marker in "${required_markers[@]}"; do
  if ! grep -q "${marker}" "${STDOUT_FILE}"; then
    emit_log "failed" "assertion_check" "missing_success_marker" "expected_test_marker_missing" "$(basename "${STDOUT_FILE}")" "Missing marker: ${marker}"
    exit 1
  fi
done

emit_log \
  "passed" \
  "blast_radius_rate_limit_and_recovery" \
  "ars_rate_limited_metric_verified" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Validated 50-sim fanout cap and ars_rate_limited metric emission"
