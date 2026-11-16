//! W4.T (ft-7h5da.5.6) — zero-RCH unit slice for dead-wire closure + governor
//! consultation. Standalone integration test, public API only, no mocks.
//!
//! Covers the LANDED, pure part of the W4.T acceptance: the connector
//! reliability controller actually DENIES a tripped circuit (the core of the
//! ft-x3211 closure — "reliability allow_operation() actually denies a tripped
//! circuit"), and a cancellation never counts as a circuit failure.
//!
//! The over-quota governor denial (acceptance #3b, `evaluate()` Reject on an
//! over-quota connector) is already covered at the dispatch level by
//! `tests/e2e_tx_connector_dispatch.rs::connector_dispatch_consults_governor_blocking_over_budget_second_effect`
//! (ft-lvglb); a standalone `ConnectorGovernor` unit needs a 6-field
//! `ConnectorGovernorConfig` (no `Default`) to isolate the quota gate from the
//! rate-limit/cost/backpressure gates, so it is not duplicated here.
//!
//! GATED remainder (blocked impl / binary / lane), tracked in the bead comment:
//! capacity-governor verdict + capacity.* reason codes + fail-closed (W4.4,
//! ft-7h5da.5.4); frankenterm-topo deadwire CI gate + wiring-status attestation
//! (W4.5, ft-7h5da.5.5 — a binary/CI lane); config dead-key honesty check (W4.6,
//! ft-7h5da.5.7); BOCPD change-point emission/dedup at INFO (W4.2 wired/closed,
//! but emission is watcher-level e2e); shadow-mode doctor divergence e2e (W4.1).

use frankenterm_core::circuit_breaker::CircuitStateKind;
use frankenterm_core::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
use frankenterm_core::connector_reliability::{
    ConnectorErrorKind, ConnectorReliabilityConfig, ConnectorReliabilityController,
};

fn sample_action() -> ConnectorAction {
    ConnectorAction {
        target_connector: "slack".to_string(),
        action_kind: ConnectorActionKind::Notify,
        correlation_id: "corr-1".to_string(),
        params: serde_json::json!({"source": "w4t"}),
        created_at_ms: 1_000,
    }
}

#[test]
fn reliability_controller_allows_while_circuit_closed() {
    let mut controller =
        ConnectorReliabilityController::new("slack", ConnectorReliabilityConfig::default());
    assert_eq!(
        controller.circuit_status().state,
        CircuitStateKind::Closed,
        "a fresh controller starts Closed"
    );
    assert!(
        controller.allow_operation(),
        "a closed circuit must admit operations"
    );
}

#[test]
fn reliability_controller_denies_after_circuit_trips_ft_x3211() {
    let mut controller =
        ConnectorReliabilityController::new("slack", ConnectorReliabilityConfig::default());
    let action = sample_action();

    // Drive well past any preset failure_threshold (5/3/10) with a
    // breaker-tripping error kind so the circuit is unambiguously Open.
    for i in 0..15 {
        let _ = controller.record_failure(
            &action,
            "connector timed out",
            ConnectorErrorKind::Timeout,
            900 + i,
        );
    }

    assert_eq!(
        controller.circuit_status().state,
        CircuitStateKind::Open,
        "repeated tripping failures must Open the circuit"
    );
    assert!(
        !controller.allow_operation(),
        "ft-x3211: a tripped (Open) circuit must DENY operations — allow_operation \
         is actually consulted, not advisory"
    );
}

#[test]
fn reliability_controller_cancellation_never_trips_the_circuit() {
    // ft-gc4hz / retry-cancel pattern: cancellation is not a reliability failure.
    let mut controller =
        ConnectorReliabilityController::new("slack", ConnectorReliabilityConfig::default());
    let action = sample_action();

    for i in 0..15 {
        let dlq_id = controller.record_failure(
            &action,
            "operation cancelled",
            ConnectorErrorKind::Cancelled,
            900 + i,
        );
        assert!(
            dlq_id.is_none(),
            "a cancelled operation must not be dead-lettered"
        );
    }

    assert_eq!(
        controller.circuit_status().state,
        CircuitStateKind::Closed,
        "cancellations must NOT trip the circuit"
    );
    assert!(
        controller.allow_operation(),
        "the circuit stays admitting after cancellations only"
    );
}

#[test]
fn reliability_controller_recovers_to_admitting_after_success_run() {
    // A closed circuit that sees only successes keeps admitting (no spurious trip
    // from the success path).
    let mut controller =
        ConnectorReliabilityController::new("slack", ConnectorReliabilityConfig::default());
    for _ in 0..20 {
        controller.record_success();
        assert!(
            controller.allow_operation(),
            "successes must never push the circuit toward denial"
        );
    }
    assert_eq!(controller.circuit_status().state, CircuitStateKind::Closed);
}
