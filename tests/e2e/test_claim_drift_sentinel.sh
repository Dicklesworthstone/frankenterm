#!/usr/bin/env bash
# ft-e87u6.16: static claim-drift sentinel contract.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SUT="${ROOT_DIR}/scripts/check-claim-drift-sentinel.sh"
REGISTRY="${ROOT_DIR}/docs/attestations/claim-registry.json"
SCHEMA="${ROOT_DIR}/docs/json-schema/ft-claim-drift-registry.json"
FIXTURES="${ROOT_DIR}/fixtures/claim-drift-sentinel/golden-cases.v1.json"

command -v jq >/dev/null 2>&1 || { echo "jq required" >&2; exit 2; }
command -v ruby >/dev/null 2>&1 || { echo "ruby required" >&2; exit 2; }

[[ -x "${SUT}" ]] || { echo "${SUT} not executable" >&2; exit 2; }

jq empty "${REGISTRY}" "${SCHEMA}" "${FIXTURES}"
bash -n "${SUT}"

json_output="$("${SUT}" --json --strict)"

claim_count="$(printf '%s' "${json_output}" | jq -r '.summary.claim_count')"
fixture_count="$(printf '%s' "${json_output}" | jq -r '.summary.fixture_case_count')"
ok="$(printf '%s' "${json_output}" | jq -r '.ok')"

[[ "${ok}" == "true" ]] || {
  printf '%s\n' "${json_output}" >&2
  exit 1
}
[[ "${claim_count}" -ge 7 ]] || {
  echo "expected at least 7 live registry claims, got ${claim_count}" >&2
  exit 1
}
[[ "${fixture_count}" -eq 6 ]] || {
  echo "expected 6 golden fixture cases, got ${fixture_count}" >&2
  exit 1
}

for claim_id in \
  readme.e2e_scripts_count \
  readme.robot_contracts_attestation \
  agents.resource_cockpit_target_class_skipped \
  robot_contracts.attention_router_planned \
  attestation.manifest_agents_md_counts_slot \
  release.checklist_head_sourced_counts \
  readme.high_scale_target_class_held_back
do
  printf '%s' "${json_output}" | jq -e --arg claim_id "${claim_id}" \
    '.checks[] | select(.claim_id == $claim_id and .status == "pass")' >/dev/null
done

for case_id in \
  fresh-head-count \
  stale-count \
  dirty-worktree-release-count \
  missing-attestation-path \
  planned-only-advertised-supported \
  unsupported-command-advertised-supported
do
  printf '%s' "${json_output}" | jq -e --arg case_id "${case_id}" \
    '.checks[] | select(.claim_id == $case_id and .name == "fixtures.expected_verdict" and .status == "pass")' >/dev/null
done

printf '%s' "${json_output}" | jq -e '
  .checks[] |
  select(.claim_id == "stale-count" and .actual.reason_codes[] == "fixture.count_drift")
' >/dev/null
printf '%s' "${json_output}" | jq -e '
  .checks[] |
  select(.claim_id == "dirty-worktree-release-count" and .actual.reason_codes[] == "fixture.release_source_not_head")
' >/dev/null
printf '%s' "${json_output}" | jq -e '
  .checks[] |
  select(.claim_id == "missing-attestation-path" and .actual.reason_codes[] == "fixture.artifact_path_null")
' >/dev/null
printf '%s' "${json_output}" | jq -e '
  .checks[] |
  select(.claim_id == "planned-only-advertised-supported" and .actual.reason_codes[] == "fixture.planned_only_advertised_supported")
' >/dev/null
printf '%s' "${json_output}" | jq -e '
  .checks[] |
  select(.claim_id == "unsupported-command-advertised-supported" and .actual.reason_codes[] == "fixture.unsupported_advertised_supported")
' >/dev/null

echo "claim-drift sentinel: static verifier passed (${claim_count} claims, ${fixture_count} fixtures)"
