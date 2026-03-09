//! Property-based tests for scan_pipeline module (ft-2oph2).
//!
//! Validates pipeline invariants: metrics consistency, compression roundtrip,
//! chunked-vs-batch parity (for line-aligned splits), and trigger accumulation.

use proptest::prelude::*;

use frankenterm_core::byte_compression::ByteCompressor;
use frankenterm_core::scan_pipeline::{ChunkedPipelineState, ScanPipeline, ScanPipelineConfig};

// =============================================================================
// Strategies
// =============================================================================

fn random_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..4096)
}

fn terminal_text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            "[a-zA-Z0-9 _\\-.:;,=/()]{1,80}\\n".prop_map(|s| s.into_bytes()),
            Just(b"ERROR: something failed\n".to_vec()),
            Just(b"   Compiling serde v1.0\n".to_vec()),
            Just(b"\x1b[32mOK\x1b[0m\n".to_vec()),
            Just(b"    Finished `dev` profile in 3s\n".to_vec()),
            Just(b"WARNING: deprecated\n".to_vec()),
        ],
        1..100,
    )
    .prop_map(|lines| lines.into_iter().flatten().collect())
}

/// Strategy that produces line-aligned chunk boundaries.
fn line_aligned_chunks(data: Vec<u8>) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut start = 0;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            chunks.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        chunks.push(data[start..].to_vec());
    }
    if chunks.is_empty() && !data.is_empty() {
        chunks.push(data);
    }
    chunks
}

fn arbitrary_chunks(data: &[u8], chunk_sizes: &[usize]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }

    if chunk_sizes.is_empty() {
        return vec![data.to_vec()];
    }

    let mut chunks = Vec::new();
    let mut offset = 0usize;
    let mut index = 0usize;
    while offset < data.len() {
        let requested = chunk_sizes[index % chunk_sizes.len()].max(1);
        let chunk_len = requested.min(data.len() - offset);
        chunks.push(data[offset..offset + chunk_len].to_vec());
        offset += chunk_len;
        index += 1;
    }
    chunks
}

// =============================================================================
// Metrics invariant tests
// =============================================================================

proptest! {
    /// Input bytes always equals what the pipeline reports.
    #[test]
    fn input_bytes_matches(data in random_bytes()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        prop_assert_eq!(output.input_bytes, data.len() as u64);
    }

    /// Newline count is consistent with memchr count.
    #[test]
    fn newline_count_matches_memchr(data in random_bytes()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        let expected = memchr::memchr_iter(b'\n', &data).count();
        prop_assert_eq!(output.metrics.newline_count, expected);
    }

    /// ANSI density is in [0, 1] range.
    #[test]
    fn ansi_density_bounded(data in random_bytes()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        prop_assert!(output.metrics.ansi_density >= 0.0);
        prop_assert!(output.metrics.ansi_density <= 1.0);
    }

    /// ANSI byte count is <= total bytes.
    #[test]
    fn ansi_bytes_bounded(data in random_bytes()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        prop_assert!(output.metrics.ansi_byte_count <= data.len());
    }
}

// =============================================================================
// Compression roundtrip tests
// =============================================================================

proptest! {
    /// When compression is enabled and above threshold, the compressed blob
    /// roundtrips back to the original.
    #[test]
    fn compression_roundtrip(data in prop::collection::vec(any::<u8>(), 300..4096)) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            compression_threshold: 256,
            ..Default::default()
        });
        let output = pipeline.process(&data);
        if let Some(compressed) = &output.compressed {
            let compressor = ByteCompressor::default();
            let decompressed = compressor.decompress(compressed).unwrap();
            prop_assert_eq!(&decompressed, &data);
        }
    }

    /// Compression stats report correct input size.
    #[test]
    fn compression_stats_input_size(data in prop::collection::vec(any::<u8>(), 300..4096)) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        if let Some(stats) = &output.compression_stats {
            prop_assert_eq!(stats.input_bytes, data.len() as u64);
        }
    }
}

// =============================================================================
// Trigger invariant tests
// =============================================================================

proptest! {
    /// Trigger total matches equals sum of per-category counts.
    #[test]
    fn trigger_total_equals_category_sum(data in terminal_text()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        if let Some(triggers) = &output.triggers {
            let sum: u64 = triggers.counts.values().sum();
            prop_assert_eq!(triggers.total_matches, sum);
        }
    }

    /// Trigger bytes_scanned matches input length.
    #[test]
    fn trigger_bytes_scanned_matches_input(data in random_bytes()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        if let Some(triggers) = &output.triggers {
            prop_assert_eq!(triggers.bytes_scanned, data.len() as u64);
        }
    }
}

// =============================================================================
// Chunked vs batch parity (line-aligned)
// =============================================================================

proptest! {
    /// When chunks are line-aligned, chunked and batch metrics agree.
    #[test]
    fn chunked_batch_newline_parity(data in terminal_text()) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });

        let batch_output = pipeline.process(&data);

        let chunks = line_aligned_chunks(data);
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            pipeline.process_chunk(chunk, &mut state);
        }
        let chunked_output = pipeline.flush(&mut state);

        prop_assert_eq!(
            batch_output.metrics.newline_count,
            chunked_output.metrics.newline_count
        );
    }

    /// Line-aligned chunked triggers match batch triggers.
    #[test]
    fn chunked_batch_trigger_parity(data in terminal_text()) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });

        let batch_output = pipeline.process(&data);

        let chunks = line_aligned_chunks(data);
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            pipeline.process_chunk(chunk, &mut state);
        }
        let chunked_output = pipeline.flush(&mut state);

        let batch_total = batch_output.triggers.as_ref().unwrap().total_matches;
        let chunked_total = chunked_output.triggers.as_ref().unwrap().total_matches;
        prop_assert_eq!(batch_total, chunked_total);
    }

    /// Arbitrary chunk boundaries preserve trigger totals via overlap carry.
    #[test]
    fn chunked_batch_trigger_parity_arbitrary_boundaries(
        data in terminal_text(),
        chunk_sizes in prop::collection::vec(0usize..128, 0..16),
    ) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });

        let batch_output = pipeline.process(&data);

        let chunks = arbitrary_chunks(&data, &chunk_sizes);
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            pipeline.process_chunk(chunk, &mut state);
        }
        let chunked_output = pipeline.flush(&mut state);

        let batch_total = batch_output.triggers.as_ref().unwrap().total_matches;
        let chunked_total = chunked_output.triggers.as_ref().unwrap().total_matches;
        prop_assert_eq!(batch_total, chunked_total);
    }

    /// Arbitrary chunk boundaries preserve logical line count.
    #[test]
    fn chunked_batch_logical_line_parity_arbitrary_boundaries(
        data in terminal_text(),
        chunk_sizes in prop::collection::vec(0usize..128, 0..16),
    ) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });

        let batch_output = pipeline.process(&data);

        let chunks = arbitrary_chunks(&data, &chunk_sizes);
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            pipeline.process_chunk(chunk, &mut state);
        }
        let chunked_output = pipeline.flush(&mut state);

        prop_assert_eq!(
            batch_output.metrics.logical_lines,
            chunked_output.metrics.logical_lines
        );
    }
}

// =============================================================================
// Chunked accumulation invariants
// =============================================================================

proptest! {
    /// Total bytes in chunked state equals sum of chunk sizes.
    #[test]
    fn chunked_total_bytes_accumulates(
        chunks in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..512),
            1..10,
        )
    ) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let mut state = ChunkedPipelineState::new(16_777_216);

        let mut expected_bytes: u64 = 0;
        for chunk in &chunks {
            pipeline.process_chunk(chunk, &mut state);
            expected_bytes += chunk.len() as u64;
        }

        prop_assert_eq!(state.total_bytes(), expected_bytes);
    }

    /// Chunked newline count equals sum of per-chunk newline counts.
    #[test]
    fn chunked_newlines_accumulate(
        chunks in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..256),
            1..8,
        )
    ) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_triggers: false,
            enable_compression: false,
            ..Default::default()
        });
        let mut state = ChunkedPipelineState::new(16_777_216);

        let expected_newlines: usize = chunks.iter()
            .flat_map(|c| c.iter())
            .filter(|&&b| b == b'\n')
            .count();

        for chunk in &chunks {
            pipeline.process_chunk(chunk, &mut state);
        }

        prop_assert_eq!(state.newline_count(), expected_newlines);
    }

    /// Flush resets all state to zero.
    #[test]
    fn flush_resets_state(data in terminal_text()) {
        let pipeline = ScanPipeline::default();
        let mut state = ChunkedPipelineState::new(16_777_216);

        pipeline.process_chunk(&data, &mut state);
        let _ = pipeline.flush(&mut state);

        prop_assert_eq!(state.total_bytes(), 0);
        prop_assert_eq!(state.newline_count(), 0);
        prop_assert_eq!(state.total_trigger_matches(), 0);
        prop_assert!(!state.has_errors());
        prop_assert!(!state.has_completions());
    }
}

// =============================================================================
// Disabled features
// =============================================================================

proptest! {
    /// With triggers disabled, output.triggers is None.
    #[test]
    fn triggers_disabled_returns_none(data in random_bytes()) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_triggers: false,
            ..Default::default()
        });
        let output = pipeline.process(&data);
        prop_assert!(output.triggers.is_none());
    }

    /// With compression disabled, compressed is None.
    #[test]
    fn compression_disabled_returns_none(data in random_bytes()) {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_compression: false,
            ..Default::default()
        });
        let output = pipeline.process(&data);
        prop_assert!(output.compressed.is_none());
        prop_assert!(output.compression_stats.is_none());
    }
}

// =============================================================================
// Serde roundtrip
// =============================================================================

proptest! {
    /// ScanOutput survives JSON roundtrip (compressed blob is skipped).
    #[test]
    fn output_serde_roundtrip(data in terminal_text()) {
        let pipeline = ScanPipeline::default();
        let output = pipeline.process(&data);
        let json = serde_json::to_string(&output).unwrap();
        let rt: frankenterm_core::scan_pipeline::ScanOutput =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.input_bytes, output.input_bytes);
        prop_assert_eq!(rt.metrics.newline_count, output.metrics.newline_count);
    }

    /// ScanPipelineConfig survives JSON roundtrip.
    #[test]
    fn config_serde_roundtrip(threshold in 0..10_000usize) {
        let config = ScanPipelineConfig {
            compression_threshold: threshold,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let rt: ScanPipelineConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.compression_threshold, threshold);
    }
}
