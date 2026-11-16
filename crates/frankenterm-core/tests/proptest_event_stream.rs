//! Property tests for event_stream module.

use proptest::prelude::*;

use frankenterm_core::event_stream::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_severity_level() -> impl Strategy<Value = SeverityLevel> {
    prop_oneof![
        Just(SeverityLevel::Info),
        Just(SeverityLevel::Warning),
        Just(SeverityLevel::Critical),
    ]
}

fn arb_wait_condition_leaf() -> impl Strategy<Value = WaitCondition> {
    prop_oneof![
        Just(WaitCondition::AnyEvent),
        "[a-z.]{3,20}".prop_map(|s| WaitCondition::RuleId { rule_id: s }),
        any::<u64>().prop_map(|p| WaitCondition::PaneDetection { pane_id: p }),
        proptest::option::of(any::<u64>())
            .prop_map(|p| WaitCondition::PaneDiscovered { pane_id: p }),
        any::<u64>().prop_map(|p| WaitCondition::PaneDisappeared { pane_id: p }),
        proptest::option::of("[a-z]{3,10}".prop_map(|s| s))
            .prop_map(|w| WaitCondition::WorkflowCompleted { workflow_id: w }),
    ]
}

// =============================================================================
// StreamCursor tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_stream_cursor(id in any::<i64>()) {
        let cursor = StreamCursor::after_id(id);
        let json = serde_json::to_string(&cursor).unwrap();
        let back: StreamCursor = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cursor, back);
    }

    #[test]
    fn cursor_from_beginning_is_zero(_dummy in Just(())) {
        let cursor = StreamCursor::from_beginning();
        prop_assert_eq!(cursor.after_id, 0);
        prop_assert!(cursor.correlation_id.is_none());
    }

    #[test]
    fn cursor_default_is_from_beginning(_dummy in Just(())) {
        let cursor = StreamCursor::default();
        let beginning = StreamCursor::from_beginning();
        prop_assert_eq!(cursor, beginning);
    }

    #[test]
    fn cursor_advance_monotonic(start in 0i64..1000, advances in proptest::collection::vec(0i64..2000, 1..10)) {
        let mut cursor = StreamCursor::after_id(start);
        let mut prev = cursor.after_id;
        for new_id in advances {
            cursor.advance(new_id);
            prop_assert!(cursor.after_id >= prev, "cursor went backwards: {} < {}", cursor.after_id, prev);
            prev = cursor.after_id;
        }
    }

    #[test]
    fn cursor_advance_ignores_smaller(start in 100i64..1000, smaller in 0i64..100) {
        let mut cursor = StreamCursor::after_id(start);
        cursor.advance(smaller);
        prop_assert_eq!(cursor.after_id, start);
    }

    #[test]
    fn cursor_advance_accepts_larger(start in 0i64..100, larger in 100i64..1000) {
        let mut cursor = StreamCursor::after_id(start);
        cursor.advance(larger);
        prop_assert_eq!(cursor.after_id, larger);
    }

    #[test]
    fn cursor_with_correlation_id(id in any::<i64>(), corr in "[a-z]{5,10}") {
        let cursor = StreamCursor::after_id(id).with_correlation_id(corr.clone());
        prop_assert_eq!(cursor.correlation_id, Some(corr));
        prop_assert_eq!(cursor.after_id, id);
    }
}

// =============================================================================
// SeverityLevel tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_severity_level(s in arb_severity_level()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: SeverityLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn severity_level_ordering(_dummy in Just(())) {
        prop_assert!(SeverityLevel::Info < SeverityLevel::Warning);
        prop_assert!(SeverityLevel::Warning < SeverityLevel::Critical);
    }

    #[test]
    fn severity_level_from_str_loose_valid(_dummy in Just(())) {
        prop_assert_eq!(SeverityLevel::from_str_loose("info"), Some(SeverityLevel::Info));
        prop_assert_eq!(SeverityLevel::from_str_loose("informational"), Some(SeverityLevel::Info));
        prop_assert_eq!(SeverityLevel::from_str_loose("warning"), Some(SeverityLevel::Warning));
        prop_assert_eq!(SeverityLevel::from_str_loose("warn"), Some(SeverityLevel::Warning));
        prop_assert_eq!(SeverityLevel::from_str_loose("critical"), Some(SeverityLevel::Critical));
        prop_assert_eq!(SeverityLevel::from_str_loose("crit"), Some(SeverityLevel::Critical));
        prop_assert_eq!(SeverityLevel::from_str_loose("error"), Some(SeverityLevel::Critical));
    }

    #[test]
    fn severity_level_from_str_loose_case_insensitive(_dummy in Just(())) {
        prop_assert_eq!(SeverityLevel::from_str_loose("INFO"), Some(SeverityLevel::Info));
        prop_assert_eq!(SeverityLevel::from_str_loose("Warning"), Some(SeverityLevel::Warning));
        prop_assert_eq!(SeverityLevel::from_str_loose("CRITICAL"), Some(SeverityLevel::Critical));
    }

    #[test]
    fn severity_level_from_str_loose_invalid(s in "[0-9]{5,10}") {
        prop_assert_eq!(SeverityLevel::from_str_loose(&s), None);
    }
}

// =============================================================================
// EventStreamFilter tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_filter(_dummy in Just(())) {
        let filter = EventStreamFilter::builder()
            .pane_id(42)
            .rule_id("test.rule".into())
            .event_types(vec!["pattern_detected".into()])
            .min_severity(SeverityLevel::Warning)
            .unhandled_only()
            .since_ms(1000)
            .until_ms(2000)
            .build();
        let json = serde_json::to_string(&filter).unwrap();
        let back: EventStreamFilter = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(filter.pane_ids.len(), back.pane_ids.len());
        prop_assert_eq!(filter.rule_ids.len(), back.rule_ids.len());
        prop_assert_eq!(filter.event_types.len(), back.event_types.len());
        prop_assert_eq!(filter.unhandled_only, back.unhandled_only);
    }

    #[test]
    fn default_filter_is_empty(_dummy in Just(())) {
        let filter = EventStreamFilter::default();
        prop_assert!(filter.is_empty());
    }

    #[test]
    fn filter_with_pane_id_not_empty(pane_id in any::<u64>()) {
        let filter = EventStreamFilter::builder().pane_id(pane_id).build();
        prop_assert!(!filter.is_empty());
    }

    #[test]
    fn filter_with_rule_id_not_empty(rule in "[a-z.]{3,10}") {
        let filter = EventStreamFilter::builder().rule_id(rule).build();
        prop_assert!(!filter.is_empty());
    }

    #[test]
    fn filter_with_severity_not_empty(sev in arb_severity_level()) {
        let filter = EventStreamFilter::builder().min_severity(sev).build();
        prop_assert!(!filter.is_empty());
    }

    #[test]
    fn filter_with_since_not_empty(ms in any::<i64>()) {
        let filter = EventStreamFilter::builder().since_ms(ms).build();
        prop_assert!(!filter.is_empty());
    }

    #[test]
    fn filter_with_until_not_empty(ms in any::<i64>()) {
        let filter = EventStreamFilter::builder().until_ms(ms).build();
        prop_assert!(!filter.is_empty());
    }

    #[test]
    fn filter_unhandled_only_not_empty(_dummy in Just(())) {
        let filter = EventStreamFilter::builder().unhandled_only().build();
        prop_assert!(!filter.is_empty());
    }
}

// =============================================================================
// WaitCondition serde tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_wait_condition_leaf(wc in arb_wait_condition_leaf()) {
        let json = serde_json::to_string(&wc).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        // Verify roundtrip by re-serializing
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn serde_roundtrip_any_event(_dummy in Just(())) {
        let wc = WaitCondition::AnyEvent;
        let json = serde_json::to_string(&wc).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }
}

// =============================================================================
// WaitCondition constructors
// =============================================================================

proptest! {
    #[test]
    fn wait_condition_rule_id_constructor(id in "[a-z.]{3,15}") {
        let wc = WaitCondition::rule_id(&id);
        let is_rule_id = matches!(wc, WaitCondition::RuleId { rule_id } if rule_id == id);
        prop_assert!(is_rule_id);
    }

    #[test]
    fn wait_condition_pane_detection_constructor(pane_id in any::<u64>()) {
        let wc = WaitCondition::pane_detection(pane_id);
        let is_pane_det = matches!(wc, WaitCondition::PaneDetection { pane_id: p } if p == pane_id);
        prop_assert!(is_pane_det);
    }

    #[test]
    fn wait_condition_pane_discovered_constructor(pane_id in proptest::option::of(any::<u64>())) {
        let wc = WaitCondition::pane_discovered(pane_id);
        let is_pane_disc = matches!(wc, WaitCondition::PaneDiscovered { pane_id: p } if p == pane_id);
        prop_assert!(is_pane_disc);
    }

    #[test]
    fn wait_condition_workflow_completed_constructor(wf_id in proptest::option::of("[a-z]{3,10}".prop_map(|s| s))) {
        let wc = WaitCondition::workflow_completed(wf_id.clone());
        let is_wf = matches!(wc, WaitCondition::WorkflowCompleted { workflow_id: w } if w == wf_id);
        prop_assert!(is_wf);
    }
}

// =============================================================================
// Builder tests
// =============================================================================

proptest! {
    #[test]
    fn builder_pane_ids(ids in proptest::collection::vec(any::<u64>(), 0..5)) {
        let filter = EventStreamFilter::builder().pane_ids(ids.clone()).build();
        prop_assert_eq!(filter.pane_ids, ids);
    }

    #[test]
    fn builder_rule_ids(ids in proptest::collection::vec("[a-z.]{3,10}".prop_map(|s| s), 0..5)) {
        let filter = EventStreamFilter::builder().rule_ids(ids.clone()).build();
        prop_assert_eq!(filter.rule_ids, ids);
    }

    #[test]
    fn builder_event_types(types in proptest::collection::vec("[a-z_]{5,15}".prop_map(|s| s), 0..5)) {
        let filter = EventStreamFilter::builder().event_types(types.clone()).build();
        prop_assert_eq!(filter.event_types, types);
    }
}
