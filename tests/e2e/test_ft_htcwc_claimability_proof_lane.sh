#!/usr/bin/env bash
# E2E: RCH-backed claimability fixture proof lane and closeout gate.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-htcwc.6"
EPIC_ID="ft-htcwc"
SCENARIO_ID="claimability_proof_lane"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.jsonl"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
FIXTURE_STATIC_LOG="${ARTIFACT_DIR}/fixture-static.log"
DOCS_STATIC_LOG="${ARTIFACT_DIR}/docs-static.log"
SHELL_STATIC_LOG="${ARTIFACT_DIR}/shell-static.log"
EPIC_STATUS_FILE="${ARTIFACT_DIR}/epic-status.json"
EPIC_STATUS_LOG="${ARTIFACT_DIR}/epic-status.log"
DEP_CYCLES_FILE="${ARTIFACT_DIR}/br-dep-cycles.json"
DEP_CYCLES_LOG="${ARTIFACT_DIR}/br-dep-cycles.log"
CLAIMABILITY_RCH_LOG="${ARTIFACT_DIR}/claimability-rch.log"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

if [[ "${RCH_REQUIRE_REMOTE:-1}" != "1" ]]; then
    echo "FATAL: RCH_REQUIRE_REMOTE=1 is required; refusing local Cargo proof." >&2
    exit 2
fi

export RCH_REQUIRE_REMOTE=1
export RCH_QUEUE_WHEN_BUSY="${RCH_QUEUE_WHEN_BUSY:-1}"
export RCH_DAEMON_TIMEOUT_MS="${RCH_DAEMON_TIMEOUT_MS:-60000}"
export RCH_DAEMON_RESPONSE_TIMEOUT_SECS="${RCH_DAEMON_RESPONSE_TIMEOUT_SECS:-120}"
export RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS="${RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS:-1200}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-3600}"
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-2}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-2}"
export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-2}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"
REMOTE_TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-htcwc-6-claimability-proof-${RUN_ID}}"
export CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_htcwc_claimability_proof_lane"

PASS=0
FAIL=0
TOTAL=0
LOCAL_STATIC_STATUS="not_run"
REMOTE_STATUS="not_run"
RCH_SUBSTRATE_BLOCKED="false"
FAILURE_CLASSIFICATION="not_applicable"

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

emit_log() {
    local step="$1"
    local status="$2"
    local artifact_path="$3"
    local failure_class="${4:-not_applicable}"
    local source_freshness="${5:-fixture_fixed}"
    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg epic_id "${EPIC_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_path "${artifact_path}" \
        --arg failure_class "${failure_class}" \
        --arg source_freshness "${source_freshness}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          epic_id: $epic_id,
          scenario_id: $scenario_id,
          surface: "claimability-v1",
          step: $step,
          status: $status,
          correlation_id: $correlation_id,
          artifact_path: $artifact_path,
          source_freshness: $source_freshness,
          failure_classification: $failure_class
        }' >>"${STRUCTURED_LOG}"
}

record_result() {
    local step="$1"
    local ok="$2"
    local artifact_path="$3"
    local failure_class="${4:-not_applicable}"
    local source_freshness="${5:-fixture_fixed}"
    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_log "${step}" "passed" "${artifact_path}" "${failure_class}" "${source_freshness}"
    else
        FAIL=$((FAIL + 1))
        FAILURE_CLASSIFICATION="${failure_class}"
        emit_log "${step}" "failed" "${artifact_path}" "${failure_class}" "${source_freshness}"
    fi
}

write_summary() {
    local selected_workers remote_cargo_reached remote_rustc_reached test_binary_reached
    local fixture_count verdict_count final_verdict_count global_cycle_count epic_cycle_count
    selected_workers="$(jq -scr '[.[].runs[]?.selected_worker_id | select(. != null and . != "required;")] | unique' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf '[]')"
    remote_cargo_reached="$(jq -sr 'any(.[].runs[]?; .remote_cargo_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    remote_rustc_reached="$(jq -sr 'any(.[].runs[]?; .remote_rustc_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    test_binary_reached="$(jq -sr 'any(.[].runs[]?; .test_binary_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    fixture_count="$(jq -r '.cases | length' "${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json" 2>/dev/null || printf '0')"
    verdict_count="$(jq -r '.verdicts | length' "${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json" 2>/dev/null || printf '0')"
    final_verdict_count="$(jq -r '[.cases[].expected.final_verdict] | unique | length' "${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json" 2>/dev/null || printf '0')"
    global_cycle_count="$(jq -r '.count // (.cycles | length) // 0' "${DEP_CYCLES_FILE}" 2>/dev/null || printf '0')"
    epic_cycle_count="$(jq -r --arg epic_id "${EPIC_ID}" '[.cycles[]? | select(any(.[]; startswith($epic_id)))] | length' "${DEP_CYCLES_FILE}" 2>/dev/null || printf '0')"
    if [[ "${FAIL}" -eq 0 && "${REMOTE_STATUS}" == "passed" ]]; then
        FAILURE_CLASSIFICATION="not_applicable"
    elif [[ "${RCH_SUBSTRATE_BLOCKED}" == "true" ]]; then
        FAILURE_CLASSIFICATION="environment_blocked"
    elif [[ "${FAILURE_CLASSIFICATION}" == "not_applicable" ]]; then
        FAILURE_CLASSIFICATION="source_regression"
    fi

    jq -cn \
        --arg bead_id "${BEAD_ID}" \
        --arg epic_id "${EPIC_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --arg local_static_status "${LOCAL_STATIC_STATUS}" \
        --arg remote_status "${REMOTE_STATUS}" \
        --arg failure_classification "${FAILURE_CLASSIFICATION}" \
        --argjson rch_substrate_blocked "${RCH_SUBSTRATE_BLOCKED}" \
        --argjson pass_count "${PASS}" \
        --argjson fail_count "${FAIL}" \
        --argjson total_count "${TOTAL}" \
        --argjson selected_workers "${selected_workers}" \
        --argjson remote_cargo_reached "${remote_cargo_reached}" \
        --argjson remote_rustc_reached "${remote_rustc_reached}" \
        --argjson test_binary_reached "${test_binary_reached}" \
        --argjson fixture_count "${fixture_count}" \
        --argjson verdict_count "${verdict_count}" \
        --argjson final_verdict_count "${final_verdict_count}" \
        --argjson global_cycle_count "${global_cycle_count}" \
        --argjson epic_cycle_count "${epic_cycle_count}" \
        '{
          bead_id: $bead_id,
          epic_id: $epic_id,
          scenario_id: $scenario_id,
          status: (
            if $rch_substrate_blocked then
              "rch_substrate_blocked"
            elif $fail_count == 0 then
              "passed"
            else
              "failed"
            end
          ),
          correlation_id: $correlation_id,
          artifact_dir: $artifact_dir,
          remote_cargo_target_dir: $remote_target_dir,
          pass_count: $pass_count,
          fail_count: $fail_count,
          total_count: $total_count,
          failure_classification: $failure_classification,
          fixture_count: $fixture_count,
          verdict_count: $verdict_count,
          final_verdict_count: $final_verdict_count,
          closeout_gate: {
            global_cycle_count: $global_cycle_count,
            epic_cycle_count: $epic_cycle_count,
            parent_close_allowed_when_children_closed: true
          },
          evidence: {
            local_static: $local_static_status,
            remote_claimability: $remote_status,
            rch_substrate_blocked: $rch_substrate_blocked,
            local_cargo_counted_as_proof: false
          },
          remote: {
            selected_workers: $selected_workers,
            remote_cargo_reached: $remote_cargo_reached,
            remote_rustc_reached: $remote_rustc_reached,
            test_binary_reached: $test_binary_reached
          },
          artifacts: {
            commands: "commands.txt",
            structured_log: "structured.jsonl",
            stdout: "stdout.txt",
            stderr: "stderr.txt",
            fixture_static: "fixture-static.log",
            docs_static: "docs-static.log",
            shell_static: "shell-static.log",
            epic_status: "epic-status.json",
            dep_cycles: "br-dep-cycles.json",
            claimability_rch: "claimability-rch.log",
            proof_ledger: "proof-ledger.jsonl"
          }
        }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

run_static_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    record_command "$*"
    set +e
    "$@" >"${log_file}" 2>&1
    local rc=$?
    set -e
    if [[ ${rc} -eq 0 ]]; then
        record_result "${step}" "true" "${log_file}"
    else
        LOCAL_STATIC_STATUS="failed"
        record_result "${step}" "false" "${log_file}" "source_regression"
        return "${rc}"
    fi
}

run_rch_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    record_command "run_rch_cargo_logged $*"
    set +e
    run_rch_cargo_logged "${log_file}" "$@"
    local rc=$?
    set -e
    if [[ ${rc} -eq 0 ]]; then
        REMOTE_STATUS="passed"
        record_result "${step}" "true" "${log_file}" "not_applicable" "rch_live"
        return 0
    fi
    REMOTE_STATUS="failed"
    local failure_class="source_regression"
    if [[ -f "${log_file}.rch_meta.json" ]] \
        && jq -e '
          .timed_out == true
          or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE"
          or .failure_reason_code == "RCH-REMOTE-STALL"
          or .wrapper_exit_code == 124
        ' "${log_file}.rch_meta.json" >/dev/null 2>&1
    then
        RCH_SUBSTRATE_BLOCKED="true"
        REMOTE_STATUS="rch_substrate_blocked"
        failure_class="environment_blocked"
    fi
    record_result "${step}" "false" "${log_file}" "${failure_class}" "rch_live"
    return "${rc}"
}

check_fixture_contract() {
    local fixture="$1"
    if ! jq -e '
      .schema_version == 1
      and (.generated_by == "ft-htcwc.2-claimability-fixtures")
      and (.cases | length) >= 8
      and (.verdicts | length) == 8
      and ([.verdicts[].verdict] | sort) == ([
        "claimable",
        "dependency_blocked",
        "dirty_overlap",
        "external_wait",
        "mail_degraded",
        "no_ready",
        "owner_blocked",
        "tracker_inconsistent"
      ] | sort)
      and ([.cases[].expected.final_verdict] | unique | length) == 8
      and ([.cases[].id] | index("bv_blocked_available_mismatch") != null)
      and ([.cases[].source_commands[]]
        | map(test("am service restart|am service stop|am doctor fix|am doctor repair|rch daemon restart|git[[:space:]]+reset[[:space:]]+--hard|git[[:space:]]+clean[[:space:]]+-[[:alnum:]]*f[[:alnum:]]*d|rm[[:space:]]+-[[:alnum:]]*r[[:alnum:]]*f|kill "; "i"))
        | any
        | not)
    ' "${fixture}" >/dev/null
    then
        return 1
    fi
    jq -r '
      "fixture_count=\(.cases | length)",
      "verdict_count=\(.verdicts | length)",
      "final_verdict_count=\([.cases[].expected.final_verdict] | unique | length)",
      "observed_mismatch_present=\([.cases[].id] | index("bv_blocked_available_mismatch") != null)"
    ' "${fixture}"
}

check_docs_contract() {
    local runbook="$1"
    local contract="$2"
    if ! grep -F "build_claimability_report_from_value" "${runbook}"; then
        return 1
    fi
    if ! grep -F "ft-e87u6.2" "${runbook}"; then
        return 1
    fi
    if ! grep -F "tracker_inconsistent" "${contract}"; then
        return 1
    fi
    if ! grep -F "claimability" "${contract}"; then
        return 1
    fi
}

check_epic_closeout_gate() {
    if ! br show "${EPIC_ID}" --json >"${EPIC_STATUS_FILE}"; then
        return 1
    fi
    if ! jq -e --arg epic_id "${EPIC_ID}" --arg bead_id "${BEAD_ID}" '
      .[0] as $epic
      | ($epic.id == $epic_id)
      and (([$epic.dependents[] | select(.dependency_type == "parent-child") | .id] | sort) == [
        "ft-htcwc.1",
        "ft-htcwc.2",
        "ft-htcwc.3",
        "ft-htcwc.4",
        "ft-htcwc.5",
        "ft-htcwc.6"
      ])
      and (all($epic.dependents[] | select(.dependency_type == "parent-child" and .id != $bead_id); .status == "closed"))
      and (any($epic.dependents[]; .id == $bead_id and (.status == "in_progress" or .status == "closed")))
    ' "${EPIC_STATUS_FILE}" >/dev/null
    then
        return 1
    fi
    jq -r '
      .[0].dependents
      | map(select(.dependency_type == "parent-child"))
      | sort_by(.id)
      | .[]
      | "\(.id)=\(.status)"
    ' "${EPIC_STATUS_FILE}"
}

check_epic_cycle_status() {
    if ! br dep cycles --json >"${DEP_CYCLES_FILE}"; then
        return 1
    fi
    if ! jq -e --arg epic_id "${EPIC_ID}" '
      (.cycles // []) as $cycles
      | [$cycles[] | select(any(.[]; startswith($epic_id)))] as $epic_cycles
      | ($epic_cycles | length) == 0
    ' "${DEP_CYCLES_FILE}" >/dev/null
    then
        return 1
    fi
    jq -r --arg epic_id "${EPIC_ID}" '
      (.count // (.cycles | length) // 0) as $global_count
      | [.cycles[]? | select(any(.[]; startswith($epic_id)))] as $epic_cycles
      | "global_cycle_count=\($global_count)",
        "epic_cycle_count=\($epic_cycles | length)"
    ' "${DEP_CYCLES_FILE}"
}

echo "=== ${BEAD_ID} claimability proof lane ==="
: >"${PROOF_LEDGER_FILE}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"

run_static_step \
    "claimability-fixture-contract" \
    "${FIXTURE_STATIC_LOG}" \
    check_fixture_contract \
    "${ROOT_DIR}/crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json"

run_static_step \
    "claimability-docs-linked" \
    "${DOCS_STATIC_LOG}" \
    check_docs_contract \
    "${ROOT_DIR}/docs/blocker-radar-runbook.md" \
    "${ROOT_DIR}/docs/blocker-radar-contract.md"

run_static_step "e2e-shell-valid" "${SHELL_STATIC_LOG}" bash -n "${BASH_SOURCE[0]}"

run_static_step "epic-closeout-prereqs" "${EPIC_STATUS_LOG}" check_epic_closeout_gate

run_static_step "epic-cycle-free" "${DEP_CYCLES_LOG}" check_epic_cycle_status

LOCAL_STATIC_STATUS="passed"

ensure_rch_ready

if run_rch_step \
    "claimability-reconciler-rch" \
    "${CLAIMABILITY_RCH_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
    cargo test -p frankenterm-core --test blocker_radar_conformance claimability -- --nocapture
then
    :
fi

echo "summary=${SUMMARY_FILE}"
[[ "${FAIL}" -eq 0 ]]
