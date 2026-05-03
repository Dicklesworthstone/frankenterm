# `with_writer_backend` overhead — bench report (br-ft-7yq2z)

## TL;DR

The `with_writer_backend(conn, |backend| { ... })` bridge introduced
at **bab94d08d** (br-ft-l1jgo writer-thread slice) costs **~11 µs per
call** on Apple Silicon with `:memory:` SQLite. Of that, **~8.6 µs is
the per-call `Connection::open_in_memory()` placeholder allocation**.
Caching the placeholder across calls drops the bridge tax to **~0.3 µs
above the pre-migration baseline** — a **~32× speedup of the bridge
overhead**, or equivalently ~9.3 µs reclaimed per WriteCommand
dispatch that goes through the bridge.

## Numbers

Measured via `cargo bench --bench writer_bridge_overhead -p
frankenterm-core --no-default-features -- --quick`.

| Bench leg | Median time | Notes |
|---|---:|---|
| `connection_open_in_memory_only` | **~8.6 µs** | Placeholder allocation alone (the suspected hot spot) |
| `baseline_direct_execute` | **~1.4 µs** | Pre-migration writer-thread path: `conn.execute(UPDATE …)` directly on a long-lived `Connection` |
| `with_writer_backend_dance` | **~11.0 µs** | Full wrap/unwrap dance with per-call placeholder allocation, exactly as `storage.rs::with_writer_backend` ships it |
| `with_writer_backend_dance_cached` | **~1.7 µs** | Same dance, but the placeholder is recycled across iterations via a swap-pair `mem::replace` |

**Bridge tax (un-cached):** `11.0 - 1.4 ≈ 9.6 µs` per WriteCommand.
**Bridge tax (cached):**    `1.7 - 1.4 ≈ 0.3 µs` per WriteCommand.

The placeholder allocation is essentially the entire bridge tax. The
rest of the dance (`mem::replace` × 2, `RusqliteBackend::new`,
`backend.into_connection()`) costs ~0.3 µs in aggregate.

## Decision

**Land the cached-placeholder optimization** (option 1 from ft-7yq2z's
fix-shape paragraph). The cached pattern needs:

1. A static `OnceLock<Mutex<Option<Connection>>>` (or equivalent
   per-thread storage) holding the placeholder.
2. A swap-pair `mem::replace` that pulls the live conn out, places
   the cached placeholder in, runs the closure, then a second
   `mem::replace` that pulls the live conn back and recovers the
   placeholder for the next call.
3. Initialization cost of one `Connection::open_in_memory()` per
   process lifetime (vs. one per WriteCommand today).

Option 2 from the fix-shape (restructure `writer_loop` to wrap the
conn ONCE for the loop's duration) is a bigger refactor and would
also reach the ~0.3 µs floor — but it requires migrating
`dispatch_write_command` to take `&dyn StorageBackend` instead of
`&mut Connection`, which means EVERY remaining `_sync` writer helper
has to migrate at the same time. That's the bead-aligned end-state
but not a single-slice change. The cached-placeholder optimization
is a strict subset that saves the bulk of the cost without forcing
the lockstep migration.

## Cost projection

- **Today:** 1 writer-thread `_sync` helper migrated
  (`mark_session_shutdown_clean_backend`). Bridge tax ≈ 9.6 µs ×
  ~1 call per `MarkSessionShutdownClean` command = negligible (this
  command fires once per session shutdown).
- **After ft-l1jgo writer-thread continuation:** ~30 `_sync` helpers
  migrated. The hot path is `WriteCommand::AppendSegment` (one per
  captured pane chunk; can fire 100s/sec under heavy ingest). At
  ~9.6 µs per call, the un-cached overhead would add **~1 ms per
  100 segments** — measurable but not catastrophic. Caching reclaims
  this entirely.

## Reproduction

```bash
RCH_DISABLE=1 \
CC=/opt/homebrew/opt/llvm/bin/clang \
CXX=/opt/homebrew/opt/llvm/bin/clang++ \
CARGO_TARGET_DIR=/tmp/ft-cc2-target \
cargo bench --bench writer_bridge_overhead \
  -p frankenterm-core --no-default-features -- --quick
```

The `--quick` flag runs criterion in fast mode (3-second sample
window per leg instead of the default 5 seconds × 100 samples) and
is sufficient for the 4 legs above to converge to stable medians.

## Cross-references

- `crates/frankenterm-core/benches/writer_bridge_overhead.rs` (the
  bench harness shipped under this bead).
- `crates/frankenterm-core/src/storage.rs::with_writer_backend`
  (substrate the bench measures, landed at bab94d08d).
- ft-l1jgo (parent epic — the writer-thread migration that drove
  the need for this bench).
- ft-7yq2z (this bead — bench + decision).
