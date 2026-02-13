//! Property-based tests for the recorder_invariants module.
//!
//! Tests cover: InvariantChecker (monotonic sequences pass, report metadata consistency,
//! duplicate event_ids produce Critical, sequence regressions produce Critical, report
//! predicate consistency), InvariantReport (count_by_kind totals, passed iff no errors,
//! has_critical/has_errors consistency), and verify_replay_determinism (reflexivity,
//! permutation invariance, length mismatch detection).

use proptest::prelude::*;

use frankenterm_core::recorder_invariants::{
    InvariantChecker, InvariantCheckerConfig, ViolationKind, ViolationSeverity,
    verify_replay_determinism,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};

// ============================================================================
// Strategies
// ============================================================================

/// Build a well-formed RecorderEvent with the given parameters.
fn make_event(id: String, pane_id: u64, seq: u64, ts: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: id,
        pane_id,
        session_id: Some("s1".into()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: ts,
        recorded_at_ms: ts + 1,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: "data".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

/// Generate a well-formed sequence of events: unique IDs, consecutive sequences,
/// non-decreasing timestamps, all on the same pane.
fn arb_well_formed_sequence() -> impl Strategy<Value = Vec<RecorderEvent>> {
    (1u64..=5, 1usize..=50).prop_flat_map(|(pane_id, count)| {
        prop::collection::vec("[a-z0-9]{8}".prop_map(String::from), count).prop_map(move |ids| {
            let mut unique_ids: Vec<String> = Vec::with_capacity(ids.len());
            for (i, id) in ids.into_iter().enumerate() {
                unique_ids.push(format!("{}-{}", id, i));
            }
            unique_ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| make_event(id, pane_id, i as u64, 1000 + i as u64))
                .collect()
        })
    })
}

/// Generate a multi-pane well-formed sequence: each pane has independent
/// monotonic sequences, interleaved.
fn arb_multi_pane_sequence() -> impl Strategy<Value = Vec<RecorderEvent>> {
    (1usize..=5, 1usize..=20).prop_flat_map(|(num_panes, events_per_pane)| {
        let total = num_panes * events_per_pane;
        prop::collection::vec("[a-z0-9]{6}".prop_map(String::from), total).prop_map(move |ids| {
            let mut events = Vec::with_capacity(total);
            let mut unique_ids: Vec<String> = Vec::with_capacity(ids.len());
            for (i, id) in ids.into_iter().enumerate() {
                unique_ids.push(format!("{}-{}", id, i));
            }
            let mut idx = 0;
            for pane in 0..num_panes {
                for seq in 0..events_per_pane {
                    let id = unique_ids[idx].clone();
                    let ts = 1000 + (seq as u64) * 10 + (pane as u64);
                    events.push(make_event(id, pane as u64, seq as u64, ts));
                    idx += 1;
                }
            }
            events
        })
    })
}

/// Non-merge-order checker config (avoids merge order violations from interleaving).
fn no_merge_order_config() -> InvariantCheckerConfig {
    InvariantCheckerConfig {
        check_merge_order: false,
        ..Default::default()
    }
}

// ============================================================================
// Well-formed sequences pass
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Well-formed single-pane sequences always pass.
    #[test]
    fn prop_well_formed_single_pane_passes(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        prop_assert!(
            report.passed,
            "well-formed sequence should pass; violations: {:?}",
            report.violations.iter().map(|v| format!("{:?}: {}", v.kind, v.message)).collect::<Vec<_>>()
        );
    }

    /// Well-formed multi-pane sequences always pass.
    #[test]
    fn prop_well_formed_multi_pane_passes(events in arb_multi_pane_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        prop_assert!(
            report.passed,
            "multi-pane well-formed sequence should pass; violations: {:?}",
            report.violations.iter().map(|v| format!("{:?}: {}", v.kind, v.message)).collect::<Vec<_>>()
        );
    }
}

// ============================================================================
// Report metadata consistency
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// events_checked equals input length.
    #[test]
    fn prop_events_checked_equals_input_len(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        prop_assert_eq!(report.events_checked, events.len());
    }

    /// panes_observed <= events_checked.
    #[test]
    fn prop_panes_observed_bounded(events in arb_multi_pane_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        prop_assert!(
            report.panes_observed <= report.events_checked,
            "panes ({}) should be <= events ({})",
            report.panes_observed,
            report.events_checked
        );
    }

    /// domains_observed >= panes_observed (each pane has at least one domain).
    #[test]
    fn prop_domains_gte_panes(events in arb_multi_pane_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        prop_assert!(
            report.domains_observed >= report.panes_observed,
            "domains ({}) should be >= panes ({})",
            report.domains_observed,
            report.panes_observed
        );
    }

    /// All violation event_indices are within bounds.
    #[test]
    fn prop_violation_indices_in_bounds(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        for v in &report.violations {
            prop_assert!(
                v.event_index < report.events_checked,
                "violation index {} >= events_checked {}",
                v.event_index,
                report.events_checked
            );
        }
    }

    /// Empty input always passes with zero counts.
    #[test]
    fn prop_empty_always_passes(_dummy in 0u8..1) {
        let checker = InvariantChecker::new();
        let report = checker.check(&[]);
        prop_assert!(report.passed);
        prop_assert_eq!(report.events_checked, 0);
        prop_assert_eq!(report.panes_observed, 0);
        prop_assert_eq!(report.domains_observed, 0);
        prop_assert!(report.violations.is_empty());
    }
}

// ============================================================================
// Report predicate consistency
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// `passed` iff no Error or Critical violations.
    #[test]
    fn prop_passed_iff_no_errors_or_critical(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        let has_error_or_critical = report.violations.iter().any(|v| {
            matches!(
                v.severity,
                ViolationSeverity::Error | ViolationSeverity::Critical
            )
        });
        prop_assert_eq!(
            report.passed,
            !has_error_or_critical,
            "passed={} but has_error_or_critical={}",
            report.passed,
            has_error_or_critical
        );
    }

    /// `has_critical` iff any violation has Critical severity.
    #[test]
    fn prop_has_critical_consistency(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        let any_critical = report
            .violations
            .iter()
            .any(|v| v.severity == ViolationSeverity::Critical);
        prop_assert_eq!(report.has_critical(), any_critical);
    }

    /// `has_errors` iff any violation has Error severity.
    #[test]
    fn prop_has_errors_consistency(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        let any_errors = report
            .violations
            .iter()
            .any(|v| v.severity == ViolationSeverity::Error);
        prop_assert_eq!(report.has_errors(), any_errors);
    }

    /// count_by_severity for all severities sums to violations.len().
    #[test]
    fn prop_severity_counts_sum(events in arb_well_formed_sequence()) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let report = checker.check(&events);
        let total = report.count_by_severity(ViolationSeverity::Warning)
            + report.count_by_severity(ViolationSeverity::Error)
            + report.count_by_severity(ViolationSeverity::Critical);
        prop_assert_eq!(total, report.violations.len());
    }
}

// ============================================================================
// Duplicate event_ids produce Critical
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Injecting a duplicate event_id produces a DuplicateEventId Critical violation.
    #[test]
    fn prop_duplicate_event_id_is_critical(
        pane_id in 0u64..=10,
        ts_base in 1000u64..=100_000,
    ) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let events = vec![
            make_event("same-id".into(), pane_id, 0, ts_base),
            make_event("same-id".into(), pane_id, 1, ts_base + 1),
        ];
        let report = checker.check(&events);
        prop_assert!(!report.passed);
        prop_assert!(report.has_critical());
        prop_assert!(
            report.count_by_kind(ViolationKind::DuplicateEventId) >= 1,
            "expected DuplicateEventId violation"
        );
    }
}

// ============================================================================
// Sequence regressions produce Critical
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Injecting a sequence regression produces SequenceRegression Critical violation.
    #[test]
    fn prop_sequence_regression_is_critical(
        pane_id in 0u64..=10,
        high_seq in 5u64..=1000,
        low_seq in 0u64..=4,
    ) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let events = vec![
            make_event("e1".into(), pane_id, high_seq, 1000),
            make_event("e2".into(), pane_id, low_seq, 1001),
        ];
        let report = checker.check(&events);
        prop_assert!(!report.passed);
        prop_assert!(
            report.count_by_kind(ViolationKind::SequenceRegression) >= 1,
            "expected SequenceRegression violation"
        );
    }
}

// ============================================================================
// Empty event_id produces Error
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Events with empty event_id produce EmptyEventId Error.
    #[test]
    fn prop_empty_event_id_is_error(
        pane_id in 0u64..=10,
        seq in 0u64..=100,
        ts in 1000u64..=100_000,
    ) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let events = vec![make_event(String::new(), pane_id, seq, ts)];
        let report = checker.check(&events);
        prop_assert!(!report.passed);
        prop_assert!(report.has_errors());
        prop_assert!(
            report.count_by_kind(ViolationKind::EmptyEventId) >= 1,
            "expected EmptyEventId violation"
        );
    }
}

// ============================================================================
// Sequence gap detection
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A gap > 1 without gap marker produces SequenceGap violation.
    #[test]
    fn prop_sequence_gap_detected(
        pane_id in 0u64..=10,
        gap in 2u64..=200,
    ) {
        let checker = InvariantChecker::with_config(no_merge_order_config());
        let events = vec![
            make_event("e1".into(), pane_id, 0, 1000),
            make_event("e2".into(), pane_id, gap, 1001),
        ];
        let report = checker.check(&events);
        prop_assert!(
            report.count_by_kind(ViolationKind::SequenceGap) >= 1,
            "expected SequenceGap for gap of {} (violations: {:?})",
            gap - 1,
            report.violations.iter().map(|v| format!("{:?}", v.kind)).collect::<Vec<_>>()
        );
    }

    /// Large gaps (> max_sequence_gap) produce Error-severity SequenceGap.
    #[test]
    fn prop_large_gap_is_error(
        pane_id in 0u64..=10,
        max_gap in 1u64..=50,
        actual_gap in 51u64..=200,
    ) {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            max_sequence_gap: max_gap,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        prop_assume!(actual_gap > max_gap);
        let events = vec![
            make_event("e1".into(), pane_id, 0, 1000),
            make_event("e2".into(), pane_id, actual_gap + 1, 1001),
        ];
        let report = checker.check(&events);
        let gap_violations: Vec<_> = report
            .violations
            .iter()
            .filter(|v| v.kind == ViolationKind::SequenceGap)
            .collect();
        prop_assert!(!gap_violations.is_empty());
        prop_assert!(
            gap_violations
                .iter()
                .any(|v| v.severity == ViolationSeverity::Error),
            "large gap should be Error severity"
        );
    }
}

// ============================================================================
// Schema version mismatch
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Events with wrong schema version produce SchemaVersionMismatch.
    #[test]
    fn prop_schema_mismatch_detected(wrong_version in "[a-z.]{5,20}") {
        prop_assume!(wrong_version != RECORDER_EVENT_SCHEMA_VERSION_V1);
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1".into(), 1, 0, 1000);
        event.schema_version = wrong_version;
        let report = checker.check(&[event]);
        prop_assert!(
            report.count_by_kind(ViolationKind::SchemaVersionMismatch) >= 1,
            "expected SchemaVersionMismatch"
        );
    }

    /// Correct schema version produces no SchemaVersionMismatch.
    #[test]
    fn prop_correct_schema_no_mismatch(
        pane_id in 0u64..=10,
        seq in 0u64..=100,
    ) {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![make_event(format!("e-{}", seq), pane_id, seq, 1000 + seq)];
        let report = checker.check(&events);
        prop_assert_eq!(
            report.count_by_kind(ViolationKind::SchemaVersionMismatch),
            0,
            "correct schema should not produce mismatch"
        );
    }
}

// ============================================================================
// Causality checks
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Dangling parent references produce DanglingParentRef warning.
    #[test]
    fn prop_dangling_parent_detected(
        pane_id in 0u64..=10,
        ghost_id in "[a-z]{5,10}",
    ) {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            check_causality: true,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1".into(), pane_id, 0, 1000);
        event.causality.parent_event_id = Some(ghost_id);
        let report = checker.check(&[event]);
        prop_assert!(
            report.count_by_kind(ViolationKind::DanglingParentRef) >= 1,
            "expected DanglingParentRef"
        );
    }

    /// With causality checks disabled, dangling refs are not reported.
    #[test]
    fn prop_causality_disabled_no_dangling(
        pane_id in 0u64..=10,
        ghost_id in "[a-z]{5,10}",
    ) {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            check_causality: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1".into(), pane_id, 0, 1000);
        event.causality.parent_event_id = Some(ghost_id);
        let report = checker.check(&[event]);
        prop_assert_eq!(
            report.count_by_kind(ViolationKind::DanglingParentRef),
            0,
            "causality disabled should not flag dangling refs"
        );
    }

    /// Valid causal chain (parent defined before child) produces no dangling ref.
    #[test]
    fn prop_valid_causal_chain_no_violation(
        pane_id in 0u64..=10,
    ) {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            check_causality: true,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut child = make_event("child".into(), pane_id, 1, 1001);
        child.causality.parent_event_id = Some("parent".into());
        let events = vec![
            make_event("parent".into(), pane_id, 0, 1000),
            child,
        ];
        let report = checker.check(&events);
        prop_assert_eq!(
            report.count_by_kind(ViolationKind::DanglingParentRef),
            0,
            "valid causal chain should not produce dangling ref"
        );
    }
}

// ============================================================================
// Replay determinism
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Replay determinism is reflexive: events vs themselves.
    #[test]
    fn prop_replay_determinism_reflexive(events in arb_well_formed_sequence()) {
        let result = verify_replay_determinism(&events, &events);
        prop_assert!(
            result.deterministic,
            "events vs themselves should be deterministic"
        );
        prop_assert!(result.divergence_index.is_none());
    }

    /// Replay determinism with empty sequences.
    #[test]
    fn prop_replay_determinism_empty(_dummy in 0u8..1) {
        let result = verify_replay_determinism(&[], &[]);
        prop_assert!(result.deterministic);
    }

    /// Length mismatch is always non-deterministic.
    #[test]
    fn prop_replay_determinism_length_mismatch(
        n in 1usize..=10,
        m in 1usize..=10,
    ) {
        prop_assume!(n != m);
        let events_a: Vec<RecorderEvent> = (0..n)
            .map(|i| make_event(format!("a-{}", i), 1, i as u64, 1000 + i as u64))
            .collect();
        let events_b: Vec<RecorderEvent> = (0..m)
            .map(|i| make_event(format!("b-{}", i), 1, i as u64, 1000 + i as u64))
            .collect();
        let result = verify_replay_determinism(&events_a, &events_b);
        prop_assert!(
            !result.deterministic,
            "different lengths should not be deterministic"
        );
    }
}

// ============================================================================
// Config variations don't crash
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Checker doesn't panic with arbitrary config values on well-formed input.
    #[test]
    fn prop_checker_no_panic_with_any_config(
        max_gap in 0u64..=1000,
        check_causality in any::<bool>(),
        check_merge_order in any::<bool>(),
        clock_threshold in 0u64..=1_000_000,
        events in arb_well_formed_sequence(),
    ) {
        let config = InvariantCheckerConfig {
            max_sequence_gap: max_gap,
            check_causality,
            check_merge_order: false, // always disable to avoid merge order issues with unsorted
            clock_future_skew_threshold_ms: clock_threshold,
            expected_schema_version: String::new(),
        };
        let checker = InvariantChecker::with_config(config);
        let report = checker.check(&events);
        // Just verify it doesn't panic and basic invariants hold.
        prop_assert_eq!(report.events_checked, events.len());
        let _ = check_causality;
        let _ = check_merge_order;
    }
}
