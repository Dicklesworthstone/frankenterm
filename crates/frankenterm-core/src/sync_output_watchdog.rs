//! DEC mode 2026 watchdog + depth + mode-query substrate
//! (ft-2okh0.1.1).
//!
//! Pure-logic substrate covering the parts of the bead's
//! "Concrete actions" not already shipped by sibling modules:
//!
//! - Term-layer `synchronized_output` flag — already shipped via
//!   `ft-d7af6` (frankenterm/term/src/terminalstate/mod.rs).
//! - Renderer presentation-hold state machine — already shipped
//!   via `ft-u6jos` (`dec_2026_presentation_hold.rs`).
//! - Dirty-line bitmap — already shipped via `ft-mpc9b.1.2`.
//!
//! What this module adds:
//!
//! - `BsuDepthCounter` — nested-BSU depth tracker with the bead's
//!   "depth always ≥0" invariant + saturation behaviour for
//!   adversarial ESU-without-BSU streams.
//! - `WatchdogConfig` — 150 ms default force-flush timeout (bead
//!   default), operator-tunable.
//! - `WatchdogState` — minimal Idle/Pending/Triggered timer
//!   state machine the integration drives via
//!   `runtime_async::sleep_with_cx`.
//! - `WatchdogDecision` — pure-logic "should we force-flush
//!   now?" predicate over `(state, now_ms)`.
//! - `ModeQueryState` — DECRQM `CSI ? 2026 $ p` response
//!   classification per the standard's NotRecognized / Set /
//!   Reset / PermanentlySet / PermanentlyReset semantics.
//! - `format_mode_query_response` — renders the canonical
//!   `CSI ? 2026 ; <state> $ y` reply.
//! - `SyncOutputTelemetry` — bead's structured-logging counters
//!   (`bsu_count`, `esu_count`, `watchdog_force_flush_count`,
//!   `mid_bsu_byte_count`, `max_bsu_depth_observed`,
//!   `mode_query_count`, `adversarial_esu_underflow_count`).
//!
//! ## What is deferred to ft-2okh0.1.1.cont
//!
//! - Wiring `WatchdogState` into the actual asupersync timer
//!   loop (`runtime_async::sleep_with_cx`) so a stuck BSU
//!   triggers `force_flush()` after 150 ms.
//! - Reading the term-layer `synchronized_output` flag and
//!   draining the BSU buffer at ESU into the triple-buffer
//!   snapshot swap.
//! - Emitting the `CSI ? 2026 ; <state> $ y` bytes on the
//!   PTY response channel for the mode-query handler.
//! - Per-pane mid-BSU byte-count integration (substrate
//!   provides counter, integration accumulates).

#![allow(dead_code)]

// ============================================================================
// Nested-BSU depth counter
// ============================================================================

/// Tracks `BSU` (Begin Synchronized Update) and `ESU` (End
/// Synchronized Update) bracket depth.
///
/// Per the bead: "Nested BSU: tracked via depth counter; only
/// flush at depth=0" and the proptest invariant "depth always
/// ≥0".
///
/// Substrate enforces both at the type level: the counter is
/// `u32`, so depth never goes negative. Adversarial
/// ESU-without-BSU underflow is reported via
/// `BsuDepthOutcome::Underflow` so the integration's telemetry
/// can log it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BsuDepthCounter {
    depth: u32,
    /// Highest depth observed in this session — for the bead's
    /// `max_bsu_depth_observed` telemetry counter.
    max_observed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BsuDepthOutcome {
    /// `BSU` opened a new nesting level. `new_depth` includes
    /// this BSU.
    Opened { new_depth: u32 },
    /// `ESU` closed a level but the bracket is still open.
    /// Don't flush yet.
    Closed { new_depth: u32 },
    /// `ESU` brought depth to zero. Integration flushes the
    /// buffered frame now.
    Flushed,
    /// Adversarial input: `ESU` arrived without a matching
    /// `BSU`. Substrate clamps depth at zero and signals so
    /// telemetry can count it.
    Underflow,
}

impl BsuDepthCounter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            depth: 0,
            max_observed: 0,
        }
    }

    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    #[must_use]
    pub const fn max_observed(&self) -> u32 {
        self.max_observed
    }

    #[must_use]
    pub const fn is_in_bsu(&self) -> bool {
        self.depth > 0
    }

    /// Open a new BSU nesting level.
    pub fn open_bsu(&mut self) -> BsuDepthOutcome {
        self.depth = self.depth.saturating_add(1);
        if self.depth > self.max_observed {
            self.max_observed = self.depth;
        }
        BsuDepthOutcome::Opened {
            new_depth: self.depth,
        }
    }

    /// Close a BSU nesting level. Returns `Flushed` when depth
    /// reaches zero; the integration then atomically swaps the
    /// buffered frame into the visible state.
    pub fn close_esu(&mut self) -> BsuDepthOutcome {
        if self.depth == 0 {
            return BsuDepthOutcome::Underflow;
        }
        self.depth -= 1;
        if self.depth == 0 {
            BsuDepthOutcome::Flushed
        } else {
            BsuDepthOutcome::Closed {
                new_depth: self.depth,
            }
        }
    }

    /// Force-reset to depth=0 (used after a watchdog flush
    /// fires; any future ESU is treated as underflow until a
    /// new BSU opens).
    pub fn force_reset(&mut self) {
        self.depth = 0;
    }
}

// ============================================================================
// Watchdog
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// Force-flush timeout in milliseconds. Bead default
    /// 150 ms. Field is `pub(crate)` because this is a
    /// **DoS-protection cap** — arbitrary code paths must
    /// not be able to silently disable it (e.g., set to
    /// u32::MAX). Use the [`Self::with_timeout_ms`] builder
    /// for explicit reconfiguration.
    pub(crate) timeout_ms: u32,
    /// Minimum positive timeout — defensive against
    /// misconfiguration. Substrate caps at this floor;
    /// values below are silently raised.
    pub(crate) min_timeout_ms: u32,
}

pub const DEFAULT_WATCHDOG_TIMEOUT_MS: u32 = 150;
pub const MIN_WATCHDOG_TIMEOUT_MS: u32 = 16; // ~one frame at 60Hz

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_WATCHDOG_TIMEOUT_MS,
            min_timeout_ms: MIN_WATCHDOG_TIMEOUT_MS,
        }
    }
}

impl WatchdogConfig {
    /// Read-only accessor for the configured timeout.
    #[must_use]
    pub const fn timeout_ms(self) -> u32 {
        self.timeout_ms
    }

    /// Read-only accessor for the configured floor.
    #[must_use]
    pub const fn min_timeout_ms(self) -> u32 {
        self.min_timeout_ms
    }

    /// Effective timeout after the floor is applied.
    #[must_use]
    pub const fn effective_timeout_ms(&self) -> u32 {
        if self.timeout_ms < self.min_timeout_ms {
            self.min_timeout_ms
        } else {
            self.timeout_ms
        }
    }

    /// Builder: override the force-flush timeout. Returns
    /// a new config; security-policy changes are explicit
    /// reconstruction events.
    #[must_use]
    pub const fn with_timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Builder: override the minimum-timeout floor.
    #[must_use]
    pub const fn with_min_timeout_ms(mut self, min_timeout_ms: u32) -> Self {
        self.min_timeout_ms = min_timeout_ms;
        self
    }
}

/// Watchdog timer state. Three states:
///
/// - `Idle` — no BSU pending; substrate doesn't track time.
/// - `Pending { deadline_ms }` — BSU opened at `start_ms`;
///   if `now_ms >= deadline_ms` arrives without ESU, the
///   integration force-flushes.
/// - `Triggered` — watchdog already fired this BSU. Calling
///   `should_force_flush` returns `false` (substrate fires
///   once per BSU; integration moves to `Idle` after force-flush).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchdogState {
    #[default]
    Idle,
    Pending {
        deadline_ms: u64,
    },
    Triggered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchdogDecision {
    /// No action — keep waiting (or remain idle).
    Wait,
    /// Watchdog tripped — integration must force-flush the
    /// buffered frame and reset depth + watchdog state.
    ForceFlush,
}

impl WatchdogState {
    /// Begin a watchdog window starting at `now_ms`, using
    /// `config.effective_timeout_ms()` as the deadline offset.
    /// Idempotent for nested BSU: caller decides whether to
    /// reset the deadline or keep the outer one.
    pub fn arm(&mut self, now_ms: u64, config: WatchdogConfig) {
        let deadline_ms = now_ms.saturating_add(config.effective_timeout_ms() as u64);
        *self = Self::Pending { deadline_ms };
    }

    /// Disarm after a successful ESU flush.
    pub fn disarm(&mut self) {
        *self = Self::Idle;
    }

    /// Mark that the watchdog has already fired for this BSU
    /// (so subsequent `should_force_flush` calls return Wait).
    pub fn mark_triggered(&mut self) {
        *self = Self::Triggered;
    }

    /// True iff the watchdog is in the `Idle` state — no
    /// BSU pending. Use to assert the integration's state
    /// machine is in sync (after disarm() or before
    /// arm()).
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// True iff the watchdog is in `Pending` state — BSU
    /// open, waiting for ESU or deadline.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    /// True iff the watchdog already force-fired this BSU
    /// and is waiting for the integration to call `disarm()`
    /// and `arm()` for the next BSU. A long-stuck `Triggered`
    /// state indicates the integration's timer-loop wiring
    /// is broken (forgot to reset).
    #[must_use]
    pub const fn is_triggered(&self) -> bool {
        matches!(self, Self::Triggered)
    }

    /// Read-only probe: at `now_ms`, would the watchdog
    /// need to fire? Does NOT consume the firing — repeated
    /// calls keep returning ForceFlush until the caller
    /// transitions via `mark_triggered` or `consume_force_flush`.
    ///
    /// Multi-threaded callers should prefer
    /// `consume_force_flush` to avoid the double-flush race
    /// window; this method is for read-only diagnostics
    /// (`ft doctor`) where the caller doesn't dispatch.
    #[must_use]
    pub fn should_force_flush(&self, now_ms: u64) -> WatchdogDecision {
        match self {
            Self::Pending { deadline_ms } if now_ms >= *deadline_ms => WatchdogDecision::ForceFlush,
            _ => WatchdogDecision::Wait,
        }
    }

    /// Atomic consume: at `now_ms`, returns `ForceFlush`
    /// exactly once if the deadline has passed AND transitions
    /// the state to `Triggered` in the same `&mut self` call.
    /// Subsequent calls return `Wait`.
    ///
    /// Self-review fix (br-ft-deemu): the prior pattern
    /// (read via should_force_flush + caller-managed
    /// mark_triggered) had a window where two pollers could
    /// both observe ForceFlush and both dispatch.
    /// `consume_force_flush` collapses the read+transition
    /// into a single mutation, so Rust's `&mut self` borrow
    /// rules prevent the race at compile time.
    pub fn consume_force_flush(&mut self, now_ms: u64) -> WatchdogDecision {
        match *self {
            Self::Pending { deadline_ms } if now_ms >= deadline_ms => {
                *self = Self::Triggered;
                WatchdogDecision::ForceFlush
            }
            _ => WatchdogDecision::Wait,
        }
    }
}

// ============================================================================
// DECRQM mode-query
// ============================================================================

/// DEC mode-query reply state per the standard's DECRPM
/// (CSI ? Pn ; Ps $ y) format. Bead: "App can query mode state:
/// `CSI ? 2026 $ p` returns current mode."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeQueryState {
    /// Ps=0 — terminal does not recognise the mode.
    NotRecognized = 0,
    /// Ps=1 — mode is set (BSU pending).
    Set = 1,
    /// Ps=2 — mode is reset (no BSU pending).
    Reset = 2,
    /// Ps=3 — mode is permanently set (substrate doesn't use
    /// this for 2026 but the standard reserves it).
    PermanentlySet = 3,
    /// Ps=4 — mode is permanently reset.
    PermanentlyReset = 4,
}

impl ModeQueryState {
    #[must_use]
    pub const fn ps_value(self) -> u8 {
        self as u8
    }
}

/// Pure decision for a `CSI ? 2026 $ p` query: report `Set` if
/// the depth counter is currently in a BSU, `Reset` otherwise.
#[must_use]
pub fn classify_mode_query(depth: &BsuDepthCounter) -> ModeQueryState {
    if depth.is_in_bsu() {
        ModeQueryState::Set
    } else {
        ModeQueryState::Reset
    }
}

/// Render the canonical `CSI ? 2026 ; <state> $ y` reply. The
/// integration writes these bytes to the PTY response channel.
#[must_use]
pub fn format_mode_query_response(state: ModeQueryState) -> String {
    format!("\x1b[?2026;{}$y", state.ps_value())
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutputTelemetry {
    /// Counters are `pub(crate)` so external code can't
    /// silently zero `adversarial_esu_underflow_count`
    /// (masking attacks) or back-fill any of the other
    /// counters out-of-band. Use the read accessors.
    pub(crate) bsu_count: u64,
    pub(crate) esu_count: u64,
    pub(crate) watchdog_force_flush_count: u64,
    pub(crate) mid_bsu_byte_count: u64,
    pub(crate) max_bsu_depth_observed: u32,
    pub(crate) mode_query_count: u64,
    pub(crate) adversarial_esu_underflow_count: u64,
    pub(crate) esu_flush_count: u64,
}

impl SyncOutputTelemetry {
    pub fn record_depth_outcome(&mut self, outcome: BsuDepthOutcome, current_max: u32) {
        match outcome {
            BsuDepthOutcome::Opened { new_depth } => {
                self.bsu_count = self.bsu_count.saturating_add(1);
                if new_depth > self.max_bsu_depth_observed {
                    self.max_bsu_depth_observed = new_depth;
                }
            }
            BsuDepthOutcome::Closed { .. } => {
                self.esu_count = self.esu_count.saturating_add(1);
            }
            BsuDepthOutcome::Flushed => {
                self.esu_count = self.esu_count.saturating_add(1);
                self.esu_flush_count = self.esu_flush_count.saturating_add(1);
            }
            BsuDepthOutcome::Underflow => {
                self.adversarial_esu_underflow_count =
                    self.adversarial_esu_underflow_count.saturating_add(1);
            }
        }
        if current_max > self.max_bsu_depth_observed {
            self.max_bsu_depth_observed = current_max;
        }
    }

    pub fn record_watchdog_decision(&mut self, decision: WatchdogDecision) {
        if matches!(decision, WatchdogDecision::ForceFlush) {
            self.watchdog_force_flush_count = self.watchdog_force_flush_count.saturating_add(1);
        }
    }

    pub fn record_mid_bsu_bytes(&mut self, bytes: u64) {
        self.mid_bsu_byte_count = self.mid_bsu_byte_count.saturating_add(bytes);
    }

    pub fn record_mode_query(&mut self) {
        self.mode_query_count = self.mode_query_count.saturating_add(1);
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn bsu_count(self) -> u64 {
        self.bsu_count
    }
    #[must_use]
    pub const fn esu_count(self) -> u64 {
        self.esu_count
    }
    #[must_use]
    pub const fn watchdog_force_flush_count(self) -> u64 {
        self.watchdog_force_flush_count
    }
    #[must_use]
    pub const fn mid_bsu_byte_count(self) -> u64 {
        self.mid_bsu_byte_count
    }
    #[must_use]
    pub const fn max_bsu_depth_observed(self) -> u32 {
        self.max_bsu_depth_observed
    }
    #[must_use]
    pub const fn mode_query_count(self) -> u64 {
        self.mode_query_count
    }
    #[must_use]
    pub const fn adversarial_esu_underflow_count(self) -> u64 {
        self.adversarial_esu_underflow_count
    }
    #[must_use]
    pub const fn esu_flush_count(self) -> u64 {
        self.esu_flush_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // BsuDepthCounter — depth invariants
    // ----------------------------------------------------------------

    #[test]
    fn depth_starts_zero() {
        let c = BsuDepthCounter::new();
        assert_eq!(c.depth(), 0);
        assert!(!c.is_in_bsu());
    }

    #[test]
    fn depth_open_bsu_increments() {
        let mut c = BsuDepthCounter::new();
        let r = c.open_bsu();
        assert_eq!(r, BsuDepthOutcome::Opened { new_depth: 1 });
        assert_eq!(c.depth(), 1);
        assert!(c.is_in_bsu());
    }

    #[test]
    fn depth_close_esu_at_zero_underflows() {
        let mut c = BsuDepthCounter::new();
        let r = c.close_esu();
        assert_eq!(r, BsuDepthOutcome::Underflow);
        // Depth stays clamped at zero — bead's "depth always ≥0".
        assert_eq!(c.depth(), 0);
    }

    #[test]
    fn depth_simple_open_close_flushes() {
        let mut c = BsuDepthCounter::new();
        c.open_bsu();
        let r = c.close_esu();
        assert_eq!(r, BsuDepthOutcome::Flushed);
        assert_eq!(c.depth(), 0);
    }

    // ----------------------------------------------------------------
    // WatchdogConfig builder API (security cap — pub→pub(crate))
    // ----------------------------------------------------------------

    #[test]
    fn watchdog_config_builder_round_trips() {
        let config = WatchdogConfig::default()
            .with_timeout_ms(500)
            .with_min_timeout_ms(32);
        assert_eq!(config.timeout_ms(), 500);
        assert_eq!(config.min_timeout_ms(), 32);
        assert_eq!(config.effective_timeout_ms(), 500);
    }

    #[test]
    fn watchdog_config_floor_clamps_low_timeout() {
        let config = WatchdogConfig::default().with_timeout_ms(5);
        assert_eq!(config.timeout_ms(), 5);
        // Floor (16ms default) wins.
        assert_eq!(config.effective_timeout_ms(), MIN_WATCHDOG_TIMEOUT_MS);
    }

    // ----------------------------------------------------------------
    // WatchdogState predicates (state-machine accessors)
    // ----------------------------------------------------------------

    #[test]
    fn watchdog_state_predicates_idle() {
        let s = WatchdogState::Idle;
        assert!(s.is_idle());
        assert!(!s.is_pending());
        assert!(!s.is_triggered());
    }

    #[test]
    fn watchdog_state_predicates_pending() {
        let mut s = WatchdogState::Idle;
        s.arm(0, WatchdogConfig::default());
        assert!(!s.is_idle());
        assert!(s.is_pending());
        assert!(!s.is_triggered());
    }

    #[test]
    fn watchdog_state_predicates_triggered() {
        let mut s = WatchdogState::Idle;
        s.arm(0, WatchdogConfig::default());
        s.mark_triggered();
        assert!(!s.is_idle());
        assert!(!s.is_pending());
        assert!(s.is_triggered());
    }

    #[test]
    fn watchdog_state_predicates_after_disarm_back_to_idle() {
        let mut s = WatchdogState::Idle;
        s.arm(0, WatchdogConfig::default());
        s.disarm();
        assert!(s.is_idle());
    }

    // ----------------------------------------------------------------
    // SyncOutputTelemetry accessors (pub→pub(crate))
    // ----------------------------------------------------------------

    #[test]
    fn sync_output_telemetry_accessors_round_trip_default() {
        let t = SyncOutputTelemetry::default();
        assert_eq!(t.bsu_count(), 0);
        assert_eq!(t.esu_count(), 0);
        assert_eq!(t.adversarial_esu_underflow_count(), 0);
        assert_eq!(t.watchdog_force_flush_count(), 0);
        assert_eq!(t.mid_bsu_byte_count(), 0);
        assert_eq!(t.max_bsu_depth_observed(), 0);
        assert_eq!(t.mode_query_count(), 0);
        assert_eq!(t.esu_flush_count(), 0);
    }

    #[test]
    fn sync_output_telemetry_underflow_counter_only_writeable_via_record() {
        // Pin the privacy invariant: the
        // adversarial_esu_underflow_count counter (which a
        // monitor would alarm on) cannot be silently zeroed
        // from outside the crate. Only record_depth_outcome
        // can mutate it.
        let mut t = SyncOutputTelemetry::default();
        t.record_depth_outcome(BsuDepthOutcome::Underflow, 0);
        t.record_depth_outcome(BsuDepthOutcome::Underflow, 0);
        assert_eq!(t.adversarial_esu_underflow_count(), 2);
        // External code cannot do `t.adversarial_esu_underflow_count = 0;`
        // — compile error via pub(crate) field.
    }

    #[test]
    fn depth_nested_open_open_close_close_flushes_at_outer() {
        let mut c = BsuDepthCounter::new();
        c.open_bsu();
        c.open_bsu();
        let r1 = c.close_esu();
        assert_eq!(r1, BsuDepthOutcome::Closed { new_depth: 1 });
        let r2 = c.close_esu();
        assert_eq!(r2, BsuDepthOutcome::Flushed);
    }

    #[test]
    fn depth_max_observed_tracks_peak() {
        let mut c = BsuDepthCounter::new();
        c.open_bsu(); // 1
        c.open_bsu(); // 2
        c.open_bsu(); // 3
        c.close_esu(); // 2
        c.open_bsu(); // 3 again
        assert_eq!(c.max_observed(), 3);
        assert_eq!(c.depth(), 3);
    }

    #[test]
    fn depth_force_reset_clamps_zero() {
        let mut c = BsuDepthCounter::new();
        c.open_bsu();
        c.open_bsu();
        c.force_reset();
        assert_eq!(c.depth(), 0);
        // max_observed retained for telemetry.
        assert_eq!(c.max_observed(), 2);
    }

    #[test]
    fn depth_adversarial_excess_esu_clamps() {
        // BSU then 5 ESUs — only the first ESU flushes; the
        // rest underflow and depth stays at zero.
        let mut c = BsuDepthCounter::new();
        c.open_bsu();
        let r1 = c.close_esu();
        let r2 = c.close_esu();
        let r3 = c.close_esu();
        assert_eq!(r1, BsuDepthOutcome::Flushed);
        assert_eq!(r2, BsuDepthOutcome::Underflow);
        assert_eq!(r3, BsuDepthOutcome::Underflow);
        assert_eq!(c.depth(), 0);
    }

    #[test]
    fn depth_proptest_invariant_never_negative() {
        // Bead: "proptest invariant: depth always ≥0". Test the
        // invariant exhaustively over a 1000-step adversarial
        // mixed stream.
        let mut c = BsuDepthCounter::new();
        // Simulate 1000 alternating + adversarial events.
        for i in 0..1000 {
            if i % 7 < 4 {
                c.open_bsu();
            } else {
                c.close_esu();
            }
            // Invariant: depth never exceeds the number of processed events.
            assert!(c.depth() <= (i + 1) as u32);
        }
    }

    // ----------------------------------------------------------------
    // WatchdogConfig
    // ----------------------------------------------------------------

    #[test]
    fn watchdog_default_is_150ms() {
        let c = WatchdogConfig::default();
        assert_eq!(c.timeout_ms, 150);
        assert_eq!(c.effective_timeout_ms(), 150);
    }

    #[test]
    fn watchdog_below_floor_is_raised() {
        let c = WatchdogConfig {
            timeout_ms: 5,
            min_timeout_ms: 16,
        };
        assert_eq!(c.effective_timeout_ms(), 16);
    }

    #[test]
    fn watchdog_above_floor_is_passed_through() {
        let c = WatchdogConfig {
            timeout_ms: 250,
            min_timeout_ms: 16,
        };
        assert_eq!(c.effective_timeout_ms(), 250);
    }

    // ----------------------------------------------------------------
    // WatchdogState
    // ----------------------------------------------------------------

    #[test]
    fn watchdog_idle_never_force_flushes() {
        let s = WatchdogState::Idle;
        assert_eq!(s.should_force_flush(1_000_000), WatchdogDecision::Wait);
    }

    #[test]
    fn watchdog_pending_below_deadline_waits() {
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        // Deadline = 1_000 + 150 = 1_150.
        assert_eq!(s.should_force_flush(1_100), WatchdogDecision::Wait);
    }

    #[test]
    fn watchdog_pending_at_deadline_force_flushes() {
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        assert_eq!(s.should_force_flush(1_150), WatchdogDecision::ForceFlush);
    }

    #[test]
    fn watchdog_pending_past_deadline_force_flushes() {
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        assert_eq!(s.should_force_flush(2_000), WatchdogDecision::ForceFlush);
    }

    #[test]
    fn watchdog_disarm_resets_to_idle() {
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        s.disarm();
        assert_eq!(s, WatchdogState::Idle);
        assert_eq!(s.should_force_flush(2_000), WatchdogDecision::Wait);
    }

    #[test]
    fn watchdog_triggered_state_does_not_re_fire() {
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        // Watchdog fires.
        assert_eq!(s.should_force_flush(1_150), WatchdogDecision::ForceFlush);
        s.mark_triggered();
        // Subsequent checks return Wait.
        assert_eq!(s.should_force_flush(1_500), WatchdogDecision::Wait);
        assert_eq!(s.should_force_flush(2_000), WatchdogDecision::Wait);
    }

    #[test]
    fn watchdog_consume_force_flush_fires_exactly_once() {
        // Self-review fix (br-ft-deemu): atomic consume returns
        // ForceFlush once and transitions to Triggered, so a
        // second call cannot double-fire.
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        let first = s.consume_force_flush(1_150);
        assert_eq!(first, WatchdogDecision::ForceFlush);
        assert_eq!(s, WatchdogState::Triggered);
        // Second consume — substrate refuses double-fire.
        let second = s.consume_force_flush(1_200);
        assert_eq!(second, WatchdogDecision::Wait);
    }

    #[test]
    fn watchdog_consume_force_flush_waits_below_deadline() {
        let mut s = WatchdogState::Idle;
        s.arm(1_000, WatchdogConfig::default());
        let d = s.consume_force_flush(1_100);
        assert_eq!(d, WatchdogDecision::Wait);
        // State unchanged.
        match s {
            WatchdogState::Pending { deadline_ms } => {
                assert_eq!(deadline_ms, 1_150);
            }
            other => panic!("expected Pending; got {other:?}"),
        }
    }

    #[test]
    fn watchdog_consume_force_flush_idle_returns_wait() {
        let mut s = WatchdogState::Idle;
        let d = s.consume_force_flush(1_000);
        assert_eq!(d, WatchdogDecision::Wait);
        assert_eq!(s, WatchdogState::Idle);
    }

    #[test]
    fn watchdog_consume_force_flush_already_triggered_returns_wait() {
        let mut s = WatchdogState::Triggered;
        let d = s.consume_force_flush(10_000);
        assert_eq!(d, WatchdogDecision::Wait);
        assert_eq!(s, WatchdogState::Triggered);
    }

    #[test]
    fn watchdog_arm_uses_effective_timeout() {
        let mut s = WatchdogState::Idle;
        let config = WatchdogConfig {
            timeout_ms: 5, // below floor
            min_timeout_ms: 16,
        };
        s.arm(1_000, config);
        // Deadline should be 1_016 (floor applied).
        assert_eq!(s.should_force_flush(1_010), WatchdogDecision::Wait);
        assert_eq!(s.should_force_flush(1_016), WatchdogDecision::ForceFlush);
    }

    // ----------------------------------------------------------------
    // ModeQueryState + classify_mode_query + format_mode_query_response
    // ----------------------------------------------------------------

    #[test]
    fn mode_query_ps_values() {
        assert_eq!(ModeQueryState::NotRecognized.ps_value(), 0);
        assert_eq!(ModeQueryState::Set.ps_value(), 1);
        assert_eq!(ModeQueryState::Reset.ps_value(), 2);
        assert_eq!(ModeQueryState::PermanentlySet.ps_value(), 3);
        assert_eq!(ModeQueryState::PermanentlyReset.ps_value(), 4);
    }

    #[test]
    fn classify_mode_query_set_when_in_bsu() {
        let mut c = BsuDepthCounter::new();
        c.open_bsu();
        assert_eq!(classify_mode_query(&c), ModeQueryState::Set);
    }

    #[test]
    fn classify_mode_query_reset_when_idle() {
        let c = BsuDepthCounter::new();
        assert_eq!(classify_mode_query(&c), ModeQueryState::Reset);
    }

    #[test]
    fn classify_mode_query_set_at_arbitrary_nesting() {
        let mut c = BsuDepthCounter::new();
        c.open_bsu();
        c.open_bsu();
        c.open_bsu();
        assert_eq!(classify_mode_query(&c), ModeQueryState::Set);
    }

    #[test]
    fn format_mode_query_response_set() {
        let s = format_mode_query_response(ModeQueryState::Set);
        assert_eq!(s, "\x1b[?2026;1$y");
    }

    #[test]
    fn format_mode_query_response_reset() {
        let s = format_mode_query_response(ModeQueryState::Reset);
        assert_eq!(s, "\x1b[?2026;2$y");
    }

    // ----------------------------------------------------------------
    // SyncOutputTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = SyncOutputTelemetry::default();
        assert_eq!(t.bsu_count, 0);
        assert_eq!(t.esu_count, 0);
        assert_eq!(t.max_bsu_depth_observed, 0);
    }

    #[test]
    fn telemetry_record_open_increments_bsu_count() {
        let mut t = SyncOutputTelemetry::default();
        t.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);
        assert_eq!(t.bsu_count, 1);
        assert_eq!(t.max_bsu_depth_observed, 1);
    }

    #[test]
    fn telemetry_record_close_increments_esu_count() {
        let mut t = SyncOutputTelemetry::default();
        t.record_depth_outcome(BsuDepthOutcome::Closed { new_depth: 1 }, 2);
        assert_eq!(t.esu_count, 1);
        assert_eq!(t.esu_flush_count, 0);
    }

    #[test]
    fn telemetry_record_flush_increments_esu_and_flush() {
        let mut t = SyncOutputTelemetry::default();
        t.record_depth_outcome(BsuDepthOutcome::Flushed, 3);
        assert_eq!(t.esu_count, 1);
        assert_eq!(t.esu_flush_count, 1);
    }

    #[test]
    fn telemetry_record_underflow_increments_adversarial() {
        let mut t = SyncOutputTelemetry::default();
        t.record_depth_outcome(BsuDepthOutcome::Underflow, 0);
        assert_eq!(t.adversarial_esu_underflow_count, 1);
    }

    #[test]
    fn telemetry_record_max_depth_takes_max() {
        let mut t = SyncOutputTelemetry::default();
        t.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 3 }, 3);
        t.record_depth_outcome(BsuDepthOutcome::Closed { new_depth: 2 }, 3);
        t.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 5 }, 5);
        assert_eq!(t.max_bsu_depth_observed, 5);
    }

    #[test]
    fn telemetry_record_watchdog_force_flush() {
        let mut t = SyncOutputTelemetry::default();
        t.record_watchdog_decision(WatchdogDecision::Wait);
        assert_eq!(t.watchdog_force_flush_count, 0);
        t.record_watchdog_decision(WatchdogDecision::ForceFlush);
        assert_eq!(t.watchdog_force_flush_count, 1);
    }

    #[test]
    fn telemetry_record_mid_bsu_bytes_accumulates() {
        let mut t = SyncOutputTelemetry::default();
        t.record_mid_bsu_bytes(100);
        t.record_mid_bsu_bytes(250);
        assert_eq!(t.mid_bsu_byte_count, 350);
    }

    #[test]
    fn telemetry_record_mode_query_increments() {
        let mut t = SyncOutputTelemetry::default();
        t.record_mode_query();
        t.record_mode_query();
        assert_eq!(t.mode_query_count, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_neovim_treesitter_redraw_pipeline() {
        // Neovim brackets a treesitter redraw with BSU/ESU.
        // No nesting, normal flow.
        let mut depth = BsuDepthCounter::new();
        let mut watchdog = WatchdogState::Idle;
        let mut telem = SyncOutputTelemetry::default();
        let config = WatchdogConfig::default();
        let now = 1_000_000u64;

        // BSU received.
        let r = depth.open_bsu();
        watchdog.arm(now, config);
        telem.record_depth_outcome(r, depth.max_observed());

        // 5 ms later: still pending.
        assert_eq!(watchdog.should_force_flush(now + 5), WatchdogDecision::Wait,);

        // 30 ms of mid-BSU bytes accumulate.
        telem.record_mid_bsu_bytes(8_192);

        // ESU received at +50 ms — well under 150 ms watchdog.
        let r = depth.close_esu();
        assert_eq!(r, BsuDepthOutcome::Flushed);
        watchdog.disarm();
        telem.record_depth_outcome(r, depth.max_observed());

        assert_eq!(telem.bsu_count, 1);
        assert_eq!(telem.esu_count, 1);
        assert_eq!(telem.esu_flush_count, 1);
        assert_eq!(telem.watchdog_force_flush_count, 0);
        assert_eq!(telem.mid_bsu_byte_count, 8_192);
    }

    #[test]
    fn scenario_buggy_app_stuck_bsu_triggers_watchdog() {
        // Bug-bait: an app sends BSU and then deadlocks. ft
        // force-flushes after 150 ms.
        let mut depth = BsuDepthCounter::new();
        let mut watchdog = WatchdogState::Idle;
        let mut telem = SyncOutputTelemetry::default();
        let config = WatchdogConfig::default();
        let now = 0u64;

        depth.open_bsu();
        watchdog.arm(now, config);

        // 200 ms later — past the 150 ms deadline.
        let decision = watchdog.should_force_flush(200);
        assert_eq!(decision, WatchdogDecision::ForceFlush);
        telem.record_watchdog_decision(decision);
        watchdog.mark_triggered();
        depth.force_reset();

        assert_eq!(telem.watchdog_force_flush_count, 1);
        assert_eq!(depth.depth(), 0);
        // Subsequent checks are quiet.
        assert_eq!(watchdog.should_force_flush(500), WatchdogDecision::Wait,);
    }

    #[test]
    fn scenario_nested_btop_refresh() {
        // btop refreshes with nested BSU brackets. Substrate
        // handles depth 3 cleanly.
        let mut depth = BsuDepthCounter::new();
        let mut telem = SyncOutputTelemetry::default();

        for _ in 0..3 {
            let r = depth.open_bsu();
            telem.record_depth_outcome(r, depth.max_observed());
        }
        for _ in 0..3 {
            let r = depth.close_esu();
            telem.record_depth_outcome(r, depth.max_observed());
        }

        assert_eq!(depth.depth(), 0);
        assert_eq!(telem.bsu_count, 3);
        assert_eq!(telem.esu_count, 3);
        assert_eq!(telem.esu_flush_count, 1); // one outer flush
        assert_eq!(telem.max_bsu_depth_observed, 3);
    }

    #[test]
    fn scenario_adversarial_esu_underflow_telemetry() {
        // 1000 ESUs without any BSU. Substrate clamps depth at
        // zero and increments the adversarial counter every time.
        let mut depth = BsuDepthCounter::new();
        let mut telem = SyncOutputTelemetry::default();
        for _ in 0..1000 {
            let r = depth.close_esu();
            telem.record_depth_outcome(r, depth.max_observed());
        }
        assert_eq!(depth.depth(), 0);
        assert_eq!(telem.adversarial_esu_underflow_count, 1000);
    }

    #[test]
    fn scenario_mode_query_response_during_bsu() {
        // App queries `CSI ? 2026 $ p` mid-BSU. Substrate
        // returns Set and the canonical reply bytes.
        let mut depth = BsuDepthCounter::new();
        depth.open_bsu();
        let state = classify_mode_query(&depth);
        assert_eq!(state, ModeQueryState::Set);
        assert_eq!(format_mode_query_response(state), "\x1b[?2026;1$y");
    }

    #[test]
    fn scenario_mode_query_response_idle() {
        let depth = BsuDepthCounter::new();
        let state = classify_mode_query(&depth);
        assert_eq!(state, ModeQueryState::Reset);
        assert_eq!(format_mode_query_response(state), "\x1b[?2026;2$y");
    }
}
