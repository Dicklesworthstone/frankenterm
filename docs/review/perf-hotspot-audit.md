# Performance Hot-Spot Audit (review pass)

**Scope:** four hot paths called out by the user — (1) ingest/poll loop
in `runtime.rs` + `ingest.rs`, (2) FTS5 query path in `storage.rs` +
`search/`, (3) pattern detection in `patterns.rs` + `pattern_trigger.rs`,
(4) codec PDU encode/decode in `frankenterm/codec`.
**Method:** ripgrep for known hot patterns (`O(n²)` loops, hot
allocations in tight scopes, sync calls in async contexts, lock
contention, redundant clones), then targeted reads of the entry points
the user named.
**Date:** 2026-04-26

## TL;DR

Two real perf opportunities filed. The hot-path code is generally
well-engineered (Aho-Corasick + quick-reject in patterns, memchr
SIMD + bounded overlap in delta extraction, read/write cache in
list_panes). The two findings are foundational rather than tactical:

| # | Finding                                                     | Severity | Bead       |
| - | ----------------------------------------------------------- | -------- | ---------- |
| 1 | Storage read-path opens fresh SQLite `Connection` per query (78 call sites, no connection pool) | medium  | **ft-bhyxz** |
| 2 | Codec `serialize_with_mode(Auto)` double-serializes for large PDUs | low | **ft-gbpoy** |

## (1) Ingest / poll loop — well-engineered

### `extract_delta` (`ingest.rs:1662`)

The delta-extraction algorithm is the literal hot path of the watcher
(runs once per pane per poll tick). It's well-optimized:

- **Fast path for pure append** (line 1671-1678): if `current.starts_with(previous)`
  and char-boundary checks pass, returns `current[previous.len()..]` — `O(N)`
  via `starts_with`, no overlap search needed.
- **Slow path uses memchr** (line 1703): SIMD-optimized first-char search
  inside a bounded suffix window, instead of byte-by-byte comparison.
- **Bounded overlap window** (line 1689): `overlap_size.min(previous.len()).min(current.len())`
  — caps the search to a config-limited prefix of `current`, avoiding
  O(N²) worst case.
- **UTF-8 char-boundary checks** (line 1693-1695, 1705-1707, 1711)
  prevent panics on multi-byte characters (Cyrillic 2B, box-drawing 3B,
  emoji 4B).

**No new bead.** The single allocation per delta (`current[overlap_len..].to_string()`
at line 1727) is unavoidable for the API shape.

### `mark_next_event_for_pane_overflow` (`ingest.rs:2659`)

Linear scan over `self.buffer` (a `VecDeque<StreamEvent>`) for the
first event matching a pane_id. Buffer is bounded by `config.capacity`
(line 2579: `VecDeque::with_capacity(config.capacity)`). At default
capacity (~1000), the linear scan runs in microseconds — not a finding
unless the buffer cap goes to 100k+.

### Maintenance loops (`runtime.rs:1492, 1644, 2191, 2541, 2843, 2993, 3551`)

Multi-second-paced ticks (snapshot trigger bridge, maintenance, etc.).
Not in the byte-level hot path. Skipped.

## (2) FTS5 query path — connection pool gap

### `search_with_results_with_cx` (`storage.rs:9480`)

The Cx-first FTS search entry point:

```rust
pub async fn search_with_results_with_cx(...) -> Result<Vec<SearchResult>> {
    cx.checkpoint().map_err(...)?;
    let db_path = Arc::clone(&self.db_path);
    let query = query.to_string();
    Self::spawn_blocking_storage_with_join_error("Task join error", move || {
        let conn = open_read_storage_conn(db_path.as_str())?;   // <-- per-query open
        search_fts_with_snippets(&conn, &query, &options)
    }).await
}
```

`open_read_storage_conn` (`storage.rs:13620`):

```rust
fn open_read_storage_conn(db_path: &str) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Ok(conn)
}
```

**78 call sites of `open_read_storage_conn`** in `storage.rs` alone
(`grep -c open_read_storage_conn → 78`). Every search, every saved-search
fetch, every embedding read, every MCP read tool, every web `/search`
endpoint pays the full SQLite open cost (file open + page cache
warmup + WAL setup + busy-timeout pragma).

For a single-agent workload this is invisible. For the README's stated
"200+ concurrent AI coding agents" with all of them potentially
running `wa.search` / `ft robot search` simultaneously, this is
hundreds of SQLite-open syscalls per second.

**Filed:** `ft-bhyxz` (P2 perf) — introduce a read-side connection pool
(`Mutex<Vec<Connection>>` LIFO, sized to e.g. 4-16 pre-warmed
read-only connections; or pull in `r2d2_sqlite`). Keep
`open_read_storage_conn` as the cold-path fallback.

### `search_fts_with_snippets` (`storage.rs:15638`)

The actual FTS5 query construction. Not deep-audited in this pass —
the connection-pool finding above dominates. The query path itself
relies on rusqlite's `prepare_cached` for query plan caching (verified
via `grep prepare_cached storage.rs → 2 hits`). A second pass should
verify that `search_fts_with_snippets` uses `prepare_cached` and not
`prepare`.

### Embedding store (`storage.rs:9505`)

Same per-query Connection::open pattern in `store_embedding_with_cx` /
`store_embedding`. Rolled into ft-bhyxz scope.

## (3) Pattern detection — well-engineered

### `PatternEngine::detect` (`patterns.rs:2271`)

Profiled hot path with proper short-circuits:

1. **Empty-text fast exit** (line 2274).
2. **Quick-reject pre-filter** (line 2280): cheap byte-level check
   before the Aho-Corasick scan; skips ~80% of irrelevant chunks per
   the existing telemetry counter (`quick_rejects`).
3. **Aho-Corasick anchor matcher** (line 2286-2291): collects
   candidate rules in O(N) with sub-linear amortized constants.
4. **Per-candidate regex evaluation** (line 2310+): only the regex-bearing
   rules whose anchors matched are evaluated.
5. **Telemetry counters everywhere**: `scans_total`, `quick_rejects`,
   `candidate_rules_evaluated`, `regex_evaluations` — operators can
   measure hot-path cost without instrumenting.

The single concern: line 2295 `let match_count = matcher.find_overlapping_iter(text).count();`
runs an extra scan per detect — but it's gated by `#[cfg(test)]` and
only fires in test builds. Acceptable.

**No new bead.**

### `TriggerScanner::scan_counts` / `scan_locate` (`pattern_trigger.rs:408, 423`)

Both iterate via `for_each_leftmost_match` callback — no per-byte
allocation, results assembled into pre-allocated `HashMap`/`Vec`. Clean.

## (4) Codec PDU encode/decode — double-serialize on Auto path

### `serialize_with_mode` (`frankenterm/codec/src/lib.rs:467`)

```rust
fn serialize_with_mode<T: serde::Serialize>(
    t: &T,
    compression_mode: CompressionMode,
) -> Result<(Vec<u8>, bool), Error> {
    let mut uncompressed = Vec::new();
    let mut encode = varbincode::Serializer::new(&mut uncompressed);
    t.serialize(&mut encode)?;                       // <-- serialize #1

    if compression_mode == CompressionMode::Never { return Ok((uncompressed, false)); }
    if compression_mode == CompressionMode::Auto && uncompressed.len() <= COMPRESS_THRESH {
        return Ok((uncompressed, false));
    }
    // It's a little heavy; let's try compressing it
    let mut compressed = Vec::new();
    let mut compress = zstd::Encoder::new(&mut compressed, zstd::DEFAULT_COMPRESSION_LEVEL)?;
    let mut encode = varbincode::Serializer::new(&mut compress);
    t.serialize(&mut encode)?;                       // <-- serialize #2
    compress.finish()?;
    ...
}
```

When the PDU is large enough to compress, the value is **serialized
twice** — once into `uncompressed` for the size check, then a second
time *through* the zstd encoder. The second serialize is wasteful: the
already-encoded `uncompressed` byte buffer could be fed straight into
`zstd::stream::encode_all(&uncompressed[..])`.

For a small PDU (`<= COMPRESS_THRESH`) this is irrelevant — only one
serialize fires. For large PDUs (mux client snapshots, render-changes,
distributed envelopes), the cost doubles.

**Filed:** `ft-gbpoy` (P3 perf) — replace the second serialize with
direct buffer compression: `zstd::bulk::compress(&uncompressed,
zstd::DEFAULT_COMPRESSION_LEVEL)`. Same wire output, half the
serialize work above the threshold.

Side note on `Vec::new()` (lines 471, 483): both buffers grow from
zero capacity. A small win would be `Vec::with_capacity(64)` for
`uncompressed` and `Vec::with_capacity(uncompressed.len() / 2)` for
`compressed` (zstd typically halves binary data). Not its own bead;
fold into ft-gbpoy if interesting.

### `deserialize` (`lib.rs:506`)

Clean. Bounded-read via `r.take((MAX_PDU_SIZE as u64) + 1)` prevents
unbounded allocation on hostile input. Both compressed and uncompressed
paths go through the same bounded-varbincode call.

## Architectural observations (not findings)

### The mux interop boundary is subprocess-cost

`crates/frankenterm-core/src/wezterm.rs:2140` — every CLI call
(`run_cli` / `run_cli_with_cx`) spawns `wezterm` as a subprocess via
`Command::new(wezterm_binary())`. Process spawn on macOS costs 5-20ms.
For `list_panes`, the cost is amortized via the time-windowed cache at
`wezterm.rs:885` (`list_panes_cache: Arc<Mutex<Option<(Instant, Vec<PaneInfo>)>>>`).
For `get_text`, every call pays the full subprocess cost — by design,
because pane content varies per-call.

This is the documented WezTerm-fork mux boundary (per
`docs/proposals/ft-zoxxq-mux-boundary-truth.md`). The architectural
direction (in-process mux session API via `MuxInterface`) is the long-
term mitigation. Not its own perf bead — the cost is intentional and
already captured in the architecture epic.

### Pane registry and cursor RwLocks

`runtime.rs` and `tailer.rs` heavily exercise `registry: Arc<RwLock<...>>`
and `cursors: Arc<RwLock<HashMap<PaneId, PaneCursor>>>`. The
deadlock-audit (`docs/review/deadlock-audit.md` at HEAD ed91ef1e)
verified consistent lock ordering and properly-scoped guards. Not a
perf concern — these are reader-heavy workloads where `RwLock` is the
right choice.

## Caveats

- **Bench numbers not gathered.** This is a pattern-spotting audit, not
  a `criterion` run. The 78 `open_read_storage_conn` call sites is a
  *count*, not a microsecond figure. Beads include the structural fix
  and the recommended verification (criterion bench before/after).
- **Allocation profiling not done.** A real `dhat` / heaptrack pass
  would surface allocations the rg-based sweep misses (e.g. the
  `String::new()` growth pattern in many `format!` calls).
- **`mcp_tools.rs` not deeply audited.** It's the largest file in the
  workspace (8484 LOC). A targeted MCP perf audit deserves its own bead
  — most likely under the same connection-pool finding (every MCP read
  tool that calls `StorageHandle::*` pays the per-query Connection::open
  cost identified in §2).

## Conclusion

Two beads filed (`ft-bhyxz`, `ft-gbpoy`); the rest of the audited
surface is clean. The codebase shows clear evidence of perf-aware
engineering — Aho-Corasick + quick-reject, memchr SIMD, bounded
overlap, time-windowed CLI cache, telemetry counters everywhere — and
the two findings are *foundational* (storage connection pool) and
*tactical* (codec serialize-once instead of twice) rather than systemic.

Anything found by a real `criterion` / `dhat` pass should grow its
own bead.
