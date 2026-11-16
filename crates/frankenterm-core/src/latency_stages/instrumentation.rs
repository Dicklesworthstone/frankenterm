//! Pipeline correlation, probe overhead tracking, and instrumented enforcer diagnostics.

use serde::{Deserialize, Serialize};
use std::fmt;

use super::{
    BudgetEnforcer, BudgetEnforcerConfig, EnforcerSnapshot, LatencyStage, Mitigation,
    ObservationResult, PipelineRun, StageObservation,
};

/// Maximum overflow rate (overflow_runs / completed_runs) at which the
/// `InstrumentedEnforcer` is still considered healthy. Above this the
/// `is_healthy()` check returns false. Operators that need stricter
/// SLO bands can compose their own predicate against
/// `overflow_rate()` and `current_degradation()`; the default tracks
/// the historic 10% acceptance band.
pub const HEALTHY_OVERFLOW_RATE_THRESHOLD: f64 = 0.10;

/// A correlation context that propagates across async boundaries.
///
/// Created at the start of a pipeline run and threaded through all
/// stages. Each stage records its start/end timestamps and the
/// context carries accumulated timing data.
///
/// # AARSP Bead: ft-2p9cb.1.2
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrelationContext {
    /// Unique run identifier.
    pub run_id: String,
    /// Correlation ID (same as run_id unless explicitly set).
    pub correlation_id: String,
    /// Scenario ID for deterministic replay.
    pub scenario_id: Option<String>,
    /// Accumulated per-stage timing entries.
    pub timings: Vec<StageTiming>,
    /// The next expected stage in the pipeline.
    pub next_expected: Option<LatencyStage>,
    /// Whether the context was propagated correctly (no gaps).
    pub propagation_intact: bool,
    /// Creation timestamp (epoch microseconds, provided by caller).
    pub created_at_us: u64,
}

impl CorrelationContext {
    /// Create a new correlation context for a pipeline run.
    pub fn new(run_id: &str, created_at_us: u64) -> Self {
        Self {
            run_id: run_id.to_string(),
            correlation_id: run_id.to_string(),
            scenario_id: None,
            timings: Vec::with_capacity(LatencyStage::PIPELINE_STAGES.len()),
            next_expected: Some(LatencyStage::PIPELINE_STAGES[0]),
            propagation_intact: true,
            created_at_us,
        }
    }

    /// Create with an explicit correlation ID.
    pub fn with_correlation(run_id: &str, correlation_id: &str, created_at_us: u64) -> Self {
        Self {
            run_id: run_id.to_string(),
            correlation_id: correlation_id.to_string(),
            scenario_id: None,
            timings: Vec::with_capacity(LatencyStage::PIPELINE_STAGES.len()),
            next_expected: Some(LatencyStage::PIPELINE_STAGES[0]),
            propagation_intact: true,
            created_at_us,
        }
    }

    /// Record the start of a stage. Returns a StageProbe for timing.
    ///
    /// # Propagation Check
    /// If the stage doesn't match `next_expected`, a gap is recorded
    /// and `propagation_intact` is set to false. If the pipeline has
    /// already completed (i.e. `next_expected == None` because every
    /// `PIPELINE_STAGES` entry has been observed), any further stage
    /// is also a propagation violation — recording past the end of
    /// the pipeline is just as much a gap as starting in the middle.
    pub fn begin_stage(&mut self, stage: LatencyStage, start_us: u64) -> StageProbe {
        match self.next_expected {
            Some(expected) if stage == expected => {}
            // Either the wrong stage was begun, or the pipeline already
            // ran to completion and a stage was begun anyway. Both are
            // out-of-order recordings.
            Some(_) | None => self.propagation_intact = false,
        }

        StageProbe {
            stage,
            start_us,
            correlation_id: self.correlation_id.clone(),
        }
    }

    /// Record the completion of a stage.
    ///
    /// Computes latency and updates the correlation chain.
    pub fn end_stage(&mut self, probe: StageProbe, end_us: u64) {
        let latency_us = if end_us >= probe.start_us {
            (end_us - probe.start_us) as f64
        } else {
            0.0 // Clock regression — treat as zero.
        };

        self.timings.push(StageTiming {
            stage: probe.stage,
            start_us: probe.start_us,
            end_us,
            latency_us,
        });

        // Advance expected stage.
        self.next_expected = Self::next_stage_after(probe.stage);
    }

    /// Convert to a PipelineRun for the BudgetEnforcer.
    pub fn to_pipeline_run(&self) -> PipelineRun {
        let observations: Vec<StageObservation> = self
            .timings
            .iter()
            .map(|t| StageObservation {
                stage: t.stage,
                latency_us: t.latency_us,
                correlation_id: self.correlation_id.clone(),
                scenario_id: self.scenario_id.clone(),
                start_epoch_us: t.start_us,
                end_epoch_us: t.end_us,
                overflow: false, // Will be filled by enforcer.
                reason: None,
                mitigation: Mitigation::None,
            })
            .collect();

        let total: f64 = observations.iter().map(|o| o.latency_us).sum();
        PipelineRun {
            run_id: self.run_id.clone(),
            correlation_id: self.correlation_id.clone(),
            scenario_id: self.scenario_id.clone(),
            stages: observations,
            total_latency_us: total,
            has_overflow: false,
            reasons: vec![],
        }
    }

    /// Get total elapsed time from first stage start to last stage end.
    pub fn total_elapsed_us(&self) -> u64 {
        if self.timings.is_empty() {
            return 0;
        }
        let first_start = self.timings.first().map(|t| t.start_us).unwrap_or(0);
        let last_end = self.timings.last().map(|t| t.end_us).unwrap_or(0);
        last_end.saturating_sub(first_start)
    }

    /// Get the number of stages recorded.
    pub fn stage_count(&self) -> usize {
        self.timings.len()
    }

    /// Check for missing stages in the pipeline.
    pub fn missing_stages(&self) -> Vec<LatencyStage> {
        let recorded: std::collections::HashSet<LatencyStage> =
            self.timings.iter().map(|t| t.stage).collect();
        LatencyStage::PIPELINE_STAGES
            .iter()
            .filter(|s| !recorded.contains(s))
            .copied()
            .collect()
    }

    fn next_stage_after(stage: LatencyStage) -> Option<LatencyStage> {
        let stages = LatencyStage::PIPELINE_STAGES;
        let pos = stages.iter().position(|&s| s == stage)?;
        stages.get(pos + 1).copied()
    }
}

/// A timing probe for a single stage.
///
/// Created by `CorrelationContext::begin_stage()`, consumed by `end_stage()`.
/// Carries the stage identity and start timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageProbe {
    /// Which stage is being timed.
    pub stage: LatencyStage,
    /// Start timestamp in epoch microseconds.
    pub start_us: u64,
    /// Correlation ID from the context.
    pub correlation_id: String,
}

/// Timing data for a completed stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: LatencyStage,
    pub start_us: u64,
    pub end_us: u64,
    pub latency_us: f64,
}

/// Overhead tracker for instrumentation itself.
///
/// Measures how much time the instrumentation probes add to the pipeline.
/// This is essential for proving the "bounded overhead" acceptance criterion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentationOverhead {
    /// Cumulative overhead from begin_stage/end_stage calls (microseconds).
    pub total_overhead_us: f64,
    /// Number of probe pairs measured.
    pub probe_count: u64,
    /// Mean overhead per probe pair.
    pub mean_overhead_us: f64,
    /// Maximum observed overhead.
    pub max_overhead_us: f64,
    /// Budget: maximum allowed overhead per probe pair (default 1μs).
    pub budget_per_probe_us: f64,
    /// Whether overhead is within budget.
    pub within_budget: bool,
}

impl InstrumentationOverhead {
    /// Create a new overhead tracker with default 1μs per-probe budget.
    pub fn new() -> Self {
        Self {
            total_overhead_us: 0.0,
            probe_count: 0,
            mean_overhead_us: 0.0,
            max_overhead_us: 0.0,
            budget_per_probe_us: 1.0,
            within_budget: true,
        }
    }

    /// Record a probe's overhead.
    pub fn record(&mut self, overhead_us: f64) {
        let budget_is_valid =
            self.budget_per_probe_us.is_finite() && self.budget_per_probe_us >= 0.0;
        let budget_per_probe_us = if budget_is_valid {
            self.budget_per_probe_us
        } else {
            0.0
        };
        let overhead_us = if overhead_us.is_finite() {
            overhead_us.max(0.0)
        } else {
            budget_per_probe_us + 1.0
        };

        self.total_overhead_us += overhead_us;
        self.probe_count += 1;
        self.mean_overhead_us = self.total_overhead_us / self.probe_count as f64;
        if overhead_us > self.max_overhead_us {
            self.max_overhead_us = overhead_us;
        }
        self.within_budget = budget_is_valid && self.max_overhead_us <= budget_per_probe_us;
    }

    /// Get the overhead as a fraction of total pipeline time.
    pub fn overhead_fraction(&self, total_pipeline_us: f64) -> f64 {
        if total_pipeline_us <= 0.0 {
            return 0.0;
        }
        self.total_overhead_us / total_pipeline_us
    }
}

impl Default for InstrumentationOverhead {
    fn default() -> Self {
        Self::new()
    }
}

/// Extended enforcer that combines budget enforcement with correlation tracking.
///
/// Provides a high-level API for instrumenting pipeline runs end-to-end.
#[derive(Debug, Clone)]
pub struct InstrumentedEnforcer {
    enforcer: BudgetEnforcer,
    overhead: InstrumentationOverhead,
    completed_runs: u64,
    overflow_runs: u64,
}

impl InstrumentedEnforcer {
    /// Create with default configuration.
    pub fn new() -> Self {
        Self {
            enforcer: BudgetEnforcer::with_defaults(),
            overhead: InstrumentationOverhead::new(),
            completed_runs: 0,
            overflow_runs: 0,
        }
    }

    /// Create with custom configuration.
    pub fn with_config(config: BudgetEnforcerConfig) -> Self {
        Self {
            enforcer: BudgetEnforcer::new(config),
            overhead: InstrumentationOverhead::new(),
            completed_runs: 0,
            overflow_runs: 0,
        }
    }

    /// Process a completed correlation context through the enforcer.
    ///
    /// Records each stage timing and returns per-stage results.
    pub fn process_run(&mut self, ctx: &CorrelationContext) -> Vec<ObservationResult> {
        let mut results = Vec::with_capacity(ctx.timings.len());
        let mut any_overflow = false;

        for timing in &ctx.timings {
            let result = self
                .enforcer
                .record(timing.stage, timing.latency_us, &ctx.correlation_id);
            if result.overflow {
                any_overflow = true;
            }
            results.push(result);
        }

        self.completed_runs += 1;
        if any_overflow {
            self.overflow_runs += 1;
        }

        results
    }

    /// Record instrumentation overhead for a probe pair.
    pub fn record_overhead(&mut self, overhead_us: f64) {
        self.overhead.record(overhead_us);
    }

    /// Get the underlying enforcer for snapshot/diagnostics.
    pub fn enforcer(&self) -> &BudgetEnforcer {
        &self.enforcer
    }

    /// Get the overhead tracker.
    pub fn overhead(&self) -> &InstrumentationOverhead {
        &self.overhead
    }

    /// Get statistics.
    pub fn completed_runs(&self) -> u64 {
        self.completed_runs
    }

    pub fn overflow_runs(&self) -> u64 {
        self.overflow_runs
    }

    /// Overflow rate as fraction of completed runs.
    pub fn overflow_rate(&self) -> f64 {
        if self.completed_runs == 0 {
            return 0.0;
        }
        self.overflow_runs as f64 / self.completed_runs as f64
    }
}

impl Default for InstrumentedEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

// ── Guardrails ─────────────────────────────────────────────────────

/// Validation errors for instrumentation inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InstrumentationError {
    /// Stage was started but never ended.
    UnterminatedProbe { stage: LatencyStage, start_us: u64 },
    /// Stage was ended without a matching begin.
    OrphanedEnd { stage: LatencyStage },
    /// Clock regression detected (end < start).
    ClockRegression {
        stage: LatencyStage,
        start_us: u64,
        end_us: u64,
    },
    /// Duplicate stage in a single run.
    DuplicateStage { stage: LatencyStage },
    /// Empty run (no stages recorded).
    EmptyRun { run_id: String },
    /// Overhead budget exceeded.
    OverheadBudgetExceeded {
        max_observed_us: f64,
        budget_us: f64,
    },
}

impl fmt::Display for InstrumentationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedProbe { stage, start_us } => {
                write!(f, "Unterminated probe for {stage} started at {start_us}μs")
            }
            Self::OrphanedEnd { stage } => {
                write!(f, "Orphaned end_stage for {stage} (no matching begin)")
            }
            Self::ClockRegression {
                stage,
                start_us,
                end_us,
            } => write!(
                f,
                "Clock regression at {stage}: start={start_us}μs > end={end_us}μs"
            ),
            Self::DuplicateStage { stage } => {
                write!(f, "Duplicate stage {stage} in single run")
            }
            Self::EmptyRun { run_id } => {
                write!(f, "Empty run {run_id} has no stages")
            }
            Self::OverheadBudgetExceeded {
                max_observed_us,
                budget_us,
            } => write!(
                f,
                "Overhead budget exceeded: observed={max_observed_us:.2}μs > budget={budget_us:.2}μs"
            ),
        }
    }
}

impl std::error::Error for InstrumentationError {}

/// Degradation level for instrumentation failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InstrumentationDegradation {
    /// Full instrumentation active.
    Full,
    /// Overhead tracking disabled to reduce cost.
    SkipOverhead,
    /// Correlation propagation disabled.
    SkipCorrelation,
    /// All instrumentation disabled — raw enforcer only.
    Passthrough,
}

impl fmt::Display for InstrumentationDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("FULL"),
            Self::SkipOverhead => f.write_str("SKIP_OVERHEAD"),
            Self::SkipCorrelation => f.write_str("SKIP_CORRELATION"),
            Self::Passthrough => f.write_str("PASSTHROUGH"),
        }
    }
}

// ── Validated Correlation Context ──────────────────────────────────

impl CorrelationContext {
    /// Validate the completed context for correctness.
    ///
    /// Returns a list of all detected issues. Empty list means valid.
    pub fn validate(&self) -> Vec<InstrumentationError> {
        let mut errors = Vec::new();

        if self.timings.is_empty() {
            errors.push(InstrumentationError::EmptyRun {
                run_id: self.run_id.clone(),
            });
            return errors;
        }

        // Check for duplicate stages.
        let mut seen = std::collections::HashSet::new();
        for timing in &self.timings {
            if !seen.insert(timing.stage) {
                errors.push(InstrumentationError::DuplicateStage {
                    stage: timing.stage,
                });
            }
        }

        // Check for clock regressions.
        for timing in &self.timings {
            if timing.end_us < timing.start_us {
                errors.push(InstrumentationError::ClockRegression {
                    stage: timing.stage,
                    start_us: timing.start_us,
                    end_us: timing.end_us,
                });
            }
        }

        // Check timestamp ordering between stages.
        for window in self.timings.windows(2) {
            if window[1].start_us < window[0].end_us {
                // Overlap detected — could indicate a gap in propagation
                // but not necessarily an error (parallel stages).
                // We only flag clock regression within a single stage.
            }
        }

        errors
    }

    /// Validate and return Ok(self) or Err(errors).
    pub fn validated(self) -> Result<Self, Vec<InstrumentationError>> {
        let errors = self.validate();
        if errors.is_empty() {
            Ok(self)
        } else {
            Err(errors)
        }
    }
}

// ── Diagnostic Dump ─────────────────────────────────────────────────

/// Full diagnostic snapshot of the instrumented pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentationDiagnostic {
    /// Current degradation level.
    pub degradation: InstrumentationDegradation,
    /// Enforcer snapshot (per-stage percentiles, slack, overflow counts).
    pub enforcer: EnforcerSnapshot,
    /// Overhead tracker state.
    pub overhead: InstrumentationOverhead,
    /// Total completed runs.
    pub completed_runs: u64,
    /// Total runs with at least one overflow.
    pub overflow_runs: u64,
    /// Overflow rate.
    pub overflow_rate: f64,
    /// Validation errors from the most recent run (if any).
    pub last_validation_errors: Vec<InstrumentationError>,
}

impl InstrumentedEnforcer {
    /// Get full diagnostic snapshot.
    pub fn diagnostic(&self) -> InstrumentationDiagnostic {
        InstrumentationDiagnostic {
            degradation: self.current_degradation(),
            enforcer: self.enforcer.snapshot(),
            overhead: self.overhead.clone(),
            completed_runs: self.completed_runs,
            overflow_runs: self.overflow_runs,
            overflow_rate: self.overflow_rate(),
            last_validation_errors: Vec::new(),
        }
    }

    /// Determine current degradation level based on overhead.
    pub fn current_degradation(&self) -> InstrumentationDegradation {
        if !self.overhead.within_budget {
            if self.overhead.max_overhead_us > self.overhead.budget_per_probe_us * 10.0 {
                InstrumentationDegradation::Passthrough
            } else if self.overhead.max_overhead_us > self.overhead.budget_per_probe_us * 5.0 {
                InstrumentationDegradation::SkipCorrelation
            } else {
                InstrumentationDegradation::SkipOverhead
            }
        } else {
            InstrumentationDegradation::Full
        }
    }

    /// Process a run with validation. Returns results and any validation errors.
    pub fn process_validated_run(
        &mut self,
        ctx: &CorrelationContext,
    ) -> (Vec<ObservationResult>, Vec<InstrumentationError>) {
        let validation_errors = ctx.validate();
        let results = self.process_run(ctx);
        (results, validation_errors)
    }

    /// Health check: returns true if instrumentation is healthy.
    ///
    /// Healthy means: at least one completed run, overhead within
    /// budget, degradation is Full, and overflow rate is below
    /// [`HEALTHY_OVERFLOW_RATE_THRESHOLD`] (10%).
    pub fn is_healthy(&self) -> bool {
        self.completed_runs > 0
            && self.overhead.within_budget
            && self.current_degradation() == InstrumentationDegradation::Full
            && self.overflow_rate() < HEALTHY_OVERFLOW_RATE_THRESHOLD
    }

    /// Get a compact status string for operator dashboards.
    pub fn status_line(&self) -> String {
        format!(
            "degradation={} runs={} overflows={} rate={:.1}% overhead_max={:.2}μs",
            self.current_degradation(),
            self.completed_runs,
            self.overflow_runs,
            self.overflow_rate() * 100.0,
            self.overhead.max_overhead_us,
        )
    }
}

// ── Fast Path Probe ─────────────────────────────────────────────────

/// Lightweight probe for the fast path — no allocation, no correlation.
///
/// For high-frequency stages where full correlation context is too expensive.
/// Simply records a start timestamp and stage identity. Use `elapsed_us()` to
/// compute latency without any heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastProbe {
    pub stage: LatencyStage,
    pub start_us: u64,
}

impl FastProbe {
    /// Create a fast probe (zero allocation).
    pub fn begin(stage: LatencyStage, start_us: u64) -> Self {
        Self { stage, start_us }
    }

    /// Compute elapsed time. Returns 0 on clock regression.
    pub fn elapsed_us(self, end_us: u64) -> f64 {
        if end_us >= self.start_us {
            (end_us - self.start_us) as f64
        } else {
            0.0
        }
    }
}
