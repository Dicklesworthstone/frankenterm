# Final Supported-Path Truth Sweep (`ft-xbnl0.3.6`)

This document records the final supported-path honesty sweep over the
`ft-xbnl0.3` finish-line program. It builds on the inventory from
`ft-xbnl0.1.2` and the verification contract from `ft-xbnl0.1.4`.

The sweep was executed by CC-2 on 2026-04-15 against
`/Users/jemanuel/projects/frankenterm` at the working tree noted in the
closing comment.

## Scope

The sweep re-verifies the six finish-line surfaces enumerated by
`ft-xbnl0.1.2`:

1. Control-plane / client truth — `ft-xbnl0.3.1` (closed)
2. Live scrollback / FTUI truth — `ft-xbnl0.3.2` (closed)
3. Mux-server pane/window operations — `ft-xbnl0.3.3` (closed)
4. Tmux / detach / SSH / remote-file semantics — `ft-xbnl0.3.4` (closed)
5. Rendering / window-backend truth — `ft-xbnl0.3.5` (open)
6. Final supported-path honesty sweep — `ft-xbnl0.3.6` (this doc)

For each surface the sweep checks three things in agreement:

- code: production paths do not return fake values, panic with
  `unimplemented!()`, or pretend a capability is supported when it is not;
- docs: rendered `docs/` references match the support claims;
- operator-visible semantics: error messages, robot-mode envelopes, and
  doc-strings cite the same matrix.

## Method

The sweep uses ripgrep to enumerate every production-path
`unimplemented!()`, `todo!()`, `panic!("not implemented")`, and
`transport not wired` site in the workspace, then classifies each match
against the inventory from `ft-xbnl0.1.2` using these decision rules:

| Marker location | Classification |
|-----------------|----------------|
| Inside `#[cfg(test)]` / `#[cfg(all(test, ...))]` / `mod tests` | test-only — not finish-line scope |
| Inside `#[cfg(not(any(target_os = "linux", target_os = "macos")))]` | unsupported-platform shim — out of scope per inventory |
| Inside a non-Rust SDK template renderer | template-only — out of scope per inventory exclusion (Rust is the supported SDK) |
| Anywhere else with a tracked `ft-xbnl0` / `ft-akx00` owner bead | acknowledged finish-line gap with a named owner |
| Anywhere else with no tracked owner | new finding — must be filed as a follow-up bead before close |

A site is allowed to remain only if (a) it falls into one of the
test-only / platform-shim / template-only buckets *and* the inventory
exclusion list cites it, or (b) a `ft-xbnl0.*` / `ft-akx00.*` bead owns
the gap.

## Findings

### Production-path `unimplemented!()` matches

All matches enumerated by

```bash
rg -nC1 'unimplemented!\(' --type rust
```

map to:

| File / line | Classification | Owner |
|-------------|----------------|-------|
| `frankenterm/mux/src/pane.rs:609-715` | test-only `FakePane` (line 595: `#[cfg(test)] mod test`) | inventory exclusion |
| `frankenterm/mux/src/tab.rs:4078-4170` | test-only `FakePane` (line 4004: `#[cfg(test)] mod test`) | inventory exclusion |
| `frankenterm/termwiz/src/render/terminfo.rs:1177` | test-only `FakeTerm::waker` (line 991: `#[cfg(all(test, unix))] mod test`) | inventory exclusion |
| `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:1682` | test-only `FakePane::writer` (line 1510: `#[cfg(test)] mod tests`) | inventory exclusion |
| `crates/frankenterm-core/src/robot_sdk_contracts.rs:2299/2318` | assertion guards — verify Rust SDK never re-introduces `unimplemented!(`/`transport not wired` markers | n/a (this sweep) |
| `frankenterm/window/src/bitmaps/mod.rs:70` (`SrgbTexture2d::read`) | production gap (texture readback) | `ft-akx00.6.3` → `ft-xbnl0.3.5` (both open) |
| `frankenterm/window/src/bitmaps/mod.rs:454` (`ImageTexture::read`) | production gap (texture readback) | `ft-akx00.6.3` → `ft-xbnl0.3.5` (both open) |
| `crates/frankenterm-gui/src/termwindow/webgpu.rs:113` (`WebGpuTexture::read`) | production gap (texture readback) | `ft-akx00.6.3` → `ft-xbnl0.3.5` (both open) |

No supported-path `unimplemented!()` exists outside the inventory
exclusions or the open finish-line owners. No anonymous fake remains.

Static caller analysis (`rg 'fn read\(&self, .* Rect, .* BitmapImage\)'`
plus `rg '\.read\(.*BitmapImage|texture\.read\('`) finds zero callers of
`Texture2d::read` in the workspace, which is consistent with the
`ft-akx00.6.3` design note that readback is currently advertised but
never invoked. This narrows the user-visible blast radius for the open
gap to "anyone who calls the trait method", which today is no one.

### `todo!()` matches

```bash
rg -n 'todo!\(' --type rust
```

returns no matches in the workspace.

### `panic!("not implemented" / "unimplemented" / "placeholder" / "todo")`

```bash
rg -in 'panic!\(.*not.implemented|panic!\(.*unimplemented|panic!\(.*placeholder|panic!\(.*todo' --type rust
```

returns no matches in the workspace, including the Go SDK template's
`panic("transport not wired")` (which lives in a quoted Go-source string
inside a Rust string literal, not in a Rust `panic!`).

### `"transport not wired"` matches

```bash
rg -n 'transport not wired'
```

matches:

| File / line | Classification | Owner |
|-------------|----------------|-------|
| `crates/frankenterm-core/src/robot_sdk_contracts.rs:917` (Python template) | template-only — non-Rust SDK | inventory exclusion |
| `crates/frankenterm-core/src/robot_sdk_contracts.rs:943` (TypeScript template) | template-only — non-Rust SDK | inventory exclusion |
| `crates/frankenterm-core/src/robot_sdk_contracts.rs:998` (Go template) | template-only — non-Rust SDK | inventory exclusion |
| `crates/frankenterm-core/src/robot_sdk_contracts.rs:2298+` | assertion guard for the Rust SDK | n/a |
| `.beads/issues.jsonl` | bead history | n/a |

The inventory excludes non-Rust SDK templates from the finish-line
supported matrix. The sweep tightens this exclusion in code by:

- adding a fully-supported gating method on `SdkLanguage`:

  ```rust
  impl SdkLanguage {
      pub fn is_fully_supported(&self) -> bool {
          matches!(self, Self::Rust)
      }
  }
  ```

- promoting the doc comments on `Python`, `TypeScript`, and `Go` to call
  out the `transport not wired` stub explicitly so any future SDK
  consumer sees the narrowed support contract on first read;

- adding a regression test
  `ft_xbnl0_3_6_only_rust_sdk_target_is_finish_line_supported` that
  asserts (a) only `SdkLanguage::Rust` reports `is_fully_supported() ==
  true` and (b) each non-Rust template still emits its `transport not
  wired` marker so a future quiet wiring change cannot silently widen
  the supported matrix without flipping `is_fully_supported`.

Operator-visible docs already match: the only `docs/extensions/sdk/`
quickstart is `rust-quickstart.md`. No doc advertises Python, TypeScript,
or Go SDKs as supported finish-line targets.

### Unsupported-platform shims

```bash
rg -nC2 '#\[cfg\(not\(any\(target_os' crates/frankenterm-core/src
```

The only finish-line-relevant shim is
`crates/frankenterm-core/src/telemetry.rs::collect_system_memory()`
returning `(0, 0)` under `#[cfg(not(any(target_os = "linux", target_os
= "macos")))]`. The function's doc comment already labels it explicitly
as a "stub for unsupported platforms", which matches the inventory
exclusion. No widening required.

### Test-only null adapter

`crates/frankenterm-core/src/fleet_scrollback_coordinator.rs` declares
`NullPaneScrollbackAccess` under `#[cfg(test)]` (line 559) with the doc
comment "Test-only null adapter preserving the historical placeholder
behavior for regression coverage." This matches the inventory note that
the live blocker is the real scrollback wiring, not removing a
production null object. No widening required.

## Residual Risks

1. `ft-xbnl0.3.5` (rendering / window-backend truth) is still open and
   carries the four `ft-akx00.6.{1,2,3,5}` blockers identified in the
   inventory. The texture-readback `unimplemented!()` sites in
   `bitmaps/mod.rs` and `webgpu.rs` will be revisited by `ft-xbnl0.3.5`'s
   closing pass; the sweep documents that they remain owned and
   tracked, not silently fake.
2. The non-Rust SDK templates remain template-only. If the supported
   matrix is ever widened to Python, TypeScript, or Go, the
   `is_fully_supported` gate must flip and the regression test must be
   updated alongside the wiring.
3. CI does not yet enforce a workspace-wide grep for `unimplemented!()`
   on supported-path crates. `ft-xbnl0.5.2` will pick that guardrail
   up when it lands; this sweep records the contract in code and docs
   so the guardrail has something to anchor against.

## Verification Commands

The sweep is anchored by two new artifacts in this commit:

- the regression test
  `ft_xbnl0_3_6_only_rust_sdk_target_is_finish_line_supported` in
  `crates/frankenterm-core/src/robot_sdk_contracts.rs`;
- the deterministic E2E harness
  `tests/e2e/test_ft_xbnl0_3_6_supported_path_truth_sweep.sh` which
  runs the cargo verification, re-runs the ripgrep classifications, and
  emits a structured artifact bundle under
  `tests/e2e/artifacts/goal-line/ft-xbnl0.3.6/<run>/`.

Recommended remote-verification commands (per the
`ft-xbnl0` verification contract):

```bash
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0-3-6-test \
  cargo test -p frankenterm-core --lib \
  ft_xbnl0_3_6_only_rust_sdk_target_is_finish_line_supported -- --nocapture
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0-3-6-check \
  cargo check -p frankenterm-core --lib --tests
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0-3-6-clippy \
  cargo clippy --no-deps -p frankenterm-core --lib --tests -- -D warnings
rch exec -- cargo fmt --check
bash tests/e2e/test_ft_xbnl0_3_6_supported_path_truth_sweep.sh
```

The harness writes `summary.json` and `structured.log` per the
`ft-xbnl0.1.4` artifact contract.
