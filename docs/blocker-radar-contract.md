# Blocker Radar Contract

Status: v1 planning contract; implementation and RCH proof pending under
`ft-9ntud`.

This document defines the first operator-facing contract for a read-only
blocker radar: a deterministic summary of whether a FrankenTerm swarm lane is
actionable, waiting on another owner, or blocked by an external substrate such
as RCH, GitHub Actions, Agent Mail, Beads state, or dirty-tree overlap.

The JSON schema sketch lives at `docs/json-schema/ft-blocker-radar.json`.

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
| `ft robot blocker-radar` | Emits the full v1 JSON envelope. |
| `ft robot --format toon blocker-radar` | Preserves blocker rows, source states, citations, and next-action ids. |
| `ft doctor --json` | May embed a blocker-radar summary when substrate evidence is available. |
| Doctor/plain output | Shows compact operator rows without implying that any repair or proof command executed. |
| MCP/Robot read resource | Optional for v1; if implemented, it must return the same contract and remain read-only. |

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

## Proof Expectations

The contract is not complete until later beads add:

- deterministic schema/golden fixtures,
- docs-smoke checks that keep docs and schema names aligned,
- Robot JSON and TOON contract tests,
- privacy fixtures that fail on raw prompt, transcript, or secret leakage,
- mock-free e2e wrapper logs with exact source classifications,
- RCH-backed proof artifacts with exact commands and isolated target dirs.

Until then, this document is a v1 contract and planning surface, not proof that
the blocker radar is implemented.
