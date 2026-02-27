//! Regression test for accumulating_title memory leak cap.
//!
//! Verifies that the TerminalState discards the accumulating title
//! when it exceeds MAX_ACCUMULATING_TITLE_LEN (8 KiB), preventing
//! unbounded memory growth from malicious or malformed tmux title
//! escape sequences.
//!
//! Part of ft-3kxe.1 (Memory leak root cause analysis and patches).

use std::sync::Arc;

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};

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

/// Verifies that a normal tmux title sequence (under the cap) works correctly.
#[test]
fn tmux_title_under_cap_is_accepted() {
    let mut term = make_term();

    // ESC k = TmuxTitle, "hello", ESC \ = StringTerminator
    let mut seq = vec![0x1Bu8, b'k'];
    seq.extend_from_slice(b"hello");
    seq.push(0x1B);
    seq.push(b'\\');

    term.advance_bytes(&seq);

    // The title should have been set via the OSC dispatch path.
    // We verify this by checking the terminal's title.
    let title = term.get_title();
    assert_eq!(title, "hello");
}

/// Verifies that a tmux title sequence exceeding the 8 KiB cap is discarded.
/// This prevents unbounded memory growth from malicious escape sequences.
#[test]
fn tmux_title_exceeding_cap_is_discarded() {
    let mut term = make_term();

    // Start tmux title escape: ESC k
    let mut seq = vec![0x1Bu8, b'k'];

    // Push 9000 bytes of printable characters (exceeds 8192 cap)
    for _ in 0..9000 {
        seq.push(b'A');
    }

    // String Terminator: ESC backslash
    seq.push(0x1B);
    seq.push(b'\\');

    term.advance_bytes(&seq);

    // The overlong title should have been discarded, so the title
    // should NOT be the 9000-char string. It should remain the default
    // or be empty since the accumulation was dropped.
    let title = term.get_title();
    assert_ne!(
        title.len(),
        9000,
        "Overlong title should have been discarded"
    );
}

/// Verifies that after an overlong title is discarded, normal title
/// sequences still work (the terminal state recovers correctly).
#[test]
fn terminal_recovers_after_overlong_title_discard() {
    let mut term = make_term();

    // First: send an overlong title that exceeds the cap
    let mut overlong = vec![0x1Bu8, b'k'];
    for _ in 0..9000 {
        overlong.push(b'X');
    }
    overlong.push(0x1B);
    overlong.push(b'\\');
    term.advance_bytes(&overlong);

    // Then: send a normal-length title
    let mut normal = vec![0x1Bu8, b'k'];
    normal.extend_from_slice(b"recovered");
    normal.push(0x1B);
    normal.push(b'\\');
    term.advance_bytes(&normal);

    let title = term.get_title();
    assert_eq!(title, "recovered");
}
