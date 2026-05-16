//! Side-effect-free swarm operating-envelope planning.
//!
//! The planner consumes already-collected, redacted coordination facts and
//! returns ranked admission windows. It does not shell out, claim Beads, mutate
//! panes, repair services, cancel RCH work, run Cargo, or inspect pane text.

#![allow(clippy::similar_names)]

use serde::{Deserialize, Serialize};

pub const OPERATING_ENVELOPE_CONTRACT_ID: &str = "ft.operating_envelope.v1";
pub const OPERATING_ENVELOPE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeControllerMode {
    DryRun,
    AdmissionPreview,
    AttestationPreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeTargetProfile {
    DeveloperLaptop,
    Target64Core256g,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeProofState {
    Measured,
    Simulated,
    Stale,
    Unavailable,
    SkippedNotProven,
    NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeSourceKind {
    CapacityResource,
    Rch,
    Beads,
    AgentMail,
    Git,
    RobotInventory,
    BlockerRadar,
    Manual,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeSourceState {
    Available,
    Degraded,
    Unavailable,
    Stale,
    Blocked,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeFreshnessState {
    Fresh,
    WithinBudget,
    Stale,
    Unknown,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeEvidenceLevel {
    LiveCommand,
    RetainedArtifact,
    Fixture,
    ManualNote,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeRedactionState {
    NotCollected,
    Redacted,
    RawForbidden,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeEvidenceCategory {
    CapacityPressure,
    TargetHardware,
    RchWorkerSelection,
    RchActiveProjectExclusion,
    RchTopologyPreflight,
    RchCargoReached,
    BeadsReadyQueue,
    BeadsBlockedQueue,
    BeadsInProgress,
    BeadsStaleCandidate,
    AgentMailAvailability,
    GitDirtyTree,
    GitUntrackedPaths,
    DeletionRisk,
    DirtyPathOverlap,
    RobotInventory,
    RedactionPosture,
    ManualNote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeEvidenceState {
    Pass,
    Warn,
    Fail,
    Blocked,
    Unavailable,
    Stale,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeOutcome {
    Admit,
    Defer,
    Degrade,
    Shed,
    Block,
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeTier {
    Green,
    Yellow,
    Orange,
    Red,
    Black,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeConfidence {
    Proven,
    Measured,
    Mixed,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeDirtyTreeState {
    Clean,
    DirtyNonOverlap,
    DirtyOverlap,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeWindowClass {
    Open,
    ReadOnly,
    StaticOnly,
    RchProofAllowed,
    ClaimAllowed,
    Defer,
    Wait,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeActionClass {
    ReadStatus,
    ClaimBead,
    AddBeadsComment,
    CreateBead,
    EditFiles,
    RunStaticCheck,
    RunRchProof,
    RequestApproval,
    Wait,
    AgentMailRepair,
    BuildCancellation,
    LocalCargoProof,
    RawPaneContent,
    RawPaneContentCapture,
    RchDaemonRestart,
    ServiceRestart,
    WorkerDrain,
    PaneMutation,
    ServiceMutation,
    DestructiveFilesystem,
    DestructiveGit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeTargetClass {
    pub profile: OperatingEnvelopeTargetProfile,
    pub requested_agent_count: u32,
    pub requested_cpu_cores: u32,
    pub requested_memory_gib: u32,
    pub proof_state: OperatingEnvelopeProofState,
    pub reason_codes: Vec<String>,
}

impl OperatingEnvelopeTargetClass {
    #[must_use]
    pub fn developer_laptop() -> Self {
        Self {
            profile: OperatingEnvelopeTargetProfile::DeveloperLaptop,
            requested_agent_count: 8,
            requested_cpu_cores: 8,
            requested_memory_gib: 32,
            proof_state: OperatingEnvelopeProofState::NotRequired,
            reason_codes: vec!["target_hardware.not_required".to_string()],
        }
    }

    #[must_use]
    pub fn target_64_core_256g() -> Self {
        Self {
            profile: OperatingEnvelopeTargetProfile::Target64Core256g,
            requested_agent_count: 64,
            requested_cpu_cores: 64,
            requested_memory_gib: 256,
            proof_state: OperatingEnvelopeProofState::SkippedNotProven,
            reason_codes: vec!["target_hardware.skipped_not_proven".to_string()],
        }
    }

    #[must_use]
    pub fn proof_state(mut self, proof_state: OperatingEnvelopeProofState) -> Self {
        self.proof_state = proof_state;
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeRedactionPosture {
    pub pane_content: OperatingEnvelopeRedactionState,
    pub secret_material: OperatingEnvelopeRedactionState,
    pub notes: Vec<String>,
}

impl Default for OperatingEnvelopeRedactionPosture {
    fn default() -> Self {
        Self {
            pane_content: OperatingEnvelopeRedactionState::NotCollected,
            secret_material: OperatingEnvelopeRedactionState::NotCollected,
            notes: vec!["source summary only".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeEvidenceItem {
    pub evidence_id: String,
    pub category: OperatingEnvelopeEvidenceCategory,
    pub state: OperatingEnvelopeEvidenceState,
    pub subject: String,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopeEvidenceItem {
    #[must_use]
    pub fn new(
        category: OperatingEnvelopeEvidenceCategory,
        state: OperatingEnvelopeEvidenceState,
        subject: impl Into<String>,
    ) -> Self {
        let subject = bounded_string(subject, "unknown");
        Self {
            evidence_id: format!("evidence-{}", slug(&subject)),
            category,
            state,
            subject,
            reason_codes: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, path: impl Into<String>) -> Self {
        push_unique(&mut self.artifact_paths, path);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeSourceSnapshot {
    pub source_id: String,
    pub source_kind: OperatingEnvelopeSourceKind,
    pub state: OperatingEnvelopeSourceState,
    pub freshness_state: OperatingEnvelopeFreshnessState,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub evidence_level: OperatingEnvelopeEvidenceLevel,
    pub redacted: bool,
    pub redaction_posture: OperatingEnvelopeRedactionPosture,
    pub raw_pane_content_stored: bool,
    pub reason_codes: Vec<String>,
    pub unavailable_reason: Option<String>,
    pub degraded_reason: Option<String>,
    pub evidence: Vec<OperatingEnvelopeEvidenceItem>,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopeSourceSnapshot {
    #[must_use]
    pub fn new(source_id: impl Into<String>, source_kind: OperatingEnvelopeSourceKind) -> Self {
        let source_id = bounded_string(source_id, "source.unknown");
        Self {
            source_id: source_id.clone(),
            source_kind,
            state: OperatingEnvelopeSourceState::Available,
            freshness_state: OperatingEnvelopeFreshnessState::Fresh,
            collected_at_ms: None,
            freshness_ms: None,
            command_or_api: "operating_envelope.core".to_string(),
            evidence_level: OperatingEnvelopeEvidenceLevel::ManualNote,
            redacted: true,
            redaction_posture: OperatingEnvelopeRedactionPosture::default(),
            raw_pane_content_stored: false,
            reason_codes: Vec::new(),
            unavailable_reason: None,
            degraded_reason: None,
            evidence: vec![OperatingEnvelopeEvidenceItem::new(
                OperatingEnvelopeEvidenceCategory::ManualNote,
                OperatingEnvelopeEvidenceState::Pass,
                source_id,
            )],
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn state(mut self, state: OperatingEnvelopeSourceState) -> Self {
        self.state = state;
        self
    }

    #[must_use]
    pub fn evidence_level(mut self, evidence_level: OperatingEnvelopeEvidenceLevel) -> Self {
        self.evidence_level = evidence_level;
        self
    }

    #[must_use]
    pub fn unavailable(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Unavailable;
        let reason_code = reason_code.into();
        push_unique(&mut self.reason_codes, reason_code.clone());
        self.unavailable_reason = Some(reason_code);
        self
    }

    #[must_use]
    pub fn degraded(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Degraded;
        let reason_code = reason_code.into();
        push_unique(&mut self.reason_codes, reason_code.clone());
        self.degraded_reason = Some(reason_code);
        self
    }

    #[must_use]
    pub fn blocked(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Blocked;
        let reason_code = reason_code.into();
        push_unique(&mut self.reason_codes, reason_code.clone());
        self.degraded_reason = Some(reason_code);
        self
    }

    #[must_use]
    pub fn stale(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Stale;
        self.freshness_state = OperatingEnvelopeFreshnessState::Stale;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn not_collected(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::NotCollected;
        self.freshness_state = OperatingEnvelopeFreshnessState::NotCollected;
        self.evidence_level = OperatingEnvelopeEvidenceLevel::NotCollected;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: OperatingEnvelopeEvidenceItem) -> Self {
        self.evidence.push(evidence);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, path: impl Into<String>) -> Self {
        push_unique(&mut self.artifact_paths, path);
        self
    }

    #[must_use]
    pub fn raw_pane_content_stored(mut self) -> Self {
        self.raw_pane_content_stored = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeInputDomains {
    pub capacity_resource: OperatingEnvelopeSourceSnapshot,
    pub rch: OperatingEnvelopeSourceSnapshot,
    pub beads: OperatingEnvelopeSourceSnapshot,
    pub agent_mail: OperatingEnvelopeSourceSnapshot,
    pub git: OperatingEnvelopeSourceSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub robot_inventory: Option<OperatingEnvelopeSourceSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeBudgets {
    pub interactive_agents: u32,
    pub long_rch_proof_lanes: u32,
    pub gui_renderer_stress_jobs: u32,
    pub docs_static_checks: u32,
    pub no_mock_e2e_harnesses: u32,
}

impl OperatingEnvelopeBudgets {
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            interactive_agents: 4,
            long_rch_proof_lanes: 1,
            gui_renderer_stress_jobs: 0,
            docs_static_checks: 2,
            no_mock_e2e_harnesses: 0,
        }
    }

    #[must_use]
    pub const fn target_class() -> Self {
        Self {
            interactive_agents: 32,
            long_rch_proof_lanes: 8,
            gui_renderer_stress_jobs: 2,
            docs_static_checks: 8,
            no_mock_e2e_harnesses: 2,
        }
    }
}

impl Default for OperatingEnvelopeBudgets {
    fn default() -> Self {
        Self::conservative()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopePlannerInput {
    pub generated_at_ms: u64,
    pub envelope_id: String,
    pub objective_id: String,
    pub controller_mode: OperatingEnvelopeControllerMode,
    pub target_class: OperatingEnvelopeTargetClass,
    pub input_domains: OperatingEnvelopeInputDomains,
    pub budgets: OperatingEnvelopeBudgets,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopePlannerInput {
    #[must_use]
    pub fn new(
        generated_at_ms: u64,
        envelope_id: impl Into<String>,
        objective_id: impl Into<String>,
        input_domains: OperatingEnvelopeInputDomains,
    ) -> Self {
        Self {
            generated_at_ms,
            envelope_id: bounded_string(envelope_id, "envelope.unknown"),
            objective_id: bounded_string(objective_id, "objective.unknown"),
            controller_mode: OperatingEnvelopeControllerMode::DryRun,
            target_class: OperatingEnvelopeTargetClass::developer_laptop(),
            input_domains,
            budgets: OperatingEnvelopeBudgets::default(),
            artifact_paths: vec!["docs/json-schema/ft-operating-envelope.json".to_string()],
        }
    }

    #[must_use]
    pub fn target_class(mut self, target_class: OperatingEnvelopeTargetClass) -> Self {
        self.target_class = target_class;
        self
    }

    #[must_use]
    pub fn budgets(mut self, budgets: OperatingEnvelopeBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    fn sources(&self) -> Vec<&OperatingEnvelopeSourceSnapshot> {
        let mut sources = vec![
            &self.input_domains.capacity_resource,
            &self.input_domains.rch,
            &self.input_domains.beads,
            &self.input_domains.agent_mail,
            &self.input_domains.git,
        ];
        if let Some(robot_inventory) = &self.input_domains.robot_inventory {
            sources.push(robot_inventory);
        }
        sources
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeDecision {
    pub decision_id: String,
    pub outcome: OperatingEnvelopeOutcome,
    pub envelope_tier: OperatingEnvelopeTier,
    pub confidence: OperatingEnvelopeConfidence,
    pub target_hardware_state: OperatingEnvelopeProofState,
    pub rch_proof_state: OperatingEnvelopeProofState,
    pub agent_mail_state: OperatingEnvelopeSourceState,
    pub dirty_tree_state: OperatingEnvelopeDirtyTreeState,
    pub max_parallel_agents: u32,
    pub max_parallel_proofs: u32,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeAdmissionWindow {
    pub window_id: String,
    pub window_class: OperatingEnvelopeWindowClass,
    pub starts_at_ms: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub max_parallel_agents: u32,
    pub max_parallel_proofs: u32,
    pub permitted_action_classes: Vec<OperatingEnvelopeActionClass>,
    pub forbidden_action_classes: Vec<OperatingEnvelopeActionClass>,
    pub source_ids: Vec<String>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeFailClosedPolicy {
    pub missing_signal_behavior: String,
    pub stale_signal_behavior: String,
    pub contradictory_signal_behavior: String,
    pub privacy_redaction_behavior: String,
    pub target_hardware_gap_behavior: String,
    pub reason_codes: Vec<String>,
}

impl Default for OperatingEnvelopeFailClosedPolicy {
    fn default() -> Self {
        Self {
            missing_signal_behavior: "lower_envelope".to_string(),
            stale_signal_behavior: "lower_envelope".to_string(),
            contradictory_signal_behavior: "block".to_string(),
            privacy_redaction_behavior: "lower_envelope".to_string(),
            target_hardware_gap_behavior: "defer_target_class_claim".to_string(),
            reason_codes: vec![
                "fail_closed.lower_missing".to_string(),
                "fail_closed.block_contradiction".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct OperatingEnvelopeSideEffectPolicy {
    pub dry_run_only: bool,
    pub raw_pane_content_allowed: bool,
    pub pane_mutation_allowed: bool,
    pub service_mutation_allowed: bool,
    pub destructive_actions_allowed: bool,
    pub local_cargo_proof_allowed: bool,
}

impl Default for OperatingEnvelopeSideEffectPolicy {
    fn default() -> Self {
        Self {
            dry_run_only: true,
            raw_pane_content_allowed: false,
            pane_mutation_allowed: false,
            service_mutation_allowed: false,
            destructive_actions_allowed: false,
            local_cargo_proof_allowed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeRedactionPolicy {
    pub policy_id: String,
    pub raw_pane_content_allowed: bool,
    pub secret_material_allowed: bool,
    pub notes: Vec<String>,
}

impl Default for OperatingEnvelopeRedactionPolicy {
    fn default() -> Self {
        Self {
            policy_id: "ft.redaction.operating_envelope.v1".to_string(),
            raw_pane_content_allowed: false,
            secret_material_allowed: false,
            notes: vec!["Envelope stores source summaries and reason codes only.".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopePlan {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub envelope_id: String,
    pub objective_id: String,
    pub controller_mode: OperatingEnvelopeControllerMode,
    pub target_class: OperatingEnvelopeTargetClass,
    pub input_domains: OperatingEnvelopeInputDomains,
    pub decision: OperatingEnvelopeDecision,
    pub admission_windows: Vec<OperatingEnvelopeAdmissionWindow>,
    pub fail_closed_policy: OperatingEnvelopeFailClosedPolicy,
    pub side_effect_policy: OperatingEnvelopeSideEffectPolicy,
    pub redaction_policy: OperatingEnvelopeRedactionPolicy,
    pub raw_pane_content_stored: bool,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
}

#[must_use]
pub fn plan_operating_envelope(input: OperatingEnvelopePlannerInput) -> OperatingEnvelopePlan {
    let facts = EnvelopeFacts::from_input(&input);
    let decision = decision_for(&input, &facts);
    let mut admission_windows = admission_windows_for(&input, &facts, &decision);
    stable_rank_windows(&mut admission_windows);

    OperatingEnvelopePlan {
        schema_version: OPERATING_ENVELOPE_SCHEMA_VERSION,
        contract_id: OPERATING_ENVELOPE_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        envelope_id: input.envelope_id,
        objective_id: input.objective_id,
        controller_mode: input.controller_mode,
        target_class: input.target_class,
        input_domains: input.input_domains,
        decision: decision.clone(),
        admission_windows,
        fail_closed_policy: OperatingEnvelopeFailClosedPolicy::default(),
        side_effect_policy: OperatingEnvelopeSideEffectPolicy::default(),
        redaction_policy: OperatingEnvelopeRedactionPolicy::default(),
        raw_pane_content_stored: false,
        reason_codes: decision.reason_codes,
        artifact_paths: input.artifact_paths,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvelopeFacts {
    privacy_violation: bool,
    capacity_black: bool,
    capacity_red: bool,
    rch_no_worker: bool,
    rch_topology_failure: bool,
    rch_active_project_exclusion: bool,
    rch_recovered: bool,
    agent_mail_unavailable: bool,
    agent_mail_degraded: bool,
    dirty_non_overlap: bool,
    dirty_overlap: bool,
    deletion_risk: bool,
    stale_owner: bool,
    active_owner: bool,
    no_ready_work: bool,
    target_class_unproven: bool,
    stale_telemetry: bool,
    insufficient_proof: bool,
}

impl EnvelopeFacts {
    fn from_input(input: &OperatingEnvelopePlannerInput) -> Self {
        let sources = input.sources();
        Self {
            privacy_violation: sources
                .iter()
                .any(|source| !source.redacted || source.raw_pane_content_stored),
            capacity_black: any_reason(&sources, &["capacity.black", "pressure.black"]),
            capacity_red: any_reason(&sources, &["capacity.red", "pressure.red"]),
            rch_no_worker: any_reason(
                &sources,
                &[
                    "rch.no_workers_passed_health",
                    "rch.worker_selection.no_available_worker",
                ],
            ),
            rch_topology_failure: any_reason(
                &sources,
                &[
                    "rch.topology_preflight_failed",
                    "rch.topology_preflight.fail",
                ],
            ),
            rch_active_project_exclusion: any_reason(
                &sources,
                &[
                    "rch.active_project_exclusion",
                    "rch.active_project_exclusion.active_project_present",
                ],
            ),
            rch_recovered: any_reason(
                &sources,
                &[
                    "rch.remote_cargo_reached_true",
                    "rch.cargo_verdict.pass",
                    "rch.remote_proof_available",
                ],
            ),
            agent_mail_unavailable: input.input_domains.agent_mail.state
                == OperatingEnvelopeSourceState::Unavailable
                || any_reason(
                    &sources,
                    &[
                        "agent_mail.unavailable_after_retry",
                        "agent_mail.database_error",
                    ],
                ),
            agent_mail_degraded: input.input_domains.agent_mail.state
                == OperatingEnvelopeSourceState::Degraded,
            dirty_non_overlap: any_reason(
                &sources,
                &[
                    "git.dirty_non_overlap",
                    "git.dirty_nonoverlap",
                    "dirty_tree.non_overlap",
                ],
            ),
            dirty_overlap: input.input_domains.git.state == OperatingEnvelopeSourceState::Blocked
                || any_reason(
                    &sources,
                    &[
                        "dirty_overlap.present",
                        "git.dirty_overlap_risk",
                        "dirty_overlap.risk",
                    ],
                ),
            deletion_risk: any_reason(
                &sources,
                &[
                    "deletion_risk.present",
                    "deletion_risk.frankenterm_core",
                    "git.deleted_paths_present",
                ],
            ),
            stale_owner: any_reason(
                &sources,
                &["beads.stale_candidate", "beads.stale_reopen_candidate"],
            ),
            active_owner: any_reason(
                &sources,
                &["beads.in_progress_active", "assignee_overlap.active"],
            ),
            no_ready_work: any_reason(&sources, &["beads.ready_empty", "beads.no_ready_work"]),
            target_class_unproven: input.target_class.proof_state
                == OperatingEnvelopeProofState::SkippedNotProven
                || input.target_class.proof_state == OperatingEnvelopeProofState::Unavailable
                || any_reason(
                    &sources,
                    &[
                        "target_hardware.skipped_not_proven",
                        "target_class.hardware_not_available",
                        "attestation.skipped_not_proven",
                        "capacity.target_class_unproven",
                    ],
                ),
            stale_telemetry: sources
                .iter()
                .any(|source| source.freshness_state == OperatingEnvelopeFreshnessState::Stale)
                || any_reason(&sources, &["telemetry.stale", "capacity.stale"]),
            insufficient_proof: any_reason(
                &sources,
                &[
                    "proof.insufficient",
                    "rch.remote_cargo_reached_false",
                    "local_cargo.forbidden",
                ],
            ),
        }
    }
}

fn decision_for(
    input: &OperatingEnvelopePlannerInput,
    facts: &EnvelopeFacts,
) -> OperatingEnvelopeDecision {
    let mut reason_codes = Vec::new();
    let (outcome, tier, confidence, dirty_tree_state, max_agents, max_proofs) =
        if facts.privacy_violation {
            push_unique(&mut reason_codes, "source.raw_content_forbidden");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.deletion_risk {
            push_unique(&mut reason_codes, "deletion_risk.present");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.capacity_black {
            push_unique(&mut reason_codes, "capacity.black");
            (
                OperatingEnvelopeOutcome::Shed,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.rch_topology_failure {
            push_unique(&mut reason_codes, "rch.topology_preflight_failed");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.capacity_red {
            push_unique(&mut reason_codes, "capacity.red");
            (
                OperatingEnvelopeOutcome::Shed,
                OperatingEnvelopeTier::Red,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.dirty_overlap || facts.active_owner {
            if facts.dirty_overlap {
                push_unique(&mut reason_codes, "dirty_overlap.present");
            }
            if facts.active_owner {
                push_unique(&mut reason_codes, "assignee_overlap.active");
            }
            (
                OperatingEnvelopeOutcome::Wait,
                OperatingEnvelopeTier::Orange,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.rch_no_worker || facts.rch_active_project_exclusion {
            if facts.rch_no_worker {
                push_unique(&mut reason_codes, "rch.no_workers_passed_health");
            }
            if facts.rch_active_project_exclusion {
                push_unique(&mut reason_codes, "rch.active_project_exclusion");
            }
            (
                OperatingEnvelopeOutcome::Defer,
                OperatingEnvelopeTier::Red,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.target_class_unproven {
            push_unique(&mut reason_codes, "target_hardware.skipped_not_proven");
            push_unique(&mut reason_codes, "capacity.target_class_unproven");
            (
                OperatingEnvelopeOutcome::Defer,
                OperatingEnvelopeTier::Orange,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.agent_mail_unavailable || facts.agent_mail_degraded {
            if facts.agent_mail_unavailable {
                push_unique(&mut reason_codes, "agent_mail.unavailable_after_retry");
            } else {
                push_unique(&mut reason_codes, "agent_mail.degraded");
            }
            push_unique(&mut reason_codes, "fallback.beads_only");
            (
                OperatingEnvelopeOutcome::Degrade,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.interactive_agents.min(4),
                0,
            )
        } else if facts.no_ready_work {
            push_unique(&mut reason_codes, "beads.ready_empty");
            (
                OperatingEnvelopeOutcome::Wait,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.stale_owner || facts.stale_telemetry {
            if facts.stale_owner {
                push_unique(&mut reason_codes, "beads.stale_candidate");
            }
            if facts.stale_telemetry {
                push_unique(&mut reason_codes, "telemetry.stale");
            }
            (
                OperatingEnvelopeOutcome::Degrade,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Stale,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.insufficient_proof && !facts.rch_recovered {
            push_unique(&mut reason_codes, "proof.insufficient");
            (
                OperatingEnvelopeOutcome::Degrade,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else {
            push_unique(&mut reason_codes, "envelope.all_required_sources_available");
            (
                OperatingEnvelopeOutcome::Admit,
                OperatingEnvelopeTier::Green,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                input.budgets.interactive_agents,
                input.budgets.long_rch_proof_lanes,
            )
        };

    push_unique(&mut reason_codes, "policy.no_local_cargo_proof");
    push_unique(&mut reason_codes, "source.redacted_summary_only");

    OperatingEnvelopeDecision {
        decision_id: format!("decision-{}", slug(&input.envelope_id)),
        outcome,
        envelope_tier: tier,
        confidence,
        target_hardware_state: input.target_class.proof_state,
        rch_proof_state: rch_proof_state(facts),
        agent_mail_state: input.input_domains.agent_mail.state,
        dirty_tree_state,
        max_parallel_agents: max_agents,
        max_parallel_proofs: max_proofs,
        reason_codes,
    }
}

fn admission_windows_for(
    input: &OperatingEnvelopePlannerInput,
    facts: &EnvelopeFacts,
    decision: &OperatingEnvelopeDecision,
) -> Vec<OperatingEnvelopeAdmissionWindow> {
    match decision.outcome {
        OperatingEnvelopeOutcome::Admit => vec![
            window(
                "admit_now",
                OperatingEnvelopeWindowClass::Open,
                input.generated_at_ms,
                Some(input.generated_at_ms.saturating_add(300_000)),
                decision.max_parallel_agents,
                decision.max_parallel_proofs,
                vec![
                    OperatingEnvelopeActionClass::ReadStatus,
                    OperatingEnvelopeActionClass::ClaimBead,
                    OperatingEnvelopeActionClass::AddBeadsComment,
                    OperatingEnvelopeActionClass::EditFiles,
                    OperatingEnvelopeActionClass::RunStaticCheck,
                    OperatingEnvelopeActionClass::RunRchProof,
                ],
                vec![
                    "envelope.admit",
                    "rch.remote_cargo_reached_true",
                    "proof.remote_only",
                ],
                input,
            ),
            proof_window("proof_only", input, input.budgets.long_rch_proof_lanes),
            static_window("docs_only", input, input.budgets.docs_static_checks),
        ],
        OperatingEnvelopeOutcome::Degrade => {
            let mut windows = vec![static_window(
                "docs_only",
                input,
                decision
                    .max_parallel_agents
                    .max(input.budgets.docs_static_checks),
            )];
            if facts.agent_mail_unavailable || facts.agent_mail_degraded {
                windows.push(wait_window(
                    "admit_after_agent_mail_recovers",
                    input,
                    &["agent_mail.recovery_required", "fallback.beads_only"],
                ));
            }
            if facts.stale_owner {
                windows.push(read_only_window(
                    "stale_reopen_status_check",
                    input,
                    &["beads.stale_candidate", "coordination.status_check_first"],
                ));
            }
            windows
        }
        OperatingEnvelopeOutcome::Defer => vec![
            static_window("docs_only", input, input.budgets.docs_static_checks),
            wait_window(
                "admit_after_rch_recovers",
                input,
                &["rch.recovery_required", "proof.remote_unavailable"],
            ),
        ],
        OperatingEnvelopeOutcome::Wait => vec![wait_window(
            "pause_admission",
            input,
            &wait_reason_codes(facts),
        )],
        OperatingEnvelopeOutcome::Shed => vec![window(
            "emergency_stop_recommended",
            OperatingEnvelopeWindowClass::Blocked,
            input.generated_at_ms,
            None,
            0,
            0,
            vec![OperatingEnvelopeActionClass::ReadStatus],
            vec!["capacity.pressure_shed", "envelope.shed"],
            input,
        )],
        OperatingEnvelopeOutcome::Block => vec![window(
            "emergency_stop_recommended",
            OperatingEnvelopeWindowClass::Blocked,
            input.generated_at_ms,
            None,
            0,
            0,
            vec![
                OperatingEnvelopeActionClass::ReadStatus,
                OperatingEnvelopeActionClass::AddBeadsComment,
            ],
            decision
                .reason_codes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            input,
        )],
    }
}

fn static_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    max_agents: u32,
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::StaticOnly,
        input.generated_at_ms,
        Some(input.generated_at_ms.saturating_add(300_000)),
        max_agents,
        0,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::AddBeadsComment,
            OperatingEnvelopeActionClass::RunStaticCheck,
        ],
        vec!["proof.static_only", "policy.no_local_cargo_proof"],
        input,
    )
}

fn proof_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    max_proofs: u32,
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::RchProofAllowed,
        input.generated_at_ms,
        Some(input.generated_at_ms.saturating_add(300_000)),
        input.budgets.interactive_agents.min(2),
        max_proofs,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::RunRchProof,
        ],
        vec!["proof.remote_only", "rch.remote_cargo_reached_true"],
        input,
    )
}

fn read_only_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    reason_codes: &[&str],
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::ReadOnly,
        input.generated_at_ms,
        Some(input.generated_at_ms.saturating_add(300_000)),
        input.budgets.docs_static_checks,
        0,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::AddBeadsComment,
        ],
        reason_codes.to_vec(),
        input,
    )
}

fn wait_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    reason_codes: &[&str],
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::Wait,
        input.generated_at_ms,
        None,
        0,
        0,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::Wait,
        ],
        reason_codes.to_vec(),
        input,
    )
}

#[allow(clippy::too_many_arguments)]
fn window(
    window_id: impl Into<String>,
    window_class: OperatingEnvelopeWindowClass,
    starts_at_ms: u64,
    expires_at_ms: Option<u64>,
    max_parallel_agents: u32,
    max_parallel_proofs: u32,
    permitted_action_classes: Vec<OperatingEnvelopeActionClass>,
    reason_codes: Vec<&str>,
    input: &OperatingEnvelopePlannerInput,
) -> OperatingEnvelopeAdmissionWindow {
    OperatingEnvelopeAdmissionWindow {
        window_id: window_id.into(),
        window_class,
        starts_at_ms: Some(starts_at_ms),
        expires_at_ms,
        max_parallel_agents,
        max_parallel_proofs,
        permitted_action_classes,
        forbidden_action_classes: default_forbidden_action_classes(),
        source_ids: source_ids(input),
        reason_codes: reason_codes.into_iter().map(str::to_string).collect(),
    }
}

fn default_forbidden_action_classes() -> Vec<OperatingEnvelopeActionClass> {
    vec![
        OperatingEnvelopeActionClass::AgentMailRepair,
        OperatingEnvelopeActionClass::BuildCancellation,
        OperatingEnvelopeActionClass::DestructiveGit,
        OperatingEnvelopeActionClass::DestructiveFilesystem,
        OperatingEnvelopeActionClass::LocalCargoProof,
        OperatingEnvelopeActionClass::RawPaneContentCapture,
        OperatingEnvelopeActionClass::RawPaneContent,
        OperatingEnvelopeActionClass::RchDaemonRestart,
        OperatingEnvelopeActionClass::ServiceRestart,
        OperatingEnvelopeActionClass::ServiceMutation,
        OperatingEnvelopeActionClass::PaneMutation,
        OperatingEnvelopeActionClass::WorkerDrain,
    ]
}

fn stable_rank_windows(windows: &mut [OperatingEnvelopeAdmissionWindow]) {
    windows.sort_by_key(|window| {
        let class_rank = match window.window_class {
            OperatingEnvelopeWindowClass::Open => 0,
            OperatingEnvelopeWindowClass::RchProofAllowed => 1,
            OperatingEnvelopeWindowClass::ClaimAllowed => 2,
            OperatingEnvelopeWindowClass::StaticOnly => 3,
            OperatingEnvelopeWindowClass::ReadOnly => 4,
            OperatingEnvelopeWindowClass::Defer => 5,
            OperatingEnvelopeWindowClass::Wait => 6,
            OperatingEnvelopeWindowClass::Blocked => 7,
        };
        (class_rank, window.window_id.clone())
    });
}

fn source_ids(input: &OperatingEnvelopePlannerInput) -> Vec<String> {
    input
        .sources()
        .into_iter()
        .map(|source| source.source_id.clone())
        .collect()
}

fn any_reason(sources: &[&OperatingEnvelopeSourceSnapshot], needles: &[&str]) -> bool {
    sources.iter().any(|source| {
        source
            .reason_codes
            .iter()
            .chain(
                source
                    .evidence
                    .iter()
                    .flat_map(|evidence| evidence.reason_codes.iter()),
            )
            .any(|reason| needles.iter().any(|needle| reason.as_str() == *needle))
    })
}

fn dirty_tree_state(facts: &EnvelopeFacts) -> OperatingEnvelopeDirtyTreeState {
    if facts.dirty_overlap {
        OperatingEnvelopeDirtyTreeState::DirtyOverlap
    } else if facts.deletion_risk {
        OperatingEnvelopeDirtyTreeState::Unknown
    } else if facts.dirty_non_overlap {
        OperatingEnvelopeDirtyTreeState::DirtyNonOverlap
    } else {
        OperatingEnvelopeDirtyTreeState::Clean
    }
}

fn wait_reason_codes(facts: &EnvelopeFacts) -> Vec<&'static str> {
    let mut reason_codes = Vec::new();
    if facts.dirty_overlap {
        reason_codes.push("dirty_overlap.present");
        reason_codes.push("coordination.wait_for_owner");
    }
    if facts.active_owner {
        reason_codes.push("assignee_overlap.active");
        reason_codes.push("coordination.wait_for_owner");
    }
    if facts.no_ready_work {
        reason_codes.push("beads.ready_empty");
        reason_codes.push("coordination.wait_for_ready_work");
    }
    if reason_codes.is_empty() {
        reason_codes.push("coordination.wait");
    }
    reason_codes
}

fn rch_proof_state(facts: &EnvelopeFacts) -> OperatingEnvelopeProofState {
    if facts.rch_recovered {
        OperatingEnvelopeProofState::Measured
    } else if facts.rch_no_worker
        || facts.rch_topology_failure
        || facts.rch_active_project_exclusion
        || facts.insufficient_proof
    {
        OperatingEnvelopeProofState::Unavailable
    } else {
        OperatingEnvelopeProofState::NotRequired
    }
}

fn push_unique<T>(values: &mut Vec<T>, value: impl Into<T>)
where
    T: Eq,
{
    let value = value.into();
    if !values.contains(&value) {
        values.push(value);
    }
}

fn bounded_string(value: impl Into<String>, fallback: &str) -> String {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.chars().take(256).collect()
    }
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_778_912_100_000;

    fn source(
        source_id: &str,
        source_kind: OperatingEnvelopeSourceKind,
        reason_code: &str,
    ) -> OperatingEnvelopeSourceSnapshot {
        OperatingEnvelopeSourceSnapshot::new(source_id, source_kind)
            .evidence_level(OperatingEnvelopeEvidenceLevel::Fixture)
            .with_reason_code(reason_code)
    }

    fn base_domains() -> OperatingEnvelopeInputDomains {
        OperatingEnvelopeInputDomains {
            capacity_resource: source(
                "capacity-green",
                OperatingEnvelopeSourceKind::CapacityResource,
                "capacity.green",
            ),
            rch: source(
                "rch-healthy",
                OperatingEnvelopeSourceKind::Rch,
                "rch.remote_cargo_reached_true",
            ),
            beads: source(
                "beads-ready",
                OperatingEnvelopeSourceKind::Beads,
                "beads.ready_available",
            ),
            agent_mail: source(
                "agent-mail-healthy",
                OperatingEnvelopeSourceKind::AgentMail,
                "agent_mail.available",
            ),
            git: source(
                "git-clean",
                OperatingEnvelopeSourceKind::Git,
                "git.clean_for_scope",
            ),
            robot_inventory: None,
        }
    }

    fn input_with(domains: OperatingEnvelopeInputDomains) -> OperatingEnvelopePlannerInput {
        OperatingEnvelopePlannerInput::new(NOW_MS, "test-envelope", "test-objective", domains)
            .target_class(
                OperatingEnvelopeTargetClass::target_64_core_256g()
                    .proof_state(OperatingEnvelopeProofState::Measured),
            )
            .budgets(OperatingEnvelopeBudgets::target_class())
    }

    fn plan_with(domains: OperatingEnvelopeInputDomains) -> OperatingEnvelopePlan {
        plan_operating_envelope(input_with(domains))
    }

    fn window_ids(plan: &OperatingEnvelopePlan) -> Vec<&str> {
        plan.admission_windows
            .iter()
            .map(|window| window.window_id.as_str())
            .collect()
    }

    #[test]
    fn clean_ready_queue_admits_ranked_windows() {
        let plan = plan_with(base_domains());

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Green);
        assert_eq!(
            window_ids(&plan),
            vec!["admit_now", "proof_only", "docs_only"]
        );
        assert_eq!(plan.decision.max_parallel_agents, 32);
        assert_eq!(plan.decision.max_parallel_proofs, 8);
        assert!(
            plan.reason_codes
                .contains(&"policy.no_local_cargo_proof".to_string())
        );
    }

    #[test]
    fn no_ready_work_keeps_wait_advice_only() {
        let mut domains = base_domains();
        domains.beads = source(
            "beads-empty",
            OperatingEnvelopeSourceKind::Beads,
            "beads.ready_empty",
        );

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Wait);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Yellow);
        assert_eq!(window_ids(&plan), vec!["pause_admission"]);
        assert!(
            plan.admission_windows[0]
                .reason_codes
                .contains(&"beads.ready_empty".to_string())
        );
        assert!(
            !plan.admission_windows[0]
                .reason_codes
                .contains(&"dirty_overlap.present".to_string())
        );
    }

    #[test]
    fn active_non_stale_owner_waits_without_claiming() {
        let mut domains = base_domains();
        domains.beads = source(
            "beads-owner",
            OperatingEnvelopeSourceKind::Beads,
            "beads.in_progress_active",
        )
        .with_reason_code("assignee_overlap.active");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Wait);
        assert_eq!(
            plan.decision.dirty_tree_state,
            OperatingEnvelopeDirtyTreeState::Clean
        );
        assert_eq!(plan.decision.max_parallel_agents, 0);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"assignee_overlap.active".to_string())
        );
        assert!(
            !plan
                .decision
                .reason_codes
                .contains(&"dirty_overlap.present".to_string())
        );
    }

    #[test]
    fn dirty_non_overlap_is_preserved_without_blocking_claims() {
        let mut domains = base_domains();
        domains.git = source(
            "git-dirty-non-overlap",
            OperatingEnvelopeSourceKind::Git,
            "git.dirty_non_overlap",
        );

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert_eq!(
            plan.decision.dirty_tree_state,
            OperatingEnvelopeDirtyTreeState::DirtyNonOverlap
        );
    }

    #[test]
    fn stale_reopen_candidate_gets_read_only_status_check() {
        let mut domains = base_domains();
        domains.beads = source(
            "beads-stale",
            OperatingEnvelopeSourceKind::Beads,
            "beads.stale_candidate",
        );

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Degrade);
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "stale_reopen_status_check"]
        );
    }

    #[test]
    fn dirty_overlap_waits_and_forbids_edits() {
        let mut domains = base_domains();
        domains.git = source(
            "git-dirty-overlap",
            OperatingEnvelopeSourceKind::Git,
            "dirty_overlap.present",
        )
        .blocked("dirty_overlap.present");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Wait);
        assert_eq!(
            plan.decision.dirty_tree_state,
            OperatingEnvelopeDirtyTreeState::DirtyOverlap
        );
        assert!(
            plan.admission_windows[0]
                .forbidden_action_classes
                .contains(&OperatingEnvelopeActionClass::DestructiveFilesystem)
        );
    }

    #[test]
    fn rch_degraded_no_worker_defers_remote_proof() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-no-worker",
            OperatingEnvelopeSourceKind::Rch,
            "rch.no_workers_passed_health",
        )
        .with_reason_code("rch.remote_cargo_reached_false")
        .blocked("rch.no_workers_passed_health");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Defer);
        assert_eq!(
            plan.decision.rch_proof_state,
            OperatingEnvelopeProofState::Unavailable
        );
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "admit_after_rch_recovers"]
        );
    }

    #[test]
    fn rch_recovered_allows_proof_window() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-recovered",
            OperatingEnvelopeSourceKind::Rch,
            "rch.remote_cargo_reached_true",
        )
        .with_reason_code("rch.cargo_verdict.pass");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert!(window_ids(&plan).contains(&"proof_only"));
    }

    #[test]
    fn target_class_insufficient_proof_defers_claims() {
        let input = OperatingEnvelopePlannerInput::new(
            NOW_MS,
            "target-class",
            "target-objective",
            base_domains(),
        )
        .target_class(OperatingEnvelopeTargetClass::target_64_core_256g())
        .budgets(OperatingEnvelopeBudgets::target_class());

        let plan = plan_operating_envelope(input);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Defer);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"target_hardware.skipped_not_proven".to_string())
        );
    }

    #[test]
    fn red_and_black_pressure_shed() {
        let mut red = base_domains();
        red.capacity_resource = source(
            "capacity-red",
            OperatingEnvelopeSourceKind::CapacityResource,
            "capacity.red",
        );
        let red_plan = plan_with(red);
        assert_eq!(red_plan.decision.outcome, OperatingEnvelopeOutcome::Shed);
        assert_eq!(red_plan.decision.envelope_tier, OperatingEnvelopeTier::Red);

        let mut black = base_domains();
        black.capacity_resource = source(
            "capacity-black",
            OperatingEnvelopeSourceKind::CapacityResource,
            "capacity.black",
        );
        let black_plan = plan_with(black);
        assert_eq!(black_plan.decision.outcome, OperatingEnvelopeOutcome::Shed);
        assert_eq!(
            black_plan.decision.envelope_tier,
            OperatingEnvelopeTier::Black
        );
    }

    #[test]
    fn agent_mail_unavailable_uses_beads_only_static_window() {
        let mut domains = base_domains();
        domains.agent_mail = source(
            "agent-mail-red",
            OperatingEnvelopeSourceKind::AgentMail,
            "agent_mail.unavailable_after_retry",
        )
        .unavailable("agent_mail.unavailable_after_retry");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Degrade);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Yellow);
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "admit_after_agent_mail_recovers"]
        );
        assert_eq!(plan.decision.max_parallel_proofs, 0);
    }

    #[test]
    fn privacy_violation_blocks_even_when_other_sources_are_green() {
        let mut domains = base_domains();
        domains.git = domains.git.clone().raw_pane_content_stored();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Block);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Black);
        assert_eq!(plan.decision.max_parallel_agents, 0);
    }
}
