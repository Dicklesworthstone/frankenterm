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
- `watch_has_changed`
  - replacement: explicit `watch::Receiver::has_changed` handling per backend semantics
- `watch_borrow_and_update_clone`
  - replacement: explicit borrow/consume logic per backend semantics
- `watch_changed`
  - replacement: explicit `watch::Receiver::changed` with caller-owned cx/lifecycle handling

## Retire

- `process::Command`
  - replacement: asupersync-native process abstraction
- `signal`
  - replacement: asupersync-native signal handling

## Contraction Slice in This Change

- Standardized blocking execution in core subsystems onto `runtime_compat::spawn_blocking`:
  - `crates/frankenterm-core/src/storage.rs`
  - `crates/frankenterm-core/src/snapshot_engine.rs`
- Replaced external call-sites of transitional MPSC helper shims with explicit local receive/send semantics:
  - `crates/frankenterm-core/src/ipc.rs`
  - `crates/frankenterm-core/src/native_events.rs` (tests)
  - `crates/frankenterm-core/src/tailer.rs` (tests)
- Replaced external call-sites of transitional MPSC/watch helper shims in runtime hot paths with explicit backend-aware local semantics:
  - `crates/frankenterm-core/src/runtime.rs`
- Replaced external call-sites of transitional MPSC/watch helper shims in vendored subscription paths with explicit backend-aware local semantics:
  - `crates/frankenterm-core/src/vendored/mux_client.rs`
- Replaced external call-sites of the retired `runtime_compat::process::Command` shim with explicit process ownership:
  - `crates/frankenterm/src/main.rs`
  - `crates/frankenterm-core/src/caut.rs`
  - `crates/frankenterm-core/src/cass.rs`
  - `crates/frankenterm-core/src/wezterm.rs`
- Replaced immediately-awaited `runtime_compat::task::spawn_blocking` call-sites in CLI paths with the canonical `runtime_compat::spawn_blocking` helper:
  - `crates/frankenterm/src/main.rs`
- Added contract guardrail e2e script:
  - `tests/e2e/test_ft_e34d9_10_2_3_runtime_compat_contraction.sh`
  - now enforces zero `mpsc_recv_option` / `mpsc_send` / `watch_has_changed` / `watch_borrow_and_update_clone` / `watch_changed` / `runtime_compat::process::Command` call-sites outside `runtime_compat.rs`, and treats `runtime_compat::task::spawn_blocking` as allowlisted only in `crates/frankenterm-core/src/search_bridge.rs`.

## Validation Artifacts

- Command:
  - `tests/e2e/test_ft_e34d9_10_2_3_runtime_compat_contraction.sh`
- Latest run:
  - `tests/e2e/logs/ft_e34d9_10_2_3_20260309_215833.jsonl`
- Outcome:
  - Static contraction guardrails passed before remote-worker preflight.
  - `rch check` reported ready, but `rch workers probe --all --json` reported `connection_failed` for all 8 configured workers.
  - Harness emitted `reason_code=rch_health_probe_mismatch` (`RCH-E101`) after the readiness/probe disagreement.
  - Offload-only policy correctly prevented cargo-test execution when no remote worker was reachable.
  - Contract guardrails run before remote-worker preflight, so helper/allowlist regression checks still execute and emit artifacts even when remote offload is unavailable.
- Slice-level static checks (non-compile, no local cargo fallback):
  - `rg -n "runtime_compat::task::spawn_blocking" crates/frankenterm/src/main.rs crates/frankenterm-core/src --glob '!crates/frankenterm-core/src/runtime_compat.rs'` -> `crates/frankenterm-core/src/search_bridge.rs:322` only
  - `rg -n "runtime_compat::process::Command|\b(runtime_compat::)?(mpsc_recv_option|mpsc_send|watch_has_changed|watch_borrow_and_update_clone|watch_changed)\b" crates/frankenterm/src/main.rs crates/frankenterm-core/src --glob '!runtime_compat.rs'` -> no matches
  - `rustfmt --edition 2024 --check crates/frankenterm/src/main.rs crates/frankenterm-core/src/caut.rs crates/frankenterm-core/src/cass.rs crates/frankenterm-core/src/wezterm.rs` -> pass
  - `bash -n tests/e2e/test_ft_e34d9_10_2_3_runtime_compat_contraction.sh` -> pass
- Notes:
  - `crates/frankenterm-core/src/vendored/mux_client.rs` now uses explicit backend-aware local receive/watch semantics instead of external transitional helper calls.
  - `crates/frankenterm/src/main.rs` now runs its internal `ft` subprocess captures via explicit `std::process::Command` wrapped in `runtime_compat::spawn_blocking`, so the runtime owns only the blocking wait instead of the subprocess API.
  - `crates/frankenterm/src/main.rs` also routes its immediately-awaited blocking tasks through `runtime_compat::spawn_blocking`, leaving `runtime_compat::task::spawn_blocking` allowlisted only for the explicit worker handle in `crates/frankenterm-core/src/search_bridge.rs`.
  - `crates/frankenterm-core/src/caut.rs`, `crates/frankenterm-core/src/cass.rs`, and `crates/frankenterm-core/src/wezterm.rs` now use explicit `tokio::process::Command` because those paths rely on async process semantics (`kill_on_drop`, async `output()`).
  - `CARGO_TARGET_DIR` is run-unique (`...-<RUN_ID>`) to avoid cross-run artifact lock contention.
  - Harness now captures both `rch check` and `workers probe` artifacts and emits explicit `rch_health_probe_mismatch` diagnostics to improve infra triage.
