#[path = "../examples/atlas_packing_attestation.rs"]
mod atlas_packing_attestation;

use frankenterm_core::atlas_bin_packing::{GlyphSize, PackerKind};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn arb_packer_kind() -> impl Strategy<Value = PackerKind> {
    prop::sample::select(vec![
        PackerKind::Shelf,
        PackerKind::Skyline,
        PackerKind::MaximalRectangles,
    ])
}

fn arb_attestation_glyph() -> impl Strategy<Value = GlyphSize> {
    (1_u32..=128, 1_u32..=128).prop_map(|(width, height)| GlyphSize { width, height })
}

fn arb_attestation_glyphs() -> impl Strategy<Value = Vec<GlyphSize>> {
    prop::collection::vec(arb_attestation_glyph(), 0..=128)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_atlas_packing_attestation_pack_into_counts_every_glyph(
        kind in arb_packer_kind(),
        glyphs in arb_attestation_glyphs(),
    ) {
        let (stats, placed_total, rejected_total) =
            atlas_packing_attestation::pack_into(kind, &glyphs);

        prop_assert_eq!(placed_total.saturating_add(rejected_total), glyphs.len() as u64);
        prop_assert_eq!(stats.alloc_total, placed_total);
        prop_assert_eq!(stats.reject_total, rejected_total);
    }

    #[test]
    fn proptest_atlas_packing_attestation_stats_stay_inside_atlas_bounds(
        kind in arb_packer_kind(),
        glyphs in arb_attestation_glyphs(),
    ) {
        let (stats, _, _) = atlas_packing_attestation::pack_into(kind, &glyphs);
        let atlas_bytes = u64::from(atlas_packing_attestation::ATLAS_SIZE)
            * u64::from(atlas_packing_attestation::ATLAS_SIZE);

        prop_assert!(stats.used_bytes <= atlas_bytes);
        prop_assert!(stats.efficiency_pct() <= 100);
        prop_assert!(stats.wasted_pct() <= 100);
        prop_assert_eq!(stats.efficiency_pct() + stats.wasted_pct(), 100);
    }

    #[test]
    fn proptest_atlas_packing_attestation_packer_labels_are_stable_json_atoms(kind in arb_packer_kind()) {
        let label = atlas_packing_attestation::packer_label(kind);

        prop_assert!(!label.is_empty());
        prop_assert!(label.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'));
        prop_assert_eq!(
            label,
            match kind {
                PackerKind::Shelf => "shelf",
                PackerKind::Skyline => "skyline",
                PackerKind::MaximalRectangles => "maximal_rectangles",
            }
        );
    }

    #[test]
    fn proptest_atlas_packing_attestation_corpora_are_nonempty_unique_and_nonzero(take_count in 0_usize..=2_000) {
        let corpora = atlas_packing_attestation::corpora();
        let names: BTreeSet<&str> = corpora.iter().map(|(name, _)| *name).collect();

        prop_assert_eq!(names.len(), corpora.len());
        prop_assert!(names.contains("latin"));
        prop_assert!(names.contains("cjk"));
        prop_assert!(names.contains("nerd_font"));
        prop_assert!(names.contains("emoji"));

        for (_, glyphs) in corpora {
            prop_assert!(!glyphs.is_empty());
            for glyph in glyphs.iter().take(take_count) {
                prop_assert!(glyph.width > 0);
                prop_assert!(glyph.height > 0);
            }
        }
    }

    #[test]
    fn proptest_atlas_packing_attestation_schema_and_corpus_runs_are_consistent(take_count in 0_usize..=2_000) {
        prop_assert_eq!(atlas_packing_attestation::SCHEMA_VERSION, "1.0.0");
        prop_assert_eq!(atlas_packing_attestation::ATLAS_SIZE, 2048);

        for (_, glyphs) in atlas_packing_attestation::corpora() {
            let bounded: Vec<GlyphSize> = glyphs.into_iter().take(take_count).collect();
            for kind in [
                PackerKind::Shelf,
                PackerKind::Skyline,
                PackerKind::MaximalRectangles,
            ] {
                let (stats, placed_total, rejected_total) =
                    atlas_packing_attestation::pack_into(kind, &bounded);

                prop_assert_eq!(placed_total + rejected_total, bounded.len() as u64);
                prop_assert_eq!(stats.alloc_total + stats.reject_total, bounded.len() as u64);
            }
        }
    }
}
