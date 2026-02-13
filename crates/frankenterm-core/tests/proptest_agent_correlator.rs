//! Property-based tests for the `agent_correlator` module.
//!
//! Covers `DetectionSource` serde roundtrips, snake_case serialization,
//! and `AgentCorrelator` basic lifecycle properties (new, tracked count,
//! remove_pane).

use frankenterm_core::agent_correlator::{AgentCorrelator, DetectionSource};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_detection_source() -> impl Strategy<Value = DetectionSource> {
    prop_oneof![
        Just(DetectionSource::PatternEngine),
        Just(DetectionSource::PaneTitle),
        Just(DetectionSource::ProcessName),
    ]
}

// =========================================================================
// DetectionSource — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// DetectionSource serde roundtrip.
    #[test]
    fn prop_detection_source_serde(source in arb_detection_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let back: DetectionSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, source);
    }

    /// DetectionSource serializes to snake_case.
    #[test]
    fn prop_detection_source_snake_case(source in arb_detection_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let expected = match source {
            DetectionSource::PatternEngine => "\"pattern_engine\"",
            DetectionSource::PaneTitle => "\"pane_title\"",
            DetectionSource::ProcessName => "\"process_name\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// DetectionSource serde is deterministic.
    #[test]
    fn prop_detection_source_deterministic(source in arb_detection_source()) {
        let j1 = serde_json::to_string(&source).unwrap();
        let j2 = serde_json::to_string(&source).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// AgentCorrelator — lifecycle properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// New correlator starts with zero tracked panes.
    #[test]
    fn prop_new_correlator_empty(_dummy in 0..1_u8) {
        let correlator = AgentCorrelator::new();
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
    }

    /// Default correlator is the same as new().
    #[test]
    fn prop_default_is_new(_dummy in 0..1_u8) {
        let a = AgentCorrelator::new();
        let b = AgentCorrelator::default();
        prop_assert_eq!(a.tracked_pane_count(), b.tracked_pane_count());
    }

    /// get_metadata returns None for any pane ID on a fresh correlator.
    #[test]
    fn prop_fresh_correlator_no_metadata(pane_id in 0_u64..10_000) {
        let correlator = AgentCorrelator::new();
        prop_assert!(correlator.get_metadata(pane_id).is_none());
    }

    /// remove_pane on a fresh correlator is a no-op.
    #[test]
    fn prop_remove_pane_noop_on_fresh(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.remove_pane(pane_id);
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn detection_source_variants_distinct() {
    assert_ne!(DetectionSource::PatternEngine, DetectionSource::PaneTitle);
    assert_ne!(DetectionSource::PatternEngine, DetectionSource::ProcessName);
    assert_ne!(DetectionSource::PaneTitle, DetectionSource::ProcessName);
}

#[test]
fn correlator_new_and_default() {
    let a = AgentCorrelator::new();
    let b = AgentCorrelator::default();
    assert_eq!(a.tracked_pane_count(), 0);
    assert_eq!(b.tracked_pane_count(), 0);
}
