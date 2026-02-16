//! Suffix Array — efficient full-text substring search.
//!
//! A suffix array is a sorted array of all suffixes of a text string,
//! enabling O(m log n) substring search where m = pattern length and
//! n = text length. Combined with the LCP (Longest Common Prefix)
//! array, it supports efficient pattern matching and duplicate detection.
//!
//! # Design
//!
//! For text "banana":
//! ```text
//! Suffix Array (sorted suffix indices):
//!   SA[0] = 5  "a"
//!   SA[1] = 3  "ana"
//!   SA[2] = 1  "anana"
//!   SA[3] = 0  "banana"
//!   SA[4] = 4  "na"
//!   SA[5] = 2  "nana"
//!
//! LCP Array (shared prefix lengths between adjacent sorted suffixes):
//!   LCP[0] = 0  (no predecessor)
//!   LCP[1] = 1  "a" shared between "a" and "ana"
//!   LCP[2] = 3  "ana" shared between "ana" and "anana"
//!   LCP[3] = 0  nothing shared between "anana" and "banana"
//!   LCP[4] = 0  nothing shared between "banana" and "na"
//!   LCP[5] = 2  "na" shared between "na" and "nana"
//! ```
//!
//! # Use Cases in FrankenTerm
//!
//! - **Scrollback search**: Find all occurrences of a pattern in terminal output.
//! - **Pattern detection**: Identify repeated substrings in agent output.
//! - **Log analysis**: Locate error messages across captured output.
//! - **Deduplication**: Find longest repeated substrings for compression.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Suffix Array ───────────────────────────────────────────────────────

/// A suffix array with optional LCP array for a text string.
///
/// Provides O(m log n) substring search where m is the pattern length
/// and n is the text length. Construction is O(n log² n) using the
/// prefix-doubling algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuffixArray {
    /// The original text.
    text: Vec<u8>,
    /// Sorted array of suffix start positions.
    sa: Vec<usize>,
    /// LCP array: `lcp[i]` = length of longest common prefix between
    /// `text[sa[i]..]` and `text[sa[i-1]..]`. `lcp[0] = 0`.
    lcp: Vec<usize>,
}

impl SuffixArray {
    /// Build a suffix array from a byte slice.
    ///
    /// Construction time: O(n log² n).
    pub fn new(text: &[u8]) -> Self {
        let n = text.len();
        if n == 0 {
            return Self {
                text: Vec::new(),
                sa: Vec::new(),
                lcp: Vec::new(),
            };
        }

        let sa = Self::build_sa(text);
        let lcp = Self::build_lcp(text, &sa);

        Self {
            text: text.to_vec(),
            sa,
            lcp,
        }
    }

    /// Build from a string.
    pub fn from_str(text: &str) -> Self {
        Self::new(text.as_bytes())
    }

    /// Return the text length.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Get the suffix array.
    pub fn suffix_array(&self) -> &[usize] {
        &self.sa
    }

    /// Get the LCP array.
    pub fn lcp_array(&self) -> &[usize] {
        &self.lcp
    }

    /// Get the original text.
    pub fn text(&self) -> &[u8] {
        &self.text
    }

    /// Search for all occurrences of a pattern.
    ///
    /// Returns a sorted list of starting positions in the original text.
    /// Time complexity: O(m log n + k) where m = pattern length, k = results.
    pub fn search(&self, pattern: &[u8]) -> Vec<usize> {
        if pattern.is_empty() || self.sa.is_empty() {
            return Vec::new();
        }

        // Binary search for the leftmost match
        let left = self.lower_bound(pattern);
        let right = self.upper_bound(pattern);

        if left >= right {
            return Vec::new();
        }

        let mut results: Vec<usize> = self.sa[left..right].to_vec();
        results.sort_unstable();
        results
    }

    /// Search for a string pattern.
    pub fn search_str(&self, pattern: &str) -> Vec<usize> {
        self.search(pattern.as_bytes())
    }

    /// Count the number of occurrences of a pattern.
    ///
    /// Time complexity: O(m log n).
    pub fn count(&self, pattern: &[u8]) -> usize {
        if pattern.is_empty() || self.sa.is_empty() {
            return 0;
        }
        let left = self.lower_bound(pattern);
        let right = self.upper_bound(pattern);
        right - left
    }

    /// Count occurrences of a string pattern.
    pub fn count_str(&self, pattern: &str) -> usize {
        self.count(pattern.as_bytes())
    }

    /// Find the longest repeated substring.
    ///
    /// Returns `(start_pos, length)` of the longest substring that appears
    /// at least twice. Returns `(0, 0)` if no repeats exist.
    pub fn longest_repeated_substring(&self) -> (usize, usize) {
        if self.lcp.is_empty() {
            return (0, 0);
        }

        let mut max_lcp = 0;
        let mut max_idx = 0;

        for (i, &l) in self.lcp.iter().enumerate() {
            if l > max_lcp {
                max_lcp = l;
                max_idx = i;
            }
        }

        if max_lcp == 0 {
            (0, 0)
        } else {
            (self.sa[max_idx], max_lcp)
        }
    }

    /// Find the longest repeated substring as a string slice.
    pub fn longest_repeated_substring_str(&self) -> &[u8] {
        let (pos, len) = self.longest_repeated_substring();
        if len == 0 {
            &[]
        } else {
            &self.text[pos..pos + len]
        }
    }

    /// Count the number of distinct substrings.
    ///
    /// Uses the formula: n*(n+1)/2 - sum(LCP).
    pub fn distinct_substring_count(&self) -> usize {
        let n = self.text.len();
        if n == 0 {
            return 0;
        }
        let total = n * (n + 1) / 2;
        let lcp_sum: usize = self.lcp.iter().sum();
        total - lcp_sum
    }

    // ── Internal: Suffix array construction ────────────────────────

    /// Build suffix array using prefix doubling (O(n log² n)).
    fn build_sa(text: &[u8]) -> Vec<usize> {
        let n = text.len();
        let mut sa: Vec<usize> = (0..n).collect();
        let mut rank: Vec<i64> = text.iter().map(|&b| b as i64).collect();
        let mut new_rank = vec![0i64; n];

        let mut k = 1;
        while k < n {
            let rank_ref = rank.clone();
            sa.sort_by(|&a, &b| {
                let ra = rank_ref[a];
                let rb = rank_ref[b];
                if ra != rb {
                    return ra.cmp(&rb);
                }
                let ra2 = if a + k < n { rank_ref[a + k] } else { -1 };
                let rb2 = if b + k < n { rank_ref[b + k] } else { -1 };
                ra2.cmp(&rb2)
            });

            new_rank[sa[0]] = 0;
            for i in 1..n {
                let prev = sa[i - 1];
                let curr = sa[i];
                let same_first = rank[prev] == rank[curr];
                let prev_second = if prev + k < n { rank[prev + k] } else { -1 };
                let curr_second = if curr + k < n { rank[curr + k] } else { -1 };
                let same_second = prev_second == curr_second;

                new_rank[curr] = new_rank[prev] + if same_first && same_second { 0 } else { 1 };
            }

            rank.copy_from_slice(&new_rank);

            // If all ranks are unique, we're done
            if rank[sa[n - 1]] as usize == n - 1 {
                break;
            }

            k *= 2;
        }

        sa
    }

    /// Build LCP array using Kasai's algorithm (O(n)).
    fn build_lcp(text: &[u8], sa: &[usize]) -> Vec<usize> {
        let n = text.len();
        let mut lcp = vec![0usize; n];
        let mut inv_sa = vec![0usize; n];

        for (rank, &pos) in sa.iter().enumerate() {
            inv_sa[pos] = rank;
        }

        let mut h = 0usize;
        for i in 0..n {
            let rank = inv_sa[i];
            if rank == 0 {
                h = 0;
                continue;
            }

            let prev = sa[rank - 1];
            while i + h < n && prev + h < n && text[i + h] == text[prev + h] {
                h += 1;
            }

            lcp[rank] = h;
            h = h.saturating_sub(1);
        }

        lcp
    }

    // ── Internal: Binary search ────────────────────────────────────

    fn lower_bound(&self, pattern: &[u8]) -> usize {
        let mut lo = 0;
        let mut hi = self.sa.len();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let suffix = &self.text[self.sa[mid]..];
            let cmp_len = pattern.len().min(suffix.len());
            if suffix[..cmp_len] < *pattern {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        lo
    }

    fn upper_bound(&self, pattern: &[u8]) -> usize {
        let mut lo = 0;
        let mut hi = self.sa.len();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let suffix = &self.text[self.sa[mid]..];
            let cmp_len = pattern.len().min(suffix.len());
            if suffix[..cmp_len] <= *pattern {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        lo
    }
}

// ── Display ────────────────────────────────────────────────────────────

impl fmt::Display for SuffixArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SuffixArray({} bytes, {} distinct substrings)",
            self.text.len(),
            self.distinct_substring_count()
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text() {
        let sa = SuffixArray::new(b"");
        assert!(sa.is_empty());
        assert_eq!(sa.len(), 0);
        assert!(sa.search(b"anything").is_empty());
        assert_eq!(sa.count(b"anything"), 0);
    }

    #[test]
    fn single_char() {
        let sa = SuffixArray::from_str("a");
        assert_eq!(sa.len(), 1);
        assert_eq!(sa.suffix_array(), &[0]);
        assert_eq!(sa.search_str("a"), vec![0]);
        assert_eq!(sa.count_str("a"), 1);
    }

    #[test]
    fn banana() {
        let sa = SuffixArray::from_str("banana");
        assert_eq!(sa.suffix_array(), &[5, 3, 1, 0, 4, 2]);
        assert_eq!(sa.lcp_array(), &[0, 1, 3, 0, 0, 2]);
    }

    #[test]
    fn search_basic() {
        let sa = SuffixArray::from_str("abcabcabc");
        let results = sa.search_str("abc");
        assert_eq!(results, vec![0, 3, 6]);
    }

    #[test]
    fn search_not_found() {
        let sa = SuffixArray::from_str("hello world");
        assert!(sa.search_str("xyz").is_empty());
        assert_eq!(sa.count_str("xyz"), 0);
    }

    #[test]
    fn search_single_char() {
        let sa = SuffixArray::from_str("abracadabra");
        let results = sa.search_str("a");
        assert_eq!(results, vec![0, 3, 5, 7, 10]);
        assert_eq!(sa.count_str("a"), 5);
    }

    #[test]
    fn search_full_text() {
        let sa = SuffixArray::from_str("hello");
        assert_eq!(sa.search_str("hello"), vec![0]);
    }

    #[test]
    fn longest_repeated() {
        let sa = SuffixArray::from_str("banana");
        let (pos, len) = sa.longest_repeated_substring();
        assert_eq!(len, 3); // "ana"
        let substr = &sa.text()[pos..pos + len];
        assert_eq!(substr, b"ana");
    }

    #[test]
    fn longest_repeated_no_repeat() {
        let sa = SuffixArray::from_str("abcde");
        let (_, len) = sa.longest_repeated_substring();
        assert_eq!(len, 0);
    }

    #[test]
    fn distinct_substrings() {
        let sa = SuffixArray::from_str("abc");
        // Substrings: a, ab, abc, b, bc, c = 6
        assert_eq!(sa.distinct_substring_count(), 6);
    }

    #[test]
    fn distinct_substrings_repeated() {
        let sa = SuffixArray::from_str("aaa");
        // Substrings: a, aa, aaa = 3
        assert_eq!(sa.distinct_substring_count(), 3);
    }

    #[test]
    fn all_same_chars() {
        let sa = SuffixArray::from_str("aaaa");
        assert_eq!(sa.search_str("a"), vec![0, 1, 2, 3]);
        assert_eq!(sa.search_str("aa"), vec![0, 1, 2]);
        assert_eq!(sa.search_str("aaa"), vec![0, 1]);
        assert_eq!(sa.search_str("aaaa"), vec![0]);
        assert_eq!(sa.search_str("aaaaa"), Vec::<usize>::new());
    }

    #[test]
    fn binary_data() {
        let data = vec![0u8, 1, 0, 1, 0, 1, 2];
        let sa = SuffixArray::new(&data);
        assert_eq!(sa.search(&[0, 1]), vec![0, 2, 4]);
    }

    #[test]
    fn serde_roundtrip() {
        let sa = SuffixArray::from_str("hello world");
        let json = serde_json::to_string(&sa).unwrap();
        let restored: SuffixArray = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 11);
        assert_eq!(restored.search_str("llo"), vec![2]);
        assert_eq!(restored.suffix_array(), sa.suffix_array());
        assert_eq!(restored.lcp_array(), sa.lcp_array());
    }

    #[test]
    fn display_format() {
        let sa = SuffixArray::from_str("hello");
        let s = format!("{}", sa);
        assert!(s.contains("5 bytes"));
    }

    #[test]
    fn longer_text() {
        let text = "the quick brown fox jumps over the lazy dog";
        let sa = SuffixArray::from_str(text);

        assert_eq!(sa.search_str("the"), vec![0, 31]);
        assert_eq!(sa.count_str("the"), 2);
        assert_eq!(sa.search_str("fox"), vec![16]);
        assert!(sa.search_str("cat").is_empty());
    }

    #[test]
    fn count_matches_search_len() {
        let sa = SuffixArray::from_str("mississippi");
        assert_eq!(sa.count_str("issi"), sa.search_str("issi").len());
        assert_eq!(sa.count_str("ss"), sa.search_str("ss").len());
        assert_eq!(sa.count_str("i"), sa.search_str("i").len());
    }

    #[test]
    fn suffix_array_is_permutation() {
        let sa = SuffixArray::from_str("mississippi");
        let arr = sa.suffix_array();
        let n = sa.len();

        // Must be a permutation of 0..n
        let mut sorted = arr.to_vec();
        sorted.sort_unstable();
        let expected: Vec<usize> = (0..n).collect();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn suffixes_are_sorted() {
        let sa = SuffixArray::from_str("banana");
        let arr = sa.suffix_array();
        let text = sa.text();

        for w in arr.windows(2) {
            let s1 = &text[w[0]..];
            let s2 = &text[w[1]..];
            assert!(s1 <= s2, "suffixes not sorted: {:?} > {:?}", s1, s2);
        }
    }

    #[test]
    fn lcp_values_correct() {
        let sa = SuffixArray::from_str("banana");
        let arr = sa.suffix_array();
        let text = sa.text();
        let lcp = sa.lcp_array();

        assert_eq!(lcp[0], 0); // First has no predecessor

        for i in 1..arr.len() {
            let s1 = &text[arr[i - 1]..];
            let s2 = &text[arr[i]..];
            let common = s1
                .iter()
                .zip(s2.iter())
                .take_while(|(a, b)| a == b)
                .count();
            assert_eq!(lcp[i], common, "LCP[{}] mismatch", i);
        }
    }

    // ── Additional tests ──────────────────────────────────────────────

    #[test]
    fn search_empty_pattern() {
        let sa = SuffixArray::from_str("hello");
        assert!(sa.search(b"").is_empty());
        assert_eq!(sa.count(b""), 0);
    }

    #[test]
    fn search_on_empty_text() {
        let sa = SuffixArray::new(b"");
        assert!(sa.search(b"a").is_empty());
        assert_eq!(sa.count(b"a"), 0);
    }

    #[test]
    fn search_str_and_search_consistent() {
        let sa = SuffixArray::from_str("abcabc");
        assert_eq!(sa.search_str("abc"), sa.search(b"abc"));
    }

    #[test]
    fn count_str_and_count_consistent() {
        let sa = SuffixArray::from_str("abcabc");
        assert_eq!(sa.count_str("abc"), sa.count(b"abc"));
    }

    #[test]
    fn search_suffix() {
        let sa = SuffixArray::from_str("hello world");
        assert_eq!(sa.search_str("world"), vec![6]);
    }

    #[test]
    fn search_prefix() {
        let sa = SuffixArray::from_str("hello world");
        assert_eq!(sa.search_str("hello"), vec![0]);
    }

    #[test]
    fn search_middle() {
        let sa = SuffixArray::from_str("hello world");
        assert_eq!(sa.search_str("lo w"), vec![3]);
    }

    #[test]
    fn search_pattern_longer_than_text() {
        let sa = SuffixArray::from_str("hi");
        assert!(sa.search_str("hello").is_empty());
    }

    #[test]
    fn search_overlapping_occurrences() {
        let sa = SuffixArray::from_str("aaaa");
        // "aa" appears at 0, 1, 2
        assert_eq!(sa.search_str("aa"), vec![0, 1, 2]);
    }

    #[test]
    fn longest_repeated_empty_text() {
        let sa = SuffixArray::new(b"");
        assert_eq!(sa.longest_repeated_substring(), (0, 0));
    }

    #[test]
    fn longest_repeated_single_char() {
        let sa = SuffixArray::from_str("a");
        assert_eq!(sa.longest_repeated_substring(), (0, 0));
    }

    #[test]
    fn longest_repeated_all_same() {
        let sa = SuffixArray::from_str("aaaa");
        let (_, len) = sa.longest_repeated_substring();
        assert_eq!(len, 3); // "aaa" is repeated
    }

    #[test]
    fn longest_repeated_substring_str() {
        let sa = SuffixArray::from_str("abcabc");
        let substr = sa.longest_repeated_substring_str();
        assert_eq!(substr, b"abc");
    }

    #[test]
    fn longest_repeated_substring_str_empty() {
        let sa = SuffixArray::new(b"");
        assert!(sa.longest_repeated_substring_str().is_empty());
    }

    #[test]
    fn distinct_substrings_empty() {
        let sa = SuffixArray::new(b"");
        assert_eq!(sa.distinct_substring_count(), 0);
    }

    #[test]
    fn distinct_substrings_single_char() {
        let sa = SuffixArray::from_str("a");
        assert_eq!(sa.distinct_substring_count(), 1);
    }

    #[test]
    fn distinct_substrings_all_unique() {
        let sa = SuffixArray::from_str("abcd");
        // a,ab,abc,abcd,b,bc,bcd,c,cd,d = 10
        assert_eq!(sa.distinct_substring_count(), 10);
    }

    #[test]
    fn text_accessor() {
        let sa = SuffixArray::from_str("hello");
        assert_eq!(sa.text(), b"hello");
    }

    #[test]
    fn suffix_array_len_matches_text() {
        let sa = SuffixArray::from_str("hello");
        assert_eq!(sa.suffix_array().len(), 5);
        assert_eq!(sa.lcp_array().len(), 5);
    }

    #[test]
    fn serde_roundtrip_empty() {
        let sa = SuffixArray::new(b"");
        let json = serde_json::to_string(&sa).unwrap();
        let restored: SuffixArray = serde_json::from_str(&json).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn display_empty() {
        let sa = SuffixArray::new(b"");
        let s = format!("{}", sa);
        assert!(s.contains("0 bytes"));
    }

    #[test]
    fn display_includes_distinct_count() {
        let sa = SuffixArray::from_str("abc");
        let s = format!("{}", sa);
        assert!(s.contains("6 distinct"));
    }

    #[test]
    fn clone_independence() {
        let sa = SuffixArray::from_str("hello");
        let clone = sa.clone();
        assert_eq!(sa.search_str("llo"), clone.search_str("llo"));
    }

    #[test]
    fn mississippi_detailed() {
        let sa = SuffixArray::from_str("mississippi");
        assert_eq!(sa.search_str("issi"), vec![1, 4]);
        assert_eq!(sa.search_str("ss"), vec![2, 5]);
        assert_eq!(sa.search_str("p"), vec![8, 9]);
        assert_eq!(sa.search_str("mississippi"), vec![0]);
    }

    #[test]
    fn two_char_text() {
        let sa = SuffixArray::from_str("ab");
        assert_eq!(sa.suffix_array(), &[0, 1]);
        assert_eq!(sa.search_str("a"), vec![0]);
        assert_eq!(sa.search_str("b"), vec![1]);
        assert_eq!(sa.search_str("ab"), vec![0]);
    }

    #[test]
    fn repeated_pattern() {
        let sa = SuffixArray::from_str("xyzxyzxyz");
        assert_eq!(sa.search_str("xyz"), vec![0, 3, 6]);
        assert_eq!(sa.count_str("xyz"), 3);
    }

    #[test]
    fn binary_data_with_zero_bytes() {
        let data = vec![0u8, 0, 1, 0, 0, 1];
        let sa = SuffixArray::new(&data);
        let results = sa.search(&[0, 0, 1]);
        assert_eq!(results, vec![0, 3]);
    }

    #[test]
    fn suffixes_sorted_invariant_mississippi() {
        let sa = SuffixArray::from_str("mississippi");
        let arr = sa.suffix_array();
        let text = sa.text();
        for w in arr.windows(2) {
            assert!(
                &text[w[0]..] <= &text[w[1]..],
                "suffix at {} not <= suffix at {}",
                w[0],
                w[1]
            );
        }
    }
}
