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

// ---------------------------------------------------------------------------
// 5. Run gate: happy path + live-contract drift detection
// ---------------------------------------------------------------------------

#[test]
fn gate_admits_a_valid_unexpired_admit_receipt_with_a_confirmed_tx_binding() {
    let r = admit_receipt(Some("tx-A"), 1_000, Some(10_000));
    // Mission is unbound (None); the captured tx binding is confirmed by a
    // matching live hash, so the gate must pass. (A positive control: the gate
    // is not refusing everything.)
    let g = steer_run_gate(&r, None, Some("tx-A"), 5_000);
    assert!(
        g.is_valid(),
        "a sealed admit receipt with a confirmed tx binding must pass the gate, got {g:?}"
    );
}

#[test]
fn gate_rejects_tx_contract_drift_never_a_silent_replan() {
    let r = admit_receipt(Some("tx-A"), 1_000, Some(10_000));
    // The live tx hash differs from the one admitted: the contract drifted.
    let g = steer_run_gate(&r, None, Some("tx-B"), 5_000);
    assert!(!g.is_valid());
    assert!(
        matches!(g, SteerRunGate::HashMismatch { contract: "tx" }),
        "live tx drift must surface as a typed tx HashMismatch (never a silent re-plan), got {g:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Schema-invariant rejections: validate() fails closed -> gate Invalid
// ---------------------------------------------------------------------------

#[test]
fn negative_ttl_receipt_is_rejected_as_invalid() {
    // ttl is excluded from `receipt_id`, so `new` still seals a matching id; the
    // negative-ttl invariant is what `validate` must reject at the boundary.
    let r = admit_receipt(Some("plan-A"), 1_000, Some(-1));
    let g = receipt_admits_action(&r, "plan-A", 1_500);
    assert!(!g.is_valid());
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "a negative ttl must surface as Invalid, got {g:?}"
    );
}

#[test]
fn a_schema_version_from_the_future_is_rejected_as_invalid() {
    let mut r = admit_receipt(Some("plan-A"), 1_000, Some(10_000));
    // A receipt minted by a newer build must not be trusted by an older one
    // (forward-compatibility guard fails closed, not open).
    r.schema_version = u32::MAX;
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid());
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "an unsupported (future) schema version must surface as Invalid, got {g:?}"
    );
}

#[test]
fn a_rehearsal_score_out_of_range_is_rejected_as_invalid() {
    // Score > 1000 is sealed into a valid id but violates the schema invariant.
    let r = SteeringReceipt::new(
        "objective",
        "ws",
        None,
        Some("plan-A".to_string()),
        "envelope.admit",
        Some(1_001),
        Vec::new(),
        1_000,
        Some(10_000),
    );
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid());
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "a rehearsal score > 1000 must surface as Invalid, got {g:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Additional forgery vectors: every content-bound field is sealed
// ---------------------------------------------------------------------------

#[test]
fn moving_a_receipt_across_workspaces_without_resealing_is_rejected() {
    // "receipts never cross workspaces": workspace_id is in canonical_string, so
    // repointing it at another workspace without re-sealing must be caught.
    let mut r = admit_receipt(Some("plan-A"), 1_000, Some(10_000));
    assert!(receipt_admits_action(&r, "plan-A", 5_000).is_valid());

    r.workspace_id = "other-workspace".to_string();
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid(), "a cross-workspace-repurposed receipt must not admit");
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "workspace tamper must surface as Invalid (receipt_id mismatch), got {g:?}"
    );
}

#[test]
fn altering_the_required_approval_set_without_resealing_is_rejected() {
    // required_approvals is sealed (sorted) into receipt_id, so silently
    // growing/shrinking the approval set must be rejected.
    let mut r = SteeringReceipt::new(
        "objective",
        "ws",
        None,
        Some("plan-A".to_string()),
        "envelope.admit",
        Some(900),
        vec!["approve-deploy".to_string()],
        1_000,
        Some(10_000),
    );
    assert!(receipt_admits_action(&r, "plan-A", 5_000).is_valid());

    r.required_approvals.push("approve-extra".to_string());
    let g = receipt_admits_action(&r, "plan-A", 5_000);
    assert!(!g.is_valid(), "an altered approval set must not admit");
    assert!(
        matches!(g, SteerRunGate::Invalid(_)),
        "approvals tamper must surface as Invalid (receipt_id mismatch), got {g:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. Typed robot error codes: the refusal contract surfaced to callers
// ---------------------------------------------------------------------------

#[test]
fn each_admission_outcome_surfaces_its_typed_robot_error_code() {
    // Valid admit -> no error code.
    let ok = admit_receipt(Some("plan-A"), 1_000, Some(10_000));
    assert_eq!(
        receipt_admits_action(&ok, "plan-A", 5_000).error_code(),
        None,
        "an admitted action has no refusal code"
    );

    // HashMismatch (tx drift) -> hash mismatch code.
    assert_eq!(
        receipt_admits_action(&ok, "plan-B", 5_000).error_code(),
        Some("robot.steer_hash_mismatch")
    );

    // Expired -> expired code.
    let expired = admit_receipt(Some("plan-A"), 1_000, Some(1_000));
    assert_eq!(
        receipt_admits_action(&expired, "plan-A", 5_000).error_code(),
        Some("robot.steer_receipt_expired")
    );

    // NotAdmitted (non-admit verdict) -> not-admitted code.
    let not_admit = SteeringReceipt::new(
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
    assert_eq!(
        receipt_admits_action(&not_admit, "plan-A", 5_000).error_code(),
        Some("robot.steer_receipt_not_admitted")
    );

    // UnverifiableBinding (captured tx, no live tx supplied) -> unverifiable code.
    assert_eq!(
        steer_run_gate(&ok, None, None, 5_000).error_code(),
        Some("robot.steer_binding_unverifiable")
    );

    // Invalid (tampered, unsealed) -> invalid code.
    let mut forged = admit_receipt(Some("plan-A"), 1_000, Some(10_000));
    forged.tx_contract_hash = Some("plan-B".to_string());
    assert_eq!(
        receipt_admits_action(&forged, "plan-B", 5_000).error_code(),
        Some("robot.steer_receipt_invalid")
    );
}
