# Performance Hot-Spot Audit (historical static pass + current proof gaps)

**Scope:** four hot paths called out by the user — (1) ingest/poll loop
in `runtime.rs` + `ingest.rs`, (2) FTS5 query path in `storage.rs` +
`search/`, (3) pattern detection in `patterns.rs` + `pattern_trigger.rs`,
(4) codec PDU encode/decode in `frankenterm/codec`.
**Method:** ripgrep for known hot patterns (`O(n²)` loops, hot
allocations in tight scopes, sync calls in async contexts, lock
contention, redundant clones), then targeted reads of the entry points
the user named.
**Original static pass:** 2026-04-26<br>
**Truth-status refresh:** 2026-08-06

## TL;DR

The 2026-04 static pass found two structural opportunities, both subsequently
implemented. It did not profile a live GUI/mux session and cannot establish
keypress latency, resize/zoom responsiveness, large-session behavior, or
target-machine qualification. In particular, there is no completed-result TTL
cache for full `list_panes` metadata: such a cache was removed because focus,
cursor, CWD, title, viewport, and zoom state are volatile.

| # | Historical finding | Current structural status | Bead |
| - | - | - | - |
| 1 | Storage read-path opened a fresh SQLite connection per query | Read-side pooling landed; live contention and tail latency remain unmeasured | **ft-bhyxz** |
| 2 | Codec `serialize_with_mode(Auto)` serialized large PDUs twice | Serialize-once/direct-buffer compression landed; end-to-end transport gain remains unmeasured | **ft-gbpoy** |

## (1) Ingest / poll loop — bounded structure, live cost unprofiled

### `extract_delta` (`ingest.rs:1662`)

The delta-extraction algorithm runs once per pane per poll tick. The original
static review found these bounded/fast-path structures:

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

**No finding in that static pass.** The returned owned delta still allocates;
calling that allocation unavoidable would require a current caller/API and
allocation-profile study that this review did not perform.

### `mark_next_event_for_pane_overflow` (`ingest.rs:2659`)

Linear scan over `self.buffer` (a `VecDeque<StreamEvent>`) for the
first event matching a pane_id. Buffer work is bounded by configured capacity.
No retained measurement in this review justifies a microsecond cost claim,
especially at large capacity or under aged-session pressure.

### Maintenance loops (`runtime.rs:1492, 1644, 2191, 2541, 2843, 2993, 3551`)

Multi-second-paced ticks (snapshot trigger bridge, maintenance, etc.).
Not in the byte-level hot path. Skipped.

## (2) FTS5 query path — historical connection-pool gap

### `search_with_results_with_cx` (`storage.rs:9480`)

The Cx-first FTS search entry point at the original review revision is shown
below. This is a pre-fix historical excerpt, not current source:

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

The historical scan counted **78 textual call sites of
`open_read_storage_conn`** in `storage.rs` alone
(`grep -c open_read_storage_conn → 78`). At that revision, the reviewed search,
saved-search, embedding, MCP-read, and web-search paths opened SQLite
connections rather than acquiring the later pool.

The review hypothesized that this would amplify under concurrent search, but it
did not measure either single-agent cost or a 200-pane workload. No throughput
or syscall-rate number should be inferred from the textual count.

**Filed at the time:** `ft-bhyxz` (P2 perf) — introduce a read-side connection pool
(`Mutex<Vec<Connection>>` LIFO, sized to e.g. 4-16 pre-warmed
read-only connections; or pull in `r2d2_sqlite`). That structural fix later
landed. This source scan did not measure pool wait time, WAL contention, cache
residency, or search tail latency under a real large session.

### `search_fts_with_snippets` (`storage.rs:15638`)

The actual FTS5 query construction. Not deep-audited in this pass —
the connection-pool finding above dominated that pass. A historical two-hit
`prepare_cached` source scan did not establish which queries benefited or the
current statement-cache hit rate; those require fresh call tracing and
measurement.

### Embedding store (`storage.rs:9505`)

The original revision had the same per-query `Connection::open` pattern in
`store_embedding_with_cx` / `store_embedding`. It was rolled into ft-bhyxz.

## (3) Pattern detection — promising structure, workload cost unprofiled

### `PatternEngine::detect` (`patterns.rs:2271`) and the live contextual path

The static review identified these short-circuits:

1. **Empty-text fast exit** (line 2274).
2. **Quick-reject pre-filter** (line 2280): cheap byte-level check
   before the Aho-Corasick scan. The `quick_rejects` counter makes the
   workload-specific rejection rate measurable; this audit retained no rate.
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

**No new bead in the historical static pass.**

That historical review did not cover the actual production call shape:
runtime orchestration calls `PatternEngine::detect_with_context`, which adds
tail copying/materialization, overlap filtering, agent sharding, and dedupe
work around the core detector. No retained profile isolates those costs under
large ongoing sessions, so the `detect` source observations do not qualify the
live pattern hot path.

### `TriggerScanner::scan_counts` / `scan_locate` (`pattern_trigger.rs:408, 423`)

Both iterate via `for_each_leftmost_match` callback — no per-byte allocation
was apparent in the reviewed body, and results were assembled into
pre-allocated `HashMap`/`Vec` values. This is a structural observation, not an
allocation-profile result.

## (4) Codec PDU encode/decode — historical double-serialize on Auto path

### `serialize_with_mode` (`frankenterm/codec/src/lib.rs:467`)

This is the pre-fix historical excerpt that motivated `ft-gbpoy`, not current
source:

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

For a small PDU (`<= COMPRESS_THRESH`), only one serialize fired. Above the
threshold, serialization ran twice; this review did not measure the resulting
end-to-end cost.

**Filed at the time:** `ft-gbpoy` (P3 perf) — replace the second serialize with
direct buffer compression: `zstd::bulk::compress(&uncompressed,
zstd::DEFAULT_COMPRESSION_LEVEL)`. The landed implementation uses direct
buffer compression. “Half the work” was a hypothesis about the serialization
stage, not an end-to-end latency result.

The original note also observed zero-capacity `Vec` construction. It retained
no allocation profile or representative compression-ratio distribution, so no
capacity recommendation or win is asserted here.

### `deserialize` (`lib.rs:506`)

The reviewed bounded-read guard used
`r.take((MAX_PDU_SIZE as u64) + 1)`. Both compressed and uncompressed paths
went through the bounded-varbincode call in that revision. This is a size-bound
observation, not a deserialization throughput or peak-RSS result.

## Architectural observations (not findings)

### The mux interop boundary has direct and fallback transport costs

`WeztermClient` prefers the existing pooled direct mux socket path. Eligible
compatibility fallback calls spawn the configured backend CLI as a subprocess.
This audit did not retain a transport-choice distribution, direct-socket
latency distribution, or native spawn distribution, so it cannot identify
which transport dominates current user-visible latency.
For `list_panes`, completed full-metadata results are deliberately not cached
by TTL: `PaneInfo` carries volatile focus, cursor, cwd, title, viewport, and
zoom state, and elapsed time is not an authority revision. Concurrent CLI-only
callers can therefore still duplicate subprocess work. The required follow-up
is cross-client, cross-transport singleflight tied to an authoritative mux
revision/event stream. `get_text` likewise remains per-call because pane
content varies per call.

This is the documented WezTerm-fork mux boundary (per
`docs/proposals/ft-zoxxq-mux-boundary-truth.md`). `MuxInterface` and the direct
pooled transport already exist; they are not a future mitigation. Remaining
work is to measure and optimize the live direct path, make fallback frequency
observable, and prevent duplicate cross-client work using authoritative mux
revision/event evidence.

### Pane registry and cursor RwLocks

`runtime.rs` and `tailer.rs` heavily exercise `registry: Arc<RwLock<...>>`
and `cursors: Arc<RwLock<HashMap<PaneId, PaneCursor>>>`. The
deadlock-audit (`docs/review/deadlock-audit.md` at HEAD ed91ef1e)
verified consistent lock ordering and properly-scoped guards in its scope, but
that does not prove low contention. Reader/writer wait distributions,
cache-line traffic, and scheduler behavior remain profiling questions.

## Negative-evidence ledger

| Desired conclusion | Negative evidence / missing proof | Required retry |
|---|---|---|
| This audit found the dominant live latency bottleneck | Static source inspection only; no Instruments, signposts, sampling profile, allocation profile, transport trace, or presented-frame timestamps | Profile the exact release build under a declared live workload and retain stacks plus stage timestamps |
| Full-pane metadata can be cached by elapsed time | The prior TTL cache was removed because the payload is volatile and elapsed time is not an authority revision | Restrict any future coalescing to one in-flight authoritative revision across clients/transports; never reuse a completed stale payload by TTL |
| Mac-to-LAN keypress response is fast | No end-to-end input → transport → PTY → renderer → presentation trace | Measure percentiles on the declared LAN peers with synchronized stage provenance |
| Resize/zoom is both fast and visually correct | No retained native GUI frame-pacing plus reflow/visual-differential artifact | Couple timing, dropped frames, CPU/GPU cost, and image/reflow correctness in one run |
| M4/M5 and Threadripper PRO 5995WX are qualified | No non-skipped end-to-end interactive qualification artifact on the named Apple generations or the 64-core/128-thread `trj` host | Run and retain the full matrix separately on each named CPU/SoC and document topology/affinity/config |
| Large ongoing sessions remain responsive | No 4h/24h/72h aged-session soak or post-parse RSS proof | Retain bounded-load soaks with tail latency, memory attribution, cancellation, and recovery evidence |

## Caveats

- **Bench numbers not gathered.** This is a pattern-spotting audit, not
  a `criterion` run. The 78 `open_read_storage_conn` call sites is a
  *count*, not a microsecond figure. Beads include the structural fix
  and the recommended verification (criterion bench before/after).
- **Allocation profiling not done.** A real `dhat` / heaptrack pass
  would surface allocations the rg-based sweep misses (e.g. the
  `String::new()` growth pattern in many `format!` calls).
- **`mcp_tools.rs` not deeply audited.** A targeted MCP perf audit deserves its
  own bead; its current storage and response-building paths require fresh
  source inspection and measurement rather than the old connection-open count.

## Conclusion

The two historical structural findings (`ft-bhyxz`, `ft-gbpoy`) landed, but
this audit does not establish that the rest of the surface is clean or that the
performance campaign is saturated. Aho-Corasick prefiltering, bounded overlap,
read pooling, and serialize-once compression are useful substrate. They are not
substitutes for live critical-path profiles, native target-class runs,
resize/zoom quality evidence, or aged-session soaks. The full-pane TTL cache is
not present; revision-aware in-flight singleflight remains future work.
