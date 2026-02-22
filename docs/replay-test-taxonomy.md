# Replay Test Taxonomy and Structured Logging Contract

> Normative testing and observability contract for the deterministic swarm
> replay system. Defines required test coverage classes, assertion patterns,
> structured logging fields, and minimum evidence for gate decisions.

**Bead:** ft-og6q6.1.4
**Status:** Living document
**Parent:** ft-og6q6.1 (T0 — Program Contract, North Star, and Evaluation Gates)
**Depends on:** ft-og6q6.1.2 (Equivalence Contract)
**Extends:** [test-logging-contract.md](test-logging-contract.md) (general test logging)

---

## 1. Test Coverage Classes

The replay system requires six distinct test coverage classes. Each track
(T1 through T6) must satisfy the coverage requirements for its scope.

### 1.1 Class U — Unit Tests

**Scope:** Single function or struct, no I/O, no async.

**Location:** Inline `#[cfg(test)] mod tests` within each source file.

**Requirements:**
- Minimum 30 tests per module (consistent with project-wide standard).
- Each public function has at least one positive and one negative test.
- Edge cases: empty input, single element, maximum capacity, zero/negative
  values where applicable.

**Naming:** `test_<function>_<scenario>` (e.g., `test_merge_key_cmp_same_timestamp`).

**Replay-specific unit tests must cover:**

| Component | Required Assertions |
|-----------|-------------------|
| `RecorderMergeKey::cmp` | Total order properties: reflexivity, antisymmetry, transitivity |
| `generate_event_id_v1` | Determinism (same input = same output), uniqueness (different input = different output) |
| `StreamKind::rank` | Ordering: Lifecycle < Control < Ingress < Egress |
| `ClockAnomalyTracker` | Regression detection, future skew detection, threshold configurability |
| Equivalence comparator | Level 0/1/2 classification, tolerance application, field exclusion |
| Decision extractor | Correct identification of decision-bearing vs observational events |
| Gap marker handling | Gap preservation, boundary event matching |
| Divergence reporter | Correct position, field, value reporting for each divergence kind |

### 1.2 Class P — Property-Based Tests (Proptest)

**Scope:** Algebraic invariants that must hold for all valid inputs.

**Location:** `crates/frankenterm-core/tests/proptest_<module>.rs`

**Requirements:**
- Minimum 20 properties per proptest file.
- Default case count: 100 (via `ProptestConfig::with_cases(100)`).
- Regression file maintained: `proptest_<module>.proptest-regressions`.

**Naming:** `prop_<invariant_name>` (e.g., `prop_merge_key_total_order`).

**Required replay properties:**

| ID | Property | Strategy |
|----|---------|----------|
| P-01 | Merge key defines total order | `arb_merge_key_pair()` |
| P-02 | Event ID is deterministic | `arb_recorder_event()` → generate twice, compare |
| P-03 | Event ID is collision-resistant | `arb_distinct_event_pair()` → IDs differ |
| P-04 | Structural equivalence is reflexive | `arb_event_sequence()` → compare to self |
| P-05 | Decision equivalence implies structural | `arb_equivalent_pair()` → L1 ⊃ L0 |
| P-06 | Full equivalence implies decision | `arb_equivalent_pair()` → L2 ⊃ L1 |
| P-07 | Equivalence is symmetric | `arb_event_sequence_pair()` → `eq(a,b) == eq(b,a)` |
| P-08 | Tolerance is monotonic | Wider tolerance never rejects what narrower accepts |
| P-09 | Gap markers preserve sequence count | Insert gaps, verify total event count unchanged |
| P-10 | Clock anomaly does not affect merge order | Permute timestamps, verify merge key order stable |
| P-11 | Causality graph is acyclic | `arb_causal_chain()` → no cycles in parent/trigger/root refs |
| P-12 | Divergence position is minimal | First divergence reported is at earliest differing position |
| P-13 | Counterfactual diff attributes root cause | Changed rule → divergence attributed to that rule |
| P-14 | Excluded fields do not affect L1 equivalence | Vary excluded fields, L1 still passes |
| P-15 | Float tolerance is symmetric | `|a-b| < tol ⟺ |b-a| < tol` |
| P-16 | JSON structural equality ignores key order | Permute keys, structural match holds |
| P-17 | Sequence monotonicity within domain | `arb_domain_events()` → `seq[i] < seq[i+1]` |
| P-18 | Per-domain sequence independence | Events in different domains have independent counters |
| P-19 | Regression artifact immutability | Serialize, deserialize, re-compare → same result |
| P-20 | Evidence chain completeness | Every divergence has non-empty evidence |

**Proptest assertion rules (project-wide):**
- NO `{var}` implicit format captures in `prop_assert!` — use `"msg {}", var`.
- NO `prop_assert_eq!` on `f64` — use `(a - b).abs() < tolerance`.
- Deduplicate strategy outputs via `BTreeMap`/`HashSet` before uniqueness assertions.
- Do not use `gen` as a variable name (reserved in Rust 2024).

### 1.3 Class I — Integration Tests

**Scope:** Cross-module interaction, async, may use mock I/O.

**Location:** `crates/frankenterm-core/tests/<feature>_integration.rs`

**Requirements:**
- Tests use `#[tokio::test]` for async.
- Isolated temp directories via `temp_db()` or `TempDir`.
- Mock objects for external dependencies (MockWezterm, MockSegmentStore).

**Naming:** `test_<modules>_<interaction>` (e.g., `test_capture_replay_roundtrip`).

**Required replay integration tests:**

| ID | Test | Modules Under Test |
|----|------|-------------------|
| I-01 | Capture → storage → replay produces same events | ingest, recorder_storage, recorder_replay |
| I-02 | Replay → pattern engine produces same detections | recorder_replay, patterns |
| I-03 | Replay → workflow engine produces same step results | recorder_replay, workflows |
| I-04 | Replay → policy engine produces same decisions | recorder_replay, policy |
| I-05 | Invariant checker passes on replayed trace | recorder_replay, recorder_invariants |
| I-06 | Equivalence comparator agrees on identical replays | replay_kernel, equivalence |
| I-07 | Counterfactual with changed rule produces expected diff | replay_kernel, counterfactual, patterns |
| I-08 | Regression artifact loads, replays, and passes | regression_store, replay_kernel |
| I-09 | Export → reimport preserves decision equivalence | recorder_export, replay_kernel |
| I-10 | Audit trail captures all replay decisions | recorder_audit, replay_kernel |

### 1.4 Class E — End-to-End Tests

**Scope:** Full system, real processes, shell script harness.

**Location:** `tests/e2e/test_replay_<scenario>.sh`

**Requirements:**
- JSON-structured logging via `log_json()` helper.
- Isolated workspace in `/tmp/ft-e2e-replay-*`.
- Artifact manifest at `<workspace>/manifest.json`.
- Exit code 0 on pass, non-zero on fail.
- All artifacts redacted per [test-logging-contract.md](test-logging-contract.md).

**Naming:** `test_replay_<scenario>.sh`

**Required replay E2E scenarios:**

| ID | Scenario | Acceptance Criterion |
|----|---------|---------------------|
| E-01 | Capture-replay roundtrip | `ft replay` on captured trace exits 0, equivalence report shows L1 pass |
| E-02 | Regression suite pass | `ft replay regression-suite` exits 0 on clean codebase |
| E-03 | Regression suite detect | `ft replay regression-suite` exits non-zero after intentional pattern change |
| E-04 | Counterfactual diff | `ft replay --candidate <modified_rules>` produces decision-diff with expected divergences |
| E-05 | Speed control | Replay at 1x, 2x, instant all produce L1-equivalent results |
| E-06 | Pane filter | Replay with `--pane-filter` produces subset-equivalent results |

### 1.5 Class R — Regression Artifact Tests

**Scope:** Fixed traces from resolved incidents, replayed against current code.

**Location:** `tests/regression/replay/` (trace files) + `tests/regression/replay/expected/` (golden outputs)

**Requirements:**
- Each artifact has a metadata header (incident ID, date, codebase version, redaction level).
- Artifacts are append-only (charter principle 5.6).
- Expected outputs are decision sequences, not full event streams.
- Tolerance overrides per-artifact allowed via `<artifact>.tolerance.toml`.

**Artifact format:**
```
tests/regression/replay/
├── INCIDENT-001/
│   ├── metadata.toml          # incident_id, captured_at, ft_version, redaction_tier
│   ├── trace.jsonl            # captured event stream
│   ├── expected_decisions.jsonl  # golden decision sequence
│   ├── tolerance.toml         # per-artifact tolerance overrides (optional)
│   └── README.md              # incident description, expected behavior
├── INCIDENT-002/
│   └── ...
└── manifest.toml              # index of all artifacts with checksums
```

**Metadata schema:**
```toml
[metadata]
incident_id = "INCIDENT-001"
captured_at = "2026-02-15T10:30:00Z"
ft_version = "0.3.2"
ft_commit = "abc123def"
redaction_tier = "T1"
pane_count = 4
event_count = 1523
description = "Rate limit cascade during 8-agent swarm"

[expected]
decision_count = 47
detection_count = 12
workflow_step_count = 31
policy_decision_count = 4

[annotations]
# If a rule was intentionally changed, mark expected divergences here
# Format: rule_id = "expected" | "regression"
# "core.codex:usage_reached" = "expected"  # Rule v2 changed threshold
```

### 1.6 Class S — Smoke Tests (CI Fast Path)

**Scope:** Minimal replay sanity checks that run in < 10 seconds.

**Location:** `crates/frankenterm-core/tests/replay_smoke.rs`

**Requirements:**
- No external dependencies, no disk I/O beyond temp files.
- Synthetic trace (5-10 events) constructed in-memory.
- Verifies: replay produces events, merge ordering is correct, event IDs are deterministic.

**Required smoke tests:**

| ID | Test | Budget |
|----|------|--------|
| S-01 | Empty trace replays to empty output | < 1ms |
| S-02 | Single-event trace roundtrips | < 1ms |
| S-03 | Multi-pane trace preserves ordering | < 5ms |
| S-04 | Clock anomaly does not crash replay | < 5ms |
| S-05 | Gap marker passes through replay | < 1ms |

---

## 2. Structured Logging Fields for Replay

In addition to the fields defined in [test-logging-contract.md](test-logging-contract.md),
replay tests emit the following domain-specific fields.

### 2.1 Replay Execution Fields

| Field | Type | When Emitted | Description |
|-------|------|-------------|-------------|
| `replay.trace_id` | string | All replay operations | Identifier for the trace being replayed |
| `replay.run_id` | string | All replay operations | Unique ID for this replay execution |
| `replay.speed` | string | Replay start | "1x", "2x", "instant" |
| `replay.event_index` | u64 | Per-event processing | Position in merge-key order |
| `replay.event_count` | u64 | Replay start/end | Total events in trace |
| `replay.pane_count` | u64 | Replay start | Number of distinct panes |
| `replay.elapsed_ms` | f64 | Replay end | Wall-clock replay duration |

### 2.2 Equivalence Check Fields

| Field | Type | When Emitted | Description |
|-------|------|-------------|-------------|
| `equiv.level` | string | Comparison start | "structural", "decision", "full" |
| `equiv.passed` | bool | Comparison end | Overall result |
| `equiv.events_compared` | u64 | Comparison end | Number of events compared |
| `equiv.divergence_count` | u64 | Comparison end | Total divergences found |
| `equiv.first_divergence_pos` | u64 | On first divergence | Position of earliest divergence |
| `equiv.divergence_kind` | string | Per divergence | "merge_order", "exact_field", "decision", "full_field" |
| `equiv.field` | string | Per field divergence | Field name that diverged |
| `equiv.baseline_value` | string | Per divergence | Baseline value (truncated to 200 chars) |
| `equiv.replay_value` | string | Per divergence | Replay value (truncated to 200 chars) |

### 2.3 Decision Tracking Fields

| Field | Type | When Emitted | Description |
|-------|------|-------------|-------------|
| `decision.type` | string | Per decision event | "detection", "workflow_step", "policy" |
| `decision.rule_id` | string | Detections and policy | Rule that produced the decision |
| `decision.rule_hash` | string | Detections and policy | SHA-256 of rule definition (evidence chain) |
| `decision.pane_id` | u64 | All decisions | Pane context |
| `decision.sequence` | u64 | All decisions | Event sequence number |
| `decision.outcome` | string | All decisions | "match"/"no_match", "continue"/"done"/"retry"/"abort", "allow"/"deny"/"elevate" |

### 2.4 Counterfactual Fields

| Field | Type | When Emitted | Description |
|-------|------|-------------|-------------|
| `cf.candidate_id` | string | Counterfactual runs | Identifier for candidate configuration |
| `cf.changed_rules` | u64 | Counterfactual start | Number of rules that differ from baseline |
| `cf.changed_workflows` | u64 | Counterfactual start | Number of workflows that differ |
| `cf.expected_divergences` | u64 | Diff end | Divergences attributable to changes |
| `cf.unexpected_divergences` | u64 | Diff end | Divergences NOT attributable to changes |
| `cf.cascade_depth` | u64 | Per cascade | Depth of divergence cascade chain |

### 2.5 Regression Suite Fields

| Field | Type | When Emitted | Description |
|-------|------|-------------|-------------|
| `regression.artifact_id` | string | Per artifact | Incident/artifact identifier |
| `regression.artifact_version` | string | Per artifact | ft version that captured the trace |
| `regression.result` | string | Per artifact | "pass", "fail", "skip", "expected_divergence" |
| `regression.tolerance_overrides` | bool | Per artifact | Whether custom tolerances were applied |
| `regression.suite_total` | u64 | Suite end | Total artifacts in suite |
| `regression.suite_passed` | u64 | Suite end | Artifacts that passed |
| `regression.suite_failed` | u64 | Suite end | Artifacts that failed |

---

## 3. Gate Evidence Requirements

Each track (T1-T6) must produce evidence artifacts sufficient for gate
decisions. This section defines the minimum evidence bundle.

### 3.1 Evidence Bundle Structure

```
evidence/
├── gate-report.json           # Machine-readable gate result
├── unit-summary.json          # Unit test counts and results
├── proptest-summary.json      # Property test counts, case counts, regressions
├── integration-summary.json   # Integration test results
├── e2e-summary.json           # E2E scenario results with artifacts
├── regression-summary.json    # Regression suite results
├── smoke-summary.json         # Smoke test results with timings
├── coverage.json              # Line/branch coverage for replay modules
└── logs/                      # Structured logs from all test runs
    ├── unit.jsonl
    ├── proptest.jsonl
    ├── integration.jsonl
    └── e2e.jsonl
```

### 3.2 Gate Report Schema

```json
{
  "version": "1",
  "format": "ft-replay-gate-report",
  "generated_at": "2026-02-22T12:00:00Z",
  "track": "T2",
  "commit": "abc123",
  "gate_result": "pass",
  "checks": [
    {
      "class": "unit",
      "total": 45,
      "passed": 45,
      "failed": 0,
      "skipped": 0,
      "required": true,
      "result": "pass"
    },
    {
      "class": "proptest",
      "total": 20,
      "cases_per_property": 100,
      "regressions_replayed": 3,
      "passed": 20,
      "failed": 0,
      "required": true,
      "result": "pass"
    },
    {
      "class": "integration",
      "total": 10,
      "passed": 10,
      "failed": 0,
      "required": true,
      "result": "pass"
    },
    {
      "class": "e2e",
      "total": 6,
      "passed": 6,
      "failed": 0,
      "required": true,
      "result": "pass"
    },
    {
      "class": "regression",
      "total": 12,
      "passed": 11,
      "expected_divergence": 1,
      "failed": 0,
      "required": true,
      "result": "pass"
    },
    {
      "class": "smoke",
      "total": 5,
      "passed": 5,
      "max_duration_ms": 4.2,
      "budget_ms": 10000,
      "required": true,
      "result": "pass"
    }
  ],
  "coverage": {
    "line_percent": 87.3,
    "branch_percent": 72.1,
    "uncovered_functions": ["experimental_feature_x"]
  }
}
```

### 3.3 Minimum Thresholds for Gate Pass

| Check | Threshold | Enforcement |
|-------|----------|-------------|
| Unit tests | 100% pass rate | Hard gate (any failure blocks) |
| Property tests | 100% pass rate, >= 20 properties | Hard gate |
| Integration tests | 100% pass rate | Hard gate |
| E2E scenarios | 100% pass rate | Hard gate |
| Regression suite | 0 unexpected failures | Hard gate (expected divergences allowed) |
| Smoke tests | 100% pass, all within time budget | Hard gate |
| Line coverage | >= 80% for replay modules | Soft gate (warning, not blocking) |
| Branch coverage | >= 60% for replay modules | Soft gate |

### 3.4 Gate Decision Matrix

| All Hard Gates Pass | Soft Gates Pass | Decision |
|:---:|:---:|---|
| Yes | Yes | **PASS** — merge allowed |
| Yes | No | **PASS WITH WARNING** — merge allowed, coverage improvement needed |
| No | Any | **FAIL** — merge blocked, fix required |

---

## 4. Per-Track Test Requirements

Each track has specific test requirements in addition to the general
requirements above.

### T1 — Capture Data Plane

| Class | Required Tests | Key Assertions |
|-------|---------------|----------------|
| U | Event serialization roundtrip, merge key construction | Serde identity, key ordering |
| P | Schema stability, field completeness | No fields lost in serialize/deserialize |
| I | Capture → storage → query pipeline | Events retrievable after storage |
| E | `ft watch` captures events from live panes | Event stream non-empty, well-formed |

### T2 — Replay Execution Kernel

| Class | Required Tests | Key Assertions |
|-------|---------------|----------------|
| U | Replay iterator, speed control, pane filtering | Correct event emission order |
| P | Determinism (same input = same output), merge ordering | EQ-06, EQ-07 invariants |
| I | Replay → detection → workflow pipeline | Same decisions as baseline |
| E | Full replay roundtrip with comparison | L1 equivalence passes |
| R | At least 1 regression artifact | Replay matches golden output |
| S | All 5 smoke tests | Under time budget |

### T3 — Counterfactual Engine

| Class | Required Tests | Key Assertions |
|-------|---------------|----------------|
| U | Override injection, scenario composition | Overrides apply correctly |
| P | Override does not affect non-targeted events | Non-targeted events unchanged |
| I | Changed rule → correct divergence attribution | Root cause matches changed rule |
| E | CLI counterfactual produces diff report | Report contains expected divergences |

### T4 — Decision-Diff Scoring

| Class | Required Tests | Key Assertions |
|-------|---------------|----------------|
| U | Divergence classification, severity scoring | Correct severity levels |
| P | Scoring is deterministic, monotonic in severity | Same input = same score |
| I | Diff scorer + evidence chain completeness | Every divergence has evidence |
| E | Diff report is human-readable and machine-parseable | Both JSON and text output valid |

### T5 — Product Interfaces

| Class | Required Tests | Key Assertions |
|-------|---------------|----------------|
| U | CLI argument parsing, output formatting | Correct flag handling |
| I | CLI → kernel → report pipeline | End-to-end data flow |
| E | `ft replay` CLI produces expected output | Exit codes, output format |

### T6 — Validation & CI Gates

| Class | Required Tests | Key Assertions |
|-------|---------------|----------------|
| U | Gate report generation, threshold checking | Correct pass/fail classification |
| I | Full evidence bundle generation | All required files present |
| E | CI workflow runs suite and reports correctly | GitHub Actions integration |
| R | Full regression suite | All artifacts pass or have annotated expected divergences |

---

## 5. Test Data Strategies

### 5.1 Synthetic Trace Generation

For unit and property tests, traces are generated programmatically:

```rust
fn synthetic_trace(pane_count: usize, events_per_pane: usize) -> Vec<RecorderEvent> {
    // Deterministic: same inputs always produce same trace
    // Uses fixed timestamps, sequential pane_ids, monotonic sequences
}

fn synthetic_detection(rule_id: &str, confidence: f64) -> Detection {
    // Minimal Detection with specified fields
}

fn synthetic_step_result(variant: &str) -> StepResult {
    // StepResult::Continue, Done, Retry, or Abort
}
```

### 5.2 Fixture Traces

For integration tests, pre-built fixture traces in `tests/fixtures/replay/`:

```
tests/fixtures/replay/
├── minimal.jsonl          # 3 events, 1 pane
├── multi_pane.jsonl       # 20 events, 3 panes
├── with_gaps.jsonl        # 15 events including 2 gap markers
├── clock_anomaly.jsonl    # 10 events with clock regression
├── with_detections.jsonl  # 30 events including pattern matches
└── with_workflows.jsonl   # 25 events including workflow steps
```

### 5.3 Production-Derived Traces

For regression tests, captured from resolved incidents. Must be:
- Redacted to T1 sensitivity tier.
- Annotated with incident metadata.
- Checksummed in `manifest.toml`.
- Never modified after initial capture (append-only principle).

---

## 6. Assertion Patterns for Replay

### 6.1 Equivalence Assertions

```rust
// Level 0: structural
assert_structural_equivalent(&baseline, &replay);

// Level 1: decision
assert_decision_equivalent(&baseline, &replay);

// Level 2: full (with tolerance)
let config = EquivalenceConfig {
    float_tolerance: 1e-9,
    json_float_tolerance: 1e-6,
    ..Default::default()
};
assert_full_equivalent(&baseline, &replay, &config);
```

### 6.2 Divergence Assertions

```rust
// Assert specific divergence at position
let report = compare(&baseline, &candidate, &config);
assert_eq!(report.divergences.len(), 1);
assert_eq!(report.divergences[0].position, 7);
assert_eq!(report.divergences[0].kind, DivergenceKind::Decision);

// Assert root cause attribution
let cause = report.divergences[0].root_cause.as_ref().unwrap();
assert_eq!(cause.changed_rule_id.as_deref(), Some("core.codex:usage_reached"));
```

### 6.3 Ordering Assertions

```rust
// Assert merge key total order
for window in events.windows(2) {
    assert!(
        merge_key(&window[0]) < merge_key(&window[1]),
        "merge order violated at seq {} -> {}",
        window[0].sequence, window[1].sequence
    );
}
```

### 6.4 Evidence Chain Assertions

```rust
// Every divergence must have evidence
for div in &report.divergences {
    assert!(
        div.root_cause.is_some(),
        "divergence at position {} has no root cause",
        div.position
    );
    let cause = div.root_cause.as_ref().unwrap();
    // At least one attribution must be present
    let has_attribution = cause.changed_rule_id.is_some()
        || cause.changed_workflow_id.is_some()
        || cause.changed_policy_id.is_some();
    assert!(has_attribution, "divergence at {} has empty attribution", div.position);
}
```

---

## 7. Acceptance Criteria for This Document (ft-og6q6.1.4)

- [x] Six test coverage classes defined (U, P, I, E, R, S)
- [x] Per-class requirements: location, naming, minimum counts, assertion patterns
- [x] 20 required proptest properties enumerated (P-01 through P-20)
- [x] 10 required integration tests enumerated (I-01 through I-10)
- [x] 6 required E2E scenarios enumerated (E-01 through E-06)
- [x] 5 required smoke tests enumerated (S-01 through S-05)
- [x] Regression artifact format with metadata schema
- [x] Replay-specific structured logging fields (5 categories, 25+ fields)
- [x] Gate evidence bundle structure and report schema
- [x] Minimum thresholds for gate pass (hard/soft gates)
- [x] Per-track (T1-T6) test requirements matrix
- [x] Test data strategies (synthetic, fixture, production-derived)
- [x] Assertion patterns for equivalence, divergence, ordering, and evidence
