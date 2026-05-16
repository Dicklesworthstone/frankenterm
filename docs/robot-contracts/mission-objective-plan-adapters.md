# Mission Objective Plan Source Adapters

**Bead:** `ft-auy2g.2`
**Status:** planning contract only. No runtime adapter implementation is shipped
by this document.

## Purpose

Mission objective planning needs a normalized, redacted source layer before the
planner can rank safe next actions. The source bundle contract is
`ft.mission_objective_sources.v1`, defined in
`docs/json-schema/ft-mission-objective-sources.json`.

The source bundle is deliberately read-only. It records what was inspected, how
fresh it is, whether it was unavailable or degraded, and why. It must not repair
Agent Mail, restart RCH, mutate workers, read raw pane content, run local Cargo
as proof, or delete files.

## Adapter Kinds

| Adapter | Source kind | Required posture |
|---|---|---|
| Beads | `beads` | Ready, blocked, in-progress, stale, and active-assignee state from `br`/`bv`. |
| Agent Mail | `agent_mail` | Available inbox/contact summary, or a first-class unavailable state after the allowed retry. |
| RCH | `rch` | Queue/status/diagnose/update facts with proof-blocker reason codes. |
| Git | `git` | Dirty path summary with overlap risk and staging status. |
| Robot | `robot` | Pane/agent inventory metadata only; no raw pane text by default. |
| Blocker radar | `blocker_radar` | Existing blocker classification, when available. |
| Resource cockpit | `resource_cockpit` | Capacity/admission posture, when available. |

## Snapshot Semantics

Every source snapshot records:

- `source_id` and `source_kind`;
- `state`: `available`, `degraded`, `unavailable`, `stale`, or `blocked`;
- freshness timestamps and the command/API used;
- `evidence_level`;
- redaction and raw-pane-content booleans, both fail-closed;
- reason codes;
- unavailable/degraded explanation fields; and
- a bounded payload with counters and rows.

The payload is intentionally summary-shaped. Raw command output belongs in a
retained artifact only if it has been redacted and reviewed.

## Required Reason-Code Families

The planner depends on stable reason-code families, including:

- `beads.ready_available`, `beads.ready_empty`, `beads.in_progress_active`,
  `beads.stale_candidate`, `beads.stale_do_not_reopen`;
- `agent_mail.available`, `agent_mail.database_error`,
  `agent_mail.unavailable_after_retry`;
- `rch.no_workers_passed_health`, `rch.active_project_exclusion`,
  `rch.topology_preflight_failed`, `rch.remote_cargo_reached_false`,
  `rch.latest_release_is_installed`;
- `git.clean_for_scope`, `git.dirty_paths_present`, `git.dirty_overlap_risk`;
- `robot.inventory_available`, `robot.raw_pane_text_skipped`;
- `capacity.admit`, `capacity.defer`, `capacity.unavailable`; and
- `source.redacted_summary_only`.

## Fixtures

Fixtures live under `fixtures/mission-planner/source-adapters/`:

- `healthy.json`: Beads/git/RCH/capacity summaries available and fresh.
- `agent-mail-unavailable.json`: Agent Mail failed after retry but Beads fallback
  remains useful.
- `rch-degraded.json`: RCH status is blocked/degraded with proof-specific reason
  codes.
- `dirty-overlap.json`: Git reports dirty tracked paths that overlap another
  active proof lane.

These fixtures are adapter-contract examples, not evidence that runtime adapter
code exists.
