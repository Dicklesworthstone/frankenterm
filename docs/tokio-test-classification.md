# Tokio Test Classification

Bead: `ft-tf6g3.7`

This document classifies the `#[tokio::test]` migration state for supported
FrankenTerm paths. The current invariant is stricter than the original bridge
plan request: there are zero active `#[tokio::test]` attributes in supported
paths at current HEAD.

Supported paths:

- `crates/**/*.rs`
- `frankenterm/**/*.rs`
- `tests/**/*.rs`

Historical mentions in docs, comments, string literals, and old migration notes
are not active test attributes. The CI lint checks only lines whose trimmed
content starts with `#[tokio::test`.

## Current Classification

| Class | Meaning | Current count | Current status |
|---|---:|---:|---|
| A | Converted to `asupersync_test!`, `RuntimeFixture`, or `run_lab_test` today | 40 port files | Complete |
| B | Requires a Cx-aware refactor before conversion | 0 active attributes | None open in supported paths |
| C | Deliberately quarantined with rationale | 0 active attributes | No supported-path carve-out |

The old "60 `#[tokio::test]`" claim is stale for the current tree. The live
scan is:

```bash
rg -n '^\s*#\[tokio::test' crates frankenterm tests -g '*.rs'
```

Expected result: no matches.

## Class A: Converted Ports

The supported migration surface is represented by `*_labruntime.rs` files under
`crates/frankenterm-core/tests/`. These files port former tokio-style async
tests to one of the supported runners:

- `common::asupersync_test!` for compact async test bodies.
- `common::fixtures::RuntimeFixture` when a test needs explicit runtime
  control.
- `common::lab::{run_lab_test, run_lab_test_simple}` when deterministic seeds,
  structured reports, or multi-seed exploration matter.

The current checked-in port files are:

```text
approval_labruntime.rs
cancellation_labruntime.rs
cleanup_labruntime.rs
diagnostic_labruntime.rs
distributed_labruntime.rs
events_labruntime.rs
export_labruntime.rs
metrics_labruntime.rs
notifications_labruntime.rs
orphan_reaper_labruntime.rs
policy_labruntime.rs
pool_labruntime.rs
protocol_recovery_labruntime.rs
recorder_lexical_ingest_labruntime.rs
recorder_migration_labruntime.rs
recorder_storage_labruntime.rs
replay_labruntime.rs
reports_labruntime.rs
restore_layout_labruntime.rs
restore_process_labruntime.rs
restore_scrollback_labruntime.rs
retry_labruntime.rs
runtime_labruntime.rs
secrets_labruntime.rs
session_correlation_labruntime.rs
sharding_labruntime.rs
simulation_labruntime.rs
snapshot_engine_labruntime.rs
spsc_labruntime.rs
storage_labruntime.rs
tailer_labruntime.rs
tantivy_ingest_labruntime.rs
tantivy_reindex_labruntime.rs
tcp_tls_labruntime.rs
telemetry_labruntime.rs
undo_labruntime.rs
watchdog_labruntime.rs
webhook_labruntime.rs
wezterm_labruntime.rs
workflows_labruntime.rs
```

The companion guard
`crates/frankenterm-core/tests/wa_22x4r_no_tokio_test_in_supported_paths.rs`
asserts both halves of the contract: zero active tokio test attributes and at
least 20 LabRuntime port files so the migration cannot disappear silently.

## Class B: Requires Cx-Aware Refactor

No active `#[tokio::test]` attribute is currently blocked on a Cx-aware refactor
inside supported paths.

If a future test cannot be moved directly to `asupersync_test!` or
`RuntimeFixture`, file a bead that names the runtime behavior that is missing
from `runtime_async` or the lab harness. Do not add a temporary
`#[tokio::test]`.

## Class C: Deliberate Quarantine

There are no supported-path quarantines at current HEAD. The policy is
fail-closed: a new active `#[tokio::test]` line fails CI unless this document
and `scripts/check_asupersync_test_only.sh` are deliberately updated in the
same change with a narrow, named exception.

Textual references to tokio in migration docs, grep guards, search fixtures,
and old integration-research notes are allowed only because they are not active
test attributes.

## Live Gates

| Gate | Path | Runs in CI | Purpose |
|---|---|---:|---|
| `asupersync-test-only` shell lint | `scripts/check_asupersync_test_only.sh` | yes | Fast PR/push guard for active `#[tokio::test]` lines |
| Rust supported-path guard | `crates/frankenterm-core/tests/wa_22x4r_no_tokio_test_in_supported_paths.rs` | yes, when Cargo tests run | Cargo-test-time mirror of the same invariant |
| Macro substrate | `crates/frankenterm-core/tests/common/asupersync_test.rs` | yes, through test compilation | Ergonomic async-test runner without tokio attributes |
| Dependency ban | `deny.toml` + CI cargo-deny step | yes | Reject direct first-party `tokio` dependencies |
| Source-pattern guards | `dependency_eradication.rs` / `forbidden_dep_guards.rs` | yes, through tests that exercise them | Reject direct tokio imports and runtime patterns |

This classification feeds the release-bundle artifact at
`docs/attestations/doctrine/tokio-eradication-status.json`.
