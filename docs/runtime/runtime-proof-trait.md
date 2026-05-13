# RuntimeProof — Type-Level Tokio Seal

**Bead:** [`ft-i2eni.1`](#) (BR-RC-DOCTRINE.G1.1) · **Module:** `frankenterm_core::runtime_proof`

## What it is

`RuntimeProof` is a sealed trait. Any type that implements it has been
explicitly enumerated by `runtime_proof.rs` as a primitive belonging to
this project's asupersync-backed runtime. External crates — including
`tokio` — physically cannot satisfy it because the supertrait
`runtime_proof::sealed::Sealed` lives in a private module that they
cannot name.

```rust
use frankenterm_core::runtime_proof::{RuntimeProof, assert_runtime_proof};
use frankenterm_core::runtime_async::Mutex;

let m: Mutex<i32> = Mutex::new(0);
assert_runtime_proof(&m); // ok
```

The same call against `tokio::sync::Mutex` is a compile error. That is
the whole point of the trait: tokio leakage in `frankenterm-core`
becomes a *type* error rather than a lint or grep failure.

## Why this exists

`AGENTS.md` states *"direct `tokio` usage is forbidden."* Today that
rule is enforced by:

1. Grep guards in `scripts/check_no_runtime_regression.sh`.
2. `cargo deny` `[bans]` against tokio.
3. Code review.

All three are *runtime* checks. A clever import path or a vendored
re-export sneaks past every one of them. The bridge plan
([`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md)
G1) called for *structural impossibility*: a way to prove tokio cannot
appear in core, full stop. The sealed-trait pattern is exactly that.

## What's sealed today

| Type | Origin | Sealed |
|------|--------|--------|
| `runtime_async::Mutex<T>` | local newtype | ✅ |
| `runtime_async::RwLock<T>` | local newtype | ✅ |
| `runtime_async::Semaphore` | local newtype | ✅ |
| `runtime_async::broadcast::Sender<T>` / `Receiver<T>` | local newtype | ✅ |
| `runtime_async::oneshot::Sender<T>` / `Receiver<T>` | local newtype | ✅ |
| `runtime_async::task::JoinHandle<T>` / `JoinSet<T>` | local newtype | ✅ |
| `runtime_async::Runtime` | local newtype | ✅ |
| `cx::Cx` | local | ✅ |
| `runtime_async::mpsc::*` (Sender/Receiver) | re-export of `asupersync::channel::mpsc::*` | ❌ (orphan rule) |
| `runtime_async::watch::*` | re-export of `asupersync::channel::watch::*` | ❌ (orphan rule) |
| `runtime_async::notify::Notify` | re-export of `asupersync::sync::Notify` | ❌ (orphan rule) |

The four un-sealed surfaces above are still gated transitively when
their callers thread `&Cx`, since `Cx: RuntimeProof`. Sealing them
directly requires wrapping them in local newtypes inside
`runtime_async`; that work is tracked under a follow-on bead.

## How to use it in new APIs

Two equivalent patterns:

### Pattern A — explicit `&impl RuntimeProof` parameter

```rust
use frankenterm_core::runtime_proof::RuntimeProof;

pub async fn drain<P: RuntimeProof>(_proof: &P) -> Vec<u8> {
    // body uses runtime_async primitives
    Vec::new()
}
```

This is the strictest form — every new public async API in core can
adopt it incrementally. Existing call-sites pass `&self` of a sealed
type (e.g., a runtime_async wrapper they already hold), which costs
nothing at runtime.

### Pattern B — thread `&Cx`

```rust
use frankenterm_core::cx::Cx;

pub async fn drain(cx: &Cx) -> Vec<u8> {
    Vec::new()
}
```

Because `Cx: RuntimeProof`, any signature that takes `&Cx` is already
witnessing a runtime-proof. This is the canonical
"structured-async-first" form and is what most public async APIs in
core already do (15 today; the workspace sweep is a follow-on).

## Adoption sweep status

The bridge plan's full acceptance criterion — *"every public async API
in core consumes `impl RuntimeProof` somewhere in its signature"* —
covers ~791 `pub async fn` sites in `crates/frankenterm-core/src/`. The
sweep is being tracked separately so reviewers can land it in
manageable chunks. This document and the canary doctest land first
(the seal is operational); per-API adoption follows.

## How the seal works (mechanically)

```rust
mod sealed {
    pub trait Sealed {} // private
}

pub trait RuntimeProof: sealed::Sealed {} // public
```

Rust's coherence rules forbid an external crate from implementing a
foreign trait *or* a local trait whose supertraits aren't all visible.
`sealed::Sealed` is unreachable from outside this crate, so:

- A crate consuming `frankenterm_core::RuntimeProof` *cannot* implement
  it for any of its own types.
- Adding a tokio re-export to `runtime_async` does not magically make
  it `RuntimeProof`-compatible — the impl block has to be added by
  hand inside `runtime_proof.rs`, which is the explicit policy choice.

This is the same pattern `serde` uses to keep `Serialize` / `Deserialize`
implementable only in the crates the maintainers control.

## Tests

| Test | Location | Asserts |
|------|----------|---------|
| Per-primitive impl tests | `runtime_proof::tests::*` | Each sealed type compiles when handed to `assert_runtime_proof` |
| `cx_impls_runtime_proof` | `runtime_proof::tests::cx_impls_runtime_proof` | `Cx` is a runtime-proof carrier |
| `generic_api_accepts_sealed_types` | `runtime_proof::tests::generic_api_accepts_sealed_types` | A generic `<P: RuntimeProof>` API accepts both sealed primitives and `Cx` |
| **Canary** | doctest on `assert_runtime_proof` | `tokio::sync::Mutex::new(0)` *fails* to compile when passed to `assert_runtime_proof` |

The canary doctest is what the bridge plan actually demands: a
mechanical demonstration that "PR introduces `tokio::sync::Mutex` in
core" is a hard compile error rather than a soft lint.

## Mechanized Soundness Model

`docs/proofs/runtime-proof-soundness.lean` models the private-supertrait
argument in Lean 4. It proves that a downstream crate cannot implement
`RuntimeProof` because it cannot name `runtime_proof::sealed::Sealed`,
and that any modeled `RuntimeProof` implementation must be in the declared
implementation set. The companion Rust test
`crates/frankenterm-core/tests/runtime_proof_soundness_model.rs` keeps the
Lean list synchronized with the live `runtime_proof.rs` impl list.

## See also

- `docs/reality-check-bridge-plan.md` §G1
- `docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md` — why `runtime_async` is the canonical async surface
- `docs/proofs/runtime-proof-soundness.md` — mechanized proof assumptions and theorem inventory
- `crates/frankenterm-core/src/runtime_proof.rs` — implementation
- `crates/frankenterm-core/src/runtime_async.rs` — wrapper primitives
