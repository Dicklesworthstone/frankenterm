# Heavy CI specialized job triage

Date: 2026-05-27
Task: `#26`
Status: live triage contract for specialized CI failures

## Purpose

This document separates ordinary fixable failures from CI lanes that need a
specific host, runtime, or hardware surface. A failed specialized job should
first be classified here before it is treated as a code defect.

## Fixable specialized jobs

These jobs run on standard GitHub-hosted runners and are expected to be fixed
in this repository when they fail.

| Job | Runner | Fix posture | Current task #26 disposition |
| --- | --- | --- | --- |
| Formal Methods | `ubuntu-latest` | Fix stale tool checksums, spec scripts, or model-test code. | Fixed stale `tla2tools.jar` SHA1 in CI. |
| Generated Artifacts | `ubuntu-latest` | Fix generated-artifact drift, count drift, or proof coverage regressions. | Fixed runtime-proof coverage drift for IPC/native-event wrappers; `storage.rs::count_events` uses its existing Cx sibling. Refreshed stale `docs/storage/callsite-migration-plan.json` drift from the storage backend callsite guard. |
| Operator Shell Tests | `ubuntu-latest`, `macos-14` | Fix shell portability, fixtures, or stale goldens. | Fixed stale Agent Mail fallback fixture/schema expectations. |
| Security Audit | `ubuntu-latest` | Fix high/critical advisories by updating or removing vulnerable dependencies; warning-class allowlisted advisories remain audit debt. | Fixed `RUSTSEC-2026-0149` by moving `wasmtime` and `wasmtime-wasi` to `44.0.2`; fixed `RUSTSEC-2026-0002` by moving the Tantivy stack off `lru 0.12.5`. |
| Coverage | `ubuntu-latest` | Fix missing CI packages or code-level coverage command breakage. | Fixed missing Cairo/X11/pkg-config packages before `cargo llvm-cov`. |
| Resize Performance Gates | `ubuntu-latest` plus RCH cargo execution inside the script | Fix compile drift, script errors, missing artifacts, or real threshold regressions when the standard runner and RCH lane are healthy. | Fixed compile drift in the timeline/warning validation lanes and gated the resize benchmark dependencies. |

## Host or hardware specific jobs

These jobs can be red for structural reasons that are not fixable by changing
ordinary Rust code.

| Job | Required surface | Blocked classification | Action when red |
| --- | --- | --- | --- |
| `Test (windows-latest)` | A real GitHub-hosted Windows runner and Windows runtime behavior, not just a local cross-check. | Host/runtime-specific. Cross-compilation can catch `cfg` and metadata errors, but it cannot prove native Windows process, filesystem, path, shell, or named-pipe behavior. | Pull the failed Windows job log. Fix compile/test defects that name repository code. OpenSSL bootstrap is expected to be provisioned by CI with vcpkg; treat missing OpenSSL as workflow drift. If the failure is runner image drift, Windows-only service behavior, path length, or another host/runtime facility after bootstrap, document the exact log line and keep it blocked on Windows runner behavior. |
| `GPU Regression (macOS 15 Metal)` | macOS 15 on Apple Silicon with a Metal adapter. | Hardware-specific reference lane. Linux llvmpipe and local headless checks are diagnostic only and cannot prove the Metal golden path. | Fix harness or fixture defects that reproduce without Metal. If adapter availability, Metal driver behavior, or hosted macOS graphics capability is the cause, retain the uploaded artifacts and classify as hardware/host blocked. |
| `GPU Regression (Linux llvmpipe)` | Ubuntu 24.04 with Mesa Vulkan software renderer and the `llvmpipe` adapter selected. | Host/runtime-specific software-renderer lane. It is not a substitute for macOS Metal goldens. | Fix package or harness defects when the Mesa stack is installable. If the hosted image cannot provide the expected software adapter or Vulkan ICD, classify as host blocked and cite the adapter/probe log. |
| `GPU Regression Required` | Aggregates the macOS Metal and Linux llvmpipe results. | Derived gate. It is only actionable through the underlying GPU jobs. | Do not patch this gate directly unless the aggregation logic is wrong. Classify by the failed dependency result. |

## Triage procedure

1. Pull the failed job log with `gh run view --job <job-id> --log-failed`.
2. Identify the first root-cause failure, not the final aggregate failure.
3. If the cause is in repository code, workflow configuration, fixtures, or
   dependency versions, fix it and commit the narrow change.
4. If the cause requires a specific hosted runner runtime or graphics adapter,
   record the exact missing surface and leave the job classified as
   host/hardware blocked.
5. For aggregate jobs, classify by their failed dependency.

## Current task #26 proof notes

- Operator Shell Tests: old run `26488812786` is superseded by commit
  `8d663a8dd`; the exact local Bats suites passed after the fixture refresh:
  `tests/clean_stale_tests.bats` 33/33, `tests/swarm_tick_tests.bats` 23/23,
  and `tests/operator_lock_tests.bats` 2/2.
- Generated Artifacts: old run `26488812786`, job `78001993946`, failed in
  `RuntimeProof coverage ratchet (ft-3kv6e)` because five public async sites
  were uncovered (`ipc.rs::{connect,accept}`,
  `native_events.rs::{connect,accept}`, and `storage.rs::count_events`).
  Commit `a3cdb36fe` fixed the IPC/native-event wrappers and made the ratchet
  understand the existing `count_events` Cx sibling. `python3
  scripts/check_runtime_proof_coverage.py` now reports `uncovered: 0`, and
  `cargo check -p frankenterm-core --lib` passed through RCH remote worker
  `vmi1227854`.
- Follow-up local generated-artifact checks on the dirty shared checkout must
  not use `scripts/check_generated_artifacts.sh` as a whole-tree verdict: that
  wrapper intentionally fails on any unrelated `git diff` or untracked file.
  The component checks are clean: schema docs have no tracked drift,
  renderer-corpus drift guard passes, mux-interface guard passes,
  workspace-cycle guard passes in GitHub Actions local-Cargo mode, loom skeleton
  coverage passes, RuntimeProof uncovered remains `0`, and the
  asupersync-test-only guard passes.
- Dependabot PR run `26507772159` failed the storage-backend callsite drift
  guard because `docs/storage/callsite-migration-plan.json` still recorded the
  old `storage.rs` line count. The generator still reports `0` callsites across
  `0` patterns; the refreshed plan is metadata-only and
  `python3 scripts/storage_backend_callsites.py --check` now passes.
- Follow-up CI run `26507296325` reached the specialized lanes after the
  Windows OpenSSL bootstrap fix, but was cancelled by both lint jobs failing
  `cargo fmt --all -- --check` before most heavy jobs could complete. Commit
  `7562fd93e` applies the clean-tree rustfmt delta from an archive copy of
  `HEAD` so the formatter gate no longer preempts the heavy-job signal.
- Formal Methods: run `26507296325` passed the old failing TLA+ install and TLC
  setup steps before the run was cancelled during the long kill-switch proof.
  No new formal-methods code defect was visible in that run.
- Security Audit: old run `26488812786`, job `78001994081`, failed after the
  Wasmtime fix because `cargo audit` still denied `RUSTSEC-2026-0002` for
  `lru 0.12.5`, pulled by Tantivy `0.22.1` and `0.25.0`. Updating workspace
  Tantivy to `0.26.1` and repinning `frankensearch` to upstream
  `2cad158f4468ece7076e3fe529c8e5c20b2e020e` removes `lru 0.12.5`.
  `cargo tree -i lru@0.12.5 --locked` reports no matching package, and
  `cargo audit --no-fetch` now exits successfully with the existing four
  allowed warnings only. A follow-up RCH compile proof for
  `cargo check -p frankenterm-core --lib` reached remote worker `vmi1153651`
  and used the updated `tantivy 0.26.1`, `frankensearch` `2cad158f...`, and
  `asupersync 0.3.2` graph, but the SSH command hit the 1800s RCH limit
  (`[RCH-E104]`), so that broad compile proof remains pending a longer remote
  window.
- Coverage: old run `26488812786`, job `78001994064`, failed before coverage
  measurement because `cairo-sys-rs` could not find `cairo.pc`. The fixable CI
  package gap is closed in `ci.yml` by installing Cairo/X11/pkg-config packages
  before `cargo llvm-cov`; newer runs were cancelled by subsequent pushes before
  producing a fresh coverage result.
- Resize Performance Gates: old run `26488812786`, job `78001994063`, failed
  before latency evaluation on compile drift: `RuntimeTelemetryLog` no longer
  exposed `normalized_max_events()`, and `RecorderStorageConfig` gained the
  required `frankensqlite` field. Commits `da1cf2317`, `ecfe47278`, and
  `7f6ca8ada` fix the benchmark dependency gate and the test compile drift.
  Targeted RCH tests reached remote workers but timed out at the SSH command
  limit (`vmi1227854` and `vmi1264463`, both `[RCH-E104]`), so the proof lane is
  still red until a remote window completes.
- Older `Test (windows-latest)` run `26503816816`, job `78050944944`, failed
  before tests because `openssl-sys` could not find `OPENSSL_DIR` or vcpkg
  OpenSSL. `ci.yml` now provisions the same `x64-windows-static-md` OpenSSL
  triplet already used by `windows-check.yml`. Run `26507296325` confirmed
  both Windows OpenSSL provisioning steps succeeded; the native Windows test
  step was cancelled, so Windows runtime behavior remains host-specific and
  unproven until a non-cancelled Windows runner completes it.
- GPU Regression: run `26507296325` passed the Linux `llvmpipe` job. The macOS
  15 Metal job was cancelled during the harness self-test after lint cancelled
  the workflow, so the Metal lane remains a hardware-specific runner result,
  not a repository-code failure.
- Local helper checks used for non-Cargo surfaces: runtime-proof script,
  operator Bats fixtures, `cargo audit`, `git diff --check`, and YAML parsing.
  `actionlint` was not installed on the local host during this pass.
