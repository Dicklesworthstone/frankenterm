#![no_main]

use libfuzzer_sys::fuzz_target;
use frankenterm_core::config::Config;

fuzz_target!(|data: &[u8]| {
    if data.len() > 32_768 {
        return;
    }

    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Config::from_toml should never panic on any input.
    let _ = Config::from_toml(text);
});
