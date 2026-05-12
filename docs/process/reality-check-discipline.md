# Reality-Check Discipline

Bead: `ft-tf6g3.16`

Reality-check is a steering mechanism, not a one-off audit. The weekly
drumbeat (`scripts/reality-check-status.sh`) reports progress on existing
reality-check beads; this discipline decides when to run the full
`/reality-check-for-project` pass again.

## Cadence

- Run a full reality-check at least quarterly.
- Run sooner when any trigger below fires.
- A weekly drumbeat report is not a substitute for a full reality-check.

## Trigger Conditions

A full reality-check is due when any condition is true:

| Trigger | Rule | Rationale |
|---|---|---|
| Calendar | At least 90 days since the latest reality-check plan date. | Quarterly minimum keeps strategic drift bounded. |
| Milestone | The minor version in `Cargo.toml` changed since the latest tracked reality-check plan commit. | A minor release can change the support surface enough to invalidate prior conclusions. |
| Bead pressure | `bv --robot-triage` reports at least 50 open beads. | Large open-work growth means the project shape changed. |
| Contract churn | Contract docs changed by at least 50 added/deleted lines since the latest tracked reality-check plan commit. | Operator contracts define the trust boundary; large changes need a fresh cross-check. |
| Headline claims | The README headline-claim estimate grew by at least 3 since the latest tracked reality-check plan commit. | New marketing claims need proof-artifact coverage. |

Run the mechanical check with:

```bash
scripts/check-reality-check-due.sh
scripts/check-reality-check-due.sh --json
```

The script is advisory by default. It prints `warning:` lines when a trigger
fires, but exits zero so cron and humans can collect the signal without breaking
unrelated jobs. Use `--strict` when a CI lane should fail on a due check.

## Standing Checklist

Each full reality-check run must preserve these phases:

1. Inventory: read `AGENTS.md`, `README.md`, the previous bridge plan, the
   current Beads graph, and live verification artifacts.
2. Reality assessment: separate shipped proof, planned work, stale claims,
   blocked proof lanes, and infrastructure breakage.
3. Gap extraction: create or refresh beads for every uncovered claim, with
   concrete acceptance criteria and proof surfaces.
4. Ambition critique: raise weak claims into stronger proof artifacts where the
   product story depends on them.
5. Refinement passes: add test companions, operator verbs, degradation behavior,
   and substrate beads needed by multiple leaf gaps.
6. Graph audit: add dependencies, check for cycles, identify blockers, and record
   what is actionable now versus blocked.
7. Publication: write the date-stamped bridge plan, link predecessor plans, and
   record the exact Beads/BV evidence used.

## Output Contract

Every full reality-check produces a new historical plan:

```text
docs/reality-check-bridge-plan-YYYY-MM-DD.md
```

Do not overwrite an older plan. The historical record is part of the discipline.
Within a single date-stamped plan, revise in place as the run goes through
ambition and refinement passes.

Every new plan must include:

- Source date and invocation.
- Predecessor plan links.
- Bead epic id and child range.
- Gap table with proof category or substrate/process classification.
- Trigger evidence explaining why the run happened.
- Current Beads/BV counts and dependency-cycle result.
- A successor note telling future runs to cross-link this plan.

The 2026-04-30 bridge plan is the first instance:
`docs/reality-check-bridge-plan.md`.

The `ft-tf6g3` epic is the second instance. Future reality-check plans must
reference both the 2026-04-30 plan and the `ft-tf6g3` successor evidence.

## README Reference

The README footer's Reference Card links this discipline and the due-check
script. Keep that link with the operator-facing install, doctor, attestation,
and drumbeat references so a new agent can find the reality-check cadence
without reading the whole process directory.
