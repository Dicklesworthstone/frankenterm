# ft-htcwc BV Stale Availability Follow-up

Status: external-tool defect report for `bv`; FrankenTerm-side guard exists.

## Classification

The stale availability language is in the external `bv --robot-triage` output,
not in FrankenTerm's blocker-radar integration.

FrankenTerm already treats BV as an advisory prioritization source:

- `docs/blocker-radar-contract.md` says `br ready --json` and
  `br show <id> --json` are authoritative for claimability.
- `crates/frankenterm-core/src/blocker_radar.rs` maps the observed
  BV/BR disagreement to `tracker_inconsistent`.
- `crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json`
  preserves the `ft-e87u6.2` regression case.

No FrankenTerm code path should consume the BV recommendation as a claim command
without reconciliation.

## Reproduction

Run in `/Users/jemanuel/projects/frankenterm` on `main`:

```bash
br show ft-e87u6.2 --json
bv --robot-triage | jq '{
  top_pick: .triage.quick_ref.top_picks[0],
  top_recommendation: .triage.recommendations[0] | {
    id, status, action, reasons, blocked_by
  },
  commands: .triage.commands
}'
```

Observed on 2026-05-11:

The live output uses emoji prefixes in the `reasons` strings; they are omitted
below so this artifact stays ASCII-only.

```json
{
  "top_pick": {
    "id": "ft-e87u6.2",
    "title": "[reality-check][attest] Subtask 2: Update manifest.json + schema for deferred-slot semantics",
    "score": 0.20853515281341778,
    "reasons": [
      "Unblocks 2 item(s): ft-e87u6.3, ft-e87u6.5",
      "Currently unclaimed - available for work"
    ],
    "unblocks": 2
  },
  "top_recommendation": {
    "id": "ft-e87u6.2",
    "status": "blocked",
    "action": "Start work on this issue",
    "reasons": [
      "Unblocks 2 item(s): ft-e87u6.3, ft-e87u6.5",
      "Currently unclaimed - available for work"
    ],
    "blocked_by": null
  },
  "commands": {
    "claim_top": "bd update ft-e87u6.2 --status=in_progress",
    "show_top": "bd show ft-e87u6.2",
    "list_ready": "bd ready",
    "list_blocked": "bd blocked",
    "refresh_triage": "bv --robot-triage"
  }
}
```

The authoritative Beads record for the same id is `status=blocked` and
`assignee=BluePike`, with fresh comments saying current-head PR 59 checks are
still pending. `br ready --json` does not list `ft-e87u6.2`.

## Expected BV Behavior

For any candidate where the source issue status is not ready/open and unassigned,
BV should not emit claimability language or a start-work action. A blocked issue
can still be ranked as high-impact, but the action must stay dependency-aware.

Expected reduced output:

```json
{
  "top_recommendation": {
    "id": "ft-e87u6.2",
    "status": "blocked",
    "action": "Wait for blocker or owner; do not claim",
    "reasons": [
      "Unblocks 2 item(s): ft-e87u6.3, ft-e87u6.5",
      "Blocked in tracker; not claimable"
    ],
    "blocked_by": []
  },
  "commands": {
    "claim_top": null,
    "show_top": "br show ft-e87u6.2 --json",
    "list_ready": "br ready --json",
    "list_blocked": "br blocked --json",
    "refresh_triage": "bv --robot-triage"
  }
}
```

## Separate Stale Command-Hint Defect

In this repository the working tracker CLI is `br`, but BV still emits `bd`
commands in `.triage.commands`. That is a separate external-tool defect.

Expected behavior:

- Detect `br` repositories and emit `br` command hints.
- Prefer JSON-safe command hints for robot mode, for example
  `br show <id> --json` and `br ready --json`.
- Avoid emitting a claim command for blocked or assigned candidates.

## FrankenTerm-side Outcome

No repo integration patch is required for this bead. The local claimability
reconciler already produces:

```json
{
  "candidate_id": "ft-e87u6.2",
  "final_verdict": "tracker_inconsistent",
  "supporting_verdicts": ["owner_blocked", "external_wait", "mail_degraded"],
  "reason_codes": [
    "bv.br_status_mismatch",
    "br.assignee_active",
    "github.current_head_queued",
    "agent_mail.degraded"
  ],
  "next_action": "wait_or_coordinate_existing_owner",
  "forbidden_actions": [
    "auto_claim",
    "reopen_without_handoff",
    "rerun_ci",
    "repair_agent_mail"
  ]
}
```
