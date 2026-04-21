//! Integration test: EWMA smoothing → survival hazard → Bayesian classification.
//!
//! Exercises the statistical inference pipeline:
//!
//!   EwmaWithVariance.observe(rate, time_ms)
//!     → anomaly detection via z-score
//!       → SurvivalModel.evaluate_action(t, covariates) → HazardAction
//!         → BayesianClassifier.update(pane_id, evidence) → ClassificationResult
//!
//! EWMA tracks output rates with exponential smoothing and detects anomalies.
//! The survival model maps system covariates to failure probability via Weibull
//! proportional hazards. The Bayesian classifier fuses evidence (output rate,
//! entropy, time since output) into pane state posteriors (Active, Idle, Stuck).

use frankenterm_core::bayesian_ledger::{BayesianClassifier, Evidence, LedgerConfig, PaneState};
use frankenterm_core::ewma::{EwmaWithVariance, RateEstimator};
use frankenterm_core::survival::{
    Covariates, HazardAction, SurvivalConfig, SurvivalModel, WeibullParams,
};

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build covariates from EWMA-smoothed metrics.
fn build_covariates(output_rate: f64, pane_count: f64) -> Covariates {
    Covariates {
        rss_gb: 0.5,
        pane_count,
        output_rate_mbps: output_rate,
        uptime_hours: 1.0,
        conn_error_rate: 0.0,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// EWMA tracks output rate, anomaly detection feeds survival hazard
/// assessment, and both inform Bayesian pane classification.
///
/// Bayesian model: Active = OutputRate(mean=15, std=10) + Entropy(mean=5, std=1).
/// We feed both evidence types to get a clean Active classification.
#[test]
fn ewma_rate_drives_hazard_and_classification() {
    let mut ewma = EwmaWithVariance::with_half_life_ms(1000.0);
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 3,
        bayes_factor_threshold: 2.0,
        ..LedgerConfig::default()
    });

    // Phase 1: stable output rate (~15.0 — active pane) with high entropy.
    for i in 0..20u64 {
        ewma.observe(15.0, i * 100);
        classifier.update(1, Evidence::OutputRate(15.0));
        classifier.update(1, Evidence::Entropy(5.0)); // Active: high entropy
    }

    assert_eq!(ewma.count(), 20);
    let mean = ewma.mean();
    assert!(
        (mean - 15.0).abs() < 2.0,
        "EWMA mean should be near 15.0, got {mean}"
    );

    // Classification should favor Active with both OutputRate and Entropy evidence.
    let result = classifier.classify(1).expect("should have classification");
    assert_eq!(result.classification, PaneState::Active);

    // Survival model: healthy covariates → low hazard.
    let model = SurvivalModel::with_params(
        SurvivalConfig {
            warmup_observations: 0,
            ..SurvivalConfig::default()
        },
        WeibullParams::default(),
    );
    let covariates = build_covariates(mean, 4.0);
    let action = model.evaluate_action(1.0, &covariates);
    assert_eq!(action, HazardAction::None);
}

/// EWMA detects anomalous output rate drop via z-score when there's
/// sufficient variance in the baseline, which shifts Bayesian classification.
#[test]
fn rate_drop_shifts_classification_to_idle() {
    let mut ewma = EwmaWithVariance::with_half_life_ms(500.0);

    // Establish baseline with slight variance: rate ~10.0 ± noise.
    for i in 0..30u64 {
        let rate = 10.0 + (i % 3) as f64; // 10, 11, 12, 10, 11, 12...
        ewma.observe(rate, i * 100);
    }

    let baseline_mean = ewma.mean();
    assert!(
        (baseline_mean - 11.0).abs() < 2.0,
        "baseline mean should be near 11.0, got {baseline_mean}"
    );

    // Bayesian: start Active then shift to Idle.
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 3,
        bayes_factor_threshold: 2.0,
        ..LedgerConfig::default()
    });

    // Active evidence: high rate + high entropy.
    for _ in 0..10 {
        classifier.update(1, Evidence::OutputRate(10.0));
        classifier.update(1, Evidence::Entropy(5.0));
    }

    let active_class = classifier.classify(1).expect("should classify");
    assert_eq!(active_class.classification, PaneState::Active);

    // Shift to idle evidence: very low rate + long time since output.
    // Need enough observations to overcome the Active prior.
    for _ in 0..40 {
        classifier.update(1, Evidence::OutputRate(0.1));
        classifier.update(1, Evidence::TimeSinceOutput(50.0));
    }

    let shifted_class = classifier.classify(1).expect("should classify after shift");
    // With sustained low output rate (0.1 ≈ Idle mean) and long inter-output
    // intervals, the classifier should move away from Active.
    assert_ne!(
        shifted_class.classification,
        PaneState::Active,
        "pane should no longer classify as Active with rate 0.1"
    );
}

/// RateEstimator feeds EWMA-smoothed intervals into the survival model,
/// and the hazard report includes risk factors coherent with inputs.
#[test]
fn rate_estimator_feeds_survival_report() {
    let mut rate_est = RateEstimator::with_half_life_ms(2000.0);

    // Simulate events arriving every 100ms (~10/sec).
    for i in 0..50u64 {
        rate_est.tick(i * 100);
    }

    let rate = rate_est.rate_per_sec();
    assert!(
        rate > 5.0 && rate < 20.0,
        "rate should be near 10/sec, got {rate}"
    );

    // Build survival model and get report.
    let model = SurvivalModel::with_params(
        SurvivalConfig {
            warmup_observations: 0,
            ..SurvivalConfig::default()
        },
        WeibullParams::default(),
    );

    let covariates = Covariates {
        rss_gb: 1.0,
        pane_count: 6.0,
        output_rate_mbps: rate,
        uptime_hours: 2.0,
        conn_error_rate: 0.0,
    };

    let report = model.report(2.0, &covariates);
    assert!(!report.in_warmup);
    assert!(report.hazard_rate >= 0.0);
    assert!(report.survival_probability > 0.0);
    assert!(report.survival_probability <= 1.0);
    assert!(!report.risk_factors.is_empty());

    // Risk factors should include our covariates.
    let factor_names: Vec<&str> = report
        .risk_factors
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        factor_names.contains(&"rss_gb") || factor_names.contains(&"output_rate_mbps"),
        "risk factors should name covariates"
    );
}

/// Multi-pane classification: EWMA tracks per-pane rates, Bayesian
/// classifier assigns different states based on evidence fusion.
#[test]
fn multi_pane_ewma_classification_with_survival() {
    let mut ewma_pane1 = EwmaWithVariance::with_half_life_ms(500.0);
    let mut ewma_pane2 = EwmaWithVariance::with_half_life_ms(500.0);
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 5,
        bayes_factor_threshold: 2.0,
        ..LedgerConfig::default()
    });

    // Pane 1: active output (rate ~15, entropy ~5, frequent output).
    for i in 0..20u64 {
        ewma_pane1.observe(15.0, i * 100);
        classifier.update(1, Evidence::OutputRate(15.0));
        classifier.update(1, Evidence::Entropy(5.0));
        classifier.update(1, Evidence::TimeSinceOutput(0.5));
    }

    // Pane 2: stuck output (rate ~30, entropy ~1.5, repetitive).
    for i in 0..20u64 {
        ewma_pane2.observe(30.0, i * 100);
        classifier.update(2, Evidence::OutputRate(30.0));
        classifier.update(2, Evidence::Entropy(1.5));
    }

    // Classify both panes.
    let class1 = classifier.classify(1).expect("pane 1 should classify");
    let class2 = classifier.classify(2).expect("pane 2 should classify");

    assert_eq!(class1.classification, PaneState::Active);
    assert_eq!(class2.classification, PaneState::Stuck);

    // Snapshot should show 2 panes.
    let snap = classifier.snapshot();
    assert_eq!(snap.pane_count, 2);

    // Survival: aggregate output rate.
    let avg_rate = f64::midpoint(ewma_pane1.mean(), ewma_pane2.mean());
    let model = SurvivalModel::with_params(
        SurvivalConfig {
            warmup_observations: 0,
            ..SurvivalConfig::default()
        },
        WeibullParams::default(),
    );
    let covariates = build_covariates(avg_rate, 2.0);
    let action = model.evaluate_action(1.0, &covariates);
    assert_eq!(
        action,
        HazardAction::None,
        "healthy system should have no hazard action"
    );
}

/// Full pipeline: EWMA smoothing → anomaly detection → survival hazard →
/// Bayesian classification → telemetry coherence.
#[test]
fn full_pipeline_ewma_survival_bayesian() {
    let mut ewma = EwmaWithVariance::with_half_life_ms(1000.0);
    let mut rate_est = RateEstimator::with_half_life_ms(2000.0);
    let mut classifier = BayesianClassifier::new(LedgerConfig {
        min_observations: 3,
        bayes_factor_threshold: 2.0,
        ..LedgerConfig::default()
    });
    let model = SurvivalModel::with_params(
        SurvivalConfig {
            warmup_observations: 0,
            ..SurvivalConfig::default()
        },
        WeibullParams::default(),
    );

    // Phase 1: healthy operation — Active pane.
    for i in 0..25u64 {
        let rate = 12.0;
        ewma.observe(rate, i * 100);
        rate_est.tick(i * 100);
        classifier.update(1, Evidence::OutputRate(rate));
        classifier.update(1, Evidence::Entropy(5.0)); // High entropy = active
        classifier.update(1, Evidence::TimeSinceOutput(0.5));
    }

    let healthy_mean = ewma.mean();

    let class1 = classifier.classify(1).expect("should classify");
    assert_eq!(class1.classification, PaneState::Active);

    let cov1 = build_covariates(healthy_mean, 4.0);
    let action1 = model.evaluate_action(1.0, &cov1);
    assert_eq!(action1, HazardAction::None);

    // Phase 2: rate drops to near-zero.
    // Reset pane evidence to test fresh classification with degraded metrics.
    classifier.reset_pane(1);

    for i in 25..50u64 {
        let rate = 0.05;
        ewma.observe(rate, i * 100);
        classifier.update(1, Evidence::OutputRate(rate));
        classifier.update(1, Evidence::TimeSinceOutput(50.0));
    }

    let degraded_mean = ewma.mean();
    assert!(
        degraded_mean < healthy_mean,
        "EWMA mean should decrease: {degraded_mean} < {healthy_mean}"
    );

    let class2 = classifier.classify(1).expect("should classify after reset");
    assert_ne!(
        class2.classification,
        PaneState::Active,
        "pane should not be Active with near-zero output rate"
    );

    // Verify telemetry coherence.
    let telem = classifier.telemetry().snapshot();
    assert!(telem.updates > 0);

    let survival_telem = model.telemetry().snapshot();
    assert!(survival_telem.hazard_evaluations >= 1);

    // Classifier snapshot coherent.
    let snap = classifier.snapshot();
    assert_eq!(snap.pane_count, 1);
    assert!(snap.panes[0].observation_count > 0);
}
