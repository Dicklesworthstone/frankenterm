# Deterministic Equivalence Contract and Tolerance Model

> Formal specification of what it means for baseline and replay to produce
> equivalent results, including exact-match requirements, tolerated drift
> domains, and machine-checkable verification procedures.

**Bead:** ft-og6q6.1.2
**Status:** Living document
**Parent:** ft-og6q6.1 (T0 — Program Contract, North Star, and Evaluation Gates)
**Depends on:** ft-og6q6.1.1 (Replay Charter)

---

## 1. Purpose

The replay charter (ft-og6q6.1.1) states that replaying the same trace twice
must produce "byte-identical decision sequences." This document defines
precisely which fields constitute the "decision sequence," which fields are
compared exactly, which tolerate drift, and which are excluded from
comparison entirely.

Without this contract, two independent implementations of the replay kernel
would have no objective way to determine whether they agree.

---

## 2. Terminology

| Term | Definition |
|------|-----------|
| **Baseline run** | The original captured trace, including all events and decisions as they occurred in production. |
| **Replay run** | A subsequent execution of the replay kernel over the same captured trace. |
| **Candidate run** | A replay run using modified rules, workflows, or policies (counterfactual). |
| **Decision sequence** | The ordered list of decision-bearing events extracted from a run. |
| **Equivalence class** | A group of events that share the same `(pane_id, stream_kind, sequence)` tuple. |
| **Merge key** | The 5-field `RecorderMergeKey` that defines total event ordering. |
| **Drift domain** | A field or field group where controlled variation between runs is expected and tolerated. |

---

## 3. Equivalence Levels

Three levels of equivalence are defined, from strictest to most permissive.
Each higher level includes all requirements of the levels below it.

### 3.1 Level 0 — Structural Equivalence (required for all replay)

Two runs are **structurally equivalent** if and only if:

1. **Same event count**: Both runs produce the same number of events.
2. **Same event types**: For each position `i` in merge-key order, both runs
   have the same `RecorderEventPayload` variant (IngressText, EgressOutput,
   ControlMarker, LifecycleMarker).
3. **Same pane topology**: The set of `pane_id` values is identical.
4. **Same sequence domains**: For each `(pane_id, stream_kind)` pair, the
   maximum sequence number is identical.
5. **Same causality graph structure**: For each event, `parent_event_id`,
   `trigger_event_id`, and `root_event_id` either both reference valid
   events or both are `null`.

Structural equivalence is the minimum bar. A replay that fails structural
equivalence has a bug in the replay kernel, not a difference in rules.

### 3.2 Level 1 — Decision Equivalence (required for regression pass)

Two runs are **decision-equivalent** if they are structurally equivalent AND:

1. **Identical decision payloads**: For each decision-bearing event (see
   Section 4), the decision-relevant fields are byte-identical.
2. **Identical merge ordering**: Events sorted by `RecorderMergeKey` appear
   in the same order. Specifically, for events at positions `i` and `j`
   where `i < j`, `merge_key(event_i) < merge_key(event_j)` in both runs.
3. **Identical event IDs**: For each position in merge order,
   `event_id` values are identical (since event IDs are deterministic
   SHA-256 hashes of content).

Decision equivalence is the regression gate. If a code change causes
decision non-equivalence on any regression artifact, that change is a
regression.

### 3.3 Level 2 — Full Equivalence (gold standard)

Two runs are **fully equivalent** if they are decision-equivalent AND:

1. **All non-excluded fields match** (see Section 5 for the exclusion list).
2. **Timestamp drift within tolerance** (see Section 6).
3. **Payload details JSON structurally equal** (key order ignored, float
   precision within tolerance).

Full equivalence is the ideal. It is not always achievable (e.g., replay
speed control deliberately distorts timing), but the gap between decision
equivalence and full equivalence must be explainable.

---

## 4. Decision-Bearing Events

Not every event carries a decision. The equivalence contract distinguishes
**decision-bearing events** (whose outputs matter for correctness) from
**observational events** (which provide context but do not affect outcomes).

### 4.1 Decision-Bearing Event Types

| Source | Event/Field | Decision Content | Exact Match Required |
|--------|------------|-----------------|---------------------|
| **Pattern engine** | `Detection` | `rule_id`, `agent_type`, `event_type`, `severity`, `confidence`, `matched_text`, `extracted` | Yes, except `confidence` (tolerance: 1e-9) |
| **Workflow engine** | `StepResult` | Variant (Continue/Done/Retry/Abort), `result` value, `reason`, `delay_ms` | Yes |
| **Policy engine** | `PolicyDecision` | Variant (Allow/Deny/Elevate), `rule_id`, `context` | Yes |
| **Control markers** | `ControlMarker` with `PolicyDecision` type | `details.decision`, `details.rule_id` | Yes |
| **Lifecycle markers** | `LifecycleMarker` | `lifecycle_phase`, `reason` | Yes |

### 4.2 Observational (Non-Decision) Event Types

| Source | Event/Field | Role | Comparison |
|--------|------------|------|-----------|
| **Egress output** | `EgressOutput` text deltas | Input to pattern engine | Content-compared at Level 2 only |
| **Ingress text** | `IngressText` | Record of injected input | Content-compared at Level 2 only |
| **Gap markers** | `EgressOutput` with `is_gap=true` | Missing data indicator | Presence-compared (gap at same sequence = pass) |

---

## 5. Field Classification

Every field of `RecorderEvent` is classified into one of four comparison
categories.

### 5.1 Exact Match (E)

These fields must be byte-identical between baseline and replay for the
same event position in merge order.

| Field | Rationale |
|-------|----------|
| `schema_version` | Must always be `"ft.recorder.event.v1"` |
| `event_id` | Deterministic SHA-256; any difference indicates content/ordering change |
| `pane_id` | Pane identity is structural |
| `sequence` | Monotonic counter per `(pane_id, stream_kind)` domain |
| `source` | Event source (Capture, Replay, Synthetic) |
| `payload` variant tag | IngressText, EgressOutput, ControlMarker, LifecycleMarker |
| `causality.parent_event_id` | Structural graph integrity |
| `causality.trigger_event_id` | Cross-stream causality |
| `causality.root_event_id` | Chain root identity |

### 5.2 Tolerance Match (T)

These fields are compared with a configurable tolerance window.

| Field | Default Tolerance | Rationale |
|-------|------------------|----------|
| `occurred_at_ms` | (excluded — see 5.4) | Wall-clock source time; non-deterministic |
| `recorded_at_ms` | (excluded — see 5.4) | Wall-clock record time; non-deterministic |
| `confidence` (in Detection) | Absolute delta < 1e-9 | Float precision across platforms |
| `details` JSON floats | Absolute delta < 1e-6 | JSON serde float precision loss |

### 5.3 Structural Match (S)

These fields are compared for structural equality (same keys, same types)
but not byte-identical values.

| Field | Comparison Method |
|-------|------------------|
| `details` (ControlMarker) | JSON deep-equal with float tolerance, key-order-independent |
| `details` (LifecycleMarker) | JSON deep-equal with float tolerance, key-order-independent |
| `extracted` (Detection) | JSON deep-equal with float tolerance |

### 5.4 Excluded from Comparison (X)

These fields are explicitly excluded from equivalence checking.

| Field | Rationale |
|-------|----------|
| `occurred_at_ms` | Wall-clock time is inherently non-deterministic; replay speed control distorts it; ordering is determined by merge key, not timestamp |
| `recorded_at_ms` | Same as above |
| `session_id` | May differ between capture and replay sessions |
| `correlation_id` | Depends on external request context |
| `workflow_id` | May be regenerated during replay |

**Important:** Excluding timestamps from comparison does NOT mean timestamps
are irrelevant. The `RecorderMergeKey` uses `recorded_at_ms` as its
primary sort key. But the ordering itself (verified via merge key comparison)
is what matters, not the absolute timestamp values.

---

## 6. Tolerance Model

### 6.1 Configuration

```
[replay.equivalence]
# Equivalence level for regression gates
level = "decision"  # "structural" | "decision" | "full"

# Float comparison tolerance
float_tolerance = 1e-9

# JSON float comparison tolerance (wider due to serde precision)
json_float_tolerance = 1e-6

# Maximum allowed timestamp drift before warning (ms)
# Only applies at Level 2 (full equivalence)
timestamp_drift_warn_ms = 100

# Maximum allowed timestamp drift before failure (ms)
timestamp_drift_fail_ms = 5000
```

### 6.2 Tolerance Application Rules

1. **Exact fields never tolerate drift.** If `event_id` differs, the events
   are non-equivalent regardless of tolerance settings.

2. **Float tolerance is absolute, not relative.** Two values `a` and `b`
   match if `|a - b| < tolerance`. This avoids division-by-zero issues
   and is appropriate for the value ranges in this system (confidence
   scores are 0.0–1.0; durations are millisecond-scale).

3. **JSON structural comparison ignores key ordering.** Objects
   `{"a":1,"b":2}` and `{"b":2,"a":1}` are structurally equal.

4. **Tolerance is symmetric.** If baseline tolerates replay, then replay
   tolerates baseline.

5. **Tolerance stacks.** When comparing `Detection.confidence` inside a
   `details` JSON blob, the tighter tolerance applies (1e-9, not 1e-6).

---

## 7. Ordering Contract

### 7.1 RecorderMergeKey Total Order

The canonical ordering for all replay comparison uses `RecorderMergeKey`:

```
1. recorded_at_ms  ascending  (primary)
2. pane_id         ascending  (secondary)
3. stream_kind     rank asc   (tertiary: Lifecycle=0 < Control=1 < Ingress=2 < Egress=3)
4. sequence        ascending  (quaternary)
5. event_id        lexicographic ascending  (quinary: deterministic tiebreaker)
```

This is a **total order** — no two distinct events share the same merge key
(guaranteed by the event_id tiebreaker). Event ordering is therefore
deterministic regardless of input permutation.

### 7.2 Ordering Classes

Events fall into ordering classes based on how strictly their position
is constrained:

| Class | Constraint | Example |
|-------|-----------|---------|
| **Totally ordered** | Position is fixed by merge key; any reordering is a violation | All events from a single `(pane_id, stream_kind)` domain |
| **Concurrently ordered** | Position is fixed by merge key but the "correct" order between panes is a function of capture timing | Events from different panes with the same `recorded_at_ms` |
| **Causally ordered** | Position must respect `trigger_event_id` → happens-before relationships | Cross-stream triggered events |

For **concurrently ordered** events (same `recorded_at_ms`, different
panes), the merge key imposes a deterministic order (by `pane_id` then
`stream_kind` then `sequence`), but this order may differ from the "true"
causal order in the original system. This is acceptable: the charter
(ft-og6q6.1.1, Principle 5.1) explicitly chooses determinism over fidelity.

### 7.3 Merge Order Verification

Given two event sequences `A` and `B`, merge order equivalence holds if
and only if:

```
for all i in 0..len(A):
    merge_key(A[i]) == merge_key(B[i])
```

If `merge_key(A[i]) != merge_key(B[i])` for any `i`, the sequences have
diverged at position `i`. The divergence report must include:

- Position `i`
- Both merge keys
- Both event payloads
- The last matching position `i-1` (for context)

---

## 8. Comparison Procedure

### 8.1 Inputs

- **Baseline**: An ordered sequence of `RecorderEvent` values, sorted by
  `RecorderMergeKey`.
- **Replay**: An ordered sequence of `RecorderEvent` values from replay,
  sorted by `RecorderMergeKey`.
- **Config**: Equivalence level and tolerance parameters.

### 8.2 Algorithm

```
function compare(baseline, replay, config) -> EquivalenceReport:
    // Phase 1: Structural check
    if len(baseline) != len(replay):
        return FAIL(CountMismatch)

    divergences = []

    for i in 0..len(baseline):
        b = baseline[i]
        r = replay[i]

        // Phase 2: Merge key check
        if merge_key(b) != merge_key(r):
            divergences.push(MergeOrderDivergence(i, b, r))
            continue  // remaining comparisons meaningless for this pair

        // Phase 3: Exact field check
        for field in EXACT_FIELDS:
            if b.field != r.field:
                divergences.push(ExactFieldMismatch(i, field, b.field, r.field))

        // Phase 4: Decision payload check (if config.level >= Decision)
        if config.level >= Decision:
            if is_decision_bearing(b):
                diff = compare_decision(b, r, config)
                if diff:
                    divergences.push(DecisionDivergence(i, diff))

        // Phase 5: Full field check (if config.level >= Full)
        if config.level >= Full:
            diff = compare_all_fields(b, r, config)
            if diff:
                divergences.push(FullFieldDivergence(i, diff))

    return EquivalenceReport(divergences)
```

### 8.3 Output: EquivalenceReport

```
EquivalenceReport {
    passed: bool,
    level_checked: EquivalenceLevel,
    events_compared: usize,
    divergences: Vec<Divergence>,
    summary: DivergenceSummary,
}

Divergence {
    position: usize,
    kind: DivergenceKind,  // MergeOrder | ExactField | Decision | FullField
    baseline_event_id: String,
    replay_event_id: String,
    baseline_merge_key: RecorderMergeKey,
    replay_merge_key: RecorderMergeKey,
    field: Option<String>,
    baseline_value: Option<String>,
    replay_value: Option<String>,
    root_cause: Option<RootCause>,
}

RootCause {
    changed_rule_id: Option<String>,
    changed_workflow_id: Option<String>,
    changed_policy_id: Option<String>,
    rule_definition_hash: Option<String>,  // Charter principle 5.3: evidence over assertion
}
```

---

## 9. Counterfactual Comparison Extension

When comparing a **baseline** against a **candidate** (modified rules),
decision equivalence is NOT expected. Instead, the comparison produces a
**decision-diff** that identifies each divergence with its root cause.

### 9.1 Expected vs Unexpected Divergences

| Category | Definition | Action |
|----------|-----------|--------|
| **Expected** | Divergence traceable to an intentionally changed rule/workflow/policy | Include in diff report with root cause |
| **Unexpected** | Divergence NOT traceable to any intentional change | Flag as potential regression |
| **Cascading** | Divergence caused by a prior divergence (e.g., workflow step changed, so downstream steps differ) | Include with cascade chain |

### 9.2 Root Cause Attribution

For each divergence, the diff engine must identify which change caused it:

1. **Rule change**: Compare rule definition hashes between baseline and
   candidate. If a Detection diverges and its `rule_id` maps to a changed
   rule, attribute to that rule.
2. **Workflow change**: Compare workflow definition hashes. If a StepResult
   diverges and the workflow version differs, attribute to the workflow.
3. **Policy change**: Compare policy configuration hashes. If a
   PolicyDecision diverges, attribute to policy.
4. **Cascade**: If no direct cause is found but a preceding event in the
   same causal chain diverged, attribute as cascade from that event.
5. **Unknown**: If none of the above apply, flag as unexpected.

---

## 10. Gap Handling

The charter (ft-og6q6.1.1, Non-Goal 3.5) states that gaps in capture
persist in replay. The equivalence contract handles gaps as follows:

1. **Gap markers are positional.** A gap event at sequence `N` in the
   baseline must appear at sequence `N` in the replay.
2. **Gap content is not compared.** The `details.gap_reason` may differ
   (e.g., "capture timeout" vs "replay gap injection").
3. **Gap boundaries are exact.** The events immediately before and after
   a gap must match exactly (by event_id).
4. **Missing gaps are violations.** If the baseline has a gap at sequence
   `N` but the replay does not (or vice versa), this is a structural
   equivalence failure.

---

## 11. Clock Anomaly Handling

Clock anomalies (`details.clock_anomaly = true`) are expected in both
baseline and replay. The equivalence contract:

1. **Preserves anomaly markers.** If the baseline marks an event as a
   clock anomaly, the replay must also mark it (structural equivalence).
2. **Does not compare anomaly timestamps.** The `occurred_at_ms` values
   for anomalous events are excluded from even Level 2 drift checking.
3. **Preserves ordering despite anomalies.** The merge key ordering is
   authoritative even when timestamps are anomalous. If the merge key
   ordering is preserved, the replay passes regardless of clock state.

---

## 12. Invariant Summary

These invariants must hold for any compliant replay implementation:

| ID | Invariant | Level | Verification |
|----|----------|-------|-------------|
| EQ-01 | Same event count | L0 | `len(baseline) == len(replay)` |
| EQ-02 | Same payload variants at each position | L0 | Variant tag comparison |
| EQ-03 | Same pane_id set | L0 | Set equality |
| EQ-04 | Same sequence domain maximums | L0 | Per-domain max comparison |
| EQ-05 | Consistent causality graph structure | L0 | Null/non-null agreement |
| EQ-06 | Identical event_id at each position | L1 | String equality |
| EQ-07 | Identical merge key ordering | L1 | Pairwise merge key comparison |
| EQ-08 | Identical decision payloads | L1 | Field-by-field exact match |
| EQ-09 | Detection confidence within tolerance | L1 | `|b - r| < 1e-9` |
| EQ-10 | All non-excluded fields match | L2 | Full field comparison |
| EQ-11 | Timestamp drift within tolerance | L2 | `|b.ts - r.ts| < threshold` |
| EQ-12 | JSON details structurally equal | L2 | Deep-equal with tolerance |
| EQ-13 | Gap markers at same positions | L0 | Sequence position match |
| EQ-14 | Clock anomaly markers preserved | L0 | Boolean field match |
| EQ-15 | Per-domain sequence monotonicity | L0 | `seq[i] < seq[i+1]` within domain |

---

## 13. Acceptance Criteria for This Document (ft-og6q6.1.2)

- [x] Three equivalence levels defined (structural, decision, full)
- [x] Decision-bearing events enumerated with exact fields
- [x] Every RecorderEvent field classified (E/T/S/X)
- [x] Tolerance model with configurable parameters
- [x] Ordering contract referencing RecorderMergeKey
- [x] Ordering classes defined (total, concurrent, causal)
- [x] Comparison algorithm specified with pseudocode
- [x] EquivalenceReport output format defined
- [x] Counterfactual extension with root cause attribution
- [x] Gap and clock anomaly handling specified
- [x] Invariant table with IDs and verification methods
