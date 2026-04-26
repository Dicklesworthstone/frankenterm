//! High-throughput pattern trigger scanning for pane output (ft-2oph2).
//!
//! Uses Aho-Corasick automaton for vectorized multi-pattern matching across
//! terminal output. Detects error keywords, completion markers, progress
//! indicators, and custom triggers in a single pass through the data.
//!
//! # Architecture
//!
//! The scanner operates in two modes:
//!
//! 1. **Count mode**: Returns the number of matches per trigger category.
//!    Useful for telemetry and backpressure decisions.
//! 2. **Locate mode**: Returns byte offsets of each match.
//!    Useful for highlighting and alert generation.
//!
//! # Usage
//!
//! ```ignore
//! use frankenterm_core::pattern_trigger::{TriggerScanner, TriggerCategory};
//!
//! let scanner = TriggerScanner::default();
//! let result = scanner.scan_counts(b"ERROR: connection failed\nOK: build complete");
//! assert_eq!(result.counts.count(TriggerCategory::Error), 1);
//! assert_eq!(result.counts.count(TriggerCategory::Completion), 1);
//! ```

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// =============================================================================
// Trigger categories
// =============================================================================

/// Number of `TriggerCategory` variants — used to size [`TriggerCategoryCounts`]'s
/// fixed-cardinality storage (ft-6db1t). Adding a variant requires bumping
/// this constant + extending the index/all() helpers below; the unit test
/// `trigger_category_count_matches_all_helper` pins them in lockstep.
pub const TRIGGER_CATEGORY_COUNT: usize = 6;

/// Category of a pattern trigger match.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerCategory {
    /// Error indicators (ERROR, FAIL, panic, etc.).
    Error = 0,
    /// Warning indicators (WARN, WARNING, deprecated, etc.).
    Warning = 1,
    /// Completion markers (Done, Complete, Finished, OK, PASS, etc.).
    Completion = 2,
    /// Progress indicators (%, Building, Compiling, Downloading, etc.).
    Progress = 3,
    /// Test results (test result, passed, failed, ignored).
    TestResult = 4,
    /// Custom user-defined category.
    Custom = 5,
}

impl TriggerCategory {
    /// Stable index into the [`TriggerCategoryCounts`] array. Matches the
    /// `#[repr(u8)]` discriminant; do not change without updating the
    /// `from_index` round-trip and `all()` ordering.
    #[inline]
    #[must_use]
    pub const fn as_index(self) -> usize {
        self as usize
    }

    /// Inverse of [`Self::as_index`].
    #[inline]
    #[must_use]
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Error),
            1 => Some(Self::Warning),
            2 => Some(Self::Completion),
            3 => Some(Self::Progress),
            4 => Some(Self::TestResult),
            5 => Some(Self::Custom),
            _ => None,
        }
    }

    /// All variants in discriminant order — same order as their slot in
    /// [`TriggerCategoryCounts`]. Useful for emitting a stable iteration
    /// of (category, count) pairs.
    #[inline]
    #[must_use]
    pub const fn all() -> [Self; TRIGGER_CATEGORY_COUNT] {
        [
            Self::Error,
            Self::Warning,
            Self::Completion,
            Self::Progress,
            Self::TestResult,
            Self::Custom,
        ]
    }
}

impl std::fmt::Display for TriggerCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error => write!(f, "error"),
            Self::Warning => write!(f, "warning"),
            Self::Completion => write!(f, "completion"),
            Self::Progress => write!(f, "progress"),
            Self::TestResult => write!(f, "test_result"),
            Self::Custom => write!(f, "custom"),
        }
    }
}

// =============================================================================
// Pattern definition
// =============================================================================

/// A single pattern with its category and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerPattern {
    /// The byte pattern to search for.
    pub pattern: String,
    /// Category this pattern belongs to.
    pub category: TriggerCategory,
    /// Whether the match must be case-insensitive.
    pub case_insensitive: bool,
}

impl TriggerPattern {
    /// Create a case-sensitive trigger pattern.
    #[must_use]
    pub fn new(pattern: &str, category: TriggerCategory) -> Self {
        Self {
            pattern: pattern.to_string(),
            category,
            case_insensitive: false,
        }
    }

    /// Create a case-insensitive trigger pattern.
    #[must_use]
    pub fn case_insensitive(pattern: &str, category: TriggerCategory) -> Self {
        Self {
            pattern: pattern.to_string(),
            category,
            case_insensitive: true,
        }
    }
}

// =============================================================================
// Scan results
// =============================================================================

/// A single match found in the input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerMatch {
    /// Byte offset of the match start in the input.
    pub offset: usize,
    /// Length of the match in bytes.
    pub length: usize,
    /// Index of the pattern that matched (into the scanner's pattern list).
    pub pattern_index: usize,
    /// Category of the matched pattern.
    pub category: TriggerCategory,
}

/// Per-category match counts backed by a fixed-size array indexed by
/// [`TriggerCategory::as_index`] (ft-6db1t). Replaces the previous
/// `HashMap<TriggerCategory, u64>`: per-match increments become a single
/// indexed array store with no hashing, no bucket walk, and no heap
/// allocation. The wire format (Serialize/Deserialize) stays a JSON map
/// keyed by the snake_case category name so external replay/dashboard
/// consumers see the same shape.
///
/// `get(&category)` keeps the HashMap-equivalent semantics: `Some(&n)`
/// when the count is non-zero, `None` otherwise — preserving call sites
/// that pattern-match on absence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TriggerCategoryCounts([u64; TRIGGER_CATEGORY_COUNT]);

impl TriggerCategoryCounts {
    /// All-zero counts.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self([0; TRIGGER_CATEGORY_COUNT])
    }

    /// HashMap-equivalent lookup: `Some(&count)` if non-zero, else `None`.
    #[inline]
    #[must_use]
    pub fn get(&self, category: &TriggerCategory) -> Option<&u64> {
        let v = &self.0[category.as_index()];
        if *v == 0 { None } else { Some(v) }
    }

    /// Direct count for a category (always returns a value; 0 for unobserved).
    #[inline]
    #[must_use]
    pub fn count(&self, category: TriggerCategory) -> u64 {
        self.0[category.as_index()]
    }

    /// Saturating-add `delta` to `category`'s slot. Hot-path equivalent of
    /// `*map.entry(c).or_insert(0) += delta` without the HashMap traffic.
    #[inline]
    pub fn add(&mut self, category: TriggerCategory, delta: u64) {
        let slot = &mut self.0[category.as_index()];
        *slot = slot.saturating_add(delta);
    }

    /// Reset every category's count to zero.
    #[inline]
    pub fn clear(&mut self) {
        self.0 = [0; TRIGGER_CATEGORY_COUNT];
    }

    /// Iterate (category, count) pairs in [`TriggerCategory::all`] order,
    /// including zero entries.
    pub fn iter(&self) -> impl Iterator<Item = (TriggerCategory, u64)> + '_ {
        TriggerCategory::all()
            .into_iter()
            .map(move |c| (c, self.0[c.as_index()]))
    }

    /// Like [`Self::iter`] but skips zero counts. Matches the historical
    /// HashMap iteration shape that callers may rely on.
    pub fn iter_nonzero(&self) -> impl Iterator<Item = (TriggerCategory, u64)> + '_ {
        self.iter().filter(|(_, v)| *v > 0)
    }

    /// Iterator over all six counts as `&u64`, in [`TriggerCategory::all`]
    /// order. Mirrors `HashMap::values()` for compatibility with existing
    /// `.values().sum()` style code.
    pub fn values(&self) -> impl Iterator<Item = &u64> + '_ {
        self.0.iter()
    }

    /// Whether every category's count is zero.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|n| *n == 0)
    }

    /// Number of categories with a non-zero count. Mirrors
    /// `HashMap::len()` semantics for legacy callers; not a hot-path call.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.iter().filter(|n| **n > 0).count()
    }
}

impl Serialize for TriggerCategoryCounts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Match the legacy HashMap wire shape: a JSON map keyed by
        // snake_case category name, only including non-zero entries.
        // Keeps replay/dashboard consumers binary-compatible.
        let nonzero = self.0.iter().filter(|n| **n > 0).count();
        let mut map = serializer.serialize_map(Some(nonzero))?;
        for cat in TriggerCategory::all() {
            let v = self.0[cat.as_index()];
            if v > 0 {
                map.serialize_entry(&cat, &v)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TriggerCategoryCounts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CountsVisitor;
        impl<'de> Visitor<'de> for CountsVisitor {
            type Value = TriggerCategoryCounts;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a map of TriggerCategory keys to u64 counts")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut out = TriggerCategoryCounts::new();
                while let Some((cat, count)) = access.next_entry::<TriggerCategory, u64>()? {
                    out.0[cat.as_index()] = count;
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(CountsVisitor)
    }
}

/// Aggregated scan results with per-category counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerScanResult {
    /// Per-category match counts.
    pub counts: TriggerCategoryCounts,
    /// Total matches across all categories.
    pub total_matches: u64,
    /// Bytes scanned.
    pub bytes_scanned: u64,
}

impl TriggerScanResult {
    /// Get the count for a specific category — `Some(&n)` when non-zero,
    /// `None` otherwise (preserves the legacy HashMap-equivalent semantic).
    #[must_use]
    pub fn get(&self, category: &TriggerCategory) -> Option<&u64> {
        self.counts.get(category)
    }

    /// Whether any error triggers were found.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.counts.count(TriggerCategory::Error) > 0
    }

    /// Whether any completion triggers were found.
    #[must_use]
    pub fn has_completions(&self) -> bool {
        self.counts.count(TriggerCategory::Completion) > 0
    }
}

// =============================================================================
// Default pattern sets
// =============================================================================

/// Returns the built-in error patterns.
#[must_use]
pub fn default_error_patterns() -> Vec<TriggerPattern> {
    vec![
        TriggerPattern::new("ERROR", TriggerCategory::Error),
        TriggerPattern::new("FATAL", TriggerCategory::Error),
        TriggerPattern::new("FAILED", TriggerCategory::Error),
        TriggerPattern::case_insensitive("panic", TriggerCategory::Error),
        TriggerPattern::case_insensitive("segfault", TriggerCategory::Error),
        TriggerPattern::new("error[E", TriggerCategory::Error),
        TriggerPattern::new("error:", TriggerCategory::Error),
        TriggerPattern::new("SIGSEGV", TriggerCategory::Error),
        TriggerPattern::new("SIGABRT", TriggerCategory::Error),
        TriggerPattern::new("Traceback (most recent call last)", TriggerCategory::Error),
    ]
}

/// Returns the built-in warning patterns.
#[must_use]
pub fn default_warning_patterns() -> Vec<TriggerPattern> {
    vec![
        TriggerPattern::new("WARNING", TriggerCategory::Warning),
        TriggerPattern::new("WARN", TriggerCategory::Warning),
        TriggerPattern::new("warning:", TriggerCategory::Warning),
        TriggerPattern::case_insensitive("deprecated", TriggerCategory::Warning),
    ]
}

/// Returns the built-in completion patterns.
#[must_use]
pub fn default_completion_patterns() -> Vec<TriggerPattern> {
    vec![
        TriggerPattern::new("Finished", TriggerCategory::Completion),
        TriggerPattern::new("Complete", TriggerCategory::Completion),
        TriggerPattern::new("Done", TriggerCategory::Completion),
        TriggerPattern::new("PASSED", TriggerCategory::Completion),
        TriggerPattern::new("test result: ok", TriggerCategory::Completion),
    ]
}

/// Returns the built-in progress patterns.
#[must_use]
pub fn default_progress_patterns() -> Vec<TriggerPattern> {
    vec![
        TriggerPattern::new("Compiling", TriggerCategory::Progress),
        TriggerPattern::new("Downloading", TriggerCategory::Progress),
        TriggerPattern::new("Building", TriggerCategory::Progress),
        TriggerPattern::new("Installing", TriggerCategory::Progress),
        TriggerPattern::new("Resolving", TriggerCategory::Progress),
    ]
}

/// Returns the built-in test result patterns.
#[must_use]
pub fn default_test_result_patterns() -> Vec<TriggerPattern> {
    vec![
        TriggerPattern::new("test result:", TriggerCategory::TestResult),
        TriggerPattern::new("tests passed", TriggerCategory::TestResult),
        TriggerPattern::new("tests failed", TriggerCategory::TestResult),
        TriggerPattern::new("... ok", TriggerCategory::TestResult),
        TriggerPattern::new("... FAILED", TriggerCategory::TestResult),
    ]
}

/// Returns all built-in patterns combined.
#[must_use]
pub fn all_default_patterns() -> Vec<TriggerPattern> {
    let mut patterns = Vec::new();
    patterns.extend(default_error_patterns());
    patterns.extend(default_warning_patterns());
    patterns.extend(default_completion_patterns());
    patterns.extend(default_progress_patterns());
    patterns.extend(default_test_result_patterns());
    patterns
}

// =============================================================================
// Scanner
// =============================================================================

/// High-throughput multi-pattern scanner using Aho-Corasick automaton.
///
/// The automaton is built once from a set of trigger patterns and can then
/// scan arbitrary byte streams in a single pass with no backtracking.
pub struct TriggerScanner {
    /// Compiled Aho-Corasick automaton (case-sensitive patterns).
    automaton: AhoCorasick,
    /// Compiled automaton for case-insensitive patterns.
    automaton_ci: Option<AhoCorasick>,
    /// Pattern metadata (category, etc.) indexed by pattern ID.
    patterns: Vec<TriggerPattern>,
    /// Indices of case-insensitive patterns (into `patterns`).
    nocase_indices: Vec<usize>,
    /// Indices of case-sensitive patterns (into `patterns`).
    exact_indices: Vec<usize>,
}

fn build_trigger_automaton(patterns: &[String], case_insensitive: bool) -> Option<AhoCorasick> {
    if patterns.is_empty() {
        return None;
    }

    let mut builder = AhoCorasickBuilder::new();
    builder.match_kind(MatchKind::LeftmostFirst);
    if case_insensitive {
        builder.ascii_case_insensitive(true);
    }

    Some(builder.build(patterns).unwrap_or_else(|err| {
        let mode = if case_insensitive {
            "case-insensitive"
        } else {
            "case-sensitive"
        };
        let indexed_patterns = patterns
            .iter()
            .enumerate()
            .map(|(idx, pattern)| format!("{idx}:{pattern:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Intentional fail-fast: built-in trigger definitions are static config, so compile drift must abort instead of silently disabling scan coverage.
        panic!("non-empty {mode} trigger pattern sets must compile: {err}; indexed_patterns=[{indexed_patterns}]");
    }))
}

impl TriggerScanner {
    /// Build a scanner from a list of trigger patterns.
    ///
    /// The patterns are split into case-sensitive and case-insensitive groups,
    /// and separate Aho-Corasick automatons are built for each.
    #[must_use]
    pub fn new(patterns: Vec<TriggerPattern>) -> Self {
        let mut exact_patterns: Vec<String> = Vec::new();
        let mut nocase_patterns: Vec<String> = Vec::new();
        let mut exact_indices: Vec<usize> = Vec::new();
        let mut nocase_indices: Vec<usize> = Vec::new();

        for (i, p) in patterns.iter().enumerate() {
            if p.case_insensitive {
                nocase_patterns.push(p.pattern.clone());
                nocase_indices.push(i);
            } else {
                exact_patterns.push(p.pattern.clone());
                exact_indices.push(i);
            }
        }

        let automaton = build_trigger_automaton(&exact_patterns, false)
            // Intentional fail-fast: an empty sentinel automaton should be infallible; failure would mean an upstream aho-corasick contract break.
            .unwrap_or_else(|| AhoCorasick::new(Vec::<&str>::new()).expect("empty automaton"));
        let automaton_ci = build_trigger_automaton(&nocase_patterns, true);

        Self {
            automaton,
            automaton_ci,
            patterns,
            nocase_indices,
            exact_indices,
        }
    }

    fn exact_trigger_match(&self, mat: aho_corasick::Match) -> TriggerMatch {
        let pattern_idx = self.exact_indices[mat.pattern().as_usize()];
        TriggerMatch {
            offset: mat.start(),
            length: mat.end() - mat.start(),
            pattern_index: pattern_idx,
            category: self.patterns[pattern_idx].category,
        }
    }

    fn nocase_trigger_match(&self, mat: aho_corasick::Match) -> TriggerMatch {
        let pattern_idx = self.nocase_indices[mat.pattern().as_usize()];
        TriggerMatch {
            offset: mat.start(),
            length: mat.end() - mat.start(),
            pattern_index: pattern_idx,
            category: self.patterns[pattern_idx].category,
        }
    }

    /// Stream matches via callback, never allocating a result Vec.
    ///
    /// Visibility raised to `pub(crate)` for ft-iwlsh — `scan_pipeline.rs`
    /// previously called the allocating wrapper [`Self::scan_locate`] and
    /// then iterated and discarded the Vec on every chunk; the callback
    /// path skips the allocation entirely on the production hot path.
    pub(crate) fn for_each_leftmost_match<F>(&self, input: &[u8], mut on_match: F)
    where
        F: FnMut(TriggerMatch),
    {
        let mut exact_iter = self
            .automaton
            .find_iter(input)
            .map(|mat| self.exact_trigger_match(mat))
            .peekable();
        let mut nocase_iter = self.automaton_ci.as_ref().map(|automaton| {
            automaton
                .find_iter(input)
                .map(|mat| self.nocase_trigger_match(mat))
                .peekable()
        });
        let mut next_allowed_offset = 0usize;

        loop {
            while let Some(matched) = exact_iter.peek() {
                if matched.offset < next_allowed_offset {
                    exact_iter.next();
                } else {
                    break;
                }
            }

            if let Some(iter) = nocase_iter.as_mut() {
                while let Some(matched) = iter.peek() {
                    if matched.offset < next_allowed_offset {
                        iter.next();
                    } else {
                        break;
                    }
                }
            }

            let next = match (
                exact_iter.peek().cloned(),
                nocase_iter.as_mut().and_then(|iter| iter.peek().cloned()),
            ) {
                (Some(exact), Some(nocase))
                    if exact.offset < nocase.offset
                        || (exact.offset == nocase.offset
                            && exact.pattern_index <= nocase.pattern_index) =>
                {
                    exact_iter.next()
                }
                (Some(_), Some(_)) => nocase_iter.as_mut().and_then(|iter| iter.next()),
                (Some(_), None) => exact_iter.next(),
                (None, Some(_)) => nocase_iter.as_mut().and_then(|iter| iter.next()),
                (None, None) => None,
            };

            let Some(matched) = next else {
                break;
            };

            next_allowed_offset = matched.offset.saturating_add(matched.length);
            on_match(matched);
        }
    }

    /// Scan and return per-category counts (fast path — no match locations).
    #[must_use]
    pub fn scan_counts(&self, input: &[u8]) -> TriggerScanResult {
        let mut result = TriggerScanResult {
            bytes_scanned: input.len() as u64,
            ..Default::default()
        };
        self.for_each_leftmost_match(input, |matched| {
            // ft-6db1t: array-indexed increment, no hashing / allocation.
            result.counts.add(matched.category, 1);
            result.total_matches += 1;
        });

        result
    }

    /// Scan and return detailed match locations.
    #[must_use]
    pub fn scan_locate(&self, input: &[u8]) -> Vec<TriggerMatch> {
        let mut matches = Vec::new();
        self.for_each_leftmost_match(input, |matched| matches.push(matched));
        matches
    }

    /// Number of patterns in this scanner.
    #[must_use]
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    /// Access the pattern definitions.
    #[must_use]
    pub fn patterns(&self) -> &[TriggerPattern] {
        &self.patterns
    }
}

impl Default for TriggerScanner {
    fn default() -> Self {
        Self::new(all_default_patterns())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Default scanner --

    #[test]
    fn default_scanner_detects_errors() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"ERROR: connection refused\n");
        assert_eq!(result.get(&TriggerCategory::Error), Some(&1));
        assert!(result.has_errors());
    }

    #[test]
    fn default_scanner_detects_warnings() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"warning: unused variable\nWARNING: disk full\n");
        assert_eq!(result.get(&TriggerCategory::Warning), Some(&2));
    }

    #[test]
    fn default_scanner_detects_completion() {
        let scanner = TriggerScanner::default();
        let result =
            scanner.scan_counts(b"    Finished `dev` profile in 2.3s\ntest result: ok. 5 passed\n");
        assert!(result.has_completions());
    }

    #[test]
    fn default_scanner_detects_progress() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"   Compiling foo v0.1.0\n   Downloading bar v1.0\n");
        assert_eq!(result.get(&TriggerCategory::Progress), Some(&2));
    }

    #[test]
    fn default_scanner_detects_test_results() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"test foo ... ok\ntest bar ... FAILED\n");
        assert_eq!(result.get(&TriggerCategory::TestResult), Some(&2));
    }

    // -- Case insensitive --

    #[test]
    fn case_insensitive_panic_detection() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"thread 'main' panicked at 'assertion failed'\n");
        assert!(result.has_errors());
    }

    #[test]
    fn case_insensitive_deprecated() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"This API is Deprecated since v2.0\n");
        assert_eq!(result.get(&TriggerCategory::Warning), Some(&1));
    }

    #[test]
    fn case_insensitive_only_patterns_build_and_match() {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::case_insensitive("panic", TriggerCategory::Error),
            TriggerPattern::case_insensitive("deprecated", TriggerCategory::Warning),
        ]);
        let result = scanner.scan_counts(b"Deprecated API\npanic in worker\n");
        assert_eq!(result.get(&TriggerCategory::Warning), Some(&1));
        assert_eq!(result.get(&TriggerCategory::Error), Some(&1));
    }

    // -- Locate mode --

    #[test]
    fn locate_returns_offsets() {
        let scanner = TriggerScanner::default();
        let input = b"OK then ERROR here";
        let matches = scanner.scan_locate(input);
        let error_match = matches
            .iter()
            .find(|m| m.category == TriggerCategory::Error);
        assert!(error_match.is_some());
        let em = error_match.unwrap();
        assert_eq!(&input[em.offset..em.offset + em.length], b"ERROR");
    }

    #[test]
    fn locate_sorted_by_offset() {
        let scanner = TriggerScanner::default();
        let input = b"Building foo\nERROR: failed\nFinished bar\n";
        let matches = scanner.scan_locate(input);
        for w in matches.windows(2) {
            assert!(w[0].offset <= w[1].offset);
        }
    }

    // -- Empty and no-match inputs --

    #[test]
    fn empty_input_no_matches() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"");
        assert_eq!(result.total_matches, 0);
        assert_eq!(result.bytes_scanned, 0);
    }

    #[test]
    fn no_triggers_in_plain_text() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"just some ordinary text with nothing special\n");
        assert_eq!(result.total_matches, 0);
    }

    // -- Multiple categories in one scan --

    #[test]
    fn multiple_categories_detected() {
        let scanner = TriggerScanner::default();
        let input = b"   Compiling foo\nwarning: unused\nERROR: oops\n    Finished `dev` profile\n";
        let result = scanner.scan_counts(input);
        assert!(result.get(&TriggerCategory::Progress).unwrap_or(&0) > &0);
        assert!(result.get(&TriggerCategory::Warning).unwrap_or(&0) > &0);
        assert!(result.get(&TriggerCategory::Error).unwrap_or(&0) > &0);
        assert!(result.get(&TriggerCategory::Completion).unwrap_or(&0) > &0);
    }

    // -- Custom patterns --

    #[test]
    fn custom_patterns() {
        let patterns = vec![
            TriggerPattern::new("DEPLOY", TriggerCategory::Custom),
            TriggerPattern::new("ROLLBACK", TriggerCategory::Custom),
        ];
        let scanner = TriggerScanner::new(patterns);
        let result = scanner.scan_counts(b"Starting DEPLOY to prod\nDEPLOY complete\n");
        assert_eq!(result.get(&TriggerCategory::Custom), Some(&2));
    }

    #[test]
    fn empty_patterns() {
        let scanner = TriggerScanner::new(Vec::new());
        let result = scanner.scan_counts(b"ERROR: this should not match\n");
        assert_eq!(result.total_matches, 0);
    }

    #[test]
    fn overlapping_exact_and_case_insensitive_patterns_follow_leftmost_first_once() {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new("ERROR", TriggerCategory::Custom),
            TriggerPattern::case_insensitive("error", TriggerCategory::Error),
        ]);

        let counts = scanner.scan_counts(b"ERROR: duplicated?");
        assert_eq!(counts.total_matches, 1);
        assert_eq!(counts.get(&TriggerCategory::Custom), Some(&1));
        assert_eq!(counts.get(&TriggerCategory::Error), None);

        let located = scanner.scan_locate(b"ERROR: duplicated?");
        assert_eq!(located.len(), 1);
        assert_eq!(located[0].offset, 0);
        assert_eq!(located[0].length, 5);
        assert_eq!(located[0].pattern_index, 0);
        assert_eq!(located[0].category, TriggerCategory::Custom);
    }

    #[test]
    fn overlapping_mixed_case_patterns_prefer_earlier_pattern_order() {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::case_insensitive("error", TriggerCategory::Error),
            TriggerPattern::new("ERROR:", TriggerCategory::Warning),
        ]);

        let located = scanner.scan_locate(b"ERROR: still one logical match");
        assert_eq!(located.len(), 1);
        assert_eq!(located[0].offset, 0);
        assert_eq!(located[0].length, 5);
        assert_eq!(located[0].pattern_index, 0);
        assert_eq!(located[0].category, TriggerCategory::Error);
    }

    // -- Large input --

    #[test]
    fn large_input_throughput() {
        let scanner = TriggerScanner::default();
        let mut input = Vec::with_capacity(1024 * 1024);
        for i in 0..10000 {
            input
                .extend_from_slice(format!("   Compiling crate-{i} v0.1.{}\n", i % 100).as_bytes());
        }
        // Inject a few errors
        input.extend_from_slice(b"ERROR: build failed\n");
        input.extend_from_slice(b"FATAL: out of memory\n");

        let result = scanner.scan_counts(&input);
        assert_eq!(result.get(&TriggerCategory::Progress), Some(&10000));
        assert_eq!(result.get(&TriggerCategory::Error), Some(&2));
    }

    // -- Rust error code pattern --

    #[test]
    fn rust_error_code_detection() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"error[E0433]: failed to resolve\n");
        assert!(result.has_errors());
    }

    // -- Python traceback --

    #[test]
    fn python_traceback_detection() {
        let scanner = TriggerScanner::default();
        let input = b"Traceback (most recent call last):\n  File \"test.py\", line 1\n";
        let result = scanner.scan_counts(input);
        assert!(result.has_errors());
    }

    // -- Serde roundtrip --

    #[test]
    fn trigger_category_serde_roundtrip() {
        let cat = TriggerCategory::Error;
        let json = serde_json::to_string(&cat).unwrap();
        assert_eq!(json, "\"error\"");
        let rt: TriggerCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, cat);
    }

    #[test]
    fn trigger_scan_result_serde_roundtrip() {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(b"ERROR: test\nDone.\n");
        let json = serde_json::to_string(&result).unwrap();
        let rt: TriggerScanResult = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.total_matches, result.total_matches);
        assert_eq!(
            rt.get(&TriggerCategory::Error),
            result.get(&TriggerCategory::Error)
        );
    }

    // -- Pattern count --

    #[test]
    fn default_pattern_count() {
        let scanner = TriggerScanner::default();
        let all = all_default_patterns();
        assert_eq!(scanner.pattern_count(), all.len());
    }

    #[test]
    fn trigger_category_display_all_variants() {
        assert_eq!(TriggerCategory::Error.to_string(), "error");
        assert_eq!(TriggerCategory::Warning.to_string(), "warning");
        assert_eq!(TriggerCategory::Completion.to_string(), "completion");
        assert_eq!(TriggerCategory::Progress.to_string(), "progress");
        assert_eq!(TriggerCategory::TestResult.to_string(), "test_result");
        assert_eq!(TriggerCategory::Custom.to_string(), "custom");
    }

    // ─── ft-6db1t: TriggerCategoryCounts array-backed contract ────────────

    #[test]
    fn trigger_category_count_matches_all_helper() {
        // Pin the cardinality constant against the all() helper so adding a
        // variant without bumping TRIGGER_CATEGORY_COUNT trips this test.
        assert_eq!(TriggerCategory::all().len(), TRIGGER_CATEGORY_COUNT);
    }

    #[test]
    fn trigger_category_index_round_trip_covers_every_variant() {
        for cat in TriggerCategory::all() {
            let i = cat.as_index();
            assert!(i < TRIGGER_CATEGORY_COUNT);
            assert_eq!(TriggerCategory::from_index(i), Some(cat));
        }
        assert_eq!(TriggerCategory::from_index(TRIGGER_CATEGORY_COUNT), None);
    }

    #[test]
    fn trigger_category_counts_get_returns_none_for_zero() {
        // Preserves the legacy HashMap-equivalent semantic: missing key /
        // zero count both surface as None so external pattern-matching
        // call sites keep working.
        let mut c = TriggerCategoryCounts::new();
        assert_eq!(c.get(&TriggerCategory::Error), None);
        c.add(TriggerCategory::Error, 3);
        assert_eq!(c.get(&TriggerCategory::Error), Some(&3));
        assert_eq!(c.count(TriggerCategory::Error), 3);
        assert_eq!(c.count(TriggerCategory::Warning), 0);
    }

    #[test]
    fn trigger_category_counts_add_saturates() {
        let mut c = TriggerCategoryCounts::new();
        c.add(TriggerCategory::Error, u64::MAX);
        c.add(TriggerCategory::Error, 5);
        assert_eq!(c.count(TriggerCategory::Error), u64::MAX);
    }

    #[test]
    fn trigger_category_counts_serde_wire_format_is_a_map() {
        // ft-6db1t: in-memory storage is [u64; 6] but the wire format must
        // stay a JSON map keyed by snake_case category name. Replay /
        // dashboard consumers that read the field directly continue to
        // see the same shape they did pre-fix.
        let mut c = TriggerCategoryCounts::new();
        c.add(TriggerCategory::Error, 5);
        c.add(TriggerCategory::Completion, 2);
        let json = serde_json::to_string(&c).unwrap();
        // Order is stable (TriggerCategory::all() discriminant order); zero
        // entries are omitted to match the historical HashMap shape.
        assert!(json.contains("\"error\":5"));
        assert!(json.contains("\"completion\":2"));
        assert!(!json.contains("warning"));
        assert!(!json.contains("progress"));

        let rt: TriggerCategoryCounts = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, c);
    }

    #[test]
    fn trigger_category_counts_deserialize_from_legacy_hashmap_json() {
        // Old captures (replay logs, dashboard payloads) emitted via the
        // pre-fix HashMap. The new array-backed type must still parse them
        // with the same semantics.
        let legacy = r#"{"error": 7, "test_result": 3}"#;
        let c: TriggerCategoryCounts = serde_json::from_str(legacy).unwrap();
        assert_eq!(c.count(TriggerCategory::Error), 7);
        assert_eq!(c.count(TriggerCategory::TestResult), 3);
        assert_eq!(c.count(TriggerCategory::Warning), 0);
        assert_eq!(c.get(&TriggerCategory::Error), Some(&7));
        assert_eq!(c.get(&TriggerCategory::Warning), None);
    }

    #[test]
    fn trigger_category_counts_iter_emits_stable_order() {
        let mut c = TriggerCategoryCounts::new();
        c.add(TriggerCategory::Custom, 1);
        c.add(TriggerCategory::Error, 4);
        let pairs: Vec<_> = c.iter_nonzero().collect();
        // Must be in TriggerCategory::all() order (Error before Custom)
        // regardless of insertion order.
        assert_eq!(
            pairs,
            vec![(TriggerCategory::Error, 4), (TriggerCategory::Custom, 1)]
        );
    }
}
