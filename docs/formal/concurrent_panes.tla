---------------------------- MODULE concurrent_panes ----------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS PaneUniverse, SizeValues, NoPane
ASSUME /\ PaneUniverse # {}
       /\ SizeValues # {}
       /\ NoPane \notin PaneUniverse

DefaultSize == CHOOSE s \in SizeValues : TRUE

DropEdges(edges, p) == {e \in edges : e[1] # p /\ e[2] # p}

VARIABLES
  created,
  alive,
  destroyed,
  persisted,
  resources,
  paneSize,
  focus,
  pendingCreate,
  pendingDestroy,
  pendingPersist,
  pendingResize,
  resizeTarget,
  pendingFocus,
  waitFor

Init ==
  /\ created = {}
  /\ alive = {}
  /\ destroyed = {}
  /\ persisted = {}
  /\ resources = [p \in PaneUniverse |-> 0]
  /\ paneSize = [p \in PaneUniverse |-> DefaultSize]
  /\ focus = NoPane
  /\ pendingCreate = {}
  /\ pendingDestroy = {}
  /\ pendingPersist = {}
  /\ pendingResize = {}
  /\ resizeTarget = [p \in PaneUniverse |-> DefaultSize]
  /\ pendingFocus = {}
  /\ waitFor = {}

RequestCreate ==
  /\ \E p \in PaneUniverse \ (created \cup pendingCreate):
       pendingCreate' = pendingCreate \cup {p}
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingDestroy, pendingPersist, pendingResize,
                 resizeTarget, pendingFocus, waitFor>>

CompleteCreate ==
  /\ \E p \in pendingCreate:
       /\ pendingCreate' = pendingCreate \ {p}
       /\ created' = created \cup {p}
       /\ alive' = alive \cup {p}
       /\ resources' = [resources EXCEPT ![p] = 1]
       /\ paneSize' = [paneSize EXCEPT ![p] = resizeTarget[p]]
  /\ UNCHANGED <<destroyed, persisted, focus, pendingDestroy, pendingPersist,
                 pendingResize, resizeTarget, pendingFocus, waitFor>>

RequestDestroy ==
  /\ \E p \in alive \ (pendingDestroy \cup pendingPersist):
       pendingDestroy' = pendingDestroy \cup {p}
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingCreate, pendingPersist, pendingResize,
                 resizeTarget, pendingFocus, waitFor>>

CompleteDestroy ==
  /\ \E p \in pendingDestroy:
       /\ pendingDestroy' = pendingDestroy \ {p}
       /\ alive' = alive \ {p}
       /\ destroyed' = destroyed \cup {p}
       /\ resources' = [resources EXCEPT ![p] = 0]
       /\ pendingPersist' = pendingPersist \ {p}
       /\ pendingResize' = pendingResize \ {p}
       /\ pendingFocus' = pendingFocus \ {p}
       /\ waitFor' = DropEdges(waitFor, p)
       /\ focus' = IF focus = p THEN NoPane ELSE focus
  /\ UNCHANGED <<created, persisted, paneSize, pendingCreate, resizeTarget>>

RequestPersist ==
  /\ \E p \in alive \ (pendingPersist \cup pendingDestroy):
       pendingPersist' = pendingPersist \cup {p}
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingCreate, pendingDestroy, pendingResize,
                 resizeTarget, pendingFocus, waitFor>>

CompletePersist ==
  /\ \E p \in pendingPersist:
       /\ pendingPersist' = pendingPersist \ {p}
       /\ alive' = alive \ {p}
       /\ persisted' = persisted \cup {p}
       /\ resources' = [resources EXCEPT ![p] = 0]
       /\ pendingDestroy' = pendingDestroy \ {p}
       /\ pendingResize' = pendingResize \ {p}
       /\ pendingFocus' = pendingFocus \ {p}
       /\ waitFor' = DropEdges(waitFor, p)
       /\ focus' = IF focus = p THEN NoPane ELSE focus
  /\ UNCHANGED <<created, destroyed, paneSize, pendingCreate, resizeTarget>>

RequestResize ==
  /\ \E p \in alive \ pendingResize, s \in SizeValues:
       /\ pendingResize' = pendingResize \cup {p}
       /\ resizeTarget' = [resizeTarget EXCEPT ![p] = s]
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingCreate, pendingDestroy, pendingPersist,
                 pendingFocus, waitFor>>

CompleteResize ==
  /\ \E p \in pendingResize:
       /\ pendingResize' = pendingResize \ {p}
       /\ paneSize' = [paneSize EXCEPT ![p] = resizeTarget[p]]
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, focus,
                 pendingCreate, pendingDestroy, pendingPersist, resizeTarget,
                 pendingFocus, waitFor>>

RequestFocus ==
  /\ \E p \in alive \ pendingFocus:
       pendingFocus' = pendingFocus \cup {p}
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingCreate, pendingDestroy, pendingPersist,
                 pendingResize, resizeTarget, waitFor>>

CompleteFocus ==
  /\ \E p \in pendingFocus \cap alive:
       /\ pendingFocus' = pendingFocus \ {p}
       /\ focus' = p
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 pendingCreate, pendingDestroy, pendingPersist, pendingResize,
                 resizeTarget, waitFor>>

AddWait ==
  /\ \E a, b \in alive:
       /\ a # b
       /\ a > b
       /\ <<a, b>> \notin waitFor
       /\ waitFor' = waitFor \cup {<<a, b>>}
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingCreate, pendingDestroy, pendingPersist,
                 pendingResize, resizeTarget, pendingFocus>>

ClearWait ==
  /\ \E e \in waitFor:
       waitFor' = waitFor \ {e}
  /\ UNCHANGED <<created, alive, destroyed, persisted, resources, paneSize,
                 focus, pendingCreate, pendingDestroy, pendingPersist,
                 pendingResize, resizeTarget, pendingFocus>>

Next ==
  \/ RequestCreate
  \/ CompleteCreate
  \/ RequestDestroy
  \/ CompleteDestroy
  \/ RequestPersist
  \/ CompletePersist
  \/ RequestResize
  \/ CompleteResize
  \/ RequestFocus
  \/ CompleteFocus
  \/ AddWait
  \/ ClearWait

TypeOK ==
  /\ created \subseteq PaneUniverse
  /\ alive \subseteq PaneUniverse
  /\ destroyed \subseteq PaneUniverse
  /\ persisted \subseteq PaneUniverse
  /\ resources \in [PaneUniverse -> 0..1]
  /\ paneSize \in [PaneUniverse -> SizeValues]
  /\ focus \in PaneUniverse \cup {NoPane}
  /\ pendingCreate \subseteq PaneUniverse
  /\ pendingDestroy \subseteq PaneUniverse
  /\ pendingPersist \subseteq PaneUniverse
  /\ pendingResize \subseteq PaneUniverse
  /\ resizeTarget \in [PaneUniverse -> SizeValues]
  /\ pendingFocus \subseteq PaneUniverse
  /\ waitFor \subseteq (PaneUniverse \X PaneUniverse)

Safety_LifecycleDisjoint ==
  /\ alive \cap destroyed = {}
  /\ alive \cap persisted = {}
  /\ destroyed \cap persisted = {}

Safety_NoOrphans ==
  (created \ alive) \subseteq (destroyed \cup persisted)

Safety_NoLeaks ==
  \A p \in PaneUniverse :
    p \in (destroyed \cup persisted) => resources[p] = 0

Safety_AliveHasResources ==
  \A p \in alive : resources[p] = 1

Safety_FocusValid ==
  focus = NoPane \/ focus \in alive

Safety_OrderedWaits ==
  \A e \in waitFor : e[1] > e[2]

Safety_NoSelfWait ==
  \A p \in PaneUniverse : <<p, p>> \notin waitFor

Liveness_CreateCompletes ==
  \A p \in PaneUniverse : [](p \in pendingCreate => <>(p \notin pendingCreate))

Liveness_DestroyCompletes ==
  \A p \in PaneUniverse : [](p \in pendingDestroy => <>(p \notin pendingDestroy))

Liveness_PersistCompletes ==
  \A p \in PaneUniverse : [](p \in pendingPersist => <>(p \notin pendingPersist))

Liveness_ResizeCompletes ==
  \A p \in PaneUniverse : [](p \in pendingResize => <>(p \notin pendingResize))

Liveness_FocusCompletes ==
  \A p \in PaneUniverse : [](p \in pendingFocus => <>(p \notin pendingFocus))

vars == <<created, alive, destroyed, persisted, resources, paneSize, focus,
          pendingCreate, pendingDestroy, pendingPersist, pendingResize,
          resizeTarget, pendingFocus, waitFor>>

Spec ==
  /\ Init
  /\ [][Next]_vars
  /\ SF_vars(CompleteCreate)
  /\ SF_vars(CompleteDestroy)
  /\ SF_vars(CompletePersist)
  /\ SF_vars(CompleteResize)
  /\ SF_vars(CompleteFocus)
  /\ SF_vars(ClearWait)

=============================================================================
