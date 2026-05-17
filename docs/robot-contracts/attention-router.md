# Attention Router Contract

Tracking bead: `ft-x3nsb.1`

Status: planned contract. This document defines the target behavior for a
future side-effect-free attention-router surface. It does not claim a shipped
CLI, Robot Mode, or MCP implementation.

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
  "priority": 2,
  "confidence": 0.92,
  "evidence": [
    {
      "source": "beads",
      "fact": "br_ready",
      "detail": "issue has no blocking dependencies"
    }
  ],
  "recommended_action": {
    "action": "claim_or_continue",
    "command_hint": "br update ft-x3nsb.1 --status=in_progress --assignee <agent>",
    "mutates": false
  }
}
```

`recommended_action.mutates` describes whether the attention-router surface
itself mutates state. For this contract, all router commands are read-only, so
it is always `false`. Command hints are advisory text for a human or agent to
review and run separately.

## Snapshot Envelope

Robot/CLI/MCP implementations should preserve this logical envelope.

```json
{
  "schema": "ft.attention_router.snapshot.v1",
  "contract_id": "ft.attention_router.v1",
  "generated_at_ms": 1778980000000,
  "workspace": "/Users/jemanuel/projects/frankenterm",
  "sources": {
    "beads": { "health": "available", "items_seen": 128 },
    "agent_mail": { "health": "degraded", "reason": "fallback_available" },
    "git": { "health": "available", "dirty_paths": 3 },
    "rch": { "health": "degraded", "reason": "no_admissible_workers" },
    "pane_state": { "health": "not_configured" },
    "operating_envelope": { "health": "degraded", "reason": "rch.no_admissible_workers" }
  },
  "items": [],
  "next_action": {
    "item_id": "attention:ft-x3nsb.1",
    "summary": "Continue docs-only contract work while RCH proof lanes remain blocked",
    "mutates": false
  },
  "warnings": [
    "local Cargo output is not closeout proof",
    "bv recommendation points at blocked ft-4tp7g; br state controls actionability"
  ]
}
```

TOON output should carry the same fields and enum values. Field order should be
stable enough for golden tests, but callers must parse by key, not by display
position. A representative TOON sketch:

```toon
schema: ft.attention_router.snapshot.v1
contract_id: ft.attention_router.v1
generated_at_ms: 1778980000000
workspace: /Users/jemanuel/projects/frankenterm
sources:
  beads:
    health: available
    items_seen: 128
  agent_mail:
    health: degraded
    reason: fallback_available
  git:
    health: available
    dirty_paths: 3
  rch:
    health: degraded
    reason: no_admissible_workers
  pane_state:
    health: not_configured
  operating_envelope:
    health: degraded
    reason: rch.no_admissible_workers
items[0]:
next_action:
  item_id: attention:ft-x3nsb.1
  summary: Continue docs-only contract work while RCH proof lanes remain blocked
  mutates: false
warnings[2]:
  - local Cargo output is not closeout proof
  - bv recommendation points at blocked ft-4tp7g; br state controls actionability
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

## Proof Plan

The first implementation pass should add golden fixtures for:

1. Empty `br ready` with `bv` recommending blocked `ft-4tp7g`.
2. Agent Mail degraded/unavailable fallback.
3. An `ack_required` message before more work.
4. Dirty path overlap with another owner.
5. RCH `no_admissible_workers` refusing remote-required proof.
6. A docs-only ready slice that can move while implementation proof is blocked.

Any Rust code, generated JSON/TOON golden tests, or docs tests that compile
code must run through RCH only. Static markdown and JSON checks are sufficient
for this planned-contract slice.
