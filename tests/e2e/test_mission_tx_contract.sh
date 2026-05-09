#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_contract"
CORRELATION_ID="ft-3yptk-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_tx_contract_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/mission_tx_contract_${RUN_ID}.stdout.log"
PROOF_LEDGER_FILE="${LOG_DIR}/mission_tx_contract_${RUN_ID}.proof-ledger.jsonl"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-mission-tx-contract-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-0}"
CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
RUSTFLAGS="${RUSTFLAGS:--Cdebuginfo=0}"
export CARGO_TARGET_DIR
export CARGO_PROFILE_DEV_DEBUG
export CARGO_PROFILE_TEST_DEBUG
export CARGO_INCREMENTAL
export RUSTFLAGS
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="ft-3yptk"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

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
    --arg component "mission_tx_contract.e2e" \
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
  "mission tx contract validation (nominal + failure-injection + recovery)"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq required for structured logging"
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "mission_tx_contract"
ensure_rch_ready

emit_log \
  "running" \
  "execution_preflight" \
  "rch_shared_guard_passed" \
  "none" \
  "$(basename "$(rch_probe_log_path)")" \
  "shared rch guard reported reachable workers and fail-closed execution"

emit_log \
  "running" \
  "execution_preflight" \
  "rch_remote_smoke_passed" \
  "none" \
  "$(basename "$(rch_smoke_log_path)")" \
  "verified remote rch exec path before mission tx contract tests"

emit_log \
  "running" \
  "proof_ledger_config" \
  "proof_ledger_enabled" \
  "none" \
  "$(basename "${PROOF_LEDGER_FILE}")" \
  "proof-ledger enabled for bead=${RCH_PROOF_LEDGER_BEAD_ID}; scenario=${RCH_PROOF_LEDGER_SCENARIO_ID}"

: >"${STDOUT_FILE}"
test_target="tx_correctness_suite"
step_log="${LOG_DIR}/mission_tx_contract_${RUN_ID}.${test_target}.log"

emit_log \
  "running" \
  "nominal_path|failure_injection_path|recovery_path|determinism_property_path" \
  "representative_mission_tx_bundle" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Executing via shared rch guard: env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} cargo test -p frankenterm-core --test ${test_target} -- --nocapture"

set +e
run_rch_cargo_logged "${step_log}" \
  env CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG}" \
    CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG}" \
    CARGO_INCREMENTAL="${CARGO_INCREMENTAL}" \
    RUSTFLAGS="${RUSTFLAGS}" \
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" \
    cargo test -p frankenterm-core --test "${test_target}" -- --nocapture
status=$?
set -e
tee -a "${STDOUT_FILE}" <"${step_log}"

if [[ ${status} -ne 0 ]]; then
  emit_log \
    "failed" \
    "nominal_path|failure_injection_path|recovery_path|determinism_property_path" \
    "test_failure" \
    "cargo_test_failed" \
    "$(basename "${STDOUT_FILE}")" \
    "exit=${status}; test_target=${test_target}"
  exit ${status}
fi

required_markers=(
  "sm_commit_requires_prepared_or_committing ... ok"
  "sm_commit_accepts_prepared ... ok"
  "pipeline_full_commit_then_full_rollback ... ok"
  "pipeline_partial_commit_then_partial_rollback ... ok"
  "receipts_monotonic_through_commit ... ok"
  "receipts_continue_sequence_from_prior ... ok"
  "idempotency_full_lifecycle_fresh_commit_then_duplicate ... ok"
  "idempotency_resume_after_crash_mid_commit ... ok"
  "deterministic_replay_same_inputs_same_results ... ok"
  "reason_codes_on_failure ... ok"
  "commit_step_results_in_ordinal_order ... ok"
  "compensation_step_results_in_reverse_ordinal_order ... ok"
  "concurrent_commit_determinism ... ok"
  "concurrent_mixed_tx_non_interference ... ok"
  "pipeline_kill_switch_safe_mode_blocks_commit ... ok"
  "pipeline_pause_suspends_commit_then_resume_idempotent ... ok"
)

for marker in "${required_markers[@]}"; do
  if ! grep -q "${marker}" "${STDOUT_FILE}"; then
    emit_log \
      "failed" \
      "assertion_check" \
      "missing_success_marker" \
      "expected_test_marker_missing" \
      "$(basename "${STDOUT_FILE}")" \
      "Missing marker: ${marker}"
    exit 1
  fi
done

emit_log \
  "running" \
  "proof_ledger_validation" \
  "validator_started" \
  "none" \
  "$(basename "${PROOF_LEDGER_FILE}")" \
  "validating every proof-ledger JSONL entry emitted by shared rch guard"

validation_dir="$(rch_validate_proof_ledger_file "${PROOF_LEDGER_FILE}")"

emit_log \
  "passed" \
  "proof_ledger_validation" \
  "proof_ledger_validated" \
  "none" \
  "$(basename "${PROOF_LEDGER_FILE}")" \
  "proof-ledger entries validated; validation_dir=${validation_dir#"${ROOT_DIR}"/}"

emit_log \
  "passed" \
  "draft->planned->prepared->committing->committed|commit_partial->compensating->rolled_back|failure_injection_rejections" \
  "transaction_contract_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Transaction entities, lifecycle matrix, failure taxonomy, and invariant/property checks validated with structured logs"
