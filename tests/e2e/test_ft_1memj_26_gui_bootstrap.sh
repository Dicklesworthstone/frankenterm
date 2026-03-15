#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/gui_bootstrap/${RUN_ID}"
SCENARIO_ID="ft_1memj_26_gui_bootstrap"
CORRELATION_ID="ft-1memj.26-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"

mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

PASS=0
FAIL=0
TOTAL=0

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1memj_26_gui_bootstrap"
ensure_rch_ready

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "gui_bootstrap_contract.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
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

record_result() {
  local name="$1"
  local ok="$2"
  local reason_code="${3:-completed}"
  local error_code="${4:-none}"
  local input_summary="${5:-}"

  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_log "passed" "${name}" "scenario_end" "${reason_code}" "none" "${LOG_FILE}" "${input_summary}"
    echo "  PASS: ${name}"
  else
    FAIL=$((FAIL + 1))
    emit_log "failed" "${name}" "scenario_end" "${reason_code}" "${error_code}" "${LOG_FILE}" "${input_summary}"
    echo "  FAIL: ${name}"
  fi
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

write_never_called_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf 'unexpected invocation: %s\n' "\$*" >> "${marker_file}"
exit 97
EOF
  chmod +x "${mock_bin}/rch"
}

write_probe_failure_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "workers" && "\${2:-}" == "probe" ]]; then
  cat <<'JSON'
{"api_version":"1.0","data":[{"id":"mock-worker","host":"127.0.0.1","status":"connection_failed","error":"RCH-E100"}]}
JSON
  exit 0
fi

if [[ "\${1:-}" == "exec" ]]; then
  printf 'unexpected exec: %s\n' "\$*" >> "${marker_file}"
  exit 0
fi

printf 'unexpected invocation: %s\n' "\$*" >> "${marker_file}"
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_success_build_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "workers" && "\${2:-}" == "probe" ]]; then
  cat <<'JSON'
{"api_version":"1.0","data":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}]}
JSON
  exit 0
fi

if [[ "\${1:-}" == "exec" ]]; then
  shift
  printf '%s\n' "\$*" > "${marker_file}"
  target_dir=""
  for arg in "\$@"; do
    if [[ "\${arg}" == CARGO_TARGET_DIR=* ]]; then
      target_dir="\${arg#CARGO_TARGET_DIR=}"
      break
    fi
  done
  if [[ -z "\${target_dir}" ]]; then
    echo "missing CARGO_TARGET_DIR" >&2
    exit 64
  fi
  mkdir -p "\${PWD}/\${target_dir}/release"
  cat > "\${PWD}/\${target_dir}/release/frankenterm-gui" <<'BIN'
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then
  echo "stub 0.0.0"
  exit 0
fi
if [[ "\${1:-}" == "--help" ]]; then
  echo "stub help"
  exit 0
fi
exit 0
BIN
  cat > "\${PWD}/\${target_dir}/release/ft" <<'BIN'
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then
  echo "stub 0.0.0"
  exit 0
fi
if [[ "\${1:-}" == "--help" ]]; then
  echo "stub help"
  exit 0
fi
exit 0
BIN
  chmod +x "\${PWD}/\${target_dir}/release/frankenterm-gui" "\${PWD}/\${target_dir}/release/ft"
  exit 0
fi

printf 'unexpected invocation: %s\n' "\$*" >> "${marker_file}"
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_codesign_mock() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/codesign" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf 'codesign %s\n' "\$*" >> "${marker_file}"
exit 0
EOF
  chmod +x "${mock_bin}/codesign"
}

write_stub_binary() {
  local path="$1"
  cat > "${path}" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then
  echo "stub 0.0.0"
  exit 0
fi
if [[ "${1:-}" == "--help" ]]; then
  echo "stub help"
  exit 0
fi
exit 0
EOF
  chmod +x "${path}"
}

scenario_dry_run_skips_rch() {
  local scenario_dir="${ARTIFACT_DIR}/dry_run_skips_rch"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-invocations.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"

  mkdir -p "${scenario_dir}"
  write_never_called_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "dry_run_skips_rch" "dry_run" "none" "none" "${stdout_file}" "scripts/e2e_gui_bootstrap.sh --dry-run"
  if env \
    RCH_BIN="${mock_bin}/rch" \
    LOG_DIR="${scenario_dir}/logs" \
    GUI_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/e2e_gui_bootstrap.sh" --dry-run >"${stdout_file}" 2>"${stderr_file}"; then
    if [[ -f "${marker_file}" ]]; then
      record_result "dry_run_skips_rch" "false" "unexpected_rch_invocation" "RCH_CALLED" "dry-run touched mock rch"
      return
    fi
    if ! grep -Eq '\[DRY-RUN\].* exec -- env CARGO_TARGET_DIR=' "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "missing_dry_run_build_line" "DRY_RUN_OUTPUT_MISSING" "dry-run output missing rch preview"
      return
    fi
    if grep -Fq "${ROOT_DIR}/Cargo.toml" "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "absolute_manifest_path" "ABSOLUTE_MANIFEST_PATH" "dry-run preview leaked local manifest path"
      return
    fi
    if grep -Fq "CARGO_TARGET_DIR=${ROOT_DIR}/" "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "absolute_target_dir" "ABSOLUTE_TARGET_DIR" "dry-run preview leaked local target path"
      return
    fi
    if ! grep -Fq -- '--manifest-path Cargo.toml' "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "missing_relative_manifest_path" "MANIFEST_PATH_NOT_RELATIVE" "dry-run preview missing repo-relative manifest path"
      return
    fi
    if ! grep -q 'Summary: pass=3 fail=0 skip=4 total=7' "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "unexpected_summary" "SUMMARY_MISMATCH" "unexpected dry-run summary"
      return
    fi
    record_result "dry_run_skips_rch" "true"
    return
  fi

  record_result "dry_run_skips_rch" "false" "dry_run_failed" "SCRIPT_EXIT_NONZERO" "script returned non-zero in dry-run"
}

scenario_e2e_probe_failure_refuses_exec() {
  local scenario_dir="${ARTIFACT_DIR}/e2e_probe_failure_refuses_exec"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local probe_log="${scenario_dir}/rch-probe.json"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_probe_failure_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "e2e_probe_failure_refuses_exec" "probe_guard" "none" "none" "${stdout_file}" "scripts/e2e_gui_bootstrap.sh probe failure"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    RCH_PROBE_LOG="${probe_log}" \
    LOG_DIR="${scenario_dir}/logs" \
    GUI_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/e2e_gui_bootstrap.sh" --skip-bundle >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "e2e_probe_failure_refuses_exec" "false" "unexpected_success" "PROBE_GUARD_MISSING" "script unexpectedly succeeded"
    return
  fi
  if [[ -f "${marker_file}" ]]; then
    record_result "e2e_probe_failure_refuses_exec" "false" "unexpected_exec" "RCH_EXEC_CALLED" "mock rch exec path was invoked"
    return
  fi
  if ! grep -q 'No reachable RCH workers detected; refusing local cargo fallback.' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "missing_guardrail_message" "FAIL_OPEN_MESSAGE_MISSING" "missing fail-closed message"
    return
  fi
  if ! grep -q '\[SKIP\] 2. verify GUI binary exists (build step failed (no reachable RCH workers); GUI binary unavailable)' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "missing_dependency_skip" "DEPENDENCY_SKIP_MISSING" "dependent GUI binary check did not skip after build failure"
    return
  fi
  if grep -q '\[FAIL\] 2. verify GUI binary exists' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "cascade_failure_present" "CASCADE_FAILURE_PRESENT" "dependent GUI binary check still failed instead of skipping"
    return
  fi
  if ! grep -q 'Summary: pass=0 fail=1 skip=6 total=7' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "unexpected_summary" "SUMMARY_MISMATCH" "probe-failure summary did not collapse to a single root-cause failure"
    return
  fi
  if ! jq -e '.data[0].status == "connection_failed"' "${probe_log}" >/dev/null; then
    record_result "e2e_probe_failure_refuses_exec" "false" "probe_artifact_missing" "PROBE_LOG_INVALID" "probe artifact missing connection_failed status"
    return
  fi
  record_result "e2e_probe_failure_refuses_exec" "true"
}

scenario_bundle_skip_build_creates_structure() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_skip_build_creates_structure"
  local mock_bin="${scenario_dir}/mock-bin"
  local target_dir="${scenario_dir}/target"
  local output_dir="${scenario_dir}/output"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local codesign_log="${scenario_dir}/codesign.log"
  local app_bundle="${output_dir}/FrankenTerm.app"

  mkdir -p "${scenario_dir}" "${target_dir}/release" "${output_dir}"
  write_stub_binary "${target_dir}/release/frankenterm-gui"
  write_stub_binary "${target_dir}/release/ft"
  write_codesign_mock "${mock_bin}" "${codesign_log}"

  emit_log "running" "bundle_skip_build_creates_structure" "skip_build_bundle" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh --skip-build"
  if env \
    PATH="${mock_bin}:${PATH}" \
    CARGO_TARGET_DIR="${target_dir}" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --skip-build --output "${output_dir}" >"${stdout_file}" 2>"${stderr_file}"; then
    for required_path in \
      "${app_bundle}/Contents/Info.plist" \
      "${app_bundle}/Contents/PkgInfo" \
      "${app_bundle}/Contents/Resources/ft.icns" \
      "${app_bundle}/Contents/Resources/frankenterm.toml" \
      "${app_bundle}/Contents/MacOS/frankenterm-gui" \
      "${app_bundle}/Contents/MacOS/ft"; do
      if [[ ! -e "${required_path}" ]]; then
        record_result "bundle_skip_build_creates_structure" "false" "missing_bundle_artifact" "BUNDLE_STRUCTURE_MISSING" "missing ${required_path}"
        return
      fi
    done
    record_result "bundle_skip_build_creates_structure" "true"
    return
  fi

  record_result "bundle_skip_build_creates_structure" "false" "bundle_creation_failed" "BUNDLE_SCRIPT_FAILED" "bundle script returned non-zero"
}

scenario_bundle_refuses_overwrite() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_refuses_overwrite"
  local mock_bin="${scenario_dir}/mock-bin"
  local target_dir="${scenario_dir}/target"
  local output_dir="${scenario_dir}/output"
  local stdout_first="${scenario_dir}/stdout-first.log"
  local stderr_first="${scenario_dir}/stderr-first.log"
  local stdout_second="${scenario_dir}/stdout-second.log"
  local stderr_second="${scenario_dir}/stderr-second.log"
  local codesign_log="${scenario_dir}/codesign.log"
  local app_bundle="${output_dir}/FrankenTerm.app"
  local rc

  mkdir -p "${scenario_dir}" "${target_dir}/release" "${output_dir}"
  write_stub_binary "${target_dir}/release/frankenterm-gui"
  write_stub_binary "${target_dir}/release/ft"
  write_codesign_mock "${mock_bin}" "${codesign_log}"

  if ! env PATH="${mock_bin}:${PATH}" CARGO_TARGET_DIR="${target_dir}" "${ROOT_DIR}/scripts/create-macos-bundle.sh" --skip-build --output "${output_dir}" >"${stdout_first}" 2>"${stderr_first}"; then
    record_result "bundle_refuses_overwrite" "false" "seed_bundle_failed" "SEED_BUNDLE_FAILED" "unable to create initial bundle"
    return
  fi
  if [[ ! -d "${app_bundle}" ]]; then
    record_result "bundle_refuses_overwrite" "false" "seed_bundle_missing" "SEED_BUNDLE_MISSING" "initial bundle missing"
    return
  fi

  emit_log "running" "bundle_refuses_overwrite" "overwrite_guard" "none" "none" "${stdout_second}" "bundle overwrite refusal"
  set +e
  env PATH="${mock_bin}:${PATH}" CARGO_TARGET_DIR="${target_dir}" "${ROOT_DIR}/scripts/create-macos-bundle.sh" --skip-build --output "${output_dir}" >"${stdout_second}" 2>"${stderr_second}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "bundle_refuses_overwrite" "false" "overwrite_allowed" "OVERWRITE_GUARD_MISSING" "second bundle invocation unexpectedly succeeded"
    return
  fi
  if ! grep -q 'Error: app bundle already exists at' "${stdout_second}"; then
    record_result "bundle_refuses_overwrite" "false" "missing_overwrite_guard_message" "OVERWRITE_MESSAGE_MISSING" "overwrite refusal message missing"
    return
  fi
  record_result "bundle_refuses_overwrite" "true"
}

scenario_bundle_probe_failure_refuses_exec() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_probe_failure_refuses_exec"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_probe_failure_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "bundle_probe_failure_refuses_exec" "probe_guard" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh probe failure"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    CARGO_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --output "${scenario_dir}/output" >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "bundle_probe_failure_refuses_exec" "false" "unexpected_success" "PROBE_GUARD_MISSING" "bundle script unexpectedly succeeded"
    return
  fi
  if [[ -f "${marker_file}" ]]; then
    record_result "bundle_probe_failure_refuses_exec" "false" "unexpected_exec" "RCH_EXEC_CALLED" "bundle script invoked mock exec path"
    return
  fi
  if ! grep -q 'Error: no reachable RCH workers detected; refusing local cargo fallback' "${stdout_file}"; then
    record_result "bundle_probe_failure_refuses_exec" "false" "missing_guardrail_message" "FAIL_OPEN_MESSAGE_MISSING" "bundle probe refusal missing"
    return
  fi
  record_result "bundle_probe_failure_refuses_exec" "true"
}

scenario_bundle_build_uses_repo_relative_paths() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_build_uses_repo_relative_paths"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local output_dir="${scenario_dir}/output"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"

  mkdir -p "${scenario_dir}" "${output_dir}"
  write_success_build_rch "${mock_bin}" "${marker_file}"
  write_codesign_mock "${mock_bin}" "${scenario_dir}/codesign.log"

  emit_log "running" "bundle_build_uses_repo_relative_paths" "remote_safe_build" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh remote-safe paths"
  if env \
    PATH="${mock_bin}:${PATH}" \
    RCH_BIN="${mock_bin}/rch" \
    CARGO_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --output "${output_dir}" >"${stdout_file}" 2>"${stderr_file}"; then
    if [[ ! -f "${marker_file}" ]]; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "missing_exec_log" "RCH_EXEC_LOG_MISSING" "mock rch exec log missing"
      return
    fi
    if grep -Fq "${ROOT_DIR}/Cargo.toml" "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "absolute_manifest_path" "ABSOLUTE_MANIFEST_PATH" "bundle build invoked rch with host manifest path"
      return
    fi
    if grep -Fq "CARGO_TARGET_DIR=${ROOT_DIR}/" "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "absolute_target_dir" "ABSOLUTE_TARGET_DIR" "bundle build invoked rch with host target dir"
      return
    fi
    if ! grep -Fq -- '--manifest-path Cargo.toml' "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "missing_relative_manifest_path" "MANIFEST_PATH_NOT_RELATIVE" "bundle build omitted repo-relative manifest path"
      return
    fi
    record_result "bundle_build_uses_repo_relative_paths" "true"
    return
  fi

  record_result "bundle_build_uses_repo_relative_paths" "false" "bundle_build_failed" "BUNDLE_BUILD_SCRIPT_FAILED" "bundle build path scenario returned non-zero"
}

main() {
  echo "=== GUI Bootstrap Contract E2E (ft-1memj.26) ==="
  echo "Artifacts: ${ARTIFACT_DIR}"
  emit_log "started" "suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

  require_cmd jq
  require_cmd python3
  require_cmd file

  scenario_dry_run_skips_rch
  scenario_e2e_probe_failure_refuses_exec
  scenario_bundle_skip_build_creates_structure
  scenario_bundle_refuses_overwrite
  scenario_bundle_probe_failure_refuses_exec
  scenario_bundle_build_uses_repo_relative_paths

  echo ""
  echo "=== Summary ==="
  echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
  echo "  Log: ${LOG_FILE}"

  emit_log "$([[ "${FAIL}" -eq 0 ]] && echo passed || echo failed)" \
    "suite" "script_end" "completed" "none" "${LOG_FILE}" \
    "total=${TOTAL},pass=${PASS},fail=${FAIL}"

  jq -cn \
    --arg test "gui_bootstrap_contract" \
    --argjson scenarios_pass "${PASS}" \
    --argjson scenarios_fail "${FAIL}" \
    --argjson total "${TOTAL}" \
    --arg log_file "${LOG_FILE}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    '{
      test: $test,
      scenarios_pass: $scenarios_pass,
      scenarios_fail: $scenarios_fail,
      total: $total,
      log_file: $log_file,
      artifact_dir: $artifact_dir
    }'

  [[ "${FAIL}" -eq 0 ]]
}

main "$@"
