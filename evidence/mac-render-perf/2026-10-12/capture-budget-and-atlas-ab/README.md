# Paint capture budget and first atlas size: native T0 GUI A/Bs (ft-yccm0.2.2.3, ft-yccm0.2.6)

**Build.** `frankenterm-gui` was built with `--profile release-perf` and frame pointers, from
a4d155502's code: 0381b72f5 plus the four files of the M.4 ledger fix. With that fix, the
main-thread counters include bounded waits that gave up.

**Setup** (mac-studio, M5 Ultra, macOS 27; 60 Hz DELL U3224KB):
- Harness: `scripts/mac-gui-throughput.sh --baseline ft --reps 3 --no-fps --no-beachball-probe`.
  SSH has no Screen Recording grant, so there is no FPS meter.
- T0 corpus: `~/color-emoji-random.bin`, 749,801,000 bytes.
- Arms interleave ABBA. Load at the go line was 37-102; each run's load is in the receipts.
- `measure-seq3.sh` is the driver.
- Every figure below comes from the run's last GUI resource snapshot (`ft_snapshot`): the
  `terminal_locks` and `frames` sections.

## A/B 1: paint capture budget (`capture-receipt.json.gz`)

The arms differ only in `RENDER_CAPTURE_WAIT`:
- arm `ft`: the default, 4 ms (ft-5ca68);
- arm `ft_baseline`: `FRANKENTERM_RENDER_CAPTURE_WAIT_MS=1`.

| run | arm | main thread blocked | waits | longest wait | stale presents / presents |
|-----|-----|---------------------|-------|--------------|---------------------------|
| 0 | 4 ms | 51.9 ms | 220 | 13.98 ms | 2 / 994 |
| 3 | 4 ms | 22.4 ms | 301 | 3.51 ms | 2 / 1046 |
| 4 | 4 ms | 29.4 ms | 268 | 10.01 ms | 2 / 892 |
| 1 | 1 ms | 12.0 ms | 271 | 1.27 ms | 6 / 967 |
| 2 | 1 ms | 15.8 ms | 260 | 1.52 ms | 5 / 1064 |
| 5 | 1 ms | 10.2 ms | 220 | 1.52 ms | 3 / 985 |

- With 1 ms, the longest main-thread wait stays at about 1.5 ms, scheduling included. The 4 ms
  budget let single waits reach 10-14 ms at this load.
- Total blocking falls by about half or more.
- The cost is a few more stale presents, frames that reused a pane's last frame: 3-6 instead of
  2 per roughly 1000. Freshness is essentially unchanged. The demand handoff that ft-5ca68 added
  delivers the lock well inside 1 ms almost every time.
- Drain time has no verdict (CV 26.7% / 5.9% at this load).
- Parser hold p99 is the (1.84, 2.10] ms bucket in 5 of 6 runs, and the (2.10, 2.62] ms bucket
  in run 0.

## A/B 2: first atlas size (`atlas-receipt.json.gz`)

The arms differ only in the first atlas size:
- arm `ft`: 1024 px, the default since 2bf25a229;
- arm `ft_baseline`: `config.glyph_atlas_initial_size = 128`, the old start that grows
  128 -> 256 -> 512 -> 1024.

| run | arm | paint passes | painted frames | extra passes | retried frames |
|-----|-----|--------------|----------------|--------------|----------------|
| 0 | 1024 | 861 | 856 | 5 | 3 |
| 3 | 1024 | 1001 | 997 | 4 | 2 |
| 4 | 1024 | 786 | 782 | 4 | 2 |
| 1 | 128 | 1218 | 1209 | 9 | 2 |
| 2 | 128 | 1049 | 1041 | 8 | 2 |
| 5 | 128 | 811 | 803 | 8 | 2 |

- The 1024 px start removes about 4 paint passes per session: the startup growth cascade.
- After startup, retries are the same 2-3 per run in both arms.
- The clear-in-place half of 2bf25a229 has no switch. It is covered by the gui bin renderstate
  tests.
