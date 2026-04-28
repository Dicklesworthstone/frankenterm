# storage.rs library-vs-binary split (ft-dn2tu)

## Pathology

`crates/frankenterm-core/src/storage.rs` is a **34,097-line file**
with the topology of a small monolithic binary, not a library
module:

| Metric | Count |
|--------|------:|
| Total lines | 34,097 |
| Production code (non-test) | ~22,660 |
| Test code (`#[cfg(test)]`) | ~11,439 |
| `pub fn` exports | 21 |
| `pub struct` / `pub enum` declarations | ~102 |
| `impl` blocks | 46 |
| `#[cfg(test)]` mod blocks | 17 |
| Section header banners (`// ===…`) | 30+ |

The file already has 30+ banner-comment "sections" — i.e., the
author drew the boundaries; they just live inside one file.

The library-vs-binary tell is the asymmetry: only **21 pub fns**
across 22,660 production lines (one pub fn per ~1,080 lines), but
**102 pub types**. The file is mostly impl plumbing for a small
public surface — exactly the shape that wants to live in
submodules.

## Section map

Production sections (line ranges from current HEAD,
`storage.rs:59` onward):

| # | Section | Lines | Range | Contains pub fns? |
|---|---------|------:|-------|------------------|
| 1 | Schema Definition (DDL strings) | 612 | 59-670 | no |
| 2 | Schema Migrations (types + classifier) | 1,508 | 671-2178 | yes (2) |
| 3 | Data Structures | 442 | 2179-2620 | no |
| 4 | Timeline Data Model (wa-6sk.1) | 999 | 2621-3619 | no |
| 5 | Schema Initialization & Migrations | 116 | 3620-3735 | yes (1) |
| 6 | Database Health Check & Repair | 2,043 | 3736-5778 | yes (3) |
| 7 | Writer Command Types | 586 | 5779-6364 | no |
| 8 | Storage Handle (impl, the big one) | 5,724 | 6365-12088 | yes (2) |
| 9 | Writer Thread Implementation | 609 | 12089-12697 | no |
| 10 | Synchronous Database Operations | 1,376 | 12698-14073 | yes (2) |
| 11 | Pane Bookmarks | 34 | 14074-14107 | no |
| 12 | Session Checkpoint Sync | 299 | 14108-14406 | yes (1) |
| 13 | Usage Metrics Operations | 243 | 14407-14649 | no |
| 14 | Notification History | 98 | 14650-14747 | no |
| 15 | Cleanup engine | 295 | 14748-15042 | no |
| 16 | Account Operations (wa-nu4.1) | 160 | 15043-15202 | no |
| 17 | Pane Reservation Sync | 488 | 15203-15690 | no |
| 18 | Read Operations | 900 | 15691-16590 | no |
| 19 | Indexing Progress Tracking (wa-upg.5.2) | 126 | 16591-16716 | no |
| 20 | Incremental FTS Sync (wa-3g9.4) | 984 | 16717-17700 | yes (1) |
| 21 | Timeline Query (wa-6sk.1) | 1,330 | 17701-19030 | no |
| 22 | Segment Scan Query | 127 | 19031-19157 | no |
| 23 | Export Query | 3,501 | 19158-22658 | no |
| TESTS | 17 nested test mods | 11,439 | 22659-34097 | n/a |

## Constraints

- `frankenterm-core` already has a `src/storage/` directory
  containing `mmap_store.rs`, declared as a submodule from
  `storage.rs:56` via `pub mod mmap_store;`. The split must
  preserve this — i.e., new submodules go alongside `mmap_store.rs`
  in `src/storage/`.
- Several types currently used across sections are
  module-private (`pub(crate)` and bare `fn`). A split must
  bump visibility carefully — over-bumping leaks API; under-bumping
  breaks the build.
- `#![forbid(unsafe_code)]` is active in `frankenterm-core/lib.rs`
  — no module-init tricks; all splits are pure renames.
- Linter agent is known to revert Edit changes (per MEMORY.md);
  every split commit must use Write + commit-immediately.

## Recommended split phases

Phases are ordered low-risk → high-risk so each lands cleanly
before the next. Each is a separate follow-on bead.

### Phase 1 — Test extraction (LOW risk, HIGHEST impact on file size)

Move the 17 `#[cfg(test)] mod` blocks (lines 22659-34097, ~11,439
lines) into `crates/frankenterm-core/src/storage/tests.rs` as a
single test submodule (or split per-feature into
`storage/tests/<area>.rs`).

- **Win:** `storage.rs` shrinks from 34K → 22.6K lines (-33%).
- **Risk:** Tests use `super::*` to reach module-private items.
  Mitigation: add `pub(super)` on the items the tests touch, or
  use `#[path]` attribute to keep `super` resolution.
- **Touch surface:** Test file moved verbatim; production file
  loses a tail.
- **Bead:** ft-dn2tu.1

### Phase 2 — Pub types module (LOW risk)

Extract Sections 1-4 + 7 (pub structs/enums + DDL strings; total
~3,180 lines) into `storage/types.rs`. These are data-only — no
impl, no helpers — so the extraction is mechanical.

- Schema Definition (1) → `storage/schema_ddl.rs`
- Schema Migrations types (2, types only) → `storage/migrations/types.rs`
- Data Structures (3) + Timeline Data Model (4) → `storage/types.rs`
- Writer Command Types (7) → `storage/writer_commands.rs`
- **Bead:** ft-dn2tu.2

### Phase 3 — Health/Repair extraction (LOW risk)

Section 6 (Database Health Check & Repair, 2,043 lines) has a
clear public surface: `database_stats`, `check_database_health`,
`repair_database`. Extract to `storage/health.rs`.

- **Bead:** ft-dn2tu.3

### Phase 4 — Migrations runner (MEDIUM risk)

Section 2 (1,508 lines) + Section 5 (Schema Initialization, 116
lines): the migration classifier + plan executor. Pub fns:
`classify_migration_rollback_trigger`,
`execute_migration_rollback_playbook`, `pending_migrations`,
`migration_plan_for_path`, `migration_status_for_path`,
`migrate_database_to_version`. Move to
`storage/migrations/runner.rs` (paired with
`storage/migrations/types.rs` from Phase 2).

- **Bead:** ft-dn2tu.4

### Phase 5 — Export Query extraction (MEDIUM risk)

Section 23 (Export Query, 3,501 lines) is read-only, query-side
code with no writer interaction. Likely the cleanest large
extraction. Move to `storage/export.rs`.

- **Bead:** ft-dn2tu.5

### Phase 6 — Storage Handle split (HIGH risk, defer)

Section 8 (Storage Handle, 5,724 lines) is the central type with
46 impl blocks across the whole file. Splitting requires either:
(a) keeping the type in `storage/handle.rs` and moving impls into
trait-impl files via `impl StorageHandle { … }` blocks across
multiple files, or (b) accepting a single large file for the
handle. Defer until Phases 1-5 land and the cross-section call
graph is visible.

- **Bead:** ft-dn2tu.6

## Why ship the plan, not a first split

The bead allows "ship a first split if doable in <1h." I assessed
ship vs document and chose document because:

1. **Linter revert risk.** MEMORY.md flags that the linter agent
   reverts Edit-tool changes on storage.rs. A small experimental
   split would likely round-trip without landing.
2. **Test interleave.** The smallest standalone production
   sections (Pane Bookmarks 34 lines, Notification History 98
   lines) are too small to demonstrate the split pattern; the
   larger sections need test relocations to come along, which
   requires Phase 1 first.
3. **Cross-section visibility surface.** A useful split needs a
   visibility audit (what's `pub(crate)` vs bare `fn`) — that's
   the work of Phase 1 + 2, not 1 hour.

The plan + 6 follow-on beads is a more useful artifact than a
34-line excerpt move.

## Concrete next-day actions

Run, in this order:

```
br create  # ft-dn2tu.1: test extraction (Phase 1)
br create  # ft-dn2tu.2: pub types module (Phase 2)
br create  # ft-dn2tu.3: health/repair (Phase 3)
br create  # ft-dn2tu.4: migrations runner (Phase 4)
br create  # ft-dn2tu.5: export query (Phase 5)
br create  # ft-dn2tu.6: storage handle split (Phase 6, deferred)
```

Phase 1 alone takes the file from 34K → 22.6K (-33%) and is the
single highest-value step. Phases 2-5 collectively take it
toward 12K production lines, at which point the Storage Handle
split (Phase 6) is the only remaining large bin and can be
designed against a cleaner call graph.

## References

- `crates/frankenterm-core/src/storage.rs` — the 34K-line file
- `crates/frankenterm-core/src/storage/mmap_store.rs` — existing
  submodule, proves the split path works
- `crates/frankenterm-core/src/lib.rs:439` — `pub mod storage;`
  declaration (must remain valid post-split)
- ft-1zej2 / ft-1fv0u (AzureDog session) — prior strangler-fig
  extractions from `web.rs` and `mcp.rs`; same pattern
