# `runtime_compat` Surface Contract (`ft-e34d9.10.2.3`)

This document records the contraction policy for `crates/frankenterm-core/src/runtime_compat.rs`.
The machine-readable source of truth is `SURFACE_CONTRACT_V1` in that module.

## Keep

- `RuntimeBuilder`
- `Runtime`
- `CompatRuntime::block_on`
- `sleep`
- `timeout`
- `spawn_blocking`

## Replace

- `CompatRuntime::spawn_detached`
  - replacement: scope-owned spawning (`cx::spawn_with_cx` / explicit scope handles)
- `task::spawn_blocking`
  - replacement: `runtime_compat::spawn_blocking` except when explicit abortable `JoinHandle` control is required
- `mpsc_recv_option`
  - replacement: explicit receive semantics with cx/cancellation awareness
- `mpsc_send`
  - replacement: explicit cx-aware channel send path

## Retire

- `process::Command`
  - replacement: asupersync-native process abstraction
- `signal`
  - replacement: asupersync-native signal handling

## Contraction Slice in This Change

- Standardized blocking execution in core subsystems onto `runtime_compat::spawn_blocking`:
  - `crates/frankenterm-core/src/storage.rs`
  - `crates/frankenterm-core/src/snapshot_engine.rs`
- Added contract guardrail e2e script:
  - `tests/e2e/test_ft_e34d9_10_2_3_runtime_compat_contraction.sh`

## Validation Artifacts

- Command:
  - `tests/e2e/test_ft_e34d9_10_2_3_runtime_compat_contraction.sh`
- Latest run:
  - `tests/e2e/logs/ft_e34d9_10_2_3_20260226_173344.jsonl`
- Outcome:
  - Preflight failed fast because `rch workers probe --all` reported zero reachable workers.
  - Harness emitted `reason_code=rch_health_probe_mismatch` (`RCH-E101`) after `rch check` passed but probe showed no reachable workers.
  - `rch exec -- cargo test ...` also confirmed fail-open behavior (`[RCH] local`) after remote connect failure, so offload-only policy correctly blocked validation.
  - Script enforces offload-only policy and refuses local fallback execution.
- Notes:
  - `CARGO_TARGET_DIR` is run-unique (`...-<RUN_ID>`) to avoid cross-run artifact lock contention.
  - Harness now captures both `rch check` and `workers probe` artifacts and emits explicit `rch_health_probe_mismatch` diagnostics to improve infra triage.
