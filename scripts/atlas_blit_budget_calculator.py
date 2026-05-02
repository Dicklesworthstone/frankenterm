#!/usr/bin/env python3
"""Atlas tiered-swap frame-budget calculator (br-ft-ktd19.1 aid).

Operator-facing tool that picks a safe per-frame blit cap given
target frame rate + memory bus throughput. Pairs with
docs/render/atlas-tiered-swap-wgpu-integration.md (cc_1's runbook
at d4701405d) which lists throughput tuning constants but leaves
the per-deployment math as an operator step.

The renderer engineer who wires the wgpu integration calls
``StagingTransferQueue::drain_pending`` once per frame, partitions
the resulting events by direction, and emits VRAM↔staging copy
commands until the frame's blit budget is exhausted. Pick that
budget too low and the queue grows unbounded under tier pressure;
too high and a single frame stalls into the next vsync. This
tool computes the budget from first principles so the per-
deployment configuration is a calculator output rather than a
guess.

## Inputs

- target_fps (default 60) — vsync rate the renderer must hit.
- frame_reserve_pct (default 20) — % of the frame budget reserved
  for non-blit work (rasterize, layout, present). Blit gets the
  remainder.
- bus_throughput_gb_s — bus-class throughput. Choose from:
    uma     — Apple Silicon / AMD APU UMA pool (~200-400 GB/s
              real, but bottlenecked at the cache; we use 200).
    pcie4   — PCIe 4.0 x16 discrete GPU (~32 GB/s).
    pcie3   — PCIe 3.0 x16 discrete GPU (~16 GB/s).
    legacy  — PCIe 2.0 / shared-bus (~8 GB/s).
- atlas_size_kib — atlas tile size. Default 256 KiB (~512x512x4B).

## Output

::

    {
        "bead": "ft-ktd19.1",
        "schema_version": 1,
        "inputs": { ... },
        "frame_budget_ms": <float>,
        "blit_budget_ms": <float>,
        "blit_budget_bytes_per_frame": <int>,
        "atlas_blits_per_frame": <int>,
        "headroom_pct": <float>,
        "notes": ["..."]
    }

## Usage

::

    python3 scripts/atlas_blit_budget_calculator.py \
        --bus uma                # Apple Silicon
    # → 16.66ms frame, 13.33ms blit, ~52 atlas tiles per frame

    python3 scripts/atlas_blit_budget_calculator.py \
        --target-fps 120 \
        --bus pcie4 \
        --atlas-size-kib 512
"""

from __future__ import annotations

import argparse
import json
import sys

SCHEMA_VERSION = 1

# bus_label: throughput in bytes/sec
BUS_THROUGHPUT_BYTES_PER_SEC: dict[str, int] = {
    "uma":    int(200e9),
    "pcie4":  int(32e9),
    "pcie3":  int(16e9),
    "legacy": int(8e9),
}

DEFAULT_BUS = "pcie4"
DEFAULT_TARGET_FPS = 60
DEFAULT_FRAME_RESERVE_PCT = 20
DEFAULT_ATLAS_SIZE_KIB = 256

# Blit overhead — wgpu command-buffer dispatch + GPU pipeline-flush
# dominate at low byte counts. The runbook calls out
# ~50us minimum dispatch on PCIe 4 and ~10us on UMA. Keep the
# constants conservative so the calculator does not over-promise.
DISPATCH_OVERHEAD_US: dict[str, int] = {
    "uma":    10,
    "pcie4":  50,
    "pcie3":  60,
    "legacy": 80,
}


def calculate(
    target_fps: int,
    frame_reserve_pct: int,
    bus: str,
    atlas_size_kib: int,
) -> dict:
    notes: list[str] = []
    if bus not in BUS_THROUGHPUT_BYTES_PER_SEC:
        raise ValueError(
            f"unknown bus class `{bus}`; expected one of "
            f"{sorted(BUS_THROUGHPUT_BYTES_PER_SEC)}"
        )
    if target_fps <= 0:
        raise ValueError("target_fps must be > 0")
    if not 0 <= frame_reserve_pct < 100:
        raise ValueError("frame_reserve_pct must be in [0, 100)")
    if atlas_size_kib <= 0:
        raise ValueError("atlas_size_kib must be > 0")

    bus_bytes_per_sec = BUS_THROUGHPUT_BYTES_PER_SEC[bus]
    dispatch_us = DISPATCH_OVERHEAD_US[bus]

    frame_budget_ms = 1000.0 / target_fps
    blit_budget_ms = frame_budget_ms * (1.0 - frame_reserve_pct / 100.0)
    blit_budget_bytes = int(bus_bytes_per_sec * (blit_budget_ms / 1000.0))

    atlas_size_bytes = atlas_size_kib * 1024
    transfer_us_per_blit = (
        atlas_size_bytes / bus_bytes_per_sec * 1_000_000.0
    ) + dispatch_us
    atlas_blits_per_frame = int(
        (blit_budget_ms * 1000.0) / max(transfer_us_per_blit, 1.0)
    )

    headroom_ms = frame_budget_ms - blit_budget_ms
    headroom_pct = (headroom_ms / frame_budget_ms) * 100.0

    if atlas_blits_per_frame < 4:
        notes.append(
            "fewer than 4 atlas blits per frame — bus is the bottleneck. "
            "Consider raising frame_reserve_pct, lowering atlas_size_kib, "
            "or batching demote/promote across multiple frames."
        )
    if atlas_blits_per_frame > 256:
        notes.append(
            "more than 256 atlas blits per frame — driver command-queue "
            "depth may be the real ceiling. Consider capping the blit "
            "loop empirically rather than by computed budget."
        )
    if frame_reserve_pct < 10:
        notes.append(
            "frame_reserve_pct under 10% leaves no slack for shading / "
            "layout / present overruns. The runbook recommends 20%."
        )
    if bus == "legacy":
        notes.append(
            "legacy bus (PCIe 2.0 / shared) is bandwidth-starved; the "
            "tiered-swap policy should prefer the Disk tier over HostRam "
            "evictions when the queue starts to back up."
        )

    return {
        "bead": "ft-ktd19.1",
        "schema_version": SCHEMA_VERSION,
        "inputs": {
            "target_fps": target_fps,
            "frame_reserve_pct": frame_reserve_pct,
            "bus": bus,
            "bus_throughput_gb_s": bus_bytes_per_sec / 1e9,
            "atlas_size_kib": atlas_size_kib,
            "atlas_size_bytes": atlas_size_bytes,
            "dispatch_overhead_us": dispatch_us,
        },
        "frame_budget_ms": round(frame_budget_ms, 4),
        "blit_budget_ms": round(blit_budget_ms, 4),
        "blit_budget_bytes_per_frame": blit_budget_bytes,
        "atlas_blits_per_frame": atlas_blits_per_frame,
        "headroom_ms": round(headroom_ms, 4),
        "headroom_pct": round(headroom_pct, 4),
        "notes": notes,
    }


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="atlas_blit_budget_calculator",
        description=(
            "Pick a per-frame blit cap for the tiered-swap drain loop. "
            "Pairs with docs/render/atlas-tiered-swap-wgpu-integration.md."
        ),
    )
    parser.add_argument(
        "--target-fps",
        type=int,
        default=DEFAULT_TARGET_FPS,
        help=f"Target vsync rate (default: {DEFAULT_TARGET_FPS}).",
    )
    parser.add_argument(
        "--frame-reserve-pct",
        type=int,
        default=DEFAULT_FRAME_RESERVE_PCT,
        help=(
            "Percent of the frame budget reserved for non-blit work "
            f"(default: {DEFAULT_FRAME_RESERVE_PCT})."
        ),
    )
    parser.add_argument(
        "--bus",
        choices=sorted(BUS_THROUGHPUT_BYTES_PER_SEC),
        default=DEFAULT_BUS,
        help=f"Memory bus class (default: {DEFAULT_BUS}).",
    )
    parser.add_argument(
        "--atlas-size-kib",
        type=int,
        default=DEFAULT_ATLAS_SIZE_KIB,
        help=f"Atlas tile size in KiB (default: {DEFAULT_ATLAS_SIZE_KIB}).",
    )
    args = parser.parse_args(argv)

    try:
        result = calculate(
            target_fps=args.target_fps,
            frame_reserve_pct=args.frame_reserve_pct,
            bus=args.bus,
            atlas_size_kib=args.atlas_size_kib,
        )
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
