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
    RuntimeTelemetryLogConfig, ScopeTelemetryEmitter, TelemetryLogSnapshot, TierTransitionRecord,
    UnifiedTelemetryRecord, UnifiedTelemetrySource,
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

    // ── Filter properties ──

    #[test]
    fn filter_by_kind_returns_only_matching(
        target_kind in arb_event_kind(),
        n in 2usize..15,
    ) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let mut expected_count = 0;
        for i in 0..n {
            let kind = if i % 3 == 0 { target_kind } else { RuntimeTelemetryKind::ScopeCreated };
            if kind == target_kind { expected_count += 1; }
            log.emit(
                RuntimeTelemetryEventBuilder::new(&format!("comp-{i}"), kind)
                    .tier(HealthTier::Green)
            );
        }
        let filtered = log.filter_by_kind(target_kind);
        prop_assert_eq!(filtered.len(), expected_count);
        for event in filtered {
            prop_assert_eq!(event.event_kind, target_kind);
        }
    }

    #[test]
    fn filter_by_tier_returns_only_matching(
        target_tier in arb_health_tier(),
        n in 2usize..15,
    ) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let mut expected_count = 0;
        let tiers = [HealthTier::Green, HealthTier::Yellow, HealthTier::Red, HealthTier::Black];
        for i in 0..n {
            let tier = tiers[i % 4];
            if tier == target_tier { expected_count += 1; }
            log.emit(
                RuntimeTelemetryEventBuilder::new("comp", RuntimeTelemetryKind::ScopeCreated)
                    .tier(tier)
            );
        }
        let filtered = log.filter_by_tier(target_tier);
        prop_assert_eq!(filtered.len(), expected_count);
        for event in filtered {
            prop_assert_eq!(event.health_tier, target_tier);
        }
    }

    #[test]
    fn filter_by_component_prefix(prefix in "[a-z]{2,5}", n in 2usize..10) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let mut expected_count = 0;
        for i in 0..n {
            let component = if i % 2 == 0 {
                format!("{prefix}::sub-{i}")
            } else {
                format!("other::sub-{i}")
            };
            if component.starts_with(&prefix) { expected_count += 1; }
            log.emit(
                RuntimeTelemetryEventBuilder::new(&component, RuntimeTelemetryKind::ScopeCreated)
                    .tier(HealthTier::Green)
            );
        }
        let filtered = log.filter_by_component(&prefix);
        prop_assert_eq!(filtered.len(), expected_count);
    }

    #[test]
    fn filter_by_correlation_isolates_events(
        corr_id in "[a-z0-9-]{5,15}",
        n in 3usize..10,
    ) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let mut expected = 0;
        for i in 0..n {
            let builder = RuntimeTelemetryEventBuilder::new(
                "comp", RuntimeTelemetryKind::ScopeCreated
            ).tier(HealthTier::Green);
            let builder = if i % 3 == 0 {
                expected += 1;
                builder.correlation(&corr_id)
            } else {
                builder.correlation(&format!("other-{i}"))
            };
            log.emit(builder);
        }
        let filtered = log.filter_by_correlation(&corr_id);
        prop_assert_eq!(filtered.len(), expected);
    }

    // ── Drain empties the log ──

    #[test]
    fn drain_empties_and_returns_all(n in 1usize..20) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new(
                    &format!("drain-{i}"),
                    RuntimeTelemetryKind::ScopeCreated,
                ).tier(HealthTier::Green)
            );
        }
        prop_assert_eq!(log.len(), n);
        let drained = log.drain();
        prop_assert_eq!(drained.len(), n);
        prop_assert!(log.is_empty());
        prop_assert_eq!(log.len(), 0);
        // total_emitted should still reflect history
        prop_assert_eq!(log.total_emitted(), n as u64);
    }

    // ── Count helpers match filter counts ──

    #[test]
    fn count_tier_matches_filter(n in 2usize..15) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let tiers = [HealthTier::Green, HealthTier::Yellow, HealthTier::Red, HealthTier::Black];
        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("comp", RuntimeTelemetryKind::ScopeCreated)
                    .tier(tiers[i % 4])
            );
        }
        for tier in &tiers {
            let count = log.count_tier(*tier);
            let filter_count = log.filter_by_tier(*tier).len();
            prop_assert_eq!(count, filter_count);
        }
    }

    // ── Unified telemetry normalization ──

    #[test]
    fn unified_from_runtime_preserves_core_fields(
        kind in arb_event_kind(),
        tier in arb_health_tier(),
        component in "[a-z]{3,10}",
    ) {
        let event = RuntimeTelemetryEventBuilder::new(&component, kind)
            .tier(tier)
            .correlation("corr-test")
            .scope_id("scope-test")
            .build();
        let record = UnifiedTelemetryRecord::from_runtime_event(&event);
        let check_source = matches!(record.source, UnifiedTelemetrySource::Runtime);
        prop_assert!(check_source, "source must be Runtime");
        prop_assert_eq!(record.component, component);
        prop_assert_eq!(record.health_tier, tier);
        prop_assert_eq!(record.timestamp_ms, event.timestamp_ms);
        prop_assert_eq!(record.correlation_id.as_deref(), Some("corr-test"));
        prop_assert_eq!(record.scope_id.as_deref(), Some("scope-test"));
        prop_assert!(!record.record_id.is_empty());
        prop_assert!(!record.schema_version.is_empty());
    }

    #[test]
    fn unified_record_serde_roundtrip(
        kind in arb_event_kind(),
        tier in arb_health_tier(),
    ) {
        let event = RuntimeTelemetryEventBuilder::new("component", kind)
            .tier(tier)
            .build();
        let record = UnifiedTelemetryRecord::from_runtime_event(&event);
        let json = serde_json::to_string(&record).unwrap();
        let decoded: UnifiedTelemetryRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(record.record_id, decoded.record_id);
        prop_assert_eq!(record.component, decoded.component);
        prop_assert_eq!(record.health_tier, decoded.health_tier);
        prop_assert_eq!(record.timestamp_ms, decoded.timestamp_ms);
    }

    // ── Eviction counters ──

    #[test]
    fn eviction_counter_tracks_overflow(capacity in 5usize..20, total in 20usize..50) {
        let config = RuntimeTelemetryLogConfig {
            max_events: capacity,
            ..RuntimeTelemetryLogConfig::default()
        };
        let mut log = RuntimeTelemetryLog::new(config);
        for i in 0..total {
            log.emit(
                RuntimeTelemetryEventBuilder::new(
                    &format!("evict-{i}"),
                    RuntimeTelemetryKind::ScopeCreated,
                ).tier(HealthTier::Green)
            );
        }
        prop_assert_eq!(log.total_emitted(), total as u64);
        let expected_evicted = if total > capacity { (total - capacity) as u64 } else { 0 };
        prop_assert_eq!(log.total_evicted(), expected_evicted);
        prop_assert!(log.len() <= capacity);
    }
}

// =============================================================================
// Additional coverage tests (RT-28 through RT-45)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── RT-28: RuntimePhase::is_terminal ────────────────────────────────────

    #[test]
    fn rt28_phase_is_terminal(phase in arb_runtime_phase()) {
        let expected = phase == RuntimePhase::Shutdown;
        prop_assert_eq!(phase.is_terminal(), expected);
    }

    // ── RT-29: RuntimePhase::is_shutting_down ───────────────────────────────

    #[test]
    fn rt29_phase_is_shutting_down(phase in arb_runtime_phase()) {
        let expected = matches!(
            phase,
            RuntimePhase::Draining | RuntimePhase::Finalizing | RuntimePhase::Cancelling
        );
        prop_assert_eq!(phase.is_shutting_down(), expected);
    }

    // ── RT-30: HealthTier::requires_attention ───────────────────────────────

    #[test]
    fn rt30_requires_attention(tier in arb_health_tier()) {
        let expected = matches!(tier, HealthTier::Red | HealthTier::Black);
        prop_assert_eq!(tier.requires_attention(), expected);
    }

    // ── RT-31: FailureClass::is_retryable ───────────────────────────────────

    #[test]
    fn rt31_is_retryable(fc in arb_failure_class()) {
        let expected = matches!(fc, FailureClass::Transient | FailureClass::Degraded | FailureClass::Timeout);
        prop_assert_eq!(fc.is_retryable(), expected);
    }

    // ── RT-32: UnifiedTelemetrySource serde roundtrip ───────────────────────

    #[test]
    fn rt32_unified_source_serde(idx in 0u8..3) {
        let source = match idx {
            0 => UnifiedTelemetrySource::Runtime,
            1 => UnifiedTelemetrySource::Connector,
            _ => UnifiedTelemetrySource::Policy,
        };
        let json = serde_json::to_string(&source).unwrap();
        let back: UnifiedTelemetrySource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(source, back);
    }

    // ── RT-33: TelemetryLogSnapshot serde roundtrip ─────────────────────────

    #[test]
    fn rt33_snapshot_serde(n in 1usize..20) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let tiers = [HealthTier::Green, HealthTier::Yellow, HealthTier::Red, HealthTier::Black];
        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(tiers[i % 4])
                    .reason("snap")
            );
        }
        let snap = log.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: TelemetryLogSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.buffered_events, back.buffered_events);
        prop_assert_eq!(snap.total_emitted, back.total_emitted);
        prop_assert_eq!(snap.total_evicted, back.total_evicted);
        prop_assert_eq!(snap.sequence, back.sequence);
        prop_assert_eq!(snap.tier_counts, back.tier_counts);
    }

    // ── RT-34: Builder detail_f64 preserves value ───────────────────────────

    #[test]
    fn rt34_detail_f64(val in -1000.0f64..1000.0) {
        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::ResourceObserved)
            .detail_f64("ratio", val)
            .build();
        let stored = event.details.get("ratio").and_then(|v| v.as_f64());
        prop_assert!(stored.is_some());
        prop_assert!((stored.unwrap() - val).abs() < 1e-10);
    }

    // ── RT-35: Builder detail_bool preserves value ──────────────────────────

    #[test]
    fn rt35_detail_bool(val in any::<bool>()) {
        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::ConfigApplied)
            .detail_bool("flag", val)
            .build();
        let stored = event.details.get("flag").and_then(|v| v.as_bool());
        prop_assert_eq!(stored, Some(val));
    }

    // ── RT-36: Builder failure() sets failure_class ─────────────────────────

    #[test]
    fn rt36_builder_failure_class(fc in arb_failure_class()) {
        let event = RuntimeTelemetryEventBuilder::new("rt.error", RuntimeTelemetryKind::TransientError)
            .failure(fc)
            .build();
        prop_assert_eq!(event.failure_class, Some(fc));
    }

    // ── RT-37: count_category matches manual filter ─────────────────────────

    #[test]
    fn rt37_count_category(n in 2usize..20) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let kinds = [
            RuntimeTelemetryKind::ScopeCreated,
            RuntimeTelemetryKind::CancellationRequested,
            RuntimeTelemetryKind::ThrottleApplied,
            RuntimeTelemetryKind::QueueDepthObserved,
            RuntimeTelemetryKind::TransientError,
            RuntimeTelemetryKind::ResourceObserved,
            RuntimeTelemetryKind::Heartbeat,
        ];
        for i in 0..n {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", kinds[i % kinds.len()])
                    .tier(HealthTier::Green)
            );
        }
        let categories = ["scope", "cancellation", "backpressure", "queue", "error", "resource", "operational"];
        let mut total = 0usize;
        for cat in &categories {
            total += log.count_category(cat);
        }
        prop_assert_eq!(total, log.len());
    }

    // ── RT-38: Disabled log ignores appends ─────────────────────────────────

    #[test]
    fn rt38_disabled_log(n in 1usize..20) {
        let config = RuntimeTelemetryLogConfig {
            max_events: 100,
            enabled: false,
        };
        let mut log = RuntimeTelemetryLog::new(config);
        for _ in 0..n {
            let seq = log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason("disabled")
            );
            prop_assert_eq!(seq, 0u64);
        }
        prop_assert!(log.is_empty());
        prop_assert_eq!(log.total_emitted(), 0);
    }

    // ── RT-39: ScopeTelemetryEmitter produces correct event kinds ───────────

    #[test]
    fn rt39_scope_emitter_event_kinds(
        scope_id in "[a-z]{3,8}",
        corr_id in "[a-z]{3,8}",
    ) {
        let emitter = ScopeTelemetryEmitter::new("rt.scope", &scope_id, &corr_id);
        prop_assert_eq!(emitter.created("daemon").event_kind, RuntimeTelemetryKind::ScopeCreated);
        prop_assert_eq!(emitter.started().event_kind, RuntimeTelemetryKind::ScopeStarted);
        prop_assert_eq!(emitter.draining("test").event_kind, RuntimeTelemetryKind::ScopeDraining);
        prop_assert_eq!(emitter.finalizing(1).event_kind, RuntimeTelemetryKind::ScopeFinalizing);
        prop_assert_eq!(emitter.closed(100).event_kind, RuntimeTelemetryKind::ScopeClosed);
    }

    // ── RT-40: CancellationTelemetryEmitter produces correct event kinds ────

    #[test]
    fn rt40_cancellation_emitter_event_kinds(
        corr_id in "[a-z]{3,8}",
    ) {
        let emitter = CancellationTelemetryEmitter::new("rt.cancel", &corr_id);
        prop_assert_eq!(
            emitter.requested("scope", "reason").event_kind,
            RuntimeTelemetryKind::CancellationRequested
        );
        prop_assert_eq!(
            emitter.propagated("parent", 5).event_kind,
            RuntimeTelemetryKind::CancellationPropagated
        );
        prop_assert_eq!(
            emitter.grace_expired("scope", 5000).event_kind,
            RuntimeTelemetryKind::GracePeriodExpired
        );
    }

    // ── RT-41: TierTransitionRecord serde roundtrip ─────────────────────────

    #[test]
    fn rt41_tier_transition_serde(
        from in arb_health_tier(),
        to in arb_health_tier(),
        ts in 1000u64..1_000_000_000,
        duration in 0u64..100_000,
    ) {
        let record = TierTransitionRecord {
            timestamp_ms: ts,
            component: "rt.bp".into(),
            from,
            to,
            reason_code: "test".into(),
            duration_in_previous_ms: duration,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: TierTransitionRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(record.from, back.from);
        prop_assert_eq!(record.to, back.to);
        prop_assert_eq!(record.timestamp_ms, back.timestamp_ms);
        prop_assert_eq!(record.duration_in_previous_ms, back.duration_in_previous_ms);
    }

    // ── RT-42: HealthTier Display lowercase ─────────────────────────────────

    #[test]
    fn rt42_health_tier_display(tier in arb_health_tier()) {
        let s = tier.to_string();
        let expected = match tier {
            HealthTier::Green => "green",
            HealthTier::Yellow => "yellow",
            HealthTier::Red => "red",
            HealthTier::Black => "black",
        };
        prop_assert_eq!(&s, expected);
    }

    // ── RT-43: RuntimePhase Display non-empty ───────────────────────────────

    #[test]
    fn rt43_phase_display(phase in arb_runtime_phase()) {
        let s = phase.to_string();
        prop_assert!(!s.is_empty());
    }

    // ── RT-44: FailureClass Display non-empty ───────────────────────────────

    #[test]
    fn rt44_failure_display(fc in arb_failure_class()) {
        let s = fc.to_string();
        prop_assert!(!s.is_empty());
    }

    // ── RT-45: RuntimeTelemetryLogConfig serde roundtrip ────────────────────

    #[test]
    fn rt45_config_serde(max_events in 1usize..10000, enabled in any::<bool>()) {
        let config = RuntimeTelemetryLogConfig {
            max_events,
            enabled,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: RuntimeTelemetryLogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.max_events, back.max_events);
        prop_assert_eq!(config.enabled, back.enabled);
    }

    // ── RT-46: PolicyMetricsDashboard adapter health tier mapping ─────────

    #[test]
    fn rt46_dashboard_adapter_health_tier(
        evals in 0..10000u64,
        denials in 0..10000u64,
        quarantines in 0..50u32,
        violations in 0..50u32,
        chain_valid in any::<bool>(),
        ks_active in any::<bool>(),
        now_ms in 0..u64::MAX / 2,
    ) {
        use frankenterm_core::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            active_quarantines: quarantines,
            active_violations: violations,
        });
        collector.update_audit_chain(100, chain_valid);
        collector.update_kill_switch(ks_active);
        let dash = collector.dashboard(now_ms);
        let record = UnifiedTelemetryRecord::from_policy_metrics_dashboard(&dash);

        // health tier must match dashboard health status
        let expected_tier = match dash.overall_health {
            HealthStatus::Healthy => HealthTier::Green,
            HealthStatus::Warning => HealthTier::Yellow,
            HealthStatus::Critical => HealthTier::Red,
            HealthStatus::Unknown => HealthTier::Black,
        };
        prop_assert_eq!(record.health_tier, expected_tier);

        // timestamp must pass through
        prop_assert_eq!(record.timestamp_ms, now_ms);

        // component must be policy.metrics_dashboard
        prop_assert_eq!(&record.component, "policy.metrics_dashboard");

        // serde roundtrip
        let json = serde_json::to_string(&record).unwrap();
        let back: UnifiedTelemetryRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(record.health_tier, back.health_tier);
        prop_assert_eq!(record.timestamp_ms, back.timestamp_ms);
    }

    // ── RT-47: Dashboard adapter failure class derivation ────────────────

    #[test]
    fn rt47_dashboard_failure_class_kill_switch(now_ms in 0..u64::MAX / 2) {
        use frankenterm_core::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_kill_switch(true);
        let dash = collector.dashboard(now_ms);
        let record = UnifiedTelemetryRecord::from_policy_metrics_dashboard(&dash);
        prop_assert_eq!(record.failure_class, Some(FailureClass::Safety));
    }

    // ── RT-48: Dashboard adapter counter passthrough ─────────────────────

    #[test]
    fn rt48_dashboard_counter_passthrough(
        evals in 0..10000u64,
        denials in 0..10000u64,
    ) {
        use frankenterm_core::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            ..Default::default()
        });
        let dash = collector.dashboard(1000);
        let record = UnifiedTelemetryRecord::from_policy_metrics_dashboard(&dash);
        prop_assert_eq!(&record.attributes["total_evaluations"], &serde_json::json!(evals));
        prop_assert_eq!(&record.attributes["total_denials"], &serde_json::json!(denials));
    }

    // ── RT-49: ComplianceSnapshot adapter health tier mapping ────────────

    #[test]
    fn rt49_compliance_adapter_produces_valid_record(
        denied in any::<bool>(),
        now_ms in 0..u64::MAX / 2,
    ) {
        use frankenterm_core::policy_compliance::*;
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_evaluation(denied);
        let snap = engine.snapshot(now_ms);
        let record = UnifiedTelemetryRecord::from_compliance_snapshot(&snap);
        prop_assert_eq!(&record.component, "policy.compliance_engine");
        prop_assert_eq!(record.timestamp_ms, now_ms);
        // Must be valid JSON
        let json = serde_json::to_string(&record).unwrap();
        let back: UnifiedTelemetryRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(record.component, back.component);
        prop_assert_eq!(record.health_tier, back.health_tier);
    }
}
