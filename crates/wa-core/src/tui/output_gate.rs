//! One-writer output gate for TUI runtime.
//!
//! FrankenTUI enforces a one-writer rule: only one entity may write to the
//! terminal (stdout/stderr) at a time.  During TUI runtime, all output must
//! flow through the rendering pipeline — stray `println!` or `eprintln!`
//! calls corrupt the cursor/layout state.
//!
//! This module provides a lightweight, thread-safe gate that tracks whether
//! the TUI is currently active.  Other parts of the codebase (logging, crash
//! handler, debug output) check this gate before writing to stderr.
//!
//! # Integration points
//!
//! - [`SessionGuard`](super::terminal_session::SessionGuard) toggles the
//!   gate on enter/leave/suspend/resume.
//! - [`logging::init_logging`](crate::logging::init_logging) can be called
//!   with [`TuiAwareWriter`] to suppress stderr during TUI.
//! - [`crash::install_panic_hook`](crate::crash::install_panic_hook)
//!   checks the gate before writing panic output.
//!
//! # Deletion criterion
//!
//! Remove the atomic gate when ftui's `TerminalWriter` fully owns output
//! routing and provides an equivalent mechanism (FTUI-09.3).

use std::sync::atomic::{AtomicU8, Ordering};

/// Output gate states — stored as a `u8` in an atomic for lock-free access.
///
/// Three states rather than a bool because callers may need to distinguish
/// "suspended" (safe to write, session paused for command handoff) from
/// "active" (unsafe to write, rendering pipeline owns the terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GatePhase {
    /// No TUI session active — safe to write to stdout/stderr.
    Inactive = 0,
    /// TUI is rendering — do NOT write to stdout/stderr.
    Active = 1,
    /// TUI is suspended for command handoff — safe to write.
    Suspended = 2,
}

impl GatePhase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Active,
            2 => Self::Suspended,
            _ => Self::Inactive,
        }
    }
}

/// Global gate state.  Relaxed ordering is fine because this is advisory
/// (best-effort suppression, not a memory-ordering fence).
static GATE: AtomicU8 = AtomicU8::new(GatePhase::Inactive as u8);

/// Set the output gate phase.
///
/// Called by `SessionGuard` lifecycle methods:
/// - `enter()` → `Active`
/// - `suspend()` → `Suspended`
/// - `resume()` → `Active`
/// - `leave()` / `drop()` → `Inactive`
pub fn set_phase(phase: GatePhase) {
    GATE.store(phase as u8, Ordering::Relaxed);
}

/// Read the current output gate phase.
pub fn phase() -> GatePhase {
    GatePhase::from_u8(GATE.load(Ordering::Relaxed))
}

/// Returns `true` when the TUI rendering pipeline owns the terminal and
/// external writes to stdout/stderr would corrupt the UI.
///
/// In practice: returns `true` only when the gate is [`GatePhase::Active`].
/// Both `Inactive` and `Suspended` are safe for direct writes.
pub fn is_output_suppressed() -> bool {
    phase() == GatePhase::Active
}

// -------------------------------------------------------------------------
// TuiAwareWriter — drop-in replacement for stderr in tracing
// -------------------------------------------------------------------------

/// A writer that forwards to stderr only when the output gate is not active.
///
/// When the TUI is rendering, writes are silently discarded to prevent
/// terminal corruption.  When the TUI is inactive or suspended, writes
/// pass through to stderr normally.
///
/// # Usage with tracing
///
/// ```ignore
/// use wa_core::tui::output_gate::TuiAwareWriter;
///
/// fmt::layer()
///     .with_writer(TuiAwareWriter)
///     // ...
/// ```
#[derive(Clone, Copy)]
pub struct TuiAwareWriter;

impl TuiAwareWriter {
    /// Returns a writer that either forwards to stderr or discards.
    #[allow(clippy::trivially_copy_pass_by_ref, clippy::unused_self)]
    fn make(&self) -> TuiAwareWriterInner {
        if is_output_suppressed() {
            TuiAwareWriterInner::Suppressed
        } else {
            TuiAwareWriterInner::Stderr(std::io::stderr())
        }
    }
}

/// Inner writer returned by [`TuiAwareWriter`].
///
/// Not intended for direct use — exposed only because `MakeWriter`
/// requires the associated type to be public.
pub enum TuiAwareWriterInner {
    /// Forwarding to stderr.
    Stderr(std::io::Stderr),
    /// Output suppressed (TUI active).
    Suppressed,
}

impl std::io::Write for TuiAwareWriterInner {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Stderr(w) => w.write(buf),
            Self::Suppressed => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stderr(w) => w.flush(),
            Self::Suppressed => Ok(()),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TuiAwareWriter {
    type Writer = TuiAwareWriterInner;

    fn make_writer(&'a self) -> Self::Writer {
        self.make()
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // NOTE: The gate is a process-global atomic.  Tests that mutate it
    // must run under a serial lock to avoid races with parallel test
    // threads.  We use a Mutex to serialize all gate-mutation tests.
    // `pub(crate)` so terminal_session tests can share it.
    use std::sync::Mutex;
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) static GATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn gate_phase_roundtrip() {
        // Pure conversion test — no global mutation.
        for &p in &[GatePhase::Inactive, GatePhase::Active, GatePhase::Suspended] {
            assert_eq!(GatePhase::from_u8(p as u8), p);
        }
        // Unknown values map to Inactive (safe default)
        assert_eq!(GatePhase::from_u8(255), GatePhase::Inactive);
    }

    #[test]
    fn active_suppresses_output() {
        let _lock = GATE_TEST_LOCK.lock().unwrap();
        set_phase(GatePhase::Active);
        assert!(is_output_suppressed());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn suspended_does_not_suppress() {
        let _lock = GATE_TEST_LOCK.lock().unwrap();
        set_phase(GatePhase::Suspended);
        assert!(!is_output_suppressed());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn full_lifecycle() {
        let _lock = GATE_TEST_LOCK.lock().unwrap();

        set_phase(GatePhase::Inactive);
        assert!(!is_output_suppressed());

        // enter
        set_phase(GatePhase::Active);
        assert!(is_output_suppressed());

        // suspend for command handoff
        set_phase(GatePhase::Suspended);
        assert!(!is_output_suppressed());

        // resume
        set_phase(GatePhase::Active);
        assert!(is_output_suppressed());

        // leave
        set_phase(GatePhase::Inactive);
        assert!(!is_output_suppressed());
    }

    #[test]
    fn tui_aware_writer_suppresses_when_active() {
        use std::io::Write;
        let _lock = GATE_TEST_LOCK.lock().unwrap();

        set_phase(GatePhase::Active);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        // Write should succeed (data is discarded)
        let n = inner.write(b"should be suppressed").unwrap();
        assert_eq!(n, b"should be suppressed".len());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_passes_through_when_inactive() {
        use std::io::Write;
        let _lock = GATE_TEST_LOCK.lock().unwrap();

        set_phase(GatePhase::Inactive);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        // Write should succeed (forwarded to stderr)
        let result = inner.write(b"test");
        assert!(result.is_ok());
        set_phase(GatePhase::Inactive);
    }
}
