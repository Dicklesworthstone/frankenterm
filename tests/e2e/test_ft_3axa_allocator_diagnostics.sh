#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3axa_allocator_diagnostics"
CORRELATION_ID="ft-3axa-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"
SUMMARY_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.summary.json"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-3axa-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${FT_3AXA_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "${REQUESTED_CARGO_TARGET_DIR}" && "${REQUESTED_CARGO_TARGET_DIR}" != /* ]]; then
    TARGET_DIR="${REQUESTED_CARGO_TARGET_DIR}"
else
    TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
FT_3AXA_CORE_STEP_TIMEOUT_SECS="${FT_3AXA_CORE_STEP_TIMEOUT_SECS:-1800}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "ft_3axa_allocator_diagnostics"

emit_log() {
    local component="$1"
    local step="$2"
    local decision_path="$3"
    local input_summary="$4"
    local outcome="$5"
    local reason_code="$6"
    local error_code="$7"
    local artifact_path="$8"
    local ts
    ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

    jq -cn \
        --arg timestamp "${ts}" \
        --arg component "${component}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg step "${step}" \
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
            step: $step,
            decision_path: $decision_path,
            input_summary: $input_summary,
            outcome: $outcome,
            reason_code: $reason_code,
            error_code: $error_code,
            artifact_path: $artifact_path
        }' >> "${LOG_FILE}"
}

require_cmd() {
    local cmd="$1"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        if command -v jq >/dev/null 2>&1; then
            emit_log \
                "preflight" \
                "dependency_check" \
                "preflight.commands" \
                "missing:${cmd}" \
                "failed" \
                "missing_prerequisite" \
                "E2E-PREREQ" \
                "${cmd}"
        fi
        echo "missing required command: ${cmd}" >&2
        exit 1
    fi
}

run_rch_step() {
    run_rch_step_with_timeout "${RCH_STEP_TIMEOUT_SECS}" "$@"
}

run_rch_step_with_timeout() {
    local timeout_secs="$1"
    local label="$2"
    local decision_path="$3"
    local input_summary="$4"
    shift 4

    local output_file="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
    emit_log \
        "validation" \
        "${label}" \
        "${decision_path}" \
        "${input_summary}" \
        "running" \
        "none" \
        "none" \
        "$(basename "${output_file}")"

    if run_rch_cargo_logged_with_timeout "${timeout_secs}" "${output_file}" env CARGO_TARGET_DIR="${TARGET_DIR}" "$@"; then
        cat "${output_file}" >> "${STDOUT_FILE}"
        emit_log \
            "validation" \
            "${label}" \
            "${decision_path}" \
            "${input_summary}" \
            "passed" \
            "command_succeeded" \
            "none" \
            "$(basename "${output_file}")"
    else
        local rc=$?
        cat "${output_file}" >> "${STDOUT_FILE}" 2>/dev/null || true
        emit_log \
            "validation" \
            "${label}" \
            "${decision_path}" \
            "${input_summary}" \
            "failed" \
            "command_failed" \
            "CARGO-FAIL" \
            "$(basename "${output_file}")"
        exit "${rc}"
    fi
}

: > "${STDOUT_FILE}"
require_cmd jq
require_cmd rch
emit_log \
    "preflight" \
    "scenario_start" \
    "startup" \
    "allocator_diagnostics; local prerequisites jq+rch only; cargo executes remotely via rch" \
    "started" \
    "none" \
    "none" \
    "$(basename "${LOG_FILE}")"
ensure_rch_ready

run_rch_step \
    "allocator_backend_jemalloc" \
    "allocator.feature_gate.jemalloc_backend" \
    "cargo test -p frankenterm-alloc --features jemalloc --lib allocator_backend_matches_feature_gate -- --nocapture" \
    cargo test -p frankenterm-alloc --features jemalloc --lib allocator_backend_matches_feature_gate -- --nocapture

run_rch_step \
    "mux_server_default_allocator" \
    "mux_server.default_allocator.jemalloc" \
    "cargo test -p frankenterm-mux-server jemalloc_feature_matches_allocator_backend -- --nocapture" \
    cargo test -p frankenterm-mux-server jemalloc_feature_matches_allocator_backend -- --nocapture

run_rch_step \
    "arena_stress_nominal_200" \
    "allocator.nominal.full_lifecycle_200_panes" \
    "cargo test -p frankenterm-alloc --test arena_stress stress_200_panes_full_lifecycle_no_leak -- --nocapture" \
    cargo test -p frankenterm-alloc --test arena_stress stress_200_panes_full_lifecycle_no_leak -- --nocapture

run_rch_step \
    "allocator_stats_failure_mode" \
    "allocator.failure.stats_unavailable_without_jemalloc" \
    "cargo test -p frankenterm-alloc --lib allocator_stats_api_matches_feature_mode -- --nocapture" \
    cargo test -p frankenterm-alloc --lib allocator_stats_api_matches_feature_mode -- --nocapture

run_rch_step \
    "arena_restart_recovery" \
    "allocator.recovery.interleaved_release_and_rereserve" \
    "cargo test -p frankenterm-alloc --test arena_stress stress_interleaved_reserve_release -- --nocapture" \
    cargo test -p frankenterm-alloc --test arena_stress stress_interleaved_reserve_release -- --nocapture

run_rch_step_with_timeout \
    "${FT_3AXA_CORE_STEP_TIMEOUT_SECS}" \
    "ingest_lifecycle_bridge" \
    "integration.ingest.discovery_tracks_pane_arenas" \
    "cargo test -p frankenterm-core --lib --no-default-features discovery_tick_tracks_pane_arena_lifecycle -- --nocapture" \
    cargo test -p frankenterm-core --lib --no-default-features discovery_tick_tracks_pane_arena_lifecycle -- --nocapture

run_rch_step_with_timeout \
    "${FT_3AXA_CORE_STEP_TIMEOUT_SECS}" \
    "ipc_observability_surface" \
    "integration.ipc.status_reports_pane_arenas" \
    "cargo test -p frankenterm-core --lib --no-default-features ipc_status_with_registry_reports_pane_arenas -- --nocapture" \
    cargo test -p frankenterm-core --lib --no-default-features ipc_status_with_registry_reports_pane_arenas -- --nocapture

run_rch_step_with_timeout \
    "${FT_3AXA_CORE_STEP_TIMEOUT_SECS}" \
    "scrollback_accounting_sync" \
    "integration.scrollback.syncs_to_allocator_registry" \
    "cargo test -p frankenterm-core --lib --no-default-features sync_to_arena_registry_updates_tracked_bytes -- --nocapture" \
    cargo test -p frankenterm-core --lib --no-default-features sync_to_arena_registry_updates_tracked_bytes -- --nocapture

run_rch_step_with_timeout \
    "${FT_3AXA_CORE_STEP_TIMEOUT_SECS}" \
    "scrollback_unregistered_skip" \
    "integration.scrollback.skips_unregistered_panes" \
    "cargo test -p frankenterm-core --lib --no-default-features sync_to_arena_registry_skips_unregistered_panes -- --nocapture" \
    cargo test -p frankenterm-core --lib --no-default-features sync_to_arena_registry_skips_unregistered_panes -- --nocapture

run_rch_step \
    "benchmark_compile_contract" \
    "benchmark_validation.arena_throughput_compiles" \
    "cargo check -p frankenterm-alloc --bench arena_throughput --message-format short" \
    cargo check -p frankenterm-alloc --bench arena_throughput --message-format short

jq -n \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "ft-3axa" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg json_log "${LOG_FILE#"${ROOT_DIR}/"}" \
    --arg stdout_log "${STDOUT_FILE#"${ROOT_DIR}/"}" \
    --arg target_dir "${TARGET_DIR}" \
    '{
        generated_at_utc: $generated_at,
        bead_id: $bead_id,
        scenario_id: $scenario_id,
        correlation_id: $correlation_id,
        user_impact: {
            pain_removed: "Makes allocator validation reproducible without local CPU-heavy cargo runs and proves pane-arena accounting survives swarm-style churn.",
            measurable_outcomes: [
                "200-pane lifecycle test leaves zero residual pane arenas",
                "IPC status surface keeps pane_id/arena_id/tracked_bytes visible to operators and agents",
                "scrollback accounting sync updates allocator-tracked bytes without touching unregistered panes"
            ],
            no_regression_guarantees: [
                "Allocator stats degrade explicitly when jemalloc stats are unavailable",
                "Recovery path re-reserves panes with fresh accounting after restart-style churn"
            ]
        },
        artifacts: {
            json_log: $json_log,
            stdout_log: $stdout_log
        },
        validation_scope: [
            "jemalloc feature gate",
            "mux server default allocator binding",
            "200-pane allocator stress lifecycle",
            "negative allocator-stats mode without jemalloc",
            "restart/recovery churn",
            "ingest lifecycle bridge",
            "IPC observability surface",
            "scrollback accounting sync",
            "benchmark compile contract"
        ],
        limitations: [
            "Requires healthy rch remote execution; the harness fails closed if rch cannot offload cargo work."
        ],
        target_dir: $target_dir
    }' > "${SUMMARY_FILE}"

emit_log \
    "summary" \
    "scenario_complete" \
    "summary.write" \
    "allocator_diagnostics_summary" \
    "passed" \
    "all_checks_passed" \
    "none" \
    "$(basename "${SUMMARY_FILE}")"

echo "Scenario: ${SCENARIO_ID}"
echo "Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
echo "Summary: ${SUMMARY_FILE#"${ROOT_DIR}/"}"
