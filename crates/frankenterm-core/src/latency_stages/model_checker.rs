use super::{
    BudgetInvariant, InvariantCheckResult, InvariantChecker, InvariantDomain, LatencyStage,
    MitigationLevel, RecoveryInvariant, SchedulerInvariant, SchedulerLane,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

// ── E2: Model-Checking Harness and Counterexample Pipeline ────────
//
// Bounded model-checking for latency invariants.  The `ModelChecker`
// explores state space via systematic injection of observations and
// records counterexample traces when invariants are violated.

/// A single step in a model-checking trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceStep {
    /// Step index (0-based).
    pub step: u64,
    /// Action applied at this step.
    pub action: TraceAction,
    /// Invariant check results after the action.
    pub check_results: Vec<InvariantCheckResult>,
    /// Timestamp (epoch μs).
    pub timestamp_us: u64,
}

/// An action in the model-checking state space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TraceAction {
    /// Observe a latency value at a stage.
    ObserveLatency {
        stage: LatencyStage,
        latency_us: f64,
    },
    /// Admit a work item to the scheduler.
    SchedulerAdmit { lane: SchedulerLane, cost_us: f64 },
    /// Trigger recovery at a stage.
    RecoveryStep {
        level_before: MitigationLevel,
        level_after: MitigationLevel,
    },
    /// Advance the epoch.
    EpochAdvance { new_epoch: u64 },
    /// Reset a subsystem.
    Reset { domain: InvariantDomain },
}

impl fmt::Display for TraceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObserveLatency { stage, latency_us } => {
                write!(f, "observe({stage}, {latency_us:.1}μs)")
            }
            Self::SchedulerAdmit { lane, cost_us } => {
                write!(f, "admit({lane:?}, {cost_us:.1}μs)")
            }
            Self::RecoveryStep {
                level_before,
                level_after,
            } => {
                write!(f, "recover({level_before} -> {level_after})")
            }
            Self::EpochAdvance { new_epoch } => write!(f, "epoch({new_epoch})"),
            Self::Reset { domain } => write!(f, "reset({domain})"),
        }
    }
}

/// A counterexample: a sequence of steps that leads to an invariant violation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Counterexample {
    /// The predicate that was violated.
    pub predicate_id: String,
    /// Domain of the violated invariant.
    pub domain: InvariantDomain,
    /// The trace of steps leading to the violation.
    pub trace: Vec<TraceStep>,
    /// Human-readable description of the violation.
    pub description: String,
    /// Timestamp when the counterexample was found.
    pub found_at_us: u64,
}

impl fmt::Display for Counterexample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "counterexample[{}]: {} ({} steps)",
            self.predicate_id,
            self.description,
            self.trace.len()
        )
    }
}

/// Exploration strategy for the model checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorationStrategy {
    /// Breadth-first: explore all states at depth d before d+1.
    BreadthFirst,
    /// Random walk: pick random actions for N steps.
    RandomWalk,
    /// Guided: prioritize actions near known violation domains.
    Guided,
}

impl fmt::Display for ExplorationStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BreadthFirst => f.write_str("bfs"),
            Self::RandomWalk => f.write_str("random"),
            Self::Guided => f.write_str("guided"),
        }
    }
}

/// Configuration for the model checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCheckerConfig {
    /// Maximum depth (steps) to explore.
    pub max_depth: u64,
    /// Maximum total states to explore before stopping.
    pub max_states: u64,
    /// Exploration strategy.
    pub strategy: ExplorationStrategy,
    /// Maximum counterexamples to collect before stopping.
    pub max_counterexamples: usize,
    /// Whether to continue exploring after first counterexample.
    pub exhaustive: bool,
}

impl Default for ModelCheckerConfig {
    fn default() -> Self {
        Self {
            max_depth: 100,
            max_states: 10_000,
            strategy: ExplorationStrategy::RandomWalk,
            max_counterexamples: 10,
            exhaustive: false,
        }
    }
}

/// Snapshot of the model checker's exploration state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCheckerSnapshot {
    pub states_explored: u64,
    pub current_depth: u64,
    pub counterexamples_found: usize,
    pub invariants_checked: u64,
    pub violations_found: u64,
    pub strategy: ExplorationStrategy,
}

/// Result of a model-checking run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelCheckVerdict {
    /// No violations found within exploration bounds.
    NoViolation {
        states_explored: u64,
        depth_reached: u64,
    },
    /// Violations found.
    ViolationsFound {
        counterexamples: Vec<Counterexample>,
    },
    /// Exploration was terminated early (budget exhausted).
    Incomplete {
        states_explored: u64,
        reason: String,
    },
}

impl fmt::Display for ModelCheckVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoViolation {
                states_explored,
                depth_reached,
            } => {
                write!(
                    f,
                    "NO_VIOLATION ({states_explored} states, depth {depth_reached})"
                )
            }
            Self::ViolationsFound { counterexamples } => {
                write!(
                    f,
                    "VIOLATIONS_FOUND ({} counterexamples)",
                    counterexamples.len()
                )
            }
            Self::Incomplete {
                states_explored,
                reason,
            } => {
                write!(f, "INCOMPLETE ({states_explored} states): {reason}")
            }
        }
    }
}

/// The model checker explores state space and finds counterexamples.
#[derive(Debug, Clone)]
pub struct ModelChecker {
    config: ModelCheckerConfig,
    checker: InvariantChecker,
    counterexamples: Vec<Counterexample>,
    current_trace: Vec<TraceStep>,
    states_explored: u64,
    current_depth: u64,
    max_depth_reached: u64,
}

impl ModelChecker {
    /// Create a new model checker.
    pub fn new(config: ModelCheckerConfig) -> Self {
        Self {
            config,
            checker: InvariantChecker::with_defaults(),
            counterexamples: Vec::new(),
            current_trace: Vec::new(),
            states_explored: 0,
            current_depth: 0,
            max_depth_reached: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ModelCheckerConfig::default())
    }

    /// Record a trace step and check invariants.
    ///
    /// If any invariant is violated, a counterexample is captured.
    pub fn step(
        &mut self,
        action: TraceAction,
        invariants: &[InvariantCheckResult],
        timestamp_us: u64,
    ) -> bool {
        let step = TraceStep {
            step: self.current_depth,
            action,
            check_results: invariants.to_vec(),
            timestamp_us,
        };

        let has_violation = step.check_results.iter().any(|r| r.violated());

        self.current_trace.push(step);
        self.states_explored += 1;
        self.current_depth += 1;
        if self.current_depth > self.max_depth_reached {
            self.max_depth_reached = self.current_depth;
        }

        if has_violation {
            // Capture counterexample from current trace
            if let Some(violated) = invariants.iter().find(|r| r.violated()) {
                let cx = Counterexample {
                    predicate_id: violated.predicate_id.clone(),
                    domain: violated.domain,
                    trace: self.current_trace.clone(),
                    description: format!("{}", violated.outcome),
                    found_at_us: timestamp_us,
                };
                self.counterexamples.push(cx);
            }
        }

        has_violation
    }

    /// Start a new trace (reset current path without clearing counterexamples).
    pub fn new_trace(&mut self) {
        self.current_trace.clear();
        self.current_depth = 0;
    }

    /// Number of counterexamples found.
    pub fn counterexample_count(&self) -> usize {
        self.counterexamples.len()
    }

    /// States explored so far.
    pub fn states_explored(&self) -> u64 {
        self.states_explored
    }

    /// Maximum depth reached.
    pub fn max_depth_reached(&self) -> u64 {
        self.max_depth_reached
    }

    /// Get all collected counterexamples.
    pub fn counterexamples(&self) -> &[Counterexample] {
        &self.counterexamples
    }

    /// Whether exploration should stop (budget exhausted or enough counterexamples).
    pub fn should_stop(&self) -> bool {
        if self.states_explored >= self.config.max_states {
            return true;
        }
        if self.current_depth >= self.config.max_depth {
            return true;
        }
        if !self.config.exhaustive && !self.counterexamples.is_empty() {
            return true;
        }
        self.counterexamples.len() >= self.config.max_counterexamples
    }

    /// Produce a verdict from the current exploration state.
    pub fn verdict(&self) -> ModelCheckVerdict {
        if !self.counterexamples.is_empty() {
            ModelCheckVerdict::ViolationsFound {
                counterexamples: self.counterexamples.clone(),
            }
        } else if self.states_explored >= self.config.max_states {
            ModelCheckVerdict::Incomplete {
                states_explored: self.states_explored,
                reason: "state budget exhausted".to_string(),
            }
        } else {
            ModelCheckVerdict::NoViolation {
                states_explored: self.states_explored,
                depth_reached: self.max_depth_reached,
            }
        }
    }

    /// State snapshot.
    pub fn snapshot(&self) -> ModelCheckerSnapshot {
        ModelCheckerSnapshot {
            states_explored: self.states_explored,
            current_depth: self.current_depth,
            counterexamples_found: self.counterexamples.len(),
            invariants_checked: self.checker.total_checks(),
            violations_found: self.checker.total_violations(),
            strategy: self.config.strategy,
        }
    }

    /// Status line.
    pub fn status_line(&self) -> String {
        format!(
            "model_check: states={} depth={}/{} cx={} strategy={}",
            self.states_explored,
            self.current_depth,
            self.config.max_depth,
            self.counterexamples.len(),
            self.config.strategy
        )
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.checker.reset();
        self.counterexamples.clear();
        self.current_trace.clear();
        self.states_explored = 0;
        self.current_depth = 0;
        self.max_depth_reached = 0;
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> ModelCheckerDegradation {
        if self.counterexamples.is_empty() {
            ModelCheckerDegradation::Healthy
        } else if self.counterexamples.len() <= 3 {
            ModelCheckerDegradation::ViolationsFound {
                count: self.counterexamples.len(),
            }
        } else {
            ModelCheckerDegradation::HighViolationRate {
                count: self.counterexamples.len(),
                states: self.states_explored,
            }
        }
    }

    /// Structured log entry.
    pub fn log_entry(&self) -> ModelCheckerLogEntry {
        ModelCheckerLogEntry {
            states_explored: self.states_explored,
            max_depth_reached: self.max_depth_reached,
            counterexamples_found: self.counterexamples.len(),
            verdict: self.verdict(),
            degradation: self.detect_degradation(),
        }
    }
}

/// Degradation state for the model checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelCheckerDegradation {
    /// No violations found.
    Healthy,
    /// Some violations found (≤3).
    ViolationsFound { count: usize },
    /// Many violations found.
    HighViolationRate { count: usize, states: u64 },
}

impl fmt::Display for ModelCheckerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::ViolationsFound { count } => write!(f, "violations({count})"),
            Self::HighViolationRate { count, states } => {
                write!(f, "high_rate({count}/{states})")
            }
        }
    }
}

/// Structured log entry for the model checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCheckerLogEntry {
    pub states_explored: u64,
    pub max_depth_reached: u64,
    pub counterexamples_found: usize,
    pub verdict: ModelCheckVerdict,
    pub degradation: ModelCheckerDegradation,
}

// ── E2 Impl: Model Checker Bridge Methods ────────────────────────

impl ModelChecker {
    /// Run a sequence of steps with scheduler invariant checks.
    pub fn run_scheduler_scenario(
        &mut self,
        checker: &mut InvariantChecker,
        actions: &[(TraceAction, Vec<SchedulerInvariant>)],
        start_us: u64,
    ) -> ModelCheckVerdict {
        for (i, (action, invariants)) in actions.iter().enumerate() {
            let ts = start_us + i as u64;
            let results = checker.check_scheduler_batch(invariants, ts);
            let violated = self.step(action.clone(), &results, ts);
            if violated && !self.config.exhaustive {
                return self.verdict();
            }
            if self.should_stop() {
                break;
            }
        }
        self.verdict()
    }

    /// Run a sequence of steps with budget invariant checks.
    pub fn run_budget_scenario(
        &mut self,
        checker: &mut InvariantChecker,
        actions: &[(TraceAction, Vec<BudgetInvariant>)],
        start_us: u64,
    ) -> ModelCheckVerdict {
        for (i, (action, invariants)) in actions.iter().enumerate() {
            let ts = start_us + i as u64;
            let results = checker.check_budget_batch(invariants, ts);
            let violated = self.step(action.clone(), &results, ts);
            if violated && !self.config.exhaustive {
                return self.verdict();
            }
            if self.should_stop() {
                break;
            }
        }
        self.verdict()
    }

    /// Run a sequence of steps with recovery invariant checks.
    pub fn run_recovery_scenario(
        &mut self,
        checker: &mut InvariantChecker,
        actions: &[(TraceAction, Vec<RecoveryInvariant>)],
        start_us: u64,
    ) -> ModelCheckVerdict {
        for (i, (action, invariants)) in actions.iter().enumerate() {
            let ts = start_us + i as u64;
            let results = checker.check_recovery_batch(invariants, ts);
            let violated = self.step(action.clone(), &results, ts);
            if violated && !self.config.exhaustive {
                return self.verdict();
            }
            if self.should_stop() {
                break;
            }
        }
        self.verdict()
    }

    /// Get counterexamples for a specific domain.
    pub fn counterexamples_by_domain(&self, domain: InvariantDomain) -> Vec<&Counterexample> {
        self.counterexamples
            .iter()
            .filter(|cx| cx.domain == domain)
            .collect()
    }

    /// Get the shortest counterexample (fewest trace steps).
    pub fn shortest_counterexample(&self) -> Option<&Counterexample> {
        self.counterexamples.iter().min_by_key(|cx| cx.trace.len())
    }

    /// Get unique violated predicate IDs.
    pub fn violated_predicates(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut preds = Vec::new();
        for cx in &self.counterexamples {
            if seen.insert(cx.predicate_id.clone()) {
                preds.push(cx.predicate_id.clone());
            }
        }
        preds
    }

    /// Access the inner invariant checker.
    pub fn inner_checker(&self) -> &InvariantChecker {
        &self.checker
    }

    /// Mutably access the inner invariant checker.
    pub fn inner_checker_mut(&mut self) -> &mut InvariantChecker {
        &mut self.checker
    }

    /// Current trace length.
    pub fn current_trace_len(&self) -> usize {
        self.current_trace.len()
    }

    /// Get the exploration strategy.
    pub fn strategy(&self) -> ExplorationStrategy {
        self.config.strategy
    }
}
