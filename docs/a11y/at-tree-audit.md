# Accessibility Tree Audit

**Bead:** [BR-TERM-EMULATOR-UPLIFT.A11Y.1] / `ft-mpc9b.10.1`
**Scope:** What does the renderer emit to the platform accessibility (AT)
tree today, what *should* it emit per
[`scenario-corpus.md`](scenario-corpus.md), and where does the gap close?

The audit is a **baseline**: a renderer change can land green today and
silently break a screen-reader user (VoiceOver / Orca / Narrator). Until
the gaps below are closed, the regression fixture in
`crates/frankenterm-core/tests/a11y_regression_fixture.rs` operates in
**contract mode** — it pins the *expected* AT event sequences against a
synthetic recorder so we can land per-platform integrations without
re-deriving the contract each time.

## Headline finding

| Platform   | AT framework         | Currently emitted | Gap                                       |
| ---------- | -------------------- | ----------------- | ----------------------------------------- |
| macOS      | `NSAccessibility`    | Nothing.          | No `NSAccessibilityPostNotification`; no `NSAccessibility` protocol implementation on the content view; no role/title/value declarations.|
| Linux/X11  | AT-SPI               | Nothing.          | No `_NET_WM_NAME` accessible-role hint; no atspi-rs registration; no event-bus emissions on focus / selection / scroll.|
| Linux/Wayland | AT-SPI            | Nothing.          | Same as X11; the Wayland window does not register as an AT-SPI accessible at all.|
| Windows    | UI Automation        | Out of scope (bead).| Marked OOS until the rest of the fixture lands.|

Across all 3 supported platforms, the renderer dispatches
`WindowEvent::FocusChanged(bool)` to the application event bus but emits
**zero** AT-tree events. A blind operator running ft today gets nothing
from their screen reader on focus, selection, dialog-open, or scroll.

## Code citations

### macOS — `frankenterm/window/src/os/macos/window.rs`

- `did_become_key` (line ~2329) and `did_resign_key` (line ~2339) handle
  the platform focus signal but only dispatch `WindowEvent::FocusChanged`.
  No `NSAccessibilityPostNotification(focusedUIElementChanged, …)` call.
- The custom `NSView` subclass (search "extern \"C\"" handlers in the
  same file) does not implement `accessibilityRole`,
  `accessibilityLabel`, `accessibilityValue`, or `accessibilityFrame` —
  so VoiceOver inspects the view as an opaque rect with no role.
- Selection state lives in `frankenterm/window/src/os/macos/window.rs`
  but no `NSAccessibilitySelectedTextChanged` notification is fired
  when the selection mutates.

**Integration points:** the `did_become_key` /
`did_resign_key` bodies, plus the selection mutation site, plus a new
`accessibility*` method block on the content view.

### Linux/X11 — `frankenterm/window/src/os/x11/window.rs`

- Focus dispatch at line 343 (`WindowEvent::FocusChanged(focused)`) and
  line 879. No AT-SPI registration; no event-bus message on `focus_in`.
- The pre-existing comment at line 826 ("accessibility settings change
  the text size") refers to the GTK/dconf font-scale accessibility hint
  — a *consumer* of the system a11y settings, not an *emitter* into
  the AT tree.
- No `atspi`/`atspi-connection`/`atspi-common` imports anywhere in the
  workspace (`grep -rn 'atspi' .`).

**Integration points:** the focus dispatch sites, the selection-update
site (split between mux and renderer), and a new module that owns the
AT-SPI bus connection lifecycle.

### Linux/Wayland — `frankenterm/window/src/os/wayland/window.rs`

- Focus dispatch at line 1273 (`WindowEvent::FocusChanged(focused)`).
- The Wayland protocol does not define an accessibility surface; on
  Linux the AT-SPI registration is shared with the X11 implementation
  and routes through `org.a11y.atspi.Registry` over D-Bus regardless of
  the windowing protocol.

**Integration points:** identical to X11 once the shared AT-SPI module
exists; the Wayland window only needs to plumb the focus / selection
hooks into it.

### Windows — `frankenterm/window/src/os/windows/`

The bead explicitly classifies UI Automation as out-of-scope for this
slice. The fixture's `Recorder` enum carries a `Windows` variant for
plumbing parity but its concrete recorder is intentionally a
no-op-with-clear-error stub.

## What the fixture pins

Because no platform code emits AT events today, the fixture cannot
*observe* a real recording. Instead it pins:

1. The **scenario corpus** (the 5 scenarios from the bead description,
   re-using the dirty-event taxonomy from `ft-mpc9b.1.6`). See
   [`scenario-corpus.md`](scenario-corpus.md).
2. The **expected AT event sequence** per scenario as a Rust value (the
   contract). When a real recorder lands, its captured stream is
   compared against this contract — divergence is a regression.
3. The **structured-logging schema** for `tests/a11y/logs/<platform>-<scenario>.jsonl`.
   The schema is enforced by serde + a JSON Schema dump so future
   recorders can't drift.
4. **Ordering invariants** that hold regardless of platform (e.g.
   focus-changed fires at most once per scenario step;
   selection-changed only fires while focused; scroll-position events
   carry monotonically advancing timestamps).

## Closure plan

The follow-on integration work that fills the gap above lives in
sibling beads under `ft-mpc9b.10`:

- **Per-platform integration beads** (one each for macOS / Linux): wire
  the platform AT framework into the `AccessibilityRecorder` trait
  emitted by this bead, using the contract above as the regression
  oracle.
- **Goldens lane**: once the integrations land, replace the contract
  recorder with the real one in CI, generating goldens at
  `tests/a11y/golden/<platform>-<scenario>.jsonl`.
- **CI gate**: a renderer-touching PR runs the fixture; any
  divergence from the goldens fails the PR.
