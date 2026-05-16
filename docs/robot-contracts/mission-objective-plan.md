# Robot Family Contract: `mission objective-plan`

**Bead:** `ft-auy2g.1`
**Status:** planning contract only. No CLI, Robot, or MCP surface is shipped by
this document.

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

## Safety Invariants

1. `raw_pane_content_stored` is always `false`.
2. `source_snapshots[*].redacted` is always `true`.
3. A plan step with `side_effect_class` of `pane_mutation`, `service_mutation`,
   or `destructive` must have `forbidden: true` unless a later execution
   contract explicitly overrides it.
4. `run_rch_proof` steps never imply local Cargo fallback is acceptable.
5. `service_mutation` covers Agent Mail and RCH restart/repair/drain/update
   actions and is forbidden by default.

## Golden Fixtures

The initial fixtures live under `fixtures/mission-planner/objective-plan/`:

- `ready-bead.json`: one safe Beads-ready contract slice.
- `no-ready-rch-blocked.json`: no ready work, active/staged overlap, Agent Mail
  unavailable, and RCH proof blocked.
- `invalid-raw-pane-content.json`: rejection fixture showing why raw pane content
  storage must fail schema validation.

These fixtures are schema examples, not proof that a CLI surface exists.

## Future Surface

Candidate commands for a later bead:

```text
ft mission objective-plan --objective "<text>" --format json
ft robot mission objective-plan --objective "<text>"
```

MCP should mirror Robot semantics if it is added. Human output should be a
compact projection of the same fields, not a separate truth source.
