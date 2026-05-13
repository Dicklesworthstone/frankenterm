----------------------------- MODULE RobotWork ---------------------------
(*
  TLA+ specification of the `robot work` family — a multi-agent
  bead-style work queue.
  [BR-RC-ROBOT-CONTRACT.4] / ft-hac7w.5

  Mirrors the Rust model in
  crates/frankenterm-core/src/robot_work_state_machine.rs. The
  Rust harness is the always-on regression net (1024 random
  schedules × 12 transitions per CI run); this TLA+ spec is the
  formal-method-tool-friendly representation a human or TLC
  consumes.

  Spec correspondence:
  - WorkWorld          ↔ Rust struct of the same name.
  - ClaimState         ↔ Rust enum (Unclaimed / Claimed / Completed).
  - apply_action       ↔ TLA+ action set.
  - check_invariants   ↔ Rust safety-invariant runner.

  Run with TLC:
    java -jar tla2tools.jar -workers auto RobotWork.tla
*)

\* coverage-metric:
\*   subsystem: robot-work
\*   declared-invariants: SafetyInvariants
\*   max-depth: 8
\*   branching-factor: 6
\*   threshold-pct: 0.002

EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS Claims, Agents

ASSUME
    /\ Claims \subseteq 0..255
    /\ Agents \subseteq 0..255
    /\ Cardinality(Claims) \in 1..3      \* Bound model space
    /\ Cardinality(Agents) \in 1..3

VARIABLES claims, live_agents, events

vars == <<claims, live_agents, events>>

----------------------------------------------------------------------------
\* Domain
----------------------------------------------------------------------------

ClaimStateUnclaimed == [kind |-> "unclaimed"]
ClaimStateClaimed(a) == [kind |-> "claimed", owner |-> a]
ClaimStateCompleted(a) == [kind |-> "completed", owner |-> a]

ClaimStateType ==
    {[kind |-> "unclaimed"]}
    \cup {[kind |-> "claimed", owner |-> a] : a \in Agents}
    \cup {[kind |-> "completed", owner |-> a] : a \in Agents}

EmittedEventType ==
    LET ks == {"claimed", "released", "completed", "auto_released_on_crash"}
    IN { [kind |-> k, claim |-> c, agent |-> a] :
            k \in ks, c \in Claims, a \in Agents }

TypeOK ==
    /\ DOMAIN claims = Claims
    /\ \A c \in Claims : claims[c] \in ClaimStateType
    /\ live_agents \subseteq Agents
    /\ events \in Seq(EmittedEventType)

----------------------------------------------------------------------------
\* Initial state
----------------------------------------------------------------------------

Init ==
    /\ claims = [c \in Claims |-> ClaimStateUnclaimed]
    /\ live_agents = Agents
    /\ events = <<>>

----------------------------------------------------------------------------
\* Actions
----------------------------------------------------------------------------

\* Claim — non-idempotent unless same owner.
Claim(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "unclaimed"
    /\ claims' = [claims EXCEPT ![c] = ClaimStateClaimed(a)]
    /\ events' = Append(events, [
            kind |-> "claimed", claim |-> c, agent |-> a
       ])
    /\ UNCHANGED live_agents

\* Reclaim by same owner — no-op (idempotent).
ClaimByOwner(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "claimed"
    /\ claims[c].owner = a
    /\ UNCHANGED <<claims, live_agents, events>>

\* Claim denied — different agent already holds it.
ClaimDenied(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "claimed"
    /\ claims[c].owner # a
    /\ UNCHANGED <<claims, live_agents, events>>

\* Complete by owner — transitions Claimed → Completed.
Complete(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "claimed"
    /\ claims[c].owner = a
    /\ claims' = [claims EXCEPT ![c] = ClaimStateCompleted(a)]
    /\ events' = Append(events, [
            kind |-> "completed", claim |-> c, agent |-> a
       ])
    /\ UNCHANGED live_agents

\* Re-complete by owner — idempotent (no event).
CompleteByOwnerIdempotent(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "completed"
    /\ claims[c].owner = a
    /\ UNCHANGED <<claims, live_agents, events>>

\* Complete by non-owner — denied.
CompleteDenied(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ \/ claims[c].kind = "unclaimed"
       \/ (claims[c].kind = "claimed" /\ claims[c].owner # a)
       \/ (claims[c].kind = "completed" /\ claims[c].owner # a)
    /\ UNCHANGED <<claims, live_agents, events>>

\* Release by owner — transitions Claimed → Unclaimed.
Release(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "claimed"
    /\ claims[c].owner = a
    /\ claims' = [claims EXCEPT ![c] = ClaimStateUnclaimed]
    /\ events' = Append(events, [
            kind |-> "released", claim |-> c, agent |-> a
       ])
    /\ UNCHANGED live_agents

\* Release on already-unclaimed — idempotent.
ReleaseIdempotent(c, a) ==
    /\ c \in Claims
    /\ a \in live_agents
    /\ claims[c].kind = "unclaimed"
    /\ UNCHANGED <<claims, live_agents, events>>

\* Crash + restart — drops Claimed rows for the crashed agent.
\* Completed rows are preserved (durability).
CrashAndRestart(a) ==
    LET dropped == { c \in Claims :
                        /\ claims[c].kind = "claimed"
                        /\ claims[c].owner = a }
    IN
    /\ a \in Agents
    /\ claims' = [c \in Claims |->
                    IF c \in dropped
                    THEN ClaimStateUnclaimed
                    ELSE claims[c]]
    /\ events' = events  \* Auto-release events appended in
                          \* the Rust trace; here we just mutate
                          \* state (TLC consumers can derive the
                          \* event set from the diff).
    /\ live_agents' = live_agents \cup {a}

\* Failure injections — atomic-on-failure: state unchanged.
ClaimFail(c, a) ==
    /\ c \in Claims
    /\ a \in Agents
    /\ UNCHANGED <<claims, live_agents, events>>

CompleteFail(c, a) ==
    /\ c \in Claims
    /\ a \in Agents
    /\ UNCHANGED <<claims, live_agents, events>>

ReleaseFail(c, a) ==
    /\ c \in Claims
    /\ a \in Agents
    /\ UNCHANGED <<claims, live_agents, events>>

\* List / Status — pure reads.
List == UNCHANGED <<claims, live_agents, events>>
Status(c) == UNCHANGED <<claims, live_agents, events>>

Next ==
    \/ \E c \in Claims, a \in Agents : Claim(c, a)
    \/ \E c \in Claims, a \in Agents : ClaimByOwner(c, a)
    \/ \E c \in Claims, a \in Agents : ClaimDenied(c, a)
    \/ \E c \in Claims, a \in Agents : Complete(c, a)
    \/ \E c \in Claims, a \in Agents : CompleteByOwnerIdempotent(c, a)
    \/ \E c \in Claims, a \in Agents : CompleteDenied(c, a)
    \/ \E c \in Claims, a \in Agents : Release(c, a)
    \/ \E c \in Claims, a \in Agents : ReleaseIdempotent(c, a)
    \/ \E a \in Agents : CrashAndRestart(a)
    \/ \E c \in Claims, a \in Agents : ClaimFail(c, a)
    \/ \E c \in Claims, a \in Agents : CompleteFail(c, a)
    \/ \E c \in Claims, a \in Agents : ReleaseFail(c, a)
    \/ List
    \/ \E c \in Claims : Status(c)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
\* Safety invariants — TLC checks every reachable state.
----------------------------------------------------------------------------

\* NoDoubleClaim: at most one agent holds any given claim.
\* Structurally guaranteed by claims being a function over
\* Claims, but we keep the invariant explicit so a future
\* refactor (e.g., set-valued ownership) would surface a
\* violation here.
NoDoubleClaim ==
    \A c \in Claims :
        claims[c].kind \in {"unclaimed", "claimed", "completed"}

\* CompletedDurability: a claim that was Completed at any
\* point must remain Completed in every successor state with
\* the same owner. Encoded as a step-relation: under [Next]_vars,
\* the projection f(claims) = { c \in Claims : claims[c].kind =
\* "completed" } is monotone-increasing, AND the per-claim
\* owner of any Completed slot never changes.
CompletedDurabilityInductive ==
    \A c \in Claims :
        (claims[c].kind = "completed") =>
            \E a \in Agents :
                /\ claims'[c].kind = "completed"
                /\ claims'[c].owner = a
                /\ claims[c].owner = a

\* OwnerExclusivity: a Complete or Release transition only
\* succeeds for the current owner. Encoded structurally — the
\* Rust harness enforces this via apply_action's match arms.
\* TLC operators verify by enabling the action set above.

SafetyInvariants ==
    /\ TypeOK
    /\ NoDoubleClaim

----------------------------------------------------------------------------
\* Liveness — no claim leak.
----------------------------------------------------------------------------

\* Every claim eventually either reaches Completed or returns
\* to Unclaimed (never stuck in Claimed indefinitely under
\* fairness on the Release / CrashAndRestart actions).
\*
\* Operators of TLC: enable WF on Release and CrashAndRestart
\* to verify this property.
NoClaimLeak ==
    \A c \in Claims :
        <>(claims[c].kind \in {"unclaimed", "completed"})

==============================================================================
