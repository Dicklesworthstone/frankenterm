# PageGrid ADR: packed page grid for the terminal model (ft-yccm0.3.3.1)

| | |
|---|---|
| Status | Proposed, revision 2: the review in section 11 is resolved. This is the gate for B3.2-B3.9 (ft-yccm0.3.3.2 .. ft-yccm0.3.3.9) and an input to C2/C4b. |
| Epic | ft-yccm0 (mac-render), Track B, B3 = ft-yccm0.3.3 |
| Review | A subagent review is recorded in section 11. An independent review by another lane or model is still requested before B3.2 starts; record it on ft-yccm0.3.3.1. |
| Sources | FrankenTerm at HEAD 79056d6ee plus the 2026-10-06 working tree (`terminalstate/mod.rs` carried another lane's uncommitted hunk). Ghostty at e500d414f (`~/projects/ghostty`). |

Line numbers drift. Every size or rate in this document is labelled as one of:
- **asserted**: a size test in the code;
- **arithmetic**: computed from type layouts;
- **measured**: a cited run, with its load average.

Nothing here is an extrapolated measurement.

## 0. Decisions at a glance

| # | Decision | Section |
|---|---|---|
| D1 | New module `frankenterm/term/src/pagegrid/` (`page.rs`, `style.rs`, `grapheme.rs`, `links.rs`, `list.rs`, `view.rs`). | 2.1 |
| D2 | **Cells are 8 bytes and carry an inline style.** Everything that today's `CellAttributes` stores without its boxed `FatAttributes` is encoded in the cell: the attribute bits plus a palette or default foreground and background. T0, T1 and typical TUIs never touch a style table. | 2.2, 3.1, 4 |
| D3 | **Rich styles only go in a per-page table.** Rich means true colour or an underline colour. Ids are `u32`. The table starts at 512 index slots and grows by rehashing its index. It never clones or splits a page. | 2.3, 3.4 |
| D4 | **Page = one `Box<[u64]>`** holding row headers, row seqnos and cells, with a row stride of `cols + 1` so the legacy overhang cell has a home. The standard cell area is 32,768 cells. Side structures grow independently. | 2.4, 3.3 |
| D5 | **The primary screen uses pages throughout; the alternate screen is one dedicated page.** Full-screen scroll appends a row. A scroll region with top margin 0 also appends, then rotates the footer rows down. Other regions rotate row headers. | 2.5 |
| D6 | **Coordinates stay `StableRowIndex` plus column.** There are no pins. Page serials are globally unique per terminal. | 2.6 |
| D7 | **Single writer.** The renderer copies dirty rows under the fair lock. Sealed pages are shared through `Arc` with no lock. | 2.7 |
| D8 | **Sealed page content is immutable.** State that can change after sealing lives outside the page (a list-level dirty floor, plus a sealing delay that keeps the seam row active). The rare writes that remain go through copy-on-write. **Eviction keeps legacy per-row admission:** receipts, refusal and the recovery hold all stay. | 2.8 |
| D9 | **A materialized `Line` view reproduces legacy rows exactly.** This includes the storage-form-dependent behaviour, which section 9 proposes fixing in legacy first. Wire and on-disk *schemas* are unchanged; *decoded* rows are equal. | 3.10, 7, 8 |
| D10 | **Kill switch `FT_GRID_ENGINE=legacy\|page`, default `legacy`.** The flip requires the DualEngine differential (I16), ingest and read-path lanes, and the memory criteria in section 8.3. | 8 |

## 1. Context

### 1.1 FrankenTerm today

**Cells and attributes**
- `Cell` is `{text: TeenyString, attrs: CellAttributes}`. Sizes are **asserted** at `frankenterm/cell/src/lib.rs:1628-1636`: `Cell` 24 B, `CellAttributes` 16 B, `ColorAttribute` 20 B, `TeenyString` 8 B.
- `TeenyString` stores text inline when it is under 8 bytes, and otherwise boxes it on the heap (`cell/src/lib.rs:1064-1203`). Explicit widths are clamped to 1..=2 (`cell/src/lib.rs:1147-1155`), but a computed width can be 0 on the heap path.
- `CellAttributes` is `attributes: u32`, `foreground: SmallColor`, `background: SmallColor`, `fat: Option<Box<FatAttributes>>` (`cell/src/lib.rs:67-79`).
  - `SmallColor` is `Default | PaletteIndex(u8)` (`cell/src/lib.rs:42-45`).
  - The attribute bits are at `cell/src/lib.rs:692-702`:

    | Bits | Attribute |
    |---|---|
    | 0-1 | intensity |
    | 2-4 | underline |
    | 5-6 | blink |
    | 7 | italic |
    | 8 | reverse |
    | 9 | strikethrough |
    | 10 | invisible |
    | 11 | wrapped |
    | 12 | overline |
    | 13-14 | semantic type |
    | 15-16 | vertical align |

    Bits 17-31 are unused.
- `FatAttributes` holds the hyperlink, images, underline colour and the true-colour fg/bg (`cell/src/lib.rs:104-115`). It is about 96 B (**arithmetic**). Once the pen has true colour or a hyperlink, every cell it prints allocates one (`cell/src/lib.rs:807-818`).

**Lines**
- `Line` is `{cells: CellStorage, zones, seqno, bits: LineBits, appdata}` (`surface/src/line/line.rs:166-175`).
- Storage is `V(VecStorage{Arc<CellBuffer>})` or `C(Arc<ClusteredLine>)` (`surface/src/line/storage.rs:10-15`, `vecstorage.rs:22-57`, `clusterline.rs:29-43`).
- In V storage, a cell that follows a width-2 cell is hidden by the visible-cell walk but keeps its own attributes (`line.rs:1796-1801`).
- Wrap is attribute bit 11 on cells (`line.rs:1861-1904`). DCH moves it inward: `erase_cell` removes cell x and pushes a default cell at the end (`line.rs:1609-1622`).
- The line seqno is the dirty signal. A seqno of 0 means "always changed" (`line.rs:755-757, 847-849`).

**Screen, scrolling and eviction**
- `Screen.lines` is a `VecDeque<Line>` (`term/src/screen.rs:2776`). Stable rows are physical rows plus `stable_row_index_offset` (`screen.rs:9228-9256`).
- Scrolling (`screen.rs:9368-9559`):
  - The row leaving the viewport is compressed (`screen.rs:9424-9427`).
  - Rows beyond `physical_rows + hot_scrollback_size()` are evicted (`screen.rs:9416`, `3997-4010`).
  - Each evicted row is spilled to the cold sink with a receipt. A refused spill keeps the row resident (`screen.rs:9451-9466`), and nothing is evicted while recovery rows exist (`screen.rs:9408-9413`).
  - Per-row spill hooks transport selection anchors, align the cold seam and append to the geometry index (`screen.rs:6524-6704`).
- New rows:
  - A default pen gives `Line::new`.
  - Any other pen gives a row filled with `cols` blanks carrying `pen.clone_sgr_only()`. The evicted row is reused through `resize_and_clear` (`screen.rs:9468-9484`, `9524-9533`; `blank_attr` at `terminalstate/mod.rs:1883`).
- `clear_line` of the whole first viewport row clears the wrap bit on the scrollback row above it (`screen.rs:9136-9144`).

**Measured symptoms** (provisional; `evidence/mac-render-perf/2026-10-05/README.md`)
- seq: 10.6 MiB/s on the term lane, at load average 36-100. `drop_slow` is about 14% of samples.
- color-emoji-random: 0.862 s mean per 64 MiB (10 runs, load average 11-20, after 2f7920f79). ghostty-bench took 0.42-0.50 s in the same window.

### 1.2 Ghostty (the incumbent)
All citations are relative to `~/projects/ghostty` at e500d414f. Section 5 has the full review.

- Cells are `packed struct(u64)` with a 16-bit `style_id` (`src/terminal/page.zig:2143-2189`). Rows are `packed struct(u64)` holding a cell offset and flags (`page.zig:2014-2080`).
- A page is one contiguous buffer (`page.zig:1762-1798`). Its standard capacity is 215x215 cells, 128 styles and 8 KiB of graphemes (`page.zig:1896-1901`).
- The 128-slot style set holds 103 usable styles (`ref_counted_set.zig:72,184-185`).
- On overflow, `PageList.increaseCapacity` clones the page into a larger one (`PageList.zig:4172-4334`, clone at `4292-4310`). `manualStyleUpdate` can also split a page (`Screen.zig:2512-2516`).
- Grown "heap" pages are destroyed at the scrollback limit instead of being recycled (`PageList.zig:4045-4055`).
- The limit is 50 MB by default (`src/config/Config.zig:1394`), with an optional line limit (`Config.zig:1396-1409`).

### 1.3 Workloads
The scoreboard rows are defined in the epic, `br show ft-yccm0`, sections 6b and 7. The corpora come from `frankenterm/term/benches/ingest/corpus.rs` (ft-yccm0.1.2).

| Row | Corpus | What it stresses in the grid |
|---|---|---|
| **T0** | `color_emoji_random` | Two 256-colour SGRs per cell, so up to 65,536 (fg, bg) styles, and about 95% wide emoji |
| T1 | `color_random` | A 256-colour foreground per character |
| T2 | `seq_lines` | Short lines: scroll, scroll, scroll |
| T3 | `long_lines` | Long ASCII lines with wrapping |
| T4 | `unicode_mix` | Graphemes, wide characters, ZWJ, RI pairs, RTL |
| T5 | `tui_repaint` | Cursor addressing, SGR runs, erase-in-line, box drawing, no scroll |

## 2. Decisions

### 2.1 D1: module location
- New code goes in `frankenterm/term/src/pagegrid/`. It is genuinely new functionality, which the AGENTS.md file-proliferation rule allows.
- `screen.rs` gains one engine switch (section 8). It does not get a second copy of its logic.
- The whole module is safe Rust, because frankenterm-term forbids unsafe code.

### 2.2 D2: high-cardinality styles are encoded inline in the cell
The bead requires choosing between (a) inline palette encoding, (b) wider cells and (c) a large or overflowing style table. **We choose (a), in its strongest form.**

- **What moves inline.** Legacy stores the attribute bits plus `SmallColor` fg/bg (default or one of 256 palette entries) without `FatAttributes`. The cell holds the same information:
  - 14 attribute bits: legacy bits 0-10, 12, 15 and 16;
  - semantic type, wrap, width and visibility as their own cell bits;
  - 9 bits each for fg and bg, where 0 means default and n+1 means palette n.
- **What goes to the table.** A style needs the table only when it uses true colour or an underline colour. Hyperlinks and images are not styles; they live in their own side maps.
- **Printing is a template store.** The pen keeps a precomputed `u64` template, so a narrow glyph costs one store of `codepoint | template`. A wide glyph also writes the following cell (section 3.1).
- **T0 style cost:** zero hash probes, zero refcount updates and zero allocations per frame.
- **Why not (b), 12- or 16-byte cells with inline true colour.** Every row, plain text included, would grow by 50-100%, and so would every renderer copy. True colour is far rarer than palette colour in agent output. This stays the fallback if B3.8 data shows otherwise.
- **Why not (c), Ghostty's model with a bigger table.** T0 would pay a hash and probe per SGR and refcount traffic per cell, and would need roughly one table entry per cell at 36-64 B each. We keep a table only for the rare rich styles (D3).

### 2.3 D3: the rich-style table and its overflow strategy
- **One table per page.** A sealed page must be self-contained, and refcount traffic stays page-local.
- **Structure.** An entry arena `Vec<RichEntry>` indexed by **`u32` id** (id 0 reserved), with a free list. Lookup goes through an index `Box<[u32]>`: power-of-two slots, linear probing, load factor at most 0.75. The initial capacity is **512 index slots**.
- **Overflow:** when live entries exceed 0.75 x slots, the index alone doubles and is rehashed in O(live entries). Cells keep their ids. There is **no page clone and no page split**.
- **Ids can never run out.** Live ids never exceed the cells per page, plus one for an acquire-before-release transient.
  - The alternate-screen page can exceed 65,535 cells (300x220 is 66,000), which is why ids are 32 bits. The cell's style field has room for them (section 3.1).
  - Link ids are 32 bits for the same reason.
- **The pen cache does not hold a reference.** It records `(page serial, id, entry generation)` and validates it in O(1). Serials come from a per-terminal monotonic `u64`, so two different pages never share one (D6).
- **Entry contents.** `RichEntry { style: RichStyle, hash: u32, refs: u32, generation: u32 }`, where `RichStyle { attrs: u16, fg, bg, underline_color: ColorAttribute }`.
  - The colours are kept as legacy `ColorAttribute` values (f32 tuples), so materialization is exact. An entry is about 72 B (**arithmetic**).
  - Packing them to RGB8 waits until B3.8 proves that equality holds.
- **SGR cache hook (B2.4).** `rich_id(&mut self, &RichStyle, hint: Option<CachedRichId>) -> (u32, CachedRichId)`.

### 2.4 D4: page geometry and capacities
- **Cell capacity.**
  - `STD_PAGE_CELLS = 32,768` gives `rows_per_page = max(1, 32768 / (cols + 1))`: 270 rows at 120 columns, 404 at 80.
  - `cols + 1 > 32,768` gives an oversize page holding one row.
  - The alternate screen is one page of exactly `rows` rows.
- **Buffer.** One `Box<[u64]>` laid out as `[row headers: R][row seqnos: R][cells: R x (cols + 1)]`.
  - At 120 columns: 270 x 2 + 270 x 121 = 33,210 words, 265,680 B or about 259 KiB (**arithmetic**).
  - **Why stride `cols + 1`:** legacy can hold `len == cols + 1` when a wide glyph is written at the last column. Its overhang cell has its own attributes, and DCH can shift it into view (`line.rs:1514-1530`, `1609-1622`). One extra word per row stores it exactly.
- **Side structures.** Each is separately allocated and grows independently. `reset()` restores each to its standard capacity with `clear()` + `shrink_to(std)`.

| Structure | Standard capacity | Grows by |
|---|---|---|
| Rich-style index and arena | 512 slots (2 KiB index); arena allocated lazily | Index rehash (D3) |
| Grapheme arena and map | 8 KiB | `Vec` growth (section 3.5) |
| Hyperlink table and map | 4 links | `Vec` growth (section 3.6) |
| Image map | empty | Map growth (section 3.7) |

- **The pool is per geometry.** A column change flushes the idle pool, because the stride changes. Oversize pages and the alternate-screen page are never pooled.
- **Why 256 KiB of cells, versus Ghostty's 215x215 cells in a buffer of about 0.39-0.40 MB** (**arithmetic** from `page.zig:1896-1901`, `PageList.zig:325`):
  - smaller sealing units reach durability and tiering sooner;
  - the partially filled tail page wastes less;
  - each page turnover costs one bounded zeroing.

### 2.5 D5: visible rows live in pages; the alternate screen is a single page
**Primary screen.** The visible rows are the last `physical_rows` rows of the PageList.

- **Full-screen scroll** (both margins at the screen edges) is `grow()`:
  - With a default pen, the new row is `len = 0` with implicitly blank cells: O(1).
  - With any other pen, legacy fills `cols` blanks carrying `pen.clone_sgr_only()` (`screen.rs:9524-9533`). The new row then stores `cols` copies of the pen's blank template, giving `len = cols`: O(cols) stores.
  - T0 and T1 always take the non-default path. That costs about 120 stores per new row, or about 2 per T0 frame.
  - The bidi row flags are applied as legacy does; see section 9 for a legacy inconsistency.
- **Top margin 0, bottom margin < rows.** Legacy still feeds scrollback, and inserts the blank at the region bottom, so the footer rows below it shift down one stable index (`screen.rs:9379-9380`, `9537`). PageGrid does the same:
  1. `grow()` appends a row at the tail;
  2. the row headers rotate so that the new row sits at the region bottom and the footer rows move down one.
  - Rotation within a page is O(footer rows) header moves. Footer rows that cross a page boundary are copied, including their re-interned side entries.
- **Other regions (top margin > 0).** These are scroll-within-margins, so scrollback is not fed. Row headers rotate within a page, and rows are copied only across a page boundary.
- **DECSLRM band scrolls** copy cell ranges between rows, as legacy does (`screen.rs:9298-9351`). Side maps are keyed by cell offset, so every copy re-keys them.

**Alternate screen.** One page of exactly `rows x (cols + 1)`, with no scrollback, like legacy. A full scroll rotates the headers and clears the reused row.

**Rejected alternative: a separate active grid plus append-only scrollback.**
- Every scroll-off would copy `len` cells.
- On T2 with raw LF, `len` averages about `cols / 2`. That is about 480 B per line, 8.53M times per 64 MiB (**arithmetic**).

### 2.6 D6: coordinates and identity
- **Global row numbers.** `StableRowIndex` (isize, `term/src/lib.rs:73-108`) remains the row coordinate. Each `PageSlot` records `first_stable`. Stable indices increase monotonically and are never reused.
- **Lookups.** Stable row to page is a binary search over at most about 371 pages at 100,080 rows and 120 columns (**arithmetic**).
- **Anchors and placements are unchanged.**
  - Selection, viewport and cold-seam anchors (`screen.rs:140-205`, `377-407`) and kitty placements (`terminalstate/image.rs:16-20`) keep their stable-row form.
  - Reflow remaps them through the existing logical-coordinate path (`screen.rs:8103-8121`).
- **Page serials are globally unique.** They are drawn from a per-terminal monotonic `u64` on every allocate, reset and clone. This mirrors Ghostty's unique generation (`PageList.zig:407-409`). Caches key on serials: the pen's rich-id cache, the row-view cache and the renderer cache.
- **No pins.** Ghostty's tracked pins exist because its references are pointers into recycled memory (`PageList.zig:7072-7082`). Stable indices make them unnecessary here.

### 2.7 D7: threading
- **One writer.** Only the parse thread mutates the PageList, under the terminal mutex.
- **Active pages.** The renderer copies the raw cells of dirty rows under the fair lock (A3, C4b), plus the side entries of flagged rows.
- **Sealed pages.** Readers hold an `Arc<Page>` and need no lock: the renderer, the search worker, the durability writer and the compressor.

### 2.8 D8: sealing, eviction, pool
**What can still change after a row becomes scrollback.** Sealing has to accommodate each of these:

| Mutation of a scrollback row | Legacy site | Under PageGrid |
|---|---|---|
| Clearing the first viewport row clears wrap on the row above | `screen.rs:9136-9144` | **Sealing delay.** A page is sealed only when every row it holds is at least two rows above the viewport top. The seam row is therefore always in an active page. |
| A palette change bumps the seqno of every resident row | `terminalstate/mod.rs:1755, 1762-1769` | **A list-level `dirty_floor` seqno.** A row's effective seqno is `max(row_seqno, dirty_floor)`, and 0 means +inf. Pages are not written. |
| Kitty placement deletion detaches images from cells and bumps seqnos | `terminalstate/kitty.rs:990-1045` | **Copy-on-write unseal** (`Arc::make_mut`) of the affected pages. Image deletion is rare. |
| Cold-seam install after a resize swaps a resident prefix | `screen.rs:4482-4485`; called from `mux/src/localpane.rs:6904-6929` | **Page replacement.** The prefix is rebuilt as new pages, as reflow does. |
| Resize reflow, and erase scrollback (CSI 3 J) | `screen.rs:8074-8219`, `9579-9594` | New pages (B3.6), or dropped pages |
| A vertical grow pulls rows back into view | resize | `Arc::try_unwrap`, or copy-on-write if the page is shared |
| Implicit links, zone caches, renderer appdata | `line.rs:1104-1214, 978-1036, 717-751` | **Derived state** in caches keyed by `(serial, slot, effective seqno)`, never stored in the page |

**Sealing**
- A page is sealed when it is not the tail page and the sealing-delay rule holds. Sealing moves the `Box<Page>` into an `Arc<Page>` and costs O(1).
- Sealed pages expose `&self` only. Every remaining write goes through copy-on-write, so I15 can be checked.

**Eviction keeps legacy semantics exactly.**
- The cap is `physical_rows + hot_scrollback_size()`, not `+ scrollback_size` (`screen.rs:9416`, `3997-4010`).
- Rows are trimmed one at a time from the front page by incrementing `front_trim`. Each trimmed row is offered to the cold sink, with a receipt, through a row view (section 3.10). The per-row hooks run unchanged: anchor transport, cold-seam alignment and the geometry index (`screen.rs:6524-6704`).
- A **refused** row stays resident, so `retained_rows` may exceed the cap, exactly as legacy (`screen.rs:9451-9466`). Nothing is trimmed while recovery rows exist (`screen.rs:9408-9413`).
- **Handing sealed pages to A1** (ft-yccm0.2.1) is only an optimization over rows that were already admitted: the writer can encode admitted rows by `(Arc<Page>, slot)` instead of cloning a `Line`.

**Recycling and pool**
- When the front page is fully trimmed and unshared (`Arc::try_unwrap` succeeds), `reset()` runs: the cells are zeroed, the side structures are cleared and shrunk, and the page takes a fresh serial. The page then joins the per-geometry pool, which keeps at most 4 idle pages per screen.
- **Every pooled geometry is recyclable.** The cell buffer never grows. Contrast Ghostty's heap pages (`PageList.zig:4045-4055`).

## 3. Layout specification

### 3.1 Cell: `u64` (`#[repr(transparent)] struct Cell(u64)`)
Bits are numbered from the least significant.

| Bits | Field | Width | Meaning |
|---|---|---|---|
| 0..=20 | `codepoint` | 21 | First scalar of the grapheme. 0 means a blank cell, which materializes as `" "`. U+0020 written as a blank is canonicalized to 0, because legacy cannot tell them apart. |
| 21 | `grapheme` | 1 | The full UTF-8 of a multi-scalar grapheme is in the page grapheme arena, keyed by this cell's offset. |
| 22 | `width2` | 1 | This cell's grapheme is two columns wide. |
| 23 | `hidden` | 1 | The legacy visible-cell walk skips this cell, because a visible width-2 cell precedes it. Writers maintain this bit exactly (I1). It gives O(1) access to the visible cell to the left. |
| 24..=25 | `semantic` | 2 | Output, Input or Prompt (legacy bits 13-14) |
| 26 | `hyperlink` | 1 | The page link map has an entry for this cell. Blank and hidden cells may carry one. |
| 27 | `image` | 1 | The page image map has entries for this cell. Blank and hidden cells may carry them. |
| 28 | `rich` | 1 | 0: the style field holds the inline style. 1: it holds a `u32` rich-style id. |
| 29..=60 | `style` | 32 | Inline style (attrs 14, fg 9, bg 9; see below), or a rich-style id. |
| 61 | `wrapped` | 1 | Legacy attribute bit 11, stored per cell because DCH and band copies move it inward. |
| 62..=63 | reserved | 2 | Zero. One candidate use is DECSCA "protected". |

The inline `style` field, from its low bit:
1. Attributes (14 bits): intensity 2, underline 3, blink 2, italic 1, reverse 1, strikethrough 1, invisible 1, overline 1, vertical align 2.
2. fg (9 bits): 0 means `Default`, n+1 means `PaletteIndex(n)`.
3. bg (9 bits): same encoding.

Values 257-511 are invalid.

- **Hidden cells are stored as full cells.** Print writes the cell after a wide head as legacy's `blank_with_attrs(head attrs)` (`line.rs:1527-1529`), but ICH, DCH and band copies can leave any cell there (`line.rs:1579-1622`, `screen.rs:9298-9330`). That cell keeps its own style and becomes visible again if the head is narrowed.
- **Ingest from the wire or cold storage is masked.** Legacy `CellAttributes` deserialized there can carry bits 17-31, or `fat: Some(..)` with every field at its default (`cell/src/lib.rs:67-79`). The view and the import path mask and normalize these, so they never reach the page.

### 3.2 Row header: `u64`, plus a seqno indexed by slot

| Bits | Field | Meaning |
|---|---|---|
| 0..=31 | `slot` | This row's cell block is cells `[slot x (cols+1), slot x (cols+1) + cols + 1)`. Rotating headers moves rows without moving cells. |
| 32..=48 | `len` | Legacy `Line::len()`, 0..=cols+1. Cells at `x >= len` are zero. |
| 49 | `dirty` | Renderer consume-and-clear (C2.4, C4b) |
| 50 | `styled` | Summary: the row may hold rich cells. |
| 51 | `grapheme` | Summary: the row may hold grapheme cells. |
| 52 | `hyperlink` | Summary: the row may hold hyperlink cells. |
| 53 | `image` | Summary: the row may hold image cells. |
| 54 | `semantic` | Summary: the row may hold cells that are not Output. |
| 55 | `bidi_enabled` | Legacy `LineBits` |
| 56 | `rtl` | Legacy `LineBits` |
| 57 | `auto_detect_direction` | Legacy `LineBits` |
| 58 | `double_width` | Legacy `LineBits` |
| 59 | `double_height_top` | Legacy `LineBits` |
| 60 | `double_height_bottom` | Legacy `LineBits` |
| 61 | `legacy_form_c` | Mirrors whether legacy would hold the row in C (clustered) or V (vector) storage. Needed only while the storage-form-dependent behaviour in section 9 survives. |
| 62..=63 | reserved | |

- **Summary flags may be stale.** Each summary flag can be true when nothing is left, but never false while something is there (I9).
- **Wrap and overhang are derived.** "Wrapped" means the last visible cell has its per-cell `wrapped` bit. "Overhang" means `len == cols + 1`.
- **Seqnos travel with the cells.** `row_seqno[slot]: u64` follows legacy `Line::seqno` semantics. It is indexed by slot, so it moves with the cells.
- **Effective seqno** is `max(row_seqno, list.dirty_floor)`, where 0 means "always changed".

### 3.3 Page

```rust
pub struct Page {
    buf: Box<[u64]>,          // [headers: rows][seqnos: rows][cells: rows * (cols + 1)]
    cols: u16,
    rows: u32,                // capacity (rows_per_page)
    used: u32,                // rows handed out by grow()
    serial: u64,              // globally unique per terminal (D6)
    max_seqno: u64,           // max row seqno; 0 = some row is "always changed"
    styles: RichStyleTable,   // 3.4
    graphemes: GraphemeArena, // 3.5
    links: LinkTable,         // 3.6
    images: ImageMap,         // 3.7
}
```

- The **cell offset** of `(slot, x)` is `slot x (cols + 1) + x`. Every side map is keyed by it.
- **Re-keying.** ICH, DCH and band copies move cells. When a moved cell has its `grapheme`, `hyperlink` or `image` bit set, the matching side-map entry is re-keyed.
- **Writing a cell**:
  1. Release the old cell's side entries, but only if one of its side bits is set.
  2. Apply legacy's invalidation of a wide head when overwriting the cell after it (`line.rs:1555-1577`), including that head's retained attributes.
  3. Store the new `u64`.
  4. Recompute the `hidden` bits from x until the walk re-synchronizes, which is O(1) for a print.
  5. Update `len`, the summary flags, `dirty`, `row_seqno` and `max_seqno`.

### 3.4 Rich-style table
- The structure is described in D3.
- **Hashing:** FxHash over the packed `RichStyle`.
- **Deletion:** a backward shift in the index when a refcount reaches 0. The id goes to the free list and its generation is bumped.
- **Equality:** field-wise, with the same `ColorAttribute` equality legacy uses.

### 3.5 Grapheme arena
- **Storage.** A cell with the `grapheme` bit stores its first scalar inline. The arena stores the complete UTF-8.
  - Legacy, by contrast, boxes graphemes of 8 bytes or more on the heap (`cell/src/lib.rs:1186-1203`).
- **Map.** Cell offset maps to `(u32 offset, u16 len)`.
- **Free space.** Size-class free lists hold freed space. The arena is compacted when waste exceeds half of it.
- **Overflow.** The `Vec` grows. Nothing is cloned. Ghostty clones the page instead (`Screen.zig:2618-2634`).
- **Width** is decided at print time with the active Unicode version, as legacy does (`performer.rs:486` at HEAD).

### 3.6 Hyperlinks (OSC 8) and implicit links
- **Explicit links**
  - Stored in a per-page `LinkTable`: entries `Vec<Option<(Arc<Hyperlink>, refs)>>` with `u32` ids and a free list, plus a map from cell offset to id.
  - **The table keeps the pen's original `Arc`.** Entries are deduplicated by `Arc::ptr_eq`, not by value, because legacy and the wire codec split hyperlink spans by pointer identity (`codec/src/lib.rs:16368-16371`).
- **Implicit links** are derived, as legacy derives them (`line.rs:1104-1214`; callers in `mux/src/pane.rs:1061`, `renderable.rs:278`, `localpane.rs:1884`). They are cached per `(serial, slot, effective seqno)`. I16 masks the two implicit-link `LineBits` (section 6).

### 3.7 Images (kitty, iTerm, sixel)
- **Per-cell storage.** The page `ImageMap` maps a cell offset to the legacy `Vec<Box<ImageCell>>` (`cell/src/image.rs:705-724`), unchanged. The cell and row `image` bits gate every access.
- **Overwrite rule.** Legacy carries placement-bearing images over when a cell is overwritten (`vecstorage.rs:401-423`). The page writer does the same.
- **Blank cells can hold images.** Placement attaches to `Cell::blank` when no cell exists (`terminalstate/image.rs:296-321`).
- **Placement deletion** on sealed pages unseals them by copy-on-write (D8).
- **Kitty state** stays in `TerminalState` (`terminalstate/kitty.rs:23-42`).

### 3.8 Semantic zones, bidi, Unicode version, double-size lines
- **Semantic zones (OSC 133).**
  - Each cell keeps 2 bits, as today.
  - Rows that hold only Output still contribute an Output range that extends the current zone, including legacy's `last_non_blank = len` quirk (`line.rs:978-1029`, `terminalstate/mod.rs:3632-3672`).
  - When the row `semantic` flag is false, the zone builder emits that range in O(1) instead of scanning the cells. It does **not** skip the row.
- **Bidi.** The bidi row flags are copied from the terminal's `BidiMode` exactly where legacy applies it, including the inconsistencies listed in section 9. Runs are still computed at render time (`surface/src/cellcluster.rs:166-194`).
- **Unicode version.** The stack stays in `TerminalState`. Cells store only the resulting width.
- **Double-size lines.** Row flags hold DECDWL and DECDHL.

### 3.9 PageList

```rust
pub struct PageList {
    pages: VecDeque<PageSlot>,  // oldest first
    pool: Vec<Box<Page>>,       // <= 4 idle, reset pages of the current geometry
    cols: u16,
    retained_rows: usize,       // may exceed hot_cap only while spills are refused
    hot_cap: usize,             // physical_rows + hot_scrollback_size()
    dirty_floor: u64,           // palette changes etc.; 0 = always changed
    next_serial: u64,
}

pub struct PageSlot {
    first_stable: StableRowIndex,
    front_trim: u32,
    page: PageRef,              // Active(Box<Page>) | Sealed(Arc<Page>)
}
```

**Change tracking.** `get_changed_stable_rows` keeps legacy's contract: it scans the requested range only (`screen.rs:10000-10035`), and includes the cold-visual branch. Within that range it skips pages whose `max_seqno` (0 meaning +inf) and `dirty_floor` are both at or below the query seqno.

### 3.10 Materialized `Line` view (B3.5)
- **Reconstruction.** `line_view(slot) -> Line` rebuilds the legacy row from the cells, side maps, row header and seqno:
  - storage form from `legacy_form_c`;
  - `len`;
  - every visible and hidden cell with its full `CellAttributes`, wrap bits included;
  - `LineBits`;
  - seqno.
- **Caching.** Views are cached per `(serial, slot, effective seqno)`. The cache also holds the relocated renderer `appdata`.
- **Mutating consumers.** Some legacy callers mutate rows in place: `for_each_phys_line_mut` (`screen.rs:10091`), overlays and implicit links. They receive owned views. Their writes are either derived caches, which stay outside the page, or explicit edits, which B3.4 maps to page writes.

## 4. High-cardinality evaluation and per-corpus estimates

### 4.1 Setup
- **Geometry:** 80 rows x 120 columns, with 3,500 lines of scrollback. Hot scrollback equals the configured scrollback, because tiering is off by default, so 3,580 rows are retained.
- **Operator config:** a scrollback of 100,000 lines, i.e. 100,080 rows.
- **T0 shape, measured from the generator:** 2,685,186 frames per 64 MiB (seed 20261005), averaging 24.99 B per frame.
  - The average width is 1.951 columns, so there are about 61.5 frames per 120-column row.
  - That gives about 43,600 rows per 64 MiB (**arithmetic**).
- **T2 shape:** about 8.53M lines per 64 MiB (**arithmetic** over `seq` digit lengths).

### 4.2 Memory per retained row (**arithmetic** from type sizes; no RSS was measured)

| Corpus | Legacy (today) | Ghostty | PageGrid |
|---|---|---|---|
| T0 | ~2.0 KB: a C row with one 24 B cluster per emoji, 256 B of text capacity, a bitset, the `ClusteredLine` and the `Line` | ~3.5 KB: a heap page of ~1.33 MB per ~385 rows after two capacity increases (style capacity 3,328, then 26,624) | **~984 B**: 121 x 8 B of cells plus 16 B of header and seqno |
| T1 | ~3.4 KB: one 24 B cluster per character | ~1.3 KB: one increase to 4,096 style slots | **~984 B** |
| T2 (raw LF) | ~384 B: one or two clusters plus about 67 B of text | ~970 B | **~984 B**. Fixed stride costs more than legacy for sparse rows; B3.7 mitigates this (section 9). |
| T5 (80 visible rows) | ~3.1 KB per V row | ~1 KB | **~984 B** |

| Retained rows | Legacy T0 / T1 / T2 | PageGrid, any corpus |
|---|---|---|
| 3,580 | ~7.2 MB / ~12 MB / ~1.4 MB | 14-15 pages x 265,680 B = 3.72-3.99 MB, plus up to 4 pooled pages (1.06 MB), plus the alternate page (80 x 121 x 8 + 1.3 KB, about 79 KB) |
| 100,080 | ~200 MB / ~340 MB / ~38 MB | about 371 pages, ~98.6 MB, plus the pool and alternate page |

- The legacy numbers come from the asserted sizes (`cell/src/lib.rs:1628-1636`) and the layouts at `clusterline.rs:20-43` and `line.rs:166-175`. The `ArcInner` and `Mutex` overheads are estimates.
- Ghostty's figures are arithmetic from its layout and capacity code (section 5). They were not measured.

### 4.3 Grid work per T0 frame (2 SGRs plus 1 wide glyph)

| | Legacy (today) | Ghostty | PageGrid |
|---|---|---|---|
| Per SGR | Writes into the pen `CellAttributes`; palette colours allocate nothing | `manualStyleUpdate`: release the old style, hash, Robin Hood probe, insert (`Screen.zig:2469-2523`). The intermediate (new fg, old bg) style is added (`2499`) and released by the next SGR (`2477`). Dead tail ids are trimmed on the next add (`ref_counted_set.zig:270-273`). | Rewrite 9 bits of the pen template |
| Glyph | `Graphemes`, width, `pen.clone()`, `Arc::make_mut`, compare attrs with the previous cluster, push a cluster, push text, grow the bitset (`clusterline.rs:356-396`) | Slow print path; 2 `styles.use` (`Terminal.zig:847-852, 1140-1155`) | 2 `u64` stores (head, then the hidden cell), the `hidden` recompute, and updates to `len`, `dirty` and the seqnos |
| New row (every ~61.5 frames) | Reuse the evicted row with `resize_and_clear` and a styled fill (`screen.rs:9468-9484`) | Memset the row with bg-only cells (`Screen.zig:1046-1053`) | `cols` template stores, about 2 per frame amortized |
| Hash probes / refcount updates per frame | 0 / 0 | ≥ 2 table operations per SGR / 2 | **0 / 0** |
| Allocation | `Vec` and `String` growth per row | A ~1.33 MB heap page per page; about 2 full-page clones per page (`PageList.zig:4292-4310`); destroyed at the limit | **None in steady state** |
| Bytes written per frame | ~28 B plus growth copies | 16 B plus style-table traffic plus the row memset | **~32 B**: 16 B for the glyph plus about 16 B of amortized row fill |

### 4.4 T2 and T5
- **T2, per line.**
  - Legacy: compress the departing row, then evict and reuse or allocate (`screen.rs:9368-9559`). That is several allocations per scroll when the row moves between V and C storage.
  - PageGrid: one row-header initialization, about six cell stores, and one pooled page turnover every 270 lines.
- **T5, per frame.**
  - Legacy: the renderer captures rows by cloning their `Arc` (`render/pane.rs:593-630`). The next write to each row hits `Arc::make_mut`, which clones a full V row of about 2.9 KB.
  - PageGrid, phase 1 (B3.5): the capture still materializes views for dirty rows, cached per seqno. Phase 1 therefore does not yet remove that cost; it is measured by the read-path lane (section 8.3).
  - PageGrid, end state (C4b): a 968 B memcpy per dirty row.

### 4.5 Throughput
- **No MiB/s figure is claimed.**
- **How it will be validated:**
  - the B3.2 micro-benches;
  - the ft-yccm0.1.2 bench with `--grid-engine legacy|page` in one binary, ABBA, in a quiet window, with `load_avg_1m` recorded;
  - lanes `term` and `mux_two_stage` for ingest, plus a new read-path lane (section 8.3).

## 5. Review against Ghostty

### 5.1 What we adopt
- **The 8-byte packed cell** (`page.zig:2143-2189`). We use width plus hidden bits instead of Ghostty's spacer tags (`page.zig:2213-2226`). The tags cannot represent legacy's hidden cells, which carry their own attributes.
- **Packed row headers holding a cell offset,** so rows rotate in O(1) (`page.zig:2014`), with conservative summary flags:
  - `styled`, `page.zig:2032-2042`;
  - `grapheme`, `page.zig:2030`;
  - `hyperlink`, `page.zig:2047`;
  - `dirty`, `page.zig:2078`.
- **O(1) `grow()`** (`PageList.zig:3962-3981`).
- **Zeroed page reuse at the limit** (`PageList.zig:4058-4085`), and a pool: Ghostty preheats 4 (`PageList.zig:35-38, 361-399`).
- **Side maps keyed by cell offset** for graphemes (`page.zig:96-100`) and hyperlinks (`hyperlink.zig:20-23`).
- **A globally unique page serial** (`PageList.zig:407-409`).
- **Per-page refcounted style storage** (`style.zig:681-696`), but only for rich styles.

### 5.2 Where we deliberately improve
1. **Palette styles cost nothing.** Ghostty's inline bg colour exists only for blank fill cells (`page.zig:2191-2205`, `style.zig:230-246`, `Screen.zig:1995-1998`). Printed glyphs always take a style id (`Terminal.zig:1709-1716`).
2. **At least 512 rich-style slots, `u32` ids, and no page clone.** Ghostty's style set holds 103 styles (`ref_counted_set.zig:72,184-185`). When it is full, it clones the page and re-adds every styled cell (`PageList.zig:4206-4310`, `page.zig:924-1038`), or splits the page (`Screen.zig:2512-2516`). On T0 that is about 2 clones per page (**arithmetic**).
3. **Every pooled geometry is recyclable.** Ghostty's heap pages are destroyed at the limit (`PageList.zig:4045-4055, 4531-4551`). Its `compact()` has no non-test caller (`PageList.zig:3609-3669`).
4. **Grapheme overflow does not clone either** (contrast `Screen.zig:2618-2634`).
5. **No pointer pins** (contrast `PageList.zig:4320-4324, 3654-3659, 4035-4042, 1644-1665`).
6. **Sealed pages feed durability and tiering.** Ghostty compresses idle pages from its renderer thread (`PageList.zig:88-91, 4801-4829`; `renderer/Thread.zig:848`) but has no durability writer.
7. **The renderer (C4b, out of scope here).** Ghostty rebuilds fully whenever the viewport moves (`render.zig:380-414`). Our per-row seqnos and page serials let the renderer keep unchanged rows.

### 5.3 Ghostty behaviour we must match or justify
- **Reflow.** Ghostty reflows only on column changes, rewriting the scrollback into new pages (`PageList.zig:1288-1486`). B3.6 must keep FrankenTerm's own semantics instead, including Knuth-Plass wrapping (`line.rs:2112-2178`) and deferred rows (`screen.rs:7139-7199`).
- **Kitty images** live per screen in Ghostty (`Screen.zig:77-81`). Ours stay in `TerminalState`.

## 6. Invariants (checkable; debug-asserted in B3.2-B3.4)

Notation:
- `p` is a page; `r` is a row header; `c(r, x)` is the cell at column `x` of row `r`; `off(r, x) = r.slot x (cols + 1) + x`.
- `stored rows` means every row in `0..p.used`, **including** front-trimmed rows that are still awaiting recycle.

**Cells and rows**

| ID | Invariant |
|---|---|
| I1 | **Visibility equals legacy's walk.** `hidden(c(r,0)) == false`. For x > 0, `hidden(c(r,x)) == !hidden(c(r,x-1)) && width2(c(r,x-1))`. |
| I2 | `r.len <= cols + 1`. For `x >= r.len`, `c(r,x) == 0`. |
| I3 | `c.codepoint == 0` implies `!c.grapheme`. Blank and hidden cells may carry hyperlinks, images and any style. |
| I4 | `!c.rich` implies inline `fg <= 256 && bg <= 256`. Reserved bits 62-63 are zero. |

**Side tables**

| ID | Invariant |
|---|---|
| I5 | For every id `k`: `p.styles.refs[k] == count of stored-row cells with rich && id == k`. Every referenced id is live. Each live id appears in exactly one index slot. |
| I6 | `c.grapheme` holds if and only if the grapheme map contains `off(r,x)`. Arena ranges are disjoint and valid UTF-8, and their first scalar equals `c.codepoint`. |
| I7 | `c.hyperlink` holds if and only if the link map contains `off(r,x)`. A link's refs equal the number of cells mapping to it. `c.image` holds if and only if the image map contains `off(r,x)`. |
| I8 | After any ICH, DCH or band copy, every side-map key names a cell that has the matching bit set (re-keying is complete). |
| I9 | Each row summary flag that is false implies none of the matching cells exist in the row: `styled`, `grapheme`, `hyperlink`, `image`, `semantic`. |

**Rows, pages and the list**

| ID | Invariant |
|---|---|
| I10 | The effective seqno of a row is at least the seqno of every write to it, `dirty_floor` included. `p.max_seqno` is at least every row seqno in `p`, and is 0 if any row seqno is 0. |
| I11 | Within a page, the row slots of headers `0..p.used` are distinct. |
| I12 | Serials are unique among every page that exists at the same time, alive or pooled, in one terminal. |
| I13 | `retained_rows == sum over pages of (used - front_trim)`. `retained_rows > hot_cap` only while the most recent spill was refused, or while recovery rows exist. Each `first_stable` equals the previous `first_stable + used - front_trim`. Stable indices strictly increase and are never reused. |
| I14 | A page leaving `reset()` has all-zero cells, `used == 0`, empty side structures at no more than standard capacity, and a fresh serial. |
| I15 | Sealed pages change only through copy-on-write (image detach, vertical grow). No sealed page contains a row within two rows of the viewport top. |

**Equivalence with legacy**

| ID | Invariant |
|---|---|
| I16 | **Differential (the B3.8 gate).** For every input byte stream and chunking, the materialized view of each retained row equals the legacy engine's `Line`. That means every stored cell (visible and hidden) with `(str, width, CellAttributes)`, plus `len`, `LineBits` with the two implicit-link scan bits masked, and the zone ranges. Seqnos are compared by `changed_since` behaviour over a set of query seqnos, not by value. Storage form is compared only while `legacy_form_c` exists. |

## 7. Consumer inventory

### 7.1 Method and scope
- **Method:** `rg` over the workspace on 2026-10-06; the command is recorded in the bead comment.
- **Result:** 69 non-test files touch `Line`, `Cell`, `CellRef`, `CellAttributes`, `ClusteredLine` or the Screen row APIs.
- **Per crate:** frankenterm-gui 18, term 9, surface 9, mux 8, frankenterm-mux-server-impl 5, client 3, termwiz 3, tabout 2, font 2, config 2, frankenterm-core 4, codec 1, cell 1, termwiz-funcs 1, mux-lua 1.
  - Only 1 of the frankenterm-core files is a real consumer (`vendored/mux_client.rs`).
  - A few files use the types only in inline tests.
- **Access patterns:**
  - **HOT** means per byte, per frame, or per visible row on every paint.
  - **MATERIALIZED** means occasional, so a view built on demand is acceptable.

### 7.2 HOT consumers

| Consumer | Location | API | Notes |
|---|---|---|---|
| Performer and Screen setters | `term/src/terminalstate/performer.rs` (print, ZWJ re-cluster, wrap); `term/src/screen.rs:8861-9165` | `set_cell_grapheme`, `set_ascii_cell_run`, `insert_cell`, `erase_cell`, `clear_line`, `line_mut` | B3.4 replaces it. Includes `clear_line`'s write to the previous row (`screen.rs:9136-9144`). |
| Scrolling and eviction | `screen.rs:9269-9560, 9623-9740, 6524-6704` | `compress_for_scrollback`, margin copies, spill hooks | B3.3 and B3.4. Per-row admission is kept (D8). |
| CSI edits, REP, DECALN, palette dirtying | `term/src/terminalstate/mod.rs:1755-1769, 2863-3210`; `performer.rs` DECALN | `cells_mut`, `fill_range`, `make_all_lines_dirty` | B3.4, plus `dirty_floor` |
| Kitty placement mutators | `term/src/terminalstate/kitty.rs:990-1045`; `image.rs:296-321` | `cells_mut`, `attach_image`, `detach_image_with_placement` | Copy-on-write on sealed pages |
| Render frame capture | `gui/src/termwindow/render/pane.rs:593-630, 1338-1342`; `mux/src/localpane.rs:374-393, 5735, 5920-5941` | `(**line).clone()`, `with_phys_lines`, appdata | Phase 1: cached views. C4b: packed dirty rows. |
| Line render and shaping | `render/pane.rs:1085-1330`; `render/screen_line.rs:63-960`; `render/mod.rs:85, 1173`; `surface/src/cellcluster.rs`; `font/src/lib.rs:23, 1863` | `WithPaneLines::with_lines_mut`, `cluster`, `get_cell`, `CellRef::attrs()` | The seam is `&mut [&mut Line]` (`mux/src/pane.rs:1022-1051`). |
| Dirty tracking | `gui/src/termwindow/mod.rs:7124-7300`; `mux/src/renderable.rs:137`; `screen.rs:10000-10035` | `get_changed_stable_rows` (including the cold-visual branch), `changed_since` | Effective seqno, page `max_seqno` |
| Implicit hyperlinks | `mux/src/localpane.rs:1858-1890`; `renderable.rs:262-290`; `line.rs:1164` | `apply_hyperlink_rules` | Derived (section 3.6) |
| Remote panes | `frankenterm/client/src/pane/renderable.rs:347-4530`; `clientpane.rs:5581-5625` | Line LRU, image hydration, prediction overlays | Fed by RPC, so they stay on `Line` |
| Render push to clients | `frankenterm-mux-server-impl/src/sessionhandler.rs:3489-3590`; `localpane.rs:2120-2237` | `lines_in_phys_range`, `SerializedLines::from` | Wire schema is `Line` serde, so views |
| Overlays (copy mode, quick select) | `gui/src/overlay/copy.rs:4225-4800`; `quickselect.rs:2000-3600` | Clone a row, then attribute overrides | Views, until a C-track override layer |

### 7.3 MATERIALIZED consumers

| Consumer | Location | Notes |
|---|---|---|
| Wire `SerializedLines`, GetLines, render-change validation | `frankenterm/codec/src/lib.rs:5768-5830, 15210-15350, 16001-16460`; `sessionhandler.rs:280-320, 9705-9760, ~9925` | Schema is `Line` serde, including `CellStorage` V/C. Hyperlink spans split by `Arc` identity. |
| Durable cold store | `mux-server-impl/src/lib.rs:7870-8260, 8940-9100` (`ExactSemanticScrollbackLineV1{line: Line, cell_widths}`) | Schema is `Line` serde. Per-row admission is kept. |
| Recovery activation | `screen.rs:6341-6355` (`ScrollbackPrefix::from_slices` borrows `&[Line]` slices of the `VecDeque`) | Needs owned views, or a page-native prefix |
| Cold reads, cold-seam install, reflow sources | `screen.rs:897-2400, 4482-4485, 9752-9990`; `mux/src/localpane.rs:6904-6929` | Page replacement (D8) |
| Selection and semantic selection | `gui/src/termwindow/selection.rs:436-760, 1475-1740, 2581-2650` | `get_logical_lines`, `as_str` |
| Search | `mux/src/localpane.rs:2957-3631`; `clientpane.rs:5702` | Read-only copies |
| Semantic zones | `term/src/terminalstate/mod.rs:3632-3672`; `mux/src/pane.rs:620-660`; Lua `mux-lua/src/pane.rs:296-370` | Derived (section 3.8); `for_each_phys_line_mut` (`screen.rs:10091`) |
| Lua text and escapes | `mux-lua/src/pane.rs:210-262`; `termwiz-funcs/src/lib.rs:269-280` | Views |
| ft capture and robot get-text | `crates/frankenterm-core/src/vendored/mux_client.rs:2907-3240, 5855-6168` | Wire copies; unaffected |
| tmux capture-pane | `mux-server-impl/src/dispatch.rs:6392-6396` | Views |
| Checkpoints | `term/src/terminalstate/checkpoint.rs:1047-1300, 2193-2230`; `screen.rs:4034-4075, 5775-6110` | Own JSON schema; views |
| Anchors and cold seams | `screen.rs:2540-2720, 4206-4760, 5319-5700` | `len`, wrap and `changed_since` |
| Own-`Line` users | `frankenterm/surface/src/lib.rs:158`; `gui/src/tabbar.rs`; `gui/src/main.rs:5210-5290` | Not grid data; unaffected |
| Recorder and replay | none: the replay crates do not import term, surface or cell | Unaffected |

### 7.4 What this means for the migration
- The **write path** stays entirely inside frankenterm-term (B3.4).
- The **read path** is `WithPaneLines` and `with_phys_lines`, served by cached views in phase 1.
- **Wire and durable schemas stay `Line` serde.** Decoded rows are equal to today's; the encoded bytes may differ only where legacy's own V/C choice is not reproduced. Section 9 covers that.

## 8. Migration plan

### 8.1 Phases (each is one bead and lands behind the kill switch)
1. **B3.2** (ft-yccm0.3.3.2): `pagegrid::{page, style, grapheme, links}`, with proptests for I1-I9, I11, I12 and I14, and criterion micro-benches.
2. **B3.3** (ft-yccm0.3.3.3): `PageList`, covering grow, region insert, trim with per-row admission, sealing, pool and stable lookup. Proptests for I10, I13 and I15.
3. **B3.4** (ft-yccm0.3.3.4): `Screen` rows become `enum Rows { Legacy(VecDeque<Line>), Page(PageList) }`. `FT_GRID_ENGINE` is read once in `Terminal::new`, with `legacy` as the default.
4. **B3.5** (ft-yccm0.3.3.5): views (section 3.10) for every consumer in sections 7.2 and 7.3 that is not yet page-native.
5. **B3.6** (ft-yccm0.3.3.6): page-wise reflow with legacy semantics. **B3.7** (ft-yccm0.3.3.7): tiering on sealed pages.
6. **B3.8** (ft-yccm0.3.3.8): the differential campaign (section 8.2). **B3.9** (ft-yccm0.3.3.9): the default flip (section 8.3).

### 8.2 Differential gating (B3.8)
- **The gate is a `DualEngine` harness**, not the ingest bench fingerprint. That fingerprint hashes only row text and visible fg/bg (`benches/ingest/lanes.rs`), so it serves as a smoke signal only.
- **What the harness compares.** It runs both engines on the same stream. After every `advance_bytes` it checks I16 for every retained row, plus:
  - the cursor;
  - `get_changed_stable_rows` over a set of query seqnos;
  - `get_semantic_zones`;
  - logical lines;
  - spill receipts.
- **Corpora:**
  - all six ingest corpora at several chunk sizes;
  - `frankenterm/term/src/test` and `frankenterm/term/tests/*`;
  - the reflow corpus;
  - vttest and esctest;
  - at least 24 hours of fuzzing, including palette changes, kitty placement and deletion, region scrolls with footers, and resizes.

### 8.3 Default-flip criteria (all required)
1. **Correctness.** Zero DualEngine divergences across the corpus set and the 24-hour fuzz. vttest and esctest show no regression against legacy.
2. **Ingest speed.** On the native M4 Pro in a quiet window, ABBA with CV ≤ 5% and the load average recorded:
   - `page` is at least as fast as `legacy` on every corpus in the `term` and `mux_two_stage` lanes;
   - T0 improves on the term lane.
3. **Read-path speed.** A new `render_capture` lane in the ft-yccm0.1.2 bench materializes the visible rows every frame, as `GetPaneRenderChanges` and the GUI capture do. `page` must be at least as fast as `legacy` there. Resize SLOs must not regress.
4. **Memory.** RSS after T1 at the operator's 100,000-line scrollback meets M2 (≤ 1 GB) with B3.7 enabled.
5. **Durability.** Per-row admission parity, including refusal and recovery holds, and cold-store round-trip parity.
6. **Kill switch.** `FT_GRID_ENGINE=legacy` stays for at least one release. Legacy hot paths are removed only with explicit owner approval (AGENTS.md Rule 1).

## 9. Risks and open questions

**Fix legacy first (Rule 1.5).** These legacy behaviours depend on internal storage form or history rather than on terminal semantics. Each should be fixed in legacy before B3.4, so that both engines can agree without the `legacy_form_c` mirror. B3.4 files beads for them; until they are fixed, PageGrid mirrors them.
- **A default space printed past the end of a row** grows `len` in V storage (`line.rs:1514-1520`) but returns early in C storage (`line.rs:1397-1401`).
- **`set_last_cell_was_wrapped` on an empty row** appends a blank only in C storage (`line.rs:1880-1903`).
- **Reused rows skip bidi.** Rows reused through the eviction `to_move` path never call `BidiMode::apply_to_line` (`screen.rs:9468-9484` against `9533`), and `resize_and_clear` zeroes the row bits.

**Memory**
- **Fixed stride for sparse rows.** T2-like scrollback costs more than legacy C rows. Mitigations: B3.7 compresses sealed, zero-heavy pages, and a later compact sealed-row form could store only `len` cells.
- **Large scrollback.** At 100,000 rows, PageGrid needs about 98.6 MB per pane at 120 columns regardless of content. Legacy needs 38-340 MB depending on content. B3.7 sets the RSS bound.

**Formats**
- **Rich colour equality.** Rich colours stay f32 `ColorAttribute` until B3.8 proves an RGB8 packing equal.
- **Wire and disk.** Phase 1 pays a materialization per pushed or spilled row. Page-native codecs need their own schema versions (`docs/proposals/codec-versioning-rolling-upgrade.md`).

**Read path**
- **Phase 1 cost.** Copy-mode decoration and every capture go through views until C4b. The read-path lane (section 8.3) measures this.

## 10. References

FrankenTerm:
- `frankenterm/cell/src/lib.rs`
- `frankenterm/surface/src/line/{line,vecstorage,clusterline,storage,linebits}.rs`
- `frankenterm/term/src/{screen.rs,config.rs,terminalstate/{mod,performer,checkpoint,image,kitty}.rs}`
- `crates/frankenterm-mux-server-impl/src/{lib.rs,sessionhandler.rs}`
- `frankenterm/codec/src/lib.rs`
- `frankenterm/mux/src/{pane,localpane,renderable}.rs`
- `crates/frankenterm-gui/src/termwindow/render/*`
- `frankenterm/term/benches/ingest/`

Ghostty (e500d414f):
- `src/terminal/{page.zig,PageList.zig,style.zig,ref_counted_set.zig,Screen.zig,Terminal.zig,hyperlink.zig,render.zig,bitmap_allocator.zig,kitty/graphics_storage.zig,kitty/graphics_unicode.zig}`
- `src/config/Config.zig`

Evidence:
- `evidence/mac-render-perf/2026-10-05/README.md`

## 11. Review record

**Revision 1 was reviewed on 2026-10-06** by a general-purpose subagent of CopperLynx's session. It is the same model family, so it does **not** satisfy the bead's "another agent or model" requirement. The reviewer checked about 55 citations and the arithmetic against the source. It reported 3 blockers, 7 majors and 6 minors. All are resolved in revision 2:

| # | Finding | Resolution |
|---|---|---|
| 1 | Sealed pages are written: `clear_line` clears wrap on the row above, a palette change dirties all rows, kitty deletion detaches images, the cold-seam install swaps a prefix | D8 mutation table: sealing delay, `dirty_floor`, copy-on-write, page replacement; I15 restated |
| 2 | I1, I3 and I5 were false for legacy (hidden cells keep their own attributes; images on blanks; truncated alternate rows) | Width and hidden bits with an exact walk (I1); overhang derived from `len == cols + 1`; I3 relaxed |
| 3 | Wrap is per cell in legacy (DCH moves it) | Per-cell `wrapped` bit (bit 61); the row-level value is derived |
| 4 | `u16` ids can run out on a large alternate-screen page | `u32` rich and link ids |
| 5 | A serial bumped on reset is not unique | Per-terminal monotonic serials (D6, I12) |
| 6 | Eviction model differed from legacy (receipts, refusal, recovery hold, hot cap) | D8 per-row admission; I13 restated |
| 7 | Top margin 0 with a bottom margin still feeds scrollback; a non-default pen fills new rows | D5 rewritten; section 4.3 bytes per frame now include the fill |
| 8 | `len` depends on storage form; bytes are not identical; links split by `Arc` identity | `legacy_form_c` plus the fix-legacy-first list; schemas, not bytes; `ptr_eq` deduplication; I16 masks the scan bits |
| 9 | Missing consumers (recovery `from_slices`, `for_each_phys_line_mut`, the `clear_line` write, kitty mutators, cold-seam install, cold-visual branch, seqno 0) | Added to sections 7.2 and 7.3, 3.9 and 3.10 |
| 10 | The flip criteria had no read-path lane; the fingerprint is not an equivalence gate | `render_capture` lane in section 8.3; DualEngine I16 harness in section 8.2 |
| 11-16 | The semantic-flag skip; bidi on reused rows; deserialization masking; pool geometry; citation fixes; arithmetic (T2 8.53M lines, 14-15 pages, pool and alternate page added) | Sections 3.8, 9, 3.1, 2.4, 1.1 and 4 updated |
