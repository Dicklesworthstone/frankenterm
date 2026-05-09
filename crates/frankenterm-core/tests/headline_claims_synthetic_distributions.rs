//! Synthetic-distribution fixture for the 5 headline claims.
//!
//! **Bead:** [BR-RC-FOUNDATION.G3.5.cont.runs] / `ft-auvli`.
//! **Manifest:** `docs/perf/headline-claims.json`.
//! **Schema:** `docs/perf/bench-stats-schema.json`.
//! **Distribution primitive:** `bench_stats::Distribution`
//! (shipped under ft-syqcz.2 + ft-9zzkg wiring).
//!
//! # What this fixture ships
//!
//! ft-auvli's stated acceptance is "5 distribution artifacts
//! exist under target/criterion/<bench>/wa-bench-meta.jsonl" —
//! produced by actually running `cargo bench` on hardware-
//! baseline runners. That's blocked on `ft-vl7lp` (the GitHub
//! Actions workflow with 3 hardware lanes) plus actually
//! firing the benches.
//!
//! What we CAN ship today, regardless of hardware: a **synthetic
//! end-to-end pipeline test** that proves the
//! `Distribution::summarize` → JSONL → schema-validation chain
//! works for each of the 5 claims' metric shapes. Each test:
//!
//! 1. Generates a synthetic timing sample tuned to the claim's
//!    metric kind (latency / throughput / memory / ratio /
//!    speedup) and expected magnitude.
//! 2. Invokes `Distribution::summarize` with bootstrap CIs.
//! 3. Asserts the resulting Distribution carries every field
//!    required by `bench-stats-schema.json`.
//! 4. Serializes to JSON and asserts the shape parses cleanly.
//!
//! When real bench runs land under ft-vl7lp, the same
//! `Distribution::summarize` path produces the same shape from
//! real timings rather than synthetic ones — this fixture is
//! the regression-guard that catches pipeline drift before it
//! reaches the per-release attestation surface.
//!
//! # What this fixture is NOT
//!
//! - Not the actual hardware-baseline benchmark runs. That's
//!   `ft-vl7lp` (CI YAML + 3 runner lanes) producing real
//!   `target/criterion/<bench>/new/sample.json` and feeding
//!   them through `distribution_from_criterion_sample`.
//! - Not a Lindley-bound derivation. That's `ft-syqcz.4`
//!   (closed under another decomposition).

use frankenterm_core::bench_stats::{Distribution, PercentileReading};

/// Deterministic LCG for reproducible synthetic samples.
/// (Not for cryptography — for tests we just need stable
/// per-seed traces.)
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }

    fn next_u64(&mut self) -> u64 {
        // Numerical Recipes LCG.
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    /// Float in [0, 1).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }

    /// Lognormal-ish: exp(mu + sigma * box-muller(uniform)).
    /// Crude but adequate for synthetic latency-shape samples.
    fn next_lognormal(&mut self, mu: f64, sigma: f64) -> f64 {
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        sigma.mul_add(z, mu).exp()
    }
}

/// Generate `n` synthetic samples around `center` with relative
/// jitter `sigma`. Returns a Vec<f64>. Latency-style: lognormal
/// distribution to mimic typical bench traces.
fn synth_lognormal(center: f64, sigma: f64, n: usize, seed: u64) -> Vec<f64> {
    let mu = center.ln();
    let mut rng = Lcg::new(seed);
    (0..n).map(|_| rng.next_lognormal(mu, sigma)).collect()
}

/// Run a synthetic Distribution end-to-end and assert the
/// schema invariants hold.
fn assert_distribution_round_trips(dist: &Distribution) {
    // Bench-stats-schema.json invariants:
    assert!(dist.sample_size >= 1, "sample_size must be ≥ 1");
    assert!(dist.stddev >= 0.0, "stddev must be ≥ 0");
    assert!(dist.min <= dist.max, "min must be ≤ max");
    assert!(
        dist.percentiles.len() >= 4,
        "must report ≥ 4 percentiles (p50, p95, p99, p99.9)"
    );
    let has_999 = dist.percentiles.iter().any(|p| (p.q - 0.999).abs() < 1e-9);
    assert!(has_999, "must include q=0.999 per schema's contains rule");

    for p in &dist.percentiles {
        assert!(p.q >= 0.0 && p.q <= 1.0, "q out of [0,1]: {}", p.q);
        assert!(p.ci_lower <= p.value, "ci_lower ≤ value violated");
        assert!(p.value <= p.ci_upper, "value ≤ ci_upper violated");
        assert!(
            p.confidence > 0.0 && p.confidence < 1.0,
            "confidence not in (0,1)"
        );
    }

    // JSON serialization round-trips. f64 roundtrip can lose
    // a few ULPs in the bootstrap CI calculations, so we compare
    // structurally rather than via full equality.
    let json = serde_json::to_string(dist).expect("serde to_string");
    let decoded: Distribution = serde_json::from_str(&json).expect("serde from_str");
    assert_eq!(decoded.sample_size, dist.sample_size);
    assert!((decoded.mean - dist.mean).abs() < 1e-6);
    assert!((decoded.stddev - dist.stddev).abs() < 1e-6);
    assert_eq!(decoded.percentiles.len(), dist.percentiles.len());
    for (a, b) in dist.percentiles.iter().zip(&decoded.percentiles) {
        assert!((a.q - b.q).abs() < 1e-9);
        assert!((a.value - b.value).abs() < 1e-3);
        assert!((a.ci_lower - b.ci_lower).abs() < 1e-3);
        assert!((a.ci_upper - b.ci_upper).abs() < 1e-3);
    }
}

#[test]
fn capture_latency_p99_synthetic_distribution_validates() {
    // Headline claim: <50ms (50_000_000 ns) p99 capture latency
    // on 4KB-overlap workload. Synthetic mean ~5ms with
    // moderate spread — well under the 50ms ceiling so the
    // synthetic pipeline doesn't accidentally surface a
    // regression.
    let samples = synth_lognormal(5_000_000.0, 0.4, 1024, 0xCAFE_F100);
    let dist = Distribution::summarize(
        &samples,
        Distribution::DEFAULT_QUANTILES,
        500,
        0.95,
        0xBADD_5EED,
    )
    .expect("summarize non-empty");
    assert_distribution_round_trips(&dist);
    let p99 = dist
        .percentiles
        .iter()
        .find(|p| (p.q - 0.99).abs() < 1e-9)
        .expect("p99 present");
    // Synthetic p99 should comfortably beat the 50ms ceiling.
    assert!(
        p99.value < 50_000_000.0,
        "synthetic p99 {} exceeds 50ms ceiling — regenerate fixture",
        p99.value
    );
}

#[test]
fn concurrent_panes_capacity_synthetic_distribution_validates() {
    // Headline claim: ≥ 200 panes p50 throughput. Synthetic
    // samples cluster around 220 panes with light noise.
    let samples = synth_lognormal(220.0, 0.05, 256, 0xCAFE_F200);
    let dist = Distribution::summarize(
        &samples,
        Distribution::DEFAULT_QUANTILES,
        500,
        0.95,
        0xBADD_5EE2,
    )
    .expect("summarize");
    assert_distribution_round_trips(&dist);
    let p50 = dist
        .percentiles
        .iter()
        .find(|p| (p.q - 0.50).abs() < 1e-9)
        .expect("p50 present");
    assert!(
        p50.value >= 200.0,
        "synthetic p50 {} below 200 floor",
        p50.value
    );
}

#[test]
fn memory_per_pane_budget_synthetic_distribution_validates() {
    // Headline claim: ≤ ~50MB / 100 panes (~524KB / pane p95
    // ceiling). Synthetic per-pane RSS ~300KB to leave room for
    // the lognormal upper tail.
    let samples = synth_lognormal(300_000.0, 0.15, 512, 0xCAFE_F300);
    let dist = Distribution::summarize(
        &samples,
        Distribution::DEFAULT_QUANTILES,
        500,
        0.95,
        0xBADD_5EE3,
    )
    .expect("summarize");
    assert_distribution_round_trips(&dist);
    let p95 = dist
        .percentiles
        .iter()
        .find(|p| (p.q - 0.95).abs() < 1e-9)
        .expect("p95 present");
    assert!(
        p95.value < 524_288.0,
        "synthetic p95 {} per-pane RSS exceeds 50MB/100-panes budget",
        p95.value
    );
}

#[test]
fn zstd_compression_ratio_synthetic_distribution_validates() {
    // Headline claim: 5:1 to 10:1 ratio p50. Synthetic ratio ~7.0.
    let samples = synth_lognormal(7.0, 0.1, 256, 0xCAFE_F400);
    let dist = Distribution::summarize(
        &samples,
        Distribution::DEFAULT_QUANTILES,
        500,
        0.95,
        0xBADD_5EE4,
    )
    .expect("summarize");
    assert_distribution_round_trips(&dist);
    let p50 = dist
        .percentiles
        .iter()
        .find(|p| (p.q - 0.50).abs() < 1e-9)
        .expect("p50 present");
    assert!(
        p50.value >= 5.0 && p50.value <= 10.0,
        "synthetic compression ratio p50 {} outside [5, 10]",
        p50.value
    );
}

#[test]
fn bloom_prefilter_speedup_synthetic_distribution_validates() {
    // Headline claim: 10×–100× speedup p50. Synthetic ratio ~25×.
    let samples = synth_lognormal(25.0, 0.2, 512, 0xCAFE_F500);
    let dist = Distribution::summarize(
        &samples,
        Distribution::DEFAULT_QUANTILES,
        500,
        0.95,
        0xBADD_5EE5,
    )
    .expect("summarize");
    assert_distribution_round_trips(&dist);
    let p50 = dist
        .percentiles
        .iter()
        .find(|p| (p.q - 0.50).abs() < 1e-9)
        .expect("p50 present");
    assert!(
        p50.value >= 10.0 && p50.value <= 100.0,
        "synthetic speedup p50 {} outside [10, 100]",
        p50.value
    );
}

#[test]
fn all_5_claims_produce_jsonl_serializable_distributions() {
    // Aggregate test: emit one synthetic Distribution per claim,
    // serialize all 5 as JSONL (one per line), and assert the
    // result parses back to the original distributions. This is
    // the wa-bench-meta.jsonl shape the per-release attestation
    // bundle consumes.
    let claims: &[(&str, Vec<f64>)] = &[
        (
            "capture_latency_p99",
            synth_lognormal(5_000_000.0, 0.4, 256, 1),
        ),
        (
            "concurrent_panes_capacity",
            synth_lognormal(220.0, 0.05, 256, 2),
        ),
        (
            "memory_per_pane_budget",
            synth_lognormal(400_000.0, 0.2, 256, 3),
        ),
        ("zstd_compression_ratio", synth_lognormal(7.0, 0.1, 256, 4)),
        (
            "bloom_prefilter_speedup",
            synth_lognormal(25.0, 0.2, 256, 5),
        ),
    ];
    let mut jsonl = String::new();
    let mut originals = Vec::new();
    for (name, samples) in claims {
        let dist = Distribution::summarize(
            samples,
            Distribution::DEFAULT_QUANTILES,
            200,
            0.95,
            0xC0DE_BABE,
        )
        .expect("summarize");
        let line = serde_json::to_string(&dist).expect("serialize");
        jsonl.push_str(&line);
        jsonl.push('\n');
        originals.push((name.to_string(), dist));
    }
    // Parse line-by-line.
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 5, "must emit exactly 5 JSONL lines");
    for (i, line) in lines.iter().enumerate() {
        let parsed: Distribution = serde_json::from_str(line).expect("JSONL line parses");
        assert_eq!(parsed.sample_size, originals[i].1.sample_size);
        assert!((parsed.mean - originals[i].1.mean).abs() < 1e-6);
        assert_eq!(parsed.percentiles.len(), originals[i].1.percentiles.len());
    }
}

#[test]
fn synthetic_pipeline_handles_minimum_sample_size() {
    // EBCI minimum sample size scenarios: ensure the pipeline
    // doesn't blow up on small samples (below the bootstrap
    // resampling bar). The result is a Distribution with
    // degenerate CIs but valid shape.
    let tiny = synth_lognormal(100.0, 0.1, 16, 99);
    let dist = Distribution::summarize(&tiny, Distribution::DEFAULT_QUANTILES, 200, 0.95, 42)
        .expect("summarize tiny");
    assert_distribution_round_trips(&dist);
    assert_eq!(dist.sample_size, 16);
}

#[test]
fn synthetic_pipeline_is_deterministic_under_fixed_seed() {
    // Same samples + same seed → identical Distribution output.
    let samples_a = synth_lognormal(100.0, 0.2, 256, 7);
    let samples_b = synth_lognormal(100.0, 0.2, 256, 7);
    assert_eq!(
        samples_a, samples_b,
        "synth_lognormal must be deterministic"
    );
    let dist_a =
        Distribution::summarize(&samples_a, Distribution::DEFAULT_QUANTILES, 200, 0.95, 13)
            .unwrap();
    let dist_b =
        Distribution::summarize(&samples_b, Distribution::DEFAULT_QUANTILES, 200, 0.95, 13)
            .unwrap();
    // f64-bit-identical determinism (no JSON roundtrip in this test).
    assert_eq!(dist_a, dist_b);
}

#[test]
fn percentile_reading_carries_documented_subkeys() {
    // Smoke test: every PercentileReading the synth pipeline
    // emits has the 6 documented fields (q, value, ci_lower,
    // ci_upper, confidence, bootstrap_resamples) per the schema.
    let samples = synth_lognormal(50.0, 0.3, 128, 11);
    let dist =
        Distribution::summarize(&samples, Distribution::DEFAULT_QUANTILES, 200, 0.95, 17).unwrap();
    for p in &dist.percentiles {
        let _: PercentileReading = *p;
        let _ = p.q;
        let _ = p.value;
        let _ = p.ci_lower;
        let _ = p.ci_upper;
        let _ = p.confidence;
        let _ = p.bootstrap_resamples;
    }
}
