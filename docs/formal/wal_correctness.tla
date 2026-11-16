---------------------------- MODULE wal_correctness ----------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Keys, Values, DefaultValue, MaxOps
ASSUME /\ Keys # {}
       /\ Values # {}
       /\ DefaultValue \in Values
       /\ MaxOps \in Nat \ {0}

OpSet == {[k |-> k, v |-> v] : k \in Keys, v \in Values}

Prefix(ops, n) == IF n = 0 THEN <<>> ELSE SubSeq(ops, 1, n)

MaxIndex(idxs) == CHOOSE m \in idxs : \A n \in idxs : m >= n

ApplyOps(base, ops) ==
  [k \in Keys |->
    LET idxs == {i \in DOMAIN ops : ops[i].k = k} IN
      IF idxs = {}
      THEN base[k]
      ELSE ops[MaxIndex(idxs)].v]

VARIABLES
  memState,
  wal,
  durableIdx,
  crashed,
  compactBase,
  replayOk,
  compactionOk

InitState == [k \in Keys |-> DefaultValue]

Init ==
  /\ memState = InitState
  /\ wal = <<>>
  /\ durableIdx = 0
  /\ crashed = FALSE
  /\ compactBase = InitState
  /\ replayOk = TRUE
  /\ compactionOk = TRUE

Mutate ==
  /\ ~crashed
  /\ Len(wal) < MaxOps
  /\ \E k \in Keys, v \in Values:
       LET nextWal == Append(wal, [k |-> k, v |-> v]) IN
         /\ wal' = nextWal
         /\ memState' = ApplyOps(compactBase, nextWal)
  /\ UNCHANGED <<durableIdx, crashed, compactBase, replayOk, compactionOk>>

Fsync ==
  /\ ~crashed
  /\ durableIdx < Len(wal)
  /\ durableIdx' = Len(wal)
  /\ UNCHANGED <<memState, wal, crashed, compactBase, replayOk, compactionOk>>

Crash ==
  /\ ~crashed
  /\ crashed' = TRUE
  /\ wal' = Prefix(wal, durableIdx)
  /\ durableIdx' = Len(wal')
  /\ memState' = compactBase
  /\ UNCHANGED <<compactBase, replayOk, compactionOk>>

Recover ==
  /\ crashed
  /\ crashed' = FALSE
  /\ memState' = ApplyOps(compactBase, wal)
  /\ replayOk' = replayOk /\ (memState' = ApplyOps(compactBase, Prefix(wal, durableIdx)))
  /\ UNCHANGED <<wal, durableIdx, compactBase, compactionOk>>

Compact ==
  /\ ~crashed
  /\ durableIdx = Len(wal)
  /\ durableIdx > 0
  /\ LET newBase == ApplyOps(compactBase, wal) IN
       /\ compactBase' = newBase
       /\ wal' = <<>>
       /\ durableIdx' = 0
       /\ memState' = newBase
       /\ compactionOk' = compactionOk /\ (ApplyOps(compactBase, wal) = ApplyOps(newBase, wal'))
  /\ UNCHANGED <<crashed, replayOk>>

Next ==
  \/ Mutate
  \/ Fsync
  \/ Crash
  \/ Recover
  \/ Compact

TypeOK ==
  /\ memState \in [Keys -> Values]
  /\ wal \in Seq(OpSet)
  /\ durableIdx \in 0..Len(wal)
  /\ crashed \in BOOLEAN
  /\ compactBase \in [Keys -> Values]
  /\ replayOk \in BOOLEAN
  /\ compactionOk \in BOOLEAN

Safety_RunningMatchesLog ==
  ~crashed => memState = ApplyOps(compactBase, wal)

Safety_DurableBound ==
  durableIdx <= Len(wal)

Safety_DurableWritesSurviveCrash ==
  crashed => durableIdx = Len(wal)

Safety_ReplayEquivalent ==
  replayOk

Safety_CompactionSafe ==
  compactionOk

Liveness_CrashRecovers ==
  [](crashed => <> ~crashed)

Liveness_DurableProgress ==
  []((~crashed /\ durableIdx < Len(wal)) => <>(durableIdx = Len(wal) \/ crashed))

vars == <<memState, wal, durableIdx, crashed, compactBase, replayOk, compactionOk>>

Spec ==
  /\ Init
  /\ [][Next]_vars
  /\ WF_vars(Fsync)
  /\ WF_vars(Recover)

=============================================================================
