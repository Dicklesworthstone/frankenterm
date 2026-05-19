# Robot Family Contract: `rch-worker-storage-recovery-proof`

**Beads:** `ft-5xwsu`, `ft-5xwsu.3`, `ft-4tp7g`
**Status:** static post-recovery proof-gate contract only; no runtime CLI
command is shipped by this document.

## Purpose

The RCH worker storage recovery proof contract defines the evidence required
after an operator-approved worker storage recovery. It proves one of two
outcomes:

- remote-required RCH reaches a worker and completes a material remote Cargo
  smoke, so the admission blocker can be treated as recovered; or
- remote-required RCH still refuses admission, but the retained proof records a
  precise stable reason code instead of hand-waving around `critical_pressure=5`.

The output contract is `ft.rch_worker_storage_recovery_proof.v1`, defined in
`docs/json-schema/ft-rch-worker-storage-recovery-proof.json`.

## Non-Authority

This contract is not recovery approval. It must be used only after the approval
artifact from `ft.rch_worker_storage_approval.v1` exists and the recovery step
has been performed by an operator. Agents must not delete files, clean targets,
restart or repair RCH, mutate workers, cancel builds, change remote mirrors,
run local Cargo as proof, or perform destructive git actions.

Every proof artifact records:

- the approval artifact path and SHA-256;
- the operator recovery reference;
- read-only `rch --json status --workers --jobs` evidence;
- a remote-required dry-run for a narrow FrankenTerm Cargo command;
- a material remote-required smoke when the dry-run selects a worker;
- transfer, skip, remote execution, and exit-state evidence;
- the installed RCH version and daemon posture;
- selected worker identity or explicit admission reason;
- retained artifact paths and hashes; and
- `br dep cycles --json` evidence.

`ft-4tp7g` must not be closed from inventories alone. Closeout is allowed only
when the gate result is `passed_remote_smoke`.

## Required Fields

The root object carries:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version, currently `1`. |
| `contract_id` | Stable string, currently `ft.rch_worker_storage_recovery_proof.v1`. |
| `proof_id` | Stable fixture, retained artifact, or run id. |
| `source_bead` | Producing bead, always `ft-5xwsu.3`. |
| `approval_contract_id` / `approval_artifact_path` / `approval_artifact_sha256` | Exact approval artifact used before recovery. |
| `operator_recovery_reference` | Human/operator recovery record. It is required for a passing gate. |
| `gate_result` | One of `passed_remote_smoke`, `blocked_no_admissible_worker`, `blocked_new_reason`, `failed_remote_smoke`, or `invalid_missing_approval`. |
| `admission_recovered` | `true` only when the material remote smoke completed with exit status 0. |
| `ft4tp7g_closeout_allowed` | `true` only for `passed_remote_smoke`. |
| `stable_reason_code` | Required for blocked or failed gates; null for a passing gate. |
| `agent_side_effect_policy` | Fail-closed flags proving agents did not perform recovery or count local Cargo. |
| `rch_status` | Read-only RCH worker/job posture evidence. |
| `remote_required_dry_run` | Remote-required dry-run selection evidence. |
| `remote_required_smoke` | Required material remote smoke, or an explicit skip when no worker is selected. |
| `br_dep_cycles` | Beads dependency proof, currently count 0. |

## Fixture Coverage

Fixtures live under `fixtures/rch-worker-storage-recovery-proof/`:

- `valid/passed-remote-smoke.json`
- `valid/blocked-no-admissible-worker.json`
- `valid/blocked-new-reason.json`
- `valid/failed-remote-smoke.json`
- `valid/invalid-missing-approval.json`

All fixtures are schema-valid. Only `passed-remote-smoke.json` allows
`ft4tp7g` closeout. The blocked fixtures require remote-smoke skip evidence and
a stable reason code. The failed-smoke fixture proves that selecting a worker is
not enough; the material remote smoke must complete successfully. The
`invalid-missing-approval` fixture covers `invalid_missing_approval`: without an
approval artifact and operator recovery reference, the proof gate is invalid,
remote-required proof commands remain not-attempted, and `ft-4tp7g` closeout is
forbidden.

## Proof Posture

Schema and fixture work is static documentation work. Local static checks are
sufficient for this substrate:

```text
jq empty docs/json-schema/ft-rch-worker-storage-recovery-proof.json fixtures/rch-worker-storage-recovery-proof/manifest.json fixtures/rch-worker-storage-recovery-proof/valid/*.json
bash tests/e2e/test_rch_worker_storage_recovery_proof_contract.sh
git diff --check -- docs/json-schema/ft-rch-worker-storage-recovery-proof.json docs/robot-contracts/rch-worker-storage-recovery-proof.md fixtures/rch-worker-storage-recovery-proof tests/e2e/test_rch_worker_storage_recovery_proof_contract.sh
br dep cycles --json
```

Any later implementation that compiles code, executes `ft`, reaches Cargo, or
claims recovered worker admission must use remote-required RCH. Local Cargo
output is not a substitute.
