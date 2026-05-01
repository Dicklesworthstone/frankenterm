# iTerm2 OSC 1337 Protocol

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.3] / `ft-2okh0.1.3`
**Status:** Foundation slice shipped. 4-subcommand parser +
`MultipartFileBuffer` reassembly + security gate + health
snapshot + JSONL log row contract + 34 lib tests all live.
Production wiring (image rendering on the Kitty graphics
atlas, prompt UI surface, palette/profile applier, A11Y
high-contrast override) is the integration follow-on.

## Headline rule

> Apps emit `OSC 1337 ; <Sub>=<args> [: <payload>] BEL`.
> Four subcommands: `File` (display image inline),
> `SetColors` (palette change), `SetProfile` (theme switch),
> `MultipartFile` (chunked upload). `SetProfile` requires
> a user prompt by default; the operator can flip
> `osc1337_profile_switch = "allow"` to skip.

## Subcommand taxonomy (4 variants)

| Subcommand | Carries | Slug |
|---|---|---|
| `File` | `name`, `size`, `inline`, `base64_payload_len` | `file` |
| `SetColors` | `Vec<PaletteEntry>` (index 0..=255 + 24-bit RGB) | `set_colors` |
| `SetProfile` | profile name | `set_profile` |
| `MultipartFile` | `upload_id`, `chunk_index`, `chunk_count`, `base64_payload_len` | `multipart_file` |

`every_subcommand_has_a_slug` test pins coverage; adding a
new subcommand fails the test until both the slug map and
the parser dispatch are extended.

## Parser

`parse_osc_1337(body) -> Result<Osc1337Sub, Osc1337ParseError>`:

- Body form: `<Sub>=<key>=<value>;<key>=<value>:<base64>`.
- Errors: `UnknownSubcommand`, `MalformedArgs`, `Truncated`,
  `OutOfRange` (palette index > 255, RGB > 0xFFFFFF, chunk
  index ≥ chunk count, chunk count = 0).
- Each subcommand has its own `parse_*` helper.

Tested edge cases:

- Truncated body (`""` → `Truncated`).
- Missing `=` after subcommand name (`"File"` → `MalformedArgs`).
- Out-of-bounds palette index (`SetColors=256=...` → `OutOfRange`).
- Out-of-bounds RGB (`SetColors=0=ffffffff` → `OutOfRange`).
- Empty `SetProfile` (rejected as `MalformedArgs`).
- Multipart with `chunk >= of` rejected.
- Multipart with `of=0` rejected.

## MultipartFile reassembly

`MultipartFileBuffer` handles every reassembly anomaly the
bead names:

| Anomaly | Behavior |
|---|---|
| **In-order chunks** | `Accepted` × N, then `Complete` |
| **Out-of-order chunks** | `Accepted` × N, then `Complete` (BTreeMap orders by index) |
| **Duplicate chunk** | `Duplicate` outcome, `duplicate_count++`, payload not overwritten |
| **Missing chunk** | `is_complete()` returns false, `finalize()` returns `None` |
| **Chunk index ≥ count** | `Rejected` outcome, no state change |

The `multipart_three_chunk_reassembly` test exercises all
four happy-and-edge-case rows in one scenario.

## Security gate

`evaluate_security_gate(sub, policy) -> SecurityGateDecision`:

| Subcommand | Decision (regardless of policy) |
|---|---|
| `File` | `Allow` |
| `MultipartFile` | `Allow` |
| `SetColors` | `Allow` (palette mutation is reverted on theme reload; high-contrast override happens upstream in `accessibility_preferences`) |

| `SetProfile` | Decision depends on `policy` |
|---|---|
| `Allow` | `Allow` |
| `Prompt` (default) | `Prompt` |
| `Deny` | `Deny` |

`ProfileSwitchPolicy::default() == Prompt` per the bead's
"Default: `osc1337_profile_switch = "prompt"`".

## "DO NOT BREAK" rules

- **A11Y high-contrast** — palette mutations are gated
  upstream by `accessibility_preferences`. This module
  always emits `Allow` for `SetColors`; the applier rejects
  if high-contrast preference is set (cross-link
  `ft-mpc9b.10.5`).
- **Color management ICC profile** — palette colors flow
  through `color_management::apply_icc()` in the applier;
  parser does not touch ICC.
- **Privacy** — image bytes flow through `MultipartFileBuffer`
  + `File`'s `base64_payload_len` only; the parser never
  writes payloads to disk. The Kitty graphics atlas (cross-
  link `ft-2okh0.1.2`) handles in-memory storage.

## Telemetry

`Osc1337Health`:
- `commands_total` / `rejected_total` — lifetime counters.
- `by_subcommand` — per-slug histogram.
- `profile_switch_allows` / `profile_switch_prompts` /
  `profile_switch_denies` — three-way split per the bead's
  indicator names.
- `multipart_uploads_in_flight` / `multipart_duplicates_total` —
  reassembly observability.
- `is_safe()`: rejection rate ≤ 5% (matches the OSC family
  pattern).

## Structured logging

`StructuredLogRow` mirrors the bead's
"`ts_ns, subcommand, args_hash, accepted,
security_gate_decision, bytes`" requirement.

`render_log_jsonl` / `parse_log_jsonl` are bidirectionally
clean (`structured_log_jsonl_roundtrip` test).

## Tests (34)

- 1 subcommand-slug coverage test.
- 13 parser tests covering all 4 subcommands + 7 error
  paths + truncation/empty-body/malformed-args.
- 5 multipart reassembly tests (in-order, out-of-order,
  duplicate, missing, out-of-bounds).
- 6 security-gate tests (File/SetColors policy invariance,
  SetProfile per-policy decisions, default = Prompt).
- 4 health-snapshot tests (baseline-safe, fold-increments,
  three-way profile-switch counter split, 5%-rejection-rate
  boundary).
- 1 structured-log JSONL roundtrip test.
- 3 headline scenarios: `imgcat_scenario`,
  `theme_switch_default_prompts`,
  `multipart_three_chunk_reassembly`.

## Bead acceptance status

| Item | Status |
|---|---|
| Parser + subcommand dispatcher | ✓ `parse_osc_1337` + `Osc1337Sub` |
| `File` subcommand support (imgcat compatibility) | ✓ envelope + payload-length contract |
| `SetColors` palette mutation | ✓ parsed + bounds-checked |
| `SetProfile` security gate (prompt by default) | ✓ `ProfileSwitchPolicy::default() == Prompt` |
| `MultipartFile` reassembly (out-of-order, missing, duplicate) | ✓ `MultipartFileBuffer` |
| Telemetry: commands_total / by_subcommand / rejected | ✓ `Osc1337Health` |
| Profile-switch prompt/allow/deny counters | ✓ three-way split |
| Image rendering on Kitty graphics atlas | ⏳ integration follow-on |
| GUI prompt UI surface | ⏳ integration follow-on |
| A11Y high-contrast palette override | ⏳ wiring follow-on (cross-link `ft-mpc9b.10.5`) |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Sibling: `ft-2okh0.1.2` (Kitty graphics — `File` payload
  flows into the same atlas), `ft-2okh0.1.4` (Kitty
  keyboard — same omnibus parent), `ft-2okh0.1.5` (OSC
  8/22/52 — sibling protocols), `ft-2okh0.1.1` (DEC mode
  2026 — sibling).
- Related: `ft-tzusd` (cont sub-bead — audit/allowlist/
  rollout machinery; lives in `iterm2_osc1337.rs`),
  `ft-mpc9b.10.5` (high-contrast preference plumbing),
  `ft-mpc9b.10.3` (color management ICC).
- Attestation: `ft-syqcz.1`.
