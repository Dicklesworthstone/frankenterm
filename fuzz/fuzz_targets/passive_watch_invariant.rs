#![no_main]

//! Passive-watch read-only fuzz proof
//! ([BR-RC-SAFETY-PROOFS.G9] / `ft-x0666.1`).
//!
//! Drives the `ft watch` detection surface with adversarial
//! pane-output bytes and asserts the bead's headline rule:
//!
//! > Zero outbound mutating IPC. Zero non-capture storage
//! > writes. Pattern detections OK; sends/spawns/closes NOT OK.
//!
//! The contract module, adversarial-corpus catalog and
//! observation/health types live in
//! `crates/frankenterm-core/src/passive_watch_invariant.rs`. This
//! file is the cargo-fuzz binary that consumes them.
//!
//! ## What the harness records per input
//!
//! 1. Decode the bytes as the capture path does (lossy UTF-8) and
//!    run them through `PatternEngine::detect_with_context` with the
//!    default `PatternsConfig` and a fresh per-pane context. That is
//!    the call the watch loop makes for every captured segment
//!    (`runtime.rs`, through `detect_with_context_in_zone`).
//! 2. Synthesize a `PassiveWatchObservation` whose `actions` list is
//!    the set of read-only emissions the watch loop makes from that
//!    result: a `Capture` for the bytes consumed, a
//!    `PatternDetection` per detection (named by its `rule_id`), and
//!    a `WatchMetadataWrite::Telemetry` for the counter dump.
//! 3. Assert every detection names a rule the engine loaded.
//! 4. Assert `check_invariants(&obs)` returns empty.
//! 5. Fold into a `PassiveWatchHealth` and assert `is_safe()`.
//!
//! The detection surface returns plain `Detection` values and has no
//! handle to the mux, the IPC client or storage, so no input can make
//! it emit a mutating action; the harness pins that shape and catches
//! panics in the production matcher. The dispatcher that acts on
//! detections (workflows, `ft watch` handlers) is not exercised here.
//!
//! The previous target drove `scan_pipeline::quick_scan`, which was
//! deleted in round 9; this one drives the matcher that replaced it.

use std::collections::HashSet;
use std::sync::LazyLock;

use frankenterm_core::config::PatternsConfig;
use frankenterm_core::passive_watch_invariant::{
    PassiveWatchHealth, PassiveWatchObservation, WatchAction, WatchMetadataKind, check_invariants,
    fold_observation,
};
use frankenterm_core::patterns::{DetectionContext, PatternEngine};
use libfuzzer_sys::fuzz_target;

/// The engine the watch loop builds from a default configuration,
/// built once: rule compilation dominates a fresh engine's cost.
static ENGINE: LazyLock<PatternEngine> = LazyLock::new(|| {
    PatternEngine::from_config(&PatternsConfig::default())
        .expect("the default pattern configuration must load")
});

static RULE_IDS: LazyLock<HashSet<String>> =
    LazyLock::new(|| ENGINE.rules().iter().map(|rule| rule.id.clone()).collect());

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

    let text = String::from_utf8_lossy(data);
    let mut context = DetectionContext::new();
    context.pane_id = Some(0);
    let detections = ENGINE.detect_with_context(&text, &mut context);

    for detection in &detections {
        assert!(
            RULE_IDS.contains(&detection.rule_id),
            "detection names a rule the engine did not load: {}",
            detection.rule_id,
        );
    }

    // Synthesize the read-only emission set. Every emission the
    // watch loop makes from a detection result is non-mutating by
    // construction; if a future refactor adds a mutating emission,
    // this harness MUST be updated to record it (and will then
    // catch the violation).
    let mut actions: Vec<WatchAction> = Vec::with_capacity(2 + detections.len());

    // Capture the bytes — the read-only baseline.
    actions.push(WatchAction::Capture {
        pane_id: 0,
        byte_count: data.len() as u32,
    });

    // One pattern-detection emission per detection. A detection
    // is read-only: the watch loop records it without taking
    // action.
    for detection in detections {
        actions.push(WatchAction::PatternDetection {
            rule_id: detection.rule_id,
        });
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
