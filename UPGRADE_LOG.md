# Dependency Upgrade Log

**Date:** 2026-01-26
**Project:** FrankenTerm (ft)
**Language:** Rust
**Manifest:** Cargo.toml (workspace), crates/frankenterm/Cargo.toml, crates/frankenterm-core/Cargo.toml, fuzz/Cargo.toml

---

## Summary

| Metric | Count |
|--------|-------|
| **Total dependencies** | 29 |
| **Updated** | 27 |
| **Skipped** | 2 |
| **Failed (rolled back)** | 0 |
| **Requires attention** | 27 |

---

## Successfully Updated

## Skipped

### fancy-regex: 0.14
**Reason:** Already at latest stable (0.14.0)

### base64: 0.22.1
**Reason:** Already at latest stable (0.22.1)

## Failed Updates (Rolled Back)

## Requires Attention

### clap: 4.5 → 4.5.54

**Changelog:** crates.io / release notes (latest patch series)

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` ran but was interrupted due to prolonged global cargo lock contention; will rerun in finalize stage.

### serde: 1.0 → 1.0.228

**Changelog:** crates.io / release notes (latest patch series)

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on cargo artifact lock; will rerun in finalize stage.

### serde_json: 1.0 → 1.0.149

**Changelog:** crates.io / release notes (latest patch series)

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on cargo artifact lock; will rerun in finalize stage.

### tokio: 1.43 → 1.49.0

**Changelog:** docs.rs / crates.io (latest stable tag)

**Breaking changes:** Possible behavioral changes across minor versions; no specific API removals noted in patch notes.

**Tests:** ⏳ `cargo test` blocked on cargo artifact lock; will rerun in finalize stage.

### anyhow: 1.0 → 1.0.100

**Changelog:** docs.rs / crates.io (latest stable tag)

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on cargo artifact lock; will rerun in finalize stage.

### tracing: 0.1 → 0.1.44

**Changelog:** cargo-run / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on package cache lock; will rerun in finalize stage.

### tracing-subscriber: 0.3 → 0.3.22

**Changelog:** crates.io / release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### toml: 0.8 → 0.8.23

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on package cache/artifact locks; will rerun in finalize stage.

### toml_edit: 0.22 → 0.24.0

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** Minor feature updates; no API removals noted.

**Notes:** Cargo warned about semver metadata; pinned to `0.24.0` to avoid metadata warnings.

**Tests:** ⏳ `cargo test` blocked on cache/artifact locks after downloading; will rerun in finalize stage.

### toon_rust: git master (4df74c0a → 788589d7)

**Changelog:** git commit update (no tagged release)

**Breaking changes:** Unknown; requires downstream tests.

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### dirs: 5.0 → 6.0.0

**Changelog:** crates.io / release notes

**Breaking changes:** Major version bump; verify API compatibility during final test run.

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### assert_cmd: 2.0 → 2.1.2

**Changelog:** crates.io / release notes

**Breaking changes:** Minor updates; no major API removals noted.

**Tests:** ⏳ `cargo test` blocked on cache/artifact locks; will rerun in finalize stage.

### predicates: 3.1 → 3.1.3

**Changelog:** crates.io / release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### thiserror: 2.0 → 2.0.18

**Changelog:** crates.io / release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### aho-corasick: 1.1 → 1.1.4

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### memchr: 2.7 → 2.7.6

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### regex: 1.10 → 1.12.2

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (minor updates; no removals noted in changelog)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### rand: 0.8 → 0.9.2

**Changelog:** rust-random upgrade guide / crates.io release notes

**Breaking changes:** API rename in 0.9 (e.g., `Rng::gen` → `Rng::random`, `Distribution::sample` → `Distribution::random`). Code may need updates if these APIs are used.

**Tests:** ⏳ `cargo test` blocked on package cache lock; will rerun in finalize stage.

### sha2: 0.10 → 0.10.9

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on package cache/artifact locks; will rerun in finalize stage.

### rusqlite: 0.32 → 0.38.0

**Changelog:** GitHub releases / crates.io release notes

**Breaking changes:** Minor/patch updates across multiple releases; review release notes for API/feature changes (e.g., updated libsqlite3-sys, hashlink).

**Tests:** ⏳ `cargo test` blocked on package cache lock; will rerun in finalize stage.

### fs2: 0.4 → 0.4.3

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### ratatui: 0.29 → 0.30.0

**Changelog:** ratatui 0.30.0 release notes

**Breaking changes:** 0.30 introduces modularization (new `ratatui-*` crates) and updated dependency surface; review API changes for any imports/paths.

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### crossterm: 0.28 → 0.29.0

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (minor update), but verify any terminal backend changes after ratatui upgrade.

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### proptest: 1.5 → 1.8.0

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (minor update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### tempfile: 3.10 → 3.23.0

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (minor update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### criterion: 0.5 → 0.7.0 (latest compatible)

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** Minor updates; note latest 0.8.1 requires newer Rust (MSRV 1.86+), so pinned to 0.7.0 to respect workspace rust-version 1.85.

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

### libfuzzer-sys: 0.4 → 0.4.10

**Changelog:** docs.rs / crates.io release notes

**Breaking changes:** None expected (patch update)

**Tests:** ⏳ `cargo test` blocked on artifact lock; will rerun in finalize stage.

## Deprecation Warnings Fixed

| Package | Warning | Fix Applied |
|---------|---------|-------------|

## Security Notes

**Vulnerabilities resolved:** None detected (cargo audit exit 0)

**New advisories:** None reported

**Audit command:** `cargo audit` (ran)

---

## Post-Upgrade Checklist

- [ ] All tests passing
- [ ] No deprecation warnings
- [ ] Manual smoke test performed
- [ ] Documentation updated (if needed)
- [ ] Changes committed

---

## Commands Used

```bash
# Update commands
cargo update -p clap
cargo update -p serde
cargo update -p serde_json
cargo update -p tokio
cargo update -p anyhow
cargo update -p tracing
cargo update -p tracing-subscriber
cargo update -p toml
cargo update -p toml_edit@0.23.10+spec-1.0.0 --precise 0.24.0+spec-1.1.0
cargo update -p toon_rust
cargo update -p dirs
cargo update -p assert_cmd
cargo update -p predicates
cargo update -p thiserror
cargo update -p aho-corasick
cargo update -p memchr
cargo update -p regex
cargo update -p rand@0.9.2
cargo update -p sha2
cargo update -p rusqlite
cargo update -p fs2
cargo update -p ratatui
cargo update -p crossterm@0.29.0
cargo update -p proptest
cargo update -p tempfile
cargo update -p criterion
cargo update -p libfuzzer-sys

# Test commands
timeout 600 cargo test

# Audit commands
cargo audit
```

---

## Notes

- Process follows library-updater skill: research each dependency before update; update one at a time; test after each.
- Cleanup steps that would delete files are skipped due to repo safety rules.
