# Atlas tiered-swap operator playbook

Operator-facing playbook for running atlases through the
three-tier (VRAM / HostRam / Disk) cascade. Assembles every
substrate piece + tooling into a single runtime workflow. Pairs
with the existing surfaces:

- [`atlas-tiered-swap-wgpu-integration.md`][wgpu] — cc_1's
  per-frame copy-command emission runbook.
- [`storage_backend_callsites.py` + `backend-migration-guide.md`][storage]
  — sister storage-backend migration aids; not tier-swap-specific
  but referenced from the bench / ft doctor surfaces below.

[wgpu]: ./atlas-tiered-swap-wgpu-integration.md
[storage]: ../storage/backend-migration-guide.md

## What's in scope

The atlas tiered-swap workflow has four substrate pieces, two
operator scripts, and one ft doctor surface. This playbook
sequences them into the runtime loop the renderer engineer
implements.

### Substrate

- [`atlas_tiered_swap`][substrate] — `AtlasTier` cascade,
  `EvictionAction` decision logic, `TierSwapStats` counters
  ([ft-mpc9b.2.11][parent]).
- `atlas_tiered_swap::StagingTransferQueue` — per-window
  VRAM↔HostRam transfer queue ([ft-ktd19.1 substrate at
  dbc8a146c][staging]).
- `atlas_tiered_swap::FrameBudgetSwapDeferrer` — admit / defer
  decision against a per-frame byte budget ([ft-ktd19.3 substrate
  at bdd118ed8][deferrer]).
- [`atlas_tier_doctor`][doctor] — typed `ft doctor` surface for
  TierSwapStats with VRAM + host RAM pressure thresholds
  ([ft-ktd19.2 substrate at fa1edc458][docrev]).

[substrate]: ../../crates/frankenterm-core/src/atlas_tiered_swap.rs
[parent]: https://github.com/frankenterm/frankenterm/issues?q=ft-ktd19
[staging]: https://github.com/frankenterm/frankenterm/issues?q=ft-ktd19.1
[deferrer]: https://github.com/frankenterm/frankenterm/issues?q=ft-ktd19.3
[doctor]: ../../crates/frankenterm-core/src/atlas_tier_doctor.rs
[docrev]: https://github.com/frankenterm/frankenterm/issues?q=ft-ktd19.2

### Operator scripts

- [`atlas_blit_budget_calculator.py`][calc] — picks a per-frame
  blit cap from target_fps + bus class + atlas tile size. Run at
  deployment time, retain the resulting JSON in operator runbooks
  for the deployment hardware profile.

[calc]: ../../scripts/atlas_blit_budget_calculator.py

### Doctor surface

- `ft doctor --json` emits an `atlas_tier_swap` section (when
  the GUI is in-process; CLI mode emits the `no_atlases_in_process`
  sentinel). Schema in
  [`crates/frankenterm-core/src/atlas_tier_doctor.rs`][doctor].

## Per-frame loop

The renderer engineer implements the loop below (the wgpu
integration runbook has the copy-command-emission detail; this
section sequences it against the deferrer + doctor):

```
each frame {
    swap_deferrer.reset_for_new_frame();
    let drained = staging_queue.drain_pending();

    let (admitted, deferred) = swap_deferrer.partition(drained);

    for event in admitted {
        emit_wgpu_copy_command(event);
    }

    // Re-enqueue the deferred events so they get another shot
    // next frame. The queue's FIFO ordering means the deferred
    // events keep their relative priority.
    for event in deferred {
        staging_queue.push(event);
    }

    record_tier_swap_stats(...);   // populates atlas_tier_doctor
}
```

The blit budget passed to the deferrer comes from
`atlas_blit_budget_calculator.py`'s output for the deployment's
hardware profile. The runbook recommends recomputing on driver
upgrade or hardware change.

## Pressure response

`ft doctor`'s `atlas_tier_swap` section maps each TierSwapStats
sample to a status bucket via
[`atlas_tier_doctor.rs::TierSwapDoctorRow::status`][status]. The
operator workflow when a row turns Warn or Fail:

[status]: ../../crates/frankenterm-core/src/atlas_tier_doctor.rs

| Status | Trigger | Operator action |
|--------|---------|-----------------|
| Warn   | pressure_pct > 75 (either tier) OR > 64 swap-out events | Increase host RAM budget if available; raise blit budget for one frame; investigate fragmentation. |
| Warn   | swap-out > 64 events | Probable demote loop. Check if the eviction policy is misconfigured (oversized atlas vs available VRAM). |
| Fail   | any disk_eviction_count > 0 | Cache lost a region — either raise the host RAM budget or accept the redraw cost. The tiered-swap policy already prefers HostRam over Disk on bandwidth-starved buses. |
| Fail   | pressure_pct > 95 (either tier) | Imminent OOM. Scale atlas tile size down or reduce the active glyph corpus. |

The thresholds live in
`atlas_tier_doctor::TierSwapDoctorRow::status`'s constants and
can be tuned per deployment. The status enum is consumed by the
CLI translator at `crates/frankenterm/src/main.rs::Commands::Doctor`.

## Bandwidth-starved deployments

The blit budget calculator emits a contextual warning when the
bus class is `legacy`:

> legacy bus (PCIe 2.0 / shared) is bandwidth-starved; the
> tiered-swap policy should prefer the Disk tier over HostRam
> evictions when the queue starts to back up.

Translated to operator action: when running on bandwidth-starved
hardware, configure the eviction selector to skip the HostRam
tier on demote (drop straight to Disk). The substrate's
`select_eviction_target` honors a per-tier veto; the wired-pass
configuration plumbing for this is part of ft-ktd19.3's wired-pass
slice.

## Verification

Per-frame:
1. `staging_queue.queue_len() <= warning_threshold` — runaway
   accumulation is the leading indicator of insufficient blit
   budget.
2. `swap_deferrer.budget_remaining_bytes() >= 0` — invariant
   from the substrate's saturating math; if it ever underflows
   the deferrer is misconfigured.
3. `atlas_tier_doctor::TierSwapDoctorReport::aggregate.total_disk_eviction_count`
   stays 0 in healthy operation.

Per-deployment:
1. `cargo test -p frankenterm-core --lib --no-default-features atlas_tier_doctor`
   — confirms the threshold buckets behave per the playbook.
2. `cargo test -p frankenterm-core --lib --no-default-features atlas_tiered_swap`
   — confirms the substrate's eviction-decision tree.
3. `python3 scripts/atlas_blit_budget_calculator.py --bus <profile>`
   — recompute the blit cap on hardware upgrade; retain the JSON
   in the deployment runbook.

## Cross-references

- [`crates/frankenterm-core/src/atlas_tiered_swap.rs`][substrate] —
  AtlasTier / EvictionAction / TierSwapStats /
  StagingTransferQueue / FrameBudgetSwapDeferrer.
- [`crates/frankenterm-core/src/atlas_tier_doctor.rs`][doctor] —
  doctor surface (ft-ktd19.2).
- [`docs/render/atlas-tiered-swap-wgpu-integration.md`][wgpu] —
  wgpu copy-command emission runbook (cc_1).
- [`scripts/atlas_blit_budget_calculator.py`][calc] — per-frame
  blit budget calculator (this slice + ft-ktd19.1 cont).
- ft-ktd19 (parent epic).
- ft-ktd19.1 (GPU blit integration; substrate at dbc8a146c).
- ft-ktd19.2 (memory probes + doctor telemetry; substrate at fa1edc458).
- ft-ktd19.3 (disk handoff + frame-budget deferrer; substrate at bdd118ed8).
