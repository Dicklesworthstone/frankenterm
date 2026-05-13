#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
RCH_LOG="${LOG_DIR}/ssim_parity_release_gate_${RUN_ID}.rch.log"
PROOF_LEDGER_FILE="${LOG_DIR}/ssim_parity_release_gate_${RUN_ID}.proof-ledger.jsonl"
TARGET_DIR="${FT_CARGO_TARGET_DIR:-/tmp/ft-tf6g3-3-3-ssim-parity-${RUN_ID}}"

mkdir -p "${LOG_DIR}"
cd "${ROOT_DIR}"

test -d tests/golden/gpu
test -f crates/frankenterm-gui/tests/ssim_parity.rs
test -f docs/attestations/tui/topology-parity.json
jq empty docs/perf/resize-quality-slo.json docs/attestations/tui/topology-parity.json
grep -q 'wa://perf/renderer-slo/ssim_parity' crates/frankenterm-core/src/render_quality.rs
grep -q 'WaRendererSsimParityResource' crates/frankenterm-core/src/mcp_resources.rs
grep -q 'RobotPerfCommands::SloStatus' crates/frankenterm/src/main.rs
grep -q 'ft robot perf slo-status --slo ssim_parity' crates/frankenterm/src/main.rs
grep -q 'adversarial_large_patch_violates_default_floor' crates/frankenterm-gui/tests/ssim_parity.rs
grep -q 'topology_cross_check_covers_terminal_conformance_expected_corpus' crates/frankenterm-gui/tests/ssim_parity.rs
grep -q 'oracle-unavailable' crates/frankenterm-core/src/render_quality.rs

jq -e '
  [.slos[] | select(.id == "RQ-S13.ssim_parity_oracle_corpus")]
  | length == 1
  and .[0].source_bench == "crates/frankenterm-gui/tests/ssim_parity.rs"
  and .[0].mcp_resource == "wa://perf/renderer-slo/ssim_parity"
  and (.[0].operator_surface | contains("ft robot perf slo-status --slo ssim_parity"))
  and (.[0].operator_surface | contains("ft doctor --json .renderer_slos.ssim_parity"))
  and .[0].topology_cross_check == "docs/attestations/tui/topology-parity.json"
  and .[0].status == "substrate_wired"
  and .[0].current_degradation == "oracle-unavailable"
  and .[0].owner_bead == "ft-tf6g3.3.3"
' docs/perf/resize-quality-slo.json >/dev/null

current_degradation="$(
  jq -r '
    [.slos[] | select(.id == "RQ-S13.ssim_parity_oracle_corpus")][0].current_degradation // "none"
  ' docs/perf/resize-quality-slo.json
)"

export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="ft-tf6g3.3.3"
export RCH_PROOF_LEDGER_SCENARIO_ID="ssim_parity_release_gate"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

rch_init "${LOG_DIR}" "${RUN_ID}" "ssim_parity_release_gate"
ensure_rch_ready

run_rch_cargo_logged "${RCH_LOG}" \
  env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" \
  cargo test -p frankenterm-gui --test ssim_parity -- --nocapture

grep -q 'test result: ok' "${RCH_LOG}"
validation_dir="$(rch_validate_proof_ledger_file "${PROOF_LEDGER_FILE}")"

if [[ "${current_degradation}" != "none" && "${SSIM_PARITY_ALLOW_DEGRADED_SUBSTRATE:-0}" != "1" ]]; then
  echo "DEGRADED SSIM parity release gate: ${current_degradation}" >&2
  echo "Substrate proof passed, but release gating requires retained ratatui-vs-ftui oracle evidence." >&2
  echo "Set SSIM_PARITY_ALLOW_DEGRADED_SUBSTRATE=1 only for non-release substrate validation." >&2
  exit 2
fi

echo "PASS SSIM parity release gate"
echo "RCH log: ${RCH_LOG}"
echo "Proof ledger validation: ${validation_dir}"
