# ADR-0011: Two-Phase Resize Transactions and Explicit State Machine

**Status:** Accepted (normative for `wa-1u90p.2.*`)  
**Date:** 2026-02-13  
**Context:** `wa-1u90p.2.1` (Resize Control Plane)

## Context

Resize/font churn currently behaves like a stream of imperative operations. Under
storm conditions this causes stale work, queue inflation, and transient frame
artifacts. We need a control-plane contract that is explicit, testable, and
stable across scheduler/reflow/render tracks.

## Decision

Adopt a **two-phase per-pane transaction model** with **latest-intent wins**
semantics and an explicit state machine.

### Transaction Record

Each resize intent is represented as:

- `pane_id`
- `intent_seq` (monotonic per pane)
- `target_geometry` (rows/cols, viewport metadata)
- `font_key` (effective font/render key, if changed)
- `submitted_ts`
- `scheduler_class` (`interactive` or `background`)

### Two Phases

1. **Phase A: Intent Admission**
- Accept incoming intent.
- Coalesce queue to retain only the latest pending intent for that pane.
- Mark older pending work stale.

2. **Phase B: Execution + Commit**
- Run `prepare -> reflow -> present`.
- Enforce cancellation checks at each phase boundary.
- Commit atomically only if transaction is still latest.

## State Machine

### States

- `Idle`: no active transaction.
- `Queued`: active latest intent is admitted and waiting.
- `Preparing`: staging resources and prerequisites.
- `Reflowing`: layout/reflow computation in progress.
- `Presenting`: frame swap/presentation in progress.
- `Committed`: successful completion marker (ephemeral).
- `Cancelled`: superseded/stale transaction marker (ephemeral).
- `Failed`: terminal failure requiring recovery path.

### Transitions and Guards

| From | Event | Guard | To | Action |
|------|-------|-------|----|--------|
| `Idle` | `SubmitIntent(seq)` | `seq > latest_seq` | `Queued` | Set `active_seq=seq`, `latest_seq=seq` |
| `Queued` | `StartPrepare` | `active_seq == latest_seq` | `Preparing` | Begin phase B |
| `Preparing` | `StartReflow` | `active_seq == latest_seq` | `Reflowing` | Continue execution |
| `Reflowing` | `StartPresent` | `active_seq == latest_seq` | `Presenting` | Continue execution |
| `Presenting` | `Commit` | `active_seq == latest_seq` | `Committed` | Atomic present + release |
| `Committed` | `Finalize` | always | `Idle` | Clear active/latest |
| `Preparing/Reflowing/Presenting` | `SubmitIntent(new_seq)` | `new_seq > latest_seq` | same state | Update `latest_seq`; mark active stale |
| `Preparing/Reflowing/Presenting/Queued` | `BoundaryCheck` | `active_seq < latest_seq` | `Cancelled -> Queued` | Cancel stale active, promote latest |
| any non-`Idle` | `Fail` | unrecoverable error | `Failed` | Emit diagnostics + recovery policy |
| `Failed` | `Recover` | policy allows | `Idle` | Reset pane transaction context |

## Invariants

1. At most one active transaction per pane.
2. `intent_seq` is strictly monotonic per pane.
3. Commit is valid only for the latest known intent.
4. Stale work is cancellable and never commits presentation.
5. Cancellation is idempotent and safe at every phase boundary.
6. Queue depth for a pane is bounded (`<= 1` pending after coalescing).

## Latest-Intent and Cancellation Semantics

- New intent does not preempt mid-instruction; it is enforced at boundary checks.
- Boundary checks are mandatory between `Queued/Preparing/Reflowing/Presenting`.
- When stale work is detected (`active_seq < latest_seq`), stale active work is
  canceled and the latest intent is promoted to `Queued`.
- Multiple superseding intents collapse to one pending intent (highest `intent_seq`).

## Scheduler Interaction Contract

- Scheduler chooses pane work globally, but per-pane execution remains single-flight.
- `interactive` class gets latency-biased service; `background` gets throughput-biased service.
- Starvation protection uses aging/credit so background panes eventually run.
- Over-budget panes can be downgraded in class but not allowed to violate
  single-flight or stale-commit invariants.

## Failure Modes and Mitigations

### Deadlock

- Use strict lock ordering and avoid nested long-lived locks across phases.
- Keep blocking external operations outside critical sections.

### Starvation

- Introduce per-pane fairness aging in scheduler scoring.
- Enforce maximum deferral count before forced service.

### Queue Overload

- Maintain bounded per-pane pending queue (coalesced to latest).
- Drop superseded pending intents eagerly and count drops for telemetry.

## Alternatives Considered

1. **Single-phase immediate resize pipeline**
- Rejected: no robust stale-work boundary, poor storm behavior.

2. **Strict FIFO queue without cancellation**
- Rejected: stale intents commit too late and amplify artifacts.

3. **Global latest-only queue without per-pane isolation**
- Rejected: cross-pane coupling breaks fairness and locality.

## Migration Plan

1. Add transaction model and telemetry in shadow mode (`feature`/flag gated).
2. Wire boundary cancellation checks and stale-intent metrics.
3. Roll out latest-intent coalescing for resize scenarios in simulation/E2E.
4. Enable by default for canary cohorts with hard kill-switch.
5. Remove legacy path once SLO and artifact gates hold.

## Validation Fixture (Normative)

`crates/frankenterm-core/tests/resize_transaction_state_machine.rs` is the
contract fixture for transitions and cancellation semantics. It validates:

- legal phase progression (`Queued -> Preparing -> Reflowing -> Presenting -> Commit`)
- stale-intent cancellation at boundaries
- storm coalescing where only latest intent commits
- stale-commit prevention via boundary cancellation

## Downstream Usage

This ADR is normative for:

- `wa-1u90p.2.2` (per-pane coalescer + cancellation tokens)
- `wa-1u90p.2.3` (global scheduler work classes)
- `wa-1u90p.5.1` and `wa-1u90p.5.7` (async LocalPane resize + lock graph)
- `wa-1u90p.6.1` (enforceable invariants)

