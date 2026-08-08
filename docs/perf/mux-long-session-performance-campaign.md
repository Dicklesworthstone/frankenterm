# Real Mux Interaction, Resize, and Long-Session Performance Campaign

**Status:** active investigation and optimization program
**Opened:** 2026-07-27
**Campaign epic:** `ft-interactive-systems-performance-4tenz`
**Product-convergence epic:** `ft-interactive-swarm-product-convergence-7xqz4`
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
| `input_latency_bench.rs` | Legacy `proxy_only` framework/DTO overhead for synthetic, caller-labelled producer/clock timestamps | Production input latency, observer effect, or any live keypress stage |
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
- The legacy `input_latency` path is permanently `proxy_only`: its
  caller-labelled stages are not wired into the production
  AppKit/mux/network/PTY/parser/renderer/presentation path. `PtyRead` denotes a
  PTY master/reader read boundary and does not by itself establish causal
  application echo; `GpuPresent` is a caller-recorded operation marker, not
  display scanout or photons. Producer and clock-domain IDs remain unverified
  labels. This module implements no registry, evidence-bundle schema/content
  binding, or clock calibration that could establish their identity.
- The collector and report evidence path (schema v1/schema v4) now fails closed
  on empty or incomplete windows, duplicate stage writes or authority-bearing
  wire keys, duplicate/reserved/unallocated measurement IDs, mismatched
  asserted clock domains within a measurement, regressing timestamps, invalid
  or oversized capacities, and sticky ID exhaustion. Invalid evidence produces
  all-or-zero summaries rather than filtering bad samples into a pass. The
  budget wire rejects malformed or noncanonical encodings before a verdict
  exists. A decoded or in-memory semantically inadmissible budget produces a
  typed non-pass verdict without erasing summaries derived from otherwise valid
  evidence. Decoding is bounded to 65,536 samples and five adjacent stage
  budgets.
- `InputLatencyBudget.regression_threshold` is serialized as a canonical
  `0x`-prefixed 16-lowercase-hex-digit IEEE-754 payload. Decimal, numeric,
  uppercase, and malformed encodings are rejected; canonically encoded but
  semantically inadmissible IEEE-754 payloads retain their exact bits until
  typed validation. Effective budgets are
  computed by exact integer scaling of the represented finite value, so
  decimal round-trip drift or `f64` rounding above `2^53` cannot mint a
  one-microsecond false pass. Approximate ratio fields were removed.
  `serde_json`'s `float_roundtrip` feature remains defense in depth only; it is
  not gate or evidence authority.
- `InputLatencyReport`, `BudgetCheckResult`, and `BudgetCheckDetail` are public
  DTO types whose derived fields are private/getter-only; they implement
  `Serialize`, not `Deserialize`. Only report/verdict envelopes carry
  `proxy_only`; a standalone detail has no authority. A report alone is not
  replay-verifiable: retained proxy evidence must include the serialized source
  collector, exact budget, and a future external registry content-bound to both
  inputs. These repairs prevent false-green proxy diagnostics; they do not
  establish live keypress latency, observer effect, cross-host clock
  calibration, presentation, or physical display timing. Those remain trace-v2
  and isolated-target-run responsibilities under `.2.1`–`.2.8`.

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
   PTY echo can then produce a second later delta. The forced response's
   `InputSerial` proves only protocol dispatch and supplies a terminal-sequence
   fence; it is not K7 PTY/application echo evidence and must never be timed or
   named as such.
8. Client unilateral processing hydrates line caches, records the dispatch
   fence, and reconciles prediction only against later authoritative row state.
   Reordered acknowledgement metadata remains admissible even when its stale
   surface content is rejected. The client then scans hyperlinks and emits a
   local mux notification.
9. Each TermWindow subscriber schedules a main-thread task before it knows
   whether the pane is visible in that window.
10. macOS invalidation is paced by the canonical timer fallback:
    `max_fps` is validated in `1..=1000`, then converted with ceiling division
    to a strictly positive integer-millisecond interval. With the default
    `max_fps = 60`, event phase alone can add nearly one 17ms timer interval;
    this fallback is not presentation-phase aligned.

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

### 2.2.1 Live snapshot and damage reconciliation (2026-08-07)

This is the retained source reconciliation for
`ft-interactive-systems-performance-4tenz.6.1`. It is a call-graph audit, not a
native performance result or a renderer cutover. No M4/M5, `trj`, LAN, visual,
or latency claim follows from it.

The current local producer-to-present path is:

```text
Terminal mutation
  -> TerminalState sequence + per-Line sequence
  -> PaneOutput/window invalidation
  -> paint_pane
  -> get_changed_since_with_source_fence(renderer-only sequence fence)
  -> DirtyLineBitmap marks
  -> LocalPane::with_lines_mut_and_apply_hyperlinks(visible stable range)
  -> shape hash + LFU line-quad lookup
  -> on miss: shaping + glyph/atlas + quad construction
  -> draw/submit/present
  -> exact DamageGeneration settlement after synchronous present success
```

Dirty discovery captures the source sequence and changed rows under one pane
operation. Hyperlink normalization and line rendering now share one terminal
guard rather than reacquiring it for a second visible-range traversal. The
important remaining critical section is still long:
`terminal_with_lines_mut_and_apply_hyperlinks` invokes the GUI's `LineRender`
callback while the terminal guard is alive, and that callback performs line
hashing, cache-key construction, shaping, glyph/atlas work, and quad
construction. The PTY parser needs the same terminal mutex. This tranche
removes redundant locking/traversal; it does not implement the short-lock
immutable snapshot required by `.6.3`.

The current remote path is:

```text
server PerPane::prepare_surface_changes
  -> checked full-viewport stable-row range
  -> get_changed_since_with_source_fence(legacy baseline)
  -> get_lines(full viewport range)
  -> filter cloned lines to dirty rows
  -> fetch/compress/deduplicate cursor row best-effort
  -> codec/connection delivery
  -> client validates bounded SerializedLines resources
  -> coordinate image RPCs (bounded concurrency/locators/deadline)
  -> decoded-image verification on a dedicated two-thread pool
  -> RenderablePane cache/prediction apply; unresolved image rows stay stale
  -> ClientPane::with_lines_mut_and_apply_hyperlinks
  -> clone requested rows + allocate mutable-reference vector
  -> same GUI line loop, caches, draw, submit, and present
```

Remote shape appdata is written back only when the rendered clone remains
content-equal to the authoritative cached row, and each `TermWindow` now owns a
distinct cache token so appdata from another window cannot alias its LFU entry.
Image admission authenticates canonical decoded identity and accounts decoded
bytes/frames, but the transport still performs coordinate-based N+1 lookups.
There is no snapshot-owned batch identity, cross-request singleflight, or
decoded/GPU/in-flight unified budget yet; `.6.7.1` owns that architectural gap.

The transactional `PaneRenderBeginSnapshot` and delivery-coordinator state
machines in `sessionhandler.rs` are compiled and tested but explicitly
`dead_code` pending live ownership transfer. The production legacy
`PerPane::compute_changes` path still advances its baseline without an
application acknowledgement. It is therefore incorrect to describe the
transactional path as the live server authority.

| Substrate or claim | Live classification | Production effect | Exact gap |
|---|---|---|---|
| terminal `SequenceNo` plus `Pane::get_changed_since` | wired, authoritative but single-baseline | discovers changed stable rows for local GUI and server | no bounded multi-consumer journal, overflow epoch, acknowledgement, or resync identity |
| GUI `DirtyLineBitmap` and source counters | wired, partial | marks row damage and attributes clean cache-hit accounting | paint and cache lookup remain independent of the bitmap; every visible line still builds a cache key, so this is not a sparse iterator |
| `DamageGeneration` and presented-frame settlement | wired, authoritative for GUI damage clearing | retains damage on failed/stale synchronous presentation and clears only an exact successful generation, including the initial whole-screen damage | synchronous present return is not GPU completion, scanout, or key-to-photon evidence; the generation is not shared with terminal/server/client content |
| LFU `line_quad_cache` | wired | valid cache hits reapply retained layers and avoid shaping/glyph/quad reconstruction | global generations still invalidate broadly; cache is not the proposed row-indexed ownership model |
| `render::per_row_quad_cache` | test-only foundation | pure invalidation-plan tests | no live paint consumer and no owned per-row quad vectors |
| GUI `TerminalStateTripleBufferRegistry` | dormant/test foundation | explicit APIs and tests can publish metadata and derive watchdog health | no production producer, frame/status consumer, or renderer acquisition path; payload also lacks lines, attributes, images, hyperlinks, selection, IME, and render geometry |
| `DifferentialCellStream` GPU delta | compiled test-only/dormant | unit tests exercise CPU diff and ring policy | types and implementation are `allow(dead_code)` and WebGPU paint has no consumer |
| server transactional render attempt/coordinator | partial/dormant | model and unit tests cover preparation/settlement identities | legacy `compute_changes` remains live; no application-ACK-owned commit path |
| server legacy render delta | wired, coarse | uses checked ranges, a source fence, saturation requery, and rollback/redirty on failed preparation or enqueue | clones the complete viewport before filtering; has no client application ACK and no autonomous retry of a successfully enqueued but unapplied delta |
| client `RenderablePane` and `ClientPane` adapter | wired, partial | bounds line/image resources, caches received rows, reconciles prediction, retains safe shape appdata, and serves GUI lines | GUI adapter clones the requested range and allocates a mutable-reference vector; coordinate image hydration lacks global singleflight/batch ownership |
| GUI renderer behavior tests | partial topology | pure helpers and library-owned surfaces execute under ordinary tests | the binary-owned `TermWindow`/renderer modules are declared with `test = false`; `.6.8.1` owns making their production behavior executable under normal gates |

The audit also found that GUI renderer damage discovery had reused
`Selection::seqno` as its render watermark. With no active selection that
watermark can remain at zero or at the last selection operation, causing old
PTY rows to be rediscovered, re-marked, and assigned new damage generations on
later paints. The corrective tranche gives renderer discovery a distinct
per-pane fence, captures the source sequence before the dirty query so races
can repeat but never skip changes, retains the selection fence only while a
selection exists, checks stable-row range arithmetic, gives viewport movement
its own full-pane invalidation source, and drops GUI pane state on pane removal.
The same pass now rejects zero/MAX shape-cache sentinels, namespaces cached
line state by `TermWindow`, clears appdata when implicit-link epochs change,
and makes overlay search results conditional on exact run/source/geometry/range
identity. These are source-candidate corrections, not native performance or
visual proof; they remain unqualified until the required remote and target
gates pass.

The non-duplicative implementation map is:

| Bead | Extend or replace | Do not mistake for the solution |
|---|---|---|
| `.6.2` | define one immutable `RenderSnapshot` payload and generation contract spanning terminal-visible state, local render, and remote delta needs | the persistence-oriented six-field `session_pane_state::TerminalState` mirror |
| `.6.3` | add a short-lock local snapshot producer and cut `LineRender` over atomically, using the existing triple-buffer mechanics only if the measured ownership/bounds fit | merely publishing richer metadata while `LocalPane::with_lines_mut` remains the live consumer |
| `.6.4` | make terminal/mux damage a bounded, multi-consumer correctness protocol with overflow-to-full-resync and sequence-exhaustion semantics | GUI `DirtyLineBitmap`, which is a frame-local render hint |
| `.6.5` | make server production gather coalesced dirty ranges from the canonical snapshot/journal and deduplicate cursor materialization | filtering after `get_lines` cloned the full viewport |
| `.6.6` | commit a minimal ordered input acknowledgement independently from later echo damage where traces prove duplicate material work | acknowledging PTY/application delivery or echo before it happened |
| `.6.7` | apply sparse rows and their hyperlink/image/cache metadata directly in the client and remove full-range GUI materialization on the admitted path | retaining stale line references across snapshot or connection generations |
| `.6.8` | prove the combined state pipeline with model, property, fuzz, failure, and native visual/IME/a11y evidence | unit tests or a static call graph alone |
| `.6.9` | adjudicate every lever independently and then the retained combination on real workloads | bundling losing or unmeasured levers behind an aggregate improvement |

Sequence-number saturation still needs a bounded system protocol in `.6.4`.
Terminal sequence numbers saturate at `usize::MAX`, after which ordinary
`changed_since(MAX)` ordering cannot identify later mutations. The local GUI
and legacy server now preserve correctness by querying from `SEQ_ZERO` while
the source is saturated, at the cost of treating the requested range as dirty
on every subsequent query. The dormant transactional server path instead
closes before ambiguous identity. None of these is the bounded epoch/resync
transition required to call sparse multi-consumer damage complete.

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

The current classified-input headless renderer harness is only a
`proxy_only` regression substrate. Its v2 evidence contract must never set a
physical target verdict, and the retained v1 macOS result remains negative
evidence rather than a baseline upgrade. RQ-S2/RQ-S3 admission still requires
the correlated native-event, mux/PTY, production-window, presentation, and
display/physical timing path defined by this campaign.

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

### Panic-containment negative-evidence ledger

The shipped interactive profile is unwind-capable, but that alone does not
make a raw `catch_unwind` a valid recovery contract. The production audit for
`ft-interactive-systems-performance-4tenz.13.1/.13.2` classifies the retained
boundaries as follows:

- mux pane, focus, resize, retirement, subscriber, tmux, registration,
  storage-writer, MCP completion/request, core dataflow/recording/search/task,
  sharding rollback, and promise-waker recovery all route through
  `frankenterm_sigpipe::catch_recoverable` or its per-poll future counterpart;
- the raw catch inside `frankenterm-sigpipe` is the canonical implementation,
  not a bypass;
- the Windows `wnd_proc` catch is a documented fatal no-unwind FFI fence that
  exits instead of continuing; and
- remaining direct catches are test-only panic/poison harnesses. They are not
  evidence for shipped recovery.

Re-run `git grep -n -E 'catch_unwind|\.catch_unwind\(' -- '*.rs'` whenever a
new production boundary is added. A new continue-serving match must either use
the canonical helper with a finite site or document why it is fatal/rethrowing.

## 7. Measurement and keep-gate protocol

The `running-the-gauntlet-on-your-rust-port` and
`extreme-software-optimization` disciplines apply:

1. Build/profile with `release-perf`, debuginfo line tables, and forced frame
   pointers where the profiler requires them. `release-perf` inherits directly
   from the workspace's canonical `release` profile and repeats `panic = "unwind"`
   explicitly, matching the shipped interactive panic contract without custom
   profile chaining. Do not compare it to an aborting GUI/mux artifact or treat
   unit-profile catch behavior as release recovery proof.
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

## 10. Target product promise

The campaign is complete only when FrankenTerm behaves like an operator-grade
interactive system, not merely when a benchmark becomes faster. The user
promise is:

> A focused remote pane feels local on a healthy LAN, remains responsive while
> the rest of a large fleet is busy, resizes and zooms without ugly or
> internally inconsistent frames, does not progressively rot during a
> workday or multi-day run, explains the cause when it cannot meet that
> promise, and recovers without losing the operator's final intent.

That promise has seven inseparable parts:

1. **Immediate interaction.** A key, paste, mouse action, or IME commit on the
   focused pane has a protected bounded path from native input to a correct
   presented response.
2. **Continuous visual quality.** Resize, zoom, display migration, font-scale
   changes, selection, cursor, images, Unicode, RTL, and IME remain coherent
   at intermediate frames as well as at final convergence.
3. **Fleet-scale fairness.** Background panes can make progress, but output,
   search, capture, maintenance, resync, and a resize storm cannot silently
   starve focused interaction.
4. **Long-session stability.** Latency, memory, GPU residency, cache churn,
   queue age, and thread count do not acquire positive slopes merely because a
   session is old.
5. **Truthful operation.** The installed GUI, CLI, mux server, config, and
   evidence schema identify themselves atomically. A mismatch or unsupported
   measurement is a typed degraded state, not an attractive false result.
6. **Actionable diagnosis.** Operators can ask why the session is slow and get
   a bounded, privacy-safe explanation with the dominant stage, confidence,
   current envelope, and next safe action.
7. **Safe adaptation.** Hardware-aware tuning is measured and reversible.
   FrankenTerm never guesses a machine profile, silently changes correctness,
   or turns one successful run into a universal default.

### 10.1 User journeys that must work

The following are product acceptance journeys, not optional demos:

| Journey | Required observable outcome |
|---|---|
| First launch after installation | GUI, bundled CLI, mux protocol, source/build identity, config provenance, and renderer capability agree; mismatch is diagnosed before measurement |
| One remote shell on the direct LAN | Typing, paste, cursor motion, and shell echo remain within the frozen interaction SLO without prediction hiding a slow real path |
| Twenty-agent daily project | The focused agent stays responsive during compile output, searches, capture, and routine pane churn |
| Fifty-pane burst | Aggregate 1 MB/s output cannot strand input or final output; degraded behavior is visible and converges exactly |
| Two-hundred-pane mission | Continuous resize and tab/split changes preserve focused interaction, final geometry, and visual correctness |
| Full workday | After four or more hours, interaction and render tails remain inside the admitted envelope with no harmful resource slope |
| Multi-day control plane | At 24 and 72 hours, reconnects, maintenance, indexing, and cache aging do not cause leak-like growth or correctness drift |
| Network interruption | Disconnect, reconnect, resync, and final-intent convergence are bounded, auditable, and do not duplicate or reorder input |
| Display and text changes | 60/120 Hz, zoom, DPI/display move, Unicode, emoji, combining marks, RTL, IME, images, and accessibility geometry remain correct |
| Performance incident | `ft` can collect a bounded incident bundle, identify likely dominant stages, state uncertainty, and recommend a reversible next step |

### 10.2 Explicit non-goals

- Do not promise zero network latency or disguise a slow remote application
  with unsafe prediction.
- Do not privilege focused input by dropping non-supersedable output or
  violating terminal ordering.
- Do not use every available core merely because it exists.
- Do not ship architecture-specific SIMD before profiles show the relevant
  kernel is material.
- Do not make a synthetic, headless, submit-time, or loaded-host result stand
  in for a target-class presentation claim.
- Do not require users to understand internal queue names to diagnose ordinary
  performance problems.

## 11. Intended runtime architecture

This section is a destination architecture. Each transition must be justified
by the traces and one-lever A/B protocol in Section 7.

### 11.1 Protected interactive fast path

The connection and mux surfaces need explicit service classes:

```text
interactive input and acknowledgement
  > correctness-critical control/resync
  > visible-pane output and resize convergence
  > non-visible output/capture/search/maintenance
```

The `>` notation expresses latency preference, not absolute starvation. Each
class needs:

- a bounded queue or bounded outstanding-work budget;
- enqueue timestamp, oldest age, depth, admitted/coalesced/rejected counters;
- a fairness quantum and maximum service gap;
- cancellation and shutdown semantics through `Cx`;
- an exact definition of which operations are supersedable;
- a final-state convergence invariant; and
- a saturation behavior that is visible to the operator.

Key bytes, ordered terminal output, protocol barriers, and the last resize
intent are never silently dropped. Intermediate resize intents, redundant
paint requests, duplicate cursor deltas, and optional high-volume telemetry
may be coalesced only under a proved equivalence contract.

FrankenTerm already contains deterministic `latency_stages` types for lane
scheduling, input rings, correlation, budget enforcement, formal invariants,
and instrumentation. Current code search finds these primarily in their module
and tests, not in the live AppKit/client/mux/render path. The campaign must
extend or integrate that substrate where its semantics fit. It must not create
a second unrelated scheduler model merely because the production wiring is
missing.

### 11.2 Coherent versioned terminal snapshots

Rendering and remote delta production should consume a coherent versioned
snapshot:

```text
terminal mutation under short lock
  -> immutable visible-state snapshot + generation
  -> lock release
  -> shape/damage/quad/delta work outside the terminal lock
  -> publish only if generation and pane identity are admissible
```

The snapshot contract includes visible lines, cursor, dimensions, palette,
selection, semantic zones, images, hyperlink identity, alternate-screen state,
input-method geometry, and a monotonic generation. It must define:

- which data are owned, shared, or copy-on-write;
- maximum lock hold and snapshot allocation budgets;
- how a stale snapshot is detected and discarded;
- whether an older complete frame may remain visible while newer work runs;
- how prediction and server deltas reconcile against generations; and
- how final state is proved byte/state equivalent to the reference path.

Triple buffering is useful only when it is live: producer, renderer, and last
known-good buffers must be wired to actual pane mutations and paint. DTOs and
synthetic tests alone do not satisfy this architecture.

### 11.3 Damage and frame production

Invalidation must describe cause and scope:

- cursor-only;
- one or more dirty row ranges;
- selection/IME/overlay only;
- surface projection changed but terminal grid did not;
- terminal grid/reflow changed;
- font metrics/DPI changed;
- atlas/glyph resource invalidation; or
- full unknown damage as a fail-safe.

The renderer should reuse line shaping and quads when the terminal content and
metric generation are unchanged, even if the window projection changes. A
same-grid resize should be able to reproject/reclip existing quads. A
grid-changing resize must publish a coherent viewport before cancelable cold
convergence.

On current macOS Metal paths, the pacing experiment should use the applicable
Metal display-link API and actual capability probe, with a fallback path for
unsupported systems. The existing VRR/display-pipeline abstractions are
inputs, not proof that the live window is display-link paced. Input-triggered
work may request an earlier paint within the display-link contract, but may not
busy-spin or defeat energy and thermal policy.

### 11.4 Persistent topology-aware execution

Resize/reflow work should use a persistent, bounded, `Cx`-aware execution
resource. A scheduling decision is a function of useful work:

```text
workers = f(independent_chunks, bytes, line complexity, cache locality,
            target latency, current pressure, QoS, thermal/power state)
```

not merely `available_parallelism()`.

On Apple silicon:

- communicate user-visible urgency using QoS;
- ensure high-QoS consumers do not wait on work intentionally demoted to
  utility/background;
- sweep bounded worker counts and record actual P/E residency;
- let the OS place work rather than hard-coding undocumented core IDs; and
- test responsiveness, energy, and thermal sustainability together.

On `trj`:

- keep the serialized connection/parser/mux critical path compact;
- sweep physical-core-only and SMT configurations;
- measure CCD/LLC locality, migration, remote memory, IRQ/network placement,
  cache misses, and context switches;
- spread only sufficiently large independent pane/reflow work; and
- reject fanout whose coordination and cache costs exceed useful work.

### 11.5 Long-session ownership discipline

Every long-lived cache, queue, subscription, worker, atlas, GPU resource,
scrollback tier, reconnect state, and background task needs:

- an owner and lifetime boundary;
- a cap or an evidence-backed reason it is naturally bounded;
- size/age/eviction/overflow counters;
- a shutdown and cancellation path;
- generation or connection identity where stale work is possible; and
- an incident-bundle representation that does not leak pane contents.

The soak lane must detect slopes and change points by subsystem. “RSS grew”
is insufficient; the artifact should attribute retained objects, mapped
regions, allocator classes, GPU residency, scrollback, caches, subscriptions,
threads, and queue backlog as far as platform APIs allow.

## 12. Operator performance control plane

Performance work becomes genuinely useful when the same truth is accessible
to a person and to an automation agent.

### 12.1 `ft perf doctor`

A side-effect-free preflight should report:

- running GUI, bundled CLI, mux-server, protocol, and source identity;
- target architecture, CPU topology, display mode, renderer/backend, power and
  thermal state;
- route/transport, endpoint, RTT/jitter/loss orientation;
- queue/trace feature availability and instrumentation overhead mode;
- session age, pane count, output rate, resource pressure, and current
  operating-envelope classification;
- whether each requested claim is measured, proxy-only, unavailable, skipped,
  or invalid; and
- typed remediation for mismatches or missing prerequisites.

It must be fast, bounded, redacted, machine-readable in JSON/TOON, and useful
in human CLI output. It must not restart the GUI, rewrite config, change
affinity, or enable expensive tracing.

### 12.2 `ft perf trace`, `status`, and `explain`

The control plane needs three distinct operations:

- `trace`: collect a bounded correlated sample around an operator action or
  reproduction window;
- `status`: show current rolling SLOs, queue pressure, dominant resources,
  degraded state, and evidence freshness; and
- `explain`: convert trace/rolling evidence into ranked causal candidates,
  confidence, falsifiers, and the next safe measurement or tuning action.

The existing `ft robot perf slo-status` is a starting surface. New commands
must share schema and semantics across human CLI, Robot Mode, MCP, and incident
bundles rather than each inventing a different truth model.

Raw text, key contents, secrets, and pane payloads are excluded by default.
Correlation IDs, durations, type names, counts, hashes, sizes, and redacted
pane/session identities are normally sufficient. The overhead budget and
sampling state must be reported in every artifact.

### 12.3 GUI overlay

An opt-in overlay may show:

- current input-to-present estimate and whether it includes a physical display
  boundary;
- frame interval/jank;
- focused-pane and connection queue age;
- output and resize pressure;
- session age and resource trend;
- active degraded mode; and
- a short reason such as `server_dispatch_wait` or `frame_phase`.

The overlay is not enabled by default, must not create a self-reinforcing
repaint loop, and must use the same measured schema as CLI/Robot Mode.

### 12.4 Measured autotuning with receipts

Autotuning has two steps:

1. a side-effect-free planner proposes a hardware/workload profile, expected
   benefit, evidence, risk, and rollback; then
2. an explicitly authorized apply action changes only declared settings and
   emits a receipt with previous values, new values, build/config identity,
   expiry/revalidation rules, and rollback command.

Candidate dimensions include worker bounds, service quanta, parser
coalescing, frame-rate range, cache budgets, and background QoS. A target
profile is promoted only after the named hardware matrix. Unknown M5 or AMD
SKUs start from conservative portable defaults and measure rather than inherit
an adjacent SKU's profile.

No autotuner may:

- weaken terminal semantics, secret safeguards, visual correctness, or release
  proof;
- change network routes, system-wide affinity, or power settings silently;
- claim causal certainty from one noisy run; or
- retain a profile after binary/config/hardware/display identity changes
  without revalidation.

## 13. Graceful overload, disconnect, and recovery

Real users eventually exceed an operating envelope. The correct outcome is
controlled degradation with exact recovery, not a hang or an invisible drop.

### 13.1 State machine

The runtime exposes a deterministic state machine:

```text
healthy
  -> pressured
  -> interaction_protected
  -> degraded
  -> recovering
  -> healthy
```

Transitions have thresholds, hysteresis, minimum dwell, reason codes,
timestamps, and receipts. Metrics include queue age, missed frame budget,
output rate, memory/GPU pressure, cache overflow, reconnect backlog, and
thermal state. The state is per relevant scope: pane, connection, window, and
process.

### 13.2 Permitted degradation order

Subject to the formal correctness contract, the preferred sequence is:

1. coalesce duplicate paint and superseded resize intents;
2. reduce optional telemetry and overlay cadence;
3. defer non-visible shaping, cold reflow, indexing, and maintenance;
4. reduce background-pane presentation frequency while preserving state
   ingestion and final convergence;
5. cap optional caches and evict by a deterministic policy;
6. surface a visible degraded state with reason and recovery progress; and
7. reject new optional work with a typed response if capacity is exhausted.

Input bytes, ordered output, approvals, protocol barriers, audit records, and
the final pane/resize state remain correctness-critical.

### 13.3 Reconnect and exact convergence

Disconnect/reconnect testing must prove:

- no input is ambiguously replayed;
- acknowledged and unacknowledged operations are distinguishable;
- connection generation prevents stale deltas and tasks from landing;
- resync is bounded and cannot permanently monopolize the interactive lane;
- notification-queue saturation cannot strand the final pane state;
- visible progress and retry/backoff state are available to the operator; and
- the post-recovery terminal state matches an authoritative reference.

Existing reconnect-window, degraded-mode, operating-envelope, and render
governor lanes should be integrated by dependency or relation. This campaign
does not duplicate their contracts.

### 13.4 Power and thermal sustainability

Responsiveness that thermal-throttles after ten minutes is not a successful
optimization. Target runs record energy impact, wakeups, idle GPU/CPU, and
thermal transitions. Display-link pacing, QoS, worker counts, background
refresh, and cache policy must have both latency and sustainability gates.
Accessibility settings such as reduced motion or limited frame rate are part
of the target contract, not configuration noise.

## 14. Verification architecture

The campaign uses a proof pyramid. Higher layers do not erase failures or
substitute for lower layers.

### 14.1 Deterministic unit and property proof

Required properties include:

- scheduler boundedness, fairness, maximum service gap, and final convergence;
- no key loss, duplication, or reordering;
- latest-intent resize coalescing;
- generation-safe snapshot publication;
- exact reference equivalence for damage-only deltas;
- cancellation and shutdown at every await/resource boundary;
- hysteresis and recovery of degraded states;
- artifact schema round-trip, redaction, and version compatibility; and
- deterministic autotune planning and rollback receipts.

Model checking and Loom-style concurrency tests should cover the smallest
scheduler, queue, generation, disconnect, and cancellation state spaces.
Existing `latency_stages` formal invariants should be extended where they map
to the production contract.

### 14.2 Fuzz and differential proof

Fuzz:

- protocol ordering, partial frames, corrupt lengths, reconnect boundaries,
  and queue saturation;
- dirty-range/cursor deduplication against full viewport materialization;
- resize/zoom/scrollback interleavings;
- Unicode, combining, bidi, emoji, image, hyperlink, and alternate-screen
  mutations;
- snapshot generations and stale-work cancellation; and
- performance artifact parsers and schema migration.

The old/reference and candidate paths run from identical seeds where possible.
Final terminal state, emitted bytes, cursor, dimensions, semantic zones,
images, and visual corpus results must agree.

### 14.3 Native visual and interaction proof

The native rig must drive actual AppKit events and live resize/zoom gestures,
exercise a production remote mux and PTY, and observe actual Metal
presentation. It must retain:

- event/trace records;
- presented-frame timing and dropped/duplicated frame information;
- synchronized screenshots or video at defined checkpoints;
- image-diff and semantic visual-oracle results;
- cursor, selection, IME, accessibility geometry, and focus state;
- route/display/build/config identity; and
- an explicit statement of whether photons were physically measured.

No visual gate may be reduced to “the final screenshot looks plausible.”
Intermediate-frame coherence is part of the resize promise.

### 14.4 Soak and chaos proof

The 4h/24h/72h runner uses deterministic seeds and resumable phase journals.
It includes:

- quiet and interactive periods;
- sustained and burst output;
- resize/zoom/display changes;
- pane/tab/window churn;
- search, capture, workflow, and maintenance overlap;
- route interruption, mux restart/reconnect, and backpressure;
- atlas-near-cap and glyph-diverse phases;
- memory and GPU pressure; and
- graceful shutdown and artifact finalization.

A failed or interrupted soak is an artifact with a terminal reason, last
successful checkpoint, partial time series, and retry recipe. It is never
silently discarded.

### 14.5 Statistical adjudication

Before each target run, freeze:

- sample size and warmup;
- primary and guardrail metrics;
- comparison method and confidence interval;
- noise classification and retry limit;
- slope/change-point method for soaks;
- materiality threshold; and
- keep/reject/indeterminate decision rule.

The baseline database is append-only or content-addressed, binds every result
to source/binary/config/hardware/workload identity, and preserves losing
results. A regression alert names the affected stage and workload rather than
only a global score.

### 14.6 CI and target tiers

| Tier | Cadence | Authority |
|---|---|---|
| Fast deterministic | each focused change/PR | unit, property, model, schema, redaction, small differential tests |
| Workspace remote | each candidate before merge/close | strict remote check, Clippy, tests, formatting, and focused benchmarks |
| Nightly integration | nightly | production mux/PTY scenarios, saturation, reconnect, visual corpus where target is available |
| Weekly target | weekly or hardware reservation | M4/M5/Threadripper native live workload and 4h screen |
| Release qualification | release candidate | selected 24h/72h, full user journeys, attestation and verifier replay |

Strict remote RCH Cargo proof and native target execution are separate
statuses. A remote Linux build cannot mint a macOS presentation artifact, and
a local native runtime result cannot replace required remote Cargo proof.

### 14.7 Canary and rollback

Risky scheduling/render changes should support a short-lived feature gate for
controlled A/B and rollback during the campaign. Before release promotion:

- the candidate is the default in a canary cohort or shadow comparison;
- rollback is exercised, not merely documented;
- schema readers tolerate the transition according to the declared version
  policy;
- incident signals identify the candidate configuration; and
- obsolete gates are removed only through a separately reviewed cleanup, never
  hidden inside the performance change.

The repository's no-backward-compatibility policy does not excuse ambiguous
live rollback during an active performance experiment.

## 15. Full Beads work breakdown

The planning pass produced two linked umbrellas:

1. `ft-interactive-systems-performance-4tenz` remains the causal performance
   campaign.
2. `ft-interactive-swarm-product-convergence-7xqz4` is the product-convergence
   umbrella.

These are live graphs and their descendant totals change whenever the campaign
discovers or closes work. Do not copy an old count into a completion claim. The
2026-08-05 reality audit observed 287 descendants under the performance root
and 170 under the product-convergence root; those values are a dated snapshot,
not an implementation percentage. Query the current graph with
`bv --robot-graph --graph-root <root-id> --graph-depth 0` before reporting it.

Every new implementation leaf carries Background, Technical Approach,
Success Criteria, Test Plan, Observability/Artifacts, and
Considerations/Non-claims. The 14 field-journey leaves use the equivalent
Background/User Value, SETUP/ACT, Success/ASSERT, Test Variants/TEARDOWN,
Observability/Artifacts, and Considerations/Non-claims contract. Parent epics
close only after every required child and artifact is terminal.

### 15.1 Causal performance graph

```text
ft-interactive-systems-performance-4tenz
|
+-- .1  campaign truth and proof boundary
+-- .10 repair 21 Cx release-verifier sites
+-- .11 reconcile four attestation producers
|
+-- .2  real input-to-present trace (8 leaves)
|   +-- trace identity/clock/privacy schema
|   +-- bounded flight recorder and platform markers
|   +-- AppKit/GUI/client instrumentation
|   +-- server/mux/terminal/PTTY/parser instrumentation
|   +-- client apply/render/presentation instrumentation
|   +-- causal-echo/fault harness
|   +-- isolated Mac-to-trj runner
|   +-- frozen statistical baselines and attribution
|
+-- .3  native resize/zoom rig (8 leaves)
|   +-- scenario contract and isolated bundle identity
|   +-- native event driver and Metal presentation capture
|   +-- transient/final semantic and visual comparators
|   +-- headless adapter, verify-the-rig canaries, M4 baseline
|
+-- .4  production aged-session rig (9 leaves)
|   +-- existing-substrate audit and deterministic workload corpus
|   +-- resource ownership/evidence schema
|   +-- resumable Cx-aware runner and real fault/maintenance phases
|   +-- slope/change-point analysis
|   +-- separate 4h, 24h, and 72h artifacts
|   +-- incident capture and minimization
|
+-- .5  mux scheduling/backpressure (12 leaves)
|   +-- PDU/service-class formal contract
|   +-- bounded client scheduler and server dispatch fairness
|   +-- buffering/flush and mux-main QoS experiments
|   +-- P0 level-triggered PaneOutput final-convergence repair
|   +-- resize coalescing and early GUI window routing
|   +-- parser, prediction, and secure route A/Bs
|   +-- saturation/fairness/final-state proof
|
+-- .6  coherent snapshots and sparse damage (9 leaves)
|   +-- live-source reconciliation and snapshot contract
|   +-- short-lock renderer cutover and damage journal
|   +-- sparse server delta, minimal key ack, sparse client apply
|   +-- differential/model/fuzz/native proof and final A/B
|
+-- .7  resize/reflow/topology (11 leaves)
|   +-- measured cost model and persistent bounded executor
|   +-- unified tab/pane transactions and active-first fairness
|   +-- actual atomic viewport-first/cancelable cold convergence
|   +-- profile-admitted clone/wrap work
|   +-- separate Apple and trj topology sweeps
|   +-- chaos proof and selected defaults
|
+-- .8  render/pacing/zoom/cache (12 leaves)
|   +-- mandatory live-call-graph reconciliation
|   +-- invalidation taxonomy, duplicate removal, quad reprojection
|   +-- real Metal display-link and input-wake behavior
|   +-- deadline-aware zoom and aged glyph/atlas stability
|   +-- profile-gated GPU experiments only
|   +-- visual/a11y, power/thermal, and combined adjudication
|
+-- .9  target certification (10 leaves)
    +-- atomic preflight and candidate rollback
    +-- separate M4, base M5, M5 Pro/Max, and trj qualification
    +-- secure route and portable-correctness matrices
    +-- real workflow dogfood and verified attestation bundle
```

The `.5` and `.6` behavior changes depend on the `.2.8` real baseline.
Resize/reflow/render behavior depends on `.3.8`. The final per-target artifacts
depend on the 4h/24h/72h evidence and all individually admitted changes.
Instrumentation, corpus, model, and correctness work may proceed in parallel
where no retained baseline is required. To preserve that parallelism, the
workstream-epic links are contextual `related` edges; blocking authority lives
on the exact leaves that consume an upstream artifact. In particular,
`4tenz.5.5` is immediately actionable and is not blocked by trace-baseline or
QoS-contract work.

### 15.2 Product-convergence graph

```text
ft-interactive-swarm-product-convergence-7xqz4
|
+-- .1  product promise, journeys, SLOs, privacy, evidence (7 leaves)
+-- .2  atomic install, first run, config, update, rollback (9 leaves)
+-- .3  real multi-host workload and fault laboratory (7 leaves)
+-- .4  complete human/automation interaction experience (8 leaves)
+-- .5  degradation, connectivity, overload, recovery (8 leaves)
+-- .6  hardware-aware explainable autotuning (8 leaves)
+-- .7  latency clinic, live SLOs, incidents, support (8 leaves)
+-- .8  session continuity and disaster recovery (9 leaves)
+-- .9  visual quality, input, native accessibility (9 leaves)
+-- .10 memory, storage, power, thermal, visibility (10 leaves)
+-- .11 exact real-world operator journeys (14 leaves)
+-- .12 release verdict, canary, rollback, docs, retraction (9 leaves)
```

The 14 field journeys cover:

- clean-Mac first hour;
- two-agent everyday project;
- twenty-pane daily swarm;
- fifty-pane M4-to-`trj` loaded fleet;
- two-hundred-pane 4h/24h/72h target mission;
- rate-limit, compaction, approval, Verified Submit, and Attention;
- concurrent search/index/backup/GC/WAL/rules/incident maintenance;
- remote host unavailable at launch;
- LAN/Wi-Fi/tailnet roam and sleep/wake;
- live update and rollback;
- component crash and recovery;
- keyboard-only, VoiceOver, reduced-motion, and low-vision use;
- privacy-safe field lag diagnosis and replay; and
- version-pinned Codex, Claude, Gemini, and supported-agent dogfood.

The final authority is
`ft-interactive-swarm-product-convergence-7xqz4.12.9`. It cannot close until
the exact packaged candidate, every required journey, claimed hardware target,
human-review boundary, attestation verifier, rollback, and retraction rehearsal
pass.

Two cross-workstream gates are intentional. The complete real visual corpus at
`ft-interactive-swarm-product-convergence-7xqz4.9.1` blocks performance visual
qualification at `ft-interactive-systems-performance-4tenz.8.10`, and release
canary promotion at `ft-interactive-swarm-product-convergence-7xqz4.12.5`
blocks on packaged install/update/rollback qualification at `.12.2`. Robot
keyword suggestions are advisory only; all other suggestions from this pass
were rejected as transitive, independent, or semantically reversed rather than
used to over-serialize the campaign.

### 15.3 Existing lanes to reuse or link

The new graph does not erase existing work:

- `ft-tf6g3.3`, `.3.8`, and `.3.9`: renderer SLO and target evidence;
- `ft-tf6g3.30`, `.32`, `.33`, and `.40`: performance-gate,
  evidence-stream, visual-corpus, and centralized-test-log substrate;
- `ft-7h5da.10.3` and `.10.3.2`: deterministic versus production load-rig
  truth;
- `ft-7h5da.10.4.3` and `ft-tf6g3.14`: target-class resource-cockpit
  measurement and current skipped-not-proven boundary;
- `ft-35zzw`: true viewport-first background reflow;
- `ft-uyt88`: persistent mux buffering and syscall reduction;
- `ft-th8ag` plus `ft-gwzrm`, `ft-96uy6`, `ft-th8ag.1`, `ft-1g4mv`, and
  `ft-1l5n2`: live renderer telemetry and unwired production sources;
- `ft-3agml` and `ft-7h5da.7.8`: render/operating-envelope governor wiring;
- `ft-c4rn6`: reconnect-window storm and real unreachable-host proof;
- `ft-uzj0s`: heavy-burst proxy benchmark;
- `ft-1itzl`: app-bundle CLI/GUI identity and robot-state hang; and
- `ft-ujwwd`: observed GPU-firmware lockup investigation.

Use blocking dependencies only where an artifact or implementation is truly a
precondition. Use related edges for shared context and independently
progressing lanes. Closed substrate remains evidence to reuse, not a reason to
reimplement it.

## 16. Dependency and execution order

### Phase A — truth, immediate correctness, and atomic identity

1. Finish `4tenz.1`, `.10`, and `.11` for release-attestation truth.
2. Complete product-truth `.1.1-.1.7` and distribution identity
   `.2.1-.2.3`.
3. Fix the P0 notification-saturation final-convergence defect at
   `4tenz.5.5`; correctness work is not delayed for a performance baseline.
4. Start the trace schema/recorder, native scenario/identity, and soak
   reality-audit/schema work in parallel.
5. Reconcile existing trace, scheduler, snapshot, damage, display-link,
   evidence, logging, and visual substrate before creating new architecture.

### Phase B — isolated lab, clean first use, and real baselines

1. Complete the isolated multi-host lab and deterministic workload actors.
2. Complete idempotent setup, effective-config truth, component lifecycle, and
   the clean-Mac first-use tour.
3. Complete input tracing through actual presentation and the native
   resize/zoom visual rig.
4. Run identity-clean quiet and loaded M4-to-`trj` baselines over each named
   route.
5. Run the four-hour screen and freeze stage budgets, slope thresholds,
   workload seeds, and statistical keep rules.

No runtime candidate is promoted from a microbenchmark before these baselines.

### Phase C — remove serialized tail costs

1. Address the largest traced input queue/lock/fanout/frame-phase stage through
   `.5` and `.6`, one lever at a time.
2. Preserve correctness, fairness, and final convergence under saturation.
3. Run the four-hour screen after every candidate that changes queueing,
   snapshot ownership, or cache lifetime.
4. Keep losing experiments in the negative ledger.

### Phase D — make resize and zoom continuously correct

1. Integrate persistent bounded reflow execution.
2. Publish coherent viewport state before cancelable cold convergence.
3. Replace coarse damage and timer pacing only where native traces admit it.
4. Exercise intermediate-frame visual, cursor, IME, image, and accessibility
   oracles at 60 and 120 Hz.

### Phase E — make performance operable under pressure

1. Wire measured service classes, scoped degraded states, disconnected-input
   semantics, overload shedding, and exact recovery.
2. Deliver the canonical `ft perf doctor/trace/status/explain` and
   incident/support workflow.
3. Complete session continuity, storage/update recovery, and DR drills.
4. Add measured, explicitly authorized, reversible tuning profiles.
5. Complete native accessibility, international input, visual/motion safety,
   and resource/power/thermal behavior.

### Phase F — qualify targets and real workflows

1. Make the clean first-hour and two-agent four-hour journeys impeccable.
2. Advance through 20, 50, then 200 panes; a larger fleet never excuses a
   regression in the smaller journey.
3. Run M4, each available M5 class, `trj`, and the route matrix independently.
4. Run portable correctness gates on supported non-macOS paths.
5. Complete 24h/72h selected worst-case soaks, all 14 field journeys, and the
   required human reviews.
6. Qualify install/update/fault/canary/regression/retraction behavior against
   the exact packaged candidate.
7. Replay the offline verifier and close only the final go/no-go bead
   `.12.9`.

Unavailable physical M5 hardware produces an explicit unavailable/not-proven
result; it does not block improving or truthfully releasing M4/Threadripper
support, and it cannot be silently inferred from those machines.

## 17. Logging and evidence requirements

Every implementation and test bead must identify the logs it produces.
Minimum live fields are:

- schema version, trace/run ID, source/binary/config identity;
- host, process, connection, window, tab, pane, generation, and correlation ID
  in redacted/stable form;
- monotonic timestamp and clock domain;
- stage/event/reason code;
- queue class, depth, oldest age, admitted/coalesced/rejected counts;
- terminal-lock wait/hold, work units, rows/bytes/glyphs/quads;
- presentation/display identity and measured boundary;
- current pressure/degraded state and transition reason;
- resource samples and ownership category;
- instrumentation sampling/overhead state; and
- terminal outcome, artifact hashes, and retry predicate.

Logs are structured, bounded, rate-limited, and usable from JSON/TOON without
scraping prose. Test output uses the existing centralized test-log substrate.
Evidence bundles include a concise human summary but never make prose the sole
source of a release claim.

## 18. Definition of the promised land

The umbrella epic closes only when all of the following are true:

1. A real native event traverses the actual remote mux/PTY/application path
   and reaches an actual presented frame under a correlated, bounded trace.
2. The frozen input, burst, resize, reflow, visual, and long-session SLOs pass
   on the named qualified targets, or a target is explicitly scoped
   unavailable/not-proven.
3. Focused interaction remains protected under 20-, 50-, and 200-pane load
   without key/output loss, reordering, starvation, or stranded final state.
4. Continuous resize, zoom, DPI/display move, and full text/image corpus show
   no critical intermediate or final visual defects.
5. Four-, 24-, and 72-hour artifacts show no disallowed positive resource or
   latency slope and retain replayable incident evidence.
6. Apple and Threadripper tuning is independently measured, conservative on
   unknown SKUs, sustainable under thermal/power constraints, and reversible.
7. Operators and agents can diagnose the current state, collect evidence,
   understand degraded behavior, and apply or roll back an authorized tuning
   profile without reading source code.
8. Disconnect, overload, saturation, recovery, and rollback journeys converge
   exactly and visibly.
9. Fast, nightly, target, release, fuzz, visual, soak, and verifier gates are
   wired to authoritative artifacts and fail closed.
10. The negative ledger contains every rejected experiment with its numeric
    evidence and one retry predicate.
11. README/support claims match the generated attestation bundle.
12. The operator pilot reports that FrankenTerm feels predictably responsive
    and trustworthy in the real project workflows this system exists to run,
    and the objective artifacts agree.

The campaign is successful when these user-level properties are durable and
explainable—not when a proxy benchmark alone reports an attractive number.

## 19. Current reality checkpoint (2026-08-07)

This campaign is **PARTIAL and not release-qualified**. The repository now has
substantial bounded protocol, persistence, rendering, telemetry, and test
substrate, but the user-level result promised above has not yet been proved:

- PDU86-PDU90 ordered-window support remains capability-fenced rather than
  activated end to end. The server can produce the ordered snapshot and the
  client contains guarded application seams, but production still uses the
  legacy coherent pane-list path. Consequently, remote-domain tab order is not
  yet restored from an authoritative ordered snapshot after reconnect/restart.
- The client persistence substrate can store stable layout intent, but the live
  GUI does not yet own the complete stable layout-window identity, startup
  binding, reorder, and active-tab restore sequence required for mixed local and
  remote domains. Transient process-local numeric mux IDs are not an acceptable
  persistence identity.
- The flat pane-arena producer and client validation/application code exist,
  but the negotiated production path is still dormant. Its presence is not a
  latency or allocation win until capability activation and real workload
  evidence prove that the legacy path is no longer serving supported sessions.
- The current source candidate adds atomic dirty source fences, exact damage
  settlement, one-lock local hyperlink/render traversal, generation-safe
  overlay search publication, revision-bound decoded-image validation, bounded
  off-main image validation, generation-bound remote shape-appdata retention,
  and checked server rollback/range behavior. These are structural correctness,
  resource-bound, and hot-path-work candidates, not proof of improved native
  key-to-photon or resize latency.
- The current mux snapshot producer performs callback-free bounded preflight
  before pane callbacks or arena mutation. The audited pane/tab/window cleanup
  paths use exact registration witnesses, the server alert backlog has exact
  entry/byte limits with protected terminal outcomes, and key/paste dispatch
  carries process-local causal input serials whose terminal identity can be
  issued once before later allocation fails closed. The legacy render path now
  gives one exact per-pane baseline revision exclusive enqueue authority
  through queue admission, with exact acknowledge/rollback/drop settlement and
  visible detached-task errors. This still is not a client application ACK:
  transport admission cannot prove that a peer applied the delta, and remote
  Cargo proof cannot establish LAN input-to-present latency.
- Decoded-image trust in the current source is private, revision-bound
  authority: untrusted wire content retains the 64 MiB ceiling while explicitly
  trusted local decoded content has a separate 256 MiB ceiling. Validation,
  cache identity, and worker publication bind the exact content revision.
  Worker-decoded animation frames retain immutable shared pixels, so ordinary
  paint does not reopen their temporary blob or clone a full frame. Invalid
  `ImageCell` texture regions, padding, and cell geometry are rejected before
  decoded-cache or quad admission; decoded source dimensions are bounded by the
  separate image-validation contract.
- Background publication uses three distinct authorities rather than one vague
  "generation": an exact request token fences GUI completion, a strong file
  metadata stamp fences path-cache replacement, and a content revision fences
  decoded-image validation. Pixel-preserving animation-speed edits may retain
  validation authority only after the transformed durations satisfy the same
  renderer timing contract. A window admits at most 127 negative-z layers and
  256 MiB of unique validated decoded-pixel lengths. That is a logical payload
  admission bound, not an RSS, allocator-capacity, metadata, cache, in-flight,
  or GPU-residency bound. While an encoded zero-duration animation root is
  awaiting a timed worker frame, the current frame remains transparent; a
  queued timed frame can publish in the same paint, while decoder completion
  without one makes the root visible rather than leaving a permanent blank.
- Long-scroll repeat/mirror phase in the current source uses exact
  binary-rational quotient/remainder reduction rather than assuming an f64
  quotient below 2^53 preserves its fractional phase. It derives the only
  consumed whole-tile property, odd/even parity, directly from the aligned
  numerator modulo twice the denominator and carries the signed exact
  remainder into origin normalization. Positive exponent deltas use fast
  modular exponentiation, so neither u64 nor u128 becomes an arbitrary
  whole-tile ceiling; the bounded negative-delta alignment is materialized
  with checked arithmetic. Row-pixel/f32-factor products are admitted by their
  actual combined odd-significand width, avoiding a false fixed 2^29
  long-session cutoff while still rejecting a genuinely rounded 54-bit
  product. Radial-gradient noise is applied as an axis offset before both
  squared-distance terms. These are structural appearance/correctness fixes,
  not visual-corpus or frame-time qualification.
- Surface's batched width-one fill remains the hot path, but width-two cells use
  the established sequential invalidation semantics. Cap-adjacent rejected
  ranges return before iteration, printable-ASCII append rejects control-byte
  graphemes, and both vector and clustered trailing-blank pruning preserve or
  remove the full width of a wide cell. Single changes, complete batches, and
  actual dimension changes now preflight sequence exhaustion and fail before
  mutation rather than publishing later content under an aliased `usize::MAX`
  token. Batch-mutated rows carry the final committed frontier, so line-level
  `changed_since` cannot miss them under the pre-batch token. A dimension change
  advances identity and forces a full repaint even when the journal was already
  empty, while a same-dimension resize is a true no-op that preserves clustered
  rows and line identity. A real resize drops discarded rows before changing
  retained widths and constructs only genuinely added rows at final geometry,
  avoiding an eagerly allocated throwaway line and duplicate new-row resizing.
  These corrections invalidate the old assumption that the closed `ft-3xrmq`
  batch optimization was equivalence-proven; its retained benchmark claim
  remains unqualified.
- Three intentional eager settled-future APIs preserve call-time cancellation,
  preflight, and completed-outcome telemetry even when the returned ready
  future is never polled. The runtime-proof census now models that contract as a
  distinct fail-closed category instead of requiring a semantic change to
  `async fn`. This is proof-doctrine hardening, not runtime performance proof.
- Remote images still use coordinate lookup rather than a snapshot-owned batch,
  and there is no global cross-request singleflight or unified decoded/GPU/
  in-flight budget. `.6.7.1` remains open for that replacement architecture.
- The feature-gated opt-in `glyphcache_unit` harness compiles the binary-owned
  renderer module graph through a distinct crate-root wrapper under Rust's
  generated test runner and provides an explicit target that does not rely on
  the prior zero-test `--lib` command. Cfg-disabling `main` prevents automatic
  application startup; it does not prove that an arbitrary test body in the
  included graph cannot call a frontend or window constructor. Only exact
  statically audited test filters qualify as nonlaunching evidence. The GUI
  binary still has `test = false`, so this opt-in harness is not yet ordinary
  workspace test authority and does not substitute for `.6.8.1` or native
  proof.
- Atlas-capacity recovery still reaches `AllowImage::Scale(n)`, where
  `allocate_with_padding` constructs and copies a full-resolution 4WH staging
  image and performs a high-quality resize synchronously on the paint thread.
  Open Bead `ft-interactive-systems-performance-4tenz.8.8.1` owns the coherent
  direct/fallible scaling, accounting, oracle, and target A/B fix; off-main
  decode alone does not make this fallback nonblocking.
- Same-numeric-pane-ID reconnect/ABA protection and delivery application ACK
  remain separate open P0/P1 work; generation-aware cleanup and cache tokens do
  not by themselves establish end-to-end successor safety.
- The latest retained target-class resource-cockpit artifact remains
  `skipped_not_proven`; there is no admissible native M4/M5/Threadripper
  key-to-photon, continuous-resize, visual-quality, or long-session result for
  the current source candidate.
- The frozen Surface source (`src/lib.rs` SHA-256
  `a10c44d0680bd316afdbaa6e5e0e6c92f1344e86a996f44170f5d6d864f55b41`;
  `tests/proptest_core_serde.rs`
  `e4454be7823df1d3dfe4bdc4fd178b7c625bd0601e42829ceb583bafcf7b2943`)
  has exact strict-remote package proof against RCH source snapshot
  `80236bf736b56008`: Clippy with `-D warnings` passed on `ovh-b` as job
  `j-29966029874528646`, and 369 unit plus 27 integration/property tests passed
  with zero failures on `vmi1153651` as job `j-29966029874528647`.
- The frozen Termwiz candidates (`widgets/mod.rs` SHA-256
  `d5e7810470bb9f0cf5f0d31d69f4a08d492997368d3d1596a0e25a4d3b04427e`;
  `render/terminfo.rs`
  `76ab2cc63dfbde745eb8d58ed1c530c23a4e7b62bafc8e82d70ff8caba282cd5`)
  have strict remote package proof against the same source snapshot. Clippy
  with `-D warnings` passed on `vmi1149989` as job
  `j-29966029874528654`; 348 unit, 1 golden, 7 escape-property, 5 Kitty
  property, 2 serde, 1 succinct, and 3 doc tests passed with zero failures on
  `vmi1153651` as job `j-29966029874528653`. An earlier Clippy attempt on
  `ovh-b` failed closed with active-project exclusion (exit 103); it was not
  counted as proof and did not fall back locally.
- The frozen mux allocator/topology candidates have final SHA-256 values
  `9bd0590b6990fd58ffad4ce62f3c9e1da90efba3c402e39098d0d515dccad68b`
  (`lib.rs`),
  `10d9ac205516bb88bc301666b610abc1247dbfdc364d3da4db2cb7ffdbc21914`
  (`domain.rs`),
  `9580abba12c919bf6fd96b711a4c02abdc4964d00028aa8920f535804a6329ce`
  (`window.rs`),
  `d6094e68cb6482871c3b907acd42419eeecc73851bc0f3d8ba9722a8fc60e693`
  (`tab.rs`), and
  `9131f9848b7c1d2813d438c2e20cb19faadafb268e2e53fa0f26275d068b5fd3`
  (`client.rs`). Strict remote library Clippy with `-D warnings` passed on
  `vmi1149989` as job `j-29966029874528657`; all 771 mux library tests passed
  there as job `j-29966029874528661`. A separately retained exact allocator
  filter ran both terminal-boundary tests successfully (2 passed, 769 filtered)
  as job `j-29966029874528667`.
- The frozen codec source SHA-256 is
  `d3cac9f580e64b37689fb7db028b5416a0499fa39a5cf639134fcd96dd1b928c`.
  On exact strict remote source snapshot `80236bf736b56008`, all 263 codec
  library tests passed with zero failures on `ovh-a` as job
  `j-29966029874528668`, and library/test Clippy with `-D warnings` passed on
  `ovh-b` as job `j-29966029874528669`. Both jobs reported remote execution;
  neither had a failed or local-fallback attempt.
- These are package/static source proofs only. They are not GUI, native visual,
  key-to-photon, resize/zoom latency, target-class resource, soak, or release
  qualification. Final strict remote proof and hash reconciliation for the
  shared-tree GUI harness, session-handler, runtime-proof, and related
  image candidates remains pending at this checkpoint. Earlier passing jobs or
  a passing job against another source snapshot do not qualify a later
  candidate.
- A separate risk-weighted closure audit sampled 15 campaign-relevant closed
  Beads. It verified three, found six substantially complete, two partial, and
  four false-closed at varying severity. Precise completion-debt Beads now
  track the four false-closed cases; the sample is intentionally not
  extrapolated to the whole tracker.

The next promotion boundary is therefore implementation and evidence, not a
broader claim: finish the open image/snapshot/application-ACK and executable
renderer-test topology, activate ordered snapshots fail-closed, complete stable
GUI layout restoration, replace the legacy production pane projection, and
then run explicitly isolated correlated native quiet/loaded interaction plus
resize/zoom qualification on the named targets without touching an operator's
live session.

## 20. Primary external references

- Apple, [Tuning your code's performance for Apple silicon](https://developer.apple.com/documentation/apple-silicon/tuning-your-code-s-performance-for-apple-silicon/)
- Apple, [Recording performance data with signposts](https://developer.apple.com/documentation/os/recording-performance-data)
- Apple, [Achieving smooth frame rates with a Metal display link](https://developer.apple.com/documentation/metal/achieving-smooth-frame-rates-with-a-metal-display-link)
- Apple, [M5 architecture overview](https://www.apple.com/newsroom/2025/10/apple-unleashes-m5-the-next-big-leap-in-ai-performance-for-apple-silicon/)
- Apple, [M5 Pro/Max Fusion Architecture overview](https://www.apple.com/newsroom/2026/03/apple-introduces-macbook-pro-with-all-new-m5-pro-and-m5-max/)
- AMD, [Ryzen Threadripper PRO 5000 WX-Series announcement](https://www.amd.com/en/newsroom/press-releases/2022-3-8-new-amd-ryzen-threadripper-pro-5000-wx-series-proc.html)
