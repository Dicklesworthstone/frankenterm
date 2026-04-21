//! Integration test: tuning config → backpressure → scan pipeline.
//!
//! Exercises the hot-path flow where operator-tunable constants drive
//! backpressure classification and scan pipeline buffer sizing:
//!
//!   TuningConfig.validate()
//!     → BackpressureConfig thresholds + BackpressureManager.evaluate()
//!       → ScanPipeline.process_chunk() with tuning-derived buffer limits
//!         → ChunkedPipelineState.should_flush() at segment boundary
//!
//! This mirrors the real ingest loop: tuning sets operator knobs,
//! backpressure gates throughput under load, and the scan pipeline
//! processes output in tuning-sized chunks.

use frankenterm_core::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureTier, QueueDepths,
};
use frankenterm_core::scan_pipeline::{ChunkedPipelineState, ScanPipeline, ScanPipelineConfig};
use frankenterm_core::tuning_config::TuningConfig;

// ── Helpers ─────────────────────────────────────────────────────────────

/// Queue depths at the given fill ratios.
fn depths_at(capture_ratio: f64, write_ratio: f64) -> QueueDepths {
    let cap = 1000;
    QueueDepths {
        capture_depth: (capture_ratio * cap as f64) as usize,
        capture_capacity: cap,
        write_depth: (write_ratio * cap as f64) as usize,
        write_capacity: cap,
    }
}

/// Backpressure config with zero hysteresis and custom thresholds.
fn test_bp_config(yellow: f64, red: f64) -> BackpressureConfig {
    BackpressureConfig {
        yellow_capture: yellow,
        red_capture: red,
        hysteresis_ms: 0,
        ..BackpressureConfig::default()
    }
}

/// Generate test output with embedded patterns.
fn generate_output(lines: usize) -> Vec<u8> {
    let mut output = Vec::new();
    for i in 0..lines {
        output.extend_from_slice(format!("line {i}: some terminal output here\n").as_bytes());
    }
    output
}

// ── Tests ───────────────────────────────────────────────────────────────

/// TuningConfig validates operator constants, and valid tuning drives
/// backpressure thresholds and scan pipeline buffer sizing.
#[test]
fn tuning_validates_and_drives_pipeline_config() {
    // Default tuning is valid.
    let tuning = TuningConfig::default();
    let errors = tuning.validate();
    assert!(
        errors.is_empty(),
        "default tuning should be valid: {errors:?}"
    );

    // Tuning constants flow into pipeline configuration.
    let segment_limit = tuning.ingest.max_persist_segment_bytes;
    assert!(
        segment_limit >= 1024,
        "segment limit should be at least 1KB"
    );

    // Create scan pipeline with tuning-derived compression threshold.
    let scan_config = ScanPipelineConfig {
        compression_threshold: segment_limit,
        ..ScanPipelineConfig::default()
    };
    let pipeline = ScanPipeline::new(scan_config);
    assert_eq!(pipeline.config().compression_threshold, segment_limit);

    // Create chunked state with tuning-derived buffer size.
    let state = ChunkedPipelineState::new(segment_limit);
    assert_eq!(state.total_bytes(), 0);
    assert!(!state.should_flush());

    // Backpressure warn ratio from tuning is in valid range.
    let warn_ratio = tuning.backpressure.warn_ratio;
    assert!(
        (0.1..=0.99).contains(&warn_ratio),
        "warn_ratio {warn_ratio} should be in [0.1, 0.99]"
    );
}

/// Invalid tuning config produces validation errors that prevent
/// misconfigured backpressure and pipeline limits.
#[test]
fn invalid_tuning_produces_validation_errors() {
    let mut tuning = TuningConfig::default();

    // Set segment limit below minimum (1KB).
    tuning.ingest.max_persist_segment_bytes = 100;
    // Set warn ratio outside valid range.
    tuning.backpressure.warn_ratio = 0.001;
    // Set coalesce window below minimum (5ms).
    tuning.runtime.output_coalesce_window_ms = 1;

    let errors = tuning.validate();
    assert!(
        errors.len() >= 3,
        "should have at least 3 errors, got {}: {errors:?}",
        errors.len()
    );

    // Verify specific error categories are caught.
    let joined = errors.join(" | ");
    assert!(
        joined.contains("max_persist_segment_bytes")
            || joined.contains("segment")
            || joined.contains("ingest"),
        "should flag segment limit: {joined}"
    );
}

/// Backpressure tier classification uses config thresholds that can be
/// tuned by the operator, and tier drives scan pipeline buffer decisions.
#[test]
fn backpressure_tier_drives_scan_buffer_decisions() {
    let tuning = TuningConfig::default();

    // Create backpressure with custom thresholds.
    let bp = BackpressureManager::new(test_bp_config(0.40, 0.70));

    // Green at low load.
    assert_eq!(bp.classify(&depths_at(0.20, 0.10)), BackpressureTier::Green);

    // Yellow when capture crosses 40%.
    assert_eq!(
        bp.classify(&depths_at(0.45, 0.10)),
        BackpressureTier::Yellow
    );

    // Red when capture crosses 70%.
    assert_eq!(bp.classify(&depths_at(0.75, 0.10)), BackpressureTier::Red);

    // Under different backpressure tiers, adjust scan buffer sizing.
    // Green: use full segment limit from tuning.
    let green_buffer = tuning.ingest.max_persist_segment_bytes;

    // Yellow: halve buffer to reduce memory pressure.
    let yellow_buffer = tuning.ingest.max_persist_segment_bytes / 2;

    // Red: quarter buffer for aggressive shedding.
    let red_buffer = tuning.ingest.max_persist_segment_bytes / 4;

    assert!(green_buffer > yellow_buffer);
    assert!(yellow_buffer > red_buffer);
    assert!(
        red_buffer >= 1024,
        "red buffer should still be at least 1KB"
    );

    // Create chunked states at each tier's buffer size.
    let green_state = ChunkedPipelineState::new(green_buffer);
    let yellow_state = ChunkedPipelineState::new(yellow_buffer);
    let red_state = ChunkedPipelineState::new(red_buffer);

    // All start empty.
    assert_eq!(green_state.total_bytes(), 0);
    assert_eq!(yellow_state.total_bytes(), 0);
    assert_eq!(red_state.total_bytes(), 0);
}

/// Full pipeline: tuning configures limits, backpressure evaluates load,
/// scan pipeline processes output in tuning-sized chunks, and backpressure
/// snapshot captures the telemetry coherently.
#[test]
fn full_pipeline_tuning_to_scan_under_backpressure() {
    let tuning = TuningConfig::default();
    assert!(tuning.validate().is_empty());

    // Create backpressure manager.
    let bp = BackpressureManager::new(test_bp_config(0.50, 0.75));

    // Create scan pipeline.
    let pipeline = ScanPipeline::new(ScanPipelineConfig {
        compression_threshold: tuning.ingest.max_persist_segment_bytes,
        ..ScanPipelineConfig::default()
    });

    // Phase 1: Green — process output at full buffer size.
    let tier = bp.classify(&depths_at(0.20, 0.10));
    assert_eq!(tier, BackpressureTier::Green);

    let buffer_size = match tier {
        BackpressureTier::Green => tuning.ingest.max_persist_segment_bytes,
        BackpressureTier::Yellow => tuning.ingest.max_persist_segment_bytes / 2,
        BackpressureTier::Red | BackpressureTier::Black => {
            tuning.ingest.max_persist_segment_bytes / 4
        }
    };

    let mut state = ChunkedPipelineState::new(buffer_size);
    let output = generate_output(100);

    // Process output in chunks.
    let metrics = pipeline.process_chunk(&output, &mut state);
    assert!(metrics.newline_count > 0);
    assert!(state.total_bytes() > 0);
    assert_eq!(state.newline_count(), metrics.newline_count);

    // Flush and verify output.
    let scan_output = pipeline.flush(&mut state);
    assert!(scan_output.input_bytes > 0);
    assert!(scan_output.metrics.newline_count >= 100);

    // Phase 2: escalate to Yellow — process with reduced buffer.
    bp.evaluate(&depths_at(0.55, 0.10));
    assert_eq!(bp.current_tier(), BackpressureTier::Yellow);

    let yellow_buffer = tuning.ingest.max_persist_segment_bytes / 2;
    let mut yellow_state = ChunkedPipelineState::new(yellow_buffer);
    let small_output = generate_output(20);

    pipeline.process_chunk(&small_output, &mut yellow_state);
    let yellow_scan = pipeline.flush(&mut yellow_state);
    assert!(yellow_scan.metrics.newline_count >= 20);

    // Phase 3: escalate to Red — even smaller buffer.
    bp.evaluate(&depths_at(0.80, 0.10));
    assert_eq!(bp.current_tier(), BackpressureTier::Red);

    let red_buffer = tuning.ingest.max_persist_segment_bytes / 4;
    let mut red_state = ChunkedPipelineState::new(red_buffer);
    let tiny_output = generate_output(5);

    pipeline.process_chunk(&tiny_output, &mut red_state);
    let red_scan = pipeline.flush(&mut red_state);
    assert!(red_scan.metrics.newline_count >= 5);

    // Verify backpressure telemetry reflects the evaluations.
    let bp_telem = bp.telemetry().snapshot();
    assert!(bp_telem.evaluations >= 2); // Yellow + Red transitions
    assert!(bp_telem.transitions >= 2);

    // Snapshot captures coherent state.
    let snap = bp.snapshot(&depths_at(0.80, 0.10));
    assert_eq!(snap.tier, BackpressureTier::Red);
    assert!(snap.capture_depth > 0);
    assert_eq!(snap.capture_capacity, 1000);
}

/// Scan pipeline metrics are consistent regardless of whether output is
/// processed in one batch or via chunked streaming, and tuning-derived
/// buffer sizes don't affect final metric accuracy.
#[test]
fn scan_metrics_consistent_across_batch_and_chunked() {
    let tuning = TuningConfig::default();
    let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
    let output = generate_output(50);

    // Batch processing.
    let batch_result = pipeline.process(&output);
    let batch_lines = batch_result.metrics.newline_count;
    let batch_logical = batch_result.metrics.logical_lines;

    // Chunked processing with tuning-derived buffer.
    let mut state = ChunkedPipelineState::new(tuning.ingest.max_persist_segment_bytes);
    pipeline.process_chunk(&output, &mut state);
    let chunked_result = pipeline.flush(&mut state);
    let chunked_lines = chunked_result.metrics.newline_count;
    let chunked_logical = chunked_result.metrics.logical_lines;

    // Metrics should match regardless of processing mode.
    assert_eq!(
        batch_lines, chunked_lines,
        "newline count should match: batch={batch_lines}, chunked={chunked_lines}"
    );
    assert_eq!(
        batch_logical, chunked_logical,
        "logical lines should match: batch={batch_logical}, chunked={chunked_logical}"
    );

    // Both should report correct input bytes.
    assert_eq!(batch_result.input_bytes, chunked_result.input_bytes);

    // Process again with a much smaller buffer (simulating Red backpressure).
    let small_buffer = tuning.ingest.max_persist_segment_bytes / 16;
    let mut small_state = ChunkedPipelineState::new(small_buffer.max(256));
    pipeline.process_chunk(&output, &mut small_state);
    let small_result = pipeline.flush(&mut small_state);

    // Even with a tiny buffer, metrics should still be correct.
    assert_eq!(
        batch_lines, small_result.metrics.newline_count,
        "small buffer should produce same newline count"
    );
}

/// Backpressure snapshot includes paused pane IDs, and the snapshot is
/// coherent with the tuning-derived warn ratio and scan pipeline state.
#[test]
fn backpressure_snapshot_coherent_with_tuning() {
    let tuning = TuningConfig::default();
    let bp = BackpressureManager::new(BackpressureConfig::default());

    // Evaluate at a level above the default yellow threshold (0.50).
    let depths = depths_at(0.60, 0.30);
    bp.evaluate(&depths);
    assert_eq!(bp.current_tier(), BackpressureTier::Yellow);

    // Pause some panes under backpressure.
    bp.pause_pane(10);
    bp.pause_pane(20);
    assert!(bp.is_pane_paused(10));
    assert_eq!(bp.paused_pane_ids().len(), 2);

    // Snapshot captures paused panes.
    let snap = bp.snapshot(&depths);
    assert_eq!(snap.tier, BackpressureTier::Yellow);
    assert_eq!(snap.paused_panes.len(), 2);
    assert!(snap.paused_panes.contains(&10));
    assert!(snap.paused_panes.contains(&20));

    // Capture and write ratios are derivable from snapshot fields.
    let capture_ratio = snap.capture_depth as f64 / snap.capture_capacity as f64;
    let write_ratio = snap.write_depth as f64 / snap.write_capacity as f64;
    assert!(capture_ratio > tuning.backpressure.warn_ratio.min(0.50));
    assert!(write_ratio < 1.0);

    // Policy accessors match config.
    assert!(bp.idle_poll_backoff_factor() > 1.0);
    assert!(bp.max_buffered_segments() > 0);

    // Resume and verify clean state.
    bp.resume_all_panes();
    assert_eq!(bp.paused_pane_ids().len(), 0);
    assert!(!bp.is_pane_paused(10));
}
