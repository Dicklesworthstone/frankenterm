//! Property-based tests for `cooldown_tracker` — time-based deduplication with expiry.

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::cooldown_tracker::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_config() -> impl Strategy<Value = CooldownConfig> {
    (1..600u64, 10..10_000usize).prop_map(|(secs, max)| CooldownConfig {
        default_cooldown: Duration::from_secs(secs),
        max_entries: max,
    })
}

fn arb_key() -> impl Strategy<Value = String> {
    "[a-z]{1,20}"
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. First check for any key is always Allowed
    #[test]
    fn first_check_always_allowed(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        let outcome = tracker.check(&key);
        prop_assert!(outcome.is_allowed());
    }

    // 2. Second immediate check is always Suppressed
    #[test]
    fn second_check_always_suppressed(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key);
        let outcome = tracker.check(&key);
        prop_assert!(outcome.is_suppressed());
    }

    // 3. Different keys are independently allowed
    #[test]
    fn different_keys_independent(k1 in arb_key(), k2 in arb_key()) {
        prop_assume!(k1 != k2);
        let mut tracker = CooldownTracker::<String>::new();
        let o1 = tracker.check(&k1);
        let o2 = tracker.check(&k2);
        prop_assert!(o1.is_allowed());
        prop_assert!(o2.is_allowed());
    }

    // 4. len tracks unique keys
    #[test]
    fn len_tracks_unique_keys(keys in proptest::collection::vec(arb_key(), 1..20)) {
        let mut tracker = CooldownTracker::<String>::new();
        for k in &keys {
            tracker.check(k);
        }
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        prop_assert_eq!(tracker.len(), unique.len());
    }

    // 5. is_empty consistent with len
    #[test]
    fn is_empty_consistent(count in 0..10usize) {
        let mut tracker = CooldownTracker::<u32>::new();
        for i in 0..count {
            tracker.check(&(i as u32));
        }
        prop_assert_eq!(tracker.is_empty(), count == 0);
    }

    // 6. clear_all empties the tracker
    #[test]
    fn clear_all_empties(keys in proptest::collection::vec(arb_key(), 1..10)) {
        let mut tracker = CooldownTracker::<String>::new();
        for k in &keys {
            tracker.check(k);
        }
        tracker.clear_all();
        prop_assert!(tracker.is_empty());
    }

    // 7. clear_key returns true for tracked keys
    #[test]
    fn clear_key_returns_true(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key);
        prop_assert!(tracker.clear_key(&key));
    }

    // 8. clear_key returns false for untracked keys
    #[test]
    fn clear_key_returns_false(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        prop_assert!(!tracker.clear_key(&key));
    }

    // 9. After clear_key, next check is Allowed
    #[test]
    fn after_clear_key_allowed(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key);
        tracker.clear_key(&key);
        prop_assert!(tracker.check(&key).is_allowed());
    }

    // 10. suppressed_count starts at 0
    #[test]
    fn suppressed_count_starts_zero(key in arb_key()) {
        let tracker = CooldownTracker::<String>::new();
        prop_assert_eq!(tracker.suppressed_count(&key), 0);
    }

    // 11. suppressed_count increments correctly
    #[test]
    fn suppressed_count_increments(key in arb_key(), extra in 1..10u32) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key); // allowed
        for _ in 0..extra {
            tracker.check(&key); // suppressed
        }
        prop_assert_eq!(tracker.suppressed_count(&key), extra as u64);
    }

    // 12. total_count = 1 + suppressed_count
    #[test]
    fn total_count_equals_one_plus_suppressed(key in arb_key(), extra in 0..10u32) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key);
        for _ in 0..extra {
            tracker.check(&key);
        }
        let total = tracker.total_count(&key);
        let suppressed = tracker.suppressed_count(&key);
        prop_assert_eq!(total, 1 + suppressed);
    }

    // 13. total_count for untracked is 0
    #[test]
    fn total_count_untracked_zero(key in arb_key()) {
        let tracker = CooldownTracker::<String>::new();
        prop_assert_eq!(tracker.total_count(&key), 0);
    }

    // 14. stats.total_allowed + stats.total_suppressed = total checks
    #[test]
    fn stats_totals_consistent(keys in proptest::collection::vec(arb_key(), 1..10)) {
        let mut tracker = CooldownTracker::<String>::new();
        let total_checks = keys.len();
        for k in &keys {
            tracker.check(k);
        }
        let stats = tracker.stats();
        let stat_total = stats.total_allowed + stats.total_suppressed;
        prop_assert_eq!(stat_total, total_checks as u64);
    }

    // 15. stats.tracked_entries matches len()
    #[test]
    fn stats_tracked_matches_len(keys in proptest::collection::vec(arb_key(), 1..15)) {
        let mut tracker = CooldownTracker::<String>::new();
        for k in &keys {
            tracker.check(k);
        }
        let stats = tracker.stats();
        prop_assert_eq!(stats.tracked_entries, tracker.len());
    }

    // 16. CooldownOutcome::Allowed helper methods
    #[test]
    fn allowed_helper_methods(_dummy in 0..1u8) {
        let outcome = CooldownOutcome::Allowed;
        prop_assert!(outcome.is_allowed());
        prop_assert!(!outcome.is_suppressed());
    }

    // 17. CooldownOutcome::Suppressed helper methods
    #[test]
    fn suppressed_helper_methods(secs in 1..100u64, count in 1..50u64) {
        let outcome = CooldownOutcome::Suppressed {
            remaining: Duration::from_secs(secs),
            suppressed_count: count,
        };
        prop_assert!(!outcome.is_allowed());
        prop_assert!(outcome.is_suppressed());
    }

    // 18. CooldownConfig default has sensible values
    #[test]
    fn config_default_sensible(_dummy in 0..1u8) {
        let config = CooldownConfig::default();
        prop_assert!(config.default_cooldown > Duration::ZERO);
        prop_assert!(config.max_entries > 0);
    }

    // 19. Custom config is preserved by tracker
    #[test]
    fn custom_config_preserved(config in arb_config()) {
        let cooldown = config.default_cooldown;
        let max = config.max_entries;
        let tracker = CooldownTracker::<String>::with_config(config);
        prop_assert_eq!(tracker.config().default_cooldown, cooldown);
        prop_assert_eq!(tracker.config().max_entries, max);
    }

    // 20. max_entries eviction keeps tracker bounded
    #[test]
    fn max_entries_bounded(max in 3..20usize) {
        let config = CooldownConfig {
            default_cooldown: Duration::from_secs(300),
            max_entries: max,
        };
        let mut tracker = CooldownTracker::<u32>::with_config(config);
        for i in 0..(max as u32 + 10) {
            tracker.check(&i);
        }
        prop_assert!(tracker.len() <= max);
    }

    // 21. Zero-duration cooldown: expired immediately
    #[test]
    fn zero_cooldown_expires_immediately(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        let cooldown = Duration::from_millis(0);
        tracker.check_with_cooldown(&key, cooldown);
        std::thread::sleep(Duration::from_millis(1));
        let outcome = tracker.check_with_cooldown(&key, cooldown);
        prop_assert!(outcome.is_allowed());
    }

    // 22. is_in_cooldown true after check
    #[test]
    fn in_cooldown_after_check(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key);
        prop_assert!(tracker.is_in_cooldown(&key));
    }

    // 23. is_in_cooldown false for untracked
    #[test]
    fn not_in_cooldown_untracked(key in arb_key()) {
        let tracker = CooldownTracker::<String>::new();
        prop_assert!(!tracker.is_in_cooldown(&key));
    }

    // 24. remaining is Some after check
    #[test]
    fn remaining_some_after_check(key in arb_key()) {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&key);
        prop_assert!(tracker.remaining(&key).is_some());
    }

    // 25. remaining is None for untracked
    #[test]
    fn remaining_none_untracked(key in arb_key()) {
        let tracker = CooldownTracker::<String>::new();
        prop_assert!(tracker.remaining(&key).is_none());
    }

    // 26. remaining <= default_cooldown
    #[test]
    fn remaining_bounded_by_cooldown(key in arb_key(), secs in 1..600u64) {
        let config = CooldownConfig {
            default_cooldown: Duration::from_secs(secs),
            max_entries: 1000,
        };
        let mut tracker = CooldownTracker::<String>::with_config(config);
        tracker.check(&key);
        let rem = tracker.remaining(&key).unwrap();
        prop_assert!(rem <= Duration::from_secs(secs));
    }

    // 27. CooldownOutcome equality
    #[test]
    fn outcome_equality(_dummy in 0..1u8) {
        let a1 = CooldownOutcome::Allowed;
        let a2 = CooldownOutcome::Allowed;
        prop_assert_eq!(a1, a2);
    }

    // 28. stats starts at zero
    #[test]
    fn stats_initial_zero(_dummy in 0..1u8) {
        let tracker = CooldownTracker::<String>::new();
        let stats = tracker.stats();
        prop_assert_eq!(stats.tracked_entries, 0);
        prop_assert_eq!(stats.active_cooldowns, 0);
        prop_assert_eq!(stats.total_suppressed, 0);
        prop_assert_eq!(stats.total_allowed, 0);
    }

    // 29. integer keys work
    #[test]
    fn integer_keys_work(key in 0..1000u64) {
        let mut tracker = CooldownTracker::<u64>::new();
        prop_assert!(tracker.check(&key).is_allowed());
        prop_assert!(tracker.check(&key).is_suppressed());
    }

    // 30. CooldownStats clone equality
    #[test]
    fn stats_clone_eq(entries in 0..100usize, active in 0..50usize, sup in 0..1000u64, allow in 0..1000u64) {
        let s = CooldownStats {
            tracked_entries: entries,
            active_cooldowns: active,
            total_suppressed: sup,
            total_allowed: allow,
        };
        let cloned = s.clone();
        prop_assert_eq!(s, cloned);
    }
}
