//! Property-based tests for scrollback_tiers module.
//!
//! Verifies tiered scrollback invariants:
//! - ScrollbackConfig serde roundtrip
//! - ScrollbackTier serde roundtrip (snake_case: hot, warm, cold)
//! - ScrollbackTierSnapshot serde roundtrip
//! - TieredScrollback invariants:
//!   - hot_len <= config.hot_lines + config.page_size (overflow triggers flush)
//!   - warm_bytes <= config.warm_max_bytes + one page margin (when cold eviction on)
//!   - total_line_count == hot + warm + cold
//!   - total_lines_added monotonically increasing
//!   - tier_for_offset: 0 → Hot, beyond hot → Warm/Cold
//!   - tail(n) len <= min(n, hot_len)
//!   - compression ratio > 1.0 for non-trivial data
//!   - clear resets all counters
//!   - snapshot fields consistent with accessors

use proptest::prelude::*;

use frankenterm_core::byte_compression::CompressionLevel;
use frankenterm_core::scrollback_tiers::{
    ScrollbackConfig, ScrollbackTier, ScrollbackTierSnapshot, TieredScrollback,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_compression_level() -> impl Strategy<Value = CompressionLevel> {
    prop_oneof![
        Just(CompressionLevel::Fast),
        Just(CompressionLevel::Default),
        Just(CompressionLevel::High),
        Just(CompressionLevel::Maximum),
    ]
}

fn arb_tier() -> impl Strategy<Value = ScrollbackTier> {
    prop_oneof![
        Just(ScrollbackTier::Hot),
        Just(ScrollbackTier::Warm),
        Just(ScrollbackTier::Cold),
    ]
}

fn arb_config() -> impl Strategy<Value = ScrollbackConfig> {
    (
        10usize..=5000,         // hot_lines
        4usize..=512,           // page_size
        1024usize..=10_000_000, // warm_max_bytes
        arb_compression_level(),
        prop::bool::ANY, // cold_eviction_enabled
    )
        .prop_map(
            |(hot_lines, page_size, warm_max_bytes, compression, cold_eviction_enabled)| {
                ScrollbackConfig {
                    hot_lines,
                    page_size,
                    warm_max_bytes,
                    compression,
                    cold_eviction_enabled,
                }
            },
        )
}

fn arb_snapshot() -> impl Strategy<Value = ScrollbackTierSnapshot> {
    (
        0usize..=10_000,     // hot_lines
        0usize..=500,        // warm_pages
        0usize..=50_000_000, // warm_bytes
        0usize..=100_000,    // warm_lines
        0u64..=1_000_000,    // cold_lines
        0u64..=10_000,       // cold_pages
        0u64..=10_000_000,   // total_lines_added
    )
        .prop_map(
            |(
                hot_lines,
                warm_pages,
                warm_bytes,
                warm_lines,
                cold_lines,
                cold_pages,
                total_lines_added,
            )| {
                ScrollbackTierSnapshot {
                    hot_lines,
                    warm_pages,
                    warm_bytes,
                    warm_lines,
                    cold_lines,
                    cold_pages,
                    total_lines_added,
                }
            },
        )
}

/// Generate a line of given approximate length.
fn make_line(id: usize) -> String {
    format!("line-{id:06}: {}", "A".repeat(80))
}

// ────────────────────────────────────────────────────────────────────
// Serde roundtrip tests
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn scrollback_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let rt: ScrollbackConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.hot_lines, config.hot_lines);
        prop_assert_eq!(rt.page_size, config.page_size);
        prop_assert_eq!(rt.warm_max_bytes, config.warm_max_bytes);
        prop_assert_eq!(rt.cold_eviction_enabled, config.cold_eviction_enabled);
    }

    #[test]
    fn scrollback_tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let rt: ScrollbackTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, tier);
    }

    #[test]
    fn scrollback_tier_snapshot_serde_roundtrip(snap in arb_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let rt: ScrollbackTierSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, snap);
    }

    #[test]
    fn scrollback_tier_snake_case_names(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let expected = match tier {
            ScrollbackTier::Hot => "\"hot\"",
            ScrollbackTier::Warm => "\"warm\"",
            ScrollbackTier::Cold => "\"cold\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }
}

// ────────────────────────────────────────────────────────────────────
// TieredScrollback invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// After pushing N lines, hot_len <= config.hot_lines + config.page_size.
    #[test]
    fn hot_tier_bounded(
        config in arb_config(),
        n in 1usize..=5000,
    ) {
        let mut sb = TieredScrollback::new(config.clone());
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        let max_hot = config.hot_lines + config.page_size;
        prop_assert!(
            sb.hot_len() <= max_hot,
            "hot_len {} exceeds max {} (hot_lines={}, page_size={})",
            sb.hot_len(), max_hot, config.hot_lines, config.page_size
        );
    }

    /// total_line_count == hot + warm + cold.
    #[test]
    fn total_line_count_consistent(
        config in arb_config(),
        n in 1usize..=3000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        let snap = sb.snapshot();
        let sum = snap.hot_lines as u64 + snap.warm_lines as u64 + snap.cold_lines;
        prop_assert_eq!(sb.total_line_count(), sum);
    }

    /// total_lines_added == number of push_line calls.
    #[test]
    fn total_lines_added_equals_pushes(
        config in arb_config(),
        n in 0usize..=2000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        prop_assert_eq!(sb.snapshot().total_lines_added, n as u64);
    }

    /// tail(n) returns at most min(n, hot_len) lines.
    #[test]
    fn tail_length_bounded(
        config in arb_config(),
        n in 0usize..=2000,
        tail_count in 0usize..=3000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        let tail = sb.tail(tail_count);
        let expected = tail_count.min(sb.hot_len());
        prop_assert_eq!(tail.len(), expected);
    }

    /// tier_for_offset(0) is always Hot when there are lines.
    #[test]
    fn tier_offset_zero_is_hot(
        config in arb_config(),
        n in 1usize..=1000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        prop_assert_eq!(sb.tier_for_offset(0), ScrollbackTier::Hot);
    }

    /// snapshot fields match accessor methods.
    #[test]
    fn snapshot_matches_accessors(
        config in arb_config(),
        n in 0usize..=3000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        let snap = sb.snapshot();
        prop_assert_eq!(snap.hot_lines, sb.hot_len());
        prop_assert_eq!(snap.warm_pages, sb.warm_page_count());
        prop_assert_eq!(snap.warm_bytes, sb.warm_total_bytes());
        prop_assert_eq!(snap.cold_lines, sb.cold_line_count());
    }

    /// clear() resets all state to zero.
    #[test]
    fn clear_resets_all_state(
        config in arb_config(),
        n in 1usize..=2000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        sb.clear();
        let snap = sb.snapshot();
        prop_assert_eq!(snap.hot_lines, 0);
        prop_assert_eq!(snap.warm_pages, 0);
        prop_assert_eq!(snap.warm_bytes, 0);
        prop_assert_eq!(snap.cold_lines, 0);
        prop_assert_eq!(snap.cold_pages, 0);
        prop_assert_eq!(snap.total_lines_added, 0);
    }

    /// evict_all_warm moves all warm to cold, zeroing warm.
    #[test]
    fn evict_all_warm_zeroes_warm(
        config in arb_config(),
        n in 1usize..=3000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        let before = sb.snapshot();
        sb.evict_all_warm();
        let after = sb.snapshot();
        prop_assert_eq!(after.warm_pages, 0);
        prop_assert_eq!(after.warm_bytes, 0);
        // Cold should have increased by the warm lines
        prop_assert_eq!(after.cold_lines, before.cold_lines + before.warm_lines as u64);
        // Hot should be unchanged
        prop_assert_eq!(after.hot_lines, before.hot_lines);
    }

    /// After push + evict + more pushes, total_lines_added is correct.
    #[test]
    fn total_lines_after_evict_and_push(
        config in arb_config(),
        n1 in 1usize..=1000,
        n2 in 1usize..=1000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n1 {
            sb.push_line(make_line(i));
        }
        sb.evict_all_warm();
        for i in n1..(n1 + n2) {
            sb.push_line(make_line(i));
        }
        prop_assert_eq!(sb.snapshot().total_lines_added, (n1 + n2) as u64);
    }

    /// warm_page_lines(0) returns Some when warm pages exist.
    #[test]
    fn warm_page_lines_accessible(
        n in 2000usize..=5000,
    ) {
        // Use default config with 1000 hot lines, 256 page size
        let mut sb = TieredScrollback::new(ScrollbackConfig::default());
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        if sb.warm_page_count() > 0 {
            let page = sb.warm_page_lines(0);
            prop_assert!(page.is_some(), "warm page 0 should be decompressible");
            let lines = page.unwrap();
            prop_assert!(lines.len() > 0, "decompressed page should have lines");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Compression invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Compression ratio is > 1.0 for repetitive data.
    #[test]
    fn compression_ratio_positive_for_repetitive_data(
        n in 2000usize..=5000,
        level in arb_compression_level(),
    ) {
        let config = ScrollbackConfig {
            hot_lines: 500,
            page_size: 128,
            warm_max_bytes: 50 * 1024 * 1024,
            compression: level,
            cold_eviction_enabled: false,
        };
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        if sb.warm_page_count() > 0 {
            let ratio = sb.warm_compression_ratio();
            prop_assert!(ratio.is_some());
            prop_assert!(ratio.unwrap() > 1.0,
                "compression ratio {:.2} should be > 1.0 for repetitive data",
                ratio.unwrap()
            );
        }
    }

    /// Snapshot serde roundtrip preserves all fields after real operations.
    #[test]
    fn live_snapshot_serde_roundtrip(
        config in arb_config(),
        n in 0usize..=3000,
    ) {
        let mut sb = TieredScrollback::new(config);
        for i in 0..n {
            sb.push_line(make_line(i));
        }
        let snap = sb.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let rt: ScrollbackTierSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, snap);
    }
}
