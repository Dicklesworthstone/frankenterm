# Deferred Proof Replay Harness Contract

Contract IDs:
- `ft.deferred_proof_replay_harness.input.v1` — retained no-mock input corpus of receipts.
- `ft.deferred_proof_replay_harness.tamper.v1` — adversarial single-receipt corpus.
- `ft.deferred_proof_replay_harness.decision.v1` — per-receipt dry-run decision record.

Source: `ft-zbnz4.6` (verification harness for the deferred RCH proof replay queue).
Schema: `docs/json-schema/ft-deferred-proof-replay-harness.json`.
Fixtures: `fixtures/deferred-proof-replay/replay-harness/`.
Static verifier: `bash tests/e2e/test_deferred_proof_replay_harness_contract.sh`.

## What the harness does

The harness projects retained receipts (extracted upstream by
`ft.deferred_proof_comment_extraction.v1`) into a queue, classifies each one
fail-closed into a single decision, records a dry-run replay decision per
receipt, and selects at most one next candidate. It is purely static: it
**never** mints material remote proof. A live material replay still requires a
real remote RCH worker, which is owned by `ft-zbnz4.4` -> `ft-5xwsu.3` and stays
blocked until RCH admission recovers.

The receipt's self-declared `eligibility.state`, `eligibility.replay_allowed`,
and `proof.evidence_classification` are **untrusted input**. The classifier
derives its decision only from structural and coordination facts, so a forged
`replay_allowed: true` or a forged green `evidence_classification` can never
upgrade a blocked or rejected receipt.

## Decision vocabulary

There is no green / `proof_complete` value anywhere in the decision space,
because the dry-run harness cannot produce material proof.

| Decision | Meaning |
| --- | --- |
| `run_static_now` | Static verifier; runnable right now with no RCH worker. The only decision that sets `replay_allowed_now: true`. |
| `would_run_remote` | Canonical remote-only receipt, admitted, clean. Would be replayed remotely once a worker is live; no proof is minted now. |
| `defer_remote_blocked` | Material remote receipt whose RCH admission was blocked at capture. Blocker recorded. |
| `defer_dirty_overlap` | Dirty-tree overlap at capture; resolve ownership first. |
| `defer_prerequisite` | A prerequisite bead is still open. |
| `cancelled` | Operator cancelled this replay; never auto-queue. |
| `reject_stale_command` | Non-canonical `command_shape_version`; refresh before any replay. |
| `reject_non_remote_command` | A material receipt whose command is not remote-only (bare/local cargo, or missing `RCH_REQUIRE_REMOTE=1`); a local fallback is reachable, so it is never proof. |
| `request_triage` | Ambiguous receipt (empty argv or ambiguous evidence). |

## Classification order (the fail-closed guarantee)

First match wins, in this order:

1. `operator_cancelled` -> `cancelled` (operator's explicit no overrides all).
2. `command_shape_version` not in `{rch-no-self-healing-v1, static-verifier-v1}` -> `reject_stale_command`.
3. Empty argv or `evidence_classification == ambiguous` -> `request_triage`.
4. Non-material: canonical static shape -> `run_static_now`, else `request_triage`.
5. Material remote:
   1. Not a remote-only command -> `reject_non_remote_command`.
   2. Dirty paths present -> `defer_dirty_overlap`.
   3. Prerequisite beads present -> `defer_prerequisite`.
   4. RCH admission blocked -> `defer_remote_blocked` (with the precise blocker).
   5. Admission admitted -> `would_run_remote`.
   6. Otherwise -> `request_triage`.

A command is **remote-only** iff argv is `rch ... --no-self-healing ... exec --
...`, both `RCH_REQUIRE_REMOTE=1` and `RCH_NO_SELF_HEALING=1` are present, and
`CARGO_TARGET_DIR=<target_dir>` is pinned in argv when a target dir is declared.

## Blocker codes

`rch.topology_preflight_failed`, `rch.worker_pressure`, `overlap.dirty_paths`,
`prereq.bead_open`, `operator.cancelled`, `command.stale_shape`,
`command.not_remote_only`, `triage.ambiguous`.

A **selected-worker topology-preflight failure** (`ln: Already exists`) keeps its
own `rch.topology_preflight_failed` blocker and its `selected_worker`. It is
never coarsened into `rch.no_admissible_workers` or `rch.worker_pressure`, never
classified as a code/test failure, and never green.

## Fail-closed invariants (locked by the tamper corpus)

- A bare/local cargo command never runs (`reject_non_remote_command`).
- A command missing `RCH_REQUIRE_REMOTE=1` never runs (local fallback reachable).
- A stale command shape never runs (`reject_stale_command`).
- A topology-preflight failure always defers, never runs, never green.
- A material receipt is never `replay_allowed_now`; `remote_exit_status` is
  always null. Only a real remote RCH replay may record a remote exit status.

## Next candidate selection

Prefer a `run_static_now` receipt (proof obtainable now, no RCH) over a
`would_run_remote` one (needs a live worker); break ties by `bead_id`; select at
most one. With RCH down, the next candidate must be `run_static_now`.

## Provenance / replay

Manual golden update from the static harness corpus. Regenerate and verify with:
`jq empty docs/json-schema/ft-deferred-proof-replay-harness.json fixtures/deferred-proof-replay/replay-harness/manifest.json fixtures/deferred-proof-replay/replay-harness/input-receipts.v1.json fixtures/deferred-proof-replay/replay-harness/tamper-cases.v1.json`,
`jq -c empty fixtures/deferred-proof-replay/replay-harness/expected/decisions.v1.jsonl`,
and `bash tests/e2e/test_deferred_proof_replay_harness_contract.sh`.
Future Rust / JSON Schema validation must run through RCH.
