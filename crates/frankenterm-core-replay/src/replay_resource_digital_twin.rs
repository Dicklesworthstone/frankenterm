//! Replay-backed resource-control digital twin engine.
//!
//! This module is intentionally analysis-only. It consumes a
//! [`DigitalTwinTrace`](crate::replay_scenario_matrix::DigitalTwinTrace), applies
//! a candidate [`ResourceControlOverridePackage`], and compares the resulting
//! baseline and candidate resource-control decision streams without touching
//! panes, storage, mux state, Agent Mail, or external services.

use crate::replay_counterfactual::{
    ResourceControlOverride, ResourceControlOverridePackage, ResourceOverrideAction,
    ResourceOverrideDomain,
};
use crate::replay_scenario_matrix::{
    DigitalTwinTrace, DigitalTwinTraceQualityFlag, DigitalTwinTraceSource, DigitalTwinTraceStep,
    ProofStatus, ScaleProofEvidenceSource,
};
use crate::replay_side_effect_barrier::{
    EffectRequest, EffectType, SideEffectBarrier, SideEffectLog,
};
use frankenterm_core::policy::ActionKind;
use frankenterm_core_replay_types::simulation_guard::SimulationGuard;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

/// Schema version for baseline-vs-candidate resource digital-twin outputs.
pub const RESOURCE_DIGITAL_TWIN_SCHEMA_VERSION: &str = "ft.resource_digital_twin.v1";
/// Schema version for checked-in digital-twin gate summary fixtures.
pub const RESOURCE_DIGITAL_TWIN_GATE_SUMMARY_SCHEMA_VERSION: &str =
    "ft.resource_digital_twin.gate_summary.v1";
/// Default minimum CPU predicate for high-scale proof claims.
pub const RESOURCE_DIGITAL_TWIN_HIGH_SCALE_MIN_CPU_COUNT: u32 = 64;
/// Default minimum RAM predicate for high-scale proof claims (256 GiB).
pub const RESOURCE_DIGITAL_TWIN_HIGH_SCALE_MIN_MEMORY_BYTES: u64 = 274_877_906_944;
/// Default minimum confidence score required before an apply recommendation.
pub const RESOURCE_DIGITAL_TWIN_DEFAULT_MIN_APPLY_CONFIDENCE_SCORE: u8 = 75;

/// Provenance for a simulated decision field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionProvenance {
    /// Copied from an observed trace field.
    Observed,
    /// Derived deterministically from observed telemetry and safe knobs.
    Estimated,
    /// Telemetry was missing or non-finite, so the engine refused to infer.
    Unknown,
}

/// Simulated admission outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulatedAdmissionAction {
    /// The work can proceed.
    Admit,
    /// The work should wait for pressure to ease.
    Defer,
    /// The work can proceed in a degraded or reduced-resource mode.
    Degrade,
    /// The work should be rejected or shed.
    Shed,
    /// The trace did not contain enough signal to decide.
    Unknown,
}

impl SimulatedAdmissionAction {
    #[must_use]
    const fn severity(self) -> u8 {
        match self {
            Self::Admit => 0,
            Self::Defer => 1,
            Self::Degrade => 2,
            Self::Shed => 3,
            Self::Unknown => 1,
        }
    }

    #[must_use]
    const fn from_severity(severity: u8) -> Self {
        match severity {
            0 => Self::Admit,
            1 => Self::Defer,
            2 => Self::Degrade,
            _ => Self::Shed,
        }
    }
}

/// Predicted latency stage for a trace step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictedLatencyStage {
    /// No latency pressure.
    Healthy,
    /// Latency is close to budget but does not require action yet.
    Watch,
    /// Latency suggests deferring new work.
    Defer,
    /// Latency suggests degraded execution.
    Degrade,
    /// Latency suggests shedding work.
    Shed,
    /// Latency telemetry was missing.
    Unknown,
}

impl PredictedLatencyStage {
    #[must_use]
    const fn severity(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Watch => 1,
            Self::Defer => 1,
            Self::Degrade => 2,
            Self::Shed => 3,
            Self::Unknown => 1,
        }
    }

    #[must_use]
    fn from_ratio(ratio: Option<f64>) -> (Self, DecisionProvenance) {
        let Some(ratio) = ratio else {
            return (Self::Unknown, DecisionProvenance::Unknown);
        };
        if ratio >= 2.0 {
            (Self::Shed, DecisionProvenance::Estimated)
        } else if ratio >= 1.35 {
            (Self::Degrade, DecisionProvenance::Estimated)
        } else if ratio >= 1.0 {
            (Self::Defer, DecisionProvenance::Estimated)
        } else if ratio >= 0.8 {
            (Self::Watch, DecisionProvenance::Estimated)
        } else {
            (Self::Healthy, DecisionProvenance::Estimated)
        }
    }
}

/// Per-step classification of a candidate change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateImpact {
    /// Candidate and baseline match.
    Neutral,
    /// Candidate reduces pressure or policy-audit burden.
    Beneficial,
    /// Candidate increases pressure or policy-audit burden.
    Harmful,
    /// Candidate improves one dimension while worsening another.
    Mixed,
}

/// Apply recommendation emitted after confidence, risk, and proof gates run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDigitalTwinApplyRecommendation {
    /// Candidate is safe to apply under the requested proof predicate.
    Apply,
    /// Candidate output is useful for operator review but must not auto-apply.
    ReviewOnly,
    /// Candidate must not apply until blockers are resolved.
    Blocked,
}

/// Shared pass/warn/block verdict used by confidence and risk gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDigitalTwinGateVerdict {
    /// Gate passed without material warnings.
    Pass,
    /// Gate produced warnings; human review is required.
    Warning,
    /// Gate failed closed.
    Blocked,
}

/// Proof-hardware predicate labels aligned with proof-ledger terminology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDigitalTwinHardwarePredicate {
    /// 64-core / 256 GiB predicate was met by live evidence.
    ProvenPredicateMet,
    /// Required predicate was absent and must be reported as skipped, not proven.
    SkippedNotProven,
    /// Evidence is reduced remote proof only.
    RemoteReduced,
    /// Predicate is unknown or not represented in the trace.
    Unknown,
}

/// Resource-class coverage used to score digital-twin confidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinCoverage {
    /// Queue utilization or pending-item telemetry was present.
    pub queue: bool,
    /// Latency-over-budget telemetry was present.
    pub latency: bool,
    /// Memory-tier budget/usage telemetry was present.
    pub memory_tier: bool,
    /// Hardware predicate evidence was present.
    pub hardware_predicate: bool,
    /// Source artifact hashes were present for replay accountability.
    pub source_artifacts: bool,
    /// Number of trace steps represented in this run.
    pub sample_size: u64,
}

/// Confidence gate for resource digital-twin output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinConfidence {
    /// Score from 0 to 100 after coverage and data-quality penalties.
    pub score: u8,
    /// Minimum score required before apply can be recommended.
    pub threshold: u8,
    /// Gate verdict.
    pub verdict: ResourceDigitalTwinGateVerdict,
    /// Resource-class coverage used by the score.
    pub coverage: ResourceDigitalTwinCoverage,
    /// Blocking reasons that prevent an apply recommendation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Non-blocking warnings that still require operator review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One candidate risk finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinRiskItem {
    /// Stable risk code.
    pub code: String,
    /// Gate severity for this risk.
    pub severity: ResourceDigitalTwinGateVerdict,
    /// Risk contribution from 0 to 100.
    pub score: u8,
    /// Candidate knobs associated with this risk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_knobs: Vec<String>,
    /// Human-actionable blocker text.
    pub blocker_text: String,
}

/// Candidate risk summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinRisk {
    /// Aggregate risk score from 0 to 100.
    pub score: u8,
    /// Aggregate risk verdict.
    pub verdict: ResourceDigitalTwinGateVerdict,
    /// Whether this risk summary prevents apply.
    pub fail_closed: bool,
    /// Individual risk items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ResourceDigitalTwinRiskItem>,
}

/// Proof classification for the high-scale claim represented by a simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinProofClassification {
    /// Existing scale proof status terminology.
    pub proof_status: ProofStatus,
    /// Strongest evidence source found in the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_source: Option<ScaleProofEvidenceSource>,
    /// Hardware predicate classification aligned with proof-ledger labels.
    pub hardware_predicate: ResourceDigitalTwinHardwarePredicate,
    /// Minimum CPU predicate requested by the run.
    pub min_cpu_count: u32,
    /// Minimum RAM predicate requested by the run.
    pub min_memory_bytes: u64,
    /// Largest observed CPU count in the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_cpu_count: Option<u32>,
    /// Largest observed RAM bytes in the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_memory_bytes: Option<u64>,
    /// Whether this simulation may support a high-scale proven claim.
    pub high_scale_claim_allowed: bool,
    /// Proof blockers preventing a high-scale apply/proven claim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Concrete proof steps operators must run next.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_proof_steps: Vec<String>,
}

/// Final apply gate decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinApplyDecision {
    /// Apply/review/block recommendation.
    pub recommendation: ResourceDigitalTwinApplyRecommendation,
    /// Stable reason codes explaining the decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    /// Human-readable blocker text for blocked or review-only output.
    pub blocker_text: String,
}

/// Tunable safe-knob values used by the deterministic simulator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinKnobs {
    /// Maximum queue utilization before new work is degraded.
    pub admission_max_queue_utilization: f64,
    /// Maximum pending items before new work is degraded.
    pub admission_max_pending_items: u64,
    /// Relative scheduling weight for interactive pane work.
    pub qos_interactive_weight: f64,
    /// Relative scheduling weight for bulk search/index work.
    pub qos_bulk_search_weight: f64,
    /// Maximum topology migrations allowed in one simulated epoch.
    pub topology_max_migrations_per_epoch: u64,
    /// Fractional preference for spreading work across locality groups.
    pub topology_locality_spread_factor: f64,
    /// Hot resident memory budget in bytes.
    pub memory_hot_resident_budget_bytes: u64,
    /// Search/index cache budget in bytes.
    pub memory_search_cache_budget_bytes: u64,
    /// Whether replay-only auto-tune candidates are active.
    pub autotune_enabled: bool,
    /// Exploration budget percent for candidate auto-tuning.
    pub autotune_exploration_budget_percent: f64,
}

impl Default for ResourceDigitalTwinKnobs {
    fn default() -> Self {
        Self {
            admission_max_queue_utilization: 0.85,
            admission_max_pending_items: 64,
            qos_interactive_weight: 1.0,
            qos_bulk_search_weight: 1.0,
            topology_max_migrations_per_epoch: 2,
            topology_locality_spread_factor: 0.35,
            memory_hot_resident_budget_bytes: 134_217_728,
            memory_search_cache_budget_bytes: 67_108_864,
            autotune_enabled: true,
            autotune_exploration_budget_percent: 5.0,
        }
    }
}

impl ResourceDigitalTwinKnobs {
    #[must_use]
    fn hash(&self) -> String {
        stable_hash(self)
    }

    fn apply_package(
        &self,
        package: &ResourceControlOverridePackage,
    ) -> Result<(Self, Vec<String>), ResourceDigitalTwinError> {
        let mut candidate = self.clone();
        let mut applied = Vec::new();
        for override_ in package.all_overrides() {
            candidate.apply_override(override_)?;
            applied.push(format!(
                "{}:{}:{}",
                override_.domain.as_str(),
                override_.knob_id,
                override_.action.as_str()
            ));
        }
        applied.sort();
        Ok((candidate, applied))
    }

    fn apply_override(
        &mut self,
        override_: &ResourceControlOverride,
    ) -> Result<(), ResourceDigitalTwinError> {
        match override_.action {
            ResourceOverrideAction::ResetToDefault => return Ok(()),
            ResourceOverrideAction::Disable => {
                if override_.knob_id == "autotune.enabled" {
                    self.autotune_enabled = false;
                    return Ok(());
                }
                return Err(ResourceDigitalTwinError::InvalidOverride {
                    knob_id: override_.knob_id.clone(),
                    reason: "disable is only meaningful for autotune.enabled".to_string(),
                });
            }
            ResourceOverrideAction::Set => {}
        }

        let value = override_.value.as_deref().ok_or_else(|| {
            ResourceDigitalTwinError::InvalidOverride {
                knob_id: override_.knob_id.clone(),
                reason: "missing candidate value".to_string(),
            }
        })?;

        match (override_.domain, override_.knob_id.as_str()) {
            (ResourceOverrideDomain::Admission, "admission.max_queue_utilization") => {
                self.admission_max_queue_utilization = parse_f64(override_, value)?;
            }
            (ResourceOverrideDomain::Admission, "admission.max_pending_items") => {
                self.admission_max_pending_items = parse_u64(override_, value)?;
            }
            (ResourceOverrideDomain::Qos, "qos.interactive_weight") => {
                self.qos_interactive_weight = parse_f64(override_, value)?;
            }
            (ResourceOverrideDomain::Qos, "qos.bulk_search_weight") => {
                self.qos_bulk_search_weight = parse_f64(override_, value)?;
            }
            (ResourceOverrideDomain::Topology, "topology.max_migrations_per_epoch") => {
                self.topology_max_migrations_per_epoch = parse_u64(override_, value)?;
            }
            (ResourceOverrideDomain::Topology, "topology.locality_spread_factor") => {
                self.topology_locality_spread_factor = parse_f64(override_, value)?;
            }
            (ResourceOverrideDomain::MemoryTier, "memory.hot_resident_budget_bytes") => {
                self.memory_hot_resident_budget_bytes = parse_u64(override_, value)?;
            }
            (ResourceOverrideDomain::MemoryTier, "memory.search_cache_budget_bytes") => {
                self.memory_search_cache_budget_bytes = parse_u64(override_, value)?;
            }
            (ResourceOverrideDomain::AutoTune, "autotune.enabled") => {
                self.autotune_enabled = parse_bool(override_, value)?;
            }
            (ResourceOverrideDomain::AutoTune, "autotune.exploration_budget_percent") => {
                self.autotune_exploration_budget_percent = parse_f64(override_, value)?;
            }
            _ => {
                return Err(ResourceDigitalTwinError::InvalidOverride {
                    knob_id: override_.knob_id.clone(),
                    reason: "knob is not wired into the resource digital twin".to_string(),
                });
            }
        }
        Ok(())
    }

    #[must_use]
    fn effective_memory_budget_delta(&self, baseline: &Self) -> i128 {
        let candidate = i128::from(self.memory_hot_resident_budget_bytes)
            + i128::from(self.memory_search_cache_budget_bytes);
        let baseline = i128::from(baseline.memory_hot_resident_budget_bytes)
            + i128::from(baseline.memory_search_cache_budget_bytes);
        candidate - baseline
    }
}

/// Engine execution options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinRunOptions {
    /// Stable generation timestamp in milliseconds.
    pub generated_at_ms: u64,
    /// Minimum confidence score required before apply can be recommended.
    pub minimum_apply_confidence_score: u8,
    /// Minimum CPU predicate for high-scale proof claims.
    pub high_scale_min_cpu_count: u32,
    /// Minimum RAM predicate for high-scale proof claims.
    pub high_scale_min_memory_bytes: u64,
    /// Test/diagnostic probe that attempts a side effect through the barrier.
    #[serde(default)]
    pub probe_side_effect_attempt: bool,
}

impl Default for ResourceDigitalTwinRunOptions {
    fn default() -> Self {
        Self {
            generated_at_ms: 0,
            minimum_apply_confidence_score:
                RESOURCE_DIGITAL_TWIN_DEFAULT_MIN_APPLY_CONFIDENCE_SCORE,
            high_scale_min_cpu_count: RESOURCE_DIGITAL_TWIN_HIGH_SCALE_MIN_CPU_COUNT,
            high_scale_min_memory_bytes: RESOURCE_DIGITAL_TWIN_HIGH_SCALE_MIN_MEMORY_BYTES,
            probe_side_effect_attempt: false,
        }
    }
}

/// One simulated decision frame for a trace step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDecisionFrame {
    /// Stable trace step id.
    pub step_id: String,
    /// Trace source family.
    pub source: DigitalTwinTraceSource,
    /// Monotonic timestamp from the trace.
    pub monotonic_ms: u64,
    /// Simulated admission action.
    pub admission_action: SimulatedAdmissionAction,
    /// Provenance of the admission action.
    pub admission_provenance: DecisionProvenance,
    /// Observed or estimated queue depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_depth: Option<u64>,
    /// Queue depth minus the active pending-item threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_depth_delta_from_threshold: Option<i128>,
    /// Predicted latency stage.
    pub predicted_latency_stage: PredictedLatencyStage,
    /// Provenance of the latency prediction.
    pub latency_provenance: DecisionProvenance,
    /// Predicted resident memory bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_memory_bytes: Option<u64>,
    /// Predicted cold or reclaimable memory bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold_memory_bytes: Option<u64>,
    /// Resident memory delta from the effective budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_delta_from_budget_bytes: Option<i128>,
    /// Simulated topology migrations for this step.
    pub topology_migration_count: u64,
    /// Simulated policy audit event count.
    pub policy_audit_event_count: u64,
    /// Whether auto-tune is active for this frame.
    pub autotune_considered: bool,
    /// Stable reason labels used to explain the frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    /// Stable warning labels for this frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Stable hash of the frame payload.
    pub decision_hash: String,
}

/// Per-step change between baseline and candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDecisionChange {
    /// Trace step id.
    pub step_id: String,
    /// Baseline admission action.
    pub baseline_action: SimulatedAdmissionAction,
    /// Candidate admission action.
    pub candidate_action: SimulatedAdmissionAction,
    /// Baseline latency stage.
    pub baseline_latency_stage: PredictedLatencyStage,
    /// Candidate latency stage.
    pub candidate_latency_stage: PredictedLatencyStage,
    /// Candidate minus baseline queue-depth delta.
    pub queue_depth_delta: i128,
    /// Candidate minus baseline resident memory bytes.
    pub resident_memory_delta_bytes: i128,
    /// Candidate minus baseline cold memory bytes.
    pub cold_memory_delta_bytes: i128,
    /// Candidate minus baseline topology migrations.
    pub topology_migration_delta: i128,
    /// Candidate minus baseline policy audit events.
    pub policy_audit_delta: i128,
    /// Change classification.
    pub impact: CandidateImpact,
}

/// Aggregate baseline-vs-candidate diff summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinDiff {
    /// Total trace steps compared.
    pub total_steps: u64,
    /// Steps with identical decision hashes.
    pub unchanged_steps: u64,
    /// Steps with any simulated decision change.
    pub changed_steps: u64,
    /// Count of admission action changes.
    pub admission_action_changes: u64,
    /// Sum of candidate minus baseline queue-depth deltas.
    pub queue_depth_delta_sum: i128,
    /// Largest absolute queue-depth delta in any step.
    pub max_queue_depth_delta_abs: u128,
    /// Count of latency-stage movements.
    pub latency_stage_movements: u64,
    /// Sum of candidate minus baseline resident memory bytes.
    pub resident_memory_delta_bytes: i128,
    /// Sum of candidate minus baseline cold memory bytes.
    pub cold_memory_delta_bytes: i128,
    /// Sum of candidate minus baseline topology migration counts.
    pub topology_migration_delta: i128,
    /// Sum of candidate minus baseline policy-audit event counts.
    pub policy_audit_delta: i128,
    /// Candidate changes classified as beneficial.
    pub beneficial_changes: u64,
    /// Candidate changes classified as harmful.
    pub harmful_changes: u64,
    /// Candidate changes with mixed signals.
    pub mixed_changes: u64,
    /// Per-step changes, sorted by trace order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<ResourceDecisionChange>,
}

/// Complete digital-twin simulation output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDigitalTwinSimulation {
    /// Schema version.
    pub schema_version: String,
    /// Stable generation timestamp in milliseconds.
    pub generated_at_ms: u64,
    /// Source trace hash.
    pub trace_hash: String,
    /// Candidate package name.
    pub package_name: String,
    /// Number of candidate overrides applied.
    pub package_override_count: usize,
    /// Baseline knob hash.
    pub baseline_knob_hash: String,
    /// Candidate knob hash.
    pub candidate_knob_hash: String,
    /// Safe-knob override labels applied to the candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_overrides: Vec<String>,
    /// Baseline decision stream.
    pub baseline_decisions: Vec<ResourceDecisionFrame>,
    /// Candidate decision stream.
    pub candidate_decisions: Vec<ResourceDecisionFrame>,
    /// Aggregate diff summary.
    pub diff: ResourceDigitalTwinDiff,
    /// Confidence and data-quality gate.
    pub confidence: ResourceDigitalTwinConfidence,
    /// Candidate risk gate.
    pub risk: ResourceDigitalTwinRisk,
    /// Proof classification for high-scale claims.
    pub proof_classification: ResourceDigitalTwinProofClassification,
    /// Final apply/review/block decision.
    pub apply_decision: ResourceDigitalTwinApplyDecision,
    /// Trace and simulation data-quality warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_quality_warnings: Vec<String>,
    /// Barrier mode used during the run.
    pub side_effect_barrier_mode: String,
    /// Number of side-effect attempts captured by the barrier.
    pub side_effects_captured: usize,
    /// Stable hash of this simulation output without the hash itself.
    pub simulation_hash: String,
}

impl ResourceDigitalTwinSimulation {
    /// Stable JSON representation for fixtures and Robot contracts.
    #[must_use]
    pub fn to_stable_json(&self) -> String {
        serde_json::to_string_pretty(self)
            .expect("resource digital twin simulation serializes to stable JSON")
    }
}

/// Resource digital-twin execution failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceDigitalTwinError {
    /// A candidate override could not be parsed or applied.
    InvalidOverride {
        /// Knob id.
        knob_id: String,
        /// Human-readable reason.
        reason: String,
    },
    /// A barrier reported that a side effect executed during replay.
    SideEffectEscaped {
        /// Barrier mode.
        mode: String,
        /// Outcome summary.
        summary: String,
    },
}

impl std::fmt::Display for ResourceDigitalTwinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOverride { knob_id, reason } => {
                write!(f, "invalid resource override {knob_id}: {reason}")
            }
            Self::SideEffectEscaped { mode, summary } => {
                write!(f, "side effect escaped {mode} barrier: {summary}")
            }
        }
    }
}

impl std::error::Error for ResourceDigitalTwinError {}

/// Pure baseline-vs-candidate resource digital twin.
#[derive(Debug, Clone, Default)]
pub struct ResourceDigitalTwinEngine {
    baseline_knobs: ResourceDigitalTwinKnobs,
}

impl ResourceDigitalTwinEngine {
    /// Create an engine with the supplied baseline safe-knob values.
    #[must_use]
    pub fn new(baseline_knobs: ResourceDigitalTwinKnobs) -> Self {
        Self { baseline_knobs }
    }

    /// Simulate with default options and the supplied barrier.
    pub fn simulate_with_barrier(
        &self,
        trace: &DigitalTwinTrace,
        package: &ResourceControlOverridePackage,
        barrier: &dyn SideEffectBarrier,
    ) -> Result<ResourceDigitalTwinSimulation, ResourceDigitalTwinError> {
        self.simulate_with_options(
            trace,
            package,
            barrier,
            &ResourceDigitalTwinRunOptions::default(),
        )
    }

    /// Simulate with explicit run options and the supplied barrier.
    pub fn simulate_with_options(
        &self,
        trace: &DigitalTwinTrace,
        package: &ResourceControlOverridePackage,
        barrier: &dyn SideEffectBarrier,
        options: &ResourceDigitalTwinRunOptions,
    ) -> Result<ResourceDigitalTwinSimulation, ResourceDigitalTwinError> {
        let _simulation_guard = SimulationGuard::enter();
        if options.probe_side_effect_attempt {
            let outcome = barrier.process(&EffectRequest {
                timestamp_ms: options.generated_at_ms,
                effect_type: EffectType::FileWrite,
                pane_id: None,
                payload: "resource-digital-twin-probe".to_string(),
                caller: "replay_resource_digital_twin::probe".to_string(),
                action_kind: ActionKind::WriteFile,
                metadata: HashMap::from([(
                    "scope".to_string(),
                    "side_effect_barrier_acceptance".to_string(),
                )]),
            });
            if outcome.executed {
                return Err(ResourceDigitalTwinError::SideEffectEscaped {
                    mode: barrier.mode_name().to_string(),
                    summary: outcome.summary,
                });
            }
        }

        let (candidate_knobs, applied_overrides) = self.baseline_knobs.apply_package(package)?;
        let baseline_decisions = trace
            .steps
            .iter()
            .map(|step| simulate_step(step, &self.baseline_knobs, &self.baseline_knobs, true))
            .collect::<Vec<_>>();
        let candidate_decisions = trace
            .steps
            .iter()
            .map(|step| simulate_step(step, &candidate_knobs, &self.baseline_knobs, false))
            .collect::<Vec<_>>();
        let diff =
            ResourceDigitalTwinDiff::from_decisions(&baseline_decisions, &candidate_decisions);
        let data_quality_warnings =
            collect_data_quality_warnings(trace, &baseline_decisions, &candidate_decisions);
        let confidence = assess_confidence(trace, &data_quality_warnings, options);
        let risk = assess_candidate_risk(
            &diff,
            &candidate_decisions,
            &data_quality_warnings,
            &applied_overrides,
        );
        let proof_classification = classify_proof(trace, options);
        let apply_decision = decide_apply(&confidence, &risk, &proof_classification);

        let mut simulation = ResourceDigitalTwinSimulation {
            schema_version: RESOURCE_DIGITAL_TWIN_SCHEMA_VERSION.to_string(),
            generated_at_ms: options.generated_at_ms,
            trace_hash: trace.trace_hash.clone(),
            package_name: package.meta.name.clone(),
            package_override_count: package.override_count(),
            baseline_knob_hash: self.baseline_knobs.hash(),
            candidate_knob_hash: candidate_knobs.hash(),
            applied_overrides,
            baseline_decisions,
            candidate_decisions,
            diff,
            confidence,
            risk,
            proof_classification,
            apply_decision,
            data_quality_warnings,
            side_effect_barrier_mode: barrier.mode_name().to_string(),
            side_effects_captured: barrier_log_len(barrier.log()),
            simulation_hash: String::new(),
        };
        let simulation_hash = {
            let payload = simulation_hash_payload(&simulation);
            stable_hash(&payload)
        };
        simulation.simulation_hash = simulation_hash;
        Ok(simulation)
    }
}

impl ResourceDigitalTwinDiff {
    #[must_use]
    fn from_decisions(
        baseline: &[ResourceDecisionFrame],
        candidate: &[ResourceDecisionFrame],
    ) -> Self {
        let mut diff = Self {
            total_steps: baseline.len().max(candidate.len()) as u64,
            ..Self::default()
        };

        for (baseline_frame, candidate_frame) in baseline.iter().zip(candidate.iter()) {
            if baseline_frame.decision_hash == candidate_frame.decision_hash {
                diff.unchanged_steps += 1;
                continue;
            }

            diff.changed_steps += 1;
            let queue_delta = option_i128(candidate_frame.queue_depth_delta_from_threshold)
                - option_i128(baseline_frame.queue_depth_delta_from_threshold);
            let resident_delta = option_i128(candidate_frame.resident_memory_bytes)
                - option_i128(baseline_frame.resident_memory_bytes);
            let cold_delta = option_i128(candidate_frame.cold_memory_bytes)
                - option_i128(baseline_frame.cold_memory_bytes);
            let topology_delta = i128::from(candidate_frame.topology_migration_count)
                - i128::from(baseline_frame.topology_migration_count);
            let audit_delta = i128::from(candidate_frame.policy_audit_event_count)
                - i128::from(baseline_frame.policy_audit_event_count);

            if baseline_frame.admission_action != candidate_frame.admission_action {
                diff.admission_action_changes += 1;
            }
            if baseline_frame.predicted_latency_stage != candidate_frame.predicted_latency_stage {
                diff.latency_stage_movements += 1;
            }
            diff.queue_depth_delta_sum += queue_delta;
            diff.max_queue_depth_delta_abs = diff
                .max_queue_depth_delta_abs
                .max(queue_delta.unsigned_abs());
            diff.resident_memory_delta_bytes += resident_delta;
            diff.cold_memory_delta_bytes += cold_delta;
            diff.topology_migration_delta += topology_delta;
            diff.policy_audit_delta += audit_delta;

            let impact = classify_impact(
                baseline_frame,
                candidate_frame,
                resident_delta,
                cold_delta,
                topology_delta,
                audit_delta,
            );
            match impact {
                CandidateImpact::Beneficial => diff.beneficial_changes += 1,
                CandidateImpact::Harmful => diff.harmful_changes += 1,
                CandidateImpact::Mixed => diff.mixed_changes += 1,
                CandidateImpact::Neutral => {}
            }

            diff.changes.push(ResourceDecisionChange {
                step_id: baseline_frame.step_id.clone(),
                baseline_action: baseline_frame.admission_action,
                candidate_action: candidate_frame.admission_action,
                baseline_latency_stage: baseline_frame.predicted_latency_stage,
                candidate_latency_stage: candidate_frame.predicted_latency_stage,
                queue_depth_delta: queue_delta,
                resident_memory_delta_bytes: resident_delta,
                cold_memory_delta_bytes: cold_delta,
                topology_migration_delta: topology_delta,
                policy_audit_delta: audit_delta,
                impact,
            });
        }

        diff
    }
}

fn simulate_step(
    step: &DigitalTwinTraceStep,
    knobs: &ResourceDigitalTwinKnobs,
    baseline_knobs: &ResourceDigitalTwinKnobs,
    baseline: bool,
) -> ResourceDecisionFrame {
    let mut warnings = warnings_from_trace_flags(&step.step_id, &step.quality_flags);
    let mut reason_codes = step.reason_codes.clone();
    reason_codes.sort();
    reason_codes.dedup();

    let queue_depth = step.pending_items;
    let queue_depth_delta_from_threshold =
        queue_depth.map(|depth| i128::from(depth) - i128::from(knobs.admission_max_pending_items));
    let queue_pressure = queue_pressure_severity(step, knobs, &mut reason_codes, &mut warnings);
    let (latency_stage, latency_provenance) =
        PredictedLatencyStage::from_ratio(effective_latency_ratio(step, knobs));
    let memory = predicted_memory(step, knobs, baseline_knobs, baseline, &mut reason_codes);
    let memory_pressure = memory_pressure_severity(
        &memory,
        &mut reason_codes,
        step.memory_budget_bytes.is_some(),
    );

    let source_pressure = step
        .effective_pressure_severity
        .unwrap_or(step.pressure_score)
        .min(3);
    let severity =
        if step.source == DigitalTwinTraceSource::ScaleProof && step.proof_status.is_some() {
            source_pressure
        } else {
            queue_pressure
                .max(latency_stage.severity())
                .max(memory_pressure)
                .max(source_pressure)
        };

    let (admission_action, admission_provenance) =
        if step.source == DigitalTwinTraceSource::ScaleProof && step.proof_status.is_some() {
            proof_status_admission(step)
        } else if step.queue_utilization.is_none()
            && step.pending_items.is_none()
            && step.max_latency_over_budget_ratio.is_none()
            && step.memory_actual_bytes.is_none()
        {
            observed_or_unknown_admission(step, &mut warnings)
        } else {
            (
                SimulatedAdmissionAction::from_severity(severity),
                DecisionProvenance::Estimated,
            )
        };

    if knobs.autotune_enabled && knobs.autotune_exploration_budget_percent > 0.0 {
        reason_codes.push(format!(
            "autotune_budget_percent:{}",
            stable_float(knobs.autotune_exploration_budget_percent)
        ));
    }

    let topology_migration_count = topology_migrations(step, knobs, admission_action);
    let policy_audit_event_count = policy_audit_events(
        admission_action,
        &warnings,
        topology_migration_count,
        step.quality_flags.as_slice(),
    );
    reason_codes.sort();
    reason_codes.dedup();
    warnings.sort();
    warnings.dedup();

    let mut frame = ResourceDecisionFrame {
        step_id: step.step_id.clone(),
        source: step.source,
        monotonic_ms: step.monotonic_ms,
        admission_action,
        admission_provenance,
        queue_depth,
        queue_depth_delta_from_threshold,
        predicted_latency_stage: latency_stage,
        latency_provenance,
        resident_memory_bytes: memory.resident_memory_bytes,
        cold_memory_bytes: memory.cold_memory_bytes,
        resident_delta_from_budget_bytes: memory.resident_delta_from_budget_bytes,
        topology_migration_count,
        policy_audit_event_count,
        autotune_considered: knobs.autotune_enabled,
        reason_codes,
        warnings,
        decision_hash: String::new(),
    };
    let decision_hash = {
        let payload = frame_hash_payload(&frame);
        stable_hash(&payload)
    };
    frame.decision_hash = decision_hash;
    frame
}

#[derive(Debug, Clone, Copy)]
struct MemoryPrediction {
    resident_memory_bytes: Option<u64>,
    cold_memory_bytes: Option<u64>,
    resident_delta_from_budget_bytes: Option<i128>,
}

fn predicted_memory(
    step: &DigitalTwinTraceStep,
    knobs: &ResourceDigitalTwinKnobs,
    baseline_knobs: &ResourceDigitalTwinKnobs,
    baseline: bool,
    reason_codes: &mut Vec<String>,
) -> MemoryPrediction {
    let Some(actual) = step.memory_actual_bytes else {
        return MemoryPrediction {
            resident_memory_bytes: None,
            cold_memory_bytes: None,
            resident_delta_from_budget_bytes: None,
        };
    };
    let base_budget = step.memory_budget_bytes.unwrap_or(actual);
    let effective_budget = if baseline {
        base_budget
    } else {
        apply_budget_delta(
            base_budget,
            knobs.effective_memory_budget_delta(baseline_knobs),
        )
    };
    let over_budget = actual.saturating_sub(effective_budget);
    let reclaimable = step.reclaimable_bytes.unwrap_or(0);
    let cold_memory = reclaimable.saturating_add(over_budget).min(actual);
    let resident_memory = actual.saturating_sub(cold_memory);
    if effective_budget != base_budget {
        reason_codes.push(format!("memory_budget_adjusted_to:{effective_budget}"));
    }

    MemoryPrediction {
        resident_memory_bytes: Some(resident_memory),
        cold_memory_bytes: Some(cold_memory),
        resident_delta_from_budget_bytes: Some(i128::from(actual) - i128::from(effective_budget)),
    }
}

fn queue_pressure_severity(
    step: &DigitalTwinTraceStep,
    knobs: &ResourceDigitalTwinKnobs,
    reason_codes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> u8 {
    let mut severity = 0u8;
    if let Some(utilization) = step.queue_utilization {
        if utilization >= knobs.admission_max_queue_utilization * 1.20 {
            severity = severity.max(3);
            reason_codes.push("queue_utilization_shed".to_string());
        } else if utilization >= knobs.admission_max_queue_utilization {
            severity = severity.max(2);
            reason_codes.push("queue_utilization_degrade".to_string());
        } else if utilization >= knobs.admission_max_queue_utilization * 0.90 {
            severity = severity.max(1);
            reason_codes.push("queue_utilization_defer".to_string());
        }
    } else if step.source == DigitalTwinTraceSource::ResourceAdmission {
        warnings.push(format!("{}:queue_utilization_unknown", step.step_id));
    }

    if let Some(pending) = step.pending_items {
        if pending >= knobs.admission_max_pending_items.saturating_mul(2) {
            severity = severity.max(3);
            reason_codes.push("pending_items_shed".to_string());
        } else if pending >= knobs.admission_max_pending_items {
            severity = severity.max(2);
            reason_codes.push("pending_items_degrade".to_string());
        } else if pending >= knobs.admission_max_pending_items.saturating_mul(9) / 10 {
            severity = severity.max(1);
            reason_codes.push("pending_items_defer".to_string());
        }
    } else if step.source == DigitalTwinTraceSource::ResourceAdmission {
        warnings.push(format!("{}:pending_items_unknown", step.step_id));
    }

    severity
}

fn effective_latency_ratio(
    step: &DigitalTwinTraceStep,
    knobs: &ResourceDigitalTwinKnobs,
) -> Option<f64> {
    let ratio = step.max_latency_over_budget_ratio?;
    let qos_ratio = (knobs.qos_bulk_search_weight / knobs.qos_interactive_weight).clamp(0.5, 2.0);
    Some(ratio * qos_ratio)
}

fn memory_pressure_severity(
    memory: &MemoryPrediction,
    reason_codes: &mut Vec<String>,
    had_memory_budget: bool,
) -> u8 {
    let Some(delta) = memory.resident_delta_from_budget_bytes else {
        return 0;
    };
    if delta <= 0 {
        return 0;
    }
    if !had_memory_budget {
        reason_codes.push("memory_budget_estimated".to_string());
    }
    let over = u128::try_from(delta).unwrap_or(u128::MAX);
    let resident = memory
        .resident_memory_bytes
        .map(u128::from)
        .unwrap_or_default()
        .max(1);
    if over >= resident / 2 {
        reason_codes.push("memory_over_budget_shed".to_string());
        3
    } else if over > 0 {
        reason_codes.push("memory_over_budget_degrade".to_string());
        2
    } else {
        0
    }
}

fn observed_or_unknown_admission(
    step: &DigitalTwinTraceStep,
    warnings: &mut Vec<String>,
) -> (SimulatedAdmissionAction, DecisionProvenance) {
    if let Some(action) = step.admission_action.as_deref() {
        warnings.push(format!("{}:admission_observed_fallback", step.step_id));
        return (
            match action {
                "admit" => SimulatedAdmissionAction::Admit,
                "defer" => SimulatedAdmissionAction::Defer,
                "degrade" => SimulatedAdmissionAction::Degrade,
                "shed" => SimulatedAdmissionAction::Shed,
                _ => SimulatedAdmissionAction::Unknown,
            },
            DecisionProvenance::Observed,
        );
    }
    warnings.push(format!("{}:admission_unknown", step.step_id));
    (
        SimulatedAdmissionAction::Unknown,
        DecisionProvenance::Unknown,
    )
}

fn proof_status_admission(
    step: &DigitalTwinTraceStep,
) -> (SimulatedAdmissionAction, DecisionProvenance) {
    (
        match step.proof_status {
            Some(ProofStatus::Passed) => SimulatedAdmissionAction::Admit,
            Some(ProofStatus::SkippedNotProven) => SimulatedAdmissionAction::Defer,
            Some(ProofStatus::Failed) => SimulatedAdmissionAction::Shed,
            None => SimulatedAdmissionAction::Unknown,
        },
        DecisionProvenance::Observed,
    )
}

fn topology_migrations(
    step: &DigitalTwinTraceStep,
    knobs: &ResourceDigitalTwinKnobs,
    admission_action: SimulatedAdmissionAction,
) -> u64 {
    let pressure = u64::from(
        step.effective_pressure_severity
            .unwrap_or_else(|| admission_action.severity())
            .max(admission_action.severity()),
    );
    if pressure < 2 {
        return 0;
    }
    let spread_bonus = u64::from(knobs.topology_locality_spread_factor >= 0.50);
    knobs
        .topology_max_migrations_per_epoch
        .min(pressure.saturating_add(spread_bonus))
}

fn policy_audit_events(
    admission_action: SimulatedAdmissionAction,
    warnings: &[String],
    topology_migration_count: u64,
    quality_flags: &[DigitalTwinTraceQualityFlag],
) -> u64 {
    let mut events = match admission_action {
        SimulatedAdmissionAction::Admit => 0,
        SimulatedAdmissionAction::Defer => 1,
        SimulatedAdmissionAction::Degrade => 2,
        SimulatedAdmissionAction::Shed => 3,
        SimulatedAdmissionAction::Unknown => 1,
    };
    if topology_migration_count > 0 {
        events += 1;
    }
    if !warnings.is_empty() || !quality_flags.is_empty() {
        events += 1;
    }
    events
}

fn classify_impact(
    baseline: &ResourceDecisionFrame,
    candidate: &ResourceDecisionFrame,
    resident_delta: i128,
    cold_delta: i128,
    topology_delta: i128,
    audit_delta: i128,
) -> CandidateImpact {
    let pressure_delta = i16::from(candidate.admission_action.severity())
        - i16::from(baseline.admission_action.severity())
        + i16::from(candidate.predicted_latency_stage.severity())
        - i16::from(baseline.predicted_latency_stage.severity());
    let memory_delta = cold_delta - resident_delta;
    let operational_delta = topology_delta + audit_delta;

    let improves = pressure_delta < 0 || memory_delta < 0 || operational_delta < 0;
    let worsens = pressure_delta > 0 || memory_delta > 0 || operational_delta > 0;
    match (improves, worsens) {
        (true, true) => CandidateImpact::Mixed,
        (true, false) => CandidateImpact::Beneficial,
        (false, true) => CandidateImpact::Harmful,
        (false, false) => CandidateImpact::Neutral,
    }
}

fn collect_data_quality_warnings(
    trace: &DigitalTwinTrace,
    baseline: &[ResourceDecisionFrame],
    candidate: &[ResourceDecisionFrame],
) -> Vec<String> {
    let mut warnings = BTreeSet::new();
    for flag in &trace.quality_flags {
        if *flag == DigitalTwinTraceQualityFlag::RedactedIdentity {
            continue;
        }
        warnings.insert(format!("trace:{}", stable_label(flag)));
    }
    for frame in baseline.iter().chain(candidate.iter()) {
        for warning in &frame.warnings {
            warnings.insert(warning.clone());
        }
    }
    warnings.into_iter().collect()
}

fn assess_confidence(
    trace: &DigitalTwinTrace,
    data_quality_warnings: &[String],
    options: &ResourceDigitalTwinRunOptions,
) -> ResourceDigitalTwinConfidence {
    let coverage = ResourceDigitalTwinCoverage {
        queue: trace
            .steps
            .iter()
            .any(|step| step.queue_utilization.is_some() || step.pending_items.is_some()),
        latency: trace
            .steps
            .iter()
            .any(|step| step.max_latency_over_budget_ratio.is_some()),
        memory_tier: trace.steps.iter().any(|step| {
            step.memory_budget_bytes.is_some()
                || step.memory_actual_bytes.is_some()
                || step.memory_tier_pressure.is_some()
        }),
        hardware_predicate: trace.steps.iter().any(|step| {
            step.proof_status == Some(ProofStatus::Passed)
                && step.evidence_source == Some(ScaleProofEvidenceSource::LiveHardware)
                && step.hardware_evidence_complete == Some(true)
                && step
                    .hardware_cpu_count
                    .is_some_and(|count| count >= options.high_scale_min_cpu_count)
                && step
                    .hardware_memory_bytes
                    .is_some_and(|bytes| bytes >= options.high_scale_min_memory_bytes)
        }),
        source_artifacts: !trace.source_artifact_hashes.is_empty()
            && trace
                .steps
                .iter()
                .all(|step| !step.source_artifact_hashes.is_empty()),
        sample_size: u64::try_from(trace.steps.len()).unwrap_or(u64::MAX),
    };

    let mut score = 100i16;
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    penalize_missing_coverage(
        coverage.queue,
        "missing_queue_coverage",
        12,
        &mut score,
        &mut blockers,
    );
    penalize_missing_coverage(
        coverage.latency,
        "missing_latency_coverage",
        10,
        &mut score,
        &mut blockers,
    );
    penalize_missing_coverage(
        coverage.memory_tier,
        "missing_memory_tier_coverage",
        10,
        &mut score,
        &mut blockers,
    );
    penalize_missing_coverage(
        coverage.hardware_predicate,
        "missing_hardware_predicate",
        20,
        &mut score,
        &mut blockers,
    );
    if !coverage.source_artifacts {
        score -= 5;
        warnings.push("missing_source_artifact_hashes".to_string());
    }
    if coverage.sample_size < 2 {
        score -= 10;
        blockers.push("sample_size_below_minimum".to_string());
    } else if coverage.sample_size < 4 {
        score -= 5;
        warnings.push("sample_size_low".to_string());
    }

    for flag in &trace.quality_flags {
        let (penalty, label, blocking) = quality_penalty(*flag);
        score -= i16::from(penalty);
        if label.is_empty() {
            continue;
        }
        if blocking {
            blockers.push(label.to_string());
        } else {
            warnings.push(label.to_string());
        }
    }
    if !data_quality_warnings.is_empty() {
        warnings.push("simulation_data_quality_warnings_present".to_string());
    }

    blockers.sort();
    blockers.dedup();
    warnings.sort();
    warnings.dedup();
    let score = u8::try_from(score.clamp(0, 100)).unwrap_or(100);
    let verdict = if score < options.minimum_apply_confidence_score || !blockers.is_empty() {
        ResourceDigitalTwinGateVerdict::Blocked
    } else if warnings.is_empty() && score >= 90 {
        ResourceDigitalTwinGateVerdict::Pass
    } else {
        ResourceDigitalTwinGateVerdict::Warning
    };

    ResourceDigitalTwinConfidence {
        score,
        threshold: options.minimum_apply_confidence_score,
        verdict,
        coverage,
        blockers,
        warnings,
    }
}

fn assess_candidate_risk(
    diff: &ResourceDigitalTwinDiff,
    candidate_decisions: &[ResourceDecisionFrame],
    data_quality_warnings: &[String],
    applied_overrides: &[String],
) -> ResourceDigitalTwinRisk {
    let mut items = Vec::new();

    if diff.harmful_changes > 0 {
        items.push(risk_item(
            "harmful_candidate_rejection",
            ResourceDigitalTwinGateVerdict::Blocked,
            60,
            knobs_for_domains(applied_overrides, &[]),
            format!(
                "{} trace step(s) became more severe under the candidate",
                diff.harmful_changes
            ),
        ));
    }
    if diff.queue_depth_delta_sum > 0 {
        items.push(risk_item(
            "predicted_queue_growth",
            ResourceDigitalTwinGateVerdict::Warning,
            20,
            knobs_for_domains(applied_overrides, &["admission", "qos"]),
            format!(
                "candidate increases aggregate queue-depth pressure by {}",
                diff.queue_depth_delta_sum
            ),
        ));
    }

    let latency_regressions = diff
        .changes
        .iter()
        .filter(|change| {
            change.candidate_latency_stage.severity() > change.baseline_latency_stage.severity()
        })
        .count();
    if latency_regressions > 0 {
        items.push(risk_item(
            "latency_stage_regression",
            ResourceDigitalTwinGateVerdict::Blocked,
            35,
            knobs_for_domains(applied_overrides, &["qos", "admission"]),
            format!("{latency_regressions} trace step(s) regress latency stage"),
        ));
    }

    let memory_oversubscription_steps = candidate_decisions
        .iter()
        .filter(|frame| frame.resident_delta_from_budget_bytes.unwrap_or_default() > 0)
        .count();
    if memory_oversubscription_steps > 0 {
        items.push(risk_item(
            "memory_oversubscription",
            ResourceDigitalTwinGateVerdict::Blocked,
            35,
            knobs_for_domains(applied_overrides, &["memory_tier"]),
            format!("{memory_oversubscription_steps} trace step(s) exceed resident memory budget"),
        ));
    }

    if diff.topology_migration_delta > 0 {
        items.push(risk_item(
            "topology_churn",
            ResourceDigitalTwinGateVerdict::Warning,
            15,
            knobs_for_domains(applied_overrides, &["topology"]),
            format!(
                "candidate increases topology migrations by {}",
                diff.topology_migration_delta
            ),
        ));
    }
    if diff.policy_audit_delta < 0 && !data_quality_warnings.is_empty() {
        items.push(risk_item(
            "policy_audit_fail_open_risk",
            ResourceDigitalTwinGateVerdict::Blocked,
            70,
            knobs_for_domains(applied_overrides, &["admission", "topology", "auto_tune"]),
            format!(
                "candidate reduces policy-audit events by {} while data-quality warnings are present",
                diff.policy_audit_delta.unsigned_abs()
            ),
        ));
    } else if diff.policy_audit_delta > 0 {
        items.push(risk_item(
            "policy_audit_fail_closed",
            ResourceDigitalTwinGateVerdict::Warning,
            15,
            knobs_for_domains(applied_overrides, &["admission", "topology", "auto_tune"]),
            format!(
                "candidate increases policy-audit events by {}",
                diff.policy_audit_delta
            ),
        ));
    }
    if !data_quality_warnings.is_empty() {
        items.push(risk_item(
            "data_quality_apply_blocker",
            ResourceDigitalTwinGateVerdict::Warning,
            10,
            Vec::new(),
            "data-quality warnings require operator review before apply".to_string(),
        ));
    }

    items.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.code.cmp(&right.code))
    });
    let score = items
        .iter()
        .map(|item| u16::from(item.score))
        .sum::<u16>()
        .min(100);
    let score = u8::try_from(score).unwrap_or(100);
    let verdict = if items
        .iter()
        .any(|item| item.severity == ResourceDigitalTwinGateVerdict::Blocked)
        || score >= 60
    {
        ResourceDigitalTwinGateVerdict::Blocked
    } else if score > 0 {
        ResourceDigitalTwinGateVerdict::Warning
    } else {
        ResourceDigitalTwinGateVerdict::Pass
    };

    ResourceDigitalTwinRisk {
        score,
        verdict,
        fail_closed: verdict == ResourceDigitalTwinGateVerdict::Blocked,
        items,
    }
}

fn classify_proof(
    trace: &DigitalTwinTrace,
    options: &ResourceDigitalTwinRunOptions,
) -> ResourceDigitalTwinProofClassification {
    let observed_cpu_count = trace
        .steps
        .iter()
        .filter_map(|step| step.hardware_cpu_count)
        .max();
    let observed_memory_bytes = trace
        .steps
        .iter()
        .filter_map(|step| step.hardware_memory_bytes)
        .max();
    let evidence_source = trace
        .steps
        .iter()
        .filter_map(|step| step.evidence_source)
        .max_by_key(|source| evidence_source_rank(*source));
    let live_predicate_met = trace.steps.iter().any(|step| {
        step.proof_status == Some(ProofStatus::Passed)
            && step.evidence_source == Some(ScaleProofEvidenceSource::LiveHardware)
            && step.hardware_evidence_complete == Some(true)
            && step
                .hardware_cpu_count
                .is_some_and(|count| count >= options.high_scale_min_cpu_count)
            && step
                .hardware_memory_bytes
                .is_some_and(|bytes| bytes >= options.high_scale_min_memory_bytes)
    });
    let failed_proof = trace
        .steps
        .iter()
        .any(|step| step.proof_status == Some(ProofStatus::Failed));

    if live_predicate_met {
        return ResourceDigitalTwinProofClassification {
            proof_status: ProofStatus::Passed,
            evidence_source,
            hardware_predicate: ResourceDigitalTwinHardwarePredicate::ProvenPredicateMet,
            min_cpu_count: options.high_scale_min_cpu_count,
            min_memory_bytes: options.high_scale_min_memory_bytes,
            observed_cpu_count,
            observed_memory_bytes,
            high_scale_claim_allowed: true,
            blockers: Vec::new(),
            next_proof_steps: Vec::new(),
        };
    }

    let mut blockers = BTreeSet::new();
    if failed_proof {
        blockers.insert("scale_proof_failed".to_string());
    }
    if trace
        .quality_flags
        .contains(&DigitalTwinTraceQualityFlag::SimulatedEvidence)
    {
        blockers.insert("simulated_evidence_skipped_not_proven".to_string());
    }
    if trace
        .quality_flags
        .contains(&DigitalTwinTraceQualityFlag::IncompleteHardwareEvidence)
        || observed_cpu_count.is_none()
        || observed_memory_bytes.is_none()
    {
        blockers.insert("incomplete_hardware_evidence".to_string());
    }
    if observed_cpu_count.is_some_and(|count| count < options.high_scale_min_cpu_count)
        || observed_memory_bytes.is_some_and(|bytes| bytes < options.high_scale_min_memory_bytes)
    {
        blockers.insert("hardware_predicate_not_met".to_string());
    }
    if blockers.is_empty() {
        blockers.insert("live_high_scale_proof_absent".to_string());
    }

    let hardware_predicate = match evidence_source {
        Some(ScaleProofEvidenceSource::RchRemote) => {
            ResourceDigitalTwinHardwarePredicate::RemoteReduced
        }
        Some(ScaleProofEvidenceSource::Synthetic | ScaleProofEvidenceSource::ReplayBacked) => {
            ResourceDigitalTwinHardwarePredicate::SkippedNotProven
        }
        Some(ScaleProofEvidenceSource::LiveHardware)
            if observed_cpu_count.is_some() || observed_memory_bytes.is_some() =>
        {
            ResourceDigitalTwinHardwarePredicate::SkippedNotProven
        }
        _ => ResourceDigitalTwinHardwarePredicate::Unknown,
    };
    let proof_status = if failed_proof {
        ProofStatus::Failed
    } else {
        ProofStatus::SkippedNotProven
    };

    ResourceDigitalTwinProofClassification {
        proof_status,
        evidence_source,
        hardware_predicate,
        min_cpu_count: options.high_scale_min_cpu_count,
        min_memory_bytes: options.high_scale_min_memory_bytes,
        observed_cpu_count,
        observed_memory_bytes,
        high_scale_claim_allowed: false,
        blockers: blockers.into_iter().collect(),
        next_proof_steps: next_proof_steps(options),
    }
}

fn decide_apply(
    confidence: &ResourceDigitalTwinConfidence,
    risk: &ResourceDigitalTwinRisk,
    proof: &ResourceDigitalTwinProofClassification,
) -> ResourceDigitalTwinApplyDecision {
    let mut reasons = BTreeSet::new();
    let mut blocker_text = Vec::new();

    if confidence.score < confidence.threshold {
        reasons.insert("confidence_below_apply_threshold".to_string());
        blocker_text.push(format!(
            "confidence score {} is below required threshold {}",
            confidence.score, confidence.threshold
        ));
    } else if confidence.verdict == ResourceDigitalTwinGateVerdict::Blocked {
        reasons.insert("confidence_gate_blocked".to_string());
        blocker_text.push("confidence gate has blocking data-quality findings".to_string());
    }
    if risk.fail_closed {
        reasons.insert("candidate_risk_fail_closed".to_string());
        blocker_text.push("candidate risk gate failed closed".to_string());
    } else if risk.verdict == ResourceDigitalTwinGateVerdict::Warning {
        reasons.insert("candidate_risk_requires_review".to_string());
    }
    if !proof.high_scale_claim_allowed {
        reasons.insert("proof_skipped_not_proven".to_string());
        blocker_text
            .push("high-scale proof is SKIPPED_NOT_PROVEN until live predicates pass".to_string());
    }
    if confidence.verdict == ResourceDigitalTwinGateVerdict::Warning {
        reasons.insert("confidence_requires_review".to_string());
    }

    let recommendation = if reasons.contains("confidence_below_apply_threshold")
        || reasons.contains("confidence_gate_blocked")
        || reasons.contains("candidate_risk_fail_closed")
        || reasons.contains("proof_skipped_not_proven")
    {
        ResourceDigitalTwinApplyRecommendation::Blocked
    } else if reasons.is_empty() {
        ResourceDigitalTwinApplyRecommendation::Apply
    } else {
        ResourceDigitalTwinApplyRecommendation::ReviewOnly
    };

    if blocker_text.is_empty() {
        blocker_text.push(match recommendation {
            ResourceDigitalTwinApplyRecommendation::Apply => {
                "candidate may apply under the recorded live proof predicate".to_string()
            }
            ResourceDigitalTwinApplyRecommendation::ReviewOnly => {
                "candidate requires operator review before apply".to_string()
            }
            ResourceDigitalTwinApplyRecommendation::Blocked => {
                "candidate is blocked until gate findings are resolved".to_string()
            }
        });
    }

    ResourceDigitalTwinApplyDecision {
        recommendation,
        reason_codes: reasons.into_iter().collect(),
        blocker_text: blocker_text.join("; "),
    }
}

fn penalize_missing_coverage(
    covered: bool,
    code: &str,
    penalty: i16,
    score: &mut i16,
    blockers: &mut Vec<String>,
) {
    if !covered {
        *score -= penalty;
        blockers.push(code.to_string());
    }
}

fn quality_penalty(flag: DigitalTwinTraceQualityFlag) -> (u8, &'static str, bool) {
    match flag {
        DigitalTwinTraceQualityFlag::StaleTimestamp => (10, "stale_timestamp", true),
        DigitalTwinTraceQualityFlag::MissingQueueTelemetry => (8, "missing_queue_telemetry", true),
        DigitalTwinTraceQualityFlag::MissingFleetTelemetry => (6, "missing_fleet_telemetry", false),
        DigitalTwinTraceQualityFlag::MissingMemoryTierTelemetry => {
            (8, "missing_memory_tier_telemetry", true)
        }
        DigitalTwinTraceQualityFlag::MissingLatencyTelemetry => {
            (8, "missing_latency_telemetry", true)
        }
        DigitalTwinTraceQualityFlag::IncompleteHardwareEvidence => {
            (15, "incomplete_hardware_evidence", true)
        }
        DigitalTwinTraceQualityFlag::SimulatedEvidence => {
            (12, "simulated_evidence_skipped_not_proven", true)
        }
        DigitalTwinTraceQualityFlag::NonMonotonicTimestampAdjusted => {
            (6, "non_monotonic_timestamp_adjusted", false)
        }
        DigitalTwinTraceQualityFlag::RedactedIdentity => (0, "", false),
        DigitalTwinTraceQualityFlag::CompactedSamples => (3, "compacted_samples", false),
        DigitalTwinTraceQualityFlag::DerivedSourceHash => (3, "derived_source_hash", false),
        DigitalTwinTraceQualityFlag::NonFiniteTelemetry => (10, "non_finite_telemetry", true),
    }
}

fn risk_item(
    code: &str,
    severity: ResourceDigitalTwinGateVerdict,
    score: u8,
    candidate_knobs: Vec<String>,
    blocker_text: String,
) -> ResourceDigitalTwinRiskItem {
    ResourceDigitalTwinRiskItem {
        code: code.to_string(),
        severity,
        score,
        candidate_knobs,
        blocker_text,
    }
}

fn knobs_for_domains(applied_overrides: &[String], domains: &[&str]) -> Vec<String> {
    let mut knobs = applied_overrides
        .iter()
        .filter(|label| {
            domains.is_empty() || domains.iter().any(|domain| label.starts_with(domain))
        })
        .cloned()
        .collect::<Vec<_>>();
    knobs.sort();
    knobs.dedup();
    knobs
}

const fn evidence_source_rank(source: ScaleProofEvidenceSource) -> u8 {
    match source {
        ScaleProofEvidenceSource::Unknown => 0,
        ScaleProofEvidenceSource::Synthetic => 1,
        ScaleProofEvidenceSource::ReplayBacked => 2,
        ScaleProofEvidenceSource::RchRemote => 3,
        ScaleProofEvidenceSource::LiveHardware => 4,
    }
}

fn next_proof_steps(options: &ResourceDigitalTwinRunOptions) -> Vec<String> {
    vec![
        format!(
            "capture a live ScaleProofMatrix row on >= {} logical CPUs and >= {} GiB RAM",
            options.high_scale_min_cpu_count,
            gib(options.high_scale_min_memory_bytes)
        ),
        "record class=live_hardware, evidence_source=live_hardware, status=PASSED, and complete CPU/RAM/storage/worker/command/git evidence".to_string(),
        "rerun the resource digital-twin report against that trace before allowing apply".to_string(),
    ]
}

const fn gib(bytes: u64) -> u64 {
    bytes / 1_073_741_824
}

fn warnings_from_trace_flags(step_id: &str, flags: &[DigitalTwinTraceQualityFlag]) -> Vec<String> {
    flags
        .iter()
        .filter(|flag| **flag != DigitalTwinTraceQualityFlag::RedactedIdentity)
        .map(|flag| format!("{step_id}:trace_{}", stable_label(flag)))
        .collect()
}

#[derive(Serialize)]
struct ResourceDecisionFrameHashPayload<'a> {
    step_id: &'a str,
    source: &'a DigitalTwinTraceSource,
    monotonic_ms: u64,
    admission_action: &'a SimulatedAdmissionAction,
    admission_provenance: &'a DecisionProvenance,
    queue_depth: Option<u64>,
    queue_depth_delta_from_threshold: Option<i128>,
    predicted_latency_stage: &'a PredictedLatencyStage,
    latency_provenance: &'a DecisionProvenance,
    resident_memory_bytes: Option<u64>,
    cold_memory_bytes: Option<u64>,
    resident_delta_from_budget_bytes: Option<i128>,
    topology_migration_count: u64,
    policy_audit_event_count: u64,
    autotune_considered: bool,
    reason_codes: &'a [String],
    warnings: &'a [String],
}

fn frame_hash_payload(frame: &ResourceDecisionFrame) -> ResourceDecisionFrameHashPayload<'_> {
    ResourceDecisionFrameHashPayload {
        step_id: &frame.step_id,
        source: &frame.source,
        monotonic_ms: frame.monotonic_ms,
        admission_action: &frame.admission_action,
        admission_provenance: &frame.admission_provenance,
        queue_depth: frame.queue_depth,
        queue_depth_delta_from_threshold: frame.queue_depth_delta_from_threshold,
        predicted_latency_stage: &frame.predicted_latency_stage,
        latency_provenance: &frame.latency_provenance,
        resident_memory_bytes: frame.resident_memory_bytes,
        cold_memory_bytes: frame.cold_memory_bytes,
        resident_delta_from_budget_bytes: frame.resident_delta_from_budget_bytes,
        topology_migration_count: frame.topology_migration_count,
        policy_audit_event_count: frame.policy_audit_event_count,
        autotune_considered: frame.autotune_considered,
        reason_codes: &frame.reason_codes,
        warnings: &frame.warnings,
    }
}

#[derive(Serialize)]
struct ResourceDigitalTwinSimulationHashPayload<'a> {
    schema_version: &'a str,
    generated_at_ms: u64,
    trace_hash: &'a str,
    package_name: &'a str,
    package_override_count: usize,
    baseline_knob_hash: &'a str,
    candidate_knob_hash: &'a str,
    applied_overrides: &'a [String],
    baseline_decisions: &'a [ResourceDecisionFrame],
    candidate_decisions: &'a [ResourceDecisionFrame],
    diff: &'a ResourceDigitalTwinDiff,
    confidence: &'a ResourceDigitalTwinConfidence,
    risk: &'a ResourceDigitalTwinRisk,
    proof_classification: &'a ResourceDigitalTwinProofClassification,
    apply_decision: &'a ResourceDigitalTwinApplyDecision,
    data_quality_warnings: &'a [String],
    side_effect_barrier_mode: &'a str,
    side_effects_captured: usize,
}

fn simulation_hash_payload(
    simulation: &ResourceDigitalTwinSimulation,
) -> ResourceDigitalTwinSimulationHashPayload<'_> {
    ResourceDigitalTwinSimulationHashPayload {
        schema_version: &simulation.schema_version,
        generated_at_ms: simulation.generated_at_ms,
        trace_hash: &simulation.trace_hash,
        package_name: &simulation.package_name,
        package_override_count: simulation.package_override_count,
        baseline_knob_hash: &simulation.baseline_knob_hash,
        candidate_knob_hash: &simulation.candidate_knob_hash,
        applied_overrides: &simulation.applied_overrides,
        baseline_decisions: &simulation.baseline_decisions,
        candidate_decisions: &simulation.candidate_decisions,
        diff: &simulation.diff,
        confidence: &simulation.confidence,
        risk: &simulation.risk,
        proof_classification: &simulation.proof_classification,
        apply_decision: &simulation.apply_decision,
        data_quality_warnings: &simulation.data_quality_warnings,
        side_effect_barrier_mode: &simulation.side_effect_barrier_mode,
        side_effects_captured: simulation.side_effects_captured,
    }
}

fn stable_hash<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_string(value).expect("resource digital twin hash payload serializes");
    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn stable_label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn stable_float(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

fn apply_budget_delta(base_budget: u64, delta: i128) -> u64 {
    if delta >= 0 {
        base_budget.saturating_add(u64::try_from(delta).unwrap_or(u64::MAX))
    } else {
        base_budget.saturating_sub(u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX))
    }
}

fn option_i128<T>(value: Option<T>) -> i128
where
    T: Into<i128>,
{
    value.map(Into::into).unwrap_or_default()
}

fn barrier_log_len(log: Option<&SideEffectLog>) -> usize {
    log.map(SideEffectLog::len).unwrap_or_default()
}

fn parse_f64(
    override_: &ResourceControlOverride,
    value: &str,
) -> Result<f64, ResourceDigitalTwinError> {
    let parsed =
        value
            .parse::<f64>()
            .map_err(|error| ResourceDigitalTwinError::InvalidOverride {
                knob_id: override_.knob_id.clone(),
                reason: error.to_string(),
            })?;
    if !parsed.is_finite() {
        return Err(ResourceDigitalTwinError::InvalidOverride {
            knob_id: override_.knob_id.clone(),
            reason: "non-finite value".to_string(),
        });
    }
    Ok(parsed)
}

fn parse_u64(
    override_: &ResourceControlOverride,
    value: &str,
) -> Result<u64, ResourceDigitalTwinError> {
    value
        .parse::<u64>()
        .map_err(|error| ResourceDigitalTwinError::InvalidOverride {
            knob_id: override_.knob_id.clone(),
            reason: error.to_string(),
        })
}

fn parse_bool(
    override_: &ResourceControlOverride,
    value: &str,
) -> Result<bool, ResourceDigitalTwinError> {
    value
        .parse::<bool>()
        .map_err(|error| ResourceDigitalTwinError::InvalidOverride {
            knob_id: override_.knob_id.clone(),
            reason: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_counterfactual::{
        RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION, ResourceControlOverrideLoader,
    };
    use crate::replay_scenario_matrix::{
        DigitalTwinTraceAdapter, ProofDimension, ProofExecutionEvidence, ScaleProofMatrix,
        ScaleScenarioClass, ScaleScenarioManifest, ScaleScenarioProof,
    };
    use crate::replay_side_effect_barrier::ReplayBarrier;
    use proptest::prelude::*;

    fn override_package(toml_body: &str) -> ResourceControlOverridePackage {
        let toml = format!(
            r#"
[meta]
schema_version = "{RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION}"
name = "candidate"
description = "test candidate"
base_trace = "test-trace"
created_at = "2026-05-06T00:00:00Z"
author = "test"

{toml_body}
"#
        );
        ResourceControlOverrideLoader::load(&toml).unwrap()
    }

    fn empty_package() -> ResourceControlOverridePackage {
        override_package("")
    }

    fn admission_step(
        step_id: &str,
        queue_utilization: Option<f64>,
        pending_items: Option<u64>,
        latency_ratio: Option<f64>,
        pressure_score: u8,
    ) -> DigitalTwinTraceStep {
        DigitalTwinTraceStep {
            step_id: step_id.to_string(),
            source: DigitalTwinTraceSource::ResourceAdmission,
            monotonic_ms: 100,
            source_hash: format!("source-{step_id}"),
            source_artifact_hashes: vec![format!("artifact-{step_id}")],
            pane_hash: None,
            agent_hash: None,
            correlation_hash: None,
            scheduler_sequence: None,
            scale_history_len: None,
            active_agent_count: None,
            queue_utilization,
            pending_items,
            admission_action: Some("defer".to_string()),
            reason_codes: vec!["fixture".to_string()],
            raw_pressure_severity: Some(pressure_score),
            effective_pressure_severity: Some(pressure_score),
            fleet_pressure: None,
            memory_tier_pressure: None,
            max_latency_over_budget_ratio: latency_ratio,
            memory_budget_bytes: None,
            memory_actual_bytes: None,
            resident_over_budget_bytes: None,
            reclaimable_bytes: None,
            proof_status: None,
            evidence_source: None,
            hardware_evidence_complete: None,
            hardware_cpu_count: None,
            hardware_memory_bytes: None,
            pressure_score,
            quality_flags: Vec::new(),
        }
    }

    fn memory_step(actual: u64, budget: u64, reclaimable: u64) -> DigitalTwinTraceStep {
        DigitalTwinTraceStep {
            step_id: "memory".to_string(),
            source: DigitalTwinTraceSource::MemoryTierBudget,
            monotonic_ms: 200,
            source_hash: "source-memory".to_string(),
            source_artifact_hashes: vec!["artifact-memory".to_string()],
            pane_hash: None,
            agent_hash: None,
            correlation_hash: None,
            scheduler_sequence: None,
            scale_history_len: None,
            active_agent_count: None,
            queue_utilization: None,
            pending_items: None,
            admission_action: None,
            reason_codes: Vec::new(),
            raw_pressure_severity: Some(0),
            effective_pressure_severity: Some(0),
            fleet_pressure: None,
            memory_tier_pressure: Some("elevated".to_string()),
            max_latency_over_budget_ratio: None,
            memory_budget_bytes: Some(budget),
            memory_actual_bytes: Some(actual),
            resident_over_budget_bytes: Some(actual.saturating_sub(budget)),
            reclaimable_bytes: Some(reclaimable),
            proof_status: None,
            evidence_source: None,
            hardware_evidence_complete: None,
            hardware_cpu_count: None,
            hardware_memory_bytes: None,
            pressure_score: 0,
            quality_flags: Vec::new(),
        }
    }

    fn proof_evidence(cpu_count: u32, memory_bytes: u64) -> ProofExecutionEvidence {
        ProofExecutionEvidence {
            cpu_count,
            memory_bytes,
            storage_bytes: 1_099_511_627_776,
            storage_class: "nvme".to_string(),
            os: "linux".to_string(),
            worker_id: "worker-proof".to_string(),
            command:
                "rch exec -- bash -lc 'cargo test -p frankenterm-core-replay --lib digital_twin'"
                    .to_string(),
            elapsed_ms: 12_345,
            git_commit: "482ab202c".to_string(),
        }
    }

    fn proof_trace_step(
        class: ScaleScenarioClass,
        status: ProofStatus,
        evidence_source: ScaleProofEvidenceSource,
        evidence: Option<ProofExecutionEvidence>,
    ) -> DigitalTwinTraceStep {
        let proof = ScaleScenarioProof {
            scenario_id: "live_10k_64core_256gib".to_string(),
            class,
            dimensions: vec![ProofDimension::Hardware, ProofDimension::Throughput],
            status,
            evidence_source,
            evidence,
            note: if status == ProofStatus::SkippedNotProven {
                "SKIPPED_NOT_PROVEN: live high-scale hardware predicate absent".to_string()
            } else {
                String::new()
            },
        };
        let matrix =
            ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), vec![proof]);
        DigitalTwinTraceAdapter::from_scale_proof_matrix(1, &matrix, Some("proof-artifact"))
            .into_iter()
            .next()
            .unwrap()
    }

    fn trace(steps: Vec<DigitalTwinTraceStep>) -> DigitalTwinTrace {
        DigitalTwinTraceAdapter::build(42, steps)
    }

    fn live_hardware_trace(mut steps: Vec<DigitalTwinTraceStep>) -> DigitalTwinTrace {
        let mut all_steps = vec![proof_trace_step(
            ScaleScenarioClass::LiveHardware,
            ProofStatus::Passed,
            ScaleProofEvidenceSource::LiveHardware,
            Some(proof_evidence(
                RESOURCE_DIGITAL_TWIN_HIGH_SCALE_MIN_CPU_COUNT,
                RESOURCE_DIGITAL_TWIN_HIGH_SCALE_MIN_MEMORY_BYTES,
            )),
        )];
        all_steps.append(&mut steps);
        trace(all_steps)
    }

    fn skipped_synthetic_trace(mut steps: Vec<DigitalTwinTraceStep>) -> DigitalTwinTrace {
        let mut all_steps = vec![proof_trace_step(
            ScaleScenarioClass::Synthetic,
            ProofStatus::SkippedNotProven,
            ScaleProofEvidenceSource::Synthetic,
            Some(proof_evidence(128, 549_755_813_888)),
        )];
        all_steps.append(&mut steps);
        trace(all_steps)
    }

    fn simulate(
        trace: &DigitalTwinTrace,
        package: &ResourceControlOverridePackage,
    ) -> ResourceDigitalTwinSimulation {
        let barrier = ReplayBarrier::new();
        ResourceDigitalTwinEngine::default()
            .simulate_with_barrier(trace, package, &barrier)
            .unwrap()
    }

    #[derive(Default)]
    struct GuardProbeBarrier {
        log: SideEffectLog,
        saw_simulation_active: std::sync::atomic::AtomicBool,
    }

    impl SideEffectBarrier for GuardProbeBarrier {
        fn process(
            &self,
            request: &EffectRequest,
        ) -> crate::replay_side_effect_barrier::EffectOutcome {
            self.saw_simulation_active.store(
                SimulationGuard::is_active(),
                std::sync::atomic::Ordering::SeqCst,
            );
            self.log
                .record(crate::replay_side_effect_barrier::SideEffectEntry {
                    index: 0,
                    timestamp_ms: request.timestamp_ms,
                    effect_type: request.effect_type,
                    pane_id: request.pane_id,
                    payload_summary: request.payload.clone(),
                    caller_hint: request.caller.clone(),
                    action_kind: request.action_kind,
                    metadata: request.metadata.clone(),
                });
            crate::replay_side_effect_barrier::EffectOutcome {
                executed: false,
                overridden: false,
                summary: "guard probe captured".to_string(),
            }
        }

        fn log(&self) -> Option<&SideEffectLog> {
            Some(&self.log)
        }

        fn mode_name(&self) -> &'static str {
            "guard_probe"
        }
    }

    #[test]
    fn simulation_scope_is_active_while_barrier_probe_runs() {
        let trace = trace(vec![admission_step(
            "stable",
            Some(0.40),
            Some(12),
            Some(0.70),
            0,
        )]);
        let options = ResourceDigitalTwinRunOptions {
            probe_side_effect_attempt: true,
            ..ResourceDigitalTwinRunOptions::default()
        };
        let barrier = GuardProbeBarrier::default();

        ResourceDigitalTwinEngine::default()
            .simulate_with_options(&trace, &empty_package(), &barrier, &options)
            .expect("guard-probe simulation should not execute side effects");

        assert!(
            barrier
                .saw_simulation_active
                .load(std::sync::atomic::Ordering::SeqCst),
            "simulation entrypoint must hold SimulationGuard while probing side effects"
        );
    }

    #[test]
    fn identical_baseline_and_candidate_emit_no_diff() {
        let trace = trace(vec![admission_step(
            "stable",
            Some(0.40),
            Some(12),
            Some(0.70),
            0,
        )]);

        let simulation = simulate(&trace, &empty_package());

        assert_eq!(simulation.diff.changed_steps, 0);
        assert_eq!(simulation.diff.admission_action_changes, 0);
        assert_eq!(simulation.diff.beneficial_changes, 0);
        assert_eq!(
            simulation.baseline_decisions,
            simulation.candidate_decisions
        );
        assert_eq!(simulation.side_effects_captured, 0);
    }

    #[test]
    fn beneficial_candidate_relaxes_queue_thresholds_and_reduces_pressure() {
        let trace = trace(vec![admission_step(
            "pressured",
            Some(0.90),
            Some(96),
            Some(1.20),
            0,
        )]);
        let package = override_package(
            r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.95"
reason = "allow higher queue utilization during burst replay"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "128"
reason = "larger pending queue on high-core workers"
"#,
        );

        let simulation = simulate(&trace, &package);
        let change = simulation.diff.changes.first().unwrap();

        assert_eq!(simulation.diff.admission_action_changes, 1);
        assert_eq!(change.baseline_action, SimulatedAdmissionAction::Degrade);
        assert_eq!(change.candidate_action, SimulatedAdmissionAction::Defer);
        assert_eq!(change.impact, CandidateImpact::Beneficial);
        assert_eq!(simulation.diff.beneficial_changes, 1);
    }

    #[test]
    fn harmful_candidate_tightens_queue_thresholds_and_increases_pressure() {
        let trace = trace(vec![admission_step(
            "moderate",
            Some(0.70),
            Some(40),
            Some(0.60),
            0,
        )]);
        let package = override_package(
            r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.55"
reason = "overly conservative queue threshold"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "20"
reason = "overly conservative pending threshold"
"#,
        );

        let simulation = simulate(&trace, &package);
        let change = simulation.diff.changes.first().unwrap();

        assert_eq!(change.baseline_action, SimulatedAdmissionAction::Admit);
        assert_eq!(change.candidate_action, SimulatedAdmissionAction::Shed);
        assert_eq!(change.impact, CandidateImpact::Harmful);
        assert_eq!(simulation.diff.harmful_changes, 1);
    }

    #[test]
    fn missing_telemetry_is_reported_as_observed_or_unknown_not_proven() {
        let mut step = admission_step("missing", None, None, None, 0);
        step.quality_flags = vec![
            DigitalTwinTraceQualityFlag::MissingQueueTelemetry,
            DigitalTwinTraceQualityFlag::MissingLatencyTelemetry,
        ];
        let trace = trace(vec![step]);

        let simulation = simulate(&trace, &empty_package());
        let frame = simulation.baseline_decisions.first().unwrap();

        assert_eq!(frame.admission_action, SimulatedAdmissionAction::Defer);
        assert_eq!(frame.admission_provenance, DecisionProvenance::Observed);
        assert!(
            simulation
                .data_quality_warnings
                .iter()
                .any(|warning| warning.contains("missing_queue_telemetry"))
        );
        assert!(
            simulation
                .data_quality_warnings
                .iter()
                .any(|warning| warning.contains("admission_observed_fallback"))
        );
    }

    #[test]
    fn side_effect_probe_is_captured_by_replay_barrier() {
        let trace = trace(vec![admission_step(
            "stable",
            Some(0.40),
            Some(12),
            Some(0.70),
            0,
        )]);
        let package = empty_package();
        let barrier = ReplayBarrier::new();
        let options = ResourceDigitalTwinRunOptions {
            generated_at_ms: 7,
            probe_side_effect_attempt: true,
            ..ResourceDigitalTwinRunOptions::default()
        };

        let simulation = ResourceDigitalTwinEngine::default()
            .simulate_with_options(&trace, &package, &barrier, &options)
            .unwrap();

        assert_eq!(simulation.side_effect_barrier_mode, "replay");
        assert_eq!(simulation.side_effects_captured, 1);
        let entries = barrier.log().unwrap().entries();
        assert_eq!(entries[0].effect_type, EffectType::FileWrite);
        assert_eq!(entries[0].payload_summary, "resource-digital-twin-probe");
    }

    #[test]
    fn fixed_trace_explains_candidate_defer_degrade_and_shed_differences() {
        let trace = trace(vec![
            admission_step("defer_case", Some(0.82), Some(60), Some(0.90), 0),
            admission_step("degrade_case", Some(0.90), Some(96), Some(1.40), 0),
            admission_step("shed_case", Some(0.99), Some(140), Some(2.20), 0),
            memory_step(512, 256, 64),
        ]);
        let package = override_package(
            r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.93"
reason = "larger queue headroom"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "128"
reason = "larger pending queue"

[[qos]]
knob_id = "qos.interactive_weight"
domain = "qos"
value = "2.0"
reason = "protect interactive latency"

[[memory_tier]]
knob_id = "memory.hot_resident_budget_bytes"
domain = "memory_tier"
value = "268435456"
reason = "more hot resident memory"
"#,
        );

        let simulation = simulate(&trace, &package);
        let changed_steps = simulation
            .diff
            .changes
            .iter()
            .map(|change| change.step_id.as_str())
            .collect::<Vec<_>>();

        assert!(changed_steps.contains(&"defer_case"));
        assert!(changed_steps.contains(&"degrade_case"));
        assert!(changed_steps.contains(&"shed_case"));
        assert!(simulation.diff.latency_stage_movements >= 1);
        assert!(simulation.diff.resident_memory_delta_bytes > 0);
        assert!(
            simulation
                .to_stable_json()
                .contains("\"admission_action_changes\"")
        );
    }

    #[test]
    fn live_hardware_low_risk_candidate_can_apply() {
        let trace = live_hardware_trace(vec![
            admission_step("stable_a", Some(0.40), Some(12), Some(0.70), 0),
            admission_step("stable_b", Some(0.45), Some(16), Some(0.75), 0),
            memory_step(96, 128, 4),
        ]);

        let simulation = simulate(&trace, &empty_package());

        assert_eq!(
            simulation.confidence.verdict,
            ResourceDigitalTwinGateVerdict::Pass
        );
        assert_eq!(
            simulation.risk.verdict,
            ResourceDigitalTwinGateVerdict::Pass
        );
        assert_eq!(
            simulation.proof_classification.proof_status,
            ProofStatus::Passed
        );
        assert_eq!(
            simulation.proof_classification.hardware_predicate,
            ResourceDigitalTwinHardwarePredicate::ProvenPredicateMet
        );
        assert!(simulation.proof_classification.high_scale_claim_allowed);
        assert_eq!(
            simulation.apply_decision.recommendation,
            ResourceDigitalTwinApplyRecommendation::Apply
        );
    }

    #[test]
    fn warning_quality_requires_review_not_apply() {
        let mut compacted = admission_step("compacted", Some(0.40), Some(12), Some(0.70), 0);
        compacted.quality_flags = vec![DigitalTwinTraceQualityFlag::CompactedSamples];
        let trace = live_hardware_trace(vec![
            compacted,
            admission_step("stable", Some(0.45), Some(16), Some(0.75), 0),
            memory_step(96, 128, 4),
        ]);

        let simulation = simulate(&trace, &empty_package());

        assert_eq!(
            simulation.confidence.verdict,
            ResourceDigitalTwinGateVerdict::Warning
        );
        assert_eq!(
            simulation.apply_decision.recommendation,
            ResourceDigitalTwinApplyRecommendation::ReviewOnly
        );
        assert!(
            simulation
                .apply_decision
                .reason_codes
                .contains(&"confidence_requires_review".to_string())
        );
    }

    #[test]
    fn harmful_candidate_rejection_blocks_apply() {
        let trace = live_hardware_trace(vec![
            admission_step("moderate", Some(0.70), Some(40), Some(0.60), 0),
            admission_step("stable", Some(0.45), Some(16), Some(0.75), 0),
            memory_step(96, 128, 4),
        ]);
        let package = override_package(
            r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.55"
reason = "overly conservative queue threshold"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "20"
reason = "overly conservative pending threshold"
"#,
        );

        let simulation = simulate(&trace, &package);

        assert_eq!(
            simulation.risk.verdict,
            ResourceDigitalTwinGateVerdict::Blocked
        );
        assert_eq!(
            simulation.apply_decision.recommendation,
            ResourceDigitalTwinApplyRecommendation::Blocked
        );
        assert!(
            simulation
                .risk
                .items
                .iter()
                .any(|item| item.code == "harmful_candidate_rejection")
        );
    }

    #[test]
    fn missing_hardware_and_low_confidence_fail_closed() {
        let trace = trace(vec![admission_step(
            "single",
            Some(0.40),
            Some(12),
            Some(0.70),
            0,
        )]);

        let simulation = simulate(&trace, &empty_package());

        assert_eq!(
            simulation.confidence.verdict,
            ResourceDigitalTwinGateVerdict::Blocked
        );
        assert!(simulation.confidence.score < simulation.confidence.threshold);
        assert_eq!(
            simulation.apply_decision.recommendation,
            ResourceDigitalTwinApplyRecommendation::Blocked
        );
        assert!(
            simulation
                .apply_decision
                .reason_codes
                .contains(&"confidence_below_apply_threshold".to_string())
        );
    }

    #[test]
    fn simulated_high_scale_benefits_remain_skipped_not_proven() {
        let trace = skipped_synthetic_trace(vec![
            admission_step("pressured", Some(0.90), Some(96), Some(1.20), 0),
            admission_step("stable", Some(0.45), Some(16), Some(0.75), 0),
            memory_step(96, 128, 4),
        ]);
        let package = override_package(
            r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.95"
reason = "synthetic high-scale candidate"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "128"
reason = "synthetic high-scale candidate"
"#,
        );

        let simulation = simulate(&trace, &package);

        assert_eq!(
            simulation.proof_classification.proof_status,
            ProofStatus::SkippedNotProven
        );
        assert_eq!(
            simulation.proof_classification.hardware_predicate,
            ResourceDigitalTwinHardwarePredicate::SkippedNotProven
        );
        assert!(!simulation.proof_classification.high_scale_claim_allowed);
        assert_eq!(
            simulation.apply_decision.recommendation,
            ResourceDigitalTwinApplyRecommendation::Blocked
        );
        assert!(
            simulation
                .apply_decision
                .reason_codes
                .contains(&"proof_skipped_not_proven".to_string())
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct GateGoldenSummary {
        schema_version: String,
        cases: Vec<GateGoldenCase>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct GateGoldenCase {
        case_id: String,
        recommendation: ResourceDigitalTwinApplyRecommendation,
        confidence_verdict: ResourceDigitalTwinGateVerdict,
        confidence_score: u8,
        risk_verdict: ResourceDigitalTwinGateVerdict,
        risk_score: u8,
        proof_status: ProofStatus,
        hardware_predicate: ResourceDigitalTwinHardwarePredicate,
        high_scale_claim_allowed: bool,
        blocker_codes: Vec<String>,
    }

    fn gate_golden_summary() -> GateGoldenSummary {
        let pass = simulate(
            &live_hardware_trace(vec![
                admission_step("stable_a", Some(0.40), Some(12), Some(0.70), 0),
                admission_step("stable_b", Some(0.45), Some(16), Some(0.75), 0),
                memory_step(96, 128, 4),
            ]),
            &empty_package(),
        );

        let mut warning_step = admission_step("compacted", Some(0.40), Some(12), Some(0.70), 0);
        warning_step.quality_flags = vec![DigitalTwinTraceQualityFlag::CompactedSamples];
        let warning = simulate(
            &live_hardware_trace(vec![
                warning_step,
                admission_step("stable", Some(0.45), Some(16), Some(0.75), 0),
                memory_step(96, 128, 4),
            ]),
            &empty_package(),
        );

        let blocked = simulate(
            &live_hardware_trace(vec![
                admission_step("moderate", Some(0.70), Some(40), Some(0.60), 0),
                admission_step("stable", Some(0.45), Some(16), Some(0.75), 0),
                memory_step(96, 128, 4),
            ]),
            &override_package(
                r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.55"
reason = "overly conservative queue threshold"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "20"
reason = "overly conservative pending threshold"
"#,
            ),
        );

        let skipped = simulate(
            &skipped_synthetic_trace(vec![
                admission_step("pressured", Some(0.90), Some(96), Some(1.20), 0),
                admission_step("stable", Some(0.45), Some(16), Some(0.75), 0),
                memory_step(96, 128, 4),
            ]),
            &override_package(
                r#"
[[admission]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.95"
reason = "synthetic high-scale candidate"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "128"
reason = "synthetic high-scale candidate"
"#,
            ),
        );

        GateGoldenSummary {
            schema_version: RESOURCE_DIGITAL_TWIN_GATE_SUMMARY_SCHEMA_VERSION.to_string(),
            cases: vec![
                gate_case("pass", &pass),
                gate_case("warning", &warning),
                gate_case("blocked", &blocked),
                gate_case("skipped_not_proven", &skipped),
            ],
        }
    }

    fn gate_case(case_id: &str, simulation: &ResourceDigitalTwinSimulation) -> GateGoldenCase {
        let mut blocker_codes = simulation.apply_decision.reason_codes.clone();
        blocker_codes.extend(simulation.confidence.blockers.clone());
        blocker_codes.extend(simulation.risk.items.iter().map(|item| item.code.clone()));
        blocker_codes.extend(simulation.proof_classification.blockers.clone());
        blocker_codes.sort();
        blocker_codes.dedup();

        GateGoldenCase {
            case_id: case_id.to_string(),
            recommendation: simulation.apply_decision.recommendation,
            confidence_verdict: simulation.confidence.verdict,
            confidence_score: simulation.confidence.score,
            risk_verdict: simulation.risk.verdict,
            risk_score: simulation.risk.score,
            proof_status: simulation.proof_classification.proof_status,
            hardware_predicate: simulation.proof_classification.hardware_predicate,
            high_scale_claim_allowed: simulation.proof_classification.high_scale_claim_allowed,
            blocker_codes,
        }
    }

    #[test]
    fn checked_in_gate_summary_fixture_matches_generated_cases() {
        let expected: GateGoldenSummary = serde_json::from_str(include_str!(
            "../../../fixtures/scale-lab/resource-digital-twin-gate-summary.v1.json"
        ))
        .unwrap();
        let generated = gate_golden_summary();

        assert_eq!(
            generated,
            expected,
            "{}",
            serde_json::to_string_pretty(&generated).unwrap()
        );
    }

    #[test]
    fn stable_json_is_repeatable_and_includes_warning_surface() {
        let mut step = admission_step("missing", None, None, None, 0);
        step.admission_action = None;
        let trace = trace(vec![step]);
        let first = simulate(&trace, &empty_package());
        let second = simulate(&trace, &empty_package());

        assert_eq!(first.to_stable_json(), second.to_stable_json());
        assert!(first.to_stable_json().contains("\"data_quality_warnings\""));
        assert!(
            first
                .data_quality_warnings
                .iter()
                .any(|warning| warning.contains("admission_unknown"))
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        #[test]
        fn deterministic_replay_for_same_trace_and_candidate(
            queue in 0.0f64..1.2,
            pending in 0u64..180,
            latency in 0.0f64..2.5,
            threshold in 50u64..180
        ) {
            let trace = trace(vec![admission_step(
                "prop",
                Some(queue),
                Some(pending),
                Some(latency),
                0,
            )]);
            let package = override_package(&format!(
                r#"
[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "{threshold}"
reason = "property threshold"
"#
            ));

            let first = simulate(&trace, &package);
            let second = simulate(&trace, &package);

            prop_assert_eq!(&first.simulation_hash, &second.simulation_hash);
            prop_assert_eq!(first.to_stable_json(), second.to_stable_json());
        }
    }
}
