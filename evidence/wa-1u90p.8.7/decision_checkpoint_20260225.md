# Decision Checkpoint — 2026-02-25 (`ft-1u90p.8.7`)

## Result
- Decision: `HOLD`
- Confidence: low-to-moderate (instrumentation + guardrails validated; real-user cohort still insufficient)

## Inputs Reviewed
- Existing fixture-derived telemetry checkpoint:
  - `e2e-artifacts/2026-02-22T17-53-14Z/scenario_01_alt_screen_conformance/`
- Evidence artifacts:
  - `evidence/wa-1u90p.8.7/beta_feedback_log.jsonl`
  - `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv`
  - `evidence/wa-1u90p.8.7/cohort_daily_summary.json`
- Decision guardrail automation:
  - `tests/e2e/test_ft_1u90p_8_7.sh`
  - `tests/e2e/logs/ft_1u90p_8_7_<RUN_ID>.jsonl`

## Gate Evaluation
- Hard no-go triggers: not observed in the current fixture-only checkpoint.
- Hold conditions:
  - sample sufficiency thresholds are still not met
  - real-user C2 cohort feedback coverage remains below minimum
- Guardrail validation:
  - baseline case correctly remains `HOLD`
  - negative case (`GO` with unmet thresholds) is rejected
  - recovery case (`GO` with met thresholds) is accepted

## Decision Rationale
The telemetry + feedback artifact pipeline is now validation-backed with explicit e2e decision guardrails, but rollout promotion remains blocked by insufficient real-user cohort evidence. `HOLD` remains the only defensible decision.

## Required Follow-up
1. Capture real-user C2 telemetry + categorized feedback across low/mid/high tiers.
2. Reach documented sample sufficiency thresholds (`resize_events`, `alt_screen_transitions`, tier/session counts, feedback volume).
3. Re-run `tests/e2e/test_ft_1u90p_8_7.sh` and append new checkpoint evidence for the latest cohort window.
