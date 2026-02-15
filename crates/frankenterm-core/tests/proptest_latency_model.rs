//! Property-based tests for latency_model.rs — network calculus and min-plus algebra.
//!
//! Bead: wa-283h4.16

use frankenterm_core::latency_model::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_sigma() -> impl Strategy<Value = f64> {
    0.0f64..1_000_000.0
}

fn arb_rho() -> impl Strategy<Value = f64> {
    0.001f64..100_000.0
}

fn arb_rate() -> impl Strategy<Value = f64> {
    1.0f64..1_000_000_000.0
}

fn arb_latency() -> impl Strategy<Value = f64> {
    0.0f64..1.0
}

fn arb_time() -> impl Strategy<Value = f64> {
    0.0f64..1000.0
}

fn arb_positive_time() -> impl Strategy<Value = f64> {
    0.001f64..100.0
}

fn arb_leaky_bucket() -> impl Strategy<Value = ArrivalCurve> {
    (arb_sigma(), arb_rho()).prop_map(|(s, r)| ArrivalCurve::leaky_bucket(s, r))
}

fn arb_token_bucket() -> impl Strategy<Value = ArrivalCurve> {
    (arb_sigma(), arb_rho(), arb_rate())
        .prop_filter("peak >= sustained", |(_, rho, peak)| *peak >= *rho)
        .prop_map(|(s, r, p)| ArrivalCurve::token_bucket(s, r, p))
}

fn arb_staircase() -> impl Strategy<Value = ArrivalCurve> {
    (0.001f64..10.0, 0.1f64..10000.0).prop_map(|(p, b)| ArrivalCurve::staircase(p, b))
}

fn arb_arrival() -> impl Strategy<Value = ArrivalCurve> {
    prop_oneof![arb_leaky_bucket(), arb_token_bucket(), arb_staircase(),]
}

fn arb_rate_latency() -> impl Strategy<Value = ServiceCurve> {
    (arb_rate(), arb_latency()).prop_map(|(r, l)| ServiceCurve::rate_latency(r, l))
}

fn arb_strict_rate() -> impl Strategy<Value = ServiceCurve> {
    arb_rate().prop_map(ServiceCurve::strict_rate)
}

fn arb_service() -> impl Strategy<Value = ServiceCurve> {
    prop_oneof![arb_rate_latency(), arb_strict_rate(),]
}

fn arb_stable_pair() -> impl Strategy<Value = (ArrivalCurve, ServiceCurve)> {
    (arb_sigma(), arb_rho(), arb_latency())
        .prop_flat_map(|(sigma, rho, latency)| {
            // Ensure rate > rho for stability
            let min_rate = rho * 1.1;
            (
                Just(ArrivalCurve::leaky_bucket(sigma, rho)),
                (min_rate..min_rate * 100.0)
                    .prop_map(move |rate| ServiceCurve::rate_latency(rate, latency)),
            )
        })
}

fn arb_piecewise_linear() -> impl Strategy<Value = PiecewiseLinear> {
    prop::collection::vec((0.0f64..100.0, 0.0f64..1000.0), 1..10).prop_map(|pairs| {
        let points: Vec<CurvePoint> = pairs
            .into_iter()
            .map(|(t, y)| CurvePoint { t, y })
            .collect();
        PiecewiseLinear::new(points)
    })
}

fn arb_pipeline_stage() -> impl Strategy<Value = PipelineStage> {
    (arb_rate(), arb_latency(), "[a-z]{3,8}").prop_map(|(rate, latency, name)| PipelineStage {
        name,
        service: ServiceCurve::rate_latency(rate, latency),
    })
}

// ── PiecewiseLinear properties ──────────────────────────────────────

proptest! {
    /// Evaluating at a breakpoint returns exactly that breakpoint's y-value.
    #[test]
    fn piecewise_eval_at_breakpoints(pw in arb_piecewise_linear()) {
        for p in pw.points() {
            let eval = pw.eval(p.t);
            prop_assert!(
                (eval - p.y).abs() < 1e-6,
                "at t={}, expected {}, got {}", p.t, p.y, eval
            );
        }
    }

    /// Constant curve returns the same value everywhere.
    #[test]
    fn constant_curve_uniform(c in -1e6f64..1e6, t in arb_time()) {
        let curve = PiecewiseLinear::constant(c);
        prop_assert!(
            (curve.eval(t) - c).abs() < 1e-9,
            "constant({}) at t={} gave {}", c, t, curve.eval(t)
        );
    }

    /// Linear curve: eval(t) = intercept + slope * t for t ≥ 0.
    #[test]
    fn linear_curve_formula(
        intercept in -1000.0f64..1000.0,
        slope in -100.0f64..100.0,
        t in arb_positive_time()
    ) {
        let curve = PiecewiseLinear::linear(intercept, slope);
        let expected = intercept + slope * t;
        let actual = curve.eval(t);
        prop_assert!(
            (actual - expected).abs() < 1e-4,
            "linear({}, {}) at t={}: expected {}, got {}", intercept, slope, t, expected, actual
        );
    }

    /// Trailing slope is consistent with last two points.
    #[test]
    fn trailing_slope_consistency(pw in arb_piecewise_linear()) {
        let pts = pw.points();
        if pts.len() >= 2 {
            let p0 = &pts[pts.len() - 2];
            let p1 = &pts[pts.len() - 1];
            let dt = p1.t - p0.t;
            if dt.abs() > 1e-12 {
                let expected_slope = (p1.y - p0.y) / dt;
                let actual = pw.trailing_slope();
                prop_assert!(
                    (actual - expected_slope).abs() < 1e-6,
                    "trailing slope: expected {}, got {}", expected_slope, actual
                );
            }
        }
    }

    /// Piecewise-linear never panics for any f64 input.
    #[test]
    fn piecewise_eval_no_panic(pw in arb_piecewise_linear(), t in -1e6f64..1e6) {
        let _ = pw.eval(t);
    }

    /// Serde roundtrip for PiecewiseLinear.
    #[test]
    fn piecewise_serde_roundtrip(pw in arb_piecewise_linear()) {
        let json = serde_json::to_string(&pw).unwrap();
        let back: PiecewiseLinear = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(pw.len(), back.len());
        for (a, b) in pw.points().iter().zip(back.points().iter()) {
            prop_assert!((a.t - b.t).abs() < 1e-12);
            prop_assert!((a.y - b.y).abs() < 1e-12);
        }
    }
}

// ── Arrival curve properties ────────────────────────────────────────

proptest! {
    /// Arrival curves are non-negative for t > 0.
    #[test]
    fn arrival_non_negative(arr in arb_arrival(), t in arb_positive_time()) {
        let val = arr.eval(t);
        prop_assert!(val >= -1e-9, "arrival({}) = {} < 0", t, val);
    }

    /// Arrival curves return 0 at t = 0.
    #[test]
    fn arrival_zero_at_origin(arr in arb_arrival()) {
        let val = arr.eval(0.0);
        prop_assert!(
            val.abs() < 1e-9,
            "arrival(0) = {} (expected 0)", val
        );
    }

    /// Leaky bucket is superadditive: α(s+t) ≥ α(s) for all s, t ≥ 0.
    /// (arrival curves are non-decreasing)
    #[test]
    fn arrival_monotone(
        arr in arb_leaky_bucket(),
        s in arb_positive_time(),
        dt in arb_positive_time()
    ) {
        let t = s + dt;
        prop_assert!(
            arr.eval(t) >= arr.eval(s) - 1e-9,
            "α({}) = {} < α({}) = {}", t, arr.eval(t), s, arr.eval(s)
        );
    }

    /// Sustained rate matches the slope at large t.
    #[test]
    fn sustained_rate_matches_slope(sigma in arb_sigma(), rho in arb_rho()) {
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let t1 = 1_000.0;
        let t2 = 2_000.0;
        let empirical_rate = (arr.eval(t2) - arr.eval(t1)) / (t2 - t1);
        prop_assert!(
            (empirical_rate - rho).abs() < 1e-3,
            "sustained rate: expected {}, empirical {}", rho, empirical_rate
        );
    }

    /// Token bucket is always ≤ leaky bucket with same σ, ρ.
    #[test]
    fn token_bucket_le_leaky(
        sigma in arb_sigma(),
        rho in arb_rho(),
        peak in arb_rate(),
        t in arb_positive_time()
    ) {
        prop_assume!(peak >= rho);
        let lb = ArrivalCurve::leaky_bucket(sigma, rho);
        let tb = ArrivalCurve::token_bucket(sigma, rho, peak);
        prop_assert!(
            tb.eval(t) <= lb.eval(t) + 1e-6,
            "token_bucket({}) = {} > leaky_bucket({}) = {}",
            t, tb.eval(t), t, lb.eval(t)
        );
    }

    /// Arrival curve serde roundtrip preserves values within float precision.
    #[test]
    fn arrival_serde_roundtrip(arr in arb_leaky_bucket()) {
        let json = serde_json::to_string(&arr).unwrap();
        let back: ArrivalCurve = serde_json::from_str(&json).unwrap();
        // Compare at sample points (JSON float precision can differ at ~15th digit)
        for t in [0.0, 0.1, 1.0, 10.0, 100.0] {
            let orig_val = arr.eval(t);
            let back_val = back.eval(t);
            let diff = (orig_val - back_val).abs();
            prop_assert!(
                diff < 1e-6 || diff / orig_val.abs().max(1.0) < 1e-10,
                "serde roundtrip diverged at t={}: {} vs {}", t, orig_val, back_val
            );
        }
    }
}

// ── Service curve properties ────────────────────────────────────────

proptest! {
    /// Service curves are non-negative for all t.
    #[test]
    fn service_non_negative(svc in arb_service(), t in arb_time()) {
        let val = svc.eval(t);
        prop_assert!(val >= -1e-9, "β({}) = {} < 0", t, val);
    }

    /// Service curves return 0 at t = 0.
    #[test]
    fn service_zero_at_origin(svc in arb_service()) {
        let val = svc.eval(0.0);
        prop_assert!(val.abs() < 1e-9, "β(0) = {}", val);
    }

    /// Service curves are non-decreasing (for non-negative t).
    #[test]
    fn service_monotone(svc in arb_service(), s in arb_positive_time(), dt in arb_positive_time()) {
        let t = s + dt;
        prop_assert!(
            svc.eval(t) >= svc.eval(s) - 1e-9,
            "β({}) = {} < β({}) = {}", t, svc.eval(t), s, svc.eval(s)
        );
    }

    /// Rate-latency: β(t) = 0 for t ≤ T.
    #[test]
    fn rate_latency_zero_during_latency(rate in arb_rate(), latency in 0.001f64..0.5) {
        let svc = ServiceCurve::rate_latency(rate, latency);
        let t = latency * 0.5;
        prop_assert!(
            svc.eval(t).abs() < 1e-9,
            "β({}) = {} (expected 0 during latency {})", t, svc.eval(t), latency
        );
    }

    /// Service curve serde roundtrip preserves values within float precision.
    #[test]
    fn service_serde_roundtrip(svc in arb_rate_latency()) {
        let json = serde_json::to_string(&svc).unwrap();
        let back: ServiceCurve = serde_json::from_str(&json).unwrap();
        // Compare at sample points (JSON float precision can differ at ~15th digit)
        for t in [0.0, 0.01, 0.1, 1.0, 10.0] {
            let orig_val = svc.eval(t);
            let back_val = back.eval(t);
            let diff = (orig_val - back_val).abs();
            prop_assert!(
                diff < 1e-6 || diff / orig_val.abs().max(1.0) < 1e-10,
                "serde roundtrip diverged at t={}: {} vs {}", t, orig_val, back_val
            );
        }
    }
}

// ── Min-plus convolution properties ─────────────────────────────────

proptest! {
    /// Convolution of rate-latency curves: rate = min(r1, r2), latency = t1 + t2.
    #[test]
    fn convolution_rate_latency_formula(
        r1 in arb_rate(),
        t1 in arb_latency(),
        r2 in arb_rate(),
        t2 in arb_latency()
    ) {
        let a = ServiceCurve::rate_latency(r1, t1);
        let b = ServiceCurve::rate_latency(r2, t2);
        let c = min_plus_convolution(&a, &b);
        let expected_rate = r1.min(r2);
        let expected_latency = t1 + t2;
        prop_assert!(
            (c.rate() - expected_rate).abs() < 1e-6,
            "rate: expected {}, got {}", expected_rate, c.rate()
        );
        prop_assert!(
            (c.latency() - expected_latency).abs() < 1e-9,
            "latency: expected {}, got {}", expected_latency, c.latency()
        );
    }

    /// Convolution is commutative: a ⊗ b = b ⊗ a (for rate-latency curves).
    #[test]
    fn convolution_commutative(
        r1 in arb_rate(),
        t1 in arb_latency(),
        r2 in arb_rate(),
        t2 in arb_latency()
    ) {
        let a = ServiceCurve::rate_latency(r1, t1);
        let b = ServiceCurve::rate_latency(r2, t2);
        let ab = min_plus_convolution(&a, &b);
        let ba = min_plus_convolution(&b, &a);
        prop_assert!(
            (ab.rate() - ba.rate()).abs() < 1e-6,
            "commutativity violated: rates {} vs {}", ab.rate(), ba.rate()
        );
        prop_assert!(
            (ab.latency() - ba.latency()).abs() < 1e-9,
            "commutativity violated: latencies {} vs {}", ab.latency(), ba.latency()
        );
    }

    /// Convolution is associative: (a ⊗ b) ⊗ c = a ⊗ (b ⊗ c).
    #[test]
    fn convolution_associative(
        r1 in arb_rate(), t1 in arb_latency(),
        r2 in arb_rate(), t2 in arb_latency(),
        r3 in arb_rate(), t3 in arb_latency()
    ) {
        let a = ServiceCurve::rate_latency(r1, t1);
        let b = ServiceCurve::rate_latency(r2, t2);
        let c = ServiceCurve::rate_latency(r3, t3);
        let ab_c = min_plus_convolution(&min_plus_convolution(&a, &b), &c);
        let a_bc = min_plus_convolution(&a, &min_plus_convolution(&b, &c));
        prop_assert!(
            (ab_c.rate() - a_bc.rate()).abs() < 1e-6,
            "associativity violated: rates {} vs {}", ab_c.rate(), a_bc.rate()
        );
        prop_assert!(
            (ab_c.latency() - a_bc.latency()).abs() < 1e-9,
            "associativity violated: latencies {} vs {}", ab_c.latency(), a_bc.latency()
        );
    }

    /// Convolution with strict-rate(∞) is identity.
    #[test]
    fn convolution_identity(rate in arb_rate(), latency in arb_latency()) {
        let a = ServiceCurve::rate_latency(rate, latency);
        let id = ServiceCurve::strict_rate(f64::INFINITY);
        let result = min_plus_convolution(&a, &id);
        // Should approximate original
        prop_assert!(
            (result.rate() - rate).abs() < 1e-6 || result.rate() >= rate,
            "identity: rate {} -> {}", rate, result.rate()
        );
    }

    /// Adding stages never increases the rate.
    #[test]
    fn convolution_rate_decreases(
        r1 in arb_rate(), t1 in arb_latency(),
        r2 in arb_rate(), t2 in arb_latency()
    ) {
        let a = ServiceCurve::rate_latency(r1, t1);
        let b = ServiceCurve::rate_latency(r2, t2);
        let c = min_plus_convolution(&a, &b);
        prop_assert!(
            c.rate() <= r1.min(r2) + 1e-6,
            "rate increased: {} > min({}, {})", c.rate(), r1, r2
        );
    }

    /// Adding stages never decreases the latency.
    #[test]
    fn convolution_latency_increases(
        r1 in arb_rate(), t1 in arb_latency(),
        r2 in arb_rate(), t2 in arb_latency()
    ) {
        let a = ServiceCurve::rate_latency(r1, t1);
        let b = ServiceCurve::rate_latency(r2, t2);
        let c = min_plus_convolution(&a, &b);
        prop_assert!(
            c.latency() >= t1 + t2 - 1e-9,
            "latency decreased: {} < {} + {}", c.latency(), t1, t2
        );
    }
}

// ── Delay bound properties ──────────────────────────────────────────

proptest! {
    /// Delay bound is non-negative for stable systems.
    #[test]
    fn delay_bound_non_negative((arr, svc) in arb_stable_pair()) {
        let d = delay_bound(&arr, &svc);
        prop_assert!(d >= -1e-9, "delay bound = {} < 0", d);
    }

    /// Delay bound is infinite for unstable systems (ρ > R).
    #[test]
    fn delay_bound_infinite_unstable(
        sigma in arb_sigma(),
        rho in 100.0f64..1000.0,
        latency in arb_latency()
    ) {
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::rate_latency(rho * 0.5, latency);
        let d = delay_bound(&arr, &svc);
        prop_assert!(d.is_infinite(), "expected infinite delay, got {}", d);
    }

    /// Increasing service rate decreases delay bound.
    #[test]
    fn delay_monotone_in_rate(
        sigma in arb_sigma(),
        rho in arb_rho(),
        latency in arb_latency(),
        rate_factor in 1.5f64..10.0
    ) {
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let slow_rate = rho * 2.0;
        let fast_rate = slow_rate * rate_factor;
        let d_slow = delay_bound(&arr, &ServiceCurve::rate_latency(slow_rate, latency));
        let d_fast = delay_bound(&arr, &ServiceCurve::rate_latency(fast_rate, latency));
        prop_assert!(
            d_fast <= d_slow + 1e-9,
            "faster rate gave higher delay: {} > {}", d_fast, d_slow
        );
    }

    /// Increasing burst increases delay bound.
    #[test]
    fn delay_monotone_in_burst(
        rho in arb_rho(),
        latency in arb_latency(),
        rate_mult in 2.0f64..100.0,
        sigma1 in 1.0f64..10000.0,
        sigma_extra in 1.0f64..10000.0
    ) {
        let rate = rho * rate_mult;
        let sigma2 = sigma1 + sigma_extra;
        let svc = ServiceCurve::rate_latency(rate, latency);
        let d1 = delay_bound(&ArrivalCurve::leaky_bucket(sigma1, rho), &svc);
        let d2 = delay_bound(&ArrivalCurve::leaky_bucket(sigma2, rho), &svc);
        prop_assert!(
            d2 >= d1 - 1e-9,
            "more burst gave less delay: {} < {}", d2, d1
        );
    }

    /// Closed-form delay: D = σ/(R-ρ) + T.
    #[test]
    fn delay_bound_closed_form(
        sigma in arb_sigma(),
        rho in arb_rho(),
        latency in arb_latency(),
        rate_mult in 1.5f64..100.0
    ) {
        let rate = rho * rate_mult;
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::rate_latency(rate, latency);
        let d = delay_bound(&arr, &svc);
        let expected = sigma / (rate - rho) + latency;
        prop_assert!(
            (d - expected).abs() < 1e-9,
            "closed form: expected {}, got {}", expected, d
        );
    }
}

// ── Backlog bound properties ────────────────────────────────────────

proptest! {
    /// Backlog bound is non-negative for stable systems.
    #[test]
    fn backlog_bound_non_negative((arr, svc) in arb_stable_pair()) {
        let b = backlog_bound(&arr, &svc);
        prop_assert!(b >= -1e-9, "backlog bound = {} < 0", b);
    }

    /// Backlog bound is infinite for unstable systems.
    #[test]
    fn backlog_bound_infinite_unstable(
        sigma in arb_sigma(),
        rho in 100.0f64..1000.0,
        latency in arb_latency()
    ) {
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::rate_latency(rho * 0.5, latency);
        let b = backlog_bound(&arr, &svc);
        prop_assert!(b.is_infinite(), "expected infinite backlog, got {}", b);
    }

    /// Closed-form backlog: B = σ + ρ·T.
    #[test]
    fn backlog_bound_closed_form(
        sigma in arb_sigma(),
        rho in arb_rho(),
        latency in arb_latency(),
        rate_mult in 1.5f64..100.0
    ) {
        let rate = rho * rate_mult;
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::rate_latency(rate, latency);
        let b = backlog_bound(&arr, &svc);
        let expected = sigma + rho * latency;
        prop_assert!(
            (b - expected).abs() < 1e-6,
            "closed form: expected {}, got {}", expected, b
        );
    }

    /// Backlog for strict-rate service is just the burst.
    #[test]
    fn backlog_strict_rate_is_burst(sigma in arb_sigma(), rho in arb_rho(), rate_mult in 1.5f64..100.0) {
        let rate = rho * rate_mult;
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::strict_rate(rate);
        let b = backlog_bound(&arr, &svc);
        prop_assert!(
            (b - sigma).abs() < 1e-6,
            "strict rate backlog: expected {}, got {}", sigma, b
        );
    }

    /// Increasing service rate does not increase backlog.
    #[test]
    fn backlog_monotone_in_rate(
        sigma in arb_sigma(),
        rho in arb_rho(),
        latency in arb_latency()
    ) {
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let rate1 = rho * 2.0;
        let rate2 = rho * 10.0;
        let b1 = backlog_bound(&arr, &ServiceCurve::rate_latency(rate1, latency));
        let b2 = backlog_bound(&arr, &ServiceCurve::rate_latency(rate2, latency));
        // Both are σ + ρ·T for leaky bucket, so rate doesn't matter
        prop_assert!(
            (b1 - b2).abs() < 1e-6,
            "backlog changed with rate: {} vs {}", b1, b2
        );
    }
}

// ── Pipeline properties ─────────────────────────────────────────────

proptest! {
    /// Pipeline with more stages has higher or equal delay.
    #[test]
    fn pipeline_delay_increases_with_stages(
        stages in prop::collection::vec(arb_pipeline_stage(), 1..6),
        sigma in arb_sigma(),
        rho in arb_rho()
    ) {
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let mut prev_delay = 0.0f64;

        for n in 1..=stages.len() {
            let pipeline = Pipeline::new(stages[..n].to_vec());
            let total = pipeline.total_service_curve();
            if total.rate() > rho {
                let d = delay_bound(&arr, &total);
                if d.is_finite() && prev_delay.is_finite() {
                    prop_assert!(
                        d >= prev_delay - 1e-6,
                        "adding stage {} decreased delay: {} -> {}", n, prev_delay, d
                    );
                }
                prev_delay = d;
            }
        }
    }

    /// Pipeline concatenation: total latency = sum of stage latencies.
    #[test]
    fn pipeline_total_latency_sum(stages in prop::collection::vec(arb_pipeline_stage(), 1..8)) {
        let pipeline = Pipeline::new(stages.clone());
        let total = pipeline.total_service_curve();
        let expected_latency: f64 = stages.iter().map(|s| s.service.latency()).sum();
        prop_assert!(
            (total.latency() - expected_latency).abs() < 1e-9,
            "total latency {} != sum {}", total.latency(), expected_latency
        );
    }

    /// Pipeline concatenation: total rate = min of stage rates.
    #[test]
    fn pipeline_total_rate_min(stages in prop::collection::vec(arb_pipeline_stage(), 1..8)) {
        let pipeline = Pipeline::new(stages.clone());
        let total = pipeline.total_service_curve();
        let expected_rate: f64 = stages
            .iter()
            .map(|s| s.service.rate())
            .fold(f64::INFINITY, f64::min);
        prop_assert!(
            (total.rate() - expected_rate).abs() < 1e-6,
            "total rate {} != min {}", total.rate(), expected_rate
        );
    }

    /// Pipeline analysis stage count matches.
    #[test]
    fn pipeline_analysis_stage_count(
        stages in prop::collection::vec(arb_pipeline_stage(), 1..6),
        sigma in arb_sigma(),
        rho in arb_rho()
    ) {
        let pipeline = Pipeline::new(stages.clone());
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let analysis = pipeline.analyze(&arr);
        prop_assert_eq!(analysis.per_stage_delays.len(), stages.len());
        prop_assert_eq!(analysis.per_stage_backlogs.len(), stages.len());
    }
}

// ── Aggregate multiplexing properties ───────────────────────────────

proptest! {
    /// Aggregate of N leaky buckets: σ_agg = Σσ, ρ_agg = Σρ.
    #[test]
    fn aggregate_leaky_bucket_sum(
        params in prop::collection::vec((arb_sigma(), arb_rho()), 1..20)
    ) {
        let arrivals: Vec<ArrivalCurve> = params
            .iter()
            .map(|(s, r)| ArrivalCurve::leaky_bucket(*s, *r))
            .collect();
        let agg = aggregate_arrival(&arrivals);
        let expected_sigma: f64 = params.iter().map(|(s, _)| s).sum();
        let expected_rho: f64 = params.iter().map(|(_, r)| r).sum();
        prop_assert!(
            (agg.burst() - expected_sigma).abs() < 1e-6,
            "sigma: expected {}, got {}", expected_sigma, agg.burst()
        );
        prop_assert!(
            (agg.sustained_rate() - expected_rho).abs() < 1e-6,
            "rho: expected {}, got {}", expected_rho, agg.sustained_rate()
        );
    }

    /// Adding panes increases the aggregate arrival (monotonicity).
    #[test]
    fn aggregate_monotone_in_pane_count(
        sigma in arb_sigma(),
        rho in arb_rho(),
        n in 1usize..20
    ) {
        let one = vec![ArrivalCurve::leaky_bucket(sigma, rho)];
        let many: Vec<_> = (0..n).map(|_| ArrivalCurve::leaky_bucket(sigma, rho)).collect();
        let agg_one = aggregate_arrival(&one);
        let agg_many = aggregate_arrival(&many);
        // more panes → higher aggregate at any t > 0
        let t = 1.0;
        prop_assert!(
            agg_many.eval(t) >= agg_one.eval(t) - 1e-9,
            "more panes gave lower arrival: {} < {}", agg_many.eval(t), agg_one.eval(t)
        );
    }

    /// Multiplexed delay scales with pane count.
    #[test]
    fn multiplexed_delay_scales(
        sigma in 100.0f64..10000.0,
        rho in 10.0f64..1000.0,
        latency in 0.001f64..0.01
    ) {
        let rate = 1_000_000.0;
        let svc = ServiceCurve::rate_latency(rate, latency);
        let n1 = 10;
        let n2 = 50;
        let panes1: Vec<_> = (0..n1).map(|_| ArrivalCurve::leaky_bucket(sigma, rho)).collect();
        let panes2: Vec<_> = (0..n2).map(|_| ArrivalCurve::leaky_bucket(sigma, rho)).collect();
        let d1 = multiplexed_delay_bound(&panes1, &svc);
        let d2 = multiplexed_delay_bound(&panes2, &svc);
        prop_assert!(
            d2 >= d1 - 1e-9,
            "more panes gave less delay: {} < {}", d2, d1
        );
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// Delay bound ≥ service latency (delay can't be less than processing time).
    #[test]
    fn delay_ge_latency((arr, svc) in arb_stable_pair()) {
        let d = delay_bound(&arr, &svc);
        let t = svc.latency();
        if d.is_finite() {
            prop_assert!(
                d >= t - 1e-9,
                "delay {} < latency {}", d, t
            );
        }
    }

    /// Backlog bound ≥ burst size.
    #[test]
    fn backlog_ge_burst(
        sigma in arb_sigma(),
        rho in arb_rho(),
        latency in arb_latency(),
        rate_mult in 1.5f64..100.0
    ) {
        let rate = rho * rate_mult;
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::rate_latency(rate, latency);
        let b = backlog_bound(&arr, &svc);
        prop_assert!(
            b >= sigma - 1e-6,
            "backlog {} < burst {}", b, sigma
        );
    }

    /// For leaky-bucket + rate-latency: delay * (R - ρ) ≈ σ + (R - ρ) * T
    /// i.e., D·(R-ρ) = σ + T·(R-ρ) → delay = σ/(R-ρ) + T
    #[test]
    fn delay_backlog_relationship(
        sigma in arb_sigma(),
        rho in arb_rho(),
        latency in arb_latency(),
        rate_mult in 2.0f64..50.0
    ) {
        let rate = rho * rate_mult;
        let arr = ArrivalCurve::leaky_bucket(sigma, rho);
        let svc = ServiceCurve::rate_latency(rate, latency);
        let d = delay_bound(&arr, &svc);
        let b = backlog_bound(&arr, &svc);
        // backlog = σ + ρ·T, delay = σ/(R-ρ) + T
        // backlog ≤ arrival_at_delay_bound: α(D) = σ + ρ·D ≥ B
        let alpha_d = sigma + rho * d;
        prop_assert!(
            alpha_d >= b - 1e-6,
            "α(D) = {} < B = {}", alpha_d, b
        );
    }
}

// ── FrankenTerm analysis properties ─────────────────────────────────

proptest! {
    /// Analysis always returns 3 stages for the standard pipeline.
    #[test]
    fn frankenterm_analysis_three_stages(
        n_panes in 1usize..100,
        burst in 100.0f64..100000.0,
        rate in 10.0f64..10000.0
    ) {
        let profiles: Vec<_> = (0..n_panes)
            .map(|_| PaneOutputProfile {
                burst_bytes: burst,
                sustained_rate_bps: rate,
            })
            .collect();
        let config = PipelineConfig {
            capture_rate_bps: 100_000_000.0,
            capture_latency_s: 0.001,
            process_rate_bps: 50_000_000.0,
            process_latency_s: 0.002,
            storage_rate_bps: 10_000_000.0,
            storage_latency_s: 0.003,
        };
        let analysis = analyze_frankenterm_pipeline(&profiles, &config);
        prop_assert_eq!(analysis.stages.len(), 3);
    }

    /// Analysis detects instability when load exceeds capacity.
    #[test]
    fn frankenterm_detects_instability(
        rate_per_pane in 10000.0f64..100000.0
    ) {
        let n_panes = 200;
        let profiles: Vec<_> = (0..n_panes)
            .map(|_| PaneOutputProfile {
                burst_bytes: 10000.0,
                sustained_rate_bps: rate_per_pane,
            })
            .collect();
        let config = PipelineConfig {
            capture_rate_bps: 1_000_000.0,
            capture_latency_s: 0.01,
            process_rate_bps: 1_000_000.0,
            process_latency_s: 0.01,
            storage_rate_bps: 1_000_000.0,
            storage_latency_s: 0.01,
        };
        let analysis = analyze_frankenterm_pipeline(&profiles, &config);
        // 200 panes × 10KB/s = 2MB/s > 1MB/s capacity
        prop_assert!(!analysis.is_stable, "should be unstable");
    }
}
