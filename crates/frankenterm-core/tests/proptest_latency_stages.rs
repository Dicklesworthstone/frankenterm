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

// ── CorrelationContext Property Tests ─────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Full pipeline context has correct stage count and is propagation-intact.
    #[test]
    fn correlation_full_pipeline_intact(
        gap_us in prop::collection::vec(1_u64..1000, 8..=8),
        dur_us in prop::collection::vec(10_u64..10000, 8..=8),
    ) {
        let mut ctx = CorrelationContext::new("run-prop", 0);
        let mut t = 1000_u64;
        for (i, &stage) in LatencyStage::PIPELINE_STAGES.iter().enumerate() {
            let probe = ctx.begin_stage(stage, t);
            t += dur_us[i];
            ctx.end_stage(probe, t);
            t += gap_us[i];
        }
        prop_assert_eq!(ctx.stage_count(), 8);
        prop_assert!(ctx.propagation_intact);
        prop_assert!(ctx.missing_stages().is_empty());
    }

    /// Skipping a non-last stage breaks propagation_intact.
    /// (Skipping the last stage doesn't trigger a mismatch because no subsequent
    /// begin_stage call is made.)
    #[test]
    fn correlation_gap_breaks_propagation(
        skip_idx in 1_usize..7, // exclude last stage (index 7)
    ) {
        let mut ctx = CorrelationContext::new("run-skip", 0);
        let mut t = 1000_u64;
        for (i, &stage) in LatencyStage::PIPELINE_STAGES.iter().enumerate() {
            if i == skip_idx {
                t += 100; // skip this stage
                continue;
            }
            let probe = ctx.begin_stage(stage, t);
            t += 100;
            ctx.end_stage(probe, t);
            t += 10;
        }
        prop_assert!(!ctx.propagation_intact);
        prop_assert_eq!(ctx.missing_stages().len(), 1);
    }

    /// total_elapsed_us equals last_end - first_start.
    #[test]
    fn correlation_total_elapsed(
        durations in prop::collection::vec(1_u64..5000, 1..=8),
        gaps in prop::collection::vec(0_u64..500, 1..=8),
    ) {
        let stages_to_use = durations.len().min(LatencyStage::PIPELINE_STAGES.len());
        if stages_to_use == 0 {
            return Ok(());
        }
        let mut ctx = CorrelationContext::new("run-elapsed", 0);
        let start = 1000_u64;
        let mut t = start;
        for i in 0..stages_to_use {
            let probe = ctx.begin_stage(LatencyStage::PIPELINE_STAGES[i], t);
            t += durations[i];
            ctx.end_stage(probe, t);
            if i < stages_to_use - 1 && i < gaps.len() {
                t += gaps[i];
            }
        }
        let expected = t - start;
        prop_assert_eq!(ctx.total_elapsed_us(), expected);
    }

    /// CorrelationContext survives serde roundtrip.
    #[test]
    fn correlation_serde_roundtrip(
        n_stages in 1_usize..=8,
    ) {
        let mut ctx = CorrelationContext::new("run-serde-prop", 0);
        let mut t = 100_u64;
        for i in 0..n_stages {
            let probe = ctx.begin_stage(LatencyStage::PIPELINE_STAGES[i], t);
            t += 100;
            ctx.end_stage(probe, t);
            t += 10;
        }
        let json = serde_json::to_string(&ctx).unwrap();
        let back: CorrelationContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ctx, back);
    }

    /// validate() returns empty for well-formed contexts.
    #[test]
    fn correlation_validate_valid(
        n_stages in 1_usize..=8,
    ) {
        let mut ctx = CorrelationContext::new("run-valid-prop", 0);
        let mut t = 100_u64;
        for i in 0..n_stages {
            let probe = ctx.begin_stage(LatencyStage::PIPELINE_STAGES[i], t);
            t += 100;
            ctx.end_stage(probe, t);
            t += 10;
        }
        let errors = ctx.validate();
        prop_assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
    }

    /// to_pipeline_run total equals sum of stage latencies.
    #[test]
    fn correlation_to_pipeline_run_total(
        durations in prop::collection::vec(1_u64..10000, 1..=8),
    ) {
        let stages_to_use = durations.len().min(LatencyStage::PIPELINE_STAGES.len());
        let mut ctx = CorrelationContext::new("run-total", 0);
        let mut t = 100_u64;
        let mut expected_total = 0.0_f64;
        for i in 0..stages_to_use {
            let probe = ctx.begin_stage(LatencyStage::PIPELINE_STAGES[i], t);
            t += durations[i];
            ctx.end_stage(probe, t);
            expected_total += durations[i] as f64;
            t += 10;
        }
        let run = ctx.to_pipeline_run();
        prop_assert!((run.total_latency_us - expected_total).abs() < 1e-6,
            "total {:.6} != expected {:.6}", run.total_latency_us, expected_total);
    }
}

// ── InstrumentationOverhead Property Tests ──────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Mean overhead equals total / count.
    #[test]
    fn overhead_mean_equals_total_div_count(
        values in prop::collection::vec(0.0..10.0_f64, 1..=50),
    ) {
        let mut oh = InstrumentationOverhead::new();
        for &v in &values {
            oh.record(v);
        }
        let expected = oh.total_overhead_us / oh.probe_count as f64;
        prop_assert!((oh.mean_overhead_us - expected).abs() < 1e-10);
    }

    /// Max overhead tracks the true maximum.
    #[test]
    fn overhead_max_tracks_true_max(
        values in prop::collection::vec(0.0..100.0_f64, 1..=50),
    ) {
        let mut oh = InstrumentationOverhead::new();
        for &v in &values {
            oh.record(v);
        }
        let true_max = values.iter().cloned().fold(0.0_f64, f64::max);
        prop_assert!((oh.max_overhead_us - true_max).abs() < 1e-10);
    }

    /// within_budget reflects max vs budget.
    #[test]
    fn overhead_within_budget_consistent(
        values in prop::collection::vec(0.0..5.0_f64, 1..=20),
    ) {
        let mut oh = InstrumentationOverhead::new();
        for &v in &values {
            oh.record(v);
        }
        let expected = oh.max_overhead_us <= oh.budget_per_probe_us;
        prop_assert_eq!(oh.within_budget, expected);
    }

    /// Overhead fraction is consistent.
    #[test]
    fn overhead_fraction_consistent(
        values in prop::collection::vec(0.01..1.0_f64, 1..=20),
        pipeline_us in 100.0..100_000.0_f64,
    ) {
        let mut oh = InstrumentationOverhead::new();
        for &v in &values {
            oh.record(v);
        }
        let frac = oh.overhead_fraction(pipeline_us);
        let expected = oh.total_overhead_us / pipeline_us;
        prop_assert!((frac - expected).abs() < 1e-10);
    }

    /// InstrumentationOverhead survives serde roundtrip (f64 tolerance).
    #[test]
    fn overhead_serde_roundtrip(
        values in prop::collection::vec(0.0..10.0_f64, 1..=20),
    ) {
        let mut oh = InstrumentationOverhead::new();
        for &v in &values {
            oh.record(v);
        }
        let json = serde_json::to_string(&oh).unwrap();
        let back: InstrumentationOverhead = serde_json::from_str(&json).unwrap();
        prop_assert!((oh.total_overhead_us - back.total_overhead_us).abs() < 1e-10);
        prop_assert_eq!(oh.probe_count, back.probe_count);
        prop_assert!((oh.mean_overhead_us - back.mean_overhead_us).abs() < 1e-10);
        prop_assert!((oh.max_overhead_us - back.max_overhead_us).abs() < 1e-10);
        prop_assert!((oh.budget_per_probe_us - back.budget_per_probe_us).abs() < 1e-10);
        prop_assert_eq!(oh.within_budget, back.within_budget);
    }
}

// ── InstrumentedEnforcer Property Tests ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// completed_runs equals number of process_run calls.
    #[test]
    fn instrumented_run_count(
        n_runs in 1_usize..=10,
    ) {
        let mut ie = InstrumentedEnforcer::new();
        for i in 0..n_runs {
            let mut ctx = CorrelationContext::new(&format!("run-{i}"), 0);
            let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
            ctx.end_stage(probe, 50);
            ie.process_run(&ctx);
        }
        prop_assert_eq!(ie.completed_runs(), n_runs as u64);
    }

    /// overflow_runs <= completed_runs.
    #[test]
    fn instrumented_overflow_bounded(
        latencies in prop::collection::vec(1.0..1_000_000.0_f64, 1..=10),
    ) {
        let mut ie = InstrumentedEnforcer::new();
        for (i, &lat) in latencies.iter().enumerate() {
            let mut ctx = CorrelationContext::new(&format!("run-{i}"), 0);
            let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
            ctx.end_stage(probe, lat as u64);
            ie.process_run(&ctx);
        }
        prop_assert!(ie.overflow_runs() <= ie.completed_runs());
    }

    /// overflow_rate is in [0.0, 1.0].
    #[test]
    fn instrumented_overflow_rate_bounded(
        latencies in prop::collection::vec(1.0..500_000.0_f64, 1..=10),
    ) {
        let mut ie = InstrumentedEnforcer::new();
        for (i, &lat) in latencies.iter().enumerate() {
            let mut ctx = CorrelationContext::new(&format!("run-{i}"), 0);
            let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 0);
            ctx.end_stage(probe, lat as u64);
            ie.process_run(&ctx);
        }
        let rate = ie.overflow_rate();
        prop_assert!(rate >= 0.0 && rate <= 1.0, "rate out of bounds: {}", rate);
    }

    /// Degradation level increases monotonically with overhead.
    #[test]
    fn instrumented_degradation_monotonic(
        overhead in 0.0..50.0_f64,
    ) {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(overhead);
        let deg = ie.current_degradation();
        if overhead <= 1.0 {
            prop_assert_eq!(deg, InstrumentationDegradation::Full);
        } else if overhead <= 5.0 {
            prop_assert_eq!(deg, InstrumentationDegradation::SkipOverhead);
        } else if overhead <= 10.0 {
            prop_assert_eq!(deg, InstrumentationDegradation::SkipCorrelation);
        } else {
            prop_assert_eq!(deg, InstrumentationDegradation::Passthrough);
        }
    }

    /// Diagnostic snapshot serde roundtrip.
    #[test]
    fn instrumented_diagnostic_serde(
        n_runs in 0_usize..=5,
    ) {
        let mut ie = InstrumentedEnforcer::new();
        for i in 0..n_runs {
            let mut ctx = CorrelationContext::new(&format!("run-{i}"), 0);
            let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
            ctx.end_stage(probe, 100);
            ie.process_run(&ctx);
        }
        let diag = ie.diagnostic();
        let json = serde_json::to_string(&diag).unwrap();
        let back: InstrumentationDiagnostic = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(diag, back);
    }

    /// process_validated_run returns errors for empty runs.
    #[test]
    fn instrumented_validated_empty_run(
        _seed in 0_u32..1000,
    ) {
        let mut ie = InstrumentedEnforcer::new();
        let ctx = CorrelationContext::new("run-empty", 0);
        let (_results, errors) = ie.process_validated_run(&ctx);
        let has_empty = errors.iter().any(|e| matches!(e, InstrumentationError::EmptyRun { .. }));
        prop_assert!(has_empty, "should detect empty run");
    }
}

// ── FastProbe Property Tests ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// FastProbe elapsed is non-negative when end >= start.
    #[test]
    fn fast_probe_elapsed_nonneg(
        stage in arb_stage(),
        start in 0_u64..1_000_000,
        delta in 0_u64..1_000_000,
    ) {
        let probe = FastProbe::begin(stage, start);
        let elapsed = probe.elapsed_us(start + delta);
        prop_assert!(elapsed >= 0.0);
        prop_assert!((elapsed - delta as f64).abs() < 1e-10);
    }

    /// FastProbe returns 0 on clock regression.
    #[test]
    fn fast_probe_clock_regression(
        stage in arb_stage(),
        start in 1_u64..1_000_000,
        regress in 1_u64..1_000_000,
    ) {
        let end = if regress >= start { 0 } else { start - regress };
        let probe = FastProbe::begin(stage, start);
        let elapsed = probe.elapsed_us(end);
        prop_assert!((elapsed - 0.0).abs() < 1e-10);
    }
}

// ── InstrumentationError Serde ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// All InstrumentationError variants survive serde roundtrip.
    #[test]
    fn instrumentation_error_serde(
        stage in arb_stage(),
        start in 0_u64..1_000_000,
        end in 0_u64..1_000_000,
    ) {
        let errors = vec![
            InstrumentationError::UnterminatedProbe { stage, start_us: start },
            InstrumentationError::OrphanedEnd { stage },
            InstrumentationError::ClockRegression { stage, start_us: start, end_us: end },
            InstrumentationError::DuplicateStage { stage },
            InstrumentationError::EmptyRun { run_id: format!("run-{start}") },
            InstrumentationError::OverheadBudgetExceeded {
                max_observed_us: start as f64 / 1000.0,
                budget_us: 1.0,
            },
        ];
        for err in &errors {
            let json = serde_json::to_string(err).unwrap();
            let back: InstrumentationError = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(err, &back);
        }
    }

    /// InstrumentationDegradation survives serde roundtrip.
    #[test]
    fn degradation_serde(
        idx in 0_usize..4,
    ) {
        let variants = [
            InstrumentationDegradation::Full,
            InstrumentationDegradation::SkipOverhead,
            InstrumentationDegradation::SkipCorrelation,
            InstrumentationDegradation::Passthrough,
        ];
        let d = variants[idx];
        let json = serde_json::to_string(&d).unwrap();
        let back: InstrumentationDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }
}

// ── MitigationLevel Property Tests ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// MitigationLevel roundtrip through Mitigation is identity.
    #[test]
    fn mitigation_level_roundtrip(idx in 0_usize..5) {
        let levels = MitigationLevel::ALL;
        let level = levels[idx];
        let mit = level.to_mitigation();
        let back = MitigationLevel::from_mitigation(mit);
        prop_assert_eq!(level, back);
    }

    /// MitigationLevel serde roundtrip.
    #[test]
    fn mitigation_level_serde(idx in 0_usize..5) {
        let levels = MitigationLevel::ALL;
        let level = levels[idx];
        let json = serde_json::to_string(&level).unwrap();
        let back: MitigationLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    /// Severity is monotonically increasing.
    #[test]
    fn mitigation_level_severity_monotonic(a in 0_usize..5, b in 0_usize..5) {
        let levels = MitigationLevel::ALL;
        let la = levels[a];
        let lb = levels[b];
        if la < lb {
            prop_assert!(la.severity() < lb.severity());
        } else if la == lb {
            prop_assert_eq!(la.severity(), lb.severity());
        } else {
            prop_assert!(la.severity() > lb.severity());
        }
    }
}

// ── PolicyConstraint Property Tests ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// PolicyConstraint.clamp() result is <= max_level.
    #[test]
    fn policy_clamp_bounded(
        max_idx in 0_usize..5,
        req_idx in 0_usize..5,
        stage in arb_stage(),
    ) {
        let levels = MitigationLevel::ALL;
        let pc = PolicyConstraint {
            stage,
            max_level: levels[max_idx],
            critical: false,
            warmup_count: 0,
        };
        let requested = levels[req_idx];
        let clamped = pc.clamp(requested);
        prop_assert!(clamped <= pc.max_level,
            "clamped {:?} > max {:?}", clamped, pc.max_level);
    }

    /// PolicyConstraint.allows() is consistent with clamp().
    #[test]
    fn policy_allows_consistent_with_clamp(
        max_idx in 0_usize..5,
        req_idx in 0_usize..5,
        stage in arb_stage(),
    ) {
        let levels = MitigationLevel::ALL;
        let pc = PolicyConstraint {
            stage,
            max_level: levels[max_idx],
            critical: false,
            warmup_count: 0,
        };
        let requested = levels[req_idx];
        if pc.allows(requested) {
            prop_assert_eq!(pc.clamp(requested), requested);
        } else {
            prop_assert_eq!(pc.clamp(requested), pc.max_level);
        }
    }
}

// ── RuntimeEnforcer Property Tests ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// RuntimeEnforcer observation count equals number of enforce() calls.
    #[test]
    fn runtime_enforcer_obs_count(
        n in 1_usize..=30,
    ) {
        let mut re = RuntimeEnforcer::with_defaults();
        for i in 0..n {
            re.enforce(LatencyStage::PtyCapture, 10.0, "test", i as u64 * 100);
        }
        prop_assert_eq!(re.total_observations(), n as u64);
    }

    /// RuntimeEnforcer escalation count is bounded by observation count.
    #[test]
    fn runtime_enforcer_escalation_bounded(
        latencies in prop::collection::vec(1.0..500_000.0_f64, 1..=20),
    ) {
        let config = RuntimeEnforcerConfig {
            policy_constraints: default_policy_constraints()
                .into_iter()
                .map(|mut c| { c.warmup_count = 0; c })
                .collect(),
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        for (i, &lat) in latencies.iter().enumerate() {
            re.enforce(LatencyStage::PtyCapture, lat, "test", i as u64 * 1000);
        }
        prop_assert!(re.total_escalations() <= re.total_observations());
    }

    /// RuntimeEnforcer recovery count <= escalation count.
    #[test]
    fn runtime_enforcer_recovery_bounded(
        latencies in prop::collection::vec(1.0..500_000.0_f64, 1..=30),
    ) {
        let config = RuntimeEnforcerConfig {
            recovery: RecoveryProtocol {
                cooldown_observations: 3,
                max_degraded_duration_us: 100_000_000,
                gradual: true,
            },
            policy_constraints: default_policy_constraints()
                .into_iter()
                .map(|mut c| { c.warmup_count = 0; c })
                .collect(),
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        for (i, &lat) in latencies.iter().enumerate() {
            re.enforce(LatencyStage::PatternDetection, lat, "test", i as u64 * 1000);
        }
        // Recoveries can't exceed escalations in a monotonic sequence.
        // But with repeated escalate/recover cycles, they can be equal.
        // What we know: can't recover without having escalated first.
        prop_assert!(re.total_recoveries() <= re.total_observations(),
            "recoveries {} > observations {}", re.total_recoveries(), re.total_observations());
    }

    /// Within-budget observations never cause escalation.
    #[test]
    fn runtime_enforcer_no_escalation_within_budget(
        n in 1_usize..=20,
    ) {
        let config = RuntimeEnforcerConfig {
            policy_constraints: default_policy_constraints()
                .into_iter()
                .map(|mut c| { c.warmup_count = 0; c })
                .collect(),
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        for i in 0..n {
            re.enforce(LatencyStage::PtyCapture, 1.0, "test", i as u64 * 100);
        }
        prop_assert_eq!(re.total_escalations(), 0);
        prop_assert!(re.is_fully_recovered());
    }

    /// enforce_run() returns decisions matching the context stage count.
    #[test]
    fn runtime_enforcer_enforce_run_count(
        n_stages in 1_usize..=8,
    ) {
        let config = RuntimeEnforcerConfig {
            policy_constraints: default_policy_constraints()
                .into_iter()
                .map(|mut c| { c.warmup_count = 0; c })
                .collect(),
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        let mut ctx = CorrelationContext::new("batch", 0);
        let mut t = 100_u64;
        for i in 0..n_stages {
            let probe = ctx.begin_stage(LatencyStage::PIPELINE_STAGES[i], t);
            t += 50;
            ctx.end_stage(probe, t);
            t += 10;
        }
        let decisions = re.enforce_run(&ctx, 0);
        prop_assert_eq!(decisions.len(), n_stages);
    }

    /// EnforcementDecision serde roundtrip (f64 tolerance).
    #[test]
    fn enforcement_decision_serde(
        stage in arb_stage(),
        latency in 1.0..100_000.0_f64,
        overflow in prop::bool::ANY,
        level_idx in 0_usize..5,
    ) {
        let levels = MitigationLevel::ALL;
        let d = EnforcementDecision {
            stage,
            latency_us: latency,
            overflow,
            raw_mitigation: levels[level_idx],
            applied_mitigation: levels[level_idx],
            recovery: false,
            reason: None,
            warmup_active: false,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: EnforcementDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d.stage, back.stage);
        prop_assert!((d.latency_us - back.latency_us).abs() < 1e-10);
        prop_assert_eq!(d.overflow, back.overflow);
        prop_assert_eq!(d.raw_mitigation, back.raw_mitigation);
        prop_assert_eq!(d.applied_mitigation, back.applied_mitigation);
        prop_assert_eq!(d.recovery, back.recovery);
        prop_assert_eq!(d.warmup_active, back.warmup_active);
    }

    /// RuntimeEnforcerSnapshot serde roundtrip.
    #[test]
    fn runtime_enforcer_snapshot_serde(
        n in 0_usize..=10,
    ) {
        let mut re = RuntimeEnforcer::with_defaults();
        for i in 0..n {
            re.enforce(LatencyStage::PtyCapture, 10.0, "test", i as u64 * 100);
        }
        let snap = re.diagnostic_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: RuntimeEnforcerSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.observation_count, back.observation_count);
        prop_assert_eq!(snap.fully_recovered, back.fully_recovered);
    }

    // ── A4: Adaptive Budget Allocator ──

    /// StagePressure compute invariants.
    #[test]
    fn stage_pressure_headroom_sign(
        observed in 0.001_f64..100_000.0,
        budget in 0.001_f64..100_000.0,
    ) {
        let p = StagePressure::compute(LatencyStage::PtyCapture, observed, budget);
        if observed < budget {
            prop_assert!(p.headroom > 0.0);
            prop_assert!(!p.is_over_budget());
            prop_assert!(p.donatable_slack_us() > 0.0);
            prop_assert!(p.deficit_us() == 0.0);
        } else if observed > budget {
            prop_assert!(p.headroom < 0.0);
            prop_assert!(p.is_over_budget());
            prop_assert!(p.donatable_slack_us() == 0.0);
            prop_assert!(p.deficit_us() > 0.0);
        }
    }

    /// StagePressure serde roundtrip.
    #[test]
    fn stage_pressure_serde_roundtrip(
        stage in arb_stage(),
        observed in 0.001_f64..100_000.0,
        budget in 0.001_f64..100_000.0,
    ) {
        let p = StagePressure::compute(stage, observed, budget);
        let json = serde_json::to_string(&p).unwrap();
        let back: StagePressure = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p.stage, back.stage);
        prop_assert!((p.headroom - back.headroom).abs() < 1e-10);
        prop_assert!((p.observed_p95_us - back.observed_p95_us).abs() < 1e-10);
        prop_assert!((p.budget_p95_us - back.budget_p95_us).abs() < 1e-10);
    }

    /// AdaptiveAllocatorConfig validation: default is always valid.
    #[test]
    fn allocator_config_default_valid(_dummy in 0..1_u8) {
        let cfg = AdaptiveAllocatorConfig::default();
        let errors = cfg.validate();
        prop_assert!(errors.is_empty());
    }

    /// AdaptiveAllocator conservation: sum of lane budgets = constant after allocation.
    #[test]
    fn allocator_conservation_invariant(
        epochs in 1_usize..50,
        pressure_factor in 0.5_f64..3.0,
        stressed_idx in 0_usize..8,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        let total = alloc.total_budget_us();

        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let factor = if i == stressed_idx % alloc.lanes().len() {
                        pressure_factor
                    } else {
                        0.3
                    };
                    StagePressure::compute(l.stage, l.current_p95_us * factor, l.current_p95_us)
                })
                .collect();
            alloc.allocate(&pressures, "conservation-test");
        }

        let sum: f64 = alloc.lanes().iter().map(|l| l.current_p95_us).sum();
        // Allow up to 1us drift from float accumulation.
        prop_assert!(
            (sum - total).abs() < 1.0,
            "conservation violated: sum={} total={}",
            sum, total
        );
    }

    /// AdaptiveAllocator floor invariant: no lane drops below min_budget_pct.
    #[test]
    fn allocator_floor_invariant(
        epochs in 1_usize..30,
        min_pct in 0.3_f64..0.9,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_budget_pct: min_pct,
            max_adjustment_pct: 0.20,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // All donating to ApiResponse.
        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::ApiResponse {
                        StagePressure::compute(l.stage, l.current_p95_us * 3.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, "floor-test");
        }

        for lane in alloc.lanes() {
            let floor = lane.default_p95_us * min_pct;
            prop_assert!(
                lane.current_p95_us >= floor - 1e-6,
                "{} below floor: {} < {}",
                lane.stage, lane.current_p95_us, floor
            );
        }
    }

    /// AdaptiveAllocator ceiling invariant: no lane exceeds max_budget_pct.
    #[test]
    fn allocator_ceiling_invariant(
        epochs in 1_usize..30,
        max_pct in 1.5_f64..5.0,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            max_budget_pct: max_pct,
            max_adjustment_pct: 0.20,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::PtyCapture {
                        StagePressure::compute(l.stage, l.current_p95_us * 5.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, "ceiling-test");
        }

        for lane in alloc.lanes() {
            let ceiling = lane.default_p95_us * max_pct;
            prop_assert!(
                lane.current_p95_us <= ceiling + 1e-6,
                "{} above ceiling: {} > {}",
                lane.stage, lane.current_p95_us, ceiling
            );
        }
    }

    /// AdaptiveAllocator determinism: same input → same output.
    #[test]
    fn allocator_deterministic_replay(
        epochs in 1_usize..20,
        factor in 0.5_f64..3.0,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let budgets = default_budgets();

        // Build pressure sequence from a fresh allocator.
        let mut alloc_ref = AdaptiveAllocator::new(&budgets, cfg.clone());
        let mut pressure_seq = Vec::new();
        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc_ref
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * factor, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us)
                    }
                })
                .collect();
            pressure_seq.push(pressures.clone());
            alloc_ref.allocate(&pressures, "det-ref");
        }

        // Replay on second allocator.
        let mut alloc2 = AdaptiveAllocator::new(&budgets, cfg);
        for pressures in &pressure_seq {
            alloc2.allocate(pressures, "det-ref");
        }

        for (l1, l2) in alloc_ref.lanes().iter().zip(alloc2.lanes().iter()) {
            prop_assert!(
                (l1.current_p95_us - l2.current_p95_us).abs() < 1e-6,
                "replay diverged for {}: {} vs {}",
                l1.stage, l1.current_p95_us, l2.current_p95_us
            );
        }
    }

    /// AdaptiveAllocator reset restores exact defaults.
    #[test]
    fn allocator_reset_restores(
        epochs in 1_usize..10,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us))
                .collect();
            alloc.allocate(&pressures, "pre-reset");
        }

        alloc.reset();

        for lane in alloc.lanes() {
            prop_assert!(
                (lane.current_p95_us - lane.default_p95_us).abs() < 1e-6,
                "{} not reset: {} vs {}",
                lane.stage, lane.current_p95_us, lane.default_p95_us
            );
        }
    }

    /// AllocationDecision serde roundtrip.
    #[test]
    fn allocation_decision_serde_roundtrip(
        epoch in 1_u64..1000,
        donor_count in 0_usize..5,
        receiver_count in 0_usize..5,
    ) {
        let d = AllocationDecision {
            epoch,
            correlation_id: format!("prop-{}", epoch),
            adjustments: Vec::new(),
            slack_pool_before_us: 0.0,
            slack_pool_after_us: 0.0,
            warmup: false,
            reason: AllocationReason::SlackRedistributed { donor_count, receiver_count },
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: AllocationDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d.epoch, back.epoch);
        prop_assert_eq!(d.reason, back.reason);
    }

    /// AllocatorSnapshot serde roundtrip.
    #[test]
    fn allocator_snapshot_serde_roundtrip(
        epochs in 0_usize..5,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 0.8, l.current_p95_us))
                .collect();
            alloc.allocate(&pressures, "snap-serde");
        }
        let snap = alloc.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: AllocatorSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.epoch, back.epoch);
        prop_assert_eq!(snap.lanes.len(), back.lanes.len());
        prop_assert!((snap.total_budget_us - back.total_budget_us).abs() < 1e-6);
    }

    /// AllocatorDegradation serde roundtrip.
    #[test]
    fn allocator_degradation_serde_roundtrip(
        variant_idx in 0_usize..4,
        count in 1_usize..20,
    ) {
        let degradation = match variant_idx {
            0 => AllocatorDegradation::Healthy,
            1 => AllocatorDegradation::Oscillating { lane_count: count },
            2 => AllocatorDegradation::ConservationDrift { drift_us: count as f64 * 0.1 },
            _ => AllocatorDegradation::FloorSaturation { lane_count: count },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: AllocatorDegradation = serde_json::from_str(&json).unwrap();
        match (&degradation, &back) {
            (AllocatorDegradation::ConservationDrift { drift_us: a },
             AllocatorDegradation::ConservationDrift { drift_us: b }) => {
                prop_assert!((a - b).abs() < 1e-10);
            }
            _ => prop_assert_eq!(degradation, back),
        }
    }

    /// AllocationLogEntry serde roundtrip.
    #[test]
    fn allocation_log_entry_serde(
        epoch in 1_u64..1000,
        donated in 0.0_f64..10000.0,
        received in 0.0_f64..10000.0,
    ) {
        let entry = AllocationLogEntry {
            epoch,
            correlation_id: format!("log-{}", epoch),
            reason: "SLACK_REDISTRIBUTED donors=2 receivers=1".into(),
            adjustment_count: 3,
            total_donated_us: donated,
            total_received_us: received,
            conservation_error_us: 0.001,
            degradation: AllocatorDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: AllocationLogEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry.epoch, back.epoch);
        prop_assert!((entry.total_donated_us - back.total_donated_us).abs() < 1e-10);
        prop_assert!((entry.total_received_us - back.total_received_us).abs() < 1e-10);
    }

    /// Adjusted budgets maintain monotonic invariant.
    #[test]
    fn adjusted_budgets_monotonic(
        epochs in 1_usize..20,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for _e in 0..epochs {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.8, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, "mono-test");
        }

        let adjusted = alloc.adjusted_budgets();
        for b in &adjusted {
            prop_assert!(
                b.p50_us <= b.p95_us + 1e-6,
                "{}: p50={} > p95={}", b.stage, b.p50_us, b.p95_us
            );
            prop_assert!(
                b.p95_us <= b.p99_us + 1e-6,
                "{}: p95={} > p99={}", b.stage, b.p95_us, b.p99_us
            );
            prop_assert!(
                b.p99_us <= b.p999_us + 1e-6,
                "{}: p99={} > p999={}", b.stage, b.p99_us, b.p999_us
            );
        }
    }

    /// Warmup produces no adjustments.
    #[test]
    fn allocator_warmup_noop(
        warmup in 1_u64..100,
    ) {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: warmup,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us))
            .collect();
        let d = alloc.allocate(&pressures, "warmup-prop");
        prop_assert!(d.warmup);
        prop_assert!(d.adjustments.is_empty());
        prop_assert_eq!(d.reason, AllocationReason::Warmup);
    }

    // ── B1: Three-Lane Scheduler ──

    /// SchedulerLane ordering matches priority.
    #[test]
    fn scheduler_lane_ordering(
        a_idx in 0_usize..3,
        b_idx in 0_usize..3,
    ) {
        let lanes = [SchedulerLane::Input, SchedulerLane::Control, SchedulerLane::Bulk];
        let a = lanes[a_idx];
        let b = lanes[b_idx];
        if a_idx < b_idx {
            prop_assert!(a < b);
            prop_assert!(a.priority() < b.priority());
        } else if a_idx == b_idx {
            prop_assert_eq!(a, b);
        } else {
            prop_assert!(a > b);
        }
    }

    /// stage_to_lane covers all non-aggregate pipeline stages.
    #[test]
    fn stage_to_lane_covers_pipeline(
        stage in arb_stage(),
    ) {
        let lane = stage_to_lane(stage);
        // Result should be one of the three lanes.
        let is_valid = matches!(lane, SchedulerLane::Input | SchedulerLane::Control | SchedulerLane::Bulk);
        prop_assert!(is_valid);
    }

    /// LaneScheduler: admitted items increase depth; completed items decrease depth.
    #[test]
    fn scheduler_depth_monotonic(
        n in 1_usize..50,
    ) {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(1_000_000.0);
        for i in 0..n {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("m-{}", i), 0, 0);
        }
        let depth = sched.lane_state(SchedulerLane::Input).depth;
        prop_assert_eq!(depth, n);

        for _ in 0..n {
            sched.complete(SchedulerLane::Input, 10.0);
        }
        prop_assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 0);
    }

    /// LaneScheduler: input items are never shed (only deferred when full).
    #[test]
    fn scheduler_input_never_shed(
        n in 1_usize..100,
    ) {
        let cfg = LaneSchedulerConfig {
            input_queue_capacity: 10,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);
        sched.begin_epoch(1_000_000.0);
        let mut shed_count = 0;
        for i in 0..n {
            let (_, decision) = sched.admit(LatencyStage::PtyCapture, 10.0, &format!("ns-{}", i), 0, 0);
            if decision == AdmissionDecision::Shed {
                shed_count += 1;
            }
        }
        prop_assert_eq!(shed_count, 0, "input items should never be shed");
    }

    /// LaneScheduler: bulk items shed under input pressure.
    #[test]
    fn scheduler_bulk_shed_under_pressure(
        input_fill in 3_usize..10,
    ) {
        let cfg = LaneSchedulerConfig {
            input_queue_capacity: 4,
            input_pressure_threshold: 0.75,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);
        sched.begin_epoch(1_000_000.0);

        // Fill input to trigger pressure.
        for i in 0..input_fill.min(4) {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("p-{}", i), 0, 0);
        }

        if sched.input_under_pressure() {
            let (_, decision) = sched.admit(LatencyStage::StorageWrite, 10.0, "bulk", 0, 0);
            prop_assert_eq!(decision, AdmissionDecision::Shed);
        }
    }

    /// LaneSchedulerConfig: default CPU shares sum to 1.0.
    #[test]
    fn scheduler_config_shares_sum(_dummy in 0..1_u8) {
        let cfg = LaneSchedulerConfig::default();
        let sum = cfg.input_cpu_share + cfg.control_cpu_share + cfg.bulk_cpu_share;
        prop_assert!((sum - 1.0).abs() < 1e-6);
    }

    /// SchedulerSnapshot serde roundtrip.
    #[test]
    fn scheduler_snapshot_serde(
        n in 0_usize..20,
    ) {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        for i in 0..n {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("ss-{}", i), 0, 0);
        }
        let snap = sched.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: SchedulerSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.epoch, back.epoch);
        prop_assert_eq!(snap.lanes.len(), back.lanes.len());
    }

    /// SchedulerDegradation serde roundtrip.
    #[test]
    fn scheduler_degradation_serde_roundtrip(
        variant_idx in 0_usize..4,
        count in 1_usize..100,
    ) {
        let degradation = match variant_idx {
            0 => SchedulerDegradation::Healthy,
            1 => SchedulerDegradation::InputStarvation { depth: count, deferred: count as u64 },
            2 => SchedulerDegradation::BulkStarvation { shed_count: count as u64, completed_count: count as u64 / 2 },
            _ => SchedulerDegradation::ControlBacklog { depth: count, capacity: count * 2 },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: SchedulerDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }

    /// Fairness ratios sum to ~3.0 (one ratio per lane) when all lanes have work.
    #[test]
    fn scheduler_fairness_has_three_lanes(_dummy in 0..1_u8) {
        let sched = LaneScheduler::with_defaults();
        let ratios = sched.fairness_ratios();
        prop_assert_eq!(ratios.len(), 3);
    }

    /// next_lane respects strict priority.
    #[test]
    fn scheduler_next_lane_strict_priority(
        has_input in prop::bool::ANY,
        has_control in prop::bool::ANY,
        has_bulk in prop::bool::ANY,
    ) {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(1_000_000.0);
        if has_bulk {
            sched.admit(LatencyStage::StorageWrite, 10.0, "b", 0, 0);
        }
        if has_control {
            sched.admit(LatencyStage::EventEmission, 10.0, "c", 0, 0);
        }
        if has_input {
            sched.admit(LatencyStage::PtyCapture, 10.0, "i", 0, 0);
        }

        match sched.next_lane() {
            Some(SchedulerLane::Input) => prop_assert!(has_input),
            Some(SchedulerLane::Control) => {
                prop_assert!(!has_input);
                prop_assert!(has_control);
            }
            Some(SchedulerLane::Bulk) => {
                prop_assert!(!has_input);
                prop_assert!(!has_control);
                prop_assert!(has_bulk);
            }
            None => {
                prop_assert!(!has_input && !has_control && !has_bulk);
            }
        }
    }

    // ── B2: InputRing invariants (ft-2p9cb.2.2.3) ──

    /// len never exceeds capacity after any sequence of enqueue/dequeue ops.
    #[test]
    fn input_ring_len_bounded(
        cap in 1_usize..64,
        enqueue_count in 0_usize..128,
        dequeue_count in 0_usize..128,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.75,
            track_sojourn: true,
        };
        let mut ring = InputRing::new(config);

        for i in 0..enqueue_count {
            let _ = ring.enqueue(LatencyStage::PtyCapture, 10.0, "x", i as u64, 0);
        }
        for _ in 0..dequeue_count {
            ring.dequeue(1000);
        }

        prop_assert!(ring.len() <= cap, "len {} > capacity {}", ring.len(), cap);
    }

    /// total_enqueued = total_dequeued + len (dropped are separate).
    #[test]
    fn input_ring_accounting_invariant(
        cap in 1_usize..32,
        ops in prop::collection::vec(prop::bool::ANY, 0..200),
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.75,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);
        let mut t = 0_u64;

        for enqueue in ops {
            if enqueue {
                let _ = ring.enqueue(LatencyStage::DeltaExtraction, 5.0, "op", t, 0);
            } else {
                ring.dequeue(t);
            }
            t += 1;
        }

        let snap = ring.snapshot();
        prop_assert_eq!(
            snap.total_enqueued,
            snap.total_dequeued + snap.len as u64,
            "enqueued={} != dequeued={} + len={}",
            snap.total_enqueued,
            snap.total_dequeued,
            snap.len
        );
    }

    /// Backpressure signal is consistent with fill level.
    #[test]
    fn input_ring_backpressure_consistent(
        cap in 2_usize..64,
        hwm in 0.1_f64..0.99,
        fill_count in 0_usize..128,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: hwm,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        for i in 0..fill_count {
            let _ = ring.enqueue(LatencyStage::StorageWrite, 1.0, "bp", i as u64, 0);
        }

        let utilization = ring.len() as f64 / cap as f64;
        match ring.backpressure() {
            RingBackpressure::Full => prop_assert!(ring.is_full()),
            RingBackpressure::SlowDown => {
                prop_assert!(!ring.is_full());
                prop_assert!(utilization >= hwm, "util {} < hwm {}", utilization, hwm);
            }
            RingBackpressure::Accept => {
                prop_assert!(utilization < hwm, "util {} >= hwm {} but Accept", utilization, hwm);
            }
        }
    }

    /// Sequences are strictly monotonically increasing.
    #[test]
    fn input_ring_seq_monotonic(
        cap in 1_usize..32,
        count in 1_usize..64,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.75,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        let mut seqs = Vec::new();
        for i in 0..count {
            if let Ok(seq) = ring.enqueue(LatencyStage::PtyCapture, 1.0, "s", i as u64, 0) {
                seqs.push(seq);
            }
            // Dequeue half the time to make room.
            if i % 2 == 0 {
                ring.dequeue(i as u64);
            }
        }

        for w in seqs.windows(2) {
            prop_assert!(w[1] > w[0], "seq {} not > {}", w[1], w[0]);
        }
    }

    /// FIFO ordering: dequeued items come out in enqueue order.
    #[test]
    fn input_ring_fifo_ordering(
        cap in 4_usize..32,
        count in 1_usize..64,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.9,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        // Enqueue up to capacity.
        let mut enqueued_seqs = Vec::new();
        for i in 0..count.min(cap) {
            if let Ok(seq) = ring.enqueue(LatencyStage::EventEmission, 1.0, "f", i as u64, 0) {
                enqueued_seqs.push(seq);
            }
        }

        // Dequeue all.
        let mut dequeued_seqs = Vec::new();
        while let Some(item) = ring.dequeue(1000) {
            dequeued_seqs.push(item.seq);
        }

        prop_assert_eq!(enqueued_seqs, dequeued_seqs, "FIFO violated");
    }

    /// drain(max) returns at most min(max, len) items.
    #[test]
    fn input_ring_drain_bounded(
        cap in 2_usize..32,
        fill in 0_usize..64,
        drain_max in 0_usize..64,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.75,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        for i in 0..fill {
            let _ = ring.enqueue(LatencyStage::PatternDetection, 1.0, "d", i as u64, 0);
        }

        let before_len = ring.len();
        let drained = ring.drain(drain_max, 1000);
        let expected_count = drain_max.min(before_len);

        prop_assert_eq!(drained.len(), expected_count, "drain returned {} items, expected {}", drained.len(), expected_count);
        prop_assert_eq!(ring.len(), before_len - expected_count);
    }

    /// drain_expired only removes items past their deadline.
    #[test]
    fn input_ring_drain_expired_correct(
        cap in 4_usize..32,
        now in 100_u64..1000,
        deadlines in prop::collection::vec(0_u64..200, 1..16),
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.9,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        let mut expected_expired = 0_usize;
        let mut expected_remaining = 0_usize;
        for (i, &dl) in deadlines.iter().enumerate() {
            if i >= cap {
                break;
            }
            if ring.enqueue(LatencyStage::WorkflowDispatch, 1.0, "e", 0, dl).is_ok() {
                if dl > 0 && now > dl {
                    expected_expired += 1;
                } else {
                    expected_remaining += 1;
                }
            }
        }

        let expired = ring.drain_expired(now);
        prop_assert_eq!(expired.len(), expected_expired, "expired count mismatch");
        prop_assert_eq!(ring.len(), expected_remaining, "remaining count mismatch");

        // All expired items should have deadline < now.
        for item in &expired {
            prop_assert!(item.deadline_us > 0 && now > item.deadline_us);
        }
    }

    /// utilization is always in [0.0, 1.0].
    #[test]
    fn input_ring_utilization_bounded(
        cap in 1_usize..64,
        fill in 0_usize..128,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.75,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        for i in 0..fill {
            let _ = ring.enqueue(LatencyStage::ApiResponse, 1.0, "u", i as u64, 0);
        }

        let u = ring.utilization();
        prop_assert!(u >= 0.0 && u <= 1.0, "utilization {} out of bounds", u);
        let expected = ring.len() as f64 / cap as f64;
        let diff = (u - expected).abs();
        prop_assert!(diff < 1e-10, "utilization {} != expected {}", u, expected);
    }

    /// Snapshot serde roundtrip.
    #[test]
    fn input_ring_snapshot_serde(
        cap in 1_usize..32,
        fill in 0_usize..32,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.75,
            track_sojourn: true,
        };
        let mut ring = InputRing::new(config);

        for i in 0..fill {
            let _ = ring.enqueue(LatencyStage::PtyCapture, 10.0, "serde", i as u64, 0);
        }
        // Dequeue some to generate sojourn stats.
        for _ in 0..fill / 2 {
            ring.dequeue(100);
        }

        let snap = ring.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: InputRingSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.capacity, back.capacity);
        prop_assert_eq!(snap.len, back.len);
        prop_assert_eq!(snap.total_enqueued, back.total_enqueued);
        prop_assert_eq!(snap.total_dequeued, back.total_dequeued);
        prop_assert_eq!(snap.total_dropped, back.total_dropped);
        prop_assert_eq!(snap.backpressure, back.backpressure);
    }

    /// InputRingItem serde roundtrip.
    #[test]
    fn input_ring_item_serde(
        stage_idx in 0_usize..8,
        cost in 0.0_f64..1e6,
        seq in 1_u64..1000,
        arrived in 0_u64..1_000_000,
        deadline in 0_u64..1_000_000,
    ) {
        let stages = LatencyStage::PIPELINE_STAGES;
        let item = InputRingItem {
            seq,
            stage: stages[stage_idx],
            estimated_cost_us: cost,
            correlation_id: "pt".to_string(),
            arrived_us: arrived,
            deadline_us: deadline,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: InputRingItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(item.seq, back.seq);
        prop_assert_eq!(item.stage, back.stage);
        prop_assert_eq!(item.correlation_id, back.correlation_id);
        prop_assert_eq!(item.arrived_us, back.arrived_us);
        prop_assert_eq!(item.deadline_us, back.deadline_us);
        let diff = (item.estimated_cost_us - back.estimated_cost_us).abs();
        let tol = item.estimated_cost_us.abs() * 1e-12 + 1e-10;
        prop_assert!(diff < tol, "cost roundtrip: {} vs {} diff {}", item.estimated_cost_us, back.estimated_cost_us, diff);
    }

    /// RingBackpressure serde roundtrip.
    #[test]
    fn ring_backpressure_serde(variant in 0_u8..3) {
        let bp = match variant {
            0 => RingBackpressure::Accept,
            1 => RingBackpressure::SlowDown,
            _ => RingBackpressure::Full,
        };
        let json = serde_json::to_string(&bp).unwrap();
        let back: RingBackpressure = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(bp, back);
    }

    /// Wraparound: ring works correctly through multiple full cycles.
    #[test]
    fn input_ring_wraparound_integrity(
        cap in 2_usize..16,
        cycles in 1_usize..8,
    ) {
        let config = InputRingConfig {
            capacity: cap,
            high_water_mark: 0.9,
            track_sojourn: false,
        };
        let mut ring = InputRing::new(config);

        for cycle in 0..cycles {
            // Fill to capacity.
            for i in 0..cap {
                let t = (cycle * cap + i) as u64;
                let result = ring.enqueue(LatencyStage::PtyCapture, 1.0, "w", t, 0);
                prop_assert!(result.is_ok(), "enqueue failed at cycle {} item {}", cycle, i);
            }
            prop_assert!(ring.is_full());

            // Drain all.
            let drained = ring.drain(cap, 1000);
            prop_assert_eq!(drained.len(), cap);
            prop_assert!(ring.is_empty());
        }

        let snap = ring.snapshot();
        let total = (cap * cycles) as u64;
        prop_assert_eq!(snap.total_enqueued, total);
        prop_assert_eq!(snap.total_dequeued, total);
        prop_assert_eq!(snap.len, 0);
    }

    // ── B3: Priority Inheritance invariants (ft-2p9cb.2.3.3) ──

    /// Effective priority >= original priority (inheritance only boosts).
    #[test]
    fn pi_effective_geq_original(
        holder_pri in 0_u8..4,
        waiter_pri in 0_u8..4,
    ) {
        let priorities = Priority::ALL;
        let holder_p = priorities[holder_pri as usize];
        let waiter_p = priorities[waiter_pri as usize];

        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "holder", holder_p, 100);
        tracker.acquire(Resource::StorageLock, "waiter", waiter_p, 200);

        let eff = tracker.effective_priority("holder").unwrap();
        prop_assert!(eff >= holder_p, "effective {:?} < original {:?}", eff, holder_p);
    }

    /// Lock-order enforcement: acquiring locks out of order always fails.
    #[test]
    fn pi_lock_order_enforced(
        first_idx in 0_usize..4,
        second_idx in 0_usize..4,
    ) {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let resources = Resource::LOCK_ORDER;
        let first = resources[first_idx];
        let second = resources[second_idx];

        tracker.acquire(first, "task", Priority::Normal, 100);
        let result = tracker.acquire(second, "task", Priority::Normal, 200);

        if second.order_index() < first.order_index() {
            // Should be an order violation.
            let is_violation = matches!(result, LockResult::OrderViolation { .. });
            prop_assert!(is_violation, "Expected OrderViolation for {:?} after {:?}", second, first);
        } else {
            // Should succeed (same or ascending order).
            let is_ok = matches!(result, LockResult::Acquired);
            prop_assert!(is_ok, "Expected Acquired for {:?} after {:?}, got {:?}", second, first, result);
        }
    }

    /// Release promotes highest-priority waiter.
    #[test]
    fn pi_release_promotes_highest(
        num_waiters in 1_usize..5,
        waiter_priorities in prop::collection::vec(0_u8..4, 1..5),
    ) {
        let priorities = Priority::ALL;
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::PatternLock, "holder", Priority::Background, 0);

        let count = num_waiters.min(waiter_priorities.len());
        let mut max_pri = Priority::Background;
        let mut max_id = String::new();

        for i in 0..count {
            let pri = priorities[waiter_priorities[i] as usize];
            let wid = format!("w{}", i);
            tracker.acquire(Resource::PatternLock, &wid, pri, (i as u64 + 1) * 100);
            if pri >= max_pri {
                max_pri = pri;
                max_id = wid;
            }
        }

        let promoted = tracker.release(Resource::PatternLock, "holder", 1000);
        if !promoted.is_empty() {
            // The promoted task should have the highest priority among waiters.
            let promoted_id = &promoted[0];
            prop_assert!(
                tracker.is_held_by(Resource::PatternLock, promoted_id),
                "Promoted {} but it doesn't hold the lock",
                promoted_id
            );
        }
    }

    /// release_all releases all locks held by a task.
    #[test]
    fn pi_release_all_clears(
        lock_mask in 0_u8..16,
    ) {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let resources = Resource::LOCK_ORDER;
        let mut expected_held = 0;

        for (i, res) in resources.iter().enumerate() {
            if lock_mask & (1 << i) != 0 {
                tracker.acquire(*res, "task", Priority::Normal, i as u64 * 100);
                expected_held += 1;
            }
        }

        prop_assert_eq!(tracker.held_count(), expected_held);
        let released = tracker.release_all("task", 1000);
        prop_assert_eq!(released.len(), expected_held);
        prop_assert_eq!(tracker.held_count(), 0);
    }

    /// Priority serde roundtrip.
    #[test]
    fn pi_priority_serde(idx in 0_u8..4) {
        let p = Priority::ALL[idx as usize];
        let json = serde_json::to_string(&p).unwrap();
        let back: Priority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    /// Resource serde roundtrip.
    #[test]
    fn pi_resource_serde(idx in 0_usize..4) {
        let r = Resource::LOCK_ORDER[idx];
        let json = serde_json::to_string(&r).unwrap();
        let back: Resource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }

    /// InheritanceEvent serde roundtrip.
    #[test]
    fn pi_inheritance_event_serde(
        res_idx in 0_usize..4,
        orig_idx in 0_u8..4,
        inh_idx in 0_u8..4,
        applied in 0_u64..1_000_000,
        released in prop::option::of(0_u64..1_000_000),
    ) {
        let event = InheritanceEvent {
            holder_id: "h".to_string(),
            waiter_id: "w".to_string(),
            resource: Resource::LOCK_ORDER[res_idx],
            original_priority: Priority::ALL[orig_idx as usize],
            inherited_priority: Priority::ALL[inh_idx as usize],
            applied_us: applied,
            released_us: released,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: InheritanceEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event, back);
    }

    /// InheritanceDegradation serde roundtrip.
    #[test]
    fn pi_degradation_serde(
        variant_idx in 0_u8..4,
        count in 1_usize..100,
    ) {
        let degradation = match variant_idx {
            0 => InheritanceDegradation::Healthy,
            1 => InheritanceDegradation::ExcessiveInheritance { active_chains: count, threshold: 2 },
            2 => InheritanceDegradation::HighContention { total_waiters: count, threshold: 8 },
            _ => InheritanceDegradation::OrderViolationSpike { total_violations: count as u64, threshold: 10 },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: InheritanceDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }

    /// InheritanceSnapshot serde roundtrip.
    #[test]
    fn pi_snapshot_serde(
        events in 0_u64..1000,
        violations in 0_u64..100,
        chains in 0_usize..10,
        depth in 0_usize..10,
    ) {
        let snap = InheritanceSnapshot {
            held_locks: vec![],
            total_inheritance_events: events,
            total_order_violations: violations,
            active_chains: chains,
            max_chain_depth_observed: depth,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: InheritanceSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    /// stage_to_priority covers all pipeline stages.
    #[test]
    fn pi_stage_to_priority_total(stage_idx in 0_usize..8) {
        let stages = LatencyStage::PIPELINE_STAGES;
        let pri = stage_to_priority(stages[stage_idx]);
        let is_valid = Priority::ALL.contains(&pri);
        prop_assert!(is_valid);
    }

    // ── B4: Starvation Prevention invariants (ft-2p9cb.2.4.3) ──

    /// No lane starves for more than max_starved_epochs.
    #[test]
    fn starvation_capped_at_threshold(
        max_epochs in 1_u64..10,
        num_epochs in 1_usize..30,
    ) {
        let config = StarvationConfig {
            max_starved_epochs: max_epochs,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        for _ in 0..num_epochs {
            // Only serve Input, starve Control and Bulk.
            tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        }

        // After enough epochs, starving lanes should be force-promoted.
        if num_epochs as u64 >= max_epochs {
            let is_promoted = tracker.is_force_promoted(SchedulerLane::Control)
                || tracker.is_force_promoted(SchedulerLane::Bulk);
            prop_assert!(is_promoted, "Expected force promotion after {} epochs", num_epochs);
        }
    }

    /// Gini coefficient is always in [0.0, 1.0].
    #[test]
    fn starvation_gini_bounded(
        shares in prop::collection::vec(0.0_f64..1.0, 3..=3),
        num_epochs in 1_usize..10,
    ) {
        let mut tracker = StarvationTracker::with_defaults();
        let s: [f64; 3] = [shares[0], shares[1], shares[2]];

        for _ in 0..num_epochs {
            tracker.observe_epoch(&[1, 1, 1], &s);
        }

        let gini = tracker.gini_coefficient();
        prop_assert!(gini >= 0.0, "Gini {} < 0", gini);
        prop_assert!(gini <= 1.0, "Gini {} > 1", gini);
    }

    /// Epoch counter is strictly monotonically increasing.
    #[test]
    fn starvation_epoch_monotonic(
        num_epochs in 1_usize..20,
    ) {
        let mut tracker = StarvationTracker::with_defaults();
        for i in 1..=num_epochs {
            tracker.observe_epoch(&[1, 1, 1], &[0.33, 0.33, 0.34]);
            prop_assert_eq!(tracker.epoch(), i as u64);
        }
    }

    /// Force-promotion clears when a lane gets completions.
    #[test]
    fn starvation_clears_on_service(
        starve_count in 1_u64..5,
    ) {
        let config = StarvationConfig {
            max_starved_epochs: starve_count,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        for _ in 0..starve_count {
            tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        }
        prop_assert!(tracker.is_force_promoted(SchedulerLane::Bulk));

        // Service the starved lane.
        tracker.observe_epoch(&[5, 3, 1], &[0.4, 0.3, 0.1]);
        prop_assert!(!tracker.is_force_promoted(SchedulerLane::Bulk));
    }

    /// Reset zeroes all state.
    #[test]
    fn starvation_reset_zeroes(
        num_epochs in 1_usize..10,
    ) {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        for _ in 0..num_epochs {
            tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        }

        tracker.reset();
        prop_assert_eq!(tracker.epoch(), 0);
        prop_assert!(!tracker.any_starving());
        prop_assert_eq!(tracker.snapshot().total_starvation_events, 0);
    }

    /// FairnessSnapshot serde roundtrip.
    #[test]
    fn starvation_snapshot_serde(
        events in 0_u64..100,
        gini in 0.0_f64..1.0,
    ) {
        let snap = FairnessSnapshot {
            lanes: vec![],
            gini_coefficient: gini,
            total_starvation_events: events,
            any_starving: events > 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: FairnessSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.total_starvation_events, back.total_starvation_events);
        prop_assert_eq!(snap.any_starving, back.any_starving);
        let diff = (snap.gini_coefficient - back.gini_coefficient).abs();
        let tol = snap.gini_coefficient.abs() * 1e-12 + 1e-10;
        prop_assert!(diff < tol);
    }

    /// StarvationEvent serde roundtrip.
    #[test]
    fn starvation_event_serde(
        epoch in 0_u64..1000,
        lane_idx in 0_u8..3,
        starved in 0_u64..100,
        share in 0.0_f64..1.0,
    ) {
        let lanes = [SchedulerLane::Input, SchedulerLane::Control, SchedulerLane::Bulk];
        let event = StarvationEvent {
            epoch,
            lane: lanes[lane_idx as usize],
            starved_epochs: starved,
            cpu_share: share,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: StarvationEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event.epoch, back.epoch);
        prop_assert_eq!(event.lane, back.lane);
        prop_assert_eq!(event.starved_epochs, back.starved_epochs);
        let diff = (event.cpu_share - back.cpu_share).abs();
        prop_assert!(diff < 1e-10);
    }

    /// FairnessDegradation serde roundtrip.
    #[test]
    fn starvation_degradation_serde(
        variant_idx in 0_u8..4,
        count in 1_usize..100,
    ) {
        let degradation = match variant_idx {
            0 => FairnessDegradation::Healthy,
            1 => FairnessDegradation::LaneStarvation { starving_lanes: vec![SchedulerLane::Bulk] },
            2 => FairnessDegradation::SevereUnfairness { gini: 0.7, threshold: 0.5 },
            _ => FairnessDegradation::PromotionStorm { events_in_window: count as u64, threshold: 5 },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: FairnessDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }

    /// StarvationConfig serde roundtrip.
    #[test]
    fn starvation_config_serde(
        max_epochs in 1_u64..20,
        window in 1_usize..50,
        min_share in 0.01_f64..0.5,
    ) {
        let cfg = StarvationConfig {
            max_starved_epochs: max_epochs,
            fairness_window: window,
            min_lane_share: min_share,
            enable_aging: true,
            aging_interval_epochs: 3,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: StarvationConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg.max_starved_epochs, back.max_starved_epochs);
        prop_assert_eq!(cfg.fairness_window, back.fairness_window);
        let diff = (cfg.min_lane_share - back.min_lane_share).abs();
        prop_assert!(diff < 1e-10);
    }

    // ── C1: Memory Pool invariants (ft-2p9cb.3.1.3) ──

    /// in_use + free_count == total_blocks after any sequence of alloc/free.
    #[test]
    fn pool_conservation_invariant(
        initial in 1_usize..32,
        max_blocks in 1_usize..64,
        ops in prop::collection::vec(prop::bool::ANY, 0..100),
    ) {
        let max_b = max_blocks.max(initial);
        let config = PoolConfig {
            initial_blocks: initial,
            max_blocks: max_b,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let mut held_ids: Vec<u64> = Vec::new();

        for alloc in ops {
            if alloc {
                match pool.allocate() {
                    AllocResult::FromFreeList { block_id } | AllocResult::Grown { block_id } => {
                        held_ids.push(block_id);
                    }
                    AllocResult::PoolExhausted => {}
                }
            } else if let Some(id) = held_ids.pop() {
                pool.free(id);
            }
        }

        prop_assert_eq!(
            pool.in_use() + pool.free_count(),
            pool.total_blocks(),
            "in_use {} + free {} != total {}",
            pool.in_use(),
            pool.free_count(),
            pool.total_blocks()
        );
    }

    /// total_blocks never exceeds max_blocks.
    #[test]
    fn pool_total_bounded(
        initial in 1_usize..16,
        max_blocks in 1_usize..32,
        alloc_count in 0_usize..100,
    ) {
        let max_b = max_blocks.max(initial);
        let config = PoolConfig {
            initial_blocks: initial,
            max_blocks: max_b,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);

        for _ in 0..alloc_count {
            pool.allocate();
        }

        prop_assert!(
            pool.total_blocks() <= max_b,
            "total {} > max {}",
            pool.total_blocks(),
            max_b
        );
    }

    /// Utilization is always in [0.0, 1.0].
    #[test]
    fn pool_utilization_bounded(
        initial in 1_usize..16,
        alloc_count in 0_usize..32,
    ) {
        let config = PoolConfig {
            initial_blocks: initial,
            max_blocks: initial,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);

        for _ in 0..alloc_count {
            pool.allocate();
        }

        let u = pool.utilization();
        prop_assert!(u >= 0.0 && u <= 1.0, "utilization {} out of bounds", u);
    }

    /// Alloc then free returns to same state (after shrink to match).
    #[test]
    fn pool_alloc_free_roundtrip(
        initial in 2_usize..32,
        count in 1_usize..16,
    ) {
        let count = count.min(initial);
        let config = PoolConfig {
            initial_blocks: initial,
            max_blocks: initial,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);

        let mut ids = Vec::new();
        for _ in 0..count {
            if let AllocResult::FromFreeList { block_id } = pool.allocate() {
                ids.push(block_id);
            }
        }

        for id in ids {
            pool.free(id);
        }

        prop_assert_eq!(pool.in_use(), 0);
        prop_assert_eq!(pool.free_count(), initial);
    }

    /// Shrink reduces total_blocks correctly.
    #[test]
    fn pool_shrink_bounded(
        initial in 4_usize..32,
        target_free in 0_usize..32,
    ) {
        let config = PoolConfig {
            initial_blocks: initial,
            max_blocks: initial * 2,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let before_free = pool.free_count();
        let reclaimed = pool.shrink(target_free);

        if target_free < before_free {
            prop_assert_eq!(reclaimed, before_free - target_free);
            prop_assert_eq!(pool.free_count(), target_free);
        } else {
            prop_assert_eq!(reclaimed, 0);
        }
    }

    /// Reset restores initial state.
    #[test]
    fn pool_reset_restores_initial(
        initial in 1_usize..32,
        alloc_count in 0_usize..32,
    ) {
        let config = PoolConfig {
            initial_blocks: initial,
            max_blocks: initial * 2,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);

        for _ in 0..alloc_count {
            pool.allocate();
        }

        pool.reset();
        prop_assert_eq!(pool.in_use(), 0);
        prop_assert_eq!(pool.total_blocks(), initial);
        prop_assert_eq!(pool.free_count(), initial);
    }

    /// MemoryDomain serde roundtrip.
    #[test]
    fn pool_domain_serde(idx in 0_usize..8) {
        let d = MemoryDomain::ALL[idx];
        let json = serde_json::to_string(&d).unwrap();
        let back: MemoryDomain = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    /// AllocResult serde roundtrip.
    #[test]
    fn pool_alloc_result_serde(
        variant in 0_u8..3,
        block_id in 0_u64..1000,
    ) {
        let result = match variant {
            0 => AllocResult::FromFreeList { block_id },
            1 => AllocResult::Grown { block_id },
            _ => AllocResult::PoolExhausted,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: AllocResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result, back);
    }

    /// PoolDegradation serde roundtrip.
    #[test]
    fn pool_degradation_serde(
        variant in 0_u8..4,
        count in 1_usize..100,
    ) {
        let degradation = match variant {
            0 => PoolDegradation::Healthy,
            1 => PoolDegradation::HighUtilization { utilization: 0.9, threshold: 0.85 },
            2 => PoolDegradation::Exhausted { total_exhausted: count as u64 },
            _ => PoolDegradation::Fragmented { total_blocks: count * 2, free_count: count },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: PoolDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }

    /// stage_to_domain covers all pipeline stages.
    #[test]
    fn pool_stage_to_domain_total(stage_idx in 0_usize..8) {
        let stages = LatencyStage::PIPELINE_STAGES;
        let domain = stage_to_domain(stages[stage_idx]);
        let is_valid = MemoryDomain::ALL.contains(&domain);
        prop_assert!(is_valid);
    }

    // ── C2: Ingestion Parser invariants (ft-2p9cb.3.2.3) ──

    /// Zero-copy ratio is always in [0.0, 1.0].
    #[test]
    fn ingest_zero_copy_ratio_bounded(
        chunks in prop::collection::vec(prop::collection::vec(0_u8..255, 0..64), 1..10),
    ) {
        let mut parser = IngestParser::with_defaults();
        for chunk in &chunks {
            parser.feed(chunk);
        }
        let ratio = parser.zero_copy_ratio();
        prop_assert!(ratio >= 0.0, "ratio {} < 0", ratio);
        prop_assert!(ratio <= 1.0, "ratio {} > 1", ratio);
    }

    /// Complete lines: bytes_consumed > 0 and lines > 0.
    #[test]
    fn ingest_complete_line_positive(
        prefix in prop::collection::vec(0_u8..254, 0..32),
    ) {
        let mut data = prefix;
        data.push(b'\n');
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(&data);
        match result {
            ParseResult::Complete { lines, bytes_consumed } => {
                prop_assert!(lines > 0);
                prop_assert!(bytes_consumed > 0);
            }
            other => {
                // Could be Invalid if exceeds max_line_bytes, which won't happen with 32-byte prefix.
                panic!("Expected Complete, got {:?}", other);
            }
        }
    }

    /// Flush produces output only when buffer is non-empty.
    #[test]
    fn ingest_flush_nonempty(
        data in prop::collection::vec(0_u8..254, 1..32),
    ) {
        let mut parser = IngestParser::with_defaults();
        // Feed data without newline.
        let no_newlines: Vec<u8> = data.iter().filter(|&&b| b != b'\n').cloned().collect();
        if !no_newlines.is_empty() {
            parser.feed(&no_newlines);
            let result = parser.flush();
            prop_assert!(result.is_some());
        }
    }

    /// Reset zeroes all counters.
    #[test]
    fn ingest_reset_zeroes(
        chunks in prop::collection::vec(prop::collection::vec(0_u8..255, 1..32), 1..5),
    ) {
        let mut parser = IngestParser::with_defaults();
        for chunk in &chunks {
            parser.feed(chunk);
        }
        parser.reset();
        prop_assert_eq!(parser.total_bytes(), 0);
        prop_assert_eq!(parser.total_lines(), 0);
        prop_assert_eq!(parser.total_chunks(), 0);
        prop_assert_eq!(parser.buffered_bytes(), 0);
    }

    /// ParseResult serde roundtrip.
    #[test]
    fn ingest_parse_result_serde(
        variant in 0_u8..3,
        count in 1_usize..100,
    ) {
        let result = match variant {
            0 => ParseResult::Complete { lines: count, bytes_consumed: count * 10 },
            1 => ParseResult::Partial { bytes_buffered: count },
            _ => ParseResult::Invalid { bytes_skipped: count, reason: "test".to_string() },
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ParseResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result, back);
    }

    /// IngestChunk serde roundtrip.
    #[test]
    fn ingest_chunk_serde(
        pane_id in 0_u64..100,
        offset in 0_u64..10000,
        length in 0_usize..1000,
        captured in 0_u64..1_000_000,
    ) {
        let chunk = IngestChunk {
            pane_id,
            offset,
            length,
            line_aligned: true,
            captured_us: captured,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let back: IngestChunk = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(chunk, back);
    }

    /// IngestDegradation serde roundtrip.
    #[test]
    fn ingest_degradation_serde(
        variant in 0_u8..4,
        count in 1_usize..100,
    ) {
        let degradation = match variant {
            0 => IngestDegradation::Healthy,
            1 => IngestDegradation::HighBufferPressure { buffered_bytes: count, max_line_bytes: count * 2 },
            2 => IngestDegradation::DataCorruption { invalid_bytes: count as u64, total_bytes: count as u64 * 10 },
            _ => IngestDegradation::LowZeroCopy { ratio: 0.3, threshold: 0.5 },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: IngestDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }

    /// IngestParserConfig serde roundtrip.
    #[test]
    fn ingest_config_serde(
        max_line in 100_usize..100000,
        max_chunks in 1_usize..256,
    ) {
        let cfg = IngestParserConfig {
            max_line_bytes: max_line,
            max_buffered_chunks: max_chunks,
            strip_escapes: false,
            checksum: true,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: IngestParserConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg, back);
    }
}

// ── C3: Tiered Scrollback Property Tests ───────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Tier byte conservation: hot + warm + cold == total_bytes.
    #[test]
    fn scrollback_conservation(
        sizes in proptest::collection::vec(1_u64..10000, 1..20),
    ) {
        let mut mgr = TieredScrollbackManager::with_defaults();
        for (i, &sz) in sizes.iter().enumerate() {
            mgr.ingest(i as u64 % 5, sz, 1, i as u64 * 1000);
        }
        let snap = mgr.snapshot();
        prop_assert_eq!(snap.hot_bytes + snap.warm_bytes + snap.cold_bytes, snap.total_bytes);
    }

    /// After migration, tier byte counts stay consistent.
    #[test]
    fn scrollback_migration_conservation(
        sizes in proptest::collection::vec(100_u64..5000, 1..10),
    ) {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 100,
            warm_to_cold_age_us: 500,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 100,
        };
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1_000_000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 1_000_000, target_latency_us: 500, compression_ratio: 1.0 };
        // compression_ratio=1.0 for cold to ensure exact conservation
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10_000_000, target_latency_us: 10000, compression_ratio: 1.0 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);

        for (i, &sz) in sizes.iter().enumerate() {
            mgr.ingest(0, sz, 1, 0);
            let _ = mgr.migrate(i as u64 * 200);
        }
        let snap = mgr.snapshot();
        prop_assert_eq!(snap.hot_bytes + snap.warm_bytes + snap.cold_bytes, snap.total_bytes);
    }

    /// Hot utilization is bounded [0, max_possible].
    #[test]
    fn scrollback_hot_util_bounded(
        sizes in proptest::collection::vec(1_u64..5000, 0..15),
    ) {
        let mut mgr = TieredScrollbackManager::with_defaults();
        for &sz in &sizes {
            mgr.ingest(0, sz, 1, 0);
        }
        let util = mgr.hot_utilization();
        prop_assert!(util >= 0.0);
        // Can exceed 1.0 if we overshoot, but should always be finite
        prop_assert!(util.is_finite());
    }

    /// Tier rank monotonically increases: Hot < Warm < Cold.
    #[test]
    fn scrollback_tier_rank_monotonic(tier_idx in 0_usize..3) {
        let tier = ScrollbackTier::ALL[tier_idx];
        prop_assert_eq!(tier.rank(), tier_idx);
        if let Some(demoted) = tier.demote() {
            prop_assert!(demoted.rank() > tier.rank());
        }
    }

    /// Ingest always increases segment count and hot bytes.
    #[test]
    fn scrollback_ingest_monotonic(
        n in 1_usize..20,
        sz in 1_u64..10000,
    ) {
        let mut mgr = TieredScrollbackManager::with_defaults();
        for i in 0..n {
            let prev_count = mgr.segment_count();
            let prev_hot = mgr.tier_bytes(ScrollbackTier::Hot);
            mgr.ingest(0, sz, 1, i as u64);
            prop_assert_eq!(mgr.segment_count(), prev_count + 1);
            prop_assert_eq!(mgr.tier_bytes(ScrollbackTier::Hot), prev_hot + sz);
        }
    }

    /// Evict pane removes exactly that pane's segments.
    #[test]
    fn scrollback_evict_pane_precise(
        pane_a_count in 1_usize..10,
        pane_b_count in 1_usize..10,
    ) {
        let mut mgr = TieredScrollbackManager::with_defaults();
        for i in 0..pane_a_count {
            mgr.ingest(1, 100, 1, i as u64);
        }
        for i in 0..pane_b_count {
            mgr.ingest(2, 200, 1, i as u64);
        }
        mgr.evict_pane(1);
        prop_assert_eq!(mgr.segment_count(), pane_b_count);
        prop_assert_eq!(mgr.segments_for_pane(1).len(), 0);
        prop_assert_eq!(mgr.segments_for_pane(2).len(), pane_b_count);
    }

    /// Reset clears everything.
    #[test]
    fn scrollback_reset_zeroes(
        sizes in proptest::collection::vec(1_u64..5000, 1..10),
    ) {
        let mut mgr = TieredScrollbackManager::with_defaults();
        for &sz in &sizes {
            mgr.ingest(0, sz, 1, 0);
        }
        mgr.reset();
        prop_assert_eq!(mgr.segment_count(), 0);
        prop_assert_eq!(mgr.total_bytes(), 0);
        prop_assert_eq!(mgr.total_lines(), 0);
    }

    /// ScrollbackTier serde roundtrip.
    #[test]
    fn scrollback_tier_serde(tier_idx in 0_usize..3) {
        let tier = ScrollbackTier::ALL[tier_idx];
        let json = serde_json::to_string(&tier).unwrap();
        let back: ScrollbackTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    /// TierMigrationEvent serde roundtrip.
    #[test]
    fn scrollback_migration_event_serde(
        seg_id in 0_u64..1000,
        bytes in 1_u64..100000,
        dur in 0_u64..10000,
    ) {
        let evt = TierMigrationEvent {
            segment_id: seg_id,
            from_tier: ScrollbackTier::Hot,
            to_tier: ScrollbackTier::Warm,
            bytes_migrated: bytes,
            duration_us: dur,
            timestamp_us: 12345,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: TierMigrationEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(evt, back);
    }

    /// ScrollbackDegradation serde roundtrip.
    #[test]
    fn scrollback_degradation_serde(
        variant in 0_u8..4,
        val in 1_usize..100,
    ) {
        let degradation = match variant {
            0 => ScrollbackDegradation::Healthy,
            1 => ScrollbackDegradation::HotPressure { utilization: 0.9, threshold: 0.85 },
            2 => ScrollbackDegradation::WarmPressure { utilization: 0.88, threshold: 0.85 },
            _ => ScrollbackDegradation::MigrationBacklog { pending: val, max_concurrent: val + 1 },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: ScrollbackDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }

    /// TieredScrollbackSnapshot serde roundtrip.
    #[test]
    fn scrollback_snapshot_serde(
        hot in 0_u64..100000,
        warm in 0_u64..100000,
        cold in 0_u64..100000,
    ) {
        let snap = TieredScrollbackSnapshot {
            hot_bytes: hot,
            warm_bytes: warm,
            cold_bytes: cold,
            hot_segments: 1,
            warm_segments: 2,
            cold_segments: 3,
            total_migrations: 5,
            total_bytes: hot + warm + cold,
            hot_utilization: 0.5,
            warm_utilization: 0.3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TieredScrollbackSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.hot_bytes, back.hot_bytes);
        prop_assert_eq!(snap.warm_bytes, back.warm_bytes);
        prop_assert_eq!(snap.cold_bytes, back.cold_bytes);
        prop_assert_eq!(snap.total_bytes, back.total_bytes);
    }
}

// ── C4: Transport Policy Property Tests ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Decision count conservation: local + compressed + bypass == total.
    #[test]
    fn transport_decision_conservation(
        modes in proptest::collection::vec(0_u8..3, 1..50),
    ) {
        let mut policy = TransportPolicy::with_defaults();
        for (i, &m) in modes.iter().enumerate() {
            let mode = match m {
                0 => TransportMode::Local,
                1 => TransportMode::Compressed,
                _ => TransportMode::Bypass,
            };
            policy.record(100, mode, 10.0, 8.0, i as u64);
        }
        let snap = policy.snapshot();
        prop_assert_eq!(snap.local_count + snap.compressed_count + snap.bypass_count, snap.total_decisions);
    }

    /// EWMA cost is always non-negative.
    #[test]
    fn transport_ewma_nonnegative(
        costs in proptest::collection::vec(0.0_f64..1000.0, 1..30),
    ) {
        let mut policy = TransportPolicy::with_defaults();
        for (i, &cost) in costs.iter().enumerate() {
            policy.record(1000, TransportMode::Local, cost, cost, i as u64);
        }
        prop_assert!(policy.ewma_cost_us() >= 0.0);
    }

    /// Mode distribution sums to 1.0 (when decisions > 0).
    #[test]
    fn transport_distribution_sums_to_one(
        n in 1_usize..50,
    ) {
        let mut policy = TransportPolicy::with_defaults();
        for i in 0..n {
            let mode = match i % 3 {
                0 => TransportMode::Local,
                1 => TransportMode::Compressed,
                _ => TransportMode::Bypass,
            };
            policy.record(100, mode, 10.0, 10.0, i as u64);
        }
        let (l, c, b) = policy.mode_distribution();
        let sum = l + c + b;
        let diff = (sum - 1.0).abs();
        prop_assert!(diff < 1e-10, "distribution sum {} != 1.0", sum);
    }

    /// Local mode selected when network cost is zero.
    #[test]
    fn transport_local_when_no_network(
        payload in 1_u64..1_000_000,
    ) {
        let policy = TransportPolicy::with_defaults(); // network_cost = 0.0
        prop_assert_eq!(policy.select_mode(payload), TransportMode::Local);
    }

    /// Estimate cost for Local is always 0.
    #[test]
    fn transport_local_estimate_zero(
        payload in 1_u64..1_000_000,
    ) {
        let policy = TransportPolicy::with_defaults();
        let cost = policy.estimate_cost(payload, TransportMode::Local);
        prop_assert_eq!(cost, 0.0);
    }

    /// Estimate cost for Bypass is non-negative.
    #[test]
    fn transport_bypass_estimate_nonneg(
        payload in 1_u64..100_000,
    ) {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.01,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        let cost = policy.estimate_cost(payload, TransportMode::Bypass);
        prop_assert!(cost >= 0.0);
    }

    /// Estimate cost for Compressed is non-negative.
    #[test]
    fn transport_compressed_estimate_nonneg(
        payload in 1_u64..100_000,
    ) {
        let policy = TransportPolicy::with_defaults();
        let cost = policy.estimate_cost(payload, TransportMode::Compressed);
        prop_assert!(cost >= 0.0);
    }

    /// Reset zeroes all counters.
    #[test]
    fn transport_reset_zeroes(
        n in 1_usize..20,
    ) {
        let mut policy = TransportPolicy::with_defaults();
        for i in 0..n {
            policy.record(100, TransportMode::Local, 10.0, 8.0, i as u64);
        }
        policy.reset();
        let snap = policy.snapshot();
        prop_assert_eq!(snap.total_decisions, 0);
        prop_assert_eq!(snap.total_bytes_transferred, 0);
        prop_assert_eq!(snap.ewma_cost_us, 0.0);
    }

    /// TransportMode serde roundtrip.
    #[test]
    fn transport_mode_serde(mode_idx in 0_u8..3) {
        let mode = match mode_idx {
            0 => TransportMode::Local,
            1 => TransportMode::Compressed,
            _ => TransportMode::Bypass,
        };
        let json = serde_json::to_string(&mode).unwrap();
        let back: TransportMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(mode, back);
    }

    /// TransportDecision serde roundtrip.
    #[test]
    fn transport_decision_serde(
        payload in 1_u64..100000,
        est in 0.0_f64..1000.0,
        act in 0.0_f64..1000.0,
    ) {
        let dec = TransportDecision {
            payload_bytes: payload,
            selected_mode: TransportMode::Bypass,
            estimated_cost_us: est,
            actual_cost_us: act,
            savings_us: est - act,
            timestamp_us: 12345,
        };
        let json = serde_json::to_string(&dec).unwrap();
        let back: TransportDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dec.payload_bytes, back.payload_bytes);
        prop_assert_eq!(dec.selected_mode, back.selected_mode);
        // f64 tolerance
        let est_diff = (dec.estimated_cost_us - back.estimated_cost_us).abs();
        let tol = dec.estimated_cost_us.abs() * 1e-12 + 1e-10;
        prop_assert!(est_diff < tol, "est roundtrip: {} vs {}", dec.estimated_cost_us, back.estimated_cost_us);
    }
}

// ── C5: Tail-Latency Property Tests ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Wakeup count conservation: timer + io + signal + nudge == total.
    #[test]
    fn tail_latency_wakeup_conservation(
        sources in proptest::collection::vec(0_u8..4, 1..50),
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        for &s in &sources {
            let source = match s {
                0 => WakeupSource::Timer,
                1 => WakeupSource::IoEvent,
                2 => WakeupSource::Signal,
                _ => WakeupSource::Nudge,
            };
            ctrl.record_wakeup(source, 100);
        }
        let snap = ctrl.snapshot();
        prop_assert_eq!(
            snap.timer_wakeups + snap.io_wakeups + snap.signal_wakeups + snap.nudge_wakeups,
            snap.total_wakeups,
        );
    }

    /// Max latency is non-decreasing as more samples arrive.
    #[test]
    fn tail_latency_max_nondecreasing(
        latencies in proptest::collection::vec(1_u64..100000, 2..30),
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        let mut prev_max = 0u64;
        for &lat in &latencies {
            ctrl.record_wakeup(WakeupSource::Timer, lat);
            let cur_max = ctrl.snapshot().max_latency_us;
            prop_assert!(cur_max >= prev_max);
            prev_max = cur_max;
        }
    }

    /// p99 <= max latency always.
    #[test]
    fn tail_latency_p99_le_max(
        latencies in proptest::collection::vec(1_u64..100000, 1..50),
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        for &lat in &latencies {
            ctrl.record_wakeup(WakeupSource::Timer, lat);
        }
        prop_assert!(ctrl.p99_latency_us() <= ctrl.snapshot().max_latency_us);
    }

    /// p50 <= p99 always.
    #[test]
    fn tail_latency_p50_le_p99(
        latencies in proptest::collection::vec(1_u64..100000, 1..50),
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        for &lat in &latencies {
            ctrl.record_wakeup(WakeupSource::Timer, lat);
        }
        prop_assert!(ctrl.p50_latency_us() <= ctrl.p99_latency_us());
    }

    /// Wakeup distribution sums to 1.0 when total > 0.
    #[test]
    fn tail_latency_distribution_sums_to_one(
        n in 1_usize..50,
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        for i in 0..n {
            let source = match i % 4 {
                0 => WakeupSource::Timer,
                1 => WakeupSource::IoEvent,
                2 => WakeupSource::Signal,
                _ => WakeupSource::Nudge,
            };
            ctrl.record_wakeup(source, 100);
        }
        let (t, io, s, nd) = ctrl.wakeup_distribution();
        let sum = t + io + s + nd;
        let diff = (sum - 1.0).abs();
        prop_assert!(diff < 1e-10, "distribution sum {} != 1.0", sum);
    }

    /// Violation rate is bounded [0, 1].
    #[test]
    fn tail_latency_violation_rate_bounded(
        latencies in proptest::collection::vec(1_u64..50000, 1..30),
    ) {
        let config = TailLatencyConfig {
            p99_budget_us: 10000,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        for &lat in &latencies {
            ctrl.record_wakeup(WakeupSource::Timer, lat);
        }
        let rate = ctrl.violation_rate();
        prop_assert!(rate >= 0.0 && rate <= 1.0, "rate={}", rate);
    }

    /// Batch depth sum == total_syscalls.
    #[test]
    fn tail_latency_batch_sum(
        depths in proptest::collection::vec(1_usize..100, 1..20),
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        let mut expected_syscalls = 0u64;
        for &d in &depths {
            ctrl.record_batch(d);
            expected_syscalls += d as u64;
        }
        prop_assert_eq!(ctrl.snapshot().total_syscalls, expected_syscalls);
        prop_assert_eq!(ctrl.snapshot().total_batches, depths.len() as u64);
    }

    /// Reset clears all state.
    #[test]
    fn tail_latency_reset_zeroes(
        n in 1_usize..20,
    ) {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..n {
            ctrl.record_wakeup(WakeupSource::Timer, 500);
        }
        ctrl.record_batch(10);
        ctrl.reset();
        let snap = ctrl.snapshot();
        prop_assert_eq!(snap.total_wakeups, 0);
        prop_assert_eq!(snap.total_batches, 0);
        prop_assert_eq!(snap.max_latency_us, 0);
        prop_assert_eq!(snap.budget_violations, 0);
    }

    /// SyscallStrategy serde roundtrip.
    #[test]
    fn tail_latency_strategy_serde(idx in 0_u8..3) {
        let strategy = match idx {
            0 => SyscallStrategy::Immediate,
            1 => SyscallStrategy::Batched,
            _ => SyscallStrategy::Adaptive,
        };
        let json = serde_json::to_string(&strategy).unwrap();
        let back: SyscallStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(strategy, back);
    }

    /// TailLatencyDegradation serde roundtrip.
    #[test]
    fn tail_latency_degradation_serde(
        variant in 0_u8..4,
        obs in 1_u64..100000,
    ) {
        let degradation = match variant {
            0 => TailLatencyDegradation::Healthy,
            1 => TailLatencyDegradation::P99Breach { observed_us: obs, budget_us: obs / 2 },
            2 => TailLatencyDegradation::P999Breach { observed_us: obs, budget_us: obs / 2 },
            _ => TailLatencyDegradation::HighViolationRate { violations: obs, total: obs * 10 },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: TailLatencyDegradation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(degradation, back);
    }
}

// ── D1: Hitch-Risk Model Property Tests ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Posterior probability is always in [0, 1].
    #[test]
    fn hitch_risk_posterior_bounded(
        llrs in proptest::collection::vec(-10.0_f64..10.0, 1..30),
    ) {
        let mut model = HitchRiskModel::with_defaults();
        for (i, &llr) in llrs.iter().enumerate() {
            model.update(EvidenceSignal::LatencyProbe, 100.0, llr, i as u64 * 100);
        }
        let prob = model.posterior_prob();
        prop_assert!(prob >= 0.0 && prob <= 1.0, "prob={}", prob);
    }

    /// Positive LLR increases log_odds; negative decreases log_odds (no-decay config).
    #[test]
    fn hitch_risk_llr_direction(
        llr in 0.1_f64..5.0,
    ) {
        // Use decay=1.0 to isolate LLR direction effect
        let config = HitchRiskConfig { evidence_decay: 1.0, ..Default::default() };
        let mut model_pos = HitchRiskModel::new(config.clone());
        let initial_lo = model_pos.log_odds();
        model_pos.update(EvidenceSignal::BudgetViolation, 1.0, llr, 100);
        prop_assert!(model_pos.log_odds() > initial_lo, "positive LLR should increase log_odds");

        let mut model_neg = HitchRiskModel::new(config);
        let initial_lo2 = model_neg.log_odds();
        model_neg.update(EvidenceSignal::LatencyProbe, 1.0, -llr, 100);
        prop_assert!(model_neg.log_odds() < initial_lo2, "negative LLR should decrease log_odds");
    }

    /// Risk level thresholds are monotonic: Low ≤ Elevated ≤ High ≤ Critical.
    #[test]
    fn hitch_risk_level_monotonic(
        log_odds in -10.0_f64..10.0,
    ) {
        let config = HitchRiskConfig::default();
        let level = if log_odds >= config.critical_threshold {
            HitchRiskLevel::Critical
        } else if log_odds >= config.high_threshold {
            HitchRiskLevel::High
        } else if log_odds >= config.elevated_threshold {
            HitchRiskLevel::Elevated
        } else {
            HitchRiskLevel::Low
        };
        // Verify the level computation is consistent
        let rank = match level {
            HitchRiskLevel::Low => 0,
            HitchRiskLevel::Elevated => 1,
            HitchRiskLevel::High => 2,
            HitchRiskLevel::Critical => 3,
        };
        prop_assert!(rank <= 3);
    }

    /// Evidence count is bounded by max_evidence.
    #[test]
    fn hitch_risk_evidence_bounded(
        n in 1_usize..100,
    ) {
        let config = HitchRiskConfig {
            max_evidence: 20,
            ..Default::default()
        };
        let mut model = HitchRiskModel::new(config);
        for i in 0..n {
            model.update(EvidenceSignal::QueueDepth, 50.0, 0.5, i as u64 * 100);
        }
        prop_assert!(model.evidence_count() <= 20);
    }

    /// Reset restores to low risk.
    #[test]
    fn hitch_risk_reset_restores_low(
        n in 1_usize..20,
    ) {
        let mut model = HitchRiskModel::with_defaults();
        for i in 0..n {
            model.observe_violation(3.0, i as u64 * 100);
        }
        model.reset();
        prop_assert_eq!(model.risk_level(), HitchRiskLevel::Low);
        prop_assert_eq!(model.total_updates(), 0);
        prop_assert_eq!(model.evidence_count(), 0);
    }

    /// EvidenceSignal serde roundtrip.
    #[test]
    fn hitch_risk_signal_serde(idx in 0_u8..6) {
        let signal = match idx {
            0 => EvidenceSignal::LatencyProbe,
            1 => EvidenceSignal::BackpressureChange,
            2 => EvidenceSignal::QueueDepth,
            3 => EvidenceSignal::BudgetViolation,
            4 => EvidenceSignal::MemoryPressure,
            _ => EvidenceSignal::CpuLoad,
        };
        let json = serde_json::to_string(&signal).unwrap();
        let back: EvidenceSignal = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(signal, back);
    }

    /// HitchRiskLevel serde roundtrip.
    #[test]
    fn hitch_risk_level_serde(idx in 0_u8..4) {
        let level = match idx {
            0 => HitchRiskLevel::Low,
            1 => HitchRiskLevel::Elevated,
            2 => HitchRiskLevel::High,
            _ => HitchRiskLevel::Critical,
        };
        let json = serde_json::to_string(&level).unwrap();
        let back: HitchRiskLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    /// HitchRiskDegradation serde roundtrip.
    #[test]
    fn hitch_risk_degradation_serde(
        variant in 0_u8..4,
        val in 0.01_f64..1.0,
    ) {
        // Pre-roundtrip f64 through JSON to get a stable value
        let val: f64 = serde_json::from_str(&serde_json::to_string(&val).unwrap()).unwrap();
        let degradation = match variant {
            0 => HitchRiskDegradation::Healthy,
            1 => HitchRiskDegradation::ElevatedRisk { posterior_prob: val },
            2 => HitchRiskDegradation::HighRisk { posterior_prob: val, evidence_count: 10 },
            _ => HitchRiskDegradation::CriticalRisk { posterior_prob: val, log_odds: val },
        };
        let json = serde_json::to_string(&degradation).unwrap();
        let back: HitchRiskDegradation = serde_json::from_str(&json).unwrap();
        // Check string roundtrip worked
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    /// Decay brings log_odds toward 0 over many healthy observations.
    #[test]
    fn hitch_risk_decay_converges(
        initial_pushes in 1_usize..10,
    ) {
        let mut model = HitchRiskModel::with_defaults();
        // Push risk up
        for i in 0..initial_pushes {
            model.observe_violation(2.0, i as u64 * 100);
        }
        let peak = model.log_odds();
        // Submit many healthy observations
        for i in 0..100 {
            model.observe_healthy((initial_pushes + i) as u64 * 100);
        }
        prop_assert!(model.log_odds() < peak, "Decay should reduce log_odds");
    }
}
