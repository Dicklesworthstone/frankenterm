# OSC 22 / 8 / 52 Cluster — Cursor + Hyperlink + Clipboard Policy

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.5.cont] / `ft-jornq`
**Parent:** `ft-7yiu2` (term-layer slice shipped at
`526e675cd` — OSC 22 dispatch added, OSC 8/52 audit tests).
**Status:** Foundation slice shipped. Cursor table + hover
state machine + clipboard policy + 21-fixture corpus + audit
doc all live; production GUI cursor / clipboard wiring is the
integration follow-on.

## Headline rule

> **OSC 52 read is default-deny.** Allowing pages to query
> the user's clipboard via `OSC 52 ; <selection> ; ?` is a
> security hole — pages can siphon credentials, secrets, and
> any other content the user copied from elsewhere. The
> default policy denies reads and prompts for writes.

## OSC 22 — Cursor mapping

`CursorShape` enumerates the **14 W3C-defined cursor names**:
`default`, `text`, `pointer`, `wait`, `crosshair`, `move`,
`not-allowed`, `help`, `ns-resize`, `ew-resize`, `nwse-resize`,
`nesw-resize`, `grab`, `grabbing`. Unknown names fall back to
`Default` per the bead's "must not crash on unknown" rule.

`native_cursor_table()` ships the per-platform mapping for
all 14 shapes × 3 OSes (macOS / Linux / Windows = 42 entries).
The integration code consumes this to call:

- macOS — `NSCursor.init(named:)` with `arrowCursor`,
  `IBeamCursor`, etc.
- Linux — `xcb_cursor_load_cursor` with `xterm`, `hand2`,
  `watch`, etc. (XCursor names; valid for Wayland via
  `wl_cursor_load_theme`).
- Windows — `LoadCursorW` with `IDC_IBEAM`, `IDC_HAND`,
  `IDC_WAIT`, etc.

`native_cursor_name(shape, os)` returns the resolved name
with safe fallback to the OS's default arrow on unmapped
combinations.

## OSC 8 — Hyperlink hover state

`HyperlinkHoverState` tracks the currently-hovered anchor so
the GUI emits the accessibility announcement **exactly once
per hover**, not once per mouse-move event. `update_hover`
returns one of:

| Outcome | Trigger |
|---|---|
| `AnchorEntered` | Cursor entered a new anchor (or moved to a different anchor) — **emit a11y announcement** |
| `NoChange` | Cursor stayed on the same anchor / non-anchor cell — no announcement |
| `AnchorLeft` | Cursor left the current anchor |

Anchors compare by `(id, url)` — gitstatus / eza-style
`OSC 8 ; id=<value>;<url>` sequences are first-class.

## OSC 52 — Clipboard policy

`ClipboardActionKind`:

| Action | Wire form | Default policy |
|---|---|---|
| `ClipboardWrite` | `OSC 52 ; <sel> ; <base64>` | `RequireApproval` (prompt) |
| `ClipboardRead` | `OSC 52 ; <sel> ; ?` | **`Deny`** |

`ClipboardPolicyTable::default()` populates these defaults.
`evaluate_clipboard(action, policy)` returns:

| Policy | Decision | Dispatcher behavior |
|---|---|---|
| `Allow` | `Allow` | Forward to OS clipboard |
| `Deny` | `Deny` | Drop silently + log |
| `RequireApproval` | `PromptUser` | Raise alert for GUI to confirm |

`Osc2xHealth::is_safe()` enforces the headline rule: if any
OSC 52 read was attempted, at least one must have been denied
— observing 100% allow-rate without explicit operator opt-in
is a gate-broken signal.

## Conformance corpus

`osc_2x_corpus()` returns **21 fixtures**:

- 14 OSC 22 cursor fixtures (one per `CursorShape`).
- 3 OSC 8 scenarios: `Simple`, `WithId`, `Nested`.
- 4 OSC 52 scenarios: `Write`, `Clear`, `QueryAllowed`,
  `QueryDenied`.

Each lives at `tests/golden/osc_<n>/<slug>/`; goldens land on
the GPU CI runner.

## Tests

22 lib tests covering: distinct cursor slugs, fallback on
unknown, slug roundtrip, native-table coverage per OS, hover
state transitions (NoChange/Entered/Left/Re-announce on
anchor change), clipboard default policies, evaluator
mapping, default-table-blocks-reads, corpus completeness,
rollout phase ordering, health safety predicate, serde
roundtrip.

## Bead acceptance status

| Item | Status |
|---|---|
| OSC 22 cursor mapping table | ✓ all 14 shapes × 3 OSes (42 mappings) |
| OSC 22 fallback on unknown name | ✓ `from_slug_with_fallback` returns `Default` |
| OSC 8 hover state machine | ✓ `HyperlinkHoverState::update_hover` |
| OSC 8 a11y announcement (debounced) | ✓ contract via `HoverOutcome::AnchorEntered` |
| OSC 52 ClipboardRead default-deny | ✓ `ClipboardPolicy::default_for(ClipboardRead) == Deny` |
| OSC 52 explicit policy gate | ✓ `evaluate_clipboard` + `ClipboardPolicyTable` |
| Conformance corpus | ✓ 21 fixture slugs (14 cursors + 3 OSC 8 + 4 OSC 52) |
| Feature-flag rollout staging | ✓ `RolloutPhase` (Hidden / OptIn / Default) |
| GUI cursor wiring (NSCursor / xcb_cursor / LoadCursorW) | ⏳ integration follow-on |
| URL handler invocation on click | ⏳ integration follow-on |
| Conformance fixture goldens | ⏳ require GUI runtime |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Parent term-layer slice:** `ft-7yiu2` shipped at
  `526e675cd`.
- **A11y harness:** `ft-mpc9b.1.6` / `ft-n0hpo` (visual
  regression fuzz lane shipped this session).
- **Sibling foundation fixtures** (same `*Health` /
  state-machine pattern this session):
  `iterm2_osc1337` (the bead one slot before this in the
  OSC cluster), `dec_2026_presentation_hold`,
  `passive_watch_invariant`, `wire_dedup_model`, etc.
- **Attestation cross-link:** `ft-syqcz.1`.
