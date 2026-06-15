# Dependency Upgrade Log

**Started:** 2026-06-14  |  **Updated:** 2026-06-15  |  **Project:** frankenterm  |  **Language:** Rust + GitHub Actions

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
