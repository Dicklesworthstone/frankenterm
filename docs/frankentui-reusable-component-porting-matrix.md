# FrankentUI Reusable Component Porting Matrix

**Bead:** `wa-1u90p.1.6`  
**Date:** 2026-02-13  
**Goal:** Map reusable components from `/Users/jemanuel/projects/frankentui` to concrete `frankenterm` integration points, with adaptation notes, risks, and expected gains.

## Scope

This matrix targets components most relevant to the current resize/reflow and rendering-performance track:

- line reflow quality and complexity control
- dirty-region diff efficiency
- allocation churn during resize/render loops
- large-list rendering scalability in TUI surfaces

## Porting Matrix

| Component | FrankentUI source evidence | Frankenterm candidate integration points | Adaptation notes | Complexity | Risks | Expected gains |
|---|---|---|---|---|---|---|
| Monospace Knuth-Plass line breaking | `/Users/jemanuel/projects/frankentui/crates/ftui-text/src/wrap.rs:570` and `/Users/jemanuel/projects/frankentui/crates/ftui-text/src/wrap.rs:727` | `frankenterm/term/src/screen.rs:127`, `frankenterm/term/src/screen.rs:192`, `frankenterm/surface/src/line/line.rs:212` | Replace greedy `Line::wrap` path used during resize reflow with bounded-lookahead optimal breaks, but keep greedy fallback for overflow/degenerate cases. Preserve cursor remap behavior in `rewrap_lines`. | Medium | Cursor placement regressions on wrap boundaries; worst-case CPU if bounds are misconfigured. | Better reflow stability and less ragged wrapping during width churn; lower visual jumpiness in resize storms. |
| Dirty-span tracking + dirty bitmap | `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/buffer.rs:21`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/buffer.rs:393` | `frankenterm/mux/src/renderable.rs:63`, `frankenterm/mux/src/localpane.rs:186`, `frankenterm/mux/src/termwiztermtab.rs:141` | Current mux interface exposes changed rows only. Introduce optional per-row span coverage alongside row dirtyness for sparse updates, with full-row fallback on overflow. | High | Dirty-soundness bugs can cause missed redraws (stale cells). | Reduced scan cells for sparse updates; improved throughput on large panes with localized changes. |
| Bayesian diff strategy selector | `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/diff_strategy.rs:11`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/diff_strategy.rs:555` | `frankenterm/mux/src/renderable.rs:63`, `crates/frankenterm-core/src/degradation.rs:39` | Add a selector deciding full diff vs dirty-only vs redraw based on observed change rates. Emit strategy evidence for diagnostics and wire severe uncertainty to degradation reporting. | High | Incorrect cost calibration can oscillate strategy choices; harder debugging if evidence not surfaced. | More predictable frame cost under mixed workloads; adaptive behavior instead of static diff policy. |
| Adaptive double buffer + resize headroom | `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/buffer.rs:1258`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/buffer.rs:1328`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/buffer.rs:1584` | `frankenterm/term/src/screen.rs:203`, `frankenterm/term/src/screen.rs:286`, `frankenterm/term/src/screen.rs:385` | Port capacity/headroom + shrink-threshold policy to resize-sensitive buffers to avoid repeated allocate/free cycles on rapid size oscillation. Keep clear-on-reuse semantics to prevent ghosting artifacts. | Medium | Retained-capacity memory overhead if thresholds are too conservative. | Lower allocator churn in resize storms; steadier tail latency during pane/tab resize operations. |
| Grapheme pooling / slot reuse | `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/grapheme_pool.rs:63`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/grapheme_pool.rs:189`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/grapheme_pool.rs:245` | `frankenterm/surface/src/cellcluster.rs:20`, `frankenterm/surface/src/line/line.rs:156`, `frankenterm/term/src/terminalstate/performer.rs:367` | Introduce interned grapheme IDs for repeated multi-codepoint clusters across lines/frames. Keep fast path for simple ASCII cells unchanged. | High | Lifetime/ownership complexity across line storage and render paths; potential pool leaks without robust GC hooks. | Lower heap pressure from repeated emoji/ZWJ clusters; reduced duplicate grapheme storage. |
| Render budget controller + degradation ladder | `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/budget.rs:49`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/budget.rs:397`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/budget.rs:620` | `crates/frankenterm-core/src/tui/ftui_stub.rs:1299`, `crates/frankenterm-core/src/tui/ftui_stub.rs:1349`, `crates/frankenterm-core/src/degradation.rs:39` | Add per-frame budget telemetry and staged fidelity reduction (decorate less, then skip non-essential) instead of binary “draw everything” behavior under overload. | Medium | User-visible quality drop if thresholds too aggressive; integration with existing degradation policy needs clear ownership. | Better responsiveness under bursty output; fewer perceived UI stalls. |
| Allocation leak detector (CUSUM + e-process) | `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/alloc_budget.rs:3`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/alloc_budget.rs:93`, `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/alloc_budget.rs:172` | `crates/frankenterm-core/src/degradation.rs:114`, `crates/frankenterm-core/src/circuit_breaker.rs:147` | Add frame-allocation observability with evidence ledger entries and thresholded alerts feeding existing degradation/circuit-breaker paths. | Medium | False positives if warmup/calibration is weak; telemetry volume growth if not sampled. | Faster detection of regressions in long-running sessions; improved triage evidence quality. |
| Virtualized large-list rendering (overscan + Fenwick heights) | `/Users/jemanuel/projects/frankentui/crates/ftui-widgets/src/virtualized.rs:45`, `/Users/jemanuel/projects/frankentui/crates/ftui-widgets/src/virtualized.rs:219`, `/Users/jemanuel/projects/frankentui/crates/ftui-widgets/src/virtualized.rs:530` | `crates/frankenterm-core/src/tui/ftui_stub.rs:375`, `crates/frankenterm-core/src/tui/ftui_stub.rs:418`, `crates/frankenterm-core/src/tui/ftui_stub.rs:2705`, `crates/frankenterm-core/src/tui/query.rs:804` | Replace full filtered-index rebuild + top-anchored rendering with scroll-window virtualization state (offset, visible count, overscan). Keep selection semantics but render only visible slice. | Medium | Behavioral drift in keyboard navigation semantics if offset/selection sync is wrong. | Scales better for large event/history/timeline datasets; lower per-frame CPU and allocation load. |

## High-Value Early Sequence

1. **Virtualized lists + filtered-index refactor** (lowest integration risk in current TUI code).
2. **Adaptive resize capacity policy** in resize hot paths.
3. **Knuth-Plass reflow prototype** behind a guard/flag for resize-only path.
4. **Dirty-span + adaptive diff strategy** after instrumentation is in place.
5. **Grapheme pooling** after explicit ownership model is agreed for line/cell storage.

## Validation Hooks (per component)

- Reflow components: resize stress scenarios from `docs/resize-baseline-scenarios.md`.
- TUI list components: snapshot + E2E keyboard navigation in `crates/frankenterm-core/src/tui/ftui_stub.rs` tests.
- Diff/dirty components: add counters for scanned cells, emitted cells, dirty coverage, chosen strategy.
- Allocation components: report detector ledger summaries into existing degradation/circuit-breaker diagnostics.

## Notes

- `truncate_str` in `crates/frankenterm-core/src/tui/ftui_stub.rs:2919` currently slices by byte length; any wrapping/truncation upgrades should be Unicode/grapheme-width safe.
- Current global degradation subsystem list in `crates/frankenterm-core/src/degradation.rs:42` does not include a render-specific subsystem; render-budget integration likely needs one.
