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

The same call against a raw `tokio::sync::Mutex` is a compile error. That is
the point of the trait: a foreign primitive cannot serve as the witness at a
`RuntimeProof`-bounded API surface, regardless of whether a lint or source scan
recognizes its import path.

## Why this exists

`AGENTS.md` states *"direct `tokio` usage is forbidden."* Today that
rule is enforced by:

1. Grep guards in `scripts/check_no_runtime_regression.sh`.
2. `cargo deny` `[bans]` against tokio.
3. Code review.

All three are external enforcement layers rather than type-system
constraints. A missed source pattern or newly introduced re-export can evade
them until a later gate runs. The bridge plan
([`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md)
G1) called for a structural type gate at covered async API boundaries. The
sealed-trait pattern makes a non-enumerated outer witness type impossible at
those boundaries; the source and dependency gates remain responsible for the
repository-wide ban.

## What's sealed today

| Type | Origin | Sealed |
|------|--------|--------|
| `runtime_async::Mutex<T>` | local newtype | ✅ |
| `runtime_async::RwLock<T>` | local newtype | ✅ |
| `runtime_async::Semaphore` | local newtype | ✅ |
| `runtime_async::mpsc::{Sender, WeakSender, Receiver, Reserve, SendPermit, Recv, RecvMany}` | local wrappers | ✅ |
| `runtime_async::watch::{Sender, Receiver, ChangedFuture}` | local wrappers | ✅ |
| `runtime_async::broadcast::Sender<T>` / `Receiver<T>` | local wrappers | ✅ |
| `runtime_async::broadcast::Recv<'a, T>` | local wrapper | ✅ |
| `runtime_async::oneshot::Sender<T>` / `Receiver<T>` | local newtype | ✅ |
| `runtime_async::task::JoinHandle<T>` / `JoinSet<T>` | local newtype | ✅ |
| `runtime_async::Runtime` | local newtype | ✅ |
| `cx::Cx` | local | ✅ |
| `runtime_async::notify::Notify` | re-export of `asupersync::sync::Notify` | ❌ (orphan rule) |

`notify::Notify` is still gated transitively when callers thread `&Cx`, since
`Cx: RuntimeProof`. MPSC and watch previously shared that limitation, but their
project-owned wrappers now carry direct seals. Non-waiting foreign error,
telemetry, and borrowed-value types are not asynchronous primitives and do not
retain task wakers, so they remain outside this proof inventory.

### Scope of the proof

`RuntimeProof` is a nominal, non-recursive witness. It proves that the outer
type handed to a bound is one of the types enumerated in `runtime_proof.rs`; it
does not inspect generic payloads. For example, a sanctioned
`runtime_async::Mutex<P>` remains a valid witness regardless of what data type
`P` is. Nor can a trait bound inspect the implementation body of the function
that consumes the witness. The source guards and dependency ban therefore
remain essential companions: they reject forbidden imports and dependencies,
while `RuntimeProof` prevents a raw, non-enumerated primitive from satisfying a
covered API's witness requirement.

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

This is the strictest form and is useful when an API has no natural `&Cx`
parameter. Existing call-sites can pass `&self` of a sealed type (for example,
a runtime_async wrapper they already hold), which costs nothing at runtime.

### Pattern B — thread `&Cx`

```rust
use frankenterm_core::cx::Cx;

pub async fn drain(cx: &Cx) -> Vec<u8> {
    Vec::new()
}
```

Because `Cx: RuntimeProof`, any signature that takes `&Cx` is already
witnessing a runtime-proof. This is the canonical
"structured-async-first" form used throughout the covered public async
surface.

## Adoption sweep status

The `ft-3kv6e` adoption sweep is complete. Its checked-in baseline records
zero uncovered public async sites, and
`scripts/check_runtime_proof_coverage.py` plus
`crates/frankenterm-core/tests/runtime_proof_coverage.rs` form the regression
ratchet. The total site count is expected to change as code evolves; the
release-relevant invariant is that `uncovered_sites` remains zero.

## How the seal works (mechanically)

```rust
mod sealed {
    pub trait Sealed {} // private
}

pub trait RuntimeProof: sealed::Sealed {} // public
```

Although a downstream crate may normally implement a foreign trait for one of
its own local types, it cannot satisfy this trait's required private
supertrait. `sealed::Sealed` is unreachable from outside this crate, so:

- A crate consuming `frankenterm_core::RuntimeProof` *cannot* implement
  it for any of its own types.
- Adding a tokio re-export to `runtime_async` does not magically make
  it `RuntimeProof`-compatible — the impl block has to be added by
  hand inside `runtime_proof.rs`, which is the explicit policy choice.

This is the conventional sealed-trait pattern used when a crate intentionally
keeps a public trait's implementation set closed.

## Tests

| Test | Location | Asserts |
|------|----------|---------|
| Per-primitive impl tests | `runtime_proof::tests::*` | Each sealed type compiles when handed to `assert_runtime_proof` |
| `channel_operation_wrappers_impl_runtime_proof` | `runtime_proof::tests::channel_operation_wrappers_impl_runtime_proof` | MPSC, watch, and broadcast operation wrappers remain in the sealed set |
| `cx_impls_runtime_proof` | `runtime_proof::tests::cx_impls_runtime_proof` | `Cx` is a runtime-proof carrier |
| `generic_api_accepts_sealed_types` | `runtime_proof::tests::generic_api_accepts_sealed_types` | A generic `<P: RuntimeProof>` API accepts both sealed primitives and `Cx` |
| **Canary** | doctest on `assert_runtime_proof` | `tokio::sync::Mutex::new(0)` remains rejected when passed to `assert_runtime_proof` |

The canary mechanically detects if that forbidden call ever starts compiling.
Rustdoc's `compile_fail` mode does not pin a particular diagnostic, so the
canary is not, by itself, proof of *why* the snippet failed. The positive impl
tests, the private-supertrait construction, and the synchronized soundness
model provide the complementary acceptance-set evidence.

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
