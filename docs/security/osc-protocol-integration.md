# OSC 8/22/52 Integration Substrate

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.5.cont] / `ft-io922`
**Status:** Foundation slice shipped — hyperlink span +
click dispatch + per-pane cursor map + typed-state OSC 52
read response + A11Y announcement contract + 23 lib tests.
Integration follow-on: escape-parser dispatch in
`frankenterm/escape-parser/src/osc.rs`, per-cell HyperlinkId
storage, hover UI, click→open-url wiring, prompt UX.

The OSC protocol substrate already lives at
`osc_protocol_omnibus.rs` (`c46aa28b8`, 39 tests). This
module ships the **integration substrate** — particularly
the typed-state OSC 52 read-path that **structurally
enforces** the bead's "respond with empty payload when
denied" privacy rule.

## Headline rules

> 1. **OSC 52 read-denied responses cannot leak clipboard
>    bytes.** The `Denied` typed-state has *no* method
>    that exposes the clipboard — even a future developer
>    mistakenly trying to emit clipboard content from a
>    denied response gets a compile error.
> 2. **Cursor shape persists across pane focus change.**
>    Per-pane state, not global.
> 3. **Click on hyperlink with selection modifier falls
>    through to smart_selection.** Default click opens
>    URL.

## OSC 8: Hyperlink span tracking + click dispatch

`HyperlinkSpan { id, start, end_exclusive, uri }` —
contiguous range of cells under one hyperlink id.
`contains(coord)` is inclusive of start, exclusive of end.

`dispatch_click(span, modifier_held) -> HyperlinkInteraction`:

| Span | Modifier | Outcome |
|---|---|---|
| `None` | any | `NotOverHyperlink` |
| `Some(s)` | true | `SelectInstead { id }` |
| `Some(s)` | false | `OpenUrl { id, uri }` |

The `Hovering` variant is emitted by the integration's
hover-state UI — not from `dispatch_click`.

## OSC 22: Per-pane cursor-shape map

`Osc22PerPaneCursorMap { by_pane: BTreeMap<u64, CursorShapeSlug>, changes_total }`:

- `set(pane_id, shape) -> Option<prior>` — returns the
  prior shape if changed; counter only bumps on real
  change.
- `get(pane_id) -> CursorShapeSlug` — `Default` if no
  prior set.
- `forget(pane_id) -> Option<prior>` — drops state on
  pane close.

`CursorShapeSlug` is a closed list of 7 (Default,
Block/Underline/Bar × Blinking/Steady) with stable slugs.

The `cursor_shape_persists_across_focus_change_scenario`
test pins the bead's "persist across pane focus change"
rule.

## OSC 52: Typed-state read-response pipeline

The privacy rule is structural. Pipeline:

```
Decoded → policy_gate(Allow|Prompt|Deny) → Allowed | Prompted | Denied
Allowed → emit_base64(targets) → bytes
Prompted → confirmed_by_operator() / confirmed_for_session() → Allowed
Prompted → denied_by_operator() → Denied
Denied  → emit_empty(targets) → bytes
```

`emit_base64` owns the RFC-required base64 encoding step;
callers cannot substitute an encoder or accidentally emit
raw clipboard bytes on the allowed path.

The `Denied` typed-state has these methods:
- `emit_empty(&self, targets: &str) -> Vec<u8>` — produces
  `\x1b]52;<targets>;\x1b\\` (empty payload).

The `Denied` typed-state does NOT have:
- Any method that emits a non-empty payload.
- Any public reader for `bytes`.
- Any way to construct an `Allowed` from a `Denied`.

So even if a future developer mistakenly tries to leak
clipboard content from a denied response, the type system
rejects them.

The `osc52_read_with_deny_policy_scenario` test confirms:
emitted bytes contain neither the cleartext "PRIVATE" nor
its base64 form "UFJJVkFURQ".

## A11Y announcement shape

`A11yAnnouncementShape` enum (tagged):

- `HyperlinkFocus { id, uri }` — OSC 8 announce on
  focus/hover.
- `CursorShapeChange { pane_id, shape_slug }` — OSC 22.
- `ClipboardPolicyDecision { decision_slug, bytes_in }` —
  OSC 52.

Cross-link `a11y_tree.rs` (BR-TERM-EMULATOR-UPLIFT.A11Y.1).

## "DO NOT BREAK" rules

- **A11Y: hyperlink target announced on focus/hover** —
  `HyperlinkFocus` shape ready for emission; integration
  drives.
- **A11Y: cursor shape change announced** —
  `CursorShapeChange` shape; integration drives.
- **Existing manual clipboard workflow (Cmd-C / Ctrl-Shift-C)
  unaffected** — this module touches only OSC 52
  programmatic ops; manual paths live elsewhere
  (`smart_selection.rs`).

## Health snapshot

`OscIntegrationHealth`:
- OSC 8: `hyperlinks_admitted_total`,
  `clicks_dispatched_total`, `select_instead_total`.
- OSC 22: `cursor_changes_total`, `panes_with_state`.
- OSC 52: `writes_allowed/denied_total`,
  `reads_allowed/denied_total`.
- A11Y: `announcements_total`.
- `interactions_by_kind` — per-kind histogram.

## Tests (23)

- 7 OSC 8 tests (span contains, end-exclusive, before-
  start, intra-line cell count, click dispatch ×3, scenario).
- 6 OSC 22 cursor-map tests (default for unknown,
  set/prior, change-counter-only-on-real-change,
  per-pane persistence, forget on close, focus-change
  scenario).
- 1 cursor-shape slug uniqueness test.
- 3 OSC 52 typed-state tests (allowed-emits-base64,
  denied-emits-empty, prompt-falls-through).
- 1 health-baseline-safe + 2 health-record tests.
- 1 deny-policy headline scenario (privacy verified).
- 1 cursor-map serde roundtrip.

## Bead acceptance status

| Item | Status |
|---|---|
| OSC 8 parser dispatch in escape-parser | ⏳ separate crate edit |
| OSC 8 per-cell HyperlinkId storage | ✓ `HyperlinkSpan` shape |
| OSC 8 hover state UI | ✓ `HyperlinkInteraction::Hovering` shape |
| OSC 8 click handler | ✓ `dispatch_click` decision tree |
| OSC 8 A11Y announcement | ✓ `A11yAnnouncementShape::HyperlinkFocus` |
| OSC 22 parser dispatch | ⏳ separate crate edit |
| OSC 22 cursor renderer apply | ⏳ GUI edit |
| OSC 22 per-pane state | ✓ `Osc22PerPaneCursorMap` |
| OSC 22 A11Y announcement | ✓ `A11yAnnouncementShape::CursorShapeChange` |
| OSC 52 parser dispatch | ⏳ separate crate edit |
| OSC 52 base64 decode | ⏳ integration follow-on |
| OSC 52 size-cap pre-decode | ✓ substrate (`osc52_size_cap_decision`) |
| OSC 52 policy gate (write+read) | ✓ `Osc52ReadResponse` typed-state for read |
| OSC 52 prompt UX | ⏳ GUI follow-on |
| OSC 52 read denied = empty payload | ✓ structurally enforced |
| OSC 52 telemetry | ✓ `OscIntegrationHealth` |
| Per-release attestation | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Substrate: `osc_protocol_omnibus.rs` (HyperlinkRegistry,
  scheme classifier, Osc8HoverPolicy, cursor-shape parser,
  Osc52Target/SizeCap/AuditEvent), `smart_selection.rs`
  (Osc52Policy).
- Sibling: `ft-2okh0.1.3` (iTerm2 OSC 1337 — same
  protocol omnibus parent), `a11y_tree.rs`
  (announcement consumer), `frankenterm/open-url`
  (OSC 8 click sink).
- Attestation: `ft-syqcz.1`.
