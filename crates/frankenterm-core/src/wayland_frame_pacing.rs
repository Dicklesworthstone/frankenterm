//! Wayland frame-callback pacing decision
//! ([BR-TERM-EMULATOR-UPLIFT.3.2] / `ft-mpc9b.3.2`).
//!
//! The bug the bead targets:
//!
//! > Under fast resize, frame callbacks can chain, the compositor
//! > floods, and paint stutters. Repro: drag a window edge rapidly
//! > back and forth on Wayland (any major compositor) for 5
//! > seconds → text smears, lags behind cursor, phantom frames.
//!
//! The chain-depth instrumentation shipped earlier
//! (commit 151bde5fe — instrumentation + 578 audit). This module
//! ships the **pacing decision** as a pure function plus the
//! chain-depth ≥3 guard the bead asks for. The Wayland
//! `window.rs::do_paint` consumes this for its paint/skip/queue
//! choice; future-platform paint loops (X11 has a sibling
//! ConfigureNotify-burst issue documented in `live_resize`) can
//! reuse the same decision shape.
//!
//! ## What this module ships
//!
//! - [`FramePacingState`] — minimal observable state (pending
//!   callback, chain depth, invalidated flag).
//! - [`FramePacingDecision`] — Paint / Skip / Queue.
//! - [`decide`] — pure function mapping state → decision.
//! - [`MAX_CHAIN_DEPTH`] — the bead's "≥3 → skip new requests
//!   until current callback fires" guard.
//! - [`FramePacingEvent`] — structured-log row matching the
//!   bead's `tests/wayland_frame_pacing/logs/<scenario>.jsonl`
//!   schema.
//! - [`FramePacingHealth`] — counter snapshot for `ft doctor`.
//!
//! ## Invariants the regression net pins
//!
//! 1. **Chain-depth ceiling.** After processing any sequence of
//!    paint/callback events, `chain_depth <= MAX_CHAIN_DEPTH`.
//! 2. **Coalescing.** Multiple Paint requests while a callback is
//!    pending collapse to a single deferred Paint (subsequent
//!    requests return `Skip` and bump the `coalesced_total`
//!    counter; the deferred Paint fires once when the callback
//!    arrives).
//! 3. **No starvation.** A pending callback eventually clears (the
//!    Wayland compositor is responsible for delivering it; the
//!    pacer doesn't infinite-loop in any state). Modeled as: every
//!    `Skip` decision is preceded by a pending callback that will
//!    be cleared via `mark_callback_fired`.
//!
//! ## What this module is NOT
//!
//! - The actual `surface.frame()` call — that's Wayland-specific
//!   and lives in `frankenterm/window/src/os/wayland/window.rs`.
//! - A multi-compositor integration test — that's the per-Tier-1
//!   compositor follow-on.
//! - The fix for the configure-vs-window_configure separation at
//!   line 578 — already audited as independent (commit 151bde5fe).

use serde::{Deserialize, Serialize};

// ============================================================================
// Constants
// ============================================================================

/// Maximum tolerated frame-callback chain depth before the pacer
/// returns `Skip` regardless of the `is_some()` guard. The bead's
/// load-bearing rule: "if chain ≥ 3, skip new requests until
/// current callback fires."
pub const MAX_CHAIN_DEPTH: u32 = 3;

// ============================================================================
// State + decision
// ============================================================================

/// Minimal observable state the pacer reasons about. Maps onto
/// `WaylandWindowInner`'s fields:
///
/// - `pending_callback` ↔ `frame_callback.is_some()`
/// - `chain_depth` ↔ `frame_callback_chain_depth`
/// - `invalidated` ↔ `invalidated`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePacingState {
    pub pending_callback: bool,
    pub chain_depth: u32,
    pub invalidated: bool,
}

impl FramePacingState {
    #[must_use]
    pub const fn idle() -> Self {
        Self {
            pending_callback: false,
            chain_depth: 0,
            invalidated: false,
        }
    }
}

/// What the pacer's `decide` says to do for a paint request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FramePacingDecision {
    /// Proceed with the paint: request a new frame callback +
    /// dispatch NeedRepaint.
    Paint,
    /// Skip this paint attempt; mark `invalidated` so the next
    /// callback-arrival path picks it up.
    Skip,
    /// The chain-depth ceiling fired. Skip without setting
    /// `invalidated` — the in-flight callbacks will eventually
    /// clear and a fresh paint request should fire then.
    SkipChainCeiling,
}

/// Pure pacing decision. The Wayland `window.rs::do_paint` calls
/// this once per paint attempt and acts on the verdict.
#[must_use]
pub fn decide(state: FramePacingState) -> FramePacingDecision {
    if state.chain_depth >= MAX_CHAIN_DEPTH {
        // The bead's #3 guard: even if `pending_callback` somehow
        // got cleared, the chain depth speaks. Refuse the new
        // request until callbacks drain.
        return FramePacingDecision::SkipChainCeiling;
    }
    if state.pending_callback {
        FramePacingDecision::Skip
    } else {
        FramePacingDecision::Paint
    }
}

// ============================================================================
// Counter machine
//
// Caller-managed FramePacingState is enough for `decide` itself,
// but the bead's structured-logging schema needs cumulative
// counters. The machine wraps `decide` + counter bookkeeping so
// the integration site can swap its existing chain-depth
// instrumentation for this single object.
// ============================================================================

/// Cumulative counter machine. Wraps `FramePacingState` + counters
/// + emits per-decision structured-log events.
#[derive(Debug, Clone)]
pub struct FramePacer {
    state: FramePacingState,
    paints_total: u64,
    skipped_pending_callback_total: u64,
    skipped_chain_ceiling_total: u64,
    coalesced_total: u64,
    callback_fires_total: u64,
    chain_depth_peak: u32,
}

impl Default for FramePacer {
    fn default() -> Self {
        Self::new()
    }
}

impl FramePacer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: FramePacingState::idle(),
            paints_total: 0,
            skipped_pending_callback_total: 0,
            skipped_chain_ceiling_total: 0,
            coalesced_total: 0,
            callback_fires_total: 0,
            chain_depth_peak: 0,
        }
    }

    /// Read-only view of the current state.
    #[must_use]
    pub fn state(&self) -> FramePacingState {
        self.state
    }

    /// Process a paint request. Returns the decision the caller
    /// should act on. Updates internal counters.
    pub fn request_paint(&mut self) -> FramePacingDecision {
        let decision = decide(self.state);
        match decision {
            FramePacingDecision::Paint => {
                self.paints_total += 1;
                self.state.pending_callback = true;
                self.state.chain_depth = self.state.chain_depth.saturating_add(1);
                if self.state.chain_depth > self.chain_depth_peak {
                    self.chain_depth_peak = self.state.chain_depth;
                }
                self.state.invalidated = false;
            }
            FramePacingDecision::Skip => {
                self.skipped_pending_callback_total += 1;
                if self.state.invalidated {
                    self.coalesced_total += 1;
                }
                self.state.invalidated = true;
            }
            FramePacingDecision::SkipChainCeiling => {
                self.skipped_chain_ceiling_total += 1;
                self.state.invalidated = true;
            }
        }
        decision
    }

    /// The compositor's frame_callback fired. Pairs with the
    /// `Paint` decision's increment of `chain_depth`. Returns
    /// `true` iff a deferred paint should fire next (the
    /// `invalidated` flag was set).
    pub fn mark_callback_fired(&mut self) -> bool {
        self.callback_fires_total += 1;
        if self.state.chain_depth > 0 {
            self.state.chain_depth -= 1;
        }
        if self.state.chain_depth == 0 {
            self.state.pending_callback = false;
        }
        let deferred = self.state.invalidated && !self.state.pending_callback;
        if deferred {
            self.state.invalidated = false;
        }
        deferred
    }

    /// Cumulative counter snapshot for `ft doctor`.
    #[must_use]
    pub fn health(&self) -> FramePacingHealth {
        FramePacingHealth {
            paints_total: self.paints_total,
            skipped_pending_callback_total: self.skipped_pending_callback_total,
            skipped_chain_ceiling_total: self.skipped_chain_ceiling_total,
            coalesced_total: self.coalesced_total,
            callback_fires_total: self.callback_fires_total,
            chain_depth_peak: self.chain_depth_peak,
        }
    }
}

// ============================================================================
// Structured log row
// ============================================================================

/// One row of the bead's `tests/wayland_frame_pacing/logs/<scenario>.jsonl`
/// schema: ts, frame_callback_pending, action_taken
/// (paint/skip/queue/skip_chain_ceiling), chain_depth, compositor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePacingEvent {
    pub ts_ms: u64,
    pub frame_callback_pending: bool,
    pub chain_depth: u32,
    pub decision: FramePacingDecision,
    /// Compositor identifier. `None` when the integration layer
    /// can't determine it (rare: e.g., headless tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor: Option<String>,
}

/// Cumulative snapshot for the `ft doctor` surface, mirroring the
/// `*Health` shape from prior beads in this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePacingHealth {
    pub paints_total: u64,
    pub skipped_pending_callback_total: u64,
    pub skipped_chain_ceiling_total: u64,
    pub coalesced_total: u64,
    pub callback_fires_total: u64,
    pub chain_depth_peak: u32,
}

impl FramePacingHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            paints_total: 0,
            skipped_pending_callback_total: 0,
            skipped_chain_ceiling_total: 0,
            coalesced_total: 0,
            callback_fires_total: 0,
            chain_depth_peak: 0,
        }
    }

    /// Whether the pacer has ever hit the chain-depth ceiling.
    /// Non-zero is the alert condition for the doctor surface.
    #[must_use]
    pub const fn has_hit_chain_ceiling(&self) -> bool {
        self.skipped_chain_ceiling_total > 0
    }
}

/// Render a slice of events as JSONL.
#[must_use]
pub fn render_events_jsonl(events: &[FramePacingEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = serde_json::to_string(ev).expect("FramePacingEvent always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse a JSONL string back into events.
pub fn parse_events_jsonl(jsonl: &str) -> Result<Vec<FramePacingEvent>, serde_json::Error> {
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
    fn idle_state_paints() {
        let s = FramePacingState::idle();
        assert_eq!(decide(s), FramePacingDecision::Paint);
    }

    #[test]
    fn pending_callback_skips() {
        let s = FramePacingState {
            pending_callback: true,
            chain_depth: 1,
            invalidated: false,
        };
        assert_eq!(decide(s), FramePacingDecision::Skip);
    }

    #[test]
    fn chain_at_ceiling_skips_with_chain_ceiling() {
        let s = FramePacingState {
            pending_callback: true,
            chain_depth: MAX_CHAIN_DEPTH,
            invalidated: false,
        };
        assert_eq!(decide(s), FramePacingDecision::SkipChainCeiling);
    }

    #[test]
    fn chain_above_ceiling_skips_with_chain_ceiling() {
        let s = FramePacingState {
            pending_callback: true,
            chain_depth: MAX_CHAIN_DEPTH + 5,
            invalidated: false,
        };
        assert_eq!(decide(s), FramePacingDecision::SkipChainCeiling);
    }

    #[test]
    fn chain_ceiling_fires_even_without_pending_callback() {
        // Defensive — if some path clears `pending_callback`
        // without decrementing chain, the ceiling still guards.
        let s = FramePacingState {
            pending_callback: false,
            chain_depth: MAX_CHAIN_DEPTH,
            invalidated: false,
        };
        assert_eq!(decide(s), FramePacingDecision::SkipChainCeiling);
    }

    #[test]
    fn pacer_request_paint_idle_paints() {
        let mut p = FramePacer::new();
        assert_eq!(p.request_paint(), FramePacingDecision::Paint);
        let s = p.state();
        assert!(s.pending_callback);
        assert_eq!(s.chain_depth, 1);
        assert_eq!(p.health().paints_total, 1);
    }

    #[test]
    fn pacer_second_request_skips_and_invalidates() {
        let mut p = FramePacer::new();
        p.request_paint(); // Paint
        let dec = p.request_paint(); // Skip
        assert_eq!(dec, FramePacingDecision::Skip);
        assert!(p.state().invalidated);
        assert_eq!(p.health().skipped_pending_callback_total, 1);
        assert_eq!(p.health().coalesced_total, 0);
    }

    #[test]
    fn pacer_third_request_coalesces() {
        let mut p = FramePacer::new();
        p.request_paint(); // Paint
        p.request_paint(); // Skip; sets invalidated
        p.request_paint(); // Skip; sees invalidated already → coalesces
        assert_eq!(p.health().skipped_pending_callback_total, 2);
        assert_eq!(p.health().coalesced_total, 1);
    }

    #[test]
    fn pacer_callback_fired_clears_pending_and_returns_deferred_when_invalidated() {
        let mut p = FramePacer::new();
        p.request_paint(); // chain=1, pending=true
        p.request_paint(); // Skip; invalidated=true
        let deferred = p.mark_callback_fired();
        assert!(deferred);
        // After the deferred-paint signal, invalidated is consumed.
        assert!(!p.state().invalidated);
        assert!(!p.state().pending_callback);
        assert_eq!(p.state().chain_depth, 0);
    }

    #[test]
    fn pacer_callback_fired_does_not_signal_when_no_invalidation() {
        let mut p = FramePacer::new();
        p.request_paint();
        let deferred = p.mark_callback_fired();
        assert!(!deferred);
    }

    #[test]
    fn pacer_chain_depth_never_exceeds_max_under_realistic_pattern() {
        // Realistic pattern: paint → callback → paint → callback → …
        // (the steady-state path). Chain depth must always stay 1
        // at peak.
        let mut p = FramePacer::new();
        for _ in 0..100 {
            assert_eq!(p.request_paint(), FramePacingDecision::Paint);
            p.mark_callback_fired();
        }
        assert_eq!(p.health().chain_depth_peak, 1);
    }

    #[test]
    fn pacer_chain_ceiling_fires_under_pathological_pattern() {
        // Pathological: caller bypasses the ceiling guard via
        // direct state mutation. Pacer's ceiling guard catches it.
        let mut p = FramePacer::new();
        // Force chain_depth above the ceiling (test-only — real
        // callers can't reach here through public API).
        p.state.pending_callback = true;
        p.state.chain_depth = MAX_CHAIN_DEPTH;
        let dec = p.request_paint();
        assert_eq!(dec, FramePacingDecision::SkipChainCeiling);
        assert_eq!(p.health().skipped_chain_ceiling_total, 1);
        assert!(p.health().has_hit_chain_ceiling());
    }

    #[test]
    fn pacer_resize_storm_burst_eventually_recovers() {
        // Resize-storm: 50 paint requests in rapid succession,
        // then 50 callback fires drain. After the storm,
        // chain_depth must be 0 again.
        let mut p = FramePacer::new();
        for _ in 0..50 {
            p.request_paint();
        }
        for _ in 0..50 {
            p.mark_callback_fired();
        }
        assert_eq!(p.state().chain_depth, 0);
        assert!(!p.state().pending_callback);
    }

    #[test]
    fn jsonl_roundtrip() {
        let events = vec![
            FramePacingEvent {
                ts_ms: 0,
                frame_callback_pending: false,
                chain_depth: 0,
                decision: FramePacingDecision::Paint,
                compositor: Some("sway".to_string()),
            },
            FramePacingEvent {
                ts_ms: 16,
                frame_callback_pending: true,
                chain_depth: 1,
                decision: FramePacingDecision::Skip,
                compositor: Some("sway".to_string()),
            },
            FramePacingEvent {
                ts_ms: 32,
                frame_callback_pending: true,
                chain_depth: 3,
                decision: FramePacingDecision::SkipChainCeiling,
                compositor: None,
            },
        ];
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).unwrap();
        assert_eq!(parsed, events);
    }

    #[test]
    fn baseline_health_has_no_ceiling_hits() {
        let h = FramePacingHealth::baseline();
        assert!(!h.has_hit_chain_ceiling());
    }
}
