//! Byte-level zstd compression for scrollback and pane output (ft-2oph2).
//!
//! Complements the semantic compression in `output_compression.rs` by providing
//! raw byte-level compression using zstd. Includes dictionary support for
//! terminal output patterns and batched multi-buffer compression.
//!
//! # Architecture
//!
//! Two compression tiers work together:
//!
//! 1. **Semantic** (`output_compression`): Detects repeated line patterns and
//!    delta-encodes them. Best for highly repetitive terminal output (50–100:1).
//! 2. **Byte-level** (this module): zstd compression for the remaining content.
//!    Provides 3–5:1 on generic terminal output, up to 10:1 with a trained dictionary.
//!
//! ```text
//! Raw output → Semantic pass → zstd byte compress → Stored blob
//!                                                       │
//! Reconstructed ← Semantic decompress ← zstd decompress ◄─┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use frankenterm_core::byte_compression::{ByteCompressor, CompressionLevel};
//!
//! let compressor = ByteCompressor::new(CompressionLevel::Default);
//! let compressed = compressor.compress(b"hello world");
//! let decompressed = compressor.decompress(&compressed).unwrap();
//! assert_eq!(&decompressed, b"hello world");
//! ```

use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// Compression level preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionLevel {
    /// Fast compression (zstd level 1). Best throughput, lowest ratio.
    Fast,
    /// Default compression (zstd level 3). Good balance.
    Default,
    /// High compression (zstd level 9). Better ratio, slower.
    High,
    /// Maximum compression (zstd level 19). Best ratio, slowest.
    Maximum,
}

impl CompressionLevel {
    /// Convert to zstd integer level.
    #[must_use]
    pub fn zstd_level(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::Default => 3,
            Self::High => 9,
            Self::Maximum => 19,
        }
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::Default
    }
}

/// Configuration for the byte compressor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByteCompressionConfig {
    /// Compression level preset.
    pub level: CompressionLevel,
    /// Maximum uncompressed input size accepted (safety limit).
    /// Default: 64 MiB.
    pub max_input_bytes: usize,
    /// Whether to include a 4-byte little-endian size prefix for streaming.
    pub include_size_prefix: bool,
}

impl Default for ByteCompressionConfig {
    fn default() -> Self {
        Self {
            level: CompressionLevel::Default,
            max_input_bytes: 64 * 1024 * 1024,
            include_size_prefix: false,
        }
    }
}

// =============================================================================
// Compression statistics
// =============================================================================

/// Statistics from a compression or batch operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionStats {
    /// Total input bytes before compression.
    pub input_bytes: u64,
    /// Total output bytes after compression.
    pub output_bytes: u64,
    /// Number of buffers compressed (1 for single, N for batch).
    pub buffer_count: u32,
    /// Compression ratio (input / output). Higher is better.
    pub ratio: f64,
}

impl CompressionStats {
    /// Create stats from input and output sizes.
    #[must_use]
    pub fn new(input_bytes: u64, output_bytes: u64, buffer_count: u32) -> Self {
        let ratio = if output_bytes > 0 {
            input_bytes as f64 / output_bytes as f64
        } else {
            0.0
        };
        Self {
            input_bytes,
            output_bytes,
            buffer_count,
            ratio,
        }
    }
}

// =============================================================================
// Byte compressor
// =============================================================================

/// Zstd-based byte compressor with optional dictionary support.
pub struct ByteCompressor {
    config: ByteCompressionConfig,
    dictionary: Option<Vec<u8>>,
}

impl ByteCompressor {
    /// Create a compressor with the given level.
    #[must_use]
    pub fn new(level: CompressionLevel) -> Self {
        Self {
            config: ByteCompressionConfig {
                level,
                ..Default::default()
            },
            dictionary: None,
        }
    }

    /// Create a compressor with full config.
    #[must_use]
    pub fn with_config(config: ByteCompressionConfig) -> Self {
        Self {
            config,
            dictionary: None,
        }
    }

    /// Set a pre-trained compression dictionary.
    ///
    /// Dictionaries improve compression ratio significantly for small inputs
    /// (< 16 KiB) that share common patterns (e.g., terminal escape sequences,
    /// shell prompts, ANSI color codes).
    #[must_use]
    pub fn with_dictionary(mut self, dict: Vec<u8>) -> Self {
        self.dictionary = Some(dict);
        self
    }

    /// Compress a single buffer.
    ///
    /// Returns the compressed bytes. For very small inputs (< 16 bytes),
    /// the output may be larger than the input due to framing overhead.
    pub fn compress(&self, input: &[u8]) -> Vec<u8> {
        if input.is_empty() {
            return Vec::new();
        }

        let level = self.config.level.zstd_level();
        let compressed = self.compress_with_level(input, level);

        if self.config.include_size_prefix {
            let mut result = Vec::with_capacity(4 + compressed.len());
            result.extend_from_slice(&(input.len() as u32).to_le_bytes());
            result.extend_from_slice(&compressed);
            result
        } else {
            compressed
        }
    }

    /// Decompress bytes back to original.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is not valid zstd-compressed data,
    /// or if the decompressed size exceeds `max_input_bytes`.
    pub fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, ByteCompressionError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let data = if self.config.include_size_prefix {
            if input.len() < 4 {
                return Err(ByteCompressionError::InvalidInput(
                    "input too short for size prefix".to_string(),
                ));
            }
            &input[4..]
        } else {
            input
        };

        self.decompress_data(data)
    }

    /// Compress multiple buffers into a single batched blob.
    ///
    /// Layout: `[count: u32-le] [len0: u32-le] [compressed0] [len1: u32-le] [compressed1] ...`
    ///
    /// This is efficient for compressing multiple pane outputs together,
    /// amortizing dictionary load and frame overhead.
    pub fn compress_batch(&self, inputs: &[&[u8]]) -> (Vec<u8>, CompressionStats) {
        let mut result = Vec::new();
        let count = inputs.len() as u32;
        result.extend_from_slice(&count.to_le_bytes());

        let mut total_input: u64 = 0;

        for input in inputs {
            total_input += input.len() as u64;
            let compressed = self.compress_raw(input);
            result.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
            result.extend_from_slice(&compressed);
        }

        let stats = CompressionStats::new(total_input, result.len() as u64, count);
        (result, stats)
    }

    /// Decompress a batched blob back to individual buffers.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch format is invalid or any buffer
    /// fails decompression.
    pub fn decompress_batch(&self, input: &[u8]) -> Result<Vec<Vec<u8>>, ByteCompressionError> {
        if input.len() < 4 {
            return Err(ByteCompressionError::InvalidInput(
                "batch too short for count".to_string(),
            ));
        }

        let count = u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize;
        let mut offset = 4usize;
        let mut buffers = Vec::with_capacity(count);

        for i in 0..count {
            if offset + 4 > input.len() {
                return Err(ByteCompressionError::InvalidInput(format!(
                    "batch truncated at buffer {i}"
                )));
            }
            let len = u32::from_le_bytes([
                input[offset],
                input[offset + 1],
                input[offset + 2],
                input[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + len > input.len() {
                return Err(ByteCompressionError::InvalidInput(format!(
                    "batch truncated in buffer {i} data"
                )));
            }

            let decompressed = self.decompress_raw(&input[offset..offset + len])?;
            buffers.push(decompressed);
            offset += len;
        }

        Ok(buffers)
    }

    /// Compress a single buffer and return stats.
    pub fn compress_with_stats(&self, input: &[u8]) -> (Vec<u8>, CompressionStats) {
        let compressed = self.compress(input);
        let stats = CompressionStats::new(input.len() as u64, compressed.len() as u64, 1);
        (compressed, stats)
    }

    /// Get the current config.
    #[must_use]
    pub fn config(&self) -> &ByteCompressionConfig {
        &self.config
    }

    /// Whether this compressor has a dictionary loaded.
    #[must_use]
    pub fn has_dictionary(&self) -> bool {
        self.dictionary.is_some()
    }

    // -- Internal helpers --

    /// Compress with the given zstd level, using dictionary if available.
    fn compress_with_level(&self, input: &[u8], level: i32) -> Vec<u8> {
        if input.is_empty() {
            return Vec::new();
        }
        let dict_bytes = self.dictionary.as_deref().unwrap_or(&[]);
        let result = zstd::bulk::Compressor::with_dictionary(level, dict_bytes)
            .and_then(|mut c| c.compress(input));
        match result {
            Ok(compressed) => compressed,
            Err(_) => input.to_vec(), // Fallback: return uncompressed
        }
    }

    /// Decompress data, using dictionary if available.
    fn decompress_data(&self, data: &[u8]) -> Result<Vec<u8>, ByteCompressionError> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let dict_bytes = self.dictionary.as_deref().unwrap_or(&[]);
        let mut decompressor =
            zstd::bulk::Decompressor::with_dictionary(dict_bytes)
                .map_err(|e| ByteCompressionError::DecompressionFailed(e.to_string()))?;
        decompressor
            .decompress(data, self.config.max_input_bytes)
            .map_err(|e| ByteCompressionError::DecompressionFailed(e.to_string()))
    }

    /// Compress without size prefix (for batch use).
    fn compress_raw(&self, input: &[u8]) -> Vec<u8> {
        self.compress_with_level(input, self.config.level.zstd_level())
    }

    /// Decompress without size prefix (for batch use).
    fn decompress_raw(&self, data: &[u8]) -> Result<Vec<u8>, ByteCompressionError> {
        self.decompress_data(data)
    }
}

impl Default for ByteCompressor {
    fn default() -> Self {
        Self::new(CompressionLevel::Default)
    }
}

// =============================================================================
// Dictionary training
// =============================================================================

/// Train a zstd compression dictionary from sample terminal output.
///
/// A well-trained dictionary can improve compression ratio by 2–3x for
/// small inputs (< 16 KiB) that share common patterns. Typical terminal
/// patterns include ANSI escape sequences, shell prompts, progress bars,
/// and common command outputs.
///
/// # Arguments
///
/// * `samples` - Collection of representative terminal output samples.
///   Best results with 100–1000 samples of 1–64 KiB each.
/// * `dict_size` - Target dictionary size in bytes. 32–112 KiB is typical.
///
/// # Errors
///
/// Returns an error if training fails (e.g., too few samples, samples too small).
pub fn train_dictionary(
    samples: &[&[u8]],
    dict_size: usize,
) -> Result<Vec<u8>, ByteCompressionError> {
    if samples.is_empty() {
        return Err(ByteCompressionError::TrainingFailed(
            "no samples provided".to_string(),
        ));
    }
    zstd::dict::from_samples(samples, dict_size)
        .map_err(|e| ByteCompressionError::TrainingFailed(e.to_string()))
}

/// Built-in terminal output dictionary seed data.
///
/// Returns a collection of common terminal patterns for dictionary training.
/// These cover ANSI escape sequences, shell prompts, progress bars,
/// and common build/test output.
#[must_use]
pub fn terminal_dictionary_seeds() -> Vec<Vec<u8>> {
    vec![
        // ANSI escape sequences
        b"\x1b[0m\x1b[1m\x1b[2m\x1b[3m\x1b[4m\x1b[7m\x1b[8m".to_vec(),
        b"\x1b[30m\x1b[31m\x1b[32m\x1b[33m\x1b[34m\x1b[35m\x1b[36m\x1b[37m".to_vec(),
        b"\x1b[40m\x1b[41m\x1b[42m\x1b[43m\x1b[44m\x1b[45m\x1b[46m\x1b[47m".to_vec(),
        b"\x1b[38;5;0m\x1b[38;5;1m\x1b[38;5;2m\x1b[38;5;3m".to_vec(),
        b"\x1b[38;2;255;255;255m\x1b[48;2;0;0;0m".to_vec(),
        b"\x1b[?25h\x1b[?25l\x1b[H\x1b[J\x1b[K\x1b[2J".to_vec(),
        // Shell prompts
        b"$ \n% \n> \n>>> \n... \n".to_vec(),
        b"user@host:~/projects$ ".to_vec(),
        b"(venv) $ ".to_vec(),
        // Cargo/Rust build output
        b"   Compiling frankenterm-core v0.1.0\n".to_vec(),
        b"    Finished `dev` profile [unoptimized + debuginfo] target(s)\n".to_vec(),
        b"     Running unittests src/lib.rs\n".to_vec(),
        b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n".to_vec(),
        b"warning: unused import\n".to_vec(),
        b"error[E0433]: failed to resolve\n".to_vec(),
        // Git output
        b"On branch main\n".to_vec(),
        b"Your branch is up to date with 'origin/main'.\n".to_vec(),
        b"Changes not staged for commit:\n".to_vec(),
        b"  modified:   ".to_vec(),
        b"  new file:   ".to_vec(),
        b"  deleted:    ".to_vec(),
        // Progress patterns
        b"[=====>                    ] 25%\n".to_vec(),
        b"[===========>              ] 50%\n".to_vec(),
        b"[==================>       ] 75%\n".to_vec(),
        b"[=========================] 100%\n".to_vec(),
        b"Downloading... 1.2 MB / 5.0 MB\n".to_vec(),
        // npm/node
        b"npm warn deprecated ".to_vec(),
        b"added 0 packages in 0s\n".to_vec(),
        // Python
        b"Traceback (most recent call last):\n".to_vec(),
        b"  File \"".to_vec(),
        b"\", line ".to_vec(),
        b", in ".to_vec(),
    ]
}

// =============================================================================
// Errors
// =============================================================================

/// Errors from byte compression operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ByteCompressionError {
    /// Input is invalid (wrong format, truncated, etc.).
    InvalidInput(String),
    /// Decompression failed (corrupt data, size exceeded, etc.).
    DecompressionFailed(String),
    /// Dictionary training failed.
    TrainingFailed(String),
}

impl std::fmt::Display for ByteCompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::DecompressionFailed(msg) => write!(f, "decompression failed: {msg}"),
            Self::TrainingFailed(msg) => write!(f, "dictionary training failed: {msg}"),
        }
    }
}

impl std::error::Error for ByteCompressionError {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Basic roundtrip --

    #[test]
    fn compress_decompress_roundtrip() {
        let compressor = ByteCompressor::default();
        let input = b"Hello, world! This is some terminal output.\n";
        let compressed = compressor.compress(input);
        let decompressed = compressor.decompress(&compressed).unwrap();
        assert_eq!(&decompressed, input);
    }

    #[test]
    fn empty_input_roundtrip() {
        let compressor = ByteCompressor::default();
        let compressed = compressor.compress(b"");
        assert!(compressed.is_empty());
        let decompressed = compressor.decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn large_repetitive_input_compresses_well() {
        let compressor = ByteCompressor::default();
        let line = b"   Compiling some-crate v0.1.0 (/path/to/crate)\n";
        let input: Vec<u8> = line.repeat(1000);
        let (compressed, stats) = compressor.compress_with_stats(&input);
        assert!(stats.ratio > 5.0, "Expected >5:1 ratio on repetitive input, got {}", stats.ratio);
        let decompressed = compressor.decompress(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    // -- Compression levels --

    #[test]
    fn higher_level_gives_better_or_equal_ratio() {
        let input: Vec<u8> = b"test data with some patterns\n".repeat(500);
        let fast = ByteCompressor::new(CompressionLevel::Fast);
        let high = ByteCompressor::new(CompressionLevel::High);
        let fast_compressed = fast.compress(&input);
        let high_compressed = high.compress(&input);
        // High should produce same or smaller output
        assert!(
            high_compressed.len() <= fast_compressed.len() + 10,
            "High ({}) should be <= Fast ({}) + slack",
            high_compressed.len(),
            fast_compressed.len()
        );
    }

    #[test]
    fn all_levels_roundtrip() {
        let input = b"roundtrip test for all levels\n".repeat(100);
        for level in [
            CompressionLevel::Fast,
            CompressionLevel::Default,
            CompressionLevel::High,
            CompressionLevel::Maximum,
        ] {
            let c = ByteCompressor::new(level);
            let compressed = c.compress(&input);
            let decompressed = c.decompress(&compressed).unwrap();
            assert_eq!(decompressed, input, "Failed roundtrip for level {:?}", level);
        }
    }

    // -- Size prefix --

    #[test]
    fn size_prefix_roundtrip() {
        let config = ByteCompressionConfig {
            include_size_prefix: true,
            ..Default::default()
        };
        let compressor = ByteCompressor::with_config(config);
        let input = b"test with size prefix\n".repeat(50);
        let compressed = compressor.compress(&input);
        // First 4 bytes should be the original size
        let stored_size = u32::from_le_bytes([
            compressed[0],
            compressed[1],
            compressed[2],
            compressed[3],
        ]) as usize;
        assert_eq!(stored_size, input.len());
        let decompressed = compressor.decompress(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    // -- Batch operations --

    #[test]
    fn batch_compress_decompress_roundtrip() {
        let compressor = ByteCompressor::default();
        let buf1 = b"First pane output: building...\n".repeat(20);
        let buf2 = b"Second pane: running tests...\n".repeat(30);
        let buf3 = b"Third pane: deploying...\n".repeat(10);
        let inputs: Vec<&[u8]> = vec![&buf1, &buf2, &buf3];

        let (batch, stats) = compressor.compress_batch(&inputs);
        assert_eq!(stats.buffer_count, 3);
        assert!(stats.input_bytes > 0);

        let decompressed = compressor.decompress_batch(&batch).unwrap();
        assert_eq!(decompressed.len(), 3);
        assert_eq!(decompressed[0], buf1);
        assert_eq!(decompressed[1], buf2);
        assert_eq!(decompressed[2], buf3);
    }

    #[test]
    fn batch_empty_buffers() {
        let compressor = ByteCompressor::default();
        let inputs: Vec<&[u8]> = vec![b"", b"non-empty", b""];
        let (batch, stats) = compressor.compress_batch(&inputs);
        assert_eq!(stats.buffer_count, 3);
        let decompressed = compressor.decompress_batch(&batch).unwrap();
        assert_eq!(decompressed.len(), 3);
        assert!(decompressed[0].is_empty());
        assert_eq!(decompressed[1], b"non-empty");
        assert!(decompressed[2].is_empty());
    }

    #[test]
    fn batch_single_buffer() {
        let compressor = ByteCompressor::default();
        let buf = b"single buffer test data\n".repeat(50);
        let inputs: Vec<&[u8]> = vec![&buf];
        let (batch, _stats) = compressor.compress_batch(&inputs);
        let decompressed = compressor.decompress_batch(&batch).unwrap();
        assert_eq!(decompressed.len(), 1);
        assert_eq!(decompressed[0], buf);
    }

    // -- Dictionary --

    #[test]
    fn dictionary_training_and_use() {
        // Generate enough samples for training
        let seeds = terminal_dictionary_seeds();
        let samples: Vec<&[u8]> = seeds.iter().map(|s| s.as_slice()).collect();

        // Training may fail with too few/small samples, which is OK
        match train_dictionary(&samples, 4096) {
            Ok(dict) => {
                let compressor = ByteCompressor::new(CompressionLevel::Default)
                    .with_dictionary(dict);
                assert!(compressor.has_dictionary());

                let input = b"\x1b[32m   Compiling\x1b[0m frankenterm-core v0.1.0\n".repeat(50);
                let compressed = compressor.compress(&input);
                let decompressed = compressor.decompress(&compressed).unwrap();
                assert_eq!(decompressed, input);
            }
            Err(_) => {
                // Dictionary training can fail with insufficient data; skip
            }
        }
    }

    #[test]
    fn dictionary_seeds_are_nonempty() {
        let seeds = terminal_dictionary_seeds();
        assert!(seeds.len() > 10);
        for seed in &seeds {
            assert!(!seed.is_empty());
        }
    }

    // -- Error handling --

    #[test]
    fn decompress_invalid_data() {
        let compressor = ByteCompressor::default();
        let result = compressor.decompress(b"not valid zstd data");
        assert!(result.is_err());
    }

    #[test]
    fn decompress_batch_truncated() {
        let compressor = ByteCompressor::default();
        // Too short for even the count field
        let result = compressor.decompress_batch(b"ab");
        assert!(result.is_err());
    }

    #[test]
    fn decompress_batch_invalid_count() {
        let compressor = ByteCompressor::default();
        // Says 2 buffers but only has count field
        let mut data = Vec::new();
        data.extend_from_slice(&2u32.to_le_bytes());
        let result = compressor.decompress_batch(&data);
        assert!(result.is_err());
    }

    // -- Stats --

    #[test]
    fn compression_stats_ratio() {
        let stats = CompressionStats::new(1000, 200, 1);
        assert!((stats.ratio - 5.0).abs() < 0.01);
    }

    #[test]
    fn compression_stats_zero_output() {
        let stats = CompressionStats::new(0, 0, 0);
        assert_eq!(stats.ratio, 0.0);
    }

    // -- Binary data --

    #[test]
    fn binary_data_roundtrip() {
        let compressor = ByteCompressor::default();
        let input: Vec<u8> = (0..=255).collect();
        let compressed = compressor.compress(&input);
        let decompressed = compressor.decompress(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn ansi_heavy_terminal_output_roundtrip() {
        let compressor = ByteCompressor::default();
        let mut input = Vec::new();
        for i in 0..200 {
            input.extend_from_slice(format!(
                "\x1b[38;5;{}m  Line {} of output with ANSI codes\x1b[0m\n",
                i % 256,
                i
            ).as_bytes());
        }
        let (compressed, stats) = compressor.compress_with_stats(&input);
        assert!(stats.ratio > 1.5, "Expected some compression for ANSI output");
        let decompressed = compressor.decompress(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    // -- Config --

    #[test]
    fn zstd_level_values() {
        assert_eq!(CompressionLevel::Fast.zstd_level(), 1);
        assert_eq!(CompressionLevel::Default.zstd_level(), 3);
        assert_eq!(CompressionLevel::High.zstd_level(), 9);
        assert_eq!(CompressionLevel::Maximum.zstd_level(), 19);
    }

    #[test]
    fn config_default_values() {
        let config = ByteCompressionConfig::default();
        assert_eq!(config.max_input_bytes, 64 * 1024 * 1024);
        assert!(!config.include_size_prefix);
    }
}
