// =============================================================================
// Metamorphic / algebraic contract tests for the event-bus causality clock
// (events.rs: VectorClock, EventCausalityClock, HLC + Lamport stamps).
//
// The four inline tests in events.rs are single-scenario examples
// (one happens-before, one merge, two bus snapshots). They do NOT pin the
// *algebraic* invariants the implementation relies on for correctness:
//
//   VectorClock::merge  -> idempotent, pointwise-max (monotone), commutative
//   VectorClock::relation_to -> reflexive(Equal), antisymmetric(Before<->After)
//   EventCausalityClock  -> HLC (wall,logical) is lexicographically monotone
//                           across ANY interleaving of local + receive events
//   observe_remote       -> Lamport receive rule (strictly dominates both),
//                           HLC causal dominance, vector frontier dominance
//
// These are the canonical Lamport / vector-clock / Kulkarni-HLC invariants.
// All ops are synchronous, so this needs no runtime/feature gate and proves
// under `cargo test -p frankenterm-core`.
// =============================================================================

use frankenterm_core::events::{
    CausalRelation, EventCausalityClock, EventCausalityStamp, VectorClock,
};

/// HLC stamps order lexicographically: wall time first, logical as tie-breaker.
fn hlc_key(stamp: &EventCausalityStamp) -> (u64, u64) {
    (stamp.hybrid.wall_time_ms, stamp.hybrid.logical)
}

// --- VectorClock algebra ---------------------------------------------------

/// `merge` is idempotent: merging the same clock a second time changes nothing.
#[test]
fn vector_clock_merge_is_idempotent() {
    let mut a = VectorClock::new();
    a.increment("n1");
    a.increment("n1");
    a.increment("n2");

    let mut b = VectorClock::new();
    b.increment("n2");
    b.increment("n3");

    a.merge(&b);
    let after_first = a.clone();
    a.merge(&b);
    assert_eq!(
        a, after_first,
        "merging the same clock twice must equal merging it once (idempotent)"
    );
}

/// `merge` is pointwise-max: every entry is >= both inputs and never decreases.
#[test]
fn vector_clock_merge_is_pointwise_max_and_monotone() {
    let mut a = VectorClock::new();
    a.increment("n1"); // n1=1
    a.increment("n1"); // n1=2
    a.increment("n2"); // n2=1

    let mut b = VectorClock::new();
    b.increment("n1"); // n1=1  (lower than a's n1=2)
    b.increment("n3"); // n3=1  (absent from a)

    let a_before = a.clone();
    a.merge(&b);

    // Pointwise maximum over the union of node ids.
    assert_eq!(a.get("n1"), 2, "n1 = max(2, 1) = 2");
    assert_eq!(a.get("n2"), 1, "n2 carried over from receiver");
    assert_eq!(a.get("n3"), 1, "n3 absorbed from the other clock");
    // Monotone: no entry ever shrinks under merge.
    for node in ["n1", "n2", "n3"] {
        assert!(
            a.get(node) >= a_before.get(node),
            "merge must never decrease entry {node}: {} < {}",
            a.get(node),
            a_before.get(node)
        );
    }
}

/// `merge` is commutative in its effect on the frontier: a<-b and b<-a converge
/// to the same set of per-node maxima.
#[test]
fn vector_clock_merge_is_commutative() {
    let mut a = VectorClock::new();
    a.increment("n1");
    a.increment("n2");
    a.increment("n2");

    let mut b = VectorClock::new();
    b.increment("n2");
    b.increment("n3");

    let mut ab = a.clone();
    ab.merge(&b);
    let mut ba = b.clone();
    ba.merge(&a);

    assert_eq!(
        ab, ba,
        "merge must be commutative on the resulting frontier"
    );
}

/// `relation_to` is reflexive: a clock is Equal to itself.
#[test]
fn vector_clock_relation_is_reflexive() {
    let mut a = VectorClock::new();
    a.increment("n1");
    a.increment("n2");
    assert_eq!(a.relation_to(&a), CausalRelation::Equal);
    assert_eq!(
        VectorClock::new().relation_to(&VectorClock::new()),
        CausalRelation::Equal
    );
}

/// `relation_to` is antisymmetric: Before one way implies After the other, and
/// Concurrent is symmetric.
#[test]
fn vector_clock_relation_is_antisymmetric() {
    // a strictly dominates b (a = {n1:2}, b = {n1:1}) -> a After b, b Before a.
    let mut a = VectorClock::new();
    a.increment("n1");
    a.increment("n1");
    let mut b = VectorClock::new();
    b.increment("n1");
    assert_eq!(a.relation_to(&b), CausalRelation::After);
    assert_eq!(b.relation_to(&a), CausalRelation::Before);

    // Disjoint advances -> Concurrent, symmetrically.
    let mut c = VectorClock::new();
    c.increment("n1");
    let mut d = VectorClock::new();
    d.increment("n2");
    assert_eq!(c.relation_to(&d), CausalRelation::Concurrent);
    assert_eq!(d.relation_to(&c), CausalRelation::Concurrent);
}

/// `increment` returns strictly increasing counters for a node and `get`
/// reflects the latest value.
#[test]
fn vector_clock_increment_is_strictly_monotone() {
    let mut a = VectorClock::new();
    let mut prev = 0u64;
    for _ in 0..50 {
        let now = a.increment("n1");
        assert!(
            now > prev,
            "increment must strictly increase ({now} <= {prev})"
        );
        prev = now;
    }
    assert_eq!(a.get("n1"), prev, "get must reflect the latest increment");
    assert_eq!(a.get("absent"), 0, "missing nodes read as zero");
}

// --- EventCausalityClock: HLC + Lamport receive rules ----------------------

/// HLC monotonicity: across ANY interleaving of local events and remote
/// observations, the (wall_time_ms, logical) pair is lexicographically
/// non-decreasing — the clock never moves backward. Also pins the Lamport
/// counter as strictly increasing on every event.
#[test]
fn causality_clock_hlc_and_lamport_are_monotone_across_interleavings() {
    let mut clock = EventCausalityClock::new("local");
    let mut remote = EventCausalityClock::new("remote");

    let mut prev_hlc = (0u64, 0u64);
    let mut prev_lamport = 0u64;

    // A scripted interleaving: forward wall-time jumps, stalls (equal wall
    // time, exercising the logical tie-breaker), and remote receives whose
    // wall clock both leads and lags the local clock.
    let local_walls = [10u64, 10, 10, 25, 25, 40, 40, 40];
    let recv_at = [15u64, 12, 30, 30];
    let mut ri = 0;
    for (i, &wall) in local_walls.iter().enumerate() {
        let stamp = clock.record_local_event(wall);
        let key = hlc_key(&stamp);
        assert!(
            key >= prev_hlc,
            "HLC must be lexicographically monotone on local event: {key:?} < {prev_hlc:?}"
        );
        assert!(
            stamp.lamport.counter > prev_lamport,
            "Lamport counter must strictly increase on local event"
        );
        prev_hlc = key;
        prev_lamport = stamp.lamport.counter;

        // Interleave a remote observation after some local events.
        if i % 2 == 1 && ri < recv_at.len() {
            let remote_wall = recv_at[ri];
            ri += 1;
            let remote_stamp = remote.record_local_event(remote_wall);
            let recv = clock.observe_remote(&remote_stamp, remote_wall.max(wall));
            let rkey = hlc_key(&recv);
            assert!(
                rkey >= prev_hlc,
                "HLC must be monotone across a receive: {rkey:?} < {prev_hlc:?}"
            );
            assert!(
                recv.lamport.counter > prev_lamport,
                "Lamport counter must strictly increase on receive"
            );
            prev_hlc = rkey;
            prev_lamport = recv.lamport.counter;
        }
    }
}

/// Lamport receive rule: after observing a remote stamp, the local counter is
/// strictly greater than BOTH the prior local counter and the remote counter.
#[test]
fn causality_clock_observe_remote_dominates_lamport() {
    let mut local = EventCausalityClock::new("local");
    // Advance local a little.
    local.record_local_event(5);
    let local_before = local.snapshot().lamport_counter;

    // A remote with a much higher Lamport counter.
    let mut remote = EventCausalityClock::new("remote");
    for _ in 0..10 {
        remote.record_local_event(100);
    }
    let remote_stamp = remote.record_local_event(100);
    let remote_counter = remote_stamp.lamport.counter;

    let recv = local.observe_remote(&remote_stamp, 100);
    assert!(
        recv.lamport.counter > local_before,
        "receive must advance past the prior local counter"
    );
    assert!(
        recv.lamport.counter > remote_counter,
        "receive must advance strictly past the remote counter (Lamport rule)"
    );
}

/// HLC + vector causal dominance: the stamp produced by `observe_remote`
/// dominates the remote stamp it absorbed — its HLC key is >= the remote's and
/// its vector frontier is strictly After the remote's (it carries the remote's
/// frontier plus the receiver's own increment).
#[test]
fn causality_clock_observe_remote_dominates_remote_frontier() {
    let mut local = EventCausalityClock::new("local");
    local.record_local_event(5);

    let mut remote = EventCausalityClock::new("remote");
    remote.record_local_event(20);
    let remote_stamp = remote.record_local_event(20);

    let recv = local.observe_remote(&remote_stamp, 20);

    // HLC of the receive dominates the remote's HLC.
    assert!(
        hlc_key(&recv) >= hlc_key(&remote_stamp),
        "receive HLC {:?} must dominate remote HLC {:?}",
        hlc_key(&recv),
        hlc_key(&remote_stamp)
    );

    // Vector frontier: the post-receive clock strictly happens-after the
    // remote frontier (max-merge of remote's entries + local self-increment).
    assert_eq!(
        recv.vector.relation_to(&remote_stamp.vector),
        CausalRelation::After,
        "post-receive vector frontier must dominate (happen-after) the remote frontier"
    );
    // And it carries the remote node's entry forward unchanged-or-higher.
    assert!(
        recv.vector.get("remote") >= remote_stamp.vector.get("remote"),
        "remote node entry must be preserved (pointwise-max) after receive"
    );
}

/// Two clocks advancing independently produce Concurrent frontiers until one
/// observes the other — a happens-before sanity round-trip over the public API.
#[test]
fn causality_clock_independent_then_observed_orders_correctly() {
    let mut a = EventCausalityClock::new("a");
    let mut b = EventCausalityClock::new("b");

    let a_stamp = a.record_local_event(10);
    let b_stamp = b.record_local_event(10);
    assert_eq!(
        a_stamp.vector.relation_to(&b_stamp.vector),
        CausalRelation::Concurrent,
        "independent local events must be concurrent"
    );

    // b observes a -> b now happens-after a.
    let b_after = b.observe_remote(&a_stamp, 11);
    assert_eq!(
        b_after.vector.relation_to(&a_stamp.vector),
        CausalRelation::After,
        "after observing a, b's frontier must happen-after a's"
    );
}
