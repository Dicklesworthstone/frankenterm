# RFC: R0-R4 Rollout Gate Pattern

## Status
Proposed

## Problem
Deploying a storage backend migration requires staged validation gates to catch regressions before they reach all users. Ad-hoc rollout processes miss edge cases and lack audit trails.

## Solution
A five-stage rollout gate system where each stage has machine-evaluable criteria, evidence artifacts, and soak requirements:

### Stages
| Stage | Name | Entry Criteria | Gate Criteria |
|-------|------|---------------|---------------|
| R0 | Baseline | E1-E4 complete | T1-T6 green, rollback drill validated |
| R1 | Shadow | R0 + migration tooling | M0-M4 reproducible, no invariant violations |
| R2 | Canary | R1 + rollback runbook | Full M0-M5 in single workspace, soak passes |
| R3 | Progressive | R2 + no high-severity incidents | SLO adherence across multiple workspaces |
| R4 | Promotion | R3 + soak window complete | Final go/no-go approval with evidence package |

### Key Properties
- **Serializable**: Gate criteria, evidence, and decisions are JSON-serializable for audit.
- **Monotonic**: Stages must be completed in order; no skipping from R0 to R3.
- **Evidence-based**: Each gate requires specific artifacts (test results, soak metrics, approval records).
- **Human-in-the-loop**: R4 requires explicit human approval via GoNoGoReview.

### GateCriterion
```rust
struct GateCriterion {
    name: String,
    description: String,
    met: bool,
    evidence_artifact: Option<String>,
}
```

### SoakMetrics
```rust
struct SoakMetrics {
    duration_hours: f64,
    health_checks_passed: u64,
    health_checks_total: u64,
    p99_lag_ms: f64,
    error_rate: f64,
}
```

## Testing Guidance
- Verify stage ordering: predecessor chain R4→R3→R2→R1→R0→None
- Test gate evaluation: all criteria met → pass, any unmet → fail
- Property test: serde roundtrip preserves all fields
- Test soak metrics thresholds: health pass rate, lag budget, error rate
