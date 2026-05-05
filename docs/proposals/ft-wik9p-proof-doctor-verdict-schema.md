# Proof-doctor verdict schema and blocker taxonomy (ft-wik9p.1)

## Status

Design contract for `ft-wik9p.1`.

This document defines the v1 proof-doctor vocabulary that later
implementation beads must use for CLI, robot-mode, Beads, Agent Mail, proof
ledger, and release-report handoffs. It is intentionally a schema and taxonomy
document, not an implementation.

## Problem

FrankenTerm proof lanes can fail in several ways that look similar in raw logs:

- RCH can select a worker and finish sync but fail before Cargo starts.
- A patched RCH binary can reach remote Cargo and then expose an unrelated
  source compile error.
- A dirty shared worktree can contain active files owned by another Bead.
- A shell-wrapped or local Cargo command can look like useful signal while
  being invalid for RCH-required proof.
- A high-scale lane can skip because the worker predicate is absent without
  proving the performance claim.

Operators need a preflight and post-run verdict that says which class of
blocker occurred, who owns the next action when known, and what evidence can
or cannot be claimed.

## Existing Anchors

Proof-doctor must consume and feed existing surfaces instead of creating a
parallel proof system.

| Surface | Existing anchor | Contract |
| --- | --- | --- |
| Proof ledger DTOs | `crates/frankenterm-core-audit-types/src/proof_lane.rs` | `ProofAttemptRecord`, `ProofState`, `ProofReportBucket`, and `validate_proof_record` remain the durable proof-lane source of truth. |
| Core re-export | `crates/frankenterm-core/src/lib.rs` | `frankenterm_core::proof_lane` exposes the ledger DTOs to current callers. |
| Robot envelope | `crates/frankenterm/src/main.rs` | Robot output uses `ok`, `data`, `error`, `error_code`, `hint`, `elapsed_ms`, `version`, and `now`. |
| Finish-line proof | `docs/ft-xbnl0-verification-contract.md` | Heavy Cargo, clippy, tests, benches, and E2E proof must use RCH or a fail-closed RCH harness. |
| Artifact logs | `docs/test-logging-contract.md` | Material test artifacts must retain exact commands, structured logs, manifests, and reason codes. |
| RCH metadata | `tests/e2e/lib_rch_guards.sh` | Existing RCH guards already extract selected worker, sync duration, remote duration, remote exit code, wrapper exit code, timeout, and fail-open signals. |
| Evidence taxonomy | `docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md` | Proof state semantics and invalid command-shape rules are the baseline for proof-doctor classification. |

## Layering

Proof-doctor is a diagnostic verdict layer around a planned or observed proof
attempt.

```text
operator / agent intent
        |
        v
ProofDoctorVerdict  (preflight + optional observed evidence)
        |
        +--> Beads comment / Agent Mail handoff
        +--> robot-mode JSON/TOON payload
        +--> ProofAttemptRecord when a material attempt exists
        +--> ProofLaneReport / release summary aggregation
```

Proof-doctor must not execute arbitrary proof commands in the first
implementation. It should parse intent, inspect local evidence, classify
blockers, and produce a machine-readable verdict. Later execution integration
may attach observed proof evidence, but it must still rely on the proof ledger
for durable closeout claims.

## Verdict Schema

The first implementation should model a proof-doctor response as:

```rust
pub struct ProofDoctorVerdict {
    pub schema_version: u32,
    pub verdict_id: String,
    pub bead_id: Option<String>,
    pub parent_bead_id: Option<String>,
    pub generated_at_utc: String,
    pub agent_name: String,
    pub repo_path: String,
    pub git_head: String,
    pub branch: String,
    pub intended_command: Vec<String>,
    pub intended_target_dir: Option<String>,
    pub intended_scope: ProofScope,
    pub required_backend: ProofBackend,
    pub phase: ProofDoctorPhase,
    pub status: ProofDoctorStatus,
    pub blockers: Vec<ProofDoctorBlocker>,
    pub evidence: ProofDoctorEvidence,
    pub ledger_projection: Option<ProofAttemptProjection>,
    pub operator_summary: String,
    pub next_action: ProofDoctorNextAction,
}
```

Field requirements:

- `schema_version` is `1`.
- `verdict_id` is stable enough to cite in a Beads comment or release report.
- `intended_command` stores argv, not shell prose.
- `intended_scope` reuses `ProofScope`.
- `required_backend` reuses `ProofBackend`.
- `operator_summary` is one short sentence, suitable for Beads and Agent Mail.
- `blockers` is empty only when the lane is runnable or already passed.
- `ledger_projection` is present only when the verdict can be losslessly mapped
  to a `ProofAttemptRecord` or a planned non-attempt.

## Phases

`ProofDoctorPhase` identifies what the doctor inspected.

| Phase | Meaning |
| --- | --- |
| `preflight` | No material proof command has run. The doctor inspected intent, tool state, dirty tree, Beads, and reservations. |
| `launch_observed` | RCH or the requested backend launched, but the doctor has only early evidence such as worker selection or sync. |
| `remote_cargo_observed` | Retained logs prove remote Cargo or rustc started, but terminal result is not classified yet. |
| `terminal_classified` | A terminal proof state or blocker has enough evidence for durable handoff. |
| `evidence_gap` | The doctor cannot tell which phase was reached because required logs or metadata are absent. |

## Status Values

`ProofDoctorStatus` is the top-level operator decision.

| Status | Meaning | Can run proof now? | Can close source bead? |
| --- | --- | --- | --- |
| `runnable` | No known preflight blocker. | Yes | No; proof has not passed yet. |
| `passed` | Existing ledger or attached evidence proves the required lane passed. | Not needed | Only if ledger validation allows it. |
| `source_blocked` | Remote Cargo/rustc/test reached code-owned failure. | No until source owner fixes it | No |
| `test_blocked` | Test, bench, or E2E assertions failed after launch. | No until behavior is fixed | No |
| `infra_blocked` | RCH, worker, shell, sync, substrate, timeout, or artifact retrieval blocked proof. | No until infrastructure is fixed | No |
| `dirty_tree_blocked` | Dirty files overlap the intended proof or are likely to invalidate attribution. | No unless the owning Bead agrees | No |
| `ownership_blocked` | The blocker is owned by a known active Bead or agent. | No for the current agent | No |
| `invalid` | The command shape or backend is off-policy for the claimed proof. | No for remote proof | No |
| `skipped_not_proven` | A required predicate is absent and the lane intentionally skipped. | No for the skipped claim | No |
| `inconclusive` | Evidence is incomplete or contradictory. | Unknown; rerun or collect logs | No |

## Blocker Taxonomy

Every blocker has a stable `reason_code`, a `blocker_kind`, an owner when
known, and a next action. Reason codes are dot-separated and lowercase.

```rust
pub struct ProofDoctorBlocker {
    pub blocker_kind: ProofDoctorBlockerKind,
    pub reason_code: String,
    pub severity: ProofDoctorSeverity,
    pub owner: Option<ProofDoctorOwner>,
    pub affected_paths: Vec<String>,
    pub evidence_keys: Vec<String>,
    pub message: String,
    pub next_action: String,
}
```

### Blocker Kinds

| Kind | Use when | Ledger mapping |
| --- | --- | --- |
| `rch_tooling` | Installed or selected RCH cannot honor required config, cannot launch the remote command, or has a known stale version. | `INFRA_BLOCKED_PRE_CARGO` unless Cargo reached. |
| `worker_capacity` | No reachable worker, wrong hardware predicate, insufficient disk, or worker admission unavailable. | `SKIPPED_NOT_PROVEN` for absent predicate, otherwise infra blocker. |
| `remote_sync` | Repo sync or artifact upload/download failed before Cargo. | `INFRA_BLOCKED_PRE_CARGO`. |
| `remote_launch` | Remote process wrapper or shell failed before Cargo. | `INFRA_BLOCKED_PRE_CARGO`. |
| `remote_substrate` | Cargo started but the worker environment, timeout, package substrate, or artifact retrieval broke the lane. | `INFRA_BLOCKED_POST_CARGO`. |
| `source_compile` | Remote Cargo/rustc reports first-party compile, feature, lint, or build-script errors. | `SOURCE_COMPILE_FAIL`. |
| `test_assertion` | Test binary, bench, E2E, or harness assertion fails. | `TEST_FAIL`. |
| `dirty_tree` | Dirty tracked/untracked paths affect the lane or prevent ownership attribution. | Usually no material ledger attempt; if attempted, `INCONCLUSIVE` unless remote source verdict is clear. |
| `bead_ownership` | Another active Bead or file reservation owns the blocker or overlapping path. | Carry as `next_action`; ledger state depends on material evidence. |
| `command_shape` | Local Cargo, shell-wrapped RCH, fail-open fallback, or unclassified command shape is offered as remote proof. | `LOCAL_INVALID`. |
| `artifact_gap` | Required logs, manifests, metadata, or redaction evidence are missing. | `INCONCLUSIVE` or `INFRA_BLOCKED_POST_CARGO` when Cargo started. |
| `policy` | Repo policy forbids the attempted proof path, backend, or touched files. | `LOCAL_INVALID` or no ledger record if preflight-only. |

### Required Initial Reason Codes

Implementations must cover these v1 reason codes before adding broader ones:

| Reason code | Status | Required evidence |
| --- | --- | --- |
| `proof.rch.stale_external_timeout_config` | `infra_blocked` | Effective RCH config source says external timeout is disabled but installed RCH still launches through the stale timeout path. |
| `proof.rch.pre_cargo_timeout_exec_missing` | `infra_blocked` | RCH selected worker/synced and then emitted `timeout: failed to execute process` before Cargo. |
| `proof.rch.sync_not_proof` | `inconclusive` | Logs show selected worker or sync completion but no Cargo/rustc/test evidence. |
| `proof.rch.remote_cargo_reached` | `runnable` or `inconclusive` | Positive remote Cargo/rustc start evidence, no terminal result yet. |
| `proof.source.remote_compile_error` | `source_blocked` | Remote Cargo/rustc diagnostic points at first-party source. |
| `proof.test.remote_assertion_failed` | `test_blocked` | Test/bench/E2E assertion failed after the intended command started. |
| `proof.command.local_cargo_invalid` | `invalid` | Local Cargo or local fail-open execution is used for an RCH-required lane. |
| `proof.command.shell_wrapped_rch_unclassified` | `invalid` | `rch exec -- bash -lc 'cargo ...'` or equivalent was not positively classified as remote Cargo. |
| `proof.command.rch_cargo_shape_required` | `invalid` | The command is neither a direct RCH Cargo argv nor a recognized local/shell-wrapped invalid shape. |
| `proof.dirty.active_owned_path_overlap` | `dirty_tree_blocked` | Dirty path overlaps lane and maps to an active Bead, reservation, or agent. |
| `proof.dirty.unowned_path_overlap` | `dirty_tree_blocked` | Dirty path overlaps lane but no owner can be identified. |
| `proof.ownership.other_agent_blocker` | `ownership_blocked` | Beads or Agent Mail identifies a different active owner for the blocker. |
| `proof.high_scale.predicate_absent` | `skipped_not_proven` | Required worker hardware or scale predicate is absent. |
| `proof.artifact.required_log_missing` | `inconclusive` | Required command log, metadata sidecar, manifest, or structured log is absent. |
| `proof.redaction.unsafe_missing` | `inconclusive` | Artifacts may contain sensitive data and no redaction status is recorded. |

## Evidence Object

`ProofDoctorEvidence` records what the doctor actually inspected. Unknown
fields must be explicit `null`/`unknown`, not omitted from human reasoning.

```rust
pub struct ProofDoctorEvidence {
    pub rch_binary_path: Option<String>,
    pub rch_version: Option<String>,
    pub rch_config_sources: Vec<ProofDoctorConfigSource>,
    pub selected_worker: Option<String>,
    pub worker_probe_artifact: Option<String>,
    pub sync_duration_ms: Option<u64>,
    pub remote_command_duration_ms: Option<u64>,
    pub wrapper_exit_code: Option<i32>,
    pub remote_exit_code: Option<i32>,
    pub remote_cargo_reached: bool,
    pub rustc_reached: bool,
    pub test_binary_started: bool,
    pub local_cargo_detected: bool,
    pub artifact_retrieval_status: ArtifactRetrievalStatus,
    pub dirty_paths: Vec<ProofDoctorDirtyPath>,
    pub active_beads: Vec<ProofDoctorBeadRef>,
    pub reservations: Vec<ProofDoctorReservationRef>,
    pub artifact_paths: Vec<String>,
}
```

Preflight-only verdicts should leave execution fields empty and focus on
tooling, dirty tree, active Beads, reservations, and command shape.

## Owner Model

Ownership is advisory but must be explicit when used for handoff.

```rust
pub enum ProofDoctorOwner {
    CurrentAgent { agent_name: String, bead_id: Option<String> },
    OtherAgent { agent_name: String, bead_id: Option<String> },
    Bead { bead_id: String, assignee: Option<String> },
    Reservation { agent_name: String, path_pattern: String },
    Unknown,
}
```

Rules:

1. If a dirty path maps to an active file reservation, use the reservation as
   the strongest owner signal.
2. If Beads show an active assignee for the path or parent domain, cite that
   Bead and assignee.
3. If ownership is unknown, do not guess. Use
   `proof.dirty.unowned_path_overlap` and ask for attribution.
4. If the owner is another active agent, the next action is handoff, not local
   source editing.

## Mapping To ProofAttemptRecord

Proof-doctor should create or project a `ProofAttemptRecord` only when a
material proof attempt exists or the implementation needs a durable planned
record. The mapping is:

| Proof-doctor status | ProofState | Notes |
| --- | --- | --- |
| `runnable` | `NOT_RUN` | Optional planned record only; cannot support closure. |
| `passed` | `PASS` | Requires ledger validation: matching backend, remote Cargo evidence for RCH, rustc/assertion flags when required, complete artifacts, and safe redaction. |
| `source_blocked` | `SOURCE_COMPILE_FAIL` | Requires positive remote Cargo/rustc evidence. |
| `test_blocked` | `TEST_FAIL` | Requires assertion execution evidence. |
| `infra_blocked` before Cargo | `INFRA_BLOCKED_PRE_CARGO` | `remote_cargo_reached=false`. |
| `infra_blocked` after Cargo | `INFRA_BLOCKED_POST_CARGO` | `remote_cargo_reached=true`, artifacts incomplete or substrate failed. |
| `dirty_tree_blocked` | none or `INCONCLUSIVE` | Prefer no attempt record for pure preflight. If a run happened and attribution is unclear, use `INCONCLUSIVE`. |
| `ownership_blocked` | same as underlying blocker | Ownership modifies `next_action`; it is not a separate proof state. |
| `invalid` | `LOCAL_INVALID` | No proven or green claim allowed. |
| `skipped_not_proven` | `SKIPPED_NOT_PROVEN` | No high-scale proven claim allowed. |
| `inconclusive` | `INCONCLUSIVE` | Missing data is never a pass. |

The existing `validate_proof_record` invariants are normative. If a
proof-doctor projection would fail validation, the doctor must either lower
the status to `inconclusive` or emit an explicit validation blocker.

## Robot-mode Envelope

The robot command must use the standard robot envelope:

```json
{
  "ok": true,
  "data": {
    "schema_version": 1,
    "verdict": { "...": "..." }
  },
  "elapsed_ms": 12,
  "version": "0.1.0",
  "now": 1777960000
}
```

Rules:

- Domain classification belongs in `data.verdict.status` and
  `data.verdict.blockers[*].reason_code`.
- Transport or argument failures use envelope `ok=false` and an
  `error_code` with the `robot.proof_doctor.*` prefix.
- A proof lane that is blocked or red is still a successful doctor response
  (`ok=true`) if classification succeeded.
- TOON output must be a format transform of the same data, not a smaller
  schema.

Initial robot error codes:

| Error code | Use when |
| --- | --- |
| `robot.proof_doctor.invalid_args` | Command intent cannot be parsed. |
| `robot.proof_doctor.unsupported_scope` | The requested proof scope is unknown. |
| `robot.proof_doctor.repo_unavailable` | Git or repo metadata cannot be read. |
| `robot.proof_doctor.bead_unavailable` | Requested Bead cannot be read. |
| `robot.proof_doctor.internal_error` | Unexpected classifier failure. |

## Command-shape Rules

Valid RCH-required proof intent:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-<purpose>-target cargo test -p <crate> <filter> -- --nocapture
```

Valid fail-closed harness intent:

```bash
run_rch_cargo_logged "$log_file" env CARGO_TARGET_DIR=target/rch-<bead>-<purpose> cargo test ...
```

Invalid for remote proof unless retained metadata proves remote Cargo started:

```bash
cargo test ...
scripts/cargo-local.sh test ...
rch exec -- bash -lc 'cargo test ...'
rch exec -- env CARGO_TARGET_DIR=/tmp/foo bash -lc 'cargo test ...'
```

The invalid forms can still be recorded as local smoke or docs/static proof
when the Bead explicitly allows that scope, but they must not support a remote
Cargo closeout claim.

## Handoff Semantics

Every non-runnable verdict must produce a concise next action:

| Status | Handoff target |
| --- | --- |
| `source_blocked` | Bead/agent owning the first-party file or compile lane. |
| `test_blocked` | Bead/agent owning the failing behavior or test harness. |
| `infra_blocked` | RCH/tooling owner when known, otherwise current operator blocks the proof Bead. |
| `dirty_tree_blocked` | Owner of the dirty path if known; otherwise Beads comment asking for attribution. |
| `ownership_blocked` | Other active owner; current agent should not edit the owned file. |
| `invalid` | Current agent fixes command shape or records proof as local-only. |
| `skipped_not_proven` | Operator supplies missing predicate or marks the claim unproven. |
| `inconclusive` | Current agent reruns with required logging or records the evidence gap. |

Beads comments and Agent Mail messages generated from the same verdict must
include:

- Bead id.
- Exact attempted or intended command.
- Whether RCH sync completed.
- Whether remote Cargo/rustc/test execution was positively observed.
- First blocker reason code.
- Owner and next action when known.
- Explicit statement of what may not be claimed.

## Example Verdicts

### RCH sync completed but Cargo did not start

```json
{
  "schema_version": 1,
  "status": "infra_blocked",
  "phase": "terminal_classified",
  "blockers": [
    {
      "blocker_kind": "remote_launch",
      "reason_code": "proof.rch.pre_cargo_timeout_exec_missing",
      "message": "RCH selected a worker and synced, then timeout failed before Cargo started.",
      "next_action": "Block the proof lane on RCH tooling; do not claim source pass or fail."
    }
  ],
  "evidence": {
    "selected_worker": "vmi1152480",
    "sync_duration_ms": 176008,
    "wrapper_exit_code": 127,
    "remote_cargo_reached": false,
    "rustc_reached": false,
    "test_binary_started": false
  }
}
```

Ledger projection: `ProofState::InfraBlockedPreCargo`.

### Patched RCH reached rustc and found source drift

```json
{
  "schema_version": 1,
  "status": "source_blocked",
  "phase": "terminal_classified",
  "blockers": [
    {
      "blocker_kind": "source_compile",
      "reason_code": "proof.source.remote_compile_error",
      "owner": {
        "type": "bead",
        "bead_id": "ft-lmg3g.6",
        "assignee": "MagentaFalcon"
      },
      "affected_paths": [
        "crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs"
      ],
      "message": "Remote rustc reached first-party code and reported a missing field initializer.",
      "next_action": "Handoff to the owning chaos Bead; current proof lane remains blocked."
    }
  ],
  "evidence": {
    "remote_cargo_reached": true,
    "rustc_reached": true,
    "remote_exit_code": 101
  }
}
```

Ledger projection: `ProofState::SourceCompileFail`.

### Dirty active file overlaps proof scope

```json
{
  "schema_version": 1,
  "status": "dirty_tree_blocked",
  "phase": "preflight",
  "blockers": [
    {
      "blocker_kind": "dirty_tree",
      "reason_code": "proof.dirty.active_owned_path_overlap",
      "owner": {
        "type": "bead",
        "bead_id": "ft-1grhq.2",
        "assignee": "CoralBeaver"
      },
      "affected_paths": [
        "crates/frankenterm-core/src/storage.rs"
      ],
      "message": "The intended proof overlaps active storage scheduler edits owned by another Bead.",
      "next_action": "Do not run or claim the proof until the owner lands or releases the path."
    }
  ]
}
```

Ledger projection: none for pure preflight.

## Tests Required By Later Beads

Later implementation and test beads must cover:

1. Unit classifiers for all required initial reason codes.
2. Golden JSON and TOON robot payloads for `runnable`, `infra_blocked`,
   `source_blocked`, `dirty_tree_blocked`, `invalid`, `skipped_not_proven`,
   and `inconclusive`.
3. Projection tests that `ProofDoctorVerdict` maps to valid
   `ProofAttemptRecord` values only when ledger invariants are satisfied.
4. E2E fixtures for:
   - stale installed RCH external-timeout behavior,
   - patched RCH reaching remote rustc and surfacing source failure,
   - dirty active path ownership blocking unrelated proof,
   - clean runnable lane before execution,
   - pass evidence attaching after a real remote proof.
5. Handoff text tests for Beads and Agent Mail templates.

All Cargo-backed verification for these beads must use RCH with a bead-specific
target dir. Fixture-only or docs-only checks must label themselves as static
proof and must not claim source health.

## Acceptance Checklist

- Infrastructure, source, test, ownership, dirty-tree, fallback, skip, and
  inconclusive states have distinct machine statuses and reason codes.
- Every blocker has an operator-facing message and a machine-readable
  `reason_code`.
- The schema reuses `ProofScope`, `ProofBackend`, `ArtifactRetrievalStatus`,
  `ProofState`, and `ProofAttemptRecord` instead of duplicating ledger state.
- Robot-mode output keeps the standard envelope and reserves
  `robot.proof_doctor.*` only for transport/classifier failures.
- Handoff semantics say exactly who acts next and what cannot be claimed.
- Later implementation beads have concrete unit, golden, and E2E proof
  requirements.
