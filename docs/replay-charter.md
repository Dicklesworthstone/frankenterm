# Deterministic Swarm Replay — Charter

> Canonical reference contract for the ft-og6q6 program.
> All implementation beads trace back to this document for scope, non-goals,
> and decision principles.

**Bead:** ft-og6q6.1.1
**Status:** Living document
**Parent:** ft-og6q6 (Deterministic Swarm Replay + Counterfactual Simulator)

---

## 1. Problem Statement

Running large AI agent swarms produces complex, multi-pane, multi-session
operational traces. When incidents occur (rate limits, cascading failures,
misdetections, workflow misfires), the debugging process is:

1. **Unreproducible**: The exact interleaving of pane output, pattern
   detections, and workflow decisions cannot be recreated from logs alone.
2. **Manual**: Operators must mentally reconstruct event ordering from
   scattered sources (capture segments, detection events, audit log, workflow
   execution records).
3. **Non-comparative**: There is no way to answer "would the new pattern rules
   have caught this faster?" or "would the updated workflow have recovered
   correctly?" without deploying to production.

The cost compounds: each incident is debugged from scratch, each pattern
update is tested only against synthetic fixtures, and each workflow change is
validated only against unit-level simulations.

---

## 2. Intended Outcomes

### 2.1 Incident Reproducibility

Given a captured operational trace (deltas, events, detections, policy
decisions, workflow actions), the system can replay it offline with
**deterministic event ordering** and produce identical detection/workflow
sequences against the same codebase version.

**Verification gate:** Replay the same trace twice; both runs produce
byte-identical decision sequences.

### 2.2 Regression Artifact Library

Operational traces from resolved incidents become permanent regression
artifacts. Pattern rule changes and workflow modifications are tested against
this library before deployment.

**Verification gate:** `ft replay regression-suite` exits 0 when no decision
diffs exceed tolerance; exits non-zero with diff report on regression.

### 2.3 Counterfactual Comparison

Given a baseline trace and a candidate change (updated patterns, modified
workflows, different policy configuration), the system produces a structured
decision-diff showing where behavior diverges.

**Verification gate:** Decision-diff output identifies each divergence point
with: timestamp, pane, baseline decision, candidate decision, and root cause
(which rule/workflow/policy changed).

### 2.4 Pre-Deploy Safety Gate

CI/CD pipelines can run the regression suite automatically. Candidate builds
must pass the artifact library with acceptable tolerance before merge.

**Verification gate:** GitHub Actions workflow runs replay suite and blocks
merge on regression.

---

## 3. Explicit Non-Goals

These are deliberate exclusions, not future work. If scope creep toward any
of these is observed, stop and revisit this charter.

### 3.1 Universal Terminal Simulator

Replay does NOT reproduce the full terminal emulator state (cursor position,
character attributes, scroll region). It replays **decision-relevant events**:
text deltas, pattern detections, workflow triggers, and policy decisions.

**Why:** Terminal state reconstruction is an unbounded problem (thousands of
escape sequences, terminal-specific behaviors). The value is in decision
replay, not pixel-perfect rendering.

### 3.2 Live Production Replay

Replay is an **offline** capability. It does not support replaying traces
against a live mux server, sending real keystrokes to real panes, or
interacting with real agents.

**Why:** Live replay would require safety interlocks equivalent to the
production policy engine. The complexity/risk ratio is wrong for the
debugging and regression use case.

### 3.3 Distributed Trace Stitching

Replay operates on traces captured by a single `ft watch` instance. It does
NOT stitch together traces from multiple independent watchers observing
different hosts or sessions.

**Why:** Distributed trace correlation requires clock synchronization and
causality tracking that belongs in the distributed mode track (ft-nu4.4.3),
not the replay system.

### 3.4 Performance Benchmarking

Replay preserves event ordering but does NOT preserve timing accuracy at
sub-millisecond granularity. It is unsuitable for performance regression
detection (latency, throughput).

**Why:** Replay speed control (1x, 2x, instant) deliberately distorts
timing. Performance testing belongs in the benchmark suite
(crates/frankenterm-core/benches/).

### 3.5 Data Recovery

Replay cannot reconstruct events that were never captured. If the capture
pipeline had gaps (marked by gap events in the trace), those gaps persist in
replay.

**Why:** Gap markers are an explicit design choice in the ingest pipeline
(ingest.rs). Fabricating missing data would undermine trust in replay
results.

### 3.6 Multi-Version Replay

Replay does NOT support replaying traces captured by version N against the
replay engine of version M (where N != M), unless explicit format migration
is implemented.

**Why:** The event schema, detection rules, and workflow definitions evolve.
Cross-version compatibility requires a versioned trace format with migration
logic — scope for a separate bead if needed.

---

## 4. Scope Boundaries

### 4.1 In Scope

| Surface | What's Captured | What's Replayed |
|---------|----------------|-----------------|
| **Pane output** | Text deltas from ingest pipeline | Fed to pattern engine as if live |
| **Pattern detections** | Rule matches with confidence/severity | Re-evaluated against candidate rules |
| **Workflow decisions** | Step results, wait conditions, actions | Re-executed against candidate workflows |
| **Policy decisions** | Allow/deny/elevate with rationale | Re-evaluated against candidate policy |
| **Event bus** | All event types with timestamps | Replayed in deterministic order |
| **Lifecycle events** | Pane open/close, resize, title change | Included in event stream |

### 4.2 Out of Scope

| Surface | Why Excluded |
|---------|-------------|
| Terminal escape sequences | Non-goal 3.1 |
| Actual agent process state | Not observable by ft |
| Network traffic content | Not captured by ingest pipeline |
| File system changes | Outside ft's observation scope |
| User input (keystrokes) | Captured optionally in WAR format, but NOT replayed against live panes |

### 4.3 Sensitivity and Redaction

Replay operates on **pre-authorized, pre-redacted** query results
(QueryResultEvent from recorder_query.rs). The replay engine never sees
unredacted sensitive data unless the actor has sufficient access tier (A2+).

Traces stored as regression artifacts MUST be redacted to T1 (standard)
sensitivity. Higher-tier data requires explicit retention justification and
audit trail.

---

## 5. Decision Principles

These guide implementation choices when beads face ambiguity.

### 5.1 Determinism Over Fidelity

When a design choice requires trading timing fidelity for deterministic
ordering, choose determinism. Replay results must be reproducible across
machines and runs.

**Example:** Events with identical timestamps are ordered by sequence number,
not by arrival order.

### 5.2 Composable Over Monolithic

Prefer small, composable replay primitives over a single "replay everything"
engine. Each track (capture, replay kernel, counterfactual, decision-diff)
should be independently testable.

**Example:** The counterfactual engine wraps the replay kernel; it does not
fork or duplicate replay logic.

### 5.3 Evidence Over Assertion

Every replay result must include the evidence chain: which events were
replayed, which rules fired, which workflows executed, what policy decisions
were made. "Trust but verify" is not sufficient.

**Example:** Decision-diff output includes the rule definition hash, not just
the rule name.

### 5.4 Fail Loud, Not Silent

If replay encounters an event it cannot process (unknown event kind, schema
mismatch, missing dependency), it MUST halt with a clear error. Silent
skipping creates false confidence.

**Example:** An unknown event_kind in the trace produces a
`ReplayError::UnknownEventKind` with the raw event data.

### 5.5 Minimize Capture Coupling

The replay system should not dictate how data is captured. It consumes
whatever the capture pipeline produces (via QueryResultEvent). Changes to
capture format require explicit migration, not implicit adaptation.

**Example:** If ingest.rs adds a new event type, replay emits an
`UnknownEventKind` error until the replay kernel is updated.

### 5.6 Regression Library is Append-Only

Regression artifacts are never modified or removed without explicit audit
trail. New artifacts are added; old artifacts are retired with rationale.

**Example:** When a pattern rule is intentionally changed, the regression
artifact that tested the old behavior is annotated with "expected divergence"
rather than deleted.

---

## 6. Existing Infrastructure

The following subsystems are already built and should be leveraged:

| Component | Location | Role in Replay |
|-----------|----------|----------------|
| WAR playback engine | `replay.rs` | Low-level frame decode, seek, timing, export |
| Forensic replay session | `recorder_replay.rs` | Event ordering, filtering, speed control |
| Capture pipeline | `ingest.rs`, `events.rs` | Source of replay input data |
| Storage backend | `recorder_storage.rs` | Append-only event persistence |
| Query executor | `recorder_query.rs` | Authorization-aware event retrieval |
| Audit log | `recorder_audit.rs` | Tamper-evident decision recording |
| Pattern engine | `patterns.rs` | Rule matching (baseline and candidate) |
| Workflow engine | `workflows.rs` | Workflow execution (baseline and candidate) |
| Export system | `recorder_export.rs` | JSONL/CSV/transcript output |

### 6.1 What Needs to Be Built

| Capability | Current State | Needed For |
|------------|--------------|-----------|
| **Decision capture** at workflow/policy decision points | Not captured | Counterfactual comparison |
| **Trace format** with versioned schema | WAR format (replay.rs) is recording-focused; no decision metadata | All replay tracks |
| **Counterfactual override engine** | Does not exist | T3 (scenario engine) |
| **Decision-diff scorer** | Does not exist | T4 (intelligence) |
| **Regression artifact storage** | Ad-hoc test fixtures | T6 (CI gates) |
| **CLI/robot/MCP replay interface** | Partial (replay.rs has export) | T5 (product surfaces) |

---

## 7. Track Decomposition

| Track | Name | Depends On | Summary |
|:---:|---|---|---|
| T0 | Program Contract | (this charter) | Charter, equivalence contract, evaluation gates |
| T1 | Capture Data Plane | T0 | Decision capture adapters, trace schema v1 |
| T2 | Replay Execution Kernel | T0, T1 | Deterministic replay with candidate swapping |
| T3 | Counterfactual Engine | T2 | Override injection, scenario composition |
| T4 | Decision-Diff Scoring | T2, T3 | Divergence detection, severity classification |
| T5 | Product Interfaces | T2 | CLI, robot mode, MCP tool surfaces |
| T6 | Validation & CI Gates | T4, T5 | Regression suite, GH Actions, artifact management |

---

## 8. Acceptance Criteria for This Charter (ft-og6q6.1.1)

- [x] Problem statement with concrete failure modes
- [x] Intended outcomes with verification gates
- [x] Explicit non-goals with rationale (6 non-goals)
- [x] Scope boundaries (in-scope / out-of-scope tables)
- [x] Decision principles for implementation guidance (6 principles)
- [x] Existing infrastructure inventory with gap analysis
- [x] Track decomposition with dependency ordering
