#![no_main]

use libfuzzer_sys::fuzz_target;
use wa_core::ingest::{Osc133State, parse_osc133_markers};

fuzz_target!(|data: &[u8]| {
    if data.len() > 64_000 {
        return;
    }

    let text = String::from_utf8_lossy(data);
    let markers = parse_osc133_markers(&text);
    let mut state = Osc133State::new();

    for marker in markers {
        state.process_marker(marker);
    }

    let _ = state.state.is_at_prompt();
});
