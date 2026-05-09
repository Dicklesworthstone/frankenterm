//! GUI bridge from smart-selection picks to AT-tree announcements.
//!
//! The core smart-selection substrate owns the match taxonomy and
//! announcement payload. This module is the GUI-side handoff point:
//! mouse handling supplies the picked [`SelectionMatch`] plus the
//! logical line text, and the active platform recorder receives the
//! rendered announcement event.
//!
//! ## Production wiring
//!
//! [`shared_smart_selection_recorder`] returns a process-wide
//! [`RecorderHandle`] (the cloneable test-recorder substrate from
//! `frankenterm-core::smart_selection_a11y_recorder`). The GUI mouse
//! handler emits to it via [`emit_smart_selection_pick`] whenever
//! `SelectionRange::smart_or_word_around` resolves to a smart
//! pattern. Word-boundary fallbacks emit nothing — keeps screen
//! readers quiet on plain word picks.
//!
//! Tests clone the handle via `shared_smart_selection_recorder().clone()`
//! to read the captured events. The platform AT bridges
//! (NSAccessibility / AT-SPI / UIA) install themselves by draining
//! this recorder periodically + forwarding events to the OS layer
//! once that wiring lands; until then the recorder doubles as the
//! production-runtime event log.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use frankenterm_core::a11y_tree::{AccessibilityEvent, AccessibilityRecorder, AnnouncePriority};
use frankenterm_core::smart_selection::{
    SelectionMatch, SelectionPatternKind, SmartSelectionA11yMessage,
};
use frankenterm_core::smart_selection_a11y_recorder::RecorderHandle;

/// Build the announcement payload for a picked smart-selection span.
///
/// Returns `None` if the span is not a valid UTF-8 slice in `line_text`;
/// callers should then skip the announcement rather than risking a
/// misleading screen-reader message.
#[must_use]
#[allow(dead_code)]
pub fn smart_selection_a11y_message(
    line_text: &str,
    selection: SelectionMatch,
) -> Option<SmartSelectionA11yMessage> {
    let selected_text = line_text.get(selection.span_start..selection.span_end)?;
    Some(SmartSelectionA11yMessage::new(
        selection.kind,
        selected_text,
    ))
}

/// Convert a picked smart-selection span into an AT-tree announcement
/// event without recording it.
#[must_use]
#[allow(dead_code)]
pub fn smart_selection_announcement_event(
    line_text: &str,
    selection: SelectionMatch,
    ts_ms: u64,
    priority: AnnouncePriority,
) -> Option<AccessibilityEvent> {
    smart_selection_a11y_message(line_text, selection)
        .map(|message| message.to_announcement_event(ts_ms, priority))
}

/// Record a picked smart-selection span through the supplied platform
/// recorder.
///
/// Production callers pass the recorder backing NSAccessibility /
/// AT-SPI; tests pass the in-memory contract recorder.
#[allow(dead_code)]
pub fn record_smart_selection_announcement<R: AccessibilityRecorder + ?Sized>(
    recorder: &mut R,
    line_text: &str,
    selection: SelectionMatch,
    ts_ms: u64,
    priority: AnnouncePriority,
) -> Option<SmartSelectionA11yMessage> {
    let message = smart_selection_a11y_message(line_text, selection)?;
    recorder.record(message.to_announcement_event(ts_ms, priority));
    Some(message)
}

/// Process-wide [`RecorderHandle`] the GUI mouse handler emits to.
/// Initialised lazily on first access; the handle is `Arc`-shared
/// so platform AT bridges and tests can each hold a clone and read
/// from the same buffer.
pub fn shared_smart_selection_recorder() -> &'static RecorderHandle {
    static SHARED: OnceLock<RecorderHandle> = OnceLock::new();
    SHARED.get_or_init(RecorderHandle::default)
}

/// Emit a [`SmartSelectionA11yMessage`] for the picked `kind` + `text`
/// to the process-wide recorder. Used by the GUI mouse handler when
/// `SelectionRange::smart_or_word_around` returns a smart match —
/// word-boundary fallback callers MUST NOT call this so screen
/// readers stay quiet on plain word picks (ft-cnil8.4 acceptance).
///
/// Timestamp is derived from `SystemTime::now()`; priority defaults
/// to `Polite` so announcements queue behind any active assertive
/// scenario (e.g., notifications, alerts).
pub fn emit_smart_selection_pick(kind: SelectionPatternKind, text: &str) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let message = SmartSelectionA11yMessage::new(kind, text);
    shared_smart_selection_recorder().record_smart_selection(
        &message,
        ts_ms,
        AnnouncePriority::Polite,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::a11y_tree::AccessibilityScenario;
    use frankenterm_core::smart_selection::SelectionPatternKind;

    fn selection_for(line: &str, kind: SelectionPatternKind, needle: &str) -> SelectionMatch {
        let start = line.find(needle).expect("needle exists");
        SelectionMatch::new(kind, start, start + needle.len())
    }

    #[test]
    fn message_renders_catalog_label_and_selected_text() {
        let line = "open https://example.com/docs now";
        let selection = selection_for(line, SelectionPatternKind::Url, "https://example.com/docs");

        let message = smart_selection_a11y_message(line, selection).expect("message");

        assert_eq!(message.render(), "URL selected: https://example.com/docs");
    }

    #[test]
    fn announcement_event_preserves_timestamp_and_priority() {
        let line = "color #aabbcc selected";
        let selection = selection_for(line, SelectionPatternKind::HexColor, "#aabbcc");

        let event =
            smart_selection_announcement_event(line, selection, 42, AnnouncePriority::Assertive)
                .expect("event");

        assert_eq!(
            event,
            AccessibilityEvent::AnnounceMessage {
                ts_ms: 42,
                priority: AnnouncePriority::Assertive,
                value: "hex color selected: #aabbcc".to_string(),
            }
        );
    }

    #[test]
    fn recorder_receives_url_email_path_and_color_announcements() {
        let fixtures = [
            (
                "open https://example.com",
                SelectionPatternKind::Url,
                "https://example.com",
                "URL selected: https://example.com",
            ),
            (
                "mail ops@example.com",
                SelectionPatternKind::Email,
                "ops@example.com",
                "email address selected: ops@example.com",
            ),
            (
                "tail /var/log/frankenterm.log",
                SelectionPatternKind::UnixPath,
                "/var/log/frankenterm.log",
                "Unix path selected: /var/log/frankenterm.log",
            ),
            (
                "theme #112233",
                SelectionPatternKind::HexColor,
                "#112233",
                "hex color selected: #112233",
            ),
        ];

        let mut recorder = RecorderHandle::new();
        recorder.start(AccessibilityScenario::SelectionChange);

        for (idx, (line, kind, needle, expected)) in fixtures.into_iter().enumerate() {
            let selection = selection_for(line, kind, needle);
            let message = record_smart_selection_announcement(
                &mut recorder,
                line,
                selection,
                idx as u64,
                AnnouncePriority::Polite,
            )
            .expect("recorded message");
            assert_eq!(message.render(), expected);
        }

        let events = recorder.finish();
        let rendered = events
            .iter()
            .map(|event| match event {
                AccessibilityEvent::AnnounceMessage { value, .. } => value.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "URL selected: https://example.com",
                "email address selected: ops@example.com",
                "Unix path selected: /var/log/frankenterm.log",
                "hex color selected: #112233",
            ]
        );
    }

    #[test]
    fn invalid_utf8_span_is_not_recorded() {
        let line = "wide 表 glyph";
        let start = line.find('表').expect("wide char exists") + 1;
        let selection = SelectionMatch::new(SelectionPatternKind::UnixPath, start, start + 1);
        let mut recorder = RecorderHandle::new();
        recorder.start(AccessibilityScenario::SelectionChange);

        let message = record_smart_selection_announcement(
            &mut recorder,
            line,
            selection,
            7,
            AnnouncePriority::Polite,
        );

        assert!(message.is_none());
        assert!(recorder.finish().is_empty());
    }

    /// Serialise tests that touch the process-wide
    /// [`shared_smart_selection_recorder`] so concurrent test
    /// execution doesn't race on its buffer.
    fn shared_recorder_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // PoisonError is fine here — a panicked sibling test
        // shouldn't block the next.
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn emit_smart_selection_pick_lands_in_shared_recorder() {
        let _guard = shared_recorder_test_lock();
        // Drain any prior events so the assertion sees only this
        // test's emission.
        let _ = shared_smart_selection_recorder().take();

        emit_smart_selection_pick(SelectionPatternKind::Url, "https://example.com/sentinel");

        let event = shared_smart_selection_recorder()
            .find_announcement_for_kind(SelectionPatternKind::Url)
            .expect("URL announcement present");
        match event {
            AccessibilityEvent::AnnounceMessage {
                value, priority, ..
            } => {
                assert_eq!(value, "URL selected: https://example.com/sentinel");
                assert_eq!(priority, AnnouncePriority::Polite);
            }
            other => panic!("expected AnnounceMessage, got {other:?}"),
        }

        // Cleanup so subsequent tests have a clean buffer.
        let _ = shared_smart_selection_recorder().take();
    }

    #[test]
    fn shared_recorder_is_a_singleton_across_calls() {
        let _guard = shared_recorder_test_lock();
        // Two calls return the same handle (same backing Arc<Mutex<…>>),
        // so an event recorded via one accessor is visible via another.
        let _ = shared_smart_selection_recorder().take();
        emit_smart_selection_pick(SelectionPatternKind::Email, "ops@example.com");
        emit_smart_selection_pick(SelectionPatternKind::HexColor, "#abcdef");

        let len_via_first = shared_smart_selection_recorder().len();
        let len_via_second = shared_smart_selection_recorder().len();
        assert_eq!(len_via_first, len_via_second);
        assert_eq!(len_via_first, 2);

        let _ = shared_smart_selection_recorder().take();
    }
}
