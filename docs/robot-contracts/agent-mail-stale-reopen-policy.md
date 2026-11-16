# Robot Contract: `agent-mail-stale-reopen-policy`

Bead: `ft-5lsqo.3`

Status: static policy and fixture contract only; no runtime command is shipped
by this document.

## Purpose

This policy defines how an agent may reason about stale in-progress Beads after
Agent Mail is unavailable after the single allowed retry. It complements the
fallback snapshot contract in
`docs/robot-contracts/agent-mail-failover-snapshot.md` by turning
`beads.stale_reopen` and `git.dirty_paths` into reviewable decisions.

The machine-readable policy is
`fixtures/agent-mail-stale-reopen/policy.v1.json`.

## Non-Authority

This policy does not authorize Agent Mail repair, service restart, process
killing, destructive git cleanup, file deletion, worker mutation, build
cancellation, or local Cargo-as-proof. It also does not authorize immediate
reopen of a stale bead. The strongest positive action it can emit is
`comment_status_check`, which records a Beads status-check comment and waits for
fresh evidence before any reopen.

## Decision Defaults

| Condition | Decision | Required evidence |
|---|---|---|
| In-progress bead is not older than the stale threshold. | `wait_for_owner` | `br list --status in_progress --json` or fallback snapshot age fields. |
| Assignee has a recent Beads comment, Agent Mail handoff, or active file reservation. | `wait_for_owner` | Recent comment, mail receipt, or reservation timestamp. |
| Stale candidate has clean git state and no active owner evidence. | `comment_status_check` | Status-check Beads comment, candidate id, threshold, clean dirty-path scan. |
| Candidate has dirty tracked overlap or reserved-path overlap. | `do_not_reopen` | Dirty/reserved paths and overlap category. |
| Candidate has relevant untracked files with unclear ownership. | `do_not_reopen` | Untracked path list and ownership-review requirement. |
| `br list --status in_progress --json` is empty but `bv --robot-triage` reports blocked work. | `verify_br_show_then_do_not_claim` | `br show` on the referenced bead plus dependency state. |

## Stale Threshold

The default stale threshold is 7200 seconds. A bead older than that threshold is
only a candidate. The candidate still requires:

- no dirty tracked overlap with the bead domain
- no relevant untracked files with unclear ownership
- no active file reservation for the same path family
- no recent Beads comment, Agent Mail handoff, or other owner signal
- a Beads status-check comment before reopening

## Dirty-Tree Policy

Dirty tracked overlap is high risk. If a stale candidate's likely work surface
touches a dirty tracked path, the decision is `do_not_reopen` unless a live
owner explicitly clears ownership. The agent should choose a disjoint ready bead
or stop at a status-check comment.

Untracked files are medium risk. They may be drafts from another pane. The
agent may continue only on clearly disjoint paths; it must not stage, overwrite,
or replace those files.

## Empty `br` Versus Blocked `bv`

`br ready --json` and `br list --status in_progress --json` are the source of
truth for claimability. If `br list --status in_progress --json` is empty while
`bv --robot-triage` reports blocked actionable work, the agent must inspect the
exact bead with `br show <id> --json`. It must not claim a blocked or closed
bead based only on `bv` output.

## Fixture Coverage

`fixtures/agent-mail-stale-reopen/policy.v1.json` covers:

- empty in-progress list
- active in-progress bead below threshold
- clean stale candidate requiring a status-check comment
- dirty tracked overlap denial
- untracked review-required denial
- `br` empty / `bv` blocked mismatch handling

The verifier confirms that dirty overlap and untracked review cases remain
`do_not_reopen`, clean stale work remains status-check-only, and all side-effect
flags stay false.

## Static Proof

Run:

```bash
bash fixtures/agent-mail-stale-reopen/verify-policy.sh
jq empty fixtures/agent-mail-stale-reopen/policy.v1.json
git diff --check -- docs/robot-contracts/agent-mail-stale-reopen-policy.md \
  fixtures/agent-mail-stale-reopen/policy.v1.json \
  fixtures/agent-mail-stale-reopen/verify-policy.sh
br dep cycles --json
```

No Cargo or RCH proof is required for this static policy slice.
