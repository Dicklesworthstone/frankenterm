# Triple-Buffered Terminal State

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.3.1] / `ft-d0ol8`
**Parent epic:** [BR-TERM-EMULATOR-UPLIFT-2.3] / `ft-2okh0.3`
**Source-of-truth pattern:** Petersen 2005, "Three-State Mailbox"

**Source status (2026-09-04):** this mailbox is a core substrate, not the
live renderer's snapshot cutover. `LocalPane` still holds the terminal
lock through visible-row callbacks. Production migration and native
performance/equivalence proof belong to
`ft-interactive-systems-performance-4tenz.6.3`.

## Problem

The renderer and input/parser paths share terminal state protected by
a mutex. Correct use of that mutex prevents simultaneous partial-state
reads, but holding it through visible-row work can block other users.
A snapshot migration must preserve coherent text, cursor, selection,
and generation state while shortening this critical section. A screenshot
alone cannot attribute an artifact to concurrent mutation under that lock.

Beyond rendering correctness, the same lock blocks robot-mode reads:
an agent calling `ft robot get-text` competes with the renderer for
the same mutex. Under a 200-pane fleet that's a real contention
point.

## Pattern

Three slots cycle between three roles:

```text
       Writer ──publish──▶ Presented
                              │
                              │ acquire (only when dirty)
                              ▼
                           Reader (held until renderer drops)
```

- The **writer** always has one slot it's mutating.
- The **reader** always has one slot it's reading from.
- The **presented** slot is "the most recent published value the
  reader hasn't seen yet" — empty when the reader has caught up.

Petersen 2005 proves the protocol with a single atomic state byte:
two CAS operations (one per `publish`, one per `acquire`) suffice
to maintain the invariant that the three slot indices are always a
permutation of `{0, 1, 2}`.

## Implementation

`crates/frankenterm-core/src/triple_buffer.rs`:

- `TripleBuffer<T: Send + Sync + 'static>` — three `Arc<T>` slots in
  a `[Mutex<Arc<T>>; 3]`, plus an `AtomicU8` packing the four state
  values (writer slot, presented slot, reader slot, dirty flag).
- `publish(T)` — acquires a slot mutex and retries a state CAS; if the previous publish was unread,
  increments `writer_overruns_total` and overwrites the *oldest
  unread* slot. The reader's currently-held slot is untouched.
- `acquire() -> Arc<T>` — returns a stable `Arc<T>` the renderer
  holds for the duration of the frame. Acquires a slot mutex and may
  retry a state CAS; it is not a wait-free read.
- `force_recycle()` — watchdog hook. Forces the reader-slot index
  to recycle to the most recently presented slot, regardless of
  the dirty flag. Counts `force_recycles_total`.
- `health() -> TripleBufferHealth` — counter snapshot for `ft
  doctor`. Mirrors the `AtlasStabilityHealth` shape from
  `ft-mpc9b.1.1` so the future doctor surface can render both
  side-by-side.

The implementation is **safe Rust** (`forbid(unsafe_code)` is in
core's `lib.rs`). The `Mutex<Arc<T>>` protects an `Arc::clone` or
slot replacement; replacement can also drop the previous payload.
It can block, and neither its hold time nor payload disposal cost is
bounded by a nanosecond claim. The returned Arc is held outside that
lock. Measure the complete publication/acquisition path before making
native latency claims.
Payloads used as coherent snapshots must also be immutable: `Arc<T>` and
`Send + Sync` alone do not prohibit interior mutation by other owners.

## Failure modes

### Writer outpaces reader

`writer_overruns_total` counts every publish that overwrote an
unread slot. A held Arc remains stable, but the mailbox does not impose
a wall-clock or one-frame freshness bound. Freshness and scheduling
require separate integration constraints.

The fixture's `writer_outpaces_reader_thousand_to_one_no_panic`
test pins this: a writer publishing 1000 values while the reader
sleeps 5ms before acquiring once. No panic, slot indices stay
distinct, counter is well-formed.

### Reader holds slot too long

A reader can retain an Arc beyond the intended frame. `force_recycle()`
rotates the reader-slot index;
the renderer's previously-held `Arc<T>` is harmless afterwards
(it continues to exist as long as the renderer holds it). Recycling an
index cannot reclaim a retained Arc or recover a crashed thread.

The watchdog wiring (the 5-second timeout the parent epic specifies)
is the integration bead's job; this module exposes the API.
`force_recycles_total > 0` is the doctor's alert condition for
"watchdog had to recover a stuck reader" — each occurrence is a
fatal-recoverable event.

## Invariants

The fixture in `crates/frankenterm-core/tests/triple_buffer_fixture.rs`
asserts:

1. **Slot distinctness.** `(writer, presented, reader)` is always a
   permutation of `(0, 1, 2)`. Proven over 256 random op sequences
   via proptest (`slot_distinctness_holds_under_arbitrary_op_sequences`).
2. **Snapshot stability under publication.** With immutable payloads,
   a renderer holding an `Arc<T>` from `acquire` sees the same value
   while the writer floods publishes (`held_snapshot_is_immutable`).
   This fixture does not prove arbitrary interior-mutable `T` is immutable.
3. **Counter monotonicity.** Counters never decrease across an op,
   and `overruns_total <= publishes_total` at all times
   (`counters_monotonic`).
4. **Concurrent safety.** A 5,000-publish-vs-5,000-acquire stress
   test on real OS threads produces consistent slot states at every
   observation point (`concurrent_writer_and_reader_stress`).
5. **JSONL serde stability.** Event streams round-trip through
   `serde_json` identity (`jsonl_roundtrip`).

## Out of scope (follow-on beads)

- **Render thread integration** (`ft-2okh0.3.2`): migrate
  `paint_pass` to read snapshots via `TripleBuffer::acquire`
  instead of borrowing the live mutex.
- **Persistent rope composition** (`ft-2okh0.3.3`): compose with
  `BR-TERM-EMULATOR-UPLIFT.2.5` so the three slots share rope
  structure; memory overhead drops from 3× to ~1.1×.
- **Loom proof** (`ft-2okh0.3.4`): full state-space exploration of
  the swap protocol per `BR-RC-FOUNDATION.G8.2`. The fixture's
  proptest sweep is the always-on regression net; Loom is the
  formal correctness proof.
- **`ft doctor` wiring**: surface `TripleBufferHealth` alongside
  `AtlasStabilityHealth` once the GUI integration bead lands.

## Cross-references

- Atlas-stability foundation: [`atlas-stability.md`](#) (parallel
  ghostty-pattern ship in `ft-mpc9b.1.1`).
- Memory overhead mitigation: persistent-rope (`ft-mpc9b.2.5`).
- Loom cross-link: `BR-RC-FOUNDATION.G8.2`.
- Per-release attestation entry: this bead contributes the
  `triple_buffer_health` field to the per-release attestation
  schema (cross-link `BR-RC-FOUNDATION.G3.1`).
