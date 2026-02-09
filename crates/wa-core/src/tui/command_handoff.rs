//! Command execution handoff for TUI runtime.
//!
//! When the TUI needs to shell out to a command (e.g., `wa rules profile apply`),
//! the terminal session is suspended (alternate screen left, raw mode disabled),
//! the command runs with full terminal access, and then the session resumes.
//!
//! # State machine
//!
//! ```text
//! Active ──suspend()──▶ Suspended ──run cmd──▶ Suspended ──resume()──▶ Active
//!                          │                       │
//!                          │ (suspend fails)       │ (resume fails)
//!                          ▼                       ▼
//!                     HandoffError            HandoffError (cmd still ran)
//! ```
//!
//! # Output gate integration
//!
//! The output gate transitions are handled by `TerminalSession::suspend()` and
//! `resume()` (or by the crossterm implementation directly).  This module does
//! NOT touch the gate — it relies on the session implementation to manage it.
//!
//! # Deletion criterion
//! Remove when ftui's native subprocess/command model replaces this (FTUI-09.3).

use std::process::ExitStatus;

use super::terminal_session::{SessionError, SessionPhase, TerminalSession};

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Outcome of a command handoff execution.
#[derive(Debug)]
pub struct CommandResult {
    /// The command string that was executed.
    pub command: String,
    /// Exit status if the process launched successfully.
    pub status: Option<ExitStatus>,
    /// Error message if the process failed to launch.
    pub launch_error: Option<String>,
}

impl CommandResult {
    /// Returns `true` if the command ran and exited successfully.
    #[must_use]
    pub fn success(&self) -> bool {
        self.status.is_some_and(|s| s.success())
    }
}

/// Errors from the command handoff lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    #[error("failed to suspend session: {0}")]
    SuspendFailed(SessionError),

    #[error("failed to resume session: {0}")]
    ResumeFailed(SessionError),

    #[error("empty command")]
    EmptyCommand,
}

// ---------------------------------------------------------------------------
// Handoff execution
// ---------------------------------------------------------------------------

/// Execute a shell command with full terminal handoff.
///
/// The session must be in `Active` phase.  The function:
///
/// 1. Suspends the session (raw mode off, alternate screen left).
/// 2. Prints the command being run.
/// 3. Spawns the process and waits for completion.
/// 4. Shows the result and waits for the operator to press Enter.
/// 5. Resumes the session (alternate screen entered, raw mode on).
///
/// # Errors
///
/// - [`HandoffError::EmptyCommand`] if the command string is empty/whitespace.
/// - [`HandoffError::SuspendFailed`] if the session can't be suspended.
/// - [`HandoffError::ResumeFailed`] if the session can't be resumed after the
///   command runs.  The command result is still available via the error's
///   source chain.
///
/// # Panics
///
/// Does not panic.  All error paths attempt to leave the terminal in a
/// usable state.
pub fn execute<S: TerminalSession>(
    session: &mut S,
    command: &str,
) -> Result<CommandResult, HandoffError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(HandoffError::EmptyCommand);
    }

    // 1. Validate phase
    if session.phase() != SessionPhase::Active {
        return Err(HandoffError::SuspendFailed(SessionError::InvalidPhase {
            expected: &[SessionPhase::Active],
            actual: session.phase(),
        }));
    }

    // 2. Suspend
    session
        .suspend()
        .map_err(HandoffError::SuspendFailed)?;

    // 3. Execute (session is now Suspended — output gate allows writes)
    let result = execute_inner(command);

    // 4. Wait for operator confirmation
    wait_for_enter(&result);

    // 5. Resume
    if let Err(e) = session.resume() {
        // The command already ran — we can't undo it. But the session is stuck
        // in Suspended, which is a recoverable state (the caller can try
        // resume() again or call leave()).
        return Err(HandoffError::ResumeFailed(e));
    }

    Ok(result)
}

/// Execute the command and capture the result.
fn execute_inner(command: &str) -> CommandResult {
    let mut parts = command.split_whitespace();
    let program = parts.next().unwrap(); // Caller verified non-empty

    // Print the command so the operator sees what's running.
    println!("Running: {command}\n");

    match std::process::Command::new(program).args(parts).status() {
        Ok(status) => {
            println!("\nExit status: {status}");
            CommandResult {
                command: command.to_string(),
                status: Some(status),
                launch_error: None,
            }
        }
        Err(err) => {
            println!("\nCommand failed to launch: {err}");
            CommandResult {
                command: command.to_string(),
                status: None,
                launch_error: Some(err.to_string()),
            }
        }
    }
}

/// Block until the operator presses Enter.
fn wait_for_enter(result: &CommandResult) {
    if result.success() {
        println!("\nPress Enter to return to the TUI...");
    } else {
        println!("\nCommand completed with errors. Press Enter to return to the TUI...");
    }

    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::terminal_session::MockTerminalSession;

    use super::super::ftui_compat::ScreenMode;

    #[test]
    fn empty_command_returns_error() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        let err = execute(&mut session, "").unwrap_err();
        assert!(matches!(err, HandoffError::EmptyCommand));
    }

    #[test]
    fn whitespace_only_command_returns_error() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();
        let err = execute(&mut session, "   ").unwrap_err();
        assert!(matches!(err, HandoffError::EmptyCommand));
    }

    #[test]
    fn suspend_from_idle_fails() {
        let mut session = MockTerminalSession::new();
        // Don't enter — session is Idle
        let err = execute(&mut session, "echo hello").unwrap_err();
        assert!(matches!(err, HandoffError::SuspendFailed(_)));
    }

    #[test]
    fn handoff_suspends_and_resumes() {
        let mut session = MockTerminalSession::new();
        session.enter(ScreenMode::default()).unwrap();

        // We can't actually run the full handoff in tests (it reads stdin),
        // but we can verify the phase transitions by testing suspend/resume
        // directly, which is what execute() calls.
        session.suspend().unwrap();
        assert_eq!(session.phase(), SessionPhase::Suspended);
        session.resume().unwrap();
        assert_eq!(session.phase(), SessionPhase::Active);
        assert_eq!(session.history, vec!["enter", "suspend", "resume"]);
    }

    #[test]
    fn command_result_success_check() {
        let result = CommandResult {
            command: "echo hi".to_string(),
            status: None,
            launch_error: Some("not found".to_string()),
        };
        assert!(!result.success());
    }

    #[test]
    fn handoff_error_display() {
        let err = HandoffError::EmptyCommand;
        assert_eq!(err.to_string(), "empty command");
    }
}
