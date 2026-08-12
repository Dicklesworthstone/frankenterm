# Dependency Upgrade Log

**Started:** 2026-06-14  |  **Updated:** 2026-08-12  |  **Project:** frankenterm  |  **Language:** Rust + GitHub Actions

## Summary

- **Updated:** GitHub Actions, direct Cargo registry dependencies, and `Cargo.lock`
- **Skipped:** no direct dependency update intentionally skipped
- **Failed:** no code migration currently known failed
- **Needs attention:** final remote `rch` workspace proof is blocked; see Proof

## Discovery

- Read `AGENTS.md` and `README.md` before editing.
- Investigated the workspace architecture and dependency surfaces before changing manifests.
- Checked open Dependabot issues and PRs. One open Dependabot PR existed: `#62`, the GitHub Actions group update.
- Closed PR `#62` after applying the workflow action updates locally with current stable patch tags.
- Rechecked Dependabot after the update: no open Dependabot PRs or issues remained.
- Enumerated direct Cargo registry dependencies from `cargo metadata --no-deps --format-version 1`.
- Queried crates.io for latest stable direct registry versions and refreshed `Cargo.lock`.

## Updates

### GitHub Actions group

- **Updated:**
  - `actions/checkout`: `v4` / `v6.0.2` -> `v6.0.3`
  - `actions/upload-artifact`: `v4` -> `v7.0.1`
  - `actions/download-artifact`: `v4` -> `v8.0.1`
  - `actions/setup-python`: `v5` -> `v6.2.0`
  - `dependabot/fetch-metadata`: `v2` -> `v3.1.0`
  - `peter-evans/repository-dispatch`: `v3` -> `v4.0.1`
  - `softprops/action-gh-release`: `v2` -> `v3.0.0`
  - `codecov/codecov-action`: `v4` -> `v7.0.0`
- **Breaking:** no workflow syntax migration required by the changed call sites.

### Cargo registry dependencies

- Updated direct registry dependency requirements across the workspace and refreshed `Cargo.lock`.
- Migrated API changes for the major touched libraries, including `bitflags`, `csscolorparser`, `ordered-float`, `mlua`, `colorgrad`, `criterion`, `governor`, `intrusive-collections`, `bloomfilter`, `rand`, `sha2`, `syn`, `rcgen`, `wgpu`, `resize`, and `jsonschema`.
- Resolved follow-on compile/clippy issues discovered during local and remote proof attempts.

### Remaining non-direct holdouts

The remaining crates known to be behind latest are transitive-only in this lockfile:

- `generic-array` `0.14.7` (latest `0.14.9`)
- `lua-src` `550.0.0` (latest `550.1.1`)
- `luajit-src` `210.6.6+707c12b` (latest `210.7.2+b925b3e`)

## Proof

- **Local format:** `cargo fmt --check` passed after final formatting fixes.
- **Local scoped clippy diagnostic:** `RUSTFLAGS='-A deprecated' cargo clippy -p frankenterm-core -p frankenterm-core-replay --lib --tests -- -D warnings` passed. This is local diagnosis only, not remote proof.
- **Audit:** `cargo audit --no-fetch` completed with only the already tolerated unmaintained warnings for `paste` and `rustls-pemfile`.
- **Remote workspace check:** `j-29884604911452538` passed earlier on `vmi1149989`, but it predates later lint/format fixes and is not final proof for the current tree.
- **Remote workspace clippy:** current final proof is unavailable. Runs exposed and drove fixes, but the latest full retry was cancelled by RCH stuck detection (`j-29884604911452568`, exit `130`) and the following retry failed closed before running with `no admissible workers: health_below_fallback=1,hard_preflight=3,active_project_exclusion=1`.
- **Remote fmt:** unavailable because RCH classifies `cargo fmt --check` as a non-compilation command and refuses local fallback under `RCH_REQUIRE_REMOTE=1`.

## Needs Attention

- Re-run remote `rch` workspace `cargo check --workspace --all-targets` and `cargo clippy --workspace --all-targets -- -D warnings` after RCH admission clears.
- Current RCH status during this update showed another active FrankenTerm build (`29884604911452571`) on `vmi1149989`; do not count local Cargo output as proof.

## 2026-08-12 targeted runtime and storage campaign

### asupersync and asupersync-macros: 0.3.5 -> 0.3.10

- **Status:** updated in `Cargo.toml` and `Cargo.lock`; strict remote proof is in progress under `ft-ifgm7`.
- **Why 0.3.10:** it is the newest release accepted by the pinned FastMCP, FastAPI, and FrankenSearch dependency graph and by stable `fsqlite 0.2.1`.
- **Latest-stable boundary:** asupersync `0.4.3` is newer, but adopting it now would resolve both 0.3.x and 0.4.x runtimes because the pinned ecosystem still requires 0.3.x. That coordinated upgrade is tracked separately as deferred bead `ft-wc3uc`; a split runtime graph is not an acceptable workaround.
- **Upstream rationale:** releases 0.3.6 through 0.3.10 include the current-thread timer-floor repair, ARM blocking-pool ordering fences, watch lost-update repair, merge busy-spin elimination, long-timer and cancellation-validator repairs, and cancellation-waker ownership isolation. These are upstream correctness/performance motivations, not evidence of a FrankenTerm mux, input, or rendering speedup.
- **Resolved transitive changes:** AES-GCM `0.10.3 -> 0.11.0`, ChaCha20Poly1305 `0.10.1 -> 0.11.0` alongside the retained 0.10.1 consumer, Base64 adds `0.23.1`, `franken-{kernel,evidence,decision} 0.3.5 -> 0.3.10`, and the macro crate adds Syn `3.0.3` alongside existing Syn versions.
- **Lock generation:** RCH rejected `cargo update` as non-compilation with `[RCH-E301]`, so local Cargo was used only to resolve `Cargo.lock`; no local compilation or test output counts as proof. All validation remains remote, fail-closed, and `--locked`.
- **Required proof before closure:** one resolved asupersync package, `runtime_async`/timer/watch/cancellation/blocking-pool/channel/LabRuntime coverage, no direct Tokio regression, workspace all-target check, warnings-denied Clippy, and exact committed-source format proof.

### FrankenSQLite / fsqlite 0.2.1

- **Status:** researched, not yet integrated. FrankenTerm currently has no `fsqlite` dependency to update; the two FrankenSQLite features are empty default-off scaffolds and the named recorder implementation still uses rusqlite. This is new backend architecture work, not a version bump.
- **Stable target:** crates.io `fsqlite 0.2.1`, pinned to the published/tagged source rather than the moving upstream `main` branch.
- **Sequencing:** complete the asupersync 0.3.10 proof, repair `StorageBackend` transaction ownership under `ft-ig9lh`, then implement the existing default-off FrankenSQLite canary under `ft-kcdqp`.
- **Safety/rollback:** use isolated temporary databases only, retain rusqlite as the rollback backend, require explicit close/cancellation/transaction/reopen/crash proof, and do not promote or claim performance without retained Apple Silicon and Threadripper benchmarks.
