#![no_main]
//! Chunked scan-pipeline fuzz target.
//!
//! Exercises the stateful `ScanPipeline::process_chunk` path rather than the
//! batch-only `quick_scan` helper. The oracle is:
//! - no panic on valid chunked use
//! - chunked flush matches batch `process()` when no mid-stream flush is needed
//! - a non-empty append after `should_flush()` trips the documented panic
//! - flushing clears the pending state so chunk ingestion can resume

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::scan_pipeline::{
    ChunkedPipelineState, CompressionLevelConfig, ScanOutput, ScanPipeline, ScanPipelineConfig,
};
use libfuzzer_sys::fuzz_target;
use std::panic::{AssertUnwindSafe, catch_unwind};

const MAX_INPUT_BYTES: usize = 128 * 1024;
const MAX_CHUNK_HINTS: usize = 128;
const MAX_BUFFER_BYTES: usize = 4096;
const MAX_COMPRESSION_THRESHOLD: usize = 4096;

#[derive(Arbitrary, Debug)]
struct FuzzCase {
    data: Vec<u8>,
    chunk_hints: Vec<u16>,
    enable_triggers: bool,
    enable_compression: bool,
    enable_ansi_analysis: bool,
    compression_threshold: u16,
    flush_pending_buffer: u16,
    compression_level: u8,
}

fn build_config(case: &FuzzCase) -> ScanPipelineConfig {
    let compression_level = match case.compression_level % 4 {
        0 => CompressionLevelConfig::Fast,
        1 => CompressionLevelConfig::Default,
        2 => CompressionLevelConfig::High,
        _ => CompressionLevelConfig::Maximum,
    };

    ScanPipelineConfig {
        enable_triggers: case.enable_triggers,
        enable_compression: case.enable_compression,
        compression_level,
        compression_threshold: usize::from(case.compression_threshold)
            .min(MAX_COMPRESSION_THRESHOLD),
        enable_ansi_analysis: case.enable_ansi_analysis,
    }
}

fn feed_chunks(
    pipeline: &ScanPipeline,
    bytes: &[u8],
    chunk_hints: &[u16],
    state: &mut ChunkedPipelineState,
) {
    let mut cursor = 0usize;

    for &hint in chunk_hints {
        let remaining = bytes.len().saturating_sub(cursor);
        if remaining == 0 {
            let _ = pipeline.process_chunk(&[], state);
            continue;
        }

        let requested = usize::from(hint).min(remaining);
        let chunk_len = if requested == 0 {
            1.min(remaining)
        } else {
            requested
        };
        let end = cursor + chunk_len;
        let _ = pipeline.process_chunk(&bytes[cursor..end], state);
        cursor = end;
    }

    if cursor < bytes.len() {
        let _ = pipeline.process_chunk(&bytes[cursor..], state);
    }
}

fn assert_outputs_match(chunked: &ScanOutput, batch: &ScanOutput) {
    assert_eq!(chunked.input_bytes, batch.input_bytes);
    assert_eq!(chunked.metrics.newline_count, batch.metrics.newline_count);
    assert_eq!(
        chunked.metrics.ansi_byte_count,
        batch.metrics.ansi_byte_count
    );
    assert_eq!(chunked.metrics.logical_lines, batch.metrics.logical_lines);
    assert_eq!(
        chunked.metrics.ansi_density.to_bits(),
        batch.metrics.ansi_density.to_bits()
    );

    match (&chunked.triggers, &batch.triggers) {
        (Some(lhs), Some(rhs)) => {
            assert_eq!(lhs.counts, rhs.counts);
            assert_eq!(lhs.total_matches, rhs.total_matches);
            assert_eq!(lhs.bytes_scanned, rhs.bytes_scanned);
        }
        (None, None) => {}
        _ => panic!("chunked/batch trigger presence drift"),
    }

    match (&chunked.compression_stats, &batch.compression_stats) {
        (Some(lhs), Some(rhs)) => {
            assert_eq!(lhs.input_bytes, rhs.input_bytes);
            assert_eq!(lhs.output_bytes, rhs.output_bytes);
            assert_eq!(lhs.buffer_count, rhs.buffer_count);
            assert_eq!(lhs.ratio.to_bits(), rhs.ratio.to_bits());
        }
        (None, None) => {}
        _ => panic!("chunked/batch compression stats presence drift"),
    }

    assert_eq!(chunked.compressed, batch.compressed);
}

fuzz_target!(|data: &[u8]| {
    let Ok(mut case) = FuzzCase::arbitrary(&mut Unstructured::new(data)) else {
        return;
    };

    if case.data.len() > MAX_INPUT_BYTES {
        case.data.truncate(MAX_INPUT_BYTES);
    }
    if case.chunk_hints.len() > MAX_CHUNK_HINTS {
        case.chunk_hints.truncate(MAX_CHUNK_HINTS);
    }

    let config = build_config(&case);
    let pipeline = ScanPipeline::new(config);

    // Valid chunked path: a large enough buffer means we should match the
    // batch path exactly after a single flush.
    let parity_capacity = case.data.len().saturating_add(1).max(1);
    let mut parity_state = ChunkedPipelineState::new(parity_capacity);
    feed_chunks(&pipeline, &case.data, &case.chunk_hints, &mut parity_state);
    let chunked_output = pipeline.flush(&mut parity_state);
    let batch_output = pipeline.process(&case.data);
    assert_outputs_match(&chunked_output, &batch_output);

    // Flush-pending contract: once the state reports backpressure, a further
    // non-empty append must panic until the caller flushes.
    if !case.data.is_empty() {
        let pending_capacity = usize::from(case.flush_pending_buffer)
            .max(1)
            .min(MAX_BUFFER_BYTES);
        let mut pending_state = ChunkedPipelineState::new(pending_capacity);
        let first_len = case.data.len().min(pending_capacity);
        let _ = pipeline.process_chunk(&case.data[..first_len], &mut pending_state);

        if pending_state.should_flush() {
            let append_byte = case.data.get(first_len).copied().unwrap_or(b'X');
            let append_result = catch_unwind(AssertUnwindSafe(|| {
                let _ = pipeline.process_chunk(&[append_byte], &mut pending_state);
            }));
            assert!(
                append_result.is_err(),
                "non-empty append after should_flush() must panic"
            );

            let _ = pipeline.flush(&mut pending_state);
            let resumed_summary = pipeline.process_chunk(&[append_byte], &mut pending_state);
            let resumed_output = pipeline.flush(&mut pending_state);
            assert_eq!(
                resumed_summary.newline_count,
                resumed_output.metrics.newline_count
            );
            assert_eq!(resumed_output.input_bytes, 1);
        }
    }
});
