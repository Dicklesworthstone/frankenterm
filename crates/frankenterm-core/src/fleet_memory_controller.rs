//! Fleet memory orchestration controller (ft-iehgn.3).
//!
//! Synthesizes decisions across memory pressure signals and operator-visible
//! tier budgets into a unified pressure tier with coordinated action dispatch
//! for 200+ pane swarms.
//!
//! # Subsystems Unified
//!
//! 1. [`BackpressureTier`] — queue-depth driven (Green/Yellow/Red/Black)
//! 2. [`MemoryPressureTier`] — system memory utilization (Green/Yellow/Orange/Red)
//! 3. [`BudgetLevel`] — per-pane memory budget (Normal/Throttled/OverBudget)
//! 4. [`FleetMemoryTierBudgetSnapshot`] — hot/warm/cold/cache/buffer budgets
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
// Operator-visible tier budget model
// =============================================================================

/// Operator-visible memory tiers coordinated by the fleet governor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMemoryTier {
    /// Hot resident pane/session data that directly contributes to RSS.
    HotResident,
    /// Warm compressed scrollback that can be evicted to the cold disk tier.
    WarmCompressed,
    /// Cold disk-backed history. This is budgeted, but does not reclaim RSS.
    ColdDisk,
    /// Lexical/semantic search and index caches that can be rebuilt.
    SearchIndexCache,
    /// GUI/render cache data that can be repopulated from authoritative state.
    RenderCache,
    /// Short-lived ingestion buffers, decode queues, and staging payloads.
    TransientIngestion,
    /// Allocator arenas and reusable pools that can be trimmed under pressure.
    AllocatorPools,
}

impl FleetMemoryTier {
    /// Stable display order for operator surfaces.
    pub const ALL: [Self; 7] = [
        Self::HotResident,
        Self::WarmCompressed,
        Self::ColdDisk,
        Self::SearchIndexCache,
        Self::RenderCache,
        Self::TransientIngestion,
        Self::AllocatorPools,
    ];

    /// Lower values reclaim first.
    #[must_use]
    pub const fn reclamation_priority(self) -> u8 {
        match self {
            Self::TransientIngestion => 10,
            Self::SearchIndexCache => 20,
            Self::RenderCache => 30,
            Self::WarmCompressed => 40,
            Self::AllocatorPools => 50,
            Self::HotResident => 60,
            Self::ColdDisk => u8::MAX,
        }
    }

    /// Whether bytes in this tier contribute to resident memory pressure.
    #[must_use]
    pub const fn is_resident(self) -> bool {
        !matches!(self, Self::ColdDisk)
    }

    /// Reclamation action for this tier, if reclaiming it can reduce RSS.
    #[must_use]
    pub const fn reclamation_action(self) -> Option<FleetMemoryTierReclamationAction> {
        match self {
            Self::TransientIngestion => {
                Some(FleetMemoryTierReclamationAction::DropTransientBuffers)
            }
            Self::SearchIndexCache => Some(FleetMemoryTierReclamationAction::ShrinkSearchCache),
            Self::RenderCache => Some(FleetMemoryTierReclamationAction::ShrinkRenderCache),
            Self::WarmCompressed => Some(FleetMemoryTierReclamationAction::EvictWarmToCold),
            Self::AllocatorPools => Some(FleetMemoryTierReclamationAction::TrimAllocatorPools),
            Self::HotResident => Some(FleetMemoryTierReclamationAction::DemoteHotToWarm),
            Self::ColdDisk => None,
        }
    }
}

/// Concrete reclamation operation the governor can request for a tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMemoryTierReclamationAction {
    /// Drop or shrink transient ingestion buffers.
    DropTransientBuffers,
    /// Shrink search/index caches without deleting authoritative indexes.
    ShrinkSearchCache,
    /// Shrink render caches without changing terminal state.
    ShrinkRenderCache,
    /// Evict warm compressed scrollback pages to the cold disk tier.
    EvictWarmToCold,
    /// Return idle allocator pool memory to the system allocator.
    TrimAllocatorPools,
    /// Demote hot resident history to warm compressed storage.
    DemoteHotToWarm,
}

/// Per-tier budget and observed accounting counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMemoryTierBudgetRecord {
    /// Tier being accounted.
    pub tier: FleetMemoryTier,
    /// Operator-visible budget for the tier.
    pub budget_bytes: u64,
    /// Current observed bytes for the tier.
    pub actual_bytes: u64,
    /// Bytes that could be reclaimed without losing authoritative history/state.
    pub reclaimable_bytes: u64,
    /// Cumulative bytes reclaimed from this tier.
    pub reclaimed_bytes: u64,
    /// Cumulative bytes evicted from this tier into a colder tier.
    pub evicted_bytes: u64,
    /// Bytes refused admission because this tier was at budget.
    pub refused_bytes: u64,
}

impl FleetMemoryTierBudgetRecord {
    /// Construct a tier budget record with no prior reclamation counters.
    #[must_use]
    pub const fn new(tier: FleetMemoryTier, budget_bytes: u64, actual_bytes: u64) -> Self {
        Self {
            tier,
            budget_bytes,
            actual_bytes,
            reclaimable_bytes: actual_bytes,
            reclaimed_bytes: 0,
            evicted_bytes: 0,
            refused_bytes: 0,
        }
    }

    /// Set the current reclaimable byte estimate.
    #[must_use]
    pub const fn with_reclaimable_bytes(mut self, reclaimable_bytes: u64) -> Self {
        self.reclaimable_bytes = reclaimable_bytes;
        self
    }

    /// Set cumulative reclamation/eviction/refusal counters for this tier.
    #[must_use]
    pub const fn with_counters(
        mut self,
        reclaimed_bytes: u64,
        evicted_bytes: u64,
        refused_bytes: u64,
    ) -> Self {
        self.reclaimed_bytes = reclaimed_bytes;
        self.evicted_bytes = evicted_bytes;
        self.refused_bytes = refused_bytes;
        self
    }

    /// Bytes above this tier's budget.
    #[must_use]
    pub const fn over_budget_bytes(&self) -> u64 {
        self.actual_bytes.saturating_sub(self.budget_bytes)
    }

    /// Remaining byte budget before this tier starts refusing or reclaiming.
    #[must_use]
    pub const fn remaining_budget_bytes(&self) -> u64 {
        self.budget_bytes.saturating_sub(self.actual_bytes)
    }

    /// Whether this tier currently has budget pressure.
    #[must_use]
    pub const fn has_pressure(&self) -> bool {
        self.actual_bytes > self.budget_bytes || self.refused_bytes > 0
    }
}

/// Aggregate budget/actual/counter totals across all memory tiers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMemoryTierBudgetTotals {
    /// Total budgeted bytes across all tiers.
    pub budget_bytes: u64,
    /// Total budgeted bytes across RSS-contributing tiers.
    pub resident_budget_bytes: u64,
    /// Total observed bytes across all tiers.
    pub actual_bytes: u64,
    /// Total observed resident bytes, excluding cold disk.
    pub resident_actual_bytes: u64,
    /// Sum of each tier's over-budget bytes.
    pub over_budget_bytes: u64,
    /// Sum of RSS-contributing tiers' over-budget bytes.
    pub resident_over_budget_bytes: u64,
    /// Bytes that can be reclaimed without losing authoritative state/history.
    pub reclaimable_bytes: u64,
    /// Cumulative reclaimed bytes across tiers.
    pub reclaimed_bytes: u64,
    /// Cumulative evicted bytes across tiers.
    pub evicted_bytes: u64,
    /// Cumulative refused bytes across tiers.
    pub refused_bytes: u64,
}

impl FleetMemoryTierBudgetTotals {
    fn add_record(&mut self, record: &FleetMemoryTierBudgetRecord) {
        self.budget_bytes = self.budget_bytes.saturating_add(record.budget_bytes);
        self.actual_bytes = self.actual_bytes.saturating_add(record.actual_bytes);
        if record.tier.is_resident() {
            self.resident_budget_bytes = self
                .resident_budget_bytes
                .saturating_add(record.budget_bytes);
            self.resident_actual_bytes = self
                .resident_actual_bytes
                .saturating_add(record.actual_bytes);
            self.resident_over_budget_bytes = self
                .resident_over_budget_bytes
                .saturating_add(record.over_budget_bytes());
        }
        self.over_budget_bytes = self
            .over_budget_bytes
            .saturating_add(record.over_budget_bytes());
        self.reclaimable_bytes = self
            .reclaimable_bytes
            .saturating_add(record.reclaimable_bytes);
        self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(record.reclaimed_bytes);
        self.evicted_bytes = self.evicted_bytes.saturating_add(record.evicted_bytes);
        self.refused_bytes = self.refused_bytes.saturating_add(record.refused_bytes);
    }
}

/// Operator-visible memory budget snapshot consumed by the fleet governor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMemoryTierBudgetSnapshot {
    /// Per-tier records in stable [`FleetMemoryTier`] order.
    pub tiers: Vec<FleetMemoryTierBudgetRecord>,
    /// Precomputed aggregate counters for dashboards and audit records.
    pub totals: FleetMemoryTierBudgetTotals,
}

impl FleetMemoryTierBudgetSnapshot {
    /// Build a deterministic snapshot from per-tier records.
    #[must_use]
    pub fn from_tiers<I>(tiers: I) -> Self
    where
        I: IntoIterator<Item = FleetMemoryTierBudgetRecord>,
    {
        let mut tiers: Vec<FleetMemoryTierBudgetRecord> = tiers.into_iter().collect();
        tiers.sort_by_key(|record| record.tier);

        let mut totals = FleetMemoryTierBudgetTotals::default();
        for record in &tiers {
            totals.add_record(record);
        }

        Self { tiers, totals }
    }

    /// Derive the fleet pressure tier represented by this budget snapshot.
    #[must_use]
    pub fn pressure_tier(&self) -> FleetPressureTier {
        if self.totals.refused_bytes > 0 && self.totals.resident_over_budget_bytes > 0 {
            return FleetPressureTier::Emergency;
        }
        if self.totals.refused_bytes > 0 {
            return FleetPressureTier::Critical;
        }
        if self.totals.resident_over_budget_bytes == 0 {
            return FleetPressureTier::Normal;
        }
        if self.totals.resident_budget_bytes == 0 {
            return FleetPressureTier::Emergency;
        }

        let over_pct_x100 = self
            .totals
            .resident_over_budget_bytes
            .saturating_mul(10_000)
            / self.totals.resident_budget_bytes;

        if over_pct_x100 <= 500 {
            FleetPressureTier::Elevated
        } else if over_pct_x100 <= 2_500 {
            FleetPressureTier::Critical
        } else {
            FleetPressureTier::Emergency
        }
    }

    /// Build RSS-reducing reclamation targets for the current over-budget bytes.
    #[must_use]
    pub fn reclamation_targets(&self) -> Vec<FleetMemoryTierReclamationTarget> {
        let mut remaining = self.totals.resident_over_budget_bytes;
        if remaining == 0 {
            return Vec::new();
        }

        let mut candidates: Vec<&FleetMemoryTierBudgetRecord> = self
            .tiers
            .iter()
            .filter(|record| {
                record.tier.reclamation_action().is_some() && record.reclaimable_bytes > 0
            })
            .collect();
        candidates.sort_by_key(|record| (record.tier.reclamation_priority(), record.tier));

        let mut targets = Vec::new();
        for record in candidates {
            if remaining == 0 {
                break;
            }
            let bytes_to_reclaim = remaining.min(record.reclaimable_bytes);
            if bytes_to_reclaim == 0 {
                continue;
            }
            targets.push(FleetMemoryTierReclamationTarget {
                tier: record.tier,
                action: record
                    .tier
                    .reclamation_action()
                    .expect("candidate filter requires reclaimable tier"),
                bytes_to_reclaim,
                over_budget_bytes: record.over_budget_bytes(),
            });
            remaining = remaining.saturating_sub(bytes_to_reclaim);
        }

        targets
    }
}

/// Reclamation target emitted for a single memory tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMemoryTierReclamationTarget {
    /// Tier to reclaim from.
    pub tier: FleetMemoryTier,
    /// Concrete action to take for this tier.
    pub action: FleetMemoryTierReclamationAction,
    /// Bytes the controller wants reclaimed from this tier.
    pub bytes_to_reclaim: u64,
    /// Current over-budget bytes for this tier.
    pub over_budget_bytes: u64,
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
    /// Optional operator-visible tier budget snapshot used in the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_budget: Option<FleetMemoryTierBudgetSnapshot>,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for fleet memory controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetMemoryConfig {
    /// Maximum decision records to keep in audit trail.
    ///
    /// Caller-supplied values are normalized to
    /// `[1, MAX_FLEET_MEMORY_AUDIT_RECORDS]` at the
    /// [`FleetMemoryController::new`] boundary.
    pub max_audit_trail: usize,
    /// Minimum evaluations at a tier before escalating (hysteresis).
    pub escalation_threshold: u64,
    /// Minimum evaluations at a tier before de-escalating.
    pub deescalation_threshold: u64,
}

/// Hard upper bound on retained fleet-memory decision records.
///
/// The controller is process-global and evaluates on every backpressure cycle,
/// so a restored or malformed config must not be able to turn the audit trail
/// into an effectively unbounded in-memory log.
pub const MAX_FLEET_MEMORY_AUDIT_RECORDS: usize = 4096;

impl Default for FleetMemoryConfig {
    fn default() -> Self {
        Self {
            max_audit_trail: 100,
            escalation_threshold: 3,
            deescalation_threshold: 5,
        }
    }
}

/// br-ft-diccw: why a `FleetMemoryConfig` was rejected by validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetMemoryConfigError {
    /// `escalation_threshold == 0`. Combined with the
    /// `len() >= esc && esc > 0` guard at the evaluate path, a
    /// zero threshold makes `sustained_high` always `Normal`,
    /// silently disabling all tier escalations.
    ZeroEscalationThreshold,
    /// `deescalation_threshold == 0`. Same shape: `sustained_low`
    /// defaults to `compound_tier`, so de-escalation never fires.
    ZeroDeescalationThreshold,
    /// `max_audit_trail == 0`. Each evaluation pushes a record and
    /// then immediately drops it (the `len() > 0` trim trips on
    /// every cycle), leaving the audit trail permanently empty.
    /// The disable knob is "don't read audit_trail()", not zero
    /// the cap.
    ZeroMaxAuditTrail,
}

impl std::fmt::Display for FleetMemoryConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroEscalationThreshold => write!(
                f,
                "br-ft-diccw: escalation_threshold must be >= 1 (zero silently disables tier escalation)"
            ),
            Self::ZeroDeescalationThreshold => write!(
                f,
                "br-ft-diccw: deescalation_threshold must be >= 1 (zero silently disables tier de-escalation)"
            ),
            Self::ZeroMaxAuditTrail => write!(
                f,
                "br-ft-diccw: max_audit_trail must be >= 1 (zero leaves the audit trail permanently empty)"
            ),
        }
    }
}

impl std::error::Error for FleetMemoryConfigError {}

impl FleetMemoryConfig {
    /// Clamp `max_audit_trail` to the finite retention range used by the
    /// controller. Zero becomes a one-record ring rather than disabling
    /// retention; callers that want no audit reads should ignore
    /// [`FleetMemoryController::audit_trail`].
    #[must_use]
    pub fn normalized_max_audit_trail(&self) -> usize {
        self.max_audit_trail
            .clamp(1, MAX_FLEET_MEMORY_AUDIT_RECORDS)
    }

    /// br-ft-diccw: validate the controller's hysteresis thresholds
    /// before installing the config. A zero `escalation_threshold`
    /// or `deescalation_threshold` silently disables tier
    /// transitions because the per-cycle guard at
    /// `fleet_memory_controller.rs:298` requires `esc > 0` to
    /// compute a non-default `sustained_high`. Catch the misconfig
    /// at construction so the operator sees an explicit error
    /// instead of a stuck-at-Normal compound tier.
    pub fn validate(&self) -> Result<(), FleetMemoryConfigError> {
        if self.escalation_threshold == 0 {
            return Err(FleetMemoryConfigError::ZeroEscalationThreshold);
        }
        if self.deescalation_threshold == 0 {
            return Err(FleetMemoryConfigError::ZeroDeescalationThreshold);
        }
        if self.max_audit_trail == 0 {
            return Err(FleetMemoryConfigError::ZeroMaxAuditTrail);
        }
        Ok(())
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
    /// Last operator-visible tier budget snapshot evaluated by the controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_budget: Option<FleetMemoryTierBudgetSnapshot>,
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
    /// Last tier budget snapshot evaluated by the controller.
    last_tier_budget: Option<FleetMemoryTierBudgetSnapshot>,
    /// Audit trail of recent decisions.
    audit_trail: VecDeque<DecisionRecord>,
}

impl FleetMemoryController {
    /// Create a new controller with given configuration.
    #[must_use]
    pub fn new(mut config: FleetMemoryConfig) -> Self {
        config.max_audit_trail = config.normalized_max_audit_trail();
        Self {
            config,
            compound_tier: FleetPressureTier::Normal,
            recent_raw_tiers: VecDeque::new(),
            total_evaluations: 0,
            total_transitions: 0,
            consecutive_at_tier: 0,
            last_actions: vec![FleetMemoryAction::None],
            last_tier_budget: None,
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
            tier_budget: self.last_tier_budget.clone(),
        }
    }

    /// Evaluate pressure signals and update compound tier.
    ///
    /// Returns the recommended actions for this evaluation cycle.
    pub fn evaluate(&mut self, signals: &PressureSignals) -> Vec<FleetMemoryAction> {
        self.evaluate_inner(signals, None)
    }

    /// Evaluate pressure signals plus an operator-visible tier budget snapshot.
    ///
    /// Existing callers can continue using [`Self::evaluate`]. New runtime,
    /// GUI, and search-cache callers can feed this snapshot to make tier-local
    /// overages/refusals visible in pressure, audit, and snapshot surfaces.
    pub fn evaluate_with_tier_budget(
        &mut self,
        signals: &PressureSignals,
        tier_budget: FleetMemoryTierBudgetSnapshot,
    ) -> Vec<FleetMemoryAction> {
        self.evaluate_inner(signals, Some(tier_budget))
    }

    fn evaluate_inner(
        &mut self,
        signals: &PressureSignals,
        tier_budget: Option<FleetMemoryTierBudgetSnapshot>,
    ) -> Vec<FleetMemoryAction> {
        self.total_evaluations = self.total_evaluations.saturating_add(1);

        // Map each subsystem to fleet tier
        let backpressure_tier = map_backpressure(signals.backpressure);
        let mem_pressure_tier = map_memory_pressure(signals.memory_pressure);
        let budget_tier = map_budget_level(signals.worst_budget);
        let tier_budget_pressure = tier_budget
            .as_ref()
            .map_or(FleetPressureTier::Normal, |snapshot| {
                snapshot.pressure_tier()
            });

        // Compound: worst-of all subsystems
        let raw = backpressure_tier
            .max(mem_pressure_tier)
            .max(budget_tier)
            .max(tier_budget_pressure);

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
            self.total_transitions = self.total_transitions.saturating_add(1);
            self.consecutive_at_tier = 1;
        } else {
            self.consecutive_at_tier = self.consecutive_at_tier.saturating_add(1);
        }

        // Determine actions for current compound tier.
        let mut actions = recommend_actions(self.compound_tier, signals);
        add_tier_budget_reclamation_actions(&mut actions, tier_budget.as_ref());
        self.last_actions.clone_from(&actions);
        self.last_tier_budget.clone_from(&tier_budget);

        // Record decision
        let record = DecisionRecord {
            sequence: self.total_evaluations,
            signals: signals.clone(),
            compound_tier: self.compound_tier,
            actions: actions.clone(),
            tier_budget,
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
        self.last_tier_budget = None;
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

fn add_tier_budget_reclamation_actions(
    actions: &mut Vec<FleetMemoryAction>,
    tier_budget: Option<&FleetMemoryTierBudgetSnapshot>,
) {
    let Some(tier_budget) = tier_budget else {
        return;
    };
    if actions.contains(&FleetMemoryAction::EmergencyCleanup) {
        return;
    }
    let should_evict_scrollback = tier_budget.reclamation_targets().iter().any(|target| {
        matches!(
            target.action,
            FleetMemoryTierReclamationAction::EvictWarmToCold
                | FleetMemoryTierReclamationAction::DemoteHotToWarm
        )
    });
    if should_evict_scrollback && !actions.contains(&FleetMemoryAction::EvictWarmScrollback) {
        if actions == &[FleetMemoryAction::None] {
            actions.clear();
        }
        actions.push(FleetMemoryAction::EvictWarmScrollback);
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

        let fleet_warm_bytes = panes
            .iter()
            .fold(0usize, |total, pane| total.saturating_add(pane.warm_bytes));
        if fleet_warm_bytes == 0 {
            self.update_activity(panes);
            return None;
        }

        // Determine eviction aggressiveness based on tier using exact integer
        // ratios; fleet totals may be large enough that f64 loses bytes/pages.
        let (target_retained_numerator, target_retained_denominator, max_pane_numerator) =
            match tier {
                // br-ft-4l0lk: structurally unreachable — the function's
                // early-exit guard above (`if tier == FleetPressureTier::Normal
                // { return None; }`) ensures Normal never reaches this match.
                // The named message surfaces the invariant if a future refactor
                // ever moves or removes the guard.
                FleetPressureTier::Normal => unreachable!(
                    "FleetPressureTier::Normal handled by early-exit guard \
                 above; this match arm is structurally unreachable"
                ),
                FleetPressureTier::Elevated => (3, 4, 1), // retain 75%, up to 50% per pane
                FleetPressureTier::Critical => (1, 4, 2), // retain 25%, full per pane
                FleetPressureTier::Emergency => (0, 1, 2), // evict everything
            };

        let fleet_warm_bytes_target = floor_fraction_usize(
            fleet_warm_bytes,
            target_retained_numerator,
            target_retained_denominator,
        );

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

            let pages_by_remaining =
                ceil_mul_div_usize(*warm_pages, remaining_to_evict, *warm_bytes);
            let max_pages_for_pane = ceil_mul_div_usize(*warm_pages, max_pane_numerator, 2);
            let pages_to_evict = pages_by_remaining
                .min(max_pages_for_pane)
                .min(*warm_pages)
                .max(1);

            let bytes_evicted =
                floor_mul_div_usize(*warm_bytes, pages_to_evict, *warm_pages).min(*warm_bytes);
            remaining_to_evict = remaining_to_evict.saturating_sub(bytes_evicted);

            targets.push(EvictionTarget {
                pane_id: *pane_id,
                pages_to_evict,
            });
        }

        self.update_activity(panes);
        self.total_plans = self.total_plans.saturating_add(1);
        self.total_targets = self
            .total_targets
            .saturating_add(u64::try_from(targets.len()).unwrap_or(u64::MAX));

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

fn floor_fraction_usize(value: usize, numerator: usize, denominator: usize) -> usize {
    debug_assert!(numerator <= denominator);
    debug_assert!(denominator > 0);

    let quotient = value / denominator;
    let remainder = value % denominator;
    quotient
        .saturating_mul(numerator)
        .saturating_add(remainder.saturating_mul(numerator) / denominator)
}

fn ceil_mul_div_usize(multiplicand: usize, numerator: usize, denominator: usize) -> usize {
    if numerator == 0 {
        return 0;
    }

    let product = u128::from(multiplicand).saturating_mul(u128::from(numerator));
    let denominator = u128::from(denominator);
    debug_assert!(denominator > 0);

    let rounded = product.div_ceil(denominator);
    usize::try_from(rounded).unwrap_or(usize::MAX)
}

fn floor_mul_div_usize(multiplicand: usize, numerator: usize, denominator: usize) -> usize {
    let product = u128::from(multiplicand).saturating_mul(u128::from(numerator));
    let denominator = u128::from(denominator);
    debug_assert!(denominator > 0);

    usize::try_from(product / denominator).unwrap_or(usize::MAX)
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
    use crate::byte_compression::CompressionLevel;
    use crate::scrollback_tiers::{
        ColdPageData, ColdRetrievalError, ColdTierRetriever, ScrollbackConfig,
        ScrollbackLocationHint, TieredScrollback,
    };

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

    fn ft_08wlf_stress_line(pane_id: u64, line_no: usize) -> String {
        format!(
            "pane-{pane_id:03}-line-{line_no:05}: deterministic cold-query payload for ft-08wlf"
        )
    }

    fn ft_08wlf_pane_info(pane_id: u64, scrollback: &TieredScrollback) -> PaneScrollbackInfo {
        let snap = scrollback.snapshot();
        PaneScrollbackInfo {
            pane_id,
            activity_counter: snap.activity_counter,
            warm_bytes: snap.warm_bytes,
            warm_pages: snap.warm_pages,
            estimated_memory_bytes: scrollback.estimated_memory_bytes(),
        }
    }

    struct GeneratedColdHistory {
        pane_id: u64,
        page_size: usize,
        lines_per_pane: usize,
    }

    impl ColdTierRetriever for GeneratedColdHistory {
        fn retrieve_page(&self, page_index: u64) -> Result<ColdPageData, ColdRetrievalError> {
            let start = usize::try_from(page_index)
                .ok()
                .and_then(|page| page.checked_mul(self.page_size))
                .ok_or(ColdRetrievalError::PageNotFound { page_index })?;
            if start >= self.lines_per_pane {
                return Err(ColdRetrievalError::PageNotFound { page_index });
            }
            let end = start
                .saturating_add(self.page_size)
                .min(self.lines_per_pane);
            Ok(ColdPageData {
                page_index,
                lines: (start..end)
                    .map(|line_no| ft_08wlf_stress_line(self.pane_id, line_no))
                    .collect(),
            })
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
    fn tier_budget_snapshot_aggregates_operator_visible_counters_ft_08wlf() {
        let snapshot = FleetMemoryTierBudgetSnapshot::from_tiers([
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::RenderCache, 100, 180)
                .with_reclaimable_bytes(120)
                .with_counters(20, 0, 0),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::ColdDisk, 5_000, 4_000)
                .with_reclaimable_bytes(0)
                .with_counters(0, 300, 7),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::WarmCompressed, 500, 650)
                .with_reclaimable_bytes(500)
                .with_counters(100, 200, 0),
        ]);

        assert_eq!(snapshot.tiers[0].tier, FleetMemoryTier::WarmCompressed);
        assert_eq!(snapshot.tiers[1].tier, FleetMemoryTier::ColdDisk);
        assert_eq!(snapshot.tiers[2].tier, FleetMemoryTier::RenderCache);
        assert_eq!(snapshot.totals.budget_bytes, 5_600);
        assert_eq!(snapshot.totals.resident_budget_bytes, 600);
        assert_eq!(snapshot.totals.actual_bytes, 4_830);
        assert_eq!(snapshot.totals.resident_actual_bytes, 830);
        assert_eq!(snapshot.totals.over_budget_bytes, 230);
        assert_eq!(snapshot.totals.resident_over_budget_bytes, 230);
        assert_eq!(snapshot.totals.reclaimable_bytes, 620);
        assert_eq!(snapshot.totals.reclaimed_bytes, 120);
        assert_eq!(snapshot.totals.evicted_bytes, 500);
        assert_eq!(snapshot.totals.refused_bytes, 7);
        assert_eq!(snapshot.pressure_tier(), FleetPressureTier::Emergency);
    }

    #[test]
    fn tier_budget_reclamation_targets_prioritize_rebuildable_state_ft_08wlf() {
        let snapshot = FleetMemoryTierBudgetSnapshot::from_tiers([
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::HotResident, 1_000, 1_300)
                .with_reclaimable_bytes(300),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::WarmCompressed, 2_000, 2_500)
                .with_reclaimable_bytes(500),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::ColdDisk, 2_000, 1_500)
                .with_reclaimable_bytes(500),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::SearchIndexCache, 1_000, 800)
                .with_reclaimable_bytes(300),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::RenderCache, 100, 400)
                .with_reclaimable_bytes(300),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::TransientIngestion, 100, 300)
                .with_reclaimable_bytes(100),
        ]);

        let targets = snapshot.reclamation_targets();
        let target_tiers: Vec<FleetMemoryTier> = targets.iter().map(|target| target.tier).collect();
        assert_eq!(
            target_tiers,
            vec![
                FleetMemoryTier::TransientIngestion,
                FleetMemoryTier::SearchIndexCache,
                FleetMemoryTier::RenderCache,
                FleetMemoryTier::WarmCompressed,
                FleetMemoryTier::HotResident,
            ]
        );
        assert_eq!(
            targets
                .iter()
                .map(|target| target.bytes_to_reclaim)
                .sum::<u64>(),
            snapshot.totals.resident_over_budget_bytes
        );
        assert_eq!(
            targets[3].action,
            FleetMemoryTierReclamationAction::EvictWarmToCold
        );
        assert!(!target_tiers.contains(&FleetMemoryTier::ColdDisk));
    }

    #[test]
    fn evaluate_with_tier_budget_escalates_and_audits_snapshot_ft_08wlf() {
        let tier_budget =
            FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
                FleetMemoryTier::HotResident,
                1_000,
                1_150,
            )
            .with_reclaimable_bytes(150)]);
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });

        let actions = ctrl.evaluate_with_tier_budget(&green_signals(), tier_budget.clone());

        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Critical);
        assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));
        assert!(actions.contains(&FleetMemoryAction::PauseIdlePanes));
        assert_eq!(
            ctrl.snapshot()
                .tier_budget
                .as_ref()
                .map(|snapshot| snapshot.totals),
            Some(tier_budget.totals)
        );
        assert_eq!(
            ctrl.audit_trail()[0]
                .tier_budget
                .as_ref()
                .map(FleetMemoryTierBudgetSnapshot::pressure_tier),
            Some(FleetPressureTier::Critical)
        );
    }

    #[test]
    fn elevated_tier_budget_warm_overage_requests_eviction_for_small_fleet_ft_08wlf() {
        let tier_budget =
            FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
                FleetMemoryTier::WarmCompressed,
                1_000,
                1_040,
            )
            .with_reclaimable_bytes(40)]);
        assert_eq!(tier_budget.pressure_tier(), FleetPressureTier::Elevated);

        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });
        let actions = ctrl.evaluate_with_tier_budget(
            &PressureSignals {
                pane_count: 4,
                ..green_signals()
            },
            tier_budget,
        );

        assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);
        assert!(actions.contains(&FleetMemoryAction::ThrottlePolling));
        assert!(
            actions.contains(&FleetMemoryAction::EvictWarmScrollback),
            "warm-tier budget overage must produce a reclaiming action even below the pane-count heuristic"
        );
    }

    #[test]
    fn tier_budget_stress_bounds_resident_bytes_and_keeps_cold_history_queryable_ft_08wlf() {
        const PANE_COUNT: u64 = 64;
        const LINES_PER_PANE: usize = 384;
        const PAGE_SIZE: usize = 8;

        let config = ScrollbackConfig {
            hot_lines: 24,
            page_size: PAGE_SIZE,
            warm_max_bytes: usize::MAX,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };
        let mut panes: Vec<(u64, TieredScrollback)> = (0..PANE_COUNT)
            .map(|pane_id| {
                let mut scrollback = TieredScrollback::new(config.clone());
                scrollback.push_lines(
                    (0..LINES_PER_PANE).map(|line_no| ft_08wlf_stress_line(pane_id, line_no)),
                );
                (pane_id, scrollback)
            })
            .collect();
        let pane_infos: Vec<PaneScrollbackInfo> = panes
            .iter()
            .map(|(pane_id, scrollback)| ft_08wlf_pane_info(*pane_id, scrollback))
            .collect();

        let before_resident_bytes: usize = pane_infos
            .iter()
            .map(|pane| pane.estimated_memory_bytes)
            .sum();
        let before_warm_bytes: usize = pane_infos.iter().map(|pane| pane.warm_bytes).sum();
        let before_hot_bytes = before_resident_bytes.saturating_sub(before_warm_bytes);
        assert!(
            before_warm_bytes > 0,
            "stress fixture must create warm pages"
        );

        let resident_budget_bytes =
            before_hot_bytes.saturating_add(before_warm_bytes.saturating_div(4));
        let before_tier_budget =
            FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
                FleetMemoryTier::WarmCompressed,
                u64::try_from(resident_budget_bytes).unwrap(),
                u64::try_from(before_resident_bytes).unwrap(),
            )
            .with_reclaimable_bytes(u64::try_from(before_warm_bytes).unwrap())]);
        assert_ne!(
            before_tier_budget.pressure_tier(),
            FleetPressureTier::Normal,
            "fixture should put the fleet over its resident budget"
        );

        let mut controller = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });
        let actions = controller.evaluate_with_tier_budget(
            &PressureSignals {
                pane_count: usize::try_from(PANE_COUNT).unwrap(),
                ..green_signals()
            },
            before_tier_budget,
        );
        assert!(
            actions.contains(&FleetMemoryAction::EvictWarmScrollback)
                || actions.contains(&FleetMemoryAction::EmergencyCleanup),
            "over-budget resident memory should request a reclaiming action"
        );

        let mut orchestrator = FleetScrollbackOrchestrator::new();
        let plan = orchestrator
            .plan_eviction(controller.compound_tier(), &pane_infos)
            .expect("over-budget warm resident data should produce an eviction plan");
        assert!(plan.fleet_warm_bytes_target < plan.fleet_warm_bytes_before);

        let first_target_pane = plan
            .targets
            .first()
            .map(|target| target.pane_id)
            .expect("eviction plan should target at least one pane");
        for target in &plan.targets {
            let scrollback = panes
                .iter_mut()
                .find(|(pane_id, _)| *pane_id == target.pane_id)
                .map(|(_, scrollback)| scrollback)
                .expect("eviction target should refer to an existing pane");
            assert_eq!(
                scrollback.evict_warm_pages(target.pages_to_evict),
                target.pages_to_evict
            );
        }

        let after_resident_bytes: usize = panes
            .iter()
            .map(|(_, scrollback)| scrollback.estimated_memory_bytes())
            .sum();
        let after_warm_bytes: usize = panes
            .iter()
            .map(|(_, scrollback)| scrollback.warm_total_bytes())
            .sum();
        assert!(
            after_warm_bytes <= plan.fleet_warm_bytes_target,
            "warm bytes should honor the fleet eviction target: after={after_warm_bytes} target={}",
            plan.fleet_warm_bytes_target
        );
        assert!(
            after_resident_bytes < before_resident_bytes,
            "eviction should reduce resident memory: before={before_resident_bytes} after={after_resident_bytes}"
        );
        assert!(
            after_resident_bytes <= before_hot_bytes + plan.fleet_warm_bytes_target,
            "resident memory should be bounded by hot bytes plus the planned warm target"
        );

        let (_, target_scrollback) = panes
            .iter()
            .find(|(pane_id, _)| *pane_id == first_target_pane)
            .expect("first target pane should still exist");
        assert!(
            target_scrollback.snapshot().cold_lines > 0,
            "planned eviction should move reachable history into the cold tier"
        );
        let oldest_offset =
            usize::try_from(target_scrollback.total_line_count().saturating_sub(1)).unwrap();
        let cold_hint = target_scrollback
            .locate_offset(oldest_offset)
            .expect("oldest retained line should have a location hint");
        assert!(matches!(cold_hint, ScrollbackLocationHint::Cold { .. }));
        let retriever = GeneratedColdHistory {
            pane_id: first_target_pane,
            page_size: PAGE_SIZE,
            lines_per_pane: LINES_PER_PANE,
        };
        assert_eq!(
            target_scrollback
                .cold_line(&cold_hint, &retriever)
                .expect("cold-tier line should be queryable"),
            ft_08wlf_stress_line(first_target_pane, 0)
        );
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

    #[test]
    fn fleet_memory_controller_counters_saturate_at_u64_max() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });

        ctrl.total_evaluations = u64::MAX;
        ctrl.consecutive_at_tier = u64::MAX;

        ctrl.evaluate(&green_signals());
        let snapshot = ctrl.snapshot();
        assert_eq!(snapshot.total_evaluations, u64::MAX);
        assert_eq!(snapshot.consecutive_at_tier, u64::MAX);
        assert_eq!(ctrl.audit_trail().back().unwrap().sequence, u64::MAX);

        ctrl.total_transitions = u64::MAX;
        ctrl.consecutive_at_tier = u64::MAX;

        ctrl.evaluate(&yellow_signals());
        let snapshot = ctrl.snapshot();
        assert_eq!(snapshot.compound_tier, FleetPressureTier::Elevated);
        assert_eq!(snapshot.total_evaluations, u64::MAX);
        assert_eq!(snapshot.total_transitions, u64::MAX);
        assert_eq!(snapshot.consecutive_at_tier, 1);
        assert_eq!(ctrl.audit_trail().back().unwrap().sequence, u64::MAX);
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

    #[test]
    fn audit_trail_clamps_pathological_config_cap_ft_v3gd9() {
        let mut ctrl = FleetMemoryController::new(FleetMemoryConfig {
            max_audit_trail: usize::MAX,
            ..FleetMemoryConfig::default()
        });

        assert_eq!(
            ctrl.config().max_audit_trail,
            MAX_FLEET_MEMORY_AUDIT_RECORDS
        );

        for _ in 0..MAX_FLEET_MEMORY_AUDIT_RECORDS + 3 {
            ctrl.evaluate(&green_signals());
        }

        assert_eq!(ctrl.audit_trail().len(), MAX_FLEET_MEMORY_AUDIT_RECORDS);
        assert_eq!(ctrl.audit_trail()[0].sequence, 4);
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
            estimated_memory_bytes: warm_bytes.saturating_add(10_000),
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
    fn orchestrator_totals_and_warm_sum_saturate() {
        let mut orch = FleetScrollbackOrchestrator::new();
        orch.total_plans = u64::MAX;
        orch.total_targets = u64::MAX - 1;
        let panes = vec![
            make_pane_info(1, 100, usize::MAX, 1),
            make_pane_info(2, 200, usize::MAX, 1),
        ];

        let plan = orch.plan_eviction(FleetPressureTier::Critical, &panes);

        assert!(plan.is_some());
        let plan = plan.unwrap();
        assert_eq!(plan.fleet_warm_bytes_before, usize::MAX);
        assert_eq!(orch.total_plans(), u64::MAX);
        assert_eq!(orch.total_targets(), u64::MAX);
    }

    #[test]
    fn orchestrator_large_fleet_targets_use_exact_integer_fractions() {
        let elevated_expected =
            (usize::MAX / 4).saturating_mul(3) + (usize::MAX % 4).saturating_mul(3) / 4;
        let critical_expected = usize::MAX / 4;

        let mut elevated_orch = FleetScrollbackOrchestrator::new();
        let elevated = elevated_orch
            .plan_eviction(
                FleetPressureTier::Elevated,
                &[make_pane_info(1, 100, usize::MAX, 4)],
            )
            .expect("large warm total should produce elevated eviction plan");
        assert_eq!(elevated.fleet_warm_bytes_before, usize::MAX);
        assert_eq!(elevated.fleet_warm_bytes_target, elevated_expected);

        let mut critical_orch = FleetScrollbackOrchestrator::new();
        let critical = critical_orch
            .plan_eviction(
                FleetPressureTier::Critical,
                &[make_pane_info(1, 100, usize::MAX, 4)],
            )
            .expect("large warm total should produce critical eviction plan");
        assert_eq!(critical.fleet_warm_bytes_before, usize::MAX);
        assert_eq!(critical.fleet_warm_bytes_target, critical_expected);
    }

    #[test]
    fn orchestrator_large_page_counts_use_exact_integer_eviction_math() {
        let large_pages = if usize::BITS >= 64 {
            (1usize << 54) + 3
        } else {
            (1usize << 20) + 3
        };
        let warm_bytes = large_pages
            .checked_mul(4)
            .expect("test page count should leave room for byte scaling");

        let mut orch = FleetScrollbackOrchestrator::new();
        let plan = orch
            .plan_eviction(
                FleetPressureTier::Elevated,
                &[make_pane_info(1, 100, warm_bytes, large_pages)],
            )
            .expect("large page count should produce an eviction plan");

        let expected_pages = large_pages.div_ceil(4);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].pages_to_evict, expected_pages);
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

    // ── br-ft-diccw: FleetMemoryConfig hysteresis-threshold validation ──

    #[test]
    fn fleet_memory_config_default_validates_ft_diccw() {
        // Sanity: documented defaults must not trip the validator.
        FleetMemoryConfig::default()
            .validate()
            .expect("default config must validate");
    }

    #[test]
    fn fleet_memory_config_zero_escalation_rejects_ft_diccw() {
        // br-ft-diccw: with escalation_threshold=0 the per-cycle
        // guard at evaluate path leaves sustained_high stuck at
        // Normal, silently disabling all tier escalations.
        let cfg = FleetMemoryConfig {
            escalation_threshold: 0,
            ..FleetMemoryConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("zero escalation_threshold must reject");
        assert_eq!(err, FleetMemoryConfigError::ZeroEscalationThreshold);
        assert!(err.to_string().contains("br-ft-diccw"));
    }

    #[test]
    fn fleet_memory_config_zero_deescalation_rejects_ft_diccw() {
        let cfg = FleetMemoryConfig {
            deescalation_threshold: 0,
            ..FleetMemoryConfig::default()
        };
        assert_eq!(
            cfg.validate(),
            Err(FleetMemoryConfigError::ZeroDeescalationThreshold)
        );
    }

    #[test]
    fn fleet_memory_config_zero_max_audit_trail_rejects_ft_diccw() {
        let cfg = FleetMemoryConfig {
            max_audit_trail: 0,
            ..FleetMemoryConfig::default()
        };
        assert_eq!(
            cfg.validate(),
            Err(FleetMemoryConfigError::ZeroMaxAuditTrail)
        );
    }

    #[test]
    fn fleet_memory_config_finite_thresholds_pass_ft_diccw() {
        let cfg = FleetMemoryConfig {
            max_audit_trail: 50,
            escalation_threshold: 1,
            deescalation_threshold: 1,
        };
        cfg.validate().expect("min-finite thresholds must validate");
    }

    #[test]
    fn fleet_memory_config_normalizes_max_audit_trail_ft_v3gd9() {
        assert_eq!(
            FleetMemoryConfig::default().normalized_max_audit_trail(),
            FleetMemoryConfig::default().max_audit_trail
        );

        assert_eq!(
            FleetMemoryConfig {
                max_audit_trail: 0,
                ..FleetMemoryConfig::default()
            }
            .normalized_max_audit_trail(),
            1
        );

        assert_eq!(
            FleetMemoryConfig {
                max_audit_trail: usize::MAX,
                ..FleetMemoryConfig::default()
            }
            .normalized_max_audit_trail(),
            MAX_FLEET_MEMORY_AUDIT_RECORDS
        );
    }

    #[test]
    fn fleet_memory_evaluate_with_zero_escalation_silently_holds_normal_ft_diccw() {
        // Pre-fix demonstration: a zero-threshold config installs
        // without complaint, then the controller silently refuses
        // to escalate under sustained pressure. validate() now
        // catches this before construction; this test pins the
        // pre-fix behavior so a future refactor that bypasses
        // validation still surfaces the "stuck at Normal" symptom
        // through the evaluate path.
        let cfg = FleetMemoryConfig {
            max_audit_trail: 100,
            escalation_threshold: 0,
            deescalation_threshold: 5,
        };
        // validate() rejects, but the constructor still accepts
        // (callers haven't migrated yet); pin both behaviors.
        assert!(cfg.validate().is_err());
        let mut ctrl = FleetMemoryController::new(cfg);

        // Drive 10 evaluations of Black-tier pressure.
        for _ in 0..10 {
            let _actions = ctrl.evaluate(&PressureSignals {
                backpressure: BackpressureTier::Black,
                memory_pressure: MemoryPressureTier::Red,
                worst_budget: BudgetLevel::OverBudget,
                pane_count: 50,
                paused_pane_count: 5,
            });
        }
        assert_eq!(
            ctrl.compound_tier(),
            FleetPressureTier::Normal,
            "pre-fix demonstration: zero-threshold config never escalates even under sustained Black pressure"
        );
    }
}
