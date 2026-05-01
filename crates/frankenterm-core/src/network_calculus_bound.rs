//! Network-calculus latency bounds substrate (ft-syqcz.4).
//!
//! Pure-math primitives for deriving Lindley-equation-based latency
//! bounds on the headline `<50ms` claim. Composes with the existing
//! `latency_stages.rs` infrastructure (29kLOC of min-plus algebra)
//! by giving callers a typed surface for arrival curves, service
//! curves, and the per-stage Lindley delay computation.
//!
//! The substrate ships:
//!
//! - `ArrivalCurve` — token-bucket model `α(t) = b + r·t` (burst `b`
//!   + rate `r`).
//! - `ServiceCurve` — rate-latency model `β(t) = R·(t − T)+` (rate
//!   `R`, latency `T`, with `(x)+ = max(x, 0)`).
//! - `delay_bound` — computes the worst-case delay
//!   `h(α, β) = T + b/R` (horizontal deviation between α and β).
//! - `backlog_bound` — peak queue depth `b + r·T` (vertical
//!   deviation at `t=0`).
//! - `StageModel` — one stage in the pipeline (capture / extract /
//!   storage). Holds an `ArrivalCurve` (input) and a
//!   `ServiceCurve` (this stage's resource).
//! - `compose_serial` — for tandem stages, the end-to-end service
//!   curve is the min-plus convolution; for two rate-latency
//!   curves with rates `R1, R2` and latencies `T1, T2`, this is
//!   `min(R1, R2)·(t − (T1 + T2))+` (Pay Bursts Only Once
//!   theorem). Pure-logic, no I/O.
//! - `pipeline_delay_bound` — total bound across N stages.
//! - `EmpiricalComparison::within_tolerance` — checks the bead's
//!   "if analytical and empirical disagree by >20%, that's a bug
//!   worth root-causing" rule.
//! - `LindleyBoundsArtifact` — pure-data record matching the
//!   bead's `docs/attestations/perf/lindley-bounds.json` schema.
//!
//! ## What is deferred to the integration bead (ft-syqcz.4.cont)
//!
//! - `docs/perf/latency-derivation.md` — operator-facing markdown
//!   showing the math + curves visualised.
//! - Wiring into `latency_stages.rs`'s existing min-plus algebra to
//!   pull live rate/burst measurements for each stage.
//! - Cross-check empirical p99 from G3.3 against the analytical
//!   bound; alert if disagreement exceeds 20%.
//! - `docs/attestations/perf/lindley-bounds.json` regen as part of
//!   release attestation.

#![allow(dead_code)]

// ============================================================================
// Arrival curve (token-bucket model)
// ============================================================================

/// Token-bucket arrival curve: `α(t) = b + r·t`. The flow is
/// `(b, r)`-bounded — at most `b` units arrive in any instant
/// (burst), and the long-term rate is at most `r`.
///
/// Invariants enforced at construction: `burst >= 0` and `rate >= 0`.
/// `try_new` returns `None` on negative input. Both must be finite.
///
/// Per ft-cjc4l fix: fields are private. The validating
/// constructors `try_new` / `new` are the only construction
/// path. Accessors are `burst()` and `rate()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrivalCurve {
    burst: f64,
    rate: f64,
}

impl ArrivalCurve {
    /// Construct, returning `None` on non-finite or negative input.
    #[must_use]
    pub fn try_new(burst: f64, rate: f64) -> Option<Self> {
        if !burst.is_finite() || !rate.is_finite() || burst < 0.0 || rate < 0.0 {
            return None;
        }
        Some(Self { burst, rate })
    }

    /// Convenience constructor for tests + known-valid call sites.
    /// Panics on degenerate input.
    #[must_use]
    pub fn new(burst: f64, rate: f64) -> Self {
        Self::try_new(burst, rate).expect("ArrivalCurve burst/rate must be finite and non-negative")
    }

    /// Burst tolerance `b` — max work that can arrive instantaneously.
    #[must_use]
    pub const fn burst(&self) -> f64 {
        self.burst
    }

    /// Long-term rate `r` (work per unit time).
    #[must_use]
    pub const fn rate(&self) -> f64 {
        self.rate
    }

    /// Evaluate `α(t)` at a given time. `t < 0` returns 0
    /// (causal — no arrivals before time zero).
    #[must_use]
    pub fn evaluate(&self, t: f64) -> f64 {
        if t < 0.0 {
            0.0
        } else {
            self.burst + self.rate * t
        }
    }
}

// ============================================================================
// Service curve (rate-latency model)
// ============================================================================

/// Rate-latency service curve: `β(t) = R·(t − T)+` where
/// `(x)+ = max(x, 0)`. The stage delivers no service for the first
/// `T` time units (latency), then serves work at rate `R`.
///
/// Invariants: `rate > 0` and `latency >= 0`. A zero rate means the
/// stage is permanently stalled and would yield infinite delay
/// bounds; the substrate refuses construction.
///
/// Per ft-cjc4l fix: fields are private. Accessors are `rate()`
/// and `latency()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServiceCurve {
    rate: f64,
    latency: f64,
}

impl ServiceCurve {
    #[must_use]
    pub fn try_new(rate: f64, latency: f64) -> Option<Self> {
        if !rate.is_finite() || !latency.is_finite() || rate <= 0.0 || latency < 0.0 {
            return None;
        }
        Some(Self { rate, latency })
    }

    #[must_use]
    pub fn new(rate: f64, latency: f64) -> Self {
        Self::try_new(rate, latency)
            .expect("ServiceCurve rate must be > 0 and latency must be >= 0 (both finite)")
    }

    /// Service rate `R` (work per unit time).
    #[must_use]
    pub const fn rate(&self) -> f64 {
        self.rate
    }

    /// Service latency `T` — time before the first work unit is served.
    #[must_use]
    pub const fn latency(&self) -> f64 {
        self.latency
    }

    /// Evaluate `β(t)` at a given time.
    #[must_use]
    pub fn evaluate(&self, t: f64) -> f64 {
        let shifted = t - self.latency;
        if shifted < 0.0 {
            0.0
        } else {
            self.rate * shifted
        }
    }
}

// ============================================================================
// Lindley bounds: delay + backlog
// ============================================================================

/// Worst-case delay bound `h(α, β)` — the horizontal deviation
/// between arrival and service curves. For token-bucket arrivals
/// + rate-latency service:
///
/// ```text
/// h(α, β) = T + b/R
/// ```
///
/// Returns `None` when stable arrival rate exceeds service rate
/// (`r >= R`) — the system is unstable and the bound diverges.
#[must_use]
pub fn delay_bound(arrival: ArrivalCurve, service: ServiceCurve) -> Option<f64> {
    if arrival.rate() >= service.rate() {
        return None;
    }
    Some(service.latency() + arrival.burst() / service.rate())
}

/// Worst-case backlog (queue depth) bound — the vertical deviation
/// between arrival and service curves at `t = T`:
///
/// ```text
/// v(α, β) = b + r·T
/// ```
///
/// Always finite (no stability check needed — backlog at `t=T` is
/// bounded even when rate exceeds capacity over the long run).
#[must_use]
pub fn backlog_bound(arrival: ArrivalCurve, service: ServiceCurve) -> f64 {
    arrival.burst() + arrival.rate() * service.latency()
}

/// Whether the system is stable: long-term arrival rate below
/// service rate.
#[must_use]
pub fn is_stable(arrival: ArrivalCurve, service: ServiceCurve) -> bool {
    arrival.rate() < service.rate()
}

// ============================================================================
// Stage composition (Pay Bursts Only Once)
// ============================================================================

/// Compose two service curves in series. Per the Pay Bursts Only
/// Once theorem in min-plus algebra, the convolution of two rate-
/// latency curves `(R1, T1)` and `(R2, T2)` is the rate-latency
/// curve `(min(R1, R2), T1 + T2)`.
///
/// Per ft-cjc4l fix: routes through ServiceCurve::new so the
/// validator catches latency-overflow into +Inf (e.g.,
/// f64::MAX + f64::MAX) rather than silently producing an
/// unrepresentable curve.
#[must_use]
pub fn compose_serial(a: ServiceCurve, b: ServiceCurve) -> ServiceCurve {
    ServiceCurve::new(a.rate().min(b.rate()), a.latency() + b.latency())
}

/// Compose N service curves in series. Returns `None` if the slice
/// is empty (no pipeline → no service).
#[must_use]
pub fn compose_pipeline(stages: &[ServiceCurve]) -> Option<ServiceCurve> {
    let mut iter = stages.iter().copied();
    let first = iter.next()?;
    Some(iter.fold(first, compose_serial))
}

// ============================================================================
// Stage model + pipeline
// ============================================================================

/// One named stage in the pipeline. The bead's headline pipeline:
/// capture → delta-extract → storage write. Each stage has its own
/// service curve.
#[derive(Debug, Clone, PartialEq)]
pub struct StageModel {
    pub name: String,
    pub service: ServiceCurve,
}

impl StageModel {
    #[must_use]
    pub fn new(name: impl Into<String>, service: ServiceCurve) -> Self {
        Self {
            name: name.into(),
            service,
        }
    }
}

/// Full pipeline delay bound: convolve all stages' service curves
/// and apply `delay_bound` against the input arrival curve.
/// Returns `None` if the pipeline is empty or unstable.
#[must_use]
pub fn pipeline_delay_bound(arrival: ArrivalCurve, stages: &[StageModel]) -> Option<f64> {
    let services: Vec<ServiceCurve> = stages.iter().map(|s| s.service).collect();
    let composed = compose_pipeline(&services)?;
    delay_bound(arrival, composed)
}

// ============================================================================
// Empirical-vs-analytical tolerance check
// ============================================================================

/// Bead-specified tolerance: "if analytical and empirical disagree
/// by >20%, that's a bug worth root-causing".
pub const TOLERANCE_PCT: f64 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmpiricalComparison {
    pub analytical_bound_ms: f64,
    pub empirical_p99_ms: f64,
}

impl EmpiricalComparison {
    /// Percent deviation of empirical from analytical:
    /// `|empirical - analytical| / analytical · 100`.
    /// Returns `None` if analytical is zero or non-finite.
    #[must_use]
    pub fn deviation_pct(&self) -> Option<f64> {
        if !self.analytical_bound_ms.is_finite()
            || !self.empirical_p99_ms.is_finite()
            || self.analytical_bound_ms == 0.0
        {
            return None;
        }
        Some(
            ((self.empirical_p99_ms - self.analytical_bound_ms).abs() / self.analytical_bound_ms)
                * 100.0,
        )
    }

    /// Whether empirical p99 is within the substrate's
    /// `TOLERANCE_PCT` (20%) of the analytical bound.
    /// Returns `false` for non-finite / zero analytical (defensive
    /// — the bound is meaningless and the integration should log).
    #[must_use]
    pub fn within_tolerance(&self) -> bool {
        self.deviation_pct().is_some_and(|d| d <= TOLERANCE_PCT)
    }

    /// Whether the empirical reading EXCEEDS the analytical bound
    /// (a stronger signal than just "deviates" — empirical >
    /// analytical means the bound is broken, not just imprecise).
    #[must_use]
    pub fn exceeds_bound(&self) -> bool {
        self.empirical_p99_ms > self.analytical_bound_ms
    }
}

// ============================================================================
// Attestation artifact
// ============================================================================

/// Pure-data record matching the bead's
/// `docs/attestations/perf/lindley-bounds.json` schema. The
/// integration's release script serialises this; the substrate
/// constructs + queries it.
#[derive(Debug, Clone, PartialEq)]
pub struct LindleyBoundsArtifact {
    pub release_version: String,
    pub arrival: ArrivalCurve,
    pub stages: Vec<StageModel>,
    pub analytical_bound_ms: f64,
    pub empirical_p99_ms: f64,
}

impl LindleyBoundsArtifact {
    /// Convenience: build the comparison record for tolerance check.
    #[must_use]
    pub fn comparison(&self) -> EmpiricalComparison {
        EmpiricalComparison {
            analytical_bound_ms: self.analytical_bound_ms,
            empirical_p99_ms: self.empirical_p99_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // ----------------------------------------------------------------
    // ArrivalCurve / ServiceCurve construction
    // ----------------------------------------------------------------

    #[test]
    fn arrival_try_new_rejects_negative() {
        assert!(ArrivalCurve::try_new(-1.0, 100.0).is_none());
        assert!(ArrivalCurve::try_new(10.0, -5.0).is_none());
        assert!(ArrivalCurve::try_new(10.0, 100.0).is_some());
    }

    #[test]
    fn arrival_try_new_rejects_non_finite() {
        assert!(ArrivalCurve::try_new(f64::NAN, 100.0).is_none());
        assert!(ArrivalCurve::try_new(10.0, f64::INFINITY).is_none());
    }

    /// ft-cjc4l regression: ArrivalCurve / ServiceCurve fields
    /// must not be reachable from outside the module. Validating
    /// constructors are the only construction path. The test
    /// itself uses the accessors to confirm they expose the same
    /// values that try_new accepted.
    #[test]
    fn arrival_and_service_accessors_are_the_only_field_path() {
        let a = ArrivalCurve::new(7.0, 11.0);
        assert!(approx(a.burst(), 7.0));
        assert!(approx(a.rate(), 11.0));

        let s = ServiceCurve::new(13.0, 17.0);
        assert!(approx(s.rate(), 13.0));
        assert!(approx(s.latency(), 17.0));

        // Direct field assignment cannot be exercised here
        // (private fields), which is the point of the fix.
        // ArrivalCurve { burst: f64::NAN, rate: -1.0 } is no
        // longer a compile-able expression in callers.
    }

    #[test]
    fn arrival_evaluate_is_zero_before_t0() {
        let a = ArrivalCurve::new(10.0, 100.0);
        assert_eq!(a.evaluate(-1.0), 0.0);
        assert_eq!(a.evaluate(-100.0), 0.0);
    }

    #[test]
    fn arrival_evaluate_at_t0_is_burst() {
        let a = ArrivalCurve::new(10.0, 100.0);
        assert!(approx(a.evaluate(0.0), 10.0));
    }

    #[test]
    fn arrival_evaluate_grows_at_rate() {
        let a = ArrivalCurve::new(10.0, 100.0);
        assert!(approx(a.evaluate(1.0), 110.0));
        assert!(approx(a.evaluate(2.0), 210.0));
    }

    #[test]
    fn service_try_new_rejects_zero_rate() {
        assert!(ServiceCurve::try_new(0.0, 5.0).is_none());
    }

    #[test]
    fn service_try_new_rejects_negative_latency() {
        assert!(ServiceCurve::try_new(100.0, -1.0).is_none());
    }

    #[test]
    fn service_evaluate_is_zero_before_latency() {
        let s = ServiceCurve::new(100.0, 5.0);
        assert_eq!(s.evaluate(0.0), 0.0);
        assert_eq!(s.evaluate(4.99), 0.0);
        assert_eq!(s.evaluate(5.0), 0.0);
    }

    #[test]
    fn service_evaluate_grows_at_rate_after_latency() {
        let s = ServiceCurve::new(100.0, 5.0);
        assert!(approx(s.evaluate(6.0), 100.0));
        assert!(approx(s.evaluate(10.0), 500.0));
    }

    // ----------------------------------------------------------------
    // delay_bound / backlog_bound
    // ----------------------------------------------------------------

    #[test]
    fn delay_bound_known_formula() {
        // h(α, β) = T + b/R
        // burst=10, rate=50, R=100, T=2 → 2 + 10/100 = 2.1
        let a = ArrivalCurve::new(10.0, 50.0);
        let s = ServiceCurve::new(100.0, 2.0);
        let d = delay_bound(a, s).unwrap();
        assert!(approx(d, 2.1));
    }

    #[test]
    fn delay_bound_unstable_returns_none() {
        // r >= R → unstable.
        let a = ArrivalCurve::new(10.0, 100.0);
        let s = ServiceCurve::new(100.0, 2.0);
        assert!(delay_bound(a, s).is_none());

        let a = ArrivalCurve::new(10.0, 200.0);
        let s = ServiceCurve::new(100.0, 2.0);
        assert!(delay_bound(a, s).is_none());
    }

    #[test]
    fn delay_bound_zero_burst_yields_pure_latency() {
        let a = ArrivalCurve::new(0.0, 50.0);
        let s = ServiceCurve::new(100.0, 5.0);
        let d = delay_bound(a, s).unwrap();
        assert!(approx(d, 5.0));
    }

    #[test]
    fn delay_bound_zero_latency_yields_burst_over_rate() {
        let a = ArrivalCurve::new(20.0, 50.0);
        let s = ServiceCurve::new(100.0, 0.0);
        let d = delay_bound(a, s).unwrap();
        assert!(approx(d, 0.2));
    }

    #[test]
    fn backlog_bound_known_formula() {
        // v = b + r·T
        // burst=10, rate=50, T=2 → 10 + 100 = 110
        let a = ArrivalCurve::new(10.0, 50.0);
        let s = ServiceCurve::new(100.0, 2.0);
        let b = backlog_bound(a, s);
        assert!(approx(b, 110.0));
    }

    #[test]
    fn backlog_bound_no_stability_dependency() {
        // Even when unstable, backlog at t=T is finite.
        let a = ArrivalCurve::new(10.0, 1000.0);
        let s = ServiceCurve::new(100.0, 2.0);
        let b = backlog_bound(a, s);
        assert!(b.is_finite());
    }

    #[test]
    fn is_stable_works() {
        assert!(is_stable(
            ArrivalCurve::new(10.0, 50.0),
            ServiceCurve::new(100.0, 2.0)
        ));
        assert!(!is_stable(
            ArrivalCurve::new(10.0, 100.0),
            ServiceCurve::new(100.0, 2.0)
        ));
    }

    // ----------------------------------------------------------------
    // compose_serial / compose_pipeline (Pay Bursts Only Once)
    // ----------------------------------------------------------------

    #[test]
    fn compose_serial_takes_min_rate_and_summed_latency() {
        let a = ServiceCurve::new(100.0, 2.0);
        let b = ServiceCurve::new(50.0, 3.0);
        let c = compose_serial(a, b);
        assert!(approx(c.rate(), 50.0));
        assert!(approx(c.latency(), 5.0));
    }

    #[test]
    fn compose_serial_commutative() {
        let a = ServiceCurve::new(100.0, 2.0);
        let b = ServiceCurve::new(50.0, 3.0);
        assert_eq!(compose_serial(a, b), compose_serial(b, a));
    }

    #[test]
    fn compose_pipeline_empty_returns_none() {
        assert!(compose_pipeline(&[]).is_none());
    }

    #[test]
    fn compose_pipeline_three_stages_chains_correctly() {
        let stages = vec![
            ServiceCurve::new(150.0, 1.0),
            ServiceCurve::new(100.0, 2.0),
            ServiceCurve::new(80.0, 3.0),
        ];
        let composed = compose_pipeline(&stages).unwrap();
        assert!(approx(composed.rate(), 80.0));
        assert!(approx(composed.latency(), 6.0));
    }

    #[test]
    fn compose_pipeline_single_stage_passes_through() {
        let stages = vec![ServiceCurve::new(100.0, 5.0)];
        let composed = compose_pipeline(&stages).unwrap();
        assert!(approx(composed.rate(), 100.0));
        assert!(approx(composed.latency(), 5.0));
    }

    // ----------------------------------------------------------------
    // pipeline_delay_bound
    // ----------------------------------------------------------------

    #[test]
    fn pipeline_delay_bound_three_stages() {
        let arrival = ArrivalCurve::new(10.0, 50.0);
        let stages = vec![
            StageModel::new("capture", ServiceCurve::new(150.0, 1.0)),
            StageModel::new("delta_extract", ServiceCurve::new(100.0, 2.0)),
            StageModel::new("storage_write", ServiceCurve::new(80.0, 3.0)),
        ];
        // Composed: rate=80, latency=6 → bound = 6 + 10/80 = 6.125
        let bound = pipeline_delay_bound(arrival, &stages).unwrap();
        assert!(approx(bound, 6.125));
    }

    #[test]
    fn pipeline_delay_bound_unstable_at_bottleneck_returns_none() {
        // Bottleneck stage rate (80) is below arrival rate (100).
        let arrival = ArrivalCurve::new(10.0, 100.0);
        let stages = vec![
            StageModel::new("a", ServiceCurve::new(150.0, 1.0)),
            StageModel::new("b", ServiceCurve::new(80.0, 2.0)),
        ];
        assert!(pipeline_delay_bound(arrival, &stages).is_none());
    }

    #[test]
    fn pipeline_delay_bound_empty_pipeline_returns_none() {
        let arrival = ArrivalCurve::new(10.0, 50.0);
        assert!(pipeline_delay_bound(arrival, &[]).is_none());
    }

    // ----------------------------------------------------------------
    // EmpiricalComparison
    // ----------------------------------------------------------------

    #[test]
    fn comparison_within_tolerance_at_5_pct() {
        let c = EmpiricalComparison {
            analytical_bound_ms: 50.0,
            empirical_p99_ms: 52.5,
        };
        assert!(approx(c.deviation_pct().unwrap(), 5.0));
        assert!(c.within_tolerance());
    }

    #[test]
    fn comparison_within_tolerance_at_exactly_20_pct() {
        let c = EmpiricalComparison {
            analytical_bound_ms: 50.0,
            empirical_p99_ms: 60.0,
        };
        assert!(approx(c.deviation_pct().unwrap(), 20.0));
        assert!(c.within_tolerance());
    }

    #[test]
    fn comparison_outside_tolerance_at_25_pct() {
        let c = EmpiricalComparison {
            analytical_bound_ms: 50.0,
            empirical_p99_ms: 62.5,
        };
        assert!(approx(c.deviation_pct().unwrap(), 25.0));
        assert!(!c.within_tolerance());
    }

    #[test]
    fn comparison_lower_empirical_still_within_tolerance() {
        // Empirical < analytical is also a deviation; symmetric on
        // |delta|.
        let c = EmpiricalComparison {
            analytical_bound_ms: 50.0,
            empirical_p99_ms: 40.0,
        };
        assert!(approx(c.deviation_pct().unwrap(), 20.0));
        assert!(c.within_tolerance());
    }

    #[test]
    fn comparison_zero_analytical_returns_none_dev() {
        let c = EmpiricalComparison {
            analytical_bound_ms: 0.0,
            empirical_p99_ms: 50.0,
        };
        assert!(c.deviation_pct().is_none());
        assert!(!c.within_tolerance());
    }

    #[test]
    fn comparison_non_finite_handled_defensively() {
        let c = EmpiricalComparison {
            analytical_bound_ms: f64::NAN,
            empirical_p99_ms: 50.0,
        };
        assert!(c.deviation_pct().is_none());
        assert!(!c.within_tolerance());
    }

    #[test]
    fn comparison_exceeds_bound_when_empirical_higher() {
        let c = EmpiricalComparison {
            analytical_bound_ms: 50.0,
            empirical_p99_ms: 55.0,
        };
        assert!(c.exceeds_bound());

        let c2 = EmpiricalComparison {
            analytical_bound_ms: 50.0,
            empirical_p99_ms: 45.0,
        };
        assert!(!c2.exceeds_bound());
    }

    // ----------------------------------------------------------------
    // LindleyBoundsArtifact
    // ----------------------------------------------------------------

    #[test]
    fn artifact_comparison_round_trips() {
        let a = LindleyBoundsArtifact {
            release_version: "v0.2.0".to_string(),
            arrival: ArrivalCurve::new(10.0, 50.0),
            stages: vec![StageModel::new("capture", ServiceCurve::new(100.0, 5.0))],
            analytical_bound_ms: 5.1,
            empirical_p99_ms: 4.8,
        };
        let cmp = a.comparison();
        // Empirical 4.8ms is ~6% under the 5.1ms bound — within
        // tolerance and below bound (the happy path).
        assert!(cmp.within_tolerance());
        assert!(!cmp.exceeds_bound());
    }

    // ----------------------------------------------------------------
    // Cross-cut: bead's <50ms headline claim
    // ----------------------------------------------------------------

    #[test]
    fn scenario_50ms_headline_claim_capture_extract_write_pipeline() {
        // Realistic-ish numbers for the bead's headline pipeline:
        // capture stage at 200 events/ms with 1ms latency,
        // delta-extract at 150 events/ms with 2ms latency,
        // storage write at 100 events/ms with 5ms latency.
        // Arrival: burst 50 events, rate 80 events/ms.
        let arrival = ArrivalCurve::new(50.0, 80.0);
        let stages = vec![
            StageModel::new("capture", ServiceCurve::new(200.0, 1.0)),
            StageModel::new("delta_extract", ServiceCurve::new(150.0, 2.0)),
            StageModel::new("storage_write", ServiceCurve::new(100.0, 5.0)),
        ];
        // Composed: rate=100, latency=8 → bound = 8 + 50/100 = 8.5ms
        let bound = pipeline_delay_bound(arrival, &stages).unwrap();
        assert!(approx(bound, 8.5));
        // Comfortably under the 50ms headline.
        assert!(bound < 50.0, "{bound} should be < 50ms headline claim");
    }

    #[test]
    fn scenario_attestation_artifact_passes_release_check() {
        // Per-release attestation: build the artifact with a
        // generous bound + tight empirical, expect within_tolerance
        // to pass.
        let arrival = ArrivalCurve::new(50.0, 80.0);
        let stages = vec![
            StageModel::new("capture", ServiceCurve::new(200.0, 1.0)),
            StageModel::new("delta_extract", ServiceCurve::new(150.0, 2.0)),
            StageModel::new("storage_write", ServiceCurve::new(100.0, 5.0)),
        ];
        let analytical = pipeline_delay_bound(arrival, &stages).unwrap();
        let empirical = analytical * 0.95; // 5% lower than bound — passing.
        let artifact = LindleyBoundsArtifact {
            release_version: "v0.2.0".to_string(),
            arrival,
            stages,
            analytical_bound_ms: analytical,
            empirical_p99_ms: empirical,
        };
        assert!(artifact.comparison().within_tolerance());
        assert!(!artifact.comparison().exceeds_bound());
    }

    #[test]
    fn scenario_release_check_fails_when_empirical_far_above_bound() {
        // Empirical 30% above analytical → tolerance fails →
        // integration files a regression bead.
        let arrival = ArrivalCurve::new(50.0, 80.0);
        let stages = vec![StageModel::new("capture", ServiceCurve::new(100.0, 5.0))];
        let analytical = pipeline_delay_bound(arrival, &stages).unwrap();
        let empirical = analytical * 1.30;
        let artifact = LindleyBoundsArtifact {
            release_version: "v0.2.0".to_string(),
            arrival,
            stages,
            analytical_bound_ms: analytical,
            empirical_p99_ms: empirical,
        };
        assert!(!artifact.comparison().within_tolerance());
        assert!(artifact.comparison().exceeds_bound());
    }
}
