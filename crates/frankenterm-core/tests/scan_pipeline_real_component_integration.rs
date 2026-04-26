//! Mock-free integration coverage for the scan pipeline.
//!
//! This test exercises the real SIMD scanner, real Aho-Corasick trigger
//! scanner, and real zstd compressor through `ScanPipeline`. It intentionally
//! stays outside the module unit tests so public integration coverage catches
//! drift between the component APIs.

use frankenterm_core::pattern_trigger::TriggerCategory;
use frankenterm_core::scan_pipeline::{
    ChunkedPipelineState, CompressionLevelConfig, ScanOutput, ScanPipeline, ScanPipelineConfig,
};
use serde_json::json;
use std::time::Instant;

fn log_phase(phase: &str, payload: serde_json::Value) {
    eprintln!(
        "{}",
        json!({
            "test": "scan_pipeline_real_component_integration",
            "phase": phase,
            "payload": payload,
        })
    );
}

fn trigger_count(output: &ScanOutput, category: TriggerCategory) -> u64 {
    output
        .triggers
        .as_ref()
        .expect("trigger scanning should be enabled")
        .counts
        .count(category)
}

#[test]
fn chunked_flush_matches_batch_for_real_scan_components() {
    let input = b"Compiling frankenterm-core\n\
                  warning: deprecated config key\n\
                  \x1b[31mERROR: pane tailer failed\x1b[0m\n\
                  test result: ok. 42 passed; 0 failed\n\
                  Finished `dev` profile\n";
    let chunks: [&[u8]; 4] = [&input[..23], &input[23..61], &input[61..102], &input[102..]];
    let config = ScanPipelineConfig {
        enable_triggers: true,
        enable_compression: true,
        compression_level: CompressionLevelConfig::Fast,
        compression_threshold: 1,
        enable_ansi_analysis: true,
    };
    let pipeline = ScanPipeline::new(config);

    let started = Instant::now();
    let batch = pipeline.process(input);
    log_phase(
        "batch",
        json!({
            "elapsed_us": started.elapsed().as_micros(),
            "input_bytes": batch.input_bytes,
            "newlines": batch.metrics.newline_count,
            "ansi_bytes": batch.metrics.ansi_byte_count,
            "errors": trigger_count(&batch, TriggerCategory::Error),
            "warnings": trigger_count(&batch, TriggerCategory::Warning),
            "completions": trigger_count(&batch, TriggerCategory::Completion),
            "progress": trigger_count(&batch, TriggerCategory::Progress),
            "compressed_bytes": batch.compressed.as_ref().map(Vec::len),
        }),
    );

    let mut state = ChunkedPipelineState::new(4096);
    for (index, chunk) in chunks.iter().enumerate() {
        let started = Instant::now();
        let summary = pipeline.process_chunk(chunk, &mut state);
        log_phase(
            "chunk",
            json!({
                "index": index,
                "elapsed_us": started.elapsed().as_micros(),
                "bytes": chunk.len(),
                "chunk_newlines": summary.newline_count,
                "state_total_bytes": state.total_bytes(),
                "state_newlines": state.newline_count(),
                "state_trigger_matches": state.total_trigger_matches(),
            }),
        );
    }

    let started = Instant::now();
    let chunked = pipeline.flush(&mut state);
    log_phase(
        "flush",
        json!({
            "elapsed_us": started.elapsed().as_micros(),
            "input_bytes": chunked.input_bytes,
            "newlines": chunked.metrics.newline_count,
            "ansi_bytes": chunked.metrics.ansi_byte_count,
            "errors": trigger_count(&chunked, TriggerCategory::Error),
            "warnings": trigger_count(&chunked, TriggerCategory::Warning),
            "completions": trigger_count(&chunked, TriggerCategory::Completion),
            "progress": trigger_count(&chunked, TriggerCategory::Progress),
            "compressed_bytes": chunked.compressed.as_ref().map(Vec::len),
        }),
    );

    assert_eq!(chunked.input_bytes, batch.input_bytes);
    assert_eq!(chunked.metrics.newline_count, batch.metrics.newline_count);
    assert_eq!(chunked.metrics.logical_lines, batch.metrics.logical_lines);
    assert_eq!(
        chunked.metrics.ansi_byte_count, batch.metrics.ansi_byte_count,
        "chunked path should preserve the real ANSI scanner result"
    );

    for category in TriggerCategory::all() {
        assert_eq!(
            trigger_count(&chunked, category),
            trigger_count(&batch, category),
            "chunked flush should match batch trigger count for {category}"
        );
    }

    assert!(
        batch
            .compressed
            .as_ref()
            .is_some_and(|blob| !blob.is_empty()),
        "batch path should use the real compressor"
    );
    assert!(
        chunked
            .compressed
            .as_ref()
            .is_some_and(|blob| !blob.is_empty()),
        "chunked flush should use the real compressor"
    );
    assert_eq!(
        state.total_bytes(),
        0,
        "flush should reset reusable chunked state"
    );
}
