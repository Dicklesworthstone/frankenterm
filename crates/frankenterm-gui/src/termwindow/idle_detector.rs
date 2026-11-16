//! Idle detector with frame-rate dropdown (ft-mpc9b.5.3).
//!
//! Drops the rendering frame rate when ft is idle (no input, no
//! PTY output, no animations) to save battery. The bead's
//! threshold table:
//!
//! | Idle duration  | Mode      | Target FPS |
//! |----------------|-----------|------------|
//! | 0–100 ms       | FullRate  | refresh    |
//! | 100–500 ms     | HalfRate  | refresh/2  |
//! | 500 ms – 5 s   | LowRate   | 5 Hz       |
//! | > 5 s          | Paused    | 0 Hz, wake on event |
//!
//! The user-visible win once the integration lands is the bead's
//! headline: 24 h battery life on M2 MacBook for 50-pane fleets
//! (currently ~12 h).
//!
//! ## What this module ships (foundation, ft-mpc9b.5.3)
//!
//! - `IdleState` — the four modes the state machine moves through.
//! - `IdleEvent` — taxonomy of "wake events" the detector
//!   recognises. Stable identifiers feed the structured-log
//!   emission so transitions can be attributed cleanly.
//! - `IdleDetector` — the central type. Tracks the
//!   last-event timestamp, the assistive-tech-active flag (the
//!   bead's accessibility constraint: VoiceOver/Orca/Narrator
//!   active forbids drop-down), the operator opt-out flag, and
//!   lifetime transition counters.
//! - `IdleTransitionReport` — per-event report carrying the
//!   pre-event state, the post-event state, and the wake latency
//!   (when transitioning out of `Paused` / `LowRate`).
//! - `IdleThresholds` — configurable durations so callers on
//!   non-default hardware (mobile, CI, bench harness) can tune.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - Wiring `IdleDetector::record_event` into every event source
//!   in `crates/frankenterm-gui/src/termwindow/` (PTY data
//!   handler, mouse handler, keyboard handler, focus handler,
//!   animation tick, IPC handler).
//! - Per-platform assistive-tech detection: macOS
//!   `NSAccessibility.isVoiceOverRunning`, Linux AT-SPI bus
//!   presence query, Windows `ScreenReaderRunning` SystemParameter.
//!   Each call lands behind the same trait method so the
//!   detector stays platform-agnostic.
//! - The frame-rate-applied side: feeding
//!   `target_frame_rate_hz()` into the actual paint scheduling
//!   (the periodic timer that decides when to invoke the redraw
//!   predicate from ft-mpc9b.5.1).
//! - Wake-up latency benchmark proving the < 16 ms p99 target.
//! - `ft doctor` surface for the lifetime transition counters
//!   and the current state.
//! - 24 h battery test.

#![allow(dead_code)]

use std::time::{Duration, Instant};

/// The four modes from the bead's threshold table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdleState {
    /// Active or near-active — render at the display's full
    /// refresh rate.
    FullRate,
    /// Slightly idle — render at half the refresh rate.
    HalfRate,
    /// Idle but cursor-blinkable — render at 5 Hz.
    LowRate,
    /// Fully idle — paint only on event wake.
    Paused,
}

impl IdleState {
    /// Default-rate hint for a refresh rate in Hz. Returns the
    /// FPS the paint scheduler should target while in this state.
    pub fn target_fps(self, refresh_rate_hz: u32) -> u32 {
        let refresh = if refresh_rate_hz == 0 {
            60
        } else {
            refresh_rate_hz
        };
        match self {
            IdleState::FullRate => refresh,
            IdleState::HalfRate => refresh.saturating_div(2).max(1),
            IdleState::LowRate => 5,
            IdleState::Paused => 0,
        }
    }
}

/// Stable taxonomy of "wake" events. Each variant maps to one
/// axis of activity the bead enumerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdleEvent {
    /// PTY output from a managed pane.
    PtyData,
    /// Keyboard input from the user.
    Keyboard,
    /// Mouse movement (hover state included).
    MouseMove,
    /// Mouse click / scroll.
    MouseClick,
    /// Window or pane focus change.
    FocusChange,
    /// Animation tick — selection-fade, modal-transition, etc.
    AnimationTick,
    /// BEL (0x07) — must wake even though it's not user-visible
    /// data per se. The bead's "DO NOT BREAK" call.
    Bell,
    /// IPC message from a sibling ft process.
    IpcMessage,
    /// The OS asked us to paint (`setNeedsDisplay` /
    /// ConfigureNotify / Wayland frame callback).
    OsPaintRequest,
}

/// Operator-tunable thresholds. Defaults match the bead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleThresholds {
    /// Below this elapsed since last event → FullRate.
    pub full_to_half: Duration,
    /// Below this → HalfRate.
    pub half_to_low: Duration,
    /// Below this → LowRate; beyond → Paused.
    pub low_to_paused: Duration,
}

impl Default for IdleThresholds {
    fn default() -> Self {
        Self {
            full_to_half: Duration::from_millis(100),
            half_to_low: Duration::from_millis(500),
            low_to_paused: Duration::from_secs(5),
        }
    }
}

/// What `record_event` returns: the previous state, the new
/// state, and the wake latency (`Some` only when transitioning
/// out of `Paused` or `LowRate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTransitionReport {
    pub prev_state: IdleState,
    pub next_state: IdleState,
    pub event: IdleEvent,
    /// Time elapsed since the last event before this one.
    pub elapsed_since_last: Duration,
    /// `Some` when the transition is a wake (Paused/LowRate →
    /// FullRate). The continuation bead's structured log records
    /// this as the wake-up-latency telemetry.
    pub wake_latency: Option<Duration>,
}

impl IdleTransitionReport {
    pub fn is_wake(&self) -> bool {
        self.wake_latency.is_some()
    }
}

/// Lifetime counters surfaced via `ft doctor`. One slot per
/// state so the operator can see how much time was spent at each
/// rate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdleStateStats {
    pub transitions_to_full: u64,
    pub transitions_to_half: u64,
    pub transitions_to_low: u64,
    pub transitions_to_paused: u64,
    /// Total wake events (Paused/LowRate → FullRate).
    pub wakes_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleDoctorSnapshot {
    pub current_state: IdleState,
    pub target_fps: u32,
    pub last_event_kind: IdleEvent,
    pub assistive_tech_active: bool,
    pub idle_dropdown_disabled: bool,
    pub stats: IdleStateStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleSchedulerDecision {
    pub target_fps: u32,
    pub sleep_until_event: bool,
}

impl IdleSchedulerDecision {
    pub fn for_state(state: IdleState, refresh_rate_hz: u32) -> Self {
        let target_fps = state.target_fps(refresh_rate_hz);
        Self {
            target_fps,
            sleep_until_event: target_fps == 0,
        }
    }
}

/// The central idle-detection state machine.
#[derive(Debug)]
pub struct IdleDetector {
    last_event_at: Instant,
    last_event_kind: IdleEvent,
    state: IdleState,
    thresholds: IdleThresholds,
    /// When true, the detector NEVER returns anything other than
    /// `FullRate`. The bead's accessibility constraint:
    /// VoiceOver/Orca/Narrator running forbids drop-down.
    assistive_tech_active: bool,
    /// Operator opt-out. When true, behaves like
    /// `assistive_tech_active`.
    idle_dropdown_disabled: bool,
    stats: IdleStateStats,
}

impl IdleDetector {
    /// Construct a detector with default thresholds and the
    /// given "now" anchor (typically `Instant::now()` at startup).
    /// Initial state is `FullRate` since startup has just produced
    /// activity.
    pub fn new(now: Instant) -> Self {
        Self {
            last_event_at: now,
            last_event_kind: IdleEvent::OsPaintRequest,
            state: IdleState::FullRate,
            thresholds: IdleThresholds::default(),
            assistive_tech_active: false,
            idle_dropdown_disabled: false,
            stats: IdleStateStats::default(),
        }
    }

    pub fn with_thresholds(mut self, thresholds: IdleThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    pub fn with_assistive_tech_active(mut self, on: bool) -> Self {
        self.assistive_tech_active = on;
        self
    }

    pub fn with_idle_dropdown_disabled(mut self, on: bool) -> Self {
        self.idle_dropdown_disabled = on;
        self
    }

    /// Record an event. Wakes the detector to `FullRate` and
    /// returns a transition report. The report's `wake_latency`
    /// is `Some` when the prior state was `LowRate` or `Paused`
    /// — the latency is the elapsed time since the previous
    /// event, which approximates the wake-from-idle delay.
    pub fn record_event(&mut self, event: IdleEvent, now: Instant) -> IdleTransitionReport {
        let prev_state = self.state;
        let elapsed_since_last = now.saturating_duration_since(self.last_event_at);
        let next_state = IdleState::FullRate;
        let wake_latency = if matches!(prev_state, IdleState::LowRate | IdleState::Paused) {
            self.stats.wakes_total = self.stats.wakes_total.saturating_add(1);
            Some(elapsed_since_last)
        } else {
            None
        };

        self.last_event_at = now;
        self.last_event_kind = event;
        if next_state != prev_state {
            self.transition_to(next_state);
        } else {
            self.state = next_state;
        }

        IdleTransitionReport {
            prev_state,
            next_state,
            event,
            elapsed_since_last,
            wake_latency,
        }
    }

    /// Compute and return the current state from elapsed time.
    /// The caller invokes this once per scheduling tick; the
    /// returned state drives the paint cadence via
    /// `target_fps`.
    ///
    /// If `assistive_tech_active` or `idle_dropdown_disabled`
    /// is set, the result is forced to `FullRate` regardless of
    /// elapsed time — the bead's accessibility-and-opt-out
    /// constraints.
    pub fn poll(&mut self, now: Instant) -> IdleState {
        if self.assistive_tech_active || self.idle_dropdown_disabled {
            if self.state != IdleState::FullRate {
                self.transition_to(IdleState::FullRate);
            }
            return IdleState::FullRate;
        }

        let elapsed = now.saturating_duration_since(self.last_event_at);
        let target = self.state_for_elapsed(elapsed);
        if target != self.state {
            self.transition_to(target);
        }
        target
    }

    /// Current cached state without touching the clock. Useful
    /// for read-only consumers (e.g., `ft doctor` query).
    pub fn current_state(&self) -> IdleState {
        self.state
    }

    /// Target FPS for the current state.
    pub fn current_target_fps(&self, refresh_rate_hz: u32) -> u32 {
        self.state.target_fps(refresh_rate_hz)
    }

    pub fn scheduler_decision(&self, refresh_rate_hz: u32) -> IdleSchedulerDecision {
        IdleSchedulerDecision::for_state(self.state, refresh_rate_hz)
    }

    pub fn doctor_snapshot(&self, refresh_rate_hz: u32) -> IdleDoctorSnapshot {
        IdleDoctorSnapshot {
            current_state: self.state,
            target_fps: self.current_target_fps(refresh_rate_hz),
            last_event_kind: self.last_event_kind,
            assistive_tech_active: self.assistive_tech_active,
            idle_dropdown_disabled: self.idle_dropdown_disabled,
            stats: self.stats.clone(),
        }
    }

    /// When the last event was recorded.
    pub fn last_event_at(&self) -> Instant {
        self.last_event_at
    }

    /// What event kind the last event was.
    pub fn last_event_kind(&self) -> IdleEvent {
        self.last_event_kind
    }

    pub fn set_assistive_tech_active(&mut self, on: bool) {
        self.assistive_tech_active = on;
    }

    pub fn set_idle_dropdown_disabled(&mut self, on: bool) {
        self.idle_dropdown_disabled = on;
    }

    pub fn assistive_tech_active(&self) -> bool {
        self.assistive_tech_active
    }

    pub fn idle_dropdown_disabled(&self) -> bool {
        self.idle_dropdown_disabled
    }

    pub fn stats(&self) -> &IdleStateStats {
        &self.stats
    }

    pub fn thresholds(&self) -> &IdleThresholds {
        &self.thresholds
    }

    fn state_for_elapsed(&self, elapsed: Duration) -> IdleState {
        if elapsed < self.thresholds.full_to_half {
            IdleState::FullRate
        } else if elapsed < self.thresholds.half_to_low {
            IdleState::HalfRate
        } else if elapsed < self.thresholds.low_to_paused {
            IdleState::LowRate
        } else {
            IdleState::Paused
        }
    }

    fn transition_to(&mut self, target: IdleState) {
        match target {
            IdleState::FullRate => {
                self.stats.transitions_to_full = self.stats.transitions_to_full.saturating_add(1);
            }
            IdleState::HalfRate => {
                self.stats.transitions_to_half = self.stats.transitions_to_half.saturating_add(1);
            }
            IdleState::LowRate => {
                self.stats.transitions_to_low = self.stats.transitions_to_low.saturating_add(1);
            }
            IdleState::Paused => {
                self.stats.transitions_to_paused =
                    self.stats.transitions_to_paused.saturating_add(1);
            }
        }
        self.state = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn target_fps_table_matches_bead() {
        assert_eq!(IdleState::FullRate.target_fps(60), 60);
        assert_eq!(IdleState::FullRate.target_fps(120), 120);
        assert_eq!(IdleState::HalfRate.target_fps(60), 30);
        assert_eq!(IdleState::HalfRate.target_fps(120), 60);
        assert_eq!(IdleState::LowRate.target_fps(60), 5);
        assert_eq!(IdleState::LowRate.target_fps(120), 5);
        assert_eq!(IdleState::Paused.target_fps(60), 0);
        assert_eq!(IdleState::Paused.target_fps(120), 0);
    }

    #[test]
    fn target_fps_with_zero_refresh_remaps_to_60hz() {
        assert_eq!(IdleState::FullRate.target_fps(0), 60);
        assert_eq!(IdleState::HalfRate.target_fps(0), 30);
        assert_eq!(IdleState::LowRate.target_fps(0), 5);
        assert_eq!(IdleState::Paused.target_fps(0), 0);
    }

    #[test]
    fn half_rate_never_sleeps_until_event_for_nonzero_refresh() {
        let decision = IdleSchedulerDecision::for_state(IdleState::HalfRate, 1);

        assert_eq!(decision.target_fps, 1);
        assert!(!decision.sleep_until_event);
    }

    #[test]
    fn fresh_detector_is_full_rate() {
        let d = IdleDetector::new(t0());
        assert_eq!(d.current_state(), IdleState::FullRate);
    }

    #[test]
    fn poll_under_100ms_stays_full_rate() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let state = d.poll(now + Duration::from_millis(50));
        assert_eq!(state, IdleState::FullRate);
    }

    #[test]
    fn poll_at_100ms_to_500ms_drops_to_half_rate() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let state = d.poll(now + Duration::from_millis(200));
        assert_eq!(state, IdleState::HalfRate);
    }

    #[test]
    fn poll_at_500ms_to_5s_drops_to_low_rate() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let state = d.poll(now + Duration::from_millis(1500));
        assert_eq!(state, IdleState::LowRate);
    }

    #[test]
    fn poll_past_5s_pauses() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let state = d.poll(now + Duration::from_secs(7));
        assert_eq!(state, IdleState::Paused);
    }

    #[test]
    fn record_event_wakes_to_full_rate() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        d.poll(now + Duration::from_secs(2)); // → LowRate
        let report = d.record_event(IdleEvent::Keyboard, now + Duration::from_secs(2));
        assert_eq!(report.prev_state, IdleState::LowRate);
        assert_eq!(report.next_state, IdleState::FullRate);
        assert_eq!(report.event, IdleEvent::Keyboard);
        assert!(report.is_wake());
        assert_eq!(d.current_state(), IdleState::FullRate);
    }

    #[test]
    fn wake_latency_reflects_elapsed_idle_time() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        d.poll(now + Duration::from_secs(3)); // → LowRate
        let report = d.record_event(IdleEvent::PtyData, now + Duration::from_secs(3));
        assert!(report.wake_latency.is_some());
        let latency = report.wake_latency.unwrap();
        // Should be approximately 3 seconds.
        assert!(latency >= Duration::from_secs(2));
        assert!(latency <= Duration::from_secs(4));
    }

    #[test]
    fn record_event_in_full_rate_is_not_a_wake() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let report = d.record_event(IdleEvent::Keyboard, now + Duration::from_millis(50));
        assert_eq!(report.prev_state, IdleState::FullRate);
        assert!(!report.is_wake());
        assert!(report.wake_latency.is_none());
    }

    #[test]
    fn record_event_from_half_rate_is_not_a_wake() {
        // Wake latency only fires on Paused/LowRate → FullRate.
        // HalfRate → FullRate is just a normal state advance.
        let now = t0();
        let mut d = IdleDetector::new(now);
        d.poll(now + Duration::from_millis(200)); // → HalfRate
        let report = d.record_event(IdleEvent::PtyData, now + Duration::from_millis(200));
        assert_eq!(report.prev_state, IdleState::HalfRate);
        assert_eq!(report.next_state, IdleState::FullRate);
        assert!(!report.is_wake());
    }

    #[test]
    fn assistive_tech_active_forces_full_rate() {
        let now = t0();
        let mut d = IdleDetector::new(now).with_assistive_tech_active(true);
        // Even with 10 seconds elapsed, poll returns FullRate.
        let state = d.poll(now + Duration::from_secs(10));
        assert_eq!(state, IdleState::FullRate);
    }

    #[test]
    fn idle_dropdown_disabled_forces_full_rate() {
        let now = t0();
        let mut d = IdleDetector::new(now).with_idle_dropdown_disabled(true);
        let state = d.poll(now + Duration::from_secs(10));
        assert_eq!(state, IdleState::FullRate);
    }

    #[test]
    fn enabling_assistive_tech_mid_idle_pulls_back_to_full_rate() {
        // The bead's accessibility invariant: if VoiceOver
        // launches while we're in Paused, the next poll must
        // immediately return FullRate so AT-driven screen
        // updates are honored.
        let now = t0();
        let mut d = IdleDetector::new(now);
        let _ = d.poll(now + Duration::from_secs(7)); // → Paused
        assert_eq!(d.current_state(), IdleState::Paused);

        d.set_assistive_tech_active(true);
        let state = d.poll(now + Duration::from_secs(7));
        assert_eq!(state, IdleState::FullRate);
        // Lifetime counter for the FullRate transition bumped.
        assert!(d.stats().transitions_to_full > 0);
    }

    #[test]
    fn bell_event_wakes_immediately() {
        // The bead's BEL constraint: a BEL must wake even though
        // it's not "user input" in the visible sense. We model
        // this by recording a Bell event from Paused and
        // confirming the state advances to FullRate.
        let now = t0();
        let mut d = IdleDetector::new(now);
        let _ = d.poll(now + Duration::from_secs(8)); // → Paused
        let report = d.record_event(IdleEvent::Bell, now + Duration::from_secs(8));
        assert_eq!(report.prev_state, IdleState::Paused);
        assert_eq!(report.next_state, IdleState::FullRate);
        assert!(report.is_wake());
        assert_eq!(report.event, IdleEvent::Bell);
    }

    #[test]
    fn focus_change_event_wakes_immediately() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let _ = d.poll(now + Duration::from_secs(8)); // → Paused
        let report = d.record_event(IdleEvent::FocusChange, now + Duration::from_secs(8));
        assert_eq!(report.next_state, IdleState::FullRate);
        assert!(report.is_wake());
    }

    #[test]
    fn mouse_move_wakes_paused_detector() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let _ = d.poll(now + Duration::from_secs(10)); // → Paused
        let report = d.record_event(IdleEvent::MouseMove, now + Duration::from_secs(10));
        assert_eq!(report.next_state, IdleState::FullRate);
        assert!(report.is_wake());
    }

    #[test]
    fn state_transitions_increment_lifetime_counters() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        // Walk through every state via poll calls.
        let _ = d.poll(now + Duration::from_millis(50)); // FullRate (no transition)
        let _ = d.poll(now + Duration::from_millis(200)); // → HalfRate
        let _ = d.poll(now + Duration::from_secs(1)); // → LowRate
        let _ = d.poll(now + Duration::from_secs(7)); // → Paused
        let _ = d.record_event(IdleEvent::Keyboard, now + Duration::from_secs(7)); // → FullRate
        let s = d.stats();
        assert_eq!(s.transitions_to_half, 1);
        assert_eq!(s.transitions_to_low, 1);
        assert_eq!(s.transitions_to_paused, 1);
        assert_eq!(s.transitions_to_full, 1);
        assert_eq!(s.wakes_total, 1);
    }

    #[test]
    fn custom_thresholds_apply() {
        // Tighter mobile-style thresholds: drop to LowRate after 50ms.
        let aggressive = IdleThresholds {
            full_to_half: Duration::from_millis(20),
            half_to_low: Duration::from_millis(50),
            low_to_paused: Duration::from_millis(500),
        };
        let now = t0();
        let mut d = IdleDetector::new(now).with_thresholds(aggressive);
        assert_eq!(d.poll(now + Duration::from_millis(30)), IdleState::HalfRate);
        assert_eq!(d.poll(now + Duration::from_millis(100)), IdleState::LowRate);
        assert_eq!(d.poll(now + Duration::from_secs(1)), IdleState::Paused);
    }

    #[test]
    fn current_target_fps_reflects_state() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        assert_eq!(d.current_target_fps(60), 60);
        let _ = d.poll(now + Duration::from_millis(200));
        assert_eq!(d.current_target_fps(60), 30);
        let _ = d.poll(now + Duration::from_secs(1));
        assert_eq!(d.current_target_fps(60), 5);
        let _ = d.poll(now + Duration::from_secs(7));
        assert_eq!(d.current_target_fps(60), 0);
    }

    #[test]
    fn paused_state_target_fps_is_zero_for_any_refresh() {
        // The paint scheduler should treat target_fps=0 as
        // "don't tick; wait for an event". Pin the invariant.
        for hz in [60, 90, 120, 144, 240, 480] {
            assert_eq!(IdleState::Paused.target_fps(hz), 0, "hz={}", hz);
        }
    }

    #[test]
    fn scheduler_decision_marks_paused_as_event_driven() {
        assert_eq!(
            IdleSchedulerDecision::for_state(IdleState::Paused, 60),
            IdleSchedulerDecision {
                target_fps: 0,
                sleep_until_event: true,
            }
        );
        assert_eq!(
            IdleSchedulerDecision::for_state(IdleState::LowRate, 60),
            IdleSchedulerDecision {
                target_fps: 5,
                sleep_until_event: false,
            }
        );
    }

    #[test]
    fn doctor_snapshot_reflects_state_and_last_event() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        d.poll(now + Duration::from_secs(8));
        d.record_event(IdleEvent::Bell, now + Duration::from_secs(9));

        let snapshot = d.doctor_snapshot(120);
        assert_eq!(snapshot.current_state, IdleState::FullRate);
        assert_eq!(snapshot.target_fps, 120);
        assert_eq!(snapshot.last_event_kind, IdleEvent::Bell);
        assert_eq!(snapshot.stats.wakes_total, 1);
    }

    #[test]
    fn last_event_kind_is_recorded() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        d.record_event(IdleEvent::Keyboard, now);
        assert_eq!(d.last_event_kind(), IdleEvent::Keyboard);
        d.record_event(IdleEvent::PtyData, now + Duration::from_millis(10));
        assert_eq!(d.last_event_kind(), IdleEvent::PtyData);
    }

    #[test]
    fn opt_out_can_be_toggled_at_runtime() {
        let now = t0();
        let mut d = IdleDetector::new(now);
        let _ = d.poll(now + Duration::from_secs(8)); // Paused
        assert_eq!(d.current_state(), IdleState::Paused);

        d.set_idle_dropdown_disabled(true);
        let state = d.poll(now + Duration::from_secs(8));
        assert_eq!(state, IdleState::FullRate);

        d.set_idle_dropdown_disabled(false);
        let state = d.poll(now + Duration::from_secs(8));
        assert_eq!(state, IdleState::Paused);
    }

    #[test]
    fn quick_24h_idle_simulation_stays_paused() {
        // The bead's headline target: 24 h on a healthy battery
        // for a 50-pane fleet. Model: simulate 24 h of silence
        // and confirm the detector stays in Paused (target_fps=0)
        // after the initial threshold transitions.
        let now = t0();
        let mut d = IdleDetector::new(now);
        let after = now + Duration::from_secs(24 * 60 * 60);
        let state = d.poll(after);
        assert_eq!(state, IdleState::Paused);
        assert_eq!(d.current_target_fps(60), 0);
    }

    #[test]
    fn typing_pattern_stays_at_full_rate() {
        // Fast typing — events at 50ms intervals. Each
        // record_event resets last_event_at; subsequent polls
        // see <100ms elapsed and stay FullRate.
        let now = t0();
        let mut d = IdleDetector::new(now);
        for i in 0..30 {
            let t = now + Duration::from_millis(50 * i);
            d.record_event(IdleEvent::Keyboard, t);
            assert_eq!(d.current_state(), IdleState::FullRate);
            // Polling 25ms later still says FullRate.
            assert_eq!(d.poll(t + Duration::from_millis(25)), IdleState::FullRate);
        }
        // No paused/low transitions during a typing session.
        assert_eq!(d.stats().transitions_to_paused, 0);
        assert_eq!(d.stats().transitions_to_low, 0);
    }
}
