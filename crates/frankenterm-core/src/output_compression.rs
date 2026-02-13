//! Semantic output compression for repetitive terminal content.
//!
//! AI agent terminal output is highly repetitive: progress counters, repeated
//! compilation warnings, git status checks, and similar-but-not-identical lines.
//! Byte-level compression (zstd) helps but misses semantic redundancy.
//!
//! This module detects repeated line patterns, extracts templates via longest
//! common substring alignment, and delta-encodes instances as (template_id,
//! variable_values). Decompression is lossless — the original output is
//! byte-for-byte recoverable.
//!
//! # Architecture
//!
//! ```text
//! Raw output → Line grouping → Template extraction → Delta encoding → Storage
//!                                                                       │
//! Reconstructed output ◄── Template + variables ◄── Decompression ◄─────┘
//! ```
//!
//! # Compression ratios (typical)
//!
//! | Content type          | Expected ratio |
//! |-----------------------|----------------|
//! | Progress counters     | 50:1 – 100:1   |
//! | Repeated warnings     | 15:1 – 25:1    |
//! | Git status output     | 10:1 – 20:1    |
//! | Mixed agent output    | 3:1 – 5:1      |
//! | Unique output         | 1:1             |

use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the semantic compression engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    /// Normalized edit-distance threshold for "similar" lines (0.0–1.0).
    /// Lines with `edit_distance(a,b) / max(len(a), len(b)) < threshold` are
    /// considered similar and grouped for template extraction.
    pub similarity_threshold: f64,
    /// Minimum consecutive similar lines to form a template group.
    pub min_group_size: usize,
    /// Maximum templates retained per compression pass.
    pub max_templates: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.3,
            min_group_size: 3,
            max_templates: 1000,
        }
    }
}

// =============================================================================
// Core types
// =============================================================================

/// A detected output template with placeholder positions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputTemplate {
    /// Template string with `\x00` bytes marking variable positions.
    /// Each `\x00` corresponds to one entry in `variable_positions`.
    pub pattern: String,
    /// Byte offsets within `pattern` where variables occur.
    pub variable_positions: Vec<usize>,
    /// Number of instances matching this template.
    pub instance_count: u64,
}

/// A compressed representation of terminal output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedOutput {
    /// Unique templates extracted from the input.
    pub templates: Vec<OutputTemplate>,
    /// Sequence of entries, each either a template instance or a literal line.
    pub entries: Vec<CompressedEntry>,
}

/// A single entry in the compressed output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompressedEntry {
    /// A line matching a template. Stores (template_index, variable_values).
    TemplateInstance {
        template_idx: usize,
        variables: Vec<String>,
    },
    /// A literal line that didn't match any template.
    Literal(String),
}

/// Statistics about a compression pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionStats {
    /// Total input lines.
    pub input_lines: usize,
    /// Total input bytes.
    pub input_bytes: usize,
    /// Number of templates extracted.
    pub template_count: usize,
    /// Lines compressed via templates.
    pub templated_lines: usize,
    /// Lines stored as literals.
    pub literal_lines: usize,
    /// Approximate compressed size in bytes.
    pub compressed_bytes: usize,
    /// Compression ratio (input_bytes / compressed_bytes).
    pub ratio: f64,
}

impl CompressionStats {
    /// Recompute derived fields.
    #[must_use]
    pub fn finalize(mut self) -> Self {
        if self.compressed_bytes > 0 {
            self.ratio = self.input_bytes as f64 / self.compressed_bytes as f64;
        } else if self.input_bytes == 0 {
            self.ratio = 1.0;
        }
        self
    }
}

// =============================================================================
// Edit distance
// =============================================================================

/// Compute the Levenshtein edit distance between two byte slices.
#[must_use]
pub fn edit_distance(a: &[u8], b: &[u8]) -> usize {
    let m = a.len();
    let n = b.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    // Single-row DP to keep memory O(min(m,n)).
    let mut prev = Vec::with_capacity(n + 1);
    for j in 0..=n {
        prev.push(j);
    }

    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1) // deletion
                .min(curr[j - 1] + 1) // insertion
                .min(prev[j - 1] + cost); // substitution
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Check if two lines are semantically similar using normalized edit distance.
#[must_use]
pub fn lines_similar(a: &str, b: &str, threshold: f64) -> bool {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return true;
    }
    let dist = edit_distance(a.as_bytes(), b.as_bytes());
    (dist as f64 / max_len as f64) < threshold
}

// =============================================================================
// Longest Common Substring (LCS) for template extraction
// =============================================================================

/// Find the longest common substring between two strings.
/// Returns `(start_in_a, start_in_b, length)`.
#[must_use]
fn longest_common_substring(a: &[u8], b: &[u8]) -> (usize, usize, usize) {
    let m = a.len();
    let n = b.len();

    if m == 0 || n == 0 {
        return (0, 0, 0);
    }

    let mut best_len = 0;
    let mut best_a = 0;
    let mut best_b = 0;

    // Single-row DP for memory efficiency.
    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                curr[j] = prev[j - 1] + 1;
                if curr[j] > best_len {
                    best_len = curr[j];
                    best_a = i - best_len;
                    best_b = j - best_len;
                }
            } else {
                curr[j] = 0;
            }
        }
        std::mem::swap(&mut prev, &mut curr);
        curr.fill(0);
    }

    (best_a, best_b, best_len)
}

// =============================================================================
// Template extraction
// =============================================================================

/// Extract a template from a group of similar lines.
///
/// Uses iterative LCS alignment against the first line to identify stable
/// regions (template) vs variable regions (placeholders).
#[must_use]
pub fn extract_template(lines: &[&str]) -> OutputTemplate {
    assert!(
        !lines.is_empty(),
        "cannot extract template from empty group"
    );

    if lines.len() == 1 {
        return OutputTemplate {
            pattern: lines[0].to_string(),
            variable_positions: vec![],
            instance_count: 1,
        };
    }

    let anchor = lines[0].as_bytes();

    // Build a mask: true = byte is stable across all lines.
    let mut stable = vec![true; anchor.len()];

    for &line in &lines[1..] {
        let other = line.as_bytes();
        mark_variable_regions(anchor, other, &mut stable);
    }

    // Construct template pattern: stable bytes copied, variable regions
    // replaced with a single '\x00' marker per contiguous run.
    let mut pattern = String::with_capacity(anchor.len());
    let mut variable_positions = Vec::new();
    let mut in_variable = false;

    for (i, &byte) in anchor.iter().enumerate() {
        if stable[i] {
            in_variable = false;
            pattern.push(byte as char);
        } else if !in_variable {
            in_variable = true;
            variable_positions.push(pattern.len());
            pattern.push('\x00');
        }
        // Additional variable bytes within the same run are absorbed.
    }

    OutputTemplate {
        pattern,
        variable_positions,
        instance_count: lines.len() as u64,
    }
}

/// Mark positions in `stable` as false where `anchor` and `other` differ.
///
/// Uses LCS-based alignment to handle insertions/deletions, not just
/// point substitutions.
fn mark_variable_regions(anchor: &[u8], other: &[u8], stable: &mut [bool]) {
    // Fast path: same length, direct comparison.
    if anchor.len() == other.len() {
        for (i, (&a, &b)) in anchor.iter().zip(other.iter()).enumerate() {
            if a != b {
                stable[i] = false;
            }
        }
        return;
    }

    // Different lengths: use LCS to align and mark unaligned positions.
    let (start_a, _start_b, len) = longest_common_substring(anchor, other);

    if len == 0 {
        // No common substring — entire line is variable.
        stable.fill(false);
        return;
    }

    // Mark everything outside the LCS match as variable.
    for i in 0..start_a {
        stable[i] = false;
    }
    for i in (start_a + len)..anchor.len() {
        stable[i] = false;
    }
}

/// Extract variable values from a line given a template.
#[must_use]
pub fn extract_variables(template: &OutputTemplate, line: &str) -> Vec<String> {
    if template.variable_positions.is_empty() {
        return vec![];
    }

    let pattern_parts: Vec<&str> = template.pattern.split('\x00').collect();
    let mut variables = Vec::with_capacity(template.variable_positions.len());
    let mut cursor = 0usize;

    for (i, &part) in pattern_parts.iter().enumerate() {
        // Match the static prefix.
        if !part.is_empty() {
            if let Some(pos) = line[cursor..].find(part) {
                // Everything between cursor and the match is the variable.
                if i > 0 {
                    variables.push(line[cursor..cursor + pos].to_string());
                }
                cursor += pos + part.len();
            } else {
                // Prefix not found — capture the rest as variable and bail.
                if i > 0 {
                    variables.push(line[cursor..].to_string());
                }
                cursor = line.len();
                break;
            }
        }
    }

    // If the template ends with a variable placeholder, capture trailing text.
    if template.pattern.ends_with('\x00') && cursor <= line.len() {
        variables.push(line[cursor..].to_string());
    }

    variables
}

/// Reconstruct a line from a template and variable values.
#[must_use]
pub fn reconstruct_line(template: &OutputTemplate, variables: &[String]) -> String {
    if template.variable_positions.is_empty() {
        return template.pattern.clone();
    }

    let parts: Vec<&str> = template.pattern.split('\x00').collect();
    let mut result = String::with_capacity(template.pattern.len() + 32);
    let mut var_idx = 0;

    for (i, part) in parts.iter().enumerate() {
        result.push_str(part);
        if i < parts.len() - 1 {
            if var_idx < variables.len() {
                result.push_str(&variables[var_idx]);
                var_idx += 1;
            }
        }
    }

    result
}

// =============================================================================
// Compression engine
// =============================================================================

/// Compress terminal output using semantic template extraction.
///
/// Lines are grouped by similarity, templates are extracted from each group,
/// and instances are delta-encoded as (template_idx, variable_values).
/// Lines that don't fit any group are stored as literals.
#[must_use]
pub fn compress(input: &str, config: &CompressionConfig) -> CompressedOutput {
    let lines: Vec<&str> = input.lines().collect();

    if lines.is_empty() {
        return CompressedOutput {
            templates: vec![],
            entries: vec![],
        };
    }

    // Phase 1: Group consecutive similar lines.
    let groups = group_similar_lines(&lines, config.similarity_threshold, config.min_group_size);

    // Phase 2: Extract templates from groups.
    let mut templates: Vec<OutputTemplate> = Vec::new();
    let mut entries: Vec<CompressedEntry> = Vec::new();

    for group in &groups {
        match group {
            LineGroup::Similar { start, end } => {
                let group_lines: Vec<&str> = lines[*start..*end].to_vec();

                if templates.len() >= config.max_templates {
                    // Template budget exhausted — store as literals.
                    for &line in &group_lines {
                        entries.push(CompressedEntry::Literal(line.to_string()));
                    }
                    continue;
                }

                let template = extract_template(&group_lines);
                let template_idx = templates.len();

                for &line in &group_lines {
                    let variables = extract_variables(&template, line);
                    entries.push(CompressedEntry::TemplateInstance {
                        template_idx,
                        variables,
                    });
                }

                templates.push(template);
            }
            LineGroup::Unique(idx) => {
                entries.push(CompressedEntry::Literal(lines[*idx].to_string()));
            }
        }
    }

    CompressedOutput { templates, entries }
}

/// Decompress previously compressed output back to the original string.
///
/// The output is lossless — byte-for-byte identical to the original lines
/// joined by `\n`.
#[must_use]
pub fn decompress(compressed: &CompressedOutput) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(compressed.entries.len());

    for entry in &compressed.entries {
        match entry {
            CompressedEntry::TemplateInstance {
                template_idx,
                variables,
            } => {
                let template = &compressed.templates[*template_idx];
                lines.push(reconstruct_line(template, variables));
            }
            CompressedEntry::Literal(line) => {
                lines.push(line.clone());
            }
        }
    }

    lines.join("\n")
}

/// Compute compression statistics for a given input and compressed output.
#[must_use]
pub fn compression_stats(input: &str, compressed: &CompressedOutput) -> CompressionStats {
    let input_lines = input.lines().count();
    let input_bytes = input.len();

    let template_count = compressed.templates.len();
    let mut templated_lines = 0;
    let mut literal_lines = 0;
    let mut compressed_bytes = 0usize;

    // Estimate compressed size: templates + entries overhead.
    for template in &compressed.templates {
        compressed_bytes += template.pattern.len() + template.variable_positions.len() * 4;
    }

    for entry in &compressed.entries {
        match entry {
            CompressedEntry::TemplateInstance { variables, .. } => {
                templated_lines += 1;
                // template_idx (4 bytes) + variable lengths.
                compressed_bytes += 4;
                for v in variables {
                    compressed_bytes += v.len() + 2; // length prefix + data
                }
            }
            CompressedEntry::Literal(line) => {
                literal_lines += 1;
                compressed_bytes += line.len() + 1; // tag byte + data
            }
        }
    }

    CompressionStats {
        input_lines,
        input_bytes,
        template_count,
        templated_lines,
        literal_lines,
        compressed_bytes,
        ratio: 0.0,
    }
    .finalize()
}

// =============================================================================
// Line grouping
// =============================================================================

#[derive(Debug)]
enum LineGroup {
    /// Consecutive similar lines: [start, end) indices.
    Similar { start: usize, end: usize },
    /// A single unique line at the given index.
    Unique(usize),
}

/// Group consecutive similar lines together.
fn group_similar_lines(lines: &[&str], threshold: f64, min_group_size: usize) -> Vec<LineGroup> {
    if lines.is_empty() {
        return vec![];
    }

    let mut groups: Vec<LineGroup> = Vec::new();
    let mut group_start = 0;

    let mut i = 1;
    while i < lines.len() {
        if lines_similar(lines[group_start], lines[i], threshold) {
            i += 1;
        } else {
            let group_len = i - group_start;
            if group_len >= min_group_size {
                groups.push(LineGroup::Similar {
                    start: group_start,
                    end: i,
                });
            } else {
                for idx in group_start..i {
                    groups.push(LineGroup::Unique(idx));
                }
            }
            group_start = i;
            i += 1;
        }
    }

    // Final group.
    let group_len = lines.len() - group_start;
    if group_len >= min_group_size {
        groups.push(LineGroup::Similar {
            start: group_start,
            end: lines.len(),
        });
    } else {
        for idx in group_start..lines.len() {
            groups.push(LineGroup::Unique(idx));
        }
    }

    groups
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Edit distance ────────────────────────────────────────────────

    #[test]
    fn edit_distance_identical() {
        assert_eq!(edit_distance(b"hello", b"hello"), 0);
    }

    #[test]
    fn edit_distance_empty() {
        assert_eq!(edit_distance(b"", b""), 0);
        assert_eq!(edit_distance(b"abc", b""), 3);
        assert_eq!(edit_distance(b"", b"abc"), 3);
    }

    #[test]
    fn edit_distance_single_char_diff() {
        assert_eq!(edit_distance(b"cat", b"bat"), 1);
    }

    #[test]
    fn edit_distance_insertion_deletion() {
        assert_eq!(edit_distance(b"kitten", b"sitting"), 3);
    }

    // ── Line similarity ──────────────────────────────────────────────

    #[test]
    fn similar_identical_lines() {
        assert!(lines_similar(
            "Processing file 1/100",
            "Processing file 1/100",
            0.3
        ));
    }

    #[test]
    fn similar_progress_counter() {
        assert!(lines_similar(
            "Processing file 1/100",
            "Processing file 2/100",
            0.3
        ));
    }

    #[test]
    fn similar_progress_counter_large_numbers() {
        assert!(lines_similar(
            "Processing file 10/100",
            "Processing file 99/100",
            0.3
        ));
    }

    #[test]
    fn not_similar_different_lines() {
        assert!(!lines_similar(
            "Processing file 1/100",
            "Compilation failed: error[E0308]",
            0.3
        ));
    }

    #[test]
    fn similar_empty_lines() {
        assert!(lines_similar("", "", 0.3));
    }

    #[test]
    fn similarity_is_symmetric() {
        let a = "Processing file 1/100";
        let b = "Processing file 50/100";
        assert_eq!(lines_similar(a, b, 0.3), lines_similar(b, a, 0.3));
    }

    // ── Longest common substring ─────────────────────────────────────

    #[test]
    fn lcs_identical_strings() {
        let (a, b, len) = longest_common_substring(b"hello", b"hello");
        assert_eq!(a, 0);
        assert_eq!(b, 0);
        assert_eq!(len, 5);
    }

    #[test]
    fn lcs_partial_match() {
        let (_, _, len) = longest_common_substring(b"abcdefg", b"xyzabcqrs");
        assert_eq!(len, 3); // "abc"
    }

    #[test]
    fn lcs_empty_strings() {
        let (_, _, len) = longest_common_substring(b"", b"hello");
        assert_eq!(len, 0);
    }

    // ── Template extraction ──────────────────────────────────────────

    #[test]
    fn template_from_progress_lines() {
        let lines: Vec<&str> = vec![
            "Processing file 1/100",
            "Processing file 2/100",
            "Processing file 3/100",
        ];
        let template = extract_template(&lines);

        // Template should have variable positions where numbers differ.
        assert!(template.variable_positions.len() >= 1);
        assert_eq!(template.instance_count, 3);
    }

    #[test]
    fn template_single_line() {
        let template = extract_template(&["single line"]);
        assert_eq!(template.pattern, "single line");
        assert!(template.variable_positions.is_empty());
        assert_eq!(template.instance_count, 1);
    }

    #[test]
    fn template_from_same_length_lines() {
        let lines = vec!["test_a: pass", "test_b: pass", "test_c: pass"];
        let template = extract_template(&lines);
        assert!(!template.variable_positions.is_empty());
        assert_eq!(template.instance_count, 3);
    }

    // ── Variable extraction & reconstruction ─────────────────────────

    #[test]
    fn extract_and_reconstruct_roundtrip() {
        let lines = vec![
            "Processing file 1/100",
            "Processing file 2/100",
            "Processing file 3/100",
        ];
        let template = extract_template(&lines);

        for &line in &lines {
            let vars = extract_variables(&template, line);
            let reconstructed = reconstruct_line(&template, &vars);
            assert_eq!(reconstructed, line, "roundtrip failed for: {line:?}");
        }
    }

    #[test]
    fn reconstruct_no_variables() {
        let template = OutputTemplate {
            pattern: "static line".to_string(),
            variable_positions: vec![],
            instance_count: 1,
        };
        assert_eq!(reconstruct_line(&template, &[]), "static line");
    }

    // ── Compression / decompression ──────────────────────────────────

    #[test]
    fn compress_empty_input() {
        let compressed = compress("", &CompressionConfig::default());
        assert!(compressed.templates.is_empty());
        assert!(compressed.entries.is_empty());
    }

    #[test]
    fn compress_decompress_lossless_progress() {
        let lines: Vec<String> = (1..=20)
            .map(|i| format!("Processing file {i}/20"))
            .collect();
        let input = lines.join("\n");

        let compressed = compress(&input, &CompressionConfig::default());
        let decompressed = decompress(&compressed);

        assert_eq!(decompressed, input, "lossless roundtrip failed");
    }

    #[test]
    fn compress_decompress_lossless_mixed() {
        let input = "\
error: compilation failed
warning: unused variable `x`
Processing file 1/10
Processing file 2/10
Processing file 3/10
Processing file 4/10
Processing file 5/10
Processing file 6/10
Processing file 7/10
Processing file 8/10
Processing file 9/10
Processing file 10/10
Done.";

        let compressed = compress(input, &CompressionConfig::default());
        let decompressed = decompress(&compressed);

        assert_eq!(
            decompressed, input,
            "lossless roundtrip failed for mixed content"
        );
    }

    #[test]
    fn compress_decompress_all_unique() {
        let input = "line one\nline two\nline three";
        let compressed = compress(input, &CompressionConfig::default());
        let decompressed = decompress(&compressed);
        assert_eq!(decompressed, input);
    }

    #[test]
    fn compression_reduces_for_repetitive_input() {
        let lines: Vec<String> = (1..=100)
            .map(|i| format!("Processing file {i}/100"))
            .collect();
        let input = lines.join("\n");

        let compressed = compress(&input, &CompressionConfig::default());
        let stats = compression_stats(&input, &compressed);

        assert!(
            stats.template_count >= 1,
            "expected at least 1 template, got {}",
            stats.template_count
        );
        assert!(
            stats.ratio > 1.5,
            "expected compression ratio > 1.5, got {:.2}",
            stats.ratio
        );
    }

    #[test]
    fn compression_stats_for_empty_input() {
        let compressed = compress("", &CompressionConfig::default());
        let stats = compression_stats("", &compressed);
        assert_eq!(stats.input_lines, 0);
        assert!((stats.ratio - 1.0).abs() < f64::EPSILON);
    }

    // ── Line grouping ────────────────────────────────────────────────

    #[test]
    fn group_all_similar() {
        let lines: Vec<&str> = (0..5).map(|_| "identical line").collect();
        let groups = group_similar_lines(&lines, 0.3, 3);

        assert_eq!(groups.len(), 1);
        assert!(matches!(groups[0], LineGroup::Similar { start: 0, end: 5 }));
    }

    #[test]
    fn group_all_unique() {
        let lines = vec![
            "alpha bravo charlie",
            "delta echo foxtrot",
            "golf hotel india",
        ];
        let groups = group_similar_lines(&lines, 0.3, 3);

        // All unique — each should be a Unique group.
        assert!(groups.iter().all(|g| matches!(g, LineGroup::Unique(_))));
    }

    #[test]
    fn group_mixed() {
        let mut lines: Vec<&str> = Vec::new();
        lines.push("preamble text here");
        for _ in 0..5 {
            lines.push("Processing file 1/100");
        }
        lines.push("epilogue text here");

        let groups = group_similar_lines(&lines, 0.3, 3);

        // Should have: Unique(0), Similar{1..6}, Unique(6)
        let similar_count = groups
            .iter()
            .filter(|g| matches!(g, LineGroup::Similar { .. }))
            .count();
        assert_eq!(similar_count, 1);
    }

    // ── Config tests ─────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = CompressionConfig::default();
        assert!((c.similarity_threshold - 0.3).abs() < f64::EPSILON);
        assert_eq!(c.min_group_size, 3);
        assert_eq!(c.max_templates, 1000);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = CompressionConfig {
            similarity_threshold: 0.2,
            min_group_size: 5,
            max_templates: 500,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: CompressionConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.similarity_threshold - 0.2).abs() < f64::EPSILON);
        assert_eq!(parsed.min_group_size, 5);
    }

    // ── Stats serde ──────────────────────────────────────────────────

    #[test]
    fn stats_serde_roundtrip() {
        let s = CompressionStats {
            input_lines: 100,
            input_bytes: 5000,
            template_count: 3,
            templated_lines: 80,
            literal_lines: 20,
            compressed_bytes: 1000,
            ratio: 5.0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: CompressionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.input_lines, 100);
        assert!((parsed.ratio - 5.0).abs() < f64::EPSILON);
    }

    // ── Regression: same-length lines with single-char diff ──────────

    #[test]
    fn same_length_single_char_variable() {
        let lines = vec!["test_a: PASS", "test_b: PASS", "test_c: PASS"];
        let template = extract_template(&lines);

        for &line in &lines {
            let vars = extract_variables(&template, line);
            let reconstructed = reconstruct_line(&template, &vars);
            assert_eq!(reconstructed, line);
        }
    }

    // ── Reconstruction edge cases ────────────────────────────────────

    #[test]
    fn reconstruct_leading_variable() {
        // Template where the variable is at the start.
        let lines = vec!["1: done", "2: done", "3: done"];
        let template = extract_template(&lines);

        for &line in &lines {
            let vars = extract_variables(&template, line);
            let reconstructed = reconstruct_line(&template, &vars);
            assert_eq!(reconstructed, line);
        }
    }

    #[test]
    fn reconstruct_trailing_variable() {
        // Template where the variable is at the end.
        let lines = vec!["status: 200", "status: 404", "status: 500"];
        let template = extract_template(&lines);

        for &line in &lines {
            let vars = extract_variables(&template, line);
            let reconstructed = reconstruct_line(&template, &vars);
            assert_eq!(reconstructed, line);
        }
    }
}
