//! Scrollback eviction — tier-based scrollback trimming under memory pressure.
//!
//! Reduces memory and SQLite storage by trimming captured scrollback data based
//! on pane activity tiers and system memory pressure.  Active panes keep full
//! scrollback; idle/dormant panes are trimmed progressively; under memory
//! pressure, all panes are trimmed aggressively.
//!
//! # Architecture
//!
//! ```text
//! MemoryPressureTier ──┐
//!                      ├──► EvictionPolicy ──► EvictionPlan ──► SegmentStore
//! PaneTier per pane ───┘
//! ```
//!
//! The module computes per-pane segment limits from pane tier + memory pressure,
//! then delegates actual deletion to a [`SegmentStore`] trait implementor.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::memory_pressure::MemoryPressureTier;
use crate::pane_tiers::PaneTier;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for scrollback eviction policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionConfig {
    /// Max segments for active panes under no memory pressure.
    pub active_max_segments: usize,
    /// Max segments for thinking panes.
    pub thinking_max_segments: usize,
    /// Max segments for idle panes.
    pub idle_max_segments: usize,
    /// Max segments for background panes.
    pub background_max_segments: usize,
    /// Max segments for dormant panes.
    pub dormant_max_segments: usize,
    /// Under memory pressure, override all limits to this value.
    pub pressure_max_segments: usize,
    /// Minimum segments to always keep (floor for any pane).
    pub min_segments: usize,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            active_max_segments: 10_000,
            thinking_max_segments: 5_000,
            idle_max_segments: 1_000,
            background_max_segments: 500,
            dormant_max_segments: 100,
            pressure_max_segments: 200,
            min_segments: 10,
        }
    }
}

impl EvictionConfig {
    /// Compute the max segments for a pane given its tier and current pressure.
    #[must_use]
    pub fn max_segments_for(
        &self,
        tier: PaneTier,
        pressure: MemoryPressureTier,
    ) -> usize {
        let base = match tier {
            PaneTier::Active => self.active_max_segments,
            PaneTier::Thinking => self.thinking_max_segments,
            PaneTier::Idle => self.idle_max_segments,
            PaneTier::Background => self.background_max_segments,
            PaneTier::Dormant => self.dormant_max_segments,
        };

        let effective = match pressure {
            MemoryPressureTier::Green => base,
            MemoryPressureTier::Yellow => base / 2,
            MemoryPressureTier::Orange => base / 4,
            // Red: emergency cap, but never more generous than Orange
            MemoryPressureTier::Red => (base / 4).min(self.pressure_max_segments),
        };

        effective.max(self.min_segments)
    }
}

// =============================================================================
// Eviction Plan
// =============================================================================

/// Per-pane eviction target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionTarget {
    pub pane_id: u64,
    pub tier: PaneTier,
    pub current_segments: usize,
    pub max_segments: usize,
    pub segments_to_remove: usize,
}

/// Full eviction plan across all panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionPlan {
    pub pressure: MemoryPressureTier,
    pub targets: Vec<EvictionTarget>,
    pub total_segments_to_remove: usize,
    pub panes_affected: usize,
}

impl EvictionPlan {
    /// Whether this plan requires any eviction work.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_segments_to_remove == 0
    }
}

// =============================================================================
// Eviction Report
// =============================================================================

/// Result of executing an eviction plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvictionReport {
    pub panes_trimmed: usize,
    pub segments_removed: usize,
    pub errors: Vec<String>,
}

// =============================================================================
// Segment Store Trait
// =============================================================================

/// Trait for segment storage operations needed by the evictor.
///
/// Implementations provide actual database access; the trait enables testing
/// with mocks.
pub trait SegmentStore: Send + Sync {
    /// Count segments for a given pane.
    fn count_segments(&self, pane_id: u64) -> Result<usize, String>;

    /// Delete the oldest `count` segments for a pane, preserving the newest.
    ///
    /// Returns the number of segments actually deleted.
    fn delete_oldest_segments(
        &self,
        pane_id: u64,
        count: usize,
    ) -> Result<usize, String>;

    /// List all known pane IDs.
    fn list_pane_ids(&self) -> Result<Vec<u64>, String>;
}

// =============================================================================
// Pane Info Source Trait
// =============================================================================

/// Provides per-pane tier classification.
pub trait PaneTierSource: Send + Sync {
    /// Get the current tier for a pane. Returns `None` if the pane is unknown.
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier>;
}

// =============================================================================
// Scrollback Evictor
// =============================================================================

/// Computes and executes tier-based scrollback eviction.
pub struct ScrollbackEvictor<S: SegmentStore, T: PaneTierSource> {
    config: EvictionConfig,
    store: S,
    tier_source: T,
}

impl<S: SegmentStore, T: PaneTierSource> ScrollbackEvictor<S, T> {
    /// Create a new evictor.
    pub fn new(config: EvictionConfig, store: S, tier_source: T) -> Self {
        Self {
            config,
            store,
            tier_source,
        }
    }

    /// Compute an eviction plan without executing it.
    pub fn plan(&self, pressure: MemoryPressureTier) -> Result<EvictionPlan, String> {
        let pane_ids = self.store.list_pane_ids()?;
        let mut targets = Vec::new();
        let mut total_to_remove = 0usize;

        for pane_id in pane_ids {
            let tier = self
                .tier_source
                .tier_for(pane_id)
                .unwrap_or(PaneTier::Dormant); // Unknown panes treated as dormant

            let current = self.store.count_segments(pane_id)?;
            let max = self.config.max_segments_for(tier, pressure);

            if current > max {
                let to_remove = current - max;
                total_to_remove += to_remove;
                targets.push(EvictionTarget {
                    pane_id,
                    tier,
                    current_segments: current,
                    max_segments: max,
                    segments_to_remove: to_remove,
                });
            }
        }

        let panes_affected = targets.len();

        Ok(EvictionPlan {
            pressure,
            targets,
            total_segments_to_remove: total_to_remove,
            panes_affected,
        })
    }

    /// Execute an eviction plan, deleting excess segments.
    pub fn execute(&self, plan: &EvictionPlan) -> EvictionReport {
        let mut report = EvictionReport::default();

        for target in &plan.targets {
            match self
                .store
                .delete_oldest_segments(target.pane_id, target.segments_to_remove)
            {
                Ok(deleted) => {
                    report.segments_removed += deleted;
                    if deleted > 0 {
                        report.panes_trimmed += 1;
                    }
                }
                Err(e) => {
                    report.errors.push(format!(
                        "pane {}: failed to delete {} segments: {}",
                        target.pane_id, target.segments_to_remove, e
                    ));
                }
            }
        }

        report
    }

    /// Plan and execute in one step.
    pub fn evict(&self, pressure: MemoryPressureTier) -> Result<EvictionReport, String> {
        let plan = self.plan(pressure)?;
        Ok(self.execute(&plan))
    }

    /// Get the current config.
    #[must_use]
    pub fn config(&self) -> &EvictionConfig {
        &self.config
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mock implementations ──────────────────────────────────────────

    /// Simple in-memory segment store for testing.
    #[derive(Debug, Default)]
    struct MockStore {
        segments: HashMap<u64, usize>,
    }

    impl MockStore {
        fn with_panes(panes: &[(u64, usize)]) -> Self {
            Self {
                segments: panes.iter().copied().collect(),
            }
        }
    }

    impl SegmentStore for MockStore {
        fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
            Ok(*self.segments.get(&pane_id).unwrap_or(&0))
        }

        fn delete_oldest_segments(
            &self,
            _pane_id: u64,
            count: usize,
        ) -> Result<usize, String> {
            Ok(count) // Pretend we deleted them
        }

        fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
            let mut ids: Vec<_> = self.segments.keys().copied().collect();
            ids.sort();
            Ok(ids)
        }
    }

    /// Tier source that maps pane IDs to predetermined tiers.
    struct MockTierSource {
        tiers: HashMap<u64, PaneTier>,
    }

    impl MockTierSource {
        fn new(tiers: &[(u64, PaneTier)]) -> Self {
            Self {
                tiers: tiers.iter().copied().collect(),
            }
        }
    }

    impl PaneTierSource for MockTierSource {
        fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
            self.tiers.get(&pane_id).copied()
        }
    }

    fn default_evictor(
        panes: &[(u64, usize)],
        tiers: &[(u64, PaneTier)],
    ) -> ScrollbackEvictor<MockStore, MockTierSource> {
        ScrollbackEvictor::new(
            EvictionConfig::default(),
            MockStore::with_panes(panes),
            MockTierSource::new(tiers),
        )
    }

    // ── Config tests ──────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = EvictionConfig::default();
        assert_eq!(c.active_max_segments, 10_000);
        assert_eq!(c.dormant_max_segments, 100);
        assert_eq!(c.pressure_max_segments, 200);
        assert_eq!(c.min_segments, 10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = EvictionConfig {
            active_max_segments: 5000,
            thinking_max_segments: 2000,
            idle_max_segments: 500,
            background_max_segments: 250,
            dormant_max_segments: 50,
            pressure_max_segments: 100,
            min_segments: 5,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: EvictionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.active_max_segments, 5000);
        assert_eq!(parsed.min_segments, 5);
    }

    // ── max_segments_for tests ────────────────────────────────────────

    #[test]
    fn active_green_gets_full_limit() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Green),
            10_000
        );
    }

    #[test]
    fn dormant_green_gets_dormant_limit() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Dormant, MemoryPressureTier::Green),
            100
        );
    }

    #[test]
    fn yellow_pressure_halves_limits() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Yellow),
            5_000
        );
        assert_eq!(
            c.max_segments_for(PaneTier::Idle, MemoryPressureTier::Yellow),
            500
        );
    }

    #[test]
    fn orange_pressure_quarters_limits() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Orange),
            2_500
        );
    }

    #[test]
    fn red_pressure_uses_emergency_limit() {
        let c = EvictionConfig::default();
        // Active: min(10000/4, 200) = 200
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Red),
            200
        );
        // Dormant: min(100/4, 200) = 25, but min_segments floor = 25.max(10) = 25
        assert_eq!(
            c.max_segments_for(PaneTier::Dormant, MemoryPressureTier::Red),
            25
        );
    }

    #[test]
    fn min_segments_floor_respected() {
        let c = EvictionConfig {
            dormant_max_segments: 4, // Below min_segments (10)
            min_segments: 10,
            ..Default::default()
        };
        assert_eq!(
            c.max_segments_for(PaneTier::Dormant, MemoryPressureTier::Green),
            10
        );
    }

    #[test]
    fn min_segments_floor_under_pressure() {
        let c = EvictionConfig {
            pressure_max_segments: 3, // Below min_segments
            min_segments: 5,
            ..Default::default()
        };
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Red),
            5
        );
    }

    // ── Plan tests ────────────────────────────────────────────────────

    #[test]
    fn plan_no_eviction_needed() {
        let ev = default_evictor(
            &[(1, 100), (2, 50)],
            &[(1, PaneTier::Active), (2, PaneTier::Idle)],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.panes_affected, 0);
    }

    #[test]
    fn plan_trims_over_limit_panes() {
        let ev = default_evictor(
            &[
                (1, 15_000), // Active: limit 10000, over by 5000
                (2, 500),    // Idle: limit 1000, under
                (3, 200),    // Dormant: limit 100, over by 100
            ],
            &[
                (1, PaneTier::Active),
                (2, PaneTier::Idle),
                (3, PaneTier::Dormant),
            ],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        assert_eq!(plan.panes_affected, 2);
        assert_eq!(plan.total_segments_to_remove, 5100);

        let t1 = plan.targets.iter().find(|t| t.pane_id == 1).unwrap();
        assert_eq!(t1.segments_to_remove, 5000);
        assert_eq!(t1.max_segments, 10_000);

        let t3 = plan.targets.iter().find(|t| t.pane_id == 3).unwrap();
        assert_eq!(t3.segments_to_remove, 100);
    }

    #[test]
    fn plan_under_pressure_trims_more() {
        let ev = default_evictor(
            &[(1, 5000), (2, 5000)],
            &[(1, PaneTier::Active), (2, PaneTier::Idle)],
        );

        let green_plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let red_plan = ev.plan(MemoryPressureTier::Red).unwrap();

        // Green: active has 5000 < 10000, idle has 5000 > 1000
        assert_eq!(green_plan.total_segments_to_remove, 4000);

        // Red: both panes get 200 limit, so 4800 + 4800 = 9600
        assert_eq!(red_plan.total_segments_to_remove, 9600);
        assert!(
            red_plan.total_segments_to_remove > green_plan.total_segments_to_remove,
            "red pressure should trim more than green"
        );
    }

    #[test]
    fn plan_unknown_panes_treated_as_dormant() {
        let ev = default_evictor(
            &[(99, 500)], // Pane 99 not in tier source
            &[],          // No tier mappings
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        // Dormant limit = 100, so 500 - 100 = 400 to remove
        assert_eq!(plan.total_segments_to_remove, 400);
    }

    // ── Execute tests ─────────────────────────────────────────────────

    #[test]
    fn execute_reports_results() {
        let ev = default_evictor(
            &[(1, 15_000), (2, 500)],
            &[(1, PaneTier::Active), (2, PaneTier::Dormant)],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let report = ev.execute(&plan);

        assert_eq!(report.panes_trimmed, 2);
        assert_eq!(report.segments_removed, 5400); // 5000 + 400
        assert!(report.errors.is_empty());
    }

    #[test]
    fn execute_empty_plan_is_noop() {
        let ev = default_evictor(
            &[(1, 100)],
            &[(1, PaneTier::Active)],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let report = ev.execute(&plan);

        assert_eq!(report.panes_trimmed, 0);
        assert_eq!(report.segments_removed, 0);
    }

    #[test]
    fn evict_convenience_method() {
        let ev = default_evictor(
            &[(1, 500)],
            &[(1, PaneTier::Dormant)],
        );

        let report = ev.evict(MemoryPressureTier::Green).unwrap();
        assert_eq!(report.segments_removed, 400);
    }

    // ── Error handling ────────────────────────────────────────────────

    struct FailingStore;

    impl SegmentStore for FailingStore {
        fn count_segments(&self, _pane_id: u64) -> Result<usize, String> {
            Ok(1000)
        }

        fn delete_oldest_segments(
            &self,
            _pane_id: u64,
            _count: usize,
        ) -> Result<usize, String> {
            Err("disk full".to_string())
        }

        fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
            Ok(vec![1])
        }
    }

    #[test]
    fn execute_records_errors() {
        let ev = ScrollbackEvictor::new(
            EvictionConfig::default(),
            FailingStore,
            MockTierSource::new(&[(1, PaneTier::Dormant)]),
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let report = ev.execute(&plan);

        assert_eq!(report.panes_trimmed, 0);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("disk full"));
    }

    // ── Eviction plan serialization ───────────────────────────────────

    #[test]
    fn plan_serializes() {
        let ev = default_evictor(
            &[(1, 500), (2, 200)],
            &[(1, PaneTier::Active), (2, PaneTier::Dormant)],
        );

        let plan = ev.plan(MemoryPressureTier::Yellow).unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: EvictionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_segments_to_remove, plan.total_segments_to_remove);
    }

    #[test]
    fn report_serializes() {
        let report = EvictionReport {
            panes_trimmed: 3,
            segments_removed: 1500,
            errors: vec!["pane 5: timeout".to_string()],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: EvictionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.panes_trimmed, 3);
        assert_eq!(parsed.errors.len(), 1);
    }

    // ── Property-based tests ──────────────────────────────────────────

    /// Dormant panes always get trimmed more aggressively than idle,
    /// which are trimmed more aggressively than active.
    #[test]
    fn tier_ordering_invariant() {
        let config = EvictionConfig::default();

        for pressure in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let active = config.max_segments_for(PaneTier::Active, pressure);
            let thinking = config.max_segments_for(PaneTier::Thinking, pressure);
            let idle = config.max_segments_for(PaneTier::Idle, pressure);
            let background = config.max_segments_for(PaneTier::Background, pressure);
            let dormant = config.max_segments_for(PaneTier::Dormant, pressure);

            assert!(
                active >= thinking,
                "active({active}) >= thinking({thinking}) at {pressure:?}"
            );
            assert!(
                thinking >= idle,
                "thinking({thinking}) >= idle({idle}) at {pressure:?}"
            );
            assert!(
                idle >= background,
                "idle({idle}) >= background({background}) at {pressure:?}"
            );
            assert!(
                background >= dormant,
                "background({background}) >= dormant({dormant}) at {pressure:?}"
            );
        }
    }

    /// Higher pressure => equal or lower limits for every tier.
    #[test]
    fn pressure_monotonicity() {
        let config = EvictionConfig::default();
        let pressures = [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ];

        for tier in [
            PaneTier::Active,
            PaneTier::Thinking,
            PaneTier::Idle,
            PaneTier::Background,
            PaneTier::Dormant,
        ] {
            for window in pressures.windows(2) {
                let lower_pressure = config.max_segments_for(tier, window[0]);
                let higher_pressure = config.max_segments_for(tier, window[1]);
                assert!(
                    lower_pressure >= higher_pressure,
                    "{tier:?}: {lower_pressure} >= {higher_pressure} ({:?} vs {:?})",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// Trimming never removes more segments than the pane actually has.
    #[test]
    fn no_over_eviction() {
        let panes = vec![(1, 50), (2, 100), (3, 5000), (4, 0)];
        let tiers = vec![
            (1, PaneTier::Dormant),
            (2, PaneTier::Idle),
            (3, PaneTier::Active),
            (4, PaneTier::Active),
        ];

        let ev = default_evictor(&panes, &tiers);

        for pressure in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let plan = ev.plan(pressure).unwrap();
            for target in &plan.targets {
                assert!(
                    target.segments_to_remove <= target.current_segments,
                    "pane {}: removing {} > current {} at {pressure:?}",
                    target.pane_id,
                    target.segments_to_remove,
                    target.current_segments,
                );
            }
        }
    }

    /// Running plan twice with unchanged state produces same result.
    #[test]
    fn plan_idempotency() {
        let ev = default_evictor(
            &[(1, 5000), (2, 300)],
            &[(1, PaneTier::Idle), (2, PaneTier::Dormant)],
        );

        let plan1 = ev.plan(MemoryPressureTier::Green).unwrap();
        let plan2 = ev.plan(MemoryPressureTier::Green).unwrap();

        assert_eq!(plan1.total_segments_to_remove, plan2.total_segments_to_remove);
        assert_eq!(plan1.panes_affected, plan2.panes_affected);
    }

    /// Min segments floor prevents total eviction.
    #[test]
    fn min_segments_prevents_total_eviction() {
        let config = EvictionConfig {
            min_segments: 20,
            ..Default::default()
        };

        for tier in [
            PaneTier::Active,
            PaneTier::Thinking,
            PaneTier::Idle,
            PaneTier::Background,
            PaneTier::Dormant,
        ] {
            for pressure in [
                MemoryPressureTier::Green,
                MemoryPressureTier::Yellow,
                MemoryPressureTier::Orange,
                MemoryPressureTier::Red,
            ] {
                let max = config.max_segments_for(tier, pressure);
                assert!(
                    max >= 20,
                    "{tier:?} at {pressure:?}: max={max} < min_segments=20"
                );
            }
        }
    }

    /// With many panes at various tiers, total eviction never exceeds total excess.
    #[test]
    fn total_eviction_bounded() {
        let panes: Vec<(u64, usize)> = (0..50).map(|i| (i, 1000)).collect();
        let tiers: Vec<(u64, PaneTier)> = (0..50)
            .map(|i| {
                let tier = match i % 5 {
                    0 => PaneTier::Active,
                    1 => PaneTier::Thinking,
                    2 => PaneTier::Idle,
                    3 => PaneTier::Background,
                    _ => PaneTier::Dormant,
                };
                (i, tier)
            })
            .collect();

        let ev = default_evictor(&panes, &tiers);
        let total_segments: usize = panes.iter().map(|(_, c)| c).sum();

        for pressure in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let plan = ev.plan(pressure).unwrap();
            assert!(
                plan.total_segments_to_remove <= total_segments,
                "can't remove more than total at {pressure:?}: {} > {}",
                plan.total_segments_to_remove,
                total_segments,
            );
        }
    }
}
