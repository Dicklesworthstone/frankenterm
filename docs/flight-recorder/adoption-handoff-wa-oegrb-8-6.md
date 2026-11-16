# Recorder Adoption, Onboarding, and Handoff Package (`wa-oegrb.8.6`)

Date: 2026-02-14  
Status: Baseline long-term ownership package for recorder rollout track  
Depends on: `wa-oegrb.8.2`, `wa-oegrb.8.4`, `wa-oegrb.8.5`

## Purpose

Turn recorder architecture and rollout artifacts into repeatable day-to-day
operating practices for developers, operators, and agentic workflows.

This package includes:
1. Role-specific onboarding guides
2. Maintenance cadence recommendations
3. Session handoff checklist
4. Adoption success metrics

Read with:
- `docs/flight-recorder/rollout-plan-wa-oegrb-8-1.md`
- `docs/flight-recorder/migration-plan-wa-oegrb-8-2.md`
- `docs/flight-recorder/ops-runbook-wa-oegrb-8-4.md`
- `docs/flight-recorder/incident-response-wa-oegrb-8-5.md`
- `docs/flight-recorder/validation-gates-wa-oegrb-7-5.md`

## Role-Specific Onboarding

### 1. Developer Onboarding (Build + Validate + Debug)

Goal:
- contributor can run recorder validation surfaces and reason about migration/rollback constraints.

Day-1 checklist:

```bash
cargo check -p frankenterm-core --all-targets
scripts/check_recorder_validation_gates.sh
ft search fts verify
ft doctor
```

Developer expectations:
1. Understand query contract compatibility (`docs/json-schema/wa-robot-search.json`).
2. Preserve lexical-safe fallback semantics for any semantic/hybrid changes.
3. Update docs + beads whenever rollout/ops behavior changes.
4. Attach validation evidence to bead comments before closure.

### 2. Operator Onboarding (Triage + Stability + Rollback)

Goal:
- on-call operator can detect and contain recorder incidents without code-level context.

Day-1 checklist:

```bash
ft status
ft triage --severity warning
ft search fts verify
ft doctor
ft db check -f plain
```

Operator expectations:
1. Follow `wa-oegrb.8.4` runbook first; escalate per severity matrix.
2. Freeze rollout progression when high/urgent recorder incidents are active.
3. Capture incident bundles for all high/urgent events.
4. Record decision checkpoints and rollback posture in incident timeline.

### 3. Agent Onboarding (Robot + Beads + Mail Coordination)

Goal:
- agent can pick impactful beads, avoid collisions, and leave auditable progress.

Agent workflow checklist:
1. Run `bv --robot-next` and `bv --robot-triage` before claiming work.
2. Claim bead and reserve file scope before edits.
3. Post intent + scope in swarm thread.
4. Add bead evidence comment and close when acceptance is met.
5. Release reservations and post completion update.

Minimum command set:

```bash
bv --robot-next
bv --robot-triage
br show <bead_id>
br update <bead_id> --claim --actor <agent>
br comments add <bead_id> --message "..."
br close <bead_id> -r "..."
```

## Maintenance Cadence

### Daily

Owner: on-call operator

1. Recorder health checks:
- `ft status`
- `ft triage --severity warning`
- `ft search fts verify`
2. Confirm no unresolved high/urgent recorder incidents.
3. Confirm no sustained drift in lag/queue/backpressure signals.

### Weekly

Owner: recorder owner + on-call backup

1. Run full validation gate script:

```bash
scripts/check_recorder_validation_gates.sh
```

2. Run recovery drill suite:

```bash
CARGO_TARGET_DIR=target-recovery-drills \
  cargo test -p frankenterm-core --test recorder_recovery_drills -- --nocapture
```

3. Review incident/postmortem backlog mapping for unassigned follow-ups.
4. Review retention and policy posture against current workload.

### Per Release / Phase Transition

Owner: change owner + approver

1. Reconfirm rollout phase entry/exit criteria from `wa-oegrb.8.1`.
2. Reconfirm migration assumptions from `wa-oegrb.8.2`.
3. Verify ops and incident templates are current (`8.4`, `8.5`).
4. Publish explicit `GO`/`NO-GO` decision log with evidence packet links.

## Session Handoff Checklist

Use this at end-of-session or owner transition:

```md
## Recorder Handoff
- Date/Time (UTC):
- Outgoing owner:
- Incoming owner:
- Active rollout phase:
- Current incident state (none/open):

### Completed this session
- bead IDs closed:
- docs/code touched:
- validation commands run:

### In-flight work
- bead IDs in progress:
- open risks:
- pending approvals:

### Operational state
- latest `ft search fts verify` summary:
- latest `ft doctor` summary:
- unresolved alerts:

### Next recommended bead(s)
- `bv` top picks:
- selected next action:
```

Handoff rule:
- no in-progress incident may be handed off without explicit owner acceptance and current evidence links.

## Adoption Success Metrics

Track these metrics weekly and per rollout phase gate:

| Metric | Definition | Target | Source |
|---|---|---|---|
| Onboarding completion time | Time for new contributor to run core recorder workflow independently | <= 1 day | onboarding checklist signoff |
| Validation gate reliability | % of scheduled gate runs completing with actionable output | >= 95% | `wa-oegrb.7.5` artifacts |
| Query efficacy | % of sampled operational queries returning expected results after cutover | >= 95% | parity worksheets + operator review |
| Incident MTTR | median time from detection to stabilized service | improving trend; <= 60m for high severity | incident timelines (`wa-oegrb.8.5`) |
| Rollback readiness | % of drills where rollback posture is executed within response window | 100% in rehearsals | drill/post-incident reports |
| Handoff continuity | sessions with completed handoff template for active recorder work | 100% | handoff logs |

## Ownership Model

| Area | Primary owner | Backup owner |
|---|---|---|
| Rollout decisions | recorder change owner | incident commander |
| Validation + drills | recorder owner | on-call operator |
| Incident process quality | security/incident owner | recorder owner |
| Onboarding docs hygiene | docs owner for rollout track | active implementing agent |

## Exit Criteria for `wa-oegrb.8.6`

1. Role-specific onboarding is explicit for developer/operator/agent workflows.
2. Maintenance cadence is defined for daily, weekly, and phase-transition operations.
3. Handoff checklist enables continuity across sessions without rediscovery.
4. Adoption metrics are concrete and measurable.
