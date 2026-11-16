#!/usr/bin/env bash
# ft-5eqd4.4 - policy subsystem count doc-pin E2E harness.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-5eqd4.4"
SCENARIO_ID="policy_count_doc_pin"
SURFACE="policy_diagnostics_count_drift_guard"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-5eqd4/${SCENARIO_ID}/${RUN_ID}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
HAPPY_LOG="${ARTIFACT_DIR}/happy_path.cargo.log"
MUTATED_README="${ARTIFACT_DIR}/README.mutated_wrong_number.md"
MUTATED_README_LOG="${ARTIFACT_DIR}/mutated_readme_wrong_number.cargo.log"
MUTATED_CONSTANT_LOG="${ARTIFACT_DIR}/mutated_constant_wrong.cargo.log"
TARGET_DIR="target/rch-ft-5eqd4-4-policy-count-doc-pin"

export RCH_REQUIRE_REMOTE=1
export RCH_SKIP_SMOKE_PREFLIGHT=1
export RCH_BUILD_SLOTS=2
export RCH_TEST_SLOTS=2
export RCH_CHECK_SLOTS=2
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"

mkdir -p "${ARTIFACT_DIR}"
: > "${COMMANDS_FILE}"
: > "${STRUCTURED_LOG}"
: > "${STDOUT_FILE}"
: > "${STDERR_FILE}"
exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_5eqd4_4_policy_count_doc_pin"

happy_rc=""
mutated_readme_rc=""
mutated_constant_rc=""

relative_path() {
  local path="$1"
  if [[ "${path}" == "${ROOT_DIR}/"* ]]; then
    printf '%s\n' "${path#"${ROOT_DIR}"/}"
  else
    printf '%s\n' "${path}"
  fi
}

record_command() {
  printf '%s\n' "$*" >> "${COMMANDS_FILE}"
}

emit_log() {
  local step="$1"
  local outcome="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "${SURFACE}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    --arg correlation_id "${CORRELATION_ID}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      correlation_id: $correlation_id
    }' >> "${STRUCTURED_LOG}"
}

write_env() {
  {
    printf 'timestamp=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf 'bead_id=%s\n' "${BEAD_ID}"
    printf 'scenario_id=%s\n' "${SCENARIO_ID}"
    printf 'surface=%s\n' "${SURFACE}"
    printf 'correlation_id=%s\n' "${CORRELATION_ID}"
    printf 'artifact_dir=%s\n' "${ARTIFACT_DIR}"
    printf 'platform=%s\n' "$(uname -srm)"
    printf 'cwd=%s\n' "${ROOT_DIR}"
    printf 'cargo_target_dir=%s\n' "${TARGET_DIR}"
    printf 'rch_require_remote=%s\n' "${RCH_REQUIRE_REMOTE}"
    printf 'rch_skip_smoke_preflight=%s\n' "${RCH_SKIP_SMOKE_PREFLIGHT}"
    printf 'rch_build_slots=%s\n' "${RCH_BUILD_SLOTS}"
    printf 'rch_test_slots=%s\n' "${RCH_TEST_SLOTS}"
    printf 'rch_check_slots=%s\n' "${RCH_CHECK_SLOTS}"
    printf 'rch_step_timeout_secs=%s\n' "${RCH_STEP_TIMEOUT_SECS}"
  } > "${ENV_FILE}"
}

write_summary() {
  local outcome="$1"
  local artifact_paths_json
  artifact_paths_json="$(jq -cn \
    --arg commands "$(relative_path "${COMMANDS_FILE}")" \
    --arg env "$(relative_path "${ENV_FILE}")" \
    --arg structured "$(relative_path "${STRUCTURED_LOG}")" \
    --arg stdout "$(relative_path "${STDOUT_FILE}")" \
    --arg stderr "$(relative_path "${STDERR_FILE}")" \
    --arg summary "$(relative_path "${SUMMARY_FILE}")" \
    --arg happy "$(relative_path "${HAPPY_LOG}")" \
    --arg mutated_readme "$(relative_path "${MUTATED_README_LOG}")" \
    --arg mutated_constant "$(relative_path "${MUTATED_CONSTANT_LOG}")" \
    --arg mutated_readme_source "$(relative_path "${MUTATED_README}")" \
    '[$commands, $env, $structured, $stdout, $stderr, $summary, $happy, $mutated_readme, $mutated_constant, $mutated_readme_source]')"
  jq -n \
    --arg run_id "${RUN_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg outcome "${outcome}" \
    --arg happy "${happy_rc}" \
    --arg mutated_readme "${mutated_readme_rc}" \
    --arg mutated_constant "${mutated_constant_rc}" \
    --argjson artifact_paths "${artifact_paths_json}" \
    '{
      run_id: $run_id,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      exit_codes: {
        happy: ($happy | if . == "" then null else tonumber end),
        mutated_readme: ($mutated_readme | if . == "" then null else tonumber end),
        mutated_constant: ($mutated_constant | if . == "" then null else tonumber end)
      },
      artifact_paths: $artifact_paths,
      outcome: $outcome
    }' > "${SUMMARY_FILE}"
}

fail_step() {
  local step="$1"
  local reason_code="$2"
  local error_code="$3"
  local artifact_path="$4"
  emit_log "${step}" "failed" "${reason_code}" "${error_code}" "${artifact_path}"
  write_summary "failed"
  exit 1
}

normalize_reason_code() {
  printf '%s\n' "$1" | tr '[:upper:]' '[:lower:]' | tr '-' '_'
}

cargo_failure_reason_code() {
  local log_file="$1"
  local fallback_reason="$2"
  local meta_path reason_code wrapper_exit_code

  meta_path="$(rch_log_meta_path "${log_file}")"
  if [[ -f "${meta_path}" ]]; then
    if jq -e '.timed_out == true' "${meta_path}" >/dev/null 2>&1; then
      printf '%s\n' "rch_remote_timeout"
      return 0
    fi
    if jq -e '.fail_open_detected == true' "${meta_path}" >/dev/null 2>&1; then
      printf '%s\n' "rch_fail_open_detected"
      return 0
    fi

    reason_code="$(jq -r '.failure_reason_code // empty' "${meta_path}" 2>/dev/null || true)"
    if [[ -n "${reason_code}" ]]; then
      normalize_reason_code "${reason_code}"
      return 0
    fi

    wrapper_exit_code="$(jq -r '.wrapper_exit_code // empty' "${meta_path}" 2>/dev/null || true)"
    case "${wrapper_exit_code}" in
      124|137)
        printf '%s\n' "rch_remote_timeout"
        return 0
        ;;
    esac
  fi

  printf '%s\n' "${fallback_reason}"
}

fail_cargo_step() {
  local step="$1"
  local fallback_reason="$2"
  local fallback_error="$3"
  local log_file="$4"
  local reason_code error_code

  reason_code="$(cargo_failure_reason_code "${log_file}" "${fallback_reason}")"
  error_code="${fallback_error}"
  if [[ "${reason_code}" == rch_* ]]; then
    error_code="${reason_code}"
  fi
  fail_step "${step}" "${reason_code}" "${error_code}" "$(relative_path "${log_file}")"
}

expected_policy_count_failure_observed() {
  local log_file="$1"
  local expected_needle="$2"

  grep -Fq "does not advertise '${expected_needle}'" "${log_file}" \
    || grep -Fq "runtime count is" "${log_file}"
}

expect_policy_count_failure() {
  local step="$1"
  local log_file="$2"
  local expected_needle="$3"
  local rc="$4"

  if [[ "${rc}" -eq 0 ]]; then
    fail_step "${step}" "mutation_not_rejected" "mutation_not_rejected" "$(relative_path "${log_file}")"
  fi
  if ! expected_policy_count_failure_observed "${log_file}" "${expected_needle}"; then
    fail_step "${step}" "mutation_rejection_output_missing" "policy_count_drift" "$(relative_path "${log_file}")"
  fi
}

extract_mutation_status() {
  local log_file="$1"
  local status
  status="$(sed -n 's/.*child_status=\([0-9][0-9]*\).*/\1/p' "${log_file}" | head -n 1)"
  if [[ ! "${status}" =~ ^[0-9]+$ ]]; then
    printf '%s\n' ""
    return 1
  fi
  printf '%s\n' "${status}"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    fail_step "preflight.cargo" "missing_prerequisite" "missing_prerequisite" "${cmd}"
  fi
}

run_policy_count_test() {
  local log_file="$1"
  shift
  record_command "run_rch_cargo_logged ${log_file} env CARGO_TARGET_DIR=${TARGET_DIR} CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 RUSTFLAGS=-Cdebuginfo=0 $*"
  (
    cd "${ROOT_DIR}"
    run_rch_cargo_logged "${log_file}" env \
      CARGO_TARGET_DIR="${TARGET_DIR}" \
      CARGO_BUILD_JOBS=2 \
      CARGO_INCREMENTAL=0 \
      CARGO_PROFILE_DEV_DEBUG=0 \
      CARGO_PROFILE_TEST_DEBUG=0 \
      RUSTFLAGS="-Cdebuginfo=0" \
      "$@"
  )
  local rc=$?
  return "${rc}"
}

current_policy_count() {
  local count
  count="$(grep -Eo '[0-9]+-subsystem policy framework' "${ROOT_DIR}/README.md" | head -n 1 | cut -d '-' -f 1)"
  if [[ ! "${count}" =~ ^[0-9]+$ ]]; then
    fail_step "preflight.cargo" "readme_count_unparseable" "readme_unparseable" "$(relative_path "${ROOT_DIR}/README.md")"
  fi
  printf '%s\n' "${count}"
}

write_mutated_readme() {
  local original_count="$1"
  local wrong_count="$2"
  sed "s/${original_count}-subsystem policy framework/${wrong_count}-subsystem policy framework/" \
    "${ROOT_DIR}/README.md" > "${MUTATED_README}"
  if ! grep -q "${wrong_count}-subsystem policy framework" "${MUTATED_README}"; then
    fail_step "mutation.readme_wrong_number" "mutation_injection_failed" "mutation_injection_failed" "$(relative_path "${MUTATED_README}")"
  fi
}

validate_artifacts() {
  local required_file
  for required_file in \
    "${COMMANDS_FILE}" \
    "${ENV_FILE}" \
    "${STRUCTURED_LOG}" \
    "${STDOUT_FILE}" \
    "${STDERR_FILE}" \
    "${SUMMARY_FILE}" \
    "${HAPPY_LOG}" \
    "${MUTATED_README_LOG}" \
    "${MUTATED_CONSTANT_LOG}" \
    "${MUTATED_README}"
  do
    if [[ ! -f "${required_file}" ]]; then
      fail_step "result" "artifact_missing" "artifact_missing" "$(relative_path "${required_file}")"
    fi
  done

  if ! jq -e '
    select(type == "object")
    | has("timestamp")
    and has("bead_id")
    and has("scenario_id")
    and has("surface")
    and has("step")
    and has("outcome")
    and has("reason_code")
    and has("error_code")
    and has("artifact_path")
  ' "${STRUCTURED_LOG}" >/dev/null; then
    fail_step "result" "structured_log_contract_failed" "structured_log_contract_failed" "$(relative_path "${STRUCTURED_LOG}")"
  fi

  if ! jq -e \
    '.run_id and .scenario_id and .correlation_id and .exit_codes and .artifact_paths and .outcome == "passed"' \
    "${SUMMARY_FILE}" >/dev/null; then
    fail_step "result" "summary_contract_failed" "summary_contract_failed" "$(relative_path "${SUMMARY_FILE}")"
  fi
}

cd "${ROOT_DIR}"
write_env
write_summary "in_progress"

require_cmd jq
require_cmd rch
require_cmd grep
require_cmd sed

record_command "ensure_rch_ready (RCH_REQUIRE_REMOTE=${RCH_REQUIRE_REMOTE}, RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
set +e
( ensure_rch_ready )
rch_ready_rc=$?
set -e
if [[ "${rch_ready_rc}" -ne 0 ]]; then
  fail_step "preflight.cargo" "rch_preflight_failed" "rch_preflight_failed" "$(relative_path "$(rch_mirror_preflight_log_path)")"
fi
emit_log "preflight.cargo" "passed" "rch_remote_ready" "none" "$(relative_path "$(rch_probe_log_path)")"

policy_count="$(current_policy_count)"
wrong_count="$((policy_count + 1))"
write_mutated_readme "${policy_count}" "${wrong_count}"

set +e
run_policy_count_test \
  "${HAPPY_LOG}" \
  cargo test -p frankenterm-core --test policy_subsystem_count_doc_pin -- --nocapture
happy_rc=$?
set -e
if [[ "${happy_rc}" -ne 0 ]]; then
  fail_cargo_step "verify.happy_path" "policy_count_guard_failed" "policy_count_drift" "${HAPPY_LOG}"
fi
emit_log "verify.happy_path" "passed" "policy_count_guard_passed" "none" "$(relative_path "${HAPPY_LOG}")"
write_summary "in_progress"

if ! grep -F "mutation_proof=readme_wrong_number" "${HAPPY_LOG}" > "${MUTATED_README_LOG}"; then
  fail_step "mutation.readme_wrong_number" "mutation_proof_missing" "mutation_proof_missing" "$(relative_path "${HAPPY_LOG}")"
fi
mutated_readme_rc="$(extract_mutation_status "${MUTATED_README_LOG}")" \
  || fail_step "mutation.readme_wrong_number" "mutation_status_missing" "mutation_status_missing" "$(relative_path "${MUTATED_README_LOG}")"
if [[ "${mutated_readme_rc}" -eq 0 ]]; then
  fail_step "mutation.readme_wrong_number" "mutation_not_rejected" "mutation_not_rejected" "$(relative_path "${MUTATED_README_LOG}")"
fi
emit_log "mutation.readme_wrong_number" "mutated" "expected_failure_detected" "none" "$(relative_path "${MUTATED_README_LOG}")"
write_summary "in_progress"

if ! grep -F "mutation_proof=constant_wrong" "${HAPPY_LOG}" > "${MUTATED_CONSTANT_LOG}"; then
  fail_step "mutation.constant_wrong" "mutation_proof_missing" "mutation_proof_missing" "$(relative_path "${HAPPY_LOG}")"
fi
mutated_constant_rc="$(extract_mutation_status "${MUTATED_CONSTANT_LOG}")" \
  || fail_step "mutation.constant_wrong" "mutation_status_missing" "mutation_status_missing" "$(relative_path "${MUTATED_CONSTANT_LOG}")"
if [[ "${mutated_constant_rc}" -eq 0 ]]; then
  fail_step "mutation.constant_wrong" "mutation_not_rejected" "mutation_not_rejected" "$(relative_path "${MUTATED_CONSTANT_LOG}")"
fi
emit_log "mutation.constant_wrong" "mutated" "expected_failure_detected" "none" "$(relative_path "${MUTATED_CONSTANT_LOG}")"

write_summary "passed"
validate_artifacts
emit_log "result" "passed" "artifact_bundle_validated" "none" "$(relative_path "${SUMMARY_FILE}")"
write_summary "passed"

echo "ft-5eqd4.4 policy count doc-pin scenario PASSED."
echo "Artifacts: ${ARTIFACT_DIR}"
echo "  summary: ${SUMMARY_FILE}"
echo "  log:     ${STRUCTURED_LOG}"
