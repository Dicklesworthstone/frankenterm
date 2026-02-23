//! Property-based tests for MDL command extraction.
//!
//! Verifies invariants of the MDL extractor, compression proxy,
//! and candidate generation across random inputs.

use proptest::prelude::*;

use frankenterm_core::mdl_extraction::{
    CommandBlock, ExtractionReason, ExtractionStats, MdlConfig, MdlExtractor, WindowBuilder,
    mdl_score,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_command() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-zA-Z0-9_/. -]{1,60}")
        .unwrap()
        .prop_filter("non-empty command", |s| !s.is_empty())
}

fn arb_exit_code() -> impl Strategy<Value = Option<i32>> {
    prop_oneof![
        3 => Just(Some(0)),
        2 => (1..128i32).prop_map(Some),
        1 => Just(None),
    ]
}

fn arb_command_block(index: u32) -> impl Strategy<Value = CommandBlock> {
    (arb_command(), arb_exit_code(), 0..10_000_000u64).prop_map(
        move |(command, exit_code, timestamp_us)| CommandBlock {
            index,
            command,
            exit_code,
            duration_us: Some(1000),
            output_preview: None,
            timestamp_us: (index as u64) * 1_000_000 + timestamp_us,
        },
    )
}

fn arb_window(min_len: usize, max_len: usize) -> impl Strategy<Value = Vec<CommandBlock>> {
    prop::collection::vec(
        (arb_command(), arb_exit_code(), 0..10_000_000u64),
        min_len..=max_len,
    )
    .prop_map(|items| {
        items
            .into_iter()
            .enumerate()
            .map(|(i, (command, exit_code, ts))| CommandBlock {
                index: i as u32,
                command,
                exit_code,
                duration_us: Some(1000),
                output_preview: None,
                timestamp_us: (i as u64) * 1_000_000 + ts,
            })
            .collect()
    })
}

fn arb_window_with_success(
    min_len: usize,
    max_len: usize,
) -> impl Strategy<Value = Vec<CommandBlock>> {
    arb_window(min_len, max_len).prop_map(|mut blocks| {
        // Ensure at least one success at the end.
        if let Some(last) = blocks.last_mut() {
            last.exit_code = Some(0);
        }
        blocks
    })
}

fn arb_config() -> impl Strategy<Value = MdlConfig> {
    (
        5..100usize,       // max_window_size
        1..5usize,         // min_window_size
        10..500usize,      // max_candidates
        1..10u32,          // compression_level
        0.0..1.0f64,       // min_confidence
        prop::bool::ANY,   // include_context_failures
    )
        .prop_map(
            |(max_window_size, min_window_size, max_candidates, compression_level, min_confidence, include_context_failures)| {
                MdlConfig {
                    max_window_size,
                    min_window_size,
                    max_candidates,
                    compression_level,
                    min_confidence,
                    include_context_failures,
                }
            },
        )
}

// =============================================================================
// CommandBlock invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn command_block_status_mutually_exclusive(block in arb_command_block(0)) {
        let statuses = [block.is_success(), block.is_failure(), block.is_unknown()];
        let count = statuses.iter().filter(|&&s| s).count();
        prop_assert_eq!(count, 1, "Exactly one status must be true, got {}", count);
    }

    #[test]
    fn command_block_serde_roundtrip(block in arb_command_block(0)) {
        let json = serde_json::to_string(&block).unwrap();
        let decoded: CommandBlock = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&decoded.command, &block.command);
        prop_assert_eq!(decoded.index, block.index);
        prop_assert_eq!(decoded.exit_code, block.exit_code);
        prop_assert_eq!(decoded.timestamp_us, block.timestamp_us);
    }

    #[test]
    fn command_block_success_iff_exit_zero(block in arb_command_block(0)) {
        let is_zero = block.exit_code == Some(0);
        prop_assert_eq!(block.is_success(), is_zero);
    }
}

// =============================================================================
// MDL scoring invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mdl_score_compressed_never_exceeds_uncompressed(
        window in arb_window(1, 10)
    ) {
        let refs: Vec<&CommandBlock> = window.iter().collect();
        let score = mdl_score(&refs, 3);
        prop_assert!(
            score.compressed_len <= score.uncompressed_len || score.uncompressed_len < 3,
            "compressed {} > uncompressed {} (len < 3 is allowed)",
            score.compressed_len,
            score.uncompressed_len
        );
    }

    #[test]
    fn mdl_score_ratio_bounded(window in arb_window(1, 10)) {
        let refs: Vec<&CommandBlock> = window.iter().collect();
        let score = mdl_score(&refs, 3);
        prop_assert!(score.ratio >= 0.0, "ratio {} < 0", score.ratio);
        let within_bounds = score.ratio <= 1.01 || score.uncompressed_len < 3;
        prop_assert!(
            within_bounds,
            "ratio {} > 1.01 (and uncompressed_len {})",
            score.ratio,
            score.uncompressed_len
        );
    }

    #[test]
    fn mdl_score_command_count_matches(window in arb_window(1, 15)) {
        let refs: Vec<&CommandBlock> = window.iter().collect();
        let score = mdl_score(&refs, 3);
        prop_assert_eq!(score.command_count, window.len());
    }

    #[test]
    fn mdl_score_empty_is_zero(_dummy in 0..1u8) {
        let score = mdl_score(&[], 3);
        prop_assert_eq!(score.compressed_len, 0);
        prop_assert_eq!(score.uncompressed_len, 0);
        prop_assert_eq!(score.command_count, 0);
    }

    #[test]
    fn mdl_score_subset_no_larger_uncompressed(window in arb_window(2, 10)) {
        let full_refs: Vec<&CommandBlock> = window.iter().collect();
        let full_score = mdl_score(&full_refs, 3);

        // Take a subset (first half).
        let half = window.len() / 2;
        if half > 0 {
            let sub_refs: Vec<&CommandBlock> = window[..half].iter().collect();
            let sub_score = mdl_score(&sub_refs, 3);
            prop_assert!(
                sub_score.uncompressed_len <= full_score.uncompressed_len,
                "subset uncompressed {} > full {}",
                sub_score.uncompressed_len,
                full_score.uncompressed_len
            );
        }
    }
}

// =============================================================================
// Extraction invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn extract_result_always_has_valid_window_size(
        window in arb_window(0, 20)
    ) {
        let ext = MdlExtractor::with_defaults();
        let result = ext.extract(&window);
        prop_assert_eq!(result.window_size, window.len());
    }

    #[test]
    fn extract_empty_always_window_too_small(_dummy in 0..1u8) {
        let ext = MdlExtractor::with_defaults();
        let result = ext.extract(&[]);
        prop_assert_eq!(result.reason_code, ExtractionReason::WindowTooSmall);
        prop_assert!(result.commands.is_empty());
    }

    #[test]
    fn extract_no_success_detected_correctly(
        window in arb_window(1, 10).prop_map(|mut w| {
            // Force all to fail.
            for b in &mut w { b.exit_code = Some(1); }
            w
        })
    ) {
        let ext = MdlExtractor::with_defaults();
        let result = ext.extract(&window);
        prop_assert_eq!(result.reason_code, ExtractionReason::NoSuccessFound);
        prop_assert!(result.commands.is_empty());
    }

    #[test]
    fn extract_with_success_returns_nonempty(
        window in arb_window_with_success(1, 15)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);
        prop_assert!(
            !result.commands.is_empty(),
            "Window with success should produce non-empty extraction, reason: {:?}",
            result.reason_code
        );
    }

    #[test]
    fn extract_commands_are_subset_of_window(
        window in arb_window_with_success(2, 15)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);
        for cmd in &result.commands {
            let found = window.iter().any(|w| w.index == cmd.index && w.command == cmd.command);
            prop_assert!(
                found,
                "Extracted command index {} '{}' not found in window",
                cmd.index,
                cmd.command
            );
        }
    }

    #[test]
    fn extract_preserves_causal_ordering(
        window in arb_window_with_success(2, 15)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);
        for pair in result.commands.windows(2) {
            prop_assert!(
                pair[0].index < pair[1].index,
                "Causal ordering violated: index {} >= {}",
                pair[0].index,
                pair[1].index
            );
        }
    }

    #[test]
    fn extract_reduction_ratio_bounded(
        window in arb_window_with_success(2, 20)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);
        prop_assert!(result.reduction_ratio >= 0.0, "reduction {} < 0", result.reduction_ratio);
        prop_assert!(result.reduction_ratio <= 1.0, "reduction {} > 1", result.reduction_ratio);
    }

    #[test]
    fn extract_confidence_bounded(
        window in arb_window_with_success(2, 20)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);
        prop_assert!(result.confidence >= 0.0, "confidence {} < 0", result.confidence);
        prop_assert!(result.confidence <= 1.0, "confidence {} > 1", result.confidence);
    }

    #[test]
    fn extract_all_success_returns_all(
        window in arb_window(2, 10).prop_map(|mut w| {
            for b in &mut w { b.exit_code = Some(0); }
            w
        })
    ) {
        let ext = MdlExtractor::with_defaults();
        let result = ext.extract(&window);
        prop_assert_eq!(result.reason_code, ExtractionReason::AllSuccessful);
        prop_assert_eq!(result.commands.len(), window.len());
    }

    #[test]
    fn extract_last_command_is_success(
        window in arb_window_with_success(1, 15)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);
        if !result.commands.is_empty() {
            prop_assert!(
                result.commands.last().unwrap().is_success(),
                "Last extracted command should be successful"
            );
        }
    }
}

// =============================================================================
// Config serde invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: MdlConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.max_window_size, config.max_window_size);
        prop_assert_eq!(decoded.min_window_size, config.min_window_size);
        prop_assert_eq!(decoded.max_candidates, config.max_candidates);
        prop_assert_eq!(decoded.compression_level, config.compression_level);
        prop_assert_eq!(decoded.include_context_failures, config.include_context_failures);
        // f64 tolerance for min_confidence.
        let diff = (decoded.min_confidence - config.min_confidence).abs();
        prop_assert!(diff < 1e-10, "min_confidence drift: {}", diff);
    }

    #[test]
    fn config_with_arb_produces_valid_extraction(
        config in arb_config(),
        window in arb_window_with_success(1, 15)
    ) {
        let ext = MdlExtractor::new(config);
        let result = ext.extract(&window);
        // Should always produce a valid result (no panic).
        prop_assert!(result.window_size >= 1);
    }
}

// =============================================================================
// WindowBuilder invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn window_builder_indexes_sequential(
        commands in prop::collection::vec(arb_command(), 1..20)
    ) {
        let mut builder = WindowBuilder::new();
        for cmd in &commands {
            builder.add_command(cmd.clone(), Some(0), None, 0);
        }
        prop_assert_eq!(builder.len(), commands.len());
        for (i, block) in builder.blocks().iter().enumerate() {
            prop_assert_eq!(block.index, i as u32, "Index mismatch at position {}", i);
        }
    }

    #[test]
    fn window_builder_clear_resets(
        commands in prop::collection::vec(arb_command(), 1..10)
    ) {
        let mut builder = WindowBuilder::new();
        for cmd in &commands {
            builder.add_command(cmd.clone(), Some(0), None, 0);
        }
        builder.clear();
        prop_assert!(builder.is_empty());
        prop_assert_eq!(builder.len(), 0);

        // After clear, next index should restart from 0.
        builder.add_command("after-clear".to_string(), Some(0), None, 0);
        prop_assert_eq!(builder.blocks()[0].index, 0);
    }

    #[test]
    fn window_builder_into_blocks_preserves(
        commands in prop::collection::vec(arb_command(), 1..10)
    ) {
        let mut builder = WindowBuilder::new();
        for cmd in &commands {
            builder.add_command(cmd.clone(), Some(0), None, 0);
        }
        let blocks = builder.into_blocks();
        prop_assert_eq!(blocks.len(), commands.len());
        for (block, cmd) in blocks.iter().zip(commands.iter()) {
            prop_assert_eq!(&block.command, cmd);
        }
    }
}

// =============================================================================
// ExtractionStats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stats_total_always_increments(
        windows in prop::collection::vec(arb_window(1, 8), 1..10)
    ) {
        let mut stats = ExtractionStats::new();
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });

        for window in &windows {
            let result = ext.extract(window);
            stats.record(&result);
        }

        prop_assert_eq!(stats.total_extractions, windows.len() as u64);
    }

    #[test]
    fn stats_successful_leq_total(
        windows in prop::collection::vec(arb_window_with_success(1, 8), 1..10)
    ) {
        let mut stats = ExtractionStats::new();
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });

        for window in &windows {
            let result = ext.extract(window);
            stats.record(&result);
        }

        prop_assert!(
            stats.successful <= stats.total_extractions,
            "successful {} > total {}",
            stats.successful,
            stats.total_extractions
        );
    }

    #[test]
    fn stats_mean_confidence_bounded(
        windows in prop::collection::vec(arb_window_with_success(2, 8), 1..10)
    ) {
        let mut stats = ExtractionStats::new();
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });

        for window in &windows {
            let result = ext.extract(window);
            stats.record(&result);
        }

        if stats.successful > 0 {
            prop_assert!(stats.mean_confidence >= 0.0, "mean_confidence {} < 0", stats.mean_confidence);
            prop_assert!(stats.mean_confidence <= 1.0, "mean_confidence {} > 1", stats.mean_confidence);
        }
    }

    #[test]
    fn stats_commands_processed_accounts(
        windows in prop::collection::vec(arb_window(1, 8), 1..5)
    ) {
        let mut stats = ExtractionStats::new();
        let ext = MdlExtractor::with_defaults();
        let mut expected_processed = 0u64;

        for window in &windows {
            expected_processed += window.len() as u64;
            let result = ext.extract(window);
            stats.record(&result);
        }

        prop_assert_eq!(
            stats.total_commands_processed, expected_processed,
            "total_commands_processed mismatch"
        );
    }

    #[test]
    fn stats_reason_counts_sum_to_total(
        windows in prop::collection::vec(arb_window(0, 8), 1..10)
    ) {
        let mut stats = ExtractionStats::new();
        let ext = MdlExtractor::with_defaults();

        for window in &windows {
            let result = ext.extract(window);
            stats.record(&result);
        }

        let sum: u64 = stats.reason_counts.values().sum();
        prop_assert_eq!(
            sum, stats.total_extractions,
            "reason_counts sum {} != total {}",
            sum, stats.total_extractions
        );
    }
}

// =============================================================================
// ExtractionResult serde invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn extraction_result_serde_roundtrip(
        window in arb_window_with_success(2, 10)
    ) {
        let ext = MdlExtractor::new(MdlConfig {
            min_confidence: 0.0,
            ..Default::default()
        });
        let result = ext.extract(&window);

        let json = serde_json::to_string(&result).unwrap();
        let decoded: frankenterm_core::mdl_extraction::ExtractionResult =
            serde_json::from_str(&json).unwrap();

        prop_assert_eq!(decoded.commands.len(), result.commands.len());
        prop_assert_eq!(decoded.window_size, result.window_size);
        prop_assert_eq!(decoded.reason_code, result.reason_code);
        // f64 tolerance for confidence and reduction_ratio.
        let conf_diff = (decoded.confidence - result.confidence).abs();
        prop_assert!(conf_diff < 1e-10, "confidence drift: {}", conf_diff);
        let red_diff = (decoded.reduction_ratio - result.reduction_ratio).abs();
        prop_assert!(red_diff < 1e-10, "reduction_ratio drift: {}", red_diff);
    }

    #[test]
    fn extraction_reason_serde_all_variants(variant in 0..6u8) {
        let reason = match variant {
            0 => ExtractionReason::Success,
            1 => ExtractionReason::WindowTooSmall,
            2 => ExtractionReason::NoSuccessFound,
            3 => ExtractionReason::AllSuccessful,
            4 => ExtractionReason::LowConfidence,
            _ => ExtractionReason::CandidateLimitReached,
        };
        let json = serde_json::to_string(&reason).unwrap();
        let decoded: ExtractionReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, reason);
    }
}
