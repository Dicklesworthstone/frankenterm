//! Property tests for runtime telemetry schema (ft-e34d9.10.7.1).
//!
//! Validates structural invariants of the unified telemetry schema:
//! - Serde roundtrip fidelity for all enum/struct types
//! - Health tier ordering and threshold consistency
//! - Event builder determinism
//! - Log buffer FIFO eviction semantics
//! - Snapshot aggregation accuracy
//! - Reason code format compliance

use proptest::prelude::*;

use frankenterm_core::runtime_telemetry::{
    CancellationTelemetryEmitter, FailureClass, HealthTier, RuntimePhase,
    RuntimeTelemetryEventBuilder, RuntimeTelemetryKind, RuntimeTelemetryLog,
    RuntimeTelemetryLogConfig, ScopeTelemetryEmitter, TierTransitionRecord,
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

fn arb_component() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("rt.scope".to_string()),
        Just("rt.scope.capture".to_string()),
        Just("rt.backpressure".to_string()),
        Just("rt.cancellation".to_string()),
        Just("rt.storage".to_string()),
        Just("rt.network".to_string()),
        Just("rt.error".to_string()),
        Just("rt.ops".to_string()),
    ]
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── Serde roundtrip ──

    #[test]
    fn health_tier_serde_roundtrip(tier in arb_health_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let rt: HealthTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, tier);
    }

    #[test]
    fn runtime_phase_serde_roundtrip(phase in arb_runtime_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let rt: RuntimePhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, phase);
    }

    #[test]
    fn event_kind_serde_roundtrip(kind in arb_event_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let rt: RuntimeTelemetryKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, kind);
    }

    #[test]
    fn failure_class_serde_roundtrip(fc in arb_failure_class()) {
        let json = serde_json::to_string(&fc).unwrap();
        let rt: FailureClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, fc);
    }

    // ── Health tier ordering ──

    #[test]
    fn health_tier_severity_matches_ord(a in arb_health_tier(), b in arb_health_tier()) {
        // Severity ordering must match Ord ordering
        let ord_result = a.cmp(&b);
        let sev_result = a.severity().cmp(&b.severity());
        prop_assert_eq!(ord_result, sev_result,
            "Ord and severity() must agree: {:?} vs {:?}", a, b);
    }

    #[test]
    fn health_tier_from_ratio_monotonic(r1 in 0.0f64..1.0, r2 in 0.0f64..1.0) {
        // Higher ratios should produce equal or higher tiers
        if r1 <= r2 {
            let t1 = HealthTier::from_ratio(r1);
            let t2 = HealthTier::from_ratio(r2);
            prop_assert!(t1 <= t2,
                "from_ratio must be monotonic: ratio {:.3} → {:?}, ratio {:.3} → {:?}", r1, t1, r2, t2);
        }
    }

    // ── Event builder determinism ──

    #[test]
    fn builder_preserves_all_fields(
        component in arb_component(),
        kind in arb_event_kind(),
        tier in arb_health_tier(),
        phase in arb_runtime_phase(),
        reason in "[a-z]+\\.[a-z]+\\.[a-z_]+",
        corr_id in "[a-z]+-[0-9]+",
        ts in 1000u64..u64::MAX,
    ) {
        let event = RuntimeTelemetryEventBuilder::new(&component, kind)
            .tier(tier)
            .phase(phase)
            .reason(&reason)
            .correlation(&corr_id)
            .timestamp_ms(ts)
            .build();

        prop_assert_eq!(&event.component, &component);
        prop_assert_eq!(event.event_kind, kind);
        prop_assert_eq!(event.health_tier, tier);
        prop_assert_eq!(event.phase, phase);
        prop_assert_eq!(&event.reason_code, &reason);
        prop_assert_eq!(&event.correlation_id, &corr_id);
        prop_assert_eq!(event.timestamp_ms, ts);
    }

    #[test]
    fn event_json_roundtrip(
        component in arb_component(),
        kind in arb_event_kind(),
        tier in arb_health_tier(),
        phase in arb_runtime_phase(),
    ) {
        let event = RuntimeTelemetryEventBuilder::new(&component, kind)
            .tier(tier)
            .phase(phase)
            .reason("test.prop.check")
            .correlation("prop-1")
            .detail_str("key", "val")
            .detail_u64("num", 42)
            .build();

        let json_str = serde_json::to_string(&event).unwrap();
        let rt: frankenterm_core::runtime_telemetry::RuntimeTelemetryEvent =
            serde_json::from_str(&json_str).unwrap();

        prop_assert_eq!(&rt.component, &event.component);
        prop_assert_eq!(rt.event_kind, event.event_kind);
        prop_assert_eq!(rt.health_tier, event.health_tier);
        prop_assert_eq!(rt.phase, event.phase);
        prop_assert_eq!(&rt.reason_code, &event.reason_code);
        prop_assert_eq!(&rt.correlation_id, &event.correlation_id);
    }

    // ── Log FIFO eviction ──

    #[test]
    fn log_fifo_preserves_newest(
        max_events in 3usize..20,
        n_events in 1usize..50,
    ) {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events,
            enabled: true,
        });

        for i in 0..n_events {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("event_{}", i)),
            );
        }

        let expected_len = n_events.min(max_events);
        prop_assert_eq!(log.len(), expected_len);
        prop_assert_eq!(log.total_emitted(), n_events as u64);

        if n_events > max_events {
            prop_assert_eq!(log.total_evicted(), (n_events - max_events) as u64);
        }

        // The *last* event emitted should be the last in the buffer
        if !log.is_empty() {
            let last = &log.events()[log.len() - 1];
            let expected_reason = format!("event_{}", n_events - 1);
            prop_assert_eq!(&last.reason_code, &expected_reason);
        }
    }

    #[test]
    fn log_sequence_strictly_monotonic(n in 1usize..50) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let mut prev_seq = 0u64;

        for _ in 0..n {
            let seq = log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason("seq_test"),
            );
            prop_assert!(seq > prev_seq, "Sequence must be strictly monotonic");
            prev_seq = seq;
        }
    }

    // ── Snapshot aggregation ──

    #[test]
    fn snapshot_tier_counts_sum_to_total(
        n_green in 0usize..10,
        n_yellow in 0usize..10,
        n_red in 0usize..10,
        n_black in 0usize..10,
    ) {
        let mut log = RuntimeTelemetryLog::with_defaults();

        for _ in 0..n_green {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Green)
                    .reason("g"),
            );
        }
        for _ in 0..n_yellow {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Yellow)
                    .reason("y"),
            );
        }
        for _ in 0..n_red {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Red)
                    .reason("r"),
            );
        }
        for _ in 0..n_black {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Black)
                    .reason("b"),
            );
        }

        let snap = log.snapshot();
        let total_tiers: u64 = snap.tier_counts.iter().sum();
        prop_assert_eq!(total_tiers, snap.buffered_events);
        prop_assert_eq!(snap.tier_counts[0], n_green as u64);
        prop_assert_eq!(snap.tier_counts[1], n_yellow as u64);
        prop_assert_eq!(snap.tier_counts[2], n_red as u64);
        prop_assert_eq!(snap.tier_counts[3], n_black as u64);
    }

    // ── Failure class properties ──

    #[test]
    fn failure_class_suggested_tier_is_degraded(fc in arb_failure_class()) {
        let tier = fc.suggested_tier();
        // All failure classes suggest at least Yellow (degraded)
        prop_assert!(tier.is_degraded(),
            "FailureClass::{:?} suggested tier {:?} must be degraded", fc, tier);
    }

    // ── Tier transition ──

    #[test]
    fn tier_transition_escalation_xor_recovery(
        from in arb_health_tier(),
        to in arb_health_tier(),
    ) {
        let record = TierTransitionRecord {
            timestamp_ms: 1000,
            component: "rt.test".into(),
            from,
            to,
            reason_code: "test".into(),
            duration_in_previous_ms: 100,
        };

        if from == to {
            // Same tier: neither escalation nor recovery
            prop_assert!(!record.is_escalation());
            prop_assert!(!record.is_recovery());
        } else {
            // Different tiers: exactly one of escalation/recovery
            prop_assert!(
                record.is_escalation() ^ record.is_recovery(),
                "Transitions between different tiers must be either escalation or recovery"
            );
        }
    }

    #[test]
    fn tier_transition_to_event_preserves_fields(
        from in arb_health_tier(),
        to in arb_health_tier(),
        ts in 1000u64..u64::MAX,
        duration in 0u64..1_000_000,
    ) {
        let record = TierTransitionRecord {
            timestamp_ms: ts,
            component: "rt.backpressure".into(),
            from,
            to,
            reason_code: "test.transition".into(),
            duration_in_previous_ms: duration,
        };

        let event = record.to_event("corr-1");
        prop_assert_eq!(event.event_kind, RuntimeTelemetryKind::TierTransition);
        prop_assert_eq!(event.health_tier, to);
        prop_assert_eq!(event.timestamp_ms, ts);
        prop_assert_eq!(&event.correlation_id, "corr-1");
    }

    // ── Scope emitter consistency ──

    #[test]
    fn scope_emitter_all_events_share_correlation(
        scope_id in "[a-z]+:[a-z]+",
        corr_id in "[a-z]+-[0-9]+",
    ) {
        let emitter = ScopeTelemetryEmitter::new("rt.scope", &scope_id, &corr_id);

        let events = vec![
            emitter.created("daemon"),
            emitter.started(),
            emitter.draining("test"),
            emitter.finalizing(2),
            emitter.closed(1000),
        ];

        for event in &events {
            prop_assert_eq!(&event.correlation_id, &corr_id);
            prop_assert_eq!(event.scope_id.as_deref(), Some(scope_id.as_str()));
        }
    }

    // ── Cancellation emitter consistency ──

    #[test]
    fn cancellation_emitter_events_share_correlation(
        corr_id in "[a-z]+-[0-9]+",
    ) {
        let emitter = CancellationTelemetryEmitter::new("rt.cancel", &corr_id);

        let events = vec![
            emitter.requested("root", "user"),
            emitter.propagated("root", 3),
            emitter.grace_expired("daemon:capture", 5000),
        ];

        for event in &events {
            prop_assert_eq!(&event.correlation_id, &corr_id);
        }
    }

    // ── JSONL export ──

    #[test]
    fn export_jsonl_line_count_matches(n in 0usize..30) {
        let mut log = RuntimeTelemetryLog::with_defaults();

        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{}", i)),
            );
        }

        let jsonl = log.export_jsonl();
        if n == 0 {
            prop_assert!(jsonl.is_empty());
        } else {
            let line_count = jsonl.lines().count();
            prop_assert_eq!(line_count, n);
        }
    }

    // ── Event kind category exhaustive ──

    #[test]
    fn event_kind_category_is_known(kind in arb_event_kind()) {
        let cat = kind.category();
        let known = ["scope", "cancellation", "backpressure", "queue", "error", "resource", "operational"];
        prop_assert!(known.contains(&cat),
            "Event kind {:?} has unknown category '{}'", kind, cat);
    }
}
