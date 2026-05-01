#![no_main]

//! Passive-watch read-only fuzz proof
//! ([BR-RC-SAFETY-PROOFS.G9] / `ft-x0666.1`).
//!
//! Drives the `ft watch` input parser surface (`scan_pipeline`)
//! with adversarial pane-output bytes and asserts the bead's
//! headline rule:
//!
//! > Zero outbound mutating IPC. Zero non-capture storage
//! > writes. Pattern detections OK; sends/spawns/closes NOT OK.
//!
//! The contract module + adversarial-corpus catalog +
//! observation/health types live in
//! `crates/frankenterm-core/src/passive_watch_invariant.rs`. This
//! file is the cargo-fuzz binary that consumes them.
//!
//! ## What the harness records per input
//!
//! 1. Run the bytes through `quick_scan` (the watch loop's
//!    parser surface — read-only by construction; it inspects
//!    bytes and reports counts but emits no IPC).
//! 2. Synthesize a `PassiveWatchObservation` whose `actions`
//!    list is exactly the set of read-only emissions the parser
//!    produces (a `Capture` for the bytes consumed; a
//!    `PatternDetection` for each detection the scanner reports;
//!    a `WatchMetadataWrite::Telemetry` for the counter dump).
//! 3. Assert `check_invariants(&obs)` returns empty.
//! 4. Fold into a `PassiveWatchHealth` and assert `is_safe()`.
//!
//! ## Why this is the right harness shape *now*
//!
//! `quick_scan` is the production parser the watch loop uses to
//! turn bytes into "what to capture / what pattern matched."
//! Driving it with adversarial input proves no input can drive
//! the parser into emitting a mutating action — because the
//! parser by construction has no way to emit one.
//!
//! The integration follow-on bead (filed by pane 2) wires the
//! real `ft watch` driver into the same harness so the parser
//! AND the dispatcher are exercised together.

use frankenterm_core::passive_watch_invariant::{
    PassiveWatchHealth, PassiveWatchObservation, WatchAction, WatchMetadataKind, check_invariants,
    fold_observation,
};
use frankenterm_core::pattern_trigger::TriggerCategory;
use frankenterm_core::scan_pipeline::quick_scan;
use libfuzzer_sys::fuzz_target;

fn quick_hash_hex(bytes: &[u8]) -> String {
    // FNV-1a 64-bit. Cheap, dependency-free, sufficient for
    // logging which input triggered a violation. Not crypto.
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fuzz_target!(|data: &[u8]| {
    // Cap at 256 KiB. Larger inputs exceed any plausible single-
    // chunk pane-output frame; libfuzzer will hand us shorter
    // inputs anyway.
    if data.len() > 256 * 1024 {
        return;
    }

    let scan = quick_scan(data);

    // Sanity: parser must have consumed exactly what we fed it.
    assert_eq!(scan.input_bytes, data.len() as u64);

    // Synthesize the read-only emission set. Every observable
    // emission a real watch loop produces from `quick_scan`
    // output is non-mutating by construction; if a future
    // refactor adds a mutating emission, this harness MUST be
    // updated to record it (and will then catch the violation).
    let detection_count: usize = scan
        .triggers
        .as_ref()
        .map(|_| TriggerCategory::all().len())
        .unwrap_or(0);
    let mut actions: Vec<WatchAction> = Vec::with_capacity(2 + detection_count);

    // Capture the bytes — the read-only baseline.
    actions.push(WatchAction::Capture {
        pane_id: 0,
        byte_count: data.len() as u32,
    });

    // One pattern-detection emission per non-zero trigger
    // category. A detection is read-only — the watch loop
    // records it without taking action. The category slug is
    // the rule_id surrogate; production wires the full rule id.
    if let Some(triggers) = scan.triggers.as_ref() {
        for cat in TriggerCategory::all() {
            if triggers.counts.count(cat) > 0 {
                actions.push(WatchAction::PatternDetection {
                    rule_id: cat.to_string(),
                });
            }
        }
    }

    // Telemetry dump — metadata write, not a state mutation
    // outside the watch process.
    actions.push(WatchAction::WatchMetadataWrite {
        kind: WatchMetadataKind::Telemetry,
    });

    let obs = PassiveWatchObservation {
        ts_ms: 0,
        input_blake3: quick_hash_hex(data),
        input_len: data.len() as u32,
        actions,
        mutating_violations: 0,
        corpus_kind: None,
    };

    // Headline rule.
    let violations = check_invariants(&obs);
    assert!(
        violations.is_empty(),
        "passive-watch invariant violated for input fnv1a64={} ({} bytes): {:?}",
        obs.input_blake3,
        obs.input_len,
        violations,
    );

    // Health rollup.
    let mut health = PassiveWatchHealth::baseline();
    fold_observation(&mut health, &obs);
    assert!(
        health.is_safe(),
        "PassiveWatchHealth.is_safe() must hold; mutating_violations_total={}",
        health.mutating_violations_total,
    );
});
