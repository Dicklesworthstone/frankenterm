//! `SteeringReceipt` — binds a steering plan's admitted artifacts (the mission
//! and tx contracts) to their canonical content hashes, closing the
//! *plan/execute identity gap*: execution (W5.3) can refuse to run anything
//! whose live contract hash differs from what was planned, scored, and admitted.
//!
//! Bead: `ft-7h5da.6.1` (W5.1) of the 2026-06-06 dueling-idea-wizards program.
//! This module owns the TYPE + hash-binding + TTL only. The `ft steer plan` /
//! `ft steer run` pipeline (W5.2 / W5.3) and Robot/MCP parity (W5.5) build on it.
//!
//! Design notes:
//! - Fields intentionally avoid `#[serde(skip_serializing_if = ...)]` so the
//!   struct is safe for positional binary (varbincode) persistence as well as
//!   JSON — every field always serializes, in declaration order.
//! - The mission hash reuses the existing canonical [`Mission::compute_hash`].
//!   The tx hash is supplied by the caller from
//!   `MissionTxContract::compute_hash`, letting W5.3 recompute the live tx
//!   contract hash before execution without coupling this foundational receipt
//!   type to the larger tx contract surface.
//! - `receipt_id` is content-addressed over the *binding* (objective, workspace,
//!   bound hashes, verdict, rehearsal score, sorted approvals) and deliberately
//!   EXCLUDES timestamps, so an identical plan always yields an identical id.

use crate::plan::Mission;
use serde::{Deserialize, Serialize};

/// Stable contract id for the steering-receipt schema.
pub const STEERING_RECEIPT_CONTRACT_ID: &str = "ft.steering_receipt.v1";

/// Current schema version (forward-compatibility guard).
pub const STEERING_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Reasons a steering receipt fails its invariant/forward-compat checks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SteeringReceiptError {
    /// The receipt was produced by a newer schema than this binary supports.
    #[error("steering receipt schema_version {found} exceeds max supported {max_supported}")]
    UnsupportedSchemaVersion {
        /// Schema version found on the receipt.
        found: u32,
        /// Highest schema version this binary understands.
        max_supported: u32,
    },
    /// A negative TTL is nonsensical (time cannot run backwards).
    #[error("steering receipt has a negative ttl_ms ({0})")]
    NegativeTtl(i64),
    /// The rehearsal score must stay within the documented 0..=1000 range.
    #[error("steering receipt rehearsal_score {0} exceeds 1000")]
    RehearsalScoreOutOfRange(u32),
    /// The stored id does not match the current content binding.
    #[error("steering receipt id mismatch: found {found}, expected {expected}")]
    ReceiptIdMismatch {
        /// Stored id found on the receipt.
        found: String,
        /// Id recomputed from the current binding fields.
        expected: String,
    },
}

/// A content-hash-bound record of an admitted steering plan.
///
/// It does not execute anything; it is the *proof token* that a specific
/// (objective → plan → envelope verdict → rehearsal score → approvals) tuple was
/// admitted, against which execution validates the live contracts by hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringReceipt {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// Content-addressed id (`steer:<32 hex>`), derived from the binding.
    pub receipt_id: String,
    /// The operator/agent objective this plan serves.
    pub objective: String,
    /// Workspace scope (receipts never cross workspaces).
    pub workspace_id: String,
    /// Canonical hash of the bound mission contract, if any.
    pub mission_contract_hash: Option<String>,
    /// Canonical hash of the bound tx contract, if any (caller-supplied).
    pub tx_contract_hash: Option<String>,
    /// Operating-envelope verdict at admission time (e.g. `envelope.admit`).
    pub envelope_verdict: String,
    /// Advisory rehearsal score 0..=1000, or `None` if not scored.
    pub rehearsal_score: Option<u32>,
    /// Approvals this plan still requires before/at execution.
    pub required_approvals: Vec<String>,
    /// Creation time (epoch ms); excluded from `receipt_id`.
    pub created_at_ms: i64,
    /// Time-to-live in ms; `None` = never expires. Excluded from `receipt_id`.
    pub ttl_ms: Option<i64>,
}

impl SteeringReceipt {
    /// Build a receipt binding an optional mission (by its canonical
    /// [`Mission::compute_hash`]) and an optional caller-computed tx hash.
    #[must_use]
    pub fn new(
        objective: impl Into<String>,
        workspace_id: impl Into<String>,
        mission: Option<&Mission>,
        tx_contract_hash: Option<String>,
        envelope_verdict: impl Into<String>,
        rehearsal_score: Option<u32>,
        required_approvals: Vec<String>,
        created_at_ms: i64,
        ttl_ms: Option<i64>,
    ) -> Self {
        let mut receipt = Self {
            schema_version: STEERING_RECEIPT_SCHEMA_VERSION,
            receipt_id: String::new(),
            objective: objective.into(),
            workspace_id: workspace_id.into(),
            mission_contract_hash: mission.map(Mission::compute_hash),
            tx_contract_hash,
            envelope_verdict: envelope_verdict.into(),
            rehearsal_score,
            required_approvals,
            created_at_ms,
            ttl_ms,
        };
        receipt.receipt_id = receipt.compute_id();
        receipt
    }

    /// The canonical source of a mission's bind hash (thin pass-through so call
    /// sites document *why* they hash a mission this way).
    #[must_use]
    pub fn mission_hash(mission: &Mission) -> String {
        mission.compute_hash()
    }

    /// Deterministic canonical string used to derive [`Self::receipt_id`].
    /// Excludes `receipt_id` itself and all timestamps.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut approvals = self.required_approvals.clone();
        approvals.sort();
        [
            canonical_field("schema_version", &self.schema_version.to_string()),
            canonical_field("workspace_id", &self.workspace_id),
            canonical_field("objective", &self.objective),
            canonical_option_field(
                "mission_contract_hash",
                self.mission_contract_hash.as_deref(),
            ),
            canonical_option_field("tx_contract_hash", self.tx_contract_hash.as_deref()),
            canonical_field("envelope_verdict", &self.envelope_verdict),
            canonical_option_u32_field("rehearsal_score", self.rehearsal_score),
            canonical_list_field("required_approvals", &approvals),
        ]
        .join("|")
    }

    /// Compute the content-addressed receipt id from [`Self::canonical_string`].
    #[must_use]
    pub fn compute_id(&self) -> String {
        let hash = sha256_hex(&self.canonical_string());
        format!("steer:{}", &hash[..32])
    }

    /// Whether `now_ms` is at or past this receipt's expiry. `None` ttl and
    /// (defensively) negative ttl never expire; use [`Self::validate`] to reject
    /// a negative ttl at the boundary.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self.ttl_ms {
            Some(ttl) if ttl >= 0 => now_ms.saturating_sub(self.created_at_ms) >= ttl,
            _ => false,
        }
    }

    /// Whether this receipt's bound mission hash matches `mission`'s LIVE hash.
    /// A `false` here is the core safety signal: the contract drifted from what
    /// was admitted (the plan/execute identity gap), so execution must refuse.
    #[must_use]
    pub fn matches_mission(&self, mission: &Mission) -> bool {
        self.mission_contract_hash.as_deref() == Some(mission.compute_hash().as_str())
    }

    /// Whether this receipt's bound tx hash matches a caller-recomputed hash.
    #[must_use]
    pub fn matches_tx_hash(&self, live_tx_hash: &str) -> bool {
        self.tx_contract_hash.as_deref() == Some(live_tx_hash)
    }

    /// Forward-compatibility + invariant guard. Call before trusting a receipt
    /// loaded from disk or the wire.
    ///
    /// # Errors
    /// Returns [`SteeringReceiptError`] if the schema is too new, ttl < 0,
    /// rehearsal score exceeds 1000, or the stored id does not match the current
    /// content binding.
    pub fn validate(&self) -> Result<(), SteeringReceiptError> {
        if self.schema_version > STEERING_RECEIPT_SCHEMA_VERSION {
            return Err(SteeringReceiptError::UnsupportedSchemaVersion {
                found: self.schema_version,
                max_supported: STEERING_RECEIPT_SCHEMA_VERSION,
            });
        }
        if let Some(ttl) = self.ttl_ms {
            if ttl < 0 {
                return Err(SteeringReceiptError::NegativeTtl(ttl));
            }
        }
        if let Some(score) = self.rehearsal_score {
            if score > 1000 {
                return Err(SteeringReceiptError::RehearsalScoreOutOfRange(score));
            }
        }
        let expected = self.compute_id();
        if self.receipt_id != expected {
            return Err(SteeringReceiptError::ReceiptIdMismatch {
                found: self.receipt_id.clone(),
                expected,
            });
        }
        Ok(())
    }
}

fn canonical_value(value: &str) -> String {
    format!("{}:{value}", value.len())
}

fn canonical_field(label: &str, value: &str) -> String {
    format!("{label}={}", canonical_value(value))
}

fn canonical_option_field(label: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{label}=some:{}", canonical_value(value)),
        None => format!("{label}=none"),
    }
}

fn canonical_option_u32_field(label: &str, value: Option<u32>) -> String {
    match value {
        Some(value) => canonical_field(label, &value.to_string()),
        None => format!("{label}=none"),
    }
}

fn canonical_list_field(label: &str, values: &[String]) -> String {
    let mut output = format!("{label}=list:{}", values.len());
    for value in values {
        output.push('|');
        output.push_str(&canonical_value(value));
    }
    output
}

/// Local SHA-256 hex helper. Kept module-local (matching the per-module pattern
/// used across the crate, e.g. `plan.rs`/`approval.rs`) so this foundational
/// type does not couple to a higher-level module for its content hash.
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Mission, MissionId, MissionOwnership};

    fn sample_mission(title: &str) -> Mission {
        Mission::new(
            MissionId("mission:steer-test".to_string()),
            title,
            "ws-main",
            MissionOwnership {
                planner: "planner-agent".to_string(),
                dispatcher: "dispatcher-agent".to_string(),
                operator: "operator-human".to_string(),
            },
            1_704_000_000_000,
        )
    }

    fn sample_receipt(
        mission: Option<&Mission>,
        created_at_ms: i64,
        ttl_ms: Option<i64>,
    ) -> SteeringReceipt {
        SteeringReceipt::new(
            "spawn 5 codex panes for the pricing refactor",
            "ws-main",
            mission,
            Some("sha256:deadbeefdeadbeefdeadbeefdeadbeef".to_string()),
            "envelope.admit",
            Some(820),
            vec!["approve:ft-7h5da.6.3".to_string()],
            created_at_ms,
            ttl_ms,
        )
    }

    #[test]
    fn binds_mission_by_canonical_hash() {
        let m = sample_mission("Refactor pricing");
        let r = sample_receipt(Some(&m), 1_704_000_000_000, Some(60_000));
        assert_eq!(r.schema_version, STEERING_RECEIPT_SCHEMA_VERSION);
        assert_eq!(
            r.mission_contract_hash.as_deref(),
            Some(m.compute_hash().as_str())
        );
        assert!(r.matches_mission(&m), "freshly-bound mission must match");
        assert!(r.receipt_id.starts_with("steer:"));
        assert_eq!(r.receipt_id.len(), "steer:".len() + 32);
    }

    #[test]
    fn detects_contract_tamper_via_hash_mismatch() {
        // The whole point of W5: a drifted contract must NOT match the receipt.
        let m = sample_mission("Refactor pricing");
        let r = sample_receipt(Some(&m), 1_704_000_000_000, None);
        let mut tampered = m.clone();
        tampered.title = "Delete the pricing module".to_string();
        assert!(
            !r.matches_mission(&tampered),
            "a mutated mission must fail the hash binding (identity-gap guard)"
        );
    }

    #[test]
    fn receipt_id_excludes_timestamps() {
        // Identical plans admitted at different times share a receipt_id.
        let m = sample_mission("Refactor pricing");
        let a = sample_receipt(Some(&m), 1_704_000_000_000, Some(60_000));
        let b = sample_receipt(Some(&m), 1_999_999_999_999, None);
        assert_eq!(
            a.receipt_id, b.receipt_id,
            "receipt_id must exclude created_at/ttl"
        );
    }

    #[test]
    fn receipt_id_disambiguates_delimiter_bearing_approvals() {
        let a = SteeringReceipt::new(
            "spawn agents",
            "ws-main",
            None,
            None,
            "envelope.admit",
            Some(820),
            vec!["a,b".to_string(), "c".to_string()],
            1_704_000_000_000,
            None,
        );
        let b = SteeringReceipt::new(
            "spawn agents",
            "ws-main",
            None,
            None,
            "envelope.admit",
            Some(820),
            vec!["a".to_string(), "b,c".to_string()],
            1_704_000_000_000,
            None,
        );

        assert_ne!(a.canonical_string(), b.canonical_string());
        assert_ne!(a.receipt_id, b.receipt_id);
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let m = sample_mission("Refactor pricing");
        let r = sample_receipt(Some(&m), 1_704_000_000_000, Some(60_000));
        let json = serde_json::to_string(&r).expect("serialize");
        let back: SteeringReceipt = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }

    #[test]
    fn ttl_expiry_semantics() {
        let m = sample_mission("Refactor pricing");
        let created = 1_704_000_000_000;
        let r = sample_receipt(Some(&m), created, Some(60_000));
        assert!(!r.is_expired(created), "not expired at creation");
        assert!(
            !r.is_expired(created + 59_999),
            "not expired just before ttl"
        );
        assert!(r.is_expired(created + 60_000), "expired exactly at ttl");
        assert!(r.is_expired(created + 120_000), "expired well past ttl");
        let never = sample_receipt(Some(&m), created, None);
        assert!(!never.is_expired(i64::MAX), "None ttl never expires");
    }

    #[test]
    fn validate_rejects_future_schema_and_negative_ttl() {
        let m = sample_mission("Refactor pricing");
        let mut r = sample_receipt(Some(&m), 1_704_000_000_000, Some(60_000));
        assert!(r.validate().is_ok());

        r.schema_version = STEERING_RECEIPT_SCHEMA_VERSION + 1;
        assert!(matches!(
            r.validate(),
            Err(SteeringReceiptError::UnsupportedSchemaVersion { .. })
        ));

        r.schema_version = STEERING_RECEIPT_SCHEMA_VERSION;
        r.ttl_ms = Some(-1);
        assert!(matches!(
            r.validate(),
            Err(SteeringReceiptError::NegativeTtl(-1))
        ));
    }

    #[test]
    fn validate_rejects_score_over_1000_and_id_mismatch() {
        let m = sample_mission("Refactor pricing");
        let mut r = sample_receipt(Some(&m), 1_704_000_000_000, Some(60_000));
        r.rehearsal_score = Some(1001);
        assert!(matches!(
            r.validate(),
            Err(SteeringReceiptError::RehearsalScoreOutOfRange(1001))
        ));

        r.rehearsal_score = Some(1000);
        r.receipt_id = "steer:00000000000000000000000000000000".to_string();
        assert!(matches!(
            r.validate(),
            Err(SteeringReceiptError::ReceiptIdMismatch { .. })
        ));
    }

    #[test]
    fn tx_hash_binding_round_trips() {
        let r = sample_receipt(None, 1_704_000_000_000, None);
        assert!(r.mission_contract_hash.is_none());
        assert!(r.matches_tx_hash("sha256:deadbeefdeadbeefdeadbeefdeadbeef"));
        assert!(!r.matches_tx_hash("sha256:0000"));
    }

    #[test]
    fn mission_hash_helper_matches_compute_hash() {
        let m = sample_mission("Refactor pricing");
        assert_eq!(SteeringReceipt::mission_hash(&m), m.compute_hash());
    }
}
