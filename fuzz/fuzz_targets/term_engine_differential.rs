#![no_main]
//! Differential fuzz target for the terminal engines (ft-yccm0.1.8).
//!
//! The first input byte picks the geometry, the second seeds the chunk plan,
//! and the rest is the output stream. Every candidate registered in the
//! shared harness must match the legacy engine after every chunk. On a
//! divergence the harness minimizes the input and writes a reproducer and a
//! readable diff to `$FT_DIFFERENTIAL_OUT` (else the system temp directory)
//! before this target panics.
//!
//! `cargo fuzz run term_engine_differential -- -dict=fuzz/term_engine_differential.dict`

#[path = "../../frankenterm/term/tests/differential/mod.rs"]
#[allow(dead_code)]
mod differential;

use differential::engine::GEOMETRIES;
use differential::harness;
use differential::streams::chunk_with_seed;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 || data.len() > MAX_INPUT_BYTES {
        return;
    }
    let geometry = GEOMETRIES[usize::from(data[0]) % GEOMETRIES.len()];
    let chunks = chunk_with_seed(&data[2..], u64::from(data[1]));
    if let Err(report) = harness::check("fuzz", &geometry, &chunks) {
        panic!("{report}");
    }
});
