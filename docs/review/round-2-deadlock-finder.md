# Round-2 Deadlock-Finder Sweep — Saturation

**Scope:** repeat the round-1 deadlock/concurrency audit
(`docs/review/deadlock-audit.md`, HEAD ed91ef1e) against the
post-rename + post-extraction codebase. Same rg patterns, same scope.
**Date:** 2026-04-26
**Verdict:** **SATURATED** — zero new findings, identical lock-await
inventory.

## Sweep results

```bash
rg -E '\.(lock|read|write)\(\)\.await' crates/frankenterm-core/src \
   crates/frankenterm-core-*/src
```

**Total: 159 hits** — identical to round 1.

| File                                      | Round 1 | Round 2 |
| ----------------------------------------- | ------: | ------: |
| `runtime.rs`                              | 51      | 51      |
| `runtime_async.rs` (was `runtime_compat`) | 45      | 45      |
| `wezterm.rs`                              | 20      | 20      |
| `tailer.rs`                               | 13      | 13      |
| `sharding.rs`                             | 9       | 9       |
| `recorder_storage.rs`                     | 6       | 6       |
| `ipc.rs`                                  | 6       | 6       |
| `vendored/mux_client.rs`                  | 3       | 3       |
| `workflows/builtin_workflows.rs`          | 2       | 2       |
| `snapshot_engine.rs`                      | 2       | 2       |
| `notifications.rs`                        | 1       | 1       |
| `metrics.rs`                              | 1       | 1       |
| **All 10 new sub-crates** (`*-types`, `tantivy`, `ars`, `fleet`, `replay`) | 0 | **0** |

The runtime_compat → runtime_async rename touched 615 occurrences in
tests + similar in production source, but those were all *imports*
(`use crate::runtime_compat::Mutex` → `use crate::runtime_async::Mutex`).
Lock-acquisition method calls (`.lock()`, `.read()`, `.write()`) are
unchanged in count and shape.

The 10 new sub-crates contain ZERO `lock().await` / `read().await` /
`write().await` patterns. The leaf-types crates use `serde` derives
+ pure type definitions; the cluster crates (tantivy, ars, fleet,
replay) inherit their async surfaces from `frankenterm-core` via
the path-dep edge but don't add new async-mutex usage of their own.

## Verification

The 7 lock-then-await pairs caught in round 1 (within 10 lines of
each other) are still present at the SAME line ranges (modulo small
shifts from the rename diff). All were verified properly scoped:

- `runtime.rs:1997-2006` — read guard inside expression block, drops
  before subsequent `.await`. ✓
- `runtime.rs:4170-4179` — read guard in explicit block, drops at `}`. ✓
- `runtime.rs:5916-5921` — test-only. ✓
- `runtime.rs:8224-8229` — test-only writer-task with explicit
  `drop(guard)` before sleep. ✓
- `runtime.rs:8246-8254` — same writer-task pattern. ✓
- `sharding.rs` two pairs — temporary read guard + temporary write
  guard, each scoped to a single expression with `.copied()` /
  `.remove()`. ✓

Cross-lock orderings (registry → cursors) — the dominant pair —
remain consistent. No reverse-order acquisitions.

## Comparison to round 1

| Category | Round 1 | Round 2 | Delta |
| --- | ---: | ---: | ---: |
| Lock-await total | 159 | 159 | 0 |
| Files containing lock-await | 12 | 12 | 0 |
| New sub-crate lock-await | n/a | 0 | confirmed clean |
| Production lock-then-await spans | 0 | 0 | 0 |
| Test-only lock-then-await | 5 | 5 | 0 |
| `std::sync::Mutex` held across `.await` | 0 | 0 | 0 |
| Cross-lock cyclic orderings | 0 | 0 | 0 |
| Genuine concurrency bugs | 0 | 0 | 0 |
| **New beads filed** | — | **0** | saturated |

## Saturation accounting

**Round 2 of 3** for the deadlock-finder rotation. Per-rotation cadence
preserved (~20-min sweep, doc shipped, no new beads).

The async-lock surface is structurally stable. The `runtime_compat →
runtime_async` rename did not perturb lock semantics (only import
paths). The 10 sub-crate extractions did not introduce new
async-mutex usage. Round 1's "this is the kind of audit that's nicest
when it returns nothing" verdict still holds.
