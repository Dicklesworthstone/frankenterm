# Deferred Proof Queue Surface Contract

`ft.deferred_proof_queue_surface.v1` is the static contract for the human,
Robot, and MCP surfaces that expose the deferred RCH proof replay queue
(`ft-zbnz4`). It lets an operator or agent answer four questions without DB or
comment archaeology:

1. What proof debt exists?
2. What can run right now?
3. Why is each item blocked?
4. What exact command would run if RCH admits it?

The schema is `docs/json-schema/ft-deferred-proof-queue-surface.json`; the golden
payload lives at `fixtures/deferred-proof-replay/queue-surface/`. The surface is
**read-only**: no view may suggest local Cargo, worker mutation, service repair,
deletion, reset, or broad formatting as an automatic action.

## Read-only guardrails

Every payload carries a `guardrails` block whose flags are all `true`
(`forbids_local_cargo`, `forbids_worker_mutation`, `forbids_service_repair`,
`forbids_deletion`, `forbids_reset`, `forbids_broad_formatting`) plus a
`remediation_allowlist`. Every remediation surfaced anywhere in the payload must
come from that allowlist, so a robot can never read a "fix it by running local
Cargo" instruction out of this surface. The remote `command_preview` is the only
place `cargo` appears, and it always runs through `rch --no-self-healing exec --`.

## Status buckets

The queue summary partitions receipts so queued proof is always distinct from
completed proof:

| Status | Meaning | Remediation |
| --- | --- | --- |
| `runnable` | RCH admitted; the receipt can replay now. | `none` |
| `wait_rch` | RCH admission is under worker pressure, or a selected worker failed remote topology preflight before Cargo. | `wait_for_rch_admission` |
| `dirty_overlap` | Captured tree had dirty paths outside the owned set. | `resolve_dirty_overlap` |
| `prerequisite_blocked` | A prerequisite bead has not landed its proof. | `complete_prerequisite_bead` |
| `stale_command` | Command lacks the remote-only RCH flags or exec shape. | `refresh_command_shape` |
| `ambiguous` | Footer carried prose, not a structured command. | `request_human_triage` |
| `completed` | Proof already replayed and passed. | `none` |

`replay_allowed` is `true` only for `runnable` entries. RCH admission failure,
worker pressure, and selected-worker topology preflight failure
(`failed_topology_preflight` / `rch.topology_preflight_failed`) are *deferral*
signals (the receipt stays queued under `wait_rch`), never an instruction to
repair RCH or mutate workers.

## Views

- **Queue summary** — `summary.total`, `summary.queued`, `summary.completed`,
  and `summary.by_status` counts for each bucket above.
- **Next candidate** — `next_candidate` is the single `runnable`,
  `replay_allowed` receipt with its `reason_codes`, `target_dir`, and the exact
  remote `command_preview` argv. It is `null` when nothing is runnable.
- **Per-bead history** — each `queue` entry carries its `source` provenance
  (comment id, author, timestamp, `source_text_sha256`) and `latest_replay`
  (`attempted_at`, `outcome`, `rch_admission_state`).
- **Explain** — `explain` has one entry per blocked receipt with a redacted
  human `why`, `blocking_reason_codes`, and an allowlisted `remediation`.
- **Robot dispatch** — `robot_dispatch` is a compact, TOON-serializable row per
  receipt (`bead_id`, `status`, `target_dir`, `replay_allowed`) for AI-to-AI
  hand-off.

## Operator runbook (after RCH recovers)

1. Read the queue summary; confirm `summary.by_status.runnable > 0`.
2. Take `next_candidate.command_preview` verbatim and run it through RCH
   (`RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- …`).
   Do not substitute local Cargo and do not edit the command shape.
3. For `wait_rch` entries, re-poll the surface; admission or topology recovery
   flips them to `runnable` without operator action.
4. For `dirty_overlap`, `prerequisite_blocked`, or `stale_command`, apply the
   listed remediation (resolve overlap, land the prerequisite, refresh the
   command shape) — never a reset, deletion, or worker change.
5. `ambiguous` entries need human triage; they never auto-queue.

## Negative corpus

`fixtures/deferred-proof-replay/queue-surface/invalid/fragments.v1.json` proves
the guardrails actually bite. Each case applies a minimal mutation to the golden
surface (e.g. flipping `forbids_local_cargo` to `false`, allowing replay on a
blocked entry, stripping `--no-self-healing` from the next-candidate command,
injecting a "run local cargo" remediation, leaking a `source_text` key, or
tampering a source digest) and asserts the verifier reports exactly that
violation. The golden surface itself must report zero violations.

Run:

```bash
bash tests/e2e/test_deferred_proof_queue_surface_contract.sh
```

This verifier is static. Any Rust, Robot, or MCP implementation proof must run
through remote-required RCH.
