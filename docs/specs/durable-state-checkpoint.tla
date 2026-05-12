------------------------ MODULE DurableStateCheckpoint ------------------------
(*
  TLA+ specification for DurableStateManager checkpoint and rollback
  atomicity.

  The model abstracts lifecycle registry snapshots as a total entity->state map
  with "absent" as the missing-entity sentinel. It focuses on the durable_state
  contract in crates/frankenterm-core/src/durable_state.rs:

  - checkpoints capture the current registry/topology and allocate monotonic IDs
  - rollback validates the target before mutating live state
  - successful rollback persists a pre-rollback checkpoint before commit
  - checkpoints newer than the target are marked rolled_back
  - failed rollback attempts leave manager and registry state unchanged

  Run with TLC:
    java -jar tla2tools.jar -workers auto DurableStateCheckpoint.tla
*)

EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS Entities, States, MaxSteps

ASSUME
    /\ Cardinality(Entities) \in 1..2
    /\ Cardinality(States) \in 1..2
    /\ MaxSteps \in 1..4

EntityStates == States \cup {"absent"}
TopologyValues == {"none", "base", "expanded"}
TriggerValues == {"manual", "pre_rollback"}
ActionNames == {
    "init",
    "checkpoint",
    "mutate_registry",
    "mutate_topology",
    "corrupt_checkpoint",
    "rollback_success",
    "rollback_not_found",
    "rollback_already_rolled_back",
    "rollback_invalid"
}
IdSpace == 1..(MaxSteps + 2)

RegistryView == [Entities -> EntityStates]

CheckpointRow == [
    id          : IdSpace,
    registry    : RegistryView,
    topology    : TopologyValues,
    trigger     : TriggerValues,
    rolled_back : BOOLEAN,
    valid       : BOOLEAN
]

RollbackRow == [
    target_id : IdSpace,
    from_id   : IdSpace
]

VARIABLES checkpoints, next_id, registry, topology, rollback_history,
          prev_checkpoints, prev_next_id, prev_registry, prev_topology,
          prev_rollback_history, last_action, step

vars == <<checkpoints, next_id, registry, topology, rollback_history,
          prev_checkpoints, prev_next_id, prev_registry, prev_topology,
          prev_rollback_history, last_action, step>>

----------------------------------------------------------------------------
\* Domain helpers
----------------------------------------------------------------------------

CheckpointIds(cps) == {cps[i].id : i \in DOMAIN cps}

UniqueCheckpointIds(cps) ==
    \A i, j \in DOMAIN cps : i # j => cps[i].id # cps[j].id

HasCheckpoint(cps, id) == id \in CheckpointIds(cps)

CheckpointIndex(cps, id) ==
    CHOOSE i \in DOMAIN cps : cps[i].id = id

CheckpointById(cps, id) == cps[CheckpointIndex(cps, id)]

LatestCheckpoint(cps) == cps[Len(cps)]

LastRollback == rollback_history[Len(rollback_history)]

MarkRolledBack(cps, target_id, from_id) ==
    [i \in DOMAIN cps |->
        LET cp == cps[i] IN
        IF /\ cp.id > target_id
           /\ cp.id # from_id
        THEN [cp EXCEPT !.rolled_back = TRUE]
        ELSE cp
    ]

PreCheckpoint(from_id, current_registry, current_topology, target_id) == [
    id          |-> from_id,
    registry    |-> current_registry,
    topology    |-> current_topology,
    trigger     |-> "pre_rollback",
    rolled_back |-> FALSE,
    valid       |-> TRUE
]

ManualCheckpoint(id, current_registry, current_topology) == [
    id          |-> id,
    registry    |-> current_registry,
    topology    |-> current_topology,
    trigger     |-> "manual",
    rolled_back |-> FALSE,
    valid       |-> TRUE
]

RecordPrevious(action) ==
    /\ prev_checkpoints' = checkpoints
    /\ prev_next_id' = next_id
    /\ prev_registry' = registry
    /\ prev_topology' = topology
    /\ prev_rollback_history' = rollback_history
    /\ last_action' = action

TypeOK ==
    /\ checkpoints \in Seq(CheckpointRow)
    /\ UniqueCheckpointIds(checkpoints)
    /\ next_id \in 1..(MaxSteps + 2)
    /\ registry \in RegistryView
    /\ topology \in TopologyValues
    /\ rollback_history \in Seq(RollbackRow)
    /\ prev_checkpoints \in Seq(CheckpointRow)
    /\ UniqueCheckpointIds(prev_checkpoints)
    /\ prev_next_id \in 1..(MaxSteps + 2)
    /\ prev_registry \in RegistryView
    /\ prev_topology \in TopologyValues
    /\ prev_rollback_history \in Seq(RollbackRow)
    /\ last_action \in ActionNames
    /\ step \in 0..MaxSteps

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

InitRegistry == [e \in Entities |-> "absent"]

Init ==
    /\ checkpoints = <<>>
    /\ next_id = 1
    /\ registry = InitRegistry
    /\ topology = "none"
    /\ rollback_history = <<>>
    /\ prev_checkpoints = <<>>
    /\ prev_next_id = 1
    /\ prev_registry = InitRegistry
    /\ prev_topology = "none"
    /\ prev_rollback_history = <<>>
    /\ last_action = "init"
    /\ step = 0

----------------------------------------------------------------------------
\* Actions
----------------------------------------------------------------------------

Checkpoint ==
    /\ step < MaxSteps
    /\ RecordPrevious("checkpoint")
    /\ checkpoints' = Append(
            checkpoints,
            ManualCheckpoint(next_id, registry, topology)
       )
    /\ next_id' = next_id + 1
    /\ UNCHANGED <<registry, topology, rollback_history>>
    /\ step' = step + 1

MutateRegistry(e, state) ==
    /\ step < MaxSteps
    /\ e \in Entities
    /\ state \in EntityStates
    /\ RecordPrevious("mutate_registry")
    /\ registry' = [registry EXCEPT ![e] = state]
    /\ UNCHANGED <<checkpoints, next_id, topology, rollback_history>>
    /\ step' = step + 1

MutateTopology(next_topology) ==
    /\ step < MaxSteps
    /\ next_topology \in TopologyValues
    /\ RecordPrevious("mutate_topology")
    /\ topology' = next_topology
    /\ UNCHANGED <<checkpoints, next_id, registry, rollback_history>>
    /\ step' = step + 1

CorruptCheckpoint(target_id) ==
    /\ step < MaxSteps
    /\ HasCheckpoint(checkpoints, target_id)
    /\ RecordPrevious("corrupt_checkpoint")
    /\ checkpoints' = [
            i \in DOMAIN checkpoints |->
                IF checkpoints[i].id = target_id
                THEN [checkpoints[i] EXCEPT !.valid = FALSE]
                ELSE checkpoints[i]
       ]
    /\ UNCHANGED <<next_id, registry, topology, rollback_history>>
    /\ step' = step + 1

RollbackSuccess(target_id) ==
    /\ step < MaxSteps
    /\ HasCheckpoint(checkpoints, target_id)
    /\ ~CheckpointById(checkpoints, target_id).rolled_back
    /\ CheckpointById(checkpoints, target_id).valid
    /\ RecordPrevious("rollback_success")
    /\ LET target == CheckpointById(checkpoints, target_id)
           from_id == next_id
           appended == Append(
               checkpoints,
               PreCheckpoint(from_id, registry, topology, target_id)
           )
       IN
       /\ checkpoints' = MarkRolledBack(appended, target_id, from_id)
       /\ next_id' = next_id + 1
       /\ registry' = target.registry
       /\ topology' = target.topology
       /\ rollback_history' = Append(
              rollback_history,
              [target_id |-> target_id, from_id |-> from_id]
          )
    /\ step' = step + 1

RollbackNotFound(target_id) ==
    /\ step < MaxSteps
    /\ target_id \in IdSpace
    /\ ~HasCheckpoint(checkpoints, target_id)
    /\ RecordPrevious("rollback_not_found")
    /\ UNCHANGED <<checkpoints, next_id, registry, topology, rollback_history>>
    /\ step' = step + 1

RollbackAlreadyRolledBack(target_id) ==
    /\ step < MaxSteps
    /\ HasCheckpoint(checkpoints, target_id)
    /\ CheckpointById(checkpoints, target_id).rolled_back
    /\ RecordPrevious("rollback_already_rolled_back")
    /\ UNCHANGED <<checkpoints, next_id, registry, topology, rollback_history>>
    /\ step' = step + 1

RollbackInvalid(target_id) ==
    /\ step < MaxSteps
    /\ HasCheckpoint(checkpoints, target_id)
    /\ ~CheckpointById(checkpoints, target_id).rolled_back
    /\ ~CheckpointById(checkpoints, target_id).valid
    /\ RecordPrevious("rollback_invalid")
    /\ UNCHANGED <<checkpoints, next_id, registry, topology, rollback_history>>
    /\ step' = step + 1

StutterAtBound ==
    /\ step = MaxSteps
    /\ UNCHANGED vars

Next ==
    \/ Checkpoint
    \/ \E e \in Entities, state \in EntityStates : MutateRegistry(e, state)
    \/ \E next_topology \in TopologyValues : MutateTopology(next_topology)
    \/ \E target_id \in IdSpace : CorruptCheckpoint(target_id)
    \/ \E target_id \in IdSpace : RollbackSuccess(target_id)
    \/ \E target_id \in IdSpace : RollbackNotFound(target_id)
    \/ \E target_id \in IdSpace : RollbackAlreadyRolledBack(target_id)
    \/ \E target_id \in IdSpace : RollbackInvalid(target_id)
    \/ StutterAtBound

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants
----------------------------------------------------------------------------

NextIdExceedsStoredIds ==
    \A i \in DOMAIN checkpoints : checkpoints[i].id < next_id

CheckpointCapturesPreviousState ==
    last_action = "checkpoint" =>
        /\ Len(checkpoints) = Len(prev_checkpoints) + 1
        /\ LatestCheckpoint(checkpoints).id = prev_next_id
        /\ LatestCheckpoint(checkpoints).registry = prev_registry
        /\ LatestCheckpoint(checkpoints).topology = prev_topology
        /\ LatestCheckpoint(checkpoints).trigger = "manual"
        /\ ~LatestCheckpoint(checkpoints).rolled_back

RollbackSuccessRestoresTarget ==
    last_action = "rollback_success" =>
        /\ Len(rollback_history) = Len(prev_rollback_history) + 1
        /\ LET target_id == LastRollback.target_id
               target == CheckpointById(prev_checkpoints, target_id)
           IN
           /\ registry = target.registry
           /\ topology = target.topology

RollbackSuccessCreatesPreRollbackCheckpoint ==
    last_action = "rollback_success" =>
        /\ Len(checkpoints) = Len(prev_checkpoints) + 1
        /\ LET pre == CheckpointById(checkpoints, prev_next_id) IN
           /\ pre.registry = prev_registry
           /\ pre.topology = prev_topology
           /\ pre.trigger = "pre_rollback"
           /\ ~pre.rolled_back
           /\ pre.valid

RollbackSuccessMarksIntermediateCheckpoints ==
    last_action = "rollback_success" =>
        LET target_id == LastRollback.target_id
            from_id == LastRollback.from_id
        IN
        \A i \in DOMAIN checkpoints :
            /\ checkpoints[i].id > target_id
            /\ checkpoints[i].id # from_id
            => checkpoints[i].rolled_back

FailedRollbackDoesNotMutateManagerOrRegistry ==
    last_action \in {
        "rollback_not_found",
        "rollback_already_rolled_back",
        "rollback_invalid"
    } =>
        /\ checkpoints = prev_checkpoints
        /\ next_id = prev_next_id
        /\ registry = prev_registry
        /\ topology = prev_topology
        /\ rollback_history = prev_rollback_history

RollbackHistoryReferencesPreRollbackCheckpoint ==
    \A i \in DOMAIN rollback_history :
        /\ HasCheckpoint(checkpoints, rollback_history[i].from_id)
        /\ CheckpointById(checkpoints, rollback_history[i].from_id).trigger =
           "pre_rollback"

SuccessfulRollbackOnlyUsesValidLiveTarget ==
    last_action = "rollback_success" =>
        LET target_id == LastRollback.target_id
            target == CheckpointById(prev_checkpoints, target_id)
        IN
        /\ target.valid
        /\ ~target.rolled_back

SafetyInvariants ==
    /\ TypeOK
    /\ NextIdExceedsStoredIds
    /\ CheckpointCapturesPreviousState
    /\ RollbackSuccessRestoresTarget
    /\ RollbackSuccessCreatesPreRollbackCheckpoint
    /\ RollbackSuccessMarksIntermediateCheckpoints
    /\ FailedRollbackDoesNotMutateManagerOrRegistry
    /\ RollbackHistoryReferencesPreRollbackCheckpoint
    /\ SuccessfulRollbackOnlyUsesValidLiveTarget

----------------------------------------------------------------------------
\* Liveness/progress block
----------------------------------------------------------------------------

\* This is intentionally a safety-only model. The production durable_state
\* contract is atomicity: checkpoint and rollback either commit the documented
\* state transition or leave manager and registry state untouched. Progress for
\* callers is covered by the bounded Rust tests and by higher-level robot
\* checkpoint/work-family models.

=============================================================================
