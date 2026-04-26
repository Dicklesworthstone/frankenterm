# Sub-Crate Mock / Stub / TODO Audit (review pass)

**Scope:** the 9 newly-extracted sub-crates from `frankenterm-core` (plus the
10th, `frankenterm-core-replay/`, which the user mentioned as cc7's lane).
**Date:** 2026-04-26
**Method:** ripgrep across `src/` for `todo!()`, `unimplemented!()`,
`unreachable!()`, `panic!()`, `FIXME` / `XXX` / `HACK` / `TODO` / `TBD`,
`Mock` / `Fake` / `fake_`, `stub`, `placeholder`, `not yet implemented`.

## Tally

| Sub-crate                            | hits | grade |
| ------------------------------------ | ---- | ----- |
| `frankenterm-core-ars`               | 1    | acceptable (defensive `unreachable!()`) |
| `frankenterm-core-tantivy`           | 2    | acceptable (intentional feature gate + test-only `Mock`) |
| `frankenterm-core-fleet`             | 0    | clean |
| `frankenterm-core-resource-types`    | 0    | clean |
| `frankenterm-core-error-types`       | 0    | clean |
| `frankenterm-core-config-types`      | 0    | clean |
| `frankenterm-core-policy-types`      | 0    | clean |
| `frankenterm-core-replay-types`      | 0    | clean |
| `frankenterm-core-telemetry-types`   | 0    | clean |
| **`frankenterm-core-replay`**        | —    | **DEAD STUB CRATE — see (D1)** |

8 of 9 type / leaf crates extracted under `ft-y0loj.*` are completely
clean of stubs, mocks, and placeholders. Two findings in the remaining
two crates are both intentional / well-defended.

## (A) Acceptable findings — no action needed

### A1. `frankenterm-core-ars/src/ars_symbolic_exec.rs:449`

```rust
ShellToken::Redirect(_) => unreachable!(),
```

**Verdict: defensive invariant assertion, properly guarded.**

Lines 419–422 short-circuit `ShellToken::Redirect(op)` via
`if let ... { expect_redirect_target = Some(op.clone()); continue; }` —
so by the time the inner `match token` runs at line 424, `Redirect` is
unreachable by construction. The `unreachable!()` documents the
invariant rather than silently mishandling.

### A2. `frankenterm-core-tantivy/src/tantivy_ingest.rs:825,871`

```rust
pub(crate) fn frankensqlite_unsupported(context: &str) -> IndexerError {
    IndexerError::Config(format!(
        "frankensqlite event reader not yet implemented for {context}; \
         enable cargo feature `frankensqlite-recorder` once support lands"
    ))
}
```

**Verdict: intentional feature-gate stub, well-tested, tracked.**

- Returns a typed `IndexerError::Config`, not a panic.
- Mirrors the config-parse gate added in commit `fe0e2ca3` so operators
  see consistent messaging.
- Names the cargo feature flag (`frankensqlite-recorder`) operators need
  to enable.
- Tracked under bead `ft-lzbkn` (per the comment at line 1637).
- Test `frankensqlite_unsupported_error_names_feature_flag_and_context`
  (line 1636) verifies the message format across both ingest and reindex
  call sites.

This is the *correct* shape for an unimplemented-feature stub: typed
error, discoverable message, tracking bead.

### A3. `frankenterm-core-tantivy/src/tantivy_ingest.rs:1791` — `MockIndexWriter`

**Verdict: test-only mock, properly gated.**

Lives inside `#[cfg(test)] mod tests { … }` (test module starts at line
1596). Compiles into the test binary only.

## (B) Test-fixture noise — no action needed

- `ars_secret_scan.rs:803` — `"ANTHROPIC_API_KEY=sk-ant-api03-XXXXX"` is
  a redacted secret in a test fixture, not an `XXX` placeholder.
- `ars_secret_scan.rs:836` — `"grep -r 'TODO' src/"` is a shell command
  in a test fixture, not a `TODO` comment.
- `error_codes.rs:3` + `lib.rs:3` — doc comments referencing the
  `WA-XXXX` error-code naming scheme, not `XXX` placeholders.
- `ars_compile.rs:297,726` + `ars_generalize.rs:159` — `placeholder` is
  the legitimate field name on a Jinja-style template-variable struct.

## (C) Genuine WIP — already tracked

None found in the 9 type / leaf crates.

## (D) Leftover from extraction — needs cleanup

### D1. `frankenterm-core-replay/` is a dead stub crate

**Filed:** `ft-lwa5q` — review/P1, dep on ft-j1qjt.

`crates/frankenterm-core-replay/src/lib.rs` declares **28 `pub mod replay_*`
submodules**, but only `lib.rs` itself is on disk in that directory. The
corresponding 28 `replay_*.rs` files are sitting **untracked** in
`crates/frankenterm-core/src/`:

```
?? crates/frankenterm-core/src/replay.rs
?? crates/frankenterm-core/src/replay_artifact_registry.rs
?? crates/frankenterm-core/src/replay_capture.rs
... (25 more)
```

This came from the partial revert in commit `baef663e`
("revert(frankenterm-core): ft-j1qjt replay extraction blocked — not a
tier-1 leaf"): the file moves out of core were undone and the 28 files
restored to `crates/frankenterm-core/src/`, but **the new sub-crate
directory was never deleted** and the replay files were never re-staged
into the working tree commitment. Result:

- `cargo check -p frankenterm-core-replay` → 28 "module file not found"
  errors on every submodule declaration.
- `cargo check -p frankenterm-core` → 16 `cannot find replay_*` errors,
  because `frankenterm-core/src/lib.rs` likely references some of those
  modules that are present on disk but untracked / their declarations
  removed.

This is the source of the "16 pre-existing `replay_*` baseline errors"
I've been filtering past in every recent build verification.

**Cleanup options for cc7's replay/ lane (per ft-lwa5q):**
1. **Hard-delete `crates/frankenterm-core-replay/`** and `git add` the
   28 `replay_*.rs` files back into `crates/frankenterm-core/src/`. Most
   honest — restores the pre-attempt baseline. Keeps the workspace
   member entry around if removed; remove that too.
2. **Complete the partial extraction** — move all 28 files into the new
   crate, rewrite ~250 cross-cluster `crate::*` references, plumb
   features. This was rejected once (baef663e).
3. **Hybrid** — split the 28 files into leaf-clean vs. cycle-blocked
   (similar to ft-y0loj.3 fleet partial extract pattern). Probably
   2-3 sessions of focused work.

Recommended: option (1) plus a fresh proposal for option (3) once the
leaf-types extractions stabilize.

## Mock-finder methodology note

This audit was deliberately narrow — only `src/`, only the listed
extraction targets. It does **not** verify that the surrounding tree is
clean. In particular, three places in the parent crate that the user's
mock-finder skill template would normally also probe are out of scope
here:

- `crates/frankenterm-core/src/mcp_helpers.rs` is orphaned (declared
  nowhere; flagged in `ft-t2d70` proposal as dead code).
- `crates/frankenterm-core/tests/proptest_fleet_dashboard.rs` has been
  broken since `ft-y0loj.3` (imports `frankenterm_core::fleet_dashboard`
  which moved to `frankenterm-core-fleet`; one-line fix).
- `crates/frankenterm-core-replay/` per (D1) above.

These are tracked separately and not included in the per-sub-crate tally.
