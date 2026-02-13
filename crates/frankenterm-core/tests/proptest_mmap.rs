//! Property tests for mmap scrollback offset/index helpers.

#[path = "../src/storage/mmap_store.rs"]
mod mmap_store;

use mmap_store::{LineOffset, build_offsets_from_lengths, page_align_down};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn page_alignment_is_monotonic(offset in any::<u64>(), page in 1u64..65536u64) {
        let aligned = page_align_down(offset, page);
        prop_assert!(aligned <= offset);
        prop_assert_eq!(aligned % page, 0);
    }

    #[test]
    fn offsets_are_monotonic(lengths in prop::collection::vec(0u64..4096u64, 0..512)) {
        let offsets = build_offsets_from_lengths(&lengths);
        for pair in offsets.windows(2) {
            prop_assert!(pair[0] <= pair[1]);
        }
    }

    #[test]
    fn offsets_match_prefix_sum(lengths in prop::collection::vec(0u64..4096u64, 0..512)) {
        let offsets = build_offsets_from_lengths(&lengths);
        let mut cursor = 0u64;
        for (i, off) in offsets.iter().enumerate() {
            prop_assert_eq!(*off, LineOffset(cursor));
            cursor = cursor.saturating_add(lengths[i]);
        }
    }
}
