#!/usr/bin/env bash
# ft-5eqd4.5 - policy subsystem count epic convergence harness.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-5eqd4.5"
SCENARIO_ID="epic_convergence"
SURFACE="policy_diagnostics + README + AGENTS.md"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-5eqd4/convergence/${RUN_ID}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/convergence_summary.json"
CONVERGENCE_LOG="${ARTIFACT_DIR}/policy_convergence.cargo.log"
AGENTS_MATCHES="${ARTIFACT_DIR}/agents_policy_count_matches.txt"
DOCS_HISTORY="${ARTIFACT_DIR}/docs_policy_count_history.txt"
MEMORY_HISTORY="${ARTIFACT_DIR}/memory_policy_count_history.txt"
TARGET_DIR="target/rch-ft-5eqd4-5-epic-convergence"
MEMORY_FILE="${FT_5EQD4_MEMORY_FILE:-/Users/jemanuel/.codex/memories/MEMORY.md}"

export RCH_REQUIRE_REMOTE=1
export RCH_SKIP_SMOKE_PREFLIGHT=1
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-2}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-2}"
export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-2}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"

mkdir -p "${ARTIFACT_DIR}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"
: >"${STDOUT_FILE}"
: >"${STDERR_FILE}"
: >"${AGENTS_MATCHES}"
: >"${DOCS_HISTORY}"
: >"${MEMORY_HISTORY}"
exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_5eqd4_5_epic_convergence"

cargo_test_rc=""
policy_count=""
readme_matches=""
diagnostics_count=""
fault_injection_cases=""
agents_bad_count=0
docs_history_count=0
memory_history_count=0

relative_path() {
  local path="$1"
  if [[ "${path}" == "${ROOT_DIR}/"* ]]; then
    printf '%s\n' "${path#"${ROOT_DIR}"/}"
  else
    printf '%s\n' "${path}"
  fi
}

record_command() {
  printf '%s\n' "$*" >>"${COMMANDS_FILE}"
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
    }' >>"${STRUCTURED_LOG}"
}

write_env() {
  {
    printf 'timestamp=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf 'bead_id=%s\n' "${BEAD_ID}"
    printf 'scenario_id=%s\n' "${SCENARIO_ID}"
    printf 'surface=%s\n' "${SURFACE}"
    printf 'correlation_id=%s\n' "${CORRELATION_ID}"
    printf 'artifact_dir=%s\n' "${ARTIFACT_DIR}"
    printf 'cargo_target_dir=%s\n' "${TARGET_DIR}"
    printf 'memory_file=%s\n' "${MEMORY_FILE}"
    printf 'rch_require_remote=%s\n' "${RCH_REQUIRE_REMOTE}"
    printf 'rch_skip_smoke_preflight=%s\n' "${RCH_SKIP_SMOKE_PREFLIGHT}"
    printf 'rch_step_timeout_secs=%s\n' "${RCH_STEP_TIMEOUT_SECS}"
  } >"${ENV_FILE}"
}

write_summary() {
  local outcome="$1"
  jq -n \
    --arg run_id "${RUN_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg outcome "${outcome}" \
    --arg policy_count "${policy_count}" \
    --arg readme_matches "${readme_matches}" \
    --arg diagnostics_count "${diagnostics_count}" \
    --arg fault_injection_cases "${fault_injection_cases}" \
    --arg cargo_test_rc "${cargo_test_rc}" \
    --arg agents_bad_count "${agents_bad_count}" \
    --arg docs_history_count "${docs_history_count}" \
    --arg memory_history_count "${memory_history_count}" \
    --arg commands "$(relative_path "${COMMANDS_FILE}")" \
    --arg env "$(relative_path "${ENV_FILE}")" \
    --arg structured "$(relative_path "${STRUCTURED_LOG}")" \
    --arg stdout "$(relative_path "${STDOUT_FILE}")" \
    --arg stderr "$(relative_path "${STDERR_FILE}")" \
    --arg cargo_log "$(relative_path "${CONVERGENCE_LOG}")" \
    --arg cargo_meta "$(relative_path "$(rch_log_meta_path "${CONVERGENCE_LOG}")")" \
    --arg agents_matches "$(relative_path "${AGENTS_MATCHES}")" \
    --arg docs_history "$(relative_path "${DOCS_HISTORY}")" \
    --arg memory_history "$(relative_path "${MEMORY_HISTORY}")" \
    --rawfile agents_history_lines "${AGENTS_MATCHES}" \
    --rawfile docs_history_lines "${DOCS_HISTORY}" \
    --rawfile memory_history_lines "${MEMORY_HISTORY}" \
    'def num_or_null($v): if $v == "" then null else ($v | tonumber) end;
    def historical_lines($source; $text):
      [
        $text
        | split("\n")[]
        | select(length > 0)
        | {
          source: $source,
          line: .,
          disposition: "historical"
        }
      ];
    {
      run_id: $run_id,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      chosen_strategy: "A",
      final_policy_subsystem_count: num_or_null($policy_count),
      readme_policy_count_matches: num_or_null($readme_matches),
      diagnostics_count: num_or_null($diagnostics_count),
      fault_injection_cases: num_or_null($fault_injection_cases),
      exit_codes: {
        cargo_policy_convergence: num_or_null($cargo_test_rc)
      },
      historical_mentions: {
        agents_bad_count: ($agents_bad_count | tonumber),
        docs_history_count: ($docs_history_count | tonumber),
        memory_history_count: ($memory_history_count | tonumber),
        agents: historical_lines("AGENTS.md"; $agents_history_lines),
        docs: historical_lines("docs"; $docs_history_lines),
        memory: historical_lines("MEMORY.md"; $memory_history_lines)
      },
      artifact_paths: [
        $commands,
        $env,
        $structured,
        $stdout,
        $stderr,
        $cargo_log,
        $cargo_meta,
        $agents_matches,
        $docs_history,
        $memory_history
      ],
      outcome: $outcome
    }' >"${SUMMARY_FILE}"
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

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    fail_step "preflight.${cmd}" "missing_prerequisite" "missing_prerequisite" "${cmd}"
  fi
}

extract_policy_count_line() {
  sed -nE \
    's/.*epic_convergence policy_subsystem_count=([0-9]+) readme_matches=([0-9]+) diagnostics_count=([0-9]+).*/\1 \2 \3/p' \
    "${CONVERGENCE_LOG}" | tail -n 1
}

extract_fault_injection_line() {
  sed -nE \
    's/.*fault_injection_count_invariance=passed cases=([0-9]+) policy_subsystem_count=([0-9]+).*/\1 \2/p' \
    "${CONVERGENCE_LOG}" | tail -n 1
}

scan_alignment_files() {
  grep -nE '21-subsystem|[0-9]+-subsystem policy framework' "${ROOT_DIR}/AGENTS.md" \
    >"${AGENTS_MATCHES}" || true
  agents_bad_count="$(awk -v expected="${policy_count}-subsystem policy framework" 'NF && index($0, expected) == 0 { count++ } END { print count + 0 }' "${AGENTS_MATCHES}")"

  grep -RInE '21-subsystem|21 subsystems|21 policy' "${ROOT_DIR}/docs" \
    >"${DOCS_HISTORY}" || true
  docs_history_count="$(awk 'NF { count++ } END { print count + 0 }' "${DOCS_HISTORY}")"

  if [[ -f "${MEMORY_FILE}" ]]; then
    grep -nE '21-subsystem|21 subsystems|21 policy' "${MEMORY_FILE}" \
      >"${MEMORY_HISTORY}" || true
  else
    : >"${MEMORY_HISTORY}"
  fi
  memory_history_count="$(awk 'NF { count++ } END { print count + 0 }' "${MEMORY_HISTORY}")"
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
  preflight_artifact="$(rch_remote_preflight_log_path)"
  [[ -f "${preflight_artifact}" ]] || preflight_artifact="$(rch_probe_log_path)"
  fail_step "convergence.preflight" "rch_preflight_failed" "rch_preflight_failed" "$(relative_path "${preflight_artifact}")"
fi
emit_log "convergence.preflight" "passed" "rch_remote_ready" "none" "$(relative_path "$(rch_probe_log_path)")"

record_command "run_rch_cargo_logged ${CONVERGENCE_LOG} env CARGO_TARGET_DIR=${TARGET_DIR} CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 RUSTFLAGS=-Cdebuginfo=0 cargo test -p frankenterm-core --test policy_subsystem_count_doc_pin -- --nocapture"
set +e
run_rch_cargo_logged "${CONVERGENCE_LOG}" env \
  CARGO_TARGET_DIR="${TARGET_DIR}" \
  CARGO_BUILD_JOBS=2 \
  CARGO_INCREMENTAL=0 \
  CARGO_PROFILE_DEV_DEBUG=0 \
  CARGO_PROFILE_TEST_DEBUG=0 \
  RUSTFLAGS="-Cdebuginfo=0" \
  cargo test -p frankenterm-core --test policy_subsystem_count_doc_pin -- --nocapture
cargo_test_rc=$?
set -e
if [[ "${cargo_test_rc}" -ne 0 ]]; then
  fail_step "convergence.ci_smoke" "cargo_policy_convergence_failed" "policy_count_drift" "$(relative_path "${CONVERGENCE_LOG}")"
fi
emit_log "convergence.ci_smoke" "passed" "policy_count_guard_passed" "none" "$(relative_path "${CONVERGENCE_LOG}")"

policy_count_line="$(extract_policy_count_line || true)"
read -r policy_count readme_matches diagnostics_count <<<"${policy_count_line}"
if [[ ! "${policy_count}" =~ ^[0-9]+$ || ! "${readme_matches}" =~ ^[0-9]+$ || ! "${diagnostics_count}" =~ ^[0-9]+$ ]]; then
  fail_step "convergence.readme_count" "policy_count_line_missing" "policy_count_drift" "$(relative_path "${CONVERGENCE_LOG}")"
fi
if [[ "${policy_count}" != "${diagnostics_count}" ]]; then
  fail_step "convergence.enumeration_count" "policy_count_mismatch" "policy_count_drift" "$(relative_path "${CONVERGENCE_LOG}")"
fi
emit_log "convergence.readme_count" "passed" "readme_matches_runtime_count" "none" "$(relative_path "${CONVERGENCE_LOG}")"
emit_log "convergence.enumeration_count" "passed" "diagnostics_matches_runtime_count" "none" "$(relative_path "${CONVERGENCE_LOG}")"

fault_injection_line="$(extract_fault_injection_line || true)"
read -r fault_injection_cases fault_policy_count <<<"${fault_injection_line}"
if [[ "${fault_injection_cases}" != "${policy_count}" || "${fault_policy_count}" != "${policy_count}" ]]; then
  fail_step "convergence.fault_injection" "fault_injection_count_missing" "policy_count_drift" "$(relative_path "${CONVERGENCE_LOG}")"
fi
emit_log "convergence.fault_injection" "passed" "fault_injection_count_invariant" "none" "$(relative_path "${CONVERGENCE_LOG}")"

if ! grep -Fq "mutation_proof=readme_wrong_number" "${CONVERGENCE_LOG}" \
  || ! grep -Fq "mutation_proof=constant_wrong" "${CONVERGENCE_LOG}"; then
  fail_step "convergence.mutation_rejection" "mutation_proof_missing" "policy_count_drift" "$(relative_path "${CONVERGENCE_LOG}")"
fi
emit_log "convergence.mutation_rejection" "expected_failure" "mutation_rejected" "none" "$(relative_path "${CONVERGENCE_LOG}")"

scan_alignment_files
if [[ "${agents_bad_count}" -ne 0 ]]; then
  fail_step "convergence.agents_alignment" "agents_policy_count_drift" "policy_count_drift" "$(relative_path "${AGENTS_MATCHES}")"
fi
emit_log "convergence.agents_alignment" "passed" "agents_count_aligned" "none" "$(relative_path "${AGENTS_MATCHES}")"
emit_log "convergence.memory_hint" "passed" "historical_mentions_recorded" "none" "$(relative_path "${MEMORY_HISTORY}")"

write_summary "passed"
if ! jq -e '.outcome == "passed" and .final_policy_subsystem_count == .diagnostics_count' "${SUMMARY_FILE}" >/dev/null; then
  fail_step "convergence.summary" "summary_contract_failed" "summary_contract_failed" "$(relative_path "${SUMMARY_FILE}")"
fi
emit_log "convergence.summary" "passed" "convergence_summary_valid" "none" "$(relative_path "${SUMMARY_FILE}")"

echo "ft-5eqd4.5 epic convergence scenario PASSED."
echo "Artifacts: ${ARTIFACT_DIR}"
echo "  summary: ${SUMMARY_FILE}"
echo "  log:     ${STRUCTURED_LOG}"
