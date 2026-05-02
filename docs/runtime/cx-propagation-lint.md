# Cx Propagation Lint

**Bead:** `ft-t9a6q.1` (BR-RC-RUNTIME-SEMANTICS.G14.1).
**Crate:** `lints/cx_propagation/`.
**Doctrine:** [`runtime-proof-trait.md`](runtime-proof-trait.md).
**Sibling audit:** [`scripts/check_runtime_proof_coverage.py`](../../scripts/check_runtime_proof_coverage.py) (ft-3kv6e ratchet).

The cx-propagation lint enforces that every `pub async fn` in
`crates/frankenterm-core/src/` takes `&Cx` (or threads one through
a `RuntimeProof` bound). It is the type-level companion to the
ft-3kv6e ratchet — the audit script catches drift at script-time;
this lint catches it via a real Rust AST analyzer with a fixture
corpus that asserts on the rule shape.

## Analyzer and Dylint plugin surfaces

The first shipped surface is a stable-Rust analyzer using `syn`:

- Same rule shape as the eventual `LateLintPass`.
- Same allow-list (mirrored from the Python script).
- Fixture corpus asserts rule semantics in unit + integration tests.
- Runnable as `cargo run -p cx_propagation_lint --
  crates/frankenterm-core/src` today.

The second surface is the Dylint compile-time plugin shipped under
**ft-rca2p**. The crate has a `cdylib` lib target and a gated
`cx_propagation_lint::dylint_plugin::LateLintPassImpl` implementation
that walks rustc HIR items and impl items. It reuses
`allow_list::EXEMPT_FILES` and `allow_list::WRAPPER_EXEMPTIONS`,
checks `FnSig::header.is_async()`, HIR parameter `TyKind::{Ref, Path}`,
generic arguments, `impl RuntimeProof`, and where-clause bounds, then
emits the same missing-`Cx` lint as the analyzer through `rustc_lint`.

Because Dylint rides rustc internals, the plugin build is pinned by
`lints/cx_propagation/rust-toolchain.toml` to nightly with
`rustc-dev`, `clippy`, and `rustfmt` components.

## Rule

A `pub async fn` (or `pub(crate)` / `pub(super)` etc.) is *covered*
if its signature contains any of:

- `&Cx` / `&mut Cx` reference parameter.
- Owned `Cx` parameter (sealed via `impl RuntimeProof for Cx`).
- Generic bound `: RuntimeProof` (in either generic params or `where` clause).
- `impl RuntimeProof` parameter type.

Any path to `Cx` is accepted (`&crate::cx::Cx`,
`&frankenterm_core::cx::Cx`) — the analyzer matches on the last
segment of the path so re-export aliases work.

A `pub async fn` is *exempt* (not subject to the rule) if it is:

1. Defined in a file listed in `EXEMPT_FILES`, OR
2. Listed in `WRAPPER_EXEMPTIONS` as a `(file, fn_name)` pair.

Anything else is *uncovered* and reported as a finding.

## Allow-list policy

### `EXEMPT_FILES`

Runtime-layer modules that *are* the seal. They define `Cx` and
`RuntimeProof` themselves; requiring them to take a `Cx` parameter
is circular.

| File | Reason |
| ---- | ------ |
| `runtime_async.rs` | Wrapper module — primitives sealed elsewhere |
| `runtime_proof.rs` | Defines the seal itself |
| `cx.rs`            | The canonical structured-async witness |
| `cx_stub.rs`       | Build-time stub of `cx.rs` (no-op shim) |

Add sparingly — every entry is permanent doctrine.

### `WRAPPER_EXEMPTIONS`

Ergonomic wrappers around a `_with_cx` / `_cx` sibling. The wrapper
constructs a default `Cx` internally and delegates. Each entry
*must* be paired with a real covered sibling on the same file —
the analyzer enforces this. A stale wrapper exemption (entry with
no matching `pub async fn` in the file) is itself a lint failure
(`StaleWrapperExemption`).

### Adding a new wrapper exemption

1. Confirm the function is genuinely a wrapper — constructs a
   default `Cx`, delegates to a covered sibling. NOT a "haven't
   gotten around to threading `Cx` yet" placeholder.
2. Add a comment in the source explaining why the wrapper is safe
   to exempt.
3. Add an entry in `lints/cx_propagation/src/allow_list.rs` AND
   in `scripts/check_runtime_proof_coverage.py`. The two lists
   must stay in lockstep; the `cont.drift` follow-up will surface
   any divergence.

## Substrate vs wired-pass scope

Same substrate-pass / wired-pass split as the tmux compat matrix
under ft-53zsr:

**Substrate-pass (shipped under ft-t9a6q.1):**
- Lint crate builds + runs (`cargo run -p cx_propagation_lint`).
- 15 unit tests on rule semantics (parameter shapes, generic
  bounds, where clauses, impl-block methods, `#[cfg(test)]`
  module skipping, `pub(crate)` visibility).
- 8 integration tests against a fixture corpus
  (`tests/fixtures/`).
- Allow-list documented inline.
- Substrate `WRAPPER_EXEMPTIONS` carrying the load-bearing
  runtime-layer entries.
- `--json` output mode for downstream consumers (the burn-down
  dashboard under ft-t9a6q.2).

**Wired-pass follow-ups:**
- `ft-t9a6q.1.cont.dylint` (**ft-rca2p, closed**): real
  `LateLintPass` plugin in
  `cx_propagation_lint::dylint_plugin::LateLintPassImpl`, built as
  a `cdylib` with the crate's `dylint` feature and pinned by
  `lints/cx_propagation/rust-toolchain.toml`.
- `ft-t9a6q.1.cont.allowlist` (**ft-l8bmk, closed**): full
  377-entry port from the Python script. Rust analyzer now at
  parity — both surfaces report 0 uncovered against the real
  core src tree.
- `ft-t9a6q.1.cont.drift` (**ft-jbsbx, closed**): lockstep
  guard at `scripts/check_cx_propagation_lockstep.py`. Imports
  the Python module + parses `lints/cx_propagation/src/allow_list.rs`,
  diffs the two `EXEMPT_FILES` + `WRAPPER_EXEMPTIONS` sets, exits 1 on
  any drift. Regression test at
  `crates/frankenterm-core/tests/cx_propagation_lockstep_guard.rs`
  shells out to the script and fails the build on drift.
- `ft-t9a6q.1.cont.ci` (**ft-s2034, closed**): PR-CI YAML
  wiring. Landed in two passes per the parent epic's
  warning-then-error cadence:
  1. **Warning mode** at commit `2fdc207a1`: step "Run
     cx-propagation lint (warning mode, ft-s2034)" with
     `continue-on-error: true`, so findings annotated PRs but
     did not block merges. Allowed the analyzer to mature
     against real PR traffic without false-positive blast
     radius.
  2. **Escalation to error mode** once ft-t9a6q.2 closed with
     `totals.uncovered_sites=0`: `continue-on-error: true`
     removed, step renamed to "Run cx-propagation lint
     (br-ft-s2034)". A reintroduced uncovered `pub async fn`
     now fails the `cargo-guards` lane on PRs + pushes to
     main with a `path:line: reason` line per finding.
  The Python audit at the `shell-guards` job
  (`cx_propagation_burndown.py --check`, br-ft-gsgll) enforces
  the same invariant via regex heuristics; both gates run, and
  the lockstep guard (ft-jbsbx) keeps the two allowlists in
  sync. Together they cover a strictly wider regression class
  than either alone.

### Lockstep guard usage

```bash
# Check + exit 1 on drift (CI-friendly).
scripts/check_cx_propagation_lockstep.py

# Check + always print the diff (operator debug).
scripts/check_cx_propagation_lockstep.py --print

# Machine-readable JSON.
scripts/check_cx_propagation_lockstep.py --json
```

The script is the source of truth for "are the two allow-lists
equivalent?" — both enforcement surfaces (Python audit + Rust
analyzer) must agree on every entry. Adding a new exemption
requires updating both files in the same commit; the lockstep
guard catches the case where one half lands without the other.

## Running locally

```bash
cargo run -p cx_propagation_lint -- crates/frankenterm-core/src
```

Exit code 0 = clean. Exit code 1 = at least one finding. Exit
code 2 = analyzer failure (bad path, walk error).

JSON output:

```bash
cargo run -p cx_propagation_lint -- --json crates/frankenterm-core/src
```

Shape:

```json
{
  "total_pub_async_sites": <int>,
  "exempt_file_sites": <int>,
  "wrapper_exempt_sites": <int>,
  "covered_sites": <int>,
  "uncovered_sites": <int>,
  "stale_exemption_sites": <int>,
  "findings": [
    {"path":"...","line":42,"fn_name":"...","reason":"missing_cx"}
  ]
}
```

The shape mirrors the Python audit script's `--json` output so
the burn-down dashboard (ft-t9a6q.2) can consume either source
interchangeably.

## Building the Dylint plugin

The plugin is feature-gated so ordinary workspace analyzer tests do
not require rustc-private crates. Build the loadable library with the
lint crate's pinned nightly:

```bash
cargo build -p cx_propagation_lint --lib --features dylint
```

Once `cargo-dylint` and `dylint-link` are available on the host, the
same HIR pass can be listed or run from the lint crate/workspace using
the generated cdylib. The expected lint name is `cx_propagation`.
The stable analyzer remains the count oracle:

```bash
cargo run -p cx_propagation_lint -- --json crates/frankenterm-core/src
```

## Cross-references

- [`scripts/check_runtime_proof_coverage.py`](../../scripts/check_runtime_proof_coverage.py) — ft-3kv6e ratchet (Python audit + JSON baseline).
- [`crates/frankenterm-core/src/runtime_proof.rs`](../../crates/frankenterm-core/src/runtime_proof.rs) — the sealed trait the lint enforces.
- [`crates/frankenterm-core/src/cx.rs`](../../crates/frankenterm-core/src/cx.rs) — the canonical structured-async witness.
- [`runtime-proof-trait.md`](runtime-proof-trait.md) — doctrine for why `&Cx` propagation matters.
- ft-t9a6q parent epic — Cx-first migration.
- ft-t9a6q.2 — burn-down dashboard + sprint (consumes this analyzer's `--json` output).
- ft-t9a6q.3 — LabRuntime virtual-time test framework (orthogonal substrate).
