# Round-2 Mock-Finder Sweep — Saturation

**Scope:** repeat the round-1 mock/stub/TODO sweep
(`docs/review/sub-crate-mock-audit.md`, HEAD 04944be4) against the
post-rename + post-extraction codebase. Same rg patterns, same audit
checklist.
**Date:** 2026-04-26
**Verdict:** **SATURATED** — zero new genuine WIP. Counts toward the
3-saturated-rounds stop-condition.

## What changed since round 1

Between round 1 (HEAD 04944be4, 2026-04-26 02:05) and now (HEAD
1245487a, 2026-04-26 03:30):

- **Replay sub-crate populated** (cc7's c62dda66 + 38599a91). The
  dead-stub state flagged in round 1 (ft-lwa5q) is fully resolved —
  `frankenterm-core-replay/src/` now contains the 24 replay_*.rs files,
  cargo check clean.
- **Two more leaf crates landed:** `frankenterm-core-config-types`
  (ft-otfxs), `frankenterm-core-policy-types` (ft-0pykm),
  `frankenterm-core-replay-types` (ft-j1qjt.1),
  `frankenterm-core-telemetry-types` (ft-yf2am).
- **runtime_compat → runtime_async migration completed** in tests
  (ft-y378j.1) + production source (ft-y378j.2) + benches (this
  rotation, swept into ft-y378j.2's commit). Residual references are
  in an 8-file allowlist guarded by CI (ft-y378j.3).
- **PooledReadConn** introduced for storage read paths (ft-bhyxz).
- **Codec double-serialize** eliminated (ft-gbpoy).
- **CI cycle guard** wired (ft-94juo).

## Round-2 sweep results

```bash
rg '\b(todo|unimplemented|panic)!\(' crates/frankenterm-core-*/src/
rg '\bunreachable!\('                crates/frankenterm-core-*/src/
rg '\b(Mock|Fake|fake_)\b'           crates/frankenterm-core-*/src/
rg '\b(todo|unimplemented|unreachable)!\(' crates/frankenterm-core/src/
```

### Findings: all benign, all matching round-1 categories

**panic!() — all in `#[cfg(test)]` blocks:**
- `frankenterm-core-ars/src/ars_evolve.rs` — 4 sites at 631/641/670/688,
  all inside test mod (started line 481). Standard test-failure idiom
  (`panic!("should evolve")`).
- `frankenterm-core-ars/src/ars_intercept.rs:730,1036` — test panics.
- `frankenterm-core-replay/src/replay_cli.rs:483` — test panic.
- `frankenterm-core-tantivy/src/tantivy_policy.rs:1169` — test panic.
- `frankenterm-core-tantivy/src/recorder_lexical_ingest.rs:457,870` —
  test panics.

**unreachable!() — all defensive invariant assertions:**
- `frankenterm-core-ars/src/ars_symbolic_exec.rs:449` — same finding as
  round 1, properly guarded by line 419's short-circuit. Already
  classified acceptable.
- `frankenterm-core/src/policy.rs:5415` — `Some(true) => unreachable!("alt-screen deny path returned earlier")`. Documents an upstream return.
- `frankenterm-core/src/policy.rs:11338` — match-arm exhaustiveness fallback.
- `frankenterm-core/src/runtime_async.rs:5383,5462,5531,5606,5684,5774` — six `unreachable!("watcher always returns Err on cancel")` sites. Each documents a watcher-type API guarantee. Defensive.
- `frankenterm-core/src/tui/keymap.rs:611` — `Scope::Global => unreachable!()` — match-arm fallback.

**Mock/Fake — all in `#[cfg(test)]` blocks:**
- `frankenterm-core-replay/src/replay_artifact_registry.rs` — `MockFs`
  test fixture (line 778, inside test mod started 771).
- `frankenterm-core-tantivy/src/tantivy_ingest.rs` — `MockIndexWriter`
  (already noted round 1, test-scope).
- `frankenterm-core-tantivy/src/tantivy_reindex.rs` — Mock fixtures
  inside test mod.

**TODO/FIXME/XXX/HACK comments — none found** (scoped to the listed
crates). The few hits in round 1 were string-literal test fixtures
(`"ANTHROPIC_API_KEY=sk-ant-api03-XXXXX"`) and remain test-scope.

## Comparison to round 1

| Category | Round 1 | Round 2 | Delta |
| --- | ---: | ---: | ---: |
| `todo!()` | 0 | 0 | 0 |
| `unimplemented!()` | 0 | 0 | 0 |
| `panic!()` (production) | 0 | 0 | 0 |
| `unreachable!()` (defensive, properly guarded) | 1 | 9 | +8 (mostly already-existing in core, swept by the broader scope) |
| Test-only `Mock`/`Fake` | 1 | 3 | +2 (replay_artifact_registry, tantivy_reindex — both test-scope) |
| Intentional feature-gate stubs (tracked) | 1 (frankensqlite_unsupported, ft-lzbkn) | 1 (same) | 0 |
| Genuine WIP | 0 | 0 | 0 |
| Dead stub crates | 1 (ft-lwa5q) | **0** (resolved by cc7) | -1 |

**No new beads filed.** The round-1 finding (ft-lwa5q replay broken
stub) is now resolved at HEAD; the remaining surface is structurally
clean.

## Saturation accounting

This is **round 2 of 3** for the saturation stop-condition.
Per-rotation cadence is preserved (~30-min sweep, doc shipped, no new
beads when saturated). Round 3 of mock-finder is the next gate; if it
also saturates, the orchestrator's stop-condition fires.

The codebase has settled. Sub-crates are not regressing; the
extractions did not introduce hidden mocks or genuine WIP. The
only "findings" are categories already documented in round 1 with
slightly broader scope (because more sub-crates exist now to sweep).
