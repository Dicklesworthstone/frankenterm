# Dependency Upgrade TODO (Granular)

## Phase 0: Setup & Safety
- [ ] Confirm active manifests (root Cargo.toml, crates/wa/Cargo.toml, crates/wa-core/Cargo.toml, fuzz/Cargo.toml)
- [ ] Create/refresh `UPGRADE_LOG.md`
- [ ] Create/refresh `claude-upgrade-progress.json`
- [ ] Enumerate all dependencies (workspace, crate-specific, dev-deps)
- [ ] Determine current Rust toolchain (from rust-toolchain.toml)
- [ ] Check availability of `cargo outdated` and `cargo audit`

## Phase 1: Per-dependency loop (repeat for each dependency)
- [ ] Identify current version in manifest or Cargo.lock
- [ ] Research breaking changes (software-research skill + web sources)
- [ ] Decide target latest stable version
- [ ] Update manifest version or run `cargo update -p` (one dep at a time)
- [ ] Regenerate Cargo.lock
- [ ] Run tests (`cargo test`)
- [ ] If tests fail: attempt fix (max 3 attempts), documenting steps
- [ ] If still failing: rollback dependency change and log failure
- [ ] Update `UPGRADE_LOG.md` entry
- [ ] Update `claude-upgrade-progress.json`

## Phase 2: Finalization
- [ ] Run full test suite (`cargo test`)
- [ ] Run required checks (`cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`)
- [ ] Run security audit (`cargo audit`) if available
- [ ] Update summary table in `UPGRADE_LOG.md`
- [ ] Post Agent Mail update in thread `wa-y6g`

## Phase 3: wa-y6g (Schema Migration Framework) continuation
- [ ] Finish CLI wiring for `wa db migrate` in `crates/wa/src/main.rs`
- [ ] Add migration status/plan output formatting
- [ ] Add migration tests (upgrade + rollback)
- [ ] Run required checks after changes
- [ ] Update bead status and Agent Mail
