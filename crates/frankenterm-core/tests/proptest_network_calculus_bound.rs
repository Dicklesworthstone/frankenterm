use proptest::prelude::*;

use frankenterm_core::network_calculus_bound::{
    backlog_bound, compose_pipeline, compose_serial, delay_bound, is_stable, pipeline_delay_bound,
    ArrivalCurve, EmpiricalComparison, ServiceCurve, StageModel, TOLERANCE_PCT,
};

fn finite_non_negative() -> impl Strategy<Value = f64> {
    0.0f64..=1_000_000.0
}

fn finite_positive() -> impl Strategy<Value = f64> {
    0.000_001f64..=1_000_000.0
}

fn arb_arrival() -> impl Strategy<Value = ArrivalCurve> {
    (finite_non_negative(), finite_non_negative())
        .prop_map(|(burst, rate)| ArrivalCurve::new(burst, rate))
}

fn arb_service() -> impl Strategy<Value = ServiceCurve> {
    (finite_positive(), finite_non_negative())
        .prop_map(|(rate, latency)| ServiceCurve::new(rate, latency))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_network_calculus_constructors_accept_exactly_finite_non_negative_values(
        burst in any::<f64>(),
        arrival_rate in any::<f64>(),
        service_rate in any::<f64>(),
        latency in any::<f64>(),
    ) {
        let arrival = ArrivalCurve::try_new(burst, arrival_rate);
        prop_assert_eq!(
            arrival.is_some(),
            burst.is_finite() && arrival_rate.is_finite() && burst >= 0.0 && arrival_rate >= 0.0,
        );

        let service = ServiceCurve::try_new(service_rate, latency);
        prop_assert_eq!(
            service.is_some(),
            service_rate.is_finite() && latency.is_finite() && service_rate > 0.0 && latency >= 0.0,
        );
    }

    #[test]
    fn proptest_network_calculus_curve_evaluation_is_causal_and_linear(
        arrival in arb_arrival(),
        service in arb_service(),
        t in finite_non_negative(),
        negative_t in -1_000_000.0f64..0.0,
    ) {
        prop_assert_eq!(arrival.evaluate(negative_t), 0.0);
        prop_assert_eq!(service.evaluate(service.latency()), 0.0);

        let arrival_value = arrival.evaluate(t);
        prop_assert!(arrival_value >= arrival.burst());
        prop_assert!((arrival_value - (arrival.burst() + arrival.rate() * t)).abs() <= 1e-6);

        let after_latency = service.latency() + t;
        prop_assert!(
            (service.evaluate(after_latency) - service.rate() * t).abs() <= 1e-6,
        );
    }

    #[test]
    fn proptest_network_calculus_delay_and_backlog_match_lindley_formulas(
        burst in finite_non_negative(),
        arrival_rate in 0.0f64..=999_999.0,
        service_margin in finite_positive(),
        latency in finite_non_negative(),
    ) {
        let service_rate = arrival_rate + service_margin;
        let arrival = ArrivalCurve::new(burst, arrival_rate);
        let service = ServiceCurve::new(service_rate, latency);

        prop_assert!(is_stable(arrival, service));
        prop_assert_eq!(delay_bound(arrival, service), Some(latency + burst / service_rate));
        prop_assert_eq!(backlog_bound(arrival, service), burst + arrival_rate * latency);
    }

    #[test]
    fn proptest_network_calculus_instability_is_rate_threshold_exact(
        burst in finite_non_negative(),
        service_rate in finite_positive(),
        latency in finite_non_negative(),
        excess_rate in finite_non_negative(),
    ) {
        let arrival = ArrivalCurve::new(burst, service_rate + excess_rate);
        let service = ServiceCurve::new(service_rate, latency);

        prop_assert!(!is_stable(arrival, service));
        prop_assert_eq!(delay_bound(arrival, service), None);
        prop_assert!(backlog_bound(arrival, service).is_finite());
    }

    #[test]
    fn proptest_network_calculus_serial_composition_matches_pipeline_delay(
        arrival in arb_arrival(),
        a in arb_service(),
        b in arb_service(),
        c in arb_service(),
    ) {
        let composed_pair = compose_serial(a, b);
        prop_assert_eq!(composed_pair.rate(), a.rate().min(b.rate()));
        prop_assert_eq!(composed_pair.latency(), a.latency() + b.latency());

        let services = [a, b, c];
        let composed = compose_pipeline(&services).expect("non-empty pipeline composes");
        prop_assert_eq!(composed.rate(), a.rate().min(b.rate()).min(c.rate()));
        prop_assert_eq!(composed.latency(), a.latency() + b.latency() + c.latency());

        let stages = vec![
            StageModel::new("capture", a),
            StageModel::new("extract", b),
            StageModel::new("storage", c),
        ];
        prop_assert_eq!(
            pipeline_delay_bound(arrival, &stages),
            delay_bound(arrival, composed),
        );
    }

    #[test]
    fn proptest_network_calculus_empirical_tolerance_uses_symmetric_percent_deviation(
        analytical in finite_positive(),
        pct in 0.0f64..=100.0,
        above in any::<bool>(),
    ) {
        let factor = if above { 1.0 + pct / 100.0 } else { 1.0 - pct / 100.0 };
        let comparison = EmpiricalComparison {
            analytical_bound_ms: analytical,
            empirical_p99_ms: analytical * factor,
        };

        prop_assert!((comparison.deviation_pct().unwrap() - pct).abs() <= 1e-6);
        prop_assert_eq!(comparison.within_tolerance(), pct <= TOLERANCE_PCT);
        prop_assert_eq!(comparison.exceeds_bound(), above && pct > 0.0);
    }
}
