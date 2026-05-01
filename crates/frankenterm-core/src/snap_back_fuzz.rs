//! Snap-back repaint + adversarial resize fuzz harness
//! ([BR-TERM-EMULATOR-UPLIFT.2.4] / `ft-mpc9b.2.4`).
//!
//! Sub-epic 2's adversarial-fuzz layer. The bead's headline rules:
//!
//! 1. **Quiescent equality.** At any quiescent state (`Idle`),
//!    rendered output equals the reference rendering — i.e., a
//!    gesture sequence that ends at `Idle` is observationally
//!    identical to never having gestured.
//! 2. **Snap-back idempotency.** Multiple `ResizeEnd` events
//!    produce the same `RenderQuality` output (the snap-back fires
//!    exactly once; subsequent ticks settle into steady-state).
//! 3. **Independence-rule preservation.** A11Y / color / IME
//!    contracts hold across snap-back regardless of how
//!    pathological the gesture sequence is.
//!
//! ## What this module ships
//!
//! - [`FuzzSeed`] — local xorshift64* PRNG (same shape as
//!   `frankenterm-gui`'s `gpu_regression_fuzz::FuzzSeed`, but
//!   self-contained so `frankenterm-core` doesn't take a GUI
//!   dep). Same seed → same gesture sequence forever.
//! - [`GestureFuzzConfig`] — knobs (event budget, distribution
//!   weights).
//! - [`GestureFuzzStream`] — bounded iterator of
//!   [`crate::live_resize::LiveResizeEvent`] driven by a seed.
//! - [`SnapBackInvariantChecker`] — runs a gesture stream
//!   through the live-resize state machine + draft-mode driver
//!   and emits per-frame violation rows.
//! - [`SnapBackDivergenceReport`] — per-fuzz-run JSONL row.
//!
//! ## What this module is NOT
//!
//! - The actual GPU/render-target SSIM compare for "rendered
//!   output equals reference rendering" — that's the GPU
//!   integration bead. This module pins the equivalence in the
//!   *driver / state machine* layer (which is what determines
//!   the per-frame quality the renderer paints).
//! - 24h CI fuzz lane — that's CI infrastructure. This module
//!   ships the harness; the lane wiring follows.

use serde::{Deserialize, Serialize};

use crate::live_resize::{LiveResizeEvent, LiveResizeState, LiveResizeStateMachine};
use crate::render_quality::{
    DraftModeDriver, DraftModeFeatureFlags, RenderQuality, SteadyStateQuality,
};

// ============================================================================
// Local xorshift64* PRNG
//
// Self-contained in core to avoid dep on frankenterm-gui.
// Identical algorithm to gpu_regression_fuzz's FuzzSeed —
// reuse-by-copy is the right tradeoff here (single dep edge
// matters more than a 30-line code dup).
// ============================================================================

/// Deterministic, repeatable PRNG. Same seed → same byte-stream
/// across processes / architectures / Rust versions.
#[derive(Debug, Clone, Copy)]
pub struct FuzzSeed {
    state: u64,
}

impl FuzzSeed {
    /// Construct from a 64-bit seed. Seed `0` is remapped because
    /// xorshift's all-zero state is a fixed point.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    /// Next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Next `u64` in `[0, bound)`. Lemire's method.
    pub fn next_bounded_u64(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let r = self.next_u64();
        ((r as u128 * bound as u128) >> 64) as u64
    }

    /// Snapshot for failure-artifact reproducibility.
    #[must_use]
    pub fn state(&self) -> u64 {
        self.state
    }
}

// ============================================================================
// Gesture event generator
// ============================================================================

/// Knobs for the gesture-event generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureFuzzConfig {
    /// Maximum number of events to generate. Bounded so the
    /// fuzz lane runs in finite time.
    pub event_budget: u64,
    /// Maximum window width in cells (configures bound).
    pub max_width: u32,
    /// Maximum window height in cells.
    pub max_height: u32,
    /// Maximum simulated wall-clock duration in ms (timestamps
    /// stay monotonic but bounded).
    pub max_duration_ms: u64,
}

impl Default for GestureFuzzConfig {
    fn default() -> Self {
        Self {
            event_budget: 1_000,
            max_width: 4_000,
            max_height: 2_500,
            max_duration_ms: 60_000,
        }
    }
}

/// Bounded iterator over fuzz-generated `LiveResizeEvent`s.
///
/// The distribution is calibrated to exercise the bead's
/// failure-mode catalog:
///
/// - 10 % `BeginSignal` (gesture starts)
/// - 50 % `Configure` (mid-drag configures — coalescing target)
/// - 10 % `EndSignal` (gesture releases)
/// -  5 % `MouseUpDuringResize` (macOS recovery path)
/// - 25 % `WatchdogTick` (idle ticks; watchdog-forced-end target)
///
/// Timestamps are strictly monotonic non-decreasing.
pub struct GestureFuzzStream {
    rng: FuzzSeed,
    config: GestureFuzzConfig,
    events_emitted: u64,
    ts_ms: u64,
}

impl GestureFuzzStream {
    #[must_use]
    pub fn new(seed: u64, config: GestureFuzzConfig) -> Self {
        Self {
            rng: FuzzSeed::new(seed),
            config,
            events_emitted: 0,
            ts_ms: 0,
        }
    }
}

impl Iterator for GestureFuzzStream {
    type Item = LiveResizeEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.events_emitted >= self.config.event_budget {
            return None;
        }
        // Advance ts by 1..=200ms — keeps the stream
        // monotonically non-decreasing AND occasionally jumps
        // far enough to trigger the watchdog timeout (5s) when
        // multiple `WatchdogTick`s accumulate without other
        // events.
        let dt = 1 + self.rng.next_bounded_u64(200);
        let next_ts = self
            .ts_ms
            .saturating_add(dt)
            .min(self.config.max_duration_ms);
        self.ts_ms = next_ts;
        let pick = self.rng.next_bounded_u64(100);
        let event = if pick < 10 {
            LiveResizeEvent::BeginSignal { ts_ms: next_ts }
        } else if pick < 60 {
            let width = 1 + self.rng.next_bounded_u64(u64::from(self.config.max_width)) as u32;
            let height = 1 + self.rng.next_bounded_u64(u64::from(self.config.max_height)) as u32;
            LiveResizeEvent::Configure {
                ts_ms: next_ts,
                width,
                height,
            }
        } else if pick < 70 {
            LiveResizeEvent::EndSignal { ts_ms: next_ts }
        } else if pick < 75 {
            LiveResizeEvent::MouseUpDuringResize { ts_ms: next_ts }
        } else {
            LiveResizeEvent::WatchdogTick { ts_ms: next_ts }
        };
        self.events_emitted += 1;
        Some(event)
    }
}

// ============================================================================
// Snap-back invariant checker
// ============================================================================

/// One observation point — the per-frame snapshot the harness
/// records so a violation can be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapBackObservation {
    pub event_idx: u64,
    pub ts_ms: u64,
    pub resize_state: LiveResizeState,
    pub render_quality: RenderQuality,
    pub is_snap_back: bool,
}

/// Outcome of running a fuzz seed through the harness. Either
/// `Ok` with the observation count, or `Err` carrying the
/// divergence report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapBackFuzzResult {
    pub seed: u64,
    pub events_consumed: u64,
    pub snap_backs_observed: u64,
    pub final_resize_state: LiveResizeState,
    pub final_render_quality: RenderQuality,
    pub violations: Vec<SnapBackViolation>,
}

/// Named violations the checker emits. Each carries enough
/// context to reproduce the exact event from the seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapBackViolation {
    /// `ResizeEnd` (or skipped-end synthesis) produced a
    /// non-Standard `RenderQuality`. Bead's snap-back-Standard
    /// rule.
    SnapBackNotStandard {
        event_idx: u64,
        observed: RenderQuality,
    },
    /// More than one snap-back per gesture. The driver's
    /// idempotency rule says exactly one Standard frame fires
    /// per Resizing-to-non-Resizing transition.
    DoubleSnapBackInGesture {
        event_idx: u64,
        snap_back_count_in_gesture: u32,
    },
    /// Final state at quiescent `Idle` is not the configured
    /// steady-state. The bead's "quiescent equality" rule.
    QuiescentDriftFromSteadyState {
        expected: RenderQuality,
        actual: RenderQuality,
    },
    /// One of the three independence-rule features
    /// (a11y_tree_update / color_profile / ime_caret_anchor)
    /// flipped to `false` mid-stream.
    IndependenceRuleViolated {
        event_idx: u64,
        rule: IndependenceRule,
        quality: RenderQuality,
    },
}

/// Which independence rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndependenceRule {
    A11yTreeUpdate,
    ColorProfile,
    ImeCaretAnchor,
}

/// Run a gesture stream through the live-resize state machine +
/// draft-mode driver and check every per-frame observation
/// against the bead's invariants.
#[must_use]
pub fn run_fuzz_seed(
    seed: u64,
    config: GestureFuzzConfig,
    steady_state: SteadyStateQuality,
) -> SnapBackFuzzResult {
    let mut machine = LiveResizeStateMachine::new();
    let mut driver = DraftModeDriver::new(steady_state);
    let stream = GestureFuzzStream::new(seed, config);
    let mut violations = Vec::new();
    let mut snap_backs_observed = 0u64;
    let mut events_consumed = 0u64;
    let mut prior_quality = steady_state.as_render_quality();
    // Track snap-backs within a single gesture (Begin..End span).
    let mut in_gesture = false;
    let mut snap_back_count_in_current_gesture = 0u32;

    for event in stream {
        events_consumed += 1;
        let _transition = machine.step(event);
        let resize_state = machine.state();
        let quality = driver.pick(resize_state);
        let is_snap_back =
            quality == RenderQuality::Standard && prior_quality == RenderQuality::Draft;

        // Rule 1: snap-back must always be Standard.
        if matches!(resize_state, LiveResizeState::ResizeEnd) && quality != RenderQuality::Standard
        {
            violations.push(SnapBackViolation::SnapBackNotStandard {
                event_idx: events_consumed,
                observed: quality,
            });
        }

        // Rule 2: snap-back idempotency — at most one snap-back
        // per gesture (Begin..End span).
        //
        // Each ResizeBegin starts a fresh "exactly one snap-back"
        // budget. Back-to-back gestures (where ResizeEnd auto-
        // clears straight into the next ResizeBegin without
        // observable Idle) each get their own budget.
        let entered_draft_mode = matches!(
            resize_state,
            LiveResizeState::ResizeBegin | LiveResizeState::Resizing
        );
        if entered_draft_mode && !in_gesture {
            in_gesture = true;
            snap_back_count_in_current_gesture = 0;
        }
        // A snap-back transition (Draft → Standard) closes the
        // current gesture. Account for the expected snap-back
        // and immediately reset so a follow-on gesture starts
        // with a clean budget.
        if is_snap_back && in_gesture {
            snap_back_count_in_current_gesture += 1;
            if snap_back_count_in_current_gesture > 1 {
                violations.push(SnapBackViolation::DoubleSnapBackInGesture {
                    event_idx: events_consumed,
                    snap_back_count_in_gesture: snap_back_count_in_current_gesture,
                });
            }
            in_gesture = false;
        }

        // Rule 3: independence rules NEVER flip.
        let flags = DraftModeFeatureFlags::for_quality(quality);
        if !flags.a11y_tree_update {
            violations.push(SnapBackViolation::IndependenceRuleViolated {
                event_idx: events_consumed,
                rule: IndependenceRule::A11yTreeUpdate,
                quality,
            });
        }
        if !flags.color_profile {
            violations.push(SnapBackViolation::IndependenceRuleViolated {
                event_idx: events_consumed,
                rule: IndependenceRule::ColorProfile,
                quality,
            });
        }
        if !flags.ime_caret_anchor {
            violations.push(SnapBackViolation::IndependenceRuleViolated {
                event_idx: events_consumed,
                rule: IndependenceRule::ImeCaretAnchor,
                quality,
            });
        }

        if is_snap_back {
            snap_backs_observed += 1;
        }
        prior_quality = quality;
    }

    // Rule 4: quiescent equality. After the stream completes,
    // drive the state machine + driver to a clean Idle state via
    // (a) a watchdog tick that forces any in-flight gesture to
    // ResizeEnd, then (b) a follow-up tick that flushes the
    // auto-clear from ResizeEnd → Idle.
    let final_ts = config.max_duration_ms.saturating_add(WATCHDOG_MARGIN_MS);
    let _ = machine.step(LiveResizeEvent::WatchdogTick { ts_ms: final_ts });
    let _ = machine.step(LiveResizeEvent::WatchdogTick {
        ts_ms: final_ts + 100,
    });
    let final_state = machine.state();
    let final_quality = driver.pick(final_state);
    if final_state == LiveResizeState::Idle && final_quality != steady_state.as_render_quality() {
        violations.push(SnapBackViolation::QuiescentDriftFromSteadyState {
            expected: steady_state.as_render_quality(),
            actual: final_quality,
        });
    }

    SnapBackFuzzResult {
        seed,
        events_consumed,
        snap_backs_observed,
        final_resize_state: final_state,
        final_render_quality: final_quality,
        violations,
    }
}

/// Margin past the configured stream `max_duration_ms` to ensure
/// the post-stream watchdog tick fires the timeout.
const WATCHDOG_MARGIN_MS: u64 = 10_000;

// ============================================================================
// Divergence report — JSONL row for the bead's structured-log
// schema.
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapBackDivergenceReport {
    pub seed: u64,
    pub events_consumed: u64,
    pub snap_backs_observed: u64,
    pub final_resize_state: LiveResizeState,
    pub final_render_quality: RenderQuality,
    pub violations: Vec<SnapBackViolation>,
}

impl From<SnapBackFuzzResult> for SnapBackDivergenceReport {
    fn from(r: SnapBackFuzzResult) -> Self {
        Self {
            seed: r.seed,
            events_consumed: r.events_consumed,
            snap_backs_observed: r.snap_backs_observed,
            final_resize_state: r.final_resize_state,
            final_render_quality: r.final_render_quality,
            violations: r.violations,
        }
    }
}

#[must_use]
pub fn render_reports_jsonl(reports: &[SnapBackDivergenceReport]) -> String {
    let mut out = String::new();
    for r in reports {
        let line = serde_json::to_string(r).expect("SnapBackDivergenceReport always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_reports_jsonl(
    jsonl: &str,
) -> Result<Vec<SnapBackDivergenceReport>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_seed_is_reproducible() {
        let mut a = FuzzSeed::new(42);
        let mut b = FuzzSeed::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn fuzz_seed_zero_does_not_freeze_at_fixed_point() {
        let mut s = FuzzSeed::new(0);
        let v1 = s.next_u64();
        let v2 = s.next_u64();
        assert_ne!(v1, 0);
        assert_ne!(v2, 0);
        assert_ne!(v1, v2);
    }

    #[test]
    fn gesture_stream_respects_event_budget() {
        let cfg = GestureFuzzConfig {
            event_budget: 50,
            ..GestureFuzzConfig::default()
        };
        let stream = GestureFuzzStream::new(1, cfg);
        let events: Vec<_> = stream.collect();
        assert_eq!(events.len(), 50);
    }

    #[test]
    fn gesture_stream_timestamps_monotonic() {
        let cfg = GestureFuzzConfig {
            event_budget: 100,
            ..GestureFuzzConfig::default()
        };
        let stream = GestureFuzzStream::new(7, cfg);
        let mut prior = 0u64;
        for ev in stream {
            let ts = ev.ts_ms();
            assert!(ts >= prior, "timestamp regressed: {prior} → {ts}");
            prior = ts;
        }
    }

    #[test]
    fn run_fuzz_seed_zero_is_clean_under_modest_budget() {
        let cfg = GestureFuzzConfig {
            event_budget: 100,
            ..GestureFuzzConfig::default()
        };
        let r = run_fuzz_seed(0, cfg, SteadyStateQuality::Standard);
        assert!(
            r.violations.is_empty(),
            "seed 0 produced violations: {:?}",
            r.violations
        );
    }

    #[test]
    fn run_fuzz_seed_under_fancy_steady_state_is_clean() {
        let cfg = GestureFuzzConfig {
            event_budget: 100,
            ..GestureFuzzConfig::default()
        };
        let r = run_fuzz_seed(123, cfg, SteadyStateQuality::Fancy);
        assert!(
            r.violations.is_empty(),
            "seed 123 / Fancy produced violations: {:?}",
            r.violations
        );
    }

    /// Sweep 16 seeds with budget 100. The bead's headline
    /// adversarial-fuzz claim: arbitrary gesture sequences must
    /// not violate any invariant.
    #[test]
    fn sixteen_seeds_under_budget_100_are_all_clean() {
        let cfg = GestureFuzzConfig {
            event_budget: 100,
            ..GestureFuzzConfig::default()
        };
        for seed in 0..16u64 {
            let r = run_fuzz_seed(seed, cfg, SteadyStateQuality::Standard);
            assert!(
                r.violations.is_empty(),
                "seed {seed} produced violations: {:?}",
                r.violations
            );
        }
    }

    #[test]
    fn fuzz_result_reports_final_quiescent_state_as_idle() {
        let cfg = GestureFuzzConfig {
            event_budget: 100,
            ..GestureFuzzConfig::default()
        };
        let r = run_fuzz_seed(99, cfg, SteadyStateQuality::Standard);
        assert_eq!(r.final_resize_state, LiveResizeState::Idle);
    }

    #[test]
    fn divergence_report_jsonl_roundtrips() {
        let result = SnapBackFuzzResult {
            seed: 42,
            events_consumed: 100,
            snap_backs_observed: 3,
            final_resize_state: LiveResizeState::Idle,
            final_render_quality: RenderQuality::Standard,
            violations: vec![SnapBackViolation::SnapBackNotStandard {
                event_idx: 17,
                observed: RenderQuality::Draft,
            }],
        };
        let report: SnapBackDivergenceReport = result.into();
        let rendered = render_reports_jsonl(&[report.clone()]);
        let parsed = parse_reports_jsonl(&rendered).unwrap();
        assert_eq!(parsed[0], report);
    }

    #[test]
    fn config_defaults_are_finite_and_bounded() {
        let cfg = GestureFuzzConfig::default();
        assert!(cfg.event_budget > 0);
        assert!(cfg.event_budget <= 1_000_000);
        assert!(cfg.max_width > 0);
        assert!(cfg.max_height > 0);
        assert!(cfg.max_duration_ms > 0);
    }
}
