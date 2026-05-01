# Mazurkiewicz Traces — runtime_async Primitive Equivalence Classes

**Bead:** ft-syqcz.7 (BR-RC-FOUNDATION.G8.2) · **Companion:** `tests/loom_<primitive>.rs`

This document is the catalog of *observable equivalence classes* for
each primitive in `runtime_async`. A Mazurkiewicz trace is a set of
schedule interleavings that the model checker treats as equivalent
because they all produce the same observable outcome under the
linearization point we picked. The Loom proofs in
`crates/frankenterm-core/tests/loom_<primitive>.rs` enumerate the
*behaviors* a primitive exhibits; this document enumerates the
*reasons* those behaviors are the only ones a correct implementation
can produce.

The structure for each section is:
1. **Operations under study** — the verbs the primitive exposes.
2. **Linearization points** — where each operation is "logically"
   ordered with respect to other operations.
3. **Equivalence classes** — sets of interleavings that produce the
   same final state. Each class names the partial order over
   operations that defines it.
4. **Distinguishable outcomes** — the points at which two interleavings
   produce *different* observable states. These are the points where
   Loom's enumeration matters.
5. **Anti-patterns** — schedules that would be a *bug*: the proof
   suite must not produce them, and a future implementation change
   must keep this property.

---

## oneshot

**Bead:** ft-zzw3s · **File:** `crates/frankenterm-core/tests/loom_oneshot.rs`

### Operations under study

| Op | Side | Effect |
| --- | --- | --- |
| `send(v)` | sender | If channel is fresh, transitions to `Some(v) ; sent=true`. Else returns `Err(v)`. |
| `close_sender` | sender | Transitions to `sender_dropped=true`. |
| `close_receiver` | receiver | Sets the unified "channel closed" flag from the sender's perspective. |
| `recv` | receiver | Blocks until `sent || sender_dropped`. Returns `state.value.take()`. |
| `try_recv_terminal` | receiver | Non-blocking; assumes the channel is already in a terminal state. |

### Linearization points

- `send(v)` linearizes at the moment its mutex critical section sets
  `sent = true` (from the receiver's perspective: when the load that
  observes `sent=true` becomes visible).
- `close_sender` / `close_receiver` linearize at the moment their
  mutex critical section sets `sender_dropped = true`.
- `recv` linearizes at the moment its `value.take()` runs (after the
  predicate `sent || sender_dropped` succeeds).

### Equivalence classes

Two schedules are observably equivalent iff their projection onto
`(sent, value, sender_dropped)` is identical at the receiver's
linearization point. The classes are:

1. **Single-deliver class.** Any schedule where `send(v)` linearizes
   before *any* `close_*` and before `recv` produces final state
   `(true, None, false)` after recv runs (with recv returning
   `Some(v)`). All such schedules are equivalent regardless of the
   physical order of `send` versus `close_*` after recv has run —
   the receiver has already taken the value.
2. **Close-then-noisy-send class.** `close_sender` linearizes before
   `send(v)`. The send observes `sender_dropped=true` and returns
   `Err(v)`; recv returns `None`. The schedules are equivalent
   because the value never enters the channel.
3. **Concurrent send/close race.** Both operations contend for the
   first lock acquisition; whichever wins linearizes first. The two
   sub-classes are equivalent to (1) and (2) above respectively. The
   key property: *exactly one* sub-class is selected per execution.
4. **Idempotent-recv class.** Once recv has linearized, all
   subsequent `try_recv_terminal` calls on the same receiver are
   equivalent (return `None`).

### Distinguishable outcomes

The visible state space at the receiver's linearization point is
exactly:

- `Some(v)` — the single-deliver class fired.
- `None` — either close-then-noisy-send fired, or the receiver was
  closed before the sender's send linearized.

These two outcomes are observably distinct and Loom's enumeration
must explore both. The proofs at lines 67, 88, 119, 152, 184, and
217 of `loom_oneshot.rs` collectively cover both halves of the
distinction.

### Anti-patterns (must-not-occur schedules)

- **Double-delivery**: a schedule where two separate `recv` calls on
  the same receiver both return `Some(v)`. Forbidden by
  `state.value.take()` consuming the option in a single critical
  section. The `loom_oneshot_recv_idempotent_after_terminal` proof
  rejects this.
- **Lost-update**: a schedule where `send(v)` runs but recv returns
  `None`. Forbidden by `delivered.notify_all()` after the
  `state.value = Some(v); state.sent = true;` write — the receiver
  cannot wake without observing the consequence.
- **Sender panic on receiver drop**: a schedule that panics the
  sender's task because the receiver dropped first. Forbidden by
  `try_send_or_drop` returning `Err(v)` instead of asserting state
  invariants. The `loom_oneshot_send_during_receiver_drop_no_panic`
  proof rejects this — Loom panics propagate as test failures.

### Cross-references

- `runtime_async::oneshot` (sealed via `runtime_proof::sealed::Sealed`
  for `Sender<T>` and `Receiver<T>`).
- `tests/loom_oneshot.rs` — the proofs themselves.

---

## Notify

**Bead:** ft-kpmej · **File:** `crates/frankenterm-core/tests/loom_notify.rs`

### Operations under study

| Op | Side | Effect |
| --- | --- | --- |
| `notify_one` | producer | If a waiter is parked, wakes one. Else accumulates a permit (capped at 1 — multiple consecutive `notify_one` calls do not stack). |
| `notify_waiters` | producer | Wakes every currently-parked waiter by bumping the epoch counter and broadcasting on the condvar. Does *not* accumulate a permit for future waiters. |
| `wait` | consumer | Returns immediately if a permit is available (consuming it). Else parks on the condvar until either a permit appears or the epoch advances. |

### Linearization points

- `notify_one` linearizes at the moment its critical section runs
  `permits = ... .min(1)`. From the waiter's view, this is the load
  that observes a non-zero permit count.
- `notify_waiters` linearizes at the moment its critical section
  bumps `epoch += 1`. From the waiter's view, this is the load that
  observes `state.epoch != baseline_epoch`.
- `wait` has *two* linearization points depending on which branch
  of its loop fires:
  1. `permits > 0` branch: linearizes at the
     `permits -= 1` decrement.
  2. `epoch != baseline_epoch` branch: linearizes at the load that
     observes the bumped epoch.

### Equivalence classes

1. **Permit-pre-accumulation class.** `notify_one` linearizes before
   `wait` starts. The waiter observes `permits > 0` on its very
   first iteration and never parks. All schedules where the permit
   is delivered before the waiter samples are equivalent.
2. **Park-then-wake class.** `wait` parks on cv before any
   producer call. The first subsequent `notify_one` or
   `notify_waiters` wakes it. Schedules differ in which producer op
   races but the observable outcome (waiter completes exactly once)
   is shared.
3. **Concurrent notify_one + wait race.** Two sub-classes:
   - producer wins → permit-pre-accumulation behavior;
   - waiter wins → park-then-wake behavior.
   Both are linearizable; the model checker explores both.
4. **Permit-cap-saturation class.** Multiple `notify_one` calls
   with no waiter present. All such schedules are equivalent
   regardless of how many calls fired (3 or 30) — final state is
   `permits == 1`.
5. **notify_waiters non-accumulation class.** `notify_waiters` with
   no waiters parked. All such schedules are equivalent: the epoch
   bump is invisible to a future waiter (which samples its own
   baseline at park time), and no permit accumulates. A future
   waiter must observe a separate `notify_one` or `notify_waiters`
   to complete.

### Distinguishable outcomes

The visible state space is `(permits ∈ {0,1}, epoch_progressed ∈
{true,false}, waiter_completed ∈ {true,false})`. Loom's enumeration
must reach every combination that any equivalence class above
permits, and reject any combination outside them — particularly:

- `permits == 2` (cap violation)
- `waiter_completed == true` without any producer op having
  linearized first (lost-wake bug)

### Anti-patterns (must-not-occur schedules)

- **Permit-stack overflow.** A schedule where `permits > 1` is
  observable. Forbidden by `saturating_add(1).min(1)` in
  `notify_one`. The `loom_notify_one_permit_caps_at_one` proof
  rejects this — if the cap were 3, the second `wait` in that test
  would not require a fresh `notify_one` and the proof would
  observe `woken.load() == 2` even with the notifier thread blocked.
- **notify_waiters permit-leak.** A schedule where a waiter that
  parks *after* a stand-alone `notify_waiters` returns without a
  subsequent producer op. Forbidden by the model: `notify_waiters`
  bumps `epoch` but does not increment `permits`, and a waiter that
  parks after the bump samples `epoch` as its own baseline. The
  `loom_notify_waiters_does_not_accumulate` proof rejects this.
- **Spurious double-wake.** A schedule where a `notify_one` issued
  after a previous waiter completed somehow re-wakes the completed
  waiter. Forbidden by the linearization-point structure: a
  completed waiter's thread has joined and cannot observe further
  notifications. The `loom_notify_one_post_wait_accumulates_for_next`
  proof structures the test so that any spurious second wake of the
  first waiter would manifest as `woken_first.load() == 2`, which
  the proof asserts is `== 1`.

### Cross-references

- `runtime_async::notify::Notify` re-exported from
  `asupersync::sync::Notify`.
- `tests/loom_notify.rs` — the proofs themselves.

---

## broadcast

**Bead:** ft-bpfb7 · **File:** `crates/frankenterm-core/tests/loom_broadcast.rs`

### Operations under study

| Op | Side | Effect |
| --- | --- | --- |
| `send(v)` | producer | Appends `(seq, v)` to the log under the bus mutex; `seq = log.len()` at append time. Notifies all waiters. |
| `close` | producer | Sets `closed = true` under the bus mutex. Notifies all waiters. |
| `recv_at(c)` | consumer | Loops: returns `Some((seq, v))` if an entry with `seq == c` exists; else returns `None` if `closed`; else parks on the condvar. |

### Linearization points

- `send(v)` linearizes at the moment its critical section runs
  `state.log.push((seq, value))`. From a receiver's view, this is
  the load that observes the new entry's seq.
- `close` linearizes at the moment its critical section runs
  `state.closed = true`.
- `recv_at(c)` linearizes at the moment its loop iteration finds
  the matching entry (returning `Some`) or observes `closed`
  (returning `None`).

### Equivalence classes

1. **Single-producer total-order class.** All sends issued by a
   single producer thread are observed by every receiver in the
   same `(0, v0), (1, v1), …` order. Schedules differ only in
   when each receiver wakes; the projection onto seq numbers is
   identical.
2. **Multi-producer serialized class.** Concurrent sends contend
   for the bus mutex. The total order observed by every receiver
   reflects whichever producer won the mutex at each step. Two
   sub-classes correspond to the two possible mutex acquisition
   orders for two producers — both produce the same multiset of
   delivered values, just attached to different seq numbers. All
   schedules within a sub-class are equivalent.
3. **Close-before-send class.** `close` linearizes before any
   `send`. Every receiver on cursor 0 observes `None`; the bus
   has no buffered data.
4. **Send-then-close class.** Producers `send(v0), send(v1), …,
   close()`. Receivers consume `Some((0, v0)), Some((1, v1)), …`
   in order, then observe `None` on cursor `len(log)`. Schedules
   that interleave send + close with active receivers are
   equivalent within this class as long as the sender-side total
   order is preserved.
5. **Late-subscriber class.** A receiver that starts after some
   sends still observes the entire historical log (in this
   unbounded model). Schedules that differ in *when* the late
   receiver subscribes — but not in the bus state at subscription
   time — are equivalent.

### Distinguishable outcomes

The visible state space at each receiver's linearization point is
`(highest_seq_observed, observed_close)`. Loom must enumerate every
schedule that produces every reachable combination, and the proofs
must reject any combination that violates total-order or
close-after-drain.

### Anti-patterns (must-not-occur schedules)

- **Total-order divergence.** A schedule where two receivers
  observe different `(seq -> value)` mappings for the same seq
  (e.g. receiver A sees `(0, 10)` while receiver B sees
  `(0, 20)`). Forbidden by the bus mutex serializing all writes.
  The skeleton `loom_broadcast_preserves_total_order_across_receivers`
  proof rejects this.
- **Close-erases-data.** A schedule where `close` retroactively
  hides previously-buffered messages from a still-pending recv.
  Forbidden by the recv loop's structure: the `state.log.iter()
  .find(...)` runs before the `state.closed` check, so a buffered
  entry always wins. The
  `loom_broadcast_close_after_sends_drains_then_closes` proof
  rejects this.
- **Lost wake on close.** A receiver parks on the condvar; `close`
  fires; receiver never wakes (deadlock). Forbidden by `close`
  doing `not_empty.notify_all()` after the state mutation. The
  `loom_broadcast_close_before_send_visible_to_all` proof rejects
  this — both receivers must terminate.
- **Early drop on late-subscribe.** A receiver that subscribes
  after `send(v)` observes `None` on cursor 0 because the entry
  was "missed". Forbidden by the unbounded log: every entry
  remains until the bus is dropped. The
  `loom_broadcast_late_receiver_sees_historical_log` proof rejects
  this. (When the bounded-ring extension lands, this property
  weakens to "entries below the lag-tolerance window remain"; the
  invariant becomes overwrite-aware.)

### Cross-references

- `runtime_async::broadcast` re-exports the asupersync broadcast
  primitives (Sender, Receiver, RecvError, etc.).
- `tests/loom_broadcast.rs` — the proofs themselves.
- Lagging-receiver / bounded-ring overwrite semantics are filed as a
  follow-on under the umbrella docs bead ft-jnaa0.

---

## Pending sections

The following primitives' Mazurkiewicz sections are filed as separate
sub-beads of ft-syqcz.7. Each one extends this document with a section
following the structure above:

- ft-e2usk — Mutex
- ft-5omg9 — RwLock
- ft-5fbkx — Semaphore
- ft-ue7sr — mpsc
- ft-r51h4 — watch

The umbrella tracker for the docs itself is **ft-jnaa0**. Each
per-primitive bead claims the corresponding `loom_<name>.rs` exhaustive
proofs *and* the matching section here, so the proof corpus and the
trace documentation stay in lockstep.
