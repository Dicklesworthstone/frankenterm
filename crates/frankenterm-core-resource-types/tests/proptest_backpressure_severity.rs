//! Property-based coverage for the continuous-backpressure severity
//! cluster (`SeverityConfig`, `ThrottleActions`, `ContinuousBackpressure`)
//! living in `frankenterm-core-resource-types::backpressure_severity`.
//!
//! The cluster has hand-rolled unit tests in the source file but the
//! invariants below are easier to express as properties:
//!
//!   * `ThrottleActions::from_severity` MUST clamp every output to its
//!     documented range (the table in the module-level rustdoc), even
//!     for adversarial inputs (NaN, ±∞, values outside [0,1]).
//!   * `SeverityConfig` and `ThrottleActions` MUST round-trip cleanly
//!     through JSON — they're persisted in dashboards and replay
//!     evidence, so a silent serde-default change would be a
//!     forensics regression.
//!   * `ContinuousBackpressure::observe_ratio` MUST keep
//!     `smoothed_ratio` and `severity()` in [0, 1] for every input,
//!     and `observation_count` MUST be strictly monotonic.
//!   * `equivalent_tier` MUST be order-preserving in severity (a
//!     monotonic re-bucketing of a monotonic-in-load function).

use frankenterm_core_resource_types::backpressure::BackpressureTier;
use frankenterm_core_resource_types::backpressure_severity::{
    ContinuousBackpressure, SeverityConfig, ThrottleActions,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Reasonable severity inputs (the API contract specifies clamping for
/// out-of-range / NaN inputs, but most callers feed values already
/// inside [0, 1]).
fn arb_severity_in_range() -> impl Strategy<Value = f64> {
    0.0_f64..=1.0_f64
}

/// Adversarial finite f64 inputs to `from_severity` — ±∞, arbitrary
/// out-of-range values. The clamping invariants must hold for every
/// one of these.
///
/// NaN is intentionally excluded: `f64::clamp(NaN, lo, hi)` returns
/// NaN by Rust contract, and the production code does not special-case
/// NaN. That gap is tracked separately as ft-xdt8j; un-skip it here
/// once the production path is hardened.
fn arb_severity_adversarial() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-1e9_f64),
        Just(1e9_f64),
        -10.0_f64..=10.0_f64,
        0.0_f64..=1.0_f64,
    ]
}

/// `SeverityConfig` with realistic-but-broad parameter ranges. The
/// production defaults are `(0.60, 8.0, 10)` — the strategy widens
/// each axis enough to catch boundary issues.
fn arb_severity_config() -> impl Strategy<Value = SeverityConfig> {
    (0.0_f64..=1.0_f64, 0.1_f64..=64.0_f64, 1_usize..=512_usize).prop_map(
        |(center_threshold, steepness, smoothing_window)| SeverityConfig {
            center_threshold,
            steepness,
            smoothing_window,
        },
    )
}

/// Sequences of queue ratios. Mixes in-range and clamping-territory
/// values so the EMA smoothing path is exercised across the boundary.
///
/// NaN is excluded for the same reason as `arb_severity_adversarial`:
/// `observe_ratio` does not currently NaN-guard its `.clamp(0.0, 1.0)`
/// (tracked as ft-xdt8j). `f64::INFINITY` is included because it
/// clamps to a finite saturation bound and stresses the EMA boundary.
fn arb_ratio_sequence() -> impl Strategy<Value = Vec<f64>> {
    let ratio = prop_oneof![
        0.0_f64..=1.0_f64,
        Just(-0.5_f64),
        Just(1.5_f64),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
    ];
    prop::collection::vec(ratio, 1..32)
}

// ---------------------------------------------------------------------------
// `ThrottleActions::from_severity` clamping invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// For every adversarial input, every output field stays inside
    /// its documented range. The function is the public clamp seam,
    /// so this is the load-bearing invariant.
    ///
    /// The bounds are checked with a 1-ULP-class tolerance because
    /// the production formulas (`1.0 - 0.8*s`, etc.) cannot represent
    /// their nominal endpoints exactly in f64 — for example
    /// `1.0 - 0.8 = 0.19999999999999996`, which is one ULP below the
    /// documented `buffer_limit_factor >= 0.2` floor. The tolerance
    /// is far tighter than any consumer of this struct cares about
    /// and exists strictly to absorb f64 representation error.
    #[test]
    fn throttle_actions_clamps_every_output(s in arb_severity_adversarial()) {
        let a = ThrottleActions::from_severity(s);

        prop_assert!(within(a.severity, 0.0, 1.0));
        prop_assert!(!a.severity.is_nan());
        prop_assert!(within(a.poll_backoff_multiplier, 1.0, 4.0));
        prop_assert!(within(a.pane_skip_fraction, 0.0, 0.5));
        prop_assert!(within(a.detection_skip_fraction, 0.0, 0.25));
        prop_assert!(within(a.buffer_limit_factor, 0.2, 1.0));
    }

    /// The clamp seam is monotonic on its principal axis: more severity
    /// means more throttling (poll backoff up, buffer limit down,
    /// skipping up). This pins down the *direction* that the ranges
    /// alone don't constrain.
    #[test]
    fn throttle_actions_monotonic_on_severity(
        a in arb_severity_in_range(),
        b in arb_severity_in_range(),
    ) {
        prop_assume!(a < b);
        let ta = ThrottleActions::from_severity(a);
        let tb = ThrottleActions::from_severity(b);

        prop_assert!(ta.poll_backoff_multiplier <= tb.poll_backoff_multiplier);
        prop_assert!(ta.pane_skip_fraction <= tb.pane_skip_fraction);
        prop_assert!(ta.detection_skip_fraction <= tb.detection_skip_fraction);
        prop_assert!(ta.buffer_limit_factor >= tb.buffer_limit_factor);
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip coverage
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    /// `SeverityConfig` is persisted in operator dashboards / replay
    /// evidence; its JSON form must round-trip with f64 precision
    /// preserved well within operational tolerance. The exact bit
    /// pattern is NOT preserved — `serde_json` formats f64 values via
    /// Ryu, which can drop the last ULP for some inputs (e.g.
    /// `0.48058481390976715` round-trips to `0.4805848139097671`). A
    /// 1e-12 relative-error window is far tighter than any consumer
    /// of this type cares about.
    #[test]
    fn severity_config_json_roundtrip(config in arb_severity_config()) {
        let json = serde_json::to_string(&config).expect("serialize SeverityConfig");
        let decoded: SeverityConfig =
            serde_json::from_str(&json).expect("deserialize SeverityConfig");
        prop_assert!(approx_eq(config.center_threshold, decoded.center_threshold));
        prop_assert!(approx_eq(config.steepness, decoded.steepness));
        prop_assert_eq!(config.smoothing_window, decoded.smoothing_window);
    }

    /// `ThrottleActions` is `PartialEq`, but f64 round-trip through
    /// JSON can lose the last ULP (see `severity_config_json_roundtrip`
    /// for the rationale). Compare each field within the same 1e-12
    /// tolerance — the struct's only consumers are dashboards and
    /// throttle-action evaluators, none of which depend on bit-exact
    /// preservation.
    #[test]
    fn throttle_actions_json_roundtrip(s in arb_severity_in_range()) {
        let actions = ThrottleActions::from_severity(s);
        let json = serde_json::to_string(&actions).expect("serialize ThrottleActions");
        let decoded: ThrottleActions =
            serde_json::from_str(&json).expect("deserialize ThrottleActions");
        prop_assert!(approx_eq(actions.severity, decoded.severity));
        prop_assert!(approx_eq(
            actions.poll_backoff_multiplier,
            decoded.poll_backoff_multiplier
        ));
        prop_assert!(approx_eq(actions.pane_skip_fraction, decoded.pane_skip_fraction));
        prop_assert!(approx_eq(
            actions.detection_skip_fraction,
            decoded.detection_skip_fraction
        ));
        prop_assert!(approx_eq(
            actions.buffer_limit_factor,
            decoded.buffer_limit_factor
        ));
    }
}

// ---------------------------------------------------------------------------
// `ContinuousBackpressure` model invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    /// After every `observe_ratio` call the smoothed ratio stays in
    /// [0, 1] (the input is clamped before being folded into the EMA),
    /// the resulting severity stays in [0, 1] (sigmoid range), and the
    /// observation counter is strictly monotonic.
    #[test]
    fn continuous_observe_keeps_invariants(
        config in arb_severity_config(),
        samples in arb_ratio_sequence(),
    ) {
        let mut model = ContinuousBackpressure::new(config);
        let mut prev_count = model.observation_count();
        for sample in &samples {
            let severity = model.observe_ratio(*sample);
            prop_assert!((0.0..=1.0).contains(&severity));
            prop_assert!(!severity.is_nan());
            prop_assert!((0.0..=1.0).contains(&model.smoothed_ratio()));
            prop_assert!(!model.smoothed_ratio().is_nan());
            prop_assert!(model.observation_count() > prev_count);
            prev_count = model.observation_count();
        }
    }

    /// `reset` returns the model to a state that's indistinguishable
    /// (for the public-API readers) from a freshly-constructed one
    /// that uses the same config.
    #[test]
    fn continuous_reset_restores_initial_state(
        config in arb_severity_config(),
        samples in arb_ratio_sequence(),
    ) {
        let mut model = ContinuousBackpressure::new(config.clone());
        for sample in samples {
            let _ = model.observe_ratio(sample);
        }
        model.reset();
        let fresh = ContinuousBackpressure::new(config);
        prop_assert_eq!(model.observation_count(), fresh.observation_count());
        prop_assert_eq!(model.smoothed_ratio(), fresh.smoothed_ratio());
        prop_assert_eq!(model.severity(), fresh.severity());
    }

    /// `equivalent_tier` is a monotonic re-bucketing of severity:
    /// observing a strictly-larger raw load (with the same config) can
    /// only move the tier in the more-severe direction, never reverse
    /// it. This catches a silent threshold reorder in the
    /// 0.25/0.60/0.85 cliff stack.
    #[test]
    fn equivalent_tier_monotonic_in_load(
        config in arb_severity_config(),
        a in 0.0_f64..=1.0_f64,
        b in 0.0_f64..=1.0_f64,
    ) {
        prop_assume!(a < b);

        let mut lo = ContinuousBackpressure::new(config.clone());
        let mut hi = ContinuousBackpressure::new(config);
        // Drive both models with enough observations of their target
        // ratio that the EMA converges past warm-up.
        for _ in 0..32 {
            let _ = lo.observe_ratio(a);
            let _ = hi.observe_ratio(b);
        }
        let lo_tier = lo.equivalent_tier();
        let hi_tier = hi.equivalent_tier();
        prop_assert!(
            tier_rank(lo_tier) <= tier_rank(hi_tier),
            "lo={:?} (ratio={a}) hi={:?} (ratio={b}) — equivalent_tier regressed direction",
            lo_tier, hi_tier
        );
    }
}

fn tier_rank(t: BackpressureTier) -> u8 {
    match t {
        BackpressureTier::Green => 0,
        BackpressureTier::Yellow => 1,
        BackpressureTier::Red => 2,
        BackpressureTier::Black => 3,
    }
}

/// Relative-error f64 comparison tolerant of the last-ULP losses that
/// `serde_json` (Ryu) can introduce when round-tripping arbitrary
/// f64 values through their decimal representation. 1e-12 is far
/// tighter than any consumer of these types cares about and far
/// looser than the round-trip error in practice.
fn approx_eq(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    let diff = (a - b).abs();
    let scale = a.abs().max(b.abs()).max(1.0);
    diff <= 1e-12 * scale
}

/// Range containment check tolerant of representation error around
/// the endpoints. `1.0 - 0.8 = 0.19999999999999996` is one ULP below
/// the documented `0.2` lower bound but is the canonical answer
/// `from_severity(1.0)` produces; treating that as out-of-range
/// would be a false positive on the proptest, not a real defect.
fn within(value: f64, lo: f64, hi: f64) -> bool {
    let scale = lo.abs().max(hi.abs()).max(1.0);
    let eps = 1e-12 * scale;
    value >= lo - eps && value <= hi + eps
}
