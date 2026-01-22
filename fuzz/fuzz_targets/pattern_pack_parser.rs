#![no_main]

use libfuzzer_sys::fuzz_target;
use serde::Deserialize;
use wa_core::patterns::{PatternEngine, PatternPack, RuleDef};

#[derive(Debug, Deserialize)]
struct PackToml {
    name: String,
    version: String,
    rules: Vec<RuleDef>,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 16_384 {
        return;
    }

    let input = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(_) => return,
    };

    let pack: PackToml = match toml::from_str(input) {
        Ok(parsed) => parsed,
        Err(_) => return,
    };

    let pack = PatternPack::new(pack.name, pack.version, pack.rules);
    let _ = PatternEngine::with_packs(vec![pack]);
});
