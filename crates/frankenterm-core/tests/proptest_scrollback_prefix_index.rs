//! Q1 (round-4 Alien Optimization Gauntlet): byte-equivalence proof for the
//! seqlock warm-tier prefix-sum fast path in `scrollback_tiers`.
//!
//! Proves the gated, incrementally-maintained cumulative line-count prefix +
//! binary-search resolution of `locate_offset` / `tier_for_offset` is
//! byte-identical to the deterministic legacy linear walk, over random
//! push/evict histories plus an exhaustive + 10k-offset sweep. The `indexed`
//! arm runs with `scrollback.prefix_index` ON; the `linear` arm is the same op
//! stream with it OFF (legacy path). Identical op streams ⇒ identical tier
//! structure, so any `ScrollbackLocationHint` / `ScrollbackTier` divergence is
//! the index's fault.
//!
//! This is an integration test (it links the lib in *normal* mode, so the
//! library's `#[cfg(test)]` unit modules are not compiled), which keeps the
//! proof isolated from unrelated sibling `#[cfg(test)]` churn elsewhere in
//! `frankenterm-core` during the round-4 campaign.

use frankenterm_core::byte_compression::CompressionLevel;
use frankenterm_core::scrollback_tiers::{ScrollbackConfig, TieredScrollback};

/// Deterministic xorshift64 PRNG (no external dep; reproducible across runs).
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Tight config: small hot tier + tiny warm cap force a real warm + cold
/// structure so all three resolution branches are exercised.
fn cfg() -> ScrollbackConfig {
    ScrollbackConfig {
        hot_lines: 12,
        page_size: 4,
        warm_max_bytes: 400,
        compression: CompressionLevel::Fast,
        cold_eviction_enabled: true,
    }
}

#[test]
fn indexed_resolution_matches_linear_walk_over_random_history_and_10k_offsets() {
    let mut indexed = TieredScrollback::new_with_prefix_index(cfg(), true);
    let mut linear = TieredScrollback::new_with_prefix_index(cfg(), false);
    assert!(
        indexed.prefix_index_active(),
        "indexed arm must resolve via the prefix index"
    );
    assert!(
        !linear.prefix_index_active(),
        "linear arm must resolve via the legacy walk"
    );

    let mut rng = 0x1234_5678_9ABC_DEF1_u64;
    let mut counter = 0usize;
    let mut saw_warm = false;
    let mut saw_cold = false;

    for round in 0..400u64 {
        match xorshift(&mut rng) % 12 {
            0 => {
                let n = (xorshift(&mut rng) % 3 + 1) as usize;
                assert_eq!(indexed.evict_warm_pages(n), linear.evict_warm_pages(n));
            }
            1 if round > 20 => {
                indexed.evict_all_warm();
                linear.evict_all_warm();
            }
            2 => {
                let target = (xorshift(&mut rng) % 300) as usize;
                assert_eq!(
                    indexed.evict_warm_to_target(target),
                    linear.evict_warm_to_target(target)
                );
            }
            _ => {
                let batch = (xorshift(&mut rng) % 6 + 1) as usize;
                for _ in 0..batch {
                    let s = format!("r{round}-l{counter}-{}", xorshift(&mut rng) % 1000);
                    indexed.push_line(s.clone());
                    linear.push_line(s);
                    counter += 1;
                }
            }
        }

        // Structural parity must hold at every step (sanity for the comparison).
        assert_eq!(indexed.hot_len(), linear.hot_len());
        assert_eq!(indexed.warm_page_count(), linear.warm_page_count());
        assert_eq!(indexed.cold_page_count(), linear.cold_page_count());
        assert_eq!(indexed.total_line_count(), linear.total_line_count());
        saw_warm |= indexed.warm_page_count() > 0;
        saw_cold |= indexed.cold_page_count() > 0;

        // The indexed instance must keep resolving via the prefix index — guards
        // against a silent fallback making the comparison vacuous.
        assert!(
            indexed.prefix_index_active(),
            "prefix index must stay live + consistent (round {round})"
        );

        let total = indexed.total_line_count() as usize;
        let span = total + 8;
        for _ in 0..40 {
            let o = (xorshift(&mut rng) as usize) % span;
            assert_eq!(
                indexed.locate_offset(o),
                linear.locate_offset(o),
                "locate_offset mismatch at offset {o} (round {round})"
            );
            assert_eq!(
                indexed.tier_for_offset(o),
                linear.tier_for_offset(o),
                "tier_for_offset mismatch at offset {o} (round {round})"
            );
        }
    }

    let total = indexed.total_line_count() as usize;
    assert!(saw_warm, "history must exercise the warm tier");
    assert!(saw_cold, "history must exercise the cold tier");
    assert!(total > 0);

    // Exhaustive sweep over every offset 0..total plus an out-of-range tail
    // (covers the `None` / "everything beyond is Cold" boundaries).
    for o in 0..total + 16 {
        assert_eq!(
            indexed.locate_offset(o),
            linear.locate_offset(o),
            "exhaustive locate_offset mismatch at {o}"
        );
        assert_eq!(
            indexed.tier_for_offset(o),
            linear.tier_for_offset(o),
            "exhaustive tier_for_offset mismatch at {o}"
        );
    }

    // 10k randomized offsets over the final structure (assignment mandate).
    let span = total + 16;
    for _ in 0..10_000 {
        let o = (xorshift(&mut rng) as usize) % span;
        assert_eq!(
            indexed.locate_offset(o),
            linear.locate_offset(o),
            "10k-sweep locate_offset mismatch at {o}"
        );
        assert_eq!(
            indexed.tier_for_offset(o),
            linear.tier_for_offset(o),
            "10k-sweep tier_for_offset mismatch at {o}"
        );
    }
}
