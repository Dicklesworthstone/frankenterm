#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="${SWARM_STRESS_LOG_DIR:-${ROOT_DIR}/tests/e2e/logs}"
ARTIFACT_DIR_BASE="${SWARM_STRESS_ARTIFACT_DIR_BASE:-${ROOT_DIR}/tests/e2e/artifacts/swarm_stress}"
RUN_ID="${RUN_ID:-$(date -u +"%Y%m%d_%H%M%S")}"
SCENARIO_ID="ft_1memj_30_swarm_stress"
CORRELATION_ID="ft-1memj.30-${RUN_ID}"
TARGET_DIR_REL="${TARGET_DIR_REL:-target/rch-e2e-ft-1memj-30-${RUN_ID}}"
PROFILE="${FT_SWARM_STRESS_PROFILE:-smoke}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
ARTIFACT_DIR="${ARTIFACT_DIR_BASE}/${RUN_ID}"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

# Cold remote compilation of the full swarm suite needs a wider guard than the
# generic 15-minute E2E default, while still allowing operator override.
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
RCH_ARTIFACT_DIR="${SWARM_STRESS_RCH_ARTIFACT_DIR:-${ARTIFACT_DIR}}"
rch_init "${RCH_ARTIFACT_DIR}" "${RUN_ID}" "1memj_30_swarm_stress" "${ROOT_DIR}"
ensure_rch_ready

printf 'scenario_id=%s\ncorrelation_id=%s\nprofile=%s\ntarget_dir_rel=%s\n' \
  "${SCENARIO_ID}" "${CORRELATION_ID}" "${PROFILE}" "${TARGET_DIR_REL}" > "${COMMANDS_FILE}"
env | sort > "${ENV_FILE}"

usage() {
  cat <<'EOF'
Usage: scripts/e2e_swarm_stress.sh

Environment:
  TARGET_DIR_REL   Repo-relative cargo target dir for rch offload
  RUN_ID           Override generated run id
  RCH_STEP_TIMEOUT_SECS  Override remote cargo step timeout (default: 2400)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 3
      ;;
  esac
done

require_repo_relative_target_dir() {
  case "${TARGET_DIR_REL}" in
    /*|../*|*/../*|..)
      echo "TARGET_DIR_REL must stay under the repo root for rch offload: ${TARGET_DIR_REL}" >&2
      exit 2
      ;;
  esac
}

require_known_profile() {
  case "${PROFILE}" in
    smoke|release)
      ;;
    *)
      echo "unsupported FT_SWARM_STRESS_PROFILE: ${PROFILE}" >&2
      exit 2
      ;;
  esac
}

append_command() {
  printf '%s\n' "$1" >> "${COMMANDS_FILE}"
}

emit_event() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local details="$6"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "swarm_stress.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    --arg details "${details}" \
    '{
      record_type: "suite_event",
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      details: $details
    }' >> "${LOG_FILE}"
}

append_metric() {
  local decision_path="$1"
  local artifact_path="$2"
  local metric_json="$3"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "swarm_stress.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg artifact_path "${artifact_path}" \
    --argjson metric "${metric_json}" \
    '$metric + {
      record_type: "swarm_metric",
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

expected_metric_names() {
  cat <<'EOF'
stress_50_panes_idle
stress_100_panes_idle
stress_200_panes_idle
stress_50_panes_active
stress_200_panes_active
stress_single_pane_10mb
stress_rapid_pane_create_destroy
stress_200_panes_backpressure
EOF
}

decision_path_for_metric() {
  local metric_name="$1"

  case "${metric_name}" in
    stress_50_panes_idle)
      printf '%s\n' "idle_50"
      ;;
    stress_100_panes_idle)
      printf '%s\n' "idle_100"
      ;;
    stress_200_panes_idle)
      printf '%s\n' "idle_200"
      ;;
    stress_50_panes_active)
      printf '%s\n' "active_50"
      ;;
    stress_200_panes_active)
      printf '%s\n' "active_200"
      ;;
    stress_single_pane_10mb)
      printf '%s\n' "single_pane_10mb"
      ;;
    stress_rapid_pane_create_destroy)
      printf '%s\n' "pane_churn"
      ;;
    stress_200_panes_backpressure)
      printf '%s\n' "backpressure_200"
      ;;
    *)
      return 1
      ;;
  esac
}

classify_rch_failure() {
  local stdout_file="$1"
  local dep_info_path=""

  dep_info_path="$(sed -n 's/^error: could not parse\/generate dep info at: //p' "${stdout_file}" | head -n 1)"

  if [[ -n "${dep_info_path}" ]] && grep -Fq 'No such file or directory (os error 2)' "${stdout_file}"; then
    printf '%s\t%s\t%s\n' \
      "cargo_dep_info_missing" \
      "cargo_dep_info_missing" \
      "dep-info failure at ${dep_info_path}; remote workspace integrity likely diverged before cargo finalized dependency metadata"
    return 0
  fi

  printf '%s\t%s\t%s\n' \
    "cargo_test_failed" \
    "cargo_command_failed" \
    "swarm suite failed"
}

run_swarm_suite() {
  local stdout_file="${ARTIFACT_DIR}/swarm_suite.stdout.log"
  local metric_names_file="${ARTIFACT_DIR}/swarm_suite.metric_names.txt"
  local saw_metrics=0
  local remote_cmd=""
  local failure_reason_code=""
  local failure_error_code=""
  local failure_details=""

  : > "${metric_names_file}"
  remote_cmd="cargo test -p frankenterm-core --test e2e_swarm_stress_core -- --nocapture"

  emit_event \
    "running" \
    "suite_rch_exec" \
    "none" \
    "none" \
    "$(basename "${stdout_file}")" \
      "${remote_cmd}"
  append_command "${remote_cmd}"

  if ! run_rch_cargo_logged \
    "${stdout_file}" \
    env CARGO_TARGET_DIR="${TARGET_DIR_REL}" \
    cargo test -p frankenterm-core --test e2e_swarm_stress_core -- --nocapture; then
    IFS=$'\t' read -r failure_reason_code failure_error_code failure_details < <(
      classify_rch_failure "${stdout_file}"
    )
    emit_event \
      "failed" \
      "suite_rch_exec" \
      "${failure_reason_code}" \
      "${failure_error_code}" \
      "$(basename "${stdout_file}")" \
      "${failure_details}"
    return 1
  fi

  while IFS= read -r metric_line; do
    local metric_json="${metric_line#FT_SWARM_METRIC }"
    local metric_name=""
    local decision_path=""

    metric_name="$(jq -r '.test // empty' <<< "${metric_json}")"
    if [[ -z "${metric_name}" ]]; then
      emit_event \
        "failed" \
        "suite_parse" \
        "invalid_metric_payload" \
        "swarm_metric_invalid" \
        "$(basename "${stdout_file}")" \
        "missing .test field in FT_SWARM_METRIC payload"
      return 1
    fi

    if ! decision_path="$(decision_path_for_metric "${metric_name}")"; then
      emit_event \
        "failed" \
        "suite_parse" \
        "unexpected_metric_name" \
        "swarm_metric_unexpected" \
        "$(basename "${stdout_file}")" \
        "unexpected metric name: ${metric_name}"
      return 1
    fi

    append_metric "${decision_path}" "$(basename "${stdout_file}")" "${metric_json}"
    printf '%s\n' "${metric_name}" >> "${metric_names_file}"
    saw_metrics=1

    emit_event \
      "passed" \
      "${decision_path}" \
      "stress_case_passed" \
      "none" \
      "$(basename "${stdout_file}")" \
      "metric=${metric_name}"
  done < <(grep '^FT_SWARM_METRIC ' "${stdout_file}" || true)

  if [[ "${saw_metrics}" -ne 1 ]]; then
    emit_event \
      "failed" \
      "suite_parse" \
      "missing_metric_output" \
      "swarm_metric_missing" \
      "$(basename "${stdout_file}")" \
      "expected FT_SWARM_METRIC lines in cargo test output"
    return 1
  fi

  while IFS= read -r expected_metric; do
    local decision_path=""
    local seen_count="0"

    decision_path="$(decision_path_for_metric "${expected_metric}")"
    seen_count="$(grep -Fxc "${expected_metric}" "${metric_names_file}" || true)"

    if [[ "${seen_count}" -eq 0 ]]; then
      emit_event \
        "failed" \
        "${decision_path}" \
        "missing_metric_output" \
        "swarm_metric_missing" \
        "$(basename "${stdout_file}")" \
        "expected metric not observed: ${expected_metric}"
      return 1
    fi

    if [[ "${seen_count}" -gt 1 ]]; then
      emit_event \
        "failed" \
        "${decision_path}" \
        "duplicate_metric_output" \
        "swarm_metric_duplicate" \
        "$(basename "${stdout_file}")" \
        "expected one metric, observed ${seen_count}: ${expected_metric}"
      return 1
    fi
  done < <(expected_metric_names)
}

emit_summary() {
  local summary_json

  summary_json="$(jq -s '
    def tier_rank($tier):
      if $tier == "Green" then 0
      elif $tier == "Yellow" then 1
      elif $tier == "Red" then 2
      elif $tier == "Black" then 3
      else -1
      end;
    {
      tests_run: ([.[] | select(.record_type == "swarm_metric")] | length),
      peak_rss_mb: ([.[] | select(.record_type == "swarm_metric") | .rss_mb // empty] | max // null),
      max_duration_s: ([.[] | select(.record_type == "swarm_metric") | .duration_s // empty] | max // null),
      highest_backpressure_tier: (
        [ .[] | select(.record_type == "swarm_metric") | .backpressure_tier // empty ] as $tiers
        | if ($tiers | length) == 0
          then null
          else ($tiers | sort_by(tier_rank(.)) | last)
          end
      ),
      pane_scales: (
        [ .[] | select(.record_type == "swarm_metric") | .pane_count // empty ]
        | unique
        | sort
      ),
      metric_names: (
        [ .[] | select(.record_type == "swarm_metric") | .test // empty ]
        | unique
        | sort
      )
    }' "${LOG_FILE}")"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg component "swarm_stress.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --argjson summary "${summary_json}" \
    '{
      record_type: "suite_summary",
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id
    } + $summary' >> "${LOG_FILE}"

  jq -cn \
    --arg schema_version "ft.swarm_stress.summary.v1" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg profile "${PROFILE}" \
    --arg target_dir_rel "${TARGET_DIR_REL}" \
    --arg log_file "${LOG_FILE}" \
    --arg commands_file "${COMMANDS_FILE}" \
    --arg env_file "${ENV_FILE}" \
    --arg stdout_file "${ARTIFACT_DIR}/swarm_suite.stdout.log" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg rch_probe_log "$(rch_probe_log_path)" \
    --arg rch_probe_meta "$(rch_log_meta_path "$(rch_probe_log_path)")" \
    --arg rch_smoke_log "$(rch_smoke_log_path)" \
    --arg rch_smoke_meta "$(rch_log_meta_path "$(rch_smoke_log_path)")" \
    --argjson summary "${summary_json}" \
    '{
      schema_version: $schema_version,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      run_id: $run_id,
      profile: $profile,
      target_dir_rel: $target_dir_rel,
      artifact_dir: $artifact_dir,
      artifacts: {
        structured_log: $log_file,
        commands: $commands_file,
        env: $env_file,
        stdout: $stdout_file,
        rch_probe: $rch_probe_log,
        rch_probe_meta: $rch_probe_meta,
        rch_smoke: $rch_smoke_log,
        rch_smoke_meta: $rch_smoke_meta
      }
    } + $summary' > "${SUMMARY_FILE}"
}

main() {
  require_repo_relative_target_dir
  require_known_profile

  emit_event \
    "started" \
    "suite_init" \
    "none" \
    "none" \
    "$(basename "${LOG_FILE}")" \
    "swarm stress suite init"

  run_swarm_suite

  emit_summary

  emit_event \
    "passed" \
    "suite_complete" \
    "all_cases_passed" \
    "none" \
    "$(basename "${LOG_FILE}")" \
    "swarm stress suite complete"

  echo "Scenario: ${SCENARIO_ID}"
  echo "Logs: tests/e2e/logs/$(basename "${LOG_FILE}")"
}

main "$@"
