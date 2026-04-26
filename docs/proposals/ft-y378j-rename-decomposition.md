# Proposal: decompose the `runtime_compat → runtime_async` rename

**Bead:** [ft-y378j](../../.beads/issues.jsonl) — closing as superseded by .1-.4 children
**Related:** ft-7iof6 (the original "runtime_compat is canonical, not compat" reframe), AGENTS.md §"Async Runtime: asupersync"

## Reality check

AGENTS.md states that the rename to `runtime_async` is "pending under
ft-g43fq" and that `runtime_compat` is the "canonical async API
surface" with the "compat" framing retired. The cc6 modes-of-reasoning
review at HEAD found this framing optimistic:

```
$ rg -l 'runtime_compat::' crates/frankenterm-core/src/      | wc -l
80
$ rg 'runtime_compat::' crates/frankenterm-core/src/         | wc -l
695
$ rg 'runtime_async::'   crates/frankenterm-core/src/        | wc -l
1
```

695 inline references across 80 source files plus another **494 in
the tests/ tree** (total ~1,189 sites in `frankenterm-core` alone).
Exactly 1 file uses the new name. The "rename" landed in name only —
mass migration has not happened.

Top offenders by reference count:

| file                                       | refs |
|--------------------------------------------|-----:|
| `distributed.rs`                           | 65   |
| `ipc.rs`                                   | 50   |
| `storage.rs`                               | 32   |
| `snapshot_engine.rs`                       | 29   |
| `vendored/mux_client.rs`                   | 27   |
| `events.rs`                                | 26   |
| `pool.rs`                                  | 25   |
| `runtime.rs`                               | 23   |
| `wezterm.rs`                               | 22   |
| `tx_execution.rs`                          | 21   |

## Why decompose

A single mass `sed s/runtime_compat/runtime_async/g` across the
workspace would touch ~1,189 sites in one commit. Three problems:

1. **AGENTS.md no-deletion guard.** Renaming a re-export module path
   inside `frankenterm-core/src/` triggers the same hook that blocked
   the ars-extraction file deletes (ref ft-mr35k commit 876537fc).
   The mass-rename commit would need either a deliberate hook bypass
   or a careful split.
2. **Test churn vs production churn.** Tests use the runtime
   primitives mostly for setup (`runtime_compat::block_on`, channel
   constructors); production uses them for hot-path orchestration.
   These are different review surfaces and reverting is easier when
   they're separate commits.
3. **Concurrent agent coordination.** With ~10 sub-crates extracted
   under ft-y0loj.* and live cc6/cc7 lanes, a 1,189-site touch in one
   commit makes merge conflicts inevitable. Smaller commits land
   atomically against whatever lands in parallel.

The ft-j1qjt revert+decompose pattern (4994a1a9 → ft-j1qjt.{1,3,2})
proved that splitting a too-big-to-land bead into 3-4 children with
explicit ordering ships in days instead of stalling for weeks. Apply
the same template here.

## Decomposition

| bead          | scope                                                                | risk  |
|---------------|----------------------------------------------------------------------|-------|
| ft-y378j.1    | mechanical sed-driven rename of `crates/frankenterm-core/tests/**` (494 sites) — test setup only, no production code path changes | low   |
| ft-y378j.2    | production source rename in non-runtime modules (the ~620 sites outside `runtime.rs` / `runtime_compat.rs` / `cancellation.rs` itself) | medium |
| ft-y378j.3    | workspace-wide audit + verify: `rg runtime_compat::` returns zero hits in non-deprecated paths; the deprecated alias still re-exports for one release; CI guard added to prevent re-introduction | medium |
| ft-y378j.4    | remove the `pub use runtime_async as runtime_compat;` deprecation alias after one release cycle has passed since .3 landed | low   |

Order: **.1 → .2 → .3 → .4**. Each step's success signal is the next
step's prerequisite — `.2` should not start until `.1` lands because
the test surface changes in `.1` would otherwise have to be
re-resolved against `.2`'s production rename.

## Why not a single sed sweep with git hook bypass

Tempting (it's a syntactic rename, after all), but two things make
the four-step path safer:

- **Linter agent re-formats per file.** When a single 1,189-site
  commit gets reformatted by the linter agent, you can't tell which
  hunks are intentional and which are linter noise. Smaller commits
  let reviewers diff each step against a clean baseline.
- **Failure isolation.** If ft-y378j.2 introduces a compile error
  somewhere unexpected, reverting `.2` doesn't lose the test-side
  rename `.1` already validated. A monolithic commit forces an
  all-or-nothing revert.

## Acceptance for THIS proposal

- [x] Reality numbers verified: 695 src refs / 494 test refs / 1 new-name ref.
- [ ] Children ft-y378j.{1,2,3,4} filed under ft-y378j.
- [ ] ft-y378j closed with "superseded" note pointing at the children.

## Cross-references

- AGENTS.md §"Async Runtime: asupersync" — the framing this proposal
  reality-checks.
- `docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md` — the
  original ft-7iof6 reframe; .1-.4 implement what it described.
- ft-j1qjt revert at baef663e — the decomposition pattern this
  proposal copies.
