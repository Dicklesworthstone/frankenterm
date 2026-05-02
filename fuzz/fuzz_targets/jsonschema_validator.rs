#![no_main]
#![allow(deprecated)]

//! [ft-ul4vi] Fuzz target for the `jsonschema` crate as it is used by
//! ft-5ikbd's robot envelope conformance gate and ft-2sumi's
//! ft.toml schema gate.
//!
//! ## Attack surface
//!
//! `jsonschema 0.21` is reused by `tests/conformance_robot_envelope_schema.rs`,
//! `tests/conformance_ft_config_schema.rs`,
//! `tests/conformance_pattern_pack_format.rs`, and
//! `tests/conformance_mcp_coverage.rs`. JSON Schema is a known
//! parser-confusion class:
//!
//! - `$ref` chains can recurse infinitely (the canonical example
//!   `{"$ref": "#"}` is the smallest stack-overflow trigger).
//! - `if/then/else` conditional clauses can construct evaluation cycles.
//! - Complex `pattern` regex strings invoke the `regex` crate; a
//!   malicious schema can pin validation on every input.
//! - `additionalProperties: false` plus deep `oneOf`/`anyOf` of complex
//!   shapes triples evaluation cost.
//!
//! Today the schemas in `docs/json-schema/` are committed by
//! maintainers (trusted) and the data validated against them is
//! produced by tests. But the crate itself will be reused when
//! conformance gates expand to validate envelopes from external
//! robot/MCP clients. At that point the validator becomes a
//! parser-on-untrusted-input.
//!
//! ## Modes (Arbitrary-driven)
//!
//! - **Compile**: feed an arbitrary byte slice as a candidate schema
//!   through `Validator::options().with_draft(Draft::Draft202012)
//!   .compile(&value)`. Hits the schema parser + state-machine
//!   construction path.
//! - **Validate**: validate an arbitrary byte slice (parsed as
//!   `serde_json::Value`) against a TRUSTED pre-compiled schema
//!   loaded from `docs/json-schema/wa-robot-envelope.json`. Hits the
//!   evaluator + walking the parsed schema's state machine over
//!   adversarial data.
//! - **BothPaths**: independent fuzzer-controlled bytes for schema
//!   AND data — covers the cross-product (untrusted schema +
//!   untrusted data) which the production code path will encounter
//!   when external-client validation lands.
//!
//! ## Contracts pinned (Archetype 1: crash detector)
//!
//! 1. **No panic on any byte sequence.** `compile()` and `validate()`
//!    must return `Ok(_)` or `Err(_)` for every input. Stack
//!    overflow on `{"$ref": "#"}`, OOM on `oneOf` of N variants,
//!    and the `pattern: "(a+)+"`-style regex pinning are exactly
//!    the bugs this harness is hunting.
//! 2. **Bounded input size.** Caps match the bead's documented
//!    limits (64 KiB schema, 256 KiB data) so libFuzzer doesn't
//!    waste cycles on inputs the production code would reject at
//!    framing.
//!
//! Pattern reuse: matches ft-h8v8v (wire_envelope) and ft-hfbsp
//! (simd_scan) — Archetype 5 structure-aware Arbitrary input +
//! Archetype 1 crash detector.

use arbitrary::Arbitrary;
use jsonschema::{Draft, JSONSchema as Validator};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use std::sync::OnceLock;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_DATA_BYTES: usize = 256 * 1024;

/// Trusted schema cached across fuzz iterations. Compiled lazily on
/// first use so the Validate / BothPaths modes don't pay setup cost
/// per iteration.
static TRUSTED_SCHEMA: OnceLock<Validator> = OnceLock::new();

fn trusted_schema() -> &'static Validator {
    TRUSTED_SCHEMA.get_or_init(|| {
        // Embedded at compile time so the harness binary is
        // self-contained — no path resolution at fuzz time, no
        // file I/O on the hot path.
        let bytes = include_bytes!("../../docs/json-schema/wa-robot-envelope.json");
        let value: Value =
            serde_json::from_slice(bytes).expect("trusted envelope schema must be valid JSON");
        Validator::options()
            .with_draft(Draft::Draft202012)
            .compile(&value)
            .expect("trusted envelope schema must compile under Draft 2020-12")
    })
}

#[derive(Arbitrary, Debug)]
enum FuzzInput<'a> {
    /// Compile path — feed bytes as a candidate schema.
    Compile(&'a [u8]),
    /// Validate path — fuzz the data side against a trusted schema.
    Validate(&'a [u8]),
    /// Cross-product — both schema AND data are fuzzer-controlled.
    BothPaths {
        schema_bytes: &'a [u8],
        data_bytes: &'a [u8],
    },
}

fn try_compile(bytes: &[u8]) {
    if bytes.len() > MAX_SCHEMA_BYTES {
        return;
    }
    let Ok(value): Result<Value, _> = serde_json::from_slice(bytes) else {
        return;
    };
    // Contract 1: compile must return Ok or Err — never panic. The
    // result is intentionally discarded; a successful compile is
    // not the goal, surviving every input is.
    let _ = Validator::options()
        .with_draft(Draft::Draft202012)
        .compile(&value);
}

fn try_validate_against_trusted(bytes: &[u8]) {
    if bytes.len() > MAX_DATA_BYTES {
        return;
    }
    let Ok(value): Result<Value, _> = serde_json::from_slice(bytes) else {
        return;
    };
    // Validator::validate returns an iterator of errors; we drain it
    // to materialize any work the validator deferred to lazy
    // iteration. Contract 1: no panic.
    let validator = trusted_schema();
    if let Err(errors) = validator.validate(&value) {
        let _ = errors.count();
    }
}

fuzz_target!(|input: FuzzInput| {
    match input {
        FuzzInput::Compile(bytes) => {
            try_compile(bytes);
        }
        FuzzInput::Validate(bytes) => {
            try_validate_against_trusted(bytes);
        }
        FuzzInput::BothPaths {
            schema_bytes,
            data_bytes,
        } => {
            // Cross-product mode. Compile a fuzzer-controlled schema;
            // if it succeeds, validate fuzzer-controlled data against
            // it. This is the production code path that lands when
            // external clients can supply both halves.
            if schema_bytes.len() > MAX_SCHEMA_BYTES || data_bytes.len() > MAX_DATA_BYTES {
                return;
            }
            let Ok(schema_value): Result<Value, _> = serde_json::from_slice(schema_bytes) else {
                return;
            };
            let validator = match Validator::options()
                .with_draft(Draft::Draft202012)
                .compile(&schema_value)
            {
                Ok(v) => v,
                Err(_) => return,
            };
            let Ok(data_value): Result<Value, _> = serde_json::from_slice(data_bytes) else {
                return;
            };
            if let Err(errors) = validator.validate(&data_value) {
                let _ = errors.count();
            }
        }
    }
});
