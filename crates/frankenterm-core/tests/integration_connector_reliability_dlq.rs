//! Integration test: connector reliability controller → DLQ → replay.
//!
//! Exercises the connector failure-recovery pipeline:
//!
//!   ConnectorReliabilityController.allow_operation()
//!     → record_failure(action, error, kind, timestamp)
//!       → DeadLetterQueue.enqueue() (auto-DLQ on retryable errors)
//!         → build_replay_plan(now, batch_size, stop_on_failure)
//!           → DLQ.record_retry() / DLQ.remove() / DLQ.purge_expired()
//!
//! This mirrors the real connector dispatch loop: the controller gates
//! operations via its circuit breaker, auto-enqueues failed actions to
//! the DLQ, and replay plans batch retries with backoff.

use frankenterm_core::circuit_breaker::CircuitStateKind;
use frankenterm_core::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
use frankenterm_core::connector_reliability::{
    ConnectorCircuitConfig, ConnectorErrorKind, ConnectorReliabilityConfig,
    ConnectorReliabilityController, DeadLetterQueueConfig,
};

// ── Helpers ─────────────────────────────────────────────────────────────

fn make_action(kind: ConnectorActionKind, correlation: &str) -> ConnectorAction {
    ConnectorAction {
        target_connector: "test-slack".to_string(),
        action_kind: kind,
        correlation_id: correlation.to_string(),
        params: serde_json::json!({"channel": "#alerts"}),
        created_at_ms: 1000,
    }
}

fn fast_circuit_config() -> ConnectorReliabilityConfig {
    ConnectorReliabilityConfig {
        circuit: ConnectorCircuitConfig {
            failure_threshold: 3,
            success_threshold: 1,
            cooldown: std::time::Duration::from_millis(10),
        },
        dlq: DeadLetterQueueConfig {
            max_entries: 100,
            max_age_ms: 60_000,
            max_retries: 3,
        },
        auto_dlq: true,
        shed_threshold: 50,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Transient failures trip the circuit breaker and auto-enqueue to DLQ,
/// while permanent failures don't trip the breaker.
#[test]
fn error_kind_drives_breaker_and_dlq_behavior() {
    let mut ctrl = ConnectorReliabilityController::new("slack-notifier", fast_circuit_config());
    assert!(ctrl.allow_operation());
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Closed);

    // Transient errors trip the breaker.
    assert!(ConnectorErrorKind::Transient.trips_breaker());
    assert!(ConnectorErrorKind::Transient.is_retryable());

    // Permanent errors don't trip and aren't retryable.
    assert!(!ConnectorErrorKind::Permanent.trips_breaker());
    assert!(!ConnectorErrorKind::Permanent.is_retryable());

    // RateLimited is retryable but doesn't trip.
    assert!(!ConnectorErrorKind::RateLimited.trips_breaker());
    assert!(ConnectorErrorKind::RateLimited.is_retryable());

    // Record transient failures — should auto-enqueue to DLQ.
    let action1 = make_action(ConnectorActionKind::Notify, "evt-1");
    let dlq_id = ctrl.record_failure(
        &action1,
        "connection refused",
        ConnectorErrorKind::Transient,
        1000,
    );
    assert!(
        dlq_id.is_some(),
        "auto_dlq should enqueue transient failures"
    );
    assert_eq!(ctrl.dlq().depth(), 1);

    // Record permanent failure — should NOT enqueue (not retryable).
    let action2 = make_action(ConnectorActionKind::Notify, "evt-2");
    let dlq_id = ctrl.record_failure(
        &action2,
        "invalid payload",
        ConnectorErrorKind::Permanent,
        2000,
    );
    assert!(dlq_id.is_none(), "permanent errors should not be enqueued");
    assert_eq!(ctrl.dlq().depth(), 1);

    // Record more transient failures to trip circuit.
    let action3 = make_action(ConnectorActionKind::Notify, "evt-3");
    ctrl.record_failure(&action3, "timeout", ConnectorErrorKind::Transient, 3000);
    let action4 = make_action(ConnectorActionKind::Notify, "evt-4");
    ctrl.record_failure(&action4, "timeout", ConnectorErrorKind::Timeout, 4000);

    // Circuit should now be open (3 breaker-tripping failures).
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Open);
    assert!(!ctrl.allow_operation());

    // DLQ should have 3 entries (transient + transient + timeout).
    assert_eq!(ctrl.dlq().depth(), 3);
}

/// DLQ replay plan batches entries for retry, and successful replays
/// remove entries while failed ones stay for future attempts.
#[test]
fn dlq_replay_plan_and_retry_lifecycle() {
    let mut ctrl = ConnectorReliabilityController::new("jira-tickets", fast_circuit_config());

    // Enqueue several failed actions.
    for i in 0..5u64 {
        let action = make_action(ConnectorActionKind::Ticket, &format!("evt-{i}"));
        ctrl.record_failure(
            &action,
            "service unavailable",
            ConnectorErrorKind::ServiceUnavailable,
            1000 + i * 100,
        );
    }
    assert_eq!(ctrl.dlq().depth(), 5);

    // Circuit tripped after 3 breaker-tripping failures.
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Open);

    // Wait for cooldown, then recover circuit.
    std::thread::sleep(std::time::Duration::from_millis(15));
    assert!(ctrl.allow_operation()); // → HalfOpen
    ctrl.record_success(); // → Closed
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Closed);

    // Build replay plan: batch of 3, stop on failure.
    let plan = ctrl.build_replay_plan(5000, 3, true);
    assert_eq!(plan.entry_ids.len(), 3);
    assert_eq!(plan.batch_size, 3);
    assert!(plan.stop_on_failure);

    // Simulate replay: first 2 succeed, 3rd fails.
    let dlq = ctrl.dlq_mut();
    dlq.remove(plan.entry_ids[0]); // success
    dlq.remove(plan.entry_ids[1]); // success
    dlq.record_retry(
        plan.entry_ids[2],
        "still unavailable",
        ConnectorErrorKind::Transient,
        5500,
    );

    // 2 removed + 3 remaining = 3 in DLQ.
    assert_eq!(ctrl.dlq().depth(), 3);

    // The retried entry has incremented attempt count.
    let entries = ctrl.dlq().pending_entries();
    let retried = entries.iter().find(|e| e.id == plan.entry_ids[2]);
    assert!(retried.is_some());
    assert!(retried.unwrap().attempt_count >= 2);
}

/// Telemetry snapshots are coherent after a mixed success/failure/replay
/// lifecycle across the controller and DLQ.
#[test]
fn telemetry_coherent_after_mixed_lifecycle() {
    let mut ctrl = ConnectorReliabilityController::new("audit-sink", fast_circuit_config());

    // 3 successful operations.
    for _ in 0..3 {
        assert!(ctrl.allow_operation());
        ctrl.record_success();
    }

    // 2 transient failures.
    for i in 0..2u64 {
        let action = make_action(ConnectorActionKind::AuditLog, &format!("audit-{i}"));
        ctrl.record_failure(
            &action,
            "network error",
            ConnectorErrorKind::Transient,
            1000 + i * 100,
        );
    }

    // Check controller telemetry.
    let snap = ctrl.telemetry_snapshot();
    assert_eq!(snap.connector_id, "audit-sink");
    assert_eq!(snap.operations_succeeded, 3);
    assert_eq!(snap.operations_failed, 2);

    // Check DLQ telemetry.
    assert_eq!(snap.dlq.total_enqueued, 2);
    assert_eq!(snap.dlq.current_depth, 2);
    assert_eq!(snap.dlq.replayed_ok, 0);

    // Replay one entry successfully.
    let entries = ctrl.dlq().pending_entries();
    let first_id = entries[0].id;
    ctrl.dlq_mut().remove(first_id);

    let snap2 = ctrl.telemetry_snapshot();
    assert_eq!(snap2.dlq.current_depth, 1);

    // Purge expired entries (none should expire yet — max_age is 60s).
    let purged = ctrl.dlq_mut().purge_expired(2000);
    assert_eq!(purged, 0);

    // Fast-forward past max_age and purge.
    let purged = ctrl.dlq_mut().purge_expired(62_000);
    assert_eq!(purged, 1);
    assert_eq!(ctrl.dlq().depth(), 0);
}

/// Circuit breaker presets (critical vs lenient) produce different
/// failure tolerance and DLQ behavior under the same error pattern.
#[test]
fn circuit_presets_produce_different_tolerance() {
    // Critical: trips after 3 failures.
    let critical_config = ConnectorReliabilityConfig {
        circuit: ConnectorCircuitConfig::critical(),
        auto_dlq: true,
        ..ConnectorReliabilityConfig::default()
    };
    let mut critical = ConnectorReliabilityController::new("critical-conn", critical_config);

    // Lenient: trips after 10 failures.
    let lenient_config = ConnectorReliabilityConfig {
        circuit: ConnectorCircuitConfig::lenient(),
        auto_dlq: true,
        ..ConnectorReliabilityConfig::default()
    };
    let mut lenient = ConnectorReliabilityController::new("lenient-conn", lenient_config);

    // Feed identical failure pattern to both.
    for i in 0..5u64 {
        let action = make_action(ConnectorActionKind::Invoke, &format!("op-{i}"));
        critical.record_failure(&action, "timeout", ConnectorErrorKind::Timeout, i * 100);
        lenient.record_failure(&action, "timeout", ConnectorErrorKind::Timeout, i * 100);
    }

    // Critical should be open after 3 failures.
    assert_eq!(critical.circuit_status().state, CircuitStateKind::Open);

    // Lenient should still be closed (threshold is 10).
    assert_eq!(lenient.circuit_status().state, CircuitStateKind::Closed);
    assert!(lenient.allow_operation());

    // Both should have 5 DLQ entries (all were Timeout = retryable).
    assert_eq!(critical.dlq().depth(), 5);
    assert_eq!(lenient.dlq().depth(), 5);

    // Critical rejects new operations; lenient allows them.
    assert!(!critical.allow_operation());
    assert!(lenient.allow_operation());
}

/// Full pipeline: operations succeed → failures escalate → circuit opens
/// → DLQ accumulates → circuit recovers → replay plan drains DLQ.
#[test]
fn full_pipeline_operate_fail_recover_replay() {
    let mut ctrl = ConnectorReliabilityController::new("webhook-dispatch", fast_circuit_config());

    // Phase 1: successful operations.
    for _ in 0..5 {
        assert!(ctrl.allow_operation());
        ctrl.record_success();
    }
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Closed);
    assert_eq!(ctrl.circuit_status().consecutive_failures, 0);

    // Phase 2: failures escalate.
    for i in 0..3u64 {
        let action = make_action(ConnectorActionKind::TriggerWorkflow, &format!("wf-{i}"));
        ctrl.record_failure(
            &action,
            "upstream 503",
            ConnectorErrorKind::ServiceUnavailable,
            5000 + i * 100,
        );
    }
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Open);
    assert_eq!(ctrl.dlq().depth(), 3);

    // Phase 3: circuit blocks new operations.
    assert!(!ctrl.allow_operation());
    let snap = ctrl.telemetry_snapshot();
    assert!(snap.circuit_rejections >= 1);

    // Phase 4: cooldown expires → half-open → probe succeeds → closed.
    std::thread::sleep(std::time::Duration::from_millis(15));
    assert!(ctrl.allow_operation()); // → HalfOpen
    ctrl.record_success(); // → Closed
    assert_eq!(ctrl.circuit_status().state, CircuitStateKind::Closed);

    // Phase 5: replay DLQ entries.
    let plan = ctrl.build_replay_plan(10_000, 10, false);
    assert_eq!(plan.entry_ids.len(), 3);

    // Simulate all replays succeeding.
    for &id in &plan.entry_ids {
        ctrl.dlq_mut().remove(id);
    }
    assert_eq!(ctrl.dlq().depth(), 0);

    // Final telemetry.
    let final_snap = ctrl.telemetry_snapshot();
    assert_eq!(final_snap.operations_succeeded, 6); // 5 initial + 1 probe
    assert_eq!(final_snap.operations_failed, 3);
    assert_eq!(final_snap.dlq.total_enqueued, 3);
    assert_eq!(final_snap.dlq.current_depth, 0);
}
