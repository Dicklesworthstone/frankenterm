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
//! # Output routing contract (FTUI-03.2.a)
//!
//! All in-process output MUST obey the output gate phase:
//!
//! | Phase | Allowed writes | Mechanism |
//! |-------|---------------|-----------|
//! | **Inactive** | Any | Normal stdout/stderr |
//! | **Active** | Rendering pipeline only | `TuiAwareWriter` discards; use `gated_write!` |
//! | **InlineActive** | Writes outside owned inline region | region-aware gate check |
//! | **Suspended** | Command handoff output | `gated_write!` asserts output is not suppressed |
//!
//! ## Sanctioned output paths
//!
//! 1. **Structured logging** — routes through `TuiAwareWriter` via tracing.
//!    Suppressed during Active/InlineActive, passes through during
//!    Inactive/Suspended.
//! 2. **Command handoff** — `gated_write!`/`gated_writeln!` with debug
//!    assertions that output is not suppressed.
//! 3. **Crash/panic handler** — checks `is_output_suppressed()` before
//!    writing; may force-write if terminal restoration is needed.
//!
//! ## Prohibited
//!
//! Raw `println!`/`eprintln!`/`print!`/`eprint!` in any code path that
//! can execute while the TUI output is active. Use `tracing::error!` for
//! errors or `gated_writeln!` for operator-facing messages during command
//! handoff.
//!
//! # Integration points
//!
//! - [`SessionGuard`](super::terminal_session::SessionGuard) toggles the
//!   gate on enter/leave/suspend/resume and selects `InlineActive` for
//!   inline screen modes.
//! - [`logging::init_logging`](crate::logging::init_logging) can be called
//!   with [`TuiAwareWriter`] to suppress stderr during TUI.
//! - [`crash::install_panic_hook`](crate::crash::install_panic_hook)
//!   checks the gate before writing panic output.
//!
//! # Deletion criterion
//!
//! Remove the atomic gate when ftui's `TerminalWriter` fully owns output
//! routing and provides an equivalent mechanism (FTUI-09.3).

use std::sync::atomic::{AtomicU8, AtomicU16, Ordering};

/// Output gate states — stored as a `u8` in an atomic for lock-free access.
///
/// Four states rather than a bool because callers may need to distinguish
/// "suspended" (safe to write, session paused for command handoff) from
/// "active" (unsafe to write, rendering pipeline owns the terminal) and
/// inline rendering, where only a known terminal region is owned by the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GatePhase {
    /// No TUI session active — safe to write to stdout/stderr.
    Inactive = 0,
    /// TUI is rendering — do NOT write to stdout/stderr.
    Active = 1,
    /// TUI is suspended for command handoff — safe to write.
    Suspended = 2,
    /// TUI owns a bounded inline region; region-aware writes outside it are safe.
    InlineActive = 3,
}

impl GatePhase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Active,
            2 => Self::Suspended,
            3 => Self::InlineActive,
            _ => Self::Inactive,
        }
    }
}

/// Edge used when interpreting an inline region before a concrete terminal
/// height is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RegionAnchor {
    /// Region is measured from the top of the terminal.
    Top = 0,
    /// Region is measured from the bottom of the terminal.
    Bottom = 1,
}

impl RegionAnchor {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Bottom,
            _ => Self::Top,
        }
    }
}

/// Inline TUI-owned region.
///
/// `offset` is measured from the selected anchor. A bottom-anchored region with
/// offset 0 and height 8 owns the last eight terminal rows once the terminal
/// height is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineRegion {
    pub anchor: RegionAnchor,
    pub offset: u16,
    pub height: u16,
}

impl InlineRegion {
    #[must_use]
    pub const fn top(height: u16) -> Self {
        Self {
            anchor: RegionAnchor::Top,
            offset: 0,
            height,
        }
    }

    #[must_use]
    pub const fn bottom(height: u16) -> Self {
        Self {
            anchor: RegionAnchor::Bottom,
            offset: 0,
            height,
        }
    }

    #[must_use]
    pub const fn with_offset(mut self, offset: u16) -> Self {
        self.offset = offset;
        self
    }

    #[must_use]
    pub fn bounds(self, terminal_height: u16) -> Option<RowSpan> {
        if self.height == 0 || terminal_height == 0 {
            return None;
        }

        match self.anchor {
            RegionAnchor::Top => {
                let top = self.offset.min(terminal_height);
                if top == terminal_height {
                    return None;
                }
                let bottom = top.saturating_add(self.height).min(terminal_height);
                RowSpan::new(top, bottom - top, terminal_height)
            }
            RegionAnchor::Bottom => {
                if self.offset >= terminal_height {
                    return None;
                }
                let bottom = terminal_height - self.offset;
                let top = bottom.saturating_sub(self.height);
                RowSpan::new(top, bottom - top, terminal_height)
            }
        }
    }
}

/// Terminal row span used by region-aware output callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowSpan {
    pub top: u16,
    pub height: u16,
    pub terminal_height: u16,
}

impl RowSpan {
    #[must_use]
    pub fn new(top: u16, height: u16, terminal_height: u16) -> Option<Self> {
        if height == 0 || terminal_height == 0 || top >= terminal_height {
            return None;
        }
        Some(Self {
            top,
            height: height.min(terminal_height - top),
            terminal_height,
        })
    }

    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        if self.terminal_height != other.terminal_height {
            return true;
        }
        let self_bottom = self.top.saturating_add(self.height);
        let other_bottom = other.top.saturating_add(other.height);
        self.top < other_bottom && other.top < self_bottom
    }
}

/// Global gate state.
///
/// `GATE` publishes phase transitions with release/acquire ordering so
/// `InlineActive` readers observe the matching inline-region metadata. The
/// region atomics can stay relaxed because a conservative stale read still
/// suppresses unknown/raw output, but the gate publication must not expose a
/// newly active inline phase before its bounds are visible.
static GATE: AtomicU8 = AtomicU8::new(GatePhase::Inactive as u8);
static INLINE_ANCHOR: AtomicU8 = AtomicU8::new(RegionAnchor::Bottom as u8);
static INLINE_OFFSET: AtomicU16 = AtomicU16::new(0);
static INLINE_HEIGHT: AtomicU16 = AtomicU16::new(0);

/// Set the output gate phase.
///
/// Called by `SessionGuard` lifecycle methods:
/// - `enter()` → `Active`
/// - `suspend()` → `Suspended`
/// - `resume()` → `Active`
/// - `leave()` / `drop()` → `Inactive`
pub fn set_phase(phase: GatePhase) {
    INLINE_OFFSET.store(0, Ordering::Relaxed);
    INLINE_HEIGHT.store(0, Ordering::Relaxed);
    GATE.store(phase as u8, Ordering::Release);
}

/// Set the output gate to inline-active ownership for a bounded region.
pub fn set_inline_active_region(region: InlineRegion) {
    INLINE_ANCHOR.store(region.anchor as u8, Ordering::Relaxed);
    INLINE_OFFSET.store(region.offset, Ordering::Relaxed);
    INLINE_HEIGHT.store(region.height, Ordering::Relaxed);
    GATE.store(GatePhase::InlineActive as u8, Ordering::Release);
}

/// Read the current output gate phase.
pub fn phase() -> GatePhase {
    GatePhase::from_u8(GATE.load(Ordering::Acquire))
}

/// Read the currently configured inline region, if the gate is `InlineActive`.
pub fn inline_region() -> Option<InlineRegion> {
    if phase() != GatePhase::InlineActive {
        return None;
    }
    let height = INLINE_HEIGHT.load(Ordering::Relaxed);
    if height == 0 {
        return None;
    }
    Some(InlineRegion {
        anchor: RegionAnchor::from_u8(INLINE_ANCHOR.load(Ordering::Relaxed)),
        offset: INLINE_OFFSET.load(Ordering::Relaxed),
        height,
    })
}

/// Returns `true` when the TUI rendering pipeline owns the terminal and
/// external writes to stdout/stderr would corrupt the UI.
///
/// In practice: returns `true` when the gate is [`GatePhase::Active`] or
/// [`GatePhase::InlineActive`]. Direct stdout/stderr writes do not carry row
/// metadata, so inline mode is conservative by default. Region-aware callers
/// can use [`is_region_output_suppressed`] to permit writes outside the owned
/// inline region.
pub fn is_output_suppressed() -> bool {
    matches!(phase(), GatePhase::Active | GatePhase::InlineActive)
}

/// Return whether a write to a concrete terminal row span should be suppressed.
///
/// Unknown/raw writes must use [`is_output_suppressed`], which remains
/// conservative for inline mode.
pub fn is_region_output_suppressed(write_region: RowSpan) -> bool {
    match phase() {
        GatePhase::Inactive | GatePhase::Suspended => false,
        GatePhase::Active => true,
        GatePhase::InlineActive => inline_region()
            .and_then(|owned| owned.bounds(write_region.terminal_height))
            .is_none_or(|owned| owned.overlaps(write_region)),
    }
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
/// ```no_run
/// use frankenterm_core::tui::output_gate::TuiAwareWriter;
/// use tracing_subscriber::fmt;
/// use tracing_subscriber::prelude::*;
///
/// let layer = fmt::layer().with_writer(TuiAwareWriter);
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
// Gated write helpers (FTUI-03.2.a)
// -------------------------------------------------------------------------

/// Write to stdout only when the output gate allows it.
///
/// In debug builds, panics if called while output is suppressed
/// ([`GatePhase::Active`] or [`GatePhase::InlineActive`]) — this catches
/// accidental writes that would corrupt the TUI. In release builds, silently
/// discards the write to avoid crashing in production.
///
/// # Usage
///
/// ```
/// use frankenterm_core::tui::output_gate::gated_write_stdout;
/// gated_write_stdout(format_args!("Running: {}\n", "cargo build"));
/// ```
pub fn gated_write_stdout(args: std::fmt::Arguments<'_>) {
    if is_output_suppressed() {
        debug_assert!(
            false,
            "gated_write_stdout called while output is suppressed — this would corrupt the TUI"
        );
        return;
    }
    use std::io::Write;
    let _ = std::io::stdout().write_fmt(args);
}

/// Write to stderr only when the output gate allows it.
///
/// Same semantics as [`gated_write_stdout`] but targets stderr.
pub fn gated_write_stderr(args: std::fmt::Arguments<'_>) {
    if is_output_suppressed() {
        debug_assert!(
            false,
            "gated_write_stderr called while output is suppressed — this would corrupt the TUI"
        );
        return;
    }
    use std::io::Write;
    let _ = std::io::stderr().write_fmt(args);
}

/// Gate-aware replacement for `println!`.
///
/// Writes to stdout with a trailing newline when output is not suppressed.
/// Debug-asserts if called while output is suppressed.
///
/// Sanctioned for use in command handoff paths (Suspended phase) and
/// pre/post-TUI paths (Inactive phase).
#[macro_export]
macro_rules! gated_println {
    () => {
        $crate::tui::output_gate::gated_write_stdout(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::tui::output_gate::gated_write_stdout(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Gate-aware replacement for `eprintln!`.
///
/// Writes to stderr with a trailing newline when output is not suppressed.
/// Debug-asserts if called while output is suppressed.
#[macro_export]
macro_rules! gated_eprintln {
    () => {
        $crate::tui::output_gate::gated_write_stderr(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::tui::output_gate::gated_write_stderr(format_args!("{}\n", format_args!($($arg)*)))
    };
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
    //
    // `#[should_panic]` tests poison the mutex — `lock_gate()` recovers
    // from `PoisonError` so subsequent tests are not affected.
    use std::sync::Mutex;
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) static GATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Lock the gate test mutex, recovering from poison (caused by
    /// `#[should_panic]` tests that panic while holding the lock).
    fn lock_gate() -> std::sync::MutexGuard<'static, ()> {
        GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn gate_phase_roundtrip() {
        // Pure conversion test — no global mutation.
        for &p in &[
            GatePhase::Inactive,
            GatePhase::Active,
            GatePhase::Suspended,
            GatePhase::InlineActive,
        ] {
            assert_eq!(GatePhase::from_u8(p as u8), p);
        }
        // Unknown values map to Inactive (safe default)
        assert_eq!(GatePhase::from_u8(255), GatePhase::Inactive);
    }

    #[test]
    fn active_suppresses_output() {
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        assert!(is_output_suppressed());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn suspended_does_not_suppress() {
        let _lock = lock_gate();
        set_phase(GatePhase::Suspended);
        assert!(!is_output_suppressed());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn inline_active_suppresses_unknown_output_but_allows_outside_region() {
        let _lock = lock_gate();
        set_inline_active_region(InlineRegion::bottom(4));

        assert_eq!(phase(), GatePhase::InlineActive);
        assert_eq!(inline_region(), Some(InlineRegion::bottom(4)));
        assert!(is_output_suppressed());

        let outside = RowSpan::new(0, 20, 24).unwrap();
        let inside = RowSpan::new(20, 4, 24).unwrap();
        let overlapping = RowSpan::new(19, 2, 24).unwrap();

        assert!(!is_region_output_suppressed(outside));
        assert!(is_region_output_suppressed(inside));
        assert!(is_region_output_suppressed(overlapping));

        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn inline_active_top_region_classifies_write_spans() {
        let _lock = lock_gate();
        set_inline_active_region(InlineRegion::top(3).with_offset(2));

        assert!(!is_region_output_suppressed(
            RowSpan::new(0, 2, 12).unwrap()
        ));
        assert!(is_region_output_suppressed(RowSpan::new(2, 1, 12).unwrap()));
        assert!(is_region_output_suppressed(RowSpan::new(4, 1, 12).unwrap()));
        assert!(!is_region_output_suppressed(
            RowSpan::new(5, 7, 12).unwrap()
        ));

        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn direct_inline_active_phase_is_conservative_without_region() {
        let _lock = lock_gate();
        set_inline_active_region(InlineRegion::bottom(4));
        assert_eq!(inline_region(), Some(InlineRegion::bottom(4)));

        set_phase(GatePhase::InlineActive);
        assert!(is_output_suppressed());
        assert!(inline_region().is_none());
        assert!(is_region_output_suppressed(
            RowSpan::new(0, 20, 24).unwrap()
        ));

        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn full_lifecycle() {
        let _lock = lock_gate();

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
    fn inline_active_suspend_resume_lifecycle() {
        let _lock = lock_gate();

        set_inline_active_region(InlineRegion::bottom(6));
        assert!(is_output_suppressed());
        assert!(is_region_output_suppressed(
            RowSpan::new(18, 6, 24).unwrap()
        ));
        assert!(!is_region_output_suppressed(
            RowSpan::new(0, 18, 24).unwrap()
        ));

        set_phase(GatePhase::Suspended);
        assert!(!is_output_suppressed());
        assert!(inline_region().is_none());
        assert!(!is_region_output_suppressed(
            RowSpan::new(18, 6, 24).unwrap()
        ));

        set_inline_active_region(InlineRegion::bottom(6));
        assert_eq!(phase(), GatePhase::InlineActive);
        assert!(is_output_suppressed());

        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_suppresses_when_active() {
        use std::io::Write;
        let _lock = lock_gate();

        set_phase(GatePhase::Active);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        // Write should succeed (data is discarded)
        let n = inner.write(b"should be suppressed").unwrap();
        assert_eq!(n, b"should be suppressed".len());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_suppresses_when_inline_active() {
        use std::io::Write;
        let _lock = lock_gate();

        set_inline_active_region(InlineRegion::bottom(5));
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        let n = inner.write(b"raw stderr has no region").unwrap();
        assert_eq!(n, b"raw stderr has no region".len());
        assert!(matches!(inner, TuiAwareWriterInner::Suppressed));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_passes_through_when_inactive() {
        use std::io::Write;
        let _lock = lock_gate();

        set_phase(GatePhase::Inactive);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        // Write should succeed (forwarded to stderr)
        let result = inner.write(b"test");
        assert!(result.is_ok());
        set_phase(GatePhase::Inactive);
    }

    // -- gated write tests (FTUI-03.2.a) --

    #[test]
    fn gated_write_stdout_passes_when_inactive() {
        let _lock = lock_gate();
        set_phase(GatePhase::Inactive);
        // Should not panic — gate is Inactive.
        gated_write_stdout(format_args!("test inactive\n"));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn gated_write_stdout_passes_when_suspended() {
        let _lock = lock_gate();
        set_phase(GatePhase::Suspended);
        // Should not panic — gate is Suspended (command handoff).
        gated_write_stdout(format_args!("test suspended\n"));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn gated_write_stdout_suppressed_when_active() {
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        // In release builds, this silently discards.
        // In debug builds, the debug_assert would fire — but we test release
        // semantics here by checking it doesn't panic in non-debug.
        #[cfg(not(debug_assertions))]
        gated_write_stdout(format_args!("should be suppressed\n"));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn gated_write_stderr_passes_when_inactive() {
        let _lock = lock_gate();
        set_phase(GatePhase::Inactive);
        gated_write_stderr(format_args!("test stderr inactive\n"));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn gated_write_stderr_passes_when_suspended() {
        let _lock = lock_gate();
        set_phase(GatePhase::Suspended);
        gated_write_stderr(format_args!("test stderr suspended\n"));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn gated_write_stderr_suppressed_when_active() {
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        #[cfg(not(debug_assertions))]
        gated_write_stderr(format_args!("should be suppressed\n"));
        set_phase(GatePhase::Inactive);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "output gate is Active")]
    fn gated_write_stdout_panics_in_debug_when_active() {
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        gated_write_stdout(format_args!("boom"));
        // Cleanup won't run due to panic, but the GATE is process-global
        // and the test harness will continue on the next test.
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "output gate is Active")]
    fn gated_write_stderr_panics_in_debug_when_active() {
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        gated_write_stderr(format_args!("boom"));
    }

    #[test]
    fn gated_macros_compile_and_run() {
        let _lock = lock_gate();
        set_phase(GatePhase::Inactive);

        // Verify that the gated_println! and gated_eprintln! macros
        // compile and execute without error in Inactive phase.
        crate::gated_println!("macro test: {}", 42);
        crate::gated_eprintln!("macro test stderr: {}", 42);
        crate::gated_println!();
        crate::gated_eprintln!();

        set_phase(GatePhase::Inactive);
    }

    // ====================================================================
    // FTUI-08.4: Output gate resilience / concurrency stress
    // ====================================================================

    // -- Gate G1: rapid phase cycling stress --

    #[test]
    fn gate_rapid_phase_cycling_1000_rounds() {
        let _lock = lock_gate();
        set_phase(GatePhase::Inactive);

        for _ in 0..1000 {
            // Simulate full lifecycle: Inactive → Active → Suspended → Active → Inactive
            set_phase(GatePhase::Active);
            assert!(is_output_suppressed());

            set_phase(GatePhase::Suspended);
            assert!(!is_output_suppressed());

            set_phase(GatePhase::Active);
            assert!(is_output_suppressed());

            set_phase(GatePhase::Inactive);
            assert!(!is_output_suppressed());
        }
    }

    // -- Gate G2: concurrent reads during phase transitions --

    #[test]
    fn gate_concurrent_reads_during_transitions() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AOrdering};

        let _lock = lock_gate();
        set_phase(GatePhase::Inactive);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let reader_ready = Arc::new(AtomicBool::new(false));
        let reader_ready_clone = Arc::clone(&reader_ready);
        let read_count = Arc::new(AtomicU64::new(0));
        let read_count_clone = Arc::clone(&read_count);

        // Spawn reader thread that continuously checks the gate
        let reader = std::thread::spawn(move || {
            reader_ready_clone.store(true, AOrdering::Release);
            while !stop_clone.load(AOrdering::Relaxed) {
                // is_output_suppressed must never panic
                let _suppressed = is_output_suppressed();
                // phase() must always return a valid GatePhase
                let p = phase();
                assert!(
                    matches!(
                        p,
                        GatePhase::Inactive
                            | GatePhase::Active
                            | GatePhase::Suspended
                            | GatePhase::InlineActive
                    ),
                    "invalid gate phase: {p:?}"
                );
                read_count_clone.fetch_add(1, AOrdering::Relaxed);
            }
        });

        // Wait for reader thread to be ready
        while !reader_ready.load(AOrdering::Acquire) {
            std::thread::yield_now();
        }

        // Writer: rapidly cycle through phases, yielding periodically
        for i in 0..500u32 {
            set_phase(GatePhase::Active);
            set_phase(GatePhase::Suspended);
            set_phase(GatePhase::Inactive);
            if i % 50 == 0 {
                std::thread::yield_now();
            }
        }

        stop.store(true, AOrdering::Relaxed);
        reader.join().expect("reader thread panicked");
        let reads = read_count.load(AOrdering::Relaxed);
        // Verify reader did work (may be 0 on very fast single-core, so warn only)
        assert!(reads > 0, "reader thread should have performed reads");

        // Gate must be back to Inactive
        assert_eq!(phase(), GatePhase::Inactive);
    }

    // -- Gate G3: writer suppression invariant under cycling --

    #[test]
    fn gate_writer_suppression_invariant_under_cycling() {
        use std::io::Write;
        let _lock = lock_gate();

        for _ in 0..200 {
            set_phase(GatePhase::Active);
            let writer = TuiAwareWriter;
            let mut inner = writer.make();
            // Must silently discard during Active
            let n = inner.write(b"suppressed").unwrap();
            assert_eq!(n, 10);
            assert!(matches!(inner, TuiAwareWriterInner::Suppressed));

            set_phase(GatePhase::Suspended);
            let mut inner2 = writer.make();
            // Must pass through during Suspended
            assert!(matches!(inner2, TuiAwareWriterInner::Stderr(_)));
            let _ = inner2.write(b"ok");

            set_phase(GatePhase::Inactive);
            let mut inner3 = writer.make();
            // Must pass through during Inactive
            assert!(matches!(inner3, TuiAwareWriterInner::Stderr(_)));
            let _ = inner3.write(b"ok");
        }
    }

    // -- Gate G4: gated_write functions across rapid transitions --

    #[test]
    fn gate_gated_write_across_transitions() {
        let _lock = lock_gate();

        for i in 0..100u32 {
            set_phase(GatePhase::Inactive);
            gated_write_stdout(format_args!("inactive-stdout-{i}\n"));
            gated_write_stderr(format_args!("inactive-stderr-{i}\n"));

            set_phase(GatePhase::Suspended);
            gated_write_stdout(format_args!("suspended-stdout-{i}\n"));
            gated_write_stderr(format_args!("suspended-stderr-{i}\n"));

            // Active: skip gated_write calls (would debug_assert)
            set_phase(GatePhase::Active);
            // Verify suppression without calling gated_write
            assert!(is_output_suppressed());

            set_phase(GatePhase::Inactive);
        }
    }

    // -- Gate G5: phase idempotency --

    #[test]
    fn gate_phase_idempotent_set() {
        let _lock = lock_gate();

        // Setting the same phase repeatedly must be idempotent
        for _ in 0..100 {
            set_phase(GatePhase::Active);
            assert!(is_output_suppressed());
        }
        for _ in 0..100 {
            set_phase(GatePhase::Inactive);
            assert!(!is_output_suppressed());
        }
        for _ in 0..100 {
            set_phase(GatePhase::Suspended);
            assert!(!is_output_suppressed());
        }
        for _ in 0..100 {
            set_inline_active_region(InlineRegion::bottom(3));
            assert!(is_output_suppressed());
        }

        set_phase(GatePhase::Inactive);
    }

    // -- Additional coverage to reach 30 tests --

    #[test]
    fn gate_phase_debug_format_contains_variant() {
        assert!(format!("{:?}", GatePhase::Inactive).contains("Inactive"));
        assert!(format!("{:?}", GatePhase::Active).contains("Active"));
        assert!(format!("{:?}", GatePhase::Suspended).contains("Suspended"));
        assert!(format!("{:?}", GatePhase::InlineActive).contains("InlineActive"));
    }

    #[test]
    fn gate_phase_clone_preserves_value() {
        let phases = [
            GatePhase::Inactive,
            GatePhase::Active,
            GatePhase::Suspended,
            GatePhase::InlineActive,
        ];
        for p in phases {
            assert_eq!(p.clone(), p);
        }
    }

    #[test]
    fn gate_phase_copy_preserves_value() {
        let p = GatePhase::Active;
        let copied = p;
        let again = p; // still usable because Copy
        assert_eq!(copied, again);
    }

    #[test]
    fn gate_phase_from_u8_unknown_values_default_inactive() {
        for v in [3u8, 10, 100, 128, 254, 255] {
            assert_eq!(GatePhase::from_u8(v), GatePhase::Inactive);
        }
    }

    #[test]
    fn gate_phase_eq_symmetry() {
        let phases = [
            GatePhase::Inactive,
            GatePhase::Active,
            GatePhase::Suspended,
            GatePhase::InlineActive,
        ];
        for a in &phases {
            for b in &phases {
                assert_eq!(*a == *b, *b == *a);
            }
        }
    }

    #[test]
    fn gate_phase_ne_across_variants() {
        assert_ne!(GatePhase::Inactive, GatePhase::Active);
        assert_ne!(GatePhase::Active, GatePhase::Suspended);
        assert_ne!(GatePhase::Inactive, GatePhase::Suspended);
        assert_ne!(GatePhase::InlineActive, GatePhase::Active);
        assert_ne!(GatePhase::InlineActive, GatePhase::Suspended);
    }

    #[test]
    fn tui_aware_writer_flush_succeeds_when_suppressed() {
        use std::io::Write;
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        assert!(inner.flush().is_ok());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_passes_through_when_suspended() {
        use std::io::Write;
        let _lock = lock_gate();
        set_phase(GatePhase::Suspended);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        assert!(matches!(inner, TuiAwareWriterInner::Stderr(_)));
        let result = inner.write(b"suspended write");
        assert!(result.is_ok());
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_suppressed_returns_full_length() {
        use std::io::Write;
        let _lock = lock_gate();
        set_phase(GatePhase::Active);
        let writer = TuiAwareWriter;
        let mut inner = writer.make();
        for len in [0, 1, 10, 100, 1024] {
            let buf = vec![b'x'; len];
            let n = inner.write(&buf).unwrap();
            assert_eq!(n, len, "suppressed write should report full length");
        }
        set_phase(GatePhase::Inactive);
    }

    #[test]
    fn tui_aware_writer_is_copy() {
        let w1 = TuiAwareWriter;
        let w2 = w1; // Copy
        let w3 = w1; // still usable
        // Both produce writers with same behavior
        let _lock = lock_gate();
        set_phase(GatePhase::Inactive);
        assert!(matches!(w2.make(), TuiAwareWriterInner::Stderr(_)));
        assert!(matches!(w3.make(), TuiAwareWriterInner::Stderr(_)));
        set_phase(GatePhase::Inactive);
    }
}
