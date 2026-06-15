//! Conformance for the SteeringReceipt admission flow (ft-7h5da.6.x).
//!
//! A `SteeringReceipt` is a content-addressed, plan-hash-scoped pre-approval. It
//! admits an action as a first-class alternative to a one-shot approval code —
//! but only for the *exact* plan it was issued for, only while unexpired, only
//! when it carries the `envelope.admit` verdict, and only while its bound fields
//! match its `receipt_id` content hash.
//!
//! These pin the security contract end-to-end (complementing the in-module
//! behavioral tests with comprehensive scoping + gate-level forgery resistance):
//!   1. admission is scoped to the exact plan hash,
//!   2. the run gate fails closed on unverifiable mission/tx bindings,
//!   3. expired / non-admit receipts never admit (no replay),
//!   4. tampering a bound field without re-sealing `receipt_id` is rejected
//!      (no forgery).

use frankenterm_core::steer_run::{receipt_admits_action, steer_run_gate, SteerRunGate};
use frankenterm_core::steering::SteeringReceipt;

/// A valid, sealed `envelope.admit` receipt bound to `tx_hash`.
fn admit_receipt(tx_hash: Option<&str>, created_at_ms: i64, ttl_ms: Option<i64>) -> SteeringReceipt {
    SteeringReceipt::new(
        "objective",
        "ws",
        None, // no mission binding
        tx_hash.map(str::to_string),
        "envelope.admit",
        Some(900),
        Vec::new(),
        created_at_ms,
        ttl_ms,
    )
}

// ---------------------------------------------------------------------------
// 1. Plan-hash scoping
// ---------------------------------------------------------------------------

#[test]
fn receipt_admits_only_the_exact_bound_plan_hash() {
    let r = admit_receipt(Some("plan-hash-XYZ"), 1_000, Some(10_000));

    // Exact match admits.
    assert!(
        receipt_admits_action(&r, "plan-hash-XYZ", 5_000).is_valid(),
        "the exact bound plan hash must be admitted"
    );

    // Every non-exact hash is refused with a typed tx HashMismatch — never admitted.
    for other in [
        "plan-hash-XY",   // prefix
        "plan-hash-XYZ ", // trailing space
        " plan-hash-XYZ", // leading space
        "PLAN-HASH-XYZ",  // case
        "plan-hash-OTHER",
        "plan-hash-XYZZ", // superstring
        "",               // empty
    ] {
        let g = receipt_admits_action(&r, other, 5_000);
        assert!(!g.is_valid(), "must not admit a non-exact plan hash: {other:?}");
        assert!(
            matches!(g, SteerRunGate::HashMismatch { contract: "tx" }),
            "non-exact plan hash {other:?} must be a typed tx HashMismatch, got {g:?}"
        );
    }
}

#[test]
fn receipt_without_a_tx_binding_admits_nothing() {
    let r = admit_receipt(None, 1_000, Some(10_000));
    let g = receipt_admits_action(&r, "any-plan", 5_000);
    assert!(!g.is_valid());
    assert!(matches!(g, SteerRunGate::HashMismatch { contract: "tx" }));
}

// ---------------------------------------------------------------------------
// 2. Fail-closed on unverifiable bindings
// ---------------------------------------------------------------------------

#[test]
fn steer_run_gate_fails_closed_on_unverifiable_tx_binding() {
    let r = admit_receipt(Some("tx-A"), 1_000, Some(10_000));
    // Captured a tx binding, but the caller supplied no live tx hash to confirm it.
    let g = steer_run_gate(&r, None, None, 5_000);
    assert!(!g.is_valid());
    assert!(matches!(g, SteerRunGate::UnverifiableBinding { contract: "tx" }));
}

#[test]
fn steer_run_gate_fails_closed_on_unverifiable_mission_binding() {
    let mut r = admit_receipt(Some("tx-A"), 1_000, Some(10_000));
    // Add a mission binding and re-seal the receipt id so it is internally valid.
    r.mission_contract_hash = Some("mission-hash".to_string());
    r.receipt_id = r.compute_id();
    // Captured a mission binding, but no live mission was supplied to confirm it.
    // (A live tx hash is supplied so the tx binding is verifiable and we isolate
    // the mission path.)
    let g = steer_run_gate(&r, None, Some("tx-A"), 5_000);
    assert!(!g.is_valid());
    assert!(matches!(g, SteerRunGate::UnverifiableBinding { contract: "mission" }));
}

// ---------------------------------------------------------------------------
// 3. No replay (expiry + verdict)
// ---------------------------------------------------------------------------

#[test]
fn expired_receipt_does_not_admit_no_replay_past_ttl() {
    // ttl 1_000 from created_at 1_000 => expires at 2_000.
    let r = admit_receipt(Some("plan-A"), 1_000, Some(1_000));
    assert!(
        receipt_admits_action(&r, "plan-A", 1_500).is_valid(),
        "must admit before expiry"
    );
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid(), "must not admit after expiry (no replay past TTL)");
    assert!(matches!(g, SteerRunGate::Expired));
}

#[test]
fn non_admit_verdict_receipt_never_admits() {
    let r = SteeringReceipt::new(
        "objective",
        "ws",
        None,
        Some("plan-A".to_string()),
        "envelope.requires_approval",
        Some(900),
        Vec::new(),
        1_000,
        Some(10_000),
    );
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid());
    assert!(matches!(g, SteerRunGate::NotAdmitted { .. }));
}

// ---------------------------------------------------------------------------
// 4. No forgery (content-hash integrity at the gate)
// ---------------------------------------------------------------------------

#[test]
fn repurposing_the_bound_plan_hash_without_resealing_is_rejected() {
    let mut r = admit_receipt(Some("plan-A"), 1_000, Some(10_000));
    assert!(receipt_admits_action(&r, "plan-A", 5_000).is_valid());

    // Forge: edit the bound tx hash to point at a different plan WITHOUT
    // recomputing receipt_id. The content-hash binding (tx_contract_hash is in
    // canonical_string) must catch it -> Invalid, never admit.
    r.tx_contract_hash = Some("plan-B".to_string());
    let g = receipt_admits_action(&r, "plan-B", 5_000);
    assert!(!g.is_valid(), "a tampered receipt must not admit");
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "tampering must surface as Invalid (receipt_id mismatch), got {g:?}"
    );
}

#[test]
fn flipping_the_verdict_to_admit_without_resealing_is_rejected() {
    // Issued as requires_approval; forging it to admit by editing the verdict
    // field (which IS in canonical_string) without re-sealing must be rejected.
    let mut r = SteeringReceipt::new(
        "objective",
        "ws",
        None,
        Some("plan-A".to_string()),
        "envelope.requires_approval",
        Some(900),
        Vec::new(),
        1_000,
        Some(10_000),
    );
    r.envelope_verdict = "envelope.admit".to_string();
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid(), "forging the verdict must not admit");
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "verdict tamper must surface as Invalid (receipt_id mismatch), got {g:?}"
    );
}
