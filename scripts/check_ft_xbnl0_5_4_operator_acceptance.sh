#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT_PATH="${ROOT_DIR}/docs/ft-xbnl0-5-4-operator-acceptance-scenarios.json"
PLAYBOOK_PATH="${ROOT_DIR}/docs/ft-xbnl0-5-4-operator-acceptance-scenarios.md"
OUTPUT_PATH=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      OUTPUT_PATH="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required" >&2
  exit 2
fi

test -f "${CONTRACT_PATH}"
test -f "${PLAYBOOK_PATH}"

required_paths=(
  "docs/operator-playbook.md"
  "docs/ft-xbnl0-verification-contract.md"
  "docs/ft-xbnl0-5-7-completion-evidence.md"
  "docs/ft-xbnl0-5-3-blessed-tuning-playbook.md"
  "docs/ft-xbnl0-5-3-completion-evidence.md"
  "docs/ft-xbnl0-4-6-completion-evidence.md"
  "scripts/check_ft_xbnl0_5_4_operator_acceptance.sh"
  "tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh"
)

for rel_path in "${required_paths[@]}"; do
  test -f "${ROOT_DIR}/${rel_path}"
done

jq -e '
  .bead_id == "ft-xbnl0.5.4" and
  (.scenario_groups | length == 5) and
  (.scenario_groups | map(.id) == ["OA-01", "OA-02", "OA-03", "OA-04", "OA-05"]) and
  (.scenario_groups | any(.slug == "clean_bootstrap" and .evidence_mode == "deterministic_harness")) and
  (.scenario_groups | any(.slug == "broken_environment_diagnosis" and .surface == "doctor")) and
  (.scenario_groups | any(.slug == "incident_triage_entry" and (.commands | index("ft session doctor -f json")))) and
  (.scenario_groups | any(.slug == "return_to_steady_state")) and
  (.scenario_groups | any(.slug == "operator_story_cross_checks" and .evidence_mode == "borrowed_evidence")) and
  (.exact_verification_commands == [
    "bash scripts/check_ft_xbnl0_5_4_operator_acceptance.sh --output docs/ft-xbnl0-5-4-operator-acceptance-validation.json",
    "bash tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh"
  ])
' "${CONTRACT_PATH}" >/dev/null

for required_snippet in \
  "OA-01 Clean Bootstrap" \
  "OA-02 Broken Environment Diagnosis" \
  "OA-03 Incident Triage Entry" \
  "OA-04 Return To Steady State" \
  "OA-05 Operator Story Cross-Checks" \
  "The E2E harness owns the remote \`cargo build -p frankenterm\` proof step via the" \
  "docs/ft-xbnl0-5-7-completion-evidence.md" \
  "docs/ft-xbnl0-4-6-completion-evidence.md"
do
  rg -F "${required_snippet}" "${PLAYBOOK_PATH}" >/dev/null
done

for evidence_snippet in \
  "The contract verifier passed and wrote \`docs/ft-xbnl0-5-3-blessed-tuning-validation.json\`." \
  "fleet_200_plus" \
  "Harness summary status passed: the harness itself completed its source audit" \
  "Harness remote lib-test lane passed on worker" \
  "Repo evaluator status is intentionally \`failed\` today"
do
  rg -F "${evidence_snippet}" \
    "${ROOT_DIR}/docs/ft-xbnl0-5-3-completion-evidence.md" \
    "${ROOT_DIR}/docs/ft-xbnl0-4-6-completion-evidence.md" >/dev/null
done

REPORT="$(jq -cn \
  --arg checked_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg contract_path "${CONTRACT_PATH}" \
  --arg playbook_path "${PLAYBOOK_PATH}" \
  --arg tuning_evidence_doc "${ROOT_DIR}/docs/ft-xbnl0-5-3-completion-evidence.md" \
  --arg release_gate_evidence_doc "${ROOT_DIR}/docs/ft-xbnl0-4-6-completion-evidence.md" \
  '{
    checked_at: $checked_at,
    status: "passed",
    contract_path: $contract_path,
    playbook_path: $playbook_path,
    evidence: {
      blessed_tuning_evidence_doc: $tuning_evidence_doc,
      release_gate_evidence_doc: $release_gate_evidence_doc
    }
  }')"

if [[ -n "${OUTPUT_PATH}" ]]; then
  printf '%s\n' "${REPORT}" > "${OUTPUT_PATH}"
fi

printf '%s\n' "${REPORT}"
