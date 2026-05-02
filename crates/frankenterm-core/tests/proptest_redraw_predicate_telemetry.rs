use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use frankenterm_core::redraw_predicate_telemetry::{
    DecisionRecord, ForcePaintSignal, IdlePaintSkipBenchResult, IdlePaintSkipBenchScenario,
    IdlePaintSkipBenchSnapshot, OsPaintSignalSource, RedrawDecisionHealth, bench_scenario_corpus,
    fold_decision, record_force_paint, record_os_paint_consumption,
};

fn arb_reason_slug() -> impl Strategy<Value = String> {
    "[a-z][a-z_]{0,15}"
}

fn arb_decision() -> impl Strategy<Value = DecisionRecord> {
    prop_oneof![
        Just(DecisionRecord::Skip),
        prop::collection::vec(arb_reason_slug(), 0..=4)
            .prop_map(|reason_slugs| DecisionRecord::Paint { reason_slugs }),
    ]
}

fn arb_os_source() -> impl Strategy<Value = OsPaintSignalSource> {
    prop_oneof![
        Just(OsPaintSignalSource::MacosSetNeedsDisplay),
        Just(OsPaintSignalSource::WaylandFrameCallback),
        Just(OsPaintSignalSource::X11ConfigureNotify),
        Just(OsPaintSignalSource::Synthetic),
    ]
}

fn arb_force_signal() -> impl Strategy<Value = ForcePaintSignal> {
    prop_oneof![
        Just(ForcePaintSignal::DragResize),
        Just(ForcePaintSignal::Bel),
        Just(ForcePaintSignal::AtUpdatePending),
        Just(ForcePaintSignal::CosmeticDeferOutstanding),
    ]
}

fn arb_bench_scenario() -> impl Strategy<Value = IdlePaintSkipBenchScenario> {
    prop_oneof![
        Just(IdlePaintSkipBenchScenario::Idle10s12PaneFleet),
        Just(IdlePaintSkipBenchScenario::TypingCadence1Hz),
        Just(IdlePaintSkipBenchScenario::ForcePaintEveryFrame),
    ]
}

fn expected_pass(scenario: IdlePaintSkipBenchScenario, health: &RedrawDecisionHealth) -> bool {
    match scenario {
        IdlePaintSkipBenchScenario::Idle10s12PaneFleet => health.skip_rate_pct() >= 99.0,
        IdlePaintSkipBenchScenario::TypingCadence1Hz => health.skip_rate_pct() >= 40.0,
        IdlePaintSkipBenchScenario::ForcePaintEveryFrame => {
            health.skips_total == 0 && health.evaluations_total > 0
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_redraw_predicate_slug_tables_are_unique_and_snake_case(
        os_source in arb_os_source(),
        force_signal in arb_force_signal(),
        scenario in arb_bench_scenario(),
    ) {
        let os_slugs: BTreeSet<_> = OsPaintSignalSource::ALL.iter().map(|source| source.slug()).collect();
        prop_assert_eq!(os_slugs.len(), OsPaintSignalSource::ALL.len());
        prop_assert!(os_slugs.contains(os_source.slug()));

        let force_slugs: BTreeSet<_> = ForcePaintSignal::ALL.iter().map(|signal| signal.slug()).collect();
        prop_assert_eq!(force_slugs.len(), ForcePaintSignal::ALL.len());
        prop_assert!(force_slugs.contains(force_signal.slug()));

        let scenario_slugs: BTreeSet<_> = IdlePaintSkipBenchScenario::ALL
            .iter()
            .map(|bench| bench.slug())
            .collect();
        prop_assert_eq!(scenario_slugs.len(), IdlePaintSkipBenchScenario::ALL.len());
        prop_assert!(scenario_slugs.contains(scenario.slug()));

        for slug in os_slugs.into_iter().chain(force_slugs).chain(scenario_slugs) {
            prop_assert!(slug.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit()));
        }
    }

    #[test]
    fn proptest_redraw_predicate_fold_decision_matches_sequence_model(
        decisions in prop::collection::vec(arb_decision(), 0..=128),
    ) {
        let mut health = RedrawDecisionHealth::baseline();
        let mut expected_reasons = BTreeMap::new();
        let mut expected_paints = 0_u64;
        let mut expected_skips = 0_u64;

        for decision in &decisions {
            match decision {
                DecisionRecord::Paint { reason_slugs } => {
                    expected_paints += 1;
                    for slug in reason_slugs {
                        *expected_reasons.entry(slug.clone()).or_insert(0_u64) += 1;
                    }
                }
                DecisionRecord::Skip => {
                    expected_skips += 1;
                }
            }
            fold_decision(&mut health, decision);
        }

        prop_assert_eq!(health.evaluations_total, decisions.len() as u64);
        prop_assert_eq!(health.paints_total, expected_paints);
        prop_assert_eq!(health.skips_total, expected_skips);
        prop_assert_eq!(health.paint_reasons, expected_reasons);
        prop_assert_eq!(health.paints_total + health.skips_total, health.evaluations_total);
    }

    #[test]
    fn proptest_redraw_predicate_health_thresholds_are_exact(
        paints in 0_u64..=10_000,
        skips in 0_u64..=10_000,
        force_signal in prop::option::of(arb_force_signal()),
        os_source in prop::option::of(arb_os_source()),
    ) {
        let mut health = RedrawDecisionHealth {
            evaluations_total: paints + skips,
            paints_total: paints,
            skips_total: skips,
            ..RedrawDecisionHealth::baseline()
        };
        if let Some(signal) = force_signal {
            record_force_paint(&mut health, signal);
        }
        if let Some(source) = os_source {
            record_os_paint_consumption(&mut health, source);
        }

        let expected_skip_rate = if paints + skips == 0 {
            1.0
        } else {
            skips as f64 / (paints + skips) as f64
        };
        prop_assert_eq!(health.skip_rate(), expected_skip_rate);
        prop_assert_eq!(health.meets_idle_skip_rq(), expected_skip_rate * 100.0 >= 99.0);
        prop_assert_eq!(health.meets_typing_cadence_rq(), expected_skip_rate * 100.0 >= 40.0);

        let expected_safe = if paints + skips == 0 {
            force_signal.is_none() && os_source.is_none()
        } else {
            expected_skip_rate * 100.0 >= 99.0
        };
        prop_assert_eq!(health.is_safe(), expected_safe);
    }

    #[test]
    fn proptest_redraw_predicate_signal_recorders_count_by_slug(
        force_signals in prop::collection::vec(arb_force_signal(), 0..=128),
        os_sources in prop::collection::vec(arb_os_source(), 0..=128),
    ) {
        let mut health = RedrawDecisionHealth::baseline();
        let mut expected_force = BTreeMap::new();
        let mut expected_os = BTreeMap::new();

        for signal in &force_signals {
            record_force_paint(&mut health, *signal);
            *expected_force.entry(signal.slug().to_string()).or_insert(0_u64) += 1;
        }
        for source in &os_sources {
            record_os_paint_consumption(&mut health, *source);
            *expected_os.entry(source.slug().to_string()).or_insert(0_u64) += 1;
        }

        prop_assert_eq!(health.force_paint_counters, expected_force);
        prop_assert_eq!(health.os_paint_consumptions, expected_os);
    }

    #[test]
    fn proptest_redraw_predicate_bench_result_and_snapshot_follow_acceptance_contract(
        paints in 0_u64..=10_000,
        skips in 0_u64..=10_000,
        scenario in arb_bench_scenario(),
    ) {
        let health = RedrawDecisionHealth {
            evaluations_total: paints + skips,
            paints_total: paints,
            skips_total: skips,
            ..RedrawDecisionHealth::baseline()
        };
        let result = IdlePaintSkipBenchResult::evaluate(scenario, health.clone());
        prop_assert_eq!(result.passed, expected_pass(scenario, &health));
        prop_assert_eq!(result.final_health, health);
        prop_assert!(result.notes.is_none());

        let mut snapshot = IdlePaintSkipBenchSnapshot::new();
        snapshot.record(result);
        prop_assert_eq!(snapshot.results.len(), 1);
        snapshot.record(IdlePaintSkipBenchResult::evaluate(
            scenario,
            RedrawDecisionHealth::baseline(),
        ));
        prop_assert_eq!(snapshot.results.len(), 1);
        prop_assert_eq!(bench_scenario_corpus(), IdlePaintSkipBenchScenario::ALL.to_vec());
    }
}
