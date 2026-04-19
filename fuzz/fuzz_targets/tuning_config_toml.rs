#![no_main]

use libfuzzer_sys::fuzz_target;
use frankenterm_core::tuning_config::TuningConfig;

fuzz_target!(|data: &[u8]| {
    if data.len() > 16_384 {
        return;
    }

    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return,
    };

    // TuningConfig uses #[serde(default)] throughout — any valid TOML
    // should either parse successfully or return a clean error, never panic.
    let _: Result<TuningConfig, _> = toml::from_str(text);
});
