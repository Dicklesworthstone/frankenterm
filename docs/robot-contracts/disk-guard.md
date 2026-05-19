# Robot Family Contract: `disk-guard`

**Beads:** `ft-fyk4x.1`, `ft-fyk4x.2`, `ft-fyk4x.3`
**Status:** core collector normalizer shipped under
`crates/frankenterm-core/src/disk_guard.rs`; no runtime CLI command is shipped by
this document.

## Purpose

The disk guard is a read-only preflight contract for deciding whether it is safe
to edit files, write Beads state, run static proof scripts, or launch RCH proof
lanes. It exists because a full APFS data volume can make patch application,
temporary target creation, Beads export, Agent Mail writes, and RCH cache writes
fail after work has already entered a half-applied state.

The output contract is `ft.disk_guard.v1`, defined in
`docs/json-schema/ft-disk-guard.json`. The core normalizer accepts read-only
probe facts, fills any missing required probes as fail-closed
`not_collected` entries, and emits the schema-shaped report without performing
cleanup or service repair.

## Required Collectors

Every valid disk-guard artifact records these probes:

- `system_data_volume`: free-space sample for `/System/Volumes/Data` or the
  platform-equivalent data volume;
- `private_tmp`: free-space and write-precondition sample for `/private/tmp`;
- `repo_write_probe`: bounded repository write probe status;
- `beads_db_writeability`: Beads SQLite writeability status;
- `beads_jsonl_exportability`: Beads JSONL export or sync status;
- `agent_mail_db_open`: Agent Mail database open or degraded-read-only status;
- `rch_cache_writeability`: local RCH cache/socket/write-precondition status;
  and
- `external_scratch`: availability of an external scratch volume or equivalent
  recovery target.

Each probe records source, timestamp, severity, reason codes, threshold bytes
when applicable, observed free bytes when applicable, probe result, error
category, retained artifact paths, and the next safe action.

The first implementation slice includes a bounded filesystem sampler using the
platform filesystem-stat API for data-volume, private-tmp, and external-scratch
paths. Repository, Beads, Agent Mail, and RCH write/status probes are normalized
from externally collected facts; the normalizer does not create and then delete
probe files.

## Side-Effect Policy

The disk guard is a preflight and inventory surface. It never deletes files,
repairs services, restarts daemons, mutates worker mirrors, cancels builds,
cleans target directories, runs local Cargo as proof, or changes Beads except for
normal tracker updates by the agent performing the work.

Cleanup candidates are advisory evidence only. They must be separated from
automatic behavior and require explicit operator approval outside this contract.

## Preflight Surfaces

`preflight_results` is an optional surface-specific decision array. It lets an
agent ask the normalizer whether a concrete write path should proceed before
the agent starts work that can otherwise fail half-applied.

Supported surfaces:

- `patch_application`: checks shared-checkout patch application before `git
  apply` or equivalent patch writes;
- `static_verifier_creation`: checks whether bounded static verifier artifacts
  can be created locally or must be retained on external scratch;
- `beads_comment_export`: checks Beads DB plus JSONL export writeability before
  comment/export work; and
- `rch_proof_lane`: checks whether local disk and RCH cache state permit a
  material remote-required proof attempt, independent of final worker
  selection.

Every result records `surface`, `action`, `write_allowed`,
`external_scratch_required`, `blocking_probe_ids`, reason codes, and the next
safe action. The result fails closed: missing, unknown, unavailable, failed, or
blocked precondition probes become explicit blockers. The static verifier
surface is the only surface that may return `external_scratch_only`; patch
application, Beads writes, and RCH proof lanes must not treat external scratch as
a substitute for the shared checkout or required local cache/write probes.

## Cleanup Inventory Candidates

`cleanup_candidates` is an optional read-only inventory attached to the
`ft.disk_guard.v1` report. It normalizes facts collected by external commands
such as `du`, `stat`, `lsof`, and process scans, but it does not run those
commands itself and never emits a deletion command.

Every candidate records:

- path and owning project;
- candidate kind, such as `cargo_target`, `cargo_home_registry`,
  `rch_cargo_home`, `rch_target`, `project_cache`, or
  `temp_proof_artifact`;
- size in bytes, mtime, derived age, and `CACHEDIR.TAG` evidence when known;
- live-use evidence from `lsof` or process-reference counts when collected;
- `risk_tier`, where `low` still means "operator review only", not safe
  automatic deletion;
- `operator_approval_required: true` and
  `automatic_cleanup_allowed: false`; and
- retained artifact paths for the inventory evidence that produced the row.

The classifier is deliberately conservative. Paths under
`crates/frankenterm-core/` and `.git/` are `protected`. Live process references,
future mtimes, recent mtimes, missing live-use evidence, missing
`CACHEDIR.TAG`, unknown candidate kind, or sub-floor size all keep the candidate
out of the low-risk advisory bucket. A low-risk candidate is still only a prompt
for explicit operator review.

## Result Semantics

`decision` is one of:

| Decision | Meaning |
| --- | --- |
| `proceed` | All required write preconditions are inside the green/yellow envelope. |
| `static_only` | Static, read-mostly work may proceed, but proof or write-heavy lanes should wait. |
| `external_scratch_only` | Local writes are unsafe; use retained external scratch artifacts only. |
| `block` | A required write precondition failed or available space is below the configured floor. |
| `unknown` | A collector could not establish enough evidence to classify safely. |

Collectors fail closed. Missing or contradictory data lowers the decision and
adds `source.*` or `fail_closed.*` reason codes.

The normalizer keeps `external_scratch_only` distinct from `block`: local disk
floors may fail while retained external artifacts remain usable, but repository,
Beads, JSONL, or RCH-cache write-precondition failures still block material
local work.

## Required Reason-Code Families

- `disk.*` for free-space thresholds and filesystem samples;
- `write_probe.*` for bounded probe outcomes;
- `beads.*` for DB/JSONL writeability and sync state;
- `agent_mail.*` for DB-open and degraded fallback posture;
- `rch.*` for cache/socket/write-precondition status;
- `external_scratch.*` for off-volume recovery surfaces;
- `cleanup_candidate.*` for advisory inventory references; and
- `fail_closed.*`, `policy.*`, and `source.*` for reductions.

## Fixtures

Fixtures live under `fixtures/disk-guard/`; the manifest is
`fixtures/disk-guard/manifest.json`:

- `manifest.json`
- `valid/current-eno-space.json`
- `valid/cleanup-inventory.json`
- `valid/preflight-surfaces.json`
- `valid/healthy.json`
- `valid/warning-low-space.json`
- `valid/fatal-write-probe-failed.json`

The retained static verifier lives at
`tests/e2e/test_disk_guard_contract.sh`. It checks the schema pointers,
manifest fixture coverage, required probe IDs, read-only side-effect policy,
cleanup-inventory operator-approval semantics, preflight surface coverage, and
README E2E count stamp. It is a static contract verifier only; it is not Cargo,
RCH, or runtime proof.

The current fixture models an ENOSPC recovery state from 2026-05-17 where
`/System/Volumes/Data` and `/private/tmp` are below the recovery floor, Beads
sync is internally clean, Agent Mail is degraded, RCH is reachable but degraded,
and an external USB scratch volume is available.
