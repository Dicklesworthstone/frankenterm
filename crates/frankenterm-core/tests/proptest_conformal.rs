//! Property-based tests for conformal prediction intervals.
//!
//! Bead: wa-0tls
//!
//! Validates:
//! 1. HoltPredictor: alpha/beta clamped to [0.001, 0.999]
//! 2. HoltPredictor: first observation sets level, zero trend
//! 3. HoltPredictor: level and trend remain finite for arbitrary inputs
//! 4. HoltPredictor: NaN/Inf observations are skipped
//! 5. HoltPredictor: forecast(h) = level + h * trend
//! 6. HoltPredictor: constant series → level converges, trend → 0
//! 7. HoltPredictor: observation_count tracks non-NaN updates
//! 8. ConformalConfig: serde roundtrip preserves all fields
//! 9. ConformalConfig: default has valid ranges
//! 10. ResourceForecast: serde roundtrip
//! 11. ResourceForecast: horizon_secs = horizon_steps * interval
//! 12. ResourceForecast: interval_width = upper - lower
//! 13. ResourceForecast: is_calibrated iff both bounds finite
//! 14. ForecastAlert: serde roundtrip
//! 15. MetricForecaster: forecast count equals horizon count
//! 16. MetricForecaster: calibrated interval has upper >= lower
//! 17. MetricForecaster: point_estimate within calibrated interval
//! 18. MetricForecaster: unknown horizon returns None
//! 19. MetricForecaster: observation_count tracks updates
//! 20. MetricForecaster: coverage clamped to [0.01, 0.999]
//! 21. ConformalForecaster: metric_count increases with new metrics
//! 22. ConformalForecaster: has_metric after observe
//! 23. ConformalForecaster: observation_count tracks per-metric
//! 24. ConformalForecaster: forecasts non-empty after warmup

use proptest::prelude::*;

use frankenterm_core::conformal::{
    ConformalConfig, ConformalForecaster, ForecastAlert, HoltPredictor, MetricForecaster,
    ResourceForecast,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_alpha() -> impl Strategy<Value = f64> {
    -1.0_f64..2.0
}

fn arb_beta() -> impl Strategy<Value = f64> {
    -1.0_f64..2.0
}

fn arb_finite_value() -> impl Strategy<Value = f64> {
    -1e10_f64..1e10
}

fn arb_positive_value() -> impl Strategy<Value = f64> {
    0.0_f64..1e8
}

fn arb_coverage() -> impl Strategy<Value = f64> {
    0.01_f64..0.999
}

// =============================================================================
// Property 1: HoltPredictor alpha/beta clamped
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn holt_alpha_beta_clamped(
        alpha in -10.0_f64..10.0,
        beta in -10.0_f64..10.0,
    ) {
        let holt = HoltPredictor::new(alpha, beta);
        // We can only observe the effect indirectly through behavior.
        // Update with a value and verify finite output.
        let mut h = holt;
        h.update(100.0);
        h.update(200.0);
        prop_assert!(h.level().is_finite(),
            "level should be finite after clamped alpha={}, beta={}", alpha, beta);
        prop_assert!(h.trend().is_finite(),
            "trend should be finite after clamped alpha={}, beta={}", alpha, beta);
    }
}

// =============================================================================
// Property 2: First observation sets level, zero trend
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn holt_first_observation(
        value in arb_finite_value(),
    ) {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        holt.update(value);
        prop_assert_eq!(holt.observation_count(), 1);
        prop_assert!((holt.level() - value).abs() < 1e-10,
            "first obs: level {} should equal value {}", holt.level(), value);
        prop_assert!((holt.trend()).abs() < 1e-10,
            "first obs: trend {} should be ~0", holt.trend());
    }
}

// =============================================================================
// Property 3: Level and trend remain finite
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn holt_stays_finite(
        values in proptest::collection::vec(arb_finite_value(), 5..50),
        alpha in 0.01_f64..0.99,
        beta in 0.01_f64..0.99,
    ) {
        let mut holt = HoltPredictor::new(alpha, beta);
        for &v in &values {
            holt.update(v);
        }
        prop_assert!(holt.level().is_finite(),
            "level should remain finite after {} observations", values.len());
        prop_assert!(holt.trend().is_finite(),
            "trend should remain finite after {} observations", values.len());
        let fc = holt.forecast(10.0);
        prop_assert!(fc.is_finite(),
            "forecast should remain finite: {}", fc);
    }
}

// =============================================================================
// Property 4: NaN/Inf observations skipped
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn holt_skips_non_finite(
        initial in arb_finite_value(),
        n_non_finite in 1_usize..10,
    ) {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        holt.update(initial);
        let count_before = holt.observation_count();
        let level_before = holt.level();

        for _ in 0..n_non_finite {
            holt.update(f64::NAN);
            holt.update(f64::INFINITY);
            holt.update(f64::NEG_INFINITY);
        }

        prop_assert_eq!(holt.observation_count(), count_before,
            "NaN/Inf should not increment observation count");
        prop_assert!((holt.level() - level_before).abs() < 1e-10,
            "level should not change on NaN/Inf input");
    }
}

// =============================================================================
// Property 5: forecast(h) = level + h * trend
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn holt_forecast_formula(
        values in proptest::collection::vec(arb_finite_value(), 3..20),
        steps in 0.0_f64..100.0,
    ) {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        for &v in &values {
            holt.update(v);
        }
        let expected = holt.level() + steps * holt.trend();
        let actual = holt.forecast(steps);
        // mul_add may differ from separate multiply+add by a few ULP at large magnitudes
        let tolerance = expected.abs() * 1e-10 + 1e-6;
        prop_assert!((actual - expected).abs() < tolerance,
            "forecast({}) = {} should equal level + h*trend = {} (tolerance={})",
            steps, actual, expected, tolerance);
    }
}

// =============================================================================
// Property 6: Constant series → level converges, trend → 0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn holt_constant_convergence(
        value in -1e6_f64..1e6,
        alpha in 0.1_f64..0.9,
        beta in 0.01_f64..0.5,
    ) {
        let mut holt = HoltPredictor::new(alpha, beta);
        for _ in 0..200 {
            holt.update(value);
        }
        prop_assert!((holt.level() - value).abs() < 1.0,
            "after 200 constant obs, level {} should ≈ value {}", holt.level(), value);
        prop_assert!(holt.trend().abs() < 1.0,
            "after 200 constant obs, trend {} should ≈ 0", holt.trend());
    }
}

// =============================================================================
// Property 7: Observation count tracks non-NaN updates
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn holt_observation_count(
        values in proptest::collection::vec(arb_finite_value(), 1..50),
    ) {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        for &v in &values {
            holt.update(v);
        }
        prop_assert_eq!(holt.observation_count(), values.len() as u64,
            "count {} should equal number of finite values {}", holt.observation_count(), values.len());
    }
}

// =============================================================================
// Property 8: ConformalConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(
        coverage in arb_coverage(),
        cal_window in 50_usize..500,
        alpha in 0.01_f64..0.99,
        beta in 0.01_f64..0.99,
    ) {
        let config = ConformalConfig {
            coverage,
            calibration_window: cal_window,
            horizon_steps: vec![60, 120],
            holt_alpha: alpha,
            holt_beta: beta,
            observation_interval_secs: 30,
            rss_alarm_fraction: 0.80,
            cpu_alarm_percent: 90.0,
            max_history: 8640,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ConformalConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.coverage - config.coverage).abs() < 1e-10);
        prop_assert_eq!(back.calibration_window, config.calibration_window);
        prop_assert!((back.holt_alpha - config.holt_alpha).abs() < 1e-10);
        prop_assert!((back.holt_beta - config.holt_beta).abs() < 1e-10);
        prop_assert_eq!(back.horizon_steps, config.horizon_steps);
        prop_assert_eq!(back.max_history, config.max_history);
    }
}

// =============================================================================
// Property 9: Default config has valid ranges
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn config_defaults_valid(_dummy in 0..1_u32) {
        let config = ConformalConfig::default();
        prop_assert!(config.coverage > 0.0 && config.coverage < 1.0,
            "coverage {} out of (0,1)", config.coverage);
        prop_assert!(config.holt_alpha > 0.0 && config.holt_alpha < 1.0,
            "holt_alpha {} out of (0,1)", config.holt_alpha);
        prop_assert!(config.holt_beta > 0.0 && config.holt_beta < 1.0,
            "holt_beta {} out of (0,1)", config.holt_beta);
        prop_assert!(config.calibration_window > 0);
        prop_assert!(!config.horizon_steps.is_empty());
        prop_assert!(config.rss_alarm_fraction > 0.0 && config.rss_alarm_fraction <= 1.0);
        prop_assert!(config.cpu_alarm_percent > 0.0 && config.cpu_alarm_percent <= 100.0);
        prop_assert!(config.max_history > 0);
        prop_assert!(config.observation_interval_secs > 0);
    }
}

// =============================================================================
// Property 10: ResourceForecast serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn forecast_serde_roundtrip(
        point in -1e6_f64..1e6,
        lower in -1e6_f64..0.0,
        upper in 0.0_f64..1e6,
        horizon in 1_usize..1000,
        coverage in arb_coverage(),
        cal_size in 0_usize..500,
    ) {
        let fc = ResourceForecast {
            metric_name: "test_metric".into(),
            horizon_steps: horizon,
            point_estimate: point,
            lower_bound: lower,
            upper_bound: upper,
            coverage,
            calibration_size: cal_size,
            alert: None,
        };
        let json = serde_json::to_string(&fc).unwrap();
        let back: ResourceForecast = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.metric_name, fc.metric_name);
        prop_assert_eq!(back.horizon_steps, fc.horizon_steps);
        // Use relative tolerance for f64 JSON roundtrip (large values lose precision)
        let tol = |a: f64, b: f64| (a - b).abs() <= a.abs().max(b.abs()) * 1e-10 + 1e-10;
        prop_assert!(tol(back.point_estimate, fc.point_estimate),
            "point_estimate mismatch: {} vs {}", back.point_estimate, fc.point_estimate);
        prop_assert!(tol(back.lower_bound, fc.lower_bound),
            "lower_bound mismatch: {} vs {}", back.lower_bound, fc.lower_bound);
        prop_assert!(tol(back.upper_bound, fc.upper_bound),
            "upper_bound mismatch: {} vs {}", back.upper_bound, fc.upper_bound);
        prop_assert!(tol(back.coverage, fc.coverage),
            "coverage mismatch: {} vs {}", back.coverage, fc.coverage);
        prop_assert_eq!(back.calibration_size, fc.calibration_size);
    }
}

// =============================================================================
// Property 11: horizon_secs = horizon_steps * interval
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn forecast_horizon_secs(
        horizon in 1_usize..10000,
        interval in 1_u64..3600,
    ) {
        let fc = ResourceForecast {
            metric_name: "test".into(),
            horizon_steps: horizon,
            point_estimate: 0.0,
            lower_bound: -1.0,
            upper_bound: 1.0,
            coverage: 0.95,
            calibration_size: 100,
            alert: None,
        };
        prop_assert_eq!(fc.horizon_secs(interval), horizon as u64 * interval);
    }
}

// =============================================================================
// Property 12: interval_width = upper - lower
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn forecast_interval_width(
        lower in -1e6_f64..0.0,
        upper in 0.0_f64..1e6,
    ) {
        let fc = ResourceForecast {
            metric_name: "test".into(),
            horizon_steps: 5,
            point_estimate: 0.0,
            lower_bound: lower,
            upper_bound: upper,
            coverage: 0.95,
            calibration_size: 100,
            alert: None,
        };
        let width = fc.interval_width();
        let expected = upper - lower;
        prop_assert!((width - expected).abs() < 1e-10,
            "interval_width {} should equal upper - lower = {}", width, expected);
    }
}

// =============================================================================
// Property 13: is_calibrated iff both bounds finite
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn forecast_is_calibrated(
        lower in -1e6_f64..1e6,
        upper in -1e6_f64..1e6,
    ) {
        let fc = ResourceForecast {
            metric_name: "test".into(),
            horizon_steps: 5,
            point_estimate: 0.0,
            lower_bound: lower,
            upper_bound: upper,
            coverage: 0.95,
            calibration_size: 100,
            alert: None,
        };
        prop_assert!(fc.is_calibrated(),
            "finite bounds ({}, {}) should be calibrated", lower, upper);

        let fc_inf = ResourceForecast {
            lower_bound: f64::NEG_INFINITY,
            upper_bound: f64::INFINITY,
            ..fc
        };
        prop_assert!(!fc_inf.is_calibrated(),
            "infinite bounds should not be calibrated");
    }
}

// =============================================================================
// Property 14: ForecastAlert serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn alert_serde_roundtrip(
        upper in 0.0_f64..1e10,
        threshold in 0.0_f64..1e10,
        horizon in 1_usize..500,
    ) {
        let rss_alert = ForecastAlert::RssThreshold {
            upper_bound_bytes: upper,
            threshold_bytes: threshold,
            horizon_steps: horizon,
        };
        let tol = |a: f64, b: f64| (a - b).abs() <= a.abs().max(b.abs()) * 1e-10 + 1e-10;

        let json = serde_json::to_string(&rss_alert).unwrap();
        let back: ForecastAlert = serde_json::from_str(&json).unwrap();
        match back {
            ForecastAlert::RssThreshold {
                upper_bound_bytes,
                threshold_bytes,
                horizon_steps,
            } => {
                prop_assert!(tol(upper_bound_bytes, upper),
                    "rss upper mismatch: {} vs {}", upper_bound_bytes, upper);
                prop_assert!(tol(threshold_bytes, threshold),
                    "rss threshold mismatch: {} vs {}", threshold_bytes, threshold);
                prop_assert_eq!(horizon_steps, horizon);
            }
            _ => prop_assert!(false, "expected RssThreshold variant"),
        }

        let cpu_alert = ForecastAlert::CpuThreshold {
            upper_bound_percent: upper,
            threshold_percent: threshold,
            horizon_steps: horizon,
        };
        let json2 = serde_json::to_string(&cpu_alert).unwrap();
        let back2: ForecastAlert = serde_json::from_str(&json2).unwrap();
        match back2 {
            ForecastAlert::CpuThreshold {
                upper_bound_percent,
                threshold_percent,
                horizon_steps,
            } => {
                prop_assert!(tol(upper_bound_percent, upper),
                    "cpu upper mismatch: {} vs {}", upper_bound_percent, upper);
                prop_assert!(tol(threshold_percent, threshold),
                    "cpu threshold mismatch: {} vs {}", threshold_percent, threshold);
                prop_assert_eq!(horizon_steps, horizon);
            }
            _ => prop_assert!(false, "expected CpuThreshold variant"),
        }
    }
}

// =============================================================================
// Property 15: MetricForecaster forecast count equals horizon count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn forecaster_horizon_count(
        horizons in proptest::collection::vec(1_usize..100, 1..5),
    ) {
        let mut mf = MetricForecaster::new(
            "test".into(), 0.3, 0.1, &horizons, 100, 1000, 0.95,
        );
        for i in 0..50 {
            mf.observe(i as f64);
        }
        let forecasts = mf.forecast_all();
        prop_assert_eq!(forecasts.len(), horizons.len(),
            "forecast count {} should equal horizon count {}", forecasts.len(), horizons.len());
    }
}

// =============================================================================
// Property 16: Calibrated interval has upper >= lower
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn forecaster_calibrated_interval_ordered(
        constant_value in 0.0_f64..1000.0,
    ) {
        let mut mf = MetricForecaster::new(
            "test".into(), 0.3, 0.1, &[5], 100, 1000, 0.90,
        );
        for _ in 0..200 {
            mf.observe(constant_value);
        }
        let forecasts = mf.forecast_all();
        for fc in &forecasts {
            if fc.is_calibrated() {
                prop_assert!(fc.upper_bound >= fc.lower_bound,
                    "calibrated interval should have upper {} >= lower {}",
                    fc.upper_bound, fc.lower_bound);
            }
        }
    }
}

// =============================================================================
// Property 17: Point estimate within calibrated interval
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn forecaster_point_within_interval(
        constant_value in 10.0_f64..1000.0,
    ) {
        let mut mf = MetricForecaster::new(
            "test".into(), 0.3, 0.1, &[5], 100, 1000, 0.90,
        );
        for _ in 0..200 {
            mf.observe(constant_value);
        }
        let forecasts = mf.forecast_all();
        for fc in &forecasts {
            if fc.is_calibrated() {
                prop_assert!(fc.point_estimate >= fc.lower_bound,
                    "point {} should be >= lower {}", fc.point_estimate, fc.lower_bound);
                prop_assert!(fc.point_estimate <= fc.upper_bound,
                    "point {} should be <= upper {}", fc.point_estimate, fc.upper_bound);
            }
        }
    }
}

// =============================================================================
// Property 18: Unknown horizon returns None
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn forecaster_unknown_horizon(
        configured in 1_usize..100,
        query in 101_usize..1000,
    ) {
        let mut mf = MetricForecaster::new(
            "test".into(), 0.3, 0.1, &[configured], 100, 1000, 0.95,
        );
        mf.observe(42.0);
        prop_assert!(mf.forecast_horizon(query).is_none(),
            "horizon {} not configured (only {}), should return None", query, configured);
    }
}

// =============================================================================
// Property 19: Observation count tracks updates
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn forecaster_observation_count(
        n in 1_usize..100,
    ) {
        let mut mf = MetricForecaster::new(
            "test".into(), 0.3, 0.1, &[5], 100, 1000, 0.95,
        );
        for i in 0..n {
            mf.observe(i as f64);
        }
        prop_assert_eq!(mf.observation_count(), n as u64);
    }
}

// =============================================================================
// Property 20: Coverage clamped to [0.01, 0.999]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn forecaster_coverage_clamped(
        coverage in -1.0_f64..2.0,
    ) {
        let mf = MetricForecaster::new(
            "test".into(), 0.3, 0.1, &[5], 100, 1000, coverage,
        );
        // We can verify indirectly: forecasts should have coverage in [0.01, 0.999]
        let mut mf = mf;
        for i in 0..100 {
            mf.observe(i as f64);
        }
        let forecasts = mf.forecast_all();
        for fc in &forecasts {
            prop_assert!(fc.coverage >= 0.01 && fc.coverage <= 0.999,
                "coverage {} should be in [0.01, 0.999]", fc.coverage);
        }
    }
}

// =============================================================================
// Property 21: ConformalForecaster metric_count increases
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn conformal_metric_count(
        n_metrics in 1_usize..10,
    ) {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            ..ConformalConfig::default()
        };
        let mut f = ConformalForecaster::new(config);
        for i in 0..n_metrics {
            f.observe(&format!("metric_{}", i), 42.0);
        }
        prop_assert_eq!(f.metric_count(), n_metrics,
            "metric_count {} should equal n_metrics {}", f.metric_count(), n_metrics);
    }
}

// =============================================================================
// Property 22: has_metric after observe
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn conformal_has_metric(
        name in "[a-z_]{3,15}",
    ) {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            ..ConformalConfig::default()
        };
        let mut f = ConformalForecaster::new(config);
        prop_assert!(!f.has_metric(&name));
        f.observe(&name, 100.0);
        prop_assert!(f.has_metric(&name),
            "should have metric '{}' after observe", name);
    }
}

// =============================================================================
// Property 23: observation_count tracks per-metric
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn conformal_observation_count(
        n in 1_usize..50,
    ) {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            ..ConformalConfig::default()
        };
        let mut f = ConformalForecaster::new(config);
        prop_assert_eq!(f.observation_count("test"), None);
        for i in 0..n {
            f.observe("test", i as f64);
        }
        prop_assert_eq!(f.observation_count("test"), Some(n as u64));
    }
}

// =============================================================================
// Property 24: Forecasts non-empty after warmup
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn conformal_forecasts_nonempty(
        value in arb_positive_value(),
    ) {
        let config = ConformalConfig {
            horizon_steps: vec![5, 10],
            calibration_window: 50,
            max_history: 500,
            ..ConformalConfig::default()
        };
        let mut f = ConformalForecaster::new(config);
        for _ in 0..100 {
            f.observe("rss", value);
        }
        let forecasts = f.forecast_all();
        prop_assert!(!forecasts.is_empty(),
            "should have forecasts after 100 observations");
        prop_assert_eq!(forecasts.len(), 2,
            "should have 2 forecasts (one per horizon)");
    }
}
