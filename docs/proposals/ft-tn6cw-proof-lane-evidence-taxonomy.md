# Proof-lane evidence taxonomy and ledger contract (ft-tn6cw.2)

## Status

Contract for `ft-tn6cw.2`. This document defines the proof-lane state
taxonomy, required ledger fields, and truthfulness rules that later
implementation beads must follow when they record Cargo, test, benchmark, E2E,
and high-scale swarm proof attempts.

This is not an implementation. It is the evidence boundary for `ft-tn6cw.3`,
`ft-tn6cw.4`, and downstream closeout/reporting work.

## Current Ground Truth

Existing repo contracts already require remote and artifact-backed proof:

| Surface | Current anchor | Contract relevance |
| --- | --- | --- |
| Finish-line proof | `docs/ft-xbnl0-verification-contract.md` | Requires exact commands, retained artifacts, `rch exec -- ...` for heavy Cargo verification, and honest artifact paths before closing implementation beads. |
| Test artifact logs | `docs/test-logging-contract.md` | Defines `summary.json`, `structured.log`, command capture, redaction, and `*.rch_meta.json` sidecars for RCH-aware harnesses. |
| RCH fail-closed shell library | `tests/e2e/lib_rch_guards.sh` | Existing extraction and metadata precedent for selected worker, worker probe, sync duration, remote exit code, wrapper exit code, timeout, and fail-open detection. |
| High-core proof | `docs/high-core-swarm-runbook.md` | Separates local or undersized smoke from real 64-core / 256 GiB proof and requires `skipped_not_proven` when the predicate is absent. |
| Capacity baseline artifacts | `docs/perf/swarm-capacity-baseline.md` | Requires command, environment, summary, and failure context. Missing data is never a pass. |

The 2026-05-05 proof-lane incident adds two concrete invalid or blocked
patterns that this contract must preserve:

1. Direct RCH proof command:

   ```bash
   rch exec -- env CARGO_TARGET_DIR=/tmp/ft-luq3w-target \
     cargo test -p frankenterm-core --lib --no-default-features auto_tune -- --nocapture
   ```

   The run selected worker `vmi1152480`, completed repo sync, and then failed
   before Cargo started with:

   ```text
   timeout: failed to execute process: No such file or directory (os error 2)
   ```

   This is `INFRA_BLOCKED_PRE_CARGO`. It is not a source compile failure, test
   failure, or pass.

2. Shell-wrapped RCH proof command:

   ```bash
   rch exec -- env CARGO_TARGET_DIR=/tmp/ft-luq3w-target \
     bash -lc 'cargo test -p frankenterm-core --lib --no-default-features auto_tune -- --nocapture'
   ```

   RCH warned that the command was not classified as a compilation command, and
   the shape can fall out of the remote Cargo proof lane. Any Cargo result from
   this shape is `LOCAL_INVALID` for FrankenTerm remote proof closeout unless a
   later guard proves the command still reached remote Cargo.

Successful RCH sync, worker SSH health, selected-worker chatter, command echo,
artifact retrieval text, or a chat summary is not proof that Cargo or rustc
ran.

## Non-Goals

- Do not fix RCH or its timeout wrapper here. That remains `ft-tn6cw.1`.
- Do not implement the proof ledger or operator report surface here. That is
  `ft-tn6cw.3`.
- Do not add command-shape linting or runtime guardrails here. That is
  `ft-tn6cw.4`.
- Do not replace `docs/test-logging-contract.md` or
  `tests/e2e/lib_rch_guards.sh`; extend them in later beads if needed.
- Do not treat local docs-only validation, such as `git diff --check`, as a
  Cargo proof lane. It can validate a docs bead, but it cannot prove source
  build/test health.

## Proof State Taxonomy

`proof_state` is the primary machine-readable field. The values below are the
complete v1 state set.

| State | Terminal | Meaning | Source verdict? | Closeout implication |
| --- | --- | --- | --- | --- |
| `NOT_RUN` | No | A required proof lane was planned but no command was launched. | No | Cannot support implementation closure. |
| `REACHED_REMOTE_CARGO` | No | RCH dispatched the command and logs prove Cargo or rustc started remotely, but the final result is not known yet. | No | Wait for terminal state or mark inconclusive. |
| `SOURCE_COMPILE_FAIL` | Yes | Remote Cargo/rustc was reached and failed on first-party source, type, lint, feature, or test-build compilation errors. | Yes, red | Source must be fixed or bead remains red. |
| `TEST_FAIL` | Yes | Remote Cargo built enough to run tests or benches, and the test/bench/E2E assertion failed. | Yes, red | Behavior must be fixed or bead remains red. |
| `PASS` | Yes | The required command reached the intended backend, returned success, and retained enough artifacts for the claimed scope. | Yes, green | May support closure if the scope matches the bead. |
| `INFRA_BLOCKED_PRE_CARGO` | Yes | RCH, worker selection, sync, command classification, timeout wrapper, shell setup, or remote process launch failed before Cargo started. | No | Mark blocked on infrastructure or wrapper; do not claim source pass/fail. |
| `INFRA_BLOCKED_POST_CARGO` | Yes | Cargo or rustc started remotely, but infrastructure, worker environment, artifact retrieval, remote package substrate, or wrapper behavior prevented complete evidence. | Usually no | Record what was reached, but do not overstate artifact-complete proof. |
| `LOCAL_INVALID` | Yes | A local Cargo run, fail-open fallback, shell-wrapped RCH command, or other off-policy command is being offered as remote proof. | No | Invalid for remote proof closeout; may only be cited as local smoke if labeled that way. |
| `SKIPPED_NOT_PROVEN` | Yes | The lane intentionally skipped because prerequisites were absent, such as target hardware, disk, worker health, feature flags, or an explicit predicate. | No | Not a failure, but cannot support the skipped claim. |
| `INCONCLUSIVE` | Yes | Logs are missing, contradictory, truncated before classification, or lack enough metadata to distinguish source, infra, and local execution. | No | Treat as unproven and rerun or block with evidence gap. |

`REACHED_REMOTE_CARGO` is intentionally non-terminal. It exists so streaming or
multi-step reports can say "remote Cargo started" without prematurely calling
the lane green or red.

## State Classification Rules

Classifiers must use positive evidence for the strongest claim.

1. Set `PASS` only when all of these are true:
   - the command shape is allowed for the proof lane,
   - the intended backend was reached,
   - Cargo, rustc, the test binary, the bench binary, or the E2E harness
     actually ran as required,
   - the relevant process returned success,
   - artifacts required by the bead scope were retained or explicitly
     declared not required for the docs-only scope.
2. Set `SOURCE_COMPILE_FAIL` when remote Cargo/rustc was reached and the
   primary diagnostic is first-party source, feature, lint, or build-script
   code under the repo's responsibility.
3. Set `TEST_FAIL` when a test, proptest, bench assertion, E2E assertion, or
   harness verification failed after the target command started correctly.
4. Set `INFRA_BLOCKED_PRE_CARGO` when `cargo_process_started=false`. Examples:
   unavailable workers, sync failure, remote timeout wrapper cannot exec,
   command classification rejected before launch, missing timeout binary before
   a harness can run, or shell setup failure before Cargo.
5. Set `INFRA_BLOCKED_POST_CARGO` when Cargo/rustc started remotely but proof
   cannot complete because of worker substrate, artifact retrieval, remote
   mirror drift, missing system packages on the worker, remote stall, or wrapper
   failure after material execution began.
6. Set `LOCAL_INVALID` when logs show fail-open local execution, `running
   locally`, `[RCH] local`, local Cargo without RCH for a remote-required lane,
   or shell-wrapped command shapes that RCH did not classify as remote Cargo.
7. Set `SKIPPED_NOT_PROVEN` only for explicit predicates. Missing hardware,
   disk, worker capacity, feature flags, or skipped smoke preflight can explain
   the skip, but they do not imply green or red source status.
8. Set `INCONCLUSIVE` when evidence is insufficient. Missing data is never a
   pass.

If several rules match, choose the most truth-preserving state in this order:

```text
LOCAL_INVALID
INFRA_BLOCKED_PRE_CARGO
INFRA_BLOCKED_POST_CARGO
SOURCE_COMPILE_FAIL or TEST_FAIL
PASS
SKIPPED_NOT_PROVEN
INCONCLUSIVE
```

This priority prevents a local fallback, wrapper failure, or artifact gap from
being hidden behind later success-looking text.

## Allowed And Invalid Command Shapes

The direct form is the normal remote proof shape:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-<purpose>-target \
  cargo test -p <crate> <filter> -- --nocapture
```

The fail-closed harness form is also valid when it uses
`tests/e2e/lib_rch_guards.sh`:

```bash
run_rch_cargo_logged "$log_file" \
  env CARGO_TARGET_DIR="target/rch-<bead>-<purpose>" cargo test ...
```

These shapes are invalid for remote Cargo proof unless a later guard records
positive remote-Cargo evidence:

```bash
rch exec -- bash -lc 'cargo test ...'
rch exec -- env CARGO_TARGET_DIR=/tmp/foo bash -lc 'cargo test ...'
cargo test ...
scripts/cargo-local.sh test ...
```

Local commands may still be valid for docs-only, shell-syntax, formatting,
static diff, or explicitly local smoke lanes. They must not be described as
remote Cargo proof.

## Ledger Record Schema

Every implementation in `ft-tn6cw.3` must persist one record per material proof
attempt. The record must be redaction-safe and stable enough for Beads comments,
release reports, and operator dashboards.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `schema_version` | u32 | Yes | Must be `1` for this contract. |
| `proof_id` | string | Yes | Stable id or hash for this attempt. |
| `bead_id` | string | Yes | Owning Beads issue, for example `ft-luq3w.1`. |
| `parent_bead_id` | string or null | No | Parent epic or proof program when useful. |
| `created_at_utc` | string | Yes | ISO 8601 timestamp when the attempt started. |
| `finished_at_utc` | string or null | Yes | ISO 8601 timestamp when terminal, else null. |
| `actor` | string | Yes | Agent or operator identity. |
| `repo_path` | string | Yes | Absolute repo path where command was launched. |
| `git_head` | string | Yes | Full or short HEAD used by the attempt. |
| `branch` | string | Yes | Branch name; FrankenTerm proof normally expects `main`. |
| `dirty_tree_summary` | string or null | Yes | Human-safe summary of owned/unowned dirty state. |
| `command` | string | Yes | Exact command as launched. |
| `command_argv` | string array | Recommended | Tokenized command when available. |
| `working_dir` | string | Yes | Working directory for launch. |
| `target_dir` | string or null | Yes | `CARGO_TARGET_DIR` or null. |
| `proof_scope` | string | Yes | `docs_static`, `cargo_check`, `cargo_clippy`, `cargo_test`, `cargo_bench`, `e2e`, `release_gate`, or `high_scale`. |
| `required_backend` | string | Yes | `rch`, `local_shell`, `ci`, or `none`. |
| `observed_backend` | string | Yes | Backend actually observed. |
| `rch_version` | string or null | If RCH | `rch --version` output when available. |
| `rch_config_digest` | string or null | If RCH | Redaction-safe digest or named config profile. |
| `selected_worker` | string or null | If RCH | Worker id parsed from RCH logs. |
| `worker_probe_artifact` | string or null | If RCH | Path to probe log or metadata. |
| `sync_duration_ms` | u64 or null | If known | RCH sync duration. |
| `remote_command_duration_ms` | u64 or null | If known | RCH remote command duration. |
| `wrapper_exit_code` | i32 or null | Yes | Exit code from RCH, timeout, or harness wrapper. |
| `remote_exit_code` | i32 or null | If known | Remote command exit code. |
| `cargo_process_started` | bool | Yes | True only with log evidence that Cargo started on intended backend. |
| `rustc_process_started` | bool | Yes | True only with log evidence that rustc or Cargo build execution started. |
| `test_binary_started` | bool | Yes | True only when tests/benches/E2E material assertions began. |
| `artifact_retrieval_status` | string | Yes | `not_applicable`, `not_started`, `complete`, `partial`, `stalled`, or `failed`. |
| `proof_state` | string | Yes | One taxonomy value from this document. |
| `reason_code` | string | Yes | Stable reason code. |
| `operator_interpretation` | string | Yes | One sentence suitable for Beads comments. |
| `safe_to_close` | bool | Yes | Whether this attempt can support closing the owning bead. |
| `high_scale_predicate` | string or null | If high-scale | `proven_predicate_met`, `skipped_not_proven`, or null. |
| `stdout_artifact` | string or null | Yes | Path to retained stdout/log. |
| `stderr_artifact` | string or null | Yes | Path to retained stderr/log. |
| `structured_log_artifact` | string or null | Recommended | Path to JSONL structured log. |
| `summary_artifact` | string or null | Recommended | Path to summary JSON. |
| `rch_meta_artifact` | string or null | If RCH | Path to `*.rch_meta.json` sidecar. |
| `redaction_status` | string | Yes | `none_needed`, `redacted`, `unsafe_missing`, or `unknown`. |

Additional fields are allowed, but the v1 names above must not change without a
schema-version bump.

## Stable Reason Codes

Reason codes are operator-facing and machine-matchable. They should be lower
snake case.

| Code | Typical state | Use |
| --- | --- | --- |
| `not_run` | `NOT_RUN` | Required lane has not been attempted. |
| `remote_cargo_reached` | `REACHED_REMOTE_CARGO` | Cargo started remotely, final status pending or still streaming. |
| `source_compile_fail` | `SOURCE_COMPILE_FAIL` | First-party source compile or lint error. |
| `test_assertion_fail` | `TEST_FAIL` | Test, bench, or harness assertion failed. |
| `pass` | `PASS` | Required proof passed with sufficient artifacts. |
| `rch_timeout_wrapper_missing` | `INFRA_BLOCKED_PRE_CARGO` | Remote timeout wrapper could not execute the process before Cargo. |
| `rch_worker_unreachable` | `INFRA_BLOCKED_PRE_CARGO` | No reachable worker for a remote-required lane. |
| `rch_sync_failed` | `INFRA_BLOCKED_PRE_CARGO` | Repo sync failed before remote command execution. |
| `rch_artifact_retrieval_stalled` | `INFRA_BLOCKED_POST_CARGO` | Remote execution started, but artifact retrieval stalled. |
| `rch_remote_mirror_drift` | `INFRA_BLOCKED_POST_CARGO` | Worker mirror lacks required repo files or paths. |
| `rch_remote_system_dependency_missing` | `INFRA_BLOCKED_POST_CARGO` | Worker lacks required system packages or cross-toolchain pieces. |
| `local_invalid_shell_wrapped_rch` | `LOCAL_INVALID` | `rch exec -- ... bash -lc 'cargo ...'` or equivalent was offered as remote proof. |
| `local_invalid_fail_open` | `LOCAL_INVALID` | RCH or harness reported local fallback. |
| `skipped_hardware_predicate` | `SKIPPED_NOT_PROVEN` | Target-class hardware predicate absent. |
| `skipped_disk_pressure` | `SKIPPED_NOT_PROVEN` | Disk prerequisite absent and lane intentionally skipped. |
| `inconclusive_missing_artifacts` | `INCONCLUSIVE` | Required logs or metadata are absent. |
| `inconclusive_conflicting_logs` | `INCONCLUSIVE` | Logs disagree about backend, worker, or exit status. |

## Truthfulness Rules For Beads And Closeout

Use the same labels in ledger records, Beads comments, commit messages, and
release notes.

- Say "RCH sync completed" only for sync. Do not shorten that to "tests ran".
- Say "remote Cargo reached" only when `cargo_process_started=true` on the
  intended remote backend.
- Say "rustc reached" only when `rustc_process_started=true`.
- Say "tests passed" only for `PASS` with a test or E2E proof scope.
- Say "source red" only for `SOURCE_COMPILE_FAIL` or `TEST_FAIL`.
- Say "blocked before Cargo" for `INFRA_BLOCKED_PRE_CARGO`.
- Say "blocked after Cargo started" for `INFRA_BLOCKED_POST_CARGO`.
- Say "local smoke only" or "invalid for remote proof" for `LOCAL_INVALID`.
- Say "skipped, not proven" for `SKIPPED_NOT_PROVEN`.
- Say "inconclusive" when artifacts are insufficient.

Implementation beads must not close on `NOT_RUN`, `LOCAL_INVALID`,
`SKIPPED_NOT_PROVEN`, or `INCONCLUSIVE`. They may close on
`INFRA_BLOCKED_PRE_CARGO` or `INFRA_BLOCKED_POST_CARGO` only if the bead's
purpose is to document, classify, or report that blocker rather than to prove
source behavior.

Docs-only contract beads may close with docs-static validation, but their
closing comments must not imply workspace Cargo health.

## Current Blocker Mapping

| Bead | Current classification | Required wording |
| --- | --- | --- |
| `ft-tn6cw.1` | `INFRA_BLOCKED_PRE_CARGO` for the remote external-timeout wrapper failure before Cargo. | "RCH wrapper/tooling blocked before Cargo; no source verdict." |
| `ft-luq3w.1` | Contract artifact complete, but direct remote Cargo proof is `INFRA_BLOCKED_PRE_CARGO`; earlier shell-wrapped proof is `LOCAL_INVALID` for remote closeout. | "Docs contract is present; remote Cargo proof remains blocked/invalid until `ft-tn6cw.1` is fixed." |
| `ft-bvyrc` | Source fix may exist, but closeout proof is blocked by `ft-tn6cw.1` if its required remote lane hits the same wrapper failure. | "Cannot upgrade to remote-proven until a direct RCH Cargo lane reaches Cargo." |
| `ft-1grhq.2` | Storage IO scheduler proof must separate source failures from RCH wrapper or worker failures. | "Queued/synced/offloaded chatter is not scheduler proof; require a terminal proof state." |
| `ft-lmg3g.1` | Remote Cargo reportedly passed, but artifact retrieval or retained artifact completeness must be classified explicitly. | Use `PASS` only if required logs and metadata are retained; otherwise use `INFRA_BLOCKED_POST_CARGO` with the remote result noted as partial evidence. |
| `ft-tn6cw.3` | Future implementation depends on this contract. | Unit and E2E fixtures must cover every v1 taxonomy state. |
| `ft-tn6cw.4` | Future guardrail depends on this contract. | It must reject or downgrade shell-wrapped local-invalid command shapes. |

## Example Records

### PASS

```json
{
  "schema_version": 1,
  "proof_id": "ft-example-pass-20260505T052000Z",
  "bead_id": "ft-example",
  "created_at_utc": "2026-05-05T05:20:00Z",
  "finished_at_utc": "2026-05-05T05:24:00Z",
  "actor": "OliveChapel",
  "repo_path": "/Users/jemanuel/projects/frankenterm",
  "git_head": "abcdef123456",
  "branch": "main",
  "dirty_tree_summary": "owned paths only",
  "command": "rch exec -- env CARGO_TARGET_DIR=/tmp/ft-example-target cargo test -p frankenterm-core --lib example_filter -- --nocapture",
  "working_dir": "/Users/jemanuel/projects/frankenterm",
  "target_dir": "/tmp/ft-example-target",
  "proof_scope": "cargo_test",
  "required_backend": "rch",
  "observed_backend": "rch",
  "rch_version": "0.12.0",
  "rch_config_digest": "redacted-config-sha256",
  "selected_worker": "vmi1152480",
  "worker_probe_artifact": "tests/e2e/artifacts/proof/ft-example/probe.log",
  "sync_duration_ms": 174000,
  "remote_command_duration_ms": 64000,
  "wrapper_exit_code": 0,
  "remote_exit_code": 0,
  "cargo_process_started": true,
  "rustc_process_started": true,
  "test_binary_started": true,
  "artifact_retrieval_status": "complete",
  "proof_state": "PASS",
  "reason_code": "pass",
  "operator_interpretation": "Remote Cargo test reached worker vmi1152480 and passed with retained artifacts.",
  "safe_to_close": true,
  "high_scale_predicate": null,
  "stdout_artifact": "tests/e2e/artifacts/proof/ft-example/test.log",
  "stderr_artifact": null,
  "structured_log_artifact": "tests/e2e/artifacts/proof/ft-example/structured.log",
  "summary_artifact": "tests/e2e/artifacts/proof/ft-example/summary.json",
  "rch_meta_artifact": "tests/e2e/artifacts/proof/ft-example/test.log.rch_meta.json",
  "redaction_status": "none_needed"
}
```

### Source Compile Failure

```json
{
  "schema_version": 1,
  "proof_id": "ft-example-source-fail-20260505T052500Z",
  "bead_id": "ft-example",
  "created_at_utc": "2026-05-05T05:25:00Z",
  "finished_at_utc": "2026-05-05T05:28:00Z",
  "actor": "OliveChapel",
  "repo_path": "/Users/jemanuel/projects/frankenterm",
  "git_head": "abcdef123456",
  "branch": "main",
  "dirty_tree_summary": "shared tree dirty outside owned paths",
  "command": "rch exec -- env CARGO_TARGET_DIR=/tmp/ft-example-target cargo check -p frankenterm-core --lib",
  "working_dir": "/Users/jemanuel/projects/frankenterm",
  "target_dir": "/tmp/ft-example-target",
  "proof_scope": "cargo_check",
  "required_backend": "rch",
  "observed_backend": "rch",
  "rch_version": "0.12.0",
  "rch_config_digest": "redacted-config-sha256",
  "selected_worker": "vmi1152480",
  "worker_probe_artifact": "tests/e2e/artifacts/proof/ft-example/probe.log",
  "sync_duration_ms": 181000,
  "remote_command_duration_ms": 93000,
  "wrapper_exit_code": 101,
  "remote_exit_code": 101,
  "cargo_process_started": true,
  "rustc_process_started": true,
  "test_binary_started": false,
  "artifact_retrieval_status": "complete",
  "proof_state": "SOURCE_COMPILE_FAIL",
  "reason_code": "source_compile_fail",
  "operator_interpretation": "Remote Cargo reached rustc and failed on first-party source; fix the compile error before closeout.",
  "safe_to_close": false,
  "high_scale_predicate": null,
  "stdout_artifact": "tests/e2e/artifacts/proof/ft-example/check.log",
  "stderr_artifact": null,
  "structured_log_artifact": "tests/e2e/artifacts/proof/ft-example/structured.log",
  "summary_artifact": "tests/e2e/artifacts/proof/ft-example/summary.json",
  "rch_meta_artifact": "tests/e2e/artifacts/proof/ft-example/check.log.rch_meta.json",
  "redaction_status": "none_needed"
}
```

### Pre-Cargo Infrastructure Failure

```json
{
  "schema_version": 1,
  "proof_id": "ft-luq3w-pre-cargo-20260505T050000Z",
  "bead_id": "ft-luq3w.1",
  "created_at_utc": "2026-05-05T05:00:00Z",
  "finished_at_utc": "2026-05-05T05:03:30Z",
  "actor": "OliveChapel",
  "repo_path": "/Users/jemanuel/projects/frankenterm",
  "git_head": "unknown",
  "branch": "main",
  "dirty_tree_summary": "shared dirty tree; no source edits in this attempt",
  "command": "rch exec -- env CARGO_TARGET_DIR=/tmp/ft-luq3w-target cargo test -p frankenterm-core --lib --no-default-features auto_tune -- --nocapture",
  "working_dir": "/Users/jemanuel/projects/frankenterm",
  "target_dir": "/tmp/ft-luq3w-target",
  "proof_scope": "cargo_test",
  "required_backend": "rch",
  "observed_backend": "rch",
  "rch_version": null,
  "rch_config_digest": null,
  "selected_worker": "vmi1152480",
  "worker_probe_artifact": null,
  "sync_duration_ms": 180611,
  "remote_command_duration_ms": null,
  "wrapper_exit_code": 127,
  "remote_exit_code": null,
  "cargo_process_started": false,
  "rustc_process_started": false,
  "test_binary_started": false,
  "artifact_retrieval_status": "not_started",
  "proof_state": "INFRA_BLOCKED_PRE_CARGO",
  "reason_code": "rch_timeout_wrapper_missing",
  "operator_interpretation": "RCH selected a worker and synced, but the remote timeout wrapper failed before Cargo started; this is no source verdict.",
  "safe_to_close": false,
  "high_scale_predicate": null,
  "stdout_artifact": null,
  "stderr_artifact": null,
  "structured_log_artifact": null,
  "summary_artifact": null,
  "rch_meta_artifact": null,
  "redaction_status": "unknown"
}
```

### Local-Invalid Proof Attempt

```json
{
  "schema_version": 1,
  "proof_id": "ft-luq3w-local-invalid-20260505T045000Z",
  "bead_id": "ft-luq3w.1",
  "created_at_utc": "2026-05-05T04:50:00Z",
  "finished_at_utc": "2026-05-05T04:55:00Z",
  "actor": "OliveChapel",
  "repo_path": "/Users/jemanuel/projects/frankenterm",
  "git_head": "unknown",
  "branch": "main",
  "dirty_tree_summary": "shared dirty tree; command evidence only",
  "command": "rch exec -- env CARGO_TARGET_DIR=/tmp/ft-luq3w-target bash -lc 'cargo test -p frankenterm-core --lib --no-default-features auto_tune -- --nocapture'",
  "working_dir": "/Users/jemanuel/projects/frankenterm",
  "target_dir": "/tmp/ft-luq3w-target",
  "proof_scope": "cargo_test",
  "required_backend": "rch",
  "observed_backend": "unknown",
  "rch_version": null,
  "rch_config_digest": null,
  "selected_worker": null,
  "worker_probe_artifact": null,
  "sync_duration_ms": null,
  "remote_command_duration_ms": null,
  "wrapper_exit_code": null,
  "remote_exit_code": null,
  "cargo_process_started": false,
  "rustc_process_started": false,
  "test_binary_started": false,
  "artifact_retrieval_status": "not_applicable",
  "proof_state": "LOCAL_INVALID",
  "reason_code": "local_invalid_shell_wrapped_rch",
  "operator_interpretation": "Shell-wrapped RCH command was not valid remote Cargo proof; rerun with direct `rch exec -- env CARGO_TARGET_DIR=... cargo ...`.",
  "safe_to_close": false,
  "high_scale_predicate": null,
  "stdout_artifact": null,
  "stderr_artifact": null,
  "structured_log_artifact": null,
  "summary_artifact": null,
  "rch_meta_artifact": null,
  "redaction_status": "unknown"
}
```

## Future Implementation Tests

`ft-tn6cw.3` must include unit tests for every v1 taxonomy state and reason
code. The fixtures must cover at least:

- direct `rch exec -- env CARGO_TARGET_DIR=... cargo test ...` with
  `remote_exit_code=0` -> `PASS`;
- remote Cargo/rustc compile error -> `SOURCE_COMPILE_FAIL`;
- remote test assertion failure -> `TEST_FAIL`;
- May 5 external-timeout failure before Cargo -> `INFRA_BLOCKED_PRE_CARGO`;
- artifact retrieval or worker stall after remote Cargo started ->
  `INFRA_BLOCKED_POST_CARGO`;
- `rch exec -- env CARGO_TARGET_DIR=... bash -lc 'cargo ...'` ->
  `LOCAL_INVALID`;
- `[RCH] local` or `running locally` fail-open text -> `LOCAL_INVALID`;
- target-class hardware predicate absent -> `SKIPPED_NOT_PROVEN`;
- missing or contradictory logs -> `INCONCLUSIVE`.

The first integration or E2E report test must produce a synthetic multi-bead
ledger with at least one `PASS`, one `INFRA_BLOCKED_PRE_CARGO`, one
`LOCAL_INVALID`, and one `SOURCE_COMPILE_FAIL`. It must retain:

- `commands.txt` with exact commands;
- `summary.json` with counts by `proof_state` and `reason_code`;
- `structured.log` rows for classification decisions;
- raw log fixtures or generated snippets;
- `*.rch_meta.json` sidecars for RCH examples when the input model includes
  RCH metadata.

Structured logs must include the matched classification rule, whether Cargo was
reached, whether rustc was reached, the selected worker when known, and the
artifact paths used to justify the state.

## Operator Report Requirements

Future reports and Beads comments must group results by:

1. source red: `SOURCE_COMPILE_FAIL`, `TEST_FAIL`;
2. remote proof passed: `PASS`;
3. pre-Cargo infrastructure blockers: `INFRA_BLOCKED_PRE_CARGO`;
4. post-Cargo infrastructure blockers: `INFRA_BLOCKED_POST_CARGO`;
5. invalid local or off-policy proof: `LOCAL_INVALID`;
6. skipped/not proven: `SKIPPED_NOT_PROVEN`;
7. missing evidence: `NOT_RUN`, `INCONCLUSIVE`.

The report must make it visually and mechanically impossible to confuse:

- worker health with Cargo proof,
- RCH sync with test execution,
- local fallback with remote execution,
- hardware smoke with 64-core / 256 GiB proof,
- partial artifact retrieval with artifact-complete evidence,
- a docs-only validation pass with workspace Cargo health.

## Privacy And Redaction

Proof records may include commands, selected worker ids, target dirs, relative
artifact paths, exit codes, and aggregate diagnostic text. They must not embed
pane text, secrets, full environment dumps, SSH keys, tokens, or large raw logs
inside the ledger record. Large logs belong in referenced artifact files with
redaction status recorded in `redaction_status`.

Home-directory paths are allowed only when necessary to identify the repo,
target directory, or retained artifact. Prefer repo-relative artifact paths in
stored records and absolute paths only in final operator closeout when the
existing repo evidence contract requires them.
