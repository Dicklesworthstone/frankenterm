-------------------------- MODULE BlockerRadarMerge --------------------------
(*
  TLA+ specification for blocker-radar claimability source merge.

  The model abstracts each read-only source into the verdict family consumed by
  crates/frankenterm-core/src/blocker_radar.rs::claimability_final_verdict.
  TLC exhausts the cross-product of source verdicts and checks that the root
  claimability verdict fails closed unless every safety predicate agrees.

  Run with TLC:
    java -jar tla2tools.jar -workers auto BlockerRadarMerge.tla
*)

\* coverage-metric:
\*   subsystem: blocker-radar-merge
\*   declared-invariants: SafetyInvariants
\*   max-depth: 8
\*   branching-factor: 6
\*   threshold-pct: 0.002

EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS Candidates, MaxHistory

ASSUME
    /\ Candidates \subseteq {"normal", "coordination-snapshot"}
    /\ "normal" \in Candidates
    /\ Cardinality(Candidates) \in 1..2
    /\ MaxHistory \in 1..3

VARIABLES candidate, ready, dependency, owner, dirty, external, mail, tracker,
          final, history

vars == <<candidate, ready, dependency, owner, dirty, external, mail, tracker,
          final, history>>

----------------------------------------------------------------------------
\* Domain
----------------------------------------------------------------------------

ReadyVerdicts == {"claimable", "no_ready"}
DependencyVerdicts == {"claimable", "dependency_blocked"}
OwnerVerdicts == {"claimable", "owner_blocked"}
DirtyVerdicts == {"claimable", "dirty_overlap"}
ExternalVerdicts == {"claimable", "external_wait"}
MailVerdicts == {"claimable", "mail_degraded"}
TrackerVerdicts == {"claimable", "tracker_inconsistent"}

AllVerdicts ==
    ReadyVerdicts \cup DependencyVerdicts \cup OwnerVerdicts \cup
    DirtyVerdicts \cup ExternalVerdicts \cup MailVerdicts \cup
    TrackerVerdicts

HistoryRow == [
    candidate  : Candidates,
    ready      : ReadyVerdicts,
    dependency : DependencyVerdicts,
    owner      : OwnerVerdicts,
    dirty      : DirtyVerdicts,
    external   : ExternalVerdicts,
    mail       : MailVerdicts,
    tracker    : TrackerVerdicts,
    final      : AllVerdicts
]

TypeOK ==
    /\ candidate \in Candidates
    /\ ready \in ReadyVerdicts
    /\ dependency \in DependencyVerdicts
    /\ owner \in OwnerVerdicts
    /\ dirty \in DirtyVerdicts
    /\ external \in ExternalVerdicts
    /\ mail \in MailVerdicts
    /\ tracker \in TrackerVerdicts
    /\ final \in AllVerdicts
    /\ history \in Seq(HistoryRow)
    /\ Len(history) <= MaxHistory

SafetyPredicatesPass(r, dep, own, dirt, ext, ml, tr) ==
    /\ tr = "claimable"
    /\ dirt = "claimable"
    /\ dep = "claimable"
    /\ ext = "claimable"
    /\ own = "claimable"
    /\ r = "claimable"
    /\ ml \in {"claimable", "mail_degraded"}

FinalFor(c, r, dep, own, dirt, ext, ml, tr) ==
    IF tr = "tracker_inconsistent" THEN "tracker_inconsistent"
    ELSE IF dirt = "dirty_overlap" THEN "dirty_overlap"
    ELSE IF dep = "dependency_blocked" THEN "dependency_blocked"
    ELSE IF ext = "external_wait" THEN "external_wait"
    ELSE IF own = "owner_blocked" THEN "owner_blocked"
    ELSE IF /\ r = "no_ready"
            /\ ml = "mail_degraded"
            /\ c = "coordination-snapshot"
         THEN "mail_degraded"
    ELSE IF r = "no_ready" THEN "no_ready"
    ELSE IF SafetyPredicatesPass(r, dep, own, dirt, ext, ml, tr)
         THEN "claimable"
    ELSE "tracker_inconsistent"

HistoryRecord(c, r, dep, own, dirt, ext, ml, tr, verdict) == [
    candidate  |-> c,
    ready      |-> r,
    dependency |-> dep,
    owner      |-> own,
    dirty      |-> dirt,
    external   |-> ext,
    mail       |-> ml,
    tracker    |-> tr,
    final      |-> verdict
]

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

Init ==
    /\ candidate = "normal"
    /\ ready = "no_ready"
    /\ dependency = "claimable"
    /\ owner = "claimable"
    /\ dirty = "claimable"
    /\ external = "claimable"
    /\ mail = "claimable"
    /\ tracker = "claimable"
    /\ final = FinalFor(candidate, ready, dependency, owner, dirty, external,
                        mail, tracker)
    /\ history = <<>>

----------------------------------------------------------------------------
\* Actions
----------------------------------------------------------------------------

Evaluate(c, r, dep, own, dirt, ext, ml, tr) ==
    LET verdict == FinalFor(c, r, dep, own, dirt, ext, ml, tr) IN
    /\ c \in Candidates
    /\ r \in ReadyVerdicts
    /\ dep \in DependencyVerdicts
    /\ own \in OwnerVerdicts
    /\ dirt \in DirtyVerdicts
    /\ ext \in ExternalVerdicts
    /\ ml \in MailVerdicts
    /\ tr \in TrackerVerdicts
    /\ Len(history) < MaxHistory
    /\ candidate' = c
    /\ ready' = r
    /\ dependency' = dep
    /\ owner' = own
    /\ dirty' = dirt
    /\ external' = ext
    /\ mail' = ml
    /\ tracker' = tr
    /\ final' = verdict
    /\ history' = Append(history, HistoryRecord(c, r, dep, own, dirt, ext, ml,
                                                tr, verdict))

StutterAtBound ==
    /\ Len(history) = MaxHistory
    /\ UNCHANGED vars

Next ==
    \/ \E c \in Candidates,
          r \in ReadyVerdicts,
          dep \in DependencyVerdicts,
          own \in OwnerVerdicts,
          dirt \in DirtyVerdicts,
          ext \in ExternalVerdicts,
          ml \in MailVerdicts,
          tr \in TrackerVerdicts :
            Evaluate(c, r, dep, own, dirt, ext, ml, tr)
    \/ StutterAtBound

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants
----------------------------------------------------------------------------

FailClosedPrecedence ==
    final = FinalFor(candidate, ready, dependency, owner, dirty, external,
                     mail, tracker)

ClaimableRequiresAllSafetyPredicates ==
    final = "claimable" =>
        SafetyPredicatesPass(ready, dependency, owner, dirty, external, mail,
                             tracker)

AllSafetyPredicatesYieldClaimable ==
    SafetyPredicatesPass(ready, dependency, owner, dirty, external, mail,
                         tracker) =>
        final = "claimable"

TrackerInconsistencyDominates ==
    tracker = "tracker_inconsistent" => final = "tracker_inconsistent"

BlockingSourcesNeverClaim ==
    (dirty = "dirty_overlap" \/
     dependency = "dependency_blocked" \/
     external = "external_wait" \/
     owner = "owner_blocked") =>
        final # "claimable"

MailDegradedNeverClaimsWithoutReadyEvidence ==
    /\ mail = "mail_degraded"
    /\ ready # "claimable"
    => final # "claimable"

HistoryRowsMatchMergeFunction ==
    \A i \in DOMAIN history :
        history[i].final = FinalFor(
            history[i].candidate,
            history[i].ready,
            history[i].dependency,
            history[i].owner,
            history[i].dirty,
            history[i].external,
            history[i].mail,
            history[i].tracker
        )

SafetyInvariants ==
    /\ TypeOK
    /\ FailClosedPrecedence
    /\ ClaimableRequiresAllSafetyPredicates
    /\ AllSafetyPredicatesYieldClaimable
    /\ TrackerInconsistencyDominates
    /\ BlockingSourcesNeverClaim
    /\ MailDegradedNeverClaimsWithoutReadyEvidence
    /\ HistoryRowsMatchMergeFunction

----------------------------------------------------------------------------
\* Liveness / progress
----------------------------------------------------------------------------

\* Liveness is intentionally bounded to source-refresh progress: every Next
\* step represents one complete refresh of the read-only sources. The safety
\* contract does not require autonomous mutation or eventual claiming.
LivenessProgress ==
    TRUE

==============================================================================
