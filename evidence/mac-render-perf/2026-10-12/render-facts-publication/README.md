# Render-facts publication cost (ft-yccm0.2.2.1 AC4, native)

**Bench.** `cargo bench -p mux --features bench-hooks --bench render_facts_publication` on mac-studio
(M5 Ultra, macOS 27). It runs paired ABBA rounds of publish against skip on the M.1 T0 corpus
(color_emoji_random) and prints a verdict: pass below 1%, or NO_ADMISSIBLE_RATIO above 5% CV.

**Source.**
- The bench's state check is the 32d718d48 version, which compares content and not sequence
  numbers. The old check failed at 32 MiB on timing-dependent seqnos.
- The publication code is e98affd06's: unchanged publications are skipped, and the facts
  allocation is refilled in place.

| file | corpus | pairs | load |
|------|--------|-------|------|
| `bench-4MiB-default.txt` | 4 MiB (default) | 10 | about 20 |
| `bench-32MiB-20pairs.txt` | 32 MiB | 20 | 21-27, fresh after a host reboot |

**Result, 32 MiB run:**

| path | batch | publish / skip median | overhead | paired median | CV publish / skip | verdict |
|------|-------|-----------------------|----------|---------------|-------------------|---------|
| actions | 4 KiB | 125.97 / 124.90 ms | 0.86% | 0.77% | 4.6 / 3.2% | **pass** |
| actions | 128 KiB | 120.38 / 119.95 ms | 0.36% | 0.41% | 2.3 / 2.6% | **pass** |
| fused | 4 KiB | 119.40 / 125.03 ms | -4.5% | 0.75% | 14.1 / 13.6% | no admissible ratio |
| fused | 128 KiB | 111.08 / 111.87 ms | -0.7% | -1.14% | 10.5 / 10.2% | no admissible ratio |

- On 2026-10-06 the action path measured +3.4% at 4 KiB, which kept the bead open. After
  e98affd06 it passes under 1% at both batch sizes.
- The fused path is the live default. Its runs swing too much here for a verdict, but its
  paired medians are +0.75% and -1.14%.
- The orchestrator's native quiet-window run on 2026-10-06 also found no measurable fused cost:
  59.63 vs 60.00 ms and 57.83 vs 58.42 ms.
- At 4 MiB every arm was inadmissible, because 13-16 ms runs have CV above 5%.
