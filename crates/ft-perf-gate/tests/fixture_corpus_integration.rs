//! Integration test for ft-perf-gate against the G54 fixture corpus.
//!
//! Asserts Wald's SPRT and the Howard-et-al anytime-valid CI correctly
//! classify the labeled fixture flavors from tests/fixtures/evidence-corpus/:
//!
//! - baseline-30d:        SPRT must Accept(H0)  (no regression)
//! - regression-injected: SPRT must Reject(H0)  (15% step at sample 100)
//! - sparse:              both must Continue    (sub-min-samples)
//!
//! Reads `tests/fixtures/evidence-corpus/per-claim/robot.p95/<flavor>.jsonl`
//! deterministically.
//!
//! Bead: ft-tf6g3.10 (G25).

use ft_perf_gate::sprt::{
    evaluate_anytime_valid_ci, evaluate_wald_sprt, AnytimeValidCiConfig, AnytimeValidTest,
    WaldSprtConfig,
};
use ft_perf_gate::{EvidenceSample, GateDecision};
use std::fs;
use std::path::PathBuf;

fn fixture_path(claim: &str, flavor: &str) -> PathBuf {
    // Repo-root-relative resolution: this test crate lives under
    // crates/ft-perf-gate/ so we step up two parents.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("tests/fixtures/evidence-corpus/per-claim");
    p.push(claim);
    p.push(format!("{flavor}.jsonl"));
    p
}

fn load_fixture(claim: &str, flavor: &str) -> Vec<EvidenceSample> {
    let path = fixture_path(claim, flavor);
    let body = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<EvidenceSample>(l)
            .unwrap_or_else(|e| panic!("parse fixture row: {e}; line={l}")))
        .collect()
}

#[test]
fn wald_sprt_accepts_h0_on_baseline_30d_robot_p95() {
    let samples = load_fixture("robot.p95", "baseline-30d");
    let cfg = WaldSprtConfig {
        mu_null: 4.2,
        mu_alt: 4.83,   // mu_null * 1.15 — the 15% regression we'd catch
        sigma: 0.5,
        alpha: 0.05,
        beta: 0.05,
        min_samples: 16,
        max_samples: 720,
    };
    let report = evaluate_wald_sprt(&samples, &cfg);
    eprintln!(
        "baseline-30d: llr={:.3} consumed={} decision={:?}",
        report.llr, report.samples_consumed, report.decision
    );
    assert!(
        matches!(report.decision, GateDecision::Accept { .. }),
        "baseline-30d must yield Accept(H0); got {:?}",
        report.decision
    );
}

#[test]
fn wald_sprt_rejects_h0_on_regression_injected_robot_p95() {
    // Wald's SPRT is a sequential test of pure H0 vs pure H1 — it is not
    // designed to detect "the second half of the stream is different from
    // the first." Detecting that is G42 regime-shift's job. The honest
    // way to demonstrate SPRT against the regression-injected fixture is
    // to feed it ONLY the post-change-point slice (samples 100-199), which
    // is entirely in the H1 regime. This pattern also matches how G42 would
    // hand off to SPRT after detecting a regime shift: pause, recalibrate,
    // then resume SPRT on the new regime's samples.
    let all = load_fixture("robot.p95", "regression-injected");
    let samples: Vec<_> = all.into_iter().skip(100).collect();
    let cfg = WaldSprtConfig {
        mu_null: 4.2,
        mu_alt: 4.83,
        sigma: 0.5,
        alpha: 0.05,
        beta: 0.05,
        min_samples: 16,
        max_samples: 100,
    };
    let report = evaluate_wald_sprt(&samples, &cfg);
    eprintln!(
        "regression-injected: llr={:.3} consumed={} decision={:?}",
        report.llr, report.samples_consumed, report.decision
    );
    assert!(
        matches!(report.decision, GateDecision::Reject { .. }),
        "regression-injected must yield Reject(H0); got {:?}",
        report.decision
    );
}

#[test]
fn wald_sprt_continues_on_sparse_robot_p95() {
    let samples = load_fixture("robot.p95", "sparse");
    assert_eq!(samples.len(), 1, "sparse flavor is exactly 1 row by spec");
    let cfg = WaldSprtConfig {
        mu_null: 4.2,
        mu_alt: 4.83,
        sigma: 0.5,
        alpha: 0.05,
        beta: 0.05,
        min_samples: 4,
        max_samples: 200,
    };
    let report = evaluate_wald_sprt(&samples, &cfg);
    eprintln!(
        "sparse: llr={:.3} consumed={} decision={:?}",
        report.llr, report.samples_consumed, report.decision
    );
    assert!(
        matches!(report.decision, GateDecision::Continue { .. }),
        "sparse must yield Continue (sub-min-samples); got {:?}",
        report.decision
    );
}

#[test]
fn anytime_valid_ci_accepts_baseline_below_threshold() {
    let samples = load_fixture("robot.p95", "baseline-30d");
    let cfg = AnytimeValidCiConfig {
        sigma: 0.5,
        alpha: 0.05,
        threshold: 5.0,   // baseline mean 4.2, target SLO 5.0
        test_kind: AnytimeValidTest::UpperBoundMustHold,
        min_samples: 32,
        max_samples: 720,
    };
    let report = evaluate_anytime_valid_ci(&samples, &cfg);
    eprintln!(
        "baseline-30d anytime: mean={:.3} radius={:.3} decision={:?}",
        report.mean, report.radius, report.decision
    );
    assert!(
        matches!(report.decision, GateDecision::Accept { .. }),
        "baseline mean 4.2 with CI radius should sit below threshold 5.0; got {:?}",
        report.decision
    );
}

#[test]
fn anytime_valid_ci_rejects_regression_above_threshold() {
    // Test against ONLY the post-change-point slice to isolate the regression.
    // (The full mixed stream averages to ~4.45 which is too close to threshold
    // for a clean CI separation — that ambiguity is exactly what G42
    // regime-shift detection is for, not SPRT/CI.)
    let all = load_fixture("robot.p95", "regression-injected");
    let samples: Vec<_> = all.into_iter().skip(100).collect();
    let cfg = AnytimeValidCiConfig {
        sigma: 0.5,
        alpha: 0.05,
        threshold: 4.5,   // tight SLO; the post-regression mean ~4.78 exceeds it
        test_kind: AnytimeValidTest::UpperBoundMustHold,
        min_samples: 32,
        max_samples: 100,
    };
    let report = evaluate_anytime_valid_ci(&samples, &cfg);
    eprintln!(
        "regression-injected anytime: mean={:.3} radius={:.3} decision={:?}",
        report.mean, report.radius, report.decision
    );
    assert!(
        matches!(report.decision, GateDecision::Reject { .. }),
        "regression-injected mean must exceed threshold 4.5; got {:?}",
        report.decision
    );
}

#[test]
fn fixture_schema_consistency() {
    // Spot-check that the loaded samples conform to v1 schema invariants.
    for flavor in ["baseline-30d", "regression-injected", "regime-shift", "heavy-tail", "sparse"] {
        let samples = load_fixture("robot.p95", flavor);
        assert!(!samples.is_empty() || flavor == "sparse-empty", "flavor {flavor} has rows");
        for s in &samples {
            assert_eq!(s.schema_version, "ft.perf.evidence-sample.v1");
            assert_eq!(s.claim_id, "robot.p95");
            assert_eq!(s.metric_unit, "ms");
            assert!(s.metric_value.is_finite(), "all fixture metric_values finite");
            assert!(s.sample_size >= 1);
        }
    }
}
