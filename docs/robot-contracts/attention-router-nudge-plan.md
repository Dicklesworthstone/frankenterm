# Attention Router Nudge-Plan Receipts

Tracking bead: `ft-x3nsb.5.1`

Status: static supplement for the planned `ft.attention_router.v1` contract.
This document defines the receipt shape for side-effect-free nudge
recommendations. It does not claim a shipped CLI, Robot Mode, MCP, or automatic
messaging implementation.

Canonical schema: `docs/json-schema/ft-attention-router-nudge-plan-receipt.json`

Fixture inventory: `fixtures/attention-router/nudge-plan-receipts.v1.json`

## Purpose

The attention router sometimes needs to recommend communication instead of
more code edits. A nudge-plan receipt records that recommendation in a stable
shape: why communication is needed, who or what it targets, what command text
an operator could review, and which mutations remain forbidden.

The receipt is evidence, not an action. `nudge.mutates` is always `false`.
Command hints are advisory text for a human or agent to review and run
separately.

## Receipt Types

| Kind | Use when | Safe next action |
| --- | --- | --- |
| `acknowledge_request` | Agent Mail reports an `ack_required` direct message. | Acknowledge or reply before starting unrelated work. |
| `reply_to_thread` | A direct question or handoff asks for a substantive answer. | Reply narrowly in the existing thread. |
| `status_check` | An in-progress Bead looks stale across tracker, mail, and git evidence. | Ask the current owner for status before takeover review. |
| `handoff_request` | Dirty paths or reservations overlap the candidate work. | Request ownership clarification or choose disjoint work. |
| `force_release_review` | A prior status-check plus multiple evidence sources suggest abandonment. | Prepare evidence for human/operator review only. |
| `no_action` | The safe outcome is to leave the target untouched. | Do not mutate; pick another lane. |

## Escalation Rules

- Elapsed time alone is never sufficient evidence for force-release or
  takeover.
- A status-check comes before force-release review.
- Dirty-path overlap stays `dirty_overlap` or `do_not_touch` until the owner
  explicitly hands off or the user directs otherwise.
- Pane idle text is only a weak liveness hint. It is not stuck-agent evidence
  by itself.
- Any mutating follow-up remains outside the router contract unless a future
  policy-gated command with dry-run preview is designed.

## Forbidden Actions

Nudge-plan generation must never:

- Send Agent Mail, acknowledge Agent Mail, post Beads comments, release
  reservations, reopen work, or force-release ownership automatically.
- Restart, stop, repair, reconstruct, or kill Agent Mail.
- Restart, repair, mutate workers, cancel builds, or delete remote mirrors in
  RCH.
- Delete files, clean targets, stash/revert/overwrite unrelated work, or edit
  another agent's dirty paths.
- Treat local Cargo, local clippy, local tests, or local e2e as proof for a
  proof-required lane.
- Store raw pane text, secret material, or full Agent Mail message bodies in
  retained fixtures.

## Static Verification

This pre-code contract is verified with fixture-local static checks:

```bash
bash fixtures/attention-router/verify-nudge-plan-receipts.sh
git diff --check -- docs/json-schema/ft-attention-router-nudge-plan-receipt.json docs/json-schema/PROVENANCE.md docs/robot-contracts/attention-router-nudge-plan.md fixtures/attention-router/nudge-plan-receipts.v1.json fixtures/attention-router/verify-nudge-plan-receipts.sh .beads/issues.jsonl
br dep cycles --json
```

If a future implementation turns these receipts into generated JSON/TOON
goldens, compiled tests and e2e proof must run through RCH. This static slice
does not require local Cargo or RCH material proof.
