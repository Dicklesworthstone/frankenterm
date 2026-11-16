#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

BENCH_PATH="crates/frankenterm-gui/benches/renderer_slo/input_to_photon.rs"
BOUND_TEST="tests/input_to_photon_bound.rs"
SLO_JSON="docs/perf/resize-quality-slo.json"
LINDLEY_JSON="docs/attestations/perf/lindley-bounds.json"

jq empty "$SLO_JSON" "$LINDLEY_JSON"

test -f "$BENCH_PATH"
test -f "$BOUND_TEST"

grep -q 'renderer_slos' crates/frankenterm/src/main.rs
grep -q 'WaRendererInputToPhotonResource' crates/frankenterm-core/src/mcp_resources.rs
grep -q 'wa://perf/renderer-slo/input_to_photon' crates/frankenterm-core/src/render_quality.rs

jq -e --arg bench "$BENCH_PATH" '
  [.slos[] | select(.id | startswith("RQ-S2.") or startswith("RQ-S3."))]
  | length == 2
  and all(.[]; .source_bench == $bench
      and .status == "substrate_wired"
      and .mcp_resource == "wa://perf/renderer-slo/input_to_photon")
' "$SLO_JSON" >/dev/null

jq -e --arg bench "$BENCH_PATH" '
  [.coverage_status[] | select(.claim_surface == "renderer_input_to_photon")]
  | length == 1
  and .[0].evidence_source == $bench
  and .[0].agreement_test == "tests/input_to_photon_bound.rs"
  and .[0].operator_surface == "ft doctor --json .renderer_slos.input_to_photon"
  and .[0].mcp_resource == "wa://perf/renderer-slo/input_to_photon"
  and .[0].status == "stage_telemetry_substrate_wired_pending_lab_run"
' "$LINDLEY_JSON" >/dev/null

if [[ "${FT_RUN_RENDERER_INPUT_SLO_RCH:-0}" == "1" ]]; then
  RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}" \
    rch exec -- env CARGO_INCREMENTAL=0 \
    cargo bench -p frankenterm-gui --features headless-render --bench input_to_photon -- --sample-size 10
fi

echo "PASS input-to-photon renderer SLO substrate"
