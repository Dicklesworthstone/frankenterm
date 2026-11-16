#![allow(clippy::naive_bytecount, clippy::needless_range_loop)]
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
//! - Clone equivalence
//! - Range frequency brute-force agreement
//! - Empty and boundary edge cases
//! - Rank of absent symbol is always 0
//! - All-same-value data properties

use frankenterm_core::wavelet_tree::WaveletTree;
use proptest::prelude::*;

// -- Strategies ---------------------------------------------------------------

fn byte_sequence_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..max_len)
}

fn small_alphabet_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0u8..=10, 1..max_len)
}

// -- Brute-force reference ----------------------------------------------------

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

fn brute_range_count(data: &[u8], symbol: u8, lo: usize, hi: usize) -> usize {
    data[lo..hi].iter().filter(|&&b| b == symbol).count()
}

fn brute_range_frequencies(data: &[u8], lo: usize, hi: usize) -> Vec<(u8, usize)> {
    let mut counts = [0usize; 256];
    for &b in &data[lo..hi] {
        counts[b as usize] += 1;
    }
    let mut result = Vec::new();
    for (byte, &count) in counts.iter().enumerate() {
        if count > 0 {
            result.push((byte as u8, count));
        }
    }
    result
}

// -- Tests --------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // -- 1. Access matches original -------------------------------------------

    #[test]
    fn access_matches_original(data in byte_sequence_strategy(100)) {
        let wt = WaveletTree::new(&data);
        for (i, &b) in data.iter().enumerate() {
            prop_assert_eq!(wt.access(i), Some(b), "access mismatch at {}", i);
        }
        prop_assert!(wt.access(data.len()).is_none());
    }

    // -- 2. Rank matches brute force ------------------------------------------

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

    // -- 3. Rank at full length -----------------------------------------------

    #[test]
    fn rank_full_length(data in byte_sequence_strategy(50)) {
        let wt = WaveletTree::new(&data);
        // Sum of ranks of all possible symbols at full length should equal data length
        let total: usize = (0..=255u16).map(|s| wt.rank(s as u8, data.len())).sum();
        prop_assert_eq!(total, data.len());
    }

    // -- 4. Select matches brute force ----------------------------------------

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

    // -- 5. Rank/select inverse -----------------------------------------------

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

    // -- 6. Range count equals rank diff --------------------------------------

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

    // -- 7. Quantile matches sorted -------------------------------------------

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

    // -- 8. Quantile out of bounds --------------------------------------------

    #[test]
    fn quantile_out_of_bounds(data in byte_sequence_strategy(30)) {
        let wt = WaveletTree::new(&data);
        prop_assert!(wt.quantile(0, data.len(), data.len()).is_none());
    }

    // -- 9. Length and empty --------------------------------------------------

    #[test]
    fn length_correct(data in byte_sequence_strategy(100)) {
        let wt = WaveletTree::new(&data);
        prop_assert_eq!(wt.len(), data.len());
        let is_empty = data.is_empty();
        prop_assert_eq!(wt.is_empty(), is_empty);
    }

    // -- 10. Serde roundtrip --------------------------------------------------

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

    // -- 11. Range frequencies sum to range length ----------------------------

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

    // -- 12. Alphabet size ----------------------------------------------------

    #[test]
    fn alphabet_bounded(data in byte_sequence_strategy(50)) {
        let wt = WaveletTree::new(&data);
        let alpha = wt.alphabet_size();
        prop_assert!(alpha >= 1);
        prop_assert!(alpha <= 256);
        prop_assert!(alpha <= data.len());
    }

    // -- 13. Rank monotonicity ------------------------------------------------

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

    // =========================================================================
    // New tests (14-25)
    // =========================================================================

    // -- 14. Clone equivalence ------------------------------------------------

    #[test]
    fn clone_equivalence(data in byte_sequence_strategy(50)) {
        let wt = WaveletTree::new(&data);
        let cloned = wt.clone();
        prop_assert_eq!(cloned.len(), wt.len());
        prop_assert_eq!(cloned.alphabet_size(), wt.alphabet_size());
        for i in 0..data.len() {
            prop_assert_eq!(cloned.access(i), wt.access(i), "clone access mismatch at {}", i);
        }
        // Verify rank agrees on a few symbols
        for &sym in &[0u8, 128, 255] {
            prop_assert_eq!(
                cloned.rank(sym, data.len()),
                wt.rank(sym, data.len()),
                "clone rank mismatch for symbol {}", sym
            );
        }
    }

    // -- 15. Serde roundtrip preserves all queries ----------------------------

    #[test]
    fn serde_roundtrip_preserves_queries(
        data in small_alphabet_strategy(40),
        symbol in 0u8..=10
    ) {
        let wt = WaveletTree::new(&data);
        let json = serde_json::to_string(&wt).unwrap();
        let restored: WaveletTree = serde_json::from_str(&json).unwrap();

        // Rank should match for every position
        for pos in 0..=data.len() {
            prop_assert_eq!(
                restored.rank(symbol, pos),
                wt.rank(symbol, pos),
                "serde rank mismatch at pos {}", pos
            );
        }
        // Select should match
        let count = wt.rank(symbol, data.len());
        for nth in 1..=count {
            prop_assert_eq!(
                restored.select(symbol, nth),
                wt.select(symbol, nth),
                "serde select mismatch for nth={}", nth
            );
        }
        // Alphabet size should match
        prop_assert_eq!(restored.alphabet_size(), wt.alphabet_size());
    }

    // -- 16. Range frequency matches brute force ------------------------------

    #[test]
    fn range_frequency_matches_brute_force(
        data in small_alphabet_strategy(40),
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

        let wt_freqs = wt.range_frequencies(lo, hi);
        let brute_freqs = brute_range_frequencies(&data, lo, hi);
        prop_assert_eq!(
            wt_freqs.len(),
            brute_freqs.len(),
            "freq count mismatch for range [{}, {})", lo, hi
        );
        for (wt_pair, brute_pair) in wt_freqs.iter().zip(brute_freqs.iter()) {
            prop_assert_eq!(wt_pair, brute_pair, "freq pair mismatch in range [{}, {})", lo, hi);
        }
    }

    // -- 17. Range count matches brute force ----------------------------------

    #[test]
    fn range_count_matches_brute_force(
        data in small_alphabet_strategy(40),
        symbol in 0u8..=10,
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

        let expected = brute_range_count(&data, symbol, lo, hi);
        let actual = wt.range_count(symbol, lo, hi);
        prop_assert_eq!(actual, expected, "range_count({}, {}, {}) mismatch", symbol, lo, hi);
    }

    // -- 18. Rank of absent symbol is always 0 --------------------------------

    #[test]
    fn rank_absent_symbol_is_zero(data in small_alphabet_strategy(50)) {
        let wt = WaveletTree::new(&data);
        // Find a symbol not present in data (small alphabet 0..=10, so 11..=255 are absent)
        let absent: u8 = 200;
        let is_absent = !data.contains(&absent);
        prop_assert!(is_absent, "expected 200 to be absent from small-alphabet data");
        for pos in 0..=data.len() {
            prop_assert_eq!(
                wt.rank(absent, pos), 0,
                "rank of absent symbol should be 0 at pos {}", pos
            );
        }
        prop_assert!(wt.select(absent, 1).is_none());
    }

    // -- 19. All-same-value data properties -----------------------------------

    #[test]
    fn all_same_value_properties(
        val in any::<u8>(),
        len in 1usize..100
    ) {
        let data = vec![val; len];
        let wt = WaveletTree::new(&data);

        prop_assert_eq!(wt.len(), len);
        prop_assert_eq!(wt.alphabet_size(), 1);

        // Rank of val at every position matches pos
        for pos in 0..=len {
            prop_assert_eq!(wt.rank(val, pos), pos, "rank mismatch at pos {}", pos);
        }

        // Rank of any other symbol is always 0
        let other = val.wrapping_add(1);
        for pos in 0..=len {
            prop_assert_eq!(
                wt.rank(other, pos), 0,
                "rank of other symbol should be 0 at pos {}", pos
            );
        }

        // Select returns sequential positions
        for nth in 1..=len {
            prop_assert_eq!(
                wt.select(val, nth), Some(nth - 1),
                "select mismatch for nth={}", nth
            );
        }
        prop_assert!(wt.select(val, len + 1).is_none());

        // Every quantile in full range is val
        for k in 0..len {
            prop_assert_eq!(
                wt.quantile(0, len, k), Some(val),
                "quantile mismatch for k={}", k
            );
        }
    }

    // -- 20. Boundary values (0 and 255) --------------------------------------

    #[test]
    fn boundary_byte_values(
        count_zero in 0usize..20,
        count_ff in 0usize..20,
        middle in prop::collection::vec(1u8..=254, 0..20)
    ) {
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(0u8, count_zero));
        data.extend(&middle);
        data.extend(std::iter::repeat_n(255u8, count_ff));

        if data.is_empty() {
            return Ok(());
        }

        let wt = WaveletTree::new(&data);

        // Rank of 0 at end should equal count of 0s
        let expected_zeros = count_zero + middle.iter().filter(|&&b| b == 0).count();
        prop_assert_eq!(
            wt.rank(0, data.len()), expected_zeros,
            "rank(0) mismatch"
        );

        // Rank of 255 at end should equal count of 255s
        let expected_ffs = count_ff + middle.iter().filter(|&&b| b == 255).count();
        prop_assert_eq!(
            wt.rank(255, data.len()), expected_ffs,
            "rank(255) mismatch"
        );

        // Access recovers all values
        for (i, &b) in data.iter().enumerate() {
            prop_assert_eq!(wt.access(i), Some(b), "access mismatch at {}", i);
        }
    }

    // -- 21. Consecutive rank values differ by 0 or 1 -------------------------

    #[test]
    fn rank_step_is_zero_or_one(
        data in byte_sequence_strategy(60),
        symbol in any::<u8>()
    ) {
        let wt = WaveletTree::new(&data);
        for pos in 0..data.len() {
            let r_now = wt.rank(symbol, pos);
            let r_next = wt.rank(symbol, pos + 1);
            let step = r_next - r_now;
            prop_assert!(
                step <= 1,
                "rank step must be 0 or 1, got {} at pos {}", step, pos
            );
            // Step is 1 iff data[pos] == symbol
            let expected_step = usize::from(data[pos] == symbol);
            prop_assert_eq!(step, expected_step, "rank step mismatch at pos {}", pos);
        }
    }

    // -- 22. Quantile result is always in the range ---------------------------

    #[test]
    fn quantile_result_in_range(
        data in byte_sequence_strategy(40),
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
            if let Some(val) = wt.quantile(lo, hi, k) {
                // The returned value must exist in the [lo, hi) range of the data
                let found = data[lo..hi].contains(&val);
                prop_assert!(found, "quantile({}, {}, {}) = {} not found in range", lo, hi, k, val);
            }
        }
    }

    // -- 23. Select of 0 always returns None ----------------------------------

    #[test]
    fn select_zero_always_none(data in byte_sequence_strategy(30)) {
        let wt = WaveletTree::new(&data);
        for sym in 0..=255u16 {
            prop_assert!(
                wt.select(sym as u8, 0).is_none(),
                "select({}, 0) should be None", sym
            );
        }
    }

    // -- 24. Range frequencies contain only values in range -------------------

    #[test]
    fn range_frequencies_values_present(
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

        // Every symbol in the frequency list must appear in the data range
        for &(sym, count) in &freqs {
            let actual = data[lo..hi].iter().filter(|&&b| b == sym).count();
            prop_assert_eq!(
                count, actual,
                "frequency of symbol {} mismatch in range [{}, {})", sym, lo, hi
            );
            prop_assert!(count > 0, "zero-count symbol {} should not appear in frequencies", sym);
        }

        // Every symbol that appears in the range must be in the frequency list
        let freq_syms: std::collections::HashSet<u8> = freqs.iter().map(|&(s, _)| s).collect();
        for &b in &data[lo..hi] {
            prop_assert!(
                freq_syms.contains(&b),
                "symbol {} in range not found in frequency list", b
            );
        }
    }

    // -- 25. Quantile is monotonically non-decreasing with k ------------------

    #[test]
    fn quantile_monotonic_in_k(
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

        let range_len = hi - lo;
        let mut prev: Option<u8> = None;
        for k in 0..range_len {
            let val = wt.quantile(lo, hi, k);
            if let (Some(p), Some(v)) = (prev, val) {
                prop_assert!(
                    v >= p,
                    "quantile not monotonic: q({})={} > q({})={}", k - 1, p, k, v
                );
            }
            prev = val;
        }
    }

    // =========================================================================
    // New tests (26-32)
    // =========================================================================

    // -- 26. is_empty agrees with len -----------------------------------------

    #[test]
    fn is_empty_agrees_with_len(data in byte_sequence_strategy(80)) {
        let wt = WaveletTree::new(&data);
        let len = wt.len();
        let empty = wt.is_empty();
        if len == 0 {
            prop_assert!(empty, "len is 0 but is_empty returned false");
        } else {
            prop_assert!(!empty, "len is {} but is_empty returned true", len);
        }
    }

    // -- 27. Display format agrees with len and alphabet_size -----------------

    #[test]
    fn display_format_consistent(data in byte_sequence_strategy(60)) {
        let wt = WaveletTree::new(&data);
        let display = format!("{}", wt);
        let expected_len = wt.len();
        let expected_alpha = wt.alphabet_size();
        let expected = format!("WaveletTree(len={}, alphabet={})", expected_len, expected_alpha);
        prop_assert_eq!(display, expected);
    }

    // -- 28. Range count additivity: count(lo,hi) = count(lo,mid) + count(mid,hi)

    #[test]
    fn range_count_additivity(
        data in small_alphabet_strategy(50),
        symbol in 0u8..=10,
        lo_frac in 0.0f64..=0.3,
        mid_frac in 0.3f64..=0.6,
        hi_frac in 0.6f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let n = data.len();
        let lo = ((lo_frac * n as f64) as usize).min(n);
        let mid = ((mid_frac * n as f64) as usize).min(n);
        let hi = ((hi_frac * n as f64) as usize).min(n);

        if lo > mid || mid > hi {
            return Ok(());
        }

        let full = wt.range_count(symbol, lo, hi);
        let left = wt.range_count(symbol, lo, mid);
        let right = wt.range_count(symbol, mid, hi);
        prop_assert_eq!(
            full, left + right,
            "range_count({}, {}, {}) != sum of parts ({} + {})", symbol, lo, hi, left, right
        );
    }

    // -- 29. Serde roundtrip preserves range_frequencies -----------------------

    #[test]
    fn serde_roundtrip_preserves_range_frequencies(
        data in small_alphabet_strategy(40),
        lo_frac in 0.0f64..=0.3,
        hi_frac in 0.7f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let n = data.len();
        let lo = ((lo_frac * n as f64) as usize).min(n);
        let hi = ((hi_frac * n as f64) as usize).min(n);

        if lo >= hi {
            return Ok(());
        }

        let json = serde_json::to_string(&wt).unwrap();
        let restored: WaveletTree = serde_json::from_str(&json).unwrap();

        let orig_freqs = wt.range_frequencies(lo, hi);
        let rest_freqs = restored.range_frequencies(lo, hi);

        prop_assert_eq!(
            orig_freqs.len(), rest_freqs.len(),
            "serde roundtrip changed frequency count for range [{}, {})", lo, hi
        );
        for (orig_pair, rest_pair) in orig_freqs.iter().zip(rest_freqs.iter()) {
            prop_assert_eq!(
                orig_pair, rest_pair,
                "serde roundtrip changed frequency pair in range [{}, {})", lo, hi
            );
        }
    }

    // -- 30. Two-value data: rank/select partition the sequence ----------------

    #[test]
    fn two_value_partition(
        bits in prop::collection::vec(prop::bool::ANY, 1..80)
    ) {
        let a: u8 = 0;
        let b: u8 = 255;
        let data: Vec<u8> = bits.iter().map(|&bit| if bit { b } else { a }).collect();
        let wt = WaveletTree::new(&data);
        let n = data.len();

        let count_a = wt.rank(a, n);
        let count_b = wt.rank(b, n);
        prop_assert_eq!(count_a + count_b, n, "two-value counts don't sum to len");

        // Select of a covers all a-positions
        for nth in 1..=count_a {
            let pos = wt.select(a, nth);
            let is_some = pos.is_some();
            prop_assert!(is_some, "select(0, {}) should return Some", nth);
            let pos_val = pos.unwrap();
            let access_val = wt.access(pos_val);
            prop_assert_eq!(access_val, Some(a), "select led to wrong symbol at pos {}", pos_val);
        }
        // Select of b covers all b-positions
        for nth in 1..=count_b {
            let pos = wt.select(b, nth);
            let is_some = pos.is_some();
            prop_assert!(is_some, "select(255, {}) should return Some", nth);
            let pos_val = pos.unwrap();
            let access_val = wt.access(pos_val);
            prop_assert_eq!(access_val, Some(b), "select led to wrong symbol at pos {}", pos_val);
        }
    }

    // -- 31. Alphabet size equals number of distinct values in data -----------

    #[test]
    fn alphabet_size_matches_distinct_values(data in byte_sequence_strategy(80)) {
        let wt = WaveletTree::new(&data);
        let mut distinct: std::collections::HashSet<u8> = std::collections::HashSet::new();
        for &b in &data {
            distinct.insert(b);
        }
        let expected = distinct.len();
        let actual = wt.alphabet_size();
        prop_assert_eq!(actual, expected, "alphabet_size mismatch");
    }

    // -- 32. Range frequencies are sorted by symbol byte value ----------------

    #[test]
    fn range_frequencies_sorted_by_symbol(
        data in byte_sequence_strategy(50),
        lo_frac in 0.0f64..=0.4,
        hi_frac in 0.6f64..=1.0
    ) {
        let wt = WaveletTree::new(&data);
        let n = data.len();
        let lo = ((lo_frac * n as f64) as usize).min(n);
        let hi = ((hi_frac * n as f64) as usize).min(n);

        if lo >= hi {
            return Ok(());
        }

        let freqs = wt.range_frequencies(lo, hi);

        // Verify the returned list is sorted by symbol value (ascending)
        for window in freqs.windows(2) {
            let sym_a = window[0].0;
            let sym_b = window[1].0;
            prop_assert!(
                sym_a < sym_b,
                "frequencies not sorted: symbol {} should come before {}", sym_a, sym_b
            );
        }

        // Also verify no duplicate symbols
        let syms: Vec<u8> = freqs.iter().map(|&(s, _)| s).collect();
        let mut deduped = syms.clone();
        deduped.dedup();
        prop_assert_eq!(syms.len(), deduped.len(), "duplicate symbols in frequency list");
    }
}
