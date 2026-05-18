# RCH Worker Storage Pressure Runbook

Bead: `ft-5xwsu.4`

Status: operator workflow for RCH worker storage pressure. This runbook does
not authorize cleanup; it binds the inventory, approval, and post-recovery proof
contracts into one fail-closed workflow.

## Scope

Use this when FrankenTerm RCH proof lanes are blocked by worker storage pressure
such as `no_admissible_workers=critical_pressure=5`.

The runbook separates five evidence streams:

- source-code defects
- RCH fleet pressure
- dirty-tree contamination
- Agent Mail outages
- Beads tracker state

Do not collapse one stream into another. A blocked RCH proof is not a Rust test
failure. A clean inventory is not approval. An approval artifact is not recovery
proof. Transfer or sync chatter is not compile/test proof.

## Forbidden Actions

Agents must not perform or recommend routine recovery actions. The forbidden
action classes are:

- `delete_files_without_approval`
- `run_agent_cleanup`
- `restart_agent_mail`
- `repair_agent_mail_db`
- `restart_rch_daemon`
- `mutate_rch_worker`
- `mutate_remote_mirror`
- `cancel_other_agent_build`
- `destructive_git`
- `run_local_cargo_as_proof`
- `close_ft4tp7g_without_remote_evidence`

Only the human operator can approve a recovery operation, and only through the
approval artifact described by `ft.rch_worker_storage_approval.v1`.

## Read-Only Evidence Collection

Start with current RCH posture:

```text
RCH_NO_SELF_HEALING=1 rch --no-self-healing --json status --workers --jobs
RCH_NO_SELF_HEALING=1 rch --no-self-healing --json check
RCH_NO_SELF_HEALING=1 rch --no-self-healing diagnose --dry-run --json -- cargo check -p frankenterm-core --lib
br ready --json
br list --status in_progress --json
br dep cycles --json
git status --short
```

Use the retained inventory contract for any worker storage evidence:

- schema: `docs/json-schema/ft-rch-worker-storage-inventory.json`
- fixtures: `fixtures/rch-worker-pressure/manifest.json`
- verifier: `tests/e2e/test_rch_worker_storage_inventory_contract.sh`

Inventory rows must record source command, worker id, path, size, freshness,
timeout state, partial-output marker, pressure reason, and retained artifact
path. Partial, stale, or telemetry-gap evidence remains review input only.

## Classification

Classify the blocker before asking for approval:

| Class | Evidence | Action |
| --- | --- | --- |
| Source defect | RCH reaches remote Cargo/test and the test fails. | Fix source under the owning bead. |
| RCH fleet pressure | Dry-run selects no worker with critical pressure or telemetry gaps. | Continue this runbook; no source verdict. |
| Dirty-tree contamination | Owned proof path overlaps unrelated dirty tracked or untracked files. | Stop and isolate ownership before proof. |
| Agent Mail outage | Mail registration or inbox fails after the single allowed retry. | Use Beads/git fallback; do not repair services. |
| Tracker drift | Beads state disagrees with shipped artifacts. | Reconcile with comments or a tracker-only commit. |

Broad project-tree pressure is not automatically safe to clean. Protected source
checkouts, live-use unknowns, missing hashes, wildcard path sets, and stale
evidence all fail closed.

## Approval Request

Create or request an approval artifact using
`ft.rch_worker_storage_approval.v1` before any recovery step.

Required references:

- schema: `docs/json-schema/ft-rch-worker-storage-approval.json`
- contract: `docs/robot-contracts/rch-worker-storage-approval.md`
- fixtures: `fixtures/rch-worker-storage-approval/manifest.json`
- verifier: `tests/e2e/test_rch_worker_storage_approval_contract.sh`

Inventory evidence alone is never enough. Approval must name exact paths, exact operations, evidence hashes, an approver
identity or approval reference, expiration, protected-path result, live-use
state, and rollback or restore notes. It must also name the post-action
verification requirement.

Approval request template:

```text
RCH worker storage approval request for <bead-id>

- inventory_artifact: <path>
- inventory_sha256: <sha256>
- affected_workers: <worker ids>
- requested_paths: <exact path list, no wildcard expansion>
- requested_operation: <single operation>
- protected_path_result: <allowed / denied>
- live_use_state: <inactive / active / unknown>
- expiration: <timestamp>
- rollback_or_restore_notes: <notes>
- post_recovery_gate: docs/robot-contracts/rch-worker-storage-recovery-proof.md

No agent will perform recovery without explicit written operator approval.
```

## Artifact Retention

Retain evidence under bead-scoped directories:

```text
tests/e2e/artifacts/retained/ft-5xwsu.1/rch-worker-pressure/<run-id>/
tests/e2e/artifacts/retained/ft-5xwsu.2/rch-worker-storage-approval/<run-id>/
tests/e2e/artifacts/retained/ft-5xwsu.3/rch-worker-storage-recovery-proof/<run-id>/
```

Each retained artifact must have a SHA-256 recorded in the relevant manifest or
proof payload. Keep partial-output and timeout markers; do not replace them with
clean summaries.

## Post-Recovery Proof

After an operator-approved recovery, run the proof gate from
`ft.rch_worker_storage_recovery_proof.v1`.

Required references:

- schema: `docs/json-schema/ft-rch-worker-storage-recovery-proof.json`
- contract: `docs/robot-contracts/rch-worker-storage-recovery-proof.md`
- fixtures: `fixtures/rch-worker-storage-recovery-proof/manifest.json`
- verifier: `tests/e2e/test_rch_worker_storage_recovery_proof_contract.sh`

Minimum proof sequence:

```text
RCH_NO_SELF_HEALING=1 rch --no-self-healing --json status --workers --jobs
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing diagnose --dry-run --json -- cargo check -p frankenterm-core --lib
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=<target-dir> cargo check -p frankenterm-core --lib
br dep cycles --json
```

Run the material remote-required smoke only when the dry-run selects a worker.
If the dry-run selects no worker, retain the stable reason code and leave
`ft-5xwsu.3` and `ft-4tp7g` blocked. Do not substitute local Cargo.

`ft-4tp7g` closeout is allowed only when the recovery proof records
`gate_result=passed_remote_smoke` and `admission_recovered=true`.

## Failure Modes

| Failure mode | Required response |
| --- | --- |
| Missing approval artifact | Stop. Record `invalid_missing_approval`. |
| Approval expired | Stop. Request a fresh approval artifact. |
| Path mismatch | Stop. Exact path hashes must match. |
| Protected path | Stop. Protected paths require separate human decision. |
| Live-use unknown | Stop. Unknown live-use state denies recovery. |
| No admissible worker after recovery | Retain `blocked_no_admissible_worker` or `blocked_new_reason`; keep blockers open. |
| Remote smoke nonzero exit | Treat as `failed_remote_smoke`; classify source/test failure separately. |
| Dirty tree overlap | Stop proof; isolate ownership before rerun. |
| Agent Mail outage | Use Beads/git fallback; do not repair Agent Mail. |

## Handoff Wording

Beads comment template:

```text
RCH worker storage pressure handoff for <bead-id>

- evidence_class: <source_defect | rch_fleet_pressure | dirty_tree | agent_mail_outage | tracker_drift>
- inventory_artifact: <path or none>
- approval_artifact: <path or none>
- recovery_reference: <operator reference or none>
- post_recovery_gate_result: <result or not_run>
- selected_worker: <worker id or none>
- stable_reason_code: <reason or none>
- proof_commands: <commands actually run>
- avoided_actions: delete_files_without_approval, run_agent_cleanup, restart_agent_mail, repair_agent_mail_db, restart_rch_daemon, mutate_rch_worker, mutate_remote_mirror, cancel_other_agent_build, destructive_git, run_local_cargo_as_proof

No local Cargo proof, service repair/restart, worker mutation, build
cancellation, remote mirror mutation, destructive git action, or file deletion
was performed by this agent.
```

Agent Mail handoff template:

```text
Thread: <bead-id>
Subject: RCH worker storage pressure status

Current classification: <classification>
Inventory artifact: <path or none>
Approval artifact: <path or none>
Recovery proof artifact: <path or none>
Blocking reason: <stable reason>
Next safe action: <read-only evidence / operator approval / post-recovery proof / source fix>
Owned paths: <paths>
Avoided paths: <dirty or reserved paths>
```

## Static Proof

Runbook smoke proof:

```text
jq empty fixtures/rch-worker-storage-runbook/contract.v1.json
bash -n tests/e2e/test_rch_worker_storage_runbook_contract.sh
bash tests/e2e/test_rch_worker_storage_runbook_contract.sh
git diff --check -- docs/robot-contracts/rch-worker-storage-runbook.md fixtures/rch-worker-storage-runbook/contract.v1.json tests/e2e/test_rch_worker_storage_runbook_contract.sh README.md
br dep cycles --json
```

This proof is docs/static only. Any future implementation, Cargo, `ft`, or
material RCH proof remains remote-required through RCH.
