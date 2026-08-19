# Dependency Upgrade Log

**Started:** 2026-06-14  |  **Updated:** 2026-08-18  |  **Project:** frankenterm  |  **Language:** Rust + GitHub Actions

## Summary

- **Updated:** GitHub Actions, direct Cargo registry dependencies, and `Cargo.lock`
- **Corrected:** workspace MSRV `1.85 -> 1.95` to match the resolved graph's
  declared maximum: `sysinfo 0.39.5`, pulled by Asupersync 0.3.10; this changes
  no dependency identity
- **Deferred:** Asupersync 0.4.7 and FrankenSQLite 0.3.5 until the single-runtime
  dependency cohort and storage-transaction prerequisites below are satisfied
- **Failed:** no code migration currently known failed
- **Needs attention:** coordinated Asupersync ecosystem convergence; exact
  committed-source proof authority is retained in `ft-ifgm7` and `ft-s1u2p`

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

## 2026-06-14 baseline proof

- **Local format:** `cargo fmt --check` passed after final formatting fixes.
- **Local scoped clippy diagnostic:** `RUSTFLAGS='-A deprecated' cargo clippy -p frankenterm-core -p frankenterm-core-replay --lib --tests -- -D warnings` passed. This is local diagnosis only, not remote proof.
- **Audit:** `cargo audit --no-fetch` completed with only the already tolerated unmaintained warnings for `paste` and `rustls-pemfile`.
- **Remote workspace check:** `j-29884604911452538` passed on `vmi1149989`,
  but it predates later lint/format fixes and is not proof for a current source
  identity.
- **Remote workspace Clippy:** the final retry in that baseline campaign was
  cancelled by RCH stuck detection (`j-29884604911452568`, exit `130`), and
  the following retry failed closed before running with `no admissible
  workers: health_below_fallback=1,hard_preflight=3,active_project_exclusion=1`.
- **Remote format:** RCH classified plain `cargo fmt --check` as a
  non-compilation command. Current source identity instead uses the dedicated
  `workspace_format_proof` test under the clean-baseline RCH contract.

## Current proof authority

- Do not infer current-tree proof from the historical jobs above. The closing
  comments for `ft-ifgm7` and `ft-s1u2p` must identify an exact committed SHA
  and retain strict-remote workspace check, warnings-denied Clippy, and the
  named clean-baseline formatting proof. Local Cargo output never counts.

## 2026-08-12 targeted runtime and storage campaign

### asupersync and asupersync-macros: 0.3.5 -> 0.3.10

- **Status:** updated in `Cargo.toml` and `Cargo.lock`; focused strict-remote
  semantic proof is retained under `ft-ifgm7`; its closing evidence must also
  be the authority for workspace check, Clippy, and exact formatting proof.
- **Why 0.3.10:** it is the newest release accepted by the pinned FastMCP, FastAPI, and current FrankenSearch dependency graph. It is a completed compatibility waypoint, not the latest upstream runtime.
- **Latest-stable boundary (re-audited 2026-08-19):** official crates.io and tagged-release evidence identify `asupersync 0.4.8` and `asupersync-macros 0.4.8` as latest stable. Adopting them now would make Cargo resolve incompatible 0.3.x and 0.4.x runtime/type universes because FrankenTerm's pinned FastMCP and FastAPI revisions still require 0.3.x and its pinned FrankenSearch revision explicitly requires `<0.4`. Stable FastMCP v0.6.0 has converged on exactly `asupersync 0.4.8`, but declares Rust 1.100 while FrankenTerm's current workspace MSRV is 1.95. Stable FastAPI v0.3.0 still requires the 0.3 family, whereas stable FrankenSearch v1.6.0 requires `>=0.4.3,<0.5`. A single-runtime migration therefore still requires a stable 0.4-compatible FastAPI release plus an explicit workspace-MSRV decision (or a compatible FastMCP release); repinning only the ready consumers cannot solve the graph. Coordinated convergence remains tracked by `ft-wc3uc`; a split runtime graph or relaxed-constraint bypass is not acceptable.
- **Stable ecosystem gate:** stable FastMCP v0.6.0 now requires exactly Asupersync 0.4.8, but its Rust 1.100 floor exceeds FrankenTerm's current 1.95 MSRV. Stable FastAPI v0.3.0 still requires the Asupersync 0.3 family, and its unreleased main branch remains on 0.3.x; downgrading FrankenTerm's pinned FastAPI revision to that tag would discard fixes without removing the blocker. Stable FrankenSearch v1.6.0 already accepts `>=0.4.3,<0.5`, so repinning the ready consumers alone would create the same forbidden split graph. Wait for a stable 0.4-compatible FastAPI release and resolve the FastMCP/workspace MSRV boundary, then migrate the runtime pair and all three consumers as one reviewed cohort.
- **Official 0.4.8 identity:** crate checksum `09c1e1074282fa940bdf764abce64e27df4ca174dd75394cb5057dbecb12bc7e`; annotated tag object `9490441f82179e65cd96a0a1c8aaf09cac4eed96` resolves to commit `ee7bd346e70684ac54ca142f3f5b105f1a003117`. The macro crate checksum is `1f4f29305f8315ffcf3c1b8eb10b5b63a4df452a319987857a31a93dccec4979`.
- **0.4.x migration surface:** the tracked-channel signature changes were already present in 0.3.10, and FrankenTerm has no direct `TrackedSender`/`TrackedPermit` callers. The remaining work is semantic: native-task abort/join behavior, typed checked-join shutdown outcomes, bounded/background runtime shutdown, cancellation acknowledgement, blocking-driver refusal/parking, panic containment, timers, channels, and `runtime_async` capability identity all require focused proof. Upstream release notes alone do not establish a FrankenTerm performance improvement.
- **Upstream rationale:** releases 0.3.6 through 0.3.10 include the current-thread timer-floor repair, ARM blocking-pool ordering fences, watch lost-update repair, merge busy-spin elimination, long-timer and cancellation-validator repairs, and cancellation-waker ownership isolation. These are upstream correctness/performance motivations, not evidence of a FrankenTerm mux, input, or rendering speedup.
- **Resolved transitive changes:** AES-GCM `0.10.3 -> 0.11.0`, ChaCha20Poly1305 `0.10.1 -> 0.11.0` alongside the retained 0.10.1 consumer, Base64 adds `0.23.1`, `franken-{kernel,evidence,decision} 0.3.5 -> 0.3.10`, and the macro crate adds Syn `3.0.3` alongside existing Syn versions.
- **Lock generation:** RCH rejected `cargo update` as non-compilation with `[RCH-E301]`, so local Cargo was used only to resolve `Cargo.lock`; no local compilation or test output counts as proof. All validation remains remote, fail-closed, and `--locked`.
- **Focused proof:** strict-remote `j-29969150772248941` passed the live runtime surface guard 6/6; `j-29969150772248942` passed 88 Cx, cancellation, LabRuntime-infrastructure, runtime-smoke, and surface tests; exact committed-source `j-29969150772248943` passed `lab_smoke` 39/39 and `pool_labruntime` 44/44 before exposing one obsolete Tokio watch assertion; exact committed-source `j-29969150772248945` then passed `proptest_runtime_compat` 64/64 with the asupersync watch-cell contract and no Proptest persistence warnings.
- **Negative evidence:** the retired `runtime_async_tests` and `integration_asupersync_migration_validation` archives each reported zero tests and are not counted as proof. The live replacements above were identified from their own audit headers and executed instead. The first replacement run's 63/64 property result is retained because it demonstrated that asupersync preserves the latest watch value across a zero-receiver gap for future subscribers; commit `038ae0c02` pins that stronger contract rather than preserving the stale Tokio error expectation.
- **Required proof before closure:** one resolved asupersync package, `runtime_async`/timer/watch/cancellation/blocking-pool/channel/LabRuntime coverage, no direct Tokio regression, workspace all-target check, warnings-denied Clippy, and exact committed-source format proof.

### FrankenSQLite / fsqlite 0.3.5

- **Status:** researched, not yet integrated. FrankenTerm currently has no `fsqlite` dependency to update; the two FrankenSQLite features are empty default-off scaffolds and the named recorder implementation still uses rusqlite. This is new backend architecture work, not a version bump.
- **Stable target (re-audited 2026-08-18):** crates.io `fsqlite 0.3.5` is the latest non-yanked stable crate at checksum `0d5c359d988d336716ac1fe84f032a392cbd4b299ca17c2c01ab10c5d8179367`; lightweight tag `v0.3.5` resolves to commit `92a4e4e735483be136b8a73cc6bf3a5d6263dcf8`. GitHub Releases still ends at v0.3.4 and the tagged changelog has no 0.3.5 section, so adoption must review the exact tag-to-crate diff rather than infer release semantics from the Releases page. FrankenTerm has no `fsqlite` package today, so this remains a new backend integration rather than an updater bump.
- **Runtime boundary:** `fsqlite 0.3.5` requires `asupersync >=0.4.3,<0.5` and exposes runtime types publicly. Adding it before the coordinated 0.4.x migration would create the forbidden split runtime universe.
- **Concurrency/API boundary:** the direct `fsqlite::Connection` is deliberately `!Send + !Sync`, while FrankenTerm's synchronous object-safe `StorageBackend` is `Send + Sync`. A reviewed `AsyncConnection` worker/actor adapter is the plausible canary, but it cannot make the current transaction guard exclusive: `ft-ig9lh` must first ensure one owner retains a transaction from begin through commit or rollback.
- **Supported-envelope boundary:** upstream documents verification for at most eight concurrent writers and does not support ten or more implicit-autocommit writers. Adoption therefore cannot justify a 128-core scaling claim. Default features also include native, io_uring, JSON, FTS5, and RTree; the canary must use an explicit minimal reviewed feature set.
- **Sequencing:** finish the exact 0.3.10 proof; obtain new stable FastMCP and
  FastAPI releases compatible with one 0.4.x runtime family; review and pin
  those releases together with the stable FrankenSearch target; move the
  Asupersync pair to 0.4.7 with one resolved family; repair transaction
  ownership under `ft-ig9lh`; then implement the default-off Fsqlite canary
  under `ft-kcdqp`.
- **Safety/rollback:** use isolated temporary databases only, retain rusqlite as the rollback backend, require schema/FTS/type equivalence plus explicit close, worker reaping, cancellation, commit-race, no-late-write, rollback, reopen, and crash proof, and do not promote or claim performance without retained Apple Silicon and Threadripper A/B evidence.
