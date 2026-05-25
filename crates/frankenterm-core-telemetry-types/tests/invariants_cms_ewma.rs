//! Invariant / metamorphic tests for the Count-Min Sketch and EWMA estimators
//! used by storage + capacity telemetry.
//!
//! These pin the *defining guarantees* of each estimator — the properties that
//! make them correct to rely on — which currently lack rigorous randomized
//! coverage. A one-character regression (e.g. `min` → `max` in CMS estimate, or
//! a sign flip in the EWMA alpha) would silently break callers; these tests
//! turn that into a loud failure.
//!
//! Deterministic LCG inputs (no proptest dependency in this crate).

use frankenterm_core_telemetry_types::count_min_sketch::CountMinSketch;
use frankenterm_core_telemetry_types::ewma::Ewma;
use std::collections::HashMap;

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

// ===========================================================================
// Count-Min Sketch
// ===========================================================================

/// THE defining CMS guarantee: a point query is a one-sided estimator that
/// NEVER underestimates the true frequency. Collisions can only inflate a
/// counter, and `estimate` takes the min across rows, so `estimate(x)` ≥
/// `true_count(x)` for every key, at every sketch dimension.
#[test]
fn cms_never_underestimates() {
    for (w, d) in [(64usize, 3usize), (256, 4), (2048, 5)] {
        let mut cms = CountMinSketch::with_dimensions(w, d);
        let mut truth: HashMap<u64, u64> = HashMap::new();
        let mut rng = Rng::new(0xCEED ^ (w as u64));
        for _ in 0..20_000 {
            // Skewed key space (mod 500) so collisions are frequent.
            let key = rng.next() % 500;
            let add = (rng.next() % 4) + 1;
            cms.add(&key, add);
            *truth.entry(key).or_insert(0) += add;
        }
        for (&key, &true_count) in &truth {
            let est = cms.estimate(&key);
            assert!(
                est >= true_count,
                "w={w} d={d}: CMS UNDERESTIMATED key {key}: est={est} < true={true_count}"
            );
        }
    }
}

/// Estimates are monotonic non-decreasing as counts are added.
#[test]
fn cms_estimate_is_monotonic() {
    let mut cms = CountMinSketch::with_dimensions(128, 4);
    let mut rng = Rng::new(0xA0B0_C0D0);
    let mut prev_for_key: HashMap<u64, u64> = HashMap::new();
    for _ in 0..10_000 {
        let key = rng.next() % 200;
        cms.add(&key, 1);
        let now = cms.estimate(&key);
        let prev = prev_for_key.get(&key).copied().unwrap_or(0);
        assert!(now >= prev, "CMS estimate for {key} decreased: {prev} -> {now}");
        prev_for_key.insert(key, now);
    }
}

/// Inserting the same multiset in any order yields identical estimates — the
/// table cells are order-independent sums.
#[test]
fn cms_insert_order_independence() {
    let items: Vec<u64> = (0..3000u64).map(|i| (i * 2654435761) % 777).collect();
    let mut forward = CountMinSketch::with_dimensions(256, 4);
    let mut reverse = CountMinSketch::with_dimensions(256, 4);
    for &i in &items {
        forward.increment(&i);
    }
    for &i in items.iter().rev() {
        reverse.increment(&i);
    }
    for key in 0..777u64 {
        assert_eq!(
            forward.estimate(&key),
            reverse.estimate(&key),
            "CMS estimate must be insert-order independent for key {key}"
        );
    }
}

/// Merge of two sketches never underestimates the combined frequency, and is
/// commutative.
#[test]
fn cms_merge_is_commutative_and_never_underestimates() {
    let mut a = CountMinSketch::with_dimensions(256, 4);
    let mut b = CountMinSketch::with_dimensions(256, 4);
    let mut truth: HashMap<u64, u64> = HashMap::new();
    let mut rng = Rng::new(0x1234_5678);
    for _ in 0..8000 {
        let k = rng.next() % 400;
        a.increment(&k);
        *truth.entry(k).or_insert(0) += 1;
    }
    for _ in 0..8000 {
        let k = rng.next() % 400;
        b.increment(&k);
        *truth.entry(k).or_insert(0) += 1;
    }
    let mut ab = a.clone();
    ab.merge(&b).unwrap();
    let mut ba = b.clone();
    ba.merge(&a).unwrap();
    for (&k, &t) in &truth {
        assert_eq!(ab.estimate(&k), ba.estimate(&k), "merge must be commutative");
        assert!(ab.estimate(&k) >= t, "merged CMS underestimated {k}: {} < {t}", ab.estimate(&k));
    }
}

/// Degenerate dimensions must be clamped, never panic (no modulo-by-zero in the
/// hash index).
#[test]
fn cms_degenerate_dimensions_do_not_panic() {
    let mut cms = CountMinSketch::with_dimensions(0, 0);
    cms.increment(&"x");
    assert!(cms.estimate(&"x") >= 1);
    assert!(cms.width() >= 1, "width must be clamped above zero to avoid % by 0");
}

// ===========================================================================
// EWMA
// ===========================================================================

/// An EWMA is a sequence of convex combinations of observed values, so it can
/// never escape the [min, max] envelope of what it has seen.
#[test]
fn ewma_stays_within_observed_range() {
    let mut rng = Rng::new(0xE3A0_B1C2);
    let mut ewma = Ewma::with_half_life_ms(1000.0);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    let mut t = 0u64;
    for _ in 0..5000 {
        let v = (rng.next() % 10_000) as f64 - 5_000.0;
        t += (rng.next() % 3000) + 1; // strictly increasing time
        ewma.observe(v, t);
        lo = lo.min(v);
        hi = hi.max(v);
        let cur = ewma.value();
        assert!(
            cur >= lo - 1e-6 && cur <= hi + 1e-6,
            "EWMA {cur} escaped observed range [{lo}, {hi}]"
        );
    }
}

/// A constant input stream converges to that constant.
#[test]
fn ewma_constant_input_converges_to_constant() {
    let mut ewma = Ewma::with_half_life_ms(100.0);
    for i in 0..1000u64 {
        ewma.observe(42.5, i * 50);
    }
    assert!((ewma.value() - 42.5).abs() < 1e-6, "constant input must converge: {}", ewma.value());
}

/// Time moving backwards must not panic and must not push the estimate outside
/// the observed range (saturating dt → alpha = 0.5, still a convex blend).
#[test]
fn ewma_backwards_time_stays_bounded() {
    let mut ewma = Ewma::with_half_life_ms(1000.0);
    ewma.observe(10.0, 100);
    ewma.observe(20.0, 50); // time goes backwards
    ewma.observe(30.0, 0);
    let v = ewma.value();
    assert!((10.0..=30.0).contains(&v), "EWMA escaped [10,30] under backwards time: {v}");
}

/// Non-finite observations are dropped (never poison the running value).
#[test]
fn ewma_drops_non_finite() {
    let mut ewma = Ewma::with_half_life_ms(1000.0);
    ewma.observe(5.0, 0);
    ewma.observe(f64::NAN, 10);
    ewma.observe(f64::INFINITY, 20);
    let v = ewma.value();
    assert!(v.is_finite(), "EWMA value poisoned by non-finite input: {v}");
    assert!((v - 5.0).abs() < 1e-9, "non-finite inputs must not move the value: {v}");
}
