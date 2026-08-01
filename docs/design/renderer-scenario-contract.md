# Renderer Resize and Zoom Scenario Contract v1

Bead: `ft-interactive-systems-performance-4tenz.3.1`

Contract ID: `ft.renderer_scenario_catalog.v1`

## Purpose and authority boundary

This contract gives the native resize, zoom, visual, terminal-state, and
accessibility proof lanes one deterministic scenario vocabulary. It defines
inputs, intermediate invariants, checkpoints, and capability gaps. It does not
claim that a GUI run happened, that a target is supported, that a performance
budget passed, or that a software present marker reached scanout or photons.

The checked-in catalog's serialized `authority` field carries the closed value
`contract_only`. A later lane must bind a catalog revision and scenario ID to
an isolated target, app/bundle/domain identity, renderer configuration, font
corpus, run receipt, state transcript, image artifacts, and timing samples
before making any measured claim.

## Canonical artifacts

- Typed contract and semantic validator:
  `crates/frankenterm-core-audit-types/src/renderer_scenario_catalog.rs`
- Machine-readable catalog:
  `docs/design/renderer-scenario-catalog.v1.json`
- JSON Schema:
  `docs/json-schema/ft-renderer-scenario-catalog.json`
- Contract tests:
  `crates/frankenterm-core/tests/renderer_scenario_catalog.rs`

Existing fixture and oracle sources are referenced, not copied:

- `tests/renderer_golden/SCENARIOS.md`
- `tests/golden/gpu/`
- `tests/renderer_golden/fuzz/README.md`
- `docs/resize-baseline-scenarios.md`
- `fixtures/simulations/resize_baseline/`
- `docs/perf/resize-quality-slo.json`
- `docs/design/gpu-regression-harness.md`
- `docs/a11y/scenario-corpus.md`
- `docs/design/product-journey-catalog.v1.json`

## Closed scenario matrix

Version 1 contains exactly 32 cells: eight gestures crossed with four exact
fleet points. A missing, duplicate, or extra cell is invalid.

### Gestures

| Serialized ID | Required transition | Primary purpose |
|---|---|---|
| `same_grid_drag` | Pixel dimensions change while rows and columns remain fixed | Reprojection, quad retention, cursor/selection geometry |
| `grid_changing_drag` | A multi-event native drag crosses at least two grid boundaries | Scheduler, reflow, intermediate coherence, snap-back |
| `reflow_80_to_200` | Width changes from exactly 80 to exactly 200 columns | Expansion reflow and logical-line identity |
| `reflow_200_to_80` | Width changes from exactly 200 to exactly 80 columns | Contraction reflow and wrapping correctness |
| `zoom_in` | Font scale increases through deterministic steps | Glyph/atlas changes, IME/cursor/a11y geometry |
| `zoom_out` | Font scale decreases through deterministic steps | Density recovery, glyph retention, final convergence |
| `dpi_display_move` | One window moves between declared display/DPI identities | Scale transition, color/texture identity, geometry |
| `output_overlap_resize` | Resize events overlap exact aggregate PTY output of 1,000,000 bytes/s | Responsiveness and coherent state under output pressure |

### Fleet points

| Serialized ID | Exact panes | Exact tabs | Exact windows |
|---|---:|---:|---:|
| `p001` | 1 | 1 | 1 |
| `p020` | 20 | 4 | 1 |
| `p050` | 50 | 8 | 2 |
| `p200` | 200 | 16 | 4 |

Fleet counts are qualification points, not ranges. A 19-pane or 201-pane run
cannot be relabeled as the `p020` or `p200` cell.

These tab and window counts are deterministic v1 renderer-layout inputs, not
measured fleet minima. Renderer `p001` is intentionally not equivalent to the
product-journey catalog's two-pane `q002` floor; consumers must never join the
two identifiers by ordinal or pane-count inference. Likewise, matching numeric
suffixes at `p020`, `p050`, and `p200` do not qualify product `q020`, `q050`, or
`q200` without product topology, lifecycle, target, and authority evidence. The
`p` prefix intentionally means a renderer pane-layout cell and avoids colliding
with the product catalog's `q` qualification namespace.

Each scenario ID is `renderer.<gesture>.<fleet-point>`. The deterministic seed is
scenario-specific and non-zero. Its JSON wire form is exactly `0x` followed by
16 lowercase hexadecimal digits; bare JSON numbers and decimal, uppercase, or
short strings are invalid. The typed Rust surface remains `u64`. This prevents
IEEE-754-only JSON tooling from rounding or collapsing the 64-bit identity.
IDs, seeds, workload identities, and event ordinals are stable inputs; changing
one requires a catalog revision. The canonical v1 document and typed validator
currently require catalog revision `2` exactly.

## State contract

Every resolved `(base cell, overlay)` declares complete initial and final
surface/configuration/content state rather than relying on ambient GUI defaults.
The base cell owns shared gesture, workload, topology, and stage vocabulary;
the overlay owns the exact phase-state/configuration/materialization bindings:

- pixel width and height;
- terminal rows and columns;
- font size and scale;
- DPI and display identity;
- exact pane, tab, and window counts;
- focused window, active tab, and focused pane ordinals;
- distinct initial and final normalized typed topology, split geometry, and
  complete per-pane state-manifest bindings resolved within the same catalog;
- ordered per-window tab sequences with stable tab IDs and contiguous ordinals;
- scrollback line count and viewport top;
- grid and terminal revision IDs;
- active-buffer identity plus distinct primary/alternate buffer identities,
  revisions, scrollback/content bindings, selection anchors, cursor coordinates,
  IME preedit/caret, typed candidate-window coordinate space and rectangle,
  composition range/segments, and input-source identity, image anchors,
  hyperlink ranges, and accessibility focus/geometry state;
- display color-space/profile identity and HDR/EDR mode/availability;
- terminal-content corpus references;
- renderer configuration and pinned-font references.

Rows, columns, pixel sizes, DPI, base font size, and scale must be positive.
Fleet counts must equal the cell's exact fleet point, and ordinals and terminal
coordinates must be in bounds. To keep p200 reviewable without weakening the
contract, the catalog normalizes state through four root layout profiles,
surface-state templates, renderer-configuration profiles,
content-materialization/distribution profiles, coverage overlays, and phase
manifests.
All IDs resolve inside this same typed catalog; none is an opaque external
manifest reference.

The closed `balanced_contiguous_v1` layout derivation distributes windows,
tabs, and panes lowest-ordinal-first. It pins stable ID formatting and
zero-padding, per-window ordered tab sequences, pane membership, split
direction/ratio/alternation, and integer rounding. A phase manifest's
`window_rect` is the window-local drawable client/tab-content region consumed
by that derivation, with `x = 0` and `y = 0`. It excludes operating-system
window chrome but includes the pane surfaces and their explicitly modelled
internal content padding, so no unmodelled pixels can be silently subtracted
from split leaves. Content/state selectors are closed to
`all`, non-overlapping half-open ordinal ranges, or explicit small ordinal
lists. Phase manifests select these profiles and carry focused ordinals plus
state/content/output overrides. A closed derivation computes feature coverage
from materialized terminal input, typed surface state, and the selected
renderer configuration; a copied feature-name list is never authoritative.

The validator expands every profile to every exact window, tab, pane, split
leaf, surface state, renderer configuration, ordered content-materialization
step, content binding, and output rate. It checks count
agreement, uniqueness, full and non-overlapping selector coverage, referential
integrity, split-tree geometry, action-target agreement, exact focused-state
equality, stable per-window tab ordering, and exact mechanically derived
feature coverage for every required overlay.
The crate exposes a pure read-only resolver for a `(scenario_id, overlay_id)`
pair. The one-pair convenience function remains fail-closed and validates the
whole catalog before resolving. Suite drivers must instead construct
`RendererPreparedScenarioCatalog` once, then call `resolve_all_overlays`: that
path performs exactly one semantic validation pass and one immutable lookup
index build before expanding all 256 pairs in canonical gesture-major,
fleet-minor, overlay-minor order. `RendererResolverPreparationStats` makes that
preparation work explicit. Pair resolution and batch expansion use no mutable
or global cache, preserve the validation report's pair-scoped readiness and
blocking gaps, and produce the same results. The resolver and validator share
the same expansion, materialization-replay, and split-geometry implementation;
downstream drivers must consume that result instead of reimplementing the
normalized DSL. Its ordered result includes every checkpoint and exact
window/tab/pane identity, membership, rectangle, focus/active state, surface
state, materialized content, output state, revision, phase, and event ordinal.
Initial, final, and checkpoint bindings therefore deterministically cover the
whole fleet without repeating 200 large surface objects at every checkpoint.
An unresolved profile ID or one phase-ambiguous manifest cannot impersonate all
three. Final state must implement the gesture's declared transition.
Surface templates and normalized geometry profiles fully type IME composition
segments/candidate rectangles, image anchors and cell/pixel rectangles,
hyperlink cell ranges and hit rectangles, and accessibility node-to-cell/pixel
geometry. Counts and focused/caret identities are derived from those entries;
an opaque geometry reference or count-plus-revision placeholder is invalid.
Surface-local image, hyperlink, and accessibility rectangles must be wholly
contained by the resolved pane viewport. IME candidate-window rectangles use
signed virtual-display coordinates because they describe an operating-system
candidate window rather than pane-local pixels; width and height remain
positive and bounded.
Every font state pins base cell width and height in milli-pixels at the
configured base point size, font scale `1.000`, display scale `1.000`, and an
explicit reference logical DPI. The
v1 integer derivation applies font scale and logical/reference DPI with checked
arithmetic, then floors cell boundaries rather than proportionally dividing an
ambient viewport. Each pane viewport must equal its split leaf, and
for either axis freezes these operations in order:

```text
effective_cell_milli_px = floor(
  base_cell_milli_px * font_scale_milli * logical_dpi_milli
  * display_scale_factor_milli
  / (1000 * 1000 * metric_reference_dpi_milli)
)
boundary_px(i) = padding_before_px
  + floor(i * effective_cell_milli_px / 1000)
```

Platform adapters must report `dpi_milli` as logical DPI independently of the
window backing scale. On Apple silicon/macOS, `scale_factor_milli` represents
the backing-pixel scale (for example `2_000` on Retina); physical panel DPI is
neither substituted for logical DPI nor multiplied as a second scale. Thus the
canonical move from logical `96_000` DPI at 1x to logical `96_000` DPI at 2x
turns an 8 px base cell into exactly 16 px, never 32 px.

Every multiply/add uses checked integer arithmetic. Then
`left padding + derived grid width + right padding` and
`top padding + derived grid height + bottom padding` must exactly equal that
viewport. Right and bottom padding are explicit deterministic residuals. A v1
image, hyperlink, or accessibility entry carrying one pixel rectangle is
restricted to one terminal row; multi-row geometry requires a future typed
row-fragment representation rather than an overbroad bounding box.
Selection anchor and focus are each independently in bounds; backward
selections are valid and must not be rejected by treating the pair as an
ordered range.
A same-grid gesture must retain rows and columns; reflow gestures must use their
exact 80/200 endpoints; zoom gestures must retain configured base font size
while moving logical font scale in the declared direction and deriving
effective font size/cell metrics through the pinned derivation revision.
Display identity, DPI, and display scale remain unchanged during a zoom. A
display move must change display identity plus at least one independent display
metric: logical DPI or backing scale. The canonical Retina move changes scale
while retaining logical DPI. A scale change without display-identity change is
not a display move. `RendererDisplayTransition` contains only display
identity, DPI, scale, color-space/profile, and HDR/EDR metadata; it has no
viewport or padding fields. Its mutation bundle is exactly `SetWindowSize`,
`MoveToDisplay`, `SetRevisions`, in that order, with one identical window
target. `SetWindowSize` alone changes the drawable extent. `MoveToDisplay`
never reads or writes dimensions: pane viewports remain exact split leaves and
padding is recomputed from pinned cell metrics after the metadata transition.
It also declares its before/after color profile and HDR/EDR state. Draft,
Standard, and Fancy must all honor the selected color profile; a quality
transition cannot silently change gamut. `output_overlap_resize` also declares whether its
overlapped resize is `same_grid` or `grid_changing`; omission or use on another
gesture is invalid.

## Timeline and intermediate invariants

Event ordinals are contiguous from zero. Offsets are strictly increasing and
fit within the declared total duration. Each timeline contains explicit
`gesture_begin` and `gesture_end` events plus the gesture-specific state
transitions. `gesture_end` occurs exactly at `gesture_duration_us`; later
quality-transition events precede the final settle event/checkpoint, which
occurs exactly at `total_duration_us`. An unused tail is invalid. This
separation lets an exact five-second input gesture have a later settle
checkpoint. No event depends on wall-clock time or an unspecified random source.

Each event contains a non-empty atomic action bundle. One native resize callback
may update viewport dimensions, grid dimensions, and renderer revisions at the
same ordinal; the contract must not fabricate observable half-states merely to
serialize fields. `resize_mutation_count` counts such events, not individual
field actions. Within one atomic event, every `SetWindowSize`, `SetGrid`,
`SetFontScale`, `SetQualityMode`, `MoveToDisplay`, and `SetRevisions` action
uses the same complete `RendererMutationTarget`: window, optional tab, and
ordered affected-pane IDs. A valid target for another window or tab is still
invalid in that bundle because it would advance generations on surfaces other
than those whose state changed. Non-targeted output/key/boundary actions do not
participate in this equality rule. Output-overlap timelines explicitly start PTY output, inject
foreground-key actions, perform resize events while output remains active, and
then stop output; merely containing those actions in a non-overlapping order is
invalid. The v1 foreground-key event is one indivisible tuple:
`logical_key = "x"`, no modifiers, and encoded bytes `78`; validating any one
field independently cannot authorize a different key event. Live-resize
snap-back timelines also encode the closed production
quality set `draft`, `standard`, and `fancy`. Mutation frames run in `draft`.
There is exactly one `snap_back`-role checkpoint and exactly one encoded
Draft-to-Standard snap-back transition. The later steady-state settle quality
equals the pinned configured quality: it remains Standard when Standard is
configured, so two Standard checkpoints are valid, and its final Settle atomic
bundle performs the Standard-to-Fancy transition when Fancy is configured.
Zoom and DPI/display-move cells use a steady-quality-only policy unless live
production behavior says otherwise. A prose note or final comparator reference
cannot stand in for these state transitions.

Every size, grid, zoom, display, topology, split, focus, and active-tab action
names an explicit target window/tab/pane or closed target set plus the expected
affected-pane set. Phase manifests must agree with those targets. The driver
must not infer action scope from the focused window. Per-window tab sequences
remain ordered through every phase; duplicate, missing, or non-contiguous tab
ordinals are invalid. This is the same semantic shape that tab-order continuity
epic `ft-interactive-swarm-product-convergence-7xqz4.8.10` must persist across
reconnect/reopen, but this renderer catalog does not implement persistence.
Topology, split membership, focus, active-tab identity, tab ordering, and the
selected content-overlay identity are invariant across v1 gesture phases;
their canary actions remain separate from the primary gesture timeline.
Geometry, renderer quality, output rates, and revisions may change only through
their corresponding typed base-timeline actions. Overlay terminal mode/content
state is inherited between anchors and may change only through its ordered
materialization steps; the base timeline has no scenario-wide
`set_terminal_mode` action. An uncaused phase-manifest difference is invalid.

The catalog freezes these eight intermediate invariant classes. Each class has
canonical phases plus a gesture/feature condition. A resolved overlay omits a
conditional invariant when that condition is false; it must not fabricate a
non-empty applicability set. The same invariant ID is intentionally evaluated
at every applicable checkpoint; it is not a one-shot assertion that can make
later transient frames invisible:

`scenario.expected_invariants` is the canonical ordered union for all eight
required overlays. It retains both conditional definitions and their phase
contracts. Each checkpoint row, and the public resolved checkpoint anchor,
carries the exact ordered subset derived from that overlay anchor's materialized
features: alternate-screen isolation appears only with `alternate_screen`, and
accessibility focus geometry only with `accessibility_geometry`. Union
membership never makes either invariant applicable to another overlay.

- `no_blank_frame_after_nonblank` — no blank frame after a previously
  non-blank frame;
- `no_stale_full_frame_reuse` — no stale full-frame reuse across a semantic
  state change;
- `coherent_grid_terminal_revision` — one coherent grid and terminal revision
  per checkpoint;
- `anchors_in_bounds` — cursor, selection, IME, image, and hyperlink anchors
  remain in bounds;
- `reflow_logical_line_identity` — logical-line text and hard/soft wrap
  identity survive reflow;
- `alternate_screen_isolation` — the typed primary and alternate buffer
  identities, revisions, and content bindings remain distinct; activating or
  mutating the alternate buffer never changes the pinned primary scrollback
  identity;
- `accessibility_focus_geometry` — accessibility focus is exclusive and
  geometry matches the visible cell map; and
- `final_state_convergence` — the final frame converges to the declared final
  terminal state.

The output-overlap family additionally fixes aggregate PTY output at exactly
1,000,000 bytes/s and declares an exact foreground key event; its `p050`
related RQ-S6 cross-map requires exactly one event with pinned logical key,
modifiers, and encoded bytes. Concurrent resize makes this an adversarial
superset, not an exact RQ-S6 scenario. Output generator revision, seed, payload
identity, and pane distribution are part of workload identity. The closed
`even_lowest_ordinal_remainder_v1` policy assigns the integer quotient to every
expanded pane and one additional byte/s to the lowest pane ordinals until the
remainder is exhausted; optional explicit selector overrides must be
non-overlapping and preserve the exact aggregate. Other families
declare zero background output unless their workload explicitly says otherwise.

Those fields currently describe structure, not executable authority.
`renderer_output_stream_v1` has no closed implementation, implementation
manifest, or digest, so every output-overlap scenario-overlay pair carries the
blocking `DeterministicOutputStreamUnavailable` gap. The production-default
pair additionally carries `KeyEffectOracleUnavailable`: its foreground-key
schedule has no canonical foreground fixture, PTY echo binding, or pre/post
terminal-state oracle connecting the keypress to first-correct-present. Both
gaps track `ft-interactive-systems-performance-4tenz.3.1.2`. Consequently all
32 output-overlap pairs are `execution_ready: false`; these rows support only
structural and explicit-gap reporting and cannot qualify a performance or key
effect claim until that child closes with the named artifacts.

## Checkpoint policy

Every resolved scenario overlay has exactly three ordered checkpoints anchored
to the shared base timeline; live-resize families have exactly four:

1. `begin` before the first mutation;
2. exactly one `mutation` checkpoint during the gesture;
3. exactly one `snap_back` checkpoint after `gesture_end` for same-grid,
   grid-changing, reflow, and output-overlap resize cells; and
4. `settle` at the declared terminal event ordinal.

Zoom and DPI/display-move cells omit `snap_back`; adding one would invent a
production guarantee that their gesture family does not own.
The closed matrix therefore contains exactly 928 checkpoint-to-manifest anchor
rows: 20 live-resize cells times eight overlays times four checkpoints, plus 12
steady-quality cells times eight overlays times three checkpoints.
The root `phase_manifests` collection contains exactly 928 unique manifest IDs
in the same canonical scenario/checkpoint traversal order. A manifest is owned
by one checkpoint row; cross-scenario or cross-checkpoint reuse is forbidden so
that a later edit cannot silently couple two proof anchors through one shared
state object. Normalization remains at layout, surface-template, configuration,
and content-distribution profiles.

A checkpoint binds all of the following:

- event ordinal and phase;
- expected state-invariant IDs;
- expected frame-content class;
- phase-specific normalized typed pane/surface-state manifest binding that
  expands completely within the catalog;
- terminal-state oracle reference;
- visual oracle reference and exact role-specific comparator-policy references:
  zero for last-Draft provenance, one for initial/intermediate/final checkpoints,
  and two for the Standard snap-back subject;
- accessibility oracle reference;
- whether native capture is required.

Every checkpoint binds at least one applicable invariant. Continuous-frame
detectors belong to the overlay observation policy; interval,
checkpoint-oracle-pair, and whole-timeline detectors belong to the scenario's
typed nonlocal detector bindings. The `begin` checkpoint has an explicit
`nonblank` frame-content class,
which establishes the baseline for
`no_blank_frame_after_nonblank` and evaluates baseline-valid invariants such as
grid/terminal revision coherence and anchor bounds. Every `mutation`
checkpoint repeats all applicable cross-frame safety checks; the validator
must not require an invariant ID to appear exactly once. The `settle`
checkpoint evaluates all settle-applicable checks, including
`final_state_convergence`.

Invariant applicability is frozen rather than left to each row:

| State invariant | Phases | Gesture/feature condition |
|---|---|---|
| `no_blank_frame_after_nonblank` | mutation, snap_back, settle | snap_back only for live-resize; begin must be nonblank |
| `no_stale_full_frame_reuse` | mutation, snap_back, settle | snap_back only for live-resize |
| `coherent_grid_terminal_revision` | begin, mutation, snap_back, settle | snap_back only for live-resize |
| `anchors_in_bounds` | begin, mutation, snap_back, settle | snap_back only for live-resize |
| `reflow_logical_line_identity` | mutation, snap_back, settle | grid-changing/reflow gestures and grid-changing output-overlap |
| `alternate_screen_isolation` | begin, mutation, snap_back, settle | overlay derives active alternate-screen state; snap_back only for live-resize |
| `accessibility_focus_geometry` | begin, mutation, snap_back, settle | overlay derives accessibility geometry; snap_back only for live-resize |
| `final_state_convergence` | settle | every gesture |

A checkpoint's invariant IDs equal the complete overlay-aware applicable set;
subset binding, cross-overlay leakage, and omission from a qualifying overlay
are invalid. `RendererResolvedCheckpointAnchor.expected_invariant_ids` exposes
that exact set so downstream runners do not reimplement feature conditions.

Structural resolution returning `Ok` is not execution authority.
`RendererResolvedScenarioOverlay` carries pair-scoped `execution_ready`, exact
blocking gap codes, and relevant typed gaps; callers must inspect them before
attempting an execution lane.

State invariants are not the entire visual oracle. Version 1 freezes these 20
serialized detector IDs in this order:

| Detector ID | Scope | Phases | Condition/policy |
|---|---|---|---|
| `no_missing_glyphs` | all observed frames | begin through settle | every gesture and overlay |
| `coherent_cell_widths` | all observed frames | begin through settle | every gesture and overlay |
| `exact_row_width` | all observed frames | begin through settle | every gesture and overlay |
| `no_flicker` | interval | mutation, snap_back, settle | explicit interval ending at the phase checkpoint |
| `coherent_renderer_generation` | all observed frames | begin through settle | correlation identity and rendered generation must agree |
| `no_mixed_generation_tear_band` | all observed frames | begin through settle | every gesture and overlay |
| `no_stale_or_duplicate_frame` | all observed frames | begin through settle | the stream evaluator compares every adjacent correlation and semantic revision |
| `nonblank_after_baseline` | all observed frames | after the nonblank begin baseline through settle | every gesture and overlay |
| `ssim_policy` | `checkpoint_oracle_pair` | begin, mutation, snap_back, settle | subject checkpoint, independent oracle, and comparator-policy reference |
| `l_inf_policy` | `checkpoint_oracle_pair` | begin, mutation, snap_back, settle | subject checkpoint, independent oracle, and comparator-policy reference |
| `changed_pixel_fraction_policy` | `checkpoint_oracle_pair` | begin, mutation, snap_back, settle | reported, but non-independent until `.3.5.1` |
| `exact_terminal_state` | all observed frames | begin through settle | event-replayed expected state for the frame's correlation |
| `cursor_geometry` | all observed frames | begin through settle | every overlay derives cursor state |
| `selection_geometry` | all observed frames | begin through settle | overlay derives active selection state |
| `ime_geometry` | all observed frames | begin through settle | overlay derives IME state |
| `hyperlink_geometry` | all observed frames | begin through settle | overlay derives hyperlink state and geometry |
| `image_geometry` | all observed frames | begin through settle | overlay derives image state and geometry |
| `alternate_screen_state` | all observed frames | begin through settle | overlay derives active alternate-screen state |
| `accessibility_geometry` | all observed frames | begin through settle | overlay derives accessibility geometry |
| `exactly_one_standard_snap_back` | whole timeline | snap_back | exactly one snap-back-role checkpoint and one Draft-to-Standard transition; later Standard settle is allowed |

All-observed-frame detector IDs live only on the overlay's continuous
observation policy. Checkpoint-oracle-pair, interval, and whole-timeline
bindings are separate typed records with exact subject/oracle/policy or interval
endpoints; neither a checkpoint row nor a second all-frame binding list can
create competing detector authority.
Every applicable detector has exactly one correctly scoped owner. The
changed-pixel detector remains explicitly non-independent until `.3.5.1`
repairs its comparator semantics.

The 15 all-observed-frame IDs form a closed suite inventory, not a requirement
to run feature-inapplicable geometry checks in every overlay. Each observation
policy carries nine universal IDs (`no_missing_glyphs`,
`coherent_cell_widths`, `exact_row_width`, `coherent_renderer_generation`,
`no_mixed_generation_tear_band`, `no_stale_or_duplicate_frame`,
`nonblank_after_baseline`, `exact_terminal_state`, and `cursor_geometry`) plus
the geometry/state IDs mechanically derived for that overlay. The exact counts
in overlay order are therefore 9, 9, 10, 10, 11, 9, 10, and 10; the
`image_hyperlink` overlay contributes both image and hyperlink detectors.

Checkpoints are deterministic oracle anchors, not a license to ignore the
transient stream between them. Every observed, captured, or presented frame
from `gesture_begin` through `settle` carries its event interval, phase,
renderer-generation identity, and all applicable per-frame and interval
detector verdicts. A scenario overlay with one mutation checkpoint therefore still
must detect a blank, mixed-width, torn-generation, stale, or duplicated
intermediate frame. Checkpoint/oracle comparisons remain anchored at the
declared checkpoints, while whole-stream safety detectors cover every frame.

For RQ-S11, the timeline carries the exact fields
`last_draft_checkpoint_id`, `standard_snap_back_subject_checkpoint_id`, and
`independent_standard_oracle_ref`. The last Draft mutation checkpoint is
transition provenance and transient-quality evidence only. The SSIM `>= 0.999`
subject is the post-snap Standard `snap_back` checkpoint, compared with an
independently rendered Standard oracle at identical final dimensions and
terminal state. The independent oracle is not a checkpoint role. The timeline
must contain the typed `draft` to `standard` renderer-quality-mode transition;
comparing last-Draft pixels directly with Standard pixels is invalid. Merely
attaching the RQ-S11 policy to a final checkpoint is not a machine-checkable
snap-back predicate.

Every v1 gesture checkpoint sets `native_capture_required` to true. Headless
fixtures may support oracle development but cannot turn that field off. A
required-but-unsupported native/capture capability keeps the catalog definition
valid while making execution readiness false and forcing a later result to
`unsupported` or `skipped_not_proven`, never pass.

Comparator-policy references bind the mechanism, policy revision, and effective
threshold source; the catalog does not copy numeric thresholds. RQ-S11 uses its
distinct SSIM `>= 0.999` snap-back policy from
`docs/perf/resize-quality-slo.json`, while RQ-S13 references the general GPU
parity policy. The current `compare_images` implementation counts a pixel only
after its delta exceeds the same L-infinity ceiling used by the global gate, so
its changed-pixel fraction is not yet an independent protection. That defect is
owned by `ft-interactive-systems-performance-4tenz.3.5.1`; no v1 scenario may
interpret the current three reported metrics as three independent proofs.
Unsupported native capture or comparator semantics remain explicit rather than
becoming a green headless result.

## Terminal-content accounting and evidence sources

The closed terminal-feature inventory is:

`ascii`, `cjk`, `rtl`, `combining_marks`, `emoji`, `ligatures`, `images`,
`hyperlinks`, `alternate_screen`, `selection`, `cursor`, `ime`, and
`accessibility_geometry`.

Coverage is not a global singleton and is not an impossible requirement that
all 13 features remain simultaneously active in one pane. Every one of the 32
gesture-by-fleet base cells carries a typed, ordered overlay suite. The closed
v1 overlay IDs are `production_default`, `unicode_maximal`,
`alternate_screen`, `ime_composing`, `image_hyperlink`,
`ligature_enabled`, `selection`, and `a11y_geometry`. A coverage lint requires
every gesture-by-fleet cell to define all eight overlays and, across that suite,
derive all 13 features. There is no caller-authored applicability escape hatch:
unsupported input or capability remains a typed per-overlay readiness gap, not
an omitted definition, empty selector, or prose exception.

| Overlay ID | Features introduced beyond inherited base state | Configuration/measurement rule |
|---|---|---|
| `production_default` | `ascii`, `cursor` | Bundled configuration; only implicit SLO candidate |
| `unicode_maximal` | `cjk`, `rtl`, `combining_marks`, `emoji` | Inherits production-default shaping/configuration |
| `alternate_screen` | `alternate_screen` | Typed primary/alternate buffers; cannot qualify primary-screen-only RQ-S9 |
| `ime_composing` | `ime` | Cannot qualify K0-through-K13 when IME may consume or transform the key |
| `image_hyperlink` | `images`, `hyperlinks` | Requires framed image input and typed image/hit geometry |
| `ligature_enabled` | `ligatures` | Separate pinned `calt`/`clig`/`liga`-enabled shaping configuration |
| `selection` | `selection` | Exact anchor/focus/granularity state |
| `a11y_geometry` | `accessibility_geometry` | Machine geometry only; no VoiceOver authority |

Every non-base overlay inherits `ascii` and `cursor`. The exact union of the
base and introduced feature sets is therefore the 13-item inventory above.

Measurement authority is closed rather than caller-selected. Exact RQ/SLO
candidate cross-maps bind `production_default` only. The other seven overlays
are `visual_coverage_related_only`: they may retain separately labelled
performance observations, but cannot inherit an exact base SLO. In addition,
`ime_composing` is hard-ineligible for
`keypress_to_first_correct_present`/RQ-S6, and `alternate_screen` is
hard-ineligible for primary-screen RQ-S9. These dispositions are enum values in
the catalog, not free-form reasons supplied by a runner.

This structure preserves exact measurement predicates. The base
`production_default` overlay uses the bundled renderer configuration and is the
only implicit SLO candidate. `ime_composing` cannot qualify a foreground-key
K0-through-K13 measurement when the IME may consume or transform that key.
`alternate_screen` cannot qualify a primary-screen 1,000-line reflow predicate.
`ligature_enabled` selects an explicit shaping configuration that enables and
pins the required OpenType features; the bundled configuration currently
disables `calt`, `clig`, and `liga`, so the ligature overlay is never described
as production-default behavior. A measurement may select another overlay only
when its binding names it and its exact predicate remains compatible.

The overlay suite does not multiply the stable 32 base scenario IDs. The eight
overlay definitions are root-global reusable templates; the validator applies
their closed cross-product to every base cell instead of serializing 256
near-identical profile definitions. Each overlay has its own stable template ID,
closed measurement-eligibility classification, capability requirements, and
readiness result. Each scenario binds the eight templates exactly once, while
its overlay-specific checkpoint rows are the sole owners of ordered
phase-manifest anchor identity. The validator checks
coverage at every gesture x fleet point while run logs always name both the
base scenario and overlay. A valid contract may therefore retain a missing
image, ligature, or live-IME overlay as an explicit readiness gap without
pretending the base scenario executed it.

Resolved cell-overlay checkpoint-to-phase-manifest references are exact
initial/final/intermediate anchors,
not one repeated manifest per timeline event. Between anchors, the validator
replays the closed atomic actions from the preceding state and verifies that the
next anchor equals the derived result. Stable overlay content/configuration is
inherited until a typed materialization action changes it. This avoids a
per-event manifest explosion in 100-mutation cells while still making every
transient event and observed frame accountable.
The scenario's overlay-template binding must not repeat a second list of anchor
manifest IDs; competing anchor lists would create ambiguous state authority.

Content inputs and evidence authority are separate collections. A content
corpus has a stable ID, relative repository reference, mechanically derivable
feature set, payload or generator identity and revision, deterministic seed or
exact payload digest, and is referenced by workloads. Its input binding uses a
closed encoding/decoder/framing kind rather than only `path + sha256`.
`hex_transcript_v1`, for example, names the exact
`tests/fixtures/terminal-conformance/manifest.json` row and transcript segment,
hex-decodes it to terminal bytes, and records whether bytes are concatenated,
framed as a terminal protocol sequence, or applied as a typed state overlay.
Raw terminal bytes, UTF-8 fixture input, GPU fixture state, and generated input
use separate closed kinds; a body-only Kitty graphics fixture cannot be
mistaken for a complete terminal stream. Protocol-body framing uses
protocol-specific enum variants with contract-fixed envelope bytes; arbitrary
caller-supplied prefix/suffix hex is invalid. The canonical OSC 8 input is the
complete hex-decoded terminal-conformance transcript, not a body-only framing
case.

Version 1 freezes the expected decoded semantic contribution for every
canonical content ID together with its source reference, digest domain,
selector, encoding, and framing. The serialized `semantic_kinds` field is a
redundant assertion and must equal that canonical mapping exactly. It cannot
relabel an ASCII payload as CJK, ligatures, images, or any other feature;
state-only inputs carry an empty semantic list and derive their feature from
typed surface/configuration state instead.

Ordered materialization steps pin corpus selector, decoder, framing, target
pane set, event boundary, and composition operation. Alternate-screen coverage
must enter the alternate buffer, materialize the intended visible content, and
hold that state through the compared checkpoint before any exit step. Selection,
cursor, IME, image/hyperlink geometry, accessibility geometry, and active-buffer
state are derived from typed surface/configuration state as well as content;
they are not falsely attributed to byte payloads. Ligature coverage requires
both ligature-bearing text and the explicitly enabled shaping profile. The
derived feature union is the result of those predicates, never a user-supplied
assertion.
Materialization replay is phase-aware: it applies `before_gesture`, `at_event`,
and `after_checkpoint` boundaries in deterministic timeline order, implements
replace/append/enter-alternate/exit-alternate/typed-state operations against
the correct buffer, and enforces every `hold_through_checkpoint_ids` lifetime
continuously from application through the furthest named checkpoint. Removing
an effect at an intermediate checkpoint and reintroducing it before the named
endpoint does not satisfy the hold.
Because v1 carries no executable transform payload for changing typed fixture
state later in a run, `apply_typed_state_overlay` is restricted to
`before_gesture`. Typed-state inputs with non-empty visible-content semantics
append their identity to the active buffer; empty-semantic IME/accessibility
mode-and-geometry overlays leave buffer identities unchanged. Later
`at_event`/`after_checkpoint` boundaries are therefore reserved for the four
explicit byte/buffer operations, which the validator replays into the exact
active, primary, and alternate buffer state.
The resulting primary/alternate buffer contents and active-buffer identity must
exactly equal each resolved surface state. Future or expired steps cannot
contribute feature coverage to an earlier checkpoint. An unavailable planned
input may retain its desired feature obligation for gap reporting, but it can
never make an overlay execution-ready or count as materialized proof.

A future driver must be able to reproduce the declared bytes and terminal state
without an ambient corpus default. A corpus/materialization gap has an explicit
availability state, limitation, and tracking reference and makes only the
affected overlay `execution_ready: false`; it does not invalidate the scenario
definition or authorize placeholder bytes. An evidence source has its
own stable ID/reference, authority scope, coverage status, limitation, and
tracking references, and is referenced by gesture-authority rows. Mock resize
replays, SLO documents, product journeys, and metadata-less stress sources are
evidence sources, never terminal workload inputs. A `gap` evidence row cannot
satisfy feature accounting or make a capability available.

For evidence sources, `direct` requires limitation/tracking fields to be absent.
`partial`, `gap`, and `present_unqualified` require a non-empty limitation and
tracking reference. Gesture authority freezes exact gesture-to-source IDs, not
only matching status labels. The catalog points to existing GPU, fuzz, resize,
accessibility, or product-journey sources. It must not embed copied fixture
payloads or invent a second golden-image format.

### Existing corpus truth

Coverage below describes checked-in contract substrate only. `Direct` means a
complete existing headless fixture package, never a native-window or target-run
pass.

Deterministic input availability is tracked separately from visual authority.
The terminal-conformance manifest and its `.hex` transcripts provide
parser-only, hex-decoded terminal-byte input for `tc-utf8-grapheme-001`
(ASCII/CJK/combining/emoji), `tc-osc8-hyperlink-001`,
`tc-cursor-mode-001`, and `tc-alt-screen-001`. The last transcript contains
both enter and exit sequences, so an alternate-screen overlay must select an
ordered enter/write/checkpoint segment rather than replay the whole artifact
and claim that its final state is still alternate. These sources authorize
deterministic bytes, not cell geometry or rendered appearance.

`fuzz/corpus/term_advance_bytes/seed_dcs_sixel_fragment.bin` is a complete raw
DCS sixel terminal stream with SHA-256
`ba2f48b0bb4e567cd66fcd75188ec870fd0b5a23474366e5a5d3deac4ea9d162`.
It can supply deterministic image-protocol input but has no manifest, expected
geometry, or visual/native authority. A Kitty `input.bin` that contains only
an APC body is not a terminal stream until a typed framing step supplies the
protocol envelope. These distinctions are why input readiness and the GPU
evidence map below are separate fields.

| Feature or source | v1 classification | Reason |
|---|---|---|
| ASCII, CJK, RTL, combining marks, emoji, cursor | `direct` | Complete `input.json`, `meta.json`, `expected.json`, and `golden.png` fixture packages exist |
| Selection | `partial` | Static char/line/word fixtures exist; a continuous drag fixture does not |
| IME | `partial` | `overlay-ime-composition` is a static visual with `ime_disabled: true`, not live composition |
| Ligatures, images, hyperlinks, alternate screen | `gap` | No canonical complete GPU fixture exists |
| Accessibility geometry | `gap` | The five accessibility scenarios cover event semantics, not cell geometry |
| `multipane-resize-static-snapshot` | `partial` | One static frame is not a native continuous resize gesture |
| `tests/golden/gpu/stress/*` | `present_unqualified` | Seven fixture directories exist without required `meta.json` identity |

The machine feature-evidence map is complete and ordered by
`RendererTerminalFeature::ALL`:

| Feature | Exact evidence source IDs | Status | Limitation and tracking |
|---|---|---|---|
| `ascii` | `gpu_fixture.text_basic_paragraph` -> `tests/golden/gpu/text-basic-paragraph/meta.json` | `direct` | none |
| `cjk` | `gpu_fixture.text_cjk_mixed` -> `tests/golden/gpu/text-cjk-mixed/meta.json` | `direct` | none |
| `rtl` | `gpu_fixture.text_rtl_arabic_hebrew` -> `tests/golden/gpu/text-rtl-arabic-hebrew/meta.json` | `direct` | none |
| `combining_marks` | `gpu_fixture.text_combining_marks` -> `tests/golden/gpu/text-combining-marks/meta.json` | `direct` | none |
| `emoji` | `gpu_fixture.text_emoji_fallback` -> `tests/golden/gpu/text-emoji-fallback/meta.json` | `direct` | none |
| `ligatures` | `inventory.gpu_ligatures_gap` -> `tests/golden/gpu/README.md` | `gap` | No canonical complete GPU fixture exercises enabled ligature shaping and exact cluster/cell geometry; `ft-interactive-systems-performance-4tenz.3.6.2`, `ft-interactive-systems-performance-4tenz.3.5`, `ft-interactive-swarm-product-convergence-7xqz4.9.2` |
| `images` | `inventory.gpu_images_gap` -> `tests/golden/gpu/README.md` | `gap` | No canonical complete GPU fixture exercises inline-image protocol state and image geometry; `ft-interactive-systems-performance-4tenz.3.6.2`, `ft-interactive-systems-performance-4tenz.3.5`, `ft-interactive-swarm-product-convergence-7xqz4.9.2` |
| `hyperlinks` | `inventory.gpu_hyperlinks_gap` -> `tests/golden/gpu/README.md` | `gap` | No canonical complete GPU fixture exercises hyperlink ranges and hit geometry; `ft-interactive-systems-performance-4tenz.3.6.2`, `ft-interactive-systems-performance-4tenz.3.5`, `ft-interactive-swarm-product-convergence-7xqz4.9.2` |
| `alternate_screen` | `inventory.renderer_alt_screen_gap` -> `tests/renderer_golden/SCENARIOS.md` | `gap` | `SCENARIOS.md` marks `alt-screen` gap; no complete package exercises primary/alternate isolation and transition; `ft-ruona`, `ft-interactive-swarm-product-convergence-7xqz4.9.2` |
| `selection` | `gpu_fixture.selection_char` -> `tests/golden/gpu/selection-char/meta.json`; `gpu_fixture.selection_word` -> `tests/golden/gpu/selection-word/meta.json`; `gpu_fixture.selection_line` -> `tests/golden/gpu/selection-line/meta.json` | `partial` | Static fixtures exist, but no continuous selection-drag timeline exists; `ft-ruona`, `ft-interactive-swarm-product-convergence-7xqz4.9.2` |
| `cursor` | `gpu_fixture.cursor_beam_blink` -> `tests/golden/gpu/cursor-beam-blink/meta.json`; `gpu_fixture.cursor_beam_steady` -> `tests/golden/gpu/cursor-beam-steady/meta.json`; `gpu_fixture.cursor_block_blink` -> `tests/golden/gpu/cursor-block-blink/meta.json`; `gpu_fixture.cursor_block_steady` -> `tests/golden/gpu/cursor-block-steady/meta.json`; `gpu_fixture.cursor_underline_blink` -> `tests/golden/gpu/cursor-underline-blink/meta.json`; `gpu_fixture.cursor_underline_steady` -> `tests/golden/gpu/cursor-underline-steady/meta.json` | `direct` | none |
| `ime` | `gpu_fixture.overlay_ime_composition` -> `tests/golden/gpu/overlay-ime-composition/meta.json` | `partial` | `input.json` sets `ime_disabled: true`; the static visual does not exercise live composition or candidate-window transitions; `ft-interactive-systems-performance-4tenz.3.6.2`, `ft-interactive-systems-performance-4tenz.3.5`, `ft-interactive-swarm-product-convergence-7xqz4.9.2`, `ft-interactive-swarm-product-convergence-7xqz4.9.5` |
| `accessibility_geometry` | `a11y.scenario_corpus_geometry_gap` -> `docs/a11y/scenario-corpus.md` | `gap` | The five scenarios define event ordering/coalescing, not rendered cell/tree geometry or a native comparator; `ft-interactive-systems-performance-4tenz.3.5`, `ft-interactive-swarm-product-convergence-7xqz4.9.3` |

Each `gpu_fixture.*` reference points to canonical package identity in
`meta.json` and is `direct` only when sibling `input.json`, `expected.json`, and
`golden.png` all exist. The inventory and accessibility contract sources are
gap-basis evidence only: they authorize neither replay nor checkpoint
comparison. No feature-evidence row satisfies the separate overlay
materialization/terminal-state coverage requirement.

`partial`, `gap`, and `present_unqualified` rows require a reason and tracking
Bead. They cannot satisfy a native checkpoint or be promoted to `direct` by a
consumer. The missing stress metadata is owned by `.3.6.1`; missing non-a11y
visual fixtures for ligatures, images, hyperlinks, and live IME are owned by
`.3.6.2`, while `ft-ruona` retains its explicit selection-drag and
alternate-screen scope. Product native qualification remains in `.9.2` and
`.9.5` rather than being inferred from either headless corpus bead.
The renderer-level accessibility-geometry comparator gap is tracked by
`ft-interactive-systems-performance-4tenz.3.5`; live NSAccessibility and
VoiceOver authority remains separately owned by
`ft-interactive-swarm-product-convergence-7xqz4.9.3`. The machine geometry lane
cannot mint VoiceOver or human-review proof.

Feature completeness and evidence authority are different fields. The v1
authority map is closed as follows:

| Canonical source | Authority scope | Headless gesture replay | Headless checkpoint comparison | Native/target authority |
|---|---|---:|---:|---:|
| Complete `tests/golden/gpu/<fixture>/` package | `headless_visual_fixture` | no | yes | no |
| MockWezterm resize baseline under `fixtures/simulations/resize_baseline/` | `headless_state_replay` | yes | state only | no |
| Duplicate-render/fuzz signal lane | `metamorphic_signal_only` | no | no | no |
| `docs/design/product-journey-catalog.v1.json` | `contract_only` | no | no | no |

A row may not derive gesture or checkpoint authority from its `direct` feature
classification. Native-window gestures, Metal captures, display moves, IME
composition, and accessibility geometry remain explicit capability gaps until a
later isolated runner supplies their own evidence.

The catalog also carries an exact gesture-authority map. These classifications
describe the nearest checked-in source, not a native pass:

| Gesture | v1 source status | Nearest source and limitation | Tracking reference |
|---|---|---|---|
| `same_grid_drag` | `partial` | `tests/golden/gpu/multipane-resize-static-snapshot/input.json`; no native drag timeline | `.3.6` headless adaptation plus `.3.3` native driving and `.3.4` capture |
| `grid_changing_drag` | `partial` plus `present_unqualified` | `fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml` plus metadata-less `tests/golden/gpu/stress/rapid-resize-10s/input.json` | YAML: `.3.6`; stress metadata: `.3.6.1`; native: `.3.3`/`.3.4` |
| `reflow_80_to_200` | `gap` | `docs/perf/resize-quality-slo.json#RQ-S9.reflow_latency` defines the policy, but no exact native visual execution corpus exists | `.3.6` headless adaptation plus `.3.3` native driving and `.3.4` capture |
| `reflow_200_to_80` | `partial` | `fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml` changes font between steps | `.3.6` headless adaptation plus `.3.3` native driving and `.3.4` capture |
| `zoom_in` | `partial` | `fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml`; no native visual checkpoint sequence | `.3.6` headless adaptation plus `.3.3` native driving and `.3.4` capture |
| `zoom_out` | `partial` | `fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml`; no native visual checkpoint sequence | `.3.6` headless adaptation plus `.3.3` native driving and `.3.4` capture |
| `dpi_display_move` | `present_unqualified` | static `tests/golden/gpu/stress/dpi-1_00/input.json` and `tests/golden/gpu/stress/dpi-2_00/input.json` endpoints lack a display-move timeline and metadata | `.3.6.1` metadata plus `.3.3` native driving and `.3.4` capture |
| `output_overlap_resize` | `gap` | `fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml` does not implement the exact p050 one-key scenario | `.3.6` headless adaptation plus `.3.3` native driving and `.3.4` capture |

For JSON references, a fragment such as `#RQ-S9.reflow_latency` resolves only
when the target document contains exactly one object whose `id` equals that
fragment. It is not accepted as an unchecked file suffix.

### Legacy mapping and rejection table

Legacy IDs are cross-references only. They cannot qualify a v1 cell or import a
legacy verdict.

| Legacy ID | Related v1 gesture(s) | Disposition and reason |
|---|---|---|
| `resize-step` | `same_grid_drag`, `grid_changing_drag` | `related_only`; static/additive coverage is not a native timeline |
| `resize-burst` | `grid_changing_drag`, `output_overlap_resize` | `related_only`; lacks the exact output/key/fleet predicate |
| `font-change` | `zoom_in`, `zoom_out` | `related_only`; no native checkpoint sequence |
| `dpi-change` | `dpi_display_move` | `related_only`; static scale change is not a display move |
| `resize_single_pane_scrollback.yaml` | both reflow directions | `related_only`; MockWezterm state replay only |
| `resize_multi_tab_storm.yaml` | `grid_changing_drag` | `related_only`; eight panes do not match a fleet cell |
| `font_churn_multi_pane.yaml` | `zoom_in`, `zoom_out` | `related_only`; mock font markers only |
| `mixed_scale_soak.yaml` | zoom and DPI/display families | `related_only`; mock mixed-scale soak, not native display movement |
| `mixed_workload_interactive_streaming.yaml` | `output_overlap_resize` | `related_only`; output distribution is not the exact RQ-S6 predicate |
| `steady_typing`, `pane_focus_change`, `dialog_open`, `selection_change`, `scroll_position_change` | none | `rejected`; event-order/coalescing corpus is not renderer geometry or a visual gesture |

The machine catalog records each source reference, target gesture list,
disposition, and reason so a stale short ID cannot be silently reinterpreted.

### Rejected source inferences

Several adjacent documents describe useful historical substrate but are not
current qualification authority:

- `tests/renderer_golden/SCENARIOS.md` maps generic editor/scroll cases to
  RQ-S6 and font/DPI changes to RQ-S10; those mappings omit the exact S6 fleet,
  output, and single-key predicate and contradict S10's no-font/no-scale rule.
- Its statement that stress fixtures are shipped is only filesystem presence;
  all seven stress directories lack mandatory `meta.json` identity.
- Its IME wording cannot establish live composition because
  `overlay-ime-composition/input.json` has `ime_disabled: true`.
- `RQ-S11.snap_back_ssim` still names the retired
  `tests/renderer_golden/scenarios` path rather than the live GPU corpus and is
  `bench_pending`. Its target text says Draft pixels versus Standard pixels,
  while its own scenario and the live Draft-mode/snap-back design require the
  final post-snap Standard frame versus an independent Standard oracle. V1
  retains the last Draft frame only as transition provenance and records this
  source contradiction under
  `ft-interactive-systems-performance-4tenz.3.5.2` rather than importing the
  stale comparison wording.
- `docs/design/gpu-regression-harness.md` retains an old crate-local fixture
  layout; `tests/golden/gpu/README.md` describes the live layout.
- `tests/renderer_golden/fuzz/README.md` calls its oracle analytic, while the
  live harness compares a second render of the same state with the first. That
  is duplicate-render determinism evidence, not an independent visual oracle.

The catalog therefore records these sources as related, partial,
present-unqualified, or rejected. It never imports their prose as a pass.

### SLO cross-map

Typed requirement bindings use the full identifiers below. A binding is
admitted only when its exact predicate holds; sharing a gesture name is not
enough and no binding is itself an SLO verdict.

| Full requirement ID | Exact catalog predicate | Binding scope |
|---|---|---|
| `RQ-S1.resize_fps` | related only: the live requirement is a synthetic 300-frame 80↔200 dirty-row bench and no v1 native matrix cell implements that exact substrate | related-only reference |
| `RQ-S6.heavy_burst_input_latency` | `output_overlap_resize.p050` preserves 50 panes, 1,000,000 PTY output bytes/s, and one key, but adds concurrent resize absent from the canonical SLO | related adversarial superset |
| `RQ-S9.reflow_latency` | only `reflow_80_to_200.p001` with exactly 1,000 scrollback lines is an exact scenario candidate; larger fleets are related stress variants because the SLO does not define per-pane/aggregate fleet semantics | p001 exact candidate; larger related-only |
| `RQ-S10.atlas_rebuild_count` | pure resize with exactly 100 resize mutations, unchanged base font/scale, zero new glyphs, zero output stream, zero foreground keys, no content-changing terminal action, and a non-output-overlap gesture; grid changes remain allowed | exact scenario predicate |
| `RQ-S11.snap_back_ssim` | last-Draft provenance, exactly one snap-back-role checkpoint, independent Standard oracle at identical final dimensions/state, encoded Draft-to-Standard transition, and SSIM `>= 0.999` between the Standard snap-back subject and Standard oracle | checkpoint/oracle predicate |
| `RQ-S13.ssim_parity_oracle_corpus` | invocation of the pinned comparator mechanism and effective policy only | comparator-mechanism reference |

RQ-S4 is a related 24-hour fuzz lane and cannot be satisfied by these finite
scenario definitions. RQ-S1 cannot be auto-added merely because a cell is
`p200`; it becomes qualifying only after a future typed synthetic substrate
matches every live predicate. The output-overlap cell cannot mint RQ-S6, and
multi-pane forward reflow cannot inherit RQ-S9's latency target. Reverse reflow
cannot borrow RQ-S9, and zoom/display transitions cannot borrow resize-only
RQ-S10 or live-resize snap-back RQ-S11.

## Workload identity

Every base cell declares shared workload identity, and every overlay supplies
the exact phase-specific content/configuration/state bindings:

- exact pane, tab, and window counts plus initial/final/checkpoint
  layout/topology manifests;
- scrollback lines per pane;
- terminal content-corpus IDs, equal in canonical order to the exact union used
  by the overlay distributions selected for that workload;
- aggregate output bytes per second plus output generator ID, revision, seed,
  payload identity, and a closed pane-distribution policy whose deterministic
  expansion covers every pane and exactly sums to the aggregate;
- exact foreground key events with logical key, modifiers, and encoded bytes;
- exact resize-mutation and new-glyph counts;
- separate gesture-input duration, total/settle duration, and event count;
- deterministic seed;
- renderer, base-font, font-metric derivation, and scenario-corpus references.

The contract validator cross-checks workload counts against the fleet point and
the output-overlap rate against the exact one-megabyte-per-second requirement.
These fields describe the future run input; they are not samples or results.
In particular, S6's single key is an exact event identity, not a non-zero rate
or an anonymous count.

## Production-stage and metric contract

Every base scenario binds measurement contract
`ft.renderer.measurement-contract.v1`; each measurement binding also names the
overlay it admits. Its closed, ordered renderer/resize stage IDs are:

1. `R0.native_event_receipt`
2. `R1.gui_return`
3. `R2.intent_enqueue`
4. `R3.mux_resize_dispatch`
5. `R4.pane_resize_apply`
6. `R5.intent_supersession`
7. `R6.worker_create`
8. `R7.worker_start`
9. `R8.terminal_lock_wait`
10. `R9.terminal_lock_hold`
11. `R10.viewport_reflow`
12. `R11.near_reflow`
13. `R12.cold_reflow`
14. `R13.first_coherent_viewport`
15. `R14.worker_join`
16. `R15.gui_invalidation`
17. `R16.paint`
18. `R17.text_shaping`
19. `R18.glyph_raster`
20. `R19.glyph_atlas`
21. `R20.line_quad_reuse_rebuild`
22. `R21.gpu_bind`
23. `R22.gpu_upload`
24. `R23.gpu_submit`
25. `R24.drawable_present_request`
26. `R25.display_completion`

Output-overlap scenarios additionally bind this distinct closed K0-through-K13
keypress sequence; `R0.native_event_receipt` cannot impersonate key receipt:

1. `K0.key_appkit_receipt`
2. `K1.gui_key_mapping_complete`
3. `K2.client_rpc_enqueue`
4. `K3.client_encode_socket_flush`
5. `K4.server_readable_decode`
6. `K5.server_dispatch_mux_wait`
7. `K6.terminal_lock_pty_write_flush`
8. `K7.pty_echo_parser_apply`
9. `K8.server_delta_compute`
10. `K9.client_receive_decode_apply`
11. `K10.local_mux_gui_invalidation`
12. `K11.paint_shape_atlas`
13. `K12.gpu_submit_drawable_request`
14. `K13.display_completion`

Their receipt payloads retain the queue depth/oldest age, socket boundary,
terminal-lock timing, PTY write/flush and causally downstream read/application,
delta rows/bytes/clone/compress work, prediction result, paints/frames, and
drawable/display boundary details required by the campaign's corresponding
K-stage. Remote-domain receipts cannot be inferred from a local resize event.

Each stage receipt records scenario/event/correlation IDs, producer identity,
clock-domain ID, thread ID, path class, begin/end or marker timestamps, and its
observed boundary class. The closed path classes are `production_native`,
`headless_reference`, and `unsupported`; only a complete correlated
`production_native` receipt chain can qualify the parent's production-path
gate. A generic `production_renderer_backend` capability cannot substitute for
receipts proving that pane resize, terminal reflow, shaping, glyph-atlas,
WebGPU, drawable, software-present, and display-presented stages actually ran.

For every duration-bearing stage and end-to-end measurement binding, retained
artifacts include raw correlated samples, sample count, and p50/p95/p99. They
also retain actual consecutive `R25.display_completion` intervals. The
`R1.gui_return`, `R8.terminal_lock_wait`, and `R9.terminal_lock_hold` IDs remain
separate intervals. Every stage records `allocation_count`, `allocated_bytes`,
`copy_count`, and `copied_bytes`; every run records thread-creation count plus
the initiating stage receipt when attributable. Missing receipts, mixed clock
domains without a pinned calibration, or summary-only data are non-qualifying.
This catalog freezes required bindings and identities, not measured values.

## Downstream run profiles and driver canaries

The 32-cell matrix does not multiply across display cadence or cache age, but
each cell carries typed references to reusable run profiles:

- fixed 60 Hz and fixed 120 Hz presentation targets, plus explicit VRR mode and
  availability metadata;
- cold, warm, and aged preconditioning profiles that separately identify
  session age, scrollback age, glyph-cache state, atlas state, and prewarm
  manifest; `cold` is the exact operational realization of parent `.3`'s
  legacy prose label `fresh`, but `fresh` is not a serialized fourth profile
  and cannot be substituted without those cold preconditions;
- measurement bindings for `first_correct_viewport`,
  `steady_presented_fps`, `cold_reflow_convergence`, `snap_back`, and the
  output-overlap-only `keypress_to_first_correct_present`, including applicable
  source and target event/checkpoint IDs.

These are requested run identities, not claims that a target supports 120 Hz,
VRR, or a particular cache state. Unsupported target availability remains a
typed skipped/unsupported outcome.

The serialized measurement bindings freeze their semantics:
`first_correct_viewport` is measured separately for every viewport mutation;
`steady_presented_fps` spans an exact presented-frame interval;
`cold_reflow_convergence` applies only to a reflow gesture selected with the
cold profile; and `snap_back` binds the last Draft/gesture-end boundary to the
first qualifying Standard presented frame. Each binding names its exact source
event/checkpoint anchors, a typed `first_satisfying_observed_frame` selection,
the semantic presented-frame predicate, and the observed boundary class. The
all-frame stream must prove that no earlier presented observation satisfies the
predicate; a sparse checkpoint alone cannot establish firstness.
`keypress_to_first_correct_present` binds the catalog's exact generated key to
its first causally correct `K13.display_completion` frame and requires the complete
K0-through-K13 trace-v2 sequence frozen in
`docs/perf/mux-long-session-performance-campaign.md` section 5.2: native receipt
and mapping, client/server queue and transport, mux dispatch, terminal-lock and
PTY write, causally downstream PTY read/parser application, delta/client apply,
GUI invalidation, paint, GPU/drawable request, and actual display completion.
The key identity and correlation token must remain identical across receipts;
the binding also freezes the key's expected terminal-state mutation and
resulting terminal/renderer generation oracle. Its target is the first actually
presented frame satisfying that semantic effect, never a later generic resize
or settle checkpoint relabeled as key response. Cross-host intervals require
the campaign's retained calibration authority.
This related responsiveness binding does not qualify RQ-S6 unless every exact
RQ-S6 predicate and authority requirement is independently met.

Root-level driver canaries define deterministic focus-window, active-tab,
focused-pane, split-geometry, and topology-manifest changes. Every canary
serializes its request, expected observed event, timeout/tolerance,
prerequisite capability, and applicability predicate. A `p001` cell cannot
claim a focus-window, active-tab, or focused-pane switching canary that its
one-window/one-tab/one-pane topology cannot exercise. Scenarios reference only
applicable canaries, but canary actions remain distinct from the primary
resize/zoom gesture and cannot silently qualify it. Downstream `.3.3` executes
them as driver correctness checks before a run can be admitted.

## Capabilities and unsupported states

Each scenario declares base capabilities, and every overlay declares its closed
required-capability delta for its checkpoints:

- `headless_state_oracle`
- `gpu_visual_capture`
- `native_window_gesture` (native resize or zoom window input)
- `native_display_move`
- `ime_composition`
- `accessibility_geometry`
- `image_protocol`
- `enabled_ligature_shaping`
- `production_mux_domain`
- `real_pty_stream`
- `production_term_window`
- `production_renderer_backend`
- `metal_drawable_capture`
- `software_present_boundary`
- `display_presentation_boundary`
- `display_photon_boundary`
- `native_key_injection`
- `native_color_profile`
- `hdr_edr_output`

Software-present, drawable, display, and photon boundaries are distinct
authority classes. A native-looking window backed by a mock mux/PTY, a generic
GPU image, or a software present timestamp cannot satisfy the production mux,
real PTY, Metal drawable, display, or photon requirements by inference.

Requirement and availability are separate axes on each resolved
scenario-overlay execution. `requirement` is `required`,
`optional`, or `not_applicable`; `availability` is `declared_available`,
`partial`, `unsupported`, `unknown_not_probed`, or `target_dependent` with a
typed profile reference. `declared_available` describes the checked-in
substrate only and is not a run verdict. `partial` and `unsupported` require a
non-empty reason and `tracking_ref`; `unknown_not_probed` remains an explicit
gap, while `target_dependent` names the profile that resolves it. Any unresolved
required row makes that overlay's execution-readiness false and cannot satisfy
a checkpoint; an
unresolved optional photon boundary does not. None of these availability states
makes the scenario definition invalid. This separation records honest gaps
without deleting the scenario, conflating applicability with support, or
silently downgrading an oracle.

Capability derivation is exact rather than advisory:

| Scenario/content condition | Derived capability requirement |
|---|---|
| Every native overlay execution | `headless_state_oracle`, `gpu_visual_capture`, `production_mux_domain`, `real_pty_stream`, `production_term_window`, `production_renderer_backend`, `metal_drawable_capture`, `software_present_boundary`, and `display_presentation_boundary` are required |
| Resize, zoom, or live-resize gesture | `native_window_gesture` is required |
| DPI/display move | `native_display_move` is required |
| Output-overlap resize | `native_key_injection` is required |
| IME, image, accessibility, or color-profile content/state in an overlay | The corresponding capability is required for that overlay |
| Ligature-enabled overlay | `enabled_ligature_shaping` is required together with ligature-bearing input and the pinned renderer/font-feature configuration |
| HDR/EDR output requested | `hdr_edr_output` is required; recording an SDR state alone does not require it |
| Every v1 scenario | `display_photon_boundary` is optional; it cannot support a photon claim unless a later typed opt-in profile makes it required and supplies a detector |

Capability requirements are derived separately for every overlay from its
materialized content, complete pane-state manifest, and renderer-configuration
profile. IME, image-protocol, accessibility state, or enabled ligature shaping
therefore cannot be hidden in a non-focused pane or copied feature list while
declaring the corresponding substrate `not_applicable`. An incompatibility can
exclude an overlay from one measurement binding, but cannot erase that
gesture-by-fleet cell's coverage obligation.

The validation report exposes definition validity and execution readiness as
separate per-overlay verdicts. Root aggregation uses two unambiguous fields:
`production_default_execution_ready` reflects only the base production-default
overlay, while `coverage_suite_execution_ready` is true only when every required
overlay is ready. Every unresolved required capability, unavailable
materialization step, and unavailable native checkpoint contributes a typed
readiness gap to the affected overlay and the coverage-suite aggregate, but it
does not falsely make the production-default overlay unrunnable. Consumers must
not interpret `valid: true`, production-default readiness, or one ready overlay
as complete feature coverage, support, or green.

### Accessibility scope

This renderer catalog models accessibility focus and visible cell geometry only.
The referenced accessibility corpus supplies five event-order/coalescing
sequences, not geometry or native assistive-technology proof. Keyboard traversal,
VoiceOver roles/names/values/announcements, reduced motion, low-vision contrast,
IME/recovery behavior, and their human-review locks remain in the product
accessibility journeys. Deterministic state replay cannot mint VoiceOver support
or substitute for a human review.

## Bounded decoding and validation

The typed decoder rejects an empty document, a document over the fixed byte
limit, unknown or duplicate struct fields, malformed JSON, and trailing data.
Semantic validation is deterministic and reports stable machine codes plus
JSON-style paths. It rejects at least:

- unknown contract or schema versions;
- empty required IDs, descriptions, or reference lists;
- duplicate IDs, seeds, composite keys, events, checkpoints, capabilities, or
  corpus bindings;
- an incomplete or extra 32-cell matrix;
- invalid fleet counts or gesture transitions;
- non-contiguous events, regressing offsets, or invalid checkpoint ordinals;
- missing begin/mutation/settle checkpoints, missing/duplicate conditional
  snap-back, an empty checkpoint invariant set, or an invariant absent from an
  applicable checkpoint;
- missing non-blank begin baseline, invalid invariant applicability, or an
  RQ-S11 checkpoint pair without its typed `draft` to `standard` transition;
- incomplete terminal-feature accounting or capability derivation;
- malformed, absolute, parent-traversing, or dangling repository references;
- malformed `partial`, `unsupported`, `unknown_not_probed`, or
  `target_dependent` capability availability; missing required reasons,
  profile references, or exact resolvable Bead tracking references;
- contradictory state, workload, or oracle declarations.

JSON Schema validates structural shape independently. Rust semantic validation
owns cross-row, exact-matrix, transition, reference, content-accounting, and
authority-source invariants.

## Deliberate defect controls

Version 1 freezes these exact negative-control, detector, and failure-code
bindings:

| Control ID | Bound detector ID | Expected failure code |
|---|---|---|
| `missing_glyph` | `no_missing_glyphs` | `RSC-CONTROL-001` |
| `mixed_renderer_generation` | `coherent_renderer_generation` | `RSC-CONTROL-002` |
| `cursor_displacement` | `cursor_geometry` | `RSC-CONTROL-003` |
| `selection_loss` | `selection_geometry` | `RSC-CONTROL-004` |
| `stale_image` | `image_geometry` | `RSC-CONTROL-005` |
| `ime_geometry_displacement` | `ime_geometry` | `RSC-CONTROL-006` |
| `hyperlink_range_corruption` | `hyperlink_geometry` | `RSC-CONTROL-007` |
| `alternate_screen_flip` | `alternate_screen_state` | `RSC-CONTROL-008` |
| `grid_dimension_mismatch` | `exact_row_width` | `RSC-CONTROL-009` |
| `duplicate_stale_frame` | `no_stale_or_duplicate_frame` | `RSC-CONTROL-010` |
| `accessibility_geometry_displacement` | `accessibility_geometry` | `RSC-CONTROL-011` |
| `blank_frame_after_nonblank` | `nonblank_after_baseline` | `RSC-CONTROL-012` |
| `mixed_generation_tear_band` | `no_mixed_generation_tear_band` | `RSC-CONTROL-013` |

Each serialized control also names an exact scenario ID, overlay ID, checkpoint
ID, injected phase, bound detector ID, and any required terminal feature.
The validator proves that the checkpoint belongs to the resolved overlay, its
phase and detector applicability agrees, and the overlay actually contains the
prerequisite feature. Detector IDs are reusable classes across the 32 cells,
not globally unique injection targets. State invariants remain a distinct
phase-applicability model. This contract proves only that definitions are
complete and internally bound; downstream `.3.5` executes the visual/state
controls against real captures, and `.3.7` separately owns
rig/event/clock/artifact/teardown canaries.

## Structured run vocabulary

Later runners must log `scenario_id`, `overlay_id`, overlay/materialization and
renderer-configuration profile revisions, `phase`, `event_ordinal`,
`expected_invariant_id` where applicable, `expected_detector_id`, detector
scope and nonlocal endpoints where applicable, and `detector_result` for every
detector. Retained run artifacts must also name
catalog revision, seed, target identity, selected cadence and preconditioning
profile IDs, exact ordered materialization identity, actual
refresh/VRR/frame phase, observed presentation-boundary
class, precondition receipt, bundle and renderer identity, corpus hashes,
checkpoint artifacts, canary outcomes, and capability verdicts. This v1
catalog contains none of those verdicts.

## Verification boundary

Focused contract prechecks are package-scoped and safe to run on a remote build
worker. They validate the checked-in JSON against both the typed contract and
Draft 2020-12 schema, verify repository references, check exact matrix,
content accounting, and authority-source bindings, exercise mutation cases,
and prove deterministic round trips.
They do not replace the AGENTS-mandated workspace check, workspace Clippy, and
format gates listed after them.

The following are the frozen fail-closed proof commands, not claims that proof
has already run. Closeout replaces `<exact-candidate-sha>` with the committed
candidate and retains remote worker/job identity plus output in the Bead and
negative-evidence ledger:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-audit-types-check \
  cargo check -p frankenterm-core-audit-types --all-targets

RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-catalog-test \
  cargo test -p frankenterm-core --test renderer_scenario_catalog \
  --no-default-features -- --nocapture

RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-catalog-clippy \
  cargo clippy -p frankenterm-core-audit-types --lib -- -D warnings

RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-integration-clippy \
  cargo clippy -p frankenterm-core --test renderer_scenario_catalog \
  --no-default-features -- -D warnings

RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-workspace-check \
  cargo check --workspace --all-targets

RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-workspace-clippy \
  cargo clippy --workspace --all-targets -- -D warnings

RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec \
  --base <exact-candidate-sha> --clean-overlay --no-overlay -- \
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-workspace-fmt \
  cargo fmt --check
```

All three broad gates must pass remotely before closeout. If RCH rejects a
command, cannot place it on an admissible worker, or reports local fallback,
that exact outcome is retained as an infrastructure blocker rather than being
relabelled as source proof. No local Cargo command substitutes for it.

The retained source artifacts are this document,
`docs/design/renderer-scenario-catalog.v1.json`,
`docs/json-schema/ft-renderer-scenario-catalog.json`, the typed module, and the
integration test. RCH local fallback, target execution, or an uncommitted
source overlay is not admissible proof.

No contract test launches the GUI, opens a window, contacts a mux domain, reads
an active pane, or qualifies a native target. Native execution belongs to the
downstream `.3.2` through `.3.8` lanes and requires explicit user authorization.
