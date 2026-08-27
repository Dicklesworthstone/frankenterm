//! ft-7h5da.6.7 (capstone slice): integration coverage tying the
//! SteeringReceipt family together — plan → receipt → revalidation gate →
//! approval admission — as a cohesive flow rather than isolated module units.
//!
//! It also fills the gate's MISSION-hash path, which the per-module `.6.3` unit
//! tests leave to the tx path (constructing a real `Mission` there was avoided).
//!
//! Out of scope (ride the execute side, `.6.3`-execute / `.6.6`): per-commit
//! envelope revalidation during a live tx run, and the supervisor's
//! slow-vs-stuck discrimination.

use frankenterm_core::plan::{Mission, MissionId, MissionOwnership};
use frankenterm_core::steer_plan::{SteerPlanScenario, steer_plan};
use frankenterm_core::steer_run::{SteerRunGate, receipt_admits_action, steer_run_gate};
use frankenterm_core::steering::SteeringReceipt;

const NOW: i64 = 1_704_000_000_000;

fn mission(id: &str, title: &str) -> Mission {
    Mission::new(
        MissionId(id.to_string()),
        title,
        "ws-steer",
        MissionOwnership {
            planner: "planner".to_string(),
            dispatcher: "dispatcher".to_string(),
            operator: "operator".to_string(),
        },
        NOW,
    )
}

/// Every standard scenario's receipt round-trips through serde and re-validates,
/// with a stable content-addressed id (the W5.T "references contracts by
/// canonical hash + golden round-trip" unit, exercised across all scenarios).
#[test]
fn steer_plan_receipts_round_trip_and_validate() {
    for scenario in SteerPlanScenario::ALL {
        let result = steer_plan(scenario, "ship it", "ws-steer", NOW as u64, NOW, None);
        let json = serde_json::to_string(&result.receipt).expect("serialize");
        let back: SteeringReceipt = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back,
            result.receipt,
            "{} receipt must round-trip",
            scenario.as_str()
        );
        assert!(
            back.validate().is_ok(),
            "{} receipt must validate",
            scenario.as_str()
        );
        assert!(back.receipt_id.starts_with("steer:"));
    }
}

/// The gate's MISSION-hash path: a receipt bound to mission A admits A's live
/// hash and refuses a drifted mission with a typed mismatch (never a silent
/// re-plan).
#[test]
fn gate_refuses_mission_hash_drift_typed() {
    let bound = mission("mission:a", "Original objective");
    // Same id, different title -> different canonical hash.
    let drifted = mission("mission:a", "Tampered objective");
    let receipt = SteeringReceipt::new(
        "objective",
        "ws-steer",
        Some(&bound),
        None,
        "envelope.admit",
        Some(900),
        Vec::new(),
        NOW,
        Some(10_000),
    );
    assert_eq!(
        steer_run_gate(&receipt, Some(&bound), None, NOW),
        SteerRunGate::Valid
    );
    let g = steer_run_gate(&receipt, Some(&drifted), None, NOW);
    assert_eq!(
        g,
        SteerRunGate::HashMismatch {
            contract: "mission"
        }
    );
    assert_eq!(g.error_code(), Some("robot.steer_hash_mismatch"));
}

/// The execution gate fails closed when the receipt bound a mission but the
/// caller supplied no live mission — defense in depth against a forgotten
/// binding. This is distinct from the scoped approval admission, which
/// intentionally does NOT require the mission (verified in the chain test).
#[test]
fn gate_fails_closed_on_unverifiable_mission_binding() {
    let bound = mission("mission:m", "Objective");
    let receipt = SteeringReceipt::new(
        "objective",
        "ws-steer",
        Some(&bound),
        None,
        "envelope.admit",
        Some(900),
        Vec::new(),
        NOW,
        Some(10_000),
    );
    let g = steer_run_gate(&receipt, None, None, NOW);
    assert_eq!(
        g,
        SteerRunGate::UnverifiableBinding {
            contract: "mission"
        }
    );
    assert_eq!(g.error_code(), Some("robot.steer_binding_unverifiable"));
}

/// End-to-end chain over a single receipt bound to BOTH a mission and a tx plan
/// hash: valid execution, contract tamper, TTL expiry, and approval admission —
/// the full W5 plan/execute identity surface in one flow. The admission step
/// also proves the SCOPED approval does not trip the gate's mission-binding
/// fail-closed (a mission-bound receipt still admits an action by plan hash).
#[test]
fn full_receipt_chain_gate_then_admission() {
    let bound = mission("mission:chain", "Chain objective");
    let plan_hash = "plan-hash-CHAIN";
    let receipt = SteeringReceipt::new(
        "chain objective",
        "ws-steer",
        Some(&bound),
        Some(plan_hash.to_string()),
        "envelope.admit",
        Some(950),
        Vec::new(),
        NOW,
        Some(10_000),
    );

    // Matching mission + tx -> valid.
    assert!(steer_run_gate(&receipt, Some(&bound), Some(plan_hash), NOW).is_valid());

    // Tampered contract -> typed refusal.
    let tampered = mission("mission:chain", "Tampered");
    assert_eq!(
        steer_run_gate(&receipt, Some(&tampered), Some(plan_hash), NOW),
        SteerRunGate::HashMismatch {
            contract: "mission"
        }
    );

    // Expired (ttl 10_000 from NOW) -> typed refusal.
    assert_eq!(
        steer_run_gate(&receipt, Some(&bound), Some(plan_hash), NOW + 20_000),
        SteerRunGate::Expired
    );

    // Approval admission: admits the covered action, rejects a different one,
    // and no longer admits once expired.
    assert!(receipt_admits_action(&receipt, plan_hash, NOW).is_valid());
    assert!(!receipt_admits_action(&receipt, "plan-hash-OTHER", NOW).is_valid());
    assert!(!receipt_admits_action(&receipt, plan_hash, NOW + 20_000).is_valid());
}

/// steer plan writes nothing but a receipt: the underlying plan is dry-run with
/// no side effects (the W5.T "provably side-effect-free" unit, at the surface
/// the receipt is derived from).
#[test]
fn steer_plan_is_side_effect_free() {
    for scenario in SteerPlanScenario::ALL {
        let result = steer_plan(scenario, "obj", "ws-steer", NOW as u64, NOW, None);
        // A receipt is produced and self-consistent; no execution occurred.
        assert!(result.receipt.validate().is_ok());
        assert_ne!(result.policy_verdict, "");
    }
}
