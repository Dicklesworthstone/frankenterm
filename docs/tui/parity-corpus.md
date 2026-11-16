# TUI Parity Test Corpus

**Bead:** [`ft-35yac.1.1`](https://github.com/frankenterm/frankenterm/issues) (BR-RC-CUTOVERS.G5.1.1).
**Parent:** [`ft-35yac.1`](https://github.com/frankenterm/frankenterm/issues) (differential oracle harness, closed).
**Sibling:** [`ft-35yac.1.2`](https://github.com/frankenterm/frankenterm/issues) (headless GPU-renderer parity test, open).
**Substrate:** [`crates/frankenterm-core/src/tui_parity_oracle.rs`](../../crates/frankenterm-core/src/tui_parity_oracle.rs) (`synthesized_event_corpus` + `RenderFrame` comparator).

The differential render oracle from `ft-35yac.1` ships an in-tree
`synthesized_event_corpus()` that exercises the keymap surface at the
mechanical level. This document describes the **complementary
real-session corpus** — VHS scripts captured from operators driving the
TUI through realistic workflows. The two corpora compose: synthesized
catches mechanical drift between backends; real-session catches drift
that only surfaces under naturalistic input timing and view transitions.

## Corpus location

```
tests/tui_render_parity/corpus/
├── 01_panes_status_overview.vhs
├── 02_search_workflow.vhs
├── 03_event_triage.vhs
├── 04_workflow_execution_monitoring.vhs
└── 05_mission_tx_inspection.vhs
```

Five scenarios cover the views the operator most commonly lands on, plus
the deepest navigation chain (mission → TX → commit log) where backend
disagreements on cursor restoration after `Escape` historically surface.

## File format

Each `.vhs` file is a [charmbracelet/vhs](https://github.com/charmbracelet/vhs)
script — a deterministic input stream the parity-oracle harness consumes
directly without invoking `vhs` itself. The `Output ./*.gif` directives
are operator-facing only (so a developer can regenerate the visual
artifact for review); the harness ignores them.

Each script's body is a sequence of three primitive types:
- `Type "..."` — keystroke literal
- `Backspace` / `Tab` / `Enter` / `Down` / `Escape` / `Space` — named keys
- `Sleep <duration>` — input pacing (ignored by the deterministic harness;
  retained so a human re-running `vhs <file>.vhs` sees a watchable
  timing)

Modifier-prefixed keys use the `Shift+`, `Ctrl+`, `Alt+` prefix syntax
(e.g. `Shift+Tab` = `KeymapAction::PrevTab`).

## Recording protocol

To add a new corpus entry from a real operator session:

1. **Pick a workflow that's not already covered.** Each existing entry
   targets one of the five base scenarios; new entries should target a
   *novel* view-transition chain or input-density profile (see "Coverage
   matrix" below).

2. **Record with VHS:**
   ```bash
   # Install if needed:
   #   brew install vhs        (macOS)
   #   sudo apt install vhs    (Ubuntu via charmbracelet PPA)

   vhs --record > tests/tui_render_parity/corpus/06_<name>.vhs
   # ... drive the TUI ...
   # Ctrl+D to finish.
   ```

3. **Sanity-check the recording:**
   ```bash
   vhs tests/tui_render_parity/corpus/06_<name>.vhs
   # Produces 06_<name>.gif alongside the .vhs file. Watch it; trim
   # any lead-in/exit noise via plain editing of the .vhs (it's text).
   ```

4. **Add the explanatory header.** Every corpus file's first comment
   block names the scenario, the view-transition path it exercises, and
   the failure class it's intended to catch. See the existing five for
   the template.

5. **Verify the harness consumes it cleanly:**
   ```bash
   cargo test -p frankenterm-core --lib tui_parity_oracle::tests \
     -- --nocapture
   ```

   The harness's `corpus_size_meets_minimum` test asserts at least 5
   files exist; any new entry must keep all existing parity tests green.

6. **Commit the `.vhs` only** — never the `.gif`. The visual artifact is
   regenerable on demand and would bloat the repo.

## Coverage matrix

| File | View chain | Failure class targeted |
|------|-----------|------------------------|
| `01_panes_status_overview.vhs` | home → panes → filters | filter-toggle parity (most-common landing path) |
| `02_search_workflow.vhs` | home → search → per-keystroke filter | per-keystroke render fidelity (FilterAppendChar / Delete / Clear) |
| `03_event_triage.vhs` | home → events → triage | high-density per-frame redraw (digit-filter cycle + TriageNumberedAction) |
| `04_workflow_execution_monitoring.vhs` | home → workflows → step log | tab-cycle wrap-vs-clamp parity at boundaries |
| `05_mission_tx_inspection.vhs` | home → mission → TX → commit log → 3× Escape | cursor-position parity after deep view-pop chain |

New entries should target a novel cell of this matrix — same view chain
+ same failure class is a duplicate; same view chain + new failure class
or new view chain + any failure class is a fit.

## Re-record cadence

Per the BR-RC-CUTOVERS.G5.1.1 acceptance criterion *"Set re-record policy
(per major UI change)"*:

- **Major UI change** = any commit that lands a new top-level view, a new
  keymap action, or a layout change that shifts cell positions of
  existing widgets. Trigger the re-record at the same time as the change.
- **Minor cosmetic change** (color, spacing, label) does NOT require a
  re-record; the differential oracle is colour/style aware and will
  catch the change on the next run regardless of corpus age.
- **No-op refactor** (rename, doc-only) never requires a re-record.

A `// re-record: <reason>` comment in the commit message is the manual
flag the operator follows. There is no automatic enforcement — the
parity oracle's drift detection is the safety net.

## CI integration

The parity oracle harness wired into CI by `ft-35yac.1` (closed) reads
this corpus directly. Specifically, `crates/frankenterm-core/tests/`
test files using `tui_parity_oracle::synthesized_event_corpus()` should
be extended (in a separate follow-up bead) to also load every `.vhs`
file under `tests/tui_render_parity/corpus/` and run the differential
comparison on each. Until that wiring lands, the corpus exists as a
checked-in regression seed any contributor or CI lane can drive on
demand via `vhs <file>.vhs`.

The wiring step is intentionally deferred to keep this bead's scope
focused on **corpus production**; the consumption side already has a
stable API (`EventScript` + `synthesized_event_corpus()`) which a
future loader-from-disk layer can mirror.

## Cross-references

- [`crates/frankenterm-core/src/tui_parity_oracle.rs`](../../crates/frankenterm-core/src/tui_parity_oracle.rs) — `EventScript`, `RenderFrame`, `FrameDiff`, `synthesized_event_corpus`.
- [`ft-35yac`](../proposals/) — Reality-Check Cutovers epic.
- [`ft-35yac.1`](../../) — Differential render oracle harness (closed).
- [`ft-35yac.1.2`](../../) — Headless GPU-renderer parity test (open).
- [`ft-35yac.2`](../../) — Default ftui in shipped binaries (blocked on .1.1 + .1.2).
- [charmbracelet/vhs](https://github.com/charmbracelet/vhs) — recording tool.
