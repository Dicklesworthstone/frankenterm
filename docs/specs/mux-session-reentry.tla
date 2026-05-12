------------------------ MODULE MuxSessionReentry ------------------------
(*
  TLA+ specification for mux session reentry and subscriber callback safety.

  The model abstracts the production mux fanout rule that subscriber callbacks
  run from a snapshot outside the subscriber lock, plus the session-handler rule
  that mux-side pane notifications must not recreate cached per-pane state after
  a pane has been removed. It also models pane and window terminal events as
  stable records so reentrant callback paths cannot double-register panes, leak
  dead subscribers, or drop pane removal events.

  Run with TLC:
    java -jar tla2tools.jar -workers auto MuxSessionReentry.tla
*)

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Panes, Windows, Subscribers

ASSUME
    /\ Panes \subseteq 0..3
    /\ Windows \subseteq 0..2
    /\ Subscribers \subseteq 0..3
    /\ Panes # {}
    /\ Windows # {}
    /\ Subscribers # {}

VARIABLES panes, per_pane, pending_outputs, queued_removed,
          terminal_panes, terminal_windows, subscribers,
          snapshot, delivered, dead, dispatching, current_event,
          pane_registration_count

vars == <<panes, per_pane, pending_outputs, queued_removed,
          terminal_panes, terminal_windows, subscribers,
          snapshot, delivered, dead, dispatching, current_event,
          pane_registration_count>>

----------------------------------------------------------------------------
\* Domain
----------------------------------------------------------------------------

EventKinds ==
    {"none", "pane_output", "pane_removed", "window_removed",
     "window_created", "pane_focused", "alert"}

TypeOK ==
    /\ panes \subseteq Panes
    /\ per_pane \subseteq Panes
    /\ pending_outputs \subseteq Panes
    /\ queued_removed \subseteq Panes
    /\ terminal_panes \subseteq Panes
    /\ terminal_windows \subseteq Windows
    /\ subscribers \subseteq Subscribers
    /\ snapshot \subseteq Subscribers
    /\ delivered \subseteq Subscribers
    /\ dead \subseteq Subscribers
    /\ dispatching \in BOOLEAN
    /\ current_event \in EventKinds
    /\ pane_registration_count \in [Panes -> 0..1]

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

Init ==
    /\ panes = {}
    /\ per_pane = {}
    /\ pending_outputs = {}
    /\ queued_removed = {}
    /\ terminal_panes = {}
    /\ terminal_windows = {}
    /\ subscribers = Subscribers
    /\ snapshot = {}
    /\ delivered = {}
    /\ dead = {}
    /\ dispatching = FALSE
    /\ current_event = "none"
    /\ pane_registration_count = [p \in Panes |-> 0]

----------------------------------------------------------------------------
\* Mux pane/window lifecycle actions
----------------------------------------------------------------------------

AddPane(p) ==
    /\ p \in Panes
    /\ p \notin panes
    /\ panes' = panes \cup {p}
    /\ pane_registration_count' =
        [pane_registration_count EXCEPT ![p] = 1]
    /\ UNCHANGED <<per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, subscribers,
                  snapshot, delivered, dead, dispatching, current_event>>

DuplicateAddPaneNoop(p) ==
    /\ p \in panes
    /\ UNCHANGED vars

RemovePane(p) ==
    /\ p \in panes
    /\ panes' = panes \ {p}
    /\ per_pane' = per_pane \ {p}
    /\ pending_outputs' = pending_outputs \ {p}
    /\ queued_removed' = queued_removed \cup {p}
    /\ terminal_panes' = terminal_panes \cup {p}
    /\ pane_registration_count' =
        [pane_registration_count EXCEPT ![p] = 0]
    /\ UNCHANGED <<terminal_windows, subscribers, snapshot, delivered, dead,
                  dispatching, current_event>>

RemoveWindow(w) ==
    /\ w \in Windows
    /\ terminal_windows' = terminal_windows \cup {w}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, subscribers, snapshot, delivered, dead,
                  dispatching, current_event, pane_registration_count>>

----------------------------------------------------------------------------
\* Session-handler per-pane cache and pane-output queue actions
----------------------------------------------------------------------------

ClientTrackPane(p) ==
    /\ p \in panes
    /\ per_pane' = per_pane \cup {p}
    /\ UNCHANGED <<panes, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, subscribers,
                  snapshot, delivered, dead, dispatching, current_event,
                  pane_registration_count>>

TrackedPaneOutput(p) ==
    /\ p \in per_pane
    /\ pending_outputs' = pending_outputs \cup {p}
    /\ UNCHANGED <<panes, per_pane, queued_removed, terminal_panes,
                  terminal_windows, subscribers, snapshot, delivered, dead,
                  dispatching, current_event, pane_registration_count>>

StalePaneOutputNoop(p) ==
    /\ p \in Panes
    /\ p \notin per_pane
    /\ UNCHANGED vars

FlushPaneOutput(p) ==
    /\ p \in pending_outputs
    /\ pending_outputs' = pending_outputs \ {p}
    /\ UNCHANGED <<panes, per_pane, queued_removed, terminal_panes,
                  terminal_windows, subscribers, snapshot, delivered, dead,
                  dispatching, current_event, pane_registration_count>>

----------------------------------------------------------------------------
\* Subscriber registration and snapshot dispatch actions
----------------------------------------------------------------------------

Subscribe(s) ==
    /\ s \in Subscribers
    /\ subscribers' = subscribers \cup {s}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, snapshot, delivered, dead,
                  dispatching, current_event, pane_registration_count>>

GuardDrop(s) ==
    /\ s \in Subscribers
    /\ subscribers' = subscribers \ {s}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, snapshot, delivered, dead,
                  dispatching, current_event, pane_registration_count>>

BeginDispatch(e) ==
    /\ ~dispatching
    /\ e \in EventKinds \ {"none"}
    /\ dispatching' = TRUE
    /\ current_event' = e
    /\ snapshot' = subscribers
    /\ delivered' = {}
    /\ dead' = {}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, subscribers,
                  pane_registration_count>>

CallbackAlive(s) ==
    /\ dispatching
    /\ s \in snapshot \ delivered
    /\ delivered' = delivered \cup {s}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, subscribers, snapshot,
                  dead, dispatching, current_event, pane_registration_count>>

CallbackDead(s) ==
    /\ dispatching
    /\ s \in snapshot \ delivered
    /\ delivered' = delivered \cup {s}
    /\ dead' = dead \cup {s}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, subscribers, snapshot,
                  dispatching, current_event, pane_registration_count>>

CallbackUnsubscribes(s, target) ==
    /\ dispatching
    /\ s \in snapshot \ delivered
    /\ target \in Subscribers
    /\ delivered' = delivered \cup {s}
    /\ subscribers' = subscribers \ {target}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, snapshot, dead,
                  dispatching, current_event, pane_registration_count>>

CallbackReentrantPaneOutput(s, p) ==
    /\ dispatching
    /\ s \in snapshot \ delivered
    /\ p \in Panes
    /\ delivered' = delivered \cup {s}
    /\ pending_outputs' =
        IF p \in panes THEN pending_outputs \cup {p} ELSE pending_outputs
    /\ UNCHANGED <<panes, per_pane, queued_removed, terminal_panes,
                  terminal_windows, subscribers, snapshot, dead, dispatching,
                  current_event, pane_registration_count>>

EndDispatch ==
    /\ dispatching
    /\ delivered = snapshot
    /\ dispatching' = FALSE
    /\ current_event' = "none"
    /\ subscribers' = subscribers \ dead
    /\ snapshot' = {}
    /\ delivered' = {}
    /\ dead' = {}
    /\ UNCHANGED <<panes, per_pane, pending_outputs, queued_removed,
                  terminal_panes, terminal_windows, pane_registration_count>>

----------------------------------------------------------------------------
\* Next-state relation
----------------------------------------------------------------------------

Next ==
    \/ \E p \in Panes : AddPane(p)
    \/ \E p \in Panes : DuplicateAddPaneNoop(p)
    \/ \E p \in Panes : RemovePane(p)
    \/ \E w \in Windows : RemoveWindow(w)
    \/ \E p \in Panes : ClientTrackPane(p)
    \/ \E p \in Panes : TrackedPaneOutput(p)
    \/ \E p \in Panes : StalePaneOutputNoop(p)
    \/ \E p \in Panes : FlushPaneOutput(p)
    \/ \E s \in Subscribers : Subscribe(s)
    \/ \E s \in Subscribers : GuardDrop(s)
    \/ \E e \in EventKinds \ {"none"} : BeginDispatch(e)
    \/ \E s \in Subscribers : CallbackAlive(s)
    \/ \E s \in Subscribers : CallbackDead(s)
    \/ \E s \in Subscribers :
        \E target \in Subscribers : CallbackUnsubscribes(s, target)
    \/ \E s \in Subscribers : \E p \in Panes : CallbackReentrantPaneOutput(s, p)
    \/ EndDispatch

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants
----------------------------------------------------------------------------

PaneLiveRegisteredAtMostOnce ==
    \A p \in Panes :
        /\ pane_registration_count[p] <= 1
        /\ (p \in panes) <=> (pane_registration_count[p] = 1)

PerPaneOnlyForLivePanes ==
    per_pane \subseteq panes

PendingOutputOnlyForLivePanes ==
    pending_outputs \subseteq panes

RemovedPaneHasTerminalQueueEvent ==
    terminal_panes \subseteq queued_removed

DispatchSnapshotWellFormed ==
    /\ delivered \subseteq snapshot
    /\ dead \subseteq snapshot
    /\ dispatching =>
        /\ snapshot \subseteq Subscribers
        /\ current_event \in EventKinds \ {"none"}
    /\ ~dispatching =>
        /\ snapshot = {}
        /\ delivered = {}
        /\ dead = {}
        /\ current_event = "none"

SubscribersStayBounded ==
    subscribers \subseteq Subscribers

TerminalEventsStayStable ==
    /\ terminal_panes \subseteq Panes
    /\ terminal_windows \subseteq Windows

SafetyInvariants ==
    /\ TypeOK
    /\ PaneLiveRegisteredAtMostOnce
    /\ PerPaneOnlyForLivePanes
    /\ PendingOutputOnlyForLivePanes
    /\ RemovedPaneHasTerminalQueueEvent
    /\ DispatchSnapshotWellFormed
    /\ SubscribersStayBounded
    /\ TerminalEventsStayStable

----------------------------------------------------------------------------
\* Liveness / progress notes
----------------------------------------------------------------------------

DispatchCanFinish ==
    dispatching /\ delivered = snapshot ~> ~dispatching

PendingOutputCanDrain ==
    pending_outputs # {} ~> pending_outputs = {}

Liveness ==
    /\ DispatchCanFinish
    /\ PendingOutputCanDrain

=============================================================================
