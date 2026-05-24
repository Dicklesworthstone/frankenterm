//! Property-based tests for the event-bus causality primitives in
//! `frankenterm_core::events`: `VectorClock`, `LamportStamp`,
//! `HybridLogicalStamp`, and the composite causality stamps.
//!
//! These distributed-clock types carry well-defined algebraic laws but had
//! zero property coverage. The suite pins:
//!
//! - VectorClock: increment-by-one, merge = pointwise max (idempotent +
//!   commutative), `relation_to` reflexivity / antisymmetry, increment
//!   induces happens-after, and merge dominates its operands.
//! - HybridLogicalStamp: derived `Ord` is lexicographic over
//!   (wall_time_ms, logical, node_id).
//! - serde round-trips for the clock stamps and the snapshot view.

use proptest::prelude::*;

use frankenterm_core::events::{
    CausalRelation, EventCausalityClock, EventCausalitySnapshot, EventCausalityStamp,
    HybridLogicalStamp, LamportStamp, VectorClock,
};

/// Vector clock over a small node alphabet so clocks overlap and the
/// happens-before relations are actually exercised (not all Concurrent).
fn arb_vector_clock() -> impl Strategy<Value = VectorClock> {
    prop::collection::btree_map("[a-c]", 0_u64..20, 0..5).prop_map(|entries| VectorClock { entries })
}

fn arb_hlc() -> impl Strategy<Value = HybridLogicalStamp> {
    (0_u64..100, 0_u64..100, "[a-b]")
        .prop_map(|(wall, logical, node)| HybridLogicalStamp::new(wall, logical, node))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Incrementing a fresh node raises its counter by exactly one and the
    /// returned value matches the stored value.
    #[test]
    fn vc_increment_increases_by_one(vc in arb_vector_clock()) {
        let mut a = vc;
        // "z" is outside the [a-c] alphabet, so it always starts at 0.
        let before = a.get("z");
        prop_assert_eq!(before, 0);
        let returned = a.increment("z");
        prop_assert_eq!(returned, 1);
        prop_assert_eq!(a.get("z"), 1);
    }

    /// merge sets every node to the pointwise maximum of the two clocks.
    #[test]
    fn vc_merge_is_pointwise_max(a in arb_vector_clock(), b in arb_vector_clock()) {
        let mut m = a.clone();
        m.merge(&b);
        for node in a.entries.keys().chain(b.entries.keys()) {
            prop_assert_eq!(m.get(node), a.get(node).max(b.get(node)),
                "merge must be pointwise max at {}", node);
        }
    }

    /// merge is idempotent: merging the same clock twice changes nothing.
    #[test]
    fn vc_merge_idempotent(a in arb_vector_clock(), b in arb_vector_clock()) {
        let mut once = a.clone();
        once.merge(&b);
        let mut twice = once.clone();
        twice.merge(&b);
        prop_assert_eq!(once, twice);
    }

    /// merge is commutative: max(a, b) == max(b, a) componentwise.
    #[test]
    fn vc_merge_commutative(a in arb_vector_clock(), b in arb_vector_clock()) {
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        prop_assert_eq!(ab, ba);
    }

    /// relation_to is reflexive: a clock is Equal to itself.
    #[test]
    fn vc_relation_reflexive(a in arb_vector_clock()) {
        prop_assert_eq!(a.relation_to(&a), CausalRelation::Equal);
    }

    /// relation_to is antisymmetric: Before/After mirror, Equal/Concurrent
    /// are symmetric.
    #[test]
    fn vc_relation_antisymmetric(a in arb_vector_clock(), b in arb_vector_clock()) {
        let forward = a.relation_to(&b);
        let backward = b.relation_to(&a);
        let expected = match forward {
            CausalRelation::Before => CausalRelation::After,
            CausalRelation::After => CausalRelation::Before,
            CausalRelation::Equal => CausalRelation::Equal,
            CausalRelation::Concurrent => CausalRelation::Concurrent,
        };
        prop_assert_eq!(backward, expected);
    }

    /// Incrementing induces a strict happens-after: the bumped clock is
    /// After the original, and the original is Before the bumped clock.
    #[test]
    fn vc_increment_yields_after(a in arb_vector_clock()) {
        let mut bumped = a.clone();
        bumped.increment("z"); // fresh node: 0 -> 1, so strictly dominates
        prop_assert_eq!(bumped.relation_to(&a), CausalRelation::After);
        prop_assert_eq!(a.relation_to(&bumped), CausalRelation::Before);
    }

    /// A merged clock dominates each operand: it is Equal or After, never
    /// Before or Concurrent relative to the inputs.
    #[test]
    fn vc_merge_dominates_operands(a in arb_vector_clock(), b in arb_vector_clock()) {
        let mut m = a.clone();
        m.merge(&b);
        let rel_a = m.relation_to(&a);
        let rel_b = m.relation_to(&b);
        prop_assert!(matches!(rel_a, CausalRelation::Equal | CausalRelation::After),
            "merge result must dominate operand a, got {:?}", rel_a);
        prop_assert!(matches!(rel_b, CausalRelation::Equal | CausalRelation::After),
            "merge result must dominate operand b, got {:?}", rel_b);
    }

    /// HybridLogicalStamp's derived Ord is lexicographic over
    /// (wall_time_ms, logical, node_id).
    #[test]
    fn hlc_ord_is_lexicographic(a in arb_hlc(), b in arb_hlc()) {
        let expected = (a.wall_time_ms, a.logical, a.node_id.clone())
            .cmp(&(b.wall_time_ms, b.logical, b.node_id.clone()));
        prop_assert_eq!(a.cmp(&b), expected);
    }

    /// serde round-trips preserve every clock stamp and the snapshot view.
    #[test]
    fn causality_serde_roundtrips(
        counter in 0_u64..1_000_000,
        node in "[a-z]{1,8}",
        vc in arb_vector_clock(),
        hlc in arb_hlc(),
        vector_nodes in 0_usize..64,
    ) {
        let lamport = LamportStamp::new(counter, node.clone());
        let l_back: LamportStamp =
            serde_json::from_str(&serde_json::to_string(&lamport).unwrap()).unwrap();
        prop_assert_eq!(lamport, l_back);

        let h_back: HybridLogicalStamp =
            serde_json::from_str(&serde_json::to_string(&hlc).unwrap()).unwrap();
        prop_assert_eq!(hlc.clone(), h_back);

        let stamp = EventCausalityStamp {
            lamport: LamportStamp::new(counter, node.clone()),
            vector: vc.clone(),
            hybrid: hlc.clone(),
        };
        let s_back: EventCausalityStamp =
            serde_json::from_str(&serde_json::to_string(&stamp).unwrap()).unwrap();
        prop_assert_eq!(stamp, s_back);

        let snapshot = EventCausalitySnapshot {
            node_id: node,
            lamport_counter: counter,
            vector_nodes,
            hybrid_wall_time_ms: hlc.wall_time_ms,
            hybrid_logical: hlc.logical,
        };
        let snap_back: EventCausalitySnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        prop_assert_eq!(snapshot, snap_back);
    }

    /// The core HLC guarantee: successive local events produce strictly
    /// increasing hybrid stamps even when the physical clock stalls or
    /// regresses (the logical component absorbs it). Lamport increments by
    /// exactly one per local event.
    #[test]
    fn clock_local_events_strictly_monotonic(
        node in "[a-z]{1,6}",
        walls in prop::collection::vec(0_u64..1000, 1..30),
    ) {
        let mut clock = EventCausalityClock::new(node);
        let mut prev_hybrid: Option<HybridLogicalStamp> = None;
        let mut prev_lamport = 0_u64;
        for w in walls {
            let stamp = clock.record_local_event(w);
            prop_assert_eq!(stamp.lamport.counter, prev_lamport + 1,
                "lamport must increment by exactly one per local event");
            prev_lamport = stamp.lamport.counter;
            if let Some(p) = &prev_hybrid {
                prop_assert!(stamp.hybrid > *p,
                    "HLC must strictly increase even on stalled/regressing wall time: {:?} !> {:?}",
                    stamp.hybrid, p);
            }
            prev_hybrid = Some(stamp.hybrid.clone());
        }
    }

    /// observe_remote establishes happens-after causality: after merging a
    /// remote stamp, the local clock's vector strictly dominates the
    /// remote's, and both the Lamport counter and the hybrid stamp exceed
    /// the remote's. (Disjoint node alphabets guarantee the local node is
    /// absent from the remote vector, so domination is strict.)
    #[test]
    fn clock_observe_remote_establishes_causality(
        local_node in "[a-l]{1,4}",
        remote_node in "[m-z]{1,4}",
        local_walls in prop::collection::vec(0_u64..500, 0..10),
        remote_walls in prop::collection::vec(0_u64..500, 1..10),
        recv_wall in 0_u64..500,
    ) {
        let mut local = EventCausalityClock::new(local_node);
        for w in local_walls {
            local.record_local_event(w);
        }
        let mut remote = EventCausalityClock::new(remote_node);
        let mut remote_stamp: Option<EventCausalityStamp> = None;
        for w in remote_walls {
            remote_stamp = Some(remote.record_local_event(w));
        }
        let remote_stamp = remote_stamp.expect("remote_walls is non-empty");

        let result = local.observe_remote(&remote_stamp, recv_wall);

        prop_assert_eq!(result.vector.relation_to(&remote_stamp.vector), CausalRelation::After,
            "observing a remote stamp must make the local vector happen-after the remote");
        prop_assert!(result.lamport.counter > remote_stamp.lamport.counter,
            "lamport must exceed the observed remote counter");
        prop_assert!(result.hybrid > remote_stamp.hybrid,
            "hybrid stamp must exceed the observed remote stamp");
    }
}
