# Atlas blit budget — canonical hardware profile examples

Reference outputs from
[`scripts/atlas_blit_budget_calculator.py`][calc] (ft-ktd19.1) for
the four bus classes the calculator supports. Operators retain the
JSON for their deployment as part of the renderer's config
runbook; this page documents the canonical 60fps / 256 KiB-atlas
profile so the per-deployment delta is easy to spot.

[calc]: ../../scripts/atlas_blit_budget_calculator.py

## Profile A — Apple Silicon / AMD APU (UMA, 200 GB/s)

```sh
python3 scripts/atlas_blit_budget_calculator.py --bus uma
```

```json
{
  "atlas_blits_per_frame": 1178,
  "bead": "ft-ktd19.1",
  "blit_budget_bytes_per_frame": 2666666666,
  "blit_budget_ms": 13.3333,
  "frame_budget_ms": 16.6667,
  "headroom_ms": 3.3333,
  "headroom_pct": 20.0,
  "inputs": {
    "atlas_size_bytes": 262144,
    "atlas_size_kib": 256,
    "bus": "uma",
    "bus_throughput_gb_s": 200.0,
    "dispatch_overhead_us": 10,
    "frame_reserve_pct": 20,
    "target_fps": 60
  },
  "notes": [
    "more than 256 atlas blits per frame — driver command-queue depth may be the real ceiling. Consider capping the blit loop empirically rather than by computed budget."
  ],
  "schema_version": 1
}
```

**Read:** UMA is bandwidth-rich; the calculator's "more than 256
blits per frame" note is the operator signal that the driver
command-queue depth is the real ceiling, not bus throughput.
**Operator action:** start with a 256-blit cap empirically; raise
in 64-blit increments while watching for frame-pacing jitter.

## Profile B — PCIe 4.0 x16 discrete GPU (32 GB/s)

```sh
python3 scripts/atlas_blit_budget_calculator.py --bus pcie4
```

```json
{
  "atlas_blits_per_frame": 229,
  "bead": "ft-ktd19.1",
  "blit_budget_bytes_per_frame": 426666666,
  "blit_budget_ms": 13.3333,
  "frame_budget_ms": 16.6667,
  "headroom_ms": 3.3333,
  "headroom_pct": 20.0,
  "inputs": {
    "atlas_size_bytes": 262144,
    "atlas_size_kib": 256,
    "bus": "pcie4",
    "bus_throughput_gb_s": 32.0,
    "dispatch_overhead_us": 50,
    "frame_reserve_pct": 20,
    "target_fps": 60
  },
  "notes": [],
  "schema_version": 1
}
```

**Read:** the most common discrete-GPU profile. ~229 blits/frame
fits comfortably under the 256-blit driver-queue heuristic, no
notes. **Operator action:** use the computed cap directly; no
empirical tuning needed in steady state.

## Profile C — PCIe 3.0 x16 discrete GPU (16 GB/s)

```sh
python3 scripts/atlas_blit_budget_calculator.py --bus pcie3
```

```json
{
  "atlas_blits_per_frame": 174,
  "bead": "ft-ktd19.1",
  "blit_budget_bytes_per_frame": 213333333,
  "blit_budget_ms": 13.3333,
  "frame_budget_ms": 16.6667,
  "headroom_ms": 3.3333,
  "headroom_pct": 20.0,
  "inputs": {
    "atlas_size_bytes": 262144,
    "atlas_size_kib": 256,
    "bus": "pcie3",
    "bus_throughput_gb_s": 16.0,
    "dispatch_overhead_us": 60,
    "frame_reserve_pct": 20,
    "target_fps": 60
  },
  "notes": [],
  "schema_version": 1
}
```

**Read:** half the bandwidth of pcie4 + 20% higher dispatch
overhead. Still comfortable at 174 blits/frame on the canonical
atlas. **Operator action:** consider lowering atlas tile size to
128 KiB if the GUI workload generates more than ~150 blits/frame
in burst peaks; that doubles the cap to ~340.

## Profile D — Legacy PCIe 2.0 / shared bus (8 GB/s)

```sh
python3 scripts/atlas_blit_budget_calculator.py --bus legacy
```

```json
{
  "atlas_blits_per_frame": 118,
  "bead": "ft-ktd19.1",
  "blit_budget_bytes_per_frame": 106666666,
  "blit_budget_ms": 13.3333,
  "frame_budget_ms": 16.6667,
  "headroom_ms": 3.3333,
  "headroom_pct": 20.0,
  "inputs": {
    "atlas_size_bytes": 262144,
    "atlas_size_kib": 256,
    "bus": "legacy",
    "bus_throughput_gb_s": 8.0,
    "dispatch_overhead_us": 80,
    "frame_reserve_pct": 20,
    "target_fps": 60
  },
  "notes": [
    "legacy bus (PCIe 2.0 / shared) is bandwidth-starved; the tiered-swap policy should prefer the Disk tier over HostRam evictions when the queue starts to back up."
  ],
  "schema_version": 1
}
```

**Read:** bandwidth-starved; the calculator's note is the operator
signal. **Operator action:** configure the eviction selector to
skip the HostRam tier on demote (drop straight to Disk), per the
operator playbook. Consider lowering target_fps to 30 if the
calculated cap (118 blits/frame) is still insufficient — that
doubles the per-frame headroom to ~33ms blit budget.

## High-refresh-rate variants (120fps / 144fps)

For 120fps the budget halves; the operator playbook recommends
either lowering atlas tile size (256 → 128 KiB) or switching to
the UMA-class tuning where the cap floats. Sample for UMA + 120fps
+ 256 KiB:

```sh
python3 scripts/atlas_blit_budget_calculator.py --bus uma --target-fps 120
```

The cap drops from 1178 to ~589 blits/frame — still well above
the driver-queue heuristic. The calculator's > 256 note still
fires; same advice applies (cap empirically).

## Cross-references

- [`scripts/atlas_blit_budget_calculator.py`][calc] — calculator
  source; all constants live there.
- [`docs/render/atlas-tiered-swap-operator-playbook.md`][play] —
  the broader operator workflow this fits into (per-frame loop +
  pressure response table + bandwidth-starved deployments).
- [`docs/render/atlas-tiered-swap-wgpu-integration.md`][wgpu] —
  cc_1's wgpu copy-command-emission runbook the cap feeds into.
- [`crates/frankenterm-core/src/atlas_tier_doctor.rs`][doctor] —
  doctor surface that reports the runtime efficiency of the
  selected cap.

[play]: ./atlas-tiered-swap-operator-playbook.md
[wgpu]: ./atlas-tiered-swap-wgpu-integration.md
[doctor]: ../../crates/frankenterm-core/src/atlas_tier_doctor.rs
