use proptest::prelude::*;

use frankenterm_core::atlas_bin_packing::{
    AllocationOutcome, Atlas2DSize, GlyphSize, PackedRect, PackerKind, make_packer,
};
use frankenterm_core::atlas_packing_telemetry::{
    AllocationPosRecord, GlyphSizeRecord, PackingEvent, PackingScenarioRecorder, packer_label,
};

fn fixed_clock() -> u64 {
    1_700_000_000_000
}

fn packer_kind_strategy() -> impl Strategy<Value = PackerKind> {
    prop::sample::select(vec![
        PackerKind::Shelf,
        PackerKind::Skyline,
        PackerKind::MaximalRectangles,
    ])
}

fn small_glyph_strategy() -> impl Strategy<Value = GlyphSize> {
    (1_u32..=96, 1_u32..=96).prop_map(|(width, height)| GlyphSize::new(width, height))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_atlas_packing_telemetry_record_conversions_preserve_geometry(
        width in 1_u32..=4096,
        height in 1_u32..=4096,
        x in 0_u32..=4096,
        y in 0_u32..=4096,
    ) {
        let glyph = GlyphSize::new(width, height);
        let glyph_record = GlyphSizeRecord::from(glyph);
        let rect = PackedRect {
            x,
            y,
            width,
            height,
        };
        let pos_record = AllocationPosRecord::from(rect);

        prop_assert_eq!(glyph_record.width, width);
        prop_assert_eq!(glyph_record.height, height);
        prop_assert_eq!(pos_record.x, x);
        prop_assert_eq!(pos_record.y, y);
    }

    #[test]
    fn proptest_atlas_packing_telemetry_packer_labels_are_stable_and_json_safe(
        kind in packer_kind_strategy(),
    ) {
        let label = packer_label(kind);

        prop_assert!(!label.is_empty());
        prop_assert!(label.chars().all(|ch| ch.is_ascii_alphanumeric()));
        prop_assert_eq!(
            match kind {
                PackerKind::Shelf => "Shelf",
                PackerKind::Skyline => "Skyline",
                PackerKind::MaximalRectangles => "MaximalRectangles",
            },
            label,
        );
    }

    #[test]
    fn proptest_atlas_packing_telemetry_event_jsonl_roundtrips_without_newlines(
        ts in any::<u64>(),
        glyph_id in any::<u64>(),
        width in 1_u32..=512,
        height in 1_u32..=512,
        x in 0_u32..=512,
        y in 0_u32..=512,
        total_atlas_bytes in 1_u64..=1_000_000,
        wasted_space_after_alloc in 0_u64..=1_000_000,
        kind in packer_kind_strategy(),
        include_pos in any::<bool>(),
        include_reject_reason in any::<bool>(),
    ) {
        let event = PackingEvent {
            ts,
            glyph_id,
            glyph_size: GlyphSizeRecord { width, height },
            allocation_pos: include_pos.then_some(AllocationPosRecord { x, y }),
            packer: packer_label(kind).to_string(),
            wasted_space_after_alloc,
            total_atlas_bytes,
            reject_reason: include_reject_reason.then(|| "AtlasFull".to_string()),
        };

        let line = event.to_jsonl_line().expect("serialize event");
        let value: serde_json::Value = serde_json::from_str(&line).expect("json value");
        let parsed: PackingEvent = serde_json::from_str(&line).expect("event roundtrip");

        prop_assert!(!line.contains('\n'));
        prop_assert_eq!(parsed, event);
        prop_assert_eq!(value.get("allocation_pos").is_some(), include_pos);
        prop_assert_eq!(value.get("reject_reason").is_some(), include_reject_reason);
    }

    #[test]
    fn proptest_atlas_packing_telemetry_recorder_emits_parseable_accounting_lines(
        kind in packer_kind_strategy(),
        atlas_width in 8_u32..=128,
        atlas_height in 8_u32..=128,
        glyphs in prop::collection::vec(small_glyph_strategy(), 1..=16),
    ) {
        let atlas = Atlas2DSize::try_new(atlas_width, atlas_height).expect("non-zero atlas");
        let mut sink = Vec::<u8>::new();
        let mut outcomes = Vec::with_capacity(glyphs.len());
        {
            let mut recorder = PackingScenarioRecorder::with_clock(
                make_packer(kind, atlas),
                &mut sink,
                fixed_clock,
            );
            prop_assert_eq!(recorder.atlas_size(), atlas);
            for (idx, glyph) in glyphs.iter().copied().enumerate() {
                outcomes.push(
                    recorder
                        .record_alloc(idx as u64, glyph)
                        .expect("record allocation"),
                );
            }
            recorder.flush().expect("flush recorder");
        }

        let output = String::from_utf8(sink).expect("utf8 jsonl");
        let lines: Vec<_> = output.lines().collect();
        let mut used_bytes = 0_u64;
        let total_atlas_bytes = atlas.area();

        prop_assert_eq!(lines.len(), glyphs.len());
        for (idx, line) in lines.iter().enumerate() {
            let event: PackingEvent = serde_json::from_str(line).expect("parse event");
            match outcomes[idx] {
                AllocationOutcome::Placed(rect) => {
                    used_bytes = used_bytes.saturating_add(rect.area());
                    prop_assert_eq!(event.allocation_pos, Some(AllocationPosRecord::from(rect)));
                    prop_assert_eq!(event.reject_reason, None);
                }
                AllocationOutcome::Rejected(reason) => {
                    prop_assert_eq!(event.allocation_pos, None);
                    prop_assert!(event.reject_reason.is_some());
                    prop_assert_eq!(outcomes[idx].reject_reason(), Some(reason));
                }
            }

            prop_assert_eq!(event.ts, fixed_clock());
            prop_assert_eq!(event.glyph_id, idx as u64);
            prop_assert_eq!(event.glyph_size, GlyphSizeRecord::from(glyphs[idx]));
            prop_assert_eq!(event.packer, packer_label(kind));
            prop_assert_eq!(event.total_atlas_bytes, total_atlas_bytes);
            prop_assert_eq!(
                event.wasted_space_after_alloc,
                total_atlas_bytes.saturating_sub(used_bytes),
            );
        }
    }

    #[test]
    fn proptest_atlas_packing_telemetry_oversized_rejections_keep_waste_unchanged(
        kind in packer_kind_strategy(),
        atlas_width in 1_u32..=128,
        atlas_height in 1_u32..=128,
        glyph_id in any::<u64>(),
        too_wide in any::<bool>(),
    ) {
        let atlas = Atlas2DSize::try_new(atlas_width, atlas_height).expect("non-zero atlas");
        let glyph = if too_wide {
            GlyphSize::new(atlas_width.saturating_add(1), atlas_height)
        } else {
            GlyphSize::new(atlas_width, atlas_height.saturating_add(1))
        };
        let mut sink = Vec::<u8>::new();
        {
            let mut recorder = PackingScenarioRecorder::with_clock(
                make_packer(kind, atlas),
                &mut sink,
                fixed_clock,
            );
            let outcome = recorder.record_alloc(glyph_id, glyph).expect("record rejection");
            prop_assert!(matches!(outcome, AllocationOutcome::Rejected(_)));
        }

        let output = String::from_utf8(sink).expect("utf8 jsonl");
        let event: PackingEvent = serde_json::from_str(output.trim()).expect("parse event");

        prop_assert_eq!(event.glyph_id, glyph_id);
        prop_assert_eq!(event.allocation_pos, None);
        prop_assert!(event.reject_reason.is_some());
        prop_assert_eq!(event.wasted_space_after_alloc, atlas.area());
        prop_assert_eq!(event.total_atlas_bytes, atlas.area());
    }
}
