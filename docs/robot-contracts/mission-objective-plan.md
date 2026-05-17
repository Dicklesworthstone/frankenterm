# Robot Family Contract: `mission objective-plan`

**Bead:** `ft-auy2g.4`
**Status:** dry-run CLI, Robot, and MCP surfaces are shipped. Source adapters
are still caller-supplied summaries; this surface does not collect raw pane text
or mutate Beads, panes, services, or proof infrastructure.

## Purpose

The existing `ft mission plan` command validates a mission file and computes a
mission contract hash. This contract defines a separate future surface:
`mission objective-plan`. Its job is to compile an operator objective into a
read-only, auditable plan before any pane mutation, service action, or work
assignment occurs.

The output contract is `ft.mission_objective_plan.v1`, defined in
`docs/json-schema/ft-mission-objective-plan.json`.

Source-adapter snapshots that feed this plan are defined separately in
`docs/robot-contracts/mission-objective-plan-adapters.md` and
`docs/json-schema/ft-mission-objective-sources.json`.

## Contract Shape

An objective plan records:

- the normalized objective and strictness level;
- redacted source snapshots from Beads, Agent Mail, RCH, git, Robot, blocker
  radar, and resource/capacity surfaces;
- a dry-run capacity admission decision;
- ranked plan steps with explicit side-effect class and approval requirements;
- proof requirements and retained artifact paths;
- forbidden action classes; and
- a redaction policy proving raw pane content and secret material were not
  stored.

The contract is intentionally dry-run first. A plan may recommend claiming a
bead, adding a Beads comment, or running an RCH proof, but it is not itself an
approval to do those things.

## State Semantics

`plan_status` uses reason-coded states rather than prose:

| State | Meaning |
|---|---|
| `actionable` | At least one safe next step is available. |
| `planning_only` | Plan can be refined, but implementation should not start. |
| `blocked` | Required dependency or policy gate prevents action. |
| `waiting_owner` | Another active assignee owns the relevant slice. |
| `waiting_external` | External queue, CI, RCH, or human approval is required. |
| `dirty_overlap` | Dirty paths overlap the planned work. |
| `no_ready_work` | No Beads-ready work is available; fallback planning may run. |
| `rch_substrate_blocked` | Cargo-heavy proof cannot run through RCH. |
| `degraded` | One or more sources are unavailable but the plan remains useful. |
| `unavailable` | The planner cannot produce a trustworthy plan. |

## Source Adapter Snapshots

Every source adapter ultimately contributes an embedded `source_snapshot` to the
objective plan. The richer standalone source bundle contract is
`ft.mission_objective_sources.v1`; this plan-level snapshot keeps the subset the
planner needs for ranking and explanation. It is a redacted summary of an
existing truth surface, not a transcript. It must include the source id/kind,
collection timestamp, freshness age and state, command or API provenance,
redaction posture, reason codes, and at least one structured `evidence` item.

Evidence categories make degraded planning auditable:

| Category | Use |
|---|---|
| `beads_ready_queue` / `beads_blocked_queue` / `beads_in_progress` | Beads queue shape and blocker state. |
| `active_assignee_overlap` | Whether a live assignee already owns the planned slice. |
| `agent_mail_availability` | Agent Mail health, including red/fallback state. |
| `rch_worker_selection` | Whether RCH selected a usable worker. |
| `rch_active_project_exclusion` | Whether active-project exclusion blocked the worker pool. |
| `rch_topology_preflight` | Whether topology/dependency preflight ran and passed. |
| `rch_cargo_verdict` | Whether Cargo/test proof ran remotely and what verdict class it produced. |
| `git_dirty_tree` / `dirty_path_overlap` | Dirty-tree presence and overlap with planned owned paths. |
| `capacity_pressure` | Resource-cockpit or blocker-radar capacity tier. |
| `robot_inventory` | Pane inventory only; raw pane text is not collected. |
| `redaction_posture` | Explicit proof that raw pane content and secrets were not stored. |

Agent Mail and RCH failures are data, not repair instructions. A source may be
`unavailable` or `degraded` while the plan remains actionable through Beads and
static checks. `service_mutation` remains forbidden by default.

## Safety Invariants

1. `raw_pane_content_stored` is always `false`.
2. `source_snapshots[*].redacted` is always `true`.
3. A plan step with `side_effect_class` of `pane_mutation`, `service_mutation`,
   or `destructive` must have `forbidden: true` unless a later execution
   contract explicitly overrides it.
4. `run_rch_proof` steps never imply local Cargo fallback is acceptable.
5. `service_mutation` covers Agent Mail and RCH restart/repair/drain/update
   actions and is forbidden by default.

## Operator Runbook

Operational use of objective-plan is documented in
[`docs/operator-runbook.md`](../operator-runbook.md#2b-mission-objective-planner-safety-gate).
That runbook is the source for degraded workflows: no ready Beads, Agent Mail
unavailable, RCH proof outage, dirty overlap, stale ownership, and
capacity-pressure admission. The important rule is unchanged: an objective plan
is an explanation artifact, not permission to mutate panes, claim Beads, repair
services, cancel builds, or count local Cargo as proof.

## Golden Fixtures

The initial fixtures live under `fixtures/mission-planner/objective-plan/`:

- `ready-bead.json`: one safe Beads-ready contract slice.
- `no-ready-rch-blocked.json`: no ready work, active/staged overlap, Agent Mail
  unavailable, and RCH proof blocked.
- `healthy-sources.json`: Beads, Agent Mail, RCH, git, capacity, and robot
  inventory sources are all available and redacted.
- `agent-mail-red.json`: Agent Mail unavailable with Beads-only fallback still
  actionable.
- `rch-degraded.json`: RCH proof blocked with distinct worker-selection,
  active-project-exclusion, topology-preflight, and Cargo-verdict evidence.
- `dirty-overlap.json`: active assignee and dirty path overlap force a wait
  plan instead of file edits.
- `invalid-raw-pane-content.json`: rejection fixture showing why raw pane content
  storage must fail schema validation.

These fixtures are schema examples, not proof that a CLI surface exists.

`fixtures/mission-planner/objective-plan-corpus/` adds the reviewed planner
golden corpus for `ft-auy2g.5`. Its manifest records scrub rules, retained
source commands, exit codes, fixture hashes, and expected contract fields for
clean ready queues, no-ready fallback, Agent Mail unavailable, RCH degraded,
dirty path overlap, stale in-progress work, and blocked proof lanes. The
`mission_objective_plan_golden_corpus` integration test feeds those fixtures
through the real dry-run planner, validates generated plan JSON against the
schema, and verifies deterministic TOON encoding.

## Shipped Surface

The same dry-run contract is exposed through human CLI, Robot mode, and MCP:

```text
ft mission objective-plan --objective "<text>" --format json
ft robot mission objective-plan --objective "<text>"
wa.mission_objective_plan {"objective":"<text>"}
wa://mission/objective-plan/{objective}
```

Human output is a compact projection of the same fields, not a separate truth
source. Passing `--execute` or MCP `execute=true` is rejected because
objective-plan is a dry-run planning surface only.
