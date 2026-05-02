use std::str::FromStr as _;

use frankenterm_core::sparse_texture_atlas::{
    allocation_decision, compatibility_tier, compute_sparse_savings_bytes, select_target_slice,
    should_enable_sparse, ArraySlice, AtlasArrayConfig, CompatibilityTier, GpuVendor,
    ResolvedSparseDecision, SliceState, SparseAllocationDecision, SparseDecision,
    SparseDecisionReason, SparseFeatureQuery, SparseOverride, SparseTelemetry, DEFAULT_SLICE_DIM,
    DEFAULT_TILE_DIM, MAX_ARRAY_SLICES,
};
use proptest::prelude::*;

fn vendor_strategy() -> impl Strategy<Value = GpuVendor> {
    prop::sample::select(vec![
        GpuVendor::AppleSilicon,
        GpuVendor::Nvidia,
        GpuVendor::AmdGcn1Plus,
        GpuVendor::IntelTigerLakePlus,
        GpuVendor::IntelOlder,
        GpuVendor::Other,
    ])
}

fn override_strategy() -> impl Strategy<Value = SparseOverride> {
    prop::sample::select(vec![
        SparseOverride::Auto,
        SparseOverride::ForceOff,
        SparseOverride::ForceOn,
    ])
}

fn allocation_strategy() -> impl Strategy<Value = SparseAllocationDecision> {
    prop_oneof![
        (0_u16..MAX_ARRAY_SLICES).prop_map(|idx| SparseAllocationDecision::Approved {
            slice: ArraySlice::new(idx).expect("generated valid array slice"),
        }),
        Just(SparseAllocationDecision::GrowArray),
        Just(SparseAllocationDecision::DeniedArrayFull),
    ]
}

fn feature_query(
    sparse_residency_available: bool,
    texture_array_available: bool,
    max_array_layers: u32,
    max_texture_2d_dim: u32,
) -> SparseFeatureQuery {
    SparseFeatureQuery {
        sparse_residency_available,
        texture_array_available,
        max_array_layers,
        max_texture_2d_dim,
    }
}

fn expected_auto_reason(vendor: GpuVendor) -> SparseDecisionReason {
    match compatibility_tier(vendor) {
        CompatibilityTier::Tier1 => SparseDecisionReason::AutoTier1,
        CompatibilityTier::Tier2 => SparseDecisionReason::AutoTier2,
        CompatibilityTier::Tier3 => SparseDecisionReason::AutoTier3,
        CompatibilityTier::Unknown => SparseDecisionReason::VendorUnsupported,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_sparse_atlas_decision_tree_respects_override_vendor_and_features(
        vendor in vendor_strategy(),
        override_ in override_strategy(),
        sparse_available in any::<bool>(),
        texture_array_available in any::<bool>(),
        max_array_layers in 0_u32..=1024,
        max_texture_2d_dim in 0_u32..=8192,
    ) {
        let query = feature_query(
            sparse_available,
            texture_array_available,
            max_array_layers,
            max_texture_2d_dim,
        );
        let actual = should_enable_sparse(vendor, query, override_);
        let expected = match override_ {
            SparseOverride::ForceOff => ResolvedSparseDecision {
                decision: SparseDecision::Fallback,
                reason: SparseDecisionReason::OperatorDisabled,
            },
            SparseOverride::ForceOn => ResolvedSparseDecision {
                decision: SparseDecision::Native,
                reason: SparseDecisionReason::Override,
            },
            SparseOverride::Auto if vendor == GpuVendor::IntelOlder => ResolvedSparseDecision {
                decision: SparseDecision::Fallback,
                reason: SparseDecisionReason::VendorUnsupported,
            },
            SparseOverride::Auto if !sparse_available => ResolvedSparseDecision {
                decision: SparseDecision::Fallback,
                reason: SparseDecisionReason::FeatureQueryNegative,
            },
            SparseOverride::Auto if !texture_array_available => ResolvedSparseDecision {
                decision: SparseDecision::Fallback,
                reason: SparseDecisionReason::TextureArrayUnavailable,
            },
            SparseOverride::Auto => ResolvedSparseDecision {
                decision: if compatibility_tier(vendor) == CompatibilityTier::Unknown {
                    SparseDecision::Fallback
                } else {
                    SparseDecision::Native
                },
                reason: expected_auto_reason(vendor),
            },
        };

        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn proptest_sparse_atlas_override_slug_parse_display_roundtrips(
        override_ in override_strategy(),
        invalid in "[A-Za-z0-9_-]{0,24}",
    ) {
        prop_assert_eq!(override_.to_string(), override_.as_str());
        prop_assert_eq!(SparseOverride::from_str(override_.as_str()), Ok(override_));

        if !["auto", "force_off", "force_on"].contains(&invalid.as_str()) {
            let err = SparseOverride::from_str(&invalid).expect_err("invalid sparse override slug");
            prop_assert_eq!(err.input, invalid);
            prop_assert!(err.to_string().contains("unknown sparse-atlas override"));
        }
    }

    #[test]
    fn proptest_sparse_atlas_config_clamps_and_tile_counts_are_saturating(
        max_slices in 0_u16..=MAX_ARRAY_SLICES,
        slice_dim in 0_u32..=8192,
        tile_dim in 0_u32..=512,
        query_layers in 0_u32..=1024,
        query_dim in 0_u32..=8192,
    ) {
        let config = AtlasArrayConfig {
            max_slices,
            slice_dim,
            tile_dim,
        };
        let query = feature_query(true, true, query_layers, query_dim);
        let clamped = config.clamp_to_query(query);
        let expected_max_slices = if query_layers < u32::from(max_slices) {
            u16::try_from(query_layers.min(u32::from(MAX_ARRAY_SLICES))).unwrap_or(0)
        } else {
            max_slices
        };
        let expected_slice_dim = if query_dim < slice_dim { query_dim } else { slice_dim };
        let expected_axis = if tile_dim == 0 { 0 } else { slice_dim / tile_dim };

        prop_assert_eq!(clamped.max_slices, expected_max_slices);
        prop_assert_eq!(clamped.slice_dim, expected_slice_dim);
        prop_assert_eq!(config.tiles_per_slice_axis(), expected_axis);
        prop_assert_eq!(config.tiles_per_slice(), expected_axis.saturating_mul(expected_axis));
        prop_assert_eq!(AtlasArrayConfig::default().max_slices, MAX_ARRAY_SLICES);
        prop_assert_eq!(AtlasArrayConfig::default().slice_dim, DEFAULT_SLICE_DIM);
        prop_assert_eq!(AtlasArrayConfig::default().tile_dim, DEFAULT_TILE_DIM);
    }

    #[test]
    fn proptest_sparse_atlas_allocation_selects_most_free_slice_or_grows(
        slice_usages in prop::collection::vec((0_u32..=128, 0_u32..=128), 0..=16),
        max_slices in 0_u16..=20,
    ) {
        let slices: Vec<_> = slice_usages
            .iter()
            .enumerate()
            .map(|(idx, (used, capacity))| SliceState {
                slice: ArraySlice::new(idx as u16).expect("generated valid slice index"),
                tiles_used: *used,
                tiles_capacity: *capacity,
            })
            .collect();
        let config = AtlasArrayConfig {
            max_slices,
            slice_dim: DEFAULT_SLICE_DIM,
            tile_dim: DEFAULT_TILE_DIM,
        };
        let expected_target = slices
            .iter()
            .filter(|slice| !slice.is_full())
            .max_by(|a, b| {
                a.tiles_free()
                    .cmp(&b.tiles_free())
                    .then_with(|| b.slice.index().cmp(&a.slice.index()))
            });

        prop_assert_eq!(select_target_slice(&slices), expected_target);
        let expected_decision = if let Some(target) = expected_target {
            SparseAllocationDecision::Approved { slice: target.slice }
        } else if (slices.len() as u32) < u32::from(max_slices) {
            SparseAllocationDecision::GrowArray
        } else {
            SparseAllocationDecision::DeniedArrayFull
        };
        prop_assert_eq!(allocation_decision(&slices, config), expected_decision);

        for slice in &slices {
            prop_assert_eq!(slice.tiles_free(), slice.tiles_capacity.saturating_sub(slice.tiles_used));
            prop_assert_eq!(slice.is_full(), slice.tiles_used >= slice.tiles_capacity);
        }
    }

    #[test]
    fn proptest_sparse_atlas_savings_matches_saturating_model(
        max_slices in 0_u16..=MAX_ARRAY_SLICES,
        slice_dim in 0_u32..=8192,
        tile_dim in 0_u32..=512,
        tiles_committed in 0_u64..=1_000_000,
        bytes_per_pixel in 0_u64..=16,
    ) {
        let config = AtlasArrayConfig {
            max_slices,
            slice_dim,
            tile_dim,
        };
        let tile_bytes = u64::from(tile_dim)
            .saturating_mul(u64::from(tile_dim))
            .saturating_mul(bytes_per_pixel);
        let total_addressable_bytes = u64::from(slice_dim)
            .saturating_mul(u64::from(slice_dim))
            .saturating_mul(u64::from(max_slices))
            .saturating_mul(bytes_per_pixel);
        let committed_bytes = tiles_committed.saturating_mul(tile_bytes);
        let expected = total_addressable_bytes.saturating_sub(committed_bytes);

        prop_assert_eq!(
            compute_sparse_savings_bytes(config, tiles_committed, bytes_per_pixel),
            expected,
        );
    }

    #[test]
    fn proptest_sparse_atlas_telemetry_records_decisions_and_allocations(
        sparse_active in any::<bool>(),
        allocations in prop::collection::vec(allocation_strategy(), 0..=64),
    ) {
        let mut telemetry = SparseTelemetry::default();
        let decision = if sparse_active {
            SparseDecision::Native
        } else {
            SparseDecision::Fallback
        };
        telemetry.record_decision(decision);

        let mut expected_tiles = 0_u64;
        let mut expected_grows = 0_u64;
        let mut expected_denials = 0_u64;
        let mut expected_slices_used = 0_u32;
        let mut expected_peak = 0_u32;

        for allocation in allocations {
            telemetry.record_allocation(allocation);
            match allocation {
                SparseAllocationDecision::Approved { .. } => {
                    expected_tiles = expected_tiles.saturating_add(1);
                }
                SparseAllocationDecision::GrowArray => {
                    expected_grows = expected_grows.saturating_add(1);
                    expected_slices_used = expected_slices_used.saturating_add(1);
                    expected_peak = expected_peak.max(expected_slices_used);
                    expected_tiles = expected_tiles.saturating_add(1);
                }
                SparseAllocationDecision::DeniedArrayFull => {
                    expected_denials = expected_denials.saturating_add(1);
                }
            }
        }

        prop_assert_eq!(telemetry.sparse_active, sparse_active);
        prop_assert_eq!(telemetry.fallback_engaged, !sparse_active);
        prop_assert_eq!(telemetry.sparse_tiles_allocated, expected_tiles);
        prop_assert_eq!(telemetry.allocation_grows, expected_grows);
        prop_assert_eq!(telemetry.allocation_denials, expected_denials);
        prop_assert_eq!(telemetry.array_slices_used, expected_slices_used);
        prop_assert_eq!(telemetry.peak_array_slices, expected_peak);
    }
}
