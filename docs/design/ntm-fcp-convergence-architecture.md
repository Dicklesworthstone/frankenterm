# FrankenTerm Convergence Architecture Contract (NTM Superset + Connector Fabric)

- Bead: `ft-3681t.1`
- Status: supplemental architecture contract (implementation-anchored matrix + gates)
- Date: 2026-03-02

## Objective
Define an explicit, implementation-grounded architecture contract for converging NTM-style swarm orchestration and flywheel connector capabilities into FrankenTerm-native control/data/policy planes.

This artifact is intended to remove ambiguity for downstream implementation beads (`ft-3681t.1.4`, `ft-3681t.1.5`, `ft-3681t.2.*`, `ft-3681t.5.*`) as parallel agents scale out.

## Canonical Precedence

- Canonical convergence spec: `docs/ft-3681t-convergence-architecture.md` (`ft-3681t.1.3` artifact).
- This document is supplemental and implementation-anchored:
  - capability matrix tied to concrete code surfaces
  - gap mapping for NTM and connector-fabric integration
  - execution-track entry/exit gates for the `ft-3681t.1.*` chain
- If this document conflicts with the canonical convergence spec, the canonical spec takes precedence and this document should be updated.

## Current FrankenTerm Capability Baseline

| Domain | Current FrankenTerm Capability | Surface | Implementation Anchors |
|---|---|---|---|
| Runtime observation | Pane discovery, capture, delta extraction, gap semantics | `ft watch`, `ft status` | `crates/frankenterm-core/src/runtime.rs`, `crates/frankenterm-core/src/ingest.rs` |
| Detection | Multi-agent pattern detection (codex/claude/gemini/wezterm) | `ft robot rules *`, `ft events` | `crates/frankenterm-core/src/patterns.rs`, `crates/frankenterm-core/src/events.rs` |
| Durable eventing | Persisted events with triage, labels, notes, dedupe keys | `ft events`, `ft robot events` | `crates/frankenterm-core/src/storage.rs` (`events`, `event_labels`, `event_notes`) |
| Search | Lexical FTS + semantic/hybrid facade | `ft search`, `ft robot search` | `crates/frankenterm-core/src/storage.rs`, `crates/frankenterm-core/src/search/*` |
| Workflow automation | Event-driven workflows with pane locks and persistence | `ft workflow *`, `ft robot workflow *` | `crates/frankenterm-core/src/workflows/*`, `crates/frankenterm-core/src/events.rs`, `crates/frankenterm-core/src/storage.rs` |
| Safe mutation gates | Capability/rate-limit/approval policy before send/run | `ft send`, `ft approve`, MCP tools | `crates/frankenterm-core/src/policy.rs`, `crates/frankenterm-core/src/approval.rs`, `crates/frankenterm-core/src/command_guard.rs` |
| Machine API | Robot envelope + MCP parity | `ft robot *`, `ft mcp serve` | `crates/frankenterm-core/src/robot_types.rs`, `crates/frankenterm-core/src/mcp.rs` |
| Session continuity | Snapshots/session restore tracks | `ft snapshot *`, `ft session *` | `crates/frankenterm-core/src/snapshot_engine.rs`, `crates/frankenterm-core/src/session_restore.rs` |
| Reservation/coordination | Pane-level reservations and ownership metadata | `ft reserve`, `ft reservations` | `crates/frankenterm-core/src/storage.rs` (`pane_reservations`), `crates/frankenterm-core/src/policy.rs` |
| Headless/remote mux | Dedicated mux server + implementation crate | `frankenterm-mux-server` | `crates/frankenterm-mux-server*` |
| Distributed transport (feature-gated) | Agent/aggregator mode for multi-host ingest | `ft distributed agent` | `crates/frankenterm-core/src/distributed.rs`, CLI distributed subcommands in `crates/frankenterm/src/main.rs` |

## NTM Capability Mapping (Parity / Superset / Gap)

| NTM-style Capability Area | FrankenTerm Status | Classification | Gap / Required Hardening |
|---|---|---|---|
| Fleet pane/session inventory | Implemented via state/list/status APIs | Parity | Formalize cross-host identity model for multi-aggregator control plane |
| Deterministic wait/automation loops | Implemented (`wait-for`, workflows, rules, events) | Superset | Add explicit orchestration contracts for multi-agent mission plans |
| Safety-gated command execution | Implemented with policy+approval+audit | Superset | Tighten policy-to-connector action mapping once connector fabric lands |
| Shared machine API for agents | Implemented (`robot` + `mcp`) | Superset | Stabilize schema versioning and backward-compat contract per endpoint |
| Work claim/coordination primitives | Partially implemented (pane reservations, Agent Mail external) | Gap | Promote work-claim semantics to first-class FrankenTerm mission layer |
| Multi-host swarm federation | Early/feature-gated distributed mode | Gap | Production hardening for reconnect, replay, and failover semantics |
| Connector-triggered orchestration | Not yet native in core runtime | Gap | Introduce connector runtime and event adapter boundaries |
| Mission-level transactionality | Partially present (`tx`/mission commands) | Gap | Unify mission transaction contracts with policy, audit, and rollback evidence |

## Flywheel Connector Fabric Integration Contract

### Connector Fabric as First-Class Subsystem
Connector capabilities should be embedded as a native subsystem, not an afterthought wrapper around external scripts.

### Proposed Planes

| Plane | Responsibility | Existing Assets Reused | New Responsibilities |
|---|---|---|---|
| Data Plane | Collect/normalize pane output, detections, connector events | `runtime.rs`, `ingest.rs`, `events.rs`, `storage.rs` | Connector event adapters with deterministic sequence/idempotency semantics |
| Control Plane | Orchestrate workflows, missions, robot/MCP commands | `workflows/*`, `mcp.rs`, robot CLI, tx/mission surfaces | Connector invocation lifecycle, retries, compensation flows |
| Policy Plane | Decide allow/deny/approval/rate limits | `policy.rs`, `approval.rs`, audit tables | Capability-scoped connector policies and secret-handling guarantees |
| Evidence Plane | Persist decisions, artifacts, and replay traces | `storage.rs`, replay modules, diagnostics | Connector execution provenance + reproducible triage bundles |

### Connector Contract (Minimum)

1. Connector invocation must carry stable `operation_id` and `correlation_id` propagated through policy, audit, and event records.
2. Every connector action must be policy-authorized as an explicit `ActionKind` (or extension) with the same deny/approval semantics as pane mutation.
3. Connector output must be emitted into the event bus with typed event categories and persisted with redaction guarantees.
4. Retry/rollback behavior must be explicit and auditable (no hidden retries with silent state mutation).
5. Failure classes must be normalized (`auth`, `quota`, `network`, `policy`, `validation`, `timeout`, `unknown`) for deterministic automation.

## Boundary Model (Data / Control / Policy)

```text
[pane + connector sources]
    -> ingest + adapters
    -> normalized events + persisted segments
    -> pattern/rule evaluation
    -> workflow/mission planner
    -> policy authorize (allow/deny/require_approval)
    -> execution (pane inject / connector action)
    -> audit + replay artifacts + notifications
```

### Boundary Invariants

- Data plane never performs mutating actions directly.
- Control plane never bypasses policy decisions.
- Policy plane always receives capability context (`pane state`, `reservation`, `actor kind`, future `connector capability`).
- Evidence artifacts are generated for both success and failure paths.

## Parallel Execution Tracks and Gates

| Track | Bead(s) | Purpose | Entry Gate | Exit Gate |
|---|---|---|---|---|
| T1 Capability Census | `ft-3681t.1.1` | NTM command/capability census | Bead claimed + source inventory complete | Complete parity/upgrade/drop matrix with edge cases |
| T2 Connector Census | `ft-3681t.1.2` | Flywheel connector security/runtime inventory | Bead claimed + source inventory complete | Complete connector capability + trust-boundary matrix |
| T3 Architecture Synthesis | `ft-3681t.1.3` | Canonical convergence architecture spec maintenance | T1 + T2 matrices accepted | Canonical spec updated with matrix deltas and approved |
| T4 Program Tracks + Critical Path | `ft-3681t.1.4` | Workstream decomposition for parallel execution | T3 complete | Traceable track graph + unblock plan + measurable gates |
| T5 Verification Pack | `ft-3681t.1.5` | Traceability + acceptance evidence harness | T3 complete | Automated verification checklist + artifact schema + pass criteria |

## Measurable Success Gates (for downstream epics)

1. Capability traceability coverage: 100% of T1/T2 entries mapped to an implementation bead or explicit defer decision.
2. Policy coverage: 100% of mutating actions (pane + connector) have explicit authorization and audit mapping.
3. Determinism coverage: all mission/workflow paths define replay/evidence artifacts and failure taxonomy.
4. Parallelization readiness: each execution track has dependency order, claim rules, and non-overlapping file ownership guidance.
5. No-regression guardrails: every convergence milestone includes baseline comparisons for latency, error rate, and operator recovery time.

## Machine-Checkable Traceability Pack (`ft-3681t.1.5.1`)

This contract now includes a machine-checkable baseline matrix and validation scaffold:

- Matrix artifact: `docs/design/ntm-fcp-traceability-matrix.json`
- Rust validator tests: `crates/frankenterm-core/tests/ntm_fcp_traceability_matrix.rs`
- E2E harness: `tests/e2e/test_ft_3681t_1_5_1_traceability_matrix.sh`

### Validation Commands

Use remote offload for cargo-heavy checks:

```bash
rch exec -- cargo test -p frankenterm-core --test ntm_fcp_traceability_matrix --no-default-features -- --nocapture
tests/e2e/test_ft_3681t_1_5_1_traceability_matrix.sh
```

### Matrix Contract Rules

1. Every entry must have a unique `capability_id`.
2. High/medium gaps must map to at least one `ft-*` bead.
3. All `implementation_anchors` must resolve to existing in-repo paths.
4. `status`/`gap_severity` combinations must be coherent (`gap` cannot have `none`; `implemented` cannot have `medium`/`high`).
5. This matrix is the baseline artifact for future T1/T2 reconciliation and drift checks.

## Immediate Follow-On Actions

1. Merge T1/T2 inventories into a single machine-readable matrix artifact for T3 consumption.
2. Merge validated deltas from this supplemental contract into `docs/ft-3681t-convergence-architecture.md` (canonical `ft-3681t.1.3` artifact) once T1/T2 owners publish their final inventories.
3. Define connector-specific policy extensions (`ActionKind` expansion and capability schema) before `ft-3681t.5.*` implementation starts.
4. Add a deterministic evidence checklist template used by all `ft-3681t.*` child beads.
