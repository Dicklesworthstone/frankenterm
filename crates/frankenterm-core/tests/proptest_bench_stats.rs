use proptest::prelude::*;

use frankenterm_core::bench_stats::{
    conformal_band, distribution_from_raw_iters_times, empirical_bernstein_ci, mann_whitney_u,
    min_sample_size_bernstein, min_sample_size_for_regression, min_sample_size_hoeffding,
    Distribution,
};

fn finite_samples() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(0.0f64..=1_000_000.0, 1..96)
}

fn bounded_samples() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(0.0f64..=1.0, 2..128)
}

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9_f64.max((a.abs() + b.abs()) * 1e-10)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_bench_stats_distribution_summary_preserves_order_statistics(
        samples in finite_samples(),
        confidence in 0.01f64..0.999,
    ) {
        let dist = Distribution::summarize(
            &samples,
            &[0.0, 0.5, 0.95, 1.0],
            0,
            confidence,
            0,
        )
        .expect("non-empty samples summarize");

        prop_assert_eq!(dist.sample_size, samples.len());
        prop_assert!(dist.min <= dist.mean);
        prop_assert!(dist.mean <= dist.max);
        prop_assert!(dist.stddev >= 0.0);
        prop_assert!(dist.stddev.is_finite());
        prop_assert_eq!(dist.percentiles.len(), 4);

        let mut previous = f64::NEG_INFINITY;
        for reading in &dist.percentiles {
            prop_assert!((0.0..=1.0).contains(&reading.q));
            prop_assert!(dist.min <= reading.value);
            prop_assert!(reading.value <= dist.max);
            prop_assert!(reading.value >= previous);
            prop_assert!(approx_eq(reading.ci_lower, reading.value));
            prop_assert!(approx_eq(reading.ci_upper, reading.value));
            prop_assert_eq!(reading.bootstrap_resamples, 0);
            prop_assert!(approx_eq(reading.confidence, confidence));
            previous = reading.value;
        }
    }

    #[test]
    fn proptest_bench_stats_raw_iter_conversion_matches_positive_finite_ratios(
        rows in prop::collection::vec((1.0f64..=10_000.0, 0.0f64..=1_000_000_000.0), 1..96),
    ) {
        let iters: Vec<f64> = rows.iter().map(|(iters, _)| *iters).collect();
        let times: Vec<f64> = rows.iter().map(|(_, times)| *times).collect();
        let ratios: Vec<f64> = rows.iter().map(|(iters, times)| times / iters).collect();

        let dist = distribution_from_raw_iters_times(&iters, &times)
            .expect("positive finite rows produce a distribution");

        let expected_min = ratios.iter().copied().fold(f64::INFINITY, f64::min);
        let expected_max = ratios.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        prop_assert_eq!(dist.sample_size, rows.len());
        prop_assert!(approx_eq(dist.min, expected_min));
        prop_assert!(approx_eq(dist.max, expected_max));
        prop_assert!(dist.mean >= dist.min);
        prop_assert!(dist.mean <= dist.max);

        let mismatched = distribution_from_raw_iters_times(&iters, &times[..times.len() - 1]);
        prop_assert!(mismatched.is_none());
    }

    #[test]
    fn proptest_bench_stats_mann_whitney_is_swap_symmetric(
        a in finite_samples(),
        b in finite_samples(),
    ) {
        let ab = mann_whitney_u(&a, &b).expect("non-empty sample A/B");
        let ba = mann_whitney_u(&b, &a).expect("non-empty sample B/A");
        let total_pairs = (a.len() * b.len()) as f64;

        prop_assert_eq!(ab.n_a, a.len());
        prop_assert_eq!(ab.n_b, b.len());
        prop_assert_eq!(ba.n_a, b.len());
        prop_assert_eq!(ba.n_b, a.len());
        prop_assert!(ab.u_a >= 0.0);
        prop_assert!(ab.u_a <= total_pairs);
        prop_assert!(ba.u_a >= 0.0);
        prop_assert!(ba.u_a <= total_pairs);
        prop_assert!(approx_eq(ab.u_a + ba.u_a, total_pairs));
        prop_assert!(approx_eq(ab.p_value, ba.p_value));
        prop_assert!((0.0..=1.0).contains(&ab.p_value));
    }

    #[test]
    fn proptest_bench_stats_mean_bounds_stay_above_empirical_mean(
        samples in bounded_samples(),
        range in 1.0f64..=10.0,
        alpha in 0.001f64..0.5,
    ) {
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let upper = empirical_bernstein_ci(&samples, range, alpha)
            .expect("valid bounded inputs produce a bound");

        prop_assert!(upper.is_finite());
        prop_assert!(upper >= mean);
    }

    #[test]
    fn proptest_bench_stats_sample_size_bounds_are_monotone_and_composite_is_tightest(
        small_threshold in 0.01f64..=100.0,
        threshold_delta in 0.0f64..=100.0,
        alpha in 0.001f64..0.5,
        range in 1.0f64..=10_000.0,
        var_fraction in 0.0f64..=2.0,
    ) {
        let large_threshold = small_threshold + threshold_delta;
        let var_bound = range * range * var_fraction;

        let h_small = min_sample_size_hoeffding(small_threshold, alpha, range).unwrap();
        let h_large = min_sample_size_hoeffding(large_threshold, alpha, range).unwrap();
        let b_small = min_sample_size_bernstein(small_threshold, alpha, range, var_bound).unwrap();
        let b_large = min_sample_size_bernstein(large_threshold, alpha, range, var_bound).unwrap();
        let composite = min_sample_size_for_regression(
            small_threshold,
            alpha,
            range,
            Some(var_bound),
        )
        .unwrap();

        prop_assert!(h_small >= h_large);
        prop_assert!(b_small >= b_large);
        prop_assert_eq!(composite, h_small.min(b_small));
        prop_assert_eq!(
            min_sample_size_for_regression(small_threshold, alpha, range, None),
            Some(h_small),
        );
    }

    #[test]
    fn proptest_bench_stats_conformal_band_contains_realised_confidence(
        samples in prop::collection::vec(0.0f64..=1_000_000.0, 4..128),
        alpha in 0.001f64..0.999,
    ) {
        let band = conformal_band(&samples, alpha).expect("valid conformal inputs");
        prop_assert!(band.lower <= band.upper);
        prop_assert!((0.0..=1.0).contains(&band.realised_confidence));
        prop_assert!(band.realised_confidence + 1e-12 >= 1.0 - alpha);

        let inside = samples
            .iter()
            .filter(|value| **value >= band.lower && **value <= band.upper)
            .count();
        let realised = inside as f64 / samples.len() as f64;
        prop_assert!(approx_eq(band.realised_confidence, realised));
    }
}
