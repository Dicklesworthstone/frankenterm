# Cross-Process Causality Clocks Are Deferred

Bead: `ft-chmai`  
Status: Deferred until distributed event delivery requires it

## Decision

Do not add Lamport timestamps, vector clocks, or Hybrid Logical Clocks to the
current `events.rs` event bus.

The current event bus is an in-process fanout surface. Ordering is handled by
local publication order, recorder sequence domains, and existing correlation
fields. Adding distributed logical clocks before there is cross-process event
delivery would create unused protocol surface without improving correctness.

## Current Model

- `crates/frankenterm-core/src/events.rs` publishes local events through a
  bounded broadcast channel.
- Recorder events already carry deterministic sequence and causality metadata:
  `event_id`, `sequence`, `correlation_id`, and parent/trigger/root links.
- Distributed mode is currently a transport and security surface. It does not
  define causal broadcast across independent event producers.

That means the system can reconstruct local and recorded causal chains, but it
does not need distributed happened-before comparisons between independent
processes today.

## Revisit Triggers

Reopen this decision when one of these becomes a concrete requirement:

1. A distributed event bus accepts events from multiple mux/runtime processes.
2. Cross-host event correlation must distinguish causal order from concurrency.
3. Event replay or workflow automation depends on causal broadcast semantics.
4. Operators need a stable cross-process total order that survives clock skew.

## Future Shape

When the triggers exist, use a staged clock model:

1. Lamport clock for a compact happened-before baseline and deterministic
   `(counter, process_id)` tie-breaking.
2. Vector clock for surfaces that must preserve concurrency information.
3. Hybrid Logical Clock only when wall-clock proximity is useful and bounded
   skew assumptions are explicitly documented.

The future implementation should keep logical-clock state out of the current
local `Event` enum until it crosses a process boundary. The boundary envelope is
the right place to carry `process_id`, logical timestamp metadata, and versioned
comparison semantics.

## Acceptance Criteria For A Future Implementation

- Defines the producer identity domain and restart behavior.
- Specifies clock merge rules for send, receive, replay, and gap events.
- Preserves existing recorder causality fields instead of replacing them.
- Includes fixtures covering causal order, concurrency, clock skew, and process
  restart.
- Keeps local-only event publication free of distributed clock overhead.
