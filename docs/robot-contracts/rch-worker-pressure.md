# Robot Family Contract: `rch-worker-pressure`

**Beads:** `ft-5xwsu`, `ft-5xwsu.1`
**Status:** schema and retained fixture contract only; no runtime CLI command is
shipped by this document.

## Purpose

The RCH worker-pressure inventory contract turns read-only worker storage
evidence into a retained artifact that can be reviewed without rerunning broad
remote scans. It exists because remote-required FrankenTerm proof lanes can be
blocked before Cargo, rustc, or test binaries are reached when every healthy RCH
worker is under critical storage pressure.

The canonical output contract is `ft.rch_worker_storage_inventory.v1`, defined
in `docs/json-schema/ft-rch-worker-storage-inventory.json`. The older
pressure-named draft is superseded and must not be used by approval or
post-recovery proof artifacts.

## Non-Authority

This contract is evidence only. It must not be used as permission to delete
files, clean targets, restart or repair RCH, mutate workers, cancel builds,
change remote mirrors, run local Cargo as proof, or perform destructive git
actions.

Every valid artifact records:

- `collection_scope.side_effect_policy.read_only: true`;
- `collection_scope.side_effect_policy.files_deleted: false`;
- `collection_scope.side_effect_policy.worker_mutated: false`;
- `collection_scope.side_effect_policy.local_cargo_counted_as_proof: false`;
- the performed read-only evidence collection actions; and
- forbidden actions including deletion, target cleaning, RCH repair/restart,
  worker mutation, build cancellation, mirror mutation, and local Cargo proof.

The follow-on approval contract under `ft-5xwsu.2` is required before any human
or automation considers destructive recovery. This inventory only provides the
input evidence for that review.

## Required Fields

The root object carries:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version, currently `1`. |
| `contract_id` | Stable string, currently `ft.rch_worker_storage_inventory.v1`. |
| `generated_at_ms` | Unix epoch milliseconds for the retained artifact. |
| `inventory_id` | Stable run or fixture id. |
| `source_bead` | Bead that produced the inventory artifact. |
| `source_context` | Optional parent bead and seed evidence pointers. |
| `collection_scope.side_effect_policy` | Read-only and forbidden-action posture. |
| `summary` | Worker counts, pressure counts, and next required action. |
| `worker_inventories` | Per-worker storage and scan evidence. |
| `artifact_paths` | Retained artifacts or Beads/Mail evidence references. |

Each worker inventory row must include `worker_id`, `host_label`,
`telemetry_status`, `pressure_reason`, `df_samples`, `shallow_scans`,
`project_du_samples`, `artifact_paths`, and `notes`.

Each retained scan entry must include `path`, `source_command`, freshness or
status, timeout or partial-output state where applicable, `pressure_reason`,
`artifact_path`, and `notes`.

## Scan Kinds

| `scan_kind` | Meaning |
| --- | --- |
| `rch_status` | Read-only `RCH_NO_SELF_HEALING=1 rch --no-self-healing --json status --workers --jobs` or equivalent status snapshot. |
| `worker_capabilities` | Read-only worker capability or probe snapshot. |
| `df_root` | Worker root filesystem free-space sample. |
| `shallow_target_temp` | Bounded target/temp scan such as `/tmp/rch-*` and `target*`. |
| `bounded_project_du` | Bounded per-project disk-usage scan. |
| `other` | Explicitly noted read-only evidence that does not fit another class. |

`timeout_state` is `completed`, `timed_out`, `partial`, or `not_run`.
`partial_output: true` means the retained output is usable as evidence of
pressure but not as a complete inventory for cleanup review.

## Fixture Coverage

Fixtures live under `fixtures/rch-worker-pressure/`:

- `manifest.json`
- `valid/healthy-complete.json`
- `valid/partial-timeout.json`
- `valid/telemetry-gap.json`

The complete fixture models a worker with a completed root `df`, shallow
target/temp scan, and bounded project inventory. The partial-timeout fixture
models bounded `du` output that produced useful rows before hitting timeout. The
telemetry-gap fixture models workers that are degraded, unreachable, or missing
fresh disk metrics.

## Proof Posture

Schema and fixture work is static documentation work. Local static checks are
sufficient for this bead:

```text
jq empty docs/json-schema/ft-rch-worker-storage-inventory.json fixtures/rch-worker-pressure/manifest.json fixtures/rch-worker-pressure/valid/*.json
bash tests/e2e/test_rch_worker_storage_inventory_contract.sh
git diff --check -- docs/json-schema/ft-rch-worker-storage-inventory.json docs/robot-contracts/rch-worker-pressure.md fixtures/rch-worker-pressure tests/e2e/test_rch_worker_storage_inventory_contract.sh
jq -c empty .beads/issues.jsonl
br dep cycles --json
```

Any later implementation that compiles code, exercises `ft`, reaches Cargo, or
claims remote worker admission recovery must use remote-required RCH. If RCH
still reports `no_admissible_workers=critical_pressure=5`, the proof remains
blocked and local Cargo output is not a substitute.
