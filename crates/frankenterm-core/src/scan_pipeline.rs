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

use crate::byte_compression::{ByteCompressor, CompressionLevel, CompressionStats};
use crate::pattern_trigger::{
    TriggerCategory, TriggerCategoryCounts, TriggerScanResult, TriggerScanner,
};
use crate::simd_scan::{
    OutputScanMetrics, OutputScanState, scan_newlines_and_ansi, scan_newlines_and_ansi_with_state,
};

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
///
/// br-ft-u139v: the `compressed` blob is intentionally not part of
/// the wire form — JSON-encoding raw compressed bytes is wasteful
/// and most consumers only need the stats. To keep the contract
/// honest, the serialized form carries `compressed_omitted: true`
/// when a runtime blob existed but was stripped, so receivers can
/// distinguish "compression didn't run" from "blob lost to wire
/// format". `compressed_omitted` deserializes to `false` if missing
/// (backwards-compat with persisted snapshots written before this
/// fix).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanOutput {
    /// Newline and ANSI density metrics from SIMD scan.
    pub metrics: ScanMetricsSummary,
    /// Pattern trigger results (if enabled).
    pub triggers: Option<TriggerScanResult>,
    /// Compressed output blob (if enabled and above threshold).
    #[serde(skip)]
    pub compressed: Option<Vec<u8>>,
    /// br-ft-u139v: when serializing, set to `true` iff
    /// `compressed` was `Some(...)` at construction time. Lets
    /// downstream consumers distinguish "compression never ran" (
    /// `compression_stats == None && compressed_omitted == false`)
    /// from "blob existed but was stripped from the wire form" (
    /// `compression_stats == Some && compressed_omitted == true`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compressed_omitted: bool,
    /// Compression statistics (if compression ran).
    pub compression_stats: Option<CompressionStats>,
    /// Number of input bytes processed.
    pub input_bytes: u64,
}

impl ScanOutput {
    /// br-ft-u139v: set `compressed_omitted` consistently with
    /// `compressed.is_some()` at the construction sites. Called by
    /// [`ScanPipeline::process`] and [`ChunkedPipeline::flush`] so
    /// the wire-truth invariant holds without duplicating the
    /// mapping in every caller.
    pub(crate) fn with_consistent_compressed_marker(mut self) -> Self {
        self.compressed_omitted = self.compressed.is_some();
        self
    }
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
    /// Accumulated trigger counts across all chunks (incremental, approximate).
    accumulated_triggers: TriggerCategoryCounts,
    /// Total trigger matches across all chunks (incremental, approximate).
    total_trigger_matches: u64,
    /// Total bytes processed.
    total_bytes: u64,
    /// Whether any non-empty chunk has been observed.
    saw_any_bytes: bool,
    /// Whether the latest non-empty chunk ended with a newline.
    ends_with_newline: bool,
    /// Buffered uncompressed output for batch compression.
    uncompressed_buffer: Vec<u8>,
    /// Sliding overlap window used to recover trigger matches split across chunks.
    trigger_overlap_buffer: Vec<u8>,
    /// Scratch buffer reused for overlap-aware trigger scans.
    ///
    /// This avoids a fresh overlap+chunk allocation on every chunk in the
    /// hot path while keeping the trigger scan logic identical to batch mode.
    trigger_scan_buffer: Vec<u8>,
    /// Accumulated raw data for definitive trigger scan at flush time when
    /// compression is disabled.
    ///
    /// The incremental overlap-based trigger scan is approximate because
    /// Aho-Corasick LeftmostFirst non-overlapping matching is not composable
    /// across chunk boundaries. At flush time, we re-scan the full accumulated
    /// bytes in batch mode for exact parity with `process()`. When compression
    /// is enabled, `uncompressed_buffer` already holds those bytes, so this
    /// buffer stays empty to avoid duplicate accumulation.
    trigger_data_buffer: Vec<u8>,
    /// Maximum buffer size before flushing compression.
    max_buffer_bytes: usize,
}

/// br-ft-om7iu: structured errors returned by the fallible
/// chunked-pipeline API. Replaces the previous `assert!`-based
/// backpressure path that aborted the caller process when the
/// flush contract was violated. The chunked pipeline is on the
/// streaming pane-ingestion hot path; backpressure must be a
/// recoverable condition, not a hard panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkedPipelineError {
    /// `should_flush()` returned true and the caller appended a
    /// non-empty chunk anyway. The current chunk has NOT been
    /// applied — the caller must drain via `flush(state)` and
    /// retry.
    FlushRequired,
    /// `max_buffer_bytes` was zero or below the configured floor;
    /// a state with that limit would be permanently flush-pending
    /// and cannot accept any non-empty chunk. Construct with a
    /// non-zero limit.
    InvalidBufferLimit { provided: usize, minimum: usize },
}

impl std::fmt::Display for ChunkedPipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FlushRequired => write!(
                f,
                "br-ft-om7iu: scan_pipeline chunked state requires flush before more bytes can be appended"
            ),
            Self::InvalidBufferLimit { provided, minimum } => write!(
                f,
                "br-ft-om7iu: scan_pipeline ChunkedPipelineState requires max_buffer_bytes >= {minimum}; got {provided}"
            ),
        }
    }
}

impl std::error::Error for ChunkedPipelineError {}

/// br-ft-om7iu: minimum acceptable `max_buffer_bytes`. Any value
/// below this would leave a freshly-constructed state immediately
/// flush-pending (`should_flush()` true) and unable to accept any
/// non-empty chunk. The floor of 1 byte is the smallest value
/// that still permits at least one byte of progress per
/// flush cycle.
pub const MIN_CHUNKED_BUFFER_BYTES: usize = 1;

impl ChunkedPipelineState {
    /// Create a new chunked pipeline state.
    ///
    /// br-ft-om7iu: this is the legacy infallible constructor.
    /// Panics on `max_buffer_bytes < MIN_CHUNKED_BUFFER_BYTES`.
    /// Callers loading the limit from config / IPC should prefer
    /// [`ChunkedPipelineState::try_new`] which returns
    /// `Err(InvalidBufferLimit)` instead of panicking.
    #[must_use]
    pub fn new(max_buffer_bytes: usize) -> Self {
        match Self::try_new(max_buffer_bytes) {
            Ok(state) => state,
            Err(err) => panic!("ChunkedPipelineState::new: {err}"),
        }
    }

    /// br-ft-om7iu: fallible constructor. Returns
    /// [`ChunkedPipelineError::InvalidBufferLimit`] when
    /// `max_buffer_bytes < MIN_CHUNKED_BUFFER_BYTES` so callers
    /// loading the limit from external configuration can surface
    /// a recoverable error instead of panicking.
    pub fn try_new(max_buffer_bytes: usize) -> std::result::Result<Self, ChunkedPipelineError> {
        if max_buffer_bytes < MIN_CHUNKED_BUFFER_BYTES {
            return Err(ChunkedPipelineError::InvalidBufferLimit {
                provided: max_buffer_bytes,
                minimum: MIN_CHUNKED_BUFFER_BYTES,
            });
        }
        Ok(Self {
            scan_state: OutputScanState::default(),
            accumulated_metrics: OutputScanMetrics::default(),
            accumulated_triggers: TriggerCategoryCounts::new(),
            total_trigger_matches: 0,
            total_bytes: 0,
            saw_any_bytes: false,
            ends_with_newline: false,
            uncompressed_buffer: Vec::with_capacity(max_buffer_bytes.min(1_048_576)),
            trigger_overlap_buffer: Vec::new(),
            trigger_scan_buffer: Vec::new(),
            trigger_data_buffer: Vec::new(),
            max_buffer_bytes,
        })
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
    ///
    /// Checks whichever raw-data buffer is active for the current config:
    /// `uncompressed_buffer` when compression is enabled, or
    /// `trigger_data_buffer` when only trigger replay needs full raw bytes.
    /// Once this returns true, callers must flush before appending another
    /// non-empty chunk.
    #[must_use]
    pub fn should_flush(&self) -> bool {
        self.uncompressed_buffer.len() >= self.max_buffer_bytes
            || self.trigger_data_buffer.len() >= self.max_buffer_bytes
    }

    /// Current accumulated trigger counts (ft-6db1t: array-backed,
    /// indexed by [`TriggerCategory::as_index`]).
    #[must_use]
    pub fn trigger_counts(&self) -> &TriggerCategoryCounts {
        &self.accumulated_triggers
    }

    /// Total trigger matches accumulated.
    #[must_use]
    pub fn total_trigger_matches(&self) -> u64 {
        self.total_trigger_matches
    }

    /// Logical line count using the same semantics as batch scanning.
    #[must_use]
    pub fn logical_lines(&self) -> usize {
        if !self.saw_any_bytes {
            return 0;
        }

        if self.ends_with_newline {
            self.accumulated_metrics.newline_count
        } else {
            self.accumulated_metrics.newline_count + 1
        }
    }

    /// Whether any errors have been detected across all chunks.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.accumulated_triggers.count(TriggerCategory::Error) > 0
    }

    /// Whether any completions have been detected across all chunks.
    #[must_use]
    pub fn has_completions(&self) -> bool {
        self.accumulated_triggers.count(TriggerCategory::Completion) > 0
    }

    /// Reset all accumulated state.
    pub fn reset(&mut self) {
        self.scan_state.reset();
        self.accumulated_metrics = OutputScanMetrics::default();
        self.accumulated_triggers.clear();
        self.total_trigger_matches = 0;
        self.total_bytes = 0;
        self.saw_any_bytes = false;
        self.ends_with_newline = false;
        self.uncompressed_buffer.clear();
        self.trigger_overlap_buffer.clear();
        self.trigger_scan_buffer.clear();
        self.trigger_data_buffer.clear();
    }
}

fn update_trigger_overlap_buffer(overlap: &mut Vec<u8>, bytes: &[u8], max_overlap: usize) {
    if max_overlap == 0 {
        overlap.clear();
        return;
    }

    let tail_from_bytes = bytes.len().min(max_overlap);
    let keep_from_overlap = max_overlap.saturating_sub(tail_from_bytes);

    if keep_from_overlap > 0 && !overlap.is_empty() {
        let overlap_start = overlap.len().saturating_sub(keep_from_overlap);
        overlap.drain(..overlap_start);
    } else {
        overlap.clear();
    }

    if tail_from_bytes > 0 {
        let byte_start = bytes.len() - tail_from_bytes;
        overlap.extend_from_slice(&bytes[byte_start..]);
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
    trigger_overlap_bytes: usize,
    compressor: ByteCompressor,
}

impl ScanPipeline {
    /// Create a pipeline with default trigger patterns and the given config.
    #[must_use]
    pub fn new(config: ScanPipelineConfig) -> Self {
        let compressor = ByteCompressor::new(config.compression_level.into());
        let trigger_scanner = TriggerScanner::default();
        let trigger_overlap_bytes = Self::compute_trigger_overlap_bytes(&trigger_scanner);
        Self {
            config,
            trigger_scanner,
            trigger_overlap_bytes,
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
        let trigger_overlap_bytes = Self::compute_trigger_overlap_bytes(&trigger_scanner);
        Self {
            config,
            trigger_scanner,
            trigger_overlap_bytes,
            compressor,
        }
    }

    /// br-ft-djjnj: fallible custom-trigger constructor.
    ///
    /// Builds the [`TriggerScanner`] via [`TriggerScanner::try_new`]
    /// (which validates pattern non-emptiness + length cap) and then
    /// the pipeline. Use this from any code path that loads custom
    /// triggers from a non-trusted source — the infallible
    /// [`Self::with_custom_triggers`] is reserved for callers that
    /// have already validated their pattern set.
    pub fn try_with_custom_triggers(
        config: ScanPipelineConfig,
        patterns: Vec<crate::pattern_trigger::TriggerPattern>,
        max_pattern_len: usize,
    ) -> Result<Self, crate::pattern_trigger::TriggerScannerError> {
        let trigger_scanner = TriggerScanner::try_new(patterns, max_pattern_len)?;
        Ok(Self::with_custom_triggers(config, trigger_scanner))
    }

    fn compute_trigger_overlap_bytes(trigger_scanner: &TriggerScanner) -> usize {
        trigger_scanner
            .patterns()
            .iter()
            .map(|pattern| pattern.pattern.len())
            .max()
            .unwrap_or(0)
            .saturating_sub(1)
    }

    fn definitive_trigger_bytes<'a>(&self, state: &'a ChunkedPipelineState) -> &'a [u8] {
        if self.config.enable_compression {
            &state.uncompressed_buffer
        } else {
            &state.trigger_data_buffer
        }
    }

    fn scan_chunk_triggers_with_overlap(
        &self,
        bytes: &[u8],
        overlap: &[u8],
        scratch: &mut Vec<u8>,
    ) -> TriggerScanResult {
        if overlap.is_empty() {
            return self.trigger_scanner.scan_counts(bytes);
        }

        scratch.clear();
        scratch.reserve(overlap.len().saturating_add(bytes.len()));
        scratch.extend_from_slice(overlap);
        scratch.extend_from_slice(bytes);

        let overlap_len = overlap.len();
        let mut result = TriggerScanResult {
            bytes_scanned: bytes.len() as u64,
            ..Default::default()
        };

        // ft-iwlsh: callback-driven scan eliminates the per-call Vec
        // allocation that the old `scan_locate(scratch)` wrapper performed.
        // Combined with ft-6db1t's array-indexed counter, the per-chunk
        // hot path now does zero hashing AND zero allocation.
        self.trigger_scanner
            .for_each_leftmost_match(scratch, |matched| {
                if matched.offset.saturating_add(matched.length) <= overlap_len {
                    return;
                }
                result.counts.add(matched.category, 1);
                result.total_matches += 1;
            });

        result
    }

    /// Process a complete buffer through all pipeline stages.
    #[must_use]
    pub fn process(&self, bytes: &[u8]) -> ScanOutput {
        // Stage 1: SIMD metrics scan
        let mut metrics = scan_newlines_and_ansi(bytes);
        // [ft-5m5xc] Respect enable_ansi_analysis. The SIMD scan
        // always computes both newline_count and ansi_byte_count in
        // one pass; when the operator opts out of ANSI analysis we
        // zero the ANSI portion before it flows into the
        // ScanMetricsSummary / downstream density math, so the
        // documented "disable ANSI analysis" switch actually
        // surfaces clean ansi_byte_count=0 / ansi_density=0.
        // Newline and logical-line counts still come through —
        // they don't depend on ANSI scanning and callers who set
        // enable_ansi_analysis=false typically still want them.
        if !self.config.enable_ansi_analysis {
            metrics.ansi_byte_count = 0;
        }
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
            compressed_omitted: false,
            compression_stats,
            input_bytes: bytes.len() as u64,
        }
        .with_consistent_compressed_marker()
    }

    /// Process a chunk through the pipeline, accumulating state.
    ///
    /// Returns the incremental metrics for this chunk. Full accumulated
    /// state is available on `state`.
    ///
    /// Callers must flush once [`ChunkedPipelineState::should_flush`] turns true.
    /// Continuing to append non-empty chunks after that point would otherwise
    /// turn backpressure into unbounded replay-buffer growth.
    ///
    /// br-ft-om7iu: this is the legacy panicking path. It now
    /// delegates to [`Self::try_process_chunk`] and panics on
    /// `Err(FlushRequired)` so existing callers that can
    /// guarantee the flush contract observe the same failure
    /// mode. Streaming callers (pane ingestion, IPC bridges)
    /// should prefer [`Self::try_process_chunk`] which surfaces
    /// backpressure as a recoverable error rather than a
    /// process-aborting panic.
    pub fn process_chunk(
        &self,
        bytes: &[u8],
        state: &mut ChunkedPipelineState,
    ) -> ScanMetricsSummary {
        match self.try_process_chunk(bytes, state) {
            Ok(summary) => summary,
            Err(err) => panic!("scan_pipeline process_chunk: {err}"),
        }
    }

    /// br-ft-om7iu: fallible variant of [`Self::process_chunk`].
    ///
    /// Returns [`ChunkedPipelineError::FlushRequired`] when
    /// `state.should_flush()` is true and `bytes` is non-empty —
    /// the chunk is NOT applied and the caller must drain via
    /// `flush(state)` before retrying. Empty chunks are no-ops
    /// regardless of flush state.
    pub fn try_process_chunk(
        &self,
        bytes: &[u8],
        state: &mut ChunkedPipelineState,
    ) -> std::result::Result<ScanMetricsSummary, ChunkedPipelineError> {
        if !bytes.is_empty() && state.should_flush() {
            return Err(ChunkedPipelineError::FlushRequired);
        }
        debug_assert!(
            bytes.is_empty() || !state.should_flush(),
            "scan_pipeline try_process_chunk: should_flush() must be false for non-empty chunks"
        );

        // Stage 1: Stateful SIMD metrics scan (cross-boundary aware)
        let mut chunk_metrics = scan_newlines_and_ansi_with_state(bytes, &mut state.scan_state);
        // [ft-5m5xc] Respect enable_ansi_analysis. Same shape as the
        // batch process() path above — zero ansi_byte_count AFTER the
        // stateful scan so the scan_state still advances (keeping the
        // cross-chunk in_escape / utf8-continuation carry honest for
        // future chunks that DO want ANSI analysis if the config is
        // ever flipped back on).
        if !self.config.enable_ansi_analysis {
            chunk_metrics.ansi_byte_count = 0;
        }

        // Accumulate metrics (saturating to prevent wrap in release builds)
        state.accumulated_metrics.newline_count = state
            .accumulated_metrics
            .newline_count
            .saturating_add(chunk_metrics.newline_count);
        state.accumulated_metrics.ansi_byte_count = state
            .accumulated_metrics
            .ansi_byte_count
            .saturating_add(chunk_metrics.ansi_byte_count);
        state.total_bytes = state.total_bytes.saturating_add(bytes.len() as u64);
        if !bytes.is_empty() {
            state.saw_any_bytes = true;
            state.ends_with_newline = bytes.last() == Some(&b'\n');
        }

        // Stage 2: Pattern trigger scan on this chunk
        if self.config.enable_triggers {
            // Incremental scan with overlap for real-time feedback
            let chunk_triggers = self.scan_chunk_triggers_with_overlap(
                bytes,
                &state.trigger_overlap_buffer,
                &mut state.trigger_scan_buffer,
            );
            state.total_trigger_matches = state
                .total_trigger_matches
                .saturating_add(chunk_triggers.total_matches);
            // ft-6db1t: array-indexed merge — O(6) bounded loop, no hashing.
            for (cat, count) in chunk_triggers.counts.iter_nonzero() {
                state.accumulated_triggers.add(cat, count);
            }

            update_trigger_overlap_buffer(
                &mut state.trigger_overlap_buffer,
                bytes,
                self.trigger_overlap_bytes,
            );

            // Accumulate raw data for definitive batch scan at flush time only
            // when compression is disabled. Otherwise the compression buffer
            // already owns the same bytes and can be reused at flush time.
            if !self.config.enable_compression {
                state.trigger_data_buffer.extend_from_slice(bytes);
            }
        }

        // Stage 3: Buffer for batch compression (no per-chunk compression)
        if self.config.enable_compression {
            state.uncompressed_buffer.extend_from_slice(bytes);
        }

        Ok(ScanMetricsSummary {
            newline_count: chunk_metrics.newline_count,
            ansi_byte_count: chunk_metrics.ansi_byte_count,
            logical_lines: chunk_metrics.logical_line_count(bytes),
            ansi_density: chunk_metrics.ansi_density(bytes.len()),
        })
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
            logical_lines: state.logical_lines(),
            ansi_density,
        };

        let triggers = if self.config.enable_triggers {
            // Definitive batch scan on accumulated data for exact parity with
            // process(). The incremental overlap-based counts are approximate
            // because Aho-Corasick LeftmostFirst non-overlapping matching is
            // context-dependent and not composable across chunk boundaries.
            Some(
                self.trigger_scanner
                    .scan_counts(self.definitive_trigger_bytes(state)),
            )
        } else {
            None
        };

        let (compressed, compression_stats) = if self.config.enable_compression
            && !state.uncompressed_buffer.is_empty()
            && state.uncompressed_buffer.len() >= self.config.compression_threshold
        {
            let (blob, comp_stats) = self
                .compressor
                .compress_with_stats(&state.uncompressed_buffer);
            (Some(blob), Some(comp_stats))
        } else {
            (None, None)
        };

        state.reset();

        ScanOutput {
            metrics: summary,
            triggers,
            compressed,
            compressed_omitted: false,
            compression_stats,
            input_bytes: total_bytes,
        }
        .with_consistent_compressed_marker()
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
    use crate::runtime_async::{self, CompatRuntime, RuntimeBuilder};
    use std::time::{Duration, Instant};

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

    // [ft-5m5xc] enable_ansi_analysis=false must zero ansi_byte_count
    // and ansi_density in the returned metrics. Pre-fix, the config
    // field was ignored entirely — setting it false still reported
    // the SIMD-scanned ANSI byte count, a silent reality-gap between
    // the documented config surface and the actual behavior.
    #[test]
    fn pipeline_honors_enable_ansi_analysis_false_ft_5m5xc() {
        let config = ScanPipelineConfig {
            enable_ansi_analysis: false,
            ..ScanPipelineConfig::default()
        };
        let pipeline = ScanPipeline::new(config);
        let data = b"line1\n\x1b[32mOK\x1b[0m\nline3\n";
        let output = pipeline.process(data);

        // Newlines and logical-line counts are independent of ANSI
        // analysis and must still flow through.
        assert_eq!(output.metrics.newline_count, 3);
        assert_eq!(output.metrics.logical_lines, 3);

        // ANSI portion must be zeroed.
        assert_eq!(
            output.metrics.ansi_byte_count, 0,
            "ft-5m5xc: enable_ansi_analysis=false must zero ansi_byte_count"
        );
        assert_eq!(
            output.metrics.ansi_density, 0.0,
            "ft-5m5xc: density derived from zeroed ansi_byte_count must also be zero"
        );

        // Sanity: the same input under the DEFAULT (enable_ansi_analysis=true)
        // config yields a non-zero ansi_byte_count, proving the branch is
        // actually doing work. Pre-fix, the "false" path returned the same
        // count as the "true" path.
        let default_pipeline = ScanPipeline::default();
        let default_output = default_pipeline.process(data);
        assert!(
            default_output.metrics.ansi_byte_count > 0,
            "sanity: default config must count ANSI bytes on a known-ANSI input"
        );
    }

    // [ft-5m5xc] Same contract for the chunked path — process_chunk
    // must respect enable_ansi_analysis so accumulated_metrics doesn't
    // grow ANSI counts when the operator opted out.
    #[test]
    fn chunked_pipeline_honors_enable_ansi_analysis_false_ft_5m5xc() {
        let config = ScanPipelineConfig {
            enable_ansi_analysis: false,
            ..ScanPipelineConfig::default()
        };
        let pipeline = ScanPipeline::new(config);
        let mut state = ChunkedPipelineState::new(1024 * 1024);

        pipeline.process_chunk(b"line1\n\x1b[32mOK\x1b[0m\n", &mut state);
        pipeline.process_chunk(b"line3\n", &mut state);

        assert_eq!(state.newline_count(), 3);
        assert_eq!(
            state.ansi_byte_count(),
            0,
            "ft-5m5xc: chunked accumulation must also respect the flag"
        );
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
        let scanner =
            TriggerScanner::new(vec![TriggerPattern::new("XYZZY", TriggerCategory::Custom)]);
        let pipeline = ScanPipeline::with_custom_triggers(ScanPipelineConfig::default(), scanner);
        let output = pipeline.process(b"XYZZY detected\n");
        let triggers = output.triggers.as_ref().unwrap();
        let custom = triggers.get(&TriggerCategory::Custom).copied().unwrap_or(0);
        assert_eq!(custom, 1);
    }

    // ── br-ft-djjnj: fallible custom-trigger validation ──

    #[test]
    fn try_with_custom_triggers_rejects_empty_pattern() {
        use crate::pattern_trigger::{DEFAULT_MAX_TRIGGER_PATTERN_LEN, TriggerScannerError};
        let patterns = vec![
            TriggerPattern::new("good", TriggerCategory::Custom),
            TriggerPattern::new("   ", TriggerCategory::Custom),
        ];
        let err = match ScanPipeline::try_with_custom_triggers(
            ScanPipelineConfig::default(),
            patterns,
            DEFAULT_MAX_TRIGGER_PATTERN_LEN,
        ) {
            Ok(_) => panic!("empty pattern must reject"),
            Err(err) => err,
        };
        assert_eq!(err, TriggerScannerError::EmptyPattern { index: 1 });
        assert!(err.to_string().contains("br-ft-djjnj"));
    }

    #[test]
    fn try_with_custom_triggers_rejects_oversized_pattern() {
        use crate::pattern_trigger::TriggerScannerError;
        let big = "a".repeat(2048);
        let patterns = vec![TriggerPattern::new(&big, TriggerCategory::Custom)];
        let err = match ScanPipeline::try_with_custom_triggers(
            ScanPipelineConfig::default(),
            patterns,
            1024, // cap below the pattern length
        ) {
            Ok(_) => panic!("oversized pattern must reject"),
            Err(err) => err,
        };
        match err {
            TriggerScannerError::PatternTooLong {
                index: 0,
                len: 2048,
                max_pattern_len: 1024,
            } => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn try_with_custom_triggers_succeeds_on_valid_set() {
        use crate::pattern_trigger::DEFAULT_MAX_TRIGGER_PATTERN_LEN;
        let patterns = vec![
            TriggerPattern::new("alpha", TriggerCategory::Custom),
            TriggerPattern::case_insensitive("BETA", TriggerCategory::Custom),
        ];
        let pipeline = ScanPipeline::try_with_custom_triggers(
            ScanPipelineConfig::default(),
            patterns,
            DEFAULT_MAX_TRIGGER_PATTERN_LEN,
        )
        .expect("valid pattern set");
        let out = pipeline.process(b"alpha then beta then BETA\n");
        let triggers = out.triggers.as_ref().expect("triggers present");
        // Both case-sensitive "alpha" and case-insensitive "BETA"
        // should fire. 1 alpha + 2 betas (lowercase "beta" + uppercase "BETA").
        assert_eq!(
            triggers.get(&TriggerCategory::Custom).copied().unwrap_or(0),
            3
        );
    }

    #[test]
    fn try_with_custom_triggers_caps_chunked_overlap_window() {
        // br-ft-djjnj: the chunked pipeline retains
        // (max_pattern_len - 1) bytes per chunk to recover
        // cross-boundary matches. With a 1024-byte cap, a hostile
        // operator can never force more than 1023 bytes of retained
        // overlap regardless of how many patterns they configure.
        use crate::pattern_trigger::DEFAULT_MAX_TRIGGER_PATTERN_LEN;
        let patterns: Vec<TriggerPattern> = (0..10)
            .map(|i| TriggerPattern::new(&format!("p{i:040}"), TriggerCategory::Custom))
            .collect();
        let pipeline = ScanPipeline::try_with_custom_triggers(
            ScanPipelineConfig::default(),
            patterns,
            DEFAULT_MAX_TRIGGER_PATTERN_LEN,
        )
        .expect("valid");
        // Largest pattern is 41 bytes ("p" + 40 zeros), so overlap is 40.
        assert!(
            pipeline.trigger_overlap_bytes <= 40,
            "overlap window must be bounded by max pattern length: got {}",
            pipeline.trigger_overlap_bytes
        );
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
    fn chunked_pipeline_reuses_compression_buffer_for_trigger_flush() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(1_048_576);
        let data = b"ERROR: oops\nCompiling serde\n";

        pipeline.process_chunk(&data[..16], &mut state);
        pipeline.process_chunk(&data[16..], &mut state);

        assert!(state.trigger_data_buffer.is_empty());
        assert_eq!(state.uncompressed_buffer, data);

        let batch_output = pipeline.process(data);
        let chunked_output = pipeline.flush(&mut state);

        assert_eq!(
            chunked_output.triggers.unwrap().total_matches,
            batch_output.triggers.unwrap().total_matches
        );
    }

    #[test]
    fn chunked_pipeline_keeps_trigger_buffer_when_compression_disabled() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let mut state = ChunkedPipelineState::new(1_048_576);
        let data = b"ERROR: oops\nCompiling serde\n";

        pipeline.process_chunk(data, &mut state);

        assert_eq!(state.trigger_data_buffer, data);
        assert!(state.uncompressed_buffer.is_empty());

        let batch_output = pipeline.process(data);
        let chunked_output = pipeline.flush(&mut state);

        assert_eq!(
            chunked_output.triggers.unwrap().total_matches,
            batch_output.triggers.unwrap().total_matches
        );
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

    #[test]
    #[should_panic(
        expected = "scan_pipeline process_chunk: br-ft-om7iu: scan_pipeline chunked state requires flush"
    )]
    fn chunked_process_chunk_panics_when_appending_after_flush_pending() {
        // br-ft-om7iu: legacy process_chunk still panics on
        // FlushRequired (delegates to try_process_chunk + unwrap).
        // Pin the bead-id-prefixed message so audit pipelines can
        // pattern-match on it; streaming callers should use
        // try_process_chunk to avoid the panic entirely.
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(64);

        pipeline.process_chunk(&[b'x'; 80], &mut state);
        assert!(state.should_flush());

        pipeline.process_chunk(b"more-bytes", &mut state);
    }

    // ── br-ft-om7iu: fallible chunked API ────────────────────────────────

    #[test]
    fn try_new_rejects_zero_max_buffer_bytes_ft_om7iu() {
        let err = ChunkedPipelineState::try_new(0).expect_err("must reject");
        assert!(matches!(
            err,
            ChunkedPipelineError::InvalidBufferLimit { provided: 0, .. }
        ));
    }

    #[test]
    fn try_new_accepts_minimum_one_byte_ft_om7iu() {
        let state = ChunkedPipelineState::try_new(MIN_CHUNKED_BUFFER_BYTES)
            .expect("MIN_CHUNKED_BUFFER_BYTES must be accepted");
        assert_eq!(state.total_bytes(), 0);
    }

    #[test]
    #[should_panic(expected = "br-ft-om7iu")]
    fn new_panics_on_zero_max_buffer_bytes_ft_om7iu() {
        // Pre-fix new(0) silently produced a permanently flush-
        // pending state. Post-fix it panics with the bead-id
        // prefixed message so the contract violation is visible.
        let _ = ChunkedPipelineState::new(0);
    }

    #[test]
    fn try_process_chunk_returns_flush_required_instead_of_panicking_ft_om7iu() {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::try_new(64).unwrap();

        // Fill past the limit so should_flush returns true.
        let _ = pipeline
            .try_process_chunk(&[b'x'; 80], &mut state)
            .expect("first chunk must succeed");
        assert!(state.should_flush());

        // Second chunk must NOT panic — return FlushRequired.
        let err = pipeline
            .try_process_chunk(b"more-bytes", &mut state)
            .expect_err("must be FlushRequired");
        assert!(matches!(err, ChunkedPipelineError::FlushRequired));
    }

    #[test]
    fn try_process_chunk_empty_chunk_no_op_even_when_flush_pending_ft_om7iu() {
        // Empty chunks cannot grow the replay buffer — they must
        // be no-ops regardless of should_flush state. This pins
        // the documented contract.
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::try_new(64).unwrap();
        let _ = pipeline.try_process_chunk(&[b'x'; 80], &mut state).unwrap();
        assert!(state.should_flush());
        let summary = pipeline
            .try_process_chunk(&[], &mut state)
            .expect("empty chunk must not error");
        assert_eq!(summary.newline_count, 0);
        assert_eq!(summary.logical_lines, 0);
    }

    #[test]
    fn try_process_chunk_resumes_after_flush_ft_om7iu() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let mut state = ChunkedPipelineState::try_new(64).unwrap();

        let _ = pipeline
            .try_process_chunk(
                b"ERROR: enough to fill the chunked buffer past 64 bytes\n",
                &mut state,
            )
            .unwrap();
        assert!(state.should_flush());

        // Try-process while pending → FlushRequired
        let err = pipeline
            .try_process_chunk(b"more", &mut state)
            .expect_err("must be FlushRequired");
        assert!(matches!(err, ChunkedPipelineError::FlushRequired));

        // Drain and retry; should now succeed.
        let _ = pipeline.flush(&mut state);
        assert!(!state.should_flush());
        let summary = pipeline
            .try_process_chunk(b"more", &mut state)
            .expect("post-flush try_process_chunk must succeed");
        assert!(summary.newline_count == 0 || summary.newline_count > 0);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 32,
            ..proptest::test_runner::Config::default()
        })]

        /// br-ft-om7iu: try_process_chunk must NEVER panic across
        /// any combination of buffer-limit, chunk-size, and chunk
        /// sequence. Replaces the assert!-based fail-fast that
        /// could abort the caller on backpressure.
        #[test]
        fn try_process_chunk_never_panics_on_any_chunk_sequence_ft_om7iu(
            max_buffer in 1usize..=256,
            chunks in proptest::collection::vec(
                proptest::collection::vec(0u8..=255, 0..=128),
                0..=8,
            ),
        ) {
            let pipeline = ScanPipeline::default();
            let mut state = ChunkedPipelineState::try_new(max_buffer).unwrap();
            for chunk in chunks {
                // Calling try_process_chunk MUST always return
                // either Ok or Err(FlushRequired) — never panic.
                let _ = pipeline.try_process_chunk(&chunk, &mut state);
            }
        }
    }

    #[test]
    fn chunked_process_chunk_resumes_after_flush_pending() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let mut state = ChunkedPipelineState::new(64);

        let first = b"ERROR: this chunk is large enough to force a flush boundary immediately\n";
        let first_batch = pipeline.process(first);

        pipeline.process_chunk(first, &mut state);
        assert!(state.should_flush());

        let first_flush = pipeline.flush(&mut state);
        assert!(
            !state.should_flush(),
            "flush must clear the pending indicator"
        );
        assert_eq!(state.total_bytes(), 0, "flush must reset accumulated bytes");
        assert_eq!(
            first_flush.triggers.as_ref().unwrap().total_matches,
            first_batch.triggers.as_ref().unwrap().total_matches,
            "flush checkpoint must preserve definitive trigger totals for the drained window"
        );

        let second = b"ERROR: oops\nCompiling serde\n";
        let second_batch = pipeline.process(second);

        pipeline.process_chunk(&second[..16], &mut state);
        pipeline.process_chunk(&second[16..], &mut state);

        let second_flush = pipeline.flush(&mut state);
        assert_eq!(
            second_flush.triggers.as_ref().unwrap().total_matches,
            second_batch.triggers.as_ref().unwrap().total_matches,
            "post-flush chunking should resume without losing overlap-trigger parity"
        );
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

    // ── br-ft-u139v: wire-truth invariant for omitted compressed blob ──

    #[test]
    fn output_with_compressed_blob_marks_omitted_on_wire() {
        // Pre-fix: serialized form had compression_stats: Some(...)
        // but never included the bytes, and there was no marker
        // telling consumers the blob was intentionally stripped.
        let config = ScanPipelineConfig {
            enable_triggers: false,
            enable_compression: true,
            compression_threshold: 16,
            ..Default::default()
        };
        let pipeline = ScanPipeline::new(config);
        let buffer = vec![b'x'; 1024]; // exceeds the 16-byte threshold
        let output = pipeline.process(&buffer);

        assert!(output.compressed.is_some(), "compression must have run");
        assert!(output.compression_stats.is_some());
        assert!(
            output.compressed_omitted,
            "construction must mark compressed_omitted when blob is present"
        );

        let json = serde_json::to_string(&output).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(
            !json.contains("\"compressed\":"),
            "compressed bytes must not appear on the wire: {json}"
        );
        assert_eq!(
            parsed["compressed_omitted"],
            serde_json::json!(true),
            "wire form must explicitly say the blob was omitted"
        );
        assert!(parsed["compression_stats"].is_object(), "stats remain");

        let rt: ScanOutput = serde_json::from_str(&json).unwrap();
        assert!(rt.compressed.is_none());
        assert!(
            rt.compressed_omitted,
            "receiver must see the omission marker"
        );
        assert!(rt.compression_stats.is_some());
    }

    #[test]
    fn output_without_compression_does_not_emit_marker() {
        let config = ScanPipelineConfig {
            enable_triggers: false,
            enable_compression: false,
            ..Default::default()
        };
        let pipeline = ScanPipeline::new(config);
        let output = pipeline.process(b"hello");

        assert!(output.compressed.is_none());
        assert!(!output.compressed_omitted);

        let json = serde_json::to_string(&output).unwrap();
        assert!(
            !json.contains("compressed_omitted"),
            "wire form must omit the marker when blob never existed: {json}"
        );
    }

    #[test]
    fn output_pre_fix_persisted_snapshot_deserializes_to_false_marker() {
        // Backwards-compat: a JSON document written before this fix
        // (no compressed_omitted field) must still deserialize, with
        // the marker defaulting to false.
        let pre_fix_json = r#"{
            "metrics": {
                "newline_count": 0,
                "ansi_byte_count": 0,
                "logical_lines": 0,
                "ansi_density": 0.0
            },
            "triggers": null,
            "compression_stats": null,
            "input_bytes": 0
        }"#;
        let rt: ScanOutput = serde_json::from_str(pre_fix_json).unwrap();
        assert!(!rt.compressed_omitted);
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
    fn chunked_recovers_split_patterns_with_overlap() {
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

        assert_eq!(chunked_total, batch_total);
    }

    #[test]
    fn chunked_flush_preserves_logical_lines_without_trailing_newline() {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let data = b"line1\nline2";

        let batch_output = pipeline.process(data);

        let mut state = ChunkedPipelineState::new(1_048_576);
        pipeline.process_chunk(b"line1\nli", &mut state);
        pipeline.process_chunk(b"ne2", &mut state);
        let chunked_output = pipeline.flush(&mut state);

        assert_eq!(chunked_output.metrics.logical_lines, 2);
        assert_eq!(
            chunked_output.metrics.logical_lines,
            batch_output.metrics.logical_lines
        );
    }

    struct LivenessReceipt {
        seed_name: &'static str,
        input_bytes: usize,
        buffer_limit: usize,
        processed_bytes: usize,
        flushed_bytes: u64,
        flushes: usize,
        retries: usize,
        yields: usize,
        elapsed_ms: u128,
        parser_decisions: Vec<String>,
    }

    impl LivenessReceipt {
        fn emit(&self) {
            eprintln!(
                "ft-aawoe scan-pipeline liveness receipt seed={} input_bytes={} buffer_limit={} processed_bytes={} flushed_bytes={} flushes={} retries={} yields={} elapsed_ms={} decisions={}",
                self.seed_name,
                self.input_bytes,
                self.buffer_limit,
                self.processed_bytes,
                self.flushed_bytes,
                self.flushes,
                self.retries,
                self.yields,
                self.elapsed_ms,
                self.parser_decisions.join("|")
            );
        }
    }

    async fn drive_terminal_seed_with_backpressure_receipt(
        seed_name: &'static str,
        seed: &[u8],
        buffer_limit: usize,
        chunk_pattern: &[usize],
    ) -> LivenessReceipt {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let mut state = ChunkedPipelineState::try_new(buffer_limit).expect("valid buffer limit");
        let start = Instant::now();
        let mut cursor = 0usize;
        let mut chunk_index = 0usize;
        let mut processed_bytes = 0usize;
        let mut flushed_bytes = 0u64;
        let mut flushes = 0usize;
        let mut retries = 0usize;
        let mut yields = 0usize;
        let mut parser_decisions = Vec::new();
        let max_attempts = seed.len().saturating_mul(4).saturating_add(64);
        let mut attempts = 0usize;

        while cursor < seed.len() {
            attempts += 1;
            assert!(
                attempts <= max_attempts,
                "ft-aawoe: scan pipeline made no forward progress seed={seed_name} cursor={cursor} len={} decisions={}",
                seed.len(),
                parser_decisions.join("|")
            );

            let requested = chunk_pattern[chunk_index % chunk_pattern.len()].max(1);
            chunk_index += 1;
            let end = cursor.saturating_add(requested).min(seed.len());
            let chunk = &seed[cursor..end];

            match pipeline.try_process_chunk(chunk, &mut state) {
                Ok(summary) => {
                    processed_bytes += chunk.len();
                    cursor = end;
                    parser_decisions.push(format!(
                        "ok:{}:{}:{}",
                        chunk.len(),
                        summary.ansi_byte_count,
                        state.should_flush()
                    ));
                }
                Err(ChunkedPipelineError::FlushRequired) => {
                    retries += 1;
                    let output = pipeline.flush(&mut state);
                    flushed_bytes = flushed_bytes.saturating_add(output.input_bytes);
                    flushes += 1;
                    parser_decisions.push(format!("retry_flush:{}", output.input_bytes));
                    runtime_async::task::yield_now().await;
                    yields += 1;
                    continue;
                }
                Err(err) => panic!("unexpected scan-pipeline error for seed {seed_name}: {err}"),
            }

            if state.should_flush() {
                let output = pipeline.flush(&mut state);
                flushed_bytes = flushed_bytes.saturating_add(output.input_bytes);
                flushes += 1;
                parser_decisions.push(format!("flush:{}", output.input_bytes));
            }

            runtime_async::task::yield_now().await;
            yields += 1;
        }

        let final_output = pipeline.flush(&mut state);
        flushed_bytes = flushed_bytes.saturating_add(final_output.input_bytes);
        if final_output.input_bytes > 0 {
            flushes += 1;
            parser_decisions.push(format!("final_flush:{}", final_output.input_bytes));
        }

        LivenessReceipt {
            seed_name,
            input_bytes: seed.len(),
            buffer_limit,
            processed_bytes,
            flushed_bytes,
            flushes,
            retries,
            yields,
            elapsed_ms: start.elapsed().as_millis(),
            parser_decisions,
        }
    }

    #[test]
    fn terminal_protocol_backpressure_liveness_receipts_ft_aawoe() {
        let mut unterminated_csi = Vec::with_capacity(258);
        unterminated_csi.extend_from_slice(b"\x1b[");
        for _ in 0..128 {
            unterminated_csi.extend_from_slice(b"0;");
        }

        let mut split_osc = Vec::new();
        split_osc.extend_from_slice(b"\x1b]0;window-title");
        split_osc.extend_from_slice(&[0xff, 0xfe, 0x80, b'\n']);
        split_osc.extend_from_slice(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");

        let seeds: &[(&str, &[u8], usize, &[usize])] = &[
            (
                "esc-flood",
                b"\x1b\x1b\x1b\x1b\x1b\x1bERROR\x1b[31mwarning\x1b",
                8,
                &[1, 2, 3],
            ),
            (
                "invalid-utf8-sgr",
                b"\xc0\xaf\x1b[38;2;255;0;128mFAIL\x1b[0m\xf0\x9f\x1b[31m",
                11,
                &[5, 1, 8, 2],
            ),
            ("unterminated-csi", &unterminated_csi, 13, &[7, 1, 16, 3]),
            ("split-osc", &split_osc, 17, &[4, 9, 2, 11]),
        ];

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime_async current-thread runtime");

        for (seed_name, seed, buffer_limit, chunk_pattern) in seeds {
            let receipt = runtime.block_on(async {
                runtime_async::timeout(
                    Duration::from_millis(250),
                    drive_terminal_seed_with_backpressure_receipt(
                        seed_name,
                        seed,
                        *buffer_limit,
                        chunk_pattern,
                    ),
                )
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "ft-aawoe: runtime_async timeout while scanning seed={seed_name} len={} err={err}",
                        seed.len()
                    )
                })
            });
            receipt.emit();
            assert_eq!(
                receipt.processed_bytes, receipt.input_bytes,
                "ft-aawoe: liveness driver must consume every byte for seed {}",
                receipt.seed_name
            );
            assert_eq!(
                receipt.flushed_bytes, receipt.input_bytes as u64,
                "ft-aawoe: flush accounting must equal consumed bytes for seed {}",
                receipt.seed_name
            );
            assert!(
                receipt.flushes > 0,
                "ft-aawoe: seed {} should exercise backpressure flush path",
                receipt.seed_name
            );
            assert!(
                receipt.yields >= receipt.flushes,
                "ft-aawoe: runtime_async yield checkpoints should accompany scan progress"
            );
        }
    }

    /// Validate all fuzz corpus seeds don't panic and produce sane output.
    #[test]
    fn fuzz_corpus_seeds_no_panic() {
        let seeds: &[&[u8]] = &[
            // Truecolor with dangling ESC at end
            b"\x1b[38;2;255;0;128mTruecolor\x1b[0m \x1b[48;5;196m256col\x1b[0m\x1b",
            // Multi-byte UTF-8 mixed with ANSI
            b"\x1b[1m\xe4\xb8\xad\xe6\x96\x87\x1b[0m \xf0\x9f\x9a\x80 A\xcc\x81\n",
            // Trigger keywords
            b"ERROR: disk full\n\x1b[33mwarning\x1b[0m: unused var\n   Compiling foo v0.1\ntest result: FAILED. 3 passed; 1 failed\n",
            // Newline/escape interleave
            b"\n\n\n\x1b[\n\x1b[31m\n\x1b[0m\n\n\x1b[1;2;3;4;5;6;7;8;9m\n\x1b",
            // Long SGR param sequence
            b"\x1b[0;1;2;3;4;5;7;8;9;21;53;38;2;100;200;50;48;2;10;20;30mSTYLED\x1b[0m",
            // OSC with BEL and ST terminators
            b"\x1b]0;Window Title\x07\x1b]8;;https://example.com\x1b\\Link\x1b]8;;\x1b\\\n",
            // DCS/APC/SOS/PM sequences
            b"\x1bP+q544d\x1b\\\x1b_APC content\x1b\\\x1bX SOS \x1b\\\x1b^ PM \x1b\\\n",
            // Binary noise mixed with valid escapes
            b"\xff\xfe\x80\x1b[32m\x00\x01\x02OK\x1b[0m\xff\xc0\n",
        ];

        for (i, seed) in seeds.iter().enumerate() {
            let output = quick_scan(seed);
            assert_eq!(
                output.input_bytes,
                seed.len() as u64,
                "seed {i}: input_bytes mismatch"
            );
        }
    }

    /// Adversarial seeds targeting crash-prone patterns: encoding abuse,
    /// boundary conditions, unterminated sequences, byte floods.
    #[test]
    fn fuzz_adversarial_seeds_no_panic() {
        let seeds: &[&[u8]] = &[
            // ESC flood — all escape, no parameters or terminators
            b"\x1b\x1b\x1b\x1b\x1b\x1b\x1b\x1b\x1b\x1b",
            // Overlong UTF-8 (invalid: 2-byte encoding of ASCII)
            b"\xc0\xaf\xc1\xbf\xe0\x80\xaf\xf0\x80\x80\xaf",
            // Truncated multi-byte UTF-8 interrupted by ANSI escapes
            b"\xe4\x1b[0m\xf0\x9f\x1b[31m\xf0\x1b",
            // 0xFF flood (256 bytes, no valid UTF-8)
            &[0xFF; 256],
        ];

        // Dynamic seeds that can't be byte literals
        let nul_esc: Vec<u8> = (0..64).flat_map(|_| [0x00, 0x1b]).collect();
        let mut unterminated_csi = Vec::with_capacity(1002);
        unterminated_csi.extend_from_slice(b"\x1b[");
        for _ in 0..500 {
            unterminated_csi.extend_from_slice(b"0;");
        }
        let mut sgr_128 = Vec::with_capacity(260);
        sgr_128.extend_from_slice(b"\x1b[");
        for i in 0..128u8 {
            if i > 0 {
                sgr_128.push(b';');
            }
            sgr_128.push(b'0');
        }
        sgr_128.push(b'm');

        let dynamic_seeds: &[&[u8]] = &[&nul_esc, &unterminated_csi, &sgr_128];

        for (i, seed) in seeds.iter().chain(dynamic_seeds.iter()).enumerate() {
            let output = quick_scan(seed);
            assert_eq!(
                output.input_bytes,
                seed.len() as u64,
                "adversarial seed {i}: input_bytes mismatch"
            );
        }
    }
}
