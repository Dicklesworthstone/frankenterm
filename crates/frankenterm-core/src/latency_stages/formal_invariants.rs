use super::{
    EnforcerSnapshot, LaneSchedulerConfig, LatencyStage, MitigationLevel, Percentile,
    RecoveryProtocol, SchedulerLane, SchedulerSnapshot, StageEnforcementState,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fmt;

// ── E1: Formal Specification Pack ──────────────────────────────────
//
// Formal invariant predicates for the scheduler, budget enforcer, and
// recovery protocol.  These types encode machine-checkable properties
// that MUST hold across all reachable states.  The InvariantChecker
// runtime validator evaluates them against live snapshots.

// ── E1.1 Invariant Domain ─────────────────────────────────────────

/// Domain to which a formal invariant belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InvariantDomain {
    /// Scheduler lane invariants (admission, ordering, starvation).
    Scheduler,
    /// Budget enforcement invariants (monotonicity, overflow, percentile order).
    Budget,
    /// Recovery protocol invariants (cooldown, escalation, de-escalation).
    Recovery,
    /// Cross-domain composition invariants.
    Composition,
}

impl fmt::Display for InvariantDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheduler => f.write_str("scheduler"),
            Self::Budget => f.write_str("budget"),
            Self::Recovery => f.write_str("recovery"),
            Self::Composition => f.write_str("composition"),
        }
    }
}

/// Severity of a formal invariant.  Critical invariants abort execution;
/// warning invariants emit diagnostics but continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InvariantSeverity {
    /// Informational — log only.
    Info,
    /// Warning — emit diagnostic, continue.
    Warning,
    /// Critical — must abort or rollback.
    Critical,
}

impl fmt::Display for InvariantSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warning => f.write_str("warning"),
            Self::Critical => f.write_str("critical"),
        }
    }
}

// ── E1.2 Formal Invariant Predicate ──────────────────────────────

/// A named, machine-checkable invariant predicate with domain and severity.
///
/// Each `FormalInvariant` encodes a single property that must hold.
/// The `predicate_id` is a stable identifier (e.g. "scheduler.no_starvation")
/// used for audit trails and counterexample references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormalInvariant {
    /// Stable identifier (dot-separated, e.g. "budget.percentile_monotonic").
    pub predicate_id: String,
    /// Human-readable description of the property.
    pub description: String,
    /// Domain this invariant belongs to.
    pub domain: InvariantDomain,
    /// Severity when violated.
    pub severity: InvariantSeverity,
    /// Whether this invariant is a safety property (must always hold)
    /// vs a liveness property (must eventually hold).
    pub is_safety: bool,
}

impl fmt::Display for FormalInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}:{}] {}",
            self.domain, self.severity, self.predicate_id
        )
    }
}

// ── E1.3 Scheduler Invariants ────────────────────────────────────

/// Formal invariants for the 3-lane scheduler.
///
/// These capture safety and liveness properties of the `LaneScheduler`:
/// - No item is lost (admitted items are tracked)
/// - Lane capacity is never exceeded
/// - Starvation freedom (bounded wait)
/// - Deterministic replay (same input → same schedule)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchedulerInvariant {
    /// Lane queue length never exceeds configured capacity.
    CapacityBound {
        lane: SchedulerLane,
        capacity: usize,
        actual: usize,
    },
    /// Total admitted items equals sum across all lanes.
    ConservationOfWork { total_admitted: u64, lane_sum: u64 },
    /// No lane has been starved beyond the max starvation threshold.
    StarvationFreedom {
        lane: SchedulerLane,
        wait_epochs: u64,
        max_epochs: u64,
    },
    /// Epoch counter is monotonically non-decreasing.
    EpochMonotonicity { previous: u64, current: u64 },
    /// Item IDs are strictly monotonically increasing.
    ItemIdMonotonicity { previous: u64, current: u64 },
    /// Determinism: identical input sequences produce identical decisions.
    DeterministicReplay {
        input_hash: u64,
        expected_hash: u64,
        actual_hash: u64,
    },
}

impl SchedulerInvariant {
    /// Check whether this invariant holds.
    pub fn holds(&self) -> bool {
        match self {
            Self::CapacityBound {
                capacity, actual, ..
            } => *actual <= *capacity,
            Self::ConservationOfWork {
                total_admitted,
                lane_sum,
            } => total_admitted == lane_sum,
            Self::StarvationFreedom {
                wait_epochs,
                max_epochs,
                ..
            } => wait_epochs <= max_epochs,
            Self::EpochMonotonicity { previous, current } => current >= previous,
            Self::ItemIdMonotonicity { previous, current } => {
                current > previous || (*previous == 0 && *current == 0)
            }
            Self::DeterministicReplay {
                expected_hash,
                actual_hash,
                ..
            } => expected_hash == actual_hash,
        }
    }

    /// The predicate ID for this invariant class.
    pub fn predicate_id(&self) -> &'static str {
        match self {
            Self::CapacityBound { .. } => "scheduler.capacity_bound",
            Self::ConservationOfWork { .. } => "scheduler.conservation_of_work",
            Self::StarvationFreedom { .. } => "scheduler.starvation_freedom",
            Self::EpochMonotonicity { .. } => "scheduler.epoch_monotonicity",
            Self::ItemIdMonotonicity { .. } => "scheduler.item_id_monotonicity",
            Self::DeterministicReplay { .. } => "scheduler.deterministic_replay",
        }
    }
}

impl fmt::Display for SchedulerInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityBound {
                lane,
                capacity,
                actual,
            } => {
                write!(f, "capacity_bound({lane:?}): {actual}/{capacity}")
            }
            Self::ConservationOfWork {
                total_admitted,
                lane_sum,
            } => {
                write!(f, "conservation: total={total_admitted}, sum={lane_sum}")
            }
            Self::StarvationFreedom {
                lane,
                wait_epochs,
                max_epochs,
            } => {
                write!(f, "starvation({lane:?}): wait={wait_epochs}/{max_epochs}")
            }
            Self::EpochMonotonicity { previous, current } => {
                write!(f, "epoch_mono: {previous} -> {current}")
            }
            Self::ItemIdMonotonicity { previous, current } => {
                write!(f, "item_id_mono: {previous} -> {current}")
            }
            Self::DeterministicReplay {
                input_hash,
                expected_hash,
                actual_hash,
            } => {
                write!(
                    f,
                    "determinism(input={input_hash:#x}): expected={expected_hash:#x}, actual={actual_hash:#x}"
                )
            }
        }
    }
}

// ── E1.4 Budget Invariants ───────────────────────────────────────

/// Formal invariants for budget enforcement.
///
/// These capture correctness properties of `BudgetEnforcer` and `RuntimeEnforcer`:
/// - Percentile targets are monotonically ordered (p50 ≤ p95 ≤ p99 ≤ p999)
/// - Budget totals are non-negative
/// - Observation counts are consistent
/// - Enforcer escalation is monotonic within a single evaluation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BudgetInvariant {
    /// Percentile targets are monotonically non-decreasing for each stage.
    PercentileMonotonicity {
        stage: LatencyStage,
        p50: f64,
        p95: f64,
        p99: f64,
        p999: f64,
    },
    /// All budget targets are non-negative.
    NonNegativeTargets {
        stage: LatencyStage,
        min_target: f64,
    },
    /// Total observation count matches per-stage sums.
    ObservationConsistency { total: u64, per_stage_sum: u64 },
    /// Overflow count never exceeds total observation count.
    OverflowBound {
        overflow_count: u64,
        total_observations: u64,
    },
    /// Enforcer escalation within a single observation is monotonic
    /// (never jumps down during a single evaluate call).
    EscalationMonotonicity {
        stage: LatencyStage,
        previous_level: MitigationLevel,
        current_level: MitigationLevel,
    },
    /// Aggregate budget ceiling is >= sum of stage budgets at each percentile.
    AggregateCeiling {
        percentile: Percentile,
        aggregate_us: f64,
        stage_sum_us: f64,
    },
}

impl BudgetInvariant {
    /// Check whether this invariant holds.
    pub fn holds(&self) -> bool {
        match self {
            Self::PercentileMonotonicity {
                p50,
                p95,
                p99,
                p999,
                ..
            } => *p50 <= *p95 && *p95 <= *p99 && *p99 <= *p999,
            Self::NonNegativeTargets { min_target, .. } => *min_target >= 0.0,
            Self::ObservationConsistency {
                total,
                per_stage_sum,
            } => total == per_stage_sum,
            Self::OverflowBound {
                overflow_count,
                total_observations,
            } => overflow_count <= total_observations,
            Self::EscalationMonotonicity {
                previous_level,
                current_level,
                ..
            } => *current_level >= *previous_level,
            Self::AggregateCeiling {
                aggregate_us,
                stage_sum_us,
                ..
            } => *aggregate_us >= *stage_sum_us || (*aggregate_us - *stage_sum_us).abs() < 1e-6,
        }
    }

    /// The predicate ID for this invariant class.
    pub fn predicate_id(&self) -> &'static str {
        match self {
            Self::PercentileMonotonicity { .. } => "budget.percentile_monotonicity",
            Self::NonNegativeTargets { .. } => "budget.non_negative_targets",
            Self::ObservationConsistency { .. } => "budget.observation_consistency",
            Self::OverflowBound { .. } => "budget.overflow_bound",
            Self::EscalationMonotonicity { .. } => "budget.escalation_monotonicity",
            Self::AggregateCeiling { .. } => "budget.aggregate_ceiling",
        }
    }
}

impl fmt::Display for BudgetInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PercentileMonotonicity {
                stage,
                p50,
                p95,
                p99,
                p999,
            } => {
                write!(
                    f,
                    "pct_mono({stage}): p50={p50:.1} p95={p95:.1} p99={p99:.1} p999={p999:.1}"
                )
            }
            Self::NonNegativeTargets { stage, min_target } => {
                write!(f, "nonneg({stage}): min={min_target:.1}")
            }
            Self::ObservationConsistency {
                total,
                per_stage_sum,
            } => {
                write!(f, "obs_consistency: total={total}, sum={per_stage_sum}")
            }
            Self::OverflowBound {
                overflow_count,
                total_observations,
            } => {
                write!(f, "overflow_bound: {overflow_count}/{total_observations}")
            }
            Self::EscalationMonotonicity {
                stage,
                previous_level,
                current_level,
            } => {
                write!(f, "esc_mono({stage}): {previous_level} -> {current_level}")
            }
            Self::AggregateCeiling {
                percentile,
                aggregate_us,
                stage_sum_us,
            } => {
                write!(
                    f,
                    "agg_ceil({percentile}): agg={aggregate_us:.1} >= sum={stage_sum_us:.1}"
                )
            }
        }
    }
}

// ── E1.5 Recovery Invariants ─────────────────────────────────────

/// Formal invariants for the recovery protocol state machine.
///
/// Recovery must satisfy:
/// - Gradual de-escalation: each recovery step drops exactly one level
/// - Cooldown enforcement: recovery only after sufficient consecutive-ok
/// - Timeout enforcement: forced recovery after max_degraded_duration
/// - No spurious escalation during recovery window
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryInvariant {
    /// In gradual mode, recovery steps down exactly one MitigationLevel at a time.
    GradualDeescalation {
        previous_level: MitigationLevel,
        recovered_level: MitigationLevel,
    },
    /// Recovery only occurs after consecutive_ok >= cooldown_observations.
    CooldownEnforced {
        consecutive_ok: u64,
        cooldown_required: u64,
    },
    /// Forced recovery triggers after max_degraded_duration_us is exceeded.
    TimeoutRecovery {
        degraded_duration_us: u64,
        max_duration_us: u64,
        recovery_triggered: bool,
    },
    /// Escalation count is monotonically non-decreasing.
    EscalationCountMonotonic { previous: u64, current: u64 },
    /// Recovery count is monotonically non-decreasing.
    RecoveryCountMonotonic { previous: u64, current: u64 },
    /// Current mitigation level is within [None, Skip] range (valid enum range).
    LevelInRange { level: MitigationLevel },
}

impl RecoveryInvariant {
    /// Check whether this invariant holds.
    pub fn holds(&self) -> bool {
        match self {
            Self::GradualDeescalation {
                previous_level,
                recovered_level,
            } => {
                previous_level.severity() > 0
                    && recovered_level.severity() == previous_level.severity() - 1
            }
            Self::CooldownEnforced {
                consecutive_ok,
                cooldown_required,
            } => consecutive_ok >= cooldown_required,
            Self::TimeoutRecovery {
                degraded_duration_us,
                max_duration_us,
                recovery_triggered,
            } => {
                if *degraded_duration_us > *max_duration_us {
                    *recovery_triggered
                } else {
                    true // no constraint before timeout
                }
            }
            Self::EscalationCountMonotonic { previous, current } => current >= previous,
            Self::RecoveryCountMonotonic { previous, current } => current >= previous,
            Self::LevelInRange { level } => level.severity() <= 4,
        }
    }

    /// The predicate ID for this invariant class.
    pub fn predicate_id(&self) -> &'static str {
        match self {
            Self::GradualDeescalation { .. } => "recovery.gradual_deescalation",
            Self::CooldownEnforced { .. } => "recovery.cooldown_enforced",
            Self::TimeoutRecovery { .. } => "recovery.timeout_recovery",
            Self::EscalationCountMonotonic { .. } => "recovery.escalation_count_monotonic",
            Self::RecoveryCountMonotonic { .. } => "recovery.recovery_count_monotonic",
            Self::LevelInRange { .. } => "recovery.level_in_range",
        }
    }
}

impl fmt::Display for RecoveryInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GradualDeescalation {
                previous_level,
                recovered_level,
            } => {
                write!(f, "gradual: {previous_level} -> {recovered_level}")
            }
            Self::CooldownEnforced {
                consecutive_ok,
                cooldown_required,
            } => {
                write!(f, "cooldown: {consecutive_ok}/{cooldown_required}")
            }
            Self::TimeoutRecovery {
                degraded_duration_us,
                max_duration_us,
                recovery_triggered,
            } => {
                write!(
                    f,
                    "timeout: {degraded_duration_us}us/{max_duration_us}us triggered={recovery_triggered}"
                )
            }
            Self::EscalationCountMonotonic { previous, current } => {
                write!(f, "esc_mono: {previous} -> {current}")
            }
            Self::RecoveryCountMonotonic { previous, current } => {
                write!(f, "rec_mono: {previous} -> {current}")
            }
            Self::LevelInRange { level } => {
                write!(f, "level_range: {level}")
            }
        }
    }
}

// ── E1.6 Invariant Check Result ──────────────────────────────────

/// Outcome of evaluating a single formal invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvariantOutcome {
    /// Invariant holds.
    Satisfied,
    /// Invariant violated with a counterexample description.
    Violated { counterexample: String },
    /// Could not be evaluated (insufficient data or timeout).
    Inconclusive { reason: String },
}

impl fmt::Display for InvariantOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Satisfied => f.write_str("SATISFIED"),
            Self::Violated { counterexample } => write!(f, "VIOLATED: {counterexample}"),
            Self::Inconclusive { reason } => write!(f, "INCONCLUSIVE: {reason}"),
        }
    }
}

/// Result of checking one invariant, with timing and context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantCheckResult {
    /// The predicate ID that was checked.
    pub predicate_id: String,
    /// Domain of the invariant.
    pub domain: InvariantDomain,
    /// Severity of the invariant.
    pub severity: InvariantSeverity,
    /// Check outcome.
    pub outcome: InvariantOutcome,
    /// Evaluation time in microseconds.
    pub eval_time_us: u64,
    /// Timestamp when the check was performed (epoch μs).
    pub timestamp_us: u64,
}

impl InvariantCheckResult {
    /// Whether the check passed.
    pub fn passed(&self) -> bool {
        self.outcome == InvariantOutcome::Satisfied
    }

    /// Whether the check found a violation.
    pub fn violated(&self) -> bool {
        matches!(self.outcome, InvariantOutcome::Violated { .. })
    }
}

impl fmt::Display for InvariantCheckResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}:{}] {} ({}μs)",
            self.domain, self.severity, self.outcome, self.eval_time_us
        )
    }
}

// ── E1.7 Invariant Checker ───────────────────────────────────────

/// Configuration for the runtime invariant checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantCheckerConfig {
    /// Maximum evaluation time per invariant (μs) before marking inconclusive.
    pub max_eval_time_us: u64,
    /// Whether to abort on critical violations.
    pub abort_on_critical: bool,
    /// Maximum results to retain in history.
    pub max_history: usize,
    /// Domains to check (empty = all).
    pub enabled_domains: Vec<InvariantDomain>,
}

impl Default for InvariantCheckerConfig {
    fn default() -> Self {
        Self {
            max_eval_time_us: 10_000, // 10ms
            abort_on_critical: true,
            max_history: 500,
            enabled_domains: Vec::new(), // all
        }
    }
}

/// Runtime invariant checker that evaluates formal predicates against live state.
///
/// The checker maintains a registry of `FormalInvariant` definitions and
/// evaluates `SchedulerInvariant`, `BudgetInvariant`, and `RecoveryInvariant`
/// instances against them.  Results are stored for audit and diagnostics.
#[derive(Debug, Clone)]
pub struct InvariantChecker {
    config: InvariantCheckerConfig,
    invariants: Vec<FormalInvariant>,
    results: VecDeque<InvariantCheckResult>,
    total_checks: u64,
    total_violations: u64,
    total_satisfied: u64,
}

impl InvariantChecker {
    /// Create a new checker with the given configuration.
    pub fn new(config: InvariantCheckerConfig) -> Self {
        Self {
            config,
            invariants: Vec::new(),
            results: VecDeque::new(),
            total_checks: 0,
            total_violations: 0,
            total_satisfied: 0,
        }
    }

    /// Create a checker with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(InvariantCheckerConfig::default())
    }

    /// Register a formal invariant definition.
    pub fn register(&mut self, inv: FormalInvariant) {
        self.invariants.push(inv);
    }

    /// Number of registered invariant definitions.
    pub fn registered_count(&self) -> usize {
        self.invariants.len()
    }

    /// Check a scheduler invariant.
    pub fn check_scheduler(
        &mut self,
        inv: &SchedulerInvariant,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let holds = inv.holds();
        let outcome = if holds {
            InvariantOutcome::Satisfied
        } else {
            InvariantOutcome::Violated {
                counterexample: format!("{inv}"),
            }
        };
        self.record_result(
            inv.predicate_id(),
            InvariantDomain::Scheduler,
            InvariantSeverity::Critical,
            outcome,
            timestamp_us,
        )
    }

    /// Check a budget invariant.
    pub fn check_budget(
        &mut self,
        inv: &BudgetInvariant,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let holds = inv.holds();
        let outcome = if holds {
            InvariantOutcome::Satisfied
        } else {
            InvariantOutcome::Violated {
                counterexample: format!("{inv}"),
            }
        };
        self.record_result(
            inv.predicate_id(),
            InvariantDomain::Budget,
            InvariantSeverity::Critical,
            outcome,
            timestamp_us,
        )
    }

    /// Check a recovery invariant.
    pub fn check_recovery(
        &mut self,
        inv: &RecoveryInvariant,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let holds = inv.holds();
        let outcome = if holds {
            InvariantOutcome::Satisfied
        } else {
            InvariantOutcome::Violated {
                counterexample: format!("{inv}"),
            }
        };
        self.record_result(
            inv.predicate_id(),
            InvariantDomain::Recovery,
            InvariantSeverity::Critical,
            outcome,
            timestamp_us,
        )
    }

    fn record_result(
        &mut self,
        predicate_id: &str,
        domain: InvariantDomain,
        severity: InvariantSeverity,
        outcome: InvariantOutcome,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let result = InvariantCheckResult {
            predicate_id: predicate_id.to_string(),
            domain,
            severity,
            outcome,
            eval_time_us: 0, // filled by caller if instrumented
            timestamp_us,
        };
        self.total_checks += 1;
        if result.passed() {
            self.total_satisfied += 1;
        }
        if result.violated() {
            self.total_violations += 1;
        }
        if self.results.len() >= self.config.max_history {
            self.results.pop_front();
        }
        self.results.push_back(result.clone());
        result
    }

    /// Total checks performed.
    pub fn total_checks(&self) -> u64 {
        self.total_checks
    }

    /// Total violations found.
    pub fn total_violations(&self) -> u64 {
        self.total_violations
    }

    /// Total satisfied checks.
    pub fn total_satisfied(&self) -> u64 {
        self.total_satisfied
    }

    /// Violation rate (0.0–1.0).
    pub fn violation_rate(&self) -> f64 {
        if self.total_checks == 0 {
            0.0
        } else {
            self.total_violations as f64 / self.total_checks as f64
        }
    }

    /// Most recent results (up to `n`).
    pub fn recent_results(&self, n: usize) -> Vec<InvariantCheckResult> {
        let start = self.results.len().saturating_sub(n);
        self.results.iter().skip(start).cloned().collect()
    }

    /// Results filtered by domain.
    pub fn results_by_domain(&self, domain: InvariantDomain) -> Vec<&InvariantCheckResult> {
        self.results.iter().filter(|r| r.domain == domain).collect()
    }

    /// All violation results.
    pub fn violations(&self) -> Vec<&InvariantCheckResult> {
        self.results.iter().filter(|r| r.violated()).collect()
    }

    /// State snapshot.
    pub fn snapshot(&self) -> InvariantCheckerSnapshot {
        InvariantCheckerSnapshot {
            total_checks: self.total_checks,
            total_violations: self.total_violations,
            total_satisfied: self.total_satisfied,
            registered_count: self.invariants.len(),
            history_len: self.results.len(),
            violation_rate: self.violation_rate(),
        }
    }

    /// Status line for display.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "invariants: checks={} ok={} violations={} rate={:.4}",
            snap.total_checks, snap.total_satisfied, snap.total_violations, snap.violation_rate
        )
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.results.clear();
        self.total_checks = 0;
        self.total_violations = 0;
        self.total_satisfied = 0;
    }

    /// Detect degradation in the invariant checker itself.
    pub fn detect_degradation(&self) -> InvariantCheckerDegradation {
        if self.total_checks == 0 {
            return InvariantCheckerDegradation::Healthy;
        }
        let rate = self.violation_rate();
        if rate > 0.1 {
            InvariantCheckerDegradation::HighViolationRate {
                violations: self.total_violations,
                total: self.total_checks,
            }
        } else if rate > 0.0 {
            InvariantCheckerDegradation::ViolationsDetected {
                violations: self.total_violations,
                total: self.total_checks,
            }
        } else {
            InvariantCheckerDegradation::Healthy
        }
    }

    /// Structured log entry.
    pub fn log_entry(&self) -> InvariantCheckerLogEntry {
        InvariantCheckerLogEntry {
            total_checks: self.total_checks,
            total_violations: self.total_violations,
            total_satisfied: self.total_satisfied,
            violation_rate: self.violation_rate(),
            degradation: self.detect_degradation(),
        }
    }
}

/// State snapshot for the invariant checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantCheckerSnapshot {
    pub total_checks: u64,
    pub total_violations: u64,
    pub total_satisfied: u64,
    pub registered_count: usize,
    pub history_len: usize,
    pub violation_rate: f64,
}

/// Degradation state for the invariant checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvariantCheckerDegradation {
    /// No violations detected.
    Healthy,
    /// Some violations but rate is low (≤10%).
    ViolationsDetected { violations: u64, total: u64 },
    /// High violation rate (>10%).
    HighViolationRate { violations: u64, total: u64 },
}

impl fmt::Display for InvariantCheckerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::ViolationsDetected { violations, total } => {
                write!(f, "violations({violations}/{total})")
            }
            Self::HighViolationRate { violations, total } => {
                write!(f, "high_rate({violations}/{total})")
            }
        }
    }
}

/// Structured log entry for the invariant checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantCheckerLogEntry {
    pub total_checks: u64,
    pub total_violations: u64,
    pub total_satisfied: u64,
    pub violation_rate: f64,
    pub degradation: InvariantCheckerDegradation,
}

// ── E1 Impl: Bridge Methods and Convenience API ──────────────────

impl InvariantChecker {
    /// Check a batch of scheduler invariants, returning all results.
    pub fn check_scheduler_batch(
        &mut self,
        invariants: &[SchedulerInvariant],
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        invariants
            .iter()
            .map(|inv| self.check_scheduler(inv, timestamp_us))
            .collect()
    }

    /// Check a batch of budget invariants, returning all results.
    pub fn check_budget_batch(
        &mut self,
        invariants: &[BudgetInvariant],
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        invariants
            .iter()
            .map(|inv| self.check_budget(inv, timestamp_us))
            .collect()
    }

    /// Check a batch of recovery invariants, returning all results.
    pub fn check_recovery_batch(
        &mut self,
        invariants: &[RecoveryInvariant],
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        invariants
            .iter()
            .map(|inv| self.check_recovery(inv, timestamp_us))
            .collect()
    }

    /// Extract and check scheduler invariants from a `SchedulerSnapshot`.
    pub fn check_from_scheduler_snapshot(
        &mut self,
        snap: &SchedulerSnapshot,
        config: &LaneSchedulerConfig,
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        let mut results = Vec::new();
        // Check capacity bounds for each lane
        for ls in &snap.lanes {
            let capacity = match ls.lane {
                SchedulerLane::Input => config.input_queue_capacity,
                SchedulerLane::Control => config.control_queue_capacity,
                SchedulerLane::Bulk => config.bulk_queue_capacity,
            };
            let inv = SchedulerInvariant::CapacityBound {
                lane: ls.lane,
                capacity,
                actual: ls.depth,
            };
            results.push(self.check_scheduler(&inv, timestamp_us));
        }
        // Check conservation of work
        let lane_sum: u64 = snap.lanes.iter().map(|ls| ls.total_admitted).sum();
        let total = snap.total_items_processed;
        let inv = SchedulerInvariant::ConservationOfWork {
            total_admitted: total,
            lane_sum,
        };
        results.push(self.check_scheduler(&inv, timestamp_us));
        results
    }

    /// Extract and check budget invariants from an `EnforcerSnapshot`.
    pub fn check_from_enforcer_snapshot(
        &mut self,
        snap: &EnforcerSnapshot,
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        let mut results = Vec::new();
        // Check overflow bound
        let inv = BudgetInvariant::OverflowBound {
            overflow_count: snap.total_overflows,
            total_observations: snap.total_observations,
        };
        results.push(self.check_budget(&inv, timestamp_us));
        // Check overflow bound per stage
        let total_overflows: u64 = snap.stages.iter().map(|s| s.overflow_count).sum();
        let inv = BudgetInvariant::OverflowBound {
            overflow_count: total_overflows,
            total_observations: snap.total_observations,
        };
        results.push(self.check_budget(&inv, timestamp_us));
        results
    }

    /// Extract and check recovery invariants from a `StageEnforcementState`.
    pub fn check_from_enforcement_state(
        &mut self,
        state: &StageEnforcementState,
        previous_state: &StageEnforcementState,
        recovery_protocol: &RecoveryProtocol,
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        let mut results = Vec::new();
        // Escalation count monotonicity
        let inv = RecoveryInvariant::EscalationCountMonotonic {
            previous: previous_state.escalation_count,
            current: state.escalation_count,
        };
        results.push(self.check_recovery(&inv, timestamp_us));
        // Recovery count monotonicity
        let inv = RecoveryInvariant::RecoveryCountMonotonic {
            previous: previous_state.recovery_count,
            current: state.recovery_count,
        };
        results.push(self.check_recovery(&inv, timestamp_us));
        // Level in range
        let inv = RecoveryInvariant::LevelInRange {
            level: state.current_level,
        };
        results.push(self.check_recovery(&inv, timestamp_us));
        // If recovery happened (level decreased), check gradual de-escalation
        if state.current_level < previous_state.current_level && recovery_protocol.gradual {
            let inv = RecoveryInvariant::GradualDeescalation {
                previous_level: previous_state.current_level,
                recovered_level: state.current_level,
            };
            results.push(self.check_recovery(&inv, timestamp_us));
        }
        // If recovery happened, check cooldown
        if state.current_level < previous_state.current_level {
            let inv = RecoveryInvariant::CooldownEnforced {
                consecutive_ok: state.consecutive_ok,
                cooldown_required: recovery_protocol.cooldown_observations,
            };
            results.push(self.check_recovery(&inv, timestamp_us));
        }
        results
    }

    /// Run all domain checks and return true only if zero violations found.
    pub fn all_satisfied(&self) -> bool {
        self.total_violations == 0
    }

    /// Count violations in a specific domain.
    pub fn violation_count_by_domain(&self, domain: InvariantDomain) -> usize {
        self.results
            .iter()
            .filter(|r| r.domain == domain && r.violated())
            .count()
    }

    /// Get the most recent violation (if any).
    pub fn last_violation(&self) -> Option<&InvariantCheckResult> {
        self.results.iter().rev().find(|r| r.violated())
    }

    /// Check whether a specific predicate has ever been violated.
    pub fn predicate_ever_violated(&self, predicate_id: &str) -> bool {
        self.results
            .iter()
            .any(|r| r.predicate_id == predicate_id && r.violated())
    }

    /// Get pass rate for a specific predicate (0.0–1.0, NaN if never checked).
    pub fn predicate_pass_rate(&self, predicate_id: &str) -> f64 {
        let matching: Vec<_> = self
            .results
            .iter()
            .filter(|r| r.predicate_id == predicate_id)
            .collect();
        if matching.is_empty() {
            return f64::NAN;
        }
        let passed = matching.iter().filter(|r| r.passed()).count();
        passed as f64 / matching.len() as f64
    }

    /// Get all unique predicate IDs that have been checked.
    pub fn checked_predicates(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut predicates = Vec::new();
        for r in &self.results {
            if seen.insert(r.predicate_id.clone()) {
                predicates.push(r.predicate_id.clone());
            }
        }
        predicates
    }

    /// Summary of checks grouped by domain.
    pub fn domain_summary(&self) -> Vec<(InvariantDomain, u64, u64)> {
        let domains = [
            InvariantDomain::Scheduler,
            InvariantDomain::Budget,
            InvariantDomain::Recovery,
            InvariantDomain::Composition,
        ];
        domains
            .iter()
            .map(|d| {
                let total = self.results.iter().filter(|r| r.domain == *d).count() as u64;
                let violations = self
                    .results
                    .iter()
                    .filter(|r| r.domain == *d && r.violated())
                    .count() as u64;
                (*d, total, violations)
            })
            .collect()
    }
}
