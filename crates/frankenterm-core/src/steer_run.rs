//! ft-7h5da.6.3: the read-only revalidation gate for `ft steer run`.
//!
//! Before a steering receipt may drive execution it must pass this gate: refuse
//! with a TYPED verdict if the receipt is structurally invalid, expired (TTL
//! elapsed), or its bound mission/tx contract hash has drifted from the live
//! contract — NEVER a silent re-plan (the plan/execute identity guarantee).
//! Reuses [`SteeringReceipt`]'s `validate` / `is_expired` / `matches_mission` /
//! `matches_tx_hash`.
//!
//! This module also hosts the W5.6 live-supervision substrate: typed W1
//! semantic snapshots, W2 tx receipt summaries, W3 live events, and the pure
//! supervisor decision that chooses continue, slow-observe, compensate, or
//! complete.

use crate::plan::Mission;
use crate::steering::SteeringReceipt;
use serde::{Deserialize, Serialize};

/// Typed outcome of the steer-run revalidation gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerRunGate {
    /// All checks pass; the receipt may drive execution.
    Valid,
    /// The receipt is structurally invalid (schema / ttl / score / id binding).
    Invalid(String),
    /// The receipt's TTL has elapsed.
    Expired,
    /// The receipt was not admitted by the planning envelope.
    NotAdmitted { verdict: String },
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
            Self::NotAdmitted { .. } => Some("robot.steer_receipt_not_admitted"),
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
    if receipt.envelope_verdict != "envelope.admit" {
        return SteerRunGate::NotAdmitted {
            verdict: receipt.envelope_verdict.clone(),
        };
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
    if receipt.envelope_verdict != "envelope.admit" {
        return SteerRunGate::NotAdmitted {
            verdict: receipt.envelope_verdict.clone(),
        };
    }
    if !receipt.matches_tx_hash(action_plan_hash) {
        return SteerRunGate::HashMismatch { contract: "tx" };
    }
    SteerRunGate::Valid
}

/// Live semantic posture for a running steered mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSupervisionSemanticState {
    Unknown,
    Planning,
    Progressing,
    Waiting,
    Complete,
    Failed,
}

impl LiveSupervisionSemanticState {
    #[must_use]
    fn advances_progress(self) -> bool {
        matches!(
            self,
            Self::Planning | Self::Progressing | Self::Waiting | Self::Complete
        )
    }

    #[must_use]
    fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    #[must_use]
    fn is_failed(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// W1 semantic state snapshot consumed by live steering supervision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionSemanticSnapshot {
    /// Optional semantic zone id stamped on the observed segment/phase.
    pub zone_id: Option<String>,
    /// Stable semantic state label.
    pub state: LiveSupervisionSemanticState,
    /// Confidence in basis points (`0..=10_000`).
    pub confidence_bps: u16,
    /// Observation timestamp in epoch milliseconds.
    pub observed_at_ms: u64,
}

impl LiveSupervisionSemanticSnapshot {
    #[must_use]
    pub fn new(
        state: LiveSupervisionSemanticState,
        confidence_bps: u16,
        observed_at_ms: u64,
    ) -> Self {
        Self {
            zone_id: None,
            state,
            confidence_bps,
            observed_at_ms,
        }
    }

    #[must_use]
    pub fn with_zone(mut self, zone_id: impl Into<String>) -> Self {
        self.zone_id = Some(zone_id.into());
        self
    }
}

/// Transaction phase represented by a W2 receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSupervisionTxPhase {
    Prepare,
    Commit,
    Compensate,
}

/// Compact W2 transaction-receipt status for supervision decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSupervisionTxStatus {
    Started,
    Progress,
    Waiting,
    StepCompleted,
    MissionCompleted,
    Failed,
    CompensationCompleted,
}

impl LiveSupervisionTxStatus {
    #[must_use]
    fn advances_progress(self) -> bool {
        matches!(
            self,
            Self::Started
                | Self::Progress
                | Self::StepCompleted
                | Self::MissionCompleted
                | Self::CompensationCompleted
        )
    }

    #[must_use]
    fn is_complete(self) -> bool {
        matches!(self, Self::MissionCompleted)
    }

    #[must_use]
    fn is_failed(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// W2 receipt summary for one observed tx step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionTxReceipt {
    /// Step id from the tx contract/ledger.
    pub step_id: String,
    /// Tx phase represented by this receipt.
    pub phase: LiveSupervisionTxPhase,
    /// Receipt status.
    pub status: LiveSupervisionTxStatus,
    /// Receipt timestamp in epoch milliseconds.
    pub observed_at_ms: u64,
}

impl LiveSupervisionTxReceipt {
    #[must_use]
    pub fn new(
        step_id: impl Into<String>,
        phase: LiveSupervisionTxPhase,
        status: LiveSupervisionTxStatus,
        observed_at_ms: u64,
    ) -> Self {
        Self {
            step_id: step_id.into(),
            phase,
            status,
            observed_at_ms,
        }
    }
}

/// W3 live event kind consumed by supervision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSupervisionEventKind {
    WorkAdvanced,
    Heartbeat,
    Waiting,
    Error,
    MissionCompleted,
    ChaosStallInjected,
}

impl LiveSupervisionEventKind {
    #[must_use]
    fn advances_progress(self) -> bool {
        matches!(self, Self::WorkAdvanced | Self::MissionCompleted)
    }

    #[must_use]
    fn is_complete(self) -> bool {
        matches!(self, Self::MissionCompleted)
    }

    #[must_use]
    fn is_failure(self) -> bool {
        matches!(self, Self::Error)
    }

    #[must_use]
    fn is_chaos_stall(self) -> bool {
        matches!(self, Self::ChaosStallInjected)
    }
}

/// W3 event summary from `watch-events` / mission-event feeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionEvent {
    /// Event source label, for example `watch-events` or `mission-audit`.
    pub source: String,
    /// Event kind.
    pub kind: LiveSupervisionEventKind,
    /// Optional step id associated with the event.
    pub step_id: Option<String>,
    /// Observation timestamp in epoch milliseconds.
    pub observed_at_ms: u64,
}

impl LiveSupervisionEvent {
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        kind: LiveSupervisionEventKind,
        observed_at_ms: u64,
    ) -> Self {
        Self {
            source: source.into(),
            kind,
            step_id: None,
            observed_at_ms,
        }
    }

    #[must_use]
    pub fn with_step(mut self, step_id: impl Into<String>) -> Self {
        self.step_id = Some(step_id.into());
        self
    }
}

/// Snapshot evaluated by the live supervisor on each monitoring tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionSnapshot {
    /// Steering receipt id driving the mission.
    pub receipt_id: String,
    /// Mission id under supervision.
    pub mission_id: String,
    /// Mission start timestamp in epoch milliseconds.
    pub started_at_ms: u64,
    /// Evaluation timestamp in epoch milliseconds.
    pub now_ms: u64,
    /// Last durable progress timestamp, if a caller has one.
    pub last_progress_ms: Option<u64>,
    /// Latest W1 semantic snapshot.
    pub semantic: Option<LiveSupervisionSemanticSnapshot>,
    /// Recent W2 tx receipts.
    pub tx_receipts: Vec<LiveSupervisionTxReceipt>,
    /// Recent W3 events.
    pub events: Vec<LiveSupervisionEvent>,
}

impl LiveSupervisionSnapshot {
    #[must_use]
    pub fn new(
        receipt_id: impl Into<String>,
        mission_id: impl Into<String>,
        started_at_ms: u64,
        now_ms: u64,
    ) -> Self {
        Self {
            receipt_id: receipt_id.into(),
            mission_id: mission_id.into(),
            started_at_ms,
            now_ms,
            last_progress_ms: None,
            semantic: None,
            tx_receipts: Vec::new(),
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_last_progress_ms(mut self, last_progress_ms: u64) -> Self {
        self.last_progress_ms = Some(last_progress_ms);
        self
    }

    #[must_use]
    pub fn with_semantic(mut self, semantic: LiveSupervisionSemanticSnapshot) -> Self {
        self.semantic = Some(semantic);
        self
    }

    #[must_use]
    pub fn with_tx_receipt(mut self, receipt: LiveSupervisionTxReceipt) -> Self {
        self.tx_receipts.push(receipt);
        self
    }

    #[must_use]
    pub fn with_event(mut self, event: LiveSupervisionEvent) -> Self {
        self.events.push(event);
        self
    }
}

/// Supervisor timing thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionConfig {
    /// No-progress age that becomes a slow-but-observed warning.
    pub slow_after_ms: u64,
    /// No-progress age that becomes a stuck verdict.
    pub stuck_after_ms: u64,
    /// Chaos-stall event age that immediately requires compensation.
    pub chaos_stall_grace_ms: u64,
    /// Minimum semantic confidence in basis points.
    pub min_semantic_confidence_bps: u16,
}

impl Default for LiveSupervisionConfig {
    fn default() -> Self {
        Self {
            slow_after_ms: 60_000,
            stuck_after_ms: 180_000,
            chaos_stall_grace_ms: 10_000,
            min_semantic_confidence_bps: 5_000,
        }
    }
}

/// Live-supervision verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSupervisionVerdict {
    Progressing,
    Slow,
    Stuck,
    Complete,
    Failed,
}

/// Action selected by the live supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSupervisionAction {
    Continue,
    ObserveSlow,
    TriggerCompensation,
    MarkComplete,
}

/// One deterministic audit row emitted by the supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionAuditEntry {
    /// Event time in epoch milliseconds.
    pub timestamp_ms: u64,
    /// Stable reason code.
    pub reason_code: String,
    /// Human-readable diagnostic note.
    pub detail: String,
}

impl LiveSupervisionAuditEntry {
    #[must_use]
    pub fn new(timestamp_ms: u64, reason_code: &str, detail: impl Into<String>) -> Self {
        Self {
            timestamp_ms,
            reason_code: reason_code.to_string(),
            detail: detail.into(),
        }
    }
}

/// Decision returned for one live supervision tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSupervisionDecision {
    pub verdict: LiveSupervisionVerdict,
    pub action: LiveSupervisionAction,
    pub progress_anchor_ms: u64,
    pub no_progress_ms: u64,
    pub audit: Vec<LiveSupervisionAuditEntry>,
}

/// Pure live supervisor for steered missions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveSupervisor {
    config: LiveSupervisionConfig,
}

impl Default for LiveSupervisor {
    fn default() -> Self {
        Self::new(LiveSupervisionConfig::default())
    }
}

impl LiveSupervisor {
    #[must_use]
    pub fn new(config: LiveSupervisionConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn config(&self) -> LiveSupervisionConfig {
        self.config
    }

    /// Evaluate one monitoring tick and return the compensate-or-complete action.
    #[must_use]
    pub fn evaluate(&self, snapshot: &LiveSupervisionSnapshot) -> LiveSupervisionDecision {
        let mut audit = vec![LiveSupervisionAuditEntry::new(
            snapshot.now_ms,
            "mission.supervision.tick",
            format!(
                "evaluated mission {} under receipt {}",
                snapshot.mission_id, snapshot.receipt_id
            ),
        )];

        if self.has_completion_evidence(snapshot) {
            audit.push(LiveSupervisionAuditEntry::new(
                snapshot.now_ms,
                "mission.supervision.complete",
                "completion evidence observed; mark mission complete",
            ));
            return self.decision(
                snapshot,
                LiveSupervisionVerdict::Complete,
                LiveSupervisionAction::MarkComplete,
                audit,
            );
        }

        if self.has_failure_evidence(snapshot) {
            audit.push(LiveSupervisionAuditEntry::new(
                snapshot.now_ms,
                "mission.supervision.failed",
                "failure evidence observed; trigger compensation",
            ));
            return self.decision(
                snapshot,
                LiveSupervisionVerdict::Failed,
                LiveSupervisionAction::TriggerCompensation,
                audit,
            );
        }

        let progress_anchor_ms = self.progress_anchor_ms(snapshot);
        let no_progress_ms = snapshot.now_ms.saturating_sub(progress_anchor_ms);

        if let Some(stall_age_ms) = self.chaos_stall_age_ms(snapshot) {
            if stall_age_ms >= self.config.chaos_stall_grace_ms {
                audit.push(LiveSupervisionAuditEntry::new(
                    snapshot.now_ms,
                    "mission.supervision.chaos_stall",
                    format!("chaos stall exceeded grace window by {stall_age_ms}ms"),
                ));
                return LiveSupervisionDecision {
                    verdict: LiveSupervisionVerdict::Stuck,
                    action: LiveSupervisionAction::TriggerCompensation,
                    progress_anchor_ms,
                    no_progress_ms,
                    audit,
                };
            }
        }

        if no_progress_ms >= self.config.stuck_after_ms {
            audit.push(LiveSupervisionAuditEntry::new(
                snapshot.now_ms,
                "mission.supervision.stuck",
                format!("no progress for {no_progress_ms}ms; trigger compensation"),
            ));
            return LiveSupervisionDecision {
                verdict: LiveSupervisionVerdict::Stuck,
                action: LiveSupervisionAction::TriggerCompensation,
                progress_anchor_ms,
                no_progress_ms,
                audit,
            };
        }

        if no_progress_ms >= self.config.slow_after_ms {
            audit.push(LiveSupervisionAuditEntry::new(
                snapshot.now_ms,
                "mission.supervision.slow",
                format!("no progress for {no_progress_ms}ms; keep observing"),
            ));
            return LiveSupervisionDecision {
                verdict: LiveSupervisionVerdict::Slow,
                action: LiveSupervisionAction::ObserveSlow,
                progress_anchor_ms,
                no_progress_ms,
                audit,
            };
        }

        audit.push(LiveSupervisionAuditEntry::new(
            snapshot.now_ms,
            "mission.supervision.progressing",
            format!("latest progress is {no_progress_ms}ms old; continue"),
        ));
        LiveSupervisionDecision {
            verdict: LiveSupervisionVerdict::Progressing,
            action: LiveSupervisionAction::Continue,
            progress_anchor_ms,
            no_progress_ms,
            audit,
        }
    }

    fn decision(
        &self,
        snapshot: &LiveSupervisionSnapshot,
        verdict: LiveSupervisionVerdict,
        action: LiveSupervisionAction,
        audit: Vec<LiveSupervisionAuditEntry>,
    ) -> LiveSupervisionDecision {
        let progress_anchor_ms = self.progress_anchor_ms(snapshot);
        LiveSupervisionDecision {
            verdict,
            action,
            progress_anchor_ms,
            no_progress_ms: snapshot.now_ms.saturating_sub(progress_anchor_ms),
            audit,
        }
    }

    fn progress_anchor_ms(&self, snapshot: &LiveSupervisionSnapshot) -> u64 {
        let mut anchor = snapshot
            .last_progress_ms
            .unwrap_or(snapshot.started_at_ms)
            .max(snapshot.started_at_ms);

        if let Some(semantic) = &snapshot.semantic {
            if semantic.confidence_bps >= self.config.min_semantic_confidence_bps
                && semantic.state.advances_progress()
            {
                anchor = anchor.max(semantic.observed_at_ms);
            }
        }

        for receipt in &snapshot.tx_receipts {
            if receipt.status.advances_progress() {
                anchor = anchor.max(receipt.observed_at_ms);
            }
        }

        for event in &snapshot.events {
            if event.kind.advances_progress() {
                anchor = anchor.max(event.observed_at_ms);
            }
        }

        anchor.min(snapshot.now_ms)
    }

    fn has_completion_evidence(&self, snapshot: &LiveSupervisionSnapshot) -> bool {
        snapshot.semantic.as_ref().is_some_and(|semantic| {
            semantic.confidence_bps >= self.config.min_semantic_confidence_bps
                && semantic.state.is_complete()
        }) || snapshot
            .tx_receipts
            .iter()
            .any(|receipt| receipt.status.is_complete())
            || snapshot.events.iter().any(|event| event.kind.is_complete())
    }

    fn has_failure_evidence(&self, snapshot: &LiveSupervisionSnapshot) -> bool {
        snapshot.semantic.as_ref().is_some_and(|semantic| {
            semantic.confidence_bps >= self.config.min_semantic_confidence_bps
                && semantic.state.is_failed()
        }) || snapshot
            .tx_receipts
            .iter()
            .any(|receipt| receipt.status.is_failed())
            || snapshot.events.iter().any(|event| event.kind.is_failure())
    }

    fn chaos_stall_age_ms(&self, snapshot: &LiveSupervisionSnapshot) -> Option<u64> {
        snapshot
            .events
            .iter()
            .filter(|event| event.kind.is_chaos_stall())
            .map(|event| snapshot.now_ms.saturating_sub(event.observed_at_ms))
            .max()
    }
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
    fn non_admitted_receipt_is_typed_refusal() {
        let mut r = receipt(Some(10_000), 1_000, None);
        r.envelope_verdict = "envelope.blocked.rch_substrate".to_string();
        r.receipt_id = r.compute_id();

        let g = steer_run_gate(&r, None, None, 5_000);

        assert_eq!(
            g,
            SteerRunGate::NotAdmitted {
                verdict: "envelope.blocked.rch_substrate".to_string()
            }
        );
        assert_eq!(g.error_code(), Some("robot.steer_receipt_not_admitted"));
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

    #[test]
    fn non_admitted_receipt_does_not_admit_action() {
        let mut r = receipt(Some(10_000), 0, Some("plan-hash-XYZ".to_string()));
        r.envelope_verdict = "envelope.requires_approval".to_string();
        r.receipt_id = r.compute_id();

        let g = receipt_admits_action(&r, "plan-hash-XYZ", 1_000);

        assert!(!g.is_valid());
        assert_eq!(g.error_code(), Some("robot.steer_receipt_not_admitted"));
    }

    #[test]
    fn live_supervisor_treats_recent_work_event_as_progressing() {
        let supervisor = LiveSupervisor::new(LiveSupervisionConfig {
            slow_after_ms: 60_000,
            stuck_after_ms: 180_000,
            chaos_stall_grace_ms: 10_000,
            min_semantic_confidence_bps: 5_000,
        });
        let snapshot = LiveSupervisionSnapshot::new("steer:abc", "mission:w5", 0, 150_000)
            .with_last_progress_ms(0)
            .with_event(LiveSupervisionEvent::new(
                "watch-events",
                LiveSupervisionEventKind::WorkAdvanced,
                149_000,
            ));

        let decision = supervisor.evaluate(&snapshot);

        assert_eq!(decision.verdict, LiveSupervisionVerdict::Progressing);
        assert_eq!(decision.action, LiveSupervisionAction::Continue);
        assert_eq!(decision.progress_anchor_ms, 149_000);
        assert_eq!(decision.no_progress_ms, 1_000);
    }

    #[test]
    fn live_supervisor_distinguishes_heartbeat_only_slow_from_progress() {
        let supervisor = LiveSupervisor::default();
        let snapshot = LiveSupervisionSnapshot::new("steer:abc", "mission:w5", 0, 90_000)
            .with_last_progress_ms(0)
            .with_event(LiveSupervisionEvent::new(
                "watch-events",
                LiveSupervisionEventKind::Heartbeat,
                89_000,
            ));

        let decision = supervisor.evaluate(&snapshot);

        assert_eq!(decision.verdict, LiveSupervisionVerdict::Slow);
        assert_eq!(decision.action, LiveSupervisionAction::ObserveSlow);
        assert_eq!(decision.progress_anchor_ms, 0);
        assert_eq!(decision.no_progress_ms, 90_000);
        assert!(decision
            .audit
            .iter()
            .any(|entry| entry.reason_code == "mission.supervision.slow"));
    }

    #[test]
    fn live_supervisor_triggers_compensation_when_stuck() {
        let supervisor = LiveSupervisor::default();
        let snapshot = LiveSupervisionSnapshot::new("steer:abc", "mission:w5", 0, 240_000)
            .with_last_progress_ms(30_000);

        let decision = supervisor.evaluate(&snapshot);

        assert_eq!(decision.verdict, LiveSupervisionVerdict::Stuck);
        assert_eq!(decision.action, LiveSupervisionAction::TriggerCompensation);
        assert_eq!(decision.no_progress_ms, 210_000);
    }

    #[test]
    fn live_supervisor_marks_complete_without_compensation() {
        let supervisor = LiveSupervisor::default();
        let snapshot = LiveSupervisionSnapshot::new("steer:abc", "mission:w5", 0, 240_000)
            .with_semantic(
                LiveSupervisionSemanticSnapshot::new(
                    LiveSupervisionSemanticState::Complete,
                    9_000,
                    239_000,
                )
                .with_zone("mission.phase3.complete"),
            );

        let decision = supervisor.evaluate(&snapshot);

        assert_eq!(decision.verdict, LiveSupervisionVerdict::Complete);
        assert_eq!(decision.action, LiveSupervisionAction::MarkComplete);
        assert!(decision
            .audit
            .iter()
            .any(|entry| entry.reason_code == "mission.supervision.complete"));
    }

    #[test]
    fn live_supervisor_handles_chaos_injected_stall_as_stuck() {
        let supervisor = LiveSupervisor::default();
        let snapshot = LiveSupervisionSnapshot::new("steer:abc", "mission:w5", 0, 80_000)
            .with_last_progress_ms(75_000)
            .with_event(LiveSupervisionEvent::new(
                "chaos",
                LiveSupervisionEventKind::ChaosStallInjected,
                60_000,
            ));

        let decision = supervisor.evaluate(&snapshot);

        assert_eq!(decision.verdict, LiveSupervisionVerdict::Stuck);
        assert_eq!(decision.action, LiveSupervisionAction::TriggerCompensation);
        assert_eq!(decision.no_progress_ms, 5_000);
        assert!(decision
            .audit
            .iter()
            .any(|entry| entry.reason_code == "mission.supervision.chaos_stall"));
    }

    #[test]
    fn live_supervisor_uses_w2_tx_receipt_progress() {
        let supervisor = LiveSupervisor::default();
        let snapshot = LiveSupervisionSnapshot::new("steer:abc", "mission:w5", 0, 70_000)
            .with_tx_receipt(LiveSupervisionTxReceipt::new(
                "step-1",
                LiveSupervisionTxPhase::Commit,
                LiveSupervisionTxStatus::StepCompleted,
                65_000,
            ));

        let decision = supervisor.evaluate(&snapshot);

        assert_eq!(decision.verdict, LiveSupervisionVerdict::Progressing);
        assert_eq!(decision.action, LiveSupervisionAction::Continue);
        assert_eq!(decision.progress_anchor_ms, 65_000);
    }
}
