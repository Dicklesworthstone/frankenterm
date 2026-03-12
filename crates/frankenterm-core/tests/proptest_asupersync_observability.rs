//! Property-based tests for the `asupersync_observability` module.
//!
//! Covers serde roundtrips and structural invariants for
//! `AsupersyncObservabilityConfig`, `AsupersyncTelemetrySnapshot`,
//! `AsupersyncIncidentContext`, and `SloBreachSummary`.

use frankenterm_core::asupersync_observability::{
    AsupersyncIncidentContext, AsupersyncObservabilityConfig, AsupersyncTelemetrySnapshot,
    SloBreachSummary,
};
use frankenterm_core::runtime_slo_gates::{GateVerdict, RuntimeAlertTier};
use frankenterm_core::runtime_telemetry::{HealthTier, RuntimePhase};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

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

fn arb_gate_verdict() -> impl Strategy<Value = GateVerdict> {
    prop_oneof![
        Just(GateVerdict::Pass),
        Just(GateVerdict::ConditionalPass),
        Just(GateVerdict::Fail),
    ]
}

fn arb_runtime_alert_tier() -> impl Strategy<Value = RuntimeAlertTier> {
    prop_oneof![
        Just(RuntimeAlertTier::Info),
        Just(RuntimeAlertTier::Warning),
        Just(RuntimeAlertTier::Critical),
        Just(RuntimeAlertTier::Page),
    ]
}

fn arb_observability_config() -> impl Strategy<Value = AsupersyncObservabilityConfig> {
    (
        any::<bool>(),
        100..10_000u64,
        100..5_000u64,
        500..10_000u64,
        4..64u32,
        100..2_000u64,
        500..5_000u64,
        10..100u64,
        25..200u64,
        64..1024u64,
    )
        .prop_map(
            |(enabled, sample_ms, scope_y, scope_r, depth, queue_w, queue_c, cancel_w, cancel_c, chan_d)| {
                AsupersyncObservabilityConfig {
                    enabled,
                    sample_interval_ms: sample_ms,
                    scope_count_yellow: scope_y,
                    scope_count_red: scope_r.max(scope_y + 1),
                    scope_depth_warn: depth,
                    queue_backlog_warn: queue_w,
                    queue_backlog_critical: queue_c.max(queue_w + 1),
                    cancel_latency_warn_ms: cancel_w,
                    cancel_latency_critical_ms: cancel_c.max(cancel_w + 1),
                    channel_depth_warn: chan_d,
                    task_leak_ratio_warn: 0.0005,
                    task_leak_ratio_critical: 0.001,
                    lock_contention_warn: 0.10,
                    recovery_time_critical_ms: 5000,
                    gate_enforcement_enabled: true,
                    min_gate_samples: 10,
                }
            },
        )
}

fn arb_telemetry_snapshot() -> impl Strategy<Value = AsupersyncTelemetrySnapshot> {
    (
        // Scope tree
        (0..10_000u64, 0..10_000u64, 0..100u64, 0..5_000u64),
        // Tasks
        (0..100_000u64, 0..100_000u64, 0..10_000u64, 0..100u64, 0..100u64),
        // Cancellation
        (0..10_000u64, 0..10_000u64, 0..1_000_000u64, 0..100_000u64, 0..1_000u64),
        // Channels + locks
        (0..100_000u64, 0..100_000u64, 0..1_000u64, 0..1_000u64),
        // Locks cont
        (0..100_000u64, 0..10_000u64, 0..100u64),
        // Permits
        (0..10_000u64, 0..1_000u64, 0..100_000u64),
        // Recovery
        (0..1_000u64, 0..1_000u64, 0..100u64, 0..10_000u64),
        // Health + gate
        (0..10_000u64, 0..10_000u64, 0..5_000u64, 0..2_000u64, 0..500u64),
        (0..10_000u64, 0..10_000u64, 0..5_000u64, 0..2_000u64),
    )
        .prop_map(
            |(scope, tasks, cancel, chan, locks, permits, recovery, health, gate)| {
                AsupersyncTelemetrySnapshot {
                    scopes_created: scope.0,
                    scopes_destroyed: scope.1,
                    scope_max_depth: scope.2,
                    scope_max_active: scope.3,
                    tasks_spawned: tasks.0,
                    tasks_completed: tasks.1,
                    tasks_cancelled: tasks.2,
                    tasks_leaked: tasks.3,
                    tasks_panicked: tasks.4,
                    cancel_requests: cancel.0,
                    cancel_completions: cancel.1,
                    cancel_latency_sum_us: cancel.2,
                    cancel_latency_max_us: cancel.3,
                    cancel_grace_expirations: cancel.4,
                    channel_sends: chan.0,
                    channel_recvs: chan.1,
                    channel_send_failures: chan.2,
                    channel_max_depth: chan.3,
                    lock_acquisitions: locks.0,
                    lock_contentions: locks.1,
                    lock_timeout_failures: locks.2,
                    permit_acquisitions: permits.0,
                    permit_timeouts: permits.1,
                    permit_max_wait_us: permits.2,
                    recovery_attempts: recovery.0,
                    recovery_successes: recovery.1,
                    recovery_failures: recovery.2,
                    recovery_latency_max_ms: recovery.3,
                    health_samples: health.0,
                    health_green_samples: health.1,
                    health_yellow_samples: health.2,
                    health_red_samples: health.3,
                    health_black_samples: health.4,
                    gate_evaluations: gate.0,
                    gate_passes: gate.1,
                    gate_conditional_passes: gate.2,
                    gate_failures: gate.3,
                }
            },
        )
}

fn arb_slo_breach_summary() -> impl Strategy<Value = SloBreachSummary> {
    (
        "[A-Z]{2,5}-[0-9]{3}",
        0.0..100.0f64,
        0.0..100.0f64,
        0..100_000u64,
        arb_runtime_alert_tier(),
        any::<bool>(),
    )
        .prop_map(|(slo_id, measured, target, duration, alert_tier, critical)| SloBreachSummary {
            slo_id,
            measured,
            target,
            breach_duration_ms: duration,
            alert_tier,
            critical,
        })
}

fn arb_incident_context() -> impl Strategy<Value = AsupersyncIncidentContext> {
    (
        arb_runtime_phase(),
        arb_health_tier(),
        arb_telemetry_snapshot(),
        proptest::option::of(arb_gate_verdict()),
        proptest::collection::vec(arb_slo_breach_summary(), 0..3),
        0..1_000_000u64,
    )
        .prop_map(
            |(phase, health_tier, telemetry, verdict, breaches, uptime)| {
                AsupersyncIncidentContext {
                    phase,
                    health_tier,
                    telemetry,
                    last_gate_verdict: verdict,
                    active_breaches: breaches,
                    uptime_ms: uptime,
                }
            },
        )
}

// =========================================================================
// AsupersyncObservabilityConfig serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn observability_config_serde_roundtrip(config in arb_observability_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: AsupersyncObservabilityConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, config.enabled);
        prop_assert_eq!(back.sample_interval_ms, config.sample_interval_ms);
        prop_assert_eq!(back.scope_count_yellow, config.scope_count_yellow);
        prop_assert_eq!(back.scope_count_red, config.scope_count_red);
        prop_assert_eq!(back.scope_depth_warn, config.scope_depth_warn);
        prop_assert_eq!(back.queue_backlog_warn, config.queue_backlog_warn);
        prop_assert_eq!(back.queue_backlog_critical, config.queue_backlog_critical);
    }
}

// =========================================================================
// AsupersyncTelemetrySnapshot serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn telemetry_snapshot_serde_roundtrip(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: AsupersyncTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, snap);
    }
}

// =========================================================================
// SloBreachSummary serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn slo_breach_summary_serde_roundtrip(breach in arb_slo_breach_summary()) {
        let json = serde_json::to_string(&breach).unwrap();
        let back: SloBreachSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.slo_id, &breach.slo_id);
        prop_assert_eq!(back.breach_duration_ms, breach.breach_duration_ms);
        prop_assert_eq!(back.critical, breach.critical);
        // f64 tolerance
        let diff = (back.measured - breach.measured).abs();
        prop_assert!(diff < 0.001, "measured drift: {}", diff);
    }
}

// =========================================================================
// AsupersyncIncidentContext serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn incident_context_serde_roundtrip(ctx in arb_incident_context()) {
        let json = serde_json::to_string(&ctx).unwrap();
        let back: AsupersyncIncidentContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.uptime_ms, ctx.uptime_ms);
        prop_assert_eq!(back.active_breaches.len(), ctx.active_breaches.len());
        prop_assert_eq!(back.last_gate_verdict, ctx.last_gate_verdict);
        prop_assert_eq!(back.telemetry, ctx.telemetry);
    }
}

// =========================================================================
// Telemetry snapshot computed metrics
// =========================================================================

proptest! {
    #[test]
    fn task_leak_ratio_in_range(snap in arb_telemetry_snapshot()) {
        let ratio = snap.task_leak_ratio();
        prop_assert!(ratio >= 0.0);
        prop_assert!(ratio <= 1.0 || snap.tasks_leaked > snap.tasks_spawned);
    }

    #[test]
    fn lock_contention_ratio_in_range(snap in arb_telemetry_snapshot()) {
        let ratio = snap.lock_contention_ratio();
        prop_assert!(ratio >= 0.0);
        prop_assert!(ratio <= 1.0 || snap.lock_contentions > snap.lock_acquisitions);
    }

    #[test]
    fn channel_failure_ratio_in_range(snap in arb_telemetry_snapshot()) {
        let ratio = snap.channel_failure_ratio();
        prop_assert!(ratio >= 0.0);
        prop_assert!(ratio <= 1.0 || snap.channel_send_failures > snap.channel_sends);
    }

    #[test]
    fn recovery_success_ratio_non_negative(snap in arb_telemetry_snapshot()) {
        let ratio = snap.recovery_success_ratio();
        prop_assert!(ratio >= 0.0);
        // When no attempts, returns 1.0; otherwise successes/attempts (may exceed 1.0
        // with unconstrained random data, which is fine — just verify no panic).
        if snap.recovery_attempts == 0 {
            let diff = (ratio - 1.0).abs();
            prop_assert!(diff < f64::EPSILON);
        }
    }

    #[test]
    fn gate_pass_ratio_non_negative(snap in arb_telemetry_snapshot()) {
        let ratio = snap.gate_pass_ratio();
        prop_assert!(ratio >= 0.0);
        if snap.gate_evaluations == 0 {
            let diff = (ratio - 1.0).abs();
            prop_assert!(diff < f64::EPSILON);
        }
    }

    #[test]
    fn health_distribution_non_negative(snap in arb_telemetry_snapshot()) {
        let dist = snap.health_distribution();
        for d in &dist {
            prop_assert!(*d >= 0.0);
        }
        // When health_samples is 0, distribution is [1.0, 0.0, 0.0, 0.0]
        if snap.health_samples == 0 {
            let diff = (dist[0] - 1.0).abs();
            prop_assert!(diff < f64::EPSILON);
        }
    }

    #[test]
    fn overall_health_tier_is_valid(
        snap in arb_telemetry_snapshot(),
        config in arb_observability_config(),
    ) {
        let tier = snap.overall_health_tier(&config);
        // Just verify it doesn't panic and returns a valid tier
        let check = matches!(tier, HealthTier::Green | HealthTier::Yellow | HealthTier::Red | HealthTier::Black);
        prop_assert!(check);
    }
}

// =========================================================================
// Default config
// =========================================================================

#[test]
fn default_config_enabled() {
    let config = AsupersyncObservabilityConfig::default();
    assert!(config.enabled);
    assert!(config.gate_enforcement_enabled);
    assert_eq!(config.sample_interval_ms, 1000);
}

#[test]
fn default_config_serde_roundtrip() {
    let config = AsupersyncObservabilityConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let back: AsupersyncObservabilityConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.enabled, config.enabled);
    assert_eq!(back.sample_interval_ms, config.sample_interval_ms);
    assert_eq!(back.scope_count_yellow, config.scope_count_yellow);
    assert_eq!(back.scope_count_red, config.scope_count_red);
}
