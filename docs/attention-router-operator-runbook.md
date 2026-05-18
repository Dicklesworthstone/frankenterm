# Attention Router Operator Runbook

Tracking bead: `ft-x3nsb.7`

Status: supplemental detail for the planned `ft.attention_router.v1` contract.
The canonical operator runbook entry is `docs/operator-runbook.md` section 2C,
linked from `docs/robot-contracts/attention-router.md`. This file preserves the
expanded scenario guidance from `ft-x3nsb.7`; when the two documents differ,
the canonical operator-runbook section controls.

The attention router answers one question: what needs attention now? It does
not claim Beads, send Agent Mail, release reservations, repair services, cancel
builds, clean files, or run proof commands. Treat every command hint as text to
review, not as an action the router already performed.

## Before Acting

Start from the same authoritative evidence the router is expected to read:

```bash
br ready --json
bv --robot-triage --robot-next
br list --status=in_progress --json
git status --short --branch
```

If Agent Mail is degraded, retry once after a short delay. If it still fails,
use the Beads/git fallback surface and continue without repair:

```bash
scripts/swarm-tick.sh --agent-mail-fallback frankenterm
```

For any candidate Bead, inspect the record before mutating state:

```bash
br show <bead-id> --json
```

For proof-heavy lanes, use RCH-only proof. Do not count local Cargo, RCH sync
chatter, queue placement, source transfer, or artifact retrieval as closeout
proof.

## Source Trust Order

Use each source for the question it can answer. Do not let a lower-authority
signal override the tracker or the repo safety rules.

| Source | Trust it for | Do not trust it for |
| --- | --- | --- |
| `br` | Current Beads status, assignee, dependencies, comments, and readiness. | Global priority when the graph is incomplete. |
| `bv` | Advisory ranking, graph pressure, stale suggestions, and conflicts worth checking. | Claimability without `br show` confirmation. |
| Agent Mail | Direct questions, acknowledgements, reservations, and active handoffs. | Source, build, test, or proof success. |
| Git | Dirty paths, staged paths, branch divergence, and recent commits. | Ownership by itself. |
| RCH | Retained remote proof artifacts for Cargo, tests, clippy, benches, and e2e. | Proof when the run failed before the material command. |
| Pane state | Liveness hints and recent operator context. | Stuck-agent proof by itself. |
| Attestations | Durable claims already backed by producing-bead artifacts. | New high-scale capability claims not yet proven. |

When sources disagree, fail closed. A safe answer is often "do not claim this
yet; pick a docs-only or planning slice".

## Classification Guide

| Classification | Operator meaning | Safe next action | Do not do |
| --- | --- | --- | --- |
| `ready_now` | `br` says the Bead is open, unblocked, unassigned or assigned to you, and dirty/reservation checks are clear. | Reserve the owned paths and claim or continue the Bead. | Do not skip `br show`, dirty-path checks, or reservation checks. |
| `blocked_infra` | The next useful action depends on RCH, Agent Mail, CI, disk, or another substrate that is unavailable or degraded. | Record the blocker with exact source evidence, then choose non-blocked work. | Do not restart, repair, kill, cancel, or mutate shared services. |
| `blocked_domain` | A real product dependency blocks the lane. | Work the dependency first, or leave a precise dependency comment. | Do not bypass the dependency because the Bead looks important. |
| `waiting_comm` | A direct question, `ack_required` message, or explicit handoff needs response. | Acknowledge or reply before starting more work. | Do not treat silence as permission to take over. |
| `stale_claim` | An in-progress Bead has no recent Beads, mail, git, or pane evidence, but ownership is not yet safely resolved. | Send a status-check comment or message with the stale evidence. | Do not reopen, force-release, or edit the owner's files without user direction or later proof. |
| `dirty_overlap` | Dirty paths overlap the candidate lane, another owner, or an active reservation. | Stop before edits and choose disjoint work or request handoff. | Do not stage broad paths, format packages, stash, revert, or clean. |
| `proof_starved` | Source changes need retained RCH proof, but no admissible proof artifact exists. | Keep the Bead open or blocked and cite the RCH reason. | Do not substitute local Cargo or wrapper chatter as proof. |
| `do_not_touch` | The candidate requires forbidden mutation, human-owned work, destructive cleanup, active owner paths, or protected files. | Leave it alone unless the user gives explicit written direction. | Do not rationalize the mutation as cleanup or unsticking work. |

## Scenario Playbooks

### No Ready Beads

Evidence shape:

```text
br ready --json => []
bv --robot-triage --robot-next => recommends blocked or unavailable work
```

Classification: `blocked_domain` when real dependencies remain,
`blocked_infra` when the blocker is a substrate, or `ready_now` only after a
new docs/spec/testing-planning Bead is created and appears ready.

Safe action:

1. Run `br list --status=in_progress --json` and inspect active ownership.
2. Inspect the `bv` recommendation with `br show <id> --json`.
3. If the recommendation is blocked, do not claim it.
4. Create or refine a narrow planning/testing Bead only when the queue is
   genuinely empty and the new work has explicit proof expectations.

Beads comment shape:

```text
attention-router: no ready Beads. br ready returned empty; bv recommended
<id>, but br show reports status=<status> with blocker=<reason>. I am using a
docs/planning slice instead of claiming blocked work.
```

### BV And BR Disagree

Evidence shape:

```text
bv: "available for work" or high ranked recommendation
br show <id> --json: status=blocked, closed, or assigned to another owner
```

Classification: `blocked_domain`, `waiting_comm`, or `do_not_touch`, depending
on the `br show` record.

Safe action: trust `br` for actionability. Use `bv` only to explain why the
item is worth revisiting later.

Agent Mail handoff shape:

```text
attention-router: bv ranked <id>, but br show says status=<status>,
assignee=<owner>, blocker=<summary>. I am not claiming it; next safe action is
<specific read-only check or dependency>.
```

### RCH Proof Lane Is Blocked

Evidence shape:

```text
RCH worker selection, queueing, source sync, or wrapper setup happened, but the
remote Cargo, clippy, test, bench, or e2e command did not produce retained
material proof.
```

Classification: `blocked_infra` or `proof_starved`.

Safe action:

1. Capture the RCH command, worker or predicate, target dir, failure point, and
   retained log path if one exists.
2. Add or refresh a Beads blocker comment.
3. Continue only with work whose closeout does not require heavy Rust proof.

Forbidden substitute proof:

```text
local cargo check
local cargo clippy
local cargo test
RCH sync complete
RCH transfer complete
RCH process started
```

Closeout wording:

```text
attention-router: classified <id> as proof_starved. RCH reached
<stage> but did not produce retained material proof for <command>. Keeping the
Bead open/blocked; no local Cargo result is claimed as closeout proof.
```

### Ack-Required Agent Mail

Evidence shape:

```text
Agent Mail inbox contains ack_required=true, a direct question, or a handoff
that changes path ownership.
```

Classification: `waiting_comm`.

Safe action:

1. Acknowledge the message if acknowledgement is required.
2. Reply with the narrow answer, owner/path impact, or handoff acceptance.
3. Only then continue Beads work.

If Agent Mail is unavailable, use the fallback snapshot and Beads comments. Do
not repair, restart, stop, kill, or reconstruct the shared Agent Mail service.

### Stale In-Progress Candidate

Evidence shape:

```text
br list --status=in_progress --json shows old ownership, but there is no fresh
mail, git, Beads comment, or pane evidence.
```

Classification: `stale_claim`.

Safe action:

1. Inspect `br show <id> --json` for comments and dependency changes.
2. Inspect recent commits and dirty paths for the likely owner.
3. Send a status-check comment or message with the stale evidence.
4. Pick another Bead unless the user explicitly directs takeover or later
   evidence proves the lane is free.

Status-check comment shape:

```text
attention-router: stale_claim candidate. Last tracker update=<timestamp>;
owner=<agent>; no recent git/mail evidence found in the checked window. Please
confirm whether this is still active before anyone reclaims paths.
```

### Dirty Overlap

Evidence shape:

```text
git status --short --branch shows dirty tracked paths that overlap the
candidate's likely files, another active Bead, or an Agent Mail reservation.
```

Classification: `dirty_overlap` or `do_not_touch`.

Safe action:

1. Stop before editing or staging.
2. Identify the active owner, Bead, and reserved path if possible.
3. Choose disjoint work or request handoff.
4. If your own change is already mixed with unrelated work, stage only owned
   paths and state the excluded paths in the Beads closeout.

Do not use `git reset --hard`, `git clean`, broad `git checkout --`, stash,
package-wide formatting, or broad `git add .` to escape overlap.

### Reservation Firewall

Evidence shape:

```text
Agent Mail reservation metadata, Beads assignee state, git dirty paths, or
publication refs disagree about who owns the candidate path.
```

Classification: `dirty_overlap` for a clear active reservation/path overlap,
or `do_not_touch` when ownership sources disagree or publication is pending.

Safe action:

1. Treat active exclusive reservations as blocking until they are released or
   expired by policy.
2. Treat a closeout or publication message as evidence, not as a reservation
   release.
3. If Beads owner, reservation holder, and dirty-path attribution disagree,
   ask for a targeted handoff or pick disjoint work.
4. If a Bead is locally closed but not committed, pushed to `origin/main`,
   mirrored to the legacy ref, and released from reservations, wait.
5. If ownership merely looks stale, send a status-check before any operator
   force-release review.

Forbidden shortcuts:

```text
edit_reserved_path
stage_unowned_tracker_changes
commit_another_agents_closeout
claim_dependent_work_before_publication
force_release_without_status_check
```

Fixture anchors: `active-exclusive-reservation-overlap`,
`reservation-release-message-not-released`,
`ownership-source-disagreement`, `local-closeout-publication-pending`, and
`stale-owner-status-before-force-release`.

## Trust Boundary

The router is a read-only decision surface. Routine operators and agents must
not perform these actions as part of attention routing:

- Agent Mail restart, stop, doctor repair/fix/reconstruct, process kill, or
  shared service debugging beyond one retry plus fallback.
- RCH restart, deploy, repair, worker mutation, queue cancellation, build
  cancellation, or remote mirror deletion.
- File deletion, target cleanup, destructive git cleanup, broad checkout,
  hard reset, or clean.
- Local Cargo, local clippy, local tests, local benches, or local e2e as
  closeout proof for RCH-required lanes.
- Editing another agent's dirty paths or staging another agent's Beads/doc
  closeout with your commit.
- Force-release, reopen, or takeover of a stale-looking lane without explicit
  user direction or later evidence that makes the lane safely available.

## Citing Router Evidence

Use short, source-grounded citations in Beads comments and Agent Mail. Include
the classification, sources checked, and the exact next action.

Beads closeout or blocker comment:

```text
attention-router: classification=<classification>; sources=<br,bv,git,rch,mail>;
evidence=<one sentence with ids/paths>; next=<safe action>. Proof=<artifact or
static check>; exclusions=<dirty paths or owners not touched>.
```

Agent Mail handoff:

```text
attention-router handoff for <id>: classification=<classification>. I checked
br=<status>, bv=<advisory summary>, git=<dirty summary>, rch=<health>, mail=<mail
state>. I touched <paths>; I avoided <paths/owners>. Next safe action is
<specific action>.
```

For docs-only slices, static proof is enough when no generated or compiled
examples are introduced:

```text
Proof: rg link/schema phrase checks; git diff --check on owned files; br dep
cycles --json count=0. No Cargo/RCH proof was run because this is docs-only.
```

For source slices, cite retained RCH artifacts instead:

```text
Proof: RCH command=<argv>; worker=<worker or predicate>; target=<target dir>;
result=<pass/fail>; artifacts=<paths>. Local Cargo was not used for closeout.
```

## High-Scale Claims

Do not turn an attention item into a capability claim. A snapshot may say that
64+ CPU, 256 GiB, high-agent-count, or target-class proof work needs attention,
but it cannot claim the capability shipped.

Use this rule:

- If a claim is backed by `docs/attestations/manifest.json` and the cited
  producing-bead artifact is retained, cite the artifact.
- If the latest retained artifact says skipped, degraded, blocked, or
  inconclusive, preserve that status in the attention item.
- If the work is planned but unproven, say "planned" or "target" and keep the
  Bead open until proof exists.

Safe wording:

```text
attention-router: high-scale target-class proof is proof_starved because
<artifact/status> is <skipped|blocked|missing>. Next action is to unblock RCH or
work a non-proof docs slice; no high-scale pass is claimed.
```

## Manual Use Until Implementation

Until a CLI, Robot Mode, or MCP command ships, operators can emulate the router
by writing the classification explicitly in comments and handoffs:

1. Gather `br`, `bv`, Agent Mail/fallback, git, and RCH state.
2. Assign one classification from this runbook.
3. Pick the least-mutating safe next action.
4. Reserve only owned paths.
5. Claim or continue only after `br show` and dirty-path checks agree.
6. Close with static docs proof or retained RCH proof, matching the change.

This manual loop is slower than a future router, but it preserves the important
property: no stale, blocked, dirty, or proof-starved lane is made to look ready.
