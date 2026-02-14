//! Property tests for mmap scrollback offset/index helpers and store operations.

#[path = "../src/storage/mmap_store.rs"]
mod mmap_store;

use mmap_store::{
    build_offsets_from_lengths, page_align_down, LineOffset, MmapScrollbackStore, MmapStoreConfig,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Section 1: page_align_down pure-function properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Existing: aligned value is always <= input and divisible by page_size.
    #[test]
    fn page_alignment_is_monotonic(offset in any::<u64>(), page in 1u64..65536u64) {
        let aligned = page_align_down(offset, page);
        prop_assert!(aligned <= offset);
        prop_assert_eq!(aligned % page, 0);
    }

    /// Aligning an already-aligned value is idempotent.
    #[test]
    fn page_align_down_idempotence(offset in any::<u64>(), page in 1u64..65536u64) {
        let first = page_align_down(offset, page);
        let second = page_align_down(first, page);
        prop_assert_eq!(
            first, second,
            "aligning twice should be idempotent: first={}, second={}", first, second
        );
    }

    /// page_align_down with zero page_size returns the offset unchanged.
    #[test]
    fn page_align_down_zero_page_size(offset in any::<u64>()) {
        let result = page_align_down(offset, 0);
        prop_assert_eq!(
            result, offset,
            "zero page_size should return offset unchanged: got {}, expected {}", result, offset
        );
    }

    /// The aligned result is always <= the input offset.
    #[test]
    fn page_align_down_never_exceeds_input(offset in any::<u64>(), page in 1u64..1_000_000u64) {
        let aligned = page_align_down(offset, page);
        prop_assert!(
            aligned <= offset,
            "aligned {} should be <= offset {}", aligned, offset
        );
    }

    /// For power-of-2 page sizes, page_align_down matches bitwise masking.
    #[test]
    fn page_align_down_power_of_two(offset in any::<u64>(), exp in 0u32..16u32) {
        let page = 1u64 << exp;
        let aligned = page_align_down(offset, page);
        let mask = !(page - 1);
        let expected = offset & mask;
        prop_assert_eq!(
            aligned, expected,
            "power-of-2 alignment: aligned={}, expected={}, page={}", aligned, expected, page
        );
    }

    /// The distance from the aligned value to the original is always < page_size.
    #[test]
    fn page_align_down_distance_less_than_page(offset in any::<u64>(), page in 1u64..65536u64) {
        let aligned = page_align_down(offset, page);
        let distance = offset - aligned;
        prop_assert!(
            distance < page,
            "distance {} should be < page_size {}", distance, page
        );
    }
}

// ---------------------------------------------------------------------------
// Section 2: build_offsets_from_lengths pure-function properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Existing: offsets are monotonically non-decreasing.
    #[test]
    fn offsets_are_monotonic(lengths in prop::collection::vec(0u64..4096u64, 0..512)) {
        let offsets = build_offsets_from_lengths(&lengths);
        for pair in offsets.windows(2) {
            prop_assert!(pair[0] <= pair[1]);
        }
    }

    /// Existing: each offset matches the prefix sum of lengths.
    #[test]
    fn offsets_match_prefix_sum(lengths in prop::collection::vec(0u64..4096u64, 0..512)) {
        let offsets = build_offsets_from_lengths(&lengths);
        let mut cursor = 0u64;
        for (i, off) in offsets.iter().enumerate() {
            prop_assert_eq!(*off, LineOffset(cursor));
            cursor = cursor.saturating_add(lengths[i]);
        }
    }

    /// Empty input yields empty output.
    #[test]
    fn build_offsets_empty_input(_dummy in Just(())) {
        let offsets = build_offsets_from_lengths(&[]);
        prop_assert!(
            offsets.is_empty(),
            "empty input should produce empty output, got len={}", offsets.len()
        );
    }

    /// First offset is always LineOffset(0) for non-empty input.
    #[test]
    fn build_offsets_first_is_zero(lengths in prop::collection::vec(0u64..4096u64, 1..512)) {
        let offsets = build_offsets_from_lengths(&lengths);
        prop_assert_eq!(
            offsets[0], LineOffset(0),
            "first offset should be 0, got {:?}", offsets[0]
        );
    }

    /// Output length always equals input length.
    #[test]
    fn build_offsets_length_matches_input(lengths in prop::collection::vec(0u64..4096u64, 0..512)) {
        let offsets = build_offsets_from_lengths(&lengths);
        prop_assert_eq!(
            offsets.len(), lengths.len(),
            "output len {} should match input len {}", offsets.len(), lengths.len()
        );
    }

    /// Differences between consecutive offsets match the corresponding input lengths.
    #[test]
    fn build_offsets_consecutive_differences(
        lengths in prop::collection::vec(0u64..4096u64, 2..256)
    ) {
        let offsets = build_offsets_from_lengths(&lengths);
        for i in 0..offsets.len() - 1 {
            let diff = offsets[i + 1].0 - offsets[i].0;
            prop_assert_eq!(
                diff, lengths[i],
                "diff at index {}: got {}, expected {}", i, diff, lengths[i]
            );
        }
    }

    /// Last offset + last length = total sum of all lengths.
    #[test]
    fn build_offsets_last_plus_length_is_total(
        lengths in prop::collection::vec(1u64..1024u64, 1..256)
    ) {
        let offsets = build_offsets_from_lengths(&lengths);
        let last_offset = offsets.last().unwrap().0;
        let last_length = *lengths.last().unwrap();
        let total: u64 = lengths.iter().sum();
        prop_assert_eq!(
            last_offset + last_length, total,
            "last_offset({}) + last_length({}) should equal total({})",
            last_offset, last_length, total
        );
    }
}

// ---------------------------------------------------------------------------
// Section 3: MmapScrollbackStore I/O properties (lower case count)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// line_count starts at 0 for a pane that has not been written to.
    #[test]
    fn store_line_count_starts_at_zero(pane_id in 1u64..1000u64) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let store = MmapScrollbackStore::new(config).unwrap();
        let count = store.line_count(pane_id);
        prop_assert_eq!(
            count, 0,
            "new pane {} should have line_count 0, got {}", pane_id, count
        );
    }

    /// line_count increases by exactly 1 for each append.
    #[test]
    fn store_line_count_increments(
        lines in prop::collection::vec("[a-zA-Z0-9 ]{1,80}", 1..20)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).unwrap();
        let pane_id = 1u64;

        for (i, line) in lines.iter().enumerate() {
            store.append_line(pane_id, line).unwrap();
            let count = store.line_count(pane_id);
            prop_assert_eq!(
                count, i + 1,
                "after {} appends, line_count should be {}, got {}", i + 1, i + 1, count
            );
        }
    }

    /// tail_lines(0) always returns an empty vec.
    #[test]
    fn store_tail_lines_zero_returns_empty(
        lines in prop::collection::vec("[a-zA-Z0-9]{1,40}", 1..10)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).unwrap();
        let pane_id = 1u64;

        for line in &lines {
            store.append_line(pane_id, line).unwrap();
        }

        let result = store.tail_lines(pane_id, 0).unwrap();
        prop_assert!(
            result.is_empty(),
            "tail_lines(0) should return empty, got {} lines", result.len()
        );
    }

    /// tail_lines(n) where n >= line_count returns all lines.
    #[test]
    fn store_tail_lines_large_n_returns_all(
        lines in prop::collection::vec("[a-zA-Z0-9]{1,40}", 1..15)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).unwrap();
        let pane_id = 1u64;

        for line in &lines {
            store.append_line(pane_id, line).unwrap();
        }

        let n = lines.len() + 10; // request more than available
        let result = store.tail_lines(pane_id, n).unwrap();
        prop_assert_eq!(
            result.len(), lines.len(),
            "tail_lines({}) should return all {} lines, got {}",
            n, lines.len(), result.len()
        );
        for (i, (got, expected)) in result.iter().zip(lines.iter()).enumerate() {
            prop_assert_eq!(
                got, expected,
                "line {} mismatch: got '{}', expected '{}'", i, got, expected
            );
        }
    }

    /// Appending lines then reading them back via tail_lines recovers the content.
    #[test]
    fn store_append_then_tail_recovers_content(
        lines in prop::collection::vec("[a-zA-Z0-9 ]{1,60}", 1..20)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).unwrap();
        let pane_id = 42u64;

        for line in &lines {
            store.append_line(pane_id, line).unwrap();
        }

        let result = store.tail_lines(pane_id, lines.len()).unwrap();
        prop_assert_eq!(
            result.len(), lines.len(),
            "expected {} lines, got {}", lines.len(), result.len()
        );
        for (i, (got, expected)) in result.iter().zip(lines.iter()).enumerate() {
            prop_assert_eq!(
                got, expected,
                "content mismatch at line {}: got '{}', expected '{}'", i, got, expected
            );
        }
    }

    /// Multi-pane isolation: writes to one pane do not affect another.
    #[test]
    fn store_multi_pane_isolation(
        lines_a in prop::collection::vec("[a-z]{1,20}", 1..10),
        lines_b in prop::collection::vec("[A-Z]{1,20}", 1..10),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).unwrap();
        let pane_a = 100u64;
        let pane_b = 200u64;

        for line in &lines_a {
            store.append_line(pane_a, line).unwrap();
        }
        for line in &lines_b {
            store.append_line(pane_b, line).unwrap();
        }

        // Verify counts are independent
        prop_assert_eq!(
            store.line_count(pane_a), lines_a.len(),
            "pane_a count: expected {}, got {}", lines_a.len(), store.line_count(pane_a)
        );
        prop_assert_eq!(
            store.line_count(pane_b), lines_b.len(),
            "pane_b count: expected {}, got {}", lines_b.len(), store.line_count(pane_b)
        );

        // Verify content is isolated
        let result_a = store.tail_lines(pane_a, lines_a.len()).unwrap();
        let result_b = store.tail_lines(pane_b, lines_b.len()).unwrap();

        for (i, (got, expected)) in result_a.iter().zip(lines_a.iter()).enumerate() {
            prop_assert_eq!(
                got, expected,
                "pane_a line {} mismatch: got '{}', expected '{}'", i, got, expected
            );
        }
        for (i, (got, expected)) in result_b.iter().zip(lines_b.iter()).enumerate() {
            prop_assert_eq!(
                got, expected,
                "pane_b line {} mismatch: got '{}', expected '{}'", i, got, expected
            );
        }
    }

    /// tail_lines respects the requested count, returning exactly min(n, total).
    #[test]
    fn store_tail_lines_respects_count(
        lines in prop::collection::vec("[a-zA-Z0-9]{1,30}", 3..20),
        requested in 1usize..30usize,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MmapStoreConfig::new(dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).unwrap();
        let pane_id = 7u64;

        for line in &lines {
            store.append_line(pane_id, line).unwrap();
        }

        let result = store.tail_lines(pane_id, requested).unwrap();
        let expected_len = requested.min(lines.len());
        prop_assert_eq!(
            result.len(), expected_len,
            "tail_lines({}) with {} total should return {}, got {}",
            requested, lines.len(), expected_len, result.len()
        );

        // Verify the returned lines are the last `expected_len` lines
        let start = lines.len() - expected_len;
        for (i, (got, expected)) in result.iter().zip(lines[start..].iter()).enumerate() {
            prop_assert_eq!(
                got, expected,
                "tail line {} mismatch: got '{}', expected '{}'", i, got, expected
            );
        }
    }
}
