# Deadlock / Concurrency Audit (review pass)

**Scope:** `crates/frankenterm-core/src/` — focus on async lock acquisitions
that could hold a guard across an `.await`, cyclic mutex orderings between
the heavy concurrent state objects (registry, cursors, panes, pane_routes),
and bounded-channel saturation patterns.
**Date:** 2026-04-26
**Method:** ripgrep for `\.lock\(\)\.await|\.write\(\)\.await|\.read\(\)\.await`
across the production tree, plus targeted reads of cross-lock orderings,
plus sweep of `std::sync::Mutex` (sync mutex held across `.await` is
*more* dangerous than `tokio::sync::Mutex`).

## Tally

**159 async lock acquisitions** across 12 files in `frankenterm-core/src/`:

| File                                          | hits |
| --------------------------------------------- | ---: |
| `runtime.rs`                                  |   51 |
| `runtime_async.rs`                            |   45 |
| `wezterm.rs`                                  |   20 |
| `tailer.rs`                                   |   13 |
| `sharding.rs`                                 |    9 |
| `recorder_storage.rs`                         |    6 |
| `ipc.rs`                                      |    6 |
| `vendored/mux_client.rs`                      |    3 |
| `snapshot_engine.rs`                          |    2 |
| `workflows/builtin_workflows.rs`              |    2 |
| `metrics.rs`                                  |    1 |
| `notifications.rs`                            |    1 |

**Plus 37 `std::sync::Mutex` usages** — concentrated in `cancellation.rs`
(8 references) and `runtime_async.rs` (5 references in the `JoinHandle`
abort-waker plumbing).

## Findings — none that warrant new beads

### (A) All production lock acquisitions are properly scoped

Every `lock().await` / `read().await` / `write().await` in **production
code paths** (i.e. outside `#[cfg(test)]`) was either:

1. **Inline expression temporary** — guard drops at the trailing `;`
   or at the end of the conditional expression. Examples:
   - `sharding.rs:841`: `if let Some(route) = self.pane_routes.read().await.get(&pane_id).copied() { return Ok(route); }`
   - `wezterm.rs:6832`: `self.panes.read().await.len()`
2. **Block-scoped** — guard explicitly bound inside `{ ... }`, no `.await`
   inside the block. Examples:
   - `runtime.rs:4169-4175`: read guard collects pane cursors, drops at `}`,
     then `storage.shutdown().await` runs without the guard.
   - `tailer.rs:973-984`: read guard for `registry`, then write guard for
     `cursors`, each in its own block, no awaits inside.
   - `ipc.rs:1611-1626`: write guard for registry inside `let installed = { ... };`
     block, no awaits inside.

I scanned all 12 files for the dangerous "lock then await without
explicit drop" pattern via:

```bash
awk '/\.(lock|read|write)\(\)\.await/{lockN=NR; lockL=$0} \
     lockN && NR>lockN && NR<=lockN+10 && /\.await/ \
     && !/\.(lock|read|write)\(\)\.await/ \
     {print "@"lockN"->"NR": "lockL" || "$0; lockN=0}'
```

Every hit landed in a `#[cfg(test)] mod tests { … }` block (e.g.
`runtime.rs:8246` writer-loop test, `runtime_async.rs:3876` test of
mutex contention with explicit `drop(guard)` before the inter-iteration
sleep). Test code is allowed to demonstrate the pattern.

### (B) Cross-lock orderings between `registry` and `cursors` are consistent

`registry` and `cursors` are the two most-frequently-paired async locks
in the codebase (both `RwLock<HashMap<PaneId, ...>>`). I extracted every
case where both are acquired in the same control-flow path:

| Site                  | Order                          |
| --------------------- | ------------------------------ |
| `runtime.rs:1744→1753` | registry.read() → cursors.write() |
| `runtime.rs:1886→1887` | registry.read() → cursors.read()  |
| `runtime.rs:2008→2009` | registry.read() → cursors.read()  |
| `runtime.rs:2396→2404` | registry.read() → cursors.write() |
| `runtime.rs:4220→4221` | registry.read() → cursors.read()  |
| `tailer.rs:975→980`    | registry.read() → cursors.write() |

Reverse-order check (cursors first → registry) returned zero hits.
Lock-order graph is acyclic for the dominant pair. ✓

### (C) `std::sync::Mutex` usages keep critical sections sync-only

`cancellation.rs` uses `std::sync::Mutex<Option<ShutdownReason>>` for
`reason` and `std::sync::Mutex<Vec<Arc<...>>>` for `children` (lines
308-310, 7 lock acquisitions throughout the file). Inspected:

- `child()` (line 332): acquires `parent.children`, then conditionally
  acquires `child.reason` on a *different* node. Both critical sections
  hold sync state only — no `.await` calls inside.
- `propagate_inner()` (line 395): acquires `inner.reason` (line 401),
  drops at `;`, then acquires `inner.children` to `.clone()` (line 406),
  releases, then iterates over the cloned vec calling
  `propagate_inner(child, …)` recursively *outside* the lock. Lock-free
  recursion. ✓
- `cancel()`, `reason()`, `child_count()`, `prune_dead_children()` —
  all hold sync mutex briefly for plain reads/writes, no awaits.

`runtime_async.rs` uses `std::sync::Mutex<Option<Waker>>` for the
`abort_waker` slot in the `JoinHandle` plumbing. Critical sections are
single-line `take()`/`replace()` operations — no awaits. ✓

### (D) Test-only mutex-contention patterns

Several files exercise lock-await patterns deliberately as part of
asupersync runtime correctness tests:

- `runtime_async.rs:3828, 3876, 3893` — RwLock and Mutex stress tests.
- `runtime.rs:8246` — writer task in a multi-reader/writer integration
  test. Uses explicit `drop(guard)` before the inter-iteration sleep.
- `vendored/mux_client.rs:4108, 7848, 7967` — concurrent client request
  fan-out tests (test scope confirmed: `#[cfg(test)]` at line 2217 of a
  9367-line file; all three locations are in the test mod).

These are by design — testing that the runtime makes progress under
contention. Acceptable.

## Things I did NOT find

- **No `lock().await` followed by another `.await` while the guard is
  still in scope** in production paths.
- **No reverse-order lock acquisitions** for the registry/cursors pair.
- **No std::sync::Mutex held across `.await`** — every sync-mutex
  critical section in the audited tree is sync-work-only.
- **No bounded-channel `send().await` patterns inside locks** that
  could deadlock when the receiver is also holding the same lock.
- **No global static `Mutex<T>` lock-then-await patterns** other than
  `RATE_LIMIT_TRACKER.lock().await` in `workflows/builtin_workflows.rs`,
  whose critical section is sync-only (record/gc/provider_status all
  return values, no `.await` on the guard scope).

## Methodological caveats

This audit looked at **lock acquisitions** explicitly. It did NOT
exhaustively cover:

- **Channel saturation** (`mpsc::Sender::send().await` blocking on full
  channel while the receiver is blocked on something the sender holds).
  Spot-checked the major channel pairs in `tailer.rs` and `runtime.rs`
  and they look fine — receivers run in their own tasks.
- **Self-deadlock via reentrant call** through callback/trait-object
  indirection. Hard to grep; would need a control-flow tool.
- **Priority inversion** under the asupersync scheduler — out of scope
  for a code-pattern audit.

Anything found there should grow its own bead.

## Conclusion

Zero new beads filed. The async-lock surface in `frankenterm-core` is
**clean and consistently scoped**. The `#![forbid(unsafe_code)]` invariant
plus the convention of explicit block-scoping for guards has held up
across the recent extractions and the existing tree. The few sync-mutex
locations (`cancellation.rs`, `runtime_async.rs`) follow the
sync-critical-section discipline.

This is the kind of audit that's nicest when it returns nothing.
