# asupersync Migration Execution Policy: rch-Only Heavy Compute

**Bead:** `ft-e34d9.10.1.4`  
**Version:** `1.0.1`
**Status:** Active baseline policy

## Purpose

This policy removes ambiguity in asupersync migration validation runs by
requiring `rch` for heavy compute workloads and a standard evidence contract
for every run.

## Scope

Applies to all `ft-e34d9.10.*` migration beads whenever commands are expected
to create material CPU/IO contention (build/test/bench/soak workloads).

## Heavy vs Light Classifier

Commands are classified as follows:

| Category | Command examples | rch required |
|---|---|---|
| Heavy | `cargo check`, `cargo build`, `cargo test`, `cargo clippy`, `cargo bench`, soak/perf loops that invoke cargo repeatedly | Yes |
| Light | `cargo fmt --check`, `cargo metadata`, `cargo locate-project`, docs/scripts that do not compile/test | No |

Classifier implementation is canonical in:

- `scripts/validate_asupersync_rch_execution_policy.sh --classify "<cmd>"`

## Mandatory Rule

For heavy commands, execution must use one of the validator-recognized
RCH-backed execution shapes:

```bash
rch exec -- <command>
```

or the shared fail-closed harness helpers:

```bash
run_rch_cargo_logged <log-file> env CARGO_TARGET_DIR=<repo-relative-target> cargo <args...>
run_rch_cargo_logged_with_timeout <seconds> <log-file> env CARGO_TARGET_DIR=<repo-relative-target> cargo <args...>
```

The helper forms are evidence-equivalent to direct `rch exec -- ...` because
they route through `tests/e2e/lib_rch_guards.sh`, reject local fallback markers,
and emit RCH metadata sidecars.

## Local Fallback Rule

Local fallback for heavy commands is allowed only when all are true:

1. `rch` is unavailable or remote workers are unhealthy.
2. Evidence entry includes non-empty `fallback_reason_code`.
3. Evidence entry includes non-empty `fallback_approved_by`.
4. Residual risk note explains impact on comparability/reproducibility.

## Evidence Contract

Every heavy run must be logged with fields:

1. `timestamp`
2. `command`
3. `is_heavy`
4. `used_rch`
5. `worker_context`
6. `artifact_paths`
7. `elapsed_seconds`
8. `exit_status`
9. `residual_risk_notes`
10. Optional fallback fields when `used_rch=false` on heavy runs:
   - `fallback_reason_code`
   - `fallback_approved_by`

Machine-readable schema:

- `docs/asupersync-rch-evidence-schema.json`

## Validation Tooling

Policy validator:

```bash
bash scripts/validate_asupersync_rch_execution_policy.sh --self-test
bash scripts/validate_asupersync_rch_execution_policy.sh --classify "cargo test --workspace"
bash scripts/validate_asupersync_rch_execution_policy.sh --validate-evidence <path-to-evidence.json>
```

E2E policy validation:

```bash
bash tests/e2e/test_asupersync_rch_execution_policy.sh
```

## User Impact

1. Prevents accidental local compilation storms from degrading operator session responsiveness.
2. Preserves reproducibility and auditability for migration quality gates.
3. Makes degraded-mode exceptions explicit and reviewable instead of implicit.
