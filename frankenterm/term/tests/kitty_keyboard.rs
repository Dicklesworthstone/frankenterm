//! Integration tests for the Kitty keyboard protocol — progressive
//! enhancement modes for full-fidelity key disambiguation.
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
//!
//! Wire format:
//! - `CSI = flags ; mode u`  → SetKittyState   (replace top of stack)
//! - `CSI > flags ; mode u`  → PushKittyState  (push new entry)
//! - `CSI < n u`             → PopKittyState   (pop `n` entries)
//! - `CSI ? u`               → QueryKittySupport (response: `CSI ? flags u`)
//!
//! `mode` codes:
//! - `1` AssignAll       — replace flags wholesale
//! - `2` SetSpecified    — current_flags | new_flags
//! - `3` ClearSpecified  — current_flags & !new_flags
//!
//! Bead: ft-68if8 (BR-TERM-EMULATOR-UPLIFT-2.1.4).

use std::io::Write;
use std::sync::{Arc, Mutex};

use frankenterm_escape_parser::csi::KittyKeyboardFlags;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{KeyCode, KeyModifiers, Terminal, TerminalConfiguration, TerminalSize};
use serde::Deserialize;
use termwiz::input::{KeyCodeEncodeModes, KeyboardEncoding};

#[derive(Debug)]
struct TestConfig;

impl TerminalConfiguration for TestConfig {
    fn scrollback_size(&self) -> usize {
        100
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn enable_kitty_keyboard(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct DisabledConfig;

impl TerminalConfiguration for DisabledConfig {
    fn scrollback_size(&self) -> usize {
        100
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
    // enable_kitty_keyboard defaults to false — exercise the gate.
}

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

impl CapturedWriter {
    fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
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

fn make_term_with_capture() -> (Terminal, CapturedWriter) {
    let captured = CapturedWriter::default();
    let term = Terminal::new(
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
        Box::new(captured.clone()),
    );
    (term, captured)
}

fn make_term_disabled() -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
            dpi: 96,
        },
        Arc::new(DisabledConfig),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    )
}

fn current_kitty_flags(term: &Terminal) -> Option<KittyKeyboardFlags> {
    match term.get_keyboard_encoding() {
        KeyboardEncoding::Kitty(flags) => Some(flags),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct KittyKeyboardFixture {
    mode: String,
    flags: u16,
    events: Vec<FixtureKeyEvent>,
}

#[derive(Debug, Deserialize)]
struct FixtureKeyEvent {
    key: String,
    #[serde(default)]
    modifiers: Vec<String>,
    #[serde(default = "default_key_down")]
    down: bool,
}

#[derive(Debug, Deserialize)]
struct ExpectedBytes {
    bytes: String,
}

fn default_key_down() -> bool {
    true
}

fn load_kitty_keyboard_fixture(name: &str) -> (KittyKeyboardFixture, ExpectedBytes) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/kitty_keyboard")
        .join(name);
    let input = std::fs::read_to_string(root.join("input.json")).unwrap();
    let expected = std::fs::read_to_string(root.join("expected.json")).unwrap();
    (
        serde_json::from_str(&input).unwrap(),
        serde_json::from_str(&expected).unwrap(),
    )
}

fn fixture_key(key: &str) -> KeyCode {
    match key {
        "backspace" => KeyCode::Backspace,
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Escape,
        "tab" => KeyCode::Tab,
        "left" => KeyCode::LeftArrow,
        "right" => KeyCode::RightArrow,
        "up" => KeyCode::UpArrow,
        "down" => KeyCode::DownArrow,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        _ => {
            let mut chars = key.chars();
            let c = chars
                .next()
                .unwrap_or_else(|| panic!("empty key name in fixture"));
            assert!(
                chars.next().is_none(),
                "unknown multi-character key name in fixture: {}",
                key
            );
            KeyCode::Char(c)
        }
    }
}

fn fixture_modifiers(modifiers: &[String]) -> KeyModifiers {
    let mut result = KeyModifiers::NONE;
    for modifier in modifiers {
        result |= match modifier.as_str() {
            "SHIFT" => KeyModifiers::SHIFT,
            "ALT" => KeyModifiers::ALT,
            "CTRL" => KeyModifiers::CTRL,
            "SUPER" => KeyModifiers::SUPER,
            "HYPER" => KeyModifiers::HYPER,
            "META" => KeyModifiers::META,
            other => panic!("unknown modifier in fixture: {}", other),
        };
    }
    result
}

fn run_kitty_keyboard_fixture(name: &str) {
    let (fixture, expected) = load_kitty_keyboard_fixture(name);
    let modes = KeyCodeEncodeModes {
        encoding: KeyboardEncoding::Kitty(KittyKeyboardFlags::from_bits_truncate(fixture.flags)),
        newline_mode: false,
        application_cursor_keys: false,
        application_keypad: false,
        modify_other_keys: None,
    };

    let mut encoded = String::new();
    for event in fixture.events {
        let key = fixture_key(&event.key);
        let modifiers = fixture_modifiers(&event.modifiers);
        encoded.push_str(&key.encode(modifiers, modes, event.down).unwrap());
    }

    assert_eq!(
        encoded, expected.bytes,
        "kitty keyboard fixture {name} must match byte-for-byte",
    );

    let (mut term, _) = make_term_with_capture();
    term.advance_bytes(fixture.mode.as_bytes());
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(fixture.flags)),
        "kitty keyboard fixture {name} mode sequence must set the expected flags",
    );
}

#[test]
fn fresh_terminal_has_no_kitty_state_on_stack() {
    let term = make_term();
    assert!(
        current_kitty_flags(&term).is_none(),
        "fresh terminal must not have Kitty mode active"
    );
}

#[test]
fn set_assignall_replaces_flags() {
    let mut term = make_term();
    // CSI = 1 ; 1 u → SetKittyState(DISAMBIGUATE, AssignAll)
    term.advance_bytes(b"\x1b[=1;1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES),
    );
}

#[test]
fn push_grows_the_stack_and_set_does_not() {
    let mut term = make_term();
    // First push to seed the stack with Kitty(NONE).
    term.advance_bytes(b"\x1b[>0;1u");
    let after_push = current_kitty_flags(&term);
    assert_eq!(after_push, Some(KittyKeyboardFlags::NONE));

    // Set replaces top of stack — does not push a new entry. Pop
    // one and we should be back to no-Kitty state.
    term.advance_bytes(b"\x1b[=15;1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(15)),
    );

    term.advance_bytes(b"\x1b[<1u");
    assert!(
        current_kitty_flags(&term).is_none(),
        "single pop should clear the only entry left after Set",
    );
}

#[test]
fn push_pop_round_trips_to_baseline() {
    let mut term = make_term();
    term.advance_bytes(b"\x1b[>1;1u");
    term.advance_bytes(b"\x1b[>2;1u");
    term.advance_bytes(b"\x1b[>4;1u");
    // Three pushes — top of stack should be flag 4 (REPORT_ALTERNATE_KEYS).
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS),
    );

    // Pop all three.
    term.advance_bytes(b"\x1b[<3u");
    assert!(
        current_kitty_flags(&term).is_none(),
        "pop 3 after 3 pushes should leave no Kitty entry"
    );
}

#[test]
fn set_specified_merges_with_current_flags() {
    let mut term = make_term();
    // Push flag 1.
    term.advance_bytes(b"\x1b[>1;1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES),
    );
    // Set flag 2 with mode 2 (SetSpecified) — should OR into 1|2=3.
    term.advance_bytes(b"\x1b[=2;2u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(3)),
    );
}

#[test]
fn clear_specified_strips_bits() {
    let mut term = make_term();
    // Push flags 7 (1|2|4).
    term.advance_bytes(b"\x1b[>7;1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(7)),
    );
    // Clear bit 1 with mode 3 (ClearSpecified) — 7 & !1 = 6.
    term.advance_bytes(b"\x1b[=1;3u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(6)),
    );
}

#[test]
fn pop_more_than_stack_size_is_safe() {
    let mut term = make_term();
    term.advance_bytes(b"\x1b[>1;1u");
    // Pop 5 entries when only 1 is present — should be a no-op
    // beyond the first pop. No panic.
    term.advance_bytes(b"\x1b[<5u");
    assert!(
        current_kitty_flags(&term).is_none(),
        "over-pop should drain the stack and leave no Kitty entry"
    );
}

#[test]
fn query_kitty_support_responds_with_current_flags() {
    let (mut term, captured) = make_term_with_capture();
    term.advance_bytes(b"\x1b[>5;1u"); // push DISAMBIGUATE | REPORT_ALTERNATE_KEYS
    term.advance_bytes(b"\x1b[?u"); // query

    let buf = captured.snapshot();
    let buf_str = String::from_utf8_lossy(&buf);
    // Response format: CSI ? flags u
    assert!(
        buf_str.contains("\x1b[?5u"),
        "QueryKittySupport should respond with current flags=5, got: {:?}",
        buf_str,
    );
}

#[test]
fn query_kitty_support_on_empty_stack_responds_with_zero() {
    let (mut term, captured) = make_term_with_capture();
    term.advance_bytes(b"\x1b[?u");

    let buf = captured.snapshot();
    let buf_str = String::from_utf8_lossy(&buf);
    // No Kitty state on the stack → flags = 0.
    assert!(
        buf_str.contains("\x1b[?0u"),
        "QueryKittySupport on empty stack should respond with flags=0, got: {:?}",
        buf_str,
    );
}

#[test]
fn disabled_config_makes_set_and_push_noops() {
    let mut term = make_term_disabled();
    term.advance_bytes(b"\x1b[>15;1u"); // would push if enabled
    term.advance_bytes(b"\x1b[=15;1u"); // would set if enabled

    assert!(
        current_kitty_flags(&term).is_none(),
        "with enable_kitty_keyboard=false, push/set must be no-ops"
    );
}

#[test]
fn stack_cap_bounds_unbounded_pushes() {
    // The dispatcher caps the keyboard_stack at 128 entries by
    // evicting from the front when a new push would exceed that
    // bound (performer.rs:563-565). 200 pushes must not OOM and
    // the top-of-stack must remain a Kitty entry.
    let mut term = make_term();
    for i in 0..200u16 {
        // Vary the flags so each push is distinct (helps catch any
        // ordering bug in the eviction policy).
        let flags = i % 16; // truncate to known KittyKeyboardFlags bits
        let seq = format!("\x1b[>{};1u", flags);
        term.advance_bytes(seq.as_bytes());
    }
    // Top-of-stack should be the most recent push.
    let top_flag_bits: u16 = 199 % 16;
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(top_flag_bits)),
    );
}

#[test]
fn pop_back_to_xterm_restores_legacy_encoding() {
    // A11Y regression catch (per the bead's A11Y section):
    // "progressive enhancement must not break existing keyboard
    // shortcut accessibility paths". An app that pushes Kitty mode
    // and later pops it must restore the original (non-Kitty)
    // encoding so legacy keybindings continue to work for assistive
    // technology that expects xterm-shaped output.
    let term_before = make_term();
    let baseline_encoding = term_before.get_keyboard_encoding();

    let mut term = make_term();
    term.advance_bytes(b"\x1b[>15;1u"); // push full Kitty mode
    assert!(matches!(
        term.get_keyboard_encoding(),
        KeyboardEncoding::Kitty(_)
    ));

    term.advance_bytes(b"\x1b[<1u"); // pop
    assert_eq!(
        term.get_keyboard_encoding(),
        baseline_encoding,
        "popping Kitty mode must restore the original encoding for a11y"
    );
}

#[test]
fn key_down_writes_to_pty_capture() {
    let (mut term, captured) = make_term_with_capture();
    term.key_down(KeyCode::Char('p'), KeyModifiers::NONE)
        .unwrap();

    assert_eq!(String::from_utf8(captured.snapshot()).unwrap(), "p");
}

#[test]
fn nested_push_set_pop_unwinds_correctly() {
    let mut term = make_term();
    // Push level 1: flag 1
    term.advance_bytes(b"\x1b[>1;1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES),
    );

    // Push level 2: flag 2 (additive in mode 2)
    term.advance_bytes(b"\x1b[>2;2u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::from_bits_truncate(3)),
    );

    // Set at level 2: replace with flag 4
    term.advance_bytes(b"\x1b[=4;1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS),
    );

    // Pop back to level 1 — top should be the original flag 1
    // entry (NOT contaminated by the level-2 set above).
    term.advance_bytes(b"\x1b[<1u");
    assert_eq!(
        current_kitty_flags(&term),
        Some(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES),
        "level-2 set must not bleed into level-1 frame"
    );

    // Pop level 1 — back to baseline.
    term.advance_bytes(b"\x1b[<1u");
    assert!(
        current_kitty_flags(&term).is_none(),
        "fully popped stack should have no Kitty entry"
    );
}

#[test]
fn kitty_keyboard_conformance_golden_fixtures_match_pty_bytes() {
    for name in [
        "vim/super_key_bindings",
        "vim/disambiguation",
        "emacs/full_progressive",
    ] {
        run_kitty_keyboard_fixture(name);
    }
}
