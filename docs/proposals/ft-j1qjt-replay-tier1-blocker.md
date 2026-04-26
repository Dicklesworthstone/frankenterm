# ft-j1qjt blocker: replay is not a tier-1 leaf

**Bead:** [ft-j1qjt](../../.beads/issues.jsonl) (was ft-y0loj.4 — extract `frankenterm-core-replay`)
**Status:** extraction attempted and reverted; filing a follow-up
**Related:** [ft-y0loj-monolith-split.md](./ft-y0loj-monolith-split.md), ft-mr35k (ars extraction, succeeded), ft-y0loj.1 (tantivy, succeeded), ft-usvnt (resource-types leaf, succeeded)

## Finding

The 28-module replay cluster (`replay`, `replay_artifact_registry`,
`replay_capture`, `replay_checkpoint`, …, `replay_usability_pilot`)
**cannot** be extracted as a tier-1 leaf the way ars and tantivy were.
The original ft-y0loj proposal classified replay as tier-1 because the
cluster's *internal* dependencies are self-contained, but a deeper
audit of *outbound* references — non-replay core files reaching into
replay modules — found a hard cycle.

## Outbound references from core to replay

`grep -n 'crate::replay' crates/frankenterm-core/src/{policy,runtime,recorder_replay,workflows/runner,workflows/mod}.rs`:

| file                                                | site count | types referenced                                                                 |
|-----------------------------------------------------|-----------:|----------------------------------------------------------------------------------|
| `policy.rs`                                         | 11         | `replay_capture::{SharedCaptureAdapter, DecisionEvent, DecisionType, CollectingCaptureSink, CaptureAdapter, CaptureConfig}` |
| `runtime.rs`                                        | 4          | `replay_capture::{SharedCaptureAdapter, CollectingCaptureSink, CaptureAdapter, CaptureConfig}` |
| `recorder_replay.rs`                                | 1          | `replay_fixture_harvest::FtreplayArtifact`                                       |
| `workflows/runner.rs`                               | 3          | `replay_capture::{SharedCaptureAdapter, DecisionEvent, DecisionType}`            |
| `workflows/mod.rs`                                  | 1          | `replay_capture::SharedCaptureAdapter`                                           |
| **total**                                           | **20**     |                                                                                  |

These are all "policy/runtime/workflow records a decision through the
capture adapter" sites — the replay subsystem is not a leaf; it's
deeply wired into the live decision-recording path.

## Why this kills the tier-1 extraction

The previous tier-1 extractions (ars, tantivy) had zero outbound
references from non-cluster core to cluster modules. Removing them
leaves core compiling cleanly, and the moved cluster only depends back
on core (one-way edge, no cycle).

Replay has a bidirectional edge:

- **replay → core** — replay modules use `crate::event_id`,
  `crate::ingest`, `crate::policy`, `crate::recorder_invariants`,
  `crate::recording`, `crate::runtime_compat`. (Same pattern as ars
  and tantivy; not the problem.)
- **core → replay** — the 20 sites above use
  `crate::replay_capture::*` and `crate::replay_fixture_harvest::*`.
  Moving replay to its own crate makes these paths fail to resolve.

Resolving by having core depend on the new replay crate creates a
**regular cargo cycle** (not a dev-dep cycle, which is allowed).
Cargo rejects it.

## Path forward

Apply the **resource-types-leaf** pattern from ft-usvnt (which broke a
similar `core → fleet` back-edge by extracting the shared types into
their own leaf crate):

1. **ft-j1qjt.1 — `frankenterm-core-capture-types` leaf crate.**
   Extract the specific types core depends on:
   - `replay_capture::SharedCaptureAdapter`
   - `replay_capture::DecisionEvent`
   - `replay_capture::DecisionType`
   - `replay_capture::CollectingCaptureSink`
   - `replay_capture::CaptureAdapter`
   - `replay_capture::CaptureConfig`
   - `replay_fixture_harvest::FtreplayArtifact`

   Carve them into their own minimal crate with just `serde` +
   `chrono` + (transitively) any other types they reach into. Same
   pattern as `frankenterm-core-resource-types`. Re-export from core
   under `crate::replay_capture::*` etc. so the 20 call sites keep
   resolving.

2. **ft-j1qjt.2 — extract `frankenterm-core-replay` proper.** Now the
   replay sub-crate depends on `frankenterm-core-capture-types` (not
   `frankenterm-core`), and the old `replay_capture` module reduces to
   re-exporting the leaf types alongside its own non-extracted
   helpers. The cycle is broken.

3. **Optional: revisit before step 1.** Audit whether any of the 20
   sites can be deleted (e.g. by inlining the capture-record write
   into a smaller adapter) rather than extracted. If 4-5 of them are
   removable, the leaf-crate surface gets smaller and the extraction
   gets easier.

## Operational outcome

- The ft-j1qjt extraction attempt was fully reverted in this commit:
  28 files git-mv'd back, lib.rs `pub mod replay*` declarations
  restored, in-file `frankenterm_core::*` imports reverted to
  `crate::*`, 36 test files reverted to `frankenterm_core::replay*`,
  workspace member entry removed, dev-dep removed, and
  `crates/frankenterm-core-replay/` directory left empty (lib.rs
  + Cargo.toml with stub modules) for the future ft-j1qjt.2 to fill.
- This proposal lands as the documentation of the blocker so the next
  pane that picks up ft-j1qjt has the context.
- ft-j1qjt is reset to OPEN by closing it with the "blocked-by-cycle"
  reason; the follow-up beads ft-j1qjt.{1,2} carry the actual work.

## Acceptance criteria for THIS proposal

- [ ] Reviewer signs off on the leaf-crate-first approach.
- [ ] ft-j1qjt.1 and ft-j1qjt.2 child beads filed under ft-j1qjt.
- [ ] The empty `crates/frankenterm-core-replay/` scaffold is either
      removed or repurposed as the destination for ft-j1qjt.2.
