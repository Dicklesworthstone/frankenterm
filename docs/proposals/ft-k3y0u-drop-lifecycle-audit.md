# Drop lifecycle audit (ft-k3y0u)

## Summary

`crates/frankenterm-core/src/` currently has 28 `Drop` impls. Most are small
RAII guards that release a slot, decrement a counter, clear a test override, or
remove lock metadata. A smaller set does semantic work at destructor time:
stopping background threads/tasks, flushing recorder frames, leaving a terminal
session, closing channels, cancelling subscriptions, or returning SQLite
connections to a global pool.

The lifecycle risk is not that `Drop` exists. The risk is when `Drop` becomes
the primary path for a logical state transition that may need ordering,
observability, or error reporting. For those cases, `Drop` should remain a
fallback; an explicit shutdown/finalize path should be the contract.

## Source Command

```bash
rg -n "^impl(?:<[^>]+>)?\s+Drop\s+for\s+|impl\s+Drop\s+for\s+" \
  crates/frankenterm-core/src -g'*.rs'
```

## Classification Matrix

| Site | Type | Classification | What Drop does | Risk |
| --- | --- | --- | --- | --- |
| `build_coord.rs:260` | `BuildLock` | resource-cleanup | Removes build lock metadata file; OS file lock releases with file handle. | Low |
| `runtime.rs:216` | `StreamingTasks` | state-machine-transition | Aborts all per-pane streaming tasks when the task map drops. | Medium: abort is a logical shutdown fallback, not an observed shutdown protocol. |
| `distributed.rs:526` | `ConnectionPermit` | resource-cleanup | Decrements active distributed connection count. | Low |
| `telemetry.rs:1011` | `ScopeTimer` | resource-cleanup | Records elapsed time histogram. | Low |
| `semantic_anomaly_watchdog.rs:413` | `SemanticAnomalyWatchdog` | state-machine-transition | Sets running false and joins the ML thread. | High: destructor can block shutdown if the worker does not observe the flag promptly. |
| `events.rs:933` | `EventSubscriber` | resource-cleanup | Decrements active subscriber metric. | Low |
| `runtime_async.rs:1467` | `KillOnDropGuard` | cleanup-on-error | Marks a child-process cancel flag if command setup exits before disarming. | Medium: correct fallback, but cancellation is only a flag until later code acts on it. |
| `workflows/lock.rs:326` | `PaneWorkflowLockGuard` | state-machine-transition | Releases pane workflow lock. | Medium: lock release is logical state and has no error channel in Drop. |
| `native_events.rs:336` | `NativeEventListener` | resource-cleanup | Removes Unix socket path on drop. | Medium: path cleanup is best-effort and not tied to an ownership token beyond the listener value. |
| `tui/terminal_session.rs:219` | `SessionGuard<S>` | state-machine-transition | Calls `session.leave()` and clears global output gate. | High: terminal session state and global output gate are semantic state changes hidden in Drop. |
| `cancellation_safe_channel.rs:91` | `Reservation` | cleanup-on-error | Rolls back uncommitted capacity reservation and wakes waiters. | Low |
| `cancellation_safe_channel.rs:706` | `ReserveGuard<'_, T>` | cleanup-on-error | Relies on inner reservation Drop to rollback; value drops normally. | Low |
| `recording.rs:134` | `FrameWriter` | cleanup-on-error | Best-effort flushes buffered frames and swallows errors/panics. | High: data finalization happens in Drop and write failures cannot be reported. |
| `spsc_ring_buffer.rs:229` | `SpscProducer<T>` | state-machine-transition | Closes SPSC channel and wakes waiters. | Medium |
| `spsc_ring_buffer.rs:320` | `SpscConsumer<T>` | state-machine-transition | Closes SPSC channel and wakes waiters. | Medium |
| `spsc_ring_buffer.rs:458` | `SpmcProducer<T>` | state-machine-transition | Closes SPMC channel and wakes all waiters. | Medium |
| `spsc_ring_buffer.rs:560` | `SpmcConsumer<T>` | state-machine-transition | Closes the whole SPMC channel when any consumer drops. | High: one consumer drop appears to terminate the producer and all consumers. |
| `mcp_tools.rs:154` | `McpTxContractLockGuard` | resource-cleanup | Unlocks file and removes in-process lock key. | Low |
| `mcp_tools.rs:5991` | `TxRunWeztermOverrideGuard` | cleanup-on-error | Clears test WezTerm override. | Low, test-only |
| `mcp_tools.rs:6388` | `CassToolTestEnv` | cleanup-on-error | Clears test CASS binary override. | Low, test-only |
| `restore_scrollback.rs:186` | `InjectionGuard` | state-machine-transition | Removes pane IDs from suppressed-output set. | Medium: suppression is semantic state, but the operation is local and idempotent. |
| `recorder_storage.rs:795` | `InFlightGuard<'_>` | resource-cleanup | Decrements in-flight append counter. | Low |
| `snapshot_engine.rs:418` | `InProgressGuard<'_>` | cleanup-on-error | Clears snapshot `in_progress` flag on every exit path. | Low |
| `lock.rs:156` | `WatcherLock` | resource-cleanup | Removes watcher lock metadata; OS file lock releases with file handle. | Low |
| `pool.rs:397` | `PoolAcquireResult<C>` | resource-cleanup | Documents permit release if not moved to `PoolAcquireGuard`; actual release is owned by permit drop. | Low |
| `storage.rs:13712` | `PooledReadConn` | resource-cleanup / cleanup-on-error | Returns autocommit read connections to global pool; discards non-autocommit connections. | Medium: correct rollback safety, but destructor controls pool reuse policy. |
| `safe_channel.rs:601` | `Reservation<T>` | cleanup-on-error | Rolls back unresolved item to queue front and wakes waiters. | Medium: cancellation-safety contract depends on Drop ordering. |
| `vendored/mux_client.rs:2125` | `PaneOutputSubscription` | state-machine-transition | Sends cancel signal to background subscription task. | Medium: cancellation is signalled, but completion is not awaited. |

## Fragile Or Duplicate Patterns

1. Thread/task shutdown in Drop:
   `SemanticAnomalyWatchdog`, `StreamingTasks`, and `PaneOutputSubscription`
   all perform logical shutdown from destructors. Only the watchdog waits for
   completion, and it waits without a timeout.

2. Drop-time data finalization:
   `FrameWriter` flushes buffered frames from Drop. This prevents common data
   loss, but it cannot report I/O failure. Recorder shutdown needs an explicit
   finalization path that callers are expected to await/call before Drop.

3. Whole-channel close on endpoint Drop:
   SPSC endpoint drop is expected to close a single-producer/single-consumer
   channel. The SPMC consumer Drop is more fragile because any consumer drop
   flips the shared closed flag for every participant.

4. Global state mutation in Drop:
   `SessionGuard`, test override guards, and `InjectionGuard` mutate global or
   shared state. The test guards are low-risk; `SessionGuard` is higher-risk
   because it leaves the terminal session and clears the output gate.

5. Duplicate guard idioms:
   `ConnectionPermit`, `InFlightGuard`, `InProgressGuard`, `PoolAcquireResult`,
   and both channel reservation guards are all scoped counter/permit patterns.
   These are generally sound, but they are easy to over-interpret as logical
   completion. They should be documented as rollback/release guards only.

## High-Risk Follow-Ons

| Bead | Site | Requested follow-up |
| --- | --- | --- |
| `ft-5vje2` | `SemanticAnomalyWatchdog` | Add explicit bounded shutdown/finalize API; keep Drop as best-effort fallback and avoid unbounded destructor joins. |
| `ft-gi85k` | `FrameWriter` | Add explicit `finish`/`close` path that reports flush errors; make Drop best-effort only and update recorder shutdown callers. |
| `ft-y9z19` | `SpmcConsumer<T>` | Verify whether dropping one SPMC consumer should close the whole channel; if not, replace Drop with per-consumer unregister/wake semantics. |
| `ft-sz1sh` | `SessionGuard<S>` | Add explicit terminal-session leave/output-gate shutdown path and tests for panic/cancel ordering; keep Drop as fallback. |

## Recommended Contract

- Resource cleanup in Drop is fine for file metadata, counters, permits, and
  rollback guards.
- Logical lifecycle transitions should expose an explicit method such as
  `shutdown`, `finish`, `leave`, or `cancel_and_join`.
- Drop may call the explicit method only when that method is synchronous,
  bounded, idempotent, and cannot report useful errors. Otherwise Drop should
  only perform best-effort cleanup and log/metric the fallback path.
- Cancellation tests should assert finalization invariants on the explicit
  lifecycle methods, not on lexical scope alone.

## Verification

This bead is an audit/doc bead. The supporting scan was:

```bash
rg -n "^impl(?:<[^>]+>)?\s+Drop\s+for\s+|impl\s+Drop\s+for\s+" \
  crates/frankenterm-core/src -g'*.rs'
```
