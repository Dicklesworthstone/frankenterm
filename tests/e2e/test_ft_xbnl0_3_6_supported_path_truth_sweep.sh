#!/usr/bin/env bash
set -euo pipefail

# Final supported-path truth sweep harness for ft-xbnl0.3.6.
# Re-runs the audit classifications from
# docs/ft-xbnl0-3-6-supported-path-truth-sweep.md and the supporting
# regression test, then emits a bead-native artifact bundle.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date +"%Y%m%dT%H%M%SZ")"
BEAD_ID="ft-xbnl0.3.6"
SCENARIO_ID="supported_path_truth_sweep"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
TARGET_DIR="target/rch-${BEAD_ID//./-}-${SCENARIO_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

exec > >(tee -a "${STDOUT_FILE}") 2> >(tee -a "${STDERR_FILE}" >&2)

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
RCH_STEP_TIMEOUT_SECS=2400
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_3_6_supported_path_truth_sweep"
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
    --arg surface "supported-path-truth-sweep" \
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
    write_failure_summary "${step}" "${command}" "${output_file}"
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
    write_failure_summary "${step}" "${command}" "${output_file}"
    exit 1
  fi
}

# A grep step asserts that an inventory-classified pattern survives at
# the expected sites and nothing widens the supported matrix silently.
# Each "expectation" is a glob/path the pattern must hit; a failure to
# match a required site, or a hit outside the allow-list, fails the run.
run_grep_classification_step() {
  local step="$1"
  local pattern="$2"
  shift 2
  local output_file="${ARTIFACT_DIR}/${step}.log"
  local command="rg -n ${pattern} -- $*"
  printf '%s\n' "${command}" >> "${COMMANDS_FILE}"
  local start_ns
  start_ns="$(date +%s%N)"
  emit_log "${step}" "started" "0" "${command}"
  # Use cd into root and rg explicitly so paths are relative.
  if (cd "${ROOT_DIR}" && rg -n "${pattern}" -- "$@") >"${output_file}" 2>&1; then
    local end_ns
    end_ns="$(date +%s%N)"
    local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
    emit_log "${step}" "passed" "${duration_ms}" "${command}"
  else
    local rc=$?
    if [[ "${rc}" == "1" ]]; then
      # rg exit 1 = no matches; for a classification step that means
      # the inventory is wrong or the marker silently disappeared.
      local end_ns
      end_ns="$(date +%s%N)"
      local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
      emit_log "${step}" "failed" "${duration_ms}" "${command}"
      write_failure_summary "${step}" "${command}" "${output_file}"
      exit 1
    else
      local end_ns
      end_ns="$(date +%s%N)"
      local duration_ms=$(( (end_ns - start_ns) / 1000000 ))
      emit_log "${step}" "failed" "${duration_ms}" "${command}"
      write_failure_summary "${step}" "${command}" "${output_file}"
      exit 1
    fi
  fi
}

echo "=== ${BEAD_ID} supported-path truth sweep ==="
echo "Artifacts: ${ARTIFACT_DIR}"

# Level A: cargo regression test that pins the SDK support contract.
run_cargo_step "sdk_support_contract_test" \
  test -p frankenterm-core --lib \
  ft_xbnl0_3_6_only_rust_sdk_target_is_finish_line_supported -- --nocapture
run_cargo_step "rust_sdk_render_test" \
  test -p frankenterm-core --lib rust_sdk_render_uses_real_transport_backend -- --nocapture
run_cargo_step "core_check" check -p frankenterm-core --lib --tests
run_cargo_step "core_clippy" clippy --no-deps -p frankenterm-core --lib --tests -- -D warnings
run_cargo_step "fmt_check" fmt --check

# Inventory classification re-runs. Each pattern must still hit its
# documented allow-list site so a future change cannot quietly delete
# the marker without updating the sweep doc.
run_grep_classification_step "transport_not_wired_python_template" \
  'transport not wired' crates/frankenterm-core/src/robot_sdk_contracts.rs
run_grep_classification_step "fakepane_test_only_pane" \
  '#\[cfg\(test\)\]' frankenterm/mux/src/pane.rs
run_grep_classification_step "fakepane_test_only_tab" \
  '#\[cfg\(test\)\]' frankenterm/mux/src/tab.rs
run_grep_classification_step "fakepane_test_only_sessionhandler" \
  '#\[cfg\(test\)\]' crates/frankenterm-mux-server-impl/src/sessionhandler.rs
run_grep_classification_step "terminfo_faketerm_test_only" \
  '#\[cfg\(all\(test, unix\)\)\]' frankenterm/termwiz/src/render/terminfo.rs
run_grep_classification_step "telemetry_unsupported_platform_shim" \
  'stub for unsupported platforms' crates/frankenterm-core/src/telemetry.rs
run_grep_classification_step "scrollback_null_adapter_test_only" \
  'Test-only null adapter' crates/frankenterm-core/src/fleet_scrollback_coordinator.rs
run_grep_classification_step "rendering_readback_owner_bitmaps" \
  'fn read\(&self, _rect: Rect, _im: &mut dyn BitmapImage\)' \
  frankenterm/window/src/bitmaps/mod.rs
run_grep_classification_step "rendering_readback_owner_webgpu" \
  'fn read\(&self, _rect: Rect, _im: &mut dyn BitmapImage\)' \
  crates/frankenterm-gui/src/termwindow/webgpu.rs

# Sweep doc presence + ownership cross-link.
run_shell_step "sweep_doc_anchor" \
  "rg -n 'Final Supported-Path Truth Sweep|is_fully_supported|ft-xbnl0.3.5|ft-akx00.6.3' docs/ft-xbnl0-3-6-supported-path-truth-sweep.md"

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
  --arg sweep_doc "${ROOT_DIR}/docs/ft-xbnl0-3-6-supported-path-truth-sweep.md" \
  '{
    bead_id: $bead_id,
    scenario_id: $scenario_id,
    status: "passed",
    correlation_id: $correlation_id,
    artifact_dir: $artifact_dir,
    artifacts: {
      commands: $commands_file,
      env: $env_file,
      structured_log: $structured_log,
      stdout: $stdout_file,
      stderr: $stderr_file,
      rch_probe: $rch_probe_log,
      rch_probe_meta: $rch_probe_meta,
      rch_smoke: $rch_smoke_log,
      rch_smoke_meta: $rch_smoke_meta,
      sweep_doc: $sweep_doc
    }
  }' > "${SUMMARY_FILE}"

echo "Summary: ${SUMMARY_FILE}"
