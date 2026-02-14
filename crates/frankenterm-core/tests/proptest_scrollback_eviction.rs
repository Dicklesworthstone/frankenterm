//! Property-based tests for scrollback eviction.
//!
//! Bead: wa-3r5e
//!
//! Verifies the following properties:
//! 1. Tier ordering invariant: dormant gets fewer segments than idle, etc.
//! 2. No over-eviction: segments_to_remove <= current_segments - min_segments
//! 3. Pressure monotonicity: higher pressure → equal or more aggressive trimming
//! 4. Plan idempotency: planning twice yields identical results
//! 5. Min segments floor: every limit is >= min_segments
//! 6. Config round-trip: serialization preserves all fields
//! 7. Plan total consistency: sum of per-pane removals equals total_segments_to_remove
//! 8. Unknown panes default to dormant tier
//! 9. Importance scoring range invariant
//! 10. Importance scoring monotonicity
//! 11. Threshold floor ordering
//! 12. Byte budget compliance
//! 13. ImportanceScoringConfig serde roundtrip
//! 14. ScrollbackLine serde roundtrip
//! 15. ImportanceBudgetReport serde roundtrip
//! 16. EvictionTarget serde roundtrip
//! 17. EvictionPlan serde roundtrip
//! 18. EvictionReport serde roundtrip
//! 19. EvictionConfig default roundtrip
//! 20. ImportanceRetentionConfig validate invariants
//! 21. EvictionPlan is_empty consistency
//! 22. ScrollbackLine bytes consistency

use proptest::prelude::*;
use std::collections::{HashMap, VecDeque};

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, EvictionPlan, EvictionReport, EvictionTarget, ImportanceBudgetReport,
    ImportanceRetentionConfig, ImportanceScoringConfig, LineImportanceScorer, PaneTierSource,
    ScrollbackEvictor, ScrollbackLine, SegmentStore, enforce_importance_budget,
};

// ── Test helpers ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct PropStore {
    segments: HashMap<u64, usize>,
}

impl SegmentStore for PropStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(*self.segments.get(&pane_id).unwrap_or(&0))
    }

    fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
        Ok(count)
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        let mut ids: Vec<_> = self.segments.keys().copied().collect();
        ids.sort();
        Ok(ids)
    }
}

struct PropTierSource {
    tiers: HashMap<u64, PaneTier>,
}

impl PaneTierSource for PropTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        self.tiers.get(&pane_id).copied()
    }
}

/// Strategy that generates a valid EvictionConfig with
/// descending limits: active >= thinking >= idle >= background >= dormant >= min_segments.
fn arb_eviction_config() -> impl Strategy<Value = EvictionConfig> {
    // min_segments: 1..50, then build ascending limits
    (
        1usize..50,
        1usize..200,
        1usize..200,
        1usize..200,
        1usize..200,
        1usize..200,
        1usize..500,
    )
        .prop_map(|(min_seg, d0, d1, d2, d3, d4, pressure_max)| {
            // Build limits by accumulating deltas onto min_segments
            let dormant = min_seg + d0;
            let background = dormant + d1;
            let idle = background + d2;
            let thinking = idle + d3;
            let active = thinking + d4;
            let pressure = min_seg.max(pressure_max.min(active));

            EvictionConfig {
                active_max_segments: active,
                thinking_max_segments: thinking,
                idle_max_segments: idle,
                background_max_segments: background,
                dormant_max_segments: dormant,
                pressure_max_segments: pressure,
                min_segments: min_seg,
            }
        })
}

fn arb_pressure() -> impl Strategy<Value = MemoryPressureTier> {
    prop_oneof![
        Just(MemoryPressureTier::Green),
        Just(MemoryPressureTier::Yellow),
        Just(MemoryPressureTier::Orange),
        Just(MemoryPressureTier::Red),
    ]
}

fn arb_tier() -> impl Strategy<Value = PaneTier> {
    prop_oneof![
        Just(PaneTier::Active),
        Just(PaneTier::Thinking),
        Just(PaneTier::Idle),
        Just(PaneTier::Background),
        Just(PaneTier::Dormant),
    ]
}

fn arb_importance_retention_config() -> impl Strategy<Value = ImportanceRetentionConfig> {
    (1usize..2048, 1usize..32, 1usize..128, 0.1_f64..0.95).prop_map(
        |(budget, min_lines, extra_lines, threshold)| ImportanceRetentionConfig {
            byte_budget_per_pane: budget,
            min_lines,
            max_lines: min_lines + extra_lines,
            importance_threshold: threshold,
            oldest_window_fraction: 1.0,
        },
    )
}

fn arb_importance_scoring_config() -> impl Strategy<Value = ImportanceScoringConfig> {
    (
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
    )
        .prop_map(
            |(
                baseline,
                critical_bonus,
                warning_bonus,
                tool_boundary_bonus,
                compilation_bonus,
                test_result_bonus,
                blank_line_penalty,
                progress_line_penalty,
                ansi_only_penalty,
                repeated_line_penalty,
            )| {
                ImportanceScoringConfig {
                    baseline,
                    critical_bonus,
                    warning_bonus,
                    tool_boundary_bonus,
                    compilation_bonus,
                    test_result_bonus,
                    blank_line_penalty,
                    progress_line_penalty,
                    ansi_only_penalty,
                    repeated_line_penalty,
                }
            },
        )
}

fn arb_scrollback_line() -> impl Strategy<Value = ScrollbackLine> {
    ("[ -~]{0,60}", 0.0_f64..1.0, 0_u64..10_000_000_000).prop_map(
        |(text, importance, timestamp_ms)| ScrollbackLine::new(text, importance, timestamp_ms),
    )
}

fn arb_eviction_target() -> impl Strategy<Value = EvictionTarget> {
    (0_u64..1000, arb_tier(), 100_usize..20_000, 1_usize..100).prop_map(
        |(pane_id, tier, current_segments, to_remove)| {
            let to_remove = to_remove.min(current_segments);
            let max_segments = current_segments - to_remove;
            EvictionTarget {
                pane_id,
                tier,
                current_segments,
                max_segments,
                segments_to_remove: to_remove,
            }
        },
    )
}

fn arb_eviction_plan() -> impl Strategy<Value = EvictionPlan> {
    (
        arb_pressure(),
        prop::collection::vec(arb_eviction_target(), 0..10),
    )
        .prop_map(|(pressure, targets)| {
            let total_segments_to_remove: usize =
                targets.iter().map(|t| t.segments_to_remove).sum();
            let panes_affected = targets.len();
            EvictionPlan {
                pressure,
                targets,
                total_segments_to_remove,
                panes_affected,
            }
        })
}

fn arb_eviction_report() -> impl Strategy<Value = EvictionReport> {
    (
        0_usize..100,
        0_usize..10_000,
        prop::collection::vec("[a-z ]{5,20}", 0..5),
    )
        .prop_map(|(panes_trimmed, segments_removed, errors)| EvictionReport {
            panes_trimmed,
            segments_removed,
            errors,
        })
}

fn arb_importance_budget_report() -> impl Strategy<Value = ImportanceBudgetReport> {
    (
        0_usize..500,
        0_usize..50_000,
        0_usize..5000,
        0_usize..200_000,
    )
        .prop_map(
            |(lines_removed, bytes_removed, remaining_lines, remaining_bytes)| {
                ImportanceBudgetReport {
                    lines_removed,
                    bytes_removed,
                    remaining_lines,
                    remaining_bytes,
                }
            },
        )
}

const ALL_PRESSURES: [MemoryPressureTier; 4] = [
    MemoryPressureTier::Green,
    MemoryPressureTier::Yellow,
    MemoryPressureTier::Orange,
    MemoryPressureTier::Red,
];

const ALL_TIERS: [PaneTier; 5] = [
    PaneTier::Active,
    PaneTier::Thinking,
    PaneTier::Idle,
    PaneTier::Background,
    PaneTier::Dormant,
];

// =============================================================================
// 1. Tier ordering invariant
// =============================================================================

proptest! {
    /// For any valid config and pressure level, segment limits must be
    /// non-increasing as tiers go from Active → Dormant.
    #[test]
    fn proptest_tier_ordering_invariant(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
    ) {
        let active = config.max_segments_for(PaneTier::Active, pressure);
        let thinking = config.max_segments_for(PaneTier::Thinking, pressure);
        let idle = config.max_segments_for(PaneTier::Idle, pressure);
        let background = config.max_segments_for(PaneTier::Background, pressure);
        let dormant = config.max_segments_for(PaneTier::Dormant, pressure);

        prop_assert!(active >= thinking,
            "active({}) >= thinking({}) at {:?}", active, thinking, pressure);
        prop_assert!(thinking >= idle,
            "thinking({}) >= idle({}) at {:?}", thinking, idle, pressure);
        prop_assert!(idle >= background,
            "idle({}) >= background({}) at {:?}", idle, background, pressure);
        prop_assert!(background >= dormant,
            "background({}) >= dormant({}) at {:?}", background, dormant, pressure);
    }
}

// =============================================================================
// 9. Importance scoring range invariant
// =============================================================================

proptest! {
    /// For any UTF-8 input line, score_line() must stay in [0, 1].
    #[test]
    fn proptest_importance_score_range(
        line in ".*",
        prev in prop::option::of(".*"),
    ) {
        let scorer = LineImportanceScorer::default();
        let score = scorer.score_line(&line, prev.as_deref());
        prop_assert!(
            (0.0..=1.0).contains(&score),
            "score out of range: {score} for line={line:?} prev={prev:?}"
        );
    }
}

// =============================================================================
// 10. Importance scoring monotonicity
// =============================================================================

proptest! {
    /// Adding high-value signals (and no extra low-value signals) must not decrease score.
    #[test]
    fn proptest_importance_monotonicity(
        include_warning in any::<bool>(),
        include_tool in any::<bool>(),
        include_compile in any::<bool>(),
        include_test in any::<bool>(),
    ) {
        let scorer = LineImportanceScorer::default();
        let mut base = String::from("status update");
        if include_warning {
            base.push_str(" warning:");
        }
        if include_tool {
            base.push_str(" Using tool");
        }
        if include_compile {
            base.push_str(" Compiling crate");
        }
        if include_test {
            base.push_str(" test result:");
        }

        let boosted = format!("{base} error: failed");
        let base_score = scorer.score_line(&base, None);
        let boosted_score = scorer.score_line(&boosted, None);

        prop_assert!(
            boosted_score >= base_score,
            "boosted score must be >= base score (base={base_score}, boosted={boosted_score}, base_line={base:?}, boosted_line={boosted:?})"
        );
    }
}

// =============================================================================
// 11. Threshold floor ordering
// =============================================================================

proptest! {
    /// While low-importance lines exist, high-importance lines at/above threshold
    /// should not be evicted first.
    #[test]
    fn proptest_threshold_floor_ordering(
        config in arb_importance_retention_config(),
        low_importance in 0.0_f64..0.79,
        high_importance in 0.8_f64..1.0,
    ) {
        let mut lines = VecDeque::new();
        for idx in 0..config.max_lines.max(4) {
            let text = format!("line-{idx}");
            let importance = if idx % 2 == 0 {
                low_importance.min(config.importance_threshold - f64::EPSILON)
            } else {
                high_importance.max(config.importance_threshold)
            };
            lines.push_back(ScrollbackLine::new(text, importance, idx as u64));
        }

        let mut overfull = lines.clone();
        overfull.push_back(ScrollbackLine::new(
            "extra-low",
            0.01_f64.min(config.importance_threshold - f64::EPSILON),
            99_999,
        ));

        let report = enforce_importance_budget(&mut overfull, &config);
        if report.lines_removed > 0 {
            let has_low = overfull
                .iter()
                .any(|line| line.importance < config.importance_threshold);
            if has_low {
                prop_assert!(
                    overfull.iter().any(|line| line.importance >= config.importance_threshold),
                    "high-importance lines should still be present while low lines remain"
                );
            }
        }
    }
}

// =============================================================================
// 12. Byte budget compliance
// =============================================================================

proptest! {
    /// After budget enforcement, bytes and line counts remain within limits,
    /// unless min_lines prevents further trimming.
    #[test]
    fn proptest_importance_budget_compliance(
        config in arb_importance_retention_config(),
        texts in prop::collection::vec("[ -~]{1,40}", 1..120),
        importances in prop::collection::vec(0.0_f64..1.0, 1..120),
    ) {
        let mut lines = VecDeque::new();
        let n = texts.len().min(importances.len());
        for i in 0..n {
            lines.push_back(ScrollbackLine::new(
                texts[i].clone(),
                importances[i],
                i as u64,
            ));
        }

        let report = enforce_importance_budget(&mut lines, &config);

        let remaining_bytes: usize = lines.iter().map(|line| line.bytes).sum();
        prop_assert_eq!(remaining_bytes, report.remaining_bytes);
        prop_assert_eq!(lines.len(), report.remaining_lines);
        prop_assert!(lines.len() <= config.max_lines || lines.len() == config.min_lines);
        if lines.len() > config.min_lines {
            prop_assert!(
                remaining_bytes <= config.byte_budget_per_pane,
                "remaining bytes {} exceed budget {}",
                remaining_bytes,
                config.byte_budget_per_pane
            );
        }
    }
}

// =============================================================================
// 2. No over-eviction
// =============================================================================

proptest! {
    /// segments_to_remove for any target never exceeds (current_segments - max_segments),
    /// and max_segments is always >= min_segments.
    #[test]
    fn proptest_no_over_eviction(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..30),
        pane_tiers in prop::collection::vec(arb_tier(), 1..30),
    ) {
        let n = pane_segments.len().min(pane_tiers.len());
        let store = PropStore {
            segments: (0..n).map(|i| (i as u64, pane_segments[i])).collect(),
        };
        let tier_source = PropTierSource {
            tiers: (0..n).map(|i| (i as u64, pane_tiers[i])).collect(),
        };
        let evictor = ScrollbackEvictor::new(config.clone(), store, tier_source);

        let plan = evictor.plan(pressure).unwrap();
        for target in &plan.targets {
            // Can never remove more than what exists
            prop_assert!(
                target.segments_to_remove <= target.current_segments,
                "pane {}: removing {} > current {}",
                target.pane_id, target.segments_to_remove, target.current_segments
            );
            // After eviction, remaining segments == max_segments
            prop_assert!(
                target.current_segments - target.segments_to_remove == target.max_segments,
                "pane {}: remaining {} != max {}",
                target.pane_id,
                target.current_segments - target.segments_to_remove,
                target.max_segments
            );
            // max_segments always respects the floor
            prop_assert!(
                target.max_segments >= config.min_segments,
                "pane {}: max {} < min_segments {}",
                target.pane_id, target.max_segments, config.min_segments
            );
        }
    }
}

// =============================================================================
// 3. Pressure monotonicity
// =============================================================================

proptest! {
    /// Higher memory pressure levels must produce equal or more aggressive
    /// eviction (higher total_segments_to_remove) for the same pane layout.
    #[test]
    fn proptest_pressure_monotonicity(
        config in arb_eviction_config(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..30),
        pane_tiers in prop::collection::vec(arb_tier(), 1..30),
    ) {
        let n = pane_segments.len().min(pane_tiers.len());
        let segments: HashMap<u64, usize> = (0..n).map(|i| (i as u64, pane_segments[i])).collect();
        let tiers: HashMap<u64, PaneTier> = (0..n).map(|i| (i as u64, pane_tiers[i])).collect();

        let mut prev_total = 0usize;
        for pressure in ALL_PRESSURES {
            let store = PropStore { segments: segments.clone() };
            let tier_source = PropTierSource { tiers: tiers.clone() };
            let evictor = ScrollbackEvictor::new(config.clone(), store, tier_source);
            let plan = evictor.plan(pressure).unwrap();

            prop_assert!(
                plan.total_segments_to_remove >= prev_total,
                "{:?}: total {} < previous {}",
                pressure, plan.total_segments_to_remove, prev_total
            );
            prev_total = plan.total_segments_to_remove;
        }
    }
}

// =============================================================================
// 4. Plan idempotency
// =============================================================================

proptest! {
    /// Computing the plan twice on the same state yields identical results.
    #[test]
    fn proptest_plan_idempotency(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..20),
    ) {
        let n = pane_segments.len();
        let store = PropStore {
            segments: (0..n).map(|i| (i as u64, pane_segments[i])).collect(),
        };
        // All dormant for simplicity
        let tier_source = PropTierSource {
            tiers: (0..n).map(|i| (i as u64, PaneTier::Dormant)).collect(),
        };
        let evictor = ScrollbackEvictor::new(config, store, tier_source);

        let plan1 = evictor.plan(pressure).unwrap();
        let plan2 = evictor.plan(pressure).unwrap();

        prop_assert_eq!(plan1.total_segments_to_remove, plan2.total_segments_to_remove);
        prop_assert_eq!(plan1.panes_affected, plan2.panes_affected);
        prop_assert_eq!(plan1.targets.len(), plan2.targets.len());
    }
}

// =============================================================================
// 5. Min segments floor
// =============================================================================

proptest! {
    /// For any tier × pressure combination, the computed limit is >= min_segments.
    #[test]
    fn proptest_min_segments_floor(
        config in arb_eviction_config(),
    ) {
        for tier in ALL_TIERS {
            for pressure in ALL_PRESSURES {
                let max = config.max_segments_for(tier, pressure);
                prop_assert!(
                    max >= config.min_segments,
                    "{:?} at {:?}: max {} < min_segments {}",
                    tier, pressure, max, config.min_segments
                );
            }
        }
    }
}

// =============================================================================
// 6. Config serde round-trip
// =============================================================================

proptest! {
    /// Serializing and deserializing an EvictionConfig preserves all fields.
    #[test]
    fn proptest_config_roundtrip(
        config in arb_eviction_config(),
    ) {
        let json = serde_json::to_string(&config).unwrap();
        let parsed: EvictionConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(config.active_max_segments, parsed.active_max_segments);
        prop_assert_eq!(config.thinking_max_segments, parsed.thinking_max_segments);
        prop_assert_eq!(config.idle_max_segments, parsed.idle_max_segments);
        prop_assert_eq!(config.background_max_segments, parsed.background_max_segments);
        prop_assert_eq!(config.dormant_max_segments, parsed.dormant_max_segments);
        prop_assert_eq!(config.pressure_max_segments, parsed.pressure_max_segments);
        prop_assert_eq!(config.min_segments, parsed.min_segments);
    }
}

// =============================================================================
// 7. Plan total consistency
// =============================================================================

proptest! {
    /// The sum of per-target segments_to_remove must equal total_segments_to_remove.
    #[test]
    fn proptest_plan_total_consistency(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..30),
        pane_tiers in prop::collection::vec(arb_tier(), 1..30),
    ) {
        let n = pane_segments.len().min(pane_tiers.len());
        let store = PropStore {
            segments: (0..n).map(|i| (i as u64, pane_segments[i])).collect(),
        };
        let tier_source = PropTierSource {
            tiers: (0..n).map(|i| (i as u64, pane_tiers[i])).collect(),
        };
        let evictor = ScrollbackEvictor::new(config, store, tier_source);

        let plan = evictor.plan(pressure).unwrap();
        let computed_total: usize = plan.targets.iter().map(|t| t.segments_to_remove).sum();

        prop_assert_eq!(
            plan.total_segments_to_remove, computed_total,
            "plan total {} != sum of targets {}",
            plan.total_segments_to_remove, computed_total
        );
    }
}

// =============================================================================
// 8. Unknown panes default to dormant
// =============================================================================

proptest! {
    /// Panes not in the tier source are treated as Dormant, which has the
    /// most aggressive eviction limits.
    #[test]
    fn proptest_unknown_panes_dormant(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        segments in 1usize..20_000,
    ) {
        let store = PropStore {
            segments: std::iter::once((42u64, segments)).collect(),
        };
        // Empty tier source — pane 42 is unknown
        let tier_source = PropTierSource {
            tiers: HashMap::new(),
        };
        let evictor = ScrollbackEvictor::new(config.clone(), store, tier_source);

        let plan = evictor.plan(pressure).unwrap();

        // Compute expected limit for Dormant tier
        let dormant_limit = config.max_segments_for(PaneTier::Dormant, pressure);

        if segments > dormant_limit {
            prop_assert_eq!(plan.panes_affected, 1);
            let target = &plan.targets[0];
            prop_assert_eq!(target.max_segments, dormant_limit);
            prop_assert_eq!(target.segments_to_remove, segments - dormant_limit);
        } else {
            prop_assert!(plan.is_empty(),
                "should not evict {} segments when limit is {}",
                segments, dormant_limit);
        }
    }
}

// =============================================================================
// 13. ImportanceScoringConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn proptest_importance_scoring_config_serde(config in arb_importance_scoring_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ImportanceScoringConfig = serde_json::from_str(&json).unwrap();
        let tol = 1e-10;
        prop_assert!((back.baseline - config.baseline).abs() < tol,
            "baseline mismatch: {} vs {}", back.baseline, config.baseline);
        prop_assert!((back.critical_bonus - config.critical_bonus).abs() < tol);
        prop_assert!((back.warning_bonus - config.warning_bonus).abs() < tol);
        prop_assert!((back.tool_boundary_bonus - config.tool_boundary_bonus).abs() < tol);
        prop_assert!((back.compilation_bonus - config.compilation_bonus).abs() < tol);
        prop_assert!((back.test_result_bonus - config.test_result_bonus).abs() < tol);
        prop_assert!((back.blank_line_penalty - config.blank_line_penalty).abs() < tol);
        prop_assert!((back.progress_line_penalty - config.progress_line_penalty).abs() < tol);
        prop_assert!((back.ansi_only_penalty - config.ansi_only_penalty).abs() < tol);
        prop_assert!((back.repeated_line_penalty - config.repeated_line_penalty).abs() < tol);
    }

    #[test]
    fn proptest_importance_scoring_config_default_roundtrip(_dummy in 0..1_u8) {
        let config = ImportanceScoringConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ImportanceScoringConfig = serde_json::from_str(&json).unwrap();
        let tol = 1e-10;
        prop_assert!((back.baseline - config.baseline).abs() < tol);
        prop_assert!((back.critical_bonus - config.critical_bonus).abs() < tol);
    }
}

// =============================================================================
// 14. ScrollbackLine serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn proptest_scrollback_line_serde(line in arb_scrollback_line()) {
        let json = serde_json::to_string(&line).unwrap();
        let back: ScrollbackLine = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.text, &line.text);
        prop_assert_eq!(back.bytes, line.bytes);
        prop_assert_eq!(back.timestamp_ms, line.timestamp_ms);
        let tol = 1e-10;
        prop_assert!((back.importance - line.importance).abs() < tol,
            "importance mismatch: {} vs {}", back.importance, line.importance);
    }

    /// ScrollbackLine::new clamps importance to [0.0, 1.0].
    #[test]
    fn proptest_scrollback_line_bytes_consistency(
        text in "[ -~]{0,80}",
        importance in -1.0_f64..2.0,
        ts in 0_u64..10_000_000_000,
    ) {
        let line = ScrollbackLine::new(text.clone(), importance, ts);
        prop_assert_eq!(line.bytes, text.len(), "bytes should equal text.len()");
        prop_assert!(line.importance >= 0.0, "importance clamped >= 0.0");
        prop_assert!(line.importance <= 1.0, "importance clamped <= 1.0");
    }
}

// =============================================================================
// 15. ImportanceBudgetReport serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn proptest_importance_budget_report_serde(report in arb_importance_budget_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: ImportanceBudgetReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.lines_removed, report.lines_removed);
        prop_assert_eq!(back.bytes_removed, report.bytes_removed);
        prop_assert_eq!(back.remaining_lines, report.remaining_lines);
        prop_assert_eq!(back.remaining_bytes, report.remaining_bytes);
    }

    #[test]
    fn proptest_importance_budget_report_default(_dummy in 0..1_u8) {
        let report = ImportanceBudgetReport::default();
        prop_assert_eq!(report.lines_removed, 0);
        prop_assert_eq!(report.bytes_removed, 0);
        prop_assert_eq!(report.remaining_lines, 0);
        prop_assert_eq!(report.remaining_bytes, 0);
        // Default also roundtrips
        let json = serde_json::to_string(&report).unwrap();
        let back: ImportanceBudgetReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.lines_removed, 0);
    }
}

// =============================================================================
// 16. EvictionTarget serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn proptest_eviction_target_serde(target in arb_eviction_target()) {
        let json = serde_json::to_string(&target).unwrap();
        let back: EvictionTarget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, target.pane_id);
        prop_assert_eq!(back.current_segments, target.current_segments);
        prop_assert_eq!(back.max_segments, target.max_segments);
        prop_assert_eq!(back.segments_to_remove, target.segments_to_remove);
    }
}

// =============================================================================
// 17. EvictionPlan serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn proptest_eviction_plan_serde(plan in arb_eviction_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let back: EvictionPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_segments_to_remove, plan.total_segments_to_remove);
        prop_assert_eq!(back.panes_affected, plan.panes_affected);
        prop_assert_eq!(back.targets.len(), plan.targets.len());
        for (a, b) in back.targets.iter().zip(plan.targets.iter()) {
            prop_assert_eq!(a.pane_id, b.pane_id);
            prop_assert_eq!(a.segments_to_remove, b.segments_to_remove);
        }
    }

    /// EvictionPlan::is_empty matches total_segments_to_remove == 0.
    #[test]
    fn proptest_eviction_plan_is_empty_consistency(plan in arb_eviction_plan()) {
        prop_assert_eq!(
            plan.is_empty(),
            plan.total_segments_to_remove == 0,
            "is_empty() should match total == 0"
        );
    }
}

// =============================================================================
// 18. EvictionReport serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn proptest_eviction_report_serde(report in arb_eviction_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: EvictionReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.panes_trimmed, report.panes_trimmed);
        prop_assert_eq!(back.segments_removed, report.segments_removed);
        prop_assert_eq!(back.errors.len(), report.errors.len());
    }

    #[test]
    fn proptest_eviction_report_default(_dummy in 0..1_u8) {
        let report = EvictionReport::default();
        prop_assert_eq!(report.panes_trimmed, 0);
        prop_assert_eq!(report.segments_removed, 0);
        prop_assert!(report.errors.is_empty());
    }
}

// =============================================================================
// 19. EvictionConfig default roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn proptest_eviction_config_default_roundtrip(_dummy in 0..1_u8) {
        let config = EvictionConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: EvictionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.active_max_segments, 10_000);
        prop_assert_eq!(back.thinking_max_segments, 5_000);
        prop_assert_eq!(back.idle_max_segments, 1_000);
        prop_assert_eq!(back.background_max_segments, 500);
        prop_assert_eq!(back.dormant_max_segments, 100);
        prop_assert_eq!(back.pressure_max_segments, 200);
        prop_assert_eq!(back.min_segments, 10);
    }
}

// =============================================================================
// 20. ImportanceRetentionConfig validate invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Valid configs produced by our strategy always pass validation.
    #[test]
    fn proptest_retention_config_validate_valid(config in arb_importance_retention_config()) {
        let result = config.validate();
        prop_assert!(result.is_ok(), "valid config should pass: {:?}", result);
    }

    /// min_lines == 0 is always rejected.
    #[test]
    fn proptest_retention_config_validate_zero_min(
        budget in 1_usize..10_000,
        max in 2_usize..500,
        threshold in 0.0_f64..1.0,
    ) {
        let config = ImportanceRetentionConfig {
            byte_budget_per_pane: budget,
            min_lines: 0,
            max_lines: max,
            importance_threshold: threshold,
            oldest_window_fraction: 0.5,
        };
        prop_assert!(config.validate().is_err(), "min_lines=0 should fail validation");
    }
}

// =============================================================================
// 21. ImportanceRetentionConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn proptest_retention_config_serde(config in arb_importance_retention_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ImportanceRetentionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.byte_budget_per_pane, config.byte_budget_per_pane);
        prop_assert_eq!(back.min_lines, config.min_lines);
        prop_assert_eq!(back.max_lines, config.max_lines);
        let tol = 1e-10;
        prop_assert!((back.importance_threshold - config.importance_threshold).abs() < tol);
        prop_assert!((back.oldest_window_fraction - config.oldest_window_fraction).abs() < tol);
    }
}

// =============================================================================
// 22. Empty store produces empty plan
// =============================================================================

proptest! {
    /// An evictor with no panes always produces an empty plan.
    #[test]
    fn proptest_empty_store_empty_plan(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
    ) {
        let store = PropStore::default();
        let tier_source = PropTierSource { tiers: HashMap::new() };
        let evictor = ScrollbackEvictor::new(config, store, tier_source);
        let plan = evictor.plan(pressure).unwrap();
        prop_assert!(plan.is_empty(), "empty store should produce empty plan");
        prop_assert_eq!(plan.panes_affected, 0);
        prop_assert!(plan.targets.is_empty());
    }
}
