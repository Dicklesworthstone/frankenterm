#![no_main]

#[path = "../contract_fuzz_common.rs"]
mod contract_fuzz_common;

use arbitrary::Arbitrary;
use contract_fuzz_common::{
    MAX_ITEMS, assert_no_raw_content_flags, assert_schema_valid, bounded_ms, compile_schema,
    roundtrip_value, stable_fragment,
};
use frankenterm_core::swarm_scheduler::{
    HerdWaveEventKind, HerdWaveMcpResourceSurface, build_herd_wave_surface_report,
};
use jsonschema::Validator;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

static SCHEMA: OnceLock<Validator> = OnceLock::new();

fn schema() -> &'static Validator {
    SCHEMA
        .get_or_init(|| compile_schema(include_bytes!("../../docs/json-schema/ft-herd-wave.json")))
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    generated_at_ms: u64,
    source: String,
    signal_panes: Vec<u64>,
    kind: RawHerdWaveKind,
    signal_spacing_ms: u64,
    max_age_ms: u64,
}

#[derive(Arbitrary, Debug)]
enum RawHerdWaveKind {
    Compaction,
    Retry,
    RateLimitRecovery,
    SearchBurst,
    WorkflowFanout,
    Wake,
    Other,
}

impl RawHerdWaveKind {
    fn into_contract(self) -> HerdWaveEventKind {
        match self {
            Self::Compaction => HerdWaveEventKind::Compaction,
            Self::Retry => HerdWaveEventKind::Retry,
            Self::RateLimitRecovery => HerdWaveEventKind::RateLimitRecovery,
            Self::SearchBurst => HerdWaveEventKind::SearchBurst,
            Self::WorkflowFanout => HerdWaveEventKind::WorkflowFanout,
            Self::Wake => HerdWaveEventKind::Wake,
            Self::Other => HerdWaveEventKind::Other,
        }
    }
}

fn pane_ids(signal_panes: Vec<u64>) -> Vec<u64> {
    let panes = signal_panes
        .into_iter()
        .take(MAX_ITEMS)
        .map(|pane_id| pane_id % 1_000_000)
        .collect::<Vec<_>>();
    if panes.is_empty() { vec![0] } else { panes }
}

fuzz_target!(|input: FuzzInput| {
    let generated_at_ms = bounded_ms(input.generated_at_ms);
    let source = stable_fragment(input.source, "cargo_fuzz.contract_herd_wave");
    let kind = input.kind.into_contract();
    let signal_spacing_ms = (input.signal_spacing_ms % 120_000).max(1);
    let max_age_ms = input.max_age_ms % 600_000;
    let signal_panes = pane_ids(input.signal_panes);

    let report = build_herd_wave_surface_report(
        &source,
        generated_at_ms,
        &signal_panes,
        kind,
        signal_spacing_ms,
        max_age_ms,
        HerdWaveMcpResourceSurface::implemented("mcp://frankenterm/herd-wave"),
    );
    assert!(
        !report.dry_run_plan.live_mutation_allowed,
        "herd-wave dry-run plan must not enable live mutation"
    );
    assert!(
        !report.mcp_resource.live_mutation_allowed,
        "herd-wave MCP metadata must remain read-only"
    );

    let value = roundtrip_value(&report.snapshot);
    assert_schema_valid(schema(), &value);
    assert_no_raw_content_flags(&value);
});
