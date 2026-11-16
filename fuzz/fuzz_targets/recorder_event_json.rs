#![no_main]

use frankenterm_core::recording::parse_recorder_event_json;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 16_384 {
        return;
    }

    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Recorder event parser should never panic on any input.
    let _ = parse_recorder_event_json(text);
});
