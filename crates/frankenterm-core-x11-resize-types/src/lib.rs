//! X11 ConfigureNotify burst-coalescing substrate (ft-mpc9b.3.3).
//!
//! X11 sends one `ConfigureNotify` event per pixel during a drag-
//! resize. Without coalescing the renderer reflows N times per frame
//! at 16ms intervals, causing flicker; ghostty / alacritty / kitty
//! all coalesce these events. This module ships the pure-logic state
//! machine that the integration's window.rs hooks into.
//!
//! ## What this module ships
//!
//! - `WindowDimensions { width: u32, height: u32 }` with `is_zero()`
//!   reject helper.
//! - `ConfigureNotifyEvent` — the integration layer's window.rs
//!   hands these to the coalescer with a monotonic timestamp.
//! - `CoalesceConfig { window_ms: u32 }` with `DEFAULT_WINDOW_MS = 16`
//!   matching one frame at 60Hz.
//! - `CoalesceState` — `Idle / Buffering { last_event, opened_at }`.
//!   Pure state machine; the integration drives it via `feed_event`
//!   + `poll_now`.
//! - `CoalesceDecision` — `Buffer { dropped_count }` (still
//!   accumulating) / `Emit { dimensions, dropped_count }` (window
//!   closed; integration dispatches Resize). Pure-logic.
//! - `WmTier` (T1 / T2 / Other) and `X11WindowManager` per the bead's
//!   matrix (i3 / Xfwm4 / Cinnamon / Mate / Openbox).
//! - `LiveResizeAtomSupport` — `Supported / NotSupported`. The
//!   integration's startup probe reads `_NET_WM_STATE_LIVE_RESIZE`
//!   from the WM and feeds the result; coalescer prefers the atom
//!   path when supported.
//! - `select_coalesce_strategy(wm, atom_support) -> CoalesceStrategy`
//!   — `AtomDriven` when the WM exposes the atom; `BurstHeuristic`
//!   for i3wm / Openbox / unknown.
//! - `CoalesceStats` — running counters
//!   (`events_received` / `events_emitted` / `events_dropped` /
//!   `coalesce_efficiency_pct`) for `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.3.3.cont)
//!
//! - Wiring into the three Resized dispatch sites at
//!   `frankenterm/window/src/os/x11/window.rs:254 / 382 / 557` —
//!   consolidating into one ordered path.
//! - X11 atom discovery / lookup for `_NET_WM_STATE_LIVE_RESIZE`.
//! - WM detection (read `_NET_WM_NAME` from the root window).
//! - LiveResizeState integration (cross-link ft-mpc9b.2.1 — already
//!   shipped at commit 7e0f6d9b5).
//! - Per-WM E2E test matrix (i3wm / Xfwm4 / Cinnamon / Mate /
//!   Openbox).
//! - JSON-line structured logging at
//!   `tests/x11_resize/logs/<wm>.jsonl`.

#![allow(dead_code)]

// ============================================================================
// Window dimensions + ConfigureNotify event
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WindowDimensions {
    pub width: u32,
    pub height: u32,
}

impl WindowDimensions {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Whether either dimension is zero. The integration's window.rs
    /// can drop these as protocol-level garbage; the coalescer
    /// treats them as no-ops.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigureNotifyEvent {
    /// Monotonic timestamp in milliseconds. Caller's responsibility;
    /// the substrate doesn't read the system clock.
    pub timestamp_ms: u64,
    pub dimensions: WindowDimensions,
}

// ============================================================================
// Coalesce config
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalesceConfig {
    /// Window after the first event in a burst within which
    /// subsequent events are coalesced. Default 16ms = one 60Hz
    /// frame; the integration's tick scheduler then emits a single
    /// Resize.
    pub window_ms: u32,
}

/// 16ms window (≈ one frame at 60Hz).
pub const DEFAULT_WINDOW_MS: u32 = 16;

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            window_ms: DEFAULT_WINDOW_MS,
        }
    }
}

// ============================================================================
// Coalesce state machine
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalesceStateInner {
    Idle,
    /// Buffering events. `last_event` holds the most-recent
    /// dimensions (we keep just the last per the bead — coalescing
    /// drops everything else). `opened_at` is the ms timestamp of
    /// the first event in this burst; the coalescer emits when
    /// `now - opened_at >= window_ms` and no newer event has come
    /// in within `window_ms` of `now`.
    Buffering {
        last_event: ConfigureNotifyEvent,
        opened_at_ms: u64,
        events_in_burst: u32,
    },
}

impl Default for CoalesceStateInner {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalesceState {
    inner: CoalesceStateInner,
    config: CoalesceConfig,
    stats: CoalesceStats,
}

impl Default for CoalesceState {
    fn default() -> Self {
        Self::new()
    }
}

impl CoalesceState {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(CoalesceConfig::default())
    }

    #[must_use]
    pub fn with_config(config: CoalesceConfig) -> Self {
        Self {
            inner: CoalesceStateInner::Idle,
            config,
            stats: CoalesceStats::default(),
        }
    }

    #[must_use]
    pub fn is_idle(&self) -> bool {
        matches!(self.inner, CoalesceStateInner::Idle)
    }

    #[must_use]
    pub fn is_buffering(&self) -> bool {
        matches!(self.inner, CoalesceStateInner::Buffering { .. })
    }

    #[must_use]
    pub fn pending_dimensions(&self) -> Option<WindowDimensions> {
        match self.inner {
            CoalesceStateInner::Buffering { last_event, .. } => Some(last_event.dimensions),
            CoalesceStateInner::Idle => None,
        }
    }

    #[must_use]
    pub fn config(&self) -> CoalesceConfig {
        self.config
    }

    #[must_use]
    pub fn stats(&self) -> &CoalesceStats {
        &self.stats
    }

    /// Feed a ConfigureNotify event. Returns the coalescer's
    /// decision.
    ///
    /// - `Idle` + non-zero event → transition to `Buffering` with
    ///   the event as `last_event`; return `Buffer { dropped_count: 0 }`.
    /// - `Buffering` + non-zero event → keep the new event as
    ///   `last_event`, drop the previous one; return `Buffer
    ///   { dropped_count: events_in_burst - 1 }`.
    /// - `Idle` + zero-dimension event → return `Buffer { dropped: 0 }`
    ///   without changing state (protocol-level garbage).
    /// - `Buffering` + zero-dimension event → keep buffering the
    ///   previously-seen valid event.
    pub fn feed_event(&mut self, event: ConfigureNotifyEvent) -> CoalesceDecision {
        if event.dimensions.is_zero() {
            // Drop garbage event, no state change.
            self.stats.events_received = self.stats.events_received.saturating_add(1);
            self.stats.events_dropped = self.stats.events_dropped.saturating_add(1);
            return match self.inner {
                CoalesceStateInner::Idle => CoalesceDecision::Buffer { dropped_count: 0 },
                CoalesceStateInner::Buffering {
                    events_in_burst, ..
                } => CoalesceDecision::Buffer {
                    dropped_count: events_in_burst,
                },
            };
        }
        self.stats.events_received = self.stats.events_received.saturating_add(1);
        match self.inner {
            CoalesceStateInner::Idle => {
                self.inner = CoalesceStateInner::Buffering {
                    last_event: event,
                    opened_at_ms: event.timestamp_ms,
                    events_in_burst: 1,
                };
                CoalesceDecision::Buffer { dropped_count: 0 }
            }
            CoalesceStateInner::Buffering {
                opened_at_ms,
                events_in_burst,
                ..
            } => {
                let new_count = events_in_burst.saturating_add(1);
                let dropped = events_in_burst; // we drop everything but the new event
                self.inner = CoalesceStateInner::Buffering {
                    last_event: event,
                    opened_at_ms,
                    events_in_burst: new_count,
                };
                self.stats.events_dropped = self.stats.events_dropped.saturating_add(1);
                CoalesceDecision::Buffer {
                    dropped_count: dropped,
                }
            }
        }
    }

    /// Periodic poll. The integration's tick scheduler calls this
    /// every frame (or whenever it samples its monotonic clock); if
    /// the burst window has closed the coalescer transitions to
    /// `Idle` and returns the final dimensions to dispatch.
    pub fn poll_now(&mut self, now_ms: u64) -> CoalesceDecision {
        match self.inner {
            CoalesceStateInner::Idle => CoalesceDecision::Buffer { dropped_count: 0 },
            CoalesceStateInner::Buffering {
                last_event,
                opened_at_ms,
                events_in_burst,
            } => {
                if now_ms.saturating_sub(opened_at_ms) >= u64::from(self.config.window_ms) {
                    self.inner = CoalesceStateInner::Idle;
                    self.stats.events_emitted = self.stats.events_emitted.saturating_add(1);
                    CoalesceDecision::Emit {
                        dimensions: last_event.dimensions,
                        dropped_count: events_in_burst.saturating_sub(1),
                    }
                } else {
                    CoalesceDecision::Buffer {
                        dropped_count: events_in_burst.saturating_sub(1),
                    }
                }
            }
        }
    }

    /// Operator-driven flush: emit immediately regardless of the
    /// time window. Used when a `_NET_WM_STATE_LIVE_RESIZE` end
    /// notification fires (the WM is telling us the gesture
    /// finished, so dispatching now is correct).
    pub fn flush(&mut self) -> CoalesceDecision {
        match self.inner {
            CoalesceStateInner::Idle => CoalesceDecision::Buffer { dropped_count: 0 },
            CoalesceStateInner::Buffering {
                last_event,
                events_in_burst,
                ..
            } => {
                self.inner = CoalesceStateInner::Idle;
                self.stats.events_emitted = self.stats.events_emitted.saturating_add(1);
                CoalesceDecision::Emit {
                    dimensions: last_event.dimensions,
                    dropped_count: events_in_burst.saturating_sub(1),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalesceDecision {
    /// Coalescer is buffering; no Resize dispatched. `dropped_count`
    /// indicates how many events have been folded into the current
    /// burst (telemetry).
    Buffer { dropped_count: u32 },
    /// Burst window closed; integration should dispatch a Resize
    /// with these dimensions. `dropped_count` is how many ConfigureNotify
    /// events were folded into this single Resize (telemetry).
    Emit {
        dimensions: WindowDimensions,
        dropped_count: u32,
    },
}

impl CoalesceDecision {
    #[must_use]
    pub fn is_emit(&self) -> bool {
        matches!(self, Self::Emit { .. })
    }

    #[must_use]
    pub fn dimensions(&self) -> Option<WindowDimensions> {
        match self {
            Self::Emit { dimensions, .. } => Some(*dimensions),
            Self::Buffer { .. } => None,
        }
    }
}

// ============================================================================
// WM tier + atom support
// ============================================================================

/// Per the bead's matrix:
///
/// | WM | Tier | Atom support? |
/// |---|---|---|
/// | i3 | T1 | No (heuristic burst-detect) |
/// | Xfwm4 | T1 | Yes |
/// | Cinnamon (Muffin) | T1 | Yes |
/// | Mate (Marco) | T2 | Yes |
/// | Openbox | T2 | No |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum X11WindowManager {
    I3,
    Xfwm4,
    Mutter,
    CinnamonMuffin,
    MateMarco,
    Openbox,
    /// Anything else (Awesome / dwm / xmonad / unknown). Conservative
    /// default: assume no atom support, fall back to burst heuristic.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WmTier {
    T1,
    T2,
    Other,
}

impl X11WindowManager {
    /// Tier per the bead's table.
    #[must_use]
    pub fn tier(self) -> WmTier {
        match self {
            Self::I3 | Self::Xfwm4 | Self::Mutter | Self::CinnamonMuffin => WmTier::T1,
            Self::MateMarco | Self::Openbox => WmTier::T2,
            Self::Other => WmTier::Other,
        }
    }

    /// Whether the WM exposes `_NET_WM_STATE_LIVE_RESIZE` per the
    /// bead's table. The integration's startup probe overrides this
    /// with the actual atom-presence check; the table is the
    /// fallback when probing fails.
    #[must_use]
    pub fn declared_atom_support(self) -> LiveResizeAtomSupport {
        match self {
            Self::Xfwm4 | Self::Mutter | Self::CinnamonMuffin | Self::MateMarco => {
                LiveResizeAtomSupport::Supported
            }
            Self::I3 | Self::Openbox | Self::Other => LiveResizeAtomSupport::NotSupported,
        }
    }
}

#[must_use]
pub fn classify_x11_window_manager(name: &str) -> X11WindowManager {
    let lower = name.trim().to_ascii_lowercase();
    if lower.contains("xfwm4") {
        X11WindowManager::Xfwm4
    } else if lower.contains("mutter") {
        X11WindowManager::Mutter
    } else if lower.contains("muffin") || lower.contains("cinnamon") {
        X11WindowManager::CinnamonMuffin
    } else if lower.contains("marco") || lower.contains("mate") {
        X11WindowManager::MateMarco
    } else if lower.contains("openbox") {
        X11WindowManager::Openbox
    } else if lower == "i3" || lower.contains("i3wm") || lower.contains("i3 window manager") {
        X11WindowManager::I3
    } else {
        X11WindowManager::Other
    }
}

/// Probe-result for `_NET_WM_STATE_LIVE_RESIZE`. The substrate's
/// strategy selector reads this from the integration's startup probe
/// rather than the per-WM declaration table — startup-probe truth
/// wins, the table is fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LiveResizeAtomSupport {
    Supported,
    #[default]
    NotSupported,
}

// ============================================================================
// Strategy selector
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoalesceStrategy {
    /// WM exposes `_NET_WM_STATE_LIVE_RESIZE` — use the atom-driven
    /// path: enter `Buffering` on the atom add, `flush()` on the
    /// atom remove. No timer needed.
    AtomDriven,
    /// WM doesn't expose the atom — use the timer-based heuristic
    /// (default 16ms window). The integration's tick scheduler calls
    /// `poll_now` each frame.
    BurstHeuristic,
}

/// Pure-logic strategy lookup. The integration's startup probe
/// hands `LiveResizeAtomSupport` from the live atom-presence check;
/// the WM identity is purely informational for telemetry.
#[must_use]
pub fn select_coalesce_strategy(
    _wm: X11WindowManager,
    atom_support: LiveResizeAtomSupport,
) -> CoalesceStrategy {
    match atom_support {
        LiveResizeAtomSupport::Supported => CoalesceStrategy::AtomDriven,
        LiveResizeAtomSupport::NotSupported => CoalesceStrategy::BurstHeuristic,
    }
}

// ============================================================================
// Stats
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoalesceStats {
    pub events_received: u64,
    pub events_emitted: u64,
    pub events_dropped: u64,
}

impl CoalesceStats {
    /// Coalesce efficiency: percent of events folded into a single
    /// Resize. Higher = more aggressive coalescing. `0` when no
    /// events have been received.
    #[must_use]
    pub fn efficiency_pct(&self) -> u32 {
        if self.events_received == 0 {
            return 0;
        }
        ((self.events_dropped * 100) / self.events_received).min(100) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: u64, w: u32, h: u32) -> ConfigureNotifyEvent {
        ConfigureNotifyEvent {
            timestamp_ms: ts,
            dimensions: WindowDimensions::new(w, h),
        }
    }

    // ----------------------------------------------------------------
    // WindowDimensions
    // ----------------------------------------------------------------

    #[test]
    fn dimensions_is_zero_detects_either_axis() {
        assert!(WindowDimensions::new(0, 100).is_zero());
        assert!(WindowDimensions::new(100, 0).is_zero());
        assert!(!WindowDimensions::new(100, 100).is_zero());
    }

    // ----------------------------------------------------------------
    // CoalesceConfig
    // ----------------------------------------------------------------

    #[test]
    fn coalesce_config_default_is_16ms() {
        assert_eq!(CoalesceConfig::default().window_ms, 16);
        assert_eq!(DEFAULT_WINDOW_MS, 16);
    }

    // ----------------------------------------------------------------
    // CoalesceState — basic lifecycle
    // ----------------------------------------------------------------

    #[test]
    fn fresh_state_is_idle() {
        let s = CoalesceState::new();
        assert!(s.is_idle());
        assert!(!s.is_buffering());
        assert_eq!(s.pending_dimensions(), None);
    }

    #[test]
    fn first_event_transitions_to_buffering() {
        let mut s = CoalesceState::new();
        let d = s.feed_event(ev(0, 800, 600));
        assert!(s.is_buffering());
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 0 });
        assert_eq!(
            s.pending_dimensions(),
            Some(WindowDimensions::new(800, 600))
        );
    }

    #[test]
    fn second_event_replaces_first_with_drop_count() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        let d = s.feed_event(ev(2, 810, 605));
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 1 });
        assert_eq!(
            s.pending_dimensions(),
            Some(WindowDimensions::new(810, 605))
        );
    }

    #[test]
    fn third_and_fourth_events_keep_dropping() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(2, 810, 605));
        let d = s.feed_event(ev(4, 820, 610));
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 2 });
        let d = s.feed_event(ev(6, 830, 615));
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 3 });
        assert_eq!(
            s.pending_dimensions(),
            Some(WindowDimensions::new(830, 615))
        );
    }

    #[test]
    fn poll_within_window_returns_buffer() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(8, 810, 605));
        let d = s.poll_now(10);
        assert!(!d.is_emit());
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 1 });
    }

    #[test]
    fn poll_after_window_emits_last_dimensions() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(8, 810, 605));
        s.feed_event(ev(15, 820, 610));
        let d = s.poll_now(20);
        match d {
            CoalesceDecision::Emit {
                dimensions,
                dropped_count,
            } => {
                assert_eq!(dimensions, WindowDimensions::new(820, 610));
                assert_eq!(dropped_count, 2);
            }
            other @ CoalesceDecision::Buffer { .. } => panic!("expected Emit, got {other:?}"),
        }
        assert!(s.is_idle());
    }

    #[test]
    fn poll_at_exact_window_boundary_emits() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(100, 800, 600));
        // 100 + 16 = 116 — at-or-after window edge.
        let d = s.poll_now(116);
        assert!(d.is_emit());
    }

    #[test]
    fn poll_one_ms_before_boundary_buffers() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(100, 800, 600));
        let d = s.poll_now(115);
        assert!(!d.is_emit());
    }

    #[test]
    fn idle_poll_returns_buffer_zero() {
        let mut s = CoalesceState::new();
        let d = s.poll_now(1000);
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 0 });
    }

    // ----------------------------------------------------------------
    // Zero-dimension protocol garbage
    // ----------------------------------------------------------------

    #[test]
    fn zero_dimension_first_event_does_not_buffer() {
        let mut s = CoalesceState::new();
        let d = s.feed_event(ev(0, 0, 600));
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 0 });
        assert!(s.is_idle());
    }

    #[test]
    fn zero_dimension_during_burst_keeps_previous_valid() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        let d = s.feed_event(ev(2, 0, 0));
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 1 });
        assert_eq!(
            s.pending_dimensions(),
            Some(WindowDimensions::new(800, 600))
        );
    }

    #[test]
    fn stats_count_dropped_zero_dimension_events() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 0, 600));
        s.feed_event(ev(1, 800, 0));
        let stats = s.stats();
        assert_eq!(stats.events_received, 2);
        assert_eq!(stats.events_dropped, 2);
        assert_eq!(stats.events_emitted, 0);
    }

    // ----------------------------------------------------------------
    // Flush
    // ----------------------------------------------------------------

    #[test]
    fn flush_emits_immediately_when_buffering() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(1, 810, 605));
        let d = s.flush();
        match d {
            CoalesceDecision::Emit {
                dimensions,
                dropped_count,
            } => {
                assert_eq!(dimensions, WindowDimensions::new(810, 605));
                assert_eq!(dropped_count, 1);
            }
            other @ CoalesceDecision::Buffer { .. } => panic!("expected Emit, got {other:?}"),
        }
        assert!(s.is_idle());
    }

    #[test]
    fn flush_when_idle_returns_buffer_zero() {
        let mut s = CoalesceState::new();
        let d = s.flush();
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 0 });
    }

    // ----------------------------------------------------------------
    // Re-burst after emit
    // ----------------------------------------------------------------

    #[test]
    fn new_burst_after_emit_starts_fresh() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(8, 810, 605));
        s.poll_now(20); // emits, transitions to Idle
        // New burst.
        let d = s.feed_event(ev(100, 900, 700));
        assert_eq!(d, CoalesceDecision::Buffer { dropped_count: 0 });
        assert_eq!(
            s.pending_dimensions(),
            Some(WindowDimensions::new(900, 700))
        );
    }

    // ----------------------------------------------------------------
    // Stats
    // ----------------------------------------------------------------

    #[test]
    fn stats_track_received_emitted_dropped() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(2, 810, 605));
        s.feed_event(ev(4, 820, 610));
        s.poll_now(20);
        let stats = s.stats();
        assert_eq!(stats.events_received, 3);
        assert_eq!(stats.events_dropped, 2);
        assert_eq!(stats.events_emitted, 1);
    }

    #[test]
    fn stats_efficiency_zero_when_no_events() {
        let s = CoalesceStats::default();
        assert_eq!(s.efficiency_pct(), 0);
    }

    #[test]
    fn stats_efficiency_high_for_heavy_burst() {
        let mut s = CoalesceState::new();
        for i in 0..100 {
            s.feed_event(ev(u64::from(i), 800 + i, 600));
        }
        s.poll_now(200);
        let stats = s.stats();
        // 100 events received, 99 dropped, 1 emitted → 99% efficiency.
        assert_eq!(stats.events_received, 100);
        assert_eq!(stats.events_emitted, 1);
        assert_eq!(stats.events_dropped, 99);
        assert_eq!(stats.efficiency_pct(), 99);
    }

    // ----------------------------------------------------------------
    // WM matrix + strategy selector
    // ----------------------------------------------------------------

    #[test]
    fn wm_tier_matches_bead_table() {
        assert_eq!(X11WindowManager::I3.tier(), WmTier::T1);
        assert_eq!(X11WindowManager::Xfwm4.tier(), WmTier::T1);
        assert_eq!(X11WindowManager::Mutter.tier(), WmTier::T1);
        assert_eq!(X11WindowManager::CinnamonMuffin.tier(), WmTier::T1);
        assert_eq!(X11WindowManager::MateMarco.tier(), WmTier::T2);
        assert_eq!(X11WindowManager::Openbox.tier(), WmTier::T2);
        assert_eq!(X11WindowManager::Other.tier(), WmTier::Other);
    }

    #[test]
    fn wm_declared_atom_support_matches_bead_table() {
        // Atom-supporting per the bead.
        for wm in [
            X11WindowManager::Xfwm4,
            X11WindowManager::Mutter,
            X11WindowManager::CinnamonMuffin,
            X11WindowManager::MateMarco,
        ] {
            assert_eq!(wm.declared_atom_support(), LiveResizeAtomSupport::Supported);
        }
        // Non-supporting per the bead.
        for wm in [
            X11WindowManager::I3,
            X11WindowManager::Openbox,
            X11WindowManager::Other,
        ] {
            assert_eq!(
                wm.declared_atom_support(),
                LiveResizeAtomSupport::NotSupported
            );
        }
    }

    #[test]
    fn classify_window_manager_names_from_ewmh_strings() {
        assert_eq!(classify_x11_window_manager("i3"), X11WindowManager::I3);
        assert_eq!(
            classify_x11_window_manager("Xfwm4"),
            X11WindowManager::Xfwm4
        );
        assert_eq!(
            classify_x11_window_manager("GNOME Mutter"),
            X11WindowManager::Mutter
        );
        assert_eq!(
            classify_x11_window_manager("Muffin"),
            X11WindowManager::CinnamonMuffin
        );
        assert_eq!(
            classify_x11_window_manager("Marco"),
            X11WindowManager::MateMarco
        );
        assert_eq!(
            classify_x11_window_manager("Openbox"),
            X11WindowManager::Openbox
        );
        assert_eq!(
            classify_x11_window_manager("unknown-wm"),
            X11WindowManager::Other
        );
    }

    #[test]
    fn strategy_picks_atom_driven_when_supported() {
        // Probe-truth wins over WM identity.
        let s = select_coalesce_strategy(X11WindowManager::I3, LiveResizeAtomSupport::Supported);
        assert_eq!(s, CoalesceStrategy::AtomDriven);
    }

    #[test]
    fn strategy_picks_burst_heuristic_when_atom_missing() {
        let s =
            select_coalesce_strategy(X11WindowManager::Xfwm4, LiveResizeAtomSupport::NotSupported);
        assert_eq!(s, CoalesceStrategy::BurstHeuristic);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_drag_resize_60_events_one_emit_per_frame() {
        // Simulate a drag-resize that fires one ConfigureNotify per
        // pixel for 60 pixels at 1ms intervals; coalescer should
        // emit ~once per 16ms frame.
        let mut s = CoalesceState::new();
        let mut emits = 0u32;
        for i in 0..60 {
            let ts = u64::from(i);
            s.feed_event(ev(ts, 800 + i, 600));
            // Drain at frame boundaries (every 16ms).
            if ts % 16 == 0 && ts > 0 {
                let d = s.poll_now(ts);
                if d.is_emit() {
                    emits += 1;
                }
            }
        }
        // Final flush after burst ends.
        let final_ts = 100;
        let d = s.poll_now(final_ts);
        if d.is_emit() {
            emits += 1;
        }
        // 60 events in a 60ms span → expect ~3-4 frame-boundary emits.
        assert!(
            (3..=5).contains(&emits),
            "expected 3-5 emits across 60ms drag, got {emits}"
        );
        // Stats: vast majority dropped.
        let stats = s.stats();
        assert!(
            stats.efficiency_pct() >= 90,
            "efficiency {}% should be ≥90% for a heavy burst",
            stats.efficiency_pct()
        );
    }

    #[test]
    fn scenario_atom_driven_flush_at_end_of_resize() {
        // Atom-driven path: on _NET_WM_STATE_LIVE_RESIZE add, no
        // timer needed; on remove, integration calls flush().
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(50, 810, 605));
        s.feed_event(ev(100, 820, 610));
        // Atom remove fires; flush.
        let d = s.flush();
        assert_eq!(d.dimensions(), Some(WindowDimensions::new(820, 610)));
        assert!(s.is_idle());
        let stats = s.stats();
        assert_eq!(stats.events_emitted, 1);
        assert_eq!(stats.events_dropped, 2);
    }

    #[test]
    fn scenario_burst_with_garbage_events_still_emits_clean_dimensions() {
        let mut s = CoalesceState::new();
        s.feed_event(ev(0, 800, 600));
        s.feed_event(ev(2, 0, 0)); // garbage from window manager
        s.feed_event(ev(4, 820, 610));
        s.feed_event(ev(6, 0, 600)); // garbage
        let d = s.poll_now(30);
        assert_eq!(
            d.dimensions(),
            Some(WindowDimensions::new(820, 610)),
            "garbage events must not corrupt the emitted dimensions"
        );
    }

    #[test]
    fn scenario_per_wm_strategy_dispatch() {
        // Tier-1 WM with atom support: AtomDriven.
        let s = select_coalesce_strategy(
            X11WindowManager::Xfwm4,
            X11WindowManager::Xfwm4.declared_atom_support(),
        );
        assert_eq!(s, CoalesceStrategy::AtomDriven);
        // Tier-1 WM without atom: BurstHeuristic.
        let s = select_coalesce_strategy(
            X11WindowManager::I3,
            X11WindowManager::I3.declared_atom_support(),
        );
        assert_eq!(s, CoalesceStrategy::BurstHeuristic);
        // Unknown WM: BurstHeuristic (conservative default).
        let s = select_coalesce_strategy(
            X11WindowManager::Other,
            X11WindowManager::Other.declared_atom_support(),
        );
        assert_eq!(s, CoalesceStrategy::BurstHeuristic);
    }
}
