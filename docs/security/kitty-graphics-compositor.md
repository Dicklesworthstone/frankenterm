# Kitty Graphics Compositor + Frame-Budget Integration

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.2.cont] / `ft-jwst6`
**Status:** Foundation slice shipped — layered-compositor
placement decision tree + query-response builder + base64
validator + frame-budget op classification + structured-
log row + 28 lib tests. Integration follow-on: APC parser
in escape-parser crate, image-crate decode pipeline, GPU
atlas upload, asupersync wiring, A11Y tree integration.

The Kitty graphics protocol substrate already lives at
`kitty_graphics.rs` (`7b050b6db`, 39 tests) + alt-text at
`kitty_graphics_alt_text.rs` (33 tests). This module ships
the **integration substrate** the bead's continuation
work consumes.

## Headline rules

> 1. **Decode async, off render thread.** The bead's
>    "atlas upload respects frame budget" rule is encoded
>    via `KittyFrameBudgetOp::DecodeAsync.runs_on_render_thread()
>    == false`.
> 2. **Virtual-placement images render below text;
>    Classical-placement floats above.** Encoded via
>    `layer_for_placement` decision tree.
> 3. **Malformed base64 surfaces `Eninput` before
>    decode budget burns.** Encoded via
>    `validate_base64_payload`.

## Layered-compositor placement (sub-task 5)

`CompositorLayer` is a closed list of 5 layers:

| Layer | z-index | Purpose |
|---|---|---|
| `Background` | 0 | Bg images (Virtual placement) |
| `Selection` | 1 | Selection highlight |
| `Text` | 2 | Glyphs |
| `Cursor` | 3 | Cursor block |
| `Overlay` | 4 | Floating images (Classical) |

`layer_for_placement(mode)`:

| Placement Mode | Layer |
|---|---|
| `Virtual` | `Background` |
| `UnicodePlaceholder` | `Background` |
| `Classical` | `Overlay` |

`z_index()` is strictly monotonic — `z_index_strictly_ordered`
test pins it.

The two headline scenarios are tests
`images_render_below_text_via_layer_z_order` and
`classical_images_render_above_text`.

## Query-response format builder (sub-task 6)

`render_query_response(outcome) -> Vec<u8>`:

- OK: `\x1b_Gi=<id>;OK\x1b\\`
- Error: `\x1b_Gi=<id>;<ERROR_CODE>\x1b\\`

`KittyErrorCode` enum (closed list of 6):
`ENOFILE` / `ENINPUT` / `EIMAGEDATA` / `EFORMAT` / `ENOIMG`
/ `EUNSUPP`. Each has a stable slug.

## Base64-payload validator (sub-task 2)

`validate_base64_payload(payload) -> Base64ValidationOutcome`:

- `Valid { decoded_len_estimate }` — sized correctly,
  alphabet OK; integration sizes its decode buffer.
- `InvalidLength` — length ≡ 1 (mod 4) (always invalid).
- `InvalidAlphabet` — character outside `[A-Za-z0-9+/-_=]`.
- `OverChunkCap` — exceeds `PER_CHUNK_BASE64_CAP = 4096`.

URL-safe base64 (`-` and `_`) accepted (Kitty protocol may
emit either form).

## Frame-budget op classification

`KittyFrameBudgetOp` partitions Kitty work for the
frame-budget allocator (`frame_budget_signal_coupling.rs`,
shipped earlier this run):

| Op | runs_on_render_thread | Slug |
|---|---|---|
| `DecodeAsync` | **false** (off-thread per bead rule) | `kitty_decode_async` |
| `AtlasUpload` | true | `kitty_atlas_upload` |
| `CompositorPlacement` | true | `kitty_compositor_placement` |
| `AtlasEviction` | true | `kitty_atlas_eviction` |

`decode_async_does_not_run_on_render_thread` test pins the
bead's stated rule.

## "DO NOT BREAK" rules

- **A11Y alt-text** — substrate
  (`kitty_graphics_alt_text.rs`) handles. This module
  doesn't touch alt-text routing; cross-link
  `ft-h8s0p`.
- **Privacy: image bytes in-memory only** — the foundation
  slice ships byte-counts only; payload is never persisted
  in the structured-log rows.
- **Frame budget** — `DecodeAsync` runs off-thread; atlas
  upload is `Cosmetic` priority (defers under pressure).
  Composes with `frame_budget_signal_coupling`.

## Structured logging (sub-task 8)

`StructuredLogRow` enum (tagged):

- `ImageAdmitted { ts_ms, image_id, format_slug, bytes_in,
  bytes_out, decode_ns, layer_slug }`
- `ImageRejected { ts_ms, image_id, reason_slug }`
- `QueryResponse { ts_ms, action_slug, image_id,
  response_slug }`
- `EvictionCycle { ts_ms, evicted_count, freed_bytes }`

Bidirectionally clean via `render_log_jsonl` /
`parse_log_jsonl`.

## Health snapshot

`KittyCompositorHealth`:
- `admitted_total` / `rejected_total` — lifetime counters.
- `by_placement_layer` — per-layer admission histogram.
- `frame_budget_ops_by_kind` — per-`KittyFrameBudgetOp`
  histogram.
- `query_responses_total` / `query_errors_total`.
- `is_safe()`: rejection rate ≤ 5%.

## Tests (28)

- 6 layered-compositor tests (z-index ordering, 3
  placement-mode mappings, headline below-text +
  above-text scenarios).
- 3 query-response tests (OK, Error, error-code uniqueness).
- 6 base64 validator tests (valid, invalid alphabet,
  invalid length, over-cap, empty, URL-safe).
- 4 frame-budget op classification tests + 1 slug
  uniqueness.
- 1 structured-log JSONL roundtrip.
- 4 health-snapshot tests.
- 3 headline scenarios:
  `imgcat_inline_image_admission_scenario`,
  `floating_image_above_text_scenario`,
  `malformed_payload_emits_eninput_response`.

## Bead acceptance status

| Sub-task | Status |
|---|---|
| 1 — APC parser in escape-parser | ⏳ separate crate edit |
| 2 — Base64 decode validation | ✓ `validate_base64_payload` |
| 3 — PNG/zlib/raw decode pipeline | ⏳ image-crate integration |
| 4 — GPU atlas upload | ⏳ sparse_texture_atlas wiring |
| 5 — Layered compositor placement | ✓ `layer_for_placement` decision tree |
| 6 — Query response emission | ✓ `render_query_response` |
| 7 — Alt-text accessibility | ✓ shipped at `ft-h8s0p` (substrate `kitty_graphics_alt_text.rs`) |
| 8 — Structured JSONL telemetry | ✓ `StructuredLogRow` |
| Frame budget composition | ✓ `KittyFrameBudgetOp` op classification |
| Per-release attestation | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Substrate: `kitty_graphics.rs`, `kitty_graphics_alt_text.rs`.
- Sibling: `frame_budget_signal_coupling.rs` (this run —
  `KittyFrameBudgetOp` projects onto its `OpKindSlug`),
  `sparse_texture_atlas.rs` (sub-task 4 atlas target),
  `a11y_tree.rs` (sub-task 7 alt-text consumer).
- Attestation: `ft-syqcz.1`.
