#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_BASE="${ROOT_DIR}/tests/e2e/artifacts/ft_nu4_3_3_11_setup_remote_docker"
mkdir -p "${LOG_DIR}" "${ARTIFACT_BASE}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_nu4_3_3_11_setup_remote_docker"
CORRELATION_ID="ft-nu4.3.3.11-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
SUMMARY_FILE="${ARTIFACT_BASE}/summary_${RUN_ID}.json"
TEST_ROOT="${ARTIFACT_BASE}/${RUN_ID}"
MOCK_BIN="${TEST_ROOT}/mock-bin"
MOCK_STATE="${TEST_ROOT}/mock-state"
SCENARIO_DIR="${TEST_ROOT}/scenario"
FAKE_HOME="${TEST_ROOT}/home"
HELPER="${ROOT_DIR}/scripts/e2e_setup_remote_docker.sh"

mkdir -p "${MOCK_BIN}" "${MOCK_STATE}" "${SCENARIO_DIR}" "${FAKE_HOME}/.ssh"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="${6:-}"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "setup_remote_docker.mock_e2e" \
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

fail_now() {
  local decision_path="$1"
  local reason_code="$2"
  local error_code="$3"
  local artifact_path="$4"
  local input_summary="${5:-}"

  emit_log "failed" "${decision_path}" "${reason_code}" "${error_code}" "${artifact_path}" "${input_summary}"
  jq -cn \
    --arg run_id "${RUN_ID}" \
    --arg outcome "failed" \
    --arg decision_path "${decision_path}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      run_id: $run_id,
      outcome: $outcome,
      decision_path: $decision_path,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' > "${SUMMARY_FILE}"
  exit 1
}

pass_step() {
  local decision_path="$1"
  local reason_code="$2"
  local artifact_path="$3"
  local input_summary="${4:-}"
  emit_log "passed" "${decision_path}" "${reason_code}" "none" "${artifact_path}" "${input_summary}"
}

require_artifact() {
  local file_path="$1"
  local label="$2"
  if [[ ! -f "${file_path}" ]]; then
    fail_now "${label}" "missing_artifact" "artifact_absent" "${file_path}" "${label}"
  fi
  pass_step "${label}" "artifact_present" "${file_path}" "${label}"
}

cat > "${MOCK_BIN}/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state_root="${FT_SETUP_REMOTE_MOCK_STATE:?}"
mkdir -p "${state_root}/containers"

cmd="${1:-}"
shift || true

case "${cmd}" in
  build)
    exit 0
    ;;
  run)
    name=""
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --name)
          name="$2"
          shift 2
          ;;
        -p)
          shift 2
          ;;
        -d|--rm)
          shift
          ;;
        *)
          shift
          ;;
      esac
    done
    [[ -n "${name}" ]] || exit 1
    cid="cid-${name}"
    container_dir="${state_root}/containers/${cid}"
    mkdir -p "${container_dir}"
    if [[ "${name}" == *good* ]]; then
      printf '%s\n' "2201" > "${container_dir}/port"
      printf '%s\n' "good" > "${container_dir}/kind"
    else
      printf '%s\n' "2202" > "${container_dir}/port"
      printf '%s\n' "fail" > "${container_dir}/kind"
    fi
    printf '%s\n' "${cid}"
    ;;
  port)
    cid="$1"
    port="$(cat "${state_root}/containers/${cid}/port")"
    printf '127.0.0.1:%s\n' "${port}"
    ;;
  exec)
    cid="$1"
    shift
    if printf '%s\n' "$*" | grep -q 'fail-enable'; then
      kind="$(cat "${state_root}/containers/${cid}/kind")"
      touch "${state_root}/${kind}_fail_enable"
    fi
    ;;
  logs)
    cid="$1"
    printf 'mock logs for %s\n' "${cid}"
    ;;
  inspect)
    cid="$1"
    printf '[{"Id":"%s","Mock":true}]\n' "${cid}"
    ;;
  rm)
    exit 0
    ;;
  image)
    exit 0
    ;;
  *)
    echo "unsupported docker mock command: ${cmd}" >&2
    exit 1
    ;;
esac
EOF

cat > "${MOCK_BIN}/ssh-keygen" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

out_file=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -f)
      out_file="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

[[ -n "${out_file}" ]] || exit 1
printf 'MOCK-PRIVATE-KEY\n' > "${out_file}"
printf 'MOCK-PUBLIC-KEY\n' > "${out_file}.pub"
EOF

cat > "${MOCK_BIN}/ssh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state_root="${FT_SETUP_REMOTE_MOCK_STATE:?}"
alias_name="${1:-}"
shift || true
command="${*:-}"
host_dir="${state_root}/hosts/${alias_name}"
mkdir -p "${host_dir}/.config/systemd/user" "${host_dir}/.ft-e2e-state"

service_file="${host_dir}/.config/systemd/user/frankenterm-mux-server.service"
service_enabled="${host_dir}/.ft-e2e-state/service-enabled"
linger_enabled="${host_dir}/.ft-e2e-state/linger-enabled"
fail_enable="${host_dir}/.ft-e2e-state/fail-enable"

case "${command}" in
  "echo ready")
    echo "ready"
    ;;
  "cat /etc/ft-e2e-container-sentinel")
    echo "FT_E2E_CONTAINER_OK"
    ;;
  "cat ~/.config/systemd/user/frankenterm-mux-server.service")
    cat "${service_file}"
    ;;
  "test -f ~/.ft-e2e-state/service-enabled && test -f ~/.ft-e2e-state/linger-enabled")
    test -f "${service_enabled}" && test -f "${linger_enabled}"
    ;;
  "test -f ~/.ft-e2e-state/service-enabled")
    test -f "${service_enabled}"
    ;;
  "test -f ~/.ft-e2e-state/linger-enabled")
    test -f "${linger_enabled}"
    ;;
  "test -f ~/.ft-e2e-state/fail-enable")
    test -f "${fail_enable}"
    ;;
  "test -f ~/.config/systemd/user/frankenterm-mux-server.service")
    test -f "${service_file}"
    ;;
  "systemctl --user status frankenterm-mux-server")
    if [[ -f "${service_enabled}" ]]; then
      echo "frankenterm-mux-server.service - active (mock)"
      exit 0
    fi
    echo "frankenterm-mux-server.service - inactive (mock)"
    exit 3
    ;;
  "cd ~ && tar -cf - .config/systemd/user .ft-e2e-state 2>/dev/null || true")
    printf 'mock-tar-%s\n' "${alias_name}"
    ;;
  *)
    echo "unsupported ssh mock command: ${command}" >&2
    exit 1
    ;;
esac
EOF

cat > "${MOCK_BIN}/ft" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state_root="${FT_SETUP_REMOTE_MOCK_STATE:?}"
mode=""
host_alias=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -v)
      shift
      ;;
    setup)
      shift
      ;;
    --dry-run)
      mode="dry-run"
      shift
      ;;
    --apply)
      mode="apply"
      shift
      ;;
    remote)
      host_alias="$2"
      shift 2
      ;;
    --timeout-secs)
      shift 2
      ;;
    --yes)
      shift
      ;;
    *)
      shift
      ;;
  esac
done

[[ -n "${mode}" ]] || exit 2
[[ -n "${host_alias}" ]] || exit 2

host_dir="${state_root}/hosts/${host_alias}"
mkdir -p "${host_dir}/.config/systemd/user" "${host_dir}/.ft-e2e-state"
service_file="${host_dir}/.config/systemd/user/frankenterm-mux-server.service"

cat > "${service_file}" <<SERVICE
[Unit]
Description=FrankenTerm Mux Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/frankenterm-mux-server --daemonize=false
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
SERVICE

case "${mode}" in
  dry-run)
    echo "ft setup remote ${host_alias} (dry run)"
    exit 0
    ;;
  apply)
    if [[ "${host_alias}" == *fail* ]]; then
      touch "${host_dir}/.ft-e2e-state/fail-enable"
      echo "rollback: disabled partial remote state after simulated enable failure" >&2
      exit 42
    fi
    touch "${host_dir}/.ft-e2e-state/service-enabled"
    touch "${host_dir}/.ft-e2e-state/linger-enabled"
    echo "ft setup remote ${host_alias} apply complete"
    exit 0
    ;;
esac

exit 2
EOF

chmod +x "${MOCK_BIN}/docker" "${MOCK_BIN}/ssh-keygen" "${MOCK_BIN}/ssh" "${MOCK_BIN}/ft"

emit_log "started" "setup" "begin" "none" "${HELPER}" "mocked helper execution"

if [[ ! -x "${HELPER}" ]]; then
  fail_now "preflight" "helper_missing" "missing_file" "${HELPER}" "helper script must exist"
fi
pass_step "preflight" "helper_present" "${HELPER}"

HELPER_STDOUT="${SCENARIO_DIR}/helper.stdout.log"
HELPER_STDERR="${SCENARIO_DIR}/helper.stderr.log"

set +e
(
  export PATH="${MOCK_BIN}:${PATH}"
  export HOME="${FAKE_HOME}"
  export FT_SETUP_REMOTE_MOCK_STATE="${MOCK_STATE}"
  export FT_E2E_PRESERVE_REMOTE_SETUP_TEMP=1
  bash "${HELPER}" \
    --scenario-dir "${SCENARIO_DIR}" \
    --ft-binary "${MOCK_BIN}/ft" \
    --timeout-secs 15 \
    --verbose
) >"${HELPER_STDOUT}" 2>"${HELPER_STDERR}"
HELPER_RC=$?
set -e

if [[ ${HELPER_RC} -ne 0 ]]; then
  fail_now "helper_exec" "helper_failed" "nonzero_exit" "${HELPER_STDERR}" "rc=${HELPER_RC}"
fi
pass_step "helper_exec" "helper_completed" "${HELPER_STDOUT}" "rc=0"

require_artifact "${SCENARIO_DIR}/setup_remote_dry_run.log" "dry_run_log"
require_artifact "${SCENARIO_DIR}/setup_remote_apply.log" "apply_log"
require_artifact "${SCENARIO_DIR}/setup_remote_apply_2.log" "idempotent_log"
require_artifact "${SCENARIO_DIR}/setup_remote_failure_injected.log" "failure_log"
require_artifact "${SCENARIO_DIR}/service_unit_after_apply_1.service" "service_unit_after_apply_1"
require_artifact "${SCENARIO_DIR}/service_unit_after_apply_2.service" "service_unit_after_apply_2"
require_artifact "${SCENARIO_DIR}/service_unit_after_failure.service" "service_unit_after_failure"
require_artifact "${SCENARIO_DIR}/service_status_after_failure.txt" "service_status_after_failure"
require_artifact "${SCENARIO_DIR}/failure_remote_state.json" "failure_remote_state"
require_artifact "${SCENARIO_DIR}/failure_remote_filesystem_snapshot.tar" "failure_remote_snapshot"
require_artifact "${SCENARIO_DIR}/setup_remote_docker_summary.json" "summary_json"

if [[ -s "${SCENARIO_DIR}/service_unit_idempotency.diff" ]]; then
  fail_now "idempotency" "service_unit_changed" "unexpected_diff" "${SCENARIO_DIR}/service_unit_idempotency.diff"
fi
pass_step "idempotency" "service_unit_stable" "${SCENARIO_DIR}/service_unit_idempotency.diff"

if ! grep -qi 'rollback' "${SCENARIO_DIR}/setup_remote_failure_injected.log"; then
  fail_now "rollback_hint" "missing_rollback_hint" "log_assertion_failed" "${SCENARIO_DIR}/setup_remote_failure_injected.log"
fi
pass_step "rollback_hint" "rollback_hint_present" "${SCENARIO_DIR}/setup_remote_failure_injected.log"

if ! grep -q 'inactive' "${SCENARIO_DIR}/service_status_after_failure.txt"; then
  fail_now "rollback_state" "inactive_status_missing" "status_assertion_failed" "${SCENARIO_DIR}/service_status_after_failure.txt"
fi
pass_step "rollback_state" "inactive_status_recorded" "${SCENARIO_DIR}/service_status_after_failure.txt"

if ! jq -e '.service_enabled == false and .failure_injected == true and .service_unit_present == true' \
  "${SCENARIO_DIR}/failure_remote_state.json" >/dev/null; then
  fail_now "rollback_state" "failure_state_invalid" "json_assertion_failed" "${SCENARIO_DIR}/failure_remote_state.json"
fi
pass_step "rollback_state" "failure_state_valid" "${SCENARIO_DIR}/failure_remote_state.json"

if ! jq -e '.failure_rollback_validated == true and .failure_remote_state == "failure_remote_state.json" and .failure_remote_snapshot == "failure_remote_filesystem_snapshot.tar"' \
  "${SCENARIO_DIR}/setup_remote_docker_summary.json" >/dev/null; then
  fail_now "summary_json" "summary_missing_failure_artifacts" "json_assertion_failed" "${SCENARIO_DIR}/setup_remote_docker_summary.json"
fi
pass_step "summary_json" "failure_artifacts_indexed" "${SCENARIO_DIR}/setup_remote_docker_summary.json"

jq -cn \
  --arg run_id "${RUN_ID}" \
  --arg outcome "passed" \
  --arg scenario_dir "${SCENARIO_DIR}" \
  '{
    run_id: $run_id,
    outcome: $outcome,
    scenario_dir: $scenario_dir
  }' > "${SUMMARY_FILE}"

emit_log "passed" "suite_complete" "complete" "none" "${SUMMARY_FILE}" "mocked setup-remote harness validated"
echo "PASS: ${SCENARIO_ID} (${SCENARIO_DIR})"
