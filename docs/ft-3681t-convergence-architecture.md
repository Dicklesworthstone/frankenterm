# ft-3681t.1.3 Convergence Architecture Spec

This document defines the target architecture contract for FrankenTerm as the
single swarm terminal and control-plane runtime. It is intended to unblock:

- `ft-3681t.2.1` native mux lifecycle engine
- `ft-3681t.5.1` connector fabric embedding
- `ft-3681t.6.1` unified policy DSL/engine
- `ft-3681t.1.4` execution tracks and critical path
- `ft-3681t.1.5` architecture traceability verification

## 1. Scope and Constraints

### Scope

- Define component boundaries, contracts, and data/control flow.
- Define parity and supersets relative to NTM.
- Define integration boundaries for flywheel connectors.
- Define failure/degradation/rollback behavior as first-class architecture.

### Constraints

- `ft` is not a thin wrapper around WezTerm. Compatibility backends are a
  migration bridge, not the product boundary.
- Observation remains passive-first (discover/capture/detect/store has no send
  side effects).
- Mutating actions are always policy-gated and auditable.
- Machine interfaces (`ft robot`, MCP) are stable-envelope contracts.
- Quality gates are mandatory for all downstream implementation beads.

## 2. Target System Topology

```text
                         Human CLI / Operator UX
                                   |
                        Robot Mode + MCP Surfaces
                                   |
                +------------------+------------------+
                |                                     |
        Orchestration Runtime                   Connector Fabric
   (scheduler, plans, workflows, tx)      (host runtime, bridges, mesh)
                |                                     |
                +------------------+------------------+
                                   |
                     Policy and Governance Plane
          (authz, approvals, quotas, isolation, audit, kill-switch)
                                   |
         +-------------------------+--------------------------+
         |                                                    |
    Native Mux Subsystem                               Observability Plane
 (session/window/pane lifecycle, I/O,                    (events, metrics,
  layouts, transport, persistence)                     diagnostics, replay)
         |                                                    |
         +-------------------------+--------------------------+
                                   |
                          Storage + State Substrate
            (SQLite/FTS5, snapshots, event log, action log, checkpoints)
```

## 3. Domain Decomposition

### 3.1 Native Mux Subsystem

Owns:

- session/window/tab/pane lifecycle
- pane metadata and text transport primitives
- command fanout/broadcast/interrupt primitives
- layout semantics and future native GUI handoff boundaries

Contracts:

- deterministic identity model for sessions/windows/panes
- idempotent mutation semantics for pane control operations
- snapshot/checkpoint hooks consumable by session restore and replay

Primary downstream beads:

- `ft-3681t.2.1`, `.2.2`, `.2.3`, `.2.4`, `.2.5`

### 3.2 Swarm Orchestration Runtime

Owns:

- fleet launch, assignment, scheduling, and rebalancing
- workflow/pipeline runtime with retries/recovery policies
- lock/conflict orchestration and handoff protocol

Contracts:

- explicit work-claim and reservation semantics
- deterministic state transitions for runs/assignments/actions
- event-sourced execution records for replay and causality analysis

Primary downstream beads:

- `ft-3681t.3.1` through `.3.7`

### 3.3 Robot and MCP Control Plane

Owns:

- machine contracts for all operational control and introspection
- idempotency/dedupe semantics for mutation commands
- streaming/wait interfaces for deterministic automation

Contracts:

- stable response envelope (`ok`, `data`, `error`, `elapsed_ms`, `version`)
- structured error taxonomy with actionable hints
- schema versioning with backward-compatible contract tests

Primary downstream beads:

- `ft-3681t.4.1` through `.4.6`

### 3.4 Connector Fabric (FCP Integration)

Owns:

- connector host runtime and signed package governance
- outbound bridge (ft events -> connector actions)
- inbound bridge (connector signals -> workflows/robot events)
- connector lifecycle operations and multi-host mesh federation

Contracts:

- capability-scoped execution envelopes and sandbox zones
- credential broker integration (no raw secret fanout)
- connector event schema and audit-chain continuity

Primary downstream beads:

- `ft-3681t.5.1` through `.5.15`

### 3.5 Policy and Governance Plane

Owns:

- identity graph and least-privilege authorization
- unified policy DSL + runtime evaluator
- approval/revocation/quarantine and tenant isolation controls
- immutable audit and compliance export pipeline

Contracts:

- default-deny for ambiguous/high-risk mutations
- explainable decisions with reason codes and evidence pointers
- actor- and namespace-scoped enforcement for every surface

Primary downstream beads:

- `ft-3681t.6.1` through `.6.6`

### 3.6 Observability and Reliability Plane

Owns:

- telemetry schema and live dashboards/alerts
- capacity governor and chaos/scale validation harnesses
- disaster recovery and restore drills

Contracts:

- every critical path emits correlated structured telemetry
- degraded-mode state is explicit, queryable, and replayable
- recovery steps are codified and drillable

Primary downstream beads:

- `ft-3681t.7.1` through `.7.6`

### 3.7 Migration and Operator UX Planes

Migration plane owns parity matrix, staged cutover, rollback gates, and NTM
decommission evidence. Operator UX plane owns command-center, timeline replay,
intervention console, and runbook overlays.

Primary downstream beads:

- migration: `ft-3681t.8.1` through `.8.6`
- UX: `ft-3681t.9.1` through `.9.7`

## 4. System Flows

### 4.1 Passive Observation Flow

```text
Mux/Backend discovery -> capture deltas -> delta/gap extraction
-> storage append -> pattern detection -> event bus fanout
-> observability + workflow eligibility
```

Guarantees:

- no mutation side effects in observation path
- explicit gap recording for uncertainty
- stable event identity keys for dedupe/suppression

### 4.2 Mutation/Action Flow

```text
Human/Robot/MCP request -> policy evaluate -> (allow|deny|require_approval)
-> mutation execution -> audit persistence -> event emission
```

Guarantees:

- all mutations are policy mediated
- approvals are explicit and scoped
- execution outcomes are auditable and replay-linked

### 4.3 Connector-Integrated Automation Flow

```text
ft events -> outbound connector bridge -> external system action
-> inbound connector result/signal -> ft workflow/robot event
-> policy/audit -> operator visibility
```

Guarantees:

- no connector side effect without policy context
- bridge failures are surfaced as typed events
- dead-letter and replay paths are available for outage recovery

### 4.4 Recovery and Rollback Flow

```text
checkpoint/snapshot capture -> failure/degradation trigger
-> rollback decision gate -> restore/replay -> post-incident evidence bundle
```

Guarantees:

- rollback capability is designed in, not retrofitted
- degraded behavior is deterministic and documented
- recovery evidence is machine-verifiable

## 5. NTM Parity and Deliberate Supersets

NTM parity targets are mapped into native ft planes:

- session and swarm operations -> native mux + orchestration runtime
- machine APIs -> robot + MCP control-plane contracts
- safety and approvals -> policy/governance plane
- operational control and insights -> observability + operator UX planes

Supersets (intentional):

- stronger policy explainability and tenant isolation
- connector-native fabric instead of external bolt-on orchestration
- first-class replay/forensics path linked to workflow and policy decisions
- explicit idempotency and transaction semantics for robot mutations

## 6. Failure, Degradation, and Rollback Matrix

| Domain | Failure Mode | Degradation Path | Rollback Path |
|---|---|---|---|
| Capture/ingest | queue saturation, missed overlap | adaptive backoff + explicit gaps | replay-based reconciliation from checkpoints |
| Mux control | command transport failures | retry with bounded backoff + quarantine pane/session | revert to last stable checkpoint and reopen control session |
| Orchestration | scheduler deadlock/conflict | lock arbitration + safe handoff | abort run, replay assignment log, replan |
| Connector mesh | external outage/rate limit | circuit breaker + dead-letter queue | replay pending actions after health gate clears |
| Policy plane | rule engine error/ambiguity | fail-closed for mutating actions | roll back to previous signed policy pack |
| Robot/MCP | schema or contract drift | compat mode with explicit warning + reduced surface | pin older schema set and replay conformance tests |
| Storage | writer lag/corruption risk | bounded queues + read-only protection mode | restore from snapshot and rebuild indexes |

## 7. Implementation Contracts for Downstream Beads

Each downstream bead must provide:

- explicit interface definition (types, schemas, invariants)
- deterministic state-transition model
- structured telemetry fields with correlation IDs
- quality-gate evidence (unit, integration, e2e, failure-injection, recovery)
- rollback/degradation behavior proof

Contract mapping:

- `ft-3681t.2.*`: native mux identities, lifecycle transitions, transport invariants
- `ft-3681t.3.*`: scheduler fairness, reservation/handoff safety, replayable runs
- `ft-3681t.4.*`: robot/mcp schema contracts, idempotency and dedupe guarantees
- `ft-3681t.5.*`: connector capability envelopes, bridge semantics, outage controls
- `ft-3681t.6.*`: policy DSL semantics, authz graph invariants, governance exports
- `ft-3681t.7.*`: telemetry schema/SLO definitions, chaos/drill acceptance
- `ft-3681t.8.*`: parity matrix, cutover guardrails, rollback rehearsal proofs
- `ft-3681t.9.*`: operator UX workflow fidelity and intervention safety

## 8. Validation and Evidence Requirements

For this architecture and each dependent bead:

1. Unit tests for happy path, boundaries, and explicit failure paths.
2. Integration tests for cross-plane contracts and at least one degraded path.
3. Deterministic end-to-end scenarios with failure injection and recovery.
4. Structured logs carrying timestamp, subsystem, correlation ID, scenario ID,
   inputs, decisions, outcomes, reason/error code.
5. Artifact bundles (stdout/stderr/logs/snapshots/reports), documented command
   list, and heavy compute through `rch exec -- ...`.

## 9. Current Merge Points From Discovery Beads

This spec is compatible with ongoing `ft-3681t.1.1` and `.1.2` work and expects
their finalized inventories to be merged into:

- parity matrix appendix for NTM capability mapping
- connector capability/security appendix for FCP integration constraints

Until those are finalized, this document remains the architecture contract and
interface boundary baseline for implementation beads.
