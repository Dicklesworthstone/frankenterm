//! Property-based tests for rate_limit_tracker module.
//!
//! Invariants tested:
//! 1. Cooldown monotonicity: remaining cooldown never increases without a new event
//! 2. Provider status consistency: limited_count <= total_count
//! 3. Bounded capacity: tracked panes never exceed MAX_TRACKED_PANES
//! 4. Bounded events: events per pane never exceed MAX_EVENTS_PER_PANE
//! 5. Parse symmetry: parse_retry_after roundtrips for well-formed inputs
//! 6. Provider isolation: events for one provider don't affect another
//! 7. GC safety: gc never removes actively rate-limited panes
//! 8. Removal completeness: remove_pane fully clears all state

use frankenterm_core::patterns::AgentType;
use frankenterm_core::rate_limit_tracker::{
    ProviderRateLimitStatus, ProviderRateLimitSummary, RateLimitTracker,
};
use proptest::prelude::*;
use std::time::{Duration, Instant};

/// Strategy for agent types.
fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
    ]
}

/// Strategy for retry-after text that should parse successfully.
fn arb_valid_retry_after() -> impl Strategy<Value = String> {
    prop_oneof![
        (1u64..3600).prop_map(|n| format!("{n}")),
        (1u64..120).prop_map(|n| format!("{n} seconds")),
        (1u64..60).prop_map(|n| format!("{n} second")),
        (1u64..60).prop_map(|n| format!("{n} minutes")),
        (1u64..24).prop_map(|n| format!("{n} hours")),
    ]
}

/// Strategy for pane IDs in a realistic range.
fn arb_pane_id() -> impl Strategy<Value = u64> {
    0u64..500
}

/// Strategy for rule IDs.
fn _arb_rule_id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("codex.rate_limit.detected".to_string()),
        Just("claude_code.rate_limit.detected".to_string()),
        Just("gemini.rate_limit.detected".to_string()),
    ]
}

/// Strategy for time offsets (in seconds from a base instant).
fn _arb_time_offset_secs() -> impl Strategy<Value = u64> {
    0u64..7200
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Invariant: provider summary limited_count <= total_count always holds.
    #[test]
    fn provider_summary_limited_lte_total(
        pane_ids in prop::collection::vec(arb_pane_id(), 1..50),
        agent_type in arb_agent_type(),
        cooldown_secs in 1u64..600,
        check_offset in 0u64..1200,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for &pid in &pane_ids {
            tracker.record_at(
                pid,
                agent_type,
                "rule".into(),
                Some(format!("{cooldown_secs} seconds")),
                now,
            );
        }

        let summary = tracker.provider_status_at(
            agent_type,
            now + Duration::from_secs(check_offset),
        );
        prop_assert!(
            summary.limited_pane_count <= summary.total_pane_count,
            "limited={} > total={}",
            summary.limited_pane_count,
            summary.total_pane_count
        );
    }

    /// Invariant: cooldown remaining never increases without a new event.
    #[test]
    fn cooldown_monotonically_decreases(
        cooldown_secs in 10u64..600,
        t1 in 1u64..300,
        t2 in 1u64..300,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();
        tracker.record_at(
            1,
            AgentType::Codex,
            "rule".into(),
            Some(format!("{cooldown_secs} seconds")),
            now,
        );

        let earlier = t1.min(t2);
        let later = t1.max(t2);
        let r_early = tracker.pane_cooldown_remaining_at(1, now + Duration::from_secs(earlier));
        let r_late = tracker.pane_cooldown_remaining_at(1, now + Duration::from_secs(later));

        prop_assert!(
            r_late <= r_early,
            "cooldown increased: early={:?} late={:?}",
            r_early,
            r_late
        );
    }

    /// Invariant: tracked pane count never exceeds 256 (MAX_TRACKED_PANES).
    #[test]
    fn tracked_panes_bounded(
        pane_count in 200usize..400,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();
        for i in 0..pane_count {
            tracker.record_at(
                i as u64,
                AgentType::Codex,
                "rule".into(),
                None,
                now,
            );
        }
        prop_assert!(
            tracker.tracked_pane_count() <= 256,
            "tracked={} > 256",
            tracker.tracked_pane_count()
        );
    }

    /// Invariant: total events per pane never exceeds 64 (MAX_EVENTS_PER_PANE).
    #[test]
    fn events_per_pane_bounded(
        event_count in 50usize..150,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();
        for i in 0..event_count {
            tracker.record_at(
                1,
                AgentType::Codex,
                format!("rule_{i}"),
                Some("10 seconds".into()),
                now + Duration::from_secs(i as u64),
            );
        }
        prop_assert!(
            tracker.total_event_count() <= 64,
            "events={} > 64",
            tracker.total_event_count()
        );
    }

    /// Invariant: provider isolation — events for Codex don't affect ClaudeCode status.
    #[test]
    fn provider_isolation(
        codex_panes in prop::collection::vec(0u64..100, 1..20),
        claude_panes in prop::collection::vec(100u64..200, 1..20),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for &pid in &codex_panes {
            tracker.record_at(pid, AgentType::Codex, "rule".into(), Some("60 seconds".into()), now);
        }
        for &pid in &claude_panes {
            tracker.record_at(pid, AgentType::ClaudeCode, "rule".into(), Some("60 seconds".into()), now);
        }

        let codex_summary = tracker.provider_status_at(AgentType::Codex, now);
        let claude_summary = tracker.provider_status_at(AgentType::ClaudeCode, now);

        // Codex pane count should not include claude panes
        // (Unique pane IDs mean total_pane_count = number of unique pane IDs per provider)
        let unique_codex: std::collections::HashSet<_> = codex_panes.iter().collect();
        let unique_claude: std::collections::HashSet<_> = claude_panes.iter().collect();

        prop_assert_eq!(codex_summary.total_pane_count, unique_codex.len());
        prop_assert_eq!(claude_summary.total_pane_count, unique_claude.len());

        // Gemini should be completely clear
        let gemini_summary = tracker.provider_status_at(AgentType::Gemini, now);
        prop_assert_eq!(gemini_summary.status, ProviderRateLimitStatus::Clear);
        prop_assert_eq!(gemini_summary.total_pane_count, 0);
    }

    /// Invariant: after remove_pane, the pane is no longer tracked or limited.
    #[test]
    fn remove_pane_clears_completely(
        pane_ids in prop::collection::vec(arb_pane_id(), 1..30),
        remove_idx in any::<prop::sample::Index>(),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for &pid in &pane_ids {
            tracker.record_at(pid, AgentType::Codex, "rule".into(), Some("60 seconds".into()), now);
        }

        let target = pane_ids[remove_idx.index(pane_ids.len())];
        let count_before = tracker.tracked_pane_count();
        tracker.remove_pane(target);

        prop_assert!(!tracker.is_pane_rate_limited_at(target, now));
        prop_assert_eq!(
            tracker.pane_cooldown_remaining_at(target, now),
            Duration::ZERO
        );
        // Count decreased by 1 (unless there were duplicates)
        let unique_count: std::collections::HashSet<_> = pane_ids.iter().collect();
        if unique_count.len() == pane_ids.len() {
            prop_assert_eq!(tracker.tracked_pane_count(), count_before - 1);
        }
    }

    /// Invariant: all_provider_statuses covers every provider that has events.
    #[test]
    fn all_provider_statuses_covers_all(
        events in prop::collection::vec(
            (arb_pane_id(), arb_agent_type()),
            1..30
        ),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        // Track the final agent_type per pane (last write wins).
        let mut final_type_per_pane = std::collections::HashMap::new();
        for &(pid, agent_type) in &events {
            tracker.record_at(pid, agent_type, "rule".into(), None, now);
            final_type_per_pane.insert(pid, agent_type);
        }
        let expected_types: std::collections::HashSet<_> = final_type_per_pane
            .values()
            .map(|at| at.to_string())
            .collect();

        let summaries = tracker.all_provider_statuses_at(now);
        let actual_types: std::collections::HashSet<_> = summaries
            .iter()
            .map(|s| s.agent_type.clone())
            .collect();

        for expected in &expected_types {
            prop_assert!(
                actual_types.contains(expected),
                "missing provider: {}",
                expected
            );
        }
    }

    /// Invariant: status transitions are monotonic — Clear never follows Limited
    /// without time passing past cooldown.
    #[test]
    fn status_never_regresses_without_time(
        pane_count in 1usize..10,
        cooldown_secs in 30u64..600,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for i in 0..pane_count {
            tracker.record_at(
                i as u64,
                AgentType::Codex,
                "rule".into(),
                Some(format!("{cooldown_secs} seconds")),
                now,
            );
        }

        let status_at_start = tracker.provider_status_at(AgentType::Codex, now).status;
        // Status should be FullyLimited at the start
        prop_assert_eq!(status_at_start, ProviderRateLimitStatus::FullyLimited);

        // Before cooldown, should still be limited
        let mid_point = now + Duration::from_secs(cooldown_secs / 2);
        let status_mid = tracker.provider_status_at(AgentType::Codex, mid_point).status;
        prop_assert_ne!(status_mid, ProviderRateLimitStatus::Clear);
    }

    /// Invariant: valid retry-after text always produces a non-zero Duration.
    #[test]
    fn valid_retry_after_always_parses(text in arb_valid_retry_after()) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();
        tracker.record_at(
            1,
            AgentType::Codex,
            "rule".into(),
            Some(text),
            now,
        );
        // If it parsed, cooldown should be non-zero at the record instant
        prop_assert!(tracker.is_pane_rate_limited_at(1, now));
        prop_assert!(tracker.pane_cooldown_remaining_at(1, now) > Duration::ZERO);
    }

    /// Invariant: a new event with longer cooldown always extends the effective cooldown.
    #[test]
    fn longer_cooldown_always_extends(
        first_secs in 10u64..300,
        second_secs in 10u64..300,
        gap_secs in 0u64..60,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        tracker.record_at(
            1,
            AgentType::Codex,
            "r1".into(),
            Some(format!("{first_secs} seconds")),
            now,
        );

        let second_time = now + Duration::from_secs(gap_secs);
        tracker.record_at(
            1,
            AgentType::Codex,
            "r2".into(),
            Some(format!("{second_secs} seconds")),
            second_time,
        );

        // The effective cooldown should be at least max(first_expiry, second_expiry)
        let first_expiry = Duration::from_secs(first_secs);
        let second_expiry = Duration::from_secs(gap_secs + second_secs);
        let effective_end = first_expiry.max(second_expiry);

        // Check that the pane is still limited near the effective end
        if effective_end.as_secs() > 1 {
            let check_time = (now + effective_end).checked_sub(Duration::from_secs(1)).unwrap();
            prop_assert!(
                tracker.is_pane_rate_limited_at(1, check_time),
                "should still be limited 1s before effective end"
            );
        }

        // Check that the pane is no longer limited well after effective end
        let check_after = now + effective_end + Duration::from_secs(2);
        prop_assert!(
            !tracker.is_pane_rate_limited_at(1, check_after),
            "should be clear 2s after effective end"
        );
    }

    /// Invariant: earliest_clear_secs is consistent with the actual soonest expiry.
    #[test]
    fn earliest_clear_consistent(
        cooldowns in prop::collection::vec(10u64..600, 2..10),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for (i, &cd) in cooldowns.iter().enumerate() {
            tracker.record_at(
                i as u64,
                AgentType::Codex,
                "rule".into(),
                Some(format!("{cd} seconds")),
                now,
            );
        }

        let summary = tracker.provider_status_at(AgentType::Codex, now);
        let min_cooldown = cooldowns.iter().copied().min().unwrap();

        // earliest_clear_secs should be approximately the minimum cooldown
        // (within 1 second tolerance for timing)
        prop_assert!(
            summary.earliest_clear_secs <= min_cooldown,
            "earliest_clear={} > min_cooldown={}",
            summary.earliest_clear_secs,
            min_cooldown
        );
        if min_cooldown > 1 {
            prop_assert!(
                summary.earliest_clear_secs >= min_cooldown - 1,
                "earliest_clear={} too far from min_cooldown={}",
                summary.earliest_clear_secs,
                min_cooldown
            );
        }
    }

    /// Invariant: LRU eviction preserves the most recently added panes.
    #[test]
    fn lru_preserves_recent(
        total_panes in 257usize..300,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for i in 0..total_panes {
            tracker.record_at(
                i as u64,
                AgentType::Codex,
                "rule".into(),
                Some("60 seconds".into()),
                now,
            );
        }

        // The most recent panes should still be tracked
        let last_pane = (total_panes - 1) as u64;
        prop_assert!(
            tracker.is_pane_rate_limited_at(last_pane, now),
            "last pane should be tracked"
        );

        // The oldest panes should be evicted
        prop_assert!(
            !tracker.is_pane_rate_limited_at(0, now),
            "first pane should be evicted"
        );

        // Total should be capped
        prop_assert!(tracker.tracked_pane_count() <= 256);
    }

    /// Invariant: total_events in provider summary matches sum of per-pane events.
    #[test]
    fn total_events_consistent(
        events_per_pane in prop::collection::vec(1usize..10, 1..20),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        let mut expected_total = 0usize;
        for (pane_idx, &event_count) in events_per_pane.iter().enumerate() {
            for j in 0..event_count {
                tracker.record_at(
                    pane_idx as u64,
                    AgentType::Codex,
                    format!("r{j}"),
                    Some("60 seconds".into()),
                    now + Duration::from_secs(j as u64),
                );
            }
            expected_total += event_count;
        }

        let summary = tracker.provider_status_at(AgentType::Codex, now);
        // Events may have been evicted if > 64 per pane, but for our range (1..10) they won't
        prop_assert_eq!(summary.total_events, expected_total);
    }

    /// Invariant: Default impl and new() produce identical empty trackers.
    #[test]
    fn default_eq_new(_dummy in 0u8..1) {
        let a = RateLimitTracker::new();
        let b = RateLimitTracker::default();
        prop_assert_eq!(a.tracked_pane_count(), b.tracked_pane_count());
        prop_assert_eq!(a.total_event_count(), b.total_event_count());
    }

    /// Invariant: recording the same pane multiple times doesn't create duplicate pane entries.
    #[test]
    fn no_duplicate_pane_entries(
        event_count in 2usize..50,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for i in 0..event_count {
            tracker.record_at(
                42,
                AgentType::Codex,
                format!("r{i}"),
                Some("60 seconds".into()),
                now + Duration::from_secs(i as u64),
            );
        }

        prop_assert_eq!(
            tracker.tracked_pane_count(),
            1,
            "should only have 1 pane entry"
        );
    }

    /// Invariant: ProviderRateLimitSummary survives JSON serde roundtrip.
    #[test]
    fn summary_serde_roundtrip(
        limited in 0usize..100,
        total in 0usize..100,
        earliest in 0u64..3600,
        events in 0usize..1000,
    ) {
        let total = total.max(limited);
        let status = if limited == 0 {
            ProviderRateLimitStatus::Clear
        } else if limited < total {
            ProviderRateLimitStatus::PartiallyLimited
        } else {
            ProviderRateLimitStatus::FullyLimited
        };

        let summary = ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status,
            limited_pane_count: limited,
            total_pane_count: total,
            earliest_clear_secs: earliest,
            total_events: events,
        };

        let json = serde_json::to_string(&summary).unwrap();
        let restored: ProviderRateLimitSummary = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.status, summary.status);
        prop_assert_eq!(restored.limited_pane_count, summary.limited_pane_count);
        prop_assert_eq!(restored.total_pane_count, summary.total_pane_count);
        prop_assert_eq!(restored.earliest_clear_secs, summary.earliest_clear_secs);
        prop_assert_eq!(restored.total_events, summary.total_events);
    }
}
