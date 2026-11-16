use frankenterm_core::backpressure::{BackpressureTier, QueueDepths};
use frankenterm_core::count_min_sketch::CountMinSketch;
use frankenterm_core::ewma::{Ewma, RateEstimator};
use proptest::prelude::*;

fn small_key() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_.:-]{0,24}"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_core_reexported_ewma_stays_between_observed_bounds(
        half_life_ms in 1.0_f64..=10_000.0,
        observations in prop::collection::vec((-1_000.0_f64..=1_000.0, 0_u64..=10_000), 1..=64),
    ) {
        let mut ewma = Ewma::with_half_life_ms(half_life_ms);
        let mut values = Vec::with_capacity(observations.len());
        let mut now = 0_u64;

        for (value, delta_ms) in observations {
            now = now.saturating_add(delta_ms);
            ewma.observe(value, now);
            values.push(value);
        }

        let min_seen = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max_seen = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let value = ewma.value();

        prop_assert!(ewma.is_initialized());
        prop_assert_eq!(ewma.count(), values.len() as u64);
        prop_assert_eq!(ewma.half_life_ms().to_bits(), half_life_ms.to_bits());
        prop_assert!(value.is_finite());
        prop_assert!(value >= min_seen.min(max_seen) - f64::EPSILON);
        prop_assert!(value <= max_seen.max(min_seen) + f64::EPSILON);
    }

    #[test]
    fn proptest_core_reexported_rate_estimator_counts_and_resets(
        half_life_ms in 1.0_f64..=10_000.0,
        intervals in prop::collection::vec(1_u64..=10_000, 0..=64),
    ) {
        let mut rate = RateEstimator::with_half_life_ms(half_life_ms);
        let mut now = 0_u64;

        rate.tick(now);
        for interval in &intervals {
            now = now.saturating_add(*interval);
            rate.tick(now);
        }

        prop_assert_eq!(rate.total_events(), intervals.len() as u64 + 1);
        if intervals.is_empty() {
            prop_assert!(rate.rate_per_sec().abs() <= f64::EPSILON);
        } else {
            prop_assert!(rate.rate_per_sec().is_finite());
            prop_assert!(rate.rate_per_sec() > 0.0);
        }

        rate.reset();
        prop_assert_eq!(rate.total_events(), 0);
        prop_assert!(rate.rate_per_sec().abs() <= f64::EPSILON);
    }

    #[test]
    fn proptest_core_reexported_count_min_sketch_estimates_upper_bound(
        key in small_key(),
        count in 0_u64..=1_000,
        noise in prop::collection::vec((small_key(), 0_u64..=50), 0..=32),
    ) {
        let mut sketch = CountMinSketch::with_dimensions(64, 4);
        sketch.add(&key, count);
        let mut total = count;

        for (noise_key, noise_count) in noise {
            sketch.add(&noise_key, noise_count);
            total = total.saturating_add(noise_count);
        }

        prop_assert_eq!(sketch.total_count(), total);
        prop_assert!(sketch.estimate(&key) >= count);
        prop_assert!(sketch.estimate(&key) <= total);
        prop_assert_eq!(sketch.stats().total_count, total);
        prop_assert!(!sketch.is_empty() || total == 0);
    }

    #[test]
    fn proptest_core_reexported_count_min_sketch_merge_preserves_dimensions_and_counts(
        left_key in small_key(),
        right_key in small_key(),
        left_count in 0_u64..=1_000,
        right_count in 0_u64..=1_000,
    ) {
        let mut left = CountMinSketch::with_dimensions(32, 3);
        let mut right = CountMinSketch::with_dimensions(32, 3);
        left.add(&left_key, left_count);
        right.add(&right_key, right_count);

        left.merge(&right).expect("matching dimensions should merge");

        let expected_total = left_count.saturating_add(right_count);
        prop_assert_eq!(left.total_count(), expected_total);
        prop_assert_eq!(left.width(), 32);
        prop_assert_eq!(left.depth(), 3);
        prop_assert!(left.estimate(&left_key) >= left_count);
        prop_assert!(left.estimate(&right_key) >= right_count);
        prop_assert_eq!(
            left.inner_product(&CountMinSketch::with_dimensions(16, 3)),
            None,
        );
    }

    #[test]
    fn proptest_core_reexported_queue_depth_ratios_match_public_model(
        capture_depth in 0_usize..=10_000,
        capture_capacity in 0_usize..=10_000,
        write_depth in 0_usize..=10_000,
        write_capacity in 0_usize..=10_000,
    ) {
        let depths = QueueDepths {
            capture_depth,
            capture_capacity,
            write_depth,
            write_capacity,
        };
        let expected_capture = if capture_capacity == 0 {
            0.0
        } else {
            capture_depth as f64 / capture_capacity as f64
        };
        let expected_write = if write_capacity == 0 {
            0.0
        } else {
            write_depth as f64 / write_capacity as f64
        };

        prop_assert!((depths.capture_ratio() - expected_capture).abs() <= f64::EPSILON);
        prop_assert!((depths.write_ratio() - expected_write).abs() <= f64::EPSILON);
        prop_assert!(depths.capture_ratio().is_finite());
        prop_assert!(depths.write_ratio().is_finite());
    }

    #[test]
    fn proptest_core_reexported_backpressure_tier_wire_shape_is_stable(
        tier in prop::sample::select(vec![
            BackpressureTier::Green,
            BackpressureTier::Yellow,
            BackpressureTier::Red,
            BackpressureTier::Black,
        ]),
    ) {
        let encoded = serde_json::to_string(&tier).expect("tier should serialize");
        let decoded: BackpressureTier =
            serde_json::from_str(&encoded).expect("tier should deserialize");

        prop_assert_eq!(decoded, tier);
        prop_assert_eq!(tier.as_u8(), tier as u8);
        prop_assert_eq!(
            tier.to_string(),
            match tier {
                BackpressureTier::Green => "GREEN",
                BackpressureTier::Yellow => "YELLOW",
                BackpressureTier::Red => "RED",
                BackpressureTier::Black => "BLACK",
            },
        );
    }
}
