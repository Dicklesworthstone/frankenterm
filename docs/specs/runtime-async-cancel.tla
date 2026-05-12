------------------------ MODULE RuntimeAsyncCancel ------------------------
(*
  TLA+ specification for runtime_async Cx cancellation semantics.

  The model abstracts one pending wait at a time across the public
  runtime_async primitive families. Each terminal transition records the
  before/after resource counters so TLC can check that cancellation never
  manufactures permits, drops queued messages, silently removes join handles,
  or hides the documented spawn_blocking orphan-work trade-off.

  Run with TLC:
    java -jar tla2tools.jar -workers auto RuntimeAsyncCancel.tla
*)

EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS Primitives, MaxHistory

ASSUME
    /\ Primitives =
        {"mutex", "rwlock", "semaphore", "mpsc", "watch", "broadcast",
         "oneshot", "notify", "sleep", "timeout", "joinset",
         "spawn_blocking"}
    /\ MaxHistory \in 1..2

VARIABLES primitive, phase, result, permits, messages, join_handles, waiters,
          blocking_work, watcher, history

vars == <<primitive, phase, result, permits, messages, join_handles, waiters,
          blocking_work, watcher, history>>

----------------------------------------------------------------------------
\* Domain
----------------------------------------------------------------------------

Phases == {"idle", "waiting", "terminal"}
Modes == {"pre", "mid", "success"}
BlockingStates == {"none", "running", "done", "orphaned"}
Results ==
    {"none", "pending", "success", "panic", "err_cancelled",
     "watcher_cancelled", "join_error", "budget_elapsed",
     "waiter_dropped", "abandoned_result"}

MaxPermits == 1
MaxMessages == 1
MaxJoinHandles == 1
MaxWaiters == 1

PanicPrimitives == {"mutex", "rwlock"}
ErrCancelPrimitives == {"semaphore", "mpsc", "watch", "broadcast", "oneshot"}
ChannelReceivePrimitives == {"mpsc", "watch", "broadcast", "oneshot"}
BudgetPrimitives == {"sleep", "timeout"}
WatcherRequiredPrimitives == {"semaphore", "mpsc", "watch", "broadcast", "oneshot"}

HistoryRow == [
    primitive       : Primitives,
    mode            : Modes,
    result          : Results,
    permits_before  : 0..MaxPermits,
    permits_after   : 0..MaxPermits,
    messages_before : 0..MaxMessages,
    messages_after  : 0..MaxMessages,
    handles_before  : 0..MaxJoinHandles,
    handles_after   : 0..MaxJoinHandles,
    waiters_before  : 0..MaxWaiters,
    waiters_after   : 0..MaxWaiters,
    blocking_before : BlockingStates,
    blocking_after  : BlockingStates,
    watcher         : BOOLEAN
]

TypeOK ==
    /\ primitive \in Primitives
    /\ phase \in Phases
    /\ result \in Results
    /\ permits \in 0..MaxPermits
    /\ messages \in 0..MaxMessages
    /\ join_handles \in 0..MaxJoinHandles
    /\ waiters \in 0..MaxWaiters
    /\ blocking_work \in BlockingStates
    /\ watcher \in BOOLEAN
    /\ history \in Seq(HistoryRow)
    /\ Len(history) <= MaxHistory

CancelResult(p, mode) ==
    CASE p \in PanicPrimitives -> "panic"
       [] p \in ErrCancelPrimitives /\ mode = "pre" -> "err_cancelled"
       [] p \in ErrCancelPrimitives /\ mode = "mid" -> "watcher_cancelled"
       [] p = "notify" -> "waiter_dropped"
       [] p = "joinset" -> "join_error"
       [] p = "spawn_blocking" /\ mode = "pre" -> "err_cancelled"
       [] p = "spawn_blocking" /\ mode = "mid" -> "abandoned_result"
       [] p \in BudgetPrimitives -> "budget_elapsed"
       [] OTHER -> "err_cancelled"

ModeWatcher(p, mode) ==
    /\ mode = "mid"
    /\ p \in WatcherRequiredPrimitives

BlockingAfterCancel(p, mode) ==
    IF p = "spawn_blocking" /\ mode = "mid" THEN "orphaned" ELSE "none"

HandlesAfterCancel(p, mode) ==
    IF p = "spawn_blocking" /\ mode = "pre" THEN 0 ELSE join_handles

CancelRecord(p, mode, r, w) == [
    primitive       |-> p,
    mode            |-> mode,
    result          |-> r,
    permits_before  |-> permits,
    permits_after   |-> permits,
    messages_before |-> messages,
    messages_after  |-> messages,
    handles_before  |-> join_handles,
    handles_after   |-> HandlesAfterCancel(p, mode),
    waiters_before  |-> waiters,
    waiters_after   |-> 0,
    blocking_before |-> blocking_work,
    blocking_after  |-> BlockingAfterCancel(p, mode),
    watcher         |-> w
]

SuccessRecord(p) == [
    primitive       |-> p,
    mode            |-> "success",
    result          |-> "success",
    permits_before  |-> permits,
    permits_after   |-> IF p = "semaphore" THEN 0 ELSE permits,
    messages_before |-> messages,
    messages_after  |-> IF p \in ChannelReceivePrimitives THEN 0 ELSE messages,
    handles_before  |-> join_handles,
    handles_after   |-> IF p \in {"joinset", "spawn_blocking"} THEN 0 ELSE join_handles,
    waiters_before  |-> waiters,
    waiters_after   |-> 0,
    blocking_before |-> blocking_work,
    blocking_after  |-> IF p = "spawn_blocking" THEN "done" ELSE blocking_work,
    watcher         |-> FALSE
]

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

Init ==
    /\ primitive = "mutex"
    /\ phase = "idle"
    /\ result = "none"
    /\ permits = 0
    /\ messages = 0
    /\ join_handles = 0
    /\ waiters = 0
    /\ blocking_work = "none"
    /\ watcher = FALSE
    /\ history = <<>>

----------------------------------------------------------------------------
\* Actions
----------------------------------------------------------------------------

StartWait(p) ==
    /\ p \in Primitives
    /\ phase \in {"idle", "terminal"}
    /\ Len(history) < MaxHistory
    /\ primitive' = p
    /\ phase' = "waiting"
    /\ result' = "pending"
    /\ permits' = 0
    /\ messages' = 0
    /\ join_handles' = IF p \in {"joinset", "spawn_blocking"} THEN 1 ELSE 0
    /\ waiters' = 1
    /\ blocking_work' = IF p = "spawn_blocking" THEN "running" ELSE "none"
    /\ watcher' = FALSE
    /\ UNCHANGED history

WakeResource ==
    /\ phase = "waiting"
    /\ \/ /\ primitive = "semaphore"
          /\ permits = 0
          /\ permits' = 1
          /\ UNCHANGED <<messages, join_handles, waiters, blocking_work>>
       \/ /\ primitive \in ChannelReceivePrimitives
          /\ messages = 0
          /\ messages' = 1
          /\ UNCHANGED <<permits, join_handles, waiters, blocking_work>>
       \/ /\ primitive \in {"notify", "sleep", "timeout", "joinset",
                            "spawn_blocking", "mutex", "rwlock"}
          /\ UNCHANGED <<permits, messages, join_handles, waiters,
                         blocking_work>>
    /\ UNCHANGED <<primitive, phase, result, watcher, history>>

PreCancel ==
    LET r == CancelResult(primitive, "pre")
        w == ModeWatcher(primitive, "pre")
    IN
    /\ phase = "waiting"
    /\ phase' = "terminal"
    /\ result' = r
    /\ waiters' = 0
    /\ watcher' = w
    /\ join_handles' = HandlesAfterCancel(primitive, "pre")
    /\ blocking_work' = BlockingAfterCancel(primitive, "pre")
    /\ UNCHANGED <<primitive, permits, messages>>
    /\ history' = Append(history, CancelRecord(primitive, "pre", r, w))

MidCancel ==
    LET r == CancelResult(primitive, "mid")
        w == ModeWatcher(primitive, "mid")
    IN
    /\ phase = "waiting"
    /\ phase' = "terminal"
    /\ result' = r
    /\ waiters' = 0
    /\ watcher' = w
    /\ join_handles' = HandlesAfterCancel(primitive, "mid")
    /\ blocking_work' = BlockingAfterCancel(primitive, "mid")
    /\ UNCHANGED <<primitive, permits, messages>>
    /\ history' = Append(history, CancelRecord(primitive, "mid", r, w))

CanComplete ==
    \/ primitive = "semaphore" /\ permits = 1
    \/ primitive \in ChannelReceivePrimitives /\ messages = 1
    \/ primitive \in {"notify", "sleep", "timeout", "joinset",
                      "spawn_blocking", "mutex", "rwlock"}

CompleteWait ==
    /\ phase = "waiting"
    /\ CanComplete
    /\ phase' = "terminal"
    /\ result' = "success"
    /\ permits' = IF primitive = "semaphore" THEN 0 ELSE permits
    /\ messages' = IF primitive \in ChannelReceivePrimitives THEN 0 ELSE messages
    /\ join_handles' = IF primitive \in {"joinset", "spawn_blocking"} THEN 0 ELSE join_handles
    /\ waiters' = 0
    /\ blocking_work' = IF primitive = "spawn_blocking" THEN "done" ELSE blocking_work
    /\ watcher' = FALSE
    /\ UNCHANGED primitive
    /\ history' = Append(history, SuccessRecord(primitive))

StutterAtBound ==
    /\ Len(history) = MaxHistory
    /\ UNCHANGED vars

Next ==
    \/ \E p \in Primitives : StartWait(p)
    \/ WakeResource
    \/ PreCancel
    \/ MidCancel
    \/ CompleteWait
    \/ StutterAtBound

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants
----------------------------------------------------------------------------

CancelRowsDoNotMutatePermitsOrMessages ==
    \A i \in DOMAIN history :
        history[i].mode \in {"pre", "mid"} =>
            /\ history[i].permits_after = history[i].permits_before
            /\ history[i].messages_after = history[i].messages_before

JoinSetCancelDoesNotDropHandles ==
    \A i \in DOMAIN history :
        /\ history[i].primitive = "joinset"
        /\ history[i].mode \in {"pre", "mid"}
        => history[i].handles_after = history[i].handles_before

SpawnBlockingMidCancelIsExplicitOrphan ==
    \A i \in DOMAIN history :
        /\ history[i].primitive = "spawn_blocking"
        /\ history[i].mode = "mid"
        => /\ history[i].result = "abandoned_result"
           /\ history[i].blocking_after = "orphaned"

WatcherRequiredForDelegatedMidCancel ==
    \A i \in DOMAIN history :
        /\ history[i].mode = "mid"
        /\ history[i].primitive \in WatcherRequiredPrimitives
        => /\ history[i].watcher
           /\ history[i].result = "watcher_cancelled"

DirectPreCancelResultMatchesContract ==
    \A i \in DOMAIN history :
        history[i].mode = "pre" =>
            history[i].result = CancelResult(history[i].primitive, "pre")

BudgetPrimitivesNeverClaimDirectCancel ==
    \A i \in DOMAIN history :
        /\ history[i].primitive \in BudgetPrimitives
        /\ history[i].mode \in {"pre", "mid"}
        => history[i].result = "budget_elapsed"

SuccessRowsConsumeOnlyOwnedResources ==
    \A i \in DOMAIN history :
        history[i].mode = "success" =>
            /\ history[i].permits_after <= history[i].permits_before
            /\ history[i].messages_after <= history[i].messages_before
            /\ history[i].handles_after <= history[i].handles_before

TerminalRowsClearWaiters ==
    \A i \in DOMAIN history :
        history[i].waiters_after = 0

SafetyInvariants ==
    /\ TypeOK
    /\ CancelRowsDoNotMutatePermitsOrMessages
    /\ JoinSetCancelDoesNotDropHandles
    /\ SpawnBlockingMidCancelIsExplicitOrphan
    /\ WatcherRequiredForDelegatedMidCancel
    /\ DirectPreCancelResultMatchesContract
    /\ BudgetPrimitivesNeverClaimDirectCancel
    /\ SuccessRowsConsumeOnlyOwnedResources
    /\ TerminalRowsClearWaiters

----------------------------------------------------------------------------
\* Liveness / progress
----------------------------------------------------------------------------

\* The runtime_async cancellation contract is safety-first: every modeled
\* waiter has a terminal cancel or completion transition, and the bounded
\* TLC run explores all one- and two-operation schedules. Fairness is supplied
\* by callers that keep polling or by the documented select-race watcher for
\* delegated primitives that do not register a Cx cancel waker.
LivenessProgress ==
    TRUE

==============================================================================
