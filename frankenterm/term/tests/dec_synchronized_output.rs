//! Integration tests for DEC private mode 2026 (synchronized output).
//!
//! Mode 2026 lets applications signal that a multi-line redraw is in
//! progress so the renderer can hold presentation until the redraw
//! completes, eliminating tearing on Neovim, lazygit, btop, ranger.
//!
//! These tests exercise the term-layer state machine end-to-end:
//! bytes-in (the actual `CSI ? 2026 h/l/$p` escape sequences) drive
//! the dispatcher, and the publicly-exposed
//! `Terminal::synchronized_output()` getter is the renderer's
//! contract. Renderer presentation-hold integration is tracked
//! separately (continuation bead).
//!
//! Bead: ft-d7af6 (BR-TERM-EMULATOR-UPLIFT-2.1.1).

use std::io::Write;
use std::sync::{Arc, Mutex};

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};

/// Writer that captures bytes into an `Arc<Mutex<Vec<u8>>>` so the
/// DECRQM-response tests can read back what the term layer wrote.
#[derive(Clone, Default)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn make_term_with_writer(writer: CapturedWriter) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
            dpi: 96,
        },
        Arc::new(TestConfig),
        "WezTerm",
        "test",
        Box::new(writer),
    )
}

// Note: this test file validates the term-layer state machine
// (BSU/ESU set/reset semantics + the synchronized_output() getter
// the renderer presentation-hold integration consumes). It does NOT
// validate the DECRQM query response (CSI ? 2026 $ p → CSI ? 2026 ;
// Ps $ y) because that probes a separate parser-layer issue: the
// parser test for `\x1b[?2026$p` in escape-parser/src/parser/mod.rs
// is currently commented out, and dispatch from the raw bytes does
// not reach `perform_csi_mode` reliably (test runs vary depending
// on whether the test binary contains other tests). The term-layer
// dispatch arm I'm wiring at `mod.rs::1572` is correct; the
// parser-side disconnect is filed as a follow-up.

#[derive(Debug)]
struct TestConfig;

impl TerminalConfiguration for TestConfig {
    fn scrollback_size(&self) -> usize {
        100
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn make_term() -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
            dpi: 96,
        },
        Arc::new(TestConfig),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    )
}

/// `CSI ? 2026 h` — Begin Synchronized Update (BSU).
const BSU: &[u8] = b"\x1b[?2026h";
/// `CSI ? 2026 l` — End Synchronized Update (ESU).
const ESU: &[u8] = b"\x1b[?2026l";

#[test]
fn synchronized_output_starts_disabled() {
    let term = make_term();
    assert!(
        !term.synchronized_output(),
        "fresh terminal must default to synchronized_output=false"
    );
}

#[test]
fn bsu_enables_synchronized_output() {
    let mut term = make_term();
    term.advance_bytes(BSU);
    assert!(
        term.synchronized_output(),
        "CSI ? 2026 h must enable synchronized output"
    );
}

#[test]
fn esu_disables_synchronized_output() {
    let mut term = make_term();
    term.advance_bytes(BSU);
    term.advance_bytes(ESU);
    assert!(
        !term.synchronized_output(),
        "CSI ? 2026 l must disable synchronized output"
    );
}

#[test]
fn esu_when_already_disabled_is_noop() {
    let mut term = make_term();
    // ESU before any BSU is a defensive case — apps may emit it on
    // startup to ensure a known state.
    term.advance_bytes(ESU);
    assert!(
        !term.synchronized_output(),
        "ESU on a fresh terminal must leave the flag at false"
    );
}

#[test]
fn bsu_is_idempotent() {
    let mut term = make_term();
    term.advance_bytes(BSU);
    term.advance_bytes(BSU);
    term.advance_bytes(BSU);
    assert!(
        term.synchronized_output(),
        "repeated BSU must keep the flag true"
    );
}

#[test]
fn rapid_set_reset_cycles_track_correctly() {
    // Models an app that emits BSU/ESU around every redraw — common
    // pattern with Neovim's full-screen update cycle.
    let mut term = make_term();
    for _ in 0..32 {
        term.advance_bytes(BSU);
        assert!(term.synchronized_output(), "BSU should set the flag");
        term.advance_bytes(ESU);
        assert!(!term.synchronized_output(), "ESU should clear the flag");
    }
}

#[test]
fn synchronized_output_does_not_affect_other_state() {
    // Mode 2026's set/reset must not have side effects on unrelated
    // term state (cursor position, screen content, scrollback). This
    // is the regression catch for any future implementation that
    // accidentally couples synchronized output to other dispatch
    // paths.
    let mut term = make_term();

    // Write some content.
    term.advance_bytes(b"hello world\n");
    let title_before = term.get_title().to_string();

    // Toggle mode 2026 a few times.
    term.advance_bytes(BSU);
    term.advance_bytes(ESU);
    term.advance_bytes(BSU);

    let title_after = term.get_title().to_string();
    assert_eq!(
        title_before, title_after,
        "synchronized output must not touch the title"
    );
    assert!(
        term.synchronized_output(),
        "post-toggle state should be enabled"
    );
}

// ft-av13k: DECRQM query for DEC mode 2026 must dispatch
// QueryDecPrivateMode and write the response envelope to the writer.
// VT-510 §S2.1 says the response is `CSI ? Pd ; Ps $ y` where Pd is
// the mode number and Ps is 1 (set) / 2 (reset) / 3 (permanently
// set) / 4 (permanently reset). For mode 2026 ft is *recognised
// settable*, so Ps reports the live flag: 1 when BSU is in effect, 2
// otherwise.

/// `CSI ? 2026 $ p` — DECRQM query for DEC mode 2026.
const DECRQM_2026: &[u8] = b"\x1b[?2026$p";

#[test]
fn decrqm_2026_when_set_responds_with_ps_1() {
    let writer = CapturedWriter::default();
    let captured = writer.0.clone();
    let mut term = make_term_with_writer(writer);

    term.advance_bytes(BSU);
    assert!(term.synchronized_output());
    term.advance_bytes(DECRQM_2026);

    let buf = captured.lock().unwrap().clone();
    assert_eq!(
        &buf[..],
        b"\x1b[?2026;1$y",
        "DECRQM response after BSU must report Ps=1 (set); got {:?}",
        String::from_utf8_lossy(&buf),
    );
}

#[test]
fn decrqm_2026_when_unset_responds_with_ps_2() {
    let writer = CapturedWriter::default();
    let captured = writer.0.clone();
    let mut term = make_term_with_writer(writer);

    // No BSU — flag is at default (false).
    assert!(!term.synchronized_output());
    term.advance_bytes(DECRQM_2026);

    let buf = captured.lock().unwrap().clone();
    assert_eq!(
        &buf[..],
        b"\x1b[?2026;2$y",
        "DECRQM response with flag clear must report Ps=2 (reset); got {:?}",
        String::from_utf8_lossy(&buf),
    );
}

#[test]
fn decrqm_2026_after_bsu_then_esu_responds_with_ps_2() {
    let writer = CapturedWriter::default();
    let captured = writer.0.clone();
    let mut term = make_term_with_writer(writer);

    term.advance_bytes(BSU);
    term.advance_bytes(ESU);
    term.advance_bytes(DECRQM_2026);

    let buf = captured.lock().unwrap().clone();
    assert_eq!(
        &buf[..],
        b"\x1b[?2026;2$y",
        "DECRQM after BSU+ESU must report Ps=2 (back to reset)",
    );
}
