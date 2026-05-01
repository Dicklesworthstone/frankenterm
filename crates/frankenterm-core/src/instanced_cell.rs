//! Instanced-quad cell-instance encoding (ft-mpc9b.6.1).
//!
//! Replaces the legacy 4-vertex-per-cell vertex buffer at
//! `crates/frankenterm-gui/src/quad.rs` with a per-cell instance
//! record. The vertex shader reads the instance buffer and computes
//! the four screen-space corners from a single shared quad index
//! buffer; the fragment shader is unchanged (still samples the
//! stable atlas from ft-mpc9b.1.1).
//!
//! ## What this module ships
//!
//! - `CellInstance` — the 32-byte per-cell instance record
//!   (`#[repr(C)]`, bytemuck-Pod-Zeroable-ready, but bytemuck is the
//!   integration layer's dep).
//! - `CellAttributes` — packed bitflags for italic/bold/underline/
//!   strikethrough/dim/reverse/blink/conceal.
//! - `CursorFlags` — packed bitflags for cursor visible/blinking/
//!   insert-mode/selection.
//! - `pack_color_rgba8` / `unpack_color_rgba8` — `u32 ↔ [f32; 4]`
//!   color round-trip; quantisation error is bounded at 1/255.
//! - Bandwidth math: `legacy_bytes_per_cell()`, `instance_bytes_per_cell()`,
//!   `bandwidth_savings_ratio()`, `bandwidth_savings_pct()`.
//! - `InstanceBufferCapacity` — per-frame capacity policy
//!   (allocation / growth / shrink hysteresis), composable with the
//!   ElasticBuffer pattern from
//!   `crates/frankenterm-gui/src/termwindow/render/elastic_buffer.rs`.
//!
//! ## What is deferred to the integration bead
//!
//! - The actual `wgpu::VertexBufferLayout` / glium
//!   `implement_vertex!` macro wiring (lives in the gui crate so this
//!   crate stays GPU-API-agnostic).
//! - The vertex shader rewrite that derives screen-space corners from
//!   the instance ID + cell grid metrics.
//! - SSIM ≥0.999 regression test against the per-cell baseline
//!   rendering on the golden corpus.
//! - Bench: `instance_count_per_frame` + `vertex_bandwidth_gbps`
//!   telemetry + the ≥75% bandwidth-reduction assertion on a 200-pane
//!   fleet workload.
//! - Atlas integration: `glyph_id` is an opaque u32 here; the
//!   integration layer maps it to a `TextureRect` from
//!   `atlas_stability.rs` (ft-mpc9b.1.1).

#![allow(dead_code)]

// ============================================================================
// Cell attributes (italic / bold / underline / etc.)
// ============================================================================

/// Per-cell glyph attributes packed into a single u8. The vertex
/// shader unpacks these via simple bit tests; the fragment shader
/// never sees them (they affect glyph selection during atlas lookup
/// and a few colour modulations, not the per-pixel sample path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellAttributes(u8);

impl CellAttributes {
    pub const NONE: Self = Self(0);
    pub const ITALIC: u8 = 1 << 0;
    pub const BOLD: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const STRIKETHROUGH: u8 = 1 << 3;
    pub const DIM: u8 = 1 << 4;
    pub const REVERSE: u8 = 1 << 5;
    pub const BLINK: u8 = 1 << 6;
    pub const CONCEAL: u8 = 1 << 7;

    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, flag: u8) -> bool {
        (self.0 & flag) == flag
    }

    pub fn insert(&mut self, flag: u8) {
        self.0 |= flag;
    }

    pub fn remove(&mut self, flag: u8) {
        self.0 &= !flag;
    }

    pub fn toggle(&mut self, flag: u8) {
        self.0 ^= flag;
    }
}

// ============================================================================
// Cursor flags (visible / blinking / insert-mode / selection)
// ============================================================================

/// Per-cell cursor + selection flags. Keeps the cursor concept out of
/// `CellAttributes` so a future schema change to the attribute byte
/// (e.g. adding double-underline) doesn't collide with cursor state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CursorFlags(u8);

impl CursorFlags {
    pub const NONE: Self = Self(0);
    pub const CURSOR_VISIBLE: u8 = 1 << 0;
    pub const CURSOR_BLINKING: u8 = 1 << 1;
    pub const INSERT_MODE: u8 = 1 << 2;
    pub const SELECTION: u8 = 1 << 3;
    /// Reserved bits 4-7 — the integration layer can claim these for
    /// IME composition / search-highlight / mouse-hover without
    /// touching the wire format.
    pub const RESERVED_MASK: u8 = 0b1111_0000;

    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, flag: u8) -> bool {
        (self.0 & flag) == flag
    }

    pub fn insert(&mut self, flag: u8) {
        self.0 |= flag;
    }

    pub fn remove(&mut self, flag: u8) {
        self.0 &= !flag;
    }
}

// ============================================================================
// Color packing
// ============================================================================

/// Pack `[r, g, b, a]` floats in `[0.0, 1.0]` into an `RGBA8` u32.
/// Out-of-range floats are clamped. Quantisation error is bounded at
/// `1/255` per channel.
#[must_use]
pub fn pack_color_rgba8(rgba: [f32; 4]) -> u32 {
    let to_u8 = |x: f32| -> u32 {
        let clamped = x.clamp(0.0, 1.0);
        (clamped * 255.0).round() as u32
    };
    (to_u8(rgba[0]) << 24)
        | (to_u8(rgba[1]) << 16)
        | (to_u8(rgba[2]) << 8)
        | to_u8(rgba[3])
}

/// Unpack an `RGBA8` u32 back to `[r, g, b, a]` floats in `[0.0, 1.0]`.
#[must_use]
pub fn unpack_color_rgba8(packed: u32) -> [f32; 4] {
    let to_f = |b: u32| -> f32 { (b & 0xff) as f32 / 255.0 };
    [
        to_f(packed >> 24),
        to_f(packed >> 16),
        to_f(packed >> 8),
        to_f(packed),
    ]
}

// ============================================================================
// CellInstance
// ============================================================================

/// One per-cell GPU instance record. 32 bytes, naturally aligned for
/// `wgpu::VertexBufferLayout::array_stride` and glium
/// `implement_vertex!`.
///
/// Layout (offsets in bytes):
/// - 0..2:   `row` (u16) — cell row in grid coordinates
/// - 2..4:   `col` (u16) — cell column in grid coordinates
/// - 4..8:   `glyph_id` (u32) — atlas index from `atlas_stability.rs`
/// - 8..12:  `fg_color` (u32, RGBA8)
/// - 12..16: `bg_color` (u32, RGBA8)
/// - 16..17: `attributes` (u8) — `CellAttributes::bits()`
/// - 17..18: `cursor_flags` (u8) — `CursorFlags::bits()`
/// - 18..20: `_pad0` (u16) — keeps `_extra` 4-byte aligned
/// - 20..24: `_extra` (u32) — reserved for integration-layer use
///   (e.g. IME caret offset, ligature group id). Ignored by this
///   module's encoding/decoding round-trip; integration sets and
///   reads it independently.
/// - 24..28: `_pad1` (u32) — reserved padding so the struct round-
///   trips to a 32-byte stride exactly (the GPU vertex-pull path
///   prefers 16- or 32-byte aligned strides for cache lines on most
///   hardware).
/// - 28..32: `_pad2` (u32) — same.
///
/// Total: **32 bytes** per cell. Combined with the shared 6-index
/// quad index buffer (12 bytes total, amortised across an entire
/// frame's worth of cells), the per-cell GPU bandwidth drops from
/// the legacy `LEGACY_VERTEX_STRIDE_BYTES * 4` per cell to 32 bytes —
/// see `bandwidth_savings_pct()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct CellInstance {
    pub row: u16,
    pub col: u16,
    pub glyph_id: u32,
    pub fg_color: u32,
    pub bg_color: u32,
    pub attributes: u8,
    pub cursor_flags: u8,
    _pad0: u16,
    pub extra: u32,
    _pad1: u32,
    _pad2: u32,
}

impl CellInstance {
    /// Build a new `CellInstance` with all reserved bits zeroed. The
    /// integration layer can write to the `extra` field directly
    /// after construction; the padding fields stay opaque.
    #[must_use]
    pub fn new(
        row: u16,
        col: u16,
        glyph_id: u32,
        fg_color: u32,
        bg_color: u32,
        attributes: CellAttributes,
        cursor_flags: CursorFlags,
    ) -> Self {
        Self {
            row,
            col,
            glyph_id,
            fg_color,
            bg_color,
            attributes: attributes.bits(),
            cursor_flags: cursor_flags.bits(),
            _pad0: 0,
            extra: 0,
            _pad1: 0,
            _pad2: 0,
        }
    }

    #[must_use]
    pub const fn cell_attributes(&self) -> CellAttributes {
        CellAttributes::from_bits(self.attributes)
    }

    #[must_use]
    pub const fn cursor(&self) -> CursorFlags {
        CursorFlags::from_bits(self.cursor_flags)
    }

    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.row == 0
            && self.col == 0
            && self.glyph_id == 0
            && self.fg_color == 0
            && self.bg_color == 0
            && self.attributes == 0
            && self.cursor_flags == 0
            && self.extra == 0
    }
}

/// Size of one `CellInstance` in bytes. Asserted at compile time.
pub const INSTANCE_BYTES_PER_CELL: usize = std::mem::size_of::<CellInstance>();
const _: () = {
    assert!(INSTANCE_BYTES_PER_CELL == 32, "CellInstance must be 32 bytes");
};

// ============================================================================
// Bandwidth math
// ============================================================================

/// Per-vertex byte stride of the legacy `quad.rs::Vertex` record.
/// Computed from the actual layout (2 + 2 + 4 + 4 + 3 + 1 + 1 = 17
/// f32 fields = 68 bytes). The integration layer's `Vertex` type is
/// the source of truth; this constant is the ground-truth comparison
/// the bandwidth-savings calculator uses.
pub const LEGACY_VERTEX_STRIDE_BYTES: usize = 68;

/// Vertices per cell in the legacy path (two triangles = 4 unique
/// vertices in the indexed-draw case at `quad.rs:VERTICES_PER_CELL`).
pub const LEGACY_VERTICES_PER_CELL: usize = 4;

/// Total bytes the legacy path uploads per cell (vertex stride times
/// per-cell vertex count).
#[must_use]
pub const fn legacy_bytes_per_cell() -> usize {
    LEGACY_VERTEX_STRIDE_BYTES * LEGACY_VERTICES_PER_CELL
}

/// Bytes uploaded per cell on the instanced path. Just the
/// `CellInstance` stride — the shared 6-index quad index buffer is
/// 12 bytes total *for an entire frame*, which amortises to ~0 per
/// cell once you have more than a handful.
#[must_use]
pub const fn instance_bytes_per_cell() -> usize {
    INSTANCE_BYTES_PER_CELL
}

/// Bytes saved per cell on the instanced path.
#[must_use]
pub const fn bandwidth_savings_bytes() -> usize {
    legacy_bytes_per_cell() - instance_bytes_per_cell()
}

/// Fractional savings as a `f64` in `[0.0, 1.0]`. Pure-arithmetic;
/// useful as the input to the integration bench's
/// `vertex_bandwidth_gbps` reduction assertion.
#[must_use]
pub fn bandwidth_savings_ratio() -> f64 {
    let saved = bandwidth_savings_bytes() as f64;
    let legacy = legacy_bytes_per_cell() as f64;
    saved / legacy
}

/// Savings as an integer percentage `[0..=100]`, rounded to nearest.
#[must_use]
pub fn bandwidth_savings_pct() -> u32 {
    (bandwidth_savings_ratio() * 100.0).round() as u32
}

// ============================================================================
// Per-frame instance-buffer capacity policy
// ============================================================================

/// Operator-tunable thresholds for the instance buffer's growth /
/// shrink hysteresis. Mirrors the `ElasticBuffer` policy shape from
/// the gui's render module so the integration layer can plug straight
/// in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstanceBufferConfig {
    /// Minimum capacity (number of instances) — the buffer never
    /// shrinks below this.
    pub min_capacity: u32,
    /// Maximum capacity. Hitting this hard-caps and the integration
    /// layer must split the frame into multiple draw calls.
    pub max_capacity: u32,
    /// Multiplier applied when growing. `2.0` doubles; `1.5` is the
    /// classic compromise.
    pub grow_factor: f32,
    /// Multiplier applied when shrinking after `shrink_after_low_frames`
    /// consecutive low-fill frames.
    pub shrink_factor: f32,
    /// How many consecutive frames below `shrink_threshold_pct` of
    /// capacity it takes to trigger a shrink.
    pub shrink_after_low_frames: u32,
    /// Below this fill ratio (`actual / capacity`) we count as a
    /// low-fill frame.
    pub shrink_threshold_pct: u8,
}

impl Default for InstanceBufferConfig {
    fn default() -> Self {
        Self {
            min_capacity: 4_096,
            max_capacity: 4_194_304,
            grow_factor: 2.0,
            shrink_factor: 0.5,
            shrink_after_low_frames: 240,
            shrink_threshold_pct: 25,
        }
    }
}

/// Per-frame decision the buffer makes given the requested instance
/// count and current capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceBufferAction {
    /// Capacity is sufficient — submit at the current size.
    Submit { capacity: u32 },
    /// Capacity is below the request; grow by `grow_factor` (capped
    /// at `max_capacity`) before submitting.
    Grow { from: u32, to: u32 },
    /// Capacity has been over-provisioned for `shrink_after_low_frames`
    /// in a row; shrink by `shrink_factor` (floored at `min_capacity`)
    /// before submitting.
    Shrink { from: u32, to: u32 },
    /// Request exceeds `max_capacity`; the integration layer must
    /// split into multiple draw calls. Carries the per-call cap so
    /// the splitter knows the chunk size.
    Split { per_call_cap: u32 },
}

/// Decide what to do given current capacity, this frame's instance
/// request, and the count of consecutive low-fill frames so far.
/// Pure-logic; the integration layer holds the running counter.
#[must_use]
pub fn decide_buffer_action(
    config: InstanceBufferConfig,
    current_capacity: u32,
    requested: u32,
    consecutive_low_frames: u32,
) -> InstanceBufferAction {
    if requested > config.max_capacity {
        return InstanceBufferAction::Split {
            per_call_cap: config.max_capacity,
        };
    }
    if requested > current_capacity {
        let grown = ((current_capacity as f32) * config.grow_factor)
            .ceil()
            .max(requested as f32) as u32;
        let grown = grown.min(config.max_capacity);
        return InstanceBufferAction::Grow {
            from: current_capacity,
            to: grown.max(config.min_capacity),
        };
    }
    if consecutive_low_frames >= config.shrink_after_low_frames {
        let target = ((current_capacity as f32) * config.shrink_factor).floor() as u32;
        let target = target.max(config.min_capacity).max(requested);
        if target < current_capacity {
            return InstanceBufferAction::Shrink {
                from: current_capacity,
                to: target,
            };
        }
    }
    InstanceBufferAction::Submit {
        capacity: current_capacity,
    }
}

/// Compute fill ratio as percent `[0..=100]` for the shrink-decision
/// counter the integration layer keeps. Returns `100` if `capacity`
/// is zero (the buffer is in an uninitialised state — never count
/// that as low-fill).
#[must_use]
pub fn fill_pct(requested: u32, capacity: u32) -> u8 {
    if capacity == 0 {
        return 100;
    }
    let ratio = (u64::from(requested) * 100 / u64::from(capacity)) as u8;
    ratio.min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // CellAttributes
    // ----------------------------------------------------------------

    #[test]
    fn attrs_default_is_empty() {
        let a = CellAttributes::default();
        assert!(a.is_empty());
        assert_eq!(a.bits(), 0);
    }

    #[test]
    fn attrs_insert_and_contains_each_flag() {
        for flag in [
            CellAttributes::ITALIC,
            CellAttributes::BOLD,
            CellAttributes::UNDERLINE,
            CellAttributes::STRIKETHROUGH,
            CellAttributes::DIM,
            CellAttributes::REVERSE,
            CellAttributes::BLINK,
            CellAttributes::CONCEAL,
        ] {
            let mut a = CellAttributes::NONE;
            assert!(!a.contains(flag));
            a.insert(flag);
            assert!(a.contains(flag), "after insert flag={flag:#010b} should contain it");
            assert!(!a.is_empty());
        }
    }

    #[test]
    fn attrs_remove_and_toggle_round_trip() {
        let mut a = CellAttributes::NONE;
        a.insert(CellAttributes::BOLD | CellAttributes::ITALIC);
        assert!(a.contains(CellAttributes::BOLD));
        assert!(a.contains(CellAttributes::ITALIC));
        a.remove(CellAttributes::BOLD);
        assert!(!a.contains(CellAttributes::BOLD));
        assert!(a.contains(CellAttributes::ITALIC));
        a.toggle(CellAttributes::ITALIC);
        assert!(!a.contains(CellAttributes::ITALIC));
        assert!(a.is_empty());
    }

    #[test]
    fn attrs_all_flags_disjoint_bits() {
        let bits = [
            CellAttributes::ITALIC,
            CellAttributes::BOLD,
            CellAttributes::UNDERLINE,
            CellAttributes::STRIKETHROUGH,
            CellAttributes::DIM,
            CellAttributes::REVERSE,
            CellAttributes::BLINK,
            CellAttributes::CONCEAL,
        ];
        let combined = bits.iter().copied().fold(0u8, |acc, b| acc | b);
        assert_eq!(combined, 0xff, "all 8 attr flags should fill the byte");
    }

    // ----------------------------------------------------------------
    // CursorFlags
    // ----------------------------------------------------------------

    #[test]
    fn cursor_default_is_empty() {
        let c = CursorFlags::default();
        assert!(c.is_empty());
    }

    #[test]
    fn cursor_flags_insert_and_remove() {
        let mut c = CursorFlags::NONE;
        c.insert(CursorFlags::CURSOR_VISIBLE | CursorFlags::SELECTION);
        assert!(c.contains(CursorFlags::CURSOR_VISIBLE));
        assert!(c.contains(CursorFlags::SELECTION));
        c.remove(CursorFlags::SELECTION);
        assert!(c.contains(CursorFlags::CURSOR_VISIBLE));
        assert!(!c.contains(CursorFlags::SELECTION));
    }

    #[test]
    fn cursor_reserved_bits_are_separate_from_active_bits() {
        // The 4 active flags occupy bits 0-3; reserved mask is 4-7.
        let active = CursorFlags::CURSOR_VISIBLE
            | CursorFlags::CURSOR_BLINKING
            | CursorFlags::INSERT_MODE
            | CursorFlags::SELECTION;
        assert_eq!(active & CursorFlags::RESERVED_MASK, 0);
        assert_eq!(active | CursorFlags::RESERVED_MASK, 0xff);
    }

    // ----------------------------------------------------------------
    // Colour packing
    // ----------------------------------------------------------------

    #[test]
    fn color_pack_zero_alpha_produces_expected_layout() {
        // [1.0, 0.0, 0.0, 0.0] should be 0xff_00_00_00.
        assert_eq!(pack_color_rgba8([1.0, 0.0, 0.0, 0.0]), 0xff00_0000);
    }

    #[test]
    fn color_pack_full_white() {
        assert_eq!(pack_color_rgba8([1.0, 1.0, 1.0, 1.0]), 0xffff_ffff);
    }

    #[test]
    fn color_pack_clamps_out_of_range() {
        // Above 1.0 clamps to 0xff; below 0.0 clamps to 0x00.
        assert_eq!(pack_color_rgba8([2.0, -1.0, 1.5, 0.5]), 0xff00_ff80);
    }

    #[test]
    fn color_unpack_inverts_pack_within_quantisation_bound() {
        // Round-trip a known canonical: 0x80 quantises 0.5 to ~128.
        let packed = pack_color_rgba8([0.5, 0.25, 0.75, 1.0]);
        let unpacked = unpack_color_rgba8(packed);
        for (a, b) in [
            (unpacked[0], 0.5_f32),
            (unpacked[1], 0.25),
            (unpacked[2], 0.75),
            (unpacked[3], 1.0),
        ] {
            assert!(
                (a - b).abs() <= 1.0 / 255.0,
                "channel {a} differs from {b} by more than 1/255"
            );
        }
    }

    #[test]
    fn color_pack_then_unpack_then_pack_is_idempotent() {
        // Once a colour has been quantised, re-packing it yields the
        // same u32 — i.e. the round-trip is a fixed point.
        for src in [
            [0.5, 0.5, 0.5, 0.5],
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 0.5],
            [0.123, 0.456, 0.789, 1.0],
        ] {
            let p1 = pack_color_rgba8(src);
            let unp = unpack_color_rgba8(p1);
            let p2 = pack_color_rgba8(unp);
            assert_eq!(p1, p2, "second pack should match first for src={src:?}");
        }
    }

    // ----------------------------------------------------------------
    // CellInstance
    // ----------------------------------------------------------------

    #[test]
    fn cell_instance_size_is_32_bytes() {
        // The compile-time assert above guarantees this; the runtime
        // assert here surfaces the failure at test time too so the
        // diagnostic is friendlier when somebody adds a field.
        assert_eq!(std::mem::size_of::<CellInstance>(), 32);
        assert_eq!(INSTANCE_BYTES_PER_CELL, 32);
    }

    #[test]
    fn cell_instance_default_is_default() {
        let c = CellInstance::default();
        assert!(c.is_default());
    }

    #[test]
    fn cell_instance_constructor_sets_visible_fields() {
        let attrs = {
            let mut a = CellAttributes::NONE;
            a.insert(CellAttributes::BOLD | CellAttributes::UNDERLINE);
            a
        };
        let cursor = CursorFlags::from_bits(CursorFlags::CURSOR_VISIBLE);
        let c = CellInstance::new(
            10,
            42,
            7777,
            0xff00_00ff,
            0x0000_00ff,
            attrs,
            cursor,
        );
        assert_eq!(c.row, 10);
        assert_eq!(c.col, 42);
        assert_eq!(c.glyph_id, 7777);
        assert_eq!(c.fg_color, 0xff00_00ff);
        assert_eq!(c.bg_color, 0x0000_00ff);
        assert_eq!(
            c.cell_attributes().bits(),
            CellAttributes::BOLD | CellAttributes::UNDERLINE
        );
        assert_eq!(c.cursor().bits(), CursorFlags::CURSOR_VISIBLE);
        // The reserved fields stay zeroed.
        assert_eq!(c.extra, 0);
    }

    #[test]
    fn cell_instance_extra_field_is_writable_independently() {
        let mut c = CellInstance::new(
            0,
            0,
            0,
            0,
            0,
            CellAttributes::NONE,
            CursorFlags::NONE,
        );
        c.extra = 0xdead_beef;
        assert_eq!(c.extra, 0xdead_beef);
        // Writing extra should not disturb other fields.
        assert_eq!(c.glyph_id, 0);
        assert_eq!(c.fg_color, 0);
    }

    #[test]
    fn cell_instance_round_trip_attributes_through_byte() {
        for bits in 0u8..=0xff {
            let attrs = CellAttributes::from_bits(bits);
            let c = CellInstance::new(0, 0, 0, 0, 0, attrs, CursorFlags::NONE);
            assert_eq!(c.cell_attributes().bits(), bits);
        }
    }

    #[test]
    fn cell_instance_row_col_max_values_round_trip() {
        let c = CellInstance::new(
            u16::MAX,
            u16::MAX,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            CellAttributes::from_bits(0xff),
            CursorFlags::from_bits(0xff),
        );
        assert_eq!(c.row, u16::MAX);
        assert_eq!(c.col, u16::MAX);
        assert_eq!(c.glyph_id, u32::MAX);
    }

    // ----------------------------------------------------------------
    // Bandwidth math
    // ----------------------------------------------------------------

    #[test]
    fn bandwidth_legacy_matches_quad_rs_layout() {
        // The legacy Vertex at crates/frankenterm-gui/src/quad.rs is:
        //   position: [f32; 2]   8
        //   tex:      [f32; 2]   8
        //   fg_color: [f32; 4]  16
        //   alt_color:[f32; 4]  16
        //   hsv:      [f32; 3]  12
        //   has_color: f32       4
        //   mix_value: f32       4
        // Total: 68 bytes.
        assert_eq!(LEGACY_VERTEX_STRIDE_BYTES, 68);
        assert_eq!(LEGACY_VERTICES_PER_CELL, 4);
        assert_eq!(legacy_bytes_per_cell(), 272);
    }

    #[test]
    fn bandwidth_instance_matches_struct_size() {
        assert_eq!(instance_bytes_per_cell(), 32);
    }

    #[test]
    fn bandwidth_savings_bytes_is_positive() {
        assert!(bandwidth_savings_bytes() > 0);
        assert_eq!(bandwidth_savings_bytes(), 272 - 32);
    }

    #[test]
    fn bandwidth_savings_meets_or_exceeds_75_pct_headline() {
        // The bead's headline target: ~75% reduction. Our actual
        // layout achieves ~88%; assert the headline as a hard floor
        // so a future regression (e.g. someone bloats CellInstance)
        // gets caught here.
        let pct = bandwidth_savings_pct();
        assert!(
            pct >= 75,
            "bandwidth savings {pct}% fell below the 75% headline target"
        );
    }

    #[test]
    fn bandwidth_savings_ratio_in_unit_interval() {
        let r = bandwidth_savings_ratio();
        assert!((0.0..=1.0).contains(&r));
    }

    // ----------------------------------------------------------------
    // InstanceBuffer policy
    // ----------------------------------------------------------------

    #[test]
    fn buffer_default_config_has_sensible_floor_and_ceiling() {
        let c = InstanceBufferConfig::default();
        assert!(c.min_capacity > 0);
        assert!(c.max_capacity >= c.min_capacity);
        assert!(c.grow_factor > 1.0);
        assert!(c.shrink_factor > 0.0 && c.shrink_factor < 1.0);
        assert!(c.shrink_after_low_frames > 0);
        assert!(c.shrink_threshold_pct <= 100);
    }

    #[test]
    fn buffer_submit_when_capacity_sufficient() {
        let cfg = InstanceBufferConfig::default();
        let action = decide_buffer_action(cfg, 8_192, 4_000, 0);
        assert_eq!(action, InstanceBufferAction::Submit { capacity: 8_192 });
    }

    #[test]
    fn buffer_grows_by_grow_factor_when_request_exceeds_capacity() {
        let cfg = InstanceBufferConfig::default();
        let action = decide_buffer_action(cfg, 8_192, 10_000, 0);
        match action {
            InstanceBufferAction::Grow { from, to } => {
                assert_eq!(from, 8_192);
                // grow_factor 2.0 → 16_384, which is also >= request.
                assert_eq!(to, 16_384);
            }
            other => panic!("expected Grow, got {other:?}"),
        }
    }

    #[test]
    fn buffer_grow_clamps_to_max_capacity() {
        let cfg = InstanceBufferConfig::default();
        // Request right at max; current capacity is max/4.
        let current = cfg.max_capacity / 4;
        let request = cfg.max_capacity - 1;
        let action = decide_buffer_action(cfg, current, request, 0);
        match action {
            InstanceBufferAction::Grow { to, .. } => {
                assert!(to <= cfg.max_capacity);
                assert!(to >= request);
            }
            other => panic!("expected Grow, got {other:?}"),
        }
    }

    #[test]
    fn buffer_grow_meets_request_when_factor_would_undershoot() {
        // grow_factor=1.5, current=1000, request=10000 — 1500 doesn't
        // meet the request, so we must jump straight to 10000 (or
        // higher).
        let cfg = InstanceBufferConfig {
            grow_factor: 1.5,
            ..InstanceBufferConfig::default()
        };
        let action = decide_buffer_action(cfg, 1_000, 10_000, 0);
        match action {
            InstanceBufferAction::Grow { to, .. } => {
                assert!(to >= 10_000);
            }
            other => panic!("expected Grow, got {other:?}"),
        }
    }

    #[test]
    fn buffer_split_when_request_exceeds_max() {
        let cfg = InstanceBufferConfig::default();
        let action = decide_buffer_action(cfg, 8_192, cfg.max_capacity + 1, 0);
        assert_eq!(
            action,
            InstanceBufferAction::Split {
                per_call_cap: cfg.max_capacity,
            }
        );
    }

    #[test]
    fn buffer_shrinks_after_threshold_low_frames() {
        let cfg = InstanceBufferConfig::default();
        let current = cfg.min_capacity * 16;
        let request = cfg.min_capacity / 2; // very low fill
        let action = decide_buffer_action(cfg, current, request, cfg.shrink_after_low_frames);
        match action {
            InstanceBufferAction::Shrink { from, to } => {
                assert_eq!(from, current);
                assert!(to < from);
                assert!(to >= cfg.min_capacity);
            }
            other => panic!("expected Shrink, got {other:?}"),
        }
    }

    #[test]
    fn buffer_does_not_shrink_below_min_capacity() {
        let cfg = InstanceBufferConfig::default();
        // Already at min_capacity — even after many low frames,
        // shrink would be a no-op so we expect Submit.
        let action =
            decide_buffer_action(cfg, cfg.min_capacity, 100, cfg.shrink_after_low_frames * 4);
        assert_eq!(
            action,
            InstanceBufferAction::Submit {
                capacity: cfg.min_capacity,
            }
        );
    }

    #[test]
    fn buffer_does_not_shrink_below_request_floor() {
        let cfg = InstanceBufferConfig::default();
        // Current is plenty over request; shrink_factor would take us
        // below `request` if left unchecked. The floor must include
        // `request`.
        let current = 100_000;
        let request = 60_000;
        let action = decide_buffer_action(cfg, current, request, cfg.shrink_after_low_frames);
        match action {
            InstanceBufferAction::Shrink { to, .. } => {
                assert!(to >= request);
            }
            // If the math says no shrink is warranted (e.g. shrink
            // would land above current), Submit is also valid.
            InstanceBufferAction::Submit { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn buffer_low_frames_below_threshold_does_not_shrink() {
        let cfg = InstanceBufferConfig::default();
        let current = cfg.min_capacity * 16;
        let request = cfg.min_capacity / 2;
        let action =
            decide_buffer_action(cfg, current, request, cfg.shrink_after_low_frames - 1);
        assert_eq!(action, InstanceBufferAction::Submit { capacity: current });
    }

    #[test]
    fn fill_pct_at_full_capacity_is_100() {
        assert_eq!(fill_pct(8_192, 8_192), 100);
    }

    #[test]
    fn fill_pct_at_quarter_is_25() {
        assert_eq!(fill_pct(2_048, 8_192), 25);
    }

    #[test]
    fn fill_pct_zero_capacity_returns_100() {
        // Uninitialised buffer state; never count as low-fill.
        assert_eq!(fill_pct(0, 0), 100);
        assert_eq!(fill_pct(123, 0), 100);
    }

    #[test]
    fn fill_pct_caps_at_100_when_overrun() {
        // If the integration layer ever asks for more than capacity,
        // pct still saturates at 100 — the shrink path keys off "low
        // fill", not "high fill", so it's safe to clamp.
        assert_eq!(fill_pct(20_000, 8_192), 100);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic frame from the bead
    // ----------------------------------------------------------------

    #[test]
    fn scenario_typical_frame_200_pane_fleet() {
        // Bead bench target: 200 panes × ~80x24 cells = ~384,000
        // cells per frame. At 32 bytes per instance that's ~12 MB of
        // instance data; legacy at 272 bytes per cell would have been
        // ~104 MB. Confirm both numbers match the constants so a
        // future regression in either direction surfaces here.
        let cells_per_frame: u64 = 200 * 80 * 24;
        let legacy_total = cells_per_frame * legacy_bytes_per_cell() as u64;
        let instance_total = cells_per_frame * instance_bytes_per_cell() as u64;
        assert!(instance_total < legacy_total / 4); // ≥75% reduction
        // Sanity: instance total in MB.
        let instance_mb = instance_total / (1024 * 1024);
        assert!(
            (10..=20).contains(&instance_mb),
            "200-pane frame instance data should land between 10-20 MB; got {instance_mb} MB"
        );
    }

    #[test]
    fn scenario_grow_then_shrink_cycle() {
        let cfg = InstanceBufferConfig::default();
        // Frame 1: bursty paste fills 5x current capacity → grow.
        let action_grow = decide_buffer_action(cfg, cfg.min_capacity, cfg.min_capacity * 5, 0);
        let grown_capacity = match action_grow {
            InstanceBufferAction::Grow { to, .. } => to,
            other => panic!("expected Grow, got {other:?}"),
        };
        // Frames 2..N: workload drops back to a trickle. Wait the
        // shrink horizon then expect a shrink.
        let action_shrink = decide_buffer_action(
            cfg,
            grown_capacity,
            grown_capacity / 16,
            cfg.shrink_after_low_frames,
        );
        match action_shrink {
            InstanceBufferAction::Shrink { from, to } => {
                assert_eq!(from, grown_capacity);
                assert!(to < grown_capacity);
                assert!(to >= cfg.min_capacity);
            }
            other => panic!("expected Shrink, got {other:?}"),
        }
    }

    #[test]
    fn scenario_split_at_max_capacity_gives_actionable_chunk_size() {
        let cfg = InstanceBufferConfig::default();
        let mega_request = cfg.max_capacity * 3 + 7;
        let action = decide_buffer_action(cfg, cfg.min_capacity, mega_request, 0);
        match action {
            InstanceBufferAction::Split { per_call_cap } => {
                // Splitter math: ceiling-div of request by cap.
                let chunks = mega_request.div_ceil(per_call_cap);
                assert_eq!(chunks, 4);
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }
}
