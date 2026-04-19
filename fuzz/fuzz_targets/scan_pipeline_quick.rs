#![no_main]

use libfuzzer_sys::fuzz_target;
use frankenterm_core::scan_pipeline::quick_scan;

fuzz_target!(|data: &[u8]| {
    if data.len() > 128_000 {
        return;
    }

    // quick_scan processes raw terminal output (bytes, ANSI codes,
    // escape sequences). It should never panic on any input.
    let output = quick_scan(data);

    // Sanity: input_bytes should match what we fed in.
    assert_eq!(output.input_bytes, data.len() as u64);
});
