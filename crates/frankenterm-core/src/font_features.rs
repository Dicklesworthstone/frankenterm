//! OpenType variable-font + color-glyph format substrate
//! (ft-2okh0.6).
//!
//! Pure-logic substrate for the bead's "OpenType variable font +
//! COLR/CPAL color-glyph support" requirement. The integration
//! crate handles the actual font-introspector port + glyph
//! rasterization; this module ships:
//!
//! - The format taxonomy + per-format atlas-key extension.
//! - Variable-axis classification (weight / width / slant / etc.)
//!   with bead-cited bounds.
//! - Reduce-motion gate (A11Y.5 cross-link).
//! - A11Y emoji-name lookup schema (CLDR).
//! - Atlas-key derivation that includes the variable-axis vector
//!   so a glyph at weight=400 caches separately from weight=700.
//! - Telemetry counters per the bead's structured-logging schema.
//!
//! ## What this module ships
//!
//! - `GlyphFormat` 5-variant (`Monochrome / VariableMono /
//!   ColrCpal / Sbix / CbdtEblc`) — the bead's test-corpus
//!   formats.
//! - `VariableAxis` — `Weight / Width / Slant / OpticalSize /
//!   Italic / Custom { tag }`. The bead notes "Recursive
//!   variable monospace" as the canonical test font (which
//!   exercises Weight + Slant axes).
//! - `AxisValue` — bounded `(axis, value)` pair with sane
//!   per-axis ranges (Weight 1–1000 per OpenType spec, etc.).
//! - `AxisVector` — sorted Vec<AxisValue> with stable hashing
//!   so atlas-keys are deterministic.
//! - `derive_axis_atlas_key` — FNV-1a-64 over (font_id +
//!   glyph_id + axis-vector + format). Pure-logic; the
//!   integration's atlas extends its existing key with this.
//! - `should_animate_axis_transition` — A11Y.5 gate predicate.
//!   When prefers-reduced-motion is on, axis transitions snap
//!   instead of animating.
//! - `EmojiName` — `{ codepoint, name, source }` payload the
//!   integration emits via accessibility tree.
//! - `lookup_emoji_name` — substrate's CLDR-name fallback
//!   (no full DB; returns `None` and the integration consults
//!   the real CLDR table).
//! - `FontFeaturesTelemetry` — bead's structured-logging
//!   counters (per-format render counts + atlas-key reuse +
//!   a11y emit count).
//!
//! ## What is deferred to the integration bead (ft-2okh0.6.cont)
//!
//! - Port or vendor the rio `font_introspector` module
//!   (`legacy_rio/rio/sugarloaf/src/font_introspector/mod.rs`).
//! - Variable-font axis parsing via `font_introspector::Variation`.
//! - COLR/CPAL renderer compositing layered solid colors with
//!   ICC-profile colour management.
//! - Bitmap-strike support per format (CBLC/CBDT, sbix, EBLC/EBDT).
//! - Atlas integration: extend existing atlas-key with axis vector.
//! - Real CLDR name DB (`unic-cldr` or vendored JSON).
//! - JSON-line emission at `tests/variable_fonts/logs/<scenario>.jsonl`.

#![allow(dead_code)]

// ============================================================================
// GlyphFormat — test-corpus formats per the bead
// ============================================================================

/// Per the bead's test-corpus matrix:
///
/// | Format | Test font |
/// |---|---|
/// | Monochrome / VariableMono | Recursive |
/// | ColrCpal | Noto Color Emoji v2 |
/// | Sbix | Apple Color Emoji |
/// | CbdtEblc | Noto Color Emoji v1, terminus |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GlyphFormat {
    /// Static single-axis monochrome glyph.
    #[default]
    Monochrome,
    /// Variable monochrome — same glyph rendered at different
    /// axis values. Atlas key includes axis vector.
    VariableMono,
    /// COLRv0 / COLRv1 + CPAL palette. Layered solid-color
    /// glyphs.
    ColrCpal,
    /// Apple sbix bitmap strikes.
    Sbix,
    /// CBLC/CBDT (Google) or EBLC/EBDT (legacy) bitmap strikes.
    CbdtEblc,
}

impl GlyphFormat {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Monochrome => "monochrome",
            Self::VariableMono => "variable_mono",
            Self::ColrCpal => "colr_cpal",
            Self::Sbix => "sbix",
            Self::CbdtEblc => "cbdt_eblc",
        }
    }

    /// Whether this format renders in colour (vs monochrome
    /// alpha-only). Bead: COLR/CPAL + sbix + CBDT/EBDT all
    /// produce RGBA atlas entries.
    #[must_use]
    pub const fn is_color(self) -> bool {
        matches!(self, Self::ColrCpal | Self::Sbix | Self::CbdtEblc)
    }

    /// Whether the atlas key needs the variable-axis vector
    /// to disambiguate. Only `VariableMono` does (other
    /// formats are static per glyph_id).
    #[must_use]
    pub const fn needs_axis_in_key(self) -> bool {
        matches!(self, Self::VariableMono)
    }
}

// ============================================================================
// VariableAxis — OpenType named axes + custom tags
// ============================================================================

/// OpenType named variation axes per the registered-tags
/// table. The bead's "Recursive" font exercises Weight +
/// Slant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VariableAxis {
    /// `wght` — 1.0–1000.0 per OpenType spec.
    Weight,
    /// `wdth` — 1.0–1000.0; semantically a percentage of
    /// normal width (100 = normal, 50 = condensed, 200 =
    /// extended).
    Width,
    /// `slnt` — −90.0..=90.0 degrees.
    Slant,
    /// `opsz` — 6.0..=72.0 typical (point-size hint).
    OpticalSize,
    /// `ital` — 0.0..=1.0 (binary axis).
    Italic,
    /// Any other 4-byte tag the font registers.
    Custom { tag: [u8; 4] },
}

impl VariableAxis {
    /// 4-byte OpenType tag for telemetry / cache-key.
    #[must_use]
    pub const fn tag(self) -> [u8; 4] {
        match self {
            Self::Weight => *b"wght",
            Self::Width => *b"wdth",
            Self::Slant => *b"slnt",
            Self::OpticalSize => *b"opsz",
            Self::Italic => *b"ital",
            Self::Custom { tag } => tag,
        }
    }

    /// Per-axis valid range per the OpenType spec.
    #[must_use]
    pub const fn valid_range(self) -> (f32, f32) {
        match self {
            Self::Weight | Self::Width => (1.0, 1000.0),
            Self::Slant => (-90.0, 90.0),
            Self::OpticalSize => (1.0, 4096.0),
            Self::Italic => (0.0, 1.0),
            // Custom axes have no spec-imposed range; substrate
            // accepts any finite f32 and the integration
            // validates against the font's `fvar` table.
            Self::Custom { .. } => (f32::MIN, f32::MAX),
        }
    }

    /// Whether `value` falls within this axis's spec-defined
    /// range. NaN always returns false.
    #[must_use]
    pub fn is_valid_value(self, value: f32) -> bool {
        if value.is_nan() {
            return false;
        }
        let (min, max) = self.valid_range();
        value >= min && value <= max
    }
}

// ============================================================================
// AxisValue + AxisVector
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisValue {
    /// **Private** per ft-jit6o: previously public, allowing
    /// callers to construct `AxisValue { axis, value: f32::NAN }`
    /// directly and bypass [`AxisValue::new`]'s validation. NaN
    /// then quantised to 0, colliding with legitimate
    /// `value = 0.0` atlas keys → wrong-glyph rendering.
    axis: VariableAxis,
    value: f32,
}

impl AxisValue {
    /// Construct an `AxisValue`, returning `None` when the
    /// value is out of spec range or NaN.
    #[must_use]
    pub fn new(axis: VariableAxis, value: f32) -> Option<Self> {
        if axis.is_valid_value(value) {
            Some(Self { axis, value })
        } else {
            None
        }
    }

    /// Read the axis tag.
    #[must_use]
    pub fn axis(self) -> VariableAxis {
        self.axis
    }

    /// Read the axis value. Always finite + within
    /// `axis().valid_range()` — guaranteed by `new()`.
    #[must_use]
    pub fn value(self) -> f32 {
        self.value
    }

    /// Encode the value to a stable u32 for hashing. Substrate
    /// quantises to integer-millis (×1000) so atlas-key
    /// stability survives f32 rounding noise across runs.
    /// Range-limited to i32 then bit-cast to u32.
    ///
    /// Safe under all internally-constructible `AxisValue`s
    /// per ft-jit6o (NaN/INF cannot enter via `new()`).
    #[must_use]
    pub fn quantised_u32(self) -> u32 {
        let scaled = (self.value * 1000.0).round();
        let clamped = scaled.clamp(i32::MIN as f32, i32::MAX as f32) as i32;
        clamped as u32
    }
}

/// Sorted axis vector. Substrate enforces canonical ordering
/// (by `(axis-tag, value)`) so two semantically-identical
/// vectors hash identically regardless of input order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AxisVector {
    entries: Vec<AxisValue>,
}

impl AxisVector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, value: AxisValue) {
        // Replace any existing entry for the same axis (last
        // write wins).
        if let Some(pos) = self.entries.iter().position(|e| e.axis == value.axis) {
            self.entries[pos] = value;
        } else {
            self.entries.push(value);
        }
        // Re-sort by tag for canonical order.
        self.entries.sort_by(|a, b| a.axis.tag().cmp(&b.axis.tag()));
    }

    #[must_use]
    pub fn entries(&self) -> &[AxisValue] {
        &self.entries
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn get(&self, axis: VariableAxis) -> Option<f32> {
        self.entries
            .iter()
            .find(|e| e.axis == axis)
            .map(|e| e.value)
    }
}

// ============================================================================
// Atlas-key derivation
// ============================================================================

const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x100_0000_01b3;

fn fnv_fold_byte(hash: u64, byte: u8) -> u64 {
    let h = hash ^ u64::from(byte);
    h.wrapping_mul(FNV_PRIME_64)
}

fn fnv_fold_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash = fnv_fold_byte(hash, *b);
    }
    hash
}

/// Derive a stable atlas-cache key for a glyph rendered at
/// the given axis vector. Caller's own atlas key prefix can
/// be combined with this; substrate emits the axis-aware
/// component.
///
/// Inputs:
/// - `font_id`: the integration's font-table identifier.
/// - `glyph_id`: OpenType glyph index.
/// - `format`: the bead's GlyphFormat.
/// - `axis_vector`: variable-axis values applied at rasterisation.
///
/// For non-variable formats `axis_vector` is empty; substrate
/// still includes the format byte so a colour glyph and a
/// monochrome glyph at the same `(font_id, glyph_id)` cache
/// separately.
#[must_use]
pub fn derive_axis_atlas_key(
    font_id: u64,
    glyph_id: u32,
    format: GlyphFormat,
    axis_vector: &AxisVector,
) -> u64 {
    let mut hash = FNV_OFFSET_BASIS_64;
    hash = fnv_fold_bytes(hash, &font_id.to_le_bytes());
    hash = fnv_fold_bytes(hash, &glyph_id.to_le_bytes());
    hash = fnv_fold_byte(hash, format as u8);
    for entry in axis_vector.entries() {
        hash = fnv_fold_bytes(hash, &entry.axis.tag());
        hash = fnv_fold_bytes(hash, &entry.quantised_u32().to_le_bytes());
    }
    hash
}

// ============================================================================
// A11Y reduce-motion gate (cross-link BR-TERM-EMULATOR-UPLIFT.A11Y.5)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AxisTransitionPolicy {
    /// Animate transitions normally. Bead's default for
    /// regular users.
    #[default]
    Animate,
    /// Snap to the new axis values immediately (no animation).
    /// Bead's A11Y.5 rule: when prefers-reduced-motion=ON.
    Snap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisTransitionDecision {
    Animate,
    Snap,
}

/// Pure decision: should the integration animate the axis
/// transition or snap? Composes operator policy + OS
/// reduce-motion signal.
#[must_use]
pub const fn should_animate_axis_transition(
    policy: AxisTransitionPolicy,
    reduce_motion_on: bool,
) -> AxisTransitionDecision {
    match policy {
        // Operator forced snap (or reduce-motion on).
        AxisTransitionPolicy::Snap => AxisTransitionDecision::Snap,
        AxisTransitionPolicy::Animate => {
            if reduce_motion_on {
                AxisTransitionDecision::Snap
            } else {
                AxisTransitionDecision::Animate
            }
        }
    }
}

// ============================================================================
// A11Y emoji-name lookup
// ============================================================================

/// Where the announceable emoji name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmojiNameSource {
    /// Looked up via the CLDR short-name table (most precise).
    Cldr,
    /// Substrate fallback — Unicode block name (e.g.,
    /// "miscellaneous symbol" for unrecognised emoji).
    UnicodeBlock,
    /// No name found.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiName {
    pub codepoint: u32,
    pub name: String,
    pub source: EmojiNameSource,
}

/// Substrate's stub: returns `None` for any codepoint. The
/// integration's CLDR-name DB plugs in here. Substrate
/// exposes the schema so the integration can build to it.
#[must_use]
pub fn lookup_emoji_name(_codepoint: u32) -> Option<EmojiName> {
    None
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FontFeaturesTelemetry {
    /// Per-format render counts.
    pub renders_monochrome: u64,
    pub renders_variable_mono: u64,
    pub renders_colr_cpal: u64,
    pub renders_sbix: u64,
    pub renders_cbdt_eblc: u64,
    /// Atlas-key cache hits / misses.
    pub atlas_cache_hits: u64,
    pub atlas_cache_misses: u64,
    /// Variable-axis transitions started (animated +
    /// snapped).
    pub axis_transitions_animated: u64,
    pub axis_transitions_snapped: u64,
    /// A11Y emoji-name announcements emitted.
    pub a11y_emoji_names_emitted: u64,
    /// Codepoints whose emoji-name lookup returned None.
    pub a11y_emoji_names_missing: u64,
    /// Out-of-range axis values rejected.
    pub axis_value_rejections: u64,
}

impl FontFeaturesTelemetry {
    pub fn record_render(&mut self, format: GlyphFormat) {
        let slot = match format {
            GlyphFormat::Monochrome => &mut self.renders_monochrome,
            GlyphFormat::VariableMono => &mut self.renders_variable_mono,
            GlyphFormat::ColrCpal => &mut self.renders_colr_cpal,
            GlyphFormat::Sbix => &mut self.renders_sbix,
            GlyphFormat::CbdtEblc => &mut self.renders_cbdt_eblc,
        };
        *slot = slot.saturating_add(1);
    }

    pub fn record_cache(&mut self, hit: bool) {
        if hit {
            self.atlas_cache_hits = self.atlas_cache_hits.saturating_add(1);
        } else {
            self.atlas_cache_misses = self.atlas_cache_misses.saturating_add(1);
        }
    }

    pub fn record_transition(&mut self, decision: AxisTransitionDecision) {
        match decision {
            AxisTransitionDecision::Animate => {
                self.axis_transitions_animated = self.axis_transitions_animated.saturating_add(1);
            }
            AxisTransitionDecision::Snap => {
                self.axis_transitions_snapped = self.axis_transitions_snapped.saturating_add(1);
            }
        }
    }

    pub fn record_emoji_lookup(&mut self, found: bool) {
        if found {
            self.a11y_emoji_names_emitted = self.a11y_emoji_names_emitted.saturating_add(1);
        } else {
            self.a11y_emoji_names_missing = self.a11y_emoji_names_missing.saturating_add(1);
        }
    }

    pub fn record_axis_value_rejection(&mut self) {
        self.axis_value_rejections = self.axis_value_rejections.saturating_add(1);
    }

    /// Cache hit-rate as integer percent `[0..=100]`. 0 when
    /// no lookups recorded.
    #[must_use]
    pub fn cache_hit_rate_pct(&self) -> u32 {
        let total = self.atlas_cache_hits + self.atlas_cache_misses;
        if total == 0 {
            return 0;
        }
        ((self.atlas_cache_hits * 100) / total).min(100) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // GlyphFormat
    // ----------------------------------------------------------------

    #[test]
    fn format_default_is_monochrome() {
        assert_eq!(GlyphFormat::default(), GlyphFormat::Monochrome);
    }

    #[test]
    fn format_label_stable() {
        assert_eq!(GlyphFormat::Monochrome.label(), "monochrome");
        assert_eq!(GlyphFormat::VariableMono.label(), "variable_mono");
        assert_eq!(GlyphFormat::ColrCpal.label(), "colr_cpal");
        assert_eq!(GlyphFormat::Sbix.label(), "sbix");
        assert_eq!(GlyphFormat::CbdtEblc.label(), "cbdt_eblc");
    }

    #[test]
    fn format_color_classification() {
        assert!(!GlyphFormat::Monochrome.is_color());
        assert!(!GlyphFormat::VariableMono.is_color());
        assert!(GlyphFormat::ColrCpal.is_color());
        assert!(GlyphFormat::Sbix.is_color());
        assert!(GlyphFormat::CbdtEblc.is_color());
    }

    #[test]
    fn format_needs_axis_in_key_only_for_variable() {
        assert!(!GlyphFormat::Monochrome.needs_axis_in_key());
        assert!(GlyphFormat::VariableMono.needs_axis_in_key());
        assert!(!GlyphFormat::ColrCpal.needs_axis_in_key());
        assert!(!GlyphFormat::Sbix.needs_axis_in_key());
    }

    // ----------------------------------------------------------------
    // VariableAxis
    // ----------------------------------------------------------------

    #[test]
    fn axis_tag_matches_opentype_registered() {
        assert_eq!(&VariableAxis::Weight.tag(), b"wght");
        assert_eq!(&VariableAxis::Width.tag(), b"wdth");
        assert_eq!(&VariableAxis::Slant.tag(), b"slnt");
        assert_eq!(&VariableAxis::OpticalSize.tag(), b"opsz");
        assert_eq!(&VariableAxis::Italic.tag(), b"ital");
    }

    #[test]
    fn axis_custom_tag_passes_through() {
        let custom = VariableAxis::Custom { tag: *b"GRAD" };
        assert_eq!(&custom.tag(), b"GRAD");
    }

    #[test]
    fn axis_weight_range_matches_opentype() {
        let (min, max) = VariableAxis::Weight.valid_range();
        assert_eq!(min, 1.0);
        assert_eq!(max, 1000.0);
    }

    #[test]
    fn axis_slant_range_matches_opentype() {
        let (min, max) = VariableAxis::Slant.valid_range();
        assert_eq!(min, -90.0);
        assert_eq!(max, 90.0);
    }

    #[test]
    fn axis_is_valid_value_within_range() {
        assert!(VariableAxis::Weight.is_valid_value(400.0));
        assert!(VariableAxis::Weight.is_valid_value(1.0));
        assert!(VariableAxis::Weight.is_valid_value(1000.0));
    }

    #[test]
    fn axis_is_valid_value_out_of_range() {
        assert!(!VariableAxis::Weight.is_valid_value(0.0));
        assert!(!VariableAxis::Weight.is_valid_value(1001.0));
        assert!(!VariableAxis::Slant.is_valid_value(91.0));
        assert!(!VariableAxis::Slant.is_valid_value(-91.0));
    }

    #[test]
    fn axis_nan_value_invalid() {
        assert!(!VariableAxis::Weight.is_valid_value(f32::NAN));
        assert!(!VariableAxis::Slant.is_valid_value(f32::NAN));
    }

    #[test]
    fn axis_italic_binary_range() {
        assert!(VariableAxis::Italic.is_valid_value(0.0));
        assert!(VariableAxis::Italic.is_valid_value(0.5));
        assert!(VariableAxis::Italic.is_valid_value(1.0));
        assert!(!VariableAxis::Italic.is_valid_value(1.1));
    }

    // ----------------------------------------------------------------
    // AxisValue
    // ----------------------------------------------------------------

    #[test]
    fn axis_value_construction_validates() {
        assert!(AxisValue::new(VariableAxis::Weight, 400.0).is_some());
        assert!(AxisValue::new(VariableAxis::Weight, 0.0).is_none());
        assert!(AxisValue::new(VariableAxis::Weight, f32::NAN).is_none());
    }

    #[test]
    fn axis_value_quantised_stable_across_rounding() {
        let a = AxisValue::new(VariableAxis::Weight, 400.0).unwrap();
        let b = AxisValue::new(VariableAxis::Weight, 400.0001).unwrap();
        // 400.0 * 1000 = 400_000; 400.0001 * 1000 ≈ 400_000.1 → rounds to 400_000.
        // Both quantise to the same u32, so atlas keys match across f32 noise.
        assert_eq!(a.quantised_u32(), b.quantised_u32());
    }

    #[test]
    fn axis_value_quantised_distinguishes_distinct() {
        let a = AxisValue::new(VariableAxis::Weight, 400.0).unwrap();
        let b = AxisValue::new(VariableAxis::Weight, 700.0).unwrap();
        assert_ne!(a.quantised_u32(), b.quantised_u32());
    }

    // ----------------------------------------------------------------
    // AxisVector
    // ----------------------------------------------------------------

    #[test]
    fn vector_canonical_order_independent_of_insertion() {
        let mut v1 = AxisVector::new();
        v1.push(AxisValue::new(VariableAxis::Slant, -10.0).unwrap());
        v1.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());

        let mut v2 = AxisVector::new();
        v2.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());
        v2.push(AxisValue::new(VariableAxis::Slant, -10.0).unwrap());

        // Same entries, different insertion order → same vector.
        assert_eq!(v1.entries().len(), 2);
        assert_eq!(v2.entries().len(), 2);
        assert_eq!(v1.entries()[0].axis.tag(), v2.entries()[0].axis.tag());
        assert_eq!(v1.entries()[1].axis.tag(), v2.entries()[1].axis.tag());
    }

    #[test]
    fn vector_push_dedupes_by_axis() {
        let mut v = AxisVector::new();
        v.push(AxisValue::new(VariableAxis::Weight, 400.0).unwrap());
        v.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap()); // overwrite
        assert_eq!(v.len(), 1);
        assert_eq!(v.get(VariableAxis::Weight), Some(700.0));
    }

    #[test]
    fn vector_get_returns_none_for_missing_axis() {
        let v = AxisVector::new();
        assert_eq!(v.get(VariableAxis::Weight), None);
    }

    // ----------------------------------------------------------------
    // derive_axis_atlas_key
    // ----------------------------------------------------------------

    #[test]
    fn key_stable_across_runs_for_same_inputs() {
        let v = AxisVector::new();
        let k1 = derive_axis_atlas_key(42, 100, GlyphFormat::Monochrome, &v);
        let k2 = derive_axis_atlas_key(42, 100, GlyphFormat::Monochrome, &v);
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_changes_with_glyph_id() {
        let v = AxisVector::new();
        let k1 = derive_axis_atlas_key(42, 100, GlyphFormat::Monochrome, &v);
        let k2 = derive_axis_atlas_key(42, 101, GlyphFormat::Monochrome, &v);
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_changes_with_format() {
        let v = AxisVector::new();
        let k_mono = derive_axis_atlas_key(42, 100, GlyphFormat::Monochrome, &v);
        let k_color = derive_axis_atlas_key(42, 100, GlyphFormat::ColrCpal, &v);
        assert_ne!(k_mono, k_color);
    }

    #[test]
    fn key_changes_with_axis_value() {
        let mut v_400 = AxisVector::new();
        v_400.push(AxisValue::new(VariableAxis::Weight, 400.0).unwrap());
        let mut v_700 = AxisVector::new();
        v_700.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());

        let k_400 = derive_axis_atlas_key(42, 100, GlyphFormat::VariableMono, &v_400);
        let k_700 = derive_axis_atlas_key(42, 100, GlyphFormat::VariableMono, &v_700);
        assert_ne!(k_400, k_700);
    }

    #[test]
    fn key_canonical_order_independence() {
        // Two AxisVectors with same entries in different
        // insertion orders must hash identically.
        let mut v1 = AxisVector::new();
        v1.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());
        v1.push(AxisValue::new(VariableAxis::Slant, -10.0).unwrap());
        let mut v2 = AxisVector::new();
        v2.push(AxisValue::new(VariableAxis::Slant, -10.0).unwrap());
        v2.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());

        let k1 = derive_axis_atlas_key(42, 100, GlyphFormat::VariableMono, &v1);
        let k2 = derive_axis_atlas_key(42, 100, GlyphFormat::VariableMono, &v2);
        assert_eq!(k1, k2);
    }

    // ----------------------------------------------------------------
    // should_animate_axis_transition (A11Y.5 gate)
    // ----------------------------------------------------------------

    #[test]
    fn animate_when_no_reduce_motion() {
        let d = should_animate_axis_transition(AxisTransitionPolicy::Animate, false);
        assert_eq!(d, AxisTransitionDecision::Animate);
    }

    #[test]
    fn snap_when_reduce_motion_on() {
        let d = should_animate_axis_transition(AxisTransitionPolicy::Animate, true);
        assert_eq!(d, AxisTransitionDecision::Snap);
    }

    #[test]
    fn snap_when_operator_forced_snap_regardless_of_motion() {
        let d_off = should_animate_axis_transition(AxisTransitionPolicy::Snap, false);
        let d_on = should_animate_axis_transition(AxisTransitionPolicy::Snap, true);
        assert_eq!(d_off, AxisTransitionDecision::Snap);
        assert_eq!(d_on, AxisTransitionDecision::Snap);
    }

    // ----------------------------------------------------------------
    // EmojiName / lookup_emoji_name
    // ----------------------------------------------------------------

    #[test]
    fn emoji_lookup_substrate_returns_none() {
        // Substrate is a stub — integration plugs in real CLDR DB.
        assert!(lookup_emoji_name(0x1F600).is_none());
    }

    // ----------------------------------------------------------------
    // FontFeaturesTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = FontFeaturesTelemetry::default();
        assert_eq!(t.renders_monochrome, 0);
        assert_eq!(t.cache_hit_rate_pct(), 0);
    }

    #[test]
    fn telemetry_record_render_routes() {
        let mut t = FontFeaturesTelemetry::default();
        t.record_render(GlyphFormat::Monochrome);
        t.record_render(GlyphFormat::VariableMono);
        t.record_render(GlyphFormat::ColrCpal);
        t.record_render(GlyphFormat::Sbix);
        t.record_render(GlyphFormat::CbdtEblc);
        assert_eq!(t.renders_monochrome, 1);
        assert_eq!(t.renders_variable_mono, 1);
        assert_eq!(t.renders_colr_cpal, 1);
        assert_eq!(t.renders_sbix, 1);
        assert_eq!(t.renders_cbdt_eblc, 1);
    }

    #[test]
    fn telemetry_record_cache_routes_and_rate() {
        let mut t = FontFeaturesTelemetry::default();
        for _ in 0..7 {
            t.record_cache(true);
        }
        for _ in 0..3 {
            t.record_cache(false);
        }
        assert_eq!(t.atlas_cache_hits, 7);
        assert_eq!(t.atlas_cache_misses, 3);
        assert_eq!(t.cache_hit_rate_pct(), 70);
    }

    #[test]
    fn telemetry_record_transition_routes() {
        let mut t = FontFeaturesTelemetry::default();
        t.record_transition(AxisTransitionDecision::Animate);
        t.record_transition(AxisTransitionDecision::Snap);
        t.record_transition(AxisTransitionDecision::Snap);
        assert_eq!(t.axis_transitions_animated, 1);
        assert_eq!(t.axis_transitions_snapped, 2);
    }

    #[test]
    fn telemetry_emoji_lookup_routes_found_vs_missing() {
        let mut t = FontFeaturesTelemetry::default();
        t.record_emoji_lookup(true);
        t.record_emoji_lookup(false);
        t.record_emoji_lookup(false);
        assert_eq!(t.a11y_emoji_names_emitted, 1);
        assert_eq!(t.a11y_emoji_names_missing, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_recursive_font_weight_transition_atlas_keys_distinct() {
        // Bead's "Recursive variable monospace" canonical
        // case: weight=400 → weight=700 transition. Atlas-key
        // changes so the new render doesn't collide with the
        // cached one.
        let font_id = 0xFEEDF00D;
        let glyph_id = 65; // 'A'
        let mut v_normal = AxisVector::new();
        v_normal.push(AxisValue::new(VariableAxis::Weight, 400.0).unwrap());
        let mut v_bold = AxisVector::new();
        v_bold.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());

        let key_normal =
            derive_axis_atlas_key(font_id, glyph_id, GlyphFormat::VariableMono, &v_normal);
        let key_bold = derive_axis_atlas_key(font_id, glyph_id, GlyphFormat::VariableMono, &v_bold);

        assert_ne!(key_normal, key_bold);
    }

    #[test]
    fn scenario_noto_color_emoji_rendered_in_color() {
        // Noto Color Emoji = COLR/CPAL. is_color() flag drives
        // the integration's RGBA storage path.
        assert!(GlyphFormat::ColrCpal.is_color());
        assert!(!GlyphFormat::ColrCpal.needs_axis_in_key());
    }

    #[test]
    fn scenario_a11y_user_with_reduce_motion_snaps_transitions() {
        // Bead's A11Y.5: reduce-motion=ON → snap. Axis value
        // changes still apply but without the transition
        // animation.
        let d = should_animate_axis_transition(AxisTransitionPolicy::Animate, true);
        assert_eq!(d, AxisTransitionDecision::Snap);
    }

    #[test]
    fn scenario_invalid_axis_rejected() {
        // Bead's "axis values within fvar bounds" rule:
        // out-of-range and NaN both refused.
        let mut t = FontFeaturesTelemetry::default();
        let bad = AxisValue::new(VariableAxis::Weight, 1500.0);
        if bad.is_none() {
            t.record_axis_value_rejection();
        }
        assert_eq!(t.axis_value_rejections, 1);
    }

    #[test]
    fn scenario_axis_quantisation_collapses_float_noise() {
        // Bead's atlas-key stability across renders: the same
        // weight=400 should hash identically even with f32
        // rounding noise from interpolation.
        let mut v1 = AxisVector::new();
        v1.push(AxisValue::new(VariableAxis::Weight, 400.0).unwrap());
        let mut v2 = AxisVector::new();
        v2.push(AxisValue::new(VariableAxis::Weight, 400.00001).unwrap());

        let k1 = derive_axis_atlas_key(1, 1, GlyphFormat::VariableMono, &v1);
        let k2 = derive_axis_atlas_key(1, 1, GlyphFormat::VariableMono, &v2);
        assert_eq!(k1, k2);
    }

    #[test]
    fn scenario_apple_color_emoji_uses_sbix_path() {
        // Bead's test corpus: Apple Color Emoji = sbix.
        // Substrate's atlas key encodes format byte so
        // Apple's Skull (sbix) doesn't collide with Noto's
        // Skull (CBDT) at the same codepoint.
        let v = AxisVector::new();
        let k_sbix = derive_axis_atlas_key(1, 0x1F480, GlyphFormat::Sbix, &v);
        let k_cbdt = derive_axis_atlas_key(1, 0x1F480, GlyphFormat::CbdtEblc, &v);
        assert_ne!(k_sbix, k_cbdt);
    }

    /// ft-jit6o regression guard: AxisValue fields are private,
    /// so callers cannot construct an AxisValue with NaN/INF
    /// outside [`AxisValue::new`]'s validation gate.
    #[test]
    fn axis_value_new_rejects_nan_and_infinity() {
        // NaN.
        assert!(AxisValue::new(VariableAxis::Weight, f32::NAN).is_none());
        assert!(AxisValue::new(VariableAxis::Slant, f32::NAN).is_none());
        // Infinity (out of spec range for non-Custom axes).
        assert!(AxisValue::new(VariableAxis::Weight, f32::INFINITY).is_none());
        assert!(AxisValue::new(VariableAxis::Weight, f32::NEG_INFINITY).is_none());
        // In-range value passes.
        assert!(AxisValue::new(VariableAxis::Weight, 400.0).is_some());
    }

    #[test]
    fn axis_value_quantised_u32_distinguishes_zero_from_other_values() {
        // ft-jit6o: previously a NaN-constructed AxisValue would
        // quantise to 0, COLLIDING with a legitimate
        // value=0.0 atlas key. With private fields, such an
        // AxisValue can only come from new() which rejects NaN.
        // Pin the post-fix invariant: 0.0's quantised key is
        // distinct from every non-zero in-range value.
        let zero = AxisValue::new(VariableAxis::Slant, 0.0).unwrap();
        let zero_key = zero.quantised_u32();
        for sample in [-90.0_f32, -45.0, -1.0, 0.001, 1.0, 45.0, 90.0] {
            if sample == 0.0 {
                continue;
            }
            let av = AxisValue::new(VariableAxis::Slant, sample).unwrap();
            assert_ne!(
                av.quantised_u32(),
                zero_key,
                "value {sample} must not collide with 0.0"
            );
        }
    }

    #[test]
    fn axis_value_field_access_via_getters_only() {
        // Compile-time check that fields are private — getter
        // path is the only access. (If fields were public again,
        // this test compiles trivially; the regression-guard
        // value is in the assertions below + the new(NaN)
        // rejection above which prevents the bypass.)
        let av = AxisValue::new(VariableAxis::Weight, 700.0).unwrap();
        assert_eq!(av.axis(), VariableAxis::Weight);
        assert!((av.value() - 700.0).abs() < f32::EPSILON);
    }
}
