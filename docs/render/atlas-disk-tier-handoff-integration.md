# Atlas disk-tier handoff — cold-tier pipeline integration

**Bead:** `ft-ktd19.3` (ft-ktd19.followup).
**In-core substrate:** `crates/frankenterm-core/src/atlas_tiered_swap.rs`
+ `crates/frankenterm-core/src/cold_tier_pipeline.rs` +
`crates/frankenterm-core/src/cold_tier_pipeline_driver.rs`.

This doc captures the wired-pass handoff shape between the
atlas tiered-swap host-RAM staging buffer and the existing
cold-tier pipeline (write/retry driver) per the bead's spec
("Integrate Disk-tier handoff with the cold-disk eviction
infrastructure").

## Substrate-pass commit table

| Slice | Commit | What ships |
|-------|--------|-----------|
| ft-ktd19.3 slice 1 | `bdd118ed8` | `FrameBudgetSwapDeferrer` + `SwapDeferralOutcome` (8 tests) |
| ft-ktd19.3 slice 2 | `5dba8440b` | `DiskTierHandoff` + `DiskHandoffDirection` + `DiskTierHandoffQueue` (6 tests) |
| ft-ktd19.1 slice 1 | `dbc8a146c` | `StagingTransferEvent` + `StagingTransferQueue` (7 tests) |

## Wired-pass integration shape

The atlas tiered-swap integration's stage_region wrapper:

```rust
use frankenterm_core::atlas_tiered_swap::{
    AtlasStagingTransferDriver, DiskHandoffDirection,
    DiskTierHandoff, DiskTierHandoffQueue,
    HostRamStagingError,
};

struct AtlasContext {
    driver: AtlasStagingTransferDriver, // host-RAM staging
    disk_queue: DiskTierHandoffQueue,
    cold_lru: ColdRegionLruRegistry,    // existing tier registry
}

impl AtlasContext {
    fn stage_with_disk_fallback(
        &mut self,
        region_id: u64,
        bytes: u64,
        frame_id: u64,
    ) -> Result<HostRamStagingAllocation, AtlasError> {
        // First attempt: stage into host-RAM.
        match self.driver.stage_region(region_id, bytes, frame_id) {
            Ok(allocation) => Ok(allocation),
            Err(HostRamStagingError::OutOfSpace { .. }) => {
                // Pick the LRU cold region from the existing
                // tier registry. The cold region's bytes are
                // demoted to disk to free staging space.
                let evict_region = self.cold_lru
                    .lru_region()
                    .ok_or(AtlasError::NoEvictionCandidate)?;
                let evict_bytes = self.cold_lru
                    .bytes_for(evict_region)
                    .unwrap_or(0);

                // Push the disk-tier handoff. The cold-tier
                // driver subscriber drains this queue per batch.
                self.disk_queue.push(DiskTierHandoff {
                    region_id: evict_region,
                    direction: DiskHandoffDirection::Demote,
                    bytes: evict_bytes,
                    frame_id,
                });

                // Release the cold region from staging (the
                // cold-tier driver has captured its bytes via
                // the handoff). Then retry the original request.
                self.driver.release_region(evict_region, frame_id)?;
                self.driver.stage_region(region_id, bytes, frame_id)
            }
            Err(other) => Err(other.into()),
        }
    }
}
```

## Cold-tier driver subscriber

The cold-tier driver consumes the disk-handoff queue per batch:

```rust
use frankenterm_core::atlas_tiered_swap::{
    DiskHandoffDirection, DiskTierHandoffQueue,
};
use frankenterm_core::cold_tier_pipeline::WritePipelineStep;
use frankenterm_core::cold_tier_pipeline_driver::evaluate_driver_tick;

fn drain_disk_handoffs_per_frame(
    queue: &mut DiskTierHandoffQueue,
    cold_writer: &mut ColdTierWriter,
    cold_reader: &mut ColdTierReader,
) {
    // Batch all writes (Demote) before reads (Promote) so the
    // disk head moves in one direction.
    let demotes = queue.by_direction(DiskHandoffDirection::Demote);
    let promotes = queue.by_direction(DiskHandoffDirection::Promote);
    queue.drain_pending(); // both directions consumed; reset

    for demote in demotes {
        // Translate Demote → cold-tier WritePipelineStep.
        // The existing cold_tier_pipeline.rs handles the
        // compression / encryption / retry policy.
        cold_writer.enqueue_write(WritePipelineStep::WriteChunk {
            region_id: demote.region_id,
            bytes: demote.bytes,
            // ... + the staging-buffer offset the cold-tier
            // reads from (passed by the integration via a
            // staging-buffer-loan mechanism).
        });
    }

    for promote in promotes {
        // Translate Promote → cold-tier read request.
        cold_reader.enqueue_read(promote.region_id, promote.bytes);
    }
}
```

## Frame-budget compliance

Combine the [DiskTierHandoffQueue] with [FrameBudgetSwapDeferrer]
from slice 1: disk handoffs are CHEAPER than VRAM↔staging blits
in upload time but MORE EXPENSIVE in disk I/O latency. The
recommended budget split per frame:

```rust
// 60fps target: 16.67ms total.
const TOTAL_FRAME_US: u64 = 16_667;
const VRAM_BLIT_BUDGET_US: u64 = 3_333;       // 20% for VRAM blits
const DISK_HANDOFF_BUDGET_US: u64 = 1_667;     // 10% for disk handoffs
const RENDER_BUDGET_US: u64 = TOTAL_FRAME_US
    - VRAM_BLIT_BUDGET_US
    - DISK_HANDOFF_BUDGET_US;                  // 70% for actual rendering
```

Disk handoffs that exceed `DISK_HANDOFF_BUDGET_US` defer to
the next frame via the queue's natural carry-over (the cold-
tier driver only drains as much as it can dispatch within
budget; remainder stays queued).

## Disk I/O budgeting

Disk write throughput model (operator-tunable):

| Storage class | Throughput |
|---------------|-----------|
| NVMe SSD (default) | ~3 GB/s ≈ 3_000 B/µs |
| SATA SSD | ~500 MB/s ≈ 500 B/µs |
| HDD (legacy) | ~150 MB/s ≈ 150 B/µs |

The bead's wired-pass adds a `DiskBudgetEstimator` (sibling
to `FrameBudgetSwapDeferrer`) that tracks how many bytes the
cold-tier writer can dispatch in the current frame's
`DISK_HANDOFF_BUDGET_US`. Exceeded handoffs re-queue.

## Cross-references

- Substrate: [`crates/frankenterm-core/src/atlas_tiered_swap.rs`](../../crates/frankenterm-core/src/atlas_tiered_swap.rs) (DiskTierHandoffQueue + FrameBudgetSwapDeferrer + StagingTransferQueue + AtlasStagingTransferDriver)
- Cold-tier pipeline: [`crates/frankenterm-core/src/cold_tier_pipeline.rs`](../../crates/frankenterm-core/src/cold_tier_pipeline.rs)
- Cold-tier driver: [`crates/frankenterm-core/src/cold_tier_pipeline_driver.rs`](../../crates/frankenterm-core/src/cold_tier_pipeline_driver.rs)
- Sibling runbook: [`atlas-tiered-swap-wgpu-integration.md`](atlas-tiered-swap-wgpu-integration.md) — the VRAM↔staging side
- Beads: ft-ktd19.3 (parent), ft-ktd19.1 (StagingTransferQueue at dbc8a146c)
