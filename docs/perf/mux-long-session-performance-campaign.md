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
   dispatch item queue, limits each outbound turn to 32 frames or 64 KiB,
   probes readable input between turns, and alternates the preferred direction
   when both sides are ready. That removes the former unbounded
   output-before-read starvation shape. Residual queue age and p95/p99 key
   latency remain measurement questions, not established wins.
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
  -> surface resize
  -> TermWindow::apply_dimensions
  -> sole whole-window coarse invalidation
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

- `apply_dimensions` is the sole whole-window resize invalidation authority;
  the former duplicate `TermWindow::resize` dirty walk has been removed.
- Even a same-grid sub-cell resize changes the global generation used by line
  quad keys, so visible line quads are rebuilt.
- A grid change resizes every tab in the window, not only the active tab.
- `Tab::apply_sizes_from_splits` dispatches pane resize intents serially.
  `LocalPane` may create one transient per-pane resize worker and coalesces to
  an in-flight intent plus the newest pending replacement; the live path no
  longer creates a scoped tab worker per pane.
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

### 2.2.1 Live snapshot and damage reconciliation (2026-08-15)

This is the retained source reconciliation for
`ft-interactive-systems-performance-4tenz.6.1`. It is a call-graph audit, not a
native performance result or a renderer cutover. No M4/M5, `trj`, LAN, visual,
or latency claim follows from it.

The current-source revalidation used code baseline
`eeb88117b129e63baae6d854d14f944776f6862e`. The documentation-only audit
changes that follow do not alter that producer/consumer graph.

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
construction, including fallible heap-quad growth and LFU updates. The PTY
parser needs the same terminal mutex. This tranche removes redundant
locking/traversal; it does not implement the short-lock immutable snapshot
required by `.6.3`.

The optional `disruptor-pane-io` feature does not change that default shipped
fact. It is absent from `frankenterm/mux`'s default features. When explicitly
enabled it can stage parser actions while paint owns the terminal mutex, but
the next terminal accessor drains those actions under the same mutex and ring
saturation falls back to blocking application. It is a bounded experimental
producer-side mitigation, not a renderer snapshot or lock-elimination proof.

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
`dead_code` pending live ownership transfer. `PerPane::compute_changes` is now
also `#[cfg(test)]`; it is not the legacy production entry point. Production
uses `prepare_legacy_render_enqueue`, installs an `InFlight` baseline revision,
and acknowledges or rolls that exact revision back when bounded queue admission
settles. That closes speculative pre-enqueue publication, but queue admission
is still not a client application acknowledgement. A successfully enqueued
delta that is lost before client application has no application-owned retry
receipt. It is therefore still incorrect to describe either the transactional
model or queue admission as the live end-to-end render authority.

| Substrate or claim | Live classification | Production effect | Exact gap |
|---|---|---|---|
| terminal `SequenceNo` plus `Pane::get_changed_since` | wired, authoritative but single-baseline | discovers changed stable rows for local GUI and server | no bounded multi-consumer journal, overflow epoch, acknowledgement, or resync identity |
| GUI `DirtyLineBitmap` and source counters | wired, partial | marks row damage and attributes clean cache-hit accounting | paint and cache lookup remain independent of the bitmap; every visible line still builds a cache key, so this is not a sparse iterator |
| `DamageGeneration` and presented-frame settlement | wired, authoritative for GUI damage clearing | retains damage on failed/stale synchronous presentation and clears only an exact successful generation, including the initial whole-screen damage | synchronous present return is not GPU completion, scanout, or key-to-photon evidence; the generation is not shared with terminal/server/client content |
| LFU `line_quad_cache` | wired | valid cache hits reapply retained layers and avoid shaping/glyph/quad reconstruction | global generations still invalidate broadly; cache is not the proposed row-indexed ownership model |
| `render::per_row_quad_cache` | test-only foundation | pure invalidation-plan tests | no live paint consumer and no owned per-row quad vectors |
| GUI `TerminalStateTripleBufferRegistry` | dormant/test foundation | explicit APIs and tests can publish metadata and derive watchdog health | no production producer, frame/status consumer, or renderer acquisition path; payload also lacks lines, attributes, images, hyperlinks, selection, IME, and render geometry |
| `DifferentialCellStream` GPU delta | compiled test-only/dormant | unit tests exercise CPU diff and ring policy | types and implementation are `allow(dead_code)` and WebGPU paint has no consumer |
| server transactional render attempt/coordinator | partial/dormant | model and unit tests cover preparation/settlement identities | live ownership still uses `prepare_legacy_render_enqueue`; no application-ACK-owned commit path |
| server legacy render delta | wired, coarse | uses checked ranges, a source fence, exact enqueue-phase baseline settlement, saturation rejection, and rollback/redirty on failed preparation or enqueue | clones the complete viewport before filtering; has no client application ACK and no autonomous retry of a successfully enqueued but unapplied delta |
| client `RenderablePane` and `ClientPane` adapter | wired, partial | bounds line/image resources, caches received rows, reconciles prediction, retains safe shape appdata, and serves GUI lines | GUI adapter clones the requested range and allocates a mutable-reference vector; coordinate image hydration lacks global singleflight/batch ownership |
| GUI renderer behavior tests | partial topology | pure helpers and library-owned surfaces execute under ordinary tests | the binary-owned `TermWindow`/renderer modules are declared with `test = false`; `.6.8.1` owns making their production behavior executable under normal gates |

Closed historical beads are evidence only for the exact slice in their close
reason. Their titles or original acceptance text do not upgrade a foundation
into a production cutover:

| Closed bead family | Retained result | Current production verdict |
|---|---|---|
| `ft-2okh0.3`, `ft-2okh0.3.2`, `ft-ipau0`, `ft-zkahy`, `ft-kyail`, `ft-r9kr6` | triple-buffer mechanics, watchdog policy, GUI registry/health adapters, and focused tests | **dormant/test foundation**: no production terminal-line producer or renderer acquire; the registered payload is the six-field persistence `TerminalState`, not renderable terminal state |
| `ft-tfzhy`, `ft-5ykn9`, `ft-jvj78`, `ft-8pcwy` | live dirty bitmaps, sources, settlement, counters, and helpers | **wired but partial**: every visible row still enters the callback and constructs a cache key; dirty iteration is not the live row traversal authority |
| `ft-mpc9b.1.5`, `ft-556zx` | invalidation-plan and telemetry foundations | **test-only**: production still owns the LFU `line_quad_cache`; there is no row-indexed quad owner or live `RowInvalidationPlan` consumer |
| `ft-mpc9b.6.7` | CPU differential-cell/ring model and unit tests | **test-only**: no WebGPU upload or paint consumer exists |
| `ft-87qfi` | bounded action-ring implementation and feature-gated concurrency tests | **off by default**: it neither removes the terminal mutex nor shortens the renderer's callback critical section |

These classifications deliberately override any broader wording in historical
bead titles, descriptions, or close reasons. A closed substrate is still useful
engineering work, but it cannot be counted as shipped renderer behavior until
the current production caller and consumer are both present and proved.

Current-source anchors for this verdict are:

- `frankenterm/mux/Cargo.toml:10-18` keeps `disruptor-pane-io` outside the
  default feature set;
- `crates/frankenterm-gui/src/termwindow/mod.rs:2134-2140` exposes the only
  terminal-state registry publication method, and the repository-wide caller
  census finds no production invocation;
- `frankenterm/mux/src/localpane.rs:912-927` acquires the terminal guard before
  calling `terminal_with_lines_mut_and_apply_hyperlinks`;
- `frankenterm/mux/src/renderable.rs:173-203` runs both implicit-link mutation
  and the caller callback while that guard remains borrowed;
- `crates/frankenterm-gui/src/termwindow/render/pane.rs:650-840` performs the
  metadata read, line hash, LFU lookup, shaping/glyph/atlas/quad work, cache
  insertion, and every-visible-row callback before returning;
- `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:1563-1678` marks
  `compute_changes` test-only and defines the live legacy enqueue guard, while
  `:2072-2174` retains the transactional preparation behind explicit
  `dead_code` ownership-transfer annotations;
- `frankenterm/client/src/pane/clientpane.rs:1376-1396` clones the requested
  remote rows, allocates the mutable-reference vector, renders outside the
  cache lock, and conditionally writes unchanged appdata back; and
- `crates/frankenterm-gui/src/quad.rs:202-437` keeps the differential-cell
  stream and replay helper explicitly `dead_code` pending WebGPU cutover.

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

### 2.2.2 Immutable coherent `RenderSnapshot` contract (2026-08-15)

This section is the normative architecture artifact for
`ft-interactive-systems-performance-4tenz.6.2`. It defines the state boundary
that `.6.3` must implement. The executable publication model lives in
`frankenterm/mux/src/renderable.rs` tests. Neither artifact is a live snapshot
producer, renderer cutover, or performance result.

#### Decision and scope

The unit of local presentation is one immutable **per-window** snapshot, not a
set of independently sampled pane lines. A frame may render an older complete
snapshot or no terminal content, but it must never combine fields from
different source, topology, geometry, overlay, prediction, or renderer-cache
generations. One frame acquires the snapshot once and carries that exact value
through shaping, glyph lookup, atlas lookup, quad construction, submit, and
presentation settlement. No per-cell, per-glyph, or per-quad mux capability
lookup is permitted.

“Per-window” means the exact active tab's displayed tiled, floating, zoomed,
and GUI-overlay pane set. Hidden tabs are represented by their frozen order and
active identity but their terminal rows are not copied into each frame. A tab
activation changes the authority and requires a new complete displayed-pane
projection.

Source coherence is deliberately **per pane**, not a claim that every terminal
in a window was frozen at one global physical instant. Every field within one
`PaneRenderSnapshot` belongs to that pane's one exact source generation and
source sequence. Different panes may carry different source generations
because the producer must not hold multiple terminal locks across a global
cut. The per-window final cut instead fences the exact pane set, layout,
window order, geometry, and GUI/cache generations simultaneously. A mutation
recorded after a pane's source cut remains damage for a successor publication;
it cannot be silently folded into the already captured pane projection.

The contract has three nested values:

```text
WindowRenderSnapshot
  authority: RenderAuthority
  geometry: WindowRenderGeometry
  gui: GuiRenderState
  panes: Arc<[PaneRenderSnapshot]>
  cache_epochs: RenderCacheEpochs
  damage: SnapshotDamageIdentity
  usage: SnapshotResourceUsage

PaneRenderSnapshot
  authority: ExactPaneAuthority
  terminal: ImmutableTerminalProjection
  gui: PaneGuiProjection
  prediction: PredictionProjection

ImmutableTerminalProjection
  source_generation + source_sequence
  dimensions + visible stable-row interval
  Arc<[immutable Line/row chunk]>
  cursor + palette + title + alternate-screen identity
  semantic zones + hyperlinks + image references
```

The sketch is descriptive, not a license to add a parallel type with only the
easy fields. Publication is legal only when every required component below is
present and belongs to the same capture transaction.

#### Exact identity and generation algebra

`RenderAuthority` is the product of:

- `MuxSessionIncarnation` and one non-exhausted `TopologyRevision`;
- the exact frozen window authority: `WindowId`, `WindowOrderRevision`, ordered
  weak exact tab identities, and exact active tab;
- the exact tab allocation held strongly only by the candidate through final
  validation, plus its tiled/floating/zoom/layout generation;
- one `PaneRegistrationHandle` for each pane; the candidate temporarily holds
  the exact pane allocation through capture and final revalidation, while the
  published snapshot retains only the weak exact registration capability and
  its immutable terminal projection;
- the remote render-connection incarnation for a `ClientPane`;
- window pixel size, DPI, terminal cell geometry, font/scale/config generation,
  viewport, alternate-screen, and overlay generations; and
- a publication generation scoped to that exact window incarnation.

Numeric pane, tab, and window IDs are diagnostic fields only. They cannot be
cache or publication authority by themselves. Process-local snapshots retain
weak exact tab/pane-registration identity plus immutable projections;
serializable remote snapshots use the existing connection, pane, delivery, and
snapshot incarnations. Reconnect creates a new identity scope even when numeric
IDs and source counters repeat.

Every new contract generation is a `u64` with checked successor semantics;
`u64::MAX` is the exhausted sentinel and is never published. Existing terminal
`SequenceNo` remains a captured source field until `.6.4` supplies its bounded
epoch/resync replacement, and its `usize::MAX` saturation can never mint a new
generation. Exhaustion is terminal for that publisher: clear presentation
eligibility, retain no apparently current snapshot, emit a finite content-free
diagnostic, and require a new owning incarnation or an explicit full
resynchronization protocol. Wrapping, saturating, resetting inside one
incarnation, or comparing raw counters across incarnations is forbidden.

`source_sequence` names terminal content, `TopologyRevision` names mux
structure, `WindowOrderRevision` names order/active membership,
`geometry_generation` names resize/zoom/DPI/cell geometry,
`overlay_generation` names selection/IME/highlight/internal-overlay state, and
`publication_generation` names the atomic immutable result. They are distinct
domains; no one counter is reused as another domain's watermark.

#### Required field and owner map

| State required for a frame | Authoritative capture owner | Immutable representation and rule |
|---|---|---|
| mux session, window, tab order, active tab, zoom, tiled/floating pane layout | `Mux`, `Window`, `Tab` | one mux-owned callback-free candidate capture with exact strong tab identities and one topology fence; publication retains weak exact identities plus immutable copied/shared layout, with no later ambient registry lookup during render |
| pane identity and lifecycle | mux pane registry | one weak exact `PaneRegistrationHandle` plus a temporary strong pane allocation during capture; a short final-cut registration guard revalidates them, then the published snapshot reads only its immutable projection and cannot keep the live pane or its retirement fence alive |
| visible rows and every `Cell` attribute | local `Terminal` mutex or remote `RenderablePane` cache | immutable row chunks covering the exact visible stable interval; no mutable renderer appdata is shared back into terminal authority |
| hyperlinks and terminal images | the same terminal/cache projection as the rows | references captured in the same source generation; remote image payload/handle batching remains `.6.7.1`, so unresolved images make the candidate incomplete |
| cursor position, shape, visibility, color, blink eligibility | terminal plus GUI blink state | terminal cursor and deterministic GUI phase are both frozen; wall-clock reads happen before capture or become a later generation |
| terminal dimensions, stable-row bounds, reverse video, palette, title, alternate screen | terminal/cache projection | sampled in the same pane critical section as rows and source fence; no separate `Pane` calls after row capture |
| semantic zones and command metadata | pane terminal semantic state | captured under the same source generation; lazy text expansion is outside the frame and cannot mutate the snapshot |
| viewport/scroll position, selection, bell, hover/highlight, mouse coordinates, pane overlay | `TermWindow::PaneState` and window GUI state | cloned into `PaneGuiProjection` under one GUI-thread capture; every mutation advances the overlay/viewport generation |
| predictive echo cells, serials, confidence display cue, reconciliation boundary | remote `RenderablePane` | a bounded immutable overlay captured with the authoritative cached-row generation; prediction never mutates snapshot rows |
| IME composition/caret geometry and accessibility-visible text/geometry | window/input/accessibility state | included or explicitly empty with its generation; accessibility is correctness-critical and cannot be deferred as cosmetic state |
| synchronized-output visibility, focus, opacity, tab bar/status/scrollbar/background | mux notification plus `TermWindow` | frozen in the window projection; BSU-hidden content cannot leak through a newer independently sampled row set |
| font set, font scale, shape rules, hyperlink-rule epoch, color scheme, renderer/device/atlas epochs | config/font/render owners | scalar immutable cache epochs; external caches are keyed by the complete snapshot identity and never by raw `PaneId` |
| damage/settlement identity | terminal damage journal plus GUI `DamageGeneration` | records the exact source ranges and GUI generation represented; clearing occurs only after the same snapshot presents successfully |
| resource counts and retained bytes | snapshot arena/admission owner | checked counts for rows/cells, unique strings/images, hidden tabs and metadata, immutable backing, and all retained generations; a candidate without a complete usage record is incomplete |
| remote connection and delivery identity | client domain and render-delivery ledger | exact connection incarnation plus pane/delivery identity; reconnect always invalidates the prior scope even when numeric IDs and counters repeat |

The test-only reference model makes this table mechanically auditable through
the one-to-one `RequiredField` mapping:
`WindowTopology`, `PaneIdentity`, `VisibleRowsAndCells`,
`HyperlinksAndImages`, `Cursor`, `TerminalMetadata`, `SemanticZones`,
`GuiOverlay`, `Prediction`, `ImeAccessibility`,
`SynchronizedOutputAndCompositing`, `FontConfigAndCacheEpochs`,
`DamageSettlement`, `ResourceUsage`, and `RemoteIdentity`. Its
`FieldSet::COMPLETE` is exactly the set of those named variants, and a focused
test removes each variant in turn and proves that publication is rejected.
Adding a field class to this contract therefore requires extending both the
owner map and the executable enum/test; an opaque bit with no owner-map entry
is forbidden.

#### Ownership, sharing, and lifetime

Copy/share policy is part of the correctness contract, not an implementation
afterthought:

- bounded scalar identities, geometry, cursor, palette, visibility, opacity,
  damage ranges, and cache epochs are copied into the candidate;
- immutable window topology and tab-bar metadata are shared as one
  generation-qualified `Arc` and reused across frames until topology or order
  changes; it contains weak exact tab identities and immutable metadata, not
  strong live-tab owners, and a frame does not clone every hidden tab allocation
  on every paint;
- visible terminal rows are immutable generation-qualified chunks shared by
  `Arc`; a mutation creates or replaces only affected chunks and never mutates
  backing already reachable from a candidate, published snapshot, or frame;
- hyperlink strings, semantic metadata, and image backing may be shared only
  through immutable exact-generation handles. Their unique backing is counted
  once within one snapshot and again when distinct retained generations do not
  share it;
- selection, IME, accessibility, prediction, and other small volatile overlays
  are bounded value copies so a later GUI mutation cannot rewrite an older
  snapshot; and
- glyph, shape, atlas, and quad caches remain external. The snapshot carries
  their epochs and complete authority keys, not mutable cache entries.

The candidate alone temporarily owns strong live-tab and live-pane allocations.
A published snapshot owns no live `Tab`, live `Pane`, terminal lock, mux lock,
retirement guard, GUI borrow, or mutable renderer cache reference. It owns only
immutable projections, weak exact capabilities, and shared immutable backing.
The candidate ends at rejection, cancellation, deadline expiry, or atomic
publication. The published value ends when superseded or invalidated and no
frame retains it.
Exactly one superseded value may remain render-held until presentation settles
or is rejected; no timer or queued callback may extend it into a fourth
generation. Every candidate has one finite configured capture deadline and a
cancel token owned by the publisher. Expiry or cancellation drops its strong
tab/pane allocations and aggregate-budget reservation without changing the
publication generation or last-known-good value. The miniature executable
model uses three logical ticks to prove this lifetime rule; `.6.3` must derive
the production duration from measured capture distributions and expose expiry
telemetry. These rules apply per window and under the session/process aggregate
arena below.

Scrollbar history summaries and offscreen scrollback are not copied into every
frame. The snapshot carries the visible rows plus bounded metadata needed to
render scroll position. A scroll that changes the visible interval creates a
new geometry/viewport generation. Accessibility requests for offscreen text
use their own bounded exact-generation snapshot rather than borrowing a frame
and extending its lifetime indefinitely.

#### Capture and publication protocol

The live implementation must follow this order:

1. **Admit.** On the GUI thread, capture the exact window authority and reserve
   one bounded candidate slot plus its session/process aggregate-arena
   reservation and finite deadline. Reject when a candidate already exists,
   the publisher is exhausted, the pane or hidden-tab metadata count exceeds
   the configured window cap, the aggregate publisher/window cap is full, or
   the peak three-generation envelope cannot be admitted.
2. **Freeze GUI state.** Copy the small window/pane GUI projections and their
   generations once. Retain each exact tab and pane allocation once for capture
   and revalidation, never per row or cell, and drop it after publish or
   rejection. Do not hold a pane-retirement lease across later capture or
   render work. Sort any multi-pane lock acquisition by stable exact identity.
3. **Freeze each terminal projection.** Under one short pane critical section,
   apply implicit hyperlink rules or consume a pre-normalized immutable line
   generation, capture rows and all terminal-owned metadata, then release the
   terminal/cache lock. No shaping, glyph lookup, atlas work, quad allocation,
   Lua callback, subscriber callback, RPC, image decode, or filesystem work is
   allowed in this section.
4. **Complete outside locks.** Assemble immutable `Arc`-owned row chunks,
   deduplicate counted image/hyperlink backing by exact identity, compute usage,
   and prepare the complete candidate. Fallible allocation happens here.
5. **Revalidate.** In one callback-free final cut, prove the mux/window/tab/pane
   identities remain exact and every captured generation still belongs to the
   candidate. Registration guards, if required by the live mux API, are
   acquired only for this bounded final cut and released before publication
   callbacks or render work. A content mutation racing after the source cut is
   handled by the damage journal, but a known stale/incomplete candidate is
   never newly published. Resize, zoom, DPI, alternate-screen, viewport,
   selection/IME, overlay, reconnect, or pane replacement invalidates the
   candidate.
6. **Publish.** Reserve the checked publication successor and atomically swap
   one `Arc<WindowRenderSnapshot>`. Publication cannot allocate, invoke a pane,
   notify a subscriber, or otherwise fail after the swap begins. An identical
   identity/source/content digest is a no-op; the same identity and source with
   a different digest is equivocation and fails closed.
7. **Render and settle.** Acquire the `Arc` once. Shape/rasterize/build quads
   outside terminal/mux locks, using caches keyed by complete authority and
   cache epochs. Settle damage only when the same publication and damage
   generation reaches the existing successful presentation boundary.

Cancellation before publication drops the candidate and its temporary exact
tab/pane allocations without advancing publication generation or changing the
last-known-good value.
Cancellation after the atomic swap cannot revoke the published value; it may
only suppress downstream optional work. Pane close/replacement tombstones all
exact-identity caches, clears publication eligibility, and prevents a delayed
candidate from attaching to a same-numbered successor. An already acquired
frame may finish reading its immutable projection, but its exact-identity
settlement must fail; retirement never waits for that frame.

#### Last-known-good policy

A prior complete snapshot remains eligible while a newer **content-only**
candidate is being built or fails allocation/budget validation. This preserves
the last coherent frame rather than presenting a partial update. It is not
eligible after window/tab/pane identity, geometry, viewport, alternate-screen,
selection/IME/overlay, synchronized-output visibility, font/config, or device
generation changes. Those transitions show the existing bounded placeholder
or retain the prior submitted GPU frame without relabeling it current, then
request a full snapshot. Exhaustion and tombstone also clear eligibility.

The publisher retains at most one eligible snapshot, one admitted candidate,
and one render-held prior generation. Queued repaint requests coalesce and
share the published `Arc`; they do not clone rows or acquire distinct snapshot
generations. Publishing a newer value supersedes an older in-flight frame, and
the presentation gate discards that frame. A second concurrent render acquire
is rejected until the single render-held slot settles. This bounds the peak
even if shaping or submission stalls while terminal output continues.

#### Resource and lock budgets

All limits are finite, configurable, observed, and checked with overflow-safe
arithmetic before publication:

- maximum active window publishers per session and process;
- maximum panes and visible rows per window snapshot;
- maximum hidden tabs and immutable hidden-tab title/status metadata bytes per
  window, session, and process;
- retained cell/line bytes and row-chunk overhead;
- unique hyperlink count and UTF-8 bytes;
- unique image references plus encoded/decoded resident bytes;
- semantic-zone, selection, prediction, IME, and accessibility payload counts;
- one published, one candidate, and at most one render-held prior snapshot,
  including shared-backing accounting; and
- short exact-registration validations and terminal/mux/GUI lock acquisitions
  per frame.

One central snapshot arena owns the session/process totals for active
publishers, distinct retained backing bytes, hidden tabs/metadata, and unique
referenced resources. Admission first computes every per-window and aggregate
successor with checked arithmetic, then reserves all counters as one token;
partial reservation is forbidden. Rejection, cancellation, expiry, detach,
and final retirement release that exact token. Shared immutable backing is
charged once per arena identity, while backing retained by distinct
generations or sessions is charged separately. A per-window budget can never
be multiplied by an unbounded number of windows.

Arena, publisher, and reservation identity are exact allocation capabilities,
not caller-selected numeric IDs. Display IDs are diagnostic only. A
reservation is non-`Copy`, carries its exact arena/publisher/token authority,
and is consumed once; duplicate, stale, or same-numbered cross-arena release
fails without changing counters. Publisher retirement is rejected while any
generation token remains, then reclaims the active-publisher slot so window
churn cannot exhaust capacity permanently.

The initial numeric memory caps must be frozen from measured visible-state
distributions in `.6.3`; `.6.2` does not invent target claims without data.
The non-negotiable structural caps are three distinct snapshot generations per
window, one candidate and one render acquire per publisher, one terminal
capture and at most one short-lived final-cut registration guard per pane, and
O(panes + visible rows + unique referenced resources) work. A guard is never
retained by the candidate, published snapshot, or render-held generation.
O(cells) authority acquisitions, multiple render-held generations, retirement
delayed by render work, and unbounded retry queues are reject conditions.

Lock **time** is telemetry, not a safe mid-critical-section timeout. The
producer records wait/hold p50/p95/p99/max, rows/bytes copied or shared, pane
count, and cause. `.6.3` must freeze a p99 hold-time gate before promotion and
retain a full-lock reference A/B. Exceeding the observational gate rejects the
optimization or triggers a coherent full fallback; it never aborts halfway
through terminal mutation and publishes a partial snapshot.

#### Relationship to remote delivery

The codec's `ExactRenderSnapshotManifestV1` is an immutable, bounded remote row
delivery protocol. It is not the local per-window frame snapshot: its authority
is per connection/pane/delivery and its projection does not contain GUI-owned
selection, IME, window layout, font/cache, or device state. `.6.5` may derive
wire deltas/manifests from the same pane terminal projection and damage journal,
but it must mint remote delivery identity and receiver bounds independently.
The local GUI must then compose received rows, prediction, and GUI state into a
new local window snapshot; it must not treat a wire manifest as a presented
frame receipt.

#### Formal invariants and retained model coverage

The executable model asserts these invariants after every transition:

1. a published value contains every named `RequiredField` and is within the
   per-window and session/process budgets, while the candidate plus every
   distinct published/render-held generation stays within the total peak
   budget and hidden-tab/metadata caps;
2. its exact target equals the current session/window/tab/pane/topology/
   geometry/alternate-screen/overlay target, and its source never exceeds
   current authority;
3. detached or exhausted publishers expose no eligible snapshot;
4. incomplete, over-budget, stale, canceled, and same-source/different-digest
   candidates cause zero publication mutation;
5. identical replay is a no-op and does not consume a generation;
6. successful publication generations strictly advance and never reach or
   cross the exhausted sentinel;
7. content-build failure preserves the prior complete value, while geometry or
   identity change clears its eligibility; and
8. reconnect changes the incarnation before counters restart; and
9. cancellation or deadline expiry releases the exact candidate allocation
   and aggregate reservation without changing publication generation or the
   last-known-good value.

Deterministic tests cover explicit stale/incomplete/over-budget/equivocation,
last-known-good, resize, pane replacement, cancellation before and after the
swap, deadline expiry/allocation release, exhaustion, detach, stale settlement,
hidden-tab limits, and reconnect cases. A bounded exhaustive model explores
every length-four trace over 34 events: valid, incomplete, unresolved-image,
over-budget, tab-count, and metadata admission; commit; both cancellation
phases; deadline tick; render acquire/release; all 19 independently mutable
generation domains; image/hyperlink mutation; reconnect; and detach. It keeps
the event path and prints a concrete counterexample trace on failure. Focused
tests prove the named state classes have explicit stale-publication semantics,
every required field rejects when absent, every independently advanced
miniature generation domain fails closed before its exhausted sentinel,
session/process reservations are bounded and reusable, and a newer publication
supersedes an older in-flight frame and that only the current exact generation
can settle.
In addition to sequential trace exploration, a thread-scheduled mutex-backed
sanity test races a writer replacing a complete `Arc<Published>` with a reader
and proves only that this miniature whole-value handoff exposes an old or new
value, never a mixed field set. It is not evidence about a production atomic
pointer, capture/revalidation race, settlement race, or memory ordering.
`.6.3` must bind the model to the chosen production publication primitive and
add production-scale stress, pane-reuse, cancellation, settlement, and
memory-fault tests. `.6.4` owns the multi-consumer damage/overflow model.

#### Migration sequence and non-claims

1. Add callback-free mux/window/tab candidate capture returning exact strong
   authority and publish only its weak exact identities plus immutable state.
2. Add one local-pane terminal projection method that captures every
   terminal-owned field under one guard; add the remote-cache equivalent.
3. Add GUI window composition and the bounded atomic publisher behind one
   experiment gate, while retaining the current full-lock renderer as oracle.
4. Make `LineRender` consume only the immutable snapshot and remove ambient
   pane/topology/state lookups from the shaped-line loop.
5. Key and tombstone line/shape/glyph/quad/image caches by exact snapshot
   identity and cache epochs; eliminate mutable line-appdata writeback.
6. After differential state/pixel/IME/accessibility proof and measured keep
   gates, remove the old live path rather than retaining two divergent modes.
7. Reuse the terminal projection and damage protocol for `.6.5` remote sparse
   delivery without conflating local publication with application ACK.

This contract and its tests prove only that the proposed state machine is
internally coherent. They do not prove that production publishes a snapshot,
that terminal-lock duration fell, that resize/zoom improved, that remote bytes
fell, or that M4/M5/Threadripper behavior improved. Those claims require the
live call graph and retained native evidence owned by `.6.3`, `.6.5`, `.6.8`,
and `.6.9`.

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

### 15.4 Source-grounded soak-substrate reuse matrix (2026-08-08)

This audit is the input contract for `ft-interactive-systems-performance-4tenz.4`.
Classifications describe what current source and retained artifacts establish,
independent of an earlier bead's status:

- **Live**: a production call path reaches the substrate; this is not long-haul
  or target-class proof.
- **Partial**: a useful production slice, historical result, or evidence
  contract exists, but the advertised end-to-end claim is not established.
- **Proxy**: a deterministic model, simulation, unit oracle, microbenchmark, or
  headless substitute is useful for regressions but skips the claimed path.
- **Dead/unwired**: code or a design shape exists without a production caller
  for the claimed behavior.
- **Obsolete**: the substrate may retain regression value but is not authority
  for this long-haul campaign.

#### Long-haul, load, and gate substrate

| Substrate | Class | Entry point, workload, metrics, and duration | Authority, limitation, reuse, and owner |
|---|---|---|---|
| `ft-xbnl0.4.1` leak-risk inventory | Live/partial | `runtime.rs::build_leak_risk_inventory` builds `LeakRiskInventorySnapshot` from runtime, mux, storage, workflow, watchdog, window, tab, workspace, and pane state; `HealthSnapshot` exposes point-in-time counts and waits through robot health renderers. | Queryable instrumentation, not allocation attribution or an aged result. `.4.3` must reuse its producers and add missing ownership fields instead of creating a second inventory API. |
| `ft-xbnl0.4.2` mux leak/fix tranche | Partial | Historical production mux teardown and leak fixes with focused regression evidence. | A closed fix tranche cannot establish absence of long-haul growth. `.4.7` and `.4.8` own current-source native proof. |
| `ft-xbnl0.4.3` runtime compaction | Partial | `compact_runtime_pane_state`, search watermark pruning, and related retention paths have deterministic source-level bounds. | Verification was blocked before relevant tests ran, so steady-state retention/recovery under production churn remains open at `.4.7`. |
| `ft-xbnl0.4.4` leak oracles | Proxy/partial | Short deterministic runtime, search, workflow, teardown, reconnect, compaction, watermark, and lock-storm unit oracles; no GUI, mux server, PTY, transport, or aged session. | `tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/leak_oracle_regressions/20260419T163650Z/` has no `summary.json`; the retained run was blocked before the new tests executed. Keep fast oracles; `.4.4` supplies the isolated production runner. |
| `ft-xbnl0.4.5` stress matrix | Proxy | `tests/e2e/test_ft_xbnl0_4_5_swarm_soak_matrix.sh`, `scripts/e2e_swarm_stress.sh`, and `e2e_swarm_stress_core.rs` label metrics `core_simulation` and repeatedly construct independent `TieredScrollback` and `BackpressureManager` models. | They skip live mux, PTY, transport, GUI, storage, search, and aged state. The referenced retained summary is absent; repeated CPU-test cycles are not elapsed 4h/24h/72h runs. Reuse seeds and pressure shapes only; `.4.5` wires production and `.4.8` retains extended evidence. |
| `ft-xbnl0.4.6` gate evaluator | Live contract/partial evidence | `docs/ft-xbnl0-4-6-release-gates.json`, validators, tests, and completion evidence consume `.4.4`/`.4.5` outputs. | The retained evaluator reported a missing leak summary and short-run duration failure. Reuse the validation shape in `.4.6`, but feed source-bound `.4.7`/`.4.8` artifacts and long-haul thresholds. |
| Replay corpus load rig | Proxy/partial | `examples/load_rig.rs` and `chaos_scale_harness.rs::run_replay_corpus_load_rig` build deterministic `LargeSwarmScenario` frames and run real `PatternEngine::detect`, recording replay-time poll/native-push bytes, frames, and detections; native push applies the real dedup-window coalescing to that corpus. | It still lacks production storage writes, live capture, mux panes, network, PTYs, and GUI. `.4.2` reuses the corpus/metamorphic checks; `ft-7h5da.10.3.2` owns production capture/storage reality and fail-closed remote proof. |
| Mission soak | Proxy/obsolete as long-haul authority | `tests/e2e/test_mission_soak.sh` runs fixed-seed suites and three roughly 16-18 second cycles. | `docs/metrics/mission_soak_chaos_evidence.json` says raw logs were not retained. Keep deterministic checks, but never use them for `.4.7`/`.4.8`. |
| Generic E2E soak runner | Partial framework | `scripts/e2e_test.sh --soak-duration` has checkpoint, resume, fault-loop, and isolation concepts; some paths execute the product and clean isolated resources. | Existence is not evidence. `.4.4` may reuse concepts only after adding fail-closed identity/ownership guards that prevent launching, attaching to, inputting to, sampling, signalling, or cleaning up an operator session. |

#### GUI, resource-cockpit, and renderer substrate

| Substrate | Class | Entry point, workload, metrics, and duration | Authority, limitation, reuse, and owner |
|---|---|---|---|
| `ft-dohm7` GUI soak | Partial/historical native | Isolated patched-GUI image/text ran 182 seconds/36 samples (RSS about 638240 KiB to 230736 KiB, maximum 719072 KiB); overflow text ran 103 seconds/35 samples (RSS about 156928 KiB to 137744 KiB, maximum 156928 KiB). It also sampled `vmmap`, heap, and cold-tier bytes. | Evidence lived under `/tmp/ft-dohm7-*`; no tracked source-bound artifact remains. Reuse workload shapes and hypotheses in `.4.2`/`.4.3`; `.4.7`/`.4.8` retain current-source native evidence. |
| Resource-cockpit schema/target receipt | Live schema/partial; target not proven | Resource DTOs, validators, `ft-rz0eb.4` conformance, and `docs/attestations/proofs/resource-cockpit-target-class.json`. The tracked target summary observed Darwin arm64, 14 CPUs, and 64 GiB instead of Linux x86_64, at least 64 CPUs, and 256 GiB. | The summary and attestation correctly say `skipped_not_proven`; target conformance was not run. `.4.3` reuses schema/unavailable semantics; `ft-7h5da.10.4.3`, `.4.7`, `.4.8`, then `ft-tf6g3.1` own target evidence and release promotion. |
| Resource pressure soak | Proxy/partial | `ft-p3457.4` generates before/during/after snapshots and validates a pressure receipt declaring `live_pane_mutation:false`. | Its cited summary exists only as untracked output and is not durable authority. Reuse fields/monotonicity in `.4.3`/`.4.6`; `.4.5` creates real pressure. |
| Dirty-row/resize SLO | Proxy/partial | Dirty-row state, line-quad cache, audit documents, and RQ-S1 deterministic core traces/timings. | No presented GUI resize/zoom/raster/compositor/display path. Reuse thresholds/traces in campaign `.8`; renderer proof leaves own native presented qualification. |
| Input-to-photon SLO | Proxy/blocked | RQ-S2 headless macOS GPU-readback measured about 33 ms steady state and 351.65 ms cold path, both over target. | It omits real `NSEvent`, mux, PTY, transport, window-server presentation, and photons; no target Wayland run. Preserve as a negative control; `ft-tf6g3.8` and campaign input-latency leaves own end-to-end proof. |
| Idle-GPU SLO | Proxy/blocked | RQ-S9 scheduler/predicate checks cover pacing decisions. | No production GPU counters or current native idle-window measurement. `ft-tf6g3.9`, `ft-96uy6`, and `ft-1l5n2` own counters and compositor evidence. |
| Visual SSIM corpus | Proxy/partial | Deterministic static golden comparisons. | A self-consistent golden does not show that live native resize/zoom produced it. Reuse comparison machinery; `ft-interactive-swarm-product-convergence-7xqz4.9.1` owns the real corpus and `.8.10` consumes it. |

#### Scrollback, cache, atlas, reconnect, and incident substrate

| Substrate | Class | Entry point, workload, metrics, and duration | Authority, limitation, reuse, and owner |
|---|---|---|---|
| Snapshot scrollback projection | Live/source-level | Schema v39 maintains one `pane_scrollback_summary` row per persisted pane in the same SQLite transaction as each `output_segments` append or retention delete. `snapshot_engine::load_latest_scrollback_refs_sync` reads only those rows, so capture work is O(requested panes), independent of retained history depth. The wire field is `retained_segment_count`: arbitrary stream fragments currently retained, not LF/CR/CRLF logical lines, wrapped display rows, or lifetime captures. | Migration performs one deliberate legacy-history backfill, then triggers own append/delete boundaries and reject negative, non-integer, overflowing, missing-summary, or metadata-rewrite states. Pane-state schema v2 rejects both legacy `total_lines_captured` and interim `total_segments_captured` keys. Source/unit proof does not establish native long-session latency; the campaign soak leaves still own that evidence. |
| Production scrollback tiering/spill | Live/partial | `Screen` receives `ScrollbackTierConfig`; `ScrollbackTieringState`, `ColdScrollbackReflowWorker`, and mux-server `LiveScrollbackSpillSink` reach `MmapScrollbackStore` and expose live spill/tiering counters. | The core stress model skips this path. The typed cold-tier document says compression, structured async I/O, encryption, indexing, cleanup, and search integration remain incomplete. `.4.3` consumes counters, `.4.5` drives churn, and `ft-35zzw` owns viewport-first background reflow. |
| Typed cold-tier pipeline | Dead/unwired/partial foundation | Types and source contracts in `docs/security/scrollback-cold-tier-pipeline.md`. | The document calls itself a foundation, not a complete live pipeline. Reuse types only where they match real owners; do not claim production persistence, cleanup, or search coverage. |
| Wrap/image/glyph/line-quad/shape caches | Live/partial | `LogicalLineWrapCache`, terminal image LRU, GUI `GlyphCache`, image LFU byte budgeting, line-quad cache, and shape cache are production-reachable; some expose hit/miss or retained-byte metrics. | Complete ownership, eviction attribution, GPU allocation, compositor-command, and per-operation frame-budget telemetry are missing. `.4.3` joins producers; `.4.5` drives pressure; `ft-th8ag`, `ft-gwzrm`, `ft-96uy6`, `ft-th8ag.1`, `ft-1g4mv`, and `ft-1l5n2` own missing sources. |
| Production glyph atlas | Live/partial | GUI `GlyphCache` owns `window::bitmaps::atlas::Atlas`, allocates sprites, reports footprint, and recreates atlases. `AllowImage::Scale(n)` performs allocation/staging/resampling on the paint thread. | `docs/perf/atlas-packing.json` is pure packer evidence; RQ-S10 synthesizes zero-rebuild events; `atlas_tiered_swap` is unwired pseudocode. `.4.3` consumes footprint, `.4.5` drives churn, and `.8.8.1` owns the paint-thread fix/A-B. |
| Reconnect loop | Live/partial | `frankenterm/client/src/client.rs` has generation authority, one lazy `ConnectionUI`, bounded attempts, healthy-session reset, and reattach-if-current. | No retained real unreachable-host/healthy-interruption run; the `.4.4` oracle run never reached tests. Same-numeric-pane-ID ABA/application acknowledgement is separate. `.4.5` owns churn; `ft-c4rn6` owns real down-host proof. |
| Incident collector/replay | Live/partial plus proxy corpus | `crash.rs::collect_incident_bundle` collects privacy-budgeted sources with robot-state global/database fallback, warnings, and replay metadata. The tracked flight-recorder attestation covers five deterministic scenarios. | That corpus is not rolling native GUI/mux capture or long-run causal fidelity. `.4.9` reuses the collector and owns content-free rolling capture, minimization, and replay fidelity. |
| Observed long-run GUI incident | Partial incident evidence | `.4.9.1` records a historical approximately 80-hour GUI incident at about 6.8 GiB and 92-95% CPU against an older installed build. | It is not current-source reproduction and grants no authority to inspect or touch an operator process. Use only as a hypothesis/workload target; `.4.9.1` owns safe isolated reproduction and durable evidence. |

#### Dependency and evidence decisions

1. `.4.2` reuses deterministic replay/mission corpora and historical
   `ft-dohm7` workload shapes, but specifies the real pane, fleet, transport,
   resize, zoom, search, reconnect, and pressure actions that replace proxies.
2. `.4.3` joins live leak, scrollback, cache, atlas, mux, storage, renderer, and
   resource producers into one ownership/unavailable-data schema. It must not
   synthesize unavailable target metrics.
3. `.4.4` reuses checkpoint/resume concepts and deterministic oracles while
   adding fail-closed process identity/ownership. Its runner must be unable to
   affect an operator's FrankenTerm session.
4. `.4.5` replaces independent core simulations with production scrollback,
   cache/atlas, resize/zoom, reconnect, capture, storage, search, and
   maintenance churn.
5. `.4.6` analyzes source-bound long-haul outputs. Its old short-run gate shape
   is reusable; its proxy inputs and three-second threshold are not.
6. `.4.7` owns isolated 4-hour current-source qualification, `.4.8` owns
   24-hour/72-hour target-class qualification, and `.4.9` owns privacy-safe
   incident capture, minimization, and replay. Subsystem beads retain their
   implementation and native-SLO ownership.

Authoritative results must be retained in Git or a content-addressed release
evidence store; bind the exact full source SHA, workload/seed, binary/package
identity, target predicate, elapsed duration, clocks, schema, and raw-metric
manifest; and pass their validator. Ignored or untracked output, `/tmp` logs,
bead comments, screenshots, missing summaries, and artifacts from another
revision are context only.

No current artifact proves a 4h/24h/72h native production run, an M4/M5 or
high-core-count Threadripper target, a real presented resize/zoom path, or a
complete input-to-photon path. Historical GUI observations, skipped target
receipts, short simulations, static goldens, and microbenchmarks cannot promote
those claims. In this matrix, **live** is callgraph evidence only.

### 15.5 Deterministic long-haul workload corpus

`fixtures/perf/soak-workload-corpus-v1.json` is the compact source of truth for
`.4.2`. `soak_confidence_gate.rs` parses, validates, materializes, hashes, and
logically replays it without launching FrankenTerm. The contract has these
properties:

- exact 20-, 50-, and 200-pane allocations with one stable fleet-slot identity
  per actor, independent of transient mux IDs and input-list order;
- a self-contained materialized plan that retains the base seed and canonical
  phase contract so identities and every scheduled action can be re-derived;
- deterministic round-robin interactive assignment across editor/TUI,
  agent-stream, progress-redraw, resize/zoom, layout-churn, and reconnect
  actors, rather than concentrating interaction in the first sorted actor;
- fixed per-actor seeds, typed drivers, exact argv vectors where a later
  isolated child is necessary, content-addressed existing assets with declared
  executable mode, payload profiles, output-rate envelopes, and final markers.
  The validator binds every dimension to its registered driver/program,
  activation, and output envelope; requires every pinned asset to be used;
  bounds verification reads even if a file grows after metadata inspection;
  and rejects non-portable paths, unknown adapters, or out-of-range isolated
  argv before a runner can trust a rehashed plan;
- explicit activation semantics: the owned editor and agent fixtures are
  persistent children spanning the 630-second cycle, while bounded built-in,
  fixture-replay, and burst adapters run on scheduled actions; the burst argv
  is sized so its 250 ms cadence produces the declared 802,688 B/s peak; each
  persistent child also carries an explicit owned shutdown contract (stdin line
  for the agent and interrupt-with-cleanup for the alternate-screen fixture);
- quiet shell, editor/TUI, build/test output, progress redraw, agent-like
  stream, image, glyph-diversity, search, capture, workflow, maintenance,
  layout churn, resize/zoom, reconnect, and burst dimensions at every scale;
- a repeatable 630-second cycle with non-overlapping quiet, interactive,
  sustained-service, visual/glyph, layout/resize, reconnect, adversarial-burst,
  and cooldown/final-state phases;
- idempotent setup/teardown operations for workspace, window, tab, pane, and
  actor resources whose exact parents and canonical order are validated, plus
  logical failure replay that still settles every owned resource. Confidence
  evidence also fails closed on missing or rewritten canonical invariants,
  deadlock-counter overflow, per-cell accounting defects that cancel only in
  aggregate, malformed timestamps/identities, and non-finite metrics; and
- explicit logical versus production-runner oracle authority. Terminal-state,
  layout-state, capture/search-quiescence, child settlement, transport closure,
  and resource-return-to-baseline remain production-runner obligations and
  cannot be minted by the logical replay.

The corpus reuses pinned agent/build samples, dummy agent/burst/alternate-screen
actors, resize/zoom simulations, and renderer images/glyph goldens identified by
the `.4.1` audit. Real Codex, Claude, Gemini, or other model sessions are a
separately versioned dogfood overlay and are excluded from the deterministic
verdict. The required dogfood identity binds agent name/version, model,
configuration, session, and transcript digests; secrets and external model
availability are not corpus dependencies.

This is workload and lifecycle authority for `.4.4`, not native soak evidence.
The later runner must execute it only in an explicitly isolated owned session,
retain actual terminal/layout/capture oracles, and prove process settlement. It
must never launch against, attach to, input to, sample, signal, or clean up an
operator's FrankenTerm session.

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

## 19. Current reality checkpoint (2026-08-10)

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
- Fleet tiered-scrollback health now has a dedicated codec-v57 bulk path
  (`PDU93`/`PDU94`) instead of borrowing per-pane render, viewport, reflow, or
  delta RPCs. Each logical request is sorted and deduplicated, admitted in
  chunks of at most 256 panes, and yields between chunks. The server freezes
  all pane registrations before invoking any pane callback and resolves one
  bounded chunk in one mux turn; one missing, closed, unavailable, or panicking
  pane becomes a typed sibling result rather than aborting the batch. The wire
  result carries ten bounded health fields rather than render payloads, with a
  4 KiB request ceiling and a 32 KiB response ceiling. Sharded domains admit at
  most sixteen shard requests concurrently and restore global request order.
  Capability absence and terminal non-cancellation errors stop new admission
  and drain already admitted siblings. Cancellation stops new admission and
  drops the remaining Cx-bound sibling futures; their owned mux clients are not
  returned to the pool, so an uncertain in-flight transport cannot be reused.
  Old peers are capability-fenced before the new PDU is written and retain a
  healthy pooled connection while the runtime uses a separately bounded legacy
  fallback. Dedicated content-free counters distinguish logical
  bulk requests, admitted batches, server wire requests, queue/snapshot
  duration, partial outcomes, cancellation, and fallback; the codec's existing
  PDU-labeled size histogram records encoded PDU93/PDU94 sizes. These are
  structural work-, memory-, fairness-, and failure-containment properties.
  They do not establish a wall-time, interactive-latency, rendering, visual,
  M4/M5, or Threadripper improvement without retained same-source target runs.
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
