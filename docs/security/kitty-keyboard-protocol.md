# Kitty Keyboard Progressive Enhancement Protocol

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.4] / `ft-2okh0.1.4`
**Status:** Foundation slice shipped. Flag taxonomy + bitset
+ stack with overflow guard + CSI parser + per-mode encoder
+ health snapshot + 36 lib tests all live. Production
wiring (per-pane stack storage, IME composition pipeline,
input thread integration, PTY query response) is the
integration follow-on.

## Headline rule

> Apps push a flag-mask via `CSI > <flags> u`. The flag
> mask controls how key events are encoded — disambiguate
> Tab from Ctrl-I, report event types (press/release/
> repeat), report alternate keys, escape all keys, report
> associated IME text. Stack-based: nested apps push/pop
> without affecting outer scope. Stack capped at 16
> frames; overflow rejected.

## Flag taxonomy (5 flags, bits 1/2/4/8/16)

| Bit | Flag | Effect |
|---|---|---|
| 1 | `Disambiguate` | Tab vs Ctrl-I, Enter vs Ctrl-M, Esc vs Ctrl-[ → CSI form |
| 2 | `ReportEventTypes` | press / release / repeat events distinguished |
| 4 | `ReportAlternateKeys` | lowercase/uppercase pair appended to encoding |
| 8 | `ReportAllKeysAsEscapes` | every key — including printable — uses CSI form |
| 16 | `ReportAssociatedText` | IME composition text appended as codepoint sequence |

`KittyKbdFlagSet::from_bits_truncate` masks high bits so
malformed input never escapes the 5-bit space.

## Stack semantics

- `KittyKbdStack::frames` is a `Vec<KittyKbdFlagSet>`; top
  = last element.
- `current()` reads top, returns empty set if stack is
  empty (legacy mode).
- `push(flags)` is rejected at `MAX_STACK_DEPTH = 16`
  (`pushes_rejected_total` bumps).
- `pop()` is a no-op on an empty stack but still counted
  (the operator submitted a pop op).
- Nested-app independence verified by
  `nested_push_does_not_affect_outer_scope`.

## CSI parser

`parse_csi_kbd(body)` parses the body of a CSI sequence:

| Input | Output |
|---|---|
| `> 5 u` | `Push { flags: 5 }` |
| `> u` | `Push { flags: 0 }` |
| `< u` | `Pop` |
| `? u` | `Query` |
| `> abc u` | `Err(MalformedFlags)` |
| `< 5 u` | `Err(MalformedFlags)` (extra body on pop) |
| `31 m` | `Err(NotKbdCsi)` |

`render_query_response(flags) → "\x1b[?<flags>u"` builds
the response to a query (foundation slice — production
routes this through the PTY writer).

## Per-mode encoding

`encode_key_event(event, flags) → Vec<u8>`:

- **Empty flags** (legacy):
  - Tab → `\t`, Enter → `\r`, Esc → `\x1b`, Backspace →
    `\x08`, printable codepoint → UTF-8 bytes.
  - Release events emit nothing (legacy can't see release).
- **Flag 1 (Disambiguate)** — collapsing keys go through
  CSI form: Tab → `\x1b[9u`. Printable keys still legacy.
- **Flag 2 (ReportEventTypes)** — encoding includes
  `;<modifier>:<event_code>` (press=1, repeat=2, release=3).
- **Flag 4 (ReportAlternateKeys)** — alt-key codepoint
  appended as `:<alt>`.
- **Flag 8 (ReportAllKeysAsEscapes)** — every key uses CSI
  form: `a` → `\x1b[97u`.
- **Flag 16 (ReportAssociatedText)** — IME composition
  appended as codepoint sequence: `\x1b[97;97:98u` for
  `a` with associated text "ab".

Modifiers packed as `(modifier_mask + 1)` per Kitty
protocol.

## "DO NOT BREAK" rules

The bead names three constraints; foundation slice
preserves each:

- **A11Y**: progressive enhancement only fires when an app
  pushes a flag mask. Empty-stack default (zero flags) is
  legacy encoding — existing keyboard accessibility paths
  unchanged. Stack-overflow guard prevents push storms
  from latching the pane into a non-default state.
- **IME**: flag 16 (`ReportAssociatedText`) is the seam.
  `KeyEvent.associated_text` carries the composition; the
  encoder appends codepoints. Production routes through
  the IME pipeline (cross-link `ft-mpc9b.10.2`).
- **Standard mode** (no flags): default behavior unchanged.
  `legacy_tab_emits_horizontal_tab_byte`,
  `legacy_enter_emits_carriage_return`,
  `legacy_release_event_emits_nothing` lock this in.

## Telemetry

`KittyKbdHealth`:
- `current_depth` — current stack depth.
- `max_depth_observed` — high-watermark.
- `pushes_total` / `pops_total` — lifetime counters.
- `pushes_rejected_total` — overflow-guard fires.
- `events_by_mode` — per-flag-set event histogram.
- `is_safe()`: depth within bounds AND zero rejected
  pushes.

## Tests (36)

- 3 flag-taxonomy tests (distinct bits, contains-after-set,
  truncate).
- 8 stack tests including
  `adversarial_thousand_pushes_stops_at_max_no_panic` and
  `stress_thousand_push_pop_pairs_stays_in_bounds` (the
  bead's "1000+ push/pop pairs, depth never goes
  negative" requirement).
- 8 CSI-parser tests covering all variants + 3 error
  paths.
- 11 encoder tests covering each flag's effect.
- 3 health-snapshot tests.
- `vim_progressive_enhancement_scenario` (headline scenario:
  app pushes mode 5, Tab disambiguates, app pops, Tab
  collapses again).
- 1 stack serde roundtrip test.

## Bead acceptance status

| Item | Status |
|---|---|
| Per-mode encoding correctness | ✓ proptest-equivalent unit suite |
| CSI > / < / ? u parser | ✓ `parse_csi_kbd` |
| Stack push/pop invariants (depth ≥0, ≤16) | ✓ overflow guard tested |
| Mode query response | ✓ `render_query_response` |
| Stack overflow protection | ✓ rejects beyond 16 |
| Telemetry: pushes / pops / max_depth / events_by_mode | ✓ `KittyKbdHealth` |
| Per-pane mode stack | ⏳ wiring (foundation ships the contract) |
| IME composition integration (flag 16) | ✓ encoder shape, ⏳ pipeline wiring |
| Vim/emacs/helix integration tests | ⏳ E2E follow-on |
| `--features kitty-keyboard` gating | ⏳ Cargo wiring follow-on |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Sibling: `ft-2okh0.1.1` (DEC mode 2026 — same omnibus
  parent), `ft-2okh0.1.5` (OSC 8/22/52 — ditto),
  `ft-mpc9b.10.2` (IME caret correctness — flag 16 seam),
  `ft-mpc9b.6.3` (latency-pinned input loop — encoded
  events flow through the same input thread).
- Attestation: `ft-syqcz.1`.
