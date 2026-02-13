//! Property-based tests for events module
//!
//! Tests invariants for Event enum (serde roundtrip, type_name, pane_id),
//! UserVarPayload (serde, decode), MetricsSnapshot/EventBusStats (serde),
//! EventDeduplicator (check invariants, capacity, independence),
//! NotificationCooldown (check invariants, capacity, independence),
//! match_rule_glob (exact, wildcard, determinism),
//! EventFilter (allow_all, permissive, exclude precedence),
//! event_identity_key (determinism, format, pane differentiation),
//! NotificationGate (pipeline ordering).

use frankenterm_core::events::*;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use proptest::prelude::*;
use std::time::Duration;

// ============================================================================
// Strategies
// ============================================================================

/// Generate arbitrary AgentType
fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
        Just(AgentType::Wezterm),
        Just(AgentType::Unknown),
    ]
}

/// Generate arbitrary Severity
fn arb_severity() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Info),
        Just(Severity::Warning),
        Just(Severity::Critical),
    ]
}

/// Generate arbitrary Detection
fn arb_detection() -> impl Strategy<Value = Detection> {
    (
        "[a-z.]{1,20}:[a-z_]{1,20}",
        arb_agent_type(),
        "[a-z_]{1,15}",
        arb_severity(),
        0.0..=1.0f64,
        "[a-zA-Z0-9 ]{0,30}",
    )
        .prop_map(
            |(rule_id, agent_type, event_type, severity, confidence, matched_text)| Detection {
                rule_id,
                agent_type,
                event_type,
                severity,
                confidence,
                extracted: serde_json::json!({}),
                matched_text,
                span: (0, 0),
            },
        )
}

/// Generate arbitrary UserVarPayload
fn arb_user_var_payload() -> impl Strategy<Value = UserVarPayload> {
    ("[a-zA-Z0-9_=-]{0,50}", proptest::option::of("[a-z_]{1,20}")).prop_map(
        |(value, event_type)| UserVarPayload {
            value,
            event_type,
            event_data: None,
        },
    )
}

/// Generate arbitrary Event
fn arb_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        // SegmentCaptured
        (0..1000u64, 0..10000u64, 0..100000usize).prop_map(|(pane_id, seq, content_len)| {
            Event::SegmentCaptured {
                pane_id,
                seq,
                content_len,
            }
        }),
        // GapDetected
        (0..1000u64, "[a-z_ ]{1,30}")
            .prop_map(|(pane_id, reason)| Event::GapDetected { pane_id, reason }),
        // PatternDetected
        (0..1000u64, arb_detection()).prop_map(|(pane_id, detection)| Event::PatternDetected {
            pane_id,
            pane_uuid: None,
            detection,
            event_id: None,
        }),
        // PaneDiscovered
        (0..1000u64, "[a-z]{1,10}", "[a-zA-Z ]{1,20}").prop_map(|(pane_id, domain, title)| {
            Event::PaneDiscovered {
                pane_id,
                domain,
                title,
            }
        }),
        // PaneDisappeared
        (0..1000u64).prop_map(|pane_id| Event::PaneDisappeared { pane_id }),
        // WorkflowStarted
        ("[a-z0-9-]{1,15}", "[a-zA-Z ]{1,20}", 0..1000u64,).prop_map(
            |(workflow_id, workflow_name, pane_id)| Event::WorkflowStarted {
                workflow_id,
                workflow_name,
                pane_id,
            }
        ),
        // WorkflowStep
        ("[a-z0-9-]{1,15}", "[a-z_]{1,15}", "[a-z]{1,10}").prop_map(
            |(workflow_id, step_name, result)| Event::WorkflowStep {
                workflow_id,
                step_name,
                result,
            }
        ),
        // WorkflowCompleted
        (
            "[a-z0-9-]{1,15}",
            proptest::bool::ANY,
            proptest::option::of("[a-z ]{1,20}"),
        )
            .prop_map(|(workflow_id, success, reason)| Event::WorkflowCompleted {
                workflow_id,
                success,
                reason,
            }),
        // UserVarReceived
        (0..1000u64, "[A-Z_]{1,15}", arb_user_var_payload()).prop_map(
            |(pane_id, name, payload)| Event::UserVarReceived {
                pane_id,
                name,
                payload,
            }
        ),
    ]
}

/// Generate arbitrary MetricsSnapshot
fn arb_metrics_snapshot() -> impl Strategy<Value = MetricsSnapshot> {
    (0..100000u64, 0..10000u64, 0..100u64, 0..50000u64).prop_map(
        |(
            events_published,
            events_dropped_no_subscribers,
            active_subscribers,
            subscriber_lag_events,
        )| {
            MetricsSnapshot {
                events_published,
                events_dropped_no_subscribers,
                active_subscribers,
                subscriber_lag_events,
            }
        },
    )
}

/// Generate arbitrary EventBusStats
fn arb_event_bus_stats() -> impl Strategy<Value = EventBusStats> {
    (
        1..10000usize,
        0..1000usize,
        0..1000usize,
        0..1000usize,
        0..100usize,
        0..100usize,
        0..100usize,
        proptest::option::of(0..60000u64),
        proptest::option::of(0..60000u64),
        proptest::option::of(0..60000u64),
    )
        .prop_map(
            |(
                capacity,
                delta_queued,
                detection_queued,
                signal_queued,
                delta_subscribers,
                detection_subscribers,
                signal_subscribers,
                delta_oldest_lag_ms,
                detection_oldest_lag_ms,
                signal_oldest_lag_ms,
            )| {
                EventBusStats {
                    capacity,
                    delta_queued,
                    detection_queued,
                    signal_queued,
                    delta_subscribers,
                    detection_subscribers,
                    signal_subscribers,
                    delta_oldest_lag_ms,
                    detection_oldest_lag_ms,
                    signal_oldest_lag_ms,
                }
            },
        )
}

/// Generate a simple dedup key
fn arb_dedup_key() -> impl Strategy<Value = String> {
    "[a-z0-9_.-]{1,30}"
}

/// Generate a simple glob pattern
fn arb_glob_pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z.]{1,15}",                    // exact
        "[a-z.]{1,10}\\*".prop_map(|s| s), // suffix wildcard
        "\\*[a-z.]{1,10}".prop_map(|s| s), // prefix wildcard
        "[a-z]{1,5}\\.[a-z]{1,5}",         // dotted exact
    ]
}

// ============================================================================
// Property Tests: Event
// ============================================================================

proptest! {
    /// Property 1: Event serde roundtrip — serialized JSON deserializes back
    /// to the same variant with matching fields.
    #[test]
    fn prop_event_serde_roundtrip(event in arb_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        // Verify type_name is preserved
        prop_assert_eq!(event.type_name(), back.type_name(),
            "type_name mismatch after roundtrip");
        // Verify pane_id is preserved
        prop_assert_eq!(event.pane_id(), back.pane_id(),
            "pane_id mismatch after roundtrip");
    }

    /// Property 2: Event JSON contains the internally-tagged "type" field.
    #[test]
    fn prop_event_json_has_type_tag(event in arb_event()) {
        let json = serde_json::to_string(&event).unwrap();
        prop_assert!(json.contains("\"type\":"),
            "JSON should contain type tag, got: {}", json);
        // The type tag value should match type_name()
        let expected_tag = format!("\"type\":\"{}\"", event.type_name());
        prop_assert!(json.contains(&expected_tag),
            "JSON should contain {}, got: {}", expected_tag, json);
    }

    /// Property 3: Event type_name() is always non-empty snake_case.
    #[test]
    fn prop_event_type_name_snake_case(event in arb_event()) {
        let name = event.type_name();
        prop_assert!(!name.is_empty(), "type_name should not be empty");
        prop_assert!(!name.contains(char::is_uppercase),
            "type_name should be snake_case, got: {}", name);
        prop_assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "type_name should only contain lowercase letters and underscores, got: {}", name);
    }

    /// Property 4: Events with pane_id field return Some from pane_id().
    /// WorkflowStep and WorkflowCompleted return None.
    #[test]
    fn prop_event_pane_id_consistency(event in arb_event()) {
        let has_pane = event.pane_id().is_some();
        let name = event.type_name();
        match name {
            "workflow_step" | "workflow_completed" => {
                prop_assert!(!has_pane, "{} should have no pane_id", name);
            }
            _ => {
                prop_assert!(has_pane, "{} should have pane_id", name);
            }
        }
    }

    /// Property 5: Event type_name() is stable — calling it twice gives the same result.
    #[test]
    fn prop_event_type_name_stable(event in arb_event()) {
        prop_assert_eq!(event.type_name(), event.type_name(),
            "type_name should be stable across calls");
    }

    // ========================================================================
    // Property Tests: UserVarPayload
    // ========================================================================

    /// Property 6: UserVarPayload serde roundtrip.
    #[test]
    fn prop_user_var_payload_serde_roundtrip(payload in arb_user_var_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        let back: UserVarPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&payload.value, &back.value, "value mismatch");
        prop_assert_eq!(payload.event_type, back.event_type, "event_type mismatch");
    }

    /// Property 7: UserVarPayload::decode in lenient mode never fails.
    #[test]
    fn prop_user_var_decode_lenient_never_fails(s in "[a-zA-Z0-9+/=!@#$%]{0,100}") {
        let result = UserVarPayload::decode(&s, true);
        prop_assert!(result.is_ok(),
            "lenient decode should never fail, got: {:?}", result.err());
    }

    /// Property 8: UserVarPayload::decode preserves raw value.
    #[test]
    fn prop_user_var_decode_preserves_value(s in "[a-zA-Z0-9+/=]{0,50}") {
        // Lenient mode always succeeds and preserves value
        let payload = UserVarPayload::decode(&s, true).unwrap();
        prop_assert_eq!(&payload.value, &s, "value should be preserved");
    }

    /// Property 9: UserVarPayload::decode strict mode rejects empty string.
    #[test]
    fn prop_user_var_decode_strict_rejects_bad_input(s in "[!@#$%^&()]{1,20}") {
        // Non-base64 chars should fail in strict mode
        let result = UserVarPayload::decode(&s, false);
        prop_assert!(result.is_err(),
            "strict decode should fail for non-base64 input: {}", s);
    }

    // ========================================================================
    // Property Tests: MetricsSnapshot
    // ========================================================================

    /// Property 10: MetricsSnapshot serde roundtrip.
    #[test]
    fn prop_metrics_snapshot_serde_roundtrip(snap in arb_metrics_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: MetricsSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.events_published, back.events_published);
        prop_assert_eq!(snap.events_dropped_no_subscribers, back.events_dropped_no_subscribers);
        prop_assert_eq!(snap.active_subscribers, back.active_subscribers);
        prop_assert_eq!(snap.subscriber_lag_events, back.subscriber_lag_events);
    }

    /// Property 11: MetricsSnapshot JSON contains all field names.
    #[test]
    fn prop_metrics_snapshot_json_fields(snap in arb_metrics_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        prop_assert!(json.contains("events_published"), "missing events_published");
        prop_assert!(json.contains("events_dropped_no_subscribers"), "missing events_dropped_no_subscribers");
        prop_assert!(json.contains("active_subscribers"), "missing active_subscribers");
        prop_assert!(json.contains("subscriber_lag_events"), "missing subscriber_lag_events");
    }

    // ========================================================================
    // Property Tests: EventBusStats
    // ========================================================================

    /// Property 12: EventBusStats serde roundtrip.
    #[test]
    fn prop_event_bus_stats_serde_roundtrip(stats in arb_event_bus_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: EventBusStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.capacity, back.capacity);
        prop_assert_eq!(stats.delta_queued, back.delta_queued);
        prop_assert_eq!(stats.detection_queued, back.detection_queued);
        prop_assert_eq!(stats.signal_queued, back.signal_queued);
        prop_assert_eq!(stats.delta_subscribers, back.delta_subscribers);
        prop_assert_eq!(stats.detection_subscribers, back.detection_subscribers);
        prop_assert_eq!(stats.signal_subscribers, back.signal_subscribers);
        prop_assert_eq!(stats.delta_oldest_lag_ms, back.delta_oldest_lag_ms);
        prop_assert_eq!(stats.detection_oldest_lag_ms, back.detection_oldest_lag_ms);
        prop_assert_eq!(stats.signal_oldest_lag_ms, back.signal_oldest_lag_ms);
    }

    // ========================================================================
    // Property Tests: EventDeduplicator
    // ========================================================================

    /// Property 13: First check for any key returns DedupeVerdict::New.
    #[test]
    fn prop_dedup_first_check_is_new(key in arb_dedup_key()) {
        let mut dedup = EventDeduplicator::new();
        prop_assert_eq!(dedup.check(&key), DedupeVerdict::New,
            "first check should return New for key: {}", key);
    }

    /// Property 14: Second check within window returns Duplicate with count 1.
    #[test]
    fn prop_dedup_second_check_is_duplicate(key in arb_dedup_key()) {
        let mut dedup = EventDeduplicator::new();
        dedup.check(&key);
        let verdict = dedup.check(&key);
        prop_assert_eq!(verdict, DedupeVerdict::Duplicate { suppressed_count: 1 },
            "second check should be Duplicate(1) for key: {}", key);
    }

    /// Property 15: Suppressed count monotonically increases with repeated checks.
    #[test]
    fn prop_dedup_suppressed_count_monotonic(
        key in arb_dedup_key(),
        n_extra in 2..20usize,
    ) {
        let mut dedup = EventDeduplicator::new();
        dedup.check(&key); // First: New
        let mut prev = 0u64;
        for _ in 0..n_extra {
            if let DedupeVerdict::Duplicate { suppressed_count } = dedup.check(&key) {
                prop_assert!(suppressed_count > prev,
                    "suppressed_count should increase: prev={}, cur={}", prev, suppressed_count);
                prev = suppressed_count;
            } else {
                return Err(proptest::test_runner::TestCaseError::Fail(
                    "expected Duplicate after first check".into()));
            }
        }
    }

    /// Property 16: Different keys are independent — checking key A doesn't affect key B.
    #[test]
    fn prop_dedup_keys_independent(
        key_a in "[a-z]{1,10}",
        key_b in "[A-Z]{1,10}",
    ) {
        let mut dedup = EventDeduplicator::new();
        // Check A multiple times
        dedup.check(&key_a);
        dedup.check(&key_a);
        dedup.check(&key_a);
        // B should still be New
        prop_assert_eq!(dedup.check(&key_b), DedupeVerdict::New,
            "key_b should be New even after checking key_a");
        // A's suppressed count should reflect its 3 checks (suppressed=2 after 3 checks, now 4th = 3)
        prop_assert_eq!(dedup.suppressed_count(&key_a), 2,
            "key_a suppressed count should be 2 after 3 checks");
        // B was checked once, so suppressed count = 0
        prop_assert_eq!(dedup.suppressed_count(&key_b), 0,
            "key_b suppressed count should be 0 after 1 check");
    }

    /// Property 17: Dedup capacity is enforced — len never exceeds max_capacity.
    #[test]
    fn prop_dedup_capacity_enforced(max_cap in 2..20usize) {
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(300), max_cap);
        for i in 0..(max_cap * 2) {
            let key = format!("key_{}", i);
            dedup.check(&key);
            prop_assert!(dedup.len() <= max_cap,
                "len {} should not exceed capacity {}", dedup.len(), max_cap);
        }
    }

    /// Property 18: After clear, all keys return New.
    #[test]
    fn prop_dedup_clear_resets(
        keys in proptest::collection::vec(arb_dedup_key(), 1..10),
    ) {
        let mut dedup = EventDeduplicator::new();
        for key in &keys {
            dedup.check(key);
        }
        prop_assert!(!dedup.is_empty(), "should not be empty before clear");
        dedup.clear();
        prop_assert!(dedup.is_empty(), "should be empty after clear");
        prop_assert_eq!(dedup.len(), 0, "len should be 0 after clear");
        // All keys should return New again
        for key in &keys {
            prop_assert_eq!(dedup.check(key), DedupeVerdict::New,
                "key {} should be New after clear", key);
        }
    }

    /// Property 19: get() returns None for unknown keys, Some for known keys.
    #[test]
    fn prop_dedup_get_consistency(key in arb_dedup_key()) {
        let mut dedup = EventDeduplicator::new();
        prop_assert!(dedup.get(&key).is_none(), "unknown key should return None");
        dedup.check(&key);
        let entry = dedup.get(&key);
        prop_assert!(entry.is_some(), "known key should return Some after check");
        let entry = entry.unwrap();
        prop_assert_eq!(entry.count, 1, "count should be 1 after first check");
    }

    /// Property 20: DedupeEntry count equals total number of checks.
    #[test]
    fn prop_dedup_entry_count_matches_checks(
        key in arb_dedup_key(),
        n_checks in 1..15usize,
    ) {
        let mut dedup = EventDeduplicator::new();
        for _ in 0..n_checks {
            dedup.check(&key);
        }
        let entry = dedup.get(&key).unwrap();
        prop_assert_eq!(entry.count, n_checks as u64,
            "entry count should match number of checks");
    }

    // ========================================================================
    // Property Tests: NotificationCooldown
    // ========================================================================

    /// Property 21: First cooldown check for any key returns Send(0).
    #[test]
    fn prop_cooldown_first_check_sends(key in arb_dedup_key()) {
        let mut cd = NotificationCooldown::new();
        prop_assert_eq!(
            cd.check(&key),
            CooldownVerdict::Send { suppressed_since_last: 0 },
            "first check should Send(0) for key: {}", key
        );
    }

    /// Property 22: Second check within cooldown returns Suppress(1).
    #[test]
    fn prop_cooldown_second_check_suppresses(key in arb_dedup_key()) {
        let mut cd = NotificationCooldown::new(); // 30s default cooldown
        cd.check(&key);
        let verdict = cd.check(&key);
        prop_assert_eq!(
            verdict,
            CooldownVerdict::Suppress { total_suppressed: 1 },
            "second check should Suppress(1) for key: {}", key
        );
    }

    /// Property 23: Cooldown suppressed count monotonically increases.
    #[test]
    fn prop_cooldown_suppressed_monotonic(
        key in arb_dedup_key(),
        n_extra in 2..20usize,
    ) {
        let mut cd = NotificationCooldown::new();
        cd.check(&key); // Send(0)
        let mut prev = 0u64;
        for _ in 0..n_extra {
            if let CooldownVerdict::Suppress { total_suppressed } = cd.check(&key) {
                prop_assert!(total_suppressed > prev,
                    "total_suppressed should increase: prev={}, cur={}", prev, total_suppressed);
                prev = total_suppressed;
            } else {
                return Err(proptest::test_runner::TestCaseError::Fail(
                    "expected Suppress after first check within cooldown".into()));
            }
        }
    }

    /// Property 24: Different cooldown keys are independent.
    #[test]
    fn prop_cooldown_keys_independent(
        key_a in "[a-z]{1,10}",
        key_b in "[A-Z]{1,10}",
    ) {
        let mut cd = NotificationCooldown::new();
        cd.check(&key_a); // Send(0)
        cd.check(&key_a); // Suppress(1)
        cd.check(&key_a); // Suppress(2)
        // B should still be Send(0)
        prop_assert_eq!(
            cd.check(&key_b),
            CooldownVerdict::Send { suppressed_since_last: 0 },
            "key_b should Send(0) even after checking key_a"
        );
    }

    /// Property 25: Cooldown capacity is enforced.
    #[test]
    fn prop_cooldown_capacity_enforced(max_cap in 2..20usize) {
        let mut cd = NotificationCooldown::with_config(Duration::from_secs(300), max_cap);
        for i in 0..(max_cap * 2) {
            let key = format!("key_{}", i);
            cd.check(&key);
            prop_assert!(cd.len() <= max_cap,
                "len {} should not exceed capacity {}", cd.len(), max_cap);
        }
    }

    /// Property 26: After clear, all keys return Send(0).
    #[test]
    fn prop_cooldown_clear_resets(
        keys in proptest::collection::vec(arb_dedup_key(), 1..10),
    ) {
        let mut cd = NotificationCooldown::new();
        for key in &keys {
            cd.check(key);
        }
        prop_assert!(!cd.is_empty(), "should not be empty before clear");
        cd.clear();
        prop_assert!(cd.is_empty(), "should be empty after clear");
        for key in &keys {
            prop_assert_eq!(
                cd.check(key),
                CooldownVerdict::Send { suppressed_since_last: 0 },
                "key {} should Send(0) after clear", key
            );
        }
    }

    // ========================================================================
    // Property Tests: match_rule_glob
    // ========================================================================

    /// Property 27: Any string matches itself exactly.
    #[test]
    fn prop_glob_exact_match(s in "[a-z.]{1,20}") {
        prop_assert!(match_rule_glob(&s, &s),
            "'{}' should match itself", s);
    }

    /// Property 28: "*" alone matches any value.
    #[test]
    fn prop_glob_star_matches_all(s in "[a-zA-Z0-9._:-]{1,30}") {
        prop_assert!(match_rule_glob("*", &s),
            "'*' should match '{}'", s);
    }

    /// Property 29: "?" matches exactly one character.
    #[test]
    fn prop_glob_question_mark_matches_single_char(
        prefix in "[a-z]{1,5}",
        c in "[a-z]",
        suffix in "[a-z]{1,5}",
    ) {
        let value = format!("{}{}{}", prefix, c, suffix);
        let pattern = format!("{}?{}", prefix, suffix);
        prop_assert!(match_rule_glob(&pattern, &value),
            "pattern '{}' should match '{}'", pattern, value);
    }

    /// Property 30: Matching is deterministic — same inputs always give same result.
    #[test]
    fn prop_glob_deterministic(
        pattern in arb_glob_pattern(),
        value in "[a-z.]{1,15}",
    ) {
        let r1 = match_rule_glob(&pattern, &value);
        let r2 = match_rule_glob(&pattern, &value);
        prop_assert_eq!(r1, r2,
            "match_rule_glob should be deterministic for pattern='{}', value='{}'",
            pattern, value);
    }

    /// Property 31: Prefix wildcard — "*.suffix" matches strings ending with ".suffix".
    #[test]
    fn prop_glob_prefix_wildcard(
        prefix in "[a-z]{1,10}",
        suffix in "[a-z]{1,10}",
    ) {
        let pattern = format!("*.{}", suffix);
        let value = format!("{}.{}", prefix, suffix);
        prop_assert!(match_rule_glob(&pattern, &value),
            "pattern '{}' should match '{}'", pattern, value);
    }

    /// Property 32: Suffix wildcard — "prefix.*" matches strings starting with "prefix.".
    #[test]
    fn prop_glob_suffix_wildcard(
        prefix in "[a-z]{1,10}",
        suffix in "[a-z]{1,10}",
    ) {
        let pattern = format!("{}.*", prefix);
        let value = format!("{}.{}", prefix, suffix);
        prop_assert!(match_rule_glob(&pattern, &value),
            "pattern '{}' should match '{}'", pattern, value);
    }

    // ========================================================================
    // Property Tests: EventFilter
    // ========================================================================

    /// Property 33: allow_all() is permissive.
    #[test]
    fn prop_filter_allow_all_is_permissive(_dummy in Just(())) {
        let f = EventFilter::allow_all();
        prop_assert!(f.is_permissive(), "allow_all should be permissive");
    }

    /// Property 34: allow_all() matches any detection.
    #[test]
    fn prop_filter_allow_all_matches_any(detection in arb_detection()) {
        let f = EventFilter::allow_all();
        prop_assert!(f.matches(&detection),
            "allow_all should match any detection, rule_id={}", detection.rule_id);
    }

    /// Property 35: Default filter is permissive (same as allow_all).
    #[test]
    fn prop_filter_default_is_permissive(_dummy in Just(())) {
        let f = EventFilter::default();
        prop_assert!(f.is_permissive(), "default should be permissive");
    }

    /// Property 36: Filter with empty config is permissive.
    #[test]
    fn prop_filter_empty_config_is_permissive(_dummy in Just(())) {
        let f = EventFilter::from_config(&[], &[], None, &[]);
        prop_assert!(f.is_permissive(), "empty config should be permissive");
    }

    /// Property 37: Exclude pattern blocks matching detections.
    #[test]
    fn prop_filter_exclude_blocks(
        prefix in "[a-z]{1,8}",
        suffix in "[a-z]{1,8}",
    ) {
        let rule_id = format!("{}.{}", prefix, suffix);
        let exclude = format!("{}.*", prefix);
        let f = EventFilter::from_config(&[], &[exclude], None, &[]);
        let d = Detection {
            rule_id,
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        prop_assert!(!f.matches(&d),
            "exclude pattern should block matching detection");
    }

    /// Property 38: Include pattern with no match blocks detection.
    #[test]
    fn prop_filter_include_no_match_blocks(detection in arb_detection()) {
        // Use a pattern that won't match any generated rule_id
        let f = EventFilter::from_config(
            &["ZZZZZ_NOMATCH.*".to_string()],
            &[],
            None,
            &[],
        );
        prop_assert!(!f.matches(&detection),
            "non-matching include should block detection: {}", detection.rule_id);
    }

    /// Property 39: Exclude takes precedence over include.
    #[test]
    fn prop_filter_exclude_wins_over_include(
        prefix in "[a-z]{1,8}",
        suffix in "[a-z]{1,8}",
    ) {
        let rule_id = format!("{}.{}", prefix, suffix);
        let include = format!("{}.*", prefix);
        let exclude = format!("{}.{}", prefix, suffix);
        let f = EventFilter::from_config(
            &[include],
            &[exclude],
            None,
            &[],
        );
        let d = Detection {
            rule_id: rule_id.clone(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        prop_assert!(!f.matches(&d),
            "exclude should win over include for rule_id: {}", rule_id);
    }

    /// Property 40: is_permissive is false when any field is set.
    #[test]
    fn prop_filter_not_permissive_with_include(
        pattern in "[a-z]{1,10}\\.[a-z]{1,10}",
    ) {
        let f = EventFilter::from_config(std::slice::from_ref(&pattern), &[], None, &[]);
        prop_assert!(!f.is_permissive(),
            "filter with include should not be permissive");
    }

    // ========================================================================
    // Property Tests: event_identity_key
    // ========================================================================

    /// Property 41: event_identity_key is deterministic.
    #[test]
    fn prop_identity_key_deterministic(
        detection in arb_detection(),
        pane_id in 0..1000u64,
    ) {
        let k1 = event_identity_key(&detection, pane_id, None);
        let k2 = event_identity_key(&detection, pane_id, None);
        prop_assert_eq!(k1, k2, "identity key should be deterministic");
    }

    /// Property 42: event_identity_key always starts with "evt:" and is 68 chars.
    #[test]
    fn prop_identity_key_format(
        detection in arb_detection(),
        pane_id in 0..1000u64,
    ) {
        let key = event_identity_key(&detection, pane_id, None);
        prop_assert!(key.starts_with("evt:"),
            "key should start with 'evt:', got: {}", key);
        prop_assert_eq!(key.len(), 68,
            "key should be 68 chars (4 prefix + 64 hex), got len={}", key.len());
    }

    /// Property 43: Different pane_ids produce different keys.
    #[test]
    fn prop_identity_key_pane_differentiation(
        detection in arb_detection(),
        pane_a in 0..500u64,
        pane_b in 500..1000u64,
    ) {
        let k1 = event_identity_key(&detection, pane_a, None);
        let k2 = event_identity_key(&detection, pane_b, None);
        prop_assert_ne!(k1, k2,
            "different pane_ids should produce different keys: pane_a={}, pane_b={}",
            pane_a, pane_b);
    }

    /// Property 44: pane_uuid overrides pane_id in identity key.
    #[test]
    fn prop_identity_key_uuid_overrides_pane_id(
        detection in arb_detection(),
        pane_a in 0..500u64,
        pane_b in 500..1000u64,
        uuid in "[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}",
    ) {
        // With same UUID, different pane_ids should produce the same key
        let k1 = event_identity_key(&detection, pane_a, Some(&uuid));
        let k2 = event_identity_key(&detection, pane_b, Some(&uuid));
        prop_assert_eq!(k1, k2,
            "same UUID should produce same key regardless of pane_id");
    }

    /// Property 45: Different rule_ids produce different keys.
    #[test]
    fn prop_identity_key_rule_differentiation(
        pane_id in 0..1000u64,
    ) {
        let d1 = Detection {
            rule_id: "alpha.one".to_string(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        let d2 = Detection {
            rule_id: "beta.two".to_string(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        let k1 = event_identity_key(&d1, pane_id, None);
        let k2 = event_identity_key(&d2, pane_id, None);
        prop_assert_ne!(k1, k2,
            "different rule_ids should produce different keys");
    }

    // ========================================================================
    // Property Tests: NotificationGate pipeline
    // ========================================================================

    /// Property 46: First event through a permissive gate always sends.
    #[test]
    fn prop_gate_first_event_sends(detection in arb_detection(), pane_id in 0..1000u64) {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let result = gate.should_notify(&detection, pane_id, None);
        prop_assert!(matches!(result, NotifyDecision::Send { suppressed_since_last: 0 }),
            "first event should Send(0), got: {:?}", result);
    }

    /// Property 47: Filtered events always return Filtered.
    #[test]
    fn prop_gate_filtered_returns_filtered(
        detection in arb_detection(),
        pane_id in 0..1000u64,
        n_attempts in 1..5usize,
    ) {
        // Create filter that excludes everything
        let f = EventFilter::from_config(&[], &["*".to_string()], None, &[]);
        let mut gate = NotificationGate::from_config(
            f,
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        for _ in 0..n_attempts {
            let result = gate.should_notify(&detection, pane_id, None);
            prop_assert_eq!(result, NotifyDecision::Filtered,
                "filtered event should always return Filtered");
        }
    }

    /// Property 48: Second identical event is deduplicated (within window).
    #[test]
    fn prop_gate_second_event_deduped(
        detection in arb_detection(),
        pane_id in 0..1000u64,
    ) {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        // First: Send
        gate.should_notify(&detection, pane_id, None);
        // Second: should be deduplicated
        let result = gate.should_notify(&detection, pane_id, None);
        prop_assert!(matches!(result, NotifyDecision::Deduplicated { .. }),
            "second event should be deduplicated, got: {:?}", result);
    }

    /// Property 49: Different panes are independent in the gate.
    #[test]
    fn prop_gate_panes_independent(
        detection in arb_detection(),
        pane_a in 0..500u64,
        pane_b in 500..1000u64,
    ) {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        // Pane A sends
        let r1 = gate.should_notify(&detection, pane_a, None);
        prop_assert!(matches!(r1, NotifyDecision::Send { .. }),
            "pane_a should Send, got: {:?}", r1);
        // Pane B also sends (independent)
        let r2 = gate.should_notify(&detection, pane_b, None);
        prop_assert!(matches!(r2, NotifyDecision::Send { .. }),
            "pane_b should Send independently, got: {:?}", r2);
    }

    /// Property 50: gate.filter() returns the filter reference.
    #[test]
    fn prop_gate_filter_accessor(_dummy in Just(())) {
        let gate_permissive = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        prop_assert!(gate_permissive.filter().is_permissive(),
            "permissive gate should have permissive filter");

        let gate_restricted = NotificationGate::from_config(
            EventFilter::from_config(&["test.*".to_string()], &[], None, &[]),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        prop_assert!(!gate_restricted.filter().is_permissive(),
            "restricted gate should not have permissive filter");
    }

    // ========================================================================
    // Property Tests: NotifyDecision
    // ========================================================================

    /// Property 51: NotifyDecision variants are mutually exclusive.
    #[test]
    fn prop_notify_decision_variant_exclusive(
        detection in arb_detection(),
        pane_id in 0..1000u64,
    ) {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let result = gate.should_notify(&detection, pane_id, None);
        let mut count = 0u32;
        if matches!(result, NotifyDecision::Send { .. }) { count += 1; }
        if matches!(result, NotifyDecision::Filtered) { count += 1; }
        if matches!(result, NotifyDecision::Deduplicated { .. }) { count += 1; }
        if matches!(result, NotifyDecision::Throttled { .. }) { count += 1; }
        prop_assert_eq!(count, 1, "exactly one variant should match");
    }

    // ========================================================================
    // Property Tests: EventBus
    // ========================================================================

    /// Property 52: EventBus capacity is preserved from constructor.
    #[test]
    fn prop_event_bus_capacity(cap in 1..10000usize) {
        let bus = EventBus::new(cap);
        prop_assert_eq!(bus.capacity(), cap, "capacity should match constructor arg");
    }

    /// Property 53: New EventBus starts with 0 subscribers.
    #[test]
    fn prop_event_bus_initial_subscribers(cap in 1..1000usize) {
        let bus = EventBus::new(cap);
        prop_assert_eq!(bus.subscriber_count(), 0, "new bus should have 0 subscribers");
    }

    /// Property 54: Publishing with no subscribers counts drops.
    #[test]
    fn prop_event_bus_no_subscriber_drops(event in arb_event()) {
        let bus = EventBus::new(100);
        let count = bus.publish(event);
        prop_assert_eq!(count, 0, "no subscribers means 0 delivered");
        let snap = bus.metrics().snapshot();
        prop_assert_eq!(snap.events_published, 1);
        prop_assert_eq!(snap.events_dropped_no_subscribers, 1);
    }

    /// Property 55: EventBusMetrics::new() starts at zero.
    #[test]
    fn prop_event_bus_metrics_initial(_dummy in Just(())) {
        let m = EventBusMetrics::new();
        let snap = m.snapshot();
        prop_assert_eq!(snap.events_published, 0);
        prop_assert_eq!(snap.events_dropped_no_subscribers, 0);
        prop_assert_eq!(snap.active_subscribers, 0);
        prop_assert_eq!(snap.subscriber_lag_events, 0);
    }

    // ========================================================================
    // Property Tests: RecvError
    // ========================================================================

    /// Property 56: RecvError::Closed display message is consistent.
    #[test]
    fn prop_recv_error_closed_display(_dummy in Just(())) {
        let err = RecvError::Closed;
        let msg = format!("{}", err);
        prop_assert!(msg.contains("closed"), "Closed error should mention 'closed': {}", msg);
    }

    /// Property 57: RecvError::Lagged display includes count.
    #[test]
    fn prop_recv_error_lagged_display(count in 1..10000u64) {
        let err = RecvError::Lagged { missed_count: count };
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&count.to_string()),
            "Lagged error should include count {}: {}", count, msg);
    }

    // ========================================================================
    // Property Tests: UserVarError
    // ========================================================================

    /// Property 58: UserVarError::WatcherNotRunning display includes socket path.
    #[test]
    fn prop_user_var_error_watcher_display(path in "/[a-z/]{1,30}\\.sock") {
        let err = UserVarError::WatcherNotRunning {
            socket_path: path.clone(),
        };
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&path),
            "WatcherNotRunning should include path: {}", msg);
    }

    /// Property 59: UserVarError::IpcSendFailed display includes message.
    #[test]
    fn prop_user_var_error_ipc_display(detail in "[a-z ]{1,30}") {
        let err = UserVarError::IpcSendFailed {
            message: detail.clone(),
        };
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&detail),
            "IpcSendFailed should include detail: {}", msg);
    }

    /// Property 60: UserVarError::ParseFailed display includes reason.
    #[test]
    fn prop_user_var_error_parse_display(reason in "[a-z ]{1,30}") {
        let err = UserVarError::ParseFailed(reason.clone());
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&reason),
            "ParseFailed should include reason: {}", msg);
    }

    // ========================================================================
    // Composite Properties
    // ========================================================================

    /// Property 61: Dedup + Cooldown sequence — N repeated checks produce
    /// the expected pattern: New, then Duplicate(1), Duplicate(2), ...
    #[test]
    fn prop_dedup_sequence_pattern(
        key in arb_dedup_key(),
        n in 2..15usize,
    ) {
        let mut dedup = EventDeduplicator::new();
        let first = dedup.check(&key);
        prop_assert_eq!(first, DedupeVerdict::New, "first should be New");
        for i in 1..n {
            let verdict = dedup.check(&key);
            prop_assert_eq!(
                verdict,
                DedupeVerdict::Duplicate { suppressed_count: i as u64 },
                "check {} should be Duplicate({})", i + 1, i
            );
        }
    }

    /// Property 62: Cooldown sequence — N repeated checks produce
    /// Send(0), then Suppress(1), Suppress(2), ...
    #[test]
    fn prop_cooldown_sequence_pattern(
        key in arb_dedup_key(),
        n in 2..15usize,
    ) {
        let mut cd = NotificationCooldown::new();
        let first = cd.check(&key);
        prop_assert_eq!(first, CooldownVerdict::Send { suppressed_since_last: 0 },
            "first should be Send(0)");
        for i in 1..n {
            let verdict = cd.check(&key);
            prop_assert_eq!(
                verdict,
                CooldownVerdict::Suppress { total_suppressed: i as u64 },
                "check {} should be Suppress({})", i + 1, i
            );
        }
    }

    /// Property 63: Multiple event types through the same gate are independent
    /// (different rule_ids get their own dedup/cooldown slots).
    #[test]
    fn prop_gate_different_events_independent(pane_id in 0..1000u64) {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let d1 = Detection {
            rule_id: "alpha.one".to_string(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        let d2 = Detection {
            rule_id: "beta.two".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "other".to_string(),
            severity: Severity::Warning,
            confidence: 0.8,
            extracted: serde_json::json!({}),
            matched_text: "other".to_string(),
            span: (0, 0),
        };
        // Both should send on first check
        let r1 = gate.should_notify(&d1, pane_id, None);
        let r2 = gate.should_notify(&d2, pane_id, None);
        prop_assert!(matches!(r1, NotifyDecision::Send { .. }),
            "d1 first check should Send, got: {:?}", r1);
        prop_assert!(matches!(r2, NotifyDecision::Send { .. }),
            "d2 first check should Send, got: {:?}", r2);
    }
}
