#!/usr/bin/env bash
# Shared rch fail-closed guard library for E2E test harnesses.
#
# Source this from any E2E harness that uses `rch exec -- cargo ...`:
#
#   source "$(dirname "$0")/lib_rch_guards.sh"
#   rch_init "${LOG_DIR}" "${run_id}" "harness_name"
#   ensure_rch_ready
#
# Then use `run_rch_cargo_logged <output_file> <cargo args...>` instead of
# bare `rch exec -- env ... cargo ...`.
#
# Environment:
#   RCH_GITHUB_ACTIONS_LOCAL_CARGO
#       Set to 1 only in GitHub-hosted Actions jobs where the hosted runner is
#       the CI execution target and the local agent/operator RCH offload policy
#       is not available.
#
# Provides:
#   rch_init()                 - Set up variables (call once at start)
#   ensure_rch_ready()         - Preflight: probe workers + smoke cargo check
#   ensure_rch_remote_only_preflight() - Queue-aware remote-only proof preflight
#   run_rch_logged_with_timeout() - Timeout-wrapped non-Cargo rch command
#   run_rch_cargo_logged()     - Timeout-wrapped rch cargo with stall/fallback detection
#   rch_write_meta_json()      - Persist worker/exit/timing metadata for an rch log
#   rch_emit_proof_ledger_entry() - Optional proof-ledger JSONL emission
#   check_rch_fallback()       - Fatal if rch entered a fail-open/off-policy path
#   run_rch()                  - TMPDIR-safe rch wrapper
#   resolve_timeout_bin()      - Find timeout or gtimeout

# Guard against double-sourcing.
[[ -n "${_LIB_RCH_GUARDS_LOADED:-}" ]] && return 0
_LIB_RCH_GUARDS_LOADED=1

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
RCH_SMOKE_TIMEOUT_SECS="${RCH_SMOKE_TIMEOUT_SECS:-600}"
RCH_PROBE_TIMEOUT_SECS="${RCH_PROBE_TIMEOUT_SECS:-120}"
RCH_LOCAL_TMPDIR="${RCH_LOCAL_TMPDIR:-/tmp}"
RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
# Set this to 1 for harnesses whose first material verification steps already
# run through `run_rch_cargo_logged`. That keeps remote execution fail-closed
# without paying a duplicate full-repo sync for a cargo smoke command.
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-0}"
RCH_SKIP_QUEUE_PREFLIGHT="${RCH_SKIP_QUEUE_PREFLIGHT:-0}"
RCH_SKIP_WORKER_SELECTION_PREFLIGHT="${RCH_SKIP_WORKER_SELECTION_PREFLIGHT:-0}"
RCH_WORKER_SELECTION_WAIT_SECS="${RCH_WORKER_SELECTION_WAIT_SECS:-0}"
RCH_WORKER_SELECTION_POLL_SECS="${RCH_WORKER_SELECTION_POLL_SECS:-15}"
RCH_REMOTE_PREFLIGHT_WAIT_SECS="${RCH_REMOTE_PREFLIGHT_WAIT_SECS:-${RCH_WORKER_SELECTION_WAIT_SECS}}"
RCH_MIRROR_REQUIRED_PATHS="${RCH_MIRROR_REQUIRED_PATHS:-}"
RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS="${RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS:-auto}"
RCH_MIRROR_BLOCK_ON_STALE_HEAD="${RCH_MIRROR_BLOCK_ON_STALE_HEAD:-0}"
RCH_MIRROR_REQUIRE_ALL_CHECKED_WORKERS="${RCH_MIRROR_REQUIRE_ALL_CHECKED_WORKERS:-0}"
RCH_MIRROR_MIN_PASSING_WORKERS="${RCH_MIRROR_MIN_PASSING_WORKERS:-1}"
RCH_SELECTED_WORKER_MIRROR_PREFLIGHT="${RCH_SELECTED_WORKER_MIRROR_PREFLIGHT:-auto}"
RCH_GITHUB_ACTIONS_LOCAL_CARGO="${RCH_GITHUB_ACTIONS_LOCAL_CARGO:-0}"

# Populated by rch_init().
_RCH_PROBE_LOG=""
_RCH_QUEUE_LOG=""
_RCH_CAPABILITIES_LOG=""
_RCH_CAPABILITIES_REFRESH_LOG=""
_RCH_SMOKE_LOG=""
_RCH_REMOTE_PREFLIGHT_LOG=""
_RCH_MIRROR_PREFLIGHT_LOG=""
_RCH_SCHEDULER_WORKERS_LOG=""
_RCH_WORKER_SELECTION_LOG=""
_RCH_SMOKE_TARGET_DIR=""
_RCH_REPO_ROOT=""
TIMEOUT_BIN=""

rch_fatal() {
    echo "FATAL: $1" >&2
    exit 1
}

run_rch() {
    TMPDIR="${RCH_LOCAL_TMPDIR}" rch "$@"
}

resolve_timeout_bin() {
    if command -v timeout >/dev/null 2>&1; then
        TIMEOUT_BIN="timeout"
    elif command -v gtimeout >/dev/null 2>&1; then
        TIMEOUT_BIN="gtimeout"
    else
        TIMEOUT_BIN=""
    fi
}

rch_github_actions_local_cargo_enabled() {
    case "${RCH_GITHUB_ACTIONS_LOCAL_CARGO:-}" in
        1|true|TRUE|yes|YES) ;;
        *) return 1 ;;
    esac

    [[ "${GITHUB_ACTIONS:-}" == "true" ]]
}

rch_probe_log_path() {
    printf '%s\n' "${_RCH_PROBE_LOG}"
}

rch_smoke_log_path() {
    printf '%s\n' "${_RCH_SMOKE_LOG}"
}

rch_queue_log_path() {
    printf '%s\n' "${_RCH_QUEUE_LOG}"
}

rch_capabilities_log_path() {
    printf '%s\n' "${_RCH_CAPABILITIES_LOG}"
}

rch_capabilities_refresh_log_path() {
    printf '%s\n' "${_RCH_CAPABILITIES_REFRESH_LOG}"
}

rch_remote_preflight_log_path() {
    printf '%s\n' "${_RCH_REMOTE_PREFLIGHT_LOG}"
}

rch_mirror_preflight_log_path() {
    printf '%s\n' "${_RCH_MIRROR_PREFLIGHT_LOG}"
}

rch_scheduler_workers_log_path() {
    printf '%s\n' "${_RCH_SCHEDULER_WORKERS_LOG}"
}

rch_worker_selection_log_path() {
    printf '%s\n' "${_RCH_WORKER_SELECTION_LOG}"
}

rch_log_meta_path() {
    printf '%s.rch_meta.json\n' "$1"
}

rch_json_bool() {
    if [[ "$1" == "true" ]]; then
        printf 'true\n'
    else
        printf 'false\n'
    fi
}

rch_is_unsigned_int() {
    [[ "$1" =~ ^[0-9]+$ ]]
}

rch_truthy() {
    case "$1" in
        1|true|TRUE|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

rch_remote_only_required() {
    rch_truthy "${RCH_REQUIRE_REMOTE}"
}

rch_proof_ledger_enabled() {
    [[ -n "${RCH_PROOF_LEDGER_FILE:-}" ]]
}

rch_proof_ledger_validator() {
    printf '%s\n' "${RCH_PROOF_LEDGER_VALIDATOR:-${_RCH_REPO_ROOT}/scripts/validate_asupersync_rch_execution_policy.sh}"
}

rch_proof_ledger_require_config() {
    local validator
    validator="$(rch_proof_ledger_validator)"

    [[ -n "${RCH_PROOF_LEDGER_BEAD_ID:-}" ]] || rch_fatal "RCH_PROOF_LEDGER_FILE is set but RCH_PROOF_LEDGER_BEAD_ID is missing."
    [[ -n "${RCH_PROOF_LEDGER_SCENARIO_ID:-}" ]] || rch_fatal "RCH_PROOF_LEDGER_FILE is set but RCH_PROOF_LEDGER_SCENARIO_ID is missing."
    [[ -x "${validator}" ]] || rch_fatal "proof-ledger validator is not executable: ${validator}"
    command -v jq >/dev/null 2>&1 || rch_fatal "jq is required for proof-ledger emission."
}

rch_repo_relative_path() {
    local path="$1"
    if [[ -n "${_RCH_REPO_ROOT}" && "${path}" == "${_RCH_REPO_ROOT}/"* ]]; then
        printf '%s\n' "${path#"${_RCH_REPO_ROOT}"/}"
    else
        printf '%s\n' "${path}"
    fi
}

rch_proof_redacted_text() {
    local text="$1"
    "$(rch_proof_ledger_validator)" --redact-text "${text}" | jq -r '.redacted'
}

rch_proof_fingerprint_text() {
    local text="$1"
    "$(rch_proof_ledger_validator)" --redact-text "${text}" | jq -r '.fingerprint'
}

rch_proof_artifact_paths_fingerprint() {
    local artifact_paths_json="$1"
    local digest

    if command -v shasum >/dev/null 2>&1; then
        digest="$(printf '%s' "${artifact_paths_json}" | shasum -a 256 | awk '{print $1}')"
    elif command -v sha256sum >/dev/null 2>&1; then
        digest="$(printf '%s' "${artifact_paths_json}" | sha256sum | awk '{print $1}')"
    else
        rch_fatal "shasum or sha256sum is required for proof-ledger artifact fingerprints."
    fi

    printf 'sha256:%s\n' "${digest}"
}

rch_extract_cargo_target_dir_from_args() {
    local arg next_is_target_dir="false"
    for arg in "$@"; do
        if [[ "${next_is_target_dir}" == "true" ]]; then
            printf '%s\n' "${arg}"
            return 0
        fi
        case "${arg}" in
            CARGO_TARGET_DIR=*)
                printf '%s\n' "${arg#CARGO_TARGET_DIR=}"
                return 0
                ;;
            CARGO_TARGET_DIR)
                next_is_target_dir="true"
                ;;
        esac
    done
    printf '%s\n' "not_applicable"
}

rch_meta_json_field() {
    local meta_file="$1"
    local jq_expr="$2"
    if [[ -f "${meta_file}" ]]; then
        jq -r "${jq_expr} // \"\"" "${meta_file}" 2>/dev/null || true
    fi
}

rch_current_repo_snapshot_head() {
    if [[ -n "${_RCH_REPO_ROOT}" ]] && git -C "${_RCH_REPO_ROOT}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git -C "${_RCH_REPO_ROOT}" rev-parse HEAD 2>/dev/null || printf '%s\n' "unknown"
    else
        printf '%s\n' "unknown"
    fi
}

rch_log_remote_cargo_reached() {
    local output_file="$1"
    rch_log_has_remote_execution_marker "${output_file}" || return 1
    grep -Eq "Remote command finished:|Compiling |Checking |Finished |error\\[E[0-9]+\\]|error:" "${output_file}" 2>/dev/null
}

rch_log_remote_rustc_reached() {
    local output_file="$1"
    rch_log_has_remote_execution_marker "${output_file}" || return 1
    grep -Eq "Compiling |Checking |Finished |error\\[E[0-9]+\\]" "${output_file}" 2>/dev/null
}

rch_log_test_binary_reached() {
    local output_file="$1"
    rch_log_has_remote_execution_marker "${output_file}" || return 1
    grep -Eq "running [0-9]+ tests?|test result:" "${output_file}" 2>/dev/null
}

rch_log_remote_required_refused_local_fallback() {
    local output_file="$1"
    grep -Fq "remote required; refusing local fallback" "${output_file}" 2>/dev/null
}

rch_log_blocked_by_active_project_exclusion() {
    local output_file="$1"
    grep -Fq "active_project_exclusion=1" "${output_file}" 2>/dev/null
}

rch_emit_proof_ledger_entry() {
    local command_text="$1"
    local log_file="$2"
    local wrapper_exit_code="${3:-0}"
    local target_dir="${4:-not_applicable}"
    local target_dir_lifecycle="${5:-retained}"
    local residual_risk_notes="${6:-}"

    rch_proof_ledger_enabled || return 0
    rch_proof_ledger_require_config

    local validator meta_file log_artifact meta_artifact artifact_paths_json artifact_paths_fingerprint
    local redacted_command command_fingerprint classified command_class is_heavy used_rch
    local selected_worker worker_context redacted_worker worker_context_fingerprint
    local redacted_target target_dir_fingerprint redacted_residual residual_risk_notes_fingerprint
    local fail_open timed_out remote_duration_ms elapsed_seconds execution_mode validation_status
    local failure_reason_code failure_reason_detail remote_exit_status
    local intended_worker repo_snapshot_head source_mirror_status source_mirror_reason_code
    local worker_queue_state worker_evidence_confidence
    local remote_cargo_reached remote_rustc_reached test_binary_reached
    local remote_required_refused

    validator="$(rch_proof_ledger_validator)"
    meta_file="$(rch_log_meta_path "${log_file}")"
    log_artifact="$(rch_repo_relative_path "${log_file}")"
    meta_artifact="$(rch_repo_relative_path "${meta_file}")"
    artifact_paths_json="$(jq -cn --arg log "${log_artifact}" --arg meta "${meta_artifact}" '[$log, $meta]')"
    artifact_paths_fingerprint="$(rch_proof_artifact_paths_fingerprint "${artifact_paths_json}")"

    redacted_command="$(rch_proof_redacted_text "${command_text}")"
    command_fingerprint="$(rch_proof_fingerprint_text "${command_text}")"
    classified="$("${validator}" --classify "${redacted_command}")"
    command_class="$(jq -r '.command_class' <<<"${classified}")"
    is_heavy="$(jq -r '.is_heavy' <<<"${classified}")"
    used_rch="$(jq -r '.used_rch' <<<"${classified}")"

    selected_worker="$(rch_meta_json_field "${meta_file}" '.selected_worker')"
    fail_open="$(rch_meta_json_field "${meta_file}" '.fail_open_detected')"
    timed_out="$(rch_meta_json_field "${meta_file}" '.timed_out')"
    remote_duration_ms="$(rch_meta_json_field "${meta_file}" '.remote_duration_ms')"
    failure_reason_code="$(rch_meta_json_field "${meta_file}" '.failure_reason_code')"
    failure_reason_detail="$(rch_meta_json_field "${meta_file}" '.failure_reason_detail')"
    remote_exit_status="$(rch_meta_json_field "${meta_file}" '.remote_exit_code')"
    intended_worker="${RCH_PROOF_LEDGER_INTENDED_WORKER_ID:-}"
    repo_snapshot_head="$(rch_current_repo_snapshot_head)"
    remote_required_refused="false"
    if [[ -f "${log_file}" ]] && rch_log_remote_required_refused_local_fallback "${log_file}"; then
        remote_required_refused="true"
    fi

    if [[ "${remote_required_refused}" == "true" ]]; then
        worker_context="local_fallback_refused"
    elif [[ "${fail_open}" == "true" ]]; then
        worker_context="local_fallback"
    elif [[ -n "${selected_worker}" ]]; then
        worker_context="worker=${selected_worker}"
    else
        worker_context="worker=unknown"
    fi
    redacted_worker="$(rch_proof_redacted_text "${worker_context}")"
    worker_context_fingerprint="$(rch_proof_fingerprint_text "${worker_context}")"

    redacted_target="$(rch_proof_redacted_text "${target_dir}")"
    target_dir_fingerprint="$(rch_proof_fingerprint_text "${target_dir}")"
    redacted_residual="$(rch_proof_redacted_text "${residual_risk_notes:-${failure_reason_detail}}")"
    residual_risk_notes_fingerprint="$(rch_proof_fingerprint_text "${residual_risk_notes:-${failure_reason_detail}}")"

    if [[ -n "${remote_duration_ms}" && "${remote_duration_ms}" =~ ^[0-9]+$ ]]; then
        elapsed_seconds="$(jq -n --arg ms "${remote_duration_ms}" '$ms | tonumber / 1000')"
    else
        elapsed_seconds="0"
    fi

    if [[ "${is_heavy}" == "false" && "${used_rch}" == "false" ]]; then
        execution_mode="local_light"
    else
        execution_mode="remote_rch"
    fi
    validation_status="valid"
    if [[ "${fail_open}" == "true" ]]; then
        if [[ "${remote_required_refused}" == "true" ]]; then
            execution_mode="refused_local_fallback"
            validation_status="fallback_refused"
            failure_reason_code="${failure_reason_code:-RCH-REMOTE-REQUIRED-FALLBACK-REFUSED}"
        else
            execution_mode="local_fallback"
            validation_status="fallback_required"
            failure_reason_code="${failure_reason_code:-RCH-LOCAL-FALLBACK}"
        fi
    elif [[ "${timed_out}" == "true" ]]; then
        validation_status="timeout"
        failure_reason_code="${failure_reason_code:-RCH-REMOTE-STALL}"
    elif [[ "${wrapper_exit_code}" != "0" ]]; then
        validation_status="invalid"
    fi

    if [[ "${remote_exit_status}" =~ ^-?[0-9]+$ ]]; then
        wrapper_exit_code="${remote_exit_status}"
    fi

    worker_queue_state="unknown"
    if [[ "${timed_out}" == "true" ]]; then
        worker_queue_state="queue_timeout"
    elif [[ "${remote_required_refused}" == "true" && -f "${log_file}" ]] && rch_log_blocked_by_active_project_exclusion "${log_file}"; then
        worker_queue_state="busy_wait"
    elif [[ "${remote_required_refused}" == "true" && -f "${log_file}" ]] && grep -Fq "queue_timeout" "${log_file}" 2>/dev/null; then
        worker_queue_state="queue_timeout"
    elif [[ "${fail_open}" == "true" ]]; then
        worker_queue_state="unsupported_worker_selection"
    elif [[ -n "${selected_worker}" ]]; then
        worker_queue_state="ready"
    fi

    source_mirror_status="not_checked"
    source_mirror_reason_code=""
    if [[ "${failure_reason_code}" == "RCH-REMOTE-MIRROR-MISSING-FILE" ]]; then
        source_mirror_status="missing"
        source_mirror_reason_code="${failure_reason_code}"
    elif [[ -n "${selected_worker}" && "${repo_snapshot_head}" =~ ^[a-f0-9]{40}$ ]]; then
        source_mirror_status="present"
    elif [[ -n "${selected_worker}" ]]; then
        source_mirror_status="unknown"
    fi

    remote_cargo_reached="false"
    remote_rustc_reached="false"
    test_binary_reached="false"
    if [[ "${fail_open}" != "true" && "${timed_out}" != "true" && "${is_heavy}" == "true" && -f "${log_file}" ]]; then
        if rch_log_remote_cargo_reached "${log_file}"; then
            remote_cargo_reached="true"
        fi
        if rch_log_remote_rustc_reached "${log_file}"; then
            remote_rustc_reached="true"
        fi
        if rch_log_test_binary_reached "${log_file}"; then
            test_binary_reached="true"
        fi
    fi

    worker_evidence_confidence="legacy_unknown_worker_evidence"
    if [[ "${fail_open}" == "true" || "${timed_out}" == "true" || "${wrapper_exit_code}" != "0" ]]; then
        worker_evidence_confidence="inconclusive_worker_evidence"
    elif [[ "${is_heavy}" == "true" && "${used_rch}" == "true" && "${execution_mode}" == "remote_rch" && -n "${selected_worker}" ]]; then
        if [[ -n "${intended_worker}" && "${intended_worker}" == "${selected_worker}" ]]; then
            worker_evidence_confidence="target_worker_remote_proof"
        else
            worker_evidence_confidence="scheduler_selected_remote_proof"
        fi
    elif [[ "${is_heavy}" == "false" && "${used_rch}" == "true" ]]; then
        worker_evidence_confidence="worker_self_test_only"
    fi

    mkdir -p "$(dirname "${RCH_PROOF_LEDGER_FILE}")"
    jq -cn \
        --argjson schema_version 3 \
        --arg bead_id "${RCH_PROOF_LEDGER_BEAD_ID}" \
        --arg policy_version "3.2.0" \
        --arg scenario_id "${RCH_PROOF_LEDGER_SCENARIO_ID}" \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg command "${redacted_command}" \
        --arg command_fingerprint "${command_fingerprint}" \
        --arg command_class "${command_class}" \
        --arg worker_context "${redacted_worker}" \
        --arg worker_context_fingerprint "${worker_context_fingerprint}" \
        --arg worker_evidence_confidence "${worker_evidence_confidence}" \
        --arg intended_worker_id "${intended_worker}" \
        --arg selected_worker_id "${selected_worker}" \
        --arg worker_queue_state "${worker_queue_state}" \
        --arg repo_snapshot_head "${repo_snapshot_head}" \
        --arg source_mirror_status "${source_mirror_status}" \
        --arg source_mirror_reason_code "${source_mirror_reason_code}" \
        --argjson remote_cargo_reached "${remote_cargo_reached}" \
        --argjson remote_rustc_reached "${remote_rustc_reached}" \
        --argjson test_binary_reached "${test_binary_reached}" \
        --arg execution_mode "${execution_mode}" \
        --arg target_dir "${redacted_target}" \
        --arg target_dir_fingerprint "${target_dir_fingerprint}" \
        --arg target_dir_lifecycle "${target_dir_lifecycle}" \
        --argjson artifact_paths "${artifact_paths_json}" \
        --arg artifact_paths_fingerprint "${artifact_paths_fingerprint}" \
        --argjson elapsed_seconds "${elapsed_seconds}" \
        --argjson exit_status "${wrapper_exit_code}" \
        --arg residual_risk_notes "${redacted_residual}" \
        --arg residual_risk_notes_fingerprint "${residual_risk_notes_fingerprint}" \
        --arg validation_status "${validation_status}" \
        --arg fallback_reason_code "${failure_reason_code}" \
        --argjson is_heavy "${is_heavy}" \
        --argjson used_rch "${used_rch}" \
        '{
          schema_version: $schema_version,
          bead_id: $bead_id,
          policy_version: $policy_version,
          scenario_id: $scenario_id,
          runs: [{
            timestamp: $timestamp,
            command: $command,
            command_fingerprint: $command_fingerprint,
            command_class: $command_class,
            is_heavy: $is_heavy,
            used_rch: $used_rch,
            worker_context: $worker_context,
            worker_context_fingerprint: $worker_context_fingerprint,
            worker_evidence_confidence: $worker_evidence_confidence,
            intended_worker_id: (if $intended_worker_id == "" then null else $intended_worker_id end),
            selected_worker_id: (if $selected_worker_id == "" then null else $selected_worker_id end),
            worker_queue_state: $worker_queue_state,
            repo_snapshot_head: (if $repo_snapshot_head == "" then "unknown" else $repo_snapshot_head end),
            source_mirror_status: $source_mirror_status,
            source_mirror_reason_code: (if $source_mirror_reason_code == "" then null else $source_mirror_reason_code end),
            remote_cargo_reached: $remote_cargo_reached,
            remote_rustc_reached: $remote_rustc_reached,
            test_binary_reached: $test_binary_reached,
            execution_mode: $execution_mode,
            target_dir: $target_dir,
            target_dir_fingerprint: $target_dir_fingerprint,
            target_dir_lifecycle: $target_dir_lifecycle,
            artifact_paths: $artifact_paths,
            artifact_paths_fingerprint: $artifact_paths_fingerprint,
            elapsed_seconds: $elapsed_seconds,
            exit_status: $exit_status,
            residual_risk_notes: $residual_risk_notes,
            residual_risk_notes_fingerprint: $residual_risk_notes_fingerprint,
            validation_status: $validation_status
          } + (if $fallback_reason_code == "" then {} else {fallback_reason_code: $fallback_reason_code} end)]
        }' >>"${RCH_PROOF_LEDGER_FILE}"
}

rch_validate_proof_ledger_file() {
    local ledger_file="$1"
    local validator validation_dir line_no entry_count entry entry_file validation_log

    rch_proof_ledger_require_config
    validator="$(rch_proof_ledger_validator)"

    [[ -f "${ledger_file}" ]] || rch_fatal "proof-ledger file does not exist: ${ledger_file}"

    validation_dir="${ledger_file}.validation"
    mkdir -p "${validation_dir}"

    line_no=0
    entry_count=0
    entry=""
    while IFS= read -r entry || [[ -n "${entry}" ]]; do
        line_no=$((line_no + 1))
        [[ -n "${entry}" ]] || continue

        entry_count=$((entry_count + 1))
        entry_file="${validation_dir}/entry_${line_no}.json"
        validation_log="${entry_file}.validation.log"
        printf '%s\n' "${entry}" >"${entry_file}"

        if ! "${validator}" --validate-evidence "${entry_file}" >"${validation_log}" 2>&1; then
            cat "${validation_log}" >&2
            rch_fatal "proof-ledger validation failed for ${ledger_file} line ${line_no}; see ${validation_log}"
        fi
    done <"${ledger_file}"

    [[ "${entry_count}" -gt 0 ]] || rch_fatal "proof-ledger file had no JSONL entries: ${ledger_file}"
    printf '%s\n' "${validation_dir}"
}

rch_write_meta_json() {
    local log_file="$1"
    local wrapper_exit_code="${2:-}"
    local meta_file probe_worker_count selected_worker sync_duration_ms remote_duration_ms remote_exit_code
    local failure_reason_code failure_reason_detail
    local probe_worker_ids_json="[]"
    local probe_worker_ids_raw=""
    local skipped_smoke_preflight="false"
    local reachable_workers_detected="false"
    local fail_open_detected="false"
    local timed_out="false"

    if ! command -v jq >/dev/null 2>&1; then
        rch_fatal "jq is required to write rch metadata artifacts."
    fi

    meta_file="$(rch_log_meta_path "${log_file}")"

    if [[ ! -f "${log_file}" ]]; then
        jq -cn \
            --arg log_file "${log_file}" \
            --arg wrapper_exit_code "${wrapper_exit_code}" \
            '{
              log_file: $log_file,
              missing: true,
              wrapper_exit_code: (if $wrapper_exit_code == "" then null else ($wrapper_exit_code | tonumber) end)
            }' > "${meta_file}"
        return 0
    fi

    probe_worker_ids_raw="$(rch_extract_probe_worker_ids "${log_file}")"
    if [[ -n "${probe_worker_ids_raw}" ]]; then
        probe_worker_ids_json="$(printf '%s\n' "${probe_worker_ids_raw}" | jq -R . | jq -s .)"
    fi

    if grep -q 'Smoke preflight skipped because RCH_SKIP_SMOKE_PREFLIGHT=1' "${log_file}" 2>/dev/null; then
        skipped_smoke_preflight="true"
    fi

    if probe_has_reachable_workers "${log_file}"; then
        reachable_workers_detected="true"
    fi

    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${log_file}" 2>/dev/null; then
        fail_open_detected="true"
    fi

    if [[ "${wrapper_exit_code}" == "124" || "${wrapper_exit_code}" == "137" ]]; then
        timed_out="true"
    fi

    probe_worker_count="$(rch_extract_probe_worker_count "${log_file}")"
    selected_worker="$(rch_extract_selected_worker "${log_file}")"
    sync_duration_ms="$(rch_extract_sync_duration_ms "${log_file}")"
    remote_duration_ms="$(rch_extract_remote_duration_ms "${log_file}")"
    remote_exit_code="$(rch_extract_remote_exit_code "${log_file}")"
    failure_reason_code="$(rch_extract_failure_reason_code "${log_file}")"
    failure_reason_detail="$(rch_extract_failure_reason_detail "${log_file}")"

    jq -cn \
        --arg log_file "${log_file}" \
        --arg selected_worker "${selected_worker}" \
        --arg probe_worker_count "${probe_worker_count}" \
        --arg sync_duration_ms "${sync_duration_ms}" \
        --arg remote_duration_ms "${remote_duration_ms}" \
        --arg remote_exit_code "${remote_exit_code}" \
        --arg wrapper_exit_code "${wrapper_exit_code}" \
        --arg failure_reason_code "${failure_reason_code}" \
        --arg failure_reason_detail "${failure_reason_detail}" \
        --arg repo_root "${_RCH_REPO_ROOT}" \
        --arg local_tmpdir "${RCH_LOCAL_TMPDIR}" \
        --arg smoke_target_dir "${_RCH_SMOKE_TARGET_DIR}" \
        --arg timeout_bin "${TIMEOUT_BIN}" \
        --arg step_timeout_secs "${RCH_STEP_TIMEOUT_SECS}" \
        --arg smoke_timeout_secs "${RCH_SMOKE_TIMEOUT_SECS}" \
        --arg probe_timeout_secs "${RCH_PROBE_TIMEOUT_SECS}" \
        --arg rch_skip_smoke_preflight_requested "${RCH_SKIP_SMOKE_PREFLIGHT}" \
        --argjson probe_worker_ids "${probe_worker_ids_json}" \
        --argjson skipped_smoke_preflight "${skipped_smoke_preflight}" \
        --argjson reachable_workers_detected "${reachable_workers_detected}" \
        --argjson fail_open_detected "${fail_open_detected}" \
        --argjson timed_out "${timed_out}" \
        '{
          log_file: $log_file,
          selected_worker: (if $selected_worker == "" then null else $selected_worker end),
          probe_worker_count: (if $probe_worker_count == "" then null else ($probe_worker_count | tonumber) end),
          probe_worker_ids: $probe_worker_ids,
          sync_duration_ms: (if $sync_duration_ms == "" then null else ($sync_duration_ms | tonumber) end),
          remote_duration_ms: (if $remote_duration_ms == "" then null else ($remote_duration_ms | tonumber) end),
          remote_exit_code: (if $remote_exit_code == "" then null else ($remote_exit_code | tonumber) end),
          wrapper_exit_code: (if $wrapper_exit_code == "" then null else ($wrapper_exit_code | tonumber) end),
          failure_reason_code: (if $failure_reason_code == "" then null else $failure_reason_code end),
          failure_reason_detail: (if $failure_reason_detail == "" then null else $failure_reason_detail end),
          repo_root: (if $repo_root == "" then null else $repo_root end),
          local_tmpdir: (if $local_tmpdir == "" then null else $local_tmpdir end),
          smoke_target_dir: (if $smoke_target_dir == "" then null else $smoke_target_dir end),
          timeout_bin: (if $timeout_bin == "" then null else $timeout_bin end),
          step_timeout_secs: (if $step_timeout_secs == "" then null else ($step_timeout_secs | tonumber) end),
          smoke_timeout_secs: (if $smoke_timeout_secs == "" then null else ($smoke_timeout_secs | tonumber) end),
          probe_timeout_secs: (if $probe_timeout_secs == "" then null else ($probe_timeout_secs | tonumber) end),
          rch_skip_smoke_preflight_requested: (if $rch_skip_smoke_preflight_requested == "" then null else ($rch_skip_smoke_preflight_requested == "1") end),
          skipped_smoke_preflight: $skipped_smoke_preflight,
          reachable_workers_detected: $reachable_workers_detected,
          fail_open_detected: $fail_open_detected,
          timed_out: $timed_out
        }' > "${meta_file}"
}

rch_extract_selected_worker() {
    local output_file="$1"
    {
        sed -nE 's/.*Selected worker: ([^ ]+) at .*/\1/p' "${output_file}" 2>/dev/null
        sed -nE 's/.*\[RCH\][[:space:]]+remote[[:space:]]+(vmi[[:alnum:]_.-]+)([[:space:]:].*)?$/\1/p' "${output_file}" 2>/dev/null
    } | tail -n 1
}

rch_extract_probe_worker_ids() {
    local output_file="$1"
    grep -Eo '"id"[[:space:]]*:[[:space:]]*"[^"]+"' "${output_file}" 2>/dev/null \
        | sed -E 's/.*"id"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/' \
        | sort -u || true
}

rch_extract_probe_worker_count() {
    local output_file="$1"
    local count
    count="$(rch_extract_probe_worker_ids "${output_file}" | sed '/^$/d' | wc -l | tr -d ' ')"
    if [[ -n "${count}" ]]; then
        printf '%s\n' "${count}"
    fi
}

rch_extract_sync_duration_ms() {
    local output_file="$1"
    sed -nE 's/.*Sync complete: .* in ([0-9]+)ms.*/\1/p' "${output_file}" 2>/dev/null | tail -n 1 | tr -cd '0-9'
}

rch_extract_remote_duration_ms() {
    local output_file="$1"
    sed -nE 's/.*Remote command finished: exit=[-0-9]+ in ([0-9]+)ms.*/\1/p' "${output_file}" 2>/dev/null | tail -n 1 | tr -cd '0-9'
}

rch_extract_remote_exit_code() {
    local output_file="$1"
    sed -nE 's/.*Remote command finished: exit=([-0-9]+) in [0-9]+ms.*/\1/p' "${output_file}" 2>/dev/null | tail -n 1 | tr -cd '0-9-'
}

rch_log_has_remote_execution_marker() {
    local output_file="$1"
    grep -Eq "Selected worker:|Sync complete:|Remote command finished:|\[RCH\][[:space:]]+remote[[:space:]]+vmi[[:alnum:]_.-]+([[:space:]:]|$)" "${output_file}" 2>/dev/null
}

rch_log_has_remote_mirror_missing_file() {
    local output_file="$1"
    rch_log_has_remote_execution_marker "${output_file}" || return 1
    grep -Eq \
        "error: couldn't read .+: No such file or directory \\(os error 2\\)|error: can't find lib .+ at path .+" \
        "${output_file}" 2>/dev/null
}

rch_log_has_cargo_dep_info_missing() {
    local output_file="$1"
    rch_log_has_remote_execution_marker "${output_file}" || return 1
    sed -n 's/^error: could not parse\/generate dep info at: //p' "${output_file}" 2>/dev/null | grep -q . \
        && grep -Fq 'No such file or directory (os error 2)' "${output_file}" 2>/dev/null
}

rch_log_has_worker_selection_all_busy() {
    local output_file="$1"
    jq -e '
        (.data.worker_selection.worker == null)
        and ((.data.worker_selection.reason // "") == "all_workers_busy")
    ' "${output_file}" >/dev/null 2>&1
}

rch_diagnose_selected_worker() {
    local output_file="$1"
    jq -r '
        (.data.worker_selection.worker // empty) as $worker
        | if ($worker | type) == "object" then ($worker.id // "")
          elif ($worker | type) == "string" then $worker
          else "" end
    ' "${output_file}" 2>/dev/null || true
}

rch_extract_failure_reason_code() {
    local output_file="$1"

    if rch_log_has_worker_selection_all_busy "${output_file}"; then
        printf '%s\n' "RCH-WORKER-SELECTION-ALL-BUSY"
    elif rch_log_has_cargo_dep_info_missing "${output_file}"; then
        printf '%s\n' "RCH-CARGO-DEP-INFO-MISSING"
    elif grep -Fq "can't find crate for \`core\`" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-CROSS-RUST-TARGET-MISSING"
    elif grep -Fq "x86_64-w64-mingw32-gcc: not found" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-CROSS-CC-MISSING-WINDOWS"
    elif grep -Eq "cc: error: unrecognized command-line option '-arch'|cc: error: unrecognized command-line option '-mmacosx-version-min=" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-CROSS-CC-MISSING-DARWIN"
    elif grep -Eq "No package 'wayland-client' found|Package wayland-client was not found in the pkg-config search path" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-PKG-CONFIG-MISSING-WAYLAND"
    elif grep -Eq "was not found in the pkg-config search path|No package '.*' found" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-PKG-CONFIG-DEPENDENCY-MISSING"
    elif grep -Fq "Error building OpenSSL:" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-VENDORED-OPENSSL-BUILD-FAILED"
    elif grep -Eq "signal: 9, SIGKILL: kill|SIGKILL" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-REMOTE-PROCESS-SIGKILL"
    elif grep -Eq "RCH-E104|SSH command timed out" "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-SSH-COMMAND-TIMEOUT"
    elif rch_log_has_remote_mirror_missing_file "${output_file}"; then
        printf '%s\n' "RCH-REMOTE-MIRROR-MISSING-FILE"
    fi
}

rch_extract_failure_reason_detail() {
    local output_file="$1"

    if rch_log_has_worker_selection_all_busy "${output_file}"; then
        jq -r '"worker_selection.reason=" + (.data.worker_selection.reason // "unknown")' "${output_file}" 2>/dev/null
    elif rch_log_has_cargo_dep_info_missing "${output_file}"; then
        sed -n '/^error: could not parse\/generate dep info at: /p' "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Fq "can't find crate for \`core\`" "${output_file}" 2>/dev/null; then
        grep -F "can't find crate for \`core\`" "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Fq "x86_64-w64-mingw32-gcc: not found" "${output_file}" 2>/dev/null; then
        grep -F "x86_64-w64-mingw32-gcc: not found" "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Eq "cc: error: unrecognized command-line option '-arch'|cc: error: unrecognized command-line option '-mmacosx-version-min=" "${output_file}" 2>/dev/null; then
        grep -E "cc: error: unrecognized command-line option '-arch'|cc: error: unrecognized command-line option '-mmacosx-version-min=" "${output_file}" 2>/dev/null | head -n 1
    elif grep -Eq "No package 'wayland-client' found|Package wayland-client was not found in the pkg-config search path" "${output_file}" 2>/dev/null; then
        grep -E "No package 'wayland-client' found|Package wayland-client was not found in the pkg-config search path" "${output_file}" 2>/dev/null | head -n 1
    elif grep -Eq "was not found in the pkg-config search path|No package '.*' found" "${output_file}" 2>/dev/null; then
        grep -E "was not found in the pkg-config search path|No package '.*' found" "${output_file}" 2>/dev/null | head -n 1
    elif grep -Fq "Error building OpenSSL:" "${output_file}" 2>/dev/null; then
        grep -F "Error building OpenSSL:" "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Eq "signal: 9, SIGKILL: kill|SIGKILL" "${output_file}" 2>/dev/null; then
        grep -E "signal: 9, SIGKILL: kill|SIGKILL" "${output_file}" 2>/dev/null | tail -n 1
    elif grep -Eq "RCH-E104|SSH command timed out" "${output_file}" 2>/dev/null; then
        grep -E "RCH-E104|SSH command timed out" "${output_file}" 2>/dev/null | tail -n 1
    elif rch_log_has_remote_mirror_missing_file "${output_file}"; then
        grep -E \
            "error: couldn't read .+: No such file or directory \\(os error 2\\)|error: can't find lib .+ at path .+" \
            "${output_file}" 2>/dev/null | head -n 1
    fi
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

rch_write_remote_preflight_json() {
    local probe_log="$1"
    local queue_log="$2"
    local queue_rc="${3:-0}"
    local output_file="$4"
    local generated_at probe_worker_ids_raw probe_worker_ids_json probe_worker_count
    local reachable_workers_detected remote_only_required queue_valid queue_success queue_has_scheduler_fields
    local slots_available slots_total workers_available workers_healthy workers_offline workers_total
    local queue_depth active_build_count queued_build_count status reason_code worker_queue_state detail

    command -v jq >/dev/null 2>&1 || rch_fatal "jq is required to write rch remote preflight artifacts."

    generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    probe_worker_ids_raw="$(rch_extract_probe_worker_ids "${probe_log}")"
    probe_worker_count="$(printf '%s\n' "${probe_worker_ids_raw}" | sed '/^$/d' | wc -l | tr -d ' ')"
    if [[ -n "${probe_worker_ids_raw}" ]]; then
        probe_worker_ids_json="$(printf '%s\n' "${probe_worker_ids_raw}" | sed '/^$/d' | jq -R . | jq -s .)"
    else
        probe_worker_ids_json="[]"
    fi

    reachable_workers_detected="false"
    if probe_has_reachable_workers "${probe_log}"; then
        reachable_workers_detected="true"
    fi

    remote_only_required="false"
    if rch_remote_only_required; then
        remote_only_required="true"
    fi

    queue_valid="false"
    queue_success=""
    queue_has_scheduler_fields="false"
    slots_available=""
    slots_total=""
    workers_available=""
    workers_healthy=""
    workers_offline=""
    workers_total=""
    queue_depth=""
    active_build_count=""
    queued_build_count=""
    if [[ -f "${queue_log}" ]] && jq -e . "${queue_log}" >/dev/null 2>&1; then
        queue_valid="true"
        queue_success="$(jq -r '(.success // .ok // true) | tostring' "${queue_log}" 2>/dev/null || true)"
        queue_has_scheduler_fields="$(jq -r '
            (.data? // .) as $d
            | (($d | has("workers_healthy"))
               or ($d | has("workers_available"))
               or ($d | has("slots_available"))
               or ($d | has("queue_depth")))
        ' "${queue_log}" 2>/dev/null || printf 'false')"
        slots_available="$(jq -r '(.data? // .) as $d | $d.slots_available // empty' "${queue_log}" 2>/dev/null || true)"
        slots_total="$(jq -r '(.data? // .) as $d | $d.slots_total // empty' "${queue_log}" 2>/dev/null || true)"
        workers_available="$(jq -r '(.data? // .) as $d | $d.workers_available // empty' "${queue_log}" 2>/dev/null || true)"
        workers_healthy="$(jq -r '(.data? // .) as $d | $d.workers_healthy // empty' "${queue_log}" 2>/dev/null || true)"
        workers_offline="$(jq -r '(.data? // .) as $d | $d.workers_offline // empty' "${queue_log}" 2>/dev/null || true)"
        workers_total="$(jq -r '(.data? // .) as $d | $d.workers_total // empty' "${queue_log}" 2>/dev/null || true)"
        queue_depth="$(jq -r '(.data? // .) as $d | $d.queue_depth // empty' "${queue_log}" 2>/dev/null || true)"
        active_build_count="$(jq -r '(.data? // .) as $d | ($d.active_builds // [] | length)' "${queue_log}" 2>/dev/null || true)"
        queued_build_count="$(jq -r '(.data? // .) as $d | ($d.queued_builds // [] | length)' "${queue_log}" 2>/dev/null || true)"
    fi

    status="passed"
    reason_code="remote_ready"
    worker_queue_state="ready"
    detail="remote-only RCH preflight passed with worker capacity available"
    if [[ "${remote_only_required}" != "true" ]]; then
        status="blocked"
        reason_code="local_fallback_forbidden"
        worker_queue_state="unsupported_worker_selection"
        detail="RCH_REQUIRE_REMOTE must be enabled for remote-only proof lanes"
    elif [[ "${reachable_workers_detected}" != "true" ]]; then
        status="blocked"
        reason_code="no_healthy_workers"
        worker_queue_state="unhealthy"
        detail="worker probe did not report any reachable workers"
    elif [[ "${queue_rc}" != "0" || "${queue_valid}" != "true" || "${queue_success}" == "false" || "${queue_has_scheduler_fields}" != "true" ]]; then
        status="warning"
        reason_code="unsupported_worker_selection"
        worker_queue_state="unsupported_worker_selection"
        detail="RCH queue output did not expose enough scheduler state; heavy command remains fail-closed"
    elif rch_is_unsigned_int "${workers_healthy}" && [[ "${workers_healthy}" -eq 0 ]]; then
        status="blocked"
        reason_code="no_healthy_workers"
        worker_queue_state="unhealthy"
        detail="RCH queue reported zero healthy workers"
    elif rch_is_unsigned_int "${workers_available}" && [[ "${workers_available}" -eq 0 ]]; then
        status="blocked"
        reason_code="remote_busy_wait"
        worker_queue_state="busy_wait"
        detail="RCH queue reported zero available workers"
    elif rch_is_unsigned_int "${slots_available}" && [[ "${slots_available}" -eq 0 ]]; then
        status="blocked"
        reason_code="remote_busy_wait"
        worker_queue_state="busy_wait"
        detail="RCH queue reported zero available slots"
    elif rch_is_unsigned_int "${queue_depth}" && [[ "${queue_depth}" -gt 0 ]]; then
        status="warning"
        reason_code="remote_queue_nonempty"
        worker_queue_state="queue_nonempty"
        detail="RCH queue has waiting builds; worker-selection preflight remains authoritative"
    fi

    mkdir -p "$(dirname "${output_file}")"
    jq -cn \
        --argjson schema_version 1 \
        --arg kind "rch_remote_only_preflight" \
        --arg generated_at "${generated_at}" \
        --arg status "${status}" \
        --arg reason_code "${reason_code}" \
        --arg worker_queue_state "${worker_queue_state}" \
        --arg detail "${detail}" \
        --arg probe_log "$(rch_repo_relative_path "${probe_log}")" \
        --arg queue_log "$(rch_repo_relative_path "${queue_log}")" \
        --argjson remote_only_required "$(rch_json_bool "${remote_only_required}")" \
        --argjson reachable_workers_detected "$(rch_json_bool "${reachable_workers_detected}")" \
        --argjson queue_checked "$(rch_json_bool "${queue_valid}")" \
        --argjson queue_success "$(rch_json_bool "${queue_success}")" \
        --argjson queue_has_scheduler_fields "$(rch_json_bool "${queue_has_scheduler_fields}")" \
        --arg queue_rc "${queue_rc}" \
        --arg probe_worker_count "${probe_worker_count}" \
        --argjson probe_worker_ids "${probe_worker_ids_json}" \
        --arg slots_available "${slots_available}" \
        --arg slots_total "${slots_total}" \
        --arg workers_available "${workers_available}" \
        --arg workers_healthy "${workers_healthy}" \
        --arg workers_offline "${workers_offline}" \
        --arg workers_total "${workers_total}" \
        --arg queue_depth "${queue_depth}" \
        --arg active_build_count "${active_build_count}" \
        --arg queued_build_count "${queued_build_count}" \
        'def num_or_null($v): if $v == "" then null else ($v | tonumber) end;
        {
          schema_version: $schema_version,
          kind: $kind,
          generated_at: $generated_at,
          status: $status,
          reason_code: $reason_code,
          worker_queue_state: $worker_queue_state,
          detail: $detail,
          remote_only_required: $remote_only_required,
          checks: {
            local_fallback_allowed: false,
            reachable_workers_detected: $reachable_workers_detected,
            scheduler_queue_checked: $queue_checked,
            queue_success: $queue_success,
            queue_has_scheduler_fields: $queue_has_scheduler_fields,
            heavy_cargo_started: false
          },
          workers: {
            probe_worker_count: num_or_null($probe_worker_count),
            probe_worker_ids: $probe_worker_ids
          },
          queue: {
            exit_status: num_or_null($queue_rc),
            slots_available: num_or_null($slots_available),
            slots_total: num_or_null($slots_total),
            workers_available: num_or_null($workers_available),
            workers_healthy: num_or_null($workers_healthy),
            workers_offline: num_or_null($workers_offline),
            workers_total: num_or_null($workers_total),
            queue_depth: num_or_null($queue_depth),
            active_build_count: num_or_null($active_build_count),
            queued_build_count: num_or_null($queued_build_count)
          },
          artifacts: {
            probe_log: $probe_log,
            queue_log: $queue_log
          }
        }' >"${output_file}"
}

run_rch_logged_with_timeout() {
    local timeout_secs="$1"
    local output_file="$2"
    shift 2

    if [[ -z "${TIMEOUT_BIN}" ]]; then
        resolve_timeout_bin
    fi
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        rch_fatal "timeout or gtimeout is required to fail closed on stalled rch preflight commands."
    fi

    local rch_env=(
        "TMPDIR=${RCH_LOCAL_TMPDIR}"
        "RCH_NO_SELF_HEALING=1"
    )
    local passthrough_key
    for passthrough_key in \
        RCH_WORKER \
        RCH_CANONICAL_PROJECT_ROOT \
        RCH_ALIAS_PROJECT_ROOT \
        RCH_DAEMON_RESPONSE_TIMEOUT_SECS \
        RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS \
        RCH_VISIBILITY \
        RCH_LOG_LEVEL
    do
        if [[ -n "${!passthrough_key:-}" ]]; then
            rch_env+=("${passthrough_key}=${!passthrough_key}")
        fi
    done

    set +e
    (
        cd "${_RCH_REPO_ROOT:-.}"
        exec env "${rch_env[@]}" \
            "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" \
            rch "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e
    return "${rc}"
}

ensure_rch_remote_only_preflight() {
    if [[ "${RCH_SKIP_QUEUE_PREFLIGHT}" == "1" ]]; then
        printf '%s\n' "Queue preflight skipped because RCH_SKIP_QUEUE_PREFLIGHT=1" >"${_RCH_REMOTE_PREFLIGHT_LOG}"
        return 0
    fi

    local wait_secs poll_secs started_at now elapsed attempt queue_rc status reason_code
    wait_secs="${RCH_REMOTE_PREFLIGHT_WAIT_SECS}"
    poll_secs="${RCH_WORKER_SELECTION_POLL_SECS}"
    rch_is_unsigned_int "${wait_secs}" || wait_secs="0"
    rch_is_unsigned_int "${poll_secs}" || poll_secs="15"
    if [[ "${poll_secs}" -eq 0 ]]; then
        poll_secs="1"
    fi

    started_at="$(date +%s)"
    attempt=0
    while :; do
        attempt=$((attempt + 1))
        set +e
        run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${_RCH_QUEUE_LOG}" --json queue
        queue_rc=$?
        set -e
        rch_write_meta_json "${_RCH_QUEUE_LOG}" "${queue_rc}"
        rch_emit_proof_ledger_entry \
            "rch --json queue" \
            "${_RCH_QUEUE_LOG}" \
            "${queue_rc}" \
            "not_applicable" \
            "not_applicable" \
            ""

        rch_write_remote_preflight_json "${_RCH_PROBE_LOG}" "${_RCH_QUEUE_LOG}" "${queue_rc}" "${_RCH_REMOTE_PREFLIGHT_LOG}"
        status="$(jq -r '.status // ""' "${_RCH_REMOTE_PREFLIGHT_LOG}")"
        reason_code="$(jq -r '.reason_code // "unknown"' "${_RCH_REMOTE_PREFLIGHT_LOG}")"
        if [[ "${status}" != "blocked" ]]; then
            break
        fi

        now="$(date +%s)"
        elapsed=$((now - started_at))
        if [[ "${elapsed}" -ge "${wait_secs}" ]]; then
            rch_fatal "rch remote-only preflight blocked after ${elapsed}s and ${attempt} attempt(s): ${reason_code}. See ${_RCH_REMOTE_PREFLIGHT_LOG}"
        fi
        sleep "${poll_secs}"
    done

    ensure_rch_worker_selection_preflight
}

ensure_rch_worker_selection_preflight() {
    [[ -n "${_RCH_WORKER_SELECTION_LOG}" ]] || rch_fatal "rch_init must be called before ensure_rch_worker_selection_preflight."

    if [[ "${RCH_SKIP_WORKER_SELECTION_PREFLIGHT}" == "1" ]]; then
        printf '%s\n' "Worker-selection preflight skipped because RCH_SKIP_WORKER_SELECTION_PREFLIGHT=1" >"${_RCH_WORKER_SELECTION_LOG}"
        rch_write_meta_json "${_RCH_WORKER_SELECTION_LOG}" "0"
        rch_emit_proof_ledger_entry \
            "RCH_SKIP_WORKER_SELECTION_PREFLIGHT=1 ensure_rch_ready" \
            "${_RCH_WORKER_SELECTION_LOG}" \
            "0" \
            "not_applicable" \
            "not_applicable" \
            "worker-selection dry-run skipped because first material verifier uses run_rch_cargo_logged"
        return 0
    fi

    local wait_secs poll_secs started_at now elapsed attempt selected_worker selection_rc would_intercept selection_reason
    wait_secs="${RCH_WORKER_SELECTION_WAIT_SECS}"
    poll_secs="${RCH_WORKER_SELECTION_POLL_SECS}"
    rch_is_unsigned_int "${wait_secs}" || wait_secs="0"
    rch_is_unsigned_int "${poll_secs}" || poll_secs="15"
    if [[ "${poll_secs}" -eq 0 ]]; then
        poll_secs="1"
    fi

    started_at="$(date +%s)"
    attempt=0
    while :; do
        attempt=$((attempt + 1))
        set +e
        run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${_RCH_WORKER_SELECTION_LOG}" --json diagnose --dry-run cargo check --help
        selection_rc=$?
        set -e
        rch_write_meta_json "${_RCH_WORKER_SELECTION_LOG}" "${selection_rc}"
        rch_emit_proof_ledger_entry \
            "rch --json diagnose --dry-run cargo check --help" \
            "${_RCH_WORKER_SELECTION_LOG}" \
            "${selection_rc}" \
            "not_applicable" \
            "not_applicable" \
            ""

        if [[ ${selection_rc} -ne 0 ]] || ! jq -e . "${_RCH_WORKER_SELECTION_LOG}" >/dev/null 2>&1; then
            rch_fatal "rch worker-selection preflight failed. See ${_RCH_WORKER_SELECTION_LOG}"
        fi

        would_intercept="$(jq -r '.data.decision.would_intercept // false' "${_RCH_WORKER_SELECTION_LOG}" 2>/dev/null || true)"
        selected_worker="$(jq -r '.data.worker_selection.worker // ""' "${_RCH_WORKER_SELECTION_LOG}" 2>/dev/null || true)"
        selection_reason="$(jq -c '.data.worker_selection.reason // {}' "${_RCH_WORKER_SELECTION_LOG}" 2>/dev/null || printf '{}')"

        if [[ "${would_intercept}" != "true" ]]; then
            rch_fatal "rch worker-selection preflight could not prove cargo offload eligibility. See ${_RCH_WORKER_SELECTION_LOG}"
        fi
        if [[ -n "${selected_worker}" ]]; then
            return 0
        fi

        now="$(date +%s)"
        elapsed=$((now - started_at))
        if [[ "${elapsed}" -ge "${wait_secs}" ]]; then
            rch_fatal "rch worker-selection preflight blocked after ${elapsed}s and ${attempt} attempt(s): ${selection_reason}. See ${_RCH_WORKER_SELECTION_LOG}"
        fi
        sleep "${poll_secs}"
    done
}

ensure_rch_runtime_capabilities() {
    set +e
    run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${_RCH_CAPABILITIES_LOG}" --json workers capabilities
    local capabilities_rc=$?
    set -e
    rch_write_meta_json "${_RCH_CAPABILITIES_LOG}" "${capabilities_rc}"
    rch_emit_proof_ledger_entry \
        "rch --json workers capabilities" \
        "${_RCH_CAPABILITIES_LOG}" \
        "${capabilities_rc}" \
        "not_applicable" \
        "not_applicable" \
        "read cached daemon-side runtime capabilities before remote-only cargo proof"

    local rust_worker_count=0
    if [[ "${capabilities_rc}" -eq 0 ]] && jq -e . "${_RCH_CAPABILITIES_LOG}" >/dev/null 2>&1; then
        rust_worker_count="$(jq -r '
            (.data.workers // .workers // [])
            | map(select(.capabilities.rustc_version? != null))
            | length
        ' "${_RCH_CAPABILITIES_LOG}" 2>/dev/null || printf '0')"
        if rch_is_unsigned_int "${rust_worker_count}" && [[ "${rust_worker_count}" -gt 0 ]]; then
            return 0
        fi
    fi

    set +e
    run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${_RCH_CAPABILITIES_REFRESH_LOG}" --json workers capabilities --refresh
    local refresh_rc=$?
    set -e
    rch_write_meta_json "${_RCH_CAPABILITIES_REFRESH_LOG}" "${refresh_rc}"
    rch_emit_proof_ledger_entry \
        "rch --json workers capabilities --refresh" \
        "${_RCH_CAPABILITIES_REFRESH_LOG}" \
        "${refresh_rc}" \
        "not_applicable" \
        "not_applicable" \
        "refresh daemon-side runtime capabilities after cached capabilities had no Rust workers"

    if [[ "${refresh_rc}" -ne 0 ]] || ! jq -e . "${_RCH_CAPABILITIES_REFRESH_LOG}" >/dev/null 2>&1; then
        rch_fatal "rch worker capability refresh failed after cached capabilities had no Rust-capable workers. See ${_RCH_CAPABILITIES_REFRESH_LOG}; cached capabilities: ${_RCH_CAPABILITIES_LOG}"
    fi

    rust_worker_count="$(jq -r '
        (.data.workers // .workers // [])
        | map(select(.capabilities.rustc_version? != null))
        | length
    ' "${_RCH_CAPABILITIES_REFRESH_LOG}" 2>/dev/null || printf '0')"
    if ! rch_is_unsigned_int "${rust_worker_count}" || [[ "${rust_worker_count}" -eq 0 ]]; then
        rch_fatal "rch worker capability refresh found no Rust-capable workers. See ${_RCH_CAPABILITIES_REFRESH_LOG}; cached capabilities: ${_RCH_CAPABILITIES_LOG}"
    fi
}

rch_mirror_required_paths() {
    [[ -n "${RCH_MIRROR_REQUIRED_PATHS}" ]] || return 0
    printf '%s\n' "${RCH_MIRROR_REQUIRED_PATHS}" \
        | tr ',:' '\n' \
        | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' \
        | sed '/^$/d'
}

ensure_rch_mirror_preflight() {
    local required_paths=()
    local workspace_member_arg=()
    local require_workspace_member_roots="false"
    local path
    while IFS= read -r path; do
        required_paths+=("--path" "${path}")
    done < <(rch_mirror_required_paths)

    case "${RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS}" in
        auto)
            [[ "${#required_paths[@]}" -gt 0 ]] && require_workspace_member_roots="true"
            ;;
        1|true|TRUE|yes|YES)
            require_workspace_member_roots="true"
            ;;
        0|false|FALSE|no|NO|"")
            require_workspace_member_roots="false"
            ;;
        *)
            rch_fatal "RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS must be auto, true, or false; got '${RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS}'."
            ;;
    esac
    if [[ "${require_workspace_member_roots}" == "true" ]]; then
        workspace_member_arg=("--workspace-member-roots")
    fi
    [[ "${#required_paths[@]}" -gt 0 || "${#workspace_member_arg[@]}" -gt 0 ]] || return 0

    local attest_script="${_RCH_REPO_ROOT}/scripts/attest_rch_worker_mirror.sh"
    [[ -x "${attest_script}" ]] || rch_fatal "mirror attestation script is not executable: ${attest_script}"

    local worker_ids worker_id worker_dir worker_json worker_rc failures total bead_arg=()
    local block_on_stale_head="false"
    local require_all_checked_workers="false"
    local min_passing_workers
    local scheduler_worker_ids scheduler_status_rc scheduler_filter_active="false"
    local pinned_worker
    worker_ids="$(rch_extract_probe_worker_ids "${_RCH_PROBE_LOG}")"
    [[ -n "${worker_ids}" ]] || rch_fatal "mirror preflight requested but worker probe did not expose worker ids. See ${_RCH_PROBE_LOG}"
    pinned_worker="${RCH_WORKER:-}"
    min_passing_workers="${RCH_MIRROR_MIN_PASSING_WORKERS}"
    if ! rch_is_unsigned_int "${min_passing_workers}" || [[ "${min_passing_workers}" -eq 0 ]]; then
        rch_fatal "RCH_MIRROR_MIN_PASSING_WORKERS must be a positive integer; got '${RCH_MIRROR_MIN_PASSING_WORKERS}'."
    fi

    set +e
    run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${_RCH_SCHEDULER_WORKERS_LOG}" --json status --workers
    scheduler_status_rc=$?
    set -e
    scheduler_worker_ids=""
    if [[ "${scheduler_status_rc}" -eq 0 ]] && jq -e . "${_RCH_SCHEDULER_WORKERS_LOG}" >/dev/null 2>&1; then
        scheduler_worker_ids="$(jq -r '
            [.data.daemon.workers[]? | select((.status // "") == "healthy") | .id]
            | .[]
        ' "${_RCH_SCHEDULER_WORKERS_LOG}" 2>/dev/null || true)"
        if [[ -n "${scheduler_worker_ids}" ]]; then
            scheduler_filter_active="true"
        fi
    fi
    if [[ -n "${pinned_worker}" ]]; then
        if ! grep -Fxq "${pinned_worker}" <<<"${worker_ids}"; then
            rch_fatal "mirror preflight requested for pinned RCH_WORKER=${pinned_worker}, but probe did not expose that worker. See ${_RCH_PROBE_LOG}"
        fi
        if [[ "${scheduler_filter_active}" == "true" ]] \
            && ! grep -Fxq "${pinned_worker}" <<<"${scheduler_worker_ids}"; then
            rch_fatal "mirror preflight requested for pinned RCH_WORKER=${pinned_worker}, but scheduler does not mark that worker healthy. See ${_RCH_SCHEDULER_WORKERS_LOG}"
        fi
        worker_ids="${pinned_worker}"
    fi

    worker_dir="${_RCH_MIRROR_PREFLIGHT_LOG%.json}.workers"
    mkdir -p "${worker_dir}"
    failures=0
    total=0
    if [[ -n "${RCH_PROOF_LEDGER_BEAD_ID:-}" ]]; then
        bead_arg=("--bead" "${RCH_PROOF_LEDGER_BEAD_ID}")
    fi
    if rch_truthy "${RCH_MIRROR_BLOCK_ON_STALE_HEAD}"; then
        block_on_stale_head="true"
    fi
    if rch_truthy "${RCH_MIRROR_REQUIRE_ALL_CHECKED_WORKERS}"; then
        require_all_checked_workers="true"
    fi

    while IFS= read -r worker_id; do
        [[ -n "${worker_id}" ]] || continue
        if [[ "${scheduler_filter_active}" == "true" ]] \
            && ! grep -Fxq "${worker_id}" <<<"${scheduler_worker_ids}"; then
            continue
        fi
        worker_json="${worker_dir}/${worker_id}.json"
        set +e
        "${attest_script}" \
            --worker "${worker_id}" \
            "${bead_arg[@]}" \
            "${required_paths[@]}" \
            "${workspace_member_arg[@]}" \
            --command "remote-only preflight mirror attestation" \
            --json >"${worker_json}"
        worker_rc=$?
        set -e
        total=$((total + 1))
        if [[ "${worker_rc}" -ne 0 ]]; then
            failures=$((failures + 1))
        fi
    done <<<"${worker_ids}"

    jq -s \
        --argjson schema_version 1 \
        --arg kind "rch_worker_pool_mirror_preflight" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --argjson total "${total}" \
        --argjson failures "${failures}" \
        --argjson min_passing_workers "${min_passing_workers}" \
        --argjson block_on_stale_head "$(rch_json_bool "${block_on_stale_head}")" \
        --argjson require_all_checked_workers "$(rch_json_bool "${require_all_checked_workers}")" \
        --argjson scheduler_filter_active "$(rch_json_bool "${scheduler_filter_active}")" \
        --arg probe_log "$(rch_repo_relative_path "${_RCH_PROBE_LOG}")" \
        --arg scheduler_workers_log "$(rch_repo_relative_path "${_RCH_SCHEDULER_WORKERS_LOG}")" \
        'def blocking_failure($block_on_stale_head):
          (.status != "passed")
          or (
            $block_on_stale_head
            and (
              .reason_code == "rch_mirror.required_files_ok_head_mismatch"
              or .reason_code == "rch_mirror.required_files_ok_head_unavailable"
            )
          );
        ([.[] | select(blocking_failure($block_on_stale_head))] | length) as $blocking_failures
        | ($total - $blocking_failures) as $passing_workers
        | (
            if $require_all_checked_workers then
              $total > 0 and $blocking_failures == 0
            else
              $passing_workers >= $min_passing_workers
            end
          ) as $pool_ready
        | {
          schema_version: $schema_version,
          kind: $kind,
          generated_at: $generated_at,
          status: (if $pool_ready then "passed" else "blocked" end),
          reason_code: (
            if $pool_ready and $blocking_failures == 0 then "source_mirror_ready"
            elif $pool_ready then "source_mirror_minimum_ready"
            elif $require_all_checked_workers and $blocking_failures > 0 then "source_mirror_checked_workers_blocked"
            else "source_mirror_blocked" end
          ),
          detail: (
            if $pool_ready and $blocking_failures == 0 then
              "all blocking source mirror checks passed; Git metadata drift is recorded separately when required file hashes match"
            elif $pool_ready then
              "the source mirror pool met the minimum ready-worker threshold; stale worker mirrors are retained as residual evidence"
            elif $require_all_checked_workers and $blocking_failures > 0 then
              "at least one checked scheduler-eligible source mirror failed blocking attestation; proof lane blocked before scheduler selection"
            else
              "too few probed workers passed blocking source mirror attestation"
            end
          ),
          total_workers_checked: $total,
          failed_workers: $failures,
          passing_workers: $passing_workers,
          required_passing_workers: $min_passing_workers,
          blocking_failed_workers: $blocking_failures,
          block_on_stale_head: $block_on_stale_head,
          require_all_checked_workers: $require_all_checked_workers,
          scheduler_filter_active: $scheduler_filter_active,
          artifacts: {
            probe_log: $probe_log,
            scheduler_workers_log: $scheduler_workers_log
          },
          worker_results: .
        }' "${worker_dir}"/*.json >"${_RCH_MIRROR_PREFLIGHT_LOG}"

    local preflight_status reason_code blocking_failures passing_workers required_passing_workers
    preflight_status="$(jq -r '.status // "blocked"' "${_RCH_MIRROR_PREFLIGHT_LOG}")"
    reason_code="$(jq -r '.reason_code // "unknown"' "${_RCH_MIRROR_PREFLIGHT_LOG}")"
    blocking_failures="$(jq -r '.blocking_failed_workers // 0' "${_RCH_MIRROR_PREFLIGHT_LOG}")"
    passing_workers="$(jq -r '.passing_workers // 0' "${_RCH_MIRROR_PREFLIGHT_LOG}")"
    required_passing_workers="$(jq -r '.required_passing_workers // 1' "${_RCH_MIRROR_PREFLIGHT_LOG}")"
    if [[ "${preflight_status}" != "passed" ]]; then
        rch_fatal "rch source mirror preflight blocked: ${reason_code}; ${passing_workers}/${total} workers passed blocking attestation; required ${required_passing_workers}; ${blocking_failures} workers failed. See ${_RCH_MIRROR_PREFLIGHT_LOG}"
    fi
}

rch_selected_worker_mirror_preflight_enabled() {
    case "${RCH_SELECTED_WORKER_MIRROR_PREFLIGHT}" in
        1|true|TRUE|yes|YES)
            return 0
            ;;
        0|false|FALSE|no|NO)
            return 1
            ;;
        auto|"")
            ;;
        *)
            rch_fatal "RCH_SELECTED_WORKER_MIRROR_PREFLIGHT must be auto, true, or false; got '${RCH_SELECTED_WORKER_MIRROR_PREFLIGHT}'."
            ;;
    esac

    case "${RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS}" in
        1|true|TRUE|yes|YES)
            return 0
            ;;
    esac
    [[ -n "${RCH_MIRROR_REQUIRED_PATHS}" ]]
}

rch_attest_selected_worker_before_cargo() {
    local output_file="$1"
    shift

    rch_selected_worker_mirror_preflight_enabled || return 0

    local required_paths=()
    local workspace_member_arg=()
    local require_workspace_member_roots="false"
    local path
    while IFS= read -r path; do
        required_paths+=("--path" "${path}")
    done < <(rch_mirror_required_paths)

    case "${RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS}" in
        auto)
            [[ "${#required_paths[@]}" -gt 0 ]] && require_workspace_member_roots="true"
            ;;
        1|true|TRUE|yes|YES)
            require_workspace_member_roots="true"
            ;;
        0|false|FALSE|no|NO|"")
            require_workspace_member_roots="false"
            ;;
        *)
            rch_fatal "RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS must be auto, true, or false; got '${RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS}'."
            ;;
    esac
    if [[ "${require_workspace_member_roots}" == "true" ]]; then
        workspace_member_arg=("--workspace-member-roots")
    fi
    [[ "${#required_paths[@]}" -gt 0 || "${#workspace_member_arg[@]}" -gt 0 ]] || return 0

    local attest_script="${_RCH_REPO_ROOT}/scripts/attest_rch_worker_mirror.sh"
    [[ -x "${attest_script}" ]] || rch_fatal "mirror attestation script is not executable: ${attest_script}"

    local diagnose_log mirror_log diagnose_rc selected_worker would_intercept selection_reason bead_arg=()
    diagnose_log="${output_file%.log}.rch_diagnose.json"
    mirror_log="${output_file%.log}.selected_worker_mirror.json"

    set +e
    run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${diagnose_log}" --json diagnose "$@"
    diagnose_rc=$?
    set -e
    rch_write_meta_json "${diagnose_log}" "${diagnose_rc}"
    rch_emit_proof_ledger_entry \
        "rch --json diagnose $*" \
        "${diagnose_log}" \
        "${diagnose_rc}" \
        "not_applicable" \
        "not_applicable" \
        "selected-worker mirror preflight before run_rch_cargo_logged material command"

    if [[ "${diagnose_rc}" -ne 0 ]] || ! jq -e . "${diagnose_log}" >/dev/null 2>&1; then
        rch_fatal "rch selected-worker diagnose failed before material cargo proof. See ${diagnose_log}"
    fi

    would_intercept="$(jq -r '.data.decision.would_intercept // false' "${diagnose_log}" 2>/dev/null || true)"
    selected_worker="$(rch_diagnose_selected_worker "${diagnose_log}")"
    selection_reason="$(jq -c '.data.worker_selection.reason // "unknown"' "${diagnose_log}" 2>/dev/null || printf '%s' '"unknown"')"
    if [[ "${would_intercept}" != "true" ]]; then
        rch_fatal "rch selected-worker diagnose could not prove cargo offload eligibility before material cargo proof. See ${diagnose_log}"
    fi
    if [[ -z "${selected_worker}" ]]; then
        rch_fatal "rch selected-worker diagnose did not return a worker before material cargo proof: ${selection_reason}. See ${diagnose_log}"
    fi
    if [[ -n "${RCH_WORKER:-}" && "${selected_worker}" != "${RCH_WORKER}" ]]; then
        rch_fatal "rch selected-worker diagnose chose ${selected_worker}, but RCH_WORKER=${RCH_WORKER}; refusing to prove on a different worker. See ${diagnose_log}"
    fi

    if [[ -n "${RCH_PROOF_LEDGER_BEAD_ID:-}" ]]; then
        bead_arg=("--bead" "${RCH_PROOF_LEDGER_BEAD_ID}")
    fi
    if ! "${attest_script}" \
        --worker "${selected_worker}" \
        "${bead_arg[@]}" \
        "${required_paths[@]}" \
        "${workspace_member_arg[@]}" \
        --command "selected-worker preflight for run_rch_cargo_logged" \
        --json >"${mirror_log}"; then
        rch_fatal "rch selected-worker source mirror preflight failed for ${selected_worker}. See ${mirror_log}"
    fi

    printf '%s\n' "${selected_worker}"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        rch_fatal "rch entered a fail-open or off-policy execution path; refusing offload policy violation. See ${output_file}"
    fi
    if rch_remote_only_required && ! rch_log_has_remote_execution_marker "${output_file}"; then
        rch_fatal "rch did not record remote execution; refusing local/non-compilation path. See ${output_file}"
    fi
}

child_pids() {
    local pid="$1"
    if command -v pgrep >/dev/null 2>&1; then
        pgrep -P "${pid}" 2>/dev/null || true
    fi
}

terminate_process_tree() {
    local pid="$1"
    local signal="${2:-TERM}"
    local child
    for child in $(child_pids "${pid}"); do
        terminate_process_tree "${child}" "${signal}"
    done
    kill -"${signal}" "${pid}" 2>/dev/null || true
}

start_rch_fallback_monitor() {
    local runner_pid="$1"
    local output_file="$2"

    (
        while kill -0 "${runner_pid}" 2>/dev/null; do
            if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
                terminate_process_tree "${runner_pid}" TERM
                sleep 2
                terminate_process_tree "${runner_pid}" KILL
                exit 0
            fi
            sleep 1
        done
    ) &
    printf '%s\n' "$!"
}

stop_rch_fallback_monitor() {
    local monitor_pid="$1"
    if [[ -n "${monitor_pid}" ]]; then
        kill "${monitor_pid}" 2>/dev/null || true
        wait "${monitor_pid}" 2>/dev/null || true
    fi
}

rch_timeout_queue_log() {
    local output_file="$1"
    local queue_log="${output_file%.log}.rch_queue_timeout.log"
    if ! run_rch queue >"${queue_log}" 2>&1; then
        queue_log="${output_file}"
    fi
    printf '%s\n' "${queue_log}"
}

rch_timeout_reason_code() {
    local output_file="$1"
    if grep -Eq 'Retrieving (build )?artifacts?( from)?' "${output_file}" 2>/dev/null; then
        printf '%s\n' "RCH-ARTIFACT-STALL"
    else
        printf '%s\n' "RCH-REMOTE-STALL"
    fi
}

rch_timeout_reason_message() {
    local reason_code="$1"
    local timeout_secs="$2"
    if [[ "${reason_code}" == "RCH-ARTIFACT-STALL" ]]; then
        printf '%s\n' "rch remote command timed out after ${timeout_secs}s while retrieving artifacts from the worker"
    else
        printf '%s\n' "rch remote command timed out after ${timeout_secs}s"
    fi
}

# Usage: run_rch_cargo_logged_with_timeout <timeout_secs> <output_file> <args passed to rch exec -- ...>
# The caller is responsible for including `env CARGO_TARGET_DIR=... cargo ...`
# in the args.
run_rch_cargo_logged_with_timeout() {
    local timeout_secs="$1"
    local output_file="$2"
    shift 2
    local caller_had_errexit="false"
    local runner_pid=""
    local monitor_pid=""

    if [[ $- == *e* ]]; then
        caller_had_errexit="true"
    fi

    if [[ -z "${TIMEOUT_BIN}" ]]; then
        resolve_timeout_bin
    fi
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        rch_fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi

    if rch_github_actions_local_cargo_enabled; then
        : >"${output_file}"

        set +e
        (
            cd "${_RCH_REPO_ROOT}"
            printf '%s\n' "[rch-guard] GitHub Actions local Cargo mode enabled; executing without rch on the hosted runner."
            exec "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" "$@"
        ) >"${output_file}" 2>&1
        local rc=$?
        set -e

        rch_write_meta_json "${output_file}" "${rc}"
        local target_dir target_dir_lifecycle command_text residual_risk_notes
        target_dir="$(rch_extract_cargo_target_dir_from_args "$@")"
        target_dir_lifecycle="retained"
        if [[ "${target_dir}" == "not_applicable" ]]; then
            target_dir_lifecycle="not_applicable"
        fi
        command_text="github_actions_local_cargo ${timeout_secs} ${output_file} $*"
        residual_risk_notes="$(rch_extract_failure_reason_detail "${output_file}")"
        rch_emit_proof_ledger_entry \
            "${command_text}" \
            "${output_file}" \
            "${rc}" \
            "${target_dir}" \
            "${target_dir_lifecycle}" \
            "${residual_risk_notes}"

        if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
            rch_fatal "RCH-GITHUB-ACTIONS-LOCAL-TIMEOUT: GitHub Actions local command timed out after ${timeout_secs}s. See ${output_file}"
        fi
        if [[ "${caller_had_errexit}" == "false" ]]; then
            set +e
        fi
        return "${rc}"
    fi

    : >"${output_file}"
    local selected_worker=""
    selected_worker="$(rch_attest_selected_worker_before_cargo "${output_file}" "$@")"

    local rch_env=(
        "TMPDIR=${RCH_LOCAL_TMPDIR}"
        "RCH_NO_SELF_HEALING=1"
        "RCH_REQUIRE_REMOTE=${RCH_REQUIRE_REMOTE}"
        "RCH_BUILD_TIMEOUT_SEC=${RCH_BUILD_TIMEOUT_SEC:-${timeout_secs}}"
        "RCH_TEST_TIMEOUT_SEC=${RCH_TEST_TIMEOUT_SEC:-${timeout_secs}}"
    )
    if [[ -n "${RCH_BUILD_SLOTS:-}" ]]; then
        rch_env+=("RCH_BUILD_SLOTS=${RCH_BUILD_SLOTS}")
    fi
    if [[ -n "${RCH_TEST_SLOTS:-}" ]]; then
        rch_env+=("RCH_TEST_SLOTS=${RCH_TEST_SLOTS}")
    fi
    if [[ -n "${RCH_CHECK_SLOTS:-}" ]]; then
        rch_env+=("RCH_CHECK_SLOTS=${RCH_CHECK_SLOTS}")
    fi
    local passthrough_key
    for passthrough_key in \
        RCH_WORKER \
        RCH_CANONICAL_PROJECT_ROOT \
        RCH_ALIAS_PROJECT_ROOT \
        RCH_DAEMON_RESPONSE_TIMEOUT_SECS \
        RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS \
        RCH_VISIBILITY \
        RCH_LOG_LEVEL
    do
        if [[ -n "${!passthrough_key:-}" ]]; then
            rch_env+=("${passthrough_key}=${!passthrough_key}")
        fi
    done
    if [[ -n "${selected_worker}" && -z "${RCH_WORKER:-}" ]]; then
        rch_env+=("RCH_WORKER=${selected_worker}")
    fi

    set +e
    (
        cd "${_RCH_REPO_ROOT}"
        exec env "${rch_env[@]}" \
            "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" \
            rch exec -- "$@"
    ) >"${output_file}" 2>&1 &
    runner_pid="$!"
    monitor_pid="$(start_rch_fallback_monitor "${runner_pid}" "${output_file}")"

    wait "${runner_pid}"
    local rc=$?
    set -e
    stop_rch_fallback_monitor "${monitor_pid}"
    rch_write_meta_json "${output_file}" "${rc}"
    local target_dir target_dir_lifecycle command_text residual_risk_notes
    target_dir="$(rch_extract_cargo_target_dir_from_args "$@")"
    target_dir_lifecycle="retained"
    if [[ "${target_dir}" == "not_applicable" ]]; then
        target_dir_lifecycle="not_applicable"
    fi
    command_text="run_rch_cargo_logged_with_timeout ${timeout_secs} ${output_file} $*"
    residual_risk_notes="$(rch_extract_failure_reason_detail "${output_file}")"
    rch_emit_proof_ledger_entry \
        "${command_text}" \
        "${output_file}" \
        "${rc}" \
        "${target_dir}" \
        "${target_dir_lifecycle}" \
        "${residual_risk_notes}"

    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        local queue_log
        queue_log="$(rch_timeout_queue_log "${output_file}")"
        local reason_code
        reason_code="$(rch_timeout_reason_code "${output_file}")"
        rch_fatal "${reason_code}: $(rch_timeout_reason_message "${reason_code}" "${timeout_secs}"). See ${queue_log}"
    fi
    if [[ "${caller_had_errexit}" == "false" ]]; then
        set +e
    fi
    return "${rc}"
}

# Usage: run_rch_cargo_logged <output_file> <args passed to rch exec -- ...>
# The caller is responsible for including `env CARGO_TARGET_DIR=... cargo ...`
# in the args.
run_rch_cargo_logged() {
    local output_file="$1"
    shift
    run_rch_cargo_logged_with_timeout "${RCH_STEP_TIMEOUT_SECS}" "${output_file}" "$@"
}

# Call once at harness start. Sets up internal variables.
# Usage: rch_init <log_dir> <run_id> <harness_name> [repo_root]
rch_init() {
    local log_dir="$1"
    local run_id="$2"
    local harness_name="$3"
    _RCH_REPO_ROOT="${4:-$(cd "$(dirname "${BASH_SOURCE[1]}")/../.." && pwd)}"

    _RCH_PROBE_LOG="${log_dir}/${harness_name}_${run_id}.rch_probe.log"
    _RCH_QUEUE_LOG="${log_dir}/${harness_name}_${run_id}.rch_queue.log"
    _RCH_CAPABILITIES_LOG="${log_dir}/${harness_name}_${run_id}.rch_capabilities.log"
    _RCH_CAPABILITIES_REFRESH_LOG="${log_dir}/${harness_name}_${run_id}.rch_capabilities_refresh.log"
    _RCH_SMOKE_LOG="${log_dir}/${harness_name}_${run_id}.rch_smoke.log"
    _RCH_REMOTE_PREFLIGHT_LOG="${log_dir}/${harness_name}_${run_id}.rch_preflight.json"
    _RCH_MIRROR_PREFLIGHT_LOG="${log_dir}/${harness_name}_${run_id}.rch_mirror_preflight.json"
    _RCH_SCHEDULER_WORKERS_LOG="${log_dir}/${harness_name}_${run_id}.rch_scheduler_workers.json"
    _RCH_WORKER_SELECTION_LOG="${log_dir}/${harness_name}_${run_id}.rch_worker_selection.json"
    local smoke_target_root="${RCH_SMOKE_TARGET_ROOT:-target/rch-smoke}"
    _RCH_SMOKE_TARGET_DIR="${RCH_SMOKE_TARGET_DIR:-${smoke_target_root}/${harness_name}/${run_id}}"
    if [[ "${RCH_SKIP_SMOKE_PREFLIGHT}" != "1" ]]; then
        mkdir -p "${_RCH_SMOKE_TARGET_DIR}"
    fi
}

# Preflight check: ensure rch is available, workers reachable, and remote
# cargo execution works. Calls rch_fatal on any failure.
ensure_rch_ready() {
    if rch_github_actions_local_cargo_enabled; then
        resolve_timeout_bin
        if [[ -z "${TIMEOUT_BIN}" ]]; then
            rch_fatal "timeout or gtimeout is required to fail closed on stalled GitHub Actions local execution."
        fi
        [[ -n "${_RCH_PROBE_LOG}" ]] || rch_fatal "rch_init must be called before ensure_rch_ready."
        printf '%s\n' \
            "GitHub Actions local Cargo mode enabled; rch preflight skipped because the hosted runner is the CI execution target." \
            >"${_RCH_PROBE_LOG}"
        rch_write_meta_json "${_RCH_PROBE_LOG}" "0"
        rch_emit_proof_ledger_entry \
            "RCH_GITHUB_ACTIONS_LOCAL_CARGO=1 ensure_rch_ready" \
            "${_RCH_PROBE_LOG}" \
            "0" \
            "not_applicable" \
            "not_applicable" \
            "hosted GitHub Actions local execution mode"
        return 0
    fi

    if ! command -v rch >/dev/null 2>&1; then
        rch_fatal "rch is required for this E2E harness; refusing local cargo execution."
    fi
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        rch_fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi

    set +e
    run_rch_logged_with_timeout "${RCH_PROBE_TIMEOUT_SECS}" "${_RCH_PROBE_LOG}" --json workers probe --all
    local probe_rc=$?
    set -e
    rch_write_meta_json "${_RCH_PROBE_LOG}" "${probe_rc}"
    rch_emit_proof_ledger_entry \
        "rch --json workers probe --all" \
        "${_RCH_PROBE_LOG}" \
        "${probe_rc}" \
        "not_applicable" \
        "not_applicable" \
        ""
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${_RCH_PROBE_LOG}"; then
        rch_fatal "rch workers are unavailable; refusing local cargo execution. See ${_RCH_PROBE_LOG}"
    fi

    ensure_rch_remote_only_preflight
    ensure_rch_runtime_capabilities
    ensure_rch_mirror_preflight

    if [[ "${RCH_SKIP_SMOKE_PREFLIGHT}" == "1" ]]; then
        printf '%s\n' "Smoke preflight skipped because RCH_SKIP_SMOKE_PREFLIGHT=1" >"${_RCH_SMOKE_LOG}"
        rch_write_meta_json "${_RCH_SMOKE_LOG}" "0"
        rch_emit_proof_ledger_entry \
            "RCH_SKIP_SMOKE_PREFLIGHT=1 ensure_rch_ready" \
            "${_RCH_SMOKE_LOG}" \
            "0" \
            "not_applicable" \
            "not_applicable" \
            "smoke preflight skipped because first material verifier uses run_rch_cargo_logged"
        return 0
    fi

    set +e
    run_rch_cargo_logged_with_timeout "${RCH_SMOKE_TIMEOUT_SECS}" "${_RCH_SMOKE_LOG}" \
        env CARGO_TARGET_DIR="${_RCH_SMOKE_TARGET_DIR}" cargo check --help
    local smoke_rc=$?
    set -e
    if [[ ${smoke_rc} -ne 0 ]]; then
        rch_fatal "rch remote smoke preflight failed. See ${_RCH_SMOKE_LOG}"
    fi
}
