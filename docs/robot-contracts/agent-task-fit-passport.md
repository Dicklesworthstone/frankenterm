# Agent Task-Fit Passport Contract

**Bead:** `ft-auy2g.9`
**Status:** static fixture contract only. No automatic task reassignment,
ranking of humans, pane mutation, Beads mutation, Agent Mail mutation, or live
mission-planner wiring is implemented by this document.

## Purpose

Large FrankenTerm swarms need a better assignment signal than "which pane is
idle." The `ft.agent_task_fit_passport.v1` contract defines a read-only,
privacy-bounded artifact for explaining whether a specific agent is currently a
safe fit for a task. It separates capability, availability, attention, and
reliability so a strong agent can still be blocked by RCH, active ownership,
stale evidence, or privacy constraints.

The JSON Schema lives at
`docs/json-schema/ft-agent-task-fit-passport.json`. The reviewed fixture
manifest lives at
`fixtures/mission-planner/agent-task-fit-passport/cases.v1.json`.

## Contract Shape

Every passport artifact records:

- the target mission context: objective id, bead id, task domain, work class,
  proof requirement, and the related `ft.mission_objective_plan.v1` contract;
- a redacted agent identity from Agent Mail, Beads, Robot inventory, or a
  fixture source, explicitly marked as not a human subject;
- claimed or reserved domains with source artifacts and freshness state;
- evidence rows from Beads closeouts, RCH proof records, Agent Mail handoffs,
  pane/runtime state, and git publication status;
- separate fit dimensions for capability, availability, attention, and
  reliability;
- one advisory recommendation: `assign`, `wait_for_owner`,
  `request_fresh_evidence`, `avoid_assignment`, or
  `require_operator_approval`;
- decay and reset rules so old wins and old failures do not permanently bias
  the planner; and
- a compact `toon_projection` table for agent-to-agent explanation.

The artifact is advisory. It may explain why an agent looks like the safest
assignment target, why other agents were not chosen, which evidence is missing,
and what fallback is safest. It must not mutate Beads, Agent Mail, panes,
services, workers, or credentials.

## Required Fail-Closed Semantics

The verifier pins these cases:

| Case | Expected recommendation | Required reason |
| --- | --- | --- |
| `good-fit` | `assign` | `fit.strong_capability` |
| `poor-fit` | `avoid_assignment` | `fit.poor_capability` |
| `stale-evidence` | `request_fresh_evidence` | `fit.stale_evidence` |
| `active-owner-conflict` | `wait_for_owner` | `fit.active_owner_conflict` |
| `missing-proof` | `request_fresh_evidence` | `fit.missing_proof` |
| `recent-failed-closeout` | `avoid_assignment` | `fit.recent_failure` |
| `agent-unavailable` | `request_fresh_evidence` | `fit.agent_unavailable` |
| `operator-approval-needed` | `require_operator_approval` | `fit.requires_approval` |
| `privacy-redacted` | `request_fresh_evidence` | `fit.privacy_redacted` |

Only `good-fit` may produce `assign`. Missing, stale, contradictory,
privacy-redacted, active-conflict, unavailable, or recently failed evidence must
fail closed to a wait, avoid, request-fresh-evidence, or approval action.

## Safety Invariants

1. `dry_run` and `read_only` are always `true`.
2. `agent_identity.human_subject` is always `false`; this is not a human
   performance scoreboard.
3. Evidence rows never store raw pane content, mail bodies, or secret material.
4. `auto_reassignment` is forbidden. Any future reassignment behavior requires a
   separate approval-gated bead and execution contract.
5. `assign` requires strong capability, available attention, reliable recent
   proof, fresh evidence, git publication evidence, and redacted summaries.

The exact forbidden action IDs are `human_performance_scoreboard`,
`raw_pane_text_storage`, `mail_body_storage`, `secret_material_storage`,
`auto_reassignment`, `beads_mutation`, `agent_mail_mutation`,
`pane_mutation`, `service_mutation`, and `local_cargo_proof`.

## Verification

Run the static verifier:

```bash
bash tests/e2e/test_agent_task_fit_passport_contract.sh
```

The verifier checks schema metadata, fixture coverage, recommendation and
reason-code coverage, fail-closed assignment semantics, decay/reset rules,
forbidden actions, TOON-ready projections, and secret-shaped strings. It uses
only shell, `jq`, `rg`, and Ruby.

Any future Rust implementation or compiled planner proof must run through RCH.
Local Cargo output is not accepted as proof for this surface.

## Negative Fixtures

The retained negative fragment corpus lives at
`fixtures/mission-planner/agent-task-fit-passport/invalid/fragments.v1.json`.
It is parseable JSON that the static verifier must reject by contract shape
rather than by syntax. The required cases are:

- `human-subject-true`
- `raw-pane-content-stored`
- `mail-body-stored`
- `auto-reassignment-permitted`
- `assign-with-stale-evidence`
- `toon-row-width-mismatch`

These fragments prove that the passport contract stays fail-closed for human
subject scoring, raw pane or mail-body retention, automatic reassignment,
assignment from stale evidence, and malformed TOON projections.
