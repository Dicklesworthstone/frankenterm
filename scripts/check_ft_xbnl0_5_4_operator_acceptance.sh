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
  "tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193546Z/summary.json"
  "tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/summary.json"
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
  (.exact_verification_commands | index("bash tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh")) != null
' "${CONTRACT_PATH}" >/dev/null

for required_snippet in \
  "OA-01 Clean Bootstrap" \
  "OA-02 Broken Environment Diagnosis" \
  "OA-03 Incident Triage Entry" \
  "OA-04 Return To Steady State" \
  "OA-05 Operator Story Cross-Checks" \
  "CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ CARGO_TARGET_DIR=/tmp/ft-cod2-target rch exec -- cargo check -p frankenterm" \
  "docs/ft-xbnl0-5-7-completion-evidence.md" \
  "docs/ft-xbnl0-4-6-completion-evidence.md"
do
  rg -F "${required_snippet}" "${PLAYBOOK_PATH}" >/dev/null
done

jq -e '
  .status == "passed" and
  .pass_count >= 4 and
  .fail_count == 0
' "${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193546Z/summary.json" >/dev/null

jq -e '
  .bead_id == "ft-xbnl0.4.6" and
  (.artifacts.release_gate_repo_eval_json | endswith("release_gate_repo_eval.json"))
' "${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/summary.json" >/dev/null

REPORT="$(jq -cn \
  --arg checked_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg contract_path "${CONTRACT_PATH}" \
  --arg playbook_path "${PLAYBOOK_PATH}" \
  --arg tuning_summary "${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193546Z/summary.json" \
  --arg release_gate_summary "${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/summary.json" \
  '{
    checked_at: $checked_at,
    status: "passed",
    contract_path: $contract_path,
    playbook_path: $playbook_path,
    evidence: {
      blessed_tuning_summary: $tuning_summary,
      release_gate_summary: $release_gate_summary
    }
  }')"

if [[ -n "${OUTPUT_PATH}" ]]; then
  printf '%s\n' "${REPORT}" > "${OUTPUT_PATH}"
fi

printf '%s\n' "${REPORT}"
