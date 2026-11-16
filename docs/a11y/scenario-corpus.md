# Accessibility Scenario Corpus

**Bead:** [BR-TERM-EMULATOR-UPLIFT.A11Y.1] / `ft-mpc9b.10.1`

This is the canonical list of scenarios that any
`AccessibilityRecorder` implementation must reproduce verbatim. The
list is **closed** for the foundation slice — adding a sixth scenario
requires a follow-on bead so the contract stays under explicit
revision control.

The five scenarios mirror the dirty-event taxonomy in
`crates/frankenterm-gui/src/gpu_regression_fuzz.rs::FuzzInputEvent`
(`ft-mpc9b.1.6`) so the AT regression lane and the visual regression
lane consume the same input streams under different recorders.

For each scenario, the table lists:

- The **trigger** — what the renderer / mux processes that should
  produce an AT event.
- The **required AT event sequence** in canonical order. This is the
  contract every per-platform recorder must reproduce; mapping to the
  platform-specific notification (`AXFocusedUIElementChanged`,
  `org.a11y.atspi.Event.Focus`, …) is the recorder's responsibility.
- The **JSONL log path** so the goldens lane has a stable name.

## 1. `steady_typing`

**Trigger.** A `FuzzInputEvent::Write { bytes }` burst arrives at the
focused pane while the buffer is at the bottom of the scrollback.

**Required AT events.** The recorder MUST produce one
`TextValueChanged` event per visible cell mutation, in scanline
(row-major) order. Subsequent identical bytes (autorepeat) coalesce
to a single event whose `value` carries the full run.

```text
[
  TextValueChanged { role: "Terminal", name: "pane:<id>", value: "<text>" }
]
```

**Log:** `tests/a11y/logs/<platform>-steady_typing.jsonl`

## 2. `pane_focus_change`

**Trigger.** The window receives `WindowEvent::FocusChanged(true)`
from the platform (or the user explicitly focuses a different pane via
the mux).

**Required AT events.**

```text
[
  FocusChanged { role: "Terminal", name: "pane:<new_id>" },
]
```

**Log:** `tests/a11y/logs/<platform>-pane_focus_change.jsonl`

Special: when the focus *leaves* the window (`FocusChanged(false)`)
the recorder MUST NOT emit a `FocusChanged` event with an empty
`name` — instead, the platform recorder elides the announcement so
the screen reader's last-known focused pane stays as the pointer of
reference.

## 3. `dialog_open`

**Trigger.** A modal overlay is opened (e.g. confirm-close-pane,
command-palette).

**Required AT events.**

```text
[
  WindowOpened     { role: "Dialog",      name: "<title>" },
  FocusChanged     { role: "Dialog",      name: "<title>" },
  AnnounceMessage  { kind: "Assertive",   value: "<title>" }
]
```

`AnnounceMessage::Assertive` is the AT-framework-agnostic equivalent
of `NSAccessibilityAnnouncementPriorityHigh` /
`org.a11y.atspi.Event.Announcement` with priority=high.

**Log:** `tests/a11y/logs/<platform>-dialog_open.jsonl`

## 4. `selection_change`

**Trigger.** A `FuzzInputEvent::SelectStart` →
`FuzzInputEvent::SelectExtend`* → `FuzzInputEvent::SelectEnd` triple
that produces a non-empty selection inside the focused pane.

**Required AT events.**

```text
[
  SelectionChanged {
    role: "Terminal",
    name: "pane:<id>",
    range_start_line: u32,
    range_start_col:  u32,
    range_end_line:   u32,
    range_end_col:    u32,
  }
]
```

The recorder MUST coalesce a contiguous extend-selection drag into a
single `SelectionChanged` carrying the *final* range so screen readers
don't read partial drags character-by-character.

**Log:** `tests/a11y/logs/<platform>-selection_change.jsonl`

## 5. `scroll_position_change`

**Trigger.** A `FuzzInputEvent::Scroll { lines }` event that moves
the viewport more than 1 row.

**Required AT events.**

```text
[
  ScrollPositionChanged {
    role: "Terminal",
    name: "pane:<id>",
    viewport_top_line: u32,
    viewport_bottom_line: u32,
  }
]
```

Single-line scrolls (`lines.abs() == 1`) intentionally do not announce
to avoid flooding the screen reader during steady output bursts; this
matches VoiceOver's default `aria-live="polite"` debouncing.

**Log:** `tests/a11y/logs/<platform>-scroll_position_change.jsonl`

## Cross-scenario invariants

Independent of platform, the recorder must obey:

- **Focus exclusivity.** At most one pane carries the focused role at
  any timestamp. A `FocusChanged` event MUST be preceded by either
  another `FocusChanged` event (with a different `name`) or by the
  start of the scenario stream.
- **Selection liveness.** A `SelectionChanged` event MUST NOT appear
  before the most recent `FocusChanged` event for the same pane.
- **Monotonic timestamps.** Every event has a u64 timestamp (ms);
  within a single scenario run the timestamp is strictly
  non-decreasing.
- **Schema stability.** Every event serializes through serde to the
  `AccessibilityEvent` enum; an unknown variant in a JSONL log is a
  schema-drift bug, not a soft failure.

These invariants are enforced as proptest properties in the regression
fixture — the recorder doesn't have to opt in.
