# Round-2 deadlock audit

Re-scan of the surface audited in round-1
(`docs/review/deadlock-audit.md`, 2026-04-26 02:16) against HEAD.
Round-1 concluded with zero new beads filed; round-2 confirms
saturation and adds the 4 sub-crates extracted in the meantime.

## Method

1. `git log --since='10 hours ago'` against the round-1 file set —
   one commit (`4ccf4175 refactor(runtime): remove runtime_compat
   alias`), pure rename, no new lock semantics.
2. Heuristic Python scan for "guard held across non-lock-acquisition
   await" across `crates/frankenterm-core/src/`,
   `frankenterm/mux/src/`, `frankenterm/term/src/`.
3. Same scan extended to the 4 new sub-crates extracted under
   ft-y0loj.* / ft-j1qjt.* (`replay-types`, `replay`, `ars`,
   `tantivy`).
4. Cross-lock acquisition order audit for the
   `cursors → contexts → tracker` triple in `runtime.rs` to confirm
   no AB-BA inversion was introduced.

## Findings

### Production code (excluding tests): 1 candidate, low concern

The heuristic returned a single non-test hit in
`crates/frankenterm-core/src/notifications.rs:325`:

```rust
let muted = {
    let storage_guard = storage.read().await;
    storage_guard
        .is_event_muted(&identity_key, now_ms)
        .await
        .unwrap_or(false)
};
```

The read guard is held across `is_event_muted().await`. Verdict: **not
a deadlock** — `is_event_muted` is a method on the value inside the
guard (`storage_guard.is_event_muted(...)`), so it does not re-acquire
the outer lock. The pattern's risk is *writer starvation under
contention*, not deadlock. The existing comment block (lines 321-323)
acknowledges the deliberate switch from `lock()` to `read()` to allow
concurrent mute checks. No bead filed.

### Test code: 12 hits, all expected

All 12 hits in `pool.rs::tests::*` and `snapshot_engine.rs::tests::*`
are tests that intentionally hold a lock across an `.await` to
exercise pool capacity / stats / wakeup behavior. Not bugs.

### Cross-lock ordering in runtime.rs

`cursors → contexts → tracker` partial order is preserved across all
sites that acquire two or three of the triple. No reverse-order
acquisition exists at HEAD. Confirmed via:

```sh
rg -n 'cursors\.(read|write)\(\)\.await|contexts\.(read|write)\(\)\.await|tracker\.(read|write)\(\)\.await' crates/frankenterm-core/src/runtime.rs
```

### Sub-crate scan: zero async-lock acquisitions

`rg '\.(lock|read|write)\(\)\.await'` against
`crates/frankenterm-core-{replay,replay-types,ars,tantivy}/src/`
returns zero hits. The extracted clusters use only sync mutexes for
local state (`replay_guardrails.rs` has 6 `.lock()` sites, all
sync-scoped) — same discipline as core.

## Verdict

**Round-2 is saturated.** Zero new beads filed. The single non-test
candidate (`notifications.rs:325`) was already audited in round-1 and
deliberately left as-is with documenting commentary.

The async-lock surface remains clean and consistently scoped across
the recent sub-crate extractions. Re-running this audit after the
ft-j1qjt.4-6 follow-ups (event_id / ingest / recording leaf
extractions, when they happen) is the next prudent moment.

## Cross-references

- `docs/review/deadlock-audit.md` — round-1 audit this re-scans.
- ft-y0loj parent + ft-j1qjt children — the extractions that motivated
  re-running the scan.
