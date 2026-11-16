# Ghostty Analysis Synthesis — Comparison Report and Improvement Roadmap

Bead: `wa-3bja.5`

This report synthesizes Ghostty analysis work into a prioritized set of improvement ideas for FrankenTerm.

Inputs (primary evidence docs):
- `evidence/ghostty/memory-architecture.md` (wa-3bja.2)
- `evidence/ghostty/io-pipeline.md` (wa-3bja.3)
- `evidence/ghostty/event-system.md` (wa-3bja.4)

Note: Where this report proposes implementation work, the derived beads **must** require `/porting-to-rust` and provide a language-neutral spec (no line-by-line Zig translation).

---

## Executive summary (top recommendations)

The highest leverage Ghostty patterns for FrankenTerm cluster into two themes:
1) **Stop work amplification** in high-frequency paths (capture → persist → detect → workflow → notify).
2) **Bound memory and contention** with explicit budgets, pooling, and coalesced notifications.

Top picks (ranked by impact/effort, and mapped to existing work where possible):

| Rank | Pattern | Why it matters | Existing beads / work |
|------|---------|----------------|------------------------|
| 1 | Coalesced wakeups + “drain then notify once” | Prevent notification storms; reduces task churn and lock contention under bursty output | `wa-x4rq` (native output coalescing + batching); `wa-7o4f` (mux notification coalescing + callback-outside-lock) |
| 2 | Data-plane vs control-plane split | Treat “bulk output deltas” differently from “structural events” (resize/title/pane lifecycle); simplifies backpressure and schemas | `wa-3dfxb.13` (native event hooks) + native event listener beads; `wa-x4rq` |
| 3 | Byte-budgeted pools + coarse eviction/reuse | Predictable memory envelopes under output floods; reduces allocator churn | `wa-2ahu0` (memory pressure engine), `wa-8vla` (mmap scrollback), `wa-3axa` (allocator work) |
| 4 | Keep bytes→state locality where possible | Avoid per-pane “byte shuttle” overhead; fewer threads/copies and less coordination | Native integration beads + runtime/tailer evolution; see wa-3bja.3 for WezTerm vs Ghostty thread model delta |
| 5 | “Consumed dirty” semantics for incremental work | Downstream stages should clear “work needed” flags once processed to avoid repeated rescans | `wa-x4rq` + event bus metrics; extend pattern scans/search/indexing similarly |

---

## 1) Quick wins (adopt immediately)

### 1.1 Coalesced notifications as the default

**What Ghostty does**
- Uses a coalescing async wakeup for redraw signals, then drains mailboxes and renders immediately.
- Coalesces “spammy” events (resize) with small windows and triggers one redraw after draining writer-thread messages.

**Language-neutral spec**
- For each high-rate source (pane output, state transitions, “pane output available”), keep a `pending` flag per key (pane/session).
- Producers set `pending=true` and notify a coalescing wakeup exactly once.
- The consumer drains all pending keys, processes them, and clears `pending`.
- Any “structural” events (pane destroyed, state change) should force-flush pending output for that key before processing the structural event.

**FrankenTerm mapping**
- `crates/frankenterm-core/src/runtime.rs` already implements this for native WezTerm output via `NativeOutputCoalescer` (wa-x4rq).
- Extend the same concept to:
  - workflow triggers (avoid repeated workflow runs per micro-event),
  - event bus publications (publish coalesced batches rather than singletons),
  - mux-output–style notifications when integrating with the vendored mux.

### 1.2 Separate data-plane deltas from control-plane events

**What Ghostty does**
- Bulk change is represented by dirty state in the terminal model.
- Small control signals are sent via bounded mailboxes and drained before rendering.

**Language-neutral spec**
- Define two event categories:
  - **data-plane**: “here are bytes/deltas” (high volume, batchable, persistence-critical)
  - **control-plane**: “pane resized/title changed/pane closed/prompt active” (lower volume, schema-rich)
- Allow independent backpressure policies per plane: merge/drop for data-plane vs always-deliver for control-plane (with bounded queue + priority ordering).

**FrankenTerm mapping**
- Native event hook design (`wa-3dfxb.13`) should adopt this split at the socket/protocol level.
- Storage schema and event bus types should reflect “bulk deltas” as first-class objects distinct from lifecycle events.

### 1.3 Byte-budgeted pools and eviction for bursty buffers

**What Ghostty does**
- Expresses scrollback capacity as a byte budget and evicts/reuses pages when crossing the budget.

**Language-neutral spec**
- For any “grows-with-output” buffer (capture staging, segment queues, scrollback caches), define:
  - a byte budget,
  - eviction granularity (page/chunk),
  - reuse strategy (pool reuse vs free),
  - metrics for evictions and pressure signals.

**FrankenTerm mapping**
- Align with existing memory pressure work (`wa-2ahu0`) and capture retention knobs (per-pane priorities/budgets).

---

## 2) Strategic improvements (worth investing in)

### 2.1 Packed baseline cells + side tables for rare features (memory/RSS)

**What Ghostty does**
- Represents most cells compactly and pays for complex features (graphemes, hyperlinks, style) via page-local side tables.

**Language-neutral spec**
- Design a baseline `Cell` representation optimized for the common case (single codepoint, common attrs).
- Store infrequent payloads out-of-line in per-chunk arenas/side tables with stable IDs.
- Keep eviction/pruning coarse and predictable.

**FrankenTerm mapping**
- This is a longer-horizon fork-hardening/perf effort; it should be evaluated with benchmarks before committing to a full refactor.

### 2.2 Lock + backpressure invariants (avoid deadlocks under load)

**What Ghostty does**
- When bounded queues fill, it explicitly wakes the consumer and may unlock the shared mutex to avoid lock inversion, then blocks/retries.

**Language-neutral spec**
- Define and enforce: “Never block on backpressure while holding a hot lock.”
- If a channel is bounded and `try_send` fails:
  - wake the consumer,
  - drop/retry/coalesce,
  - but do not hold locks across the blocking path.

**FrankenTerm mapping**
- Apply to persistence/detection/workflow channels and any future in-process subscription fan-out.

---

## 3) Future considerations (track, but defer)

- Async backend selection (epoll/kqueue/io_uring) as a first-class portability knob for future native event ingestion.
- More aggressive per-pane scheduling based on value-of-information (VOI) once base coalescing/backpressure is stable.

---

## 4) Ghostty ↔ Zellij comparison notes (for wa-2bai5 cross-reference)

This synthesis should be compared side-by-side with `wa-2bai5` once the Zellij synthesis exists. The key comparison axes to explicitly resolve:

1) **Event propagation**: wakeup coalescing strategy and notification fan-out model
2) **Concurrency model**: per-pane threads vs centralized event loop; structured cancellation semantics
3) **Memory discipline**: byte budgets and chunk eviction granularity vs line-based limits
4) **Persistence hooks**: what native events expose vs what must be inferred from polling/deltas

Recommendation: once `wa-2bai5` is complete, add a short “convergence vs divergence” appendix here with:
- patterns both agree on (high-confidence),
- patterns where they diverge (requires FrankenTerm-specific choice),
- unique innovations per system.

---

## 5) Proposed execution roadmap

1) Land coalescing + data/control-plane split across native events and workflow triggers (reduce storms first).
2) Extend memory pressure/budgeting to all grow-with-output buffers (predictability).
3) Evaluate deeper terminal/mux memory refactors (packed cells) with benchmarks before committing.

---

## 6) Follow-on beads (to be created/linked)

This section should list:
- Which recommendations map to existing beads (preferred).
- New beads created for genuinely uncovered work, each requiring `/porting-to-rust`.

### Existing beads to lean on

- Coalescing notifications: `wa-x4rq`
- Native event hook surface (data/control split): `wa-3dfxb.13` (and the in-progress native event listener beads)
- Memory pressure / budgeting: `wa-2ahu0`, `wa-3axa`, `wa-8vla`

### New beads created from this synthesis

- `wa-7o4f` — Mux notifications: coalesce `PaneOutput` and run subscribers outside lock
  - Derived from `evidence/ghostty/event-system.md` (Ghostty wakeup + mailbox drain model vs mux fan-out).
  - Requires `/porting-to-rust` (spec-first).
