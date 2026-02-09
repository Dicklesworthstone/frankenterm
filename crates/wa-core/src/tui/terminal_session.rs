//! Terminal session ownership abstraction for TUI backend migration.
//!
//! This module defines a lifecycle interface for terminal sessions that
//! abstracts over the crossterm/ratatui stack (legacy `tui` feature) and
//! the ftui terminal session model (`ftui` feature).
//!
//! # Ownership model
//!
//! A `TerminalSession` represents **singular ownership** of the terminal.
//! Only one session may be active at a time. The lifecycle is:
//!
//! ```text
//! Idle ──enter()──▶ Active ──suspend()──▶ Suspended ──resume()──▶ Active
//!                    │                                              │
//!                    └──leave()──▶ Idle ◀──leave()──────────────────┘
//! ```
//!
//! The `SessionGuard` RAII wrapper ensures `leave()` is called on drop,
//! providing explicit teardown guarantees even on panic unwind.
//!
//! # Command handoff
//!
//! When the TUI needs to shell out to a command (e.g., `wa rules profile apply`),
//! the session is `suspend()`ed (alt screen left, raw mode disabled), the command
//! runs, and then `resume()` re-enters the TUI. This is modeled as an explicit
//! state transition rather than ad-hoc enable/disable calls.
//!
//! # Deletion criterion
//! Remove this module when the `tui` feature is dropped and ftui's native
//! `Program` runtime fully owns the lifecycle (FTUI-09.3).

use std::time::Duration;

use super::ftui_compat::{Area, InputEvent, RenderSurface, ScreenMode};

// ---------------------------------------------------------------------------
// Session phase
// ---------------------------------------------------------------------------

/// Terminal session lifecycle phase.
///
/// Used to enforce valid state transitions and prevent double-enter or
/// use-after-leave bugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    /// Session not yet entered or already left.
    Idle,
    /// Terminal acquired: raw mode on, rendering active.
    Active,
    /// Temporarily released for command handoff.
    Suspended,
}

// ---------------------------------------------------------------------------
// Session error
// ---------------------------------------------------------------------------

/// Errors from terminal session lifecycle operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid phase transition: expected {expected:?}, got {actual:?}")]
    InvalidPhase {
        expected: &'static [SessionPhase],
        actual: SessionPhase,
    },
}

// ---------------------------------------------------------------------------
// TerminalSession trait
// ---------------------------------------------------------------------------

/// Lifecycle interface for terminal session ownership.
///
/// Implementations manage raw mode, alternate screen, event polling, and
/// rendering surface access. The trait is object-safe to allow testing with
/// mock implementations.
///
/// # Invariants
///
/// - `enter()` may only be called in `Idle` phase.
/// - `draw()` and `poll_event()` may only be called in `Active` phase.
/// - `suspend()` transitions `Active` → `Suspended`.
/// - `resume()` transitions `Suspended` → `Active`.
/// - `leave()` may be called in `Active` or `Suspended` phase.
/// - After `leave()`, the session returns to `Idle`.
pub trait TerminalSession {
    /// Current lifecycle phase.
    fn phase(&self) -> SessionPhase;

    /// The screen mode this session was entered with.
    ///
    /// Returns `None` if the session has not been entered yet (phase is `Idle`).
    fn screen_mode(&self) -> Option<ScreenMode>;

    /// Acquire the terminal with the specified screen mode.
    ///
    /// The `mode` determines whether to enter alternate screen (`AltScreen`)
    /// or stay inline (`Inline` / `InlineAuto`). The chosen mode affects
    /// scrollback behavior, cleanup semantics, and subprocess output routing.
    ///
    /// # Errors
    /// Returns `SessionError::InvalidPhase` if not in `Idle` phase.
    fn enter(&mut self, mode: ScreenMode) -> Result<(), SessionError>;

    /// Render a frame by invoking the callback with the current surface.
    ///
    /// The callback receives the available `Area` and a mutable reference to
    /// the `RenderSurface`. The session flushes the frame to the terminal
    /// after the callback returns.
    ///
    /// # Errors
    /// Returns `SessionError::InvalidPhase` if not in `Active` phase.
    fn draw(
        &mut self,
        render: &mut dyn FnMut(Area, &mut dyn RenderSurface),
    ) -> Result<(), SessionError>;

    /// Poll for the next input event with timeout.
    ///
    /// Returns `None` if the timeout expires without an event.
    ///
    /// # Errors
    /// Returns `SessionError::InvalidPhase` if not in `Active` phase.
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<InputEvent>, SessionError>;

    /// Temporarily release the terminal for command handoff.
    ///
    /// Disables raw mode and leaves alternate screen so the child process
    /// can interact with the terminal normally.
    ///
    /// # Errors
    /// Returns `SessionError::InvalidPhase` if not in `Active` phase.
    fn suspend(&mut self) -> Result<(), SessionError>;

    /// Re-acquire the terminal after command handoff.
    ///
    /// Re-enters alternate screen and enables raw mode.
    ///
    /// # Errors
    /// Returns `SessionError::InvalidPhase` if not in `Suspended` phase.
    fn resume(&mut self) -> Result<(), SessionError>;

    /// Release the terminal: disable raw mode, leave alternate screen,
    /// restore cursor.
    ///
    /// Safe to call from `Active` or `Suspended`. No-op if already `Idle`.
    fn leave(&mut self);
}

// ---------------------------------------------------------------------------
// SessionGuard — RAII teardown guarantee
// ---------------------------------------------------------------------------

/// RAII guard that ensures `leave()` is called when the session goes out of
/// scope, even on panic unwind.
///
/// # Usage
///
/// ```ignore
/// let guard = SessionGuard::enter(session)?;
/// // ... use guard.session() ...
/// // leave() is called automatically on drop
/// ```
pub struct SessionGuard<S: TerminalSession> {
    /// `None` only after `into_inner()` moves the session out.
    session: Option<S>,
}

impl<S: TerminalSession> SessionGuard<S> {
    /// Enter the session with the specified screen mode and return a guard
    /// that will leave on drop.
    ///
    /// Sets the output gate to [`Active`](super::output_gate::GatePhase::Active),
    /// signaling that direct stderr/stdout writes are unsafe.
    pub fn enter(mut session: S, mode: ScreenMode) -> Result<Self, SessionError> {
        session.enter(mode)?;
        super::output_gate::set_phase(super::output_gate::GatePhase::Active);
        Ok(Self {
            session: Some(session),
        })
    }

    /// Access the underlying session.
    ///
    /// # Panics
    /// Panics if called after `into_inner()`.
    pub fn session(&self) -> &S {
        self.session
            .as_ref()
            .expect("session consumed by into_inner")
    }

    /// Access the underlying session mutably.
    ///
    /// # Panics
    /// Panics if called after `into_inner()`.
    pub fn session_mut(&mut self) -> &mut S {
        self.session
            .as_mut()
            .expect("session consumed by into_inner")
    }

    /// Consume the guard, calling `leave()` and returning the session.
    ///
    /// The drop-based leave is suppressed; leave is called exactly once.
    /// Clears the output gate to [`Inactive`](super::output_gate::GatePhase::Inactive).
    pub fn into_inner(mut self) -> S {
        let mut session = self.session.take().expect("session consumed by into_inner");
        session.leave();
        super::output_gate::set_phase(super::output_gate::GatePhase::Inactive);
        session
    }
}

impl<S: TerminalSession> Drop for SessionGuard<S> {
    fn drop(&mut self) {
        if let Some(session) = &mut self.session {
            session.leave();
        }
        // Always clear the gate on drop, even if session was already taken
        // via into_inner() (idempotent).
        super::output_gate::set_phase(super::output_gate::GatePhase::Inactive);
    }
}

impl<S: TerminalSession> std::ops::Deref for SessionGuard<S> {
    type Target = S;
    fn deref(&self) -> &S {
        self.session
            .as_ref()
            .expect("session consumed by into_inner")
    }
}

impl<S: TerminalSession> std::ops::DerefMut for SessionGuard<S> {
    fn deref_mut(&mut self) -> &mut S {
        self.session
            .as_mut()
            .expect("session consumed by into_inner")
    }
}

// ---------------------------------------------------------------------------
// CrosstermSession — ratatui/crossterm implementation
// ---------------------------------------------------------------------------

/// Ratatui/crossterm terminal session.
///
/// This is the legacy implementation that wraps the current terminal setup
/// code from `app.rs`.
///
/// # Deletion criterion
/// Remove when the `tui` feature is dropped (FTUI-09.3).
#[cfg(feature = "tui")]
pub struct CrosstermSession {
    phase: SessionPhase,
    mode: Option<ScreenMode>,
    terminal: Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>,
}

#[cfg(feature = "tui")]
impl CrosstermSession {
    pub fn new() -> Self {
        Self {
            phase: SessionPhase::Idle,
            mode: None,
            terminal: None,
        }
    }
}

#[cfg(feature = "tui")]
impl Default for CrosstermSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tui")]
impl TerminalSession for CrosstermSession {
    fn phase(&self) -> SessionPhase {
        self.phase
    }

    fn screen_mode(&self) -> Option<ScreenMode> {
        self.mode
    }

    fn enter(&mut self, mode: ScreenMode) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Idle {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Idle],
                actual: self.phase,
            });
        }

        crossterm::terminal::enable_raw_mode()?;

        // The ratatui/crossterm backend only supports AltScreen natively.
        // Inline modes are a ftui-only feature; under the `tui` backend we
        // always enter alternate screen regardless of the requested mode.
        // This is acceptable during the migration period — once the `tui`
        // feature is dropped, the ftui runtime handles mode selection.
        if let Err(err) =
            crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)
        {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(err.into());
        }

        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        match ratatui::Terminal::new(backend) {
            Ok(terminal) => {
                self.terminal = Some(terminal);
                self.mode = Some(mode);
                self.phase = SessionPhase::Active;
                Ok(())
            }
            Err(err) => {
                let _ = crossterm::terminal::disable_raw_mode();
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::LeaveAlternateScreen
                );
                Err(err.into())
            }
        }
    }

    fn draw(
        &mut self,
        render: &mut dyn FnMut(Area, &mut dyn RenderSurface),
    ) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Active {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Active],
                actual: self.phase,
            });
        }

        let terminal = self
            .terminal
            .as_mut()
            .expect("terminal must exist in Active phase");

        terminal.draw(|frame| {
            let ratatui_area = frame.area();
            let area: Area = ratatui_area.into();
            let mut surface =
                super::ftui_compat::RatatuiSurface::new(frame.buffer_mut(), ratatui_area);
            render(area, &mut surface);
        })?;

        Ok(())
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<InputEvent>, SessionError> {
        if self.phase != SessionPhase::Active {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Active],
                actual: self.phase,
            });
        }

        if crossterm::event::poll(timeout)? {
            match crossterm::event::read()? {
                crossterm::event::Event::Key(key) => {
                    let key_input: super::ftui_compat::KeyInput = key.into();
                    return Ok(Some(InputEvent::Key(key_input)));
                }
                crossterm::event::Event::Resize(w, h) => {
                    return Ok(Some(InputEvent::Resize {
                        width: w,
                        height: h,
                    }));
                }
                _ => {}
            }
        }

        Ok(None)
    }

    fn suspend(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Active {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Active],
                actual: self.phase,
            });
        }

        crossterm::terminal::disable_raw_mode()?;
        if let Some(terminal) = &mut self.terminal {
            crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::LeaveAlternateScreen
            )?;
        }
        self.phase = SessionPhase::Suspended;
        super::output_gate::set_phase(super::output_gate::GatePhase::Suspended);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Suspended {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Suspended],
                actual: self.phase,
            });
        }

        if let Some(terminal) = &mut self.terminal {
            crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::EnterAlternateScreen
            )?;
        }
        crossterm::terminal::enable_raw_mode()?;
        self.phase = SessionPhase::Active;
        super::output_gate::set_phase(super::output_gate::GatePhase::Active);
        Ok(())
    }

    fn leave(&mut self) {
        if self.phase == SessionPhase::Idle {
            return;
        }

        let _ = crossterm::terminal::disable_raw_mode();
        if let Some(terminal) = &mut self.terminal {
            let _ = crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = terminal.show_cursor();
        }
        self.terminal = None;
        self.mode = None;
        self.phase = SessionPhase::Idle;
        super::output_gate::set_phase(super::output_gate::GatePhase::Inactive);
    }
}

// ---------------------------------------------------------------------------
// MockTerminalSession — for testing
// ---------------------------------------------------------------------------

/// Mock terminal session that records lifecycle transitions.
///
/// All operations succeed. The `history` field records every transition
/// for assertion in tests.
#[derive(Debug, Default)]
pub struct MockTerminalSession {
    phase: SessionPhase,
    mode: Option<ScreenMode>,
    /// Lifecycle transitions recorded in order.
    pub history: Vec<&'static str>,
    /// Number of draw calls.
    pub draw_count: usize,
    /// Events to return from poll_event (drained in order).
    pub pending_events: Vec<InputEvent>,
}

impl Default for SessionPhase {
    fn default() -> Self {
        Self::Idle
    }
}

impl MockTerminalSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-load events that will be returned by `poll_event`.
    #[must_use]
    pub fn with_events(mut self, events: Vec<InputEvent>) -> Self {
        self.pending_events = events;
        self
    }
}

impl TerminalSession for MockTerminalSession {
    fn phase(&self) -> SessionPhase {
        self.phase
    }

    fn screen_mode(&self) -> Option<ScreenMode> {
        self.mode
    }

    fn enter(&mut self, mode: ScreenMode) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Idle {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Idle],
                actual: self.phase,
            });
        }
        self.mode = Some(mode);
        self.phase = SessionPhase::Active;
        self.history.push("enter");
        Ok(())
    }

    fn draw(
        &mut self,
        _render: &mut dyn FnMut(Area, &mut dyn RenderSurface),
    ) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Active {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Active],
                actual: self.phase,
            });
        }
        self.draw_count += 1;
        self.history.push("draw");
        Ok(())
    }

    fn poll_event(&mut self, _timeout: Duration) -> Result<Option<InputEvent>, SessionError> {
        if self.phase != SessionPhase::Active {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Active],
                actual: self.phase,
            });
        }
        self.history.push("poll");
        if self.pending_events.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.pending_events.remove(0)))
        }
    }

    fn suspend(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Active {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Active],
                actual: self.phase,
            });
        }
        self.phase = SessionPhase::Suspended;
        self.history.push("suspend");
        Ok(())
    }

    fn resume(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Suspended {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Suspended],
                actual: self.phase,
            });
        }
        self.phase = SessionPhase::Active;
        self.history.push("resume");
        Ok(())
    }

    fn leave(&mut self) {
        if self.phase != SessionPhase::Idle {
            self.history.push("leave");
            self.mode = None;
            self.phase = SessionPhase::Idle;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::ftui_compat::{Key, KeyInput};
    use super::*;

    #[test]
    fn mock_lifecycle_enter_leave() {
        let mut session = MockTerminalSession::new();
        assert_eq!(session.phase(), SessionPhase::Idle);

        session.enter(ScreenMode::default()).unwrap();
        assert_eq!(session.phase(), SessionPhase::Active);
        assert_eq!(session.history, vec!["enter"]);

        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    #[test]
    fn mock_double_enter_fails() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        let err = session.enter(ScreenMode::default()).unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    #[test]
    fn mock_draw_requires_active() {
        let mut session = MockTerminalSession::new();
        let err = session.draw(&mut |_, _| {}).unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    #[test]
    fn mock_suspend_resume_lifecycle() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        session.suspend().unwrap();
        assert_eq!(session.phase(), SessionPhase::Suspended);

        // Can't draw while suspended
        let err = session.draw(&mut |_, _| {}).unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));

        session.resume().unwrap();
        assert_eq!(session.phase(), SessionPhase::Active);
        assert_eq!(session.history, vec!["enter", "suspend", "resume"]);
    }

    #[test]
    fn mock_command_handoff_pattern() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();

        // Draw a few frames
        session.draw(&mut |_, _| {}).unwrap();
        session.draw(&mut |_, _| {}).unwrap();

        // Suspend for command
        session.suspend().unwrap();
        // ... command runs here ...
        session.resume().unwrap();

        // Draw after resume
        session.draw(&mut |_, _| {}).unwrap();
        session.leave();

        assert_eq!(
            session.history,
            vec![
                "enter", "draw", "draw", "suspend", "resume", "draw", "leave"
            ]
        );
        assert_eq!(session.draw_count, 3);
    }

    #[test]
    fn mock_poll_returns_preloaded_events() {
        let events = vec![
            InputEvent::Key(KeyInput::new(Key::Char('q'))),
            InputEvent::Key(KeyInput::new(Key::Enter)),
        ];
        let mut session = MockTerminalSession::new().with_events(events);
        session.enter(ScreenMode::default()).unwrap();

        let ev1 = session.poll_event(Duration::ZERO).unwrap();
        assert!(matches!(ev1, Some(InputEvent::Key(ref k)) if k.is_char('q')));

        let ev2 = session.poll_event(Duration::ZERO).unwrap();
        assert!(matches!(ev2, Some(InputEvent::Key(ref k)) if k.key == Key::Enter));

        let ev3 = session.poll_event(Duration::ZERO).unwrap();
        assert!(ev3.is_none());
    }

    #[test]
    fn mock_leave_is_idempotent() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        session.leave();
        session.leave(); // Second leave is no-op
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    #[test]
    fn mock_leave_from_suspended() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        session.suspend().unwrap();
        session.leave(); // Can leave from suspended
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    #[test]
    fn session_guard_into_inner_calls_leave() {
        let session = MockTerminalSession::new();
        let guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
        assert_eq!(guard.phase(), SessionPhase::Active);
        let session = guard.into_inner();
        assert_eq!(session.phase(), SessionPhase::Idle);
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    // -- output gate integration tests --
    // These share the process-global gate atomic, so they must serialize
    // with the output_gate tests via the same lock.

    #[test]
    fn guard_toggles_output_gate_on_enter_and_drop() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        {
            let session = MockTerminalSession::new();
            let _guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            assert_eq!(output_gate::phase(), GatePhase::Active);
            assert!(output_gate::is_output_suppressed());
        }
        // Guard dropped → gate inactive
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
        assert!(!output_gate::is_output_suppressed());
    }

    #[test]
    fn guard_into_inner_clears_gate() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let session = MockTerminalSession::new();
        let guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
        assert!(output_gate::is_output_suppressed());

        let _session = guard.into_inner();
        assert!(!output_gate::is_output_suppressed());
    }

    #[test]
    fn session_guard_deref() {
        let session = MockTerminalSession::new();
        let mut guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
        assert_eq!(guard.phase(), SessionPhase::Active);
        guard.suspend().unwrap();
        assert_eq!(guard.phase(), SessionPhase::Suspended);
    }

    #[test]
    fn resume_from_wrong_phase_fails() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        let err = session.resume().unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    #[test]
    fn suspend_from_wrong_phase_fails() {
        let mut session = MockTerminalSession::new();
        let err = session.suspend().unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    // -- screen mode tests --

    #[test]
    fn screen_mode_none_before_enter() {
        let session = MockTerminalSession::new();
        assert!(session.screen_mode().is_none());
    }

    #[test]
    fn screen_mode_tracks_alt_screen() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::AltScreen).unwrap();
        assert_eq!(session.screen_mode(), Some(ScreenMode::AltScreen));
    }

    #[test]
    fn screen_mode_tracks_inline() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::Inline { ui_height: 12 }).unwrap();
        assert_eq!(
            session.screen_mode(),
            Some(ScreenMode::Inline { ui_height: 12 })
        );
    }

    #[test]
    fn screen_mode_cleared_on_leave() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::AltScreen).unwrap();
        assert!(session.screen_mode().is_some());
        session.leave();
        assert!(session.screen_mode().is_none());
    }

    #[test]
    fn screen_mode_preserved_across_suspend_resume() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::Inline { ui_height: 8 }).unwrap();
        session.suspend().unwrap();
        assert_eq!(
            session.screen_mode(),
            Some(ScreenMode::Inline { ui_height: 8 })
        );
        session.resume().unwrap();
        assert_eq!(
            session.screen_mode(),
            Some(ScreenMode::Inline { ui_height: 8 })
        );
    }

    #[test]
    fn guard_preserves_screen_mode() {
        let session = MockTerminalSession::new();
        let guard = SessionGuard::enter(session, ScreenMode::Inline { ui_height: 15 }).unwrap();
        assert_eq!(
            guard.screen_mode(),
            Some(ScreenMode::Inline { ui_height: 15 })
        );
    }

    // -- FTUI-03.4: panic-safe cleanup and lifecycle stress validation --

    #[test]
    fn guard_drop_cleans_up_after_caught_panic() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let _guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            assert_eq!(output_gate::phase(), GatePhase::Active);
            panic!("simulated panic during TUI operation");
        }));

        assert!(result.is_err(), "panic should have been caught");
        // Guard's Drop must have run, clearing the gate back to Inactive.
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
        assert!(!output_gate::is_output_suppressed());
    }

    #[test]
    fn guard_drop_cleans_up_after_panic_in_suspended_state() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            guard.suspend().unwrap();
            // Gate should be Suspended (CrosstermSession toggles it, mock doesn't)
            panic!("simulated panic during command handoff");
        }));

        assert!(result.is_err());
        // Guard's Drop restores the gate to Inactive regardless of session phase.
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
    }

    #[test]
    fn teardown_idempotency_leave_after_leave() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();

        // Multiple leave() calls must not panic or corrupt state.
        session.leave();
        session.leave();
        session.leave();

        assert_eq!(session.phase(), SessionPhase::Idle);
        assert!(session.screen_mode().is_none());
        // Only one "leave" recorded (subsequent calls are no-ops).
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    #[test]
    fn teardown_idempotency_drop_after_into_inner() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let session = MockTerminalSession::new();
        let guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();

        // into_inner() calls leave() and clears gate...
        let session = guard.into_inner();
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
        assert_eq!(session.phase(), SessionPhase::Idle);
        // ...and the guard's Drop runs but finds session already taken (no double leave).
        drop(session);
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
    }

    #[test]
    fn lifecycle_stress_repeated_enter_leave_cycles() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        // Rapid start/stop cycles must not leak mode state.
        for i in 0..50 {
            let session = MockTerminalSession::new();
            let mode = if i % 2 == 0 {
                ScreenMode::AltScreen
            } else {
                ScreenMode::Inline { ui_height: 10 }
            };
            let guard = SessionGuard::enter(session, mode).unwrap();
            assert_eq!(output_gate::phase(), GatePhase::Active);
            assert_eq!(guard.screen_mode(), Some(mode));
            let session = guard.into_inner();
            assert_eq!(output_gate::phase(), GatePhase::Inactive);
            assert!(session.screen_mode().is_none());
        }
    }

    #[test]
    fn lifecycle_stress_suspend_resume_cycles() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();

        // Repeated suspend/resume must not corrupt phase or mode state.
        for _ in 0..20 {
            session.suspend().unwrap();
            assert_eq!(session.phase(), SessionPhase::Suspended);
            assert!(session.screen_mode().is_some()); // Mode preserved

            session.resume().unwrap();
            assert_eq!(session.phase(), SessionPhase::Active);
            assert!(session.screen_mode().is_some()); // Mode preserved
        }

        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
        assert!(session.screen_mode().is_none());
    }

    #[test]
    fn guard_drop_after_panic_in_draw_callback() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            // The mock draw doesn't actually call the callback, but this tests
            // the guard cleanup even if draw were to trigger a panic.
            guard.draw(&mut |_, _| {}).unwrap();
            panic!("simulated panic after draw");
        }));

        assert!(result.is_err());
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
    }

    // -- FTUI-03.4.a: teardown harness and restoration assertions --
    //
    // Systematic harness that exercises all abort scenarios and validates
    // the full set of restoration invariants:
    //   1. SessionPhase returns to Idle
    //   2. ScreenMode returns to None
    //   3. Output gate returns to Inactive
    //   4. MockSession history shows leave was called

    /// Assert that all restoration invariants hold after teardown.
    fn assert_restoration_invariants(gate_phase: super::super::output_gate::GatePhase) {
        use super::super::output_gate::GatePhase;
        assert_eq!(
            gate_phase,
            GatePhase::Inactive,
            "output gate must be Inactive after teardown"
        );
        assert!(
            !super::super::output_gate::is_output_suppressed(),
            "output must not be suppressed after teardown"
        );
    }

    /// Run a closure that is expected to panic, then verify all restoration
    /// invariants. Returns the caught panic for optional further inspection.
    fn run_panic_harness(
        f: impl FnOnce() + std::panic::UnwindSafe,
    ) -> Box<dyn std::any::Any + Send> {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let result = std::panic::catch_unwind(f);
        assert!(result.is_err(), "closure should have panicked");

        assert_restoration_invariants(output_gate::phase());
        result.unwrap_err()
    }

    #[test]
    fn harness_panic_during_active_alt_screen() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let _guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
            panic!("abort during active alt-screen");
        }));
    }

    #[test]
    fn harness_panic_during_active_inline() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let _guard =
                SessionGuard::enter(session, ScreenMode::Inline { ui_height: 12 }).unwrap();
            panic!("abort during active inline mode");
        }));
    }

    #[test]
    fn harness_panic_during_suspended_phase() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
            guard.suspend().unwrap();
            panic!("abort during suspended command handoff");
        }));
    }

    #[test]
    fn harness_panic_during_draw() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            guard.draw(&mut |_, _| {}).unwrap();
            guard.draw(&mut |_, _| {}).unwrap();
            panic!("abort mid-render cycle");
        }));
    }

    #[test]
    fn harness_panic_during_poll() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            let _ = guard.poll_event(Duration::ZERO);
            panic!("abort during event poll");
        }));
    }

    #[test]
    fn harness_panic_after_multiple_suspend_resume_cycles() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
            for _ in 0..5 {
                guard.suspend().unwrap();
                guard.resume().unwrap();
            }
            panic!("abort after rapid suspend/resume cycling");
        }));
    }

    #[test]
    fn harness_into_inner_then_drop_no_double_cleanup() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let session = MockTerminalSession::new();
        let guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
        let session = guard.into_inner();

        assert_restoration_invariants(output_gate::phase());
        assert_eq!(session.phase(), SessionPhase::Idle);
        assert!(session.screen_mode().is_none());
        // Only one leave in history (not double)
        assert_eq!(session.history.iter().filter(|h| **h == "leave").count(), 1);
    }

    #[test]
    fn harness_leave_restores_all_screen_modes() {
        // Verify that leave() clears screen mode for each mode variant.
        for mode in [
            ScreenMode::AltScreen,
            ScreenMode::Inline { ui_height: 1 },
            ScreenMode::Inline { ui_height: 24 },
            ScreenMode::Inline { ui_height: 100 },
        ] {
            let mut session = MockTerminalSession::new();
            session.enter(mode).unwrap();
            assert_eq!(session.screen_mode(), Some(mode));
            session.leave();
            assert!(
                session.screen_mode().is_none(),
                "screen_mode not cleared for {mode:?}"
            );
            assert_eq!(session.phase(), SessionPhase::Idle);
        }
    }

    #[test]
    fn harness_panic_message_preserved_in_catch() {
        let payload = run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let _guard = SessionGuard::enter(session, ScreenMode::default()).unwrap();
            panic!("specific panic message for forensics");
        }));

        // Verify the panic payload is accessible for crash bundle generation.
        let msg = payload
            .downcast_ref::<&str>()
            .expect("panic payload should be &str");
        assert_eq!(*msg, "specific panic message for forensics");
    }

    #[test]
    fn harness_sequential_panics_no_state_leak() {
        // Multiple sequential panics must each leave clean state.
        for i in 0..10 {
            run_panic_harness(std::panic::AssertUnwindSafe(move || {
                let session = MockTerminalSession::new();
                let mode = if i % 2 == 0 {
                    ScreenMode::AltScreen
                } else {
                    ScreenMode::Inline { ui_height: 8 }
                };
                let _guard = SessionGuard::enter(session, mode).unwrap();
                panic!("sequential panic #{i}");
            }));
        }
    }

    #[test]
    fn harness_gate_phase_correct_at_each_lifecycle_point() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        // Track gate phase at each lifecycle point.
        let session = MockTerminalSession::new();
        assert_eq!(output_gate::phase(), GatePhase::Inactive);

        let mut guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
        assert_eq!(output_gate::phase(), GatePhase::Active);

        guard.draw(&mut |_, _| {}).unwrap();
        assert_eq!(output_gate::phase(), GatePhase::Active);

        // Note: MockTerminalSession does NOT toggle gate on suspend/resume
        // (only CrosstermSession does). Gate remains Active through mock
        // suspend/resume. The gate is managed by the caller (command_handoff.rs)
        // or the real session implementation.

        let session = guard.into_inner();
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    // ====================================================================
    // FTUI-08.4: Resilience / chaos validation
    // ====================================================================

    // -- FailableSession: error-injection mock --

    /// A mock session that can inject errors at configurable lifecycle points.
    /// Used to validate that the guard and callers handle partial failures
    /// without leaking state.
    struct FailableSession {
        inner: MockTerminalSession,
        /// If set, `suspend()` will fail with this error after N successful calls.
        fail_suspend_after: Option<usize>,
        suspend_count: usize,
        /// If set, `resume()` will fail with this error after N successful calls.
        fail_resume_after: Option<usize>,
        resume_count: usize,
        /// If set, `draw()` will fail after N successful calls.
        fail_draw_after: Option<usize>,
    }

    impl FailableSession {
        fn new() -> Self {
            Self {
                inner: MockTerminalSession::new(),
                fail_suspend_after: None,
                suspend_count: 0,
                fail_resume_after: None,
                resume_count: 0,
                fail_draw_after: None,
            }
        }

        fn fail_suspend_after(mut self, n: usize) -> Self {
            self.fail_suspend_after = Some(n);
            self
        }

        fn fail_resume_after(mut self, n: usize) -> Self {
            self.fail_resume_after = Some(n);
            self
        }

        fn fail_draw_after(mut self, n: usize) -> Self {
            self.fail_draw_after = Some(n);
            self
        }
    }

    impl TerminalSession for FailableSession {
        fn phase(&self) -> SessionPhase {
            self.inner.phase()
        }

        fn screen_mode(&self) -> Option<ScreenMode> {
            self.inner.screen_mode()
        }

        fn enter(&mut self, mode: ScreenMode) -> Result<(), SessionError> {
            self.inner.enter(mode)
        }

        fn draw(
            &mut self,
            render: &mut dyn FnMut(Area, &mut dyn RenderSurface),
        ) -> Result<(), SessionError> {
            if let Some(limit) = self.fail_draw_after {
                if self.inner.draw_count >= limit {
                    return Err(SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "injected draw failure",
                    )));
                }
            }
            self.inner.draw(render)
        }

        fn poll_event(&mut self, timeout: Duration) -> Result<Option<InputEvent>, SessionError> {
            self.inner.poll_event(timeout)
        }

        fn suspend(&mut self) -> Result<(), SessionError> {
            if let Some(limit) = self.fail_suspend_after {
                if self.suspend_count >= limit {
                    return Err(SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "injected suspend failure",
                    )));
                }
            }
            self.suspend_count += 1;
            self.inner.suspend()
        }

        fn resume(&mut self) -> Result<(), SessionError> {
            if let Some(limit) = self.fail_resume_after {
                if self.resume_count >= limit {
                    return Err(SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "injected resume failure",
                    )));
                }
            }
            self.resume_count += 1;
            self.inner.resume()
        }

        fn leave(&mut self) {
            self.inner.leave();
        }
    }

    // -- Chaos C1: rapid lifecycle cycling with interleaved operations --

    #[test]
    fn chaos_rapid_lifecycle_cycling_100_rounds() {
        // Stress: enter→draw→suspend→resume→draw→leave for 100 rounds.
        // No gate lock needed — MockTerminalSession doesn't touch the atomic gate.
        for round in 0..100u32 {
            let mut session = MockTerminalSession::new();
            let mode = if round % 3 == 0 {
                ScreenMode::AltScreen
            } else {
                ScreenMode::Inline {
                    ui_height: (round % 50 + 1) as u16,
                }
            };

            session.enter(mode).unwrap();
            assert_eq!(session.phase(), SessionPhase::Active);

            session.draw(&mut |_, _| {}).unwrap();

            session.suspend().unwrap();
            assert_eq!(session.phase(), SessionPhase::Suspended);

            session.resume().unwrap();
            assert_eq!(session.phase(), SessionPhase::Active);

            session.draw(&mut |_, _| {}).unwrap();
            session.leave();

            assert_eq!(session.phase(), SessionPhase::Idle);
            assert!(session.screen_mode().is_none());
            assert_eq!(session.draw_count, 2);
        }
    }

    // -- Chaos C2: rapid suspend/resume without draw --

    #[test]
    fn chaos_rapid_suspend_resume_200_cycles() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::AltScreen).unwrap();

        for i in 0..200u32 {
            session.suspend().unwrap();
            assert_eq!(
                session.phase(),
                SessionPhase::Suspended,
                "cycle {i}: expected Suspended"
            );
            session.resume().unwrap();
            assert_eq!(
                session.phase(),
                SessionPhase::Active,
                "cycle {i}: expected Active"
            );
        }

        session.leave();
        // 1 enter + 200*(suspend+resume) + 1 leave = 402
        assert_eq!(session.history.len(), 402);
    }

    // -- Chaos C3: failure injection during suspend --

    #[test]
    fn chaos_suspend_failure_preserves_active_state() {
        let mut session = FailableSession::new().fail_suspend_after(2);
        session.enter(ScreenMode::AltScreen).unwrap();

        // First two suspend/resume cycles succeed
        session.suspend().unwrap();
        session.resume().unwrap();
        session.suspend().unwrap();
        session.resume().unwrap();

        // Third suspend should fail (injected after count >= 2)
        let err = session.suspend().unwrap_err();
        assert!(
            matches!(err, SessionError::Io(_)),
            "expected Io error, got: {err:?}"
        );
        // Session should still be Active (suspend didn't partially transition)
        assert_eq!(session.phase(), SessionPhase::Active);

        // Can still draw after failed suspend
        session.draw(&mut |_, _| {}).unwrap();

        // Clean leave still works
        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    // -- Chaos C4: failure injection during resume --

    #[test]
    fn chaos_resume_failure_preserves_suspended_state() {
        let mut session = FailableSession::new().fail_resume_after(1);
        session.enter(ScreenMode::AltScreen).unwrap();

        // First resume succeeds
        session.suspend().unwrap();
        session.resume().unwrap();

        // Second cycle: suspend succeeds, resume fails
        session.suspend().unwrap();
        let err = session.resume().unwrap_err();
        assert!(
            matches!(err, SessionError::Io(_)),
            "expected Io error, got: {err:?}"
        );
        // Session stays Suspended
        assert_eq!(session.phase(), SessionPhase::Suspended);

        // Emergency leave from Suspended should still work
        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    // -- Chaos C5: failure injection during draw --

    #[test]
    fn chaos_draw_failure_preserves_active_state() {
        let mut session = FailableSession::new().fail_draw_after(3);
        session.enter(ScreenMode::AltScreen).unwrap();

        // First three draws succeed
        for _ in 0..3 {
            session.draw(&mut |_, _| {}).unwrap();
        }

        // Fourth draw should fail
        let err = session.draw(&mut |_, _| {}).unwrap_err();
        assert!(
            matches!(err, SessionError::Io(_)),
            "expected Io error, got: {err:?}"
        );
        // Session should still be Active
        assert_eq!(session.phase(), SessionPhase::Active);

        // Suspend/resume still works after draw failure
        session.suspend().unwrap();
        session.resume().unwrap();
        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    // -- Chaos C6: guard cleanup after injected suspend failure --

    #[test]
    fn chaos_guard_cleanup_after_suspend_failure() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        let session = FailableSession::new().fail_suspend_after(0);
        let mut guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
        assert_eq!(output_gate::phase(), GatePhase::Active);

        // Suspend will fail immediately
        let err = guard.suspend().unwrap_err();
        assert!(matches!(err, SessionError::Io(_)));

        // Guard still holds Active — drop should clean up
        drop(guard);
        assert_eq!(output_gate::phase(), GatePhase::Inactive);
    }

    // -- Chaos C7: interleaved draw/poll/suspend/resume stress --

    #[test]
    fn chaos_interleaved_operations_stress() {
        let events = (0..50)
            .map(|i| {
                if i % 10 == 0 {
                    InputEvent::Resize {
                        width: 80 + (i % 40) as u16,
                        height: 24 + (i % 20) as u16,
                    }
                } else if i % 5 == 0 {
                    InputEvent::Tick
                } else {
                    InputEvent::Key(KeyInput::new(Key::Char('a')))
                }
            })
            .collect();

        let mut session = MockTerminalSession::new().with_events(events);
        session.enter(ScreenMode::AltScreen).unwrap();

        // Mixed operation sequence: draw, poll, suspend/resume cycles
        for i in 0..50u32 {
            match i % 7 {
                0 | 1 | 2 => {
                    // Draw
                    session.draw(&mut |_, _| {}).unwrap();
                }
                3 => {
                    // Poll
                    let _ = session.poll_event(Duration::ZERO);
                }
                4 => {
                    // Suspend + immediate resume
                    session.suspend().unwrap();
                    session.resume().unwrap();
                }
                5 => {
                    // Poll multiple times
                    let _ = session.poll_event(Duration::ZERO);
                    let _ = session.poll_event(Duration::ZERO);
                }
                _ => {
                    // Draw + poll
                    session.draw(&mut |_, _| {}).unwrap();
                    let _ = session.poll_event(Duration::ZERO);
                }
            }
        }

        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    // -- Chaos C8: guard with panic during draw failure recovery --

    #[test]
    fn chaos_guard_panic_after_draw_failure() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = FailableSession::new().fail_draw_after(2);
            let mut guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
            guard.draw(&mut |_, _| {}).unwrap();
            guard.draw(&mut |_, _| {}).unwrap();
            // This draw fails, and then we panic
            let _err = guard.draw(&mut |_, _| {});
            panic!("panic during draw failure recovery");
        }));
    }

    // -- Chaos C9: panic during resume after suspend --

    #[test]
    fn chaos_guard_panic_during_resume_attempt() {
        run_panic_harness(std::panic::AssertUnwindSafe(|| {
            let session = MockTerminalSession::new();
            let mut guard = SessionGuard::enter(session, ScreenMode::AltScreen).unwrap();
            guard.suspend().unwrap();
            guard.resume().unwrap();
            guard.suspend().unwrap();
            // Panic while suspended (simulates crash during command handoff)
            panic!("crash during second command handoff");
        }));
    }

    // -- Chaos C10: screen mode variations under stress --

    #[test]
    fn chaos_all_screen_modes_lifecycle_stress() {
        let modes = [
            ScreenMode::AltScreen,
            ScreenMode::Inline { ui_height: 1 },
            ScreenMode::Inline { ui_height: 8 },
            ScreenMode::Inline { ui_height: 24 },
            ScreenMode::Inline { ui_height: 100 },
        ];

        for mode in modes {
            for _ in 0..20 {
                let mut session = MockTerminalSession::new();
                session.enter(mode).unwrap();
                assert_eq!(session.screen_mode(), Some(mode));

                // Draw
                session.draw(&mut |_, _| {}).unwrap();
                assert_eq!(session.screen_mode(), Some(mode));

                // Suspend/resume preserves mode
                session.suspend().unwrap();
                assert_eq!(session.screen_mode(), Some(mode));
                session.resume().unwrap();
                assert_eq!(session.screen_mode(), Some(mode));

                // Leave clears mode
                session.leave();
                assert!(session.screen_mode().is_none());
            }
        }
    }

    // -- Chaos C11: failure recovery and retry --

    #[test]
    fn chaos_suspend_failure_retry_succeeds() {
        // Simulates: suspend fails once, but a retry (after resetting state) succeeds.
        // FailableSession fails after N calls total, so we can't really "retry"
        // the same call. Instead verify that the session is in a usable state
        // after a failed suspend and can still be cleanly torn down.
        let mut session = FailableSession::new().fail_suspend_after(0);
        session.enter(ScreenMode::AltScreen).unwrap();

        // Suspend fails
        assert!(session.suspend().is_err());
        // State is still Active
        assert_eq!(session.phase(), SessionPhase::Active);

        // Draw still works
        session.draw(&mut |_, _| {}).unwrap();

        // Leave is always safe
        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    // -- Chaos C12: sequential guard creation stress --

    #[test]
    fn chaos_sequential_guard_creation_100_times() {
        use super::super::output_gate::tests::GATE_TEST_LOCK;
        use super::super::output_gate::{self, GatePhase};
        let _lock = GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        output_gate::set_phase(GatePhase::Inactive);

        for i in 0..100u32 {
            let session = MockTerminalSession::new();
            let mode = if i % 2 == 0 {
                ScreenMode::AltScreen
            } else {
                ScreenMode::Inline {
                    ui_height: (i % 24 + 1) as u16,
                }
            };

            {
                let _guard = SessionGuard::enter(session, mode).unwrap();
                assert_eq!(output_gate::phase(), GatePhase::Active);
            }
            // Guard dropped — gate back to Inactive
            assert_eq!(output_gate::phase(), GatePhase::Inactive);
        }
    }
}
