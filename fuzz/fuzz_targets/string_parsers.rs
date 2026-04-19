#![no_main]

use libfuzzer_sys::fuzz_target;
use frankenterm_core::caut::CautService;
use frankenterm_core::event_stream::SeverityLevel;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4_096 {
        return;
    }

    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return,
    };

    // CautService::from_cli_input should handle any string without panicking.
    let _ = CautService::from_cli_input(text);

    // SeverityLevel::from_str_loose uses case-insensitive matching with
    // multiple aliases — should never panic on any input.
    let _ = SeverityLevel::from_str_loose(text);
});
