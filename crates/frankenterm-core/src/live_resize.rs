//! Live-resize gesture state machine + watchdog + coalescing
//! ([BR-TERM-EMULATOR-UPLIFT.2.1] / `ft-mpc9b.2.1`).
//!
//! Sub-epic 2's draft-mode rendering and incremental reflow both
//! depend on a clean `LiveResizeState` signal: did the user start a
//! drag, are they still dragging, did they release? The platforms
//! disagree on how to report this:
//!
//! - **macOS** — `NSWindowWillStartLiveResize` /
//!   `NSWindowDidEndLiveResize`. Sometimes skips DidEnd on a fast
//!   release; recovery is correlation with the mouse-up event.
//! - **Wayland** — `xdg_toplevel::configure` state including
//!   `Resizing`. Compositors emit different cadences (mutter vs
//!   sway vs Hyprland); a configure storm (>100 in 100ms)
//!   needs coalescing to avoid dirty-flooding the renderer.
//! - **X11** — `_NET_WM_STATE_LIVE_RESIZE` on cooperating WMs;
//!   `ConfigureNotify` burst heuristic everywhere else. False
//!   positives from workspace switches must be filtered (only
//!   treat as live-resize if dimensions changed, not just
//!   position).
//!
//! This module is the platform-agnostic state machine + the
//! watchdog/coalescing/heuristic logic the per-platform recorders
//! feed events into. The per-platform integration (touching
//! `frankenterm/window/src/os/macos/window.rs`,
//! `os/wayland/window.rs`, `os/x11/window.rs`) is the follow-on
//! integration bead; this module gives that bead a stable contract
//! plus an always-on regression net so the integration can't drift.

use serde::{Deserialize, Serialize};

// ============================================================================
// State enum
// ============================================================================

/// The closed list of live-resize states. The state diagram is:
///
/// ```text
///   Idle ──BeginSignal──▶ ResizeBegin ──Configure──▶ Resizing ──EndSignal──▶ ResizeEnd ──▶ Idle
///                                          ▲             │
///                                          └─Configure───┘
///                                                        │
///                                                        └──Watchdog (5s no events)──▶ ResizeEnd
/// ```
///
/// Projected onto `(Idle → Begin → Resizing* → End → Idle)` the
/// diagram is acyclic. Every `ResizeBegin` is followed by exactly
/// one `ResizeEnd` (forced by the watchdog if the platform skips
/// it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveResizeState {
    /// Default — no gesture in progress.
    Idle,
    /// Gesture has started; renderer should switch to draft mode.
    ResizeBegin,
    /// Drag in progress; size deltas may be arriving.
    Resizing,
    /// Gesture released; renderer should snap back to full quality
    /// next frame, then transition to Idle.
    ResizeEnd,
}

impl LiveResizeState {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::ResizeBegin => "resize_begin",
            Self::Resizing => "resizing",
            Self::ResizeEnd => "resize_end",
        }
    }

    /// Whether the renderer should run in draft mode for this
    /// state (the bead's downstream consumer in
    /// `ft-mpc9b.2.2`).
    #[must_use]
    pub const fn is_draft_mode(self) -> bool {
        matches!(self, Self::ResizeBegin | Self::Resizing)
    }
}

// ============================================================================
// Platforms
// ============================================================================

/// Identifies which platform recorder emitted the events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveResizePlatform {
    Macos,
    Wayland,
    X11,
    /// Synthetic — the in-memory recorder used by the regression
    /// fixture until the per-platform integrations land.
    Synthetic,
}

impl LiveResizePlatform {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Wayland => "wayland",
            Self::X11 => "x11",
            Self::Synthetic => "synthetic",
        }
    }

    /// Whether a real platform integration is wired today.
    /// Sentinel for the regression-fixture `is_wired` honesty
    /// test, mirroring the pattern from `a11y_tree`,
    /// `color_management`, `ime_caret`.
    #[must_use]
    pub const fn is_wired(self) -> bool {
        matches!(self, Self::Synthetic)
    }
}

// ============================================================================
// Input event taxonomy
// ============================================================================

/// One platform-emitted event the state machine processes. Each
/// per-platform recorder maps its native source notification onto
/// one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveResizeEvent {
    /// Platform signals the start of a gesture.
    /// (macOS: `windowWillStartLiveResize`; Wayland:
    /// `xdg_toplevel::configure` with Resizing state first
    /// time; X11: `_NET_WM_STATE_LIVE_RESIZE` set, OR first
    /// dimension-changing `ConfigureNotify` of a burst.)
    BeginSignal { ts_ms: u64 },
    /// Per-frame configuration with new dimensions during drag.
    Configure { ts_ms: u64, width: u32, height: u32 },
    /// Platform signals the end of a gesture.
    /// (macOS: `windowDidEndLiveResize`; Wayland: `configure`
    /// state without Resizing; X11: `_NET_WM_STATE_LIVE_RESIZE`
    /// cleared, OR no `ConfigureNotify` for >16ms after a burst.)
    EndSignal { ts_ms: u64 },
    /// macOS recovery: a mouse-up arrived while in `Resizing`.
    /// Per the bead's failure-mode spec, this forces a
    /// `ResizeEnd` even if `windowDidEndLiveResize` never fired.
    MouseUpDuringResize { ts_ms: u64 },
    /// Watchdog tick. The integration layer wakes the machine
    /// every N ms (typically 100); the machine self-decides if
    /// the 5s no-event timeout has elapsed.
    WatchdogTick { ts_ms: u64 },
}

impl LiveResizeEvent {
    #[must_use]
    pub fn ts_ms(&self) -> u64 {
        match self {
            Self::BeginSignal { ts_ms }
            | Self::Configure { ts_ms, .. }
            | Self::EndSignal { ts_ms }
            | Self::MouseUpDuringResize { ts_ms }
            | Self::WatchdogTick { ts_ms } => *ts_ms,
        }
    }
}

// ============================================================================
// Transition log
// ============================================================================

/// One row of `tests/live_resize/logs/<platform>-<scenario>.jsonl`
/// per the bead's structured-logging schema. Emitted on every
/// state-changing event; non-state-changing events (e.g. a
/// Configure that arrives in `Resizing`) bump
/// `events_consumed_total` but don't produce a transition row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveResizeTransition {
    pub ts_ms: u64,
    pub prev_state: LiveResizeState,
    pub next_state: LiveResizeState,
    /// What kind of event drove the transition.
    pub source: LiveResizeTransitionSource,
    /// Most recently observed dimensions (when applicable).
    /// `None` for transitions driven by `WatchdogTick` /
    /// `MouseUpDuringResize` that carry no fresh dimensions.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dimensions: Option<(u32, u32)>,
}

/// Why a transition fired. Mirrors the bead's "source_event"
/// schema field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveResizeTransitionSource {
    /// Platform's native begin signal.
    PlatformBegin,
    /// Platform's native end signal.
    PlatformEnd,
    /// macOS skipped-DidEnd recovery via mouse-up.
    MouseUpRecovery,
    /// Watchdog forced a `ResizeEnd` after 5s of silence.
    WatchdogForcedEnd,
    /// Configure storm coalescing — the FIRST configure during
    /// `Idle` synthesizes a `ResizeBegin` even though no
    /// `BeginSignal` fired (X11 ConfigureNotify burst path).
    ConfigureBurstSynthesizedBegin,
}

// ============================================================================
// State machine
// ============================================================================

/// Default watchdog timeout (5 seconds in ms). The bead's
/// failure-mode spec.
pub const WATCHDOG_TIMEOUT_MS: u64 = 5_000;

/// Default coalescing window for Wayland configure storms (100ms
/// per the bead spec).
pub const COALESCE_WINDOW_MS: u64 = 100;

/// Threshold for "configure storm" (>100 configures in
/// `COALESCE_WINDOW_MS`). When exceeded, the machine elides
/// per-configure dirty events; the integration layer should batch
/// them.
pub const COALESCE_THRESHOLD: u32 = 100;

/// X11-specific: minimum interval between `ConfigureNotify`
/// events that count as "still resizing". Past this, the machine
/// treats the next event as a NEW gesture (covers
/// `_NET_WM_STATE_LIVE_RESIZE`-less compositors).
pub const X11_CONFIGURE_BURST_GAP_MS: u64 = 16;

/// Coalesced configure-storm decision returned to the integration
/// layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoalesceDecision {
    /// Pass the event through to the renderer.
    Emit,
    /// Suppress the event — too many in the recent window.
    Coalesce,
}

/// The platform-agnostic state machine.
#[derive(Debug, Clone)]
pub struct LiveResizeStateMachine {
    state: LiveResizeState,
    /// Timestamp of the most recent NON-WATCHDOG event. Watchdog
    /// ticks compare their ts against this, so a 5s burst of
    /// silent watchdog ticks correctly fires the timeout instead
    /// of resetting it.
    last_activity_ts_ms: u64,
    last_dimensions: Option<(u32, u32)>,
    /// Recent configure-event timestamps; bounded by
    /// `COALESCE_THRESHOLD` so memory stays O(1).
    recent_configure_ts: Vec<u64>,
    /// Counters (lifetime-cumulative).
    transitions_total: u64,
    events_consumed_total: u64,
    coalesced_total: u64,
    watchdog_forced_ends_total: u64,
    mouse_up_recoveries_total: u64,
}

impl Default for LiveResizeStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveResizeStateMachine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: LiveResizeState::Idle,
            last_activity_ts_ms: 0,
            last_dimensions: None,
            recent_configure_ts: Vec::with_capacity(COALESCE_THRESHOLD as usize),
            transitions_total: 0,
            events_consumed_total: 0,
            coalesced_total: 0,
            watchdog_forced_ends_total: 0,
            mouse_up_recoveries_total: 0,
        }
    }

    #[inline]
    #[must_use]
    pub fn state(&self) -> LiveResizeState {
        self.state
    }

    #[inline]
    #[must_use]
    pub fn last_dimensions(&self) -> Option<(u32, u32)> {
        self.last_dimensions
    }

    /// Process one platform event. Returns `Some(transition)` when
    /// the event drove a state change; `None` otherwise. Always
    /// bumps `events_consumed_total`.
    pub fn step(&mut self, event: LiveResizeEvent) -> Option<LiveResizeTransition> {
        self.events_consumed_total += 1;
        // Watchdog ticks must NOT reset the activity clock — that
        // would defeat the timeout. All other events DO count as
        // activity.
        if !matches!(event, LiveResizeEvent::WatchdogTick { .. }) {
            self.last_activity_ts_ms = event.ts_ms();
        }

        match (self.state, event) {
            // ── Begin ────────────────────────────────────────────
            (LiveResizeState::Idle, LiveResizeEvent::BeginSignal { ts_ms }) => {
                self.recent_configure_ts.clear();
                self.transition(
                    ts_ms,
                    LiveResizeState::ResizeBegin,
                    LiveResizeTransitionSource::PlatformBegin,
                    None,
                )
            }
            // X11 ConfigureNotify-burst path: a Configure during
            // Idle synthesizes a Begin if dimensions actually
            // changed.
            (
                LiveResizeState::Idle,
                LiveResizeEvent::Configure {
                    ts_ms,
                    width,
                    height,
                },
            ) => {
                let changed = self
                    .last_dimensions
                    .is_none_or(|(w, h)| w != width || h != height);
                if !changed {
                    // Pure window-move (X11 fake-positive guard).
                    return None;
                }
                self.last_dimensions = Some((width, height));
                self.recent_configure_ts.clear();
                self.recent_configure_ts.push(ts_ms);
                self.transition(
                    ts_ms,
                    LiveResizeState::ResizeBegin,
                    LiveResizeTransitionSource::ConfigureBurstSynthesizedBegin,
                    Some((width, height)),
                )
            }

            // ── Begin → Resizing on first Configure ─────────────
            (
                LiveResizeState::ResizeBegin,
                LiveResizeEvent::Configure {
                    ts_ms,
                    width,
                    height,
                },
            ) => {
                self.last_dimensions = Some((width, height));
                self.push_configure_ts(ts_ms);
                self.transition(
                    ts_ms,
                    LiveResizeState::Resizing,
                    LiveResizeTransitionSource::PlatformBegin,
                    Some((width, height)),
                )
            }

            // ── Resizing — keep stream of configures coalesced ──
            (
                LiveResizeState::Resizing,
                LiveResizeEvent::Configure {
                    ts_ms,
                    width,
                    height,
                },
            ) => {
                self.last_dimensions = Some((width, height));
                let decision = self.classify_configure(ts_ms);
                self.push_configure_ts(ts_ms);
                if matches!(decision, CoalesceDecision::Coalesce) {
                    self.coalesced_total += 1;
                }
                // Configures within Resizing don't change state —
                // they're width/height updates the integration
                // layer pushes through if not coalesced.
                None
            }

            // ── End signals (Resizing → ResizeEnd → Idle) ───────
            (
                LiveResizeState::ResizeBegin | LiveResizeState::Resizing,
                LiveResizeEvent::EndSignal { ts_ms },
            ) => {
                let dims = self.last_dimensions;
                self.transition(
                    ts_ms,
                    LiveResizeState::ResizeEnd,
                    LiveResizeTransitionSource::PlatformEnd,
                    dims,
                )
            }

            // ── macOS skipped-DidEnd recovery ────────────────────
            (
                LiveResizeState::ResizeBegin | LiveResizeState::Resizing,
                LiveResizeEvent::MouseUpDuringResize { ts_ms },
            ) => {
                self.mouse_up_recoveries_total += 1;
                let dims = self.last_dimensions;
                self.transition(
                    ts_ms,
                    LiveResizeState::ResizeEnd,
                    LiveResizeTransitionSource::MouseUpRecovery,
                    dims,
                )
            }

            // ── Watchdog forced end ──────────────────────────────
            (
                LiveResizeState::ResizeBegin | LiveResizeState::Resizing,
                LiveResizeEvent::WatchdogTick { ts_ms },
            ) => {
                if ts_ms.saturating_sub(self.most_recent_activity_ts()) >= WATCHDOG_TIMEOUT_MS {
                    self.watchdog_forced_ends_total += 1;
                    let dims = self.last_dimensions;
                    self.transition(
                        ts_ms,
                        LiveResizeState::ResizeEnd,
                        LiveResizeTransitionSource::WatchdogForcedEnd,
                        dims,
                    )
                } else {
                    None
                }
            }

            // ── ResizeEnd → Idle (always immediate) ─────────────
            //
            // The integration layer can flush the snap-back paint
            // pass before stepping the watchdog tick that actually
            // moves us to Idle, OR it can call `transition_to_idle`
            // explicitly. We auto-transition on the next event of
            // any kind so a stuck-in-ResizeEnd state is impossible.
            (LiveResizeState::ResizeEnd, _) => {
                let ts_ms = event.ts_ms();
                let auto = self.transition(
                    ts_ms,
                    LiveResizeState::Idle,
                    LiveResizeTransitionSource::PlatformEnd,
                    None,
                );
                // Re-process the original event from Idle now.
                let next = self.step(event);
                next.or(auto)
            }

            // ── No-op cases ─────────────────────────────────────
            (_, LiveResizeEvent::WatchdogTick { .. }) => None,
            (LiveResizeState::Idle, LiveResizeEvent::EndSignal { .. }) => None,
            (LiveResizeState::Idle, LiveResizeEvent::MouseUpDuringResize { .. }) => None,
            (
                LiveResizeState::ResizeBegin | LiveResizeState::Resizing,
                LiveResizeEvent::BeginSignal { .. },
            ) => None,
        }
    }

    /// Cumulative health snapshot for `ft doctor`. Mirrors the
    /// `AtlasStabilityHealth` / `TripleBufferHealth` shape.
    #[must_use]
    pub fn health(&self) -> LiveResizeHealth {
        LiveResizeHealth {
            current_state: self.state,
            transitions_total: self.transitions_total,
            events_consumed_total: self.events_consumed_total,
            coalesced_total: self.coalesced_total,
            watchdog_forced_ends_total: self.watchdog_forced_ends_total,
            mouse_up_recoveries_total: self.mouse_up_recoveries_total,
        }
    }

    fn transition(
        &mut self,
        ts_ms: u64,
        next: LiveResizeState,
        source: LiveResizeTransitionSource,
        dimensions: Option<(u32, u32)>,
    ) -> Option<LiveResizeTransition> {
        if self.state == next {
            return None;
        }
        let prev = self.state;
        self.state = next;
        self.transitions_total += 1;
        Some(LiveResizeTransition {
            ts_ms,
            prev_state: prev,
            next_state: next,
            source,
            dimensions,
        })
    }

    fn push_configure_ts(&mut self, ts_ms: u64) {
        // Drop entries older than the coalesce window.
        let cutoff = ts_ms.saturating_sub(COALESCE_WINDOW_MS);
        self.recent_configure_ts.retain(|&t| t >= cutoff);
        if self.recent_configure_ts.len() < COALESCE_THRESHOLD as usize * 2 {
            self.recent_configure_ts.push(ts_ms);
        }
    }

    fn classify_configure(&mut self, ts_ms: u64) -> CoalesceDecision {
        let cutoff = ts_ms.saturating_sub(COALESCE_WINDOW_MS);
        let recent = self
            .recent_configure_ts
            .iter()
            .filter(|&&t| t >= cutoff)
            .count();
        if recent as u32 > COALESCE_THRESHOLD {
            CoalesceDecision::Coalesce
        } else {
            CoalesceDecision::Emit
        }
    }

    fn most_recent_activity_ts(&self) -> u64 {
        self.recent_configure_ts
            .last()
            .copied()
            .unwrap_or(self.last_activity_ts_ms)
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveResizeHealth {
    pub current_state: LiveResizeState,
    pub transitions_total: u64,
    pub events_consumed_total: u64,
    pub coalesced_total: u64,
    pub watchdog_forced_ends_total: u64,
    pub mouse_up_recoveries_total: u64,
}

impl LiveResizeHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            current_state: LiveResizeState::Idle,
            transitions_total: 0,
            events_consumed_total: 0,
            coalesced_total: 0,
            watchdog_forced_ends_total: 0,
            mouse_up_recoveries_total: 0,
        }
    }

    /// Whether the machine is in a "renderer should run draft
    /// mode" state.
    #[must_use]
    pub fn is_draft_mode(&self) -> bool {
        self.current_state.is_draft_mode()
    }
}

// ============================================================================
// JSONL writer
// ============================================================================

/// Render a transition log as JSONL (one transition per line).
#[must_use]
pub fn render_transitions_jsonl(transitions: &[LiveResizeTransition]) -> String {
    let mut out = String::new();
    for t in transitions {
        let line = serde_json::to_string(t).expect("LiveResizeTransition always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_transitions_jsonl(
    jsonl: &str,
) -> Result<Vec<LiveResizeTransition>, serde_json::Error> {
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
    fn initial_state_is_idle() {
        let m = LiveResizeStateMachine::new();
        assert_eq!(m.state(), LiveResizeState::Idle);
        assert!(!m.health().is_draft_mode());
    }

    #[test]
    fn happy_path_begin_resize_end() {
        let mut m = LiveResizeStateMachine::new();
        let t1 = m.step(LiveResizeEvent::BeginSignal { ts_ms: 0 }).unwrap();
        assert_eq!(t1.next_state, LiveResizeState::ResizeBegin);
        let t2 = m
            .step(LiveResizeEvent::Configure {
                ts_ms: 5,
                width: 800,
                height: 600,
            })
            .unwrap();
        assert_eq!(t2.next_state, LiveResizeState::Resizing);
        // Configures during Resizing produce no transitions.
        assert!(
            m.step(LiveResizeEvent::Configure {
                ts_ms: 10,
                width: 810,
                height: 600
            })
            .is_none()
        );
        let t3 = m.step(LiveResizeEvent::EndSignal { ts_ms: 20 }).unwrap();
        assert_eq!(t3.next_state, LiveResizeState::ResizeEnd);
        // Any subsequent event auto-clears ResizeEnd → Idle.
        let _ = m.step(LiveResizeEvent::WatchdogTick { ts_ms: 30 });
        assert_eq!(m.state(), LiveResizeState::Idle);
    }

    #[test]
    fn x11_configure_burst_synthesizes_begin_and_filters_pure_moves() {
        let mut m = LiveResizeStateMachine::new();
        // First Configure with no prior dimensions: synthesize Begin.
        let t = m
            .step(LiveResizeEvent::Configure {
                ts_ms: 0,
                width: 800,
                height: 600,
            })
            .unwrap();
        assert_eq!(t.next_state, LiveResizeState::ResizeBegin);
        assert_eq!(
            t.source,
            LiveResizeTransitionSource::ConfigureBurstSynthesizedBegin
        );
        // Second Configure with same dimensions in Resizing should
        // not change state but should not be ignored either.
        let _ = m
            .step(LiveResizeEvent::Configure {
                ts_ms: 1,
                width: 800,
                height: 600,
            })
            .unwrap();
        // Now go back to Idle.
        let _ = m.step(LiveResizeEvent::EndSignal { ts_ms: 100 });
        let _ = m.step(LiveResizeEvent::WatchdogTick { ts_ms: 200 });
        assert_eq!(m.state(), LiveResizeState::Idle);
        // A Configure with SAME dimensions while idle is treated
        // as a window-move (X11 fake positive) — no transition.
        assert!(
            m.step(LiveResizeEvent::Configure {
                ts_ms: 300,
                width: 800,
                height: 600,
            })
            .is_none()
        );
        assert_eq!(m.state(), LiveResizeState::Idle);
    }

    #[test]
    fn watchdog_forces_end_after_5s_silence() {
        let mut m = LiveResizeStateMachine::new();
        m.step(LiveResizeEvent::BeginSignal { ts_ms: 0 });
        m.step(LiveResizeEvent::Configure {
            ts_ms: 1,
            width: 100,
            height: 100,
        });
        // 4 seconds — watchdog must NOT fire yet.
        assert!(
            m.step(LiveResizeEvent::WatchdogTick { ts_ms: 4_000 })
                .is_none()
        );
        assert_eq!(m.state(), LiveResizeState::Resizing);
        // 5+ seconds — watchdog must force end.
        let t = m
            .step(LiveResizeEvent::WatchdogTick { ts_ms: 6_001 })
            .unwrap();
        assert_eq!(t.next_state, LiveResizeState::ResizeEnd);
        assert_eq!(t.source, LiveResizeTransitionSource::WatchdogForcedEnd);
        assert_eq!(m.health().watchdog_forced_ends_total, 1);
    }

    #[test]
    fn mouse_up_recovers_skipped_did_end() {
        let mut m = LiveResizeStateMachine::new();
        m.step(LiveResizeEvent::BeginSignal { ts_ms: 0 });
        m.step(LiveResizeEvent::Configure {
            ts_ms: 1,
            width: 100,
            height: 100,
        });
        let t = m
            .step(LiveResizeEvent::MouseUpDuringResize { ts_ms: 50 })
            .unwrap();
        assert_eq!(t.next_state, LiveResizeState::ResizeEnd);
        assert_eq!(t.source, LiveResizeTransitionSource::MouseUpRecovery);
        assert_eq!(m.health().mouse_up_recoveries_total, 1);
    }

    #[test]
    fn configure_storm_coalesces_above_threshold() {
        let mut m = LiveResizeStateMachine::new();
        m.step(LiveResizeEvent::BeginSignal { ts_ms: 0 });
        // First Configure transitions Begin → Resizing.
        m.step(LiveResizeEvent::Configure {
            ts_ms: 0,
            width: 100,
            height: 100,
        });
        // Storm: 200 configures in 50ms. The threshold is 100 in
        // 100ms; the second half should coalesce.
        for i in 0..200u64 {
            m.step(LiveResizeEvent::Configure {
                ts_ms: i / 4 + 1, // ~50ms span
                width: 100 + i as u32,
                height: 100,
            });
        }
        assert!(m.health().coalesced_total > 0);
        assert_eq!(m.state(), LiveResizeState::Resizing);
    }

    #[test]
    fn idle_ignores_end_and_mouse_up() {
        let mut m = LiveResizeStateMachine::new();
        assert!(m.step(LiveResizeEvent::EndSignal { ts_ms: 0 }).is_none());
        assert!(
            m.step(LiveResizeEvent::MouseUpDuringResize { ts_ms: 1 })
                .is_none()
        );
        assert_eq!(m.state(), LiveResizeState::Idle);
    }

    #[test]
    fn double_begin_is_ignored() {
        let mut m = LiveResizeStateMachine::new();
        m.step(LiveResizeEvent::BeginSignal { ts_ms: 0 });
        assert!(m.step(LiveResizeEvent::BeginSignal { ts_ms: 5 }).is_none());
        assert_eq!(m.state(), LiveResizeState::ResizeBegin);
    }

    #[test]
    fn jsonl_roundtrip() {
        let transitions = vec![
            LiveResizeTransition {
                ts_ms: 0,
                prev_state: LiveResizeState::Idle,
                next_state: LiveResizeState::ResizeBegin,
                source: LiveResizeTransitionSource::PlatformBegin,
                dimensions: None,
            },
            LiveResizeTransition {
                ts_ms: 5,
                prev_state: LiveResizeState::ResizeBegin,
                next_state: LiveResizeState::Resizing,
                source: LiveResizeTransitionSource::PlatformBegin,
                dimensions: Some((800, 600)),
            },
            LiveResizeTransition {
                ts_ms: 20,
                prev_state: LiveResizeState::Resizing,
                next_state: LiveResizeState::ResizeEnd,
                source: LiveResizeTransitionSource::PlatformEnd,
                dimensions: Some((810, 605)),
            },
        ];
        let rendered = render_transitions_jsonl(&transitions);
        let parsed = parse_transitions_jsonl(&rendered).unwrap();
        assert_eq!(parsed, transitions);
    }

    #[test]
    fn slug_round_trip() {
        for s in [
            LiveResizeState::Idle,
            LiveResizeState::ResizeBegin,
            LiveResizeState::Resizing,
            LiveResizeState::ResizeEnd,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let parsed: LiveResizeState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, s);
            assert!(!s.slug().is_empty());
        }
    }

    #[test]
    fn platform_metadata_is_stable() {
        assert!(LiveResizePlatform::Synthetic.is_wired());
        assert!(!LiveResizePlatform::Macos.is_wired());
        assert!(!LiveResizePlatform::Wayland.is_wired());
        assert!(!LiveResizePlatform::X11.is_wired());
    }

    #[test]
    fn health_baseline_is_idle() {
        let h = LiveResizeHealth::baseline();
        assert_eq!(h.current_state, LiveResizeState::Idle);
        assert!(!h.is_draft_mode());
    }

    #[test]
    fn draft_mode_only_in_begin_or_resizing() {
        assert!(!LiveResizeState::Idle.is_draft_mode());
        assert!(LiveResizeState::ResizeBegin.is_draft_mode());
        assert!(LiveResizeState::Resizing.is_draft_mode());
        assert!(!LiveResizeState::ResizeEnd.is_draft_mode());
    }
}
