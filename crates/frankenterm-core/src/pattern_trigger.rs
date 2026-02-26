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
//! let counts = scanner.scan_counts(b"ERROR: connection failed\nOK: build complete");
//! assert_eq!(counts.get(&TriggerCategory::Error), Some(&1));
//! assert_eq!(counts.get(&TriggerCategory::Completion), Some(&1));
//! ```

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =============================================================================
// Trigger categories
// =============================================================================

/// Category of a pattern trigger match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerCategory {
    /// Error indicators (ERROR, FAIL, panic, etc.).
    Error,
    /// Warning indicators (WARN, WARNING, deprecated, etc.).
    Warning,
    /// Completion markers (Done, Complete, Finished, OK, PASS, etc.).
    Completion,
    /// Progress indicators (%, Building, Compiling, Downloading, etc.).
    Progress,
    /// Test results (test result, passed, failed, ignored).
    TestResult,
    /// Custom user-defined category.
    Custom,
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

/// Aggregated scan results with per-category counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerScanResult {
    /// Per-category match counts.
    pub counts: HashMap<TriggerCategory, u64>,
    /// Total matches across all categories.
    pub total_matches: u64,
    /// Bytes scanned.
    pub bytes_scanned: u64,
}

impl TriggerScanResult {
    /// Get the count for a specific category.
    #[must_use]
    pub fn get(&self, category: &TriggerCategory) -> Option<&u64> {
        self.counts.get(category)
    }

    /// Whether any error triggers were found.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.counts.get(&TriggerCategory::Error).copied().unwrap_or(0) > 0
    }

    /// Whether any completion triggers were found.
    #[must_use]
    pub fn has_completions(&self) -> bool {
        self.counts
            .get(&TriggerCategory::Completion)
            .copied()
            .unwrap_or(0)
            > 0
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
    ci_indices: Vec<usize>,
    /// Indices of case-sensitive patterns (into `patterns`).
    cs_indices: Vec<usize>,
}

impl TriggerScanner {
    /// Build a scanner from a list of trigger patterns.
    ///
    /// The patterns are split into case-sensitive and case-insensitive groups,
    /// and separate Aho-Corasick automatons are built for each.
    #[must_use]
    pub fn new(patterns: Vec<TriggerPattern>) -> Self {
        let mut cs_patterns: Vec<String> = Vec::new();
        let mut ci_patterns: Vec<String> = Vec::new();
        let mut cs_indices: Vec<usize> = Vec::new();
        let mut ci_indices: Vec<usize> = Vec::new();

        for (i, p) in patterns.iter().enumerate() {
            if p.case_insensitive {
                ci_patterns.push(p.pattern.clone());
                ci_indices.push(i);
            } else {
                cs_patterns.push(p.pattern.clone());
                cs_indices.push(i);
            }
        }

        let automaton = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostFirst)
            .build(&cs_patterns)
            .unwrap_or_else(|_| AhoCorasick::new(Vec::<&str>::new()).unwrap());

        let automaton_ci = if ci_patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .match_kind(MatchKind::LeftmostFirst)
                    .ascii_case_insensitive(true)
                    .build(&ci_patterns)
                    .unwrap_or_else(|_| AhoCorasick::new(Vec::<&str>::new()).unwrap()),
            )
        };

        Self {
            automaton,
            automaton_ci,
            patterns,
            ci_indices,
            cs_indices,
        }
    }

    /// Scan and return per-category counts (fast path — no match locations).
    #[must_use]
    pub fn scan_counts(&self, input: &[u8]) -> TriggerScanResult {
        let mut result = TriggerScanResult {
            bytes_scanned: input.len() as u64,
            ..Default::default()
        };

        // Case-sensitive matches
        for mat in self.automaton.find_iter(input) {
            let pattern_idx = self.cs_indices[mat.pattern().as_usize()];
            let category = self.patterns[pattern_idx].category;
            *result.counts.entry(category).or_insert(0) += 1;
            result.total_matches += 1;
        }

        // Case-insensitive matches
        if let Some(ref aci) = self.automaton_ci {
            for mat in aci.find_iter(input) {
                let pattern_idx = self.ci_indices[mat.pattern().as_usize()];
                let category = self.patterns[pattern_idx].category;
                *result.counts.entry(category).or_insert(0) += 1;
                result.total_matches += 1;
            }
        }

        result
    }

    /// Scan and return detailed match locations.
    #[must_use]
    pub fn scan_locate(&self, input: &[u8]) -> Vec<TriggerMatch> {
        let mut matches = Vec::new();

        // Case-sensitive matches
        for mat in self.automaton.find_iter(input) {
            let pattern_idx = self.cs_indices[mat.pattern().as_usize()];
            matches.push(TriggerMatch {
                offset: mat.start(),
                length: mat.end() - mat.start(),
                pattern_index: pattern_idx,
                category: self.patterns[pattern_idx].category,
            });
        }

        // Case-insensitive matches
        if let Some(ref aci) = self.automaton_ci {
            for mat in aci.find_iter(input) {
                let pattern_idx = self.ci_indices[mat.pattern().as_usize()];
                matches.push(TriggerMatch {
                    offset: mat.start(),
                    length: mat.end() - mat.start(),
                    pattern_index: pattern_idx,
                    category: self.patterns[pattern_idx].category,
                });
            }
        }

        // Sort by offset for deterministic output
        matches.sort_by_key(|m| m.offset);
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
        let result = scanner.scan_counts(
            b"    Finished `dev` profile in 2.3s\ntest result: ok. 5 passed\n",
        );
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

    // -- Locate mode --

    #[test]
    fn locate_returns_offsets() {
        let scanner = TriggerScanner::default();
        let input = b"OK then ERROR here";
        let matches = scanner.scan_locate(input);
        let error_match = matches.iter().find(|m| m.category == TriggerCategory::Error);
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
        let input =
            b"   Compiling foo\nwarning: unused\nERROR: oops\n    Finished `dev` profile\n";
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

    // -- Large input --

    #[test]
    fn large_input_throughput() {
        let scanner = TriggerScanner::default();
        let mut input = Vec::with_capacity(1024 * 1024);
        for i in 0..10000 {
            input.extend_from_slice(
                format!("   Compiling crate-{i} v0.1.{}\n", i % 100).as_bytes(),
            );
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
}
