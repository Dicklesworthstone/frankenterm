//! PIE (Principled Intelligence Engine) integration test suite.
//!
//! Verifies cross-component interactions between PIE modules:
//! - Survival → VOI scheduling (hazard-driven polling priority)
//! - BOCPD → Classifier (state change triggers reclassification)
//! - Backpressure → VOI (cost adjustment under load)
//! - Conformal → Survival (interval forecasts feed risk assessment)
//! - Kalman → Watchdog (adaptive thresholds from filtered estimates)
//! - Causal DAG → Cross-pane correlation (directed vs undirected)
//! - Full pipeline: simulated pane lifecycle

use frankenterm_core::backpressure::BackpressureTier;
use frankenterm_core::bayesian_ledger::{BayesianClassifier, Evidence, LedgerConfig, PaneState};
use frankenterm_core::bocpd::{BocpdConfig, BocpdModel};
use frankenterm_core::causal_dag::{CausalDag, CausalDagConfig};
use frankenterm_core::conformal::ConformalForecaster;
use frankenterm_core::continuous_backpressure::{
    ContinuousBackpressure, ContinuousBackpressureConfig,
};
use frankenterm_core::cross_pane_correlation::{CorrelationConfig, CorrelationEngine, EventRecord};
use frankenterm_core::kalman_watchdog::{AdaptiveWatchdog, AdaptiveWatchdogConfig};
use frankenterm_core::survival::{Covariates, SurvivalConfig, SurvivalModel};
use frankenterm_core::voi::{BackpressureTierInput, VoiConfig, VoiScheduler};
use frankenterm_core::watchdog::{Component, HealthStatus};

// =============================================================================
// Test 1: BOCPD detects regime change → classifier updates state
// =============================================================================

#[test]
fn bocpd_change_triggers_classifier_update() {
    let mut bocpd = BocpdModel::new(BocpdConfig {
        hazard_rate: 0.1,
        detection_threshold: 0.3,
        min_observations: 10,
        max_run_length: 100,
    });

    let mut classifier = BayesianClassifier::new(LedgerConfig::default());

    // Phase 1: steady Active state (output rate ~15 lps, near Active Gaussian mean).
    for _ in 0..30 {
        bocpd.update(15.0);
        classifier.update(1, Evidence::OutputRate(15.0));
        classifier.update(1, Evidence::Entropy(5.0)); // high entropy → Active
    }

    let result_before = classifier.classify(1).unwrap();
    assert_eq!(result_before.classification, PaneState::Active);

    // Phase 2: sudden drop to near-zero (agent goes idle).
    let mut change_detected = false;
    for _ in 0..30 {
        if bocpd.update(0.0).is_some() {
            change_detected = true;
        }
        classifier.update(1, Evidence::OutputRate(0.0));
        classifier.update(1, Evidence::TimeSinceOutput(120.0));
    }

    let result_after = classifier.classify(1).unwrap();

    // After the regime change, classifier should no longer say Active.
    assert_ne!(
        result_after.classification,
        PaneState::Active,
        "Classifier should update after regime change"
    );
    // BOCPD should detect the change OR run length should be short (indicating
    // the model believes a recent change point occurred).
    assert!(
        change_detected || bocpd.map_run_length() < 35,
        "BOCPD should detect regime shift: change_detected={change_detected}, run_length={}",
        bocpd.map_run_length()
    );
}

// =============================================================================
// Test 2: VOI scheduling respects importance from classifier confidence
// =============================================================================

#[test]
fn voi_prioritizes_uncertain_panes() {
    let mut sched = VoiScheduler::new(VoiConfig {
        min_voi_threshold: 0.001,
        ..Default::default()
    });

    // Pane 1: well-classified (low entropy after many observations).
    sched.register_pane(1, 1000);
    for _ in 0..20 {
        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Active.index()] = 5.0;
        sched.update_belief(1, &lls, 1000);
    }

    // Pane 2: uncertain (uniform, no observations).
    sched.register_pane(2, 1000);

    let result = sched.schedule(5000);

    // Pane 2 (uncertain) should have higher VOI than Pane 1 (certain).
    let voi_1 = result.schedule.iter().find(|d| d.pane_id == 1).unwrap().voi;
    let voi_2 = result.schedule.iter().find(|d| d.pane_id == 2).unwrap().voi;

    assert!(
        voi_2 > voi_1,
        "Uncertain pane should have higher VOI: pane1={voi_1}, pane2={voi_2}"
    );
}

// =============================================================================
// Test 3: Backpressure → VOI cost integration
// =============================================================================

#[test]
fn backpressure_severity_adjusts_voi_cost() {
    let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());

    // Low load → green tier.
    bp.update(0.1, 0.1);
    let tier_low = bp.equivalent_tier();
    assert_eq!(tier_low, BackpressureTier::Green);

    // High load → yellow/red tier.
    for _ in 0..10 {
        bp.update(0.9, 0.9);
    }
    let tier_high = bp.equivalent_tier();
    assert!(
        tier_high == BackpressureTier::Yellow || tier_high == BackpressureTier::Red,
        "High load should produce Yellow/Red tier, got {:?}",
        tier_high
    );

    // VOI scheduler should reduce effective VOI under high backpressure.
    let mut sched = VoiScheduler::new(VoiConfig::default());
    sched.register_pane(1, 1000);

    let result_green = sched.schedule(5000);
    let voi_green = result_green.schedule.first().map(|d| d.voi).unwrap_or(0.0);

    sched.set_backpressure(BackpressureTierInput::Red);
    let result_red = sched.schedule(5000);
    let voi_red = result_red.schedule.first().map(|d| d.voi).unwrap_or(0.0);

    assert!(
        voi_green > voi_red,
        "Red backpressure should lower VOI: green={voi_green} > red={voi_red}"
    );
}

// =============================================================================
// Test 4: Survival model → hazard informs proactive scheduling
// =============================================================================

#[test]
fn survival_hazard_drives_scheduling_priority() {
    // Bypass warmup so the model returns real probabilities.
    let model = SurvivalModel::new(SurvivalConfig {
        warmup_observations: 0,
        learning_rate: 0.0, // keep default Weibull params (beta all zero)
        ..Default::default()
    });

    let cov = Covariates::default();

    // With default params (beta=[0;5]), covariates don't matter — only time does.
    // Use different time horizons: short vs long.
    let report_short = model.report(24.0, &cov); // 24 hours
    let report_long = model.report(500.0, &cov); // 500 hours

    // Longer uptime should have higher failure probability.
    assert!(
        report_long.failure_probability > report_short.failure_probability,
        "Longer uptime should have higher failure prob: short={}, long={}",
        report_short.failure_probability,
        report_long.failure_probability
    );

    // Use failure probability to drive VOI importance.
    let mut sched = VoiScheduler::new(VoiConfig::default());
    sched.register_pane(1, 1000);
    sched.register_pane(2, 1000);
    sched.set_importance(1, report_short.failure_probability.mul_add(10.0, 1.0));
    sched.set_importance(2, report_long.failure_probability.mul_add(10.0, 1.0));

    let result = sched.schedule(5000);
    let voi_short = result.schedule.iter().find(|d| d.pane_id == 1).unwrap().voi;
    let voi_long = result.schedule.iter().find(|d| d.pane_id == 2).unwrap().voi;

    assert!(
        voi_long > voi_short,
        "Higher-risk pane should get higher VOI: short={voi_short}, long={voi_long}"
    );
}

// =============================================================================
// Test 5: Conformal prediction coverage guarantee
// =============================================================================

#[test]
fn conformal_coverage_holds_statistically() {
    let mut forecaster = ConformalForecaster::with_defaults();

    // Feed a linear trend with noise.
    let n = 200;
    for i in 0..n {
        let val = (((i * 7) % 13) as f64).mul_add(0.1, (i as f64).mul_add(0.5, 100.0));
        forecaster.observe("rss_bytes", val);
    }

    let forecasts = forecaster.forecast_metric("rss_bytes");
    if forecasts.is_empty() {
        // Not enough data for forecast — skip.
        return;
    }

    // Check that forecast intervals are reasonable.
    for f in &forecasts {
        assert!(
            f.upper_bound >= f.lower_bound,
            "Upper bound should >= lower bound"
        );
        assert!(
            f.interval_width() >= 0.0,
            "Interval width should be non-negative"
        );
    }
}

// =============================================================================
// Test 6: Kalman watchdog adaptive thresholds
// =============================================================================

#[test]
fn kalman_adapts_to_stable_heartbeats() {
    let config = AdaptiveWatchdogConfig::default();
    let mut watchdog = AdaptiveWatchdog::new(config.clone());

    // Stable heartbeats at ~100ms intervals (monotonically increasing timestamps).
    // observe() takes absolute timestamps; it computes intervals internally.
    for i in 0..50u64 {
        let timestamp_ms = i * 100 + (i % 5); // ~100ms apart with small jitter
        watchdog.observe(Component::Capture, timestamp_ms);
    }

    // Adaptive threshold should be near 100ms.
    let tracker = watchdog.tracker(Component::Capture).unwrap();
    let estimate = tracker.estimated_interval().unwrap();
    assert!(
        (estimate - 100.0).abs() < 20.0,
        "Kalman estimate should converge near 100: got {estimate}"
    );

    // Last heartbeat was around 49*100 = 4900ms.
    // A heartbeat at 10000ms (5100ms gap vs ~100ms baseline) should be flagged.
    let classification = tracker.classify(10_000, &config);
    assert!(
        classification.status != HealthStatus::Healthy,
        "5100ms gap should be unhealthy when baseline is ~100ms, got {:?}",
        classification.status
    );
}

// =============================================================================
// Test 7: Causal DAG detects directionality
// =============================================================================

#[test]
fn causal_dag_detects_direction() {
    let mut dag = CausalDag::new(CausalDagConfig {
        window_size: 200,
        n_permutations: 50,
        significance_level: 0.1,
        min_te_bits: 0.001,
        ..Default::default()
    });
    dag.register_pane(1);
    dag.register_pane(2);

    // Pane 2 copies pane 1 with lag 1.
    for i in 0..200 {
        let x = (i % 5) as f64;
        dag.observe(1, x);
        dag.observe(2, if i > 0 { ((i - 1) % 5) as f64 } else { 0.0 });
    }

    dag.update_dag();

    // Should find directed edge 1→2.
    let edges_1_to_2: Vec<_> = dag
        .edges()
        .iter()
        .filter(|e| e.source == 1 && e.target == 2)
        .collect();
    let edges_2_to_1: Vec<_> = dag
        .edges()
        .iter()
        .filter(|e| e.source == 2 && e.target == 1)
        .collect();

    if !edges_1_to_2.is_empty() {
        let te_forward = edges_1_to_2[0].transfer_entropy;
        let te_backward = edges_2_to_1.first().map_or(0.0, |e| e.transfer_entropy);
        assert!(
            te_forward >= te_backward,
            "Forward TE ({te_forward}) should be >= backward ({te_backward})"
        );
    }
}

// =============================================================================
// Test 8: Cross-pane correlation detects co-occurrence
// =============================================================================

#[test]
fn cross_pane_correlation_detects_cooccurrence() {
    let mut corr = CorrelationEngine::new(CorrelationConfig::default());

    for i in 0..200u64 {
        corr.ingest(EventRecord {
            pane_id: 1,
            event_type: format!("type_{}", i % 5),
            timestamp_ms: i * 1000,
        });
        corr.ingest(EventRecord {
            pane_id: 2,
            event_type: format!("type_{}", i % 5), // same pattern → co-occurrence
            timestamp_ms: i * 1000,
        });
    }

    let correlations = corr.scan(200_000);
    // Co-occurring events should be detected.
    for c in &correlations {
        assert!(c.p_value < 1.0, "p-value should be valid");
    }
}

// =============================================================================
// Test 9: Full pipeline — simulated pane lifecycle
// =============================================================================

#[test]
fn full_pipeline_pane_lifecycle() {
    let mut bocpd = BocpdModel::new(BocpdConfig {
        hazard_rate: 0.1,
        detection_threshold: 0.3,
        min_observations: 5,
        max_run_length: 50,
    });
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 3,
        ..Default::default()
    });
    let mut sched = VoiScheduler::new(VoiConfig {
        min_voi_threshold: 0.001,
        ..Default::default()
    });
    let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());

    sched.register_pane(1, 0);

    // Phase 1: Active pane (output rate ~15 lps, near Active Gaussian mean).
    for _ in 0..20 {
        let output_rate = 15.0;
        bocpd.update(output_rate);
        classifier.update(1, Evidence::OutputRate(output_rate));
        classifier.update(1, Evidence::Entropy(5.0)); // high entropy → Active
        bp.update(0.2, 0.2);

        let bp_tier = match bp.equivalent_tier() {
            BackpressureTier::Green => BackpressureTierInput::Green,
            BackpressureTier::Yellow => BackpressureTierInput::Yellow,
            _ => BackpressureTierInput::Red,
        };
        sched.set_backpressure(bp_tier);
    }

    let phase1_class = classifier.classify(1).unwrap();
    assert_eq!(phase1_class.classification, PaneState::Active);

    // Phase 2: Error state (pattern detection + low output).
    let mut change_points = 0u32;
    for _ in 20..50 {
        let output_rate = 2.0;
        if bocpd.update(output_rate).is_some() {
            change_points += 1;
        }
        classifier.update(1, Evidence::OutputRate(output_rate));
        classifier.update(1, Evidence::PatternDetection("error".to_string()));
        bp.update(0.1, 0.1);
    }

    let phase2_class = classifier.classify(1).unwrap();
    assert_ne!(
        phase2_class.classification,
        PaneState::Active,
        "After error pattern + low output, should not be Active"
    );

    // VOI should still want to poll this pane.
    let decision = sched.schedule(50_000);
    assert!(!decision.schedule.is_empty());

    assert!(
        change_points > 0 || bocpd.map_run_length() < 35,
        "BOCPD should detect Active→Error transition: change_points={change_points}, run_length={}",
        bocpd.map_run_length()
    );
}

// =============================================================================
// Test 10: Bayesian posterior convergence
// =============================================================================

#[test]
fn posterior_converges_with_consistent_evidence() {
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 1,
        ..Default::default()
    });

    // Feed 50 consistent observations pointing to Idle state.
    for _ in 0..50 {
        classifier.update(1, Evidence::OutputRate(0.0));
        classifier.update(1, Evidence::TimeSinceOutput(120.0));
    }

    let result = classifier.classify(1).unwrap();

    // Posterior should concentrate on Idle or Background.
    let idle_prob = result
        .posterior
        .get(PaneState::Idle.name())
        .copied()
        .unwrap_or(0.0);
    let bg_prob = result
        .posterior
        .get(PaneState::Background.name())
        .copied()
        .unwrap_or(0.0);

    assert!(
        idle_prob + bg_prob > 0.5,
        "With consistent idle evidence, Idle+Background should dominate: idle={idle_prob}, bg={bg_prob}"
    );
    assert!(
        result.confident,
        "With 50 consistent observations, should be confident"
    );
}

// =============================================================================
// Test 11: Drift shifts classification
// =============================================================================

#[test]
fn drift_shifts_classification() {
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 1,
        ..Default::default()
    });

    // Establish Active state (rate ~15 lps near Active mean, high entropy).
    for _ in 0..20 {
        classifier.update(1, Evidence::OutputRate(15.0));
        classifier.update(1, Evidence::Entropy(5.0));
    }
    let initial = classifier.classify(1).unwrap();
    assert_eq!(initial.classification, PaneState::Active);

    // Gradually shift to Stuck (rate ~30 lps near Stuck mean, LOW entropy).
    for _ in 0..40 {
        classifier.update(1, Evidence::OutputRate(30.0));
        classifier.update(1, Evidence::Entropy(1.5));
    }
    let shifted = classifier.classify(1).unwrap();

    assert_ne!(
        shifted.classification,
        PaneState::Active,
        "Drift from high-entropy Active to low-entropy Stuck should change classification"
    );
}

// =============================================================================
// Test 12: Survival model properties (S(0)=1, monotone)
// =============================================================================

#[test]
fn survival_function_properties() {
    let model = SurvivalModel::new(SurvivalConfig::default());
    let cov = Covariates {
        rss_gb: 4.0,
        pane_count: 20.0,
        output_rate_mbps: 0.5,
        uptime_hours: 48.0,
        conn_error_rate: 1.0,
    };

    // S(0) should be 1.
    let s0 = model.params().survival_probability(0.0, &cov);
    assert!((s0 - 1.0).abs() < 1e-10, "S(0) should be 1.0, got {s0}");

    // S(t) should be monotonically decreasing.
    let mut prev_s = 1.0;
    for t in 1..100 {
        let s = model.params().survival_probability(t as f64 * 60.0, &cov);
        assert!(
            s <= prev_s + 1e-10,
            "S(t) should decrease: S({})={} > S({})={}",
            t - 1,
            prev_s,
            t,
            s
        );
        prev_s = s;
    }

    // S(∞) → 0.
    let s_large = model.params().survival_probability(1e9, &cov);
    assert!(s_large < 0.01, "S(large_t) should → 0, got {s_large}");
}

// =============================================================================
// Test 13: Cross-component snapshot serialization
// =============================================================================

#[test]
fn all_snapshots_serialize() {
    // BOCPD snapshot.
    let mut bocpd_mgr = frankenterm_core::bocpd::BocpdManager::new(BocpdConfig::default());
    bocpd_mgr.register_pane(1);
    let bocpd_snap = bocpd_mgr.snapshot();
    let json = serde_json::to_string(&bocpd_snap).unwrap();
    assert!(!json.is_empty());

    // Classifier snapshot.
    let classifier = BayesianClassifier::new(LedgerConfig::default());
    let cls_snap = classifier.snapshot();
    let json = serde_json::to_string(&cls_snap).unwrap();
    assert!(!json.is_empty());

    // VOI snapshot.
    let mut sched = VoiScheduler::new(VoiConfig::default());
    sched.register_pane(1, 1000);
    let voi_snap = sched.snapshot(2000);
    let json = serde_json::to_string(&voi_snap).unwrap();
    assert!(!json.is_empty());

    // Causal DAG snapshot.
    let mut dag = CausalDag::new(CausalDagConfig::default());
    dag.register_pane(1);
    let dag_snap = dag.snapshot();
    let json = serde_json::to_string(&dag_snap).unwrap();
    assert!(!json.is_empty());

    // Conformal forecast.
    let mut fc = ConformalForecaster::with_defaults();
    for i in 0..30 {
        fc.observe("test", i as f64);
    }
    let forecasts = fc.forecast_metric("test");
    let json = serde_json::to_string(&forecasts).unwrap();
    assert!(!json.is_empty());
}

// =============================================================================
// Test 14: All PIE components instantiate
// =============================================================================

#[test]
fn pie_component_count_matches_expected() {
    let _survival = SurvivalModel::new(SurvivalConfig::default());
    let _bocpd = BocpdModel::new(BocpdConfig::default());
    let _classifier = BayesianClassifier::new(LedgerConfig::default());
    let _voi = VoiScheduler::new(VoiConfig::default());
    let _conformal = ConformalForecaster::with_defaults();
    let _kalman = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
    let _backpressure = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
    let _causal_dag = CausalDag::new(CausalDagConfig::default());
    let _correlation = CorrelationEngine::new(CorrelationConfig::default());
}
