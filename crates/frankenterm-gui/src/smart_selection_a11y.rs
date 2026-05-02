//! GUI bridge from smart-selection picks to AT-tree announcements.
//!
//! The core smart-selection substrate owns the match taxonomy and
//! announcement payload. This module is the GUI-side handoff point:
//! mouse handling supplies the picked [`SelectionMatch`] plus the
//! logical line text, and the active platform recorder receives the
//! rendered announcement event.

use frankenterm_core::a11y_tree::{AccessibilityEvent, AccessibilityRecorder, AnnouncePriority};
use frankenterm_core::smart_selection::{SelectionMatch, SmartSelectionA11yMessage};

/// Build the announcement payload for a picked smart-selection span.
///
/// Returns `None` if the span is not a valid UTF-8 slice in `line_text`;
/// callers should then skip the announcement rather than risking a
/// misleading screen-reader message.
#[must_use]
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

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::a11y_tree::{AccessibilityScenario, ContractRecorder};
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

        let mut recorder = ContractRecorder::new();
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
        let mut recorder = ContractRecorder::new();
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
}
