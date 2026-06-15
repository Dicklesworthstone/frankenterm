//! Regression guard for the "wired-but-inert" gap class (ft-9dy83).
//!
//! A recurring reality-check defect class in this codebase: a config field is
//! parsed/stored but never read, or a component is fully built + tested but
//! never called on any production path — so configured behavior is a silent
//! no-op (e.g. ft-mhq10, ft-jl09u, ft-rrqhm, ft-pfton, ft-y7x56, ft-xx5cl).
//!
//! This is a ZERO-RCH **source-scan** guard. For each genuinely-wired item it
//! asserts a production (non-test) callsite still exists, so re-introducing the
//! defect (deleting the wiring) fails CI. `#[cfg(test)]` blocks are stripped
//! first, so a lingering test-only copy of a needle cannot keep the guard green
//! after the production wiring is removed.
//!
//! IMPORTANT — honesty about scope (verified 2026-06-15):
//! Two components commonly grouped into this class are NOT in fact wired and are
//! intentionally **excluded** from the asserted matrix (asserting them would be
//! a lie):
//!   * `RetentionManager` (recorder_retention.rs) — only test constructors
//!     exist; tracked OPEN by ft-y7x56 ("built but never wired to any
//!     scheduler").
//!   * the `segment_embeddings` writer (`Storage::store_embedding*`) — has no
//!     in-tree production caller; tracked OPEN by ft-xx5cl ("no production
//!     writer -> semantic/hybrid search silently degrades to lexical-only").
//! When those beads land, add their production callsites to `WIRED` below.

use std::path::PathBuf;

/// Read a source file under this crate's `src/`.
fn read_src(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Net `{` minus `}` on a line (adequate for well-formed sources; braces inside
/// string/char literals are a tolerated minor imprecision for a guard).
fn brace_delta(line: &str) -> i32 {
    let opens = line.bytes().filter(|&b| b == b'{').count() as i32;
    let closes = line.bytes().filter(|&b| b == b'}').count() as i32;
    opens - closes
}

/// Remove `#[cfg(test)]`-attributed items (modules / fns / braceless decls) so
/// wiring assertions count only production code.
fn production_only(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut armed = false; // saw `#[cfg(test)]`, awaiting the item
    let mut in_test = false;
    let mut depth: i32 = 0;

    for line in src.lines() {
        if !in_test && !armed && line.trim_start().starts_with("#[cfg(test)]") {
            armed = true;
            continue;
        }
        if armed {
            if line.contains('{') {
                depth += brace_delta(line);
                if depth > 0 {
                    in_test = true;
                } else {
                    depth = 0; // opened and closed on one line
                }
                armed = false;
                continue;
            }
            // Braceless `#[cfg(test)]` item (e.g. `use ...;`, `static X = ...;`).
            if line.trim_end().ends_with(';') {
                armed = false;
            }
            // else: rare multi-line pre-brace continuation — stay armed.
            continue;
        }
        if in_test {
            depth += brace_delta(line);
            if depth <= 0 {
                in_test = false;
                depth = 0;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// (source file, production needle, why it must stay wired).
/// Every needle below was confirmed to live in a production (non-test) callsite.
const WIRED: &[(&str, &str, &str)] = &[
    // FilteredEventStream — live IPC streaming, not just a library type.
    (
        "ipc.rs",
        "FilteredEventStream::new(",
        "FilteredEventStream must be constructed on the production IPC stream path",
    ),
    (
        "ipc.rs",
        "return stream_subscribe_events(",
        "IPC SubscribeEvents must dispatch to the FilteredEventStream-backed streamer",
    ),
    // Kill-switch tiers — consulted by production policy authorization.
    (
        "policy.rs",
        "kill_switch().is_emergency()",
        "policy authorization must consult the kill-switch tier in production",
    ),
    // storage.retention_max_mb — the size cap must actually be enforced (ft-rrqhm).
    (
        "runtime.rs",
        "enforce_size_limit_with_cx(&loop_cx, retention_max_mb)",
        "storage.retention_max_mb size cap must be enforced in the maintenance loop (ft-rrqhm)",
    ),
    // retention_tiers — config changes must be applied on hot-reload.
    (
        "runtime.rs",
        "new_config.retention_tiers != retention_tiers",
        "retention_tiers config changes must be applied on hot-reload",
    ),
];

#[test]
fn wired_components_have_production_callsites() {
    let mut missing = Vec::new();
    for (file, needle, why) in WIRED {
        let prod = production_only(&read_src(file));
        if !prod.contains(needle) {
            missing.push(format!("  - {file}: missing `{needle}`\n      ({why})"));
        }
    }
    assert!(
        missing.is_empty(),
        "wired-but-inert regression (ft-9dy83): production callsite(s) gone — \
         a previously-wired component/config field is no longer consulted in \
         production:\n{}",
        missing.join("\n")
    );
}

/// The test-stripper must not accidentally eat production code (which would make
/// the wiring assertions vacuously pass on an empty haystack).
#[test]
fn production_only_strips_tests_but_keeps_production() {
    let sample = "\
fn prod_one() { let x = 1; }
#[cfg(test)]
mod tests {
    fn helper() { let secret = 2; }
    #[test]
    fn t() { assert_eq!(1, 1); }
}
fn prod_two() { let y = 3; }
#[cfg(test)]
use std::collections::HashMap;
fn prod_three() {}
";
    let prod = production_only(sample);
    assert!(prod.contains("fn prod_one"), "kept production fn one");
    assert!(prod.contains("fn prod_two"), "kept production fn two");
    assert!(prod.contains("fn prod_three"), "kept production fn after braceless cfg(test) use");
    assert!(!prod.contains("fn helper"), "stripped test-module body");
    assert!(!prod.contains("secret"), "stripped test-module body");
    assert!(!prod.contains("HashMap"), "stripped braceless cfg(test) item");

    // A production needle that ALSO appears only inside a test block must be
    // removed by stripping (this is what stops a stale test copy from masking
    // deleted production wiring).
    let only_in_test = "\
#[cfg(test)]
mod tests {
    fn t() { thing.is_wired(); }
}
";
    assert!(
        !production_only(only_in_test).contains("is_wired()"),
        "a needle living only in a test block must not survive stripping"
    );
}
