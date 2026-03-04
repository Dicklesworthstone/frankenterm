//! Proactive pane lifecycle management — health classification, zombie reaping,
//! stuck detection, and age-based cleanup.
//!
//! Wraps [`process_triage`](crate::process_triage) with pane-aware context:
//! per-pane health classification based on age × CPU × child processes,
//! configurable age reaper with graceful shutdown, and resource pressure
//! response (renice idle panes under load).
//!
//! # Health Classification
//!
//! | Age | CPU% | Status | Action |
//! |-----|------|--------|--------|
//! | <4h | >10% | Active | Protect |
//! | <4h | <2% | Thinking | Protect |
//! | 4–16h | >5% | Working | Check children |
//! | 4–16h | <2% | Possibly stuck | Flag for review |
//! | 16–24h | any | Likely stuck | Kill if only MCP children |
//! | >24h | any | Abandoned | Kill immediately |

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::process_tree::{PaneActivity, ProcessTree, infer_activity};
use crate::process_triage::{ClassifiedProcess, ProcessContext, TriagePlan, build_plan, classify};

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the pane lifecycle engine.
///
/// Uses plain `u64` because `PaneLifecycleEngine` methods take `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct PaneLifecycleTelemetry {
    /// Total health_check() invocations.
    health_checks: u64,
    /// New panes registered (first health_check for a pane ID).
    panes_registered: u64,
    /// Total panes removed via remove_pane().
    panes_removed: u64,
    /// health_check actions recommending None.
    actions_none: u64,
    /// health_check actions recommending Warn.
    actions_warn: u64,
    /// health_check actions recommending Review.
    actions_review: u64,
    /// health_check actions recommending GracefulKill.
    actions_graceful_kill: u64,
    /// health_check actions recommending ForceKill.
    actions_force_kill: u64,
}

impl PaneLifecycleTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> PaneLifecycleTelemetrySnapshot {
        PaneLifecycleTelemetrySnapshot {
            health_checks: self.health_checks,
            panes_registered: self.panes_registered,
            panes_removed: self.panes_removed,
            actions_none: self.actions_none,
            actions_warn: self.actions_warn,
            actions_review: self.actions_review,
            actions_graceful_kill: self.actions_graceful_kill,
            actions_force_kill: self.actions_force_kill,
        }
    }
}

/// Serializable snapshot of pane lifecycle engine telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLifecycleTelemetrySnapshot {
    /// Total health_check() invocations.
    pub health_checks: u64,
    /// New panes registered.
    pub panes_registered: u64,
    /// Total panes removed.
    pub panes_removed: u64,
    /// Actions recommending None.
    pub actions_none: u64,
    /// Actions recommending Warn.
    pub actions_warn: u64,
    /// Actions recommending Review.
    pub actions_review: u64,
    /// Actions recommending GracefulKill.
    pub actions_graceful_kill: u64,
    /// Actions recommending ForceKill.
    pub actions_force_kill: u64,
}

// =============================================================================
// Health Status
// =============================================================================

/// Pane health classification derived from age, CPU usage, and child processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneHealth {
    /// <4h, active CPU — healthy, protect.
    Active,
    /// <4h, low CPU — agent may be thinking, protect.
    Thinking,
    /// 4–16h, moderate CPU — working but aging, check children.
    Working,
    /// 4–16h, low CPU — possibly stuck, flag for review.
    PossiblyStuck,
    /// 16–24h, any CPU — likely stuck.
    LikelyStuck,
    /// >24h — abandoned, kill immediately.
    Abandoned,
}

impl std::fmt::Display for PaneHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Thinking => write!(f, "thinking"),
            Self::Working => write!(f, "working"),
            Self::PossiblyStuck => write!(f, "possibly_stuck"),
            Self::LikelyStuck => write!(f, "likely_stuck"),
            Self::Abandoned => write!(f, "abandoned"),
        }
    }
}

impl PaneHealth {
    /// Whether this pane should be protected from cleanup.
    #[must_use]
    pub const fn is_protected(self) -> bool {
        matches!(self, Self::Active | Self::Thinking)
    }

    /// Whether this pane needs human review before action.
    #[must_use]
    pub const fn needs_review(self) -> bool {
        matches!(self, Self::PossiblyStuck | Self::Working)
    }

    /// Whether automatic cleanup is appropriate.
    #[must_use]
    pub const fn is_reapable(self) -> bool {
        matches!(self, Self::LikelyStuck | Self::Abandoned)
    }
}

// =============================================================================
// Health Sample
// =============================================================================

/// A single health sample for a pane at a point in time.
#[derive(Debug, Clone)]
pub struct PaneHealthSample {
    /// Pane identifier.
    pub pane_id: u64,
    /// Classified health status.
    pub health: PaneHealth,
    /// Inferred pane activity from process tree.
    pub activity: PaneActivity,
    /// CPU usage percentage (0–100+).
    pub cpu_percent: f64,
    /// Total RSS in KB for the pane's process subtree.
    pub rss_kb: u64,
    /// Number of child processes.
    pub child_count: usize,
    /// Pane age (time since creation).
    pub age: Duration,
    /// Process tree root PID.
    pub root_pid: u32,
}

// =============================================================================
// Lifecycle Configuration
// =============================================================================

/// Configuration for the pane lifecycle engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleConfig {
    /// Enable proactive lifecycle management.
    pub enabled: bool,
    /// Health check sampling interval.
    pub sample_interval: Duration,
    /// Ring buffer capacity for trend analysis per pane.
    pub trend_window: usize,

    // -- Age reaper thresholds --
    /// Warn threshold (hours).
    pub warn_age_hours: f64,
    /// Kill threshold (hours).
    pub kill_age_hours: f64,
    /// Grace period for SIGTERM before SIGKILL.
    pub grace_period: Duration,

    // -- CPU thresholds for health classification --
    /// CPU% above which a young pane (<4h) is "active".
    pub active_cpu_threshold: f64,
    /// CPU% below which a mid-age pane (4–16h) is "possibly stuck".
    pub stuck_cpu_threshold: f64,

    // -- Resource pressure response --
    /// CPU load above which idle panes get reniced (0.0–1.0 of total).
    pub pressure_renice_threshold: f64,
    /// Nice value for reniced panes.
    pub renice_value: i32,

    // -- Per-pane overrides --
    /// Pane IDs exempt from automatic reaping (manually protected).
    pub protected_panes: Vec<u64>,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval: Duration::from_secs(30),
            trend_window: 60,
            warn_age_hours: 16.0,
            kill_age_hours: 24.0,
            grace_period: Duration::from_secs(30),
            active_cpu_threshold: 10.0,
            stuck_cpu_threshold: 2.0,
            pressure_renice_threshold: 0.8,
            renice_value: 19,
            protected_panes: Vec::new(),
        }
    }
}

// =============================================================================
// Lifecycle Action
// =============================================================================

/// Action recommended by the lifecycle engine for a specific pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum LifecycleAction {
    /// No action needed — pane is healthy.
    None,
    /// Send a warning notification (pane aging).
    Warn { reason: String },
    /// Flag for manual review.
    Review { reason: String },
    /// Renice the pane to reduce priority.
    Renice { nice: i32, reason: String },
    /// Gracefully terminate the pane (SIGTERM + grace period).
    GracefulKill {
        grace_period: Duration,
        reason: String,
    },
    /// Immediately kill the pane (SIGKILL).
    ForceKill { reason: String },
}

impl LifecycleAction {
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::GracefulKill { .. } | Self::ForceKill { .. })
    }
}

// =============================================================================
// Pane Lifecycle Engine
// =============================================================================

/// Per-pane state tracked by the lifecycle engine.
#[derive(Debug, Clone)]
struct PaneState {
    pane_id: u64,
    root_pid: u32,
    age: Duration,
    samples: VecDeque<PaneHealthSample>,
    last_health: PaneHealth,
    warned: bool,
}

/// The pane lifecycle engine — classifies pane health, recommends actions,
/// and delegates process classification to `process_triage`.
pub struct PaneLifecycleEngine {
    config: LifecycleConfig,
    pane_states: Vec<PaneState>,
    /// Operational telemetry counters.
    telemetry: PaneLifecycleTelemetry,
}

impl PaneLifecycleEngine {
    /// Create a new engine with the given configuration.
    #[must_use]
    pub fn new(config: LifecycleConfig) -> Self {
        Self {
            config,
            pane_states: Vec::new(),
            telemetry: PaneLifecycleTelemetry::new(),
        }
    }

    /// Create an engine with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(LifecycleConfig::default())
    }

    /// Get the current configuration.
    #[must_use]
    pub fn config(&self) -> &LifecycleConfig {
        &self.config
    }

    /// Classify a pane's health based on age and CPU usage.
    #[must_use]
    pub fn classify_health(&self, age: Duration, cpu_percent: f64) -> PaneHealth {
        let hours = age.as_secs_f64() / 3600.0;

        if hours > self.config.kill_age_hours {
            PaneHealth::Abandoned
        } else if hours > self.config.warn_age_hours {
            PaneHealth::LikelyStuck
        } else if hours > 4.0 {
            if cpu_percent > 5.0 {
                PaneHealth::Working
            } else if cpu_percent < self.config.stuck_cpu_threshold {
                PaneHealth::PossiblyStuck
            } else {
                PaneHealth::Working
            }
        } else if cpu_percent > self.config.active_cpu_threshold {
            PaneHealth::Active
        } else {
            PaneHealth::Thinking
        }
    }

    /// Perform a health check for a pane given its process tree and metadata.
    ///
    /// Returns a health sample and the recommended lifecycle action.
    pub fn health_check(
        &mut self,
        pane_id: u64,
        root_pid: u32,
        age: Duration,
        cpu_percent: f64,
        tree: Option<&ProcessTree>,
    ) -> (PaneHealthSample, LifecycleAction) {
        self.telemetry.health_checks += 1;

        let health = self.classify_health(age, cpu_percent);

        let (rss_kb, child_count, activity) = tree
            .map(|t| {
                (
                    t.total_rss_kb,
                    t.total_processes.saturating_sub(1),
                    infer_activity(t),
                )
            })
            .unwrap_or((0, 0, PaneActivity::Idle));

        let sample = PaneHealthSample {
            pane_id,
            health,
            activity,
            cpu_percent,
            rss_kb,
            child_count,
            age,
            root_pid,
        };

        // Capture config before mutable borrow.
        let trend_window = self.config.trend_window;

        // Track new pane registration.
        let is_new = !self.pane_states.iter().any(|s| s.pane_id == pane_id);
        if is_new {
            self.telemetry.panes_registered += 1;
        }

        // Get or create pane state.
        let state = self.ensure_pane_state(pane_id, root_pid, age);
        state.last_health = health;
        state.age = age;
        if health != PaneHealth::PossiblyStuck {
            // A recovered pane should warn again if it later becomes possibly stuck.
            state.warned = false;
        }

        // Store sample in ring buffer.
        if state.samples.len() >= trend_window {
            state.samples.pop_front();
        }
        state.samples.push_back(sample.clone());

        // Determine action.
        let action = self.recommend_action(pane_id, health, &sample);

        // Track action type.
        match &action {
            LifecycleAction::None => self.telemetry.actions_none += 1,
            LifecycleAction::Warn { .. } => self.telemetry.actions_warn += 1,
            LifecycleAction::Review { .. } => self.telemetry.actions_review += 1,
            LifecycleAction::Renice { .. } => {} // tracked elsewhere if needed
            LifecycleAction::GracefulKill { .. } => self.telemetry.actions_graceful_kill += 1,
            LifecycleAction::ForceKill { .. } => self.telemetry.actions_force_kill += 1,
        }

        (sample, action)
    }

    /// Build a triage plan from the current pane states and their process trees.
    ///
    /// Delegates to `process_triage::build_plan` with pane-aware context.
    #[must_use]
    pub fn build_triage_plan(
        &self,
        pane_trees: &[(u64, ProcessTree, Duration, f64)],
    ) -> TriagePlan {
        let inputs: Vec<_> = pane_trees
            .iter()
            .map(|(pane_id, tree, age, cpu_pct)| {
                let ctx_age = *age;
                let ctx_cpu = *cpu_pct;
                let context_fn: Box<dyn Fn(u32) -> ProcessContext> =
                    Box::new(move |_pid| ProcessContext {
                        age: ctx_age,
                        cpu_percent: ctx_cpu,
                        is_test: false,
                    });
                (tree.clone(), Some(*pane_id), context_fn)
            })
            .collect();

        // Convert to the expected slice-of-tuples format.
        type PlanInput<'a> = (ProcessTree, Option<u64>, &'a dyn Fn(u32) -> ProcessContext);
        let refs: Vec<PlanInput<'_>> = inputs
            .iter()
            .map(|(tree, pane_id, ctx_fn)| (tree.clone(), *pane_id, ctx_fn.as_ref()))
            .collect();

        build_plan(&refs)
    }

    /// Classify a single process node with pane-aware context.
    #[must_use]
    pub fn classify_process(
        node: &crate::process_tree::ProcessNode,
        age: Duration,
        cpu_percent: f64,
    ) -> ClassifiedProcess {
        let ctx = ProcessContext {
            age,
            cpu_percent,
            is_test: false,
        };
        let (category, action, reason) = classify(node, &ctx);
        ClassifiedProcess {
            pid: node.pid,
            name: node.name.clone(),
            category,
            action,
            reason,
            pane_id: None,
        }
    }

    /// Get the most recent health status for a pane.
    #[must_use]
    pub fn pane_health(&self, pane_id: u64) -> Option<PaneHealth> {
        self.pane_states
            .iter()
            .find(|s| s.pane_id == pane_id)
            .map(|s| s.last_health)
    }

    /// Get the current process tree root PID for a pane.
    #[must_use]
    pub fn pane_root_pid(&self, pane_id: u64) -> Option<u32> {
        self.pane_states
            .iter()
            .find(|s| s.pane_id == pane_id)
            .map(|s| s.root_pid)
    }

    /// Get the number of health samples collected for a pane.
    #[must_use]
    pub fn sample_count(&self, pane_id: u64) -> usize {
        self.pane_states
            .iter()
            .find(|s| s.pane_id == pane_id)
            .map_or(0, |s| s.samples.len())
    }

    /// Get the number of tracked panes.
    #[must_use]
    pub fn tracked_pane_count(&self) -> usize {
        self.pane_states.len()
    }

    /// Remove tracking state for a pane (e.g., when closed).
    pub fn remove_pane(&mut self, pane_id: u64) {
        let before = self.pane_states.len();
        self.pane_states.retain(|s| s.pane_id != pane_id);
        if self.pane_states.len() < before {
            self.telemetry.panes_removed += 1;
        }
    }

    /// Get all panes currently classified as reapable.
    #[must_use]
    pub fn reapable_panes(&self) -> Vec<u64> {
        self.pane_states
            .iter()
            .filter(|s| s.last_health.is_reapable())
            .filter(|s| !self.config.protected_panes.contains(&s.pane_id))
            .map(|s| s.pane_id)
            .collect()
    }

    /// Get all panes needing manual review.
    #[must_use]
    pub fn review_panes(&self) -> Vec<u64> {
        self.pane_states
            .iter()
            .filter(|s| s.last_health.needs_review())
            .map(|s| s.pane_id)
            .collect()
    }

    /// Access telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &PaneLifecycleTelemetry {
        &self.telemetry
    }

    // ========================================================================
    // Internal
    // ========================================================================

    fn ensure_pane_state(&mut self, pane_id: u64, root_pid: u32, age: Duration) -> &mut PaneState {
        let pos = self.pane_states.iter().position(|s| s.pane_id == pane_id);
        match pos {
            Some(idx) => {
                self.pane_states[idx].root_pid = root_pid;
                self.pane_states[idx].age = age;
                &mut self.pane_states[idx]
            }
            None => {
                self.pane_states.push(PaneState {
                    pane_id,
                    root_pid,
                    age,
                    samples: VecDeque::with_capacity(self.config.trend_window),
                    last_health: PaneHealth::Thinking,
                    warned: false,
                });
                self.pane_states.last_mut().unwrap()
            }
        }
    }

    fn recommend_action(
        &mut self,
        pane_id: u64,
        health: PaneHealth,
        _sample: &PaneHealthSample,
    ) -> LifecycleAction {
        // Protected panes are never reaped.
        if self.config.protected_panes.contains(&pane_id) {
            return LifecycleAction::None;
        }

        match health {
            PaneHealth::Active | PaneHealth::Thinking => LifecycleAction::None,
            PaneHealth::Working => LifecycleAction::None,
            PaneHealth::PossiblyStuck => {
                let state = self
                    .pane_states
                    .iter_mut()
                    .find(|s| s.pane_id == pane_id)
                    .unwrap();
                if !state.warned {
                    state.warned = true;
                    LifecycleAction::Warn {
                        reason: format!(
                            "Pane {pane_id} has low CPU usage (4–16h age range) — may be stuck"
                        ),
                    }
                } else {
                    LifecycleAction::Review {
                        reason: format!(
                            "Pane {pane_id} remains possibly stuck after previous warning"
                        ),
                    }
                }
            }
            PaneHealth::LikelyStuck => LifecycleAction::GracefulKill {
                grace_period: self.config.grace_period,
                reason: format!(
                    "Pane {pane_id} likely stuck (16–24h age range) — scheduling graceful kill"
                ),
            },
            PaneHealth::Abandoned => LifecycleAction::ForceKill {
                reason: format!("Pane {pane_id} abandoned (>24h) — force killing"),
            },
        }
    }
}

// =============================================================================
// Resource Pressure Response
// =============================================================================

/// Evaluate resource pressure and recommend pane renice actions.
///
/// Returns a list of pane IDs that should be reniced to reduce CPU priority.
#[must_use]
pub fn pressure_renice_candidates(
    pane_healths: &[(u64, PaneHealth, Duration)],
    cpu_load_fraction: f64,
    config: &LifecycleConfig,
) -> Vec<(u64, i32)> {
    if cpu_load_fraction < config.pressure_renice_threshold {
        return Vec::new();
    }

    // Under pressure: renice panes that are idle or old, oldest first.
    let mut candidates: Vec<_> = pane_healths
        .iter()
        .filter(|(_, health, _)| {
            matches!(
                health,
                PaneHealth::PossiblyStuck
                    | PaneHealth::LikelyStuck
                    | PaneHealth::Abandoned
                    | PaneHealth::Thinking
            )
        })
        .collect();

    // Sort by age descending (oldest first).
    candidates.sort_by_key(|x| std::cmp::Reverse(x.2));

    candidates
        .iter()
        .map(|(pane_id, _, _)| (*pane_id, config.renice_value))
        .collect()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_tree::{ProcessNode, ProcessState};
    use crate::process_triage::{TriageAction, TriageCategory};

    fn make_tree(name: &str, children: Vec<ProcessNode>) -> ProcessTree {
        let total = 1 + children.len();
        ProcessTree {
            root: ProcessNode {
                pid: 1000,
                ppid: 1,
                name: name.to_string(),
                argv: vec![],
                state: ProcessState::Running,
                rss_kb: 1024,
                children,
            },
            total_processes: total,
            total_rss_kb: 1024 * total as u64,
        }
    }

    fn make_child(name: &str, pid: u32) -> ProcessNode {
        ProcessNode {
            pid,
            ppid: 1000,
            name: name.to_string(),
            argv: vec![],
            state: ProcessState::Running,
            rss_kb: 512,
            children: vec![],
        }
    }

    // ========================================================================
    // Health Classification
    // ========================================================================

    #[test]
    fn young_active_pane_is_active() {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(3600), 50.0); // 1h, 50% CPU
        assert_eq!(health, PaneHealth::Active);
    }

    #[test]
    fn young_idle_pane_is_thinking() {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(3600), 1.0); // 1h, 1% CPU
        assert_eq!(health, PaneHealth::Thinking);
    }

    #[test]
    fn mid_age_active_pane_is_working() {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(8 * 3600), 15.0); // 8h, 15% CPU
        assert_eq!(health, PaneHealth::Working);
    }

    #[test]
    fn mid_age_idle_pane_is_possibly_stuck() {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(8 * 3600), 0.5); // 8h, 0.5% CPU
        assert_eq!(health, PaneHealth::PossiblyStuck);
    }

    #[test]
    fn old_pane_is_likely_stuck() {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(20 * 3600), 30.0); // 20h
        assert_eq!(health, PaneHealth::LikelyStuck);
    }

    #[test]
    fn very_old_pane_is_abandoned() {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(30 * 3600), 0.0); // 30h
        assert_eq!(health, PaneHealth::Abandoned);
    }

    // ========================================================================
    // Health Properties
    // ========================================================================

    #[test]
    fn active_is_protected() {
        assert!(PaneHealth::Active.is_protected());
        assert!(PaneHealth::Thinking.is_protected());
        assert!(!PaneHealth::Working.is_protected());
        assert!(!PaneHealth::PossiblyStuck.is_protected());
    }

    #[test]
    fn abandoned_is_reapable() {
        assert!(PaneHealth::Abandoned.is_reapable());
        assert!(PaneHealth::LikelyStuck.is_reapable());
        assert!(!PaneHealth::Active.is_reapable());
        assert!(!PaneHealth::PossiblyStuck.is_reapable());
    }

    #[test]
    fn working_needs_review() {
        assert!(PaneHealth::Working.needs_review());
        assert!(PaneHealth::PossiblyStuck.needs_review());
        assert!(!PaneHealth::Active.needs_review());
        assert!(!PaneHealth::Abandoned.needs_review());
    }

    // ========================================================================
    // Health Check Actions
    // ========================================================================

    #[test]
    fn active_pane_gets_no_action() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let tree = make_tree("zsh", vec![make_child("cargo", 1001)]);
        let (_, action) =
            engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, Some(&tree));
        assert!(matches!(action, LifecycleAction::None));
    }

    #[test]
    fn abandoned_pane_gets_force_kill() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let (_, action) = engine.health_check(1, 1000, Duration::from_secs(30 * 3600), 0.0, None);
        assert!(action.is_destructive());
        assert!(matches!(action, LifecycleAction::ForceKill { .. }));
    }

    #[test]
    fn likely_stuck_gets_graceful_kill() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let (_, action) = engine.health_check(1, 1000, Duration::from_secs(20 * 3600), 5.0, None);
        assert!(action.is_destructive());
        assert!(matches!(action, LifecycleAction::GracefulKill { .. }));
    }

    #[test]
    fn possibly_stuck_warns_first_then_reviews() {
        let mut engine = PaneLifecycleEngine::with_defaults();

        // First check: warn
        let (_, action1) = engine.health_check(1, 1000, Duration::from_secs(8 * 3600), 0.5, None);
        assert!(matches!(action1, LifecycleAction::Warn { .. }));

        // Second check: review (already warned)
        let (_, action2) = engine.health_check(1, 1000, Duration::from_secs(9 * 3600), 0.3, None);
        assert!(matches!(action2, LifecycleAction::Review { .. }));
    }

    #[test]
    fn possibly_stuck_warning_resets_after_recovery() {
        let mut engine = PaneLifecycleEngine::with_defaults();

        let (_, first) = engine.health_check(1, 1000, Duration::from_secs(8 * 3600), 0.5, None);
        assert!(matches!(first, LifecycleAction::Warn { .. }));

        let (_, recovered) = engine.health_check(1, 1000, Duration::from_secs(3600), 20.0, None);
        assert!(matches!(recovered, LifecycleAction::None));

        let (_, second) = engine.health_check(1, 1000, Duration::from_secs(10 * 3600), 0.4, None);
        assert!(matches!(second, LifecycleAction::Warn { .. }));
    }

    // ========================================================================
    // Protected Panes
    // ========================================================================

    #[test]
    fn protected_pane_not_reaped() {
        let mut engine = PaneLifecycleEngine::new(LifecycleConfig {
            protected_panes: vec![42],
            ..LifecycleConfig::default()
        });
        let (_, action) = engine.health_check(42, 1000, Duration::from_secs(30 * 3600), 0.0, None);
        assert!(matches!(action, LifecycleAction::None));
    }

    #[test]
    fn protected_pane_excluded_from_reapable() {
        let mut engine = PaneLifecycleEngine::new(LifecycleConfig {
            protected_panes: vec![1],
            ..LifecycleConfig::default()
        });
        engine.health_check(1, 1000, Duration::from_secs(30 * 3600), 0.0, None);
        engine.health_check(2, 2000, Duration::from_secs(30 * 3600), 0.0, None);

        let reapable = engine.reapable_panes();
        assert!(!reapable.contains(&1));
        assert!(reapable.contains(&2));
    }

    // ========================================================================
    // Pane State Tracking
    // ========================================================================

    #[test]
    fn tracks_multiple_panes() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, None);
        engine.health_check(2, 2000, Duration::from_secs(20 * 3600), 0.0, None);
        engine.health_check(3, 3000, Duration::from_secs(8 * 3600), 0.5, None);

        assert_eq!(engine.tracked_pane_count(), 3);
        assert_eq!(engine.pane_health(1), Some(PaneHealth::Active));
        assert_eq!(engine.pane_health(2), Some(PaneHealth::LikelyStuck));
        assert_eq!(engine.pane_health(3), Some(PaneHealth::PossiblyStuck));
    }

    #[test]
    fn remove_pane_drops_state() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, None);
        assert_eq!(engine.tracked_pane_count(), 1);

        engine.remove_pane(1);
        assert_eq!(engine.tracked_pane_count(), 0);
        assert_eq!(engine.pane_health(1), None);
    }

    #[test]
    fn sample_count_tracks_history() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        for i in 0..5 {
            engine.health_check(1, 1000, Duration::from_secs(3600 + i * 30), 50.0, None);
        }
        assert_eq!(engine.sample_count(1), 5);
    }

    #[test]
    fn sample_ring_buffer_wraps() {
        let mut engine = PaneLifecycleEngine::new(LifecycleConfig {
            trend_window: 3,
            ..LifecycleConfig::default()
        });
        for i in 0..10 {
            engine.health_check(1, 1000, Duration::from_secs(3600 + i * 30), 50.0, None);
        }
        assert_eq!(engine.sample_count(1), 3); // Capped at trend_window
    }

    // ========================================================================
    // Reapable / Review Lists
    // ========================================================================

    #[test]
    fn reapable_panes_returns_stuck_and_abandoned() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, None); // Active
        engine.health_check(2, 2000, Duration::from_secs(20 * 3600), 0.0, None); // LikelyStuck
        engine.health_check(3, 3000, Duration::from_secs(30 * 3600), 0.0, None); // Abandoned

        let reapable = engine.reapable_panes();
        assert_eq!(reapable.len(), 2);
        assert!(reapable.contains(&2));
        assert!(reapable.contains(&3));
    }

    #[test]
    fn review_panes_returns_working_and_possibly_stuck() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, None); // Active
        engine.health_check(2, 2000, Duration::from_secs(8 * 3600), 15.0, None); // Working
        engine.health_check(3, 3000, Duration::from_secs(8 * 3600), 0.5, None); // PossiblyStuck

        let review = engine.review_panes();
        assert_eq!(review.len(), 2);
        assert!(review.contains(&2));
        assert!(review.contains(&3));
    }

    // ========================================================================
    // Health Check with Process Tree
    // ========================================================================

    #[test]
    fn health_check_captures_tree_metrics() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let tree = make_tree(
            "zsh",
            vec![make_child("cargo", 1001), make_child("rustc", 1002)],
        );
        let (sample, _) =
            engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, Some(&tree));

        assert_eq!(sample.child_count, 2);
        assert_eq!(sample.rss_kb, tree.total_rss_kb);
        assert_eq!(sample.root_pid, 1000);
    }

    #[test]
    fn health_check_without_tree() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let (sample, _) = engine.health_check(1, 1000, Duration::from_secs(3600), 50.0, None);

        assert_eq!(sample.child_count, 0);
        assert_eq!(sample.rss_kb, 0);
        assert_eq!(sample.activity, PaneActivity::Idle);
    }

    // ========================================================================
    // Resource Pressure Response
    // ========================================================================

    #[test]
    fn no_renice_below_threshold() {
        let config = LifecycleConfig::default();
        let healths = vec![
            (1, PaneHealth::Thinking, Duration::from_secs(3600)),
            (2, PaneHealth::PossiblyStuck, Duration::from_secs(8 * 3600)),
        ];
        let candidates = pressure_renice_candidates(&healths, 0.5, &config);
        assert!(candidates.is_empty());
    }

    #[test]
    fn renice_under_pressure() {
        let config = LifecycleConfig::default();
        let healths = vec![
            (1, PaneHealth::Active, Duration::from_secs(3600)),
            (2, PaneHealth::Thinking, Duration::from_secs(3600)),
            (3, PaneHealth::PossiblyStuck, Duration::from_secs(8 * 3600)),
            (4, PaneHealth::Abandoned, Duration::from_secs(30 * 3600)),
        ];
        let candidates = pressure_renice_candidates(&healths, 0.9, &config);

        // Active panes should NOT be reniced
        assert!(!candidates.iter().any(|(id, _)| *id == 1));
        // Thinking, PossiblyStuck, Abandoned should be reniced
        assert!(candidates.iter().any(|(id, _)| *id == 2));
        assert!(candidates.iter().any(|(id, _)| *id == 3));
        assert!(candidates.iter().any(|(id, _)| *id == 4));
    }

    #[test]
    fn renice_sorts_oldest_first() {
        let config = LifecycleConfig::default();
        let healths = vec![
            (1, PaneHealth::Thinking, Duration::from_secs(3600)),
            (2, PaneHealth::Thinking, Duration::from_secs(3 * 3600)),
            (3, PaneHealth::Thinking, Duration::from_secs(2 * 3600)),
        ];
        let candidates = pressure_renice_candidates(&healths, 0.9, &config);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].0, 2); // 3h oldest
        assert_eq!(candidates[1].0, 3); // 2h
        assert_eq!(candidates[2].0, 1); // 1h youngest
    }

    // ========================================================================
    // Process Triage Integration
    // ========================================================================

    #[test]
    fn classify_process_delegates_to_triage() {
        let node = ProcessNode {
            pid: 1234,
            ppid: 1,
            name: "zombie_proc".to_string(),
            argv: vec![],
            state: ProcessState::Zombie,
            rss_kb: 0,
            children: vec![],
        };
        let classified =
            PaneLifecycleEngine::classify_process(&node, Duration::from_secs(3600), 0.0);
        assert_eq!(classified.pid, 1234);
        assert_eq!(classified.category, TriageCategory::Zombie);
        assert!(matches!(classified.action, TriageAction::ReapZombie { .. }));
    }

    // ========================================================================
    // Lifecycle Action Properties
    // ========================================================================

    #[test]
    fn destructive_actions_identified() {
        assert!(
            LifecycleAction::ForceKill {
                reason: String::new()
            }
            .is_destructive()
        );
        assert!(
            LifecycleAction::GracefulKill {
                grace_period: Duration::from_secs(30),
                reason: String::new()
            }
            .is_destructive()
        );
        assert!(!LifecycleAction::None.is_destructive());
        assert!(
            !LifecycleAction::Warn {
                reason: String::new()
            }
            .is_destructive()
        );
        assert!(
            !LifecycleAction::Review {
                reason: String::new()
            }
            .is_destructive()
        );
    }

    // ========================================================================
    // Display Impls
    // ========================================================================

    #[test]
    fn health_display() {
        assert_eq!(PaneHealth::Active.to_string(), "active");
        assert_eq!(PaneHealth::Abandoned.to_string(), "abandoned");
        assert_eq!(PaneHealth::PossiblyStuck.to_string(), "possibly_stuck");
    }

    // ========================================================================
    // Extended PaneHealth Tests
    // ========================================================================

    #[test]
    fn pane_health_clone() {
        let variants = [
            PaneHealth::Active,
            PaneHealth::Thinking,
            PaneHealth::Working,
            PaneHealth::PossiblyStuck,
            PaneHealth::LikelyStuck,
            PaneHealth::Abandoned,
        ];
        for v in &variants {
            let cloned = *v;
            assert_eq!(*v, cloned);
        }
    }

    #[test]
    fn pane_health_debug_all() {
        let variants = [
            (PaneHealth::Active, "Active"),
            (PaneHealth::Thinking, "Thinking"),
            (PaneHealth::Working, "Working"),
            (PaneHealth::PossiblyStuck, "PossiblyStuck"),
            (PaneHealth::LikelyStuck, "LikelyStuck"),
            (PaneHealth::Abandoned, "Abandoned"),
        ];
        for (v, name) in &variants {
            let debug = format!("{:?}", v);
            assert!(
                debug.contains(name),
                "Debug for {:?} should contain {}",
                v,
                name
            );
        }
    }

    #[test]
    fn pane_health_serde_roundtrip_all() {
        let variants = [
            PaneHealth::Active,
            PaneHealth::Thinking,
            PaneHealth::Working,
            PaneHealth::PossiblyStuck,
            PaneHealth::LikelyStuck,
            PaneHealth::Abandoned,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: PaneHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back, "Serde roundtrip failed for {:?}", v);
        }
    }

    #[test]
    fn pane_health_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(PaneHealth::Active);
        set.insert(PaneHealth::Thinking);
        set.insert(PaneHealth::Working);
        set.insert(PaneHealth::PossiblyStuck);
        set.insert(PaneHealth::LikelyStuck);
        set.insert(PaneHealth::Abandoned);
        assert_eq!(set.len(), 6);
    }

    #[test]
    fn pane_health_ord_ordering() {
        assert!(PaneHealth::Active < PaneHealth::Thinking);
        assert!(PaneHealth::Thinking < PaneHealth::Working);
        assert!(PaneHealth::Working < PaneHealth::PossiblyStuck);
        assert!(PaneHealth::PossiblyStuck < PaneHealth::LikelyStuck);
        assert!(PaneHealth::LikelyStuck < PaneHealth::Abandoned);
    }

    #[test]
    fn pane_health_display_all_variants() {
        assert_eq!(PaneHealth::Active.to_string(), "active");
        assert_eq!(PaneHealth::Thinking.to_string(), "thinking");
        assert_eq!(PaneHealth::Working.to_string(), "working");
        assert_eq!(PaneHealth::PossiblyStuck.to_string(), "possibly_stuck");
        assert_eq!(PaneHealth::LikelyStuck.to_string(), "likely_stuck");
        assert_eq!(PaneHealth::Abandoned.to_string(), "abandoned");
    }

    #[test]
    fn pane_health_is_protected_all() {
        assert!(PaneHealth::Active.is_protected());
        assert!(PaneHealth::Thinking.is_protected());
        assert!(!PaneHealth::Working.is_protected());
        assert!(!PaneHealth::PossiblyStuck.is_protected());
        assert!(!PaneHealth::LikelyStuck.is_protected());
        assert!(!PaneHealth::Abandoned.is_protected());
    }

    #[test]
    fn pane_health_needs_review_all() {
        assert!(!PaneHealth::Active.needs_review());
        assert!(!PaneHealth::Thinking.needs_review());
        assert!(PaneHealth::Working.needs_review());
        assert!(PaneHealth::PossiblyStuck.needs_review());
        assert!(!PaneHealth::LikelyStuck.needs_review());
        assert!(!PaneHealth::Abandoned.needs_review());
    }

    #[test]
    fn pane_health_is_reapable_all() {
        assert!(!PaneHealth::Active.is_reapable());
        assert!(!PaneHealth::Thinking.is_reapable());
        assert!(!PaneHealth::Working.is_reapable());
        assert!(!PaneHealth::PossiblyStuck.is_reapable());
        assert!(PaneHealth::LikelyStuck.is_reapable());
        assert!(PaneHealth::Abandoned.is_reapable());
    }

    // ========================================================================
    // LifecycleConfig Tests
    // ========================================================================

    #[test]
    fn lifecycle_config_default_values() {
        let cfg = LifecycleConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.sample_interval, Duration::from_secs(30));
        assert_eq!(cfg.trend_window, 60);
        assert!((cfg.warn_age_hours - 16.0).abs() < f64::EPSILON);
        assert!((cfg.kill_age_hours - 24.0).abs() < f64::EPSILON);
        assert_eq!(cfg.grace_period, Duration::from_secs(30));
        assert!((cfg.active_cpu_threshold - 10.0).abs() < f64::EPSILON);
        assert!((cfg.stuck_cpu_threshold - 2.0).abs() < f64::EPSILON);
        assert!((cfg.pressure_renice_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(cfg.renice_value, 19);
        assert!(cfg.protected_panes.is_empty());
    }

    #[test]
    fn lifecycle_config_clone() {
        let cfg = LifecycleConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cloned.enabled, cfg.enabled);
        assert_eq!(cloned.sample_interval, cfg.sample_interval);
        assert_eq!(cloned.trend_window, cfg.trend_window);
        assert!((cloned.warn_age_hours - cfg.warn_age_hours).abs() < f64::EPSILON);
        assert!((cloned.kill_age_hours - cfg.kill_age_hours).abs() < f64::EPSILON);
        assert_eq!(cloned.grace_period, cfg.grace_period);
        assert!((cloned.active_cpu_threshold - cfg.active_cpu_threshold).abs() < f64::EPSILON);
        assert!((cloned.stuck_cpu_threshold - cfg.stuck_cpu_threshold).abs() < f64::EPSILON);
        assert!(
            (cloned.pressure_renice_threshold - cfg.pressure_renice_threshold).abs() < f64::EPSILON
        );
        assert_eq!(cloned.renice_value, cfg.renice_value);
        assert_eq!(cloned.protected_panes, cfg.protected_panes);
    }

    #[test]
    fn lifecycle_config_debug() {
        let cfg = LifecycleConfig::default();
        let debug = format!("{:?}", cfg);
        assert!(debug.contains("LifecycleConfig"));
    }

    #[test]
    fn lifecycle_config_serde_roundtrip() {
        let cfg = LifecycleConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: LifecycleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enabled, cfg.enabled);
        assert_eq!(back.trend_window, cfg.trend_window);
        assert!((back.warn_age_hours - cfg.warn_age_hours).abs() < f64::EPSILON);
        assert!((back.kill_age_hours - cfg.kill_age_hours).abs() < f64::EPSILON);
        assert_eq!(back.renice_value, cfg.renice_value);
    }

    #[test]
    fn lifecycle_config_serde_missing_fields() {
        let json = r#"{"enabled": false}"#;
        let cfg: LifecycleConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        // All other fields should get defaults.
        assert_eq!(cfg.sample_interval, Duration::from_secs(30));
        assert_eq!(cfg.trend_window, 60);
        assert!((cfg.warn_age_hours - 16.0).abs() < f64::EPSILON);
        assert!((cfg.kill_age_hours - 24.0).abs() < f64::EPSILON);
        assert_eq!(cfg.grace_period, Duration::from_secs(30));
        assert!((cfg.active_cpu_threshold - 10.0).abs() < f64::EPSILON);
        assert!((cfg.stuck_cpu_threshold - 2.0).abs() < f64::EPSILON);
        assert!((cfg.pressure_renice_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(cfg.renice_value, 19);
        assert!(cfg.protected_panes.is_empty());
    }

    // ========================================================================
    // LifecycleAction Tests
    // ========================================================================

    #[test]
    fn lifecycle_action_serde_roundtrip_none() {
        let action = LifecycleAction::None;
        let json = serde_json::to_string(&action).unwrap();
        let back: LifecycleAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, LifecycleAction::None));
    }

    #[test]
    fn lifecycle_action_serde_roundtrip_warn() {
        let action = LifecycleAction::Warn {
            reason: "test warning".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: LifecycleAction = serde_json::from_str(&json).unwrap();
        match back {
            LifecycleAction::Warn { reason } => assert_eq!(reason, "test warning"),
            other => panic!("Expected Warn, got {:?}", other),
        }
    }

    #[test]
    fn lifecycle_action_serde_roundtrip_force_kill() {
        let action = LifecycleAction::ForceKill {
            reason: "abandoned pane".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: LifecycleAction = serde_json::from_str(&json).unwrap();
        match back {
            LifecycleAction::ForceKill { reason } => assert_eq!(reason, "abandoned pane"),
            other => panic!("Expected ForceKill, got {:?}", other),
        }
    }

    #[test]
    fn lifecycle_action_clone() {
        let none = LifecycleAction::None;
        let cloned_none = none.clone();
        assert!(matches!(cloned_none, LifecycleAction::None));

        let warn = LifecycleAction::Warn {
            reason: "clone test".to_string(),
        };
        let cloned_warn = warn.clone();
        match cloned_warn {
            LifecycleAction::Warn { reason } => assert_eq!(reason, "clone test"),
            other => panic!("Expected Warn, got {:?}", other),
        }

        let kill = LifecycleAction::ForceKill {
            reason: "kill clone".to_string(),
        };
        let cloned_kill = kill.clone();
        match cloned_kill {
            LifecycleAction::ForceKill { reason } => assert_eq!(reason, "kill clone"),
            other => panic!("Expected ForceKill, got {:?}", other),
        }
    }

    #[test]
    fn lifecycle_action_debug() {
        let none_debug = format!("{:?}", LifecycleAction::None);
        assert!(none_debug.contains("None"));

        let warn_debug = format!(
            "{:?}",
            LifecycleAction::Warn {
                reason: "dbg".to_string(),
            }
        );
        assert!(warn_debug.contains("Warn"));

        let kill_debug = format!(
            "{:?}",
            LifecycleAction::ForceKill {
                reason: "dbg".to_string(),
            }
        );
        assert!(kill_debug.contains("ForceKill"));
    }

    // ========================================================================
    // PaneHealthSample Tests
    // ========================================================================

    #[test]
    fn pane_health_sample_clone() {
        use crate::process_tree::PaneActivity;
        let sample = PaneHealthSample {
            pane_id: 42,
            health: PaneHealth::Active,
            activity: PaneActivity::Idle,
            cpu_percent: 55.0,
            rss_kb: 2048,
            child_count: 3,
            age: Duration::from_secs(7200),
            root_pid: 9999,
        };
        let cloned = sample.clone();
        assert_eq!(cloned.pane_id, 42);
        assert_eq!(cloned.health, PaneHealth::Active);
        assert!((cloned.cpu_percent - 55.0).abs() < f64::EPSILON);
        assert_eq!(cloned.rss_kb, 2048);
        assert_eq!(cloned.child_count, 3);
        assert_eq!(cloned.age, Duration::from_secs(7200));
        assert_eq!(cloned.root_pid, 9999);
    }

    #[test]
    fn pane_health_sample_debug() {
        use crate::process_tree::PaneActivity;
        let sample = PaneHealthSample {
            pane_id: 1,
            health: PaneHealth::Thinking,
            activity: PaneActivity::Idle,
            cpu_percent: 0.5,
            rss_kb: 512,
            child_count: 0,
            age: Duration::from_secs(60),
            root_pid: 100,
        };
        let debug = format!("{:?}", sample);
        assert!(debug.contains("PaneHealthSample"));
    }

    // ========================================================================
    // Engine Accessor Tests
    // ========================================================================

    #[test]
    fn engine_config_accessor() {
        let engine = PaneLifecycleEngine::with_defaults();
        let cfg = engine.config();
        assert!(cfg.enabled);
        assert_eq!(cfg.trend_window, 60);
    }

    // ========================================================================
    // Boundary Classification Tests
    // ========================================================================

    #[test]
    fn classify_health_exactly_4h_boundary() {
        let engine = PaneLifecycleEngine::with_defaults();
        // Exactly 4h with high CPU -- still young (<= 4h boundary uses >4.0 check).
        // 4h = 14400s. The check is `hours > 4.0`, so exactly 4.0 stays in the young branch.
        let health_high_cpu = engine.classify_health(Duration::from_secs(14400), 50.0);
        assert_eq!(health_high_cpu, PaneHealth::Active);

        let health_low_cpu = engine.classify_health(Duration::from_secs(14400), 0.5);
        assert_eq!(health_low_cpu, PaneHealth::Thinking);

        // One second past 4h enters mid-age branch.
        let health_past = engine.classify_health(Duration::from_secs(14401), 50.0);
        assert_eq!(health_past, PaneHealth::Working);
    }

    #[test]
    fn classify_health_exactly_warn_boundary() {
        let engine = PaneLifecycleEngine::with_defaults();
        // Exactly 16h = 57600s. The check is `hours > 16.0`, so exactly 16.0 stays in mid-age.
        let health_at = engine.classify_health(Duration::from_secs(57600), 50.0);
        assert_eq!(health_at, PaneHealth::Working);

        // One second past 16h enters LikelyStuck.
        let health_past = engine.classify_health(Duration::from_secs(57601), 50.0);
        assert_eq!(health_past, PaneHealth::LikelyStuck);
    }

    #[test]
    fn classify_health_exactly_kill_boundary() {
        let engine = PaneLifecycleEngine::with_defaults();
        // Exactly 24h = 86400s. The check is `hours > 24.0`, so exactly 24.0 stays LikelyStuck.
        let health_at = engine.classify_health(Duration::from_secs(86400), 0.0);
        assert_eq!(health_at, PaneHealth::LikelyStuck);

        // One second past 24h enters Abandoned.
        let health_past = engine.classify_health(Duration::from_secs(86401), 0.0);
        assert_eq!(health_past, PaneHealth::Abandoned);
    }

    #[test]
    fn classify_health_cpu_at_active_threshold() {
        let engine = PaneLifecycleEngine::with_defaults();
        // active_cpu_threshold is 10.0. Check is `cpu_percent > 10.0`.
        // Exactly 10.0 should NOT be Active (falls to Thinking since 10.0 is not > 10.0).
        let health_at = engine.classify_health(Duration::from_secs(3600), 10.0);
        assert_eq!(health_at, PaneHealth::Thinking);

        // Just above threshold is Active.
        let health_above = engine.classify_health(Duration::from_secs(3600), 10.1);
        assert_eq!(health_above, PaneHealth::Active);
    }

    // ========================================================================
    // Engine State Access Tests
    // ========================================================================

    #[test]
    fn pane_root_pid_accessor() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        // Before health_check, no pane state exists.
        assert_eq!(engine.pane_root_pid(42), None);

        // After health_check, pane_root_pid returns the pid used.
        let _ = engine.health_check(42, 5678, Duration::from_secs(3600), 50.0, None);
        assert_eq!(engine.pane_root_pid(42), Some(5678));
    }

    // ========================================================================
    // Pressure Renice Edge Cases
    // ========================================================================

    #[test]
    fn pressure_renice_working_excluded() {
        let config = LifecycleConfig::default();
        let healths = vec![(1, PaneHealth::Working, Duration::from_secs(8 * 3600))];
        // Above threshold, Working is NOT in the renice filter.
        let candidates = pressure_renice_candidates(&healths, 0.9, &config);
        assert!(candidates.is_empty());
    }

    #[test]
    fn pressure_renice_empty_healths() {
        let config = LifecycleConfig::default();
        let healths: Vec<(u64, PaneHealth, Duration)> = vec![];
        let candidates = pressure_renice_candidates(&healths, 0.9, &config);
        assert!(candidates.is_empty());
    }

    #[test]
    fn pressure_renice_at_exact_threshold() {
        let config = LifecycleConfig::default();
        let healths = vec![
            (1, PaneHealth::Thinking, Duration::from_secs(3600)),
            (2, PaneHealth::PossiblyStuck, Duration::from_secs(8 * 3600)),
        ];
        // Exactly at threshold (0.8). The guard is `cpu_load_fraction < threshold`,
        // so 0.8 < 0.8 is false -- we do NOT early-return. Candidates are produced.
        let candidates = pressure_renice_candidates(&healths, 0.8, &config);
        assert_eq!(
            candidates.len(),
            2,
            "At exact threshold, < guard does not fire so candidates returned"
        );

        // Just below threshold: early return fires.
        let below = pressure_renice_candidates(&healths, 0.79, &config);
        assert!(below.is_empty(), "Below threshold should return empty");
    }

    // ── Telemetry counter tests ──────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let engine = PaneLifecycleEngine::with_defaults();
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.health_checks, 0);
        assert_eq!(snap.panes_registered, 0);
        assert_eq!(snap.panes_removed, 0);
        assert_eq!(snap.actions_none, 0);
        assert_eq!(snap.actions_warn, 0);
        assert_eq!(snap.actions_review, 0);
        assert_eq!(snap.actions_graceful_kill, 0);
        assert_eq!(snap.actions_force_kill, 0);
    }

    #[test]
    fn telemetry_health_check_counted() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 100, Duration::from_secs(60), 5.0, None);
        engine.health_check(1, 100, Duration::from_secs(120), 5.0, None);

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.health_checks, 2);
        assert_eq!(snap.panes_registered, 1); // first check registers pane
    }

    #[test]
    fn telemetry_remove_pane_counted() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 100, Duration::from_secs(60), 5.0, None);
        engine.remove_pane(1);

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.panes_removed, 1);
    }

    #[test]
    fn telemetry_action_counted() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        // A young, healthy pane should get LifecycleAction::None
        engine.health_check(1, 100, Duration::from_secs(10), 5.0, None);

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.actions_none, 1);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(1, 100, Duration::from_secs(60), 5.0, None);

        let snap = engine.telemetry().snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: PaneLifecycleTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.health_checks, snap.health_checks);
        assert_eq!(parsed.panes_registered, snap.panes_registered);
    }
}
