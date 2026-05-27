//! Metamorphic / invariant tests for the HyperLogLog cardinality estimator
//! (the math behind storage's distinct-count sketch).
//!
//! These verify spec-level relations that must hold for any correct HLL,
//! independent of the (unknowable-in-advance) exact estimate:
//!
//! - **Insert-order independence** (equivalence): registers are per-bucket maxes,
//!   so the same multiset inserted in any order yields the same estimate.
//! - **Idempotence**: re-inserting an already-seen element changes nothing.
//! - **Merge commutativity** (equivalence): a∪b == b∪a.
//! - **Union lower bound** (inclusive): |a ∪ b| ≥ max(|a|, |b|).
//! - **Jaccard range**: a similarity coefficient must lie in [0, 1].
//!
//! No proptest dependency in this crate, so inputs come from a deterministic
//! LCG sweep across a range of precisions (low precision stresses the
//! small-range/linear-counting branch boundary where estimator discontinuities
//! live).

use frankenterm_core_telemetry_types::hyperloglog::HyperLogLog;

/// Deterministic xorshift64* PRNG — reproducible across runs/platforms.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

#[test]
fn insert_order_independence() {
    for p in [4u8, 8, 12, 14] {
        let mut forward = HyperLogLog::with_precision(p);
        let mut reverse = HyperLogLog::with_precision(p);
        let items: Vec<u64> = (0..5000u64).collect();
        for &i in &items {
            forward.insert(&i);
        }
        for &i in items.iter().rev() {
            reverse.insert(&i);
        }
        assert_eq!(
            forward.cardinality(),
            reverse.cardinality(),
            "p={p}: HLL estimate must be insert-order independent"
        );
    }
}

#[test]
fn idempotent_reinsertion() {
    for p in [4u8, 10, 14] {
        let mut hll = HyperLogLog::with_precision(p);
        for i in 0..3000u64 {
            hll.insert(&i);
        }
        let before = hll.cardinality();
        // Re-insert the exact same set; distinct cardinality must not move.
        for i in 0..3000u64 {
            hll.insert(&i);
        }
        assert_eq!(
            before,
            hll.cardinality(),
            "p={p}: re-inserting the same set must not change the estimate"
        );
    }
}

#[test]
fn cardinality_is_monotonic_nondecreasing() {
    // Sweep through the small-range → raw-estimate transition (the danger zone
    // for estimator discontinuities) at several precisions.
    for p in [4u8, 6, 8] {
        let mut hll = HyperLogLog::with_precision(p);
        let mut prev = hll.cardinality();
        let mut rng = Rng::new(0xC0FFEE ^ u64::from(p));
        for _ in 0..200_000u64 {
            hll.insert(&rng.next());
            let now = hll.cardinality();
            assert!(
                now >= prev,
                "p={p}: cardinality decreased on insert ({prev} -> {now}) — \
                 estimator is non-monotonic"
            );
            prev = now;
        }
    }
}

#[test]
fn merge_is_commutative() {
    for p in [4u8, 10, 14] {
        let mut a = HyperLogLog::with_precision(p);
        let mut b = HyperLogLog::with_precision(p);
        let mut rng = Rng::new(0xABCDEF ^ u64::from(p));
        for _ in 0..4000 {
            a.insert(&rng.next());
        }
        for _ in 0..4000 {
            b.insert(&rng.next());
        }
        let mut ab = a.clone();
        ab.merge(&b).unwrap();
        let mut ba = b.clone();
        ba.merge(&a).unwrap();
        assert_eq!(
            ab.cardinality(),
            ba.cardinality(),
            "p={p}: merge must be commutative"
        );
    }
}

#[test]
fn union_is_lower_bounded_by_each_input() {
    for p in [4u8, 8, 12] {
        let mut rng = Rng::new(0x1234 ^ u64::from(p));
        for trial in 0..40 {
            let na = (rng.next() % 5000) as usize;
            let nb = (rng.next() % 5000) as usize;
            let mut a = HyperLogLog::with_precision(p);
            let mut b = HyperLogLog::with_precision(p);
            for _ in 0..na {
                a.insert(&rng.next());
            }
            for _ in 0..nb {
                b.insert(&rng.next());
            }
            let mut union = a.clone();
            union.merge(&b).unwrap();
            let (ca, cb, cu) = (a.cardinality(), b.cardinality(), union.cardinality());
            assert!(
                cu >= ca && cu >= cb,
                "p={p} trial={trial}: union {cu} must be >= max(a={ca}, b={cb})"
            );
        }
    }
}

#[test]
fn jaccard_is_within_unit_interval() {
    for p in [4u8, 6, 8, 12] {
        let mut rng = Rng::new(0x5EED ^ u64::from(p));
        for trial in 0..60 {
            let na = (rng.next() % 6000) as usize;
            let nb = (rng.next() % 6000) as usize;
            let overlap = rng.next() % 2000;
            let mut a = HyperLogLog::with_precision(p);
            let mut b = HyperLogLog::with_precision(p);
            // Shared prefix so the pair has a non-trivial intersection.
            for i in 0..overlap {
                a.insert(&i);
                b.insert(&i);
            }
            for _ in 0..na {
                a.insert(&(rng.next() | (1 << 63)));
            }
            for _ in 0..nb {
                b.insert(&(rng.next() | (1 << 62)));
            }
            if let Some(j) = a.jaccard(&b) {
                assert!(
                    (0.0..=1.0).contains(&j),
                    "p={p} trial={trial}: Jaccard similarity {j} out of [0,1] \
                     (na={na}, nb={nb}, overlap={overlap})"
                );
            }
        }
    }
}
