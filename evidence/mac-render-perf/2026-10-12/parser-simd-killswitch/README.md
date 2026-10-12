# Parser SIMD kill-switch A/B on Apple Silicon (ft-yccm0.3.2.2, ft-yccm0.3.2.3)

**Binary.** One build serves every arm: `ingest_throughput` at 15a2b1c46, `--profile release-perf`,
`--lane term`, built natively on mac-studio (M5 Ultra, macOS 27).

**Arms.**
- `FT_PARSER_SIMD=1` (default, Simd16) against `=0` (the scalar oracle).
- `FT_PARSER_SIMD_UTF8=1` (default) against `=0`.

**Method.** Each corpus is generated once and fed through `--from-file`, so both arms read identical
bytes. Arms alternate on/off then off/on in each pair, and every run records the 1-minute load.
The scripts wait for a quiet host before starting, but other projects share mac-studio and the load
rose during some passes. Read each row with its load.

| pass | file | corpora | pairs |
|------|------|---------|-------|
| 1 | `pass1.txt` | 64 MiB, all six arm/corpus pairs | 6 |
| 2 | `pass2.txt` | 512 MiB (unicode_mix 64 MiB) | 10 |
| 3 | `pass3.txt` | UTF-8 only, quiet window | 10 / 6 |

**Results.** off/on above 1 means SIMD is faster. The paired median is the median of off/on over
the pairs.

| switch | corpus | pass | load | off/on paired median | SIMD faster | CV on / off |
|--------|--------|------|------|----------------------|-------------|-------------|
| FT_PARSER_SIMD | long_lines | 2 | 19-32 | **1.098** | 9/10 | 9.4 / 8.1% |
| FT_PARSER_SIMD | long_lines | 1 | 38-39 | 1.080 (medians) | | 6.9 / 7.1% |
| FT_PARSER_SIMD | seq_lines | 1 | 37-44 | 1.000 (medians) | | 1.1 / 0.9% |
| FT_PARSER_SIMD | color_random | 2 | 36-91 | 0.993 | 3/10 | 7.5 / 7.4% |
| FT_PARSER_SIMD | color_random | 1 | 43-44 | 0.973 (medians) | | 4.7 / 3.4% |
| FT_PARSER_SIMD | color_emoji_random | 2 | 80-96 | 0.985 | 5/10 | 5.7 / 3.8% |
| FT_PARSER_SIMD_UTF8 | unicode_mix | 3 | 21-55 | **1.052** | 9/10 | 3.1 / 3.7% |
| FT_PARSER_SIMD_UTF8 | color_emoji_random | 3 | 56-61 | 1.018 | 3/6 | 2.0 / 2.2% |

Pass 2's UTF-8 rows ran at load 58-107 (CV 27-51%) and are not used.

**Reading.**
- The ASCII scan wins about 10% on long_lines: 9 of 10 pairs, sign test p about 0.02.
- It is neutral on seq. seq rows are 1-8 characters behind an LF, and their cost is row work
  (ft-70s4z, ft-3floy), not scanning.
- It shows no loss above noise on color_random, the 1-character-run corpus. Pass 2's paired
  median is -0.7%; pass 1's median-of-medians is -2.7%, within that pass's CV.
- One-pass UTF-8 validation wins about 5% on unicode_mix: 9 of 10 pairs at CV about 3%.
- Both are neutral on the T0 emoji corpus, whose time goes elsewhere (see
  `../t0-hotspots-07f454fb4/`).
