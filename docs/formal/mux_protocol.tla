----------------------------- MODULE mux_protocol -----------------------------
EXTENDS Naturals, Sequences, FiniteSets

\* Finite bound to keep TLC state-space manageable.
CONSTANTS MaxQueue, MaxMsgId
ASSUME /\ MaxQueue \in Nat \ {0}
       /\ MaxMsgId \in Nat \ {0}

VARIABLES
  state,
  nextMsgId,
  sendQ,
  recvQ,
  delivered,
  inFlight,
  errorReported

States == {"Idle", "Sending", "Receiving", "Error", "Reconnecting"}

SeqToSet(s) == {s[i] : i \in 1..Len(s)}

StrictlyIncreasing(s) ==
  \A i, j \in DOMAIN s : i < j => s[i] < s[j]

SentSet == 1..(nextMsgId - 1)

PendingSet == SeqToSet(sendQ) \cup SeqToSet(recvQ) \cup inFlight

Init ==
  /\ state = "Idle"
  /\ nextMsgId = 1
  /\ sendQ = << >>
  /\ recvQ = << >>
  /\ delivered = << >>
  /\ inFlight = {}
  /\ errorReported = {}

ClientSend ==
  /\ state \in {"Idle", "Receiving"}
  /\ Len(sendQ) < MaxQueue
  /\ nextMsgId <= MaxMsgId
  /\ LET msg == nextMsgId IN
     /\ sendQ' = Append(sendQ, msg)
     /\ inFlight' = inFlight \cup {msg}
     /\ nextMsgId' = msg + 1
  /\ state' = "Sending"
  /\ UNCHANGED <<recvQ, delivered, errorReported>>

WireDeliver ==
  /\ state \in {"Sending", "Receiving"}
  /\ Len(sendQ) > 0
  /\ LET msg == Head(sendQ) IN
     /\ sendQ' = Tail(sendQ)
     /\ recvQ' = Append(recvQ, msg)
     /\ inFlight' = inFlight
  /\ state' = "Receiving"
  /\ UNCHANGED <<nextMsgId, delivered, errorReported>>

ServerProcess ==
  /\ state = "Receiving"
  /\ Len(recvQ) > 0
  /\ LET msg == Head(recvQ) IN
     /\ recvQ' = Tail(recvQ)
     /\ delivered' = Append(delivered, msg)
     /\ inFlight' = inFlight \ {msg}
  /\ state' =
      IF Len(sendQ) = 0 /\ Len(Tail(recvQ)) = 0
      THEN "Idle"
      ELSE "Receiving"
  /\ UNCHANGED <<nextMsgId, sendQ, errorReported>>

ErrorDetected ==
  /\ state \in {"Sending", "Receiving"}
  /\ inFlight # {}
  /\ LET msg == CHOOSE m \in inFlight : TRUE IN
     /\ inFlight' = inFlight \ {msg}
     /\ errorReported' = errorReported \cup {msg}
  /\ state' = "Error"
  /\ UNCHANGED <<nextMsgId, sendQ, recvQ, delivered>>

ReconnectStart ==
  /\ state = "Error"
  /\ state' = "Reconnecting"
  /\ UNCHANGED <<nextMsgId, sendQ, recvQ, delivered, inFlight, errorReported>>

ReconnectComplete ==
  /\ state = "Reconnecting"
  /\ state' = "Idle"
  /\ UNCHANGED <<nextMsgId, sendQ, recvQ, delivered, inFlight, errorReported>>

Next ==
  \/ ClientSend
  \/ WireDeliver
  \/ ServerProcess
  \/ ErrorDetected
  \/ ReconnectStart
  \/ ReconnectComplete

\* Safety properties.
Safety_MessageTracked ==
  SentSet = PendingSet \cup SeqToSet(delivered) \cup errorReported

Safety_NoDuplicateDelivery ==
  Cardinality(SeqToSet(delivered)) = Len(delivered)

Safety_OrderedDelivery ==
  StrictlyIncreasing(delivered)

\* Temporal progress checks.
Liveness_LeavesError ==
  [](state = "Error" => <>(state # "Error"))

Liveness_EventuallyIdleOrError ==
  []<>(state = "Idle" \/ state = "Error")

vars == <<state, nextMsgId, sendQ, recvQ, delivered, inFlight, errorReported>>

Spec ==
  /\ Init
  /\ [][Next]_vars
  /\ WF_vars(WireDeliver)
  /\ WF_vars(ServerProcess)
  /\ WF_vars(ReconnectStart)
  /\ WF_vars(ReconnectComplete)

=============================================================================
