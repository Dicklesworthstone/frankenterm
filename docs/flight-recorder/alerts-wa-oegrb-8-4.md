# Recorder Alert Strategy (`wa-oegrb.8.4`)

Date: 2026-02-14  
Status: Draft alert matrix for rollout-track operations  
Source of truth for phase controls: `docs/flight-recorder/rollout-plan-wa-oegrb-8-1.md`

## Purpose

Define explicit alert conditions, severities, and response actions for recorder/index/search reliability during staged rollout.

This document is designed to be directly consumed by:
- on-call operators
- runbook users (`ops-runbook-wa-oegrb-8-4.md`)
- incident-response templates (`wa-oegrb.8.5`)

## Signals and Sources

Primary command surface:

```bash
ft status --health | jq
ft triage --severity warning
ft events --event-type gap --unhandled
ft robot events --event-type gap --limit 50
```

Primary health fields:
- `health.db_writable`
- `health.ingest_lag_avg_ms`
- `health.ingest_lag_max_ms`
- `health.capture_queue_depth`
- `health.write_queue_depth`
- `health.backpressure_tier`
- `health.scheduler.total_rate_limited`
- `health.scheduler.total_byte_budget_exceeded`
- `health.scheduler.total_throttle_events`
- `health.in_crash_loop`
- `health.consecutive_crashes`

Event signal:
- `event_type=gap` (including overflow-related discontinuities)

## Severity Levels

| Severity | Meaning | Expected response |
|---|---|---|
| `info` | Expected/brief transient behavior | Monitor only |
| `warning` | Degradation trend that may impact reliability | Respond within 30 min |
| `critical` | Active reliability or safety risk | Immediate mitigation + rollback decision |

## Alert Catalog

## A1. Recorder DB Unwritable

- Severity: `critical`
- Trigger:
  - `health.db_writable == false`
- Verify:
```bash
ft status --health | jq '.health.db_writable'
```
- Required action:
  1. Freeze rollout progression.
  2. Move to safe posture per rollout plan if user impact exists.
  3. Capture incident/diag bundles and escalate.

## A2. Ingest Lag Sustained

- Severity: `warning` then `critical`
- Trigger:
  - `warning`: `health.ingest_lag_max_ms > 5000` for 3 consecutive checks
  - `critical`: `health.ingest_lag_max_ms > 15000` for 3 consecutive checks
- Verify:
```bash
ft status --health | jq '.health.ingest_lag_max_ms'
```
- Required action:
  1. Check queue depths and scheduler throttling.
  2. If critical persists, reduce rollout exposure and run recovery validation.

## A3. Queue Backlog Growth

- Severity: `warning`
- Trigger:
  - `capture_queue_depth` or `write_queue_depth` increases across 3 checks without recovery to baseline
- Verify:
```bash
ft status --health | jq '{capture:.health.capture_queue_depth, write:.health.write_queue_depth}'
```
- Required action:
  1. Correlate with load window.
  2. If not expected load, treat as degradation and escalate to capture/storage owners.

## A4. Scheduler Throttling Surge

- Severity: `warning` then `critical`
- Trigger:
  - sustained growth in:
    - `health.scheduler.total_rate_limited`
    - `health.scheduler.total_byte_budget_exceeded`
    - `health.scheduler.total_throttle_events`
  - plus recurring `gap` events indicates likely overload incident (`critical`)
- Verify:
```bash
ft status --health | jq '.health.scheduler'
ft events --event-type gap --unhandled
```
- Required action:
  1. Confirm whether surge is planned.
  2. If unplanned, hold rollout and execute overflow/recovery checks.

## A5. Gap Burst (Unknown Cause)

- Severity: `warning` then `critical`
- Trigger:
  - multiple unhandled `gap` events in short window
  - no planned restart/recovery operation logged
- Verify:
```bash
ft events --event-type gap --unhandled
ft robot events --event-type gap --limit 50
```
- Required action:
  1. Validate continuity and expected maintenance context.
  2. If unknown, escalate and capture forensic artifacts.

## A6. Crash Loop

- Severity: `critical`
- Trigger:
  - `health.in_crash_loop == true` OR `health.consecutive_crashes >= 3`
- Verify:
```bash
ft status --health | jq '{in_crash_loop:.health.in_crash_loop, consecutive:.health.consecutive_crashes}'
```
- Required action:
  1. Immediate rollback to safe rollout posture.
  2. Export crash and diagnostic bundles.
  3. Open critical incident with artifacts.

## A7. Privacy/Audit Integrity Regression

- Severity: `critical`
- Trigger:
  - security/privacy suite failure
  - redaction leak indication
  - audit integrity/tamper-evidence failure
- Verify:
```bash
scripts/check_recorder_validation_gates.sh
cargo test -p frankenterm-core --test recorder_security_privacy_validation -- --nocapture
```
- Required action:
  1. Stop progression and enforce rollback.
  2. Route to security + incident commander immediately.

## Routing Matrix

| Alert | Primary owner | Secondary owner | Escalation |
|---|---|---|---|
| A1 | Recorder on-call | Storage owner | Incident commander |
| A2 | Recorder on-call | Search/index owner | Incident commander (critical) |
| A3 | Recorder on-call | Capture owner | Incident commander if sustained |
| A4 | Recorder on-call | Capture + storage owners | Incident commander if gaps surge |
| A5 | Recorder on-call | Capture owner | Incident commander |
| A6 | Recorder on-call | Platform owner | Incident commander |
| A7 | Security owner | Recorder on-call | Incident commander + approver |

## Alert Hygiene Rules

1. Do not auto-resolve critical alerts without explicit operator verification.
2. Every critical alert must produce an evidence packet (health snapshots + event evidence + bundle paths).
3. Alert thresholds are phase-aware:
- rollout progression is blocked on unresolved warning trends
- any critical alert triggers immediate go/no-go reevaluation.

## Exit Criteria for `wa-oegrb.8.4` Alerting

1. Alert conditions map to concrete, observable fields/events.
2. Every alert has deterministic verification + first actions.
3. Severity routing is explicit and reusable by downstream incident/onboarding docs.
