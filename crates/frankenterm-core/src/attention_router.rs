//! Read-only attention-router source snapshot adapters.
//!
//! This module intentionally does not execute subprocesses, inspect live panes,
//! call coordination services, run proof commands, or mutate project state. It
//! normalizes bounded, already-redacted caller observations into the source
//! snapshot substrate for the `ft.attention_router.v1` contract.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

pub const ATTENTION_ROUTER_CONTRACT_ID: &str = "ft.attention_router.v1";
pub const ATTENTION_ROUTER_ITEM_SCHEMA: &str = "ft.attention_router.item.v1";
pub const ATTENTION_ROUTER_NUDGE_PLAN_RECEIPT_SCHEMA: &str =
    "ft.attention_router.nudge_plan_receipt.v1";
pub const ATTENTION_ROUTER_NUDGE_PLAN_RECEIPTS_CONTRACT_ID: &str =
    "ft.attention_router.nudge_plan_receipts.v1";
pub const ATTENTION_ROUTER_SNAPSHOT_SCHEMA: &str = "ft.attention_router.snapshot.v1";
pub const ATTENTION_ROUTER_SURFACE_SCHEMA: &str = "ft.attention_router.surface.v1";
pub const ATTENTION_ROUTER_MCP_CURRENT_URI: &str = "wa://attention-router/current";
pub const ATTENTION_ROUTER_MCP_ITEM_URI_TEMPLATE: &str = "wa://attention-router/items/{item_id}";
pub const ATTENTION_ROUTER_SOURCE_SCHEMA_VERSION: u16 = 1;
pub const ATTENTION_ROUTER_SUMMARY_MAX_CHARS: usize = 512;
const ATTENTION_ROUTER_NUDGE_GLOBAL_FORBIDDEN_ACTIONS: &[&str] = &[
    "agent_mail_repair",
    "agent_mail_restart",
    "rch_service_restart",
    "worker_mutation",
    "delete_files",
    "delete_targets",
    "stash_or_revert_unrelated_dirty_work",
    "edit_overlapping_dirty_paths",
    "local_cargo_proof",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSourceKind {
    Beads,
    AgentMail,
    Git,
    Rch,
    PaneState,
    OperatingEnvelope,
    Manual,
    Fixture,
}

impl AttentionRouterSourceKind {
    fn slug(self) -> &'static str {
        match self {
            Self::Beads => "beads",
            Self::AgentMail => "agent_mail",
            Self::Git => "git",
            Self::Rch => "rch",
            Self::PaneState => "pane_state",
            Self::OperatingEnvelope => "operating_envelope",
            Self::Manual => "manual",
            Self::Fixture => "fixture",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSourceHealth {
    Available,
    Degraded,
    Unavailable,
    NotConfigured,
}

impl AttentionRouterSourceHealth {
    fn reason_code(self, source_kind: AttentionRouterSourceKind) -> String {
        let state = match self {
            Self::Available => "available",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
            Self::NotConfigured => "not_configured",
        };
        format!("source.{}.{}", source_kind.slug(), state)
    }

    fn is_attention_issue(self) -> bool {
        self != Self::Available
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterRedactionPosture {
    Redacted,
    SummaryOnly,
    NoSensitiveContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSourceFactKind {
    BeadsReady,
    BeadsBlocked,
    BeadsInProgress,
    BeadsPriority,
    BeadsAssignee,
    BeadsDependencies,
    BeadsAge,
    BeadsRecentComments,
    BvRecommendationConflict,
    AgentMailRegisteredAgents,
    AgentMailRecentMessages,
    AgentMailAckRequired,
    AgentMailFileReservations,
    AgentMailFallbackState,
    GitBranchDivergence,
    GitDirtyPaths,
    GitStagedPaths,
    GitRecentCommits,
    GitClaimOverlap,
    RchInstalledStatus,
    RchQueueState,
    RchWorkerPressure,
    RchRemoteDryRun,
    RchProofStarvation,
    PaneAgentLiveness,
    PaneIdleSignal,
    PaneStuckSignal,
    PaneCodexPlaceholderCaveat,
    OperatingEnvelopeCapacity,
    OperatingEnvelopeSideEffectPolicy,
    OperatingEnvelopeProofPosture,
    SourceUnavailable,
    SourceNotConfigured,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceFact {
    pub fact: AttentionRouterSourceFactKind,
    pub summary: String,
    pub count: Option<u64>,
    pub bead_ids: Vec<String>,
    pub agent_names: Vec<String>,
    pub affected_paths: Vec<String>,
    pub reason_codes: Vec<String>,
}

impl AttentionRouterSourceFact {
    #[must_use]
    pub fn new(fact: AttentionRouterSourceFactKind, summary: impl Into<String>) -> Self {
        Self {
            fact,
            summary: bounded_string(summary, "source fact unavailable"),
            count: None,
            bead_ids: Vec::new(),
            agent_names: Vec::new(),
            affected_paths: Vec::new(),
            reason_codes: Vec::new(),
        }
    }

    #[must_use]
    pub fn count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }

    #[must_use]
    pub fn with_bead_id(mut self, bead_id: impl Into<String>) -> Self {
        push_unique(&mut self.bead_ids, bead_id);
        self
    }

    #[must_use]
    pub fn with_agent_name(mut self, agent_name: impl Into<String>) -> Self {
        push_unique(&mut self.agent_names, agent_name);
        self
    }

    #[must_use]
    pub fn with_affected_path(mut self, affected_path: impl Into<String>) -> Self {
        push_unique(&mut self.affected_paths, affected_path);
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceObservation {
    pub source_id: String,
    pub source_kind: AttentionRouterSourceKind,
    pub health: AttentionRouterSourceHealth,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub live: bool,
    pub redaction_posture: AttentionRouterRedactionPosture,
    pub source_summary: String,
    pub reason_codes: Vec<String>,
    pub facts: Vec<AttentionRouterSourceFact>,
    pub items_seen: Option<u64>,
}

impl AttentionRouterSourceObservation {
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        source_kind: AttentionRouterSourceKind,
        health: AttentionRouterSourceHealth,
        command_or_api: impl Into<String>,
        source_summary: impl Into<String>,
    ) -> Self {
        let mut reason_codes = Vec::new();
        push_unique(&mut reason_codes, health.reason_code(source_kind));
        Self {
            source_id: bounded_string(source_id, "source.unknown"),
            source_kind,
            health,
            collected_at_ms: None,
            freshness_ms: None,
            command_or_api: bounded_string(command_or_api, "collector.unavailable"),
            live: false,
            redaction_posture: AttentionRouterRedactionPosture::Redacted,
            source_summary: bounded_string(source_summary, "source summary unavailable"),
            reason_codes,
            facts: Vec::new(),
            items_seen: None,
        }
    }

    #[must_use]
    pub fn live(mut self, collected_at_ms: u64, freshness_ms: u64) -> Self {
        self.live = true;
        self.collected_at_ms = Some(collected_at_ms);
        self.freshness_ms = Some(freshness_ms);
        self
    }

    #[must_use]
    pub fn redaction_posture(mut self, posture: AttentionRouterRedactionPosture) -> Self {
        self.redaction_posture = posture;
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_fact(mut self, fact: AttentionRouterSourceFact) -> Self {
        self.facts.push(fact);
        self
    }

    #[must_use]
    pub fn items_seen(mut self, items_seen: u64) -> Self {
        self.items_seen = Some(items_seen);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceAdapterInput {
    pub generated_at_ms: u64,
    pub workspace: String,
    pub observations: Vec<AttentionRouterSourceObservation>,
}

impl AttentionRouterSourceAdapterInput {
    #[must_use]
    pub fn new(generated_at_ms: u64, workspace: impl Into<String>) -> Self {
        Self {
            generated_at_ms,
            workspace: bounded_string(workspace, "."),
            observations: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_observation(mut self, observation: AttentionRouterSourceObservation) -> Self {
        self.observations.push(observation);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceSnapshot {
    pub source_id: String,
    pub source_kind: AttentionRouterSourceKind,
    pub health: AttentionRouterSourceHealth,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub live: bool,
    pub redaction_posture: AttentionRouterRedactionPosture,
    pub source_summary: String,
    pub redacted: bool,
    pub reason_codes: Vec<String>,
    pub facts: Vec<AttentionRouterSourceFact>,
    pub items_seen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceHealthRecord {
    pub source_kind: AttentionRouterSourceKind,
    pub health: AttentionRouterSourceHealth,
    pub source_id: String,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceBundle {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub workspace: String,
    pub sources: Vec<AttentionRouterSourceSnapshot>,
    pub source_health: Vec<AttentionRouterSourceHealthRecord>,
    pub warnings: Vec<String>,
    pub raw_pane_content_stored: bool,
    pub raw_message_bodies_stored: bool,
    pub side_effects_executed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterClassification {
    ReadyNow,
    BlockedInfra,
    BlockedDomain,
    WaitingComm,
    StaleClaim,
    DirtyOverlap,
    ProofStarved,
    DoNotTouch,
}

impl AttentionRouterClassification {
    #[must_use]
    pub fn severity_rank(self) -> u8 {
        match self {
            Self::DoNotTouch => 0,
            Self::DirtyOverlap | Self::WaitingComm | Self::ProofStarved => 1,
            Self::BlockedInfra | Self::BlockedDomain | Self::StaleClaim => 2,
            Self::ReadyNow => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterConfidence {
    Low,
    Medium,
    High,
}

impl AttentionRouterConfidence {
    #[must_use]
    pub fn score(self) -> f32 {
        match self {
            Self::Low => 0.45,
            Self::Medium => 0.7,
            Self::High => 0.92,
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::High => 0,
            Self::Medium => 1,
            Self::Low => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterItemKind {
    ReadyWork,
    Blocker,
    Communication,
    Ownership,
    Proof,
    SourceHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSafeAction {
    ClaimReadyStaticSliceReservePathsAndRunStaticChecks,
    DoNotClaimBvPickRecordBlockerOrFindDisjointStaticSlice,
    KeepProofRequiredBeadOpenOrBlockedAndRecordRchReasonCode,
    AcknowledgeMessageThenReplyOrContinueBasedOnRequest,
    ReplyToThreadWithBoundedContext,
    SendOrDraftStatusCheckBeforeForceRelease,
    AvoidOverlappingPathsAndClaimOnlyDisjointReadyWork,
    NotifyOwnerWaitForPublishOrPickDisjointWork,
    IgnoreBvCommandHintsUseBrJsonState,
    ChooseDisjointWorkOrRequestHandoffBeforeEditing,
    WaitForReservationReleaseOrExpiryThenRecheck,
    FailClosedRequestTargetedHandoffOrPickDisjointWork,
    WaitForCommitPushMirrorAndReservationRelease,
    SendStatusCheckThenPrepareOperatorReviewIfNoResponse,
    PauseWriteWorkRequestExactCleanupApprovalOrPickReadOnlyWork,
    WorkDomainDependencyFirst,
    RefreshUnavailableSourceOrUseRemainingReadOnlyContext,
}

impl AttentionRouterSafeAction {
    fn summary(self) -> &'static str {
        match self {
            Self::ClaimReadyStaticSliceReservePathsAndRunStaticChecks => {
                "Claim the ready static slice, reserve its paths, and run the required static checks."
            }
            Self::DoNotClaimBvPickRecordBlockerOrFindDisjointStaticSlice => {
                "Do not claim the bv pick; record the blocker or find a disjoint static slice."
            }
            Self::KeepProofRequiredBeadOpenOrBlockedAndRecordRchReasonCode => {
                "Keep proof-required work open or blocked and record the exact RCH reason."
            }
            Self::AcknowledgeMessageThenReplyOrContinueBasedOnRequest => {
                "Acknowledge the coordination request, then reply or continue based on the message."
            }
            Self::ReplyToThreadWithBoundedContext => {
                "Reply to the coordination thread with bounded context; do not broadcast automatically."
            }
            Self::SendOrDraftStatusCheckBeforeForceRelease => {
                "Send or draft a targeted status check before any force-release request."
            }
            Self::AvoidOverlappingPathsAndClaimOnlyDisjointReadyWork => {
                "Avoid overlapping paths and claim only disjoint ready work."
            }
            Self::NotifyOwnerWaitForPublishOrPickDisjointWork => {
                "Notify the owner, wait for publication, or pick disjoint work."
            }
            Self::IgnoreBvCommandHintsUseBrJsonState => {
                "Ignore bv command hints and use authoritative br JSON state."
            }
            Self::ChooseDisjointWorkOrRequestHandoffBeforeEditing => {
                "Choose disjoint work or request handoff before editing reserved paths."
            }
            Self::WaitForReservationReleaseOrExpiryThenRecheck => {
                "Wait for reservation release or expiry, then recheck tracker and git state."
            }
            Self::FailClosedRequestTargetedHandoffOrPickDisjointWork => {
                "Fail closed; request targeted handoff or pick disjoint work."
            }
            Self::WaitForCommitPushMirrorAndReservationRelease => {
                "Wait for commit, push, legacy mirror sync, and reservation release."
            }
            Self::SendStatusCheckThenPrepareOperatorReviewIfNoResponse => {
                "Send a status check, then prepare operator review if there is no response."
            }
            Self::PauseWriteWorkRequestExactCleanupApprovalOrPickReadOnlyWork => {
                "Pause write work, request exact cleanup approval, or pick read-only work."
            }
            Self::WorkDomainDependencyFirst => {
                "Work the product dependency before claiming the blocked candidate."
            }
            Self::RefreshUnavailableSourceOrUseRemainingReadOnlyContext => {
                "Refresh the unavailable source or proceed only from remaining read-only context."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterRecommendedAction {
    pub action: AttentionRouterSafeAction,
    pub summary: String,
    pub command_hint: Option<String>,
    pub mutates: bool,
}

impl AttentionRouterRecommendedAction {
    fn new(action: AttentionRouterSafeAction) -> Self {
        Self {
            action,
            summary: bounded_string(action.summary(), "attention action unavailable"),
            command_hint: None,
            mutates: false,
        }
    }

    fn with_command_hint(mut self, command_hint: impl Into<String>) -> Self {
        self.command_hint = Some(bounded_string(command_hint, "command hint unavailable"));
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterNudgeKind {
    AcknowledgeRequest,
    ReplyToThread,
    StatusCheck,
    HandoffRequest,
    ForceReleaseReview,
    NoAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterNudgeUrgency {
    Low,
    Normal,
    High,
    Urgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterNudgeTargetKind {
    Agent,
    Bead,
    Thread,
    Operator,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterNudgeTarget {
    pub kind: AttentionRouterNudgeTargetKind,
    pub bead_id: Option<String>,
    pub thread_ref: Option<String>,
    pub agent_name: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterNudgeEvidence {
    pub sources_checked: Vec<String>,
    pub reason_codes: Vec<String>,
    pub subjects: Vec<String>,
    pub minimum_source_count: u8,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterNudgeAction {
    pub kind: AttentionRouterNudgeKind,
    pub command_hint: String,
    pub safe_command_text: String,
    pub urgency: AttentionRouterNudgeUrgency,
    pub mutates: bool,
    pub review_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterNudgeEscalation {
    pub status_check_before_force_release: bool,
    pub elapsed_time_alone_sufficient: bool,
    pub human_review_required_for_mutation: bool,
    pub minimum_evidence_sources: u8,
    pub minimum_wait_minutes_after_status_check: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterNudgeBodyHandling {
    MetadataOnly,
    Summarized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterNudgeRedaction {
    pub body_handling: AttentionRouterNudgeBodyHandling,
    pub raw_pane_text_allowed: bool,
    pub full_message_body_allowed: bool,
    pub secret_material_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterNudgePlanReceipt {
    pub schema: String,
    pub contract_id: String,
    pub receipt_id: String,
    pub trigger_item_id: String,
    pub trigger_classification: AttentionRouterClassification,
    pub recipient: Option<String>,
    pub target: AttentionRouterNudgeTarget,
    pub evidence: AttentionRouterNudgeEvidence,
    pub nudge: AttentionRouterNudgeAction,
    pub escalation: AttentionRouterNudgeEscalation,
    pub redaction: AttentionRouterNudgeRedaction,
    pub forbidden_actions: Vec<String>,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSubject {
    pub bead_id: Option<String>,
    pub title: Option<String>,
    pub path: Option<String>,
    pub agent_name: Option<String>,
}

impl AttentionRouterSubject {
    fn from_fact(fact: &AttentionRouterSourceFact) -> Self {
        Self {
            bead_id: fact.bead_ids.first().cloned(),
            title: None,
            path: fact.affected_paths.first().cloned(),
            agent_name: fact.agent_names.first().cloned(),
        }
    }

    fn stable_slug(&self) -> String {
        self.bead_id
            .as_deref()
            .or(self.path.as_deref())
            .or(self.agent_name.as_deref())
            .unwrap_or("source")
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterEvidence {
    pub source_kind: AttentionRouterSourceKind,
    pub source_id: String,
    pub fact: AttentionRouterSourceFactKind,
    pub detail: String,
    pub bead_ids: Vec<String>,
    pub agent_names: Vec<String>,
    pub affected_paths: Vec<String>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionRouterItem {
    pub schema: String,
    pub item_id: String,
    pub kind: AttentionRouterItemKind,
    pub subject: AttentionRouterSubject,
    pub classification: AttentionRouterClassification,
    pub priority: u8,
    pub confidence: f32,
    pub confidence_label: AttentionRouterConfidence,
    pub evidence: Vec<AttentionRouterEvidence>,
    pub reason_codes: Vec<String>,
    pub recommended_action: AttentionRouterRecommendedAction,
    pub nudge_plan_receipt: AttentionRouterNudgePlanReceipt,
    pub forbidden_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionRouterSnapshot {
    pub schema: String,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub workspace: String,
    pub sources: Vec<AttentionRouterSourceSnapshot>,
    pub source_health: Vec<AttentionRouterSourceHealthRecord>,
    pub warnings: Vec<String>,
    pub items: Vec<AttentionRouterItem>,
    pub next_action: Option<AttentionRouterRecommendedAction>,
    pub side_effects_executed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSurface {
    Status,
    Next,
    Explain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterMcpResourceDescriptor {
    pub name: String,
    pub uri: Option<String>,
    pub uri_template: Option<String>,
    pub description: String,
    pub mime_type: String,
    pub read_only: bool,
    pub live_mutation_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterDegradedMode {
    pub active: bool,
    pub summary: String,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSurfaceLookupStatus {
    Status,
    NextItem,
    Matched,
    NotFound,
    NoItems,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSurfaceExplanation {
    pub status: AttentionRouterSurfaceLookupStatus,
    pub requested_item_id: Option<String>,
    pub matched_item_id: Option<String>,
    pub matched: bool,
    pub summary: String,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionRouterSurfacePayload {
    pub schema: String,
    pub contract_id: String,
    pub surface: AttentionRouterSurface,
    pub generated_at_ms: u64,
    pub workspace: String,
    pub source: String,
    pub dry_run: bool,
    pub raw_pane_content_stored: bool,
    pub raw_message_bodies_stored: bool,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
    pub degraded_mode: AttentionRouterDegradedMode,
    pub selected_item: Option<AttentionRouterItem>,
    pub next_action: Option<AttentionRouterRecommendedAction>,
    pub explanation: AttentionRouterSurfaceExplanation,
    pub mcp_resources: Vec<AttentionRouterMcpResourceDescriptor>,
    pub snapshot: AttentionRouterSnapshot,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AttentionRouterScoringEngine;

impl AttentionRouterScoringEngine {
    #[must_use]
    pub fn score(bundle: &AttentionRouterSourceBundle) -> AttentionRouterSnapshot {
        let mut items = Vec::new();
        for source in &bundle.sources {
            for fact in &source.facts {
                if let Some(item) = item_from_fact(source, fact) {
                    items.push(item);
                }
            }
        }
        items.sort_by(item_order);

        let mut warnings = bundle.warnings.clone();
        for item in &items {
            for reason_code in &item.reason_codes {
                if reason_code.contains("local_cargo") {
                    push_unique(&mut warnings, "local cargo output is not closeout proof");
                }
                if reason_code.contains("bv.") {
                    push_unique(
                        &mut warnings,
                        "bv recommendation is advisory; br JSON state controls actionability",
                    );
                }
            }
        }

        AttentionRouterSnapshot {
            schema: ATTENTION_ROUTER_SNAPSHOT_SCHEMA.to_string(),
            contract_id: ATTENTION_ROUTER_CONTRACT_ID.to_string(),
            generated_at_ms: bundle.generated_at_ms,
            workspace: bundle.workspace.clone(),
            sources: bundle.sources.clone(),
            source_health: bundle.source_health.clone(),
            warnings,
            next_action: items.first().map(|item| item.recommended_action.clone()),
            items,
            side_effects_executed: false,
        }
    }
}

#[must_use]
pub fn score_attention_router_source_bundle(
    bundle: &AttentionRouterSourceBundle,
) -> AttentionRouterSnapshot {
    AttentionRouterScoringEngine::score(bundle)
}

#[must_use]
pub fn build_attention_router_snapshot(
    input: &AttentionRouterSourceAdapterInput,
) -> AttentionRouterSnapshot {
    let bundle = build_attention_router_source_bundle(input);
    score_attention_router_source_bundle(&bundle)
}

#[must_use]
pub fn build_attention_router_surface_payload(
    input: &AttentionRouterSourceAdapterInput,
    surface: AttentionRouterSurface,
    source: impl Into<String>,
    requested_item_id: Option<&str>,
) -> AttentionRouterSurfacePayload {
    build_attention_router_surface_payload_from_snapshot(
        build_attention_router_snapshot(input),
        surface,
        source,
        requested_item_id,
    )
}

#[must_use]
pub fn build_attention_router_surface_payload_from_snapshot(
    snapshot: AttentionRouterSnapshot,
    surface: AttentionRouterSurface,
    source: impl Into<String>,
    requested_item_id: Option<&str>,
) -> AttentionRouterSurfacePayload {
    let selected_item = selected_attention_router_item(&snapshot, surface, requested_item_id);
    let explanation = attention_router_surface_explanation(
        &snapshot,
        surface,
        requested_item_id,
        selected_item.as_ref(),
    );
    AttentionRouterSurfacePayload {
        schema: ATTENTION_ROUTER_SURFACE_SCHEMA.to_string(),
        contract_id: ATTENTION_ROUTER_CONTRACT_ID.to_string(),
        surface,
        generated_at_ms: snapshot.generated_at_ms,
        workspace: snapshot.workspace.clone(),
        source: bounded_string(source, "attention_router.surface"),
        dry_run: true,
        raw_pane_content_stored: false,
        raw_message_bodies_stored: false,
        live_mutation_allowed: false,
        side_effects_executed: false,
        degraded_mode: attention_router_degraded_mode(&snapshot),
        selected_item,
        next_action: snapshot.next_action.clone(),
        explanation,
        mcp_resources: attention_router_mcp_resources(),
        snapshot,
    }
}

#[must_use]
pub fn attention_router_mcp_resources() -> Vec<AttentionRouterMcpResourceDescriptor> {
    vec![
        AttentionRouterMcpResourceDescriptor {
            name: "current".to_string(),
            uri: Some(ATTENTION_ROUTER_MCP_CURRENT_URI.to_string()),
            uri_template: None,
            description: "Read-only current attention-router surface".to_string(),
            mime_type: "application/json".to_string(),
            read_only: true,
            live_mutation_allowed: false,
        },
        AttentionRouterMcpResourceDescriptor {
            name: "item".to_string(),
            uri: None,
            uri_template: Some(ATTENTION_ROUTER_MCP_ITEM_URI_TEMPLATE.to_string()),
            description: "Read-only attention-router item explanation template".to_string(),
            mime_type: "application/json".to_string(),
            read_only: true,
            live_mutation_allowed: false,
        },
    ]
}

struct AttentionRouterRule {
    classification: AttentionRouterClassification,
    kind: AttentionRouterItemKind,
    action: AttentionRouterSafeAction,
    confidence: AttentionRouterConfidence,
    priority: u8,
    forbidden_actions: &'static [&'static str],
}

fn item_from_fact(
    source: &AttentionRouterSourceSnapshot,
    fact: &AttentionRouterSourceFact,
) -> Option<AttentionRouterItem> {
    let mut reason_codes = vec![source.health.reason_code(source.source_kind)];
    for reason_code in &source.reason_codes {
        if reason_code.starts_with("source.") {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }
    for reason_code in &fact.reason_codes {
        push_unique(&mut reason_codes, reason_code.clone());
    }

    if fact.fact == AttentionRouterSourceFactKind::SourceNotConfigured
        && source.source_kind == AttentionRouterSourceKind::PaneState
    {
        return None;
    }

    let rule = rule_from_fact(source, fact, &reason_codes)?;
    let subject = AttentionRouterSubject::from_fact(fact);
    let item_id = format!(
        "attention:{}:{}:{}",
        stable_ident(rule.classification),
        stable_ident(fact.fact),
        item_subject_slug(&subject, source)
    );
    let evidence = AttentionRouterEvidence {
        source_kind: source.source_kind,
        source_id: source.source_id.clone(),
        fact: fact.fact,
        detail: fact.summary.clone(),
        bead_ids: fact.bead_ids.clone(),
        agent_names: fact.agent_names.clone(),
        affected_paths: fact.affected_paths.clone(),
        reason_codes: reason_codes.clone(),
    };
    let recommended_action = action_for_rule(rule.action, &subject);
    let forbidden_actions = rule
        .forbidden_actions
        .iter()
        .map(|action| (*action).to_string())
        .collect::<Vec<_>>();
    let nudge_plan_receipt = nudge_plan_receipt_for_item(NudgeReceiptInput {
        item_id: &item_id,
        classification: rule.classification,
        action: rule.action,
        source,
        fact,
        subject: &subject,
        evidence: &evidence,
        reason_codes: &reason_codes,
        forbidden_actions: &forbidden_actions,
    });

    Some(AttentionRouterItem {
        schema: ATTENTION_ROUTER_ITEM_SCHEMA.to_string(),
        item_id,
        kind: rule.kind,
        subject,
        classification: rule.classification,
        priority: rule.priority,
        confidence: rule.confidence.score(),
        confidence_label: rule.confidence,
        evidence: vec![evidence],
        reason_codes,
        recommended_action,
        nudge_plan_receipt,
        forbidden_actions,
    })
}

fn rule_from_fact(
    source: &AttentionRouterSourceSnapshot,
    fact: &AttentionRouterSourceFact,
    reason_codes: &[String],
) -> Option<AttentionRouterRule> {
    let text = format!(
        "{} {} {}",
        source.source_summary, fact.summary, source.command_or_api
    )
    .to_ascii_lowercase();

    if contains_text(
        &text,
        &[
            "enospc",
            "disk pressure",
            "disk-pressure",
            "cleanup approval",
        ],
    ) || contains_reason(
        reason_codes,
        &["disk_pressure", "cleanup_approval", "enospc"],
    ) {
        return Some(rule(
            AttentionRouterClassification::BlockedInfra,
            AttentionRouterItemKind::Blocker,
            AttentionRouterSafeAction::PauseWriteWorkRequestExactCleanupApprovalOrPickReadOnlyWork,
            AttentionRouterConfidence::High,
            &[
                "destructive_cleanup_without_exact_approval",
                "rm_rf",
                "git_clean",
                "treat_cleanup_inventory_as_permission",
            ],
        ));
    }

    if contains_reason(
        reason_codes,
        &["bv.stale_command_hints", "bv.uses_legacy_bd"],
    ) || contains_text(&text, &["bd command hints", "stale command hints"])
    {
        return Some(rule(
            AttentionRouterClassification::DoNotTouch,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::IgnoreBvCommandHintsUseBrJsonState,
            AttentionRouterConfidence::High,
            &[
                "run_bd_claim_command",
                "run_bv_claim_hint_without_br_reconciliation",
                "auto_claim_bv_pick",
            ],
        ));
    }

    if contains_reason(
        reason_codes,
        &[
            "ownership.source_disagreement",
            "ownership.sources_disagree",
            "ownership.disagreement",
            "ownership.conflict",
        ],
    ) || contains_text(&text, &["ownership sources disagree"])
    {
        return Some(rule(
            AttentionRouterClassification::DoNotTouch,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::FailClosedRequestTargetedHandoffOrPickDisjointWork,
            AttentionRouterConfidence::High,
            &[
                "choose_winner_from_conflicting_metadata",
                "stage_unowned_tracker_changes",
                "force_release_without_status_check",
            ],
        ));
    }

    if contains_reason(
        reason_codes,
        &[
            "reservation.release_message_not_released",
            "reservation.not_released",
            "reservation.release_pending",
        ],
    ) {
        return Some(rule(
            AttentionRouterClassification::DoNotTouch,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::WaitForReservationReleaseOrExpiryThenRecheck,
            AttentionRouterConfidence::High,
            &[
                "treat_publication_message_as_release",
                "edit_reserved_path",
                "stage_reserved_path",
            ],
        ));
    }

    if contains_reason(
        reason_codes,
        &[
            "git.origin_main_missing_closeout",
            "git.legacy_mirror_missing_closeout",
            "publication_pending",
            "local_closeout",
        ],
    ) {
        return Some(rule(
            AttentionRouterClassification::DoNotTouch,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::WaitForCommitPushMirrorAndReservationRelease,
            AttentionRouterConfidence::High,
            &[
                "commit_another_agents_closeout",
                "stage_unowned_tracker_changes",
                "claim_dependent_before_publish",
            ],
        ));
    }

    if contains_reason(
        reason_codes,
        &[
            "beads.status_closed",
            "git.tracker_dirty",
            "agent_mail.active_owner_claim",
        ],
    ) && contains_reason(
        reason_codes,
        &["git.owned_paths_dirty", "reservation.owner_present"],
    ) {
        return Some(rule(
            AttentionRouterClassification::DoNotTouch,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::NotifyOwnerWaitForPublishOrPickDisjointWork,
            AttentionRouterConfidence::Medium,
            &[
                "commit_another_agents_closeout",
                "stage_unowned_tracker_changes",
                "claim_dependent_before_publish",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::AgentMailAckRequired
        || contains_reason(reason_codes, &["agent_mail.ack_required"])
    {
        return Some(rule(
            AttentionRouterClassification::WaitingComm,
            AttentionRouterItemKind::Communication,
            AttentionRouterSafeAction::AcknowledgeMessageThenReplyOrContinueBasedOnRequest,
            AttentionRouterConfidence::High,
            &[
                "ignore_ack_required_message",
                "agent_mail_repair",
                "broadcast_spam",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::AgentMailRecentMessages
        && (contains_reason(
            reason_codes,
            &[
                "agent_mail.reply_required",
                "agent_mail.thread_reply_required",
                "coordination.thread_reply",
            ],
        ) || contains_text(&text, &["reply required", "thread reply", "direct request"]))
    {
        return Some(rule(
            AttentionRouterClassification::WaitingComm,
            AttentionRouterItemKind::Communication,
            AttentionRouterSafeAction::ReplyToThreadWithBoundedContext,
            AttentionRouterConfidence::High,
            &[
                "ignore_direct_thread_request",
                "auto_send_reply",
                "agent_mail_repair",
                "broadcast_spam",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::RchProofStarvation
        || contains_reason(
            reason_codes,
            &[
                "rch.no_admissible_workers",
                "rch.remote_cargo_reached_false",
                "rch.proof_starved",
                "local_cargo",
                "local_fallback",
                "worker_null",
            ],
        )
        || contains_text(
            &text,
            &[
                "[rch] local",
                "running locally",
                "local fallback",
                "worker=null",
                "no admissible workers",
            ],
        )
    {
        return Some(rule(
            AttentionRouterClassification::ProofStarved,
            AttentionRouterItemKind::Proof,
            AttentionRouterSafeAction::KeepProofRequiredBeadOpenOrBlockedAndRecordRchReasonCode,
            AttentionRouterConfidence::High,
            &[
                "local_cargo_proof",
                "rch_service_restart",
                "worker_mutation",
                "build_cancellation",
            ],
        ));
    }

    if contains_reason(
        reason_codes,
        &[
            "reservation.active_exclusive",
            "agent_mail.file_reservation_overlap",
            "reservation.overlap",
        ],
    ) {
        return Some(rule(
            AttentionRouterClassification::DirtyOverlap,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::ChooseDisjointWorkOrRequestHandoffBeforeEditing,
            AttentionRouterConfidence::High,
            &[
                "edit_reserved_path",
                "stage_reserved_path",
                "claim_overlapping_work",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::GitClaimOverlap
        || fact.fact == AttentionRouterSourceFactKind::GitDirtyPaths
        || fact.fact == AttentionRouterSourceFactKind::AgentMailFileReservations
        || contains_reason(
            reason_codes,
            &[
                "git.claim_overlap",
                "git.owned_paths_dirty",
                "dirty_overlap",
                "active_owner",
            ],
        )
    {
        return Some(rule(
            AttentionRouterClassification::DirtyOverlap,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::AvoidOverlappingPathsAndClaimOnlyDisjointReadyWork,
            AttentionRouterConfidence::Medium,
            &[
                "stash_unrelated_changes",
                "revert_unrelated_changes",
                "stage_unrelated_dirty_files",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::BeadsInProgress
        && (contains_reason(
            reason_codes,
            &["beads.no_recent_update", "stale_claim", "stale_owner"],
        ) || contains_text(&text, &["no recent update", "appears stale"]))
    {
        let action = if contains_reason(
            reason_codes,
            &["force_release_review", "owner_status_before_force_release"],
        ) {
            AttentionRouterSafeAction::SendStatusCheckThenPrepareOperatorReviewIfNoResponse
        } else {
            AttentionRouterSafeAction::SendOrDraftStatusCheckBeforeForceRelease
        };
        return Some(rule(
            AttentionRouterClassification::StaleClaim,
            AttentionRouterItemKind::Ownership,
            action,
            AttentionRouterConfidence::Medium,
            &[
                "force_release_without_status_check",
                "treat_elapsed_time_as_takeover_permission",
                "broadcast_spam",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::PaneStuckSignal
        && !contains_reason(reason_codes, &["pane_state.codex_placeholder_caveat"])
    {
        return Some(rule(
            AttentionRouterClassification::StaleClaim,
            AttentionRouterItemKind::Ownership,
            AttentionRouterSafeAction::SendOrDraftStatusCheckBeforeForceRelease,
            AttentionRouterConfidence::Low,
            &[
                "treat_pane_text_as_ownership_proof",
                "force_release_without_status_check",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::BvRecommendationConflict
        || contains_reason(
            reason_codes,
            &["bv.recommends_blocked_issue", "beads.ready_empty"],
        )
    {
        return Some(rule(
            AttentionRouterClassification::BlockedInfra,
            AttentionRouterItemKind::Blocker,
            AttentionRouterSafeAction::DoNotClaimBvPickRecordBlockerOrFindDisjointStaticSlice,
            AttentionRouterConfidence::High,
            &[
                "claim_bv_pick",
                "run_local_cargo",
                "restart_rch",
                "force_release_assignee",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::SourceUnavailable
        || fact.fact == AttentionRouterSourceFactKind::AgentMailFallbackState
        || source.health == AttentionRouterSourceHealth::Unavailable
    {
        return Some(rule(
            AttentionRouterClassification::BlockedInfra,
            AttentionRouterItemKind::SourceHealth,
            AttentionRouterSafeAction::RefreshUnavailableSourceOrUseRemainingReadOnlyContext,
            AttentionRouterConfidence::Medium,
            &[
                "agent_mail_repair",
                "agent_mail_restart",
                "rch_service_restart",
                "service_mutation",
            ],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::RchQueueState
        || fact.fact == AttentionRouterSourceFactKind::RchWorkerPressure
        || fact.fact == AttentionRouterSourceFactKind::RchRemoteDryRun
    {
        return Some(rule(
            AttentionRouterClassification::BlockedInfra,
            AttentionRouterItemKind::Blocker,
            AttentionRouterSafeAction::DoNotClaimBvPickRecordBlockerOrFindDisjointStaticSlice,
            AttentionRouterConfidence::Medium,
            &["restart_rch", "worker_mutation", "build_cancellation"],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::BeadsBlocked
        || fact.fact == AttentionRouterSourceFactKind::BeadsDependencies
    {
        return Some(rule(
            AttentionRouterClassification::BlockedDomain,
            AttentionRouterItemKind::Blocker,
            AttentionRouterSafeAction::WorkDomainDependencyFirst,
            AttentionRouterConfidence::Medium,
            &["claim_blocked_dependent", "force_close_dependency"],
        ));
    }

    if fact.fact == AttentionRouterSourceFactKind::BeadsReady {
        return Some(rule(
            AttentionRouterClassification::ReadyNow,
            AttentionRouterItemKind::ReadyWork,
            AttentionRouterSafeAction::ClaimReadyStaticSliceReservePathsAndRunStaticChecks,
            AttentionRouterConfidence::High,
            &[
                "claim_without_reservation",
                "run_local_cargo_as_proof",
                "edit_overlapping_dirty_paths",
            ],
        ));
    }

    None
}

fn rule(
    classification: AttentionRouterClassification,
    kind: AttentionRouterItemKind,
    action: AttentionRouterSafeAction,
    confidence: AttentionRouterConfidence,
    forbidden_actions: &'static [&'static str],
) -> AttentionRouterRule {
    AttentionRouterRule {
        classification,
        kind,
        action,
        confidence,
        priority: classification.severity_rank(),
        forbidden_actions,
    }
}

fn action_for_rule(
    action: AttentionRouterSafeAction,
    subject: &AttentionRouterSubject,
) -> AttentionRouterRecommendedAction {
    let recommended_action = AttentionRouterRecommendedAction::new(action);
    if action == AttentionRouterSafeAction::ClaimReadyStaticSliceReservePathsAndRunStaticChecks {
        if let Some(bead_id) = &subject.bead_id {
            return recommended_action.with_command_hint(format!("br show {bead_id} --json"));
        }
    }
    recommended_action
}

struct NudgeReceiptInput<'a> {
    item_id: &'a str,
    classification: AttentionRouterClassification,
    action: AttentionRouterSafeAction,
    source: &'a AttentionRouterSourceSnapshot,
    fact: &'a AttentionRouterSourceFact,
    subject: &'a AttentionRouterSubject,
    evidence: &'a AttentionRouterEvidence,
    reason_codes: &'a [String],
    forbidden_actions: &'a [String],
}

fn nudge_plan_receipt_for_item(input: NudgeReceiptInput<'_>) -> AttentionRouterNudgePlanReceipt {
    let kind = nudge_kind_for_action(input.action, input.fact.fact, input.reason_codes);
    let recipient = nudge_recipient(kind, input.subject);
    let target = nudge_target(kind, input.subject);
    let command_hint = nudge_command_hint(kind, input.subject);
    let minimum_source_count = minimum_source_count_for_nudge(kind);
    let summary = nudge_summary(kind, &input);

    AttentionRouterNudgePlanReceipt {
        schema: ATTENTION_ROUTER_NUDGE_PLAN_RECEIPT_SCHEMA.to_string(),
        contract_id: ATTENTION_ROUTER_NUDGE_PLAN_RECEIPTS_CONTRACT_ID.to_string(),
        receipt_id: nudge_receipt_id(input.item_id, kind),
        trigger_item_id: input.item_id.to_string(),
        trigger_classification: input.classification,
        recipient,
        target,
        evidence: AttentionRouterNudgeEvidence {
            sources_checked: sources_checked_for_nudge(input.source, kind),
            reason_codes: input.reason_codes.to_vec(),
            subjects: nudge_subjects(input.fact, input.subject),
            minimum_source_count,
            summary,
        },
        nudge: AttentionRouterNudgeAction {
            kind,
            command_hint: command_hint.clone(),
            safe_command_text: command_hint,
            urgency: urgency_for_nudge(kind),
            mutates: false,
            review_required: review_required_for_nudge(kind),
        },
        escalation: AttentionRouterNudgeEscalation {
            status_check_before_force_release: true,
            elapsed_time_alone_sufficient: false,
            human_review_required_for_mutation: true,
            minimum_evidence_sources: minimum_source_count,
            minimum_wait_minutes_after_status_check: wait_minutes_for_nudge(kind),
        },
        redaction: AttentionRouterNudgeRedaction {
            body_handling: body_handling_for_nudge(kind),
            raw_pane_text_allowed: false,
            full_message_body_allowed: false,
            secret_material_allowed: false,
        },
        forbidden_actions: nudge_forbidden_actions(input.forbidden_actions),
        live_mutation_allowed: false,
        side_effects_executed: false,
    }
}

fn nudge_forbidden_actions(rule_forbidden_actions: &[String]) -> Vec<String> {
    let mut forbidden_actions = Vec::new();
    for action in rule_forbidden_actions {
        push_unique(&mut forbidden_actions, action.clone());
    }
    for action in ATTENTION_ROUTER_NUDGE_GLOBAL_FORBIDDEN_ACTIONS {
        push_unique(&mut forbidden_actions, (*action).to_string());
    }
    forbidden_actions
}

fn nudge_kind_for_action(
    action: AttentionRouterSafeAction,
    fact_kind: AttentionRouterSourceFactKind,
    reason_codes: &[String],
) -> AttentionRouterNudgeKind {
    match action {
        AttentionRouterSafeAction::AcknowledgeMessageThenReplyOrContinueBasedOnRequest => {
            AttentionRouterNudgeKind::AcknowledgeRequest
        }
        AttentionRouterSafeAction::ReplyToThreadWithBoundedContext => {
            AttentionRouterNudgeKind::ReplyToThread
        }
        AttentionRouterSafeAction::SendOrDraftStatusCheckBeforeForceRelease => {
            AttentionRouterNudgeKind::StatusCheck
        }
        AttentionRouterSafeAction::SendStatusCheckThenPrepareOperatorReviewIfNoResponse => {
            if contains_reason(
                reason_codes,
                &["force_release_review", "agent_mail.status_check_sent"],
            ) {
                AttentionRouterNudgeKind::ForceReleaseReview
            } else {
                AttentionRouterNudgeKind::StatusCheck
            }
        }
        AttentionRouterSafeAction::AvoidOverlappingPathsAndClaimOnlyDisjointReadyWork
        | AttentionRouterSafeAction::ChooseDisjointWorkOrRequestHandoffBeforeEditing
        | AttentionRouterSafeAction::FailClosedRequestTargetedHandoffOrPickDisjointWork
        | AttentionRouterSafeAction::NotifyOwnerWaitForPublishOrPickDisjointWork => {
            AttentionRouterNudgeKind::HandoffRequest
        }
        AttentionRouterSafeAction::KeepProofRequiredBeadOpenOrBlockedAndRecordRchReasonCode
            if fact_kind == AttentionRouterSourceFactKind::RchProofStarvation =>
        {
            AttentionRouterNudgeKind::NoAction
        }
        _ => AttentionRouterNudgeKind::NoAction,
    }
}

fn nudge_recipient(
    kind: AttentionRouterNudgeKind,
    subject: &AttentionRouterSubject,
) -> Option<String> {
    match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest
        | AttentionRouterNudgeKind::ReplyToThread
        | AttentionRouterNudgeKind::HandoffRequest
        | AttentionRouterNudgeKind::StatusCheck => subject
            .agent_name
            .clone()
            .or_else(|| {
                subject
                    .bead_id
                    .as_ref()
                    .map(|bead_id| format!("bead-thread:{bead_id}"))
            })
            .or_else(|| Some("coordination-thread".to_string())),
        AttentionRouterNudgeKind::ForceReleaseReview => Some("operator-review".to_string()),
        AttentionRouterNudgeKind::NoAction => None,
    }
}

fn nudge_target(
    kind: AttentionRouterNudgeKind,
    subject: &AttentionRouterSubject,
) -> AttentionRouterNudgeTarget {
    let target_kind = match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest | AttentionRouterNudgeKind::ReplyToThread => {
            AttentionRouterNudgeTargetKind::Thread
        }
        AttentionRouterNudgeKind::StatusCheck => AttentionRouterNudgeTargetKind::Bead,
        AttentionRouterNudgeKind::HandoffRequest => AttentionRouterNudgeTargetKind::Agent,
        AttentionRouterNudgeKind::ForceReleaseReview => AttentionRouterNudgeTargetKind::Operator,
        AttentionRouterNudgeKind::NoAction => AttentionRouterNudgeTargetKind::None,
    };
    AttentionRouterNudgeTarget {
        kind: target_kind,
        bead_id: subject.bead_id.clone(),
        thread_ref: subject.bead_id.clone(),
        agent_name: subject.agent_name.clone(),
        path: subject.path.clone(),
    }
}

fn nudge_command_hint(kind: AttentionRouterNudgeKind, subject: &AttentionRouterSubject) -> String {
    match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest => {
            "acknowledge_message(project_key, agent_name, message_id) after reviewing the message metadata".to_string()
        }
        AttentionRouterNudgeKind::ReplyToThread => {
            let thread = subject.bead_id.as_deref().unwrap_or("[THREAD_ID]");
            format!("draft reply_message(thread_id='{thread}') with bounded context; do not broadcast automatically")
        }
        AttentionRouterNudgeKind::StatusCheck => {
            let target = subject
                .bead_id
                .as_deref()
                .or(subject.agent_name.as_deref())
                .unwrap_or("the ownership thread");
            format!("draft status check for {target}; send only by explicit caller action before any force-release review")
        }
        AttentionRouterNudgeKind::HandoffRequest => {
            let target = subject
                .agent_name
                .as_deref()
                .or(subject.path.as_deref())
                .unwrap_or("the current owner");
            format!("draft handoff request to {target}; do not edit or stage overlapping paths")
        }
        AttentionRouterNudgeKind::ForceReleaseReview => {
            let target = subject
                .bead_id
                .as_deref()
                .or(subject.agent_name.as_deref())
                .unwrap_or("the stale claim");
            format!("prepare force-release review evidence for {target}; do not reopen or release automatically")
        }
        AttentionRouterNudgeKind::NoAction => {
            "record the blocker or choose disjoint static work; do not send a broadcast or mutate services".to_string()
        }
    }
}

fn minimum_source_count_for_nudge(kind: AttentionRouterNudgeKind) -> u8 {
    match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest | AttentionRouterNudgeKind::ReplyToThread => 1,
        AttentionRouterNudgeKind::StatusCheck | AttentionRouterNudgeKind::HandoffRequest => 3,
        AttentionRouterNudgeKind::ForceReleaseReview | AttentionRouterNudgeKind::NoAction => 4,
    }
}

fn wait_minutes_for_nudge(kind: AttentionRouterNudgeKind) -> u16 {
    match kind {
        AttentionRouterNudgeKind::StatusCheck => 30,
        AttentionRouterNudgeKind::ForceReleaseReview => 60,
        _ => 0,
    }
}

fn urgency_for_nudge(kind: AttentionRouterNudgeKind) -> AttentionRouterNudgeUrgency {
    match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest
        | AttentionRouterNudgeKind::ForceReleaseReview => AttentionRouterNudgeUrgency::High,
        AttentionRouterNudgeKind::ReplyToThread
        | AttentionRouterNudgeKind::StatusCheck
        | AttentionRouterNudgeKind::HandoffRequest
        | AttentionRouterNudgeKind::NoAction => AttentionRouterNudgeUrgency::Normal,
    }
}

fn review_required_for_nudge(kind: AttentionRouterNudgeKind) -> bool {
    matches!(
        kind,
        AttentionRouterNudgeKind::StatusCheck
            | AttentionRouterNudgeKind::HandoffRequest
            | AttentionRouterNudgeKind::ForceReleaseReview
    )
}

fn body_handling_for_nudge(kind: AttentionRouterNudgeKind) -> AttentionRouterNudgeBodyHandling {
    match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest | AttentionRouterNudgeKind::NoAction => {
            AttentionRouterNudgeBodyHandling::MetadataOnly
        }
        AttentionRouterNudgeKind::ReplyToThread
        | AttentionRouterNudgeKind::StatusCheck
        | AttentionRouterNudgeKind::HandoffRequest
        | AttentionRouterNudgeKind::ForceReleaseReview => {
            AttentionRouterNudgeBodyHandling::Summarized
        }
    }
}

fn nudge_summary(kind: AttentionRouterNudgeKind, input: &NudgeReceiptInput<'_>) -> String {
    let action = match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest => "acknowledge an existing direct request",
        AttentionRouterNudgeKind::ReplyToThread => "reply to the existing coordination thread",
        AttentionRouterNudgeKind::StatusCheck => {
            "draft a status check before any force-release review"
        }
        AttentionRouterNudgeKind::HandoffRequest => {
            "request handoff or choose disjoint work before editing"
        }
        AttentionRouterNudgeKind::ForceReleaseReview => {
            "prepare evidence for operator force-release review only"
        }
        AttentionRouterNudgeKind::NoAction => "take no communication action from this receipt",
    };
    bounded_string(
        format!("{action}: {}", input.evidence.detail),
        "nudge plan evidence unavailable",
    )
}

fn sources_checked_for_nudge(
    source: &AttentionRouterSourceSnapshot,
    kind: AttentionRouterNudgeKind,
) -> Vec<String> {
    let mut sources = Vec::new();
    push_unique(&mut sources, source.source_id.clone());
    match kind {
        AttentionRouterNudgeKind::AcknowledgeRequest | AttentionRouterNudgeKind::ReplyToThread => {
            push_unique(&mut sources, "agent_mail.inbox");
        }
        AttentionRouterNudgeKind::StatusCheck => {
            push_unique(&mut sources, "beads.in_progress");
            push_unique(&mut sources, "git.history");
            push_unique(&mut sources, "agent_mail.search");
        }
        AttentionRouterNudgeKind::HandoffRequest => {
            push_unique(&mut sources, "git.status");
            push_unique(&mut sources, "agent_mail.reservations");
            push_unique(&mut sources, "beads.owner_state");
        }
        AttentionRouterNudgeKind::ForceReleaseReview => {
            push_unique(&mut sources, "beads.in_progress");
            push_unique(&mut sources, "agent_mail.search");
            push_unique(&mut sources, "git.history");
            push_unique(&mut sources, "pane_state.optional");
        }
        AttentionRouterNudgeKind::NoAction => {
            push_unique(&mut sources, "br.ready");
            push_unique(&mut sources, "bv.triage");
            push_unique(&mut sources, "rch.diagnose");
            push_unique(&mut sources, "beads.blocker_state");
        }
    }
    sources
}

fn nudge_subjects(
    fact: &AttentionRouterSourceFact,
    subject: &AttentionRouterSubject,
) -> Vec<String> {
    let mut subjects = Vec::new();
    for bead_id in &fact.bead_ids {
        push_unique(&mut subjects, bead_id.clone());
    }
    for agent_name in &fact.agent_names {
        push_unique(&mut subjects, agent_name.clone());
    }
    for path in &fact.affected_paths {
        push_unique(&mut subjects, path.clone());
    }
    if let Some(bead_id) = &subject.bead_id {
        push_unique(&mut subjects, bead_id.clone());
    }
    if let Some(agent_name) = &subject.agent_name {
        push_unique(&mut subjects, agent_name.clone());
    }
    if subjects.is_empty() {
        push_unique(&mut subjects, "source");
    }
    subjects
}

fn nudge_receipt_id(item_id: &str, kind: AttentionRouterNudgeKind) -> String {
    format!(
        "nudge-{}-{}",
        stable_ident(kind).replace('_', "-"),
        item_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
    )
}

fn item_order(left: &AttentionRouterItem, right: &AttentionRouterItem) -> Ordering {
    (
        left.classification.severity_rank(),
        left.priority,
        left.confidence_label.sort_rank(),
        &left.item_id,
    )
        .cmp(&(
            right.classification.severity_rank(),
            right.priority,
            right.confidence_label.sort_rank(),
            &right.item_id,
        ))
}

fn item_subject_slug(
    subject: &AttentionRouterSubject,
    source: &AttentionRouterSourceSnapshot,
) -> String {
    let slug = subject.stable_slug();
    if slug.is_empty() || slug == "source" {
        stable_ident(&source.source_id)
    } else {
        slug
    }
}

fn contains_reason(reason_codes: &[String], needles: &[&str]) -> bool {
    reason_codes.iter().any(|reason_code| {
        let lower = reason_code.to_ascii_lowercase();
        needles.iter().any(|needle| lower.contains(needle))
    })
}

fn contains_text(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn stable_ident(value: impl std::fmt::Debug) -> String {
    let debug = format!("{value:?}");
    let mut output = String::new();
    for (index, ch) in debug.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
        } else if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
        } else if !output.ends_with('_') {
            output.push('_');
        }
    }
    output.trim_matches('_').to_string()
}

#[must_use]
pub fn build_attention_router_source_bundle(
    input: &AttentionRouterSourceAdapterInput,
) -> AttentionRouterSourceBundle {
    let mut observations = input.observations.clone();
    for source_kind in required_source_kinds() {
        if !observations
            .iter()
            .any(|observation| observation.source_kind == source_kind)
        {
            observations.push(missing_source_observation(
                source_kind,
                input.generated_at_ms,
            ));
        }
    }

    observations.sort_by_key(|observation| {
        (
            observation.source_kind,
            observation.source_id.clone(),
            observation.health,
        )
    });

    let sources = observations.iter().map(source_snapshot).collect::<Vec<_>>();
    let source_health = sources
        .iter()
        .filter(|source| source.health.is_attention_issue())
        .map(|source| AttentionRouterSourceHealthRecord {
            source_kind: source.source_kind,
            health: source.health,
            source_id: source.source_id.clone(),
            reason_codes: source.reason_codes.clone(),
        })
        .collect::<Vec<_>>();
    let warnings = source_health
        .iter()
        .map(|record| {
            format!(
                "{} source health is {}",
                record.source_kind.slug(),
                stable_ident(record.health)
            )
        })
        .collect::<Vec<_>>();

    AttentionRouterSourceBundle {
        schema_version: ATTENTION_ROUTER_SOURCE_SCHEMA_VERSION,
        contract_id: ATTENTION_ROUTER_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        workspace: input.workspace.clone(),
        sources,
        source_health,
        warnings,
        raw_pane_content_stored: false,
        raw_message_bodies_stored: false,
        side_effects_executed: false,
    }
}

fn selected_attention_router_item(
    snapshot: &AttentionRouterSnapshot,
    surface: AttentionRouterSurface,
    requested_item_id: Option<&str>,
) -> Option<AttentionRouterItem> {
    match surface {
        AttentionRouterSurface::Status => None,
        AttentionRouterSurface::Next => snapshot.items.first().cloned(),
        AttentionRouterSurface::Explain => match requested_item_id {
            Some(item_id) => snapshot
                .items
                .iter()
                .find(|item| item.item_id == item_id)
                .cloned(),
            None => snapshot.items.first().cloned(),
        },
    }
}

fn attention_router_surface_explanation(
    snapshot: &AttentionRouterSnapshot,
    surface: AttentionRouterSurface,
    requested_item_id: Option<&str>,
    selected_item: Option<&AttentionRouterItem>,
) -> AttentionRouterSurfaceExplanation {
    let mut reason_codes = Vec::new();
    let (status, summary) = match surface {
        AttentionRouterSurface::Status => {
            push_unique(&mut reason_codes, "attention_router.status.snapshot");
            (
                AttentionRouterSurfaceLookupStatus::Status,
                format!("{} attention item(s) scored", snapshot.items.len()),
            )
        }
        AttentionRouterSurface::Next => match selected_item {
            Some(item) => {
                reason_codes.extend(item.reason_codes.clone());
                (
                    AttentionRouterSurfaceLookupStatus::NextItem,
                    item.recommended_action.summary.clone(),
                )
            }
            None => {
                push_unique(&mut reason_codes, "attention_router.next.no_items");
                (
                    AttentionRouterSurfaceLookupStatus::NoItems,
                    "no attention items were scored".to_string(),
                )
            }
        },
        AttentionRouterSurface::Explain => match (requested_item_id, selected_item) {
            (_, Some(item)) => {
                reason_codes.extend(item.reason_codes.clone());
                (
                    AttentionRouterSurfaceLookupStatus::Matched,
                    item.recommended_action.summary.clone(),
                )
            }
            (Some(item_id), None) => {
                push_unique(&mut reason_codes, "attention_router.explain.item_not_found");
                (
                    AttentionRouterSurfaceLookupStatus::NotFound,
                    format!("attention item {item_id} was not found in the scored snapshot"),
                )
            }
            (None, None) => {
                push_unique(&mut reason_codes, "attention_router.explain.no_items");
                (
                    AttentionRouterSurfaceLookupStatus::NoItems,
                    "no attention items were available to explain".to_string(),
                )
            }
        },
    };

    AttentionRouterSurfaceExplanation {
        status,
        requested_item_id: requested_item_id.map(ToOwned::to_owned),
        matched_item_id: selected_item.map(|item| item.item_id.clone()),
        matched: selected_item.is_some(),
        summary: bounded_string(summary, "attention-router explanation unavailable"),
        reason_codes,
    }
}

fn attention_router_degraded_mode(
    snapshot: &AttentionRouterSnapshot,
) -> AttentionRouterDegradedMode {
    let mut reason_codes = Vec::new();
    for record in &snapshot.source_health {
        for reason_code in &record.reason_codes {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }
    for warning in &snapshot.warnings {
        push_unique(
            &mut reason_codes,
            format!("warning:{}", stable_warning_slug(warning)),
        );
    }
    let active = !reason_codes.is_empty();
    let summary = if active {
        format!(
            "{} degraded source/diagnostic signal(s) require caller caution",
            reason_codes.len()
        )
    } else {
        "all required attention-router sources are healthy".to_string()
    };
    AttentionRouterDegradedMode {
        active,
        summary,
        reason_codes,
    }
}

fn source_snapshot(
    observation: &AttentionRouterSourceObservation,
) -> AttentionRouterSourceSnapshot {
    let mut reason_codes = observation.reason_codes.clone();
    push_unique(
        &mut reason_codes,
        observation.health.reason_code(observation.source_kind),
    );
    for fact in &observation.facts {
        for reason_code in &fact.reason_codes {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }

    AttentionRouterSourceSnapshot {
        source_id: observation.source_id.clone(),
        source_kind: observation.source_kind,
        health: observation.health,
        collected_at_ms: observation.collected_at_ms,
        freshness_ms: observation.freshness_ms,
        command_or_api: observation.command_or_api.clone(),
        live: observation.live,
        redaction_posture: observation.redaction_posture,
        source_summary: observation.source_summary.clone(),
        redacted: true,
        reason_codes,
        facts: observation.facts.clone(),
        items_seen: observation
            .items_seen
            .unwrap_or_else(|| fact_count(&observation.facts)),
    }
}

fn fact_count(facts: &[AttentionRouterSourceFact]) -> u64 {
    facts
        .iter()
        .map(|fact| fact.count.unwrap_or(1))
        .sum::<u64>()
}

fn required_source_kinds() -> [AttentionRouterSourceKind; 6] {
    [
        AttentionRouterSourceKind::Beads,
        AttentionRouterSourceKind::AgentMail,
        AttentionRouterSourceKind::Git,
        AttentionRouterSourceKind::Rch,
        AttentionRouterSourceKind::PaneState,
        AttentionRouterSourceKind::OperatingEnvelope,
    ]
}

fn missing_source_observation(
    source_kind: AttentionRouterSourceKind,
    generated_at_ms: u64,
) -> AttentionRouterSourceObservation {
    match source_kind {
        AttentionRouterSourceKind::PaneState => AttentionRouterSourceObservation::new(
            "pane_state.not_configured",
            source_kind,
            AttentionRouterSourceHealth::NotConfigured,
            "collector.optional",
            "pane state source was not configured by the caller",
        )
        .live(generated_at_ms, 0)
        .with_fact(
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::SourceNotConfigured,
                "pane state is optional and was not configured",
            )
            .with_reason_code("pane_state.optional_not_configured"),
        ),
        _ => {
            let slug = source_kind.slug();
            AttentionRouterSourceObservation::new(
                format!("{slug}.unavailable"),
                source_kind,
                AttentionRouterSourceHealth::Unavailable,
                "collector.unavailable",
                format!("{slug} source was not collected by the caller"),
            )
            .live(generated_at_ms, 0)
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::SourceUnavailable,
                    format!("{slug} source was missing from adapter input"),
                )
                .with_reason_code(format!("source.{slug}.missing")),
            )
        }
    }
}

fn bounded_string(value: impl Into<String>, fallback: &str) -> String {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    let mut output = String::new();
    for ch in trimmed.chars().take(ATTENTION_ROUTER_SUMMARY_MAX_CHARS) {
        output.push(ch);
    }
    if trimmed.chars().count() > ATTENTION_ROUTER_SUMMARY_MAX_CHARS {
        let keep = ATTENTION_ROUTER_SUMMARY_MAX_CHARS.saturating_sub(3);
        output = trimmed.chars().take(keep).collect::<String>();
        output.push_str("...");
    }
    output
}

fn stable_warning_slug(value: &str) -> String {
    let mut slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        "attention_router_warning".to_string()
    } else {
        bounded_string(slug, "attention_router_warning")
    }
}

fn push_unique(values: &mut Vec<String>, value: impl Into<String>) {
    let value = bounded_string(value, "");
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(
        bundle: &AttentionRouterSourceBundle,
        kind: AttentionRouterSourceKind,
    ) -> &AttentionRouterSourceSnapshot {
        bundle
            .sources
            .iter()
            .find(|source| source.source_kind == kind)
            .unwrap_or_else(|| panic!("missing source {kind:?}"))
    }

    fn healthy_observation(kind: AttentionRouterSourceKind) -> AttentionRouterSourceObservation {
        AttentionRouterSourceObservation::new(
            format!("{}.healthy", kind.slug()),
            kind,
            AttentionRouterSourceHealth::Available,
            "fixture.read_only",
            format!("{} fixture source is healthy", kind.slug()),
        )
        .with_fact(
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::Manual,
                "healthy fixture placeholder",
            )
            .with_reason_code(format!("{}.healthy_placeholder", kind.slug())),
        )
    }

    fn complete_input(
        observations: Vec<AttentionRouterSourceObservation>,
    ) -> AttentionRouterSourceAdapterInput {
        let mut input = AttentionRouterSourceAdapterInput::new(1_770_000_100_000, "/repo");
        let provided = observations
            .iter()
            .map(|observation| observation.source_kind)
            .collect::<Vec<_>>();
        for observation in observations {
            input = input.with_observation(observation);
        }
        for kind in required_source_kinds() {
            if kind != AttentionRouterSourceKind::PaneState && !provided.contains(&kind) {
                input = input.with_observation(healthy_observation(kind));
            }
        }
        input
    }

    fn score(observations: Vec<AttentionRouterSourceObservation>) -> AttentionRouterSnapshot {
        build_attention_router_snapshot(&complete_input(observations))
    }

    fn item_with_nudge(
        snapshot: &AttentionRouterSnapshot,
        kind: AttentionRouterNudgeKind,
    ) -> &AttentionRouterItem {
        snapshot
            .items
            .iter()
            .find(|item| item.nudge_plan_receipt.nudge.kind == kind)
            .unwrap_or_else(|| panic!("missing nudge kind {kind:?}"))
    }

    #[test]
    fn missing_required_sources_are_explicitly_unhealthy() {
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(1_770_000_000_100, "/repo").with_observation(
                AttentionRouterSourceObservation::new(
                    "beads.ready",
                    AttentionRouterSourceKind::Beads,
                    AttentionRouterSourceHealth::Available,
                    "br ready --json",
                    "ready beads were collected",
                )
                .items_seen(2)
                .with_fact(
                    AttentionRouterSourceFact::new(
                        AttentionRouterSourceFactKind::BeadsReady,
                        "two ready beads",
                    )
                    .count(2)
                    .with_bead_id("ft-ready")
                    .with_reason_code("beads.ready_available"),
                ),
            ),
        );

        assert_eq!(bundle.contract_id, ATTENTION_ROUTER_CONTRACT_ID);
        assert!(!bundle.raw_pane_content_stored);
        assert!(!bundle.raw_message_bodies_stored);
        assert!(!bundle.side_effects_executed);
        assert_eq!(
            source(&bundle, AttentionRouterSourceKind::Beads).health,
            AttentionRouterSourceHealth::Available
        );
        assert_eq!(
            source(&bundle, AttentionRouterSourceKind::AgentMail).health,
            AttentionRouterSourceHealth::Unavailable
        );
        assert_eq!(
            source(&bundle, AttentionRouterSourceKind::PaneState).health,
            AttentionRouterSourceHealth::NotConfigured
        );
        assert!(bundle.source_health.iter().any(|record| {
            record.source_kind == AttentionRouterSourceKind::AgentMail
                && record.health == AttentionRouterSourceHealth::Unavailable
        }));
        assert!(bundle.source_health.iter().any(|record| {
            record.source_kind == AttentionRouterSourceKind::PaneState
                && record.health == AttentionRouterSourceHealth::NotConfigured
        }));
    }

    #[test]
    fn source_facts_are_bounded_redacted_and_deduplicated() {
        let long_summary = "pane output ".repeat(80);
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(1, "/repo").with_observation(
                AttentionRouterSourceObservation::new(
                    "pane_state.live",
                    AttentionRouterSourceKind::PaneState,
                    AttentionRouterSourceHealth::Degraded,
                    "ft robot state --format toon",
                    long_summary,
                )
                .redaction_posture(AttentionRouterRedactionPosture::SummaryOnly)
                .with_reason_code("pane_state.summary_only")
                .with_reason_code("pane_state.summary_only")
                .with_fact(
                    AttentionRouterSourceFact::new(
                        AttentionRouterSourceFactKind::PaneCodexPlaceholderCaveat,
                        "Codex placeholder text is caveat evidence, not stuck evidence",
                    )
                    .with_agent_name("IvoryCreek")
                    .with_reason_code("pane_state.codex_placeholder_caveat"),
                ),
            ),
        );
        let pane = source(&bundle, AttentionRouterSourceKind::PaneState);

        assert!(pane.redacted);
        assert_eq!(
            pane.redaction_posture,
            AttentionRouterRedactionPosture::SummaryOnly
        );
        assert!(
            pane.reason_codes
                .contains(&"pane_state.summary_only".to_string())
        );
        assert_eq!(
            pane.reason_codes
                .iter()
                .filter(|reason| reason.as_str() == "pane_state.summary_only")
                .count(),
            1
        );
        assert!(pane.source_id.len() <= ATTENTION_ROUTER_SUMMARY_MAX_CHARS);
        assert!(
            pane.facts
                .iter()
                .all(|fact| { fact.summary.chars().count() <= ATTENTION_ROUTER_SUMMARY_MAX_CHARS })
        );
    }

    #[test]
    fn adapters_preserve_agent_mail_git_and_rch_signals() {
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(2, "/repo")
                .with_observation(
                    AttentionRouterSourceObservation::new(
                        "agent_mail.inbox",
                        AttentionRouterSourceKind::AgentMail,
                        AttentionRouterSourceHealth::Available,
                        "mcp.agent_mail.fetch_inbox",
                        "recent inbox metadata collected",
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::AgentMailAckRequired,
                            "ack-required messages need response",
                        )
                        .count(2)
                        .with_agent_name("SapphireCardinal")
                        .with_reason_code("agent_mail.ack_required"),
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::AgentMailFileReservations,
                            "one active reservation overlaps a planned path",
                        )
                        .count(1)
                        .with_affected_path("crates/frankenterm-core/src/attention_router.rs")
                        .with_reason_code("agent_mail.file_reservation_overlap"),
                    ),
                )
                .with_observation(
                    AttentionRouterSourceObservation::new(
                        "git.status",
                        AttentionRouterSourceKind::Git,
                        AttentionRouterSourceHealth::Degraded,
                        "git status --short --branch",
                        "dirty tree requires ownership firewall",
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::GitDirtyPaths,
                            "tracked file is dirty",
                        )
                        .with_affected_path("docs/robot-contracts/attention-router.md")
                        .with_reason_code("git.tracked_dirty_paths"),
                    ),
                )
                .with_observation(
                    AttentionRouterSourceObservation::new(
                        "rch.dry_run",
                        AttentionRouterSourceKind::Rch,
                        AttentionRouterSourceHealth::Degraded,
                        "rch remote-required dry-run",
                        "RCH refused remote proof before Cargo",
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::RchProofStarvation,
                            "remote-required proof is starved",
                        )
                        .with_reason_code("rch.proof_starved"),
                    ),
                ),
        );

        let mail = source(&bundle, AttentionRouterSourceKind::AgentMail);
        assert_eq!(mail.items_seen, 3);
        assert!(mail.facts.iter().any(|fact| {
            fact.fact == AttentionRouterSourceFactKind::AgentMailAckRequired
                && fact.count == Some(2)
        }));
        let git = source(&bundle, AttentionRouterSourceKind::Git);
        assert_eq!(git.health, AttentionRouterSourceHealth::Degraded);
        assert!(git.facts.iter().any(|fact| {
            fact.affected_paths
                .contains(&"docs/robot-contracts/attention-router.md".to_string())
        }));
        let rch = source(&bundle, AttentionRouterSourceKind::Rch);
        assert!(rch.reason_codes.contains(&"rch.proof_starved".to_string()));
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.contains("rch"))
        );
    }

    #[test]
    fn bundle_serializes_stable_contract_values() {
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(3, "/repo").with_observation(
                AttentionRouterSourceObservation::new(
                    "operating_envelope.proof_posture",
                    AttentionRouterSourceKind::OperatingEnvelope,
                    AttentionRouterSourceHealth::Available,
                    "ft operating-envelope snapshot",
                    "target hardware proof posture collected",
                )
                .with_fact(
                    AttentionRouterSourceFact::new(
                        AttentionRouterSourceFactKind::OperatingEnvelopeProofPosture,
                        "target class proof remains skipped",
                    )
                    .with_reason_code("operating_envelope.target_class_skipped"),
                ),
            ),
        );

        let value = serde_json::to_value(bundle).expect("attention source bundle serializes");
        assert_eq!(
            value["contract_id"].as_str(),
            Some(ATTENTION_ROUTER_CONTRACT_ID)
        );
        assert_eq!(
            value["schema_version"].as_u64(),
            Some(u64::from(ATTENTION_ROUTER_SOURCE_SCHEMA_VERSION))
        );
        assert_eq!(value["side_effects_executed"].as_bool(), Some(false));
        assert_eq!(value["raw_message_bodies_stored"].as_bool(), Some(false));
        assert!(value["sources"].as_array().is_some_and(|sources| {
            sources.iter().any(|source| {
                source["source_kind"].as_str() == Some("operating_envelope")
                    && source["health"].as_str() == Some("available")
            })
        }));
    }

    #[test]
    fn scoring_engine_covers_contract_classification_vocabulary() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "beads.ready",
                AttentionRouterSourceKind::Beads,
                AttentionRouterSourceHealth::Available,
                "br ready --json",
                "ready static slice",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsReady,
                    "docs-only ready work",
                )
                .with_bead_id("ft-ready")
                .with_reason_code("beads.ready_available"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsBlocked,
                    "domain dependency is still open",
                )
                .with_bead_id("ft-blocked-domain")
                .with_reason_code("beads.status_blocked"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsInProgress,
                    "candidate has no recent update",
                )
                .with_bead_id("ft-stale")
                .with_reason_code("beads.no_recent_update"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BvRecommendationConflict,
                    "bv recommends blocked issue",
                )
                .with_bead_id("ft-4tp7g")
                .with_reason_code("bv.recommends_blocked_issue"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BvRecommendationConflict,
                    "bv emitted stale bd command hints",
                )
                .with_bead_id("ft-bv-stale")
                .with_reason_code("bv.stale_command_hints"),
            ),
            AttentionRouterSourceObservation::new(
                "agent_mail.inbox",
                AttentionRouterSourceKind::AgentMail,
                AttentionRouterSourceHealth::Available,
                "fetch_inbox",
                "mail metadata",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::AgentMailAckRequired,
                    "ack required",
                )
                .with_agent_name("SapphireCardinal")
                .with_reason_code("agent_mail.ack_required"),
            ),
            AttentionRouterSourceObservation::new(
                "git.status",
                AttentionRouterSourceKind::Git,
                AttentionRouterSourceHealth::Available,
                "git status --short --branch",
                "dirty overlap",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::GitClaimOverlap,
                    "dirty path overlaps another owner",
                )
                .with_affected_path("crates/frankenterm-core/src/config.rs")
                .with_reason_code("git.claim_overlap"),
            ),
            AttentionRouterSourceObservation::new(
                "rch.dry_run",
                AttentionRouterSourceKind::Rch,
                AttentionRouterSourceHealth::Degraded,
                "rch dry-run",
                "no admissible workers",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::RchProofStarvation,
                    "remote Cargo was not reached",
                )
                .with_reason_code("rch.no_admissible_workers")
                .with_reason_code("rch.remote_cargo_reached_false"),
            ),
        ]);

        for classification in [
            AttentionRouterClassification::ReadyNow,
            AttentionRouterClassification::BlockedInfra,
            AttentionRouterClassification::BlockedDomain,
            AttentionRouterClassification::WaitingComm,
            AttentionRouterClassification::StaleClaim,
            AttentionRouterClassification::DirtyOverlap,
            AttentionRouterClassification::ProofStarved,
            AttentionRouterClassification::DoNotTouch,
        ] {
            assert!(
                snapshot
                    .items
                    .iter()
                    .any(|item| item.classification == classification),
                "missing classification {classification:?}"
            );
        }
    }

    #[test]
    fn scoring_fails_closed_on_stale_bv_command_hints() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "beads.ready",
                AttentionRouterSourceKind::Beads,
                AttentionRouterSourceHealth::Available,
                "br ready --json + bv --robot-next",
                "bv command hints conflict with br state",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsReady,
                    "ready work exists",
                )
                .with_bead_id("ft-ready")
                .with_reason_code("beads.ready_available"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BvRecommendationConflict,
                    "bv emitted stale bd command hints",
                )
                .with_bead_id("ft-blocked")
                .with_reason_code("bv.stale_command_hints")
                .with_reason_code("bv.uses_legacy_bd"),
            ),
        ]);

        let next = snapshot
            .items
            .first()
            .expect("stale bv command hint should produce an item");
        assert_eq!(
            next.classification,
            AttentionRouterClassification::DoNotTouch
        );
        assert_eq!(
            next.recommended_action.action,
            AttentionRouterSafeAction::IgnoreBvCommandHintsUseBrJsonState
        );
        assert!(
            snapshot
                .items
                .iter()
                .all(|item| !item.recommended_action.mutates)
        );
    }

    #[test]
    fn scoring_treats_rch_local_fallback_as_proof_starved() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "rch.proof",
                AttentionRouterSourceKind::Rch,
                AttentionRouterSourceHealth::Degraded,
                "rch --no-self-healing exec",
                "[RCH] local fallback worker=null",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::RchRemoteDryRun,
                    "remote-required proof did not reach a worker",
                )
                .with_reason_code("local_cargo_not_proof"),
            ),
        ]);

        let next = snapshot
            .items
            .first()
            .expect("local RCH fallback should be attention");
        assert_eq!(
            next.classification,
            AttentionRouterClassification::ProofStarved
        );
        assert_eq!(
            next.recommended_action.action,
            AttentionRouterSafeAction::KeepProofRequiredBeadOpenOrBlockedAndRecordRchReasonCode
        );
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("local cargo"))
        );
    }

    #[test]
    fn scoring_ack_required_precedes_ready_work() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "beads.ready",
                AttentionRouterSourceKind::Beads,
                AttentionRouterSourceHealth::Available,
                "br ready --json",
                "ready work",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsReady,
                    "ready work",
                )
                .with_bead_id("ft-ready"),
            ),
            AttentionRouterSourceObservation::new(
                "agent_mail.inbox",
                AttentionRouterSourceKind::AgentMail,
                AttentionRouterSourceHealth::Available,
                "fetch_inbox",
                "ack-required message",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::AgentMailAckRequired,
                    "ack required",
                )
                .with_reason_code("agent_mail.ack_required"),
            ),
        ]);

        assert_eq!(
            snapshot.next_action.expect("next action").action,
            AttentionRouterSafeAction::AcknowledgeMessageThenReplyOrContinueBasedOnRequest
        );
    }

    #[test]
    fn nudge_plan_receipts_distinguish_communication_and_escalation_paths() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "agent_mail.inbox",
                AttentionRouterSourceKind::AgentMail,
                AttentionRouterSourceHealth::Available,
                "fetch_inbox",
                "coordination metadata",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::AgentMailAckRequired,
                    "ack required direct request",
                )
                .with_agent_name("SapphireCardinal")
                .with_reason_code("agent_mail.ack_required"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::AgentMailRecentMessages,
                    "thread reply required for open coordination",
                )
                .with_bead_id("ft-x3nsb.5")
                .with_agent_name("IvoryCreek")
                .with_reason_code("coordination.thread_reply"),
            ),
            AttentionRouterSourceObservation::new(
                "beads.in_progress",
                AttentionRouterSourceKind::Beads,
                AttentionRouterSourceHealth::Available,
                "br list --status in_progress --json",
                "stale claims",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsInProgress,
                    "candidate has no recent update",
                )
                .with_bead_id("ft-stale")
                .with_reason_code("beads.no_recent_update"),
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsInProgress,
                    "status check already sent and force-release review may be needed",
                )
                .with_bead_id("ft-stale-review")
                .with_reason_code("beads.no_recent_update")
                .with_reason_code("agent_mail.status_check_sent")
                .with_reason_code("force_release_review"),
            ),
        ]);

        let ack = item_with_nudge(&snapshot, AttentionRouterNudgeKind::AcknowledgeRequest);
        for item in &snapshot.items {
            assert_eq!(
                item.nudge_plan_receipt.nudge.command_hint,
                item.nudge_plan_receipt.nudge.safe_command_text
            );
            assert!(!item.nudge_plan_receipt.live_mutation_allowed);
            assert!(!item.nudge_plan_receipt.side_effects_executed);
            for forbidden_action in [
                "agent_mail_repair",
                "agent_mail_restart",
                "rch_service_restart",
                "delete_files",
                "delete_targets",
                "stash_or_revert_unrelated_dirty_work",
                "edit_overlapping_dirty_paths",
                "local_cargo_proof",
            ] {
                assert!(
                    item.nudge_plan_receipt
                        .forbidden_actions
                        .iter()
                        .any(|action| action == forbidden_action),
                    "nudge receipt {} missing global forbidden action {forbidden_action}",
                    item.nudge_plan_receipt.receipt_id
                );
            }
        }
        assert_eq!(
            ack.nudge_plan_receipt.schema,
            ATTENTION_ROUTER_NUDGE_PLAN_RECEIPT_SCHEMA
        );
        assert_eq!(
            ack.nudge_plan_receipt.contract_id,
            ATTENTION_ROUTER_NUDGE_PLAN_RECEIPTS_CONTRACT_ID
        );
        assert_eq!(
            ack.nudge_plan_receipt.recipient.as_deref(),
            Some("SapphireCardinal")
        );
        assert_eq!(
            ack.nudge_plan_receipt.target.kind,
            AttentionRouterNudgeTargetKind::Thread
        );
        assert_eq!(
            ack.nudge_plan_receipt.nudge.urgency,
            AttentionRouterNudgeUrgency::High
        );
        assert!(!ack.nudge_plan_receipt.nudge.review_required);
        assert_eq!(
            ack.nudge_plan_receipt.redaction.body_handling,
            AttentionRouterNudgeBodyHandling::MetadataOnly
        );

        let reply = item_with_nudge(&snapshot, AttentionRouterNudgeKind::ReplyToThread);
        assert_eq!(
            reply.recommended_action.action,
            AttentionRouterSafeAction::ReplyToThreadWithBoundedContext
        );
        assert_eq!(
            reply.nudge_plan_receipt.recipient.as_deref(),
            Some("IvoryCreek")
        );
        assert!(
            reply
                .nudge_plan_receipt
                .nudge
                .safe_command_text
                .contains("draft reply_message")
        );

        let status = item_with_nudge(&snapshot, AttentionRouterNudgeKind::StatusCheck);
        assert_eq!(
            status.nudge_plan_receipt.recipient.as_deref(),
            Some("bead-thread:ft-stale")
        );
        assert_eq!(
            status.nudge_plan_receipt.target.kind,
            AttentionRouterNudgeTargetKind::Bead
        );
        assert_eq!(
            status
                .nudge_plan_receipt
                .escalation
                .minimum_evidence_sources,
            3
        );
        assert_eq!(
            status
                .nudge_plan_receipt
                .escalation
                .minimum_wait_minutes_after_status_check,
            30
        );

        let review = item_with_nudge(&snapshot, AttentionRouterNudgeKind::ForceReleaseReview);
        assert_eq!(
            review.nudge_plan_receipt.recipient.as_deref(),
            Some("operator-review")
        );
        assert_eq!(
            review.nudge_plan_receipt.target.kind,
            AttentionRouterNudgeTargetKind::Operator
        );
        assert!(review.nudge_plan_receipt.nudge.review_required);
        assert_eq!(
            review
                .nudge_plan_receipt
                .escalation
                .minimum_evidence_sources,
            4
        );
        assert!(
            review
                .nudge_plan_receipt
                .nudge
                .safe_command_text
                .contains("do not reopen or release automatically")
        );
    }

    #[test]
    fn nudge_plan_receipts_handle_agent_mail_fallback_without_repair_or_mutation() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "agent_mail.fallback",
                AttentionRouterSourceKind::AgentMail,
                AttentionRouterSourceHealth::Unavailable,
                "scripts/swarm-tick.sh --agent-mail-fallback frankenterm",
                "Agent Mail unavailable; using Beads/git fallback metadata",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::AgentMailFallbackState,
                    "mail unavailable fallback is active",
                )
                .with_reason_code("agent_mail.unavailable")
                .with_reason_code("agent_mail.fallback_state"),
            ),
        ]);

        let item = item_with_nudge(&snapshot, AttentionRouterNudgeKind::NoAction);
        assert_eq!(
            item.classification,
            AttentionRouterClassification::BlockedInfra
        );
        assert_eq!(item.nudge_plan_receipt.recipient, None);
        assert!(!item.nudge_plan_receipt.live_mutation_allowed);
        assert!(!item.nudge_plan_receipt.side_effects_executed);
        assert!(!item.nudge_plan_receipt.nudge.mutates);
        assert!(
            item.nudge_plan_receipt
                .forbidden_actions
                .iter()
                .any(|action| action == "agent_mail_repair")
        );
        assert!(
            item.nudge_plan_receipt
                .forbidden_actions
                .iter()
                .any(|action| action == "agent_mail_restart")
        );
    }

    #[test]
    fn scoring_codex_placeholder_caveat_does_not_mark_stale() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "pane_state.live",
                AttentionRouterSourceKind::PaneState,
                AttentionRouterSourceHealth::Available,
                "ft robot state --format toon",
                "Codex idle placeholder visible",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::PaneCodexPlaceholderCaveat,
                    "Codex placeholder text is caveat evidence, not stuck evidence",
                )
                .with_reason_code("pane_state.codex_placeholder_caveat"),
            ),
        ]);

        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.classification == AttentionRouterClassification::StaleClaim)
        );
    }

    #[test]
    fn scoring_missing_required_source_blocks_infra_but_optional_pane_does_not() {
        let snapshot = build_attention_router_snapshot(&AttentionRouterSourceAdapterInput::new(
            1_770_000_200_000,
            "/repo",
        ));

        assert!(snapshot.items.iter().any(|item| {
            item.classification == AttentionRouterClassification::BlockedInfra
                && item.kind == AttentionRouterItemKind::SourceHealth
        }));
        assert!(!snapshot.items.iter().any(|item| {
            item.evidence.iter().any(|evidence| {
                evidence.fact == AttentionRouterSourceFactKind::SourceNotConfigured
                    && evidence.source_kind == AttentionRouterSourceKind::PaneState
            })
        }));
        let item_ids = snapshot
            .items
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            item_ids.len(),
            snapshot.items.len(),
            "source-health attention item ids must be unique"
        );
    }

    #[test]
    fn scoring_order_is_deterministic_when_source_order_changes() {
        let beads = AttentionRouterSourceObservation::new(
            "beads.ready",
            AttentionRouterSourceKind::Beads,
            AttentionRouterSourceHealth::Available,
            "br ready --json",
            "ready work",
        )
        .with_fact(
            AttentionRouterSourceFact::new(AttentionRouterSourceFactKind::BeadsReady, "ready work")
                .with_bead_id("ft-ready"),
        );
        let rch = AttentionRouterSourceObservation::new(
            "rch.dry_run",
            AttentionRouterSourceKind::Rch,
            AttentionRouterSourceHealth::Degraded,
            "rch dry-run",
            "no admissible workers",
        )
        .with_fact(
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::RchProofStarvation,
                "proof starved",
            )
            .with_reason_code("rch.no_admissible_workers"),
        );

        let forward = score(vec![beads.clone(), rch.clone()]);
        let reverse = score(vec![rch, beads]);

        assert_eq!(
            forward
                .items
                .iter()
                .map(|item| &item.item_id)
                .collect::<Vec<_>>(),
            reverse
                .items
                .iter()
                .map(|item| &item.item_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(forward.next_action, reverse.next_action);
    }

    #[test]
    fn scoring_snapshot_json_and_toon_preserve_semantics() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "beads.ready",
                AttentionRouterSourceKind::Beads,
                AttentionRouterSourceHealth::Available,
                "br ready --json",
                "docs-only ready static slice",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsReady,
                    "docs-only ready static slice",
                )
                .with_bead_id("ft-docs")
                .with_reason_code("beads.ready_available"),
            ),
        ]);
        let json_value = serde_json::to_value(&snapshot).expect("snapshot serializes");
        let toon = toon_rust::encode(json_value.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).expect("snapshot TOON decodes");
        let decoded_json =
            toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
        let roundtripped: serde_json::Value =
            serde_json::from_str(&decoded_json).expect("decoded TOON is JSON");

        assert_eq!(
            roundtripped["contract_id"].as_str(),
            Some(ATTENTION_ROUTER_CONTRACT_ID)
        );
        assert_eq!(
            roundtripped["schema"].as_str(),
            Some(ATTENTION_ROUTER_SNAPSHOT_SCHEMA)
        );
        assert_eq!(roundtripped["side_effects_executed"].as_bool(), Some(false));
        assert_eq!(
            roundtripped["items"][0]["recommended_action"]["mutates"].as_bool(),
            Some(false)
        );
        assert_eq!(
            roundtripped["items"][0]["nudge_plan_receipt"]["nudge"]["mutates"].as_bool(),
            Some(false)
        );
        assert_eq!(
            roundtripped["items"][0]["nudge_plan_receipt"]["live_mutation_allowed"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn surface_payload_preserves_read_only_status_next_explain_contract() {
        let input = complete_input(vec![
            AttentionRouterSourceObservation::new(
                "beads.ready",
                AttentionRouterSourceKind::Beads,
                AttentionRouterSourceHealth::Available,
                "br ready --json",
                "docs-only ready static slice",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::BeadsReady,
                    "docs-only ready static slice",
                )
                .with_bead_id("ft-docs")
                .with_reason_code("beads.ready_available"),
            ),
        ]);

        let status = build_attention_router_surface_payload(
            &input,
            AttentionRouterSurface::Status,
            "test.status",
            None,
        );
        assert_eq!(status.schema, ATTENTION_ROUTER_SURFACE_SCHEMA);
        assert_eq!(status.surface, AttentionRouterSurface::Status);
        assert!(status.dry_run);
        assert!(!status.raw_pane_content_stored);
        assert!(!status.raw_message_bodies_stored);
        assert!(!status.live_mutation_allowed);
        assert!(!status.side_effects_executed);
        assert!(status.selected_item.is_none());
        assert_eq!(
            status.mcp_resources[0].uri.as_deref(),
            Some(ATTENTION_ROUTER_MCP_CURRENT_URI)
        );
        assert_eq!(
            status.mcp_resources[1].uri_template.as_deref(),
            Some(ATTENTION_ROUTER_MCP_ITEM_URI_TEMPLATE)
        );

        let next = build_attention_router_surface_payload(
            &input,
            AttentionRouterSurface::Next,
            "test.next",
            None,
        );
        let item = next.selected_item.as_ref().expect("next item");
        assert_eq!(item.item_id, "attention:ready_now:beads_ready:ft-docs");
        assert!(!item.recommended_action.mutates);
        assert_eq!(
            item.nudge_plan_receipt.nudge.kind,
            AttentionRouterNudgeKind::NoAction
        );
        assert!(!item.nudge_plan_receipt.nudge.mutates);
        assert_eq!(
            next.explanation.status,
            AttentionRouterSurfaceLookupStatus::NextItem
        );

        let explain = build_attention_router_surface_payload(
            &input,
            AttentionRouterSurface::Explain,
            "test.explain",
            Some("attention:ready_now:beads_ready:ft-docs"),
        );
        assert!(explain.explanation.matched);
        assert_eq!(
            explain.explanation.status,
            AttentionRouterSurfaceLookupStatus::Matched
        );
        assert_eq!(
            explain.selected_item.expect("explained item").item_id,
            "attention:ready_now:beads_ready:ft-docs"
        );
    }

    #[test]
    fn surface_payload_marks_no_input_mode_degraded_and_toon_safe() {
        let payload = build_attention_router_surface_payload(
            &AttentionRouterSourceAdapterInput::new(1_770_000_400_000, "/repo"),
            AttentionRouterSurface::Status,
            "test.no_input",
            None,
        );

        assert!(payload.degraded_mode.active);
        assert!(
            payload
                .degraded_mode
                .reason_codes
                .iter()
                .any(|reason| reason == "source.beads.missing")
        );
        let json_value = serde_json::to_value(&payload).expect("payload serializes");
        let toon = toon_rust::encode(json_value.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).expect("surface TOON decodes");
        let decoded_json =
            toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
        let roundtripped: serde_json::Value =
            serde_json::from_str(&decoded_json).expect("decoded TOON is JSON");

        assert_eq!(
            roundtripped["contract_id"].as_str(),
            Some(ATTENTION_ROUTER_CONTRACT_ID)
        );
        assert_eq!(
            roundtripped["schema"].as_str(),
            Some(ATTENTION_ROUTER_SURFACE_SCHEMA)
        );
        assert_eq!(roundtripped["dry_run"].as_bool(), Some(true));
        assert_eq!(roundtripped["live_mutation_allowed"].as_bool(), Some(false));
        assert_eq!(
            roundtripped["mcp_resources"][1]["uri_template"].as_str(),
            Some(ATTENTION_ROUTER_MCP_ITEM_URI_TEMPLATE)
        );
    }

    #[test]
    fn retained_surface_goldens_match_read_only_contract_invariants() {
        let input: AttentionRouterSourceAdapterInput = serde_json::from_str(include_str!(
            "../../../fixtures/attention-router/source-adapter-input.ready.v1.json"
        ))
        .expect("adapter input fixture parses");
        let payload = build_attention_router_surface_payload(
            &input,
            AttentionRouterSurface::Status,
            "cli.attention.status",
            None,
        );
        let live = serde_json::to_value(&payload).expect("payload serializes");
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/attention-router/surface-status.golden.json"
        ))
        .expect("JSON golden parses");

        for key in [
            "schema",
            "contract_id",
            "surface",
            "generated_at_ms",
            "workspace",
            "source",
            "dry_run",
            "raw_pane_content_stored",
            "raw_message_bodies_stored",
            "live_mutation_allowed",
            "side_effects_executed",
        ] {
            assert_eq!(golden[key], live[key], "golden field {key} drifted");
        }
        assert_eq!(
            golden["degraded_mode"]["active"],
            live["degraded_mode"]["active"]
        );
        assert_eq!(
            golden["next_action"], live["next_action"],
            "next action drifted"
        );
        for reason in golden["degraded_mode"]["reason_codes"]
            .as_array()
            .expect("golden reason code array")
        {
            assert!(
                live["degraded_mode"]["reason_codes"]
                    .as_array()
                    .expect("live reason code array")
                    .contains(reason),
                "missing retained degraded reason {reason:?}"
            );
        }
        assert_eq!(
            golden["snapshot"]["schema"], live["snapshot"]["schema"],
            "snapshot schema drifted"
        );
        assert_eq!(
            golden["snapshot"]["contract_id"], live["snapshot"]["contract_id"],
            "snapshot contract drifted"
        );
        assert_eq!(
            golden["snapshot"]["side_effects_executed"], live["snapshot"]["side_effects_executed"],
            "snapshot side-effect flag drifted"
        );
        assert_eq!(
            golden["snapshot"]["items"][0]["item_id"], live["snapshot"]["items"][0]["item_id"],
            "golden item id drifted"
        );
        assert_eq!(
            golden["snapshot"]["items"][0]["recommended_action"],
            live["snapshot"]["items"][0]["recommended_action"],
            "golden item recommendation drifted"
        );
        assert_eq!(
            golden["snapshot"]["items"][0]["nudge_plan_receipt"]["nudge"],
            live["snapshot"]["items"][0]["nudge_plan_receipt"]["nudge"],
            "golden item nudge action drifted"
        );
        assert_eq!(
            golden["snapshot"]["items"][0]["nudge_plan_receipt"]["live_mutation_allowed"],
            live["snapshot"]["items"][0]["nudge_plan_receipt"]["live_mutation_allowed"],
            "golden item nudge mutation gate drifted"
        );
        for resource in golden["mcp_resources"]
            .as_array()
            .expect("golden resource array")
        {
            assert!(
                live["mcp_resources"]
                    .as_array()
                    .expect("live resource array")
                    .iter()
                    .any(|candidate| {
                        candidate["uri"] == resource["uri"]
                            && candidate["uri_template"] == resource["uri_template"]
                            && candidate["read_only"] == resource["read_only"]
                            && candidate["live_mutation_allowed"]
                                == resource["live_mutation_allowed"]
                    }),
                "missing retained MCP resource {resource:?}"
            );
        }

        let toon = include_str!("../../../fixtures/attention-router/surface-status.golden.toon");
        for needle in [
            ATTENTION_ROUTER_SURFACE_SCHEMA,
            ATTENTION_ROUTER_CONTRACT_ID,
            "surface: status",
            "attention:ready_now:beads_ready:ft-docs",
            "br show ft-docs --json",
            "nudge_plan_receipt",
            ATTENTION_ROUTER_NUDGE_PLAN_RECEIPT_SCHEMA,
            ATTENTION_ROUTER_NUDGE_PLAN_RECEIPTS_CONTRACT_ID,
            "kind: no_action",
            ATTENTION_ROUTER_MCP_CURRENT_URI,
            ATTENTION_ROUTER_MCP_ITEM_URI_TEMPLATE,
            "side_effects_executed: false",
        ] {
            assert!(toon.contains(needle), "TOON golden missing {needle}");
        }
    }

    #[test]
    fn scoring_recommendations_do_not_embed_forbidden_service_mutation_strings() {
        let snapshot = score(vec![
            AttentionRouterSourceObservation::new(
                "rch.dry_run",
                AttentionRouterSourceKind::Rch,
                AttentionRouterSourceHealth::Degraded,
                "rch dry-run",
                "no admissible workers",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::RchProofStarvation,
                    "proof starved",
                )
                .with_reason_code("rch.no_admissible_workers"),
            ),
            AttentionRouterSourceObservation::new(
                "agent_mail.inbox",
                AttentionRouterSourceKind::AgentMail,
                AttentionRouterSourceHealth::Available,
                "fetch_inbox",
                "ack required",
            )
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::AgentMailAckRequired,
                    "ack required",
                )
                .with_reason_code("agent_mail.ack_required"),
            ),
        ]);
        let text = serde_json::to_string(&snapshot).expect("snapshot serializes");

        for forbidden in [
            concat!("am service ", "restart"),
            concat!("am service ", "stop"),
            concat!("am doctor ", "fix"),
            concat!("am doctor ", "repair"),
            concat!("git reset ", "--hard"),
            concat!("git clean ", "-fd"),
            concat!("rm ", "-rf"),
            concat!("local cargo ", "proof"),
        ] {
            assert!(
                !text.contains(forbidden),
                "recommendation text must not embed forbidden command {forbidden}"
            );
        }
    }
}
