#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_124z4"
CORRELATION_ID="ft-124z4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"

DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft124z4"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

LAST_STEP_LOG=""
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_SOCKET_PATH_REGEX='unix_listener: path .*too long for Unix domain socket|too long for Unix domain socket'
LOCAL_RCH_TMPDIR_OVERRIDE=""
RCH_WORKERS_TOML="${HOME}/.config/rch/workers.toml"
RCH_CONFIG_FILE="${ROOT_DIR}/.rch/config.toml"
RCH_REMOTE_BASE_DEFAULT="/home/ubuntu/rch"

if [[ "$(uname -s)" == "Darwin" ]]; then
  LOCAL_RCH_TMPDIR_OVERRIDE="/tmp"
fi

emit_log() {
  local component="$1"
  local decision_path="$2"
  local input_summary="$3"
  local outcome="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "${component}" \
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

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  "$@" 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return ${rc}
}

rch_fail_open_detected() {
  local log_path="$1"
  grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${log_path}"
}

rch_socket_path_issue_detected() {
  local log_path="$1"
  grep -Eq "${RCH_SOCKET_PATH_REGEX}" "${log_path}"
}

run_rch() {
  if [[ -n "${LOCAL_RCH_TMPDIR_OVERRIDE}" ]]; then
    TMPDIR="${LOCAL_RCH_TMPDIR_OVERRIDE}" rch "$@"
  else
    rch "$@"
  fi
}

extract_rch_remote_base() {
  local configured
  configured="$(awk -F'"' '/^[[:space:]]*remote_base[[:space:]]*=/{print $2; exit}' "${RCH_CONFIG_FILE}" 2>/dev/null || true)"
  if [[ -n "${configured}" ]]; then
    printf '%s\n' "${configured}"
  else
    printf '%s\n' "${RCH_REMOTE_BASE_DEFAULT}"
  fi
}

collect_external_cargo_manifest_paths() {
  python3 - "${ROOT_DIR}" <<'PY'
from pathlib import Path
import sys
import tomllib

root = Path(sys.argv[1]).resolve()
project_parent = root.parent
found = set()

def iter_dependency_tables(doc: dict):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = doc.get(key)
        if isinstance(table, dict):
            yield table

    workspace = doc.get("workspace")
    if isinstance(workspace, dict):
        table = workspace.get("dependencies")
        if isinstance(table, dict):
            yield table

    target = doc.get("target")
    if isinstance(target, dict):
        for target_cfg in target.values():
            if not isinstance(target_cfg, dict):
                continue
            for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                table = target_cfg.get(key)
                if isinstance(table, dict):
                    yield table

    for key in ("patch", "replace"):
        table = doc.get(key)
        if not isinstance(table, dict):
            continue
        for registry_cfg in table.values():
            if isinstance(registry_cfg, dict):
                yield registry_cfg

for manifest in root.rglob("Cargo.toml"):
    try:
        doc = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError):
        continue

    for table in iter_dependency_tables(doc):
        for spec in table.values():
            if not isinstance(spec, dict):
                continue
            dep_path = spec.get("path")
            if not isinstance(dep_path, str):
                continue

            resolved = (manifest.parent / dep_path).resolve()
            if resolved == root or str(resolved).startswith(str(root) + "/"):
                continue
            try:
                rel = resolved.relative_to(project_parent)
            except ValueError:
                continue
            manifest_path = rel / "Cargo.toml" if resolved.is_dir() else rel
            found.add(str(manifest_path))

for item in sorted(found):
    print(item)
PY
}

load_worker_topology_tsv() {
  python3 - "${RCH_WORKERS_TOML}" <<'PY'
from pathlib import Path
import sys
import tomllib

path = Path(sys.argv[1]).expanduser()
if not path.is_file():
    sys.exit(1)

data = tomllib.loads(path.read_text(encoding="utf-8"))
for worker in data.get("workers", []):
    identity_file = worker.get("identity_file", "")
    if identity_file:
        identity_file = str(Path(identity_file).expanduser())
    print(
        "\t".join(
            [
                worker.get("id", ""),
                worker.get("host", ""),
                worker.get("user", ""),
                identity_file,
            ]
        )
    )
PY
}

append_topology_report() {
  local report_path="$1"
  local worker_id="$2"
  local host="$3"
  local outcome="$4"
  local detail="$5"

  jq -cn \
    --arg worker_id "${worker_id}" \
    --arg host "${host}" \
    --arg outcome "${outcome}" \
    --arg detail "${detail}" \
    '{
      worker_id: $worker_id,
      host: $host,
      outcome: $outcome,
      detail: $detail
    }' >> "${report_path}"
}

probe_worker_remote_paths() {
  local worker_id="$1"
  local host="$2"
  local user="$3"
  local identity_file="$4"
  local remote_base="$5"
  local worker_log="$6"
  shift 6

  set +e
  ssh \
    -o BatchMode=yes \
    -o ConnectTimeout=10 \
    -o ControlMaster=no \
    -i "${identity_file}" \
    "${user}@${host}" \
    bash -s -- "${remote_base}" "$@" >"${worker_log}" 2>&1 <<'EOF'
set -euo pipefail
remote_base="$1"
shift

for dep in "$@"; do
  target="${remote_base}/${dep}"
  if [[ ! -f "${target}" ]]; then
    echo "missing:${target}"
    exit 12
  fi
done

echo "ok"
EOF
  local rc=$?
  set -e
  return "${rc}"
}

run_rch_topology_preflight() {
  local remote_base
  local deps_file topology_report worker_rows_raw worker_row
  local worker_id host user identity_file worker_log
  local missing_any="false"
  local worker_count=0
  local -a dep_paths=() worker_rows=()

  remote_base="$(extract_rch_remote_base)"
  mapfile -t dep_paths < <(collect_external_cargo_manifest_paths)

  if [[ ${#dep_paths[@]} -eq 0 ]]; then
    emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "skipped" "no_external_path_dependencies" "none" "$(basename "${STDOUT_FILE}")"
    return 0
  fi

  if [[ ! -f "${RCH_WORKERS_TOML}" ]]; then
    emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "failed" "missing_workers_toml" "RCH-WORKERS-CONFIG-MISSING" "$(basename "${STDOUT_FILE}")"
    echo "rch workers config not found: ${RCH_WORKERS_TOML}" >&2
    return 2
  fi

  deps_file="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_external_path_deps.txt"
  topology_report="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_worker_topology.jsonl"
  printf '%s\n' "${dep_paths[@]}" > "${deps_file}"
  : > "${topology_report}"

  emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "running" "none" "none" "$(basename "${deps_file}")"

  if ! worker_rows_raw="$(load_worker_topology_tsv)"; then
    emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "failed" "invalid_workers_toml" "RCH-WORKERS-CONFIG-INVALID" "$(basename "${deps_file}")"
    echo "failed to parse rch workers config: ${RCH_WORKERS_TOML}" >&2
    return 2
  fi

  if [[ -z "${worker_rows_raw}" ]]; then
    emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "failed" "no_workers_declared" "RCH-WORKERS-CONFIG-EMPTY" "$(basename "${deps_file}")"
    echo "rch workers config contains no workers: ${RCH_WORKERS_TOML}" >&2
    return 2
  fi

  mapfile -t worker_rows <<< "${worker_rows_raw}"

  for worker_row in "${worker_rows[@]}"; do
    IFS=$'\t' read -r worker_id host user identity_file <<< "${worker_row}"
    [[ -n "${worker_id}" && -n "${host}" && -n "${user}" && -n "${identity_file}" ]] || continue
    worker_count=$((worker_count + 1))
    worker_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${worker_id}_topology.log"

    if probe_worker_remote_paths "${worker_id}" "${host}" "${user}" "${identity_file}" "${remote_base}" "${worker_log}" "${dep_paths[@]}"; then
      append_topology_report "${topology_report}" "${worker_id}" "${host}" "ok" "all external path manifests present"
    else
      missing_any="true"
      append_topology_report "${topology_report}" "${worker_id}" "${host}" "missing" "$(tr '\n' ' ' < "${worker_log}")"
    fi
  done

  if [[ ${worker_count} -lt 1 ]]; then
    emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "failed" "no_valid_workers_declared" "RCH-WORKERS-CONFIG-EMPTY" "$(basename "${topology_report}")"
    echo "rch workers config contains no valid worker rows: ${RCH_WORKERS_TOML}" >&2
    return 2
  fi

  if [[ "${missing_any}" == "true" ]]; then
    emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "failed" "rch_worker_topology_drift" "RCH-REMOTE-TOPOLOGY" "$(basename "${topology_report}")"
    echo "rch worker topology drift detected; see ${topology_report}" >&2
    return 2
  fi

  emit_log "preflight" "rch_topology_preflight" "external_cargo_paths" "passed" "remote_path_dependencies_present" "none" "$(basename "${topology_report}")"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rch
require_cmd cargo
require_cmd python3
require_cmd ssh

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"

if [[ -n "${LOCAL_RCH_TMPDIR_OVERRIDE}" ]]; then
  emit_log "preflight" "rch_local_tmpdir_workaround" "TMPDIR=${LOCAL_RCH_TMPDIR_OVERRIDE}" "applied" "darwin_controlmaster_socket_guard" "none" "$(basename "${STDOUT_FILE}")"
fi

if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" == /* ]]; then
  emit_log "preflight" "rch_target_dir_sanitizer" "inherited=${INHERITED_CARGO_TARGET_DIR}" "applied" "absolute_target_dir_rewritten_for_remote_exec" "none" "$(basename "${STDOUT_FILE}")"
fi

probe_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_probe.json"
set +e
run_rch workers probe --all --json > "${probe_log}" 2>>"${STDOUT_FILE}"
probe_rc=$?
set -e

if [[ ${probe_rc} -ne 0 ]]; then
  if rch_socket_path_issue_detected "${STDOUT_FILE}"; then
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_local_socket_path_too_long" "RCH-LOCAL-TMPDIR" "$(basename "${STDOUT_FILE}")"
    echo "rch workers probe failed due to local SSH control socket path length; try TMPDIR=/tmp" >&2
  else
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_probe_failed" "RCH-E100" "$(basename "${probe_log}")"
    echo "rch workers probe failed" >&2
  fi
  exit 2
fi

healthy_workers=$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${probe_log}")
if [[ "${healthy_workers}" -lt 1 ]]; then
  emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_workers_unreachable" "RCH-E100" "$(basename "${probe_log}")"
  echo "no reachable rch workers; refusing local fallback" >&2
  exit 2
fi

emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"

if ! run_rch_topology_preflight; then
  exit 2
fi

emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step rch_remote_smoke run_rch exec -- cargo check --help; then
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    if rch_socket_path_issue_detected "${LAST_STEP_LOG}"; then
      emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "failed" "rch_local_socket_path_too_long" "RCH-LOCAL-TMPDIR" "$(basename "${LAST_STEP_LOG}")"
    else
      emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    fi
    echo "rch remote smoke check failed-open to local execution; refusing offload policy violation" >&2
    exit 3
  fi
  emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "passed" "remote_exec_confirmed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "failed" "rch_remote_smoke_failed" "RCH-REMOTE-SMOKE-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 2
fi

emit_log "validation" "nominal_path" "tailer_labruntime_tests" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step nominal_labruntime \
  run_rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --test tailer_labruntime --features asupersync-runtime -- --nocapture; then
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    emit_log "validation" "nominal_path" "tailer_labruntime_tests" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
  emit_log "validation" "nominal_path" "tailer_labruntime_tests" "passed" "tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  emit_log "validation" "nominal_path" "tailer_labruntime_tests" "failed" "test_failure" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "validation" "failure_injection_path" "bench_without_feature" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
set +e
run_step failure_missing_feature \
  run_rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo check -p frankenterm-core --bench tailer --message-format short
missing_feature_rc=$?
set -e

if rch_fail_open_detected "${LAST_STEP_LOG}"; then
  emit_log "validation" "failure_injection_path" "bench_without_feature" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
  echo "rch fell back to local execution; failing per offload-only policy" >&2
  exit 3
fi

if [[ ${missing_feature_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection_path" "bench_without_feature" "failed" "expected_failure_missing" "EXPECTED-FAILURE-NOT-TRIGGERED" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

if ! grep -q "requires the features: .*asupersync-runtime" "${LAST_STEP_LOG}"; then
  emit_log "validation" "failure_injection_path" "bench_without_feature" "failed" "unexpected_error_signature" "FEATURE-GATE-SIGNATURE-MISSING" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "validation" "failure_injection_path" "bench_without_feature" "passed" "expected_feature_gate_failure" "none" "$(basename "${LAST_STEP_LOG}")"

emit_log "validation" "recovery_path" "bench_with_feature" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step recovery_with_feature \
  run_rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo check -p frankenterm-core --bench tailer --features asupersync-runtime --message-format short; then
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    emit_log "validation" "recovery_path" "bench_with_feature" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
  emit_log "validation" "recovery_path" "bench_with_feature" "passed" "recovery_success" "none" "$(basename "${LAST_STEP_LOG}")"
else
  emit_log "validation" "recovery_path" "bench_with_feature" "failed" "recovery_failed" "CARGO-CHECK-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "summary" "nominal->failure_injection->recovery" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"

echo "ft-124z4 e2e scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
