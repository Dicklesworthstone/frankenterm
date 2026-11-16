# Mux Session Reentry Spec Mapping

Spec: `mux-session-reentry.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `EventKinds` | `frankenterm/mux/src/lib.rs:119` | `MuxNotification` is the production notification domain modeled by the spec. |
| `subscribers` | `frankenterm/mux/src/lib.rs:201` | Mux subscriber map protected by an `RwLock`. |
| `pending_outputs` | `frankenterm/mux/src/lib.rs:167` | Pending pane-output queue with a set for dedupe and a vector for drain order. |
| `per_pane` | `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:271` | Per-session cached pane state keyed by pane id. |
| `terminal_panes` / `terminal_windows` | `crates/frankenterm-core/src/headless_mux_server.rs:384` | Headless status exposes pane, session, and window lifecycle counts. |
| Lifecycle registry | `crates/frankenterm-core/src/headless_mux_server.rs:530` | The headless server exposes lifecycle registry access for session topology accounting. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `AddPane` / `DuplicateAddPaneNoop` | `frankenterm/mux/src/lib.rs:1149` | `add_pane` returns early when a pane id is already registered, otherwise inserts and emits `PaneAdded`. |
| `RemovePane` | `frankenterm/mux/src/lib.rs:1195` | Pane removal kills the pane, discards pending pane output, emits `PaneRemoved`, and recomputes counts. |
| `RemoveWindow` | `frankenterm/mux/src/lib.rs:1235` | Window removal removes owned tabs and panes before emitting `WindowRemoved`. |
| `ClientTrackPane` | `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:298` | Client request paths intentionally create first-use `per_pane` state. |
| `TrackedPaneOutput` / `StalePaneOutputNoop` | `crates/frankenterm-mux-server-impl/src/dispatch.rs:911` | Pane-output notifications call the non-inserting tracked-push path. |
| Stale alert notification | `crates/frankenterm-mux-server-impl/src/dispatch.rs:923` | Alert notifications use the non-inserting accessor so removed panes are not recreated. |
| `FlushPaneOutput` | `frankenterm/mux/src/lib.rs:1049` | The mux drains pending pane-output notifications in batches until the queue is empty. |
| `Subscribe` | `frankenterm/mux/src/lib.rs:898` | Subscription allocates an id and inserts a callback. |
| `GuardDrop` | `crates/frankenterm-mux-server-impl/src/dispatch.rs:373` | Dispatch subscription guard unregisters the mux callback on drop. |
| `BeginDispatch` / callback actions / `EndDispatch` | `frankenterm/mux/src/lib.rs:981` | `dispatch_notification` snapshots subscribers before invoking callbacks and removes dead subscribers after fanout. |
| Dispatch loop subscription | `crates/frankenterm-mux-server-impl/src/dispatch.rs:847` | The session dispatcher subscribes once and routes notifications into a bounded item queue. |
| `PaneRemoved` terminal queue event | `crates/frankenterm-mux-server-impl/src/dispatch.rs:915` | The dispatcher clears cached state and queues a `PaneRemoved` PDU. |
| Headless status count | `crates/frankenterm-core/src/headless_mux_server.rs:1093` | Status counts panes, sessions, and windows from the lifecycle registry. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `PaneLiveRegisteredAtMostOnce` | `frankenterm/mux/src/lib.rs:1149` | Duplicate live pane ids return before insertion or notification; later id reuse is modeled as a fresh live registration after removal. |
| `PerPaneOnlyForLivePanes` | `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:317` | Removed panes clear cached per-pane state. |
| `PendingOutputOnlyForLivePanes` | `frankenterm/mux/src/lib.rs:1040` | Pending pane-output notifications are discarded on pane removal. |
| `RemovedPaneHasTerminalQueueEvent` | `crates/frankenterm-mux-server-impl/src/dispatch.rs:915` | A `PaneRemoved` notification queues the terminal PDU after state cleanup. |
| `DispatchSnapshotWellFormed` | `frankenterm/mux/src/lib.rs:981` | Callbacks are invoked from a snapshot outside the subscriber lock, and dead callbacks are removed after fanout. |
| `SubscribersStayBounded` | `frankenterm/mux/src/lib.rs:898` | Subscriber ids come from the registered subscriber set and are removed through `unsubscribe`. |
| `TerminalEventsStayStable` | `crates/frankenterm-core/src/headless_mux_server.rs:1093` | Headless status derives stable pane/session/window counts from lifecycle registry state. |
| Unsubscribe reentry cross-check | `frankenterm/mux/src/lib.rs:2087` | `notification_callbacks_can_unsubscribe_without_lock_reentrancy` exercises callback-side unsubscribe behavior. |
| Pane-output reentry cross-check | `frankenterm/mux/src/lib.rs:2260` | `pane_output_reentrant_enqueue_is_drained_before_returning` exercises reentrant pane-output enqueue and drain behavior. |
| Dead subscriber cross-check | `frankenterm/mux/src/lib.rs:2653` | `panicking_subscriber_is_removed_and_does_not_poison_others` covers dead-subscriber cleanup. |
| Removed-pane cache cross-check | `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:2001` | `tracked_pane_push_does_not_recreate_removed_pane_state` covers stale mux notification behavior. |
| Missing mux/pane cleanup cross-check | `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:3616` | Property test ensures cancel/error paths do not retain per-pane state. |

## TLC Configuration

Config: `mux-session-reentry.cfg`

The deterministic smoke model uses two panes, one window, and two subscribers.
That keeps the state space small enough for the repository TLC wrapper while
still covering duplicate pane registration, pane removal, stale pane-output and
alert notifications, callback-side unsubscribe, callback death, reentrant
pane-output enqueue, and dispatch completion. The release-bundle proof slot is
`proofs/mux-session-reentry.json`.
