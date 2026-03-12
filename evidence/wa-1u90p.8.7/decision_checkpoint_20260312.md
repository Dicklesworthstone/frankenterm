# Decision Checkpoint — 2026-03-12 (`ft-1u90p.8.7`)

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
  - `tests/e2e/logs/ft_1u90p_8_7_20260312_013012.jsonl`

## Gate Evaluation
- Hard no-go triggers: not observed in the current fixture-only checkpoint.
- Hold conditions:
  - sample sufficiency thresholds are still not met
  - real-user C2 cohort feedback coverage remains below minimum
- Guardrail validation run:
  - executed at `2026-03-12T05:30:12Z` (`2026-03-12T01:30:12-04:00` local)
  - baseline case correctly remains `HOLD`
  - negative case (`GO` with unmet thresholds) is rejected
  - recovery case (`GO` with met thresholds) is accepted

## Decision Rationale
The evidence pipeline and promotion guardrails remain healthy and reproducible, but promotion is still blocked on real-user C2 sample sufficiency. `HOLD` remains the correct operational decision.

## Required Follow-up
1. Capture real-user C2 telemetry and categorized feedback across low/mid/high tiers.
2. Reach documented sample sufficiency thresholds (`resize_events`, `alt_screen_transitions`, tier/session counts, feedback volume).
3. Re-run `tests/e2e/test_ft_1u90p_8_7.sh` after new cohort ingestion and append the next dated decision checkpoint.
