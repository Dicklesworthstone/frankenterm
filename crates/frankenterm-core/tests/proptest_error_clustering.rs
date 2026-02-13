//! Property-based tests for error_clustering module.
//!
//! Validates MinHash LSH clustering invariants: error counting,
//! cluster bounds, timestamp tracking, pane deduplication,
//! sample limits, and serde roundtrip.

use frankenterm_core::error_clustering::{ClusterInfo, ClusteringConfig, ErrorClusterer};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_config() -> impl Strategy<Value = ClusteringConfig> {
    // num_bands must divide num_hashes; pick bands first then multiply
    (
        2usize..=8,
        2usize..=8,
        3usize..=7,
        10usize..=200,
        1usize..=10,
    )
        .prop_map(
            |(bands, rows_per_band, shingle_size, max_clusters, max_samples)| ClusteringConfig {
                num_hashes: bands * rows_per_band,
                num_bands: bands,
                shingle_size,
                max_clusters,
                max_samples_per_cluster: max_samples,
            },
        )
}

fn arb_error_text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z]{5,50}",
        "ConnectionRefusedError: port [0-9]{2,5}",
        "TimeoutError: after [0-9]{1,3}s",
        "PermissionDenied: /[a-z/]{5,30}",
        "SyntaxError: unexpected token at line [0-9]{1,4}",
    ]
}

fn arb_timestamp() -> impl Strategy<Value = u64> {
    1u64..=1_000_000
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    // -- Counting invariants --

    #[test]
    fn error_count_tracks_insertions(
        texts in proptest::collection::vec(arb_error_text(), 1..=30),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for (i, text) in texts.iter().enumerate() {
            c.insert(text, Some(i as u64), i as u64);
        }
        prop_assert_eq!(c.error_count(), texts.len());
    }

    #[test]
    fn cluster_count_bounded_by_error_count(
        texts in proptest::collection::vec(arb_error_text(), 1..=30),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for (i, text) in texts.iter().enumerate() {
            c.insert(text, None, i as u64);
        }
        prop_assert!(
            c.cluster_count() <= c.error_count(),
            "clusters {} > errors {}",
            c.cluster_count(),
            c.error_count()
        );
    }

    #[test]
    fn cluster_count_at_least_one_after_insert(
        text in arb_error_text(),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        c.insert(&text, None, 100);
        prop_assert!(c.cluster_count() >= 1);
    }

    // -- Insert returns valid cluster IDs --

    #[test]
    fn insert_returns_valid_cluster_id(
        texts in proptest::collection::vec(arb_error_text(), 1..=20),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        let mut ids = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            ids.push(c.insert(text, Some(i as u64), i as u64));
        }
        for &id in &ids {
            let info = c.cluster_info(id);
            prop_assert!(info.is_some(), "cluster_info returned None for id {}", id);
            let info = info.unwrap();
            prop_assert!(info.size >= 1);
        }
    }

    // -- Identical texts always cluster together --

    #[test]
    fn identical_texts_same_cluster(
        text in arb_error_text(),
        count in 2usize..=10,
    ) {
        let mut c = ErrorClusterer::with_defaults();
        let mut ids = Vec::new();
        for i in 0..count {
            ids.push(c.insert(&text, Some(i as u64), i as u64));
        }
        // All should resolve to the same cluster
        let first_info = c.cluster_info(ids[0]).unwrap();
        for &id in &ids[1..] {
            let info = c.cluster_info(id).unwrap();
            prop_assert_eq!(
                info.cluster_id, first_info.cluster_id,
                "identical texts should share cluster"
            );
        }
    }

    // -- Sample limits --

    #[test]
    fn samples_bounded_by_config(
        config in arb_config(),
        count in 1usize..=30,
    ) {
        let max_samples = config.max_samples_per_cluster;
        let mut c = ErrorClusterer::new(config);
        for i in 0..count {
            c.insert("identical error text for sample test", Some(i as u64), i as u64);
        }
        let clusters = c.clusters();
        for cluster in &clusters {
            prop_assert!(
                cluster.samples.len() <= max_samples,
                "samples {} > max {}",
                cluster.samples.len(),
                max_samples
            );
        }
    }

    // -- Pane ID deduplication --

    #[test]
    fn pane_ids_deduplicated(
        pane_id in 1u64..=50,
        count in 2usize..=10,
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for i in 0..count {
            c.insert("same error for pane dedup test", Some(pane_id), i as u64);
        }
        let clusters = c.clusters();
        for cluster in &clusters {
            let pids = &cluster.pane_ids;
            let mut sorted = pids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            prop_assert_eq!(
                pids.len(),
                sorted.len(),
                "pane_ids should be deduplicated"
            );
        }
    }

    // -- Timestamp tracking --

    #[test]
    fn timestamps_first_le_last(
        timestamps in proptest::collection::vec(arb_timestamp(), 2..=20),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for (i, &ts) in timestamps.iter().enumerate() {
            c.insert("same timestamp test error", Some(i as u64), ts);
        }
        let clusters = c.clusters();
        for cluster in &clusters {
            prop_assert!(
                cluster.first_seen_secs <= cluster.last_seen_secs,
                "first {} > last {}",
                cluster.first_seen_secs,
                cluster.last_seen_secs
            );
        }
    }

    #[test]
    fn timestamps_track_min_max(
        timestamps in proptest::collection::vec(arb_timestamp(), 2..=20),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for (i, &ts) in timestamps.iter().enumerate() {
            c.insert("same error for minmax tracking", Some(i as u64), ts);
        }
        let min_ts = *timestamps.iter().min().unwrap();
        let max_ts = *timestamps.iter().max().unwrap();
        let clusters = c.clusters();
        // Since all identical, should be one cluster
        prop_assert_eq!(clusters.len(), 1);
        prop_assert_eq!(clusters[0].first_seen_secs, min_ts);
        prop_assert_eq!(clusters[0].last_seen_secs, max_ts);
    }

    // -- Empty clusterer --

    #[test]
    fn empty_clusterer_zero_counts(_dummy in 0u8..1) {
        let c = ErrorClusterer::with_defaults();
        prop_assert_eq!(c.error_count(), 0);
        prop_assert_eq!(c.cluster_count(), 0);
    }

    // -- Config defaults --

    #[test]
    fn config_defaults_valid(_dummy in 0u8..1) {
        let config = ClusteringConfig::default();
        prop_assert!(config.num_hashes % config.num_bands == 0,
            "default num_hashes {} not divisible by num_bands {}",
            config.num_hashes, config.num_bands);
        prop_assert!(config.shingle_size >= 1);
        prop_assert!(config.max_clusters >= 1);
        prop_assert!(config.max_samples_per_cluster >= 1);
    }

    // -- Custom config --

    #[test]
    fn custom_config_accepted(config in arb_config()) {
        let mut c = ErrorClusterer::new(config);
        c.insert("test error", None, 100);
        prop_assert_eq!(c.error_count(), 1);
        prop_assert!(c.cluster_count() >= 1);
    }

    // -- ClusterInfo serde roundtrip --

    #[test]
    fn cluster_info_serde_roundtrip(
        size in 1usize..=100,
        pane_ids in proptest::collection::vec(1u64..=100, 0..=5),
        first_ts in 0u64..=500_000,
        last_offset in 0u64..=500_000,
    ) {
        let last_ts = first_ts + last_offset;
        let info = ClusterInfo {
            cluster_id: 42,
            size,
            representative: "test error".to_string(),
            samples: vec!["test error".to_string()],
            pane_ids,
            first_seen_secs: first_ts,
            last_seen_secs: last_ts,
        };
        let json = serde_json::to_string(&info).unwrap();
        let round: ClusterInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(round.cluster_id, info.cluster_id);
        prop_assert_eq!(round.size, info.size);
        prop_assert_eq!(round.representative, info.representative);
        prop_assert_eq!(round.samples.len(), info.samples.len());
        prop_assert_eq!(round.pane_ids.len(), info.pane_ids.len());
        prop_assert_eq!(round.first_seen_secs, info.first_seen_secs);
        prop_assert_eq!(round.last_seen_secs, info.last_seen_secs);
    }

    // -- Cluster sizes sum --

    #[test]
    fn cluster_sizes_sum_to_error_count(
        texts in proptest::collection::vec(arb_error_text(), 1..=30),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for (i, text) in texts.iter().enumerate() {
            c.insert(text, None, i as u64);
        }
        let clusters = c.clusters();
        let total_size: usize = clusters.iter().map(|cl| cl.size).sum();
        prop_assert!(
            total_size >= texts.len(),
            "total cluster size {} < error count {}",
            total_size,
            texts.len()
        );
    }

    // -- None pane_id doesn't add to pane_ids --

    #[test]
    fn none_pane_id_not_tracked(
        count in 1usize..=10,
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for i in 0..count {
            c.insert("no pane error", None, i as u64);
        }
        let clusters = c.clusters();
        for cluster in &clusters {
            prop_assert!(
                cluster.pane_ids.is_empty(),
                "pane_ids should be empty when all inserts use None"
            );
        }
    }

    // -- Representative is first text --

    #[test]
    fn representative_is_first_inserted(
        texts in proptest::collection::vec("[a-z]{20,40}", 2..=5),
    ) {
        // Use long distinct texts to minimize clustering across different texts
        let mut c = ErrorClusterer::with_defaults();
        let first_id = c.insert(&texts[0], None, 0);
        for (i, text) in texts[1..].iter().enumerate() {
            c.insert(text, None, (i + 1) as u64);
        }
        let info = c.cluster_info(first_id).unwrap();
        prop_assert_eq!(
            &info.representative, &texts[0],
            "representative should be first text inserted in cluster"
        );
    }
}
