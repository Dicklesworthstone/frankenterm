#![no_main]

//! [ft-he0w7] Fuzz target for the wezterm event-socket wire decoder.
//!
//! `native_events::decode_wire_event` parses newline-delimited JSON
//! frames coming off the wezterm mux event socket. The PaneOutput
//! variant embeds a base64-encoded payload that the decoder bounds at
//! `MAX_OUTPUT_BYTES` (64 KiB) before promoting to a `NativeEvent`.
//! This harness exercises the parser with arbitrary byte sequences
//! interpreted as UTF-8 strings; the contract is "any input either
//! parses to `Ok(_)` or returns `Err(_)`, never panics".
//!
//! Invariants under fuzz:
//!   - No panic on malformed JSON, malformed base64, oversize payloads,
//!     deeply nested escapes, or partial frames.
//!   - The output `NativeEvent::PaneOutput.data` length never exceeds
//!     `MAX_OUTPUT_BYTES` (asserted on the success path).
//!
//! Crash discoveries should be triaged into proptest regressions in
//! `crates/frankenterm-core/src/native_events.rs`.

use frankenterm_core::native_events::{NativeEvent, decode_wire_event_for_fuzz};
use libfuzzer_sys::fuzz_target;

// Match the runtime's MAX_EVENT_LINE_BYTES upper bound so the harness
// reflects production reality. Lines above this are dropped at the
// socket layer before they reach decode_wire_event.
const MAX_LINE_BYTES: usize = 64 * 1024;
// Same constant as native_events::MAX_OUTPUT_BYTES — duplicated here
// because the prod constant is private and bumping it without
// updating the assertion would be a benign drift.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_LINE_BYTES {
        return;
    }
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(Some(NativeEvent::PaneOutput { data, .. })) = decode_wire_event_for_fuzz(line) {
        assert!(
            data.len() <= MAX_OUTPUT_BYTES,
            "PaneOutput.data length {} exceeded MAX_OUTPUT_BYTES {}",
            data.len(),
            MAX_OUTPUT_BYTES
        );
    }
});
