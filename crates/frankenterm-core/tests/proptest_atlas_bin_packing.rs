use proptest::prelude::*;

use frankenterm_core::atlas_bin_packing::{
    AllocationOutcome, Atlas2DSize, BinPacker, GlyphSize, MaximalRectanglesPacker, PackedRect,
    PackerKind, PackerSelectionThresholds, PackingStats, RejectReason, ShelfPacker, SkylinePacker,
    first_overlap, make_packer, non_overlapping, select_packer,
};

fn arb_atlas_size() -> impl Strategy<Value = Atlas2DSize> {
    (1u32..=128, 1u32..=128).prop_map(|(width, height)| Atlas2DSize { width, height })
}

fn arb_glyph_size() -> impl Strategy<Value = GlyphSize> {
    (1u32..=160, 1u32..=160).prop_map(|(width, height)| GlyphSize { width, height })
}

fn arb_glyph_sequence() -> impl Strategy<Value = Vec<GlyphSize>> {
    prop::collection::vec(arb_glyph_size(), 0..96)
}

fn rect_in_bounds(rect: PackedRect, atlas: Atlas2DSize) -> bool {
    rect.right() <= atlas.width as u64 && rect.bottom() <= atlas.height as u64
}

fn assert_outcome_shape(outcome: AllocationOutcome) -> Result<(), TestCaseError> {
    match outcome {
        AllocationOutcome::Placed(rect) => {
            prop_assert_eq!(outcome.placed(), Some(rect));
            prop_assert_eq!(outcome.reject_reason(), None);
        }
        AllocationOutcome::Rejected(reason) => {
            prop_assert_eq!(outcome.placed(), None);
            prop_assert_eq!(outcome.reject_reason(), Some(reason));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_atlas_bin_packing_size_constructors_reject_zero_and_area_is_product(
        width in 0u32..=4096,
        height in 0u32..=4096,
    ) {
        let atlas = Atlas2DSize::try_new(width, height);
        let glyph = GlyphSize::try_new(width, height);

        prop_assert_eq!(atlas.is_some(), width != 0 && height != 0);
        prop_assert_eq!(glyph.is_some(), width != 0 && height != 0);
        if let Some(size) = atlas {
            prop_assert_eq!(size.area(), u64::from(width) * u64::from(height));
        }
        if let Some(size) = glyph {
            prop_assert_eq!(size.area(), u64::from(width) * u64::from(height));
        }
    }

    #[test]
    fn proptest_atlas_bin_packing_rect_overlap_is_symmetric_and_edge_touching_is_clear(
        x in 0u32..=256,
        y in 0u32..=256,
        width in 1u32..=64,
        height in 1u32..=64,
        dx in 0u32..=128,
        dy in 0u32..=128,
    ) {
        let a = PackedRect { x, y, width, height };
        let b = PackedRect {
            x: x.saturating_add(dx),
            y: y.saturating_add(dy),
            width,
            height,
        };

        prop_assert_eq!(a.overlaps(&b), b.overlaps(&a));
        prop_assert_eq!(a.area(), u64::from(width) * u64::from(height));

        let touching_right = PackedRect {
            x: x.saturating_add(width),
            y,
            width,
            height,
        };
        let touching_bottom = PackedRect {
            x,
            y: y.saturating_add(height),
            width,
            height,
        };
        prop_assert!(!a.overlaps(&touching_right));
        prop_assert!(!a.overlaps(&touching_bottom));
    }

    #[test]
    fn proptest_atlas_bin_packing_selector_matches_threshold_policy(
        width in 1u32..=8192,
        height in 1u32..=8192,
        shelf_max_axis in 0u32..=2048,
        maximal_rectangles_min_axis in 0u32..=8192,
    ) {
        let size = Atlas2DSize { width, height };
        let thresholds = PackerSelectionThresholds {
            shelf_max_axis,
            maximal_rectangles_min_axis,
        };
        let selected = select_packer(size, thresholds);
        let expected = if width <= shelf_max_axis && height <= shelf_max_axis {
            PackerKind::Shelf
        } else if width.max(height) >= maximal_rectangles_min_axis {
            PackerKind::MaximalRectangles
        } else {
            PackerKind::Skyline
        };

        prop_assert_eq!(selected, expected);
    }

    #[test]
    fn proptest_atlas_bin_packing_shelf_allocations_are_in_bounds_and_non_overlapping(
        atlas in arb_atlas_size(),
        glyphs in arb_glyph_sequence(),
    ) {
        let mut packer = ShelfPacker::new(atlas);

        for glyph in glyphs {
            let outcome = packer.try_alloc(glyph);
            assert_outcome_shape(outcome)?;
            match outcome {
                AllocationOutcome::Placed(rect) => {
                    prop_assert_eq!(rect.width, glyph.width);
                    prop_assert_eq!(rect.height, glyph.height);
                    prop_assert!(rect_in_bounds(rect, atlas));
                }
                AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas) => {
                    prop_assert!(glyph.width > atlas.width);
                }
                AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas) => {
                    prop_assert!(glyph.height > atlas.height);
                }
                AllocationOutcome::Rejected(RejectReason::AtlasFull) => {
                    prop_assert!(glyph.width <= atlas.width);
                    prop_assert!(glyph.height <= atlas.height);
                }
                AllocationOutcome::Rejected(RejectReason::MaximalRectanglesNotImplemented) => {
                    prop_assert!(false, "shelf packer must not emit maximal-rectangles placeholder");
                }
            }
            prop_assert!(non_overlapping(packer.placements()));
            prop_assert_eq!(first_overlap(packer.placements()), None);
        }

        prop_assert_eq!(packer.size(), atlas);
    }

    #[test]
    fn proptest_atlas_bin_packing_skyline_allocations_are_in_bounds_and_non_overlapping(
        atlas in arb_atlas_size(),
        glyphs in arb_glyph_sequence(),
    ) {
        let mut packer = SkylinePacker::new(atlas);

        for glyph in glyphs {
            let outcome = packer.try_alloc(glyph);
            assert_outcome_shape(outcome)?;
            match outcome {
                AllocationOutcome::Placed(rect) => {
                    prop_assert_eq!(rect.width, glyph.width);
                    prop_assert_eq!(rect.height, glyph.height);
                    prop_assert!(rect_in_bounds(rect, atlas));
                }
                AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas) => {
                    prop_assert!(glyph.width > atlas.width);
                }
                AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas) => {
                    prop_assert!(glyph.height > atlas.height);
                }
                AllocationOutcome::Rejected(RejectReason::AtlasFull) => {
                    prop_assert!(glyph.width <= atlas.width);
                    prop_assert!(glyph.height <= atlas.height);
                }
                AllocationOutcome::Rejected(RejectReason::MaximalRectanglesNotImplemented) => {
                    prop_assert!(false, "skyline packer must not emit maximal-rectangles placeholder");
                }
            }
            prop_assert!(non_overlapping(packer.placements()));
            prop_assert_eq!(first_overlap(packer.placements()), None);
        }

        prop_assert_eq!(packer.size(), atlas);
    }

    #[test]
    fn proptest_atlas_bin_packing_stats_invariants(
        atlas in arb_atlas_size(),
        rects in prop::collection::vec((0u32..=32, 0u32..=32, 1u32..=32, 1u32..=32), 0..64),
        reject_count in 0usize..64,
    ) {
        let mut stats = PackingStats::default();
        stats.record_atlas_size(atlas);
        let mut used = 0u64;
        for (x, y, width, height) in rects {
            let rect = PackedRect { x, y, width, height };
            stats.record_placed(rect);
            used = used.saturating_add(rect.area());
        }
        for _ in 0..reject_count {
            stats.record_reject();
        }

        prop_assert_eq!(stats.used_bytes, used);
        prop_assert_eq!(stats.alloc_total + stats.reject_total, stats.alloc_total + reject_count as u64);
        prop_assert!(stats.efficiency_pct() <= 100);
        prop_assert_eq!(stats.wasted_pct(), 100 - stats.efficiency_pct());
    }

    #[test]
    fn proptest_atlas_bin_packing_maximal_rectangles_allocations_are_in_bounds_and_non_overlapping(
        atlas in arb_atlas_size(),
        glyphs in arb_glyph_sequence(),
    ) {
        let mut packer = MaximalRectanglesPacker::new(atlas);

        for glyph in glyphs {
            let outcome = packer.try_alloc(glyph);
            assert_outcome_shape(outcome)?;
            match outcome {
                AllocationOutcome::Placed(rect) => {
                    prop_assert_eq!(rect.width, glyph.width);
                    prop_assert_eq!(rect.height, glyph.height);
                    prop_assert!(rect_in_bounds(rect, atlas));
                }
                AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas) => {
                    prop_assert!(glyph.width > atlas.width);
                }
                AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas) => {
                    prop_assert!(glyph.height > atlas.height);
                }
                AllocationOutcome::Rejected(RejectReason::AtlasFull) => {
                    prop_assert!(glyph.width <= atlas.width);
                    prop_assert!(glyph.height <= atlas.height);
                }
                AllocationOutcome::Rejected(RejectReason::MaximalRectanglesNotImplemented) => {
                    prop_assert!(
                        false,
                        "maximal-rectangles packer must not emit the retired placeholder reason"
                    );
                }
            }
            prop_assert!(non_overlapping(packer.placements()));
            prop_assert_eq!(first_overlap(packer.placements()), None);
        }

        prop_assert_eq!(packer.size(), atlas);
        prop_assert_eq!(BinPacker::kind(&packer), PackerKind::MaximalRectangles);
    }

    #[test]
    fn proptest_atlas_bin_packing_make_packer_round_trip_preserves_kind_and_size(
        size in arb_atlas_size(),
        kind in prop::sample::select(vec![
            PackerKind::Shelf,
            PackerKind::Skyline,
            PackerKind::MaximalRectangles,
        ]),
    ) {
        let packer = make_packer(kind, size);
        prop_assert_eq!(packer.kind(), kind);
        prop_assert_eq!(packer.size(), size);
    }

    #[test]
    fn proptest_atlas_bin_packing_make_packer_dispatch_preserves_non_overlap(
        size in arb_atlas_size(),
        kind in prop::sample::select(vec![
            PackerKind::Shelf,
            PackerKind::Skyline,
            PackerKind::MaximalRectangles,
        ]),
        glyphs in arb_glyph_sequence(),
    ) {
        let mut packer = make_packer(kind, size);
        for glyph in glyphs {
            let _ = packer.try_alloc(glyph);
            prop_assert!(non_overlapping(packer.placements()));
        }
    }
}
