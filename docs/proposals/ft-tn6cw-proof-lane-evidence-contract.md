# Proof-lane evidence contract (ft-tn6cw.2)

## Status

Contract for `ft-tn6cw.2`. This document defines how FrankenTerm
classifies build/test proof attempts, what evidence a proof record must
carry, and which claims agents may make from each proof state.

This is not an implementation. It is the truthfulness boundary for
`ft-tn6cw.3` through `ft-tn6cw.5`, and for any resource-autopilot,
storage-IO, chaos, or auto-tuning closeout that cites remote proof.

## Current Ground Truth

The repo requires CPU-intensive Cargo proof to run through `rch` so
large agent swarms do not overwhelm the local machine. On 2026-05-05,
multiple agents observed a failure mode where RCH sync and worker
selection succeeded but the remote command failed before Cargo started:

```text
timeout: failed to execute process: No such file or directory (os error 2)
```

The same investigation also showed that some shell-wrapped `rch exec`
forms can start local Cargo after RCH reports that the command is not a
remote compilation job. That evidence is invalid for FrankenTerm
closeout even if the local compile reaches rustc.

Important active references:

| Surface | Current anchor | Evidence relevance |
| --- | --- | --- |
| RCH wrapper blocker | `ft-tn6cw.1` | Remote proof can fail before Cargo because the external timeout wrapper is broken. |
| Storage IO scheduler proof | `ft-1grhq.2` | Source work can be ready while closeout remains blocked by pre-Cargo RCH infra. |
| Safe auto-tuning proof | `ft-luq3w.1` and children | High-scale benefit claims require explicit proof levels, not reduced-mode smoke. |
| Resource chaos proof | `ft-lmg3g.*` | Chaos reports must distinguish PASS from missing services, missing hardware, and infra blockers. |
| Config Linux blocker | `ft-bvyrc` | Source compile blockers must stay separate from unrelated RCH wrapper failures. |

Successful sync, healthy SSH, worker selection, detached build chatter,
and local Cargo output are not sufficient proof for a remote Cargo
closeout. A valid remote compile/test claim requires evidence that Cargo
or rustc actually ran on an RCH worker.

## Non-Goals

- Do not fix RCH itself in this repo; `ft-tn6cw.1` tracks that blocker.
- Do not edit user-global RCH config as part of proof classification.
- Do not restart or repair Agent Mail, RCH, or other shared services.
- Do not store raw pane content, secrets, or unbounded logs in proof
  records.
- Do not treat this contract as permission to run local Cargo for proof.
- Do not claim 64-core / 256 GiB performance from reduced-mode or
  synthetic runs.

## Proof Attempt State

Every proof attempt has exactly one terminal state.

| State | Meaning | May close source bead? | May claim remote proof? |
| --- | --- | --- | --- |
| `NOT_RUN` | Required proof has not been attempted. | No | No |
| `REACHED_REMOTE_CARGO` | RCH worker reached Cargo/rustc, but final result is not yet classified. | No | Partial only |
| `SOURCE_COMPILE_FAIL` | Remote Cargo/rustc ran and reported a compile error attributable to source or configuration. | No | Yes, as a failing proof |
| `TEST_FAIL` | Remote Cargo ran tests and one or more tests failed. | No | Yes, as a failing proof |
| `PASS` | Required remote command completed successfully and all predicates for the claimed scope passed. | Yes | Yes |
| `INFRA_BLOCKED_PRE_CARGO` | RCH or environment failed before Cargo/rustc started. | No | No |
| `INFRA_BLOCKED_POST_CARGO` | Cargo/rustc started, but infra failed before a source/test verdict could be trusted. | No | Partial only |
| `LOCAL_INVALID` | The command ran, or may have run, local Cargo instead of remote proof. | No | No |
| `SKIPPED_NOT_PROVEN` | Proof was intentionally skipped because a required worker, service, hardware predicate, or fixture was unavailable. | No | No |
| `INCONCLUSIVE` | Logs are insufficient to classify the attempt safely. | No | No |

`REACHED_REMOTE_CARGO` is an intermediate evidence state, not a closeout
state. Implementations may record it while a detached command is still
running, but the final ledger row must become `PASS`, `SOURCE_COMPILE_FAIL`,
`TEST_FAIL`, `INFRA_BLOCKED_POST_CARGO`, or `INCONCLUSIVE`.

## Required Record Fields

Proof records must be machine-readable and stable. Field names below are
the initial schema for implementation beads.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `schema_version` | u32 | yes | Proof record schema version. |
| `proof_id` | string | yes | Stable id or hash for this attempt. |
| `bead_id` | string | yes | Bead whose proof is being recorded. |
| `parent_bead_id` | optional string | no | Epic or parent proof lane. |
| `attempted_at_utc` | string | yes | RFC3339 timestamp. |
| `agent_name` | string | yes | Agent or operator that ran the attempt. |
| `cwd` | string | yes | Working directory. |
| `command` | string array | yes | Exact argv, not shell prose. |
| `declared_target_dir` | optional string | no | `CARGO_TARGET_DIR` when set. |
| `rch_version` | optional string | when RCH is used | RCH version string if known. |
| `rch_config_fingerprint` | optional string | when relevant | Hash or summarized config evidence, not secrets. |
| `selected_worker` | optional string | when known | Worker id or host label. |
| `remote_cargo_reached` | bool | yes | True only when logs prove Cargo/rustc ran remotely. |
| `local_cargo_detected` | bool | yes | True when Cargo ran locally or command shape makes local fallback possible. |
| `exit_code` | optional i32 | no | Process exit status. |
| `state` | string | yes | One proof state from this contract. |
| `reason_code` | string | yes | Stable reason code. |
| `summary` | string | yes | Short operator-facing interpretation. |
| `artifact_paths` | string array | yes | Retained logs, reports, or fixture paths. Empty only when unavailable. |
| `hardware_predicate` | optional string | no | `target_hardware`, `remote_reduced`, `local_reduced`, or `unknown`. |
| `claims_allowed` | string array | yes | Explicit claims allowed by this record. |
| `next_action` | string | yes | What should happen next. |

The command must be represented as argv so implementations can classify
shell-wrapper forms without brittle prose parsing.

## Reason Codes

Reason codes are stable strings for tests, reports, Beads comments, and
operator tooling.

| Prefix | Use |
| --- | --- |
| `proof.pass.*` | Valid proof completed successfully. |
| `proof.source.compile_fail.*` | Remote Cargo/rustc found a source or config compile error. |
| `proof.test.fail.*` | Remote tests executed and failed. |
| `proof.infra.pre_cargo.*` | Infra failed before Cargo/rustc started. |
| `proof.infra.post_cargo.*` | Infra failed after Cargo/rustc started but before a trusted verdict. |
| `proof.local_invalid.*` | Local Cargo ran or command shape is invalid for remote proof. |
| `proof.skipped.*` | Required proof was intentionally skipped and not proven. |
| `proof.inconclusive.*` | Logs cannot support a stronger classification. |

Required suffixes:

| Suffix | Use |
| --- | --- |
| `rch_timeout_wrapper` | RCH external timeout wrapper failed before Cargo. |
| `rch_non_compilation_command` | RCH identified the command as non-compilation. |
| `shell_wrapped_cargo` | Shell wrapper can bypass remote Cargo classification. |
| `worker_unreachable` | Worker cannot be reached. |
| `missing_hardware_predicate` | Target hardware predicate is unavailable. |
| `missing_service` | Required external service or daemon is unavailable. |
| `source_error` | Compiler/test output points to source or config. |
| `artifact_missing` | Required proof artifact was not retained. |

## Truthfulness Rules

1. `PASS` is the only state that can support "passed", "green", or
   "proven" wording for the exact command and proof level recorded.
2. `SOURCE_COMPILE_FAIL` and `TEST_FAIL` are valid remote proof attempts,
   but they are failing proof. The bead remains open or blocked until the
   source/test failure is fixed.
3. `INFRA_BLOCKED_PRE_CARGO` means no compiler/test verdict exists.
   Beads may cite it only as an infrastructure blocker.
4. `INFRA_BLOCKED_POST_CARGO` may prove that remote Cargo started, but it
   cannot prove source correctness.
5. `LOCAL_INVALID` invalidates the attempt for closeout even if local
   output looks useful for diagnosis.
6. `SKIPPED_NOT_PROVEN` is acceptable for reduced reports only when the
   report clearly says the claimed high-scale predicate was not proven.
7. `INCONCLUSIVE` is the safe fallback when logs are partial, truncated,
   or ambiguous.
8. Sync volume, worker SSH health, queue placement, and detached process
   ids are supporting facts, not proof states.

## Command Shape Classification

Implementations under `ft-tn6cw.4` must classify proof commands before
they run when possible, and classify observed output afterward.

Accepted baseline shapes:

```text
rch exec -- cargo check ...
rch exec -- cargo test ...
rch exec -- cargo clippy ...
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-... cargo test ...
```

Rejected or suspicious shapes:

```text
rch exec -- bash -lc 'cargo test ...'
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-... bash -lc 'cargo test ...'
rch exec -- sh -c 'cargo check ...'
```

If RCH emits a warning that the command is non-compilation and then Cargo
starts locally, the attempt must be `LOCAL_INVALID` with
`proof.local_invalid.rch_non_compilation_command`.

## Example Records

Passing remote proof:

```json
{
  "schema_version": 1,
  "proof_id": "proof-ft-example-pass",
  "bead_id": "ft-example",
  "attempted_at_utc": "2026-05-05T05:30:00Z",
  "agent_name": "ExampleAgent",
  "cwd": "/Users/jemanuel/projects/frankenterm",
  "command": ["rch", "exec", "--", "cargo", "test", "-p", "frankenterm-core", "--lib", "proof_fixture"],
  "declared_target_dir": "/tmp/ft-proof-example",
  "rch_version": "1.0.24",
  "rch_config_fingerprint": "redacted-config-sha256",
  "selected_worker": "vmi1149989",
  "remote_cargo_reached": true,
  "local_cargo_detected": false,
  "exit_code": 0,
  "state": "PASS",
  "reason_code": "proof.pass.remote_reduced",
  "summary": "Remote Cargo reached rustc and the focused test passed on an RCH worker.",
  "artifact_paths": ["docs/proof-artifacts/ft-example/pass.json"],
  "hardware_predicate": "remote_reduced",
  "claims_allowed": ["focused_remote_test_passed"],
  "next_action": "Use this only for the focused reduced proof claim."
}
```

Source compile failure:

```json
{
  "schema_version": 1,
  "proof_id": "proof-ft-example-source-fail",
  "bead_id": "ft-example",
  "attempted_at_utc": "2026-05-05T05:35:00Z",
  "agent_name": "ExampleAgent",
  "cwd": "/Users/jemanuel/projects/frankenterm",
  "command": ["rch", "exec", "--", "cargo", "test", "-p", "frankenterm-core", "--lib", "storage::io_scheduler"],
  "declared_target_dir": "/tmp/ft-1grhq-storage-io",
  "rch_version": "1.0.24",
  "rch_config_fingerprint": "redacted-config-sha256",
  "selected_worker": "vmi1149989",
  "remote_cargo_reached": true,
  "local_cargo_detected": false,
  "exit_code": 101,
  "state": "SOURCE_COMPILE_FAIL",
  "reason_code": "proof.source.compile_fail.source_error",
  "summary": "Remote Cargo reached rustc and rustc reported a source compile error.",
  "artifact_paths": ["docs/proof-artifacts/ft-example/source-fail.txt"],
  "hardware_predicate": "remote_reduced",
  "claims_allowed": ["remote_compile_attempted"],
  "next_action": "Fix the source compile error and rerun the same remote proof lane."
}
```

Pre-Cargo RCH infrastructure blocker:

```json
{
  "schema_version": 1,
  "proof_id": "proof-ft-tn6cw-rch-timeout",
  "bead_id": "ft-1grhq.2",
  "parent_bead_id": "ft-tn6cw",
  "attempted_at_utc": "2026-05-05T05:09:46Z",
  "agent_name": "CoralBeaver",
  "cwd": "/Users/jemanuel/projects/frankenterm",
  "command": ["rch", "exec", "--", "env", "CARGO_TARGET_DIR=/tmp/ft-1grhq-storage-io", "cargo", "test", "-p", "frankenterm-core", "--lib", "storage::io_scheduler"],
  "declared_target_dir": "/tmp/ft-1grhq-storage-io",
  "rch_version": "1.0.24",
  "rch_config_fingerprint": "external_timeout_enabled=true",
  "selected_worker": "vmi1149989",
  "remote_cargo_reached": false,
  "local_cargo_detected": false,
  "exit_code": 127,
  "state": "INFRA_BLOCKED_PRE_CARGO",
  "reason_code": "proof.infra.pre_cargo.rch_timeout_wrapper",
  "summary": "RCH synced to a worker but the timeout wrapper failed before Cargo started.",
  "artifact_paths": [],
  "hardware_predicate": "unknown",
  "claims_allowed": ["infra_blocker_observed"],
  "next_action": "Fix or bypass the RCH timeout wrapper with explicit approval, then rerun the remote proof lane."
}
```

Invalid local fallback:

```json
{
  "schema_version": 1,
  "proof_id": "proof-ft-local-invalid-shell-wrapper",
  "bead_id": "ft-tn6cw.4",
  "attempted_at_utc": "2026-05-05T05:10:30Z",
  "agent_name": "CoralBeaver",
  "cwd": "/Users/jemanuel/projects/frankenterm",
  "command": ["rch", "exec", "--", "bash", "-lc", "cargo test -p frankenterm-core --lib storage::io_scheduler"],
  "declared_target_dir": "/tmp/ft-1grhq-storage-io",
  "rch_version": "1.0.24",
  "rch_config_fingerprint": "unknown",
  "selected_worker": null,
  "remote_cargo_reached": false,
  "local_cargo_detected": true,
  "exit_code": null,
  "state": "LOCAL_INVALID",
  "reason_code": "proof.local_invalid.rch_non_compilation_command",
  "summary": "RCH treated the shell wrapper as a non-compilation command and Cargo started locally.",
  "artifact_paths": [],
  "hardware_predicate": "local_reduced",
  "claims_allowed": ["diagnostic_only"],
  "next_action": "Stop local Cargo and rerun using a direct remote Cargo command shape."
}
```

## Test and Logging Requirements

Implementation beads must include:

1. Unit tests for every proof state and required reason-code prefix.
2. Command-shape tests for accepted direct RCH Cargo forms and rejected
   shell-wrapper forms.
3. Fixture logs for the 2026-05-05 external-timeout failure and
   local-invalid fallback pattern.
4. A multi-bead report test that contains at least one pass, one source
   failure, one pre-Cargo infra blocker, one skipped high-scale predicate,
   and one local-invalid attempt.
5. Detailed classification logs that show the parsed argv, detected RCH
   phase, whether remote Cargo was reached, final state, and allowed
   claims.

Tests must assert stable states and reason codes. They should not rely on
full raw stderr prose unless the prose is part of a fixture being parsed.

## Closeout Rules for Future Beads

When a bead cites proof:

1. Include the proof state, reason code, command, and artifact path.
2. Say whether the proof is `local_reduced`, `remote_reduced`, or
   `target_hardware`.
3. If the state is not `PASS`, mark the bead blocked or explain why the
   proof was diagnostic-only.
4. If the state is `PASS` but the hardware predicate is reduced, avoid
   high-scale wording.
5. If RCH is blocked before Cargo, cite `ft-tn6cw.1` or the active RCH
   blocker rather than inventing a source failure.

This keeps the swarm moving without letting communication artifacts,
infrastructure failures, or local fallbacks masquerade as verified code.
