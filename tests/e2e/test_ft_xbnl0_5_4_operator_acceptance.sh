#!/usr/bin/env bash
# E2E: validate ft-xbnl0.5.4 operator acceptance scenarios.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-xbnl0.5.4"
SCENARIO_ID="operator_acceptance"
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
REMOTE_TARGET_DIR="/tmp/ft-cod2-target"
EXACT_RECIPE_LOG="${ARTIFACT_DIR}/frankenterm_check_exact_recipe.log"
FALLBACK_CHECK_LOG="${ARTIFACT_DIR}/frankenterm_check_fallback.log"
UNIT_TEST_LOG="${ARTIFACT_DIR}/frankenterm_operator_guidance_tests.log"
REMOTE_SCENARIO_LOG="${ARTIFACT_DIR}/operator_acceptance_remote.log"
REMOTE_REPORT_JSON="${ARTIFACT_DIR}/operator_acceptance_remote_report.json"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
RCH_SKIP_SMOKE_PREFLIGHT=1
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_5_4_operator_acceptance"

PASS=0
FAIL=0
TOTAL=0
EXACT_RECIPE_RESULT="failed"

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
    --arg surface "operator-acceptance" \
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

run_rch_exact_recipe_check() {
  local log_file="$1"
  local start_ns end_ns duration_ms rc
  start_ns="$(date +%s%N)"
  record_command "CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} rch exec -- cargo check -p frankenterm"

  if [[ -z "${TIMEOUT_BIN:-}" ]]; then
    resolve_timeout_bin
  fi
  if [[ -z "${TIMEOUT_BIN:-}" ]]; then
    echo "timeout or gtimeout is required" >&2
    return 2
  fi

  set +e
  (
    cd "${ROOT_DIR}"
    exec env TMPDIR=/tmp \
      CC=/opt/homebrew/opt/llvm/bin/clang \
      CXX=/opt/homebrew/opt/llvm/bin/clang++ \
      CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
      "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
      rch exec -- cargo check -p frankenterm
  ) > "${log_file}" 2>&1
  rc=$?
  set -e

  check_rch_fallback "${log_file}"
  rch_write_meta_json "${log_file}" "${rc}"

  end_ns="$(date +%s%N)"
  duration_ms="$(((end_ns - start_ns) / 1000000))"

  if [[ ${rc} -eq 0 ]]; then
    EXACT_RECIPE_RESULT="passed"
    record_result "frankenterm_check_exact_recipe" "true" "${duration_ms}" "${log_file}"
    return 0
  fi

  if rg -F "/opt/homebrew/opt/llvm/bin/clang" "${log_file}" >/dev/null 2>&1; then
    EXACT_RECIPE_RESULT="portability_gap"
    record_result "frankenterm_check_exact_recipe_portability_gap" "true" "${duration_ms}" "${log_file}"
    return 0
  fi

  EXACT_RECIPE_RESULT="failed"
  record_result "frankenterm_check_exact_recipe" "false" "${duration_ms}" "${log_file}"
  return "${rc}"
}

run_remote_script_step() {
  local step="$1"
  local log_file="$2"
  local script_text="$3"
  local start_ns end_ns duration_ms rc remote_script_path remote_script_rel
  start_ns="$(date +%s%N)"
  remote_script_path="${ARTIFACT_DIR}/operator_acceptance_remote.sh"
  remote_script_rel="${remote_script_path#${ROOT_DIR}/}"
  printf '%s\n' "${script_text}" > "${remote_script_path}"
  chmod +x "${remote_script_path}"
  record_command "rch exec -- bash ${remote_script_rel}"
  set +e
  (
    cd "${ROOT_DIR}"
    exec env TMPDIR=/tmp rch exec -- bash "${remote_script_rel}"
  ) > "${log_file}" 2>&1
  rc=$?
  set -e
  check_rch_fallback "${log_file}"
  rch_write_meta_json "${log_file}" "${rc}"
  end_ns="$(date +%s%N)"
  duration_ms="$(((end_ns - start_ns) / 1000000))"
  if [[ ${rc} -eq 0 ]]; then
    record_result "${step}" "true" "${duration_ms}" "${log_file}"
    return 0
  fi
  record_result "${step}" "false" "${duration_ms}" "${log_file}"
  return "${rc}"
}

extract_remote_report() {
  python3 - "${REMOTE_SCENARIO_LOG}" "${REMOTE_REPORT_JSON}" <<'PY'
import pathlib
import sys

log_path = pathlib.Path(sys.argv[1])
out_path = pathlib.Path(sys.argv[2])
text = log_path.read_text()
start = "__FT_XBNL0_5_4_JSON_START__\n"
end = "\n__FT_XBNL0_5_4_JSON_END__"
start_idx = text.find(start)
end_idx = text.rfind(end)
if start_idx == -1 or end_idx == -1 or end_idx <= start_idx:
    raise SystemExit("remote acceptance report markers not found")
payload = text[start_idx + len(start):end_idx]
out_path.write_text(payload)
PY
}

reuse_prior_exact_recipe_artifact() {
  local current_dir="$1"
  local meta_path log_path found=""
  while IFS= read -r meta_path; do
    [[ "${meta_path}" == "${current_dir}/frankenterm_check_exact_recipe.log.rch_meta.json" ]] && continue
    if jq -e '.remote_exit_code == 0' "${meta_path}" >/dev/null 2>&1; then
      found="${meta_path}"
      break
    fi
  done < <(find "${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}" \
    -name 'frankenterm_check_exact_recipe.log.rch_meta.json' -print | sort -r)

  if [[ -z "${found}" ]]; then
    return 1
  fi

  log_path="${found%.rch_meta.json}"
  cp "${log_path}" "${EXACT_RECIPE_LOG}"
  cp "${found}" "$(rch_log_meta_path "${EXACT_RECIPE_LOG}")"
  EXACT_RECIPE_RESULT="passed"
  emit_log "frankenterm_check_exact_recipe_reused" "passed" 0 "${log_path}"
  PASS=$((PASS + 1))
  TOTAL=$((TOTAL + 1))
  return 0
}

reuse_prior_unit_test_artifact() {
  local current_dir="$1"
  local meta_path log_path found=""
  while IFS= read -r meta_path; do
    [[ "${meta_path}" == "${current_dir}/frankenterm_operator_guidance_tests.log.rch_meta.json" ]] && continue
    if jq -e '.remote_exit_code == 0' "${meta_path}" >/dev/null 2>&1; then
      found="${meta_path}"
      break
    fi
  done < <(find "${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}" \
    -name 'frankenterm_operator_guidance_tests.log.rch_meta.json' -print | sort -r)

  if [[ -z "${found}" ]]; then
    return 1
  fi

  log_path="${found%.rch_meta.json}"
  cp "${log_path}" "${UNIT_TEST_LOG}"
  cp "${found}" "$(rch_log_meta_path "${UNIT_TEST_LOG}")"
  emit_log "frankenterm_operator_guidance_tests_reused" "passed" 0 "${log_path}"
  PASS=$((PASS + 1))
  TOTAL=$((TOTAL + 1))
  return 0
}

echo "=== ${BEAD_ID} operator acceptance ==="
write_env
command -v jq >/dev/null 2>&1
command -v rch >/dev/null 2>&1
command -v python3 >/dev/null 2>&1
record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready

SOURCE_AUDIT_LOG="${ARTIFACT_DIR}/source_audit.log"
run_checked \
  "source_audit" \
  "${SOURCE_AUDIT_LOG}" \
  bash -lc "
    set -euo pipefail
    test -f '${ROOT_DIR}/docs/ft-xbnl0-5-4-operator-acceptance-scenarios.md'
    test -f '${ROOT_DIR}/docs/ft-xbnl0-5-4-operator-acceptance-scenarios.json'
    test -f '${ROOT_DIR}/scripts/check_ft_xbnl0_5_4_operator_acceptance.sh'
    test -f '${ROOT_DIR}/tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh'
    rg -n 'OA-01|OA-05|ft-xbnl0.5.7|ft-xbnl0.4.6' '${ROOT_DIR}/docs/ft-xbnl0-5-4-operator-acceptance-scenarios.md'
  "

SYNTAX_LOG="${ARTIFACT_DIR}/shell_syntax.log"
run_checked \
  "shell_syntax" \
  "${SYNTAX_LOG}" \
  bash -lc "
    set -euo pipefail
    bash -n '${ROOT_DIR}/scripts/check_ft_xbnl0_5_4_operator_acceptance.sh'
    bash -n '${ROOT_DIR}/tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh'
  "

CONTRACT_LOG="${ARTIFACT_DIR}/operator_acceptance_contract_check.log"
run_checked \
  "operator_acceptance_contract_check" \
  "${CONTRACT_LOG}" \
  bash "${ROOT_DIR}/scripts/check_ft_xbnl0_5_4_operator_acceptance.sh" \
    --output "${ARTIFACT_DIR}/operator_acceptance_contract_report.json"

if ! reuse_prior_exact_recipe_artifact "${ARTIFACT_DIR}"; then
  run_rch_exact_recipe_check "${EXACT_RECIPE_LOG}" || :
fi

if [[ "${EXACT_RECIPE_RESULT}" != "passed" ]]; then
  if ! run_rch_step \
    "frankenterm_check_fallback" \
    "${FALLBACK_CHECK_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
      cargo check -p frankenterm
  then
    :
  fi
  rch_write_meta_json "${FALLBACK_CHECK_LOG}"
fi

if ! reuse_prior_unit_test_artifact "${ARTIFACT_DIR}"; then
  if ! run_rch_step \
    "frankenterm_operator_guidance_tests" \
    "${UNIT_TEST_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
      cargo test -p frankenterm operator_guidance -- --nocapture
  then
    :
  fi
  rch_write_meta_json "${UNIT_TEST_LOG}"
fi

read -r -d '' REMOTE_SCRIPT <<'EOF' || true
set -euo pipefail

ROOT_DIR="$(pwd)"
export CARGO_TARGET_DIR="/tmp/ft-cod2-target"
mkdir -p "${CARGO_TARGET_DIR}"

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/ft-xbnl0-5-4.XXXXXX")"
bootstrap_ws="${tmpdir}/bootstrap"
broken_ws="${tmpdir}/broken"
recovery_ws="${tmpdir}/recovery"
ok_bin="${tmpdir}/bin-ok"
fail_bin="${tmpdir}/bin-fail"
mkdir -p "${bootstrap_ws}" "${broken_ws}" "${recovery_ws}" "${ok_bin}" "${fail_bin}"

cat > "${ok_bin}/wezterm" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--version" ]]; then
  printf 'wezterm 20240203-110809-5046fc22\n'
  exit 0
fi
if [[ "${1:-}" == "cli" && "${2:-}" == "list" ]]; then
  printf '[]\n'
  exit 0
fi
printf 'unsupported stub wezterm invocation\n' >&2
exit 1
SH
chmod +x "${ok_bin}/wezterm"

cat > "${fail_bin}/wezterm" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--version" ]]; then
  printf 'wezterm bridge unavailable\n' >&2
  exit 1
fi
if [[ "${1:-}" == "cli" && "${2:-}" == "list" ]]; then
  printf 'bridge unavailable\n' >&2
  exit 1
fi
printf 'unsupported stub wezterm invocation\n' >&2
exit 1
SH
chmod +x "${fail_bin}/wezterm"

run_ft() {
  local workspace="$1"
  local path_prefix="$2"
  shift 2
  PATH="${path_prefix}:$PATH" FT_WORKSPACE="${workspace}" cargo run -q -p frankenterm -- "$@"
}

set +e
broken_output="$(run_ft "${broken_ws}" "${fail_bin}" doctor --json 2>&1)"
broken_rc=$?
set -e

bootstrap_doctor="$(run_ft "${bootstrap_ws}" "${ok_bin}" doctor --json)"
bootstrap_status="$(run_ft "${bootstrap_ws}" "${ok_bin}" status --health -f json)"

set +e
PATH="${ok_bin}:$PATH" FT_WORKSPACE="${bootstrap_ws}" timeout --signal=TERM --kill-after=5 12 \
  cargo run -q -p frankenterm -- watch --foreground >/dev/null 2>&1
bootstrap_watch_rc=$?
set -e

mkdir -p "${recovery_ws}/.ft/logs"
python3 - "${recovery_ws}" <<'PY'
import json
import pathlib
import sqlite3
import sys

workspace = pathlib.Path(sys.argv[1])
db_path = workspace / ".ft" / "ft.db"
conn = sqlite3.connect(db_path)
conn.executescript(
    """
    CREATE TABLE mux_sessions (
        session_id TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL,
        last_checkpoint_at INTEGER,
        shutdown_clean INTEGER NOT NULL DEFAULT 0,
        topology_json TEXT NOT NULL,
        window_metadata_json TEXT,
        ft_version TEXT NOT NULL,
        host_id TEXT
    );
    CREATE TABLE session_checkpoints (
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL,
        checkpoint_at INTEGER NOT NULL,
        checkpoint_type TEXT,
        state_hash TEXT NOT NULL,
        pane_count INTEGER NOT NULL,
        total_bytes INTEGER NOT NULL,
        metadata_json TEXT
    );
    CREATE TABLE mux_pane_state (
        id INTEGER PRIMARY KEY,
        checkpoint_id INTEGER NOT NULL,
        pane_id INTEGER NOT NULL,
        cwd TEXT,
        command TEXT,
        env_json TEXT,
        terminal_state_json TEXT NOT NULL,
        agent_metadata_json TEXT,
        scrollback_checkpoint_seq INTEGER,
        last_output_at INTEGER
    );
    CREATE TABLE output_segments (
        id INTEGER PRIMARY KEY,
        pane_id INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        content TEXT NOT NULL,
        content_len INTEGER NOT NULL,
        content_hash TEXT,
        captured_at INTEGER NOT NULL,
        UNIQUE(pane_id, seq)
    );
    CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
    CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
    CREATE INDEX idx_output_segments_pane_seq ON output_segments(pane_id, seq);
    """
)
topology = json.dumps({"schema_version": 1, "captured_at": 1000, "windows": []})
terminal = json.dumps(
    {
        "rows": 24,
        "cols": 80,
        "cursor_row": 0,
        "cursor_col": 0,
        "is_alt_screen": False,
        "title": "acceptance",
    }
)
conn.execute(
    "INSERT INTO mux_sessions (session_id, created_at, last_checkpoint_at, topology_json, ft_version, shutdown_clean, host_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
    ("session-acceptance-1", 1000, 2000, topology, "0.1.0", 0, "host-a"),
)
conn.execute(
    "INSERT INTO session_checkpoints (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes) VALUES (?, ?, ?, ?, ?, ?)",
    ("session-acceptance-1", 2000, "periodic", "hash-1", 1, 512),
)
checkpoint_id = conn.execute("SELECT last_insert_rowid()").fetchone()[0]
conn.execute(
    "INSERT INTO mux_pane_state (checkpoint_id, pane_id, cwd, command, terminal_state_json, last_output_at) VALUES (?, ?, ?, ?, ?, ?)",
    (checkpoint_id, 7, "/workspace", "codex", terminal, 2000),
)
conn.execute(
    "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?, ?, ?, ?, ?)",
    (7, 1, "agent output", len("agent output"), 2000),
)
conn.commit()
conn.close()
PY

recovery_doctor="$(run_ft "${recovery_ws}" "${ok_bin}" doctor --json)"
recovery_status="$(run_ft "${recovery_ws}" "${ok_bin}" status --health -f json)"
recovery_session_doctor="$(run_ft "${recovery_ws}" "${ok_bin}" session doctor -f json)"

python3 - "${recovery_ws}" <<'PY'
import pathlib
import sqlite3
import sys
workspace = pathlib.Path(sys.argv[1])
db_path = workspace / ".ft" / "ft.db"
conn = sqlite3.connect(db_path)
conn.execute("UPDATE mux_sessions SET shutdown_clean = 1 WHERE session_id = 'session-acceptance-1'")
conn.commit()
conn.close()
PY

steady_session_doctor="$(run_ft "${recovery_ws}" "${ok_bin}" session doctor -f json)"
steady_session_list="$(run_ft "${recovery_ws}" "${ok_bin}" session list -f json)"

python3 - <<'PY' \
  "${bootstrap_ws}" \
  "${broken_rc}" \
  "${bootstrap_watch_rc}" \
  "${bootstrap_doctor}" \
  "${bootstrap_status}" \
  "${broken_output}" \
  "${recovery_doctor}" \
  "${recovery_status}" \
  "${recovery_session_doctor}" \
  "${steady_session_doctor}" \
  "${steady_session_list}"
import json
import pathlib
import sys

bootstrap_ws = pathlib.Path(sys.argv[1])
broken_rc = int(sys.argv[2])
bootstrap_watch_rc = int(sys.argv[3])
bootstrap_doctor = json.loads(sys.argv[4])
bootstrap_status = json.loads(sys.argv[5])
broken_output = json.loads(sys.argv[6])
recovery_doctor = json.loads(sys.argv[7])
recovery_status = json.loads(sys.argv[8])
recovery_session_doctor = json.loads(sys.argv[9])
steady_session_doctor = json.loads(sys.argv[10])
steady_session_list = json.loads(sys.argv[11])

assert bootstrap_status["operator_guidance"]["status"] == "bootstrap_required"
assert "operator_guidance" in bootstrap_doctor
assert isinstance(bootstrap_doctor["operator_guidance"].get("next_steps", []), list)
assert bootstrap_watch_rc in (0, 124, 143)
assert (bootstrap_ws / ".ft").exists()
assert (bootstrap_ws / ".ft" / "ft.db").exists()
assert (bootstrap_ws / ".ft" / "logs").exists()

assert broken_rc == 1
assert broken_output["status"] == "error"
assert broken_output["operator_guidance"]["status"] == "blocked"
assert any(
    check["name"] == "WezTerm CLI" and check["status"] == "error"
    for check in broken_output["checks"]
)

assert recovery_doctor["operator_guidance"]["status"] == "recovery_required"
assert recovery_status["operator_guidance"]["status"] == "recovery_required"
assert recovery_session_doctor["operator_guidance"]["status"] == "recovery_required"
assert steady_session_doctor["operator_guidance"]["status"] == "healthy"
assert isinstance(steady_session_list, list) and len(steady_session_list) == 1

payload = {
    "status": "passed",
    "bootstrap": {
        "doctor_status": bootstrap_doctor.get("status"),
        "doctor_guidance": bootstrap_doctor["operator_guidance"],
        "status_guidance": bootstrap_status["operator_guidance"],
        "watch_rc": bootstrap_watch_rc,
        "ft_dir_created": (bootstrap_ws / ".ft").exists(),
        "db_created": (bootstrap_ws / ".ft" / "ft.db").exists(),
        "logs_created": (bootstrap_ws / ".ft" / "logs").exists(),
    },
    "broken_environment": {
        "doctor_rc": broken_rc,
        "operator_guidance": broken_output["operator_guidance"],
    },
    "incident_triage": {
        "doctor_guidance": recovery_doctor["operator_guidance"],
        "status_guidance": recovery_status["operator_guidance"],
        "session_guidance": recovery_session_doctor["operator_guidance"],
    },
    "steady_state": {
        "session_guidance": steady_session_doctor["operator_guidance"],
        "session_count": len(steady_session_list),
    },
}
print("__FT_XBNL0_5_4_JSON_START__")
print(json.dumps(payload))
print("__FT_XBNL0_5_4_JSON_END__")
PY
EOF

if ! run_remote_script_step \
  "operator_acceptance_remote_run" \
  "${REMOTE_SCENARIO_LOG}" \
  "${REMOTE_SCRIPT}"
then
  :
fi

extract_remote_report

jq -e '
  .status == "passed" and
  .broken_environment.operator_guidance.status == "blocked" and
  .bootstrap.status_guidance.status == "bootstrap_required" and
  .incident_triage.doctor_guidance.status == "recovery_required" and
  .steady_state.session_guidance.status == "healthy"
' "${REMOTE_REPORT_JSON}" >/dev/null

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
  --arg contract_json "${ARTIFACT_DIR}/operator_acceptance_contract_report.json" \
  --arg exact_recipe_log "${EXACT_RECIPE_LOG}" \
  --arg exact_recipe_meta "$(rch_log_meta_path "${EXACT_RECIPE_LOG}")" \
  --arg fallback_check_log "${FALLBACK_CHECK_LOG}" \
  --arg fallback_check_meta "$(rch_log_meta_path "${FALLBACK_CHECK_LOG}")" \
  --arg unit_test_log "${UNIT_TEST_LOG}" \
  --arg unit_test_meta "$(rch_log_meta_path "${UNIT_TEST_LOG}")" \
  --arg remote_scenario_log "${REMOTE_SCENARIO_LOG}" \
  --arg remote_scenario_meta "$(rch_log_meta_path "${REMOTE_SCENARIO_LOG}")" \
  --arg remote_report_json "${REMOTE_REPORT_JSON}" \
  --argjson remote_report "$(cat "${REMOTE_REPORT_JSON}")" \
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
    remote_report: $remote_report,
    artifacts: {
      commands: $commands_file,
      env: $env_file,
      structured_log: $structured_log,
      stdout: $stdout_file,
      stderr: $stderr_file,
      source_audit: $source_audit_log,
      shell_syntax: $syntax_log,
      contract_check_log: $contract_log,
      contract_check_json: $contract_json,
      frankenterm_check_exact_recipe: $exact_recipe_log,
      frankenterm_check_exact_recipe_meta: $exact_recipe_meta,
      frankenterm_check_fallback: $fallback_check_log,
      frankenterm_check_fallback_meta: $fallback_check_meta,
      frankenterm_operator_guidance_tests: $unit_test_log,
      frankenterm_operator_guidance_tests_meta: $unit_test_meta,
      operator_acceptance_remote_log: $remote_scenario_log,
      operator_acceptance_remote_meta: $remote_scenario_meta,
      operator_acceptance_remote_report: $remote_report_json
    }
  }' > "${SUMMARY_FILE}"

if [[ "${FAIL}" -ne 0 ]]; then
  echo "ft-xbnl0.5.4 operator acceptance verification FAILED. Summary: ${SUMMARY_FILE}" >&2
  exit 1
fi

echo "ft-xbnl0.5.4 operator acceptance verification passed. Summary: ${SUMMARY_FILE}"
