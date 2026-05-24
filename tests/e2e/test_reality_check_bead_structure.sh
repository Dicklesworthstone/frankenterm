#!/usr/bin/env bash
# E2E: validate the reality-check bead structure conformance guard.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_DIR="${ROOT_DIR}/target/test-logs/reality-check-structure/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

VALID_BEADS="${ARTIFACT_DIR}/valid.issues.jsonl"
INVALID_BEADS="${ARTIFACT_DIR}/invalid.issues.jsonl"
VALID_JSON="${ARTIFACT_DIR}/valid.json"
INVALID_JSON="${ARTIFACT_DIR}/invalid.json"
INVALID_REPORT="${ARTIFACT_DIR}/invalid-report.md"
LIVE_JSON="${ARTIFACT_DIR}/live.json"
LIVE_REPORT="${ARTIFACT_DIR}/live-report.md"
CANARY_RESULTS="${ARTIFACT_DIR}/canary-results.jsonl"
CANARY_SUMMARY="${ARTIFACT_DIR}/validator-canary.json"
SUMMARY="${ARTIFACT_DIR}/summary.json"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"

emit_log() {
  local step="$1" outcome="$2" reason="$3" artifact="$4"
  jq -cn \
    --arg ts "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg run_id "${RUN_ID}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg reason "${reason}" \
    --arg artifact "${artifact}" \
    '{ts:$ts, run_id:$run_id, step:$step, outcome:$outcome, reason:$reason, artifact:$artifact}' \
    >> "${STRUCTURED_LOG}"
}

assert_cli_error() {
  local step="$1" expected_text="$2"
  shift 2
  local out rc
  set +e
  out=$(scripts/check-reality-check-bead-structure.sh "$@" 2>&1 >/dev/null)
  rc=$?
  set -e
  if [[ "${rc}" -ne 2 || "${out}" != *"${expected_text}"* || "${out}" == *"unbound variable"* ]]; then
    printf 'FAIL %s: rc=%s output=%s\n' "${step}" "${rc}" "${out}" >&2
    emit_log "${step}" "failed" "cli_error_contract_mismatch" "${STRUCTURED_LOG}"
    exit 1
  fi
  emit_log "${step}" "passed" "clean_cli_error" "${STRUCTURED_LOG}"
}

cat > "${VALID_BEADS}" <<'JSONL'
{"id":"ft-fixture","title":"fixture epic","status":"open","description":"proof_category: process"}
{"id":"ft-fixture.1","title":"well-formed fixture child","status":"closed","created_at":"2026-05-12T19:00:00Z","description":"Background: fixture.\n\nWhy this matters: fixture.\n\nAcceptance criteria: fixture.\n\nReferences: fixture.\n\n### Test companion\nfixture.\n\n### Operator surface\nfixture.\n\n### Degradation behavior\nfixture.\n\n### Proof category\n4 (conformance)\n\nproof_category: 4 (conformance)","comments":[{"text":"G55 affected-bead audit: verified docs/example and command output."}]}
JSONL

cat > "${INVALID_BEADS}" <<'JSONL'
{"id":"ft-fixture","title":"fixture epic","status":"open","description":"proof_category: process"}
{"id":"ft-fixture.1","title":"silent close canary","status":"closed","created_at":"2026-05-12T19:00:00Z","description":"Closed without the template.","comments":[]}
JSONL

cd "${ROOT_DIR}"

assert_cli_error "missing_epic_id_value" "error: --epic-id requires a value" --epic-id
assert_cli_error "missing_write_report_value" "error: --write-report requires a value" --write-report

scripts/check-reality-check-bead-structure.sh \
  --beads "${VALID_BEADS}" \
  --epic-id ft-fixture \
  --strict-all \
  --json > "${VALID_JSON}"
jq -e '.ok == true and .summary.error_count == 0' "${VALID_JSON}" >/dev/null
emit_log "valid_fixture" "passed" "accepted_well_formed_fixture" "${VALID_JSON}"

set +e
scripts/check-reality-check-bead-structure.sh \
  --beads "${INVALID_BEADS}" \
  --epic-id ft-fixture \
  --strict-all \
  --write-report "${INVALID_REPORT}" \
  --json > "${INVALID_JSON}"
invalid_rc=$?
set -e
if [[ "${invalid_rc}" -eq 0 ]]; then
  emit_log "silent_close_canary" "failed" "invalid_fixture_was_accepted" "${INVALID_JSON}"
  exit 1
fi
jq -e '
  .ok == false
  and (.violations | map(select(.kind == "missing_proof_category")) | length) >= 1
  and (.violations | map(select(.kind == "missing_closeout_evidence_comment")) | length) >= 1
' "${INVALID_JSON}" >/dev/null
test -s "${INVALID_REPORT}"
emit_log "silent_close_canary" "passed" "rejected_missing_template_and_comment" "${INVALID_JSON}"

canary_pass=0
canary_total=0
while IFS='|' read -r fixture expected_status expected_kinds; do
  [[ -n "${fixture}" ]] || continue
  canary_total=$((canary_total + 1))
  canary_json="${ARTIFACT_DIR}/canary-${fixture%.jsonl}.json"
  set +e
  scripts/check-reality-check-bead-structure.sh \
    --beads "tests/fixtures/bead-validator-canary/${fixture}" \
    --epic-id ft-canary \
    --strict-all \
    --json > "${canary_json}"
  canary_rc=$?
  set -e

  if [[ "${expected_status}" == "pass" ]]; then
    if [[ "${canary_rc}" -ne 0 ]]; then
      emit_log "validator_canary_${fixture}" "failed" "expected_pass_but_failed" "${canary_json}"
      exit 1
    fi
    jq -e '.ok == true' "${canary_json}" >/dev/null
  else
    if [[ "${canary_rc}" -eq 0 ]]; then
      emit_log "validator_canary_${fixture}" "failed" "expected_fail_but_passed" "${canary_json}"
      exit 1
    fi
    jq -e '.ok == false' "${canary_json}" >/dev/null
  fi

  IFS=',' read -r -a kinds <<< "${expected_kinds}"
  for kind in "${kinds[@]}"; do
    [[ -n "${kind}" ]] || continue
    jq -e --arg kind "${kind}" \
      '.violations | map(select(.kind == $kind)) | length >= 1' \
      "${canary_json}" >/dev/null
  done
  jq -cn \
    --arg fixture "${fixture}" \
    --arg expected_status "${expected_status}" \
    --arg artifact "${canary_json}" \
    --argjson ok true \
    '{fixture:$fixture, expected_status:$expected_status, ok:$ok, artifact:$artifact}' \
    >> "${CANARY_RESULTS}"
  canary_pass=$((canary_pass + 1))
  emit_log "validator_canary_${fixture}" "passed" "fixture_verdict_matched_expectation" "${canary_json}"
done <<'EOF'
missing_background.jsonl|fail|missing_section
missing_acceptance.jsonl|fail|missing_section
missing_test_companion.jsonl|fail|missing_section
missing_operator_surface.jsonl|fail|missing_section
missing_degradation.jsonl|fail|missing_section
missing_proof_category.jsonl|fail|missing_proof_category
invalid_proof_category.jsonl|fail|unknown_proof_category
closed_without_audit_comment.jsonl|fail|missing_closeout_evidence_comment
foreign_language_description.jsonl|pass|parse_warning
degenerate_short_description.jsonl|fail|degenerate_description
description_with_unicode_zero_width.jsonl|pass|
duplicate_section_headers.jsonl|pass|duplicate_section_header
notes_null.jsonl|fail|missing_notes,missing_proof_category
notes_empty_string.jsonl|fail|missing_notes,missing_proof_category
all_sections_present_valid.jsonl|pass|
EOF

jq -n \
  --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg fixture_dir "tests/fixtures/bead-validator-canary" \
  --arg results_jsonl "${CANARY_RESULTS}" \
  --argjson fixtures "${canary_total}" \
  --argjson passing "${canary_pass}" \
  '{
    schema_version: "reality_check.structure_validator_canary.v1",
    generated_at: $generated_at,
    fixture_dir: $fixture_dir,
    fixtures: $fixtures,
    passing: $passing,
    mutation_kill_rate: null,
    mutation_kill_rate_reason: "targeted fixture canary for script validator; cargo-mutants is not applicable to the shell/Python validator surface",
    results_jsonl: $results_jsonl
  }' > "${CANARY_SUMMARY}"
emit_log "validator_canary_matrix" "passed" "all_fixture_verdicts_matched_expectation" "${CANARY_SUMMARY}"

if ! git diff --quiet -- .beads/issues.jsonl || ! git diff --cached --quiet -- .beads/issues.jsonl; then
  scripts/check-reality-check-bead-structure.sh \
    --write-report "${LIVE_REPORT}" \
    --json > "${LIVE_JSON}" || true
  emit_log "live_reality_check_epic" "skipped" "beads_db_dirty_uncommitted" "${LIVE_JSON}"
else
  scripts/check-reality-check-bead-structure.sh \
    --write-report "${LIVE_REPORT}" \
    --json > "${LIVE_JSON}"
  jq -e '.ok == true and .summary.error_count == 0' "${LIVE_JSON}" >/dev/null
  test -s "${LIVE_REPORT}"
  emit_log "live_reality_check_epic" "passed" "live_epic_has_no_hard_errors" "${LIVE_JSON}"
fi

jq -n \
  --arg run_id "${RUN_ID}" \
  --arg artifact_dir "${ARTIFACT_DIR}" \
  --arg structured_log "${STRUCTURED_LOG}" \
  --arg valid_json "${VALID_JSON}" \
  --arg invalid_json "${INVALID_JSON}" \
  --arg live_json "${LIVE_JSON}" \
  --arg canary_json "${CANARY_SUMMARY}" \
  '{
    schema_version: "reality_check.bead_structure.e2e.summary.v1",
    run_id: $run_id,
    outcome: "passed",
    artifact_dir: $artifact_dir,
    structured_log: $structured_log,
    valid_json: $valid_json,
    invalid_json: $invalid_json,
    canary_json: $canary_json,
    live_json: $live_json
  }' > "${SUMMARY}"

cat "${SUMMARY}"
