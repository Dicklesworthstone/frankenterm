# Capture-Fairness Scheduler Spec Mapping

Spec: `capture-fairness-scheduler.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `Panes` / `selected` / `serviced` | `crates/frankenterm-core/src/tailer.rs:1178` | Abstracts `TailerSupervisor::spawn_ready` over the set of ready polling panes and started poll tasks. |
| `HighPanes`, `LowPrimaryPanes`, `LowSecondaryPanes` | `crates/frankenterm-core/src/tailer.rs:171` | Mirrors `CapturePriorityTier::from_priority`, where priorities `0..=50` are high and `51..=u32::MAX` are low. |
| `high_offset` | `crates/frankenterm-core/src/tailer.rs:1131` | Abstracts equal-priority round-robin rotation before scheduler selection. |
| `low_floor_offset` | `crates/frankenterm-core/src/tailer.rs:519` | Abstracts the low-tier floor cursor used when multiple low-priority subtier values are ready under high-tier pressure. |
| `backpressure` / `overflow_pending` | `crates/frankenterm-core/src/tailer.rs:1545` | Mirrors consecutive backpressure accounting and the `overflow_gap_pending` flag. |
| `last_reason` | `crates/frankenterm-core/src/tailer.rs:100` | Uses the stable `CaptureSkipReason` vocabulary for budget, backpressure, overflow, and terminal states. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `FairRound` | `crates/frankenterm-core/src/tailer.rs:1205` | Models ready-pane filtering, deterministic priority ordering, equal-priority rotation, selection, and completed poll-task service. |
| `FairRound` | `crates/frankenterm-core/src/tailer.rs:647` | Models the 20% low-tier floor for `effective_limit >= 2`. |
| `FairRound` | `crates/frankenterm-core/src/tailer.rs:668` | Models low-subtier floor rotation under high-tier contention. |
| `ExhaustCaptureBudget` | `crates/frankenterm-core/src/tailer.rs:626` | Abstracts a zero capture-token window that returns no selected panes and records capture-budget exhaustion. |
| `ExhaustByteBudget` | `crates/frankenterm-core/src/tailer.rs:1189` | Abstracts the supervisor's fail-closed byte-budget precheck before ready work is admitted. |
| `Backpressure` | `crates/frankenterm-core/src/tailer.rs:1545` | Models `PollOutcome::Backpressure`, consecutive counter increments, and overflow-gap pending state. |
| `EnterShutdown` / `EnterCancelled` | `crates/frankenterm-core/src/tailer.rs:1182` | Shutdown stops admission and records terminal non-service. Cancellation is the same no-new-work terminal abstraction for this model. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `NoDuplicateInflight` | `crates/frankenterm-core/src/tailer.rs:1209` and `:1307` | Panes already in `capturing_panes` are skipped and selected panes are inserted before the poll task starts. |
| `SelectionWithinPermits` | `crates/frankenterm-core/src/tailer.rs:1231` | `available_permits` bounds the selected set. |
| `FairSelectionRespectsTierShares` | `crates/frankenterm-core/src/tailer.rs:647` and `:658` | Checks high-tier precedence after reserving the low-tier floor. |
| `SingleSlotPriorityNoFloor` | `crates/frankenterm-core/src/tailer.rs:647` | Captures the documented no-floor behavior when only one slot is available. |
| `BackpressureThresholdMarksOverflow` | `crates/frankenterm-core/src/tailer.rs:1554` | Sustained backpressure reaches the configured threshold and marks `overflow_gap_pending`. |
| `OverflowGapClearsBackpressure` | `crates/frankenterm-core/src/tailer.rs:1565` | Emitted overflow GAP clears pending overflow state and resets the consecutive counter. |
| `BudgetDeferralsAreExplicit` | `crates/frankenterm-core/src/tailer.rs:1237` and `:1248` | Non-selected ready panes receive an explicit budget or permit reason. |
| `TerminalStatesDoNotAdmit` | `crates/frankenterm-core/src/tailer.rs:1182` | Shutdown records `CaptureSkipReason::Shutdown` and returns without scheduling. |
| `NoEligiblePaneStarvesAtFairBound` | `crates/frankenterm-core/src/tailer.rs:3150` and `:3242` | Cross-checks the same fairness claim as the Rust scheduler model: uninterrupted fair rounds service every modeled pane, including the lower low-priority subtier. |

## TLC Configuration

Config: `capture-fairness-scheduler.cfg`

The deterministic smoke model uses four panes: two high-priority panes, one
primary low-priority pane, and one lower low-priority pane. `AvailablePermits =
2` forces contention so each fair round admits one high pane and one low-floor
pane. `BackpressureThreshold = 5` matches `OVERFLOW_BACKPRESSURE_THRESHOLD`.
`MaxFairRounds = 4` is larger than the two-round minimum needed for this
configuration, leaving room for offset movement while still keeping TLC state
space small. The release-bundle proof slot is
`proofs/capture-fairness-scheduler.json`.
