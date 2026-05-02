# Atlas tiered-swap → wgpu integration

**Bead:** `ft-ktd19` (BR-TERM-EMULATOR-UPLIFT-2.11.cont) +
sub-beads `ft-ktd19.1` (GPU blit) and `ft-ktd19.3` (disk
handoff + frame-budget swap deferral).
**Substrate:** `crates/frankenterm-core/src/atlas_tiered_swap.rs`.

This doc captures the wired-pass handoff shape for the renderer
team: how the in-core substrate (host-RAM staging buffer +
transfer-event queue + frame-budget deferrer) plugs into the
wgpu copy-command emitter without coupling the substrate to
wgpu types.

## Substrate surface (shipped under ft-ktd19.1 + ft-ktd19.3)

```text
┌──────────────────────────────────────────────────────────┐
│  HostRamStagingBuffer (existing, from ft-ktd19 36bf8...) │
│    .stage_region(id, bytes) -> HostRamStagingAllocation  │
│    .release_region(id) -> HostRamStagingAllocation       │
└──────────────────────────────────────────────────────────┘
                       │
                       ▼ (integration wraps each call)
┌──────────────────────────────────────────────────────────┐
│  StagingTransferQueue (br-ft-ktd19.1 dbc8a146c)          │
│    .push(StagingTransferEvent {                          │
│       region_id, direction, allocation, frame_id })      │
│    .drain_pending() -> Vec<StagingTransferEvent>         │
│    .pending_bytes() -> u64                               │
└──────────────────────────────────────────────────────────┘
                       │
                       ▼ (per-frame drain)
┌──────────────────────────────────────────────────────────┐
│  FrameBudgetSwapDeferrer (br-ft-ktd19.3 bdd118ed8)       │
│    .partition(&[events], frame_budget_us)                │
│      -> (admitted, deferred)                             │
│    .reset_for_new_frame()                                │
└──────────────────────────────────────────────────────────┘
                       │
                       ▼ (admitted events drive wgpu copies)
┌──────────────────────────────────────────────────────────┐
│  wgpu copy-command emitter (WIRED-PASS — sub-bead scope) │
│    Demote: VRAM texture → staging buffer (download)      │
│    Promote: staging buffer → VRAM texture (upload)       │
└──────────────────────────────────────────────────────────┘
                       │
                       ▼ (after queue submit)
              re-enqueue deferred for next frame
              + reset_for_new_frame()
```

## Per-frame integration loop

```rust
use frankenterm_core::atlas_tiered_swap::{
    FrameBudgetSwapDeferrer, StagingTransferDirection,
    StagingTransferQueue,
};

struct AtlasSwapContext {
    queue: StagingTransferQueue,
    deferrer: FrameBudgetSwapDeferrer,
    deferred_carryover: Vec<StagingTransferEvent>,
}

impl AtlasSwapContext {
    fn dispatch_per_frame(
        &mut self,
        wgpu_encoder: &mut wgpu::CommandEncoder,
        frame_budget_us: u64,
    ) {
        // Drain everything queued this frame + carry-over from
        // last frame's deferred set.
        let mut events = std::mem::take(&mut self.deferred_carryover);
        events.extend(self.queue.drain_pending());

        // Gate on frame budget.
        let (admitted, deferred) = self.deferrer.partition(
            &events,
            frame_budget_us,
        );
        self.deferred_carryover = deferred;

        // Emit wgpu copy commands for admitted events.
        for event in admitted {
            match event.direction {
                StagingTransferDirection::Demote => {
                    // VRAM texture → staging buffer.
                    // wgpu_encoder.copy_texture_to_buffer(...)
                    // sized by event.bytes(), offset by
                    // event.offset() within the staging buffer.
                }
                StagingTransferDirection::Promote => {
                    // Staging buffer → VRAM texture.
                    // wgpu_encoder.copy_buffer_to_texture(...)
                    // sourced from event.offset(), event.bytes().
                }
            }
        }

        // After encoding all admitted copies, reset the
        // deferrer's per-frame accumulator. The next frame
        // starts with full budget + the carryover set from
        // self.deferred_carryover.
        self.deferrer.reset_for_new_frame();
    }
}
```

## Throughput tuning (FrameBudgetSwapDeferrer)

The default throughput model is `12_000` bytes per microsecond
(12 GB/s ≈ PCIe 4.0 x16). Override per platform / GPU class:

| Platform | Throughput estimate | Configure |
|----------|--------------------|-----------|
| Apple Silicon UMA (no PCIe transfer) | 50 GB/s ≈ 50_000 B/µs | `FrameBudgetSwapDeferrer::with_throughput(50_000)` |
| PCIe 4.0 x16 (default) | 12 GB/s ≈ 12_000 B/µs | `FrameBudgetSwapDeferrer::new()` |
| PCIe 3.0 x16 | 6 GB/s ≈ 6_000 B/µs | `FrameBudgetSwapDeferrer::with_throughput(6_000)` |
| PCIe 2.0 x8 (legacy) | 2 GB/s ≈ 2_000 B/µs | `FrameBudgetSwapDeferrer::with_throughput(2_000)` |

The bead's spec calls for the wired-pass to probe the platform's
real PCIe class and tune at startup; until then, the integration
defaults to PCIe 4.0 (the modal hardware in 2026).

## Frame-budget recommendations

Frame budget is `(target_frame_time_us − current_render_us)`.
For a 60 fps target the total frame time is 16_667 µs; reserving
20% (3_333 µs) for swap dispatch is the bead's recommendation.

```rust
const TARGET_FRAME_TIME_US: u64 = 16_667;
const SWAP_BUDGET_FRACTION: u64 = 5; // 1/5 = 20%
let swap_budget_us = TARGET_FRAME_TIME_US / SWAP_BUDGET_FRACTION;
```

If the renderer's main path runs hot (>13 ms), the integration
can shrink the swap budget to 0 — `FrameBudgetSwapDeferrer`
defers everything and the carry-over set grows; next-frame
recovery is automatic when the main path returns to normal.

## Disk-tier handoff (ft-ktd19.3 wired-pass)

The bead's "disk handoff" item integrates with the existing
cold-disk eviction infrastructure (cross-link
`crates/frankenterm-core/src/cold_tier_pipeline.rs`). The
wired-pass shape:

1. `HostRamStagingBuffer` overflow → drop to `Disk` tier via
   the cold-tier pipeline's existing `cold_persist` path.
2. Promote-from-disk on next access → read into staging →
   `StagingTransferQueue::push(Promote)` → frame-budget gate
   → wgpu upload.

The substrate already exposes the `AtlasTier::Disk` variant +
`should_evict_from(BudgetPressure::Critical, has_cold_regions)`
gate. Wired-pass: glue the cold-tier read/write paths to
`stage_region` / `release_region` such that the
`StagingTransferQueue` events fire transparently regardless of
which tier the region was last in.

## Cross-references

- Substrate: [`crates/frankenterm-core/src/atlas_tiered_swap.rs`](../../crates/frankenterm-core/src/atlas_tiered_swap.rs)
- ft-ktd19.1 substrate-pass: `dbc8a146c` (StagingTransferQueue + events + 7 tests)
- ft-ktd19.3 substrate-pass: `bdd118ed8` (FrameBudgetSwapDeferrer + 8 tests)
- ft-ktd19 parent (host-RAM staging substrate at `36bf8...` shipped earlier)
- Cold-tier pipeline: [`crates/frankenterm-core/src/cold_tier_pipeline.rs`](../../crates/frankenterm-core/src/cold_tier_pipeline.rs)
