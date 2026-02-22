//! Edit distance algorithms for sequence comparison.
//!
//! Provides Levenshtein distance, Damerau-Levenshtein distance, and normalized
//! variants. Useful for comparing terminal output sequences, detecting behavioral
//! drift, classifying sessions by similarity, and matching anomalous output to
//! known patterns.
//!
//! # Algorithms
//!
//! - **Levenshtein**: Minimum insertions, deletions, and substitutions to
//!   transform one sequence into another. O(mn) time and O(min(m,n)) space.
//! - **Damerau-Levenshtein**: Extends Levenshtein with transpositions of
//!   adjacent elements. O(mn) time and space.
//! - **Hamming**: Edit distance for equal-length sequences (substitutions only).
//! - **LCS length**: Longest common subsequence length via DP.
//! - **Jaro-Winkler**: Similarity metric favoring strings with common prefixes.

use std::collections::HashMap;

/// Levenshtein edit distance between two byte slices.
///
/// Uses O(min(m,n)) space via two-row optimization.
#[must_use]
pub fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let n = short.len();
    let m = long.len();

    if n == 0 {
        return m;
    }

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(long[i - 1] != short[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Levenshtein distance between two strings.
#[must_use]
pub fn levenshtein_str(a: &str, b: &str) -> usize {
    levenshtein(a.as_bytes(), b.as_bytes())
}

/// Normalized Levenshtein similarity in [0.0, 1.0].
///
/// Returns 1.0 for identical sequences, 0.0 for maximally different.
#[must_use]
pub fn levenshtein_normalized(a: &[u8], b: &[u8]) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein(a, b);
    1.0 - (dist as f64 / max_len as f64)
}

/// Damerau-Levenshtein distance (optimal string alignment).
///
/// Extends Levenshtein with transposition of two adjacent characters.
/// Uses full matrix — O(mn) time and space.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn damerau_levenshtein(a: &[u8], b: &[u8]) -> usize {
    let n = a.len();
    let m = b.len();

    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }

    // Full matrix for transposition lookback
    let mut d = vec![vec![0usize; m + 1]; n + 1];

    for (i, row) in d.iter_mut().enumerate().take(n + 1) {
        row[0] = i;
    }
    for (j, slot) in d[0].iter_mut().enumerate().take(m + 1) {
        *slot = j;
    }

    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);

            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }

    d[n][m]
}

/// Damerau-Levenshtein distance between strings.
#[must_use]
pub fn damerau_levenshtein_str(a: &str, b: &str) -> usize {
    damerau_levenshtein(a.as_bytes(), b.as_bytes())
}

/// Normalized Damerau-Levenshtein similarity in [0.0, 1.0].
#[must_use]
pub fn damerau_levenshtein_normalized(a: &[u8], b: &[u8]) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = damerau_levenshtein(a, b);
    1.0 - (dist as f64 / max_len as f64)
}

/// Hamming distance between two equal-length byte slices.
///
/// Returns `None` if lengths differ.
#[must_use]
pub fn hamming(a: &[u8], b: &[u8]) -> Option<usize> {
    if a.len() != b.len() {
        return None;
    }
    Some(a.iter().zip(b.iter()).filter(|(x, y)| x != y).count())
}

/// Hamming distance between two strings.
#[must_use]
pub fn hamming_str(a: &str, b: &str) -> Option<usize> {
    hamming(a.as_bytes(), b.as_bytes())
}

/// Longest Common Subsequence length.
///
/// O(mn) time, O(min(m,n)) space.
#[must_use]
pub fn lcs_length(a: &[u8], b: &[u8]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let n = short.len();

    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];

    for &lb in long {
        for j in 1..=n {
            curr[j] = if lb == short[j - 1] {
                prev[j - 1] + 1
            } else {
                prev[j].max(curr[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
        curr.fill(0);
    }

    prev[n]
}

/// Longest Common Subsequence length for strings.
#[must_use]
pub fn lcs_length_str(a: &str, b: &str) -> usize {
    lcs_length(a.as_bytes(), b.as_bytes())
}

/// Jaro similarity in [0.0, 1.0].
#[must_use]
pub fn jaro(a: &[u8], b: &[u8]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let match_distance = (a.len().max(b.len()) / 2).saturating_sub(1);

    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0usize;
    let mut transpositions = 0usize;

    for i in 0..a.len() {
        let start = i.saturating_sub(match_distance);
        let end = (i + match_distance + 1).min(b.len());
        for j in start..end {
            if b_matched[j] || a[i] != b[j] {
                continue;
            }
            a_matched[i] = true;
            b_matched[j] = true;
            matches += 1;
            break;
        }
    }

    if matches == 0 {
        return 0.0;
    }

    let mut k = 0;
    for i in 0..a.len() {
        if !a_matched[i] {
            continue;
        }
        while !b_matched[k] {
            k += 1;
        }
        if a[i] != b[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0
}

/// Jaro-Winkler similarity in [0.0, 1.0].
///
/// Extends Jaro with a bonus for common prefixes (up to 4 characters).
#[must_use]
pub fn jaro_winkler(a: &[u8], b: &[u8]) -> f64 {
    let jaro_sim = jaro(a, b);
    let prefix_len = a
        .iter()
        .zip(b.iter())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count();
    let scaling = 0.1;
    (prefix_len as f64 * scaling).mul_add(1.0 - jaro_sim, jaro_sim)
}

/// Jaro-Winkler similarity for strings.
#[must_use]
pub fn jaro_winkler_str(a: &str, b: &str) -> f64 {
    jaro_winkler(a.as_bytes(), b.as_bytes())
}

/// Band-limited Levenshtein with early termination.
///
/// Only considers edits within a diagonal band of width `2*k+1`.
/// Returns `None` if the true distance exceeds `k`.
/// O(n*k) time and O(k) space.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn levenshtein_bounded(a: &[u8], b: &[u8], k: usize) -> Option<usize> {
    let n = a.len();
    let m = b.len();

    if n.abs_diff(m) > k {
        return None;
    }

    // Use the two-row DP but only fill within the band
    let mut prev = vec![usize::MAX; m + 1];
    let mut curr = vec![usize::MAX; m + 1];

    for (j, slot) in prev.iter_mut().enumerate().take(k.min(m) + 1) {
        *slot = j;
    }

    for i in 1..=n {
        curr[0] = i;
        if i > k {
            curr[0] = usize::MAX;
        }

        let j_min = i.saturating_sub(k);
        let j_max = (i + k).min(m);

        if j_min > 0 {
            curr[j_min - 1] = usize::MAX;
        }

        for j in j_min..=j_max {
            if j == 0 {
                curr[0] = i;
                continue;
            }
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut val = usize::MAX;
            if prev[j] != usize::MAX {
                val = val.min(prev[j] + 1);
            }
            if curr[j - 1] != usize::MAX {
                val = val.min(curr[j - 1] + 1);
            }
            if prev[j - 1] != usize::MAX {
                val = val.min(prev[j - 1] + cost);
            }
            curr[j] = val;
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    if prev[m] <= k { Some(prev[m]) } else { None }
}

/// Weighted edit distance with custom operation costs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditCosts {
    /// Cost of inserting a character.
    pub insert: f64,
    /// Cost of deleting a character.
    pub delete: f64,
    /// Cost of substituting a character.
    pub substitute: f64,
}

impl Default for EditCosts {
    fn default() -> Self {
        Self {
            insert: 1.0,
            delete: 1.0,
            substitute: 1.0,
        }
    }
}

/// Weighted Levenshtein distance with custom costs.
#[must_use]
pub fn levenshtein_weighted(a: &[u8], b: &[u8], costs: &EditCosts) -> f64 {
    let n = a.len();
    let m = b.len();

    let mut prev: Vec<f64> = (0..=m).map(|j| j as f64 * costs.insert).collect();
    let mut curr = vec![0.0f64; m + 1];

    for i in 1..=n {
        curr[0] = i as f64 * costs.delete;
        for j in 1..=m {
            let sub_cost = if a[i - 1] == b[j - 1] {
                0.0
            } else {
                costs.substitute
            };
            curr[j] = (prev[j] + costs.delete)
                .min(curr[j - 1] + costs.insert)
                .min(prev[j - 1] + sub_cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[m]
}

/// Edit operations for the edit script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOp {
    /// Keep character at position.
    Keep(usize),
    /// Insert character from `b` at position.
    Insert(usize),
    /// Delete character from `a` at position.
    Delete(usize),
    /// Substitute character at position.
    Substitute(usize),
}

/// Compute the edit script (sequence of operations) to transform `a` into `b`.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn edit_script(a: &[u8], b: &[u8]) -> Vec<EditOp> {
    let n = a.len();
    let m = b.len();

    // Full matrix for backtracking
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate().take(n + 1) {
        row[0] = i;
    }
    for (j, slot) in d[0].iter_mut().enumerate().take(m + 1) {
        *slot = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
        }
    }

    // Backtrack
    let mut ops = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] && d[i][j] == d[i - 1][j - 1] {
            ops.push(EditOp::Keep(i - 1));
            i -= 1;
            j -= 1;
        } else if i > 0 && j > 0 && d[i][j] == d[i - 1][j - 1] + 1 {
            ops.push(EditOp::Substitute(i - 1));
            i -= 1;
            j -= 1;
        } else if j > 0 && d[i][j] == d[i][j - 1] + 1 {
            ops.push(EditOp::Insert(j - 1));
            j -= 1;
        } else {
            ops.push(EditOp::Delete(i - 1));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Generic Levenshtein distance for arbitrary sequences.
#[must_use]
pub fn levenshtein_generic<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let n = short.len();
    let m = long.len();

    if n == 0 {
        return m;
    }

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(long[i - 1] != short[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Compute multiple distances from one reference against many targets.
///
/// Returns a vector of (index, distance) pairs sorted by distance.
#[must_use]
pub fn nearest_neighbors(
    reference: &[u8],
    targets: &[&[u8]],
    max_results: usize,
) -> Vec<(usize, usize)> {
    let mut results: Vec<(usize, usize)> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| (i, levenshtein(reference, t)))
        .collect();
    results.sort_by_key(|&(_, d)| d);
    results.truncate(max_results);
    results
}

/// Compute n-gram similarity between byte slices.
///
/// Uses character n-gram overlap (Jaccard coefficient on n-gram sets).
#[must_use]
pub fn ngram_similarity(a: &[u8], b: &[u8], n: usize) -> f64 {
    if n == 0 || a.len() < n || b.len() < n {
        if a == b {
            return 1.0;
        }
        return 0.0;
    }

    let mut a_grams: HashMap<&[u8], usize> = HashMap::new();
    for w in a.windows(n) {
        *a_grams.entry(w).or_insert(0) += 1;
    }

    let mut b_grams: HashMap<&[u8], usize> = HashMap::new();
    for w in b.windows(n) {
        *b_grams.entry(w).or_insert(0) += 1;
    }

    let mut intersection = 0usize;
    let mut union = 0usize;

    for (gram, &a_count) in &a_grams {
        let b_count = b_grams.get(gram).copied().unwrap_or(0);
        intersection += a_count.min(b_count);
        union += a_count.max(b_count);
    }
    for (gram, &b_count) in &b_grams {
        if !a_grams.contains_key(gram) {
            union += b_count;
        }
    }

    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Levenshtein --

    #[test]
    fn lev_empty_empty() {
        assert_eq!(levenshtein(b"", b""), 0);
    }

    #[test]
    fn lev_empty_nonempty() {
        assert_eq!(levenshtein(b"", b"abc"), 3);
        assert_eq!(levenshtein(b"xyz", b""), 3);
    }

    #[test]
    fn lev_identical() {
        assert_eq!(levenshtein(b"kitten", b"kitten"), 0);
    }

    #[test]
    fn lev_classic() {
        assert_eq!(levenshtein_str("kitten", "sitting"), 3);
    }

    #[test]
    fn lev_single_insert() {
        assert_eq!(levenshtein(b"abc", b"abcd"), 1);
    }

    #[test]
    fn lev_single_delete() {
        assert_eq!(levenshtein(b"abcd", b"abc"), 1);
    }

    #[test]
    fn lev_single_sub() {
        assert_eq!(levenshtein(b"abc", b"axc"), 1);
    }

    #[test]
    fn lev_symmetric() {
        assert_eq!(levenshtein(b"abc", b"def"), levenshtein(b"def", b"abc"));
    }

    #[test]
    fn lev_normalized_identical() {
        assert!((levenshtein_normalized(b"abc", b"abc") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lev_normalized_empty() {
        assert!((levenshtein_normalized(b"", b"") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lev_normalized_disjoint() {
        let sim = levenshtein_normalized(b"aaa", b"bbb");
        assert!((sim - 0.0).abs() < f64::EPSILON);
    }

    // -- Damerau-Levenshtein --

    #[test]
    fn dl_empty() {
        assert_eq!(damerau_levenshtein(b"", b""), 0);
        assert_eq!(damerau_levenshtein(b"", b"ab"), 2);
    }

    #[test]
    fn dl_transposition() {
        assert_eq!(damerau_levenshtein(b"ab", b"ba"), 1);
        assert_eq!(levenshtein(b"ab", b"ba"), 2); // no transposition
    }

    #[test]
    fn dl_classic() {
        assert_eq!(damerau_levenshtein_str("ca", "abc"), 3);
    }

    #[test]
    fn dl_normalized() {
        let sim = damerau_levenshtein_normalized(b"abc", b"abc");
        assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    // -- Hamming --

    #[test]
    fn hamming_equal() {
        assert_eq!(hamming(b"abc", b"abc"), Some(0));
    }

    #[test]
    fn hamming_one_diff() {
        assert_eq!(hamming(b"abc", b"axc"), Some(1));
    }

    #[test]
    fn hamming_different_length() {
        assert_eq!(hamming(b"ab", b"abc"), None);
    }

    #[test]
    fn hamming_all_diff() {
        assert_eq!(hamming_str("abc", "xyz"), Some(3));
    }

    // -- LCS --

    #[test]
    fn lcs_empty() {
        assert_eq!(lcs_length(b"", b"abc"), 0);
    }

    #[test]
    fn lcs_identical() {
        assert_eq!(lcs_length(b"abcdef", b"abcdef"), 6);
    }

    #[test]
    fn lcs_classic() {
        assert_eq!(lcs_length_str("ABCBDAB", "BDCAB"), 4);
    }

    #[test]
    fn lcs_no_common() {
        assert_eq!(lcs_length(b"abc", b"xyz"), 0);
    }

    // -- Jaro / Jaro-Winkler --

    #[test]
    fn jaro_identical() {
        assert!((jaro(b"abc", b"abc") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaro_empty() {
        assert!((jaro(b"", b"") - 1.0).abs() < f64::EPSILON);
        assert!((jaro(b"", b"a") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaro_classic() {
        let sim = jaro(b"MARTHA", b"MARHTA");
        assert!((sim - 0.9444).abs() < 0.001);
    }

    #[test]
    fn jw_prefix_boost() {
        let j = jaro(b"MARTHA", b"MARHTA");
        let jw = jaro_winkler(b"MARTHA", b"MARHTA");
        assert!(jw >= j);
    }

    #[test]
    fn jw_identical() {
        assert!((jaro_winkler_str("abc", "abc") - 1.0).abs() < f64::EPSILON);
    }

    // -- Bounded --

    #[test]
    fn bounded_within() {
        assert_eq!(levenshtein_bounded(b"kitten", b"sitting", 5), Some(3));
    }

    #[test]
    fn bounded_exact() {
        assert_eq!(levenshtein_bounded(b"kitten", b"sitting", 3), Some(3));
    }

    #[test]
    fn bounded_exceeded() {
        assert_eq!(levenshtein_bounded(b"kitten", b"sitting", 2), None);
    }

    #[test]
    fn bounded_empty() {
        assert_eq!(levenshtein_bounded(b"", b"abc", 3), Some(3));
        assert_eq!(levenshtein_bounded(b"", b"abcd", 3), None);
    }

    // -- Weighted --

    #[test]
    fn weighted_default() {
        let d = levenshtein_weighted(b"abc", b"axc", &EditCosts::default());
        assert!((d - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn weighted_expensive_sub() {
        let costs = EditCosts {
            insert: 1.0,
            delete: 1.0,
            substitute: 3.0,
        };
        let d = levenshtein_weighted(b"abc", b"axc", &costs);
        // sub costs 3, but insert+delete costs 2
        assert!((d - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn weighted_identical() {
        let d = levenshtein_weighted(b"abc", b"abc", &EditCosts::default());
        assert!((d - 0.0).abs() < f64::EPSILON);
    }

    // -- Edit script --

    #[test]
    fn script_empty() {
        let ops = edit_script(b"", b"");
        assert!(ops.is_empty());
    }

    #[test]
    fn script_identical() {
        let ops = edit_script(b"abc", b"abc");
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().all(|op| matches!(op, EditOp::Keep(_))));
    }

    #[test]
    fn script_insert() {
        let ops = edit_script(b"ac", b"abc");
        let inserts = ops
            .iter()
            .filter(|op| matches!(op, EditOp::Insert(_)))
            .count();
        assert_eq!(inserts, 1);
    }

    #[test]
    fn script_distance_matches() {
        let ops = edit_script(b"kitten", b"sitting");
        let dist: usize = ops
            .iter()
            .filter(|op| !matches!(op, EditOp::Keep(_)))
            .count();
        assert_eq!(dist, levenshtein(b"kitten", b"sitting"));
    }

    // -- Generic --

    #[test]
    fn generic_u32() {
        let a = vec![1u32, 2, 3];
        let b = vec![1, 4, 3];
        assert_eq!(levenshtein_generic(&a, &b), 1);
    }

    #[test]
    fn generic_empty() {
        let a: Vec<i32> = vec![];
        let b = vec![1, 2, 3];
        assert_eq!(levenshtein_generic(&a, &b), 3);
    }

    // -- Nearest neighbors --

    #[test]
    fn nn_basic() {
        let reference = b"abc";
        let targets: Vec<&[u8]> = vec![b"abc", b"ab", b"xyz", b"abcd"];
        let nn = nearest_neighbors(reference, &targets, 2);
        assert_eq!(nn.len(), 2);
        assert_eq!(nn[0], (0, 0)); // exact match
        assert_eq!(nn[1].1, 1); // distance 1
    }

    #[test]
    fn nn_empty_targets() {
        let nn = nearest_neighbors(b"abc", &[], 5);
        assert!(nn.is_empty());
    }

    // -- N-gram similarity --

    #[test]
    fn ngram_identical() {
        assert!((ngram_similarity(b"abcdef", b"abcdef", 2) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ngram_no_overlap() {
        let sim = ngram_similarity(b"aaa", b"bbb", 2);
        assert!((sim - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ngram_partial() {
        let sim = ngram_similarity(b"abcd", b"bcde", 2);
        assert!(sim > 0.0);
        assert!(sim < 1.0);
    }

    #[test]
    fn ngram_short_inputs() {
        // inputs shorter than n
        let sim = ngram_similarity(b"a", b"b", 2);
        assert!((sim - 0.0).abs() < f64::EPSILON);
    }
}
