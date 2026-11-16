# Recorder Incident Response and Postmortem Process (`wa-oegrb.8.5`)

Date: 2026-02-14  
Status: Baseline incident-response contract for recorder rollout operations  
Depends on: `wa-oegrb.8.4`

## Purpose

Define a recorder-specific incident framework that is executable during active
failures and produces postmortems that map directly into actionable backlog
work.

This document provides:
1. Incident classification matrix
2. Response timeline/checklist
3. Postmortem template with recorder-specific evidence requirements
4. Feedback loop from incidents into beads

Read with:
- `docs/flight-recorder/ops-runbook-wa-oegrb-8-4.md`
- `docs/flight-recorder/rollout-plan-wa-oegrb-8-1.md`
- `docs/flight-recorder/recovery-drills-wa-oegrb-7-4.md`
- `docs/flight-recorder/recorder-governance-policy.md`
- `docs/incident-bundles.md`

## Incident Classification Matrix

| Class ID | Class | Typical trigger | Primary risk | Default severity | Required escalation |
|---|---|---|---|---|---|
| `REC-DATA-LOSS` | Data loss / capture discontinuity | sustained gap bursts, missing replay ranges, failed resume | forensic incompleteness | high | recorder owner + incident commander |
| `REC-ORDER-DRIFT` | Ordering/checkpoint drift | checkpoint regression, non-monotonic replay indicators | incorrect replay/causality | high | recorder owner |
| `REC-PRIVACY-LEAK` | Privacy/redaction failure | secret leakage in query/output/artifacts | security/compliance breach | urgent | security approver + incident commander |
| `REC-PERF-COLLAPSE` | Performance/availability collapse | storage tier black, persistent lag, crash loops | degraded operations and rollout instability | high (urgent if prolonged) | recorder owner + incident commander |

Escalation rule:
- Any integrity or privacy violation (`REC-PRIVACY-LEAK`, confirmed corruption) is treated as `urgent` immediately.

## Standard Response Timeline

### T+0 to T+5 minutes (Detect + Triage)

1. Declare incident class and provisional severity.
2. Freeze rollout advancement (no phase promotion while incident is active).
3. Start evidence capture:

```bash
ft status
ft triage -f json
ft search fts verify
ft doctor
ft db check -f plain
```

4. If crash or suspected policy issue, export incident artifacts:

```bash
ft reproduce export --kind crash
# or
ft reproduce export --kind manual
```

### T+5 to T+15 minutes (Contain + Stabilize)

1. Apply containment from runbook (reindex, degrade/failover, rollback posture).
2. Confirm lexical-safe search path remains available.
3. For privacy/integrity signals, disable risky exposure and require explicit approval for privileged operations.
4. Assign owners:
- incident commander
- technical lead (recorder owner)
- communications owner

### T+15 to T+30 minutes (Decision Checkpoint)

1. Decide `GO`/`PARTIAL`/`ROLLBACK` for current rollout phase.
2. Record decision with timestamp and rationale.
3. Publish current customer/operator impact statement.
4. Ensure all open action items have an owner and next deadline.

### T+30+ minutes (Recovery + Verification)

1. Execute recovery drills or targeted validation where needed.
2. Re-run key health checks and verify stabilization.
3. Transition incident to monitoring mode only after criteria are met.

## Live Incident Checklist

- [ ] Incident class selected (`REC-DATA-LOSS`, `REC-ORDER-DRIFT`, `REC-PRIVACY-LEAK`, `REC-PERF-COLLAPSE`)
- [ ] Severity assigned (`warning`, `high`, `urgent`)
- [ ] Rollout progression frozen
- [ ] Evidence commands executed and outputs saved
- [ ] Containment action executed
- [ ] 30-minute decision checkpoint recorded
- [ ] Stakeholder update posted

## Recorder-Specific Evidence Requirements

Every recorder incident record must include:

1. **Health snapshots**: at least three timestamped snapshots during incident progression.
2. **Indexing integrity evidence**: `ft search fts verify` output before and after mitigation.
3. **Policy/redaction evidence** (if applicable): secret scan or privacy validation output.
4. **Recovery evidence** (if applicable): relevant `[ARTIFACT][recorder-recovery-drill] ...` lines.
5. **Bundle artifacts**:
- `incident_manifest.json`
- `redaction_report.json`
- `health_snapshot.json` (if available)
6. **Config posture diff** for any rollback/failover actions.

## Postmortem Template (Recorder-Specific)

```md
# Recorder Incident Postmortem

## 1) Summary
- Incident ID:
- Date/Time (UTC):
- Class:
- Severity:
- Rollout phase at incident time:
- User/operational impact:

## 2) Timeline
- Detection:
- Triage started:
- Containment started:
- Decision checkpoint(s):
- Recovery complete:

## 3) What Failed
- Trigger condition:
- Failing component(s):
- Why safeguards did/did not catch it:

## 4) Evidence
- Health snapshots:
- FTS verify output:
- Incident bundle path:
- Recovery drill artifacts (if run):
- Policy/redaction evidence (if applicable):

## 5) Root Cause
- Primary root cause:
- Contributing factors:
- Why this escaped prior validation:

## 6) Corrective Actions
- Immediate fixes:
- Preventive changes:
- Owner + due date for each action:

## 7) Backlog Mapping
- New bead IDs created:
- Existing beads updated/reopened:
- Priority rationale:

## 8) Verification Plan
- How fixes will be validated:
- Required gate(s) to re-enter rollout progression:
```

## Feedback Loop to Beads

After each incident/postmortem:

1. Create or update bead(s) for each corrective action.
2. Link beads in postmortem `Backlog Mapping` section.
3. Tag by incident class:
- `incident-data-loss`
- `incident-ordering`
- `incident-privacy`
- `incident-performance`
4. For urgent incidents, require one owner-assigned bead before incident closure.
5. Feed recurrent patterns into validation gates and recovery drills.

## Tabletop Exercise Procedure

Run a quarterly tabletop using this process:

1. Pick one class scenario (rotate across all four classes).
2. Execute T+0/T+15/T+30 workflow as a dry run.
3. Fill postmortem template with simulated evidence placeholders.
4. Record procedural gaps and create follow-up beads.

Tabletop completion criteria:
- team can complete timeline/checklist without ad hoc process invention
- evidence list is feasible with existing tooling
- backlog mapping is explicit and actionable

## Exit Criteria for `wa-oegrb.8.5`

1. Incident classification matrix is explicit and actionable.
2. Standard response timeline/checklist is runnable by on-call team.
3. Postmortem template captures recorder-specific evidence requirements.
4. Feedback loop to beads is defined and enforceable.
