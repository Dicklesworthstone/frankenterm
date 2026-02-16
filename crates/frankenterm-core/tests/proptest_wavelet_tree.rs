//! Property-based tests for `wavelet_tree` module.
//!
//! Verifies correctness invariants:
//! - Rank matches brute-force count
//! - Select finds correct position
//! - Rank/select are inverse
//! - Quantile matches sorted subarray
//! - Range count = rank(hi) - rank(lo)
//! - Access matches original
//! - Serde roundtrip

use frankenterm_core::wavelet_tree::WaveletTree;
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn byte_sequence_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..max_len)
}

fn small_alphabet_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0u8..=10, 1..max_len)
}

// ── Brute-force reference ──────────────────────────────────────────────

fn brute_rank(data: &[u8], symbol: u8, pos: usize) -> usize {
    data[..pos].iter().filter(|&&b| b == symbol).count()
}

fn brute_select(data: &[u8], symbol: u8, nth: usize) -> Option<usize> {
    let mut count = 0;
    for (i, &b) in data.iter().enumerate() {
        if b == symbol {
            count += 1;
            if count == nth {
                return Some(i);
            }
        }
    }
    None
}

fn brute_quantile(data: &[u8], lo: usize, hi: usize, k: usize) -> Option<u8> {
    if lo >= hi || k >= hi - lo {
        return None;
    }
    let mut slice: Vec<u8> = data[lo..hi].to_vec();
    slice.sort_unstable();
    Some(slice[k])
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── Access matches original ──────────────────────────────────

    #[test]
    fn access_matches_original(data in byte_sequence_strategy(100)) {
        let wt = WaveletTree::new(&data);
        for (i, &b) in data.iter().enumerate() {
            prop_assert_eq!(wt.access(i), Some(b), "access mismatch at {}", i);
        }
        prop_assert!(wt.access(data.len()).is_none());
    }

    // ── Rank matches brute force ─────────────────────────────────

    #[test]
    fn rank_matches_brute_force(
        data in small_alphabet_strategy(50),
        symbol in 0u8..=10,
        pos_frac in 0.0f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let pos = (pos_frac * data.len() as f64) as usize;
        let pos = pos.min(data.len());
        let expected = brute_rank(&data, symbol, pos);
        prop_assert_eq!(wt.rank(symbol, pos), expected, "rank({}, {}) mismatch", symbol, pos);
    }

    // ── Rank at full length ──────────────────────────────────────

    #[test]
    fn rank_full_length(data in byte_sequence_strategy(50)) {
        let wt = WaveletTree::new(&data);
        // Sum of ranks of all possible symbols at full length should equal data length
        let total: usize = (0..=255u16).map(|s| wt.rank(s as u8, data.len())).sum();
        prop_assert_eq!(total, data.len());
    }

    // ── Select matches brute force ───────────────────────────────

    #[test]
    fn select_matches_brute_force(
        data in small_alphabet_strategy(50),
        symbol in 0u8..=10
    ) {
        let wt = WaveletTree::new(&data);
        let total = brute_rank(&data, symbol, data.len());
        for nth in 1..=total {
            let expected = brute_select(&data, symbol, nth);
            prop_assert_eq!(wt.select(symbol, nth), expected, "select({}, {}) mismatch", symbol, nth);
        }
        // One beyond should return None
        prop_assert!(wt.select(symbol, total + 1).is_none());
    }

    // ── Rank/select inverse ──────────────────────────────────────

    #[test]
    fn rank_select_inverse(data in small_alphabet_strategy(50)) {
        let wt = WaveletTree::new(&data);
        // For each symbol, select(symbol, rank(symbol, i)+1) when data[i]==symbol should == i
        for (i, &b) in data.iter().enumerate() {
            let r = wt.rank(b, i + 1);
            let s = wt.select(b, r);
            prop_assert_eq!(s, Some(i), "rank/select inverse failed for byte {} at pos {}", b, i);
        }
    }

    // ── Range count consistency ──────────────────────────────────

    #[test]
    fn range_count_equals_rank_diff(
        data in small_alphabet_strategy(50),
        symbol in 0u8..=10,
        lo_frac in 0.0f64..=1.0,
        hi_frac in 0.0f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let lo = (lo_frac * data.len() as f64) as usize;
        let hi = (hi_frac * data.len() as f64) as usize;
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let lo = lo.min(data.len());
        let hi = hi.min(data.len());

        let expected = wt.rank(symbol, hi) - wt.rank(symbol, lo);
        prop_assert_eq!(wt.range_count(symbol, lo, hi), expected);
    }

    // ── Quantile matches sorted ──────────────────────────────────

    #[test]
    fn quantile_matches_sorted(
        data in small_alphabet_strategy(30),
        lo_frac in 0.0f64..=0.5,
        hi_frac in 0.5f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let lo = (lo_frac * data.len() as f64) as usize;
        let hi = (hi_frac * data.len() as f64) as usize;
        let lo = lo.min(data.len());
        let hi = hi.min(data.len());

        if lo >= hi {
            return Ok(());
        }

        for k in 0..(hi - lo) {
            let wt_result = wt.quantile(lo, hi, k);
            let expected = brute_quantile(&data, lo, hi, k);
            prop_assert_eq!(wt_result, expected, "quantile({}, {}, {}) mismatch", lo, hi, k);
        }
    }

    // ── Quantile out of bounds ───────────────────────────────────

    #[test]
    fn quantile_out_of_bounds(data in byte_sequence_strategy(30)) {
        let wt = WaveletTree::new(&data);
        prop_assert!(wt.quantile(0, data.len(), data.len()).is_none());
    }

    // ── Length and empty ──────────────────────────────────────────

    #[test]
    fn length_correct(data in byte_sequence_strategy(100)) {
        let wt = WaveletTree::new(&data);
        prop_assert_eq!(wt.len(), data.len());
        let is_empty = data.is_empty();
        prop_assert_eq!(wt.is_empty(), is_empty);
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(data in byte_sequence_strategy(50)) {
        let wt = WaveletTree::new(&data);
        let json = serde_json::to_string(&wt).unwrap();
        let restored: WaveletTree = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), wt.len());
        for i in 0..data.len() {
            prop_assert_eq!(restored.access(i), wt.access(i));
        }
    }

    // ── Range frequencies sum to range length ────────────────────

    #[test]
    fn range_freq_sum(
        data in byte_sequence_strategy(50),
        lo_frac in 0.0f64..=0.5,
        hi_frac in 0.5f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let lo = (lo_frac * data.len() as f64) as usize;
        let hi = (hi_frac * data.len() as f64) as usize;
        let lo = lo.min(data.len());
        let hi = hi.min(data.len());

        if lo >= hi {
            return Ok(());
        }

        let freqs = wt.range_frequencies(lo, hi);
        let total: usize = freqs.iter().map(|&(_, c)| c).sum();
        prop_assert_eq!(total, hi - lo, "frequencies don't sum to range length");
    }

    // ── Alphabet size ────────────────────────────────────────────

    #[test]
    fn alphabet_bounded(data in byte_sequence_strategy(50)) {
        let wt = WaveletTree::new(&data);
        let alpha = wt.alphabet_size();
        prop_assert!(alpha >= 1);
        prop_assert!(alpha <= 256);
        prop_assert!(alpha <= data.len());
    }

    // ── Rank monotonicity ────────────────────────────────────────

    #[test]
    fn rank_monotonic(data in small_alphabet_strategy(30), symbol in 0u8..=10) {
        let wt = WaveletTree::new(&data);
        let mut prev = 0;
        for pos in 0..=data.len() {
            let r = wt.rank(symbol, pos);
            prop_assert!(r >= prev, "rank not monotonic at pos {}", pos);
            prev = r;
        }
    }
}
