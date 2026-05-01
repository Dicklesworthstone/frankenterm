//! Sparse texture-array atlas substrate (ft-2okh0.15).
//!
//! Pure-logic substrate for the bead's "sparse texture-array
//! atlas — alien-artifact uplift". The integration crate handles
//! the actual wgpu texture-array creation, sparse residency
//! commitment, and shader sampling; this module ships the
//! GPU-vendor compatibility matrix, feature-detection decision
//! tree, slice/tile addressing, allocation policy, and the
//! fallback-to-flat-2D path.
//!
//! ## What this module ships
//!
//! - `GpuVendor` 6-variant family (`AppleSilicon`/`Nvidia`/`AmdGcn1Plus`/
//!   `IntelTigerLakePlus`/`IntelOlder`/`Other`) per the bead's
//!   compatibility table.
//! - `CompatibilityTier` 4-variant (`Tier1`/`Tier2`/`Tier3`/`Unknown`).
//! - `compatibility_tier(GpuVendor) -> CompatibilityTier`.
//! - `SparseFeatureQuery` — what wgpu reports at startup.
//! - `should_enable_sparse(...)` decision tree composing vendor +
//!   feature query + operator override → `SparseDecision`.
//! - `ArraySlice(u8)` — bounded to 256 (bead default).
//! - `TileCoord` — `{ array_slice, tile_x, tile_y }` addressing.
//! - `AtlasArrayConfig` — 256 slices × 4096² × 128² tiles
//!   (= 1024 tiles per slice). Operator-tunable.
//! - `SliceState` — per-slice tile usage tracker.
//! - `SparseAllocationDecision` 3-variant approve/deny enum.
//! - `select_target_slice` — pick the slice with most free
//!   tiles for next allocation.
//! - `compute_sparse_savings_bytes` — bytes saved vs. fully
//!   committed flat texture.
//! - `SparseTelemetry` — per-session counters per the bead's
//!   "Structured logging" section.
//!
//! ## What is deferred to the integration bead (ft-2okh0.15.cont)
//!
//! - wgpu feature detection at startup.
//! - Actual sparse-residency texture-array creation
//!   (vk_sparse_residency / MTLHeap / tiled resources).
//! - Shader sample-array updates (glyph_id includes
//!   array_slice).
//! - Per-tile commitment / decommitment via wgpu sparse APIs.
//! - Fallback wiring to flat 2D atlas
//!   (BR-TERM-EMULATOR-UPLIFT.1.1).
//! - JSON-line telemetry emit (substrate provides counters,
//!   integration emits).

#![allow(dead_code)]

// ============================================================================
// GPU vendor + compatibility tier
// ============================================================================

/// GPU vendor families per the bead's compatibility matrix. The
/// integration's wgpu adapter-info heuristic maps adapter strings
/// to these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuVendor {
    /// Apple Silicon (M1/M2/M3/M4) — Tier 1, Metal.
    AppleSilicon,
    /// Nvidia (GTX 600 series and later) — Tier 1.
    Nvidia,
    /// AMD GCN 1.0 and later — Tier 1.
    AmdGcn1Plus,
    /// Intel Tiger Lake (11th gen) and later — Tier 2.
    IntelTigerLakePlus,
    /// Pre-Tiger-Lake Intel — sparse residency unavailable;
    /// substrate forces fallback to flat 2D atlas.
    IntelOlder,
    /// Anything else — unknown support; substrate defers to
    /// runtime feature query.
    Other,
}

/// Bead's tier classification. The integration uses this to
/// gate the test matrix and the operator-facing sparse-active
/// hint in `ft doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CompatibilityTier {
    /// Native sparse residency, well-tested across the model
    /// range. Apple Silicon, Nvidia, AMD.
    Tier1,
    /// Native sparse residency, partial or recent test coverage.
    /// Intel Tiger Lake+.
    Tier2,
    /// Sparse residency MIGHT work but isn't tested here.
    /// Mesa lavapipe, mobile GPUs.
    Tier3,
    /// No sparse residency — fall back to flat 2D atlas.
    Unknown,
}

#[must_use]
pub const fn compatibility_tier(vendor: GpuVendor) -> CompatibilityTier {
    match vendor {
        GpuVendor::AppleSilicon | GpuVendor::Nvidia | GpuVendor::AmdGcn1Plus => {
            CompatibilityTier::Tier1
        }
        GpuVendor::IntelTigerLakePlus => CompatibilityTier::Tier2,
        GpuVendor::Other => CompatibilityTier::Tier3,
        GpuVendor::IntelOlder => CompatibilityTier::Unknown,
    }
}

// ============================================================================
// Feature query + decision
// ============================================================================

/// What the integration's wgpu adapter reports at startup. The
/// bead's "Detect at startup via wgpu feature query" gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SparseFeatureQuery {
    /// Sparse residency available on this GPU per wgpu.
    pub sparse_residency_available: bool,
    /// Texture-array binding available (always true on modern
    /// stacks; substrate keeps it explicit for the operator
    /// override path).
    pub texture_array_available: bool,
    /// Maximum array layer count the GPU supports. Substrate
    /// caps allocation at min(this, 256).
    pub max_array_layers: u32,
    /// Maximum 2D-texture dimension. Substrate caps slice_dim
    /// at min(this, 4096).
    pub max_texture_2d_dim: u32,
}

impl SparseFeatureQuery {
    /// A defensive default for the no-info case (e.g. early
    /// startup, before adapter-info lands). Forces fallback.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            sparse_residency_available: false,
            texture_array_available: false,
            max_array_layers: 1,
            max_texture_2d_dim: 4096,
        }
    }
}

/// Operator override per the bead — most operators run defaults,
/// but power users can force sparse off (debugging) or on
/// (testing on edge GPUs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SparseOverride {
    /// Substrate decides per the vendor + feature query.
    #[default]
    Auto,
    /// Force sparse residency off; use flat 2D atlas.
    ForceOff,
    /// Force sparse residency on (operator-acknowledged risk).
    ForceOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SparseDecision {
    /// Sparse texture-array atlas active.
    Native,
    /// Flat 2D atlas (BR-TERM-EMULATOR-UPLIFT.1.1) used instead.
    /// Bead's fallback path: "No user-visible degradation."
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SparseDecisionReason {
    /// Auto + Tier1 vendor + features available.
    AutoTier1,
    /// Auto + Tier2 vendor + features available.
    AutoTier2,
    /// Auto + Tier3 vendor + features available — substrate
    /// honours the runtime detect even though the test matrix
    /// can't promise tier-1 stability.
    AutoTier3,
    /// Operator forced on.
    Override,
    /// Operator forced off.
    OperatorDisabled,
    /// Vendor classified as unknown (e.g. older Intel).
    VendorUnsupported,
    /// Feature query says no.
    FeatureQueryNegative,
    /// Texture array unavailable — needed for the array-slice
    /// addressing path even when sparse residency is on.
    TextureArrayUnavailable,
}

/// Composed decision: what the integration should activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedSparseDecision {
    pub decision: SparseDecision,
    pub reason: SparseDecisionReason,
}

/// Decision tree. Order matters:
///
/// 1. `ForceOff` always falls back.
/// 2. `ForceOn` always tries native (operator owns the risk).
/// 3. Auto: vendor `IntelOlder` always falls back.
/// 4. Auto: feature query negative on sparse residency or
///    texture array → fallback with the precise reason.
/// 5. Auto: native sparse, attribute the tier.
#[must_use]
pub fn should_enable_sparse(
    vendor: GpuVendor,
    query: SparseFeatureQuery,
    override_: SparseOverride,
) -> ResolvedSparseDecision {
    match override_ {
        SparseOverride::ForceOff => {
            return ResolvedSparseDecision {
                decision: SparseDecision::Fallback,
                reason: SparseDecisionReason::OperatorDisabled,
            };
        }
        SparseOverride::ForceOn => {
            return ResolvedSparseDecision {
                decision: SparseDecision::Native,
                reason: SparseDecisionReason::Override,
            };
        }
        SparseOverride::Auto => {}
    }

    if matches!(vendor, GpuVendor::IntelOlder) {
        return ResolvedSparseDecision {
            decision: SparseDecision::Fallback,
            reason: SparseDecisionReason::VendorUnsupported,
        };
    }

    if !query.sparse_residency_available {
        return ResolvedSparseDecision {
            decision: SparseDecision::Fallback,
            reason: SparseDecisionReason::FeatureQueryNegative,
        };
    }

    if !query.texture_array_available {
        return ResolvedSparseDecision {
            decision: SparseDecision::Fallback,
            reason: SparseDecisionReason::TextureArrayUnavailable,
        };
    }

    let reason = match compatibility_tier(vendor) {
        CompatibilityTier::Tier1 => SparseDecisionReason::AutoTier1,
        CompatibilityTier::Tier2 => SparseDecisionReason::AutoTier2,
        CompatibilityTier::Tier3 => SparseDecisionReason::AutoTier3,
        CompatibilityTier::Unknown => {
            // Already handled IntelOlder above; this arm is the
            // belt-and-braces path for any future Unknown vendor.
            return ResolvedSparseDecision {
                decision: SparseDecision::Fallback,
                reason: SparseDecisionReason::VendorUnsupported,
            };
        }
    };
    ResolvedSparseDecision {
        decision: SparseDecision::Native,
        reason,
    }
}

// ============================================================================
// Slice + tile addressing
// ============================================================================

/// Per the bead: "up to 256 array slices."
pub const MAX_ARRAY_SLICES: u16 = 256;
pub const DEFAULT_SLICE_DIM: u32 = 4096;
pub const DEFAULT_TILE_DIM: u32 = 128;

/// Bounded array-slice index. Substrate constructs only via
/// `ArraySlice::new` to keep the 256 cap honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArraySlice(u16);

impl ArraySlice {
    /// Construct an `ArraySlice` from a `u16`. Returns `None`
    /// when the index meets or exceeds `MAX_ARRAY_SLICES`.
    #[must_use]
    pub const fn new(index: u16) -> Option<Self> {
        if index < MAX_ARRAY_SLICES {
            Some(Self(index))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn index(self) -> u16 {
        self.0
    }
}

/// (slice, tile_x, tile_y) addressing for a single tile in the
/// sparse atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    pub array_slice: ArraySlice,
    pub tile_x: u16,
    pub tile_y: u16,
}

// ============================================================================
// Config
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasArrayConfig {
    pub max_slices: u16,
    pub slice_dim: u32,
    pub tile_dim: u32,
}

impl Default for AtlasArrayConfig {
    fn default() -> Self {
        Self {
            max_slices: MAX_ARRAY_SLICES,
            slice_dim: DEFAULT_SLICE_DIM,
            tile_dim: DEFAULT_TILE_DIM,
        }
    }
}

impl AtlasArrayConfig {
    /// Tiles per slice in one dimension. With 4096 slice / 128
    /// tile = 32 tiles per side, 1024 tiles per slice.
    #[must_use]
    pub const fn tiles_per_slice_axis(&self) -> u32 {
        if self.tile_dim == 0 {
            0
        } else {
            self.slice_dim / self.tile_dim
        }
    }

    /// Total tiles per slice (both axes).
    #[must_use]
    pub const fn tiles_per_slice(&self) -> u32 {
        let axis = self.tiles_per_slice_axis();
        axis * axis
    }

    /// Apply runtime caps from the wgpu feature query. Honours
    /// the lower of (config, hardware) for both slice count and
    /// slice dimension.
    #[must_use]
    pub fn clamp_to_query(mut self, query: SparseFeatureQuery) -> Self {
        if query.max_array_layers < self.max_slices as u32 {
            self.max_slices =
                u16::try_from(query.max_array_layers.min(MAX_ARRAY_SLICES as u32)).unwrap_or(0);
        }
        if query.max_texture_2d_dim < self.slice_dim {
            self.slice_dim = query.max_texture_2d_dim;
        }
        self
    }
}

// ============================================================================
// Slice state + allocation
// ============================================================================

/// Per-slice tile usage tracker. The integration's allocator
/// holds an array of these; substrate just tracks the counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceState {
    pub slice: ArraySlice,
    pub tiles_used: u32,
    pub tiles_capacity: u32,
}

impl SliceState {
    #[must_use]
    pub const fn new(slice: ArraySlice, tiles_capacity: u32) -> Self {
        Self {
            slice,
            tiles_used: 0,
            tiles_capacity,
        }
    }

    #[must_use]
    pub const fn tiles_free(&self) -> u32 {
        self.tiles_capacity.saturating_sub(self.tiles_used)
    }

    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.tiles_used >= self.tiles_capacity
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SparseAllocationDecision {
    /// Slice has free tiles; integration commits the next one.
    Approved { slice: ArraySlice },
    /// All current slices are full but we can grow the array
    /// (under MAX_ARRAY_SLICES). Integration adds a new slice.
    GrowArray,
    /// Both current slices and the array cap are exhausted —
    /// integration must spill to the warm tier or refuse the
    /// allocation.
    DeniedArrayFull,
}

/// Pick the slice with the most free tiles to use for the next
/// allocation. Returns `None` when no slices exist or when every
/// slice is full.
#[must_use]
pub fn select_target_slice(slices: &[SliceState]) -> Option<&SliceState> {
    slices.iter().filter(|s| !s.is_full()).max_by(|a, b| {
        a.tiles_free()
            .cmp(&b.tiles_free())
            .then_with(|| b.slice.index().cmp(&a.slice.index()))
    })
}

/// Pure decision: should the integration allocate from an
/// existing slice, grow the array, or refuse?
#[must_use]
pub fn allocation_decision(
    slices: &[SliceState],
    config: AtlasArrayConfig,
) -> SparseAllocationDecision {
    if let Some(target) = select_target_slice(slices) {
        return SparseAllocationDecision::Approved {
            slice: target.slice,
        };
    }
    let used_slices = slices.len() as u32;
    if used_slices < config.max_slices as u32 {
        return SparseAllocationDecision::GrowArray;
    }
    SparseAllocationDecision::DeniedArrayFull
}

// ============================================================================
// Savings calculation
// ============================================================================

/// Bytes saved vs. a fully committed flat texture of the same
/// addressable size. The bead's "sparse_savings_bytes"
/// telemetry counter.
///
/// A flat texture would commit `slice_dim² × max_slices ×
/// bytes_per_pixel` bytes. The sparse path commits only the
/// tiles actually allocated.
#[must_use]
pub fn compute_sparse_savings_bytes(
    config: AtlasArrayConfig,
    tiles_committed: u64,
    bytes_per_pixel: u64,
) -> u64 {
    let tile_bytes = (config.tile_dim as u64) * (config.tile_dim as u64) * bytes_per_pixel;
    let total_addressable_bytes = (config.slice_dim as u64)
        * (config.slice_dim as u64)
        * (config.max_slices as u64)
        * bytes_per_pixel;
    let sparse_committed_bytes = tiles_committed.saturating_mul(tile_bytes);
    total_addressable_bytes.saturating_sub(sparse_committed_bytes)
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SparseTelemetry {
    pub sparse_active: bool,
    pub array_slices_used: u32,
    pub peak_array_slices: u32,
    pub sparse_tiles_allocated: u64,
    pub total_addressable_glyphs: u64,
    pub fallback_engaged: bool,
    pub allocation_grows: u64,
    pub allocation_denials: u64,
}

impl SparseTelemetry {
    pub fn record_decision(&mut self, decision: SparseDecision) {
        self.sparse_active = matches!(decision, SparseDecision::Native);
        self.fallback_engaged = matches!(decision, SparseDecision::Fallback);
    }

    pub fn record_allocation(&mut self, decision: SparseAllocationDecision) {
        match decision {
            SparseAllocationDecision::Approved { .. } => {
                self.sparse_tiles_allocated = self.sparse_tiles_allocated.saturating_add(1);
            }
            SparseAllocationDecision::GrowArray => {
                self.allocation_grows = self.allocation_grows.saturating_add(1);
                self.array_slices_used = self.array_slices_used.saturating_add(1);
                if self.array_slices_used > self.peak_array_slices {
                    self.peak_array_slices = self.array_slices_used;
                }
                self.sparse_tiles_allocated = self.sparse_tiles_allocated.saturating_add(1);
            }
            SparseAllocationDecision::DeniedArrayFull => {
                self.allocation_denials = self.allocation_denials.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice_state(idx: u16, used: u32, capacity: u32) -> SliceState {
        SliceState {
            slice: ArraySlice::new(idx).expect("valid slice"),
            tiles_used: used,
            tiles_capacity: capacity,
        }
    }

    // ----------------------------------------------------------------
    // Compatibility tier
    // ----------------------------------------------------------------

    #[test]
    fn tier_apple_silicon_tier1() {
        assert_eq!(
            compatibility_tier(GpuVendor::AppleSilicon),
            CompatibilityTier::Tier1
        );
    }

    #[test]
    fn tier_nvidia_amd_tier1() {
        assert_eq!(
            compatibility_tier(GpuVendor::Nvidia),
            CompatibilityTier::Tier1
        );
        assert_eq!(
            compatibility_tier(GpuVendor::AmdGcn1Plus),
            CompatibilityTier::Tier1
        );
    }

    #[test]
    fn tier_intel_tiger_lake_plus_tier2() {
        assert_eq!(
            compatibility_tier(GpuVendor::IntelTigerLakePlus),
            CompatibilityTier::Tier2,
        );
    }

    #[test]
    fn tier_intel_older_unknown_means_fallback() {
        assert_eq!(
            compatibility_tier(GpuVendor::IntelOlder),
            CompatibilityTier::Unknown
        );
    }

    #[test]
    fn tier_other_is_tier3() {
        assert_eq!(
            compatibility_tier(GpuVendor::Other),
            CompatibilityTier::Tier3
        );
    }

    // ----------------------------------------------------------------
    // SparseFeatureQuery
    // ----------------------------------------------------------------

    #[test]
    fn feature_query_unknown_defaults_safe() {
        let q = SparseFeatureQuery::unknown();
        assert!(!q.sparse_residency_available);
        assert!(!q.texture_array_available);
        assert_eq!(q.max_array_layers, 1);
    }

    // ----------------------------------------------------------------
    // should_enable_sparse — decision tree
    // ----------------------------------------------------------------

    fn good_query() -> SparseFeatureQuery {
        SparseFeatureQuery {
            sparse_residency_available: true,
            texture_array_available: true,
            max_array_layers: 256,
            max_texture_2d_dim: 4096,
        }
    }

    #[test]
    fn force_off_always_falls_back() {
        let r = should_enable_sparse(
            GpuVendor::AppleSilicon,
            good_query(),
            SparseOverride::ForceOff,
        );
        assert_eq!(r.decision, SparseDecision::Fallback);
        assert_eq!(r.reason, SparseDecisionReason::OperatorDisabled);
    }

    #[test]
    fn force_on_always_picks_native() {
        // Even with intel-older, force-on respects the operator.
        let r = should_enable_sparse(GpuVendor::IntelOlder, good_query(), SparseOverride::ForceOn);
        assert_eq!(r.decision, SparseDecision::Native);
        assert_eq!(r.reason, SparseDecisionReason::Override);
    }

    #[test]
    fn auto_intel_older_falls_back() {
        let r = should_enable_sparse(GpuVendor::IntelOlder, good_query(), SparseOverride::Auto);
        assert_eq!(r.decision, SparseDecision::Fallback);
        assert_eq!(r.reason, SparseDecisionReason::VendorUnsupported);
    }

    #[test]
    fn auto_query_negative_falls_back() {
        let mut q = good_query();
        q.sparse_residency_available = false;
        let r = should_enable_sparse(GpuVendor::Nvidia, q, SparseOverride::Auto);
        assert_eq!(r.decision, SparseDecision::Fallback);
        assert_eq!(r.reason, SparseDecisionReason::FeatureQueryNegative);
    }

    #[test]
    fn auto_no_texture_array_falls_back() {
        let mut q = good_query();
        q.texture_array_available = false;
        let r = should_enable_sparse(GpuVendor::AppleSilicon, q, SparseOverride::Auto);
        assert_eq!(r.decision, SparseDecision::Fallback);
        assert_eq!(r.reason, SparseDecisionReason::TextureArrayUnavailable);
    }

    #[test]
    fn auto_apple_silicon_native_tier1() {
        let r = should_enable_sparse(GpuVendor::AppleSilicon, good_query(), SparseOverride::Auto);
        assert_eq!(r.decision, SparseDecision::Native);
        assert_eq!(r.reason, SparseDecisionReason::AutoTier1);
    }

    #[test]
    fn auto_intel_tiger_lake_native_tier2() {
        let r = should_enable_sparse(
            GpuVendor::IntelTigerLakePlus,
            good_query(),
            SparseOverride::Auto,
        );
        assert_eq!(r.decision, SparseDecision::Native);
        assert_eq!(r.reason, SparseDecisionReason::AutoTier2);
    }

    #[test]
    fn auto_other_native_tier3() {
        let r = should_enable_sparse(GpuVendor::Other, good_query(), SparseOverride::Auto);
        assert_eq!(r.decision, SparseDecision::Native);
        assert_eq!(r.reason, SparseDecisionReason::AutoTier3);
    }

    #[test]
    fn priority_force_off_beats_force_on_intent() {
        // Defensive: ForceOff path must short-circuit before any
        // vendor / query checks.
        let q = SparseFeatureQuery::unknown();
        let r = should_enable_sparse(GpuVendor::AppleSilicon, q, SparseOverride::ForceOff);
        assert_eq!(r.decision, SparseDecision::Fallback);
    }

    // ----------------------------------------------------------------
    // ArraySlice
    // ----------------------------------------------------------------

    #[test]
    fn array_slice_within_cap() {
        assert!(ArraySlice::new(0).is_some());
        assert!(ArraySlice::new(255).is_some());
        assert_eq!(ArraySlice::new(256), None);
        assert_eq!(ArraySlice::new(u16::MAX), None);
    }

    #[test]
    fn array_slice_index_roundtrip() {
        let s = ArraySlice::new(42).unwrap();
        assert_eq!(s.index(), 42);
    }

    // ----------------------------------------------------------------
    // AtlasArrayConfig
    // ----------------------------------------------------------------

    #[test]
    fn config_defaults_match_bead() {
        let c = AtlasArrayConfig::default();
        assert_eq!(c.max_slices, 256);
        assert_eq!(c.slice_dim, 4096);
        assert_eq!(c.tile_dim, 128);
    }

    #[test]
    fn tiles_per_slice_default() {
        let c = AtlasArrayConfig::default();
        // 4096 / 128 = 32 per axis = 1024 total.
        assert_eq!(c.tiles_per_slice_axis(), 32);
        assert_eq!(c.tiles_per_slice(), 1024);
    }

    #[test]
    fn tiles_per_slice_axis_zero_tile_dim_safe() {
        let mut c = AtlasArrayConfig::default();
        c.tile_dim = 0;
        assert_eq!(c.tiles_per_slice_axis(), 0);
    }

    #[test]
    fn config_clamp_to_query_caps_slices() {
        let c = AtlasArrayConfig::default();
        let q = SparseFeatureQuery {
            sparse_residency_available: true,
            texture_array_available: true,
            max_array_layers: 64,
            max_texture_2d_dim: 4096,
        };
        let clamped = c.clamp_to_query(q);
        assert_eq!(clamped.max_slices, 64);
    }

    #[test]
    fn config_clamp_to_query_caps_slice_dim() {
        let c = AtlasArrayConfig::default();
        let q = SparseFeatureQuery {
            sparse_residency_available: true,
            texture_array_available: true,
            max_array_layers: 256,
            max_texture_2d_dim: 2048,
        };
        let clamped = c.clamp_to_query(q);
        assert_eq!(clamped.slice_dim, 2048);
    }

    #[test]
    fn config_clamp_keeps_lower_of_hw_cap_and_substrate_cap() {
        // Hardware reports 1024 layers (way over 256 substrate
        // cap) — substrate cap wins.
        let c = AtlasArrayConfig::default();
        let q = SparseFeatureQuery {
            sparse_residency_available: true,
            texture_array_available: true,
            max_array_layers: 1024,
            max_texture_2d_dim: 8192,
        };
        let clamped = c.clamp_to_query(q);
        assert_eq!(clamped.max_slices, 256);
    }

    // ----------------------------------------------------------------
    // SliceState
    // ----------------------------------------------------------------

    #[test]
    fn slice_state_tiles_free_simple() {
        let s = slice_state(0, 100, 1024);
        assert_eq!(s.tiles_free(), 924);
        assert!(!s.is_full());
    }

    #[test]
    fn slice_state_full_when_used_meets_capacity() {
        let s = slice_state(0, 1024, 1024);
        assert_eq!(s.tiles_free(), 0);
        assert!(s.is_full());
    }

    #[test]
    fn slice_state_full_when_used_exceeds_capacity_safe() {
        // Defensive: tiles_free saturates at zero.
        let s = slice_state(0, 2000, 1024);
        assert_eq!(s.tiles_free(), 0);
        assert!(s.is_full());
    }

    // ----------------------------------------------------------------
    // select_target_slice
    // ----------------------------------------------------------------

    #[test]
    fn select_picks_slice_with_most_free() {
        let slices = vec![
            slice_state(0, 900, 1024),
            slice_state(1, 100, 1024),
            slice_state(2, 500, 1024),
        ];
        let target = select_target_slice(&slices).unwrap();
        assert_eq!(target.slice.index(), 1);
    }

    #[test]
    fn select_skips_full_slices() {
        let slices = vec![
            slice_state(0, 1024, 1024),
            slice_state(1, 1024, 1024),
            slice_state(2, 800, 1024),
        ];
        let target = select_target_slice(&slices).unwrap();
        assert_eq!(target.slice.index(), 2);
    }

    #[test]
    fn select_returns_none_when_all_full() {
        let slices = vec![slice_state(0, 1024, 1024), slice_state(1, 1024, 1024)];
        assert!(select_target_slice(&slices).is_none());
    }

    #[test]
    fn select_returns_none_for_empty_slice_list() {
        let slices: Vec<SliceState> = vec![];
        assert!(select_target_slice(&slices).is_none());
    }

    #[test]
    fn select_breaks_ties_by_lower_slice_index() {
        let slices = vec![
            slice_state(5, 100, 1024),
            slice_state(2, 100, 1024),
            slice_state(8, 100, 1024),
        ];
        let target = select_target_slice(&slices).unwrap();
        assert_eq!(target.slice.index(), 2);
    }

    // ----------------------------------------------------------------
    // allocation_decision
    // ----------------------------------------------------------------

    #[test]
    fn alloc_approved_when_slice_has_room() {
        let slices = vec![slice_state(0, 100, 1024)];
        let d = allocation_decision(&slices, AtlasArrayConfig::default());
        match d {
            SparseAllocationDecision::Approved { slice } => assert_eq!(slice.index(), 0),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn alloc_grows_when_all_slices_full_and_below_cap() {
        let slices = vec![slice_state(0, 1024, 1024), slice_state(1, 1024, 1024)];
        let d = allocation_decision(&slices, AtlasArrayConfig::default());
        assert_eq!(d, SparseAllocationDecision::GrowArray);
    }

    #[test]
    fn alloc_grows_from_empty_slice_list() {
        let slices: Vec<SliceState> = vec![];
        let d = allocation_decision(&slices, AtlasArrayConfig::default());
        assert_eq!(d, SparseAllocationDecision::GrowArray);
    }

    #[test]
    fn alloc_denied_when_array_at_cap() {
        let config = AtlasArrayConfig {
            max_slices: 2,
            ..AtlasArrayConfig::default()
        };
        let slices = vec![slice_state(0, 1024, 1024), slice_state(1, 1024, 1024)];
        let d = allocation_decision(&slices, config);
        assert_eq!(d, SparseAllocationDecision::DeniedArrayFull);
    }

    // ----------------------------------------------------------------
    // compute_sparse_savings_bytes
    // ----------------------------------------------------------------

    #[test]
    fn savings_zero_committed_full_savings() {
        let c = AtlasArrayConfig::default();
        // 4096*4096*256*4 = 17,179,869,184 bytes (~16 GiB
        // addressable space at RGBA8).
        let savings = compute_sparse_savings_bytes(c, 0, 4);
        assert_eq!(savings, 17_179_869_184);
    }

    #[test]
    fn savings_full_committed_zero() {
        let c = AtlasArrayConfig::default();
        // 256 slices * 1024 tiles per slice = 262,144 tiles.
        let savings = compute_sparse_savings_bytes(c, 262_144, 4);
        assert_eq!(savings, 0);
    }

    #[test]
    fn savings_partial_committed_proportional() {
        let c = AtlasArrayConfig::default();
        // 1024 tiles * 128*128*4 = 67,108,864 bytes committed.
        // Total addressable = 17,179,869,184. Savings =
        // 17,112,760,320.
        let savings = compute_sparse_savings_bytes(c, 1024, 4);
        assert_eq!(savings, 17_112_760_320);
    }

    // ----------------------------------------------------------------
    // SparseTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zeroed() {
        let t = SparseTelemetry::default();
        assert!(!t.sparse_active);
        assert!(!t.fallback_engaged);
        assert_eq!(t.array_slices_used, 0);
    }

    #[test]
    fn telemetry_record_native_decision() {
        let mut t = SparseTelemetry::default();
        t.record_decision(SparseDecision::Native);
        assert!(t.sparse_active);
        assert!(!t.fallback_engaged);
    }

    #[test]
    fn telemetry_record_fallback_decision() {
        let mut t = SparseTelemetry::default();
        t.record_decision(SparseDecision::Fallback);
        assert!(!t.sparse_active);
        assert!(t.fallback_engaged);
    }

    #[test]
    fn telemetry_record_alloc_approved_increments_tiles() {
        let mut t = SparseTelemetry::default();
        let s = ArraySlice::new(0).unwrap();
        t.record_allocation(SparseAllocationDecision::Approved { slice: s });
        assert_eq!(t.sparse_tiles_allocated, 1);
        assert_eq!(t.allocation_grows, 0);
    }

    #[test]
    fn telemetry_record_alloc_grow_increments_slices_and_peak() {
        let mut t = SparseTelemetry::default();
        for _ in 0..5 {
            t.record_allocation(SparseAllocationDecision::GrowArray);
        }
        assert_eq!(t.allocation_grows, 5);
        assert_eq!(t.array_slices_used, 5);
        assert_eq!(t.peak_array_slices, 5);
        assert_eq!(t.sparse_tiles_allocated, 5);
    }

    #[test]
    fn telemetry_record_alloc_denied_increments_denials() {
        let mut t = SparseTelemetry::default();
        t.record_allocation(SparseAllocationDecision::DeniedArrayFull);
        assert_eq!(t.allocation_denials, 1);
        assert_eq!(t.sparse_tiles_allocated, 0);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_apple_silicon_default_path() {
        // Modern Apple Silicon laptop, no operator override, full
        // wgpu support. Bead's Tier-1 default path.
        let r = should_enable_sparse(GpuVendor::AppleSilicon, good_query(), SparseOverride::Auto);
        assert_eq!(r.decision, SparseDecision::Native);
        assert_eq!(r.reason, SparseDecisionReason::AutoTier1);
    }

    #[test]
    fn scenario_old_intel_falls_back_no_user_visible_degradation() {
        // Bead's "no user-visible degradation" promise: substrate
        // returns Fallback, integration's flat 2D atlas takes
        // over invisibly.
        let r = should_enable_sparse(
            GpuVendor::IntelOlder,
            SparseFeatureQuery::unknown(),
            SparseOverride::Auto,
        );
        assert_eq!(r.decision, SparseDecision::Fallback);
        assert_eq!(r.reason, SparseDecisionReason::VendorUnsupported);
    }

    #[test]
    fn scenario_full_session_tracks_peak() {
        // Verbose session: grows the array 30×, never gets denied.
        let mut t = SparseTelemetry::default();
        t.record_decision(SparseDecision::Native);
        for _ in 0..30 {
            t.record_allocation(SparseAllocationDecision::GrowArray);
        }
        assert!(t.sparse_active);
        assert_eq!(t.peak_array_slices, 30);
        assert_eq!(t.allocation_denials, 0);
    }

    #[test]
    fn scenario_glyph_rich_session_at_cap_denies() {
        // CJK + emoji + 12 Nerd Font variants — fills 256 slices.
        let config = AtlasArrayConfig {
            max_slices: 256,
            ..AtlasArrayConfig::default()
        };
        let slices: Vec<SliceState> = (0..256)
            .map(|i| slice_state(i as u16, 1024, 1024))
            .collect();
        let d = allocation_decision(&slices, config);
        assert_eq!(d, SparseAllocationDecision::DeniedArrayFull);
    }

    #[test]
    fn scenario_savings_meaningful_at_realistic_workload() {
        // A typical scrollback session might allocate 5,000 tiles
        // (5,000 * 128 * 128 * 4 = 327 MiB committed) out of the
        // 16 GiB addressable space.
        let c = AtlasArrayConfig::default();
        let savings = compute_sparse_savings_bytes(c, 5_000, 4);
        // Savings should be > 16 GiB - 1 GiB.
        assert!(savings > 16 * 1024 * 1024 * 1024 - 1024 * 1024 * 1024);
    }
}
