# Runtime Async Cancel Spec Mapping

Spec: `runtime-async-cancel.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `PanicPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:88` | `Mutex::lock_with_cx` preserves the infallible lock contract by panicking on underlying acquire errors, including Cx cancellation. |
| `PanicPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:159` | `RwLock::read_with_cx` and `write_with_cx` intentionally share the mutex panic-on-cancel contract. |
| `ErrCancelPrimitives` / `WatcherRequiredPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:329` | `Semaphore::acquire_with_cx` maps pre-cancel to `AcquireError::Cancelled` and documents the mid-flight select-race requirement. |
| `ErrCancelPrimitives` / `WatcherRequiredPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:436` | `mpsc::Receiver::recv(cx)` observes pre-cancel but needs the caller-side select race for already-suspended waits. |
| `ErrCancelPrimitives` / `WatcherRequiredPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:479` | `watch::Receiver::changed(cx)` uses the same pre-cancel and mid-flight watcher contract. |
| `ErrCancelPrimitives` / `WatcherRequiredPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:666` | `broadcast::Receiver::recv_with_cx` maps asupersync cancelled receives to `broadcast::RecvError::Cancelled`. |
| `ErrCancelPrimitives` / `WatcherRequiredPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:2498` | `oneshot_recv_with_cx` maps Cx-cancelled asupersync receives to a stringified cancellation error. |
| `notify` | `crates/frankenterm-core/src/runtime_async.rs:779` | `Notify` is re-exported directly; the cancellation model treats dropped waiters as non-mutating wait cancellation. |
| `BudgetPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:2135` | `sleep_with_cx` observes Cx budget deadlines through `budget_sleep`, not direct `is_cancel_requested` checks. |
| `BudgetPrimitives` | `crates/frankenterm-core/src/runtime_async.rs:2171` | `timeout_with_cx` observes Cx budget deadlines through `budget_timeout`; direct cancellation must be checked by callers before wrapping. |
| `joinset` | `crates/frankenterm-core/src/runtime_async.rs:1014` | `JoinSet::join_next_with_cx` checks Cx before polling and during every poll loop. |
| `spawn_blocking` | `crates/frankenterm-core/src/runtime_async.rs:2222` | `spawn_blocking_with_cx` gates pre-cancel before spawn and select-races mid-flight cancellation, explicitly orphaning the blocking closure until it returns. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `StartWait` | `crates/frankenterm-core/src/runtime_async.rs:104` | A pending wait represents entering one Cx-aware primitive call before its await resolves. |
| `WakeResource` | `crates/frankenterm-core/src/runtime_async.rs:349` | External wake/resource availability abstracts permit release, channel send, notify wake, task completion, or timer completion. |
| `PreCancel` | `crates/frankenterm-core/src/runtime_async.rs:1058` | Pre-flight cancellation maps to each primitive's documented eager-cancel result before resource mutation. |
| `MidCancel` | `crates/frankenterm-core/src/runtime_async.rs:1064` | JoinSet observes mid-flight cancellation on the poll loop; delegated asupersync waits require the caller-side watcher. |
| `CompleteWait` | `crates/frankenterm-core/src/runtime_async.rs:1001` | Successful completion consumes only the resource being waited for: a permit, queued message, join handle, waiter wake, timer, or blocking result. |
| `CancelRecord` / `SuccessRecord` | `crates/frankenterm-core/src/runtime_async.rs:4447` | Runtime tests record the same cancel and success classes through pre-cancel, mid-flight select-race, and successful completion assertions. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `CancelRowsDoNotMutatePermitsOrMessages` | `crates/frankenterm-core/tests/loom_sync.rs:581` | Loom semaphore proofs pin capacity restoration and reject permit leaks; channel Loom tests pin no phantom delivery under cancellation. |
| `JoinSetCancelDoesNotDropHandles` | `crates/frankenterm-core/src/runtime_async.rs:1058` | `join_next_with_cx` returns a synthesized cancel error without `swap_remove` when Cx cancellation fires. |
| `SpawnBlockingMidCancelIsExplicitOrphan` | `crates/frankenterm-core/src/runtime_async.rs:2234` | Mid-flight cancellation returns promptly while the blocking closure continues until natural completion. |
| `WatcherRequiredForDelegatedMidCancel` | `crates/frankenterm-core/src/runtime_async.rs:6075` | The channel/semaphore mid-flight tests use a select-race watcher because the delegated waits do not register a Cx cancel waker. |
| `DirectPreCancelResultMatchesContract` | `crates/frankenterm-core/src/runtime_async.rs:6588` | Pre-cancel tests assert oneshot, broadcast, mpsc, semaphore, watch, and JoinSet return their documented cancellation shapes. |
| `BudgetPrimitivesNeverClaimDirectCancel` | `crates/frankenterm-core/src/runtime_async.rs:5377` | The timer tests distinguish budget-deadline observation from direct cancellation observation. |
| `SuccessRowsConsumeOnlyOwnedResources` | `docs/proofs/runtime-async-cancel-traces.md:11` | The cancel-trace catalog states cancellation suppresses future observations without duplicating or erasing already-linearized resources. |

## TLC Configuration

Config: `runtime-async-cancel.cfg`

The deterministic smoke model enumerates the twelve runtime_async primitive
families named by `ft-tf6g3.18.2`: `Mutex`, `RwLock`, `Semaphore`, `mpsc`,
`watch`, `broadcast`, `oneshot`, `Notify`, `sleep`, `timeout`, `JoinSet`, and
`spawn_blocking`. `MaxHistory = 2` keeps TLC bounded while still exploring
back-to-back waits that reset the resource counters between terminal rows. The
release-bundle proof slot is `proofs/runtime-async-cancel.json`.
