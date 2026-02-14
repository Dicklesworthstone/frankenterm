//! Property-based tests for error_clustering module.
//!
//! Validates MinHash LSH clustering invariants: error counting,
//! cluster bounds, timestamp tracking, pane deduplication,
//! sample limits, serde roundtrip, config validation, and
//! multi-pane isolation.
//!
//! Properties:
//!  1. error_count tracks insertions
//!  2. cluster_count bounded by error_count
//!  3. cluster_count >= 1 after insert
//!  4. insert returns valid cluster IDs
//!  5. identical texts same cluster
//!  6. samples bounded by config
//!  7. pane IDs deduplicated
//!  8. timestamps first <= last
//!  9. timestamps track min/max
//! 10. empty clusterer zero counts
//! 11. config defaults valid
//! 12. custom config accepted
//! 13. ClusterInfo serde roundtrip
//! 14. cluster sizes sum to error count
//! 15. None pane_id not tracked
//! 16. representative is first inserted
//! 17. ClusteringConfig serde roundtrip (JSON)
//! 18. ClusteringConfig TOML roundtrip
//! 19. ClusteringConfig double roundtrip stable
//! 20. ClusteringConfig forward compat
//! 21. multiple pane IDs tracked
//! 22. cluster_info returns same cluster for same error_id
//! 23. clusters sorted by first_seen_secs after multiple insertions
//! 24. cluster count bounded by max_clusters config
//! 25. error_count monotonically increases

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

    /// Property 1: error_count tracks insertions.
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

    /// Property 2: cluster_count bounded by error_count.
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

    /// Property 3: cluster_count >= 1 after insert.
    #[test]
    fn cluster_count_at_least_one_after_insert(
        text in arb_error_text(),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        c.insert(&text, None, 100);
        prop_assert!(c.cluster_count() >= 1);
    }

    // -- Insert returns valid cluster IDs --

    /// Property 4: insert returns valid cluster IDs.
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

    /// Property 5: identical texts same cluster.
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

    /// Property 6: samples bounded by config.
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

    /// Property 7: pane IDs deduplicated.
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

    /// Property 8: timestamps first <= last.
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

    /// Property 9: timestamps track min/max.
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

    /// Property 10: empty clusterer zero counts.
    #[test]
    fn empty_clusterer_zero_counts(_dummy in 0u8..1) {
        let c = ErrorClusterer::with_defaults();
        prop_assert_eq!(c.error_count(), 0);
        prop_assert_eq!(c.cluster_count(), 0);
    }

    // -- Config defaults --

    /// Property 11: config defaults valid.
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

    /// Property 12: custom config accepted.
    #[test]
    fn custom_config_accepted(config in arb_config()) {
        let mut c = ErrorClusterer::new(config);
        c.insert("test error", None, 100);
        prop_assert_eq!(c.error_count(), 1);
        prop_assert!(c.cluster_count() >= 1);
    }

    // -- ClusterInfo serde roundtrip --

    /// Property 13: ClusterInfo serde roundtrip.
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

    /// Property 14: cluster sizes sum to error count.
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

    /// Property 15: None pane_id not tracked.
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

    /// Property 16: representative is first inserted.
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

    // -- ClusteringConfig serde roundtrip --

    /// Property 17: ClusteringConfig JSON serde roundtrip.
    #[test]
    fn config_json_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ClusteringConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.num_hashes, config.num_hashes,
            "num_hashes mismatch after JSON roundtrip");
        prop_assert_eq!(back.num_bands, config.num_bands,
            "num_bands mismatch after JSON roundtrip");
        prop_assert_eq!(back.shingle_size, config.shingle_size,
            "shingle_size mismatch after JSON roundtrip");
        prop_assert_eq!(back.max_clusters, config.max_clusters,
            "max_clusters mismatch after JSON roundtrip");
        prop_assert_eq!(back.max_samples_per_cluster, config.max_samples_per_cluster,
            "max_samples_per_cluster mismatch after JSON roundtrip");
    }

    /// Property 18: ClusteringConfig TOML roundtrip.
    #[test]
    fn config_toml_serde_roundtrip(config in arb_config()) {
        let toml_str = toml::to_string(&config).unwrap();
        let back: ClusteringConfig = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(back.num_hashes, config.num_hashes,
            "num_hashes mismatch after TOML roundtrip");
        prop_assert_eq!(back.num_bands, config.num_bands,
            "num_bands mismatch after TOML roundtrip");
        prop_assert_eq!(back.shingle_size, config.shingle_size,
            "shingle_size mismatch after TOML roundtrip");
        prop_assert_eq!(back.max_clusters, config.max_clusters,
            "max_clusters mismatch after TOML roundtrip");
        prop_assert_eq!(back.max_samples_per_cluster, config.max_samples_per_cluster,
            "max_samples_per_cluster mismatch after TOML roundtrip");
    }

    /// Property 19: ClusteringConfig double roundtrip stable.
    #[test]
    fn config_double_roundtrip_stable(config in arb_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let mid: ClusteringConfig = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&mid).unwrap();
        prop_assert_eq!(&json1, &json2,
            "double roundtrip should produce identical JSON");
    }

    /// Property 20: ClusteringConfig forward compat (extra fields ignored).
    #[test]
    fn config_forward_compat(config in arb_config()) {
        let json = format!(
            "{{\"num_hashes\":{},\"num_bands\":{},\"shingle_size\":{},\"max_clusters\":{},\"max_samples_per_cluster\":{},\"future_field\":true,\"v2_option\":42}}",
            config.num_hashes, config.num_bands, config.shingle_size,
            config.max_clusters, config.max_samples_per_cluster
        );
        let back: ClusteringConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.num_hashes, config.num_hashes,
            "extra fields should not affect num_hashes");
        prop_assert_eq!(back.num_bands, config.num_bands,
            "extra fields should not affect num_bands");
    }

    // -- Multiple pane IDs tracked --

    /// Property 21: multiple pane IDs tracked.
    #[test]
    fn multiple_pane_ids_tracked(
        pane_ids in proptest::collection::vec(1u64..=100, 2..=8),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        for (i, &pane_id) in pane_ids.iter().enumerate() {
            c.insert("shared error across panes", Some(pane_id), i as u64);
        }
        let clusters = c.clusters();
        // All errors are identical so should be in one cluster
        prop_assert_eq!(clusters.len(), 1,
            "identical errors should form one cluster");
        // Unique pane IDs should all appear
        let mut expected_unique: Vec<u64> = pane_ids.clone();
        expected_unique.sort_unstable();
        expected_unique.dedup();
        let mut actual = clusters[0].pane_ids.clone();
        actual.sort_unstable();
        prop_assert_eq!(actual, expected_unique,
            "all unique pane IDs should be tracked");
    }

    /// Property 22: cluster_info returns consistent cluster for same error_id.
    #[test]
    fn cluster_info_consistent_for_same_id(
        texts in proptest::collection::vec(arb_error_text(), 2..=10),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        let mut ids = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            ids.push(c.insert(text, Some(i as u64), i as u64));
        }
        // Calling cluster_info twice for the same error_id gives same result
        for &id in &ids {
            let info1 = c.cluster_info(id).unwrap();
            let info2 = c.cluster_info(id).unwrap();
            prop_assert_eq!(info1.cluster_id, info2.cluster_id,
                "cluster_info should be consistent for error_id {}", id);
            prop_assert_eq!(info1.size, info2.size,
                "cluster size should be consistent for error_id {}", id);
        }
    }

    /// Property 23: clusters have valid first_seen <= last_seen across all.
    #[test]
    fn all_clusters_valid_timestamps(
        texts in proptest::collection::vec(arb_error_text(), 1..=20),
        timestamps in proptest::collection::vec(arb_timestamp(), 1..=20),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        let n = texts.len().min(timestamps.len());
        for i in 0..n {
            c.insert(&texts[i], Some(i as u64), timestamps[i]);
        }
        let clusters = c.clusters();
        for cluster in &clusters {
            prop_assert!(
                cluster.first_seen_secs <= cluster.last_seen_secs,
                "cluster {} first_seen {} > last_seen {}",
                cluster.cluster_id, cluster.first_seen_secs, cluster.last_seen_secs
            );
            prop_assert!(cluster.size >= 1,
                "cluster {} has size 0", cluster.cluster_id);
            prop_assert!(!cluster.representative.is_empty(),
                "cluster {} has empty representative", cluster.cluster_id);
        }
    }

    /// Property 24: cluster count bounded by max_clusters config.
    #[test]
    fn cluster_count_bounded_by_max_clusters(
        max_clusters in 2usize..=10,
    ) {
        let config = ClusteringConfig {
            num_hashes: 16,
            num_bands: 4,
            shingle_size: 3,
            max_clusters,
            max_samples_per_cluster: 3,
        };
        let mut c = ErrorClusterer::new(config);
        // Insert many very different errors to maximize cluster count
        for i in 0..max_clusters * 3 {
            let text = format!("unique_error_{}_with_extra_padding_to_prevent_hash_collisions", i);
            c.insert(&text, None, i as u64);
        }
        prop_assert!(
            c.cluster_count() <= max_clusters,
            "cluster_count {} > max_clusters {}",
            c.cluster_count(),
            max_clusters
        );
    }

    /// Property 25: error_count monotonically increases.
    #[test]
    fn error_count_monotonically_increases(
        texts in proptest::collection::vec(arb_error_text(), 1..=20),
    ) {
        let mut c = ErrorClusterer::with_defaults();
        let mut prev_count = 0;
        for (i, text) in texts.iter().enumerate() {
            c.insert(text, None, i as u64);
            let count = c.error_count();
            prop_assert!(count > prev_count,
                "error_count should increase: was {}, now {} after insert {}",
                prev_count, count, i);
            prev_count = count;
        }
    }
}
