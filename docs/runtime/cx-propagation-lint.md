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

## Why a syn-based analyzer rather than a cargo-dylint plugin

The bead title specifies "dylint", and a real `LateLintPass` is the
target end state. But cargo-dylint requires a pinned nightly
toolchain because the rustc internals it rides on aren't stable —
shipping a dylint plugin in the same commit as the rule shape
would couple the lint to a nightly that other agents working in
this repo aren't on.

The substrate ship is a stable-Rust analyzer using `syn`:

- Same rule shape as the eventual `LateLintPass`.
- Same allow-list (mirrored from the Python script).
- Fixture corpus asserts rule semantics in unit + integration tests.
- Runnable as `cargo run -p cx_propagation_lint --
  crates/frankenterm-core/src` today.

The dylint migration drops in against the same analyzer surface
and is filed as **ft-t9a6q.1.cont.dylint**. The substrate-first
shape matches the format-substrate-first pattern under ft-kscfg /
ft-5te6x / ft-hs5f6.

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

**Substrate-pass (shipped):**
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

**Wired-pass (deferred follow-ups):**
- `ft-t9a6q.1.cont.dylint`: real `LateLintPass` plugin with
  pinned nightly toolchain. Drops in against the same analyzer
  surface.
- `ft-t9a6q.1.cont.allowlist`: port the long tail of
  `WRAPPER_EXEMPTIONS` from the Python script (~150 entries) into
  the Rust crate. Until then, the Rust analyzer surfaces a
  superset of findings vs the Python ratchet — by design.
- `ft-t9a6q.1.cont.drift`: CI sweep that fails if the Rust
  allow-list and the Python allow-list drift apart.
- `ft-t9a6q.1.cont.ci`: PR-CI YAML wiring. Initially as a
  warning; escalate to error after the burn-down completes.

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

## Cross-references

- [`scripts/check_runtime_proof_coverage.py`](../../scripts/check_runtime_proof_coverage.py) — ft-3kv6e ratchet (Python audit + JSON baseline).
- [`crates/frankenterm-core/src/runtime_proof.rs`](../../crates/frankenterm-core/src/runtime_proof.rs) — the sealed trait the lint enforces.
- [`crates/frankenterm-core/src/cx.rs`](../../crates/frankenterm-core/src/cx.rs) — the canonical structured-async witness.
- [`runtime-proof-trait.md`](runtime-proof-trait.md) — doctrine for why `&Cx` propagation matters.
- ft-t9a6q parent epic — Cx-first migration.
- ft-t9a6q.2 — burn-down dashboard + sprint (consumes this analyzer's `--json` output).
- ft-t9a6q.3 — LabRuntime virtual-time test framework (orthogonal substrate).
