//! Integration tests for the FTVI (FrankenTerm Vector Index) format.
//!
//! Tests read/write roundtrips, search accuracy, f16 precision,
//! and edge cases for the binary vector index.

use frankenterm_core::search::{FtviIndex, FtviWriter};

/// Helper: build an FTVI index from (id, vector) pairs using write_ftvi_vec.
fn build_index(dimension: u16, records: &[(u64, &[f32])]) -> FtviIndex {
    let buf = frankenterm_core::search::write_ftvi_vec(dimension, records).expect("write_ftvi_vec");
    FtviIndex::from_bytes(&buf).expect("from_bytes")
}

// =============================================================================
// Write/Read Roundtrip Tests
// =============================================================================

#[test]
fn ftvi_basic_roundtrip() {
    let records: Vec<(u64, &[f32])> = vec![
        (1, &[1.0, 0.0, 0.0, 0.0]),
        (2, &[0.0, 1.0, 0.0, 0.0]),
        (3, &[0.0, 0.0, 1.0, 0.0]),
    ];

    let index = build_index(4, &records);
    assert_eq!(index.len(), 3);
    assert_eq!(index.dimension(), 4);
}

#[test]
fn ftvi_search_finds_nearest() {
    let records: Vec<(u64, &[f32])> = vec![
        (10, &[1.0, 0.0, 0.0]),
        (20, &[0.0, 1.0, 0.0]),
        (30, &[0.0, 0.0, 1.0]),
    ];

    let index = build_index(3, &records);

    // Search for a query close to record 10
    let results = index.search(&[0.9, 0.1, 0.0], 3);
    assert_eq!(results[0].0, 10, "nearest should be record 10");

    // Search for a query close to record 30
    let results = index.search(&[0.0, 0.1, 0.95], 3);
    assert_eq!(results[0].0, 30, "nearest should be record 30");
}

#[test]
fn ftvi_search_top_k_limits() {
    let records: Vec<(u64, Vec<f32>)> = (0..100)
        .map(|i| {
            let angle = i as f32 * 0.0628;
            (i as u64, vec![angle.cos(), angle.sin()])
        })
        .collect();

    let refs: Vec<(u64, &[f32])> = records.iter().map(|(id, v)| (*id, v.as_slice())).collect();
    let index = build_index(2, &refs);

    let results_5 = index.search(&[1.0, 0.0], 5);
    assert_eq!(results_5.len(), 5);

    let results_10 = index.search(&[1.0, 0.0], 10);
    assert_eq!(results_10.len(), 10);

    // Top-5 should be a subset of top-10
    for (id, _) in &results_5 {
        assert!(results_10.iter().any(|(rid, _)| rid == id));
    }
}

#[test]
fn ftvi_search_scores_decrease() {
    let records: Vec<(u64, Vec<f32>)> = (0..50)
        .map(|i| {
            let v = vec![
                (i as f32 * 0.1).sin(),
                (i as f32 * 0.2).cos(),
                (i as f32 * 0.3).sin(),
                (i as f32 * 0.4).cos(),
            ];
            (i as u64, v)
        })
        .collect();

    let refs: Vec<(u64, &[f32])> = records.iter().map(|(id, v)| (*id, v.as_slice())).collect();
    let index = build_index(4, &refs);
    let results = index.search(&[0.5, 0.5, 0.5, 0.5], 20);

    for window in results.windows(2) {
        assert!(
            window[0].1 >= window[1].1,
            "scores should be non-increasing: {} >= {}",
            window[0].1,
            window[1].1
        );
    }
}

// =============================================================================
// F16 Precision Tests
// =============================================================================

#[test]
fn ftvi_f16_preserves_reasonable_precision() {
    let original: Vec<f32> = vec![0.123456, 0.789012, -0.345678, 0.567890];

    let records: Vec<(u64, &[f32])> = vec![(1, &original)];
    let index = build_index(4, &records);

    let results = index.search(&original, 1);
    assert_eq!(results[0].0, 1);

    let self_dot: f32 = original.iter().map(|x| x * x).sum();
    let score = results[0].1;

    let relative_error = (score - self_dot).abs() / self_dot;
    assert!(
        relative_error < 0.05,
        "f16 precision loss too high: score={score}, expected~={self_dot}, error={relative_error}"
    );
}

#[test]
fn ftvi_f16_handles_zero_vectors() {
    let records: Vec<(u64, &[f32])> = vec![(1, &[0.0, 0.0, 0.0]), (2, &[1.0, 0.0, 0.0])];

    let index = build_index(3, &records);
    let results = index.search(&[1.0, 0.0, 0.0], 2);
    assert_eq!(results[0].0, 2);
}

#[test]
fn ftvi_f16_handles_large_values() {
    let records: Vec<(u64, &[f32])> = vec![(1, &[100.0, 200.0]), (2, &[0.001, 0.002])];

    let index = build_index(2, &records);
    let results = index.search(&[100.0, 200.0], 2);
    assert_eq!(results.len(), 2);
}

// =============================================================================
// Edge Cases
// =============================================================================

#[test]
fn ftvi_empty_index() {
    let records: Vec<(u64, &[f32])> = vec![];
    let index = build_index(4, &records);
    assert_eq!(index.len(), 0);

    let results = index.search(&[1.0, 0.0, 0.0, 0.0], 5);
    assert!(results.is_empty());
}

#[test]
fn ftvi_single_record() {
    let records: Vec<(u64, &[f32])> = vec![(42, &[0.7, 0.3])];
    let index = build_index(2, &records);
    assert_eq!(index.len(), 1);

    let results = index.search(&[0.7, 0.3], 10);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, 42);
}

#[test]
fn ftvi_bad_magic_rejected() {
    let bad_data = b"BADMxxxxxxxx";
    assert!(FtviIndex::from_bytes(bad_data).is_err());
}

#[test]
fn ftvi_truncated_data_rejected() {
    let short = b"FTVI\x01\x00\x04\x00";
    assert!(FtviIndex::from_bytes(short).is_err());
}

#[test]
fn ftvi_dimension_mismatch_returns_empty() {
    let records: Vec<(u64, &[f32])> = vec![(1, &[1.0, 0.0, 0.0])];
    let index = build_index(3, &records);

    // Query with wrong dimension
    let results = index.search(&[1.0, 0.0], 5);
    assert!(results.is_empty(), "dimension mismatch should return empty");
}

// =============================================================================
// Writer Tests
// =============================================================================

#[test]
fn ftvi_writer_tracks_count() {
    let mut buf = Vec::new();
    let mut writer = FtviWriter::new(&mut buf, 4).unwrap();

    assert_eq!(writer.count(), 0);
    writer.push(1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
    assert_eq!(writer.count(), 1);
    writer.push(2, &[0.0, 1.0, 0.0, 0.0]).unwrap();
    assert_eq!(writer.count(), 2);

    writer.finish().unwrap();
}

#[test]
fn ftvi_large_index_search() {
    let dim = 64;
    let n = 1000;

    let records: Vec<(u64, Vec<f32>)> = (0..n)
        .map(|i| {
            let mut v = vec![0.0f32; dim];
            v[i % dim] = 1.0;
            v[(i + 1) % dim] = 0.5;
            (i as u64, v)
        })
        .collect();

    let refs: Vec<(u64, &[f32])> = records.iter().map(|(id, v)| (*id, v.as_slice())).collect();
    let index = build_index(dim as u16, &refs);
    assert_eq!(index.len(), n);

    let mut query = vec![0.0f32; dim];
    query[0] = 1.0;
    query[1] = 0.5;

    let results = index.search(&query, 5);
    assert_eq!(results.len(), 5);
    assert_eq!(results[0].0, 0, "record 0 should be nearest for this query");
}

#[test]
fn ftvi_reproducible_output() {
    let records: Vec<(u64, &[f32])> = vec![(1, &[0.5f32, 0.5]), (2, &[0.1, 0.9])];

    let buf1 = frankenterm_core::search::write_ftvi_vec(2, &records).unwrap();
    let buf2 = frankenterm_core::search::write_ftvi_vec(2, &records).unwrap();

    assert_eq!(buf1, buf2, "same input should produce identical output");
}
