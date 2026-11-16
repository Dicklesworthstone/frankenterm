# Robot Family Contract: `agent-mail-failover`

**Beads:** `ft-5lsqo`, `ft-5lsqo.1`
**Status:** static contract for the Beads-only fallback snapshot emitted when
Agent Mail is unavailable or recovering.

## Purpose

`scripts/swarm-tick.sh --agent-mail-fallback frankenterm` is the AGENTS.md
fallback surface for red-mail mode. It emits a read-only JSON snapshot that lets
agents coordinate from Beads and git without repairing Agent Mail or guessing
whether an in-progress bead is stale.

The schema is `ft-agent-mail-failover-snapshot.json`, with retained fixture
coverage in `fixtures/agent-mail-failover/snapshots.v1.json`.

## Required Semantics

The snapshot must carry:

- Agent Mail status and the single-retry fallback marker.
- Forbidden service and destructive actions.
- Beads ready and in-progress counts.
- Active assignees, bead age, and stale-over-two-hours classification.
- Stale-reopen guidance that defaults to `do_not_reopen`.
- Dirty-tree risk, including tracked overlap and untracked review-required paths.
- Next actions that tell agents to use Beads as the source of truth until Agent
  Mail recovers.
- A proof disclaimer saying the snapshot is coordination-only, not Cargo/RCH
  source proof.

## Safety Rules

This contract never authorizes:

- `am service restart`, `am service stop`, `am doctor fix`, `am doctor repair`,
  or `am doctor reconstruct`;
- killing `am`, `am serve-http`, or `mcp-agent-mail` processes;
- deleting files, running destructive git cleanup, mutating RCH workers,
  cancelling RCH builds, or counting local Cargo as proof; or
- reopening another agent's bead solely because Agent Mail is unavailable.

Dirty tracked/shared paths keep stale-reopen recommendations conservative until
ownership is explicit. Untracked paths remain review-required; they are not
cleanup authorization.

## Fixture Coverage

The fixture corpus covers:

- healthy Agent Mail;
- unavailable after the allowed retry;
- transient database-recovery failure text;
- no in-progress beads;
- stale candidates present;
- dirty tracked overlap; and
- untracked review-required paths.

## Proof Posture

This is a static schema/fixture contract. Local static proof is sufficient:

```text
jq empty docs/json-schema/ft-agent-mail-failover-snapshot.json fixtures/agent-mail-failover/snapshots.v1.json
bash fixtures/agent-mail-failover/verify-fallback-snapshots.sh
git diff --check -- docs/json-schema/ft-agent-mail-failover-snapshot.json docs/robot-contracts/agent-mail-failover.md fixtures/agent-mail-failover
br dep cycles --json
```

If future work adds Rust, CLI, robot, or MCP implementation, all Cargo-heavy
proof must use RCH. Local Cargo output is not closeout evidence.
