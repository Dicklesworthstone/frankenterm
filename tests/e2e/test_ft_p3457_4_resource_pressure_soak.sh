#!/usr/bin/env bash
# E2E: RCH-backed resource-pressure cockpit soak proof lane.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-p3457.4"
SCENARIO_ID="resource_pressure_soak"
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
SOURCE_AUDIT_LOG="${ARTIFACT_DIR}/source_audit.log"
RECEIPTS_TEST_LOG="${ARTIFACT_DIR}/resource_pressure_receipts_tests.log"
HOST_CAPABILITY_LOG="${RECEIPTS_TEST_LOG}"
HOST_CAPABILITY_JSON="${ARTIFACT_DIR}/host_capability.json"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"
PROOF_LEDGER_VALIDATION_DIR=""
SNAPSHOT_BEFORE="${ARTIFACT_DIR}/cockpit-before.json"
SNAPSHOT_DURING="${ARTIFACT_DIR}/cockpit-during.json"
SNAPSHOT_AFTER="${ARTIFACT_DIR}/cockpit-after.json"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-1800}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"
export RCH_MIRROR_REQUIRED_PATHS="${RCH_MIRROR_REQUIRED_PATHS:-}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_p3457_4_resource_pressure_soak"

PASS=0
FAIL=0
TOTAL=0
HOST_CAPABILITY_OK="false"
TARGET_HARDWARE_MET="false"
HIGH_SCALE_PROOF_STATUS="skipped_not_proven"
LIVE_SOAK_STATUS="skipped_not_proven"
REMOTE_RECEIPTS_TEST_OK="false"
PROOF_LEDGER_OK="false"
REMOTE_PREFLIGHT_DEGRADED="false"
SOURCE_MIRROR_DEGRADED="false"
DEFAULT_CARGO_TARGET_DIR="/tmp/ft-p3457-4-resource-pressure-soak-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${FT_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "${REQUESTED_CARGO_TARGET_DIR}" ]]; then
    REMOTE_TARGET_DIR="${REQUESTED_CARGO_TARGET_DIR}"
else
    REMOTE_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}"

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

emit_log() {
    local step="$1"
    local status="$2"
    local duration_ms="$3"
    local message="$4"
    local reason_code="${5:-}"
    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg surface "resource-pressure-soak" \
        --arg step "${step}" \
        --arg status "${status}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg backend "rch" \
        --arg platform "$(uname -srm)" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg redaction "none" \
        --arg message "${message}" \
        --arg reason_code "${reason_code}" \
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
        } + (if $reason_code == "" then {} else {reason_code: $reason_code} end)' >>"${STRUCTURED_LOG}"
}

record_result() {
    local step="$1"
    local ok="$2"
    local duration_ms="$3"
    local message="$4"
    local reason_code="${5:-}"
    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_log "${step}" "passed" "${duration_ms}" "${message}" "${reason_code}"
    else
        FAIL=$((FAIL + 1))
        emit_log "${step}" "failed" "${duration_ms}" "${message}" "${reason_code}"
    fi
}

run_checked() {
    local step="$1"
    local log_file="$2"
    shift 2
    local start_ns end_ns duration_ms rc
    start_ns="$(date +%s%N)"
    record_command "$*"
    set +e
    "$@" >"${log_file}" 2>&1
    rc=$?
    set -e
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    if [[ ${rc} -eq 0 ]]; then
        record_result "${step}" "true" "${duration_ms}" "${log_file}"
        return 0
    fi
    record_result "${step}" "false" "${duration_ms}" "${log_file}" "resource.proof.failed_static_check"
    return 1
}

run_rch_step() {
    local step="$1"
    local log_file="$2"
    shift 2
    local start_ns end_ns duration_ms rc
    start_ns="$(date +%s%N)"
    record_command "run_rch_cargo_logged $*"
    set +e
    run_rch_cargo_logged "${log_file}" "$@"
    rc=$?
    set -e
    end_ns="$(date +%s%N)"
    duration_ms="$(((end_ns - start_ns) / 1000000))"
    if [[ ${rc} -eq 0 ]]; then
        record_result "${step}" "true" "${duration_ms}" "${log_file}"
        return 0
    fi
    record_result "${step}" "false" "${duration_ms}" "${log_file}" "resource.proof.remote_command_failed"
    return 1
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
        printf 'required_logical_cpus=%s\n' "64"
        printf 'required_memory_gib=%s\n' "256"
        printf 'target_panes=%s\n' "200"
        printf 'rch_require_remote=%s\n' "${RCH_REQUIRE_REMOTE}"
        printf 'rch_skip_smoke_preflight=%s\n' "${RCH_SKIP_SMOKE_PREFLIGHT}"
        printf 'rch_step_timeout_secs=%s\n' "${RCH_STEP_TIMEOUT_SECS}"
        printf 'rch_mirror_required_paths=%s\n' "${RCH_MIRROR_REQUIRED_PATHS}"
    } >"${ENV_FILE}"
}

write_default_host_capability() {
    jq -cn \
        --arg status "unavailable" \
        --arg reason_code "resource.telemetry.unavailable" \
        '{
          status: $status,
          reason_code: $reason_code,
          logical_cpus: 0,
          memory_kib: 0,
          memory_gib: 0,
          probe_rss_kib: 0,
          uname: "unknown"
        }' >"${HOST_CAPABILITY_JSON}"
}

extract_host_capability() {
    local host_line logical_cpus memory_kib
    host_line="$(grep -E '^FT_P3457_HOST_CAPABILITY_JSON:' "${HOST_CAPABILITY_LOG}" 2>/dev/null | tail -n 1 | sed 's/^FT_P3457_HOST_CAPABILITY_JSON://')"
    [[ -n "${host_line}" ]] || return 1
    printf '%s\n' "${host_line}" | jq -e '
      (.logical_cpus | type == "number") and
      (.memory_kib | type == "number") and
      (.probe_rss_kib | type == "number") and
      (.uname | type == "string")
    ' >/dev/null
    printf '%s\n' "${host_line}" | jq \
        --arg status "measured" \
        --arg reason_code "resource.proof.remote_host_capability_measured" \
        '. + {
          status: $status,
          reason_code: $reason_code,
          memory_gib: (.memory_kib / 1024 / 1024)
        }' >"${HOST_CAPABILITY_JSON}"

    logical_cpus="$(jq -r '.logical_cpus' "${HOST_CAPABILITY_JSON}")"
    memory_kib="$(jq -r '.memory_kib' "${HOST_CAPABILITY_JSON}")"
    HOST_CAPABILITY_OK="true"
    if [[ "${logical_cpus}" =~ ^[0-9]+$ && "${memory_kib}" =~ ^[0-9]+$ ]] \
        && ((logical_cpus >= 64)) \
        && ((memory_kib >= 268435456))
    then
        TARGET_HARDWARE_MET="true"
        HIGH_SCALE_PROOF_STATUS="target_hardware_measured"
    else
        TARGET_HARDWARE_MET="false"
        HIGH_SCALE_PROOF_STATUS="skipped_not_proven"
    fi
}

update_preflight_degraded_state() {
    local remote_status mirror_status
    remote_status="$(jq -r '.status // ""' "$(rch_remote_preflight_log_path)" 2>/dev/null || true)"
    mirror_status="$(jq -r '.status // ""' "$(rch_mirror_preflight_log_path)" 2>/dev/null || true)"
    if [[ "${remote_status}" == "warning" ]]; then
        REMOTE_PREFLIGHT_DEGRADED="true"
    fi
    if [[ "${mirror_status}" != "" && "${mirror_status}" != "passed" ]]; then
        SOURCE_MIRROR_DEGRADED="true"
    fi
}

write_mirror_preflight_not_checked() {
    local mirror_path
    mirror_path="$(rch_mirror_preflight_log_path)"
    if [[ -f "${mirror_path}" ]]; then
        return 0
    fi
    jq -cn \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg probe_log "$(rch_repo_relative_path "$(rch_probe_log_path)")" \
        --arg scheduler_workers_log "$(rch_repo_relative_path "$(rch_scheduler_workers_log_path)")" \
        '{
          schema_version: 1,
          kind: "rch_worker_pool_mirror_preflight",
          generated_at: $generated_at,
          status: "not_checked",
          reason_code: "source_mirror_preflight_not_requested",
          detail: "RCH_MIRROR_REQUIRED_PATHS was empty; selected-worker remote execution remains proven by the proof ledger",
          total_workers_checked: 0,
          failed_workers: 0,
          blocking_failed_workers: 0,
          block_on_stale_head: false,
          scheduler_filter_active: false,
          artifacts: {
            probe_log: $probe_log,
            scheduler_workers_log: $scheduler_workers_log
          },
          worker_results: []
        }' >"${mirror_path}"
}

write_cockpit_snapshot() {
    local phase="$1"
    local output_file="$2"
    local evidence_state="$3"
    local proof_gate="$4"
    jq -cn \
        --slurpfile host "${HOST_CAPABILITY_JSON}" \
        --arg phase "${phase}" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg run_id "${RUN_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg evidence_state "${evidence_state}" \
        --arg proof_gate "${proof_gate}" \
        --arg high_scale_proof_status "${HIGH_SCALE_PROOF_STATUS}" \
        --arg live_soak_status "${LIVE_SOAK_STATUS}" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --argjson target_hardware_met "${TARGET_HARDWARE_MET}" \
        --argjson target_panes 200 \
        '
        ($host[0] // {}) as $host_capability
        | {
          schema_version: 1,
          contract_id: "ft.resource_pressure_cockpit.v1",
          generated_at_ms: (now * 1000 | floor),
          source: "tests/e2e/test_ft_p3457_4_resource_pressure_soak.sh",
          phase: $phase,
          status: "watch",
          proof_gate: $proof_gate,
          evidence_state: $evidence_state,
          summary: "RCH resource-pressure soak lane retained artifacts without claiming live 200-pane proof",
          next_operator_move: "Inspect summary.json and proof-ledger.jsonl before citing any high-scale result",
          run_identity: {
            bead_id: $bead_id,
            scenario_id: $scenario_id,
            run_id: $run_id,
            correlation_id: $correlation_id,
            evidence_level: "remote_reduced",
            artifact_dir: $artifact_dir,
            remote_cargo_target_dir: $remote_target_dir,
            hardware_predicate: {
              required_logical_cpus: 64,
              required_memory_gib: 256,
              observed_logical_cpus: ($host_capability.logical_cpus // 0),
              observed_memory_gib: ($host_capability.memory_gib // 0),
              target_class: $target_hardware_met,
              proof_status: $high_scale_proof_status
            },
            live_soak: {
              target_panes: $target_panes,
              proof_status: $live_soak_status,
              reason_codes: ["resource.proof.skipped", "resource.telemetry.simulated"]
            }
          },
          domains: {
            memory: {
              name: "memory",
              evidence_state: "simulated",
              pressure_tier: "unknown",
              summary: "scaled-equivalent memory pressure scenario defined; live pane memory was not mutated",
              operator_action: "run on target hardware before citing high-scale pressure",
              reason_codes: ["resource.telemetry.simulated"]
            },
            rss_residency: {
              name: "rss_residency",
              evidence_state: "unavailable",
              pressure_tier: "unknown",
              summary: "no live FrankenTerm process RSS peak was captured by this first proof lane",
              operator_action: "capture ft doctor and macOS residency bundle during a live soak",
              reason_codes: ["resource.telemetry.unavailable"]
            },
            queue_backpressure: {
              name: "queue_backpressure",
              evidence_state: "simulated",
              pressure_tier: "unknown",
              summary: "queue pressure is represented by the scenario definition, not live queue mutation",
              operator_action: "run the live swarm exercise before promotion",
              reason_codes: ["resource.telemetry.simulated"]
            },
            storage_io: {
              name: "storage_io",
              evidence_state: "simulated",
              pressure_tier: "unknown",
              summary: "storage IO pressure is in the scaled-equivalent plan only",
              operator_action: "retain storage scheduler artifacts during a live soak",
              reason_codes: ["resource.telemetry.simulated"]
            },
            worker_pool: {
              name: "worker_pool",
              evidence_state: ($host_capability.status // "unavailable"),
              pressure_tier: "unknown",
              summary: "RCH worker host capability is measured separately from fleet worker-pool saturation",
              operator_action: "inspect RCH preflight and proof-ledger artifacts",
              reason_codes: [($host_capability.reason_code // "resource.telemetry.unavailable")]
            }
          },
          residency_buckets: [
            {
              bucket: "unknown",
              evidence_state: "unavailable",
              peak_rss_bytes: null,
              probe_rss_kib: ($host_capability.probe_rss_kib // 0),
              reason_codes: ["resource.telemetry.unavailable"]
            }
          ],
          queue_backpressure: [
            {
              queue: "resource_admission",
              evidence_state: "simulated",
              target_panes: $target_panes,
              reason_codes: ["resource.telemetry.simulated"]
            }
          ],
          action_receipts: [
            {
              receipt_id: ($bead_id + ":" + $run_id + ":receipts-test"),
              action: "dry_run",
              target_domain: "action_receipts",
              status: "dry_run",
              dry_run: true,
              policy_decision: "not_checked",
              evidence_state: "measured",
              reason_codes: ["action_receipt.dry_run"],
              artifact_paths: ["proof-ledger.jsonl", "resource_pressure_receipts_tests.log"]
            }
          ],
          artifact_paths: [
            "commands.txt",
            "env.txt",
            "structured.log",
            "summary.json",
            "proof-ledger.jsonl",
            "host_capability.json"
          ]
        }' >"${output_file}"
}

write_summary() {
    jq -cn \
        --slurpfile host "${HOST_CAPABILITY_JSON}" \
        --slurpfile ledger "${PROOF_LEDGER_FILE}" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "${ARTIFACT_DIR}" \
        --arg commands_file "${COMMANDS_FILE}" \
        --arg env_file "${ENV_FILE}" \
        --arg structured_log "${STRUCTURED_LOG}" \
        --arg stdout_file "${STDOUT_FILE}" \
        --arg stderr_file "${STDERR_FILE}" \
        --arg host_capability_log "${HOST_CAPABILITY_LOG}" \
        --arg host_capability_json "${HOST_CAPABILITY_JSON}" \
        --arg host_capability_meta "$(rch_log_meta_path "${HOST_CAPABILITY_LOG}")" \
        --arg source_audit_log "${SOURCE_AUDIT_LOG}" \
        --arg receipts_test_log "${RECEIPTS_TEST_LOG}" \
        --arg receipts_test_meta "$(rch_log_meta_path "${RECEIPTS_TEST_LOG}")" \
        --arg proof_ledger "${PROOF_LEDGER_FILE}" \
        --arg proof_ledger_validation_dir "${PROOF_LEDGER_VALIDATION_DIR}" \
        --arg remote_preflight "$(rch_remote_preflight_log_path)" \
        --arg mirror_preflight "$(rch_mirror_preflight_log_path)" \
        --arg snapshot_before "${SNAPSHOT_BEFORE}" \
        --arg snapshot_during "${SNAPSHOT_DURING}" \
        --arg snapshot_after "${SNAPSHOT_AFTER}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --arg high_scale_proof_status "${HIGH_SCALE_PROOF_STATUS}" \
        --arg live_soak_status "${LIVE_SOAK_STATUS}" \
        --argjson pass_count "${PASS}" \
        --argjson fail_count "${FAIL}" \
        --argjson total_count "${TOTAL}" \
        --argjson host_capability_ok "${HOST_CAPABILITY_OK}" \
        --argjson target_hardware_met "${TARGET_HARDWARE_MET}" \
        --argjson remote_receipts_test_ok "${REMOTE_RECEIPTS_TEST_OK}" \
        --argjson proof_ledger_ok "${PROOF_LEDGER_OK}" \
        --argjson remote_preflight_degraded "${REMOTE_PREFLIGHT_DEGRADED}" \
        --argjson source_mirror_degraded "${SOURCE_MIRROR_DEGRADED}" \
        '
        ($host[0] // {}) as $host_capability
        | {
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          status: (if $fail_count == 0 then "passed" else "failed" end),
          correlation_id: $correlation_id,
          artifact_dir: $artifact_dir,
          pass_count: $pass_count,
          fail_count: $fail_count,
          total_count: $total_count,
          remote_cargo_target_dir: $remote_target_dir,
          host_capability: $host_capability,
          high_scale_claim: {
            target_panes: 200,
            required_logical_cpus: 64,
            required_memory_gib: 256,
            target_hardware_met: $target_hardware_met,
            proof_status: $high_scale_proof_status,
            live_soak_status: $live_soak_status,
            reason_codes: (
              if $target_hardware_met then
                ["resource.proof.skipped", "resource.telemetry.simulated"]
              else
                ["resource.proof.target_hardware_missing", "resource.proof.skipped"]
              end
            )
          },
          evidence: {
            measured: {
              remote_host_capability: $host_capability_ok,
              remote_receipts_cargo_test: $remote_receipts_test_ok,
              proof_ledger_validated: $proof_ledger_ok
            },
            simulated: {
              scaled_equivalent_pressure_snapshots: true,
              live_pane_mutation: false
            },
            skipped: {
              high_scale_200_pane_claim: ($live_soak_status == "skipped_not_proven"),
              reason_code: (
                if $target_hardware_met then
                  "resource.telemetry.simulated"
                else
                  "resource.proof.target_hardware_missing"
                end
              )
            },
            degraded: {
              remote_preflight_warning: $remote_preflight_degraded,
              source_mirror_warning: $source_mirror_degraded
            },
            failed: {
              count: $fail_count
            }
          },
          rss_residency: {
            peak_rss_bytes: null,
            evidence_state: "unavailable",
            probe_rss_kib: ($host_capability.probe_rss_kib // 0),
            buckets: [
              {
                bucket: "unknown",
                evidence_state: "unavailable",
                reason_codes: ["resource.telemetry.unavailable"]
              }
            ]
          },
          proof_ledger_entries: ($ledger | length),
          selected_workers: [
            $ledger[]
            | .runs[]
            | select(.selected_worker_id != null)
            | .selected_worker_id
          ] | unique,
          remote_cargo_reached: any($ledger[]?.runs[]?; .remote_cargo_reached == true),
          remote_rustc_reached: any($ledger[]?.runs[]?; .remote_rustc_reached == true),
          test_binary_reached: any($ledger[]?.runs[]?; .test_binary_reached == true),
          source_mirror_statuses: [
            $ledger[]
            | .runs[]
            | .source_mirror_status
          ] | unique,
          artifacts: {
            commands: $commands_file,
            env: $env_file,
            structured_log: $structured_log,
            stdout: $stdout_file,
            stderr: $stderr_file,
            host_capability_log: $host_capability_log,
            host_capability_json: $host_capability_json,
            host_capability_meta: $host_capability_meta,
            source_audit: $source_audit_log,
            resource_pressure_receipts_tests: $receipts_test_log,
            resource_pressure_receipts_tests_meta: $receipts_test_meta,
            proof_ledger: $proof_ledger,
            proof_ledger_validation_dir: $proof_ledger_validation_dir,
            remote_preflight: $remote_preflight,
            mirror_preflight: $mirror_preflight,
            cockpit_before: $snapshot_before,
            cockpit_during: $snapshot_during,
            cockpit_after: $snapshot_after
          }
        }' >"${SUMMARY_FILE}"
}

echo "=== ${BEAD_ID} resource-pressure RCH soak lane ==="
write_env
write_default_host_capability
: >"${PROOF_LEDGER_FILE}"
command -v jq >/dev/null 2>&1
command -v rg >/dev/null 2>&1
command -v rch >/dev/null 2>&1

record_command "ensure_rch_ready (RCH_SKIP_SMOKE_PREFLIGHT=${RCH_SKIP_SMOKE_PREFLIGHT})"
ensure_rch_ready
write_mirror_preflight_not_checked
update_preflight_degraded_state

if ! run_checked \
    "source_audit" \
    "${SOURCE_AUDIT_LOG}" \
    bash -lc "
        set -euo pipefail
        test -f '${ROOT_DIR}/docs/resource-pressure-cockpit-contract.md'
        test -f '${ROOT_DIR}/docs/json-schema/ft-resource-pressure-cockpit.json'
        test -f '${ROOT_DIR}/crates/frankenterm-core/src/memory_pressure.rs'
        jq empty '${ROOT_DIR}/docs/json-schema/ft-resource-pressure-cockpit.json'
        rg -n 'ResourcePressureReceiptStatus|ResourcePressureEvidenceState|summarize_resource_pressure_receipts|resource_pressure_soak_host_capability_probe' '${ROOT_DIR}/crates/frankenterm-core/src/memory_pressure.rs'
        rg -n 'High-scale or 200\\+ pane cockpit claim|skipped_not_proven' '${ROOT_DIR}/docs/resource-pressure-cockpit-contract.md'
        bash -n '${ROOT_DIR}/tests/e2e/test_ft_p3457_4_resource_pressure_soak.sh'
    "
then
    :
fi

write_cockpit_snapshot "before" "${SNAPSHOT_BEFORE}" "mixed" "skipped_proof"
write_cockpit_snapshot "during" "${SNAPSHOT_DURING}" "mixed" "skipped_proof"

if run_rch_step \
    "resource_pressure_receipts_tests" \
    "${RECEIPTS_TEST_LOG}" \
    env CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
        cargo test -p frankenterm-core resource_pressure_ --lib -- --nocapture
then
    REMOTE_RECEIPTS_TEST_OK="true"
fi

start_ns="$(date +%s%N)"
if extract_host_capability; then
    end_ns="$(date +%s%N)"
    record_result "host_capability_parse" "true" "$(((end_ns - start_ns) / 1000000))" "${HOST_CAPABILITY_JSON}"
else
    end_ns="$(date +%s%N)"
    record_result "host_capability_parse" "false" "$(((end_ns - start_ns) / 1000000))" "${HOST_CAPABILITY_LOG}" "resource.proof.host_capability_unparseable"
fi

if [[ -s "${PROOF_LEDGER_FILE}" ]]; then
    start_ns="$(date +%s%N)"
    PROOF_LEDGER_VALIDATION_DIR="$(rch_validate_proof_ledger_file "${PROOF_LEDGER_FILE}")"
    end_ns="$(date +%s%N)"
    PROOF_LEDGER_OK="true"
    record_result "proof_ledger_validation" "true" "$(((end_ns - start_ns) / 1000000))" "${PROOF_LEDGER_VALIDATION_DIR}"
else
    record_result "proof_ledger_validation" "false" "0" "${PROOF_LEDGER_FILE}" "resource.proof.missing_ledger"
fi

write_cockpit_snapshot "after" "${SNAPSHOT_AFTER}" "mixed" "skipped_proof"
write_summary

if [[ "${FAIL}" -ne 0 ]]; then
    echo "${BEAD_ID} resource-pressure RCH soak lane FAILED. Summary: ${SUMMARY_FILE}" >&2
    exit 1
fi

echo "${BEAD_ID} resource-pressure RCH soak lane passed. Summary: ${SUMMARY_FILE}"
