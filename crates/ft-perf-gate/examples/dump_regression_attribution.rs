use ft_perf_gate::causal_attribution::{attribute_regression_event, CausalAttributionConfig};
use ft_perf_gate::EvidenceSample;
use std::fs;
use std::path::PathBuf;

fn main() {
    // Anchor the fixture path on CARGO_MANIFEST_DIR (= crates/ft-perf-gate)
    // and navigate up two parents to reach the workspace root. The
    // previous form used a CWD-relative path which only worked when
    // cargo was invoked from the workspace root.
    let mut fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fixture_path.pop();
    fixture_path.pop();
    fixture_path.push("tests/fixtures/evidence-corpus/per-claim/robot.p95/regression-injected.jsonl");
    let body = fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("read fixture at {}: {e}", fixture_path.display()));
    let mut samples: Vec<EvidenceSample> = body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let change_point = 100;
    for (idx, sample) in samples.iter_mut().enumerate() {
        sample.commit_sha = Some(if idx < change_point { "fd89ed378-baseline" } else { "g43-regression-commit" }.to_string());
        sample.hardware_fingerprint = Some("fixture-m2-pro".to_string());
        sample.runner_sku = Some("github-hosted-macos-14-3core".to_string());
        sample.workload_class = Some("robot-mode-mixed-fixtures".to_string());
    }
    let cfg = CausalAttributionConfig {
        min_samples: 64,
        baseline: Some(4.2),
        min_alternative_support: 20,
        min_risk_lift: 0.20,
        ..CausalAttributionConfig::default()
    };
    let report = attribute_regression_event("robot-p95-regression-injected", &samples, &cfg);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
