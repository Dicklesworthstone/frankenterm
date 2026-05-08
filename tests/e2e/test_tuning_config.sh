#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_ojpy0_tuning_config_workflow"
CORRELATION_ID="ft-ojpy0-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_ojpy0_tuning_config_${RUN_ID}.jsonl"
REMOTE_LOG="${LOG_DIR}/ft_ojpy0_tuning_config_${RUN_ID}.remote.log"
DOCTOR_JSON="${LOG_DIR}/ft_ojpy0_tuning_config_${RUN_ID}.doctor.json"
RCH_TARGET_DIR="target/rch-e2e-tuning-config-${RUN_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "tuning_config.e2e" \
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

fatal() {
  echo "FATAL: $1" >&2
  exit 1
}

run_rch_remote_logged() {
  local output_file="$1"
  shift

  run_rch_cargo_logged_with_timeout 1800 "${output_file}" "$@"
}

if ! command -v jq >/dev/null 2>&1; then
  fatal "jq is required for structured logging and doctor JSON validation."
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "ft_ojpy0_tuning_config"

emit_log "started" "script_init" "none" "none" \
  "$(basename "${LOG_FILE}")" \
  "tuning config e2e started"

ensure_rch_ready

read -r -d '' REMOTE_SCRIPT <<'EOF' || true
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/ft-tuning-config.XXXXXX")"
watch_pid=""

cleanup() {
  if [[ -n "${watch_pid}" ]]; then
    kill "${watch_pid}" 2>/dev/null || true
    wait "${watch_pid}" 2>/dev/null || true
  fi
  rm -rf "${workdir}"
}
trap cleanup EXIT

config="${workdir}/ft.toml"
stub="${workdir}/wezterm"
watch_log="${workdir}/watch.log"
doctor_json="${workdir}/doctor.json"
doctor_stderr="${workdir}/doctor.stderr"

cat >"${config}" <<'CONFIG'
[general]
log_level = "info"

[vendored]
mux_socket_path = ""

[vendored.sharding]
enabled = false

[tuning.web]
default_host = "0.0.0.0"
default_port = 9911
stream_keepalive_secs = 9

[tuning.workflows]
max_steps = 12
CONFIG

cat >"${stub}" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  --version)
    printf '%s\n' 'wezterm 2026.03.25 ft-e2e'
    ;;
  cli)
    shift
    case "${1:-}" in
      list)
        printf '%s\n' '[]'
        ;;
      get-text)
        printf '%s' ''
        ;;
      *)
        exit 0
        ;;
    esac
    ;;
  *)
    exit 0
    ;;
esac
STUB
chmod +x "${stub}"

export FT_WEZTERM_CLI="${stub}"
export WEZTERM_UNIX_SOCKET=""

cargo run -p frankenterm -- --workspace "${workdir}" --config "${config}" \
  watch --foreground --poll-interval 200 >"${watch_log}" 2>&1 &
watch_pid="$!"

started=0
for _ in $(seq 1 240); do
  if [[ -f "${workdir}/.ft/ft.lock" || -f "${workdir}/.ft/ft.db" ]]; then
    started=1
    break
  fi
  if ! kill -0 "${watch_pid}" 2>/dev/null; then
    echo "watch exited before workspace artifacts appeared" >&2
    sed -n '1,200p' "${watch_log}" >&2 || true
    exit 1
  fi
  sleep 1
done

if [[ "${started}" -ne 1 ]]; then
  echo "watch did not create runtime artifacts within timeout" >&2
  sed -n '1,200p' "${watch_log}" >&2 || true
  exit 1
fi

set +e
cargo run -p frankenterm -- --workspace "${workdir}" --config "${config}" \
  doctor --json >"${doctor_json}" 2>"${doctor_stderr}"
doctor_rc=$?
set -e

if [[ "${doctor_rc}" -ne 0 && "${doctor_rc}" -ne 1 ]]; then
  echo "unexpected doctor exit code: ${doctor_rc}" >&2
  cat "${doctor_stderr}" >&2 || true
  exit "${doctor_rc}"
fi

printf 'DOCTOR_EXIT=%s\n' "${doctor_rc}"
printf '%s\n' 'DOCTOR_JSON_BEGIN'
cat "${doctor_json}"
printf '\n%s\n' 'DOCTOR_JSON_END'
printf '%s\n' 'WATCH_LOG_TAIL_BEGIN'
tail -n 80 "${watch_log}" || true
printf '\n%s\n' 'WATCH_LOG_TAIL_END'
EOF

emit_log "running" "remote_e2e" "rch_exec" "none" \
  "$(basename "${REMOTE_LOG}")" \
  "start remote workspace, launch watch, and inspect doctor json"

set +e
run_rch_remote_logged "${REMOTE_LOG}" \
  env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" bash -lc "${REMOTE_SCRIPT}"
remote_rc=$?
set -e

if [[ ${remote_rc} -ne 0 ]]; then
  emit_log "failed" "remote_e2e" "remote_execution_failed" "REMOTE_FAIL" \
    "$(basename "${REMOTE_LOG}")" \
    "remote rch execution failed"
  fatal "remote tuning config e2e run failed; see ${REMOTE_LOG}"
fi

awk '
  /^DOCTOR_JSON_BEGIN$/ { capture=1; next }
  /^DOCTOR_JSON_END$/ { capture=0; next }
  capture { print }
' "${REMOTE_LOG}" > "${DOCTOR_JSON}"

if [[ ! -s "${DOCTOR_JSON}" ]]; then
  emit_log "failed" "doctor_extract" "missing_doctor_json" "JSON_MISSING" \
    "$(basename "${REMOTE_LOG}")" \
    "doctor json markers were not found in remote output"
  fatal "doctor json was not captured from remote run"
fi

emit_log "running" "doctor_validate" "jq_assertions" "none" \
  "$(basename "${DOCTOR_JSON}")" \
  "validate doctor json contains active tuning overrides and running daemon status"

if ! jq -e '
  .checks | any(
    .name == "tuning"
    and ((.detail // "") | contains("tuning.web.default_host=\"0.0.0.0\""))
    and ((.detail // "") | contains("tuning.web.default_port=9911"))
    and ((.detail // "") | contains("tuning.web.stream_keepalive_secs=9"))
    and ((.detail // "") | contains("tuning.workflows.max_steps=12"))
  )
' "${DOCTOR_JSON}" >/dev/null; then
  emit_log "failed" "doctor_validate" "missing_tuning_values" "ASSERT_FAIL" \
    "$(basename "${DOCTOR_JSON}")" \
    "doctor json did not include expected tuning override values"
  fatal "doctor json did not report the expected tuning overrides"
fi

if ! jq -e '
  .checks | any(
    .name == "daemon status"
    and ((.detail // "") | contains("running"))
  )
' "${DOCTOR_JSON}" >/dev/null; then
  emit_log "failed" "doctor_validate" "daemon_not_running" "ASSERT_FAIL" \
    "$(basename "${DOCTOR_JSON}")" \
    "doctor json did not show the watcher as running"
  fatal "doctor json did not show a running watcher"
fi

emit_log "passed" "doctor_validate" "tuning_overrides_visible" "none" \
  "$(basename "${DOCTOR_JSON}")" \
  "doctor json reported tuning overrides and running daemon status"

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "remote watch+doctor tuning workflow passed"
