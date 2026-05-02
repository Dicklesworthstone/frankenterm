# Proof techniques for FrankenTerm

When to reach for which formal-method or property-test tool. The
techniques below are NOT mainstream Rust skills, so this playbook is
the canonical entry point. Each section names the tool, the kind of
question it answers, and a concrete example bead in this repo.

Index from `AGENTS.md` § Testing.

---

## Loom — concurrency primitive proofs

**Question**: "Does this lock-free / atomic-coordination data structure
have the linearization / no-lost-write / no-deadlock property under
*all* possible thread interleavings?"

**When to use**:
- New atomic-state-machine substrate (atomic counter compositions, CAS
  loops, triple-buffer-style swap).
- Multi-thread coordination primitives (broadcast, oneshot, mpsc,
  spsc, watch).
- Anything that mutates state across `Arc<...>` boundaries with
  custom Acquire/Release/AcqRel orderings.

**When NOT to use**:
- Single-threaded data structures.
- Logic that relies on `Mutex` for serialization (Loom's value-add is
  in lock-free / weak-ordering proofs).

**Existing examples in this repo**:
- `crates/frankenterm-core/tests/loom_triple_buffer.rs` — proves the
  triple-buffer's 1R/2W lost-write properties under all interleavings.
  Pairs with the user-facing single-writer guarantee in
  `triple_buffer.rs` docstrings.
- `crates/frankenterm-core/tests/loom_mpsc.rs` — mpsc channel
  ordering + drop-safety.
- `crates/frankenterm-core/tests/loom_spsc_ring_buffer.rs` —
  SPSC ring without ABA / torn-read.
- `crates/frankenterm-core/tests/loom_broadcast.rs`,
  `loom_oneshot.rs`, `loom_sync.rs`, `loom_watch.rs`,
  `loom_lockfree.rs`, `loom_notify.rs`.

**Pattern**:
```rust
#[cfg(loom)]
use loom::sync::Arc;
#[cfg(not(loom))]
use std::sync::Arc;

#[test]
fn property_holds() {
    loom::model(|| {
        // 2-3 threads, exercise the ordering you care about
    });
}
```

Run with `RUSTFLAGS="--cfg loom" cargo test --release --test loom_*`.
Loom's interleaving search is exponential; keep models small (≤3
threads, ≤2 iterations per thread) and the model time bounded.

---

## TLA+ — state-machine safety + liveness, distributed protocols

**Question**: "Across all reachable states of this distributed
protocol, does the safety invariant `INV` hold? And does the system
make liveness progress under fairness assumptions?"

**When to use**:
- Distributed audit-replication or commit protocols (e.g., Reed-
  Solomon erasure encoding, multi-aggregator quorum).
- Multi-pane mux protocols where state is replicated across hosts.
- Liveness questions ("does the system eventually drain its queue
  under fair scheduling?").

**When NOT to use**:
- In-process algorithms (use Loom or Stateright instead).
- Single-host concurrency (TLA+'s overhead doesn't pay off).

**Existing examples in this repo**:
- TLA+ models live in `docs/tla/` (when shipped).

**Pattern**:
1. Sketch the protocol in TLA+ (state variables, init, next).
2. Define safety invariant: `Inv == ...`.
3. Run TLC (TLA+ Toolbox or `tla2tools.jar`) with bounded scope.
4. If safety holds, derive a model-state diff to validate against the
   Rust impl (manually or via Stateright).

---

## Stateright — in-Rust model checking with the real impl

**Question**: "Does my Rust impl satisfy the safety invariant under
**all** orderings, **without** a separate TLA+ spec to maintain?"

**When to use**:
- The protocol is small enough for exhaustive model checking
  (reachable state space < 10⁷ states, typical limit before BFS
  exhausts memory).
- You want the proof to track the Rust implementation byte-for-byte
  (no spec drift).
- You want both safety + linearizability properties.

**When NOT to use**:
- Large state spaces (use TLA+ + abstraction instead).
- You haven't written the Rust impl yet (sketch in TLA+ first).

**Existing examples in this repo**:
- `crates/frankenterm-core/tests/proptest_formal_mux_protocol.rs` —
  formal mux protocol coverage.
- `crates/frankenterm-core/tests/wire_dedup_model.rs` — wire dedup
  model checks.
- `crates/frankenterm-core/tests/robot_family_conformance.rs` —
  robot-contract family invariants.

**Pattern**: Implement `stateright::Model` for your state machine,
declare the invariant via `properties()`, and run
`Checker::default().check(&model).assert_properties()`.

---

## proptest — data-shape invariants, serde roundtrip, fuzz-sized inputs

**Question**: "Does this pure function / data-structure invariant hold
for **arbitrary** inputs from the input domain?"

**When to use**:
- Serde roundtrips: `parse(serialize(x)) == x` for any `x`.
- Pure-function invariants: `f(g(x)) == x` for inverse pairs.
- Decision-function exhaustive coverage where the input is too large
  for a hand-written test matrix (e.g., budget-clamping logic).
- Numeric stability: `f(x) ≥ 0` for all finite `x`.

**When NOT to use**:
- Stateful concurrency (use Loom / Stateright).
- Distributed protocols (use TLA+).

**Existing examples in this repo**: see any
`crates/frankenterm-core/tests/proptest_*.rs` — there are 100+ such
files. The naming convention is `proptest_<module>.rs`.

**Pattern**:
```rust
proptest! {
    #[test]
    fn config_repair_is_idempotent(cfg in any::<DecodeBudget>()) {
        let once = cfg.with_repaired_invariants();
        let twice = once.with_repaired_invariants();
        prop_assert_eq!(once, twice);
    }
}
```

**Watch out**:
- `prop_assert!(matches!(x, Foo { .. }))` — the `{ .. }` confuses the
  format-string parser. Use `let check = matches!(x, Foo { .. });
  prop_assert!(check);` instead. (Pattern from session memory.)

---

## dylint custom lints — project-specific structural checks

**Question**: "Does this codebase satisfy a structural rule that
`clippy` doesn't enforce?"

**When to use**:
- Project-specific call-graph constraints (e.g., "no render-thread
  function reaches a `Mutation` snapshot guard" — see
  `render_call_graph_audit.rs`).
- Forbid-pattern enforcement that's too codebase-specific to upstream
  to clippy.
- Cx-propagation-burndown: "every `runtime_async::*` async function
  takes a `cx` parameter".

**When NOT to use**:
- One-shot CI checks (use a `cargo test` integration test instead).
- Patterns better expressed as type constraints (use the type system).

**Existing examples in this repo**:
- `crates/frankenterm-core/src/render_call_graph_audit.rs` +
  `render_call_graph_populator.rs` (regex-based; the bead allowed
  "custom dylint plugin or grep+regex harness").
- `BR-RC-RUNTIME-SEMANTICS.G14.1.cont.dylint` — full LateLintPass
  migration tracked at `ft-rca2p`.

**Pattern**:
1. Sketch the rule as a code-walking predicate (visitor over HIR or
   token stream).
2. Decide: regex-pass + integration test, or full dylint plugin?
   Start with regex-pass; promote to dylint when (a) you need
   semantic analysis or (b) the rule lands on PR CI.
3. If dylint: see `cargo-dylint` book; register as a workspace
   member.

---

## cargo-deny — dependency-graph constraints

**Question**: "Does the workspace's dependency graph satisfy our
license, advisory, ban, and source constraints?"

**When to use**:
- License compliance enforcement (allow-list of OSS licenses).
- CVE / RUSTSEC advisory tripwires (block builds on known-bad
  versions).
- Crate ban-list (avoid e.g. `unicode-ident` < 1.x for known
  perf bugs).
- Multiple-version detection (no two versions of the same crate
  without explicit allow).

**When NOT to use**:
- Code-quality lints (use clippy / dylint).
- Behavioral checks (use proptest / integration tests).

**Pattern**:
1. `deny.toml` at workspace root with `[licenses]`, `[advisories]`,
   `[bans]`, `[sources]` sections.
2. `cargo deny check` in PR CI.
3. Treat advisory hits as P1 release blockers; treat
   bans/licenses/sources as P2 in non-release work.

---

## Picking the right tool

| Question                                               | Tool         |
|--------------------------------------------------------|--------------|
| Atomic ordering / lock-free correctness?               | Loom         |
| Distributed-protocol safety + liveness?                | TLA+         |
| In-Rust model check with real impl?                    | Stateright   |
| Pure-function invariant under arbitrary input?         | proptest     |
| Project-specific structural rule?                      | dylint       |
| Dependency-graph constraint?                           | cargo-deny   |

When in doubt: start with `proptest` (cheapest); promote to Loom
when concurrency enters the picture; promote to Stateright when the
state space is small enough for exhaustive checking; reach for TLA+
when the problem is genuinely distributed.
