# Agent Mail Failover Runbook

Bead: `ft-5lsqo.5`

Status: operator and agent workflow for sessions where Agent Mail is unavailable
after the single allowed retry.

## Scope

This runbook is for communication outage handling only. It does not diagnose
source defects, does not claim RCH fleet health, and does not make dirty-tree
ownership decisions by itself. Those remain separate evidence streams.

Use this when Agent Mail startup, registration, contact approval, or inbox
checking fails after one retry.

## Startup Sequence

1. Attempt normal Agent Mail registration and inbox/context fetch.
2. If Agent Mail is unavailable, recovering, timing out, or failing
   registration/contact setup, wait briefly and retry once.
3. If the retry does not produce usable mailbox coordination, stop using Agent
   Mail for this session and classify the outcome with
   `scripts/agent-mail-failover-classifier.sh`.
4. Capture the Beads/git fallback state with
   `scripts/swarm-tick.sh --agent-mail-fallback frankenterm`.
5. Continue only from Beads and git evidence until Agent Mail is usable again.

`FT_AGENT_MAIL_FAILURE_CLASS` may be set before the fallback snapshot when the
observed outcome is known. Supported classes are documented in
`fixtures/agent-mail-failover/retry-classifier-cases.json`.

## Ready And Triage

Use Beads as the claimability source of truth:

```text
br ready --json
br list --status in_progress --json
```

Use `bv --robot-triage` only as graph context. If `br ready --json` is empty or
`bv --robot-triage` reports blocked actionable work, inspect the exact bead with
`br show <id> --json` before making any claim decision.

## Stale Reopen

Apply `docs/robot-contracts/agent-mail-stale-reopen-policy.md`.

Default posture is `do_not_reopen`. A stale in-progress bead can receive only a
status-check comment unless all required evidence is clean:

- age exceeds the threshold in the latest fallback snapshot
- no recent owner signal exists
- dirty tracked paths do not overlap the likely work surface
- relevant untracked files are not present or ownership is explicit
- Beads status and dependencies make the reopen safe

Dirty tracked overlap, relevant untracked files, active reservations, or recent
owner comments keep the action at `do_not_reopen`.

## Dirty-Tree Handling

Treat dirty tracked paths as another active pane's work until ownership is
explicit. Treat relevant untracked paths as drafts requiring review. Do not
stage, overwrite, replace, or include those paths in your closeout unless they
are yours.

Separate the dirty tree from the Agent Mail outage: a communication outage does
not prove a source defect, and a source defect does not prove Agent Mail health.
RCH fleet pressure is also separate; transfer logs and sync chatter are not
build proof.

## Beads Handoff Template

Review the fallback snapshot and then post a concise Beads comment:

```text
Agent Mail fallback handoff for <bead-id>

- reason_codes: <agent_mail.reason_codes>
- fallback_snapshot_mode: <mode>
- ready_count: <beads.ready_count>
- in_progress_count: <beads.in_progress_count>
- dirty_risk: <git.risk_level>
- stale_reopen_default: <beads.stale_reopen.default_action>
- touched_paths: <owned paths only>
- avoided_paths: <dirty or unowned paths>
- proof_commands: <commands actually run>

Agent Mail was not usable after the single retry. Continuing from Beads and git
evidence only; no service repair, source cleanup, worker mutation, or local
Cargo proof is claimed.
```

`scripts/swarm-tick.sh --agent-mail-handoff --bead <id> ... frankenterm` can
format a draft block. Review it before posting with `br comments add`.

## Recovery Acknowledgement Template

When Agent Mail becomes usable again, send or post a short acknowledgement:

```text
Agent Mail recovery acknowledgement

- session: frankenterm
- recovered_at: <timestamp>
- mailbox_action: registration/inbox coordination resumed
- beads_state_checked: <br ready / br show command>
- active_bead: <id or none>
- fallback_artifacts_used: <snapshot, gate, or runbook paths>

Beads remains the source of truth for claimability. Work completed during the
outage is documented in Beads comments and commits.
```

## Retained Proof

The retained contract and static proof surfaces are:

- `docs/robot-contracts/agent-mail-failover-snapshot.md`
- `docs/robot-contracts/agent-mail-stale-reopen-policy.md`
- `fixtures/agent-mail-failover/manifest.json`
- `fixtures/agent-mail-failover/no-service-action-gate.json`
- `fixtures/agent-mail-no-service-action/manifest.json`
- `docs/robot-contracts/agent-mail-no-service-action-gate.md`
- `tests/e2e/test_agent_mail_failover_snapshot_contract.sh`
- `tests/e2e/test_agent_mail_retry_classifier_contract.sh`
- `tests/e2e/test_agent_mail_no_service_action_gate.sh`
- `tests/e2e/test_agent_mail_no_service_action_contract.sh`
- `tests/e2e/test_agent_mail_failover_runbook_contract.sh`

Runbook smoke proof:

```text
bash tests/e2e/test_agent_mail_failover_runbook_contract.sh
git diff --check -- docs/robot-contracts/agent-mail-failover-runbook.md docs/robot-contracts/agent-mail-no-service-action-gate.md tests/e2e/test_agent_mail_failover_runbook_contract.sh fixtures/agent-mail-failover/manifest.json fixtures/agent-mail-no-service-action/manifest.json
br dep cycles --json
```
