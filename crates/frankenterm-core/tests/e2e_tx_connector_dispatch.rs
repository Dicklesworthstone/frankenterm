//! Perfect-E2E: transactional + connector dispatch idempotency (ft-lvglb).
//!
//! Locks in three end-to-end safety properties of the dispatch path that the
//! reality-check sweep flagged as easy to silently regress:
//!
//! 1. **ft-iz1ki durable, restart-safe idempotency.** A committed tx step
//!    recorded in a *durable* [`IdempotencyStore`] (opened against a workspace
//!    `.ft` dir) is still recognized as already-executed after the store is
//!    dropped and re-opened — i.e. a crash/restart does NOT re-dispatch a
//!    committed side effect. This is the exact gap ft-iz1ki closed
//!    (`IdempotencyStore::open` persists each ledger to
//!    `<ft_dir>/tx_ledgers/<execution_id>.json` and reloads it on reopen), so
//!    this test fails closed if the durability sink is ever removed or the
//!    reload regresses back to in-memory-only behavior.
//!
//! 2. **Key-scoped dedup (no over-suppression).** A *different* idempotency key
//!    is NOT deduped after restart — restart-safety must not turn into a blanket
//!    "skip everything" that would drop legitimate new work.
//!
//! 3. **Connector governor consulted + no double-effect.** The connector
//!    outbound bridge consults the connector governor on dispatch (a tight quota
//!    blocks an over-budget second action) and deduplicates a replayed event
//!    (same correlation id), so a repeated/over-budget dispatch never produces a
//!    second external effect.
//!
//! Zero-RCH: authored against the real public APIs (no mocks). These are pure
//! in-process + tempdir tests — no network, no remote workers.

use frankenterm_core::tx_idempotency::{
    IdempotencyKey, IdempotencyPolicy, IdempotencyStore, StepOutcome, TxPhase,
};
use frankenterm_core::tx_plan_compiler::{StepRisk, TxPlan, TxRiskSummary};

use frankenterm_core::config::SafetyConfig;
use frankenterm_core::connector_governor::QuotaConfig;
use frankenterm_core::connector_outbound_bridge::{
    ConnectorActionKind, ConnectorOutboundBridge, ConnectorOutboundBridgeConfig, OutboundEvent,
    OutboundEventSource, OutboundRoutingRule,
};
use frankenterm_core::policy::PolicyEngine;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Minimal valid compiled plan for ledger creation. The plan body is irrelevant
/// to dedup (which matches on the idempotency *key*); the store only needs a
/// ledger to hang the recorded step on.
fn minimal_plan(plan_id: &str) -> TxPlan {
    TxPlan {
        plan_id: plan_id.to_string(),
        plan_hash: 0,
        steps: Vec::new(),
        execution_order: Vec::new(),
        parallel_levels: Vec::new(),
        risk_summary: TxRiskSummary {
            total_steps: 0,
            high_risk_count: 0,
            critical_risk_count: 0,
            uncompensated_steps: 0,
            overall_risk: StepRisk::Low,
        },
        rejected_edges: Vec::new(),
        rejected_assignments: Vec::new(),
    }
}

fn notify_rule(rule_id: &str, connector: &str) -> OutboundRoutingRule {
    OutboundRoutingRule {
        rule_id: rule_id.to_string(),
        source_filter: None,
        event_type_prefix: None,
        min_severity: None,
        target_connector: connector.to_string(),
        action_kind: ConnectorActionKind::Notify,
        enabled: true,
        priority: 0,
    }
}

// ── 1. ft-iz1ki durable, restart-safe idempotency ────────────────────────────

#[test]
fn tx_durable_idempotency_survives_store_restart_no_double_effect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ft_dir = tmp.path();

    const EXEC_ID: &str = "txe-1000";
    let plan_id = "plan-e2e";
    let key = IdempotencyKey::new(plan_id, "step-0", "commit");

    // --- Run 1: open a DURABLE store, commit a step, observe in-process dedup.
    {
        let mut store = IdempotencyStore::open(ft_dir, IdempotencyPolicy::default())
            .expect("open durable idempotency store");
        store
            .create_ledger(EXEC_ID, &minimal_plan(plan_id))
            .expect("create ledger");
        store
            .transition_phase(EXEC_ID, TxPhase::Preparing)
            .expect("enter prepare phase");
        store
            .transition_phase(EXEC_ID, TxPhase::Committing)
            .expect("enter commit phase");
        let mut reservation = store
            .acquire_durable_reservation(EXEC_ID, &key, 999)
            .expect("acquire durable reservation");
        assert!(reservation.observed_outcome().is_none());
        store
            .record_execution_reserved(
                &mut reservation,
                EXEC_ID,
                key.clone(),
                StepOutcome::Pending,
                StepRisk::High,
                "agent-e2e",
                999,
            )
            .expect("persist pending step before dispatch");
        store
            .complete_execution_reserved(
                reservation,
                EXEC_ID,
                key.clone(),
                StepOutcome::Success {
                    result: Some("committed".to_string()),
                },
                1000,
            )
            .expect("persist committed step");
        store
            .transition_phase(EXEC_ID, TxPhase::Completed)
            .expect("complete transaction");

        assert!(
            store.peek_cached_outcome(&key, 1000).is_some(),
            "committed step must dedup within the same process"
        );
    } // store dropped — simulates process exit / crash after the durable commit.

    // The durable spool must actually hold the committed ledger on disk.
    let ledger_path = ft_dir.join("tx_ledgers").join(format!("{EXEC_ID}.json"));
    assert!(
        ledger_path.exists(),
        "ft-iz1ki: the committed ledger must be persisted to {}",
        ledger_path.display()
    );

    // --- Run 2 (RESTART): a brand-new store opened against the SAME dir must
    // reload the persisted ledger and still recognize the committed step, so a
    // re-run would skip it instead of re-dispatching the side effect.
    {
        let mut restarted = IdempotencyStore::open(ft_dir, IdempotencyPolicy::default())
            .expect("reopen durable idempotency store after restart");
        restarted
            .create_ledger("txe-1001", &minimal_plan(plan_id))
            .expect("create retry ledger");
        let reservation = restarted
            .acquire_durable_reservation("txe-1001", &key, 1001)
            .expect("refresh authoritative proof under key lock");
        let recovered = reservation.observed_outcome();
        assert!(
            recovered.is_some(),
            "ft-iz1ki RESTART-SAFETY: a committed step must still dedup after the \
             store is dropped and re-opened — otherwise a crash re-dispatches it"
        );
        assert!(
            matches!(
                recovered,
                Some(StepOutcome::Success { result }) if result.as_deref() == Some("committed")
            ),
            "the recovered outcome must be the original commit receipt, got {recovered:?}"
        );
    }
}

#[test]
fn tx_durable_idempotency_is_key_scoped_not_blanket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ft_dir = tmp.path();

    let plan_id = "plan-scope";
    let committed_key = IdempotencyKey::new(plan_id, "step-0", "commit");
    let fresh_key = IdempotencyKey::new(plan_id, "step-1", "commit");

    {
        let mut store = IdempotencyStore::open(ft_dir, IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger("txe-2000", &minimal_plan(plan_id))
            .expect("create ledger");
        store
            .transition_phase("txe-2000", TxPhase::Preparing)
            .expect("enter prepare phase");
        store
            .transition_phase("txe-2000", TxPhase::Committing)
            .expect("enter commit phase");
        let mut reservation = store
            .acquire_durable_reservation("txe-2000", &committed_key, 1999)
            .expect("acquire durable reservation");
        assert!(reservation.observed_outcome().is_none());
        store
            .record_execution_reserved(
                &mut reservation,
                "txe-2000",
                committed_key.clone(),
                StepOutcome::Pending,
                StepRisk::Medium,
                "agent-e2e",
                1999,
            )
            .expect("persist pending step before dispatch");
        store
            .complete_execution_reserved(
                reservation,
                "txe-2000",
                committed_key.clone(),
                StepOutcome::Success { result: None },
                2000,
            )
            .expect("persist committed step");
        store
            .transition_phase("txe-2000", TxPhase::Completed)
            .expect("complete transaction");
    }

    // After restart: the committed key dedups, but an unrelated key must NOT —
    // restart-safety must never become a blanket suppressor of new work.
    let mut restarted =
        IdempotencyStore::open(ft_dir, IdempotencyPolicy::default()).expect("reopen durable store");
    restarted
        .create_ledger("txe-2001", &minimal_plan(plan_id))
        .expect("create retry ledger");
    let committed = restarted
        .acquire_durable_reservation("txe-2001", &committed_key, 2001)
        .expect("refresh committed key under lock");
    assert!(
        committed.observed_outcome().is_some(),
        "the previously-committed step must dedup after restart"
    );
    drop(committed);
    let fresh = restarted
        .acquire_durable_reservation("txe-2001", &fresh_key, 2001)
        .expect("refresh fresh key under lock");
    assert!(
        fresh.observed_outcome().is_none(),
        "an uncommitted step must NOT be deduped — dedup is key-scoped, not blanket"
    );
}

// ── 3. connector governor consulted + no double-effect ───────────────────────

#[test]
fn connector_dispatch_consults_governor_blocking_over_budget_second_effect() {
    // A tight quota (1 action/window) means the governor must reject the second
    // over-budget action — proving the governor is consulted on the dispatch
    // path and that a redundant dispatch cannot produce a second external
    // effect.
    let mut safety = SafetyConfig::default();
    safety.connector_governor.default_quota = QuotaConfig {
        max_actions: 1,
        window_ms: 60_000,
        warning_threshold: 0.8,
    };

    let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig {
        enforce_sandbox: false,
        ..Default::default()
    });
    bridge.set_policy_engine(PolicyEngine::from_safety_config(&safety));
    bridge.set_connector_admission_enforced("slack", true);
    bridge.add_rule(notify_rule("r1", "slack"));

    // Two DISTINCT events (no explicit correlation id → distinct auto ids → not
    // deduped) both target "slack" with action quota = 1.
    let first = OutboundEvent::new(
        OutboundEventSource::Custom,
        "alert",
        serde_json::json!({"n": 1}),
    )
    .with_timestamp_ms(1000);
    let second = OutboundEvent::new(
        OutboundEventSource::Custom,
        "alert",
        serde_json::json!({"n": 2}),
    )
    .with_timestamp_ms(2000);

    bridge.process_event(&first).expect("process first event");
    bridge.process_event(&second).expect("process second event");

    let tel = bridge.telemetry();
    assert!(
        tel.actions_blocked_governor >= 1,
        "the connector governor must be consulted and block the over-budget \
         second action (actions_blocked_governor={})",
        tel.actions_blocked_governor
    );
    assert!(
        bridge.pending_action_count() <= 1,
        "no double-effect: at most one action may reach the dispatch queue under \
         a 1-action quota (pending={})",
        bridge.pending_action_count()
    );
}

#[test]
fn connector_dispatch_deduplicates_replayed_event_no_double_effect() {
    // A replayed event (identical explicit correlation id) must be deduplicated,
    // so a retry/replay never enqueues the action twice.
    let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig {
        enforce_sandbox: false,
        ..Default::default()
    });
    bridge.add_rule(notify_rule("r1", "slack"));

    let event = OutboundEvent::new(
        OutboundEventSource::Custom,
        "alert",
        serde_json::json!({"n": 1}),
    )
    .with_timestamp_ms(1000)
    .with_correlation_id("replay-key-1");

    let first = bridge.process_event(&event).expect("process first");
    assert!(
        !first.deduplicated,
        "first delivery must not be deduplicated"
    );

    let replay = bridge.process_event(&event).expect("process replay");
    assert!(
        replay.deduplicated,
        "a replayed event (same correlation id) must be deduplicated"
    );
    assert!(
        replay.actions_dispatched.is_empty(),
        "the replay must dispatch no new actions"
    );
    assert!(
        bridge.pending_action_count() <= 1,
        "no double-effect: the action must be enqueued at most once across the \
         original + replay (pending={})",
        bridge.pending_action_count()
    );
}

// ── ft-pmbe1 harm model: durable ledger honors a committed step blindly ───────
// Standalone, public-API counterpart to the (RCH-lane-blocked) ft-pmbe1 behavioral
// e2e. The durable idempotency ledger records a committed step with NO provenance
// or trust marker, and check_dedup keys on (plan, step, action) — NOT the
// execution id. So a step committed by a BYPASSED run (e.g. the synthetic
// allow-all executor that `ft (robot) tx run` used to fall back to when the real
// runtime was unavailable) is, after restart, indistinguishable from a real gated
// commit and SUPPRESSES re-execution of that step by a later legitimate run
// ("ledger poisoning"). This locks in WHY ft-pmbe1's fix (refuse the synthetic
// commit) matters: once a forged commit reaches the ledger, dedup honors it.
#[test]
fn tx_committed_step_is_honored_across_runs_regardless_of_provenance() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ft_dir = tmp.path();
    let plan_id = "plan-poison";
    let key = IdempotencyKey::new(plan_id, "step-0", "commit");

    // A bypassed/synthetic run commits the step (distinct execution id + a marker
    // result that screams "synthetic").
    {
        let mut store = IdempotencyStore::open(ft_dir, IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger("txe-synthetic-1000", &minimal_plan(plan_id))
            .expect("create ledger");
        store
            .transition_phase("txe-synthetic-1000", TxPhase::Preparing)
            .expect("enter prepare phase");
        store
            .transition_phase("txe-synthetic-1000", TxPhase::Committing)
            .expect("enter commit phase");
        let mut reservation = store
            .acquire_durable_reservation("txe-synthetic-1000", &key, 999)
            .expect("acquire durable reservation");
        assert!(reservation.observed_outcome().is_none());
        store
            .record_execution_reserved(
                &mut reservation,
                "txe-synthetic-1000",
                key.clone(),
                StepOutcome::Pending,
                StepRisk::High,
                "synthetic-fallback",
                999,
            )
            .expect("persist pending step before dispatch");
        store
            .complete_execution_reserved(
                reservation,
                "txe-synthetic-1000",
                key.clone(),
                StepOutcome::Success {
                    result: Some("committed-by-bypassed-run".to_string()),
                },
                1000,
            )
            .expect("persist committed step");
        store
            .transition_phase("txe-synthetic-1000", TxPhase::Completed)
            .expect("complete transaction");
    }

    // RESTART: a later legitimate run opens the same durable store under a fresh
    // execution id. The key-locked live-spool refresh finds the prior commit —
    // with no way to tell it was produced by a bypassed run.
    let mut later =
        IdempotencyStore::open(ft_dir, IdempotencyPolicy::default()).expect("reopen durable store");
    later
        .create_ledger("txe-legitimate-2000", &minimal_plan(plan_id))
        .expect("create later execution ledger");
    let reservation = later
        .acquire_durable_reservation("txe-legitimate-2000", &key, 2000)
        .expect("refresh authoritative prior proof");
    let recovered = reservation.observed_outcome();
    assert!(
        matches!(
            recovered,
            Some(StepOutcome::Success { result }) if result.as_deref() == Some("committed-by-bypassed-run")
        ),
        "ft-pmbe1: a committed step survives restart and is honored verbatim by any \
         later run regardless of which execution recorded it — the ledger has no \
         provenance check, so a bypassed commit poisons later runs; got {recovered:?}"
    );
}
