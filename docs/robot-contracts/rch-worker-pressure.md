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

The output contract is `ft.rch_worker_pressure.inventory.v1`, defined in
`docs/json-schema/ft-rch-worker-pressure-inventory.json`.

## Non-Authority

This contract is evidence only. It must not be used as permission to delete
files, clean targets, restart or repair RCH, mutate workers, cancel builds,
change remote mirrors, run local Cargo as proof, or perform destructive git
actions.

Every valid artifact records:

- `side_effect_policy.read_only: true`;
- `side_effect_policy.operator_approval_required: true`;
- `side_effect_policy.automatic_cleanup_allowed: false`;
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
| `contract_id` | Stable string, currently `ft.rch_worker_pressure.inventory.v1`. |
| `generated_at_ms` | Unix epoch milliseconds for the retained artifact. |
| `inventory_id` | Stable run or fixture id. |
| `source_bead` | Bead that produced the inventory artifact. |
| `source_context` | Optional parent bead and seed evidence pointers. |
| `side_effect_policy` | Read-only and forbidden-action posture. |
| `summary` | Worker counts, pressure counts, and next required action. |
| `workers` | Per-worker storage and scan evidence. |
| `artifact_paths` | Retained artifacts or Beads/Mail evidence references. |

Each worker row must include `worker_id`, `host_label`, `sampled_at_ms`,
`pressure_state`, `admission_state`, `scan_status`, `source_commands`,
`entries`, `reason_codes`, and `notes`.

Each inventory entry must include `path`, `source_command`, `scan_kind`,
`freshness`, `timeout_state`, `partial_output`, `pressure_reason`,
`artifact_path`, and `notes`. It must also include either `size_bytes` or
`size_text`.

## Scan Kinds

| `scan_kind` | Meaning |
| --- | --- |
| `rch_status` | Read-only `rch --json status --workers` or equivalent status snapshot. |
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
- `valid/complete-inventory.json`
- `valid/partial-timeout-inventory.json`
- `valid/telemetry-gap-inventory.json`

The complete fixture models a worker with a completed root `df`, shallow
target/temp scan, and bounded project inventory. The partial-timeout fixture
models bounded `du` output that produced useful rows before hitting timeout. The
telemetry-gap fixture models workers that are degraded, unreachable, or missing
fresh disk metrics.

## Proof Posture

Schema and fixture work is static documentation work. Local static checks are
sufficient for this bead:

```text
jq empty docs/json-schema/ft-rch-worker-pressure-inventory.json fixtures/rch-worker-pressure/manifest.json fixtures/rch-worker-pressure/valid/complete-inventory.json fixtures/rch-worker-pressure/valid/partial-timeout-inventory.json fixtures/rch-worker-pressure/valid/telemetry-gap-inventory.json
git diff --check -- docs/json-schema/ft-rch-worker-pressure-inventory.json docs/robot-contracts/rch-worker-pressure.md fixtures/rch-worker-pressure
jq -c empty .beads/issues.jsonl
br dep cycles --json
```

Any later implementation that compiles code, exercises `ft`, reaches Cargo, or
claims remote worker admission recovery must use remote-required RCH. If RCH
still reports `no_admissible_workers=critical_pressure=5`, the proof remains
blocked and local Cargo output is not a substitute.
