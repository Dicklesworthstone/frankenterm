#!/usr/bin/env bash
# E2E: ft-e87u6.7 README attestation cross-link regression wrapper.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-e87u6.7"
SCENARIO_ID="readme_cross_links"
RUN_ID="${FT_E87U6_7_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${FT_E87U6_7_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-e87u6/readme-cross-links/${RUN_ID}}"
REMOTE_TARGET_DIR="${FT_E87U6_7_CARGO_TARGET_DIR:-/tmp/ft-e87u6-7-readme-cross-links-${RUN_ID}}"

COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
STDOUT_FILE="${ARTIFACT_DIR}/stdout.txt"
STDERR_FILE="${ARTIFACT_DIR}/stderr.txt"
SHELL_LOG="${ARTIFACT_DIR}/shell-static.log"
CARGO_LOG="${ARTIFACT_DIR}/readme-cross-links-rch.log"
PROOF_LEDGER_FILE="${ARTIFACT_DIR}/proof-ledger.jsonl"

mkdir -p "${ARTIFACT_DIR}"
: >"${COMMANDS_FILE}"
: >"${STRUCTURED_LOG}"
: >"${PROOF_LEDGER_FILE}"

exec > >(tee -a "${STDOUT_FILE}")
exec 2> >(tee -a "${STDERR_FILE}" >&2)

export RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-1}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-1}"
export RCH_CHECK_SLOTS="${RCH_CHECK_SLOTS:-1}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="${BEAD_ID}"
export RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}"
if [[ -n "${RCH_MIRROR_REQUIRED_PATHS:-}" ]]; then
    export RCH_MIRROR_REQUIRED_PATHS="${RCH_MIRROR_REQUIRED_PATHS}:README.md:docs/attestations/manifest.json:.beads/issues.jsonl:crates/frankenterm-core/tests/readme_footnotes_resolve.rs"
else
    export RCH_MIRROR_REQUIRED_PATHS="README.md:docs/attestations/manifest.json:.beads/issues.jsonl:crates/frankenterm-core/tests/readme_footnotes_resolve.rs"
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e87u6_7_readme_cross_links" "${ROOT_DIR}"

PASS=0
FAIL=0
TOTAL=0
FOOTNOTE_COUNT=0
TABLE_ROW_COUNT=0
RCH_SUBSTRATE_BLOCKED=false
RUST_CROSS_LINK_VERDICT=false

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
        --arg surface "README.md attestation footnotes + Trust & Attestation map" \
        --arg step "${step}" \
        --arg outcome "${outcome}" \
        --arg reason_code "${reason_code}" \
        --arg error_code "${error_code}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_path "${artifact_path#"${ROOT_DIR}/"}" \
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
    local ok="$2"
    local reason_code="$3"
    local error_code="$4"
    local artifact_path="$5"
    local message="$6"
    local detail="${7:-}"

    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        emit_event "${step}" "passed" "${reason_code}" "${error_code}" "${artifact_path}" "${message}" "${detail}"
    else
        FAIL=$((FAIL + 1))
        emit_event "${step}" "failed" "${reason_code}" "${error_code}" "${artifact_path}" "${message}" "${detail}"
    fi
}

write_summary() {
    local selected_workers remote_cargo_reached remote_rustc_reached test_binary_reached
    selected_workers="$(jq -scr '[.[].runs[]?.selected_worker_id | select(. != null and . != "required;")] | unique' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf '[]')"
    remote_cargo_reached="$(jq -sr 'any(.[].runs[]?; .remote_cargo_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    remote_rustc_reached="$(jq -sr 'any(.[].runs[]?; .remote_rustc_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"
    test_binary_reached="$(jq -sr 'any(.[].runs[]?; .test_binary_reached == true)' "${PROOF_LEDGER_FILE}" 2>/dev/null || printf 'false')"

    jq -cn \
        --arg bead_id "${BEAD_ID}" \
        --arg scenario_id "${SCENARIO_ID}" \
        --arg run_id "${RUN_ID}" \
        --arg correlation_id "${CORRELATION_ID}" \
        --arg artifact_dir "${ARTIFACT_DIR#"${ROOT_DIR}/"}" \
        --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
        --argjson pass_count "${PASS}" \
        --argjson fail_count "${FAIL}" \
        --argjson total_count "${TOTAL}" \
        --argjson footnote_count "${FOOTNOTE_COUNT}" \
        --argjson table_row_count "${TABLE_ROW_COUNT}" \
        --argjson rch_substrate_blocked "${RCH_SUBSTRATE_BLOCKED}" \
        --argjson rust_cross_link_verdict "${RUST_CROSS_LINK_VERDICT}" \
        --argjson selected_workers "${selected_workers}" \
        --argjson remote_cargo_reached "${remote_cargo_reached}" \
        --argjson remote_rustc_reached "${remote_rustc_reached}" \
        --argjson test_binary_reached "${test_binary_reached}" \
        '($rch_substrate_blocked or (($rust_cross_link_verdict | not) and ($remote_cargo_reached | not) and ($test_binary_reached | not))) as $effective_rch_substrate_blocked
        | {
          bead_id: $bead_id,
          scenario_id: $scenario_id,
          run_id: $run_id,
          correlation_id: $correlation_id,
          status: (if $effective_rch_substrate_blocked then "rch_substrate_blocked" elif $fail_count == 0 then "passed" else "failed" end),
          artifact_dir: $artifact_dir,
          remote_cargo_target_dir: $remote_target_dir,
          counts: {
            total: $total_count,
            passed: $pass_count,
            failed: $fail_count,
            footnote_refs: $footnote_count,
            trust_attestation_rows: $table_row_count
          },
          rch_substrate_blocked: $effective_rch_substrate_blocked,
          rust_cross_link_verdict: $rust_cross_link_verdict,
          remote: {
            selected_workers: $selected_workers,
            remote_cargo_reached: $remote_cargo_reached,
            remote_rustc_reached: $remote_rustc_reached,
            test_binary_reached: $test_binary_reached
          },
          artifacts: {
            commands: "commands.txt",
            structured_log: "structured.log",
            stdout: "stdout.txt",
            stderr: "stderr.txt",
            shell_static: "shell-static.log",
            cargo_log: "readme-cross-links-rch.log",
            proof_ledger: "proof-ledger.jsonl"
          }
        }' >"${SUMMARY_FILE}"
}

trap write_summary EXIT

run_shell_static() {
    record_command "bash -n ${BASH_SOURCE[0]}"
    if bash -n "${BASH_SOURCE[0]}" >"${SHELL_LOG}" 2>&1; then
        record_result "shell.static" "true" "shell_static_passed" "none" "${SHELL_LOG}" "Harness shell syntax passed." "harness"
    else
        record_result "shell.static" "false" "shell_static_failed" "readme_cross_link_harness_invalid" "${SHELL_LOG}" "Harness shell syntax failed." "harness"
        return 1
    fi
}

record_cross_link_details() {
    local anchors rows row_index row
    anchors="$(
        awk '
            /^### Why Use ft\?$/ { in_section = 1; next }
            in_section && /^---$/ { exit }
            in_section { print }
        ' "${ROOT_DIR}/README.md" \
            | grep -o '\[\^ft-attest-[^]]*\]' \
            | sed 's/^\[\^//; s/\]$//' \
            | sort -u || true
    )"

    if [[ -z "${anchors}" ]]; then
        record_result "footnote.resolve.none" "false" "no_why_use_footnotes" "readme_cross_link_missing_footnotes" "${ROOT_DIR}/README.md" "Why Use ft? has no attestation footnote refs."
        return 1
    fi

    while IFS= read -r anchor; do
        [[ -z "${anchor}" ]] && continue
        FOOTNOTE_COUNT=$((FOOTNOTE_COUNT + 1))
        record_result "footnote.resolve.${anchor}" "true" "footnote_resolved" "none" "${ROOT_DIR}/README.md" "Why Use ft? footnote resolves to a populated manifest slot." "${anchor}"
    done <<<"${anchors}"

    rows="$(
        awk '
            /<!-- attestation-claim-map:start -->/ { in_table = 1; next }
            /<!-- attestation-claim-map:end -->/ { exit }
            in_table && /^\|/ && $0 !~ /README claim/ && $0 !~ /^\|[[:space:]\-:|]+\|?$/ { print }
        ' "${ROOT_DIR}/README.md"
    )"

    if [[ -z "${rows}" ]]; then
        record_result "trust_attestation_table.row.none" "false" "claim_map_missing" "readme_cross_link_missing_table" "${ROOT_DIR}/README.md" "Trust & Attestation claim-map table has no rows."
        return 1
    fi

    if [[ ! -f "${ROOT_DIR}/.beads/issues.jsonl" ]]; then
        record_result "trust_attestation_table.beads_db" "false" "beads_db_missing" "readme_cross_link_missing_beads_db" "${ROOT_DIR}/.beads/issues.jsonl" "Local Beads DB is required for producing-bead validation."
        return 1
    fi

    row_index=0
    while IFS= read -r row; do
        [[ -z "${row}" ]] && continue
        row_index=$((row_index + 1))
        TABLE_ROW_COUNT=$((TABLE_ROW_COUNT + 1))
        local bead_id
        bead_id="$(grep -Eo 'ft-[a-z0-9]+(\.[0-9]+)*' <<<"${row}" | head -n 1 || true)"
        if [[ -z "${bead_id}" ]]; then
            record_result "trust_attestation_table.row.${row_index}" "false" "claim_map_row_missing_bead" "readme_cross_link_missing_bead" "${ROOT_DIR}/README.md" "Trust & Attestation row has no producing bead." "${row}"
            continue
        fi
        if ! jq -e --arg bead_id "${bead_id}" 'select(.id == $bead_id)' "${ROOT_DIR}/.beads/issues.jsonl" >/dev/null; then
            record_result "trust_attestation_table.row.${row_index}" "false" "claim_map_bead_not_found" "readme_cross_link_unknown_bead" "${ROOT_DIR}/.beads/issues.jsonl" "Trust & Attestation row cites a bead absent from .beads/issues.jsonl." "${row}"
            continue
        fi
        record_result "trust_attestation_table.row.${row_index}" "true" "claim_map_row_resolved" "none" "${ROOT_DIR}/README.md" "Trust & Attestation row resolves to manifest slot and producing bead." "${row}"
    done <<<"${rows}"
}

run_rust_cross_link_test() {
    record_command "run_rch_cargo_logged env CARGO_TARGET_DIR=${REMOTE_TARGET_DIR} cargo test --manifest-path crates/frankenterm-core/Cargo.toml --test readme_footnotes_resolve -- --nocapture"
    set +e
    run_rch_cargo_logged "${CARGO_LOG}" \
        env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" RUST_TEST_THREADS=1 \
        cargo test --manifest-path crates/frankenterm-core/Cargo.toml --test readme_footnotes_resolve -- --nocapture
    local rc=$?
    set -e

    if [[ "${rc}" -eq 0 ]]; then
        if record_cross_link_details; then
            RUST_CROSS_LINK_VERDICT=true
            return 0
        fi
        return 1
    fi

    if [[ -f "${CARGO_LOG}.rch_meta.json" ]] \
        && jq -e '.timed_out == true or .failure_reason_code == "RCH-REMOTE-MIRROR-MISSING-FILE" or .failure_reason_code == "RCH-REMOTE-STALL" or .wrapper_exit_code == 124' "${CARGO_LOG}.rch_meta.json" >/dev/null 2>&1
    then
        RCH_SUBSTRATE_BLOCKED=true
        record_result "rust.readme_cross_links" "false" "rch_substrate_blocked" "readme_cross_link_rch_substrate_blocked" "${CARGO_LOG}" "RCH substrate blocked before a trustworthy README cross-link verdict."
    else
        record_result "rust.readme_cross_links" "false" "rust_test_failed" "readme_cross_link_guard_failed" "${CARGO_LOG}" "Rust README cross-link test failed."
    fi
    return "${rc}"
}

run_shell_static
ensure_rch_ready
run_rust_cross_link_test || true

if [[ "${FAIL}" -eq 0 ]]; then
    record_result "summary" "true" "readme_cross_links_passed" "none" "${SUMMARY_FILE}" "README attestation cross-links passed."
    echo "PASS ${BEAD_ID} ${SCENARIO_ID}: artifacts at ${ARTIFACT_DIR#"${ROOT_DIR}/"}"
    exit 0
fi

record_result "summary" "false" "readme_cross_links_failed" "readme_cross_link_summary_failed" "${SUMMARY_FILE}" "README attestation cross-links failed."
echo "FAIL ${BEAD_ID} ${SCENARIO_ID}: ${FAIL} failed row(s)" >&2
exit 1
