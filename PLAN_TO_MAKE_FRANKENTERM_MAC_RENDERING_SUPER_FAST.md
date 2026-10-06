# PLAN TO MAKE FRANKENTERM MAC RENDERING SUPER FAST

> **The plan has been converted to beads.** Epic **`ft-yccm0`** has 94 child beads and 148 blocking edges. Its description holds the full plan-label -> bead legend, and each bead is self-contained. Treat the beads as the source of truth from here on; this file is the original narrative.
>
> **Primary acceptance test (T0).** This is the operator's own test: `time \cat color-emoji-random.bin`. The corpus is 30M frames of `ESC[38;5;{fg}m ESC[48;5;{bg}m {emoji-or-ascii}`, 749,801,000 bytes. Compare **total time and FPS** against Ghostty, which measured `41.827 s total` on a busy machine.
>
> Headless on a 64 MiB slice: ghostty-bench 1.6-2.0 s vs FrankenTerm 10.3 s. 52% of FrankenTerm's time is an O(cursor_x) allocating scan in `Performer::recluster_at_cursor`, which runs on every multi-byte grapheme. Bead `ft-yccm0.2.11` fixes it.
>
> Evidence: `evidence/mac-render-perf/2026-10-05/`.

> Status: draft v1 (2026-10-05). It is grounded in a live-hang diagnosis plus headless measurements, listed in §2.
> It has not yet been through external review rounds (see §13).
> Goal: beat Ghostty on Apple Silicon (M4 now, M5/M6 next) on throughput, latency, smoothness, and memory, without giving up FrankenTerm's correctness, durability, or swarm features.

---

## 0. Executive summary

When you ran `time cat color-random.bin` (1.1 GB, `ESC[38;5;Nm` before every one of 100M characters):

- Ghostty finished in 14.2 s, about 78–81 MB/s.
- FrankenTerm drained about **37 KB/s**, roughly **2,000× slower**. It beach-balled and was still running minutes later.

Lack of GPU power is **not** the cause. Three compounding defects are, in order of impact:

1. **Durable, encrypted scrollback writes run synchronously on the PTY parse thread.** In the installed build (0.15.2 / `319a56d`) every evicted row cost about 30 F_FULLFSYNCs. That work ran *under the terminal mutex*, so the UI thread blocked behind the fsyncs as well.
2. **The UI thread takes blocking terminal locks in AppKit event handlers.** Mouse-down calls `is_mouse_grabbed()` → `terminal.lock()`, and paint takes cursor/dimensions/palette locks. As long as the parser holds that lock, the main thread blocks, which shows up as the beach ball.
3. **The memory and architecture are heavy compared with Ghostty.** Specifically:
   - a per-cell attribute model: a 24 B `Cell` holding a 16 B `CellAttributes`;
   - copy-on-write `Arc` line storage, plus an `Arc::make_mut` on every character;
   - a two-stage parse → `Vec<Action>` → apply pipeline;
   - a per-character config mutex lookup;
   - on the main thread: OpenGL by default, or wgpu with per-frame full geometry rebuild and re-upload;
   - pacing by a timer instead of a display link;
   - an atlas leak that likely pins about 3 GB of GPU memory.

The plan has three tracks:
- **Track A (days):** stop the bleeding. Durability moves fully off the ingest path, and the UI thread never blocks on terminal state.
- **Track B (weeks):** a Ghostty-class ingest engine and page-based grid built to beat Ghostty, not merely match it.
- **Track C (weeks):** a native Metal renderer on its own thread, driven by `CAMetalDisplayLink`, with instanced cells, persistent GPU buffers, dirty-row uploads, and a GPU-side scroll ring.

Every lever ships behind a kill-switch with an A/B against a pinned Ghostty in the same window.

---

## 1. Definition of "faster than Ghostty" (the scoreboard)

All rows are measured on the same host, interleaved with a pinned Ghostty (an ABBA hyperfine run with a CV of 5% or less), and the evidence is SHA-256-manifested. A self-speedup counts as maintenance, not a win.

| ID | Metric | Workload | Ghostty bar (to be measured; known points shown) | FrankenTerm target |
|---|---|---|---|---|
| T1 | PTY drain throughput, GUI end-to-end | `cat` of the color-random file (SGR on every char) | 1.1 GB in 14.2 s, about 80 MB/s (user run) | ≥ 1.25× Ghostty |
| T2 | PTY drain throughput | plain short lines (`seq`) | TBD | ≥ 1.25× |
| T3 | PTY drain throughput | long ASCII lines (`cat` of a source corpus) | TBD | ≥ 1.25× |
| T4 | PTY drain throughput | Unicode/CJK/emoji heavy | TBD | ≥ 1.0× |
| T5 | PTY drain throughput | TUI full-screen repaint (cursor addressing, as in Claude Code or htop) | TBD | ≥ 1.25× |
| L1 | Keypress-to-photon p50 / p99 | idle shell, 120 Hz ProMotion | TBD | p50 ≤ Ghostty − 2 ms, p99 ≤ 16 ms |
| L2 | Keypress-to-photon while another pane floods | T1 running in a sibling pane | TBD | p99 ≤ 25 ms; the UI thread never blocks more than 4 ms |
| S1 | Frame pacing during a flood | T1 | — | p99 frame interval ≤ 1.25 × the refresh interval; 0 beach balls |
| S2 | Scroll smoothness | trackpad scroll through 100k lines of scrollback | — | 120 fps sustained, 0 dropped frames |
| M1 | Bytes per cell, resident | colored text | Ghostty: 8 B cell + style id | ≤ 8 B |
| M2 | GUI footprint after T1 | single window | — | ≤ 1 GB RSS, bounded GPU memory, no growth over 24 h |
| E1 | Idle GPU and CPU power | idle 6K window | — | ≤ Ghostty, measured with `powermetrics` |

The headless rows (H1–H3, §2.3) gate Track B before the GUI rows can be measured.

---

## 2. Evidence: the diagnosis

Raw artifacts are in the session scratchpad under `perf/`. Copy them into `evidence/mac-render-perf/2026-10-05/` when this lands.

### 2.1 Live sample of the frozen GUI (pid 47759, v0.15.2 `319a56d`, front_end=WebGpu)

**Backpressure on `cat`:**
- The TTY write offset advanced in exact **524,288-byte** steps, with 20 s or more between steps.
- `cat` sat at 0% CPU, blocked in `write()`, and the GUI was at about 7% CPU in state `U`.
- The 512 KiB step comes from the user config: `mux_output_parser_buffer_size = 512*1024` at `~/.config/frankenterm/frankenterm.lua:502`.

**`mux-parse-pane-2`:**
- About 100% of samples were inside about 35 distinct `fcntl` call sites, roughly 150–200 ms each (consistent with F_FULLFSYNC).
- The rest was `rename`, `openat`, `opendir`/`getdirentries` (the keyring directory rescan), `fstatat`, and `read`.
- One durable cycle took about 6 s and admitted one 512 KiB chunk.

**Main thread:**
- 100% of samples were in `_pthread_cond_wait`, under `-[NSWindow _handleMouseDownEvent:]` → FrankenTerm code.
- That is consistent with a contended parking_lot terminal mutex. In `319a56d`, `store_scrollback_line` ran per row under the terminal lock.

**Memory:**
- Physical footprint 6.2 GB (peak 8.4 GB).
- About 4.2 GB of jemalloc heap ("Memory Tag 254", 31,947 mappings).
- About 3.0 GB of "owned unmapped" GPU memory and about 1.2 GB of IOSurface/IOAccelerator.
- The host had 20.1 of 21.5 GB of swap in use, so every fault paid a page-in. That amplified the stalls.

### 2.2 What HEAD (`fc3fd2121`) already fixed, and what remains

Already fixed, from the git log and the ft-y0gy9 bead:
- A deferred sink queue moves the flush outside the terminal mutex (5e9d50073).
- F_BARRIERFSYNC instead of F_FULLFSYNC (388399963).
- A keyring stat fast path and a pooled nonce source (c73ff22ff, 2f603651d).
- Batches of up to 4096 rows per transaction.

Measured effect: the store's slowdown fell from 16× to about 2.7–4.9×.

Still broken at HEAD:
- The flush still runs on the **parse thread**, with about 8 barrier syncs and 2 renames per batch. It holds the pane's `output_application` mutex and a process-wide keyring mutex.
- Rows are written twice (WAL plus log). Write amplification is 21–72×.
- About 3 keyring rescans and about 8 manifest re-reads happen per batch.
- Even with the store disabled, recorded GUI drain is only **2.8–5.2 MB/s** (ft-y0gy9 runs), about 15–30× below Ghostty.
- GUI ingest MB/s has never been recorded against Ghostty.

### 2.3 Headless measurements on HEAD

Setup: an out-of-tree `ftbench` driver calling `frankenterm-term` directly, at 80×120 and 3,500 lines of scrollback. These are smoke numbers taken on a loaded host (load average about 60); final numbers must be re-taken.

| Lane | color-random 64 MiB | `seq` 64 MiB |
|---|---|---|
| H1 parse only (vtparse + escape-parser → `Vec<Action>`) | 138 MiB/s | — |
| H2 `Terminal::advance_bytes` | 73 MiB/s | **10.6 MiB/s** |
| H3 mux two-stage (parse → Vec → `perform_actions`) | 70 MiB/s | — |
| H3 + production-like config (mutex + `Arc` clone per config read) | **50 MiB/s** | — |

What the numbers show:
- The terminal model alone runs the user's workload at about Ghostty speed. The 2,000× collapse comes from the durable store plus the lock stalls.
- **Plain short lines are about 7× slower than colored text.** That works out to about 0.68 µs per scrolled row.

Where the `seq` time goes (from the sample):
- `Line::set_cell_impl` → `ClusteredLine::append_grapheme` and `guarded_reserve_text` (regrowth plus zeroing);
- `Arc<ClusteredLine>::make_mut` on every character;
- `CellAttributes::eq` on every character;
- `Arc<ClusteredLine>::drop_slow` on every scroll (about 14%).

### 2.4 Renderer findings (code audit)

- **Backend and threading:**
  - The default macOS `front_end` is **OpenGL/CGL** (`frontend.rs:5-11`), with vsync explicitly off (`window.rs:378-384`).
  - WebGpu is opt-in (Fifo present mode, frame latency 2).
  - No `CVDisplayLink`/`CAMetalDisplayLink` is used anywhere. `max_fps` is a timer: 60 by default, 30 in the user config.
  - Paint runs on the **main thread**. `nextDrawable` is acquired before geometry is built, and wgpu disables the drawable timeout.
- **Geometry and upload cost:**
  - Each cell gets a background quad, a glyph quad, and possibly an underline quad. Each quad is 4 vertices × 68 B, which is about 600 B per cell.
  - The entire vertex stream is rebuilt and re-uploaded every frame, with no dirty-range upload.
  - Every visible `Line` is deep-cloned under the lock each frame.
  - The line-quad cache key includes `top_pixel_y`, so a scroll misses the cache for every row.
- **Atlas:**
  - There is one RGBA8 atlas for both grayscale and color glyphs, starting at 128 px.
  - Rebuilding it creates a new texture. `shape_cache` keeps glyph `Arc`s that pin **old atlases** (up to 256 MiB each), which is the likely source of the 3 GB of GPU memory.
  - Atlas `try_alloc` accounted for 119 of 128 glyph samples (ft-8r43i.10).
- **Native Metal:**
  - None exists. `MetalDirect` is a log-only policy probe, and bead ft-mpc9b.3.1 was retired without a renderer.
  - Several "Ghostty-pattern substrates" (triple buffer, `per_row_quad_cache`, `DifferentialCellStream`, `adaptive_fps`) have **no production consumer**.

### 2.5 Ghostty's design (from `~/projects/ghostty`, HEAD `e500d414f`)

- **Ingest threads:** a dedicated `io-gather` thread and an `io-reader` (parse) thread share a ring of 4×64 KiB buffers. The gather thread waits up to 3 ms (16 immediate reads, then a 1 ms poll) once 1 KiB has arrived. Both threads run at QoS `user_initiated` (+15% measured).
- **Parser:**
  - SIMD UTF-8 decode and ESC search (Highway + simdutf);
  - `print_slice` for runs;
  - CSI fast paths with fixed parameter arrays;
  - SGR → style set with an early return when the style is unchanged.
- **Grid:**
  - `packed struct(u64)` cells with a 16-bit style id;
  - pages of about 512 KiB with a reference-counted style set per page;
  - O(1) scroll via `PageList.grow()`, recycling the first page at the scrollback limit (50 MB by default).
- **Locking:**
  - The parse thread holds the terminal mutex per batch and wakes the renderer once per batch.
  - `lockDemand`/`yieldToDemand` hand the lock to a waiting renderer within 1 ms.
  - The render snapshot copies only dirty rows' raw u64 cells under the lock and expands styles outside it.
- **Metal renderer:**
  - IOSurface layers, triple buffering, shared write-combined buffers;
  - 3 draw calls: a background fill, a full-screen per-cell background pass at 4 B per cell, and **one instanced glyph draw** at 32 B per glyph;
  - separate R8 grayscale and BGRA color atlases;
  - CVDisplayLink-driven draws at most once per vsync.
- **Ghostty weaknesses to exploit:**
  1. Single-threaded parse is its throughput ceiling: `cat` at 35% CPU means the parser can't keep up.
  2. A style-set hash and probe plus two refcount updates per SGR. Its 128-style page capacity is exceeded by this exact workload, which forces page clones.
  3. `updateFrame`/`rebuildCells` is uncapped on every wakeup during a flood.
  4. **Any viewport change forces a full rebuild and re-copy of every row.** There is no GPU scroll ring.
  5. The full screen is re-uploaded every drawn frame.

---

## 3. Design principles (non-negotiable)

1. **The ingest path never touches disk or sleeps on I/O.** Durability is asynchronous, ordered, and group-committed. It is bounded in memory and degrades (gap marker plus counter) rather than block the PTY.
2. **The UI (AppKit) thread never takes a blocking lock on terminal state.** It reads published snapshots and atomics only. Input goes to the PTY writer without a terminal lock.
3. **One owner per stage, with lock-free handoff.** The stages are PTY gather → parse/apply → render snapshot → GPU encode → present. Only the parse/apply thread mutates terminal state. The renderer copies dirty rows under a lock held for microseconds, with demand-yield fairness.
4. **Do work proportional to change, not to screen size:** dirty rows only, scroll as a ring offset, uploads only for new data.
5. **Pace to the display.** Draw at most once per refresh on a `CAMetalDisplayLink` at 120 Hz on ProMotion, latch input late, and pause the link when idle.
6. **Correctness first.** Every lever is either byte-identical to the old engine on a differential fuzz corpus or documented in `DISCREPANCIES`. Rejected levers go into `docs/perf-ledger/interactive-systems-negative-results.md` with a do-not-retry predicate.
7. **Every claim is backed by a measured, pinned-incumbent A/B.** Never cite "faster than Ghostty" without §1 evidence.

---

## 4. Track A — Stop the bleeding (target: 1–2 weeks; ships independently)

### A1. Move the durable scrollback store off the parse thread (P0, extends ft-y0gy9 and ft-lucdu)

- **Writer thread.** Add a per-process `scrollback-durability` writer thread, one for all panes, at QoS `utility`. It takes a bounded MPSC queue of *sealed row batches*; the parse thread only enqueues `Arc<[Row]>` and never waits.
- **Group commit.** Commit when 250 ms have passed, 8 MiB has accumulated, or on pane close, whichever comes first. Each commit is one barrier sync on the log, plus one manifest publish every N commits or 1 s (not per batch).
- **Remove WAL payload duplication.** The WAL records log offsets and digests, not row payloads. Target write amplification is ≤3× for 91-byte lines.
- **Encryption cost.** Seal a *segment* (64–256 KiB) with one XChaCha20-Poly1305 operation instead of one per row. Encode records into an arena with no per-row allocation.
- **Keyring.** Resolve the cipher once per writer epoch and cache it with the stat fingerprint. Remove the per-batch keyring rescans and the shared-keyring-mutex convoy, so manifest publishes no longer serialize across panes.
- **Overload policy.** When the queue exceeds its byte budget (default 64 MiB per process), the writer coalesces, and beyond that it records a **durability gap** (rows count, seqno range). The parse thread never blocks. Gaps surface in `ft doctor` and `ft session list-durable`.
  - This is an owner-level durability trade, and the plan proposes it explicitly: ordering is preserved, and a power loss may drop the newest ≤250 ms.
  - It matches the SQLite-on-macOS trade already accepted in ft-y0gy9.
- **Kill switch:** `FT_SCROLLBACK_DURABLE_ASYNC=0` restores the current synchronous path for A/B.
- **Gates:**
  - With the store on, H3-style GUI drain is within 5% of store-off.
  - The `seq 1 2000000` A/B is recorded.
  - Crash/recovery tests: kill -9 during commit, torn tail, reordered-write injection.
  - Linux keeps the real fsync semantics.

### A2. The UI thread never blocks on terminal locks (P0, extends 4tenz.6.3, ft-8aoi6, 4tenz.7.17)

- **Published render facts.** Publish a per-pane `PaneRenderFacts` (atomics or arc-swap), updated by the parse thread after each batch. Fields:
  - `mouse_grabbed`, `alt_screen`, cursor position/shape/visibility, dimensions, palette generation, title, seqno.
- **Read them instead of locking.** Replace every blocking `terminal.lock()` on the main thread with reads of the published facts:
  - `is_mouse_grabbed`, `is_alt_screen_active`, `get_cursor_position`, `get_dimensions`, `palette()`;
  - paint's `update_text_cursor`;
  - mouse-up selection text, which becomes asynchronous: request, then copy on completion.
- **Writer flush.** `ThreadedWriter::flush` becomes fire-and-forget; it no longer blocks on `ack_receiver.recv()`. Replies to DA/DSR/DECRQSS are queued and never flushed while the terminal lock is held.
- **Mouse reports.** Generate them from the published facts and send them through the writer queue with no terminal lock.
- **Debug guard.** Add a debug assertion, `main_thread_terminal_lock_guard`, that panics in debug builds if the main thread acquires the terminal mutex. It keeps the property from regressing.
- **Gate:** while T1 floods a sibling pane, a main-thread sample shows **0** samples in `pthread_cond_wait` under event handlers, the UI responds within 4 ms, and there are 0 beach balls over a 10-minute run.

### A3. Parser lock fairness (extends ft-8r43i.1.3)

- **Demand-yield.** Port Ghostty's demand-yield. The renderer or snapshotter increments `demand` before locking. The parse thread checks `demand` at **action-batch boundaries**, never mid-grapheme, and parks for up to 1 ms until the handoff generation changes.
- **Bound the lock hold.** Apply actions in slices of at most about 2 ms. Batch boundaries must respect grapheme and seqno ordering; the earlier rejection was for a line-boundary split.

### A4. Cheap correctness-preserving fixes

- **Config reads.** Cache the config `Arc` once per batch in `Performer`/`TerminalState`. This removes the per-character mutex + `Arc` clone (`performer.rs:666`, `max_accumulating_title_len`) and the roughly 10 config lookups per scroll. Measured cost: −29% on H3 (70 → 50 MiB/s without the fix).
- **Atlas leak.** On atlas rebuild, drop the glyph/sprite `Arc`s cached in `shape_cache`, keeping only the shaping data, so old atlas textures are freed. Add a live `WebGpuTexture` counter to `ft doctor`. Gate: GPU memory stays flat across 1,000 forced atlas rebuilds.
- **Atlas reuse.** Clear in place (`Atlas::clear`) instead of creating a new texture at the same size. Start at 1024², not 128², to avoid the repeated rebuild-and-retry cycles.
- **Sanity checks.** `ft doctor` warns when `mux_output_parser_buffer_size` is above 128 KiB, and when scrollback/durability settings would put disk I/O on hot paths.
- **jemalloc tuning.** Set `background_thread:true`, `dirty_decay_ms:1000`, and `muzzy_decay_ms:0` for the GUI. Measure fragmentation, given the 31,947 mappings.
- **Delivery.** Ship a current build to the user's machine. The installed app is 1,654 commits behind and lacks every fix in §2.2.

**Track A exit:** T1 GUI throughput ≥ 50% of Ghostty, zero beach balls, and the store-on/store-off gap ≤ 5%.

---

## 5. Track B — An ingest engine built to beat Ghostty (target: 4–8 weeks)

### B1. Gather/parse thread split with a zero-copy ring (replaces the socketpair)

- **Today:** the reader `read()`s, `write()`s to an AF_UNIX socketpair (1 MiB buffers), and the parser `read()`s back. That is two extra syscalls and two kernel copies per chunk, plus a `delivery_gate` and 4–5 `state` mutex acquisitions per write (`mux/src/lib.rs:3943-4097`).
- **New:** a preallocated SPSC ring of 8 × 64 KiB slots, with only the slot indices atomic. The gather thread runs Ghostty's policy: batch at ≥1 KiB, up to 16 immediate reads, then a 1 ms poll, with a 3 ms cap. A self-pipe wake keeps an idle parser from waiting out the poll.
- **Backpressure:** the gather thread blocks when the ring is full, and the kernel PTY queue then throttles the child. That is correct, and no disk I/O ever sits upstream of it.
- **Exact delivery accounting** (checkpoint fences, model checkpoints) becomes slot sequence numbers instead of per-write mutex reservations.
- **QoS:** gather and parse run at `QOS_CLASS_USER_INITIATED`, renderer at `USER_INTERACTIVE` while focused. Keep them on P-cores during floods; this is an FFI island in the window/GUI crate with an audited `unsafe` contract.

### B2. Single-stage, allocation-free parse and apply

- **Fuse the stages.** Remove the `Vec<Action>` materialization, which expands memory about 6× (32 B per Action). The parser calls a monomorphized `Handler` trait directly; no `&mut dyn VTActor` callback per byte.
- **Ground-state fast path.** Scan for the next byte that is ≥0x80, ESC, or a C0 control using **`std::simd`**. Portable SIMD is safe Rust, and the toolchain is already nightly, so no `unsafe` is needed. On aarch64 this compiles to NEON 16-byte compares. Printable ASCII runs go straight to `print_ascii_run(&[u8])`.
- **UTF-8.** Validate and decode runs with a SIMD validator (safe `std::simd` port of the simdutf approach, behind a kill-switch with a scalar oracle and a byte-identical differential test).
- **CSI fast path.** Accumulate parameters into fixed arrays in local variables (Ghostty `stream.zig:975-1029`).
- **SGR fast path for `38;5;N`, `48;5;N`, `38;2;r;g;b`, and `0`:** one branch-predicted match with no iterator. Keep a **one-entry last-SGR → style-id cache**, which Ghostty lacks; this is a direct win on the user's workload.
- **Mux preflight.** Make the admission preflight passes (`localpane.rs:3985-4100`) incremental flags computed during the parse, instead of 2–3 extra passes over a Vec.

### B3. A page-based grid that replaces per-cell attributes (the big one)

- **Cell.** An 8 B packed cell:
  - 21-bit codepoint or grapheme-arena index;
  - 16-bit style id;
  - wide/spacer bits, hyperlink bit, protected bit, dirty-hint bit.

  It replaces the 24 B `Cell` with an embedded 16 B `CellAttributes`.
- **Page.** About 512 KiB contiguous: a row headers array (u64 flags including dirty, wrapped, styled, grapheme, and semantic-zone bits), a cell array, a grapheme arena, and a **per-page style table** with refcounts.
  - Style capacity starts at **512**, not Ghostty's 128, so the 256-color workload never triggers page clones.
  - Lookup goes through the one-entry SGR cache, then a SwissTable-style probe.
- **Scrolling.** The PageList keeps a pool, and scrolling is O(1). At the scrollback limit the oldest page is popped, zeroed (or handed to the durability writer as a sealed immutable page), and reused as the new tail. There is no per-row allocation or free; today's `Arc<ClusteredLine>::drop_slow` costs about 14%.
- **Durability by page.** Durable persistence (A1) consumes **sealed pages** rather than per-row clones. A full page is immutable, so the writer can encode and seal it off-thread with no copy. That removes `Arc::new(line.clone())` per evicted row.
- **Hot/warm/cold tiering maps onto pages:**
  - hot: the last N pages, raw;
  - warm: zstd-compressed pages, compressed on an idle background thread with `try_lock` only;
  - cold: durable pages on disk.
- **Compatibility layer.** Keep `Line`/`Cell` as a *materialized view* for non-hot consumers: selection, search, mux codec, `ft` capture, Lua. They get `page.row(i).to_line()`. Hot paths (print, scroll, erase, render snapshot) operate on packed cells.
- **Migration order:**
  1. PageGrid behind the `Screen` API, with a kill-switch `FT_GRID_ENGINE=legacy|page`.
  2. Differential fuzzing: the same byte streams into both engines, diffing materialized lines, cursor, and modes after every chunk.
  3. Conformance: vttest, the esctest suite, all existing `frankenterm-term` tests, and the reflow/resize corpus (ft-8r43i).
  4. Flip the default.
  5. Delete legacy hot paths. That deletion needs explicit owner approval per AGENTS.md Rule 1.
- **Reflow.** Reflow on resize operates page-wise. The rejected persistent-rope approach (IS-N008) stays dead.

### B4. Optional two-core pipeline, to beat Ghostty's single-thread ceiling

- **Shape.** Stage 1 (SIMD scan, UTF-8 decode, and CSI tokenizing into a compact 4–8 B token stream) runs on one P-core. Stage 2 (apply to grid) runs on another, connected by an SPSC token ring.
- **Why it can win.** Ghostty's `cat` sits at 35% CPU because the parse+apply thread is saturated. Splitting those stages can roughly double the ceiling on SGR-heavy streams.
- **Gate.** The pipeline must beat the fused single-stage engine (B2) by ≥20% on T1/T3 and must not regress L1. It defaults on only if it wins on both M4 P-cores and E-core-constrained runs. If the handoff cost eats the gain, ledger it as negative evidence.

**Track B gates (headless, H-lanes):**

| Lane | Target |
|---|---|
| H2 color-random | ≥ 2× Ghostty `TerminalStream` bench on the same file |
| H2 `seq` | ≥ 1.5× Ghostty |
| Allocations in steady-state printing and scrolling | 0 (asserted with a counting allocator) |

---

## 6. Track C — A native Metal renderer that beats Ghostty's (target: 6–10 weeks, in parallel with B after A)

### C1. Architecture

- **Crate.** Add a new `frankenterm-renderer-metal` crate as the macOS default `front_end = "Metal"`. WebGpu/OpenGL stay for other platforms and as fallbacks.
- **FFI.** Use `objc2-metal`, `objc2-quartz-core`, and `objc2-foundation`. Unsafe is confined to this audited native crate, with a contract list per AGENTS.md.
- **Threads:**
  - **Render thread** (QoS `user_interactive`): owns the `MTLDevice` and `MTLCommandQueue` (MTL4CommandQueue/allocators on macOS 26+ with a Metal 3 fallback), the atlases, and the GPU buffers.
  - **Main thread:** only AppKit events, window state, and IME. It never paints.
- **Presentation:** `CAMetalLayer` with `displaySyncEnabled`, set opaque when the window is opaque (today it is non-opaque, so the window server blends a full 6K layer), `maximumDrawableCount = 3`, and `framebufferOnly = true`.
- **Pacing:** `CAMetalDisplayLink` (macOS 14+), with a CVDisplayLink fallback. It honors ProMotion 120 Hz and adaptive rate, and is paused when idle.
  - **Late-latch:** at the display-link callback, take the render snapshot as late as possible within a budget (target deadline minus measured encode time, EWMA). This minimizes keypress-to-photon.
  - **Input wake:** a keypress or PTY output marks "dirty". If the link is paused, render immediately, then resume it (a Ghostty-style `draw_now`).

### C2. GPU data model (persistent, dirty-row updated, scroll ring)

- **Buffers.** Shared storage mode on unified memory, write-combined CPU cache, triple-buffered per frame slot with a `dispatch_semaphore`.
- **`CellBg`:** 4 B per cell (RGBA8), laid out as a **row ring**. A uniform `row_offset` rotates rows in the shader, so **a scroll frame uploads only the newly exposed rows**. Ghostty rebuilds and re-uploads every row on viewport change; this is the main smoothness and power lever, beating Ghostty on S2 and E1.
- **`CellText` instances:** 16–24 B each (grid col/row u16×2, glyph atlas rect index u32, fg RGBA8, flags/atlas-select u8s). They are packed in per-row ranges in the same row ring, with a per-row instance count and offset table.
- **Draw calls:**
  1. clear;
  2. a full-screen triangle that reads `CellBg` (per-cell backgrounds, selection, cursor block);
  3. **one instanced glyph draw** (`drawPrimitives` with `instanceCount = total glyphs`, with the shader skipping empty slots);
  4. decorations: underline/strikethrough/undercurl computed in the fragment shader from cell flags, with no extra quads.
- **Uploads.** Only dirty rows, via the `PageList` dirty bitsets scanned 64 rows at a time, written as `memcpy` into the slot's buffer region. A dirty-row bitmap per frame slot handles triple-buffer catch-up.
- **TBDR.** Single render pass, `loadAction = clear`, `storeAction = store` only on the drawable, no blending in the background pass, and memoryless intermediates if any are ever added.

### C3. Glyphs and text

- **Rasterize with CoreText** on macOS, replacing FreeType for quality and macOS-native smoothing parity. Keep FreeType as a kill-switch fallback.
- **Two atlases:** R8 grayscale and BGRA8 color (emoji), each a texture array or large texture sized up front (2048² initially) with a skyline packer.
  - This replaces the maximal-rectangles packer that rescans on every glyph (ft-8r43i.10, 119 of 128 samples).
  - Incremental `replaceRegion` uploads go only to the frame slot's copy. When an atlas is full, evict LRU pages; never recreate and leak.
- **ASCII bypass.** For each (font, style, size), precompute glyph ids and advances for 0x20–0x7E. A pure-ASCII run with no ligature-relevant features **skips HarfBuzz entirely**. Ghostty still shapes those runs.
- **Run splitting.** Shaping runs split on font, style, and ligature attributes, but **not on fg color**. Today a color change starts a new run, and Ghostty does the same. Color is applied per instance after shaping, so the user's workload shapes as long runs.
- **Shape cache.** Keyed by (font face, features, text) with no position, LFU-bounded, and storing glyph ids, not atlas handles. That fixes the atlas-pinning class from §2.4.

### C4. Render snapshot (no long locks, no line clones)

- **Under the terminal lock** (demand-yield, B/A3): copy dirty rows' raw 8 B cells and the referenced styles into a render-side mirror. That is about 512 cols × 117 rows × 8 B = 480 KB worst case, typically a few KB. The legacy deep `Line` clone per row per frame goes away.
- **Outside the lock:** resolve styles to colors, hyperlinks, selection, and cursor, then build the `CellText` instances for dirty rows only.
- **Rate cap.** `rebuild` runs at most once per display-link tick, which avoids Ghostty's uncapped rebuild-per-wakeup during floods.

### C5. Multi-pane and swarm scale

- **One encoder per window.** Panes become viewports (scissor plus per-pane uniform offsets) into one frame. Idle panes cost 0 uploads.
- **Large layouts.** With 200 panes, per-pane dirty tracking keeps frame cost proportional to active panes. This is FrankenTerm's native workload and Ghostty's weakness.

**Track C gates:** L1, L2, S1, S2, M2, and E1 versus pinned Ghostty, plus an SSIM ≥ 0.995 parity corpus against the current renderer (`renderer_slo/ssim_parity.rs`) and Ghostty screenshots.

---

## 7. Apple Silicon specifics (M4 now, M5/M6 next)

- **Unified memory.** Avoid staging copies entirely. CPU writes land in shared buffers the GPU reads directly. Today's wgpu path does `write_buffer` plus a staging buffer plus 3-way rotation, which is three or more copies.
- **P/E cores.** Keep the ingest threads at `user_initiated` (Ghostty measured +15% on M4 Max) and the render thread at `user_interactive`. Verify placement with `powermetrics --samplers cpu_power` during T1. Don't hard-code core counts; the worker-count tuning in IS-N005/N056 is graveyarded.
- **Metal 4 (macOS 26):**
  - `MTL4CommandAllocator` reuse and residency sets (`MTLResidencySet`) cut per-frame residency overhead.
  - Argument tables replace per-frame bind-group creation (today wgpu creates 2 bind groups per frame).
  - Fall back to Metal 3 on older macOS.
- **ProMotion and adaptive refresh.** Use `CAMetalDisplayLink.preferredFrameRateRange` to run 120 Hz during interaction and scrolling, drop to 60 or lower when only text output changes slowly, and to 0 (paused) when idle.
- **6K/XDR displays.** Opaque layers, `framebufferOnly`, and no full-screen blending. At 6144×3456 each drawable is about 85 MB, so keep drawable count at 3 and no offscreen targets.
- **M5/M6.** The architecture scales with GPU core count automatically (one instanced draw). The neural accelerators in M5 GPU cores are not relevant here, and the plan deliberately does not chase them. Re-run the full §1 matrix on each chip; no cross-generation extrapolation (README negative-evidence ledger).

---

## 8. Measurement and verification substrate (build first, in parallel with A)

1. **`frankenterm-term` bench lane `ingest_throughput`.** Bring the scratch `ftbench` in-tree as a proper Criterion bench plus CLI, with corpora generated deterministically from seeds: color-random, seq, long-lines, unicode, TUI-repaint. Lanes: parse, term, mux-two-stage, and production-config.
2. **Pinned incumbent.** `ghostty-bench +terminal-stream --data <same file> --terminal-rows 80 --terminal-cols 120`, built from a pinned Ghostty SHA with a pinned zig (0.16.0 is required at `e500d414f`). Record the incumbent contract per the mega-kernel skill's `INCUMBENT-CONTRACT-TEMPLATE`.
3. **GUI end-to-end harness** `scripts/mac-gui-throughput.sh`:
   - Launch a dev GUI with an **isolated HOME/XDG** and no socket env (memory: e2e-harness-live-mux-hazard), using `--always-new-process`.
   - Spawn `cat <corpus>` in a pane and measure drain time from the TTY offset (`lsof -o`), plus frame-interval telemetry.
   - Run Ghostty.app (pinned version) in the same window: `open -na Ghostty --args -e …`.
   - Run ABBA interleaved via hyperfine, refusing results with CV > 5%.
4. **Frame and latency telemetry.**
   - `os_signpost` intervals: gather, parse batch, lock hold, snapshot, rebuild, encode, commit, present. Inspect with `xctrace` (Time Profiler plus Metal System Trace).
   - Keypress-to-photon: a photodiode or high-speed-camera rig (bead 4tenz.3.4), with the in-process K0–K13 stage contract as the software proxy.
5. **Lock-hold histograms** (hdrhistogram) for the terminal mutex, exported via `ft doctor --json`, and a main-thread-blocked-time counter.
6. **Memory gates.** `vmmap --summary` and `footprint` after T1 and after a 24 h soak. Live counters for GPU textures and buffers.
7. **Correctness:** a differential fuzzer (legacy vs page grid), vttest/esctest, the SSIM corpus, the reflow corpus, and durability crash tests.

The AGENTS.md proof rules still apply. GUI crate tests are native-macOS only (`--bin frankenterm-gui`, local). Everything else is proven remotely on RCH. Benchmarks used for claims run on a quiet host, and the CV gate refuses noisy rows.

---

## 9. Phased schedule and dependency graph

```
A0 measurement substrate (§8.1-8.3) ─┬─> A1 async durability ──┐
                                      ├─> A2 UI never blocks ───┤
                                      ├─> A3 demand-yield ──────┼─> TRACK A EXIT (ship build to user)
                                      └─> A4 cheap fixes ───────┘
TRACK A EXIT ─┬─> B1 gather ring ─> B2 single-stage SIMD parse ─┬─> B3 page grid ─> B4 2-core pipeline (optional)
              │                                                  │        │
              └─> C1 Metal skeleton + display link ─> C2 GPU data model ─┴─> C4 snapshot from page grid ─> C5 multi-pane
                                     └─> C3 CoreText + dual atlas + ASCII bypass ──┘
B3 + C4 ─> Final §1 matrix vs pinned Ghostty on M4 Pro (then M5/M6 when available)
```

- Rough sizes:
  - A: 1–2 weeks.
  - B1–B2: 2 weeks.
  - B3: 3–5 weeks, the long pole because of migration and conformance.
  - C1–C3: 4 weeks, in parallel with B.
  - C4–C5: 2 weeks after B3.
- B3 and C2 are designed together so that the GPU row-ring mirrors page rows.

---

## 10. Risks and mitigations

| Risk | Mitigation |
|---|---|
| The page-grid migration breaks one of the many `Line` consumers | Materialized-`Line` compatibility view, a differential fuzzer, a kill-switch, and a staged flip |
| Async durability weakens crash guarantees | Explicit owner decision (§4 A1), ordered recovery, gap records, crash-injection tests, Linux unchanged |
| The native Metal crate adds an unsafe surface | Confined to one audited crate. Unsafe/UB exorcist skills run before release. WebGpu stays as a fallback |
| CAMetalDisplayLink is unavailable on old macOS | CVDisplayLink fallback |
| SIMD scalar/vector divergence | Scalar oracle, a byte-identical differential test, and a kill-switch |
| Benchmarks on a shared, swapping host (load average 60, 20 GB swap observed) | Quiet-window protocol, interleaved ABBA, CV gate, and a recorded fingerprint |
| Ghostty moves | Incumbent pinned by SHA and version, then re-baselined each release |

---

## 11. Graveyard: do not re-propose (from the repo's negative-evidence ledger)

| Rejected lever | Retry only if |
|---|---|
| SoA instanced glyph quads on wgpu (IS-N007) | A profile shows ≥0.5% in vertex create/bind/upload. Note that C2 is a different design: persistent buffers plus a row ring, not a per-frame instanced rebuild |
| COW snapshots instead of short locks (IS-N009) | Terminal-lock p95 ≥ 50 µs |
| Persistent-rope reflow (IS-N008) | Never as a standalone patch |
| Per-op micro-tuning: MPHF, Teddy, ANSI DFA, table dispatch D2 | A profiler attributes the specific operation above noise |
| SDF atlas | Screenshot/SSIM proof below 14 px |
| GPU compute shaping | A ≥2× demonstration with a HarfBuzz-exact corpus |
| `FT_PROFILE_WEBGPU_FRAME_LATENCY=1` | A same-binary A/B shows a causal gain. Immediate/tearing present modes are forbidden |
| Splitting parser lock yields at line boundaries (ft-8r43i.1.3) | A3 must yield at batch boundaries with a continuation protocol |
| Counting dormant substrates (triple buffer, `per_row_quad_cache`, `adaptive_fps`, `DifferentialCellStream`) as progress | They need a production call-graph proof plus a native A/B |

---

## 12. Immediate next actions (first 72 hours)

1. Finish the HEAD reproduction. A symbolized `release-perf` GUI build is in progress at `/tmp/ft-macperf-gui-native`.
   - Run T1 with an isolated HOME, store on vs off, and capture `xctrace` Time Profiler output.
   - Confirm whether A2's main-thread blocking persists at HEAD.
2. Run `ghostty-bench` (building with zig 0.16.0) against `ftbench` on the 64 MiB corpora on a quiet host, and record H1–H3 against Ghostty.
3. File beads for A1–A4, B1–B4, C1–C5, and §8, with dependency edges matching §9, under a new epic "mac-render-super-fast". Link them to ft-y0gy9, ft-lucdu, 4tenz.6.3, 4tenz.8.5/8.6, ft-8r43i.10, and ft-8aoi6.
4. Ship the user a current build. The installed 0.15.2 has the worst-case synchronous per-row F_FULLFSYNC path. As an interim workaround, set `config.scrollback_tiered_enabled = false` and remove the `mux_output_parser_buffer_size = 512*1024` line from `frankenterm.lua`.

---

## 13. Plan-review protocol (planning-workflow)

- This is draft v1. Run four or more review rounds with GPT Pro Extended Reasoning using the skill's exact prompt, integrate each round in place, and stop at steady state.
- Optionally blend other models' reviews (Gemini Deep Think, Opus), with GPT Pro as arbiter.
- Then convert to beads (§12.3) and run polish rounds on the beads until steady state.
