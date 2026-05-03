//! Fleet memory orchestration controller (ft-iehgn.3).
//!
//! Synthesizes decisions across the 5 independent memory subsystems into a
//! unified pressure tier with coordinated action dispatch for 200+ pane swarms.
//!
//! # Subsystems Unified
//!
//! 1. [`BackpressureTier`] — queue-depth driven (Green/Yellow/Red/Black)
//! 2. [`MemoryPressureTier`] — system memory utilization (Green/Yellow/Orange/Red)
//! 3. [`BudgetLevel`] — per-pane memory budget (Normal/Throttled/OverBudget)
//!
//! # Decision Logic
//!
//! The controller maps each subsystem's tier to a unified 4-level
//! [`FleetPressureTier`] and takes the worst-of as the compound tier.
//! Actions escalate monotonically with tier severity.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::backpressure::BackpressureTier;
use crate::memory_budget::BudgetLevel;
use crate::memory_pressure::MemoryPressureTier;

// =============================================================================
// Fleet pressure tier
// =============================================================================

/// Unified fleet-wide pressure tier synthesized from all subsystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetPressureTier {
    /// All subsystems nominal. No action needed.
    Normal,
    /// One or more subsystems at warning level. Throttle non-critical work.
    Elevated,
    /// Significant pressure. Actively shed load and reclaim memory.
    Critical,
    /// Near-failure conditions. Emergency measures required.
    Emergency,
}

impl FleetPressureTier {
    /// Numeric value for gauge metrics (0–3).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Elevated => 1,
            Self::Critical => 2,
            Self::Emergency => 3,
        }
    }
}

impl std::fmt::Display for FleetPressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "NORMAL"),
            Self::Elevated => write!(f, "ELEVATED"),
            Self::Critical => write!(f, "CRITICAL"),
            Self::Emergency => write!(f, "EMERGENCY"),
        }
    }
}

// =============================================================================
// Pressure signals
// =============================================================================

/// Aggregated pressure readings from all subsystems.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureSignals {
    /// Pipeline backpressure tier (queue depths).
    pub backpressure: BackpressureTier,
    /// System memory pressure tier.
    pub memory_pressure: MemoryPressureTier,
    /// Worst per-pane budget level across fleet.
    pub worst_budget: BudgetLevel,
    /// Number of panes currently registered.
    pub pane_count: usize,
    /// Number of panes currently paused by backpressure.
    pub paused_pane_count: usize,
}

// =============================================================================
// Fleet actions
// =============================================================================

/// Recommended fleet-wide action based on compound pressure tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMemoryAction {
    /// No intervention needed.
    None,
    /// Increase idle pane poll intervals, reduce detection frequency.
    ThrottlePolling,
    /// Evict warm scrollback pages to cold tier on idle panes.
    EvictWarmScrollback,
    /// Pause output capture on lowest-priority panes.
    PauseIdlePanes,
    /// Emergency: evict all warm, pause most panes, trigger GC.
    EmergencyCleanup,
}

impl FleetMemoryAction {
    /// Whether this action involves pausing panes.
    #[must_use]
    pub const fn involves_pausing(self) -> bool {
        matches!(self, Self::PauseIdlePanes | Self::EmergencyCleanup)
    }

    /// Whether this action involves scrollback eviction.
    #[must_use]
    pub const fn involves_eviction(self) -> bool {
        matches!(self, Self::EvictWarmScrollback | Self::EmergencyCleanup)
    }
}

// =============================================================================
// Decision record
// =============================================================================

/// A recorded decision with its inputs and outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Evaluation sequence number.
    pub sequence: u64,
    /// Input signals.
    pub signals: PressureSignals,
    /// Compound tier computed.
    pub compound_tier: FleetPressureTier,
    /// Recommended actions.
    pub actions: Vec<FleetMemoryAction>,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for fleet memory controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetMemoryConfig {
    /// Maximum decision records to keep in audit trail.
    pub max_audit_trail: usize,
    /// Minimum evaluations at a tier before escalating (hysteresis).
    pub escalation_threshold: u64,
    /// Minimum evaluations at a tier before de-escalating.
    pub deescalation_threshold: u64,
}

impl Default for FleetMemoryConfig {
    fn default() -> Self {
        Self {
            max_audit_trail: 100,
            escalation_threshold: 3,
            deescalation_threshold: 5,
        }
    }
}

// =============================================================================
// Snapshot
// =============================================================================

/// Serializable snapshot of the fleet memory controller state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetMemorySnapshot {
    /// Current compound pressure tier.
    pub compound_tier: FleetPressureTier,
    /// Total evaluations performed.
    pub total_evaluations: u64,
    /// Total tier transitions.
    pub total_transitions: u64,
    /// Consecutive evaluations at current tier.
    pub consecutive_at_tier: u64,
    /// Last recommended actions.
    pub last_actions: Vec<FleetMemoryAction>,
}

// =============================================================================
// Controller
// =============================================================================

/// Fleet memory orchestration controller.
///
/// Synthesizes decisions across backpressure, memory pressure, and budget
/// subsystems into coordinated fleet-wide actions.
#[derive(Debug)]
pub struct FleetMemoryController {
    config: FleetMemoryConfig,
    /// Current compound tier.
    compound_tier: FleetPressureTier,
    /// Recent raw tiers to compute sliding window hysteresis.
    recent_raw_tiers: VecDeque<FleetPressureTier>,
    /// Total evaluations.
    total_evaluations: u64,
    /// Total tier transitions.
    total_transitions: u64,
    /// Consecutive evaluations at current compound tier.
    consecutive_at_tier: u64,
    /// Last recommended actions.
    last_actions: Vec<FleetMemoryAction>,
    /// Audit trail of recent decisions.
    audit_trail: VecDeque<DecisionRecord>,
}

impl FleetMemoryController {
    /// Create a new controller with given configuration.
    #[must_use]
    pub fn new(config: FleetMemoryConfig) -> Self {
        Self {
            config,
            compound_tier: FleetPressureTier::Normal,
            recent_raw_tiers: VecDeque::new(),
            total_evaluations: 0,
            total_transitions: 0,
            consecutive_at_tier: 0,
            last_actions: vec![FleetMemoryAction::None],
            audit_trail: VecDeque::new(),
        }
    }

    /// Current compound pressure tier.
    #[must_use]
    pub fn compound_tier(&self) -> FleetPressureTier {
        self.compound_tier
    }

    /// Total evaluations performed.
    #[must_use]
    pub fn total_evaluations(&self) -> u64 {
        self.total_evaluations
    }

    /// Total tier transitions.
    #[must_use]
    pub fn total_transitions(&self) -> u64 {
        self.total_transitions
    }

    /// Last recommended actions.
    #[must_use]
    pub fn last_actions(&self) -> &[FleetMemoryAction] {
        &self.last_actions
    }

    /// Audit trail of recent decisions.
    #[must_use]
    pub fn audit_trail(&self) -> &VecDeque<DecisionRecord> {
        &self.audit_trail
    }

    /// Configuration.
    #[must_use]
    pub fn config(&self) -> &FleetMemoryConfig {
        &self.config
    }

    /// Take a snapshot of the controller state.
    #[must_use]
    pub fn snapshot(&self) -> FleetMemorySnapshot {
        FleetMemorySnapshot {
            compound_tier: self.compound_tier,
            total_evaluations: self.total_evaluations,
            total_transitions: self.total_transitions,
            consecutive_at_tier: self.consecutive_at_tier,
            last_actions: self.last_actions.clone(),
        }
    }

    /// Evaluate pressure signals and update compound tier.
    ///
    /// Returns the recommended actions for this evaluation cycle.
    pub fn evaluate(&mut self, signals: &PressureSignals) -> Vec<FleetMemoryAction> {
        self.total_evaluations += 1;

        // Map each subsystem to fleet tier
        let backpressure_tier = map_backpressure(signals.backpressure);
        let mem_pressure_tier = map_memory_pressure(signals.memory_pressure);
        let budget_tier = map_budget_level(signals.worst_budget);

        // Compound: worst-of all subsystems
        let raw = backpressure_tier.max(mem_pressure_tier).max(budget_tier);

        // Hysteresis: require sustained readings before transitioning
        self.recent_raw_tiers.push_back(raw);
        let max_history = self
            .config
            .escalation_threshold
            .max(self.config.deescalation_threshold) as usize;
        if self.recent_raw_tiers.len() > max_history {
            self.recent_raw_tiers.pop_front();
        }

        let esc_thresh = self.config.escalation_threshold as usize;
        let sustained_high = if self.recent_raw_tiers.len() >= esc_thresh && esc_thresh > 0 {
            *self
                .recent_raw_tiers
                .iter()
                .rev()
                .take(esc_thresh)
                .min()
                .unwrap()
        } else {
            FleetPressureTier::Normal
        };

        let deesc_thresh = self.config.deescalation_threshold as usize;
        let sustained_low = if self.recent_raw_tiers.len() >= deesc_thresh && deesc_thresh > 0 {
            *self
                .recent_raw_tiers
                .iter()
                .rev()
                .take(deesc_thresh)
                .max()
                .unwrap()
        } else {
            self.compound_tier
        };

        let mut target_tier = self.compound_tier;
        let mut should_transition = false;

        // Escalation takes precedence
        if sustained_high > self.compound_tier {
            should_transition = true;
            target_tier = sustained_high;
        } else if sustained_low < self.compound_tier {
            should_transition = true;
            target_tier = sustained_low;
        }

        if should_transition {
            self.compound_tier = target_tier;
            self.total_transitions += 1;
            self.consecutive_at_tier = 1;
        } else {
            self.consecutive_at_tier += 1;
        }

        // Determine actions for current compound tier
        let actions = recommend_actions(self.compound_tier, signals);
        self.last_actions.clone_from(&actions);

        // Record decision
        let record = DecisionRecord {
            sequence: self.total_evaluations,
            signals: signals.clone(),
            compound_tier: self.compound_tier,
            actions: actions.clone(),
        };
        self.audit_trail.push_back(record);
        if self.audit_trail.len() > self.config.max_audit_trail {
            self.audit_trail.pop_front();
        }

        actions
    }

    /// Reset the controller to initial state.
    pub fn reset(&mut self) {
        self.compound_tier = FleetPressureTier::Normal;
        self.recent_raw_tiers.clear();
        self.total_evaluations = 0;
        self.total_transitions = 0;
        self.consecutive_at_tier = 0;
        self.last_actions = vec![FleetMemoryAction::None];
        self.audit_trail.clear();
    }
}

impl Default for FleetMemoryController {
    fn default() -> Self {
        Self::new(FleetMemoryConfig::default())
    }
}

// =============================================================================
// Tier mapping functions
// =============================================================================

/// Map `BackpressureTier` to `FleetPressureTier`.
#[must_use]
pub fn map_backpressure(tier: BackpressureTier) -> FleetPressureTier {
    match tier {
        BackpressureTier::Green => FleetPressureTier::Normal,
        BackpressureTier::Yellow => FleetPressureTier::Elevated,
        BackpressureTier::Red => FleetPressureTier::Critical,
        BackpressureTier::Black => FleetPressureTier::Emergency,
    }
}

/// Map `MemoryPressureTier` to `FleetPressureTier`.
#[must_use]
pub fn map_memory_pressure(tier: MemoryPressureTier) -> FleetPressureTier {
    match tier {
        MemoryPressureTier::Green => FleetPressureTier::Normal,
        MemoryPressureTier::Yellow => FleetPressureTier::Elevated,
        MemoryPressureTier::Orange => FleetPressureTier::Critical,
        MemoryPressureTier::Red => FleetPressureTier::Emergency,
    }
}

/// Map `BudgetLevel` to `FleetPressureTier`.
#[must_use]
pub fn map_budget_level(level: BudgetLevel) -> FleetPressureTier {
    match level {
        BudgetLevel::Normal => FleetPressureTier::Normal,
        BudgetLevel::Throttled => FleetPressureTier::Elevated,
        BudgetLevel::OverBudget => FleetPressureTier::Critical,
    }
}

/// Recommend actions for a given compound tier and current signals.
#[must_use]
pub fn recommend_actions(
    tier: FleetPressureTier,
    signals: &PressureSignals,
) -> Vec<FleetMemoryAction> {
    match tier {
        FleetPressureTier::Normal => vec![FleetMemoryAction::None],
        FleetPressureTier::Elevated => {
            let mut actions = vec![FleetMemoryAction::ThrottlePolling];
            // If many panes, also start warming up eviction
            if signals.pane_count > 100 {
                actions.push(FleetMemoryAction::EvictWarmScrollback);
            }
            actions
        }
        FleetPressureTier::Critical => {
            vec![
                FleetMemoryAction::ThrottlePolling,
                FleetMemoryAction::EvictWarmScrollback,
                FleetMemoryAction::PauseIdlePanes,
            ]
        }
        FleetPressureTier::Emergency => {
            vec![FleetMemoryAction::EmergencyCleanup]
        }
    }
}

// =============================================================================
// Fleet scrollback orchestrator
// =============================================================================

/// Per-pane scrollback metadata used for eviction targeting.
///
/// The orchestrator collects these from all panes and sorts them to determine
/// which panes should be evicted first under memory pressure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneScrollbackInfo {
    /// Pane identifier.
    pub pane_id: u64,
    /// Current activity counter from the pane's `TieredScrollback`.
    pub activity_counter: u64,
    /// Warm tier compressed bytes for this pane.
    pub warm_bytes: usize,
    /// Number of warm pages.
    pub warm_pages: usize,
    /// Estimated total memory (hot + warm) in bytes.
    pub estimated_memory_bytes: usize,
}

/// Eviction plan produced by the orchestrator for a single evaluation cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionPlan {
    /// Pane IDs to evict, ordered by priority (most idle first).
    pub targets: Vec<EvictionTarget>,
    /// Fleet pressure tier that triggered this plan.
    pub trigger_tier: FleetPressureTier,
    /// Total warm bytes across fleet before eviction.
    pub fleet_warm_bytes_before: usize,
    /// Target warm bytes after eviction (0 for emergency).
    pub fleet_warm_bytes_target: usize,
}

/// A single pane targeted for eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionTarget {
    /// Pane ID to evict from.
    pub pane_id: u64,
    /// Number of warm pages to evict from this pane.
    pub pages_to_evict: usize,
}

/// Orchestrates fleet-wide scrollback eviction decisions.
///
/// Given the current fleet pressure tier and per-pane scrollback info,
/// produces an `EvictionPlan` that targets idle panes first and evicts
/// proportionally to bring fleet memory under control.
#[derive(Debug)]
pub struct FleetScrollbackOrchestrator {
    /// Activity counter snapshot from the last evaluation cycle.
    /// Used to compute activity delta (idle = counter unchanged).
    last_activity: std::collections::HashMap<u64, u64>,
    /// Total eviction plans produced.
    total_plans: u64,
    /// Total panes targeted for eviction across all plans.
    total_targets: u64,
}

impl FleetScrollbackOrchestrator {
    /// Create a new orchestrator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_activity: std::collections::HashMap::new(),
            total_plans: 0,
            total_targets: 0,
        }
    }

    /// Total eviction plans produced.
    #[must_use]
    pub fn total_plans(&self) -> u64 {
        self.total_plans
    }

    /// Total pane eviction targets across all plans.
    #[must_use]
    pub fn total_targets(&self) -> u64 {
        self.total_targets
    }

    /// Produce an eviction plan based on the current fleet state.
    ///
    /// `panes` should contain scrollback info for every active pane.
    /// The plan targets idle panes (unchanged activity counter) first,
    /// then sorts by warm bytes descending to maximize memory recovery.
    ///
    /// Returns `None` if no eviction is needed (Normal tier or no warm data).
    pub fn plan_eviction(
        &mut self,
        tier: FleetPressureTier,
        panes: &[PaneScrollbackInfo],
    ) -> Option<EvictionPlan> {
        if tier == FleetPressureTier::Normal {
            self.update_activity(panes);
            return None;
        }

        let fleet_warm_bytes: usize = panes.iter().map(|p| p.warm_bytes).sum();
        if fleet_warm_bytes == 0 {
            self.update_activity(panes);
            return None;
        }

        // Determine eviction aggressiveness based on tier
        let (target_fraction, max_pane_fraction) = match tier {
            // br-ft-4l0lk: structurally unreachable — the function's
            // early-exit guard above (`if tier == FleetPressureTier::Normal
            // { return None; }`) ensures Normal never reaches this match.
            // The named message surfaces the invariant if a future refactor
            // ever moves or removes the guard.
            FleetPressureTier::Normal => unreachable!(
                "FleetPressureTier::Normal handled by early-exit guard \
                 above; this match arm is structurally unreachable"
            ),
            FleetPressureTier::Elevated => (0.25, 0.5), // evict 25% of fleet warm, up to 50% per pane
            FleetPressureTier::Critical => (0.75, 1.0), // evict 75% of fleet warm, full per pane
            FleetPressureTier::Emergency => (1.0, 1.0), // evict everything
        };

        let fleet_warm_bytes_target =
            ((fleet_warm_bytes as f64) * (1.0 - target_fraction)) as usize;

        // Score each pane for eviction priority.
        // Lower score = higher eviction priority (evict first).
        // Idle panes (no activity delta) get priority, then sort by warm bytes desc.
        let mut scored: Vec<(u64, u64, usize, usize)> = panes
            .iter()
            .filter(|p| p.warm_pages > 0)
            .map(|p| {
                let prev = self.last_activity.get(&p.pane_id).copied().unwrap_or(0);
                let delta = p.activity_counter.saturating_sub(prev);
                (p.pane_id, delta, p.warm_bytes, p.warm_pages)
            })
            .collect();

        // Sort: idle panes first (delta == 0), then by warm_bytes descending
        scored.sort_by(|a, b| {
            let a_idle = a.1 == 0;
            let b_idle = b.1 == 0;
            match (a_idle, b_idle) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.2.cmp(&a.2), // more warm bytes = evict first
            }
        });

        let mut targets = Vec::new();
        let mut remaining_to_evict = fleet_warm_bytes.saturating_sub(fleet_warm_bytes_target);

        for (pane_id, _delta, warm_bytes, warm_pages) in &scored {
            if remaining_to_evict == 0 {
                break;
            }

            if *warm_bytes == 0 {
                continue;
            }

            let target_frac = remaining_to_evict as f64 / *warm_bytes as f64;
            let capped_frac = target_frac.min(max_pane_fraction).min(1.0);

            let pages_to_evict = (capped_frac * *warm_pages as f64).ceil() as usize;
            let pages_to_evict = pages_to_evict.min(*warm_pages).max(1);

            let bytes_evicted =
                (*warm_bytes as f64 * (pages_to_evict as f64 / *warm_pages as f64)) as usize;
            remaining_to_evict = remaining_to_evict.saturating_sub(bytes_evicted.min(*warm_bytes));

            targets.push(EvictionTarget {
                pane_id: *pane_id,
                pages_to_evict,
            });
        }

        self.update_activity(panes);
        self.total_plans += 1;
        self.total_targets += targets.len() as u64;

        if targets.is_empty() {
            None
        } else {
            Some(EvictionPlan {
                targets,
                trigger_tier: tier,
                fleet_warm_bytes_before: fleet_warm_bytes,
                fleet_warm_bytes_target,
            })
        }
    }

    fn update_activity(&mut self, panes: &[PaneScrollbackInfo]) {
        self.last_activity.clear();
        for p in panes {
            self.last_activity.insert(p.pane_id, p.activity_counter);
        }
    }
}

impl Default for FleetScrollbackOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn green_signals() -> PressureSignals {
        PressureSignals {
            backpressure: BackpressureTier::Green,
            memory_pressure: MemoryPressureTier::Green,
            worst_budget: BudgetLevel::Normal,
            pane_count: 50,
            paused_pane_count: 0,
        }
    }

    fn yellow_signals() -> PressureSignals {
        PressureSignals {
            backpressure: BackpressureTier::Yellow,
            memory_pressure: MemoryPressureTier::Green,
            worst_budget: BudgetLevel::Normal,
            pane_count: 200,
            paused_pane_count: 0,
        }
    }

    fn red_signals() -> PressureSignals {
        PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Throttled,
            pane_count: 200,
            paused_pane_count: 10,
        }
    }

    fn black_signals() -> PressureSignals {
        PressureSignals {
            backpressure: BackpressureTier::Black,
            memory_pressure: MemoryPressureTier::Red,
            worst_budget: BudgetLevel::OverBudget,
            pane_count: 200,
            paused_pane_count: 100,
        }
    }

    // ── Tier mapping ─────────────────────────────────────────────────

    #[test]
    fn map_backpressure_tiers() {
        assert_eq!(
            map_backpressure(BackpressureTier::Green),
            FleetPressureTier::Normal
        );
        assert_eq!(
            map_backpressure(BackpressureTier::Yellow),
            FleetPressureTier::Elevated
        );
        assert_eq!(
            map_backpressure(BackpressureTier::Red),
            FleetPressureTier::Critical
        );
        assert_eq!(
            map_backpressure(BackpressureTier::Black),
            FleetPressureTier::Emergency
        );
    }

    #[test]
    fn map_memory_pressure_tiers() {
        assert_eq!(
            map_memory_pressure(MemoryPressureTier::Green),
            FleetPressureTier::Normal
        );
        assert_eq!(
            map_memory_pressure(MemoryPressureTier::Yellow),
            FleetPressureTier::Elevated
        );
        assert_eq!(
            map_memory_pressure(MemoryPressureTier::Orange),
            FleetPressureTier::Critical
        );
        assert_eq!(
            map_memory_pressure(MemoryPressureTier::Red),
            FleetPressureTier::Emergency
        );
    }

    #[test]
    fn map_budget_levels() {
        assert_eq!(
            map_budget_level(BudgetLevel::Normal),
            FleetPressureTier::Normal
        );
        assert_eq!(
            map_budget_level(BudgetLevel::Throttled),
            FleetPressureTier::Elevated
        );
        assert_eq!(
            map_budget_level(BudgetLevel::OverBudget),
            FleetPressureTier::Critical
        );
    }

    // ── Tier ordering ────────────────────────────────────────────────

    #[test]
    fn fleet_tier_ordering() {
        assert!(FleetPressureTier::Normal < FleetPressureTier::Elevated);
        assert!(FleetPressureTier::Elevated < FleetPressureTier::Critical);
        assert!(FleetPressureTier::Critical < FleetPressureTier::Emergency);
    }

    #[test]
    fn fleet_tier_as_u8_monotonic() {
        let tiers = [
            FleetPressureTier::Normal,
            FleetPressureTier::Elevated,
            FleetPressureTier::Critical,
            FleetPressureTier::Emergency,
        ];
        for i in 1..tiers.len() {
            assert!(tiers[i].as_u8() > tiers[i - 1].as_u8());
        }
    }

    // ── Serde ────────────────────────────────────────────────────────

    #[test]
    fn fleet_tier_serde_roundtrip() {
        for tier in [
            FleetPressureTier::Normal,
            FleetPressureTier::Elevated,
            FleetPressureTier::Critical,
            FleetPressureTier::Emergency,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let rt: FleetPressureTier = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, tier);
        }
    }

    #[test]
    fn fleet_tier_snake_case() {
        assert_eq!(
            serde_json::to_string(&FleetPressureTier::Normal).unwrap(),
            "\"normal\""
        );
        assert_eq!(
            serde_json::to_string(&FleetPressureTier::Elevated).unwrap(),
            "\"elevated\""
        );
        assert_eq!(
            serde_json::to_string(&FleetPressureTier::Critical).unwrap(),
            "\"critical\""
        );
        assert_eq!(
            serde_json::to_string(&FleetPressureTier::Emergency).unwrap(),
            "\"emergency\""
        );
    }

    #[test]
    fn fleet_action_serde_roundtrip() {
        for action in [
            FleetMemoryAction::None,
            FleetMemoryAction::ThrottlePolling,
            FleetMemoryAction::EvictWarmScrollback,
            FleetMemoryAction::PauseIdlePanes,
            FleetMemoryAction::EmergencyCleanup,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let rt: FleetMemoryAction = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, action);
        }
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = FleetMemoryConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let rt: FleetMemoryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.max_audit_trail, config.max_audit_trail);
        assert_eq!(rt.escalation_threshold, config.escalation_threshold);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut ctrl = FleetMemoryController::default();
        ctrl.evaluate(&green_signals());
        let snap = ctrl.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let rt: FleetMemorySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, snap);
    }

    // ── Evaluate ─────────────────────────────────────────────────────

    #[test]
    fn evaluate_green_stays_normal() {
        let mut ctrl = FleetMemoryController::default();
        let actions = ctrl.evaluate(&green_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);
        assert_eq!(actions, vec![FleetMemoryAction::None]);
    }

    #[test]
    fn evaluate_worst_of_subsystems() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1, // immediate escalation for test
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });
        // Memory pressure is Orange (Critical) while backpressure is Green
        let signals = PressureSignals {
            backpressure: BackpressureTier::Green,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: 50,
            paused_pane_count: 0,
        };
        ctrl.evaluate(&signals);
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Critical);
    }

    #[test]
    fn escalation_requires_consecutive_readings() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 3,
            deescalation_threshold: 5,
            ..FleetMemoryConfig::default()
        });

        // First two yellow readings: no escalation yet
        ctrl.evaluate(&yellow_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);
        ctrl.evaluate(&yellow_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);

        // Third yellow reading: escalation triggers
        ctrl.evaluate(&yellow_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);
    }

    #[test]
    fn deescalation_requires_more_consecutive_readings() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 3,
            ..FleetMemoryConfig::default()
        });

        // Escalate to Elevated
        ctrl.evaluate(&yellow_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);

        // First two green readings: still Elevated
        ctrl.evaluate(&green_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);
        ctrl.evaluate(&green_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);

        // Third green: de-escalate
        ctrl.evaluate(&green_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);
    }

    #[test]
    fn transition_count_tracks_changes() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });

        ctrl.evaluate(&green_signals()); // Normal → Normal (no change)
        assert_eq!(ctrl.total_transitions(), 0);

        ctrl.evaluate(&yellow_signals()); // Normal → Elevated
        assert_eq!(ctrl.total_transitions(), 1);

        ctrl.evaluate(&green_signals()); // Elevated → Normal
        assert_eq!(ctrl.total_transitions(), 2);
    }

    // ── Actions ──────────────────────────────────────────────────────

    #[test]
    fn normal_tier_recommends_none() {
        let actions = recommend_actions(FleetPressureTier::Normal, &green_signals());
        assert_eq!(actions, vec![FleetMemoryAction::None]);
    }

    #[test]
    fn elevated_tier_throttles_polling() {
        let actions = recommend_actions(FleetPressureTier::Elevated, &yellow_signals());
        assert!(actions.contains(&FleetMemoryAction::ThrottlePolling));
    }

    #[test]
    fn elevated_tier_evicts_at_high_pane_count() {
        let signals = PressureSignals {
            pane_count: 150,
            ..yellow_signals()
        };
        let actions = recommend_actions(FleetPressureTier::Elevated, &signals);
        assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));
    }

    #[test]
    fn elevated_tier_no_eviction_at_low_pane_count() {
        let signals = PressureSignals {
            pane_count: 50,
            ..yellow_signals()
        };
        let actions = recommend_actions(FleetPressureTier::Elevated, &signals);
        assert!(!actions.contains(&FleetMemoryAction::EvictWarmScrollback));
    }

    #[test]
    fn critical_tier_pauses_panes() {
        let actions = recommend_actions(FleetPressureTier::Critical, &red_signals());
        assert!(actions.contains(&FleetMemoryAction::PauseIdlePanes));
        assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));
        assert!(actions.contains(&FleetMemoryAction::ThrottlePolling));
    }

    #[test]
    fn emergency_tier_triggers_cleanup() {
        let actions = recommend_actions(FleetPressureTier::Emergency, &black_signals());
        assert_eq!(actions, vec![FleetMemoryAction::EmergencyCleanup]);
    }

    #[test]
    fn action_escalation_monotonic() {
        // Actions at higher tiers are supersets of lower tier actions
        // (Emergency is special — it's a single consolidated action)
        let normal = recommend_actions(FleetPressureTier::Normal, &green_signals());
        let elevated = recommend_actions(FleetPressureTier::Elevated, &yellow_signals());
        let critical = recommend_actions(FleetPressureTier::Critical, &red_signals());

        // Normal has no real actions
        assert!(normal.iter().all(|a| *a == FleetMemoryAction::None));
        // Elevated includes throttling
        assert!(elevated.contains(&FleetMemoryAction::ThrottlePolling));
        // Critical includes everything Elevated has plus more
        assert!(critical.contains(&FleetMemoryAction::ThrottlePolling));
        assert!(critical.contains(&FleetMemoryAction::PauseIdlePanes));
    }

    // ── Action predicates ────────────────────────────────────────────

    #[test]
    fn action_involves_pausing() {
        assert!(!FleetMemoryAction::None.involves_pausing());
        assert!(!FleetMemoryAction::ThrottlePolling.involves_pausing());
        assert!(!FleetMemoryAction::EvictWarmScrollback.involves_pausing());
        assert!(FleetMemoryAction::PauseIdlePanes.involves_pausing());
        assert!(FleetMemoryAction::EmergencyCleanup.involves_pausing());
    }

    #[test]
    fn action_involves_eviction() {
        assert!(!FleetMemoryAction::None.involves_eviction());
        assert!(!FleetMemoryAction::ThrottlePolling.involves_eviction());
        assert!(FleetMemoryAction::EvictWarmScrollback.involves_eviction());
        assert!(!FleetMemoryAction::PauseIdlePanes.involves_eviction());
        assert!(FleetMemoryAction::EmergencyCleanup.involves_eviction());
    }

    // ── Audit trail ──────────────────────────────────────────────────

    #[test]
    fn audit_trail_records_decisions() {
        let mut ctrl = FleetMemoryController::default();
        ctrl.evaluate(&green_signals());
        ctrl.evaluate(&yellow_signals());
        assert_eq!(ctrl.audit_trail().len(), 2);
        assert_eq!(ctrl.audit_trail()[0].sequence, 1);
        assert_eq!(ctrl.audit_trail()[1].sequence, 2);
    }

    #[test]
    fn audit_trail_bounded_by_config() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            max_audit_trail: 5,
            ..FleetMemoryConfig::default()
        });
        for _ in 0..10 {
            ctrl.evaluate(&green_signals());
        }
        assert_eq!(ctrl.audit_trail().len(), 5);
        // Oldest should have been evicted
        assert_eq!(ctrl.audit_trail()[0].sequence, 6);
    }

    // ── Reset ────────────────────────────────────────────────────────

    #[test]
    fn reset_clears_all_state() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });
        ctrl.evaluate(&red_signals());
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Critical);

        ctrl.reset();
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);
        assert_eq!(ctrl.total_evaluations(), 0);
        assert_eq!(ctrl.total_transitions(), 0);
        assert!(ctrl.audit_trail().is_empty());
    }

    // ── Scale ────────────────────────────────────────────────────────

    #[test]
    fn evaluate_200_pane_scenario() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });

        // Phase 1: Normal operation with 200 panes
        let normal = PressureSignals {
            backpressure: BackpressureTier::Green,
            memory_pressure: MemoryPressureTier::Green,
            worst_budget: BudgetLevel::Normal,
            pane_count: 200,
            paused_pane_count: 0,
        };
        ctrl.evaluate(&normal);
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);

        // Phase 2: Memory pressure rises
        let pressure = PressureSignals {
            memory_pressure: MemoryPressureTier::Yellow,
            ..normal.clone()
        };
        let actions = ctrl.evaluate(&pressure);
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);
        // With 200 panes, should recommend eviction too
        assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));

        // Phase 3: Backpressure hits Red
        let critical = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Throttled,
            pane_count: 200,
            paused_pane_count: 20,
        };
        let actions = ctrl.evaluate(&critical);
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Critical);
        assert!(actions.contains(&FleetMemoryAction::PauseIdlePanes));

        // Phase 4: Recovery
        ctrl.evaluate(&normal);
        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);
        assert_eq!(ctrl.total_transitions(), 3); // Normal→Elevated→Critical→Normal
    }

    #[test]
    fn snapshot_reflects_evaluate_state() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });
        ctrl.evaluate(&green_signals());
        ctrl.evaluate(&yellow_signals());

        let snap = ctrl.snapshot();
        assert_eq!(snap.compound_tier, FleetPressureTier::Elevated);
        assert_eq!(snap.total_evaluations, 2);
        assert_eq!(snap.total_transitions, 1);
    }

    #[test]
    fn decision_record_contains_signals() {
        let mut ctrl = FleetMemoryController::default();
        let signals = red_signals();
        ctrl.evaluate(&signals);

        let record = &ctrl.audit_trail()[0];
        assert_eq!(record.signals.backpressure, BackpressureTier::Red);
        assert_eq!(record.signals.pane_count, 200);
    }

    // ── FleetScrollbackOrchestrator ─────────────────────────────────

    fn make_pane_info(
        pane_id: u64,
        activity: u64,
        warm_bytes: usize,
        warm_pages: usize,
    ) -> PaneScrollbackInfo {
        PaneScrollbackInfo {
            pane_id,
            activity_counter: activity,
            warm_bytes,
            warm_pages,
            estimated_memory_bytes: warm_bytes + 10_000,
        }
    }

    #[test]
    fn orchestrator_no_eviction_at_normal() {
        let mut orch = FleetScrollbackOrchestrator::new();
        let panes = vec![make_pane_info(1, 100, 5000, 10)];
        let plan = orch.plan_eviction(FleetPressureTier::Normal, &panes);
        assert!(plan.is_none());
    }

    #[test]
    fn orchestrator_no_eviction_when_no_warm_data() {
        let mut orch = FleetScrollbackOrchestrator::new();
        let panes = vec![make_pane_info(1, 100, 0, 0)];
        let plan = orch.plan_eviction(FleetPressureTier::Critical, &panes);
        assert!(plan.is_none());
    }

    #[test]
    fn orchestrator_evicts_at_elevated_with_warm_data() {
        let mut orch = FleetScrollbackOrchestrator::new();
        let panes = vec![
            make_pane_info(1, 100, 5000, 10),
            make_pane_info(2, 200, 3000, 6),
        ];
        let plan = orch.plan_eviction(FleetPressureTier::Elevated, &panes);
        assert!(plan.is_some());
        let plan = plan.unwrap();
        assert_eq!(plan.trigger_tier, FleetPressureTier::Elevated);
        assert!(!plan.targets.is_empty());
    }

    #[test]
    fn orchestrator_targets_idle_panes_first() {
        let mut orch = FleetScrollbackOrchestrator::new();
        // First cycle: set activity baselines
        let panes_initial = vec![
            make_pane_info(1, 100, 5000, 10), // will be idle
            make_pane_info(2, 200, 5000, 10), // will be active
        ];
        orch.plan_eviction(FleetPressureTier::Normal, &panes_initial);

        // Second cycle: pane 1 idle (same activity), pane 2 active (bumped)
        let panes = vec![
            make_pane_info(1, 100, 5000, 10), // idle: counter unchanged
            make_pane_info(2, 300, 5000, 10), // active: counter bumped
        ];
        let plan = orch.plan_eviction(FleetPressureTier::Elevated, &panes);
        assert!(plan.is_some());
        let plan = plan.unwrap();
        // Idle pane (1) should be first target
        assert_eq!(plan.targets[0].pane_id, 1);
    }

    #[test]
    fn orchestrator_emergency_evicts_everything() {
        let mut orch = FleetScrollbackOrchestrator::new();
        let panes = vec![
            make_pane_info(1, 100, 5000, 10),
            make_pane_info(2, 200, 3000, 6),
            make_pane_info(3, 300, 7000, 14),
        ];
        let plan = orch.plan_eviction(FleetPressureTier::Emergency, &panes);
        assert!(plan.is_some());
        let plan = plan.unwrap();
        assert_eq!(plan.fleet_warm_bytes_target, 0);
        // Should target all panes with warm data
        assert_eq!(plan.targets.len(), 3);
    }

    #[test]
    fn orchestrator_tracks_totals() {
        let mut orch = FleetScrollbackOrchestrator::new();
        assert_eq!(orch.total_plans(), 0);
        assert_eq!(orch.total_targets(), 0);

        let panes = vec![make_pane_info(1, 100, 5000, 10)];
        orch.plan_eviction(FleetPressureTier::Critical, &panes);
        assert_eq!(orch.total_plans(), 1);
        assert!(orch.total_targets() >= 1);
    }

    #[test]
    fn orchestrator_eviction_plan_serde_roundtrip() {
        let plan = EvictionPlan {
            targets: vec![
                EvictionTarget {
                    pane_id: 1,
                    pages_to_evict: 5,
                },
                EvictionTarget {
                    pane_id: 2,
                    pages_to_evict: 3,
                },
            ],
            trigger_tier: FleetPressureTier::Critical,
            fleet_warm_bytes_before: 100_000,
            fleet_warm_bytes_target: 25_000,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let rt: EvictionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.targets.len(), 2);
        assert_eq!(rt.targets[0].pane_id, 1);
        assert_eq!(rt.fleet_warm_bytes_before, 100_000);
    }

    #[test]
    fn pane_scrollback_info_serde_roundtrip() {
        let info = PaneScrollbackInfo {
            pane_id: 42,
            activity_counter: 1000,
            warm_bytes: 50_000,
            warm_pages: 20,
            estimated_memory_bytes: 60_000,
        };
        let json = serde_json::to_string(&info).unwrap();
        let rt: PaneScrollbackInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.pane_id, 42);
        assert_eq!(rt.activity_counter, 1000);
    }

    #[test]
    fn orchestrator_200_pane_critical_eviction() {
        let mut orch = FleetScrollbackOrchestrator::new();
        let panes: Vec<PaneScrollbackInfo> = (0..200)
            .map(|i| make_pane_info(i, i * 10, 50_000, 100))
            .collect();

        let plan = orch.plan_eviction(FleetPressureTier::Critical, &panes);
        assert!(plan.is_some());
        let plan = plan.unwrap();
        // At Critical, target 75% eviction
        let total_warm: usize = panes.iter().map(|p| p.warm_bytes).sum();
        assert_eq!(plan.fleet_warm_bytes_before, total_warm);
        assert!(plan.fleet_warm_bytes_target < total_warm);
        assert!(!plan.targets.is_empty());
    }
}
