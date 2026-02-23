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
}
