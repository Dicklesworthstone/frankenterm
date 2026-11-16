#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
BEAD_ID="ft-akx00.7.2"
SCENARIO_ID="domain_detach_semantics"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/artifacts/placeholder-remediation/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
TARGET_DIR="target/rch-e2e-ft-akx00-7-2-${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

exec > >(tee -a "${STDOUT_FILE}") 2> >(tee -a "${STDERR_FILE}" >&2)

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_akx00_7_2_domain_detach_semantics"
export RCH_SKIP_SMOKE_PREFLIGHT=1
ensure_rch_ready

printf 'bead_id=%s\nscenario_id=%s\ncorrelation_id=%s\n' \
  "${BEAD_ID}" "${SCENARIO_ID}" "${CORRELATION_ID}" > "${COMMANDS_FILE}"
env | sort > "${ENV_FILE}"
PLATFORM="$(uname -s)-$(uname -m)"

emit_log() {
  local step="$1"
  local status="$2"
  local duration_ms="$3"
  local command="$4"
  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "mux-domain" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg duration_ms "${duration_ms}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg backend "rch" \
    --arg platform "${PLATFORM}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg redaction "none" \
    --arg command "${command}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      status: $status,
      duration_ms: ($duration_ms | tonumber),
      correlation_id: $correlation_id,
      backend: $backend,
      platform: $platform,
      artifact_dir: $artifact_dir,
      redaction: $redaction,
      command: $command
    }' >> "${STRUCTURED_LOG}"
}

run_cargo_step() {
  local step="$1"
  shift
  local output_file="${ARTIFACT_DIR}/${step}.log"
  local command="cargo $*"
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  local start_ns
  start_ns="$(date +%s%N)"
  emit_log "${step}" "started" "0" "${command}"
  if run_rch_cargo_logged "${output_file}" env CARGO_TARGET_DIR="${TARGET_DIR}" cargo "$@"; then
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "passed" "${duration_ms}" "${command}"
  else
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "failed" "${duration_ms}" "${command}"
    jq -cn \
      --arg bead_id "${BEAD_ID}" \
      --arg scenario_id "${SCENARIO_ID}" \
      --arg step "${step}" \
      --arg command "${command}" \
      --arg artifact "${output_file}" \
      '{bead_id:$bead_id,scenario_id:$scenario_id,status:"failed",failed_step:$step,command:$command,artifact:$artifact}' \
      > "${SUMMARY_FILE}"
    exit 1
  fi
}

run_shell_step() {
  local step="$1"
  local command="$2"
  local output_file="${ARTIFACT_DIR}/${step}.log"
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  local start_ns
  start_ns="$(date +%s%N)"
  emit_log "${step}" "started" "0" "${command}"
  if (cd "${ROOT_DIR}" && eval "${command}") >"${output_file}" 2>&1; then
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "passed" "${duration_ms}" "${command}"
  else
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "failed" "${duration_ms}" "${command}"
    jq -cn \
      --arg bead_id "${BEAD_ID}" \
      --arg scenario_id "${SCENARIO_ID}" \
      --arg step "${step}" \
      --arg command "${command}" \
      --arg artifact "${output_file}" \
      '{bead_id:$bead_id,scenario_id:$scenario_id,status:"failed",failed_step:$step,command:$command,artifact:$artifact}' \
      > "${SUMMARY_FILE}"
    exit 1
  fi
}

echo "=== ${BEAD_ID} domain detach semantics verification ==="
echo "Artifacts: ${ARTIFACT_DIR}"

run_cargo_step "mux_detach_tests" test -p mux detach_ -- --nocapture
run_cargo_step "mux_check_all_targets" check -p mux --all-targets
run_cargo_step "mux_clippy_all_targets" clippy --no-deps -p mux --all-targets -- -D warnings
run_cargo_step "workspace_fmt_check" fmt --check
run_shell_step "placeholder_audit" \
  "! rg -n 'detach not implemented for (LocalDomain|TmuxDomain|RemoteSshDomain|TermWizTerminalDomain)' frankenterm/mux/src/domain.rs frankenterm/mux/src/tmux.rs frankenterm/mux/src/ssh.rs frankenterm/mux/src/termwiztermtab.rs"
run_shell_step "surface_audit" \
  "rg -n 'detach is unsupported for|tmux_domain_detach_|local_domain_detach_|remote_ssh_domain_detach_|termwiz_domain_detach_' frankenterm/mux/src/domain.rs frankenterm/mux/src/tmux.rs frankenterm/mux/src/ssh.rs frankenterm/mux/src/termwiztermtab.rs"

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
  --arg rch_probe_log "$(rch_probe_log_path)" \
  --arg rch_probe_meta "$(rch_log_meta_path "$(rch_probe_log_path)")" \
  --arg rch_smoke_log "$(rch_smoke_log_path)" \
  --arg rch_smoke_meta "$(rch_log_meta_path "$(rch_smoke_log_path)")" \
  '{bead_id:$bead_id,scenario_id:$scenario_id,status:"passed",correlation_id:$correlation_id,artifact_dir:$artifact_dir,artifacts:{commands:$commands_file,env:$env_file,structured_log:$structured_log,stdout:$stdout_file,stderr:$stderr_file,rch_probe:$rch_probe_log,rch_probe_meta:$rch_probe_meta,rch_smoke:$rch_smoke_log,rch_smoke_meta:$rch_smoke_meta}}' \
  > "${SUMMARY_FILE}"

echo "Summary: ${SUMMARY_FILE}"
