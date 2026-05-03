//! Test-side recorder for [`crate::smart_selection`] AT-tree
//! announcements (ft-cnil8.4 scope item 3).
//!
//! Substrate slice. The bead's wired-pass items 1 + 2 are platform-
//! specific (NSAccessibility on macOS, AT-SPI on Linux); this slice
//! ships the *test fixture* the bead's scope item 3 names —
//! "simulated mouse click + AT-tree assertion" — as a reusable
//! recorder. The GUI's mouse handler stays unaware of the
//! recorder; production code emits to the platform bridge, tests
//! emit to this in-memory log instead.
//!
//! ## Design
//!
//! The recorder is a thin wrapper around `Vec<AccessibilityEvent>`
//! with a small helper API tuned to the smart-selection use case:
//! match by [`crate::smart_selection::SelectionPatternKind`],
//! locate the most recent announcement, drain on demand.
//!
//! Implementations:
//! - `RecorderHandle` — clonable shared reference (`Arc<Mutex<…>>`)
//!   so the GUI side can clone it once at fixture setup and the
//!   test asserts against the same buffer the production-side
//!   emission populates.
//! - `record(event)` — append-only.
//! - `events()` — snapshot of every recorded event.
//! - `take()` — drain.
//! - `find_announcement_for_kind(kind)` — convenience: walk the
//!   buffer in reverse and return the first AnnounceMessage whose
//!   value starts with the kind's `display_name`. Honors the
//!   smart-selection a11y rule that announcements render as
//!   `"<display_name> selected: <text>"`.
//! - `assert_announcement_for_kind(kind)` — same lookup but
//!   panics with a structured diagnostic when the announcement is
//!   missing. Returns the matched AccessibilityEvent for
//!   downstream assertions.

use std::sync::{Arc, Mutex};

use crate::a11y_tree::{
    AccessibilityEvent, AccessibilityRecorder, AccessibilityScenario, AnnouncePriority,
};
use crate::smart_selection::{SelectionPatternKind, SmartSelectionA11yMessage};

/// Cloneable handle to an in-memory recorder. Production code
/// owns a single recorder instance; tests clone the handle into
/// the GUI fixture so both sides share the underlying buffer.
#[derive(Debug, Clone, Default)]
pub struct RecorderHandle {
    inner: Arc<Mutex<Vec<AccessibilityEvent>>>,
}

impl RecorderHandle {
    /// New empty recorder. Mirrors `Default::default()` but sticks
    /// to the explicit constructor convention the rest of the
    /// substrate uses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `event` to the buffer. Lock-poisoning is treated as
    /// "buffer corrupted" and silently dropped — the recorder is
    /// a test fixture, not a load-bearing pipeline; surfacing the
    /// poison would only obscure the underlying panic that caused
    /// it.
    pub fn record(&self, event: AccessibilityEvent) {
        if let Ok(mut buf) = self.inner.lock() {
            buf.push(event);
        }
    }

    /// Convenience: render `msg` via
    /// [`SmartSelectionA11yMessage::to_announcement_event`] and
    /// record the resulting event.
    pub fn record_smart_selection(
        &self,
        msg: &SmartSelectionA11yMessage,
        ts_ms: u64,
        priority: AnnouncePriority,
    ) {
        self.record(msg.to_announcement_event(ts_ms, priority));
    }

    /// Snapshot every event recorded so far (in record order).
    /// Returns a clone so the caller can iterate without holding
    /// the lock.
    #[must_use]
    pub fn events(&self) -> Vec<AccessibilityEvent> {
        self.inner.lock().map_or_else(|_| Vec::new(), |b| b.clone())
    }

    /// Drain the buffer and return its previous contents. Used
    /// between test phases to reset the assertion surface.
    #[must_use]
    pub fn take(&self) -> Vec<AccessibilityEvent> {
        self.inner
            .lock()
            .map_or_else(|_| Vec::new(), |mut b| std::mem::take(&mut *b))
    }

    /// Convenience: locate the most recent
    /// [`AccessibilityEvent::AnnounceMessage`] whose value starts
    /// with `kind.display_name()`. The smart-selection rule renders
    /// announcements as `"<display_name> selected: <text>"`; this
    /// lookup matches that prefix without requiring callers to
    /// hard-code the format.
    #[must_use]
    pub fn find_announcement_for_kind(
        &self,
        kind: SelectionPatternKind,
    ) -> Option<AccessibilityEvent> {
        let prefix = kind.display_name();
        let buf = self.inner.lock().ok()?;
        buf.iter()
            .rev()
            .find(|event| match event {
                AccessibilityEvent::AnnounceMessage { value, .. } => value.starts_with(prefix),
                _ => false,
            })
            .cloned()
    }

    /// Same as [`find_announcement_for_kind`] but panics with a
    /// structured diagnostic when the announcement is missing.
    /// Returns the match so the test can chain further
    /// assertions on the event payload.
    pub fn assert_announcement_for_kind(&self, kind: SelectionPatternKind) -> AccessibilityEvent {
        match self.find_announcement_for_kind(kind) {
            Some(event) => event,
            None => panic!(
                "expected AnnounceMessage for {kind:?} (display_name `{prefix}`); \
                 recorder buffer: {events:?}",
                prefix = kind.display_name(),
                events = self.events(),
            ),
        }
    }

    /// Length of the underlying buffer. Cheap snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |b| b.len())
    }

    /// Whether the recorder has captured any events. Convenience
    /// for the empty-precondition assertion at fixture setup.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AccessibilityRecorder for RecorderHandle {
    fn start(&mut self, _scenario: AccessibilityScenario) {
        let _ = self.take();
    }

    fn record(&mut self, event: AccessibilityEvent) {
        RecorderHandle::record(self, event);
    }

    fn finish(&mut self) -> Vec<AccessibilityEvent> {
        self.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(kind: SelectionPatternKind, text: &str) -> SmartSelectionA11yMessage {
        SmartSelectionA11yMessage::new(kind, text)
    }

    #[test]
    fn recorder_starts_empty() {
        let r = RecorderHandle::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.events().is_empty());
    }

    #[test]
    fn record_smart_selection_round_trips_through_announce_message() {
        let r = RecorderHandle::new();
        let msg = synth(SelectionPatternKind::Url, "https://example.com");
        r.record_smart_selection(&msg, 1, AnnouncePriority::Polite);
        let events = r.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AccessibilityEvent::AnnounceMessage {
                ts_ms,
                priority,
                value,
            } => {
                assert_eq!(*ts_ms, 1);
                assert_eq!(*priority, AnnouncePriority::Polite);
                assert!(value.starts_with("URL selected:"));
                assert!(value.contains("https://example.com"));
            }
            other => panic!("unexpected event variant: {other:?}"),
        }
    }

    #[test]
    fn find_announcement_for_kind_matches_by_display_name() {
        let r = RecorderHandle::new();
        r.record_smart_selection(
            &synth(SelectionPatternKind::Email, "alice@example.com"),
            10,
            AnnouncePriority::Polite,
        );
        r.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://example.com"),
            11,
            AnnouncePriority::Polite,
        );
        let url_event = r
            .find_announcement_for_kind(SelectionPatternKind::Url)
            .expect("URL announcement");
        match url_event {
            AccessibilityEvent::AnnounceMessage { value, .. } => {
                assert!(value.starts_with("URL"));
            }
            _ => panic!("expected AnnounceMessage"),
        }
    }

    #[test]
    fn find_returns_most_recent_when_multiple_match() {
        let r = RecorderHandle::new();
        r.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://first"),
            1,
            AnnouncePriority::Polite,
        );
        r.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://second"),
            2,
            AnnouncePriority::Polite,
        );
        let event = r
            .find_announcement_for_kind(SelectionPatternKind::Url)
            .expect("URL announcement");
        match event {
            AccessibilityEvent::AnnounceMessage { value, .. } => {
                assert!(value.contains("second"));
            }
            _ => panic!("expected AnnounceMessage"),
        }
    }

    #[test]
    fn find_returns_none_when_kind_not_emitted() {
        let r = RecorderHandle::new();
        r.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://example.com"),
            1,
            AnnouncePriority::Polite,
        );
        assert!(
            r.find_announcement_for_kind(SelectionPatternKind::Email)
                .is_none()
        );
    }

    #[test]
    fn assert_announcement_for_kind_returns_match_on_success() {
        let r = RecorderHandle::new();
        r.record_smart_selection(
            &synth(SelectionPatternKind::Email, "alice@example.com"),
            1,
            AnnouncePriority::Polite,
        );
        let event = r.assert_announcement_for_kind(SelectionPatternKind::Email);
        match event {
            AccessibilityEvent::AnnounceMessage { value, .. } => {
                assert!(value.starts_with("email address"));
            }
            _ => panic!("expected AnnounceMessage"),
        }
    }

    #[test]
    #[should_panic(expected = "expected AnnounceMessage")]
    fn assert_announcement_for_kind_panics_with_diagnostic_on_miss() {
        let r = RecorderHandle::new();
        let _ = r.assert_announcement_for_kind(SelectionPatternKind::HexColor);
    }

    #[test]
    fn take_drains_the_buffer() {
        let r = RecorderHandle::new();
        r.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://example.com"),
            1,
            AnnouncePriority::Polite,
        );
        let drained = r.take();
        assert_eq!(drained.len(), 1);
        assert!(r.is_empty(), "recorder must be empty after take");
        assert!(r.events().is_empty());
    }

    #[test]
    fn handle_clone_shares_underlying_buffer() {
        // The GUI fixture clones the handle into the production
        // emit site; the test asserts against its own clone. Both
        // must see the same events.
        let a = RecorderHandle::new();
        let b = a.clone();
        a.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://example.com"),
            1,
            AnnouncePriority::Polite,
        );
        assert_eq!(b.len(), 1);
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn handle_implements_accessibility_recorder_contract() {
        let mut recorder = RecorderHandle::new();
        recorder.record_smart_selection(
            &synth(SelectionPatternKind::Url, "https://stale.example"),
            1,
            AnnouncePriority::Polite,
        );

        recorder.start(AccessibilityScenario::SelectionChange);
        AccessibilityRecorder::record(
            &mut recorder,
            synth(SelectionPatternKind::Email, "ops@example.com")
                .to_announcement_event(2, AnnouncePriority::Assertive),
        );

        let events = recorder.finish();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AccessibilityEvent::AnnounceMessage {
                ts_ms,
                priority,
                value,
            } => {
                assert_eq!(*ts_ms, 2);
                assert_eq!(*priority, AnnouncePriority::Assertive);
                assert_eq!(value, "email address selected: ops@example.com");
            }
            other => panic!("unexpected event variant: {other:?}"),
        }
        assert!(recorder.is_empty());
    }

    #[test]
    fn record_accepts_arbitrary_accessibility_events() {
        // The recorder isn't smart-selection-specific — any
        // AccessibilityEvent variant lands in the buffer. Useful
        // when the GUI fixture also emits FocusChanged /
        // SelectionChanged events alongside the announcement.
        let r = RecorderHandle::new();
        r.record(AccessibilityEvent::AnnounceMessage {
            ts_ms: 0,
            priority: AnnouncePriority::Assertive,
            value: "manual".to_string(),
        });
        assert_eq!(r.len(), 1);
    }
}
