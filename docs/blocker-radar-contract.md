# Blocker Radar Contract

Status: v1 contract. The DTO normalization and first read-only robot/doctor
surfaces are implemented under `ft-9ntud.2` / `ft-9ntud.3`; deterministic
golden/e2e proof is pinned by `ft-9ntud.4`.

This document defines the first operator-facing contract for a read-only
blocker radar: a deterministic summary of whether a FrankenTerm swarm lane is
actionable, waiting on another owner, or blocked by an external substrate such
as RCH, GitHub Actions, Agent Mail, Beads state, or dirty-tree overlap.

The JSON schema lives at `docs/json-schema/ft-blocker-radar.json`.
Operator interpretation and Beads handoff examples live in
`docs/blocker-radar-runbook.md`.

## Existing Anchors

The blocker radar is grounded in surfaces that agents already use by hand:

| Surface | Existing evidence | Contract use |
| --- | --- | --- |
| `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` | Agent Mail availability, active assignees, ready beads, dirty paths | Coordination and stale-owner posture without repairing Agent Mail. |
| `br ready`, `br show`, `br list`, `br dep cycles --json` | Beads status, dependencies, assignees, comments, cycle checks | Actionability, owner, and dependency citations. |
| `bv --robot-triage` | Ranked work graph and blocked/ready recommendations | Prioritization hint, never the only source of truth. |
| `git status --short --branch` | Dirty tracked paths and branch state | Dirty-overlap and staging-risk rows. |
| `rch status`, `rch diagnose`, retained RCH metadata | Queue state, selected worker, local-fallback refusal, Cargo/rustc/test reachability | Proof-substrate blocker classification. |
| `gh run view`, `gh run download`, PR/check-suite views | Queued CI, zero-job suites, missing artifacts, current-head status | External CI and package-artifact blocker classification. |

This contract does not replace proof-doctor verdicts, proof-ledger records,
incident bundles, resource-pressure cockpit output, context-horizon prediction,
or terminal-conformance ledgers. It tells an agent whether those lanes are
currently actionable and which evidence justifies that answer.

## Output Surfaces

Required implementation surfaces:

| Command or API | Required posture |
| --- | --- |
| `ft robot blocker-radar` | Emits the full v1 JSON envelope from read-only coordination fallback evidence. |
| `ft robot --format toon blocker-radar` | Preserves blocker rows, source states, citations, and next-action ids. |
| `ft doctor --json` | Embeds the blocker-radar summary under `blocker_radar` when diagnostics run. |
| Doctor/plain output | Shows compact operator rows without implying that any repair or proof command executed. |
| MCP read resource | Deferred for v1 until the robot/doctor surface has golden and e2e proof; any future MCP resource must return the same contract and remain read-only. |

## Versioned Envelope

The contract id is `ft.blocker_radar.v1`. The root object must carry:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version. Version 1 is this contract. |
| `contract_id` | Stable string, currently `ft.blocker_radar.v1`. |
| `generated_at_ms` | Unix epoch milliseconds for this radar snapshot. |
| `source` | Producer path, for example `robot.blocker_radar`. |
| `overall_state` | Root synthesis of actionability and blocker state. |
| `sources` | Per-substrate source snapshots with freshness and provenance. |
| `blockers` | Concrete blocker rows with citations and next actions. |
| `active_agents` | Active assignees and their current beads, when known. |
| `dirty_overlap` | Dirty tracked paths that may collide with an intended lane. |
| `external_queues` | RCH, CI, package, or artifact queues relevant to the blocker set. |
| `next_actions` | Non-mutating, safe actions an agent can take next. |
| `forbidden_actions` | Commands or action families the radar must never recommend. |
| `citations` | Redacted evidence references only; never raw pane text. |
| `unavailable_sources` | Sources that could not be collected and how that affected confidence. |
| `redaction_policy` | Privacy posture and raw-content prohibition. |
| `artifact_paths` | Retained artifacts needed to audit fixtures or proof runs. |

## Evidence States

The blocker radar must not collapse all blockers into a single vague state.

| State | Meaning | Allowed operator use |
| --- | --- | --- |
| `actionable` | Evidence shows a lane can be worked safely now. | May support claiming a bead when ownership and dirty paths agree. |
| `waiting_external` | Work is blocked by an external queue, API, package, or artifact. | Wait or recheck read-only status; do not mutate the substrate. |
| `waiting_owner` | A live owner or fresh in-progress bead controls the lane. | Do not reopen or take over without a handoff. |
| `stale_possible` | A bead may be stale, but ownership or dirty overlap is not proven clear. | Comment or gather more evidence before reopening. |
| `dirty_overlap` | Dirty tracked files overlap the intended work or ownership is unclear. | Avoid staging/touching those paths until ownership is clear. |
| `rch_substrate_blocked` | RCH failed before a trustworthy source verdict could be reached. | Do not count setup/sync chatter as proof. |
| `ci_queued` | GitHub Actions or similar CI is queued with jobs waiting. | Wait or inspect current-head status; do not infer pass/fail. |
| `ci_zero_jobs` | A check suite exists but has not materialized jobs. | Treat as external scheduling/materialization blocker. |
| `artifact_missing` | A required package or proof artifact is absent. | Do not unblock downstream rollout/proof beads. |
| `mail_unavailable` | Agent Mail is unavailable or degraded. | Use Beads/git fallback; do not repair or restart the service. |
| `degraded` | One or more sources failed, timed out, or returned partial data. | Surface the degradation and fail closed. |
| `unknown` | The radar cannot classify the lane from available evidence. | Do not claim safety or proof. |

Rows may combine states through multiple blockers, but each blocker row must
retain its source-specific state and reason codes.

## Claimability Reconciliation

`ft-htcwc.1` extends the blocker-radar contract with a pre-claim
claimability reconciliation vocabulary. This is stricter than the root
`actionable` state: a high graph score or unblock count is never enough to
claim a bead. The claimability check compares the ready queue, the individual
Beads record, BV's ranked recommendation, degraded-mail fallback state,
dirty-tree risk, and optional GitHub/RCH evidence before it returns a final
answer.

The claimability check must treat `bv --robot-triage` and `bv --robot-next` as
advisory prioritization only. `br ready --json` and `br show <id> --json` are
authoritative for Beads status, dependencies, and assignee state. If BV says a
blocked or assigned bead is "available for work" while BR says the bead is not
ready, the final verdict is `tracker_inconsistent` and non-claimable.

The observed regression fixture for this contract is `ft-e87u6.2`: BV reported
the bead as status `blocked` while also giving the reason "Currently
unclaimed - available for work"; BR showed status `blocked`, assignee
`BluePike`, and fresh PR 59 CI-wait comments. That state must resolve to
`tracker_inconsistent` plus `owner_blocked` or `external_wait`, never
`claimable`.

Required claimability verdicts:

| Verdict | Meaning | Safe next action |
| --- | --- | --- |
| `claimable` | The bead is in `br ready --json`, dependencies are satisfied, ownership is clear, dirty paths do not overlap, and no fresh external wait blocks the lane. | Reserve the owned paths, then claim with `br update <id> --claim --actor <agent> --json`. |
| `no_ready` | The ready queue is empty and no candidate can be made safe from the advisory BV list. | Use idea-wizard or create/refine planning beads; do not force-claim blocked work. |
| `dependency_blocked` | A Beads dependency or parent blocker prevents the candidate from being ready. | Work the dependency if it is claimable, otherwise wait or create a precise blocker. |
| `owner_blocked` | Another assignee, fresh activity, reservation, or handoff evidence controls the lane. | Wait, ask for handoff, or choose another bead. |
| `external_wait` | CI, RCH, package, Agent Mail, or another substrate is queued, pending, or missing evidence without a source failure to fix. | Recheck read-only status later or comment with exact external evidence. |
| `dirty_overlap` | Local dirty tracked paths overlap the candidate's likely edit surface. | Stop before editing or staging and request handoff/split. |
| `mail_degraded` | Agent Mail list/inbox is unavailable, so Beads/git fallback is the handoff surface. | Continue with Beads/git evidence only; do not repair or restart Agent Mail. |
| `tracker_inconsistent` | BV, BR, fallback state, or live substrate evidence disagree in a way that could mislead an agent into an unsafe claim. | Fail closed, cite the conflicting sources, and do not claim until the authoritative source changes. |

Required claimability output fields are `candidate_id`, `generated_at`,
`sources`, `ready_queue_verdict`, `dependency_verdict`, `owner_verdict`,
`dirty_path_verdict`, `external_wait_verdict`, `mail_verdict`,
`tracker_consistency_verdict`, `final_verdict`, `reason_codes`,
`next_action`, and `forbidden_actions`.

Precedence is fail-closed:

1. `tracker_inconsistent`, `owner_blocked`, `dependency_blocked`,
   `dirty_overlap`, and `external_wait` override BV score, priority, PageRank,
   and unblock count.
2. `mail_degraded` requires Beads/git fallback citations before any claim, and
   it can only support `claimable` when BR, ownership, dirty paths, and
   dependencies all agree.
3. `claimable` is only valid when every safety predicate passes. Missing source
   data produces `unknown`, `degraded`, or `tracker_inconsistent`, not a claim.

Forbidden actions for claimability reconciliation include claiming or assigning
a bead automatically, posting comments automatically, restarting/repairing
Agent Mail or RCH, cancelling/rerunning CI, mutating panes, staging dirty
overlap, or treating sync chatter as proof.

## Source Snapshots

Each `sources` row describes one read-only observation:

| Field | Meaning |
| --- | --- |
| `source_id` | Stable id within this radar snapshot. |
| `source_kind` | `rch`, `github_actions`, `agent_mail`, `beads`, `git`, `manual`, or `fixture`. |
| `evidence_state` | One of the blocker-radar states above. |
| `collected_at_ms` | Collection timestamp when live evidence exists. |
| `freshness_ms` | Age budget or age of the observed source. |
| `command_or_api` | Bounded command or API name, redacted where needed. |
| `live` | Whether the evidence came from the live workspace. |
| `redacted` | Must be `true` for retained command/API snippets. |
| `reason_codes` | Stable explanations such as `queue_timeout`, `active_owner`, or `artifact_absent`. |
| `artifact_paths` | Retained metadata paths, if any. |

Unavailable sources must appear in `unavailable_sources` rather than silently
dropping from the output.

## Blocker Rows

Each `blockers` entry describes one reason the lane is or is not actionable:

| Field | Meaning |
| --- | --- |
| `blocker_id` | Stable id within the snapshot. |
| `evidence_state` | Classification for this blocker. |
| `severity` | `info`, `warning`, `blocked`, or `critical`. |
| `summary` | Concise human summary. |
| `source_ids` | Source snapshots that justify the row. |
| `citation_ids` | Redacted citations for review. |
| `dependency_ids` | Bead ids, run ids, job ids, worker ids, or artifact ids. |
| `next_action_ids` | Safe next actions tied to this blocker. |

Examples:

- RCH package rollout blocked because `dist-macos-aarch64` is missing.
- GitHub Actions suite is queued or has zero jobs materialized.
- Agent Mail is unavailable, so Beads-only coordination is active.
- A bead is inside the stale threshold and should not be reopened.
- Dirty tracked files overlap another agent's active terminal-conformance lane.

## Next Actions

Recommendations are read-only advice, not executed actions.

Required fields:

| Field | Meaning |
| --- | --- |
| `action_id` | Stable id within the snapshot. |
| `action_kind` | `recheck_status`, `inspect_artifact`, `add_beads_comment`, `wait_for_owner`, `choose_ready_bead`, `run_bv_robot_triage`, `run_swarm_tick`, `file_followup_bead`, or `none`. |
| `mutation_allowed` | Must be `false` for every v1 action. |
| `operator_summary` | Concise reason for the action. |
| `suggested_command` | Optional read-only command. |
| `reason_codes` | Stable reasons. |
| `citation_ids` | Evidence references. |

Suggested commands must be safe read-only commands or no-op planning commands.
They must not restart services, repair Agent Mail, update RCH, cancel CI runs,
push commits, delete files, reset git state, or mutate panes.

## Forbidden Actions

The root `forbidden_actions` list must include action families relevant to the
current repo rules. At minimum:

| Pattern | Reason |
| --- | --- |
| `am service restart` | Shared Agent Mail singleton must not be touched. |
| `am doctor fix` | Agent Mail repair is forbidden during normal agent work. |
| `kill am` | Killing shared mail processes disrupts other agents. |
| `rch daemon restart` | RCH rollout belongs to an explicit drain/update bead. |
| `git reset --hard` | Destructive git commands require explicit user approval. |
| `git clean -fd` | Destructive filesystem cleanup requires explicit user approval. |

Implementations may include additional forbidden commands from `AGENTS.md`.

## Privacy Invariants

The blocker radar must never store or emit raw private content:

- no raw pane transcript,
- no prompt body,
- no session cookies, API keys, or bearer tokens,
- no unbounded command output,
- no hidden mutation through a recommended command.

The root field `raw_pane_content_stored` must be `false`. Citations may use
bounded command names, run ids, job ids, bead ids, worker ids, hashes, and
redacted labels. Live command output must be bounded and redacted before it is
retained.

## Failure Classification

Contract and proof artifacts should classify failures as:

| Class | Meaning |
| --- | --- |
| `source_regression` | The implementation violates the schema or expected behavior. |
| `privacy_violation` | Raw or secret-like content escaped into output or fixtures. |
| `environment_blocked` | RCH, CI, mail, filesystem, or platform dependencies prevented proof. |
| `unavailable_evidence` | The radar ran but required evidence was missing. |
| `external_queue_blocked` | A queue or check-suite state prevents a verdict. |
| `dirty_tree_blocked` | Local tracked changes make ownership unsafe. |
| `owner_handoff_required` | Another active owner controls the lane. |
| `target_hardware_skipped` | High-scale 64 CPU / 256 GiB claims were not proven. |

## Conformance Matrix

`ft-9ntud.4` pins deterministic semantic goldens in
`crates/frankenterm-core/tests/fixtures/blocker_radar/conformance_cases.json`
and validates them with
`crates/frankenterm-core/tests/blocker_radar_conformance.rs`. The fixture matrix
must stay small, scrubbed, and reviewable; expected outputs are the required
state vectors, action kinds, citations, failure classes, dirty paths, and
artifact paths for each scenario.

MUST-level state coverage:

| Evidence state | Fixture case(s) |
| --- | --- |
| `actionable` | `normal-actionable` |
| `waiting_external` | `waiting-external-generic` |
| `waiting_owner` | `active-owner` |
| `stale_possible` | `stale-possible` |
| `dirty_overlap` | `dirty-overlap` |
| `rch_substrate_blocked` | `rch-substrate-blocked`, `rch-local-fallback-refused`, `all-external-blocked` |
| `ci_queued` | `ci-queued`, `all-external-blocked` |
| `ci_zero_jobs` | `ci-zero-jobs` |
| `artifact_missing` | `artifact-missing`, `all-external-blocked` |
| `mail_unavailable` | `mail-unavailable` |
| `degraded` | `mixed-degraded` |
| `unknown` | `mixed-degraded` |

The fixture test validates every generated report against
`docs/json-schema/ft-blocker-radar.json` and fails on missing citations,
state collapse, nondeterministic timestamps, raw secret-like fixture content,
or mutating recommendations. The mock-free wrapper
`tests/e2e/test_ft_9ntud_4_blocker_radar_conformance.sh` runs the same Rust
conformance lane through RCH and writes structured JSONL plus a summary artifact
instead of treating local Cargo as proof.

## Proof Expectations

The v1 proof stack is complete only when every layer below is present and its
current artifact is cited in the closing bead:

- deterministic schema/golden fixtures,
- docs-smoke checks that keep docs and schema names aligned,
- Robot JSON and TOON contract tests,
- privacy fixtures that fail on raw prompt, transcript, or secret leakage,
- mock-free e2e wrapper logs with exact source classifications,
- RCH-backed proof artifacts with exact commands and isolated target dirs.

This document is the v1 contract and live surface description. It is not, by
itself, proof that every live substrate collector has end-to-end coverage; use
the fixture/e2e artifacts and RCH logs for that claim.
