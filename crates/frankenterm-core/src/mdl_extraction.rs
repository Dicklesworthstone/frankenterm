//! Minimum Description Length (MDL) command extraction for ARS recovery sequences.
//!
//! When agents flail through errors — typing wrong commands, retrying failed
//! operations — we need to extract only the *minimal causal sequence* that
//! actually fixed the problem. MDL (Kolmogorov complexity approximation) finds
//! the shortest description of the recovery, discarding noise.
//!
//! # Algorithm
//!
//! 1. Partition terminal history into [`CommandBlock`]s using OSC 133 boundaries
//! 2. Label each block: success (exit 0), failure (exit != 0), or unknown
//! 3. Compute MDL score for candidate subsequences using `zstd` compressed length
//! 4. Find the minimal subsequence that:
//!    - Ends with a successful command
//!    - Has the lowest MDL (compressed description length)
//!    - Preserves causal ordering
//!
//! # Performance
//!
//! Extraction targets < 1ms for typical recovery sequences (< 50 commands).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for MDL extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MdlConfig {
    /// Maximum commands to consider in a recovery window.
    pub max_window_size: usize,
    /// Minimum commands required for extraction.
    pub min_window_size: usize,
    /// Maximum candidate subsequences to evaluate (combinatorial bound).
    pub max_candidates: usize,
    /// Compression level for MDL scoring (1-22, higher = better but slower).
    pub compression_level: u32,
    /// Minimum confidence score to accept an extraction (0.0–1.0).
    pub min_confidence: f64,
    /// Whether to include failed commands that appear necessary for context.
    pub include_context_failures: bool,
}

impl Default for MdlConfig {
    fn default() -> Self {
        Self {
            max_window_size: 50,
            min_window_size: 1,
            max_candidates: 1000,
            compression_level: 3,
            min_confidence: 0.3,
            include_context_failures: false,
        }
    }
}

// =============================================================================
// Command block
// =============================================================================

/// A discrete command block delimited by OSC 133 markers.
///
/// Represents one shell command from prompt → execution → completion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandBlock {
    /// Sequential index within the recovery window.
    pub index: u32,
    /// The command text (as entered).
    pub command: String,
    /// Exit code (None if unknown/interrupted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Execution duration in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_us: Option<u64>,
    /// Truncated stdout (first N bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    /// Timestamp (epoch μs).
    pub timestamp_us: u64,
}

impl CommandBlock {
    /// Whether this command succeeded (exit code 0).
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// Whether this command failed (exit code != 0).
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self.exit_code, Some(c) if c != 0)
    }

    /// Whether the exit code is unknown.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self.exit_code.is_none()
    }
}

// =============================================================================
// MDL scoring
// =============================================================================

/// Compute the MDL score for a command sequence.
///
/// Uses compression ratio as a proxy for Kolmogorov complexity:
/// - Lower compressed size = simpler description = better MDL score
/// - Normalized by uncompressed length for comparability
///
/// Returns (compressed_len, uncompressed_len, ratio).
#[must_use]
pub fn mdl_score(commands: &[&CommandBlock], level: u32) -> MdlScore {
    if commands.is_empty() {
        return MdlScore {
            compressed_len: 0,
            uncompressed_len: 0,
            ratio: 0.0,
            command_count: 0,
        };
    }

    // Build the description string: command texts concatenated with delimiters.
    let description = commands
        .iter()
        .map(|c| c.command.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");

    let uncompressed = description.as_bytes();
    let uncompressed_len = uncompressed.len();

    // Use zstd-style byte estimation. Since we forbid unsafe code and may not
    // have zstd available, we use a deterministic approximation: count unique
    // byte sequences of length 3 (trigrams) as a proxy for compressibility.
    let compressed_len = estimate_compressed_len(uncompressed, level);

    let ratio = if uncompressed_len == 0 {
        0.0
    } else {
        compressed_len as f64 / uncompressed_len as f64
    };

    MdlScore {
        compressed_len,
        uncompressed_len,
        ratio,
        command_count: commands.len(),
    }
}

/// MDL score for a candidate subsequence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MdlScore {
    /// Estimated compressed length (bytes).
    pub compressed_len: usize,
    /// Original uncompressed length (bytes).
    pub uncompressed_len: usize,
    /// Compression ratio (compressed / uncompressed). Lower = better.
    pub ratio: f64,
    /// Number of commands in the subsequence.
    pub command_count: usize,
}

/// Estimate compressed length using trigram entropy as a proxy.
///
/// This is a deterministic, safe approximation of compression ratio
/// without requiring actual compression libraries.
fn estimate_compressed_len(data: &[u8], _level: u32) -> usize {
    if data.len() < 3 {
        return data.len();
    }

    // Count unique trigrams as a measure of information content.
    let mut trigram_set = std::collections::HashSet::new();
    for window in data.windows(3) {
        trigram_set.insert((window[0], window[1], window[2]));
    }

    let unique_trigrams = trigram_set.len();
    let total_trigrams = data.len() - 2;

    // Entropy-based estimate: more unique trigrams = less compressible.
    // Approximate compressed size as: header + unique_trigrams * avg_code_len
    let uniqueness_ratio = unique_trigrams as f64 / total_trigrams as f64;

    // Model: compressed ≈ data_len * (base_ratio + uniqueness_factor)
    // base_ratio ~0.1 for highly repetitive, ~0.9 for random
    let base_ratio = 0.8f64.mul_add(uniqueness_ratio, 0.1);
    let estimated = (data.len() as f64 * base_ratio).ceil() as usize;

    // Never compress to more than original.
    estimated.min(data.len()).max(1)
}

// =============================================================================
// Extraction result
// =============================================================================

/// Result of MDL command extraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionResult {
    /// The extracted minimal command sequence.
    pub commands: Vec<CommandBlock>,
    /// MDL score of the extracted sequence.
    pub score: MdlScore,
    /// Confidence in this extraction (0.0–1.0).
    /// Higher when the extracted sequence is much simpler than the full window.
    pub confidence: f64,
    /// Total commands in the original window.
    pub window_size: usize,
    /// Reduction ratio: 1 - (extracted_len / window_len). Higher = more noise removed.
    pub reduction_ratio: f64,
    /// Reason code for the extraction outcome.
    pub reason_code: ExtractionReason,
    /// Correlation ID linking to the context snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Why the extraction produced this result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExtractionReason {
    /// Successfully extracted a minimal sequence.
    Success,
    /// Window too small to extract (< min_window_size).
    WindowTooSmall,
    /// No successful command found in the window.
    NoSuccessFound,
    /// All commands were successful (no noise to remove).
    AllSuccessful,
    /// Confidence below threshold.
    LowConfidence,
    /// Candidate limit reached; result may be suboptimal.
    CandidateLimitReached,
}

// =============================================================================
// MDL extractor
// =============================================================================

/// Extracts the minimal causal command sequence from a recovery window.
pub struct MdlExtractor {
    config: MdlConfig,
}

impl MdlExtractor {
    /// Create a new extractor with the given configuration.
    #[must_use]
    pub fn new(config: MdlConfig) -> Self {
        Self { config }
    }

    /// Create an extractor with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(MdlConfig::default())
    }

    /// Extract the minimal causal sequence from a command window.
    ///
    /// The window should contain commands between a regime shift (error detected)
    /// and recovery (successful command observed).
    #[must_use]
    pub fn extract(&self, window: &[CommandBlock]) -> ExtractionResult {
        let window_size = window.len();

        // Edge case: window too small.
        if window_size < self.config.min_window_size {
            return ExtractionResult {
                commands: Vec::new(),
                score: MdlScore {
                    compressed_len: 0,
                    uncompressed_len: 0,
                    ratio: 0.0,
                    command_count: 0,
                },
                confidence: 0.0,
                window_size,
                reduction_ratio: 0.0,
                reason_code: ExtractionReason::WindowTooSmall,
                correlation_id: None,
            };
        }

        // Truncate to max window size (keep most recent).
        let effective_window = if window_size > self.config.max_window_size {
            &window[window_size - self.config.max_window_size..]
        } else {
            window
        };

        // Find the last successful command.
        let last_success_idx = effective_window.iter().rposition(CommandBlock::is_success);

        if last_success_idx.is_none() {
            return ExtractionResult {
                commands: Vec::new(),
                score: MdlScore {
                    compressed_len: 0,
                    uncompressed_len: 0,
                    ratio: 0.0,
                    command_count: 0,
                },
                confidence: 0.0,
                window_size,
                reduction_ratio: 0.0,
                reason_code: ExtractionReason::NoSuccessFound,
                correlation_id: None,
            };
        }

        let last_success = last_success_idx.unwrap();

        // Check if all commands are successful.
        let all_success = effective_window.iter().all(CommandBlock::is_success);
        if all_success {
            let refs: Vec<&CommandBlock> = effective_window.iter().collect();
            let score = mdl_score(&refs, self.config.compression_level);
            return ExtractionResult {
                commands: effective_window.to_vec(),
                score,
                confidence: 1.0,
                window_size,
                reduction_ratio: 0.0,
                reason_code: ExtractionReason::AllSuccessful,
                correlation_id: None,
            };
        }

        // Generate candidate subsequences and find the one with best MDL.
        let candidates = self.generate_candidates(effective_window, last_success);
        let full_score = {
            let refs: Vec<&CommandBlock> = effective_window.iter().collect();
            mdl_score(&refs, self.config.compression_level)
        };

        let mut best_result: Option<(Vec<CommandBlock>, MdlScore)> = None;
        let mut hit_limit = false;

        for (i, candidate) in candidates.iter().enumerate() {
            if i >= self.config.max_candidates {
                hit_limit = true;
                break;
            }

            let refs: Vec<&CommandBlock> = candidate.iter().collect();
            let score = mdl_score(&refs, self.config.compression_level);

            let is_better = match &best_result {
                None => true,
                Some((_, best_score)) => {
                    // Prefer fewer commands; break ties by compression ratio.
                    score.command_count < best_score.command_count
                        || (score.command_count == best_score.command_count
                            && score.ratio < best_score.ratio)
                }
            };

            if is_better {
                best_result = Some((candidate.clone(), score));
            }
        }

        match best_result {
            Some((commands, score)) => {
                let reduction = if window_size > 0 {
                    1.0 - (commands.len() as f64 / window_size as f64)
                } else {
                    0.0
                };

                // Confidence: how much simpler is the extracted sequence vs full window?
                let confidence = if full_score.compressed_len > 0 {
                    let ratio_improvement =
                        1.0 - (score.compressed_len as f64 / full_score.compressed_len as f64);
                    // Blend reduction ratio and compression improvement.
                    (reduction * 0.6 + ratio_improvement.max(0.0) * 0.4).clamp(0.0, 1.0)
                } else {
                    reduction
                };

                let reason = if hit_limit {
                    ExtractionReason::CandidateLimitReached
                } else if confidence < self.config.min_confidence {
                    ExtractionReason::LowConfidence
                } else {
                    ExtractionReason::Success
                };

                debug!(
                    window_size = window_size,
                    extracted = commands.len(),
                    reduction = format!("{:.1}%", reduction * 100.0),
                    confidence = format!("{:.3}", confidence),
                    reason = ?reason,
                    "MDL extraction complete"
                );

                ExtractionResult {
                    commands,
                    score,
                    confidence,
                    window_size,
                    reduction_ratio: reduction,
                    reason_code: reason,
                    correlation_id: None,
                }
            }
            None => ExtractionResult {
                commands: Vec::new(),
                score: MdlScore {
                    compressed_len: 0,
                    uncompressed_len: 0,
                    ratio: 0.0,
                    command_count: 0,
                },
                confidence: 0.0,
                window_size,
                reduction_ratio: 0.0,
                reason_code: ExtractionReason::NoSuccessFound,
                correlation_id: None,
            },
        }
    }

    /// Generate candidate subsequences from the window.
    ///
    /// Strategy: enumerate subsets that include the last successful command,
    /// preferring shorter sequences. Uses greedy pruning to stay within limits.
    fn generate_candidates(
        &self,
        window: &[CommandBlock],
        last_success: usize,
    ) -> Vec<Vec<CommandBlock>> {
        let mut candidates = Vec::new();
        let n = window.len();

        // Candidate 1: just the last successful command.
        candidates.push(vec![window[last_success].clone()]);

        // Candidate 2: all successful commands up to and including last_success.
        let success_only: Vec<CommandBlock> = window[..=last_success]
            .iter()
            .filter(|c| c.is_success())
            .cloned()
            .collect();
        if success_only.len() > 1 {
            candidates.push(success_only);
        }

        // Candidate 3: contiguous suffix ending at last_success.
        // Try suffixes of increasing length.
        for start in (0..last_success).rev() {
            if candidates.len() >= self.config.max_candidates {
                break;
            }
            let suffix: Vec<CommandBlock> = window[start..=last_success].to_vec();
            candidates.push(suffix);
        }

        // Candidate 4: if include_context_failures, include the last failure
        // before the success (as context for what went wrong).
        if self.config.include_context_failures {
            if let Some(last_fail) = window[..last_success]
                .iter()
                .rposition(CommandBlock::is_failure)
            {
                let with_context = vec![window[last_fail].clone(), window[last_success].clone()];
                candidates.push(with_context);
            }
        }

        // Candidate 5: for each successful command, the pair with the final success.
        for (i, cmd) in window[..last_success].iter().enumerate() {
            if candidates.len() >= self.config.max_candidates {
                break;
            }
            if cmd.is_success() && i != last_success {
                candidates.push(vec![cmd.clone(), window[last_success].clone()]);
            }
        }

        // Deduplicate by command sequence.
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| {
            let key: Vec<u32> = c.iter().map(|b| b.index).collect();
            seen.insert(key)
        });

        trace!(
            candidates = candidates.len(),
            window_size = n,
            "Generated MDL candidates"
        );

        candidates
    }
}

// =============================================================================
// Recovery window builder
// =============================================================================

/// Builds a command window from raw terminal data for MDL extraction.
pub struct WindowBuilder {
    blocks: Vec<CommandBlock>,
    next_index: u32,
}

impl WindowBuilder {
    /// Create a new empty window builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            next_index: 0,
        }
    }

    /// Add a command block to the window.
    pub fn add_command(
        &mut self,
        command: String,
        exit_code: Option<i32>,
        duration_us: Option<u64>,
        timestamp_us: u64,
    ) -> &CommandBlock {
        let block = CommandBlock {
            index: self.next_index,
            command,
            exit_code,
            duration_us,
            output_preview: None,
            timestamp_us,
        };
        self.next_index += 1;
        self.blocks.push(block);
        self.blocks.last().unwrap()
    }

    /// Add a command block with output preview.
    pub fn add_command_with_output(
        &mut self,
        command: String,
        exit_code: Option<i32>,
        duration_us: Option<u64>,
        timestamp_us: u64,
        output_preview: String,
    ) -> &CommandBlock {
        let block = CommandBlock {
            index: self.next_index,
            command,
            exit_code,
            duration_us,
            output_preview: Some(output_preview),
            timestamp_us,
        };
        self.next_index += 1;
        self.blocks.push(block);
        self.blocks.last().unwrap()
    }

    /// Get the built window.
    #[must_use]
    pub fn blocks(&self) -> &[CommandBlock] {
        &self.blocks
    }

    /// Consume the builder and return the command blocks.
    #[must_use]
    pub fn into_blocks(self) -> Vec<CommandBlock> {
        self.blocks
    }

    /// Number of blocks in the window.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the window is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Clear the window.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.next_index = 0;
    }
}

impl Default for WindowBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Extraction statistics
// =============================================================================

/// Aggregate statistics across multiple extractions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionStats {
    /// Total extractions performed.
    pub total_extractions: u64,
    /// Successful extractions.
    pub successful: u64,
    /// Average reduction ratio across successful extractions.
    pub mean_reduction: f64,
    /// Average confidence across successful extractions.
    pub mean_confidence: f64,
    /// Total commands processed.
    pub total_commands_processed: u64,
    /// Total commands in extracted sequences.
    pub total_commands_extracted: u64,
    /// Per-reason code counts.
    pub reason_counts: HashMap<String, u64>,
}

impl ExtractionStats {
    /// Create empty stats.
    #[must_use]
    pub fn new() -> Self {
        Self {
            total_extractions: 0,
            successful: 0,
            mean_reduction: 0.0,
            mean_confidence: 0.0,
            total_commands_processed: 0,
            total_commands_extracted: 0,
            reason_counts: HashMap::new(),
        }
    }

    /// Record an extraction result.
    pub fn record(&mut self, result: &ExtractionResult) {
        self.total_extractions += 1;
        self.total_commands_processed += result.window_size as u64;
        self.total_commands_extracted += result.commands.len() as u64;

        let reason_key = format!("{:?}", result.reason_code);
        *self.reason_counts.entry(reason_key).or_insert(0) += 1;

        if result.reason_code == ExtractionReason::Success {
            self.successful += 1;
            // Running mean update.
            let n = self.successful as f64;
            self.mean_reduction += (result.reduction_ratio - self.mean_reduction) / n;
            self.mean_confidence += (result.confidence - self.mean_confidence) / n;
        }
    }
}

impl Default for ExtractionStats {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(index: u32, cmd: &str, exit_code: Option<i32>) -> CommandBlock {
        CommandBlock {
            index,
            command: cmd.to_string(),
            exit_code,
            duration_us: Some(1000),
            output_preview: None,
            timestamp_us: (index as u64 + 1) * 1_000_000,
        }
    }

    fn success_block(index: u32, cmd: &str) -> CommandBlock {
        make_block(index, cmd, Some(0))
    }

    fn failure_block(index: u32, cmd: &str) -> CommandBlock {
        make_block(index, cmd, Some(1))
    }

    fn default_extractor() -> MdlExtractor {
        MdlExtractor::with_defaults()
    }

    // -------------------------------------------------------------------------
    // MdlConfig
    // -------------------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let cfg = MdlConfig::default();
        assert_eq!(cfg.max_window_size, 50);
        assert_eq!(cfg.min_window_size, 1);
        assert_eq!(cfg.max_candidates, 1000);
        assert_eq!(cfg.compression_level, 3);
        assert!((cfg.min_confidence - 0.3).abs() < f64::EPSILON);
        assert!(!cfg.include_context_failures);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = MdlConfig {
            max_window_size: 100,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: MdlConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.max_window_size, 100);
    }

    // -------------------------------------------------------------------------
    // CommandBlock
    // -------------------------------------------------------------------------

    #[test]
    fn command_block_success() {
        let block = success_block(0, "cargo build");
        assert!(block.is_success());
        assert!(!block.is_failure());
        assert!(!block.is_unknown());
    }

    #[test]
    fn command_block_failure() {
        let block = failure_block(0, "cargo build");
        assert!(!block.is_success());
        assert!(block.is_failure());
        assert!(!block.is_unknown());
    }

    #[test]
    fn command_block_unknown() {
        let block = make_block(0, "cargo build", None);
        assert!(!block.is_success());
        assert!(!block.is_failure());
        assert!(block.is_unknown());
    }

    #[test]
    fn command_block_serde() {
        let block = success_block(42, "ls -la");
        let json = serde_json::to_string(&block).unwrap();
        let decoded: CommandBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.command, "ls -la");
        assert_eq!(decoded.index, 42);
        assert_eq!(decoded.exit_code, Some(0));
    }

    // -------------------------------------------------------------------------
    // MDL scoring
    // -------------------------------------------------------------------------

    #[test]
    fn mdl_score_empty() {
        let score = mdl_score(&[], 3);
        assert_eq!(score.compressed_len, 0);
        assert_eq!(score.uncompressed_len, 0);
        assert_eq!(score.command_count, 0);
    }

    #[test]
    fn mdl_score_single_command() {
        let block = success_block(0, "ls");
        let score = mdl_score(&[&block], 3);
        assert!(score.compressed_len > 0);
        assert_eq!(score.uncompressed_len, 2); // "ls"
        assert_eq!(score.command_count, 1);
    }

    #[test]
    fn mdl_score_repetitive_more_compressible() {
        let b1 = success_block(0, "echo hello");
        let b2 = success_block(1, "echo hello");
        let b3 = success_block(2, "echo hello");
        let score_rep = mdl_score(&[&b1, &b2, &b3], 3);

        let b4 = success_block(3, "ls -la /tmp");
        let b5 = success_block(4, "cat /etc/hosts");
        let b6 = success_block(5, "grep foo bar.txt");
        let score_diverse = mdl_score(&[&b4, &b5, &b6], 3);

        // Repetitive commands should have lower compression ratio.
        assert!(
            score_rep.ratio <= score_diverse.ratio,
            "Repetitive ratio {} should be <= diverse ratio {}",
            score_rep.ratio,
            score_diverse.ratio
        );
    }

    #[test]
    fn mdl_score_longer_has_more_info() {
        let b1 = success_block(0, "x");
        let score_short = mdl_score(&[&b1], 3);

        let b2 = success_block(1, "x");
        let b3 = success_block(2, "x");
        let score_long = mdl_score(&[&b1, &b2, &b3], 3);

        assert!(score_long.uncompressed_len > score_short.uncompressed_len);
    }

    // -------------------------------------------------------------------------
    // MdlExtractor — edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn extract_empty_window() {
        let ext = default_extractor();
        let result = ext.extract(&[]);
        assert_eq!(result.reason_code, ExtractionReason::WindowTooSmall);
        assert!(result.commands.is_empty());
    }

    #[test]
    fn extract_single_success() {
        let ext = default_extractor();
        let window = vec![success_block(0, "cargo build")];
        let result = ext.extract(&window);
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.window_size, 1);
    }

    #[test]
    fn extract_no_success() {
        let ext = default_extractor();
        let window = vec![
            failure_block(0, "cargo build"),
            failure_block(1, "cargo build"),
        ];
        let result = ext.extract(&window);
        assert_eq!(result.reason_code, ExtractionReason::NoSuccessFound);
    }

    #[test]
    fn extract_all_successful() {
        let ext = default_extractor();
        let window = vec![
            success_block(0, "cd /project"),
            success_block(1, "cargo build"),
            success_block(2, "cargo test"),
        ];
        let result = ext.extract(&window);
        assert_eq!(result.reason_code, ExtractionReason::AllSuccessful);
        assert_eq!(result.commands.len(), 3);
        assert!((result.confidence - 1.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // MdlExtractor — noise removal
    // -------------------------------------------------------------------------

    #[test]
    fn extract_removes_failed_retries() {
        let ext = default_extractor();
        let window = vec![
            failure_block(0, "cargo build"),
            failure_block(1, "cargo build"),
            failure_block(2, "cargo build"),
            success_block(3, "cargo fix && cargo build"),
        ];
        let result = ext.extract(&window);
        // Should extract fewer commands than the full window.
        assert!(result.commands.len() < window.len());
        // Must include the successful command.
        assert!(result.commands.iter().any(|c| c.is_success()));
    }

    #[test]
    fn extract_identifies_fix_command() {
        let ext = default_extractor();
        let window = vec![
            failure_block(0, "ls nonexistent"),
            failure_block(1, "cat badfile"),
            success_block(2, "touch newfile"),
            failure_block(3, "rm -f wrongthing"),
            success_block(4, "cargo build"),
        ];
        let result = ext.extract(&window);
        // The extracted sequence must end with a success.
        assert!(!result.commands.is_empty());
        assert!(result.commands.last().unwrap().is_success());
    }

    #[test]
    fn extract_prefers_shorter_sequence() {
        let ext = default_extractor();
        // Many failed commands, then one success.
        let mut window: Vec<CommandBlock> = (0..10)
            .map(|i| failure_block(i, &format!("attempt-{i}")))
            .collect();
        window.push(success_block(10, "the-fix"));

        let result = ext.extract(&window);
        // Should extract just the fix, not all the failures.
        assert!(
            result.commands.len() <= 3,
            "Expected <= 3 commands, got {}",
            result.commands.len()
        );
    }

    #[test]
    fn extract_reduction_ratio() {
        let ext = default_extractor();
        let mut window: Vec<CommandBlock> = (0..8)
            .map(|i| failure_block(i, &format!("fail-{i}")))
            .collect();
        window.push(success_block(8, "fix"));
        window.push(success_block(9, "verify"));

        let result = ext.extract(&window);
        // Should have positive reduction.
        assert!(result.reduction_ratio > 0.0);
        assert!(result.reduction_ratio <= 1.0);
    }

    // -------------------------------------------------------------------------
    // MdlExtractor — context failures
    // -------------------------------------------------------------------------

    #[test]
    fn extract_with_context_failures() {
        let config = MdlConfig {
            include_context_failures: true,
            min_confidence: 0.0,
            ..Default::default()
        };
        let ext = MdlExtractor::new(config);

        let window = vec![
            failure_block(0, "bad command"),
            failure_block(1, "another fail"),
            success_block(2, "the fix"),
        ];
        let result = ext.extract(&window);
        // At minimum should have the fix; may include the last failure for context.
        assert!(!result.commands.is_empty());
    }

    // -------------------------------------------------------------------------
    // MdlExtractor — max window truncation
    // -------------------------------------------------------------------------

    #[test]
    fn extract_truncates_large_window() {
        let config = MdlConfig {
            max_window_size: 5,
            min_confidence: 0.0,
            ..Default::default()
        };
        let ext = MdlExtractor::new(config);

        let mut window: Vec<CommandBlock> = (0..20)
            .map(|i| failure_block(i, &format!("fail-{i}")))
            .collect();
        window.push(success_block(20, "fix"));

        let result = ext.extract(&window);
        // Should still find the fix in the truncated window.
        assert!(!result.commands.is_empty());
    }

    // -------------------------------------------------------------------------
    // WindowBuilder
    // -------------------------------------------------------------------------

    #[test]
    fn window_builder_basic() {
        let mut builder = WindowBuilder::new();
        assert!(builder.is_empty());

        builder.add_command("ls".to_string(), Some(0), Some(100), 1000);
        builder.add_command("cd /tmp".to_string(), Some(0), Some(200), 2000);

        assert_eq!(builder.len(), 2);
        assert!(!builder.is_empty());
        assert_eq!(builder.blocks()[0].index, 0);
        assert_eq!(builder.blocks()[1].index, 1);
    }

    #[test]
    fn window_builder_with_output() {
        let mut builder = WindowBuilder::new();
        builder.add_command_with_output(
            "ls".to_string(),
            Some(0),
            Some(100),
            1000,
            "file1.txt\nfile2.txt".to_string(),
        );

        assert_eq!(
            builder.blocks()[0].output_preview.as_deref(),
            Some("file1.txt\nfile2.txt")
        );
    }

    #[test]
    fn window_builder_clear() {
        let mut builder = WindowBuilder::new();
        builder.add_command("ls".to_string(), Some(0), None, 1000);
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
    }

    #[test]
    fn window_builder_into_blocks() {
        let mut builder = WindowBuilder::new();
        builder.add_command("ls".to_string(), Some(0), None, 1000);
        let blocks = builder.into_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].command, "ls");
    }

    // -------------------------------------------------------------------------
    // ExtractionStats
    // -------------------------------------------------------------------------

    #[test]
    fn stats_empty() {
        let stats = ExtractionStats::new();
        assert_eq!(stats.total_extractions, 0);
        assert_eq!(stats.successful, 0);
    }

    #[test]
    fn stats_record_success() {
        let mut stats = ExtractionStats::new();
        let result = ExtractionResult {
            commands: vec![success_block(0, "fix")],
            score: MdlScore {
                compressed_len: 3,
                uncompressed_len: 3,
                ratio: 1.0,
                command_count: 1,
            },
            confidence: 0.8,
            window_size: 5,
            reduction_ratio: 0.8,
            reason_code: ExtractionReason::Success,
            correlation_id: None,
        };

        stats.record(&result);
        assert_eq!(stats.total_extractions, 1);
        assert_eq!(stats.successful, 1);
        assert!((stats.mean_confidence - 0.8).abs() < f64::EPSILON);
        assert!((stats.mean_reduction - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_record_failure() {
        let mut stats = ExtractionStats::new();
        let result = ExtractionResult {
            commands: Vec::new(),
            score: MdlScore {
                compressed_len: 0,
                uncompressed_len: 0,
                ratio: 0.0,
                command_count: 0,
            },
            confidence: 0.0,
            window_size: 3,
            reduction_ratio: 0.0,
            reason_code: ExtractionReason::NoSuccessFound,
            correlation_id: None,
        };

        stats.record(&result);
        assert_eq!(stats.total_extractions, 1);
        assert_eq!(stats.successful, 0);
        assert_eq!(*stats.reason_counts.get("NoSuccessFound").unwrap(), 1);
    }

    #[test]
    fn stats_running_mean() {
        let mut stats = ExtractionStats::new();
        for conf in [0.6, 0.8, 1.0] {
            let result = ExtractionResult {
                commands: vec![success_block(0, "fix")],
                score: MdlScore {
                    compressed_len: 3,
                    uncompressed_len: 3,
                    ratio: 1.0,
                    command_count: 1,
                },
                confidence: conf,
                window_size: 5,
                reduction_ratio: conf,
                reason_code: ExtractionReason::Success,
                correlation_id: None,
            };
            stats.record(&result);
        }
        assert_eq!(stats.successful, 3);
        assert!((stats.mean_confidence - 0.8).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // ExtractionReason serde
    // -------------------------------------------------------------------------

    #[test]
    fn extraction_reason_serde() {
        for reason in [
            ExtractionReason::Success,
            ExtractionReason::WindowTooSmall,
            ExtractionReason::NoSuccessFound,
            ExtractionReason::AllSuccessful,
            ExtractionReason::LowConfidence,
            ExtractionReason::CandidateLimitReached,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: ExtractionReason = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, reason);
        }
    }

    // -------------------------------------------------------------------------
    // Integration: extract with real-ish scenario
    // -------------------------------------------------------------------------

    #[test]
    fn integration_realistic_recovery() {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });

        // Simulate: agent tries to build, fails, fixes, succeeds.
        let window = vec![
            failure_block(0, "cargo build"),
            failure_block(1, "cargo build"),
            failure_block(2, "cargo build --verbose"),
            success_block(3, "vim src/main.rs"), // edit
            failure_block(4, "cargo build"),
            success_block(5, "cargo fix"),
            success_block(6, "cargo build"),
        ];

        let result = ext.extract(&window);
        assert!(!result.commands.is_empty());
        // The extracted sequence should be smaller than the full window.
        assert!(result.commands.len() <= window.len());
        // Must end with success.
        assert!(result.commands.last().unwrap().is_success());
    }

    #[test]
    fn integration_builder_to_extraction() {
        let mut builder = WindowBuilder::new();
        builder.add_command("whoami".to_string(), Some(0), Some(50), 1000);
        builder.add_command("bad-cmd".to_string(), Some(127), Some(100), 2000);
        builder.add_command("good-cmd".to_string(), Some(0), Some(200), 3000);

        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(builder.blocks());

        assert!(!result.commands.is_empty());
        assert_eq!(result.window_size, 3);
    }

    // -------------------------------------------------------------------------
    // estimate_compressed_len
    // -------------------------------------------------------------------------

    #[test]
    fn compressed_len_short_data() {
        let compressed = estimate_compressed_len(b"ab", 3);
        assert_eq!(compressed, 2); // Too short for trigrams.
    }

    #[test]
    fn compressed_len_zero() {
        let compressed = estimate_compressed_len(b"", 3);
        assert_eq!(compressed, 0);
    }

    #[test]
    fn compressed_len_repetitive() {
        let data = b"aaaaaaaaaaaaaaaaaaaaaaaaa";
        let compressed = estimate_compressed_len(data, 3);
        assert!(compressed < data.len());
    }

    #[test]
    fn compressed_len_never_exceeds_original() {
        let data = b"abc123xyz!@#";
        let compressed = estimate_compressed_len(data, 3);
        assert!(compressed <= data.len());
    }
}
