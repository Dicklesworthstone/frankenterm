//! Property-based tests for connector_host_runtime state machine.
//!
//! Coverage targets:
//! - State machine transition invariants (no invalid state combos)
//! - Budget enforcement ↔ degraded-state bidirectional link
//! - Serde roundtrip for all public types
//! - Operation envelope monotonicity and format
//! - Heartbeat liveness / readiness invariants
//! - Transition history bounded at capacity (64)
//! - Config validation exhaustive edge cases
//!
//! ft-3681t.5.1 quality support slice.

use proptest::prelude::*;

use frankenterm_core::connector_host_runtime::{
    ConnectorFailureClass, ConnectorHostConfig, ConnectorHostRuntime, ConnectorHostRuntimeError,
    ConnectorLifecyclePhase, ConnectorProtocolVersion, ConnectorRuntimeBudgets,
    ConnectorRuntimeUsage, ConnectorSandboxZone, StartupProbeResult,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_failure_class() -> impl Strategy<Value = ConnectorFailureClass> {
    prop_oneof![
        Just(ConnectorFailureClass::Auth),
        Just(ConnectorFailureClass::Quota),
        Just(ConnectorFailureClass::Network),
        Just(ConnectorFailureClass::Policy),
        Just(ConnectorFailureClass::Validation),
        Just(ConnectorFailureClass::Timeout),
        Just(ConnectorFailureClass::Unknown),
    ]
}

fn arb_protocol_version() -> impl Strategy<Value = ConnectorProtocolVersion> {
    (1u16..100, 0u16..100, 0u16..100)
        .prop_map(|(major, minor, patch)| ConnectorProtocolVersion::new(major, minor, patch))
}

fn arb_valid_budgets() -> impl Strategy<Value = ConnectorRuntimeBudgets> {
    (
        1u32..2000,
        1u64..1_073_741_824,
        1u64..100_000_000,
        1u32..1024,
    )
        .prop_map(|(cpu, mem, io, ops)| ConnectorRuntimeBudgets {
            cpu_millis_per_second: cpu,
            memory_bytes: mem,
            io_bytes_per_second: io,
            max_inflight_ops: ops,
        })
}

fn arb_valid_config() -> impl Strategy<Value = ConnectorHostConfig> {
    (
        arb_valid_budgets(),
        1u64..60_000,
        1u64..30_000,
        1u64..60_000,
    )
        .prop_map(
            |(budgets, startup_timeout_ms, heartbeat_interval_ms, failure_backoff_ms)| {
                ConnectorHostConfig {
                    host_id: "proptest-host".to_string(),
                    protocol_version: ConnectorProtocolVersion::default(),
                    budgets,
                    startup_timeout_ms,
                    heartbeat_interval_ms,
                    failure_backoff_ms,
                    sandbox: ConnectorSandboxZone::default(),
                }
            },
        )
}

fn arb_usage_within(budgets: &ConnectorRuntimeBudgets) -> ConnectorRuntimeUsage {
    ConnectorRuntimeUsage {
        cpu_millis_in_window: budgets.cpu_millis_per_second.saturating_sub(1),
        memory_bytes: budgets.memory_bytes.saturating_sub(1),
        io_bytes_in_window: budgets.io_bytes_per_second.saturating_sub(1),
        inflight_ops: budgets.max_inflight_ops.saturating_sub(1),
    }
}

fn arb_usage_exceeding_cpu(budgets: &ConnectorRuntimeBudgets) -> ConnectorRuntimeUsage {
    ConnectorRuntimeUsage {
        cpu_millis_in_window: budgets.cpu_millis_per_second + 1,
        memory_bytes: 0,
        io_bytes_in_window: 0,
        inflight_ops: 0,
    }
}

// ---------------------------------------------------------------------------
// State machine transition invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Starting a stopped host always reaches Running on healthy probe.
    #[test]
    fn start_from_stopped_reaches_running(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        rt.start(100).unwrap();
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Running);
    }

    /// Starting a running host is rejected.
    #[test]
    fn double_start_is_rejected(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        rt.start(100).unwrap();
        let err = rt.start(200).unwrap_err();
        assert!(matches!(err, ConnectorHostRuntimeError::InvalidTransition { .. }));
        // State should remain Running — not corrupted
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Running);
    }

    /// Stop from running always reaches Stopped.
    #[test]
    fn stop_from_running_reaches_stopped(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        rt.start(100).unwrap();
        rt.stop(200).unwrap();
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Stopped);
    }

    /// Double stop is rejected.
    #[test]
    fn double_stop_is_rejected(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        rt.start(100).unwrap();
        rt.stop(200).unwrap();
        let err = rt.stop(300).unwrap_err();
        assert!(matches!(err, ConnectorHostRuntimeError::InvalidTransition { .. }));
    }

    /// Starting with a failed probe results in Failed state.
    #[test]
    fn start_with_failed_probe_reaches_failed(
        config in arb_valid_config(),
        class in arb_failure_class(),
    ) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        let probe = StartupProbeResult::failed(class, "proptest_reason");
        let err = rt.start_with_probe(100, probe).unwrap_err();
        assert!(matches!(err, ConnectorHostRuntimeError::StartupProbeFailed { .. }));
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Failed);
    }

    /// Restart from any non-stopped state stops then starts.
    #[test]
    fn restart_from_running_cycles_through_stopped(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        rt.start(100).unwrap();
        rt.restart(200).unwrap();
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Running);
        // Should have stop + start transitions
        let history = rt.transition_history();
        assert!(history.iter().any(|t| t.to == ConnectorLifecyclePhase::Stopped));
    }

    /// Restart from failed state succeeds (stops implicitly then starts).
    #[test]
    fn restart_from_failed_recovers(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        let _ = rt.start_with_probe(100, StartupProbeResult::failed(
            ConnectorFailureClass::Network, "down"
        ));
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Failed);

        rt.restart(200).unwrap();
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Running);
    }
}

// ---------------------------------------------------------------------------
// Budget enforcement properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Exceeding any budget dimension forces Degraded state.
    #[test]
    fn budget_exceed_forces_degraded(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config.clone()).unwrap();
        rt.start(100).unwrap();
        rt.record_heartbeat(110).unwrap();

        let over_cpu = arb_usage_exceeding_cpu(&config.budgets);
        let err = rt.observe_usage(120, over_cpu).unwrap_err();
        assert!(matches!(err, ConnectorHostRuntimeError::BudgetExceeded { .. }));
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Degraded);
    }

    /// Within-budget observation from Degraded recovers to Running.
    #[test]
    fn within_budget_recovers_from_degraded(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config.clone()).unwrap();
        rt.start(100).unwrap();
        rt.record_heartbeat(110).unwrap();

        let over = arb_usage_exceeding_cpu(&config.budgets);
        let _ = rt.observe_usage(120, over);
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Degraded);

        let within = arb_usage_within(&config.budgets);
        rt.observe_usage(130, within).unwrap();
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Running);
    }

    /// Within-budget observation from Running stays Running.
    #[test]
    fn within_budget_stays_running(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config.clone()).unwrap();
        rt.start(100).unwrap();

        let within = arb_usage_within(&config.budgets);
        rt.observe_usage(120, within).unwrap();
        assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Running);
    }
}

// ---------------------------------------------------------------------------
// Operation envelope monotonicity
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Consecutive operation IDs are strictly increasing (lexicographic on hex suffix).
    #[test]
    fn operation_ids_strictly_increasing(
        config in arb_valid_config(),
        n in 2u32..20,
    ) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        rt.start(100).unwrap();

        let mut prev_id = String::new();
        for i in 0..n {
            let envelope = rt
                .build_operation_envelope(200 + u64::from(i), "test.action", format!("corr-{i}"))
                .unwrap();
            if !prev_id.is_empty() {
                assert!(envelope.operation_id > prev_id,
                    "op ID {} should be > {}",
                    envelope.operation_id, prev_id);
            }
            prev_id = envelope.operation_id;
        }
    }

    /// Envelope can only be built in Running state.
    #[test]
    fn envelope_requires_running_state(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        // Stopped
        let err = rt.build_operation_envelope(100, "test", "c1").unwrap_err();
        assert!(matches!(err, ConnectorHostRuntimeError::HostNotRunnable { .. }));

        // Failed
        let _ = rt.start_with_probe(200, StartupProbeResult::failed(
            ConnectorFailureClass::Auth, "no_token"
        ));
        let err = rt.build_operation_envelope(300, "test", "c2").unwrap_err();
        assert!(matches!(err, ConnectorHostRuntimeError::HostNotRunnable { .. }));
    }
}

// ---------------------------------------------------------------------------
// Heartbeat liveness invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Liveness requires heartbeat within 3x interval.
    #[test]
    fn liveness_respects_heartbeat_deadline(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config.clone()).unwrap();
        rt.start(100).unwrap();

        // Fresh heartbeat — should be live
        let snap = rt.health_snapshot(100);
        assert!(snap.is_live, "fresh start should be live");

        // Just within deadline (heartbeat at 100, check at 100 + 3*interval)
        let deadline_edge = 100 + config.heartbeat_interval_ms * 3;
        let snap = rt.health_snapshot(deadline_edge);
        assert!(snap.is_live, "should be live at exactly deadline");

        // Past deadline by 1ms
        let snap = rt.health_snapshot(deadline_edge + 1);
        assert!(!snap.is_live, "should NOT be live past deadline");
    }

    /// Readiness requires Running + live + no budget exceedance.
    #[test]
    fn readiness_requires_running_and_live_and_within_budget(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config.clone()).unwrap();
        rt.start(100).unwrap();

        // Within budget + live → ready
        let within = arb_usage_within(&config.budgets);
        rt.observe_usage(110, within).unwrap();
        let snap = rt.health_snapshot(110);
        assert!(snap.is_ready, "Running + live + within budget should be ready");

        // Exceed budget → degraded → not ready
        let over = arb_usage_exceeding_cpu(&config.budgets);
        let _ = rt.observe_usage(120, over);
        let snap = rt.health_snapshot(120);
        assert!(!snap.is_ready, "Degraded should not be ready");
    }

    /// Heartbeat recording is only allowed in Running or Degraded.
    #[test]
    fn heartbeat_only_when_running_or_degraded(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config).unwrap();
        // Stopped
        assert!(rt.record_heartbeat(100).is_err());

        rt.start(200).unwrap();
        // Running — OK
        assert!(rt.record_heartbeat(210).is_ok());

        rt.stop(300).unwrap();
        // Stopped again
        assert!(rt.record_heartbeat(310).is_err());
    }
}

// ---------------------------------------------------------------------------
// Transition history bounded
// ---------------------------------------------------------------------------

#[test]
fn transition_history_bounded_at_capacity() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();

    // Each start/stop cycle generates transitions: start→Starting, start→Running, stop→Stopped = 3
    // 64 / 3 = ~21.3, so 25 cycles should exceed capacity
    for i in 0..25u64 {
        let base = (i + 1) * 1000;
        rt.start(base).unwrap();
        rt.stop(base + 100).unwrap();
    }

    let history = rt.transition_history();
    assert!(
        history.len() <= 64,
        "history should be bounded at 64, got {}",
        history.len()
    );
}

// ---------------------------------------------------------------------------
// Serde roundtrip properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// ConnectorProtocolVersion survives JSON roundtrip.
    #[test]
    fn protocol_version_serde_roundtrip(ver in arb_protocol_version()) {
        let json = serde_json::to_string(&ver).unwrap();
        let decoded: ConnectorProtocolVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(ver, decoded);
    }

    /// ConnectorRuntimeBudgets survives JSON roundtrip.
    #[test]
    fn budgets_serde_roundtrip(budgets in arb_valid_budgets()) {
        let json = serde_json::to_string(&budgets).unwrap();
        let decoded: ConnectorRuntimeBudgets = serde_json::from_str(&json).unwrap();
        assert_eq!(budgets, decoded);
    }

    /// ConnectorFailureClass survives JSON roundtrip.
    #[test]
    fn failure_class_serde_roundtrip(class in arb_failure_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let decoded: ConnectorFailureClass = serde_json::from_str(&json).unwrap();
        assert_eq!(class, decoded);
    }

    /// Full runtime state survives JSON roundtrip after lifecycle operations.
    #[test]
    fn runtime_serde_roundtrip_after_lifecycle(config in arb_valid_config()) {
        let mut rt = ConnectorHostRuntime::new(config.clone()).unwrap();
        rt.start(100).unwrap();
        rt.record_heartbeat(110).unwrap();

        let within = arb_usage_within(&config.budgets);
        rt.observe_usage(120, within).unwrap();

        let json = serde_json::to_string(&rt).unwrap();
        let decoded: ConnectorHostRuntime = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, decoded);
    }

    /// ConnectorHostRuntimeError survives JSON roundtrip.
    #[test]
    fn error_serde_roundtrip(class in arb_failure_class()) {
        let errors = vec![
            ConnectorHostRuntimeError::InvalidConfig { reason: "test".into() },
            ConnectorHostRuntimeError::BudgetExceeded { dimension: "cpu".into() },
            ConnectorHostRuntimeError::StartupProbeFailed { class, reason_code: "reason".into() },
            ConnectorHostRuntimeError::HostNotRunnable { phase: ConnectorLifecyclePhase::Stopped },
            ConnectorHostRuntimeError::ProtocolUpgradeRejected { reason: "old".into() },
        ];
        for err in errors {
            let json = serde_json::to_string(&err).unwrap();
            let decoded: ConnectorHostRuntimeError = serde_json::from_str(&json).unwrap();
            assert_eq!(err, decoded);
        }
    }
}

// ---------------------------------------------------------------------------
// Config validation exhaustive edge cases
// ---------------------------------------------------------------------------

#[test]
fn config_rejects_empty_host_id() {
    let mut config = ConnectorHostConfig::default();
    config.host_id = String::new();
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn config_rejects_whitespace_host_id() {
    let mut config = ConnectorHostConfig::default();
    config.host_id = "  ".to_string();
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn config_rejects_zero_startup_timeout() {
    let mut config = ConnectorHostConfig::default();
    config.startup_timeout_ms = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn config_rejects_zero_heartbeat_interval() {
    let mut config = ConnectorHostConfig::default();
    config.heartbeat_interval_ms = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn config_rejects_zero_failure_backoff() {
    let mut config = ConnectorHostConfig::default();
    config.failure_backoff_ms = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn budget_rejects_zero_cpu() {
    let mut config = ConnectorHostConfig::default();
    config.budgets.cpu_millis_per_second = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn budget_rejects_zero_memory() {
    let mut config = ConnectorHostConfig::default();
    config.budgets.memory_bytes = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn budget_rejects_zero_io() {
    let mut config = ConnectorHostConfig::default();
    config.budgets.io_bytes_per_second = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

#[test]
fn budget_rejects_zero_max_inflight() {
    let mut config = ConnectorHostConfig::default();
    config.budgets.max_inflight_ops = 0;
    assert!(ConnectorHostRuntime::new(config).is_err());
}

// ---------------------------------------------------------------------------
// Protocol upgrade invariants
// ---------------------------------------------------------------------------

#[test]
fn upgrade_requires_higher_version() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();

    // Same version rejected
    let err = rt
        .upgrade_and_restart(
            200,
            ConnectorProtocolVersion::new(1, 0, 0),
            StartupProbeResult::healthy(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        ConnectorHostRuntimeError::ProtocolUpgradeRejected { .. }
    ));

    // Lower version rejected
    let err = rt
        .upgrade_and_restart(
            300,
            ConnectorProtocolVersion::new(0, 9, 0),
            StartupProbeResult::healthy(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        ConnectorHostRuntimeError::ProtocolUpgradeRejected { .. }
    ));

    // Higher version succeeds
    rt.upgrade_and_restart(
        400,
        ConnectorProtocolVersion::new(1, 1, 0),
        StartupProbeResult::healthy(),
    )
    .unwrap();
    assert_eq!(
        rt.config().protocol_version,
        ConnectorProtocolVersion::new(1, 1, 0)
    );
}

#[test]
fn upgrade_from_stopped_updates_version_without_starting() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();

    rt.upgrade_and_restart(
        100,
        ConnectorProtocolVersion::new(2, 0, 0),
        StartupProbeResult::healthy(),
    )
    .unwrap();
    assert_eq!(
        rt.config().protocol_version,
        ConnectorProtocolVersion::new(2, 0, 0)
    );
    // Host should still be stopped since it was never started
    assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Stopped);
}

// ---------------------------------------------------------------------------
// mark_failure edge cases
// ---------------------------------------------------------------------------

#[test]
fn mark_failure_transitions_to_failed() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();

    rt.mark_failure(200, ConnectorFailureClass::Network, "connection_reset")
        .unwrap();
    assert_eq!(rt.state().phase(), ConnectorLifecyclePhase::Failed);
    assert_eq!(
        rt.state().failure().unwrap().reason_code,
        "connection_reset"
    );
}

#[test]
fn mark_failure_rejects_empty_reason_code() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();

    let err = rt
        .mark_failure(200, ConnectorFailureClass::Auth, "")
        .unwrap_err();
    assert!(matches!(
        err,
        ConnectorHostRuntimeError::InvalidConfig { .. }
    ));
}

#[test]
fn mark_failure_increments_active_failures() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();

    let snap_before = rt.health_snapshot(150);
    assert_eq!(snap_before.active_failures, 0);

    rt.mark_failure(200, ConnectorFailureClass::Timeout, "deadline_exceeded")
        .unwrap();
    let snap_after = rt.health_snapshot(250);
    assert_eq!(snap_after.active_failures, 1);
}

// ---------------------------------------------------------------------------
// Usage exceeded_dimension coverage
// ---------------------------------------------------------------------------

#[test]
fn exceeded_dimension_reports_first_exceeded() {
    let budgets = ConnectorRuntimeBudgets::default();

    // Memory exceeded
    let usage = ConnectorRuntimeUsage {
        cpu_millis_in_window: 0,
        memory_bytes: budgets.memory_bytes + 1,
        io_bytes_in_window: 0,
        inflight_ops: 0,
    };
    assert_eq!(usage.exceeded_dimension(&budgets), Some("memory_bytes"));

    // IO exceeded
    let usage = ConnectorRuntimeUsage {
        cpu_millis_in_window: 0,
        memory_bytes: 0,
        io_bytes_in_window: budgets.io_bytes_per_second + 1,
        inflight_ops: 0,
    };
    assert_eq!(
        usage.exceeded_dimension(&budgets),
        Some("io_bytes_per_second")
    );

    // Max inflight ops exceeded
    let usage = ConnectorRuntimeUsage {
        cpu_millis_in_window: 0,
        memory_bytes: 0,
        io_bytes_in_window: 0,
        inflight_ops: budgets.max_inflight_ops + 1,
    };
    assert_eq!(usage.exceeded_dimension(&budgets), Some("max_inflight_ops"));

    // All within budget
    let usage = ConnectorRuntimeUsage {
        cpu_millis_in_window: 0,
        memory_bytes: 0,
        io_bytes_in_window: 0,
        inflight_ops: 0,
    };
    assert_eq!(usage.exceeded_dimension(&budgets), None);
}

// ---------------------------------------------------------------------------
// Health snapshot invariants
// ---------------------------------------------------------------------------

#[test]
fn health_snapshot_stopped_is_not_live_not_ready() {
    let rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    let snap = rt.health_snapshot(100);
    assert_eq!(snap.phase, ConnectorLifecyclePhase::Stopped);
    assert!(!snap.is_live);
    assert!(!snap.is_ready);
    assert!(snap.last_heartbeat_at_ms.is_none());
}

#[test]
fn health_snapshot_failed_is_not_live_not_ready() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    let _ = rt.start_with_probe(
        100,
        StartupProbeResult::failed(ConnectorFailureClass::Auth, "no_creds"),
    );
    let snap = rt.health_snapshot(200);
    assert_eq!(snap.phase, ConnectorLifecyclePhase::Failed);
    assert!(!snap.is_live);
    assert!(!snap.is_ready);
}

// ---------------------------------------------------------------------------
// Display/as_str coverage
// ---------------------------------------------------------------------------

#[test]
fn failure_class_display_roundtrip() {
    let classes = [
        (ConnectorFailureClass::Auth, "auth"),
        (ConnectorFailureClass::Quota, "quota"),
        (ConnectorFailureClass::Network, "network"),
        (ConnectorFailureClass::Policy, "policy"),
        (ConnectorFailureClass::Validation, "validation"),
        (ConnectorFailureClass::Timeout, "timeout"),
        (ConnectorFailureClass::Unknown, "unknown"),
    ];
    for (class, expected) in classes {
        assert_eq!(class.as_str(), expected);
        assert_eq!(class.to_string(), expected);
    }
}

#[test]
fn lifecycle_phase_display_roundtrip() {
    let phases = [
        (ConnectorLifecyclePhase::Stopped, "stopped"),
        (ConnectorLifecyclePhase::Starting, "starting"),
        (ConnectorLifecyclePhase::Running, "running"),
        (ConnectorLifecyclePhase::Degraded, "degraded"),
        (ConnectorLifecyclePhase::Failed, "failed"),
    ];
    for (phase, expected) in phases {
        assert_eq!(phase.as_str(), expected);
        assert_eq!(phase.to_string(), expected);
    }
}

#[test]
fn protocol_version_display() {
    let ver = ConnectorProtocolVersion::new(2, 3, 4);
    assert_eq!(ver.to_string(), "2.3.4");
}

#[test]
fn protocol_version_ordering() {
    let v1 = ConnectorProtocolVersion::new(1, 0, 0);
    let v1_1 = ConnectorProtocolVersion::new(1, 1, 0);
    let v2 = ConnectorProtocolVersion::new(2, 0, 0);
    assert!(v1 < v1_1);
    assert!(v1_1 < v2);
    assert!(v1 < v2);
}

// ---------------------------------------------------------------------------
// Envelope validation edge cases
// ---------------------------------------------------------------------------

#[test]
fn envelope_rejects_empty_action() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();
    let err = rt.build_operation_envelope(200, "", "corr-1").unwrap_err();
    assert!(matches!(
        err,
        ConnectorHostRuntimeError::InvalidConfig { .. }
    ));
}

#[test]
fn envelope_rejects_whitespace_action() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();
    let err = rt
        .build_operation_envelope(200, "  ", "corr-1")
        .unwrap_err();
    assert!(matches!(
        err,
        ConnectorHostRuntimeError::InvalidConfig { .. }
    ));
}

#[test]
fn envelope_rejects_empty_correlation_id() {
    let mut rt = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    rt.start(100).unwrap();
    let err = rt
        .build_operation_envelope(200, "test.action", "")
        .unwrap_err();
    assert!(matches!(
        err,
        ConnectorHostRuntimeError::InvalidConfig { .. }
    ));
}
