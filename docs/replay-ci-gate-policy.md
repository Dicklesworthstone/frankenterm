# Replay CI Gate Policy, Exception Process, and Rollout Rubric

> Operational policy that converts replay equivalence checks and test
> results into deployment decisions. Defines pass/fail thresholds,
> waiver rules, emergency override workflow, and evidence requirements
> for deployment approval.

**Bead:** ft-og6q6.1.3
**Status:** Living document
**Parent:** ft-og6q6.1 (T0 — Program Contract, North Star, and Evaluation Gates)
**Depends on:** ft-og6q6.1.2 (Equivalence Contract), ft-og6q6.1.4 (Test Taxonomy)

---

## 1. Gate Architecture

The replay CI pipeline has three sequential gates. A pull request must
pass all three to merge.

```
PR opened
  │
  ▼
┌─────────────┐     ┌─────────────┐     ┌──────────────┐
│  Gate 1:     │────▶│  Gate 2:     │────▶│  Gate 3:      │──── Merge
│  Smoke       │     │  Test Suite  │     │  Regression   │     Allowed
│  (< 30s)     │     │  (< 10 min)  │     │  Suite        │
└─────────────┘     └─────────────┘     │  (< 30 min)   │
       │                   │             └──────────────┘
       ▼                   ▼                    │
    FAIL-FAST          FAIL-FAST               ▼
    (block merge)      (block merge)        FAIL or PASS
```

### 1.1 Gate 1 — Smoke Gate

**Purpose:** Fast rejection of obviously broken changes.

**Runs:** On every push to a PR branch.

**Time budget:** < 30 seconds total.

**Contents:**
- Smoke tests S-01 through S-05 (from test taxonomy).
- Compile check of replay modules.
- Schema version check (event schema v1).

**Pass criteria:** All smoke tests pass within time budget.

**Failure action:** PR is blocked. No further gates run.

### 1.2 Gate 2 — Test Suite Gate

**Purpose:** Verify correctness of replay implementation.

**Runs:** On every push to a PR branch, after Gate 1 passes.

**Time budget:** < 10 minutes total.

**Contents:**
- All unit tests (Class U) for replay modules.
- All property tests (Class P) with 100 cases per property.
- All integration tests (Class I).

**Pass criteria:** See Section 2 thresholds.

**Failure action:** PR is blocked. Gate 3 does not run.

### 1.3 Gate 3 — Regression Gate

**Purpose:** Verify that no previously-correct behavior has regressed.

**Runs:** After Gate 2 passes. Also runs nightly on `main`.

**Time budget:** < 30 minutes total.

**Contents:**
- Full regression artifact suite (Class R).
- E2E scenarios (Class E).
- Evidence bundle generation.

**Pass criteria:** See Section 2 thresholds.

**Failure action:** PR is blocked. Evidence bundle is uploaded as CI artifact.

---

## 2. Pass/Fail Thresholds

### 2.1 Hard Gates (Blocking)

Any failure in a hard gate blocks the merge. No exceptions without a
formal waiver (Section 4).

| Check | Threshold | Gate |
|-------|----------|------|
| Smoke tests | 100% pass, all within time budget | G1 |
| Unit tests | 100% pass | G2 |
| Property tests | 100% pass, >= 20 properties | G2 |
| Integration tests | 100% pass | G2 |
| E2E scenarios | 100% pass | G3 |
| Regression suite — unexpected failures | 0 | G3 |
| Evidence bundle completeness | All required files present | G3 |
| Schema version match | Event schema matches expected version | G1 |

### 2.2 Soft Gates (Warning)

Soft gate failures produce warnings but do not block merge.

| Check | Threshold | Warning Message |
|-------|----------|----------------|
| Line coverage (replay modules) | < 80% | "Replay module coverage below target" |
| Branch coverage (replay modules) | < 60% | "Replay branch coverage below target" |
| Regression suite duration | > 20 minutes | "Regression suite approaching time budget" |
| Property test regressions replayed | < seed count | "Not all proptest regressions were replayed" |

### 2.3 Informational Metrics

These are tracked but have no pass/fail threshold.

| Metric | Purpose |
|--------|---------|
| Total event count across regression artifacts | Track growth of artifact library |
| Average divergence count per counterfactual run | Track baseline drift |
| Gate 3 wall-clock duration trend | Detect suite bloat |
| Flaky test rate (failures that pass on retry) | Track test reliability |

---

## 3. Regression Suite Outcomes

Each regression artifact produces one of four outcomes.

### 3.1 PASS

Replay produces L1 (decision) equivalence with the golden decision
sequence. No action required.

### 3.2 EXPECTED DIVERGENCE

Replay produces divergences, but all divergences are annotated as
"expected" in the artifact's `tolerance.toml` or `annotations` section.
This occurs when a rule or workflow was intentionally changed.

**Requirements for expected divergence:**
1. Every divergence position is listed in the artifact's annotations.
2. Each annotation references the rule/workflow change that caused it.
3. The annotation was added in the same PR that changed the rule/workflow.

**Action:** PASS. The gate treats this as a pass.

### 3.3 UNEXPECTED FAILURE

Replay produces divergences that are NOT annotated as expected.

**Action:** FAIL. The PR is blocked. The author must either:
1. Fix the regression, or
2. Obtain a waiver (Section 4), or
3. Add expected-divergence annotations with justification.

### 3.4 SKIP

An artifact is skipped when its metadata indicates incompatibility
(e.g., schema version mismatch with no migration path).

**Requirements for skip:**
- The skip reason must be logged.
- Skipped artifacts count against a skip budget (max 10% of suite).
- If > 10% of artifacts are skipped, the gate fails.

---

## 4. Waiver Process

A waiver allows a PR to merge despite a hard gate failure. Waivers
are exceptional and tracked.

### 4.1 Waiver Eligibility

Waivers are available for:
- **Flaky test failures** where the test passes on re-run and the flake
  is documented in a tracking issue.
- **Infrastructure failures** (CI runner issues, network timeouts) that
  are not caused by the PR.
- **Intentional behavior changes** that require regression artifact
  updates but the updates cannot be completed in the same PR cycle.

Waivers are NOT available for:
- New regressions without a remediation plan.
- Skipping tests to meet a deadline.
- Ignoring unexpected divergences without investigation.

### 4.2 Waiver Authority

| Waiver Scope | Authority Required |
|-------------|-------------------|
| Single flaky test (with re-run evidence) | PR author + 1 reviewer |
| Infrastructure failure (with CI logs) | PR author + 1 reviewer |
| Intentional behavior change (with plan) | PR author + project owner |
| Multiple gate failures | Project owner only |

### 4.3 Waiver Record

Every waiver must be recorded in the PR description with:

```markdown
## Waiver

- **Gate:** [G1/G2/G3]
- **Check:** [specific check that failed]
- **Reason:** [why the failure is not a real regression]
- **Evidence:** [link to re-run, CI logs, or remediation plan]
- **Approved by:** [approver name/handle]
- **Remediation:** [issue/bead ID for follow-up fix, if applicable]
- **Expiry:** [date by which remediation must be complete]
```

### 4.4 Waiver Expiry

Waivers are time-limited:
- Flaky test waivers expire after 7 days (fix the flake).
- Behavior change waivers expire after 14 days (update artifacts).
- Infrastructure waivers expire after 1 day (re-run after infra fix).

Expired waivers re-block the PR (or any subsequent PR that triggers
the same failure).

---

## 5. Emergency Override

In a production incident where a fix must ship immediately, the normal
gate process may be too slow. The emergency override allows bypassing
Gate 3 (regression suite) under strict conditions.

### 5.1 Override Conditions (ALL must be true)

1. There is an active production incident (severity P0 or P1).
2. The PR contains ONLY the incident fix (no unrelated changes).
3. Gate 1 (smoke) and Gate 2 (test suite) pass.
4. The override is approved by the project owner.
5. A follow-up bead is created to run the full regression suite
   within 24 hours.

### 5.2 Override Procedure

1. PR author adds label `emergency-override` to the PR.
2. PR author posts a comment with:
   ```
   EMERGENCY OVERRIDE REQUEST
   Incident: [incident ID/link]
   Severity: [P0/P1]
   Fix scope: [brief description]
   Gate 1: PASS
   Gate 2: PASS
   Gate 3: SKIPPED (emergency)
   Follow-up bead: [bead ID for regression verification]
   ```
3. Project owner approves with a comment: `EMERGENCY OVERRIDE APPROVED`.
4. PR is merged with Gate 3 skipped.
5. Within 24 hours, the follow-up bead must verify that the full
   regression suite passes on `main` with the fix included.

### 5.3 Override Audit

All emergency overrides are tracked in an append-only log:

```
docs/replay-override-log.md
```

Each entry includes: date, PR link, incident ID, approver, follow-up
bead ID, and whether the follow-up regression suite passed.

---

## 6. Rollout Go/No-Go Rubric

When a new replay track (T1-T6) is ready for integration into the CI
pipeline, the following rubric determines readiness.

### 6.1 Track Readiness Criteria

| Criterion | Required | Evidence |
|-----------|---------|---------|
| All hard gates pass on current `main` | Yes | CI run link |
| Gate evidence bundle is complete | Yes | Bundle artifact link |
| Regression artifacts exist (>= 3 for T2+) | Yes | Artifact manifest |
| No open P0/P1 bugs in the track | Yes | Bead query showing 0 results |
| Documentation updated (charter, contracts) | Yes | Doc links in PR |
| Performance impact measured | Yes | Benchmark comparison |
| Rollback procedure documented | Yes | Rollback section in PR |

### 6.2 Go/No-Go Decision

| All criteria met | Decision | Action |
|:---:|---|---|
| Yes | **GO** | Enable gate in CI, announce to team |
| Criteria met except performance | **CONDITIONAL GO** | Enable with monitoring, set performance improvement bead |
| Missing regression artifacts | **NO-GO** | Create artifacts first |
| Open P0/P1 bugs | **NO-GO** | Fix bugs first |

### 6.3 Rollback Procedure

If a newly enabled gate causes excessive false positives or CI slowdown:

1. **Observe:** Track flaky rate and gate duration for 48 hours after enabling.
2. **Threshold:** If flaky rate > 5% or gate duration > 2x budget, trigger rollback.
3. **Rollback:** Disable the gate via CI config (not by deleting tests).
4. **Investigate:** Create a bead to diagnose and fix the issue.
5. **Re-enable:** After fix, follow the go/no-go rubric again.

---

## 7. Evidence Requirements for Deployment Approval

### 7.1 Standard Deployment

For a standard merge to `main`, the PR must have:

1. Gate report (`gate-report.json`) showing all hard gates pass.
2. No active waivers with expired remediation deadlines.
3. At least one reviewer approval.

### 7.2 Release Deployment

For a tagged release, additional evidence is required:

| Evidence | Format | Location |
|---------|--------|----------|
| Full regression suite report | `gate-report.json` | CI artifacts |
| Regression artifact library checksums | `manifest.toml` | `tests/regression/replay/` |
| All waiver remediations complete | Bead status check | beads query |
| No emergency overrides pending follow-up | Override log check | `docs/replay-override-log.md` |
| Counterfactual diff for any changed rules | Decision-diff report | CI artifacts |

### 7.3 Evidence Retention

| Evidence Type | Retention Period | Storage |
|--------------|-----------------|---------|
| Gate reports | 90 days | CI artifact storage |
| Regression suite logs | 90 days | CI artifact storage |
| Waiver records | Permanent | PR description (GitHub) |
| Emergency override log | Permanent | `docs/replay-override-log.md` (git) |
| Regression artifacts | Permanent (append-only) | `tests/regression/replay/` (git) |

---

## 8. CI Configuration Reference

### 8.1 GitHub Actions Workflow Structure

```yaml
name: Replay Gates
on:
  pull_request:
    paths:
      - 'crates/frankenterm-core/src/replay**'
      - 'crates/frankenterm-core/src/recorder**'
      - 'crates/frankenterm-core/src/event_id**'
      - 'crates/frankenterm-core/src/patterns**'
      - 'crates/frankenterm-core/src/workflows**'
      - 'crates/frankenterm-core/src/policy**'
      - 'tests/regression/replay/**'
      - 'docs/replay-**'

jobs:
  gate-1-smoke:
    runs-on: ubuntu-latest
    timeout-minutes: 2
    steps:
      - uses: actions/checkout@v4
      - name: Smoke tests
        run: cargo test --lib replay_smoke -- --test-threads=1
      - name: Schema check
        run: cargo test --lib event_id::tests::test_schema_version

  gate-2-test-suite:
    needs: gate-1-smoke
    runs-on: ubuntu-latest
    timeout-minutes: 15
    steps:
      - uses: actions/checkout@v4
      - name: Unit tests
        run: cargo test --lib -- replay kernel equivalence
      - name: Property tests
        run: cargo test --test proptest_replay --test proptest_equivalence
      - name: Integration tests
        run: cargo test --test replay_integration

  gate-3-regression:
    needs: gate-2-test-suite
    runs-on: ubuntu-latest
    timeout-minutes: 45
    steps:
      - uses: actions/checkout@v4
      - name: E2E scenarios
        run: bash tests/e2e/test_replay_roundtrip.sh
      - name: Regression suite
        run: cargo test --test replay_regression_suite
      - name: Generate evidence bundle
        run: cargo run -- replay generate-evidence
      - name: Upload evidence
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: replay-evidence
          path: evidence/
          retention-days: 90
```

### 8.2 Path Triggers

The replay gates only trigger on changes to replay-related code. This
avoids unnecessary CI time on unrelated changes.

**Trigger paths:**
- `crates/frankenterm-core/src/replay*`
- `crates/frankenterm-core/src/recorder*`
- `crates/frankenterm-core/src/event_id*`
- `crates/frankenterm-core/src/patterns*`
- `crates/frankenterm-core/src/workflows*`
- `crates/frankenterm-core/src/policy*`
- `tests/regression/replay/**`
- `docs/replay-*`

### 8.3 Nightly Regression Run

Gate 3 also runs nightly on `main` to catch regressions from
merged changes that didn't trigger the path filter.

```yaml
on:
  schedule:
    - cron: '0 4 * * *'  # 4 AM UTC daily
```

---

## 9. Acceptance Criteria for This Document (ft-og6q6.1.3)

- [x] Three-gate architecture defined with time budgets
- [x] Hard and soft gate thresholds specified
- [x] Four regression suite outcome types (pass, expected divergence, unexpected failure, skip)
- [x] Waiver process with eligibility, authority, record format, and expiry
- [x] Emergency override with conditions, procedure, and audit trail
- [x] Rollout go/no-go rubric with criteria and rollback procedure
- [x] Evidence requirements for standard and release deployments
- [x] Evidence retention policy
- [x] CI configuration reference with path triggers and nightly schedule
