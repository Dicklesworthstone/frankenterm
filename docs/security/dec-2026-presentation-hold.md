# DEC 2026 Renderer Presentation-Hold

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.1.cont] / `ft-u6jos`
**Parent:** `ft-d7af6` (term-layer state machine shipped at
`12b684db6` — `synchronized_output: bool` field + BSU/ESU
dispatch + `Terminal::synchronized_output()` getter).
**Status:** Foundation slice shipped. State machine + 4-app
conformance corpus types + RolloutPhase staging + BFS proof
harness all live; production paint.rs wiring + actual VHS-
captured fixtures are integration follow-ons (require GPU
runtime).

## Headline behavior

> When the term layer's `synchronized_output` flag is set, the
> renderer **holds presentation** — accumulating dirty bits
> but suppressing `Present` calls. On the true→false
> transition, the renderer flushes a single frame. Visible
> result: zero intermediate flicker even when the app issues
> many partial redraws inside the BSU/ESU bracket.

## Protocol

DEC private mode 2026, "synchronized output":

| Sequence | Term-layer effect | Renderer effect |
|---|---|---|
| `CSI ? 2026 h` (BSU — Begin Synchronized Update) | sets `synchronized_output = true` | enter Hold; suppress `Present` until ESU/Reset |
| `CSI ? 2026 l` (ESU — End Synchronized Update) | clears `synchronized_output = false` | flush single frame using union of accumulated dirty lines |

## State machine

`PresentationHoldState`:

```text
            ┌─────────── Bsu ───────────┐
            │                            ▼
   ┌────────────────┐              ┌──────────────────┐
   │   Idle         │              │   Hold           │
   │ (active=false, │              │ (active=true,    │
   │  dirty={})     │◀── Esu/      │  dirty={accum})  │
   │                │   Reset ─────┤                  │
   └────────────────┘  (Flush iff  └──────────────────┘
            │          dirty≠∅)            │
            │                              │
            ▼ FrameReady                   ▼ FrameReady
        Present                         Hold
            ▼ DirtyLineMarked              ▼ DirtyLineMarked
        NoOp                            (insert into dirty)
```

Source: `crates/frankenterm-core/src/dec_2026_presentation_hold.rs`.

## Safety invariants (proven)

`PresentationHoldViolation` enumerates the 4 named failures
the BFS harness checks every reachable state for:

| # | Violation | Bug class |
|---|---|---|
| 1 | `OrphanHeldLines` | dirty set non-empty but flag is false (state-machine drift) |
| 2 | `SaturatedCounter` | runaway `bsu_count_total` / `esu_count_total` / `frames_held_total` / `frames_flushed_total` |
| 3 | `HoldOutsideWindow` | `Hold` outcome fired with flag clear (renderer ignored hold rule) |
| 4 | `PresentDuringWindow` | `Present` fired with flag set (renderer ignored hold rule) |

Verified by:

| Test | Coverage |
|---|---|
| `bfs_exhausts_state_space_clean_at_depth_5` | exhaustive BFS over reachable states up to depth 5 (every event from `action_alphabet()`) |
| `random_schedule_sweep_no_violations` (lib) | 1024 × 16 = ~16k transitions |
| `high_volume_event_sweep_invariants_hold` (harness) | 1024 × 96 = ~98k transitions |
| `canonical_redraw_window_holds_then_flushes` | bead's headline scenario end-to-end |
| `no_double_flush_on_back_to_back_esu` | back-to-back ESU defensive |
| `nested_bsu_does_not_double_count_held_dirty` | overlapping-BSU misuse handled |
| `reset_during_active_hold_flushes_then_idle` | Reset implicit-end behavior |

## Per-app conformance corpus

`ConformanceApp` enumerates the bead's 4 apps:

| App | Slug | Coverage rationale |
|---|---|---|
| `NvimTreesitter` | `nvim_treesitter` | heavy redraw with syntax highlighting |
| `Lazygit` | `lazygit` | staging-area scroll |
| `Btop` | `btop` | full-screen redraw |
| `Ranger` | `ranger` | multi-pane file browser |

`ConformanceFixture` is the per-fixture record contract;
`conformance_corpus()` returns the 4-row baseline. Each
fixture's `meta.json` records:

- `app` — the `ConformanceApp` slug.
- `input_bytes_path` — relative path to the captured byte
  stream.
- `expected_present_count` — **acceptance bound: exactly 1
  `Present` per BSU/ESU window** (the bead's "frame count =
  1, no intermediate frames" rule).
- `expected_frames_held` — count of `FrameReady` events
  suppressed during the hold (populated per-fixture from the
  VHS capture).
- `expected_lines_flushed` — count of dirty lines flushed.

The 4 fixture goldens (input bytes + meta + expected post-
state) ship via VHS capture on the Linux GPU CI runner; the
contract module is the slot they fill.

## ft doctor surface

`SynchronizedOutputHealth` mirrors this session's `*Health`
shape:

```text
synchronized_output_active : bool — current flag
bsu_count_total            : lifetime BSU events
esu_count_total            : lifetime ESU events
frames_held_total          : count of FrameReady ticks suppressed
frames_flushed_total       : count of single-flush emissions
held_lines_now             : current hold window's dirty count
```

`bsu_esu_balanced()` returns true iff `bsu_count_total ==
esu_count_total + (1 if active else 0)` — operators surface
unbalanced bracket counters (apps misusing the protocol).

## Rollout staging

`RolloutPhase` mirrors `ft-mpc9b.9` rollout substrate:

| Phase | User-visible | On-by-default | Operator escape hatch |
|---|---|---|---|
| `Hidden` | no | no | `FT_FEATURE_DEC_2026=force_on` |
| `OptIn` | yes | no | `[features] dec_2026 = "off"` reverts |
| `Default` | yes | yes | (Hidden / OptIn paths remain one release cycle) |

The integration bead progresses Hidden → OptIn → Default per
the rollout policy; this contract layer pins the phase enum
the doctor / config layer reads.

## Re-running

```bash
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib dec_2026_presentation_hold:: \
    --features asupersync-runtime --no-default-features
# → 20 passed (state machine + corpus + rollout)

cargo test -p frankenterm-core --test dec_2026_presentation_hold \
    --features asupersync-runtime --no-default-features
# → 8 passed (BFS proof + ~98k randomized transitions)
```

## Bead acceptance status

| Item | Status |
|---|---|
| Renderer presentation-hold contract layer | ✓ `PresentationHoldState` + 4 named invariants + BFS proof |
| Renderer paint.rs wiring | ⏳ integration follow-on (uses `apply_event` from this module) |
| Per-app conformance corpus types | ✓ `ConformanceApp` + `conformance_corpus()` |
| 4 app fixture goldens (nvim_treesitter / lazygit / btop / ranger) | ⏳ require VHS capture on GPU runner |
| ft doctor telemetry (`synchronized_output_active`, `bsu_count_total`, `esu_count_total`, `frames_held_total`) | ✓ `SynchronizedOutputHealth` snapshot; doctor wiring is one-line projection |
| Feature-flag staging (Hidden → OptIn → Default) | ✓ `RolloutPhase` enum |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Term-layer state (parent):**
  `frankenterm/term/src/terminalstate/mod.rs` —
  `synchronized_output: bool` + BSU/ESU dispatch +
  `Terminal::synchronized_output()` getter.
- **Dirty-line bitmap:** `ft-mpc9b.1.2` / `ft-tfzhy` — the
  substrate the hold accumulates into.
- **Elastic-buffer policy:** `ft-mpc9b.1.3` — synchronized
  output is a "gesture-like" window; shrink should also be
  suppressed during the hold.
- **SLO:** `docs/perf/resize-quality-slo.md` — RQ-S4
  (24h fuzz, 0 critical) + RQ-S11 (snap-back SSIM).
- **Sibling foundation fixtures** (same `*Health` /
  state-machine pattern this session):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`,
  `wire_dedup_model`, `redactor_coverage_matrix`,
  `tui_parity_oracle`, `robot_checkpoint_state_machine`,
  `robot_work_state_machine`, `robot_fleet_state_machine`,
  `wayland_compositor_matrix`, `audit_erasure_spec`,
  `robot_context_state_machine`,
  `gpu_regression_fuzz_report`.
- **Attestation cross-link:** `ft-syqcz.1`.
