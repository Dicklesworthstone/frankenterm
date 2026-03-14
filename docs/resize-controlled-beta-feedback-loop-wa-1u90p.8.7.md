# Controlled Beta Feedback Loop (`ft-1u90p.8.7`)

Date: 2026-03-14
Status: In progress (unblocked; instrumentation + checkpoint artifacts active; e2e validation harness revalidated 2026-03-14 UTC with anomaly-schema guardrails; HOLD unchanged pending real-user cohort data)
Depends on: `ft-1u90p.8.1`, `ft-1u90p.8.2`, `ft-1u90p.8.3`, `ft-1u90p.7.5`, `docs/resize-performance-slos.md`  
Related closure: `ft-1u90p.7.7` (closed 2026-02-22)

## Purpose

Define the controlled beta loop that combines objective telemetry with structured user feedback so rollout promotion decisions are based on both system behavior and user-perceived smoothness.

This document contributes the `ft-1u90p.8.7` deliverables:

1. beta cohort plan and feedback taxonomy
2. telemetry-to-feedback correlation dashboard contract
3. promotion/rollback decision rubric

## Beta Cohort Plan

The controlled beta maps to rollout cohort `C2` from `docs/resize-rollout-plan-wa-1u90p.8.2.md`.

### Cohort Construction

- Exposure target: 10-40% of active sessions (`C2` window).
- Hardware coverage: minimum representation from all tiers in `docs/resize-performance-slos.md` (`low`, `mid`, `high`).
- Workflow coverage: include at least one group for each:
  - editor-heavy (vim/tmux-heavy)
  - long-scrollback monitoring
  - high tab/pane churn
  - mixed font-size/zoom workflows
- Session duration mix:
  - short burst sessions (<30 min)
  - standard work sessions (30 min - 4 h)
  - long-haul sessions (>4 h)

### Run Windows and Sample Sufficiency

- Beta duration: minimum 14 consecutive days after `C2` entry.
- Daily minimum telemetry target:
  - >= 500 resize-class events
  - >= 50 alt-screen transitions
  - >= 30 sessions per hardware tier
- Feedback sufficiency target:
  - >= 40 categorized feedback items total
  - >= 10 items from each workflow group
- Only feedback items with `is_user_feedback = true` and `counts_toward_thresholds = true` count toward those promotion thresholds. Fixture-only or synthetic harness entries may stay in the evidence log for auditability, but they never satisfy beta sample sufficiency on their own.

If sample thresholds are not met, decision status remains `HOLD`.

## Feedback Taxonomy

All beta feedback must be tagged with exactly one primary category and optional secondary tags.

### Primary Categories

| Code | Category | Definition |
|---|---|---|
| `P1` | Perceived hitching | User reports visible lag/stutter during resize or font churn |
| `P2` | Visual artifact | User reports stretching, stale frame, tearing, blank region, or flicker |
| `P3` | Interaction break | User reports input lag, missed keystrokes, cursor jumps, or unsafe behavior |
| `P4` | Alt-screen regression | User reports broken vim/less/htop/tmux behavior during/after resize |
| `P5` | Stability incident | Crash, hang, watchdog emergency mode, or forced recovery |
| `P6` | Positive confirmation | User explicitly reports smooth behavior in a representative workflow |

### Mandatory Metadata per Feedback Item

- `feedback_id` (UUID)
- `reported_at_utc`
- `cohort` (`C2`)
- `hardware_tier` (`low`/`mid`/`high`)
- `workflow_group`
- `category_code` (`P1..P6`)
- `severity` (`critical`/`high`/`medium`/`low`)
- `session_id` (or anonymized equivalent)
- `notes_md` (freeform operator note)
- `is_user_feedback` (`true` only for real cohort feedback)
- `counts_toward_thresholds` (`true` only when that item is eligible to satisfy promotion sample sufficiency)

## Anomaly and Remediation Taxonomy

The beta evidence package must also maintain an explicit anomaly ledger for every open blocker or regression signal that keeps the rollout in `HOLD` or forces `ROLLBACK`.

### Anomaly Categories

| Code | Category | Definition |
|---|---|---|
| `A1` | Sample sufficiency gap | Telemetry/session/feedback volume is below required cohort thresholds |
| `A2` | Feedback ingestion gap | Checkpoint still relies on fixture/synthetic evidence instead of real-user perception data |
| `A3` | Perception vs telemetry mismatch | User-reported smoothness issues diverge from the current telemetry picture |
| `A4` | Regression signal awaiting closure | Repeated `P1`/`P2`/`P4`/`P5` signals remain unresolved |
| `A5` | Evidence integrity gap | Required joins, owners, remediation state, or evidence links are missing/incomplete |

### Mandatory Metadata per Anomaly

- `anomaly_id` (stable slug or UUID)
- `category_code` (`A1..A5`)
- `title`
- `severity` (`critical`/`high`/`medium`/`low`)
- `status` (`open`/`investigating`/`mitigated`/`closed`)
- `blocking_decision` (`GO`/`HOLD`/`ROLLBACK`)
- `triage_owner`
- `remediation_owner`
- `opened_at_utc`
- `last_updated_at_utc`
- `summary`
- `linked_feedback_ids`
- `linked_artifacts`
- `close_loop_status`
- `close_loop_evidence`
- `tracking_issue_ids`

## Telemetry-to-Feedback Correlation Contract

Each feedback item must map to a telemetry window and relevant resize metrics.

### Correlation Keys

- `session_id`
- `pane_id` / `tab_id` where available
- `window_id` where available
- `event_time_bucket_utc` (1-minute bucket)
- `resize_transaction_id` when emitted by harnesses

### Required Telemetry Fields

- SLO lanes from `docs/resize-performance-slos.md`:
  - `m1_p95_ms`, `m1_p99_ms`
  - stage p95 latency: `scheduler_queueing`, `logical_reflow`, `render_prep`, `presentation`
  - artifact counts: critical and minor
  - crash/hang indicators
- Runtime pressure surfaces:
  - `storage_lock_wait_p95_ms`
  - `storage_lock_hold_p95_ms`
  - `cursor_snapshot_bytes_p95`
  - watchdog warnings and degradation tier transitions

### Dashboard Views (minimum)

1. **Cohort Health**
   - resize event volume by day, tier, workflow group
   - M1/M2 percentile trends vs thresholds
2. **Perception vs Telemetry**
   - feedback category counts overlaid with latency/artifact time series
   - top correlated telemetry spikes for `P1..P5`
3. **Regression Lens**
   - incidents with linked artifacts/log bundles
   - unresolved critical/high feedback items by age
4. **Confidence View**
   - positive confirmations (`P6`) vs negative categories
   - decision confidence score for GO/HOLD/ROLLBACK

### Artifact Outputs

- `evidence/wa-1u90p.8.7/beta_feedback_log.jsonl`
- `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv`
- `evidence/wa-1u90p.8.7/cohort_daily_summary.json` including `anomaly_taxonomy[]` with owners, remediation status, and evidence links
- `evidence/wa-1u90p.8.7/decision_checkpoint_<YYYYMMDD>.md` mirroring the current open anomalies and close-loop plan
- `tests/e2e/test_ft_1u90p_8_7.sh`
- `tests/e2e/logs/ft_1u90p_8_7_<RUN_ID>.jsonl`

### Current Checkpoint (2026-03-14)

- Decision: `HOLD`
- Basis:
  - instrumentation path verified using deterministic fixture-only alt-screen conformance run
  - e2e evidence guardrail harness revalidated on 2026-03-14 UTC (baseline/feedback-threshold-contract/anomaly-schema/negative/recovery/anomaly-negative)
  - sample sufficiency thresholds for promotion are not yet met
  - real-user feedback ingestion has not started
- Active anomaly ledger:
  - `sample-sufficiency-gap-2026-03-12` (`A1`, `high`, triage owner `resize-rollout-ops`, remediation owner `beta-program`)
  - `fixture-only-feedback-source-2026-03-12` (`A2`, `medium`, triage owner `telemetry-correlation`, remediation owner `beta-program`)
- Evidence:
  - `e2e-artifacts/2026-02-22T17-53-14Z/scenario_01_alt_screen_conformance/`
  - `evidence/wa-1u90p.8.7/beta_feedback_log.jsonl`
  - `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv`
  - `evidence/wa-1u90p.8.7/cohort_daily_summary.json`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260222.md`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260225.md`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260302.md`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260303.md`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260312.md`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260313.md`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260314.md`
  - `tests/e2e/test_ft_1u90p_8_7.sh`
  - `tests/e2e/logs/ft_1u90p_8_7_20260314_002836.jsonl`

## Decision Rubric (Promotion / Hold / Rollback)

Decisions are evaluated at fixed checkpoints (daily operational, weekly promotion).

### Hard No-Go Triggers (Immediate Rollback)

Any of the following forces `ROLLBACK`:

1. Any invariant breach from `docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md` (`RC-ALTSCREEN-001`, `RC-INTERACTION-001`, `RC-LIFECYCLE-001` included).
2. Any critical artifact incidence > 0 in gate scenarios.
3. Crash/hang incident (`P5`) with confirmed resize/reflow causality.
4. `M1` p99 exceeds target by >20% in two consecutive checkpoint windows.

### Hold Conditions

Set `HOLD` when no hard trigger fired but confidence is insufficient:

- sample sufficiency thresholds not met
- unresolved `high` severity feedback older than 48 hours
- repeated `P1`/`P2` spikes without root-cause closure
- alt-screen conformance (`ft-1u90p.7.7`) not closed

### Promotion Conditions (`GO`)

Promotion is allowed only when all are true:

1. No hard no-go trigger fired during checkpoint window.
2. Sample sufficiency thresholds met.
3. SLO conformance within approved thresholds (M1/M2/M3/M4).
4. No unresolved `critical` or `high` feedback items.
5. At least 7 consecutive days of stable beta metrics before broadening exposure.

## Operating Cadence

- Daily:
  - ingest new feedback and classify with taxonomy
  - refresh telemetry correlation artifacts
  - refresh anomaly owner/remediation state and confirm every open item still has linked evidence
  - publish daily checkpoint status (`GO`/`HOLD`/`ROLLBACK`)
- Weekly:
  - trend review across tiers/workflows
  - promotion decision review with approver sign-off

## Validation Commands

Use existing rollout/observability surfaces for daily and weekly checkpoints:

```bash
# Current rollout and health posture
ft status --health

# Resize/reflow relevant events and search surfaces
ft robot events --limit 200
ft robot search "resize OR reflow OR artifact OR watchdog OR emergency_disable" --limit 200

# Deterministic scenario replay for correlation checkpoints
ft simulate run fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml --json --resize-timeline-json
ft simulate run fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml --json --resize-timeline-json

# Evidence guardrail e2e (baseline + negative + recovery paths)
tests/e2e/test_ft_1u90p_8_7.sh
```

## Exit Criteria for `ft-1u90p.8.7`

1. Controlled beta runs collect statistically useful telemetry and categorized feedback.
2. Promotion decisions cite both objective SLOs and perception signals.
3. Open anomalies have explicit triage owners, remediation owners, and close-loop evidence.
4. Findings are documented and feed final release guidance (`ft-1u90p.8.5`, `ft-1u90p.8.6`).
