# TX Kill-Switch Spec Mapping

Spec: `tx-killswitch.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `MissionTxState` | `crates/frankenterm-core/src/plan.rs:2726` | Production transaction lifecycle enum. |
| `KillLevel` | `crates/frankenterm-core/src/plan.rs:2778` | Production kill-switch levels. |
| `KillSwitchModelState` | `crates/frankenterm-core/src/tx_killswitch_model.rs:78` | Formal model state: tx state, kill switch, committed/compensated steps. |
| `KillSwitchAction` | `crates/frankenterm-core/src/tx_killswitch_model.rs:117` | Legal observable transitions. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Plan` / `Prepare` / `BeginCommit` | `crates/frankenterm-core/src/tx_killswitch_model.rs:150` | Forward progress actions, disabled under HardStop. |
| `CommitStep` / `FinishCommit` / `FailCommit` | `crates/frankenterm-core/src/tx_killswitch_model.rs:150` | Commit loop and failure path. |
| `BeginCompensate` / `CompensateStep` / `FinishCompensate` / `RollBack` | `crates/frankenterm-core/src/tx_killswitch_model.rs:150` | Recovery actions that remain available under HardStop. |
| `FlipKillSwitch` | `crates/frankenterm-core/src/tx_killswitch_model.rs:150` | Operator kill-switch transition. |
| `Next` transition application | `crates/frankenterm-core/src/tx_killswitch_model.rs:235` | Pure transition function used by the state-space harness. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `NoSilentPartialCommit` | `crates/frankenterm-core/src/tx_killswitch_model.rs:266` | Committed state implies every step was committed. |
| `NoOrphanCompensation` | `crates/frankenterm-core/src/tx_killswitch_model.rs:266` | Compensation is a subset of committed steps. |
| `StepIdsInBound` | `crates/frankenterm-core/src/tx_killswitch_model.rs:266` | Step ids stay within `0..step_count`. |
| `EventuallyDrained` / `HardStopAdmitsProgress` | `crates/frankenterm-core/src/tx_killswitch_model.rs:338` | HardStop admits a finite path to a drained state. |

## TLC Configuration

Config: `tx-killswitch.cfg`

The deterministic smoke model uses `StepCount = 2`, matching the small
always-on Rust state-space proof. Wider runs can raise the bound in a copied
artifact config and record the state-count delta.
