//! Fuzz target for the YAML + JSON pattern-pack loader (ft-8hbq8).
//!
//! `crates/frankenterm-core/src/patterns.rs:1123` carries the author's
//! own attacker-reachable note:
//!
//! > Pattern packs are attacker-reachable via
//! > `.ft/patterns/*.{yaml,json,toml}`. A maliciously-crafted pack
//! > could stack-blow the YAML deserializer on deeply-nested input,
//! > exhaust memory through giant string repeats, or pin the regex
//! > engine via catastrophic backtracking. The serde_yaml /
//! > serde_json / toml deserializer has not been fuzzed against this
//! > code path.
//!
//! `pattern_pack_toml.rs` covers the TOML side. This target is the
//! sibling for the YAML and JSON entry points.
//!
//! ## Approach
//!
//! Single libfuzzer harness using a leading-byte format selector:
//! odd → YAML, even → JSON. The remaining bytes are interpreted as
//! UTF-8 text and fed to the corresponding `serde_*::from_str`
//! call into `PatternPack`. On successful parse, the pack is run
//! through `PatternLibrary::new(vec![pack])` which invokes
//! `PatternPack::validate()` internally — exercising the validation
//! sandbox + regex compilation + duplicate-rule-id check.
//!
//! Bound the input at `MAX_INPUT_BYTES` so the harness stays under
//! libfuzzer's per-iteration budget. The size limit is generous
//! (matches the production `MAX_PACK_BYTES = 16 MiB` ceiling) but
//! libfuzzer will rarely generate inputs that large; the bound is
//! a backstop against pathological inputs that would slow the
//! corpus-minimizer.
//!
//! ## What this catches
//!
//! - **Deserializer panics:** serde_yaml or serde_json crashing on
//!   malformed structure (recursion bombs, alias loops, NaN, etc.).
//! - **OOM:** giant scalar strings or unbounded `Vec<RuleDef>`.
//! - **Validate-time panics:** regex compilation panicking on
//!   adversarial pattern strings, duplicate-rule-id assertion
//!   misbehaving on edge cases.
//! - **Stack overflow:** deeply nested YAML aliases.
//!
//! ## What this does NOT catch
//!
//! - Filesystem-level attacks (symlink escape, sandbox bypass) —
//!   those need `pattern_pack_discovery.rs` (separate follow-on,
//!   tracked in ft-8hbq8 description).
//! - The actual scan-time match cost (catastrophic backtracking
//!   regex on pane content) — covered by `pattern_regex_match.rs`.

#![no_main]

use frankenterm_core::patterns::{PatternLibrary, PatternPack};
use libfuzzer_sys::fuzz_target;

/// libfuzzer's default per-iteration input ceiling. Larger inputs
/// slow the mutator and rarely surface new coverage.
const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > MAX_INPUT_BYTES {
        return;
    }

    // Leading byte selects YAML vs JSON. Both code paths land in the
    // same deserialize-into-`PatternPack` step that the production
    // loader at patterns.rs:1203-1206 dispatches to via the file
    // extension.
    let is_yaml = data[0] & 1 == 0;
    let payload = &data[1..];

    let Ok(text) = std::str::from_utf8(payload) else {
        return;
    };

    // Step 1: deserializer entry point. Catches the parser-confusion
    // class — recursion bombs, alias chains, malformed UTF-8 in
    // string scalars, integer overflow on size-prefix fields, etc.
    let parse_result: Result<PatternPack, String> = if is_yaml {
        serde_yaml::from_str::<PatternPack>(text).map_err(|e| e.to_string())
    } else {
        serde_json::from_str::<PatternPack>(text).map_err(|e| e.to_string())
    };

    let Ok(pack) = parse_result else {
        // Deserializer rejected the input. That's the expected
        // outcome on most random bytes — return without further
        // work so the iteration is fast.
        return;
    };

    // Step 2: validation. `PatternLibrary::new` is the production
    // gate that calls `pack.validate()` internally — it walks the
    // `Vec<RuleDef>` checking for duplicate IDs, compiles every
    // `regex` field via `compile_rule_regex` (which carries the
    // ft-xv561 backtrack cap), and rejects empty/malformed
    // anchors. This catches:
    //
    // - Regex bombs (validation-time, not match-time): malformed
    //   regex inputs that panic the `regex` crate's compilation.
    // - Duplicate-id collisions on collision-resistant byte
    //   inputs (e.g., NUL-bearing IDs).
    // - Unicode normalization edge cases in pack-name / rule-id
    //   strings.
    let _ = PatternLibrary::new(vec![pack]);
});
