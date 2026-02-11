# Approvals -- prepare/commit flow

This guide explains how to run gated actions safely using the prepare/commit flow.
It applies to any action that can require approval (send text, workflows, etc.).

## Why prepare/commit exists

Approvals are a trust boundary. The goal is to avoid approving one thing and
executing another (TOCTOU). The prepare/commit flow solves this by:

- Building a deterministic ActionPlan before any side effects
- Computing a plan hash from that plan
- Binding approvals to the plan hash (and scope)
- Executing only the exact prepared plan

If anything changes (plan content, target pane identity, workspace), commit is
rejected and you must re-prepare.

## When to use prepare/commit

Use prepare/commit whenever:

- A command may require approval (policy says require_approval)
- You want a dry-run preview before executing
- You need a stable plan hash for auditing or automation

You can still use direct commands, but prepare/commit is the safest and most
predictable path.

## Human CLI quickstart

Prepare a plan (no side effects):

```bash
ft prepare send --pane-id 3 "ls"
ft prepare workflow run handle_compaction --pane-id 3
```

The output includes:

- plan_id and plan_hash
- a plan preview (steps, preconditions, verification)
- approval instructions if required
- the commit command

Commit the prepared plan:

```bash
ft commit plan:abcd1234 --text "ls"
```

If approval is required, include the approval code from prepare:

```bash
ft commit plan:abcd1234 --text "rm -rf /tmp/test" --approval-code AB12CD34
```

## Plan hash and scope

The plan hash is derived from the ActionPlan content (not timestamps). It is
used to bind approvals to exactly one plan. A valid approval is also scoped by:

- workspace_id
- pane_uuid (for pane-scoped steps)
- action kinds present in the plan
- expiry (TTL)

If any scope check fails, commit is rejected.

## Approval expiry

Approvals are single-use and time-limited. If the approval expires or has
already been consumed, you must re-run prepare and approve again.

## Troubleshooting

Common refusal reasons and what to do next:

- Plan not found: re-run `ft prepare ...` to create a new plan.
- Plan expired: re-run prepare and approve again.
- Plan hash mismatch: re-run prepare; approvals only work for the exact plan.
- Approval missing: run `ft approve <code>` or pass `--approval-code` on commit.
- Pane mismatch: the pane identity changed; re-run prepare for the current pane.
- Preconditions failed: state changed; re-run prepare and re-check the preview.

## Related docs

- docs/action-plan-schema.md -- plan structure and hashing details
- docs/risk-model-design.md -- how actions become require_approval
- docs/cli-reference.md -- command surface
