#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_1_3_migration_scoreboard"
CORRELATION_ID="ft-e34d9.10.1.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/asupersync_migration_scoreboard_${RUN_ID}.jsonl"

GENERATOR="${ROOT_DIR}/scripts/generate_asupersync_migration_scoreboard.sh"
DOC_JSON="${ROOT_DIR}/docs/asupersync-migration-scoreboard.json"
DOC_MD="${ROOT_DIR}/docs/asupersync-migration-scoreboard.md"

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
    --arg component "asupersync_migration_scoreboard.e2e" \
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

validate_scoreboard_json() {
  local scoreboard="$1"
  jq -e '.schema_version == 1' "${scoreboard}" >/dev/null || return 1
  jq -e '.bead_id == "ft-e34d9.10.1.3"' "${scoreboard}" >/dev/null || return 1
  jq -e '.counts.total > 0' "${scoreboard}" >/dev/null || return 1
  jq -e '. as $root | (.issue_progress | type == "array") and ((.issue_progress | length) == $root.counts.total)' "${scoreboard}" >/dev/null || return 1
  jq -e '.issue_progress[] | select((.evidence_hint | length) > 0)' "${scoreboard}" >/dev/null || return 1
  jq -e '.critical_path | type == "array" and length == 8' "${scoreboard}" >/dev/null || return 1
  jq -e '.risk_ledger | type == "array" and length >= 5' "${scoreboard}" >/dev/null || return 1
  jq -e '.highest_risk.score >= 12' "${scoreboard}" >/dev/null || return 1
  jq -e '.evidence_contract.required_artifacts | index("docs/asupersync-migration-scoreboard.json")' "${scoreboard}" >/dev/null || return 1
  jq -e '.evidence_contract.structured_log_fields | index("correlation_id")' "${scoreboard}" >/dev/null || return 1
  jq -e '.critical_path[] | select(.id == "ft-e34d9.10.1")' "${scoreboard}" >/dev/null || return 1
  jq -e '.critical_path[] | select(.id == "ft-e34d9.10.8")' "${scoreboard}" >/dev/null || return 1
  return 0
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-e34d9.10.1.3 migration scoreboard validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required"
  exit 1
fi

if ! command -v br >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_br" \
    "br_missing" \
    "br_not_found" \
    "$(basename "${LOG_FILE}")" \
    "br is required"
  exit 1
fi

if [[ ! -x "${GENERATOR}" ]]; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_generator" \
    "generator_not_executable" \
    "missing_generator" \
    "$(basename "${GENERATOR}")" \
    "scoreboard generator missing or not executable"
  exit 1
fi

tmp_issues="$(mktemp)"
tmp_ready="$(mktemp)"
tmp_json_a="$(mktemp)"
tmp_md_a="$(mktemp)"
tmp_json_b="$(mktemp)"
tmp_md_b="$(mktemp)"
tmp_norm_a="$(mktemp)"
tmp_norm_b="$(mktemp)"
tmp_norm_doc="$(mktemp)"
tmp_md_norm_a="$(mktemp)"
tmp_md_norm_doc="$(mktemp)"
tmp_issues_mutated="$(mktemp)"
tmp_json_mutated="$(mktemp)"
tmp_md_mutated="$(mktemp)"
tmp_negative="$(mktemp)"

cleanup() {
  rm -f \
    "${tmp_issues}" "${tmp_ready}" \
    "${tmp_json_a}" "${tmp_md_a}" \
    "${tmp_json_b}" "${tmp_md_b}" \
    "${tmp_norm_a}" "${tmp_norm_b}" "${tmp_norm_doc}" \
    "${tmp_md_norm_a}" "${tmp_md_norm_doc}" \
    "${tmp_issues_mutated}" "${tmp_json_mutated}" "${tmp_md_mutated}" \
    "${tmp_negative}"
}
trap cleanup EXIT

emit_log \
  "running" \
  "fixture_capture" \
  "snapshot_beads_state" \
  "none" \
  "none" \
  "$(basename "${tmp_issues}")" \
  "capturing beads issue and ready snapshots"

br list --json > "${tmp_issues}"
br ready --json > "${tmp_ready}"

emit_log \
  "running" \
  "unit_determinism" \
  "fixed_input_generation" \
  "none" \
  "none" \
  "$(basename "${tmp_json_a}")" \
  "running deterministic generation checks with fixed timestamp and fixtures"

FT_ASUPERSYNC_ISSUES_JSON="${tmp_issues}" \
FT_ASUPERSYNC_READY_JSON="${tmp_ready}" \
FT_ASUPERSYNC_GENERATED_AT="2026-02-25T00:00:00Z" \
  "${GENERATOR}" "${tmp_json_a}" "${tmp_md_a}" >/dev/null

FT_ASUPERSYNC_ISSUES_JSON="${tmp_issues}" \
FT_ASUPERSYNC_READY_JSON="${tmp_ready}" \
FT_ASUPERSYNC_GENERATED_AT="2026-02-25T00:00:00Z" \
  "${GENERATOR}" "${tmp_json_b}" "${tmp_md_b}" >/dev/null

jq 'del(.generated_at)' "${tmp_json_a}" > "${tmp_norm_a}"
jq 'del(.generated_at)' "${tmp_json_b}" > "${tmp_norm_b}"

if ! diff -u "${tmp_norm_a}" "${tmp_norm_b}" >/dev/null; then
  emit_log \
    "failed" \
    "unit_determinism" \
    "fixed_input_generation" \
    "non_deterministic_output" \
    "scoreboard_diff_detected" \
    "$(basename "${tmp_json_a}")" \
    "scoreboard generation is not deterministic for fixed inputs"
  exit 1
fi

if ! validate_scoreboard_json "${tmp_json_a}"; then
  emit_log \
    "failed" \
    "unit_determinism" \
    "schema_validation" \
    "invalid_scoreboard_schema" \
    "scoreboard_schema_failed" \
    "$(basename "${tmp_json_a}")" \
    "generated scoreboard failed schema validation"
  exit 1
fi

emit_log \
  "passed" \
  "unit_determinism" \
  "fixed_input_generation" \
  "deterministic_generation_validated" \
  "none" \
  "$(basename "${tmp_json_a}")" \
  "deterministic generation and schema validation passed"

emit_log \
  "running" \
  "integration_autoupdate" \
  "mutated_issue_fixture" \
  "none" \
  "none" \
  "$(basename "${tmp_issues_mutated}")" \
  "injecting fixture status change to verify scoreboard reflects bead graph updates"

jq 'map(if .id == "ft-e34d9.10.2" then .status = "in_progress" else . end)' "${tmp_issues}" > "${tmp_issues_mutated}"

FT_ASUPERSYNC_ISSUES_JSON="${tmp_issues_mutated}" \
FT_ASUPERSYNC_READY_JSON="${tmp_ready}" \
FT_ASUPERSYNC_GENERATED_AT="2026-02-25T00:00:00Z" \
  "${GENERATOR}" "${tmp_json_mutated}" "${tmp_md_mutated}" >/dev/null

if ! jq -e '.critical_path[] | select(.id == "ft-e34d9.10.2" and .status == "in_progress")' "${tmp_json_mutated}" >/dev/null; then
  emit_log \
    "failed" \
    "integration_autoupdate" \
    "mutated_issue_fixture" \
    "status_not_reflected" \
    "graph_update_not_applied" \
    "$(basename "${tmp_json_mutated}")" \
    "critical path status did not update from mutated fixture"
  exit 1
fi

emit_log \
  "passed" \
  "integration_autoupdate" \
  "mutated_issue_fixture" \
  "graph_update_reflected" \
  "none" \
  "$(basename "${tmp_json_mutated}")" \
  "scoreboard reflects mutated fixture status as expected"

emit_log \
  "running" \
  "doc_drift_guard" \
  "generated_vs_docs" \
  "none" \
  "none" \
  "$(basename "${DOC_JSON}")" \
  "comparing generated scoreboard outputs against committed docs artifacts"

jq 'del(.generated_at)' "${DOC_JSON}" > "${tmp_norm_doc}"
if ! diff -u "${tmp_norm_a}" "${tmp_norm_doc}" >/dev/null; then
  emit_log \
    "failed" \
    "doc_drift_guard" \
    "generated_vs_docs" \
    "scoreboard_doc_outdated" \
    "docs_drift_detected" \
    "$(basename "${DOC_JSON}")" \
    "docs/asupersync-migration-scoreboard.json is out of date"
  exit 1
fi

sed '/^Generated at:/d' "${tmp_md_a}" > "${tmp_md_norm_a}"
sed '/^Generated at:/d' "${DOC_MD}" > "${tmp_md_norm_doc}"
if ! diff -u "${tmp_md_norm_a}" "${tmp_md_norm_doc}" >/dev/null; then
  emit_log \
    "failed" \
    "doc_drift_guard" \
    "generated_vs_docs" \
    "scoreboard_markdown_outdated" \
    "docs_drift_detected" \
    "$(basename "${DOC_MD}")" \
    "docs/asupersync-migration-scoreboard.md is out of date"
  exit 1
fi

emit_log \
  "passed" \
  "doc_drift_guard" \
  "generated_vs_docs" \
  "docs_in_sync" \
  "none" \
  "$(basename "${DOC_JSON}")" \
  "scoreboard docs are in sync with generator output"

emit_log \
  "running" \
  "failure_injection" \
  "negative_guardrail" \
  "none" \
  "none" \
  "$(basename "${tmp_negative}")" \
  "injecting invalid scoreboard payload and expecting schema rejection"

jq '.risk_ledger = []' "${tmp_json_a}" > "${tmp_negative}"
if validate_scoreboard_json "${tmp_negative}" >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "failure_injection" \
    "negative_guardrail" \
    "guardrail_not_enforced" \
    "unexpected_negative_pass" \
    "$(basename "${tmp_negative}")" \
    "invalid scoreboard unexpectedly passed validation"
  exit 1
fi

emit_log \
  "passed" \
  "failure_injection" \
  "negative_guardrail" \
  "negative_guardrail_enforced" \
  "none" \
  "$(basename "${tmp_negative}")" \
  "invalid scoreboard correctly rejected"

emit_log \
  "running" \
  "recovery_validation" \
  "recovery_guardrail" \
  "none" \
  "none" \
  "$(basename "${tmp_json_a}")" \
  "recovery check with canonical generated scoreboard"

if ! validate_scoreboard_json "${tmp_json_a}"; then
  emit_log \
    "failed" \
    "recovery_validation" \
    "recovery_guardrail" \
    "unexpected_recovery_fail" \
    "canonical_scoreboard_rejected" \
    "$(basename "${tmp_json_a}")" \
    "canonical generated scoreboard failed recovery validation"
  exit 1
fi

emit_log \
  "passed" \
  "recovery_validation" \
  "recovery_guardrail" \
  "recovery_path_validated" \
  "none" \
  "$(basename "${tmp_json_a}")" \
  "recovery validation passed"

emit_log \
  "passed" \
  "suite_complete" \
  "fixture_capture->unit_determinism->integration_autoupdate->doc_drift_guard->failure_injection->recovery_validation" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-e34d9.10.1.3 migration scoreboard validation passed"

echo "ft-e34d9.10.1.3 scoreboard e2e validation passed. Log: ${LOG_FILE}"
