//! W4.2 (follow-up slice) — BOCPD change-point detector UPDATE determinism +
//! input-safety conformance.
//!
//! The W4.2 BOCPD detector (`bocpd::BocpdModel`) is wired into the live watcher;
//! its change-point emissions feed snapshots, dedup, and replay goldens, so the
//! `update()` recursion must be reproducible bit-for-bit. The existing
//! `proptest_bocpd.rs` suite checks bounds, monotonicity, warmup, and serde
//! round-trips, but NOT update-stream determinism — flagged as an open
//! zero-RCH follow-up on the W4.T verification bead (ft-7h5da.5.6, cc_2 note).
//! This file closes that gap using the public API only (no source edits).
//!
//! Every property here is structurally guaranteed (the recursion is pure f64
//! arithmetic over a Vec posterior — no RNG, no clock, no map iteration order),
//! so the tests are a regression lock: they fail only if a future change
//! introduces non-determinism or breaks the non-finite-input guard.

use frankenterm_core::bocpd::{BocpdConfig, BocpdModel, ChangePoint};

/// A fixed, fully-deterministic multi-regime sequence: three level regimes
/// (≈1.0, ≈9.0, ≈-3.0), each with a small deterministic oscillation. All values
/// are finite. The exact detection outcome is intentionally NOT asserted (it is
/// covered by the property suite); these tests assert reproducibility and
/// input-safety, which hold regardless of where alarms fire.
fn multi_regime_sequence() -> Vec<f64> {
    let mut xs = Vec::with_capacity(120);
    for level in [1.0_f64, 9.0, -3.0] {
        for i in 0..40_u32 {
            // Deterministic micro-oscillation in [-0.10, +0.10] around the level.
            let osc = 0.05 * (f64::from(i % 5) - 2.0);
            xs.push(level + osc);
        }
    }
    xs
}

/// Field-wise equality for `Option<ChangePoint>` (ChangePoint does not derive
/// `PartialEq`); floats are compared by bit pattern so determinism is exact.
fn change_points_match(a: Option<&ChangePoint>, b: Option<&ChangePoint>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.observation_index == y.observation_index
                && x.posterior_probability.to_bits() == y.posterior_probability.to_bits()
                && x.map_run_length == y.map_run_length
        }
        _ => false,
    }
}

fn posteriors_match(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

#[test]
fn two_fresh_models_on_identical_sequence_are_bit_identical() {
    let xs = multi_regime_sequence();
    let mut a = BocpdModel::new(BocpdConfig::default());
    let mut b = BocpdModel::new(BocpdConfig::default());

    for (step, &x) in xs.iter().enumerate() {
        let ca = a.update(x);
        let cb = b.update(x);
        assert!(
            change_points_match(ca.as_ref(), cb.as_ref()),
            "change-point emission diverged at step {step}: {ca:?} vs {cb:?}"
        );

        // Every derived getter must agree, bit-for-bit, at every step.
        assert_eq!(
            a.observation_count(),
            b.observation_count(),
            "obs count @ {step}"
        );
        assert_eq!(
            a.change_point_count(),
            b.change_point_count(),
            "cp count @ {step}"
        );
        assert_eq!(
            a.map_run_length(),
            b.map_run_length(),
            "map run length @ {step}"
        );
        assert_eq!(a.in_warmup(), b.in_warmup(), "warmup @ {step}");
        assert_eq!(
            a.change_point_probability().to_bits(),
            b.change_point_probability().to_bits(),
            "cp probability @ {step}"
        );
        assert_eq!(
            a.posterior_sum().to_bits(),
            b.posterior_sum().to_bits(),
            "posterior sum @ {step}"
        );
        assert!(
            posteriors_match(&a.run_length_posterior(), &b.run_length_posterior()),
            "run-length posterior diverged at step {step}"
        );
    }
}

#[test]
fn change_point_stream_is_reproducible_across_runs() {
    let xs = multi_regime_sequence();

    let run = |seq: &[f64]| -> Vec<(u64, usize)> {
        let mut model = BocpdModel::new(BocpdConfig::default());
        let mut stream = Vec::new();
        for &x in seq {
            if let Some(cp) = model.update(x) {
                stream.push((cp.observation_index, cp.map_run_length));
            }
        }
        stream
    };

    let first = run(&xs);
    let second = run(&xs);
    assert_eq!(
        first, second,
        "the emitted change-point stream must be identical across independent runs"
    );
    // The reported stream length must equal the model's own change-point counter.
    let mut counter_model = BocpdModel::new(BocpdConfig::default());
    for &x in &xs {
        counter_model.update(x);
    }
    assert_eq!(
        counter_model.change_point_count() as usize,
        first.len(),
        "change_point_count() must equal the number of emitted change points"
    );
}

#[test]
fn non_finite_inputs_are_ignored_and_do_not_advance_state() {
    let xs = multi_regime_sequence();
    let mut model = BocpdModel::new(BocpdConfig::default());
    for &x in &xs {
        model.update(x);
    }

    // Snapshot the full observable state.
    let obs_before = model.observation_count();
    let cp_before = model.change_point_count();
    let map_before = model.map_run_length();
    let posterior_before = model.run_length_posterior();
    let prob_before = model.change_point_probability().to_bits();

    // Every non-finite input must be a no-op: returns None and mutates nothing
    // (guards the posterior against NaN/inf poisoning).
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            model.update(bad).is_none(),
            "non-finite input {bad} must not emit a change point"
        );
    }

    assert_eq!(
        model.observation_count(),
        obs_before,
        "non-finite inputs must not advance the observation count"
    );
    assert_eq!(model.change_point_count(), cp_before);
    assert_eq!(model.map_run_length(), map_before);
    assert_eq!(
        model.change_point_probability().to_bits(),
        prob_before,
        "non-finite inputs must not perturb the change-point probability"
    );
    assert!(
        posteriors_match(&model.run_length_posterior(), &posterior_before),
        "non-finite inputs must leave the run-length posterior untouched"
    );

    // Observation count equals the number of finite updates fed.
    assert_eq!(
        obs_before,
        xs.len() as u64,
        "every finite update must advance the observation count exactly once"
    );
}

#[test]
fn fresh_model_starts_in_warmup_with_no_change_points() {
    // Pure construction invariants — no updates yet.
    let model = BocpdModel::new(BocpdConfig::default());
    assert_eq!(model.observation_count(), 0);
    assert_eq!(model.change_point_count(), 0);
    assert!(
        model.in_warmup(),
        "a model with zero observations must report warmup"
    );
}
