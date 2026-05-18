# Robot Family Contract: `rch-worker-storage-approval`

**Beads:** `ft-5xwsu`, `ft-5xwsu.2`
**Status:** static approval artifact contract only; no runtime CLI command is
shipped by this document.

## Purpose

The RCH worker storage approval contract records the human authorization needed
before any worker storage recovery can be considered. It is a fail-closed bridge
between read-only retained inventory evidence and an operator-run recovery
procedure.

The output contract is `ft.rch_worker_storage_approval.v1`, defined in
`docs/json-schema/ft-rch-worker-storage-approval.json`.

## Non-Authority

This contract does not authorize agents to clean workers. It records whether an
operator approval artifact is valid enough for a human-controlled recovery step.
Agents must not delete files, clean targets, restart or repair RCH, mutate
workers, cancel builds, change remote mirrors, run local Cargo as proof, or
perform destructive git actions.

Every valid approval artifact records:

- `explicit_human_approval_required: true`;
- an approver identity or approval reference;
- the exact requested and approved path-set hashes;
- the single allowed operation, if any;
- broad forbidden operations including unlisted paths, wildcard expansion,
  protected paths, live-use unknowns, expired approvals, service repair/restart,
  worker mutation, build cancellation, and local Cargo proof;
- expiration and rollback or restore notes; and
- a post-action verification gate that requires remote-required RCH evidence.

Inventory evidence alone is never enough. A missing evidence hash, expired
approval, path mismatch, protected path, or live-use unknown must set
`destructive_recovery_allowed: false`.

## Required Fields

The root object carries:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version, currently `1`. |
| `contract_id` | Stable string, currently `ft.rch_worker_storage_approval.v1`. |
| `approval_id` | Stable fixture, retained artifact, or run id. |
| `source_bead` | Bead that produced the approval artifact. |
| `evidence_contract_id` | Retained inventory contract used as source evidence. |
| `evidence_artifact_path` / `evidence_artifact_sha256` | Exact retained inventory artifact and hash. |
| `approval_decision` | One of `approved`, `expired`, `path_mismatch`, `protected_path`, `missing_evidence_hash`, `live_use_unknown`, or `denied`. |
| `destructive_recovery_allowed` | `true` only for an unexpired exact-path approval with evidence hashes and inactive paths. |
| `approval_record` | Human identity/reference, approval timestamp, expiration, scope, and approval text hash. |
| `requested_paths` | Exact path rows, each with path hash, requested operation, classification, evidence hash, approval match, and live-use state. |
| `protected_path_policy` | Fail-closed rules for protected paths, exact matching, hash requirements, live-use unknowns, and source evidence. |
| `post_action_verification` | Remote-required RCH proof and Beads updates required after an approved recovery. |

## Fixture Coverage

Fixtures live under `fixtures/rch-worker-storage-approval/`:

- `valid/approved-candidate.json`
- `valid/expired-approval.json`
- `valid/path-mismatch.json`
- `valid/protected-path.json`
- `valid/missing-evidence-hash.json`
- `valid/live-use-unknown.json`

All fixtures are schema-valid. Only `approved-candidate.json` allows a recovery
operation, and it allows only `move_to_quarantine` for an exact inactive path.
The other fixtures are negative, fail-closed artifacts that explain why recovery
must not proceed.

## Proof Posture

Schema and fixture work is static documentation work. Local static checks are
sufficient for this substrate:

```text
jq empty docs/json-schema/ft-rch-worker-storage-approval.json fixtures/rch-worker-storage-approval/manifest.json fixtures/rch-worker-storage-approval/valid/*.json
bash tests/e2e/test_rch_worker_storage_approval_contract.sh
git diff --check -- docs/json-schema/ft-rch-worker-storage-approval.json docs/robot-contracts/rch-worker-storage-approval.md fixtures/rch-worker-storage-approval tests/e2e/test_rch_worker_storage_approval_contract.sh
br dep cycles --json
```

Any later implementation that compiles code, executes `ft`, reaches Cargo, or
claims recovered worker admission must use remote-required RCH. Local Cargo
output is not a substitute.
