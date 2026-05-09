# RCH Proof Ledger Execution Policy

**Bead:** `ft-54ut8`
**Version:** `3.0.0`
**Status:** Active proof-ledger foundation

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
```

E2E policy validation:

```bash
bash tests/e2e/test_asupersync_rch_execution_policy.sh
```

The self-test and E2E cover accepted remote proof, rejected local-heavy proof,
accepted human-approved fallback, light local commands, stale schema versions,
malformed bead IDs, missing artifacts, missing required booleans, unredacted
provider tokens, SSH-style secret paths, RCH setup chatter, and shell wrappers
that mention RCH while running Cargo locally.

## User Impact

1. Prevents accidental local compilation storms from degrading operator session responsiveness.
2. Preserves reproducibility and auditability for migration, storage, fleet, GUI, search, release, and future proof lanes.
3. Makes degraded-mode exceptions explicit and reviewable instead of implicit.
