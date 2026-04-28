# storage.rs Phase 1 — corrected scope (ft-dzats)

## What shipped (Phase 1a, commit 35fa605c5)

Three self-contained test modules extracted from `storage.rs` into
sibling files under `crates/frankenterm-core/src/storage/`:

| Module | New file | Lines | Tests |
|--------|----------|------:|------:|
| `queue_depth_tests` | `storage/queue_depth_tests.rs` | 254 | 6 |
| `backpressure_integration_tests` | `storage/backpressure_integration_tests.rs` | 308 | 6 |
| `proptest_tests` | `storage/proptest_tests.rs` | 216 | 3 |

Net: storage.rs went from **34,264 → 33,493 lines** (-771).
All 15 extracted tests pass:
`cargo test -p frankenterm-core --lib --no-default-features storage::queue_depth_tests` (6 passed),
`storage::backpressure_integration_tests` (6 passed),
`storage::proptest_tests` (3 passed).

The pattern works cleanly:
- `#[cfg(test)] mod foo;` declared in `storage.rs` resolves to
  `storage/foo.rs` (the same Rust path-resolution rule that the
  long-standing `pub mod mmap_store;` already uses).
- Inside each extracted file, `use super::*;` resolves to
  `crate::storage::*` because the file IS the mod (not a wrapper)
  — no path rewrites, no `super::super`, no visibility bumps.
- Each extracted mod has its own copy of the `run_async_test`
  helper, so they're truly self-contained.

## Why the rest didn't ship in this hour

The original ft-dn2tu proposal estimated "~11.4K lines of test
mod blocks." Audit at HEAD shows the real shape is more
complicated:

### Actual breakdown (lines 19584-34264, ~14,680 lines of test code)

| Block | Lines | Range | Shape |
|-------|------:|-------|-------|
| `mod tests` (the original one) | 3,158 | 19584-22741 | clean mod, but the largest |
| FTS Search & Async StorageHandle flat tests | 3,648 | 22742-26389 | **flat `#[test]` fns at module level + a `run_async_test` helper at line 23618 with 101 callers across the file** |
| `mod db_check_repair_tests` | 390 | 26390-26779 | clean mod |
| `mod fts_sync_tests` | 993 | 26780-27772 | clean mod |
| `mod timeline_tests` | 322 | 27773-28094 | clean mod |
| `mod storage_handle_tests` | 1,888 | 28095-29982 | clean mod |
| **`mod queue_depth_tests`** | **253** | **29983-30235** | **shipped Phase 1a** |
| **`mod backpressure_integration_tests`** | **308** | **30241-30548** | **shipped Phase 1a** |
| **`mod proptest_tests`** | **216** | **30554-30769** | **shipped Phase 1a** |
| `mod accounts_db_tests` | 285 | 30775-31059 | clean mod |
| `mod reservation_tests` | 454 | 31060-31513 | clean mod |
| `mod timeline_correlation_tests` | 1,888 | 31514-33401 | clean mod |
| `mod timeline_integration_tests` | 862 | 33402-34264 | clean mod |

### The flat-test region is the load-bearing blocker

Lines 22742-26389 are NOT inside a `mod` block. They're a
sequence of top-level `#[test]` functions at the module level
of `crate::storage`, plus a file-level `#[cfg(test)] fn
run_async_test<F>(...)` helper at line 23618 that 101 call
sites across the whole file depend on (the helper is also
shadowed by per-mod copies inside several mod blocks below).

Moving this region requires either:

1. **Wrap then move.** Wrap the 3,648-line region in a new
   `#[cfg(test)] mod fts_async_flat_tests { use super::*; ... }`
   block first (in-place rename), then extract that wrapper as
   its own file. Two commits.
2. **Extract `run_async_test` to a shared test-helper file
   first.** Then move the flat tests into their own wrapper.
   The shared helper would live at
   `storage/test_support.rs` and be imported by every consuming
   mod. Touches every callsite inside the cleanly-bounded mods
   that have their own duplicate copy — the duplicates can be
   removed, but each removal is a verification cycle.

Neither is a "1-hour mechanical move." Both reshape the call
graph that the 101 references rely on.

## Recommended remaining phases

Each becomes its own follow-on bead (Phase 1a was the validation
that the per-file pattern works).

### Phase 1b — clean-mod harvest (LOW risk)

Move all 9 remaining clean `mod xxx_tests { … }` blocks into
sibling files using the exact pattern shipped in Phase 1a. Total
~10,440 lines moved, ~10,440 fewer lines in storage.rs.

Estimated effort: 1-2 hours (per-mod extraction is mechanical,
each mod's test sweep takes 60-90s on the local target).

Order to ship (small → large):
1. `mod accounts_db_tests` (285 lines)
2. `mod timeline_tests` (322 lines)
3. `mod db_check_repair_tests` (390 lines)
4. `mod reservation_tests` (454 lines)
5. `mod timeline_integration_tests` (862 lines)
6. `mod fts_sync_tests` (993 lines)
7. `mod timeline_correlation_tests` (1,888 lines)
8. `mod storage_handle_tests` (1,888 lines)
9. `mod tests` (the original 3,158-line one, hardest because
   the name `tests` is already conventionally used and would
   need a rename like `policy_decision_tests` to reflect content)

### Phase 1c — flat-test region (HIGH risk)

Wrap lines 22742-26389 in a new `mod fts_async_flat_tests` block
(in-place rename, no extraction yet). Verify all 101 callers of
`run_async_test` still compile (they will — `super::*` continues
to resolve correctly while the helper stays at file scope).
Then extract the wrapped mod to its own file.

Estimated effort: 2-3 hours, including the verification cycle on
the 3,648 lines.

### Phase 1d — `run_async_test` consolidation (OPTIONAL)

Several mods carry their own duplicate copy of `run_async_test`
(I saw at least 3 during Phase 1a). After Phase 1c, lift the
single canonical copy into `storage/test_support.rs` and remove
the duplicates.

Estimated effort: 1 hour, mostly verification.

## Net projection

| Phase | storage.rs after | Files added |
|-------|-----------------:|------------:|
| HEAD before ft-dzats | 34,264 | — |
| **Phase 1a (shipped)** | **33,493** | **3** |
| Phase 1b | ~23,053 | +9 |
| Phase 1c | ~19,405 | +1 |
| Phase 1d | ~19,405 | +1 (test_support.rs) |

Phase 1c is the single biggest reduction. Phase 1a pinned the
extraction pattern; everything else mechanical.

## Why ship the corrected scope as a doc (not just a bead comment)

The original ft-dn2tu proposal had the wrong line counts and
missed the flat-test region entirely. A future agent picking up
Phase 1b-d needs the corrected breakdown, the per-mod ranges,
and the rationale for the recommended order — that doesn't fit
in a bead's `--reason` field. This file is the load-bearing
input for the follow-on bead.

## References

- Phase 1a commit: `35fa605c5` (linter-co-committed).
- Original proposal: `docs/proposals/ft-dn2tu-storage-split.md`.
- ft-dn2tu (parent epic, closed).
- Pattern proven by `pub mod mmap_store;` resolving to
  `storage/mmap_store.rs` since well before this work.
