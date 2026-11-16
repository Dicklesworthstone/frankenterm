use super::{
    CanonicalizerConfig, DeterministicTrace, ReplayCanonicalizer, ReplayComparisonResult,
    TraceMismatch, TraceStep,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// A golden artifact: a reference trace with checksum for regression checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenArtifact {
    /// Unique identifier for this artifact (e.g., "scheduler-hot-path-v2").
    pub artifact_id: String,
    /// Version of the artifact (incremented on approved changes).
    pub version: u64,
    /// The reference trace.
    pub trace: DeterministicTrace,
    /// FNV-1a digest of the canonical trace.
    pub checksum: u64,
    /// Description of the optimization this artifact guards.
    pub description: String,
    /// Timestamp when this artifact was created/updated.
    pub created_at_us: u64,
}

impl GoldenArtifact {
    /// Create a new golden artifact from a trace.
    pub fn new(
        artifact_id: String,
        trace: DeterministicTrace,
        description: String,
        created_at_us: u64,
    ) -> Self {
        let checksum = trace.digest();
        Self {
            artifact_id,
            version: 1,
            trace,
            checksum,
            description,
            created_at_us,
        }
    }

    /// Verify that the stored checksum matches the trace digest.
    pub fn verify_checksum(&self) -> bool {
        self.trace.digest() == self.checksum
    }

    /// Update the golden artifact with a new trace (bumps version).
    pub fn update(&mut self, trace: DeterministicTrace, created_at_us: u64) {
        self.checksum = trace.digest();
        self.trace = trace;
        self.version += 1;
        self.created_at_us = created_at_us;
    }
}

impl fmt::Display for GoldenArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Golden[{} v{}, entries={}, checksum={:#x}]",
            self.artifact_id,
            self.version,
            self.trace.len(),
            self.checksum
        )
    }
}

/// Verdict from an optimization proof gate check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProofGateVerdict {
    /// Optimization preserves behavior exactly.
    Equivalent,
    /// Optimization preserves behavior under reordering (isomorphic).
    IsomorphicEquivalent { reordered_count: usize },
    /// Semantic drift detected: optimization changed behavior.
    SemanticDrift {
        /// Index of the first divergent entry.
        first_divergence_idx: usize,
        /// Mismatches found.
        mismatches: Vec<TraceMismatch>,
        /// Human-readable summary.
        summary: String,
    },
    /// Checksum mismatch on golden artifact (corruption or tampering).
    ChecksumFailure { expected: u64, actual: u64 },
}

impl ProofGateVerdict {
    /// Whether the verdict allows the optimization to proceed.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Equivalent | Self::IsomorphicEquivalent { .. })
    }

    /// Whether the verdict blocks the optimization.
    pub fn is_fail(&self) -> bool {
        !self.is_pass()
    }
}

impl fmt::Display for ProofGateVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Equivalent => write!(f, "PASS: equivalent"),
            Self::IsomorphicEquivalent { reordered_count } => {
                write!(f, "PASS: isomorphic ({reordered_count} reordered)")
            }
            Self::SemanticDrift {
                first_divergence_idx,
                mismatches,
                summary,
            } => {
                write!(
                    f,
                    "FAIL: semantic drift at [{first_divergence_idx}], {} mismatches: {summary}",
                    mismatches.len()
                )
            }
            Self::ChecksumFailure { expected, actual } => {
                write!(f, "FAIL: checksum {expected:#x} != {actual:#x}")
            }
        }
    }
}

/// Configuration for the proof gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofGateConfig {
    /// Whether to allow isomorphic equivalence (reordered but same content).
    pub allow_isomorphic: bool,
    /// Maximum number of mismatches to report before truncating.
    pub max_mismatches: usize,
    /// Canonicalization config to use for comparisons.
    pub canonicalizer_config: CanonicalizerConfig,
}

impl Default for ProofGateConfig {
    fn default() -> Self {
        Self {
            allow_isomorphic: true,
            max_mismatches: 50,
            canonicalizer_config: CanonicalizerConfig::default(),
        }
    }
}

/// A proof summary for logging/CI output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofSummary {
    /// Artifact being checked.
    pub artifact_id: String,
    /// Golden version tested against.
    pub golden_version: u64,
    /// Verdict of the check.
    pub verdict: ProofGateVerdict,
    /// Number of entries in the candidate trace.
    pub candidate_entries: usize,
    /// Number of entries in the golden trace.
    pub golden_entries: usize,
    /// Duration of the proof check (microseconds).
    pub check_duration_us: u64,
    /// Timestamp of the check.
    pub timestamp_us: u64,
}

impl fmt::Display for ProofSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Proof[{} v{}: {} ({}/{}e, {}μs)]",
            self.artifact_id,
            self.golden_version,
            self.verdict,
            self.candidate_entries,
            self.golden_entries,
            self.check_duration_us,
        )
    }
}

/// Snapshot of the proof gate state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofGateSnapshot {
    /// Total checks run.
    pub checks_run: u64,
    /// Total passes (equivalent or isomorphic).
    pub passes: u64,
    /// Total failures (drift or checksum).
    pub failures: u64,
    /// Number of golden artifacts stored.
    pub artifacts_count: usize,
    /// Configuration.
    pub config: ProofGateConfig,
}

/// Degradation state for the proof gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProofGateDegradation {
    /// Operating normally.
    Healthy,
    /// High failure rate suggests unstable optimizations.
    HighFailureRate { rate: f64 },
    /// Many artifacts may slow down CI checks.
    HighArtifactCount { count: usize },
}

impl fmt::Display for ProofGateDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::HighFailureRate { rate } => write!(f, "high-failure-rate({rate:.2})"),
            Self::HighArtifactCount { count } => write!(f, "high-artifact-count({count})"),
        }
    }
}

/// Log entry for proof gate operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofGateLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Artifact checked.
    pub artifact_id: String,
    /// Pass or fail.
    pub passed: bool,
    /// Duration of check.
    pub check_duration_us: u64,
}

/// The optimization isomorphism proof gate.
pub struct ProofGate {
    config: ProofGateConfig,
    artifacts: Vec<GoldenArtifact>,
    canonicalizer: ReplayCanonicalizer,
    checks_run: u64,
    passes: u64,
    failures: u64,
}

impl ProofGate {
    /// Create a new proof gate with the given config.
    pub fn new(config: ProofGateConfig) -> Self {
        let canonicalizer = ReplayCanonicalizer::new(config.canonicalizer_config.clone());
        Self {
            config,
            artifacts: Vec::new(),
            canonicalizer,
            checks_run: 0,
            passes: 0,
            failures: 0,
        }
    }

    /// Register a golden artifact.
    pub fn register_golden(&mut self, artifact: GoldenArtifact) {
        if let Some(pos) = self
            .artifacts
            .iter()
            .position(|a| a.artifact_id == artifact.artifact_id)
        {
            self.artifacts[pos] = artifact;
        } else {
            self.artifacts.push(artifact);
        }
    }

    /// Look up a golden artifact by ID.
    pub fn get_golden(&self, artifact_id: &str) -> Option<&GoldenArtifact> {
        self.artifacts.iter().find(|a| a.artifact_id == artifact_id)
    }

    /// Check a candidate trace against a golden artifact.
    pub fn check(
        &mut self,
        artifact_id: &str,
        candidate: &DeterministicTrace,
        timestamp_us: u64,
    ) -> ProofSummary {
        self.checks_run += 1;

        let golden = self.artifacts.iter().find(|a| a.artifact_id == artifact_id);
        let golden = match golden {
            Some(g) => g.clone(),
            None => {
                self.failures += 1;
                return ProofSummary {
                    artifact_id: artifact_id.to_string(),
                    golden_version: 0,
                    verdict: ProofGateVerdict::SemanticDrift {
                        first_divergence_idx: 0,
                        mismatches: vec![],
                        summary: format!("golden artifact '{artifact_id}' not found"),
                    },
                    candidate_entries: candidate.len(),
                    golden_entries: 0,
                    check_duration_us: 0,
                    timestamp_us,
                };
            }
        };

        if !golden.verify_checksum() {
            self.failures += 1;
            return ProofSummary {
                artifact_id: artifact_id.to_string(),
                golden_version: golden.version,
                verdict: ProofGateVerdict::ChecksumFailure {
                    expected: golden.checksum,
                    actual: golden.trace.digest(),
                },
                candidate_entries: candidate.len(),
                golden_entries: golden.trace.len(),
                check_duration_us: 0,
                timestamp_us,
            };
        }

        let comparison = self.canonicalizer.compare(&golden.trace, candidate);
        let verdict = match comparison {
            ReplayComparisonResult::Identical => ProofGateVerdict::Equivalent,
            ReplayComparisonResult::Isomorphic { reordered_count }
                if self.config.allow_isomorphic =>
            {
                ProofGateVerdict::IsomorphicEquivalent { reordered_count }
            }
            ReplayComparisonResult::Isomorphic { reordered_count } => {
                ProofGateVerdict::SemanticDrift {
                    first_divergence_idx: 0,
                    mismatches: vec![],
                    summary: format!("isomorphic not allowed ({reordered_count} reordered)"),
                }
            }
            ReplayComparisonResult::Divergent {
                first_divergence_idx,
                description,
            } => {
                let mut mismatches = self
                    .canonicalizer
                    .diagnose_mismatches(&golden.trace, candidate);
                if mismatches.len() > self.config.max_mismatches {
                    mismatches.truncate(self.config.max_mismatches);
                }
                ProofGateVerdict::SemanticDrift {
                    first_divergence_idx,
                    mismatches,
                    summary: description,
                }
            }
        };

        let passed = verdict.is_pass();
        if passed {
            self.passes += 1;
        } else {
            self.failures += 1;
        }

        ProofSummary {
            artifact_id: artifact_id.to_string(),
            golden_version: golden.version,
            verdict,
            candidate_entries: candidate.len(),
            golden_entries: golden.trace.len(),
            check_duration_us: 0,
            timestamp_us,
        }
    }

    /// Check all registered golden artifacts against a set of candidates.
    pub fn check_all(
        &mut self,
        candidates: &HashMap<String, DeterministicTrace>,
        timestamp_us: u64,
    ) -> Vec<ProofSummary> {
        let artifact_ids: Vec<String> = self
            .artifacts
            .iter()
            .map(|a| a.artifact_id.clone())
            .collect();
        let mut summaries = Vec::new();
        for id in &artifact_ids {
            if let Some(candidate) = candidates.get(id) {
                summaries.push(self.check(id, candidate, timestamp_us));
            }
        }
        summaries
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> ProofGateSnapshot {
        ProofGateSnapshot {
            checks_run: self.checks_run,
            passes: self.passes,
            failures: self.failures,
            artifacts_count: self.artifacts.len(),
            config: self.config.clone(),
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> ProofGateDegradation {
        if self.artifacts.len() > 100 {
            return ProofGateDegradation::HighArtifactCount {
                count: self.artifacts.len(),
            };
        }
        if self.checks_run > 0 {
            let rate = self.failures as f64 / self.checks_run as f64;
            if rate > 0.5 {
                return ProofGateDegradation::HighFailureRate { rate };
            }
        }
        ProofGateDegradation::Healthy
    }

    /// Create a log entry.
    pub fn log_entry(
        &self,
        artifact_id: &str,
        passed: bool,
        check_duration_us: u64,
    ) -> ProofGateLogEntry {
        ProofGateLogEntry {
            timestamp_us: self.checks_run,
            artifact_id: artifact_id.to_string(),
            passed,
            check_duration_us,
        }
    }

    /// Number of registered artifacts.
    pub fn artifact_count(&self) -> usize {
        self.artifacts.len()
    }

    /// All artifact IDs.
    pub fn artifact_ids(&self) -> Vec<String> {
        self.artifacts
            .iter()
            .map(|a| a.artifact_id.clone())
            .collect()
    }

    /// Reset counters (keeps artifacts).
    pub fn reset_counters(&mut self) {
        self.checks_run = 0;
        self.passes = 0;
        self.failures = 0;
    }

    /// Remove a golden artifact by ID.
    pub fn remove_golden(&mut self, artifact_id: &str) -> bool {
        let len_before = self.artifacts.len();
        self.artifacts.retain(|a| a.artifact_id != artifact_id);
        self.artifacts.len() < len_before
    }

    /// Access inner canonicalizer.
    pub fn canonicalizer(&self) -> &ReplayCanonicalizer {
        &self.canonicalizer
    }

    /// Access config.
    pub fn config(&self) -> &ProofGateConfig {
        &self.config
    }

    /// Check a candidate trace against a golden artifact built from
    /// ModelChecker TraceSteps (v1 -> v2 upgrade + proof check).
    pub fn check_from_mc_trace(
        &mut self,
        artifact_id: &str,
        mc_steps: &[TraceStep],
        seed: u64,
        timestamp_us: u64,
    ) -> ProofSummary {
        let candidate =
            self.canonicalizer
                .upgrade_trace(mc_steps, "mc-candidate".to_string(), seed);
        self.check(artifact_id, &candidate, timestamp_us)
    }

    /// Register a golden artifact from model-checker output.
    pub fn register_golden_from_mc(
        &mut self,
        artifact_id: String,
        mc_steps: &[TraceStep],
        seed: u64,
        description: String,
        created_at_us: u64,
    ) {
        let trace = self
            .canonicalizer
            .upgrade_trace(mc_steps, artifact_id.clone(), seed);
        let ga = GoldenArtifact::new(artifact_id, trace, description, created_at_us);
        self.register_golden(ga);
    }

    /// Approve a semantic drift: update the golden artifact to the candidate trace.
    pub fn approve_drift(
        &mut self,
        artifact_id: &str,
        candidate: &DeterministicTrace,
        created_at_us: u64,
    ) -> bool {
        if let Some(pos) = self
            .artifacts
            .iter()
            .position(|a| a.artifact_id == artifact_id)
        {
            self.artifacts[pos].update(candidate.clone(), created_at_us);
            true
        } else {
            false
        }
    }

    /// Get all failing artifact IDs from the latest check_all results.
    pub fn failing_artifacts(summaries: &[ProofSummary]) -> Vec<String> {
        summaries
            .iter()
            .filter(|s| s.verdict.is_fail())
            .map(|s| s.artifact_id.clone())
            .collect()
    }

    /// Get all passing artifact IDs from the latest check_all results.
    pub fn passing_artifacts(summaries: &[ProofSummary]) -> Vec<String> {
        summaries
            .iter()
            .filter(|s| s.verdict.is_pass())
            .map(|s| s.artifact_id.clone())
            .collect()
    }

    /// Pass rate across a set of proof summaries.
    pub fn pass_rate(summaries: &[ProofSummary]) -> f64 {
        if summaries.is_empty() {
            return 1.0;
        }
        let passes = summaries.iter().filter(|s| s.verdict.is_pass()).count();
        passes as f64 / summaries.len() as f64
    }

    /// Total pass count.
    pub fn total_passes(&self) -> u64 {
        self.passes
    }

    /// Total failure count.
    pub fn total_failures(&self) -> u64 {
        self.failures
    }

    /// Total checks.
    pub fn total_checks(&self) -> u64 {
        self.checks_run
    }
}
