//! Deterministic policy recommendation dry-run receipt model.
//!
//! This module is a replay/proof surface for Robot/MCP recommendation output.
//! It never executes candidate actions, consumes rate limits, calls live
//! approval stores, or issues allow-once approval tokens.

use serde::{Deserialize, Serialize};

/// Schema version for policy recommendation receipt reports.
pub const POLICY_RECOMMENDATION_SCHEMA_VERSION: u32 = 1;

/// Dry-run recommendation outcome for a candidate action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRecommendationOutcome {
    /// Candidate looks safe in recommendation mode.
    Allow,
    /// Candidate should not proceed.
    Deny,
    /// Candidate needs explicit approval before any mutating commit.
    RequireApproval,
    /// Candidate should wait for resource pressure to clear.
    Delay,
    /// Infrastructure degradation prevents a trustworthy recommendation.
    Degrade,
    /// Human judgement is required before proceeding.
    AskHuman,
}

impl PolicyRecommendationOutcome {
    /// Stable key for reports and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::RequireApproval => "require_approval",
            Self::Delay => "delay",
            Self::Degrade => "degrade",
            Self::AskHuman => "ask_human",
        }
    }
}

/// Stable reason code for a recommendation receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRecommendationReasonCode {
    /// Read-only action with safe command context.
    SafeRead,
    /// Destructive command or action is denied.
    DestructiveCommandDenied,
    /// Approval is required before a mutating commit.
    ApprovalRequired,
    /// Recommendation is delayed because resource pressure is high.
    ResourcePressure,
    /// Stale ownership means a human should verify before proceeding.
    StaleOwnership,
    /// Agent Mail is degraded or unreachable.
    AgentMailDegraded,
    /// RCH proof infrastructure is degraded or unreachable.
    RchDegraded,
    /// Sensitive evidence was redacted before output.
    EvidenceRedacted,
    /// Recommendation mode is dry-run only.
    DryRunOnlyNoExecution,
    /// Approval metadata is a preview, not a live token.
    ApprovalPreviewNoToken,
    /// Human judgement is required.
    HumanJudgmentRequired,
}

impl PolicyRecommendationReasonCode {
    /// Stable key for reports and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SafeRead => "safe_read",
            Self::DestructiveCommandDenied => "destructive_command_denied",
            Self::ApprovalRequired => "approval_required",
            Self::ResourcePressure => "resource_pressure",
            Self::StaleOwnership => "stale_ownership",
            Self::AgentMailDegraded => "agent_mail_degraded",
            Self::RchDegraded => "rch_degraded",
            Self::EvidenceRedacted => "evidence_redacted",
            Self::DryRunOnlyNoExecution => "dry_run_only_no_execution",
            Self::ApprovalPreviewNoToken => "approval_preview_no_token",
            Self::HumanJudgmentRequired => "human_judgment_required",
        }
    }
}

/// Policy action kind mirrored into the dry-run proof surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRecommendationActionKind {
    /// Read pane output.
    ReadOutput,
    /// Search pane output.
    SearchOutput,
    /// Send text to a pane.
    SendText,
    /// Start a workflow.
    WorkflowRun,
    /// Reserve a pane.
    ReservePane,
    /// Delete a file.
    DeleteFile,
    /// Execute an external command.
    ExecCommand,
}

impl PolicyRecommendationActionKind {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOutput => "read_output",
            Self::SearchOutput => "search_output",
            Self::SendText => "send_text",
            Self::WorkflowRun => "workflow_run",
            Self::ReservePane => "reserve_pane",
            Self::DeleteFile => "delete_file",
            Self::ExecCommand => "exec_command",
        }
    }

    /// Whether the action is read-only.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOutput | Self::SearchOutput)
    }

    /// Whether the action is destructive enough to deny in recommendation mode.
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::DeleteFile | Self::ExecCommand)
    }
}

/// Actor requesting a dry-run recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRecommendationActor {
    /// Human CLI caller.
    Human,
    /// Robot Mode caller.
    Robot,
    /// MCP tool caller.
    Mcp,
    /// Automated workflow caller.
    Workflow,
}

impl PolicyRecommendationActor {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Robot => "robot",
            Self::Mcp => "mcp",
            Self::Workflow => "workflow",
        }
    }
}

/// Surface where the candidate action originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRecommendationSurface {
    /// Robot command/API surface.
    Robot,
    /// MCP tool-serving surface.
    Mcp,
    /// Workflow runtime surface.
    Workflow,
    /// Swarm orchestration surface.
    Swarm,
}

impl PolicyRecommendationSurface {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Robot => "robot",
            Self::Mcp => "mcp",
            Self::Workflow => "workflow",
            Self::Swarm => "swarm",
        }
    }
}

/// Caller-provided command safety classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSafety {
    /// Command context is read-only or otherwise safe.
    Safe,
    /// Command may be safe, but requires approval before mutation.
    NeedsApproval,
    /// Command is destructive.
    Destructive,
    /// Command safety is unknown.
    Unknown,
}

impl CommandSafety {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::NeedsApproval => "needs_approval",
            Self::Destructive => "destructive",
            Self::Unknown => "unknown",
        }
    }
}

/// Coarse resource pressure signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationResourcePressure {
    /// Resource pressure is nominal.
    Nominal,
    /// Resource queues or budgets are under backpressure.
    Backpressure,
    /// Resource queues or budgets are saturated.
    Saturated,
}

impl RecommendationResourcePressure {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Backpressure => "backpressure",
            Self::Saturated => "saturated",
        }
    }

    /// Whether pressure should block immediate recommendation.
    #[must_use]
    pub const fn is_pressured(self) -> bool {
        matches!(self, Self::Backpressure | Self::Saturated)
    }
}

/// Infrastructure health signal for recommendation trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationInfraHealth {
    /// Infrastructure is healthy.
    Healthy,
    /// Infrastructure is degraded but partially responsive.
    Degraded,
    /// Infrastructure is unreachable.
    Unreachable,
    /// Infrastructure was deliberately skipped.
    Skipped,
}

impl RecommendationInfraHealth {
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

    /// Whether health should degrade the recommendation.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded | Self::Unreachable)
    }
}

/// Evidence field used by a recommendation receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRecommendationEvidenceKind {
    /// Candidate action signal.
    Action,
    /// Actor/surface signal.
    Actor,
    /// Command safety signal.
    CommandSafety,
    /// Resource pressure signal.
    ResourcePressure,
    /// Stale ownership signal.
    StaleOwnership,
    /// Agent Mail health signal.
    AgentMailHealth,
    /// RCH health signal.
    RchHealth,
    /// Redaction signal.
    Redaction,
    /// Approval preview signal.
    ApprovalPreview,
}

/// Caller-provided evidence before output redaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationEvidenceInput {
    /// Evidence kind.
    pub kind: PolicyRecommendationEvidenceKind,
    /// Stable field name.
    pub field: String,
    /// Detail string before output redaction.
    pub detail: String,
    /// Whether detail must be redacted before output.
    pub sensitive: bool,
}

/// Redacted evidence item emitted in a recommendation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationEvidence {
    /// Evidence kind.
    pub kind: PolicyRecommendationEvidenceKind,
    /// Stable field name.
    pub field: String,
    /// Whether this evidence supports the outcome.
    pub supports_outcome: bool,
    /// Whether the detail was redacted.
    pub redacted: bool,
    /// Detail string safe for Robot/MCP output.
    pub detail: String,
}

/// Non-mutating approval preview metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPreview {
    /// Stable preview id.
    pub preview_id: String,
    /// Scope the eventual approval would need to bind.
    pub scope: String,
    /// Approval class previewed by this receipt.
    pub approval_kind: String,
    /// Whether a live approval token was issued.
    pub token_issued: bool,
    /// Whether a human-enterable approval code was issued.
    pub approval_code_issued: bool,
    /// Expiry timestamp for a live token; absent in recommendation mode.
    pub expires_at_ms: Option<u64>,
}

/// Thresholds and toggles for deterministic recommendation evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationPolicy {
    /// Deny destructive actions in recommendation mode.
    pub deny_destructive_commands: bool,
    /// Delay recommendations while resource pressure is high.
    pub delay_on_resource_pressure: bool,
    /// Degrade recommendations when supporting infrastructure is degraded.
    pub degrade_on_infra_health: bool,
    /// Ask a human when ownership appears stale.
    pub require_human_for_stale_ownership: bool,
}

impl Default for PolicyRecommendationPolicy {
    fn default() -> Self {
        Self {
            deny_destructive_commands: true,
            delay_on_resource_pressure: true,
            degrade_on_infra_health: true,
            require_human_for_stale_ownership: true,
        }
    }
}

/// Deterministic input for one policy recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationInput {
    /// Stable candidate id.
    pub candidate_id: String,
    /// Candidate action.
    pub action: PolicyRecommendationActionKind,
    /// Requesting actor.
    pub actor: PolicyRecommendationActor,
    /// Origin surface.
    pub surface: PolicyRecommendationSurface,
    /// Command safety classification.
    pub command_safety: CommandSafety,
    /// Resource pressure signal.
    pub resource_pressure: RecommendationResourcePressure,
    /// Whether ownership evidence is stale.
    pub stale_ownership: bool,
    /// Agent Mail health signal.
    pub agent_mail_health: RecommendationInfraHealth,
    /// RCH health signal.
    pub rch_health: RecommendationInfraHealth,
    /// Evidence fields to include after redaction.
    pub evidence: Vec<PolicyRecommendationEvidenceInput>,
}

/// One typed policy recommendation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationReceipt {
    /// Stable receipt id.
    pub receipt_id: String,
    /// Candidate id.
    pub candidate_id: String,
    /// Recommendation outcome.
    pub outcome: PolicyRecommendationOutcome,
    /// Stable reason codes.
    pub reason_codes: Vec<PolicyRecommendationReasonCode>,
    /// Redacted evidence trail.
    pub evidence: Vec<PolicyRecommendationEvidence>,
    /// Approval-token preview metadata, when approval would be needed.
    pub approval_preview: Option<ApprovalPreview>,
    /// Whether a separate policy/approval step is required before execution.
    pub policy_gate_required: bool,
    /// Whether recommendation mode executed the candidate action.
    pub executes_action: bool,
    /// Whether recommendation mode issued a live approval token.
    pub issues_approval_token: bool,
    /// Number of evidence fields redacted from output.
    pub redacted_evidence_fields: usize,
    /// Concise operator-facing summary.
    pub operator_summary: String,
}

/// Compact report summary suitable for checked fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationSummary {
    /// Report schema version.
    pub schema_version: u32,
    /// Total candidate actions.
    pub total_candidates: usize,
    /// Allow recommendation count.
    pub allow: usize,
    /// Deny recommendation count.
    pub deny: usize,
    /// Require-approval recommendation count.
    pub require_approval: usize,
    /// Delay recommendation count.
    pub delay: usize,
    /// Degrade recommendation count.
    pub degrade: usize,
    /// Ask-human recommendation count.
    pub ask_human: usize,
    /// Receipts requiring another policy/approval step before execution.
    pub policy_gated_receipts: usize,
    /// Approval previews emitted.
    pub approval_previews: usize,
    /// Live approval tokens issued by recommendation mode.
    pub issued_approval_tokens: usize,
    /// Candidate actions executed by recommendation mode.
    pub executed_actions: usize,
    /// Evidence fields redacted from Robot/MCP output.
    pub redacted_evidence_fields: usize,
    /// Concise operator summary.
    pub operator_summary: String,
}

impl PolicyRecommendationSummary {
    /// Render a stable compact TOON-like fixture.
    #[must_use]
    pub fn to_toon(&self) -> String {
        format!(
            "schema_version: {}\ntotal_candidates: {}\noutcomes: allow={} deny={} require_approval={} delay={} degrade={} ask_human={}\npolicy_gated_receipts: {}\napproval_previews: {}\nissued_approval_tokens: {}\nexecuted_actions: {}\nredacted_evidence_fields: {}\noperator_summary: {}\n",
            self.schema_version,
            self.total_candidates,
            self.allow,
            self.deny,
            self.require_approval,
            self.delay,
            self.degrade,
            self.ask_human,
            self.policy_gated_receipts,
            self.approval_previews,
            self.issued_approval_tokens,
            self.executed_actions,
            self.redacted_evidence_fields,
            self.operator_summary
        )
    }
}

/// Full deterministic policy recommendation report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecommendationReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Receipts sorted by candidate id.
    pub receipts: Vec<PolicyRecommendationReceipt>,
}

impl PolicyRecommendationReport {
    /// Build a compact summary from receipts.
    #[must_use]
    pub fn summary(&self) -> PolicyRecommendationSummary {
        let mut summary = PolicyRecommendationSummary {
            schema_version: self.schema_version,
            total_candidates: self.receipts.len(),
            allow: 0,
            deny: 0,
            require_approval: 0,
            delay: 0,
            degrade: 0,
            ask_human: 0,
            policy_gated_receipts: 0,
            approval_previews: 0,
            issued_approval_tokens: 0,
            executed_actions: 0,
            redacted_evidence_fields: 0,
            operator_summary: String::new(),
        };

        for receipt in &self.receipts {
            match receipt.outcome {
                PolicyRecommendationOutcome::Allow => summary.allow += 1,
                PolicyRecommendationOutcome::Deny => summary.deny += 1,
                PolicyRecommendationOutcome::RequireApproval => summary.require_approval += 1,
                PolicyRecommendationOutcome::Delay => summary.delay += 1,
                PolicyRecommendationOutcome::Degrade => summary.degrade += 1,
                PolicyRecommendationOutcome::AskHuman => summary.ask_human += 1,
            }
            if receipt.policy_gate_required {
                summary.policy_gated_receipts += 1;
            }
            if receipt.approval_preview.is_some() {
                summary.approval_previews += 1;
            }
            if receipt.issues_approval_token {
                summary.issued_approval_tokens += 1;
            }
            if receipt.executes_action {
                summary.executed_actions += 1;
            }
            summary.redacted_evidence_fields += receipt.redacted_evidence_fields;
        }

        summary.operator_summary = format!(
            "policy_recommendations: allow={} deny={} require_approval={} delay={} degrade={} ask_human={}; policy_gated={}; previews={}; tokens_issued={}; executed={}; redacted_fields={}",
            summary.allow,
            summary.deny,
            summary.require_approval,
            summary.delay,
            summary.degrade,
            summary.ask_human,
            summary.policy_gated_receipts,
            summary.approval_previews,
            summary.issued_approval_tokens,
            summary.executed_actions,
            summary.redacted_evidence_fields
        );
        summary
    }
}

/// Evaluate policy recommendation receipts deterministically.
#[must_use]
pub fn evaluate_policy_recommendations(
    policy: &PolicyRecommendationPolicy,
    inputs: &[PolicyRecommendationInput],
) -> PolicyRecommendationReport {
    let mut sorted = inputs.to_vec();
    sorted.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));

    let receipts = sorted
        .iter()
        .map(|input| classify_policy_recommendation(policy, input))
        .collect();

    PolicyRecommendationReport {
        schema_version: POLICY_RECOMMENDATION_SCHEMA_VERSION,
        receipts,
    }
}

fn classify_policy_recommendation(
    policy: &PolicyRecommendationPolicy,
    input: &PolicyRecommendationInput,
) -> PolicyRecommendationReceipt {
    let evidence = build_recommendation_evidence(input);
    let redacted_evidence_fields = evidence.iter().filter(|item| item.redacted).count();
    let mut reason_codes = vec![PolicyRecommendationReasonCode::DryRunOnlyNoExecution];

    if redacted_evidence_fields > 0 {
        reason_codes.push(PolicyRecommendationReasonCode::EvidenceRedacted);
    }

    let outcome = if policy.deny_destructive_commands
        && (input.action.is_destructive() || input.command_safety == CommandSafety::Destructive)
    {
        reason_codes.push(PolicyRecommendationReasonCode::DestructiveCommandDenied);
        PolicyRecommendationOutcome::Deny
    } else if policy.require_human_for_stale_ownership && input.stale_ownership {
        reason_codes.push(PolicyRecommendationReasonCode::StaleOwnership);
        reason_codes.push(PolicyRecommendationReasonCode::HumanJudgmentRequired);
        PolicyRecommendationOutcome::AskHuman
    } else if policy.delay_on_resource_pressure && input.resource_pressure.is_pressured() {
        reason_codes.push(PolicyRecommendationReasonCode::ResourcePressure);
        PolicyRecommendationOutcome::Delay
    } else if policy.degrade_on_infra_health
        && (input.agent_mail_health.is_degraded() || input.rch_health.is_degraded())
    {
        if input.agent_mail_health.is_degraded() {
            reason_codes.push(PolicyRecommendationReasonCode::AgentMailDegraded);
        }
        if input.rch_health.is_degraded() {
            reason_codes.push(PolicyRecommendationReasonCode::RchDegraded);
        }
        PolicyRecommendationOutcome::Degrade
    } else if input.command_safety == CommandSafety::NeedsApproval
        || (!input.action.is_read_only() && input.command_safety != CommandSafety::Safe)
    {
        reason_codes.push(PolicyRecommendationReasonCode::ApprovalRequired);
        reason_codes.push(PolicyRecommendationReasonCode::ApprovalPreviewNoToken);
        PolicyRecommendationOutcome::RequireApproval
    } else if input.command_safety == CommandSafety::Unknown {
        reason_codes.push(PolicyRecommendationReasonCode::HumanJudgmentRequired);
        PolicyRecommendationOutcome::AskHuman
    } else {
        if input.action.is_read_only() {
            reason_codes.push(PolicyRecommendationReasonCode::SafeRead);
        }
        PolicyRecommendationOutcome::Allow
    };

    let approval_preview = if outcome == PolicyRecommendationOutcome::RequireApproval {
        Some(build_approval_preview(input))
    } else {
        None
    };
    let policy_gate_required = matches!(
        outcome,
        PolicyRecommendationOutcome::RequireApproval
            | PolicyRecommendationOutcome::Delay
            | PolicyRecommendationOutcome::Degrade
            | PolicyRecommendationOutcome::AskHuman
    );
    let executes_action = false;
    let issues_approval_token = false;
    let operator_summary = format!(
        "{} outcome={}; action={}; reasons={}; executes_action={}; issues_approval_token={}",
        input.candidate_id,
        outcome.as_str(),
        input.action.as_str(),
        reason_codes
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>()
            .join(","),
        executes_action,
        issues_approval_token
    );

    PolicyRecommendationReceipt {
        receipt_id: format!("policy-recommendation-{}", input.candidate_id),
        candidate_id: input.candidate_id.clone(),
        outcome,
        reason_codes,
        evidence,
        approval_preview,
        policy_gate_required,
        executes_action,
        issues_approval_token,
        redacted_evidence_fields,
        operator_summary,
    }
}

fn build_approval_preview(input: &PolicyRecommendationInput) -> ApprovalPreview {
    ApprovalPreview {
        preview_id: format!("approval-preview-{}", input.candidate_id),
        scope: format!(
            "{}:{}:{}",
            input.actor.as_str(),
            input.surface.as_str(),
            input.action.as_str()
        ),
        approval_kind: "allow_once".to_string(),
        token_issued: false,
        approval_code_issued: false,
        expires_at_ms: None,
    }
}

fn build_recommendation_evidence(
    input: &PolicyRecommendationInput,
) -> Vec<PolicyRecommendationEvidence> {
    let mut evidence = vec![
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::Action,
            field: "action".to_string(),
            supports_outcome: true,
            redacted: false,
            detail: input.action.as_str().to_string(),
        },
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::Actor,
            field: "actor_surface".to_string(),
            supports_outcome: true,
            redacted: false,
            detail: format!("{}:{}", input.actor.as_str(), input.surface.as_str()),
        },
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::CommandSafety,
            field: "command_safety".to_string(),
            supports_outcome: true,
            redacted: false,
            detail: input.command_safety.as_str().to_string(),
        },
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::ResourcePressure,
            field: "resource_pressure".to_string(),
            supports_outcome: input.resource_pressure.is_pressured(),
            redacted: false,
            detail: input.resource_pressure.as_str().to_string(),
        },
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::StaleOwnership,
            field: "stale_ownership".to_string(),
            supports_outcome: input.stale_ownership,
            redacted: false,
            detail: input.stale_ownership.to_string(),
        },
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::AgentMailHealth,
            field: "agent_mail_health".to_string(),
            supports_outcome: input.agent_mail_health.is_degraded(),
            redacted: false,
            detail: input.agent_mail_health.as_str().to_string(),
        },
        PolicyRecommendationEvidence {
            kind: PolicyRecommendationEvidenceKind::RchHealth,
            field: "rch_health".to_string(),
            supports_outcome: input.rch_health.is_degraded(),
            redacted: false,
            detail: input.rch_health.as_str().to_string(),
        },
    ];

    evidence.extend(input.evidence.iter().map(|item| {
        let detail = if item.sensitive {
            format!("redacted:{}", item.field)
        } else {
            item.detail.clone()
        };
        PolicyRecommendationEvidence {
            kind: item.kind,
            field: item.field.clone(),
            supports_outcome: true,
            redacted: item.sensitive,
            detail,
        }
    }));

    evidence
}

/// Deterministic fixture inputs for policy recommendation summaries.
#[must_use]
pub fn policy_recommendation_golden_inputs() -> Vec<PolicyRecommendationInput> {
    vec![
        PolicyRecommendationInput {
            candidate_id: "approval-send-text".to_string(),
            action: PolicyRecommendationActionKind::SendText,
            actor: PolicyRecommendationActor::Mcp,
            surface: PolicyRecommendationSurface::Mcp,
            command_safety: CommandSafety::NeedsApproval,
            resource_pressure: RecommendationResourcePressure::Nominal,
            stale_ownership: false,
            agent_mail_health: RecommendationInfraHealth::Healthy,
            rch_health: RecommendationInfraHealth::Healthy,
            evidence: vec![PolicyRecommendationEvidenceInput {
                kind: PolicyRecommendationEvidenceKind::Redaction,
                field: "pane_excerpt".to_string(),
                detail: "secret transcript token".to_string(),
                sensitive: true,
            }],
        },
        PolicyRecommendationInput {
            candidate_id: "ask-human-stale-owner".to_string(),
            action: PolicyRecommendationActionKind::ReservePane,
            actor: PolicyRecommendationActor::Robot,
            surface: PolicyRecommendationSurface::Swarm,
            command_safety: CommandSafety::NeedsApproval,
            resource_pressure: RecommendationResourcePressure::Nominal,
            stale_ownership: true,
            agent_mail_health: RecommendationInfraHealth::Healthy,
            rch_health: RecommendationInfraHealth::Healthy,
            evidence: vec![PolicyRecommendationEvidenceInput {
                kind: PolicyRecommendationEvidenceKind::StaleOwnership,
                field: "beads_owner_age".to_string(),
                detail: "claim_age_ms=5400000".to_string(),
                sensitive: false,
            }],
        },
        PolicyRecommendationInput {
            candidate_id: "degrade-infra-policy".to_string(),
            action: PolicyRecommendationActionKind::SearchOutput,
            actor: PolicyRecommendationActor::Robot,
            surface: PolicyRecommendationSurface::Robot,
            command_safety: CommandSafety::Safe,
            resource_pressure: RecommendationResourcePressure::Nominal,
            stale_ownership: false,
            agent_mail_health: RecommendationInfraHealth::Degraded,
            rch_health: RecommendationInfraHealth::Unreachable,
            evidence: vec![PolicyRecommendationEvidenceInput {
                kind: PolicyRecommendationEvidenceKind::AgentMailHealth,
                field: "agent_mail_status".to_string(),
                detail: "degraded_read_only".to_string(),
                sensitive: false,
            }],
        },
        PolicyRecommendationInput {
            candidate_id: "delay-workflow-pressure".to_string(),
            action: PolicyRecommendationActionKind::WorkflowRun,
            actor: PolicyRecommendationActor::Workflow,
            surface: PolicyRecommendationSurface::Workflow,
            command_safety: CommandSafety::NeedsApproval,
            resource_pressure: RecommendationResourcePressure::Saturated,
            stale_ownership: false,
            agent_mail_health: RecommendationInfraHealth::Healthy,
            rch_health: RecommendationInfraHealth::Healthy,
            evidence: vec![PolicyRecommendationEvidenceInput {
                kind: PolicyRecommendationEvidenceKind::ResourcePressure,
                field: "memory_tier".to_string(),
                detail: "hot_queue_saturated=true".to_string(),
                sensitive: false,
            }],
        },
        PolicyRecommendationInput {
            candidate_id: "deny-delete-file".to_string(),
            action: PolicyRecommendationActionKind::DeleteFile,
            actor: PolicyRecommendationActor::Robot,
            surface: PolicyRecommendationSurface::Robot,
            command_safety: CommandSafety::Destructive,
            resource_pressure: RecommendationResourcePressure::Nominal,
            stale_ownership: false,
            agent_mail_health: RecommendationInfraHealth::Healthy,
            rch_health: RecommendationInfraHealth::Healthy,
            evidence: vec![PolicyRecommendationEvidenceInput {
                kind: PolicyRecommendationEvidenceKind::CommandSafety,
                field: "command_text".to_string(),
                detail: "rm -rf /tmp/prod-secret".to_string(),
                sensitive: true,
            }],
        },
        PolicyRecommendationInput {
            candidate_id: "safe-read-output".to_string(),
            action: PolicyRecommendationActionKind::ReadOutput,
            actor: PolicyRecommendationActor::Robot,
            surface: PolicyRecommendationSurface::Robot,
            command_safety: CommandSafety::Safe,
            resource_pressure: RecommendationResourcePressure::Nominal,
            stale_ownership: false,
            agent_mail_health: RecommendationInfraHealth::Healthy,
            rch_health: RecommendationInfraHealth::Healthy,
            evidence: vec![PolicyRecommendationEvidenceInput {
                kind: PolicyRecommendationEvidenceKind::Action,
                field: "pane_id".to_string(),
                detail: "pane_id=12".to_string(),
                sensitive: false,
            }],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden_report() -> PolicyRecommendationReport {
        evaluate_policy_recommendations(
            &PolicyRecommendationPolicy::default(),
            &policy_recommendation_golden_inputs(),
        )
    }

    fn receipt<'a>(
        report: &'a PolicyRecommendationReport,
        candidate: &str,
    ) -> &'a PolicyRecommendationReceipt {
        report
            .receipts
            .iter()
            .find(|receipt| receipt.candidate_id == candidate)
            .unwrap()
    }

    #[test]
    fn policy_recommendation_safe_read_allows_without_side_effects() {
        let report = golden_report();
        let receipt = receipt(&report, "safe-read-output");

        assert_eq!(receipt.outcome, PolicyRecommendationOutcome::Allow);
        assert!(
            receipt
                .reason_codes
                .contains(&PolicyRecommendationReasonCode::SafeRead)
        );
        assert!(!receipt.policy_gate_required);
        assert!(!receipt.executes_action);
        assert!(!receipt.issues_approval_token);
        assert!(receipt.approval_preview.is_none());
    }

    #[test]
    fn policy_recommendation_destructive_command_is_denied() {
        let report = golden_report();
        let receipt = receipt(&report, "deny-delete-file");

        assert_eq!(receipt.outcome, PolicyRecommendationOutcome::Deny);
        assert!(
            receipt
                .reason_codes
                .contains(&PolicyRecommendationReasonCode::DestructiveCommandDenied)
        );
        assert!(!receipt.executes_action);
        assert!(!receipt.issues_approval_token);
    }

    #[test]
    fn policy_recommendation_approval_required_gets_preview_not_token() {
        let report = golden_report();
        let receipt = receipt(&report, "approval-send-text");
        let preview = receipt.approval_preview.as_ref().unwrap();

        assert_eq!(
            receipt.outcome,
            PolicyRecommendationOutcome::RequireApproval
        );
        assert!(receipt.policy_gate_required);
        assert_eq!(preview.scope, "mcp:mcp:send_text");
        assert!(!preview.token_issued);
        assert!(!preview.approval_code_issued);
        assert_eq!(preview.expires_at_ms, None);
        assert!(!receipt.executes_action);
        assert!(!receipt.issues_approval_token);
    }

    #[test]
    fn policy_recommendation_resource_pressure_delays() {
        let report = golden_report();
        let receipt = receipt(&report, "delay-workflow-pressure");

        assert_eq!(receipt.outcome, PolicyRecommendationOutcome::Delay);
        assert!(
            receipt
                .reason_codes
                .contains(&PolicyRecommendationReasonCode::ResourcePressure)
        );
        assert!(receipt.policy_gate_required);
        assert!(!receipt.executes_action);
    }

    #[test]
    fn policy_recommendation_degraded_infrastructure_degrades() {
        let report = golden_report();
        let receipt = receipt(&report, "degrade-infra-policy");

        assert_eq!(receipt.outcome, PolicyRecommendationOutcome::Degrade);
        assert!(
            receipt
                .reason_codes
                .contains(&PolicyRecommendationReasonCode::AgentMailDegraded)
        );
        assert!(
            receipt
                .reason_codes
                .contains(&PolicyRecommendationReasonCode::RchDegraded)
        );
        assert!(receipt.policy_gate_required);
        assert!(!receipt.executes_action);
    }

    #[test]
    fn policy_recommendation_stale_ownership_asks_human() {
        let report = golden_report();
        let receipt = receipt(&report, "ask-human-stale-owner");

        assert_eq!(receipt.outcome, PolicyRecommendationOutcome::AskHuman);
        assert!(
            receipt
                .reason_codes
                .contains(&PolicyRecommendationReasonCode::StaleOwnership)
        );
        assert!(receipt.policy_gate_required);
        assert!(!receipt.executes_action);
    }

    #[test]
    fn policy_recommendation_redacts_sensitive_evidence() {
        let report = golden_report();
        let denied = receipt(&report, "deny-delete-file");
        let approval = receipt(&report, "approval-send-text");

        assert_eq!(denied.redacted_evidence_fields, 1);
        assert_eq!(approval.redacted_evidence_fields, 1);
        assert!(
            denied
                .evidence
                .iter()
                .any(|item| item.redacted && item.detail == "redacted:command_text")
        );
        assert!(
            approval
                .evidence
                .iter()
                .any(|item| item.redacted && item.detail == "redacted:pane_excerpt")
        );
        assert!(
            !denied
                .evidence
                .iter()
                .any(|item| item.detail.contains("prod-secret"))
        );
        assert!(
            !approval
                .evidence
                .iter()
                .any(|item| item.detail.contains("secret transcript"))
        );
    }

    #[test]
    fn policy_recommendation_golden_json_and_toon_fixtures_match() {
        let expected_json: PolicyRecommendationSummary = serde_json::from_str(include_str!(
            "../../../fixtures/scale-lab/policy-recommendation-summary.v1.json"
        ))
        .unwrap();
        let expected_toon =
            include_str!("../../../fixtures/scale-lab/policy-recommendation-summary.v1.toon");
        let summary = golden_report().summary();

        assert_eq!(summary, expected_json);
        assert_eq!(summary.to_toon(), expected_toon);
    }
}
