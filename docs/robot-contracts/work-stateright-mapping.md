# Robot Work Stateright Mapping

Model crate: `tests/robot_work_atomicity_model`

## Rust Correspondence

| Stateright symbol | Rust target | Notes |
|---|---|---|
| `RobotWorkAtomicityModel` | `tests/robot_work_atomicity_model/src/lib.rs:13` | Actual `stateright::Model` wrapper for the robot work state machine. |
| `RobotWorkState` | `tests/robot_work_atomicity_model/src/lib.rs:7` | Stateright state pairs the existing `WorkWorld` with an accumulated violation bit. |
| `WorkWorld` | `crates/frankenterm-core/src/robot_work_state_machine.rs:86` | Existing model of the `work_claims` table, live agents, and emitted event trace. |
| `WorkAction` | `crates/frankenterm-core/src/robot_work_state_machine.rs:154` | Claim, complete, release, read, failure-injection, and crash/restart actions. |
| `apply_action` | `crates/frankenterm-core/src/robot_work_state_machine.rs:211` | Existing transition function reused directly by the Stateright model. |
| `check_invariants` | `crates/frankenterm-core/src/robot_work_state_machine.rs:387` | Existing invariant checker reused by `next_state` to avoid a second divergent model. |

## Action Mapping

| Stateright action source | Rust target | Notes |
|---|---|---|
| `Model::init_states` | `tests/robot_work_atomicity_model/src/lib.rs:34` | Seeds two work items and two agents for deterministic CI coverage. |
| `Model::actions` | `tests/robot_work_atomicity_model/src/lib.rs:41` | Enumerates `list`, `status`, `claim`, `complete`, `release`, failure actions, and crash/restart. |
| `Model::next_state` | `tests/robot_work_atomicity_model/src/lib.rs:77` | Applies the existing transition and records any invariant failure as sticky state. |
| `within_boundary` | `tests/robot_work_atomicity_model/src/lib.rs:92` | Bounds the event trace so BFS remains finite and repeatable. |
| CLI proof runner | `tests/robot_work_atomicity_model/src/main.rs:4` | Runs the same Stateright BFS and emits machine-readable state counts. |

## Invariant Mapping

| Stateright property | Rust target | Notes |
|---|---|---|
| `no_safety_violation` | `tests/robot_work_atomicity_model/src/lib.rs:111` | Fails if `check_invariants` ever reports `DoubleClaim`, `CompletedRegressed`, `NonOwnerMutation`, or `CrashLeftClaimedRow`. |
| `completed_events_are_durable` | `tests/robot_work_atomicity_model/src/lib.rs:115` | Every emitted completion event must still correspond to a completed row with the same owner. |
| `claimed_rows_have_one_owner` | `tests/robot_work_atomicity_model/src/lib.rs:130` | Structural single-owner property for the claim table. |
| `claim_is_reachable` | `tests/robot_work_atomicity_model/src/lib.rs:145` | Positive reachability check so the model explores real claim transitions. |
| `completion_is_reachable` | `tests/robot_work_atomicity_model/src/lib.rs:153` | Positive reachability check for durable completion. |
| `crash_auto_release_is_reachable` | `tests/robot_work_atomicity_model/src/lib.rs:161` | Positive reachability check for crash/restart auto-release behavior. |
| Always-on regression test | `tests/robot_work_atomicity_model/src/lib.rs:179` | Runs BFS through Stateright and asserts all properties. |

## CI Configuration

Workflow: `.github/workflows/robot-work-atomicity-model.yml`

The workflow is path-scoped to the model crate, robot work state machine,
robot contract docs, and Stateright mapping files. It runs:

```bash
cargo test --manifest-path tests/robot_work_atomicity_model/Cargo.toml
cargo run --manifest-path tests/robot_work_atomicity_model/Cargo.toml
```

The release-bundle proof slot is `proofs/robot-work-atomicity.json`.
