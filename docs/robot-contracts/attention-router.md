# Attention Router Contract

Tracking bead: `ft-x3nsb.1`

Status: live read-only surface with caller-supplied source-adapter input.
`frankenterm_core::attention_router` scores bounded source snapshots and the
same surface payload is exposed through `ft attention ...`, `ft robot attention
...`, MCP tool `wa.attention`, and MCP resources
`wa://attention-router/current` plus `wa://attention-router/items/{item_id}`.
Without explicit input the surface emits an explicit degraded no-input
snapshot; it does not collect live state or mutate project state.

Operator runbook: `docs/operator-runbook.md` section 2C.

## Purpose

Large FrankenTerm swarms need a reliable answer to one operational question:
what needs attention now?

The attention router is a read-only planning surface that ranks Beads, Agent
Mail, git, RCH, and optional pane signals into a single attention snapshot. It
is not an execution engine. It recommends safe next actions, but it does not
claim work, send mail, release reservations, reopen Beads, cancel builds, clean
files, restart services, or run proof commands.

## Overlap Audit

This contract deliberately does not replace existing planning and proof lanes:

| Existing lane | Boundary |
|---|---|
| `ft-booek` operating envelope | Capacity admission remains owned there. The attention router may consume envelope/RCH health as a source signal. |
| `ft-auy2g` mission objective planner | Mission planning and execution remain owned there. The attention router only says which work deserves attention. |
| `ft-ogr3n` flight recorder | Causal incident replay remains owned there. The attention router is live triage, not post-incident reconstruction. |
| `ft-lecbn` demo lab | Replayable scenarios remain owned there. The attention router may borrow scenario format ideas but is not a demo surface. |
| `ft-b94bx` capacity autopilot | High-core/high-RAM control remains owned there. The attention router does not admit or spawn work. |
| `ft-4tp7g` RCH substrate blocker | Heavy Rust proof remains blocked until RCH can produce retained remote Cargo artifacts. The attention router must surface that blockage honestly. |

## Source Snapshot

An attention snapshot is computed from bounded, read-only source snapshots.
Each source records health independently so missing telemetry cannot look
healthy.

| Source | Required fields |
|---|---|
| Beads | Ready, blocked, in-progress, priority, assignee, dependencies, updated time, closeout/proof comments, and `bv` recommendation conflicts. |
| Agent Mail | Registered agents, recent direct messages, `ack_required` messages, active file reservations, and unavailable/degraded fallback state. |
| Git | Branch divergence, dirty paths, staged paths, recent commits, and path overlap with claimed/reserved work. |
| RCH | Installed status, queue state, worker pressure, remote-required dry-run reason, and proof-starvation reason without service mutation. |
| Pane/robot state | Optional agent liveness and pane state, interpreted with the AGENTS.md caveat that Codex idle placeholder text is not stuck evidence. |
| Operating envelope | Capacity, side-effect policy, and target-hardware proof posture from `ft.operating_envelope.v1`. |

Source health is one of:

| State | Meaning |
|---|---|
| `available` | Snapshot collected within the source-specific freshness budget. |
| `degraded` | Snapshot exists but carries warnings, stale telemetry, partial coverage, or degraded service posture. |
| `unavailable` | Source could not be queried without forbidden repair/restart behavior. |
| `not_configured` | Source is optional for this host or build. |

## Attention Item Model

Each item is one candidate requiring attention.

```json
{
  "schema": "ft.attention_router.item.v1",
  "item_id": "attention:ft-x3nsb.1",
  "kind": "ready_work",
  "subject": {
    "bead_id": "ft-x3nsb.1",
    "title": "[idea-wizard][attention-router] Contract, overlap audit, and JSON/TOON sketch"
  },
  "classification": "ready_now",
  "priority": 3,
  "confidence": 0.92,
  "evidence": [
    {
      "source_kind": "beads",
      "source_id": "beads.ready",
      "fact": "beads_ready",
      "detail": "issue has no blocking dependencies",
      "bead_ids": ["ft-x3nsb.1"],
      "agent_names": [],
      "affected_paths": [],
      "reason_codes": ["beads.ready_available"]
    }
  ],
  "recommended_action": {
    "action": "claim_ready_static_slice_reserve_paths_and_run_static_checks",
    "summary": "Claim the ready static slice, reserve its paths, and run the required static checks.",
    "command_hint": "br show ft-x3nsb.1 --json",
    "mutates": false
  }
}
```

`recommended_action.mutates` describes whether the attention-router surface
itself mutates state. For this contract, all router commands are read-only, so
it is always `false`. Command hints are advisory text for a human or agent to
review and run separately.

Each scored item also carries `nudge_plan_receipt`, a side-effect-free planning
receipt for communication or stale-work follow-up. The receipt uses the static
inventory contract in
`fixtures/attention-router/nudge-plan-receipts.v1.json` as its vocabulary and
adds the live trigger item id:

```json
{
  "schema": "ft.attention_router.nudge_plan_receipt.v1",
  "contract_id": "ft.attention_router.nudge_plan_receipts.v1",
  "trigger_classification": "stale_claim",
  "recipient": "bead-thread:ft-x3nsb.5",
  "target": {
    "kind": "bead",
    "bead_id": "ft-x3nsb.5"
  },
  "nudge": {
    "kind": "status_check",
    "command_hint": "draft status check for ft-x3nsb.5; send only by explicit caller action before any force-release review",
    "safe_command_text": "draft status check for ft-x3nsb.5; send only by explicit caller action before any force-release review",
    "urgency": "normal",
    "mutates": false,
    "review_required": true
  },
  "escalation": {
    "status_check_before_force_release": true,
    "elapsed_time_alone_sufficient": false,
    "human_review_required_for_mutation": true
  },
  "live_mutation_allowed": false,
  "side_effects_executed": false
}
```

Live nudge kinds are `acknowledge_request`, `reply_to_thread`, `status_check`,
`handoff_request`, `force_release_review`, and `no_action`. `command_hint` and
`safe_command_text` are draft text only; the router never sends mail,
acknowledges messages, comments on Beads, releases reservations, force-releases
owners, restarts services, cancels builds, deletes files, or treats local Cargo
as proof.

## Surface Envelope

Robot/CLI/MCP implementations preserve this logical surface envelope. The
retained input-backed JSON and TOON examples for the ready status surface are:

- `fixtures/attention-router/source-adapter-input.ready.v1.json`
- `fixtures/attention-router/surface-status.golden.json`
- `fixtures/attention-router/surface-status.golden.toon`

The current JSON payload is an object with `schema =
ft.attention_router.surface.v1`, the source snapshot, selected/next item
fields, degraded-mode reason codes, and MCP resource descriptors. The snapshot
embedded inside the surface keeps the scored `ft.attention_router.snapshot.v1`
object.

```json
{
  "schema": "ft.attention_router.surface.v1",
  "contract_id": "ft.attention_router.v1",
  "surface": "status",
  "generated_at_ms": 1770000300001,
  "workspace": "/Users/jemanuel/projects/frankenterm",
  "dry_run": true,
  "live_mutation_allowed": false,
  "side_effects_executed": false,
  "degraded_mode": {
    "active": false,
    "reason_codes": []
  },
  "snapshot": {
    "schema": "ft.attention_router.snapshot.v1",
    "items": [],
    "side_effects_executed": false
  }
}
```

TOON output carries the same fields and enum values. Field order should be
stable enough for golden tests, but callers must parse by key, not by display
position. A representative TOON sketch:

```toon
schema: ft.attention_router.surface.v1
contract_id: ft.attention_router.v1
surface: status
generated_at_ms: 1770000300001
workspace: /Users/jemanuel/projects/frankenterm
dry_run: true
live_mutation_allowed: false
side_effects_executed: false
degraded_mode:
  active: false
  reason_codes[0]:
snapshot:
  schema: ft.attention_router.snapshot.v1
  side_effects_executed: false
```

## Classifications

| Classification | Meaning | Example next action |
|---|---|---|
| `ready_now` | A Bead is actionable and has no known owner or path conflict. | Claim or continue the Bead. |
| `blocked_infra` | Work is blocked by RCH, Agent Mail, or another infrastructure substrate. | Record/refresh the blocker, then pick non-proof work. |
| `blocked_domain` | Work has real product dependencies. | Work the dependency first. |
| `waiting_comm` | A direct question or `ack_required` message needs response. | Acknowledge or reply. |
| `stale_claim` | Work appears in progress without recent Beads, Mail, git, or pane evidence. | Send a status-check nudge before any force-release request. |
| `dirty_overlap` | Dirty paths overlap another active owner or reservation. | Avoid those paths and choose disjoint work. |
| `proof_starved` | Source changes exist but retained RCH proof is missing or refused. | Keep the Bead open/blocked and cite the exact RCH reason. |
| `do_not_touch` | Human-owned, active owner, destructive cleanup, service mutation, or forbidden path. | Do not mutate; ask only if the user explicitly directs it. |

## Degraded Behavior

- If Agent Mail is unavailable, report `agent_mail.health = "unavailable"` and
  continue from Beads/git/RCH/pane state. Do not repair or restart the service.
- If `br ready` is empty and `bv` recommends a blocked issue, explain the
  disagreement and treat `br` as the actionability source.
- If RCH has no admissible workers, classify proof-heavy work as
  `blocked_infra` or `proof_starved`; do not run local Cargo as proof.
- If the shared tree is dirty, include path overlap evidence. Do not stash,
  revert, overwrite, or clean another agents work.
- If no useful work is ready, recommend a docs/spec/testing-planning slice or a
  new Beads issue with explicit scope and proof expectations.

## Reservation Firewall

The router treats Agent Mail reservations, Beads ownership, dirty paths, and
publication state as a single ownership firewall. It must never decide that a
path is safe from only one source when another source still reports an active
owner or unreleased lease.

The retained fixture matrix in
`fixtures/attention-router/scenarios.v1.json` covers these firewall cases:

| Scenario | Required outcome |
|---|---|
| `active-exclusive-reservation-overlap` | Active exclusive reservations block editing, staging, claiming, and committing overlapping paths. |
| `reservation-release-message-not-released` | A publication or closeout message is not a reservation release; wait for release or expiry and recheck. |
| `ownership-source-disagreement` | Beads assignee, reservation holder, and dirty-path attribution conflicts classify as `do_not_touch`. |
| `local-closeout-publication-pending` | Local Beads closed state is not durable until commit, `origin/main`, legacy mirror, and reservation release all agree. |
| `stale-owner-status-before-force-release` | Stale-looking ownership may produce a status-check or operator-review recommendation, never an automatic force-release. |

Every firewall recommendation is side-effect-free. The router may explain the
next evidence to collect, but it does not release reservations, reopen Beads,
send handoff mail, stage files, or publish another agent's closeout.

## Safety Rules

The attention router must never perform these actions:

- `am service restart`, `am service stop`, `am doctor fix`, `am doctor repair`,
  or any process kill targeting Agent Mail.
- RCH service restart/deploy/repair, worker mutation, build cancellation, or
  remote mirror deletion.
- `git reset --hard`, `git clean`, file deletion, target cleanup, or any other
  destructive filesystem operation.
- Local Cargo proof for closeout.
- Automatic Beads reopen/force-release or Agent Mail broadcast without a future
  explicit policy-gated mutating command.

## Proof And Goldens

The implementation pass retains the live surface examples in:

- `fixtures/attention-router/source-adapter-input.ready.v1.json`
- `fixtures/attention-router/surface-status.golden.json`
- `fixtures/attention-router/surface-status.golden.toon`

The broader scenario inventory in `fixtures/attention-router/scenarios.v1.json`
continues to cover:

1. Empty `br ready` with `bv` recommending blocked `ft-4tp7g`.
2. Agent Mail degraded/unavailable fallback.
3. An `ack_required` message before more work.
4. Dirty path overlap with another owner.
5. RCH `no_admissible_workers` refusing remote-required proof.
6. A docs-only ready slice that can move while implementation proof is blocked.
7. Reservation firewall cases where leases, dirty paths, Beads ownership, and
   publication state disagree or have not all cleared.

`tests/e2e/test_attention_router_scenarios.sh` validates that inventory and
emits JSONL proof records for each retained scenario, including the expected
classification, safe action, source reason codes, explanation terms, and
volatility level.

Any Rust code, generated JSON/TOON golden tests, or docs tests that compile
code must run through RCH only. Static markdown and JSON checks may supplement
the RCH proof, but local Cargo output is not closeout proof.
