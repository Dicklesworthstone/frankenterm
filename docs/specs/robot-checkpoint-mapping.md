# Robot Checkpoint Spec Mapping

Spec: `robot-checkpoint.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `CheckpointWorld` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:85` | Snapshot table, session state, emitted events. |
| `SessionView` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:102` | Current content plus the latest checkpoint pointer. |
| `CheckpointAction` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:168` | Save, rollback, content mutation, failure injection, and list actions. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Save` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:233` | Content-addressed save and duplicate detection. |
| `Rollback` / `RollbackDryRun` / `RollbackDenied` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:269` | Approval-token and target existence checks. |
| `MutateContent` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:303` | External content mutation between snapshots. |
| `SaveFail` / `RollbackFail` / `List` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:313` | Atomic failure and pure-read surfaces. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `NoOrphanCheckpoint` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:367` | Every session pointer resolves to `snapshots`. |
| `NoDoubleSaveOnSameContent` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:367` | One checkpoint id per content hash. |
| `AfterSavePointerIsSet` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:367` | Save updates the session checkpoint pointer. |
| `SaveLandsContent` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs:233` | Saved checkpoint content matches the session content at save time. |

## TLC Configuration

Config: `robot-checkpoint.cfg`

The deterministic smoke model uses one session, two content values, and a small
`MaxSteps` bound. Larger checkpoint sweeps should keep this file unchanged and
record their expanded constants in a separate proof artifact.
