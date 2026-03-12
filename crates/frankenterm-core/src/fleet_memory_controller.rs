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
    /// Raw (un-hysteresis'd) tier from last evaluation.
    raw_tier: FleetPressureTier,
    /// Consecutive evaluations where raw tier matches.
    consecutive_raw: u64,
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
            raw_tier: FleetPressureTier::Normal,
            consecutive_raw: 0,
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
        let bp_tier = map_backpressure(signals.backpressure);
        let mp_tier = map_memory_pressure(signals.memory_pressure);
        let bl_tier = map_budget_level(signals.worst_budget);

        // Compound: worst-of all subsystems
        let raw = bp_tier.max(mp_tier).max(bl_tier);

        // Hysteresis: require consecutive readings before transitioning
        if raw == self.raw_tier {
            self.consecutive_raw += 1;
        } else {
            self.raw_tier = raw;
            self.consecutive_raw = 1;
        }

        let should_transition = if raw > self.compound_tier {
            // Escalation: require fewer consecutive readings
            self.consecutive_raw >= self.config.escalation_threshold
        } else if raw < self.compound_tier {
            // De-escalation: require more consecutive readings
            self.consecutive_raw >= self.config.deescalation_threshold
        } else {
            false
        };

        if should_transition {
            self.compound_tier = raw;
            self.total_transitions += 1;
            self.consecutive_at_tier = 1;
        } else {
            self.consecutive_at_tier += 1;
        }

        // Determine actions for current compound tier
        let actions = recommend_actions(self.compound_tier, signals);
        self.last_actions = actions.clone();

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
        self.raw_tier = FleetPressureTier::Normal;
        self.consecutive_raw = 0;
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
        assert_eq!(map_backpressure(BackpressureTier::Green), FleetPressureTier::Normal);
        assert_eq!(map_backpressure(BackpressureTier::Yellow), FleetPressureTier::Elevated);
        assert_eq!(map_backpressure(BackpressureTier::Red), FleetPressureTier::Critical);
        assert_eq!(map_backpressure(BackpressureTier::Black), FleetPressureTier::Emergency);
    }

    #[test]
    fn map_memory_pressure_tiers() {
        assert_eq!(map_memory_pressure(MemoryPressureTier::Green), FleetPressureTier::Normal);
        assert_eq!(map_memory_pressure(MemoryPressureTier::Yellow), FleetPressureTier::Elevated);
        assert_eq!(map_memory_pressure(MemoryPressureTier::Orange), FleetPressureTier::Critical);
        assert_eq!(map_memory_pressure(MemoryPressureTier::Red), FleetPressureTier::Emergency);
    }

    #[test]
    fn map_budget_levels() {
        assert_eq!(map_budget_level(BudgetLevel::Normal), FleetPressureTier::Normal);
        assert_eq!(map_budget_level(BudgetLevel::Throttled), FleetPressureTier::Elevated);
        assert_eq!(map_budget_level(BudgetLevel::OverBudget), FleetPressureTier::Critical);
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
        assert_eq!(serde_json::to_string(&FleetPressureTier::Normal).unwrap(), "\"normal\"");
        assert_eq!(serde_json::to_string(&FleetPressureTier::Elevated).unwrap(), "\"elevated\"");
        assert_eq!(serde_json::to_string(&FleetPressureTier::Critical).unwrap(), "\"critical\"");
        assert_eq!(serde_json::to_string(&FleetPressureTier::Emergency).unwrap(), "\"emergency\"");
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
        assert!(elevated.iter().any(|a| *a == FleetMemoryAction::ThrottlePolling));
        // Critical includes everything Elevated has plus more
        assert!(critical.iter().any(|a| *a == FleetMemoryAction::ThrottlePolling));
        assert!(critical.iter().any(|a| *a == FleetMemoryAction::PauseIdlePanes));
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
}
