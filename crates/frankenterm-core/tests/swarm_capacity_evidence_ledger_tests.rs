//! Tests for the audit-facing invariants of `SwarmCapacityEvidenceLedger`
//! in `frankenterm_core::runtime_telemetry`. The ledger underpins the
//! deferred-proof / reproducibility system (its `ledger_hash` is a stable
//! decision-record digest), but had no coverage. These pin the cheap,
//! input-free conformance invariants without constructing certificates or
//! tail-risk reports (which belong to the operator-owned decision path).

use frankenterm_core::runtime_telemetry::SwarmCapacityEvidenceLedger;

#[test]
fn evidence_ledger_default_is_empty_and_replays_nothing() {
    let ledger = SwarmCapacityEvidenceLedger::with_defaults();
    assert!(
        ledger.replay_all().is_empty(),
        "a fresh ledger has no recorded decisions to replay"
    );
    assert!(
        !ledger.ledger_hash().is_empty(),
        "ledger_hash must be a non-empty digest even for an empty ledger"
    );
}

#[test]
fn evidence_ledger_hash_is_deterministic() {
    let a = SwarmCapacityEvidenceLedger::with_defaults();
    let b = SwarmCapacityEvidenceLedger::with_defaults();
    assert_eq!(
        a.ledger_hash(),
        b.ledger_hash(),
        "two fresh ledgers must produce the same digest"
    );
    // Stable across repeated calls on the same instance (no per-call nonce).
    assert_eq!(a.ledger_hash(), a.ledger_hash());
}
