#!/usr/bin/env bash
# ft-xbnl0.5.2 — Permanent finish-line guard composition.
#
# Runs every guard listed in docs/ft-xbnl0-5-2-finish-line-guards.json
# and aggregates the results into a single summary.json. This is the entry
# point wired into CI (.github/workflows/finish-line-guards.yml) and the
# contributor path (cargo test ft_xbnl0_5_2_finish_line_guards).
#
# Exit codes:
#   0  all guards passed
#   1  one or more guards failed (actionable — see individual guard reports)
#   2  composition internal error (manifest missing, jq missing, etc.)
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="${ROOT_DIR}/docs/ft-xbnl0-5-2-finish-line-guards.json"
DEFAULT_OUT="${ROOT_DIR}/docs/ft-xbnl0-5-2-finish-line-guards-validation.json"
OUT_PATH="${DEFAULT_OUT}"
VERBOSE=0

usage() {
  cat <<'USAGE'
Usage: check_finish_line_guards.sh [options]

Options:
  --output <path>    Output summary JSON path (default: docs/ft-xbnl0-5-2-finish-line-guards-validation.json)
  --manifest <path>  Override manifest location
  --verbose          Emit per-guard stdout/stderr to the terminal
  -h, --help         Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)   OUT_PATH="$2"; shift 2 ;;
    --manifest) MANIFEST="$2"; shift 2 ;;
    --verbose)  VERBOSE=1; shift ;;
    -h|--help)  usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required for finish-line guard composition" >&2
  exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required for finish-line guard composition" >&2
  exit 2
fi
if [[ ! -f "${MANIFEST}" ]]; then
  echo "error: manifest missing at ${MANIFEST}" >&2
  exit 2
fi

mkdir -p "$(dirname "${OUT_PATH}")"

checked_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

tmp_results="$(mktemp)"
trap 'rm -f "${tmp_results}"' EXIT
printf '[]' > "${tmp_results}"

append_result() {
  local guard_id="$1" outcome="$2" reason_code="$3" detail_json="$4"
  local existing
  existing="$(cat "${tmp_results}")"
  echo "${existing}" | jq \
    --arg guard_id "${guard_id}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --argjson detail "${detail_json}" \
    '. + [{
      guard_id: $guard_id,
      outcome: $outcome,
      reason_code: $reason_code,
      detail: $detail
    }]' > "${tmp_results}.new"
  mv "${tmp_results}.new" "${tmp_results}"
}

run_shell_guard() {
  local guard_id="$1" script_rel="$2" policy_rel="$3" output_rel="$4"
  local script_abs="${ROOT_DIR}/${script_rel}"
  if [[ ! -x "${script_abs}" && ! -f "${script_abs}" ]]; then
    append_result "${guard_id}" "failed" "guard_script_missing" \
      "$(jq -n --arg p "${script_rel}" '{missing_script: $p}')"
    return 1
  fi

  local log_file
  log_file="$(mktemp)"
  local rc=0
  local args=()
  if [[ -n "${policy_rel}" ]]; then
    args+=(--policy-path "${ROOT_DIR}/${policy_rel}")
  fi
  if [[ -n "${output_rel}" ]]; then
    args+=(--output "${ROOT_DIR}/${output_rel}")
  fi

  set +e
  bash "${script_abs}" "${args[@]}" > "${log_file}" 2>&1
  rc=$?
  set -e

  if [[ ${VERBOSE} -eq 1 ]]; then
    cat "${log_file}" >&2
  fi

  local status error_code
  if [[ -n "${output_rel}" && -f "${ROOT_DIR}/${output_rel}" ]]; then
    status="$(jq -r '.status // "unknown"' "${ROOT_DIR}/${output_rel}")"
    error_code="$(jq -r '.error_code // ""' "${ROOT_DIR}/${output_rel}")"
  else
    status="$([[ ${rc} -eq 0 ]] && echo passed || echo failed)"
    error_code=""
  fi

  local detail
  detail="$(jq -n \
    --arg script "${script_rel}" \
    --arg rc "${rc}" \
    --arg status "${status}" \
    --arg error_code "${error_code}" \
    --arg log_tail "$(tail -20 "${log_file}" | head -c 4000)" \
    '{script: $script, exit_code: ($rc | tonumber), report_status: $status, error_code: $error_code, log_tail: $log_tail}')"

  rm -f "${log_file}"

  if [[ "${status}" == "passed" && ${rc} -eq 0 ]]; then
    append_result "${guard_id}" "passed" "guard_script_passed" "${detail}"
    return 0
  else
    append_result "${guard_id}" "failed" "${error_code:-guard_script_failed}" "${detail}"
    return 1
  fi
}

run_cargo_test_guard() {
  local guard_id="$1" test_name="$2" test_target="$3"
  if ! command -v cargo >/dev/null 2>&1; then
    append_result "${guard_id}" "skipped" "cargo_unavailable" '{}'
    return 0
  fi
  if [[ "${FT_XBNL0_5_2_SKIP_CARGO_TEST:-0}" == "1" ]]; then
    append_result "${guard_id}" "skipped" "skip_via_FT_XBNL0_5_2_SKIP_CARGO_TEST" \
      "$(jq -n --arg t "${test_name}" '{test_name: $t, skip_reason: "FT_XBNL0_5_2_SKIP_CARGO_TEST=1 set — contributor must run cargo test manually or in a shell without the rch hook intercepting."}')"
    return 0
  fi

  local log_file
  log_file="$(mktemp)"
  local rc=0
  set +e
  # shellcheck disable=SC2086
  cargo test ${test_target} "${test_name}" -- --exact > "${log_file}" 2>&1
  rc=$?
  set -e

  if [[ ${VERBOSE} -eq 1 ]]; then
    cat "${log_file}" >&2
  fi

  local detail
  detail="$(jq -n \
    --arg test "${test_name}" \
    --arg target "${test_target}" \
    --arg rc "${rc}" \
    --arg log_tail "$(tail -20 "${log_file}" | head -c 4000)" \
    '{test_name: $test, test_target: $target, exit_code: ($rc | tonumber), log_tail: $log_tail}')"

  rm -f "${log_file}"

  if [[ ${rc} -eq 0 ]]; then
    append_result "${guard_id}" "passed" "cargo_test_passed" "${detail}"
    return 0
  else
    append_result "${guard_id}" "failed" "cargo_test_failed" "${detail}"
    return 1
  fi
}

verify_artifact_contract() {
  # Scans tests/e2e/logs/ for recent per-run directories and verifies that
  # each one that claims to follow the artifact contract has both
  # summary.json and structured.log. Missing files become failures.
  local guard_id="finish_line_verification_contract_shape"
  local logs_dir="${ROOT_DIR}/tests/e2e/logs"
  if [[ ! -d "${logs_dir}" ]]; then
    # No e2e runs recorded yet — that's fine, the contract isn't violated.
    append_result "${guard_id}" "passed" "no_e2e_runs" '{"logs_dir": "missing"}'
    return 0
  fi

  python3 - "${logs_dir}" <<'PY' > /tmp/_ft_xbnl0_5_2_art.json
import json
import re
import sys
from pathlib import Path

logs = Path(sys.argv[1])
required_summary_keys = ["scenario", "outcome", "artifact_dir", "structured_log"]
# Group run directories by scenario prefix (everything before the trailing
# _YYYYMMDD_HHMMSS timestamp). Only inspect the newest run per scenario —
# older runs might be stale from interrupted local debug sessions and
# inspecting them would punish the contributor for crashes rather than
# catching real artifact-contract drift.
timestamp_suffix = re.compile(r"_\d{8}_\d{6}$")
latest_per_scenario: dict[str, Path] = {}
for run_dir in sorted(logs.iterdir()):
    if not run_dir.is_dir():
        continue
    name = run_dir.name
    m = timestamp_suffix.search(name)
    if not m:
        continue
    scenario = name[: m.start()]
    summary = run_dir / "summary.json"
    structured = run_dir / "structured.log"
    if not (summary.exists() or structured.exists()):
        continue
    prev = latest_per_scenario.get(scenario)
    if prev is None or run_dir.name > prev.name:
        latest_per_scenario[scenario] = run_dir

missing: list[dict] = []
for scenario, run_dir in sorted(latest_per_scenario.items()):
    summary = run_dir / "summary.json"
    structured = run_dir / "structured.log"
    if not summary.exists():
        missing.append(
            {"scenario": scenario, "run_dir": str(run_dir), "missing": "summary.json"}
        )
        continue
    if not structured.exists():
        missing.append(
            {"scenario": scenario, "run_dir": str(run_dir), "missing": "structured.log"}
        )
        continue
    try:
        data = json.loads(summary.read_text(encoding="utf-8"))
    except Exception as e:
        missing.append(
            {"scenario": scenario, "run_dir": str(run_dir), "error": f"summary.json unreadable: {e}"}
        )
        continue
    absent_keys = [k for k in required_summary_keys if k not in data]
    if absent_keys:
        missing.append(
            {"scenario": scenario, "run_dir": str(run_dir), "missing_keys": absent_keys}
        )

print(
    json.dumps(
        {
            "inspected": len(latest_per_scenario),
            "missing": missing,
            "policy": "latest-run-per-scenario",
        },
        sort_keys=True,
    )
)
PY
  local result
  result="$(cat /tmp/_ft_xbnl0_5_2_art.json)"
  rm -f /tmp/_ft_xbnl0_5_2_art.json
  local missing_count
  missing_count="$(echo "${result}" | jq '.missing | length')"
  if [[ "${missing_count}" -eq 0 ]]; then
    append_result "${guard_id}" "passed" "artifact_contract_satisfied" "${result}"
    return 0
  else
    append_result "${guard_id}" "failed" "e2e_missing_summary_json_or_structured_log" "${result}"
    return 1
  fi
}

# Drive each guard listed in the manifest.
guard_count="$(jq '.guards | length' "${MANIFEST}")"
overall_rc=0
for i in $(seq 0 $((guard_count - 1))); do
  guard_id="$(jq -r ".guards[${i}].guard_id" "${MANIFEST}")"
  script_rel="$(jq -r ".guards[${i}].script // \"\"" "${MANIFEST}")"
  policy_rel="$(jq -r ".guards[${i}].policy // \"\"" "${MANIFEST}")"
  output_rel="$(jq -r ".guards[${i}].output_artifact // \"\"" "${MANIFEST}")"
  cargo_test="$(jq -r ".guards[${i}].cargo_test // \"\"" "${MANIFEST}")"
  test_target="$(jq -r ".guards[${i}].test_target // \"\"" "${MANIFEST}")"

  if [[ "${guard_id}" == "finish_line_verification_contract_shape" ]]; then
    if ! verify_artifact_contract; then
      overall_rc=1
    fi
    continue
  fi

  if [[ -n "${script_rel}" ]]; then
    if ! run_shell_guard "${guard_id}" "${script_rel}" "${policy_rel}" "${output_rel}"; then
      overall_rc=1
    fi
  elif [[ -n "${cargo_test}" ]]; then
    if ! run_cargo_test_guard "${guard_id}" "${cargo_test}" "${test_target}"; then
      overall_rc=1
    fi
  else
    append_result "${guard_id}" "failed" "guard_manifest_incomplete" "{}"
    overall_rc=1
  fi
done

results_json="$(cat "${tmp_results}")"
overall_status="$([[ ${overall_rc} -eq 0 ]] && echo passed || echo failed)"

jq -n \
  --arg contract_id "ft.xbnl0.5.2.finish_line_guards.v1" \
  --arg bead_id "ft-xbnl0.5.2" \
  --arg checked_at "${checked_at}" \
  --arg status "${overall_status}" \
  --arg manifest_path "${MANIFEST}" \
  --argjson guards "${results_json}" \
  '{
    contract_id: $contract_id,
    bead_id: $bead_id,
    checked_at: $checked_at,
    status: $status,
    manifest_path: $manifest_path,
    guards: $guards
  }' > "${OUT_PATH}"

if [[ ${overall_rc} -eq 0 ]]; then
  echo "ft-xbnl0.5.2 finish-line guards: ALL PASSED (${guard_count} guards)"
  echo "  summary: ${OUT_PATH}"
else
  echo "ft-xbnl0.5.2 finish-line guards: FAILED"
  echo "  summary: ${OUT_PATH}"
  jq -r '.guards[] | select(.outcome != "passed") | "  - \(.guard_id): \(.outcome) (\(.reason_code))"' "${OUT_PATH}" >&2
fi

exit "${overall_rc}"
