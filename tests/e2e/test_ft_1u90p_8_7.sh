#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1u90p.8.7 E2E: Controlled beta feedback loop evidence validation
#
# Validates:
# 1. Baseline checkpoint remains HOLD while sample sufficiency is unmet.
# 2. Negative guardrail: synthetic GO with unmet thresholds is rejected.
# 3. Recovery guardrail: synthetic GO with met thresholds is accepted.
# 4. Evidence consistency across summary, feedback log, and correlation CSV.
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
EVIDENCE_DIR="${ROOT_DIR}/evidence/wa-1u90p.8.7"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1u90p_8_7_controlled_beta_feedback"
CORRELATION_ID="ft-1u90p.8.7-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1u90p_8_7_${RUN_ID}.jsonl"

SUMMARY_FILE="${EVIDENCE_DIR}/cohort_daily_summary.json"
FEEDBACK_FILE="${EVIDENCE_DIR}/beta_feedback_log.jsonl"
CORRELATION_FILE="${EVIDENCE_DIR}/telemetry_feedback_correlation.csv"

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "resize_beta_feedback.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
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

validate_decision_summary() {
  local summary_path="$1"
  local decision thresholds issue_id reason_count

  decision="$(jq -r '.decision' "${summary_path}")"
  thresholds="$(jq -r '.sample_sufficiency.thresholds_met' "${summary_path}")"
  issue_id="$(jq -r '.issue_id' "${summary_path}")"
  reason_count="$(jq '.decision_reason | length' "${summary_path}")"

  if [[ "${issue_id}" != "ft-1u90p.8.7" ]]; then
    echo "unexpected issue_id: ${issue_id}" >&2
    return 1
  fi

  if [[ "${reason_count}" -lt 1 ]]; then
    echo "decision_reason must contain at least one entry" >&2
    return 1
  fi

  case "${decision}" in
    GO|HOLD|ROLLBACK) ;;
    *)
      echo "invalid decision: ${decision}" >&2
      return 1
      ;;
  esac

  if [[ "${decision}" == "GO" && "${thresholds}" != "true" ]]; then
    echo "GO is not allowed when sample sufficiency thresholds are unmet" >&2
    return 1
  fi

  return 0
}

validate_anomaly_taxonomy() {
  local summary_path="$1"

  jq -e '
    .anomaly_taxonomy
    | type == "array"
      and length > 0
      and all(
        .[];
        (.anomaly_id | type == "string" and length > 0)
        and (.category_code | type == "string" and test("^A[1-5]$"))
        and (.title | type == "string" and length > 0)
        and (.severity | type == "string" and test("^(critical|high|medium|low)$"))
        and (.status | type == "string" and test("^(open|investigating|mitigated|closed)$"))
        and (.blocking_decision | type == "string" and test("^(GO|HOLD|ROLLBACK)$"))
        and (.triage_owner | type == "string" and length > 0)
        and (.remediation_owner | type == "string" and length > 0)
        and (.opened_at_utc | type == "string" and length > 0)
        and (.last_updated_at_utc | type == "string" and length > 0)
        and (.summary | type == "string" and length > 0)
        and (.linked_feedback_ids | type == "array" and length > 0)
        and (.linked_artifacts | type == "array" and length > 0)
        and (.close_loop_status | type == "string" and length > 0)
        and (.close_loop_evidence | type == "array" and length > 0)
        and (.tracking_issue_ids | type == "array" and index("ft-1u90p.8.7") != null)
      )
  ' "${summary_path}" >/dev/null
}

validate_checkpoint_mirror() {
  local summary_path="$1"
  local checkpoint_path="$2"
  local anomaly_id triage_owner remediation_owner close_loop_status

  [[ -f "${checkpoint_path}" ]] || {
    echo "missing checkpoint file: ${checkpoint_path}" >&2
    return 1
  }

  while IFS=$'\t' read -r anomaly_id triage_owner remediation_owner close_loop_status; do
    if [[ -z "${anomaly_id}" ]]; then
      continue
    fi
    grep -Fq "${anomaly_id}" "${checkpoint_path}" || {
      echo "checkpoint missing anomaly id: ${anomaly_id}" >&2
      return 1
    }
    grep -Fq "${triage_owner}" "${checkpoint_path}" || {
      echo "checkpoint missing triage owner: ${triage_owner}" >&2
      return 1
    }
    grep -Fq "${remediation_owner}" "${checkpoint_path}" || {
      echo "checkpoint missing remediation owner: ${remediation_owner}" >&2
      return 1
    }
    grep -Fq "${close_loop_status}" "${checkpoint_path}" || {
      echo "checkpoint missing close-loop status: ${close_loop_status}" >&2
      return 1
    }
  done < <(
    jq -r '.anomaly_taxonomy[] | [.anomaly_id, .triage_owner, .remediation_owner, .close_loop_status] | @tsv' \
      "${summary_path}"
  )
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-1u90p.8.7 evidence guardrail validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for evidence validation"
  exit 1
fi

for artifact in "${SUMMARY_FILE}" "${FEEDBACK_FILE}" "${CORRELATION_FILE}"; do
  if [[ ! -f "${artifact}" ]]; then
    emit_log \
      "failed" \
      "suite_init" \
      "preflight_artifacts" \
      "missing_artifact" \
      "artifact_not_found" \
      "${artifact}" \
      "Required evidence artifact is missing"
    exit 1
  fi
done

CHECKPOINT_DATE="$(jq -r '.checkpoint_date' "${SUMMARY_FILE}")"
CHECKPOINT_FILE="${EVIDENCE_DIR}/decision_checkpoint_${CHECKPOINT_DATE//-/}.md"

if [[ ! -f "${CHECKPOINT_FILE}" ]]; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_artifacts" \
    "missing_checkpoint_artifact" \
    "artifact_not_found" \
    "${CHECKPOINT_FILE}" \
    "Required checkpoint artifact is missing"
  exit 1
fi

emit_log \
  "running" \
  "baseline_hold" \
  "decision_guardrail" \
  "none" \
  "none" \
  "$(basename "${SUMMARY_FILE}")" \
  "Validate current baseline checkpoint consistency"

if ! validate_decision_summary "${SUMMARY_FILE}"; then
  emit_log \
    "failed" \
    "baseline_hold" \
    "decision_guardrail" \
    "invalid_baseline_summary" \
    "summary_validation_failed" \
    "$(basename "${SUMMARY_FILE}")" \
    "Baseline summary failed structural or decision checks"
  exit 1
fi

baseline_decision="$(jq -r '.decision' "${SUMMARY_FILE}")"
baseline_thresholds="$(jq -r '.sample_sufficiency.thresholds_met' "${SUMMARY_FILE}")"
if [[ "${baseline_decision}" != "HOLD" || "${baseline_thresholds}" != "false" ]]; then
  emit_log \
    "failed" \
    "baseline_hold" \
    "decision_guardrail" \
    "unexpected_baseline_state" \
    "baseline_not_hold" \
    "$(basename "${SUMMARY_FILE}")" \
    "Expected HOLD with thresholds_met=false"
  exit 1
fi

emit_log \
  "passed" \
  "baseline_hold" \
  "decision_guardrail" \
  "baseline_hold_expected" \
  "none" \
  "$(basename "${SUMMARY_FILE}")" \
  "Baseline correctly remains HOLD while thresholds are unmet"

emit_log \
  "running" \
  "evidence_consistency" \
  "summary_csv_feedback_join" \
  "none" \
  "none" \
  "$(basename "${CORRELATION_FILE}")" \
  "Validate run_id/feedback_id correlation across evidence artifacts"

summary_run_id="$(jq -r '.telemetry_snapshot.run_id' "${SUMMARY_FILE}")"
csv_run_id="$(tail -n +2 "${CORRELATION_FILE}" | tail -n1 | cut -d',' -f2)"
csv_feedback_id="$(tail -n +2 "${CORRELATION_FILE}" | tail -n1 | cut -d',' -f3)"

if [[ -z "${summary_run_id}" || "${summary_run_id}" == "null" ]]; then
  emit_log \
    "failed" \
    "evidence_consistency" \
    "summary_csv_feedback_join" \
    "missing_run_id" \
    "invalid_summary_run_id" \
    "$(basename "${SUMMARY_FILE}")" \
    "telemetry_snapshot.run_id must be present"
  exit 1
fi

if [[ "${summary_run_id}" != "${csv_run_id}" ]]; then
  emit_log \
    "failed" \
    "evidence_consistency" \
    "summary_csv_feedback_join" \
    "run_id_mismatch" \
    "summary_csv_mismatch" \
    "$(basename "${CORRELATION_FILE}")" \
    "summary run_id (${summary_run_id}) does not match CSV (${csv_run_id})"
  exit 1
fi

if ! jq -e --arg feedback_id "${csv_feedback_id}" '.feedback_id == $feedback_id' "${FEEDBACK_FILE}" \
  >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "evidence_consistency" \
    "summary_csv_feedback_join" \
    "feedback_id_missing" \
    "feedback_join_missing" \
    "$(basename "${FEEDBACK_FILE}")" \
    "Feedback log missing CSV feedback_id=${csv_feedback_id}"
  exit 1
fi

emit_log \
  "passed" \
  "evidence_consistency" \
  "summary_csv_feedback_join" \
  "evidence_join_valid" \
  "none" \
  "$(basename "${CORRELATION_FILE}")" \
  "run_id and feedback_id correlation is consistent"

emit_log \
  "running" \
  "anomaly_taxonomy" \
  "owner_remediation_schema" \
  "none" \
  "none" \
  "$(basename "${SUMMARY_FILE}")" \
  "Validate anomaly owner/remediation schema and checkpoint mirroring"

if ! validate_anomaly_taxonomy "${SUMMARY_FILE}"; then
  emit_log \
    "failed" \
    "anomaly_taxonomy" \
    "owner_remediation_schema" \
    "invalid_anomaly_schema" \
    "anomaly_schema_validation_failed" \
    "$(basename "${SUMMARY_FILE}")" \
    "Anomaly taxonomy is missing required owner/status/evidence fields"
  exit 1
fi

if ! validate_checkpoint_mirror "${SUMMARY_FILE}" "${CHECKPOINT_FILE}"; then
  emit_log \
    "failed" \
    "anomaly_taxonomy" \
    "checkpoint_mirror" \
    "checkpoint_anomaly_mismatch" \
    "checkpoint_mirror_validation_failed" \
    "$(basename "${CHECKPOINT_FILE}")" \
    "Decision checkpoint does not mirror anomaly tracking fields"
  exit 1
fi

emit_log \
  "passed" \
  "anomaly_taxonomy" \
  "owner_remediation_schema->checkpoint_mirror" \
  "anomaly_tracking_valid" \
  "none" \
  "$(basename "${CHECKPOINT_FILE}")" \
  "Anomaly taxonomy includes owners/remediation fields and is mirrored in the checkpoint"

tmp_negative="$(mktemp)"
tmp_recovery="$(mktemp)"
tmp_anomaly_negative="$(mktemp)"
cleanup() {
  rm -f "${tmp_negative}" "${tmp_recovery}" "${tmp_anomaly_negative}"
}
trap cleanup EXIT

jq \
  '.decision = "GO"
   | .sample_sufficiency.thresholds_met = false
   | .decision_reason = ["synthetic negative case: forced GO while thresholds unmet"]' \
  "${SUMMARY_FILE}" > "${tmp_negative}"

emit_log \
  "running" \
  "negative_guardrail" \
  "promotion_guardrail_negative" \
  "none" \
  "none" \
  "$(basename "${tmp_negative}")" \
  "Forced GO with thresholds unmet should be rejected"

if validate_decision_summary "${tmp_negative}" >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "negative_guardrail" \
    "promotion_guardrail_negative" \
    "guardrail_not_enforced" \
    "unexpected_guardrail_pass" \
    "$(basename "${tmp_negative}")" \
    "Negative case unexpectedly passed"
  exit 1
fi

emit_log \
  "passed" \
  "negative_guardrail" \
  "promotion_guardrail_negative" \
  "guardrail_enforced" \
  "none" \
  "$(basename "${tmp_negative}")" \
  "Negative case correctly rejected"

jq \
  '.decision = "GO"
   | .decision_reason = ["synthetic recovery case: thresholds met, no hard no-go trigger"]
   | .sample_sufficiency.thresholds_met = true
   | .sample_sufficiency.observed.resize_events_per_day = 642
   | .sample_sufficiency.observed.alt_screen_transitions_per_day = 96
   | .sample_sufficiency.observed.sessions_per_hardware_tier_per_day = {"low": 34, "mid": 39, "high": 36}
   | .sample_sufficiency.observed.feedback_items_total = 57
   | .sample_sufficiency.observed.feedback_items_per_workflow_group =
      {"editor-heavy": 14, "long-scrollback-monitoring": 11, "high-tab-pane-churn": 16, "mixed-font-size-zoom": 16}' \
  "${SUMMARY_FILE}" > "${tmp_recovery}"

emit_log \
  "running" \
  "recovery_go" \
  "promotion_guardrail_recovery" \
  "none" \
  "none" \
  "$(basename "${tmp_recovery}")" \
  "Synthetic GO with thresholds met should be accepted"

if ! validate_decision_summary "${tmp_recovery}"; then
  emit_log \
    "failed" \
    "recovery_go" \
    "promotion_guardrail_recovery" \
    "unexpected_recovery_reject" \
    "recovery_validation_failed" \
    "$(basename "${tmp_recovery}")" \
    "Recovery case should have passed but failed"
  exit 1
fi

emit_log \
  "passed" \
  "recovery_go" \
  "promotion_guardrail_recovery" \
  "recovery_path_validated" \
  "none" \
  "$(basename "${tmp_recovery}")" \
  "Recovery path accepted when thresholds are met"

jq \
  '.anomaly_taxonomy[0].triage_owner = ""
   | .anomaly_taxonomy[0].linked_artifacts = []' \
  "${SUMMARY_FILE}" > "${tmp_anomaly_negative}"

emit_log \
  "running" \
  "anomaly_negative_guardrail" \
  "owner_remediation_schema_negative" \
  "none" \
  "none" \
  "$(basename "${tmp_anomaly_negative}")" \
  "Missing anomaly owner/evidence should be rejected"

if validate_anomaly_taxonomy "${tmp_anomaly_negative}" >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "anomaly_negative_guardrail" \
    "owner_remediation_schema_negative" \
    "anomaly_guardrail_not_enforced" \
    "unexpected_anomaly_schema_pass" \
    "$(basename "${tmp_anomaly_negative}")" \
    "Invalid anomaly schema unexpectedly passed"
  exit 1
fi

emit_log \
  "passed" \
  "anomaly_negative_guardrail" \
  "owner_remediation_schema_negative" \
  "anomaly_guardrail_enforced" \
  "none" \
  "$(basename "${tmp_anomaly_negative}")" \
  "Invalid anomaly schema correctly rejected"

emit_log \
  "passed" \
  "suite_complete" \
  "baseline->consistency->anomaly_schema->negative_guardrail->recovery_guardrail->anomaly_negative_guardrail" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-1u90p.8.7 evidence guardrail validation passed"

echo "ft-1u90p.8.7 e2e validation passed. Log: ${LOG_FILE}"
