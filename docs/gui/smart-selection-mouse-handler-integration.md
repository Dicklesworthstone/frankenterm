# Smart-selection mouse handler — GUI integration runbook

**Bead:** `ft-cnil8` (BR-TERM-EMULATOR-UPLIFT-2.14.cont) +
sub-beads `ft-cnil8.1` / `ft-cnil8.2` / `ft-cnil8.4`.
**In-core substrate:** `crates/frankenterm-core/src/smart_selection.rs`
+ `smart_selection_patterns.rs` + `a11y_tree.rs`.

This doc captures the wired-pass handoff shape for the
frankenterm-gui mouse handler engineer: how the in-core
substrate (pattern catalog, classifier, A11y bridge) plugs
into the GUI's existing click → selection pipeline without
duplicating logic in the GUI crate.

The substrate ships under three sibling sub-beads, all with
substrate-pass landed:

| Sub-bead | Slice | Commit | What ships |
|----------|-------|--------|-----------|
| ft-cnil8.4 | a11y bridge | `96004cffc` | `SmartSelectionA11yMessage::to_announcement_event(ts_ms, AnnouncePriority) -> AccessibilityEvent::AnnounceMessage` |
| ft-cnil8.2 | dispatcher | `668e8d662` | `ClickKind::from_click_count(u32)` + `classify_click(kind, candidates, click_pos, line_start, line_end) -> Option<SelectionMatch>` |
| ft-cnil8.1 | pipeline | `ed1654f0b` | `pick_click_target(line_text, click_pos, kind) -> Option<SelectionMatch>` |

## GUI integration shape (2-call sequence)

The GUI mouse handler becomes:

```rust
use frankenterm_core::a11y_tree::{AccessibilityRecorder, AnnouncePriority};
use frankenterm_core::smart_selection::{
    pick_click_target, ClickKind, SmartSelectionA11yMessage,
};

fn handle_mouse_click(
    line_text: &str,         // line under cursor
    click_pos: usize,        // byte offset within line_text
    click_count: u32,        // 1, 2, 3, ... from debouncer
    ts_ms: u64,              // event timestamp
    recorder: &mut dyn AccessibilityRecorder,
) -> Option<SelectionSpan> {
    // Step 1: classify the click. Pure-logic; pulls from the
    // catalog regex + drop_shell_quoted_supersets pre-filter
    // + classify_click dispatcher.
    let kind = ClickKind::from_click_count(click_count);
    let pick = pick_click_target(line_text, click_pos, kind)?;

    // Step 2: emit AT-tree announcement. The a11y bridge maps
    // the SelectionMatch's kind to the canonical "URL selected:
    // ..." / "email selected: ..." / etc. announcement.
    let selected_text = &line_text[pick.span_start..pick.span_end];
    let msg = SmartSelectionA11yMessage::new(pick.kind, selected_text);
    recorder.record(msg.to_announcement_event(ts_ms, AnnouncePriority::Polite));

    Some(SelectionSpan {
        start: pick.span_start,
        end: pick.span_end,
    })
}
```

That's the entire GUI-side wiring. 5 lines of substrate use +
the existing word-boundary fallback path stays as the `None`
return-value handler.

## Click-count debouncing (GUI-side, scope item 1 of ft-cnil8.1)

The substrate's `ClickKind::from_click_count` accepts a `u32`;
the GUI is responsible for accumulating the count across rapid
clicks. The browser-style threshold:

```rust
const RAPID_CLICK_WINDOW_MS: u64 = 500;
const MAX_CLICK_COUNT: u32 = 3; // beyond 3 = PlainFallback

struct ClickAccumulator {
    last_click_ms: u64,
    last_click_pos: (u32, u32), // cell coords
    consecutive_count: u32,
}

impl ClickAccumulator {
    fn count_for_event(
        &mut self,
        now_ms: u64,
        cell_pos: (u32, u32),
    ) -> u32 {
        let recent = now_ms.saturating_sub(self.last_click_ms)
            <= RAPID_CLICK_WINDOW_MS;
        let same_pos = cell_pos == self.last_click_pos;
        if recent && same_pos {
            self.consecutive_count =
                (self.consecutive_count + 1).min(MAX_CLICK_COUNT);
        } else {
            self.consecutive_count = 1;
        }
        self.last_click_ms = now_ms;
        self.last_click_pos = cell_pos;
        self.consecutive_count
    }
}
```

## Cell coords → byte offset (GUI-side, scope item 1)

The substrate accepts a byte offset (`click_pos: usize`); the
GUI translates from `(line_idx, cell_x)` via the existing line-
text encoding. For ASCII this is straightforward; for multi-
byte chars / wide cells the GUI's grid module already maintains
the cell→byte map.

```rust
fn cell_to_byte_offset(line_text: &str, cell_x: u32) -> usize {
    line_text
        .char_indices()
        .nth(cell_x as usize)
        .map(|(i, _)| i)
        .unwrap_or(line_text.len())
}
```

## Word-boundary fallback (scope item 6)

The substrate returns `None` from `pick_click_target` when no
smart-selection pattern matches; the GUI falls back to its
existing word-boundary or line-span selection (the same path
that fires today on plain-text clicks). No new fallback code
needed in the GUI — just route the `None` return-value through
the existing word-selection handler.

## AT-tree recorder (scope item 1 of ft-cnil8.4)

The `AccessibilityRecorder` trait surface is the single
integration point for AT-SPI (Linux) and NSAccessibility
(macOS). The smart-selection handler doesn't touch the
recorder's platform-specific binding; it just calls
`recorder.record(event)` with the bridge-produced
`AccessibilityEvent::AnnounceMessage`.

The platform recorders ship under
`crates/frankenterm-gui/src/a11y/` (per-OS modules); they
wrap the `record` call into `NSAccessibilityPostNotification`
or AT-SPI's announcement signal.

## Test fixture (scope item 3 of ft-cnil8.4)

The substrate ships unit tests covering every classifier path.
The GUI integration test the bead asks for ("simulated mouse
click + AT-tree assertion") composes those into an end-to-end
fixture:

```rust
#[test]
fn double_click_url_emits_announcement() {
    use frankenterm_core::a11y_tree::ContractRecorder;
    use frankenterm_core::smart_selection::*;

    let mut recorder = ContractRecorder::new();
    let line = "Visit https://example.com today";
    let click_pos = 15; // mid-URL
    handle_mouse_click(line, click_pos, 2, 0, &mut recorder);

    let events = recorder.snapshot();
    assert_eq!(events.len(), 1);
    matches!(
        events[0],
        AccessibilityEvent::AnnounceMessage { value, .. }
            if value == "URL selected: https://example.com"
    );
}
```

The substrate's `ContractRecorder` (already in `a11y_tree.rs`)
provides the test-shaped recorder so this fixture lands without
needing the platform-specific bindings.

## Cross-references

- Substrate: [`crates/frankenterm-core/src/smart_selection.rs`](../../crates/frankenterm-core/src/smart_selection.rs)
- Pattern catalog: [`crates/frankenterm-core/src/smart_selection_patterns.rs`](../../crates/frankenterm-core/src/smart_selection_patterns.rs)
- AT-tree: [`crates/frankenterm-core/src/a11y_tree.rs`](../../crates/frankenterm-core/src/a11y_tree.rs)
- ft-cnil8.1 substrate-pass: `ed1654f0b`
- ft-cnil8.2 substrate-pass: `668e8d662`
- ft-cnil8.4 substrate-pass: `96004cffc` + `d99d6d0f4`
