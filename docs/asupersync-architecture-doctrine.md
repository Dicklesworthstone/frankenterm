# asupersync Architecture Doctrine for FrankenTerm

> Canonical reference for the runtime migration from tokio/smol to asupersync.
> This document defines the architectural target, behavioral invariants, and
> migration governance for `ft-e34d9` and its sub-programs.

**Bead:** ft-e34d9.10.1
**Status:** Living document (update as migration progresses)
**Companion files:**
- `docs/asupersync-migration-playbook.md` — mechanical rewrite patterns
- `docs/asupersync-runtime-inventory.json` — machine-readable snapshot

---

## 1. Doctrine Principles

### 1.1 Single Runtime Target

asupersync is the sole async runtime for FrankenTerm. tokio remains only as a
transitional fallback behind `#[cfg(not(feature = "asupersync-runtime"))]`.
No new code may introduce direct tokio dependencies; all async primitives flow
through `runtime_compat.rs`.

### 1.2 Cx Everywhere

Every async function that performs I/O, acquires locks, or sends on channels
must accept `cx: &mut Cx` (or obtain one from its scope). `Cx::for_testing()`
is permitted only inside `runtime_compat.rs` shims and `#[cfg(test)]` blocks.
Production code must never create ad-hoc Cx instances.

### 1.3 Structured Concurrency

All spawned tasks belong to a `Scope`. No orphan tasks. Scope hierarchy:

```
RuntimeScope (process lifetime)
  +-- WatcherScope (ft watch session)
  |     +-- PaneTailerScope (per-pane capture)
  |     +-- PatternEngineScope
  |     +-- WorkflowScope (per-workflow execution)
  +-- RobotApiScope (ft robot request)
  +-- WebServerScope (ft web, feature-gated)
  +-- McpServerScope (ft mcp serve, feature-gated)
```

### 1.4 Four-Valued Outcome

Use `Outcome<T, E>` instead of `Result<T, E>` at scope boundaries:

| Variant | Meaning | Action |
|---------|---------|--------|
| `Ok(T)` | Success | Propagate value |
| `Err(E)` | Domain error | Handle or propagate |
| `Cancelled` | Scope/parent requested stop | Clean up, do not retry |
| `Panicked` | Unrecoverable | Log with scope context, propagate |

**Rule:** Never silently downcast `Outcome` to `Result` unless the call site
explicitly documents why `Cancelled`/`Panicked` information is expendable.

### 1.5 Two-Phase Channel Operations

For any channel send that must survive cancellation:
```rust
let permit = tx.reserve(&cx).await?;   // Phase 1: reserve slot
permit.send(message);                   // Phase 2: commit (infallible)
```

This eliminates the tokio pattern where `select!` + `tx.send()` can lose
messages when the send branch is cancelled after enqueueing.

### 1.6 No Ambient Authority

No global runtime handles, no `tokio::spawn()` from arbitrary call sites.
All effects are gated through the Cx capability token. This enables:
- Deterministic testing via LabRuntime
- Per-scope resource budgets
- Auditable task ownership

---

## 2. Runtime Compatibility Layer

### 2.1 Architecture

```
Production code (29+ modules)
        |
        v
runtime_compat.rs (compile-time dispatch)
        |
   +---------+-----------+
   |                     |
   v                     v
asupersync           tokio (fallback)
(feature flag)       (default, transitional)
```

### 2.2 Primitive Coverage Matrix

| Primitive | asupersync impl | tokio fallback | Gap status |
|-----------|:-:|:-:|---|
| `Mutex<T>` | Y | Y | Complete |
| `RwLock<T>` | Y | Y | Complete |
| `Semaphore` | Y | Y | Complete, incl. owned permits |
| `mpsc` channel | Y | Y | Complete, Cx-aware send/recv |
| `watch` channel | Y | Y | Complete |
| `broadcast` channel | -- | Y | **Gap: needs asupersync impl** |
| `oneshot` channel | -- | Y | **Gap: needs asupersync impl** |
| `Notify` | -- | Y | **Gap: needs asupersync impl** |
| `sleep` / `timeout` | Y | Y | Complete |
| `spawn` / `spawn_blocking` | -- | Y | **Gap: needs scope.spawn** |
| `JoinHandle` / `JoinSet` | -- | Y | **Gap: needs Scope integration** |
| `select!` / `join!` | -- | Y | **Gap: needs combinator adapters** |
| `UnixStream` / `UnixListener` | Y | Y | Complete |
| `TcpListener` / `TcpStream` | -- | Y | **Gap: needs asupersync net** |
| `process::Command` | -- | Y | **Gap: needs asupersync process** |
| `signal` (ctrl_c, unix) | -- | Y | **Gap: needs asupersync signal** |
| `AsyncRead` / `AsyncWrite` | -- | Y | **Gap: trait adapter needed** |
| `time::pause` / `advance` | -- | Y (test-util) | **Gap: LabRuntime equivalent** |

### 2.3 Module Adoption

32 modules currently import from `runtime_compat`. Full list:

| Module | Primitives used | Feature-gated |
|--------|-----------------|:---:|
| cass.rs | process::Command, timeout | -- |
| caut.rs | process::Command, timeout | -- |
| cpu_pressure.rs | sleep | -- |
| events.rs | broadcast | -- |
| ipc.rs | RwLock, mpsc, unix sockets | -- |
| mcp.rs | CompatRuntime, RuntimeBuilder | asupersync-runtime |
| memory_budget.rs | sleep | -- |
| memory_pressure.rs | sleep | -- |
| metrics.rs | io, net, task::JoinHandle | -- |
| native_events.rs | mpsc, task::JoinSet, unix | -- |
| pool.rs | Mutex, Semaphore | -- |
| recorder_storage.rs | Mutex | -- |
| recording.rs | Mutex, CompatRuntime | -- |
| replay.rs | sleep, watch | vendored |
| restore_scrollback.rs | Semaphore, sleep | -- |
| runtime.rs | core compat primitives | -- |
| search_bridge.rs | Notify, CompatRuntime, mpsc | -- |
| sharding.rs | RwLock | -- |
| snapshot_engine.rs | Mutex, RwLock, mpsc, timeout, watch | -- |
| spsc_ring_buffer.rs | Notify | -- |
| storage.rs | mpsc, oneshot | -- |
| survival.rs | sleep | -- |
| tailer.rs | mpsc, timeout, RwLock, Semaphore | -- |
| telemetry.rs | sleep | -- |
| vendored/mux_client.rs | (vendored scope) | vendored |
| vendored/mux_pool.rs | (vendored scope) | vendored |
| wait.rs | sleep, CompatRuntime | -- |
| watchdog.rs | task::JoinHandle | -- |
| web.rs | mpsc, sleep, task, timeout | web |
| wezterm.rs | sleep, timeout, process::Command | -- |
| workflows.rs | sleep | -- |

---

## 3. Dependency Inventory Summary

### 3.1 Reference Counts (workspace-wide)

| Pattern | References | Files |
|---------|:-:|:-:|
| `tokio::` | 1,234 | 73 |
| `runtime_compat::` | 476 | 72 |
| `asupersync::` | 246 | 31 |
| `smol::` | 68 | 12 |

### 3.2 By Crate

| Crate | tokio | runtime_compat | asupersync | smol |
|-------|:-:|:-:|:-:|:-:|
| frankenterm-core | 1,234 | 430 | 119 | 0 |
| frankenterm (binary) | 0 | 46 | 120 | 0 |
| frankenterm/ssh | 0 | 0 | 2 | 44 |
| frankenterm/codec | 0 | 0 | 1 | 12 |
| frankenterm/config | 0 | 0 | 0 | 5 |
| frankenterm/pty | 0 | 0 | 0 | 4 |
| frankenterm/scripting | 0 | 0 | 0 | 2 |
| frankenterm/mux | 0 | 0 | 0 | 1 |

### 3.3 Hotspot Files (top 10 by runtime references)

| File | Refs | Notes |
|------|:-:|---|
| runtime_compat.rs | 213 | Abstraction layer itself |
| main.rs | 166 | Binary entrypoint; uses asupersync |
| storage.rs | 86 | Heavy channel + spawn usage |
| workflows.rs | 82 | Sleep + channel patterns |
| pool.rs | 65 | Mutex + Semaphore |
| snapshot_engine.rs | 54 | Full primitive spread |
| wezterm.rs | 46 | Backend adapter |
| tantivy_ingest.rs | 46 | Feature-gated (recorder-lexical) |
| tantivy_reindex.rs | 45 | Feature-gated (recorder-lexical) |
| simulation.rs | 42 | Test infrastructure |

### 3.4 Test Infrastructure

- **1,104** `#[tokio::test]` attributes remaining (all in test blocks)
- **0** `#[tokio::main]` in production code
- Test migration to `run_async_test()` pattern in progress (ingest, survival, recording done)

### 3.5 Vendored Crate Runtime Surface

The `frankenterm/` directory contains vendored ex-WezTerm crates. These use
**smol** (68 references across 12 files), not tokio. The asupersync migration
for vendored crates is tracked under ft-e34d9.10.5 and requires different
patterns (smol -> asupersync, not tokio -> asupersync).

---

## 4. Migration Risk Register

### 4.1 Risk Scoring

Each risk is scored on:
- **Likelihood** (1-5): How likely is this to cause problems?
- **Impact** (1-5): How severe if it occurs?
- **Score** = Likelihood x Impact (1-25)

### 4.2 Register

| ID | Risk | L | I | Score | Mitigation | Owner |
|----|------|:-:|:-:|:-:|---|---|
| R1 | **Cx threading breaks function signatures across 61+ async files** | 5 | 4 | **20** | Phase 4 scoping: thread Cx through scope boundaries first, then leaf functions. Use `Cx::for_testing()` as bridge during transition. | ft-e34d9.10.3 |
| R2 | **select!/join! macro removal causes subtle cancellation bugs** | 4 | 5 | **20** | Catalog every select! site. Test each with explicit cancellation scenarios before/after migration. Two-phase channel ops eliminate the worst class. | ft-e34d9.10.3 |
| R3 | **Broadcast channel migration breaks event bus fanout** | 4 | 4 | **16** | events.rs is the sole broadcast user in core. Design asupersync broadcast or replace with multi-consumer mpsc. Prototype in isolation first. | ft-e34d9.10.2 |
| R4 | **Signal handling gap blocks graceful shutdown** | 3 | 5 | **15** | web.rs is the only production signal user. Implement runtime_compat signal module. Can use Unix pipe as interim bridge. | ft-e34d9.10.4 |
| R5 | **Vendored smol crates conflict with asupersync reactor** | 4 | 3 | **12** | Feature-isolate vendored crates. The two reactors can coexist if epoll/kqueue registrations don't overlap. Track under ft-e34d9.10.5. | ft-e34d9.10.5 |
| R6 | **Process::Command migration breaks cass/caut agent detection** | 3 | 3 | **9** | Process spawning is synchronous under the hood; wrapping in spawn_blocking suffices. Low semantic change. | ft-e34d9.10.4 |
| R7 | **LabRuntime deterministic time differs from tokio test-util** | 3 | 3 | **9** | Tests using `time::pause()`/`advance()` (test-only) need LabRuntime equivalents. Not blocking production. | ft-e34d9.10.6 |
| R8 | **Multi-agent merge conflicts on runtime_compat.rs** | 4 | 2 | **8** | File reservations via Agent Mail. Small atomic PRs. runtime_compat.rs is append-mostly (new primitives), reducing conflict surface. | All agents |
| R9 | **Binary entrypoint (main.rs) migration breaks CLI startup** | 2 | 4 | **8** | main.rs already uses asupersync (120 refs). Risk is in untested code paths. Add integration smoke tests before cutover. | ft-e34d9.10.2 |
| R10 | **TCP/net migration breaks metrics server and distributed mode** | 3 | 2 | **6** | metrics.rs and distributed.rs are feature-gated. Can migrate independently. Low blast radius. | ft-e34d9.10.4 |

### 4.3 Risk Heatmap

```
Impact  5 |         R4    R2
        4 | R9           R1
        3 |    R10  R6,R7  R5  R3
        2 |    R8
        1 |
          +--+----+----+----+----+
            1    2    3    4    5  Likelihood
```

---

## 5. Migration Scorecard

### 5.1 Phase Completion

| Phase | Description | Status | % Complete | Blocking |
|:---:|---|---|:---:|---|
| 0 | Epic framing | Done | 100% | -- |
| 1 | Workspace integration | Done | 100% | -- |
| 2 | Sync primitives | In progress | 70% | Broadcast, Oneshot, Notify gaps |
| 3 | Network I/O | Not started | 10% | Unix sockets done; TCP, Process, Signal remain |
| 4 | Task spawning / structured concurrency | Not started | 0% | Requires Scope design + Cx threading |
| 5 | Channels | In progress | 50% | mpsc + watch done; broadcast + oneshot remain |
| 6 | LabRuntime test infra | Spike only | 5% | Needs time control + DPOR integration |
| 7 | Cutover | Not started | 0% | All above must complete |

### 5.2 Module Migration Depth

Depth levels:
- **D0**: No runtime_compat usage (raw tokio or smol)
- **D1**: Imports runtime_compat but uses tokio-only primitives (broadcast, oneshot, spawn)
- **D2**: Uses dual-impl primitives (Mutex, mpsc, sleep, etc.)
- **D3**: Fully asupersync-ready (all primitives have asupersync paths)

| Depth | Module count | Examples |
|:---:|:---:|---|
| D0 | ~40 | Pure data structures, no async (majority of core modules) |
| D1 | 8 | events.rs (broadcast), storage.rs (oneshot), watchdog.rs (spawn) |
| D2 | 20 | pool.rs, ipc.rs, tailer.rs, snapshot_engine.rs |
| D3 | 4 | cpu_pressure.rs, memory_pressure.rs, survival.rs, workflows.rs |

### 5.3 Test Migration Depth

| Metric | Count | Notes |
|--------|:---:|---|
| Total `#[tokio::test]` | 1,104 | Must convert to runtime-agnostic |
| Converted to `run_async_test()` | ~15 | ingest, survival, recording modules |
| Remaining | ~1,089 | Bulk conversion is Phase 6 work |
| asupersync-specific tests | ~50 | Spike + smoke + integration tests |

---

## 6. Sequencing Recommendations

### 6.1 Critical Path

```
ft-e34d9.10.1 (this doc)
  -> ft-e34d9.10.2 (Core runtime substrate)
       -> ft-e34d9.10.3 (Structured concurrency + cancellation)
            -> ft-e34d9.10.4 (IPC + network stack)
                 -> ft-e34d9.10.5 (Vendored crate harmonization)
                      -> ft-e34d9.10.6 (Deterministic verification)
                           -> ft-e34d9.10.7 (Observability + SLO gates)
                                -> ft-e34d9.10.8 (Cutover governance)
```

### 6.2 Parallel Work Opportunities

These can proceed in parallel with the critical path:

1. **Test migration** (converting `#[tokio::test]` to `run_async_test()`) can
   happen at any time without blocking other work.
2. **Broadcast channel design** can be prototyped independently.
3. **LabRuntime spike tests** can expand without waiting for production migration.
4. **Vendored smol crates** investigation (ft-e34d9.10.5) can start during Phase 2.

### 6.3 Priority Order for Remaining Gaps

| Priority | Gap | Rationale |
|:---:|---|---|
| 1 | broadcast channel | Only primitive blocking events.rs migration |
| 2 | select!/join! combinators | Blocks Phase 4 scope.spawn conversion |
| 3 | oneshot channel | Used in storage.rs request-response |
| 4 | Notify | Used in search_bridge.rs, spsc_ring_buffer.rs |
| 5 | TCP networking | Feature-gated (metrics, distributed) |
| 6 | Process::Command | Low semantic change, spawn_blocking bridge works |
| 7 | Signal handling | Only web.rs production use |
| 8 | AsyncRead/AsyncWrite traits | Blocked on asupersync trait design |

---

## 7. Rules for New Code

1. **All new async code** must use `runtime_compat` imports, never direct tokio.
2. **All new tests** should use `run_async_test()` pattern, not `#[tokio::test]`.
3. **Never add `tokio` to a crate's direct dependencies** — use workspace dep.
4. **Document Cx threading** in function signatures with `/// # Cx` doc comments.
5. **Prefer `Outcome<T, E>`** at scope boundaries; `Result<T, E>` within scopes.
6. **File reservations required** before modifying `runtime_compat.rs`.

---

## 8. Acceptance Criteria for ft-e34d9.10.1

- [x] Exhaustive async/runtime dependency inventory (Section 3 + companion JSON)
- [x] Canonical asupersync doctrine (Sections 1, 6, 7)
- [x] Migration risk register with scores (Section 4)
- [x] Phase completion scorecard (Section 5)
- [x] Sequencing recommendations with parallel opportunities (Section 6)
- [x] Rules for new code (Section 7)
