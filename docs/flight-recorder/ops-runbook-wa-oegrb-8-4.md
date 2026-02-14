# Recorder Ops Runbook (`wa-oegrb.8.4`)

Date: 2026-02-14  
Status: Draft runbook baseline for rollout-track operations  
Depends on: `wa-oegrb.8.1`, `wa-oegrb.7.4`, `wa-oegrb.3.6`

## Purpose

Provide a practical operator playbook for recorder/index/search reliability:

1. Healthy-state checks and drift detection.
2. Common failure signatures and deterministic remediation.
3. Escalation/ownership routing.
4. Standard evidence capture for incident and postmortem workflows.

Read with:
- `docs/flight-recorder/rollout-plan-wa-oegrb-8-1.md`
- `docs/flight-recorder/incident-response-wa-oegrb-8-5.md`
- `docs/flight-recorder/alerts-wa-oegrb-8-4.md`
- `docs/flight-recorder/recovery-drills-wa-oegrb-7-4.md`
- `docs/flight-recorder/validation-gates-wa-oegrb-7-5.md`
- `docs/flight-recorder/security-privacy-validation-wa-oegrb-7-6.md`
- `docs/flight-recorder/recorder-governance-policy.md`

## Quick Triage Loop

Run this loop first before deeper intervention:

```bash
ft status --health | jq
ft triage --severity warning
ft events --event-type gap --unhandled
ft robot events --event-type gap --limit 20
```

Interpretation anchors:
- `health.db_writable` must remain `true`.
- `health.ingest_lag_max_ms` should remain below sustained-warning thresholds.
- `health.scheduler.total_*` counters should not trend upward continuously.
- `gap` events are expected only for explicit discontinuities; repeated gap bursts are a reliability signal.

## Healthy-State Checklist

The system is considered healthy when all are true:

1. `ft status --health` reports watcher running and `db_writable=true`.
2. `ingest_lag_max_ms` remains under warning threshold in normal load windows.
3. `capture_queue_depth` and `write_queue_depth` return to near-zero between bursts.
4. `backpressure_tier` remains `Green` (or unset in low/no-pressure runs).
5. No sustained increase in:
- `scheduler.total_rate_limited`
- `scheduler.total_byte_budget_exceeded`
- `scheduler.total_throttle_events`
6. No recurring unhandled `gap` events without a known operational cause.

## Failure Signatures and Response

### 1) Database Not Writable / Storage Degrade

Symptoms:
- `health.db_writable=false`
- status warnings mention write failures or degraded storage behavior

Actions:
1. Freeze rollout progression (no phase advancement under `wa-oegrb.8.1`).
2. Verify storage mode and fallback policy from config.
3. If user impact is active, apply rollback posture from rollout plan:
- disable recorder and keep lexical-safe search defaults.
4. Capture evidence bundle:
- `ft reproduce --kind manual`
- `ft diag bundle --output /tmp/ft-diag-recorder-db`
5. Open/attach incident with bundle paths and health snapshot JSON.

### 2) Sustained Ingest Lag / Queue Buildup

Symptoms:
- `ingest_lag_max_ms` above warning/critical thresholds
- `capture_queue_depth` or `write_queue_depth` remain elevated

Actions:
1. Confirm whether load is expected (planned stress/canary window) or anomalous.
2. Check backpressure + scheduler counters:
- `health.backpressure_tier`
- `health.scheduler.total_throttle_events`
3. If lag remains critical through the response window:
- reduce rollout exposure (canary rollback or shadow-only posture)
- run targeted recovery drill verification from `wa-oegrb.7.4`
4. Capture timeline:
- 3+ consecutive `ft status --health` snapshots (include timestamps)
- matching `ft events --event-type gap` output

### 3) Gap Surge / Backpressure Overflow

Symptoms:
- recurring `gap` events in events stream
- search/event traces include `backpressure_overflow` indicators

Actions:
1. Verify if gaps are expected (restart/recovery window) vs unplanned.
2. Query for overflow markers:
```bash
ft search "backpressure_overflow" --limit 50
ft robot search "backpressure_overflow" --limit 50 --mode lexical
```
3. If unplanned and sustained:
- treat as reliability incident
- hold rollout progression
- execute recovery drill set before resuming phase transitions

### 4) Crash Loop / Repeated Restarts

Symptoms:
- `health.in_crash_loop=true`
- `health.consecutive_crashes > 0`
- repeated watcher restarts during normal load

Actions:
1. Move to safe rollout posture (Phase 0 or prior known-good phase).
2. Collect latest crash bundle and incident bundle:
```bash
ft reproduce --kind crash
ft diag bundle --output /tmp/ft-diag-crash-loop
```
3. Route incident as critical with crash/health artifacts attached.

### 5) Privacy/Policy Integrity Regression

Symptoms:
- security/privacy suite failure
- redaction leak indicators or audit integrity mismatch

Actions:
1. Stop rollout advancement immediately.
2. Disable risky exposure paths per rollout rollback rules.
3. Re-run validation and security suites:
```bash
scripts/check_recorder_validation_gates.sh
cargo test -p frankenterm-core --test recorder_security_privacy_validation -- --nocapture
```
4. Escalate as privacy/security incident and require explicit go/no-go reapproval.

## Escalation and Ownership

| Condition | Initial owner | Escalate to | SLA |
|---|---|---|---|
| Ingest lag warning | Recorder on-call | Search/index owner | 30m |
| Ingest lag critical | Recorder on-call | Incident commander | 10m |
| DB not writable | Recorder on-call | Storage owner + incident commander | 10m |
| Gap surge (unknown cause) | Recorder on-call | Capture/storage owners | 15m |
| Crash loop | Recorder on-call | Incident commander | 5m |
| Privacy leak/audit integrity failure | Security owner + recorder on-call | Incident commander + approver | Immediate |

## Maintenance Procedures

### Daily

1. Review `ft status --health` snapshot for lag/queue/backpressure drift.
2. Review unhandled gap events:
```bash
ft events --event-type gap --unhandled
```
3. Confirm no unresolved warning-level triage items:
```bash
ft triage --severity warning
```

### Pre-Phase Change (Rollout Gate)

1. Re-run recorder validation gates:
```bash
scripts/check_recorder_validation_gates.sh
```
2. Verify security/privacy suite:
```bash
cargo test -p frankenterm-core --test recorder_security_privacy_validation -- --nocapture
```
3. Confirm recovery drill readiness:
```bash
cargo test -p frankenterm-core --test recorder_recovery_drills -- --nocapture
```
4. Attach evidence packet to rollout decision log template (`wa-oegrb.8.1`).

## Incident Evidence Template

```md
## Recorder Incident Evidence
- Date/Time (UTC):
- Reporter:
- Rollout phase:
- Trigger condition:

### Health snapshots
- Snapshot 1:
- Snapshot 2:
- Snapshot 3:

### Event evidence
- `ft events --event-type gap --unhandled` output:
- Relevant `ft search`/`ft robot search` excerpts:

### Artifacts
- crash bundle path:
- incident bundle path:
- diag bundle path:

### Actions taken
- rollback posture:
- mitigations:
- follow-up beads:
```

## Exit Criteria for `wa-oegrb.8.4`

1. Runbook supports on-call response without implementation deep-dives.
2. Alert mappings are explicit and paired with concrete commands.
3. Escalation matrix and evidence template are reusable for `wa-oegrb.8.5` and `.8.6`.
