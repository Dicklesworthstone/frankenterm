//! Property-based tests for auto_tune module.
//!
//! Verifies adaptive control loop invariants:
//! - ParamRange clamping: monotone, idempotent, within bounds
//! - TunableParams: clamp_to_ranges always produces valid params
//! - Gradual change: max_change_per_tick bounds per-tick delta
//! - Hysteresis: no change before sustained signal threshold
//! - Deadband: metrics near targets produce no adjustments
//! - Direction correctness: pressure moves params the right way
//! - Pinned isolation: pinned params never modified
//! - Tick count: equals number of tick() calls
//! - Serde roundtrip: TunableParams, AutoTuneConfig survive JSON
//! - Adjustment log: adjustments reflect actual changes
//!
//! Bead: wa-sv4e

use proptest::prelude::*;

use frankenterm_core::auto_tune::{
    AutoTuneConfig, AutoTuner, BACKPRESSURE_THRESHOLD_RANGE, POLL_INTERVAL_RANGE, POOL_SIZE_RANGE,
    PinnedParams, SCROLLBACK_LINES_RANGE, SNAPSHOT_INTERVAL_RANGE, TunableParams, TunerMetrics,
    TuningTargets,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_metrics() -> impl Strategy<Value = TunerMetrics> {
    (0.0..=1.0_f64, 0.1..=100.0_f64, 0.0..=1.0_f64).prop_map(|(rss, latency, cpu)| TunerMetrics {
        rss_fraction: rss,
        mux_latency_ms: latency,
        cpu_fraction: cpu,
    })
}

fn arb_tunable_params() -> impl Strategy<Value = TunableParams> {
    (
        0.0..=20_000.0_f64,
        0.0..=20_000.0_f64,
        0.0..=3_600.0_f64,
        0.0..=32.0_f64,
        -0.5..=1.5_f64,
    )
        .prop_map(|(poll, scroll, snap, pool, bp)| TunableParams {
            poll_interval_ms: poll,
            scrollback_lines: scroll,
            snapshot_interval_secs: snap,
            pool_size: pool,
            backpressure_threshold: bp,
        })
}

fn arb_valid_tunable_params() -> impl Strategy<Value = TunableParams> {
    (
        POLL_INTERVAL_RANGE.min..=POLL_INTERVAL_RANGE.max,
        SCROLLBACK_LINES_RANGE.min..=SCROLLBACK_LINES_RANGE.max,
        SNAPSHOT_INTERVAL_RANGE.min..=SNAPSHOT_INTERVAL_RANGE.max,
        POOL_SIZE_RANGE.min..=POOL_SIZE_RANGE.max,
        BACKPRESSURE_THRESHOLD_RANGE.min..=BACKPRESSURE_THRESHOLD_RANGE.max,
    )
        .prop_map(|(poll, scroll, snap, pool, bp)| TunableParams {
            poll_interval_ms: poll,
            scrollback_lines: scroll,
            snapshot_interval_secs: snap,
            pool_size: pool,
            backpressure_threshold: bp,
        })
}

fn arb_config() -> impl Strategy<Value = AutoTuneConfig> {
    (1usize..=10, 0.01..=0.5_f64, 5usize..=200).prop_map(|(hyst, max_change, hist_limit)| {
        AutoTuneConfig {
            enabled: true,
            tick_interval_secs: 30,
            targets: TuningTargets::default(),
            max_change_per_tick: max_change,
            hysteresis_ticks: hyst,
            history_limit: hist_limit,
        }
    })
}

fn arb_pinned() -> impl Strategy<Value = PinnedParams> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(poll, scroll, snap, pool, bp)| PinnedParams {
            poll_interval_ms: poll,
            scrollback_lines: scroll,
            snapshot_interval_secs: snap,
            pool_size: pool,
            backpressure_threshold: bp,
        })
}

fn arb_deadband_metrics() -> impl Strategy<Value = TunerMetrics> {
    let targets = TuningTargets::default();
    let rss_low = targets.target_rss_fraction * 0.95;
    let rss_high = targets.target_rss_fraction * 1.05;
    let lat_low = targets.target_latency_ms * 0.95;
    let lat_high = targets.target_latency_ms * 1.05;
    let cpu_low = targets.target_cpu_fraction * 0.95;
    let cpu_high = targets.target_cpu_fraction * 1.05;
    (rss_low..=rss_high, lat_low..=lat_high, cpu_low..=cpu_high).prop_map(|(rss, latency, cpu)| {
        TunerMetrics {
            rss_fraction: rss,
            mux_latency_ms: latency,
            cpu_fraction: cpu,
        }
    })
}

// ────────────────────────────────────────────────────────────────────
// ParamRange clamping invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Clamped value is always within [min, max].
    #[test]
    fn prop_param_range_clamp_within_bounds(v in -1e6..=1e6_f64) {
        let ranges = [
            POLL_INTERVAL_RANGE,
            SCROLLBACK_LINES_RANGE,
            SNAPSHOT_INTERVAL_RANGE,
            POOL_SIZE_RANGE,
            BACKPRESSURE_THRESHOLD_RANGE,
        ];
        for range in &ranges {
            let clamped = range.clamp(v);
            prop_assert!(
                clamped >= range.min,
                "clamped {} < min {} for value {}",
                clamped, range.min, v
            );
            prop_assert!(
                clamped <= range.max,
                "clamped {} > max {} for value {}",
                clamped, range.max, v
            );
        }
    }

    /// Clamping is idempotent: clamp(clamp(x)) == clamp(x).
    #[test]
    fn prop_param_range_clamp_idempotent(v in -1e6..=1e6_f64) {
        let ranges = [
            POLL_INTERVAL_RANGE,
            SCROLLBACK_LINES_RANGE,
            SNAPSHOT_INTERVAL_RANGE,
            POOL_SIZE_RANGE,
            BACKPRESSURE_THRESHOLD_RANGE,
        ];
        for range in &ranges {
            let once = range.clamp(v);
            let twice = range.clamp(once);
            prop_assert!(
                (once - twice).abs() < f64::EPSILON,
                "clamping must be idempotent for value {}: {} != {}",
                v, once, twice
            );
        }
    }

    /// Clamping is monotone: if a <= b, then clamp(a) <= clamp(b).
    #[test]
    fn prop_param_range_clamp_monotone(
        a in -1e6..=1e6_f64,
        b in -1e6..=1e6_f64,
    ) {
        let ranges = [
            POLL_INTERVAL_RANGE,
            SCROLLBACK_LINES_RANGE,
            SNAPSHOT_INTERVAL_RANGE,
            POOL_SIZE_RANGE,
            BACKPRESSURE_THRESHOLD_RANGE,
        ];
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        for range in &ranges {
            let clo = range.clamp(lo);
            let chi = range.clamp(hi);
            prop_assert!(
                clo <= chi,
                "clamp must be monotone: clamp({}) = {} > clamp({}) = {}",
                lo, clo, hi, chi
            );
        }
    }

    /// Values already within range are unchanged by clamping.
    #[test]
    fn prop_param_range_clamp_identity_in_range(params in arb_valid_tunable_params()) {
        let mut clamped = params.clone();
        clamped.clamp_to_ranges();
        prop_assert_eq!(params, clamped, "in-range params must not change under clamping");
    }
}

// ────────────────────────────────────────────────────────────────────
// TunableParams clamp_to_ranges
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After clamp_to_ranges, all parameters are within their valid ranges.
    #[test]
    fn prop_clamp_to_ranges_all_valid(params in arb_tunable_params()) {
        let mut p = params;
        p.clamp_to_ranges();
        prop_assert!(p.poll_interval_ms >= POLL_INTERVAL_RANGE.min);
        prop_assert!(p.poll_interval_ms <= POLL_INTERVAL_RANGE.max);
        prop_assert!(p.scrollback_lines >= SCROLLBACK_LINES_RANGE.min);
        prop_assert!(p.scrollback_lines <= SCROLLBACK_LINES_RANGE.max);
        prop_assert!(p.snapshot_interval_secs >= SNAPSHOT_INTERVAL_RANGE.min);
        prop_assert!(p.snapshot_interval_secs <= SNAPSHOT_INTERVAL_RANGE.max);
        prop_assert!(p.pool_size >= POOL_SIZE_RANGE.min);
        prop_assert!(p.pool_size <= POOL_SIZE_RANGE.max);
        prop_assert!(p.backpressure_threshold >= BACKPRESSURE_THRESHOLD_RANGE.min);
        prop_assert!(p.backpressure_threshold <= BACKPRESSURE_THRESHOLD_RANGE.max);
    }
}

// ────────────────────────────────────────────────────────────────────
// AutoTuner: range invariant under arbitrary metrics
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// For any sequence of metrics and config, all output params stay within ranges.
    #[test]
    fn prop_tuner_range_invariant(
        config in arb_config(),
        metrics in prop::collection::vec(arb_metrics(), 1..=50),
    ) {
        let mut tuner = AutoTuner::new(config);
        for m in &metrics {
            let params = tuner.tick(m);
            prop_assert!(params.poll_interval_ms >= POLL_INTERVAL_RANGE.min);
            prop_assert!(params.poll_interval_ms <= POLL_INTERVAL_RANGE.max);
            prop_assert!(params.scrollback_lines >= SCROLLBACK_LINES_RANGE.min);
            prop_assert!(params.scrollback_lines <= SCROLLBACK_LINES_RANGE.max);
            prop_assert!(params.snapshot_interval_secs >= SNAPSHOT_INTERVAL_RANGE.min);
            prop_assert!(params.snapshot_interval_secs <= SNAPSHOT_INTERVAL_RANGE.max);
            prop_assert!(params.pool_size >= POOL_SIZE_RANGE.min);
            prop_assert!(params.pool_size <= POOL_SIZE_RANGE.max);
            prop_assert!(params.backpressure_threshold >= BACKPRESSURE_THRESHOLD_RANGE.min);
            prop_assert!(params.backpressure_threshold <= BACKPRESSURE_THRESHOLD_RANGE.max);
        }
    }

    /// Range invariant holds even with arbitrary initial params.
    #[test]
    fn prop_tuner_range_invariant_with_initial(
        config in arb_config(),
        initial in arb_tunable_params(),
        metrics in prop::collection::vec(arb_metrics(), 1..=30),
    ) {
        let mut tuner = AutoTuner::with_params(config, initial);
        for m in &metrics {
            let params = tuner.tick(m);
            prop_assert!(params.poll_interval_ms >= POLL_INTERVAL_RANGE.min);
            prop_assert!(params.poll_interval_ms <= POLL_INTERVAL_RANGE.max);
            prop_assert!(params.scrollback_lines >= SCROLLBACK_LINES_RANGE.min);
            prop_assert!(params.scrollback_lines <= SCROLLBACK_LINES_RANGE.max);
            prop_assert!(params.snapshot_interval_secs >= SNAPSHOT_INTERVAL_RANGE.min);
            prop_assert!(params.snapshot_interval_secs <= SNAPSHOT_INTERVAL_RANGE.max);
            prop_assert!(params.pool_size >= POOL_SIZE_RANGE.min);
            prop_assert!(params.pool_size <= POOL_SIZE_RANGE.max);
            prop_assert!(params.backpressure_threshold >= BACKPRESSURE_THRESHOLD_RANGE.min);
            prop_assert!(params.backpressure_threshold <= BACKPRESSURE_THRESHOLD_RANGE.max);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Gradual change: bounded per-tick delta
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Each tick changes each parameter by at most max_change_per_tick fraction
    /// per pressure source. poll_interval_ms can be adjusted by both latency
    /// and CPU pressures in the same tick (2x bound).
    #[test]
    fn prop_gradual_change_bounded(
        max_change in 0.01..=0.3_f64,
        metrics in prop::collection::vec(arb_metrics(), 2..=30),
    ) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            max_change_per_tick: max_change,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let mut prev = tuner.params().clone();

        for m in &metrics {
            tuner.tick(m);
            let current = tuner.params();
            let tolerance = 1e-6;

            // poll_interval_ms: affected by BOTH latency AND CPU pressure.
            // Compounding: second adjustment uses already-modified value,
            // so total bound is max_change * (2 + max_change). Use 3x for safety
            // since clamp_to_ranges() may also shift the value.
            if prev.poll_interval_ms > 0.0 {
                let delta = (current.poll_interval_ms - prev.poll_interval_ms).abs();
                let bound = (prev.poll_interval_ms * max_change).mul_add(3.0, tolerance);
                prop_assert!(
                    delta <= bound,
                    "poll_interval delta {} exceeds bound {}",
                    delta, bound
                );
            }

            // scrollback_lines: affected by memory pressure only → 1x bound
            if prev.scrollback_lines > 0.0 {
                let delta = (current.scrollback_lines - prev.scrollback_lines).abs();
                let bound = prev.scrollback_lines.mul_add(max_change, tolerance);
                prop_assert!(
                    delta <= bound,
                    "scrollback delta {} exceeds bound {}",
                    delta, bound
                );
            }

            // pool_size: affected by CPU pressure only → 1x bound
            if prev.pool_size > 0.0 {
                let delta = (current.pool_size - prev.pool_size).abs();
                let bound = prev.pool_size.mul_add(max_change, tolerance);
                prop_assert!(
                    delta <= bound,
                    "pool_size delta {} exceeds bound {}",
                    delta, bound
                );
            }

            // snapshot_interval_secs: affected by memory pressure only → 1x bound
            if prev.snapshot_interval_secs > 0.0 {
                let delta =
                    (current.snapshot_interval_secs - prev.snapshot_interval_secs).abs();
                let bound = prev.snapshot_interval_secs.mul_add(max_change, tolerance);
                prop_assert!(
                    delta <= bound,
                    "snapshot_interval delta {} exceeds bound {}",
                    delta, bound
                );
            }

            prev = current.clone();
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Hysteresis: no change before threshold
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// With hysteresis_ticks = N, fewer than N ticks of pressure produce no change.
    #[test]
    fn prop_hysteresis_prevents_early_change(
        hysteresis in 2usize..=8,
        high_rss in 0.7..=0.95_f64,
    ) {
        let config = AutoTuneConfig {
            hysteresis_ticks: hysteresis,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().clone();

        let metrics = TunerMetrics {
            rss_fraction: high_rss,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.15,
        };

        for _ in 0..(hysteresis - 1) {
            tuner.tick(&metrics);
        }

        prop_assert!(
            (tuner.params().scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON,
            "scrollback should not change before hysteresis threshold: {} != {}",
            tuner.params().scrollback_lines, initial.scrollback_lines
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Deadband: metrics near targets → no change
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Metrics within the 0.95-1.05 deadband of targets produce no parameter changes.
    #[test]
    fn prop_deadband_no_change(
        metrics in prop::collection::vec(arb_deadband_metrics(), 1..=30),
    ) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().clone();

        for m in &metrics {
            tuner.tick(m);
        }

        prop_assert!(
            (tuner.params().poll_interval_ms - initial.poll_interval_ms).abs() < f64::EPSILON,
            "poll_interval should not change in deadband: {} != {}",
            tuner.params().poll_interval_ms, initial.poll_interval_ms
        );
        prop_assert!(
            (tuner.params().scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON,
            "scrollback should not change in deadband: {} != {}",
            tuner.params().scrollback_lines, initial.scrollback_lines
        );
        prop_assert!(
            (tuner.params().snapshot_interval_secs - initial.snapshot_interval_secs).abs() < f64::EPSILON,
            "snapshot_interval should not change in deadband: {} != {}",
            tuner.params().snapshot_interval_secs, initial.snapshot_interval_secs
        );
        prop_assert!(
            (tuner.params().pool_size - initial.pool_size).abs() < f64::EPSILON,
            "pool_size should not change in deadband: {} != {}",
            tuner.params().pool_size, initial.pool_size
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Pinned params: never modified under any input
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Pinned parameters are never modified regardless of metrics.
    #[test]
    fn prop_pinned_params_invariant(
        pinned in arb_pinned(),
        metrics in prop::collection::vec(arb_metrics(), 1..=40),
    ) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        tuner.set_pinned(pinned.clone());
        let initial = tuner.params().clone();

        for m in &metrics {
            tuner.tick(m);
            let current = tuner.params();
            if pinned.poll_interval_ms {
                prop_assert!(
                    (current.poll_interval_ms - initial.poll_interval_ms).abs() < f64::EPSILON,
                    "pinned poll_interval_ms must not change: {} != {}",
                    current.poll_interval_ms, initial.poll_interval_ms
                );
            }
            if pinned.scrollback_lines {
                prop_assert!(
                    (current.scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON,
                    "pinned scrollback_lines must not change: {} != {}",
                    current.scrollback_lines, initial.scrollback_lines
                );
            }
            if pinned.snapshot_interval_secs {
                prop_assert!(
                    (current.snapshot_interval_secs - initial.snapshot_interval_secs).abs() < f64::EPSILON,
                    "pinned snapshot_interval_secs must not change: {} != {}",
                    current.snapshot_interval_secs, initial.snapshot_interval_secs
                );
            }
            if pinned.pool_size {
                prop_assert!(
                    (current.pool_size - initial.pool_size).abs() < f64::EPSILON,
                    "pinned pool_size must not change: {} != {}",
                    current.pool_size, initial.pool_size
                );
            }
            if pinned.backpressure_threshold {
                prop_assert!(
                    (current.backpressure_threshold - initial.backpressure_threshold).abs() < f64::EPSILON,
                    "pinned backpressure_threshold must not change: {} != {}",
                    current.backpressure_threshold, initial.backpressure_threshold
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Tick count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// tick_count() equals the number of tick() calls.
    #[test]
    fn prop_tick_count_accurate(n in 1usize..=100) {
        let mut tuner = AutoTuner::new(AutoTuneConfig::default());
        let metrics = TunerMetrics {
            rss_fraction: 0.5,
            mux_latency_ms: 10.0,
            cpu_fraction: 0.3,
        };
        for _ in 0..n {
            tuner.tick(&metrics);
        }
        prop_assert_eq!(tuner.tick_count(), n as u64, "tick count must equal calls");
    }
}

// ────────────────────────────────────────────────────────────────────
// Direction correctness under sustained pressure
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Sustained high memory pressure reduces scrollback.
    #[test]
    fn prop_memory_pressure_reduces_scrollback(rss in 0.65..=0.95_f64) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let initial_scrollback = tuner.params().scrollback_lines;

        let metrics = TunerMetrics {
            rss_fraction: rss,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.15,
        };

        for _ in 0..10 {
            tuner.tick(&metrics);
        }

        prop_assert!(
            tuner.params().scrollback_lines <= initial_scrollback,
            "high memory ({}) should reduce scrollback: initial={}, current={}",
            rss, initial_scrollback, tuner.params().scrollback_lines
        );
    }

    /// Sustained high memory pressure increases snapshot interval.
    #[test]
    fn prop_memory_pressure_increases_snapshot(rss in 0.65..=0.95_f64) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let initial_snap = tuner.params().snapshot_interval_secs;

        let metrics = TunerMetrics {
            rss_fraction: rss,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.15,
        };

        for _ in 0..10 {
            tuner.tick(&metrics);
        }

        prop_assert!(
            tuner.params().snapshot_interval_secs >= initial_snap,
            "high memory ({}) should increase snapshot interval",
            rss
        );
    }

    /// Sustained high latency pressure increases poll interval.
    #[test]
    fn prop_latency_pressure_increases_poll(latency in 15.0..=80.0_f64) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let initial_poll = tuner.params().poll_interval_ms;

        let metrics = TunerMetrics {
            rss_fraction: 0.3,
            mux_latency_ms: latency,
            cpu_fraction: 0.15,
        };

        for _ in 0..10 {
            tuner.tick(&metrics);
        }

        prop_assert!(
            tuner.params().poll_interval_ms >= initial_poll,
            "high latency ({}) should increase poll interval",
            latency
        );
    }

    /// Sustained high CPU pressure reduces pool size.
    #[test]
    fn prop_cpu_pressure_reduces_pool(cpu in 0.45..=0.95_f64) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let initial_pool = tuner.params().pool_size;

        let metrics = TunerMetrics {
            rss_fraction: 0.3,
            mux_latency_ms: 5.0,
            cpu_fraction: cpu,
        };

        for _ in 0..10 {
            tuner.tick(&metrics);
        }

        prop_assert!(
            tuner.params().pool_size <= initial_pool,
            "high CPU ({}) should reduce pool: initial={}, current={}",
            cpu, initial_pool, tuner.params().pool_size
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Convergence: constant input → stable parameters
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// After many ticks of constant input, parameter deltas approach zero.
    #[test]
    fn prop_convergence_constant_input(metrics in arb_metrics()) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);

        let mut prev = tuner.params().clone();
        for _ in 0..1000 {
            let current = tuner.tick(&metrics);
            prev = current;
        }

        let final_params = tuner.tick(&metrics);
        let delta = (final_params.poll_interval_ms - prev.poll_interval_ms).abs()
            + (final_params.scrollback_lines - prev.scrollback_lines).abs()
            + (final_params.snapshot_interval_secs - prev.snapshot_interval_secs).abs()
            + (final_params.pool_size - prev.pool_size).abs();

        prop_assert!(
            delta < 50.0,
            "after 1000+ ticks of constant input, delta should be small, got: {}",
            delta
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// TunableParams survives JSON serialization roundtrip (approximate f64 equality).
    #[test]
    fn prop_tunable_params_serde_roundtrip(params in arb_valid_tunable_params()) {
        let json = serde_json::to_string(&params).unwrap();
        let back: TunableParams = serde_json::from_str(&json).unwrap();
        let eps = 1e-9;
        prop_assert!(
            (params.poll_interval_ms - back.poll_interval_ms).abs() < eps,
            "poll_interval_ms serde drift"
        );
        prop_assert!(
            (params.scrollback_lines - back.scrollback_lines).abs() < eps,
            "scrollback_lines serde drift"
        );
        prop_assert!(
            (params.snapshot_interval_secs - back.snapshot_interval_secs).abs() < eps,
            "snapshot_interval_secs serde drift"
        );
        prop_assert!(
            (params.pool_size - back.pool_size).abs() < eps,
            "pool_size serde drift"
        );
        prop_assert!(
            (params.backpressure_threshold - back.backpressure_threshold).abs() < eps,
            "backpressure_threshold serde drift"
        );
    }

    /// AutoTuneConfig survives JSON serialization roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: AutoTuneConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.hysteresis_ticks, back.hysteresis_ticks);
        prop_assert_eq!(config.history_limit, back.history_limit);
        prop_assert_eq!(config.tick_interval_secs, back.tick_interval_secs);
    }
}

// ────────────────────────────────────────────────────────────────────
// Adjustment log
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Adjustments log is non-empty after sustained high pressure.
    #[test]
    fn prop_adjustments_logged_under_pressure(rss in 0.7..=0.95_f64) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);

        let metrics = TunerMetrics {
            rss_fraction: rss,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.15,
        };

        for _ in 0..10 {
            tuner.tick(&metrics);
        }

        prop_assert!(
            !tuner.adjustments().is_empty(),
            "adjustments should be logged under sustained pressure (rss={})",
            rss
        );
    }

    /// clear_adjustments() empties the log.
    #[test]
    fn prop_clear_adjustments_empties(
        metrics in prop::collection::vec(arb_metrics(), 5..=20),
    ) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);

        for m in &metrics {
            tuner.tick(m);
        }

        tuner.clear_adjustments();
        prop_assert!(
            tuner.adjustments().is_empty(),
            "adjustments must be empty after clear"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Monotonic memory response
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Under monotonically increasing memory pressure, scrollback never increases.
    #[test]
    fn prop_monotonic_memory_scrollback_decrease(base_rss in 0.6..=0.8_f64) {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            max_change_per_tick: 0.1,
            ..AutoTuneConfig::default()
        };
        let mut tuner = AutoTuner::new(config);
        let mut prev_scrollback = tuner.params().scrollback_lines;

        for i in 0..20 {
            let rss = (i as f64).mul_add(0.005, base_rss).min(1.0);
            let metrics = TunerMetrics {
                rss_fraction: rss,
                mux_latency_ms: 5.0,
                cpu_fraction: 0.15,
            };
            tuner.tick(&metrics);
            let current_scrollback = tuner.params().scrollback_lines;
            prop_assert!(
                current_scrollback <= prev_scrollback + f64::EPSILON,
                "scrollback must not increase under rising memory: prev={}, current={}, rss={}",
                prev_scrollback, current_scrollback, rss
            );
            prev_scrollback = current_scrollback;
        }
    }
}
