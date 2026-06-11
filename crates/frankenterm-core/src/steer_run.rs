//! ft-7h5da.6.3: the read-only revalidation gate for `ft steer run`.
//!
//! Before a steering receipt may drive execution it must pass this gate: refuse
//! with a TYPED verdict if the receipt is structurally invalid, expired (TTL
//! elapsed), or its bound mission/tx contract hash has drifted from the live
//! contract — NEVER a silent re-plan (the plan/execute identity guarantee).
//! Reuses [`SteeringReceipt`]'s `validate` / `is_expired` / `matches_mission` /
//! `matches_tx_hash`.
//!
//! The actual tx prepare/commit delegation — driving a *valid* receipt through
//! `ft mission run`'s envelope path with per-commit revalidation, embedding the
//! receipt id — is the remaining (execute) half of W5.3 and is intentionally
//! NOT in this module; this is the safety precondition for it.

use crate::plan::Mission;
use crate::steering::SteeringReceipt;

/// Typed outcome of the steer-run revalidation gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerRunGate {
    /// All checks pass; the receipt may drive execution.
    Valid,
    /// The receipt is structurally invalid (schema / ttl / score / id binding).
    Invalid(String),
    /// The receipt's TTL has elapsed.
    Expired,
    /// The receipt's bound contract hash differs from the live contract
    /// (`contract` is `"mission"` or `"tx"`).
    HashMismatch { contract: &'static str },
    /// The receipt CAPTURED a contract binding the caller did not supply for
    /// revalidation, so it cannot be confirmed. Fail closed — refuse rather than
    /// execute on an unverified contract (`contract` is `"mission"` or `"tx"`).
    UnverifiableBinding { contract: &'static str },
}

impl SteerRunGate {
    /// The typed robot error code for a refusal (`None` when [`Self::Valid`]).
    #[must_use]
    pub fn error_code(&self) -> Option<&'static str> {
        match self {
            Self::Valid => None,
            Self::Invalid(_) => Some("robot.steer_receipt_invalid"),
            Self::Expired => Some("robot.steer_receipt_expired"),
            Self::HashMismatch { .. } => Some("robot.steer_hash_mismatch"),
            Self::UnverifiableBinding { .. } => Some("robot.steer_binding_unverifiable"),
        }
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }
}

/// Revalidate a receipt before executing it. Checks run in order — structural
/// validity, TTL, unverifiable bindings, then mission/tx hash drift — returning
/// the first failure as a typed verdict (no silent re-plan).
///
/// `live_mission` / `live_tx_hash` are the caller's freshly-recomputed live
/// contract bindings. The caller MUST supply a live binding for every contract
/// the receipt captured: if the receipt bound a mission/tx hash but the matching
/// live value is `None`, the gate fails closed with
/// [`SteerRunGate::UnverifiableBinding`] rather than executing on an unconfirmed
/// contract. `None` is only legitimate for a contract the receipt did not bind.
#[must_use]
pub fn steer_run_gate(
    receipt: &SteeringReceipt,
    live_mission: Option<&Mission>,
    live_tx_hash: Option<&str>,
    now_ms: i64,
) -> SteerRunGate {
    if let Err(e) = receipt.validate() {
        return SteerRunGate::Invalid(e.to_string());
    }
    if receipt.is_expired(now_ms) {
        return SteerRunGate::Expired;
    }
    // Fail closed: a contract the receipt CAPTURED but the caller did not supply
    // cannot be revalidated. Refusing here prevents a caller that forgets a live
    // binding from silently bypassing drift detection.
    if receipt.mission_contract_hash.is_some() && live_mission.is_none() {
        return SteerRunGate::UnverifiableBinding {
            contract: "mission",
        };
    }
    if receipt.tx_contract_hash.is_some() && live_tx_hash.is_none() {
        return SteerRunGate::UnverifiableBinding { contract: "tx" };
    }
    if let Some(mission) = live_mission {
        if !receipt.matches_mission(mission) {
            return SteerRunGate::HashMismatch {
                contract: "mission",
            };
        }
    }
    if let Some(tx_hash) = live_tx_hash {
        if !receipt.matches_tx_hash(tx_hash) {
            return SteerRunGate::HashMismatch { contract: "tx" };
        }
    }
    SteerRunGate::Valid
}

/// ft-7h5da.6.4: whether a steering receipt admits an action that would
/// otherwise require per-step approval — the receipt as a first-class
/// ALTERNATIVE to a one-shot approval code.
///
/// Admits (returns [`SteerRunGate::Valid`]) iff the receipt is structurally
/// valid, unexpired, and its bound tx contract hash equals the action's live
/// plan hash (so it covers *this exact* plan, not a stale or different one). Any
/// other verdict — mismatch / expired / invalid — means the receipt does NOT
/// admit and the action falls back to requiring an approval code. Steering is
/// never mandatory: this only *subsidizes* the pre-validated path, it never
/// forces it and never weakens the underlying approval requirement.
///
/// This admission is SCOPED to the action's plan hash; a mission binding the
/// receipt also carries is an execution concern revalidated by
/// [`steer_run_gate`], not a precondition for admitting an individual action
/// (so admission does not require the caller to supply the live mission).
#[must_use]
pub fn receipt_admits_action(
    receipt: &SteeringReceipt,
    action_plan_hash: &str,
    now_ms: i64,
) -> SteerRunGate {
    if let Err(e) = receipt.validate() {
        return SteerRunGate::Invalid(e.to_string());
    }
    if receipt.is_expired(now_ms) {
        return SteerRunGate::Expired;
    }
    if !receipt.matches_tx_hash(action_plan_hash) {
        return SteerRunGate::HashMismatch { contract: "tx" };
    }
    SteerRunGate::Valid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(
        ttl_ms: Option<i64>,
        created_at_ms: i64,
        tx_hash: Option<String>,
    ) -> SteeringReceipt {
        SteeringReceipt::new(
            "objective",
            "ws",
            None,
            tx_hash,
            "envelope.admit",
            Some(900),
            Vec::new(),
            created_at_ms,
            ttl_ms,
        )
    }

    #[test]
    fn valid_receipt_passes() {
        let r = receipt(Some(10_000), 1_000, None);
        assert_eq!(steer_run_gate(&r, None, None, 5_000), SteerRunGate::Valid);
        assert!(steer_run_gate(&r, None, None, 5_000).error_code().is_none());
    }

    #[test]
    fn expired_receipt_is_typed_refusal() {
        let r = receipt(Some(1_000), 0, None);
        let g = steer_run_gate(&r, None, None, 2_000);
        assert_eq!(g, SteerRunGate::Expired);
        assert_eq!(g.error_code(), Some("robot.steer_receipt_expired"));
    }

    #[test]
    fn no_ttl_never_expires() {
        let r = receipt(None, 0, None);
        assert!(steer_run_gate(&r, None, None, i64::MAX).is_valid());
    }

    #[test]
    fn tx_hash_drift_is_typed_refusal() {
        let r = receipt(None, 0, Some("hash-A".to_string()));
        let g = steer_run_gate(&r, None, Some("hash-B"), 100);
        assert_eq!(g, SteerRunGate::HashMismatch { contract: "tx" });
        assert_eq!(g.error_code(), Some("robot.steer_hash_mismatch"));
        // Matching live hash -> valid.
        assert!(steer_run_gate(&r, None, Some("hash-A"), 100).is_valid());
    }

    #[test]
    fn gate_fails_closed_on_unverifiable_tx_binding() {
        // Receipt CAPTURED a tx binding, but the caller supplied no live tx hash
        // -> the binding can't be revalidated -> refuse rather than pass blind.
        let r = receipt(None, 0, Some("hash-A".to_string()));
        let g = steer_run_gate(&r, None, None, 100);
        assert_eq!(g, SteerRunGate::UnverifiableBinding { contract: "tx" });
        assert_eq!(g.error_code(), Some("robot.steer_binding_unverifiable"));
    }

    #[test]
    fn unbound_receipt_with_no_live_contracts_is_valid() {
        // A receipt that bound NO contracts is fine with no live contracts.
        let r = receipt(Some(10_000), 0, None);
        assert!(steer_run_gate(&r, None, None, 1_000).is_valid());
    }

    #[test]
    fn invalid_receipt_short_circuits() {
        // Negative ttl -> validate() fails before any other check.
        let r = receipt(Some(-1), 0, None);
        let g = steer_run_gate(&r, None, None, 100);
        assert!(matches!(g, SteerRunGate::Invalid(_)));
        assert_eq!(g.error_code(), Some("robot.steer_receipt_invalid"));
    }

    #[test]
    fn ttl_is_checked_before_hash() {
        // Expired AND tx-mismatched -> reports Expired (TTL is checked first).
        let r = receipt(Some(1_000), 0, Some("hash-A".to_string()));
        assert_eq!(
            steer_run_gate(&r, None, Some("hash-B"), 5_000),
            SteerRunGate::Expired
        );
    }

    // ft-7h5da.6.4: receipt-as-approval-alternative admission.

    #[test]
    fn receipt_admits_matching_action_plan() {
        let r = receipt(Some(10_000), 0, Some("plan-hash-XYZ".to_string()));
        // Covers exactly this action's plan hash, valid + unexpired -> admits.
        assert!(receipt_admits_action(&r, "plan-hash-XYZ", 1_000).is_valid());
    }

    #[test]
    fn receipt_does_not_admit_a_different_action() {
        let r = receipt(Some(10_000), 0, Some("plan-hash-XYZ".to_string()));
        // A receipt for a DIFFERENT plan must never bypass approval.
        let g = receipt_admits_action(&r, "plan-hash-OTHER", 1_000);
        assert!(!g.is_valid());
        assert_eq!(g.error_code(), Some("robot.steer_hash_mismatch"));
    }

    #[test]
    fn expired_receipt_does_not_admit() {
        let r = receipt(Some(1_000), 0, Some("plan-hash-XYZ".to_string()));
        assert!(!receipt_admits_action(&r, "plan-hash-XYZ", 5_000).is_valid());
    }
}
