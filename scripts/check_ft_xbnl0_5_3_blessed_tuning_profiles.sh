#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT_PATH="${ROOT_DIR}/docs/ft-xbnl0-5-3-blessed-tuning-profiles.json"
PLAYBOOK_PATH="${ROOT_DIR}/docs/ft-xbnl0-5-3-blessed-tuning-playbook.md"
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
  "docs/tuning-reference.md"
  "docs/operator-playbook.md"
  "docs/ft-xbnl0-verification-contract.md"
  "docs/ft-xbnl0-4-6-release-gates.json"
  "docs/ft-xbnl0-4-6-release-gates-validation.json"
  "tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json"
  "fixtures/e2e/blessed_tuning_profiles/fleet_10.toml"
  "fixtures/e2e/blessed_tuning_profiles/fleet_50.toml"
  "fixtures/e2e/blessed_tuning_profiles/fleet_200_plus.toml"
  "scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh"
  "tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh"
)

for rel_path in "${required_paths[@]}"; do
  test -f "${ROOT_DIR}/${rel_path}"
done

jq -e '
  .global_thresholds.release_gate_max_duration_s == 3 and
  .global_thresholds.release_gate_max_peak_rss_mb == 32 and
  .global_thresholds.required_backpressure_tier == "Black" and
  .global_thresholds.required_pane_scales == [1, 50, 100, 200] and
  (.profiles | length == 3)
' "${CONTRACT_PATH}" >/dev/null

jq -e '
  .status == "failed" and
  (.checks | any(.gate_id == "REL-03-soak-confidence" and .status == "passed")) and
  (.checks | any(.gate_id == "REL-04-performance-budget" and .status == "failed")) and
  (.summary.latest_soak_wrapper_summary | endswith("/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json"))
' "${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates-validation.json" >/dev/null

jq -e '
  .status == "passed" and
  .profiles.smoke.cycles == 1 and
  .profiles.release.cycles == 3
' "${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json" >/dev/null

for required_snippet in \
  "docs/ft-xbnl0-4-6-release-gates-validation.json" \
  "ft config profile apply fleet_50 --path ./ft.toml" \
  "ft doctor --json" \
  "tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh"
do
  rg -F "${required_snippet}" "${PLAYBOOK_PATH}" >/dev/null
done

REPORT="$(jq -cn \
  --arg checked_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg contract_path "${CONTRACT_PATH}" \
  --arg playbook_path "${PLAYBOOK_PATH}" \
  --arg soak_summary "${ROOT_DIR}/tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json" \
  --arg gate_validation "${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates-validation.json" \
  '{
    checked_at: $checked_at,
    status: "passed",
    contract_path: $contract_path,
    playbook_path: $playbook_path,
    evidence: {
      soak_summary: $soak_summary,
      gate_validation: $gate_validation
    }
  }')"

if [[ -n "${OUTPUT_PATH}" ]]; then
  printf '%s\n' "${REPORT}" > "${OUTPUT_PATH}"
fi

printf '%s\n' "${REPORT}"
