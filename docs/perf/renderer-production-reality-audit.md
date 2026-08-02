# Renderer production reality audit

**Bead:** `ft-interactive-systems-performance-4tenz.8.1`

**Audit date:** 2026-08-01

**Source snapshot:** `20b0e667721df3a3ca5abb2fe4b976572ea4352b`

**Evidence class:** static source, Git, and Beads inspection only

## Purpose and proof boundary

This audit identifies the renderer, invalidation, pacing, presentation, and
telemetry code that a production FrankenTerm GUI window actually reaches. It
also distinguishes that code from the substantial policy and test substrate
that is compiled but is not connected to the production call graph.

This is deliberately not a performance result. The audit did not launch or
attach to FrankenTerm, enumerate or signal processes, drive the GUI, run Cargo,
sample a live session, call platform display APIs, or collect GPU/display
timings. In particular, it does not prove input-to-photon latency, sustained
resize FPS, visual quality, VRR, display-link pacing, direct Metal rendering,
or behavior specific to an M4/M5 Mac. Those claims require the live measurement
lanes defined by the parent campaign.

### Classification legend

| Classification | Meaning in this audit |
|---|---|
| **Live** | A production GUI path constructs or calls the substrate. |
| **Partial** | Some production state or calls exist, but the advertised optimization or end-to-end contract does not. |
| **Fallback** | A live conservative mechanism used when a stronger mechanism is absent. |
| **Test-only** | Exercised by tests or benches, but no production caller was found. |
| **Dead/unconsumed** | Compiled or exposed, but no production construction/call/consumer was found. |
| **Unproven** | Static code cannot establish the runtime mechanism or performance claim. |

“Live” means reachable, not fast, correct under every failure, or validated on
target hardware.

## Executive result

The current production architecture is simpler than several module names and
comments imply:

1. Native window code produces `Resized` and `NeedRepaint` events.
2. `TermWindow::resize` updates surface state, dirties every pane, and calls
   `apply_dimensions`; `apply_dimensions` dirties every pane a second time.
3. The renderer still walks every visible line. A live LFU line-quad cache can
   reuse quads, but `quad_generation` is part of its key, so either resize-side
   generation bump invalidates every cached line.
4. macOS, X11, and Windows pace repaint demand with a fixed timer derived from
   configured `max_fps`. Wayland uses the compositor frame-callback chain.
5. `FrameBudget` participates in the live paint path, but it is permanently
   constructed at 60 Hz. The idle detector records state, but its scheduling
   decision is discarded.
6. OpenGL is the default renderer. WebGPU is live only when selected in config;
   its present path uses FIFO mode and unconditionally submits and presents.
7. Display probes, adaptive FPS, VRR negotiation, conditional redraw, frame
   deduplication, per-row invalidation planning, a compositor layer stack, and
   “MetalDirect” are policy/test substrates rather than production mechanisms.
8. Production timing observability is limited to in-process histograms and
   counters. There is no retained trace joining native damage, cache work,
   render submission, present mechanism, and display completion.

The most important architectural conclusion is that subsequent beads should
cut over one existing production graph. They must not build another parallel
set of attractive but unconsumed policy objects.

## Production call and configuration graph

### macOS invalidation, resize, and presentation

```text
WindowOps::invalidate
  -> macOS WindowInner::invalidate
       -> [NSView setNeedsDisplay:YES]
       -> invalidated = true

AppKit windowDidResize
  -> backing pixel dimensions + DPI
  -> WindowEvent::Resized
       -> TermWindow::resize
            -> quad_generation += 1
            -> mark every pane dirty (Resize)
            -> WebGpuState::resize, when WebGPU is configured
            -> apply_dimensions / scaling_changed
                 -> quad_generation += 1
                 -> mark every pane dirty (Resize)
                 -> same-grid guard may skip Tab::resize + overlay resize

AppKit displayLayer/drawRect
  -> if paint_throttled: coalesce as invalidated=true
  -> otherwise WindowEvent::NeedRepaint
       -> TermWindow event dispatch
            -> defer while programmatic resizes are pending, otherwise
            -> paint_impl
                 -> paint all positioned panes / all visible lines
                 -> LFU LineQuadCache lookup per line
                 -> call_draw(Glium | WebGpu)
                      -> WebGPU: get surface texture, encode full pass,
                         queue.submit, output.present
                 -> clear dirty-line state according to whole-screen gate
       -> fixed max_fps timer clears paint_throttled
       -> setNeedsDisplay again if a request was coalesced
```

Source anchors:

- The public event and invalidation contract is in
  `frankenterm/window/src/lib.rs:153-175` and `:248-275`.
- The macOS façade forwards invalidation at
  `frankenterm/window/src/os/macos/window.rs:856-860`; the inner implementation
  calls `setNeedsDisplay:` and records pending invalidation at `:1316-1322`.
- Native live-resize state and resize event production are at
  `frankenterm/window/src/os/macos/window.rs:2987-2998` and `:3001-3077`.
- `displayLayer` delegates to `draw_rect` at `:3090-3095`; the live throttle and
  repaint event are at `:3134-3175`.
- `TermWindow` dispatches resize and repaint at
  `crates/frankenterm-gui/src/termwindow/mod.rs:2611-2628` and `:2659-2667`.
- Renderer selection and construction are at
  `crates/frankenterm-gui/src/termwindow/mod.rs:2472-2509`.
- The draw dispatch is at
  `crates/frankenterm-gui/src/termwindow/render/draw.rs:13-18`; the WebGPU
  acquire/encode/submit/present path is at `:21-194`.

### Cross-platform repaint pacing

| Platform | Live demand/pacing path | Classification | Important limit |
|---|---|---|---|
| macOS | AppKit `setNeedsDisplay:` plus `paint_throttled` and a detached `max_fps` sleep (`window.rs:1316-1322`, `:3134-3175`) | **Live; fallback** | Fixed, unaligned millisecond timer; no display-link tick or completion timestamp. |
| Wayland | One in-flight compositor `frame_callback`; new invalidations coalesce and callback completion requests another paint (`wayland/window.rs:1198-1204`, `:1328-1343`, `:1386-1393`) | **Live** | Correct callback backpressure is not VRR or tearing-control integration. |
| X11 | Pending repaint coalescing plus detached `max_fps` sleep (`x11/window.rs:371-458`) | **Live; fallback** | Fixed timer; no XPresent scheduling in the production path. |
| Windows | Repaint dispatch plus detached `max_fps` sleep (`windows/window.rs:1675-1718`) | **Live; fallback** | Fixed timer; no production DWM timing integration found. |

`frame_interval_for_max_fps` is a shared live helper. At this snapshot it clamps
to 1–1,000 FPS and returns a nonzero ceiling-divided millisecond interval
(`frankenterm/config/src/config.rs:2366-2397`). This prevents zero-duration
spin and accidental overshoot of the configured cap, but a millisecond timer
cannot phase-lock presentation to a variable-refresh display.

The helper also supplies the minimum decoded image-frame duration in the live
glyph/image cache (`crates/frankenterm-gui/src/glyphcache.rs:775-832`,
`:1217-1225`, `:1463-1731`). That is a second consumer of the same config cap,
not a display-synchronized scheduler.

macOS does read `NSScreen.maximumFramesPerSecond` into `ScreenInfo.max_fps`
(`frankenterm/window/src/os/macos/connection.rs:223-271` and
`frankenterm/window/src/screen.rs:12-18`). No production consumer was found
that binds this value to `TermWindow::FrameBudget`, the native repaint timer,
or WebGPU presentation. It is therefore a **live producer with an unconsumed
output**, not adaptive pacing.

## Resize, damage, and cache reality

### Confirmed duplicate resize invalidation

Every accepted nonzero resize with changed dimensions or window state performs
these operations in `TermWindow::resize`:

- `quad_generation += 1`;
- full-pane dirty marking with source `Resize`;
- then `apply_dimensions` or `scaling_changed`, which reaches
  `apply_dimensions`.

`apply_dimensions` immediately repeats both operations. The two sites are
`crates/frankenterm-gui/src/termwindow/resize.rs:73-78` and `:237-242`.
Consequently, one native resize event causes exactly two quad-generation bumps,
two complete traversals of registered pane dirty bitmaps, and two resize-source
counter increments in the ordinary path.

This is static proof of duplicated invalidation bookkeeping. It is **not** proof
of two GPU submissions, two presents, or two mux reflows: repaint requests can
coalesce, and the same-grid guard can suppress `Tab::resize`.

### Same-grid behavior

The same-grid optimization at
`crates/frankenterm-gui/src/termwindow/resize.rs:390-422` correctly avoids
`Tab::resize` and overlay resize when the recomputed `TerminalSize` is unchanged.
It still updates surface dimensions and arrives after both whole-pane dirty
marks. It also cannot reuse line quads across the resize because
`LineQuadCacheKey` contains `quad_generation`
(`crates/frankenterm-gui/src/termwindow/render/pane.rs:671-691`).

That establishes the opportunity for same-grid reprojection: the terminal cell
model and shaped row content can remain unchanged while only viewport/surface
projection changes. The current single `quad_generation` conflates those
domains.

### Zoom and DPI behavior

A scale change has genuinely different invalidation requirements. It changes
font metrics, advances `shape_generation`, clears the shape cache, evicts stale
cell-metric glyphs, and performs a synchronous best-effort glyph warm-up under
the fixed scale-change budget
(`crates/frankenterm-gui/src/termwindow/resize.rs:130-194`). It then invalidates
the fancy tab bar and modal (`:208-223`).

The campaign must preserve those correctness effects. A pure pixel-only resize
may reuse shaped content; a font-scale or DPI change generally may not. The
typed contract in `.8.2` needs to make that distinction explicit rather than
inferring it from one global generation.

### What dirty-line tracking actually saves

The live dirty-line state and whole-screen gate are stored on `TermWindow`
(`crates/frankenterm-gui/src/termwindow/mod.rs:922-973`). Live producers include:

- PTY stable-row changes and selection invalidation
  (`crates/frankenterm-gui/src/termwindow/mod.rs:3518-3602`);
- cursor movement discovered during pane paint
  (`crates/frankenterm-gui/src/termwindow/render/pane.rs:232-295`);
- focus, font, theme, and resize whole-screen invalidation
  (`termwindow/mod.rs:2094-2096`, `:2793-2807`, `:3614-3627`, and
  `resize.rs:73-78`, `:237-242`).

The renderer still calls `with_lines_mut` and iterates every visible line
(`render/pane.rs:824-839`). For each line it computes a full cache key and
performs the LFU lookup (`:669-735`). The dirty bitmap's “clean” decision only
controls whether a successful cache reuse increments `clean_lines_skipped`;
cache lookup and reuse can occur for a dirty line too when its complete key is
unchanged. The bitmap therefore provides live dirty-source accounting and
cache-reuse attribution, but does not yet provide true dirty-row-only iteration.

The comments in `render/dirty_lines.rs:1-39` correctly narrow the current
integration at `:30-39`, but the opening “replaces ... whole-screen
invalidation” description overstates the live cutover. Production should be
classified **partial**, not fully dirty-driven.

### Damage and cache substrate classification

| Substrate | Production reality | Classification | Canonical decision |
|---|---|---|---|
| `DirtyLineBitmap` and TermWindow producer counters | Constructed and mutated in production; clear predicate is live. | **Live; partial optimization** | Keep as the canonical row-damage representation, but make it drive iteration after correctness gates. |
| Existing `LineQuadCacheKey` / LFU cache | Queried for every live visible line and can avoid reshaping/rebuilding. | **Live** | Reuse it for `.8.4`; split its content and projection generations rather than introducing a second live cache prematurely. |
| `iter_dirty_render_gate_enabled` | Enabled by default, but only labels successful clean-line cache reuse. | **Partial** | Do not use the flag as proof of dirty-only rendering. Rename or replace semantics during cutover. |
| `TiledGridLayer` dirty rectangle adapter | Types and helpers carry `#[allow(dead_code)]`; its `LayerStack` use is under `#[cfg(test)]` (`render/pane.rs:40-175`, `:853-1000`). | **Test-only; dead in production** | It may inform tests, but is not a production damage path. |
| `LayerStack` compositor | Construction sites found in module tests and pane tests; no production stack construction found. Module says integration is deferred (`render/compositor.rs:1-50`). | **Test-only; dead in production** | Do not build `.8.2` around it unless the same bead performs and proves the cutover. |
| `per_row_quad_cache` plan | Module explicitly says live paint wiring and cache replacement are deferred (`render/per_row_quad_cache.rs:1-40`). | **Test-only; dead in production** | Prefer the already-live line cache unless profiling and design proof require replacement. |
| `redraw_predicate` | Module explicitly defers `TermWindow::should_paint` and paint-entry wiring (`render/redraw_predicate.rs:1-50`). | **Test-only; dead in production** | Wire only through the single pacing coordinator, with force-present and a11y/recording contracts. |
| `FrameDeduplicator` | All discovered constructors are tests; module defers hashing and pre-present wiring (`render/frame_dedup.rs:1-64`). | **Test-only; dead in production** | Do not hash a readback framebuffer on faith; require profile and backend-native change identity. |

## Frame budgeting, idle state, and adaptive FPS

`paint_impl` does use the live frame budget: it begins and ends a frame, gates
dirty-quad and cosmetic operations, drains deferred cosmetic work, and can
force a follow-up invalidation
(`crates/frankenterm-gui/src/termwindow/render/paint.rs:19-26`, `:45-50`,
`:159-191`). This substrate is not just a test fixture.

Its refresh input is nevertheless static. `TermWindow` constructs
`FrameBudget::new(60)` (`termwindow/mod.rs:2330-2334`), and `FrameBudget`
derives an immutable nanosecond ceiling from that constructor argument
(`termwindow/frame_budget.rs:235-275`). No live refresh-rate update method or
production adaptive sink was found.

The idle detector records live events and state transitions, but
`poll_idle_scheduler` hard-codes 60 Hz (`termwindow/mod.rs:2070-2080`). The
status-event caller discards the returned decision (`:3056-3058`). Thus it is
**live telemetry/state, not a live scheduler**.

| Substrate | Static result | Classification |
|---|---|---|
| `FrameBudget` | Live paint gating; fixed 60 Hz; no display/adaptive reconfiguration. | **Live; partial** |
| Idle detector | Live event/state recording; scheduler decision discarded. | **Live; partial, telemetry-only scheduling** |
| `frankenterm-core::adaptive_fps::select_decision` | Pure policy; production call sites were not found outside the unconsumed loop adapter. Module documents missing OS/config/tick integration. | **Test-only/unconsumed** |
| GUI `AdaptiveFpsLoop` | Has mutable state and a sink abstraction, but all discovered constructions are tests. | **Test-only; dead in production** |
| `display_platform_probe` | Pure-data schema explicitly defers CADisplayLink/DWM/Linux protocol probes (`display_platform_probe.rs:1-47`). | **Unconsumed schema** |
| `display_pipeline` | Pure policy explicitly defers platform probes, CADisplayLink configuration, and paint/present wiring (`display_pipeline.rs:1-41`). | **Test-only/unconsumed** |
| `vrr_negotiation` | A second pure negotiation contract; no platform API implementation or live present caller was found. | **Test-only/unconsumed; overlapping authority** |

There are currently overlapping policy authorities for refresh/presentation.
`adaptive_fps`, `vrr_negotiation`, and `display_pipeline` should not each grow
their own event loop. `.8.5` needs one production coordinator that consumes
their decisions or deliberately retires overlap after a proven cutover.

Animations are paced independently by `animation_fps` (default 10) through
timer math in `crates/frankenterm-gui/src/colorease.rs:98-120`; the default and
`max_fps` defaults are in `frankenterm/config/src/config.rs:2506-2512`.
The animation interval is not currently governed by the same adaptive target.
That correctness and policy relationship belongs to `.8.5.2`.

## Renderer and presentation reality

### OpenGL and WebGPU

`FrontEndSelection` defaults to OpenGL
(`frankenterm/config/src/frontend.rs:5-11`). `TermWindow` creates either an
OpenGL/Glium or WebGPU context according to config, not according to the macOS
backend selector (`termwindow/mod.rs:2472-2509`). Both are therefore production
paths; OpenGL is the default and WebGPU is config-selected.

The WebGPU surface is configured with `PresentMode::Fifo` and desired maximum
frame latency 2 (`crates/frankenterm-gui/src/termwindow/webgpu.rs:1240-1354`).
The live draw clears the target, walks all render layers/buffers, submits the
encoder, and unconditionally calls `output.present()`
(`termwindow/render/draw.rs:21-194`). It does not consult `PresentAction`,
`FrameDeduplicator`, VRR negotiation, direct-scanout eligibility, or mechanism
telemetry.

The WebGPU present/tearing/scanout helpers are explicitly marked
`#[allow(dead_code)]` (`termwindow/webgpu.rs:198-297`, `:317-390`). Their
callers are other helpers and tests, not the production `call_draw_webgpu`.
They are **compiled, test-only policy**, not a live present path.

### CAMetalLayer is not direct Metal renderer proof

The macOS native view creates a `CAMetalLayer`
(`frankenterm/window/src/os/macos/window.rs:3120-3131`). This establishes the
backing-layer type only. It does not establish a FrankenTerm-owned direct Metal
renderer, `MTLCommandBuffer` path, drawable scheduling, or
`presentDrawable:afterMinimumDuration:`. OpenGL/ANGLE may also use a Metal layer
internally, which is still not the proposed direct backend.

No `frankenterm-renderer-metal` crate or live direct-renderer dispatch was found.
`frankenterm-core/src/macos_backend_select.rs:1-46` explicitly says that crate,
CAMetalLayer integration, synchronization, shaders, and routing are deferred.
Its pure selector nevertheless defaults to `MetalDirect` on supported Apple
Silicon (`:199-263`). GUI startup probes and logs that result
(`crates/frankenterm-gui/src/main.rs:115-205`), but renderer construction ignores
it and follows `config.front_end`.

That is more than an incomplete optimization: on a supported Apple Silicon Mac,
startup can log `backend=MetalDirect` while the actual default renderer is
OpenGL. Until dispatch exists, the log must be treated as a **policy
recommendation, not an observed mechanism**. A follow-up correctness bead must
either route the selection into the renderer or change the field/message so it
cannot claim an uninstantiated backend. That repair is pinned as
`ft-interactive-systems-performance-4tenz.8.1.1`.

### Draw-error propagation gap

`paint_impl` calls `self.call_draw(frame).ok()` and then advances to dirty-line
clearing (`termwindow/render/paint.rs:149-157`). Since `paint_impl` returns `()`,
`do_paint_webgpu_impl` always returns `Ok(true)`
(`termwindow/mod.rs:2782-2784`). Therefore WebGPU acquisition errors from
`call_draw_webgpu` cannot reach the surrounding classify/reconfigure/retry path
at `termwindow/mod.rs:2746-2779`.

This creates a correctness and latency risk: a failed present can be followed
by dirty-state clearing, and the apparently live retry branch is unreachable
for those errors. It requires its own fix/proof bead. The repair should
propagate the error, retain or restore damage until a successful draw/present,
and count classified failures/retries; it should not merely add another log.
The P0 repair is pinned as
`ft-interactive-systems-performance-4tenz.8.2.1`.

### Presentation substrate classification

| Substrate | Production reality | Classification |
|---|---|---|
| Glium/OpenGL draw | Constructed for default frontend and used by `call_draw`. | **Live; default** |
| WebGPU draw/present | Constructed when configured; FIFO surface; unconditional submit/present. | **Live; config-selected** |
| `WebGpuPresentProbeInputs` / tearing-free/direct-scanout decisions | Marked dead-code; used by helpers/tests, not `call_draw_webgpu`. | **Test-only; dead in production** |
| Native `CAMetalLayer` backing view | Created by macOS window layer code. | **Live backing surface; unproven renderer mechanism** |
| `MacosBackend::MetalDirect` selector | Pure selector is logged, but no render dispatch consumes it. | **Live log of unconsumed policy; backend dead** |
| CADisplayLink / preferred frame-rate range | Named in policies and deferred-work comments; no live API implementation found. | **Absent/unproven** |
| VRR and direct scanout | Pure decision tables only; actual protocol/platform/present wiring absent. | **Absent/unproven** |

## Producer-consumer matrix

The following matrix makes the missing edges explicit. “Consumer” means a
production call or state mutation, not a test assertion or documentation claim.

| Producer / signal | Produced at | Current production consumer | Result / missing edge |
|---|---|---|---|
| AppKit invalidation request | macOS `WindowInner::invalidate` | AppKit `draw_rect` | **Live**, coalesced behind timer throttle. |
| AppKit resize dimensions/DPI/live flag | macOS `did_resize` | `TermWindow::resize` | **Live**. |
| `ScreenInfo.max_fps` | macOS screen probe | No frame-budget or scheduler consumer found | **Unconsumed** display fact. |
| Config `max_fps` | config | macOS/X11/Windows repaint sleeps and decoded image-frame minimums | **Live** cap/fallback; not adaptive or phase-aligned. |
| Config `animation_fps` | config | color-ease animation interval | **Live**, separate pacing authority. |
| `DirtyEventSource::Resize` | `resize` and `apply_dimensions` | dirty bitmaps/source counters | **Live but duplicated** per accepted resize. |
| PTY/selection/cursor/focus/font/theme damage | TermWindow/pane paths | dirty bitmap and line-cache accounting | **Live**, but bitmap does not select the line iteration set. |
| `quad_generation` | resize/focus/config paths | `LineQuadCacheKey` | **Live and coarse**; resize invalidates all line-cache keys. |
| Dirty-line `iter_dirty()` | bitmap | Test-only `TiledGridLayer` | **No live render consumer**. |
| `FrameBudget` decision | `paint_impl` | required/cosmetic operation gates and follow-up invalidate | **Live**, fixed at 60 Hz. |
| Idle scheduler decision | `poll_idle_scheduler` | Caller discards return value | **Unconsumed scheduling output**. |
| Adaptive FPS decision | core pure policy / GUI loop | No native timer, frame budget, or present sink | **Unconsumed**. |
| Display capability probe schema | `display_platform_probe` | Dead WebGPU decision helper | **No platform producer and no live consumer**. |
| VRR/present action | `display_pipeline` / `vrr_negotiation` | No live pre-present branch | **Unconsumed**. |
| macOS backend selection | GUI startup probe | Info log only | **Unconsumed by renderer; potentially misleading**. |
| WebGPU surface texture | `call_draw_webgpu` | full render pass, queue submit, present | **Live**. |
| WebGPU draw error | `call_draw` result | Converted to `Option` and discarded | **Lost**; outer retry cannot observe it. |
| Paint duration histograms | pane/screen/paint helpers | Process-local metrics recorder / optional stats output | **Live but not a joined retained trace**. |
| Dirty/frame-budget/idle snapshot accessors | `TermWindow` | No `doctor`/robot call site found by exact-name search | **Unconsumed operator surface**. |
| Renderer SLO artifacts | benches/headless fixtures | SLO catalog and render-parity ledger | **Retained proxy/substrate evidence only**, not the live graph above. |

## Telemetry and attestation truth

### Live instrumentation

The production renderer records duration histograms for `gui.paint.impl`
(`render/paint.rs:175-182`), pane-line paint (`render/pane.rs:824-846`),
screen-line render (`render/screen_line.rs:779`), and cached cluster shaping
(`render/mod.rs:930`). `TermWindow` also accumulates dirty-source, cache-reuse,
frame-budget, elastic-buffer, and idle-detector state.

This is useful local observability, but it does not presently answer the key
campaign question. There is no retained per-frame record joining:

- native event and invalidation timestamps;
- damage cause and generation changes;
- panes/rows visited, rebuilt, and reused;
- shaping/atlas/quad allocation time;
- scheduler deadline, wake reason, and lateness;
- selected renderer and *observed* present mechanism;
- queue submission, present result, and display completion;
- input event or resize gesture identity.

Exact-name source searches found definitions of `dirty_lines_telemetry`,
`frame_budget_telemetry`, `elastic_buffer_telemetry`, and
`idle_detector_doctor_snapshot`, but no production doctor/robot consumer. Any
comments saying those snapshots are already consumed by `ft doctor` are stale
until that edge exists.

### Retained evidence

The renderer SLO catalog itself states the relevant boundaries:

- RQ-S1 is a synthetic dirty-row loop and does not enter TermWindow, mux/pane
  resize, reflow, shaping, GPU, or display present
  (`docs/perf/resize-quality-slo.md:62-88` and
  `docs/perf/resize-quality-slo.json:37-72`).
- The macOS RQ-S2 artifact is a loaded-host, headless, forced-readback proxy
  that measured over target. It omits native event delivery, mux/PTY,
  production invalidation, scan-out, and photons, and is neither an upper nor
  lower bound (`resize-quality-slo.md:96-114`, `:138-147`; also
  `docs/attestations/tui/render-parity.json:189-273`).
- The render-parity closeout remains blocked pending missing retained runs
  (`render-parity.json:189-197`).
- The release manifest has render-parity artifact slots
  (`docs/attestations/manifest.json:53-82`), but no dedicated artifact that
  attests the live damage-to-present mechanism and timing chain.

The parent negative-evidence ledger already forbids two tempting
misinterpretations:

- `IS-N001`: do not use RQ-S1 as live 200-pane GUI resize proof.
- `IS-N002`: do not call the headless RQ-S2 proxy physical input-to-photon.

It also records `IS-N007`: do not repeat generic Apple Metal structure-of-arrays
or vertex-bandwidth work without a live profile attributing at least 0.5% of
frame time to the targeted operation. This audit adds no evidence that would
reopen that retired hypothesis.

## Canonical reuse and cutover architecture

The campaign should converge on the following single authority chain:

```text
typed DamageEvent / GenerationDomains
  -> one TermWindow damage accumulator
  -> one FramePacingCoordinator per native window
       inputs: damage, input urgency, animation deadline, a11y/recording,
               power/thermal policy, display capabilities, backend health
       owns: pending-frame state, deadline/tick, fallback timer,
             display migration, wake reason, mechanism telemetry
  -> existing pane line cache + dirty-row iteration
  -> RenderState geometry/build
  -> selected live backend (OpenGL or WebGPU; direct Metal only if real)
  -> pre-present decision at the actual submission boundary
  -> present result/completion telemetry
  -> clear damage only after a successful, attributable frame outcome
```

### Reuse points

1. Keep `WindowOps::invalidate` as the cross-platform demand ingress. Native
   backends should coalesce demand, while the coordinator owns why and when the
   next frame is due.
2. Keep `DirtyLineBitmap` as the canonical per-pane row-damage representation.
   Make it select iteration after `.8.2` establishes correctness semantics.
3. Keep the existing live `LineQuadCache` for `.8.4`. Split its generation key
   into content/shape/layout/projection domains so same-grid surface changes do
   not invalidate unchanged shaped rows.
4. Keep `adaptive_fps::select_decision` as the power/thermal/quality policy if
   its semantics remain suitable. Feed its target into one capability clamp and
   scheduler; do not let it become another timer loop.
5. Treat `display_pipeline::PresentAction` as usable only when it is consulted
   immediately before a real backend submission/present and its mechanism is
   confirmed by a live capability probe.
6. Put CADisplayLink lifecycle, screen rebinding, and the timer fallback in the
   macOS native scheduler. Pacing must work for the default OpenGL path as well
   as WebGPU; a WebGPU-only cutover would leave the default frontend untouched.
7. Use one typed trace schema from `.8.2` for line-cache telemetry, pacing,
   overlay/visual correctness, and the 20/50/200-pane live rig. Do not create
   mutually incomparable counters for each bead.

### Cutover gaps that must be closed

- Collapse duplicate resize invalidation into one semantically complete event.
- Separate content/shape invalidation from surface projection and presentation.
- Move dirty discovery that is currently performed inside paint early enough to
  drive scheduling and row selection without missing cursor/selection changes.
- Make dirty-row iteration real while retaining safe whole-screen fallbacks for
  font, theme, DPI, focus-border, modal, tab-bar, and uncertain damage.
- Bind actual display capability to frame budget and scheduler; update it when
  the window moves between screens.
- Reconcile config `max_fps`, `animation_fps`, display limits, power/thermal
  policy, and input-urgent wakeups under one documented precedence contract.
- Propagate renderer failures and clear damage only after success.
- Record the actual selected backend/present mechanism, not a pure-policy
  recommendation.
- Add an explicit timer fallback and fallback-reason telemetry for every
  platform-specific pacing mechanism.
- Retain a live trace artifact before promoting any user-visible latency/FPS or
  visual-quality claim.

## Dependency handoff for `.8.2` through `.8.5`

The Beads relationships below were read statically during this audit. They are
entry gates, not claims that the dependencies are complete.

### `.8.2` — typed damage/invalidation and trace contract

**Dependencies:** `.8.1` and `.6.1`.

**Blocks:** `.8.3` and `.8.4`.

Required outputs before downstream work:

- typed damage causes that distinguish cell content, cursor/selection, pane
  layout, surface size/projection, scale/DPI/font, theme, overlay/tab bar, and
  explicit unknown/full damage;
- separate monotonic generation domains with wrap-safe comparison semantics;
- one event identity propagated from native resize/invalidation through render;
- producer/consumer assertions so an unconsumed damage source fails tests;
- a retained trace schema with timestamps and counts at native event, damage
  accumulation, pane/row build, submission, present result, and completion;
- a clear-on-success contract, including draw error/retry behavior;
- bounded fallback behavior when exact damage is unavailable.

The P0 draw-failure/damage-recovery bead
`ft-interactive-systems-performance-4tenz.8.2.1` is part of this contract's
correctness floor: no downstream optimization may clear or retire damage for a
frame whose draw/present outcome was not successful and attributable.

Do not base the contract on `TiledGridLayer`, `LayerStack`, or
`per_row_quad_cache` merely because they exist. They are not production
integration points at this snapshot.

### `.8.3` — remove duplicate resize invalidation

**Dependency:** `.8.2`.

This bead should do one thing: make one accepted native resize produce one
typed damage transition and one full-pane traversal only when the typed contract
requires it. It should prove:

- no duplicate `quad_generation`/replacement generation transition;
- no duplicate resize-source counter increment;
- state-only changes still invalidate the required decorations;
- scale/DPI changes still invalidate shaping/glyph state;
- no lost repaint when a resize is coalesced or draw fails.

Keep display-link work and same-grid projection reuse out of this bead so its
effect is independently measurable.

### `.8.4` — same-grid reprojection

**Dependencies:** `.8.2` and `.6.3`.

Use the existing line cache, but remove pure surface projection from the shaped
row-content identity. When rows/columns, font metrics, content, selection,
cursor semantics, and pane layout are unchanged, a pixel-only resize should
update projection/surface state without reshaping or rebuilding unchanged row
quads.

Acceptance must include correctness of tab bar, modal, background images,
floating panes/borders, cursor, ligatures, images, IME composition, and retina
scale transitions. Performance proof must use the live 20/50/200-pane resize
rig, report rows visited/rebuilt/reused and present timing, and compare first
correct frame—not just CPU loop time.

### `.8.5` — adaptive native frame pacing

**Dependencies:** `.8.1` and `.3.4`.

**Children:** `.8.5.1` (`max_fps` validation/timer safety) and `.8.5.2`
(`animation_fps` correctness).

**Blocks:** `.8.6`, `.8.10`, `.8.11`, and `.8.12`.

The implementation must replace, not sit beside, the macOS fixed repaint timer
for the proven path. It needs:

- a real display capability producer and screen-change rebinding;
- CADisplayLink or the chosen native callback feeding the one pacing
  coordinator, with a bounded fixed-timer fallback;
- explicit behavior for both default OpenGL and configured WebGPU, or a
  separately proven/configured frontend change;
- target-rate precedence across display capability, `max_fps`, adaptive
  power/thermal policy, animation deadlines, and input urgency;
- refresh-rate updates to `FrameBudget` rather than permanent 60 Hz;
- observed mechanism, requested/actual rate, deadline miss, wake reason,
  fallback reason, submit/present result, and completion telemetry;
- tests for display migration, 60/120 Hz, variable ranges, occlusion, sleep/wake,
  recording/a11y force-present, and callback loss;
- target traces on the intended Apple Silicon systems before any M4/M5 claim.

`.8.5.1` makes the fallback safe; it does not establish adaptive pacing.
`.8.5.2` must prevent an independent animation timer from violating the final
coordinator's target-rate contract.

## Negative-evidence ledger and explicit nonclaims

The following findings are retained so later agents do not re-infer capability
from type names, comments, or synthetic evidence.

| ID | Negative evidence | Consequence / retry condition |
|---|---|---|
| RA-N001 | No production call was found for `LayerStack`, the `per_row_quad_cache` planner, `redraw_predicate::evaluate`, or `FrameDeduplicator`; discovered uses are tests or deferred helpers. | Do not cite them as live. Reclassify only with an exact production call edge and proof. |
| RA-N002 | `DirtyLineBitmap::iter_dirty` is consumed by the dead/test `TiledGridLayer`, while production still iterates all visible lines. | Do not claim dirty-row-only rendering until row selection is on the live pane path. |
| RA-N003 | Native resize advances `quad_generation` and marks all panes twice. | Treat `.8.3` as a measured, isolated duplicate-removal lever; do not claim duplicate presents. |
| RA-N004 | `ScreenInfo.max_fps` is populated but not connected to frame budget or pacing. | A display-frequency field is not adaptive scheduling proof. |
| RA-N005 | Adaptive FPS, display probe, VRR, scanout, and `PresentAction` are pure/unconsumed policy. | Reclassify only after platform probes and actual pre-present dispatch are live and observable. |
| RA-N006 | WebGPU uses FIFO and unconditionally submits/presents; OpenGL is the default. | A WebGPU-only or policy-only optimization cannot support a general FrankenTerm claim. |
| RA-N007 | A `CAMetalLayer` exists, but the proposed direct Metal crate/dispatch does not. | Do not claim direct Metal, sub-vsync drawable scheduling, or M4/M5 specialization. |
| RA-N008 | Startup can log pure-policy `MetalDirect` without instantiating it. | Treat the log as misleading until corrected or wired. |
| RA-N009 | Draw errors are discarded before dirty clearing; WebGPU retry cannot observe them. | Fix error/damage semantics before using retry counts or successful-frame latency claims. |
| RA-N010 | Current histograms and snapshots do not form a retained event-to-present trace. | Require the `.8.2` trace and live rigs for causal attribution. |
| RA-N011 | RQ-S1 is synthetic and RQ-S2 is an over-target headless proxy. | Preserve `IS-N001`/`IS-N002`; neither closes live GUI performance. |
| RA-N012 | Prior generic Metal SoA/vertex work was rejected (`IS-N007`). | Reopen only after a live M4/M5 profile attributes at least 0.5% to that operation. |

Accordingly, this audit makes none of the following claims:

- that current FrankenTerm reaches 60 or 120 presented FPS during resize;
- that keypress-to-photon latency meets any target;
- that the active backend on this Mac is OpenGL, WebGPU, ANGLE/Metal, or direct
  Metal (the configured runtime was intentionally not inspected);
- that any live session exhibits the duplicate bookkeeping at a particular
  rate or cost;
- that dirty-line tracking reduces production line iteration;
- that CAMetalLayer implies direct Metal command encoding;
- that CADisplayLink, VRR, XPresent async, direct scanout, frame deduplication,
  or conditional redraw is active;
- that a passing unit test, synthetic Criterion run, or headless GPU proxy
  validates the production GUI path;
- that Apple Silicon or high-core-count AMD specialization is warranted before
  correlated live profiles identify the limiting stages.

## Static audit method

The audit traced named constructors, methods, state fields, and enum variants
from native event production through GUI dispatch and rendering, then searched
for all callers of the optimization substrates. It cross-checked source comments
against actual construction/call sites and compared the result with the SLO and
attestation ledgers. Absence findings are bounded to the recorded source
snapshot and should be refreshed after any relevant cutover.

No runtime observation was performed. That boundary is load-bearing: the next
stage is to make the live graph observable and only then optimize from retained
causal evidence.
