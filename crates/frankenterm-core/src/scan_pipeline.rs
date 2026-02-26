//! Unified scan pipeline for pane output processing (ft-2oph2).
//!
//! Orchestrates the three-stage scanning pipeline:
//!
//! 1. **Metrics scan** (`simd_scan`): SIMD-accelerated newline and ANSI density.
//! 2. **Pattern trigger** (`pattern_trigger`): Aho-Corasick multi-pattern match.
//! 3. **Byte compression** (`byte_compression`): zstd compression of raw output.
//!
//! The pipeline can run in two modes:
//!
//! - **Batch mode**: Process a complete buffer at once.
//! - **Chunked mode**: Process output in chunks with cross-boundary state carry,
//!   suitable for streaming ingestion from pane tailers.
//!
//! # Architecture
//!
//! ```text
//! raw bytes ──►  ScanPipeline::process()
//!                 ├── simd_scan::scan_newlines_and_ansi()  ──► OutputScanMetrics
//!                 ├── pattern_trigger::TriggerScanner::scan_counts() ──► TriggerScanResult
//!                 └── byte_compression::ByteCompressor::compress() ──► compressed blob
//!                     └── ScanOutput { metrics, triggers, compressed, stats }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::byte_compression::{ByteCompressor, CompressionLevel, CompressionStats};
use crate::pattern_trigger::{TriggerCategory, TriggerScanResult, TriggerScanner};
use crate::simd_scan::{scan_newlines_and_ansi, scan_newlines_and_ansi_with_state, OutputScanMetrics, OutputScanState};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the scan pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanPipelineConfig {
    /// Whether to run pattern trigger scanning.
    pub enable_triggers: bool,
    /// Whether to compress the output.
    pub enable_compression: bool,
    /// Compression level for byte compression.
    pub compression_level: CompressionLevelConfig,
    /// Minimum bytes to bother compressing (skip for tiny buffers).
    pub compression_threshold: usize,
    /// Whether to run ANSI density analysis.
    pub enable_ansi_analysis: bool,
}

impl Default for ScanPipelineConfig {
    fn default() -> Self {
        Self {
            enable_triggers: true,
            enable_compression: true,
            compression_level: CompressionLevelConfig::Default,
            compression_threshold: 256,
            enable_ansi_analysis: true,
        }
    }
}

/// Serializable mirror of `CompressionLevel`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionLevelConfig {
    Fast,
    Default,
    High,
    Maximum,
}

impl From<CompressionLevelConfig> for CompressionLevel {
    fn from(c: CompressionLevelConfig) -> Self {
        match c {
            CompressionLevelConfig::Fast => CompressionLevel::Fast,
            CompressionLevelConfig::Default => CompressionLevel::Default,
            CompressionLevelConfig::High => CompressionLevel::High,
            CompressionLevelConfig::Maximum => CompressionLevel::Maximum,
        }
    }
}

// =============================================================================
// Output types
// =============================================================================

/// Result of processing a buffer through the scan pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanOutput {
    /// Newline and ANSI density metrics from SIMD scan.
    pub metrics: ScanMetricsSummary,
    /// Pattern trigger results (if enabled).
    pub triggers: Option<TriggerScanResult>,
    /// Compressed output blob (if enabled and above threshold).
    #[serde(skip)]
    pub compressed: Option<Vec<u8>>,
    /// Compression statistics (if compression ran).
    pub compression_stats: Option<CompressionStats>,
    /// Number of input bytes processed.
    pub input_bytes: u64,
}

/// Serializable metrics summary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScanMetricsSummary {
    /// Count of newline bytes.
    pub newline_count: usize,
    /// Count of bytes in ANSI escape sequences.
    pub ansi_byte_count: usize,
    /// Logical line count (text.lines() semantics).
    pub logical_lines: usize,
    /// ANSI density as fraction in [0, 1].
    pub ansi_density: f64,
}

impl ScanMetricsSummary {
    fn from_metrics(metrics: OutputScanMetrics, bytes: &[u8]) -> Self {
        Self {
            newline_count: metrics.newline_count,
            ansi_byte_count: metrics.ansi_byte_count,
            logical_lines: metrics.logical_line_count(bytes),
            ansi_density: metrics.ansi_density(bytes.len()),
        }
    }
}

// =============================================================================
// Chunked state
// =============================================================================

/// Accumulator for chunked (streaming) pipeline processing.
///
/// Tracks cross-chunk state for SIMD scan and aggregates trigger results
/// across multiple chunks.
#[derive(Debug)]
pub struct ChunkedPipelineState {
    /// Cross-boundary ANSI/UTF-8 state.
    scan_state: OutputScanState,
    /// Accumulated metrics across all chunks.
    accumulated_metrics: OutputScanMetrics,
    /// Accumulated trigger counts across all chunks.
    accumulated_triggers: HashMap<TriggerCategory, u64>,
    /// Total trigger matches across all chunks.
    total_trigger_matches: u64,
    /// Total bytes processed.
    total_bytes: u64,
    /// Buffered uncompressed output for batch compression.
    uncompressed_buffer: Vec<u8>,
    /// Maximum buffer size before flushing compression.
    max_buffer_bytes: usize,
}

impl ChunkedPipelineState {
    /// Create a new chunked pipeline state.
    #[must_use]
    pub fn new(max_buffer_bytes: usize) -> Self {
        Self {
            scan_state: OutputScanState::default(),
            accumulated_metrics: OutputScanMetrics::default(),
            accumulated_triggers: HashMap::new(),
            total_trigger_matches: 0,
            total_bytes: 0,
            uncompressed_buffer: Vec::with_capacity(max_buffer_bytes.min(1_048_576)),
            max_buffer_bytes,
        }
    }

    /// Total bytes processed so far.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Accumulated newline count.
    #[must_use]
    pub fn newline_count(&self) -> usize {
        self.accumulated_metrics.newline_count
    }

    /// Accumulated ANSI byte count.
    #[must_use]
    pub fn ansi_byte_count(&self) -> usize {
        self.accumulated_metrics.ansi_byte_count
    }

    /// Whether the buffer is full and should be flushed.
    #[must_use]
    pub fn should_flush(&self) -> bool {
        self.uncompressed_buffer.len() >= self.max_buffer_bytes
    }

    /// Current accumulated trigger counts.
    #[must_use]
    pub fn trigger_counts(&self) -> &HashMap<TriggerCategory, u64> {
        &self.accumulated_triggers
    }

    /// Total trigger matches accumulated.
    #[must_use]
    pub fn total_trigger_matches(&self) -> u64 {
        self.total_trigger_matches
    }

    /// Whether any errors have been detected across all chunks.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.accumulated_triggers
            .get(&TriggerCategory::Error)
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// Whether any completions have been detected across all chunks.
    #[must_use]
    pub fn has_completions(&self) -> bool {
        self.accumulated_triggers
            .get(&TriggerCategory::Completion)
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// Reset all accumulated state.
    pub fn reset(&mut self) {
        self.scan_state.reset();
        self.accumulated_metrics = OutputScanMetrics::default();
        self.accumulated_triggers.clear();
        self.total_trigger_matches = 0;
        self.total_bytes = 0;
        self.uncompressed_buffer.clear();
    }
}

// =============================================================================
// Pipeline
// =============================================================================

/// Unified scan pipeline for pane output processing.
///
/// Holds pre-built scanners and compressor so they can be reused across
/// multiple buffers without reconstruction overhead.
pub struct ScanPipeline {
    config: ScanPipelineConfig,
    trigger_scanner: TriggerScanner,
    compressor: ByteCompressor,
}

impl ScanPipeline {
    /// Create a pipeline with default trigger patterns and the given config.
    #[must_use]
    pub fn new(config: ScanPipelineConfig) -> Self {
        let compressor = ByteCompressor::new(config.compression_level.into());
        Self {
            config,
            trigger_scanner: TriggerScanner::default(),
            compressor,
        }
    }

    /// Create a pipeline with custom trigger patterns.
    #[must_use]
    pub fn with_custom_triggers(
        config: ScanPipelineConfig,
        trigger_scanner: TriggerScanner,
    ) -> Self {
        let compressor = ByteCompressor::new(config.compression_level.into());
        Self {
            config,
            trigger_scanner,
            compressor,
        }
    }

    /// Process a complete buffer through all pipeline stages.
    #[must_use]
    pub fn process(&self, bytes: &[u8]) -> ScanOutput {
        // Stage 1: SIMD metrics scan
        let metrics = scan_newlines_and_ansi(bytes);
        let summary = ScanMetricsSummary::from_metrics(metrics, bytes);

        // Stage 2: Pattern trigger scan
        let triggers = if self.config.enable_triggers {
            Some(self.trigger_scanner.scan_counts(bytes))
        } else {
            None
        };

        // Stage 3: Byte compression
        let (compressed, compression_stats) =
            if self.config.enable_compression && bytes.len() >= self.config.compression_threshold {
                let (blob, stats) = self.compressor.compress_with_stats(bytes);
                (Some(blob), Some(stats))
            } else {
                (None, None)
            };

        ScanOutput {
            metrics: summary,
            triggers,
            compressed,
            compression_stats,
            input_bytes: bytes.len() as u64,
        }
    }

    /// Process a chunk through the pipeline, accumulating state.
    ///
    /// Returns the incremental metrics for this chunk. Full accumulated
    /// state is available on `state`.
    pub fn process_chunk(&self, bytes: &[u8], state: &mut ChunkedPipelineState) -> ScanMetricsSummary {
        // Stage 1: Stateful SIMD metrics scan (cross-boundary aware)
        let chunk_metrics =
            scan_newlines_and_ansi_with_state(bytes, &mut state.scan_state);

        // Accumulate metrics
        state.accumulated_metrics.newline_count += chunk_metrics.newline_count;
        state.accumulated_metrics.ansi_byte_count += chunk_metrics.ansi_byte_count;
        state.total_bytes += bytes.len() as u64;

        // Stage 2: Pattern trigger scan on this chunk
        if self.config.enable_triggers {
            let chunk_triggers = self.trigger_scanner.scan_counts(bytes);
            state.total_trigger_matches += chunk_triggers.total_matches;
            for (cat, count) in &chunk_triggers.counts {
                *state.accumulated_triggers.entry(*cat).or_insert(0) += count;
            }
        }

        // Stage 3: Buffer for batch compression (no per-chunk compression)
        if self.config.enable_compression {
            state.uncompressed_buffer.extend_from_slice(bytes);
        }

        ScanMetricsSummary {
            newline_count: chunk_metrics.newline_count,
            ansi_byte_count: chunk_metrics.ansi_byte_count,
            logical_lines: chunk_metrics.logical_line_count(bytes),
            ansi_density: chunk_metrics.ansi_density(bytes.len()),
        }
    }

    /// Flush accumulated chunked state into a final `ScanOutput`.
    ///
    /// Compresses the buffered data and produces the aggregate result.
    /// The `ChunkedPipelineState` is reset after flushing.
    pub fn flush(&self, state: &mut ChunkedPipelineState) -> ScanOutput {
        let total_bytes = state.total_bytes;
        let ansi_density = if total_bytes > 0 {
            state.accumulated_metrics.ansi_byte_count as f64 / total_bytes as f64
        } else {
            0.0
        };

        let summary = ScanMetricsSummary {
            newline_count: state.accumulated_metrics.newline_count,
            ansi_byte_count: state.accumulated_metrics.ansi_byte_count,
            logical_lines: state.accumulated_metrics.newline_count, // approximate for chunked
            ansi_density,
        };

        let triggers = if self.config.enable_triggers {
            Some(TriggerScanResult {
                counts: state.accumulated_triggers.clone(),
                total_matches: state.total_trigger_matches,
                bytes_scanned: total_bytes,
            })
        } else {
            None
        };

        let (compressed, compression_stats) = if self.config.enable_compression
            && !state.uncompressed_buffer.is_empty()
            && state.uncompressed_buffer.len() >= self.config.compression_threshold
        {
            let (blob, comp_stats) = self.compressor.compress_with_stats(&state.uncompressed_buffer);
            (Some(blob), Some(comp_stats))
        } else {
            (None, None)
        };

        state.reset();

        ScanOutput {
            metrics: summary,
            triggers,
            compressed,
            compression_stats,
            input_bytes: total_bytes,
        }
    }

    /// Access the trigger scanner for direct use.
    #[must_use]
    pub fn trigger_scanner(&self) -> &TriggerScanner {
        &self.trigger_scanner
    }

    /// Access the compressor for direct use.
    #[must_use]
    pub fn compressor(&self) -> &ByteCompressor {
        &self.compressor
    }

    /// Access the pipeline configuration.
    #[must_use]
    pub fn config(&self) -> &ScanPipelineConfig {
        &self.config
    }
}

impl Default for ScanPipeline {
    fn default() -> Self {
        Self::new(ScanPipelineConfig::default())
    }
}

// =============================================================================
// Convenience functions
// =============================================================================

/// Quick scan of a buffer with default settings.
///
/// Creates a default pipeline and processes the buffer. For repeated use,
/// prefer creating a `ScanPipeline` and reusing it.
#[must_use]
pub fn quick_scan(bytes: &[u8]) -> ScanOutput {
    ScanPipeline::default().process(bytes)
}

/// Quick metrics-only scan (no triggers, no compression).
#[must_use]
pub fn quick_metrics(bytes: &[u8]) -> ScanMetricsSummary {
    let metrics = scan_newlines_and_ansi(bytes);
    ScanMetricsSummary::from_metrics(metrics, bytes)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern_trigger::TriggerPattern;

    // -----------------------------------------------------------------------
    // Basic pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn default_pipeline_processes_empty_input() {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(b"");
        assert_eq!(output.input_bytes, 0);
        assert_eq!(output.metrics.newline_count, 0);
        assert_eq!(output.metrics.ansi_byte_count, 0);
        assert_eq!(output.metrics.logical_lines, 0);
        assert!(output.compressed.is_none()); // below threshold
    }

    #[test]
    fn pipeline_detects_newlines() {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(b"line1\nline2\nline3\n");
        assert_eq!(output.metrics.newline_count, 3);
        assert_eq!(output.metrics.logical_lines, 3);
    }

    #[test]
    fn pipeline_detects_ansi() {
        let pipeline = ScanPipeline::default();
        let data = b"\x1b[32mOK\x1b[0m\n";
        let output = pipeline.process(data);
        assert!(output.metrics.ansi_byte_count > 0);
        assert!(output.metrics.ansi_density > 0.0);
    }

    #[test]
    fn pipeline_detects_triggers() {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(b"ERROR: connection refused\n   Compiling serde\n");
        let triggers = output.triggers.as_ref().unwrap();
        assert!(triggers.has_errors());
        assert!(triggers.total_matches >= 2); // ERROR + Compiling
    }

    #[test]
    fn pipeline_compresses_above_threshold() {
        let data = "hello world\n".repeat(100);
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(data.as_bytes());
        assert!(output.compressed.is_some());
        let stats = output.compression_stats.as_ref().unwrap();
        assert_eq!(stats.input_bytes, data.len() as u64);
        assert!(stats.output_bytes < stats.input_bytes);
    }

    #[test]
    fn pipeline_skips_compression_below_threshold() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            compression_threshold: 1024,
            ..Default::default()
        });
        let output = pipeline.process(b"short");
        assert!(output.compressed.is_none());
        assert!(output.compression_stats.is_none());
    }

    #[test]
    fn pipeline_with_triggers_disabled() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_triggers: false,
            ..Default::default()
        });
        let output = pipeline.process(b"ERROR: something\n");
        assert!(output.triggers.is_none());
    }

    #[test]
    fn pipeline_with_compression_disabled() {
        let data = "hello\n".repeat(200);
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let output = pipeline.process(data.as_bytes());
        assert!(output.compressed.is_none());
        assert!(output.compression_stats.is_none());
    }

    #[test]
    fn pipeline_with_custom_triggers() {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new("XYZZY", TriggerCategory::Custom),
        ]);
        let pipeline = ScanPipeline::with_custom_triggers(
            ScanPipelineConfig::default(),
            scanner,
        );
        let output = pipeline.process(b"XYZZY detected\n");
        let triggers = output.triggers.as_ref().unwrap();
        let custom = triggers.get(&TriggerCategory::Custom).copied().unwrap_or(0);
        assert_eq!(custom, 1);
    }

    // -----------------------------------------------------------------------
    // Chunked pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn chunked_pipeline_accumulates_metrics() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(1_048_576);

        pipeline.process_chunk(b"line1\nline2\n", &mut state);
        assert_eq!(state.newline_count(), 2);

        pipeline.process_chunk(b"line3\n", &mut state);
        assert_eq!(state.newline_count(), 3);
        assert_eq!(state.total_bytes(), 18);
    }

    #[test]
    fn chunked_pipeline_accumulates_triggers() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(1_048_576);

        pipeline.process_chunk(b"ERROR: failure\n", &mut state);
        assert!(state.has_errors());
        assert!(!state.has_completions());

        pipeline.process_chunk(b"    Finished `dev` profile\n", &mut state);
        assert!(state.has_errors());
        assert!(state.has_completions());
        assert!(state.total_trigger_matches() >= 2);
    }

    #[test]
    fn chunked_pipeline_flush_resets() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(1_048_576);

        let data = "error line\n".repeat(50);
        pipeline.process_chunk(data.as_bytes(), &mut state);
        assert!(state.total_bytes() > 0);

        let output = pipeline.flush(&mut state);
        assert!(output.input_bytes > 0);

        // State should be reset
        assert_eq!(state.total_bytes(), 0);
        assert_eq!(state.newline_count(), 0);
        assert!(!state.has_errors());
    }

    #[test]
    fn chunked_pipeline_flush_compresses() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(1_048_576);

        let data = "hello world output line\n".repeat(100);
        pipeline.process_chunk(data.as_bytes(), &mut state);

        let output = pipeline.flush(&mut state);
        assert!(output.compressed.is_some());
        assert!(output.compression_stats.is_some());
    }

    #[test]
    fn chunked_pipeline_cross_boundary_ansi() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(1_048_576);

        // Split ANSI escape across chunks: "\x1b[31" | "m red\x1b[0m"
        pipeline.process_chunk(b"text\x1b[31", &mut state);
        pipeline.process_chunk(b"mred\x1b[0m\n", &mut state);

        assert_eq!(state.newline_count(), 1);
        assert!(state.ansi_byte_count() > 0);
    }

    #[test]
    fn chunked_should_flush_respects_max_buffer() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(100);

        pipeline.process_chunk(b"short\n", &mut state);
        assert!(!state.should_flush());

        let big_chunk = vec![b'x'; 100];
        pipeline.process_chunk(&big_chunk, &mut state);
        assert!(state.should_flush());
    }

    // -----------------------------------------------------------------------
    // Convenience function tests
    // -----------------------------------------------------------------------

    #[test]
    fn quick_scan_works() {
        let output = quick_scan(b"ERROR: oops\nDone\n");
        assert_eq!(output.metrics.newline_count, 2);
        assert!(output.triggers.as_ref().unwrap().has_errors());
    }

    #[test]
    fn quick_metrics_works() {
        let summary = quick_metrics(b"line1\n\x1b[0mline2\n");
        assert_eq!(summary.newline_count, 2);
        assert!(summary.ansi_byte_count > 0);
    }

    // -----------------------------------------------------------------------
    // Config serialization
    // -----------------------------------------------------------------------

    #[test]
    fn config_serde_roundtrip() {
        let config = ScanPipelineConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let rt: ScanPipelineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.enable_triggers, config.enable_triggers);
        assert_eq!(rt.enable_compression, config.enable_compression);
        assert_eq!(rt.compression_threshold, config.compression_threshold);
    }

    #[test]
    fn output_serde_roundtrip() {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(b"hello\n");
        let json = serde_json::to_string(&output).unwrap();
        let rt: ScanOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.input_bytes, output.input_bytes);
        assert_eq!(rt.metrics.newline_count, output.metrics.newline_count);
    }

    // -----------------------------------------------------------------------
    // Batch vs chunked consistency
    // -----------------------------------------------------------------------

    #[test]
    fn batch_and_chunked_agree_on_line_aligned_chunks() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });

        // Split at line boundaries so trigger patterns are never bisected.
        let chunk1 = b"ERROR: oops\n";
        let chunk2 = b"Compiling serde\n";
        let chunk3 = b"Finished dev\nline4\nWARNING: x\n";
        let mut full = Vec::new();
        full.extend_from_slice(chunk1);
        full.extend_from_slice(chunk2);
        full.extend_from_slice(chunk3);

        // Batch
        let batch_output = pipeline.process(&full);

        // Chunked — line-aligned boundaries
        let mut state = ChunkedPipelineState::new(1_048_576);
        pipeline.process_chunk(chunk1, &mut state);
        pipeline.process_chunk(chunk2, &mut state);
        pipeline.process_chunk(chunk3, &mut state);
        let chunked_output = pipeline.flush(&mut state);

        // Metrics should agree
        assert_eq!(
            batch_output.metrics.newline_count,
            chunked_output.metrics.newline_count
        );
        assert_eq!(
            batch_output.metrics.ansi_byte_count,
            chunked_output.metrics.ansi_byte_count
        );

        // Trigger totals agree when chunks are line-aligned
        let batch_triggers = batch_output.triggers.unwrap();
        let chunked_triggers = chunked_output.triggers.unwrap();
        assert_eq!(batch_triggers.total_matches, chunked_triggers.total_matches);
    }

    #[test]
    fn chunked_may_miss_split_patterns() {
        // When chunks split in the middle of a keyword, the chunked pipeline
        // may find fewer matches than batch mode. This is expected behavior.
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });

        let data = b"ERROR: oops\nCompiling serde\n";
        let batch_output = pipeline.process(data);
        let batch_total = batch_output.triggers.unwrap().total_matches;

        // Split "Compiling" across chunks: "Comp" | "iling"
        let mut state = ChunkedPipelineState::new(1_048_576);
        pipeline.process_chunk(&data[..16], &mut state); // "ERROR: oops\nComp"
        pipeline.process_chunk(&data[16..], &mut state); // "iling serde\n"
        let chunked_output = pipeline.flush(&mut state);
        let chunked_total = chunked_output.triggers.unwrap().total_matches;

        // Chunked should find <= batch (split pattern missed)
        assert!(chunked_total <= batch_total);
    }
}
