#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ARTIFACT_LIB="${ROOT_DIR}/scripts/lib/e2e_artifacts.sh"

if [[ ! -f "${ARTIFACT_LIB}" ]]; then
  echo "missing artifact helper: ${ARTIFACT_LIB}" >&2
  exit 2
fi

# shellcheck disable=SC1090
source "${ARTIFACT_LIB}"

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-1800}"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

TEST_ID="ft-gofz5.1"
DEFAULT_GIT_URL="https://github.com/Dicklesworthstone/frankenterm.git"
PACKAGE_NAME="frankenterm"
BINARY_NAME="ft"
RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"

KEEP_ARTIFACTS=false
INSTALL_SOURCE="${FT_INSTALL_SOURCE:-git}"
INSTALL_GIT_URL="${FT_INSTALL_GIT_URL:-${DEFAULT_GIT_URL}}"
INSTALL_GIT_REF="${FT_INSTALL_GIT_REF:-}"
INSTALL_PATH="${FT_INSTALL_PATH:-${ROOT_DIR}/crates/frankenterm}"

usage() {
  cat <<'EOF'
Usage: test_ft_gofz5_1_cargo_install.sh [--keep-artifacts]

Note: RCH remote work artifacts are always retained as evidence; the flag is
recorded in the report for operator intent.

Environment overrides:
  FT_INSTALL_SOURCE=git|path
  FT_INSTALL_GIT_URL=<git-url>
  FT_INSTALL_GIT_REF=<rev-or-tag>
  FT_INSTALL_PATH=<local-package-path>
  E2E_ARTIFACTS_BASE=<artifact-dir>
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep-artifacts)
      KEEP_ARTIFACTS=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "missing required command: ${cmd}" >&2
    exit 2
  fi
}

scenario_cargo_install_validation() {
  local scenario_dir remote_report_file rch_log remote_script remote_install_path rch_rc

  case "${INSTALL_SOURCE}" in
    git|path) ;;
    *)
      echo "FT_INSTALL_SOURCE must be git or path, got: ${INSTALL_SOURCE}" >&2
      return 2
      ;;
  esac

  remote_install_path="${INSTALL_PATH}"
  if [[ "${INSTALL_SOURCE}" == "path" ]]; then
    if [[ "${INSTALL_PATH}" == "${ROOT_DIR}/"* ]]; then
      remote_install_path="${INSTALL_PATH#"${ROOT_DIR}/"}"
    elif [[ "${INSTALL_PATH}" == /* ]]; then
      echo "FT_INSTALL_PATH must be inside the repo for RCH execution: ${INSTALL_PATH}" >&2
      return 2
    fi
  fi

  scenario_dir="${E2E_SCENARIOS_DIR}/${E2E_CURRENT_SCENARIO}"
  rch_log="${scenario_dir}/rch-cargo-install.log"
  remote_report_file="${scenario_dir}/remote-report.json"

  remote_script="$(cat <<'REMOTE'
set -euo pipefail

require_remote_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "missing required remote command: ${cmd}" >&2
    exit 2
  fi
}

time_flag() {
  case "$(uname -s)" in
    Darwin) echo "-l" ;;
    *) echo "-v" ;;
  esac
}

extract_peak_rss_kb() {
  local metrics_file="$1"
  python3 - "$metrics_file" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
patterns = [
    r"Maximum resident set size \(kbytes\):\s*(\d+)",
    r"(\d+)\s+maximum resident set size",
]
for pattern in patterns:
    match = re.search(pattern, text, flags=re.IGNORECASE)
    if match:
        print(match.group(1))
        break
else:
    print("")
PY
}

du_kb() {
  local path="$1"
  local value
  value="$(du -sk "${path}" 2>/dev/null | awk '{print $1}' || true)"
  printf '%s\n' "${value:-0}"
}

require_remote_cmd cargo
require_remote_cmd jq
require_remote_cmd python3
require_remote_cmd du
require_remote_cmd df
if [[ ! -x /usr/bin/time ]]; then
  echo "missing required remote command: /usr/bin/time" >&2
  exit 2
fi

work_dir="${FT_REMOTE_WORK_DIR:-target/rch-e2e-ft-gofz5-1/${FT_REMOTE_RUN_ID}}"
install_root="${work_dir}/install-root"
cargo_home="${work_dir}/cargo-home"
cargo_target="${work_dir}/cargo-target"
workspace="${work_dir}/workspace"
mkdir -p "${install_root}" "${cargo_home}" "${cargo_target}" "${workspace}"

install_stdout="${work_dir}/cargo-install.stdout.log"
install_stderr="${work_dir}/cargo-install.stderr.log"
version_stdout="${work_dir}/ft-version.stdout.log"
version_stderr="${work_dir}/ft-version.stderr.log"
doctor_stdout="${work_dir}/ft-doctor.stdout.log"
doctor_stderr="${work_dir}/ft-doctor.stderr.log"
disk_before="${work_dir}/disk-before.txt"
disk_after="${work_dir}/disk-after.txt"
df_before="${work_dir}/df-before.txt"
df_after="${work_dir}/df-after.txt"
install_command_file="${work_dir}/install-command.txt"
report_file="${work_dir}/report.json"

du -sk "${work_dir}" > "${disk_before}" 2>/dev/null || true
df -Pk "${work_dir}" > "${df_before}" 2>/dev/null || true

install_cmd=(cargo install --locked --root "${install_root}" --bin "${FT_BINARY_NAME}")
if [[ "${FT_INSTALL_SOURCE}" == "path" ]]; then
  install_cmd+=(--path "${FT_INSTALL_PATH_REMOTE}")
  source_summary="path:${FT_INSTALL_PATH_REMOTE}"
else
  install_cmd+=(--git "${FT_INSTALL_GIT_URL}")
  if [[ -n "${FT_INSTALL_GIT_REF}" ]]; then
    install_cmd+=(--rev "${FT_INSTALL_GIT_REF}")
  fi
  install_cmd+=("${FT_PACKAGE_NAME}")
  source_summary="git:${FT_INSTALL_GIT_URL}${FT_INSTALL_GIT_REF:+@${FT_INSTALL_GIT_REF}}"
fi

printf '%q ' "${install_cmd[@]}" > "${install_command_file}"
printf '\n' >> "${install_command_file}"

wall_start_ms="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"

install_exit=0
env \
  CARGO_HOME="${cargo_home}" \
  CARGO_TARGET_DIR="${cargo_target}" \
  CARGO_INSTALL_ROOT="${install_root}" \
  CARGO_TERM_COLOR=never \
  /usr/bin/time "$(time_flag)" "${install_cmd[@]}" >"${install_stdout}" 2>"${install_stderr}" || install_exit=$?

wall_end_ms="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"

if [[ ${install_exit} -ne 0 ]]; then
  cat "${install_stderr}" >&2
  exit "${install_exit}"
fi

ft_path="${install_root}/bin/${FT_BINARY_NAME}"
if [[ ! -x "${ft_path}" ]]; then
  echo "installed binary missing: ${ft_path}" >&2
  exit 1
fi

version_exit=0
"${ft_path}" --version >"${version_stdout}" 2>"${version_stderr}" || version_exit=$?
if [[ ${version_exit} -ne 0 ]]; then
  echo "installed ft --version failed" >&2
  exit "${version_exit}"
fi

doctor_exit=0
env \
  FT_WORKSPACE="${workspace}" \
  FT_WEZTERM_CLI=/nonexistent/wezterm \
  "${ft_path}" doctor --json >"${doctor_stdout}" 2>"${doctor_stderr}" || doctor_exit=$?

if [[ ! -s "${doctor_stdout}" ]]; then
  echo "installed ft doctor --json produced no stdout" >&2
  cat "${doctor_stderr}" >&2 || true
  exit 1
fi

if ! jq -e '
  type == "object"
  and (.ok | type == "boolean")
  and (.status | type == "string")
  and (.version | type == "string")
  and (.checks | type == "array")
' "${doctor_stdout}" >/dev/null; then
  echo "installed ft doctor --json produced an invalid diagnostic envelope" >&2
  cat "${doctor_stdout}" >&2
  cat "${doctor_stderr}" >&2 || true
  exit 1
fi

doctor_status="$(jq -r '.status' "${doctor_stdout}")"
doctor_ok="$(jq -r '.ok' "${doctor_stdout}")"
doctor_error_count="$(jq '[.checks[] | select((.status // "") == "error")] | length' "${doctor_stdout}")"
doctor_warning_count="$(jq '[.checks[] | select((.status // "") == "warning")] | length' "${doctor_stdout}")"
doctor_check_count="$(jq '.checks | length' "${doctor_stdout}")"

if [[ "${doctor_ok}" == "true" && ${doctor_exit} -ne 0 ]]; then
  echo "installed ft doctor --json exited nonzero despite reporting ok=true" >&2
  exit 1
fi

if [[ "${doctor_ok}" == "false" && ${doctor_exit} -eq 0 ]]; then
  echo "installed ft doctor --json exited zero despite reporting ok=false" >&2
  exit 1
fi

peak_rss_kb="$(extract_peak_rss_kb "${install_stderr}")"
du -sk "${work_dir}" > "${disk_after}" 2>/dev/null || true
df -Pk "${work_dir}" > "${df_after}" 2>/dev/null || true

jq -n \
  --arg schema_version "ft.cargo_install.validation.v1" \
  --arg bead_id "${FT_TEST_ID}" \
  --arg mode "${FT_INSTALL_SOURCE}" \
  --arg source "${source_summary}" \
  --arg advertised_command "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git --bin ft frankenterm" \
  --arg install_root "${install_root}" \
  --arg cargo_home "${cargo_home}" \
  --arg cargo_target "${cargo_target}" \
  --arg workspace "${workspace}" \
  --arg remote_work_dir "${work_dir}" \
  --arg os_name "$(uname -s)" \
  --arg os_arch "$(uname -m)" \
  --arg ft_path "${ft_path}" \
  --arg doctor_status "${doctor_status}" \
  --arg peak_rss_kb "${peak_rss_kb}" \
  --argjson keep_artifacts_requested "${FT_KEEP_ARTIFACTS}" \
  --argjson doctor_ok "${doctor_ok}" \
  --argjson doctor_error_count "${doctor_error_count}" \
  --argjson doctor_warning_count "${doctor_warning_count}" \
  --argjson doctor_check_count "${doctor_check_count}" \
  --argjson install_exit "${install_exit}" \
  --argjson version_exit "${version_exit}" \
  --argjson doctor_exit "${doctor_exit}" \
  --argjson wall_time_ms "$((wall_end_ms - wall_start_ms))" \
  --argjson work_dir_kb "$(du_kb "${work_dir}")" \
  --argjson install_root_kb "$(du_kb "${install_root}")" \
  --argjson cargo_home_kb "$(du_kb "${cargo_home}")" \
  --argjson cargo_target_kb "$(du_kb "${cargo_target}")" \
  --rawfile install_command "${install_command_file}" \
  --rawfile install_stdout "${install_stdout}" \
  --rawfile install_stderr "${install_stderr}" \
  --rawfile version_stdout "${version_stdout}" \
  --rawfile version_stderr "${version_stderr}" \
  --rawfile doctor_stdout "${doctor_stdout}" \
  --rawfile doctor_stderr "${doctor_stderr}" \
  --rawfile df_before "${df_before}" \
  --rawfile df_after "${df_after}" \
  --rawfile disk_before "${disk_before}" \
  --rawfile disk_after "${disk_after}" \
  '{
    schema_version: $schema_version,
    bead_id: $bead_id,
    validation: {
      mode: $mode,
      source: $source,
      advertised_command: $advertised_command,
      execution: "rch"
    },
    platform: {
      os: $os_name,
      arch: $os_arch
    },
    install: {
      exit_code: $install_exit,
      wall_time_ms: $wall_time_ms,
      peak_rss_kb: (if $peak_rss_kb == "" then null else ($peak_rss_kb | tonumber) end),
      work_dir_kb: $work_dir_kb,
      install_root_kb: $install_root_kb,
      cargo_home_kb: $cargo_home_kb,
      cargo_target_kb: $cargo_target_kb,
      install_root: $install_root,
      cargo_home: $cargo_home,
      cargo_target: $cargo_target,
      remote_work_dir: $remote_work_dir
    },
    artifacts: {
      keep_artifacts_requested: $keep_artifacts_requested,
      cleanup: "retained_for_remote_rch_evidence"
    },
    verification: {
      ft_path: $ft_path,
      version_exit: $version_exit,
      doctor_exit: $doctor_exit,
      doctor_ok: $doctor_ok,
      doctor_status: $doctor_status,
      doctor_error_count: $doctor_error_count,
      doctor_warning_count: $doctor_warning_count,
      doctor_check_count: $doctor_check_count,
      workspace: $workspace
    },
    outputs: {
      install_command: $install_command,
      install_stdout: $install_stdout,
      install_stderr: $install_stderr,
      version_stdout: $version_stdout,
      version_stderr: $version_stderr,
      doctor_stdout: $doctor_stdout,
      doctor_stderr: $doctor_stderr,
      df_before: $df_before,
      df_after: $df_after,
      disk_before: $disk_before,
      disk_after: $disk_after
    }
  }' > "${report_file}"

printf 'FT_GOFZ5_REPORT_BEGIN\n'
cat "${report_file}"
printf '\nFT_GOFZ5_REPORT_END\n'
REMOTE
)"

  ensure_rch_ready

  set +e
  run_rch_cargo_logged "${rch_log}" env \
    FT_BINARY_NAME="${BINARY_NAME}" \
    FT_INSTALL_GIT_REF="${INSTALL_GIT_REF}" \
    FT_INSTALL_GIT_URL="${INSTALL_GIT_URL}" \
    FT_INSTALL_PATH_REMOTE="${remote_install_path}" \
    FT_INSTALL_SOURCE="${INSTALL_SOURCE}" \
    FT_PACKAGE_NAME="${PACKAGE_NAME}" \
    FT_REMOTE_RUN_ID="${RUN_ID}" \
    FT_REMOTE_WORK_DIR="target/rch-e2e-ft-gofz5-1/${RUN_ID}" \
    FT_KEEP_ARTIFACTS="${KEEP_ARTIFACTS}" \
    FT_TEST_ID="${TEST_ID}" \
    bash -lc "${remote_script}"
  rch_rc=$?
  set -e

  e2e_add_file "rch-cargo-install.log" < "${rch_log}"
  if [[ ${rch_rc} -ne 0 ]]; then
    return "${rch_rc}"
  fi

  awk '
    /^FT_GOFZ5_REPORT_BEGIN$/ { capture = 1; next }
    /^FT_GOFZ5_REPORT_END$/ { capture = 0; next }
    capture { print }
  ' "${rch_log}" > "${remote_report_file}"

  if ! jq -e '
    .schema_version == "ft.cargo_install.validation.v1"
    and .validation.execution == "rch"
    and (.install.exit_code == 0)
    and (.verification.version_exit == 0)
    and (.verification.doctor_ok | type == "boolean")
  ' "${remote_report_file}" >/dev/null; then
    echo "remote cargo install report missing or invalid" >&2
    cat "${remote_report_file}" >&2 || true
    return 1
  fi

  jq -r '.outputs.install_command // ""' "${remote_report_file}" | e2e_add_file "install-command.txt"
  jq -r '.outputs.install_stdout // ""' "${remote_report_file}" | e2e_add_file "cargo-install.stdout.log"
  jq -r '.outputs.install_stderr // ""' "${remote_report_file}" | e2e_add_file "cargo-install.stderr.log"
  jq -r '.outputs.version_stdout // ""' "${remote_report_file}" | e2e_add_file "ft-version.stdout.log"
  jq -r '.outputs.version_stderr // ""' "${remote_report_file}" | e2e_add_file "ft-version.stderr.log"
  jq -r '.outputs.doctor_stdout // ""' "${remote_report_file}" | e2e_add_file "ft-doctor.stdout.log"
  jq -r '.outputs.doctor_stderr // ""' "${remote_report_file}" | e2e_add_file "ft-doctor.stderr.log"
  jq -r '.outputs.df_before // ""' "${remote_report_file}" | e2e_add_file "df-before.txt"
  jq -r '.outputs.df_after // ""' "${remote_report_file}" | e2e_add_file "df-after.txt"
  jq -r '.outputs.disk_before // ""' "${remote_report_file}" | e2e_add_file "disk-before.txt"
  jq -r '.outputs.disk_after // ""' "${remote_report_file}" | e2e_add_file "disk-after.txt"
  e2e_add_file "report.json" < "${remote_report_file}"
}

main() {
  require_cmd jq

  e2e_init_artifacts "ft-gofz5-1-cargo-install" >/dev/null
  rch_init "${E2E_RUN_DIR}" "${RUN_ID}" "ft_gofz5_1_cargo_install" "${ROOT_DIR}"

  local exit_code=0
  e2e_capture_scenario "cargo_install_validation" scenario_cargo_install_validation || exit_code=$?
  e2e_finalize "${exit_code}" >/dev/null
  return "${exit_code}"
}

main "$@"
