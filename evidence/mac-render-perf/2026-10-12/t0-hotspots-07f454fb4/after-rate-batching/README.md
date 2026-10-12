# Main thread after batching cache `.rate` metrics

The profile one directory up showed the GUI main thread spending about 5% of
its CPU on per-lookup metrics during a T0 flood. Every glyph-cache and
`LfuCache` hit called `metrics::histogram!(…".rate").record(1.)`: a handle
lookup by key, then the recorder's `Throughput` mutex and a clock read.
`lfucache::RateCounter` now counts lookups in place and emits every 256,
plus the remainder on drop.

Same harness, script and sampling as the first profile (four floods, 7 s
`sample` each, symbolized `release-perf` GUI), with the change applied to
de50a4e4b. Host load 15-20, against 28-31 for the first profile.

| Main thread, share of its on-CPU samples | Before (`../sample-run*`) | After (here) |
|---|---|---|
| `register_histogram`, `HistogramFn::record`, metrics `Key` comparison | 2.11% | 0.22% |
| `mach_absolute_time` reached from `HistogramFn::record` | 2.65% | 0.03% |
| All metrics overhead | 4.76% | 0.25% |
| All `mach_absolute_time` | 5.68% | 3.30% |

The main thread stays about 88% on CPU in both, because it renders
continuously: the CPU freed goes into rendering. The drain itself is
producer-bound (see `../hotspots.md`) and was not expected to move.
