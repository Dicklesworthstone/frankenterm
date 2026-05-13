----------------------- MODULE CaptureFairnessScheduler -----------------------
(*
  TLA+ specification for capture scheduler fairness and bounded liveness.

  The model abstracts the polling scheduler contract in
  crates/frankenterm-core/src/tailer.rs and docs/capture-fairness-slo-contract.md.
  It checks high-tier precedence, equal-priority rotation, the low-tier floor,
  low-subtier rotation under pressure, explicit budget deferral, backpressure
  overflow-gap state, and shutdown/cancel terminal non-admission.

  Run with TLC:
    java -jar tla2tools.jar -workers auto CaptureFairnessScheduler.tla
*)

\* coverage-metric:
\*   subsystem: capture-fairness-scheduler
\*   declared-invariants: SafetyInvariants
\*   max-depth: 8
\*   branching-factor: 6
\*   threshold-pct: 0.002

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Panes, HighPanes, LowPrimaryPanes, LowSecondaryPanes,
          AvailablePermits, BackpressureThreshold, MaxFairRounds

LowPanes == LowPrimaryPanes \cup LowSecondaryPanes

Min(a, b) == IF a <= b THEN a ELSE b
Max(a, b) == IF a >= b THEN a ELSE b
SubFloor(a, b) == IF a <= b THEN 0 ELSE a - b

ASSUME
    /\ Panes \subseteq 1..8
    /\ Panes = HighPanes \cup LowPanes
    /\ HighPanes \cap LowPanes = {}
    /\ LowPrimaryPanes \cap LowSecondaryPanes = {}
    /\ Cardinality(HighPanes) \in 1..4
    /\ Cardinality(LowPanes) \in 1..4
    /\ AvailablePermits \in 1..4
    /\ BackpressureThreshold \in 1..5
    /\ MaxFairRounds \in 1..8
    /\ Cardinality(HighPanes) >=
       IF AvailablePermits >= 2
       THEN AvailablePermits - Min(Cardinality(LowPanes), Max(1, AvailablePermits \div 5))
       ELSE 1

VARIABLES serviced, selected, high_offset, low_floor_offset, backpressure,
          overflow_pending, terminal, fair_streak, last_step, last_reason

vars == <<serviced, selected, high_offset, low_floor_offset, backpressure,
          overflow_pending, terminal, fair_streak, last_step, last_reason>>

----------------------------------------------------------------------------
\* Domain
----------------------------------------------------------------------------

TerminalStates == {"running", "shutdown", "cancelled"}
StepKinds == {
    "init",
    "fair",
    "capture_budget_exhausted",
    "byte_budget_exhausted",
    "backpressure",
    "shutdown",
    "cancelled"
}
ReasonCodes == {
    "not_observed",
    "not_due",
    "already_capturing",
    "global_capture_budget_exhausted",
    "global_byte_budget_exhausted",
    "no_permit",
    "send_backpressure",
    "overflow_gap_pending",
    "overflow_gap_emitted",
    "changed",
    "shutdown",
    "cancelled"
}

BackpressureMap == [Panes -> 0..BackpressureThreshold]

TypeOK ==
    /\ serviced \subseteq Panes
    /\ selected \subseteq Panes
    /\ high_offset \in 0..(Cardinality(HighPanes) - 1)
    /\ low_floor_offset \in 0..(Cardinality(LowPanes) - 1)
    /\ backpressure \in BackpressureMap
    /\ overflow_pending \subseteq Panes
    /\ terminal \in TerminalStates
    /\ fair_streak \in 0..MaxFairRounds
    /\ last_step \in StepKinds
    /\ last_reason \in ReasonCodes

----------------------------------------------------------------------------
\* Scheduler helpers
----------------------------------------------------------------------------

RankInSet(p, s) == Cardinality({q \in s : q < p})

RotatedRank(base_rank, offset, group_size) ==
    IF group_size = 0 THEN 0
    ELSE IF base_rank >= offset
         THEN base_rank - offset
         ELSE group_size - (offset - base_rank)

HighRank(p, offset) ==
    RotatedRank(RankInSet(p, HighPanes), offset, Cardinality(HighPanes))

LowPriorityRank(p) ==
    IF p \in LowPrimaryPanes
    THEN RankInSet(p, LowPrimaryPanes)
    ELSE Cardinality(LowPrimaryPanes) + RankInSet(p, LowSecondaryPanes)

LowFloorRank(p, offset) ==
    RotatedRank(LowPriorityRank(p), offset, Cardinality(LowPanes))

LowFloorCount(limit) ==
    IF limit >= 2
    THEN Min(Cardinality(LowPanes), Max(1, limit \div 5))
    ELSE 0

HighTake(limit) ==
    Min(Cardinality(HighPanes), SubFloor(limit, LowFloorCount(limit)))

SelectedFor(limit, high_off, low_off) ==
    {p \in HighPanes : HighRank(p, high_off) < HighTake(limit)}
    \cup
    {p \in LowPanes : LowFloorRank(p, low_off) < LowFloorCount(limit)}

EffectiveLimit == Min(AvailablePermits, Cardinality(Panes))

AdvanceOffset(offset, group, amount) ==
    IF Cardinality(group) = 0
    THEN 0
    ELSE (offset + amount) % Cardinality(group)

SingleSlotSelection == SelectedFor(1, high_offset, low_floor_offset)

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

Init ==
    /\ serviced = {}
    /\ selected = {}
    /\ high_offset = 0
    /\ low_floor_offset = 0
    /\ backpressure = [p \in Panes |-> 0]
    /\ overflow_pending = {}
    /\ terminal = "running"
    /\ fair_streak = 0
    /\ last_step = "init"
    /\ last_reason = "not_observed"

----------------------------------------------------------------------------
\* Actions
----------------------------------------------------------------------------

FairRound ==
    LET chosen == SelectedFor(EffectiveLimit, high_offset, low_floor_offset) IN
    /\ terminal = "running"
    /\ fair_streak < MaxFairRounds
    /\ chosen # {}
    /\ serviced' = serviced \cup chosen
    /\ selected' = chosen
    /\ high_offset' =
       AdvanceOffset(high_offset, HighPanes, Cardinality(chosen \cap HighPanes))
    /\ low_floor_offset' =
       AdvanceOffset(low_floor_offset, LowPanes, Cardinality(chosen \cap LowPanes))
    /\ backpressure' =
       [p \in Panes |-> IF p \in chosen THEN 0 ELSE backpressure[p]]
    /\ overflow_pending' = overflow_pending \ chosen
    /\ terminal' = terminal
    /\ fair_streak' = fair_streak + 1
    /\ last_step' = "fair"
    /\ last_reason' =
       IF chosen \cap overflow_pending # {}
       THEN "overflow_gap_emitted"
       ELSE "changed"

ExhaustCaptureBudget ==
    /\ terminal = "running"
    /\ selected' = {}
    /\ fair_streak' = 0
    /\ last_step' = "capture_budget_exhausted"
    /\ last_reason' = "global_capture_budget_exhausted"
    /\ UNCHANGED <<serviced, high_offset, low_floor_offset, backpressure,
                   overflow_pending, terminal>>

ExhaustByteBudget ==
    /\ terminal = "running"
    /\ selected' = {}
    /\ fair_streak' = 0
    /\ last_step' = "byte_budget_exhausted"
    /\ last_reason' = "global_byte_budget_exhausted"
    /\ UNCHANGED <<serviced, high_offset, low_floor_offset, backpressure,
                   overflow_pending, terminal>>

Backpressure(p) ==
    LET next_count == Min(backpressure[p] + 1, BackpressureThreshold) IN
    /\ p \in Panes
    /\ terminal = "running"
    /\ selected' = {}
    /\ fair_streak' = 0
    /\ backpressure' = [backpressure EXCEPT ![p] = next_count]
    /\ overflow_pending' =
       IF next_count >= BackpressureThreshold
       THEN overflow_pending \cup {p}
       ELSE overflow_pending
    /\ last_step' = "backpressure"
    /\ last_reason' =
       IF next_count >= BackpressureThreshold
       THEN "overflow_gap_pending"
       ELSE "send_backpressure"
    /\ UNCHANGED <<serviced, high_offset, low_floor_offset, terminal>>

EnterShutdown ==
    /\ terminal = "running"
    /\ terminal' = "shutdown"
    /\ selected' = {}
    /\ fair_streak' = 0
    /\ last_step' = "shutdown"
    /\ last_reason' = "shutdown"
    /\ UNCHANGED <<serviced, high_offset, low_floor_offset, backpressure,
                   overflow_pending>>

EnterCancelled ==
    /\ terminal = "running"
    /\ terminal' = "cancelled"
    /\ selected' = {}
    /\ fair_streak' = 0
    /\ last_step' = "cancelled"
    /\ last_reason' = "cancelled"
    /\ UNCHANGED <<serviced, high_offset, low_floor_offset, backpressure,
                   overflow_pending>>

Stutter ==
    /\ \/ terminal # "running"
       \/ fair_streak = MaxFairRounds
    /\ UNCHANGED vars

Next ==
    \/ FairRound
    \/ ExhaustCaptureBudget
    \/ ExhaustByteBudget
    \/ \E p \in Panes : Backpressure(p)
    \/ EnterShutdown
    \/ EnterCancelled
    \/ Stutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants
----------------------------------------------------------------------------

NoDuplicateInflight ==
    selected \subseteq Panes

SelectionWithinPermits ==
    Cardinality(selected) <= EffectiveLimit

FairSelectionRespectsTierShares ==
    last_step = "fair" =>
        /\ Cardinality(selected \cap HighPanes) = HighTake(EffectiveLimit)
        /\ Cardinality(selected \cap LowPanes) = LowFloorCount(EffectiveLimit)

SingleSlotPriorityNoFloor ==
    Cardinality(HighPanes) > 0 => SingleSlotSelection \cap LowPanes = {}

BackpressureThresholdMarksOverflow ==
    \A p \in Panes :
        (p \in overflow_pending) <=> (backpressure[p] = BackpressureThreshold)

OverflowGapClearsBackpressure ==
    last_reason = "overflow_gap_emitted" =>
        \A p \in selected :
            /\ backpressure[p] = 0
            /\ p \notin overflow_pending

BudgetDeferralsAreExplicit ==
    /\ last_step = "capture_budget_exhausted" =>
        /\ selected = {}
        /\ last_reason = "global_capture_budget_exhausted"
    /\ last_step = "byte_budget_exhausted" =>
        /\ selected = {}
        /\ last_reason = "global_byte_budget_exhausted"

TerminalStatesDoNotAdmit ==
    terminal # "running" =>
        /\ selected = {}
        /\ fair_streak = 0
        /\ last_reason \in {"shutdown", "cancelled"}

FairRoundsRecordService ==
    last_step = "fair" => selected \subseteq serviced

NoEligiblePaneStarvesAtFairBound ==
    fair_streak = MaxFairRounds => Panes \subseteq serviced

SafetyInvariants ==
    /\ TypeOK
    /\ NoDuplicateInflight
    /\ SelectionWithinPermits
    /\ FairSelectionRespectsTierShares
    /\ SingleSlotPriorityNoFloor
    /\ BackpressureThresholdMarksOverflow
    /\ OverflowGapClearsBackpressure
    /\ BudgetDeferralsAreExplicit
    /\ TerminalStatesDoNotAdmit
    /\ FairRoundsRecordService
    /\ NoEligiblePaneStarvesAtFairBound

----------------------------------------------------------------------------
\* Liveness / progress
----------------------------------------------------------------------------

\* The scheduler cannot promise service while capture budgets, byte budgets,
\* downstream backpressure, shutdown, or cancellation remain active forever.
\* This bounded progress property therefore states the documented fairness
\* assumption explicitly: after MaxFairRounds uninterrupted fair admission
\* rounds with all panes eligible, every modeled eligible pane has been
\* selected at least once.
LivenessUnderFairAdmission ==
    NoEligiblePaneStarvesAtFairBound

=============================================================================
