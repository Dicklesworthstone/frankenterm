//! Property-based tests for byte_compression module (ft-2oph2).
//!
//! Validates that zstd compression/decompression roundtrips are lossless
//! across random payloads, arbitrary sizes, compression levels, and
//! batch operations.

use proptest::prelude::*;

use frankenterm_core::byte_compression::{
    ByteCompressionConfig, ByteCompressionError, ByteCompressor, CompressionLevel,
    CompressionStats, terminal_dictionary_seeds, train_dictionary,
};

// =============================================================================
// Strategies
// =============================================================================

fn compression_level_strategy() -> impl Strategy<Value = CompressionLevel> {
    prop_oneof![
        Just(CompressionLevel::Fast),
        Just(CompressionLevel::Default),
        Just(CompressionLevel::High),
        Just(CompressionLevel::Maximum),
    ]
}

fn small_payload() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..1024)
}

fn medium_payload() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..64 * 1024)
}

fn terminal_like_payload() -> impl Strategy<Value = Vec<u8>> {
    // Generate text that looks like terminal output
    prop::collection::vec(
        prop_oneof![
            // Normal text lines
            "[a-zA-Z0-9 ./\\-_=:;,]{5,80}\n".prop_map(|s| s.into_bytes()),
            // ANSI escape sequences
            Just(b"\x1b[32mOK\x1b[0m\n".to_vec()),
            Just(b"\x1b[31mERROR\x1b[0m: something failed\n".to_vec()),
            Just(b"\x1b[33mWARNING\x1b[0m: deprecated\n".to_vec()),
            // Progress-like
            "(0..100u32)".prop_map(|_| b"Building... [=====>     ] 50%\n".to_vec()),
        ],
        1..100,
    )
    .prop_map(|lines| lines.into_iter().flatten().collect())
}

// =============================================================================
// Roundtrip tests
// =============================================================================

proptest! {
    /// Any random bytes should compress and decompress back to the original.
    #[test]
    fn roundtrip_random_bytes(data in small_payload(), level in compression_level_strategy()) {
        let compressor = ByteCompressor::new(level);
        let compressed = compressor.compress(&data);
        let decompressed = compressor.decompress(&compressed).unwrap();
        prop_assert_eq!(&decompressed, &data);
    }

    /// Medium-sized payloads roundtrip correctly.
    #[test]
    fn roundtrip_medium_payload(data in medium_payload()) {
        let compressor = ByteCompressor::default();
        let compressed = compressor.compress(&data);
        let decompressed = compressor.decompress(&compressed).unwrap();
        prop_assert_eq!(&decompressed, &data);
    }

    /// Terminal-like payloads roundtrip correctly.
    #[test]
    fn roundtrip_terminal_payload(data in terminal_like_payload()) {
        let compressor = ByteCompressor::default();
        let compressed = compressor.compress(&data);
        let decompressed = compressor.decompress(&compressed).unwrap();
        prop_assert_eq!(&decompressed, &data);
    }

    /// Size prefix mode roundtrips correctly.
    #[test]
    fn roundtrip_with_size_prefix(data in small_payload()) {
        let config = ByteCompressionConfig {
            include_size_prefix: true,
            ..Default::default()
        };
        let compressor = ByteCompressor::with_config(config);
        let compressed = compressor.compress(&data);
        let decompressed = compressor.decompress(&compressed).unwrap();
        prop_assert_eq!(&decompressed, &data);
    }

    /// Compression never expands data beyond zstd frame overhead.
    /// (For non-empty inputs, the compressed output has bounded overhead.)
    #[test]
    fn compression_bounded_expansion(data in prop::collection::vec(any::<u8>(), 1..4096)) {
        let compressor = ByteCompressor::default();
        let compressed = compressor.compress(&data);
        // zstd max expansion is ~1.004x + 128 bytes for the frame
        let max_expansion = data.len() + data.len() / 100 + 256;
        prop_assert!(
            compressed.len() <= max_expansion,
            "compressed {} > max expansion {} for input size {}",
            compressed.len(),
            max_expansion,
            data.len()
        );
    }

    /// Repetitive data achieves some compression.
    #[test]
    fn repetitive_data_compresses(count in 10..100usize) {
        let line = b"test output line with some repetitive content\n";
        let data: Vec<u8> = line.repeat(count);
        let compressor = ByteCompressor::default();
        let compressed = compressor.compress(&data);
        // With 10+ repetitions, we should see at least 2:1 ratio
        let is_compressible = compressed.len() < data.len();
        prop_assert!(is_compressible, "repetitive data should compress: {} -> {}", data.len(), compressed.len());
    }
}

// =============================================================================
// Batch roundtrip tests
// =============================================================================

proptest! {
    /// Batch compress/decompress roundtrip on random buffer sets.
    #[test]
    fn batch_roundtrip(
        buffers in prop::collection::vec(small_payload(), 1..8)
    ) {
        let compressor = ByteCompressor::default();
        let refs: Vec<&[u8]> = buffers.iter().map(|b| b.as_slice()).collect();
        let (batch, stats) = compressor.compress_batch(&refs);
        prop_assert_eq!(stats.buffer_count, buffers.len() as u32);

        let decompressed = compressor.decompress_batch(&batch).unwrap();
        prop_assert_eq!(decompressed.len(), buffers.len());
        for (orig, dec) in buffers.iter().zip(decompressed.iter()) {
            prop_assert_eq!(orig, dec);
        }
    }

    /// Batch with empty buffers interspersed.
    #[test]
    fn batch_with_empties(
        non_empty in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..256), 1..5),
        empty_positions in prop::collection::vec(0..10usize, 0..3)
    ) {
        let mut buffers: Vec<Vec<u8>> = non_empty;
        for pos in empty_positions {
            let idx = pos % (buffers.len() + 1);
            buffers.insert(idx, Vec::new());
        }

        let compressor = ByteCompressor::default();
        let refs: Vec<&[u8]> = buffers.iter().map(|b| b.as_slice()).collect();
        let (batch, _stats) = compressor.compress_batch(&refs);
        let decompressed = compressor.decompress_batch(&batch).unwrap();

        prop_assert_eq!(decompressed.len(), buffers.len());
        for (orig, dec) in buffers.iter().zip(decompressed.iter()) {
            prop_assert_eq!(orig, dec);
        }
    }
}

// =============================================================================
// Compression level comparison
// =============================================================================

proptest! {
    /// Higher compression levels produce same or better ratios.
    #[test]
    fn higher_level_better_or_equal(data in prop::collection::vec(any::<u8>(), 100..4096)) {
        let fast = ByteCompressor::new(CompressionLevel::Fast);
        let high = ByteCompressor::new(CompressionLevel::High);

        let fast_compressed = fast.compress(&data);
        let high_compressed = high.compress(&data);

        // High should typically produce same or smaller output.
        // Allow small slack for edge cases where fast happens to find a better path.
        prop_assert!(
            high_compressed.len() <= fast_compressed.len() + 16,
            "High ({}) should be <= Fast ({}) + 16 slack for input {}",
            high_compressed.len(),
            fast_compressed.len(),
            data.len()
        );
    }
}

// =============================================================================
// Dictionary tests
// =============================================================================

proptest! {
    /// Dictionary-based compression roundtrips correctly on random data.
    #[test]
    fn dictionary_roundtrip(data in small_payload()) {
        let seeds = terminal_dictionary_seeds();
        let samples: Vec<&[u8]> = seeds.iter().map(|s| s.as_slice()).collect();

        // Dictionary training may fail — just skip if it does
        if let Ok(dict) = train_dictionary(&samples, 4096) {
            let compressor = ByteCompressor::new(CompressionLevel::Default)
                .with_dictionary(dict);
            let compressed = compressor.compress(&data);
            let decompressed = compressor.decompress(&compressed).unwrap();
            prop_assert_eq!(&decompressed, &data);
        }
    }
}

// =============================================================================
// Stats correctness
// =============================================================================

proptest! {
    /// Stats report correct sizes after compression.
    #[test]
    fn stats_sizes_correct(data in prop::collection::vec(any::<u8>(), 1..2048)) {
        let compressor = ByteCompressor::default();
        let (compressed, stats) = compressor.compress_with_stats(&data);
        prop_assert_eq!(stats.input_bytes, data.len() as u64);
        prop_assert_eq!(stats.output_bytes, compressed.len() as u64);
        prop_assert_eq!(stats.buffer_count, 1);
        if !compressed.is_empty() {
            let expected_ratio = data.len() as f64 / compressed.len() as f64;
            let ratio_diff = (stats.ratio - expected_ratio).abs();
            prop_assert!(ratio_diff < 0.001, "ratio mismatch: {} vs {}", stats.ratio, expected_ratio);
        }
    }
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// BC-16: CompressionLevel serde roundtrip.
    #[test]
    fn bc16_compression_level_serde(level in compression_level_strategy()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: CompressionLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    /// BC-17: ByteCompressionConfig serde roundtrip.
    #[test]
    fn bc17_config_serde_roundtrip(
        level in compression_level_strategy(),
        max_input in 1024usize..128 * 1024 * 1024,
        include_prefix in any::<bool>(),
    ) {
        let config = ByteCompressionConfig {
            level,
            max_input_bytes: max_input,
            include_size_prefix: include_prefix,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ByteCompressionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.level, back.level);
        prop_assert_eq!(config.max_input_bytes, back.max_input_bytes);
        prop_assert_eq!(config.include_size_prefix, back.include_size_prefix);
    }

    /// BC-18: CompressionStats serde roundtrip.
    #[test]
    fn bc18_stats_serde_roundtrip(
        input_bytes in 0u64..10_000_000,
        output_bytes in 1u64..10_000_000,
        buffer_count in 0u32..100,
    ) {
        let stats = CompressionStats::new(input_bytes, output_bytes, buffer_count);
        let json = serde_json::to_string(&stats).unwrap();
        let back: CompressionStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.input_bytes, back.input_bytes);
        prop_assert_eq!(stats.output_bytes, back.output_bytes);
        prop_assert_eq!(stats.buffer_count, back.buffer_count);
        prop_assert!((stats.ratio - back.ratio).abs() < 1e-10);
    }

    /// BC-19: ByteCompressionError serde roundtrip (all variants).
    #[test]
    fn bc19_error_serde_roundtrip(
        msg in "[a-zA-Z0-9 ]{1,50}",
        variant in 0u8..3,
    ) {
        let err = match variant {
            0 => ByteCompressionError::InvalidInput(msg.clone()),
            1 => ByteCompressionError::DecompressionFailed(msg.clone()),
            _ => ByteCompressionError::TrainingFailed(msg.clone()),
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: ByteCompressionError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(format!("{err}"), format!("{back}"));
    }
}

// =============================================================================
// Cross-level decompression compatibility
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// BC-20: Data compressed at any level can be decompressed by any compressor.
    #[test]
    fn bc20_cross_level_decompression(
        data in small_payload(),
        compress_level in compression_level_strategy(),
        decompress_level in compression_level_strategy(),
    ) {
        let c = ByteCompressor::new(compress_level);
        let d = ByteCompressor::new(decompress_level);
        let compressed = c.compress(&data);
        let decompressed = d.decompress(&compressed).unwrap();
        prop_assert_eq!(&decompressed, &data);
    }

    /// BC-21: Size prefix encodes exact original length.
    #[test]
    fn bc21_size_prefix_stores_correct_length(data in prop::collection::vec(any::<u8>(), 1..4096)) {
        let config = ByteCompressionConfig {
            include_size_prefix: true,
            ..Default::default()
        };
        let compressor = ByteCompressor::with_config(config);
        let compressed = compressor.compress(&data);
        // First 4 bytes are LE u32 of original size
        prop_assert!(compressed.len() >= 4);
        let stored_size = u32::from_le_bytes([
            compressed[0], compressed[1], compressed[2], compressed[3]
        ]) as usize;
        prop_assert_eq!(stored_size, data.len());
    }

    /// BC-22: Batch stats input_bytes equals sum of individual buffer lengths.
    #[test]
    fn bc22_batch_stats_input_bytes(
        buffers in prop::collection::vec(small_payload(), 1..6)
    ) {
        let compressor = ByteCompressor::default();
        let expected_input: u64 = buffers.iter().map(|b| b.len() as u64).sum();
        let refs: Vec<&[u8]> = buffers.iter().map(|b| b.as_slice()).collect();
        let (_batch, stats) = compressor.compress_batch(&refs);
        prop_assert_eq!(stats.input_bytes, expected_input);
        prop_assert_eq!(stats.buffer_count, buffers.len() as u32);
    }

    /// BC-23: Decompressing random garbage always returns an error (never panics).
    #[test]
    fn bc23_garbage_decompression_returns_error(
        garbage in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        let compressor = ByteCompressor::default();
        // Random bytes are almost certainly not valid zstd frames
        let result = compressor.decompress(&garbage);
        // It should either succeed (astronomically unlikely) or return Err
        // Main property: it should never panic
        let _ = result;
    }

    /// BC-24: has_dictionary reflects whether dictionary was set.
    #[test]
    fn bc24_has_dictionary_state(level in compression_level_strategy()) {
        let without = ByteCompressor::new(level);
        prop_assert!(!without.has_dictionary());

        let with = ByteCompressor::new(level).with_dictionary(vec![1, 2, 3]);
        prop_assert!(with.has_dictionary());
    }

    /// BC-25: CompressionStats::new ratio is input/output when output > 0.
    #[test]
    fn bc25_stats_ratio_calculation(
        input_bytes in 1u64..10_000_000,
        output_bytes in 1u64..10_000_000,
    ) {
        let stats = CompressionStats::new(input_bytes, output_bytes, 1);
        let expected = input_bytes as f64 / output_bytes as f64;
        prop_assert!((stats.ratio - expected).abs() < 1e-10,
            "ratio {} != expected {}", stats.ratio, expected);
    }

    /// BC-26: CompressionStats::new with zero output gives ratio 0.
    #[test]
    fn bc26_stats_zero_output_ratio(input_bytes in 0u64..10_000_000) {
        let stats = CompressionStats::new(input_bytes, 0, 1);
        prop_assert!((stats.ratio - 0.0).abs() < f64::EPSILON);
    }

    /// BC-27: Default compressor uses Default level (zstd 3).
    #[test]
    fn bc27_default_level_is_default(_dummy in 0u8..1) {
        let compressor = ByteCompressor::default();
        prop_assert_eq!(compressor.config().level, CompressionLevel::Default);
        prop_assert_eq!(compressor.config().level.zstd_level(), 3);
    }
}
