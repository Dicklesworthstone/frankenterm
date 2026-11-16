# frankenterm-term

This crate provides FrankenTerm's virtual terminal emulator. It is maintained
in this repository and derives from the WezTerm terminal implementation.

It is full featured, providing terminal escape sequence parsing, keyboard
and mouse input encoding, a model for the screen cells including scrollback,
sixel and iTerm2 image support, OSC 8 Hyperlinks and a wide range of
terminal cell attributes.

This crate does not provide any kind of gui, nor does it directly
manage a PTY; you provide a `std::io::Write` implementation that
could connect to a PTY, and supply bytes to the model via the
`advance_bytes` method.

The entrypoint to the crate is the [Terminal](src/terminal.rs)
struct.

The `byte_to_grid` benchmark includes `reflow_cpu` cases that call the real
`Terminal::resize` path with ASCII and Unicode scrollback. They measure cold
resize and repeated width changes, exclude parsing from timed resize, and
check generated text and the cursor's hard-newline position. These CPU-stage
elapsed-time measurements do not measure GUI presentation, font rasterization
or GPU work. Run them through the strict
RCH development lane, for example:

```sh
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  cargo bench -j2 --locked -p frankenterm-term --bench byte_to_grid \
  --profile release-perf -- reflow_cpu
```

The September 6, 2026 final measurement for
`ft-interactive-systems-performance-4tenz.7.12.1` compared fresh target hashing
with reuse of the exact signature of an immutable cached text wrap. Full source
validation still runs; image-bearing wraps keep fresh hashing because their
shared image data can change independently of the lines.

On the same shared Linux AMD EPYC 7282 worker, ten Criterion samples per case
gave these elapsed times for a **four-width cycle** (61, 200, 79, 120 columns).
The benchmark uses the library defaults with readability scoring disabled:

| Scrollback | Before | After | Reduction (95% interval) |
| --- | ---: | ---: | ---: |
| ASCII, 1,000 logical lines | 84.30 ms | 48.73 ms | 42.2% (40.3–44.2%) |
| Unicode, 1,000 logical lines | 101.45 ms | 70.67 ms | 30.3% (30.0–30.7%) |
| ASCII, 10,000 logical lines | 925.40 ms | 595.87 ms | 35.6% (35.1–36.1%) |
| Unicode, 10,000 logical lines | 1,374.55 ms | 945.59 ms | 31.2% (27.8–35.1%) |

The intervals use 100,000 independent bootstrap resamples of mean elapsed time
per iteration, with seed 20260906. The four cold-resize cases showed no detected
regression against the original baseline. The final run includes the cached
readability-scorecard correction; its 10,000-line ASCII cycle was 1.9% slower
than the intermediate optimized build, while retaining the gain above.

The retained RCH receipts are baseline `j23470`, final benchmark `j23497`,
the complete 416-test terminal suite `j23492`, and all-target Clippy `j23500`.
These results establish a CPU-stage improvement;
instantaneous resize, native Apple Silicon frame latency, and font-zoom
presentation remain separate measurements.

License: MIT
