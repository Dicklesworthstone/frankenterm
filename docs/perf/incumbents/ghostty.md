# Ghostty incumbent contract

Bead ft-yccm0.1.3 (plan M.2) of the mac-render epic ft-yccm0.

This contract fixes what "Ghostty" means in every headless head-to-head the epic reports. It is
consumed by:
- the Track B headless gate (ft-yccm0.3.5: color-random >= 2x, seq >= 1.5x);
- the B2 checkpoint numbers (ft-yccm0.3.2);
- the final scoreboard (ft-yccm0.5.1).

The rule from the suite: a win needs the incumbent live in the same invocation. A speedup measured
against an old number counts as maintenance, not as a win.

The runner is `scripts/ghostty-headless-h2h.sh`. It reads the pin block below and **fails closed**
on any drift: exit 3, before anything is measured.

## Pins

The runner parses this block. It refuses an unknown, missing, duplicated or malformed key.

```ghostty-h2h-pins
# Incumbent source: ~/projects/ghostty (override: --ghostty-src). Read only, never modified.
ghostty_commit = e500d414f2ef688d86d0228f39e8ca1a1285f72a
ghostty_version = 1.3.2-dev

# Toolchain: the official zig release tarball, verified before extraction.
zig_version = 0.16.0
zig_tarball = zig-aarch64-macos-0.16.0.tar.xz
zig_tarball_url = https://ziglang.org/download/0.16.0/zig-aarch64-macos-0.16.0.tar.xz
zig_tarball_sha256 = b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489
zig_tarball_top_dir = zig-aarch64-macos-0.16.0

# Build (run from the checkout with --prefix and --cache-dir outside it).
build_flags = -Demit-bench -Doptimize=ReleaseFast -Demit-macos-app=false
bench_binary = bin/ghostty-bench

# Bench invocation: +terminal-stream --data=FILE --terminal-rows=R --terminal-cols=C
bench_action = +terminal-stream
# Facts of TerminalStream.zig at the pinned commit (not flags): 64 KiB reads,
# Terminal.init default max_scrollback_bytes.
bench_read_chunk_bytes = 65536
bench_default_scrollback_bytes = 10000

# Dimensions, ROWSxCOLS: the 80x120 default of both benches, then the
# operator's 6K window (landscape: 117 rows of 512 columns).
geometries = 80x120 117x512
primary_corpus = color_emoji_random

# Ghostty.app, the incumbent of the GUI rows (M.3). Verified here so that
# headless and GUI numbers in one report name the same incumbent build.
app_path = /Applications/Ghostty.app
app_bundle_id = com.mitchellh.ghostty
app_short_version = 46edeee40
app_build = 16813
app_binary = Contents/MacOS/ghostty
app_binary_sha256 = 38695a64cf09b89712c38f72fbb04a4a656917ec51faea2f0b9b404d0d49c719

# FrankenTerm arm: frankenterm-term's ingest_throughput example, lane term.
ft_profile = release-perf
ft_scrollback = 3500
ft_chunk_bytes = 65536

# Gates.
min_runs = 10
warmup = 3
max_cv_pct = 5
max_load_1m = 4.0
```

### Re-pinning

Drift is never waved through by a flag. To move the incumbent, follow these steps:
1. Update the pins above in one commit, saying why: a new Ghostty commit, a new zig, a Ghostty.app
   update.
2. Run `scripts/ghostty-headless-h2h.sh --check-pins` on the measurement host.
3. Re-run the baseline.

Receipts record the contract's SHA-256, so every number names the exact pins it was taken under.
Ghostty.app updates itself, so expect its three app pins to drift first. The runner checks them
even for headless-only runs, so one report never mixes incumbent builds.

## Toolchain

The pinned Ghostty needs zig >= 0.16.0 (`minimum_zig_version` in its `build.zig.zon`; the runner
checks it). Download hygiene:
- `--fetch-zig` downloads `zig_tarball_url` into a fresh, empty directory under `--work`. It uses
  curl with `--proto =https` and the user agent AGENTS.md requires.
- The tarball's SHA-256 is checked against the pin before anything is extracted.
- `--zig-tarball PATH` uses an already-downloaded tarball, held to the same check.

Each extraction goes into a fresh directory. The runner records a digest of every file's path,
size and mode, and checks the digest on every reuse. A tree that no longer matches is never used
again; the runner extracts afresh from the verified tarball, and nothing is deleted.

This is not hypothetical. On 2026-10-06, the copy extracted during planning had lost its whole
`lib/std/Target/` directory. A cleanup sweep for `target` directories matches `Target` on the
case-insensitive APFS volume. Builds with that copy failed with `unable to load 'x86.zig'`.

## Build

From the Ghostty checkout:

    zig build -Demit-bench -Doptimize=ReleaseFast -Demit-macos-app=false \
        --prefix  <work>/ghostty-<sha12>-zig0.16.0/out \
        --cache-dir <work>/ghostty-<sha12>-zig0.16.0/cache

The checkout must be clean at the pinned commit before the build and after it. The runner checks
`git status --porcelain --untracked-files=all` with `--no-optional-locks`, so even the check writes
nothing there. Zig's global package cache (`~/.cache/zig`) holds the fetched dependencies, and it
is outside the checkout too.

A stamp next to the binary records:
- the build key: commit, zig tarball SHA-256 and flags;
- the binary's SHA-256.

A later run reuses the binary only when both still match. Otherwise it rebuilds.

## Arms

| Arm | Command (identical corpus file and dimensions) |
|---|---|
| ghostty | `ghostty-bench +terminal-stream --data=F --terminal-rows=R --terminal-cols=C` |
| frankenterm | `ingest_throughput --lane term --from-file F --rows R --cols C --scrollback 3500 --chunk 65536` |

**What `+terminal-stream` does** (`src/benchmark/TerminalStream.zig` at the pinned commit):
- `Terminal.init` with only rows and cols, followed by `fullReset`;
- the full readonly stream handler (`terminal.TerminalStream`), so every escape updates real
  terminal state;
- unbuffered 64 KiB `readSliceShort` reads straight from the file, each handed to `nextSlice`.

**What the FrankenTerm arm does:**
- It is `frankenterm/term/examples/ingest_throughput.rs`, built with
  `--profile release-perf` (opt-level 3, thin LTO).
- It reads the file into memory and feeds `Terminal::advance_bytes` 64 KiB at a time.
- It hashes the input (SHA-256) and fingerprints the final grid. The fingerprint also checks
  sanity, and the process exits 1 if a check fails.
- `release-interactive` (the shipped profile) is accepted through `--ft-profile`. `release`
  (opt-level z) and debug builds are refused: the bench reports its profile from its cargo target
  path, so the binary must stay where cargo put it.

## Fairness controls

**Identical bytes.**
- The runner generates the corpora once, with `ingest_throughput --gen-only` from ft-yccm0.1.2. It
  then hashes every file itself and refuses any file whose SHA-256 differs from the generator's.
- Both arms read the same path.
- `--corpus-file` adds arbitrary files, e.g. the operator's 750 MB `color-emoji-random.bin`.

**Identical dimensions.** 80x120 (both benches' default) plus 117x512 (the operator's 6K window).

**Identical feed size.** Both engines get 64 KiB per call. ghostty-bench's read size is fixed in
its source, so the FrankenTerm arm follows it. FrankenTerm's own bench defaults to 128 KiB; the
runner warns if `--ft-chunk` breaks the match.

**Scrollback equivalence.** This one cannot be made exact, and here is the equivalence used:
- **Ghostty's side.** ghostty-bench has no scrollback flag, and this contract forbids modifying
  Ghostty. `Terminal.init`'s default `max_scrollback_bytes = 10000` is below one page. PageList
  raises an effective limit to whatever holds the active area, so the timed Ghostty arm keeps about
  one to two standard pages:
  - 396 rows per page at 120 columns;
  - 93 rows per page at 512 columns.

  It evicts whole pages at a time. The width probe (below) runs a terminal with these defaults over
  the same bytes and records its final row count as `bench_equivalent_total_rows` for each
  geometry.
- **FrankenTerm's side.** The arm keeps its shipped default of 3500 lines plus the viewport.
- **Why this is equivalent per row.** Both engines are at capacity after the first few thousand
  rows of a 64 MiB corpus, so each new row evicts an old one. The per-row work is the same kind:
  FrankenTerm pops a line, and Ghostty recycles a page every few hundred rows.
- **Where they differ, and why FT keeps its default.** The difference is resident history. Holding
  more history can only cost FrankenTerm. Shrinking FrankenTerm's scrollback to Ghostty's level
  would tune FrankenTerm for the benchmark, so the arm keeps its default.
- `--ft-scrollback` runs a sensitivity row; the receipt records the value used.
- Ghostty's GUI default (`scrollback-limit-bytes = 50_000_000`) matters only for the GUI rows.

**Strongest optimisation for both arms.** ReleaseFast for Ghostty, release-perf for FrankenTerm.

**Process wall time for both arms.**
- hyperfine runs both arms without a shell (`-N`), so nothing is subtracted or estimated. Each
  sample is one process from exec to exit.
- The FrankenTerm sample includes its input hash and its final-state fingerprint. The bench's own
  clock excludes both, so they count against FrankenTerm.
- The bench's internal feed-loop time is recorded per row for attribution
  (`internal_lane_secs_untimed_runs`), but it never enters a verdict.

**Interleaving.**
- Each round is one hyperfine invocation with one timed run per arm.
- The order alternates AB, BA, AB, BA (ABBA), so neither arm always runs second in a thermal or
  frequency window.
- Round 1 adds `--warmup 3` for each arm.
- Every round's hyperfine JSON is kept in the receipt directory.

**Sanity on every timed run.** The FrankenTerm bench exits 1 when its final state fails the grid
invariants, and hyperfine aborts the row on any non-zero exit. Two untimed FrankenTerm runs, one
before the rounds and one after, record the bench's JSON line, profile and final-state
fingerprint. If the two fingerprints differ, the row is refused.

## Gates and verdicts

A row's verdict is exactly one of:
- `ft_faster`: Ghostty's median divided by FrankenTerm's median is above 1;
- `ghostty_faster`: that ratio is below 1;
- `NO_ADMISSIBLE_RATIO (reason; ...)`.

The runner refuses a ratio when any of these holds:
- **runs:** fewer than `min_runs` (10) timed runs for an arm;
- **cv:** either arm's coefficient of variation is above `max_cv_pct` (5%). This is the sample
  standard deviation over the mean, using every timed run;
- **load:** the 1-minute load average, sampled before every round and after the last, peaked above
  `max_load_1m` (4.0) or could not be read;
- **ft sanity / ft build / ft state:** a failed sanity check, the wrong profile, debug assertions,
  or different final states before and after the rounds;
- **tie:** equal medians.

A refused row reports `ft_speedup: null`. The unadmitted ratio is kept only as
`ft_speedup_unadmitted`, for diagnosis. The receipt's top-level verdict is the primary corpus row's
verdict: color_emoji_random, the operator's T0 workload, at the first geometry.

**Why 4.0.** The host is a Mac mini M4 Pro with 10 performance cores and 4 efficiency cores. Both
arms are single-threaded. Below 4 runnable threads, each arm can have a performance core to itself,
and the shared memory bandwidth, cache and thermal budget are lightly used. Numbers taken under the
swarm's usual load of 7–20 are the planning session's provisional figures, not verdicts.

**Overrides only tighten.**
- `--max-cv` and `--max-load` can lower the contract's values.
- `--rounds` can raise the run count.
- Anything looser is a usage error (exit 2).

Loosening a gate means editing this contract, which leaves a trace.

**p95** is the nearest-rank percentile. With 10 runs, p95 is the slowest run.

## Emoji width parity

If the engines give the same character different widths, they wrap a different number of rows and
do different work. The runner checks this on color_emoji_random, for every geometry, untimed:
- **Ghostty side.** `scripts/ghostty-h2h-probe/` is a small Zig program built against the
  `ghostty-vt` module of the pinned checkout, with the same zig. It feeds the file exactly as
  `+terminal-stream` does: `Terminal.init`, `fullReset`, default modes, 64 KiB slices. It prints:
  - the final cursor;
  - the total rows of an unlimited-scrollback terminal;
  - the number of soft-wrapped rows;
  - the row count of a terminal with ghostty-bench's default limit.
- **FrankenTerm side.** One `ingest_throughput --lane term` run with a scrollback large enough to
  keep every row.
- **Compared.** Rows written (`total_rows - (rows - 1 - cursor_y)`, so wraps =
  `rows_written - 1 - line_feeds`) and the final cursor column. A disagreement is recorded verbatim
  in the receipt's `emoji_width_parity` block; it is not a gate.
- **Saturated cap.** If the FrankenTerm run fills its scrollback cap, the comparison is reported as
  undecided (`agree: null`).
- **Probe output.** It is written with a streaming writer. Zig's default `File.writer` uses
  positional writes, which overwrite earlier lines when stdout is a file opened for appending.

**Known disagreements at the pinned commit,** found while building the probe on a 1 MiB slice of
the corpus:
- **ft-b35o7.** A wide character that reaches the last column is clipped in place by FrankenTerm.
  Ghostty instead leaves a spacer and wraps the character to the next row. This is the dominant
  effect: Ghostty wrote 617 rows to FrankenTerm's 611, and the first divergence is frame 137 of
  41,948.
- **ft-7cy5r.** Seven Unicode 17 emoji in the pool are width 1 in FrankenTerm and wide in Ghostty.

Every other codepoint of the 1376-emoji pool has the same width in both engines, with the corpus's
SGR sequences between characters. With those SGRs in place, neither engine joins a skin-tone
modifier to the emoji before it. Ghostty starts with mode 2027 (grapheme clustering) off.

## Receipt

`--out DIR` must be new or empty, because receipts are never overwritten. It receives:

| File | Content |
|---|---|
| `receipt.json` | schema `ft.bench.ghostty-h2h.v1` (see the fields below) |
| `rows/<corpus>@<RxC>/round-NN.json` | the raw hyperfine export of every round |
| `rows/.../ft-admission-{pre,post}.jsonl` | the FrankenTerm bench's own JSON line before and after the rounds |
| `rows/.../load.txt` | every load sample |
| `rows/.../row.json` | the row summary |
| `parity/color_emoji_random@<RxC>/` | both probe outputs and the comparison |
| `pins.json`, `gates.json`, `fingerprint.json`, `corpora.jsonl`, `inputs.jsonl`, `ghostty-app.json`, `ghostty-bench-stamp.json` | the inputs to the verdict |
| `run.log` | every step, timestamped (also on stderr) |
| `SHA256SUMS` | every file above except `run.log`; verify with `cd DIR && shasum -a 256 -c SHA256SUMS` |

`receipt.json` holds:
- the contract SHA-256 and the parsed pins;
- the effective gates;
- the host fingerprint: model, CPU, cores, memory, OS, thermal state, load, plus zig, hyperfine,
  rustc and macOS SDK versions;
- the SHA-256 of every input and of every binary (ghostty-bench, the width probe,
  ingest_throughput);
- the Ghostty commit and describe, and the Ghostty.app identity;
- per row: per-arm n, median, p95, mean, stddev, CV, min, max, every sample and peak RSS;
- per row: the paired-round vote and the ratio;
- the parity blocks;
- the verdicts.

`python3 -I scripts/ghostty_h2h.py validate receipt.json` checks the schema. Every run does this
before printing its verdicts.

Exit codes:
- 0: every row admitted a verdict;
- 5: the receipt was written, but some row is `NO_ADMISSIBLE_RATIO`;
- 3: pin drift;
- 4: an arm, build or input failed;
- 2: usage;
- 1: internal error.

## Running it

    # Pins only (also builds ghostty-bench and the probe when their stamps are stale):
    scripts/ghostty-headless-h2h.sh --check-pins --zig-tarball <path>/zig-aarch64-macos-0.16.0.tar.xz

    # End-to-end smoke with both real arms on 64 KiB (2 rounds; must refuse on runs):
    scripts/ghostty-headless-h2h.sh --self-test --zig-tarball <...> --ft-bin <target>/release-perf/examples/ingest_throughput

    # The baseline, in a quiet window, every ingest_throughput corpus at 64 MiB, both geometries:
    scripts/ghostty-headless-h2h.sh --zig-tarball <...> --build-ft \
        --out evidence/mac-render-perf/<date>/ghostty-h2h

    # The operator's own file as well:
    scripts/ghostty-headless-h2h.sh ... --corpus-file ~/color-emoji-random.bin

`scripts/test_ghostty_headless_h2h.sh` is the runner's own test. It covers:
- shellcheck;
- unit tests for pins, statistics, verdicts, parity and receipts;
- an end-to-end run against fake arms, including every drift case.

It needs neither Ghostty nor zig.

## Not covered here

- **GUI rows** (time to drain `cat` in Ghostty.app vs FrankenTerm, FPS). Those belong to M.3. This
  contract only pins the app build they use.
- **Allocation counts.** ingest_throughput reports `allocations: null`; a counting global allocator
  needs an audited unsafe impl.
- **Possible build-option difference.** The probe's `ghostty-vt` module is built in Ghostty's
  library mode, and ghostty-bench in app mode. The documented difference is the Kitty image storage
  limit, which no corpus here touches. Width and wrapping use the same tables and the same stream
  handler.
