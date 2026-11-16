# Ghostty memory architecture analysis (scrollback, cells, allocation)

Bead: `wa-3bja.2`

This document analyzes Ghostty’s memory model for terminal state (scrollback + cells) and compares it with vendored WezTerm in this repo, with an eye toward actionable FrankenTerm improvements.

Scope:
- Ghostty code: `legacy_ghostty/src/terminal/`
- Vendored WezTerm terminal model: `frankenterm/term/` + `frankenterm/cell/`

---

## 1) Scrollback storage

### Ghostty: byte-budgeted, page-based scrollback with pruning + reuse

Ghostty’s terminal `Screen` is backed by a `PageList` of `Page`s. The scrollback limit is expressed in **bytes**, not lines:
- `legacy_ghostty/src/terminal/Screen.zig:252` (`Options.max_scrollback`)
- `legacy_ghostty/src/terminal/Screen.zig:291` (passes `max_scrollback` to `PageList.init`)

Key design points:
- **Pages are fixed-size allocations** (standard capacity) backed by page-aligned memory pools:
  - `legacy_ghostty/src/terminal/PageList.zig:60` (`PagePool` is `MemoryPoolAligned` with page alignment)
  - `legacy_ghostty/src/terminal/PageList.zig:330` (`std_size` derived from a standard page layout)
- The `PageList` maintains a `page_size` counter and an `explicit_max_size` byte budget:
  - `legacy_ghostty/src/terminal/PageList.zig:143` (`page_size`)
  - `legacy_ghostty/src/terminal/PageList.zig:149` (`explicit_max_size`)
  - `legacy_ghostty/src/terminal/PageList.zig:3051` (`maxSize()` clamps to minimum needed for active area)
- When more rows are needed and a new page would exceed the byte budget, Ghostty **prunes the oldest page** and **reuses its memory** (for standard-size pages):
  - `legacy_ghostty/src/terminal/PageList.zig:3084` (prune decision)
  - `legacy_ghostty/src/terminal/PageList.zig:3091` (pop first page)
  - `legacy_ghostty/src/terminal/PageList.zig:3148` (tracked pins moved/garbage-marked)
  - `legacy_ghostty/src/terminal/PageList.zig:3177` (memset + re-init + insert at end)

Observations:
- The eviction granularity is **page-level** (not per-line). This keeps pruning cheap (linked-list ops + counter math) and amortizes allocations.
- Memory reuse avoids allocator churn in bursty output regimes.
- Byte budgeting aligns better with real memory consumption than “N lines” because line/cell content can vary wildly in size.

### WezTerm: line-budgeted scrollback with `VecDeque<Line>`

WezTerm’s `Screen` stores the entire scrollback as a `VecDeque<Line>` with capacity for `physical_rows + scrollback_size`:
- `frankenterm/term/src/screen.rs:13` (`lines: VecDeque<Line>`)
- `frankenterm/term/src/screen.rs:55` (capacity = physical + scrollback lines)

Observations:
- The budget is primarily **line-count based** (`TerminalConfiguration::scrollback_size()`), which is easy to reason about but can hide heavy per-line/per-cell allocations.
- The representation is not obviously page-aligned or pool-allocated; allocator behavior depends on standard Rust collections and whatever `Line`/`Cell` allocate internally.

---

## 2) Terminal cell representation

### Ghostty: `Cell` is a packed `u64` + page-local side tables

Ghostty’s cell is a bit-packed `u64` with small enums/flags and an inline codepoint:
- `legacy_ghostty/src/terminal/page.zig:1962` (`pub const Cell = packed struct(u64)`)

Notable properties:
- Inline scalar for the common case: `content.codepoint: u21`.
- Wide character handling via a tiny `Wide` enum (narrow/wide/spacer variants).
- Style is an ID (`style_id`) into a page-local `StyleSet`.
- Multi-codepoint graphemes are stored out-of-line in a page-local grapheme allocator + map:
  - `legacy_ghostty/src/terminal/page.zig:25` (bitmap allocator for graphemes, 4-codepoint chunks)
  - `legacy_ghostty/src/terminal/page.zig:137` (`grapheme_alloc`, `grapheme_map`)
- Hyperlinks are stored out-of-line via a page-local map+set:
  - `legacy_ghostty/src/terminal/page.zig:151` (`hyperlink_map`, `hyperlink_set`)
- Small shared strings (OSC8 IDs/URIs) use a bitmap allocator with a chosen chunk size:
  - `legacy_ghostty/src/terminal/page.zig:45` (32-byte chunk bitmap allocator)

Net effect: a very compact baseline cell representation (8 bytes per cell), with variable-sized features paid for only when needed and scoped to a page.

### WezTerm: `Cell` stores text + attributes as structs

WezTerm’s `Cell` is a struct holding `TeenyString` text and a `CellAttributes` struct:
- `frankenterm/cell/src/lib.rs:715` (`pub struct Cell { text: TeenyString, attrs: CellAttributes }`)

Observations:
- This is flexible and feature-rich, but tends to be **heavier per-cell** than Ghostty’s packed `u64`.
- Memory locality and overhead depend on the internal layout of `TeenyString` and `CellAttributes`.

---

## 3) Allocation patterns

Ghostty uses multiple targeted allocation strategies:
- **Page-aligned bulk allocations** for pages, pooled and reused:
  - `legacy_ghostty/src/terminal/PageList.zig:60` (`MemoryPoolAligned` for pages)
- **Intrusive linked list** for page nodes:
  - `legacy_ghostty/src/terminal/PageList.zig:36` (`IntrusiveDoublyLinkedList`)
- **Bitmap allocators** for small fixed-size chunks (graphemes, strings):
  - `legacy_ghostty/src/terminal/page.zig:18` (graphemes)
  - `legacy_ghostty/src/terminal/page.zig:45` (strings)

This combination strongly biases toward:
- fewer allocations on hot paths,
- better locality for scan/render,
- bounded worst-case behavior under heavy scrollback churn.

---

## 4) Memory pressure handling

Ghostty’s primary memory pressure mechanism (in the terminal model) is its explicit byte budget for scrollback plus pruning on growth:
- `legacy_ghostty/src/terminal/Screen.zig:252` (`max_scrollback` bytes)
- `legacy_ghostty/src/terminal/PageList.zig:3066`..`3189` (`PageList.grow` prune path)

The code analyzed here doesn’t show an OS-level memory monitor in this layer; instead it provides a deterministic bound on terminal-state growth and uses eviction when crossing it.

---

## 5) FrankenTerm opportunities (3–5 actionable ideas)

These are framed as “Ghostty approach → WezTerm today → FrankenTerm opportunity”.

### Idea A — Prefer byte budgets + coarse chunk pruning for high-volume buffers

- **Ghostty approach:** scrollback limit is bytes; evict oldest pages when `page_size` would exceed `maxSize()`; reuse memory (`PageList.grow`) (`legacy_ghostty/src/terminal/PageList.zig:3066`..`3189`).
- **WezTerm today:** scrollback is line-based with `VecDeque<Line>` capacity (`frankenterm/term/src/screen.rs:13`..`62`).
- **FrankenTerm opportunity:** for the parts we control (capture pipeline buffers, segment staging, snapshot artifacts), adopt **byte-budgeted pools** with coarse eviction/reuse. This aligns naturally with ft’s operator knobs (`retention_max_mb`, per-pane priorities/budgets) and reduces allocator churn when an agent floods output.
  - Expected impact: fewer allocator spikes, predictable memory envelopes under bursty workloads.

### Idea B — Pursue a “packed cell + side tables” design for vendored WezTerm term (longer-horizon)

- **Ghostty approach:** 8-byte packed cell with style IDs and optional out-of-line tables (`legacy_ghostty/src/terminal/page.zig:1962`).
- **WezTerm today:** richer per-cell struct (`frankenterm/cell/src/lib.rs:715`) and scrollback of `Line`s (`frankenterm/term/src/screen.rs:13`).
- **FrankenTerm opportunity:** if mux-server memory is a real bottleneck (50–200 panes), investigate a phased refactor in the vendored term model toward:
  - packed baseline cell,
  - per-page style tables,
  - sparse allocation for rare features (graphemes/hyperlinks).
  - Expected impact: large reductions in RSS proportional to rows×cols×panes; better cache behavior in render/scrollback traversal.

### Idea C — Bitmap/slab allocators for “many small things”

- **Ghostty approach:** bitmap allocators for grapheme chunks and small shared strings, scoped to a page (`legacy_ghostty/src/terminal/page.zig:18`..`66`).
- **WezTerm today:** standard Rust allocation patterns for strings/attrs; performance depends on allocator and object lifetimes.
- **FrankenTerm opportunity:** apply a similar pattern where we have many small allocations with clear lifetime domains (e.g., per-pane session metadata, per-capture ephemeral parses, per-workflow scratch buffers). A small slab/bitmap allocator can reduce fragmentation and speed up teardown.
  - Expected impact: lower fragmentation, fewer syscalls, improved tail latency under load.

### Idea D — “Tracked pin” semantics for stable references across eviction/compaction

- **Ghostty approach:** `PageList` maintains tracked pins and updates them on prune; pins can be marked garbage when their page is recycled (`legacy_ghostty/src/terminal/PageList.zig:3148`..`3164`).
- **WezTerm today:** stable row index offsets exist (`stable_row_index_offset`) but are primarily for scrollback indexing (`frankenterm/term/src/screen.rs:27`..`33`).
- **FrankenTerm opportunity:** for ft’s “bookmarks” and “session restore” surfaces, model stable anchors that survive:
  - retention eviction,
  - snapshot compaction,
  - DB vacuum/reindex.
  - Expected impact: fewer “bookmark drift” bugs and more robust session reconstruction.

### Idea E — Expose pruning/pressure metrics as first-class signals

- **Ghostty approach:** pruning is deterministic and happens at a single choke point (`PageList.grow`).
- **WezTerm today:** line-capacity-based; eviction is implicit via deque behavior.
- **FrankenTerm opportunity:** wherever we add byte-budget pruning/pooling, emit structured counters:
  - pages/chunks evicted,
  - bytes retained per pane,
  - prune reasons (budget exceeded vs explicit clear),
  - “garbage pin” events (if applicable).
  - Expected impact: makes memory incidents diagnosable and integrates with ft’s policy/automation.

---

## Next steps / follow-on beads

If we decide to act on these ideas, the highest leverage follow-on work is likely:
- byte-budgeted retention/pooling for ft capture+storage hot paths (ties into `wa-2ahu0`, `wa-3r5e`)
- a targeted RSS reduction project for vendored WezTerm term/mux server (ties into fork hardening `wa-3kxe`)

