# Robot Contract: `agent-mail-failover-completion-audit`

Bead: `ft-5lsqo.6`

Status: static graph-completion audit for the `ft-5lsqo` Agent Mail failover
family.

## Purpose

This audit proves that the Agent Mail failover graph is self-contained before
the parent epic is considered ready for closeout. It maps every child bead to
its artifacts, static verifier, proof level, blocker status, and safety posture.

The retained manifest is
`fixtures/agent-mail-failover/completion-audit.v1.json`. The verifier is
`tests/e2e/test_agent_mail_failover_completion_audit.sh`.

## Completion Scope

The audit covers:

- `ft-5lsqo.1` fallback snapshot schema and fixture corpus
- `ft-5lsqo.2` retry classifier and no-repair reason codes
- `ft-5lsqo.3` stale-reopen and dirty-tree overlap policy
- `ft-5lsqo.4` no-service-action static gate
- `ft-5lsqo.5` operator runbook and recovery handoff wording

All five children must be closed in Beads with explicit close reasons before
this audit can pass. The audit also checks that `ft-5lsqo.6` depends on the
five children and the parent epic.

## Recorded Graph Evidence

Closeout records:

- `br dep cycles --json` returned `count: 0`
- `bv --robot-triage --robot-next` selected `ft-4tp7g` but marked it blocked by
  `ft-5xwsu.3`
- `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` emitted
  `mode: agent_mail_unavailable_beads_only`

The fallback snapshot also reported dirty tracked and untracked paths. Those
paths are coordination evidence, not defects in this static audit.

## Static Proof

Run:

```bash
jq empty fixtures/agent-mail-failover/completion-audit.v1.json
bash -n tests/e2e/test_agent_mail_failover_completion_audit.sh
bash tests/e2e/test_agent_mail_failover_completion_audit.sh
git diff --check -- docs/robot-contracts/agent-mail-failover-completion-audit.md \
  fixtures/agent-mail-failover/completion-audit.v1.json \
  tests/e2e/test_agent_mail_failover_completion_audit.sh \
  .beads/issues.jsonl
br dep cycles --json
```

No Cargo or RCH proof is required for this static audit slice.
