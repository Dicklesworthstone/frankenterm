#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_tkpgy_4_2_blast_radius_controller"
CORRELATION_ID="ft-tkpgy.4.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_tkpgy_4_2_${RUN_ID}.jsonl"
PROBE_FILE="${LOG_DIR}/ft_tkpgy_4_2_${RUN_ID}.probe.log"
STDOUT_FILE="${LOG_DIR}/ft_tkpgy_4_2_${RUN_ID}.stdout.log"

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

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ARS token-bucket blast-radius controller verification"

if ! command -v rch >/dev/null 2>&1; then
  emit_log "failed" "preflight_rch" "rch_missing" "rch_not_found" "$(basename "${LOG_FILE}")" "rch required"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  emit_log "failed" "preflight_jq" "jq_missing" "jq_not_found" "$(basename "${LOG_FILE}")" "jq required"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -ne 0 ]] || ! grep -q "✓" "${PROBE_FILE}"; then
  emit_log \
    "failed" \
    "preflight_rch_workers" \
    "rch_workers_unreachable" \
    "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" \
    "No healthy remote RCH worker available"
  exit 1
fi

TEST_CMDS=(
  "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-tkpgy-4-2 cargo test -p frankenterm-core decide_fallback_on_blast_radius_limit -- --nocapture"
  "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-tkpgy-4-2 cargo test -p frankenterm-core swarm_blast_radius_allows_exactly_five_of_fifty -- --nocapture"
  "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-tkpgy-4-2 cargo test -p frankenterm-core intercept_stats_render_prometheus_includes_ars_rate_limited_metric -- --nocapture"
  "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-tkpgy-4-2 cargo test -p frankenterm-core rate_replenishes_over_time -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  emit_log "running" "cargo_test" "none" "none" "$(basename "${STDOUT_FILE}")" "Executing: ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "${test_cmd}"
  ) 2>&1 | tee -a "${STDOUT_FILE}"
  status=${PIPESTATUS[0]}
  set -e

  if grep -q "\[RCH\] local" "${STDOUT_FILE}"; then
    emit_log "failed" "cargo_test" "rch_fallback_local" "offload_contract_violation" "$(basename "${STDOUT_FILE}")" "rch fell back to local execution"
    exit 1
  fi

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
