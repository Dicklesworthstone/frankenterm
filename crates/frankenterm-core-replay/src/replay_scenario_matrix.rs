//! Scenario matrix runner for baseline-vs-candidate sweeps (ft-og6q6.4.3).
//!
//! Provides:
//! - [`MatrixConfig`] — TOML-based matrix definition (artifacts x overrides).
//! - [`ScenarioMatrixRunner`] — Executes all (artifact, override) pairs.
//! - [`MatrixResult`] — Aggregate results with diff summaries.
//! - [`ScenarioResult`] — Per-scenario outcome with decision diffs.

use serde::{Deserialize, Serialize};
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
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
