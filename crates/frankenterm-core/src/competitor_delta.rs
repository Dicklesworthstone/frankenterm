//! Per-release competitor-delta proof substrate (ft-mpc9b.8).
//!
//! The bead's claim: "ft ≥ ghostty on resize-FPS metric" — without
//! published per-release proof, that's a claim, not a fact. This
//! substrate ships the pure-logic types + regression-detection
//! policy that the integration's bench harness drives:
//!
//! - `Competitor` — `Ft / WezTerm / Ghostty / Rio`. The bead's
//!   reference set.
//! - `Metric` — the named perf axes (`FpsP50 / FpsP95 / FpsP99 /
//!   FrameTimeP95Ms / GpuMemoryPeakMb / CpuPeakPct`). Pure data.
//! - `MetricKind` — `HigherIsBetter / LowerIsBetter`. Drives
//!   comparison sign.
//! - `MetricSample` — one terminal's reading on one metric.
//! - `CompetitorMatrix` — registry mapping (competitor, metric) to
//!   sample. The integration layer fills this from the bench JSON.
//! - `delta_pct` — pure-logic comparator: how much better/worse is
//!   ft vs a competitor on a metric, signed.
//! - `RegressionPolicy` — `MAX_REGRESSION_PCT = 10.0` per the bead
//!   ("ft is ≥10% slower than ghostty on any p95 metric for 2
//!   consecutive releases → file a P1 regression bead").
//! - `RegressionState` — running 2-consecutive-release detector
//!   per (metric, competitor). Transitions
//!   `Clean → SingleRegression → ConsecutiveRegression`.
//! - `HardwareBaseline` — pure-data identifier for the bench host
//!   (per the bead's table: M2 MacBook / Framework / Threadripper /
//!   GitHub-Actions-runner).
//! - `CompetitorMatrixSnapshot` — full per-release output struct
//!   matching the bead's `docs/perf/competitor-resize-<version>.json`
//!   schema.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.8.cont)
//!
//! - The actual bench harness: spawn 4 terminals via shell script,
//!   run resize-storm via xdotool/wayland-utils, capture timing via
//!   ftrace/Instruments.
//! - Per-release runner: GitHub Actions workflow that runs the
//!   bench on the documented runner SKU + local-baseline machines.
//! - JSON output writer at `docs/perf/competitor-resize-<version>.json`.
//! - Auto-file-P1-regression-bead-on-2-consecutive-regressions wiring
//!   (the substrate's `RegressionState::ConsecutiveRegression`
//!   variant is the policy-decision payload; the integration's
//!   release script reads it and runs `br create`).
//! - Cross-link to BR-RC-FOUNDATION.G3.5 (the competitor-matrix
//!   bead) for shared baseline data.

#![allow(dead_code)]

// ============================================================================
// Competitor identity
// ============================================================================

/// Reference competitor set per the bead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Competitor {
    /// frankenterm itself (the head currently being benchmarked).
    Ft,
    WezTerm,
    Ghostty,
    Rio,
}

impl Competitor {
    /// Human-readable name for telemetry / report rendering.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ft => "ft",
            Self::WezTerm => "wezterm",
            Self::Ghostty => "ghostty",
            Self::Rio => "rio",
        }
    }

    /// Iterate the four competitors in stable order. Used by the
    /// integration's matrix-render loop.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Ft, Self::WezTerm, Self::Ghostty, Self::Rio]
    }
}

// ============================================================================
// Metric identity
// ============================================================================

/// Named perf axes the bench harness captures per terminal per
/// release. Each `Metric` has a single `MetricKind` (higher-is-
/// better or lower-is-better) that drives the sign of `delta_pct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    /// Frame-rate median (higher is better).
    FpsP50,
    /// Frame-rate at 95th percentile (higher is better).
    FpsP95,
    /// Frame-rate at 99th percentile (higher is better).
    FpsP99,
    /// Frame-time at 95th percentile in ms (lower is better).
    FrameTimeP95Ms,
    /// GPU memory peak in MB (lower is better).
    GpuMemoryPeakMb,
    /// CPU peak as percent (lower is better).
    CpuPeakPct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricKind {
    HigherIsBetter,
    LowerIsBetter,
}

impl Metric {
    #[must_use]
    pub const fn kind(self) -> MetricKind {
        match self {
            Self::FpsP50 | Self::FpsP95 | Self::FpsP99 => MetricKind::HigherIsBetter,
            Self::FrameTimeP95Ms | Self::GpuMemoryPeakMb | Self::CpuPeakPct => {
                MetricKind::LowerIsBetter
            }
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FpsP50 => "fps_p50",
            Self::FpsP95 => "fps_p95",
            Self::FpsP99 => "fps_p99",
            Self::FrameTimeP95Ms => "frame_time_p95_ms",
            Self::GpuMemoryPeakMb => "gpu_memory_peak_mb",
            Self::CpuPeakPct => "cpu_peak_pct",
        }
    }

    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::FpsP50,
            Self::FpsP95,
            Self::FpsP99,
            Self::FrameTimeP95Ms,
            Self::GpuMemoryPeakMb,
            Self::CpuPeakPct,
        ]
    }
}

// ============================================================================
// MetricSample
// ============================================================================

/// One terminal's reading on one metric. The bench harness fills
/// `value` from its captured timing data; `unit` is informational
/// for report rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricSample {
    pub value: f64,
}

impl MetricSample {
    #[must_use]
    pub const fn new(value: f64) -> Self {
        Self { value }
    }
}

// ============================================================================
// Delta computation
// ============================================================================

/// Compute the percent delta of `ft` against `competitor` on a
/// metric. Sign-correct: positive = ft is *better*, negative = ft is
/// *worse*. Returns `None` when either sample is non-finite or the
/// competitor's value is zero (avoid div-by-zero).
///
/// Math:
/// - HigherIsBetter (FPS): `(ft - competitor) / competitor * 100`
/// - LowerIsBetter (frame time / memory / CPU):
///   `(competitor - ft) / competitor * 100`
#[must_use]
pub fn delta_pct(metric: Metric, ft: MetricSample, competitor: MetricSample) -> Option<f64> {
    if !ft.value.is_finite() || !competitor.value.is_finite() {
        return None;
    }
    if competitor.value == 0.0 {
        return None;
    }
    let raw = match metric.kind() {
        MetricKind::HigherIsBetter => (ft.value - competitor.value) / competitor.value,
        MetricKind::LowerIsBetter => (competitor.value - ft.value) / competitor.value,
    };
    Some(raw * 100.0)
}

// ============================================================================
// Regression policy
// ============================================================================

/// The bead's policy: "if ft is ≥10% slower than ghostty on any
/// p95 metric for 2 consecutive releases, file a P1 regression
/// bead". `MAX_REGRESSION_PCT` is the threshold; `delta_pct` <= this
/// negative value triggers `SingleRegression`.
pub const MAX_REGRESSION_PCT: f64 = -10.0;

/// Per-release classification for one (metric, competitor) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegressionClass {
    /// ft is at-or-better than the competitor (delta ≥ 0) or within
    /// the regression threshold (delta > MAX_REGRESSION_PCT).
    Clean,
    /// ft is below the regression threshold for this release. One
    /// more consecutive Regressed → `ConsecutiveRegression`.
    Regressed,
}

#[must_use]
pub fn classify_regression(delta: Option<f64>) -> RegressionClass {
    match delta {
        Some(d) if d > MAX_REGRESSION_PCT => RegressionClass::Clean,
        Some(_) => RegressionClass::Regressed,
        // Missing / non-finite delta — conservative: treat as Clean
        // (the bench result is bad, not the code; integration logs
        // separately).
        None => RegressionClass::Clean,
    }
}

/// Running detector: tracks the last release's classification per
/// (metric, competitor) and surfaces the 2-consecutive-regression
/// alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RegressionState {
    /// No regression last release.
    #[default]
    Clean,
    /// Regressed last release; one more triggers the alarm.
    SingleRegression,
    /// 2 consecutive regressions — file the P1 regression bead.
    /// Stays in this state until a `Clean` reading arrives.
    ConsecutiveRegression,
}

/// Transition signal emitted by `observe`. Lets the
/// integration distinguish "state stayed bad" from "state
/// just crossed into bad" so it files a P1 bead exactly
/// once per transition.
///
/// Self-review fix (br-ft-4zrdg): previously `should_file_p1`
/// returned true on every observation in
/// `ConsecutiveRegression`, so a 4-release-long bad streak
/// produced 3 duplicate P1 beads. The transition payload
/// closes that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RegressionTransition {
    /// No significant transition (state stayed Clean, stayed
    /// SingleRegression, or stayed ConsecutiveRegression).
    #[default]
    NoChange,
    /// Just entered SingleRegression. Operator-visible alert
    /// (yellow status); no P1 bead yet.
    EnteredSingleRegression,
    /// Just entered ConsecutiveRegression — file a P1 bead.
    /// Emitted exactly once per Clean→...→ConsecutiveRegression
    /// edge.
    EnteredConsecutive,
    /// Just recovered to Clean (from SingleRegression or
    /// ConsecutiveRegression). Integration may close any
    /// open P1 bead.
    Recovered,
}

impl RegressionState {
    /// Advance the detector with the latest release's class.
    /// Returns the transition payload so the integration can
    /// take action exactly on edges.
    pub fn observe(&mut self, class: RegressionClass) -> RegressionTransition {
        let prev = *self;
        let next = match (prev, class) {
            (Self::Clean, RegressionClass::Clean) => Self::Clean,
            (Self::Clean, RegressionClass::Regressed) => Self::SingleRegression,
            (Self::SingleRegression, RegressionClass::Clean) => Self::Clean,
            (Self::SingleRegression, RegressionClass::Regressed) => Self::ConsecutiveRegression,
            (Self::ConsecutiveRegression, RegressionClass::Clean) => Self::Clean,
            (Self::ConsecutiveRegression, RegressionClass::Regressed) => {
                Self::ConsecutiveRegression
            }
        };
        *self = next;
        match (prev, next) {
            (Self::Clean, Self::SingleRegression) => RegressionTransition::EnteredSingleRegression,
            (Self::SingleRegression, Self::ConsecutiveRegression) => {
                RegressionTransition::EnteredConsecutive
            }
            (Self::SingleRegression | Self::ConsecutiveRegression, Self::Clean) => {
                RegressionTransition::Recovered
            }
            _ => RegressionTransition::NoChange,
        }
    }

    /// Whether the integration's status display should show a
    /// P1 alert *right now*. Distinct from
    /// `RegressionTransition::EnteredConsecutive` (which fires
    /// exactly once at the edge) — this predicate stays true
    /// for the duration of the bad-state.
    #[must_use]
    pub fn should_file_p1(self) -> bool {
        matches!(self, Self::ConsecutiveRegression)
    }
}

// ============================================================================
// CompetitorMatrix — registry
// ============================================================================

/// Per-(competitor, metric) sample registry. The integration's bench
/// harness builds one per release; `CompetitorMatrixSnapshot`
/// serialises it.
#[derive(Debug, Clone, Default)]
pub struct CompetitorMatrix {
    samples: Vec<((Competitor, Metric), MetricSample)>,
}

impl CompetitorMatrix {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, competitor: Competitor, metric: Metric, sample: MetricSample) {
        let key = (competitor, metric);
        if let Some(slot) = self.samples.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = sample;
        } else {
            self.samples.push((key, sample));
        }
    }

    #[must_use]
    pub fn get(&self, competitor: Competitor, metric: Metric) -> Option<MetricSample> {
        self.samples
            .iter()
            .find(|(k, _)| *k == (competitor, metric))
            .map(|(_, s)| *s)
    }

    /// Compute the delta of ft vs `competitor` on `metric`. Returns
    /// `None` if either sample is missing.
    #[must_use]
    pub fn ft_vs(&self, competitor: Competitor, metric: Metric) -> Option<f64> {
        let ft = self.get(Competitor::Ft, metric)?;
        let other = self.get(competitor, metric)?;
        delta_pct(metric, ft, other)
    }

    /// Number of (competitor, metric) entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

// ============================================================================
// HardwareBaseline
// ============================================================================

/// Per the bead's "Hardware baseline (declared once, used per
/// release)" table. Each release's snapshot records which baseline
/// was used so cross-release deltas are apples-to-apples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HardwareBaseline {
    /// Local macOS: M2 MacBook Pro (16GB unified memory, 10-core).
    M2MacBookPro16Gb,
    /// Local Linux: Framework 13 (i7-1370P, 32GB RAM, integrated
    /// graphics).
    FrameworkLaptop13,
    /// Local Linux: Threadripper workstation (24-core, RTX 4070).
    ThreadripperRtx4070,
    /// GitHub Actions runner — exact SKU lives in the bench JSON's
    /// `runner_sku` field.
    GithubActionsRunner,
}

impl HardwareBaseline {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::M2MacBookPro16Gb => "m2-macbook-pro-16gb",
            Self::FrameworkLaptop13 => "framework-laptop-13-i7",
            Self::ThreadripperRtx4070 => "threadripper-rtx-4070",
            Self::GithubActionsRunner => "github-actions-runner",
        }
    }
}

// ============================================================================
// Snapshot — per-release output
// ============================================================================

/// Per-release snapshot matching the bead's
/// `docs/perf/competitor-resize-<version>.json` schema. The
/// integration's bench writer serialises this; the substrate
/// constructs + queries it.
#[derive(Debug, Clone)]
pub struct CompetitorMatrixSnapshot {
    pub release_version: String,
    pub baseline: HardwareBaseline,
    pub matrix: CompetitorMatrix,
}

impl CompetitorMatrixSnapshot {
    #[must_use]
    pub fn new(
        release_version: impl Into<String>,
        baseline: HardwareBaseline,
        matrix: CompetitorMatrix,
    ) -> Self {
        Self {
            release_version: release_version.into(),
            baseline,
            matrix,
        }
    }

    /// Walk the matrix and classify ft's regression state vs each
    /// competitor on each metric. Returns the per-(metric,
    /// competitor) class so the integration's regression detector
    /// can advance its state machine.
    #[must_use]
    pub fn classify_all(&self) -> Vec<((Metric, Competitor), RegressionClass)> {
        let mut classes = Vec::new();
        for &metric in Metric::all() {
            for &competitor in Competitor::all() {
                if matches!(competitor, Competitor::Ft) {
                    continue;
                }
                let class = classify_regression(self.matrix.ft_vs(competitor, metric));
                classes.push(((metric, competitor), class));
            }
        }
        classes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: f64) -> MetricSample {
        MetricSample::new(v)
    }

    // ----------------------------------------------------------------
    // Competitor + Metric
    // ----------------------------------------------------------------

    #[test]
    fn competitor_all_lists_four_in_order_with_ft_first() {
        let all = Competitor::all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], Competitor::Ft);
    }

    #[test]
    fn competitor_labels_match_bead() {
        assert_eq!(Competitor::Ft.label(), "ft");
        assert_eq!(Competitor::WezTerm.label(), "wezterm");
        assert_eq!(Competitor::Ghostty.label(), "ghostty");
        assert_eq!(Competitor::Rio.label(), "rio");
    }

    #[test]
    fn metric_kind_higher_is_better_for_fps() {
        assert_eq!(Metric::FpsP50.kind(), MetricKind::HigherIsBetter);
        assert_eq!(Metric::FpsP95.kind(), MetricKind::HigherIsBetter);
        assert_eq!(Metric::FpsP99.kind(), MetricKind::HigherIsBetter);
    }

    #[test]
    fn metric_kind_lower_is_better_for_costs() {
        assert_eq!(Metric::FrameTimeP95Ms.kind(), MetricKind::LowerIsBetter);
        assert_eq!(Metric::GpuMemoryPeakMb.kind(), MetricKind::LowerIsBetter);
        assert_eq!(Metric::CpuPeakPct.kind(), MetricKind::LowerIsBetter);
    }

    #[test]
    fn metric_all_lists_six() {
        assert_eq!(Metric::all().len(), 6);
    }

    // ----------------------------------------------------------------
    // delta_pct sign correctness
    // ----------------------------------------------------------------

    #[test]
    fn delta_higher_is_better_positive_when_ft_higher() {
        // ft 110 fps vs competitor 100 fps → +10%.
        let d = delta_pct(Metric::FpsP95, s(110.0), s(100.0)).unwrap();
        assert!((d - 10.0).abs() < 0.001);
    }

    #[test]
    fn delta_higher_is_better_negative_when_ft_lower() {
        // ft 90 fps vs competitor 100 fps → -10%.
        let d = delta_pct(Metric::FpsP95, s(90.0), s(100.0)).unwrap();
        assert!((d - -10.0).abs() < 0.001);
    }

    #[test]
    fn delta_lower_is_better_positive_when_ft_lower() {
        // ft 9ms vs competitor 10ms → ft is 10% better.
        let d = delta_pct(Metric::FrameTimeP95Ms, s(9.0), s(10.0)).unwrap();
        assert!((d - 10.0).abs() < 0.001);
    }

    #[test]
    fn delta_lower_is_better_negative_when_ft_higher() {
        // ft 11ms vs competitor 10ms → ft is 10% worse.
        let d = delta_pct(Metric::FrameTimeP95Ms, s(11.0), s(10.0)).unwrap();
        assert!((d - -10.0).abs() < 0.001);
    }

    #[test]
    fn delta_div_by_zero_returns_none() {
        assert_eq!(delta_pct(Metric::FpsP95, s(60.0), s(0.0)), None);
    }

    #[test]
    fn delta_non_finite_returns_none() {
        assert_eq!(delta_pct(Metric::FpsP95, s(f64::NAN), s(60.0)), None);
        assert_eq!(delta_pct(Metric::FpsP95, s(60.0), s(f64::INFINITY)), None);
    }

    // ----------------------------------------------------------------
    // RegressionClass thresholding
    // ----------------------------------------------------------------

    #[test]
    fn classify_clean_when_ft_better() {
        assert_eq!(classify_regression(Some(5.0)), RegressionClass::Clean);
        assert_eq!(classify_regression(Some(50.0)), RegressionClass::Clean);
    }

    #[test]
    fn classify_clean_when_within_regression_threshold() {
        // -9.99% is just inside the threshold.
        assert_eq!(classify_regression(Some(-9.99)), RegressionClass::Clean);
        // -5% is comfortably inside.
        assert_eq!(classify_regression(Some(-5.0)), RegressionClass::Clean);
    }

    #[test]
    fn classify_regressed_at_or_below_threshold() {
        // Exactly -10% → Regressed (the bead says ≥10% slower).
        assert_eq!(classify_regression(Some(-10.0)), RegressionClass::Regressed);
        assert_eq!(classify_regression(Some(-15.0)), RegressionClass::Regressed);
        assert_eq!(classify_regression(Some(-50.0)), RegressionClass::Regressed);
    }

    #[test]
    fn classify_clean_when_delta_missing() {
        // None → Clean (defensive: bench bug, not code regression).
        assert_eq!(classify_regression(None), RegressionClass::Clean);
    }

    // ----------------------------------------------------------------
    // RegressionState machine
    // ----------------------------------------------------------------

    #[test]
    fn state_default_is_clean() {
        assert_eq!(RegressionState::default(), RegressionState::Clean);
    }

    #[test]
    fn state_clean_to_single_to_consecutive() {
        let mut s = RegressionState::default();
        assert!(!s.should_file_p1());
        s.observe(RegressionClass::Regressed);
        assert_eq!(s, RegressionState::SingleRegression);
        assert!(!s.should_file_p1());
        s.observe(RegressionClass::Regressed);
        assert_eq!(s, RegressionState::ConsecutiveRegression);
        assert!(s.should_file_p1());
    }

    #[test]
    fn state_clean_observation_resets_from_single() {
        let mut s = RegressionState::default();
        s.observe(RegressionClass::Regressed);
        assert_eq!(s, RegressionState::SingleRegression);
        s.observe(RegressionClass::Clean);
        assert_eq!(s, RegressionState::Clean);
    }

    #[test]
    fn state_clean_observation_resets_from_consecutive() {
        let mut s = RegressionState::default();
        s.observe(RegressionClass::Regressed);
        s.observe(RegressionClass::Regressed);
        assert_eq!(s, RegressionState::ConsecutiveRegression);
        s.observe(RegressionClass::Clean);
        assert_eq!(s, RegressionState::Clean);
        assert!(!s.should_file_p1());
    }

    #[test]
    fn state_consecutive_stays_until_clean() {
        let mut s = RegressionState::ConsecutiveRegression;
        s.observe(RegressionClass::Regressed);
        assert_eq!(s, RegressionState::ConsecutiveRegression);
        s.observe(RegressionClass::Regressed);
        assert_eq!(s, RegressionState::ConsecutiveRegression);
    }

    #[test]
    fn observe_emits_entered_consecutive_exactly_once() {
        // Self-review fix (br-ft-4zrdg): the transition payload
        // fires once on the SingleRegression→ConsecutiveRegression
        // edge so the integration files exactly one P1 bead.
        let mut s = RegressionState::default();
        let t1 = s.observe(RegressionClass::Regressed);
        assert_eq!(t1, RegressionTransition::EnteredSingleRegression);
        let t2 = s.observe(RegressionClass::Regressed);
        assert_eq!(t2, RegressionTransition::EnteredConsecutive);
        // Subsequent regressions don't re-fire.
        let t3 = s.observe(RegressionClass::Regressed);
        assert_eq!(t3, RegressionTransition::NoChange);
        let t4 = s.observe(RegressionClass::Regressed);
        assert_eq!(t4, RegressionTransition::NoChange);
        // should_file_p1 is still true the whole time (status
        // display).
        assert!(s.should_file_p1());
    }

    #[test]
    fn observe_emits_recovered_when_returning_to_clean() {
        let mut s = RegressionState::default();
        s.observe(RegressionClass::Regressed);
        s.observe(RegressionClass::Regressed);
        let t = s.observe(RegressionClass::Clean);
        assert_eq!(t, RegressionTransition::Recovered);
        assert_eq!(s, RegressionState::Clean);
    }

    #[test]
    fn observe_no_change_for_steady_clean() {
        let mut s = RegressionState::default();
        let t = s.observe(RegressionClass::Clean);
        assert_eq!(t, RegressionTransition::NoChange);
    }

    #[test]
    fn observe_recovered_from_single_regression() {
        let mut s = RegressionState::default();
        s.observe(RegressionClass::Regressed);
        let t = s.observe(RegressionClass::Clean);
        assert_eq!(t, RegressionTransition::Recovered);
    }

    #[test]
    fn observe_no_duplicate_p1_filing_across_4_release_streak() {
        // 4-release bad streak: we want exactly one
        // EnteredConsecutive event, not 3.
        let mut s = RegressionState::default();
        let mut p1_filings = 0;
        for _ in 0..4 {
            let t = s.observe(RegressionClass::Regressed);
            if matches!(t, RegressionTransition::EnteredConsecutive) {
                p1_filings += 1;
            }
        }
        assert_eq!(p1_filings, 1);
    }

    // ----------------------------------------------------------------
    // CompetitorMatrix
    // ----------------------------------------------------------------

    #[test]
    fn matrix_empty_default() {
        let m = CompetitorMatrix::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn matrix_insert_get_round_trip() {
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(110.0));
        m.insert(Competitor::Ghostty, Metric::FpsP95, s(100.0));
        assert_eq!(m.get(Competitor::Ft, Metric::FpsP95), Some(s(110.0)));
        assert_eq!(m.get(Competitor::Ghostty, Metric::FpsP95), Some(s(100.0)));
        assert_eq!(m.get(Competitor::Rio, Metric::FpsP95), None);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn matrix_insert_overwrites_existing_key() {
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(100.0));
        m.insert(Competitor::Ft, Metric::FpsP95, s(120.0));
        assert_eq!(m.get(Competitor::Ft, Metric::FpsP95), Some(s(120.0)));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn matrix_ft_vs_competitor() {
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(110.0));
        m.insert(Competitor::Ghostty, Metric::FpsP95, s(100.0));
        let d = m.ft_vs(Competitor::Ghostty, Metric::FpsP95).unwrap();
        assert!((d - 10.0).abs() < 0.001);
    }

    #[test]
    fn matrix_ft_vs_returns_none_when_either_missing() {
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(110.0));
        // Ghostty sample missing.
        assert_eq!(m.ft_vs(Competitor::Ghostty, Metric::FpsP95), None);
    }

    // ----------------------------------------------------------------
    // CompetitorMatrixSnapshot.classify_all
    // ----------------------------------------------------------------

    #[test]
    fn snapshot_classify_all_emits_per_metric_per_non_ft_competitor() {
        // 6 metrics × 3 non-ft competitors = 18 classifications.
        let mut m = CompetitorMatrix::new();
        for &metric in Metric::all() {
            for &competitor in Competitor::all() {
                m.insert(competitor, metric, s(100.0)); // identical → 0% delta → Clean
            }
        }
        let snap = CompetitorMatrixSnapshot::new("v0.2.0", HardwareBaseline::M2MacBookPro16Gb, m);
        let classes = snap.classify_all();
        assert_eq!(classes.len(), 18);
        assert!(classes.iter().all(|(_, c)| *c == RegressionClass::Clean));
    }

    #[test]
    fn snapshot_classify_all_finds_regression_below_threshold() {
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(85.0));
        m.insert(Competitor::Ghostty, Metric::FpsP95, s(100.0));
        // Other metrics: identical (Clean).
        for &metric in Metric::all() {
            if metric == Metric::FpsP95 {
                continue;
            }
            for &competitor in Competitor::all() {
                m.insert(competitor, metric, s(100.0));
            }
        }
        // Fill missing ft / wezterm / rio FpsP95 with identical values
        // so only the ghostty match shows a regression.
        m.insert(Competitor::WezTerm, Metric::FpsP95, s(85.0));
        m.insert(Competitor::Rio, Metric::FpsP95, s(85.0));
        let snap = CompetitorMatrixSnapshot::new("v0.2.0", HardwareBaseline::M2MacBookPro16Gb, m);
        let classes = snap.classify_all();
        let regressed: Vec<_> = classes
            .iter()
            .filter(|(_, c)| *c == RegressionClass::Regressed)
            .collect();
        assert_eq!(regressed.len(), 1);
        assert_eq!(regressed[0].0, (Metric::FpsP95, Competitor::Ghostty));
    }

    // ----------------------------------------------------------------
    // HardwareBaseline labels
    // ----------------------------------------------------------------

    #[test]
    fn baseline_labels_match_bead_table() {
        assert_eq!(
            HardwareBaseline::M2MacBookPro16Gb.label(),
            "m2-macbook-pro-16gb"
        );
        assert_eq!(
            HardwareBaseline::FrameworkLaptop13.label(),
            "framework-laptop-13-i7"
        );
        assert_eq!(
            HardwareBaseline::ThreadripperRtx4070.label(),
            "threadripper-rtx-4070"
        );
        assert_eq!(
            HardwareBaseline::GithubActionsRunner.label(),
            "github-actions-runner"
        );
    }

    // ----------------------------------------------------------------
    // Cross-cut: bead's exact threshold
    // ----------------------------------------------------------------

    #[test]
    fn scenario_bead_threshold_exactly_10_pct() {
        // ft 90 fps vs ghostty 100 fps → -10% delta → Regressed
        // (matches the bead's "≥10% slower").
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(90.0));
        m.insert(Competitor::Ghostty, Metric::FpsP95, s(100.0));
        let delta = m.ft_vs(Competitor::Ghostty, Metric::FpsP95).unwrap();
        assert!((delta - -10.0).abs() < 0.001);
        assert_eq!(classify_regression(Some(delta)), RegressionClass::Regressed);
    }

    #[test]
    fn scenario_two_release_regression_pipeline_files_p1() {
        // Release N: ft 90 / ghostty 100 → Regressed.
        // Release N+1: ft 88 / ghostty 100 → Regressed → ConsecutiveRegression.
        let mut state = RegressionState::default();
        let mut m = CompetitorMatrix::new();
        m.insert(Competitor::Ft, Metric::FpsP95, s(90.0));
        m.insert(Competitor::Ghostty, Metric::FpsP95, s(100.0));
        state.observe(classify_regression(
            m.ft_vs(Competitor::Ghostty, Metric::FpsP95),
        ));
        assert_eq!(state, RegressionState::SingleRegression);
        assert!(!state.should_file_p1());

        // Release N+1: ft slipped further.
        m.insert(Competitor::Ft, Metric::FpsP95, s(88.0));
        state.observe(classify_regression(
            m.ft_vs(Competitor::Ghostty, Metric::FpsP95),
        ));
        assert_eq!(state, RegressionState::ConsecutiveRegression);
        assert!(
            state.should_file_p1(),
            "two consecutive regressions must trigger P1 bead filing"
        );

        // Release N+2: recovered.
        m.insert(Competitor::Ft, Metric::FpsP95, s(105.0));
        state.observe(classify_regression(
            m.ft_vs(Competitor::Ghostty, Metric::FpsP95),
        ));
        assert_eq!(state, RegressionState::Clean);
        assert!(!state.should_file_p1());
    }

    #[test]
    fn scenario_one_release_regression_then_recovery_does_not_file() {
        // Release N: regressed.
        // Release N+1: recovered.
        // Should NOT file P1.
        let mut state = RegressionState::default();
        state.observe(RegressionClass::Regressed);
        assert!(!state.should_file_p1());
        state.observe(RegressionClass::Clean);
        assert_eq!(state, RegressionState::Clean);
        assert!(!state.should_file_p1());
    }
}
