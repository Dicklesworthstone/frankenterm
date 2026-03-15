# Decision Checkpoint — 2026-03-14 (`ft-1u90p.8.7`)

## Result
- Decision: `HOLD`
- Confidence: low-to-moderate (guardrails and evidence integrity remain healthy; real-user cohort remains insufficient)

## Inputs Reviewed
- Existing fixture-derived telemetry checkpoint:
  - `e2e-artifacts/2026-02-22T17-53-14Z/scenario_01_alt_screen_conformance/`
- Evidence artifacts:
  - `evidence/wa-1u90p.8.7/beta_feedback_log.jsonl`
  - `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv`
  - `evidence/wa-1u90p.8.7/cohort_daily_summary.json`
- Decision guardrail automation:
  - `tests/e2e/test_ft_1u90p_8_7.sh`
  - `tests/e2e/logs/ft_1u90p_8_7_20260314_204826.jsonl`

## Gate Evaluation
- Hard no-go triggers: not observed in the current fixture-only checkpoint.
- Hold conditions:
  - sample sufficiency thresholds are still not met
  - real-user C2 cohort feedback coverage remains below minimum
- Guardrail validation run:
  - executed at `2026-03-15T00:48:26Z` (`2026-03-14T20:48:26-04:00` local)
  - baseline case correctly remains `HOLD`
  - correlation CSV preserves threshold-eligibility metadata end-to-end
  - correlation CSV threshold mismatches are rejected
  - fixture-only feedback is excluded from threshold counts and summary totals must match countable real-user feedback
  - synthetic feedback miscounting is rejected
  - summary-only threshold inflation without countable real-user feedback is rejected
  - anomaly owner/remediation schema and checkpoint mirroring remain valid
  - negative case (`GO` with unmet thresholds) is rejected
  - recovery case (`GO` with met thresholds) is accepted
  - anomaly-negative case (missing owner/evidence) is rejected

## Decision Rationale
The evidence pipeline, correlation CSV threshold contract, real-user threshold contract, anomaly ledger schema, and promotion guardrails remain healthy and reproducible, but promotion is still blocked on real-user C2 sample sufficiency. `HOLD` remains the correct operational decision.

## Active Anomaly / Remediation Tracking

1. `sample-sufficiency-gap-2026-03-12`
   - Category: `A1`
   - Severity: `high`
   - Status: `open`
   - Triage owner: `resize-rollout-ops`
   - Remediation owner: `beta-program`
   - Close-loop status: `awaiting_real_user_cohort_ingest`
   - Evidence: `evidence/wa-1u90p.8.7/cohort_daily_summary.json`, `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv`, `tests/e2e/logs/ft_1u90p_8_7_20260314_204826.jsonl`

2. `fixture-only-feedback-source-2026-03-12`
   - Category: `A2`
   - Severity: `medium`
   - Status: `open`
   - Triage owner: `telemetry-correlation`
   - Remediation owner: `beta-program`
   - Close-loop status: `waiting_for_c2_feedback_collection`
   - Evidence: `evidence/wa-1u90p.8.7/beta_feedback_log.jsonl`, `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv`, `evidence/wa-1u90p.8.7/decision_checkpoint_20260314.md`

## Required Follow-up
1. Capture real-user C2 telemetry and categorized feedback across low/mid/high tiers.
2. Reach documented sample sufficiency thresholds (`resize_events`, `alt_screen_transitions`, tier/session counts, real-user countable feedback volume).
3. Re-run `tests/e2e/test_ft_1u90p_8_7.sh` after new cohort ingestion and append the next dated decision checkpoint.
4. Update the anomaly ledger entries above until both have close-loop evidence and can move to `closed`.
