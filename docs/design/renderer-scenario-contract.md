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
scenario-specific and non-zero. IDs, seeds, workload identities, and event
ordinals are stable inputs; changing one requires a catalog revision.

## State contract

Every cell declares complete initial and final state rather than relying on
ambient GUI defaults:

- pixel width and height;
- terminal rows and columns;
- font size and scale;
- DPI and display identity;
- exact pane, tab, and window counts;
- focused window, active tab, and focused pane ordinals;
- distinct initial and final inline typed topology, split geometry, and
  complete per-pane state manifests;
- ordered per-window tab sequences with stable tab IDs and contiguous ordinals;
- scrollback line count and viewport top;
- grid and terminal revision IDs;
- alternate-screen, selection anchors, cursor coordinates, IME preedit/caret,
  candidate-window rectangle, composition range/segments, and input-source
  identity, image anchors, hyperlink ranges, and accessibility focus/geometry
  state;
- display color-space/profile identity and HDR/EDR mode/availability;
- terminal-content corpus references;
- renderer configuration and pinned-font references.

Rows, columns, pixel sizes, DPI, base font size, and scale must be positive.
Fleet counts must equal the cell's exact fleet point, and ordinals and terminal
coordinates must be in bounds. The top-level focused `surface_state` must
exactly equal the focused-pane entry in the corresponding inline manifest.
Initial, final, and checkpoint manifests deterministically carry every
pane/tab/window, split geometry, content identity, and output distribution for
that phase. Each phase manifest contains inline typed windows, ordered tabs,
panes, split trees and rectangles, focus/active IDs, each pane's complete
surface state, an exact pane-to-content-corpus mapping, and output distribution,
plus the feature union derived from those corpus bindings. The validator checks
count agreement, uniqueness, referential integrity, complete pane coverage,
split-tree geometry, action-target agreement, exact focused-state equality, and
exact feature-union equality. An opaque reference or one phase-ambiguous
manifest cannot impersonate all three. Final state must implement the gesture's
declared transition. A
same-grid gesture must retain rows and columns; reflow gestures must use their
exact 80/200 endpoints; zoom gestures must retain configured base font size
while moving logical font scale in the declared direction and deriving
effective font size/cell metrics through the pinned derivation revision.
Display identity, DPI, and display scale remain unchanged during a zoom. A
display move must change both display identity and DPI; a display-scale change
alone is not a display move. It also declares its before/after color profile and
HDR/EDR state. Draft, Standard, and Fancy must all honor the selected color
profile; a quality transition cannot silently change gamut. `output_overlap_resize` also declares whether its
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
field actions. Output-overlap timelines explicitly start PTY output, inject
foreground-key actions, perform resize events while output remains active, and
then stop output; merely containing those actions in a non-overlapping order is
invalid. Live-resize snap-back timelines also encode the closed production
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

The catalog freezes these eight intermediate invariant classes. Each class has
canonical phases plus a gesture/feature condition. A scenario omits a
conditional invariant when that condition is false; it must not fabricate a
non-empty applicability set. The same invariant ID is intentionally evaluated
at every applicable checkpoint; it is not a one-shot assertion that can make
later transient frames invisible:

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
- `alternate_screen_isolation` — alternate-screen identity is not merged with
  primary scrollback;
- `accessibility_focus_geometry` — accessibility focus is exclusive and
  geometry matches the visible cell map; and
- `final_state_convergence` — the final frame converges to the declared final
  terminal state.

The output-overlap family additionally fixes aggregate PTY output at exactly
1,000,000 bytes/s and declares an exact foreground key event; its `p050`
related RQ-S6 cross-map requires exactly one event with pinned logical key,
modifiers, and encoded bytes. Concurrent resize makes this an adversarial
superset, not an exact RQ-S6 scenario. Output generator revision, seed, payload
identity, and pane distribution are part of workload identity. Other families
declare zero background output unless their workload explicitly says otherwise.

## Checkpoint policy

Every scenario has at least three ordered checkpoints; live-resize families
have at least four:

1. `begin` before the first mutation;
2. one or more `mutation` checkpoints during the gesture;
3. exactly one `snap_back` checkpoint after `gesture_end` for same-grid,
   grid-changing, reflow, and output-overlap resize cells; and
4. `settle` at the declared terminal event ordinal.

Zoom and DPI/display-move cells omit `snap_back`; adding one would invent a
production guarantee that their gesture family does not own.

A checkpoint binds all of the following:

- event ordinal and phase;
- expected state-invariant IDs;
- expected detector IDs and expected frame-content class;
- phase-specific complete inline typed pane/surface-state manifest;
- terminal-state oracle reference;
- visual oracle reference and one or more comparator-policy references;
- accessibility oracle reference;
- whether native capture is required.

Every checkpoint binds at least one applicable invariant and every applicable
detector. The `begin` checkpoint has an explicit `nonblank` frame-content class,
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
| `alternate_screen_isolation` | begin, mutation, snap_back, settle | content includes alternate screen; snap_back only for live-resize |
| `accessibility_focus_geometry` | begin, mutation, snap_back, settle | content includes accessibility geometry; snap_back only for live-resize |
| `final_state_convergence` | settle | every gesture |

A checkpoint's invariant IDs equal the complete applicable set; subset binding
is invalid.

State invariants are not the entire visual oracle. Version 1 freezes these 20
serialized detector IDs in this order:

| Detector ID | Scope | Phases | Condition/policy |
|---|---|---|---|
| `no_missing_glyphs` | single checkpoint | begin, mutation, snap_back, settle | every gesture; snap_back only for live-resize |
| `coherent_cell_widths` | single checkpoint | begin, mutation, snap_back, settle | every gesture; snap_back only for live-resize |
| `exact_row_width` | single checkpoint | begin, mutation, snap_back, settle | every gesture; snap_back only for live-resize |
| `no_flicker` | interval | mutation, snap_back, settle | explicit interval ending at the phase checkpoint |
| `coherent_renderer_generation` | single checkpoint | begin, mutation, snap_back, settle | every gesture; snap_back only for live-resize |
| `no_mixed_generation_tear_band` | single checkpoint | mutation, snap_back, settle | every gesture; snap_back only for live-resize |
| `no_stale_or_duplicate_frame` | checkpoint pair | mutation, snap_back, settle | explicit source and target checkpoints |
| `nonblank_after_baseline` | checkpoint pair | mutation, snap_back, settle | nonblank begin baseline and current checkpoint |
| `ssim_policy` | checkpoint/oracle pair | begin, mutation, snap_back, settle | comparator-policy reference |
| `l_inf_policy` | checkpoint/oracle pair | begin, mutation, snap_back, settle | comparator-policy reference |
| `changed_pixel_fraction_policy` | checkpoint/oracle pair | begin, mutation, snap_back, settle | reported, but non-independent until `.3.5.1` |
| `exact_terminal_state` | single checkpoint | begin, mutation, snap_back, settle | phase-specific terminal-state oracle |
| `cursor_geometry` | single checkpoint | begin, mutation, snap_back, settle | content includes cursor |
| `selection_geometry` | single checkpoint | begin, mutation, snap_back, settle | content includes selection |
| `ime_geometry` | single checkpoint | begin, mutation, snap_back, settle | content includes IME |
| `hyperlink_geometry` | single checkpoint | begin, mutation, snap_back, settle | content includes hyperlinks |
| `image_geometry` | single checkpoint | begin, mutation, snap_back, settle | content includes images |
| `alternate_screen_state` | single checkpoint | begin, mutation, snap_back, settle | content includes alternate screen |
| `accessibility_geometry` | single checkpoint | begin, mutation, snap_back, settle | content includes accessibility geometry |
| `exactly_one_standard_snap_back` | whole timeline | snap_back | exactly one snap-back-role checkpoint and one Draft-to-Standard transition; later Standard settle is allowed |

Single-checkpoint bindings live on checkpoints. Pair, interval, and
whole-timeline bindings are separate typed records with exact source/target or
interval endpoints; a checkpoint-local ID cannot impersonate sequence proof.
Every applicable detector has exactly one correctly scoped binding. The
changed-pixel detector remains explicitly non-independent until `.3.5.1`
repairs its comparator semantics.

Checkpoints are deterministic oracle anchors, not a license to ignore the
transient stream between them. Every observed, captured, or presented frame
from `gesture_begin` through `settle` carries its event interval, phase,
renderer-generation identity, and all applicable per-frame and interval
detector verdicts. A scenario with one mutation checkpoint therefore still
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

Every one of the 32 gesture-by-fleet scenarios, in every initial, final, and
checkpoint phase manifest, must derive the complete closed terminal-feature
union:

`ascii`, `cjk`, `rtl`, `combining_marks`, `emoji`, `ligatures`, `images`,
`hyperlinks`, `alternate_screen`, `selection`, `cursor`, `ime`, and
`accessibility_geometry`.

This is not global singleton coverage. The `p001` cells use a canonical
mixed-content pane/corpus that carries all 13 features; larger fleets distribute
the same feature set deterministically across panes, but no gesture, fleet
point, or phase may omit one. This keeps the p200 large-session cells and every
gesture axis subject to the same appearance gates, and it derives IME, image,
and accessibility capability requirements for every cell.

Content inputs and evidence authority are separate collections. A content
corpus has a stable ID, relative repository reference, feature set, payload or
generator revision, and is referenced by workloads. An evidence source has its
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

| Feature or source | v1 classification | Reason |
|---|---|---|
| ASCII, CJK, RTL, combining marks, emoji, cursor | `direct` | Complete `input.json`, `meta.json`, `expected.json`, and `golden.png` fixture packages exist |
| Selection | `partial` | Static char/line/word fixtures exist; a continuous drag fixture does not |
| IME | `partial` | `overlay-ime-composition` is a static visual with `ime_disabled: true`, not live composition |
| Ligatures, images, hyperlinks, alternate screen | `gap` | No canonical complete GPU fixture exists |
| Accessibility geometry | `gap` | The five accessibility scenarios cover event semantics, not cell geometry |
| `multipane-resize-static-snapshot` | `partial` | One static frame is not a native continuous resize gesture |
| `tests/golden/gpu/stress/*` | `present_unqualified` | Seven fixture directories exist without required `meta.json` identity |

`partial`, `gap`, and `present_unqualified` rows require a reason and tracking
Bead. They cannot satisfy a native checkpoint or be promoted to `direct` by a
consumer. The missing stress metadata is owned by `.3.6.1`; missing non-a11y
visual fixtures remain tracked by `ft-ruona` and the product visual-corpus lane.
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
| `RQ-S10.atlas_rebuild_count` | exactly 100 resize mutations, unchanged font/scale, and zero new glyphs | exact scenario predicate |
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

Every cell declares:

- exact pane, tab, and window counts plus initial/final/checkpoint
  layout/topology manifests;
- scrollback lines per pane;
- terminal content-corpus IDs;
- aggregate output bytes per second plus output generator ID, revision, seed,
  payload identity, and pane-distribution policy;
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

Every scenario binds measurement contract
`ft.renderer.measurement-contract.v1`. Its closed, ordered stage IDs are:

1. `native_gesture_callback`
2. `main_thread_return`
3. `mux_resize_dispatch`
4. `pane_resize_apply`
5. `terminal_lock_wait`
6. `terminal_lock_hold`
7. `terminal_reflow`
8. `text_shaping`
9. `glyph_atlas`
10. `webgpu_encode_submit`
11. `metal_drawable`
12. `software_present`
13. `display_presented`

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
also retain actual consecutive `display_presented` intervals. The
`main_thread_return`, `terminal_lock_wait`, and `terminal_lock_hold` IDs remain
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
and target events or checkpoints plus the observed boundary class.
`keypress_to_first_correct_present` binds the catalog's exact generated key to
its first causally correct `display_presented` frame and requires the complete
K0-through-K13 trace-v2 sequence frozen in
`docs/perf/mux-long-session-performance-campaign.md` section 5.2: native receipt
and mapping, client/server queue and transport, mux dispatch, terminal-lock and
PTY write, causally downstream PTY read/parser application, delta/client apply,
GUI invalidation, paint, GPU/drawable request, and actual display completion.
The key identity and correlation token must remain identical across receipts;
cross-host intervals require the campaign's retained calibration authority.
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

Each scenario declares the closed capability set needed by its checkpoints:

- `headless_state_oracle`
- `gpu_visual_capture`
- `native_window_gesture` (native resize or zoom window input)
- `native_display_move`
- `ime_composition`
- `accessibility_geometry`
- `image_protocol`
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

Requirement and availability are separate axes. `requirement` is `required`,
`optional`, or `not_applicable`; `availability` is `declared_available`,
`partial`, `unsupported`, `unknown_not_probed`, or `target_dependent` with a
typed profile reference. `declared_available` describes the checked-in
substrate only and is not a run verdict. `partial` and `unsupported` require a
non-empty reason and `tracking_ref`; `unknown_not_probed` remains an explicit
gap, while `target_dependent` names the profile that resolves it. Any unresolved
required row makes `execution_ready` false and cannot satisfy a checkpoint; an
unresolved optional photon boundary does not. None of these availability states
makes the scenario definition invalid. This separation records honest gaps
without deleting the scenario, conflating applicability with support, or
silently downgrading an oracle.

Capability derivation is exact rather than advisory:

| Scenario/content condition | Derived capability requirement |
|---|---|
| Every native scenario | `headless_state_oracle`, `gpu_visual_capture`, `production_mux_domain`, `real_pty_stream`, `production_term_window`, `production_renderer_backend`, `metal_drawable_capture`, `software_present_boundary`, and `display_presentation_boundary` are required |
| Resize, zoom, or live-resize gesture | `native_window_gesture` is required |
| DPI/display move | `native_display_move` is required |
| Output-overlap resize | `native_key_injection` is required |
| IME, image, accessibility, or color-profile content/state | The corresponding capability is required |
| HDR/EDR output requested | `hdr_edr_output` is required; recording an SDR state alone does not require it |
| Every v1 scenario | `display_photon_boundary` is optional; it cannot support a photon claim unless a later typed opt-in profile makes it required and supplies a detector |

Capability requirements are derived from every content corpus and complete
pane-state manifest used by the scenario. IME, image-protocol, and
accessibility content therefore cannot be hidden in a non-focused pane while
declaring the corresponding capability `not_applicable`.

The validation report exposes definition validity and execution readiness as
separate verdicts. A structurally and semantically valid catalog may still have
`execution_ready: false`; every unresolved required capability and every
unavailable native checkpoint contributes a typed readiness gap. Consumers
must not interpret `valid: true` as runnable, supported, or green.

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

Each serialized control also names an exact scenario ID, checkpoint ID,
injected phase, bound detector ID, and any required terminal feature.
The validator proves that the checkpoint belongs to the scenario, its phase and
detector applicability agrees, and the scenario actually contains the
prerequisite feature. Detector IDs are reusable classes across the 32 cells,
not globally unique injection targets. State invariants remain a distinct
phase-applicability model. This contract proves only that definitions are
complete and internally bound; downstream `.3.5` executes the visual/state
controls against real captures, and `.3.7` separately owns
rig/event/clock/artifact/teardown canaries.

## Structured run vocabulary

Later runners must log `scenario_id`, `phase`, `event_ordinal`,
`expected_invariant_id` where applicable, `expected_detector_id`, detector
scope and nonlocal endpoints where applicable, and `detector_result` for every
detector. Retained run artifacts must also name
catalog revision, seed, target identity, selected cadence and preconditioning
profile IDs, actual refresh/VRR/frame phase, observed presentation-boundary
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
  env CARGO_TARGET_DIR=/tmp/ft-4tenz-3-1-audit-types-test \
  cargo test -p frankenterm-core-audit-types --lib renderer_scenario_catalog

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
