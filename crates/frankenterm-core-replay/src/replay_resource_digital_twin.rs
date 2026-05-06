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
};
use crate::replay_side_effect_barrier::{
    EffectRequest, EffectType, SideEffectBarrier, SideEffectLog,
};
use frankenterm_core::policy::ActionKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

/// Schema version for baseline-vs-candidate resource digital-twin outputs.
pub const RESOURCE_DIGITAL_TWIN_SCHEMA_VERSION: &str = "ft.resource_digital_twin.v1";

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
    /// Test/diagnostic probe that attempts a side effect through the barrier.
    #[serde(default)]
    pub probe_side_effect_attempt: bool,
}

impl Default for ResourceDigitalTwinRunOptions {
    fn default() -> Self {
        Self {
            generated_at_ms: 0,
            probe_side_effect_attempt: false,
        }
    }
}

/// One simulated decision frame for a trace step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    let mut severity = queue_pressure
        .max(latency_stage.severity())
        .max(memory_pressure)
        .max(source_pressure);
    if step.source == DigitalTwinTraceSource::ScaleProof && step.proof_status.is_some() {
        severity = source_pressure;
    }

    let (admission_action, admission_provenance) = if step.queue_utilization.is_none()
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
    let spread_bonus = if knobs.topology_locality_spread_factor >= 0.50 {
        1
    } else {
        0
    };
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
        warnings.insert(format!("trace:{}", stable_label(flag)));
    }
    for frame in baseline.iter().chain(candidate.iter()) {
        for warning in &frame.warnings {
            warnings.insert(warning.clone());
        }
    }
    warnings.into_iter().collect()
}

fn warnings_from_trace_flags(step_id: &str, flags: &[DigitalTwinTraceQualityFlag]) -> Vec<String> {
    flags
        .iter()
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
    use crate::replay_scenario_matrix::DigitalTwinTraceAdapter;
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
            pressure_score: 0,
            quality_flags: Vec::new(),
        }
    }

    fn trace(steps: Vec<DigitalTwinTraceStep>) -> DigitalTwinTrace {
        DigitalTwinTraceAdapter::build(42, steps)
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
