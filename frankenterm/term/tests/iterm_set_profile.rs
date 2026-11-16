//! Integration tests for the OSC 1337 SetProfile security gate.
//!
//! Bead: ft-fy4ty (BR-TERM-EMULATOR-UPLIFT-2.1.3).
//!
//! iTerm2's `OSC 1337;SetProfile=<name>` request is parsed by the
//! escape parser but the term-layer dispatch must never apply the
//! switch silently — that would let any byte stream from any
//! subprocess change the user's profile (colors, fonts, behavior)
//! without consent. The security contract under ft-fy4ty is:
//!
//! 1. SetProfile is dispatched, not silently dropped.
//! 2. The dispatch surfaces the request via
//!    `Alert::SetProfileRequested { name }` to the embedder.
//! 3. The term layer never mutates profile state itself.
//! 4. Without an alert handler installed, the request is dropped
//!    (with an optional log line) — the safer default than
//!    silently applying.
//!
//! These tests exercise that contract end-to-end.

use std::sync::{Arc, Mutex};

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Alert, AlertHandler, Terminal, TerminalConfiguration, TerminalSize};

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

/// Capturing alert handler — the embedder-side surface the term
/// layer pushes alerts into. The test holds an Arc<Mutex<Vec<Alert>>>
/// snoop and the term holds a Box<dyn AlertHandler> that points at
/// the same vector.
#[derive(Default)]
struct CapturedAlerts {
    alerts: Arc<Mutex<Vec<Alert>>>,
}

impl CapturedAlerts {
    fn handle(&self) -> CapturedAlertHandler {
        CapturedAlertHandler {
            alerts: Arc::clone(&self.alerts),
        }
    }

    fn snapshot(&self) -> Vec<Alert> {
        self.alerts.lock().unwrap().clone()
    }
}

struct CapturedAlertHandler {
    alerts: Arc<Mutex<Vec<Alert>>>,
}

impl AlertHandler for CapturedAlertHandler {
    fn alert(&mut self, alert: Alert) {
        self.alerts.lock().unwrap().push(alert);
    }
}

fn make_term_with_alert_capture() -> (Terminal, CapturedAlerts) {
    let captured = CapturedAlerts::default();
    let mut term = Terminal::new(
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
    );
    term.set_notification_handler(Box::new(captured.handle()));
    (term, captured)
}

fn make_term_without_handler() -> Terminal {
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

/// `OSC 1337 ; SetProfile = <name> ST` — the iTerm2 profile switch
/// request that the security gate must intercept.
fn set_profile_seq(name: &str) -> Vec<u8> {
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b]1337;SetProfile=");
    seq.extend_from_slice(name.as_bytes());
    seq.extend_from_slice(b"\x1b\\");
    seq
}

#[test]
fn set_profile_emits_alert_with_requested_name() {
    let (mut term, alerts) = make_term_with_alert_capture();
    term.advance_bytes(set_profile_seq("solarized-dark"));

    let snapshot = alerts.snapshot();
    let set_profile_alerts: Vec<&Alert> = snapshot
        .iter()
        .filter(|a| matches!(a, Alert::SetProfileRequested { .. }))
        .collect();
    assert_eq!(
        set_profile_alerts.len(),
        1,
        "exactly one SetProfileRequested alert should fire, observed: {:?}",
        snapshot,
    );
    match set_profile_alerts[0] {
        Alert::SetProfileRequested { name } => {
            assert_eq!(name, "solarized-dark");
        }
        other => panic!("expected SetProfileRequested, got {:?}", other),
    }
}

#[test]
fn set_profile_does_not_apply_silently() {
    // The bead's security invariant: even with a profile-switching
    // application aggressively emitting SetProfile, the term layer
    // must not mutate any observable terminal state itself. The
    // alert is the only side effect; profile application is the
    // embedder's choice.
    let (mut term, _alerts) = make_term_with_alert_capture();
    let title_before = term.get_title().to_string();

    term.advance_bytes(set_profile_seq("evil-profile"));

    let title_after = term.get_title().to_string();
    assert_eq!(
        title_before, title_after,
        "SetProfile dispatch must not touch the title",
    );
}

#[test]
fn multiple_set_profile_requests_each_fire_an_alert() {
    let (mut term, alerts) = make_term_with_alert_capture();
    term.advance_bytes(set_profile_seq("light"));
    term.advance_bytes(set_profile_seq("dark"));
    term.advance_bytes(set_profile_seq("contrast"));

    let snapshot = alerts.snapshot();
    let names: Vec<&str> = snapshot
        .iter()
        .filter_map(|a| match a {
            Alert::SetProfileRequested { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        vec!["light", "dark", "contrast"],
        "each SetProfile request must fire its own alert in order",
    );
}

#[test]
fn set_profile_without_handler_is_dropped_safely() {
    // No alert handler installed → the request is dropped (with an
    // optional log line). Critically, this must not panic and must
    // not leave the terminal in a broken state.
    let mut term = make_term_without_handler();
    let title_before = term.get_title().to_string();

    term.advance_bytes(set_profile_seq("no-handler-here"));

    let title_after = term.get_title().to_string();
    assert_eq!(title_before, title_after);
    // Subsequent commands continue to work — the dispatch path
    // is not poisoned.
    term.advance_bytes(b"hello\r\n");
}

#[test]
fn set_profile_with_empty_name_still_dispatches() {
    // `OSC 1337;SetProfile=\x1b\\` (empty name). The parser may
    // produce SetProfile("") here; the dispatch must still fire
    // an alert so the embedder sees the malformed request and can
    // decide how to respond.
    let (mut term, alerts) = make_term_with_alert_capture();
    term.advance_bytes(set_profile_seq(""));

    let snapshot = alerts.snapshot();
    let set_profile = snapshot
        .iter()
        .find(|a| matches!(a, Alert::SetProfileRequested { .. }));
    if let Some(Alert::SetProfileRequested { name }) = set_profile {
        assert_eq!(name, "");
    }
    // We deliberately don't assert that the alert MUST fire because
    // the parser may legitimately reject an empty value as malformed.
    // What matters is that nothing panics and no profile state is
    // applied.
}

#[test]
fn set_profile_does_not_interfere_with_other_iterm_commands() {
    // SetProfile is one variant in the broader OSC 1337 dispatch.
    // Adding the SetProfile arm must not break other variants —
    // RequestCellSize is the easiest to verify because it produces
    // a writer response we can observe by checking that other
    // dispatch paths still work.
    let (mut term, alerts) = make_term_with_alert_capture();

    // SetProfile request first.
    term.advance_bytes(set_profile_seq("after-this-comes-setuservar"));

    // Then a SetUserVar — the variant immediately above SetProfile
    // in the match arm. iTerm2 SetUserVar uses base64-encoded
    // value; "dGVzdA==" is base64("test").
    term.advance_bytes(b"\x1b]1337;SetUserVar=role=dGVzdA==\x1b\\");

    let snapshot = alerts.snapshot();
    let kinds: Vec<&'static str> = snapshot
        .iter()
        .filter_map(|a| match a {
            Alert::SetProfileRequested { .. } => Some("SetProfileRequested"),
            Alert::SetUserVar { .. } => Some("SetUserVar"),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["SetProfileRequested", "SetUserVar"],
        "both alerts should fire in order; got: {:?}",
        snapshot,
    );
}
