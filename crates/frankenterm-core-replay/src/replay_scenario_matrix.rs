//! Scenario matrix runner for baseline-vs-candidate sweeps (ft-og6q6.4.3).
//!
//! Provides:
//! - [`MatrixConfig`] — TOML-based matrix definition (artifacts x overrides).
//! - [`ScenarioMatrixRunner`] — Executes all (artifact, override) pairs.
//! - [`MatrixResult`] — Aggregate results with diff summaries.
//! - [`ScenarioResult`] — Per-scenario outcome with decision diffs.
//! - [`ScaleScenarioManifest`] — Checked-in massive-swarm scenario tiers.
//! - [`ScaleProofMatrix`] — Machine-readable proof coverage for scale claims.
//!
//! Massive-swarm scale proof starts from
//! [`ScaleScenarioManifest::massive_swarm_defaults`]. Add new scenarios there
//! with deterministic seeds and expected counters, then attach bounded,
//! replay-backed, or live-hardware proof rows through [`ScaleProofMatrix`].
//! Hardware claims such as 64-core / 256 GiB hosts are only proven by passed
//! [`ScaleScenarioClass::LiveHardware`] rows with complete execution evidence.

use frankenterm_core::fleet_memory_controller::FleetMemoryTierBudgetSnapshot;
use frankenterm_core::resource_pressure_chaos::{
    ResourcePressureChaosStatus, ResourcePressureChaosVerdict,
};
use frankenterm_core::swarm_scheduler::{ResourceAdmissionDecisionSummary, SchedulerSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

// ============================================================================
// DiffSummary — decision-level diff between baseline and candidate
// ============================================================================

/// Summary of decision differences between baseline and candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Total decisions compared.
    pub total_decisions: u64,
    /// Decisions that are identical.
    pub unchanged: u64,
    /// Decisions in candidate but not in baseline.
    pub added: u64,
    /// Decisions in baseline but not in candidate.
    pub removed: u64,
    /// Decisions that exist in both but differ.
    pub modified: u64,
}

impl DiffSummary {
    /// Whether baseline and candidate produced identical decisions.
    #[must_use]
    pub fn is_identical(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.modified == 0
    }

    /// Total divergent decisions.
    #[must_use]
    pub fn divergence_count(&self) -> u64 {
        self.added + self.removed + self.modified
    }

    /// Compute diff from two decision sequences.
    #[must_use]
    pub fn compute(baseline: &[String], candidate: &[String]) -> Self {
        let base_len = baseline.len() as u64;
        let cand_len = candidate.len() as u64;
        let total = base_len.max(cand_len);
        let min_len = baseline.len().min(candidate.len());

        let mut unchanged = 0u64;
        let mut modified = 0u64;
        for i in 0..min_len {
            if baseline[i] == candidate[i] {
                unchanged += 1;
            } else {
                modified += 1;
            }
        }

        let added = cand_len.saturating_sub(base_len);
        let removed = base_len.saturating_sub(cand_len);

        Self {
            total_decisions: total,
            unchanged,
            added,
            removed,
            modified,
        }
    }
}

// ============================================================================
// ScenarioResult — per-scenario outcome
// ============================================================================

/// Result of a single (artifact, override) scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Artifact label or path.
    pub artifact_label: String,
    /// Override label or path (empty for baseline-only).
    pub override_label: String,
    /// Baseline decision sequence.
    pub baseline_decisions: Vec<String>,
    /// Candidate decision sequence.
    pub candidate_decisions: Vec<String>,
    /// Diff summary.
    pub diff: DiffSummary,
    /// Error message if scenario failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock duration in ms.
    pub duration_ms: u64,
}

impl ScenarioResult {
    /// Whether this scenario succeeded (no error).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }

    /// Whether baseline and candidate diverged.
    #[must_use]
    pub fn has_divergence(&self) -> bool {
        !self.diff.is_identical()
    }
}

// ============================================================================
// MatrixResult — aggregate results
// ============================================================================

/// Aggregate results across all scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixResult {
    /// Individual scenario results.
    pub scenarios: Vec<ScenarioResult>,
    /// Total scenarios executed.
    pub total_scenarios: usize,
    /// Scenarios that passed (identical decisions).
    pub pass_count: usize,
    /// Scenarios with divergence.
    pub divergence_count: usize,
    /// Scenarios with errors.
    pub error_count: usize,
    /// Total wall-clock duration in ms.
    pub total_duration_ms: u64,
}

impl MatrixResult {
    /// Build from scenario results.
    #[must_use]
    pub fn from_results(scenarios: Vec<ScenarioResult>) -> Self {
        let total_scenarios = scenarios.len();
        let pass_count = scenarios
            .iter()
            .filter(|s| s.is_ok() && !s.has_divergence())
            .count();
        let divergence_count = scenarios
            .iter()
            .filter(|s| s.is_ok() && s.has_divergence())
            .count();
        let error_count = scenarios.iter().filter(|s| !s.is_ok()).count();
        let total_duration_ms = scenarios.iter().map(|s| s.duration_ms).sum();

        Self {
            scenarios,
            total_scenarios,
            pass_count,
            divergence_count,
            error_count,
            total_duration_ms,
        }
    }

    /// Whether all scenarios passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.divergence_count == 0 && self.error_count == 0
    }

    /// Export as JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

// ============================================================================
// ProgressEvent — emitted during matrix execution
// ============================================================================

/// Progress event emitted during matrix execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    /// Completed scenarios so far.
    pub completed: usize,
    /// Total scenarios to run.
    pub total: usize,
    /// Current artifact being processed.
    pub current_artifact: String,
    /// Current override being applied.
    pub current_override: String,
}

// ============================================================================
// MatrixConfig — TOML-based matrix definition
// ============================================================================

/// Entry in the artifact list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// File path to the .ftreplay artifact.
    pub path: String,
    /// Human-readable label.
    #[serde(default)]
    pub label: String,
}

/// Entry in the override list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideEntry {
    /// File path to the .ftoverride package.
    pub path: String,
    /// Human-readable label.
    #[serde(default)]
    pub label: String,
}

/// Runner configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// Max concurrent scenarios (default: 2).
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Timeout per scenario in ms (default: 5 minutes).
    #[serde(default = "default_timeout")]
    pub timeout_per_scenario_ms: u64,
    /// Stop on first divergence.
    #[serde(default)]
    pub fail_fast: bool,
}

fn default_concurrency() -> usize {
    2
}
fn default_timeout() -> u64 {
    300_000
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            timeout_per_scenario_ms: default_timeout(),
            fail_fast: false,
        }
    }
}

/// Full matrix configuration (.ftmatrix TOML format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Artifacts to replay.
    #[serde(default)]
    pub artifacts: Vec<ArtifactEntry>,
    /// Override packages to apply.
    #[serde(default)]
    pub overrides: Vec<OverrideEntry>,
    /// Runner configuration.
    #[serde(default)]
    pub config: RunnerConfig,
}

impl MatrixConfig {
    /// Load from TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("matrix config parse error: {e}"))
    }

    /// Total scenarios = artifacts x overrides (or artifacts if no overrides).
    #[must_use]
    pub fn scenario_count(&self) -> usize {
        if self.overrides.is_empty() {
            self.artifacts.len()
        } else {
            self.artifacts.len() * self.overrides.len()
        }
    }

    /// Generate the (artifact, override) pairs to run.
    #[must_use]
    pub fn scenario_pairs(&self) -> Vec<(ArtifactEntry, Option<OverrideEntry>)> {
        let mut pairs = Vec::new();
        if self.overrides.is_empty() {
            for art in &self.artifacts {
                pairs.push((art.clone(), None));
            }
        } else {
            for art in &self.artifacts {
                for ovr in &self.overrides {
                    pairs.push((art.clone(), Some(ovr.clone())));
                }
            }
        }
        pairs
    }
}

// ============================================================================
// ScaleScenarioManifest — deterministic massive-swarm scenario inventory
// ============================================================================

/// Scenario origin class for scale-lab proof rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleScenarioClass {
    /// Deterministic synthetic generation; useful for bounded correctness.
    Synthetic,
    /// Replay artifact derived from real captured runs.
    ReplayBacked,
    /// Live execution on a measured hardware worker.
    LiveHardware,
}

/// Source class for evidence attached to a scale proof row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScaleProofEvidenceSource {
    /// Deterministic synthetic fixture evidence.
    Synthetic,
    /// Replay artifact derived from captured or generated transcripts.
    ReplayBacked,
    /// Reduced proof produced by an RCH remote worker.
    RchRemote,
    /// Proof produced on target-class high-core/high-memory hardware.
    LiveHardware,
    /// Older or incomplete rows that did not record the source class.
    #[default]
    Unknown,
}

/// Coverage dimension exercised by a scenario proof row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofDimension {
    /// Decision and event correctness.
    Correctness,
    /// Throughput and scheduling capacity.
    Throughput,
    /// Memory pressure and retention behavior.
    Memory,
    /// Live hardware capacity evidence.
    Hardware,
}

/// Proof row status for machine-readable scale evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProofStatus {
    /// The proof completed and satisfied its expected counters.
    Passed,
    /// The proof ran and failed.
    Failed,
    /// The proof gap is intentional and must not be marketed as proven.
    SkippedNotProven,
}

/// Expected traffic counters for one scale scenario fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleScenarioCounters {
    /// Logical pane count simulated or replayed by the scenario.
    pub logical_panes: u64,
    /// Logical agent count simulated or replayed by the scenario.
    pub logical_agents: u64,
    /// Pane/agent churn events.
    pub churn_events: u64,
    /// Alternate-screen enter/exit transitions.
    pub alt_screen_flips: u64,
    /// Burst event storms.
    pub event_storms: u64,
    /// Output burst volume in bytes.
    pub output_burst_bytes: u64,
    /// Storage writes expected from capture/index/audit paths.
    pub storage_writes: u64,
    /// Policy-denial or require-approval audit records.
    pub policy_denials: u64,
}

/// One deterministic massive-swarm scenario fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleScenarioManifestEntry {
    /// Stable scenario identifier.
    pub id: String,
    /// Human-readable scenario label.
    pub label: String,
    /// Scenario origin class.
    pub class: ScaleScenarioClass,
    /// Deterministic generation seed.
    pub deterministic_seed: u64,
    /// Expected traffic counters.
    pub counters: ScaleScenarioCounters,
    /// Proof dimensions this scenario is intended to cover.
    pub dimensions: Vec<ProofDimension>,
}

/// Checked-in deterministic scale scenario manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleScenarioManifest {
    /// Scenario fixtures available to the scale lab.
    pub scenarios: Vec<ScaleScenarioManifestEntry>,
}

impl ScaleScenarioManifest {
    /// Standard massive-swarm tiers for bounded proof runs.
    #[must_use]
    pub fn massive_swarm_defaults() -> Self {
        Self {
            scenarios: vec![
                ScaleScenarioManifestEntry {
                    id: "synthetic_1k_churn".to_string(),
                    label: "1k logical panes with churn and output bursts".to_string(),
                    class: ScaleScenarioClass::Synthetic,
                    deterministic_seed: 1_001_001,
                    counters: ScaleScenarioCounters {
                        logical_panes: 1_024,
                        logical_agents: 1_024,
                        churn_events: 6_144,
                        alt_screen_flips: 2_048,
                        event_storms: 32,
                        output_burst_bytes: 268_435_456,
                        storage_writes: 65_536,
                        policy_denials: 512,
                    },
                    dimensions: vec![
                        ProofDimension::Correctness,
                        ProofDimension::Throughput,
                        ProofDimension::Memory,
                    ],
                },
                ScaleScenarioManifestEntry {
                    id: "synthetic_5k_event_storm".to_string(),
                    label: "5k logical panes under event storms".to_string(),
                    class: ScaleScenarioClass::Synthetic,
                    deterministic_seed: 5_005_005,
                    counters: ScaleScenarioCounters {
                        logical_panes: 5_120,
                        logical_agents: 5_120,
                        churn_events: 40_960,
                        alt_screen_flips: 12_288,
                        event_storms: 128,
                        output_burst_bytes: 1_610_612_736,
                        storage_writes: 327_680,
                        policy_denials: 2_560,
                    },
                    dimensions: vec![
                        ProofDimension::Correctness,
                        ProofDimension::Throughput,
                        ProofDimension::Memory,
                    ],
                },
                ScaleScenarioManifestEntry {
                    id: "synthetic_10k_policy_audit".to_string(),
                    label: "10k logical panes with policy audit traffic".to_string(),
                    class: ScaleScenarioClass::Synthetic,
                    deterministic_seed: 10_010_010,
                    counters: ScaleScenarioCounters {
                        logical_panes: 10_240,
                        logical_agents: 10_240,
                        churn_events: 102_400,
                        alt_screen_flips: 24_576,
                        event_storms: 256,
                        output_burst_bytes: 4_294_967_296,
                        storage_writes: 786_432,
                        policy_denials: 10_240,
                    },
                    dimensions: vec![
                        ProofDimension::Correctness,
                        ProofDimension::Throughput,
                        ProofDimension::Memory,
                    ],
                },
            ],
        }
    }

    /// Look up a scenario by stable id.
    #[must_use]
    pub fn scenario(&self, id: &str) -> Option<&ScaleScenarioManifestEntry> {
        self.scenarios.iter().find(|scenario| scenario.id == id)
    }
}

/// Execution evidence attached to a proof row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofExecutionEvidence {
    /// Logical CPU count reported by the worker.
    pub cpu_count: u32,
    /// Worker RAM in bytes.
    pub memory_bytes: u64,
    /// Storage capacity or scratch budget in bytes.
    pub storage_bytes: u64,
    /// Storage class observed for the proof lane.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub storage_class: String,
    /// Operating system identifier.
    pub os: String,
    /// Worker identifier.
    pub worker_id: String,
    /// Exact command that produced the evidence.
    pub command: String,
    /// Command elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// Git commit tested by the command.
    pub git_commit: String,
}

impl ProofExecutionEvidence {
    /// Whether all evidence fields needed for hardware claims are present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.cpu_count > 0
            && self.memory_bytes > 0
            && self.storage_bytes > 0
            && !self.storage_class.is_empty()
            && !self.os.is_empty()
            && !self.worker_id.is_empty()
            && !self.command.is_empty()
            && self.elapsed_ms > 0
            && !self.git_commit.is_empty()
    }
}

/// One row in the scale proof matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleScenarioProof {
    /// Scenario id from [`ScaleScenarioManifest`].
    pub scenario_id: String,
    /// Proof origin class.
    pub class: ScaleScenarioClass,
    /// Dimensions this proof row covers.
    pub dimensions: Vec<ProofDimension>,
    /// Proof result.
    pub status: ProofStatus,
    /// Where this proof evidence came from.
    #[serde(default)]
    pub evidence_source: ScaleProofEvidenceSource,
    /// Execution evidence, required for live hardware claims.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ProofExecutionEvidence>,
    /// Short operator note for failures or skipped gaps.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl ScaleScenarioProof {
    /// Whether this row proves a live hardware capacity claim.
    #[must_use]
    pub fn proves_hardware_claim(&self, min_cpu_count: u32, min_memory_bytes: u64) -> bool {
        self.status == ProofStatus::Passed
            && self.class == ScaleScenarioClass::LiveHardware
            && self.evidence_source == ScaleProofEvidenceSource::LiveHardware
            && self.dimensions.contains(&ProofDimension::Hardware)
            && self.evidence.as_ref().is_some_and(|evidence| {
                evidence.is_complete()
                    && evidence.cpu_count >= min_cpu_count
                    && evidence.memory_bytes >= min_memory_bytes
            })
    }
}

/// Machine-readable scale proof matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleProofMatrix {
    /// Deterministic scenario inventory.
    pub manifest: ScaleScenarioManifest,
    /// Proof rows attached to scenarios.
    pub proofs: Vec<ScaleScenarioProof>,
}

impl ScaleProofMatrix {
    /// Build a matrix from a manifest and proof rows.
    #[must_use]
    pub fn new(manifest: ScaleScenarioManifest, proofs: Vec<ScaleScenarioProof>) -> Self {
        Self { manifest, proofs }
    }

    /// Whether at least one live hardware proof satisfies the requested host claim.
    #[must_use]
    pub fn hardware_claims_proven(&self, min_cpu_count: u32, min_memory_bytes: u64) -> bool {
        self.proofs
            .iter()
            .any(|proof| proof.proves_hardware_claim(min_cpu_count, min_memory_bytes))
    }

    /// Hardware-dimension rows that do not prove the requested host claim.
    #[must_use]
    pub fn unproven_hardware_claims(
        &self,
        min_cpu_count: u32,
        min_memory_bytes: u64,
    ) -> Vec<String> {
        self.proofs
            .iter()
            .filter(|proof| proof.dimensions.contains(&ProofDimension::Hardware))
            .filter(|proof| !proof.proves_hardware_claim(min_cpu_count, min_memory_bytes))
            .map(|proof| proof.scenario_id.clone())
            .collect()
    }

    /// Summarize proof status and dimension coverage.
    #[must_use]
    pub fn coverage_summary(&self) -> ScaleProofCoverageSummary {
        ScaleProofCoverageSummary::from_proofs(&self.proofs)
    }

    /// Validate proof rows as a durable evidence index.
    #[must_use]
    pub fn validate_evidence_index(
        &self,
        min_cpu_count: u32,
        min_memory_bytes: u64,
    ) -> Vec<ScaleProofMatrixFinding> {
        let mut findings = Vec::new();

        for proof in &self.proofs {
            if proof.scenario_id.trim().is_empty() {
                findings.push(ScaleProofMatrixFinding::error(
                    proof,
                    "missing_scenario_id",
                    "proof rows must link to a stable scale-lab scenario id",
                ));
            } else if self.manifest.scenario(&proof.scenario_id).is_none() {
                findings.push(ScaleProofMatrixFinding::error(
                    proof,
                    "unknown_scenario_id",
                    "proof row scenario_id is not present in the scale-lab manifest",
                ));
            }

            if proof.status == ProofStatus::Passed
                && proof.evidence_source == ScaleProofEvidenceSource::Unknown
            {
                findings.push(ScaleProofMatrixFinding::warning(
                    proof,
                    "unknown_evidence_source",
                    "passed proof rows should record synthetic, replay, RCH, or live-hardware evidence source",
                ));
            }

            if proof.status == ProofStatus::Passed
                && proof.dimensions.contains(&ProofDimension::Hardware)
            {
                if proof.class != ScaleScenarioClass::LiveHardware
                    || proof.evidence_source != ScaleProofEvidenceSource::LiveHardware
                {
                    findings.push(ScaleProofMatrixFinding::error(
                        proof,
                        "hardware_pass_not_live_hardware",
                        "passed hardware claims must come from live-hardware rows, not synthetic or replay evidence",
                    ));
                    continue;
                }

                let Some(evidence) = &proof.evidence else {
                    findings.push(ScaleProofMatrixFinding::error(
                        proof,
                        "hardware_pass_missing_execution_evidence",
                        "passed hardware claims require CPU, RAM, storage, worker, command, elapsed time, and git evidence",
                    ));
                    continue;
                };

                if !evidence.is_complete() {
                    findings.push(ScaleProofMatrixFinding::error(
                        proof,
                        "hardware_pass_incomplete_execution_evidence",
                        "passed hardware claims cannot omit CPU, RAM, storage class, worker, command, elapsed time, or git evidence",
                    ));
                }

                if evidence.cpu_count < min_cpu_count || evidence.memory_bytes < min_memory_bytes {
                    findings.push(ScaleProofMatrixFinding::error(
                        proof,
                        "hardware_pass_predicate_not_met",
                        "passed hardware claims must satisfy the requested CPU and RAM predicates",
                    ));
                }
            }

            if proof.status == ProofStatus::SkippedNotProven
                && proof.dimensions.contains(&ProofDimension::Hardware)
                && !proof.note.contains("SKIPPED_NOT_PROVEN")
            {
                findings.push(ScaleProofMatrixFinding::warning(
                    proof,
                    "hardware_gap_missing_skip_label",
                    "hardware proof gaps should say SKIPPED_NOT_PROVEN so reports cannot promote them to proven",
                ));
            }
        }

        findings
    }
}

/// Aggregated coverage counters for a scale proof matrix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleProofCoverageSummary {
    /// Total proof rows.
    pub total_rows: usize,
    /// Passed proof rows.
    pub passed_rows: usize,
    /// Failed proof rows.
    pub failed_rows: usize,
    /// Intentionally skipped proof gaps.
    pub skipped_not_proven_rows: usize,
    /// Synthetic proof rows.
    pub synthetic_rows: usize,
    /// Replay-backed proof rows.
    pub replay_backed_rows: usize,
    /// Live-hardware proof rows.
    pub live_hardware_rows: usize,
    /// Passed correctness coverage rows.
    pub correctness_passed: usize,
    /// Passed throughput coverage rows.
    pub throughput_passed: usize,
    /// Passed memory coverage rows.
    pub memory_passed: usize,
    /// Passed live-hardware coverage rows.
    pub hardware_passed: usize,
}

/// Severity for scale proof matrix validation findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleProofFindingSeverity {
    /// The evidence index violates a truthfulness invariant.
    Error,
    /// The row is usable, but less explicit than operator surfaces expect.
    Warning,
}

/// Validation finding for a scale proof evidence index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleProofMatrixFinding {
    /// Scenario row that produced the finding.
    pub scenario_id: String,
    /// Finding severity.
    pub severity: ScaleProofFindingSeverity,
    /// Stable machine-readable reason.
    pub reason_code: String,
    /// Operator-facing finding text.
    pub message: String,
}

impl ScaleProofMatrixFinding {
    fn error(proof: &ScaleScenarioProof, reason_code: &str, message: &str) -> Self {
        Self {
            scenario_id: proof.scenario_id.clone(),
            severity: ScaleProofFindingSeverity::Error,
            reason_code: reason_code.to_string(),
            message: message.to_string(),
        }
    }

    fn warning(proof: &ScaleScenarioProof, reason_code: &str, message: &str) -> Self {
        Self {
            scenario_id: proof.scenario_id.clone(),
            severity: ScaleProofFindingSeverity::Warning,
            reason_code: reason_code.to_string(),
            message: message.to_string(),
        }
    }
}

impl ScaleProofCoverageSummary {
    fn from_proofs(proofs: &[ScaleScenarioProof]) -> Self {
        let mut summary = Self::default();

        for proof in proofs {
            summary.total_rows += 1;
            match proof.status {
                ProofStatus::Passed => summary.passed_rows += 1,
                ProofStatus::Failed => summary.failed_rows += 1,
                ProofStatus::SkippedNotProven => summary.skipped_not_proven_rows += 1,
            }

            match proof.class {
                ScaleScenarioClass::Synthetic => summary.synthetic_rows += 1,
                ScaleScenarioClass::ReplayBacked => summary.replay_backed_rows += 1,
                ScaleScenarioClass::LiveHardware => summary.live_hardware_rows += 1,
            }

            if proof.status != ProofStatus::Passed {
                continue;
            }

            for dimension in &proof.dimensions {
                match dimension {
                    ProofDimension::Correctness => summary.correctness_passed += 1,
                    ProofDimension::Throughput => summary.throughput_passed += 1,
                    ProofDimension::Memory => summary.memory_passed += 1,
                    ProofDimension::Hardware => {
                        if proof.class == ScaleScenarioClass::LiveHardware {
                            summary.hardware_passed += 1;
                        }
                    }
                }
            }
        }

        summary
    }
}

// ============================================================================
// DigitalTwinTrace — deterministic resource-control what-if trace adapter
// ============================================================================

/// Schema version for replay-backed digital-twin trace artifacts.
pub const DIGITAL_TWIN_TRACE_SCHEMA_VERSION: &str = "ft.digital_twin_trace.v1";

/// Source family represented by one digital-twin trace step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigitalTwinTraceSource {
    /// Scheduler checkpoint or autoscaler snapshot.
    SchedulerSnapshot,
    /// Resource admission controller decision.
    ResourceAdmission,
    /// Fleet memory-tier budget snapshot.
    MemoryTierBudget,
    /// Resource-pressure chaos verdict row.
    ResourcePressureChaos,
    /// Scale-lab proof matrix row.
    ScaleProof,
}

impl DigitalTwinTraceSource {
    /// Stable source label for machine-facing trace output and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SchedulerSnapshot => "scheduler_snapshot",
            Self::ResourceAdmission => "resource_admission",
            Self::MemoryTierBudget => "memory_tier_budget",
            Self::ResourcePressureChaos => "resource_pressure_chaos",
            Self::ScaleProof => "scale_proof",
        }
    }
}

/// Data quality markers attached to trace steps and aggregate traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigitalTwinTraceQualityFlag {
    /// A source timestamp was zero or stale enough to be unsafe for sequencing.
    StaleTimestamp,
    /// Queue utilization or backlog data was absent.
    MissingQueueTelemetry,
    /// Fleet pressure data was absent.
    MissingFleetTelemetry,
    /// Memory-tier budget data was absent.
    MissingMemoryTierTelemetry,
    /// Latency-stage data was absent.
    MissingLatencyTelemetry,
    /// Hardware proof evidence was absent or incomplete.
    IncompleteHardwareEvidence,
    /// Evidence came from synthetic or replay data, not live target hardware.
    SimulatedEvidence,
    /// A source timestamp regressed and was clamped to preserve monotonic order.
    NonMonotonicTimestampAdjusted,
    /// Raw pane, agent, scenario, or correlation identifiers were hashed.
    RedactedIdentity,
    /// Low-value samples were dropped by phase/extrema compaction.
    CompactedSamples,
    /// A source artifact hash was absent and had to be derived from the source row.
    DerivedSourceHash,
    /// Non-finite numeric telemetry was dropped from the trace.
    NonFiniteTelemetry,
}

/// One deterministic step consumed by the replay-backed resource digital twin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigitalTwinTraceStep {
    /// Stable step id inside the trace.
    pub step_id: String,
    /// Source family for this step.
    pub source: DigitalTwinTraceSource,
    /// Monotonic timestamp used by the simulator.
    pub monotonic_ms: u64,
    /// Deterministic hash of the source row used to derive this step.
    pub source_hash: String,
    /// Optional hashes for external source artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_artifact_hashes: Vec<String>,
    /// Redacted pane id, if the source row carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_hash: Option<String>,
    /// Redacted agent id, if the source row carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_hash: Option<String>,
    /// Redacted scenario or correlation id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_hash: Option<String>,
    /// Scheduler sequence, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler_sequence: Option<u64>,
    /// Count of scheduler scale events represented by a snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_history_len: Option<u64>,
    /// Count of known agents represented by a scheduler snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_agent_count: Option<u64>,
    /// Queue utilization observed by an admission or capacity source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_utilization: Option<f64>,
    /// Pending work items observed by an admission or scheduler source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_items: Option<u64>,
    /// Final admission action label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission_action: Option<String>,
    /// Stable reason labels carried by the decision source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    /// Raw pressure severity before protection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_pressure_severity: Option<u8>,
    /// Effective pressure severity after protection/fail-closed gates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_pressure_severity: Option<u8>,
    /// Compound fleet pressure label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_pressure: Option<String>,
    /// Memory-tier pressure label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_tier_pressure: Option<String>,
    /// Maximum latency-over-budget ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_latency_over_budget_ratio: Option<f64>,
    /// Total memory budget bytes represented by this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_budget_bytes: Option<u64>,
    /// Total observed memory bytes represented by this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_actual_bytes: Option<u64>,
    /// Resident memory bytes above budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_over_budget_bytes: Option<u64>,
    /// Bytes reclaimable without losing authoritative state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaimable_bytes: Option<u64>,
    /// Proof row status, if this step came from scale evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_status: Option<ProofStatus>,
    /// Proof evidence source, if this step came from scale evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_source: Option<ScaleProofEvidenceSource>,
    /// Whether hardware evidence is complete for live proof claims.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware_evidence_complete: Option<bool>,
    /// Normalized pressure score used by compaction and risk ordering.
    pub pressure_score: u8,
    /// Step-local data quality flags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quality_flags: Vec<DigitalTwinTraceQualityFlag>,
}

impl DigitalTwinTraceStep {
    fn normalized(mut self) -> Self {
        sort_dedup(&mut self.source_artifact_hashes);
        sort_dedup(&mut self.reason_codes);
        sort_dedup(&mut self.quality_flags);
        self
    }
}

/// Deterministic trace consumed by future baseline-vs-candidate digital twins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigitalTwinTrace {
    /// Versioned schema id.
    pub schema_version: String,
    /// Stable generation timestamp supplied by the caller.
    pub generated_at_ms: u64,
    /// Deterministic hash of normalized trace content.
    pub trace_hash: String,
    /// Hashes of source artifacts represented by this trace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_artifact_hashes: Vec<String>,
    /// Ordered trace steps.
    pub steps: Vec<DigitalTwinTraceStep>,
    /// Aggregate data-quality flags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quality_flags: Vec<DigitalTwinTraceQualityFlag>,
}

impl DigitalTwinTrace {
    /// Whether any source step reports incomplete, stale, synthetic, or adjusted data.
    #[must_use]
    pub fn has_quality_warnings(&self) -> bool {
        !self.quality_flags.is_empty()
    }

    /// Stable JSON representation for golden fixtures and Robot contracts.
    #[must_use]
    pub fn to_stable_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("digital twin trace serializes to stable JSON")
    }

    /// Stable compact TOON-like representation for golden fixtures and robot summaries.
    #[must_use]
    pub fn to_toon(&self) -> String {
        let mut output = format!(
            "schema_version: {}\ngenerated_at_ms: {}\ntrace_hash: {}\nquality_flags: {}\nsteps[{}]:\n",
            self.schema_version,
            self.generated_at_ms,
            self.trace_hash,
            label_list(&self.quality_flags),
            self.steps.len()
        );
        for step in &self.steps {
            output.push_str(&format!(
                "  - id={} source={} t={} pressure={} action={} fleet={} memory={} proof={} evidence={} flags={}\n",
                step.step_id,
                step.source.as_str(),
                step.monotonic_ms,
                step.pressure_score,
                step.admission_action.as_deref().unwrap_or("-"),
                step.fleet_pressure.as_deref().unwrap_or("-"),
                step.memory_tier_pressure.as_deref().unwrap_or("-"),
                option_label(step.proof_status.as_ref()),
                option_label(step.evidence_source.as_ref()),
                label_list(&step.quality_flags)
            ));
        }
        output
    }
}

/// Builder and source adapters for deterministic digital-twin traces.
pub struct DigitalTwinTraceAdapter;

impl DigitalTwinTraceAdapter {
    /// Build a normalized deterministic trace from pre-adapted source steps.
    #[must_use]
    pub fn build(generated_at_ms: u64, steps: Vec<DigitalTwinTraceStep>) -> DigitalTwinTrace {
        Self::build_with_compaction(generated_at_ms, steps, None)
    }

    /// Build a trace and optionally compact low-value repeated samples.
    #[must_use]
    pub fn build_with_compaction(
        generated_at_ms: u64,
        steps: Vec<DigitalTwinTraceStep>,
        max_steps: Option<usize>,
    ) -> DigitalTwinTrace {
        let mut steps = steps
            .into_iter()
            .map(DigitalTwinTraceStep::normalized)
            .collect::<Vec<_>>();
        steps.sort_by(|left, right| {
            left.monotonic_ms
                .cmp(&right.monotonic_ms)
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.step_id.cmp(&right.step_id))
        });
        clamp_monotonic_timestamps(&mut steps);

        let mut aggregate_flags = Vec::new();
        if let Some(limit) = max_steps.filter(|limit| *limit > 1) {
            let before = steps.len();
            steps = compact_phase_changes_and_extrema(steps, limit);
            if steps.len() < before {
                aggregate_flags.push(DigitalTwinTraceQualityFlag::CompactedSamples);
                for step in &mut steps {
                    step.quality_flags
                        .push(DigitalTwinTraceQualityFlag::CompactedSamples);
                    sort_dedup(&mut step.quality_flags);
                }
            }
        }

        let mut source_artifact_hashes = Vec::new();
        for step in &steps {
            aggregate_flags.extend(step.quality_flags.iter().copied());
            source_artifact_hashes.extend(step.source_artifact_hashes.iter().cloned());
            source_artifact_hashes.push(step.source_hash.clone());
        }
        sort_dedup(&mut aggregate_flags);
        sort_dedup(&mut source_artifact_hashes);

        let trace_hash = stable_hash(&(
            DIGITAL_TWIN_TRACE_SCHEMA_VERSION,
            generated_at_ms,
            &source_artifact_hashes,
            &steps,
        ));

        DigitalTwinTrace {
            schema_version: DIGITAL_TWIN_TRACE_SCHEMA_VERSION.to_string(),
            generated_at_ms,
            trace_hash,
            source_artifact_hashes,
            steps,
            quality_flags: aggregate_flags,
        }
    }

    /// Adapt a scheduler snapshot into a digital-twin trace step.
    #[must_use]
    pub fn from_scheduler_snapshot(
        step_id: &str,
        snapshot: &SchedulerSnapshot,
        source_artifact_hash: Option<&str>,
    ) -> DigitalTwinTraceStep {
        let mut quality_flags = Vec::new();
        if snapshot.last_evaluation_ms == 0 {
            quality_flags.push(DigitalTwinTraceQualityFlag::StaleTimestamp);
        }

        DigitalTwinTraceStep {
            step_id: step_id.to_string(),
            source: DigitalTwinTraceSource::SchedulerSnapshot,
            monotonic_ms: snapshot.last_evaluation_ms,
            source_hash: stable_hash(snapshot),
            source_artifact_hashes: source_hashes(
                source_artifact_hash,
                snapshot,
                &mut quality_flags,
            ),
            pane_hash: None,
            agent_hash: None,
            correlation_hash: None,
            scheduler_sequence: Some(snapshot.sequence),
            scale_history_len: Some(snapshot.scale_history.len() as u64),
            active_agent_count: Some(snapshot.agent_first_seen.len() as u64),
            queue_utilization: None,
            pending_items: None,
            admission_action: None,
            reason_codes: Vec::new(),
            raw_pressure_severity: None,
            effective_pressure_severity: None,
            fleet_pressure: None,
            memory_tier_pressure: None,
            max_latency_over_budget_ratio: None,
            memory_budget_bytes: None,
            memory_actual_bytes: None,
            resident_over_budget_bytes: None,
            reclaimable_bytes: None,
            proof_status: None,
            evidence_source: None,
            hardware_evidence_complete: None,
            pressure_score: 0,
            quality_flags,
        }
    }

    /// Adapt one resource admission decision into a digital-twin trace step.
    #[must_use]
    pub fn from_resource_admission_decision(
        step_id: &str,
        observed_at_ms: u64,
        decision: &ResourceAdmissionDecisionSummary,
        pane_id: Option<&str>,
        agent_id: Option<&str>,
        source_artifact_hash: Option<&str>,
    ) -> DigitalTwinTraceStep {
        let mut quality_flags = Vec::new();
        if observed_at_ms == 0 {
            quality_flags.push(DigitalTwinTraceQualityFlag::StaleTimestamp);
        }
        let queue_utilization = finite_f64(decision.queue_utilization, &mut quality_flags);
        let max_latency_over_budget_ratio =
            finite_f64(decision.max_latency_over_budget_ratio, &mut quality_flags);
        if queue_utilization.is_none() || decision.pending_items.is_none() {
            quality_flags.push(DigitalTwinTraceQualityFlag::MissingQueueTelemetry);
        }
        if decision.fleet_pressure.is_none() {
            quality_flags.push(DigitalTwinTraceQualityFlag::MissingFleetTelemetry);
        }
        if decision.memory_tier_pressure.is_none() {
            quality_flags.push(DigitalTwinTraceQualityFlag::MissingMemoryTierTelemetry);
        }
        if max_latency_over_budget_ratio.is_none() {
            quality_flags.push(DigitalTwinTraceQualityFlag::MissingLatencyTelemetry);
        }

        let pane_hash = redacted_hash("pane", pane_id, &mut quality_flags);
        let agent_hash = redacted_hash("agent", agent_id, &mut quality_flags);

        DigitalTwinTraceStep {
            step_id: step_id.to_string(),
            source: DigitalTwinTraceSource::ResourceAdmission,
            monotonic_ms: observed_at_ms,
            source_hash: stable_hash(decision),
            source_artifact_hashes: source_hashes(
                source_artifact_hash,
                decision,
                &mut quality_flags,
            ),
            pane_hash,
            agent_hash,
            correlation_hash: None,
            scheduler_sequence: None,
            scale_history_len: None,
            active_agent_count: None,
            queue_utilization,
            pending_items: decision.pending_items.map(u64::from),
            admission_action: Some(stable_label(&decision.action)),
            reason_codes: decision.reason_codes.iter().map(stable_label).collect(),
            raw_pressure_severity: Some(decision.raw_pressure_severity),
            effective_pressure_severity: Some(decision.effective_pressure_severity),
            fleet_pressure: decision.fleet_pressure.map(|tier| stable_label(&tier)),
            memory_tier_pressure: decision
                .memory_tier_pressure
                .map(|tier| stable_label(&tier)),
            max_latency_over_budget_ratio,
            memory_budget_bytes: None,
            memory_actual_bytes: None,
            resident_over_budget_bytes: None,
            reclaimable_bytes: None,
            proof_status: None,
            evidence_source: None,
            hardware_evidence_complete: None,
            pressure_score: decision.effective_pressure_severity,
            quality_flags,
        }
    }

    /// Adapt a memory-tier budget snapshot into a digital-twin trace step.
    #[must_use]
    pub fn from_memory_budget_snapshot(
        step_id: &str,
        observed_at_ms: u64,
        snapshot: &FleetMemoryTierBudgetSnapshot,
        source_artifact_hash: Option<&str>,
    ) -> DigitalTwinTraceStep {
        let mut quality_flags = Vec::new();
        if observed_at_ms == 0 {
            quality_flags.push(DigitalTwinTraceQualityFlag::StaleTimestamp);
        }
        if snapshot.tiers.is_empty() {
            quality_flags.push(DigitalTwinTraceQualityFlag::MissingMemoryTierTelemetry);
        }
        let pressure = snapshot.pressure_tier();

        DigitalTwinTraceStep {
            step_id: step_id.to_string(),
            source: DigitalTwinTraceSource::MemoryTierBudget,
            monotonic_ms: observed_at_ms,
            source_hash: stable_hash(snapshot),
            source_artifact_hashes: source_hashes(
                source_artifact_hash,
                snapshot,
                &mut quality_flags,
            ),
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
            raw_pressure_severity: Some(pressure.as_u8()),
            effective_pressure_severity: Some(pressure.as_u8()),
            fleet_pressure: Some(stable_label(&pressure)),
            memory_tier_pressure: Some(stable_label(&pressure)),
            max_latency_over_budget_ratio: None,
            memory_budget_bytes: Some(snapshot.totals.budget_bytes),
            memory_actual_bytes: Some(snapshot.totals.actual_bytes),
            resident_over_budget_bytes: Some(snapshot.totals.resident_over_budget_bytes),
            reclaimable_bytes: Some(snapshot.totals.reclaimable_bytes),
            proof_status: None,
            evidence_source: None,
            hardware_evidence_complete: None,
            pressure_score: pressure.as_u8(),
            quality_flags,
        }
    }

    /// Adapt a scale proof matrix into one trace step per proof row.
    #[must_use]
    pub fn from_scale_proof_matrix(
        observed_at_ms: u64,
        matrix: &ScaleProofMatrix,
        source_artifact_hash: Option<&str>,
    ) -> Vec<DigitalTwinTraceStep> {
        matrix
            .proofs
            .iter()
            .enumerate()
            .map(|(idx, proof)| {
                let mut quality_flags = Vec::new();
                if observed_at_ms == 0 {
                    quality_flags.push(DigitalTwinTraceQualityFlag::StaleTimestamp);
                }
                if proof.class != ScaleScenarioClass::LiveHardware
                    || proof.evidence_source != ScaleProofEvidenceSource::LiveHardware
                {
                    quality_flags.push(DigitalTwinTraceQualityFlag::SimulatedEvidence);
                }
                let evidence_complete = proof.evidence.as_ref().is_some_and(|e| e.is_complete());
                if proof.dimensions.contains(&ProofDimension::Hardware) && !evidence_complete {
                    quality_flags.push(DigitalTwinTraceQualityFlag::IncompleteHardwareEvidence);
                }
                let correlation_hash =
                    redacted_hash("scenario", Some(&proof.scenario_id), &mut quality_flags);
                let pressure_score = match proof.status {
                    ProofStatus::Passed => 0,
                    ProofStatus::SkippedNotProven => 1,
                    ProofStatus::Failed => 3,
                };

                DigitalTwinTraceStep {
                    step_id: format!("scale_proof:{idx:04}"),
                    source: DigitalTwinTraceSource::ScaleProof,
                    monotonic_ms: observed_at_ms.saturating_add(idx as u64),
                    source_hash: stable_hash(proof),
                    source_artifact_hashes: source_hashes(
                        source_artifact_hash,
                        proof,
                        &mut quality_flags,
                    ),
                    pane_hash: None,
                    agent_hash: None,
                    correlation_hash,
                    scheduler_sequence: None,
                    scale_history_len: None,
                    active_agent_count: None,
                    queue_utilization: None,
                    pending_items: None,
                    admission_action: None,
                    reason_codes: proof.dimensions.iter().map(stable_label).collect(),
                    raw_pressure_severity: Some(pressure_score),
                    effective_pressure_severity: Some(pressure_score),
                    fleet_pressure: None,
                    memory_tier_pressure: None,
                    max_latency_over_budget_ratio: None,
                    memory_budget_bytes: proof.evidence.as_ref().map(|e| e.memory_bytes),
                    memory_actual_bytes: None,
                    resident_over_budget_bytes: None,
                    reclaimable_bytes: None,
                    proof_status: Some(proof.status),
                    evidence_source: Some(proof.evidence_source),
                    hardware_evidence_complete: Some(evidence_complete),
                    pressure_score,
                    quality_flags,
                }
            })
            .collect()
    }

    /// Adapt a resource-pressure chaos verdict into a digital-twin trace step.
    #[must_use]
    pub fn from_resource_pressure_verdict(
        step_id: &str,
        observed_at_ms: u64,
        verdict: &ResourcePressureChaosVerdict,
        source_artifact_hash: Option<&str>,
    ) -> DigitalTwinTraceStep {
        let mut quality_flags = Vec::new();
        if observed_at_ms == 0 {
            quality_flags.push(DigitalTwinTraceQualityFlag::StaleTimestamp);
        }
        if verdict.hardware_evidence.is_none() {
            quality_flags.push(DigitalTwinTraceQualityFlag::IncompleteHardwareEvidence);
        }
        let correlation_hash =
            redacted_hash("scenario", Some(&verdict.scenario_id), &mut quality_flags);
        let pressure_score = match verdict.status {
            ResourcePressureChaosStatus::Pass => 0,
            ResourcePressureChaosStatus::SkippedNotProven
            | ResourcePressureChaosStatus::ExpectedBlockedByInfra => 1,
            ResourcePressureChaosStatus::Fail => 3,
        };
        let queue_utilization = verdict
            .admission_observation
            .as_ref()
            .and_then(|observation| {
                finite_f64(
                    Some(f64::from(observation.queue.queue_utilization_basis_points) / 10_000.0),
                    &mut quality_flags,
                )
            });
        let mut reason_codes = vec![stable_label(&verdict.pressure_class)];
        if let Some(observation) = verdict.admission_observation.as_ref() {
            reason_codes.extend(observation.admission_reason_codes.iter().map(stable_label));
            sort_dedup(&mut reason_codes);
        }

        DigitalTwinTraceStep {
            step_id: step_id.to_string(),
            source: DigitalTwinTraceSource::ResourcePressureChaos,
            monotonic_ms: observed_at_ms,
            source_hash: stable_hash(verdict),
            source_artifact_hashes: source_hashes(
                source_artifact_hash,
                verdict,
                &mut quality_flags,
            ),
            pane_hash: None,
            agent_hash: None,
            correlation_hash,
            scheduler_sequence: None,
            scale_history_len: None,
            active_agent_count: None,
            queue_utilization,
            pending_items: verdict
                .admission_observation
                .as_ref()
                .map(|observation| u64::from(observation.queue.pending_items)),
            admission_action: verdict
                .admission_observation
                .as_ref()
                .map(|observation| stable_label(&observation.admission_action)),
            reason_codes,
            raw_pressure_severity: verdict
                .admission_observation
                .as_ref()
                .map_or(Some(pressure_score), |observation| {
                    Some(observation.resource_cockpit.raw_pressure_severity)
                }),
            effective_pressure_severity: verdict
                .admission_observation
                .as_ref()
                .map_or(Some(pressure_score), |observation| {
                    Some(observation.resource_cockpit.effective_pressure_severity)
                }),
            fleet_pressure: None,
            memory_tier_pressure: verdict.memory_observation.as_ref().map(|observation| {
                stable_label(&observation.resource_cockpit.compound_pressure_tier)
            }),
            max_latency_over_budget_ratio: None,
            memory_budget_bytes: verdict
                .memory_observation
                .as_ref()
                .map(|observation| observation.tier_budget.totals.budget_bytes),
            memory_actual_bytes: verdict
                .memory_observation
                .as_ref()
                .map(|observation| observation.tier_budget.totals.actual_bytes),
            resident_over_budget_bytes: verdict
                .memory_observation
                .as_ref()
                .map(|observation| observation.tier_budget.totals.resident_over_budget_bytes),
            reclaimable_bytes: verdict
                .memory_observation
                .as_ref()
                .map(|observation| observation.tier_budget.totals.reclaimable_bytes),
            proof_status: None,
            evidence_source: None,
            hardware_evidence_complete: Some(verdict.hardware_evidence.is_some()),
            pressure_score,
            quality_flags,
        }
    }
}

fn compact_phase_changes_and_extrema(
    steps: Vec<DigitalTwinTraceStep>,
    max_steps: usize,
) -> Vec<DigitalTwinTraceStep> {
    if steps.len() <= max_steps {
        return steps;
    }
    if max_steps == 0 {
        return Vec::new();
    }
    if max_steps == 1 {
        return steps.into_iter().take(1).collect();
    }

    let mut keep = vec![0, steps.len() - 1];
    if max_steps == keep.len() {
        keep.sort_unstable();
        return keep.into_iter().map(|idx| steps[idx].clone()).collect();
    }
    let mut phase_changes = Vec::new();
    for idx in 1..steps.len() {
        let previous = &steps[idx - 1];
        let current = &steps[idx];
        if current.source != previous.source || current.pressure_score != previous.pressure_score {
            phase_changes.push(idx);
        }
    }

    phase_changes.sort_by(|left, right| {
        steps[*right]
            .pressure_score
            .cmp(&steps[*left].pressure_score)
            .then_with(|| steps[*left].monotonic_ms.cmp(&steps[*right].monotonic_ms))
    });
    for idx in phase_changes {
        if keep.len() >= max_steps {
            break;
        }
        if !keep.contains(&idx) {
            keep.push(idx);
            sort_dedup(&mut keep);
        }
        if keep.len() >= max_steps {
            break;
        }
    }

    let mut ranked = (0..steps.len()).collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        steps[*right]
            .pressure_score
            .cmp(&steps[*left].pressure_score)
            .then_with(|| steps[*left].monotonic_ms.cmp(&steps[*right].monotonic_ms))
    });
    for idx in ranked {
        if keep.len() >= max_steps {
            break;
        }
        if !keep.contains(&idx) {
            keep.push(idx);
            sort_dedup(&mut keep);
        }
        if keep.len() >= max_steps {
            break;
        }
    }
    keep.sort_unstable();
    keep.into_iter().map(|idx| steps[idx].clone()).collect()
}

fn clamp_monotonic_timestamps(steps: &mut [DigitalTwinTraceStep]) {
    let mut last = 0_u64;
    for step in steps {
        if step.monotonic_ms < last {
            step.monotonic_ms = last;
            step.quality_flags
                .push(DigitalTwinTraceQualityFlag::NonMonotonicTimestampAdjusted);
            sort_dedup(&mut step.quality_flags);
        }
        last = step.monotonic_ms;
    }
}

fn source_hashes<T: Serialize>(
    source_artifact_hash: Option<&str>,
    source: &T,
    quality_flags: &mut Vec<DigitalTwinTraceQualityFlag>,
) -> Vec<String> {
    match source_artifact_hash {
        Some(hash) if !hash.trim().is_empty() => vec![hash.trim().to_string()],
        _ => {
            quality_flags.push(DigitalTwinTraceQualityFlag::DerivedSourceHash);
            vec![stable_hash(source)]
        }
    }
}

fn redacted_hash(
    namespace: &str,
    raw: Option<&str>,
    quality_flags: &mut Vec<DigitalTwinTraceQualityFlag>,
) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    quality_flags.push(DigitalTwinTraceQualityFlag::RedactedIdentity);
    Some(stable_hash(&(namespace, raw)))
}

fn finite_f64(
    value: Option<f64>,
    quality_flags: &mut Vec<DigitalTwinTraceQualityFlag>,
) -> Option<f64> {
    match value {
        Some(value) if value.is_finite() => Some(value),
        Some(_) => {
            quality_flags.push(DigitalTwinTraceQualityFlag::NonFiniteTelemetry);
            None
        }
        None => None,
    }
}

fn stable_label<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(label)) => label,
        Ok(value) => value.to_string(),
        Err(_) => "unknown".to_string(),
    }
}

fn option_label<T: Serialize>(value: Option<&T>) -> String {
    value.map(stable_label).unwrap_or_else(|| "-".to_string())
}

fn label_list<T: Serialize>(values: &[T]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values
            .iter()
            .map(stable_label)
            .collect::<Vec<_>>()
            .join("|")
    }
}

fn stable_hash<T: Serialize>(value: &T) -> String {
    let bytes =
        serde_json::to_vec(value).expect("digital twin trace source serializes to stable JSON");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn sort_dedup<T: Ord>(values: &mut Vec<T>) {
    values.sort();
    values.dedup();
}

// ============================================================================
// ScenarioMatrixRunner — executes the matrix
// ============================================================================

/// Callback for generating decisions from an artifact+override pair.
/// In production, this invokes the replay kernel. In tests, it's mocked.
pub type DecisionGenerator =
    Box<dyn Fn(&str, Option<&str>) -> Result<Vec<String>, String> + Send + Sync>;

type SharedDecisionGenerator =
    Arc<dyn Fn(&str, Option<&str>) -> Result<Vec<String>, String> + Send + Sync>;

/// Executes a scenario matrix, collecting decision diffs.
pub struct ScenarioMatrixRunner {
    config: MatrixConfig,
    generator: SharedDecisionGenerator,
}

impl ScenarioMatrixRunner {
    /// Create a runner with a decision generator callback.
    pub fn new(config: MatrixConfig, generator: DecisionGenerator) -> Self {
        Self {
            config,
            generator: generator.into(),
        }
    }

    /// Execute the matrix. Returns results and emits progress events to the callback.
    pub fn run<F>(&self, mut on_progress: F) -> MatrixResult
    where
        F: FnMut(ProgressEvent),
    {
        let pairs = self.config.scenario_pairs();
        let total = pairs.len();
        if total == 0 {
            return MatrixResult::from_results(Vec::new());
        }

        let mut results = vec![None; total];
        let fail_fast = self.config.config.fail_fast;
        let concurrency = if fail_fast {
            1
        } else {
            self.config.config.concurrency.max(1).min(total)
        };
        let timeout = Duration::from_millis(self.config.config.timeout_per_scenario_ms);
        let (tx, rx) = mpsc::channel();
        let mut next = 0usize;
        let mut active = 0usize;

        while next < total && active < concurrency {
            Self::spawn_scenario(
                next,
                pairs[next].0.clone(),
                pairs[next].1.clone(),
                Arc::clone(&self.generator),
                timeout,
                tx.clone(),
            );
            let (art, ovr) = &pairs[next];
            let override_label = ovr.as_ref().map(|o| o.label.clone()).unwrap_or_default();

            on_progress(ProgressEvent {
                completed: next,
                total,
                current_artifact: art.label.clone(),
                current_override: override_label,
            });

            next += 1;
            active += 1;
        }
        while active > 0 {
            let Ok((index, scenario)) = rx.recv() else {
                break;
            };
            active -= 1;
            let should_stop = fail_fast && (!scenario.is_ok() || scenario.has_divergence());
            results[index] = Some(scenario);

            if should_stop {
                break;
            }

            while next < total && active < concurrency {
                Self::spawn_scenario(
                    next,
                    pairs[next].0.clone(),
                    pairs[next].1.clone(),
                    Arc::clone(&self.generator),
                    timeout,
                    tx.clone(),
                );
                let (art, ovr) = &pairs[next];
                let override_label = ovr.as_ref().map(|o| o.label.clone()).unwrap_or_default();
                on_progress(ProgressEvent {
                    completed: next,
                    total,
                    current_artifact: art.label.clone(),
                    current_override: override_label,
                });

                next += 1;
                active += 1;
            }
        }

        MatrixResult::from_results(results.into_iter().flatten().collect())
    }

    fn spawn_scenario(
        index: usize,
        art: ArtifactEntry,
        ovr: Option<OverrideEntry>,
        generator: SharedDecisionGenerator,
        timeout: Duration,
        tx: mpsc::Sender<(usize, ScenarioResult)>,
    ) {
        drop(thread::spawn(move || {
            let result = Self::run_scenario_with_timeout(art, ovr, generator, timeout);
            let _ = tx.send((index, result));
        }));
    }

    fn run_scenario_with_timeout(
        art: ArtifactEntry,
        ovr: Option<OverrideEntry>,
        generator: SharedDecisionGenerator,
        timeout: Duration,
    ) -> ScenarioResult {
        let started = Instant::now();
        let artifact_label = art.label.clone();
        let artifact_path = art.path.clone();
        let override_label = ovr.as_ref().map(|o| o.label.clone()).unwrap_or_default();
        let override_path = ovr.as_ref().map(|o| o.path.clone());
        let (work_tx, work_rx) = mpsc::channel();

        drop(thread::spawn(move || {
            let baseline_result = generator(&artifact_path, None);
            let candidate_result = generator(&artifact_path, override_path.as_deref());
            let _ = work_tx.send((baseline_result, candidate_result));
        }));

        match work_rx.recv_timeout(timeout) {
            Ok((baseline_result, candidate_result)) => Self::scenario_from_generator_results(
                artifact_label,
                override_label,
                baseline_result,
                candidate_result,
                started,
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => ScenarioResult {
                artifact_label,
                override_label,
                baseline_decisions: Vec::new(),
                candidate_decisions: Vec::new(),
                diff: DiffSummary::default(),
                error: Some(format!(
                    "scenario timeout after {} ms",
                    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)
                )),
                duration_ms: elapsed_ms(started),
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => ScenarioResult {
                artifact_label,
                override_label,
                baseline_decisions: Vec::new(),
                candidate_decisions: Vec::new(),
                diff: DiffSummary::default(),
                error: Some("scenario worker disconnected".to_string()),
                duration_ms: elapsed_ms(started),
            },
        }
    }

    fn scenario_from_generator_results(
        artifact_label: String,
        override_label: String,
        baseline_result: Result<Vec<String>, String>,
        candidate_result: Result<Vec<String>, String>,
        started: Instant,
    ) -> ScenarioResult {
        match (baseline_result, candidate_result) {
            (Ok(baseline), Ok(candidate)) => {
                let diff = DiffSummary::compute(&baseline, &candidate);
                ScenarioResult {
                    artifact_label,
                    override_label,
                    baseline_decisions: baseline,
                    candidate_decisions: candidate,
                    diff,
                    error: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            (Err(e), _) => ScenarioResult {
                artifact_label,
                override_label,
                baseline_decisions: Vec::new(),
                candidate_decisions: Vec::new(),
                diff: DiffSummary::default(),
                error: Some(format!("baseline error: {e}")),
                duration_ms: elapsed_ms(started),
            },
            (_, Err(e)) => ScenarioResult {
                artifact_label,
                override_label,
                baseline_decisions: Vec::new(),
                candidate_decisions: Vec::new(),
                diff: DiffSummary::default(),
                error: Some(format!("candidate error: {e}")),
                duration_ms: elapsed_ms(started),
            },
        }
    }

    /// Get the matrix config.
    #[must_use]
    pub fn config(&self) -> &MatrixConfig {
        &self.config
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::fleet_memory_controller::{
        FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetPressureTier,
    };
    use frankenterm_core::resource_pressure_chaos::{sample_fail_verdict, sample_pass_verdict};
    use frankenterm_core::swarm_scheduler::{
        AdmissionAction, AdmissionDecisionCounters, AdmissionReasonCode, SchedulerConfig,
    };
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scheduler_snapshot(last_evaluation_ms: u64, sequence: u64) -> SchedulerSnapshot {
        SchedulerSnapshot {
            config: SchedulerConfig::default(),
            last_scale_up_ms: 0,
            last_scale_down_ms: 0,
            last_evaluation_ms,
            consecutive_scale_ops: 0,
            circuit_breaker_tripped_at: None,
            scale_history: Vec::new(),
            agent_first_seen: BTreeMap::new(),
            agent_completed: BTreeMap::new(),
            agent_failed: BTreeMap::new(),
            sequence,
        }
    }

    fn admission_summary(
        action: AdmissionAction,
        queue_utilization: Option<f64>,
        pending_items: Option<u32>,
        fleet_pressure: Option<FleetPressureTier>,
        memory_tier_pressure: Option<FleetPressureTier>,
        max_latency_over_budget_ratio: Option<f64>,
    ) -> ResourceAdmissionDecisionSummary {
        let counters = match action {
            AdmissionAction::Admit => AdmissionDecisionCounters {
                admitted: 1,
                ..AdmissionDecisionCounters::default()
            },
            AdmissionAction::Defer => AdmissionDecisionCounters {
                deferred: 1,
                ..AdmissionDecisionCounters::default()
            },
            AdmissionAction::Degrade => AdmissionDecisionCounters {
                degraded: 1,
                ..AdmissionDecisionCounters::default()
            },
            AdmissionAction::Shed => AdmissionDecisionCounters {
                shed: 1,
                ..AdmissionDecisionCounters::default()
            },
        };

        ResourceAdmissionDecisionSummary {
            action,
            reason_codes: vec![
                AdmissionReasonCode::QueueSaturated,
                AdmissionReasonCode::LatencyStageOverBudget,
            ],
            counters,
            raw_pressure_severity: 3,
            effective_pressure_severity: action.severity(),
            priority_protection_units: 1,
            queue_utilization,
            pending_items,
            fleet_pressure,
            memory_tier_pressure,
            max_latency_over_budget_ratio,
            herd_wave_pressure: None,
            herd_wave_recommended_stagger_ms: None,
            herd_wave_cohort_max_stagger_ms: None,
        }
    }

    fn minimal_trace_step(
        step_id: &str,
        source: DigitalTwinTraceSource,
        monotonic_ms: u64,
        pressure_score: u8,
    ) -> DigitalTwinTraceStep {
        DigitalTwinTraceStep {
            step_id: step_id.to_string(),
            source,
            monotonic_ms,
            source_hash: format!("source-{step_id}"),
            source_artifact_hashes: vec![format!("artifact-{step_id}")],
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
            raw_pressure_severity: Some(pressure_score),
            effective_pressure_severity: Some(pressure_score),
            fleet_pressure: None,
            memory_tier_pressure: None,
            max_latency_over_budget_ratio: None,
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

    fn golden_admission_step(
        step_id: &str,
        observed_at_ms: u64,
        action: AdmissionAction,
        queue_utilization: Option<f64>,
        pending_items: Option<u32>,
        fleet_pressure: Option<FleetPressureTier>,
        memory_tier_pressure: Option<FleetPressureTier>,
        max_latency_over_budget_ratio: Option<f64>,
        reason_codes: Vec<AdmissionReasonCode>,
        source_artifact_hash: Option<&str>,
    ) -> DigitalTwinTraceStep {
        let mut summary = admission_summary(
            action,
            queue_utilization,
            pending_items,
            fleet_pressure,
            memory_tier_pressure,
            max_latency_over_budget_ratio,
        );
        summary.reason_codes = reason_codes;
        summary.raw_pressure_severity = action.severity();
        summary.effective_pressure_severity = action.severity();

        DigitalTwinTraceAdapter::from_resource_admission_decision(
            step_id,
            observed_at_ms,
            &summary,
            None,
            None,
            source_artifact_hash,
        )
    }

    fn digital_twin_golden_trace() -> DigitalTwinTrace {
        DigitalTwinTraceAdapter::build(
            4_000,
            vec![
                golden_admission_step(
                    "healthy",
                    10,
                    AdmissionAction::Admit,
                    Some(0.20),
                    Some(2),
                    Some(FleetPressureTier::Normal),
                    Some(FleetPressureTier::Normal),
                    Some(0.50),
                    vec![AdmissionReasonCode::Healthy],
                    Some("golden-healthy"),
                ),
                golden_admission_step(
                    "pressured",
                    20,
                    AdmissionAction::Defer,
                    Some(0.84),
                    Some(48),
                    Some(FleetPressureTier::Elevated),
                    Some(FleetPressureTier::Elevated),
                    Some(1.10),
                    vec![
                        AdmissionReasonCode::QueueElevated,
                        AdmissionReasonCode::FleetPressure,
                    ],
                    Some("golden-pressured"),
                ),
                golden_admission_step(
                    "degraded",
                    30,
                    AdmissionAction::Degrade,
                    Some(0.94),
                    Some(96),
                    Some(FleetPressureTier::Critical),
                    Some(FleetPressureTier::Critical),
                    Some(1.75),
                    vec![
                        AdmissionReasonCode::QueueSaturated,
                        AdmissionReasonCode::MemoryTierPressure,
                        AdmissionReasonCode::LatencyStageOverBudget,
                    ],
                    Some("golden-degraded"),
                ),
                golden_admission_step(
                    "missing_telemetry",
                    40,
                    AdmissionAction::Defer,
                    None,
                    None,
                    None,
                    None,
                    None,
                    vec![
                        AdmissionReasonCode::MissingQueueTelemetry,
                        AdmissionReasonCode::MissingFleetTelemetry,
                        AdmissionReasonCode::MissingMemoryTierTelemetry,
                        AdmissionReasonCode::MissingLatencyTelemetry,
                    ],
                    None,
                ),
            ],
        )
    }

    fn scale_lab_fixture_path(file_name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/scale-lab")
            .join(file_name)
    }

    #[test]
    fn digital_twin_trace_orders_hashes_and_redacts_identities() {
        let admission = DigitalTwinTraceAdapter::from_resource_admission_decision(
            "admission",
            200,
            &admission_summary(
                AdmissionAction::Degrade,
                Some(0.91),
                Some(42),
                Some(FleetPressureTier::Critical),
                Some(FleetPressureTier::Elevated),
                Some(1.7),
            ),
            Some("pane-raw-7"),
            Some("agent-raw-alpha"),
            None,
        );
        let scheduler = DigitalTwinTraceAdapter::from_scheduler_snapshot(
            "scheduler",
            &scheduler_snapshot(100, 9),
            None,
        );

        let trace =
            DigitalTwinTraceAdapter::build(1_000, vec![admission.clone(), scheduler.clone()]);
        let repeated = DigitalTwinTraceAdapter::build(1_000, vec![admission, scheduler]);

        assert_eq!(trace.schema_version, DIGITAL_TWIN_TRACE_SCHEMA_VERSION);
        assert_eq!(
            trace.steps[0].source,
            DigitalTwinTraceSource::SchedulerSnapshot
        );
        assert_eq!(
            trace.steps[1].source,
            DigitalTwinTraceSource::ResourceAdmission
        );
        assert_eq!(trace.steps[1].admission_action.as_deref(), Some("degrade"));
        assert_eq!(
            DigitalTwinTraceSource::ResourceAdmission.as_str(),
            "resource_admission"
        );
        assert_eq!(trace.trace_hash, repeated.trace_hash);
        assert!(
            trace
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::DerivedSourceHash)
        );
        assert!(
            trace
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::RedactedIdentity)
        );

        let json = trace.to_stable_json();
        assert!(json.contains("\"schema_version\": \"ft.digital_twin_trace.v1\""));
        assert!(!json.contains("pane-raw-7"));
        assert!(!json.contains("agent-raw-alpha"));
    }

    #[test]
    fn digital_twin_trace_flags_missing_telemetry_without_derived_hash_when_artifact_hash_exists() {
        let step = DigitalTwinTraceAdapter::from_resource_admission_decision(
            "missing",
            0,
            &admission_summary(AdmissionAction::Defer, None, None, None, None, None),
            None,
            None,
            Some("artifact-hash-explicit"),
        );
        let trace = DigitalTwinTraceAdapter::build(2_000, vec![step]);

        for flag in [
            DigitalTwinTraceQualityFlag::StaleTimestamp,
            DigitalTwinTraceQualityFlag::MissingQueueTelemetry,
            DigitalTwinTraceQualityFlag::MissingFleetTelemetry,
            DigitalTwinTraceQualityFlag::MissingMemoryTierTelemetry,
            DigitalTwinTraceQualityFlag::MissingLatencyTelemetry,
        ] {
            assert!(trace.quality_flags.contains(&flag), "missing flag {flag:?}");
        }
        assert!(
            !trace
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::DerivedSourceHash)
        );
        assert!(
            trace
                .source_artifact_hashes
                .iter()
                .any(|hash| hash == "artifact-hash-explicit")
        );
    }

    #[test]
    fn digital_twin_trace_compaction_preserves_boundaries_and_pressure_extrema() {
        let trace = DigitalTwinTraceAdapter::build_with_compaction(
            3_000,
            vec![
                minimal_trace_step("first", DigitalTwinTraceSource::SchedulerSnapshot, 10, 0),
                minimal_trace_step("elevated", DigitalTwinTraceSource::SchedulerSnapshot, 20, 1),
                minimal_trace_step("critical", DigitalTwinTraceSource::SchedulerSnapshot, 30, 3),
                minimal_trace_step(
                    "admission",
                    DigitalTwinTraceSource::ResourceAdmission,
                    40,
                    2,
                ),
                minimal_trace_step("last", DigitalTwinTraceSource::ResourceAdmission, 50, 0),
            ],
            Some(3),
        );

        assert_eq!(trace.steps.len(), 3);
        assert_eq!(trace.steps.first().unwrap().step_id, "first");
        assert_eq!(trace.steps.last().unwrap().step_id, "last");
        assert!(trace.steps.iter().any(|step| step.step_id == "critical"));
        assert!(
            trace
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::CompactedSamples)
        );
        assert!(trace.steps.iter().all(|step| {
            step.quality_flags
                .contains(&DigitalTwinTraceQualityFlag::CompactedSamples)
        }));
    }

    #[test]
    fn digital_twin_trace_clamps_regressed_timestamps() {
        let mut steps = vec![
            minimal_trace_step("a", DigitalTwinTraceSource::SchedulerSnapshot, 20, 0),
            minimal_trace_step("b", DigitalTwinTraceSource::SchedulerSnapshot, 10, 1),
        ];
        clamp_monotonic_timestamps(&mut steps);

        assert_eq!(steps[1].monotonic_ms, 20);
        assert!(
            steps[1]
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::NonMonotonicTimestampAdjusted)
        );
    }

    #[test]
    fn digital_twin_trace_adapts_memory_and_scale_proof_sources() {
        let memory = FleetMemoryTierBudgetSnapshot::from_tiers([
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::HotResident, 100, 140)
                .with_reclaimable_bytes(25),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::ColdDisk, 1_000, 250),
        ]);
        let memory_step = DigitalTwinTraceAdapter::from_memory_budget_snapshot(
            "memory",
            42,
            &memory,
            Some("memory-artifact"),
        );
        assert_eq!(
            memory_step.memory_budget_bytes,
            Some(memory.totals.budget_bytes)
        );
        assert_eq!(
            memory_step.memory_actual_bytes,
            Some(memory.totals.actual_bytes)
        );
        assert!(memory_step.pressure_score > 0);

        let proof = ScaleScenarioProof {
            scenario_id: "synthetic_10k_policy_audit".to_string(),
            class: ScaleScenarioClass::Synthetic,
            dimensions: vec![ProofDimension::Hardware],
            status: ProofStatus::SkippedNotProven,
            evidence_source: ScaleProofEvidenceSource::Synthetic,
            evidence: None,
            note: "SKIPPED_NOT_PROVEN: waiting for live high-core worker".to_string(),
        };
        let matrix =
            ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), vec![proof]);
        let proof_steps = DigitalTwinTraceAdapter::from_scale_proof_matrix(50, &matrix, None);
        let trace = DigitalTwinTraceAdapter::build(5_000, proof_steps);

        assert_eq!(trace.steps.len(), 1);
        assert_eq!(
            trace.steps[0].proof_status,
            Some(ProofStatus::SkippedNotProven)
        );
        assert_eq!(
            trace.steps[0].evidence_source,
            Some(ScaleProofEvidenceSource::Synthetic)
        );
        assert_eq!(trace.steps[0].hardware_evidence_complete, Some(false));
        for flag in [
            DigitalTwinTraceQualityFlag::SimulatedEvidence,
            DigitalTwinTraceQualityFlag::IncompleteHardwareEvidence,
            DigitalTwinTraceQualityFlag::RedactedIdentity,
            DigitalTwinTraceQualityFlag::DerivedSourceHash,
        ] {
            assert!(trace.quality_flags.contains(&flag), "missing flag {flag:?}");
        }
    }

    #[test]
    fn digital_twin_trace_adapts_resource_pressure_verdict_statuses() {
        let pass = DigitalTwinTraceAdapter::from_resource_pressure_verdict(
            "pass",
            60,
            &sample_pass_verdict(),
            Some("pass-artifact"),
        );
        assert_eq!(pass.pressure_score, 0);
        assert_eq!(pass.hardware_evidence_complete, Some(true));
        assert!(pass.queue_utilization.is_some());
        assert!(
            pass.reason_codes
                .iter()
                .any(|reason| reason == "queue_saturated")
        );

        let fail = DigitalTwinTraceAdapter::from_resource_pressure_verdict(
            "fail",
            70,
            &sample_fail_verdict(),
            None,
        );
        let trace = DigitalTwinTraceAdapter::build(6_000, vec![fail]);
        assert_eq!(trace.steps[0].pressure_score, 3);
        assert_eq!(trace.steps[0].hardware_evidence_complete, Some(false));
        assert!(
            trace
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::IncompleteHardwareEvidence)
        );
        assert!(
            trace
                .quality_flags
                .contains(&DigitalTwinTraceQualityFlag::DerivedSourceHash)
        );
    }

    #[test]
    fn digital_twin_trace_golden_json_and_toon_fixtures_match() {
        let trace = digital_twin_golden_trace();
        let actual_json = trace.to_stable_json() + "\n";
        let actual_toon = trace.to_toon();
        let json_path = scale_lab_fixture_path("digital-twin-trace-summary.v1.json");
        let toon_path = scale_lab_fixture_path("digital-twin-trace-summary.v1.toon");

        if std::env::var_os("UPDATE_GOLDENS").is_some() {
            fs::write(&json_path, &actual_json).expect("digital twin JSON fixture can be updated");
            fs::write(&toon_path, &actual_toon).expect("digital twin TOON fixture can be updated");
        }

        let expected_json =
            fs::read_to_string(&json_path).expect("digital twin JSON fixture is checked in");
        let expected_toon =
            fs::read_to_string(&toon_path).expect("digital twin TOON fixture is checked in");

        assert_eq!(expected_json, actual_json);
        assert_eq!(expected_toon, actual_toon);
    }

    fn sample_matrix_toml() -> &'static str {
        r#"
[[artifacts]]
path = "trace_a.ftreplay"
label = "incident_a"

[[artifacts]]
path = "trace_b.ftreplay"
label = "incident_b"

[[overrides]]
path = "strict_rules.ftoverride"
label = "strict"

[[overrides]]
path = "relaxed_rules.ftoverride"
label = "relaxed"

[config]
concurrency = 4
timeout_per_scenario_ms = 60000
fail_fast = false
"#
    }

    fn mock_generator(decisions: Vec<String>) -> DecisionGenerator {
        Box::new(move |_art, ovr| {
            if ovr.is_some() {
                // Candidate: add one extra decision.
                let mut d = decisions.clone();
                d.push("extra_decision".into());
                Ok(d)
            } else {
                Ok(decisions.clone())
            }
        })
    }

    fn identical_generator() -> DecisionGenerator {
        Box::new(|_art, _ovr| Ok(vec!["d1".into(), "d2".into(), "d3".into()]))
    }

    fn error_generator() -> DecisionGenerator {
        Box::new(|_art, _ovr| Err("simulated failure".into()))
    }

    fn matrix_for_runner_budget_tests(
        scenario_count: usize,
        concurrency: usize,
        timeout_per_scenario_ms: u64,
        fail_fast: bool,
    ) -> MatrixConfig {
        MatrixConfig {
            artifacts: (0..scenario_count)
                .map(|idx| ArtifactEntry {
                    path: format!("trace_{idx}.ftreplay"),
                    label: format!("trace_{idx}"),
                })
                .collect(),
            overrides: Vec::new(),
            config: RunnerConfig {
                concurrency,
                timeout_per_scenario_ms,
                fail_fast,
            },
        }
    }

    fn record_max_observed(max_active: &AtomicUsize, active: usize) {
        let mut observed = max_active.load(Ordering::SeqCst);
        while active > observed {
            match max_active.compare_exchange(observed, active, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(next) => observed = next,
            }
        }
    }

    // ── MatrixConfig parsing ────────────────────────────────────────────

    #[test]
    fn parse_matrix_config() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        assert_eq!(config.artifacts.len(), 2);
        assert_eq!(config.overrides.len(), 2);
        assert_eq!(config.config.concurrency, 4);
        assert_eq!(config.config.timeout_per_scenario_ms, 60_000);
        assert!(!config.config.fail_fast);
    }

    #[test]
    fn scenario_count_with_overrides() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        assert_eq!(config.scenario_count(), 4); // 2 x 2
    }

    #[test]
    fn scenario_count_no_overrides() {
        let toml = r#"
[[artifacts]]
path = "a.ftreplay"
label = "a"

[[artifacts]]
path = "b.ftreplay"
label = "b"
"#;
        let config = MatrixConfig::from_toml(toml).unwrap();
        assert_eq!(config.scenario_count(), 2);
    }

    #[test]
    fn scenario_count_empty() {
        let toml = "[config]\nconcurrency = 1\n";
        let config = MatrixConfig::from_toml(toml).unwrap();
        assert_eq!(config.scenario_count(), 0);
    }

    #[test]
    fn scenario_pairs_generated() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        let pairs = config.scenario_pairs();
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0].0.label, "incident_a");
        assert_eq!(pairs[0].1.as_ref().unwrap().label, "strict");
    }

    // ── DiffSummary ─────────────────────────────────────────────────────

    #[test]
    fn diff_identical() {
        let base = vec!["d1".into(), "d2".into(), "d3".into()];
        let cand = vec!["d1".into(), "d2".into(), "d3".into()];
        let diff = DiffSummary::compute(&base, &cand);
        assert!(diff.is_identical());
        assert_eq!(diff.unchanged, 3);
        assert_eq!(diff.divergence_count(), 0);
    }

    #[test]
    fn diff_added() {
        let base = vec!["d1".into(), "d2".into()];
        let cand = vec!["d1".into(), "d2".into(), "d3".into()];
        let diff = DiffSummary::compute(&base, &cand);
        assert!(!diff.is_identical());
        assert_eq!(diff.added, 1);
        assert_eq!(diff.unchanged, 2);
    }

    #[test]
    fn diff_removed() {
        let base = vec!["d1".into(), "d2".into(), "d3".into()];
        let cand = vec!["d1".into(), "d2".into()];
        let diff = DiffSummary::compute(&base, &cand);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.unchanged, 2);
    }

    #[test]
    fn diff_modified() {
        let base = vec!["d1".into(), "d2".into()];
        let cand = vec!["d1".into(), "d2_changed".into()];
        let diff = DiffSummary::compute(&base, &cand);
        assert_eq!(diff.modified, 1);
        assert_eq!(diff.unchanged, 1);
    }

    #[test]
    fn diff_empty_sequences() {
        let diff = DiffSummary::compute(&[], &[]);
        assert!(diff.is_identical());
        assert_eq!(diff.total_decisions, 0);
    }

    // ── ScenarioResult ──────────────────────────────────────────────────

    #[test]
    fn scenario_result_ok() {
        let result = ScenarioResult {
            artifact_label: "a".into(),
            override_label: "o".into(),
            baseline_decisions: vec!["d1".into()],
            candidate_decisions: vec!["d1".into()],
            diff: DiffSummary::compute(&["d1".into()], &["d1".into()]),
            error: None,
            duration_ms: 100,
        };
        assert!(result.is_ok());
        assert!(!result.has_divergence());
    }

    #[test]
    fn scenario_result_with_error() {
        let result = ScenarioResult {
            artifact_label: "a".into(),
            override_label: "o".into(),
            baseline_decisions: vec![],
            candidate_decisions: vec![],
            diff: DiffSummary::default(),
            error: Some("fail".into()),
            duration_ms: 0,
        };
        assert!(!result.is_ok());
    }

    // ── MatrixResult ────────────────────────────────────────────────────

    #[test]
    fn matrix_result_all_pass() {
        let scenarios = vec![ScenarioResult {
            artifact_label: "a".into(),
            override_label: String::new(),
            baseline_decisions: vec!["d1".into()],
            candidate_decisions: vec!["d1".into()],
            diff: DiffSummary {
                total_decisions: 1,
                unchanged: 1,
                added: 0,
                removed: 0,
                modified: 0,
            },
            error: None,
            duration_ms: 50,
        }];
        let result = MatrixResult::from_results(scenarios);
        assert!(result.all_passed());
        assert_eq!(result.pass_count, 1);
        assert_eq!(result.divergence_count, 0);
        assert_eq!(result.error_count, 0);
    }

    #[test]
    fn matrix_result_with_divergence() {
        let scenarios = vec![
            ScenarioResult {
                artifact_label: "a".into(),
                override_label: "o1".into(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d1".into(), "d2".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d1".into(), "d2".into()]),
                error: None,
                duration_ms: 50,
            },
            ScenarioResult {
                artifact_label: "a".into(),
                override_label: "o2".into(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d1".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d1".into()]),
                error: None,
                duration_ms: 30,
            },
        ];
        let result = MatrixResult::from_results(scenarios);
        assert!(!result.all_passed());
        assert_eq!(result.pass_count, 1);
        assert_eq!(result.divergence_count, 1);
    }

    #[test]
    fn matrix_result_json_roundtrip() {
        let scenarios = vec![ScenarioResult {
            artifact_label: "test".into(),
            override_label: String::new(),
            baseline_decisions: vec!["d1".into()],
            candidate_decisions: vec!["d1".into()],
            diff: DiffSummary::default(),
            error: None,
            duration_ms: 10,
        }];
        let result = MatrixResult::from_results(scenarios);
        let json = result.to_json();
        let restored: MatrixResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.total_scenarios, result.total_scenarios);
    }

    // ── ScenarioMatrixRunner ────────────────────────────────────────────

    #[test]
    fn runner_executes_all() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        let dg = identical_generator();
        let runner = ScenarioMatrixRunner::new(config, dg);
        let mut progress_events = Vec::new();
        let result = runner.run(|p| progress_events.push(p));
        assert_eq!(result.total_scenarios, 4);
        assert_eq!(result.pass_count, 4);
        assert!(result.all_passed());
        assert!(!progress_events.is_empty());
    }

    #[test]
    fn runner_detects_divergence() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        let dg = mock_generator(vec!["d1".into(), "d2".into()]);
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        // All scenarios have overrides → candidate adds extra → all diverge.
        assert_eq!(result.divergence_count, 4);
        assert!(!result.all_passed());
    }

    #[test]
    fn runner_fail_fast() {
        let toml = r#"
[[artifacts]]
path = "a.ftreplay"
label = "a"

[[artifacts]]
path = "b.ftreplay"
label = "b"

[[overrides]]
path = "o.ftoverride"
label = "o"

[config]
fail_fast = true
"#;
        let config = MatrixConfig::from_toml(toml).unwrap();
        let dg = mock_generator(vec!["d1".into()]);
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        // fail_fast should stop after first divergence.
        assert_eq!(result.total_scenarios, 1);
    }

    #[test]
    fn runner_handles_errors() {
        let toml = r#"
[[artifacts]]
path = "a.ftreplay"
label = "a"
"#;
        let config = MatrixConfig::from_toml(toml).unwrap();
        let dg = error_generator();
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        assert_eq!(result.error_count, 1);
        assert!(!result.all_passed());
    }

    #[test]
    fn runner_fail_fast_stops_on_generator_error() {
        let config = matrix_for_runner_budget_tests(3, 3, 1_000, true);
        let dg = error_generator();
        let runner = ScenarioMatrixRunner::new(config, dg);

        let result = runner.run(|_| {});

        assert_eq!(result.total_scenarios, 1);
        assert_eq!(result.error_count, 1);
    }

    #[test]
    fn runner_records_real_duration_for_successful_scenarios() {
        let config = matrix_for_runner_budget_tests(1, 1, 1_000, false);
        let dg = Box::new(|_art: &str, _ovr: Option<&str>| {
            thread::sleep(Duration::from_millis(5));
            Ok(vec!["decision".into()])
        });
        let runner = ScenarioMatrixRunner::new(config, dg);

        let result = runner.run(|_| {});

        assert_eq!(result.total_scenarios, 1);
        assert!(result.scenarios[0].duration_ms > 0);
        assert_eq!(result.total_duration_ms, result.scenarios[0].duration_ms);
    }

    #[test]
    fn runner_times_out_slow_scenario_and_fail_fast_stops_on_error() {
        let config = matrix_for_runner_budget_tests(2, 4, 20, true);
        let dg = Box::new(|_art: &str, _ovr: Option<&str>| {
            thread::sleep(Duration::from_millis(250));
            Ok(vec!["late".into()])
        });
        let runner = ScenarioMatrixRunner::new(config, dg);
        let started = Instant::now();

        let result = runner.run(|_| {});

        assert_eq!(result.total_scenarios, 1);
        assert_eq!(result.error_count, 1);
        assert!(
            result.scenarios[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("scenario timeout"))
        );
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "runner did not return near the configured timeout"
        );
    }

    #[test]
    fn runner_enforces_concurrency_cap() {
        let config = matrix_for_runner_budget_tests(6, 2, 1_000, false);
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let active_for_generator = Arc::clone(&active);
        let max_for_generator = Arc::clone(&max_active);
        let dg = Box::new(move |_art: &str, _ovr: Option<&str>| {
            let now_active = active_for_generator.fetch_add(1, Ordering::SeqCst) + 1;
            record_max_observed(&max_for_generator, now_active);
            thread::sleep(Duration::from_millis(25));
            active_for_generator.fetch_sub(1, Ordering::SeqCst);
            Ok(vec!["decision".into()])
        });
        let runner = ScenarioMatrixRunner::new(config, dg);

        let result = runner.run(|_| {});

        assert_eq!(result.total_scenarios, 6);
        assert!(result.all_passed());
        assert!(max_active.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn runner_no_overrides_baseline_only() {
        let toml = r#"
[[artifacts]]
path = "a.ftreplay"
label = "a"

[[artifacts]]
path = "b.ftreplay"
label = "b"
"#;
        let config = MatrixConfig::from_toml(toml).unwrap();
        let dg = identical_generator();
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        assert_eq!(result.total_scenarios, 2);
        assert!(result.all_passed());
    }

    #[test]
    fn runner_empty_matrix() {
        let toml = "[config]\nconcurrency = 1\n";
        let config = MatrixConfig::from_toml(toml).unwrap();
        let dg = identical_generator();
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        assert_eq!(result.total_scenarios, 0);
        assert!(result.all_passed());
    }

    #[test]
    fn runner_progress_events() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        let dg = identical_generator();
        let runner = ScenarioMatrixRunner::new(config, dg);
        let mut events = Vec::new();
        runner.run(|p| events.push(p));
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].completed, 0);
        assert_eq!(events[0].total, 4);
        assert_eq!(events[3].completed, 3);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        #[test]
        fn runner_property_never_exceeds_configured_concurrency(
            scenario_count in 1usize..8,
            concurrency in 1usize..5,
        ) {
            let config = matrix_for_runner_budget_tests(scenario_count, concurrency, 1_000, false);
            let active = Arc::new(AtomicUsize::new(0));
            let max_active = Arc::new(AtomicUsize::new(0));
            let active_for_generator = Arc::clone(&active);
            let max_for_generator = Arc::clone(&max_active);
            let dg = Box::new(move |_art: &str, _ovr: Option<&str>| {
                let now_active = active_for_generator.fetch_add(1, Ordering::SeqCst) + 1;
                record_max_observed(&max_for_generator, now_active);
                thread::sleep(Duration::from_millis(2));
                active_for_generator.fetch_sub(1, Ordering::SeqCst);
                Ok(vec!["decision".into()])
            });
            let runner = ScenarioMatrixRunner::new(config, dg);

            let result = runner.run(|_| {});

            prop_assert_eq!(result.total_scenarios, scenario_count);
            prop_assert!(result.all_passed());
            prop_assert!(
                max_active.load(Ordering::SeqCst) <= concurrency.min(scenario_count),
                "max active generator calls exceeded configured concurrency"
            );
        }
    }

    // ── Serde roundtrips ────────────────────────────────────────────────

    #[test]
    fn diff_summary_serde() {
        let diff = DiffSummary {
            total_decisions: 10,
            unchanged: 7,
            added: 1,
            removed: 1,
            modified: 1,
        };
        let json = serde_json::to_string(&diff).unwrap();
        let restored: DiffSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, diff);
    }

    #[test]
    fn scenario_result_serde() {
        let result = ScenarioResult {
            artifact_label: "a".into(),
            override_label: "o".into(),
            baseline_decisions: vec!["d1".into()],
            candidate_decisions: vec!["d1".into()],
            diff: DiffSummary::default(),
            error: None,
            duration_ms: 100,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: ScenarioResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.artifact_label, "a");
    }

    #[test]
    fn progress_event_serde() {
        let event = ProgressEvent {
            completed: 3,
            total: 10,
            current_artifact: "trace_a".into(),
            current_override: "strict".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ProgressEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.completed, 3);
    }

    #[test]
    fn matrix_config_serde() {
        let config = MatrixConfig::from_toml(sample_matrix_toml()).unwrap();
        let json = serde_json::to_string(&config).unwrap();
        let restored: MatrixConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.artifacts.len(), 2);
    }

    #[test]
    fn runner_config_defaults() {
        let config = RunnerConfig::default();
        assert_eq!(config.concurrency, 2);
        assert_eq!(config.timeout_per_scenario_ms, 300_000);
        assert!(!config.fail_fast);
    }

    // ── Massive-swarm proof matrix ─────────────────────────────────────

    fn complete_evidence(cpu_count: u32, memory_bytes: u64) -> ProofExecutionEvidence {
        ProofExecutionEvidence {
            cpu_count,
            memory_bytes,
            storage_bytes: 1_099_511_627_776,
            storage_class: "rch-ephemeral-nvme".to_string(),
            os: "linux-x86_64".to_string(),
            worker_id: "rch-scale-worker-01".to_string(),
            command: "cargo test -p frankenterm-core-replay scale_proof_matrix".to_string(),
            elapsed_ms: 42_000,
            git_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        }
    }

    #[test]
    fn massive_swarm_manifest_has_required_tiers_and_counters() {
        let manifest = ScaleScenarioManifest::massive_swarm_defaults();

        assert_eq!(manifest.scenarios.len(), 3);
        for minimum in [1_000, 5_000, 10_000] {
            assert!(
                manifest.scenarios.iter().any(|scenario| {
                    scenario.counters.logical_panes >= minimum
                        && scenario.counters.logical_agents >= minimum
                        && scenario.deterministic_seed > 0
                        && scenario.counters.churn_events > 0
                        && scenario.counters.alt_screen_flips > 0
                        && scenario.counters.event_storms > 0
                        && scenario.counters.output_burst_bytes > 0
                        && scenario.counters.storage_writes > 0
                        && scenario.counters.policy_denials > 0
                }),
                "missing required {minimum}-tier scale scenario"
            );
        }

        let ten_k = manifest.scenario("synthetic_10k_policy_audit").unwrap();
        assert_eq!(ten_k.class, ScaleScenarioClass::Synthetic);
        assert!(ten_k.dimensions.contains(&ProofDimension::Correctness));
        assert!(ten_k.dimensions.contains(&ProofDimension::Throughput));
        assert!(ten_k.dimensions.contains(&ProofDimension::Memory));
    }

    #[test]
    fn synthetic_proof_cannot_satisfy_live_hardware_claim() {
        let proof = ScaleScenarioProof {
            scenario_id: "synthetic_10k_policy_audit".to_string(),
            class: ScaleScenarioClass::Synthetic,
            dimensions: vec![ProofDimension::Hardware],
            status: ProofStatus::SkippedNotProven,
            evidence_source: ScaleProofEvidenceSource::Synthetic,
            evidence: Some(complete_evidence(128, 549_755_813_888)),
            note: "SKIPPED_NOT_PROVEN: synthetic proof does not prove live 64-core/256GiB capacity"
                .to_string(),
        };
        let matrix =
            ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), vec![proof]);

        assert!(!matrix.hardware_claims_proven(64, 274_877_906_944));
        assert_eq!(
            matrix.unproven_hardware_claims(64, 274_877_906_944),
            vec!["synthetic_10k_policy_audit".to_string()]
        );

        let summary = matrix.coverage_summary();
        assert_eq!(summary.synthetic_rows, 1);
        assert_eq!(summary.skipped_not_proven_rows, 1);
        assert_eq!(summary.hardware_passed, 0);
    }

    #[test]
    fn live_hardware_proof_satisfies_host_claim_when_evidence_is_complete() {
        let proof = ScaleScenarioProof {
            scenario_id: "live_10k_64core_256gib".to_string(),
            class: ScaleScenarioClass::LiveHardware,
            dimensions: vec![ProofDimension::Hardware],
            status: ProofStatus::Passed,
            evidence_source: ScaleProofEvidenceSource::LiveHardware,
            evidence: Some(complete_evidence(96, 549_755_813_888)),
            note: String::new(),
        };
        let matrix =
            ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), vec![proof]);

        assert!(matrix.hardware_claims_proven(64, 274_877_906_944));
        assert!(
            matrix
                .unproven_hardware_claims(64, 274_877_906_944)
                .is_empty()
        );

        let summary = matrix.coverage_summary();
        assert_eq!(summary.live_hardware_rows, 1);
        assert_eq!(summary.hardware_passed, 1);
    }

    #[test]
    fn proof_coverage_summary_distinguishes_dimensions() {
        let proofs = vec![
            ScaleScenarioProof {
                scenario_id: "synthetic_1k_churn".to_string(),
                class: ScaleScenarioClass::Synthetic,
                dimensions: vec![
                    ProofDimension::Correctness,
                    ProofDimension::Throughput,
                    ProofDimension::Memory,
                ],
                status: ProofStatus::Passed,
                evidence_source: ScaleProofEvidenceSource::RchRemote,
                evidence: Some(complete_evidence(16, 68_719_476_736)),
                note: String::new(),
            },
            ScaleScenarioProof {
                scenario_id: "synthetic_10k_policy_audit".to_string(),
                class: ScaleScenarioClass::Synthetic,
                dimensions: vec![ProofDimension::Hardware],
                status: ProofStatus::SkippedNotProven,
                evidence_source: ScaleProofEvidenceSource::Synthetic,
                evidence: None,
                note: "SKIPPED_NOT_PROVEN: waiting for live high-core worker".to_string(),
            },
        ];
        let matrix = ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), proofs);

        let summary = matrix.coverage_summary();
        assert_eq!(summary.total_rows, 2);
        assert_eq!(summary.passed_rows, 1);
        assert_eq!(summary.skipped_not_proven_rows, 1);
        assert_eq!(summary.correctness_passed, 1);
        assert_eq!(summary.throughput_passed, 1);
        assert_eq!(summary.memory_passed, 1);
        assert_eq!(summary.hardware_passed, 0);
    }

    #[test]
    fn scale_proof_matrix_serde_preserves_machine_readable_status() {
        let proofs = vec![
            ScaleScenarioProof {
                scenario_id: "synthetic_1k_churn".to_string(),
                class: ScaleScenarioClass::Synthetic,
                dimensions: vec![ProofDimension::Correctness],
                status: ProofStatus::Passed,
                evidence_source: ScaleProofEvidenceSource::Synthetic,
                evidence: Some(complete_evidence(16, 68_719_476_736)),
                note: String::new(),
            },
            ScaleScenarioProof {
                scenario_id: "synthetic_10k_policy_audit".to_string(),
                class: ScaleScenarioClass::Synthetic,
                dimensions: vec![ProofDimension::Hardware],
                status: ProofStatus::SkippedNotProven,
                evidence_source: ScaleProofEvidenceSource::Synthetic,
                evidence: None,
                note: "SKIPPED_NOT_PROVEN: hardware proof gap".to_string(),
            },
        ];
        let matrix = ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), proofs);

        let json = serde_json::to_string_pretty(&matrix).unwrap();
        assert!(json.contains("\"PASSED\""));
        assert!(json.contains("\"SKIPPED_NOT_PROVEN\""));

        let restored: ScaleProofMatrix = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.manifest.scenarios.len(), 3);
        assert_eq!(restored.proofs[1].status, ProofStatus::SkippedNotProven);
    }

    #[test]
    fn checked_in_scale_proof_evidence_index_fixture_validates() {
        let json = include_str!("../../../fixtures/scale-lab/massive-swarm-evidence-index.v1.json");
        let matrix: ScaleProofMatrix = serde_json::from_str(json).unwrap();

        assert_eq!(matrix.proofs.len(), 3);
        assert!(
            matrix
                .proofs
                .iter()
                .any(|proof| proof.evidence_source == ScaleProofEvidenceSource::Synthetic)
        );
        assert!(
            matrix
                .proofs
                .iter()
                .any(|proof| proof.evidence_source == ScaleProofEvidenceSource::RchRemote)
        );
        assert!(
            matrix
                .proofs
                .iter()
                .any(|proof| proof.status == ProofStatus::SkippedNotProven
                    && proof.dimensions.contains(&ProofDimension::Hardware)
                    && proof.note.contains("SKIPPED_NOT_PROVEN"))
        );

        let findings = matrix.validate_evidence_index(64, 274_877_906_944);
        assert!(
            findings
                .iter()
                .all(|finding| finding.severity != ScaleProofFindingSeverity::Error),
            "unexpected evidence-index errors: {findings:?}"
        );
        assert!(!matrix.hardware_claims_proven(64, 274_877_906_944));

        let summary = matrix.coverage_summary();
        assert_eq!(summary.total_rows, 3);
        assert_eq!(summary.passed_rows, 2);
        assert_eq!(summary.skipped_not_proven_rows, 1);
        assert_eq!(summary.hardware_passed, 0);
    }

    #[test]
    fn green_hardware_claim_requires_complete_worker_command_and_capacity_evidence() {
        let mut incomplete = complete_evidence(0, 0);
        incomplete.worker_id.clear();
        incomplete.command.clear();
        incomplete.storage_class.clear();

        let proof = ScaleScenarioProof {
            scenario_id: "synthetic_10k_policy_audit".to_string(),
            class: ScaleScenarioClass::LiveHardware,
            dimensions: vec![ProofDimension::Hardware],
            status: ProofStatus::Passed,
            evidence_source: ScaleProofEvidenceSource::LiveHardware,
            evidence: Some(incomplete),
            note: String::new(),
        };
        let matrix =
            ScaleProofMatrix::new(ScaleScenarioManifest::massive_swarm_defaults(), vec![proof]);

        let findings = matrix.validate_evidence_index(64, 274_877_906_944);
        assert!(
            findings.iter().any(|finding| finding.reason_code
                == "hardware_pass_incomplete_execution_evidence"),
            "missing incomplete-evidence finding: {findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.reason_code == "hardware_pass_predicate_not_met"),
            "missing capacity-predicate finding: {findings:?}"
        );
        assert!(!matrix.hardware_claims_proven(64, 274_877_906_944));
    }
}
