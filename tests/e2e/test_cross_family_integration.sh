#!/usr/bin/env bash
# E2E: validate the cross-family contract matrix and retained attestation slot.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_DIR="${ROOT_DIR}/target/test-logs/cross-family/${RUN_ID}"
SUMMARY_JSON="${ARTIFACT_DIR}/summary.json"
DOC_PATH="${ROOT_DIR}/docs/contract-families-cross-invariants.md"
ATTESTATION_PATH="${ROOT_DIR}/docs/attestations/contracts/cross-family-matrix.json"
TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-tf6g3-46-cross-family-${RUN_ID}}"

mkdir -p "${ARTIFACT_DIR}"

cd "${ROOT_DIR}"

doc_hash="$(shasum -a 256 "${DOC_PATH}" | awk '{print $1}')"
jq -e --arg doc_hash "${doc_hash}" '
  .bead_id == "ft-tf6g3.46"
  and .matrix.tuple_count == 7776
  and .matrix.pass_rate == 1
  and .invariants_sha256 == $doc_hash
' "${ATTESTATION_PATH}" >/dev/null

if command -v rch >/dev/null 2>&1; then
  rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" \
    cargo test -p frankenterm-core \
      --test contract_families_invariant_parser \
      --test contract_families_integration_matrix \
      -- --nocapture
else
  CARGO_TARGET_DIR="${TARGET_DIR}" cargo test -p frankenterm-core \
    --test contract_families_invariant_parser \
    --test contract_families_integration_matrix \
    -- --nocapture
fi

jq -cn \
  --arg run_id "${RUN_ID}" \
  --arg artifact_dir "${ARTIFACT_DIR}" \
  --arg doc_hash "${doc_hash}" \
  '{
    run_id: $run_id,
    status: "passed",
    artifact_dir: $artifact_dir,
    bead_id: "ft-tf6g3.46",
    tuple_count: 7776,
    invariant_count: 7,
    invariants_sha256: $doc_hash
  }' > "${SUMMARY_JSON}"

echo "cross-family integration summary: ${SUMMARY_JSON}"
