#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date +"%Y%m%dT%H%M%SZ")"
BEAD_ID="ft-xbnl0.4.5"
SCENARIO_ID="swarm_soak_matrix"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
SWARM_SCRIPT="${FT_XBNL0_4_5_SWARM_SCRIPT:-${ROOT_DIR}/scripts/e2e_swarm_stress.sh}"

mkdir -p "${ARTIFACT_DIR}"

exec > >(tee -a "${STDOUT_FILE}") 2> >(tee -a "${STDERR_FILE}" >&2)

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
RCH_STEP_TIMEOUT_SECS=2400
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_4_5_swarm_soak_matrix"
export RCH_SKIP_SMOKE_PREFLIGHT=1
ensure_rch_ready

printf 'bead_id=%s\nscenario_id=%s\ncorrelation_id=%s\nswarm_script=%s\n' \
  "${BEAD_ID}" "${SCENARIO_ID}" "${CORRELATION_ID}" "${SWARM_SCRIPT}" > "${COMMANDS_FILE}"
env | sort > "${ENV_FILE}"
PLATFORM="$(uname -s)-$(uname -m)"

emit_log() {
  local step="$1"
  local status="$2"
  local duration_ms="$3"
  local message="$4"
  local command="${5:-}"
  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "swarm-soak-matrix" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg duration_ms "${duration_ms}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg backend "rch" \
    --arg platform "${PLATFORM}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg redaction "none" \
    --arg message "${message}" \
    --arg command "${command}" \
    '{
      timestamp: $timestamp,
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
      message: $message,
      command: $command
    }' >> "${STRUCTURED_LOG}"
}

write_failure_summary() {
  local step="$1"
  local command="$2"
  local artifact="$3"
  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg failed_step "${step}" \
    --arg command "${command}" \
    --arg artifact "${artifact}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      status: "failed",
      correlation_id: $correlation_id,
      artifact_dir: $artifact_dir,
      failed_step: $failed_step,
      command: $command,
      artifact: $artifact
    }' > "${SUMMARY_FILE}"
}

run_shell_step() {
  local step="$1"
  local command="$2"
  local output_file="${ARTIFACT_DIR}/${step}.log"
  local start_ns
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  start_ns="$(date +%s%N)"
  emit_log "${step}" "started" "0" "${output_file}" "${command}"
  if (cd "${ROOT_DIR}" && eval "${command}") >"${output_file}" 2>&1; then
    local end_ns
    end_ns="$(date +%s%N)"
    emit_log "${step}" "passed" "$(( (end_ns - start_ns) / 1000000 ))" "${output_file}" "${command}"
  else
    local end_ns
    end_ns="$(date +%s%N)"
    emit_log "${step}" "failed" "$(( (end_ns - start_ns) / 1000000 ))" "${output_file}" "${command}"
    write_failure_summary "${step}" "${command}" "${output_file}"
    exit 1
  fi
}

run_profile_cycle() {
  local profile="$1"
  local cycle="$2"
  local cycle_id
  local cycle_artifact_root
  local cycle_log_root
  local cycle_run_id
  local cycle_rch_artifact_dir
  local step_name
  local output_file
  local command
  local start_ns

  cycle_id="$(printf "%s_cycle_%02d" "${profile}" "${cycle}")"
  cycle_artifact_root="${ARTIFACT_DIR}/${cycle_id}/artifacts"
  cycle_log_root="${ARTIFACT_DIR}/${cycle_id}/logs"
  cycle_run_id="${RUN_ID}-${cycle_id}"
  cycle_rch_artifact_dir="${cycle_artifact_root}/${cycle_run_id}"
  step_name="${cycle_id}_run"
  output_file="${ARTIFACT_DIR}/${step_name}.log"
  printf -v command \
    'RUN_ID=%q FT_SWARM_STRESS_PROFILE=%q SWARM_STRESS_ARTIFACT_DIR_BASE=%q SWARM_STRESS_LOG_DIR=%q SWARM_STRESS_RCH_ARTIFACT_DIR=%q %q' \
    "${cycle_run_id}" \
    "${profile}" \
    "${cycle_artifact_root}" \
    "${cycle_log_root}" \
    "${cycle_rch_artifact_dir}" \
    "${SWARM_SCRIPT}"

  mkdir -p "${cycle_artifact_root}" "${cycle_log_root}"
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  start_ns="$(date +%s%N)"
  emit_log "${step_name}" "started" "0" "${output_file}" "${command}"
  if (cd "${ROOT_DIR}" && eval "${command}") >"${output_file}" 2>&1; then
    local cycle_summary="${cycle_rch_artifact_dir}/summary.json"
    local end_ns
    end_ns="$(date +%s%N)"
    if jq -e '
      (.tests_run == 8) and
      (.pane_scales == [1,50,100,200]) and
      (.metric_names | length == 8)
    ' "${cycle_summary}" >/dev/null; then
      emit_log "${step_name}" "passed" "$(( (end_ns - start_ns) / 1000000 ))" "${cycle_summary}" "${command}"
      printf '%s\n' "${cycle_summary}"
    else
      emit_log "${step_name}" "failed" "$(( (end_ns - start_ns) / 1000000 ))" "${cycle_summary}" "${command}"
      write_failure_summary "${step_name}" "${command}" "${cycle_summary}"
      exit 1
    fi
  else
    local end_ns
    end_ns="$(date +%s%N)"
    emit_log "${step_name}" "failed" "$(( (end_ns - start_ns) / 1000000 ))" "${output_file}" "${command}"
    write_failure_summary "${step_name}" "${command}" "${output_file}"
    exit 1
  fi
}

echo "=== ${BEAD_ID} swarm soak matrix ==="
echo "Artifacts: ${ARTIFACT_DIR}"

run_shell_step "source_audit" \
  "rg -n 'stress_50_panes_idle|stress_100_panes_idle|stress_200_panes_idle|stress_200_panes_active|stress_200_panes_backpressure' scripts/e2e_swarm_stress.sh crates/frankenterm-core/tests/e2e_swarm_stress_core.rs"
run_shell_step "stress_contract_test" \
  "bash tests/e2e/test_ft_1memj_30_swarm_stress.sh"
run_shell_step "swarm_script_syntax" \
  "bash -n scripts/e2e_swarm_stress.sh"
run_shell_step "wrapper_syntax" \
  "bash -n tests/e2e/test_ft_xbnl0_4_5_swarm_soak_matrix.sh"

smoke_summary="$(run_profile_cycle smoke 1)"
release_summary_1="$(run_profile_cycle release 1)"
release_summary_2="$(run_profile_cycle release 2)"
release_summary_3="$(run_profile_cycle release 3)"

run_shell_step "release_profile_consistency" \
  "jq -s -e 'map({tests_run, pane_scales, metric_names, highest_backpressure_tier}) | unique | length == 1' ${release_summary_1@Q} ${release_summary_2@Q} ${release_summary_3@Q}"

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
  --arg smoke_summary "${smoke_summary}" \
  --arg release_summary_1 "${release_summary_1}" \
  --arg release_summary_2 "${release_summary_2}" \
  --arg release_summary_3 "${release_summary_3}" \
  '{
    bead_id: $bead_id,
    scenario_id: $scenario_id,
    status: "passed",
    correlation_id: $correlation_id,
    artifact_dir: $artifact_dir,
    profiles: {
      smoke: {
        cycles: 1,
        summary: $smoke_summary
      },
      release: {
        cycles: 3,
        summaries: [
          $release_summary_1,
          $release_summary_2,
          $release_summary_3
        ]
      }
    },
    artifacts: {
      commands: $commands_file,
      env: $env_file,
      structured_log: $structured_log,
      stdout: $stdout_file,
      stderr: $stderr_file,
      rch_probe: $rch_probe_log,
      rch_probe_meta: $rch_probe_meta,
      rch_smoke: $rch_smoke_log,
      rch_smoke_meta: $rch_smoke_meta
    }
  }' > "${SUMMARY_FILE}"

echo "Summary: ${SUMMARY_FILE}"
