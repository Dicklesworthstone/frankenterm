#![no_main]

use frankenterm_core::ntm_importer::{parse_ntm_config, parse_ntm_sessions, parse_ntm_workflows};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 65_536 {
        return;
    }

    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return,
    };

    // All three NTM parsers should never panic on any input.
    let _ = parse_ntm_sessions(text);
    let _ = parse_ntm_workflows(text);
    let _ = parse_ntm_config(text);
});
