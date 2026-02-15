//! Network calculus for formal worst-case latency guarantees.
//!
//! Implements min-plus algebra, arrival curves, service curves, and delay/backlog
//! bound computations. Every function is pure — no I/O, no async, no allocations
//! beyond what the data structures need.
//!
//! # Background
//!
//! Network calculus provides *formal guarantees* about worst-case delay and buffer
//! requirements. Unlike empirical benchmarks (which tell you what happened), network
//! calculus tells you what *can* happen under worst-case conditions.
//!
//! Given:
//! - An arrival curve α(t) bounding the maximum input in any interval of length t
//! - A service curve β(t) guaranteeing the minimum service in any busy period of length t
//!
//! We can derive:
//! - **Delay bound**: D ≤ h(α, β) — the maximum time any bit waits in the system
//! - **Backlog bound**: B ≤ v(α, β) — the maximum buffer occupancy
//!
//! # Min-Plus Algebra
//!
//! The foundation is the (min, +) semiring where:
//! - Addition is replaced by min
//! - Multiplication is replaced by +
//! - Convolution: (f ⊗ g)(t) = inf_{0≤s≤t} { f(s) + g(t-s) }
//! - Deconvolution: (f ⊘ g)(t) = sup_{s≥0} { f(t+s) - g(s) }

use serde::{Deserialize, Serialize};

// ── Piecewise-Linear Curve ──────────────────────────────────────────

/// A point on a piecewise-linear curve.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub t: f64,
    pub y: f64,
}

/// A piecewise-linear curve defined by a sorted sequence of (t, y) breakpoints.
/// Between breakpoints the curve is linearly interpolated.
/// Before the first breakpoint, the value is the first breakpoint's y.
/// After the last breakpoint, the curve extends with the slope of the last segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiecewiseLinear {
    /// Sorted by t, at least one point required.
    points: Vec<CurvePoint>,
}

impl PiecewiseLinear {
    /// Create a new piecewise-linear curve from points.
    /// Points are sorted by t; duplicates are removed (last wins).
    pub fn new(mut points: Vec<CurvePoint>) -> Self {
        assert!(!points.is_empty(), "curve must have at least one point");
        points.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        // Deduplicate: keep last point at each t.
        let mut deduped: Vec<CurvePoint> = Vec::with_capacity(points.len());
        for point in points {
            if let Some(last) = deduped.last_mut() {
                if (last.t - point.t).abs() < 1e-12 {
                    *last = point;
                    continue;
                }
            }
            deduped.push(point);
        }
        Self { points: deduped }
    }

    /// Create a constant curve at value `c` for all t ≥ 0.
    pub fn constant(c: f64) -> Self {
        Self {
            points: vec![CurvePoint { t: 0.0, y: c }],
        }
    }

    /// Create a linear curve: y = slope * t + intercept, starting at t=0.
    pub fn linear(intercept: f64, slope: f64) -> Self {
        Self {
            points: vec![
                CurvePoint {
                    t: 0.0,
                    y: intercept,
                },
                CurvePoint {
                    t: 1.0,
                    y: intercept + slope,
                },
            ],
        }
    }

    /// Evaluate the curve at time t.
    pub fn eval(&self, t: f64) -> f64 {
        if self.points.is_empty() {
            return 0.0;
        }
        if self.points.len() == 1 {
            return self.points[0].y;
        }

        // Before first point
        if t <= self.points[0].t {
            return self.points[0].y;
        }

        // Find the segment containing t
        for i in 1..self.points.len() {
            if t <= self.points[i].t {
                let p0 = &self.points[i - 1];
                let p1 = &self.points[i];
                let dt = p1.t - p0.t;
                if dt.abs() < 1e-15 {
                    return p1.y;
                }
                let frac = (t - p0.t) / dt;
                return p0.y + frac * (p1.y - p0.y);
            }
        }

        // After last point: extend with last segment's slope
        let n = self.points.len();
        if n >= 2 {
            let p0 = &self.points[n - 2];
            let p1 = &self.points[n - 1];
            let dt = p1.t - p0.t;
            if dt.abs() < 1e-15 {
                return p1.y;
            }
            let slope = (p1.y - p0.y) / dt;
            p1.y + slope * (t - p1.t)
        } else {
            self.points[n - 1].y
        }
    }

    /// The trailing slope of the curve (slope of the last segment).
    pub fn trailing_slope(&self) -> f64 {
        let n = self.points.len();
        if n == 0 {
            return 0.0;
        }
        if n < 2 {
            return 0.0;
        }
        let p0 = &self.points[n - 2];
        let p1 = &self.points[n - 1];
        let dt = p1.t - p0.t;
        if dt.abs() < 1e-15 {
            return 0.0;
        }
        (p1.y - p0.y) / dt
    }

    /// Number of breakpoints.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether this curve has no breakpoints.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Access the breakpoints.
    pub fn points(&self) -> &[CurvePoint] {
        &self.points
    }
}

// ── Arrival Curves ──────────────────────────────────────────────────

/// An arrival curve α(t) bounding the maximum amount of data entering the
/// system in any interval of length t.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ArrivalCurve {
    /// Leaky bucket: α(t) = σ + ρ·t
    /// σ = burst tolerance, ρ = sustained rate
    LeakyBucket { sigma: f64, rho: f64 },

    /// Token bucket (dual leaky bucket): α(t) = min(σ + ρ·t, peak·t)
    /// Adds a peak-rate constraint on top of the leaky bucket.
    TokenBucket {
        sigma: f64,
        rho: f64,
        peak_rate: f64,
    },

    /// Staircase function: α(t) = ceil(t / period) × burst
    /// Models periodic bursty arrivals.
    Staircase { period: f64, burst: f64 },

    /// Arbitrary piecewise-linear arrival curve.
    Piecewise(PiecewiseLinear),
}

impl ArrivalCurve {
    /// Create a leaky bucket arrival curve.
    pub fn leaky_bucket(sigma: f64, rho: f64) -> Self {
        ArrivalCurve::LeakyBucket { sigma, rho }
    }

    /// Create a token bucket arrival curve.
    pub fn token_bucket(sigma: f64, rho: f64, peak_rate: f64) -> Self {
        ArrivalCurve::TokenBucket {
            sigma,
            rho,
            peak_rate,
        }
    }

    /// Create a staircase arrival curve.
    pub fn staircase(period: f64, burst: f64) -> Self {
        ArrivalCurve::Staircase { period, burst }
    }

    /// Evaluate α(t).
    pub fn eval(&self, t: f64) -> f64 {
        if t <= 0.0 {
            return 0.0;
        }
        match self {
            ArrivalCurve::LeakyBucket { sigma, rho } => sigma + rho * t,
            ArrivalCurve::TokenBucket {
                sigma,
                rho,
                peak_rate,
            } => {
                let lb = sigma + rho * t;
                let pk = peak_rate * t;
                lb.min(pk)
            }
            ArrivalCurve::Staircase { period, burst } => {
                if *period <= 0.0 {
                    return 0.0;
                }
                (t / period).ceil() * burst
            }
            ArrivalCurve::Piecewise(pw) => pw.eval(t),
        }
    }

    /// The sustained (long-term) rate.
    pub fn sustained_rate(&self) -> f64 {
        match self {
            ArrivalCurve::LeakyBucket { rho, .. } => *rho,
            ArrivalCurve::TokenBucket { rho, .. } => *rho,
            ArrivalCurve::Staircase { period, burst } => {
                if *period <= 0.0 {
                    0.0
                } else {
                    burst / period
                }
            }
            ArrivalCurve::Piecewise(pw) => pw.trailing_slope(),
        }
    }

    /// Burst tolerance (maximum instantaneous excess over sustained rate).
    pub fn burst(&self) -> f64 {
        match self {
            ArrivalCurve::LeakyBucket { sigma, .. } => *sigma,
            ArrivalCurve::TokenBucket { sigma, .. } => *sigma,
            ArrivalCurve::Staircase { burst, .. } => *burst,
            ArrivalCurve::Piecewise(pw) => pw.eval(0.0),
        }
    }
}

// ── Service Curves ──────────────────────────────────────────────────

/// A service curve β(t) guaranteeing the minimum amount of service
/// provided in any busy period of length t.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServiceCurve {
    /// Rate-latency server: β(t) = R·[t - T]⁺
    /// R = service rate, T = processing latency
    RateLatency { rate: f64, latency: f64 },

    /// Strict constant-rate server: β(t) = R·t (zero latency)
    StrictRate { rate: f64 },

    /// Arbitrary piecewise-linear service curve.
    Piecewise(PiecewiseLinear),
}

impl ServiceCurve {
    /// Create a rate-latency service curve.
    pub fn rate_latency(rate: f64, latency: f64) -> Self {
        ServiceCurve::RateLatency { rate, latency }
    }

    /// Create a strict constant-rate service curve.
    pub fn strict_rate(rate: f64) -> Self {
        ServiceCurve::StrictRate { rate }
    }

    /// Evaluate β(t).
    pub fn eval(&self, t: f64) -> f64 {
        if t <= 0.0 {
            return 0.0;
        }
        match self {
            ServiceCurve::RateLatency { rate, latency } => {
                let effective = t - latency;
                if effective <= 0.0 {
                    0.0
                } else {
                    rate * effective
                }
            }
            ServiceCurve::StrictRate { rate } => rate * t,
            ServiceCurve::Piecewise(pw) => pw.eval(t).max(0.0),
        }
    }

    /// The service rate.
    pub fn rate(&self) -> f64 {
        match self {
            ServiceCurve::RateLatency { rate, .. } => *rate,
            ServiceCurve::StrictRate { rate } => *rate,
            ServiceCurve::Piecewise(pw) => pw.trailing_slope(),
        }
    }

    /// The processing latency (delay before service begins).
    pub fn latency(&self) -> f64 {
        match self {
            ServiceCurve::RateLatency { latency, .. } => *latency,
            ServiceCurve::StrictRate { .. } => 0.0,
            ServiceCurve::Piecewise(pw) => {
                // First t where curve > 0
                for p in pw.points() {
                    if p.y > 0.0 {
                        return p.t;
                    }
                }
                0.0
            }
        }
    }
}

// ── Min-Plus Algebra ────────────────────────────────────────────────

/// Resolution for numerical convolution/deconvolution sampling.
const SAMPLE_RESOLUTION: usize = 1000;

/// Default time horizon for sampling.
const DEFAULT_HORIZON: f64 = 100.0;

/// Min-plus convolution: (f ⊗ g)(t) = inf_{0≤s≤t} { f(s) + g(t-s) }
///
/// For rate-latency service curves, this has a closed-form solution.
/// For general curves, we use numerical sampling.
pub fn min_plus_convolution(a: &ServiceCurve, b: &ServiceCurve) -> ServiceCurve {
    // Closed-form for rate-latency ⊗ rate-latency
    match (a, b) {
        (
            ServiceCurve::RateLatency {
                rate: r1,
                latency: t1,
            },
            ServiceCurve::RateLatency {
                rate: r2,
                latency: t2,
            },
        ) => {
            // β₁ ⊗ β₂ = rate-latency(min(R₁,R₂), T₁+T₂)
            ServiceCurve::RateLatency {
                rate: r1.min(*r2),
                latency: t1 + t2,
            }
        }
        (ServiceCurve::StrictRate { rate: r1 }, ServiceCurve::StrictRate { rate: r2 }) => {
            ServiceCurve::StrictRate { rate: r1.min(*r2) }
        }
        (
            ServiceCurve::StrictRate { rate },
            ServiceCurve::RateLatency {
                rate: r2,
                latency: t2,
            },
        ) => ServiceCurve::RateLatency {
            rate: rate.min(*r2),
            latency: *t2,
        },
        (
            ServiceCurve::RateLatency {
                rate: r1,
                latency: t1,
            },
            ServiceCurve::StrictRate { rate },
        ) => ServiceCurve::RateLatency {
            rate: r1.min(*rate),
            latency: *t1,
        },
        _ => {
            // Numerical convolution for general piecewise curves
            let horizon = DEFAULT_HORIZON;
            let step = horizon / SAMPLE_RESOLUTION as f64;
            let mut points = Vec::with_capacity(SAMPLE_RESOLUTION + 1);
            for i in 0..=SAMPLE_RESOLUTION {
                let t = i as f64 * step;
                let mut min_val = f64::INFINITY;
                for j in 0..=i {
                    let s = j as f64 * step;
                    let val = a.eval(s) + b.eval(t - s);
                    if val < min_val {
                        min_val = val;
                    }
                }
                points.push(CurvePoint {
                    t,
                    y: min_val.max(0.0),
                });
            }
            ServiceCurve::Piecewise(PiecewiseLinear::new(points))
        }
    }
}

/// Min-plus deconvolution: (f ⊘ g)(t) = sup_{s≥0} { f(t+s) - g(s) }
///
/// Used for computing output arrival curves and leftover service curves.
pub fn min_plus_deconvolution_sampled(
    f: &dyn Fn(f64) -> f64,
    g: &dyn Fn(f64) -> f64,
    horizon: f64,
) -> PiecewiseLinear {
    let step = horizon / SAMPLE_RESOLUTION as f64;
    let mut points = Vec::with_capacity(SAMPLE_RESOLUTION + 1);
    for i in 0..=SAMPLE_RESOLUTION {
        let t = i as f64 * step;
        let mut max_val = f64::NEG_INFINITY;
        for j in 0..=SAMPLE_RESOLUTION {
            let s = j as f64 * step;
            let val = f(t + s) - g(s);
            if val > max_val {
                max_val = val;
            }
        }
        points.push(CurvePoint { t, y: max_val });
    }
    PiecewiseLinear::new(points)
}

// ── Delay and Backlog Bounds ────────────────────────────────────────

/// Compute the maximum horizontal distance h(α, β) between arrival and
/// service curves. This is the delay bound.
///
/// D ≤ h(α, β) = sup_{t≥0} { inf { d ≥ 0 : α(t) ≤ β(t+d) } }
///
/// For leaky-bucket + rate-latency, this has a closed form:
/// D = σ/R + T
pub fn delay_bound(arrival: &ArrivalCurve, service: &ServiceCurve) -> f64 {
    // Closed-form for leaky-bucket + rate-latency
    match (arrival, service) {
        (ArrivalCurve::LeakyBucket { sigma, rho }, ServiceCurve::RateLatency { rate, latency }) => {
            if *rate <= *rho {
                return f64::INFINITY; // System is unstable
            }
            sigma / (rate - rho) + latency
        }
        (ArrivalCurve::LeakyBucket { sigma, rho }, ServiceCurve::StrictRate { rate }) => {
            if *rate <= *rho {
                return f64::INFINITY;
            }
            sigma / (rate - rho)
        }
        _ => {
            // Numerical computation for general curves
            let horizon = DEFAULT_HORIZON;
            let step = horizon / SAMPLE_RESOLUTION as f64;
            let mut max_delay = 0.0f64;

            for i in 0..=SAMPLE_RESOLUTION {
                let t = i as f64 * step;
                let a_t = arrival.eval(t);

                // Binary search for the smallest d where β(t+d) ≥ α(t)
                let mut lo = 0.0f64;
                let mut hi = horizon;
                for _ in 0..64 {
                    let mid = (lo + hi) / 2.0;
                    if service.eval(t + mid) >= a_t {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                max_delay = max_delay.max(hi);
            }

            max_delay
        }
    }
}

/// Compute the maximum vertical distance v(α, β) between arrival and
/// service curves. This is the backlog (buffer) bound.
///
/// B ≤ v(α, β) = sup_{t≥0} { α(t) - β(t) }
///
/// For leaky-bucket + rate-latency:
/// B = σ + ρ·T
pub fn backlog_bound(arrival: &ArrivalCurve, service: &ServiceCurve) -> f64 {
    // Closed-form for leaky-bucket + rate-latency
    match (arrival, service) {
        (ArrivalCurve::LeakyBucket { sigma, rho }, ServiceCurve::RateLatency { rate, latency }) => {
            if *rate <= *rho {
                return f64::INFINITY;
            }
            sigma + rho * latency
        }
        (ArrivalCurve::LeakyBucket { sigma, .. }, ServiceCurve::StrictRate { .. }) => *sigma,
        _ => {
            let horizon = DEFAULT_HORIZON;
            let step = horizon / SAMPLE_RESOLUTION as f64;
            let mut max_backlog = 0.0f64;

            for i in 0..=SAMPLE_RESOLUTION {
                let t = i as f64 * step;
                let diff = arrival.eval(t) - service.eval(t);
                max_backlog = max_backlog.max(diff);
            }

            max_backlog
        }
    }
}

// ── Pipeline Composition ────────────────────────────────────────────

/// A processing stage in the data pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineStage {
    pub name: String,
    pub service: ServiceCurve,
}

/// A multi-stage processing pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub stages: Vec<PipelineStage>,
}

/// Result of pipeline analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineAnalysis {
    /// End-to-end delay bound.
    pub delay_bound: f64,
    /// End-to-end backlog bound.
    pub backlog_bound: f64,
    /// Per-stage delay bounds.
    pub per_stage_delays: Vec<f64>,
    /// Per-stage backlog bounds.
    pub per_stage_backlogs: Vec<f64>,
    /// The concatenated (total) service curve rate.
    pub total_service_rate: f64,
    /// The concatenated (total) service latency.
    pub total_service_latency: f64,
}

impl Pipeline {
    /// Create a new pipeline.
    pub fn new(stages: Vec<PipelineStage>) -> Self {
        Self { stages }
    }

    /// Compute the concatenated service curve using min-plus convolution.
    ///
    /// β_total = β₁ ⊗ β₂ ⊗ ... ⊗ βₙ
    pub fn total_service_curve(&self) -> ServiceCurve {
        if self.stages.is_empty() {
            return ServiceCurve::strict_rate(f64::INFINITY);
        }
        let mut total = self.stages[0].service.clone();
        for stage in &self.stages[1..] {
            total = min_plus_convolution(&total, &stage.service);
        }
        total
    }

    /// Analyze the pipeline with the given arrival curve.
    ///
    /// Computes end-to-end and per-stage delay/backlog bounds using the
    /// concatenation theorem of network calculus.
    pub fn analyze(&self, arrival: &ArrivalCurve) -> PipelineAnalysis {
        let total = self.total_service_curve();

        let total_delay = delay_bound(arrival, &total);
        let total_backlog = backlog_bound(arrival, &total);

        let mut per_stage_delays = Vec::with_capacity(self.stages.len());
        let mut per_stage_backlogs = Vec::with_capacity(self.stages.len());

        for stage in &self.stages {
            per_stage_delays.push(delay_bound(arrival, &stage.service));
            per_stage_backlogs.push(backlog_bound(arrival, &stage.service));
        }

        PipelineAnalysis {
            delay_bound: total_delay,
            backlog_bound: total_backlog,
            per_stage_delays,
            per_stage_backlogs,
            total_service_rate: total.rate(),
            total_service_latency: total.latency(),
        }
    }
}

// ── Aggregate Multiplexing ──────────────────────────────────────────

/// Aggregate arrival curves from N multiplexed panes using FIFO scheduling.
///
/// α_agg = Σᵢ αᵢ
/// For identical leaky-bucket panes: α_agg = N·σ + N·ρ·t
pub fn aggregate_arrival(curves: &[ArrivalCurve]) -> ArrivalCurve {
    if curves.is_empty() {
        return ArrivalCurve::leaky_bucket(0.0, 0.0);
    }

    // If all are leaky buckets, aggregate analytically
    let all_leaky = curves
        .iter()
        .all(|c| matches!(c, ArrivalCurve::LeakyBucket { .. }));
    if all_leaky {
        let mut total_sigma = 0.0;
        let mut total_rho = 0.0;
        for c in curves {
            if let ArrivalCurve::LeakyBucket { sigma, rho } = c {
                total_sigma += sigma;
                total_rho += rho;
            }
        }
        return ArrivalCurve::leaky_bucket(total_sigma, total_rho);
    }

    // General case: sample and sum
    let horizon = DEFAULT_HORIZON;
    let step = horizon / SAMPLE_RESOLUTION as f64;
    let mut points = Vec::with_capacity(SAMPLE_RESOLUTION + 1);
    for i in 0..=SAMPLE_RESOLUTION {
        let t = i as f64 * step;
        let sum: f64 = curves.iter().map(|c| c.eval(t)).sum();
        points.push(CurvePoint { t, y: sum });
    }
    ArrivalCurve::Piecewise(PiecewiseLinear::new(points))
}

/// Compute delay bound for N multiplexed panes sharing a service curve.
///
/// For N identical leaky-bucket panes with rate-latency server:
/// D_max = (N·σ) / (R - N·ρ) + T
pub fn multiplexed_delay_bound(pane_arrivals: &[ArrivalCurve], service: &ServiceCurve) -> f64 {
    let agg = aggregate_arrival(pane_arrivals);
    delay_bound(&agg, service)
}

/// Compute backlog bound for N multiplexed panes sharing a service curve.
pub fn multiplexed_backlog_bound(pane_arrivals: &[ArrivalCurve], service: &ServiceCurve) -> f64 {
    let agg = aggregate_arrival(pane_arrivals);
    backlog_bound(&agg, service)
}

// ── FrankenTerm-Specific Analysis ───────────────────────────────────

/// Configuration for a pane's output characteristics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneOutputProfile {
    /// Maximum burst size in bytes (σ).
    pub burst_bytes: f64,
    /// Sustained output rate in bytes/second (ρ).
    pub sustained_rate_bps: f64,
}

/// Configuration for the processing pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// PTY capture rate (bytes/second).
    pub capture_rate_bps: f64,
    /// Capture processing latency (seconds).
    pub capture_latency_s: f64,
    /// Pattern processing rate (bytes/second).
    pub process_rate_bps: f64,
    /// Pattern processing latency (seconds).
    pub process_latency_s: f64,
    /// Storage write rate (bytes/second).
    pub storage_rate_bps: f64,
    /// Storage write latency (seconds).
    pub storage_latency_s: f64,
}

/// Analysis results for the FrankenTerm pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrankenTermAnalysis {
    /// Maximum end-to-end delay from PTY output to storage.
    pub max_delay_ms: f64,
    /// Maximum buffer requirement in bytes.
    pub max_backlog_bytes: f64,
    /// Per-stage analysis.
    pub stages: Vec<StageAnalysis>,
    /// Whether the system is stable (service rate > arrival rate).
    pub is_stable: bool,
}

/// Per-stage analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageAnalysis {
    pub name: String,
    pub delay_bound_ms: f64,
    pub backlog_bound_bytes: f64,
}

/// Analyze the FrankenTerm pane pipeline for N panes with given profiles.
///
/// Returns formal worst-case guarantees for delay and buffer requirements.
pub fn analyze_frankenterm_pipeline(
    pane_profiles: &[PaneOutputProfile],
    config: &PipelineConfig,
) -> FrankenTermAnalysis {
    let pane_arrivals: Vec<ArrivalCurve> = pane_profiles
        .iter()
        .map(|p| ArrivalCurve::leaky_bucket(p.burst_bytes, p.sustained_rate_bps))
        .collect();

    let agg = aggregate_arrival(&pane_arrivals);
    let total_rate = agg.sustained_rate();

    let pipeline = Pipeline::new(vec![
        PipelineStage {
            name: "capture".to_string(),
            service: ServiceCurve::rate_latency(config.capture_rate_bps, config.capture_latency_s),
        },
        PipelineStage {
            name: "process".to_string(),
            service: ServiceCurve::rate_latency(config.process_rate_bps, config.process_latency_s),
        },
        PipelineStage {
            name: "storage".to_string(),
            service: ServiceCurve::rate_latency(config.storage_rate_bps, config.storage_latency_s),
        },
    ]);

    let analysis = pipeline.analyze(&agg);
    let min_rate = config
        .capture_rate_bps
        .min(config.process_rate_bps)
        .min(config.storage_rate_bps);

    let stages = pipeline
        .stages
        .iter()
        .zip(
            analysis
                .per_stage_delays
                .iter()
                .zip(analysis.per_stage_backlogs.iter()),
        )
        .map(|(stage, (delay, backlog))| StageAnalysis {
            name: stage.name.clone(),
            delay_bound_ms: delay * 1000.0,
            backlog_bound_bytes: *backlog,
        })
        .collect();

    FrankenTermAnalysis {
        max_delay_ms: analysis.delay_bound * 1000.0,
        max_backlog_bytes: analysis.backlog_bound,
        stages,
        is_stable: min_rate > total_rate,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-9;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < TOL || (a - b).abs() / a.abs().max(b.abs()).max(1.0) < 1e-6
    }

    // ── PiecewiseLinear tests ──

    #[test]
    fn constant_curve_eval() {
        let c = PiecewiseLinear::constant(42.0);
        assert!(approx_eq(c.eval(0.0), 42.0));
        assert!(approx_eq(c.eval(100.0), 42.0));
        assert!(approx_eq(c.eval(-5.0), 42.0));
    }

    #[test]
    fn linear_curve_eval() {
        let c = PiecewiseLinear::linear(10.0, 5.0);
        assert!(approx_eq(c.eval(0.0), 10.0));
        assert!(approx_eq(c.eval(1.0), 15.0));
        assert!(approx_eq(c.eval(2.0), 20.0));
        assert!(approx_eq(c.eval(0.5), 12.5));
    }

    #[test]
    fn piecewise_interpolation() {
        let pw = PiecewiseLinear::new(vec![
            CurvePoint { t: 0.0, y: 0.0 },
            CurvePoint { t: 1.0, y: 10.0 },
            CurvePoint { t: 3.0, y: 20.0 },
        ]);
        assert!(approx_eq(pw.eval(0.0), 0.0));
        assert!(approx_eq(pw.eval(0.5), 5.0));
        assert!(approx_eq(pw.eval(1.0), 10.0));
        assert!(approx_eq(pw.eval(2.0), 15.0));
        assert!(approx_eq(pw.eval(3.0), 20.0));
        // Extrapolation: slope of last segment = (20-10)/(3-1) = 5
        assert!(approx_eq(pw.eval(4.0), 25.0));
    }

    #[test]
    fn trailing_slope() {
        let pw = PiecewiseLinear::new(vec![
            CurvePoint { t: 0.0, y: 0.0 },
            CurvePoint { t: 2.0, y: 6.0 },
        ]);
        assert!(approx_eq(pw.trailing_slope(), 3.0));
    }

    #[test]
    fn dedup_keeps_last_at_same_t() {
        let pw = PiecewiseLinear::new(vec![
            CurvePoint { t: 1.0, y: 10.0 },
            CurvePoint { t: 1.0, y: 20.0 },
        ]);
        assert!(approx_eq(pw.eval(1.0), 20.0));
    }

    #[test]
    fn empty_deserialized_piecewise_is_safe() {
        let pw: PiecewiseLinear =
            serde_json::from_str(r#"{"points":[]}"#).expect("empty points should deserialize");
        assert!(pw.is_empty());
        assert!(approx_eq(pw.eval(0.0), 0.0));
        assert!(approx_eq(pw.trailing_slope(), 0.0));
    }

    // ── Arrival curve tests ──

    #[test]
    fn leaky_bucket_eval() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 10.0);
        assert!(approx_eq(arr.eval(0.0), 0.0));
        assert!(approx_eq(arr.eval(1.0), 110.0));
        assert!(approx_eq(arr.eval(10.0), 200.0));
    }

    #[test]
    fn token_bucket_eval() {
        let arr = ArrivalCurve::token_bucket(100.0, 10.0, 50.0);
        // min(100 + 10*t, 50*t)
        assert!(approx_eq(arr.eval(1.0), 50.0)); // min(110, 50) = 50
        assert!(approx_eq(arr.eval(3.0), 130.0)); // min(130, 150) = 130
    }

    #[test]
    fn staircase_eval() {
        let arr = ArrivalCurve::staircase(2.0, 5.0);
        assert!(approx_eq(arr.eval(0.5), 5.0)); // ceil(0.25) = 1, 1*5
        assert!(approx_eq(arr.eval(2.0), 5.0)); // ceil(1) = 1, 1*5
        assert!(approx_eq(arr.eval(2.1), 10.0)); // ceil(1.05) = 2, 2*5
    }

    #[test]
    fn sustained_rate() {
        assert!(approx_eq(
            ArrivalCurve::leaky_bucket(100.0, 10.0).sustained_rate(),
            10.0
        ));
        assert!(approx_eq(
            ArrivalCurve::staircase(2.0, 5.0).sustained_rate(),
            2.5
        ));
    }

    // ── Service curve tests ──

    #[test]
    fn rate_latency_eval() {
        let svc = ServiceCurve::rate_latency(100.0, 0.005);
        assert!(approx_eq(svc.eval(0.0), 0.0));
        assert!(approx_eq(svc.eval(0.005), 0.0));
        assert!(approx_eq(svc.eval(0.015), 1.0)); // 100 * 0.01 = 1.0
    }

    #[test]
    fn strict_rate_eval() {
        let svc = ServiceCurve::strict_rate(200.0);
        assert!(approx_eq(svc.eval(0.0), 0.0));
        assert!(approx_eq(svc.eval(1.0), 200.0));
    }

    // ── Min-plus convolution tests ──

    #[test]
    fn convolution_rate_latency() {
        let a = ServiceCurve::rate_latency(100.0, 0.01);
        let b = ServiceCurve::rate_latency(200.0, 0.02);
        let c = min_plus_convolution(&a, &b);
        match c {
            ServiceCurve::RateLatency { rate, latency } => {
                assert!(approx_eq(rate, 100.0)); // min(100, 200)
                assert!(approx_eq(latency, 0.03)); // 0.01 + 0.02
            }
            _ => panic!("expected RateLatency"),
        }
    }

    #[test]
    fn convolution_strict_rates() {
        let a = ServiceCurve::strict_rate(100.0);
        let b = ServiceCurve::strict_rate(200.0);
        let c = min_plus_convolution(&a, &b);
        match c {
            ServiceCurve::StrictRate { rate } => {
                assert!(approx_eq(rate, 100.0));
            }
            _ => panic!("expected StrictRate"),
        }
    }

    // ── Delay bound tests ──

    #[test]
    fn delay_bound_leaky_rate_latency() {
        // D = σ/(R-ρ) + T = 1000/(100_000-1000) + 0.005
        let arr = ArrivalCurve::leaky_bucket(1000.0, 1000.0);
        let svc = ServiceCurve::rate_latency(100_000.0, 0.005);
        let d = delay_bound(&arr, &svc);
        let expected = 1000.0 / (100_000.0 - 1000.0) + 0.005;
        assert!(approx_eq(d, expected));
    }

    #[test]
    fn delay_bound_unstable() {
        // ρ > R → infinite delay
        let arr = ArrivalCurve::leaky_bucket(100.0, 200.0);
        let svc = ServiceCurve::rate_latency(100.0, 0.01);
        let d = delay_bound(&arr, &svc);
        assert!(d.is_infinite());
    }

    // ── Backlog bound tests ──

    #[test]
    fn backlog_bound_leaky_rate_latency() {
        // B = σ + ρ·T = 1000 + 1000*0.005 = 1005
        let arr = ArrivalCurve::leaky_bucket(1000.0, 1000.0);
        let svc = ServiceCurve::rate_latency(100_000.0, 0.005);
        let b = backlog_bound(&arr, &svc);
        assert!(approx_eq(b, 1005.0));
    }

    #[test]
    fn backlog_bound_strict_rate() {
        // B = σ (no latency means backlog is just the burst)
        let arr = ArrivalCurve::leaky_bucket(500.0, 10.0);
        let svc = ServiceCurve::strict_rate(1000.0);
        let b = backlog_bound(&arr, &svc);
        assert!(approx_eq(b, 500.0));
    }

    // ── Pipeline tests ──

    #[test]
    fn pipeline_three_stages() {
        let pipeline = Pipeline::new(vec![
            PipelineStage {
                name: "capture".into(),
                service: ServiceCurve::rate_latency(1_000_000.0, 0.001),
            },
            PipelineStage {
                name: "process".into(),
                service: ServiceCurve::rate_latency(500_000.0, 0.002),
            },
            PipelineStage {
                name: "storage".into(),
                service: ServiceCurve::rate_latency(200_000.0, 0.003),
            },
        ]);

        let total = pipeline.total_service_curve();
        // rate = min(1M, 500K, 200K) = 200K
        // latency = 0.001 + 0.002 + 0.003 = 0.006
        assert!(approx_eq(total.rate(), 200_000.0));
        assert!(approx_eq(total.latency(), 0.006));
    }

    #[test]
    fn pipeline_analysis() {
        let pipeline = Pipeline::new(vec![
            PipelineStage {
                name: "capture".into(),
                service: ServiceCurve::rate_latency(100_000.0, 0.001),
            },
            PipelineStage {
                name: "storage".into(),
                service: ServiceCurve::rate_latency(100_000.0, 0.002),
            },
        ]);

        let arr = ArrivalCurve::leaky_bucket(10_000.0, 1_000.0);
        let analysis = pipeline.analyze(&arr);

        // Total: rate=100K, latency=0.003
        // D = 10000/(100000-1000) + 0.003 = 0.10101... + 0.003 ≈ 0.10401
        let expected_delay = 10_000.0 / (100_000.0 - 1_000.0) + 0.003;
        assert!(approx_eq(analysis.delay_bound, expected_delay));
    }

    // ── Aggregate multiplexing tests ──

    #[test]
    fn aggregate_leaky_buckets() {
        let arrivals = vec![
            ArrivalCurve::leaky_bucket(100.0, 10.0),
            ArrivalCurve::leaky_bucket(200.0, 20.0),
            ArrivalCurve::leaky_bucket(150.0, 15.0),
        ];
        let agg = aggregate_arrival(&arrivals);
        match agg {
            ArrivalCurve::LeakyBucket { sigma, rho } => {
                assert!(approx_eq(sigma, 450.0));
                assert!(approx_eq(rho, 45.0));
            }
            _ => panic!("expected LeakyBucket"),
        }
    }

    #[test]
    fn multiplexed_50_panes() {
        // 50 panes, each: σ=10KB burst, ρ=1KB/s
        // Server: R=100MB/s, T=5ms
        let panes: Vec<_> = (0..50)
            .map(|_| ArrivalCurve::leaky_bucket(10_000.0, 1_000.0))
            .collect();
        let svc = ServiceCurve::rate_latency(100_000_000.0, 0.005);

        let delay = multiplexed_delay_bound(&panes, &svc);
        // D = (50*10000)/(100M - 50*1000) + 0.005
        //   = 500000/99950000 + 0.005
        //   ≈ 0.005005 seconds ≈ 5.005 ms
        let expected = 500_000.0 / (100_000_000.0 - 50_000.0) + 0.005;
        assert!(approx_eq(delay, expected));
        assert!(delay < 0.006); // < 6ms total delay
    }

    // ── FrankenTerm analysis tests ──

    #[test]
    fn frankenterm_analysis_50_panes() {
        let profiles: Vec<_> = (0..50)
            .map(|_| PaneOutputProfile {
                burst_bytes: 10_000.0,
                sustained_rate_bps: 1_000.0,
            })
            .collect();

        let config = PipelineConfig {
            capture_rate_bps: 100_000_000.0,
            capture_latency_s: 0.001,
            process_rate_bps: 50_000_000.0,
            process_latency_s: 0.002,
            storage_rate_bps: 10_000_000.0,
            storage_latency_s: 0.003,
        };

        let analysis = analyze_frankenterm_pipeline(&profiles, &config);
        assert!(analysis.is_stable);
        assert!(analysis.max_delay_ms < 100.0); // < 100ms end-to-end
        assert!(analysis.stages.len() == 3);
    }

    #[test]
    fn frankenterm_analysis_unstable() {
        // Service too slow for the load
        let profiles: Vec<_> = (0..200)
            .map(|_| PaneOutputProfile {
                burst_bytes: 100_000.0,
                sustained_rate_bps: 100_000.0,
            })
            .collect();

        let config = PipelineConfig {
            capture_rate_bps: 1_000_000.0, // 1MB/s — too slow for 200*100KB/s = 20MB/s
            capture_latency_s: 0.01,
            process_rate_bps: 1_000_000.0,
            process_latency_s: 0.01,
            storage_rate_bps: 1_000_000.0,
            storage_latency_s: 0.01,
        };

        let analysis = analyze_frankenterm_pipeline(&profiles, &config);
        assert!(!analysis.is_stable);
    }

    // ── Serde roundtrip tests ──

    #[test]
    fn arrival_curve_serde_roundtrip() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 10.0);
        let json = serde_json::to_string(&arr).unwrap();
        let back: ArrivalCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(arr, back);
    }

    #[test]
    fn service_curve_serde_roundtrip() {
        let svc = ServiceCurve::rate_latency(100.0, 0.01);
        let json = serde_json::to_string(&svc).unwrap();
        let back: ServiceCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(svc, back);
    }

    #[test]
    fn pipeline_analysis_serde_roundtrip() {
        let analysis = PipelineAnalysis {
            delay_bound: 0.01,
            backlog_bound: 1000.0,
            per_stage_delays: vec![0.005, 0.003, 0.002],
            per_stage_backlogs: vec![500.0, 300.0, 200.0],
            total_service_rate: 100_000.0,
            total_service_latency: 0.006,
        };
        let json = serde_json::to_string(&analysis).unwrap();
        let back: PipelineAnalysis = serde_json::from_str(&json).unwrap();
        assert_eq!(analysis, back);
    }

    // ── Deconvolution test ──

    #[test]
    fn deconvolution_sampled() {
        // f(t) = 2t, g(t) = t → (f ⊘ g)(t) = sup_{s≥0} {2(t+s) - s} = sup {2t + s}
        // This diverges, so let's use bounded functions:
        // f(t) = min(2t, 10), g(t) = t
        let f = |t: f64| (2.0 * t).min(10.0);
        let g = |t: f64| t;
        let result = min_plus_deconvolution_sampled(&f, &g, 20.0);
        // At t=0: sup_{s≥0} {min(2s, 10) - s} = sup {s for s≤5, 10-s for s>5} = 5 at s=5
        assert!(result.eval(0.0) > 4.5);
        assert!(result.eval(0.0) < 5.5);
    }

    #[test]
    fn empty_pipeline() {
        let pipeline = Pipeline::new(vec![]);
        let total = pipeline.total_service_curve();
        // Empty pipeline: infinite rate, no delay
        assert!(total.rate().is_infinite());
    }

    #[test]
    fn empty_aggregate() {
        let agg = aggregate_arrival(&[]);
        assert!(approx_eq(agg.eval(1.0), 0.0));
    }

    #[test]
    fn arrival_at_negative_time() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 10.0);
        assert!(approx_eq(arr.eval(-1.0), 0.0));
    }

    #[test]
    fn service_at_negative_time() {
        let svc = ServiceCurve::rate_latency(100.0, 0.01);
        assert!(approx_eq(svc.eval(-1.0), 0.0));
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ───────────────────────────────────

    // ── CurvePoint traits ──

    #[test]
    fn curve_point_debug() {
        let p = CurvePoint { t: 1.0, y: 2.0 };
        let dbg = format!("{:?}", p);
        assert!(dbg.contains("CurvePoint"));
    }

    #[test]
    fn curve_point_clone_copy_eq() {
        let p = CurvePoint { t: 3.0, y: 7.0 };
        let p2 = p; // Copy
        let p3 = p.clone();
        assert_eq!(p, p2);
        assert_eq!(p, p3);
    }

    #[test]
    fn curve_point_serde_roundtrip() {
        let p = CurvePoint { t: 1.5, y: 42.0 };
        let json = serde_json::to_string(&p).unwrap();
        let back: CurvePoint = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    // ── PiecewiseLinear additional ──

    #[test]
    fn piecewise_linear_len_and_is_empty() {
        let pw = PiecewiseLinear::constant(5.0);
        assert_eq!(pw.len(), 1);
        assert!(!pw.is_empty());

        let pw2 = PiecewiseLinear::linear(0.0, 1.0);
        assert_eq!(pw2.len(), 2);
    }

    #[test]
    fn piecewise_linear_debug() {
        let pw = PiecewiseLinear::constant(10.0);
        let dbg = format!("{:?}", pw);
        assert!(dbg.contains("PiecewiseLinear"));
    }

    #[test]
    fn piecewise_linear_clone_eq() {
        let pw = PiecewiseLinear::linear(1.0, 2.0);
        let cloned = pw.clone();
        assert_eq!(pw, cloned);
    }

    #[test]
    fn piecewise_linear_serde_roundtrip() {
        let pw = PiecewiseLinear::new(vec![
            CurvePoint { t: 0.0, y: 0.0 },
            CurvePoint { t: 1.0, y: 5.0 },
            CurvePoint { t: 3.0, y: 15.0 },
        ]);
        let json = serde_json::to_string(&pw).unwrap();
        let back: PiecewiseLinear = serde_json::from_str(&json).unwrap();
        assert_eq!(pw, back);
    }

    #[test]
    fn piecewise_linear_points_accessor() {
        let pw = PiecewiseLinear::new(vec![
            CurvePoint { t: 0.0, y: 1.0 },
            CurvePoint { t: 2.0, y: 3.0 },
        ]);
        let pts = pw.points();
        assert_eq!(pts.len(), 2);
        assert!(approx_eq(pts[0].t, 0.0));
        assert!(approx_eq(pts[1].y, 3.0));
    }

    #[test]
    fn piecewise_trailing_slope_constant() {
        let pw = PiecewiseLinear::constant(42.0);
        assert!(approx_eq(pw.trailing_slope(), 0.0));
    }

    // ── ArrivalCurve additional ──

    #[test]
    fn arrival_curve_token_bucket_serde_roundtrip() {
        let arr = ArrivalCurve::token_bucket(50.0, 10.0, 30.0);
        let json = serde_json::to_string(&arr).unwrap();
        let back: ArrivalCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(arr, back);
    }

    #[test]
    fn arrival_curve_staircase_serde_roundtrip() {
        let arr = ArrivalCurve::staircase(2.0, 5.0);
        let json = serde_json::to_string(&arr).unwrap();
        let back: ArrivalCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(arr, back);
    }

    #[test]
    fn arrival_curve_piecewise_serde_roundtrip() {
        let pw = PiecewiseLinear::linear(0.0, 10.0);
        let arr = ArrivalCurve::Piecewise(pw);
        let json = serde_json::to_string(&arr).unwrap();
        let back: ArrivalCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(arr, back);
    }

    #[test]
    fn arrival_curve_burst_all_variants() {
        assert!(approx_eq(
            ArrivalCurve::leaky_bucket(100.0, 10.0).burst(),
            100.0
        ));
        assert!(approx_eq(
            ArrivalCurve::token_bucket(200.0, 10.0, 50.0).burst(),
            200.0
        ));
        assert!(approx_eq(ArrivalCurve::staircase(2.0, 5.0).burst(), 5.0));
    }

    #[test]
    fn arrival_curve_sustained_rate_token_bucket() {
        let arr = ArrivalCurve::token_bucket(100.0, 15.0, 50.0);
        assert!(approx_eq(arr.sustained_rate(), 15.0));
    }

    #[test]
    fn staircase_zero_period() {
        let arr = ArrivalCurve::staircase(0.0, 5.0);
        assert!(approx_eq(arr.eval(1.0), 0.0));
        assert!(approx_eq(arr.sustained_rate(), 0.0));
    }

    #[test]
    fn arrival_curve_debug() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 10.0);
        let dbg = format!("{:?}", arr);
        assert!(dbg.contains("LeakyBucket"));
    }

    #[test]
    fn arrival_curve_clone_eq() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 10.0);
        let cloned = arr.clone();
        assert_eq!(arr, cloned);
    }

    // ── ServiceCurve additional ──

    #[test]
    fn service_curve_strict_rate_serde_roundtrip() {
        let svc = ServiceCurve::strict_rate(200.0);
        let json = serde_json::to_string(&svc).unwrap();
        let back: ServiceCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(svc, back);
    }

    #[test]
    fn service_curve_piecewise_serde_roundtrip() {
        let pw = PiecewiseLinear::linear(0.0, 50.0);
        let svc = ServiceCurve::Piecewise(pw);
        let json = serde_json::to_string(&svc).unwrap();
        let back: ServiceCurve = serde_json::from_str(&json).unwrap();
        assert_eq!(svc, back);
    }

    #[test]
    fn service_curve_rate_all_variants() {
        assert!(approx_eq(
            ServiceCurve::rate_latency(100.0, 0.01).rate(),
            100.0
        ));
        assert!(approx_eq(ServiceCurve::strict_rate(200.0).rate(), 200.0));
    }

    #[test]
    fn service_curve_latency_all_variants() {
        assert!(approx_eq(
            ServiceCurve::rate_latency(100.0, 0.01).latency(),
            0.01
        ));
        assert!(approx_eq(ServiceCurve::strict_rate(200.0).latency(), 0.0));
    }

    #[test]
    fn service_curve_piecewise_latency() {
        // First positive y determines the "latency"
        let pw = PiecewiseLinear::new(vec![
            CurvePoint { t: 0.0, y: 0.0 },
            CurvePoint { t: 0.5, y: 0.0 },
            CurvePoint { t: 1.0, y: 10.0 },
        ]);
        let svc = ServiceCurve::Piecewise(pw);
        assert!(approx_eq(svc.latency(), 1.0));
    }

    #[test]
    fn service_curve_debug() {
        let svc = ServiceCurve::rate_latency(100.0, 0.01);
        let dbg = format!("{:?}", svc);
        assert!(dbg.contains("RateLatency"));
    }

    #[test]
    fn service_curve_clone_eq() {
        let svc = ServiceCurve::rate_latency(100.0, 0.01);
        let cloned = svc.clone();
        assert_eq!(svc, cloned);
    }

    // ── Convolution: mixed strict/rate-latency ──

    #[test]
    fn convolution_strict_rate_with_rate_latency() {
        let a = ServiceCurve::strict_rate(100.0);
        let b = ServiceCurve::rate_latency(200.0, 0.01);
        let c = min_plus_convolution(&a, &b);
        match c {
            ServiceCurve::RateLatency { rate, latency } => {
                assert!(approx_eq(rate, 100.0));
                assert!(approx_eq(latency, 0.01));
            }
            _ => panic!("expected RateLatency"),
        }
    }

    #[test]
    fn convolution_rate_latency_with_strict_rate() {
        let a = ServiceCurve::rate_latency(150.0, 0.02);
        let b = ServiceCurve::strict_rate(300.0);
        let c = min_plus_convolution(&a, &b);
        match c {
            ServiceCurve::RateLatency { rate, latency } => {
                assert!(approx_eq(rate, 150.0));
                assert!(approx_eq(latency, 0.02));
            }
            _ => panic!("expected RateLatency"),
        }
    }

    // ── Pipeline + Analysis traits ──

    #[test]
    fn pipeline_stage_debug_clone_eq() {
        let stage = PipelineStage {
            name: "test".to_string(),
            service: ServiceCurve::strict_rate(100.0),
        };
        let cloned = stage.clone();
        assert_eq!(stage, cloned);
        let dbg = format!("{:?}", stage);
        assert!(dbg.contains("PipelineStage"));
    }

    #[test]
    fn pipeline_debug_clone_eq() {
        let pipeline = Pipeline::new(vec![PipelineStage {
            name: "s1".to_string(),
            service: ServiceCurve::strict_rate(100.0),
        }]);
        let cloned = pipeline.clone();
        assert_eq!(pipeline, cloned);
        let dbg = format!("{:?}", pipeline);
        assert!(dbg.contains("Pipeline"));
    }

    #[test]
    fn pipeline_analysis_debug_clone() {
        let analysis = PipelineAnalysis {
            delay_bound: 0.01,
            backlog_bound: 1000.0,
            per_stage_delays: vec![0.005],
            per_stage_backlogs: vec![500.0],
            total_service_rate: 100_000.0,
            total_service_latency: 0.005,
        };
        let cloned = analysis.clone();
        assert_eq!(analysis, cloned);
        let dbg = format!("{:?}", analysis);
        assert!(dbg.contains("PipelineAnalysis"));
    }

    // ── FrankenTerm-specific types traits ──

    #[test]
    fn pane_output_profile_debug_clone_eq_serde() {
        let profile = PaneOutputProfile {
            burst_bytes: 10_000.0,
            sustained_rate_bps: 1_000.0,
        };
        let cloned = profile.clone();
        assert_eq!(profile, cloned);
        let dbg = format!("{:?}", profile);
        assert!(dbg.contains("PaneOutputProfile"));

        let json = serde_json::to_string(&profile).unwrap();
        let back: PaneOutputProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
    }

    #[test]
    fn pipeline_config_debug_clone_eq_serde() {
        let config = PipelineConfig {
            capture_rate_bps: 1_000_000.0,
            capture_latency_s: 0.001,
            process_rate_bps: 500_000.0,
            process_latency_s: 0.002,
            storage_rate_bps: 200_000.0,
            storage_latency_s: 0.003,
        };
        let cloned = config.clone();
        assert_eq!(config, cloned);
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("PipelineConfig"));

        let json = serde_json::to_string(&config).unwrap();
        let back: PipelineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn frankenterm_analysis_debug_clone_eq_serde() {
        let analysis = FrankenTermAnalysis {
            max_delay_ms: 5.0,
            max_backlog_bytes: 10_000.0,
            stages: vec![StageAnalysis {
                name: "capture".to_string(),
                delay_bound_ms: 2.0,
                backlog_bound_bytes: 5_000.0,
            }],
            is_stable: true,
        };
        let cloned = analysis.clone();
        assert_eq!(analysis, cloned);
        let dbg = format!("{:?}", analysis);
        assert!(dbg.contains("FrankenTermAnalysis"));

        let json = serde_json::to_string(&analysis).unwrap();
        let back: FrankenTermAnalysis = serde_json::from_str(&json).unwrap();
        assert_eq!(analysis, back);
    }

    #[test]
    fn stage_analysis_debug_clone_eq_serde() {
        let sa = StageAnalysis {
            name: "storage".to_string(),
            delay_bound_ms: 3.0,
            backlog_bound_bytes: 2_000.0,
        };
        let cloned = sa.clone();
        assert_eq!(sa, cloned);
        let dbg = format!("{:?}", sa);
        assert!(dbg.contains("StageAnalysis"));

        let json = serde_json::to_string(&sa).unwrap();
        let back: StageAnalysis = serde_json::from_str(&json).unwrap();
        assert_eq!(sa, back);
    }

    // ── Delay/backlog bounds: leaky bucket + strict rate ──

    #[test]
    fn delay_bound_leaky_strict_rate() {
        // D = σ / (R - ρ)
        let arr = ArrivalCurve::leaky_bucket(500.0, 10.0);
        let svc = ServiceCurve::strict_rate(1000.0);
        let d = delay_bound(&arr, &svc);
        let expected = 500.0 / (1000.0 - 10.0);
        assert!(approx_eq(d, expected));
    }

    #[test]
    fn delay_bound_unstable_strict_rate() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 200.0);
        let svc = ServiceCurve::strict_rate(100.0);
        let d = delay_bound(&arr, &svc);
        assert!(d.is_infinite());
    }

    #[test]
    fn backlog_bound_unstable() {
        let arr = ArrivalCurve::leaky_bucket(100.0, 200.0);
        let svc = ServiceCurve::rate_latency(100.0, 0.01);
        let b = backlog_bound(&arr, &svc);
        assert!(b.is_infinite());
    }

    // ── Aggregate: mixed arrival curves ──

    #[test]
    fn aggregate_mixed_triggers_numerical() {
        let arrivals = vec![
            ArrivalCurve::leaky_bucket(100.0, 10.0),
            ArrivalCurve::staircase(2.0, 5.0),
        ];
        let agg = aggregate_arrival(&arrivals);
        // Mixed types → Piecewise result
        match &agg {
            ArrivalCurve::Piecewise(_) => {}
            _ => panic!("expected Piecewise for mixed arrivals"),
        }
        // At t=1: leaky=110, staircase=5 → sum=115
        let val = agg.eval(1.0);
        assert!(val > 100.0, "aggregate should be > 100 at t=1");
    }

    // ── Pipeline: single stage ──

    #[test]
    fn pipeline_single_stage() {
        let pipeline = Pipeline::new(vec![PipelineStage {
            name: "only".into(),
            service: ServiceCurve::rate_latency(1000.0, 0.01),
        }]);
        let total = pipeline.total_service_curve();
        assert!(approx_eq(total.rate(), 1000.0));
        assert!(approx_eq(total.latency(), 0.01));
    }

    // ── FrankenTerm: empty pane list ──

    #[test]
    fn frankenterm_analysis_empty_panes() {
        let config = PipelineConfig {
            capture_rate_bps: 1_000_000.0,
            capture_latency_s: 0.001,
            process_rate_bps: 500_000.0,
            process_latency_s: 0.002,
            storage_rate_bps: 200_000.0,
            storage_latency_s: 0.003,
        };
        let analysis = analyze_frankenterm_pipeline(&[], &config);
        assert!(analysis.is_stable);
        assert_eq!(analysis.stages.len(), 3);
    }
}
