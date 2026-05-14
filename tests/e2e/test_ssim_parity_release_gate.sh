#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
CORE_RCH_LOG="${LOG_DIR}/ssim_parity_release_gate_${RUN_ID}.core_driver.rch.log"
GUI_RCH_LOG="${LOG_DIR}/ssim_parity_release_gate_${RUN_ID}.gui_corpus.rch.log"
PROOF_LEDGER_FILE="${LOG_DIR}/ssim_parity_release_gate_${RUN_ID}.proof-ledger.jsonl"
CORE_TARGET_DIR="${FT_CORE_CARGO_TARGET_DIR:-/tmp/ft-tf6g3-3-3-core-driver-${RUN_ID}}"
GUI_TARGET_DIR="${FT_GUI_CARGO_TARGET_DIR:-${FT_CARGO_TARGET_DIR:-/tmp/ft-tf6g3-3-3-ssim-parity-${RUN_ID}}}"

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
grep -q 'backend_driver_harness_reaches_real_ratatui_and_ftui_renderers' crates/frankenterm-core/src/tui/mod.rs
grep -q 'retained-release-run-pending' crates/frankenterm-core/src/render_quality.rs

jq -e '
  [.slos[] | select(.id == "RQ-S13.ssim_parity_oracle_corpus")]
  | length == 1
  and .[0].source_bench == "crates/frankenterm-gui/tests/ssim_parity.rs"
  and .[0].backend_driver_test == "crates/frankenterm-core/src/tui/mod.rs::backend_driver_harness_reaches_real_ratatui_and_ftui_renderers"
  and .[0].mcp_resource == "wa://perf/renderer-slo/ssim_parity"
  and (.[0].operator_surface | contains("ft robot perf slo-status --slo ssim_parity"))
  and (.[0].operator_surface | contains("ft doctor --json .renderer_slos.ssim_parity"))
  and .[0].topology_cross_check == "docs/attestations/tui/topology-parity.json"
  and .[0].status == "substrate_wired"
  and .[0].current_degradation == "retained-release-run-pending"
  and .[0].owner_bead == "ft-tf6g3.3.3"
' docs/perf/resize-quality-slo.json >/dev/null

current_degradation="$(
  jq -r '
    [.slos[] | select(.id == "RQ-S13.ssim_parity_oracle_corpus")][0].current_degradation // "none"
  ' docs/perf/resize-quality-slo.json
)"

export RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
export RCH_SKIP_WORKER_SELECTION_PREFLIGHT="${RCH_SKIP_WORKER_SELECTION_PREFLIGHT:-1}"
export RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-2400}"
export RCH_PROOF_LEDGER_FILE="${PROOF_LEDGER_FILE}"
export RCH_PROOF_LEDGER_BEAD_ID="ft-tf6g3.3.3"
export RCH_PROOF_LEDGER_SCENARIO_ID="ssim_parity_release_gate"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

rch_init "${LOG_DIR}" "${RUN_ID}" "ssim_parity_release_gate"
ensure_rch_ready

run_rch_cargo_logged "${CORE_RCH_LOG}" \
  env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${CORE_TARGET_DIR}" \
  cargo test -p frankenterm-core --lib --features rollout \
    backend_driver_harness_reaches_real_ratatui_and_ftui_renderers -- --nocapture

run_rch_cargo_logged "${GUI_RCH_LOG}" \
  env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${GUI_TARGET_DIR}" \
  cargo test -p frankenterm-gui --lib ssim_parity -- --nocapture

grep -q 'test result: ok' "${CORE_RCH_LOG}"
grep -q 'test result: ok' "${GUI_RCH_LOG}"
validation_dir="$(rch_validate_proof_ledger_file "${PROOF_LEDGER_FILE}")"

if [[ "${current_degradation}" != "none" && "${SSIM_PARITY_ALLOW_DEGRADED_SUBSTRATE:-0}" != "1" ]]; then
  echo "DEGRADED SSIM parity release gate: ${current_degradation}" >&2
  echo "Substrate proof passed, but release gating requires retained ratatui-vs-ftui oracle evidence." >&2
  echo "Set SSIM_PARITY_ALLOW_DEGRADED_SUBSTRATE=1 only for non-release substrate validation." >&2
  exit 2
fi

echo "PASS SSIM parity release gate"
echo "Core backend-driver RCH log: ${CORE_RCH_LOG}"
echo "GUI corpus RCH log: ${GUI_RCH_LOG}"
echo "Proof ledger validation: ${validation_dir}"
