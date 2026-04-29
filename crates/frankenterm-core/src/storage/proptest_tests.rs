//! ft-dzats Phase 1a: extracted from storage.rs (mod proptest_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to `crate::storage::*`.
//!
//! [ft-iq339 / ft-3tvvt] Each proptest case routes through
//! `super::run_storage_proptest_async`, which centralizes the
//! `RuntimeBuilder::multi_thread().build().expect(...).block_on(...)`
//! boilerplate behind the same panic-catching + runtime-drop-absorbing
//! envelope used by `run_storage_async_test`. The helper returns the
//! async block's value so each proptest can run its `prop_assert_*`
//! calls *outside* the async scope.

use super::*;
use proptest::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

// Counter for unique temp DB paths
static PROPTEST_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique temp DB path
fn temp_db_path() -> String {
    let counter = PROPTEST_DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!("wa_proptest_{counter}_{}.db", std::process::id()))
        .to_str()
        .unwrap()
        .to_string()
}

/// Helper to create a test pane record
fn test_pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

// Strategy for generating valid segment content (non-empty ASCII strings)
fn segment_content_strategy() -> impl Strategy<Value = String> {
    // Generate strings of 1-100 printable ASCII characters
    "[a-zA-Z0-9 .,!?]{1,100}"
}

// Strategy for generating write operations (pane_id, content)
fn write_ops_strategy() -> impl Strategy<Value = Vec<(u64, String)>> {
    // Generate 1-50 write operations across 1-5 panes
    let pane_count = 1u64..=5;
    pane_count.prop_flat_map(|max_panes| {
        proptest::collection::vec((1..=max_panes, segment_content_strategy()), 1..50)
    })
}

proptest! {
    // Set configuration for deterministic, CI-friendly runs
    #![proptest_config(ProptestConfig {
        cases: 50,  // Bounded case count
        max_shrink_iters: 100,
        .. ProptestConfig::default()
    })]

    /// Property: Sequence numbers are monotonically increasing per pane
    ///
    /// For any sequence of write operations across multiple panes,
    /// each pane's segments must have strictly increasing seq numbers.
    #[test]
    fn prop_seq_monotonic_per_pane(writes in write_ops_strategy()) {
        let db_path = temp_db_path();

        // Collect results from async block for verification
        let verification_results: Vec<(u64, Vec<u64>)> =
            super::run_storage_proptest_async(async {
                let handle = StorageHandle::new(&db_path).await.expect("create storage");

                // Determine which panes we need to create
                let pane_ids: std::collections::HashSet<u64> = writes.iter().map(|(p, _)| *p).collect();

                // Create all needed panes
                for &pane_id in &pane_ids {
                    handle.upsert_pane(test_pane(pane_id)).await.expect("create pane");
                }

                // Execute all writes
                for (pane_id, content) in &writes {
                    handle.append_segment(*pane_id, content, None).await.expect("append segment");
                }

                // Collect seq values for each pane
                let mut results = Vec::new();
                for &pane_id in &pane_ids {
                    let segments = handle.get_segments(pane_id, 1000).await.expect("get segments");
                    // Segments are returned in descending seq order, reverse for ascending
                    let seqs: Vec<u64> = segments.iter().rev().map(|s| s.seq).collect();
                    results.push((pane_id, seqs));
                }

                handle.shutdown().await.expect("shutdown");
                results
            });

        let _ = std::fs::remove_file(&db_path);

        // Verify monotonicity outside async block
        for (pane_id, seqs) in verification_results {
            for (expected, actual) in seqs.iter().enumerate() {
                prop_assert_eq!(
                    *actual, expected as u64,
                    "Pane {} seq at index {} should be {} but got {}",
                    pane_id, expected, expected, actual
                );
            }
        }
    }

    /// Property: Inserted text becomes searchable via FTS
    ///
    /// For any valid search term inserted as segment content,
    /// FTS search should find it.
    #[test]
    fn prop_fts_finds_inserted_text(content in "[a-zA-Z]{3,20}") {
        let db_path = temp_db_path();

        // Collect search results from async block
        let (results_empty, found_content): (bool, bool) =
            super::run_storage_proptest_async(async {
                let handle = StorageHandle::new(&db_path).await.expect("create storage");
                handle.upsert_pane(test_pane(1)).await.expect("create pane");

                // Insert the content as a segment
                handle.append_segment(1, &content, None).await.expect("append segment");

                // Search for the content
                let results = handle.search(&content).await.expect("search");

                let is_empty = results.is_empty();
                let found = results.iter().any(|seg| seg.content.contains(&content));

                handle.shutdown().await.expect("shutdown");
                (is_empty, found)
            });

        let _ = std::fs::remove_file(&db_path);

        // Verify outside async block
        prop_assert!(
            !results_empty,
            "FTS search for '{}' should return results",
            content
        );
        prop_assert!(
            found_content,
            "At least one result should contain '{}'",
            content
        );
    }

    /// Property: FTS respects pane scoping
    ///
    /// Content inserted in one pane should not appear in searches
    /// scoped to a different pane.
    #[test]
    fn prop_fts_respects_pane_scope(
        (content1, content2) in ("[a-zA-Z]{5,15}", "[a-zA-Z]{5,15}")
            .prop_filter("contents must differ", |(a, b)| a != b)
    ) {
        let db_path = temp_db_path();

        // Collect search results from async block
        let (found_in_pane1, found_in_pane2): (bool, bool) =
            super::run_storage_proptest_async(async {
                let handle = StorageHandle::new(&db_path).await.expect("create storage");

                // Create two panes
                handle.upsert_pane(test_pane(1)).await.expect("create pane 1");
                handle.upsert_pane(test_pane(2)).await.expect("create pane 2");

                // Insert different content in each pane
                handle.append_segment(1, &content1, None).await.expect("append to pane 1");
                handle.append_segment(2, &content2, None).await.expect("append to pane 2");

                // Search for content1 scoped to pane 1
                let opts1 = SearchOptions {
                    pane_id: Some(1),
                    ..Default::default()
                };
                let results1 = handle.search_with_options(&content1, opts1).await.expect("search pane 1");

                // Search for content1 scoped to pane 2
                let opts2 = SearchOptions {
                    pane_id: Some(2),
                    ..Default::default()
                };
                let results2 = handle.search_with_options(&content1, opts2).await.expect("search pane 2");

                handle.shutdown().await.expect("shutdown");
                (!results1.is_empty(), !results2.is_empty())
            });

        let _ = std::fs::remove_file(&db_path);

        // Verify outside async block
        prop_assert!(
            found_in_pane1,
            "Should find '{}' in pane 1",
            content1
        );
        prop_assert!(
            !found_in_pane2,
            "Should NOT find '{}' in pane 2",
            content1
        );
    }
}
