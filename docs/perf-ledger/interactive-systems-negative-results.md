# Interactive Systems Performance Negative-Evidence Ledger

**Campaign:** `ft-interactive-systems-performance-4tenz`  
**Contract:** `docs/perf/mux-long-session-performance-campaign.md`  
**Opened:** 2026-07-27

This ledger prevents the mux/renderer/long-session campaign from repeating
wrong-pipeline measurements, retired micro-optimizations, and attractive but
falsified explanations. The detailed round 4-9 ledgers remain authoritative
for their experiments; this file is the cross-round index for the real
interactive-systems campaign.

## Rules

Every rejected candidate records:

- source revision and candidate revision;
- exact workload, host/topology, profile, configuration, and transport;
- focused and broad commands/artifacts from the same measurement window;
- sample count, distribution, confidence/noise, and `cv_pct`;
- semantic/byte/state and visual-equivalence results;
- the measured outcome and decision; and
- exactly one primary retry predicate using one of the eight repository-owned
  canonical forms in
  [`round4-negative-results.md`](round4-negative-results.md#the-8-retry-condition-forms-every-entry-closes-with-exactly-one-no-anti-vocabulary).

A candidate with incomplete identity or noisy evidence remains an open
experiment. It is not converted into a flattering keep or a durable rejection.

## Closed evidence-boundary entries

### IS-N001 — The RQ-S1 dirty-row loop is not live 200-pane resize proof

- **Classification:** wrong evidence pipeline for the live claim
- **Source revision/artifact:** retained
  `docs/attestations/tui/resize-fps-rq-s1-20260606T2351Z.jsonl`;
  `crates/frankenterm-core/benches/resize_storm.rs`
- **Observed result:** the retained artifact contains 20 rows and reports a
  maximum p99 frame time of `106us`.
- **Code-path result:** the timed loop iterates pane IDs and dirty-row tuples.
  It does not enter `TermWindow`, mux resize, panes, scrollback reflow, shaping,
  WebGPU, or display presentation. A separate assertion resizes one terminal
  with 512 lines.
- **Decision:** retain the benchmark as a core dirty-work regression substrate.
  Reject its use as evidence of presented GUI FPS or resize appearance.
- **Primary retry condition:**
  > Do not retry from a cold read; use the real TermWindow/CAMetalLayer 20/50/200-pane live-resize rig in `ft-interactive-systems-performance-4tenz.3` instead.

### IS-N002 — The current RQ-S2 bench is not physical input-to-photon

- **Classification:** wrong evidence pipeline for the live claim
- **Source revision/artifacts:** 2026-07-16 retained macOS run;
  `docs/attestations/tui/input-to-photon-rq-s2-macos-20260716T140705Z.jsonl`;
  Criterion estimates beside it
- **Observed result:** cold sample `351.65ms`; Criterion steady-state mean
  `33.30ms`, median `33.54ms`, standard deviation `2.72ms`; target `<16ms`;
  `within_target=false`.
- **Code-path result:** the bench renders a synthetic headless fixture, creates
  GPU resources, forces GPU-to-CPU readback, and uses four fixed pre-render
  stage durations. It does not receive an `NSEvent`, traverse the client/server
  mux and PTY, invalidate a production window, or measure display scan-out.
- **Decision:** preserve the result as an honest native Metal proxy and its
  over-target status. Its confounders point in both directions: host load and
  GPU-to-CPU readback add work, while NSEvent, mux/PTY, production invalidation,
  scan-out, and photons are omitted. It is not an upper or lower bound. Reject
  any physical input-to-photon claim from this substrate.
- **Primary retry condition:**
  > Do not retry from a cold read; use the correlated NSEvent-to-LAN-to-PTY-to-CAMetalDrawable trace and physical/display timing in `ft-interactive-systems-performance-4tenz.2` instead.

### IS-N003 — RQ-S6 does not measure a key under a live 50-pane burst

- **Classification:** wrong evidence pipeline for the live claim
- **Source artifact:** `crates/frankenterm-core/benches/heavy_burst.rs`
- **Code-path result:** the bench drives `SustainedBurstHarness` deferred
  operations and asserts a modeled per-frame p95 budget. It does not send a key
  through the connection FIFO, socket, server dispatch, mux main thread,
  terminal/PTY, or renderer.
- **Decision:** retain it as a deferred-operation regression substrate. Reject
  it as live heavy-burst input evidence.
- **Primary retry condition:**
  > Do not retry from a cold read; use the 50-pane 1MB/s production workload with the stage-correlated keypress trace in `ft-interactive-systems-performance-4tenz.2` instead.

### IS-N004 — A short core simulation is not an aged-session soak

- **Classification:** unsupported scope upgrade
- **Source artifact:** `crates/frankenterm-core/tests/e2e_swarm_stress_core.rs`
- **Code-path result:** the test uses simulated `TieredScrollback` panes and
  identifies `metric_source=core_simulation`. It does not drive the GUI, mux
  transport, PTYs, actual storage/search pipeline, reconnects, or GPU.
- **Decision:** preserve the deterministic core test. Reject “50/100/200-pane
  long-haul” or resource-stability claims based solely on this run.
- **Primary retry condition:**
  > Do not retry from a cold read; use the retained 4h/24h/72h live mux, PTY, network, GUI, storage, and renderer soak in `ft-interactive-systems-performance-4tenz.4` instead.

### IS-N005 — “Use all Threadripper CPUs” is not a latency strategy

- **Classification:** architectural explanation rejected
- **Source revision inspected:** `5e23496785fa02018ad76a6534a1fb6588221e58`
- **Observed target:** `trj` is a Ryzen Threadripper PRO 5995WX with 64
  physical cores and 128 logical CPUs.
- **Code-path result:** one remote connection task, server dispatch lane, mux
  main queue, terminal mutex, and GUI main thread remain serialized. The reflow
  heuristic can select 64 workers for a 64-line batch and then reject
  parallelism because it requires eight lines per selected worker. Pane resize
  can simultaneously create many OS workers.
- **Decision:** reject indiscriminate worker-count increases and the claim that
  aggregate core count alone should remove keypress lag. Test bounded useful
  work and locality.
- **Primary retry condition:**
  > Reconsider only inside the broader topology-aware persistent resize/reflow scheduler redesign tracked as `ft-interactive-systems-performance-4tenz.7`.

### IS-N006 — Direct-LAN routing is not yet the primary lag explanation

- **Classification:** orientation measurement; primary-cause hypothesis
  rejected pending a real transport trace
- **Host/date:** local Mac to `trj`, 2026-07-27
- **Observed result:** direct `10.10.10.1` ping averaged about `0.257ms` with
  `0.861ms` maximum over 30 samples; tailnet `100.91.120.17` averaged about
  `0.994ms` with `1.559ms` maximum. The live SSH process used the tailnet
  endpoint.
- **Decision:** keep route/transport in the matrix because direct LAN is lower
  latency and jitter. Reject the roughly `0.74ms` ping delta as a sufficient
  explanation for conspicuous multi-frame sluggishness.
- **Primary retry condition:**
  > Retry only if the correlated production trace attributes a clearly-above-noise share to transport or route selection on the 50-pane burst or 200-pane resize-concurrent workload.

### IS-N007 — Generic Apple Metal SoA/vertex-bandwidth work is retired

- **Classification:** measured performance rejection
- **Source:** `docs/perf-ledger/round6-negative-results.md`
- **Observed result:** the SoA instanced-glyph experiment did not produce a
  realistic Apple Metal win; GPU readback/wait dominated while vertex work was
  below the ledger's meaningful attribution threshold.
- **Decision:** do not repeat buffer-layout or generic vertex-bandwidth changes
  from source inspection. Per-frame bind-group/buffer reuse remains a distinct
  profile-gated hypothesis.
- **Primary retry condition:**
  > Retry only if a profiler attributes a clearly-above-noise share of at least 0.5% of live frame time to vertex create, bind, or upload work on the real M4/M5 resize workload.

### IS-N008 — Persistent-rope reflow is retired as a standalone design

- **Classification:** measured algorithm rejection
- **Source:** `docs/perf/persistent-rope-evaluation.md`
- **Observed result:** the evaluated persistent-rope design was approximately
  `21x` slower on reflow than the incumbent representation.
- **Decision:** preserve the negative. The campaign can change publication,
  cancellation, batching, snapshotting, and scheduler topology without
  reviving this representation.
- **Primary retry condition:**
  > Not worth retrying as a standalone patch.

### IS-N009 — COW snapshots do not solve a sub-microsecond scrollback lock

- **Classification:** measured opportunity rejection
- **Source:** round 4-9 performance ledgers
- **Observed result:** measured lock p95 was roughly `250-333ns`, while clone
  holds were roughly `15-47us`.
- **Decision:** reject copy-on-write solely to evade that measured lock. This
  does not reject a coherent short-lock render snapshot when a live profile
  attributes millisecond-scale paint/parser contention.
- **Primary retry condition:**
  > Worth reconsidering when the campaign's real live-session terminal-lock p95 reaches 50us.

### IS-N010 — Stable GPU allocation and `device.poll` were red herrings for the progressive slowdown

- **Classification:** historical causal rejection
- **Source revision:** root-cause/fix commit
  `1d9e3b9e67cc43910c4c138d9e2ee03b7742435f`
- **Observed incident:** render CPU rose from roughly 30% toward 70% over about
  40 minutes as glyph diversity accumulated. The stable GPU allocation did not
  explain the slope.
- **Root cause/result:** atlas overflow cleared the shape cache and repeatedly
  reintroduced full-screen HarfBuzz work. Decoupling atlas-invariant shape
  information reported roughly 56% fewer HarfBuzz calls per rebuild and about
  31% lower render CPU with byte-identical re-resolution.
- **Decision:** preserve the root cause and the failed GPU-memory/
  `device.poll` theories. Do not restart generic GPU-memory work to explain a
  recurrence without new attribution.
- **Primary retry condition:**
  > Do not retry from a cold read; use the 4h/24h/72h time series for atlas generations, HarfBuzz calls, CPU, GPU residency, and cache hit rates instead.

### IS-N011 — Per-op micro-mining is not the opening move for this campaign

- **Classification:** campaign-level evidence boundary
- **Source:** round 4-9 keep and negative ledgers
- **Observed history:** custom maps/vectors, broad prefilters/caches, serial
  replacements for vectorized code, CSI/OSC lookup changes, overlap changes,
  Bloom/MPHF/fingerprint/Teddy ideas, and several other micro-levers were
  neutral, noisy, or regressions. Adaptive CDC, the scrollback prefix index,
  and dense ASCII paths already captured the proven workload-specific wins.
- **Decision:** the new campaign starts with end-to-end queue, lock, reflow,
  frame, and resource attribution.
- **Primary retry condition:**
  > Retry only if a profiler attributes a clearly-above-noise share to the exact retired operation on the real keypress, live-resize, or aged-session workload.

## Open hypothesis register

These are not negative results. Each remains open until a retained same-window
A/B satisfies the campaign keep gate or creates a closed entry above.

| Hypothesis | Bead | Required first evidence |
|---|---|---|
| Input/control traffic needs bounded priority/fair scheduling | `ft-interactive-systems-performance-4tenz.5` | client and server queue depth/age plus socket-ready→decode and decode→mux-start tails |
| Key acknowledgement performs redundant viewport work | `ft-interactive-systems-performance-4tenz.6` | deltas, rows, bytes, compression, and paints per key |
| Render should use a coherent short-lock terminal snapshot | `ft-interactive-systems-performance-4tenz.6` | terminal-lock hold/wait and parser lag attributed during paint |
| Resize/reflow needs a persistent topology-aware pool | `ft-interactive-systems-performance-4tenz.7` | thread creation/join, context switch, migration, batch-size, and p95/p99 attribution |
| Same-grid resize can retain/reproject line quads | `ft-interactive-systems-performance-4tenz.8` | global quad-miss/rebuild counts and live visual timing |
| Display-link pacing improves phase and input tails | `ft-interactive-systems-performance-4tenz.8` | actual 60/120Hz frame-phase and present timing |
| Zoom warmup should be deadline-aware | `ft-interactive-systems-performance-4tenz.8` | scale-change stage trace and missing-glyph/SSIM oracle |
| True atomic viewport-first reflow improves first present | `ft-35zzw` plus `.8` | first coherent viewport, first present, and cold convergence as separate intervals |

## New entry template

```markdown
### IS-YYYYMMDD-NNN — <candidate>

- **Classification:** measured performance rejection | wrong evidence pipeline | kept structural
- **Bead:** <id>
- **Baseline revision:** <sha>
- **Candidate revision:** <sha>
- **Target identity:** <host, CPU/GPU/topology, OS, display, transport>
- **Workload identity:** <pane count, age, output, action, seed, config/font hashes>
- **Focused command/artifact:** <exact command and retained path>
- **Broad command/artifact:** <exact command and retained path>
- **Samples/statistics:** <n, p50/p95/p99/p999, CI, cv_pct>
- **Equivalence:** <byte/state/visual/cursor/IME/a11y results>
- **Measured result:** <baseline -> candidate and percent>
- **Decision:** rejected | kept durable infrastructure
- **Primary retry condition:**
  > <one verbatim canonical form, fully instantiated>
```
