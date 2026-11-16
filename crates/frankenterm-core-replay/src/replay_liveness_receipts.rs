//! Deterministic agent liveness and stale-ownership receipt model.
//!
//! This module is a replay/proof surface for the receipts Robot Mode and Beads
//! can eventually expose. It does not shell out to `br`, inspect live panes, or
//! rely on Agent Mail availability.

use serde::{Deserialize, Serialize};

/// Schema version for agent liveness receipt reports.
pub const AGENT_LIVENESS_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Coarse liveness and ownership classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLivenessClass {
    /// Pane and output heartbeat are fresh.
    Alive,
    /// Pane heartbeat exists, but output is quiet within policy bounds.
    QuietButActive,
    /// The pane or process is unreachable, but stale-claim criteria are not met.
    Unreachable,
    /// Beads ownership appears abandoned and needs a policy-gated recovery step.
    StaleClaim,
    /// Live evidence conflicts with Beads ownership.
    ConflictingClaim,
    /// Infrastructure degradation prevents a definitive ownership decision.
    RecoveryNeeded,
}

impl AgentLivenessClass {
    /// Stable key for reports and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::QuietButActive => "quiet_but_active",
            Self::Unreachable => "unreachable",
            Self::StaleClaim => "stale_claim",
            Self::ConflictingClaim => "conflicting_claim",
            Self::RecoveryNeeded => "recovery_needed",
        }
    }
}

/// Safe next action recommended by a liveness receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleOwnershipAction {
    /// Wait; the evidence indicates normal progress or quiet activity.
    Wait,
    /// Ping the owner or check the pane before changing ownership.
    Ping,
    /// Reopen the stale claim after policy approval.
    Reopen,
    /// Reassign the claim after policy approval.
    Reassign,
    /// Escalate to a human because evidence conflicts or is unsafe.
    EscalateHuman,
}

impl StaleOwnershipAction {
    /// Stable key for reports and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Ping => "ping",
            Self::Reopen => "reopen",
            Self::Reassign => "reassign",
            Self::EscalateHuman => "escalate_human",
        }
    }
}

/// Evidence field used by the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessEvidenceKind {
    /// Pane heartbeat age.
    PaneHeartbeat,
    /// Recent pane output age.
    RecentOutput,
    /// Working directory signal.
    WorkingDirectory,
    /// Command/process identity signal.
    CommandIdentity,
    /// Beads assignee signal.
    BeadsAssignment,
    /// Agent Mail health signal.
    AgentMailHealth,
    /// Age of the Beads ownership claim.
    ClaimAge,
    /// Owner mismatch signal.
    OwnerConflict,
    /// Pane missing/dead signal.
    PaneMissing,
}

/// Agent Mail health as observed by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMailHealth {
    /// Agent Mail is healthy enough to use as evidence.
    Healthy,
    /// Agent Mail is degraded but responsive enough to disclose.
    Degraded,
    /// Agent Mail is unreachable.
    Unreachable,
    /// Agent Mail was deliberately skipped by policy or caller.
    Skipped,
}

impl AgentMailHealth {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unreachable => "unreachable",
            Self::Skipped => "skipped",
        }
    }

    /// Whether the health state should block automatic ownership recovery.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded | Self::Unreachable)
    }
}

/// One evidence item in a liveness receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivenessEvidence {
    /// Evidence kind.
    pub kind: LivenessEvidenceKind,
    /// Whether this evidence supports the classification.
    pub supports_classification: bool,
    /// Stable detail string.
    pub detail: String,
}

/// Thresholds used by the deterministic liveness classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLivenessPolicy {
    /// Heartbeat age considered fresh.
    pub fresh_heartbeat_ms: u64,
    /// Recent output age considered fresh.
    pub fresh_output_ms: u64,
    /// Maximum heartbeat age for quiet-but-active work.
    pub quiet_active_heartbeat_ms: u64,
    /// Beads claim age after which missing panes are stale.
    pub stale_claim_ms: u64,
}

impl Default for AgentLivenessPolicy {
    fn default() -> Self {
        Self {
            fresh_heartbeat_ms: 120_000,
            fresh_output_ms: 120_000,
            quiet_active_heartbeat_ms: 900_000,
            stale_claim_ms: 1_800_000,
        }
    }
}

/// Deterministic input for one agent liveness decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLivenessInput {
    /// Agent identity being evaluated.
    pub agent_name: String,
    /// Pane id if one is known.
    pub pane_id: Option<u64>,
    /// Whether the pane still exists.
    pub pane_alive: bool,
    /// Age of the last heartbeat, when known.
    pub heartbeat_age_ms: Option<u64>,
    /// Age of the last observed output, when known.
    pub recent_output_age_ms: Option<u64>,
    /// Last observed working directory.
    pub cwd: Option<String>,
    /// Last observed command identity.
    pub command_identity: Option<String>,
    /// Beads assignee for the claimed work, when known.
    pub beads_assignee: Option<String>,
    /// Age of the Beads ownership claim, when known.
    pub claim_age_ms: Option<u64>,
    /// Agent Mail health signal.
    pub agent_mail_health: AgentMailHealth,
}

/// One typed liveness and ownership receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLivenessReceipt {
    /// Stable receipt id.
    pub receipt_id: String,
    /// Agent identity being evaluated.
    pub agent_name: String,
    /// Liveness/ownership classification.
    pub classification: AgentLivenessClass,
    /// Safe next action.
    pub recommended_action: StaleOwnershipAction,
    /// Whether a policy gate must approve the action.
    pub policy_gate_required: bool,
    /// Whether automatic reopening is allowed.
    pub automatic_reopen_allowed: bool,
    /// Evidence trail used for the classification.
    pub evidence: Vec<LivenessEvidence>,
    /// Concise operator-facing summary.
    pub operator_summary: String,
}

/// Compact report summary suitable for checked fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLivenessSummary {
    /// Report schema version.
    pub schema_version: u32,
    /// Total receipts.
    pub total_agents: usize,
    /// Alive count.
    pub alive: usize,
    /// Quiet-but-active count.
    pub quiet_but_active: usize,
    /// Unreachable count.
    pub unreachable: usize,
    /// Stale-claim count.
    pub stale_claim: usize,
    /// Conflicting-claim count.
    pub conflicting_claim: usize,
    /// Recovery-needed count.
    pub recovery_needed: usize,
    /// Wait recommendation count.
    pub wait: usize,
    /// Ping recommendation count.
    pub ping: usize,
    /// Reopen recommendation count.
    pub reopen: usize,
    /// Reassign recommendation count.
    pub reassign: usize,
    /// Escalate-human recommendation count.
    pub escalate_human: usize,
    /// Receipts requiring policy approval.
    pub policy_gated_receipts: usize,
    /// Receipts allowing automatic reopen.
    pub automatic_reopen_allowed: usize,
    /// Concise operator summary.
    pub operator_summary: String,
}

impl AgentLivenessSummary {
    /// Render a stable compact TOON-like fixture.
    #[must_use]
    pub fn to_toon(&self) -> String {
        format!(
            "schema_version: {}\ntotal_agents: {}\nclasses: alive={} quiet_but_active={} unreachable={} stale_claim={} conflicting_claim={} recovery_needed={}\nactions: wait={} ping={} reopen={} reassign={} escalate_human={}\npolicy_gated_receipts: {}\nautomatic_reopen_allowed: {}\noperator_summary: {}\n",
            self.schema_version,
            self.total_agents,
            self.alive,
            self.quiet_but_active,
            self.unreachable,
            self.stale_claim,
            self.conflicting_claim,
            self.recovery_needed,
            self.wait,
            self.ping,
            self.reopen,
            self.reassign,
            self.escalate_human,
            self.policy_gated_receipts,
            self.automatic_reopen_allowed,
            self.operator_summary
        )
    }
}

/// Full deterministic liveness report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLivenessReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Receipts sorted by agent name.
    pub receipts: Vec<AgentLivenessReceipt>,
}

impl AgentLivenessReport {
    /// Build a compact summary from receipts.
    #[must_use]
    pub fn summary(&self) -> AgentLivenessSummary {
        let mut summary = AgentLivenessSummary {
            schema_version: self.schema_version,
            total_agents: self.receipts.len(),
            alive: 0,
            quiet_but_active: 0,
            unreachable: 0,
            stale_claim: 0,
            conflicting_claim: 0,
            recovery_needed: 0,
            wait: 0,
            ping: 0,
            reopen: 0,
            reassign: 0,
            escalate_human: 0,
            policy_gated_receipts: 0,
            automatic_reopen_allowed: 0,
            operator_summary: String::new(),
        };

        for receipt in &self.receipts {
            match receipt.classification {
                AgentLivenessClass::Alive => summary.alive += 1,
                AgentLivenessClass::QuietButActive => summary.quiet_but_active += 1,
                AgentLivenessClass::Unreachable => summary.unreachable += 1,
                AgentLivenessClass::StaleClaim => summary.stale_claim += 1,
                AgentLivenessClass::ConflictingClaim => summary.conflicting_claim += 1,
                AgentLivenessClass::RecoveryNeeded => summary.recovery_needed += 1,
            }
            match receipt.recommended_action {
                StaleOwnershipAction::Wait => summary.wait += 1,
                StaleOwnershipAction::Ping => summary.ping += 1,
                StaleOwnershipAction::Reopen => summary.reopen += 1,
                StaleOwnershipAction::Reassign => summary.reassign += 1,
                StaleOwnershipAction::EscalateHuman => summary.escalate_human += 1,
            }
            if receipt.policy_gate_required {
                summary.policy_gated_receipts += 1;
            }
            if receipt.automatic_reopen_allowed {
                summary.automatic_reopen_allowed += 1;
            }
        }

        summary.operator_summary = format!(
            "agent_liveness: {} alive, {} quiet, {} unreachable, {} stale, {} conflicting, {} recovery_needed; actions wait={} ping={} reopen={} reassign={} escalate={}; policy_gated={}; auto_reopen={}",
            summary.alive,
            summary.quiet_but_active,
            summary.unreachable,
            summary.stale_claim,
            summary.conflicting_claim,
            summary.recovery_needed,
            summary.wait,
            summary.ping,
            summary.reopen,
            summary.reassign,
            summary.escalate_human,
            summary.policy_gated_receipts,
            summary.automatic_reopen_allowed
        );
        summary
    }
}

/// Evaluate agent liveness receipts deterministically.
#[must_use]
pub fn evaluate_agent_liveness(
    policy: &AgentLivenessPolicy,
    inputs: &[AgentLivenessInput],
) -> AgentLivenessReport {
    let mut sorted = inputs.to_vec();
    sorted.sort_by(|left, right| left.agent_name.cmp(&right.agent_name));

    let receipts = sorted
        .iter()
        .map(|input| classify_agent_liveness(policy, input))
        .collect();

    AgentLivenessReport {
        schema_version: AGENT_LIVENESS_RECEIPT_SCHEMA_VERSION,
        receipts,
    }
}

fn classify_agent_liveness(
    policy: &AgentLivenessPolicy,
    input: &AgentLivenessInput,
) -> AgentLivenessReceipt {
    let owner_conflict = input
        .beads_assignee
        .as_deref()
        .is_some_and(|assignee| assignee != input.agent_name);
    let stale_claim = input
        .claim_age_ms
        .is_some_and(|age| age >= policy.stale_claim_ms);
    let heartbeat_fresh = input
        .heartbeat_age_ms
        .is_some_and(|age| age <= policy.fresh_heartbeat_ms);
    let output_fresh = input
        .recent_output_age_ms
        .is_some_and(|age| age <= policy.fresh_output_ms);
    let heartbeat_quiet_active = input
        .heartbeat_age_ms
        .is_some_and(|age| age <= policy.quiet_active_heartbeat_ms);

    let (classification, recommended_action, policy_gate_required) = if owner_conflict {
        (
            AgentLivenessClass::ConflictingClaim,
            StaleOwnershipAction::EscalateHuman,
            true,
        )
    } else if !input.pane_alive && stale_claim {
        (
            AgentLivenessClass::StaleClaim,
            StaleOwnershipAction::Reopen,
            true,
        )
    } else if input.agent_mail_health.is_degraded() {
        (
            AgentLivenessClass::RecoveryNeeded,
            StaleOwnershipAction::Ping,
            true,
        )
    } else if input.pane_alive && heartbeat_fresh && output_fresh {
        (AgentLivenessClass::Alive, StaleOwnershipAction::Wait, false)
    } else if input.pane_alive && heartbeat_quiet_active {
        (
            AgentLivenessClass::QuietButActive,
            StaleOwnershipAction::Wait,
            false,
        )
    } else {
        (
            AgentLivenessClass::Unreachable,
            StaleOwnershipAction::Ping,
            false,
        )
    };

    let automatic_reopen_allowed = false;
    let evidence = build_evidence(input, owner_conflict, stale_claim);
    let operator_summary = format!(
        "{} classified as {}; next_action={}; policy_gate_required={}; automatic_reopen_allowed={}",
        input.agent_name,
        classification.as_str(),
        recommended_action.as_str(),
        policy_gate_required,
        automatic_reopen_allowed
    );

    AgentLivenessReceipt {
        receipt_id: format!("agent-liveness-{}", input.agent_name),
        agent_name: input.agent_name.clone(),
        classification,
        recommended_action,
        policy_gate_required,
        automatic_reopen_allowed,
        evidence,
        operator_summary,
    }
}

fn build_evidence(
    input: &AgentLivenessInput,
    owner_conflict: bool,
    stale_claim: bool,
) -> Vec<LivenessEvidence> {
    let mut evidence = vec![
        LivenessEvidence {
            kind: LivenessEvidenceKind::PaneHeartbeat,
            supports_classification: input.pane_alive && input.heartbeat_age_ms.is_some(),
            detail: format!(
                "pane_id={}; pane_alive={}; heartbeat_age_ms={}",
                option_u64(input.pane_id),
                input.pane_alive,
                option_u64(input.heartbeat_age_ms)
            ),
        },
        LivenessEvidence {
            kind: LivenessEvidenceKind::RecentOutput,
            supports_classification: input.recent_output_age_ms.is_some(),
            detail: format!(
                "recent_output_age_ms={}",
                option_u64(input.recent_output_age_ms)
            ),
        },
        LivenessEvidence {
            kind: LivenessEvidenceKind::WorkingDirectory,
            supports_classification: input.cwd.is_some(),
            detail: format!("cwd={}", option_str(input.cwd.as_deref())),
        },
        LivenessEvidence {
            kind: LivenessEvidenceKind::CommandIdentity,
            supports_classification: input.command_identity.is_some(),
            detail: format!("command={}", option_str(input.command_identity.as_deref())),
        },
        LivenessEvidence {
            kind: LivenessEvidenceKind::BeadsAssignment,
            supports_classification: input.beads_assignee.is_some(),
            detail: format!(
                "beads_assignee={}",
                option_str(input.beads_assignee.as_deref())
            ),
        },
        LivenessEvidence {
            kind: LivenessEvidenceKind::AgentMailHealth,
            supports_classification: !matches!(input.agent_mail_health, AgentMailHealth::Skipped),
            detail: format!("agent_mail={}", input.agent_mail_health.as_str()),
        },
        LivenessEvidence {
            kind: LivenessEvidenceKind::ClaimAge,
            supports_classification: stale_claim,
            detail: format!("claim_age_ms={}", option_u64(input.claim_age_ms)),
        },
    ];

    if owner_conflict {
        evidence.push(LivenessEvidence {
            kind: LivenessEvidenceKind::OwnerConflict,
            supports_classification: true,
            detail: format!(
                "observed_agent={} beads_assignee={}",
                input.agent_name,
                option_str(input.beads_assignee.as_deref())
            ),
        });
    }
    if !input.pane_alive {
        evidence.push(LivenessEvidence {
            kind: LivenessEvidenceKind::PaneMissing,
            supports_classification: true,
            detail: format!("pane_id={} missing_or_dead=true", option_u64(input.pane_id)),
        });
    }

    evidence
}

fn option_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_string(), |value| value.to_string())
}

fn option_str(value: Option<&str>) -> String {
    value.unwrap_or("none").to_string()
}

/// Deterministic fixture inputs for liveness receipt summaries.
#[must_use]
pub fn agent_liveness_golden_inputs() -> Vec<AgentLivenessInput> {
    vec![
        AgentLivenessInput {
            agent_name: "conflict-agent".to_string(),
            pane_id: Some(44),
            pane_alive: true,
            heartbeat_age_ms: Some(30_000),
            recent_output_age_ms: Some(40_000),
            cwd: Some("/repo".to_string()),
            command_identity: Some("codex".to_string()),
            beads_assignee: Some("other-agent".to_string()),
            claim_age_ms: Some(300_000),
            agent_mail_health: AgentMailHealth::Healthy,
        },
        AgentLivenessInput {
            agent_name: "dead-stale-agent".to_string(),
            pane_id: Some(55),
            pane_alive: false,
            heartbeat_age_ms: Some(3_600_000),
            recent_output_age_ms: Some(3_600_000),
            cwd: Some("/repo".to_string()),
            command_identity: Some("codex".to_string()),
            beads_assignee: Some("dead-stale-agent".to_string()),
            claim_age_ms: Some(3_600_000),
            agent_mail_health: AgentMailHealth::Healthy,
        },
        AgentLivenessInput {
            agent_name: "fresh-agent".to_string(),
            pane_id: Some(11),
            pane_alive: true,
            heartbeat_age_ms: Some(5_000),
            recent_output_age_ms: Some(8_000),
            cwd: Some("/repo".to_string()),
            command_identity: Some("codex".to_string()),
            beads_assignee: Some("fresh-agent".to_string()),
            claim_age_ms: Some(60_000),
            agent_mail_health: AgentMailHealth::Healthy,
        },
        AgentLivenessInput {
            agent_name: "mail-degraded-agent".to_string(),
            pane_id: Some(66),
            pane_alive: true,
            heartbeat_age_ms: Some(60_000),
            recent_output_age_ms: Some(480_000),
            cwd: Some("/repo".to_string()),
            command_identity: Some("codex".to_string()),
            beads_assignee: Some("mail-degraded-agent".to_string()),
            claim_age_ms: Some(600_000),
            agent_mail_health: AgentMailHealth::Degraded,
        },
        AgentLivenessInput {
            agent_name: "quiet-agent".to_string(),
            pane_id: Some(22),
            pane_alive: true,
            heartbeat_age_ms: Some(240_000),
            recent_output_age_ms: Some(600_000),
            cwd: Some("/repo".to_string()),
            command_identity: Some("codex".to_string()),
            beads_assignee: Some("quiet-agent".to_string()),
            claim_age_ms: Some(240_000),
            agent_mail_health: AgentMailHealth::Healthy,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden_report() -> AgentLivenessReport {
        evaluate_agent_liveness(
            &AgentLivenessPolicy::default(),
            &agent_liveness_golden_inputs(),
        )
    }

    fn receipt<'a>(report: &'a AgentLivenessReport, agent: &str) -> &'a AgentLivenessReceipt {
        report
            .receipts
            .iter()
            .find(|receipt| receipt.agent_name == agent)
            .unwrap()
    }

    #[test]
    fn liveness_fresh_agent_waits_without_policy_gate() {
        let report = golden_report();
        let receipt = receipt(&report, "fresh-agent");

        assert_eq!(receipt.classification, AgentLivenessClass::Alive);
        assert_eq!(receipt.recommended_action, StaleOwnershipAction::Wait);
        assert!(!receipt.policy_gate_required);
        assert!(!receipt.automatic_reopen_allowed);
    }

    #[test]
    fn liveness_quiet_agent_is_not_stale() {
        let report = golden_report();
        let receipt = receipt(&report, "quiet-agent");

        assert_eq!(receipt.classification, AgentLivenessClass::QuietButActive);
        assert_eq!(receipt.recommended_action, StaleOwnershipAction::Wait);
        assert!(!receipt.policy_gate_required);
    }

    #[test]
    fn liveness_dead_pane_stale_claim_requires_gated_reopen() {
        let report = golden_report();
        let receipt = receipt(&report, "dead-stale-agent");

        assert_eq!(receipt.classification, AgentLivenessClass::StaleClaim);
        assert_eq!(receipt.recommended_action, StaleOwnershipAction::Reopen);
        assert!(receipt.policy_gate_required);
        assert!(!receipt.automatic_reopen_allowed);
        assert!(
            receipt
                .evidence
                .iter()
                .any(|evidence| evidence.kind == LivenessEvidenceKind::PaneMissing)
        );
    }

    #[test]
    fn liveness_agent_mail_degraded_blocks_automatic_recovery() {
        let report = golden_report();
        let receipt = receipt(&report, "mail-degraded-agent");

        assert_eq!(receipt.classification, AgentLivenessClass::RecoveryNeeded);
        assert_eq!(receipt.recommended_action, StaleOwnershipAction::Ping);
        assert!(receipt.policy_gate_required);
        assert!(!receipt.automatic_reopen_allowed);
        assert!(
            receipt
                .evidence
                .iter()
                .any(|evidence| evidence.detail == "agent_mail=degraded")
        );
    }

    #[test]
    fn liveness_conflicting_claim_escalates_to_human() {
        let report = golden_report();
        let receipt = receipt(&report, "conflict-agent");

        assert_eq!(receipt.classification, AgentLivenessClass::ConflictingClaim);
        assert_eq!(
            receipt.recommended_action,
            StaleOwnershipAction::EscalateHuman
        );
        assert!(receipt.policy_gate_required);
        assert!(
            receipt
                .evidence
                .iter()
                .any(|evidence| evidence.kind == LivenessEvidenceKind::OwnerConflict)
        );
    }

    #[test]
    fn liveness_golden_json_and_toon_fixtures_match() {
        let expected_json: AgentLivenessSummary = serde_json::from_str(include_str!(
            "../../../fixtures/scale-lab/agent-liveness-summary.v1.json"
        ))
        .unwrap();
        let expected_toon =
            include_str!("../../../fixtures/scale-lab/agent-liveness-summary.v1.toon");
        let summary = golden_report().summary();

        assert_eq!(summary, expected_json);
        assert_eq!(summary.to_toon(), expected_toon);
    }
}
