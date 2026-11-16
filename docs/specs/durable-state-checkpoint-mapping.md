# Durable State Checkpoint Spec Mapping

Spec: `durable-state-checkpoint.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `CheckpointRow` | `crates/frankenterm-core/src/durable_state.rs:27` | Abstracts `Checkpoint` fields relevant to rollback atomicity: ID, snapshot, topology, trigger, `rolled_back`, and validity. |
| `RollbackRow` | `crates/frankenterm-core/src/durable_state.rs:73` | Abstracts `RollbackRecord` with target and pre-rollback checkpoint IDs. |
| `checkpoints` / `next_id` / `rollback_history` | `crates/frankenterm-core/src/durable_state.rs:98` | Manager-owned durable state. |
| `registry` | `crates/frankenterm-core/src/durable_state.rs:154` | The live `LifecycleRegistry` passed to checkpoint and rollback operations. |
| `topology` | `crates/frankenterm-core/src/durable_state.rs:166` | The optional live topology snapshot restored alongside lifecycle registry state. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Checkpoint` | `crates/frankenterm-core/src/durable_state.rs:163` | `checkpoint_with_topology` allocates `next_id`, snapshots registry/topology, pushes a checkpoint, and enforces retention. |
| `MutateRegistry` | `crates/frankenterm-core/src/durable_state.rs:660` | Test helper `make_registry` stands in for live lifecycle changes between checkpoints. |
| `MutateTopology` | `crates/frankenterm-core/src/durable_state.rs:673` | Test helper topology snapshots stand in for live mux topology drift between checkpoints. |
| `CorruptCheckpoint` | `crates/frankenterm-core/src/durable_state.rs:1202` | Test-only corruption used to force rollback validation failure. |
| `RollbackSuccess` | `crates/frankenterm-core/src/durable_state.rs:263` | `rollback_with_topology` validates the target, rebuilds the registry, persists a pre-rollback checkpoint, commits registry/topology, marks newer checkpoints, and records rollback history. |
| `RollbackNotFound` | `crates/frankenterm-core/src/durable_state.rs:270` | Missing target exits before mutation. |
| `RollbackAlreadyRolledBack` | `crates/frankenterm-core/src/durable_state.rs:277` | Previously superseded target exits before mutation. |
| `RollbackInvalid` | `crates/frankenterm-core/src/durable_state.rs:318` | Invalid checkpoint record exits before pre-rollback checkpoint, live registry mutation, ID increment, or history append. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `NextIdExceedsStoredIds` | `crates/frankenterm-core/src/durable_state.rs:171` | Checkpoint IDs are monotonic and allocated from `next_id`. |
| `CheckpointCapturesPreviousState` | `crates/frankenterm-core/src/durable_state.rs:174` | Checkpoint rows capture the pre-action registry and topology snapshot. |
| `RollbackSuccessRestoresTarget` | `crates/frankenterm-core/src/durable_state.rs:345` | Successful rollback commits the rebuilt registry and target topology. |
| `RollbackSuccessCreatesPreRollbackCheckpoint` | `crates/frankenterm-core/src/durable_state.rs:329` | Validation succeeds before the pre-rollback checkpoint is appended. |
| `RollbackSuccessMarksIntermediateCheckpoints` | `crates/frankenterm-core/src/durable_state.rs:348` | Checkpoints newer than the target, except the pre-rollback checkpoint, are marked `rolled_back`. |
| `FailedRollbackDoesNotMutateManagerOrRegistry` | `crates/frankenterm-core/src/durable_state.rs:1194` | Regression test asserts registry snapshot, checkpoint count, `next_id`, and rollback history do not change after invalid checkpoint failure. |
| `RollbackHistoryReferencesPreRollbackCheckpoint` | `crates/frankenterm-core/src/durable_state.rs:355` | Every successful rollback record points to the checkpoint created from the pre-rollback state. |
| `SuccessfulRollbackOnlyUsesValidLiveTarget` | `crates/frankenterm-core/src/durable_state.rs:277` | Rolled-back targets are rejected; invalid records are rejected before commit. |

## TLC Configuration

Config: `durable-state-checkpoint.cfg`

The deterministic smoke model uses two entities, two live states plus the
`absent` sentinel, three topology values, and `MaxSteps = 3`. That bound is
large enough to cover checkpoint creation, live drift, successful rollback,
intermediate checkpoint marking, test-only corruption, and invalid rollback
failure atomicity. The release-bundle proof slot is
`proofs/durable-state-checkpoint.json`.
