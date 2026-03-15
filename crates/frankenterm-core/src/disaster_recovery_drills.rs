//! Disaster recovery drill framework for control-plane continuity testing.
//!
//! Provides structured, repeatable DR drill scenarios that exercise the
//! backup/restore pipeline and measure RTO/RPO compliance. Each drill
//! produces a scored report with pass/fail verdicts and metric breakdowns.
//!
//! # Architecture
//!
//! ```text
//! DrillScenario  ──▶  DrillRunner  ──▶  DrillReport
//!   (what to test)     (execute)         (scored results)
//!       │                  │
//!       │                  ├── backup.rs (ExportResult, verify_backup)
//!       │                  ├── snapshot_engine.rs (SnapshotEngine)
//!       │                  └── session_restore.rs (SessionRestorer)
//!       │
//!       ├── ColdStart       (DB missing, rebuild from backup)
//!       ├── PartialFailure  (some panes fail to restore)
//!       ├── CascadingFailure (backup + restore both degrade)
//!       ├── TimeTravel       (restore to an older checkpoint)
//!       └── ScaleRecovery    (many panes, measure throughput)
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use frankenterm_core::disaster_recovery_drills::*;
//!
//! let config = DrillConfig::default();
//! let mut runner = DrillRunner::new(config);
//!
//! // Register a cold-start drill
//! runner.add_scenario(DrillScenario::cold_start());
//!
//! // Execute all registered drills
//! let report = runner.execute_all();
//! assert!(report.overall_verdict.is_pass());
//! ```

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ─── RTO / RPO targets ──────────────────────────────────────────────────────

/// Recovery Time Objective — max acceptable time to restore service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtoTarget {
    /// Maximum allowed recovery duration.
    pub max_duration: Duration,
    /// Description for reports.
    pub label: String,
}

impl RtoTarget {
    #[must_use]
    pub fn new(max_duration: Duration, label: impl Into<String>) -> Self {
        Self {
            max_duration,
            label: label.into(),
        }
    }

    /// Check if the actual recovery time meets the target.
    #[must_use]
    pub fn is_met(&self, actual: Duration) -> bool {
        actual <= self.max_duration
    }

    /// How far off (positive = over target, negative = under).
    #[must_use]
    pub fn delta(&self, actual: Duration) -> i64 {
        actual.as_millis() as i64 - self.max_duration.as_millis() as i64
    }
}

/// Recovery Point Objective — max acceptable data loss window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpoTarget {
    /// Maximum acceptable data loss window.
    pub max_data_loss: Duration,
    /// Description for reports.
    pub label: String,
}

impl RpoTarget {
    #[must_use]
    pub fn new(max_data_loss: Duration, label: impl Into<String>) -> Self {
        Self {
            max_data_loss,
            label: label.into(),
        }
    }

    /// Check if the actual data loss is within the target.
    #[must_use]
    pub fn is_met(&self, actual_loss: Duration) -> bool {
        actual_loss <= self.max_data_loss
    }
}

// ─── Drill scenarios ─────────────────────────────────────────────────────────

/// The type of DR drill to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrillKind {
    /// Database is missing/corrupted; rebuild entirely from the latest backup.
    ColdStart,
    /// Some panes fail to restore (simulate I/O errors on specific panes).
    PartialFailure {
        /// Fraction of panes that should fail (0.0–1.0).
        failure_fraction: u32, // stored as permille (e.g. 250 = 25%)
    },
    /// Both backup creation and restore degrade simultaneously.
    CascadingFailure,
    /// Restore to an older checkpoint, not the most recent one.
    TimeTravel {
        /// How far back to restore (relative to most recent checkpoint).
        lookback: Duration,
    },
    /// Large-scale recovery: measure throughput at N panes.
    ScaleRecovery {
        /// Number of simulated panes to recover.
        pane_count: u64,
    },
    /// Custom scenario defined by name.
    Custom(String),
}

impl std::fmt::Display for DrillKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ColdStart => write!(f, "cold-start"),
            Self::PartialFailure { failure_fraction } => {
                write!(f, "partial-failure({}‰)", failure_fraction)
            }
            Self::CascadingFailure => write!(f, "cascading-failure"),
            Self::TimeTravel { lookback } => write!(f, "time-travel({}s)", lookback.as_secs()),
            Self::ScaleRecovery { pane_count } => write!(f, "scale-recovery({})", pane_count),
            Self::Custom(name) => write!(f, "custom:{name}"),
        }
    }
}

/// A complete drill scenario with targets and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillScenario {
    /// Unique identifier for this scenario.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// What kind of drill this is.
    pub kind: DrillKind,
    /// RTO target (None = no RTO check).
    pub rto: Option<RtoTarget>,
    /// RPO target (None = no RPO check).
    pub rpo: Option<RpoTarget>,
    /// Minimum recovery completeness required (0.0–1.0 as permille 0–1000).
    pub min_completeness_permille: u32,
    /// Tags for filtering.
    pub tags: Vec<String>,
}

impl DrillScenario {
    /// Create a cold-start drill with default targets.
    #[must_use]
    pub fn cold_start() -> Self {
        Self {
            id: "dr-cold-start".into(),
            name: "Cold Start Recovery".into(),
            kind: DrillKind::ColdStart,
            rto: Some(RtoTarget::new(Duration::from_secs(120), "2-minute RTO")),
            rpo: Some(RpoTarget::new(Duration::from_secs(300), "5-minute RPO")),
            min_completeness_permille: 950, // 95%
            tags: vec!["critical".into(), "baseline".into()],
        }
    }

    /// Create a partial-failure drill.
    #[must_use]
    pub fn partial_failure(failure_permille: u32) -> Self {
        Self {
            id: format!("dr-partial-{failure_permille}"),
            name: format!("Partial Failure ({failure_permille}‰ pane loss)"),
            kind: DrillKind::PartialFailure {
                failure_fraction: failure_permille,
            },
            rto: Some(RtoTarget::new(Duration::from_secs(60), "1-minute RTO")),
            rpo: None,
            min_completeness_permille: 1000 - failure_permille,
            tags: vec!["resilience".into()],
        }
    }

    /// Create a cascading failure drill.
    #[must_use]
    pub fn cascading_failure() -> Self {
        Self {
            id: "dr-cascading".into(),
            name: "Cascading Failure".into(),
            kind: DrillKind::CascadingFailure,
            rto: Some(RtoTarget::new(Duration::from_secs(300), "5-minute RTO")),
            rpo: Some(RpoTarget::new(Duration::from_secs(600), "10-minute RPO")),
            min_completeness_permille: 800, // 80%
            tags: vec!["chaos".into(), "critical".into()],
        }
    }

    /// Create a time-travel drill.
    #[must_use]
    pub fn time_travel(lookback: Duration) -> Self {
        Self {
            id: format!("dr-timetravel-{}s", lookback.as_secs()),
            name: format!("Time Travel ({}s lookback)", lookback.as_secs()),
            kind: DrillKind::TimeTravel { lookback },
            rto: Some(RtoTarget::new(Duration::from_secs(180), "3-minute RTO")),
            rpo: None,
            min_completeness_permille: 900,
            tags: vec!["checkpoint".into()],
        }
    }

    /// Create a scale recovery drill.
    #[must_use]
    pub fn scale_recovery(pane_count: u64) -> Self {
        Self {
            id: format!("dr-scale-{pane_count}"),
            name: format!("Scale Recovery ({pane_count} panes)"),
            kind: DrillKind::ScaleRecovery { pane_count },
            rto: Some(RtoTarget::new(
                Duration::from_secs(pane_count.max(60)),
                format!("{pane_count}s RTO"),
            )),
            rpo: None,
            min_completeness_permille: 950,
            tags: vec!["scale".into(), "performance".into()],
        }
    }
}

// ─── Drill execution results ─────────────────────────────────────────────────

/// Verdict for a single drill or the overall report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrillVerdict {
    /// All targets met.
    Pass,
    /// Some targets not met but recovery succeeded.
    Degraded,
    /// Recovery failed or critical targets missed.
    Fail,
    /// Drill was skipped (e.g. preconditions not met).
    Skipped,
}

impl DrillVerdict {
    #[must_use]
    pub fn is_pass(self) -> bool {
        self == Self::Pass
    }

    #[must_use]
    pub fn is_fail(self) -> bool {
        self == Self::Fail
    }

    /// Combine two verdicts: the worse one wins.
    #[must_use]
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Fail, _) | (_, Self::Fail) => Self::Fail,
            (Self::Degraded, _) | (_, Self::Degraded) => Self::Degraded,
            (Self::Skipped, _) | (_, Self::Skipped) => Self::Skipped,
            (Self::Pass, Self::Pass) => Self::Pass,
        }
    }
}

impl std::fmt::Display for DrillVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Degraded => write!(f, "DEGRADED"),
            Self::Fail => write!(f, "FAIL"),
            Self::Skipped => write!(f, "SKIPPED"),
        }
    }
}

/// Metrics collected during a single drill execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrillMetrics {
    /// Total recovery time (backup locate + restore + verify).
    pub recovery_time_ms: u64,
    /// Data loss window (gap between last checkpoint and failure point).
    pub data_loss_window_ms: u64,
    /// Fraction of panes/resources successfully recovered (permille).
    pub completeness_permille: u32,
    /// Number of panes attempted.
    pub panes_attempted: u64,
    /// Number of panes successfully restored.
    pub panes_restored: u64,
    /// Number of panes that failed to restore.
    pub panes_failed: u64,
    /// Scrollback lines recovered.
    pub scrollback_lines_recovered: u64,
    /// Total scrollback lines expected.
    pub scrollback_lines_expected: u64,
    /// Backup size in bytes (if applicable).
    pub backup_size_bytes: u64,
    /// Additional metrics keyed by name.
    pub extra: HashMap<String, u64>,
}

impl DrillMetrics {
    /// Recovery completeness as a fraction (0.0 – 1.0).
    #[must_use]
    pub fn completeness_fraction(&self) -> f64 {
        self.completeness_permille as f64 / 1000.0
    }

    /// Scrollback recovery ratio.
    #[must_use]
    pub fn scrollback_ratio(&self) -> f64 {
        if self.scrollback_lines_expected == 0 {
            return 1.0;
        }
        self.scrollback_lines_recovered as f64 / self.scrollback_lines_expected as f64
    }
}

/// Result of executing a single drill scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillResult {
    /// The scenario that was executed.
    pub scenario_id: String,
    /// The kind of drill.
    pub kind: DrillKind,
    /// Pass/fail verdict.
    pub verdict: DrillVerdict,
    /// Collected metrics.
    pub metrics: DrillMetrics,
    /// Whether the RTO target was met (None if no target set).
    pub rto_met: Option<bool>,
    /// Whether the RPO target was met (None if no target set).
    pub rpo_met: Option<bool>,
    /// Whether the completeness target was met.
    pub completeness_met: bool,
    /// Human-readable notes about the drill execution.
    pub notes: Vec<String>,
    /// Errors encountered during the drill.
    pub errors: Vec<String>,
}

/// Aggregate report from executing all drills.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillReport {
    /// Timestamp of report generation (unix ms).
    pub generated_at_ms: u64,
    /// Overall verdict (worst of all drill results).
    pub overall_verdict: DrillVerdict,
    /// Individual drill results.
    pub results: Vec<DrillResult>,
    /// Summary statistics.
    pub summary: DrillSummary,
}

/// Summary statistics across all drills.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrillSummary {
    pub total_drills: u32,
    pub passed: u32,
    pub degraded: u32,
    pub failed: u32,
    pub skipped: u32,
    pub avg_recovery_time_ms: u64,
    pub worst_recovery_time_ms: u64,
    pub avg_completeness_permille: u32,
}

// ─── Drill runner ────────────────────────────────────────────────────────────

/// Configuration for the drill runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillConfig {
    /// Whether to continue executing drills after a failure.
    pub continue_on_failure: bool,
    /// Maximum duration for a single drill before it's considered timed out.
    pub drill_timeout: Duration,
    /// Whether to produce verbose notes in the report.
    pub verbose: bool,
}

impl Default for DrillConfig {
    fn default() -> Self {
        Self {
            continue_on_failure: true,
            drill_timeout: Duration::from_secs(600),
            verbose: false,
        }
    }
}

/// Executes DR drill scenarios and collects results.
///
/// The runner evaluates scenarios against their targets (RTO, RPO, completeness)
/// and produces a scored report. Scenarios can be added individually or in
/// predefined suites.
pub struct DrillRunner {
    config: DrillConfig,
    scenarios: Vec<DrillScenario>,
    results: Vec<DrillResult>,
}

impl DrillRunner {
    /// Create a new drill runner.
    #[must_use]
    pub fn new(config: DrillConfig) -> Self {
        Self {
            config,
            scenarios: Vec::new(),
            results: Vec::new(),
        }
    }

    /// Add a single scenario.
    pub fn add_scenario(&mut self, scenario: DrillScenario) {
        self.scenarios.push(scenario);
    }

    /// Add the standard baseline suite (cold start + partial + cascading).
    pub fn add_baseline_suite(&mut self) {
        self.scenarios.push(DrillScenario::cold_start());
        self.scenarios.push(DrillScenario::partial_failure(250)); // 25%
        self.scenarios.push(DrillScenario::cascading_failure());
    }

    /// Add a scale testing suite.
    pub fn add_scale_suite(&mut self) {
        for count in [10, 50, 100, 200] {
            self.scenarios.push(DrillScenario::scale_recovery(count));
        }
    }

    /// Number of registered scenarios.
    #[must_use]
    pub fn scenario_count(&self) -> usize {
        self.scenarios.len()
    }

    /// Execute a single scenario with simulated metrics.
    ///
    /// In production, this would invoke real backup/restore operations.
    /// The simulated path evaluates the scenario structure and target
    /// configuration to produce a valid drill result.
    pub fn execute_scenario(&self, scenario: &DrillScenario) -> DrillResult {
        self.evaluate_scenario(scenario, None)
    }

    /// Execute a scenario with externally-provided metrics.
    ///
    /// This is the integration point for real backup/restore operations.
    /// Pass actual metrics from running the backup pipeline and this method
    /// evaluates them against the scenario's targets.
    pub fn evaluate_scenario(
        &self,
        scenario: &DrillScenario,
        metrics: Option<DrillMetrics>,
    ) -> DrillResult {
        let metrics = metrics.unwrap_or_else(|| self.simulate_metrics(scenario));
        let mut notes = Vec::new();
        let mut errors = Vec::new();

        // Evaluate RTO
        let rto_met = scenario.rto.as_ref().map(|target| {
            let actual = Duration::from_millis(metrics.recovery_time_ms);
            let met = target.is_met(actual);
            if self.config.verbose {
                notes.push(format!(
                    "RTO: actual={}ms target={}ms {}",
                    metrics.recovery_time_ms,
                    target.max_duration.as_millis(),
                    if met { "MET" } else { "MISSED" }
                ));
            }
            if !met {
                errors.push(format!("RTO exceeded by {}ms", target.delta(actual)));
            }
            met
        });

        // Evaluate RPO
        let recovery_point_met = scenario.rpo.as_ref().map(|target| {
            let actual = Duration::from_millis(metrics.data_loss_window_ms);
            let rpo_passed = target.is_met(actual);
            if self.config.verbose {
                notes.push(format!(
                    "RPO: actual={}ms target={}ms {}",
                    metrics.data_loss_window_ms,
                    target.max_data_loss.as_millis(),
                    if rpo_passed { "MET" } else { "MISSED" }
                ));
            }
            if !rpo_passed {
                errors.push(format!(
                    "RPO exceeded: data_loss={}ms > target={}ms",
                    metrics.data_loss_window_ms,
                    target.max_data_loss.as_millis()
                ));
            }
            rpo_passed
        });

        // Evaluate completeness
        let completeness_met = metrics.completeness_permille >= scenario.min_completeness_permille;
        if !completeness_met {
            errors.push(format!(
                "completeness {:.1}% < required {:.1}%",
                metrics.completeness_permille as f64 / 10.0,
                scenario.min_completeness_permille as f64 / 10.0
            ));
        }

        // Determine verdict
        let verdict =
            if !completeness_met || rto_met == Some(false) || recovery_point_met == Some(false) {
                if completeness_met && (rto_met != Some(false) || recovery_point_met != Some(false))
                {
                    DrillVerdict::Degraded
                } else {
                    DrillVerdict::Fail
                }
            } else {
                DrillVerdict::Pass
            };

        notes.push(format!(
            "completeness={:.1}% (target={:.1}%)",
            metrics.completeness_permille as f64 / 10.0,
            scenario.min_completeness_permille as f64 / 10.0
        ));

        DrillResult {
            scenario_id: scenario.id.clone(),
            kind: scenario.kind.clone(),
            verdict,
            metrics,
            rto_met,
            rpo_met: recovery_point_met,
            completeness_met,
            notes,
            errors,
        }
    }

    /// Execute all registered scenarios and produce a report.
    pub fn execute_all(&mut self) -> DrillReport {
        let scenarios: Vec<DrillScenario> = self.scenarios.clone();
        self.results.clear();

        for scenario in &scenarios {
            let result = self.execute_scenario(scenario);
            let is_fail = result.verdict.is_fail();
            self.results.push(result);

            if is_fail && !self.config.continue_on_failure {
                break;
            }
        }

        self.generate_report()
    }

    /// Execute all scenarios with externally-provided metrics per scenario.
    ///
    /// `metrics_map` is keyed by scenario ID.
    pub fn execute_with_metrics(
        &mut self,
        metrics_map: &HashMap<String, DrillMetrics>,
    ) -> DrillReport {
        let scenarios: Vec<DrillScenario> = self.scenarios.clone();
        self.results.clear();

        for scenario in &scenarios {
            let metrics = metrics_map.get(&scenario.id).cloned();
            let result = self.evaluate_scenario(scenario, metrics);
            let is_fail = result.verdict.is_fail();
            self.results.push(result);

            if is_fail && !self.config.continue_on_failure {
                break;
            }
        }

        self.generate_report()
    }

    /// Generate a report from collected results.
    fn generate_report(&self) -> DrillReport {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        let mut overall = DrillVerdict::Pass;
        let mut summary = DrillSummary {
            total_drills: self.results.len() as u32,
            ..Default::default()
        };

        let mut total_recovery_ms: u64 = 0;
        let mut total_completeness: u64 = 0;

        for r in &self.results {
            overall = overall.combine(r.verdict);
            match r.verdict {
                DrillVerdict::Pass => summary.passed += 1,
                DrillVerdict::Degraded => summary.degraded += 1,
                DrillVerdict::Fail => summary.failed += 1,
                DrillVerdict::Skipped => summary.skipped += 1,
            }
            total_recovery_ms += r.metrics.recovery_time_ms;
            total_completeness += r.metrics.completeness_permille as u64;
            summary.worst_recovery_time_ms = summary
                .worst_recovery_time_ms
                .max(r.metrics.recovery_time_ms);
        }

        if !self.results.is_empty() {
            summary.avg_recovery_time_ms = total_recovery_ms / self.results.len() as u64;
            summary.avg_completeness_permille =
                (total_completeness / self.results.len() as u64) as u32;
        }

        DrillReport {
            generated_at_ms: now_ms,
            overall_verdict: overall,
            results: self.results.clone(),
            summary,
        }
    }

    /// Simulate metrics for a scenario (used when no real pipeline is available).
    #[allow(clippy::unused_self)]
    fn simulate_metrics(&self, scenario: &DrillScenario) -> DrillMetrics {
        match &scenario.kind {
            DrillKind::ColdStart => DrillMetrics {
                recovery_time_ms: 45_000,     // 45s simulated
                data_loss_window_ms: 120_000, // 2min simulated
                completeness_permille: 980,
                panes_attempted: 10,
                panes_restored: 10,
                panes_failed: 0,
                scrollback_lines_recovered: 9500,
                scrollback_lines_expected: 10000,
                backup_size_bytes: 50 * 1024 * 1024,
                extra: HashMap::new(),
            },
            DrillKind::PartialFailure { failure_fraction } => {
                let total = 20u64;
                let failed = (total * (*failure_fraction as u64)) / 1000;
                let restored = total - failed;
                let completeness = (restored * 1000)
                    .checked_div(total)
                    .map_or(1000, |v| v as u32);
                DrillMetrics {
                    recovery_time_ms: 30_000,
                    data_loss_window_ms: 0,
                    completeness_permille: completeness,
                    panes_attempted: total,
                    panes_restored: restored,
                    panes_failed: failed,
                    scrollback_lines_recovered: restored * 500,
                    scrollback_lines_expected: total * 500,
                    backup_size_bytes: 20 * 1024 * 1024,
                    extra: HashMap::new(),
                }
            }
            DrillKind::CascadingFailure => DrillMetrics {
                recovery_time_ms: 180_000,    // 3min — under 5min target
                data_loss_window_ms: 300_000, // 5min — under 10min target
                completeness_permille: 850,
                panes_attempted: 15,
                panes_restored: 13,
                panes_failed: 2,
                scrollback_lines_recovered: 6000,
                scrollback_lines_expected: 7500,
                backup_size_bytes: 30 * 1024 * 1024,
                extra: HashMap::new(),
            },
            DrillKind::TimeTravel { lookback } => DrillMetrics {
                recovery_time_ms: lookback.as_millis() as u64 / 10 + 20_000,
                data_loss_window_ms: lookback.as_millis() as u64,
                completeness_permille: 950,
                panes_attempted: 10,
                panes_restored: 10,
                panes_failed: 0,
                scrollback_lines_recovered: 8000,
                scrollback_lines_expected: 10000,
                backup_size_bytes: 40 * 1024 * 1024,
                extra: HashMap::new(),
            },
            DrillKind::ScaleRecovery { pane_count } => {
                let per_pane_ms = 200;
                DrillMetrics {
                    recovery_time_ms: pane_count * per_pane_ms,
                    data_loss_window_ms: 60_000,
                    completeness_permille: 990,
                    panes_attempted: *pane_count,
                    panes_restored: *pane_count,
                    panes_failed: 0,
                    scrollback_lines_recovered: pane_count * 500,
                    scrollback_lines_expected: pane_count * 500,
                    backup_size_bytes: pane_count * 2 * 1024 * 1024,
                    extra: HashMap::new(),
                }
            }
            DrillKind::Custom(_) => DrillMetrics {
                completeness_permille: 1000,
                ..Default::default()
            },
        }
    }

    /// Get the last generated results.
    #[must_use]
    pub fn results(&self) -> &[DrillResult] {
        &self.results
    }
}

// ─── Continuity health check ─────────────────────────────────────────────────

/// Health status of a DR subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContinuityStatus {
    /// Subsystem is healthy and drill-ready.
    Healthy,
    /// Subsystem has warnings but is functional.
    Warning(String),
    /// Subsystem is unhealthy.
    Unhealthy(String),
    /// Status is unknown (not checked yet).
    Unknown,
}

impl ContinuityStatus {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// A single health check for a DR subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuityHealthCheck {
    /// Subsystem name.
    pub subsystem: String,
    /// Current status.
    pub status: ContinuityStatus,
    /// When this was last checked (unix ms).
    pub last_checked_ms: u64,
    /// When the last successful backup completed (unix ms), if applicable.
    pub last_backup_ms: Option<u64>,
    /// Number of available restore points.
    pub restore_points: u32,
}

/// Aggregate continuity health across all DR subsystems.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuityReport {
    /// Per-subsystem health checks.
    pub checks: Vec<ContinuityHealthCheck>,
    /// Whether the system is drill-ready.
    pub drill_ready: bool,
    /// Overall status.
    pub overall: ContinuityStatus,
}

impl ContinuityReport {
    /// Construct a report from individual checks.
    #[must_use]
    pub fn from_checks(checks: Vec<ContinuityHealthCheck>) -> Self {
        let drill_ready = checks.iter().all(|c| c.status.is_healthy());
        let overall = if drill_ready {
            ContinuityStatus::Healthy
        } else if checks
            .iter()
            .any(|c| matches!(c.status, ContinuityStatus::Unhealthy(_)))
        {
            ContinuityStatus::Unhealthy("one or more subsystems unhealthy".into())
        } else if checks
            .iter()
            .all(|c| matches!(c.status, ContinuityStatus::Unknown))
        {
            ContinuityStatus::Unknown
        } else {
            ContinuityStatus::Warning("some subsystems have warnings".into())
        };
        Self {
            checks,
            drill_ready,
            overall,
        }
    }

    /// Number of healthy subsystems.
    #[must_use]
    pub fn healthy_count(&self) -> usize {
        self.checks.iter().filter(|c| c.status.is_healthy()).count()
    }

    /// Total number of restore points across all subsystems.
    #[must_use]
    pub fn total_restore_points(&self) -> u32 {
        self.checks.iter().map(|c| c.restore_points).sum()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── RTO / RPO ──

    #[test]
    fn rto_target_met() {
        let target = RtoTarget::new(Duration::from_secs(60), "1min");
        assert!(target.is_met(Duration::from_secs(30)));
        assert!(target.is_met(Duration::from_secs(60)));
        assert!(!target.is_met(Duration::from_secs(61)));
    }

    #[test]
    fn rto_delta() {
        let target = RtoTarget::new(Duration::from_secs(60), "1min");
        assert_eq!(target.delta(Duration::from_secs(60)), 0);
        assert!(target.delta(Duration::from_secs(90)) > 0);
        assert!(target.delta(Duration::from_secs(30)) < 0);
    }

    #[test]
    fn rpo_target_met() {
        let target = RpoTarget::new(Duration::from_secs(300), "5min");
        assert!(target.is_met(Duration::from_secs(200)));
        assert!(!target.is_met(Duration::from_secs(301)));
    }

    // ── DrillKind ──

    #[test]
    fn drill_kind_display() {
        assert_eq!(DrillKind::ColdStart.to_string(), "cold-start");
        assert_eq!(
            DrillKind::PartialFailure {
                failure_fraction: 250
            }
            .to_string(),
            "partial-failure(250‰)"
        );
        assert_eq!(DrillKind::CascadingFailure.to_string(), "cascading-failure");
        assert!(
            DrillKind::TimeTravel {
                lookback: Duration::from_secs(60)
            }
            .to_string()
            .contains("60s")
        );
        assert!(
            DrillKind::ScaleRecovery { pane_count: 100 }
                .to_string()
                .contains("100")
        );
        assert!(
            DrillKind::Custom("test".into())
                .to_string()
                .contains("test")
        );
    }

    #[test]
    fn drill_kind_serde_roundtrip() {
        let kinds = vec![
            DrillKind::ColdStart,
            DrillKind::PartialFailure {
                failure_fraction: 500,
            },
            DrillKind::CascadingFailure,
            DrillKind::TimeTravel {
                lookback: Duration::from_secs(120),
            },
            DrillKind::ScaleRecovery { pane_count: 50 },
            DrillKind::Custom("custom-test".into()),
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let back: DrillKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    // ── DrillVerdict ──

    #[test]
    fn verdict_predicates() {
        assert!(DrillVerdict::Pass.is_pass());
        assert!(!DrillVerdict::Pass.is_fail());
        assert!(DrillVerdict::Fail.is_fail());
        assert!(!DrillVerdict::Fail.is_pass());
    }

    #[test]
    fn verdict_combine() {
        assert_eq!(
            DrillVerdict::Pass.combine(DrillVerdict::Pass),
            DrillVerdict::Pass
        );
        assert_eq!(
            DrillVerdict::Pass.combine(DrillVerdict::Degraded),
            DrillVerdict::Degraded
        );
        assert_eq!(
            DrillVerdict::Degraded.combine(DrillVerdict::Fail),
            DrillVerdict::Fail
        );
        assert_eq!(
            DrillVerdict::Fail.combine(DrillVerdict::Pass),
            DrillVerdict::Fail
        );
        assert_eq!(
            DrillVerdict::Skipped.combine(DrillVerdict::Pass),
            DrillVerdict::Skipped
        );
    }

    #[test]
    fn verdict_display() {
        assert_eq!(DrillVerdict::Pass.to_string(), "PASS");
        assert_eq!(DrillVerdict::Fail.to_string(), "FAIL");
        assert_eq!(DrillVerdict::Degraded.to_string(), "DEGRADED");
        assert_eq!(DrillVerdict::Skipped.to_string(), "SKIPPED");
    }

    // ── DrillMetrics ──

    #[test]
    fn metrics_completeness_fraction() {
        let m = DrillMetrics {
            completeness_permille: 950,
            ..Default::default()
        };
        assert!((m.completeness_fraction() - 0.95).abs() < 0.001);
    }

    #[test]
    fn metrics_scrollback_ratio() {
        let m = DrillMetrics {
            scrollback_lines_recovered: 80,
            scrollback_lines_expected: 100,
            ..Default::default()
        };
        assert!((m.scrollback_ratio() - 0.8).abs() < 0.001);
    }

    #[test]
    fn metrics_scrollback_ratio_zero_expected() {
        let m = DrillMetrics::default();
        assert!((m.scrollback_ratio() - 1.0).abs() < 0.001);
    }

    // ── DrillScenario constructors ──

    #[test]
    fn cold_start_scenario() {
        let s = DrillScenario::cold_start();
        assert_eq!(s.kind, DrillKind::ColdStart);
        assert!(s.rto.is_some());
        assert!(s.rpo.is_some());
        assert_eq!(s.min_completeness_permille, 950);
    }

    #[test]
    fn partial_failure_scenario() {
        let s = DrillScenario::partial_failure(300);
        assert!(matches!(
            s.kind,
            DrillKind::PartialFailure {
                failure_fraction: 300
            }
        ));
        assert_eq!(s.min_completeness_permille, 700);
    }

    #[test]
    fn cascading_failure_scenario() {
        let s = DrillScenario::cascading_failure();
        assert_eq!(s.kind, DrillKind::CascadingFailure);
        assert_eq!(s.min_completeness_permille, 800);
    }

    #[test]
    fn time_travel_scenario() {
        let s = DrillScenario::time_travel(Duration::from_secs(300));
        assert!(matches!(s.kind, DrillKind::TimeTravel { .. }));
    }

    #[test]
    fn scale_recovery_scenario() {
        let s = DrillScenario::scale_recovery(200);
        assert!(matches!(
            s.kind,
            DrillKind::ScaleRecovery { pane_count: 200 }
        ));
    }

    // ── DrillRunner ──

    #[test]
    fn runner_add_scenarios() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        assert_eq!(runner.scenario_count(), 0);
        runner.add_scenario(DrillScenario::cold_start());
        assert_eq!(runner.scenario_count(), 1);
    }

    #[test]
    fn runner_baseline_suite() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_baseline_suite();
        assert_eq!(runner.scenario_count(), 3);
    }

    #[test]
    fn runner_scale_suite() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scale_suite();
        assert_eq!(runner.scenario_count(), 4);
    }

    #[test]
    fn execute_cold_start_passes() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scenario(DrillScenario::cold_start());
        let report = runner.execute_all();
        assert_eq!(report.results.len(), 1);
        assert!(report.overall_verdict.is_pass());
        assert_eq!(report.summary.passed, 1);
    }

    #[test]
    fn execute_baseline_suite() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_baseline_suite();
        let report = runner.execute_all();
        assert_eq!(report.results.len(), 3);
        assert_eq!(report.summary.total_drills, 3);
        // All simulated metrics are designed to pass
        assert!(report.overall_verdict.is_pass());
    }

    #[test]
    fn execute_with_external_metrics() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scenario(DrillScenario::cold_start());

        let mut metrics_map = HashMap::new();
        metrics_map.insert(
            "dr-cold-start".to_string(),
            DrillMetrics {
                recovery_time_ms: 50_000,
                data_loss_window_ms: 100_000,
                completeness_permille: 1000,
                panes_attempted: 5,
                panes_restored: 5,
                panes_failed: 0,
                ..Default::default()
            },
        );

        let report = runner.execute_with_metrics(&metrics_map);
        assert!(report.overall_verdict.is_pass());
    }

    #[test]
    fn execute_with_failing_metrics() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scenario(DrillScenario::cold_start());

        let mut metrics_map = HashMap::new();
        metrics_map.insert(
            "dr-cold-start".to_string(),
            DrillMetrics {
                recovery_time_ms: 999_999,    // way over 2-minute RTO
                data_loss_window_ms: 999_999, // way over 5-minute RPO
                completeness_permille: 100,   // 10% — way under 95%
                ..Default::default()
            },
        );

        let report = runner.execute_with_metrics(&metrics_map);
        assert!(report.overall_verdict.is_fail());
        assert_eq!(report.summary.failed, 1);
        assert!(!report.results[0].errors.is_empty());
    }

    #[test]
    fn stop_on_failure_when_configured() {
        let config = DrillConfig {
            continue_on_failure: false,
            ..Default::default()
        };
        let mut runner = DrillRunner::new(config);
        runner.add_baseline_suite(); // 3 scenarios

        // Make the first one fail
        let mut metrics_map = HashMap::new();
        metrics_map.insert(
            "dr-cold-start".to_string(),
            DrillMetrics {
                recovery_time_ms: 999_999,
                data_loss_window_ms: 999_999,
                completeness_permille: 0,
                ..Default::default()
            },
        );

        let report = runner.execute_with_metrics(&metrics_map);
        // Should have stopped after first failure
        assert_eq!(report.results.len(), 1);
        assert!(report.overall_verdict.is_fail());
    }

    #[test]
    fn report_summary_stats() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_baseline_suite();
        let report = runner.execute_all();

        assert!(report.summary.avg_recovery_time_ms > 0);
        assert!(report.summary.worst_recovery_time_ms > 0);
        assert!(report.summary.avg_completeness_permille > 0);
        assert!(report.generated_at_ms > 0);
    }

    #[test]
    fn verbose_mode_adds_notes() {
        let config = DrillConfig {
            verbose: true,
            ..Default::default()
        };
        let runner = DrillRunner::new(config);
        let result = runner.execute_scenario(&DrillScenario::cold_start());
        assert!(result.notes.iter().any(|n| n.contains("RTO")));
    }

    // ── ContinuityHealthCheck ──

    #[test]
    fn continuity_status_healthy() {
        assert!(ContinuityStatus::Healthy.is_healthy());
        assert!(!ContinuityStatus::Warning("warn".into()).is_healthy());
        assert!(!ContinuityStatus::Unhealthy("bad".into()).is_healthy());
        assert!(!ContinuityStatus::Unknown.is_healthy());
    }

    #[test]
    fn continuity_report_all_healthy() {
        let checks = vec![
            ContinuityHealthCheck {
                subsystem: "backup".into(),
                status: ContinuityStatus::Healthy,
                last_checked_ms: 1000,
                last_backup_ms: Some(900),
                restore_points: 5,
            },
            ContinuityHealthCheck {
                subsystem: "snapshots".into(),
                status: ContinuityStatus::Healthy,
                last_checked_ms: 1000,
                last_backup_ms: None,
                restore_points: 10,
            },
        ];
        let report = ContinuityReport::from_checks(checks);
        assert!(report.drill_ready);
        assert!(report.overall.is_healthy());
        assert_eq!(report.healthy_count(), 2);
        assert_eq!(report.total_restore_points(), 15);
    }

    #[test]
    fn continuity_report_with_unhealthy() {
        let checks = vec![
            ContinuityHealthCheck {
                subsystem: "backup".into(),
                status: ContinuityStatus::Healthy,
                last_checked_ms: 1000,
                last_backup_ms: Some(900),
                restore_points: 5,
            },
            ContinuityHealthCheck {
                subsystem: "protocol".into(),
                status: ContinuityStatus::Unhealthy("connection dead".into()),
                last_checked_ms: 1000,
                last_backup_ms: None,
                restore_points: 0,
            },
        ];
        let report = ContinuityReport::from_checks(checks);
        assert!(!report.drill_ready);
        assert!(matches!(report.overall, ContinuityStatus::Unhealthy(_)));
        assert_eq!(report.healthy_count(), 1);
    }

    #[test]
    fn continuity_report_all_unknown() {
        let checks = vec![ContinuityHealthCheck {
            subsystem: "test".into(),
            status: ContinuityStatus::Unknown,
            last_checked_ms: 0,
            last_backup_ms: None,
            restore_points: 0,
        }];
        let report = ContinuityReport::from_checks(checks);
        assert!(!report.drill_ready);
        assert!(matches!(report.overall, ContinuityStatus::Unknown));
    }

    #[test]
    fn continuity_report_with_warnings() {
        let checks = vec![ContinuityHealthCheck {
            subsystem: "snapshots".into(),
            status: ContinuityStatus::Warning("stale".into()),
            last_checked_ms: 1000,
            last_backup_ms: Some(100),
            restore_points: 2,
        }];
        let report = ContinuityReport::from_checks(checks);
        assert!(!report.drill_ready);
        assert!(matches!(report.overall, ContinuityStatus::Warning(_)));
    }

    // ── Serde roundtrips ──

    #[test]
    fn drill_scenario_serde_roundtrip() {
        let s = DrillScenario::cold_start();
        let json = serde_json::to_string(&s).unwrap();
        let back: DrillScenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, s.id);
        assert_eq!(back.kind, s.kind);
    }

    #[test]
    fn drill_report_serde_roundtrip() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scenario(DrillScenario::cold_start());
        let report = runner.execute_all();

        let json = serde_json::to_string(&report).unwrap();
        let back: DrillReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.results.len(), 1);
        assert_eq!(back.overall_verdict, report.overall_verdict);
    }

    #[test]
    fn continuity_report_serde_roundtrip() {
        let checks = vec![ContinuityHealthCheck {
            subsystem: "backup".into(),
            status: ContinuityStatus::Healthy,
            last_checked_ms: 1000,
            last_backup_ms: Some(900),
            restore_points: 5,
        }];
        let report = ContinuityReport::from_checks(checks);
        let json = serde_json::to_string(&report).unwrap();
        let back: ContinuityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.healthy_count(), 1);
    }

    // ── Scale drill ──

    #[test]
    fn scale_drills_pass() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scale_suite();
        let report = runner.execute_all();
        assert_eq!(report.results.len(), 4);
        assert!(report.overall_verdict.is_pass());
    }

    // ── DrillResult fields ──

    #[test]
    fn drill_result_fields() {
        let runner = DrillRunner::new(DrillConfig::default());
        let result = runner.execute_scenario(&DrillScenario::cold_start());
        assert_eq!(result.scenario_id, "dr-cold-start");
        assert_eq!(result.kind, DrillKind::ColdStart);
        assert!(result.rto_met.is_some());
        assert!(result.rpo_met.is_some());
        assert!(result.completeness_met);
    }

    // ── Custom scenario ──

    #[test]
    fn custom_scenario() {
        let s = DrillScenario {
            id: "custom-1".into(),
            name: "Custom Test".into(),
            kind: DrillKind::Custom("my-test".into()),
            rto: None,
            rpo: None,
            min_completeness_permille: 1000,
            tags: vec![],
        };
        let runner = DrillRunner::new(DrillConfig::default());
        let result = runner.execute_scenario(&s);
        assert!(result.verdict.is_pass());
        assert!(result.rto_met.is_none());
        assert!(result.rpo_met.is_none());
    }

    // ── Empty runner ──

    #[test]
    fn empty_runner_produces_pass_report() {
        let mut runner = DrillRunner::new(DrillConfig::default());
        let report = runner.execute_all();
        assert_eq!(report.results.len(), 0);
        assert!(report.overall_verdict.is_pass());
        assert_eq!(report.summary.total_drills, 0);
    }
}
