# Blocker Radar Operator Runbook

Status: operator runbook for `ft.blocker_radar.v1`.

Use this when `ft robot blocker-radar`, `ft doctor --json`, or a Beads handoff
reports that a lane is actionable, owned, stale, dirty, externally blocked, or
running with degraded coordination. The blocker radar is read-only. It tells an
agent what can be claimed from current evidence; it does not repair substrates,
prove builds, close source beads, or replace proof-ledger artifacts.

Primary anchors:

- Contract and schema: `docs/blocker-radar-contract.md` and
  `docs/json-schema/ft-blocker-radar.json`.
- Deterministic fixtures:
  `crates/frankenterm-core/tests/fixtures/blocker_radar/conformance_cases.json`.
- Conformance test:
  `crates/frankenterm-core/tests/blocker_radar_conformance.rs`.
- RCH e2e wrapper:
  `tests/e2e/test_ft_9ntud_4_blocker_radar_conformance.sh`.
- Passing conformance artifact for the `ft-9ntud.4` closeout:
  `tests/e2e/artifacts/goal-line/ft-9ntud.4/blocker_radar_conformance/20260511T134857Z/summary.json`.
- Proof-doctor vocabulary:
  `docs/proposals/ft-wik9p-proof-doctor-verdict-schema.md`.
- RCH proof-ledger policy: `docs/asupersync-rch-execution-policy.md`.

## Before Acting

Run the radar, then verify the cited evidence before mutating anything:

```bash
ft robot blocker-radar
ft doctor --json
br show <bead-id> --json
git status --short --branch
```

If Agent Mail is degraded, use the read-only fallback:

```bash
scripts/swarm-tick.sh --agent-mail-fallback frankenterm
```

Do not treat setup, sync, transfer, queue placement, worker selection, or
artifact retrieval chatter as proof that the material command passed. A proof
claim needs the material command result and retained artifacts. For heavy Cargo,
clippy, test, bench, or e2e claims, the acceptable closeout is remote RCH proof
or an explicitly approved local fallback described as degraded evidence.

## Claimability Startup Loop

The claimability surface shipped under `ft-htcwc.3` is the read-only
`ClaimabilityReport` builder in `crates/frankenterm-core/src/blocker_radar.rs`
(`build_claimability_report_from_value`). It is a reconciliation surface, not a
claiming command. Until a robot command wraps it, agents use the same source
sequence by hand:

1. Read the repo instructions and this runbook before choosing work.
2. Try Agent Mail once; if it stays degraded, run
   `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` and cite that
   fallback snapshot.
3. Read authoritative Beads state with `br ready --json` and
   `br show <candidate> --json`.
4. Read `bv --robot-triage` only as an advisory ranking snapshot. BV score,
   PageRank, unblock count, and "available for work" language are never enough
   to claim.
5. Compare the ready queue, individual Beads record, BV recommendation,
   Agent Mail/fallback state, dirty paths, and external queues before claiming.
6. Claim only after the final verdict is `claimable`; otherwise wait, comment,
   file a blocker, or choose another ready bead.

When `br ready --json` is empty, fail closed. A BV recommendation can explain
which dependency or external queue matters most, but it cannot manufacture
ready work. Use idea-wizard/planning only if there is genuinely no claimable
bead.

When Agent Mail is degraded, Beads/git become the handoff surface. Continue
only with explicit fallback citations, and do not repair, restart, stop, or kill
Agent Mail.

### Claimability Examples

| Final verdict | Example evidence | Safe outcome |
| --- | --- | --- |
| `tracker_inconsistent` | `bv --robot-triage` recommends `ft-e87u6.2` as "Currently unclaimed - available for work", but `br show ft-e87u6.2 --json` says `status=blocked`, `assignee=BluePike`, with fresh PR 59 queue comments. | Do not claim. Wait or coordinate with the owner; record `bv.br_status_mismatch`. |
| `external_wait` | GitHub Actions current-head run is queued or the check suite has zero jobs and no failure log. | Recheck read-only later or comment with run/check ids; do not cancel, rerun, or infer pass/fail. |
| `mail_degraded` | Agent Mail list/inbox is unavailable, but Beads/git evidence is otherwise usable. | Use `scripts/swarm-tick.sh --agent-mail-fallback frankenterm`; cite fallback state before any claim. |
| `dirty_overlap` | `git status --short` shows tracked paths that overlap the candidate's likely edit surface or another owner lane. | Stop before edits/staging; request handoff or split the work. |
| `claimable` | Candidate appears in `br ready --json`, `br show` has `status=open` and no assignee, dependencies are clear, dirty paths do not overlap, and external queues are not blocking. | Reserve owned paths, then claim with `br update <id> --claim --actor <agent> --json`. |

These examples do not prove source behavior. They only decide whether a lane is
safe to claim. Source, test, package, or CI claims still need their own proof
lane evidence.

## State Guide

| State | What it means | Safe next action | Do not do |
| --- | --- | --- | --- |
| `actionable` | The cited Beads, git, and substrate evidence shows the lane can be worked now. | Confirm dependencies and dirty paths, then claim the bead with `br update <id> --claim --actor <agent> --json`. | Do not skip the ownership/dirty-path check. |
| `waiting_external` | A queue, API, package, worker, or artifact outside the repo is blocking a verdict. | Recheck read-only status or add a Beads comment with the cited blocker. | Do not restart services, cancel CI, rerun package jobs, or call the lane passed. |
| `waiting_owner` | A live owner or fresh in-progress bead controls the lane. | Wait, ask for handoff, or pick another ready bead. | Do not reopen, reclaim, edit, or stage the owner's paths. |
| `stale_possible` | The bead may be stale, but the radar cannot prove the lane is free. | Comment with the stale evidence and request confirmation. | Do not take over unless Beads/git evidence later proves the owner is gone or the user directs it. |
| `dirty_overlap` | Dirty tracked files overlap the intended work or owner scope. | Stop before editing or staging; identify the owner and request handoff. | Do not stage broad paths, format the package, or mix unrelated changes into proof. |
| `rch_substrate_blocked` | RCH failed before a trustworthy Cargo/test/source verdict was reached. | File or update a blocker with the RCH log, worker, target dir, and reason code. | Do not count local Cargo or RCH sync chatter as proof. |
| `ci_queued` | GitHub Actions has queued jobs or pending checks. | Inspect the current-head run/check suite read-only and wait. | Do not infer pass/fail or cancel/rerun CI without operator approval. |
| `ci_zero_jobs` | A check suite exists but jobs have not materialized. | Record the zero-job state and recheck later. | Do not call the suite passed or failed. |
| `artifact_missing` | A required package, proof, or release artifact is absent. | Inspect artifact metadata/logs and block downstream closeout. | Do not fabricate an artifact path or rerun publishing from this lane. |
| `mail_unavailable` | Agent Mail is unavailable or degraded. | Use Beads/git fallback and cite the fallback snapshot. | Do not repair, restart, stop, or kill Agent Mail. |
| `degraded` | One or more sources timed out, failed, or returned partial data. | Fail closed and gather better read-only evidence. | Do not claim safety from a partial snapshot. |
| `unknown` | The radar cannot classify the lane. | Re-read Beads/git/substrate state or ask for direction. | Do not claim that the lane is safe. |

## Recurrent Cases

### Passing docs or conformance lane

Use `actionable` only when the ownership and dirty-path checks agree and the
retained proof artifact exists. For blocker-radar conformance closeout, cite
the fixture path, the Rust conformance test, the RCH wrapper, and the retained
summary artifact. The runbook text itself is not proof that the conformance
matrix passed.

Closeout evidence shape:

- command: the exact wrapper or RCH command,
- worker or execution mode,
- target dir,
- test result,
- retained artifact paths,
- residual risk or unrelated dirty-tree note.

### Missing macOS RCH package artifact

Radar state: `artifact_missing`.

Fixture anchor: `artifact-missing`.

The missing package artifact blocks rollout or packaging claims. Inspect the
artifact metadata, run logs, and expected artifact name. Add a Beads comment
with the missing artifact id/path and do not unblock dependent packaging beads
until a later proof run publishes or retrieves the artifact.

### GitHub Actions queued with zero jobs

Radar states: `ci_queued` or `ci_zero_jobs`.

Fixture anchors: `ci-queued`, `ci-zero-jobs`.

Queued checks and zero-job suites are external scheduling states. They are not
source failures and not passes. Recheck the current-head run or check suite
read-only, then comment with the run id, check suite id, observed status, and
next recheck time.

### Agent Mail fallback mode

Radar state: `mail_unavailable`.

Fixture anchor: `mail-unavailable`.

Use `scripts/swarm-tick.sh --agent-mail-fallback frankenterm`, Beads comments,
and git status as the handoff surface. Do not run Agent Mail repair commands or
kill shared mail processes. A fallback snapshot can prove the coordination
posture, but it cannot prove Cargo, tests, package artifacts, or CI verdicts.

### RCH queue timeout or local fallback refused

Radar state: `rch_substrate_blocked`.

Fixture anchors: `rch-substrate-blocked`, `rch-local-fallback-refused`.

Classify the run as a blocked verifier unless retained logs prove remote Cargo,
rustc, and the test binary reached the required stage. If local fallback was
refused by policy, keep the source bead open or blocked. A local heavy Cargo
run is not remote RCH proof unless the user explicitly approved the fallback
and the closeout labels it as degraded evidence.

### Dirty-tree overlap with active owner

Radar states: `dirty_overlap` and often `waiting_owner`.

Fixture anchors: `dirty-overlap`, `active-owner`.

Stop before editing or staging. Identify the dirty paths, active bead, owner,
and reservation if known. Ask for handoff or choose another ready bead. If your
own changes are already mixed with unrelated dirty paths, stage only owned
paths and state the unrelated paths in the Beads comment and final closeout.

### Stale owner is possible but not proven

Radar state: `stale_possible`.

Fixture anchor: `stale-possible`.

Comment with the last activity timestamp, owner, affected paths, and missing
evidence. Do not reopen or claim the bead until a later read-only check proves
the owner is inactive, the user directs a takeover, or Beads state is updated
to make the lane available.

## Proof Boundaries

Acceptable proof:

- Static docs proof for docs-only changes: markdown grep checks, shell syntax
  checks, JSON parsing, and `git diff --check` on owned files.
- Remote RCH proof for source behavior: exact command, selected worker, target
  dir, remote Cargo/rustc/test reachability when required, terminal result, and
  retained proof artifacts.
- Proof-ledger closeout: `proof-ledger.jsonl`, aggregate report when emitted,
  command log, metadata sidecars, and a Beads comment that cites those paths.
- Proof-doctor verdicts: status, blockers, reason codes, dirty-tree evidence,
  and ledger projection from the proof-doctor surface.

Not proof:

- RCH source sync, worker selection, queue placement, transfer logs, detached
  process setup, or artifact retrieval by itself.
- A command that was printed by a wrapper but never reached remote Cargo/test.
- Local heavy Cargo when RCH is required and no user-approved fallback exists.
- GitHub Actions queued or zero-job state.
- Agent Mail fallback or Beads sync output as evidence that tests passed.
- A hand-written taxonomy claim without fixture, test, or artifact citation.

## Forbidden Actions

The blocker radar must never recommend these actions, and an agent must not run
them during normal blocker-radar triage:

| Forbidden action | Safe alternative |
| --- | --- |
| `am service restart`, `am service stop`, `am doctor fix`, `am doctor repair`, `am doctor reconstruct`, or killing Agent Mail processes | Retry once, then use `scripts/swarm-tick.sh --agent-mail-fallback frankenterm`. |
| `rch daemon restart`, worker drain, worker update, or shared service mutation | Use read-only `rch status`/logs, or file/update a blocker with retained evidence. |
| GitHub Actions cancel/rerun or package republish from a docs/proof triage lane | Inspect the current-head run and comment with exact blocker evidence. |
| `git reset --hard`, `git clean -fd`, `rm -rf`, broad checkout, force push, or deleting files | Use `git status`, `git diff`, narrow staging, and ask the user before any irreversible action. |
| Staging another owner path or broad package formatting over dirty overlap | Reserve/claim the owned slice, stage only owned paths, and request handoff for overlap. |
| Fabricating artifact paths, proof-ledger entries, or terminal pass claims | Leave the bead open/blocked and cite the missing evidence. |

## Beads Comment Templates

Use these as starting points. Replace every bracketed value with current
evidence and keep the source bead's own closeout evidence separate.

### Actionable claim

```text
Blocker radar: actionable for [bead]. Evidence: [br show/ready citation],
dirty-path check [clean or owned paths], dependencies [state]. Claiming as
[agent] for [narrow scope]. Proof expectations: [static-only or RCH command].
```

### External blocker

```text
Blocker radar: waiting_external on [substrate]. State [ci_queued/ci_zero_jobs/
artifact_missing/rch_substrate_blocked]; reason [reason_code]. Evidence:
[run/check/artifact/worker/log path]. Safe next action: [read-only recheck or
follow-up bead]. No pass/fail or source-behavior claim is made.
```

### Owner handoff

```text
Blocker radar: waiting_owner for [bead/path]. Current owner [agent], last
activity [timestamp], owned paths [paths]. I am not editing or staging these
paths. Requesting handoff or confirmation before takeover.
```

### Dirty overlap

```text
Blocker radar: dirty_overlap on [paths]. Overlap source [owner/bead/status].
Stopping before edits/staging. Proposed safe split: I keep [owned paths] and
leave [unowned paths] untouched until handoff.
```

### Stale possible

```text
Blocker radar: stale_possible for [bead]. Last evidence [timestamp/source];
missing evidence [mail/git/beads detail]. I am not reopening or claiming yet.
Next safe action: [read-only recheck/comment/request direction].
```

### Agent Mail fallback

```text
Blocker radar: mail_unavailable. Agent Mail [error/degraded state]; using
scripts/swarm-tick.sh --agent-mail-fallback frankenterm plus Beads/git as the
handoff surface. No Agent Mail repair/restart attempted.
```

### RCH blocked verifier

```text
Blocker radar: rch_substrate_blocked for [bead]. Command [argv]; worker
[worker or unknown]; target dir [target]; retained log/artifact [path].
Remote Cargo/rustc/test reached: [yes/no/unknown]. Result is a blocked
verifier, not proof of source behavior.
```

### Artifact missing

```text
Blocker radar: artifact_missing for [artifact id/name]. Expected source
[run/package/proof lane]; observed evidence [metadata/log path]. Downstream
closeout remains blocked until the artifact exists and is cited.
```

## Closeout Checklist

Before closing a blocker-radar bead:

1. Cite the deterministic fixture or live artifact that backs the classification.
2. Distinguish docs/static proof from source-behavior proof.
3. For RCH proof, include the command, worker, target dir, terminal result, and
   retained artifact paths.
4. State unrelated dirty paths separately and leave them unstaged.
5. Keep handoff comments separate from source bead closeout evidence.
6. If any source is degraded or unknown, close only docs/runbook work that does
   not depend on that source; leave behavior/proof claims open or blocked.
