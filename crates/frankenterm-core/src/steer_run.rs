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
        }
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }
}

/// Revalidate a receipt before executing it. Checks run in order — structural
/// validity, TTL, then mission/tx hash drift — returning the first failure as a
/// typed verdict (no silent re-plan). `live_mission` / `live_tx_hash` are the
/// freshly-recomputed live contract bindings; pass `None` to skip a binding the
/// receipt did not capture.
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
}
