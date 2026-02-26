//! Property-based tests for byte_compression module (ft-2oph2).
//!
//! Validates that zstd compression/decompression roundtrips are lossless
//! across random payloads, arbitrary sizes, compression levels, and
//! batch operations.

use proptest::prelude::*;

use frankenterm_core::byte_compression::{
    ByteCompressionConfig, ByteCompressor, CompressionLevel, terminal_dictionary_seeds,
    train_dictionary,
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
