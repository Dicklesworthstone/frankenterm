//! W5.T net-new conformance for SteeringReceipt (ft-7h5da.6.7).
//!
//! Complements the existing steering conformance files (conformance_steer_plan_
//! golden, conformance_steering_receipt_chain, steering_receipt_admission_
//! conformance) — which already cover golden round-trip, mission/tx hash-drift,
//! fail-closed bindings, tamper resistance, and basic expiry — with three
//! properties they leave unpinned:
//!   1. `steer_plan` is DETERMINISTIC per input (same inputs => byte-identical
//!      receipt), the precondition for any golden/replay claim.
//!   2. the content-addressed `receipt_id` actually BINDS the plan inputs
//!      (different objective/workspace => different id), so the address is real,
//!      not nominal — a forged-by-input-swap receipt cannot reuse an id.
//!   3. the TTL expiry boundary is EXACT (no off-by-one replay): valid at the
//!      last ms before TTL, refused exactly at the TTL boundary.
//!
//! Zero-RCH, public API only, no source edits. The execute-side W5.T items
//! (per-commit envelope revalidation during a live tx, supervisor
//! slow-vs-stuck, e2e mission drive) ride .6.4/.6.5/.6.6 (blocked/deferred as
//! security-sensitive) and are out of scope for this plan-side file.

use frankenterm_core::steer_plan::{steer_plan, SteerPlanScenario};
use frankenterm_core::steer_run::receipt_admits_action;
use frankenterm_core::steering::SteeringReceipt;

const GEN_MS: u64 = 1_700_000_000_000;
const CREATED_MS: i64 = 1_700_000_000_000;

/// (1) DETERMINISM: identical inputs must yield a byte-identical receipt for
/// every scenario — otherwise the golden matrices and any receipt-as-replay
/// guarantee are meaningless.
#[test]
fn steer_plan_is_deterministic_per_input() {
    for scenario in SteerPlanScenario::ALL {
        let a = steer_plan(scenario, "ship the W5 family", "ws-det", GEN_MS, CREATED_MS, None);
        let b = steer_plan(scenario, "ship the W5 family", "ws-det", GEN_MS, CREATED_MS, None);

        assert_eq!(
            a.receipt.receipt_id,
            b.receipt.receipt_id,
            "{} receipt_id is non-deterministic",
            scenario.as_str()
        );
        assert_eq!(
            serde_json::to_value(&a.receipt).expect("serialize a"),
            serde_json::to_value(&b.receipt).expect("serialize b"),
            "{} receipt is non-deterministic across identical runs",
            scenario.as_str()
        );
        assert!(
            a.receipt.validate().is_ok(),
            "{} receipt must validate",
            scenario.as_str()
        );
    }
}

/// (2) CONTENT-ADDRESS BINDS INPUTS: for a fixed scenario, the same inputs map
/// to the same id, but a different objective or workspace maps to a different
/// id. This proves `receipt_id` is a real content address over the plan inputs
/// — so you cannot swap the objective/workspace under a stable id.
#[test]
fn receipt_id_binds_objective_and_workspace_inputs() {
    let scenario = SteerPlanScenario::ALL[0];
    let id = |objective: &str, ws: &str| {
        steer_plan(scenario, objective, ws, GEN_MS, CREATED_MS, None)
            .receipt
            .receipt_id
    };

    let base = id("objective-A", "ws-1");
    assert_eq!(base, id("objective-A", "ws-1"), "same inputs => same id");
    assert_ne!(
        base,
        id("objective-B", "ws-1"),
        "a different objective must change the content-addressed id (objective is bound)"
    );
    assert_ne!(
        base,
        id("objective-A", "ws-2"),
        "a different workspace must change the content-addressed id (workspace is bound)"
    );
}

/// (3) TTL boundary is exact — no off-by-one replay. `is_expired` is
/// `now - created >= ttl`, so a receipt is valid through `created + ttl - 1` and
/// expired at exactly `created + ttl`. The existing expiry test only checks a
/// mid-window valid point and a far-past-TTL expired point; this pins the edge.
#[test]
fn receipt_ttl_expiry_boundary_is_exact_no_off_by_one_replay() {
    const TTL: i64 = 1_000;
    let receipt = SteeringReceipt::new(
        "objective",
        "ws",
        None, // no mission binding
        Some("plan-hash".to_string()),
        "envelope.admit",
        Some(900),
        Vec::new(),
        CREATED_MS,
        Some(TTL),
    );
    assert!(receipt.validate().is_ok());

    assert!(
        receipt_admits_action(&receipt, "plan-hash", CREATED_MS).is_valid(),
        "must admit at creation time"
    );
    assert!(
        receipt_admits_action(&receipt, "plan-hash", CREATED_MS + TTL - 1).is_valid(),
        "must admit at the last ms before TTL"
    );
    assert!(
        !receipt_admits_action(&receipt, "plan-hash", CREATED_MS + TTL).is_valid(),
        "must be expired exactly at the TTL boundary (no off-by-one replay)"
    );
    assert!(
        !receipt_admits_action(&receipt, "plan-hash", CREATED_MS + TTL + 10_000).is_valid(),
        "must remain expired past the TTL boundary"
    );
}

/// A receipt with no TTL never expires — admission is then governed purely by
/// hash/verdict, not by a clock the caller can outlast.
#[test]
fn receipt_without_ttl_never_expires() {
    let receipt = SteeringReceipt::new(
        "objective",
        "ws",
        None,
        Some("plan-hash".to_string()),
        "envelope.admit",
        Some(900),
        Vec::new(),
        CREATED_MS,
        None,
    );
    assert!(
        receipt_admits_action(&receipt, "plan-hash", CREATED_MS + 10_000_000_000).is_valid(),
        "a no-TTL receipt must still admit arbitrarily far in the future"
    );
}
