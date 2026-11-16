#![no_main]

#[path = "../contract_fuzz_common.rs"]
mod contract_fuzz_common;

use arbitrary::Arbitrary;
use contract_fuzz_common::{
    assert_no_raw_content_flags, assert_schema_valid, bounded_ms, compile_schema, roundtrip_value,
    stable_fragment,
};
use frankenterm_core::runtime_telemetry::{
    SwarmCapacityOperatorSummary, SwarmResourceCockpitSnapshot,
};
use jsonschema::Validator;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

static SCHEMA: OnceLock<Validator> = OnceLock::new();

fn schema() -> &'static Validator {
    SCHEMA.get_or_init(|| {
        compile_schema(include_bytes!(
            "../../docs/json-schema/ft-resource-pressure-cockpit.json"
        ))
    })
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    generated_at_ms: u64,
    transparency_level: u8,
    source: String,
}

fuzz_target!(|input: FuzzInput| {
    let generated_at_ms = bounded_ms(input.generated_at_ms);
    let source = stable_fragment(input.source, "cargo_fuzz.contract_resource_cockpit");
    let summary = SwarmCapacityOperatorSummary::unavailable(
        generated_at_ms,
        input.transparency_level,
        &source,
    );
    let cockpit = SwarmResourceCockpitSnapshot::from_capacity_summary(&summary, None, &[]);
    let value = roundtrip_value(&cockpit);
    assert_schema_valid(schema(), &value);
    assert_no_raw_content_flags(&value);
});
