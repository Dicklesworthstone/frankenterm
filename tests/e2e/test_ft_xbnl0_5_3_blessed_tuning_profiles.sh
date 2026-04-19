#!/usr/bin/env bash
# E2E: validate ft-xbnl0.5.3 blessed tuning profiles and operator playbook flow.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-xbnl0.5.3"
SCENARIO_ID="blessed_tuning_profiles"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
REMOTE_TARGET_DIR="/tmp/ft-$(whoami)-target"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT=1
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_5_3_blessed_tuning_profiles"

PASS=0
FAIL=0
TOTAL=0

record_command() {
  printf '%s\n' "$*" >> "${COMMANDS_FILE}"
}

write_env() {
  {
    printf 'timestamp=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf 'bead_id=%s\n' "${BEAD_ID}"
    printf 'scenario_id=%s\n' "${SCENARIO_ID}"
    printf 'correlation_id=%s\n' "${CORRELATION_ID}"
    printf 'artifact_dir=%s\n' "${ARTIFACT_DIR}"
    printf 'platform=%s\n' "$(uname -srm)"
    printf 'cwd=%s\n' "${ROOT_DIR}"
    printf 'remote_cargo_target_dir=%s\n' "${REMOTE_TARGET_DIR}"
    printf 'rch_skip_smoke_preflight=%s\n' "${RCH_SKIP_SMOKE_PREFLIGHT}"
  } > "${ENV_FILE}"
}

emit_log() {
  local step="$1"
  local status="$2"
  local duration_ms="$3"
  local message="$4"
  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "blessed-tuning-profiles" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg backend "rch" \
    --arg platform "$(uname -srm)" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg redaction "none" \
    --arg message "${message}" \
    --argjson duration_ms "${duration_ms}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      status: $status,
      duration_ms: $duration_ms,
      correlation_id: $correlation_id,
      backend: $backend,
      platform: $platform,
      artifact_dir: $artifact_dir,
      redaction: $redaction,
      message: $message
    }' >> "${STRUCTURED_LOG}"
}

record_result() {
  local step="$1"
  local ok="$2"
  local duration_ms="$3"
  local message="$4"
  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_log "${step}" "passed" "${duration_ms}" "${message}"
  else
    FAIL=$((FAIL + 1))
    emit_log "${step}" "failed" "${duration_ms}" "${message}"
  fi
}

run_checked() {
  local step="$1"
  local log_file="$2"
  shift 2
  local start_ns end_ns duration_ms
  start_ns="$(date +%s%N)"
  record_command "$*"
  if "$@" > "${log_file}" 2>&1; then
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    record_result "${step}" "true" "${duration_ms}" "${log_file}"
    return 0
  fi
  end_ns="$(date +%s%N)"
  duration_ms="$(((end_ns - start_ns) / 1000000))"
  record_result "${step}" "false" "${duration_ms}" "${log_file}"
  return 1
}

run_rch_step() {
  local step="$1"
  local log_file="$2"
  shift 2
  local start_ns end_ns duration_ms
  start_ns="$(date +%s%N)"
  record_command "rch exec -- $*"
  if run_rch_cargo_logged "${log_file}" "$@"; then
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    record_result "${step}" "true" "${duration_ms}" "${log_file}"
    return 0
  fi
  end_ns="$(date +%s%N)"
  duration_ms="$(((end_ns - start_ns) / 1000000))"
  record_result "${step}" "false" "${duration_ms}" "${log_file}"
  return 1
}

run_rch_remote_logged() {
  local output_file="$1"
  shift

  if [[ -z "${TIMEOUT_BIN:-}" ]]; then
    resolve_timeout_bin
  fi
  if [[ -z "${TIMEOUT_BIN:-}" ]]; then
    echo "timeout or gtimeout is required" >&2
    return 2
  fi

  : > "${output_file}"

  set +e
  (
    cd "${ROOT_DIR}"
    exec env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
      rch exec -- "$@"
  ) > "${output_file}" 2>&1
  local rc=$?
  set -e

  check_rch_fallback "${output_file}"
  if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
    local queue_log
    queue_log="$(rch_timeout_queue_log "${output_file}")"
    echo "RCH timeout; see ${queue_log}" >&2
  fi

  return "${rc}"
}

echo "=== ${BEAD_ID} blessed tuning profiles ==="
write_env
command -v jq >/dev/null 2>&1
command -v rch >/dev/null 2>&1
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready

SOURCE_AUDIT_LOG="${ARTIFACT_DIR}/source_audit.log"
run_checked \
  "source_audit" \
  "${SOURCE_AUDIT_LOG}" \
  bash -lc "
    set -euo pipefail
    test -f '${ROOT_DIR}/docs/ft-xbnl0-5-3-blessed-tuning-playbook.md'
    test -f '${ROOT_DIR}/docs/ft-xbnl0-5-3-blessed-tuning-profiles.json'
    test -f '${ROOT_DIR}/fixtures/e2e/blessed_tuning_profiles/fleet_10.toml'
    test -f '${ROOT_DIR}/fixtures/e2e/blessed_tuning_profiles/fleet_50.toml'
    test -f '${ROOT_DIR}/fixtures/e2e/blessed_tuning_profiles/fleet_200_plus.toml'
    rg -n 'ft-xbnl0.4.5|ft-xbnl0.4.6|ft doctor --json|fleet_200_plus' '${ROOT_DIR}/docs/ft-xbnl0-5-3-blessed-tuning-playbook.md'
  "

SYNTAX_LOG="${ARTIFACT_DIR}/shell_syntax.log"
run_checked \
  "shell_syntax" \
  "${SYNTAX_LOG}" \
  bash -lc "
    set -euo pipefail
    bash -n '${ROOT_DIR}/scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh'
    bash -n '${ROOT_DIR}/tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh'
  "

CONTRACT_LOG="${ARTIFACT_DIR}/profile_contract_check.log"
run_checked \
  "profile_contract_check" \
  "${CONTRACT_LOG}" \
  bash "${ROOT_DIR}/scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh" \
    --output "${ARTIFACT_DIR}/profile_contract_report.json"

CHECK_LOG="${ARTIFACT_DIR}/frankenterm_check.log"
if ! run_rch_step \
  "frankenterm_check" \
  "${CHECK_LOG}" \
  env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    cargo check -p frankenterm
then
  :
fi
rch_write_meta_json "${CHECK_LOG}"

read -r -d '' REMOTE_SCRIPT <<'EOF' || true
set -euo pipefail

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/ft-xbnl0-5-3.XXXXXX")"
cleanup() {
  rm -rf "${tmpdir}"
}
trap cleanup EXIT

config_path="${tmpdir}/ft.toml"
profiles_dir="${tmpdir}/profiles"
baseline_path="${tmpdir}/baseline.toml"
mkdir -p "${profiles_dir}"

cp fixtures/e2e/config_baseline.toml "${config_path}"
cp fixtures/e2e/config_baseline.toml "${baseline_path}"
cp fixtures/e2e/blessed_tuning_profiles/manifest.json "${profiles_dir}/manifest.json"
cp fixtures/e2e/blessed_tuning_profiles/fleet_10.toml "${profiles_dir}/fleet_10.toml"
cp fixtures/e2e/blessed_tuning_profiles/fleet_50.toml "${profiles_dir}/fleet_50.toml"
cp fixtures/e2e/blessed_tuning_profiles/fleet_200_plus.toml "${profiles_dir}/fleet_200_plus.toml"

list_json="${tmpdir}/profiles.json"
cargo run -p frankenterm -- config profile list --json --path "${config_path}" > "${list_json}"
jq -e 'map(.name) == ["default", "fleet_10", "fleet_50", "fleet_200_plus"]' "${list_json}" >/dev/null

validate_profile() {
  local profile="$1"
  local jq_filter="$2"
  local expected_snippet="$3"
  local diff_log="${tmpdir}/${profile}.diff.log"
  local apply_log="${tmpdir}/${profile}.apply.log"
  local validate_log="${tmpdir}/${profile}.validate.log"
  local show_json="${tmpdir}/${profile}.show.json"
  local rollback_log="${tmpdir}/${profile}.rollback.log"

  cargo run -p frankenterm -- config profile diff "${profile}" --path "${config_path}" > "${diff_log}"
  rg -F "${expected_snippet}" "${diff_log}" >/dev/null

  cargo run -p frankenterm -- config profile apply "${profile}" --path "${config_path}" > "${apply_log}"
  cargo run -p frankenterm -- config validate --path "${config_path}" > "${validate_log}"
  cargo run -p frankenterm -- config show --json --path "${config_path}" > "${show_json}"
  jq -e "${jq_filter}" "${show_json}" >/dev/null

  cargo run -p frankenterm -- config profile rollback --yes --path "${config_path}" > "${rollback_log}"
  cmp -s "${config_path}" "${baseline_path}"
}

validate_profile \
  "fleet_10" \
  '.tuning.runtime.output_coalesce_window_ms == 25 and .tuning.backpressure.warn_ratio == 0.7 and .tuning.search.max_limit == 500' \
  'output_coalesce_window_ms = 25'

validate_profile \
  "fleet_50" \
  '.tuning.runtime.output_coalesce_max_delay_ms == 225 and .tuning.patterns.max_seen_keys == 4000 and .tuning.search.default_limit == 50' \
  'output_coalesce_max_delay_ms = 225'

validate_profile \
  "fleet_200_plus" \
  '.tuning.runtime.output_coalesce_window_ms == 100 and .tuning.policy.max_tracked_panes == 1024 and .tuning.web.stream_default_max_hz == 20' \
  'max_tracked_panes = 1024'

jq -cn \
  --arg status "passed" \
  --arg config_path "${config_path}" \
  --arg profiles_dir "${profiles_dir}" \
  --arg list_json "${list_json}" \
  '{
    status: $status,
    config_path: $config_path,
    profiles_dir: $profiles_dir,
    list_json: $list_json
  }'
EOF

REMOTE_FLOW_LOG="${ARTIFACT_DIR}/profile_cli_flow.log"
record_command "rch exec -- env CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} bash -lc <profile-cli-flow>"
start_ns="$(date +%s%N)"
set +e
run_rch_remote_logged \
  "${REMOTE_FLOW_LOG}" \
  env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    bash -lc "${REMOTE_SCRIPT}"
remote_rc=$?
set -e
end_ns="$(date +%s%N)"
duration_ms="$(((end_ns - start_ns) / 1000000))"
if [[ ${remote_rc} -eq 0 ]]; then
  record_result "profile_cli_flow" "true" "${duration_ms}" "${REMOTE_FLOW_LOG}"
else
  record_result "profile_cli_flow" "false" "${duration_ms}" "${REMOTE_FLOW_LOG}"
fi
rch_write_meta_json "${REMOTE_FLOW_LOG}" "${remote_rc}"

jq -cn \
  --arg bead_id "${BEAD_ID}" \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg artifact_dir "${ARTIFACT_DIR}" \
  --arg commands_file "${COMMANDS_FILE}" \
  --arg env_file "${ENV_FILE}" \
  --arg structured_log "${STRUCTURED_LOG}" \
  --arg stdout_file "${STDOUT_FILE}" \
  --arg stderr_file "${STDERR_FILE}" \
  --arg source_audit_log "${SOURCE_AUDIT_LOG}" \
  --arg syntax_log "${SYNTAX_LOG}" \
  --arg contract_log "${CONTRACT_LOG}" \
  --arg contract_report "${ARTIFACT_DIR}/profile_contract_report.json" \
  --arg check_log "${CHECK_LOG}" \
  --arg check_meta "$(rch_log_meta_path "${CHECK_LOG}")" \
  --arg remote_flow_log "${REMOTE_FLOW_LOG}" \
  --arg remote_flow_meta "$(rch_log_meta_path "${REMOTE_FLOW_LOG}")" \
  --argjson pass_count "${PASS}" \
  --argjson fail_count "${FAIL}" \
  --argjson total_count "${TOTAL}" \
  '{
    bead_id: $bead_id,
    scenario_id: $scenario_id,
    status: (if $fail_count == 0 then "passed" else "failed" end),
    correlation_id: $correlation_id,
    artifact_dir: $artifact_dir,
    pass_count: $pass_count,
    fail_count: $fail_count,
    total_count: $total_count,
    artifacts: {
      commands: $commands_file,
      env: $env_file,
      structured_log: $structured_log,
      stdout: $stdout_file,
      stderr: $stderr_file,
      source_audit: $source_audit_log,
      shell_syntax: $syntax_log,
      profile_contract_check: $contract_log,
      profile_contract_report: $contract_report,
      frankenterm_check: $check_log,
      frankenterm_check_meta: $check_meta,
      profile_cli_flow: $remote_flow_log,
      profile_cli_flow_meta: $remote_flow_meta
    }
  }' > "${SUMMARY_FILE}"

if [[ "${FAIL}" -ne 0 ]]; then
  echo "ft-xbnl0.5.3 blessed tuning profile verification FAILED. Summary: ${SUMMARY_FILE}" >&2
  exit 1
fi

echo "ft-xbnl0.5.3 blessed tuning profile verification passed. Summary: ${SUMMARY_FILE}"
