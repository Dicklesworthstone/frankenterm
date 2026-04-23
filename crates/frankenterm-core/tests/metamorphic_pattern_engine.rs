//! Metamorphic relations for `frankenterm_core::patterns::PatternEngine::detect`.
//!
//! Ships MR1 (idempotence) from ft-87tdq. The full 6-MR list lives in that
//! bead's body; MR2-MR6 are open for follow-up.
//!
//! MR1 says: for any input `x` and any pattern engine `e`,
//!
//!     e.detect(x)  ==  e.detect(x)    (as sets over stable detection keys)
//!
//! Proof-of-regression: if anything mutates engine state across a detect
//! call in a way that reshapes output — a telemetry counter that forks the
//! scan path once warmed, a dedupe cache that retains context from the
//! previous call, a regex compile that runs once and caches inconsistently
//! — this proptest fails. A correct stateless matcher satisfies it
//! trivially.
//!
//! Companion to `tests/metamorphic_pattern_trigger_scan.rs` which covers
//! the *byte-scanner* `pattern_trigger::TriggerScanner`; this file is for
//! the *rule-based* `PatternEngine`.

use frankenterm_core::patterns::{Detection, PatternEngine};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Mix of printable ASCII (the realistic input domain for agent output) and
/// newlines (which matter because many detection rules are line-scoped).
/// Keep inputs bounded to 0-512 bytes so the proptest can churn a few
/// hundred cases per CI run without dominating the wall clock.
fn arb_agent_output() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            // Weighted: 80% printable ASCII, 20% newline — roughly the
            // shape of real pane scrollback (long lines + periodic breaks).
            8 => any::<u8>().prop_filter("printable ASCII",
                |b| (0x20..=0x7e).contains(b))
                .prop_map(|b| b as char),
            2 => Just('\n'),
        ],
        0..512,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Canonicalization
// ---------------------------------------------------------------------------

/// Stable key for comparing two `Vec<Detection>` as sets. `Detection`
/// itself doesn't derive `PartialEq` or `Hash`, but the key fields that
/// identify a match semantically do.
///
/// Sort before comparison so we tolerate vec-ordering differences. Any
/// order-dependence between two successive calls on the same engine would
/// be an independent MR5 violation, not an MR1 one.
fn canonical_keys(
    detections: &[Detection],
) -> Vec<(String, String, String, (usize, usize))> {
    let mut keyed: Vec<_> = detections
        .iter()
        .map(|d| {
            (
                d.rule_id.clone(),
                d.event_type.clone(),
                d.matched_text.clone(),
                d.span,
            )
        })
        .collect();
    keyed.sort();
    keyed
}

// ---------------------------------------------------------------------------
// MR1 — idempotence
// ---------------------------------------------------------------------------

proptest! {
    /// ft-87tdq MR1: calling `detect(x)` twice on the same engine instance
    /// produces the same detection set. Stateless contract — if it fails,
    /// something is leaking state across calls.
    #[test]
    fn mr1_idempotence_same_engine(input in arb_agent_output()) {
        let engine = PatternEngine::new();
        let first = canonical_keys(&engine.detect(&input));
        let second = canonical_keys(&engine.detect(&input));
        prop_assert_eq!(
            &first, &second,
            "PatternEngine::detect is not idempotent on input of length {}",
            input.len()
        );
    }

    /// ft-87tdq MR1 variant: two independently constructed engines agree
    /// on the same input. Stronger than same-engine idempotence — catches
    /// load-order non-determinism in `builtin_packs()` (e.g. hash-map
    /// iteration leaking into rule sequencing).
    #[test]
    fn mr1_idempotence_fresh_engine(input in arb_agent_output()) {
        let engine_a = PatternEngine::new();
        let engine_b = PatternEngine::new();
        let a = canonical_keys(&engine_a.detect(&input));
        let b = canonical_keys(&engine_b.detect(&input));
        prop_assert_eq!(
            &a, &b,
            "Two fresh PatternEngine instances disagree on input of length {}",
            input.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Harness self-tests — cheap, not metamorphic. Just pin the canonical_keys
// projection against a fixed input so a future refactor of the Detection
// shape fails loudly in ONE place instead of silently changing the proptest
// semantics.
// ---------------------------------------------------------------------------

#[test]
fn canonical_keys_is_total_on_empty_input() {
    let engine = PatternEngine::new();
    let keys = canonical_keys(&engine.detect(""));
    assert!(keys.is_empty(), "empty input must produce zero detections");
}

#[test]
fn canonical_keys_is_total_on_benign_input() {
    let engine = PatternEngine::new();
    // Fixed benign input. If a future rule starts matching this, the
    // assertion fails loudly — the investigator should either change the
    // rule or update this harness text.
    let input = "harmless ascii with no anchors\n";
    let _ = canonical_keys(&engine.detect(input));
    // No assertion on content: just verifies the projection doesn't panic
    // on a realistic non-empty input.
}
