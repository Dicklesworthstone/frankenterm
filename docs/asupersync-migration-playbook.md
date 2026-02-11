# asupersync Runtime Migration Playbook (wa-e34d9)

This document is an operator/developer playbook for migrating FrankenTerm from `tokio` to `asupersync`.

It is intentionally **workflow-oriented**: it maps the *bead phases* to concrete code changes, gives safe
mechanical rewrite patterns, and lists “gotchas” that tend to cause long-tail churn.

## Scope

`wa-e34d9` is the umbrella epic: **replace all tokio usage** across the workspace with asupersync concepts:

- runtime entrypoints (`#[tokio::main]`, `Runtime`, `spawn`)
- sync primitives (`Mutex`, `RwLock`, `Semaphore`, atomics + notify patterns)
- channels (`mpsc`, `watch`, `broadcast`)
- time (`sleep`, `timeout`, `Instant`, interval loops)
- select/race patterns
- I/O (`Tcp*`, `Unix*`, TLS/HTTP stack)
- tests (virtual time, DPOR, structured cancellation)

This playbook assumes we will run **dual-runtime** during the transition (feature gated), so we can land work
incrementally while keeping `main` green.

## Terminology (asupersync mental model)

- **`Cx`**: capability context token; async functions typically take `cx: &mut Cx`.
- **`Scope`**: structured concurrency boundary; all tasks are scoped (no orphans).
- **`Outcome<T, E>`**: four-valued result (`Ok`, `Err`, `Cancelled`, `Panicked`).
- **Two‑phase ops**: `reserve/commit` channel and similar patterns to avoid cancellation loss.
- **LabRuntime**: deterministic test runtime w/ virtual time and DPOR exploration.

## Phase Map (beads → code)

Suggested execution order matches the bead DAG:

1. **Phase 0 — Epic framing (`wa-e34d9`)**
   - Decide workspace‑level strategy: feature flags, crate boundaries, what “done” means.
   - Inventory tokio usage, identify the “trunk” call graphs (watcher loop, mux clients, workflows).

2. **Phase 1 — Workspace integration (`wa-hj458`)**
   - Add `asupersync` and `asupersync-macros` under `[workspace.dependencies]` (pinning strategy explicit).
   - Add crate‑level optional deps + a feature flag such as `async-asupersync` / `asupersync-runtime`.
   - Ensure `cargo build` works on macOS and Linux, default path unchanged.

3. **Phase 2 — Sync primitives (`wa-3d14m`)**
   - Replace `tokio::sync::{Mutex,RwLock,Semaphore,Notify}` with asupersync equivalents.
   - Key risk: deadlocks/priority inversions if we keep “callbacks inside lock” patterns.

4. **Phase 3 — Network I/O** (already tracked in beads; keep the migration surface narrow per file)
   - Convert `UnixStream/UnixListener`, mux protocol sockets, and any HTTP webhook paths.

5. **Phase 4 — Task spawning / structured concurrency (`wa-1bznu`)**
   - Convert `tokio::spawn` trees into `scope.spawn` trees, plumb `Cx`.
   - Decide: one `Scope` per watcher, per pane tailer, per workflow execution, etc.

6. **Phase 5 — Channels (`wa-2abzy`)**
   - Convert MPSC/watch/broadcast usage. Prefer narrowing broadcast usage early (fanout hotspots).

7. **Phase 6 — LabRuntime test infra (`wa-a4goc`)**
   - Port the highest-value concurrency invariants first (deadlocks, lost events, cancellation correctness).

8. **Cutover**
   - Remove tokio deps and any compat shims once every crate builds under asupersync path by default.

## Mechanical Rewrite Patterns

### `spawn`

- **Tokio**
  - `tokio::spawn(async move { ... })`
- **Asupersync**
  - `scope.spawn(|cx| async move { ... })` (exact API may vary; keep all spawns *owned by a scope*)

Migration tip: during dual-runtime, centralize spawning behind a small internal helper so call sites don’t
need to know which runtime is active.

### `select!` / racing futures

- Prefer “race/first-complete” combinators and keep cancellation semantics explicit.
- Be careful with “select + channel receive”: the classic tokio pattern can lose messages on cancellation; the
two-phase reserve/commit is designed to fix this.

### `sleep` / intervals

Replace `tokio::time::sleep(d).await` with `cx.sleep(d).await` (or the asupersync equivalent).

Avoid “wall clock” drift bugs by keeping interval loops as `sleep_until(next_deadline)` rather than “sleep(d)
then do work” when correctness matters.

## Failure Semantics Checklist (Outcome)

When you convert a function to `Outcome<T, E>`:

- Decide which call sites should treat `Cancelled` as a normal shutdown path vs an error.
- Log `Panicked` with enough context to diagnose scope ownership.
- Avoid silently mapping `Outcome` back to `Result` unless the loss of information is explicitly acceptable.

## High-Churn Hotspots (plan around them)

These areas tend to cause the most merge conflicts if multiple agents work without coordination:

- workspace `Cargo.toml` / feature matrices
- core runtime entrypoints + watcher loop glue
- event bus fanout / notification coalescing
- mux client pools and reconnection logic

Use Agent Mail file reservations aggressively around those files, and prefer “small vertical slices” per bead.

## Suggested Inventory Commands

These are intended for quick sizing and routing work to beads:

```bash
rg -n "tokio::" crates frankenterm
rg -n "tokio::(spawn|select!|time::|sync::)" crates frankenterm
rg -n "async fn" crates frankenterm
```

## Coordination Notes (multi-agent)

- Avoid landing partial feature‑flag wiring without the accompanying `cargo check --all-targets` pass.
- If the shared checkout is dirty on reserved files, **do not** “clean it up” for someone else; message the
reservation holder and wait, or work on docs/specs that don’t touch those paths.

