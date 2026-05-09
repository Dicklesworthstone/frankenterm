use proptest::prelude::*;

use frankenterm_core::instanced_cell::{
    CellAttributes, CellInstance, CursorFlags, InstanceBufferAction, InstanceBufferConfig,
    bandwidth_savings_bytes, bandwidth_savings_pct, bandwidth_savings_ratio, decide_buffer_action,
    fill_pct, instance_bytes_per_cell, legacy_bytes_per_cell, pack_color_rgba8, unpack_color_rgba8,
};

fn arb_attr_flag() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(CellAttributes::ITALIC),
        Just(CellAttributes::BOLD),
        Just(CellAttributes::UNDERLINE),
        Just(CellAttributes::STRIKETHROUGH),
        Just(CellAttributes::DIM),
        Just(CellAttributes::REVERSE),
        Just(CellAttributes::BLINK),
        Just(CellAttributes::CONCEAL),
    ]
}

fn arb_cursor_flag() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(CursorFlags::CURSOR_VISIBLE),
        Just(CursorFlags::CURSOR_BLINKING),
        Just(CursorFlags::INSERT_MODE),
        Just(CursorFlags::SELECTION),
        Just(CursorFlags::RESERVED_MASK),
    ]
}

fn arb_rgba() -> impl Strategy<Value = [f32; 4]> {
    (-2.0f32..=2.0, -2.0f32..=2.0, -2.0f32..=2.0, -2.0f32..=2.0).prop_map(<[f32; 4]>::from)
}

fn arb_instance_config() -> impl Strategy<Value = InstanceBufferConfig> {
    (
        1u32..=16_384,
        16_385u32..=1_000_000,
        1.05f32..=4.0,
        0.05f32..=0.95,
        0u32..=512,
        0u8..=100,
    )
        .prop_map(
            |(
                min_capacity,
                max_capacity,
                grow_factor,
                shrink_factor,
                shrink_after_low_frames,
                shrink_threshold_pct,
            )| InstanceBufferConfig {
                min_capacity,
                max_capacity,
                grow_factor,
                shrink_factor,
                shrink_after_low_frames,
                shrink_threshold_pct,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_instanced_cell_attribute_bits_follow_bitwise_semantics(
        initial in any::<u8>(),
        flag in arb_attr_flag(),
    ) {
        let mut attrs = CellAttributes::from_bits(initial);
        prop_assert_eq!(attrs.bits(), initial);
        prop_assert_eq!(attrs.contains(flag), (initial & flag) == flag);
        prop_assert_eq!(attrs.is_empty(), initial == 0);

        attrs.insert(flag);
        prop_assert_eq!(attrs.bits(), initial | flag);
        prop_assert!(attrs.contains(flag));

        attrs.toggle(flag);
        prop_assert_eq!(attrs.bits(), (initial | flag) ^ flag);

        attrs.remove(flag);
        prop_assert_eq!(attrs.bits() & flag, 0);
    }

    #[test]
    fn proptest_instanced_cell_cursor_flags_follow_bitwise_semantics(
        initial in any::<u8>(),
        flag in arb_cursor_flag(),
    ) {
        let mut cursor = CursorFlags::from_bits(initial);
        prop_assert_eq!(cursor.bits(), initial);
        prop_assert_eq!(cursor.contains(flag), (initial & flag) == flag);
        prop_assert_eq!(cursor.is_empty(), initial == 0);

        cursor.insert(flag);
        prop_assert_eq!(cursor.bits(), initial | flag);
        prop_assert!(cursor.contains(flag));

        cursor.remove(flag);
        prop_assert_eq!(cursor.bits(), (initial | flag) & !flag);
        prop_assert!(!cursor.contains(flag));
    }

    #[test]
    fn proptest_instanced_cell_color_pack_round_trip_is_bounded_and_idempotent(
        rgba in arb_rgba(),
    ) {
        let packed = pack_color_rgba8(rgba);
        let unpacked = unpack_color_rgba8(packed);

        for channel in unpacked {
            prop_assert!((0.0..=1.0).contains(&channel));
        }

        let repacked = pack_color_rgba8(unpacked);
        prop_assert_eq!(repacked, packed);
    }

    #[test]
    fn proptest_instanced_cell_instance_constructor_round_trips_public_fields(
        row in any::<u16>(),
        col in any::<u16>(),
        glyph_id in any::<u32>(),
        fg_color in any::<u32>(),
        bg_color in any::<u32>(),
        attr_bits in any::<u8>(),
        cursor_bits in any::<u8>(),
    ) {
        let attrs = CellAttributes::from_bits(attr_bits);
        let cursor = CursorFlags::from_bits(cursor_bits);
        let instance = CellInstance::new(row, col, glyph_id, fg_color, bg_color, attrs, cursor);

        prop_assert_eq!(instance.row, row);
        prop_assert_eq!(instance.col, col);
        prop_assert_eq!(instance.glyph_id, glyph_id);
        prop_assert_eq!(instance.fg_color, fg_color);
        prop_assert_eq!(instance.bg_color, bg_color);
        prop_assert_eq!(instance.cell_attributes(), attrs);
        prop_assert_eq!(instance.cursor(), cursor);
        prop_assert_eq!(instance.extra, 0);
        prop_assert_eq!(instance.is_default(), instance == CellInstance::default());
    }

    #[test]
    fn proptest_instanced_cell_buffer_decisions_preserve_capacity_invariants(
        config in arb_instance_config(),
        current_capacity in 0u32..=1_000_000,
        requested in 0u32..=1_200_000,
        low_frames in 0u32..=1_000,
    ) {
        match decide_buffer_action(config, current_capacity, requested, low_frames) {
            InstanceBufferAction::Split { per_call_cap } => {
                prop_assert!(requested > config.max_capacity);
                prop_assert_eq!(per_call_cap, config.max_capacity);
            }
            InstanceBufferAction::Grow { from, to } => {
                prop_assert_eq!(from, current_capacity);
                prop_assert!(requested > current_capacity);
                prop_assert!(to >= requested);
                prop_assert!(to >= config.min_capacity);
                prop_assert!(to <= config.max_capacity);
            }
            InstanceBufferAction::Shrink { from, to } => {
                prop_assert_eq!(from, current_capacity);
                prop_assert!(requested <= current_capacity);
                prop_assert!(low_frames >= config.shrink_after_low_frames);
                prop_assert!(to >= config.min_capacity);
                prop_assert!(to >= requested);
                prop_assert!(to < current_capacity);
            }
            InstanceBufferAction::Submit { capacity } => {
                prop_assert_eq!(capacity, current_capacity);
                prop_assert!(requested <= config.max_capacity);
                prop_assert!(requested <= current_capacity);
            }
        }

        let fill = fill_pct(requested, current_capacity);
        prop_assert!(fill <= 100);
        if current_capacity == 0 {
            prop_assert_eq!(fill, 100);
        }
    }

    #[test]
    fn proptest_instanced_cell_bandwidth_math_stays_consistent(
        _unit in any::<()>(),
    ) {
        prop_assert!(legacy_bytes_per_cell() > instance_bytes_per_cell());
        prop_assert_eq!(
            bandwidth_savings_bytes(),
            legacy_bytes_per_cell() - instance_bytes_per_cell(),
        );

        let expected_ratio = bandwidth_savings_bytes() as f64 / legacy_bytes_per_cell() as f64;
        prop_assert!((bandwidth_savings_ratio() - expected_ratio).abs() < f64::EPSILON);
        prop_assert_eq!(bandwidth_savings_pct(), (expected_ratio * 100.0).round() as u32);
    }
}
