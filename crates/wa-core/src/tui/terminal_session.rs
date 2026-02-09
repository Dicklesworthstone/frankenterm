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

use super::ftui_compat::{Area, InputEvent, RenderSurface};

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

    /// Acquire the terminal: enable raw mode, enter alternate screen.
    ///
    /// # Errors
    /// Returns `SessionError::InvalidPhase` if not in `Idle` phase.
    fn enter(&mut self) -> Result<(), SessionError>;

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
    /// Enter the session and return a guard that will leave on drop.
    pub fn enter(mut session: S) -> Result<Self, SessionError> {
        session.enter()?;
        Ok(Self {
            session: Some(session),
        })
    }

    /// Access the underlying session.
    ///
    /// # Panics
    /// Panics if called after `into_inner()`.
    pub fn session(&self) -> &S {
        self.session.as_ref().expect("session consumed by into_inner")
    }

    /// Access the underlying session mutably.
    ///
    /// # Panics
    /// Panics if called after `into_inner()`.
    pub fn session_mut(&mut self) -> &mut S {
        self.session.as_mut().expect("session consumed by into_inner")
    }

    /// Consume the guard, calling `leave()` and returning the session.
    ///
    /// The drop-based leave is suppressed; leave is called exactly once.
    pub fn into_inner(mut self) -> S {
        let mut session = self.session.take().expect("session consumed by into_inner");
        session.leave();
        session
    }
}

impl<S: TerminalSession> Drop for SessionGuard<S> {
    fn drop(&mut self) {
        if let Some(session) = &mut self.session {
            session.leave();
        }
    }
}

impl<S: TerminalSession> std::ops::Deref for SessionGuard<S> {
    type Target = S;
    fn deref(&self) -> &S {
        self.session.as_ref().expect("session consumed by into_inner")
    }
}

impl<S: TerminalSession> std::ops::DerefMut for SessionGuard<S> {
    fn deref_mut(&mut self) -> &mut S {
        self.session.as_mut().expect("session consumed by into_inner")
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
    terminal: Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>,
}

#[cfg(feature = "tui")]
impl CrosstermSession {
    pub fn new() -> Self {
        Self {
            phase: SessionPhase::Idle,
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

    fn enter(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Idle {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Idle],
                actual: self.phase,
            });
        }

        crossterm::terminal::enable_raw_mode()?;

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
        self.phase = SessionPhase::Idle;
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

    fn enter(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::Idle {
            return Err(SessionError::InvalidPhase {
                expected: &[SessionPhase::Idle],
                actual: self.phase,
            });
        }
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
            self.phase = SessionPhase::Idle;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::ftui_compat::{Key, KeyInput};

    #[test]
    fn mock_lifecycle_enter_leave() {
        let mut session = MockTerminalSession::new();
        assert_eq!(session.phase(), SessionPhase::Idle);

        session.enter().unwrap();
        assert_eq!(session.phase(), SessionPhase::Active);
        assert_eq!(session.history, vec!["enter"]);

        session.leave();
        assert_eq!(session.phase(), SessionPhase::Idle);
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    #[test]
    fn mock_double_enter_fails() {
        let mut session = MockTerminalSession::new();
        session.enter().unwrap();
        let err = session.enter().unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    #[test]
    fn mock_draw_requires_active() {
        let mut session = MockTerminalSession::new();
        let err = session
            .draw(&mut |_, _| {})
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    #[test]
    fn mock_suspend_resume_lifecycle() {
        let mut session = MockTerminalSession::new();
        session.enter().unwrap();
        session.suspend().unwrap();
        assert_eq!(session.phase(), SessionPhase::Suspended);

        // Can't draw while suspended
        let err = session.draw(&mut |_, _| {}).unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));

        session.resume().unwrap();
        assert_eq!(session.phase(), SessionPhase::Active);
        assert_eq!(
            session.history,
            vec!["enter", "suspend", "resume"]
        );
    }

    #[test]
    fn mock_command_handoff_pattern() {
        let mut session = MockTerminalSession::new();
        session.enter().unwrap();

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
            vec!["enter", "draw", "draw", "suspend", "resume", "draw", "leave"]
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
        session.enter().unwrap();

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
        session.enter().unwrap();
        session.leave();
        session.leave(); // Second leave is no-op
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    #[test]
    fn mock_leave_from_suspended() {
        let mut session = MockTerminalSession::new();
        session.enter().unwrap();
        session.suspend().unwrap();
        session.leave(); // Can leave from suspended
        assert_eq!(session.phase(), SessionPhase::Idle);
    }

    #[test]
    fn session_guard_into_inner_calls_leave() {
        let session = MockTerminalSession::new();
        let guard = SessionGuard::enter(session).unwrap();
        assert_eq!(guard.phase(), SessionPhase::Active);
        let session = guard.into_inner();
        assert_eq!(session.phase(), SessionPhase::Idle);
        assert_eq!(session.history, vec!["enter", "leave"]);
    }

    #[test]
    fn session_guard_deref() {
        let session = MockTerminalSession::new();
        let mut guard = SessionGuard::enter(session).unwrap();
        assert_eq!(guard.phase(), SessionPhase::Active);
        guard.suspend().unwrap();
        assert_eq!(guard.phase(), SessionPhase::Suspended);
    }

    #[test]
    fn resume_from_wrong_phase_fails() {
        let mut session = MockTerminalSession::new();
        session.enter().unwrap();
        let err = session.resume().unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }

    #[test]
    fn suspend_from_wrong_phase_fails() {
        let mut session = MockTerminalSession::new();
        let err = session.suspend().unwrap_err();
        assert!(matches!(err, SessionError::InvalidPhase { .. }));
    }
}
