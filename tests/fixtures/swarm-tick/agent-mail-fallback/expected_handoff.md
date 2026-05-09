Red-mail Beads handoff for ft-u45ni

Agent Mail: unavailable - Agent Mail unavailable: retry once, do not repair/restart service; continue with Beads-only coordination.
Snapshot: 2026-05-06T17:00:00Z session=frankenterm ready_count=2

Active assignees:
- BlueLake: ft-stale1 (stale_over_2h, age_seconds=9000)
- RedTower: ft-active1 (active_not_stale, age_seconds=1800)

Stale/non-stale classification:
- default_action: do_not_reopen
- threshold_seconds: 7200

Active, do not reopen:
- ft-active1: do_not_reopen for RedTower (age_seconds=1800)

Stale candidates requiring status check:
- ft-stale1: status_check_before_reopen for BlueLake (age_seconds=9000)

Dirty risk:
- risk_level: high
- risk_reason: tracked or shared coordination files are already dirty
- tracked_dirty_count: 2
- untracked_dirty_count: 2
- high_risk_count: 2

Dirty overlap unknown:
- .beads/issues.jsonl [shared_tracker/high]: do_not_reopen_related_beads_until_owner_clear
- crates/frankenterm-core/src/storage.rs [tracked_overlap_risk/high]: do_not_reopen_related_beads_until_owner_clear
- docs/robot-contracts/policy-recommendations.md [untracked_review_required/medium]: do_not_reopen_related_beads_until_owner_clear

Touched paths:
- scripts/swarm-tick.sh
- docs/operator-runbook.md

Avoided paths:
- crates/frankenterm/src/main.rs

Proof commands actually run:
- bash -n scripts/swarm-tick.sh
- shellcheck scripts/swarm-tick.sh

Closure basis: use only the proof commands above plus Beads state. Sync chatter, transfer logs, and code presence alone are not proof.

Review this block before posting. Suggested command:
br comments add ft-u45ni --author <agent> --file <reviewed-handoff.md>
