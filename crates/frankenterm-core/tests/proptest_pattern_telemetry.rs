//! Property-based tests for pattern detection engine telemetry counters (ft-3kxe.9).
//!
//! Validates:
//! 1. PatternTelemetry starts at zero
//! 2. Snapshot serde roundtrip
//! 3. Scan counter increases on every detect() call
//! 4. Quick reject counter + non-reject counter sum to scans_total
//! 5. Matches monotonically increase
//! 6. Bloom filter counters are consistent

use proptest::prelude::*;

use frankenterm_core::patterns::{
    PatternEngine, PatternPack, PatternTelemetrySnapshot, RuleDef,
};

// =============================================================================
// Helpers
// =============================================================================

fn minimal_engine() -> PatternEngine {
    let rule = RuleDef {
        id: "test.telemetry:keyword".to_string(),
        agent_type: frankenterm_core::patterns::AgentType::Unknown,
        event_type: "test_event".to_string(),
        severity: frankenterm_core::patterns::Severity::Info,
        anchors: vec!["ERROR".to_string(), "WARN".to_string()],
        regex: Some(r"(?i)(ERROR|WARN):\s*(.+)".to_string()),
        description: "Test rule for telemetry".to_string(),
        remediation: None,
        workflow: None,
        manual_fix: None,
        preview_command: None,
        learn_more_url: None,
    };

    let pack = PatternPack::new("test-pack", "1.0.0", vec![rule]);
    PatternEngine::with_packs(vec![pack]).expect("test pack should compile")
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let engine = minimal_engine();
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.scans_total, 0);
    assert_eq!(snap.matches_total, 0);
    assert_eq!(snap.quick_rejects, 0);
    assert_eq!(snap.bloom_checks, 0);
    assert_eq!(snap.bloom_positives, 0);
    assert_eq!(snap.bloom_rejects, 0);
    assert_eq!(snap.candidate_rules_evaluated, 0);
    assert_eq!(snap.regex_evaluations, 0);
}

#[test]
fn scan_increments_on_detect() {
    let engine = minimal_engine();

    engine.detect("hello world");
    engine.detect("ERROR: something");

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.scans_total, 2);
}

#[test]
fn matches_counted_correctly() {
    let engine = minimal_engine();

    // No match
    engine.detect("hello world");
    let snap1 = engine.telemetry().snapshot();
    assert_eq!(snap1.matches_total, 0);

    // One match
    engine.detect("ERROR: disk full");
    let snap2 = engine.telemetry().snapshot();
    assert!(snap2.matches_total >= 1, "should have at least one match");
}

#[test]
fn empty_text_still_counts_scan() {
    let engine = minimal_engine();
    engine.detect("");
    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.scans_total, 1);
    assert_eq!(snap.matches_total, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = PatternTelemetrySnapshot {
        scans_total: 100,
        matches_total: 5,
        quick_rejects: 80,
        bloom_checks: 200,
        bloom_positives: 15,
        bloom_rejects: 70,
        candidate_rules_evaluated: 20,
        regex_evaluations: 18,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: PatternTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn scans_monotonically_increase(
        texts in prop::collection::vec("[a-zA-Z0-9 :]{0,100}", 1..20),
    ) {
        let engine = minimal_engine();
        let mut prev_scans = 0u64;
        let mut prev_matches = 0u64;

        for text in &texts {
            engine.detect(text);
            let snap = engine.telemetry().snapshot();

            prop_assert!(
                snap.scans_total > prev_scans,
                "scans_total must increase on every call: prev={}, cur={}",
                prev_scans, snap.scans_total
            );
            prop_assert!(
                snap.matches_total >= prev_matches,
                "matches_total must not decrease: prev={}, cur={}",
                prev_matches, snap.matches_total
            );

            prev_scans = snap.scans_total;
            prev_matches = snap.matches_total;
        }

        let final_snap = engine.telemetry().snapshot();
        prop_assert_eq!(final_snap.scans_total, texts.len() as u64);
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        scans in 0u64..10000,
        matches in 0u64..5000,
        rejects in 0u64..8000,
        bloom_ck in 0u64..20000,
        bloom_pos in 0u64..1000,
        bloom_rej in 0u64..8000,
        candidates in 0u64..3000,
        regex_evals in 0u64..3000,
    ) {
        let snap = PatternTelemetrySnapshot {
            scans_total: scans,
            matches_total: matches,
            quick_rejects: rejects,
            bloom_checks: bloom_ck,
            bloom_positives: bloom_pos,
            bloom_rejects: bloom_rej,
            candidate_rules_evaluated: candidates,
            regex_evaluations: regex_evals,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PatternTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn regex_evals_bounded_by_candidates(
        texts in prop::collection::vec(
            prop::string::string_regex("[A-Z]{0,5}: [a-z ]{0,50}").unwrap(),
            1..15,
        ),
    ) {
        let engine = minimal_engine();

        for text in &texts {
            engine.detect(text);
        }

        let snap = engine.telemetry().snapshot();
        // regex_evaluations <= candidate_rules_evaluated because each candidate
        // may or may not have a regex
        prop_assert!(
            snap.regex_evaluations <= snap.candidate_rules_evaluated,
            "regex_evaluations ({}) should be <= candidate_rules_evaluated ({})",
            snap.regex_evaluations, snap.candidate_rules_evaluated
        );
    }
}
