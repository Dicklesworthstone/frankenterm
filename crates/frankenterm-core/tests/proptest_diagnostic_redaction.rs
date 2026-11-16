//! Property-based tests for diagnostic_redaction.rs.
//!
//! Covers serde roundtrips for all redaction types, DiagnosticFieldPolicy
//! default/strict/permissive key-set invariants, DiagnosticPrivacyBudget
//! preset ordering, RedactionStats total_redactions arithmetic, redaction
//! logic (always-redact keys, always-safe keys, truncation, budget limits),
//! and deterministic behaviour.

use std::collections::{HashMap, HashSet};

use frankenterm_core::diagnostic_redaction::{
    DiagnosticFieldPolicy, DiagnosticPrivacyBudget, DiagnosticRedactor, RedactionStats,
};
use frankenterm_core::runtime_telemetry::{
    HealthTier, RuntimePhase, RuntimeTelemetryEvent, RuntimeTelemetryKind,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_redaction_stats() -> impl Strategy<Value = RedactionStats> {
    (
        0..=1000u64,
        0..=1000u64,
        0..=500u64,
        0..=500u64,
        0..=200u64,
        0..=200u64,
        0..=100u64,
        0..=100u64,
        0..=100u64,
        0..=1_000_000u64,
        any::<bool>(),
    )
        .prop_map(
            |(
                events_processed,
                events_dropped,
                details_redacted_pattern,
                details_redacted_policy,
                details_truncated,
                details_dropped,
                evidence_truncated,
                correlation_ids_redacted,
                scope_ids_redacted,
                output_bytes,
                budget_exceeded,
            )| {
                RedactionStats {
                    events_processed,
                    events_dropped,
                    details_redacted_pattern,
                    details_redacted_policy,
                    details_truncated,
                    details_dropped,
                    evidence_truncated,
                    correlation_ids_redacted,
                    scope_ids_redacted,
                    output_bytes,
                    budget_exceeded,
                }
            },
        )
}

fn arb_budget() -> impl Strategy<Value = DiagnosticPrivacyBudget> {
    (
        1..=500usize,
        1..=200usize,
        1..=100usize,
        1..=50usize,
        10..=2000usize,
        1..=100usize,
        1..=50usize,
        1024..=4_194_304usize,
    )
        .prop_map(
            |(
                max_events,
                max_tier_transitions,
                max_active_failures,
                max_details_per_event,
                max_detail_value_len,
                max_health_checks,
                max_evidence_lines,
                max_total_bytes,
            )| {
                DiagnosticPrivacyBudget {
                    max_events,
                    max_tier_transitions,
                    max_active_failures,
                    max_details_per_event,
                    max_detail_value_len,
                    max_health_checks,
                    max_evidence_lines,
                    max_total_bytes,
                }
            },
        )
}

fn make_event_with_details(details: HashMap<String, serde_json::Value>) -> RuntimeTelemetryEvent {
    RuntimeTelemetryEvent {
        timestamp_ms: 1_000_000,
        component: "test.redaction".to_string(),
        scope_id: Some("scope-1".to_string()),
        event_kind: RuntimeTelemetryKind::TierTransition,
        health_tier: HealthTier::Green,
        phase: RuntimePhase::Running,
        reason_code: "test.ok".to_string(),
        correlation_id: "corr-123".to_string(),
        failure_class: None,
        details,
    }
}

// ── DiagnosticFieldPolicy serde ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 1. Default policy serde roundtrip
    #[test]
    fn field_policy_default_serde_roundtrip(_seed in 0..=10u32) {
        let policy = DiagnosticFieldPolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let restored: DiagnosticFieldPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.always_redact.len(), policy.always_redact.len());
        prop_assert_eq!(restored.always_safe.len(), policy.always_safe.len());
        prop_assert_eq!(restored.scan_unknown_keys, policy.scan_unknown_keys);
        prop_assert_eq!(restored.redact_correlation_ids, policy.redact_correlation_ids);
        prop_assert_eq!(restored.redact_scope_ids, policy.redact_scope_ids);
        prop_assert_eq!(&restored.redaction_marker, &policy.redaction_marker);
    }

    // 2. Strict policy serde roundtrip
    #[test]
    fn field_policy_strict_serde_roundtrip(_seed in 0..=10u32) {
        let policy = DiagnosticFieldPolicy::strict();
        let json = serde_json::to_string(&policy).unwrap();
        let restored: DiagnosticFieldPolicy = serde_json::from_str(&json).unwrap();
        prop_assert!(restored.redact_correlation_ids);
        prop_assert!(restored.redact_scope_ids);
        prop_assert!(restored.scan_unknown_keys);
    }

    // 3. Permissive policy serde roundtrip
    #[test]
    fn field_policy_permissive_serde_roundtrip(_seed in 0..=10u32) {
        let policy = DiagnosticFieldPolicy::permissive();
        let json = serde_json::to_string(&policy).unwrap();
        let restored: DiagnosticFieldPolicy = serde_json::from_str(&json).unwrap();
        prop_assert!(!restored.scan_unknown_keys);
        prop_assert!(!restored.redact_correlation_ids);
        prop_assert!(!restored.redact_scope_ids);
    }
}

// ── DiagnosticFieldPolicy invariants ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 4. Default always_redact and always_safe are disjoint
    #[test]
    fn field_policy_redact_safe_disjoint(_seed in 0..=10u32) {
        let policy = DiagnosticFieldPolicy::default();
        let intersection: HashSet<_> = policy
            .always_redact
            .intersection(&policy.always_safe)
            .collect();
        prop_assert!(intersection.is_empty(), "redact and safe sets must not overlap");
    }

    // 5. Default always_redact contains core sensitive keys
    #[test]
    fn field_policy_default_redacts_secrets(_seed in 0..=10u32) {
        let policy = DiagnosticFieldPolicy::default();
        for key in ["password", "api_key", "token", "secret", "credential"] {
            prop_assert!(
                policy.always_redact.contains(key),
                "default policy must redact '{}'", key
            );
        }
    }

    // 6. Default always_safe contains structural keys
    #[test]
    fn field_policy_default_allows_structural(_seed in 0..=10u32) {
        let policy = DiagnosticFieldPolicy::default();
        for key in ["queue_depth", "count", "ratio"] {
            prop_assert!(
                policy.always_safe.contains(key),
                "default policy must allow '{}'", key
            );
        }
    }

    // 7. Strict policy is a superset of default in strictness
    #[test]
    fn field_policy_strict_stricter_than_default(_seed in 0..=10u32) {
        let default_policy = DiagnosticFieldPolicy::default();
        let strict = DiagnosticFieldPolicy::strict();
        // Strict redacts correlation_ids and scope_ids, default does not
        prop_assert!(strict.redact_correlation_ids);
        prop_assert!(strict.redact_scope_ids);
        prop_assert!(!default_policy.redact_correlation_ids);
        prop_assert!(!default_policy.redact_scope_ids);
    }
}

// ── DiagnosticPrivacyBudget serde ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 8. Budget serde roundtrip
    #[test]
    fn budget_serde_roundtrip(budget in arb_budget()) {
        let json = serde_json::to_string(&budget).unwrap();
        let restored: DiagnosticPrivacyBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.max_events, budget.max_events);
        prop_assert_eq!(restored.max_tier_transitions, budget.max_tier_transitions);
        prop_assert_eq!(restored.max_active_failures, budget.max_active_failures);
        prop_assert_eq!(restored.max_details_per_event, budget.max_details_per_event);
        prop_assert_eq!(restored.max_detail_value_len, budget.max_detail_value_len);
        prop_assert_eq!(restored.max_health_checks, budget.max_health_checks);
        prop_assert_eq!(restored.max_evidence_lines, budget.max_evidence_lines);
        prop_assert_eq!(restored.max_total_bytes, budget.max_total_bytes);
    }

    // 9. Default budget preset serde roundtrip
    #[test]
    fn budget_default_serde_roundtrip(_seed in 0..=10u32) {
        let budget = DiagnosticPrivacyBudget::default();
        let json = serde_json::to_string(&budget).unwrap();
        let restored: DiagnosticPrivacyBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.max_events, budget.max_events);
        prop_assert_eq!(restored.max_total_bytes, budget.max_total_bytes);
    }
}

// ── DiagnosticPrivacyBudget preset ordering ─────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 10. Strict budget ≤ default budget ≤ verbose budget (all dimensions)
    #[test]
    fn budget_presets_ordered(_seed in 0..=10u32) {
        let strict = DiagnosticPrivacyBudget::strict();
        let default_budget = DiagnosticPrivacyBudget::default();
        let verbose = DiagnosticPrivacyBudget::verbose();

        // strict ≤ default
        prop_assert!(strict.max_events <= default_budget.max_events);
        prop_assert!(strict.max_tier_transitions <= default_budget.max_tier_transitions);
        prop_assert!(strict.max_active_failures <= default_budget.max_active_failures);
        prop_assert!(strict.max_details_per_event <= default_budget.max_details_per_event);
        prop_assert!(strict.max_detail_value_len <= default_budget.max_detail_value_len);
        prop_assert!(strict.max_total_bytes <= default_budget.max_total_bytes);

        // default ≤ verbose
        prop_assert!(default_budget.max_events <= verbose.max_events);
        prop_assert!(default_budget.max_tier_transitions <= verbose.max_tier_transitions);
        prop_assert!(default_budget.max_active_failures <= verbose.max_active_failures);
        prop_assert!(default_budget.max_details_per_event <= verbose.max_details_per_event);
        prop_assert!(default_budget.max_detail_value_len <= verbose.max_detail_value_len);
        prop_assert!(default_budget.max_total_bytes <= verbose.max_total_bytes);
    }
}

// ── RedactionStats ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 11. total_redactions sums the 6 redaction-action fields
    #[test]
    fn stats_total_redactions_correct(stats in arb_redaction_stats()) {
        let expected = stats.details_redacted_pattern
            + stats.details_redacted_policy
            + stats.details_truncated
            + stats.details_dropped
            + stats.correlation_ids_redacted
            + stats.scope_ids_redacted;
        prop_assert_eq!(stats.total_redactions(), expected);
    }

    // 12. Default stats has zero total_redactions
    #[test]
    fn stats_default_zero(_seed in 0..=10u32) {
        let stats = RedactionStats::default();
        prop_assert_eq!(stats.total_redactions(), 0);
        prop_assert_eq!(stats.events_processed, 0);
        prop_assert_eq!(stats.events_dropped, 0);
        prop_assert!(!stats.budget_exceeded);
    }

    // 13. RedactionStats serde roundtrip
    #[test]
    fn stats_serde_roundtrip(stats in arb_redaction_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let restored: RedactionStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.events_processed, stats.events_processed);
        prop_assert_eq!(restored.events_dropped, stats.events_dropped);
        prop_assert_eq!(restored.details_redacted_pattern, stats.details_redacted_pattern);
        prop_assert_eq!(restored.details_redacted_policy, stats.details_redacted_policy);
        prop_assert_eq!(restored.details_truncated, stats.details_truncated);
        prop_assert_eq!(restored.details_dropped, stats.details_dropped);
        prop_assert_eq!(restored.evidence_truncated, stats.evidence_truncated);
        prop_assert_eq!(restored.correlation_ids_redacted, stats.correlation_ids_redacted);
        prop_assert_eq!(restored.scope_ids_redacted, stats.scope_ids_redacted);
        prop_assert_eq!(restored.output_bytes, stats.output_bytes);
        prop_assert_eq!(restored.budget_exceeded, stats.budget_exceeded);
        // total_redactions is derived, should match
        prop_assert_eq!(restored.total_redactions(), stats.total_redactions());
    }
}

// ── Redaction logic: always-redact keys ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 14. Always-redact keys produce redaction marker in output
    #[test]
    fn redact_always_redact_keys(value in "[A-Za-z0-9]{5,50}") {
        let redactor = DiagnosticRedactor::default();
        let policy = redactor.field_policy();
        let marker = policy.redaction_marker.clone();

        // Build event with one always-redact key
        let mut details = HashMap::new();
        details.insert(
            "password".to_string(),
            serde_json::Value::String(value),
        );

        let event = make_event_with_details(details);
        let redacted = redactor.redact_event(&event);

        let redacted_val = redacted.details.get("password").unwrap();
        prop_assert_eq!(redacted_val.as_str().unwrap(), &marker);
    }

    // 15. Always-safe keys are NOT redacted (when no secret patterns present)
    #[test]
    fn redact_always_safe_keys_pass_through(value in "[0-9]{1,10}") {
        let redactor = DiagnosticRedactor::default();

        let mut details = HashMap::new();
        details.insert(
            "queue_depth".to_string(),
            serde_json::Value::String(value.clone()),
        );

        let event = make_event_with_details(details);
        let redacted = redactor.redact_event(&event);

        let redacted_val = redacted.details.get("queue_depth").unwrap();
        // Value should start with original content (may have truncation suffix)
        let result_str = redacted_val.as_str().unwrap();
        prop_assert!(result_str.starts_with(&value));
    }

    // 16. Non-string values (numbers, booleans) are never redacted
    #[test]
    fn redact_non_string_values_pass_through(num in -1000i64..=1000i64) {
        let redactor = DiagnosticRedactor::default();

        let mut details = HashMap::new();
        details.insert(
            "password".to_string(), // even an always-redact key
            serde_json::json!(num),
        );

        let event = make_event_with_details(details);
        let redacted = redactor.redact_event(&event);

        // Non-string values pass through even for always-redact keys
        let redacted_val = redacted.details.get("password").unwrap();
        prop_assert_eq!(redacted_val.as_i64(), Some(num));
    }
}

// ── Redaction logic: truncation ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 17. Values exceeding max_detail_value_len are truncated
    #[test]
    fn redact_truncates_long_values(
        extra_len in 1..=100usize,
    ) {
        let budget = DiagnosticPrivacyBudget {
            max_detail_value_len: 20,
            ..DiagnosticPrivacyBudget::default()
        };
        let redactor = DiagnosticRedactor::new(
            DiagnosticFieldPolicy::permissive(),
            budget,
        );

        let long_value = "a".repeat(20 + extra_len);
        let mut details = HashMap::new();
        details.insert(
            "safe_key".to_string(),
            serde_json::Value::String(long_value),
        );

        let event = make_event_with_details(details);
        let redacted = redactor.redact_event(&event);

        let result = redacted.details.get("safe_key").unwrap().as_str().unwrap();
        prop_assert!(result.contains("[truncated]"));
        // Truncated string starts with 20 'a's
        prop_assert!(result.starts_with(&"a".repeat(20)));
    }

    // 18. Values within max_detail_value_len are NOT truncated
    #[test]
    fn redact_short_values_not_truncated(len in 1..=20usize) {
        let budget = DiagnosticPrivacyBudget {
            max_detail_value_len: 20,
            ..DiagnosticPrivacyBudget::default()
        };
        let redactor = DiagnosticRedactor::new(
            DiagnosticFieldPolicy::permissive(),
            budget,
        );

        let short_value = "b".repeat(len);
        let mut details = HashMap::new();
        details.insert(
            "custom_key".to_string(),
            serde_json::Value::String(short_value.clone()),
        );

        let event = make_event_with_details(details);
        let redacted = redactor.redact_event(&event);

        let result = redacted.details.get("custom_key").unwrap().as_str().unwrap();
        prop_assert_eq!(result, &short_value);
    }
}

// ── Redaction logic: correlation/scope ID redaction ─────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 19. Strict policy redacts correlation_id
    #[test]
    fn strict_redacts_correlation_id(corr_id in "[a-z0-9-]{5,20}") {
        let redactor = DiagnosticRedactor::strict();

        let mut event = make_event_with_details(HashMap::new());
        event.correlation_id = corr_id;

        let redacted = redactor.redact_event(&event);
        prop_assert_eq!(&redacted.correlation_id, "[REDACTED]");
    }

    // 20. Strict policy redacts scope_id
    #[test]
    fn strict_redacts_scope_id(scope_id in "[a-z0-9-]{5,20}") {
        let redactor = DiagnosticRedactor::strict();

        let mut event = make_event_with_details(HashMap::new());
        event.scope_id = Some(scope_id);

        let redacted = redactor.redact_event(&event);
        prop_assert_eq!(redacted.scope_id.as_deref(), Some("[REDACTED]"));
    }

    // 21. Default policy does NOT redact correlation_id
    #[test]
    fn default_preserves_correlation_id(corr_id in "[a-z0-9-]{5,20}") {
        let redactor = DiagnosticRedactor::default();

        let mut event = make_event_with_details(HashMap::new());
        event.correlation_id = corr_id.clone();

        let redacted = redactor.redact_event(&event);
        prop_assert_eq!(&redacted.correlation_id, &corr_id);
    }
}

// ── Batch redaction: budget enforcement ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 22. Batch redaction respects max_events limit
    #[test]
    fn batch_respects_max_events(n_events in 1..=50usize) {
        let max_events = 10;
        let budget = DiagnosticPrivacyBudget {
            max_events,
            max_total_bytes: 10_000_000, // large enough to not interfere
            ..DiagnosticPrivacyBudget::default()
        };
        let redactor = DiagnosticRedactor::new(
            DiagnosticFieldPolicy::default(),
            budget,
        );

        let events: Vec<RuntimeTelemetryEvent> = (0..n_events)
            .map(|i| {
                let mut e = make_event_with_details(HashMap::new());
                e.timestamp_ms = i as u64;
                e
            })
            .collect();

        let (result, stats) = redactor.redact_events(&events);
        prop_assert!(result.len() <= max_events);

        if n_events > max_events {
            prop_assert!(stats.events_dropped > 0);
        }
    }

    // 23. Batch redaction events_processed + events_dropped = input length
    #[test]
    fn batch_stats_conservation(n_events in 1..=30usize) {
        let budget = DiagnosticPrivacyBudget {
            max_total_bytes: 10_000_000,
            ..DiagnosticPrivacyBudget::default()
        };
        let redactor = DiagnosticRedactor::new(
            DiagnosticFieldPolicy::default(),
            budget,
        );

        let events: Vec<RuntimeTelemetryEvent> = (0..n_events)
            .map(|i| {
                let mut e = make_event_with_details(HashMap::new());
                e.timestamp_ms = i as u64;
                e
            })
            .collect();

        let (_result, stats) = redactor.redact_events(&events);
        prop_assert_eq!(
            stats.events_processed + stats.events_dropped,
            n_events as u64
        );
    }
}

// ── Redaction logic: per-event detail budget ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 24. Details per event are bounded by max_details_per_event
    #[test]
    fn details_bounded_by_budget(n_details in 1..=30usize) {
        let max_details = 5;
        let budget = DiagnosticPrivacyBudget {
            max_details_per_event: max_details,
            ..DiagnosticPrivacyBudget::default()
        };
        let redactor = DiagnosticRedactor::new(
            DiagnosticFieldPolicy::permissive(),
            budget,
        );

        let mut details = HashMap::new();
        for i in 0..n_details {
            details.insert(
                format!("key_{i}"),
                serde_json::Value::String(format!("val_{i}")),
            );
        }

        let event = make_event_with_details(details);
        let redacted = redactor.redact_event(&event);

        prop_assert!(redacted.details.len() <= max_details);
    }
}

// ── Structural invariants ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 25. Redacted event preserves non-detail structural fields
    #[test]
    fn redact_preserves_structure(ts in 0..=u64::MAX) {
        let redactor = DiagnosticRedactor::default();

        let mut event = make_event_with_details(HashMap::new());
        event.timestamp_ms = ts;

        let redacted = redactor.redact_event(&event);
        prop_assert_eq!(redacted.timestamp_ms, ts);
        prop_assert_eq!(&redacted.component, &event.component);
        prop_assert_eq!(redacted.health_tier, event.health_tier);
        prop_assert_eq!(redacted.phase, event.phase);
        prop_assert_eq!(&redacted.reason_code, &event.reason_code);
    }

    // 26. Redaction is idempotent (double-redact = single-redact)
    #[test]
    fn redact_idempotent(val in "[a-z]{5,20}") {
        let redactor = DiagnosticRedactor::default();

        let mut details = HashMap::new();
        details.insert("password".to_string(), serde_json::Value::String(val));

        let event = make_event_with_details(details);
        let once = redactor.redact_event(&event);
        let twice = redactor.redact_event(&once);

        // Second redaction should not change anything
        prop_assert_eq!(
            once.details.get("password"),
            twice.details.get("password"),
        );
    }
}

// ── Determinism ─────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 27. Same inputs produce identical redacted output
    #[test]
    fn redact_deterministic(val in "[A-Za-z0-9]{1,30}") {
        let r1 = DiagnosticRedactor::default();
        let r2 = DiagnosticRedactor::default();

        let mut details = HashMap::new();
        details.insert("custom".to_string(), serde_json::Value::String(val));

        let event = make_event_with_details(details);
        let out1 = r1.redact_event(&event);
        let out2 = r2.redact_event(&event);

        prop_assert_eq!(out1.details, out2.details);
        prop_assert_eq!(&out1.correlation_id, &out2.correlation_id);
        prop_assert_eq!(out1.scope_id, out2.scope_id);
    }
}
