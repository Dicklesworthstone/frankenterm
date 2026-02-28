//! Transaction observability, event taxonomy, and forensic bundle output (ft-1i2ge.8.9).
//!
//! Defines structured events for the tx plan/prepare/commit/compensate pipeline,
//! timeline reconstruction for forensics, and evidence bundles for incident triage.
//!
//! # Architecture
//!
//! ```text
//! TxExecutionLedger ──┐
//!                     ├─> build_forensic_bundle() ─> TxForensicBundle
//! TxPlan ─────────────┤                              (JSON artifact)
//!                     │
//! MissionEventLog ────┘
//!
//! RedactionPolicy ─> redact_bundle() ─> TxForensicBundle (sanitized)
//! ```
//!
//! The bundle format is deterministic: given the same inputs and redaction policy,
//! the output is identical — suitable for CI artifact comparison and replay linkage.

use crate::tx_idempotency::{
    ChainVerification, ResumeContext, ResumeRecommendation, StepOutcome, TxExecutionLedger, TxPhase,
};
use crate::tx_plan_compiler::{StepRisk, TxPlan};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Tx Event Taxonomy ───────────────────────────────────────────────────────

/// Transaction-specific event kinds for the observability pipeline.
///
/// These extend the mission event taxonomy with fine-grained tx lifecycle events.
/// Reason codes follow the `tx.<phase>.<detail>` naming convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxEventKind {
    // ── Plan phase ──
    /// A TxPlan was compiled from an AssignmentSet.
    PlanCompiled,
    /// Risk assessment completed for the compiled plan.
    RiskAssessed,

    // ── Prepare phase ──
    /// Prepare phase started — validating preconditions.
    PrepareStarted,
    /// A single precondition was validated.
    PreconditionValidated,
    /// A precondition validation failed.
    PreconditionFailed,
    /// Prepare phase completed (all preconditions passed).
    PrepareCompleted,

    // ── Commit phase ──
    /// Commit phase started — executing steps.
    CommitStarted,
    /// A single step was committed (executed successfully).
    StepCommitted,
    /// A single step failed during commit.
    StepFailed,
    /// Commit phase completed (all steps committed or failure boundary reached).
    CommitCompleted,

    // ── Compensation phase ──
    /// Compensation phase started — rolling back after failure.
    CompensationStarted,
    /// A single step was compensated (rolled back).
    StepCompensated,
    /// Compensation phase completed.
    CompensationCompleted,

    // ── Resume / recovery ──
    /// Resume context was built from a persisted ledger.
    ResumeContextBuilt,
    /// Resume execution was attempted.
    ResumeExecuted,

    // ── Observability ──
    /// A step execution was recorded in the ledger.
    ExecutionRecorded,
    /// Hash chain integrity was verified.
    ChainVerified,
    /// Forensic bundle was exported.
    BundleExported,
}

impl TxEventKind {
    /// Return the pipeline phase this event belongs to.
    #[must_use]
    pub fn phase(&self) -> TxObservabilityPhase {
        match self {
            Self::PlanCompiled | Self::RiskAssessed => TxObservabilityPhase::Plan,
            Self::PrepareStarted
            | Self::PreconditionValidated
            | Self::PreconditionFailed
            | Self::PrepareCompleted => TxObservabilityPhase::Prepare,
            Self::CommitStarted
            | Self::StepCommitted
            | Self::StepFailed
            | Self::CommitCompleted => TxObservabilityPhase::Commit,
            Self::CompensationStarted | Self::StepCompensated | Self::CompensationCompleted => {
                TxObservabilityPhase::Compensate
            }
            Self::ResumeContextBuilt | Self::ResumeExecuted => TxObservabilityPhase::Resume,
            Self::ExecutionRecorded | Self::ChainVerified | Self::BundleExported => {
                TxObservabilityPhase::Observability
            }
        }
    }
}

/// Tx observability phases (superset of TxPhase for event routing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxObservabilityPhase {
    Plan,
    Prepare,
    Commit,
    Compensate,
    Resume,
    Observability,
}

// ── Stable reason codes ─────────────────────────────────────────────────────

/// Stable, grep-friendly reason codes for tx events.
pub mod reason_codes {
    // Plan
    pub const PLAN_COMPILED: &str = "tx.plan.compiled";
    pub const PLAN_RISK_ASSESSED: &str = "tx.plan.risk_assessed";
    pub const PLAN_RISK_HIGH: &str = "tx.plan.risk_high";
    pub const PLAN_RISK_CRITICAL: &str = "tx.plan.risk_critical";

    // Prepare
    pub const PREPARE_STARTED: &str = "tx.prepare.started";
    pub const PRECONDITION_PASS: &str = "tx.prepare.precondition_pass";
    pub const PRECONDITION_FAIL: &str = "tx.prepare.precondition_fail";
    pub const PREPARE_COMPLETED: &str = "tx.prepare.completed";

    // Commit
    pub const COMMIT_STARTED: &str = "tx.commit.started";
    pub const STEP_COMMITTED: &str = "tx.commit.step_committed";
    pub const STEP_FAILED: &str = "tx.commit.step_failed";
    pub const COMMIT_COMPLETED: &str = "tx.commit.completed";
    pub const COMMIT_PARTIAL: &str = "tx.commit.partial_failure";

    // Compensate
    pub const COMPENSATE_STARTED: &str = "tx.compensate.started";
    pub const STEP_COMPENSATED: &str = "tx.compensate.step_compensated";
    pub const COMPENSATE_COMPLETED: &str = "tx.compensate.completed";

    // Resume
    pub const RESUME_CONTEXT_BUILT: &str = "tx.resume.context_built";
    pub const RESUME_CONTINUE: &str = "tx.resume.continue_checkpoint";
    pub const RESUME_RESTART: &str = "tx.resume.restart_fresh";
    pub const RESUME_ABORT: &str = "tx.resume.compensate_abort";
    pub const RESUME_ALREADY_DONE: &str = "tx.resume.already_complete";

    // Observability
    pub const EXECUTION_RECORDED: &str = "tx.observe.execution_recorded";
    pub const CHAIN_VERIFIED: &str = "tx.observe.chain_verified";
    pub const CHAIN_BROKEN: &str = "tx.observe.chain_broken";
    pub const BUNDLE_EXPORTED: &str = "tx.observe.bundle_exported";
}

// ── Tx Observability Event ──────────────────────────────────────────────────

/// A single tx observability event with timeline fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxObservabilityEvent {
    /// Monotonic sequence within this tx's event stream.
    pub sequence: u64,
    /// Event timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Event kind discriminant.
    pub kind: TxEventKind,
    /// Stable reason code (grep-friendly).
    pub reason_code: String,
    /// Pipeline phase.
    pub phase: TxObservabilityPhase,

    // ── Timeline linkage ──
    /// Transaction execution ID.
    pub execution_id: String,
    /// Plan ID (from TxPlan).
    pub plan_id: String,
    /// Deterministic plan hash.
    pub plan_hash: u64,
    /// Step ID (if step-scoped, empty for phase-level events).
    pub step_id: String,
    /// Idempotency key (if step-scoped).
    pub idem_key: String,
    /// Current tx phase at time of event.
    pub tx_phase: TxPhase,
    /// Hash chain linkage (previous record hash).
    pub chain_hash: String,

    // ── Agent identity ──
    /// Agent that triggered this event.
    pub agent_id: String,

    // ── Event-specific details ──
    /// Additional key-value details.
    pub details: HashMap<String, serde_json::Value>,
}

// ── Timeline Entry ──────────────────────────────────────────────────────────

/// A flattened timeline entry for forensic reconstruction.
///
/// Sorted by timestamp_ms, these entries reconstruct the full execution history
/// of a transaction from plan compilation through completion or abort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxTimelineEntry {
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// Phase at time of entry.
    pub phase: TxObservabilityPhase,
    /// Step ID (empty for phase-level entries).
    pub step_id: String,
    /// Event kind that produced this entry.
    pub kind: TxEventKind,
    /// Reason code.
    pub reason_code: String,
    /// Abbreviated details summary.
    pub summary: String,
    /// Agent identity.
    pub agent_id: String,
    /// Ledger ordinal (if from a StepExecutionRecord).
    pub ordinal: Option<u64>,
    /// Record hash (for chain verification).
    pub record_hash: String,
}

/// Build a timeline from a ledger and optional observability events.
pub fn build_timeline(
    ledger: &TxExecutionLedger,
    events: &[TxObservabilityEvent],
) -> Vec<TxTimelineEntry> {
    let mut entries = Vec::new();

    // Add ledger records
    for record in ledger.records() {
        let (kind, reason_code) = match &record.outcome {
            StepOutcome::Success { .. } => {
                (TxEventKind::StepCommitted, reason_codes::STEP_COMMITTED)
            }
            StepOutcome::Failed { .. } => (TxEventKind::StepFailed, reason_codes::STEP_FAILED),
            StepOutcome::Skipped { .. } => {
                (TxEventKind::StepCommitted, reason_codes::STEP_COMMITTED)
            }
            StepOutcome::Compensated { .. } => {
                (TxEventKind::StepCompensated, reason_codes::STEP_COMPENSATED)
            }
            StepOutcome::Pending => continue,
        };

        let summary = match &record.outcome {
            StepOutcome::Success { result } => {
                format!(
                    "success{}",
                    result.as_deref().map_or("", |_| " (with result)")
                )
            }
            StepOutcome::Failed {
                error_code,
                compensated,
                ..
            } => format!("failed: {} (compensated={})", error_code, compensated),
            StepOutcome::Skipped { reason } => format!("skipped: {}", reason),
            StepOutcome::Compensated { .. } => "compensated".to_string(),
            StepOutcome::Pending => unreachable!(),
        };

        entries.push(TxTimelineEntry {
            timestamp_ms: record.timestamp_ms,
            phase: kind.phase(),
            step_id: record.idem_key.step_id().to_string(),
            kind,
            reason_code: reason_code.to_string(),
            summary,
            agent_id: record.agent_id.clone(),
            ordinal: Some(record.ordinal),
            record_hash: record.hash(),
        });
    }

    // Add observability events
    for event in events {
        entries.push(TxTimelineEntry {
            timestamp_ms: event.timestamp_ms,
            phase: event.phase,
            step_id: event.step_id.clone(),
            kind: event.kind.clone(),
            reason_code: event.reason_code.clone(),
            summary: event
                .details
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            agent_id: event.agent_id.clone(),
            ordinal: None,
            record_hash: String::new(),
        });
    }

    // Sort by timestamp (stable sort preserves ordinal order for same-ms events)
    entries.sort_by_key(|e| e.timestamp_ms);
    entries
}

// ── Redaction ───────────────────────────────────────────────────────────────

/// What to redact from forensic bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionPolicy {
    /// Redact command text in step actions.
    pub redact_command_text: bool,
    /// Redact error messages (keep error codes).
    pub redact_error_messages: bool,
    /// Redact result payloads.
    pub redact_results: bool,
    /// Redact approval codes.
    pub redact_approval_codes: bool,
    /// Redact workspace/track labels.
    pub redact_labels: bool,
    /// Marker text for redacted fields.
    pub redaction_marker: String,
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            redact_command_text: true,
            redact_error_messages: false,
            redact_results: false,
            redact_approval_codes: true,
            redact_labels: false,
            redaction_marker: "[REDACTED]".to_string(),
        }
    }
}

impl RedactionPolicy {
    /// No redaction — full forensic detail.
    pub fn none() -> Self {
        Self {
            redact_command_text: false,
            redact_error_messages: false,
            redact_results: false,
            redact_approval_codes: false,
            redact_labels: false,
            redaction_marker: "[REDACTED]".to_string(),
        }
    }

    /// Maximum redaction — only structural data retained.
    pub fn maximum() -> Self {
        Self {
            redact_command_text: true,
            redact_error_messages: true,
            redact_results: true,
            redact_approval_codes: true,
            redact_labels: true,
            redaction_marker: "[REDACTED]".to_string(),
        }
    }
}

/// Redact a step outcome according to policy.
pub fn redact_outcome(outcome: &StepOutcome, policy: &RedactionPolicy) -> StepOutcome {
    match outcome {
        StepOutcome::Success { result } => StepOutcome::Success {
            result: if policy.redact_results {
                result.as_ref().map(|_| policy.redaction_marker.clone())
            } else {
                result.clone()
            },
        },
        StepOutcome::Failed {
            error_code,
            error_message,
            compensated,
        } => StepOutcome::Failed {
            error_code: error_code.clone(),
            error_message: if policy.redact_error_messages {
                policy.redaction_marker.clone()
            } else {
                error_message.clone()
            },
            compensated: *compensated,
        },
        StepOutcome::Skipped { reason } => StepOutcome::Skipped {
            reason: if policy.redact_error_messages {
                policy.redaction_marker.clone()
            } else {
                reason.clone()
            },
        },
        StepOutcome::Compensated {
            original_outcome,
            compensation_result,
        } => StepOutcome::Compensated {
            original_outcome: Box::new(redact_outcome(original_outcome, policy)),
            compensation_result: if policy.redact_results {
                policy.redaction_marker.clone()
            } else {
                compensation_result.clone()
            },
        },
        StepOutcome::Pending => StepOutcome::Pending,
    }
}

// ── Forensic Bundle ─────────────────────────────────────────────────────────

/// Bundle metadata for provenance tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMetadata {
    /// Bundle format version.
    pub version: u32,
    /// Generation timestamp in milliseconds.
    pub generated_at_ms: u64,
    /// Agent/system that generated this bundle.
    pub generator: String,
    /// Incident or request ID that triggered bundle generation.
    pub incident_id: String,
    /// Classification level.
    pub classification: BundleClassification,
    /// Workspace label (may be redacted).
    pub workspace: String,
    /// Track label (may be redacted).
    pub track: String,
}

/// Classification level for a forensic bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleClassification {
    /// Internal diagnostics only.
    Internal,
    /// Suitable for team-level incident review.
    TeamReview,
    /// Suitable for external audit (maximally redacted).
    ExternalAudit,
}

/// Plan summary snapshot for the bundle (avoids embedding full plan).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSnapshot {
    /// Plan ID.
    pub plan_id: String,
    /// Deterministic plan hash.
    pub plan_hash: u64,
    /// Number of steps.
    pub step_count: usize,
    /// Step IDs in execution order.
    pub execution_order: Vec<String>,
    /// Risk summary.
    pub high_risk_count: usize,
    pub critical_risk_count: usize,
    pub uncompensated_steps: usize,
    pub overall_risk: StepRisk,
    /// Steps with their risk levels (no action payloads — those may be sensitive).
    pub step_risks: Vec<(String, StepRisk)>,
}

impl PlanSnapshot {
    /// Build a snapshot from a full TxPlan.
    pub fn from_plan(plan: &TxPlan) -> Self {
        Self {
            plan_id: plan.plan_id.clone(),
            plan_hash: plan.plan_hash,
            step_count: plan.steps.len(),
            execution_order: plan.execution_order.clone(),
            high_risk_count: plan.risk_summary.high_risk_count,
            critical_risk_count: plan.risk_summary.critical_risk_count,
            uncompensated_steps: plan.risk_summary.uncompensated_steps,
            overall_risk: plan.risk_summary.overall_risk,
            step_risks: plan.steps.iter().map(|s| (s.id.clone(), s.risk)).collect(),
        }
    }
}

/// Ledger summary for the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    /// Execution ID.
    pub execution_id: String,
    /// Plan ID.
    pub plan_id: String,
    /// Plan hash.
    pub plan_hash: u64,
    /// Current phase.
    pub phase: TxPhase,
    /// Total records in the ledger.
    pub record_count: usize,
    /// Records with outcome summaries (redacted as needed).
    pub records: Vec<LedgerRecordSummary>,
    /// Hash chain tip.
    pub last_hash: String,
}

/// Summary of a single ledger record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecordSummary {
    pub ordinal: u64,
    pub step_id: String,
    pub idem_key: String,
    pub timestamp_ms: u64,
    pub outcome_kind: String,
    pub risk: StepRisk,
    pub agent_id: String,
    pub record_hash: String,
}

impl LedgerSnapshot {
    /// Build a snapshot from a ledger with optional redaction.
    pub fn from_ledger(ledger: &TxExecutionLedger) -> Self {
        let records = ledger
            .records()
            .iter()
            .map(|r| LedgerRecordSummary {
                ordinal: r.ordinal,
                step_id: r.idem_key.step_id().to_string(),
                idem_key: r.idem_key.as_str().to_string(),
                timestamp_ms: r.timestamp_ms,
                outcome_kind: outcome_kind_str(&r.outcome),
                risk: r.risk,
                agent_id: r.agent_id.clone(),
                record_hash: r.hash(),
            })
            .collect();

        Self {
            execution_id: ledger.execution_id().to_string(),
            plan_id: ledger.plan_id().to_string(),
            plan_hash: ledger.plan_hash(),
            phase: ledger.phase(),
            record_count: ledger.records().len(),
            records,
            last_hash: ledger.last_hash().to_string(),
        }
    }
}

/// Resume summary for the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeSummary {
    /// Execution ID.
    pub execution_id: String,
    /// Phase when interruption occurred.
    pub interrupted_phase: TxPhase,
    /// Steps completed before interruption.
    pub completed_count: usize,
    /// Steps that failed.
    pub failed_count: usize,
    /// Steps remaining.
    pub remaining_count: usize,
    /// Steps already compensated.
    pub compensated_count: usize,
    /// Whether the hash chain is intact.
    pub chain_intact: bool,
    /// Recommendation for resume.
    pub recommendation: String,
}

impl ResumeSummary {
    /// Build from a ResumeContext.
    pub fn from_context(ctx: &ResumeContext) -> Self {
        Self {
            execution_id: ctx.execution_id.clone(),
            interrupted_phase: ctx.interrupted_phase,
            completed_count: ctx.completed_steps.len(),
            failed_count: ctx.failed_steps.len(),
            remaining_count: ctx.remaining_steps.len(),
            compensated_count: ctx.compensated_steps.len(),
            chain_intact: ctx.chain_intact,
            recommendation: match ctx.recommendation {
                ResumeRecommendation::ContinueFromCheckpoint => {
                    "continue_from_checkpoint".to_string()
                }
                ResumeRecommendation::RestartFresh => "restart_fresh".to_string(),
                ResumeRecommendation::CompensateAndAbort => "compensate_and_abort".to_string(),
                ResumeRecommendation::AlreadyComplete => "already_complete".to_string(),
            },
        }
    }
}

/// Chain verification result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerificationSummary {
    pub chain_intact: bool,
    pub first_break_at: Option<u64>,
    pub missing_ordinals: Vec<u64>,
    pub total_records: usize,
}

impl From<ChainVerification> for ChainVerificationSummary {
    fn from(cv: ChainVerification) -> Self {
        Self {
            chain_intact: cv.chain_intact,
            first_break_at: cv.first_break_at,
            missing_ordinals: cv.missing_ordinals,
            total_records: cv.total_records,
        }
    }
}

/// Redaction metadata for the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionMetadata {
    /// Policy that was applied.
    pub policy: RedactionPolicy,
    /// Number of fields that were redacted.
    pub fields_redacted: usize,
    /// Categories of redacted data.
    pub categories: Vec<String>,
}

/// Complete forensic bundle for a transaction execution.
///
/// This is the primary artifact for incident triage, CI gating, and
/// replay linkage. It is deterministic: same inputs + redaction policy
/// produce the same output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxForensicBundle {
    /// Bundle metadata and provenance.
    pub metadata: BundleMetadata,
    /// Plan snapshot (structural, no sensitive payloads).
    pub plan: PlanSnapshot,
    /// Ledger snapshot with execution records.
    pub ledger: LedgerSnapshot,
    /// Hash chain verification result.
    pub chain_verification: ChainVerificationSummary,
    /// Flattened timeline for forensic reconstruction.
    pub timeline: Vec<TxTimelineEntry>,
    /// Resume context (if applicable).
    pub resume: Option<ResumeSummary>,
    /// Redaction metadata.
    pub redaction: RedactionMetadata,
}

/// Configuration for forensic bundle generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxObservabilityConfig {
    /// Maximum timeline entries to include.
    pub max_timeline_entries: usize,
    /// Maximum observability events to retain.
    pub max_events: usize,
    /// Default redaction policy.
    pub redaction_policy: RedactionPolicy,
    /// Default bundle classification.
    pub default_classification: BundleClassification,
}

impl Default for TxObservabilityConfig {
    fn default() -> Self {
        Self {
            max_timeline_entries: 1024,
            max_events: 4096,
            redaction_policy: RedactionPolicy::default(),
            default_classification: BundleClassification::TeamReview,
        }
    }
}

/// Build a forensic bundle from a plan, ledger, and optional events/resume context.
pub fn build_forensic_bundle(
    plan: &TxPlan,
    ledger: &TxExecutionLedger,
    events: &[TxObservabilityEvent],
    resume_ctx: Option<&ResumeContext>,
    generator: &str,
    incident_id: &str,
    timestamp_ms: u64,
    config: &TxObservabilityConfig,
) -> TxForensicBundle {
    let plan_snapshot = PlanSnapshot::from_plan(plan);
    let ledger_snapshot = LedgerSnapshot::from_ledger(ledger);
    let chain_verification: ChainVerificationSummary = ledger.verify_chain().into();

    let mut timeline = build_timeline(ledger, events);
    if timeline.len() > config.max_timeline_entries {
        timeline.truncate(config.max_timeline_entries);
    }

    let resume = resume_ctx.map(ResumeSummary::from_context);

    let mut categories = Vec::new();
    let mut fields_redacted = 0;
    if config.redaction_policy.redact_command_text {
        categories.push("command_text".to_string());
        fields_redacted += plan.steps.len();
    }
    if config.redaction_policy.redact_approval_codes {
        categories.push("approval_codes".to_string());
    }
    if config.redaction_policy.redact_error_messages {
        categories.push("error_messages".to_string());
    }
    if config.redaction_policy.redact_results {
        categories.push("result_payloads".to_string());
    }
    if config.redaction_policy.redact_labels {
        categories.push("workspace_labels".to_string());
    }

    TxForensicBundle {
        metadata: BundleMetadata {
            version: 1,
            generated_at_ms: timestamp_ms,
            generator: generator.to_string(),
            incident_id: incident_id.to_string(),
            classification: config.default_classification.clone(),
            workspace: if config.redaction_policy.redact_labels {
                config.redaction_policy.redaction_marker.clone()
            } else {
                String::new()
            },
            track: if config.redaction_policy.redact_labels {
                config.redaction_policy.redaction_marker.clone()
            } else {
                String::new()
            },
        },
        plan: plan_snapshot,
        ledger: ledger_snapshot,
        chain_verification,
        timeline,
        resume,
        redaction: RedactionMetadata {
            policy: config.redaction_policy.clone(),
            fields_redacted,
            categories,
        },
    }
}

/// Outcome kind as a string label.
fn outcome_kind_str(outcome: &StepOutcome) -> String {
    match outcome {
        StepOutcome::Success { .. } => "success".to_string(),
        StepOutcome::Failed { .. } => "failed".to_string(),
        StepOutcome::Skipped { .. } => "skipped".to_string(),
        StepOutcome::Compensated { .. } => "compensated".to_string(),
        StepOutcome::Pending => "pending".to_string(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_idempotency::{IdempotencyPolicy, IdempotencyStore};
    use crate::tx_plan_compiler::TxRiskSummary;

    fn make_test_plan() -> TxPlan {
        use crate::tx_plan_compiler::{CompensatingAction, CompensationKind, TxStep};
        TxPlan {
            plan_id: "plan-001".to_string(),
            plan_hash: 0xDEADBEEF,
            steps: vec![
                TxStep {
                    id: "step-1".to_string(),
                    bead_id: "bead-a".to_string(),
                    agent_id: "agent-1".to_string(),
                    description: "Execute bead-a".to_string(),
                    depends_on: Vec::new(),
                    preconditions: Vec::new(),
                    compensations: vec![CompensatingAction {
                        step_id: "step-1".to_string(),
                        description: "rollback step-1".to_string(),
                        action_type: CompensationKind::Rollback,
                    }],
                    risk: StepRisk::Low,
                    score: 0.9,
                },
                TxStep {
                    id: "step-2".to_string(),
                    bead_id: "bead-b".to_string(),
                    agent_id: "agent-2".to_string(),
                    description: "Execute bead-b".to_string(),
                    depends_on: vec!["step-1".to_string()],
                    preconditions: Vec::new(),
                    compensations: vec![CompensatingAction {
                        step_id: "step-2".to_string(),
                        description: "notify operator for step-2".to_string(),
                        action_type: CompensationKind::NotifyOperator,
                    }],
                    risk: StepRisk::Medium,
                    score: 0.7,
                },
                TxStep {
                    id: "step-3".to_string(),
                    bead_id: "bead-c".to_string(),
                    agent_id: "agent-1".to_string(),
                    description: "Execute bead-c".to_string(),
                    depends_on: vec!["step-1".to_string()],
                    preconditions: Vec::new(),
                    compensations: Vec::new(),
                    risk: StepRisk::High,
                    score: 0.5,
                },
            ],
            execution_order: vec![
                "step-1".to_string(),
                "step-2".to_string(),
                "step-3".to_string(),
            ],
            parallel_levels: vec![
                vec!["step-1".to_string()],
                vec!["step-2".to_string(), "step-3".to_string()],
            ],
            risk_summary: TxRiskSummary {
                total_steps: 3,
                high_risk_count: 1,
                critical_risk_count: 0,
                uncompensated_steps: 1,
                overall_risk: StepRisk::High,
            },
            rejected_edges: Vec::new(),
        }
    }

    fn make_test_ledger(plan: &TxPlan) -> TxExecutionLedger {
        use crate::tx_idempotency::IdempotencyKey;

        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        store.create_ledger("exec-001", plan).unwrap();
        let ledger = store.get_ledger_mut("exec-001").unwrap();

        // Transition through phases
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        // Simulate step-1 success
        let key1 = IdempotencyKey::new("plan-001", "step-1", "action-1");
        ledger
            .append(
                key1,
                StepOutcome::Success {
                    result: Some("done".to_string()),
                },
                StepRisk::Low,
                "agent-1",
                1000,
            )
            .unwrap();

        // Simulate step-2 failure
        let key2 = IdempotencyKey::new("plan-001", "step-2", "action-2");
        ledger
            .append(
                key2,
                StepOutcome::Failed {
                    error_code: "TX-E001".to_string(),
                    error_message: "connection refused to target".to_string(),
                    compensated: false,
                },
                StepRisk::Medium,
                "agent-2",
                2000,
            )
            .unwrap();

        // Transition to terminal phase for archiving
        ledger.transition_phase(TxPhase::Aborted).unwrap();

        store.archive_ledger("exec-001").unwrap()
    }

    // ── TxEventKind ──

    #[test]
    fn event_kind_phase_mapping() {
        assert_eq!(
            TxEventKind::PlanCompiled.phase(),
            TxObservabilityPhase::Plan
        );
        assert_eq!(
            TxEventKind::PrepareStarted.phase(),
            TxObservabilityPhase::Prepare
        );
        assert_eq!(
            TxEventKind::StepCommitted.phase(),
            TxObservabilityPhase::Commit
        );
        assert_eq!(
            TxEventKind::CompensationStarted.phase(),
            TxObservabilityPhase::Compensate
        );
        assert_eq!(
            TxEventKind::ResumeContextBuilt.phase(),
            TxObservabilityPhase::Resume
        );
        assert_eq!(
            TxEventKind::BundleExported.phase(),
            TxObservabilityPhase::Observability
        );
    }

    #[test]
    fn event_kind_serde_roundtrip() {
        let kinds = vec![
            TxEventKind::PlanCompiled,
            TxEventKind::StepFailed,
            TxEventKind::CompensationCompleted,
            TxEventKind::ResumeExecuted,
            TxEventKind::ChainVerified,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let restored: TxEventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, kind);
        }
    }

    // ── Redaction ──

    #[test]
    fn redact_success_result() {
        let outcome = StepOutcome::Success {
            result: Some("secret data".to_string()),
        };
        let redacted = redact_outcome(&outcome, &RedactionPolicy::default());
        // Default policy does not redact results
        if let StepOutcome::Success { result } = &redacted {
            assert_eq!(result.as_deref(), Some("secret data"));
        }

        let redacted_max = redact_outcome(&outcome, &RedactionPolicy::maximum());
        if let StepOutcome::Success { result } = &redacted_max {
            assert_eq!(result.as_deref(), Some("[REDACTED]"));
        }
    }

    #[test]
    fn redact_failure_message() {
        let outcome = StepOutcome::Failed {
            error_code: "TX-E001".to_string(),
            error_message: "secret error details".to_string(),
            compensated: false,
        };
        let redacted = redact_outcome(&outcome, &RedactionPolicy::maximum());
        if let StepOutcome::Failed {
            error_code,
            error_message,
            ..
        } = &redacted
        {
            assert_eq!(error_code, "TX-E001"); // Code preserved
            assert_eq!(error_message, "[REDACTED]"); // Message redacted
        }
    }

    #[test]
    fn redact_compensated_recursive() {
        let inner = StepOutcome::Failed {
            error_code: "TX-E002".to_string(),
            error_message: "inner secret".to_string(),
            compensated: true,
        };
        let outcome = StepOutcome::Compensated {
            original_outcome: Box::new(inner),
            compensation_result: "rollback details".to_string(),
        };
        let redacted = redact_outcome(&outcome, &RedactionPolicy::maximum());
        if let StepOutcome::Compensated {
            original_outcome,
            compensation_result,
        } = &redacted
        {
            assert_eq!(compensation_result, "[REDACTED]");
            if let StepOutcome::Failed { error_message, .. } = original_outcome.as_ref() {
                assert_eq!(error_message, "[REDACTED]");
            }
        }
    }

    #[test]
    fn redaction_policy_none_preserves_all() {
        let outcome = StepOutcome::Success {
            result: Some("data".to_string()),
        };
        let redacted = redact_outcome(&outcome, &RedactionPolicy::none());
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            serde_json::to_string(&redacted).unwrap()
        );
    }

    #[test]
    fn redact_pending_is_noop() {
        let redacted = redact_outcome(&StepOutcome::Pending, &RedactionPolicy::maximum());
        let is_pending = matches!(redacted, StepOutcome::Pending);
        assert!(is_pending);
    }

    // ── Timeline ──

    #[test]
    fn timeline_from_ledger_sorted() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);

        let timeline = build_timeline(&ledger, &[]);

        assert_eq!(timeline.len(), 2); // step-1 success + step-2 failure
        assert!(timeline[0].timestamp_ms <= timeline[1].timestamp_ms);
        assert_eq!(timeline[0].step_id, "step-1");
        assert_eq!(timeline[1].step_id, "step-2");
    }

    #[test]
    fn timeline_includes_events() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);

        let events = vec![TxObservabilityEvent {
            sequence: 1,
            timestamp_ms: 500,
            kind: TxEventKind::PrepareStarted,
            reason_code: reason_codes::PREPARE_STARTED.to_string(),
            phase: TxObservabilityPhase::Prepare,
            execution_id: "exec-001".to_string(),
            plan_id: "plan-001".to_string(),
            plan_hash: 0xDEADBEEF,
            step_id: String::new(),
            idem_key: String::new(),
            tx_phase: TxPhase::Preparing,
            chain_hash: String::new(),
            agent_id: "system".to_string(),
            details: HashMap::new(),
        }];

        let timeline = build_timeline(&ledger, &events);

        assert_eq!(timeline.len(), 3);
        // Event at 500ms should come first
        assert_eq!(timeline[0].timestamp_ms, 500);
        assert_eq!(timeline[0].kind, TxEventKind::PrepareStarted);
    }

    // ── Plan Snapshot ──

    #[test]
    fn plan_snapshot_captures_structure() {
        let plan = make_test_plan();
        let snapshot = PlanSnapshot::from_plan(&plan);

        assert_eq!(snapshot.plan_id, "plan-001");
        assert_eq!(snapshot.plan_hash, 0xDEADBEEF);
        assert_eq!(snapshot.step_count, 3);
        assert_eq!(snapshot.execution_order.len(), 3);
        assert_eq!(snapshot.high_risk_count, 1);
        assert_eq!(snapshot.uncompensated_steps, 1);
        assert_eq!(snapshot.step_risks.len(), 3);
    }

    // ── Ledger Snapshot ──

    #[test]
    fn ledger_snapshot_captures_records() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);
        let snapshot = LedgerSnapshot::from_ledger(&ledger);

        assert_eq!(snapshot.execution_id, "exec-001");
        assert_eq!(snapshot.plan_id, "plan-001");
        assert_eq!(snapshot.record_count, 2);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(snapshot.records[0].outcome_kind, "success");
        assert_eq!(snapshot.records[1].outcome_kind, "failed");
    }

    // ── Forensic Bundle ──

    #[test]
    fn build_bundle_basic() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);
        let config = TxObservabilityConfig::default();

        let bundle = build_forensic_bundle(
            &plan,
            &ledger,
            &[],
            None,
            "test-generator",
            "INC-001",
            5000,
            &config,
        );

        assert_eq!(bundle.metadata.version, 1);
        assert_eq!(bundle.metadata.generator, "test-generator");
        assert_eq!(bundle.metadata.incident_id, "INC-001");
        assert_eq!(bundle.plan.step_count, 3);
        assert_eq!(bundle.ledger.record_count, 2);
        assert!(bundle.chain_verification.chain_intact);
        assert_eq!(bundle.timeline.len(), 2);
        assert!(bundle.resume.is_none());
    }

    #[test]
    fn bundle_serde_roundtrip() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);
        let config = TxObservabilityConfig::default();

        let bundle =
            build_forensic_bundle(&plan, &ledger, &[], None, "test", "INC-002", 6000, &config);

        let json = serde_json::to_string(&bundle).unwrap();
        let restored: TxForensicBundle = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.metadata.incident_id, "INC-002");
        assert_eq!(restored.plan.plan_hash, bundle.plan.plan_hash);
        assert_eq!(restored.ledger.record_count, bundle.ledger.record_count);
        assert_eq!(
            restored.chain_verification.chain_intact,
            bundle.chain_verification.chain_intact
        );
    }

    #[test]
    fn bundle_with_redaction() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);
        let config = TxObservabilityConfig {
            redaction_policy: RedactionPolicy::maximum(),
            ..Default::default()
        };

        let bundle =
            build_forensic_bundle(&plan, &ledger, &[], None, "test", "INC-003", 7000, &config);

        assert_eq!(bundle.metadata.workspace, "[REDACTED]");
        assert_eq!(bundle.metadata.track, "[REDACTED]");
        assert!(
            bundle
                .redaction
                .categories
                .contains(&"command_text".to_string())
        );
        assert!(
            bundle
                .redaction
                .categories
                .contains(&"workspace_labels".to_string())
        );
        assert!(bundle.redaction.fields_redacted > 0);
    }

    #[test]
    fn bundle_with_resume() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);
        let resume = ResumeContext::from_ledger(&ledger, &plan);
        let config = TxObservabilityConfig::default();

        let bundle = build_forensic_bundle(
            &plan,
            &ledger,
            &[],
            Some(&resume),
            "test",
            "INC-004",
            8000,
            &config,
        );

        assert!(bundle.resume.is_some());
        let r = bundle.resume.unwrap();
        assert_eq!(r.execution_id, "exec-001");
        assert_eq!(r.completed_count, 1); // step-1 succeeded
        assert_eq!(r.failed_count, 1); // step-2 failed
    }

    #[test]
    fn timeline_truncated_by_config() {
        let plan = make_test_plan();
        let ledger = make_test_ledger(&plan);
        let config = TxObservabilityConfig {
            max_timeline_entries: 1,
            ..Default::default()
        };

        let bundle =
            build_forensic_bundle(&plan, &ledger, &[], None, "test", "INC-005", 9000, &config);

        assert_eq!(bundle.timeline.len(), 1);
    }

    // ── Reason codes ──

    #[test]
    fn reason_codes_follow_convention() {
        // All reason codes must start with "tx."
        let codes = [
            reason_codes::PLAN_COMPILED,
            reason_codes::PLAN_RISK_ASSESSED,
            reason_codes::PREPARE_STARTED,
            reason_codes::PRECONDITION_PASS,
            reason_codes::PRECONDITION_FAIL,
            reason_codes::COMMIT_STARTED,
            reason_codes::STEP_COMMITTED,
            reason_codes::STEP_FAILED,
            reason_codes::COMMIT_COMPLETED,
            reason_codes::COMPENSATE_STARTED,
            reason_codes::STEP_COMPENSATED,
            reason_codes::COMPENSATE_COMPLETED,
            reason_codes::RESUME_CONTEXT_BUILT,
            reason_codes::RESUME_CONTINUE,
            reason_codes::RESUME_RESTART,
            reason_codes::RESUME_ABORT,
            reason_codes::RESUME_ALREADY_DONE,
            reason_codes::EXECUTION_RECORDED,
            reason_codes::CHAIN_VERIFIED,
            reason_codes::CHAIN_BROKEN,
            reason_codes::BUNDLE_EXPORTED,
        ];
        for code in codes {
            assert!(
                code.starts_with("tx."),
                "Reason code must start with tx.: {}",
                code
            );
        }
    }

    // ── Config ──

    #[test]
    fn config_serde_roundtrip() {
        let config = TxObservabilityConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: TxObservabilityConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.max_timeline_entries, config.max_timeline_entries);
        assert_eq!(restored.max_events, config.max_events);
    }

    // ── Classification ──

    #[test]
    fn classification_serde() {
        let c = BundleClassification::ExternalAudit;
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"external_audit\"");
        let restored: BundleClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, c);
    }

    // ── Outcome kind string ──

    #[test]
    fn outcome_kind_str_covers_all() {
        assert_eq!(
            outcome_kind_str(&StepOutcome::Success { result: None }),
            "success"
        );
        assert_eq!(
            outcome_kind_str(&StepOutcome::Failed {
                error_code: "x".into(),
                error_message: "y".into(),
                compensated: false
            }),
            "failed"
        );
        assert_eq!(
            outcome_kind_str(&StepOutcome::Skipped { reason: "z".into() }),
            "skipped"
        );
        assert_eq!(
            outcome_kind_str(&StepOutcome::Compensated {
                original_outcome: Box::new(StepOutcome::Pending),
                compensation_result: "ok".into()
            }),
            "compensated"
        );
        assert_eq!(outcome_kind_str(&StepOutcome::Pending), "pending");
    }
}
