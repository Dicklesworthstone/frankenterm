//! Property-based tests for latency_stages budget algebra invariants.
//!
//! AARSP bead: ft-2p9cb.1.1.1 (verification matrix, property tests)

use frankenterm_core::latency_stages::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_stage() -> impl Strategy<Value = LatencyStage> {
    prop_oneof![
        Just(LatencyStage::PtyCapture),
        Just(LatencyStage::DeltaExtraction),
        Just(LatencyStage::StorageWrite),
        Just(LatencyStage::PatternDetection),
        Just(LatencyStage::EventEmission),
        Just(LatencyStage::WorkflowDispatch),
        Just(LatencyStage::ActionExecution),
        Just(LatencyStage::ApiResponse),
    ]
}

fn arb_percentile() -> impl Strategy<Value = Percentile> {
    prop_oneof![
        Just(Percentile::P50),
        Just(Percentile::P95),
        Just(Percentile::P99),
        Just(Percentile::P999),
    ]
}

/// Generate a valid (monotonic, non-negative) set of percentile targets.
fn arb_monotonic_targets() -> impl Strategy<Value = (f64, f64, f64, f64)> {
    (1.0..1_000_000.0_f64).prop_flat_map(|base| {
        let p50 = base;
        (Just(p50), p50..=(p50 * 10.0)).prop_flat_map(move |(p50, p95)| {
            (Just(p50), Just(p95), p95..=(p95 * 10.0)).prop_flat_map(
                move |(p50, p95, p99)| {
                    (Just(p50), Just(p95), Just(p99), p99..=(p99 * 10.0))
                },
            )
        })
    })
}

fn arb_stage_budget() -> impl Strategy<Value = StageBudget> {
    (arb_stage(), arb_monotonic_targets()).prop_map(|(stage, (p50, p95, p99, p999))| {
        StageBudget::new(stage, p50, p95, p99, p999).unwrap()
    })
}

fn arb_probability() -> impl Strategy<Value = f64> {
    0.0..=1.0_f64
}

fn arb_mitigation() -> impl Strategy<Value = Mitigation> {
    prop_oneof![
        Just(Mitigation::Skip),
        Just(Mitigation::Degrade),
        Just(Mitigation::Shed),
        Just(Mitigation::Defer),
        Just(Mitigation::None),
    ]
}

// ── Budget Construction Invariants ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Valid monotonic targets always construct successfully.
    #[test]
    fn budget_construction_valid(
        stage in arb_stage(),
        targets in arb_monotonic_targets(),
    ) {
        let (p50, p95, p99, p999) = targets;
        let result = StageBudget::new(stage, p50, p95, p99, p999);
        prop_assert!(result.is_ok(), "valid targets should construct: {:?}", targets);
        let b = result.unwrap();
        prop_assert!(b.p50_us <= b.p95_us);
        prop_assert!(b.p95_us <= b.p99_us);
        prop_assert!(b.p99_us <= b.p999_us);
    }

    /// Negative targets always fail.
    #[test]
    fn budget_rejects_negative(
        stage in arb_stage(),
        neg in -1_000_000.0..-0.001_f64,
    ) {
        let result = StageBudget::new(stage, neg, 100.0, 200.0, 300.0);
        let is_negative_err = matches!(result, Err(BudgetError::NegativeTarget { .. }));
        prop_assert!(is_negative_err, "negative target should fail: {:?}", result);
    }

    /// Non-monotonic targets fail with NonMonotonic error.
    #[test]
    fn budget_rejects_nonmonotonic(
        stage in arb_stage(),
        p50 in 100.0..1000.0_f64,
        delta in 1.0..100.0_f64,
    ) {
        // p95 < p50 (violates monotonicity)
        let result = StageBudget::new(stage, p50, p50 - delta, p50 + 100.0, p50 + 200.0);
        let is_nonmono = matches!(result, Err(BudgetError::NonMonotonic { .. }));
        prop_assert!(is_nonmono, "non-monotonic should fail: {:?}", result);
    }

    /// target() returns the correct value for each percentile.
    #[test]
    fn budget_target_correct(budget in arb_stage_budget(), pctl in arb_percentile()) {
        let expected = match pctl {
            Percentile::P50 => budget.p50_us,
            Percentile::P95 => budget.p95_us,
            Percentile::P99 => budget.p99_us,
            Percentile::P999 => budget.p999_us,
        };
        prop_assert!((budget.target(pctl) - expected).abs() < 1e-10);
    }

    /// exceeds() is true iff observed > target.
    #[test]
    fn budget_exceeds_correct(
        budget in arb_stage_budget(),
        pctl in arb_percentile(),
        observed in 0.0..2_000_000.0_f64,
    ) {
        let target = budget.target(pctl);
        let result = budget.exceeds(pctl, observed);
        prop_assert_eq!(result, observed > target);
    }
}

// ── Sequential Composition ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Sequential composition is additive: Seq(A, B) = A + B at each percentile.
    #[test]
    fn sequential_is_additive(
        a in arb_stage_budget(),
        b in arb_stage_budget(),
        pctl in arb_percentile(),
    ) {
        let seq = BudgetNode::Seq(vec![BudgetNode::Leaf(a), BudgetNode::Leaf(b)]);
        let expected = a.target(pctl) + b.target(pctl);
        let actual = seq.aggregate(pctl);
        prop_assert!((actual - expected).abs() < 1e-6,
            "seq aggregate {:.6} != sum {:.6}", actual, expected);
    }

    /// Sequential composition of N children sums all.
    #[test]
    fn sequential_n_children_additive(
        budgets in prop::collection::vec(arb_stage_budget(), 1..=8),
        pctl in arb_percentile(),
    ) {
        let nodes: Vec<BudgetNode> = budgets.iter().map(|b| BudgetNode::Leaf(*b)).collect();
        let seq = BudgetNode::Seq(nodes);
        let expected: f64 = budgets.iter().map(|b| b.target(pctl)).sum();
        let actual = seq.aggregate(pctl);
        prop_assert!((actual - expected).abs() < 1e-3,
            "seq {}-child aggregate {:.3} != sum {:.3}", budgets.len(), actual, expected);
    }

    /// Empty Seq aggregates to 0.
    #[test]
    fn sequential_empty_is_zero(pctl in arb_percentile()) {
        let seq = BudgetNode::Seq(vec![]);
        prop_assert!((seq.aggregate(pctl) - 0.0).abs() < 1e-10);
    }
}

// ── Parallel Composition ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Parallel composition takes max: Par(A, B) = max(A, B) at each percentile.
    #[test]
    fn parallel_is_max(
        a in arb_stage_budget(),
        b in arb_stage_budget(),
        pctl in arb_percentile(),
    ) {
        let par = BudgetNode::Par(vec![BudgetNode::Leaf(a), BudgetNode::Leaf(b)]);
        let expected = a.target(pctl).max(b.target(pctl));
        let actual = par.aggregate(pctl);
        prop_assert!((actual - expected).abs() < 1e-6,
            "par aggregate {:.6} != max {:.6}", actual, expected);
    }

    /// Parallel of N children = max of all.
    #[test]
    fn parallel_n_children_max(
        budgets in prop::collection::vec(arb_stage_budget(), 1..=8),
        pctl in arb_percentile(),
    ) {
        let nodes: Vec<BudgetNode> = budgets.iter().map(|b| BudgetNode::Leaf(*b)).collect();
        let par = BudgetNode::Par(nodes);
        let expected = budgets.iter().map(|b| b.target(pctl)).fold(0.0_f64, f64::max);
        let actual = par.aggregate(pctl);
        prop_assert!((actual - expected).abs() < 1e-3,
            "par {}-child aggregate {:.3} != max {:.3}", budgets.len(), actual, expected);
    }
}

// ── Conditional Composition ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Conditional = p*then + (1-p)*else.
    #[test]
    fn conditional_is_weighted(
        then_b in arb_stage_budget(),
        else_b in arb_stage_budget(),
        p in arb_probability(),
        pctl in arb_percentile(),
    ) {
        let cond = BudgetNode::Cond {
            probability: p,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: Some(Box::new(BudgetNode::Leaf(else_b))),
        };
        let expected = p * then_b.target(pctl) + (1.0 - p) * else_b.target(pctl);
        let actual = cond.aggregate(pctl);
        prop_assert!((actual - expected).abs() < 1e-3,
            "cond aggregate {:.3} != weighted {:.3}", actual, expected);
    }

    /// Conditional without else = p*then.
    #[test]
    fn conditional_no_else(
        then_b in arb_stage_budget(),
        p in arb_probability(),
        pctl in arb_percentile(),
    ) {
        let cond = BudgetNode::Cond {
            probability: p,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: None,
        };
        let expected = p * then_b.target(pctl);
        let actual = cond.aggregate(pctl);
        prop_assert!((actual - expected).abs() < 1e-3,
            "cond-no-else aggregate {:.3} != p*then {:.3}", actual, expected);
    }

    /// p=1.0 => aggregate = then, p=0.0 => aggregate = else.
    #[test]
    fn conditional_boundary_probabilities(
        then_b in arb_stage_budget(),
        else_b in arb_stage_budget(),
        pctl in arb_percentile(),
    ) {
        let cond_1 = BudgetNode::Cond {
            probability: 1.0,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: Some(Box::new(BudgetNode::Leaf(else_b))),
        };
        let cond_0 = BudgetNode::Cond {
            probability: 0.0,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: Some(Box::new(BudgetNode::Leaf(else_b))),
        };
        prop_assert!((cond_1.aggregate(pctl) - then_b.target(pctl)).abs() < 1e-6);
        prop_assert!((cond_0.aggregate(pctl) - else_b.target(pctl)).abs() < 1e-6);
    }
}

// ── Slack Conservation ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Slack = ceiling - aggregate (exact).
    #[test]
    fn slack_conservation(
        budget in arb_stage_budget(),
        pctl in arb_percentile(),
        ceiling in 0.0..2_000_000.0_f64,
    ) {
        let node = BudgetNode::Leaf(budget);
        let slack = node.slack(pctl, ceiling);
        let expected = ceiling - budget.target(pctl);
        prop_assert!((slack - expected).abs() < 1e-6,
            "slack {:.6} != ceiling - target {:.6}", slack, expected);
    }

    /// Positive slack means headroom, negative means over-budget.
    #[test]
    fn slack_sign_semantics(
        budget in arb_stage_budget(),
        pctl in arb_percentile(),
        ceiling in 0.0..2_000_000.0_f64,
    ) {
        let node = BudgetNode::Leaf(budget);
        let slack = node.slack(pctl, ceiling);
        if ceiling >= budget.target(pctl) {
            prop_assert!(slack >= -1e-10, "expected non-negative slack: {}", slack);
        } else {
            prop_assert!(slack <= 1e-10, "expected non-positive slack: {}", slack);
        }
    }
}

// ── Serde Roundtrip ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// StageBudget survives JSON roundtrip.
    #[test]
    fn budget_serde_roundtrip(budget in arb_stage_budget()) {
        let json = serde_json::to_string(&budget).unwrap();
        let back: StageBudget = serde_json::from_str(&json).unwrap();
        // Use tolerance for f64 precision
        prop_assert!((budget.p50_us - back.p50_us).abs() < 1e-6);
        prop_assert!((budget.p95_us - back.p95_us).abs() < 1e-6);
        prop_assert!((budget.p99_us - back.p99_us).abs() < 1e-6);
        prop_assert!((budget.p999_us - back.p999_us).abs() < 1e-6);
        prop_assert_eq!(budget.stage, back.stage);
    }

    /// ReasonCode survives JSON roundtrip.
    #[test]
    fn reason_code_serde_roundtrip(
        stage in arb_stage(),
        pctl in arb_percentile(),
    ) {
        let rc = ReasonCode::BudgetExceeded { stage, percentile: pctl };
        let json = serde_json::to_string(&rc).unwrap();
        let back: ReasonCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rc, back);
    }

    /// Mitigation survives JSON roundtrip.
    #[test]
    fn mitigation_serde_roundtrip(mit in arb_mitigation()) {
        let json = serde_json::to_string(&mit).unwrap();
        let back: Mitigation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(mit, back);
    }
}

// ── Leaves Collection ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// leaves() collects all leaf budgets from a Seq tree.
    #[test]
    fn leaves_count_matches_seq(
        budgets in prop::collection::vec(arb_stage_budget(), 1..=10),
    ) {
        let nodes: Vec<BudgetNode> = budgets.iter().map(|b| BudgetNode::Leaf(*b)).collect();
        let seq = BudgetNode::Seq(nodes);
        prop_assert_eq!(seq.leaves().len(), budgets.len());
    }

    /// leaves() collects all leaf budgets from a Par tree.
    #[test]
    fn leaves_count_matches_par(
        budgets in prop::collection::vec(arb_stage_budget(), 1..=10),
    ) {
        let nodes: Vec<BudgetNode> = budgets.iter().map(|b| BudgetNode::Leaf(*b)).collect();
        let par = BudgetNode::Par(nodes);
        prop_assert_eq!(par.leaves().len(), budgets.len());
    }
}

// ── Aggregate Non-Negativity ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Aggregate is always non-negative for valid budgets.
    #[test]
    fn aggregate_nonnegative(budget in arb_stage_budget(), pctl in arb_percentile()) {
        let node = BudgetNode::Leaf(budget);
        prop_assert!(node.aggregate(pctl) >= 0.0);
    }

    /// Sequential aggregate is non-negative.
    #[test]
    fn seq_aggregate_nonnegative(
        budgets in prop::collection::vec(arb_stage_budget(), 1..=5),
        pctl in arb_percentile(),
    ) {
        let nodes: Vec<BudgetNode> = budgets.iter().map(|b| BudgetNode::Leaf(*b)).collect();
        let seq = BudgetNode::Seq(nodes);
        prop_assert!(seq.aggregate(pctl) >= 0.0);
    }

    /// Parallel aggregate is non-negative.
    #[test]
    fn par_aggregate_nonnegative(
        budgets in prop::collection::vec(arb_stage_budget(), 1..=5),
        pctl in arb_percentile(),
    ) {
        let nodes: Vec<BudgetNode> = budgets.iter().map(|b| BudgetNode::Leaf(*b)).collect();
        let par = BudgetNode::Par(nodes);
        prop_assert!(par.aggregate(pctl) >= 0.0);
    }

    /// Conditional aggregate is non-negative.
    #[test]
    fn cond_aggregate_nonnegative(
        then_b in arb_stage_budget(),
        else_b in arb_stage_budget(),
        p in arb_probability(),
        pctl in arb_percentile(),
    ) {
        let cond = BudgetNode::Cond {
            probability: p,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: Some(Box::new(BudgetNode::Leaf(else_b))),
        };
        prop_assert!(cond.aggregate(pctl) >= 0.0);
    }
}

// ── Default Pipeline Integrity ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Default pipeline tree aggregate is consistent at all percentiles.
    #[test]
    fn default_pipeline_consistent(pctl in arb_percentile()) {
        let tree = default_pipeline_tree();
        let agg = tree.aggregate(pctl);
        // Must be positive and finite.
        prop_assert!(agg > 0.0);
        prop_assert!(agg.is_finite());
    }
}

// ── BudgetEnforcer Property Tests ───────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Recording within-budget observations never produces overflow.
    #[test]
    fn enforcer_no_overflow_within_budget(
        stage in arb_stage(),
    ) {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Use 1μs — guaranteed within any budget.
        let result = enforcer.record(stage, 1.0, "test");
        prop_assert!(!result.overflow);
        prop_assert_eq!(result.recommended_mitigation, Mitigation::None);
    }

    /// Recording way above p999 always produces overflow.
    #[test]
    fn enforcer_always_overflow_above_p999(
        stage in arb_stage(),
    ) {
        let mut enforcer = BudgetEnforcer::with_defaults();
        if let Some(budget) = enforcer.stage_budget(stage).copied() {
            let above = budget.p999_us * 2.0;
            let result = enforcer.record(stage, above, "test");
            prop_assert!(result.overflow, "should overflow at {}μs (p999={}μs)", above, budget.p999_us);
        }
    }

    /// Total observations equals sum of records.
    #[test]
    fn enforcer_observation_count_consistent(
        records in prop::collection::vec(
            (arb_stage(), 1.0..100_000.0_f64),
            1..=50,
        ),
    ) {
        let mut enforcer = BudgetEnforcer::with_defaults();
        for (stage, latency) in &records {
            enforcer.record(*stage, *latency, "test");
        }
        prop_assert_eq!(enforcer.total_observations(), records.len() as u64);
    }

    /// Overflow count ≤ observation count.
    #[test]
    fn enforcer_overflow_bounded(
        records in prop::collection::vec(
            (arb_stage(), 1.0..1_000_000.0_f64),
            1..=50,
        ),
    ) {
        let mut enforcer = BudgetEnforcer::with_defaults();
        for (stage, latency) in &records {
            enforcer.record(*stage, *latency, "test");
        }
        prop_assert!(enforcer.total_overflows() <= enforcer.total_observations());
    }

    /// Snapshot stages match pipeline stages.
    #[test]
    fn enforcer_snapshot_coverage(
        stage in arb_stage(),
    ) {
        let mut enforcer = BudgetEnforcer::with_defaults();
        enforcer.record(stage, 100.0, "test");
        let snap = enforcer.snapshot();
        prop_assert_eq!(snap.stages.len(), LatencyStage::PIPELINE_STAGES.len());
    }

    /// EnforcerSnapshot survives JSON roundtrip.
    #[test]
    fn enforcer_snapshot_serde(
        records in prop::collection::vec(
            (arb_stage(), 1.0..10_000.0_f64),
            1..=20,
        ),
    ) {
        let mut enforcer = BudgetEnforcer::with_defaults();
        for (stage, latency) in &records {
            enforcer.record(*stage, *latency, "test");
        }
        let snap = enforcer.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: EnforcerSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.total_observations, back.total_observations);
        prop_assert_eq!(snap.total_overflows, back.total_overflows);
        prop_assert_eq!(snap.stages.len(), back.stages.len());
    }

    /// Log entries only generated when configured.
    #[test]
    fn enforcer_log_overflow_only(
        latency in 1.0..500.0_f64,
    ) {
        let config = BudgetEnforcerConfig {
            log_overflows_only: true,
            log_all_observations: false,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        // DeltaExtraction p50 = 200μs, so ≤200 should be within budget.
        enforcer.record(LatencyStage::DeltaExtraction, latency.min(150.0), "test");
        prop_assert_eq!(enforcer.log_count(), 0, "within-budget should not log");
    }

    /// Mitigation severity increases with percentile tier.
    #[test]
    fn enforcer_mitigation_monotonic_severity(
        stage in arb_stage(),
    ) {
        let enforcer = BudgetEnforcer::with_defaults();
        let m_p50 = enforcer.mitigation_for(stage, Percentile::P50);
        let m_p95 = enforcer.mitigation_for(stage, Percentile::P95);
        let m_p99 = enforcer.mitigation_for(stage, Percentile::P99);
        let m_p999 = enforcer.mitigation_for(stage, Percentile::P999);

        fn severity(m: Mitigation) -> u8 {
            match m {
                Mitigation::None => 0,
                Mitigation::Defer => 1,
                Mitigation::Degrade => 2,
                Mitigation::Shed => 3,
                Mitigation::Skip => 4,
            }
        }

        // p50 is always None, and severity should be non-decreasing.
        prop_assert_eq!(m_p50, Mitigation::None);
        prop_assert!(severity(m_p95) <= severity(m_p99),
            "p95 mitigation ({:?}) more severe than p99 ({:?}) for {}", m_p95, m_p99, stage);
        prop_assert!(severity(m_p99) <= severity(m_p999),
            "p99 mitigation ({:?}) more severe than p999 ({:?}) for {}", m_p99, m_p999, stage);
    }

    /// Build_run total equals sum of observations.
    #[test]
    fn enforcer_build_run_total_consistent(
        latencies in prop::collection::vec(100.0..50_000.0_f64, 1..=8),
    ) {
        let enforcer = BudgetEnforcer::with_defaults();
        let mut t = 1_000_000_u64;
        let observations: Vec<StageObservation> = LatencyStage::PIPELINE_STAGES
            .iter()
            .take(latencies.len())
            .zip(latencies.iter())
            .map(|(&stage, &lat)| {
                let obs = StageObservation {
                    stage,
                    latency_us: lat,
                    correlation_id: "test".into(),
                    scenario_id: None,
                    start_epoch_us: t,
                    end_epoch_us: t + lat as u64,
                    overflow: false,
                    reason: None,
                    mitigation: Mitigation::None,
                };
                t += lat as u64 + 100;
                obs
            })
            .collect();

        let expected_total: f64 = observations.iter().map(|o| o.latency_us).sum();
        let run = enforcer.build_run("r1", "c1", observations);
        prop_assert!((run.total_latency_us - expected_total).abs() < 1e-6);
    }
}
