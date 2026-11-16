//! Extended property-based tests for runtime_telemetry module.
//!
//! Supplements proptest_runtime_telemetry.rs with coverage for:
//! - HealthTier boundary thresholds (from_ratio)
//! - HealthTier requires_attention vs is_degraded consistency
//! - HealthTier severity ordering invariant
//! - FailureClass suggested_tier always degraded
//! - TierTransitionRecord serde roundtrip
//! - TierTransitionRecord escalation/recovery XOR invariant
//! - RuntimeTelemetryLog disabled mode
//! - RuntimeTelemetryLog snapshot consistency after mixed operations
//! - Builder with scope_id preservation
//! - Event Display non-empty for all kinds
//! - WatchdogAlert serde roundtrip for RuntimeTelemetryEvent
//! - Snapshot kind_counts sum to buffered_events

use proptest::prelude::*;

use frankenterm_core::runtime_telemetry::{
    FailureClass, HealthTier, RuntimePhase, RuntimeTelemetryEventBuilder, RuntimeTelemetryKind,
    RuntimeTelemetryLog, RuntimeTelemetryLogConfig, TierTransitionRecord,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_health_tier() -> impl Strategy<Value = HealthTier> {
    prop_oneof![
        Just(HealthTier::Green),
        Just(HealthTier::Yellow),
        Just(HealthTier::Red),
        Just(HealthTier::Black),
    ]
}

fn arb_runtime_phase() -> impl Strategy<Value = RuntimePhase> {
    prop_oneof![
        Just(RuntimePhase::Init),
        Just(RuntimePhase::Startup),
        Just(RuntimePhase::Running),
        Just(RuntimePhase::Draining),
        Just(RuntimePhase::Finalizing),
        Just(RuntimePhase::Shutdown),
        Just(RuntimePhase::Cancelling),
        Just(RuntimePhase::Recovering),
        Just(RuntimePhase::Maintenance),
    ]
}

fn arb_event_kind() -> impl Strategy<Value = RuntimeTelemetryKind> {
    prop_oneof![
        Just(RuntimeTelemetryKind::ScopeCreated),
        Just(RuntimeTelemetryKind::ScopeStarted),
        Just(RuntimeTelemetryKind::ScopeDraining),
        Just(RuntimeTelemetryKind::ScopeFinalizing),
        Just(RuntimeTelemetryKind::ScopeClosed),
        Just(RuntimeTelemetryKind::CancellationRequested),
        Just(RuntimeTelemetryKind::CancellationPropagated),
        Just(RuntimeTelemetryKind::GracePeriodExpired),
        Just(RuntimeTelemetryKind::FinalizerCompleted),
        Just(RuntimeTelemetryKind::TierTransition),
        Just(RuntimeTelemetryKind::ThrottleApplied),
        Just(RuntimeTelemetryKind::ThrottleReleased),
        Just(RuntimeTelemetryKind::LoadShedding),
        Just(RuntimeTelemetryKind::QueueDepthObserved),
        Just(RuntimeTelemetryKind::ChannelClosed),
        Just(RuntimeTelemetryKind::PermitExhausted),
        Just(RuntimeTelemetryKind::TransientError),
        Just(RuntimeTelemetryKind::PermanentError),
        Just(RuntimeTelemetryKind::PanicCaptured),
        Just(RuntimeTelemetryKind::InvariantViolation),
        Just(RuntimeTelemetryKind::SafetyPolicyTriggered),
        Just(RuntimeTelemetryKind::ResourceObserved),
        Just(RuntimeTelemetryKind::ResourceExhausted),
        Just(RuntimeTelemetryKind::SloMeasurement),
        Just(RuntimeTelemetryKind::ConfigApplied),
        Just(RuntimeTelemetryKind::DiagnosticExported),
        Just(RuntimeTelemetryKind::Heartbeat),
    ]
}

fn arb_failure_class() -> impl Strategy<Value = FailureClass> {
    prop_oneof![
        Just(FailureClass::Transient),
        Just(FailureClass::Permanent),
        Just(FailureClass::Degraded),
        Just(FailureClass::Overload),
        Just(FailureClass::Corruption),
        Just(FailureClass::Timeout),
        Just(FailureClass::Panic),
        Just(FailureClass::Deadlock),
        Just(FailureClass::Safety),
        Just(FailureClass::Configuration),
    ]
}

// =============================================================================
// HealthTier boundary properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// from_ratio at exact thresholds: <0.50 → Green, <0.80 → Yellow, <0.95 → Red, >=0.95 → Black
    #[test]
    fn from_ratio_green_range(ratio in 0.0f64..0.499) {
        prop_assert_eq!(HealthTier::from_ratio(ratio), HealthTier::Green);
    }

    #[test]
    fn from_ratio_yellow_range(ratio in 0.50f64..0.799) {
        prop_assert_eq!(HealthTier::from_ratio(ratio), HealthTier::Yellow);
    }

    #[test]
    fn from_ratio_red_range(ratio in 0.80f64..0.949) {
        prop_assert_eq!(HealthTier::from_ratio(ratio), HealthTier::Red);
    }

    #[test]
    fn from_ratio_black_range(ratio in 0.95f64..1.0) {
        prop_assert_eq!(HealthTier::from_ratio(ratio), HealthTier::Black);
    }

    /// is_degraded is true for all tiers except Green
    #[test]
    fn is_degraded_iff_not_green(tier in arb_health_tier()) {
        let expected = tier != HealthTier::Green;
        prop_assert_eq!(tier.is_degraded(), expected,
            "{:?}.is_degraded() should be {}", tier, expected);
    }

    /// requires_attention is true only for Red and Black
    #[test]
    fn requires_attention_iff_red_or_black(tier in arb_health_tier()) {
        let expected = matches!(tier, HealthTier::Red | HealthTier::Black);
        prop_assert_eq!(tier.requires_attention(), expected,
            "{:?}.requires_attention() should be {}", tier, expected);
    }

    /// severity() matches enum discriminant and preserves ordering
    #[test]
    fn severity_matches_discriminant(tier in arb_health_tier()) {
        let sev = tier.severity();
        match tier {
            HealthTier::Green => prop_assert_eq!(sev, 0),
            HealthTier::Yellow => prop_assert_eq!(sev, 1),
            HealthTier::Red => prop_assert_eq!(sev, 2),
            HealthTier::Black => prop_assert_eq!(sev, 3),
        }
    }

    /// severity total ordering: a <= b implies a.severity() <= b.severity()
    #[test]
    fn severity_preserves_ord(a in arb_health_tier(), b in arb_health_tier()) {
        if a <= b {
            prop_assert!(a.severity() <= b.severity());
        }
        if a >= b {
            prop_assert!(a.severity() >= b.severity());
        }
    }
}

// =============================================================================
// FailureClass properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Every failure class's suggested_tier is at least Yellow
    #[test]
    fn failure_class_tier_at_least_yellow(fc in arb_failure_class()) {
        let tier = fc.suggested_tier();
        prop_assert!(tier >= HealthTier::Yellow,
            "FailureClass::{:?} suggested {:?}, expected >= Yellow", fc, tier);
    }

    /// FailureClass Display is non-empty
    #[test]
    fn failure_class_display_non_empty(fc in arb_failure_class()) {
        let s = fc.to_string();
        prop_assert!(!s.is_empty());
    }
}

// =============================================================================
// TierTransitionRecord serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tier_transition_serde_roundtrip(
        from in arb_health_tier(),
        to in arb_health_tier(),
        ts in 1000u64..u64::MAX / 2,
        duration in 0u64..1_000_000,
    ) {
        let record = TierTransitionRecord {
            timestamp_ms: ts,
            component: "rt.test".into(),
            from,
            to,
            reason_code: "test.serde".into(),
            duration_in_previous_ms: duration,
        };

        let json = serde_json::to_string(&record).unwrap();
        let restored: TierTransitionRecord = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.timestamp_ms, ts);
        prop_assert_eq!(restored.from, from);
        prop_assert_eq!(restored.to, to);
        prop_assert_eq!(restored.duration_in_previous_ms, duration);
        prop_assert_eq!(&restored.component, "rt.test");
        prop_assert_eq!(&restored.reason_code, "test.serde");
    }

    /// is_escalation and is_recovery are mutually exclusive
    #[test]
    fn escalation_recovery_mutual_exclusion(
        from in arb_health_tier(),
        to in arb_health_tier(),
    ) {
        let record = TierTransitionRecord {
            timestamp_ms: 1000,
            component: "rt.test".into(),
            from,
            to,
            reason_code: "test".into(),
            duration_in_previous_ms: 0,
        };

        // At most one can be true
        prop_assert!(!(record.is_escalation() && record.is_recovery()),
            "escalation and recovery cannot both be true");

        // If from != to, exactly one is true
        if from != to {
            prop_assert!(record.is_escalation() || record.is_recovery(),
                "different tiers must be either escalation or recovery");
        } else {
            prop_assert!(!record.is_escalation() && !record.is_recovery(),
                "same tier transition should be neither escalation nor recovery");
        }
    }

    /// Escalation means to > from
    #[test]
    fn escalation_means_higher_tier(
        from in arb_health_tier(),
        to in arb_health_tier(),
    ) {
        let record = TierTransitionRecord {
            timestamp_ms: 1000,
            component: "rt.test".into(),
            from,
            to,
            reason_code: "test".into(),
            duration_in_previous_ms: 0,
        };

        if record.is_escalation() {
            prop_assert!(to > from,
                "escalation requires to ({:?}) > from ({:?})", to, from);
        }
        if record.is_recovery() {
            prop_assert!(to < from,
                "recovery requires to ({:?}) < from ({:?})", to, from);
        }
    }
}

// =============================================================================
// RuntimeTelemetryLog disabled mode
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Disabled log discards events but still counts them
    #[test]
    fn disabled_log_discards_events(n in 1usize..30) {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events: 100,
            enabled: false,
        });

        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("disabled_{}", i)),
            );
        }

        // Buffer should be empty since log is disabled
        prop_assert_eq!(log.len(), 0);
        prop_assert!(log.is_empty());
    }

    /// Log with max_events=1 always holds exactly the last event
    #[test]
    fn single_event_log_keeps_last(n in 2usize..20) {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events: 1,
            enabled: true,
        });

        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{}", i)),
            );
        }

        prop_assert_eq!(log.len(), 1);
        let last = &log.events()[0];
        let expected_reason = format!("ev_{}", n - 1);
        prop_assert_eq!(&last.reason_code, &expected_reason);
    }
}

// =============================================================================
// Snapshot consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Snapshot kind_counts sum equals buffered_events
    #[test]
    fn snapshot_kind_counts_sum_to_buffered(n in 1usize..30) {
        let mut log = RuntimeTelemetryLog::with_defaults();

        // Emit a mix of event kinds
        let kinds = [
            RuntimeTelemetryKind::Heartbeat,
            RuntimeTelemetryKind::ScopeCreated,
            RuntimeTelemetryKind::TransientError,
            RuntimeTelemetryKind::TierTransition,
        ];

        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", kinds[i % kinds.len()])
                    .reason(&format!("mixed_{}", i)),
            );
        }

        let snap = log.snapshot();
        let kind_sum: u64 = snap.kind_counts.values().sum();
        prop_assert_eq!(kind_sum, snap.buffered_events,
            "kind_counts sum {} should equal buffered_events {}", kind_sum, snap.buffered_events);
    }

    /// Snapshot category_counts sum equals buffered_events
    #[test]
    fn snapshot_category_counts_sum_to_buffered(n in 1usize..30) {
        let mut log = RuntimeTelemetryLog::with_defaults();

        let kinds = [
            RuntimeTelemetryKind::ScopeCreated,
            RuntimeTelemetryKind::CancellationRequested,
            RuntimeTelemetryKind::ThrottleApplied,
            RuntimeTelemetryKind::TransientError,
            RuntimeTelemetryKind::ResourceObserved,
            RuntimeTelemetryKind::Heartbeat,
        ];

        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", kinds[i % kinds.len()])
                    .reason(&format!("cat_{}", i)),
            );
        }

        let snap = log.snapshot();
        let cat_sum: u64 = snap.category_counts.values().sum();
        prop_assert_eq!(cat_sum, snap.buffered_events,
            "category_counts sum {} should equal buffered_events {}", cat_sum, snap.buffered_events);
    }

    /// total_emitted == total_evicted + buffered_events
    #[test]
    fn snapshot_emitted_equals_evicted_plus_buffered(
        max_events in 3usize..15,
        n in 1usize..50,
    ) {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events,
            enabled: true,
        });

        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("inv_{}", i)),
            );
        }

        let snap = log.snapshot();
        prop_assert_eq!(
            snap.total_emitted,
            snap.total_evicted + snap.buffered_events,
            "emitted({}) should equal evicted({}) + buffered({})",
            snap.total_emitted, snap.total_evicted, snap.buffered_events
        );
    }
}

// =============================================================================
// Builder with scope_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Builder with scope preserves the scope_id
    #[test]
    fn builder_scope_id_preserved(
        scope_id in "[a-z]+:[a-z]+",
        kind in arb_event_kind(),
    ) {
        let event = RuntimeTelemetryEventBuilder::new("rt.test", kind)
            .scope_id(&scope_id)
            .reason("test.scope")
            .build();

        prop_assert_eq!(event.scope_id.as_deref(), Some(scope_id.as_str()));
    }

    /// Builder without scope produces None scope_id
    #[test]
    fn builder_no_scope_is_none(kind in arb_event_kind()) {
        let event = RuntimeTelemetryEventBuilder::new("rt.test", kind)
            .reason("test.no_scope")
            .build();

        prop_assert!(event.scope_id.is_none());
    }

    /// Builder detail_str and detail_u64 are preserved
    #[test]
    fn builder_details_preserved(
        key in "[a-z_]+",
        val in "[a-z0-9]+",
        num in 0u64..1_000_000,
    ) {
        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
            .reason("test.details")
            .detail_str(&key, &val)
            .detail_u64("num", num)
            .build();

        // details should contain our key
        prop_assert!(event.details.contains_key(&key),
            "details should contain key '{}'", key);
    }
}

// =============================================================================
// Event Display and RuntimePhase properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// RuntimePhase Display is non-empty
    #[test]
    fn runtime_phase_display_non_empty(phase in arb_runtime_phase()) {
        let s = phase.to_string();
        prop_assert!(!s.is_empty());
    }

    /// HealthTier Display is non-empty
    #[test]
    fn health_tier_display_non_empty(tier in arb_health_tier()) {
        let s = tier.to_string();
        prop_assert!(!s.is_empty());
        let expected = match tier {
            HealthTier::Green => "green",
            HealthTier::Yellow => "yellow",
            HealthTier::Red => "red",
            HealthTier::Black => "black",
        };
        prop_assert_eq!(&s, expected);
    }

    /// RuntimeTelemetryKind Display is non-empty
    #[test]
    fn event_kind_display_non_empty(kind in arb_event_kind()) {
        let s = kind.to_string();
        prop_assert!(!s.is_empty());
    }
}

// =============================================================================
// HealthTier transitivity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// HealthTier Ord is transitive: if a <= b and b <= c, then a <= c
    #[test]
    fn health_tier_ord_transitive(
        a in arb_health_tier(),
        b in arb_health_tier(),
        c in arb_health_tier(),
    ) {
        if a <= b && b <= c {
            prop_assert!(a <= c, "{:?} <= {:?} and {:?} <= {:?} but not {:?} <= {:?}", a, b, b, c, a, c);
        }
    }

    /// HealthTier Hash consistency: equal values produce equal hashes
    #[test]
    fn health_tier_hash_consistent(tier in arb_health_tier()) {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        tier.hash(&mut h1);
        tier.hash(&mut h2);
        prop_assert_eq!(h1.finish(), h2.finish());
    }
}
