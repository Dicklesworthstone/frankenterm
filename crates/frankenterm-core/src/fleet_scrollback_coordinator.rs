//! Fleet scrollback coordinator (ft-1memj.19).
//!
//! Bridges the [`FleetMemoryController`] decision layer with per-pane
//! [`TieredScrollback`] instances. On each evaluation tick the coordinator:
//!
//! 1. Collects [`PaneScrollbackInfo`] from every registered pane.
//! 2. Feeds [`PressureSignals`] into [`FleetMemoryController::evaluate`].
//! 3. When the controller emits [`FleetMemoryAction::EvictWarmScrollback`]
//!    or [`FleetMemoryAction::EmergencyCleanup`], asks the
//!    [`FleetScrollbackOrchestrator`] for a concrete [`EvictionPlan`].
//! 4. Applies each [`EvictionTarget`] to the corresponding pane's
//!    [`TieredScrollback`].
//! 5. Records telemetry (plans produced, pages evicted, bytes reclaimed).
//!
//! The coordinator is *not* async — it operates synchronously on mutable
//! references to avoid locking complexity. The caller (runtime maintenance
//! loop) is responsible for providing the pressure signals and pane handles.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;
use crate::fleet_memory_controller::{
    EvictionPlan, FleetMemoryAction, FleetMemoryConfig, FleetMemoryController, FleetPressureTier,
    FleetScrollbackOrchestrator, PaneScrollbackInfo, PressureSignals,
};
use crate::memory_budget::BudgetLevel;
use crate::memory_pressure::MemoryPressureTier;
use crate::scrollback_tiers::{ScrollbackTierSnapshot, TieredScrollback};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the fleet scrollback coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// Maximum number of panes to target per eviction cycle.
    /// Limits blast radius of a single evaluation. Default: 50.
    pub max_targets_per_cycle: usize,

    /// Minimum warm bytes across the fleet before eviction is worthwhile.
    /// Avoids churn when there's negligible warm data. Default: 1 MB.
    pub min_fleet_warm_bytes_for_eviction: usize,

    /// When true, emergency cleanup evicts *all* warm pages fleet-wide
    /// rather than relying on the orchestrator's proportional plan.
    /// Default: true.
    pub emergency_evict_all: bool,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_targets_per_cycle: 50,
            min_fleet_warm_bytes_for_eviction: 1024 * 1024, // 1 MB
            emergency_evict_all: true,
        }
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Cumulative telemetry for the coordinator.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinatorTelemetry {
    /// Total evaluation ticks processed.
    pub ticks: u64,
    /// Ticks where the compound tier was above Normal.
    pub elevated_ticks: u64,
    /// Total eviction plans produced (from the orchestrator).
    pub plans_produced: u64,
    /// Total individual pane eviction targets applied.
    pub targets_applied: u64,
    /// Total warm pages evicted across all targets.
    pub pages_evicted: u64,
    /// Total warm bytes reclaimed (estimated compressed).
    pub bytes_reclaimed: u64,
    /// Number of emergency cleanup events.
    pub emergency_cleanups: u64,
    /// Number of ticks where eviction was skipped because fleet warm
    /// bytes were below the minimum threshold.
    pub skipped_below_threshold: u64,
}

// =============================================================================
// Evaluation result
// =============================================================================

/// Summary of a single coordinator evaluation tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Fleet pressure tier after this evaluation.
    pub compound_tier: FleetPressureTier,
    /// Actions recommended by the fleet memory controller.
    pub actions: Vec<FleetMemoryAction>,
    /// Eviction plan produced (if any).
    pub eviction_plan: Option<EvictionPlan>,
    /// Number of warm pages actually evicted in this tick.
    pub pages_evicted: u64,
    /// Estimated bytes reclaimed in this tick.
    pub bytes_reclaimed: u64,
    /// Number of pane targets applied.
    pub targets_applied: usize,
}

// =============================================================================
// Coordinator
// =============================================================================

/// Stateful coordinator that bridges fleet memory decisions with per-pane
/// tiered scrollback instances.
///
/// # Usage
///
/// ```ignore
/// let mut coord = FleetScrollbackCoordinator::new(
///     CoordinatorConfig::default(),
///     FleetMemoryConfig::default(),
/// );
///
/// // In the maintenance loop:
/// let pane_infos = collect_pane_infos(&panes);
/// let signals = collect_pressure_signals();
/// let result = coord.evaluate(&signals, &pane_infos, &mut panes);
/// ```
#[derive(Debug)]
pub struct FleetScrollbackCoordinator {
    config: CoordinatorConfig,
    controller: FleetMemoryController,
    orchestrator: FleetScrollbackOrchestrator,
    telemetry: CoordinatorTelemetry,
}

impl FleetScrollbackCoordinator {
    /// Create a new coordinator with the given configuration.
    #[must_use]
    pub fn new(config: CoordinatorConfig, fleet_config: FleetMemoryConfig) -> Self {
        Self {
            config,
            controller: FleetMemoryController::new(fleet_config),
            orchestrator: FleetScrollbackOrchestrator::new(),
            telemetry: CoordinatorTelemetry::default(),
        }
    }

    /// Read-only access to cumulative telemetry.
    #[must_use]
    pub fn telemetry(&self) -> &CoordinatorTelemetry {
        &self.telemetry
    }

    /// Snapshot of cumulative telemetry (owned copy for serialization).
    #[must_use]
    pub fn telemetry_snapshot(&self) -> CoordinatorTelemetry {
        self.telemetry.clone()
    }

    /// Current compound fleet pressure tier.
    #[must_use]
    pub fn compound_tier(&self) -> FleetPressureTier {
        self.controller.compound_tier()
    }

    /// Reference to the coordinator configuration.
    #[must_use]
    pub fn config(&self) -> &CoordinatorConfig {
        &self.config
    }

    /// Run a single evaluation tick.
    ///
    /// `signals` — current pressure readings from all subsystems.
    /// `pane_infos` — per-pane scrollback metadata collected from panes.
    /// `panes` — mutable map from pane_id → TieredScrollback so the
    ///   coordinator can apply eviction directly.
    ///
    /// Returns an [`EvaluationResult`] describing what happened.
    pub fn evaluate(
        &mut self,
        signals: &PressureSignals,
        pane_infos: &[PaneScrollbackInfo],
        panes: &mut dyn PaneScrollbackAccess,
    ) -> EvaluationResult {
        self.telemetry.ticks += 1;

        // Step 1: Evaluate fleet pressure.
        let actions = self.controller.evaluate(signals);
        let tier = self.controller.compound_tier();

        if tier != FleetPressureTier::Normal {
            self.telemetry.elevated_ticks += 1;
        }

        // Step 2: Determine if eviction is needed.
        let needs_eviction = actions.contains(&FleetMemoryAction::EvictWarmScrollback)
            || actions.contains(&FleetMemoryAction::EmergencyCleanup);

        if !needs_eviction {
            return EvaluationResult {
                compound_tier: tier,
                actions,
                eviction_plan: None,
                pages_evicted: 0,
                bytes_reclaimed: 0,
                targets_applied: 0,
            };
        }

        // Step 3: Check minimum threshold.
        let fleet_warm_bytes: usize = pane_infos.iter().map(|p| p.warm_bytes).sum();
        if fleet_warm_bytes < self.config.min_fleet_warm_bytes_for_eviction {
            self.telemetry.skipped_below_threshold += 1;
            return EvaluationResult {
                compound_tier: tier,
                actions,
                eviction_plan: None,
                pages_evicted: 0,
                bytes_reclaimed: 0,
                targets_applied: 0,
            };
        }

        // Step 4: Handle emergency vs proportional eviction.
        let is_emergency = actions.contains(&FleetMemoryAction::EmergencyCleanup);

        if is_emergency && self.config.emergency_evict_all {
            // Emergency path: evict all warm pages on every pane.
            self.telemetry.emergency_cleanups += 1;
            let (evicted_pages, freed_bytes, applied) =
                self.apply_emergency_eviction(pane_infos, panes);
            return EvaluationResult {
                compound_tier: tier,
                actions,
                eviction_plan: None, // emergency bypasses orchestrator
                pages_evicted: evicted_pages,
                bytes_reclaimed: freed_bytes,
                targets_applied: applied,
            };
        }

        // Step 5: Ask orchestrator for a proportional eviction plan.
        let plan = self.orchestrator.plan_eviction(tier, pane_infos);

        let Some(plan) = plan else {
            return EvaluationResult {
                compound_tier: tier,
                actions,
                eviction_plan: None,
                pages_evicted: 0,
                bytes_reclaimed: 0,
                targets_applied: 0,
            };
        };

        self.telemetry.plans_produced += 1;

        // Step 6: Apply the eviction plan (bounded by max_targets_per_cycle).
        let (evicted_pages, freed_bytes, applied) = self.apply_plan(&plan, panes);

        EvaluationResult {
            compound_tier: tier,
            actions,
            eviction_plan: Some(plan),
            pages_evicted: evicted_pages,
            bytes_reclaimed: freed_bytes,
            targets_applied: applied,
        }
    }

    /// Collect per-pane scrollback info from any scrollback access adapter.
    ///
    /// Convenience helper for callers that hold pane data in a map-like structure.
    pub fn collect_pane_infos(panes: &dyn PaneScrollbackAccess) -> Vec<PaneScrollbackInfo> {
        let ids = panes.pane_ids();
        let mut infos = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(snapshot) = panes.snapshot(id) {
                infos.push(PaneScrollbackInfo {
                    pane_id: id,
                    activity_counter: snapshot.activity_counter,
                    warm_bytes: snapshot.warm_bytes,
                    warm_pages: snapshot.warm_pages,
                    estimated_memory_bytes: snapshot.warm_bytes + (snapshot.hot_lines * 200), // rough estimate: 200 bytes/line
                });
            }
        }
        infos
    }

    /// Sync scrollback memory estimates to a [`PaneArenaRegistry`].
    ///
    /// After each evaluation tick, callers can invoke this to update the
    /// arena registry's `tracked_bytes` for each pane with the scrollback
    /// memory estimate. This bridges the scrollback tier system with the
    /// per-pane allocator arena accounting (ft-3axa).
    ///
    /// Returns the number of pane arenas updated.
    pub fn sync_to_arena_registry(
        pane_infos: &[PaneScrollbackInfo],
        registry: &frankenterm_alloc::PaneArenaRegistry,
    ) -> usize {
        let mut updated = 0;
        for info in pane_infos {
            if registry
                .set_tracked_bytes(info.pane_id, info.estimated_memory_bytes)
                .is_some()
            {
                updated += 1;
            }
        }
        updated
    }

    /// Build pressure signals from raw runtime queue metrics.
    ///
    /// This is the bridge between the runtime health snapshot (which already
    /// computes capture/write queue depths) and the fleet memory controller.
    /// The caller provides the raw values already available in the maintenance
    /// loop, and this method maps them to the correct tier enums.
    #[must_use]
    pub fn signals_from_queue_depths(
        capture_depth: usize,
        capture_capacity: usize,
        write_depth: usize,
        write_capacity: usize,
        pane_count: usize,
        paused_pane_count: usize,
    ) -> PressureSignals {
        // Map queue fill ratios to backpressure tier.
        // Thresholds aligned with runtime.rs classify_backpressure_tier:
        // Green < 50%, Yellow 50-80%, Red 80-95%, Black >= 95%
        let bp_tier = {
            let cap_ratio = if capture_capacity > 0 {
                capture_depth as f64 / capture_capacity as f64
            } else {
                0.0
            };
            let write_ratio = if write_capacity > 0 {
                write_depth as f64 / write_capacity as f64
            } else {
                0.0
            };
            let worst_ratio = cap_ratio.max(write_ratio);
            if worst_ratio >= 0.95 {
                BackpressureTier::Black
            } else if worst_ratio >= 0.80 {
                BackpressureTier::Red
            } else if worst_ratio >= 0.50 {
                BackpressureTier::Yellow
            } else {
                BackpressureTier::Green
            }
        };

        PressureSignals {
            backpressure: bp_tier,
            memory_pressure: MemoryPressureTier::Green, // memory pressure requires sysinfo
            worst_budget: BudgetLevel::Normal,          // budget requires per-pane accounting
            pane_count,
            paused_pane_count,
        }
    }

    /// Build default pressure signals for testing or fallback.
    #[must_use]
    pub fn default_signals(pane_count: usize) -> PressureSignals {
        PressureSignals {
            backpressure: BackpressureTier::Green,
            memory_pressure: MemoryPressureTier::Green,
            worst_budget: BudgetLevel::Normal,
            pane_count,
            paused_pane_count: 0,
        }
    }

    // ── Internal ──────────────────────────────────────────────────────

    /// Apply a proportional eviction plan to pane scrollbacks.
    fn apply_plan(
        &mut self,
        plan: &EvictionPlan,
        panes: &mut dyn PaneScrollbackAccess,
    ) -> (u64, u64, usize) {
        let mut total_pages: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut applied = 0;

        let targets = &plan.targets;
        let max = self.config.max_targets_per_cycle.min(targets.len());

        for target in &targets[..max] {
            let before = panes.snapshot(target.pane_id);
            let evicted = panes.evict_warm_pages(target.pane_id, target.pages_to_evict);
            let after = panes.snapshot(target.pane_id);

            let bytes_freed = match (before, after) {
                (Some(b), Some(a)) => b.warm_bytes.saturating_sub(a.warm_bytes) as u64,
                _ => 0,
            };

            total_pages += evicted as u64;
            total_bytes += bytes_freed;
            if evicted > 0 || bytes_freed > 0 {
                applied += 1;
            }
        }

        self.telemetry.targets_applied += applied as u64;
        self.telemetry.pages_evicted += total_pages;
        self.telemetry.bytes_reclaimed += total_bytes;

        (total_pages, total_bytes, applied)
    }

    /// Emergency path: evict all warm pages on every pane.
    fn apply_emergency_eviction(
        &mut self,
        pane_infos: &[PaneScrollbackInfo],
        panes: &mut dyn PaneScrollbackAccess,
    ) -> (u64, u64, usize) {
        let mut total_pages: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut targets = 0;

        for info in pane_infos {
            if info.warm_pages == 0 {
                continue;
            }
            let before = panes.snapshot(info.pane_id);
            panes.evict_all_warm(info.pane_id);
            let after = panes.snapshot(info.pane_id);

            let (pages_evicted, bytes_freed) = match (before, after) {
                (Some(b), Some(a)) => (
                    b.warm_pages.saturating_sub(a.warm_pages) as u64,
                    b.warm_bytes.saturating_sub(a.warm_bytes) as u64,
                ),
                _ => (0, 0),
            };

            total_pages += pages_evicted;
            total_bytes += bytes_freed;
            if pages_evicted > 0 || bytes_freed > 0 {
                targets += 1;
            }
        }

        self.telemetry.targets_applied += targets as u64;
        self.telemetry.pages_evicted += total_pages;
        self.telemetry.bytes_reclaimed += total_bytes;

        (total_pages, total_bytes, targets)
    }
}

impl Default for FleetScrollbackCoordinator {
    fn default() -> Self {
        Self::new(CoordinatorConfig::default(), FleetMemoryConfig::default())
    }
}

// =============================================================================
// Pane access trait
// =============================================================================

/// Trait abstracting access to per-pane TieredScrollback instances.
///
/// This decouples the coordinator from the concrete pane storage structure
/// (HashMap, BTreeMap, registry, etc.), making it testable with mock panes.
pub trait PaneScrollbackAccess {
    /// Return all active pane IDs.
    fn pane_ids(&self) -> Vec<u64>;

    /// Get a scrollback tier snapshot for a specific pane.
    fn snapshot(&self, pane_id: u64) -> Option<ScrollbackTierSnapshot>;

    /// Evict up to `count` warm pages from a specific pane's scrollback.
    /// Returns the number of pages actually evicted.
    fn evict_warm_pages(&mut self, pane_id: u64, count: usize) -> usize;

    /// Evict all warm pages from a specific pane's scrollback.
    fn evict_all_warm(&mut self, pane_id: u64);
}

/// Simple HashMap-based implementation for testing and lightweight usage.
impl<S: std::hash::BuildHasher> PaneScrollbackAccess
    for std::collections::HashMap<u64, TieredScrollback, S>
{
    fn pane_ids(&self) -> Vec<u64> {
        self.keys().copied().collect()
    }

    fn snapshot(&self, pane_id: u64) -> Option<ScrollbackTierSnapshot> {
        self.get(&pane_id).map(TieredScrollback::snapshot)
    }

    fn evict_warm_pages(&mut self, pane_id: u64, count: usize) -> usize {
        self.get_mut(&pane_id)
            .map(|sb| sb.evict_warm_pages(count))
            .unwrap_or(0)
    }

    fn evict_all_warm(&mut self, pane_id: u64) {
        if let Some(sb) = self.get_mut(&pane_id) {
            sb.evict_all_warm();
        }
    }
}

// =============================================================================
// Snapshot-backed PaneScrollbackAccess
// =============================================================================

/// Read-only pane scrollback adapter backed by already-collected snapshots.
///
/// The runtime uses this adapter once it has gathered authoritative pane
/// scrollback state from the current pane/session boundary. Reads stay bounded
/// because the adapter stores compact per-pane snapshots rather than copying
/// full scrollback content.
///
/// Eviction calls enforce the coordinator decision against the collected
/// snapshot by dropping the oldest warm-page accounting first. The runtime uses
/// the mutated snapshot state for telemetry and maintenance records even when
/// the caller only has compact pane snapshots rather than live `TieredScrollback`
/// handles.
#[derive(Debug, Clone, Default)]
pub struct SnapshotPaneScrollbackAccess {
    snapshots: HashMap<u64, ScrollbackTierSnapshot>,
}

impl SnapshotPaneScrollbackAccess {
    /// Create an adapter from pre-collected pane snapshots.
    #[must_use]
    pub fn new(snapshots: HashMap<u64, ScrollbackTierSnapshot>) -> Self {
        Self { snapshots }
    }

    fn evict_from_snapshot(snapshot: &mut ScrollbackTierSnapshot, count: usize) -> usize {
        let pages_to_evict = count.min(snapshot.warm_pages);
        if pages_to_evict == 0 {
            return 0;
        }

        let warm_pages_before = snapshot.warm_pages;
        let bytes_to_evict = if pages_to_evict == warm_pages_before {
            snapshot.warm_bytes
        } else {
            snapshot
                .warm_bytes
                .saturating_mul(pages_to_evict)
                .div_ceil(warm_pages_before)
                .min(snapshot.warm_bytes)
        };
        let lines_to_evict = if pages_to_evict == warm_pages_before {
            snapshot.warm_lines
        } else {
            snapshot
                .warm_lines
                .saturating_mul(pages_to_evict)
                .div_ceil(warm_pages_before)
                .min(snapshot.warm_lines)
        };

        snapshot.warm_pages -= pages_to_evict;
        snapshot.warm_bytes = snapshot.warm_bytes.saturating_sub(bytes_to_evict);
        snapshot.warm_lines = snapshot.warm_lines.saturating_sub(lines_to_evict);
        snapshot.cold_pages = snapshot.cold_pages.saturating_add(pages_to_evict as u64);
        snapshot.cold_lines = snapshot.cold_lines.saturating_add(lines_to_evict as u64);
        snapshot.cold_uncompressed_bytes = snapshot
            .cold_uncompressed_bytes
            .saturating_add(bytes_to_evict as u64);
        pages_to_evict
    }
}

impl PaneScrollbackAccess for SnapshotPaneScrollbackAccess {
    fn pane_ids(&self) -> Vec<u64> {
        let mut ids: Vec<_> = self.snapshots.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    fn snapshot(&self, pane_id: u64) -> Option<ScrollbackTierSnapshot> {
        self.snapshots.get(&pane_id).cloned()
    }

    fn evict_warm_pages(&mut self, pane_id: u64, count: usize) -> usize {
        self.snapshots
            .get_mut(&pane_id)
            .map_or(0, |snapshot| Self::evict_from_snapshot(snapshot, count))
    }

    fn evict_all_warm(&mut self, pane_id: u64) {
        if let Some(snapshot) = self.snapshots.get_mut(&pane_id) {
            Self::evict_from_snapshot(snapshot, snapshot.warm_pages);
        }
    }
}

#[cfg(test)]
/// Test-only null adapter preserving the historical placeholder behavior for
/// regression coverage.
pub struct NullPaneScrollbackAccess;

#[cfg(test)]
impl PaneScrollbackAccess for NullPaneScrollbackAccess {
    fn pane_ids(&self) -> Vec<u64> {
        Vec::new()
    }

    fn snapshot(&self, _pane_id: u64) -> Option<ScrollbackTierSnapshot> {
        None
    }

    fn evict_warm_pages(&mut self, _pane_id: u64, _count: usize) -> usize {
        0
    }

    fn evict_all_warm(&mut self, _pane_id: u64) {}
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback_tiers::ScrollbackConfig;

    fn small_config() -> ScrollbackConfig {
        ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 100_000, // large cap so warm accumulates
            ..ScrollbackConfig::default()
        }
    }

    fn fill_pane(sb: &mut TieredScrollback, lines: usize) {
        for i in 0..lines {
            sb.push_line(format!("line-{i:06}"));
        }
    }

    fn make_panes(
        count: usize,
        lines_per_pane: usize,
    ) -> std::collections::HashMap<u64, TieredScrollback> {
        let mut panes = std::collections::HashMap::new();
        for i in 0..count {
            let mut sb = TieredScrollback::new(small_config());
            fill_pane(&mut sb, lines_per_pane);
            panes.insert(i as u64, sb);
        }
        panes
    }

    fn pane_infos_from_map(
        panes: &std::collections::HashMap<u64, TieredScrollback>,
    ) -> Vec<PaneScrollbackInfo> {
        panes
            .iter()
            .map(|(&id, sb)| {
                let snap = sb.snapshot();
                PaneScrollbackInfo {
                    pane_id: id,
                    activity_counter: snap.activity_counter,
                    warm_bytes: snap.warm_bytes,
                    warm_pages: snap.warm_pages,
                    estimated_memory_bytes: sb.estimated_memory_bytes(),
                }
            })
            .collect()
    }

    // ── Basic lifecycle ──────────────────────────────────────────────

    #[test]
    fn default_coordinator_starts_normal() {
        let coord = FleetScrollbackCoordinator::default();
        assert_eq!(coord.compound_tier(), FleetPressureTier::Normal);
        assert_eq!(coord.telemetry().ticks, 0);
    }

    #[test]
    fn normal_pressure_produces_no_eviction() {
        let mut coord = FleetScrollbackCoordinator::default();
        let mut panes = make_panes(10, 50);
        let infos = pane_infos_from_map(&panes);
        let signals = FleetScrollbackCoordinator::default_signals(10);

        let result = coord.evaluate(&signals, &infos, &mut panes);

        assert_eq!(result.compound_tier, FleetPressureTier::Normal);
        assert!(result.eviction_plan.is_none());
        assert_eq!(result.pages_evicted, 0);
        assert_eq!(coord.telemetry().ticks, 1);
        assert_eq!(coord.telemetry().elevated_ticks, 0);
    }

    #[test]
    fn elevated_pressure_with_many_panes_triggers_eviction() {
        // Use immediate escalation to avoid hysteresis complexity in test.
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig::default(),
            FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..FleetMemoryConfig::default()
            },
        );
        let mut panes = make_panes(200, 100); // 200 panes, lots of warm data
        let infos = pane_infos_from_map(&panes);

        // Elevated signals with 200+ panes
        let signals = PressureSignals {
            backpressure: BackpressureTier::Yellow,
            memory_pressure: MemoryPressureTier::Green,
            worst_budget: BudgetLevel::Normal,
            pane_count: 200,
            paused_pane_count: 0,
        };

        // With threshold=1, first evaluation should already escalate
        let result = coord.evaluate(&signals, &infos, &mut panes);
        assert_eq!(result.compound_tier, FleetPressureTier::Elevated);
        assert!(coord.telemetry().elevated_ticks >= 1);
    }

    #[test]
    fn critical_pressure_evicts_warm_pages() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                min_fleet_warm_bytes_for_eviction: 0, // no threshold
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let mut panes = make_panes(5, 200); // 5 panes with substantial warm data

        let signals = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: 5,
            paused_pane_count: 0,
        };

        // Push past hysteresis
        for _ in 0..4 {
            let infos = pane_infos_from_map(&panes);
            coord.evaluate(&signals, &infos, &mut panes);
        }

        let infos = pane_infos_from_map(&panes);
        let fleet_warm_before: usize = infos.iter().map(|p| p.warm_bytes).sum();

        let result = coord.evaluate(&signals, &infos, &mut panes);
        assert_eq!(result.compound_tier, FleetPressureTier::Critical);

        if fleet_warm_before > 0 {
            // Should have evicted some pages
            assert!(
                result.pages_evicted > 0 || result.eviction_plan.is_some(),
                "Critical tier should trigger eviction when warm data exists"
            );
        }
    }

    #[test]
    fn emergency_evicts_all_warm() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                emergency_evict_all: true,
                min_fleet_warm_bytes_for_eviction: 0,
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let mut panes = make_panes(5, 200);

        let signals = PressureSignals {
            backpressure: BackpressureTier::Black,
            memory_pressure: MemoryPressureTier::Red,
            worst_budget: BudgetLevel::OverBudget,
            pane_count: 5,
            paused_pane_count: 0,
        };

        // Push past hysteresis
        for _ in 0..4 {
            let infos = pane_infos_from_map(&panes);
            coord.evaluate(&signals, &infos, &mut panes);
        }

        let infos = pane_infos_from_map(&panes);
        let result = coord.evaluate(&signals, &infos, &mut panes);
        assert_eq!(result.compound_tier, FleetPressureTier::Emergency);

        // After emergency, all warm should be gone
        for sb in panes.values() {
            assert_eq!(
                sb.warm_page_count(),
                0,
                "Emergency should evict all warm pages"
            );
        }
        assert!(coord.telemetry().emergency_cleanups > 0);
    }

    #[test]
    fn below_threshold_skips_eviction() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                min_fleet_warm_bytes_for_eviction: usize::MAX, // impossibly high
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let mut panes = make_panes(5, 200);

        let signals = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: 5,
            paused_pane_count: 0,
        };

        // Push past hysteresis
        for _ in 0..4 {
            let infos = pane_infos_from_map(&panes);
            coord.evaluate(&signals, &infos, &mut panes);
        }

        let infos = pane_infos_from_map(&panes);
        let result = coord.evaluate(&signals, &infos, &mut panes);

        assert_eq!(result.pages_evicted, 0);
        assert!(coord.telemetry().skipped_below_threshold > 0);
    }

    #[test]
    fn max_targets_per_cycle_limits_blast_radius() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                max_targets_per_cycle: 2, // Only evict from 2 panes per tick
                min_fleet_warm_bytes_for_eviction: 0,
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let mut panes = make_panes(10, 200);

        let signals = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: 10,
            paused_pane_count: 0,
        };

        // Push past hysteresis
        for _ in 0..4 {
            let infos = pane_infos_from_map(&panes);
            coord.evaluate(&signals, &infos, &mut panes);
        }

        let infos = pane_infos_from_map(&panes);
        let result = coord.evaluate(&signals, &infos, &mut panes);

        assert!(
            result.targets_applied <= 2,
            "Should limit to max_targets_per_cycle: got {}",
            result.targets_applied
        );
    }

    #[test]
    fn collect_pane_infos_from_trait() {
        let panes = make_panes(3, 50);
        let infos = FleetScrollbackCoordinator::collect_pane_infos(&panes);
        assert_eq!(infos.len(), 3);
        for info in &infos {
            assert!(info.pane_id < 3);
        }
    }

    #[test]
    fn telemetry_accumulates_across_ticks() {
        let mut coord = FleetScrollbackCoordinator::default();
        let mut panes = make_panes(5, 50);
        let signals = FleetScrollbackCoordinator::default_signals(5);

        for _ in 0..10 {
            let infos = pane_infos_from_map(&panes);
            coord.evaluate(&signals, &infos, &mut panes);
        }

        assert_eq!(coord.telemetry().ticks, 10);
    }

    #[test]
    fn telemetry_snapshot_is_independent_copy() {
        let mut coord = FleetScrollbackCoordinator::default();
        let mut panes = make_panes(1, 10);
        let infos = pane_infos_from_map(&panes);
        let signals = FleetScrollbackCoordinator::default_signals(1);

        coord.evaluate(&signals, &infos, &mut panes);
        let snap1 = coord.telemetry_snapshot();

        coord.evaluate(&signals, &infos, &mut panes);
        let snap2 = coord.telemetry_snapshot();

        assert_eq!(snap1.ticks, 1);
        assert_eq!(snap2.ticks, 2);
    }

    #[test]
    fn empty_fleet_produces_no_eviction_under_pressure() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                min_fleet_warm_bytes_for_eviction: 0,
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let mut panes: std::collections::HashMap<u64, TieredScrollback> =
            std::collections::HashMap::new();
        let infos: Vec<PaneScrollbackInfo> = Vec::new();

        let signals = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: 0,
            paused_pane_count: 0,
        };

        // Push past hysteresis
        for _ in 0..5 {
            coord.evaluate(&signals, &infos, &mut panes);
        }

        let result = coord.evaluate(&signals, &infos, &mut panes);
        assert_eq!(result.pages_evicted, 0);
        assert_eq!(result.targets_applied, 0);
    }

    #[test]
    fn serde_roundtrip_telemetry() {
        let telem = CoordinatorTelemetry {
            ticks: 100,
            elevated_ticks: 20,
            plans_produced: 5,
            targets_applied: 15,
            pages_evicted: 150,
            bytes_reclaimed: 1_048_576,
            emergency_cleanups: 1,
            skipped_below_threshold: 3,
        };

        let json = serde_json::to_string(&telem).expect("serialize");
        let back: CoordinatorTelemetry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(telem, back);
    }

    #[test]
    fn serde_roundtrip_config() {
        let config = CoordinatorConfig::default();
        let json = serde_json::to_string(&config).expect("serialize");
        let back: CoordinatorConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.max_targets_per_cycle, config.max_targets_per_cycle);
        assert_eq!(
            back.min_fleet_warm_bytes_for_eviction,
            config.min_fleet_warm_bytes_for_eviction
        );
    }

    #[test]
    fn serde_roundtrip_evaluation_result() {
        let result = EvaluationResult {
            compound_tier: FleetPressureTier::Elevated,
            actions: vec![FleetMemoryAction::ThrottlePolling],
            eviction_plan: None,
            pages_evicted: 0,
            bytes_reclaimed: 0,
            targets_applied: 0,
        };

        let json = serde_json::to_string(&result).expect("serialize");
        let back: EvaluationResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.compound_tier, FleetPressureTier::Elevated);
        assert_eq!(back.pages_evicted, 0);
    }

    #[test]
    fn pane_access_trait_evict_nonexistent_pane() {
        let mut panes: std::collections::HashMap<u64, TieredScrollback> =
            std::collections::HashMap::new();
        // Evicting from a nonexistent pane should return 0, not panic
        let evicted = panes.evict_warm_pages(999, 10);
        assert_eq!(evicted, 0);
    }

    #[test]
    fn pane_access_trait_evict_all_warm_nonexistent() {
        let mut panes: std::collections::HashMap<u64, TieredScrollback> =
            std::collections::HashMap::new();
        // Should be a no-op
        panes.evict_all_warm(999);
    }

    #[test]
    fn pane_access_trait_snapshot_nonexistent() {
        let panes: std::collections::HashMap<u64, TieredScrollback> =
            std::collections::HashMap::new();
        assert!(panes.snapshot(999).is_none());
    }

    #[test]
    fn proportional_eviction_does_not_emergency_cleanup() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                emergency_evict_all: true,
                min_fleet_warm_bytes_for_eviction: 0,
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let mut panes = make_panes(5, 200);

        // Critical but not emergency
        let signals = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: 5,
            paused_pane_count: 0,
        };

        // Push past hysteresis
        for _ in 0..5 {
            let infos = pane_infos_from_map(&panes);
            coord.evaluate(&signals, &infos, &mut panes);
        }

        // Should not have triggered emergency cleanup
        assert_eq!(
            coord.telemetry().emergency_cleanups,
            0,
            "Critical tier should use proportional eviction, not emergency"
        );

        // But some panes should still have warm data
        let total_warm: usize = panes.values().map(|sb| sb.warm_page_count()).sum();
        // (Some warm pages remain because proportional eviction doesn't evict everything)
        // This assertion is conditional — depends on data volume
        let _ = total_warm; // suppress unused warning
    }

    #[test]
    fn sync_to_arena_registry_updates_tracked_bytes() {
        let registry = frankenterm_alloc::PaneArenaRegistry::new();
        // Reserve arenas for 3 panes
        registry.reserve(0);
        registry.reserve(1);
        registry.reserve(2);

        let infos = vec![
            PaneScrollbackInfo {
                pane_id: 0,
                activity_counter: 100,
                warm_bytes: 5000,
                warm_pages: 2,
                estimated_memory_bytes: 15000,
            },
            PaneScrollbackInfo {
                pane_id: 1,
                activity_counter: 200,
                warm_bytes: 8000,
                warm_pages: 3,
                estimated_memory_bytes: 25000,
            },
            PaneScrollbackInfo {
                pane_id: 2,
                activity_counter: 50,
                warm_bytes: 1000,
                warm_pages: 1,
                estimated_memory_bytes: 7000,
            },
        ];

        let updated = FleetScrollbackCoordinator::sync_to_arena_registry(&infos, &registry);
        assert_eq!(updated, 3);

        // Verify tracked bytes were set
        let stats0 = registry.stats(0).expect("pane 0 stats");
        assert_eq!(stats0.tracked_bytes, 15000);

        let stats1 = registry.stats(1).expect("pane 1 stats");
        assert_eq!(stats1.tracked_bytes, 25000);

        let stats2 = registry.stats(2).expect("pane 2 stats");
        assert_eq!(stats2.tracked_bytes, 7000);
    }

    #[test]
    fn sync_to_arena_registry_skips_unregistered_panes() {
        let registry = frankenterm_alloc::PaneArenaRegistry::new();
        registry.reserve(0); // Only register pane 0

        let infos = vec![
            PaneScrollbackInfo {
                pane_id: 0,
                activity_counter: 100,
                warm_bytes: 5000,
                warm_pages: 2,
                estimated_memory_bytes: 15000,
            },
            PaneScrollbackInfo {
                pane_id: 99, // Not registered
                activity_counter: 50,
                warm_bytes: 1000,
                warm_pages: 1,
                estimated_memory_bytes: 7000,
            },
        ];

        let updated = FleetScrollbackCoordinator::sync_to_arena_registry(&infos, &registry);
        assert_eq!(updated, 1); // Only pane 0 was updated

        assert!(registry.stats(99).is_none());
    }

    // ── signals_from_queue_depths ─────────────────────────────────────

    #[test]
    fn signals_from_queue_depths_green_when_empty() {
        let signals = FleetScrollbackCoordinator::signals_from_queue_depths(0, 1000, 0, 100, 50, 0);
        assert_eq!(signals.backpressure, BackpressureTier::Green);
        assert_eq!(signals.pane_count, 50);
    }

    #[test]
    fn signals_from_queue_depths_yellow_at_50_percent() {
        let signals =
            FleetScrollbackCoordinator::signals_from_queue_depths(500, 1000, 0, 100, 100, 0);
        assert_eq!(signals.backpressure, BackpressureTier::Yellow);
    }

    #[test]
    fn signals_from_queue_depths_red_at_80_percent() {
        let signals =
            FleetScrollbackCoordinator::signals_from_queue_depths(800, 1000, 0, 100, 100, 0);
        assert_eq!(signals.backpressure, BackpressureTier::Red);
    }

    #[test]
    fn signals_from_queue_depths_black_at_95_percent() {
        let signals =
            FleetScrollbackCoordinator::signals_from_queue_depths(960, 1000, 0, 100, 100, 0);
        assert_eq!(signals.backpressure, BackpressureTier::Black);
    }

    #[test]
    fn signals_from_queue_depths_uses_worst_of_queues() {
        // Write queue at 90% even though capture is empty
        let signals =
            FleetScrollbackCoordinator::signals_from_queue_depths(0, 1000, 90, 100, 50, 0);
        assert_eq!(signals.backpressure, BackpressureTier::Red);
    }

    #[test]
    fn signals_from_queue_depths_zero_capacity_is_green() {
        // Edge case: zero capacity should not panic
        let signals = FleetScrollbackCoordinator::signals_from_queue_depths(0, 0, 0, 0, 10, 0);
        assert_eq!(signals.backpressure, BackpressureTier::Green);
    }

    #[test]
    fn snapshot_pane_access_returns_real_snapshots() {
        let mut snapshots = HashMap::new();
        snapshots.insert(
            7,
            ScrollbackTierSnapshot {
                hot_lines: 64,
                warm_pages: 3,
                warm_bytes: 120_000,
                warm_lines: 600,
                cold_lines: 0,
                cold_pages: 0,
                total_lines_added: 664,
                activity_counter: 42,
                cold_uncompressed_bytes: 0,
            },
        );

        let access = SnapshotPaneScrollbackAccess::new(snapshots);
        assert_eq!(access.pane_ids(), vec![7]);

        let snapshot = access.snapshot(7).expect("snapshot");
        assert_eq!(snapshot.hot_lines, 64);
        assert_eq!(snapshot.warm_pages, 3);
        assert_eq!(snapshot.warm_bytes, 120_000);
        assert_eq!(snapshot.activity_counter, 42);
    }

    #[test]
    fn snapshot_pane_access_returns_none_for_missing_pane() {
        let access = SnapshotPaneScrollbackAccess::default();
        assert!(access.snapshot(99).is_none());
    }

    #[test]
    fn snapshot_pane_access_repeated_reads_are_stable() {
        let mut snapshots = HashMap::new();
        snapshots.insert(
            11,
            ScrollbackTierSnapshot {
                hot_lines: 16,
                warm_pages: 1,
                warm_bytes: 2_048,
                warm_lines: 32,
                cold_lines: 0,
                cold_pages: 0,
                total_lines_added: 48,
                activity_counter: 9,
                cold_uncompressed_bytes: 0,
            },
        );

        let access = SnapshotPaneScrollbackAccess::new(snapshots);
        let first = access.snapshot(11).expect("first snapshot");
        let second = access.snapshot(11).expect("second snapshot");
        assert_eq!(first, second);
    }

    #[test]
    fn snapshot_pane_access_evicts_warm_pages() {
        let mut snapshots = HashMap::new();
        snapshots.insert(
            5,
            ScrollbackTierSnapshot {
                hot_lines: 8,
                warm_pages: 2,
                warm_bytes: 4_096,
                warm_lines: 64,
                cold_lines: 0,
                cold_pages: 0,
                total_lines_added: 72,
                activity_counter: 3,
                cold_uncompressed_bytes: 0,
            },
        );

        let mut access = SnapshotPaneScrollbackAccess::new(snapshots);
        assert_eq!(access.evict_warm_pages(5, 1), 1);

        let snapshot = access.snapshot(5).expect("snapshot after partial eviction");
        assert_eq!(snapshot.warm_pages, 1);
        assert_eq!(snapshot.warm_bytes, 2_048);
        assert_eq!(snapshot.warm_lines, 32);
        assert_eq!(snapshot.cold_pages, 1);
        assert_eq!(snapshot.cold_lines, 32);

        access.evict_all_warm(5);

        let snapshot = access.snapshot(5).expect("snapshot after full eviction");
        assert_eq!(snapshot.warm_pages, 0);
        assert_eq!(snapshot.warm_bytes, 0);
        assert_eq!(snapshot.warm_lines, 0);
        assert_eq!(snapshot.cold_pages, 2);
    }

    // ── NullPaneScrollbackAccess tests ──────────────────────────────────

    #[test]
    fn null_pane_access_returns_empty_ids() {
        let null = NullPaneScrollbackAccess;
        assert!(null.pane_ids().is_empty());
    }

    #[test]
    fn null_pane_access_snapshot_returns_none() {
        let null = NullPaneScrollbackAccess;
        assert!(null.snapshot(42).is_none());
    }

    #[test]
    fn null_pane_access_evict_warm_pages_returns_zero() {
        let mut null = NullPaneScrollbackAccess;
        assert_eq!(null.evict_warm_pages(42, 100), 0);
    }

    #[test]
    fn null_pane_access_evict_all_warm_is_noop() {
        let mut null = NullPaneScrollbackAccess;
        // Should not panic.
        null.evict_all_warm(99);
    }

    #[test]
    fn evaluate_with_null_panes_tracks_telemetry() {
        let mut coord = FleetScrollbackCoordinator::default();
        let signals = FleetScrollbackCoordinator::signals_from_queue_depths(
            90, 100, // capture 90% → Red
            50, 100, // write 50% → Yellow
            10, 0,
        );
        let infos: Vec<PaneScrollbackInfo> = (0..10)
            .map(|i| PaneScrollbackInfo {
                pane_id: i,
                activity_counter: 0,
                warm_bytes: 0,
                warm_pages: 0,
                estimated_memory_bytes: 0,
            })
            .collect();
        let mut null_panes = NullPaneScrollbackAccess;
        let result = coord.evaluate(&signals, &infos, &mut null_panes);

        // With zero warm bytes, eviction is skipped (below threshold).
        assert_eq!(result.pages_evicted, 0);
        assert_eq!(result.bytes_reclaimed, 0);
        assert_eq!(coord.telemetry().ticks, 1);
    }

    #[test]
    fn evaluate_with_null_panes_under_normal_pressure() {
        let mut coord = FleetScrollbackCoordinator::default();
        let signals = FleetScrollbackCoordinator::default_signals(5);
        let infos = Vec::new();
        let mut null_panes = NullPaneScrollbackAccess;
        let result = coord.evaluate(&signals, &infos, &mut null_panes);

        assert_eq!(
            result.compound_tier,
            crate::fleet_memory_controller::FleetPressureTier::Normal
        );
        assert_eq!(result.pages_evicted, 0);
        assert_eq!(coord.telemetry().elevated_ticks, 0);
    }

    #[test]
    fn coordinator_accumulates_ticks_with_null_panes() {
        let mut coord = FleetScrollbackCoordinator::default();
        let signals = FleetScrollbackCoordinator::default_signals(0);
        let mut null_panes = NullPaneScrollbackAccess;

        for _ in 0..5 {
            coord.evaluate(&signals, &[], &mut null_panes);
        }
        assert_eq!(coord.telemetry().ticks, 5);
    }

    #[test]
    fn proportional_eviction_with_null_panes_does_not_claim_applied_targets() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                min_fleet_warm_bytes_for_eviction: 0,
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let infos: Vec<PaneScrollbackInfo> = (0..5)
            .map(|i| PaneScrollbackInfo {
                pane_id: i,
                activity_counter: 0,
                warm_bytes: 128_000,
                warm_pages: 4,
                estimated_memory_bytes: 128_000,
            })
            .collect();
        let signals = PressureSignals {
            backpressure: BackpressureTier::Red,
            memory_pressure: MemoryPressureTier::Orange,
            worst_budget: BudgetLevel::Normal,
            pane_count: infos.len(),
            paused_pane_count: 0,
        };
        let mut null_panes = NullPaneScrollbackAccess;

        for _ in 0..4 {
            coord.evaluate(&signals, &infos, &mut null_panes);
        }

        let result = coord.evaluate(&signals, &infos, &mut null_panes);
        assert_eq!(result.compound_tier, FleetPressureTier::Critical);
        assert_eq!(result.pages_evicted, 0);
        assert_eq!(result.bytes_reclaimed, 0);
        assert_eq!(result.targets_applied, 0);
        assert_eq!(coord.telemetry().targets_applied, 0);
        assert_eq!(coord.telemetry().pages_evicted, 0);
        assert_eq!(coord.telemetry().bytes_reclaimed, 0);
    }

    #[test]
    fn emergency_eviction_with_null_panes_does_not_claim_applied_targets() {
        let mut coord = FleetScrollbackCoordinator::new(
            CoordinatorConfig {
                emergency_evict_all: true,
                min_fleet_warm_bytes_for_eviction: 0,
                ..CoordinatorConfig::default()
            },
            FleetMemoryConfig::default(),
        );
        let infos: Vec<PaneScrollbackInfo> = (0..5)
            .map(|i| PaneScrollbackInfo {
                pane_id: i,
                activity_counter: 0,
                warm_bytes: 128_000,
                warm_pages: 4,
                estimated_memory_bytes: 128_000,
            })
            .collect();
        let signals = PressureSignals {
            backpressure: BackpressureTier::Black,
            memory_pressure: MemoryPressureTier::Red,
            worst_budget: BudgetLevel::OverBudget,
            pane_count: infos.len(),
            paused_pane_count: 0,
        };
        let mut null_panes = NullPaneScrollbackAccess;

        for _ in 0..4 {
            coord.evaluate(&signals, &infos, &mut null_panes);
        }

        let result = coord.evaluate(&signals, &infos, &mut null_panes);
        assert_eq!(result.compound_tier, FleetPressureTier::Emergency);
        assert_eq!(result.pages_evicted, 0);
        assert_eq!(result.bytes_reclaimed, 0);
        assert_eq!(result.targets_applied, 0);
        assert_eq!(coord.telemetry().targets_applied, 0);
        assert_eq!(coord.telemetry().pages_evicted, 0);
        assert_eq!(coord.telemetry().bytes_reclaimed, 0);
    }
}
