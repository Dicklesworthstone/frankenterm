# Ratatui/Crossterm Decommission and Removal Plan

**Bead:** wa-1q7m (FTUI-09.3)
**Date:** 2026-02-09

---

## 1  Scope

Remove the ratatui/crossterm legacy TUI backend after the ftui migration reaches Stage 3 (General Availability). This plan covers module-by-module removal, dependency cleanup, and post-removal verification.

## 2  Pre-Removal Checklist

Before executing any removal steps, all of the following must be true:

- [ ] Stage 2 (Beta) has run for >= 4 weeks with zero S1/S2 issues
- [ ] Go/no-go cutover review completed (wa-1i50)
- [ ] All P0+P1 environments pass compatibility certification
- [ ] No open `WA_TUI_BACKEND=ratatui` override usage reports
- [ ] Import guardrail tests pass (`agnostic_modules_have_no_bare_ratatui_imports`)

## 3  Module-by-Module Removal Sequence

Execute in the order listed. Each step includes its verification check.

### Phase 1: Remove Legacy Backend Modules

| Step | File | Action | Verification |
|------|------|--------|-------------|
| 1.1 | `crates/wa-core/src/tui/app.rs` | Delete | `cargo check -p wa-core --features ftui` passes |
| 1.2 | `crates/wa-core/src/tui/views.rs` | Delete | `cargo check -p wa-core --features ftui` passes |
| 1.3 | `crates/wa-core/src/tui/mod.rs` | Remove `#[cfg(feature = "tui")]` blocks for app/views imports (lines 85-93) | Compile check |

### Phase 2: Remove Compatibility Layer

| Step | File | Action | Verification |
|------|------|--------|-------------|
| 2.1 | `crates/wa-core/src/tui/ftui_compat.rs` | Remove all `#[cfg(feature = "tui")]` conversion blocks | No ratatui types in compat layer |
| 2.2 | `crates/wa-core/src/tui/ftui_compat.rs` | If only ftui conversions remain, inline into calling code and delete module | Compile check |
| 2.3 | `crates/wa-core/src/tui/mod.rs` | Remove `pub mod ftui_compat;` if module deleted | Compile check |

### Phase 3: Remove Feature Flag Plumbing

| Step | File | Action | Verification |
|------|------|--------|-------------|
| 3.1 | `crates/wa-core/src/lib.rs` | Remove `compile_error!` for `tui+ftui` mutual exclusion (line 129-131) | Compile check |
| 3.2 | `crates/wa-core/src/lib.rs` | Remove `cfg(feature = "tui")` guards | Compile check |
| 3.3 | `crates/wa-core/src/tui/mod.rs` | Remove `#[cfg(feature = "ftui")]` guards — make ftui the unconditional backend | Compile check |
| 3.4 | `crates/wa-core/src/tui/mod.rs` | Remove import guardrail tests (no longer needed) | Test suite still passes |
| 3.5 | `crates/wa-core/src/logging.rs` | Replace `cfg(any(feature = "tui", feature = "ftui"))` with `cfg(feature = "ftui")`, then simplify | Compile check |
| 3.6 | `crates/wa-core/src/crash.rs` | Same cfg simplification | Compile check |
| 3.7 | `crates/wa-core/src/replay.rs` | Same cfg simplification | Compile check |

### Phase 4: Remove Cargo Dependencies

| Step | File | Action | Verification |
|------|------|--------|-------------|
| 4.1 | `crates/wa-core/Cargo.toml` | Remove `ratatui` and `crossterm` from `[dependencies]` | `cargo check` passes |
| 4.2 | `crates/wa-core/Cargo.toml` | Remove `tui` from `[features]` section (line 120) | Feature no longer exists |
| 4.3 | `Cargo.toml` (workspace) | Remove `ratatui` and `crossterm` from `[workspace.dependencies]` (lines 197-198) | `cargo check` passes |
| 4.4 | Run `cargo update` | Prune lock file | `Cargo.lock` shrinks |

### Phase 5: Rename Feature Flag

| Step | File | Action | Verification |
|------|------|--------|-------------|
| 5.1 | `crates/wa-core/Cargo.toml` | Rename `ftui` feature to `tui` (now the only backend) | Compile check |
| 5.2 | All source files | Replace `cfg(feature = "ftui")` with `cfg(feature = "tui")` | Compile check |
| 5.3 | `crates/wa/Cargo.toml` | Update feature references | Compile check |
| 5.4 | CI workflows | Update `--features ftui` to `--features tui` | CI passes |
| 5.5 | Documentation | Update all feature flag references | Grep for "ftui" returns 0 hits |

## 4  Shared Module Audit

These modules are currently shared between backends. After ratatui removal, they may be simplified:

| Module | Current State | Post-Removal Action |
|--------|--------------|---------------------|
| `terminal_session.rs` | Has `CrosstermSession` impl gated on `tui` | Remove `CrosstermSession` impl, keep `FtuiSession` |
| `command_handoff.rs` | Backend-agnostic | Keep as-is (mod.rs DELETION marker is conservative) |
| `output_gate.rs` | Backend-agnostic | Keep as-is; review once ftui owns output routing |
| `keymap.rs` | Backend-agnostic | Keep as-is; remove legacy parity tests |
| `state.rs` | Backend-agnostic | Keep as-is |
| `view_adapters.rs` | Backend-agnostic | Keep as-is |

## 5  Post-Removal Verification Checklist

After all removal phases complete:

- [ ] `cargo check -p wa-core` (headless) passes
- [ ] `cargo check -p wa-core --features tui` (renamed from ftui) passes
- [ ] `cargo check -p wa --features tui` passes
- [ ] `cargo test -p wa-core --features tui --lib` — all tests pass (320+)
- [ ] `cargo clippy -p wa-core --features tui -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `grep -r "ratatui\|crossterm" crates/` returns only comments/docs/changelogs
- [ ] `grep -r 'feature.*"tui".*"ftui"' crates/` returns 0 hits (no dual-feature code)
- [ ] Binary size comparison before/after (expect ~15-20% reduction)
- [ ] `Cargo.lock` diff shows ratatui + crossterm + transitive deps removed

## 6  Guardrails

### Compile-Time Prevention of Reintroduction

The import guardrail tests in `tui/mod.rs` (wa-sgy2 / FTUI-09.3.a) prevent accidental reintroduction of ratatui/crossterm:

- `agnostic_modules_have_no_bare_ratatui_imports` — scans migration-complete modules
- `no_new_ratatui_modules_without_allowlist` — catches new modules with ratatui refs
- `allowed_files_list_is_consistent` — validates allowlist entries exist

After removal, these tests can be simplified to a single check:
```rust
#[test]
fn no_ratatui_references_in_codebase() {
    // After FTUI-09.3, ratatui/crossterm should not appear anywhere
    // in non-comment code within the tui module.
}
```

### CI Enforcement

- The `tui` feature (formerly `ftui`) is the only TUI build target
- CI must NOT have a `--features tui,ftui` combination (removed)
- Add a CI step that greps for `ratatui` in source (fail if found outside comments)

## 7  Timeline

| Phase | Estimated Effort | Dependencies |
|-------|-----------------|--------------|
| Phase 1: Legacy modules | 30 min | Go/no-go approval |
| Phase 2: Compat layer | 1 hour | Phase 1 |
| Phase 3: Feature flags | 1 hour | Phase 2 |
| Phase 4: Cargo deps | 15 min | Phase 3 |
| Phase 5: Rename | 1 hour | Phase 4 |
| **Total** | ~4 hours | Sequential |

## 8  References

- `docs/ftui-rollout-strategy.md` — Rollout stages and transitions
- `docs/ftui-cargo-feature-matrix.md` — Current feature matrix
- `docs/ftui-contributor-migration-guide.md` — Contributor migration guide
- `crates/wa-core/src/tui/mod.rs` — Import guardrail tests (FTUI-09.3.a)
