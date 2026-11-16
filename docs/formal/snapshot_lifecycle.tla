--------------------------- MODULE snapshot_lifecycle ---------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS PaneIds, Values, DefaultValue, MaxHistory
ASSUME /\ PaneIds # {}
       /\ Values # {}
       /\ DefaultValue \in Values
       /\ MaxHistory \in Nat \ {0}

PhaseSet == {"Normal", "Capturing", "Writing", "Stored", "Restoring", "Restored"}

InitPaneState == [p \in PaneIds |-> DefaultValue]

UpdateState(s, p, v) == [s EXCEPT ![p] = v]

VARIABLES
  phase,
  liveState,
  history,
  captureState,
  captureVersion,
  writeBuffer,
  hasSnapshot,
  snapshotState,
  snapshotVersion,
  preRestoreLive,
  restoreBuffer,
  appliedPanes,
  restoreTargetVersion,
  hasRestored,
  lastRestoredVersion,
  lastRestoredState,
  idempotentOk

Init ==
  /\ phase = "Normal"
  /\ liveState = InitPaneState
  /\ history = <<liveState>>
  /\ captureState = liveState
  /\ captureVersion = 0
  /\ writeBuffer = liveState
  /\ hasSnapshot = FALSE
  /\ snapshotState = liveState
  /\ snapshotVersion = 0
  /\ preRestoreLive = liveState
  /\ restoreBuffer = liveState
  /\ appliedPanes = {}
  /\ restoreTargetVersion = 0
  /\ hasRestored = FALSE
  /\ lastRestoredVersion = 0
  /\ lastRestoredState = liveState
  /\ idempotentOk = TRUE

Mutate ==
  /\ phase \in {"Normal", "Stored", "Restored"}
  /\ Len(history) <= MaxHistory
  /\ \E p \in PaneIds, v \in Values:
       LET nextState == UpdateState(liveState, p, v) IN
         /\ liveState' = nextState
         /\ history' = Append(history, nextState)
  /\ phase' = "Normal"
  /\ UNCHANGED <<captureState, captureVersion, writeBuffer, hasSnapshot, snapshotState,
                 snapshotVersion, preRestoreLive, restoreBuffer, appliedPanes,
                 restoreTargetVersion, hasRestored, lastRestoredVersion,
                 lastRestoredState, idempotentOk>>

StartCapture ==
  /\ phase \in {"Normal", "Stored", "Restored"}
  /\ phase' = "Capturing"
  /\ captureState' = liveState
  /\ captureVersion' = Len(history) - 1
  /\ UNCHANGED <<liveState, history, writeBuffer, hasSnapshot, snapshotState,
                 snapshotVersion, preRestoreLive, restoreBuffer, appliedPanes,
                 restoreTargetVersion, hasRestored, lastRestoredVersion,
                 lastRestoredState, idempotentOk>>

FinishCapture ==
  /\ phase = "Capturing"
  /\ phase' = "Writing"
  /\ writeBuffer' = captureState
  /\ UNCHANGED <<liveState, history, captureState, captureVersion, hasSnapshot,
                 snapshotState, snapshotVersion, preRestoreLive, restoreBuffer,
                 appliedPanes, restoreTargetVersion, hasRestored,
                 lastRestoredVersion, lastRestoredState, idempotentOk>>

CommitWrite ==
  /\ phase = "Writing"
  /\ phase' = "Stored"
  /\ hasSnapshot' = TRUE
  /\ snapshotState' = writeBuffer
  /\ snapshotVersion' = captureVersion
  /\ UNCHANGED <<liveState, history, captureState, captureVersion, writeBuffer,
                 preRestoreLive, restoreBuffer, appliedPanes, restoreTargetVersion,
                 hasRestored, lastRestoredVersion, lastRestoredState, idempotentOk>>

StartRestore ==
  /\ hasSnapshot
  /\ phase \in {"Normal", "Stored", "Restored"}
  /\ phase' = "Restoring"
  /\ preRestoreLive' = liveState
  /\ restoreBuffer' = liveState
  /\ appliedPanes' = {}
  /\ restoreTargetVersion' = snapshotVersion
  /\ UNCHANGED <<liveState, history, captureState, captureVersion, writeBuffer,
                 hasSnapshot, snapshotState, snapshotVersion, hasRestored,
                 lastRestoredVersion, lastRestoredState, idempotentOk>>

RestoreChunk ==
  /\ phase = "Restoring"
  /\ appliedPanes # PaneIds
  /\ \E p \in PaneIds \ appliedPanes:
       /\ restoreBuffer' = [restoreBuffer EXCEPT ![p] = snapshotState[p]]
       /\ appliedPanes' = appliedPanes \cup {p}
  /\ UNCHANGED <<phase, liveState, history, captureState, captureVersion,
                 writeBuffer, hasSnapshot, snapshotState, snapshotVersion,
                 preRestoreLive, restoreTargetVersion, hasRestored,
                 lastRestoredVersion, lastRestoredState, idempotentOk>>

CompleteRestore ==
  /\ phase = "Restoring"
  /\ appliedPanes = PaneIds
  /\ phase' = "Restored"
  /\ liveState' = restoreBuffer
  /\ hasRestored' = TRUE
  /\ lastRestoredVersion' = restoreTargetVersion
  /\ lastRestoredState' = restoreBuffer
  /\ idempotentOk' =
      IF hasRestored /\ lastRestoredVersion = restoreTargetVersion
      THEN idempotentOk /\ (restoreBuffer = lastRestoredState)
      ELSE idempotentOk
  /\ UNCHANGED <<history, captureState, captureVersion, writeBuffer, hasSnapshot,
                 snapshotState, snapshotVersion, preRestoreLive, restoreBuffer,
                 appliedPanes, restoreTargetVersion>>

AbortRestore ==
  /\ phase = "Restoring"
  /\ phase' = "Stored"
  /\ liveState' = preRestoreLive
  /\ restoreBuffer' = preRestoreLive
  /\ appliedPanes' = {}
  /\ UNCHANGED <<history, captureState, captureVersion, writeBuffer, hasSnapshot,
                 snapshotState, snapshotVersion, preRestoreLive,
                 restoreTargetVersion, hasRestored, lastRestoredVersion,
                 lastRestoredState, idempotentOk>>

Next ==
  \/ Mutate
  \/ StartCapture
  \/ FinishCapture
  \/ CommitWrite
  \/ StartRestore
  \/ RestoreChunk
  \/ CompleteRestore
  \/ AbortRestore

TypeOK ==
  /\ phase \in PhaseSet
  /\ liveState \in [PaneIds -> Values]
  /\ history \in Seq([PaneIds -> Values])
  /\ Len(history) >= 1
  /\ captureState \in [PaneIds -> Values]
  /\ captureVersion \in 0..(Len(history) - 1)
  /\ writeBuffer \in [PaneIds -> Values]
  /\ hasSnapshot \in BOOLEAN
  /\ snapshotState \in [PaneIds -> Values]
  /\ snapshotVersion \in Nat
  /\ preRestoreLive \in [PaneIds -> Values]
  /\ restoreBuffer \in [PaneIds -> Values]
  /\ appliedPanes \subseteq PaneIds
  /\ restoreTargetVersion \in Nat
  /\ hasRestored \in BOOLEAN
  /\ lastRestoredVersion \in Nat
  /\ lastRestoredState \in [PaneIds -> Values]
  /\ idempotentOk \in BOOLEAN

Safety_CaptureConsistent ==
  /\ phase = "Capturing" => captureState = liveState
  /\ phase = "Writing" => writeBuffer = captureState

Safety_Atomicity ==
  phase = "Restoring" => liveState = preRestoreLive

Safety_NoDataLoss ==
  (hasSnapshot /\ phase = "Restored") => liveState = snapshotState

Safety_Idempotency ==
  idempotentOk

Safety_NoPartialCommit ==
  phase = "Restored" => appliedPanes = PaneIds

Liveness_CaptureProgress ==
  [](phase = "Capturing" => <>(phase # "Capturing"))

Liveness_RestoreProgress ==
  [](phase = "Restoring" => <>(phase = "Restored" \/ phase = "Stored"))

vars == <<phase, liveState, history, captureState, captureVersion, writeBuffer,
          hasSnapshot, snapshotState, snapshotVersion, preRestoreLive,
          restoreBuffer, appliedPanes, restoreTargetVersion, hasRestored,
          lastRestoredVersion, lastRestoredState, idempotentOk>>

Spec ==
  /\ Init
  /\ [][Next]_vars
  /\ WF_vars(FinishCapture)
  /\ WF_vars(CommitWrite)
  /\ WF_vars(RestoreChunk)
  /\ WF_vars(CompleteRestore)

=============================================================================
