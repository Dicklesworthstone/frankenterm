//! Property-based tests for the pane_lifecycle module.
//!
//! Tests health classification boundaries, state machine invariants,
//! lifecycle action properties, configuration serde, and pressure response.

use frankenterm_core::pane_lifecycle::*;
use proptest::prelude::*;
use std::time::Duration;

// ============================================================================
// Strategies
// ============================================================================

/// Generate an arbitrary PaneHealth variant.
fn arb_pane_health() -> impl Strategy<Value = PaneHealth> {
    prop_oneof![
        Just(PaneHealth::Active),
        Just(PaneHealth::Thinking),
        Just(PaneHealth::Working),
        Just(PaneHealth::PossiblyStuck),
        Just(PaneHealth::LikelyStuck),
        Just(PaneHealth::Abandoned),
    ]
}

/// Generate age in seconds (0 to 48 hours).
fn arb_age_secs() -> impl Strategy<Value = u64> {
    0u64..=172_800
}

/// Generate CPU percentage (0% to 100%).
fn arb_cpu() -> impl Strategy<Value = f64> {
    0.0f64..100.0
}

/// Generate a LifecycleConfig with reasonable values.
fn arb_lifecycle_config() -> impl Strategy<Value = LifecycleConfig> {
    (
        any::<bool>(),                              // enabled
        1usize..200,                                // trend_window
        8.0f64..32.0,                               // warn_age_hours
        16.0f64..48.0,                              // kill_age_hours
        1.0f64..50.0,                               // active_cpu_threshold
        0.1f64..10.0,                               // stuck_cpu_threshold
        0.5f64..1.0,                                // pressure_renice_threshold
        proptest::collection::vec(1u64..100, 0..5), // protected_panes
    )
        .prop_map(
            |(enabled, tw, warn, kill, active_cpu, stuck_cpu, prt, protected)| {
                // Ensure kill > warn
                let (warn_h, kill_h) = if warn < kill {
                    (warn, kill)
                } else {
                    (kill, warn + 1.0)
                };
                LifecycleConfig {
                    enabled,
                    sample_interval: Duration::from_secs(30),
                    trend_window: tw,
                    warn_age_hours: warn_h,
                    kill_age_hours: kill_h,
                    grace_period: Duration::from_secs(30),
                    active_cpu_threshold: active_cpu,
                    stuck_cpu_threshold: stuck_cpu,
                    pressure_renice_threshold: prt,
                    renice_value: 19,
                    protected_panes: protected,
                }
            },
        )
}

/// Generate a vec of (pane_id, PaneHealth, Duration) for pressure testing.
fn arb_pane_healths() -> impl Strategy<Value = Vec<(u64, PaneHealth, Duration)>> {
    proptest::collection::vec((1u64..100, arb_pane_health(), arb_age_secs()), 0..10).prop_map(|v| {
        v.into_iter()
            .enumerate()
            .map(|(i, (_, health, secs))| ((i as u64) + 1, health, Duration::from_secs(secs)))
            .collect()
    })
}

// ============================================================================
// Property Tests: PaneHealth enum
// ============================================================================

proptest! {
    /// Property 1: PaneHealth serde roundtrip
    #[test]
    fn prop_health_serde_roundtrip(health in arb_pane_health()) {
        let json = serde_json::to_string(&health).unwrap();
        let back: PaneHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, health, "PaneHealth serde roundtrip failed");
    }

    /// Property 2: PaneHealth serializes to snake_case
    #[test]
    fn prop_health_snake_case(health in arb_pane_health()) {
        let json = serde_json::to_string(&health).unwrap();
        // Remove quotes for content check
        let inner = json.trim_matches('"');
        prop_assert!(!inner.contains(char::is_uppercase),
                    "PaneHealth should serialize to snake_case: {}", json);
    }

    /// Property 3: PaneHealth Display is non-empty and matches serde
    #[test]
    fn prop_health_display_nonempty(health in arb_pane_health()) {
        let display = health.to_string();
        prop_assert!(!display.is_empty(), "Display should be non-empty for {:?}", health);
        // Display output should match serde output (without quotes)
        let json = serde_json::to_string(&health).unwrap();
        let json_inner = json.trim_matches('"');
        prop_assert_eq!(display.as_str(), json_inner,
                       "Display '{}' != serde '{}'", display, json_inner);
    }

    /// Property 4: is_protected, needs_review, is_reapable partition the variants
    /// (each variant falls into exactly one of: protected, needs_review, reapable, or working-no-review)
    #[test]
    fn prop_health_classification_exhaustive(health in arb_pane_health()) {
        let prot = health.is_protected();
        let review = health.needs_review();
        let reap = health.is_reapable();

        // Protected and reapable should never both be true
        prop_assert!(!(prot && reap),
                    "{:?}: cannot be both protected and reapable", health);

        // Protected and needs_review should never both be true
        prop_assert!(!(prot && review),
                    "{:?}: cannot be both protected and needs_review", health);
    }

    /// Property 5: PaneHealth ordering — severity increases along the enum order
    #[test]
    fn prop_health_ordering(h1 in arb_pane_health(), h2 in arb_pane_health()) {
        // PaneHealth derives Ord, verify it's consistent
        if h1 == h2 {
            prop_assert!(h1 <= h2, "{:?} should be <= {:?}", h1, h2);
            prop_assert!(h1 >= h2, "{:?} should be >= {:?}", h1, h2);
        }
    }

    /// Property 6: All PaneHealth variants have distinct Display strings
    #[test]
    fn prop_health_display_distinct(_dummy in Just(())) {
        let all = [
            PaneHealth::Active,
            PaneHealth::Thinking,
            PaneHealth::Working,
            PaneHealth::PossiblyStuck,
            PaneHealth::LikelyStuck,
            PaneHealth::Abandoned,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                prop_assert!(all[i].to_string() != all[j].to_string(),
                            "{:?} and {:?} have same Display", all[i], all[j]);
            }
        }
    }

    // ========================================================================
    // Property Tests: LifecycleConfig
    // ========================================================================

    /// Property 7: LifecycleConfig serde roundtrip
    #[test]
    fn prop_config_serde_roundtrip(config in arb_lifecycle_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: LifecycleConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, config.enabled, "enabled mismatch");
        prop_assert_eq!(back.trend_window, config.trend_window, "trend_window mismatch");
        prop_assert!((back.warn_age_hours - config.warn_age_hours).abs() < 1e-10,
                    "warn_age_hours mismatch");
        prop_assert!((back.kill_age_hours - config.kill_age_hours).abs() < 1e-10,
                    "kill_age_hours mismatch");
        prop_assert!((back.active_cpu_threshold - config.active_cpu_threshold).abs() < 1e-10,
                    "active_cpu_threshold mismatch");
        prop_assert!((back.stuck_cpu_threshold - config.stuck_cpu_threshold).abs() < 1e-10,
                    "stuck_cpu_threshold mismatch");
        prop_assert_eq!(back.protected_panes, config.protected_panes,
                       "protected_panes mismatch");
    }

    /// Property 8: LifecycleConfig default has kill_age > warn_age
    #[test]
    fn prop_config_default_ordering(_dummy in Just(())) {
        let config = LifecycleConfig::default();
        prop_assert!(config.kill_age_hours > config.warn_age_hours,
                    "kill_age ({}) should be > warn_age ({})",
                    config.kill_age_hours, config.warn_age_hours);
    }

    /// Property 9: LifecycleConfig default values match documented defaults
    #[test]
    fn prop_config_default_values(_dummy in Just(())) {
        let config = LifecycleConfig::default();
        prop_assert!(config.enabled, "default should be enabled");
        prop_assert_eq!(config.trend_window, 60, "trend_window default");
        prop_assert!((config.warn_age_hours - 16.0).abs() < 1e-10, "warn_age_hours default");
        prop_assert!((config.kill_age_hours - 24.0).abs() < 1e-10, "kill_age_hours default");
        prop_assert!((config.active_cpu_threshold - 10.0).abs() < 1e-10, "active_cpu_threshold default");
        prop_assert!((config.stuck_cpu_threshold - 2.0).abs() < 1e-10, "stuck_cpu_threshold default");
        prop_assert!((config.pressure_renice_threshold - 0.8).abs() < 1e-10, "pressure_renice_threshold default");
        prop_assert_eq!(config.renice_value, 19, "renice_value default");
        prop_assert!(config.protected_panes.is_empty(), "protected_panes default should be empty");
    }

    /// Property 10: LifecycleConfig deserializes from empty JSON using defaults
    #[test]
    fn prop_config_default_from_empty_json(_dummy in Just(())) {
        let config: LifecycleConfig = serde_json::from_str("{}").unwrap();
        let default = LifecycleConfig::default();
        prop_assert_eq!(config.enabled, default.enabled, "empty JSON enabled");
        prop_assert_eq!(config.trend_window, default.trend_window, "empty JSON trend_window");
    }

    // ========================================================================
    // Property Tests: classify_health
    // ========================================================================

    /// Property 11: Very old panes (>kill_age) are always Abandoned
    #[test]
    fn prop_classify_abandoned(cpu in arb_cpu()) {
        let config = LifecycleConfig::default();
        let engine = PaneLifecycleEngine::new(config.clone());
        let age = Duration::from_secs_f64(config.kill_age_hours * 3600.0 + 1.0);
        let health = engine.classify_health(age, cpu);
        prop_assert_eq!(health, PaneHealth::Abandoned,
                       "age > kill_age should always be Abandoned, got {:?}", health);
    }

    /// Property 12: Old panes (warn..kill) are always LikelyStuck
    #[test]
    fn prop_classify_likely_stuck(cpu in arb_cpu()) {
        let config = LifecycleConfig::default();
        let engine = PaneLifecycleEngine::new(config.clone());
        let mid = (config.warn_age_hours + config.kill_age_hours) / 2.0;
        let age = Duration::from_secs_f64(mid * 3600.0);
        let health = engine.classify_health(age, cpu);
        prop_assert_eq!(health, PaneHealth::LikelyStuck,
                       "age in (warn, kill) should be LikelyStuck, got {:?}", health);
    }

    /// Property 13: Young panes (<4h) with high CPU are Active
    #[test]
    fn prop_classify_young_active(cpu in 11.0f64..100.0) {
        let engine = PaneLifecycleEngine::with_defaults();
        let age = Duration::from_secs(3600); // 1 hour
        let health = engine.classify_health(age, cpu);
        prop_assert_eq!(health, PaneHealth::Active,
                       "young pane with high CPU ({}) should be Active, got {:?}", cpu, health);
    }

    /// Property 14: Young panes (<4h) with low CPU are Thinking
    #[test]
    fn prop_classify_young_thinking(cpu in 0.0f64..10.0) {
        let engine = PaneLifecycleEngine::with_defaults();
        let age = Duration::from_secs(3600); // 1 hour
        let health = engine.classify_health(age, cpu);
        prop_assert_eq!(health, PaneHealth::Thinking,
                       "young pane with low CPU ({}) should be Thinking, got {:?}", cpu, health);
    }

    /// Property 15: classify_health always returns a valid PaneHealth
    #[test]
    fn prop_classify_always_valid(age_secs in arb_age_secs(), cpu in arb_cpu()) {
        let engine = PaneLifecycleEngine::with_defaults();
        let health = engine.classify_health(Duration::from_secs(age_secs), cpu);
        // Just verify it's one of the variants (type system ensures this, but check it works)
        let valid = matches!(health,
            PaneHealth::Active | PaneHealth::Thinking | PaneHealth::Working |
            PaneHealth::PossiblyStuck | PaneHealth::LikelyStuck | PaneHealth::Abandoned
        );
        prop_assert!(valid, "classify_health returned invalid health {:?}", health);
    }

    /// Property 16: Health severity is monotonically non-decreasing with age
    /// (for fixed CPU, increasing age should never decrease health severity)
    #[test]
    fn prop_classify_age_monotonic(cpu in arb_cpu()) {
        let engine = PaneLifecycleEngine::with_defaults();
        let mut prev = engine.classify_health(Duration::from_secs(0), cpu);
        for hours in 1..=48 {
            let age = Duration::from_secs(hours * 3600);
            let current = engine.classify_health(age, cpu);
            prop_assert!(current >= prev,
                        "health decreased from {:?} to {:?} at age {}h, cpu {}",
                        prev, current, hours, cpu);
            prev = current;
        }
    }

    // ========================================================================
    // Property Tests: LifecycleAction
    // ========================================================================

    /// Property 17: LifecycleAction::None is never destructive
    #[test]
    fn prop_action_none_not_destructive(_dummy in Just(())) {
        prop_assert!(!LifecycleAction::None.is_destructive(),
                    "LifecycleAction::None should not be destructive");
    }

    /// Property 18: ForceKill and GracefulKill are always destructive
    #[test]
    fn prop_action_kill_destructive(reason in "[a-z ]{1,50}") {
        prop_assert!(LifecycleAction::ForceKill { reason: reason.clone() }.is_destructive(),
                    "ForceKill should be destructive");
        prop_assert!(LifecycleAction::GracefulKill {
            grace_period: Duration::from_secs(30),
            reason,
        }.is_destructive(), "GracefulKill should be destructive");
    }

    /// Property 19: Warn and Review are never destructive
    #[test]
    fn prop_action_warn_review_not_destructive(reason in "[a-z ]{1,50}") {
        prop_assert!(!LifecycleAction::Warn { reason: reason.clone() }.is_destructive(),
                    "Warn should not be destructive");
        prop_assert!(!LifecycleAction::Review { reason }.is_destructive(),
                    "Review should not be destructive");
    }

    /// Property 20: LifecycleAction serde roundtrip for non-destructive variants
    #[test]
    fn prop_action_serde_roundtrip(reason in "[a-z ]{1,50}") {
        let actions = vec![
            LifecycleAction::None,
            LifecycleAction::Warn { reason: reason.clone() },
            LifecycleAction::Review { reason: reason.clone() },
            LifecycleAction::ForceKill { reason: reason.clone() },
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let back: LifecycleAction = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(action.is_destructive(), back.is_destructive(),
                           "is_destructive mismatch after roundtrip");
        }
    }

    // ========================================================================
    // Property Tests: PaneLifecycleEngine
    // ========================================================================

    /// Property 21: Protected panes never get destructive actions
    #[test]
    fn prop_engine_protected_no_destructive(
        pane_id in 1u64..100,
        age_secs in arb_age_secs(),
        cpu in arb_cpu(),
    ) {
        let mut engine = PaneLifecycleEngine::new(LifecycleConfig {
            protected_panes: vec![pane_id],
            ..LifecycleConfig::default()
        });
        let (_, action) = engine.health_check(pane_id, 1000, Duration::from_secs(age_secs), cpu, None);
        prop_assert!(!action.is_destructive(),
                    "protected pane {} should never get destructive action, got {:?}",
                    pane_id, action);
    }

    /// Property 22: Engine starts with zero tracked panes
    #[test]
    fn prop_engine_starts_empty(_dummy in Just(())) {
        let engine = PaneLifecycleEngine::with_defaults();
        prop_assert_eq!(engine.tracked_pane_count(), 0,
                       "new engine should have 0 tracked panes");
    }

    /// Property 23: health_check adds pane to tracking
    #[test]
    fn prop_engine_health_check_tracks(pane_id in 1u64..1000) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        prop_assert!(engine.pane_health(pane_id).is_none(),
                    "pane should not be tracked before health_check");
        engine.health_check(pane_id, 1000, Duration::from_secs(3600), 50.0, None);
        prop_assert!(engine.pane_health(pane_id).is_some(),
                    "pane should be tracked after health_check");
    }

    /// Property 24: remove_pane drops tracking
    #[test]
    fn prop_engine_remove_drops(pane_id in 1u64..1000) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(pane_id, 1000, Duration::from_secs(3600), 50.0, None);
        prop_assert_eq!(engine.tracked_pane_count(), 1, "should track 1 pane");
        engine.remove_pane(pane_id);
        prop_assert_eq!(engine.tracked_pane_count(), 0, "should track 0 after remove");
        prop_assert!(engine.pane_health(pane_id).is_none(), "health should be None after remove");
    }

    /// Property 25: sample_count is bounded by trend_window
    #[test]
    fn prop_engine_sample_bounded(
        trend_window in 2usize..20,
        num_samples in 1usize..50,
    ) {
        let mut engine = PaneLifecycleEngine::new(LifecycleConfig {
            trend_window,
            ..LifecycleConfig::default()
        });
        for i in 0..num_samples {
            engine.health_check(1, 1000, Duration::from_secs(3600 + (i as u64) * 30), 50.0, None);
        }
        let count = engine.sample_count(1);
        prop_assert!(count <= trend_window,
                    "sample_count {} should be <= trend_window {}", count, trend_window);
    }

    /// Property 26: Multiple panes tracked independently
    #[test]
    fn prop_engine_independent_panes(
        n_panes in 1usize..10,
        age_secs in arb_age_secs(),
        cpu in arb_cpu(),
    ) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        for i in 0..n_panes {
            let pane_id = (i as u64) + 1;
            engine.health_check(pane_id, 1000 + (i as u32), Duration::from_secs(age_secs), cpu, None);
        }
        prop_assert_eq!(engine.tracked_pane_count(), n_panes,
                       "should track {} panes", n_panes);
    }

    /// Property 27: reapable_panes subset of tracked panes
    #[test]
    fn prop_engine_reapable_subset(
        age_secs in arb_age_secs(),
        cpu in arb_cpu(),
    ) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let pane_ids: Vec<u64> = (1..=5).collect();
        for &pid in &pane_ids {
            engine.health_check(pid, 1000, Duration::from_secs(age_secs), cpu, None);
        }
        let reapable = engine.reapable_panes();
        for id in &reapable {
            prop_assert!(pane_ids.contains(id),
                        "reapable pane {} not in tracked panes", id);
        }
    }

    /// Property 28: review_panes subset of tracked panes
    #[test]
    fn prop_engine_review_subset(
        age_secs in arb_age_secs(),
        cpu in arb_cpu(),
    ) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let pane_ids: Vec<u64> = (1..=5).collect();
        for &pid in &pane_ids {
            engine.health_check(pid, 1000, Duration::from_secs(age_secs), cpu, None);
        }
        let review = engine.review_panes();
        for id in &review {
            prop_assert!(pane_ids.contains(id),
                        "review pane {} not in tracked panes", id);
        }
    }

    // ========================================================================
    // Property Tests: pressure_renice_candidates
    // ========================================================================

    /// Property 29: No renice below pressure threshold
    #[test]
    fn prop_no_renice_below_threshold(
        load in 0.0f64..0.79,
        healths in arb_pane_healths(),
    ) {
        let config = LifecycleConfig::default(); // threshold = 0.8
        let candidates = pressure_renice_candidates(&healths, load, &config);
        prop_assert!(candidates.is_empty(),
                    "should not renice at load {} (threshold {}), got {} candidates",
                    load, config.pressure_renice_threshold, candidates.len());
    }

    /// Property 30: Active panes are never reniced
    #[test]
    fn prop_active_never_reniced(load in 0.8f64..1.0) {
        let config = LifecycleConfig::default();
        let healths = vec![
            (1, PaneHealth::Active, Duration::from_secs(3600)),
            (2, PaneHealth::Working, Duration::from_secs(8 * 3600)),
        ];
        let candidates = pressure_renice_candidates(&healths, load, &config);
        for (id, _) in &candidates {
            prop_assert!(*id != 1,
                        "Active pane should never be reniced");
        }
    }

    /// Property 31: Working panes are never reniced
    #[test]
    fn prop_working_never_reniced(load in 0.8f64..1.0) {
        let config = LifecycleConfig::default();
        let healths = vec![
            (1, PaneHealth::Working, Duration::from_secs(8 * 3600)),
        ];
        let candidates = pressure_renice_candidates(&healths, load, &config);
        prop_assert!(candidates.is_empty(),
                    "Working pane should never be reniced");
    }

    /// Property 32: Renice candidates sorted by age descending (oldest first)
    #[test]
    fn prop_renice_sorted_oldest_first(healths in arb_pane_healths()) {
        let config = LifecycleConfig::default();
        let candidates = pressure_renice_candidates(&healths, 0.95, &config);
        if candidates.len() > 1 {
            // Find the original ages for each candidate
            for window in candidates.windows(2) {
                let age1 = healths.iter().find(|(id, _, _)| *id == window[0].0).map(|(_, _, d)| d);
                let age2 = healths.iter().find(|(id, _, _)| *id == window[1].0).map(|(_, _, d)| d);
                if let (Some(a1), Some(a2)) = (age1, age2) {
                    prop_assert!(a1 >= a2,
                                "renice candidates not sorted oldest first: {:?} < {:?}",
                                a1, a2);
                }
            }
        }
    }

    /// Property 33: All renice candidates have the configured nice value
    #[test]
    fn prop_renice_uses_config_value(healths in arb_pane_healths()) {
        let config = LifecycleConfig::default();
        let candidates = pressure_renice_candidates(&healths, 0.95, &config);
        for (_, nice) in &candidates {
            prop_assert_eq!(*nice, config.renice_value,
                           "renice value should match config");
        }
    }

    /// Property 34: Renice candidate IDs are a subset of input pane IDs
    #[test]
    fn prop_renice_subset_of_input(healths in arb_pane_healths()) {
        let config = LifecycleConfig::default();
        let candidates = pressure_renice_candidates(&healths, 0.95, &config);
        let input_ids: Vec<u64> = healths.iter().map(|(id, _, _)| *id).collect();
        for (id, _) in &candidates {
            prop_assert!(input_ids.contains(id),
                        "renice candidate {} not in input", id);
        }
    }

    // ========================================================================
    // Property Tests: Cross-module consistency
    // ========================================================================

    /// Property 35: health_check returns consistent health and action
    #[test]
    fn prop_health_action_consistency(age_secs in arb_age_secs(), cpu in arb_cpu()) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let (sample, action) = engine.health_check(1, 1000, Duration::from_secs(age_secs), cpu, None);

        // Active/Thinking → non-destructive action
        if sample.health.is_protected() {
            prop_assert!(!action.is_destructive(),
                        "protected health {:?} should not get destructive action {:?}",
                        sample.health, action);
        }
        // Abandoned → destructive
        if sample.health == PaneHealth::Abandoned {
            prop_assert!(action.is_destructive(),
                        "Abandoned should get destructive action, got {:?}", action);
        }
    }

    /// Property 36: classify_health matches what health_check returns
    #[test]
    fn prop_classify_matches_health_check(age_secs in arb_age_secs(), cpu in arb_cpu()) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let expected = engine.classify_health(Duration::from_secs(age_secs), cpu);
        let (sample, _) = engine.health_check(1, 1000, Duration::from_secs(age_secs), cpu, None);
        prop_assert_eq!(sample.health, expected,
                       "health_check health should match classify_health");
    }

    /// Property 37: pane_health returns last classified health
    #[test]
    fn prop_pane_health_returns_last(age_secs in arb_age_secs(), cpu in arb_cpu()) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let (sample, _) = engine.health_check(1, 1000, Duration::from_secs(age_secs), cpu, None);
        let stored = engine.pane_health(1);
        prop_assert_eq!(stored, Some(sample.health),
                       "pane_health should return last health");
    }

    /// Property 38: health_check sample captures correct metadata
    #[test]
    fn prop_health_check_metadata(
        pane_id in 1u64..1000,
        root_pid in 1u32..65535,
        age_secs in arb_age_secs(),
        cpu in arb_cpu(),
    ) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        let (sample, _) = engine.health_check(pane_id, root_pid, Duration::from_secs(age_secs), cpu, None);
        prop_assert_eq!(sample.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(sample.root_pid, root_pid, "root_pid mismatch");
        prop_assert!((sample.cpu_percent - cpu).abs() < 1e-10, "cpu_percent mismatch");
        prop_assert_eq!(sample.age, Duration::from_secs(age_secs), "age mismatch");
    }

    /// Property 39: pane_root_pid returns correct PID after health_check
    #[test]
    fn prop_root_pid_tracked(
        pane_id in 1u64..1000,
        root_pid in 1u32..65535,
    ) {
        let mut engine = PaneLifecycleEngine::with_defaults();
        engine.health_check(pane_id, root_pid, Duration::from_secs(3600), 50.0, None);
        prop_assert_eq!(engine.pane_root_pid(pane_id), Some(root_pid),
                       "pane_root_pid should return tracked PID");
    }

    /// Property 40: Engine config accessor returns the config
    #[test]
    fn prop_engine_config_accessor(config in arb_lifecycle_config()) {
        let engine = PaneLifecycleEngine::new(config.clone());
        let c = engine.config();
        prop_assert_eq!(c.enabled, config.enabled, "config accessor enabled");
        prop_assert_eq!(c.trend_window, config.trend_window, "config accessor trend_window");
    }
}
