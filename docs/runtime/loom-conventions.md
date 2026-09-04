# Loom Conventions for `runtime_async`

**Bead:** `ft-syqcz.6` (skeleton harness — this doc + the `tests/loom_*.rs` files)
**Next bead:** `ft-syqcz.7` (G8.2 — exhaustive Loom proofs + Mazurkiewicz-trace doc)
**Dev-dep declared:** `crates/frankenterm-core/Cargo.toml` line ~150 (`loom = "0.7"`)

This doc captures how to write, run, and add Loom model-checking tests
for the primitives exposed by `runtime_async`.

## Why Loom

`runtime_async` is the load-bearing concurrency surface of the project
(115+ exports; 1 622 import sites at last count — see
`docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md`).
Property tests are good for serde and value-level invariants but weak
at exhaustive interleaving exploration. Loom complements them by
explicitly enumerating the schedules that matter for memory ordering
and permit-count invariants.

## What we model, and what we don't

Loom **cannot instrument tokio or asupersync internals**. Loom-controlled
schedules need every concurrency primitive (mutex, condvar, atomic,
thread spawn) to come from `loom::*`. Reaching into asupersync from a
Loom test would simply run the asupersync code with the OS scheduler —
no model checking happens.

We therefore model the **contracts** that the `runtime_async` surface
must preserve, using loom-native primitives. The skeleton tests live at:

| Primitive | Skeleton file |
|---|---|
| `Mutex` | `crates/frankenterm-core/tests/loom_sync.rs::loom_mutex_preserves_mutual_exclusion` |
| `RwLock` | `crates/frankenterm-core/tests/loom_sync.rs::loom_rwlock_preserves_reader_writer_invariant` |
| `Semaphore` | `crates/frankenterm-core/tests/loom_sync.rs::loom_semaphore_*` |
| `mpsc` | `crates/frankenterm-core/tests/loom_mpsc.rs` |
| `watch` | `crates/frankenterm-core/tests/loom_watch.rs` |
| `broadcast` | `crates/frankenterm-core/tests/loom_broadcast.rs` |
| `oneshot` | `crates/frankenterm-core/tests/loom_oneshot.rs` |
| `Notify` | `crates/frankenterm-core/tests/loom_notify.rs` |

Plus the existing lock-free / SPSC ring tests:

| Subsystem | File |
|---|---|
| Lock-free counters | `crates/frankenterm-core/tests/loom_lockfree.rs` |
| SPSC ring buffer | `crates/frankenterm-core/tests/loom_spsc_ring_buffer.rs` |

**Contract model vs production proof:** the original `ft-syqcz.6` skeletons
have been expanded. For example, `loom_mpsc.rs` directly uses Loom primitives
and `loom::model` to enumerate model schedules. This explores the encoded
contract within its configured bounds; it does not instrument the production
asupersync implementation or establish every production cancellation behavior.

## Running Loom tests through RCH

The contract tests directly call `loom::model`; they do not require
`--cfg loom` to switch from ordinary primitives into schedule exploration.
Use the test's actual model bounds and retain the executed test count.

```bash
# Run the mpsc contract models.
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  cargo test -p frankenterm-core --test loom_mpsc --locked

# Run all matching Loom test targets; retain each executed target/result.
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  cargo test -p frankenterm-core --test 'loom_*' --locked
```

No cfg flag can turn a contract model into proof of an uninstrumented
production primitive.

### Tuning the explorer

Loom respects two environment knobs that matter for our suite:

- `LOOM_MAX_BRANCHES` — default 1000; raise for richer skeletons.
- `LOOM_MAX_DURATION_SECS` — default unlimited; cap if you want to fail
  fast.
- `LOOM_LOG=1` — verbose schedule log; useful when reading a failure.
- `LOOM_CHECKPOINT_FILE=…/checkpoint` — saves the failed schedule for
  reproduction; rerun with the same env var to replay.

A bounded remote invocation:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env LOOM_LOG=1 LOOM_MAX_BRANCHES=4000 \
  cargo test -p frankenterm-core --test loom_mpsc --locked -- --nocapture
```

## DSR and RCH proof lanes

The former scheduled wiring under `ft-93mra` is historical. Current model
execution requires remote RCH admission; DSR exclusively owns release
orchestration and retained release evidence. No nightly scheduler is asserted
here. Set explicit model budgets and retain timeout or incomplete-exploration
results as such.

- `scripts/check_loom_skeleton_coverage.sh` is the seal-on-add gate.
  It maintains a manifest of `(primitive, skeleton file)` pairs and
  fails CI if a new `pub struct`/`pub mod` primitive lands in
  `runtime_async.rs` without an accompanying skeleton row. The script
  belongs in the configured DSR static quality gates. Verify its actual
  entry in `scripts/release-gates.sh` before claiming it ran. Update the
  manifest in the same commit that adds (or removes / renames) a
  primitive.

The exhaustive proof additions (and the Mazurkiewicz-trace doc) ride on
this infrastructure under `ft-syqcz.7`.

## How to add a new primitive

1. Pick a skeleton file under `crates/frankenterm-core/tests/loom_*.rs`
   that's closest in shape to your primitive.
2. Copy it to `tests/loom_<primitive>.rs`.
3. Replace the contract model with one that mirrors your primitive's
   invariants. Use loom-native primitives only:
   - `loom::sync::{Arc, Mutex, RwLock, Condvar}` (NOT `std::sync`)
   - `loom::sync::atomic::*` (NOT `std::sync::atomic`)
   - `loom::thread::spawn` / `loom::thread::yield_now`
4. Wrap the test body in `loom::model(|| { … })`.
5. Keep the assertions tight — every assertion is a separate
   model-checked invariant.
6. If the primitive grows new methods, extend the skeleton or open a
   companion test file so each invariant gets its own focused model.

## Failure-triage tips

- A Loom failure is **deterministic for a given schedule**. Capture
  `LOOM_CHECKPOINT_FILE` from the failing run, then replay with the
  same env var; the explorer rewinds to the offending interleaving.
- Loom enforces a finite branch budget. A test that fails with
  `model exceeded LOOM_MAX_BRANCHES` is over-budget, not buggy — split
  it into smaller invariants or raise the budget.
- `loom::sync::Mutex` panics on poison just like `std::sync::Mutex`;
  do not propagate `unwrap()` errors as model assertions.
- Loom does not model `tokio::task::yield_now` or asupersync futures.
  If your primitive needs an async path, model it synchronously with
  `loom::thread::spawn` calls that mimic the future's await points.

## Cross-references

- `crates/frankenterm-core/src/runtime_async.rs` — the surface under
  test
- `docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md` — why
  the wrapper exists and is staying
- `loom` crate docs — <https://docs.rs/loom> (mirrored upstream)
- `ft-syqcz.7` — the exhaustive proof bead this skeleton enables
