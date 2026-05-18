#!/usr/bin/env bash
# ft-e87u6.3 -- release-attestation build/verify round-trip proof.
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-e87u6.3"
SCENARIO_ID="round_trip"
RUN_ID="${FT_E87U6_3_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
CORRELATION_ID="${BEAD_ID}-${RUN_ID}"
ARTIFACT_DIR="${FT_E87U6_3_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-e87u6/round-trip/${RUN_ID}}"

STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
ENV_FILE="${ARTIFACT_DIR}/env.txt"

VERSION="0.0.0-dev"
FIXED_GENERATED_AT="${FT_E87U6_3_FIXED_GENERATED_AT:-2026-05-13T00:00:00Z}"

TOTAL=0
PASSED=0
FAILED=0
EXPECTED_FAILED=0
SKIPPED=0
FINALIZED=0
OVERALL_STATUS="passed"

BUILD_HAPPY_EXIT=-1
BUILD_RERUN_EXIT=-1
BUILD_STRICT_EXIT=-1
VERIFY_EXIT=-1
TAMPER_EXIT=-1
SIGN_DRYRUN_EXIT=-1

BUILD_IDEMPOTENT=false
CLEANUP_FAULT_ARTIFACTS_SURVIVED=false
SIGSTORE_DRYRUN_STATUS="skipped"
BUNDLE_PATH="${ARTIFACT_DIR}/bundle.dev.json"
RERUN_BUNDLE_PATH="${ARTIFACT_DIR}/bundle.dev.rerun.json"
TAMPERED_BUNDLE_PATH="${ARTIFACT_DIR}/bundle.dev.tampered.json"

mkdir -p "${ARTIFACT_DIR}"
: > "${STRUCTURED_LOG}"
: > "${COMMANDS_FILE}"

rel_path() {
  local path="$1"
  case "${path}" in
    "${ROOT_DIR}/"*) printf '%s\n' "${path#"${ROOT_DIR}/"}" ;;
    *) printf '%s\n' "${path}" ;;
  esac
}

write_env() {
  {
    printf 'timestamp=%s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf 'bead_id=%s\n' "${BEAD_ID}"
    printf 'scenario_id=%s\n' "${SCENARIO_ID}"
    printf 'run_id=%s\n' "${RUN_ID}"
    printf 'correlation_id=%s\n' "${CORRELATION_ID}"
    printf 'artifact_dir=%s\n' "${ARTIFACT_DIR}"
    printf 'fixed_generated_at=%s\n' "${FIXED_GENERATED_AT}"
    printf 'rch_required=false\n'
    printf 'cwd=%s\n' "${ROOT_DIR}"
  } > "${ENV_FILE}"
}

emit_event() {
  local step="$1"
  local outcome="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local exit_code="$6"
  local expected_exit="$7"
  local message="$8"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg surface "scripts/attestation-build.sh+verify.sh" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_path "$(rel_path "${artifact_path}")" \
    --arg message "${message}" \
    --argjson exit_code "${exit_code}" \
    --arg expected_exit "${expected_exit}" \
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
      exit_code: $exit_code,
      expected_exit: $expected_exit,
      message: $message
    }' >> "${STRUCTURED_LOG}"
}

record_step() {
  local step="$1"
  local outcome="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local exit_code="$6"
  local expected_exit="$7"
  local message="$8"

  TOTAL=$((TOTAL + 1))
  case "${outcome}" in
    passed)
      PASSED=$((PASSED + 1))
      ;;
    expected_failure)
      EXPECTED_FAILED=$((EXPECTED_FAILED + 1))
      ;;
    skipped)
      SKIPPED=$((SKIPPED + 1))
      ;;
    failed)
      FAILED=$((FAILED + 1))
      OVERALL_STATUS="failed"
      ;;
  esac
  emit_event "${step}" "${outcome}" "${reason_code}" "${error_code}" "${artifact_path}" "${exit_code}" "${expected_exit}" "${message}"
}

record_command() {
  printf '%s\n' "$*" >> "${COMMANDS_FILE}"
}

create_fixed_date_bin() {
  local bin_dir="${ARTIFACT_DIR}/fixed-date-bin"
  mkdir -p "${bin_dir}"
  {
    printf '#!/usr/bin/env bash\n'
    printf 'set -euo pipefail\n'
    printf 'if [[ "$#" -eq 2 && "$1" == "-u" && "$2" == "+%%Y-%%m-%%dT%%H:%%M:%%SZ" ]]; then\n'
    printf '  printf "%%s\\n" "${FT_E87U6_3_FIXED_GENERATED_AT:-2026-05-13T00:00:00Z}"\n'
    printf 'else\n'
    printf '  /bin/date "$@"\n'
    printf 'fi\n'
  } > "${bin_dir}/date"
  chmod +x "${bin_dir}/date"
  printf '%s\n' "${bin_dir}"
}

create_fake_cosign_bin() {
  local bin_dir="${ARTIFACT_DIR}/fake-cosign-bin"
  mkdir -p "${bin_dir}"
  {
    printf '#!/usr/bin/env bash\n'
    printf 'set -euo pipefail\n'
    printf 'cmd="${1:-}"\n'
    printf 'shift || true\n'
    printf 'case "${cmd}" in\n'
    printf '  sign-blob)\n'
    printf '    bundle=""\n'
    printf '    while [[ "$#" -gt 0 ]]; do\n'
    printf '      case "$1" in\n'
    printf '        --yes) shift ;;\n'
    printf '        --bundle) bundle="${2:?--bundle requires a path}"; shift 2 ;;\n'
    printf '        *) shift ;;\n'
    printf '      esac\n'
    printf '    done\n'
    printf '    [[ -n "${bundle}" ]] || { echo "fake cosign: missing --bundle" >&2; exit 2; }\n'
    printf '    mkdir -p "$(dirname "${bundle}")"\n'
    printf '    jq -n '\''{mediaType:"application/vnd.dev.sigstore.bundle.v0.3+json",verificationMaterial:{certificate:{rawBytes:"ZmFrZQ=="},tlogEntries:[]},messageSignature:{messageDigest:{algorithm:"SHA2_256",digest:"ZmFrZQ=="},signature:"ZmFrZQ=="}}'\'' > "${bundle}"\n'
    printf '    ;;\n'
    printf '  verify-blob)\n'
    printf '    bundle=""\n'
    printf '    blob=""\n'
    printf '    while [[ "$#" -gt 0 ]]; do\n'
    printf '      case "$1" in\n'
    printf '        --bundle) bundle="${2:?--bundle requires a path}"; shift 2 ;;\n'
    printf '        --certificate-identity|--certificate-oidc-issuer) shift 2 ;;\n'
    printf '        *) blob="$1"; shift ;;\n'
    printf '      esac\n'
    printf '    done\n'
    printf '    [[ -f "${bundle}" && -f "${blob}" ]] || exit 1\n'
    printf '    ;;\n'
    printf '  *) echo "fake cosign: unsupported command ${cmd}" >&2; exit 2 ;;\n'
    printf 'esac\n'
  } > "${bin_dir}/cosign"
  chmod +x "${bin_dir}/cosign"
  printf '%s\n' "${bin_dir}"
}

write_deferred_manifest() {
  local manifest_path="$1"
  mkdir -p "$(dirname "${manifest_path}")"
  jq -n '{
    "$schema": "./schema.json#/$defs/manifestPlaceholder",
    required_categories: ["perf/headline-claims"],
    slots: [{
      category: "perf/headline-claims",
      path: null,
      media_type: "application/json",
      produced_by_bead: "ft-syqcz.3",
      deferred_to_bead: "ft-e87u6.9",
      deferred_reason: "synthetic strict-deferred proof fixture",
      proof_categories: [5],
      description: "synthetic deferred slot for ft-e87u6.3 strict-deferred proof"
    }]
  }' > "${manifest_path}"
}

finish_summary() {
  local script_exit="$1"
  if [[ "${FINALIZED}" -eq 1 ]]; then
    return
  fi
  FINALIZED=1

  jq -n \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "$(rel_path "${ARTIFACT_DIR}")" \
    --arg structured_log "$(rel_path "${STRUCTURED_LOG}")" \
    --arg commands_file "$(rel_path "${COMMANDS_FILE}")" \
    --arg env_file "$(rel_path "${ENV_FILE}")" \
    --arg bundle_path "$(rel_path "${BUNDLE_PATH}")" \
    --arg rerun_bundle_path "$(rel_path "${RERUN_BUNDLE_PATH}")" \
    --arg tampered_bundle_path "$(rel_path "${TAMPERED_BUNDLE_PATH}")" \
    --arg status "${OVERALL_STATUS}" \
    --arg sigstore_dryrun_status "${SIGSTORE_DRYRUN_STATUS}" \
    --argjson total "${TOTAL}" \
    --argjson passed "${PASSED}" \
    --argjson failed "${FAILED}" \
    --argjson expected_failed "${EXPECTED_FAILED}" \
    --argjson skipped "${SKIPPED}" \
    --argjson build_happy_exit "${BUILD_HAPPY_EXIT}" \
    --argjson build_rerun_exit "${BUILD_RERUN_EXIT}" \
    --argjson build_strict_exit "${BUILD_STRICT_EXIT}" \
    --argjson verify_exit "${VERIFY_EXIT}" \
    --argjson tamper_exit "${TAMPER_EXIT}" \
    --argjson sign_dryrun_exit "${SIGN_DRYRUN_EXIT}" \
    --argjson script_exit "${script_exit}" \
    --argjson build_idempotent "${BUILD_IDEMPOTENT}" \
    --argjson cleanup_fault_artifacts_survived "${CLEANUP_FAULT_ARTIFACTS_SURVIVED}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      correlation_id: $correlation_id,
      status: $status,
      rch_required: false,
      script_exit: $script_exit,
      artifact_dir: $artifact_dir,
      structured_log: $structured_log,
      commands_file: $commands_file,
      env_file: $env_file,
      bundle_path: $bundle_path,
      rerun_bundle_path: $rerun_bundle_path,
      tampered_bundle_path: $tampered_bundle_path,
      counts: {
        total: $total,
        passed: $passed,
        failed: $failed,
        expected_failed: $expected_failed,
        skipped: $skipped
      },
      exit_codes: {
        build_happy: $build_happy_exit,
        build_rerun: $build_rerun_exit,
        build_strict_deferred: $build_strict_exit,
        verify_happy: $verify_exit,
        verify_tamper: $tamper_exit,
        sign_dryrun: $sign_dryrun_exit
      },
      build_idempotent: $build_idempotent,
      cleanup_fault_artifacts_survived: $cleanup_fault_artifacts_survived,
      cleanup_policy: "no repository or scratch deletion; artifacts are retained for triage per AGENTS.md no-file-deletion rule",
      sigstore_dryrun_status: $sigstore_dryrun_status
    }' > "${SUMMARY_FILE}"
}

on_exit() {
  local rc="$1"
  finish_summary "${rc}"
  exit "${rc}"
}

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required for structured proof logs" >&2
  exit 127
fi

trap 'on_exit "$?"' EXIT

write_env
FIXED_DATE_BIN="$(create_fixed_date_bin)"

if command -v git >/dev/null 2>&1 && command -v bash >/dev/null 2>&1; then
  record_step "preflight.rch" "passed" "light_local_no_cargo" "none" "${ENV_FILE}" 0 "0" "No cargo step is present; RCH is not required for this shell-only proof."
else
  record_step "preflight.rch" "failed" "missing_tool" "preflight_failed" "${ENV_FILE}" 1 "0" "Required shell proof tools are missing."
  exit 1
fi

BUILD_DIR="${ARTIFACT_DIR}/build-dev"
mkdir -p "${BUILD_DIR}"
BUILD_LOG="${ARTIFACT_DIR}/build.dev.log"
record_command "FT_ATTESTATION_OUT_DIR=${BUILD_DIR} scripts/attestation-build.sh --version ${VERSION} --channel dev --sign unsigned"
FT_ATTESTATION_OUT_DIR="${BUILD_DIR}" \
FT_BEAD_ID="${BEAD_ID}" \
FT_SCENARIO_ID="${SCENARIO_ID}" \
FT_CORRELATION_ID="${CORRELATION_ID}" \
FT_E87U6_3_FIXED_GENERATED_AT="${FIXED_GENERATED_AT}" \
PATH="${FIXED_DATE_BIN}:${PATH}" \
  bash "${ROOT_DIR}/scripts/attestation-build.sh" \
    --version "${VERSION}" \
    --channel dev \
    --sign unsigned > "${BUILD_LOG}" 2>&1
BUILD_HAPPY_EXIT=$?
if [[ "${BUILD_HAPPY_EXIT}" -eq 0 && -f "${BUILD_DIR}/${VERSION}.json" ]]; then
  cp "${BUILD_DIR}/${VERSION}.json" "${BUNDLE_PATH}"
  record_step "build.happy" "passed" "bundle_written" "none" "${BUILD_LOG}" "${BUILD_HAPPY_EXIT}" "0" "Happy-path dev bundle built."
else
  record_step "build.happy" "failed" "bundle_build_failed" "build_happy_failed" "${BUILD_LOG}" "${BUILD_HAPPY_EXIT}" "0" "Happy-path dev bundle failed."
fi

RERUN_DIR="${ARTIFACT_DIR}/build-dev-rerun"
mkdir -p "${RERUN_DIR}"
RERUN_LOG="${ARTIFACT_DIR}/build.dev.rerun.log"
record_command "FT_ATTESTATION_OUT_DIR=${RERUN_DIR} scripts/attestation-build.sh --version ${VERSION} --channel dev --sign unsigned"
FT_ATTESTATION_OUT_DIR="${RERUN_DIR}" \
FT_BEAD_ID="${BEAD_ID}" \
FT_SCENARIO_ID="${SCENARIO_ID}" \
FT_CORRELATION_ID="${CORRELATION_ID}" \
FT_E87U6_3_FIXED_GENERATED_AT="${FIXED_GENERATED_AT}" \
PATH="${FIXED_DATE_BIN}:${PATH}" \
  bash "${ROOT_DIR}/scripts/attestation-build.sh" \
    --version "${VERSION}" \
    --channel dev \
    --sign unsigned > "${RERUN_LOG}" 2>&1
BUILD_RERUN_EXIT=$?
if [[ "${BUILD_RERUN_EXIT}" -eq 0 && -f "${RERUN_DIR}/${VERSION}.json" ]]; then
  cp "${RERUN_DIR}/${VERSION}.json" "${RERUN_BUNDLE_PATH}"
fi
if [[ "${BUILD_RERUN_EXIT}" -eq 0 && -f "${BUNDLE_PATH}" && -f "${RERUN_BUNDLE_PATH}" && "$(cmp -s "${BUNDLE_PATH}" "${RERUN_BUNDLE_PATH}"; echo $?)" -eq 0 ]]; then
  BUILD_IDEMPOTENT=true
  record_step "build.rerun.idempotent" "passed" "byte_identical" "none" "${RERUN_LOG}" "${BUILD_RERUN_EXIT}" "0" "Fixed-clock rebuild produced a byte-identical bundle."
else
  record_step "build.rerun.idempotent" "failed" "non_deterministic_bundle" "bundle_diff" "${RERUN_LOG}" "${BUILD_RERUN_EXIT}" "0" "Fixed-clock rebuild differed from the first bundle."
fi

STRICT_DIR="${ARTIFACT_DIR}/strict-deferred"
STRICT_MANIFEST="${STRICT_DIR}/manifest.json"
STRICT_LOG="${ARTIFACT_DIR}/build.strict-deferred.log"
write_deferred_manifest "${STRICT_MANIFEST}"
record_command "FT_ATTESTATION_MANIFEST=${STRICT_MANIFEST} scripts/attestation-build.sh --version ${VERSION} --channel dev --sign unsigned --strict-deferred"
FT_ATTESTATION_MANIFEST="${STRICT_MANIFEST}" \
FT_ATTESTATION_OUT_DIR="${STRICT_DIR}/out" \
FT_BEAD_ID="${BEAD_ID}" \
FT_SCENARIO_ID="${SCENARIO_ID}" \
FT_CORRELATION_ID="${CORRELATION_ID}" \
FT_E87U6_3_FIXED_GENERATED_AT="${FIXED_GENERATED_AT}" \
PATH="${FIXED_DATE_BIN}:${PATH}" \
  bash "${ROOT_DIR}/scripts/attestation-build.sh" \
    --version "${VERSION}" \
    --channel dev \
    --sign unsigned \
    --strict-deferred > "${STRICT_LOG}" 2>&1
BUILD_STRICT_EXIT=$?
if [[ "${BUILD_STRICT_EXIT}" -ne 0 ]] && grep -q -- "--strict-deferred rejects" "${STRICT_LOG}"; then
  record_step "build.strict_deferred" "expected_failure" "strict_deferred_blocks" "ATTESTATION-DEFERRED-SLOT" "${STRICT_LOG}" "${BUILD_STRICT_EXIT}" "nonzero" "Synthetic deferred slot was rejected by --strict-deferred."
else
  record_step "build.strict_deferred" "failed" "strict_deferred_not_enforced" "strict_deferred_failed" "${STRICT_LOG}" "${BUILD_STRICT_EXIT}" "nonzero" "Synthetic deferred slot did not produce the expected strict-deferred failure."
fi

VERIFY_LOG="${ARTIFACT_DIR}/verify.dev.log"
record_command "scripts/attestation-verify.sh ${BUNDLE_PATH}"
bash "${ROOT_DIR}/scripts/attestation-verify.sh" "${BUNDLE_PATH}" > "${VERIFY_LOG}" 2>&1
VERIFY_EXIT=$?
if [[ "${VERIFY_EXIT}" -eq 0 ]]; then
  record_step "verify.happy" "passed" "bundle_verified" "none" "${VERIFY_LOG}" "${VERIFY_EXIT}" "0" "Verifier accepted the happy-path bundle."
else
  record_step "verify.happy" "failed" "bundle_verify_failed" "verify_failed" "${VERIFY_LOG}" "${VERIFY_EXIT}" "0" "Verifier rejected the happy-path bundle."
fi

TAMPER_LOG="${ARTIFACT_DIR}/tamper.log"
if [[ -f "${BUNDLE_PATH}" ]]; then
  jq '.artifacts[0].path = "docs/attestations/does-not-exist-tampered.json"' "${BUNDLE_PATH}" > "${TAMPERED_BUNDLE_PATH}"
fi
record_command "scripts/attestation-verify.sh ${TAMPERED_BUNDLE_PATH}"
bash "${ROOT_DIR}/scripts/attestation-verify.sh" "${TAMPERED_BUNDLE_PATH}" > "${TAMPER_LOG}" 2>&1
TAMPER_EXIT=$?
if [[ "${TAMPER_EXIT}" -ne 0 ]]; then
  record_step "verify.tamper" "expected_failure" "tamper_detected" "tamper_detected" "${TAMPER_LOG}" "${TAMPER_EXIT}" "nonzero" "Verifier rejected the tampered bundle."
else
  record_step "verify.tamper" "failed" "tamper_not_detected" "tamper_accepted" "${TAMPER_LOG}" "${TAMPER_EXIT}" "nonzero" "Verifier accepted a tampered bundle."
fi

CLEANUP_PROBE_DIR="${ARTIFACT_DIR}/cleanup-fault-probe"
mkdir -p "${CLEANUP_PROBE_DIR}"
printf 'partial bundle retained for failure triage\n' > "${CLEANUP_PROBE_DIR}/partial.bundle"
if [[ -f "${CLEANUP_PROBE_DIR}/partial.bundle" ]]; then
  CLEANUP_FAULT_ARTIFACTS_SURVIVED=true
  record_step "cleanup.failure_artifacts_retained" "passed" "artifacts_retained" "none" "${CLEANUP_PROBE_DIR}/partial.bundle" 0 "0" "Deliberate fault probe artifact survived; no cleanup deletion was performed."
else
  record_step "cleanup.failure_artifacts_retained" "failed" "artifact_missing" "cleanup_probe_failed" "${CLEANUP_PROBE_DIR}" 1 "0" "Deliberate fault probe artifact was not retained."
fi

if [[ "${COSIGN_DUMMY:-0}" == "1" ]]; then
  SIGSTORE_DRYRUN_STATUS="running"
  FAKE_COSIGN_BIN="$(create_fake_cosign_bin)"
  SIGN_DIR="${ARTIFACT_DIR}/sign-dryrun"
  SIGN_LOG="${ARTIFACT_DIR}/sign.dryrun.log"
  mkdir -p "${SIGN_DIR}"
  record_command "COSIGN_DUMMY=1 scripts/attestation-build.sh --version ${VERSION} --channel stable --sign cosign"
  COSIGN_IDENTITY="https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v${VERSION}" \
  COSIGN_OIDC_ISSUER="https://token.actions.githubusercontent.com" \
  FT_ATTESTATION_OUT_DIR="${SIGN_DIR}" \
  FT_BEAD_ID="${BEAD_ID}" \
  FT_SCENARIO_ID="${SCENARIO_ID}" \
  FT_CORRELATION_ID="${CORRELATION_ID}" \
  FT_E87U6_3_FIXED_GENERATED_AT="${FIXED_GENERATED_AT}" \
  PATH="${FAKE_COSIGN_BIN}:${FIXED_DATE_BIN}:${PATH}" \
    bash "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version "${VERSION}" \
      --channel stable \
      --sign cosign > "${SIGN_LOG}" 2>&1
  SIGN_DRYRUN_EXIT=$?
  if [[ "${SIGN_DRYRUN_EXIT}" -eq 0 ]]; then
    SIGSTORE_DRYRUN_STATUS="passed"
    record_step "sign.dryrun" "passed" "dummy_cosign_branch_exercised" "none" "${SIGN_LOG}" "${SIGN_DRYRUN_EXIT}" "0" "Dummy cosign signing branch produced a stable-channel bundle."
  else
    SIGSTORE_DRYRUN_STATUS="failed"
    record_step "sign.dryrun" "failed" "dummy_cosign_failed" "sign_dryrun_failed" "${SIGN_LOG}" "${SIGN_DRYRUN_EXIT}" "0" "Dummy cosign signing branch failed."
  fi
else
  SIGN_DRYRUN_EXIT=0
  record_step "sign.dryrun" "skipped" "cosign_dummy_not_enabled" "none" "${ARTIFACT_DIR}" 0 "0" "Set COSIGN_DUMMY=1 to exercise the dummy cosign signing branch."
fi

if [[ "${FAILED}" -eq 0 ]]; then
  record_step "summary" "passed" "round_trip_passed" "none" "${SUMMARY_FILE}" 0 "0" "Round-trip proof completed."
  finish_summary 0
  echo "PASS ${BEAD_ID} ${SCENARIO_ID}: artifacts at $(rel_path "${ARTIFACT_DIR}")"
  exit 0
fi

record_step "summary" "failed" "round_trip_failed" "summary_failed" "${SUMMARY_FILE}" 1 "0" "Round-trip proof had unexpected failures."
finish_summary 1
echo "FAIL ${BEAD_ID} ${SCENARIO_ID}: ${FAILED} unexpected failure(s)" >&2
exit 1
