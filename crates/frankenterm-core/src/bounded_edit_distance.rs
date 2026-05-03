//! Bounded Levenshtein edit distance (Ukkonen 1985 diagonal-band variant).
//!
//! Replaces the full O(N×M) Wagner-Fischer DP with an O(N×k) banded DP
//! when the caller only needs to know whether the distance is ≤ k.
//! Early-terminates as soon as every cell in a column exceeds k.
//!
//! Reference: Ukkonen, Esko (1985) "Algorithms for approximate string
//! matching", Information and Control 64(1-3):100-118.
//!
//! Typical use case in this codebase: `output_compression::lines_similar`
//! takes a similarity threshold (e.g. 0.3 normalized edit distance), then
//! computes the full distance and divides. For typical agent-output lines
//! (~80-200 chars) at threshold 0.3, the bounded variant explores ~25-60
//! diagonals out of 6400-40000 total cells — a 5-100× speedup, plus the
//! early-termination short-circuit for clearly-dissimilar pairs.
//!
//! Sibling-module substrate (per cc1 swarm convention): this file ships
//! the bounded primitive + tests. Integration into output_compression
//! and any other pairwise-similarity callsite is a follow-up bead.

/// Compute Levenshtein distance, returning `None` if the true distance
/// exceeds `max_distance`. Equivalent to
/// `Some(edit_distance(a, b))` whenever the result is ≤ `max_distance`,
/// and `None` otherwise.
///
/// Time: O(N × k) where N = max(|a|, |b|) and k = max_distance.
/// Space: O(N).
///
/// Algorithm: Ukkonen's diagonal-band DP. The full Wagner-Fischer DP
/// has the property that any cell more than k diagonals from the main
/// diagonal must contain a value > k (length difference forces it).
/// We therefore compute only cells in the band [j - k, j + k] for each
/// row, and early-terminate when the minimum value in a row exceeds k.
#[must_use]
pub fn edit_distance_bounded(a: &[u8], b: &[u8], max_distance: usize) -> Option<usize> {
    let m = a.len();
    let n = b.len();

    // The length difference is a lower bound on edit distance.
    // If the difference already exceeds k, the true distance is > k.
    let len_diff = m.abs_diff(n);
    if len_diff > max_distance {
        return None;
    }

    if m == 0 {
        return if n <= max_distance { Some(n) } else { None };
    }
    if n == 0 {
        return if m <= max_distance { Some(m) } else { None };
    }

    // Width-(2k+1) band around the main diagonal. We store the full
    // row to keep indexing simple but only update cells in the band.
    // Cells outside the band are conceptually `usize::MAX / 2` (infinity).
    let inf = max_distance + 1;
    let mut prev = vec![inf; n + 1];
    let mut curr = vec![inf; n + 1];

    for j in 0..=max_distance.min(n) {
        prev[j] = j;
    }

    for i in 1..=m {
        curr[0] = if i <= max_distance { i } else { inf };

        let band_start = i.saturating_sub(max_distance).max(1);
        let band_end = (i + max_distance).min(n);

        // Cell at band_start - 1 is conceptually inf if outside band;
        // we already set curr[0] = inf when i > max_distance, but for
        // intermediate rows we must reset cells immediately left of
        // the band so they cannot be read as a stale value from the
        // previous row when curr is reused.
        if band_start > 1 {
            curr[band_start - 1] = inf;
        }

        let mut row_min = curr[0];

        for j in band_start..=band_end {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let del = prev[j].saturating_add(1);
            let ins = curr[j - 1].saturating_add(1);
            let sub = prev[j - 1].saturating_add(cost);
            let v = del.min(ins).min(sub);
            curr[j] = v;
            if v < row_min {
                row_min = v;
            }
        }

        // Cells immediately right of the band must be reset too, since
        // the next row's band may extend further left and read prev[j+1]
        // where j+1 was outside this row's band.
        if band_end < n {
            curr[band_end + 1] = inf;
        }

        // Early termination: if the smallest value in this row already
        // exceeds the budget, no completion of the DP can produce a
        // value ≤ max_distance. Bail.
        if row_min > max_distance {
            return None;
        }

        std::mem::swap(&mut prev, &mut curr);
    }

    let result = prev[n];
    if result <= max_distance {
        Some(result)
    } else {
        None
    }
}

/// Convenience: returns `true` iff `edit_distance(a, b) <= max_distance`.
/// Cheaper than `edit_distance_bounded(...).is_some()` only by a name.
#[must_use]
pub fn within_edit_distance(a: &[u8], b: &[u8], max_distance: usize) -> bool {
    edit_distance_bounded(a, b, max_distance).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation from output_compression (full Wagner-
    /// Fischer DP). Used for equivalence testing.
    fn edit_distance_reference(a: &[u8], b: &[u8]) -> usize {
        let m = a.len();
        let n = b.len();
        if m == 0 {
            return n;
        }
        if n == 0 {
            return m;
        }
        let mut prev: Vec<usize> = (0..=n).collect();
        let mut curr = vec![0; n + 1];
        for i in 1..=m {
            curr[0] = i;
            for j in 1..=n {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            }
            std::mem::swap(&mut prev, &mut curr);
        }
        prev[n]
    }

    #[test]
    fn empty_strings_zero_distance() {
        assert_eq!(edit_distance_bounded(b"", b"", 0), Some(0));
        assert_eq!(edit_distance_bounded(b"", b"abc", 5), Some(3));
        assert_eq!(edit_distance_bounded(b"abc", b"", 5), Some(3));
    }

    #[test]
    fn empty_vs_long_exceeds_bound_returns_none() {
        assert_eq!(edit_distance_bounded(b"", b"abcdef", 3), None);
        assert_eq!(edit_distance_bounded(b"abcdef", b"", 3), None);
    }

    #[test]
    fn identical_strings_zero_distance() {
        assert_eq!(edit_distance_bounded(b"hello", b"hello", 0), Some(0));
        assert_eq!(edit_distance_bounded(b"hello", b"hello", 100), Some(0));
    }

    #[test]
    fn substitution_cost_one() {
        // "abc" vs "abd" — single substitution at position 2.
        assert_eq!(edit_distance_bounded(b"abc", b"abd", 0), None);
        assert_eq!(edit_distance_bounded(b"abc", b"abd", 1), Some(1));
        assert_eq!(edit_distance_bounded(b"abc", b"abd", 5), Some(1));
    }

    #[test]
    fn insertion_and_deletion_cost_one() {
        assert_eq!(edit_distance_bounded(b"ab", b"abc", 1), Some(1));
        assert_eq!(edit_distance_bounded(b"abc", b"ab", 1), Some(1));
    }

    #[test]
    fn length_difference_exceeds_bound_short_circuits() {
        // |m - n| = 5; bound = 3 → must return None without DP work.
        assert_eq!(edit_distance_bounded(b"a", b"abcdef", 3), None);
    }

    #[test]
    fn classic_kitten_to_sitting_distance_3() {
        let a = b"kitten";
        let b = b"sitting";
        assert_eq!(edit_distance_bounded(a, b, 2), None);
        assert_eq!(edit_distance_bounded(a, b, 3), Some(3));
        assert_eq!(edit_distance_bounded(a, b, 100), Some(3));
    }

    #[test]
    fn equivalence_with_reference_under_bound() {
        let pairs: &[(&[u8], &[u8])] = &[
            (b"hello world", b"hello there"),
            (b"compile", b"compiler"),
            (b"sunday", b"saturday"),
            (b"abcdef", b"badcfe"),
            (b"the quick brown fox", b"the slow brown fox"),
            (b"ABCDEFGH", b"ABCDEFGH"),
            (b"x", b"y"),
            (b"prefix common", b"prefix common"),
        ];
        for (a, b) in pairs {
            let truth = edit_distance_reference(a, b);
            // Bound generously above truth — must equal exact distance.
            assert_eq!(
                edit_distance_bounded(a, b, truth + 5),
                Some(truth),
                "pair ({:?}, {:?})",
                a,
                b
            );
            // Bound at truth — still returns the value.
            assert_eq!(
                edit_distance_bounded(a, b, truth),
                Some(truth),
                "tight bound failed for ({:?}, {:?})",
                a,
                b
            );
            // Bound below truth — must return None.
            if truth > 0 {
                assert_eq!(
                    edit_distance_bounded(a, b, truth - 1),
                    None,
                    "below-bound should be None for ({:?}, {:?})",
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn early_termination_for_dissimilar_strings_returns_none() {
        // 50-byte strings with no common substructure; bound = 5.
        let a: Vec<u8> = (0..50).map(|i| b'a' + (i % 26) as u8).collect();
        let b: Vec<u8> = (0..50).map(|i| b'A' + (i % 26) as u8).collect();
        assert_eq!(edit_distance_bounded(&a, &b, 5), None);
    }

    #[test]
    fn within_edit_distance_matches_bounded() {
        assert!(within_edit_distance(b"abc", b"abd", 1));
        assert!(!within_edit_distance(b"abc", b"xyz", 2));
        assert!(within_edit_distance(b"", b"", 0));
    }

    #[test]
    fn long_strings_within_band_match_reference() {
        // Long similar strings (one substitution) — bounded should succeed
        // with bound=1 and match the reference.
        let mut a = vec![b'x'; 200];
        let mut b = a.clone();
        b[100] = b'y';
        assert_eq!(edit_distance_bounded(&a, &b, 0), None);
        assert_eq!(edit_distance_bounded(&a, &b, 1), Some(1));
        assert_eq!(edit_distance_reference(&a, &b), 1);

        // Two substitutions at distance — must NOT short-circuit
        // erroneously.
        a[50] = b'a';
        b[50] = b'b';
        assert_eq!(edit_distance_bounded(&a, &b, 1), None);
        assert_eq!(edit_distance_bounded(&a, &b, 2), Some(2));
    }
}
