#!/usr/bin/env bash
# E2E: ft-e87u6.8 attestation epic convergence guard.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-e87u6.8"
SCENARIO_ID="epic_convergence"
SURFACE="docs/attestations + scripts/attestation-* + frankenterm-core tests"
RUN_ID="${FT_E87U6_8_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${FT_E87U6_8_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-e87u6/convergence/${RUN_ID}}"
REMOTE_TARGET_DIR="${FT_E87U6_8_CARGO_TARGET_DIR:-/tmp/ft-e87u6-8-epic-convergence-${RUN_ID}}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/convergence_summary.json"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SHELL_LOG="${ARTIFACT_DIR}/shell-static.log"
BUILD_LOG="${ARTIFACT_DIR}/attestation-build.log"
STRICT_BUILD_LOG="${ARTIFACT_DIR}/attestation-build-strict-deferred.log"
VERIFY_LOG="${ARTIFACT_DIR}/attestation-verify.log"
HEDGE_LOG="${ARTIFACT_DIR}/readme-hedge-alignment-rch.log"
MANIFEST_LOG="${ARTIFACT_DIR}/attestation-manifest-completeness-rch.log"
FOOTNOTES_LOG="${ARTIFACT_DIR}/readme-footnotes-resolve-rch.log"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"
RESOLUTION_FILE="${ARTIFACT_DIR}/original-null-slot-resolution.json"
HEDGE_SCAN_FILE="${ARTIFACT_DIR}/memory-envelope-hedge-scan.txt"
CHECKLIST_SCAN_FILE="${ARTIFACT_DIR}/closing-checklist-regression-refs.txt"
BUILD_DIR="${ARTIFACT_DIR}/bundle-dev"
STRICT_BUILD_DIR="${ARTIFACT_DIR}/bundle-dev-strict"
BUNDLE_PATH="${BUILD_DIR}/0.0.0-dev.json"

mkdir -p "${ARTIFACT_DIR}" "${BUILD_DIR}" "${STRICT_BUILD_DIR}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"
: >"${PROOF_LEDGER_FILE}"
: >"${HEDGE_SCAN_FILE}"
: >"${CHECKLIST_SCAN_FILE}"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-3600}"
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-1}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-1}"
export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-1}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"
if [[ -n "${RCH_MIRROR_REQUIRED_PATHS:-}" ]]; then
    export RCH_MIRROR_REQUIRED_PATHS="${RCH_MIRROR_REQUIRED_PATHS}:README.md:AGENTS.md:docs/attestations/manifest.json:docs/attestations/null-slot-reconciliation.json:docs/release/attestation-checklist.md:scripts/attestation-build.sh:scripts/attestation-verify.sh:crates/frankenterm-core/tests/readme_hedge_alignment.rs:crates/frankenterm-core/tests/attestation_manifest_completeness.rs:crates/frankenterm-core/tests/readme_footnotes_resolve.rs"
else
    export RCH_MIRROR_REQUIRED_PATHS="README.md:AGENTS.md:docs/attestations/manifest.json:docs/attestations/null-slot-reconciliation.json:docs/release/attestation-checklist.md:scripts/attestation-build.sh:scripts/attestation-verify.sh:crates/frankenterm-core/tests/readme_hedge_alignment.rs:crates/frankenterm-core/tests/attestation_manifest_completeness.rs:crates/frankenterm-core/tests/readme_footnotes_resolve.rs"
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e87u6_8_epic_convergence" "${ROOT_DIR}"

PASS=0
FAIL=0
TOTAL=0
EXPECTED_FAIL=0
SKIPPED=0
RCH_SUBSTRATE_BLOCKED=false
FINALIZED=0

UNRESOLVED_SLOT_COUNT=0
DEFERRED_SLOT_COUNT=0
STRICT_BUILD_EXIT=-1
BUILD_EXIT=-1
VERIFY_EXIT=-1
HEDGE_EXIT=-1
MANIFEST_EXIT=-1
FOOTNOTES_EXIT=-1
HEDGE_MATCH_COUNT=0
CHECKLIST_REF_COUNT=0

relative_path() {
    local path="$1"
    if [[ "${path}" == "${ROOT_DIR}/"* ]]; then
        printf '%s\n' "${path#"${ROOT_DIR}/"}"
    else
        printf '%s\n' "${path}"
    fi
}

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

emit_event() {
    local step="$1"
    local outcome="$2"
    local reason_code="$3"
    local error_code="$4"
    local artifact_path="$5"
    local message="$6"
    local detail="${7:-}"

    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg surface "${SURFACE}" \
        --arg step "${step}" \
        --arg outcome "${outcome}" \
        --arg reason_code "${reason_code}" \
        --arg error_code "${error_code}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_path "$(relative_path "${artifact_path}")" \
        --arg message "${message}" \
        --arg detail "${detail}" \
        '{
          timestamp: $timestamp,
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          surface: $surface,
          step: $step,
          outcome: $outcome,
          reason_code: $reason_code,
          error_code: $error_code,
          correlation_id: $correlation_id,
          artifact_path: $artifact_path,
          message: $message
        } + (if $detail == "" then {} else {detail: $detail} end)' \
        >>"${STRUCTURED_LOG}"
}

record_result() {
    local step="$1"
    local outcome="$2"
    local reason_code="$3"
    local error_code="$4"
    local artifact_path="$5"
    local message="$6"
    local detail="${7:-}"

    TOTAL=$((TOTAL + 1))
    case "${outcome}" in
        passed)
            PASS=$((PASS + 1))
            ;;
        expected_failure)
            EXPECTED_FAIL=$((EXPECTED_FAIL + 1))
            ;;
        skipped)
            SKIPPED=$((SKIPPED + 1))
            ;;
        *)
            FAIL=$((FAIL + 1))
            ;;
    esac
    emit_event "${step}" "${outcome}" "${reason_code}" "${error_code}" "${artifact_path}" "${message}" "${detail}"
}

selected_workers_json() {
    jq -scr '[.[].runs[]?.selected_worker_id | select(. != null and . != "required;")] | unique' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf '[]'
}

proof_ledger_any() {
    local query="$1"
    jq -sr "any(.[].runs[]?; ${query})" "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false'
}

write_summary() {
    local selected_workers remote_cargo_reached remote_rustc_reached test_binary_reached
    selected_workers="$(selected_workers_json)"
    remote_cargo_reached="$(proof_ledger_any '.remote_cargo_reached == true')"
    remote_rustc_reached="$(proof_ledger_any '.remote_rustc_reached == true')"
    test_binary_reached="$(proof_ledger_any '.test_binary_reached == true')"

    jq -n \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg run_id "${RUN_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "$(relative_path "${ARTIFACT_DIR}")" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --arg bundle_path "$(relative_path "${BUNDLE_PATH}")" \
        --arg commands "$(relative_path "${COMMANDS_FILE}")" \
        --arg structured "$(relative_path "${STRUCTURED_LOG}")" \
        --arg stdout "$(relative_path "${STDOUT_FILE}")" \
        --arg stderr "$(relative_path "${STDERR_FILE}")" \
        --arg shell_log "$(relative_path "${SHELL_LOG}")" \
        --arg build_log "$(relative_path "${BUILD_LOG}")" \
        --arg strict_build_log "$(relative_path "${STRICT_BUILD_LOG}")" \
        --arg verify_log "$(relative_path "${VERIFY_LOG}")" \
        --arg hedge_log "$(relative_path "${HEDGE_LOG}")" \
        --arg manifest_log "$(relative_path "${MANIFEST_LOG}")" \
        --arg footnotes_log "$(relative_path "${FOOTNOTES_LOG}")" \
        --arg proof_ledger "$(relative_path "${PROOF_LEDGER_FILE}")" \
        --arg resolution_file "$(relative_path "${RESOLUTION_FILE}")" \
        --arg hedge_scan "$(relative_path "${HEDGE_SCAN_FILE}")" \
        --arg checklist_scan "$(relative_path "${CHECKLIST_SCAN_FILE}")" \
        --argjson pass_count "${PASS}" \
        --argjson fail_count "${FAIL}" \
        --argjson expected_fail_count "${EXPECTED_FAIL}" \
        --argjson skipped_count "${SKIPPED}" \
        --argjson total_count "${TOTAL}" \
        --argjson unresolved_slot_count "${UNRESOLVED_SLOT_COUNT}" \
        --argjson deferred_slot_count "${DEFERRED_SLOT_COUNT}" \
        --argjson strict_build_exit "${STRICT_BUILD_EXIT}" \
        --argjson build_exit "${BUILD_EXIT}" \
        --argjson verify_exit "${VERIFY_EXIT}" \
        --argjson hedge_exit "${HEDGE_EXIT}" \
        --argjson manifest_exit "${MANIFEST_EXIT}" \
        --argjson footnotes_exit "${FOOTNOTES_EXIT}" \
        --argjson hedge_match_count "${HEDGE_MATCH_COUNT}" \
        --argjson checklist_ref_count "${CHECKLIST_REF_COUNT}" \
        --argjson rch_substrate_blocked "${RCH_SUBSTRATE_BLOCKED}" \
        --argjson selected_workers "${selected_workers}" \
        --argjson remote_cargo_reached "${remote_cargo_reached}" \
        --argjson remote_rustc_reached "${remote_rustc_reached}" \
        --argjson test_binary_reached "${test_binary_reached}" \
        --slurpfile resolution "${RESOLUTION_FILE}" \
        '{
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          run_id: $run_id,
          correlation_id: $correlation_id,
          status: (if $rch_substrate_blocked then "rch_substrate_blocked" elif $fail_count == 0 then "passed" else "failed" end),
          artifact_dir: $artifact_dir,
          remote_cargo_target_dir: $remote_target_dir,
          bundle_path: $bundle_path,
          counts: {
            total: $total_count,
            passed: $pass_count,
            expected_failures: $expected_fail_count,
            skipped: $skipped_count,
            failed: $fail_count,
            unresolved_manifest_slots: $unresolved_slot_count,
            deferred_manifest_slots: $deferred_slot_count,
            memory_envelope_hedge_matches: $hedge_match_count,
            checklist_regression_refs: $checklist_ref_count
          },
          exit_codes: {
            attestation_build: $build_exit,
            attestation_build_strict_deferred: $strict_build_exit,
            attestation_verify: $verify_exit,
            readme_hedge_alignment: $hedge_exit,
            attestation_manifest_completeness: $manifest_exit,
            readme_footnotes_resolve: $footnotes_exit
          },
          original_null_slot_convergence: ($resolution[0] // {}),
          rch_substrate_blocked: $rch_substrate_blocked,
          remote: {
            selected_workers: $selected_workers,
            remote_cargo_reached: $remote_cargo_reached,
            remote_rustc_reached: $remote_rustc_reached,
            test_binary_reached: $test_binary_reached
          },
          artifacts: {
            commands: $commands,
            structured_log: $structured,
            stdout: $stdout,
            stderr: $stderr,
            shell_static: $shell_log,
            attestation_build_log: $build_log,
            strict_deferred_build_log: $strict_build_log,
            attestation_verify_log: $verify_log,
            readme_hedge_alignment_log: $hedge_log,
            attestation_manifest_completeness_log: $manifest_log,
            readme_footnotes_resolve_log: $footnotes_log,
            proof_ledger: $proof_ledger,
            original_null_slot_resolution: $resolution_file,
            memory_envelope_hedge_scan: $hedge_scan,
            closing_checklist_scan: $checklist_scan
          },
          final_statement: "the 2026-05-09 reality-check NO_BEAD gap (manifest.json had 9 of 14 slots null) is closed; remaining null slots, if any, carry deferred_to_bead references documented in convergence_summary.json"
        }' >"${SUMMARY_FILE}"
}

finalize() {
    local rc="$1"
    if [[ "${FINALIZED}" -eq 1 ]]; then
        exit "${rc}"
    fi
    FINALIZED=1
    if [[ ! -f "${RESOLUTION_FILE}" ]]; then
        jq -n '{status:"not_generated"}' >"${RESOLUTION_FILE}"
    fi
    write_summary
    exit "${rc}"
}

trap 'finalize "$?"' EXIT

require_cmd() {
    local cmd="$1"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        record_result "preflight.${cmd}" "failed" "missing_prerequisite" "missing_prerequisite" "${cmd}" "Required command is missing."
        return 1
    fi
}

run_shell_static() {
    record_command "bash -n ${BASH_SOURCE[0]}"
    if bash -n "${BASH_SOURCE[0]}" >"${SHELL_LOG}" 2>&1; then
        record_result "convergence.shell_static" "passed" "shell_static_passed" "none" "${SHELL_LOG}" "Harness shell syntax passed."
    else
        record_result "convergence.shell_static" "failed" "shell_static_failed" "epic_convergence_harness_invalid" "${SHELL_LOG}" "Harness shell syntax failed."
        return 1
    fi
}

run_manifest_gap_check() {
    UNRESOLVED_SLOT_COUNT="$(
        jq '[.slots[]? | select((.path == null) and (.deferred_to_bead == null))] | length' \
            "${ROOT_DIR}/docs/attestations/manifest.json"
    )"
    DEFERRED_SLOT_COUNT="$(
        jq '[.slots[]? | select((.path == null) and (.deferred_to_bead != null))] | length' \
            "${ROOT_DIR}/docs/attestations/manifest.json"
    )"

    jq -n \
        --slurpfile original "${ROOT_DIR}/docs/attestations/null-slot-reconciliation.json" \
        --slurpfile manifest "${ROOT_DIR}/docs/attestations/manifest.json" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        '
        ($original[0].slots // []) as $original_slots
        | ($manifest[0].slots // []) as $live_slots
        | {
            generated_at: $generated_at,
            bead_id: $bead_id,
            scenario_id: $scenario_id,
            original_manifest_sha256: ($original[0].manifest.sha256 // null),
            original_null_slots_observed: ($original[0].manifest.null_slots_observed // ($original_slots | length)),
            live_manifest_slot_count: ($live_slots | length),
            live_unresolved_null_slots: [
              $live_slots[]
              | select((.path == null) and (.deferred_to_bead == null))
              | {category, produced_by_bead}
            ],
            live_deferred_slots: [
              $live_slots[]
              | select((.path == null) and (.deferred_to_bead != null))
              | {category, produced_by_bead, deferred_to_bead, deferred_reason}
            ],
            original_slots: [
              $original_slots[]
              | . as $slot
              | ($live_slots | map(select(.category == $slot.category))) as $matches
              | {
                  category: $slot.category,
                  original_disposition: $slot.disposition,
                  original_artifact_path: $slot.artifact_path,
                  original_follow_up_bead: $slot.follow_up_bead,
                  current_paths: ($matches | map(.path // empty)),
                  current_deferred_to_beads: ($matches | map(.deferred_to_bead // empty)),
                  current_resolution:
                    (if ($matches | length) == 0 then "slot-deletion"
                     elif ($matches | any((.path == null) and (.deferred_to_bead == null))) then "unresolved-null-slot"
                     elif ($matches | any(.deferred_to_bead != null)) then "deferred-to-live-bead"
                     else "populated" end)
                }
            ]
          }
        ' >"${RESOLUTION_FILE}"

    if [[ "${UNRESOLVED_SLOT_COUNT}" -eq 0 ]]; then
        record_result "convergence.no_bead_gap" "passed" "no_unresolved_null_slots" "none" "${RESOLUTION_FILE}" "Manifest has zero path=null and deferred_to_bead=null slots."
    else
        record_result "convergence.no_bead_gap" "failed" "unresolved_null_slots" "convergence_failed_no_bead_gap" "${RESOLUTION_FILE}" "Manifest still has unresolved null slots."
        return 1
    fi

    if jq -e '.original_null_slots_observed == 9 and (.original_slots | length == 9) and ([.original_slots[] | select(.current_resolution == "unresolved-null-slot")] | length == 0)' "${RESOLUTION_FILE}" >/dev/null; then
        record_result "convergence.original_gap_history" "passed" "original_gap_resolved" "none" "${RESOLUTION_FILE}" "Original 9 null slots all resolve to populated/deferred/deleted dispositions."
    else
        record_result "convergence.original_gap_history" "failed" "original_gap_unresolved" "convergence_failed_original_gap_history" "${RESOLUTION_FILE}" "Original null-slot reconciliation history is incomplete or unresolved."
        return 1
    fi
}

run_attestation_build() {
    record_command "FT_ATTESTATION_OUT_DIR=${BUILD_DIR} scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned"
    set +e
    FT_ATTESTATION_OUT_DIR="${BUILD_DIR}" \
    FT_BEAD_ID="${BEAD_ID}" \
    FT_SCENARIO_ID="${SCENARIO_ID}" \
    FT_CORRELATION_ID="${CORRELATION_ID}" \
        bash "${ROOT_DIR}/scripts/attestation-build.sh" \
            --version 0.0.0-dev \
            --channel dev \
            --sign unsigned >"${BUILD_LOG}" 2>&1
    BUILD_EXIT=$?
    set -e

    if [[ "${BUILD_EXIT}" -eq 0 && -f "${BUNDLE_PATH}" ]]; then
        record_result "convergence.attestation_build" "passed" "bundle_built" "none" "${BUILD_LOG}" "Full dev bundle built without --allow-partial."
        return 0
    fi

    record_result "convergence.attestation_build" "failed" "bundle_build_failed" "convergence_failed_attestation_build" "${BUILD_LOG}" "Full dev bundle build failed."
    return 1
}

run_strict_deferred_build() {
    record_command "FT_ATTESTATION_OUT_DIR=${STRICT_BUILD_DIR} scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned --strict-deferred"
    set +e
    FT_ATTESTATION_OUT_DIR="${STRICT_BUILD_DIR}" \
    FT_BEAD_ID="${BEAD_ID}" \
    FT_SCENARIO_ID="${SCENARIO_ID}" \
    FT_CORRELATION_ID="${CORRELATION_ID}" \
        bash "${ROOT_DIR}/scripts/attestation-build.sh" \
            --version 0.0.0-dev \
            --channel dev \
            --sign unsigned \
            --strict-deferred >"${STRICT_BUILD_LOG}" 2>&1
    STRICT_BUILD_EXIT=$?
    set -e

    if [[ "${DEFERRED_SLOT_COUNT}" -eq 0 && "${STRICT_BUILD_EXIT}" -eq 0 ]]; then
        record_result "convergence.strict_deferred_build" "passed" "strict_deferred_no_deferred_slots" "none" "${STRICT_BUILD_LOG}" "Strict-deferred build passed because no deferred slots remain."
        return 0
    fi
    if [[ "${DEFERRED_SLOT_COUNT}" -gt 0 && "${STRICT_BUILD_EXIT}" -ne 0 ]]; then
        record_result "convergence.strict_deferred_build" "expected_failure" "strict_deferred_rejected_deferred_slots" "none" "${STRICT_BUILD_LOG}" "Strict-deferred build rejected remaining deferred slots as expected."
        return 0
    fi

    record_result "convergence.strict_deferred_build" "failed" "strict_deferred_matrix_mismatch" "convergence_failed_strict_deferred" "${STRICT_BUILD_LOG}" "Strict-deferred exit code did not match current deferred-slot count."
    return 1
}

run_attestation_verify() {
    if [[ ! -f "${BUNDLE_PATH}" ]]; then
        record_result "convergence.attestation_verify" "failed" "bundle_missing" "convergence_failed_attestation_verify" "${VERIFY_LOG}" "Cannot verify because bundle path is missing."
        VERIFY_EXIT=1
        return 1
    fi

    record_command "scripts/attestation-verify.sh ${BUNDLE_PATH}"
    set +e
    bash "${ROOT_DIR}/scripts/attestation-verify.sh" "${BUNDLE_PATH}" >"${VERIFY_LOG}" 2>&1
    VERIFY_EXIT=$?
    set -e

    if [[ "${VERIFY_EXIT}" -eq 0 ]]; then
        record_result "convergence.attestation_verify" "passed" "bundle_verified" "none" "${VERIFY_LOG}" "Built bundle verified successfully."
        return 0
    fi

    record_result "convergence.attestation_verify" "failed" "bundle_verify_failed" "convergence_failed_attestation_verify" "${VERIFY_LOG}" "Built bundle failed verification."
    return 1
}

is_rch_substrate_failure() {
    local log_file="$1"
    local meta_file="${log_file}.rch_meta.json"
    if [[ -f "${meta_file}" ]] \
        && jq -e '.timed_out == true
                  or .fail_open_detected == true
                  or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE"
                  or .failure_reason_code == "RCH-REMOTE-STALL"
                  or .failure_reason_code == "RCH-CARGO-DEP-INFO-MISSING"
                  or .wrapper_exit_code == 124' "${meta_file}" >/dev/null 2>&1
    then
        return 0
    fi
    grep -E "No space left on device|IO failure on output stream|ld terminated with signal 7|Bus error" "${log_file}" >/dev/null 2>&1
}

run_rust_guard() {
    local step="$1"
    local log_file="$2"
    local target_suffix="$3"
    shift 3

    record_command "run_rch_cargo_logged env CARGO_TARGET_DIR=${REMOTE_TARGET_DIR}-${target_suffix} $*"
    set +e
    run_rch_cargo_logged "${log_file}" \
        env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}-${target_suffix}" RUST_TEST_THREADS=1 PROPTEST_CASES=64 \
        "$@"
    local rc=$?
    set -e

    case "${step}" in
        readme_hedge_alignment) HEDGE_EXIT="${rc}" ;;
        attestation_manifest_completeness) MANIFEST_EXIT="${rc}" ;;
        readme_footnotes_resolve) FOOTNOTES_EXIT="${rc}" ;;
    esac

    if [[ "${rc}" -eq 0 ]]; then
        record_result "convergence.${step}" "passed" "${step}_passed" "none" "${log_file}" "Rust guard ${step} passed through RCH."
        return 0
    fi

    if is_rch_substrate_failure "${log_file}"; then
        RCH_SUBSTRATE_BLOCKED=true
        record_result "convergence.${step}" "failed" "rch_substrate_blocked" "convergence_failed_${step}_rch_substrate_blocked" "${log_file}" "RCH substrate blocked before a trustworthy ${step} verdict."
    else
        record_result "convergence.${step}" "failed" "rust_guard_failed" "convergence_failed_${step}" "${log_file}" "Rust guard ${step} failed."
    fi
    return "${rc}"
}

run_memory_envelope_hedge_check() {
    local deferred_beads
    deferred_beads="$(
        jq -r '.slots[]? | select(.path == null and .deferred_to_bead != null) | .deferred_to_bead' \
            "${ROOT_DIR}/docs/attestations/manifest.json" \
            | sort -u
    )"

    grep -nH "memory-envelope claims should be treated as benchmark-dependent until linked artifacts are published" \
        "${ROOT_DIR}/README.md" "${ROOT_DIR}/AGENTS.md" >"${HEDGE_SCAN_FILE}" 2>/dev/null || true
    HEDGE_MATCH_COUNT="$(awk 'NF { count++ } END { print count + 0 }' "${HEDGE_SCAN_FILE}")"

    if [[ "${HEDGE_MATCH_COUNT}" -eq 0 ]]; then
        record_result "convergence.memory_envelope_hedge" "passed" "legacy_hedge_lifted" "none" "${HEDGE_SCAN_FILE}" "Legacy memory-envelope hedge is absent from README.md and AGENTS.md."
        return 0
    fi

    if [[ "${DEFERRED_SLOT_COUNT}" -gt 0 ]]; then
        local bad_count=0
        while IFS= read -r line; do
            [[ -z "${line}" ]] && continue
            if ! grep -Ff <(printf '%s\n' "${deferred_beads}") <<<"${line}" >/dev/null 2>&1; then
                bad_count=$((bad_count + 1))
            fi
        done <"${HEDGE_SCAN_FILE}"
        if [[ "${bad_count}" -eq 0 ]]; then
            record_result "convergence.memory_envelope_hedge" "passed" "legacy_hedge_cites_deferred_bead" "none" "${HEDGE_SCAN_FILE}" "Legacy hedge remains only with deferred-bead citations."
            return 0
        fi
    fi

    record_result "convergence.memory_envelope_hedge" "failed" "legacy_hedge_unjustified" "convergence_failed_memory_envelope_hedge" "${HEDGE_SCAN_FILE}" "Legacy memory-envelope hedge remains without live deferred-slot justification."
    return 1
}

run_closing_checklist_check() {
    local checklist="${ROOT_DIR}/docs/release/attestation-checklist.md"
    : >"${CHECKLIST_SCAN_FILE}"
    for needle in \
        "cargo test -p frankenterm-core --test readme_hedge_alignment" \
        "cargo test -p frankenterm-core --test attestation_manifest_completeness --no-default-features"
    do
        if grep -nF "${needle}" "${checklist}" >>"${CHECKLIST_SCAN_FILE}"; then
            CHECKLIST_REF_COUNT=$((CHECKLIST_REF_COUNT + 1))
        fi
    done

    if [[ "${CHECKLIST_REF_COUNT}" -eq 2 ]]; then
        record_result "convergence.closing_checklist_refs" "passed" "closing_checklist_refs_present" "none" "${CHECKLIST_SCAN_FILE}" "Closing checklist cites the ft-e87u6.4 and ft-e87u6.5 regression tests."
        return 0
    fi

    record_result "convergence.closing_checklist_refs" "failed" "closing_checklist_refs_missing" "convergence_failed_closing_checklist_refs" "${CHECKLIST_SCAN_FILE}" "Closing checklist is missing a required regression-test reference."
    return 1
}

cd "${ROOT_DIR}"

require_cmd jq
require_cmd grep
require_cmd bash
if ! rch_github_actions_local_cargo_enabled; then
    require_cmd rch
fi

run_shell_static
run_manifest_gap_check
run_attestation_build || true
run_strict_deferred_build || true
run_attestation_verify || true
run_memory_envelope_hedge_check || true
run_closing_checklist_check || true

record_command "ensure_rch_ready"
set +e
( ensure_rch_ready )
RCH_READY_RC=$?
set -e
if [[ "${RCH_READY_RC}" -eq 0 ]]; then
    record_result "convergence.rch_preflight" "passed" "rch_ready" "none" "${ARTIFACT_DIR}" "RCH preflight completed."
else
    RCH_SUBSTRATE_BLOCKED=true
    PREFLIGHT_ARTIFACT="$(rch_remote_preflight_log_path)"
    [[ -f "${PREFLIGHT_ARTIFACT}" ]] || PREFLIGHT_ARTIFACT="$(rch_probe_log_path)"
    record_result "convergence.rch_preflight" "failed" "rch_preflight_failed" "convergence_failed_rch_preflight" "${PREFLIGHT_ARTIFACT}" "RCH preflight failed before Rust guards."
fi

if [[ "${RCH_SUBSTRATE_BLOCKED}" != "true" ]]; then
    run_rust_guard "readme_hedge_alignment" "${HEDGE_LOG}" "hedge" \
        cargo test -p frankenterm-core --test readme_hedge_alignment -- --nocapture || true
    run_rust_guard "attestation_manifest_completeness" "${MANIFEST_LOG}" "manifest" \
        cargo test -p frankenterm-core --test attestation_manifest_completeness --no-default-features -- --nocapture || true
    run_rust_guard "readme_footnotes_resolve" "${FOOTNOTES_LOG}" "footnotes" \
        cargo test -p frankenterm-core --test readme_footnotes_resolve -- --nocapture || true
else
    record_result "convergence.readme_hedge_alignment" "skipped" "rch_preflight_blocked" "none" "${HEDGE_LOG}" "Skipped because RCH preflight failed."
    record_result "convergence.attestation_manifest_completeness" "skipped" "rch_preflight_blocked" "none" "${MANIFEST_LOG}" "Skipped because RCH preflight failed."
    record_result "convergence.readme_footnotes_resolve" "skipped" "rch_preflight_blocked" "none" "${FOOTNOTES_LOG}" "Skipped because RCH preflight failed."
fi

if [[ "${RCH_SUBSTRATE_BLOCKED}" == "true" ]]; then
    record_result "convergence.summary" "failed" "rch_substrate_blocked" "convergence_failed_rch_substrate_blocked" "${SUMMARY_FILE}" "Epic convergence blocked by RCH substrate before a trustworthy full verdict."
    echo "BLOCKED ${BEAD_ID} ${SCENARIO_ID}: RCH substrate blocked; artifacts at $(relative_path "${ARTIFACT_DIR}")" >&2
    finalize 1
fi

if [[ "${FAIL}" -eq 0 ]]; then
    record_result "convergence.summary" "passed" "epic_convergence_passed" "none" "${SUMMARY_FILE}" "ft-e87u6 epic convergence guard passed."
    echo "PASS ${BEAD_ID} ${SCENARIO_ID}: artifacts at $(relative_path "${ARTIFACT_DIR}")"
    finalize 0
fi

record_result "convergence.summary" "failed" "epic_convergence_failed" "convergence_failed_summary" "${SUMMARY_FILE}" "ft-e87u6 epic convergence guard failed."
echo "FAIL ${BEAD_ID} ${SCENARIO_ID}: ${FAIL} failed row(s); artifacts at $(relative_path "${ARTIFACT_DIR}")" >&2
finalize 1
