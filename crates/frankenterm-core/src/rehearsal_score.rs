//! Objective rehearsal score receipt schema.
//!
//! This module defines the stable receipt contract, side-effect-free source
//! adapters, and deterministic scoring engine for future rehearsal scoring
//! surfaces. It deliberately treats blocked, skipped, simulated, and missing
//! evidence as typed states instead of prose.

#![allow(clippy::module_name_repetitions)]

use crate::demo_scenarios::{
    DEMO_SCENARIO_MANIFEST_SCHEMA_VERSION, DemoScenarioArtifact, DemoScenarioDegradationReason,
    DemoScenarioManifest, DemoScenarioRedactionTier, DemoScenarioSpec,
};
use serde::{Deserialize, Serialize};

pub const REHEARSAL_SCORE_RECEIPT_CONTRACT_ID: &str = "ft.rehearsal_score_receipt.v1";
pub const REHEARSAL_SCORE_SURFACE_CONTRACT_ID: &str = "ft.rehearsal_score_surface.v1";
pub const REHEARSAL_SCORE_HARNESS_LOG_CONTRACT_ID: &str = "ft.rehearsal_score_harness_log.v1";
pub const REHEARSAL_SCORE_MCP_CURRENT_URI: &str = "wa://rehearsal-score/current";
pub const REHEARSAL_SCORE_MCP_SURFACE_URI_TEMPLATE: &str = "wa://rehearsal-score/{surface}";
pub const REHEARSAL_SCORE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalVerdict {
    Pass,
    Fail,
    Blocked,
    MissingEvidence,
    Degraded,
    Skipped,
    NotApplicable,
}

impl RehearsalVerdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Blocked => "blocked",
            Self::MissingEvidence => "missing_evidence",
            Self::Degraded => "degraded",
            Self::Skipped => "skipped",
            Self::NotApplicable => "not_applicable",
        }
    }

    #[must_use]
    pub fn is_passing(self) -> bool {
        matches!(self, Self::Pass | Self::NotApplicable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalCriterionKind {
    ScenarioCompletion,
    SafetyPolicy,
    RedactionPrivacy,
    RchProof,
    AgentMailCoordination,
    DirtyOverlapOwnership,
    ResourceEnvelope,
    LatencyThroughput,
    ArtifactIntegrity,
}

impl RehearsalCriterionKind {
    pub const ALL: [Self; 9] = [
        Self::ScenarioCompletion,
        Self::SafetyPolicy,
        Self::RedactionPrivacy,
        Self::RchProof,
        Self::AgentMailCoordination,
        Self::DirtyOverlapOwnership,
        Self::ResourceEnvelope,
        Self::LatencyThroughput,
        Self::ArtifactIntegrity,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScenarioCompletion => "scenario_completion",
            Self::SafetyPolicy => "safety_policy",
            Self::RedactionPrivacy => "redaction_privacy",
            Self::RchProof => "rch_proof",
            Self::AgentMailCoordination => "agent_mail_coordination",
            Self::DirtyOverlapOwnership => "dirty_overlap_ownership",
            Self::ResourceEnvelope => "resource_envelope",
            Self::LatencyThroughput => "latency_throughput",
            Self::ArtifactIntegrity => "artifact_integrity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalEvidenceState {
    Proven,
    FixtureOnly,
    Simulated,
    Redacted,
    Degraded,
    Blocked,
    Missing,
}

impl RehearsalEvidenceState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proven => "proven",
            Self::FixtureOnly => "fixture_only",
            Self::Simulated => "simulated",
            Self::Redacted => "redacted",
            Self::Degraded => "degraded",
            Self::Blocked => "blocked",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalEvidenceRef {
    pub source: String,
    pub reference: String,
    pub state: RehearsalEvidenceState,
    #[serde(default)]
    pub digest: Option<String>,
}

impl RehearsalEvidenceRef {
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        reference: impl Into<String>,
        state: RehearsalEvidenceState,
    ) -> Self {
        Self {
            source: source.into(),
            reference: reference.into(),
            state,
            digest: None,
        }
    }

    #[must_use]
    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalCriterionReceipt {
    pub criterion_id: String,
    pub kind: RehearsalCriterionKind,
    pub verdict: RehearsalVerdict,
    pub confidence_percent: u8,
    #[serde(default)]
    pub evidence: Vec<RehearsalEvidenceRef>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub note: String,
}

impl RehearsalCriterionReceipt {
    #[must_use]
    pub fn new(
        criterion_id: impl Into<String>,
        kind: RehearsalCriterionKind,
        verdict: RehearsalVerdict,
    ) -> Self {
        Self {
            criterion_id: criterion_id.into(),
            kind,
            verdict,
            confidence_percent: 100,
            evidence: Vec::new(),
            blockers: Vec::new(),
            next_actions: Vec::new(),
            note: String::new(),
        }
    }

    #[must_use]
    pub fn with_confidence_percent(mut self, confidence_percent: u8) -> Self {
        self.confidence_percent = confidence_percent.min(100);
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: RehearsalEvidenceRef) -> Self {
        self.evidence.push(evidence);
        self
    }

    #[must_use]
    pub fn with_blocker(mut self, blocker: impl Into<String>) -> Self {
        self.blockers.push(blocker.into());
        self
    }

    #[must_use]
    pub fn with_next_action(mut self, next_action: impl Into<String>) -> Self {
        self.next_actions.push(next_action.into());
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalAggregateScore {
    pub total_criteria: u32,
    pub scorable_criteria: u32,
    pub passed: u32,
    pub failed: u32,
    pub blocked: u32,
    pub missing_evidence: u32,
    pub degraded: u32,
    pub skipped: u32,
    pub not_applicable: u32,
    pub score_percent: u8,
}

impl RehearsalAggregateScore {
    #[must_use]
    pub fn from_criteria(criteria: &[RehearsalCriterionReceipt]) -> Self {
        let mut score = Self {
            total_criteria: saturated_len(criteria.len()),
            scorable_criteria: 0,
            passed: 0,
            failed: 0,
            blocked: 0,
            missing_evidence: 0,
            degraded: 0,
            skipped: 0,
            not_applicable: 0,
            score_percent: 0,
        };

        for criterion in criteria {
            match criterion.verdict {
                RehearsalVerdict::Pass => {
                    score.scorable_criteria = score.scorable_criteria.saturating_add(1);
                    score.passed = score.passed.saturating_add(1);
                }
                RehearsalVerdict::Fail => {
                    score.scorable_criteria = score.scorable_criteria.saturating_add(1);
                    score.failed = score.failed.saturating_add(1);
                }
                RehearsalVerdict::Blocked => {
                    score.scorable_criteria = score.scorable_criteria.saturating_add(1);
                    score.blocked = score.blocked.saturating_add(1);
                }
                RehearsalVerdict::MissingEvidence => {
                    score.scorable_criteria = score.scorable_criteria.saturating_add(1);
                    score.missing_evidence = score.missing_evidence.saturating_add(1);
                }
                RehearsalVerdict::Degraded => {
                    score.scorable_criteria = score.scorable_criteria.saturating_add(1);
                    score.degraded = score.degraded.saturating_add(1);
                }
                RehearsalVerdict::Skipped => {
                    score.scorable_criteria = score.scorable_criteria.saturating_add(1);
                    score.skipped = score.skipped.saturating_add(1);
                }
                RehearsalVerdict::NotApplicable => {
                    score.not_applicable = score.not_applicable.saturating_add(1);
                }
            }
        }

        score.score_percent = percent(score.passed, score.scorable_criteria);
        score
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalScoreReceipt {
    pub schema_version: u16,
    pub contract_id: String,
    pub rehearsal_id: String,
    pub scenario_id: String,
    pub aggregate_verdict: RehearsalVerdict,
    pub aggregate_score: RehearsalAggregateScore,
    pub criteria: Vec<RehearsalCriterionReceipt>,
    #[serde(default)]
    pub source_artifacts: Vec<RehearsalEvidenceRef>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl RehearsalScoreReceipt {
    #[must_use]
    pub fn new(
        rehearsal_id: impl Into<String>,
        scenario_id: impl Into<String>,
        criteria: Vec<RehearsalCriterionReceipt>,
    ) -> Self {
        let aggregate_score = RehearsalAggregateScore::from_criteria(&criteria);
        let aggregate_verdict = aggregate_verdict(&criteria);

        Self {
            schema_version: REHEARSAL_SCORE_SCHEMA_VERSION,
            contract_id: REHEARSAL_SCORE_RECEIPT_CONTRACT_ID.to_string(),
            rehearsal_id: rehearsal_id.into(),
            scenario_id: scenario_id.into(),
            aggregate_verdict,
            aggregate_score,
            criteria,
            source_artifacts: Vec::new(),
            generated_at: None,
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_source_artifact(mut self, artifact: RehearsalEvidenceRef) -> Self {
        self.source_artifacts.push(artifact);
        self
    }

    #[must_use]
    pub fn with_generated_at(mut self, generated_at: impl Into<String>) -> Self {
        self.generated_at = Some(generated_at.into());
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalRollupContribution {
    Passed,
    CriticalFailure,
    Blocked,
    MissingEvidence,
    Degraded,
    Skipped,
    NotApplicable,
}

impl RehearsalRollupContribution {
    #[must_use]
    pub fn from_verdict(verdict: RehearsalVerdict) -> Self {
        match verdict {
            RehearsalVerdict::Pass => Self::Passed,
            RehearsalVerdict::Fail => Self::CriticalFailure,
            RehearsalVerdict::Blocked => Self::Blocked,
            RehearsalVerdict::MissingEvidence => Self::MissingEvidence,
            RehearsalVerdict::Degraded => Self::Degraded,
            RehearsalVerdict::Skipped => Self::Skipped,
            RehearsalVerdict::NotApplicable => Self::NotApplicable,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::CriticalFailure => "critical_failure",
            Self::Blocked => "blocked",
            Self::MissingEvidence => "missing_evidence",
            Self::Degraded => "degraded",
            Self::Skipped => "skipped",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalCriterionEvaluationLog {
    pub criterion_id: String,
    pub evaluator: String,
    pub original_verdict: RehearsalVerdict,
    pub verdict: RehearsalVerdict,
    pub confidence_percent: u8,
    #[serde(default)]
    pub input_evidence: Vec<RehearsalEvidenceRef>,
    pub rollup_contribution: RehearsalRollupContribution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalNextActionHint {
    pub rank: u32,
    pub criterion_id: String,
    pub verdict: RehearsalVerdict,
    pub action: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalScoringResult {
    pub receipt: RehearsalScoreReceipt,
    pub aggregate_confidence_percent: u8,
    #[serde(default)]
    pub next_action_hints: Vec<RehearsalNextActionHint>,
    #[serde(default)]
    pub log: Vec<RehearsalCriterionEvaluationLog>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalScoreSurface {
    Score,
    Explain,
}

impl RehearsalScoreSurface {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Score => "score",
            Self::Explain => "explain",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalScoreSurfaceReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub surface: RehearsalScoreSurface,
    pub source_adapter: RehearsalSourceAdapterKind,
    pub source_ref: String,
    pub source_schema_version: String,
    pub source_adapter_log: RehearsalAdapterExtractionLog,
    pub receipt: RehearsalScoreReceipt,
    pub aggregate_confidence_percent: u8,
    #[serde(default)]
    pub next_action_hints: Vec<RehearsalNextActionHint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluation_log: Vec<RehearsalCriterionEvaluationLog>,
    pub raw_pane_content_stored: bool,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
}

impl RehearsalScoreSurfaceReport {
    #[must_use]
    pub fn from_demo_scenario_manifest(
        manifest: &DemoScenarioManifest,
        source_ref: impl Into<String>,
        rehearsal_id: impl Into<String>,
        scenario_id: impl Into<String>,
        surface: RehearsalScoreSurface,
    ) -> Self {
        let extraction =
            RehearsalAdapterExtraction::from_demo_scenario_manifest(manifest, source_ref);
        let source_adapter = extraction.adapter_kind;
        let source_ref = extraction.source_ref.clone();
        let source_schema_version = extraction.source_schema_version.clone();
        let source_adapter_log = extraction.log.clone();
        let result =
            RehearsalScoringEngine::score_extraction(extraction, rehearsal_id, scenario_id);
        let evaluation_log = if surface == RehearsalScoreSurface::Explain {
            result.log
        } else {
            Vec::new()
        };

        Self {
            schema_version: REHEARSAL_SCORE_SCHEMA_VERSION,
            contract_id: REHEARSAL_SCORE_SURFACE_CONTRACT_ID.to_string(),
            surface,
            source_adapter,
            source_ref,
            source_schema_version,
            source_adapter_log,
            receipt: result.receipt,
            aggregate_confidence_percent: result.aggregate_confidence_percent,
            next_action_hints: result.next_action_hints,
            evaluation_log,
            raw_pane_content_stored: false,
            live_mutation_allowed: false,
            side_effects_executed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalHarnessProofStatus {
    Proven,
    Failed,
    Blocked,
    MissingEvidence,
    Degraded,
    Skipped,
    NotApplicable,
}

impl RehearsalHarnessProofStatus {
    #[must_use]
    pub fn from_verdict(verdict: RehearsalVerdict) -> Self {
        match verdict {
            RehearsalVerdict::Pass => Self::Proven,
            RehearsalVerdict::Fail => Self::Failed,
            RehearsalVerdict::Blocked => Self::Blocked,
            RehearsalVerdict::MissingEvidence => Self::MissingEvidence,
            RehearsalVerdict::Degraded => Self::Degraded,
            RehearsalVerdict::Skipped => Self::Skipped,
            RehearsalVerdict::NotApplicable => Self::NotApplicable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalHarnessCriterionInput {
    pub criterion_id: String,
    pub kind: RehearsalCriterionKind,
    pub verdict: RehearsalVerdict,
    pub evidence_states: Vec<RehearsalEvidenceState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalScoreHarnessLogEntry {
    pub schema_version: u16,
    pub contract_id: String,
    pub scenario_id: String,
    pub source_artifact_ids: Vec<String>,
    pub commands_or_resources_queried: Vec<String>,
    pub criterion_inputs: Vec<RehearsalHarnessCriterionInput>,
    pub score_receipt: RehearsalScoreReceipt,
    pub elapsed_ms: u64,
    pub resource_notes: Vec<String>,
    pub proof_status: RehearsalHarnessProofStatus,
}

impl RehearsalScoreHarnessLogEntry {
    #[must_use]
    pub fn from_surface_report(
        report: &RehearsalScoreSurfaceReport,
        commands_or_resources_queried: Vec<String>,
        elapsed_ms: u64,
        resource_notes: Vec<String>,
    ) -> Self {
        let source_artifact_ids = report
            .receipt
            .source_artifacts
            .iter()
            .map(|artifact| format!("{}:{}", artifact.source, artifact.reference))
            .collect();
        let criterion_inputs = report
            .receipt
            .criteria
            .iter()
            .map(|criterion| RehearsalHarnessCriterionInput {
                criterion_id: criterion.criterion_id.clone(),
                kind: criterion.kind,
                verdict: criterion.verdict,
                evidence_states: criterion
                    .evidence
                    .iter()
                    .map(|evidence| evidence.state)
                    .collect(),
            })
            .collect();

        Self {
            schema_version: REHEARSAL_SCORE_SCHEMA_VERSION,
            contract_id: REHEARSAL_SCORE_HARNESS_LOG_CONTRACT_ID.to_string(),
            scenario_id: report.receipt.scenario_id.clone(),
            source_artifact_ids,
            commands_or_resources_queried,
            criterion_inputs,
            score_receipt: report.receipt.clone(),
            elapsed_ms,
            resource_notes,
            proof_status: RehearsalHarnessProofStatus::from_verdict(
                report.receipt.aggregate_verdict,
            ),
        }
    }

    pub fn to_jsonl_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RehearsalScoringEngine;

impl RehearsalScoringEngine {
    #[must_use]
    pub fn score_extraction(
        extraction: RehearsalAdapterExtraction,
        rehearsal_id: impl Into<String>,
        scenario_id: impl Into<String>,
    ) -> RehearsalScoringResult {
        let adapter_id = extraction.adapter_id.clone();
        let redaction_criterion = redaction_state_criterion(&extraction);
        let mut criteria = extraction.criteria;
        criteria.push(redaction_criterion);

        let mut result = Self::score_criteria(rehearsal_id, scenario_id, criteria);
        for artifact in extraction.source_artifacts {
            result.receipt = result.receipt.with_source_artifact(artifact);
        }
        result.receipt = result.receipt.with_note(format!(
            "score_engine evaluated adapter {adapter_id} with aggregate confidence {}%",
            result.aggregate_confidence_percent
        ));

        result
    }

    #[must_use]
    pub fn score_criteria(
        rehearsal_id: impl Into<String>,
        scenario_id: impl Into<String>,
        criteria: Vec<RehearsalCriterionReceipt>,
    ) -> RehearsalScoringResult {
        let mut evaluated_criteria = Vec::with_capacity(criteria.len());
        let mut log = Vec::with_capacity(criteria.len());

        for criterion in criteria {
            let (evaluated, entry) = evaluate_criterion(criterion);
            evaluated_criteria.push(evaluated);
            log.push(entry);
        }

        let aggregate_confidence_percent = aggregate_confidence_percent(&evaluated_criteria);
        let next_action_hints = next_action_hints(&evaluated_criteria);
        let receipt = RehearsalScoreReceipt::new(rehearsal_id, scenario_id, evaluated_criteria)
            .with_note(format!(
                "score_engine aggregate_confidence_percent={aggregate_confidence_percent} next_action_hints={}",
                next_action_hints.len()
            ));

        RehearsalScoringResult {
            receipt,
            aggregate_confidence_percent,
            next_action_hints,
            log,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalSourceAdapterKind {
    DemoScenarioManifest,
    FlightRecorder,
    MissionTwinSnapshot,
    OperatingEnvelope,
    ProofReplay,
    RchAdmission,
    AgentMailFallback,
}

impl RehearsalSourceAdapterKind {
    pub const ALL: [Self; 7] = [
        Self::DemoScenarioManifest,
        Self::FlightRecorder,
        Self::MissionTwinSnapshot,
        Self::OperatingEnvelope,
        Self::ProofReplay,
        Self::RchAdmission,
        Self::AgentMailFallback,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DemoScenarioManifest => "demo_scenario_manifest",
            Self::FlightRecorder => "flight_recorder",
            Self::MissionTwinSnapshot => "mission_twin_snapshot",
            Self::OperatingEnvelope => "operating_envelope",
            Self::ProofReplay => "proof_replay",
            Self::RchAdmission => "rch_admission",
            Self::AgentMailFallback => "agent_mail_fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalAdapterRedactionState {
    Public,
    Redacted,
    Unknown,
    Unsafe,
}

impl RehearsalAdapterRedactionState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Redacted => "redacted",
            Self::Unknown => "unknown",
            Self::Unsafe => "unsafe",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalSourceObservation {
    pub criterion_id: String,
    pub kind: RehearsalCriterionKind,
    pub verdict: RehearsalVerdict,
    pub evidence: RehearsalEvidenceRef,
    #[serde(default = "default_confidence_percent")]
    pub confidence_percent: u8,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub note: String,
}

impl RehearsalSourceObservation {
    #[must_use]
    pub fn new(
        criterion_id: impl Into<String>,
        kind: RehearsalCriterionKind,
        verdict: RehearsalVerdict,
        evidence: RehearsalEvidenceRef,
    ) -> Self {
        Self {
            criterion_id: criterion_id.into(),
            kind,
            verdict,
            evidence,
            confidence_percent: 100,
            blockers: Vec::new(),
            next_actions: Vec::new(),
            note: String::new(),
        }
    }

    #[must_use]
    pub fn missing_required(
        criterion_id: impl Into<String>,
        kind: RehearsalCriterionKind,
        source: impl Into<String>,
        reference: impl Into<String>,
        blocker: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        Self::new(
            criterion_id,
            kind,
            RehearsalVerdict::MissingEvidence,
            RehearsalEvidenceRef::new(source, reference, RehearsalEvidenceState::Missing),
        )
        .with_confidence_percent(70)
        .with_blocker(blocker)
        .with_next_action(next_action)
    }

    #[must_use]
    pub fn with_confidence_percent(mut self, confidence_percent: u8) -> Self {
        self.confidence_percent = confidence_percent.min(100);
        self
    }

    #[must_use]
    pub fn with_blocker(mut self, blocker: impl Into<String>) -> Self {
        self.blockers.push(blocker.into());
        self
    }

    #[must_use]
    pub fn with_next_action(mut self, next_action: impl Into<String>) -> Self {
        self.next_actions.push(next_action.into());
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    fn into_criterion(self) -> RehearsalCriterionReceipt {
        let mut criterion =
            RehearsalCriterionReceipt::new(self.criterion_id, self.kind, self.verdict)
                .with_confidence_percent(self.confidence_percent)
                .with_evidence(self.evidence);

        for blocker in self.blockers {
            criterion = criterion.with_blocker(blocker);
        }
        for next_action in self.next_actions {
            criterion = criterion.with_next_action(next_action);
        }
        if !self.note.is_empty() {
            criterion = criterion.with_note(self.note);
        }

        criterion
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalAdapterExtractionLog {
    pub adapter_id: String,
    pub source_ref: String,
    pub source_schema_version: String,
    pub extracted_criteria_count: u32,
    pub missing_evidence_count: u32,
    pub redaction_state: RehearsalAdapterRedactionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalAdapterExtraction {
    pub adapter_id: String,
    pub adapter_kind: RehearsalSourceAdapterKind,
    pub source_ref: String,
    pub source_schema_version: String,
    pub criteria: Vec<RehearsalCriterionReceipt>,
    #[serde(default)]
    pub source_artifacts: Vec<RehearsalEvidenceRef>,
    pub missing_evidence_count: u32,
    pub redaction_state: RehearsalAdapterRedactionState,
    pub log: RehearsalAdapterExtractionLog,
}

impl RehearsalAdapterExtraction {
    #[must_use]
    pub fn from_demo_scenario_manifest(
        manifest: &DemoScenarioManifest,
        source_ref: impl Into<String>,
    ) -> Self {
        let source_ref = source_ref.into();
        let redaction_state = demo_manifest_redaction_state(manifest);
        let mut criteria = Vec::new();

        for scenario in &manifest.scenarios {
            criteria.push(demo_scenario_completion_criterion(scenario));
            for artifact in &scenario.expected_artifacts {
                criteria.push(demo_artifact_criterion(&scenario.id, artifact));
            }
            if let Some(criterion) = demo_degradation_policy_criterion(
                scenario,
                &source_ref,
                DemoScenarioDegradationReason::AgentMailUnavailable,
                RehearsalCriterionKind::AgentMailCoordination,
            ) {
                criteria.push(criterion);
            }
            if let Some(criterion) = demo_degradation_policy_criterion(
                scenario,
                &source_ref,
                DemoScenarioDegradationReason::RchProofUnavailable,
                RehearsalCriterionKind::RchProof,
            ) {
                criteria.push(criterion);
            }
        }

        let source_artifacts = vec![RehearsalEvidenceRef::new(
            "demo_scenario_manifest",
            source_ref.clone(),
            RehearsalEvidenceState::FixtureOnly,
        )];
        Self::new(
            "demo_scenario_manifest.v1",
            RehearsalSourceAdapterKind::DemoScenarioManifest,
            source_ref,
            DEMO_SCENARIO_MANIFEST_SCHEMA_VERSION,
            criteria,
            source_artifacts,
            redaction_state,
        )
    }

    #[must_use]
    pub fn from_source_observations(
        adapter_id: impl Into<String>,
        adapter_kind: RehearsalSourceAdapterKind,
        source_ref: impl Into<String>,
        source_schema_version: impl Into<String>,
        observations: Vec<RehearsalSourceObservation>,
        source_artifacts: Vec<RehearsalEvidenceRef>,
        redaction_state: RehearsalAdapterRedactionState,
    ) -> Self {
        let criteria = observations
            .into_iter()
            .map(RehearsalSourceObservation::into_criterion)
            .collect();
        Self::new(
            adapter_id,
            adapter_kind,
            source_ref,
            source_schema_version,
            criteria,
            source_artifacts,
            redaction_state,
        )
    }

    #[must_use]
    pub fn new(
        adapter_id: impl Into<String>,
        adapter_kind: RehearsalSourceAdapterKind,
        source_ref: impl Into<String>,
        source_schema_version: impl Into<String>,
        criteria: Vec<RehearsalCriterionReceipt>,
        source_artifacts: Vec<RehearsalEvidenceRef>,
        redaction_state: RehearsalAdapterRedactionState,
    ) -> Self {
        let adapter_id = adapter_id.into();
        let source_ref = source_ref.into();
        let source_schema_version = source_schema_version.into();
        let missing_evidence_count = missing_evidence_count(&criteria);
        let extracted_criteria_count = saturated_len(criteria.len());
        let log = RehearsalAdapterExtractionLog {
            adapter_id: adapter_id.clone(),
            source_ref: source_ref.clone(),
            source_schema_version: source_schema_version.clone(),
            extracted_criteria_count,
            missing_evidence_count,
            redaction_state,
        };

        Self {
            adapter_id,
            adapter_kind,
            source_ref,
            source_schema_version,
            criteria,
            source_artifacts,
            missing_evidence_count,
            redaction_state,
            log,
        }
    }

    #[must_use]
    pub fn into_receipt(
        self,
        rehearsal_id: impl Into<String>,
        scenario_id: impl Into<String>,
    ) -> RehearsalScoreReceipt {
        let log = self.log.clone();
        let mut receipt = RehearsalScoreReceipt::new(rehearsal_id, scenario_id, self.criteria)
            .with_note(format!(
                "adapter {} extracted {} criteria from {} with {} missing evidence item(s)",
                log.adapter_id,
                log.extracted_criteria_count,
                log.source_ref,
                log.missing_evidence_count
            ));

        for artifact in self.source_artifacts {
            receipt = receipt.with_source_artifact(artifact);
        }

        receipt
    }
}

#[must_use]
pub fn aggregate_verdict(criteria: &[RehearsalCriterionReceipt]) -> RehearsalVerdict {
    if criteria
        .iter()
        .any(|criterion| criterion.verdict == RehearsalVerdict::Fail)
    {
        return RehearsalVerdict::Fail;
    }
    if criteria
        .iter()
        .any(|criterion| criterion.verdict == RehearsalVerdict::Blocked)
    {
        return RehearsalVerdict::Blocked;
    }
    if criteria
        .iter()
        .any(|criterion| criterion.verdict == RehearsalVerdict::MissingEvidence)
    {
        return RehearsalVerdict::MissingEvidence;
    }
    if criteria
        .iter()
        .any(|criterion| criterion.verdict == RehearsalVerdict::Degraded)
    {
        return RehearsalVerdict::Degraded;
    }
    if criteria
        .iter()
        .any(|criterion| criterion.verdict == RehearsalVerdict::Skipped)
    {
        return RehearsalVerdict::Skipped;
    }
    if criteria
        .iter()
        .all(|criterion| criterion.verdict == RehearsalVerdict::NotApplicable)
    {
        return RehearsalVerdict::NotApplicable;
    }

    RehearsalVerdict::Pass
}

const FORBIDDEN_SIDE_EFFECT_MARKERS: &[&str] = &[
    "am service restart",
    "am service stop",
    "am doctor fix",
    "am doctor repair",
    "am doctor reconstruct",
    "kill am",
    "kill mcp-agent-mail",
    "kill am serve-http",
    "git reset --hard",
    "git clean -fd",
    "rm -rf",
];

const RCH_LOCAL_FALLBACK_MARKERS: &[&str] = &[
    "[rch] local",
    "running locally",
    "local fallback",
    "worker=null",
    "no admissible workers",
    "did not reach a remote worker",
];

const TIMEOUT_MARKERS: &[&str] = &["timeout", "timed out", "ssh_timeout"];

fn redaction_state_criterion(extraction: &RehearsalAdapterExtraction) -> RehearsalCriterionReceipt {
    let criterion_id = format!("{}.redaction_state", extraction.adapter_id);
    let source = extraction.adapter_kind.as_str();
    let reference = format!("{}#redaction_state", extraction.source_ref);

    match extraction.redaction_state {
        RehearsalAdapterRedactionState::Public => RehearsalCriterionReceipt::new(
            criterion_id,
            RehearsalCriterionKind::RedactionPrivacy,
            RehearsalVerdict::Pass,
        )
        .with_evidence(RehearsalEvidenceRef::new(
            source,
            reference,
            RehearsalEvidenceState::Proven,
        ))
        .with_note("adapter source declares public redaction state"),
        RehearsalAdapterRedactionState::Redacted => RehearsalCriterionReceipt::new(
            criterion_id,
            RehearsalCriterionKind::RedactionPrivacy,
            RehearsalVerdict::Pass,
        )
        .with_confidence_percent(90)
        .with_evidence(RehearsalEvidenceRef::new(
            source,
            reference,
            RehearsalEvidenceState::Redacted,
        ))
        .with_note("adapter source declares redacted evidence"),
        RehearsalAdapterRedactionState::Unknown => RehearsalCriterionReceipt::new(
            criterion_id,
            RehearsalCriterionKind::RedactionPrivacy,
            RehearsalVerdict::MissingEvidence,
        )
        .with_confidence_percent(70)
        .with_evidence(RehearsalEvidenceRef::new(
            source,
            reference,
            RehearsalEvidenceState::Missing,
        ))
        .with_blocker("redaction.state_unknown")
        .with_next_action("attach privacy classification or redaction audit evidence")
        .with_note("unknown redaction state fails closed until privacy evidence exists"),
        RehearsalAdapterRedactionState::Unsafe => RehearsalCriterionReceipt::new(
            criterion_id,
            RehearsalCriterionKind::RedactionPrivacy,
            RehearsalVerdict::Fail,
        )
        .with_confidence_percent(95)
        .with_evidence(RehearsalEvidenceRef::new(
            source,
            reference,
            RehearsalEvidenceState::Blocked,
        ))
        .with_blocker("redaction.state_unsafe")
        .with_next_action("stop rehearsal export and fix unsafe read path before rerun")
        .with_note("unsafe redaction state is a hard privacy failure"),
    }
}

fn evaluate_criterion(
    mut criterion: RehearsalCriterionReceipt,
) -> (RehearsalCriterionReceipt, RehearsalCriterionEvaluationLog) {
    let original_verdict = criterion.verdict;
    let input_evidence = criterion.evidence.clone();
    let evaluator = evaluator_for_kind(criterion.kind).to_string();

    apply_evidence_state_guards(&mut criterion);
    apply_kind_guards(&mut criterion);

    let rollup_contribution = RehearsalRollupContribution::from_verdict(criterion.verdict);
    let entry = RehearsalCriterionEvaluationLog {
        criterion_id: criterion.criterion_id.clone(),
        evaluator,
        original_verdict,
        verdict: criterion.verdict,
        confidence_percent: criterion.confidence_percent,
        input_evidence,
        rollup_contribution,
    };

    (criterion, entry)
}

fn apply_evidence_state_guards(criterion: &mut RehearsalCriterionReceipt) {
    if criterion.evidence.is_empty() && criterion.verdict != RehearsalVerdict::NotApplicable {
        let action = format!("attach evidence for criterion `{}`", criterion.criterion_id);
        apply_scoring_guard(
            criterion,
            RehearsalVerdict::MissingEvidence,
            60,
            "evidence.required_missing",
            action,
        );
        return;
    }

    let Some(state) = worst_evidence_state(&criterion.evidence) else {
        return;
    };
    match state {
        RehearsalEvidenceState::Missing => apply_scoring_guard(
            criterion,
            RehearsalVerdict::MissingEvidence,
            70,
            "evidence.state_missing",
            "attach or record the missing evidence artifact",
        ),
        RehearsalEvidenceState::Blocked => apply_scoring_guard(
            criterion,
            RehearsalVerdict::Blocked,
            80,
            "evidence.state_blocked",
            "resolve the blocked proof or keep the rehearsal blocked",
        ),
        RehearsalEvidenceState::Simulated => apply_scoring_guard(
            criterion,
            RehearsalVerdict::Degraded,
            65,
            "evidence.state_simulated",
            "replace simulated evidence with a live or replayed proof artifact",
        ),
        RehearsalEvidenceState::Degraded => apply_scoring_guard(
            criterion,
            RehearsalVerdict::Degraded,
            75,
            "evidence.state_degraded",
            "rerun or attach non-degraded evidence before treating this as fully proven",
        ),
        RehearsalEvidenceState::Proven
        | RehearsalEvidenceState::FixtureOnly
        | RehearsalEvidenceState::Redacted => {}
    }
}

fn apply_kind_guards(criterion: &mut RehearsalCriterionReceipt) {
    let criterion_text = criterion_text(criterion);
    if contains_marker(&criterion_text, FORBIDDEN_SIDE_EFFECT_MARKERS) {
        criterion
            .next_actions
            .retain(|action| !contains_marker(action, FORBIDDEN_SIDE_EFFECT_MARKERS));
        apply_scoring_guard(
            criterion,
            RehearsalVerdict::Fail,
            95,
            "side_effect.forbidden_command",
            "remove unsafe side-effect action and rerun rehearsal with read-only evidence",
        );
    }

    if criterion.kind != RehearsalCriterionKind::RchProof {
        return;
    }

    if contains_marker(&criterion_text, RCH_LOCAL_FALLBACK_MARKERS) {
        apply_scoring_guard(
            criterion,
            RehearsalVerdict::Blocked,
            85,
            "rch.remote_proof_not_admissible",
            "rerun proof with RCH_REQUIRE_REMOTE=1 and retain only remote worker output",
        );
    }

    if contains_marker(&criterion_text, TIMEOUT_MARKERS) {
        apply_scoring_guard(
            criterion,
            RehearsalVerdict::Blocked,
            80,
            "rch.remote_proof_timeout",
            "rerun focused proof on a warmed remote worker or keep proof blocked",
        );
    }
}

fn apply_scoring_guard(
    criterion: &mut RehearsalCriterionReceipt,
    verdict: RehearsalVerdict,
    confidence_ceiling: u8,
    blocker: impl Into<String>,
    next_action: impl Into<String>,
) {
    if verdict_severity(verdict) > verdict_severity(criterion.verdict) {
        criterion.verdict = verdict;
    }
    criterion.confidence_percent = criterion.confidence_percent.min(confidence_ceiling);
    push_unique(&mut criterion.blockers, blocker);
    push_unique(&mut criterion.next_actions, next_action);
}

fn push_unique(values: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !values.contains(&value) {
        values.push(value);
    }
}

fn aggregate_confidence_percent(criteria: &[RehearsalCriterionReceipt]) -> u8 {
    if criteria.is_empty() {
        return 0;
    }

    let sum = criteria
        .iter()
        .map(|criterion| u32::from(criterion.confidence_percent))
        .sum::<u32>();
    percent(sum, saturated_len(criteria.len()).saturating_mul(100))
}

fn next_action_hints(criteria: &[RehearsalCriterionReceipt]) -> Vec<RehearsalNextActionHint> {
    let mut seen = std::collections::BTreeSet::new();
    let mut candidates = Vec::new();

    for criterion in criteria {
        if criterion.verdict.is_passing() {
            continue;
        }
        for action in &criterion.next_actions {
            let key = format!("{}\n{action}", criterion.criterion_id);
            if !seen.insert(key) {
                continue;
            }
            candidates.push(HintCandidate {
                severity: verdict_severity(criterion.verdict),
                kind_priority: kind_priority(criterion.kind),
                criterion_id: criterion.criterion_id.clone(),
                verdict: criterion.verdict,
                action: action.clone(),
                reason: criterion.blockers.first().cloned().unwrap_or_else(|| {
                    format!("{} {}", criterion.kind.as_str(), criterion.verdict.as_str())
                }),
            });
        }
    }

    candidates.sort_by(|left, right| {
        right
            .severity
            .cmp(&left.severity)
            .then_with(|| left.kind_priority.cmp(&right.kind_priority))
            .then_with(|| left.criterion_id.cmp(&right.criterion_id))
            .then_with(|| left.action.cmp(&right.action))
    });

    candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| RehearsalNextActionHint {
            rank: saturated_len(index.saturating_add(1)),
            criterion_id: candidate.criterion_id,
            verdict: candidate.verdict,
            action: candidate.action,
            reason: candidate.reason,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HintCandidate {
    severity: u8,
    kind_priority: u8,
    criterion_id: String,
    verdict: RehearsalVerdict,
    action: String,
    reason: String,
}

fn evaluator_for_kind(kind: RehearsalCriterionKind) -> &'static str {
    match kind {
        RehearsalCriterionKind::ScenarioCompletion => "scenario_completion_evaluator",
        RehearsalCriterionKind::SafetyPolicy => "safety_policy_evaluator",
        RehearsalCriterionKind::RedactionPrivacy => "redaction_privacy_evaluator",
        RehearsalCriterionKind::RchProof => "rch_proof_evaluator",
        RehearsalCriterionKind::AgentMailCoordination => "agent_mail_coordination_evaluator",
        RehearsalCriterionKind::DirtyOverlapOwnership => "dirty_overlap_ownership_evaluator",
        RehearsalCriterionKind::ResourceEnvelope => "resource_envelope_evaluator",
        RehearsalCriterionKind::LatencyThroughput => "latency_throughput_evaluator",
        RehearsalCriterionKind::ArtifactIntegrity => "artifact_integrity_evaluator",
    }
}

fn kind_priority(kind: RehearsalCriterionKind) -> u8 {
    match kind {
        RehearsalCriterionKind::SafetyPolicy => 0,
        RehearsalCriterionKind::RedactionPrivacy => 1,
        RehearsalCriterionKind::RchProof => 2,
        RehearsalCriterionKind::DirtyOverlapOwnership => 3,
        RehearsalCriterionKind::ResourceEnvelope => 4,
        RehearsalCriterionKind::AgentMailCoordination => 5,
        RehearsalCriterionKind::ScenarioCompletion => 6,
        RehearsalCriterionKind::LatencyThroughput => 7,
        RehearsalCriterionKind::ArtifactIntegrity => 8,
    }
}

fn worst_evidence_state(evidence: &[RehearsalEvidenceRef]) -> Option<RehearsalEvidenceState> {
    evidence
        .iter()
        .map(|evidence| evidence.state)
        .max_by_key(|state| evidence_state_rank(*state))
}

fn evidence_state_rank(state: RehearsalEvidenceState) -> u8 {
    match state {
        RehearsalEvidenceState::Missing => 5,
        RehearsalEvidenceState::Blocked => 4,
        RehearsalEvidenceState::Simulated => 3,
        RehearsalEvidenceState::Degraded => 2,
        RehearsalEvidenceState::Redacted => 1,
        RehearsalEvidenceState::Proven | RehearsalEvidenceState::FixtureOnly => 0,
    }
}

fn verdict_severity(verdict: RehearsalVerdict) -> u8 {
    match verdict {
        RehearsalVerdict::Fail => 5,
        RehearsalVerdict::Blocked => 4,
        RehearsalVerdict::MissingEvidence => 3,
        RehearsalVerdict::Degraded => 2,
        RehearsalVerdict::Skipped => 1,
        RehearsalVerdict::Pass | RehearsalVerdict::NotApplicable => 0,
    }
}

fn criterion_text(criterion: &RehearsalCriterionReceipt) -> String {
    let mut parts = vec![
        criterion.criterion_id.clone(),
        criterion.kind.as_str().to_string(),
        criterion.verdict.as_str().to_string(),
        criterion.note.clone(),
    ];
    parts.extend(criterion.blockers.iter().cloned());
    parts.extend(criterion.next_actions.iter().cloned());
    for evidence in &criterion.evidence {
        parts.push(evidence.source.clone());
        parts.push(evidence.reference.clone());
        parts.push(evidence.state.as_str().to_string());
        if let Some(digest) = &evidence.digest {
            parts.push(digest.clone());
        }
    }
    parts.join("\n")
}

fn contains_marker(text: &str, markers: &[&str]) -> bool {
    let text = text.to_ascii_lowercase();
    markers.iter().any(|marker| text.contains(marker))
}

fn missing_evidence_count(criteria: &[RehearsalCriterionReceipt]) -> u32 {
    saturated_len(
        criteria
            .iter()
            .filter(|criterion| criterion.verdict == RehearsalVerdict::MissingEvidence)
            .count(),
    )
}

fn default_confidence_percent() -> u8 {
    100
}

fn demo_manifest_redaction_state(
    manifest: &DemoScenarioManifest,
) -> RehearsalAdapterRedactionState {
    if manifest
        .scenarios
        .iter()
        .any(|scenario| scenario.redaction_tier == DemoScenarioRedactionTier::T2Restricted)
    {
        return RehearsalAdapterRedactionState::Unknown;
    }
    if manifest
        .scenarios
        .iter()
        .any(|scenario| scenario.redaction_tier == DemoScenarioRedactionTier::T1Standard)
    {
        return RehearsalAdapterRedactionState::Redacted;
    }

    RehearsalAdapterRedactionState::Public
}

fn demo_scenario_completion_criterion(scenario: &DemoScenarioSpec) -> RehearsalCriterionReceipt {
    RehearsalCriterionReceipt::new(
        format!("demo.{}.scenario_completion", scenario.id),
        RehearsalCriterionKind::ScenarioCompletion,
        RehearsalVerdict::MissingEvidence,
    )
    .with_confidence_percent(70)
    .with_evidence(RehearsalEvidenceRef::new(
        "demo_scenario_manifest",
        scenario.scenario_path.clone(),
        RehearsalEvidenceState::FixtureOnly,
    ))
    .with_blocker("demo_lab.run_output_missing")
    .with_next_action(format!(
        "attach a validated run artifact for demo scenario `{}`",
        scenario.id
    ))
    .with_note("manifest metadata is read-only input; it does not prove scenario execution")
}

fn demo_artifact_criterion(
    scenario_id: &str,
    artifact: &DemoScenarioArtifact,
) -> RehearsalCriterionReceipt {
    let digest = artifact
        .sha256
        .as_ref()
        .map(|sha256| format!("sha256:{sha256}"));
    let mut evidence = RehearsalEvidenceRef::new(
        "demo_scenario_manifest",
        artifact.path.clone(),
        RehearsalEvidenceState::FixtureOnly,
    );
    if let Some(digest) = digest {
        evidence = evidence.with_digest(digest);
    }

    let missing_required_hash = artifact.content_hash_required && artifact.sha256.is_none();
    let verdict = if missing_required_hash {
        RehearsalVerdict::MissingEvidence
    } else {
        RehearsalVerdict::Pass
    };
    let mut criterion = RehearsalCriterionReceipt::new(
        format!("demo.{scenario_id}.artifact.{}", artifact.id),
        RehearsalCriterionKind::ArtifactIntegrity,
        verdict,
    )
    .with_evidence(evidence);

    if missing_required_hash {
        criterion = criterion
            .with_confidence_percent(75)
            .with_blocker(format!(
                "demo artifact `{}` requires a content hash but the manifest does not pin one",
                artifact.path
            ))
            .with_next_action(format!(
                "record or attach the digest for demo artifact `{}`",
                artifact.id
            ));
    }

    criterion
}

fn demo_degradation_policy_criterion(
    scenario: &DemoScenarioSpec,
    source_ref: &str,
    reason: DemoScenarioDegradationReason,
    kind: RehearsalCriterionKind,
) -> Option<RehearsalCriterionReceipt> {
    scenario
        .degradation
        .iter()
        .find(|degradation| degradation.reason == reason)
        .map(|degradation| {
            RehearsalCriterionReceipt::new(
                format!("demo.{}.degradation.{reason}", scenario.id),
                kind,
                RehearsalVerdict::Pass,
            )
            .with_evidence(RehearsalEvidenceRef::new(
                "demo_scenario_manifest",
                format!("{source_ref}#scenario.{}.degradation.{reason}", scenario.id),
                RehearsalEvidenceState::FixtureOnly,
            ))
            .with_note(format!(
                "documented fail-closed status {:?}; this is not a live proof attempt",
                degradation.status
            ))
        })
}

fn saturated_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

fn percent(numerator: u32, denominator: u32) -> u8 {
    if denominator == 0 {
        return 0;
    }

    let raw =
        (u128::from(numerator) * 100 + (u128::from(denominator) / 2)) / u128::from(denominator);
    u8::try_from(raw.min(100)).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::{Number, Value};

    const GOLDEN_MATRIX_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rehearsal_score_receipt_golden_matrix.json"
    ));
    const DEMO_MANIFEST_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/demo-lab/manifest.v1.json"
    ));

    #[derive(Debug, Deserialize)]
    struct GoldenMatrix {
        schema_version: u16,
        generated_by: String,
        proof_target: String,
        cases: Vec<GoldenCase>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenCase {
        name: String,
        format: String,
        receipt: Value,
    }

    fn load_golden_matrix() -> GoldenMatrix {
        serde_json::from_str(GOLDEN_MATRIX_JSON)
            .expect("rehearsal score golden matrix fixture must parse")
    }

    fn blocked_remote_proof_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-blocked-rch",
            "demo_lab.remote_proof",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.completed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "demo_lab",
                    "artifact://demo-lab/rehearsal.jsonl",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "proof.remote",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Blocked,
                )
                .with_confidence_percent(80)
                .with_evidence(RehearsalEvidenceRef::new(
                    "rch",
                    "rch://vmi1293453/j-29870718577541150",
                    RehearsalEvidenceState::Blocked,
                ))
                .with_blocker("rch.ssh_timeout")
                .with_next_action("rerun focused proof on a warmed remote worker")
                .with_note("remote proof did not return a terminal verdict"),
            ],
        )
        .with_source_artifact(RehearsalEvidenceRef::new(
            "agent_mail",
            "thread://coordination-2026-06-03-purplecanyon",
            RehearsalEvidenceState::Proven,
        ))
        .with_generated_at("2026-06-03T05:18:56Z")
        .with_note("blocked proof remains visible in the aggregate rollup")
    }

    fn rch_no_local_fallback_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-rch-no-local-fallback",
            "demo_lab.remote_required",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.completed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "demo_lab",
                    "artifact://demo-lab/run.jsonl",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "proof.remote",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Blocked,
                )
                .with_confidence_percent(85)
                .with_evidence(RehearsalEvidenceRef::new(
                    "rch",
                    "job://proof-output#[RCH] local worker=null",
                    RehearsalEvidenceState::Proven,
                ))
                .with_blocker("rch.remote_proof_not_admissible")
                .with_next_action(
                    "rerun proof with RCH_REQUIRE_REMOTE=1 and retain only remote worker output",
                )
                .with_note("local fallback path did not reach a remote worker"),
            ],
        )
        .with_source_artifact(RehearsalEvidenceRef::new(
            "rch_admission",
            "rch://admission/no-admissible-workers",
            RehearsalEvidenceState::Blocked,
        ))
        .with_generated_at("2026-06-04T06:35:00Z")
        .with_note("local fallback output is retained as a blocker, not proof")
    }

    fn privacy_redaction_fail_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-redaction-fail",
            "policy_lab.redaction_guard",
            vec![
                RehearsalCriterionReceipt::new(
                    "policy.applied",
                    RehearsalCriterionKind::SafetyPolicy,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "policy_audit",
                    "audit://policy-denial/wa-mcp-0006",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "redaction.secret_absent",
                    RehearsalCriterionKind::RedactionPrivacy,
                    RehearsalVerdict::Fail,
                )
                .with_confidence_percent(95)
                .with_evidence(RehearsalEvidenceRef::new(
                    "read_path_matrix",
                    "docs/security/read-path-redaction-matrix.md",
                    RehearsalEvidenceState::Redacted,
                ))
                .with_blocker("secret token remained visible after outbound read")
                .with_next_action("capture failing payload and tighten redactor rule")
                .with_note("hard privacy failures dominate aggregate verdicts"),
            ],
        )
        .with_generated_at("2026-06-03T05:19:00Z")
    }

    fn toon_contract_shape_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-toon-contract",
            "operator_loop.toon_surface",
            vec![
                RehearsalCriterionReceipt::new(
                    "artifact.integrity",
                    RehearsalCriterionKind::ArtifactIntegrity,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(
                    RehearsalEvidenceRef::new(
                        "fixture_matrix",
                        "tests/fixtures/rehearsal_score_receipt_golden_matrix.json",
                        RehearsalEvidenceState::FixtureOnly,
                    )
                    .with_digest("sha256:fixture-contract"),
                ),
                RehearsalCriterionReceipt::new(
                    "agent_mail.coordination",
                    RehearsalCriterionKind::AgentMailCoordination,
                    RehearsalVerdict::Skipped,
                )
                .with_confidence_percent(60)
                .with_blocker("agent mail outage during rehearsal setup")
                .with_next_action("retry once and continue with bead/git handoff if still down"),
            ],
        )
        .with_generated_at("2026-06-03T05:19:10Z")
        .with_note("TOON output must preserve the same typed receipt vocabulary")
    }

    fn agent_mail_outage_fallback_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-agent-mail-outage",
            "coordination.agent_mail_outage_fallback",
            vec![
                RehearsalCriterionReceipt::new(
                    "fallback.handoff_recorded",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "swarm_tick",
                    "fallback://agent-mail-unavailable/frankenterm",
                    RehearsalEvidenceState::Proven,
                ))
                .with_note("Beads/git handoff was retained while Agent Mail was unavailable"),
                RehearsalCriterionReceipt::new(
                    "agent_mail.outage",
                    RehearsalCriterionKind::AgentMailCoordination,
                    RehearsalVerdict::Degraded,
                )
                .with_confidence_percent(75)
                .with_evidence(RehearsalEvidenceRef::new(
                    "agent_mail",
                    "health://mcp-agent-mail/unreachable-after-one-retry",
                    RehearsalEvidenceState::Degraded,
                ))
                .with_blocker("agent_mail.unavailable_after_retry")
                .with_next_action(
                    "continue with scripts/swarm-tick.sh --agent-mail-fallback frankenterm and Beads/git handoff",
                )
                .with_note(
                    "fallback is acceptable only after one retry and retained fallback evidence",
                ),
            ],
        )
        .with_source_artifact(RehearsalEvidenceRef::new(
            "swarm_tick",
            "fallback://agent-mail-unavailable/frankenterm",
            RehearsalEvidenceState::Proven,
        ))
        .with_generated_at("2026-06-04T06:35:30Z")
        .with_note("Agent Mail fallback remains visible instead of being collapsed into pass")
    }

    fn source_adapter_missing_evidence_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-source-adapters",
            "artifact_bundle.proof_replay",
            vec![
                RehearsalCriterionReceipt::new(
                    "proof.replay.remote_verdict",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Blocked,
                )
                .with_confidence_percent(85)
                .with_evidence(RehearsalEvidenceRef::new(
                    "proof_replay",
                    "fixtures/deferred-proof-replay/receipt/valid/cases.v1.json#case.critical_pressure",
                    RehearsalEvidenceState::Blocked,
                ))
                .with_blocker("rch.critical_pressure")
                .with_next_action("wait for operator-approved RCH worker recovery")
                .with_note(
                    "deferred proof replay receipt records blocked state rather than green proof",
                ),
                RehearsalCriterionReceipt::new(
                    "envelope.target_class",
                    RehearsalCriterionKind::ResourceEnvelope,
                    RehearsalVerdict::Skipped,
                )
                .with_confidence_percent(80)
                .with_evidence(RehearsalEvidenceRef::new(
                    "operating_envelope",
                    "fixtures/operating-envelope/valid/target-hardware-skipped.json",
                    RehearsalEvidenceState::FixtureOnly,
                ))
                .with_blocker("target_hardware.skipped_not_proven")
                .with_next_action(
                    "retain skipped state until target-class proof artifact exists",
                )
                .with_note(
                    "operating envelope fixture is advisory until target-class proof graduates",
                ),
                RehearsalCriterionReceipt::new(
                    "agent_mail.delivery_receipt",
                    RehearsalCriterionKind::AgentMailCoordination,
                    RehearsalVerdict::MissingEvidence,
                )
                .with_confidence_percent(70)
                .with_evidence(RehearsalEvidenceRef::new(
                    "agent_mail_fallback",
                    "artifacts/agent-mail/outbox/replay.json",
                    RehearsalEvidenceState::Missing,
                ))
                .with_blocker("agent_mail.replay_receipt_missing")
                .with_next_action("retry Agent Mail once, then record Beads/git fallback handoff"),
            ],
        )
        .with_source_artifact(RehearsalEvidenceRef::new(
            "proof_replay",
            "fixtures/deferred-proof-replay/receipt/valid/cases.v1.json",
            RehearsalEvidenceState::FixtureOnly,
        ))
        .with_source_artifact(RehearsalEvidenceRef::new(
            "operating_envelope",
            "fixtures/operating-envelope/valid/target-hardware-skipped.json",
            RehearsalEvidenceState::FixtureOnly,
        ))
        .with_source_artifact(RehearsalEvidenceRef::new(
            "agent_mail_fallback",
            "artifacts/agent-mail/outbox/replay.json",
            RehearsalEvidenceState::Missing,
        ))
        .with_generated_at("2026-06-03T07:30:00Z")
        .with_note("source adapters preserve blocked, skipped, and missing evidence without side effects")
    }

    fn successful_bundled_rehearsal_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-bundled-success",
            "demo_lab.quickstart.no_mock",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.completed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(
                    RehearsalEvidenceRef::new(
                        "demo_lab",
                        "fixtures/demo-lab/golden/quickstart.json",
                        RehearsalEvidenceState::Proven,
                    )
                    .with_digest(
                        "sha256:17c1881cb23c0fd4997968cb575a10568f5aa9c88aba8e8e5318cf10f4081be4",
                    ),
                ),
                RehearsalCriterionReceipt::new(
                    "proof.remote",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "rch",
                    "rch://vmi1293453/j-29871232832766187#[RCH] remote vmi1293453",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "agent_mail.coordination",
                    RehearsalCriterionKind::AgentMailCoordination,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "agent_mail",
                    "thread://ft-oohsx.5/claim",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "resource.envelope",
                    RehearsalCriterionKind::ResourceEnvelope,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "operating_envelope",
                    "docs/attestations/proofs/resource-cockpit-target-class.json",
                    RehearsalEvidenceState::Proven,
                )),
            ],
        )
        .with_source_artifact(RehearsalEvidenceRef::new(
            "demo_scenario_manifest",
            "fixtures/demo-lab/manifest.v1.json#scenario.quickstart",
            RehearsalEvidenceState::FixtureOnly,
        ))
        .with_generated_at("2026-06-04T06:30:00Z")
        .with_note("no-mock harness uses the bundled demo manifest and retained quickstart golden")
    }

    fn dirty_overlap_ownership_risk_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-dirty-overlap",
            "coordination.dirty_overlap",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.claimed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "beads",
                    "bead://ft-oohsx.5/status/in_progress",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "git.dirty_overlap",
                    RehearsalCriterionKind::DirtyOverlapOwnership,
                    RehearsalVerdict::Blocked,
                )
                .with_confidence_percent(80)
                .with_evidence(RehearsalEvidenceRef::new(
                    "git_status",
                    "git://status#crates/frankenterm-core/src/rehearsal_score.rs",
                    RehearsalEvidenceState::Blocked,
                ))
                .with_blocker("dirty_overlap.owner_unclear")
                .with_next_action("confirm ownership or narrow the patch before claiming proof")
                .with_note("shared dirty files must block ownership-sensitive rehearsals"),
            ],
        )
        .with_source_artifact(RehearsalEvidenceRef::new(
            "swarm_tick",
            "fallback://agent-mail-unavailable/beads-only",
            RehearsalEvidenceState::Proven,
        ))
        .with_generated_at("2026-06-04T06:31:00Z")
    }

    fn policy_require_approval_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-policy-approval",
            "policy.require_approval",
            vec![
                RehearsalCriterionReceipt::new(
                    "policy.deny_recorded",
                    RehearsalCriterionKind::SafetyPolicy,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "policy_audit",
                    "audit://policy-denial/wa-mcp-approval-required",
                    RehearsalEvidenceState::Proven,
                ))
                .with_note("deny or require-approval decisions are acceptable when retained"),
                RehearsalCriterionReceipt::new(
                    "policy.requires_operator",
                    RehearsalCriterionKind::SafetyPolicy,
                    RehearsalVerdict::Blocked,
                )
                .with_confidence_percent(85)
                .with_evidence(RehearsalEvidenceRef::new(
                    "policy_audit",
                    "audit://policy-denial/wa-mcp-approval-required#operator",
                    RehearsalEvidenceState::Proven,
                ))
                .with_blocker("policy.require_approval")
                .with_next_action("request explicit operator approval before executing the action"),
            ],
        )
        .with_generated_at("2026-06-04T06:32:00Z")
    }

    fn redaction_missing_evidence_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-redaction-missing",
            "privacy.redaction_missing",
            vec![
                RehearsalCriterionReceipt::new(
                    "redaction.intent",
                    RehearsalCriterionKind::RedactionPrivacy,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "read_path_matrix",
                    "docs/security/read-path-redaction-matrix.md",
                    RehearsalEvidenceState::Redacted,
                )),
                RehearsalCriterionReceipt::new(
                    "redaction.audit_artifact",
                    RehearsalCriterionKind::RedactionPrivacy,
                    RehearsalVerdict::MissingEvidence,
                )
                .with_confidence_percent(70)
                .with_evidence(RehearsalEvidenceRef::new(
                    "redaction_audit",
                    "artifacts/rehearsal-score/redaction-audit.json",
                    RehearsalEvidenceState::Missing,
                ))
                .with_blocker("redaction.audit_missing")
                .with_next_action(
                    "attach redaction audit evidence before treating the rehearsal as proven",
                ),
            ],
        )
        .with_generated_at("2026-06-04T06:33:00Z")
    }

    fn resource_pressure_degraded_receipt() -> RehearsalScoreReceipt {
        RehearsalScoreReceipt::new(
            "rehearsal-resource-pressure",
            "operating_envelope.resource_pressure",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.completed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "demo_lab",
                    "fixtures/demo-lab/golden/usage_limit.json",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "resource.pressure",
                    RehearsalCriterionKind::ResourceEnvelope,
                    RehearsalVerdict::Degraded,
                )
                .with_confidence_percent(75)
                .with_evidence(RehearsalEvidenceRef::new(
                    "operating_envelope",
                    "metrics://resource-envelope/memory-pressure",
                    RehearsalEvidenceState::Degraded,
                ))
                .with_blocker("resource.pressure_degraded")
                .with_next_action(
                    "rerun with retained resource envelope below pressure thresholds",
                ),
            ],
        )
        .with_generated_at("2026-06-04T06:34:00Z")
    }

    fn expected_receipt(case_name: &str) -> RehearsalScoreReceipt {
        match case_name {
            "blocked_remote_proof" => blocked_remote_proof_receipt(),
            "rch_no_local_fallback" => rch_no_local_fallback_receipt(),
            "privacy_redaction_fail" => privacy_redaction_fail_receipt(),
            "toon_contract_shape" => toon_contract_shape_receipt(),
            "agent_mail_outage_fallback" => agent_mail_outage_fallback_receipt(),
            "source_adapter_missing_evidence" => source_adapter_missing_evidence_receipt(),
            "successful_bundled_rehearsal" => successful_bundled_rehearsal_receipt(),
            "dirty_overlap_ownership_risk" => dirty_overlap_ownership_risk_receipt(),
            "policy_require_approval" => policy_require_approval_receipt(),
            "redaction_missing_evidence" => redaction_missing_evidence_receipt(),
            "resource_pressure_degraded" => resource_pressure_degraded_receipt(),
            other => panic!("unexpected rehearsal score golden case {other}"),
        }
    }

    fn load_demo_manifest() -> DemoScenarioManifest {
        DemoScenarioManifest::from_json(DEMO_MANIFEST_JSON)
            .expect("bundled demo scenario manifest fixture must parse and validate")
    }

    fn find_criterion<'a>(
        extraction: &'a RehearsalAdapterExtraction,
        criterion_id: &str,
    ) -> &'a RehearsalCriterionReceipt {
        extraction
            .criteria
            .iter()
            .find(|criterion| criterion.criterion_id == criterion_id)
            .unwrap_or_else(|| panic!("missing criterion {criterion_id}"))
    }

    fn find_receipt_criterion<'a>(
        receipt: &'a RehearsalScoreReceipt,
        criterion_id: &str,
    ) -> &'a RehearsalCriterionReceipt {
        receipt
            .criteria
            .iter()
            .find(|criterion| criterion.criterion_id == criterion_id)
            .unwrap_or_else(|| panic!("missing receipt criterion {criterion_id}"))
    }

    fn normalize_integral_toon_numbers(value: &mut Value) {
        match value {
            Value::Array(items) => {
                for item in items {
                    normalize_integral_toon_numbers(item);
                }
            }
            Value::Object(entries) => {
                for nested in entries.values_mut() {
                    normalize_integral_toon_numbers(nested);
                }
            }
            Value::Number(number) => {
                let Some(raw) = number.as_f64() else {
                    return;
                };
                if raw.is_finite() && raw.fract() == 0.0 && raw >= 0.0 && raw <= u64::MAX as f64 {
                    *value = Value::Number(Number::from(raw as u64));
                }
            }
            Value::Null | Value::Bool(_) | Value::String(_) => {}
        }
    }

    #[test]
    fn receipt_serializes_contract_and_snake_case_verdicts() {
        let receipt = RehearsalScoreReceipt::new(
            "rehearsal-001",
            "demo-lab.quickstart",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.completed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "demo_lab",
                    "artifact://demo/quickstart.jsonl",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "proof.remote",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Blocked,
                )
                .with_confidence_percent(250)
                .with_blocker("rch.ssh_timeout")
                .with_next_action("rerun with smaller package-scoped proof")
                .with_note("remote worker timed out before test verdict"),
            ],
        )
        .with_source_artifact(
            RehearsalEvidenceRef::new(
                "proof_replay",
                "bead://ft-wjjkp.1/comment/6259",
                RehearsalEvidenceState::Blocked,
            )
            .with_digest("sha256:example"),
        )
        .with_generated_at("2026-06-03T03:20:00Z")
        .with_note("fixture keeps blocked proof visible");

        let value = serde_json::to_value(&receipt).expect("serialize receipt");

        assert_eq!(value["schema_version"], REHEARSAL_SCORE_SCHEMA_VERSION);
        assert_eq!(value["contract_id"], REHEARSAL_SCORE_RECEIPT_CONTRACT_ID);
        assert_eq!(value["aggregate_verdict"], "blocked");
        assert_eq!(value["criteria"][1]["kind"], "rch_proof");
        assert_eq!(value["criteria"][1]["verdict"], "blocked");
        assert_eq!(value["criteria"][1]["confidence_percent"], 100);
        assert_eq!(value["source_artifacts"][0]["state"], "blocked");
    }

    #[test]
    fn aggregate_verdict_fails_closed_in_severity_order() {
        let blocked = RehearsalCriterionReceipt::new(
            "proof.remote",
            RehearsalCriterionKind::RchProof,
            RehearsalVerdict::Blocked,
        );
        let missing = RehearsalCriterionReceipt::new(
            "mail.evidence",
            RehearsalCriterionKind::AgentMailCoordination,
            RehearsalVerdict::MissingEvidence,
        );
        let failed = RehearsalCriterionReceipt::new(
            "redaction.secret_absent",
            RehearsalCriterionKind::RedactionPrivacy,
            RehearsalVerdict::Fail,
        );

        assert_eq!(
            aggregate_verdict(&[blocked.clone(), missing.clone()]),
            RehearsalVerdict::Blocked
        );
        assert_eq!(
            aggregate_verdict(&[blocked, missing, failed]),
            RehearsalVerdict::Fail
        );
    }

    #[test]
    fn aggregate_score_excludes_not_applicable_and_counts_only_passes() {
        let criteria = vec![
            RehearsalCriterionReceipt::new(
                "scenario.completed",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Pass,
            ),
            RehearsalCriterionReceipt::new(
                "latency.slo",
                RehearsalCriterionKind::LatencyThroughput,
                RehearsalVerdict::Degraded,
            ),
            RehearsalCriterionReceipt::new(
                "agent_mail.outage",
                RehearsalCriterionKind::AgentMailCoordination,
                RehearsalVerdict::NotApplicable,
            ),
        ];

        let score = RehearsalAggregateScore::from_criteria(&criteria);

        assert_eq!(score.total_criteria, 3);
        assert_eq!(score.scorable_criteria, 2);
        assert_eq!(score.passed, 1);
        assert_eq!(score.degraded, 1);
        assert_eq!(score.not_applicable, 1);
        assert_eq!(score.score_percent, 50);
    }

    #[test]
    fn adapter_kind_registry_covers_declared_source_families() {
        let kinds = RehearsalSourceAdapterKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        for expected in [
            "demo_scenario_manifest",
            "flight_recorder",
            "mission_twin_snapshot",
            "operating_envelope",
            "proof_replay",
            "rch_admission",
            "agent_mail_fallback",
        ] {
            assert!(kinds.contains(expected), "missing adapter kind {expected}");
        }
    }

    #[test]
    fn demo_manifest_adapter_extracts_fixture_backed_missing_evidence() {
        let manifest = load_demo_manifest();
        let extraction = RehearsalAdapterExtraction::from_demo_scenario_manifest(
            &manifest,
            "fixtures/demo-lab/manifest.v1.json",
        );

        assert_eq!(
            extraction.adapter_kind,
            RehearsalSourceAdapterKind::DemoScenarioManifest
        );
        assert_eq!(
            extraction.source_schema_version,
            DEMO_SCENARIO_MANIFEST_SCHEMA_VERSION
        );
        assert_eq!(
            extraction.redaction_state,
            RehearsalAdapterRedactionState::Redacted
        );
        assert_eq!(
            extraction.log.extracted_criteria_count,
            saturated_len(extraction.criteria.len())
        );
        assert_eq!(
            extraction.log.missing_evidence_count,
            extraction.missing_evidence_count
        );
        assert!(
            extraction.missing_evidence_count > 0,
            "manifest-only extraction must not claim a scenario run happened"
        );

        let quickstart_completion =
            find_criterion(&extraction, "demo.quickstart.scenario_completion");
        assert_eq!(
            quickstart_completion.verdict,
            RehearsalVerdict::MissingEvidence
        );
        assert!(
            quickstart_completion
                .blockers
                .contains(&"demo_lab.run_output_missing".to_string())
        );

        let quickstart_yaml = find_criterion(&extraction, "demo.quickstart.artifact.scenario_yaml");
        assert_eq!(quickstart_yaml.verdict, RehearsalVerdict::Pass);
        assert_eq!(
            quickstart_yaml.evidence[0].digest.as_deref(),
            Some("sha256:7f33c947552bbecf774a3172200b44fd9e014c0a1b7365e37bbea04a9b1c845b")
        );

        let usage_limit_log = find_criterion(&extraction, "demo.usage_limit.artifact.proof_ledger");
        assert_eq!(usage_limit_log.verdict, RehearsalVerdict::MissingEvidence);
        assert_eq!(
            usage_limit_log.evidence[0].reference,
            "fixtures/demo-lab/proof/proof-ledger.v1.jsonl"
        );

        let fallback_policy = find_criterion(
            &extraction,
            "demo.compaction.degradation.agent_mail_unavailable",
        );
        assert_eq!(
            fallback_policy.kind,
            RehearsalCriterionKind::AgentMailCoordination
        );
        assert_eq!(fallback_policy.verdict, RehearsalVerdict::Pass);
        assert!(
            fallback_policy
                .note
                .contains("this is not a live proof attempt")
        );
    }

    #[test]
    fn adapter_extraction_builds_receipt_without_losing_source_log() {
        let manifest = load_demo_manifest();
        let extraction = RehearsalAdapterExtraction::from_demo_scenario_manifest(
            &manifest,
            "fixtures/demo-lab/manifest.v1.json",
        );
        let missing_evidence_count = extraction.missing_evidence_count;

        let receipt = extraction.into_receipt("rehearsal-demo-adapter", "demo_lab.manifest");

        assert_eq!(receipt.rehearsal_id, "rehearsal-demo-adapter");
        assert_eq!(receipt.scenario_id, "demo_lab.manifest");
        assert_eq!(receipt.aggregate_verdict, RehearsalVerdict::MissingEvidence);
        assert_eq!(
            receipt.aggregate_score.missing_evidence,
            missing_evidence_count
        );
        assert_eq!(
            receipt.source_artifacts[0].reference,
            "fixtures/demo-lab/manifest.v1.json"
        );
        assert!(receipt.notes[0].contains("demo_scenario_manifest.v1 extracted"));
    }

    #[test]
    fn source_observation_adapters_preserve_read_only_artifact_states() {
        let proof_replay = RehearsalAdapterExtraction::from_source_observations(
            "proof_replay.v1",
            RehearsalSourceAdapterKind::ProofReplay,
            "fixtures/deferred-proof-replay/receipt/valid/cases.v1.json",
            "ft.deferred_proof_replay.receipt.v1",
            vec![
                RehearsalSourceObservation::new(
                    "proof.replay.remote_verdict",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Blocked,
                    RehearsalEvidenceRef::new(
                        "proof_replay",
                        "fixtures/deferred-proof-replay/receipt/valid/cases.v1.json#case.critical_pressure",
                        RehearsalEvidenceState::Blocked,
                    ),
                )
                .with_confidence_percent(85)
                .with_blocker("rch.critical_pressure")
                .with_next_action("wait for operator-approved RCH worker recovery")
                .with_note(
                    "deferred proof replay receipt records blocked state rather than green proof",
                ),
            ],
            vec![RehearsalEvidenceRef::new(
                "proof_replay",
                "fixtures/deferred-proof-replay/receipt/valid/cases.v1.json",
                RehearsalEvidenceState::FixtureOnly,
            )],
            RehearsalAdapterRedactionState::Redacted,
        );
        let envelope = RehearsalAdapterExtraction::from_source_observations(
            "operating_envelope.v1",
            RehearsalSourceAdapterKind::OperatingEnvelope,
            "fixtures/operating-envelope/valid/target-hardware-skipped.json",
            "ft.operating_envelope.v1",
            vec![
                RehearsalSourceObservation::new(
                    "envelope.target_class",
                    RehearsalCriterionKind::ResourceEnvelope,
                    RehearsalVerdict::Skipped,
                    RehearsalEvidenceRef::new(
                        "operating_envelope",
                        "fixtures/operating-envelope/valid/target-hardware-skipped.json",
                        RehearsalEvidenceState::FixtureOnly,
                    ),
                )
                .with_confidence_percent(80)
                .with_blocker("target_hardware.skipped_not_proven")
                .with_next_action("retain skipped state until target-class proof artifact exists"),
            ],
            vec![RehearsalEvidenceRef::new(
                "operating_envelope",
                "fixtures/operating-envelope/valid/target-hardware-skipped.json",
                RehearsalEvidenceState::FixtureOnly,
            )],
            RehearsalAdapterRedactionState::Public,
        );
        let mail = RehearsalAdapterExtraction::from_source_observations(
            "agent_mail_fallback.v1",
            RehearsalSourceAdapterKind::AgentMailFallback,
            "artifacts/agent-mail/outbox/replay.json",
            "ft.agent_mail_outbox.v1",
            vec![RehearsalSourceObservation::missing_required(
                "agent_mail.delivery_receipt",
                RehearsalCriterionKind::AgentMailCoordination,
                "agent_mail_fallback",
                "artifacts/agent-mail/outbox/replay.json",
                "agent_mail.replay_receipt_missing",
                "retry Agent Mail once, then record Beads/git fallback handoff",
            )],
            vec![RehearsalEvidenceRef::new(
                "agent_mail_fallback",
                "artifacts/agent-mail/outbox/replay.json",
                RehearsalEvidenceState::Missing,
            )],
            RehearsalAdapterRedactionState::Unknown,
        );

        assert_eq!(
            proof_replay.adapter_kind,
            RehearsalSourceAdapterKind::ProofReplay
        );
        assert_eq!(proof_replay.log.extracted_criteria_count, 1);
        assert_eq!(proof_replay.missing_evidence_count, 0);
        assert_eq!(
            find_criterion(&proof_replay, "proof.replay.remote_verdict").verdict,
            RehearsalVerdict::Blocked
        );

        assert_eq!(
            find_criterion(&envelope, "envelope.target_class").verdict,
            RehearsalVerdict::Skipped
        );
        assert_eq!(
            envelope.source_artifacts[0].reference,
            "fixtures/operating-envelope/valid/target-hardware-skipped.json"
        );

        assert_eq!(mail.missing_evidence_count, 1);
        let delivery = find_criterion(&mail, "agent_mail.delivery_receipt");
        assert_eq!(delivery.verdict, RehearsalVerdict::MissingEvidence);
        assert_eq!(delivery.evidence[0].state, RehearsalEvidenceState::Missing);
        assert!(delivery.next_actions.contains(
            &"retry Agent Mail once, then record Beads/git fallback handoff".to_string()
        ));
    }

    #[test]
    fn scoring_engine_evaluator_registry_covers_all_criterion_kinds() {
        let evaluators = RehearsalCriterionKind::ALL
            .iter()
            .map(|kind| evaluator_for_kind(*kind))
            .collect::<std::collections::BTreeSet<_>>();

        for expected in [
            "scenario_completion_evaluator",
            "safety_policy_evaluator",
            "redaction_privacy_evaluator",
            "rch_proof_evaluator",
            "agent_mail_coordination_evaluator",
            "dirty_overlap_ownership_evaluator",
            "resource_envelope_evaluator",
            "latency_throughput_evaluator",
            "artifact_integrity_evaluator",
        ] {
            assert!(
                evaluators.contains(expected),
                "missing evaluator {expected}"
            );
        }
    }

    #[test]
    fn scoring_engine_blocks_local_rch_fallback_even_when_claimed_pass() {
        let result = RehearsalScoringEngine::score_criteria(
            "rehearsal-local-fallback",
            "remote_proof.required",
            vec![
                RehearsalCriterionReceipt::new(
                    "scenario.completed",
                    RehearsalCriterionKind::ScenarioCompletion,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "demo_lab",
                    "artifact://demo-lab/run.jsonl",
                    RehearsalEvidenceState::Proven,
                )),
                RehearsalCriterionReceipt::new(
                    "proof.remote",
                    RehearsalCriterionKind::RchProof,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "rch",
                    "job://proof-output#[RCH] local worker=null",
                    RehearsalEvidenceState::Proven,
                ))
                .with_note("local fallback path did not reach a remote worker"),
            ],
        );

        let proof = find_receipt_criterion(&result.receipt, "proof.remote");
        assert_eq!(proof.verdict, RehearsalVerdict::Blocked);
        assert!(
            proof
                .blockers
                .contains(&"rch.remote_proof_not_admissible".to_string())
        );
        assert_eq!(result.receipt.aggregate_verdict, RehearsalVerdict::Blocked);
        assert_eq!(result.next_action_hints[0].criterion_id, "proof.remote");
        assert_eq!(result.log[1].evaluator, "rch_proof_evaluator");
        assert_eq!(
            result.log[1].rollup_contribution,
            RehearsalRollupContribution::Blocked
        );
    }

    #[test]
    fn scoring_engine_turns_redaction_uncertainty_into_missing_evidence() {
        let extraction = RehearsalAdapterExtraction::from_source_observations(
            "agent_mail_fallback.v1",
            RehearsalSourceAdapterKind::AgentMailFallback,
            "artifacts/agent-mail/outbox/replay.json",
            "ft.agent_mail_outbox.v1",
            vec![RehearsalSourceObservation::new(
                "agent_mail.delivery_receipt",
                RehearsalCriterionKind::AgentMailCoordination,
                RehearsalVerdict::Pass,
                RehearsalEvidenceRef::new(
                    "agent_mail_fallback",
                    "artifacts/agent-mail/outbox/replay.json#delivery",
                    RehearsalEvidenceState::Proven,
                ),
            )],
            vec![RehearsalEvidenceRef::new(
                "agent_mail_fallback",
                "artifacts/agent-mail/outbox/replay.json",
                RehearsalEvidenceState::FixtureOnly,
            )],
            RehearsalAdapterRedactionState::Unknown,
        );

        let result = RehearsalScoringEngine::score_extraction(
            extraction,
            "rehearsal-redaction-unknown",
            "mail.fallback",
        );
        let redaction =
            find_receipt_criterion(&result.receipt, "agent_mail_fallback.v1.redaction_state");

        assert_eq!(redaction.verdict, RehearsalVerdict::MissingEvidence);
        assert!(
            redaction
                .next_actions
                .contains(&"attach privacy classification or redaction audit evidence".to_string())
        );
        assert_eq!(
            result.receipt.aggregate_verdict,
            RehearsalVerdict::MissingEvidence
        );
        assert_eq!(result.receipt.source_artifacts.len(), 1);
        assert!(
            result
                .next_action_hints
                .iter()
                .any(|hint| hint.criterion_id == "agent_mail_fallback.v1.redaction_state")
        );
    }

    #[test]
    fn scoring_engine_fails_for_unsafe_side_effect_hints() {
        let result = RehearsalScoringEngine::score_criteria(
            "rehearsal-side-effect",
            "policy.side_effect_guard",
            vec![
                RehearsalCriterionReceipt::new(
                    "policy.side_effect",
                    RehearsalCriterionKind::SafetyPolicy,
                    RehearsalVerdict::Pass,
                )
                .with_evidence(RehearsalEvidenceRef::new(
                    "operator_action",
                    "mail://coordination#attempt",
                    RehearsalEvidenceState::Proven,
                ))
                .with_next_action("am service restart"),
            ],
        );

        let policy = find_receipt_criterion(&result.receipt, "policy.side_effect");
        assert_eq!(policy.verdict, RehearsalVerdict::Fail);
        assert!(
            policy
                .blockers
                .contains(&"side_effect.forbidden_command".to_string())
        );
        assert!(
            !policy
                .next_actions
                .contains(&"am service restart".to_string())
        );
        assert_eq!(
            result.next_action_hints[0].action,
            "remove unsafe side-effect action and rerun rehearsal with read-only evidence"
        );
        assert_eq!(
            result.log[0].rollup_contribution,
            RehearsalRollupContribution::CriticalFailure
        );
    }

    #[test]
    fn scoring_engine_scores_demo_manifest_adapter_fixture_fail_closed() {
        let manifest = load_demo_manifest();
        let extraction = RehearsalAdapterExtraction::from_demo_scenario_manifest(
            &manifest,
            "fixtures/demo-lab/manifest.v1.json",
        );

        let result = RehearsalScoringEngine::score_extraction(
            extraction,
            "rehearsal-demo-scored",
            "demo_lab.manifest",
        );

        assert_eq!(
            result.receipt.aggregate_verdict,
            RehearsalVerdict::MissingEvidence
        );
        assert!(result.aggregate_confidence_percent < 100);
        assert_eq!(result.log.len(), result.receipt.criteria.len());
        assert!(
            result
                .next_action_hints
                .iter()
                .any(|hint| hint.action.contains("attach a validated run artifact"))
        );
        assert_eq!(
            result.receipt.source_artifacts[0].reference,
            "fixtures/demo-lab/manifest.v1.json"
        );
    }

    #[test]
    fn score_surface_reports_read_only_contract_and_explain_log() {
        let manifest = load_demo_manifest();
        let score = RehearsalScoreSurfaceReport::from_demo_scenario_manifest(
            &manifest,
            "fixtures/demo-lab/manifest.v1.json",
            "surface-score-test",
            "demo_lab.manifest",
            RehearsalScoreSurface::Score,
        );
        let explain = RehearsalScoreSurfaceReport::from_demo_scenario_manifest(
            &manifest,
            "fixtures/demo-lab/manifest.v1.json",
            "surface-explain-test",
            "demo_lab.manifest",
            RehearsalScoreSurface::Explain,
        );

        assert_eq!(score.contract_id, REHEARSAL_SCORE_SURFACE_CONTRACT_ID);
        assert_eq!(score.surface, RehearsalScoreSurface::Score);
        assert_eq!(
            score.receipt.aggregate_verdict,
            RehearsalVerdict::MissingEvidence
        );
        assert_eq!(score.evaluation_log, [] as [rehearsal_score::RehearsalCriterionEvaluationLog; 0]);
        assert_ne!(explain.evaluation_log, [] as [rehearsal_score::RehearsalCriterionEvaluationLog; 0]);
        assert_eq!(explain.evaluation_log.len(), explain.receipt.criteria.len());
        assert!(!score.raw_pane_content_stored);
        assert!(!score.live_mutation_allowed);
        assert!(!score.side_effects_executed);
        assert_eq!(
            explain.source_adapter_log.adapter_id,
            "demo_scenario_manifest.v1"
        );
    }

    #[test]
    fn no_mock_bundled_demo_manifest_harness_log_records_jsonl_and_toon_surface() {
        let manifest = load_demo_manifest();
        let report = RehearsalScoreSurfaceReport::from_demo_scenario_manifest(
            &manifest,
            "fixtures/demo-lab/manifest.v1.json",
            "harness-no-mock-demo-manifest",
            "demo_lab.manifest",
            RehearsalScoreSurface::Score,
        );
        let log = RehearsalScoreHarnessLogEntry::from_surface_report(
            &report,
            vec![
                "ft rehearse score fixtures/demo-lab/manifest.v1.json --format json".to_string(),
                REHEARSAL_SCORE_MCP_CURRENT_URI.to_string(),
            ],
            12,
            vec![
                "bundled demo manifest path; side-effect-free no-mock adapter".to_string(),
                "raw pane content is never stored by the scoring surface".to_string(),
            ],
        );

        assert_eq!(log.contract_id, REHEARSAL_SCORE_HARNESS_LOG_CONTRACT_ID);
        assert_eq!(log.scenario_id, "demo_lab.manifest");
        assert_eq!(log.source_artifact_ids.len(), 1);
        assert!(log.commands_or_resources_queried.iter().any(|command| {
            command.starts_with("ft rehearse score fixtures/demo-lab/manifest.v1.json")
        }));
        assert!(
            log.commands_or_resources_queried
                .contains(&REHEARSAL_SCORE_MCP_CURRENT_URI.to_string())
        );
        assert_eq!(log.elapsed_ms, 12);
        assert!(
            log.resource_notes
                .iter()
                .any(|note| note.contains("side-effect-free no-mock adapter"))
        );
        assert_eq!(
            log.score_receipt.contract_id,
            REHEARSAL_SCORE_RECEIPT_CONTRACT_ID
        );
        assert_eq!(
            log.proof_status,
            RehearsalHarnessProofStatus::MissingEvidence
        );
        assert!(
            log.criterion_inputs
                .iter()
                .any(|input| input.kind == RehearsalCriterionKind::RchProof)
        );
        assert!(
            log.score_receipt
                .notes
                .iter()
                .any(|note| note.contains("score_engine evaluated adapter"))
        );

        let jsonl = log.to_jsonl_line().expect("serialize harness JSONL");
        assert!(
            !jsonl.contains('\n'),
            "one harness entry must serialize as exactly one JSONL line"
        );
        let decoded: RehearsalScoreHarnessLogEntry =
            serde_json::from_str(&jsonl).expect("decode harness JSONL line");
        assert_eq!(decoded, log);

        let surface_json = serde_json::to_value(&report).expect("serialize surface report");
        let surface_toon = toon_rust::encode(surface_json.clone(), None);
        let decoded_toon = toon_rust::try_decode(&surface_toon, None).expect("decode surface TOON");
        let decoded_json =
            toon_rust::cli::json_stringify::json_stringify_lines(&decoded_toon, 0).join("\n");
        let mut decoded_surface: Value =
            serde_json::from_str(&decoded_json).expect("surface TOON decoded JSON");
        normalize_integral_toon_numbers(&mut decoded_surface);

        assert_eq!(decoded_surface["contract_id"], surface_json["contract_id"]);
        assert_eq!(decoded_surface["surface"], surface_json["surface"]);
        assert_eq!(
            decoded_surface["side_effects_executed"],
            surface_json["side_effects_executed"]
        );
        assert_ne!(
            surface_toon.trim_start().chars().next(),
            Some('{'),
            "surface TOON output must not collapse back to JSON text"
        );
    }

    #[test]
    fn golden_fixture_matrix_is_current_and_covers_json_and_toon() {
        let matrix = load_golden_matrix();

        assert_eq!(matrix.schema_version, REHEARSAL_SCORE_SCHEMA_VERSION);
        assert_eq!(matrix.generated_by, "ft-oohsx.1-rehearsal-score-contract");
        assert!(
            matrix.proof_target.contains("RCH_REQUIRE_REMOTE=1")
                && matrix.proof_target.contains("RCH_NO_SELF_HEALING=1")
                && matrix
                    .proof_target
                    .contains("rch --no-self-healing exec -- env ")
                && !matrix.proof_target.contains("rch exec -- env "),
            "proof target must use fail-closed RCH: {}",
            matrix.proof_target
        );
        assert!(
            matrix.proof_target.contains(" cargo test "),
            "proof target must be a cargo test lane: {}",
            matrix.proof_target
        );

        let case_keys = matrix
            .cases
            .iter()
            .map(|case| (case.name.as_str(), case.format.as_str()))
            .collect::<std::collections::BTreeSet<_>>();

        for expected in [
            ("successful_bundled_rehearsal", "json"),
            ("blocked_remote_proof", "json"),
            ("rch_no_local_fallback", "json"),
            ("privacy_redaction_fail", "json"),
            ("toon_contract_shape", "toon"),
            ("agent_mail_outage_fallback", "json"),
            ("source_adapter_missing_evidence", "json"),
            ("dirty_overlap_ownership_risk", "json"),
            ("policy_require_approval", "json"),
            ("redaction_missing_evidence", "json"),
            ("resource_pressure_degraded", "json"),
        ] {
            assert!(
                case_keys.contains(&expected),
                "missing golden fixture case {expected:?}"
            );
        }
    }

    #[test]
    fn golden_receipts_match_contract_builders() {
        let matrix = load_golden_matrix();

        for case in matrix.cases {
            let expected = serde_json::to_value(expected_receipt(&case.name))
                .expect("serialize expected receipt");

            assert_eq!(case.receipt, expected, "golden case {}", case.name);
        }
    }

    #[test]
    fn golden_toon_case_round_trips_to_json_contract() {
        let matrix = load_golden_matrix();
        let toon_case = matrix
            .cases
            .iter()
            .find(|case| case.format == "toon")
            .expect("matrix must contain a TOON case");

        let toon = toon_rust::encode(toon_case.receipt.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).expect("decode golden receipt TOON");
        let json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
        let mut roundtripped: Value = serde_json::from_str(&json).expect("TOON decoded JSON");
        normalize_integral_toon_numbers(&mut roundtripped);

        assert_eq!(roundtripped, toon_case.receipt);
        assert_ne!(
            toon.trim_start().chars().next(),
            Some('{'),
            "TOON fixture must not collapse back to JSON text"
        );
    }
}
