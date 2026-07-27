# Real Mux Interaction, Resize, and Long-Session Performance Campaign

**Status:** active investigation and optimization program  
**Opened:** 2026-07-27  
**Campaign epic:** `ft-interactive-systems-performance-4tenz`  
**Truth-contract bead:** `ft-interactive-systems-performance-4tenz.1`  
**Evidence ledger:** `docs/perf-ledger/interactive-systems-negative-results.md`

This document is the operating contract for improving the latency and visual
smoothness that an operator actually experiences in a FrankenTerm window. It
covers:

- a key event received on a local Mac, transported through a remote mux domain,
  written to a remote PTY, echoed or redrawn by the application, transported
  back, and presented in the local window;
- continuous window resize, grid reflow, font zoom, and display-scale changes;
- long-lived, high-pane-count sessions whose scrollback, glyph diversity,
  caches, queues, and resource use have had hours or days to age; and
- separate optimization and certification on recent Apple silicon and the
  `trj` Threadripper host.

This is deliberately a systems campaign, not a tenth round of speculative
sub-microsecond container swaps. Rounds 4 through 9 already built a strong
negative-evidence moat around many isolated CPU ideas. New work starts from the
live user path and its tail latency.

## 1. Authority and non-claims

The machine-readable attestation artifacts remain authoritative for release
claims. This campaign does not upgrade an SLO merely because code, a synthetic
benchmark, or an instrumentation DTO exists.

| Existing surface | What it currently establishes | What it does **not** establish |
|---|---|---|
| `resize_storm.rs` / RQ-S1 | Cost of a deterministic 200-pane dirty-row core loop, plus a separate single-terminal resize assertion | Presented GUI resize FPS, mux work, per-pane resize, full scrollback reflow, shaping, Metal, or appearance |
| `input_to_photon.rs` / RQ-S2 | A deterministic headless render substrate and one native Metal readback run | Physical key event, LAN transport, remote PTY, production window invalidation, `CAMetalDrawable` presentation, or photons |
| `input_latency_bench.rs` | Framework/DTO overhead for synthetic timestamps | Production input latency |
| `heavy_burst.rs` / RQ-S6 | Deferred-operation harness behavior under a modeled burst | A real key sharing the client socket, server dispatcher, mux main thread, PTY, and renderer with 50 live panes |
| `load-rig.md` | Deterministic replay and modeled load-regression substrate | Live mux panes, real PTYs, GUI/Metal, actual network transport, or an aged session |
| `e2e_swarm_stress_core.rs` | Core `TieredScrollback` simulation | A multi-hour GUI/mux/network/storage/reconnect soak |

In particular:

- The retained RQ-S1 result remains valid for the code it measured, but its
  `106us` maximum p99 row is not evidence of 200-pane presented resize FPS.
- The 2026-07-16 native macOS RQ-S2 run is correctly non-claiming. It measured a
  `351.65ms` cold sample and Criterion steady-state mean/median near
  `33.30/33.54ms`, over the `<16ms` target, through a loaded-host,
  headless-GPU-readback proxy.
- RQ-S2 on macOS, RQ-S3 on Wayland, RQ-S5 idle GPU, live high-scale resize, and
  long-session stability remain unproven until their named target runs exist.
- A software call to `present()` is a submit/presentation request. It is not a
  measured display or photon boundary.

## 2. Code-grounded architecture

### 2.1 Remote keypress path

The production path is:

```text
NSEvent
  -> WindowView::key_common
  -> TermWindow::process_key / key_event_impl
  -> ClientPane::key_down
  -> connection-wide client PDU FIFO
  -> client socket encode + per-PDU flush
  -> server dispatch queue
  -> mux main-thread task
  -> LocalPane::key_down
  -> Terminal mutex
  -> PTY write + flush
  -> PTY reader/parser
  -> Terminal mutex
  -> mux PaneOutput notification
  -> server render-delta computation
  -> client socket
  -> ClientPane cache/prediction reconciliation
  -> local mux notification fanout
  -> TermWindow invalidation
  -> CPU shaping/quad preparation
  -> WebGPU submit + present
  -> display scan-out
```

The important implementation points are:

1. `frankenterm/window/src/os/macos/window.rs::WindowView::key_common` receives
   and normalizes the native key event on the AppKit thread.
2. `crates/frankenterm-gui/src/termwindow/keyevent.rs` performs key-table,
   modal, encoding, and pane dispatch work on the GUI thread.
3. `frankenterm/client/src/pane/clientpane.rs::ClientPane::key_down` updates
   prediction state and detaches a `SendKeyDown` RPC.
4. `frankenterm/client/src/client.rs` places every request for a connection in
   one unbounded FIFO. A single connection task arbitrates input, resize,
   line-fetch, resync, and other RPCs, encodes each PDU, and flushes it.
5. `crates/frankenterm-mux-server-impl/src/dispatch.rs::dispatch` uses a bounded
   dispatch item queue and services queued output/notifications before polling
   socket readability. Continuous outbound work can therefore delay inbound
   key decoding; this is a hypothesis until queue-age traces confirm it.
6. `SessionHandler::process_one` sends `SendKeyDown` to the generic mux
   main-thread queue. `LocalPane::key_down` holds the terminal mutex while
   encoding, writing, and flushing the PTY.
7. The key task immediately calls `PerPane::compute_changes` before returning
   its acknowledgement. That function reads pane metadata, clones the visible
   viewport, filters dirty rows, and fetches the cursor row separately. Normal
   PTY echo can then produce a second later delta.
8. Client unilateral processing hydrates line caches, reconciles prediction,
   scans hyperlinks, and emits a local mux notification.
9. Each TermWindow subscriber schedules a main-thread task before it knows
   whether the pane is visible in that window.
10. macOS invalidation is paced by an integer-millisecond
    `1000 / max_fps` delay. With the default `max_fps = 60`, event phase alone
    can add nearly one 16.67ms refresh interval.

This path has several serialized stages. A host with 128 logical CPUs cannot
compensate for one congested connection writer, one server dispatch lane, one
mux main queue, one terminal mutex, or one GUI thread.

### 2.2 Resize and zoom path

The production resize path is:

```text
NSView live resize
  -> WindowEvent::Resized
  -> TermWindow::resize
  -> surface resize + coarse invalidation
  -> TermWindow::apply_dimensions
  -> every tab's Tab::resize when the terminal grid changes
  -> pane resize intents / workers
  -> Terminal::resize
  -> synchronous viewport, near, and cold scrollback reflow
  -> shaping, atlas lookup/raster, and quad creation
  -> WebGPU submit + present
```

The current implementation already coalesces a useful case: if pixel
dimensions change without changing the complete `TerminalSize`, it does not
send a mux resize. That falsifies the simple theory that every drag pixel
causes a remote PTY resize.

The remaining high-value facts are:

- `TermWindow::resize` and `apply_dimensions` both advance
  `quad_generation` and mark all panes dirty during a native resize.
- Even a same-grid sub-cell resize changes the global generation used by line
  quad keys, so visible line quads are rebuilt.
- A grid change resizes every tab in the window, not only the active tab.
- `Tab::apply_sizes_from_splits` creates scoped workers during the resize call,
  and each `LocalPane` may also create a dedicated resize worker. Large tabs
  can therefore create roughly one OS thread per pane.
- Reflow labels viewport, near, and cold batches, but all cold work still
  completes synchronously before resize returns. The recorded
  “viewport-ready” timestamp is not a first-present boundary.
- The reflow worker heuristic starts from `available_parallelism`, caps batches
  at 64 lines, and requires at least eight lines per selected worker. A 64-line
  batch chooses 14 workers on the local M4 Pro or 64 workers on `trj`, then
  rejects parallelism because `64 < workers * 8`. The batch runs serially on
  precisely the high-core systems this campaign targets.
- Local painting enters `LocalPane::with_lines_mut`, holding the terminal mutex
  across line hashing, shaping, glyph/atlas work, and quad generation. The PTY
  parser needs the same mutex.
- Server push clones the entire visible viewport before filtering dirty rows.
  Remote GUI painting then clones line vectors again through the pane trait
  adapter.
- Font zoom correctly invalidates scale-dependent shaping state, but also
  spends up to a fixed 16ms synchronously warm-rasterizing common glyphs.
- macOS already discovers the screen maximum refresh rate, while the repaint
  throttle remains fixed to the configured 60 FPS default and is not
  display-link phase aligned.

The full-grid triple-buffer and true asynchronous viewport-first architectures
described elsewhere in the docs are not wired into this live paint/reflow path
today. They remain candidate designs, not current performance facts.

### 2.3 Long-session risk model

Long sessions change the workload:

- scrollback becomes deeper and more fragmented;
- glyph diversity drives atlas churn;
- shape, line, image, hyperlink, and remote pane caches age;
- reconnection and subscription history can expose resource-lifetime bugs;
- burst output overlaps maintenance, search, and operator actions; and
- small positive CPU, memory, GPU-residency, queue, or thread slopes become
  visible only after hours.

The June 2026 progressive-render incident is the canonical warning. Commit
`1d9e3b9e6` traced rising render CPU to atlas-overflow-driven shape-cache
clearing, not the stable GPU allocation first suspected. The retained change
decoupled atlas-invariant shaping data, reported roughly 56% fewer HarfBuzz
calls per rebuild and about 31% lower render CPU, and preserved byte identity.
Synthetic resize storms had not reproduced the real aged-session symptom.

Consequently, every soak must retain time series and change-point evidence,
not just a start/end RSS pair or a short simulated loop.

An uncontrolled five-second `/usr/bin/sample` of the operator's already-aged
GUI on 2026-07-27 reinforces the need for that rig without constituting proof.
The process reported about `6.3G` physical footprint (`6.9G` peak), and
603 of 3011 sampled main-thread stacks included the `do_paint_webgpu` ancestry,
with line hashing/cloning, shaping/cache, glyph, quad, and copy work visible.
That is sampled stack ancestry, not 20% self-time; the host was live, the
binary identity was mismatched/aged, and the workload was uncontrolled. No
capacity or causal claim is derived from it.

## 3. Target systems

These are separate certification lanes, not interchangeable representatives.

| Lane | Observed system | Campaign implications |
|---|---|---|
| Local controller | Mac mini `Mac16,11`, M4 Pro, 14 CPU cores (`10P + 4E`), 64 GiB, macOS 26.2 | Protect AppKit/input/render latency; use QoS semantics and measure P/E placement, display mode, frame phase, unified-memory pressure, and thermal state |
| Future M5 | Exact M5/M5 Pro/M5 Max SKU to be recorded when hardware is available | Run independently; do not extrapolate M4 results. Core topology and display/GPU behavior are campaign dimensions |
| `trj` remote | Ryzen Threadripper PRO 5995WX, **64 physical cores / 128 threads**, 8 observed L3 instances, about 536 GB live RAM, Linux 6.17 | Favor compact latency-critical placement and topology-aware bounded parallelism; measure SMT, CCD/LLC locality, migration, IRQ/network placement, and memory locality |

The operator shorthand “128-core `trj`” refers to its 128 logical CPUs. AMD
specifies the 5995WX as 64 cores and 128 threads. That distinction matters:
spawning 128 workers for a small reflow batch is not a hardware optimization.

On Apple silicon, thread QoS is the primary scheduling signal. We will test
`userInteractive` latency-critical work and lower-QoS background work, but we
will not hard-code guessed P-core IDs. Apple explicitly recommends QoS and
actual-hardware measurement over assumptions about asymmetric cores.

## 4. Workload contract

Every retained run records the Cartesian coordinates that identify its
workload. Missing coordinates make a run a diagnostic, not a certification
artifact.

| Dimension | Required values |
|---|---|
| Pane count | 1, 20, 50, 200 |
| Session age | cold, 4h screening, 24h, 72h |
| Output | idle, interactive shell, mixed fleet, aggregate 1 MB/s, burst/adversarial |
| Interaction | typing, paste/IME, scroll, split/tab change, resize, zoom, DPI/display move, reconnect |
| Resize | same-grid pixel drag, grid-changing continuous drag, 80→200 and 200→80 columns |
| Transport | SSH proxy/socketpair, SSH stdio, direct TLS; actual route recorded |
| Display | actual refresh mode; at least 60 Hz and 120 Hz when available |
| Content | ASCII, Unicode/combining/RTL, emoji/color glyphs, images, hyperlinks, alternate screen |
| State | fresh caches, warm caches, atlas-near-cap, deep scrollback, active prediction |

The minimum baseline matrix is:

1. one quiet remote pane, interactive shell, no concurrent output;
2. 50 panes with aggregate 1 MB/s output and a designated interactive pane;
3. 200 panes with mixed output and a continuous five-second live resize;
4. font zoom in/out after a glyph-diverse warmup;
5. the same interaction cells after a four-hour screen; and
6. selected worst cells at 24 and 72 hours.

Runs freeze:

- source and binary SHA;
- build profile and compiler flags;
- config, font files/hashes, cell metrics, window dimensions, display and
  refresh mode;
- transport, endpoint address, route, RTT/jitter/loss;
- pane/workload seed and remote command;
- OS/kernel, firmware, power mode, thermal state, and background load;
- CPU topology, affinity/QoS, memory/NUMA policy; and
- evidence schema version and wall-clock/monotonic clock metadata.

The preflight must also prove that the running GUI, bundled `ft`, and mux
protocol identity come from the intended compatible build. The live app bundle
observed during this investigation still exhibits the already-tracked
`ft-1itzl` mismatch: the on-disk GUI reports 0.12.0 while bundled `ft` reports
0.11.0, and `ft robot state` can fall back to a mismatched CLI path and hang.
No retained campaign result may silently mix those binaries. Restarting or
replacing the operator's running GUI remains an explicit operator action.

Orientation measurements on 2026-07-27 found direct `10.10.10.1` LAN ping
around `0.257ms` average (`0.861ms` max over 30 samples), while the live
FrankenTerm SSH process used the tailnet endpoint and measured around
`0.994ms` average (`1.559ms` max). The roughly `0.74ms` mean difference is
worth recording and A/B testing, but it cannot by itself explain conspicuous
sluggishness. Queueing, terminal-lock, and frame-phase evidence remain the
primary investigation.

## 5. Correlated trace contract

### 5.1 Identity and clocks

Each operator action receives a monotonic 64-bit correlation ID. It propagates
through protocol-safe metadata where possible and is associated with deltas,
paints, and presentation callbacks.

Never subtract unsynchronized clocks across hosts. Report:

- Mac-local intervals;
- remote-host-local intervals;
- wire/round-trip intervals measured on one clock; and
- any cross-host interval only when a retained synchronization bound (for
  example PTP) is tighter than the claimed result.

Wall clock is metadata. Stage duration uses monotonic clocks.

### 5.2 Required keypress stages

| Stage | Required event/interval |
|---|---|
| K0 | AppKit key callback receipt |
| K1 | GUI key mapping complete |
| K2 | client RPC enqueue, queue depth, and oldest age |
| K3 | client dequeue, encode complete, socket flush complete |
| K4 | server socket readable and decode complete |
| K5 | server dispatch queue and mux-main task wait |
| K6 | terminal-lock wait/acquire; PTY write and flush |
| K7 | PTY echo/read; parser wait/apply |
| K8 | server delta compute: rows, bytes, clone/compress time |
| K9 | client receive/decode/apply; prediction result |
| K10 | local mux fanout and GUI invalidation |
| K11 | paint start; terminal-lock wait/hold; shape/atlas/quad work |
| K12 | GPU submit and drawable present request |
| K13 | drawable/display completion; physical photon only if a detector measures it |

Also count RPCs, deltas, dirty rows, full viewport clones, cursor-row
duplicates, paints, and frames per key. This will distinguish an input queue
problem from redundant acknowledgement/echo work.

### 5.3 Required resize/zoom stages

Record:

- native event receipt and event rate;
- same-grid versus grid-changing classification;
- GUI return time;
- tab/pane count visited;
- resize intent enqueue, supersession, worker creation, start, and join;
- terminal-lock wait/hold;
- viewport, near, and cold reflow duration and bytes/lines;
- first internally coherent viewport;
- first correctly presented frame;
- cold convergence completion;
- invalidated/reused/rebuilt line quads;
- shaping, glyph raster, atlas rebuild, bind/upload/submit, and frame phase; and
- final visual-oracle result.

“Viewport ready” is not “viewport presented.” Both timestamps are mandatory.

## 6. Performance and correctness contracts

Existing frozen release SLOs remain in force:

- RQ-S2 macOS input-to-photon: p95 `<16ms` for its defined scenario;
- RQ-S3 Wayland input-to-photon: p95 `<20ms`;
- RQ-S6 heavy burst: p95 `<50ms` at 1 MB/s across 50 panes;
- RQ-S1 resize: at least 60 sustained presented FPS with p99 frame
  `<=16.6ms` on the defined 200-pane gesture;
- reflow p95 `<5ms` for the defined 1000-line 80→200-column case; and
- RQ-S11 snap-back SSIM `>=0.999`, plus the existing parity corpus limits.

Those numbers are not changed here. The campaign adds the following admission
and diagnostic contracts:

### Input

- zero key loss, duplication, or reordering;
- no starvation of output, resize, resync, or control traffic;
- host-local stage budgets are frozen only after the first quiet-host baseline;
- the report always separates OS→socket, wire/remote, echo→invalidate,
  paint→present, and frame-phase latency; and
- loaded and aged tails are reported separately from quiet steady state.

As an initial user-perception envelope, report p95 relative to measured RTT
plus one actual display interval and p99 relative to RTT plus two intervals.
This is a diagnostic decomposition, not a replacement for the absolute RQ-S2
target.

### Resize and zoom

- no critical visual artifacts, stale frame, mixed-width frame, missing-glyph
  frame, cursor jump, selection/IME error, or accessibility geometry error;
- first correct present and complete cold reflow are separate metrics;
- presented-frame intervals, not synthetic loop iterations, adjudicate the
  live FPS claim;
- snap-back SSIM is at least `0.999`, with existing L-infinity and
  changed-pixel limits; and
- zoom and DPI changes preserve exact final rows, columns, glyph advances,
  cursor, images, hyperlinks, and alternate-screen state.

### Long sessions

- no crash, deadlock, stranded final pane update, or unbounded queue/cache/thread
  growth;
- after a declared warmup, no sustained statistically significant positive
  slope or change point in CPU, p95/p99 latency, RSS/physical footprint, GPU
  residency, atlas rebuild rate, queue age, or thread count;
- any numerical slope/cap threshold is frozen before the target run; and
- a four-hour screen gates expensive 24h/72h certification, but does not replace
  it.

## 7. Measurement and keep-gate protocol

The `running-the-gauntlet-on-your-rust-port` and
`extreme-software-optimization` disciplines apply:

1. Build/profile with `release-perf`, debuginfo line tables, and forced frame
   pointers where the profiler requires them.
2. Capture a fresh live-workload profile. A code optimization is admissible
   only if the target contributes at least 0.1% self-time, explains a material
   queue/lock wait or tail stage, or has an independently quantified resource
   opportunity. Aggregate CPU is not enough.
3. State one hypothesis and one lever. Do not bundle queue priority, batching,
   lock shortening, reflow parallelism, and cache changes into one A/B.
4. Capture focused and broad measurements from the same source snapshot,
   target, host, and measurement window.
5. Use distributions (`p50/p95/p99/p99.9`), sample counts, confidence
   intervals, and repeated-run variance. Microbench keep decisions require
   `CV <= 5%`; noisier end-to-end cells are classified `measurement_noise` and
   rerun rather than cherry-picked.
6. Run semantic/byte/state equivalence plus visual/cursor/IME/accessibility
   oracles before calling a candidate faster.
7. Keep only a material improvement beyond the confidence/noise bound and
   without a regression in another frozen metric.
8. Record every rejection in
   `docs/perf-ledger/interactive-systems-negative-results.md` with numeric
   results and exactly one primary retry condition.
9. Use strict remote RCH for Cargo proof:

   ```bash
   RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 \
     rch --no-self-healing exec -- \
     env CARGO_TARGET_DIR=/tmp/ft-<bead>-<lane> \
     cargo <check-or-test-command>
   ```

   Any local fallback is a failed proof lane. Native target-class runtime
   measurement on macOS is necessary for Metal/AppKit evidence but is not a
   substitute for the independently required Cargo proof.
10. Capture `git rev-parse HEAD` before and after every shared-tree proof and
    report snapshot movement.

## 8. Profile-gated optimization lanes

| Lane | Current evidence | First experiment | Falsifier / rollback gate |
|---|---|---|---|
| Client traffic arbitration | One unbounded FIFO and one socket task serve input, resize, fetch, and resync | Add class/depth/age telemetry; A/B bounded fair input/control service and latest-intent resize coalescing | Reject if key queue age is not attributed, tails do not improve, or output convergence/starvation worsens |
| Server dispatch fairness | Queued notifications are tried before socket readability | Measure socket-ready→decode delay; A/B a bounded outbound quota before polling input | Reject if delay stays negligible under reproduced lag or fairness hurts throughput/convergence |
| Mux-main/terminal lock | Input, resize, delta, parser, and paint converge on serialized queues/locks | Measure task wait and terminal-lock wait/hold before changing scheduling | Reject priority/lock changes if these stages are not tail contributors |
| Forced key delta | Key task computes a viewport delta before normal PTY echo may compute another | Count rows/bytes/deltas/paints per key; A/B minimal acknowledgement/cursor work | Reject if normal workloads do not duplicate material work or correctness/tails regress |
| Terminal render snapshot | Local paint holds terminal state through shaping and quad construction | Copy one coherent visible snapshot under a short lock, render outside | Reject if lock wait is negligible, copying dominates, or any mixed/stale state appears |
| Damage-only server push | Server clones full viewport, then filters dirty rows | Fetch/coalesce only dirty ranges and deduplicate cursor row | Reject if allocation/lock profile is insignificant or wire/state equivalence fails |
| Resize worker topology | Fresh/scoped/per-pane workers and high-core heuristic mismatch | Persistent bounded pool; sweep 1/2/4/8/P-core and useful-chunk fanout | Reject losing worker counts; never default to all 128 logical CPUs |
| True viewport-first reflow | Current “viewport-first” order still blocks on cold work | Atomically publish a coherent viewport, then cancelable cold convergence | Reject if first present is unchanged, final state differs, or mixed widths are observable |
| Same-grid invalidation | Native resize duplicates coarse invalidation and generation bumps | Remove the duplicate only; then separately split surface/projection from grid damage | Roll back on SSIM/cursor/selection/IME/a11y regression or no tail win |
| Frame pacing | Timer sleep defaults to 60 FPS despite known screen rate | A/B display-link-aware 60/120Hz pacing with input wake override | Reject if frame phase/jitter/input tails do not improve or power use is unacceptable |
| Zoom warmup | Fixed 16ms synchronous non-correctness warmup | Sweep 0/2/4/16ms and deadline-aware/background lazy warmup | Never retain missing glyphs or incorrect scale metrics; shaping invalidation remains required |
| Transport | Live route currently uses tailnet despite direct LAN availability | Compare identical production traces over SSH proxy, SSH stdio, and direct TLS/LAN | Do not promote route work if it saves only sub-ms and the user-visible tail remains |
| GPU allocation/binding | Per-frame bind groups/views and buffer mapping exist | Touch only after Metal/profile attribution | Existing SoA/vertex result blocks generic bandwidth work until its retry predicate fires |

Parser coalescing and predictive-echo thresholds are second-order experiments.
Predictive echo is currently suppressed below its default RTT threshold and on
the first key. Sweep parser coalescing `0/1/3ms` and prediction thresholds only
after traces quantify their contribution and all secret/alternate-screen/
misprediction safeguards remain intact.

## 9. Hardware-specific execution

### 9.1 M4 and M5

Use:

- Instruments Time Profiler and System Trace;
- `OSSignposter` intervals/events carrying the campaign correlation ID;
- Core Animation and Metal System Trace;
- actual drawable/presentation callbacks and, for a true photon claim, a
  high-speed camera or photodiode;
- per-thread QoS, P/E-core residency, migrations, wakeups, and main-thread
  stalls;
- actual 60/120Hz display mode and frame phase;
- unified-memory, physical-footprint, GPU-residency, and thermal/power time
  series; and
- identical A/Bs on each physical SKU.

Latency-critical GUI/input/render work should communicate
`userInteractive` intent. Search/indexing, cold reflow, artifact hashing, and
maintenance are candidates for lower QoS. Avoid priority inversion: a
user-interactive consumer must not wait on a utility/background producer.

For the local 10P+4E M4 Pro, the first bounded CPU-worker sweep is
`1/2/4/8/10`, with QoS and actual residency recorded; 14 workers is not the
presumed optimum. Base M5 has up to four performance and six efficiency cores,
while M5 Pro/Max uses a two-die Fusion Architecture with up to six super cores
and twelve performance cores. M5 is therefore not “M4 but faster.” Base M5 and
M5 Pro/Max need separate worker, queue-handoff, frame-pacing, and unified-memory
evidence, and the OS remains responsible for physical core placement.

### 9.2 `trj` Threadripper

Use:

- `perf stat`, `perf record`, scheduler/migration events, and off-CPU/eBPF
  traces;
- `numactl --hardware`, `numastat`, LLC/CCD topology, `perf c2c`, and IRQ/
  network queue placement;
- compact/pinned connection-parser-mux placements compared with the normal
  scheduler;
- SMT-on/off or one-thread-per-core A/Bs for latency-critical lanes;
- bounded persistent pools sized by useful work and cache locality; and
- explicit context-switch, migration, remote-memory, cache-miss, throughput,
  and tail-latency evidence.

The likely Threadripper win is isolation and locality, not maximal fanout.
Connection decode, mux mutation, and one terminal are inherently ordered.
Parallelism belongs across independent panes or reflow chunks only when the
chunks are large enough to amortize scheduling and cache costs.

## 10. Campaign graph

```text
ft-interactive-systems-performance-4tenz
|
+-- .1 truth/proof contract (materialized; release-artifact closure waits on .10/.11)
|   +-- .10 burn down the 21-site Cx release-verifier regression
|   +-- .11 reconcile four unresolved/unmirrored attestation producers
|       (both block only .1 closure, not the .2/.3/.4 measurement lanes)
|
+-- .2 real keypress trace
|   +-- .5 client/server priority and resize-coalescing A/B
|   +-- .6 short-lock terminal snapshot and damage-only delta A/B
|
+-- .3 live Metal resize/zoom rig
|   +-- .7 topology-aware resize/reflow scheduling A/B
|   +-- .8 damage-aware invalidation, pacing, and zoom A/B
|
+-- .4 real 4h/24h/72h aged-session rig
|
+-- .9 M4/M5 and Threadripper target certification
    (depends on .2, .3, and .4)
```

Related existing lanes are linked, not replaced:

- `ft-tf6g3.3`, `.3.8`, and `.3.9`: renderer SLO target evidence;
- `ft-7h5da.10.3` and `.10.3.2`: deterministic/production load-rig truth gap;
- `ft-35zzw`: true viewport-first background reflow;
- `ft-uyt88`: persistent mux buffering with unresolved harness ambiguity;
- `ft-uzj0s`: heavy-burst proxy bench; and
- `ft-1itzl`: mismatched app-bundle CLI/GUI identity and robot-state hang.

## 11. First execution sequence

1. Use the reconciled truth contract and preserve all non-claims. Close `.1`
   only after `.10` and `.11` clear the independent required-category
   attestation verifier; `.2`, `.3`, and `.4` are intentionally free to start
   meanwhile.
2. Add the correlation schema and minimally invasive queue/lock/presentation
   telemetry.
3. Run quiet M4→`trj` direct-LAN and current-route baselines.
4. Reproduce lag under 50-pane burst, 200-pane resize, and four-hour aged
   conditions.
5. Attribute p95/p99 tails to stages before selecting a code change.
6. Run one-lever A/Bs in the dependency order above.
7. Preserve every losing result and retry predicate.
8. Promote only retained changes through focused proof, broad proof, 4h screen,
   and target-class certification.

The campaign is successful when the real interaction is fast and visually
stable on the named machines, the result survives aged sessions, and the
evidence is strong enough to fail closed—not when a proxy benchmark alone
reports an attractive number.

## 12. Primary external references

- Apple, [Tuning your code's performance for Apple silicon](https://developer.apple.com/documentation/apple-silicon/tuning-your-code-s-performance-for-apple-silicon/)
- Apple, [Recording performance data with signposts](https://developer.apple.com/documentation/os/recording-performance-data)
- Apple, [Achieving smooth frame rates with a Metal display link](https://developer.apple.com/documentation/metal/achieving-smooth-frame-rates-with-a-metal-display-link)
- Apple, [M5 architecture overview](https://www.apple.com/newsroom/2025/10/apple-unleashes-m5-the-next-big-leap-in-ai-performance-for-apple-silicon/)
- Apple, [M5 Pro/Max Fusion Architecture overview](https://www.apple.com/newsroom/2026/03/apple-introduces-macbook-pro-with-all-new-m5-pro-and-m5-max/)
- AMD, [Ryzen Threadripper PRO 5000 WX-Series announcement](https://www.amd.com/en/newsroom/press-releases/2022-3-8-new-amd-ryzen-threadripper-pro-5000-wx-series-proc.html)
