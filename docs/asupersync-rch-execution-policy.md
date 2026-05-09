# RCH Proof Ledger Execution Policy

**Bead:** `ft-kvs1e`
**Version:** `3.1.0`
**Status:** Active proof-ledger foundation
**Worker-targeted extension:** `ft-ilxky.1`

## Purpose

This policy removes ambiguity in FrankenTerm validation runs by requiring `rch`
for heavy compute workloads and a standard proof-ledger evidence contract for
every verifier run that future agents cite as proof. Agents should cite
validated proof-ledger artifacts instead of treating RCH setup, sync, or
transfer chatter as proof that a verifier ran.

## Scope

Applies to all `ft-*` FrankenTerm beads whenever commands are expected to
create material CPU/IO contention (build/test/bench/soak workloads). It
generalizes the original asupersync migration policy and carries forward the
closed `ft-uz2gg` proof-lane requirement that RCH closeouts identify the target
directory lifecycle instead of leaving temp-target state ambiguous.

The historical file names still contain `asupersync` because this contract
started in the asupersync migration lane. The contract is now repository-wide:
normal `ft-*` bead IDs are accepted, and the schema is no longer restricted to
`ft-e34d9.10.*`.

## Heavy vs Light Classifier

Commands are classified as follows:

| Category | Command examples | rch required |
|---|---|---|
| Heavy | `cargo check`, `cargo build`, `cargo test`, `cargo clippy`, `cargo bench`, `cargo run`, `cargo install`, soak/perf loops that invoke Cargo repeatedly | Yes |
| Light | `cargo fmt --check`, `cargo metadata`, `cargo locate-project`, docs/scripts that do not compile/test | No |

Classifier implementation is canonical in:

- `scripts/validate_asupersync_rch_execution_policy.sh --classify "<cmd>"`

The classifier is intentionally strict: setup or sync chatter that merely
mentions `rch` is not proof that Cargo ran remotely.

## Mandatory Rule

For heavy commands, execution must use one of the validator-recognized
RCH-backed execution shapes:

```bash
rch exec -- <command>
VAR=value rch exec -- <command>
```

or the shared fail-closed harness helpers:

```bash
run_rch_cargo_logged <log-file> env CARGO_TARGET_DIR=<repo-relative-target> cargo <args...>
run_rch_cargo_logged_with_timeout <seconds> <log-file> env CARGO_TARGET_DIR=<repo-relative-target> cargo <args...>
```

The helper forms are evidence-equivalent to direct `rch exec -- ...` because
they route through `tests/e2e/lib_rch_guards.sh`, reject local fallback markers,
and emit RCH metadata sidecars.

RCH status checks, worker probes, sync logs, or shell wrappers that merely echo
`rch exec -- ...` are not proof. If the material Cargo command is local, the
ledger must classify it as a fallback and include approval metadata.

## Worker-Targeted Proof Contract

Worker-targeted proof is required when a bead or closeout claim is about a
specific RCH worker, source mirror, remote target directory, or per-worker
failure mode. A green run on a different worker can still be useful evidence,
but it is not equivalent to target-worker proof.

### Confidence Levels

| Level | Meaning | May close a worker-specific blocker? |
|---|---|---|
| `target_worker_remote_proof` | The material command ran on the named worker, reached remote Cargo/rustc when required, and retained worker/mirror metadata. | Yes, if the command itself passed and artifacts validate. |
| `target_worker_mirror_attestation` | The named worker was checked for repo snapshot and required tracked files, but no material Cargo proof ran. | No; it can unblock or diagnose a later proof run. |
| `scheduler_selected_remote_proof` | RCH selected a worker and the retained metadata identifies it, but the command did not require that exact worker up front. | Only for non-worker-specific source/test claims, or as weaker supporting evidence with residual risk. |
| `worker_self_test_only` | `rch check`, `rch status`, `rch workers probe`, SSH reachability, or smoke preflight ran without the material proof command. | No; this proves availability only. |
| `sync_or_transfer_only` | Logs show source sync, transfer, detached process setup, queue placement, or wrapper startup, but not Cargo/rustc/test execution. | No. |
| `inconclusive_worker_evidence` | The selected worker, repo snapshot, target dir, mirror status, or remote Cargo evidence is absent or contradictory. | No; rerun or collect better artifacts. |

### Required Evidence

When a claim depends on a specific worker, the proof-ledger entry, Beads
comment, or retained artifact bundle must include all of these fields or
explicitly classify the result as `inconclusive_worker_evidence`:

1. The intended worker id or host label, plus the observed selected worker when
   RCH reports one.
2. The intended command class: heavy remote Cargo, light local check, preflight,
   mirror attestation, or approved fallback.
3. Queue or admission state when available: ready, busy/waiting, unhealthy,
   unsupported worker selection, queue timeout, or unknown.
4. Repository snapshot: branch, HEAD commit, project path, and whether the
   worker source tree matched that snapshot.
5. Source mirror status: required tracked files present, missing, stale, or not
   checked. Missing tracked files are infrastructure blockers, not source
   failures.
6. Target directory and lifecycle, including whether it was retained,
   inventory-only, or not applicable.
7. Elapsed time, wrapper exit status, remote exit status when available, and
   whether Cargo, rustc, and the test binary were actually reached remotely.
8. Retained artifact paths for logs, metadata sidecars, mirror attestation, and
   proof-ledger JSONL.
9. Residual risk notes when proof was scheduler-selected rather than
   target-worker-enforced.

Until the proof-ledger schema grows first-class worker-targeted fields, record
the extra worker evidence in `worker_context`, `artifact_paths`, and
`residual_risk_notes` with stable fingerprints. Do not silently promote those
records to target-worker proof.

### Allowed And Forbidden Incident Actions

Allowed during worker-specific RCH incidents:

- Read-only `rch status`, `rch check`, worker probe, Beads, git, and log
  inspection.
- A material proof command through `rch exec -- ...` or a fail-closed harness.
- A read-only mirror attestation that checks HEAD and tracked file presence on
  the selected worker.
- Reopening or blocking a bead with exact artifact paths and reason codes when
  the worker-specific proof cannot be trusted.

Forbidden during worker-specific RCH incidents:

- Restarting, repairing, stopping, or killing Agent Mail, RCH, or shared
  services.
- Running local heavy Cargo and calling it remote proof.
- Draining or mutating shared workers just to make a proof easier to schedule.
- Deleting remote source files, target dirs, or local files to manufacture a
  negative test.
- Treating sync chatter, worker health, queue placement, or smoke preflight as
  evidence that the source/test command passed.

### Closeout Examples

Valid closeout evidence for a worker-specific blocker:

- `target_worker_remote_proof` on the named worker.
- Remote Cargo/rustc reached when the command class requires it.
- Required tracked files were present on that worker at the intended HEAD.
- Proof artifacts validate, and the Beads closeout cites the exact command,
  worker, target dir, metadata sidecar, and ledger entry.

Valid diagnostic evidence that is not enough to close:

- `target_worker_mirror_attestation` showing a missing tracked file. This can
  block the proof lane as an infrastructure/mirror issue, but it does not prove
  the source behavior under test.
- `scheduler_selected_remote_proof` on the same worker when the command was not
  explicitly target-worker-enforced. This may reduce residual risk but must be
  described as scheduler-selected evidence.
- `worker_self_test_only` from `rch check` or `rch status`. This only proves the
  substrate was partly reachable.

Required reopen triggers:

- A later run reports `RCH-REMOTE-MIRROR-MISSING-FILE` or equivalent missing
  tracked source on the same worker.
- A closeout claimed target-worker proof but retained artifacts lack the
  selected worker or repo snapshot.
- Local fallback markers appear in a proof cited as remote RCH evidence.

## Local Fallback Rule

Local fallback for heavy commands is allowed only when all are true:

1. `rch` is unavailable or remote workers are unhealthy.
2. Evidence entry includes non-empty `fallback_reason_code`.
3. Evidence entry includes non-empty `fallback_approved_by`.
4. `execution_mode` is `approved_local_fallback`.
5. `validation_status` is `approved_fallback`.
6. Residual risk note explains impact on comparability/reproducibility.

## Evidence Contract

Every proof-ledger run must be logged with fields:

1. `timestamp`
2. `command`
3. `command_fingerprint`
4. `command_class` (`heavy` or `light`)
5. `is_heavy`
6. `used_rch`
7. `worker_context`
8. `worker_context_fingerprint`
9. `execution_mode` (`remote_rch`, `local_light`, or `approved_local_fallback`)
10. `target_dir`
11. `target_dir_fingerprint`
12. `target_dir_lifecycle` (`not_applicable`, `retained`, `inventory_only`, or `cleanup_approved`)
13. `artifact_paths` (each path must exist when evidence is validated)
14. `artifact_paths_fingerprint`
15. `elapsed_seconds`
16. `exit_status`
17. `residual_risk_notes`
18. `residual_risk_notes_fingerprint`
19. `validation_status` (`valid` or `approved_fallback`)
20. Optional fallback fields when a heavy run is not confirmed as remote RCH:
   - `fallback_reason_code`
   - `fallback_approved_by`

Heavy runs must include a real `target_dir` and a non-`not_applicable`
`target_dir_lifecycle`. Light local checks should use `target_dir:
"not_applicable"` and `target_dir_lifecycle: "not_applicable"`.

Ledger-visible fields must already be redacted before they are written or
cited. The validator rejects known provider tokens, bearer/JWT-like secrets,
credential-looking key/value pairs, and SSH private-key paths in command,
worker context, target-dir, artifact-path, and residual-risk fields. The
`*_fingerprint` fields preserve stable correlation without requiring agents to
print raw sensitive values.

Machine-readable schema:

- `docs/asupersync-rch-evidence-schema.json` (`schema_version: 3`)

## Validation Tooling

Policy validator:

```bash
bash scripts/validate_asupersync_rch_execution_policy.sh --self-test
bash scripts/validate_asupersync_rch_execution_policy.sh --classify "cargo test --workspace"
bash scripts/validate_asupersync_rch_execution_policy.sh --redact-text "API_KEY=... cargo test"
bash scripts/validate_asupersync_rch_execution_policy.sh --validate-evidence <path-to-evidence.json>
bash scripts/validate_asupersync_rch_execution_policy.sh --aggregate-ledger <path-to-ledger.jsonl> [more-ledgers.jsonl ...]
```

E2E policy validation:

```bash
bash tests/e2e/test_asupersync_rch_execution_policy.sh
```

Shared RCH wrapper emission is opt-in. Harnesses that source
`tests/e2e/lib_rch_guards.sh` can set:

```bash
RCH_PROOF_LEDGER_FILE=tests/e2e/logs/<run>/proof-ledger.jsonl
RCH_PROOF_LEDGER_BEAD_ID=ft-...
RCH_PROOF_LEDGER_SCENARIO_ID=<scenario>
```

When those variables are present, `ensure_rch_ready`,
`run_rch_cargo_logged`, and `run_rch_cargo_logged_with_timeout` append
schema-v3 proof-ledger JSONL entries for probe/smoke and remote Cargo logs.
Successful remote entries validate directly. Fail-open local fallback,
timeouts, non-zero wrapper exits, missing metadata, or unredacted public fields
produce entries that are intentionally not valid proof.

Aggregate quality gate:

- `--aggregate-ledger` scans one or more proof-ledger JSONL files, validates
  every run in every retained evidence object, and emits an operator report.
- Each row carries the bead ID, scenario ID, command, worker context, artifact
  path(s), category, and stable reason code so Beads and release-readiness
  comments can cite the exact proof shape.
- Categories are:
  - `proven_remote`: heavy Cargo proof ran through an RCH-recognized remote path.
  - `light_local`: local non-heavy checks such as formatting.
  - `approved_fallback`: heavy local fallback with explicit approval metadata.
  - `rejected_local_heavy`: heavy local proof without required fallback approval.
  - `malformed`: invalid JSON, stale schema, missing required fields, or malformed bead IDs.
  - `missing_artifact`: an artifact path named by evidence is not retained.
  - `residual_risk_only`: otherwise valid evidence with residual risk notes.
- Blocking categories are `rejected_local_heavy`, `malformed`, and
  `missing_artifact`. They make the aggregate command exit non-zero.
- `approved_fallback` and `residual_risk_only` produce an `overall_verdict` of
  `partial_risk`; this is acceptable evidence only when the operator-facing
  closeout names the residual risk instead of claiming a clean pass.

The self-test and E2E cover accepted remote proof, rejected local-heavy proof,
accepted human-approved fallback, light local commands, stale schema versions,
malformed bead IDs, missing artifacts, missing required booleans, unredacted
provider tokens, SSH-style secret paths, wrapper-emitted ledger records, RCH
setup chatter, shell wrappers that mention RCH while running Cargo locally, and
aggregate quality-gate classification for mixed, missing, rejected, and
malformed proof-ledger JSONL.

## User Impact

1. Prevents accidental local compilation storms from degrading operator session responsiveness.
2. Preserves reproducibility and auditability for migration, storage, fleet, GUI, search, release, and future proof lanes.
3. Makes degraded-mode exceptions explicit and reviewable instead of implicit.
