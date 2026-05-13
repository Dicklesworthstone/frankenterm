-------------------------- MODULE HerdWaveAdmission --------------------------
(*
  TLA+ specification for herd-wave admission-control and dry-run planning.

  The model abstracts the v1 herd-wave contract implemented in
  crates/frankenterm-core/src/swarm_scheduler.rs. It checks synchronized cohort
  detection, deterministic rank/stagger planning, missing telemetry fail-closed
  admission, priority-protection severity reduction, cooldown/circuit-breaker
  non-mutation posture, and dry-run-only calendar rows.

  Run with TLC:
    java -jar tla2tools.jar -workers auto HerdWaveAdmission.tla
*)

\* coverage-metric:
\*   subsystem: herd-wave-admission
\*   declared-invariants: SafetyInvariants
\*   max-depth: 8
\*   branching-factor: 6
\*   threshold-pct: 0.002

EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS Panes, MinDistinct, CriticalDistinct, EmergencyDistinct,
          BaseStaggerMs, MaxStaggerMs, MissingTelemetrySeverity, MaxHistory

ASSUME
    /\ Panes \subseteq 1..8
    /\ Cardinality(Panes) \in 1..4
    /\ MinDistinct \in 1..4
    /\ CriticalDistinct \in MinDistinct..8
    /\ EmergencyDistinct \in CriticalDistinct..8
    /\ BaseStaggerMs \in 1..1000
    /\ MaxStaggerMs \in BaseStaggerMs..30000
    /\ MissingTelemetrySeverity \in 1..3
    /\ MaxHistory \in 1..2

SourceStates == {"fresh", "missing", "stale"}
Actions == {"admit", "defer", "degrade", "shed"}
Recommendations == {
    "noop",
    "admit_after_stagger",
    "defer_until_pressure_drops",
    "degrade_after_stagger",
    "require_operator_review_before_shed"
}
OverallStates == {
    "normal",
    "elevated",
    "critical",
    "emergency",
    "missing_telemetry",
    "stale_evidence",
    "priority_protected",
    "operator_override",
    "cooldown_active",
    "circuit_breaker_active"
}

CalendarRow == [
    rank               : Nat,
    delay_ms           : Nat,
    scheduled_after_ms : Nat,
    admission_action   : Actions,
    recommendation     : Recommendations,
    priority_protected : BOOLEAN,
    dry_run_only       : BOOLEAN,
    live_mutation      : BOOLEAN
]

HistoryRow == [
    active              : SUBSET Panes,
    source_state        : SourceStates,
    priority_units      : 0..3,
    operator_override   : BOOLEAN,
    cooldown_active     : BOOLEAN,
    circuit_active      : BOOLEAN,
    pressure_severity   : 0..3,
    raw_severity        : 0..3,
    effective_severity  : 0..3,
    admission_action    : Actions,
    recommended_stagger : Nat,
    cohort_max_stagger  : Nat,
    overall_state       : OverallStates
]

VARIABLES active, source_state, priority_units, operator_override,
          cooldown_active, circuit_active, pressure_severity, raw_severity,
          effective_severity, admission_action, recommended_stagger_ms,
          cohort_max_stagger_ms, overall_state, calendar, history

vars == <<active, source_state, priority_units, operator_override,
          cooldown_active, circuit_active, pressure_severity, raw_severity,
          effective_severity, admission_action, recommended_stagger_ms,
          cohort_max_stagger_ms, overall_state, calendar, history>>

----------------------------------------------------------------------------
\* Helpers
----------------------------------------------------------------------------

Min(a, b) == IF a <= b THEN a ELSE b
Max(a, b) == IF a >= b THEN a ELSE b
SubFloor(a, b) == IF a <= b THEN 0 ELSE a - b

DelayForRank(rank) == Min(rank * BaseStaggerMs, MaxStaggerMs)

PressureFor(cohort) ==
    LET distinct == Cardinality(cohort) IN
    IF distinct < Max(MinDistinct, 2) THEN 0
    ELSE IF distinct >= EmergencyDistinct THEN 3
    ELSE IF distinct >= CriticalDistinct THEN 2
    ELSE 1

Detected(cohort) == PressureFor(cohort) > 0

RawSeverityFor(cohort, source) ==
    IF source = "missing"
    THEN Max(PressureFor(cohort), MissingTelemetrySeverity)
    ELSE PressureFor(cohort)

ActionForSeverity(severity) ==
    CASE severity = 0 -> "admit"
      [] severity = 1 -> "defer"
      [] severity = 2 -> "degrade"
      [] OTHER -> "shed"

EffectiveSeverityFor(cohort, source, protection_units) ==
    LET after_protection ==
            SubFloor(RawSeverityFor(cohort, source), protection_units)
    IN
    IF /\ source = "missing"
       /\ ActionForSeverity(after_protection) = "admit"
    THEN 1
    ELSE after_protection

RecommendationFor(detected, action) ==
    IF ~detected THEN "noop"
    ELSE CASE action = "admit" -> "admit_after_stagger"
      [] action = "defer" -> "defer_until_pressure_drops"
      [] action = "degrade" -> "degrade_after_stagger"
      [] OTHER -> "require_operator_review_before_shed"

Rank(p, cohort) == Cardinality({q \in cohort : q < p})

CalendarFor(cohort, action, protected) ==
    [p \in cohort |->
        [rank               |-> Rank(p, cohort),
         delay_ms           |-> DelayForRank(Rank(p, cohort)),
         scheduled_after_ms |-> DelayForRank(Rank(p, cohort)),
         admission_action   |-> action,
         recommendation     |-> RecommendationFor(Detected(cohort), action),
         priority_protected |-> protected,
         dry_run_only       |-> TRUE,
         live_mutation      |-> FALSE]]

RecommendedStaggerFor(cohort) ==
    IF Detected(cohort) THEN DelayForRank(1) ELSE 0

CohortMaxStaggerFor(cohort) ==
    IF Detected(cohort) /\ Cardinality(cohort) > 0
    THEN DelayForRank(Cardinality(cohort) - 1)
    ELSE 0

OverallStateFor(cohort, source, protection_units, override, cooldown, circuit) ==
    IF source = "missing" THEN "missing_telemetry"
    ELSE IF source = "stale" THEN "stale_evidence"
    ELSE IF circuit THEN "circuit_breaker_active"
    ELSE IF cooldown THEN "cooldown_active"
    ELSE IF override THEN "operator_override"
    ELSE IF protection_units > 0 /\ RawSeverityFor(cohort, source) >
            EffectiveSeverityFor(cohort, source, protection_units)
         THEN "priority_protected"
    ELSE CASE PressureFor(cohort) = 0 -> "normal"
      [] PressureFor(cohort) = 1 -> "elevated"
      [] PressureFor(cohort) = 2 -> "critical"
      [] OTHER -> "emergency"

HistoryRecord(cohort, source, protection_units, override, cooldown, circuit) ==
    LET pressure == PressureFor(cohort)
        raw == RawSeverityFor(cohort, source)
        effective == EffectiveSeverityFor(cohort, source, protection_units)
        action == ActionForSeverity(effective)
    IN
    [active              |-> cohort,
     source_state        |-> source,
     priority_units      |-> protection_units,
     operator_override   |-> override,
     cooldown_active     |-> cooldown,
     circuit_active      |-> circuit,
     pressure_severity   |-> pressure,
     raw_severity        |-> raw,
     effective_severity  |-> effective,
     admission_action    |-> action,
     recommended_stagger |-> RecommendedStaggerFor(cohort),
     cohort_max_stagger  |-> CohortMaxStaggerFor(cohort),
     overall_state       |-> OverallStateFor(
                              cohort,
                              source,
                              protection_units,
                              override,
                              cooldown,
                              circuit)]

TypeOK ==
    /\ active \subseteq Panes
    /\ source_state \in SourceStates
    /\ priority_units \in 0..3
    /\ operator_override \in BOOLEAN
    /\ cooldown_active \in BOOLEAN
    /\ circuit_active \in BOOLEAN
    /\ pressure_severity \in 0..3
    /\ raw_severity \in 0..3
    /\ effective_severity \in 0..3
    /\ admission_action \in Actions
    /\ recommended_stagger_ms \in Nat
    /\ cohort_max_stagger_ms \in Nat
    /\ overall_state \in OverallStates
    /\ DOMAIN calendar = active
    /\ \A p \in DOMAIN calendar : calendar[p] \in CalendarRow
    /\ history \in Seq(HistoryRow)
    /\ Len(history) <= MaxHistory

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

Init ==
    /\ active = {}
    /\ source_state = "fresh"
    /\ priority_units = 0
    /\ operator_override = FALSE
    /\ cooldown_active = FALSE
    /\ circuit_active = FALSE
    /\ pressure_severity = PressureFor(active)
    /\ raw_severity = RawSeverityFor(active, source_state)
    /\ effective_severity = EffectiveSeverityFor(
            active,
            source_state,
            priority_units)
    /\ admission_action = ActionForSeverity(effective_severity)
    /\ recommended_stagger_ms = RecommendedStaggerFor(active)
    /\ cohort_max_stagger_ms = CohortMaxStaggerFor(active)
    /\ overall_state = OverallStateFor(
            active,
            source_state,
            priority_units,
            operator_override,
            cooldown_active,
            circuit_active)
    /\ calendar = CalendarFor(active, admission_action, FALSE)
    /\ history = <<>>

----------------------------------------------------------------------------
\* Actions
----------------------------------------------------------------------------

Evaluate(cohort, source, protection_units, override, cooldown, circuit) ==
    LET pressure == PressureFor(cohort)
        raw == RawSeverityFor(cohort, source)
        effective == EffectiveSeverityFor(cohort, source, protection_units)
        action == ActionForSeverity(effective)
        protected == protection_units > 0 /\ raw > effective
    IN
    /\ Len(history) < MaxHistory
    /\ cohort \subseteq Panes
    /\ source \in SourceStates
    /\ protection_units \in 0..3
    /\ override \in BOOLEAN
    /\ cooldown \in BOOLEAN
    /\ circuit \in BOOLEAN
    /\ active' = cohort
    /\ source_state' = source
    /\ priority_units' = protection_units
    /\ operator_override' = override
    /\ cooldown_active' = cooldown
    /\ circuit_active' = circuit
    /\ pressure_severity' = pressure
    /\ raw_severity' = raw
    /\ effective_severity' = effective
    /\ admission_action' = action
    /\ recommended_stagger_ms' = RecommendedStaggerFor(cohort)
    /\ cohort_max_stagger_ms' = CohortMaxStaggerFor(cohort)
    /\ overall_state' = OverallStateFor(
            cohort,
            source,
            protection_units,
            override,
            cooldown,
            circuit)
    /\ calendar' = CalendarFor(cohort, action, protected)
    /\ history' = Append(
            history,
            HistoryRecord(cohort, source, protection_units, override, cooldown, circuit))

StutterAtBound ==
    /\ Len(history) = MaxHistory
    /\ UNCHANGED vars

Next ==
    \/ \E cohort \in SUBSET Panes,
          source \in SourceStates,
          protection_units \in 0..3,
          override \in BOOLEAN,
          cooldown \in BOOLEAN,
          circuit \in BOOLEAN :
          Evaluate(cohort, source, protection_units, override, cooldown, circuit)
    \/ StutterAtBound

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants
----------------------------------------------------------------------------

PressureMatchesThresholds ==
    pressure_severity = PressureFor(active)

AdmissionMatchesEffectiveSeverity ==
    /\ raw_severity = RawSeverityFor(active, source_state)
    /\ effective_severity = EffectiveSeverityFor(
            active,
            source_state,
            priority_units)
    /\ admission_action = ActionForSeverity(effective_severity)

MissingTelemetryFailsClosed ==
    source_state = "missing" =>
        /\ admission_action # "admit"
        /\ effective_severity >= 1
        /\ overall_state = "missing_telemetry"

StaleEvidenceNotNormal ==
    source_state = "stale" => overall_state = "stale_evidence"

PriorityProtectionOnlyReducesSeverity ==
    /\ effective_severity <= raw_severity
    /\ priority_units > 0
    /\ raw_severity > effective_severity
    => \A p \in DOMAIN calendar : calendar[p].priority_protected

StaggerDelayIsBoundedAndRanked ==
    /\ recommended_stagger_ms = RecommendedStaggerFor(active)
    /\ cohort_max_stagger_ms = CohortMaxStaggerFor(active)
    /\ \A p \in DOMAIN calendar :
        /\ calendar[p].rank = Rank(p, active)
        /\ calendar[p].delay_ms = DelayForRank(calendar[p].rank)
        /\ calendar[p].delay_ms <= MaxStaggerMs
        /\ calendar[p].scheduled_after_ms = calendar[p].delay_ms
    /\ \A p, q \in DOMAIN calendar :
        p < q => calendar[p].rank < calendar[q].rank

DryRunNeverMutates ==
    \A p \in DOMAIN calendar :
        /\ calendar[p].dry_run_only
        /\ ~calendar[p].live_mutation

CooldownAndCircuitStayReadOnly ==
    /\ (cooldown_active \/ circuit_active) => DryRunNeverMutates
    /\ (source_state = "fresh" /\ (cooldown_active \/ circuit_active)) =>
        overall_state \in {"cooldown_active", "circuit_breaker_active"}

HistoryRowsMatchPlanner ==
    \A i \in DOMAIN history :
        LET row == history[i] IN
        /\ row.pressure_severity = PressureFor(row.active)
        /\ row.raw_severity = RawSeverityFor(row.active, row.source_state)
        /\ row.effective_severity =
           EffectiveSeverityFor(row.active, row.source_state, row.priority_units)
        /\ row.admission_action = ActionForSeverity(row.effective_severity)
        /\ row.recommended_stagger = RecommendedStaggerFor(row.active)
        /\ row.cohort_max_stagger = CohortMaxStaggerFor(row.active)
        /\ row.overall_state = OverallStateFor(
              row.active,
              row.source_state,
              row.priority_units,
              row.operator_override,
              row.cooldown_active,
              row.circuit_active)

SafetyInvariants ==
    /\ TypeOK
    /\ PressureMatchesThresholds
    /\ AdmissionMatchesEffectiveSeverity
    /\ MissingTelemetryFailsClosed
    /\ StaleEvidenceNotNormal
    /\ PriorityProtectionOnlyReducesSeverity
    /\ StaggerDelayIsBoundedAndRanked
    /\ DryRunNeverMutates
    /\ CooldownAndCircuitStayReadOnly
    /\ HistoryRowsMatchPlanner

----------------------------------------------------------------------------
\* Liveness/progress block
----------------------------------------------------------------------------

\* This is a safety-oriented admission model. The v1 herd-wave surface is
\* explicitly dry-run-only; progress/liveness is represented by the existence
\* of a deterministic calendar for every detected cohort rather than by live
\* queue or pane mutation. Future policy-gated mutation work must use a
\* separate model.

=============================================================================
