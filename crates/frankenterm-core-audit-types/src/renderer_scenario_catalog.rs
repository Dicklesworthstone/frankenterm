//! Versioned renderer resize, zoom, and visual-state scenario contract.
//!
//! This module describes deterministic workloads. It does not execute a GUI,
//! retain measurements, or authorize performance, visual-quality, native-host,
//! or accessibility proof claims. Version 1 is deliberately
//! [`RendererCatalogAuthority::ContractOnly`].
//!
//! Corpus entries are references to canonical repository fixtures and oracles;
//! the contract contains no terminal text, image bytes, comparator thresholds,
//! or copied renderer fixtures. Repository existence is an integration-test
//! concern. This leaf module validates the reference syntax without file I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

mod renderer_seed_wire {
    use serde::{Deserialize, Deserializer, Serializer};

    // Serde's field-adapter contract requires the value as `&T`.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(seed: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&format_args!("0x{seed:016x}"))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = encoded.as_bytes();
        if bytes.len() != 18
            || &bytes[..2] != b"0x"
            || !bytes[2..]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(serde::de::Error::custom(
                "renderer seed must be exactly `0x` plus 16 lowercase hexadecimal digits",
            ));
        }
        u64::from_str_radix(&encoded[2..], 16).map_err(serde::de::Error::custom)
    }
}

/// Contract identifier accepted by schema version 1.
pub const RENDERER_SCENARIO_CONTRACT_ID: &str = "ft.renderer_scenario_catalog.v1";

/// Schema version implemented by this module.
pub const RENDERER_SCENARIO_SCHEMA_VERSION: u32 = 1;

/// Exact canonical catalog revision implemented by this module.
pub const RENDERER_SCENARIO_CATALOG_REVISION: u32 = 2;

/// Bead that owns the version-1 contract.
pub const RENDERER_SCENARIO_SOURCE_BEAD_ID: &str =
    "ft-interactive-systems-performance-4tenz.3.1";

/// Maximum raw JSON document accepted by the bounded decoder.
pub const MAX_RENDERER_SCENARIO_CATALOG_BYTES: usize = 8 * 1024 * 1024;

/// Exact decimal-byte rate used by the output-overlap resize gesture.
pub const OUTPUT_OVERLAP_BYTES_PER_SECOND: u64 = 1_000_000;

/// External policy reference required for RQ-S11 checkpoints.
pub const RQ_S11_COMPARATOR_POLICY_REF: &str =
    "docs/perf/resize-quality-slo.json#RQ-S11.snap_back_ssim";

/// External policy reference required for RQ-S13 checkpoints.
pub const RQ_S13_COMPARATOR_POLICY_REF: &str =
    "docs/perf/resize-quality-slo.json#RQ-S13.ssim_parity_oracle_corpus";

/// Bead that owns machine comparison of renderer accessibility geometry.
pub const RENDERER_ACCESSIBILITY_GEOMETRY_TRACKING_REF: &str =
    "ft-interactive-systems-performance-4tenz.3.5";

/// Bead that owns live NSAccessibility, VoiceOver, and human-review authority.
pub const NATIVE_ACCESSIBILITY_AUTHORITY_TRACKING_REF: &str =
    "ft-interactive-swarm-product-convergence-7xqz4.9.3";

/// Tracker for the stale Draft-versus-Standard RQ-S11 target wording.
pub const RQ_S11_CONTRADICTION_TRACKING_REF: &str =
    "ft-interactive-systems-performance-4tenz.3.5.2";

/// Tracker for non-independent changed-pixel-fraction comparator semantics.
pub const CHANGED_PIXEL_FRACTION_TRACKING_REF: &str =
    "ft-interactive-systems-performance-4tenz.3.5.1";

/// Tracking authority for deterministic output and foreground-key effect proof.
pub const RENDERER_OUTPUT_AUTHORITY_TRACKING_REF: &str =
    "ft-interactive-systems-performance-4tenz.3.1.2";

/// Exact downstream measurement vocabulary identity.
pub const RENDERER_MEASUREMENT_CONTRACT_ID: &str = "ft.renderer.measurement-contract.v1";

/// Number of required closed-domain gestures.
pub const REQUIRED_RENDERER_GESTURE_COUNT: usize = 8;

/// Number of exact fleet qualification points.
pub const REQUIRED_RENDERER_FLEET_POINT_COUNT: usize = 4;

/// Number of gesture-by-fleet coverage cells.
pub const REQUIRED_RENDERER_SCENARIO_COUNT: usize =
    REQUIRED_RENDERER_GESTURE_COUNT * REQUIRED_RENDERER_FLEET_POINT_COUNT;

/// Number of live gestures carrying Begin/Draft/SnapBack/Settle checkpoints.
pub const REQUIRED_RENDERER_LIVE_GESTURE_COUNT: usize = 5;

/// Number of steady gestures carrying Begin/Mutation/Settle checkpoints.
pub const REQUIRED_RENDERER_STEADY_GESTURE_COUNT: usize =
    REQUIRED_RENDERER_GESTURE_COUNT - REQUIRED_RENDERER_LIVE_GESTURE_COUNT;

/// Number of reusable coverage overlays applied to every base cell.
pub const REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT: usize = 8;

/// Exact checkpoint-to-manifest binding rows: 20 live cells and 12 steady cells.
pub const REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT: usize =
    REQUIRED_RENDERER_LIVE_GESTURE_COUNT
        * REQUIRED_RENDERER_FLEET_POINT_COUNT
        * REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT
        * 4
        + REQUIRED_RENDERER_STEADY_GESTURE_COUNT
            * REQUIRED_RENDERER_FLEET_POINT_COUNT
            * REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT
            * 3;

/// Number of terminal-content features required across active corpus entries.
pub const REQUIRED_RENDERER_TERMINAL_FEATURE_COUNT: usize = 13;

/// Closed invariant class inventory; scenario applicability is conditional.
pub const REQUIRED_RENDERER_INVARIANT_COUNT: usize = 8;

/// Canonical invariant identities, in deterministic contract order.
pub const REQUIRED_RENDERER_INVARIANT_IDS: [&str; REQUIRED_RENDERER_INVARIANT_COUNT] = [
    "no_blank_frame_after_nonblank",
    "no_stale_full_frame_reuse",
    "coherent_grid_terminal_revision",
    "anchors_in_bounds",
    "reflow_logical_line_identity",
    "alternate_screen_isolation",
    "accessibility_focus_geometry",
    "final_state_convergence",
];

/// Maximum timeline events in one scenario.
pub const MAX_RENDERER_TIMELINE_EVENTS: usize = 256;

/// Maximum distinct mutations in one atomic timeline event.
pub const MAX_RENDERER_ACTIONS_PER_EVENT: usize = 9;

/// Maximum expected invariants in one scenario.
pub const MAX_RENDERER_EXPECTED_INVARIANTS: usize = 128;

/// Maximum visual checkpoints in one scenario.
pub const MAX_RENDERER_CHECKPOINTS: usize = 64;

/// Maximum workload definitions in one catalog.
pub const MAX_RENDERER_WORKLOADS: usize = 64;

/// Maximum corpus-reference definitions in one catalog.
pub const MAX_RENDERER_CORPUS_REFERENCES: usize = 128;

/// Maximum declared output rate for a workload or timeline event.
pub const MAX_RENDERER_OUTPUT_BYTES_PER_SECOND: u64 = 10_000_000_000;

const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_REPOSITORY_REFERENCE_BYTES: usize = 512;
const MAX_REASON_BYTES: usize = 2048;
const MAX_GRID_DIMENSION: u16 = 4096;
const MAX_VIEWPORT_DIMENSION_PX: u32 = 131_072;
const MAX_FONT_SIZE_MILLI_POINTS: u32 = 512_000;
const MAX_DPI_MILLI: u32 = 4_000_000;
const MAX_SCALE_FACTOR_MILLI: u32 = 32_000;

/// Claim authority carried by a renderer scenario catalog.
///
/// Version 1 has no representation for a native pass or evidence pass. A raw
/// document attempting either fails closed during deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCatalogAuthority {
    /// Definitions only; no runtime or evidence authority.
    ContractOnly,
}

/// Closed resize, zoom, display, and output-overlap gesture domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererGesture {
    /// Change the native window size without changing terminal rows or columns.
    SameGridDrag,
    /// Change the native window size and terminal grid.
    GridChangingDrag,
    /// Reflow from exactly 80 columns to exactly 200 columns.
    #[serde(rename = "reflow_80_to_200")]
    Reflow80To200,
    /// Reflow from exactly 200 columns to exactly 80 columns.
    #[serde(rename = "reflow_200_to_80")]
    Reflow200To80,
    /// Increase logical font scale while retaining configured base size.
    ZoomIn,
    /// Decrease logical font scale while retaining configured base size.
    ZoomOut,
    /// Move to a different display/DPI configuration.
    DpiDisplayMove,
    /// Resize while deterministic output runs at exactly one decimal MB/s.
    OutputOverlapResize,
}

impl RendererGesture {
    /// Every gesture required by version 1, in canonical order.
    pub const ALL: [Self; REQUIRED_RENDERER_GESTURE_COUNT] = [
        Self::SameGridDrag,
        Self::GridChangingDrag,
        Self::Reflow80To200,
        Self::Reflow200To80,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::DpiDisplayMove,
        Self::OutputOverlapResize,
    ];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SameGridDrag => "same_grid_drag",
            Self::GridChangingDrag => "grid_changing_drag",
            Self::Reflow80To200 => "reflow_80_to_200",
            Self::Reflow200To80 => "reflow_200_to_80",
            Self::ZoomIn => "zoom_in",
            Self::ZoomOut => "zoom_out",
            Self::DpiDisplayMove => "dpi_display_move",
            Self::OutputOverlapResize => "output_overlap_resize",
        }
    }

    const fn seed_discriminant(self) -> u64 {
        match self {
            Self::SameGridDrag => 1,
            Self::GridChangingDrag => 2,
            Self::Reflow80To200 => 3,
            Self::Reflow200To80 => 4,
            Self::ZoomIn => 5,
            Self::ZoomOut => 6,
            Self::DpiDisplayMove => 7,
            Self::OutputOverlapResize => 8,
        }
    }
}

/// Exact pane-count qualification point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RendererFleetPoint {
    /// One pane.
    #[serde(rename = "p001")]
    P001,
    /// Twenty panes.
    #[serde(rename = "p020")]
    P020,
    /// Fifty panes.
    #[serde(rename = "p050")]
    P050,
    /// Two hundred panes.
    #[serde(rename = "p200")]
    P200,
}

impl RendererFleetPoint {
    /// Every fleet point required by version 1, in canonical order.
    pub const ALL: [Self; REQUIRED_RENDERER_FLEET_POINT_COUNT] =
        [Self::P001, Self::P020, Self::P050, Self::P200];

    /// Exact number of panes represented by this point.
    #[must_use]
    pub const fn pane_count(self) -> u16 {
        match self {
            Self::P001 => 1,
            Self::P020 => 20,
            Self::P050 => 50,
            Self::P200 => 200,
        }
    }

    /// Exact number of tabs represented by this point.
    #[must_use]
    pub const fn tab_count(self) -> u16 {
        match self {
            Self::P001 => 1,
            Self::P020 => 4,
            Self::P050 => 8,
            Self::P200 => 16,
        }
    }

    /// Exact number of windows represented by this point.
    #[must_use]
    pub const fn window_count(self) -> u16 {
        match self {
            Self::P001 | Self::P020 => 1,
            Self::P050 => 2,
            Self::P200 => 4,
        }
    }

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P001 => "p001",
            Self::P020 => "p020",
            Self::P050 => "p050",
            Self::P200 => "p200",
        }
    }
}

/// Terminal-content and user-visible state features required by the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererTerminalFeature {
    /// Basic ASCII text.
    Ascii,
    /// CJK wide-cell text.
    Cjk,
    /// Right-to-left scripts.
    Rtl,
    /// Combining-mark sequences.
    CombiningMarks,
    /// Emoji and fallback-font sequences.
    Emoji,
    /// Font ligature sequences.
    Ligatures,
    /// Inline image content.
    Images,
    /// Hyperlinked terminal cells.
    Hyperlinks,
    /// Alternate-screen state.
    AlternateScreen,
    /// Active selection state.
    Selection,
    /// Cursor shape, visibility, and blink state.
    Cursor,
    /// IME composition state.
    Ime,
    /// Accessibility tree and geometry state.
    AccessibilityGeometry,
}

impl RendererTerminalFeature {
    /// Exact terminal-feature set required across active corpus references.
    pub const ALL: [Self; REQUIRED_RENDERER_TERMINAL_FEATURE_COUNT] = [
        Self::Ascii,
        Self::Cjk,
        Self::Rtl,
        Self::CombiningMarks,
        Self::Emoji,
        Self::Ligatures,
        Self::Images,
        Self::Hyperlinks,
        Self::AlternateScreen,
        Self::Selection,
        Self::Cursor,
        Self::Ime,
        Self::AccessibilityGeometry,
    ];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Cjk => "cjk",
            Self::Rtl => "rtl",
            Self::CombiningMarks => "combining_marks",
            Self::Emoji => "emoji",
            Self::Ligatures => "ligatures",
            Self::Images => "images",
            Self::Hyperlinks => "hyperlinks",
            Self::AlternateScreen => "alternate_screen",
            Self::Selection => "selection",
            Self::Cursor => "cursor",
            Self::Ime => "ime",
            Self::AccessibilityGeometry => "accessibility_geometry",
        }
    }
}

/// Harness capability whose availability must be explicit for every scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCapability {
    /// Deterministic headless terminal-state oracle.
    HeadlessStateOracle,
    /// GPU visual capture without an implied comparator verdict.
    GpuVisualCapture,
    /// Native window drag/resize input.
    NativeWindowGesture,
    /// Native move between display/DPI configurations.
    NativeDisplayMove,
    /// IME composition input and state.
    ImeComposition,
    /// Accessibility tree and geometry observation.
    AccessibilityGeometry,
    /// Inline-image terminal protocol support.
    ImageProtocol,
    /// Production mux and domain rather than a mock adapter.
    ProductionMuxDomain,
    /// Real PTY stream rather than generated in-memory terminal state.
    RealPtyStream,
    /// Production TermWindow path.
    ProductionTermWindow,
    /// Production WebGPU/renderer backend.
    ProductionRendererBackend,
    /// Metal drawable capture boundary.
    MetalDrawableCapture,
    /// Observable software-present boundary.
    SoftwarePresentBoundary,
    /// Actual non-photon display-presentation feedback boundary.
    DisplayPresentationBoundary,
    /// Physical display/photon boundary, kept distinct and optional.
    DisplayPhotonBoundary,
    /// Native foreground-key injection.
    NativeKeyInjection,
    /// Native color-profile binding and observation.
    NativeColorProfile,
    /// HDR/EDR output capability when requested by display state.
    HdrEdrOutput,
    /// Explicit pinned shaping configuration with required ligature features enabled.
    EnabledLigatureShaping,
}

impl RendererCapability {
    /// Exact capability matrix required on every scenario.
    pub const ALL: [Self; 19] = [
        Self::HeadlessStateOracle,
        Self::GpuVisualCapture,
        Self::NativeWindowGesture,
        Self::NativeDisplayMove,
        Self::ImeComposition,
        Self::AccessibilityGeometry,
        Self::ImageProtocol,
        Self::ProductionMuxDomain,
        Self::RealPtyStream,
        Self::ProductionTermWindow,
        Self::ProductionRendererBackend,
        Self::MetalDrawableCapture,
        Self::SoftwarePresentBoundary,
        Self::DisplayPresentationBoundary,
        Self::DisplayPhotonBoundary,
        Self::NativeKeyInjection,
        Self::NativeColorProfile,
        Self::HdrEdrOutput,
        Self::EnabledLigatureShaping,
    ];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeadlessStateOracle => "headless_state_oracle",
            Self::GpuVisualCapture => "gpu_visual_capture",
            Self::NativeWindowGesture => "native_window_gesture",
            Self::NativeDisplayMove => "native_display_move",
            Self::ImeComposition => "ime_composition",
            Self::AccessibilityGeometry => "accessibility_geometry",
            Self::ImageProtocol => "image_protocol",
            Self::ProductionMuxDomain => "production_mux_domain",
            Self::RealPtyStream => "real_pty_stream",
            Self::ProductionTermWindow => "production_term_window",
            Self::ProductionRendererBackend => "production_renderer_backend",
            Self::MetalDrawableCapture => "metal_drawable_capture",
            Self::SoftwarePresentBoundary => "software_present_boundary",
            Self::DisplayPresentationBoundary => "display_presentation_boundary",
            Self::DisplayPhotonBoundary => "display_photon_boundary",
            Self::NativeKeyInjection => "native_key_injection",
            Self::NativeColorProfile => "native_color_profile",
            Self::HdrEdrOutput => "hdr_edr_output",
            Self::EnabledLigatureShaping => "enabled_ligature_shaping",
        }
    }
}

/// Whether a scenario semantically requires a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCapabilityRequirement {
    /// The scenario cannot exercise its declared semantics without it.
    Required,
    /// Useful additional authority, but unresolved availability does not block.
    Optional,
    /// The capability is represented in the matrix but is not required here.
    NotApplicable,
}

/// Explicit availability of one harness capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererCapabilityAvailability {
    /// The scenario substrate declares the capability available.
    ///
    /// This is contract metadata, not evidence that a live target exercised it.
    DeclaredAvailable,
    /// Some contract substrate exists, but the capability is incomplete.
    Partial {
        reason: String,
        tracking_ref: String,
    },
    /// No isolated target probe has established availability.
    UnknownNotProbed {
        reason: String,
        tracking_ref: String,
    },
    /// Availability depends on an exact target profile.
    TargetDependent {
        target_profile_ref: String,
        reason: String,
        tracking_ref: String,
    },
    /// The capability is unavailable for the declared scenario substrate.
    Unsupported {
        /// Non-empty explanation of the limitation.
        reason: String,
        /// Relative repository or Bead-style tracking reference.
        tracking_ref: String,
    },
}

/// One complete row in a scenario's capability matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererCapabilityBinding {
    /// Closed capability identity.
    pub capability: RendererCapability,
    /// Semantic requirement, independent of current substrate availability.
    pub requirement: RendererCapabilityRequirement,
    /// Explicit supported or unsupported-with-reason declaration.
    pub availability: RendererCapabilityAvailability,
}

/// Ordered phase of a deterministic gesture timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererTimelinePhase {
    /// Gesture boundary before any mutation.
    Begin,
    /// One deterministic state mutation.
    Mutation,
    /// Gesture input has ended.
    End,
    /// Exactly one post-gesture Standard-quality snap-back frame.
    SnapBack,
    /// Renderer and terminal state are expected to have settled.
    Settle,
}

impl RendererTimelinePhase {
    const fn rank(self) -> u8 {
        match self {
            Self::Begin => 0,
            Self::Mutation => 1,
            Self::End => 2,
            Self::SnapBack => 3,
            Self::Settle => 4,
        }
    }
}

/// Terminal grid dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererGridState {
    /// Terminal columns.
    pub columns: u16,
    /// Terminal rows.
    pub rows: u16,
}

/// Font identity and integer fixed-point size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererFontState {
    /// Stable repository/catalog font identity, not font bytes.
    pub font_id: String,
    /// Relative reference pinning the exact font identity/configuration.
    pub pinned_font_ref: String,
    /// Configured base point size multiplied by 1,000; zoom never mutates it.
    pub base_size_milli_points: u32,
    /// Renderer font scale multiplied by 1,000.
    pub scale_milli: u32,
    /// Base cell width in milli-pixels at base font size, scale 1.000, and reference DPI.
    pub base_cell_width_milli_px: u32,
    /// Base cell height in milli-pixels at base font size, scale 1.000, and reference DPI.
    pub base_cell_height_milli_px: u32,
    /// DPI in milli-DPI at which the base cell extents were pinned.
    pub metric_reference_dpi_milli: u32,
    /// Named revision of the effective-size/cell-metric derivation.
    pub metric_derivation_revision: String,
}

/// Authority class of a pinned renderer configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererConfigAuthority {
    /// The bundled FrankenTerm configuration used by production-default runs.
    BundledProductionDefault,
    /// A deliberately non-default configuration used to exercise ligatures.
    FeatureMaximalNonDefault,
}

/// Explicit OpenType ligature feature state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererLigatureFeatureState {
    pub calt_enabled: bool,
    pub clig_enabled: bool,
    pub liga_enabled: bool,
}

/// Availability of a pinned renderer configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererConfigurationAvailability {
    Available,
    Unavailable {
        reason: String,
        tracking_refs: Vec<String>,
    },
}

/// Same-catalog renderer-configuration profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererConfigProfile {
    pub renderer_config_profile_id: String,
    pub profile_revision: u32,
    pub repository_ref: String,
    pub authority: RendererConfigAuthority,
    pub ligature_features: RendererLigatureFeatureState,
    pub availability: RendererConfigurationAvailability,
}

/// Explicit output dynamic-range mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererDynamicRangeMode {
    /// Standard dynamic range.
    Sdr,
    /// High dynamic range.
    Hdr,
}

/// Display, DPI, scale, and viewport state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererDisplayState {
    /// Stable display identity within the scenario.
    pub display_id: String,
    /// Logical DPI multiplied by 1,000.
    pub dpi_milli: u32,
    /// Scale factor multiplied by 1,000.
    pub scale_factor_milli: u32,
    /// Stable color-space identity.
    pub color_space_id: String,
    /// Pinned display color-profile reference.
    pub color_profile_ref: String,
    /// SDR or HDR rendering mode.
    pub dynamic_range_mode: RendererDynamicRangeMode,
    /// Whether EDR is available on the declared display target.
    pub edr_available: bool,
    /// EDR headroom multiplied by 1,000; exactly 1,000 when unavailable.
    pub edr_headroom_milli: u32,
    /// Drawable viewport width.
    pub viewport_width_px: u32,
    /// Drawable viewport height.
    pub viewport_height_px: u32,
    /// Surface-local content padding before column zero.
    pub content_padding_left_px: u32,
    /// Surface-local content padding above row zero.
    pub content_padding_top_px: u32,
    /// Explicit residual padding after the derived grid width.
    pub content_padding_right_px: u32,
    /// Explicit residual padding after the derived grid height.
    pub content_padding_bottom_px: u32,
}

/// Display metadata changed by a native display move.
///
/// Drawable dimensions and surface-local padding are intentionally absent:
/// window size is changed by `SetWindowSize`, while pane viewport and padding
/// are deterministically derived from the expanded split geometry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererDisplayTransition {
    pub display_id: String,
    /// Logical DPI, independent of physical panel DPI and backing scale.
    pub dpi_milli: u32,
    /// Backing-pixel scale applied exactly once after logical-DPI normalization.
    pub scale_factor_milli: u32,
    pub color_space_id: String,
    pub color_profile_ref: String,
    pub dynamic_range_mode: RendererDynamicRangeMode,
    pub edr_available: bool,
    pub edr_headroom_milli: u32,
}

/// Integer pixel rectangle used by accessibility geometry state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPixelRect {
    /// Horizontal origin.
    pub x: i32,
    /// Vertical origin.
    pub y: i32,
    /// Positive width.
    pub width: u32,
    /// Positive height.
    pub height: u32,
}

/// Coordinate space attached to geometry whose origin would otherwise be ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererPixelCoordinateSpace {
    /// Coordinates relative to a window's drawable client/tab-content origin.
    WindowDrawable,
    /// Signed coordinates in the virtual-display desktop space.
    VirtualDisplay,
}

/// IME candidate-window geometry is a native popup in virtual-display space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererImeCandidateWindowGeometry {
    pub coordinate_space: RendererPixelCoordinateSpace,
    pub rect: RendererPixelRect,
}

/// Zero-based terminal cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererCellCoordinate {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub column: u16,
}

/// Active selection granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererSelectionGranularity {
    /// Character-granularity selection.
    Character,
    /// Word-granularity selection.
    Word,
    /// Line-granularity selection.
    Line,
}

/// Explicit selection state and geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererSelectionState {
    /// No active selection.
    Inactive,
    /// Active selection with exact anchor/focus cell coordinates.
    Active {
        /// Selection granularity.
        granularity: RendererSelectionGranularity,
        /// Fixed selection anchor.
        anchor: RendererCellCoordinate,
        /// Moving selection focus.
        focus: RendererCellCoordinate,
    },
}

/// Closed cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCursorShape {
    /// Block cursor.
    Block,
    /// Beam cursor.
    Beam,
    /// Underline cursor.
    Underline,
}

/// Closed cursor blink phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCursorBlinkPhase {
    /// Cursor does not blink.
    Steady,
    /// Visible phase of a blinking cursor.
    On,
    /// Hidden phase of a blinking cursor.
    Off,
}

/// Explicit cursor state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererCursorState {
    /// Cursor shape.
    pub shape: RendererCursorShape,
    /// Whether the cursor is logically visible.
    pub visible: bool,
    /// Deterministic blink phase.
    pub blink_phase: RendererCursorBlinkPhase,
    /// Zero-based cursor row.
    pub row: u16,
    /// Zero-based cursor column.
    pub column: u16,
}

/// Explicit IME state without copied composition text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererImeState {
    /// No active composition.
    Inactive,
    /// Composition content comes from a canonical corpus identity.
    Composing {
        /// Corpus entry containing the composition fixture.
        preedit_content_corpus_id: String,
        /// Cell at which the preedit begins.
        preedit_origin: RendererCellCoordinate,
        /// Cell containing the IME caret.
        caret: RendererCellCoordinate,
        /// Stable platform input-source identity.
        input_source_id: String,
        /// Exact ordered composition segments/ranges.
        composition_segments: Vec<RendererImeCompositionSegment>,
        /// Optional native candidate-window geometry in explicit coordinate space.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        candidate_window_geometry: Option<RendererImeCandidateWindowGeometry>,
    },
}

/// One inline IME composition segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererImeCompositionSegment {
    pub segment_id: String,
    pub start: RendererCellCoordinate,
    pub end: RendererCellCoordinate,
    pub selected: bool,
}

/// One inline image's complete visible geometry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererInlineImageGeometry {
    pub image_id: String,
    pub cell_start: RendererCellCoordinate,
    pub cell_end: RendererCellCoordinate,
    pub pixel_rect: RendererPixelRect,
}

/// One hyperlink's complete visible terminal range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererHyperlinkGeometry {
    pub hyperlink_id: String,
    pub cell_start: RendererCellCoordinate,
    pub cell_end: RendererCellCoordinate,
    pub pixel_rect: RendererPixelRect,
}

/// One accessibility node's visible cell and pixel geometry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererAccessibilityNodeGeometry {
    pub node_id: String,
    pub role_id: String,
    pub cell_start: RendererCellCoordinate,
    pub cell_end: RendererCellCoordinate,
    pub pixel_rect: RendererPixelRect,
    pub focused: bool,
}

/// Accessibility tree and geometry state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererAccessibilityGeometryState {
    Inactive,
    Active {
        /// Deterministic tree revision.
        tree_revision: u64,
        /// Exact ordered visible node geometry/cell map.
        nodes: Vec<RendererAccessibilityNodeGeometry>,
        /// Expected caret geometry when present.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        caret_rect: Option<RendererPixelRect>,
    },
}

/// Primary or alternate terminal buffer identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererTerminalBufferKind {
    Primary,
    Alternate,
}

/// Exact terminal-buffer identity needed to verify alternate-screen isolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererTerminalBufferState {
    pub buffer_id: String,
    pub revision: u64,
    /// Ordered materialized-content identities visible in this buffer.
    pub content_corpus_ids: Vec<String>,
    pub scrollback_lines: u32,
}

/// Complete terminal-mode state visible during a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererTerminalModeState {
    /// Stable-row viewport origin; zero is the live bottom and history is negative.
    pub viewport_top: i64,
    /// Exact active buffer; a boolean cannot establish buffer isolation.
    pub active_buffer: RendererTerminalBufferKind,
    /// Stable primary-buffer identity, revision, content, and scrollback.
    pub primary_buffer: RendererTerminalBufferState,
    /// Stable alternate-buffer identity, revision, content, and scrollback.
    pub alternate_buffer: RendererTerminalBufferState,
    /// Explicit selection state.
    pub selection: RendererSelectionState,
    /// Explicit cursor state.
    pub cursor: RendererCursorState,
    /// Explicit IME state.
    pub ime: RendererImeState,
    /// Exact inline image identities and geometry.
    pub inline_images: Vec<RendererInlineImageGeometry>,
    /// Exact hyperlink identities and visible ranges.
    pub hyperlinks: Vec<RendererHyperlinkGeometry>,
    /// Accessibility tree and geometry state.
    pub accessibility_geometry: RendererAccessibilityGeometryState,
}

/// Fully explicit focused-pane renderer state.
///
/// Same-catalog normalized phase manifests expand this template across exact
/// per-window/tab/pane state; one template is not an entire fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererSurfaceState {
    /// Monotonic renderer generation for the focused pane.
    pub renderer_generation: u64,
    /// Monotonic grid generation for the focused pane.
    pub grid_revision: u64,
    /// Monotonic terminal-state generation for the focused pane.
    pub terminal_revision: u64,
    /// Same-catalog pinned renderer-configuration identity.
    pub renderer_config_profile_id: String,
    /// Explicit renderer quality mode for Draft-to-Standard transitions.
    pub quality_mode: RendererQualityMode,
    /// Terminal grid state.
    pub grid: RendererGridState,
    /// Font state.
    pub font: RendererFontState,
    /// Display, DPI, scale, and viewport state.
    pub display: RendererDisplayState,
    /// Terminal-mode and accessibility state.
    pub terminal: RendererTerminalModeState,
}

/// Grid behavior chosen for an output-overlap resize scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererResizeMode {
    /// Resize viewport pixels while preserving rows and columns.
    SameGrid,
    /// Resize viewport pixels while changing rows and/or columns.
    GridChanging,
}

/// Renderer quality mode used by live-resize snap-back scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererQualityMode {
    /// Reduced-cost live-resize mode.
    Draft,
    /// Ground-truth steady-state mode.
    Standard,
    /// Configured high-quality steady state after the Standard snap-back frame.
    Fancy,
}

/// Explicit target and affected-pane set for one primary gesture mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererMutationTarget {
    /// Stable target window identity.
    pub window_id: String,
    /// Stable target tab identity when the operation is tab-scoped.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub tab_id: Option<String>,
    /// Exact non-empty affected pane identities in canonical manifest order.
    pub affected_pane_ids: Vec<String>,
}

/// Replayable state mutation in a gesture timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererTimelineAction {
    /// Start the gesture.
    #[serde(rename = "gesture_begin")]
    BeginGesture,
    /// Change only drawable viewport dimensions.
    SetWindowSize {
        /// Explicit target window and affected panes.
        target: RendererMutationTarget,
        /// New drawable width.
        width_px: u32,
        /// New drawable height.
        height_px: u32,
    },
    /// Change terminal grid dimensions.
    SetGrid {
        /// Explicit target tab/panes.
        target: RendererMutationTarget,
        /// New terminal columns.
        columns: u16,
        /// New terminal rows.
        rows: u16,
    },
    /// Change the renderer font scale while retaining configured base size.
    SetFontScale {
        /// Explicit target window/tab/panes.
        target: RendererMutationTarget,
        /// New font scale multiplied by 1,000.
        scale_milli: u32,
    },
    /// Change renderer quality mode.
    SetQualityMode {
        /// Explicit target window/tab/panes.
        target: RendererMutationTarget,
        /// New Draft, Standard, or Fancy mode.
        mode: RendererQualityMode,
    },
    /// Change display identity, DPI, scale, color, and dynamic-range metadata.
    MoveToDisplay {
        /// Explicit target window and affected panes.
        target: RendererMutationTarget,
        /// New display metadata; drawable size is a separate action.
        display: RendererDisplayTransition,
    },
    /// Start or change deterministic output generation.
    SetOutputRate {
        /// Workload-local stream identity.
        stream_id: String,
        /// Output bytes per second; zero explicitly stops output.
        bytes_per_second: u64,
    },
    /// Inject foreground key events into the focused pane.
    ForegroundKey {
        /// Workload-local exact key-event identity.
        key_event_id: String,
    },
    /// Advance explicit focused-pane grid/terminal generations.
    SetRevisions {
        /// Explicit target tab/panes.
        target: RendererMutationTarget,
        /// New renderer generation.
        renderer_generation: u64,
        /// New grid generation.
        grid_revision: u64,
        /// New terminal-state generation.
        terminal_revision: u64,
    },
    /// End input for the gesture.
    #[serde(rename = "gesture_end")]
    EndGesture,
    /// Declare the deterministic settle boundary.
    Settle,
}

/// One event in a deterministic gesture timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererTimelineEvent {
    /// Zero-based, contiguous event ordinal.
    pub event_ordinal: u32,
    /// Strictly increasing offset from gesture start; event zero is at zero.
    pub at_us: u64,
    /// Ordered gesture phase.
    pub phase: RendererTimelinePhase,
    /// Non-empty atomic action bundle, unique by action kind.
    pub actions: Vec<RendererTimelineAction>,
}

/// Expected state invariant bound to an exact timeline event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererExpectedInvariant {
    /// Stable invariant identity used in structured assertion logs.
    pub invariant_id: String,
    /// Non-empty canonical phases in which checkpoints evaluate this invariant.
    pub applicable_phases: Vec<RendererTimelinePhase>,
    /// Relative, non-traversing repository reference to the oracle definition.
    pub oracle_ref: String,
}

/// Semantic role of a renderer checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCheckpointRole {
    /// Initial nonblank identity baseline.
    InitialBaseline,
    /// Last Draft frame retained only as transition provenance.
    #[serde(rename = "last_draft_provenance")]
    LastDraftProvenance,
    /// Non-RQ-S11 intermediate mutation checkpoint.
    Intermediate,
    /// Post-gesture Standard subject compared to an independent Standard oracle.
    #[serde(rename = "standard_snap_back_subject")]
    StandardSnapBackSubject,
    /// Final configured Standard-or-Fancy steady state.
    FinalSteadyState,
}

/// Expected visible-content class at a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererFrameContentClass {
    /// Explicit nonblank baseline preceding every cross-frame detector.
    NonblankBaseline,
    /// Nonblank in-gesture transient frame.
    NonblankTransient,
    /// Exactly one nonblank Standard post-gesture snap-back frame.
    NonblankStandardSnapBack,
    /// Nonblank final configured steady-state frame.
    NonblankSteadyState,
}

/// Closed feature-overlay suite applied to each of the 32 base cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCoverageOverlayId {
    ProductionDefault,
    UnicodeMaximal,
    AlternateScreen,
    ImeComposing,
    ImageHyperlink,
    LigatureEnabled,
    Selection,
    A11yGeometry,
}

impl RendererCoverageOverlayId {
    pub const ALL: [Self; 8] = [
        Self::ProductionDefault,
        Self::UnicodeMaximal,
        Self::AlternateScreen,
        Self::ImeComposing,
        Self::ImageHyperlink,
        Self::LigatureEnabled,
        Self::Selection,
        Self::A11yGeometry,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProductionDefault => "production_default",
            Self::UnicodeMaximal => "unicode_maximal",
            Self::AlternateScreen => "alternate_screen",
            Self::ImeComposing => "ime_composing",
            Self::ImageHyperlink => "image_hyperlink",
            Self::LigatureEnabled => "ligature_enabled",
            Self::Selection => "selection",
            Self::A11yGeometry => "a11y_geometry",
        }
    }
}

/// Split-tree direction used by the closed layout derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererSplitDirection {
    Horizontal,
    Vertical,
}

/// Branch identity in the frozen balanced contiguous split tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererSplitTreeBranch {
    First,
    Second,
}

/// Closed, deterministic topology expansion algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererLayoutDerivation {
    BalancedContiguousV1,
}

/// Exact normalized layout profile; the validator expands every stable ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererLayoutProfile {
    pub layout_profile_id: String,
    pub fleet_point: RendererFleetPoint,
    pub derivation: RendererLayoutDerivation,
    pub stable_id_revision: String,
    pub pane_count: u16,
    pub tab_count: u16,
    pub window_count: u16,
    pub initial_split_direction: RendererSplitDirection,
    pub split_ratio_milli: u16,
    pub alternate_split_direction: bool,
    pub lowest_ordinal_gets_remainder: bool,
}

/// Deduplicated full pane-surface state used by normalized phase manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererSurfaceStateTemplate {
    pub surface_state_template_id: String,
    pub surface_state: RendererSurfaceState,
}

/// Closed pane-ordinal selector used by content and state distributions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererPaneOrdinalSelector {
    All,
    OrdinalRange { start: u16, end_exclusive: u16 },
    Explicit { ordinals: Vec<u16> },
}

/// One non-overlapping pane-content assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererContentDistributionAssignment {
    pub selector: RendererPaneOrdinalSelector,
    /// Ordered, replayable composition steps for every selected pane.
    pub materialization_steps: Vec<RendererContentMaterializationStep>,
}

/// Operation used to compose one deterministic content input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererContentCompositionOperation {
    ReplaceActiveBuffer,
    AppendToActiveBuffer,
    EnterAlternateBuffer,
    ExitAlternateBuffer,
    /// Apply a complete typed fixture. A fixture carrying text/protocol
    /// semantic kinds appends its visible identity to the active buffer;
    /// a state-only fixture with no semantic kinds changes typed geometry/mode
    /// state without claiming visible buffer content.
    ApplyTypedStateOverlay,
}

/// Exact timeline boundary at which a materialization step is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererContentApplicationBoundary {
    BeforeGesture,
    AtEvent { event_ordinal: u32 },
    AfterCheckpoint { checkpoint_id: String },
}

/// One ordered content-input/state-composition step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererContentMaterializationStep {
    pub step_ordinal: u16,
    pub content_corpus_id: String,
    pub operation: RendererContentCompositionOperation,
    pub application_boundary: RendererContentApplicationBoundary,
    /// Ordered checkpoints through which the resulting state must remain active.
    pub hold_through_checkpoint_ids: Vec<String>,
    /// A missing step blocks only overlays selecting this profile.
    pub availability: RendererContentInputAvailability,
}

/// Fully typed normalized content distribution for one exact fleet point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererContentDistributionProfile {
    pub content_distribution_profile_id: String,
    pub profile_revision: u32,
    pub fleet_point: RendererFleetPoint,
    pub assignments: Vec<RendererContentDistributionAssignment>,
}

/// Exact per-pane output state in a phase manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPaneOutputState {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stream_id: Option<String>,
    pub bytes_per_second: u64,
}

/// One non-overlapping phase-specific pane state/output override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPhasePaneOverride {
    pub selector: RendererPaneOrdinalSelector,
    pub surface_state_template_id: String,
    pub output: RendererPaneOutputState,
}

/// Exact per-window phase state; one row is required per layout window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPhaseWindowState {
    pub window_ordinal: u16,
    /// Tab ordinal local to this window's ordered tab sequence.
    pub active_tab_ordinal: u16,
    /// Window-local drawable client/tab-content region; x=y=0 and OS chrome is
    /// excluded. Pane surfaces partition this rectangle, and their content
    /// padding remains internal to those pane viewports.
    pub window_rect: RendererPixelRect,
}

/// Compact same-catalog manifest expanded and checked for every pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPhaseManifest {
    pub phase_manifest_id: String,
    pub phase: RendererTimelinePhase,
    pub event_ordinal: u32,
    /// Closed coverage overlay identity; never inferred from scenario position.
    pub overlay_id: RendererCoverageOverlayId,
    pub layout_profile_id: String,
    pub default_surface_state_template_id: String,
    pub content_distribution_profile_id: String,
    pub focused_window_ordinal: u16,
    pub focused_pane_ordinal: u16,
    pub window_states: Vec<RendererPhaseWindowState>,
    pub default_output: RendererPaneOutputState,
    pub pane_overrides: Vec<RendererPhasePaneOverride>,
}

/// Closed reason an overlay cannot qualify a particular measurement predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererOverlayExclusionReason {
    ImeMayConsumeOrTransformForegroundKey,
    AlternateScreenConflictsWithPrimaryScrollback,
}

/// Exact SLO or measurement target excluded for an otherwise runnable overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "target_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererOverlayQualificationTarget {
    Requirement { requirement_id: RendererRequirementId },
    Measurement { measurement_role: RendererMeasurementRole },
}

/// Typed overlay-to-measurement incompatibility; omission never implies exclusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOverlayQualificationExclusion {
    pub target: RendererOverlayQualificationTarget,
    pub reason: RendererOverlayExclusionReason,
    pub detail: String,
    pub tracking_ref: String,
}

/// Whether an overlay may carry an exact requirement cross-map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererOverlayQualificationClass {
    ProductionDefaultSloCandidate,
    VisualCoverageRelatedOnly,
}

/// Same-catalog feature overlay for one exact scenario cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererCoverageOverlayProfile {
    pub overlay_profile_id: String,
    pub profile_revision: u32,
    pub overlay_id: RendererCoverageOverlayId,
    pub renderer_config_profile_id: String,
    pub qualification_class: RendererOverlayQualificationClass,
    /// Capability rows replacing inherited rows for this overlay only.
    pub capability_deltas: Vec<RendererCapabilityBinding>,
    /// Closed incompatibilities; all other explicitly named bindings are compatible.
    pub qualification_exclusions: Vec<RendererOverlayQualificationExclusion>,
}

/// Visual checkpoint bound to state, bitmap, and accessibility oracles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererVisualCheckpoint {
    /// Stable checkpoint identity.
    pub checkpoint_id: String,
    /// Exact overlay whose state/oracles this checkpoint binds.
    pub overlay_id: RendererCoverageOverlayId,
    /// Initial, Draft source, intermediate, or final role.
    pub role: RendererCheckpointRole,
    /// Phase copied from the bound timeline event.
    pub phase: RendererTimelinePhase,
    /// Exact event ordinal at which all three oracles are evaluated.
    pub event_ordinal: u32,
    /// Non-empty invariant identities evaluated at this exact event.
    pub expected_invariant_ids: Vec<String>,
    /// Explicitly nonblank baseline/transient/snap-back/steady class.
    pub expected_frame_content_class: RendererFrameContentClass,
    /// Same-catalog normalized phase manifest identity.
    pub phase_manifest_id: String,
    /// Repository reference to the state-oracle definition.
    pub state_oracle_ref: String,
    /// Repository reference to the visual oracle/comparator input.
    pub visual_oracle_ref: String,
    /// Independent Standard oracle used only by the snap-back subject.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub independent_standard_oracle_ref: Option<String>,
    /// Repository reference to the accessibility geometry oracle.
    pub accessibility_oracle_ref: String,
    /// Repository references to comparator policies; thresholds stay there.
    pub comparator_policy_refs: Vec<String>,
    /// Whether this checkpoint requires native capture rather than a proxy.
    pub native_capture_required: bool,
}

/// Narrow authority carried by a canonical corpus source.
///
/// No variant can authorize native-window, native-target, performance, or
/// product-accessibility proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCorpusAuthorityScope {
    /// Checked-in pixels usable by a headless visual comparator.
    HeadlessVisualFixture,
    /// Checked-in state/event data usable for deterministic headless replay.
    HeadlessStateReplay,
    /// Metamorphic/fuzz signal only; not a visual or native verdict.
    MetamorphicSignalOnly,
    /// Product/plan contract reference without execution authority.
    ContractOnly,
}

impl RendererCorpusAuthorityScope {
    /// Exact authority-scope inventory required by version 1.
    pub const ALL: [Self; 4] = [
        Self::HeadlessVisualFixture,
        Self::HeadlessStateReplay,
        Self::MetamorphicSignalOnly,
        Self::ContractOnly,
    ];
}

/// Qualification of a corpus entry for the feature mapping it declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCorpusCoverageStatus {
    /// Canonical corpus entry directly covers the declared feature mapping.
    Direct,
    /// Canonical corpus entry covers only part of the declared mapping.
    Partial,
    /// No qualifying corpus entry currently covers the mapping.
    Gap,
    /// Bytes are present, but the entry is not qualified for this mapping.
    PresentUnqualified,
}

/// Closed source encoding for deterministic terminal input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererContentEncoding {
    RawTerminalBytes,
    Utf8Text,
    HexTranscriptV1,
    GpuFixtureStateV1,
    GeneratedTerminalBytesV1,
    GeneratedTypedStateV1,
}

/// Closed transformation from encoded source bytes to materialized input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererContentDecoder {
    Identity,
    Utf8ValidateV1,
    HexDecodeV1,
    JsonFixtureStateV1,
    GeneratorV1,
}

/// Closed semantic classes derived by the pinned decoder/materializer.
///
/// Stateful features such as cursor, selection, IME, and accessibility are not
/// represented here; they are derived from typed surface state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererContentSemanticKind {
    AsciiText,
    CjkText,
    RtlText,
    CombiningMarkText,
    EmojiText,
    LigatureSequence,
    ImageProtocol,
    HyperlinkProtocol,
    AlternateScreenControl,
}

impl RendererContentSemanticKind {
    pub const ALL: [Self; 9] = [
        Self::AsciiText,
        Self::CjkText,
        Self::RtlText,
        Self::CombiningMarkText,
        Self::EmojiText,
        Self::LigatureSequence,
        Self::ImageProtocol,
        Self::HyperlinkProtocol,
        Self::AlternateScreenControl,
    ];
}

/// Exact selector within a checked-in payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererContentPayloadSelector {
    WholePayload,
    ManifestRowSegment {
        manifest_ref: String,
        manifest_row_id: String,
        decoded_byte_start: u64,
        decoded_byte_end_exclusive: u64,
    },
}

/// Closed framing/materialization interpretation of decoded content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererContentFraming {
    CompleteTerminalStream,
    Utf8Text,
    /// The materializer supplies the fixed ESC_G ... ESC\\ envelope.
    KittyGraphicsApcBodyV1,
    /// The materializer supplies the fixed ESC_P ... ESC\\ envelope.
    SixelDcsBodyV1,
    TypedStateOverlay,
}

/// Availability of reproducible content input, independent of evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererContentInputAvailability {
    Available,
    Unavailable {
        reason: String,
        tracking_refs: Vec<String>,
    },
}

/// Terminal-content input referenced by deterministic workloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererContentCorpusReference {
    /// Stable terminal-content identity.
    pub content_corpus_id: String,
    /// Relative, non-traversing repository reference to its canonical source.
    pub repository_ref: String,
    /// Closed semantics produced by the pinned decoder/materializer.
    pub semantic_kinds: Vec<RendererContentSemanticKind>,
    /// Positive payload or generator revision.
    pub payload_revision: u32,
    /// Explicit input readiness; unavailable inputs remain typed gaps.
    pub availability: RendererContentInputAvailability,
    /// Exact reproducible payload or generator identity.
    pub deterministic_identity: RendererContentDeterministicIdentity,
}

/// Reproducible content bytes/state without evidence authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererContentDeterministicIdentity {
    Generator {
        generator_id: String,
        generator_revision: u32,
        generator_seed: u64,
        input_manifest_ref: String,
        output_encoding: RendererContentEncoding,
        output_decoder: RendererContentDecoder,
        output_framing: RendererContentFraming,
    },
    Payload {
        payload_ref: String,
        selector: RendererContentPayloadSelector,
        encoding: RendererContentEncoding,
        decoder: RendererContentDecoder,
        framing: RendererContentFraming,
        /// SHA-256 of the checked-in encoded payload.
        encoded_payload_sha256: String,
        /// SHA-256 of bytes after decoding and before framing.
        decoded_payload_sha256: String,
    },
}

/// Evidence/authority source, deliberately separate from terminal content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererEvidenceSource {
    /// Stable evidence-source identity.
    pub evidence_source_id: String,
    /// Exact canonical repository source.
    pub repository_ref: String,
    /// Narrow headless/metamorphic/contract authority of this source.
    pub authority_scope: RendererCorpusAuthorityScope,
    /// Whether this source can authorize deterministic headless gesture replay.
    pub authorizes_gesture_replay: bool,
    /// Whether it can authorize headless checkpoint comparison.
    pub authorizes_headless_checkpoint_comparison: bool,
    /// Overall source qualification, separate from feature-specific mappings.
    pub coverage_status: RendererCorpusCoverageStatus,
    /// Required for non-direct source qualification.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub limitation: Option<String>,
    /// Required non-empty owner list for non-direct source qualification.
    pub tracking_refs: Vec<String>,
}

/// One evidence source's classification for one terminal feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererFeatureEvidenceSourceBinding {
    pub evidence_source_id: String,
    pub coverage_status: RendererCorpusCoverageStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub limitation: Option<String>,
    pub tracking_refs: Vec<String>,
}

/// Exact feature-to-evidence mapping, never terminal-content input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererFeatureEvidenceBinding {
    pub terminal_feature: RendererTerminalFeature,
    pub sources: Vec<RendererFeatureEvidenceSourceBinding>,
}

/// Closed deterministic aggregate-output distribution policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererOutputDistributionPolicy {
    EvenLowestOrdinalRemainderV1,
}

/// Optional small selector override applied after the closed distribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOutputRateOverride {
    pub selector: RendererPaneOrdinalSelector,
    pub bytes_per_second: u64,
}

/// Fully pinned deterministic PTY output generator and distribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOutputStreamDefinition {
    /// Workload-local stream identity used by timeline actions.
    pub stream_id: String,
    /// Stable generator implementation identity.
    pub generator_id: String,
    /// Positive generator revision.
    pub generator_revision: u32,
    /// Non-zero generator seed.
    pub generator_seed: u64,
    /// Exact payload/generator-input manifest.
    pub payload_manifest_ref: String,
    /// Stable pane-distribution identity.
    pub distribution_id: String,
    /// Exact normalized layout receiving the stream.
    pub layout_profile_id: String,
    /// Closed expansion policy; no 200-pane hand-authored list.
    pub distribution_policy: RendererOutputDistributionPolicy,
    /// Exact aggregate decimal byte rate.
    pub aggregate_bytes_per_second: u64,
    /// Optional non-overlapping small selector overrides.
    pub rate_overrides: Vec<RendererOutputRateOverride>,
}

/// Closed keyboard modifier identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererKeyModifier {
    Shift,
    Control,
    Alt,
    Super,
}

/// Exact deterministic foreground key event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererForegroundKeyEvent {
    /// Workload-local identity referenced by one timeline action.
    pub key_event_id: String,
    /// Stable logical-key name.
    pub logical_key: String,
    /// Canonical modifier order without duplicates.
    pub modifiers: Vec<RendererKeyModifier>,
    /// Lowercase even-length hexadecimal encoded terminal bytes.
    pub encoded_bytes_hex: String,
    /// Exact target pane identity.
    pub target_pane_id: String,
}

/// Deterministic workload identity assembled from canonical corpus references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererWorkloadDefinition {
    /// Stable workload identity.
    pub workload_id: String,
    /// Positive content revision.
    pub revision: u32,
    /// Exact pane count for the bound fleet point.
    pub pane_count: u16,
    /// Exact tab count for the bound fleet point.
    pub tab_count: u16,
    /// Exact window count for the bound fleet point.
    pub window_count: u16,
    /// Stable workload layout/topology profile identity.
    pub layout_profile_id: String,
    /// Same-catalog production-default renderer configuration identity.
    pub renderer_config_profile_id: String,
    /// Exact effective font/cell-metric derivation identity.
    pub font_metric_derivation_ref: String,
    /// Exact native gesture-input duration; gesture_end occurs here.
    pub gesture_duration_us: u64,
    /// Positive end-to-end duration; final settle occurs here.
    pub total_duration_us: u64,
    /// Exact number of timeline events in every bound scenario.
    pub event_count: u32,
    /// Optional fully pinned PTY stream; absent means zero background output.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub output_stream: Option<RendererOutputStreamDefinition>,
    /// Exact foreground key identities referenced by timeline actions.
    pub foreground_key_events: Vec<RendererForegroundKeyEvent>,
    /// Exact preloaded scrollback lines in every pane.
    pub scrollback_lines_per_pane: u32,
    /// Exact count of glyph identities introduced during the gesture.
    pub new_glyph_count: u32,
    /// Exact native/window resize mutation count.
    pub resize_mutation_count: u32,
    /// Non-empty references into the terminal-content corpus map.
    pub content_corpus_ids: Vec<String>,
}

/// Repository references governing all scenario-oracle bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOracleContracts {
    /// State-oracle contract definition.
    pub state_oracle_contract_ref: String,
    /// Canonical visual comparator contract; numeric thresholds stay there.
    pub visual_comparator_contract_ref: String,
    /// Accessibility tree/geometry oracle contract.
    pub accessibility_oracle_contract_ref: String,
    /// Checkpoint placement and evaluation policy.
    pub checkpoint_policy_ref: String,
    /// Exact tracker preventing stale Draft-vs-Standard RQ-S11 interpretation.
    pub rq_s11_contradiction_tracking_ref: String,
}

/// Explicit separation between machine geometry and product accessibility.
///
/// Both references identify future work. Neither grants a verdict, and the
/// renderer geometry lane can never substitute for native or human authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererAccessibilityAuthorityBoundary {
    /// Exact owner of machine-renderer geometry comparison.
    pub renderer_geometry_tracking_ref: String,
    /// Exact owner of live NSAccessibility, VoiceOver, and human review.
    pub native_accessibility_tracking_ref: String,
    /// Must remain false; machine geometry never grants native authority.
    pub machine_geometry_authorizes_native_accessibility: bool,
}

/// Renderer requirement identifiers referenced by the scenario crosswalk.
///
/// These are scope links only. Their presence does not assert that a scenario
/// ran or that a requirement passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RendererRequirementId {
    /// Resize frame-rate requirement.
    #[serde(rename = "RQ-S1.resize_fps")]
    RqS1ResizeFps,
    /// Heavy-burst input-latency requirement.
    #[serde(rename = "RQ-S6.heavy_burst_input_latency")]
    RqS6HeavyBurstInputLatency,
    /// Forward 80-to-200 reflow-latency requirement.
    #[serde(rename = "RQ-S9.reflow_latency")]
    RqS9ReflowLatency,
    /// Pure-resize atlas-rebuild requirement.
    #[serde(rename = "RQ-S10.atlas_rebuild_count")]
    RqS10AtlasRebuildCount,
    /// Final snap-back image-similarity requirement.
    #[serde(rename = "RQ-S11.snap_back_ssim")]
    RqS11SnapBackSsim,
    /// Comparator-oracle corpus scope.
    #[serde(rename = "RQ-S13.ssim_parity_oracle_corpus")]
    RqS13SsimParityOracleCorpus,
}

impl RendererRequirementId {
    /// Stable serialized requirement identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RqS1ResizeFps => "RQ-S1.resize_fps",
            Self::RqS6HeavyBurstInputLatency => "RQ-S6.heavy_burst_input_latency",
            Self::RqS9ReflowLatency => "RQ-S9.reflow_latency",
            Self::RqS10AtlasRebuildCount => "RQ-S10.atlas_rebuild_count",
            Self::RqS11SnapBackSsim => "RQ-S11.snap_back_ssim",
            Self::RqS13SsimParityOracleCorpus => "RQ-S13.ssim_parity_oracle_corpus",
        }
    }
}

/// Closed renderer checkpoint detector inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCheckpointDetectorId {
    NoMissingGlyphs,
    CoherentCellWidths,
    ExactRowWidth,
    NoFlicker,
    CoherentRendererGeneration,
    NoMixedGenerationTearBand,
    NoStaleOrDuplicateFrame,
    NonblankAfterBaseline,
    SsimPolicy,
    LInfPolicy,
    ChangedPixelFractionPolicy,
    ExactTerminalState,
    CursorGeometry,
    SelectionGeometry,
    ImeGeometry,
    HyperlinkGeometry,
    ImageGeometry,
    AlternateScreenState,
    AccessibilityGeometry,
    ExactlyOneStandardSnapBack,
}

impl RendererCheckpointDetectorId {
    /// Exact detector inventory in canonical serialized/checkpoint order.
    pub const ALL: [Self; 20] = [
        Self::NoMissingGlyphs,
        Self::CoherentCellWidths,
        Self::ExactRowWidth,
        Self::NoFlicker,
        Self::CoherentRendererGeneration,
        Self::NoMixedGenerationTearBand,
        Self::NoStaleOrDuplicateFrame,
        Self::NonblankAfterBaseline,
        Self::SsimPolicy,
        Self::LInfPolicy,
        Self::ChangedPixelFractionPolicy,
        Self::ExactTerminalState,
        Self::CursorGeometry,
        Self::SelectionGeometry,
        Self::ImeGeometry,
        Self::HyperlinkGeometry,
        Self::ImageGeometry,
        Self::AlternateScreenState,
        Self::AccessibilityGeometry,
        Self::ExactlyOneStandardSnapBack,
    ];

    /// Stable detector identity.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoMissingGlyphs => "no_missing_glyphs",
            Self::CoherentCellWidths => "coherent_cell_widths",
            Self::ExactRowWidth => "exact_row_width",
            Self::NoFlicker => "no_flicker",
            Self::CoherentRendererGeneration => "coherent_renderer_generation",
            Self::NoMixedGenerationTearBand => "no_mixed_generation_tear_band",
            Self::NoStaleOrDuplicateFrame => "no_stale_or_duplicate_frame",
            Self::NonblankAfterBaseline => "nonblank_after_baseline",
            Self::SsimPolicy => "ssim_policy",
            Self::LInfPolicy => "l_inf_policy",
            Self::ChangedPixelFractionPolicy => "changed_pixel_fraction_policy",
            Self::ExactTerminalState => "exact_terminal_state",
            Self::CursorGeometry => "cursor_geometry",
            Self::SelectionGeometry => "selection_geometry",
            Self::ImeGeometry => "ime_geometry",
            Self::HyperlinkGeometry => "hyperlink_geometry",
            Self::ImageGeometry => "image_geometry",
            Self::AlternateScreenState => "alternate_screen_state",
            Self::AccessibilityGeometry => "accessibility_geometry",
            Self::ExactlyOneStandardSnapBack => "exactly_one_standard_snap_back",
        }
    }
}

/// Contract status of one detector mechanism; never an execution verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererDetectorMechanismStatus {
    /// The detector contract is defined without claiming an independent proof.
    ContractDefined,
    /// The detector is known to share semantics with another gate.
    KnownNonIndependent {
        /// Precise explanation of the dependency.
        reason: String,
        /// Exact repair owner.
        tracking_ref: String,
    },
}

/// Canonical detector contract row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererDetectorContract {
    /// Closed detector identity.
    pub detector_id: RendererCheckpointDetectorId,
    /// Single-checkpoint, pair, interval, or whole-timeline scope.
    pub scope: RendererDetectorScope,
    /// Repository reference to the detector/oracle definition.
    pub oracle_ref: String,
    /// Contract-defined or known-non-independent mechanism status.
    pub mechanism_status: RendererDetectorMechanismStatus,
}

/// Authority scope of a renderer requirement binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererRequirementScope {
    /// The complete scenario/workload predicate must match the requirement.
    ExactScenarioPredicate,
    /// Structurally exact candidate whose source/authority is still unproven.
    ExactCandidateUnproven,
    /// Related context only; no predicate equivalence.
    RelatedOnly,
    /// Deliberately harder concurrent workload related to, not equal to, an SLO.
    RelatedAdversarialSuperset,
    /// Larger-fleet stress variant related to a singular source predicate.
    RelatedFleetStress,
    /// One exact final/settled checkpoint carries the binding.
    CheckpointPredicate,
    /// The row references comparator mechanics only, never an SLO verdict.
    ComparatorMechanismOnly,
}

/// Typed requirement crosswalk row with requirement-specific endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "requirement_id", deny_unknown_fields)]
pub enum RendererRequirementBinding {
    #[serde(rename = "RQ-S1.resize_fps")]
    RqS1 {
        scope: RendererRequirementScope,
        overlay_id: RendererCoverageOverlayId,
    },
    #[serde(rename = "RQ-S6.heavy_burst_input_latency")]
    RqS6 {
        scope: RendererRequirementScope,
        overlay_id: RendererCoverageOverlayId,
    },
    #[serde(rename = "RQ-S9.reflow_latency")]
    RqS9 {
        scope: RendererRequirementScope,
        overlay_id: RendererCoverageOverlayId,
    },
    #[serde(rename = "RQ-S10.atlas_rebuild_count")]
    RqS10 {
        scope: RendererRequirementScope,
        overlay_id: RendererCoverageOverlayId,
    },
    #[serde(rename = "RQ-S11.snap_back_ssim")]
    RqS11 {
        scope: RendererRequirementScope,
        overlay_id: RendererCoverageOverlayId,
        last_draft_checkpoint_id: String,
        standard_snap_back_subject_checkpoint_id: String,
        independent_standard_oracle_ref: String,
    },
    #[serde(rename = "RQ-S13.ssim_parity_oracle_corpus")]
    RqS13 {
        scope: RendererRequirementScope,
        overlay_id: RendererCoverageOverlayId,
    },
}

impl RendererRequirementBinding {
    const fn requirement_id(&self) -> RendererRequirementId {
        match self {
            Self::RqS1 { .. } => RendererRequirementId::RqS1ResizeFps,
            Self::RqS6 { .. } => RendererRequirementId::RqS6HeavyBurstInputLatency,
            Self::RqS9 { .. } => RendererRequirementId::RqS9ReflowLatency,
            Self::RqS10 { .. } => RendererRequirementId::RqS10AtlasRebuildCount,
            Self::RqS11 { .. } => RendererRequirementId::RqS11SnapBackSsim,
            Self::RqS13 { .. } => RendererRequirementId::RqS13SsimParityOracleCorpus,
        }
    }

    const fn scope(&self) -> RendererRequirementScope {
        match self {
            Self::RqS1 { scope, .. }
            | Self::RqS6 { scope, .. }
            | Self::RqS9 { scope, .. }
            | Self::RqS10 { scope, .. }
            | Self::RqS11 { scope, .. }
            | Self::RqS13 { scope, .. } => *scope,
        }
    }

    const fn overlay_id(&self) -> RendererCoverageOverlayId {
        match self {
            Self::RqS1 { overlay_id, .. }
            | Self::RqS6 { overlay_id, .. }
            | Self::RqS9 { overlay_id, .. }
            | Self::RqS10 { overlay_id, .. }
            | Self::RqS11 { overlay_id, .. }
            | Self::RqS13 { overlay_id, .. } => *overlay_id,
        }
    }
}

/// Canonical deliberate-defect control identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererNegativeControlId {
    /// Remove a glyph that should be visible.
    MissingGlyph,
    /// Combine state from two renderer generations.
    MixedRendererGeneration,
    /// Displace the cursor from its expected cell.
    CursorDisplacement,
    /// Drop an active selection.
    SelectionLoss,
    /// Retain an obsolete inline image.
    StaleImage,
    /// Displace IME preedit/caret geometry.
    ImeGeometryDisplacement,
    /// Corrupt a hyperlink cell range.
    HyperlinkRangeCorruption,
    /// Flip primary/alternate-screen state.
    AlternateScreenFlip,
    /// Report a wrong terminal grid.
    GridDimensionMismatch,
    /// Present a duplicate stale frame.
    DuplicateStaleFrame,
    /// Displace accessibility geometry.
    AccessibilityGeometryDisplacement,
    /// Emit a blank frame after a nonblank frame.
    BlankFrameAfterNonblank,
    /// Introduce a cross-generation tear band.
    MixedGenerationTearBand,
}

impl RendererNegativeControlId {
    /// Exact negative-control inventory required by version 1.
    pub const ALL: [Self; 13] = [
        Self::MissingGlyph,
        Self::MixedRendererGeneration,
        Self::CursorDisplacement,
        Self::SelectionLoss,
        Self::StaleImage,
        Self::ImeGeometryDisplacement,
        Self::HyperlinkRangeCorruption,
        Self::AlternateScreenFlip,
        Self::GridDimensionMismatch,
        Self::DuplicateStaleFrame,
        Self::AccessibilityGeometryDisplacement,
        Self::BlankFrameAfterNonblank,
        Self::MixedGenerationTearBand,
    ];

    /// Stable control label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingGlyph => "missing_glyph",
            Self::MixedRendererGeneration => "mixed_renderer_generation",
            Self::CursorDisplacement => "cursor_displacement",
            Self::SelectionLoss => "selection_loss",
            Self::StaleImage => "stale_image",
            Self::ImeGeometryDisplacement => "ime_geometry_displacement",
            Self::HyperlinkRangeCorruption => "hyperlink_range_corruption",
            Self::AlternateScreenFlip => "alternate_screen_flip",
            Self::GridDimensionMismatch => "grid_dimension_mismatch",
            Self::DuplicateStaleFrame => "duplicate_stale_frame",
            Self::AccessibilityGeometryDisplacement => {
                "accessibility_geometry_displacement"
            }
            Self::BlankFrameAfterNonblank => "blank_frame_after_nonblank",
            Self::MixedGenerationTearBand => "mixed_generation_tear_band",
        }
    }

    /// Stable failure code expected when this defect is injected.
    #[must_use]
    pub const fn expected_failure_code(self) -> &'static str {
        match self {
            Self::MissingGlyph => "RSC-CONTROL-001",
            Self::MixedRendererGeneration => "RSC-CONTROL-002",
            Self::CursorDisplacement => "RSC-CONTROL-003",
            Self::SelectionLoss => "RSC-CONTROL-004",
            Self::StaleImage => "RSC-CONTROL-005",
            Self::ImeGeometryDisplacement => "RSC-CONTROL-006",
            Self::HyperlinkRangeCorruption => "RSC-CONTROL-007",
            Self::AlternateScreenFlip => "RSC-CONTROL-008",
            Self::GridDimensionMismatch => "RSC-CONTROL-009",
            Self::DuplicateStaleFrame => "RSC-CONTROL-010",
            Self::AccessibilityGeometryDisplacement => "RSC-CONTROL-011",
            Self::BlankFrameAfterNonblank => "RSC-CONTROL-012",
            Self::MixedGenerationTearBand => "RSC-CONTROL-013",
        }
    }
}

/// One required deliberate-defect control definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererNegativeControlDefinition {
    /// Canonical control identity.
    pub control_id: RendererNegativeControlId,
    /// Exact scenario containing the injected defect.
    pub scenario_id: String,
    /// Exact overlay in which the deliberate defect is injected.
    pub overlay_id: RendererCoverageOverlayId,
    /// Exact checkpoint expected to detect the defect.
    pub checkpoint_id: String,
    /// Timeline phase in which the defect is injected.
    pub injected_phase: RendererTimelinePhase,
    /// Closed checkpoint detector expected to reject the defect.
    pub bound_detector_id: RendererCheckpointDetectorId,
    /// Required corpus feature for feature-specific controls.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub required_feature: Option<RendererTerminalFeature>,
    /// Must equal [`RendererNegativeControlId::expected_failure_code`].
    pub expected_failure_code: String,
}

/// One source classification inside a gesture-authority row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererGestureAuthoritySource {
    /// Canonical evidence-source identity carrying this classification.
    pub evidence_source_id: String,
    /// Direct, partial, gap, or present-unqualified source status.
    pub coverage_status: RendererCorpusCoverageStatus,
    /// Required for every non-direct status; absent for direct.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub limitation: Option<String>,
    /// Required for every non-direct status; empty for direct.
    ///
    /// Multiple independent implementation lanes may jointly own one gap.
    pub tracking_refs: Vec<String>,
}

/// One gesture's current headless contract authority and source classifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererGestureAuthorityEntry {
    /// Closed gesture key; exactly one entry is required per gesture.
    pub gesture: RendererGesture,
    /// Non-empty, independently classified canonical sources.
    pub sources: Vec<RendererGestureAuthoritySource>,
    /// Whether current sources authorize deterministic headless replay.
    pub headless_gesture_replay: bool,
    /// Whether current sources authorize headless checkpoint comparison.
    pub headless_checkpoint_compare: bool,
    /// Must remain false in a contract-only catalog.
    pub native_target_authority: bool,
}

/// Canonical legacy scenario/corpus identity that must be mapped explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererLegacyScenarioId {
    /// Renderer-overhaul plan row.
    #[serde(rename = "resize-step")]
    ResizeStep,
    /// Renderer-overhaul plan row.
    #[serde(rename = "resize-burst")]
    ResizeBurst,
    /// Renderer-overhaul plan row.
    #[serde(rename = "font-change")]
    FontChange,
    /// Renderer-overhaul plan row.
    #[serde(rename = "dpi-change")]
    DpiChange,
    /// Resize-baseline YAML basename.
    #[serde(rename = "resize_single_pane_scrollback.yaml")]
    ResizeSinglePaneScrollback,
    /// Resize-baseline YAML basename.
    #[serde(rename = "resize_multi_tab_storm.yaml")]
    ResizeMultiTabStorm,
    /// Resize-baseline YAML basename.
    #[serde(rename = "font_churn_multi_pane.yaml")]
    FontChurnMultiPane,
    /// Resize-baseline YAML basename.
    #[serde(rename = "mixed_scale_soak.yaml")]
    MixedScaleSoak,
    /// Resize-baseline YAML basename.
    #[serde(rename = "mixed_workload_interactive_streaming.yaml")]
    MixedWorkloadInteractiveStreaming,
    /// Accessibility corpus scenario.
    #[serde(rename = "steady_typing")]
    A11ySteadyTyping,
    /// Accessibility corpus scenario.
    #[serde(rename = "pane_focus_change")]
    A11yPaneFocusChange,
    /// Accessibility corpus scenario.
    #[serde(rename = "dialog_open")]
    A11yDialogOpen,
    /// Accessibility corpus scenario.
    #[serde(rename = "selection_change")]
    A11ySelectionChange,
    /// Accessibility corpus scenario.
    #[serde(rename = "scroll_position_change")]
    A11yScrollPositionChange,
}

impl RendererLegacyScenarioId {
    /// Exact version-1 legacy mapping inventory.
    pub const ALL: [Self; 14] = [
        Self::ResizeStep,
        Self::ResizeBurst,
        Self::FontChange,
        Self::DpiChange,
        Self::ResizeSinglePaneScrollback,
        Self::ResizeMultiTabStorm,
        Self::FontChurnMultiPane,
        Self::MixedScaleSoak,
        Self::MixedWorkloadInteractiveStreaming,
        Self::A11ySteadyTyping,
        Self::A11yPaneFocusChange,
        Self::A11yDialogOpen,
        Self::A11ySelectionChange,
        Self::A11yScrollPositionChange,
    ];
}

/// Non-qualifying disposition of a legacy scenario mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererLegacyMappingDisposition {
    /// Useful context, but not equivalent to a version-1 native scenario.
    RelatedOnly,
    /// Deliberately rejected as a scenario-equivalence source.
    Rejected,
}

/// Explicit non-qualifying mapping from a legacy source to new gestures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererLegacyScenarioMapping {
    /// Closed legacy identity.
    pub legacy_id: RendererLegacyScenarioId,
    /// Canonical relative source reference.
    pub source_ref: String,
    /// Related target gestures; never qualifying coverage.
    pub target_gestures: Vec<RendererGesture>,
    /// Related-only or rejected disposition.
    pub disposition: RendererLegacyMappingDisposition,
    /// Non-empty explanation of the non-equivalence.
    pub reason: String,
}

/// Closed requested presentation cadence profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererPresentationTargetProfileId {
    Fixed60Hz,
    Fixed120Hz,
    VariableRefreshRate,
}

impl RendererPresentationTargetProfileId {
    pub const ALL: [Self; 3] = [
        Self::Fixed60Hz,
        Self::Fixed120Hz,
        Self::VariableRefreshRate,
    ];
}

/// Availability metadata for a requested presentation target, not a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererPresentationTargetAvailability {
    /// Target availability has not yet been probed on an isolated runner.
    UnknownNotProbed {
        reason: String,
        tracking_ref: String,
    },
    /// Availability depends on an exact external target profile.
    TargetDependent {
        target_profile_ref: String,
        reason: String,
        tracking_ref: String,
    },
    /// The requested cadence is explicitly unavailable in the declared lane.
    Unsupported {
        reason: String,
        tracking_ref: String,
    },
}

/// Fixed or variable requested presentation cadence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPresentationTargetProfile {
    pub profile_id: RendererPresentationTargetProfileId,
    /// Inclusive minimum requested cadence in milli-Hz.
    pub minimum_millihz: u32,
    /// Inclusive maximum requested cadence in milli-Hz.
    pub maximum_millihz: u32,
    /// Explicit target availability without support authority.
    pub availability: RendererPresentationTargetAvailability,
}

/// Closed preconditioning profile identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererPreconditioningProfileId {
    Cold,
    Warm,
    Aged,
}

impl RendererPreconditioningProfileId {
    pub const ALL: [Self; 3] = [
        Self::Cold,
        Self::Warm,
        Self::Aged,
    ];
}

/// Cache/atlas preconditioning class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererCachePrecondition {
    Cold,
    Warm,
    Aged,
}

/// Deterministic fresh/aged session precondition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPreconditioningProfile {
    pub profile_id: RendererPreconditioningProfileId,
    pub session_age_us: u64,
    pub scrollback_age_us: u64,
    pub scrollback_lines_per_pane: u32,
    pub glyph_cache: RendererCachePrecondition,
    pub atlas: RendererCachePrecondition,
    pub prewarm_manifest_ref: String,
}

/// Closed downstream measurement role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererMeasurementRole {
    FirstCorrectViewport,
    SteadyPresentedFps,
    ColdReflowConvergence,
    SnapBack,
    KeypressToFirstCorrectPresent,
}

/// Closed execution-path authority class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererExecutionPathClass {
    ProductionNative,
    HeadlessReference,
    Unsupported,
}

/// Closed observation/presentation boundary class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererObservedBoundaryClass {
    InternalState,
    SoftwarePresent,
    MetalDrawable,
    DisplayPresented,
    Photon,
}

/// Closed target-selection rule for latency/convergence measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererObservedFrameSelection {
    FirstSatisfyingObservedFrame,
}

/// Exact K0-K13 keypress trace-v2 stage inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RendererKeypressTraceStage {
    #[serde(rename = "K0.key_appkit_receipt")]
    KeyAppkitReceipt,
    #[serde(rename = "K1.gui_key_mapping_complete")]
    GuiKeyMappingComplete,
    #[serde(rename = "K2.client_rpc_enqueue")]
    ClientRpcEnqueue,
    #[serde(rename = "K3.client_encode_socket_flush")]
    ClientEncodeSocketFlush,
    #[serde(rename = "K4.server_readable_decode")]
    ServerReadableDecode,
    #[serde(rename = "K5.server_dispatch_mux_wait")]
    ServerDispatchMuxWait,
    #[serde(rename = "K6.terminal_lock_pty_write_flush")]
    TerminalLockPtyWriteFlush,
    #[serde(rename = "K7.pty_echo_parser_apply")]
    PtyEchoParserApply,
    #[serde(rename = "K8.server_delta_compute")]
    ServerDeltaCompute,
    #[serde(rename = "K9.client_receive_decode_apply")]
    ClientReceiveDecodeApply,
    #[serde(rename = "K10.local_mux_gui_invalidation")]
    LocalMuxGuiInvalidation,
    #[serde(rename = "K11.paint_shape_atlas")]
    PaintShapeAtlas,
    #[serde(rename = "K12.gpu_submit_drawable_request")]
    GpuSubmitDrawableRequest,
    #[serde(rename = "K13.display_completion")]
    DisplayCompletion,
}

impl RendererKeypressTraceStage {
    pub const ALL: [Self; 14] = [
        Self::KeyAppkitReceipt,
        Self::GuiKeyMappingComplete,
        Self::ClientRpcEnqueue,
        Self::ClientEncodeSocketFlush,
        Self::ServerReadableDecode,
        Self::ServerDispatchMuxWait,
        Self::TerminalLockPtyWriteFlush,
        Self::PtyEchoParserApply,
        Self::ServerDeltaCompute,
        Self::ClientReceiveDecodeApply,
        Self::LocalMuxGuiInvalidation,
        Self::PaintShapeAtlas,
        Self::GpuSubmitDrawableRequest,
        Self::DisplayCompletion,
    ];
}

/// Exact R0-R25 resize/zoom production-path trace stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RendererResizeTraceStage {
    #[serde(rename = "R0.native_event_receipt")]
    NativeEventReceipt,
    #[serde(rename = "R1.gui_return")]
    GuiReturn,
    #[serde(rename = "R2.intent_enqueue")]
    IntentEnqueue,
    #[serde(rename = "R3.mux_resize_dispatch")]
    MuxResizeDispatch,
    #[serde(rename = "R4.pane_resize_apply")]
    PaneResizeApply,
    #[serde(rename = "R5.intent_supersession")]
    IntentSupersession,
    #[serde(rename = "R6.worker_create")]
    WorkerCreate,
    #[serde(rename = "R7.worker_start")]
    WorkerStart,
    #[serde(rename = "R8.terminal_lock_wait")]
    TerminalLockWait,
    #[serde(rename = "R9.terminal_lock_hold")]
    TerminalLockHold,
    #[serde(rename = "R10.viewport_reflow")]
    ViewportReflow,
    #[serde(rename = "R11.near_reflow")]
    NearReflow,
    #[serde(rename = "R12.cold_reflow")]
    ColdReflow,
    #[serde(rename = "R13.first_coherent_viewport")]
    FirstCoherentViewport,
    #[serde(rename = "R14.worker_join")]
    WorkerJoin,
    #[serde(rename = "R15.gui_invalidation")]
    GuiInvalidation,
    #[serde(rename = "R16.paint")]
    Paint,
    #[serde(rename = "R17.text_shaping")]
    TextShaping,
    #[serde(rename = "R18.glyph_raster")]
    GlyphRaster,
    #[serde(rename = "R19.glyph_atlas")]
    GlyphAtlas,
    #[serde(rename = "R20.line_quad_reuse_rebuild")]
    LineQuadReuseRebuild,
    #[serde(rename = "R21.gpu_bind")]
    GpuBind,
    #[serde(rename = "R22.gpu_upload")]
    GpuUpload,
    #[serde(rename = "R23.gpu_submit")]
    GpuSubmit,
    #[serde(rename = "R24.drawable_present_request")]
    DrawablePresentRequest,
    #[serde(rename = "R25.display_completion")]
    DisplayCompletion,
}

impl RendererResizeTraceStage {
    pub const ALL: [Self; 26] = [
        Self::NativeEventReceipt,
        Self::GuiReturn,
        Self::IntentEnqueue,
        Self::MuxResizeDispatch,
        Self::PaneResizeApply,
        Self::IntentSupersession,
        Self::WorkerCreate,
        Self::WorkerStart,
        Self::TerminalLockWait,
        Self::TerminalLockHold,
        Self::ViewportReflow,
        Self::NearReflow,
        Self::ColdReflow,
        Self::FirstCoherentViewport,
        Self::WorkerJoin,
        Self::GuiInvalidation,
        Self::Paint,
        Self::TextShaping,
        Self::GlyphRaster,
        Self::GlyphAtlas,
        Self::LineQuadReuseRebuild,
        Self::GpuBind,
        Self::GpuUpload,
        Self::GpuSubmit,
        Self::DrawablePresentRequest,
        Self::DisplayCompletion,
    ];
}

/// Scenario-local measurement endpoints without a measurement verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererMeasurementBinding {
    FirstCorrectViewport {
        overlay_id: RendererCoverageOverlayId,
        mutation_event_ordinal: u32,
        target_selection: RendererObservedFrameSelection,
        target_presented_frame_predicate_ref: String,
        required_stage_ids: Vec<RendererResizeTraceStage>,
        observed_boundary: RendererObservedBoundaryClass,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    SteadyPresentedFps {
        overlay_id: RendererCoverageOverlayId,
        interval_start_event_ordinal: u32,
        interval_end_event_ordinal: u32,
        minimum_presented_interval_count: u32,
        presented_interval_contract_ref: String,
        required_stage_ids: Vec<RendererResizeTraceStage>,
        observed_boundary: RendererObservedBoundaryClass,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    ColdReflowConvergence {
        overlay_id: RendererCoverageOverlayId,
        trigger_event_ordinal: u32,
        target_checkpoint_id: String,
        target_selection: RendererObservedFrameSelection,
        target_presented_frame_predicate_ref: String,
        preconditioning_profile_id: RendererPreconditioningProfileId,
        required_stage_ids: Vec<RendererResizeTraceStage>,
        observed_boundary: RendererObservedBoundaryClass,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    SnapBack {
        overlay_id: RendererCoverageOverlayId,
        last_draft_checkpoint_id: String,
        standard_snap_back_subject_checkpoint_id: String,
        independent_standard_oracle_ref: String,
        target_selection: RendererObservedFrameSelection,
        target_presented_frame_predicate_ref: String,
        required_stage_ids: Vec<RendererResizeTraceStage>,
        observed_boundary: RendererObservedBoundaryClass,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    KeypressToFirstCorrectPresent {
        overlay_id: RendererCoverageOverlayId,
        key_event_id: String,
        key_action_event_ordinal: u32,
        target_selection: RendererObservedFrameSelection,
        target_presented_frame_predicate_ref: String,
        expected_terminal_effect_oracle_ref: String,
        stage_metrics_contract_ref: String,
        required_stage_ids: Vec<RendererKeypressTraceStage>,
        observed_boundary: RendererObservedBoundaryClass,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
}

impl RendererMeasurementBinding {
    const fn role(&self) -> RendererMeasurementRole {
        match self {
            Self::FirstCorrectViewport { .. } => RendererMeasurementRole::FirstCorrectViewport,
            Self::SteadyPresentedFps { .. } => RendererMeasurementRole::SteadyPresentedFps,
            Self::ColdReflowConvergence { .. } => {
                RendererMeasurementRole::ColdReflowConvergence
            }
            Self::SnapBack { .. } => RendererMeasurementRole::SnapBack,
            Self::KeypressToFirstCorrectPresent { .. } => {
                RendererMeasurementRole::KeypressToFirstCorrectPresent
            }
        }
    }

    const fn overlay_id(&self) -> RendererCoverageOverlayId {
        match self {
            Self::FirstCorrectViewport { overlay_id, .. }
            | Self::SteadyPresentedFps { overlay_id, .. }
            | Self::ColdReflowConvergence { overlay_id, .. }
            | Self::SnapBack { overlay_id, .. }
            | Self::KeypressToFirstCorrectPresent { overlay_id, .. } => *overlay_id,
        }
    }
}

/// Closed non-qualifying native-driver canary identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererDriverCanaryId {
    FocusWindow,
    ActivateTab,
    FocusPane,
    SplitGeometry,
    TopologyManifest,
}

impl RendererDriverCanaryId {
    pub const ALL: [Self; 5] = [
        Self::FocusWindow,
        Self::ActivateTab,
        Self::FocusPane,
        Self::SplitGeometry,
        Self::TopologyManifest,
    ];
}

/// Exact driver-canary action; never a primary-gesture mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererDriverCanaryAction {
    FocusWindow { target_window_ordinal: u16 },
    ActivateTab {
        target_window_ordinal: u16,
        target_tab_ordinal: u16,
    },
    FocusPane { target_pane_ordinal: u16 },
    SetSplitGeometry { split_geometry_ref: String },
    SetTopologyManifest { topology_ref: String },
}

/// Root canary definition referenced by scenarios.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererDriverCanaryDefinition {
    pub canary_id: RendererDriverCanaryId,
    pub action: RendererDriverCanaryAction,
    pub expected_observed_event_ref: String,
    pub expected_state_ref: String,
    pub timeout_us: u64,
    pub maximum_geometry_delta_px: u32,
    pub prerequisite_capabilities: Vec<RendererCapability>,
    pub minimum_window_count: u16,
    pub minimum_tab_count: u16,
    pub minimum_pane_count: u16,
}

/// Scope of one detector assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererDetectorScope {
    CheckpointOraclePair,
    Interval,
    WholeTimeline,
    AllObservedFrames,
}

/// Explicit non-local detector endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererDetectorBinding {
    CheckpointOraclePair {
        overlay_id: RendererCoverageOverlayId,
        detector_id: RendererCheckpointDetectorId,
        subject_checkpoint_id: String,
        independent_oracle_ref: String,
        comparator_policy_ref: String,
    },
    Interval {
        overlay_id: RendererCoverageOverlayId,
        detector_id: RendererCheckpointDetectorId,
        start_checkpoint_id: String,
        end_checkpoint_id: String,
    },
    WholeTimeline {
        overlay_id: RendererCoverageOverlayId,
        detector_id: RendererCheckpointDetectorId,
    },
}

impl RendererDetectorBinding {
    const fn detector_id(&self) -> RendererCheckpointDetectorId {
        match self {
            Self::CheckpointOraclePair { detector_id, .. }
            | Self::Interval { detector_id, .. }
            | Self::WholeTimeline { detector_id, .. } => *detector_id,
        }
    }

    const fn overlay_id(&self) -> RendererCoverageOverlayId {
        match self {
            Self::CheckpointOraclePair { overlay_id, .. }
            | Self::Interval { overlay_id, .. }
            | Self::WholeTimeline { overlay_id, .. } => *overlay_id,
        }
    }
}

/// Identity carried by every observed/captured/presented frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererObservedFrameIdentityField {
    TimelineEventOrdinal,
    TimelinePhase,
    RendererGeneration,
    GridRevision,
    TerminalRevision,
    PresentationBoundary,
    ActualRefreshMillihz,
    VrrState,
}

impl RendererObservedFrameIdentityField {
    pub const ALL: [Self; 8] = [
        Self::TimelineEventOrdinal,
        Self::TimelinePhase,
        Self::RendererGeneration,
        Self::GridRevision,
        Self::TerminalRevision,
        Self::PresentationBoundary,
        Self::ActualRefreshMillihz,
        Self::VrrState,
    ];
}

/// Frame/boundary classes that must remain correlation-linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererObservedFrameClass {
    RendererProduced,
    MetalDrawable,
    SoftwarePresented,
    DisplayPresented,
}

impl RendererObservedFrameClass {
    pub const ALL: [Self; 4] = [
        Self::RendererProduced,
        Self::MetalDrawable,
        Self::SoftwarePresented,
        Self::DisplayPresented,
    ];
}

/// Whole-stream transient observation requirement; checkpoints are anchors only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererObservedFramePolicy {
    pub overlay_id: RendererCoverageOverlayId,
    pub observation_policy_ref: String,
    pub start_event_ordinal: u32,
    pub end_event_ordinal: u32,
    pub required_frame_classes: Vec<RendererObservedFrameClass>,
    pub require_monotonic_correlation: bool,
    pub detect_dropped_correlations: bool,
    pub detect_duplicate_correlations: bool,
    pub identity_fields: Vec<RendererObservedFrameIdentityField>,
    pub all_frame_detector_ids: Vec<RendererCheckpointDetectorId>,
}

/// Strict downstream run-log, stage-metric, and production-path contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererRunObservationContracts {
    pub measurement_contract_id: String,
    pub observed_frame_stream_ref: String,
    pub stage_metrics_contract_ref: String,
    pub production_path_receipt_contract_ref: String,
    pub run_log_schema_ref: String,
    /// Every run/measurement/artifact identity must include overlay and profile revisions.
    pub require_overlay_and_profile_revision_identity: bool,
    pub resize_stage_ids: Vec<RendererResizeTraceStage>,
    pub keypress_stage_ids: Vec<RendererKeypressTraceStage>,
}

/// Optional exact synthetic RQ-S1 substrate, distinct from the native matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererRqS1SyntheticSubstrate {
    pub benchmark_ref: String,
    pub frame_count: u16,
    pub low_columns: u16,
    pub high_columns: u16,
    pub dirty_rows_per_frame: u16,
    pub gesture_duration_us: u64,
}

/// One exact gesture-by-fleet scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererScenarioDefinition {
    /// Must equal [`expected_renderer_scenario_id`] for this coverage cell.
    pub scenario_id: String,
    /// Closed gesture identity.
    pub gesture: RendererGesture,
    /// Exact fleet point.
    pub fleet_point: RendererFleetPoint,
    /// Required production execution path; never inferred from headless sources.
    pub required_path_class: RendererExecutionPathClass,
    /// Exact pane count; validated against `fleet_point`.
    pub pane_count: u16,
    /// Exact tab count; validated against `fleet_point`.
    pub tab_count: u16,
    /// Exact window count; validated against `fleet_point`.
    pub window_count: u16,
    /// Exact eight reusable overlay-profile IDs in canonical overlay order.
    pub coverage_overlay_profile_ids: Vec<String>,
    /// Must equal [`expected_renderer_scenario_seed`] for this coverage cell.
    ///
    /// JSON uses fixed-width lowercase hexadecimal so IEEE-754-only consumers
    /// cannot silently round or collapse this 64-bit identity.
    #[serde(with = "renderer_seed_wire")]
    pub seed: u64,
    /// Reference to a deterministic workload definition.
    pub workload_id: String,
    /// Exact, typed requirement scope crosswalk for this scenario.
    pub requirement_bindings: Vec<RendererRequirementBinding>,
    /// Required only for output-overlap resize; forbidden otherwise.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub output_overlap_resize_mode: Option<RendererResizeMode>,
    /// Configured post-snap steady mode; Draft is forbidden here.
    pub configured_steady_quality: RendererQualityMode,
    /// Replayable, zero-based, strictly ordered gesture timeline.
    pub timeline: Vec<RendererTimelineEvent>,
    /// Expected intermediate invariants.
    pub expected_invariants: Vec<RendererExpectedInvariant>,
    /// State, visual, and accessibility checkpoints.
    pub visual_checkpoints: Vec<RendererVisualCheckpoint>,
    /// One continuous frame-observation policy per required overlay.
    pub observed_frame_policies: Vec<RendererObservedFramePolicy>,
    /// Pair/interval/whole-timeline detector assertions.
    pub detector_bindings: Vec<RendererDetectorBinding>,
    /// Downstream measurement endpoint vocabulary.
    pub measurement_bindings: Vec<RendererMeasurementBinding>,
    /// Exact cadence profiles requested without multiplying matrix cells.
    pub presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    /// Exact cold/warm/aged precondition profiles requested for this cell.
    pub preconditioning_profile_ids: Vec<RendererPreconditioningProfileId>,
    /// Non-qualifying driver canaries kept separate from the primary gesture.
    pub driver_canary_ids: Vec<RendererDriverCanaryId>,
    /// Exact capability matrix; unsupported entries require reasons.
    pub capabilities: Vec<RendererCapabilityBinding>,
}

/// Root version-1 renderer scenario catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererScenarioCatalog {
    /// Must equal [`RENDERER_SCENARIO_CONTRACT_ID`].
    pub contract_id: String,
    /// Must equal [`RENDERER_SCENARIO_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Positive catalog content revision.
    pub catalog_revision: u32,
    /// Must equal [`RENDERER_SCENARIO_SOURCE_BEAD_ID`].
    pub source_bead_id: String,
    /// Closed contract-only authority.
    pub authority: RendererCatalogAuthority,
    /// Canonical state, visual-comparator, accessibility, and checkpoint policy.
    pub oracle_contracts: RendererOracleContracts,
    /// Strict frame-stream, metrics, production-path, and run-log vocabularies.
    pub run_observation_contracts: RendererRunObservationContracts,
    /// Machine-geometry and native-product accessibility authority separation.
    pub accessibility_authority_boundary: RendererAccessibilityAuthorityBoundary,
    /// Terminal-content inputs; no evidence authority is carried here.
    pub content_corpus_references: Vec<RendererContentCorpusReference>,
    /// Headless/metamorphic/contract evidence sources, separate from content.
    pub evidence_sources: Vec<RendererEvidenceSource>,
    /// Exact 13-feature evidence truth, separate from content presence.
    pub feature_evidence_bindings: Vec<RendererFeatureEvidenceBinding>,
    /// Deterministic workload identities.
    pub workloads: Vec<RendererWorkloadDefinition>,
    /// Pinned bundled-default and feature-maximal renderer configurations.
    pub renderer_config_profiles: Vec<RendererConfigProfile>,
    /// Exact four normalized layout profiles, one per fleet point.
    pub layout_profiles: Vec<RendererLayoutProfile>,
    /// Deduplicated complete pane-surface states.
    pub surface_state_templates: Vec<RendererSurfaceStateTemplate>,
    /// Fully typed content distributions used by phase manifests.
    pub content_distribution_profiles: Vec<RendererContentDistributionProfile>,
    /// Fully resolvable phase manifests referenced by scenario events/checkpoints.
    pub phase_manifests: Vec<RendererPhaseManifest>,
    /// Exactly eight reusable global templates cross-applied by scenario bindings.
    pub coverage_overlay_profiles: Vec<RendererCoverageOverlayProfile>,
    /// Exact detector mechanism inventory.
    pub detector_contracts: Vec<RendererDetectorContract>,
    /// Fixed 60/120 and variable-refresh request profiles.
    pub presentation_target_profiles: Vec<RendererPresentationTargetProfile>,
    /// Exact cold, warm, and aged session/cache precondition profiles.
    pub preconditioning_profiles: Vec<RendererPreconditioningProfile>,
    /// Exact five non-qualifying driver-canary definitions.
    pub driver_canaries: Vec<RendererDriverCanaryDefinition>,
    /// Optional synthetic RQ-S1 substrate, never inferred from native cells.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub rq_s1_synthetic_substrate: Option<RendererRqS1SyntheticSubstrate>,
    /// Exact eight-row gesture authority classification.
    pub gesture_authority_map: Vec<RendererGestureAuthorityEntry>,
    /// Explicit non-qualifying mappings from canonical legacy sources.
    pub legacy_mappings: Vec<RendererLegacyScenarioMapping>,
    /// Exact canonical deliberate-defect control catalog.
    pub negative_controls: Vec<RendererNegativeControlDefinition>,
    /// Exact 8-by-4 scenario coverage matrix.
    pub scenarios: Vec<RendererScenarioDefinition>,
}

impl RendererScenarioCatalog {
    /// Decode one bounded JSON document and reject unknown or trailing data.
    pub fn decode_json_bounded(raw: &[u8]) -> Result<Self, RendererScenarioDecodeError> {
        decode_renderer_scenario_catalog(raw)
    }

    /// Validate all semantic invariants not expressed by Serde's closed shape.
    #[must_use]
    pub fn validate(&self) -> RendererScenarioValidationReport {
        validate_renderer_scenario_catalog(self)
    }
}

/// Return the only valid scenario identity for a gesture/fleet coverage cell.
#[must_use]
pub fn expected_renderer_scenario_id(
    gesture: RendererGesture,
    fleet_point: RendererFleetPoint,
) -> String {
    format!(
        "renderer.{}.{}",
        gesture.as_str(),
        fleet_point.as_str()
    )
}

/// Return the deterministic, collision-free version-1 seed for a coverage cell.
///
/// The high 32 bits are ASCII `FTRS`; the low bits bind the gesture ordinal and
/// exact pane count. This deliberately avoids platform hash implementations.
#[must_use]
pub const fn expected_renderer_scenario_seed(
    gesture: RendererGesture,
    fleet_point: RendererFleetPoint,
) -> u64 {
    0x4654_5253_0000_0000
        | (gesture.seed_discriminant() << 16)
        | fleet_point.pane_count() as u64
}

/// Stable bounded-decoder failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererScenarioDecodeCode {
    /// Raw bytes exceed the catalog limit.
    PayloadTooLarge,
    /// The first JSON value is malformed or has the wrong closed shape.
    InvalidJson,
    /// Non-whitespace data follows the first valid catalog value.
    TrailingData,
}

impl RendererScenarioDecodeCode {
    /// Stable machine-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PayloadTooLarge => "RSC-DECODE-001",
            Self::InvalidJson => "RSC-DECODE-002",
            Self::TrailingData => "RSC-DECODE-003",
        }
    }
}

/// Failure returned by [`decode_renderer_scenario_catalog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererScenarioDecodeError {
    /// Raw input exceeds the pre-deserialization limit.
    PayloadTooLarge {
        /// Observed byte count.
        actual_bytes: usize,
        /// Configured maximum byte count.
        max_bytes: usize,
    },
    /// Serde rejected malformed JSON, an unknown field, or a wrong type/value.
    InvalidJson {
        /// Parser diagnostic; callers should route on [`Self::code`].
        detail: String,
    },
    /// A valid first value was followed by non-whitespace data.
    TrailingData {
        /// Parser diagnostic; callers should route on [`Self::code`].
        detail: String,
    },
}

impl RendererScenarioDecodeError {
    /// Stable decoder category.
    #[must_use]
    pub const fn code(&self) -> RendererScenarioDecodeCode {
        match self {
            Self::PayloadTooLarge { .. } => RendererScenarioDecodeCode::PayloadTooLarge,
            Self::InvalidJson { .. } => RendererScenarioDecodeCode::InvalidJson,
            Self::TrailingData { .. } => RendererScenarioDecodeCode::TrailingData,
        }
    }
}

impl fmt::Display for RendererScenarioDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge {
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "{}: renderer scenario catalog is {actual_bytes} bytes (maximum {max_bytes})",
                self.code().as_str()
            ),
            Self::InvalidJson { detail } | Self::TrailingData { detail } => {
                write!(formatter, "{}: {detail}", self.code().as_str())
            }
        }
    }
}

impl Error for RendererScenarioDecodeError {}

/// Decode one bounded, closed-shape JSON catalog.
///
/// The byte limit is checked before Serde allocates catalog vectors. Serde's
/// `deny_unknown_fields` annotations reject schema drift, and `end()` rejects
/// any second value or other trailing non-whitespace input.
pub fn decode_renderer_scenario_catalog(
    raw: &[u8],
) -> Result<RendererScenarioCatalog, RendererScenarioDecodeError> {
    if raw.len() > MAX_RENDERER_SCENARIO_CATALOG_BYTES {
        return Err(RendererScenarioDecodeError::PayloadTooLarge {
            actual_bytes: raw.len(),
            max_bytes: MAX_RENDERER_SCENARIO_CATALOG_BYTES,
        });
    }

    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let catalog = RendererScenarioCatalog::deserialize(&mut decoder).map_err(|error| {
        RendererScenarioDecodeError::InvalidJson {
            detail: error.to_string(),
        }
    })?;
    decoder
        .end()
        .map_err(|error| RendererScenarioDecodeError::TrailingData {
            detail: error.to_string(),
        })?;
    Ok(catalog)
}

/// Stable semantic validation category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererScenarioValidationCode {
    /// Contract identifier differs from version 1.
    UnknownContract,
    /// Schema version differs from version 1.
    UnknownSchemaVersion,
    /// Catalog revision differs from the exact canonical revision.
    UnknownCatalogRevision,
    /// Source bead identity differs from the contract owner.
    UnknownSourceBead,
    /// A required field or collection is empty.
    EmptyRequiredField,
    /// A collection or scalar exceeds a semantic limit.
    LimitExceeded,
    /// A stable identifier is malformed.
    InvalidIdentifier,
    /// A stable identifier appears more than once.
    DuplicateId,
    /// A relative repository reference is malformed or traversing.
    MalformedRepositoryReference,
    /// A workload, corpus, or IME reference is undefined.
    DanglingReference,
    /// A declared workload or corpus definition is unused.
    UnreferencedDefinition,
    /// A corpus definition is internally invalid.
    InvalidCorpusReference,
    /// The exact gesture-authority map is malformed or over-authoritative.
    InvalidGestureAuthority,
    /// A legacy scenario mapping is missing or malformed.
    InvalidLegacyMapping,
    /// A canonical negative control is missing or malformed.
    InvalidNegativeControl,
    /// A workload definition is internally invalid.
    InvalidWorkload,
    /// The active corpus union omits required terminal features.
    MissingTerminalFeatureCoverage,
    /// A scenario seed differs from its deterministic derived value.
    InvalidSeed,
    /// A gesture/fleet cell appears more than once.
    DuplicateCoverageCell,
    /// One or more of the exact 32 cells are absent.
    MissingRequiredCoverage,
    /// Typed RQ scope differs from the exact gesture/fleet crosswalk.
    InvalidRequirementCrosswalk,
    /// A timeline is not contiguous, strictly ordered, or replayable.
    InvalidTimeline,
    /// Initial, intermediate, or final state is malformed or inconsistent.
    InvalidState,
    /// A gesture's required state transition is absent or contradictory.
    InvalidGestureTransition,
    /// An invariant binding is malformed.
    InvalidInvariant,
    /// A checkpoint binding is malformed.
    InvalidCheckpoint,
    /// A capability row is duplicated, omitted, or malformed.
    InvalidCapabilityMatrix,
    /// An unsupported capability has no actionable reason.
    MissingUnsupportedCapabilityReason,
    /// Output-overlap does not use exactly one decimal MB/s.
    InvalidOutputOverlapRate,
}

impl RendererScenarioValidationCode {
    /// Stable machine-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownContract => "RSC-CONTRACT-001",
            Self::UnknownSchemaVersion => "RSC-SCHEMA-001",
            Self::UnknownCatalogRevision => "RSC-SCHEMA-003",
            Self::UnknownSourceBead => "RSC-CONTRACT-002",
            Self::EmptyRequiredField => "RSC-SCHEMA-002",
            Self::LimitExceeded => "RSC-BOUNDS-001",
            Self::InvalidIdentifier => "RSC-IDENTITY-001",
            Self::DuplicateId => "RSC-IDENTITY-002",
            Self::MalformedRepositoryReference => "RSC-REFERENCE-001",
            Self::DanglingReference => "RSC-REFERENCE-002",
            Self::UnreferencedDefinition => "RSC-REFERENCE-003",
            Self::InvalidCorpusReference => "RSC-CORPUS-001",
            Self::InvalidGestureAuthority => "RSC-AUTHORITY-001",
            Self::InvalidLegacyMapping => "RSC-LEGACY-001",
            Self::InvalidNegativeControl => "RSC-CONTROL-CATALOG-001",
            Self::InvalidWorkload => "RSC-WORKLOAD-001",
            Self::MissingTerminalFeatureCoverage => "RSC-COVERAGE-001",
            Self::InvalidSeed => "RSC-DETERMINISM-001",
            Self::DuplicateCoverageCell => "RSC-COVERAGE-002",
            Self::MissingRequiredCoverage => "RSC-COVERAGE-003",
            Self::InvalidRequirementCrosswalk => "RSC-CROSSWALK-001",
            Self::InvalidTimeline => "RSC-TIMELINE-001",
            Self::InvalidState => "RSC-STATE-001",
            Self::InvalidGestureTransition => "RSC-GESTURE-001",
            Self::InvalidInvariant => "RSC-INVARIANT-001",
            Self::InvalidCheckpoint => "RSC-CHECKPOINT-001",
            Self::InvalidCapabilityMatrix => "RSC-CAPABILITY-001",
            Self::MissingUnsupportedCapabilityReason => "RSC-CAPABILITY-002",
            Self::InvalidOutputOverlapRate => "RSC-OUTPUT-001",
        }
    }
}

/// One actionable semantic validation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererScenarioValidationError {
    /// Stable error category.
    pub code: RendererScenarioValidationCode,
    /// JSON-style path of the failing field.
    pub path: String,
    /// Stable, human-readable diagnostic.
    pub detail: String,
}

/// Stable non-green gap category that does not make the contract malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererScenarioGapCode {
    /// A corpus mapping is partial, absent, or present-but-unqualified.
    CorpusCoverageNotDirect,
    /// A gesture's current canonical sources do not provide direct coverage.
    GestureCoverageNotDirect,
    /// A semantically required capability is explicitly unavailable.
    RequiredCapabilityUnavailable,
    /// A native checkpoint cannot currently obtain its required capture.
    NativeCaptureUnavailable,
    /// Deterministic content bytes/state cannot currently be materialized.
    ContentMaterializationUnavailable,
    /// A required renderer configuration is not available.
    RendererConfigurationUnavailable,
    /// The pinned output stream lacks a closed implementation and identity manifest.
    DeterministicOutputStreamUnavailable,
    /// Foreground keypress effect lacks a PTY echo and terminal-state oracle.
    KeyEffectOracleUnavailable,
}

impl RendererScenarioGapCode {
    /// Stable machine-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CorpusCoverageNotDirect => "RSC-GAP-CORPUS-001",
            Self::GestureCoverageNotDirect => "RSC-GAP-GESTURE-001",
            Self::RequiredCapabilityUnavailable => "RSC-GAP-CAPABILITY-001",
            Self::NativeCaptureUnavailable => "RSC-GAP-CAPTURE-001",
            Self::ContentMaterializationUnavailable => "RSC-GAP-CONTENT-001",
            Self::RendererConfigurationUnavailable => "RSC-GAP-CONFIG-001",
            Self::DeterministicOutputStreamUnavailable => "RSC-GAP-OUTPUT-001",
            Self::KeyEffectOracleUnavailable => "RSC-GAP-KEY-EFFECT-001",
        }
    }
}

/// One explicit, tracked gap in an otherwise structurally valid contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererScenarioValidationGap {
    /// Stable gap category.
    pub code: RendererScenarioGapCode,
    /// JSON-style path of the gap declaration.
    pub path: String,
    /// Human-readable limitation.
    pub detail: String,
    /// Relative repository or Bead-style tracking reference.
    pub tracking_ref: String,
    /// Scenario identity when the gap blocks one overlay execution.
    pub scenario_id: Option<String>,
    /// Overlay identity when the gap blocks one overlay execution.
    pub overlay_id: Option<RendererCoverageOverlayId>,
}

/// Execution readiness for one scenario-overlay pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOverlayReadiness {
    pub scenario_id: String,
    pub overlay_id: RendererCoverageOverlayId,
    pub execution_ready: bool,
    pub blocking_gap_codes: Vec<RendererScenarioGapCode>,
}

/// Deterministically ordered semantic validation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererScenarioValidationReport {
    /// True only when no semantic errors were found.
    pub valid: bool,
    /// Readiness of all 32 bundled production-default cells.
    pub production_default_execution_ready: bool,
    /// Readiness of every required overlay in all 32 cells.
    pub coverage_suite_execution_ready: bool,
    /// Per-cell/per-overlay readiness; required input/config/capability gaps block it.
    pub overlay_readiness: Vec<RendererOverlayReadiness>,
    /// Errors sorted by path, stable code, and detail.
    pub errors: Vec<RendererScenarioValidationError>,
    /// Explicit non-green gaps sorted by path, stable code, and detail.
    pub gaps: Vec<RendererScenarioValidationGap>,
}

/// Deterministically resolved window state at one checkpoint anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResolvedWindow {
    pub window_ordinal: u16,
    pub window_id: String,
    pub coordinate_space: RendererPixelCoordinateSpace,
    pub drawable_rect: RendererPixelRect,
    pub ordered_tab_ids: Vec<String>,
    pub active_tab_ordinal: u16,
    pub active_tab_id: String,
    pub focused: bool,
}

/// Deterministically resolved tab identity and pane order at one anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResolvedTab {
    pub tab_ordinal: u16,
    pub tab_id: String,
    pub window_ordinal: u16,
    pub window_id: String,
    pub window_local_tab_ordinal: u16,
    pub ordered_pane_ids: Vec<String>,
    pub active: bool,
}

/// Deterministically materialized pane state at one checkpoint anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResolvedPane {
    pub pane_ordinal: u16,
    pub pane_id: String,
    pub window_ordinal: u16,
    pub window_id: String,
    pub tab_ordinal: u16,
    pub tab_id: String,
    pub coordinate_space: RendererPixelCoordinateSpace,
    pub window_content_rect: RendererPixelRect,
    pub split_path: Vec<RendererSplitTreeBranch>,
    pub active_tab: bool,
    pub focused: bool,
    pub surface_state: RendererSurfaceState,
    pub output: RendererPaneOutputState,
    pub applied_materialization_steps: Vec<RendererContentMaterializationStep>,
}

/// One fully resolved checkpoint/manifest anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResolvedCheckpointAnchor {
    pub checkpoint_id: String,
    pub role: RendererCheckpointRole,
    pub phase: RendererTimelinePhase,
    pub event_ordinal: u32,
    pub phase_manifest_id: String,
    /// Complete canonical invariant set applicable to this resolved overlay anchor.
    pub expected_invariant_ids: Vec<String>,
    pub layout_profile_id: String,
    pub layout_stable_id_revision: String,
    pub content_distribution_profile_id: String,
    pub content_distribution_profile_revision: u32,
    pub focused_window_id: String,
    pub focused_pane_id: String,
    pub windows: Vec<RendererResolvedWindow>,
    pub tabs: Vec<RendererResolvedTab>,
    pub panes: Vec<RendererResolvedPane>,
}

/// Public, deterministic expansion of one scenario-overlay execution contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResolvedScenarioOverlay {
    pub contract_id: String,
    pub schema_version: u32,
    pub catalog_revision: u32,
    pub scenario_id: String,
    pub gesture: RendererGesture,
    pub fleet_point: RendererFleetPoint,
    pub workload_id: String,
    pub workload_revision: u32,
    pub overlay_id: RendererCoverageOverlayId,
    pub overlay_profile_id: String,
    pub overlay_profile_revision: u32,
    pub renderer_config_profile_id: String,
    pub renderer_config_profile_revision: u32,
    /// Pair-scoped execution truth; structural resolution does not imply readiness.
    pub execution_ready: bool,
    pub blocking_gap_codes: Vec<RendererScenarioGapCode>,
    pub blocking_gaps: Vec<RendererScenarioValidationGap>,
    pub anchors: Vec<RendererResolvedCheckpointAnchor>,
}

/// Fail-closed deterministic resolver error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererScenarioResolveError {
    InvalidCatalog {
        report: RendererScenarioValidationReport,
    },
    ScenarioNotFound {
        scenario_id: String,
    },
    OverlayNotFound {
        scenario_id: String,
        overlay_id: RendererCoverageOverlayId,
    },
    MissingReference {
        scenario_id: String,
        overlay_id: RendererCoverageOverlayId,
        detail: String,
    },
}

impl fmt::Display for RendererScenarioResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalog { report } => write!(
                formatter,
                "renderer scenario catalog is invalid ({} semantic errors)",
                report.errors.len()
            ),
            Self::ScenarioNotFound { scenario_id } => {
                write!(formatter, "renderer scenario `{scenario_id}` is undefined")
            }
            Self::OverlayNotFound {
                scenario_id,
                overlay_id,
            } => write!(
                formatter,
                "renderer scenario `{scenario_id}` does not select overlay `{}`",
                overlay_id.as_str()
            ),
            Self::MissingReference {
                scenario_id,
                overlay_id,
                detail,
            } => write!(
                formatter,
                "renderer scenario `{scenario_id}` overlay `{}` cannot resolve: {detail}",
                overlay_id.as_str()
            ),
        }
    }
}

impl Error for RendererScenarioResolveError {}

impl RendererScenarioValidationReport {
    /// Whether this report contains a stable validation category.
    #[must_use]
    pub fn contains_code(&self, code: RendererScenarioValidationCode) -> bool {
        self.errors.iter().any(|error| error.code == code)
    }

    /// Whether this report contains a particular non-green gap category.
    #[must_use]
    pub fn contains_gap(&self, code: RendererScenarioGapCode) -> bool {
        self.gaps.iter().any(|gap| gap.code == code)
    }
}

struct Validator {
    errors: Vec<RendererScenarioValidationError>,
    gaps: Vec<RendererScenarioValidationGap>,
    overlay_readiness: Vec<RendererOverlayReadiness>,
}

impl Validator {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            gaps: Vec::new(),
            overlay_readiness: Vec::new(),
        }
    }

    fn error(
        &mut self,
        code: RendererScenarioValidationCode,
        path: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.errors.push(RendererScenarioValidationError {
            code,
            path: path.into(),
            detail: detail.into(),
        });
    }

    fn gap(
        &mut self,
        code: RendererScenarioGapCode,
        path: impl Into<String>,
        detail: impl Into<String>,
        tracking_ref: impl Into<String>,
    ) {
        self.gaps.push(RendererScenarioValidationGap {
            code,
            path: path.into(),
            detail: detail.into(),
            tracking_ref: tracking_ref.into(),
            scenario_id: None,
            overlay_id: None,
        });
    }

    fn overlay_gap(
        &mut self,
        code: RendererScenarioGapCode,
        path: impl Into<String>,
        detail: impl Into<String>,
        tracking_ref: impl Into<String>,
        scenario_id: &str,
        overlay_id: RendererCoverageOverlayId,
    ) {
        self.gaps.push(RendererScenarioValidationGap {
            code,
            path: path.into(),
            detail: detail.into(),
            tracking_ref: tracking_ref.into(),
            scenario_id: Some(scenario_id.to_string()),
            overlay_id: Some(overlay_id),
        });
    }

    fn record_overlay_readiness(
        &mut self,
        scenario_id: &str,
        overlay_id: RendererCoverageOverlayId,
        mut blocking_gap_codes: Vec<RendererScenarioGapCode>,
    ) {
        blocking_gap_codes.sort();
        blocking_gap_codes.dedup();
        self.overlay_readiness.push(RendererOverlayReadiness {
            scenario_id: scenario_id.to_string(),
            overlay_id,
            execution_ready: blocking_gap_codes.is_empty(),
            blocking_gap_codes,
        });
    }

    fn finish(mut self) -> RendererScenarioValidationReport {
        self.errors.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.code.as_str().cmp(right.code.as_str()))
                .then_with(|| left.detail.cmp(&right.detail))
        });
        self.gaps.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.code.as_str().cmp(right.code.as_str()))
                .then_with(|| left.detail.cmp(&right.detail))
                .then_with(|| left.tracking_ref.cmp(&right.tracking_ref))
        });
        self.overlay_readiness.sort_by(|left, right| {
            left.scenario_id
                .cmp(&right.scenario_id)
                .then_with(|| left.overlay_id.cmp(&right.overlay_id))
        });
        let valid = self.errors.is_empty();
        if !valid {
            for readiness in &mut self.overlay_readiness {
                readiness.execution_ready = false;
            }
        }
        let complete_readiness_matrix =
            self.overlay_readiness.len() == REQUIRED_RENDERER_SCENARIO_COUNT * 8;
        let production_default_execution_ready = valid
            && complete_readiness_matrix
            && self.overlay_readiness.iter().all(|entry| {
                entry.overlay_id != RendererCoverageOverlayId::ProductionDefault
                    || entry.execution_ready
            });
        let coverage_suite_execution_ready = valid
            && complete_readiness_matrix
            && self
                .overlay_readiness
                .iter()
                .all(|entry| entry.execution_ready);
        RendererScenarioValidationReport {
            valid,
            production_default_execution_ready,
            coverage_suite_execution_ready,
            overlay_readiness: self.overlay_readiness,
            errors: self.errors,
            gaps: self.gaps,
        }
    }

    fn require_identifier(&mut self, path: &str, value: &str) {
        if let Err(detail) = validate_identifier(value) {
            self.error(
                RendererScenarioValidationCode::InvalidIdentifier,
                path,
                detail,
            );
        }
    }

    fn require_repository_ref(&mut self, path: &str, value: &str) {
        if let Err(detail) = validate_renderer_repository_reference(value) {
            self.error(
                RendererScenarioValidationCode::MalformedRepositoryReference,
                path,
                detail,
            );
        }
    }
}

fn validate_identifier(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("identifier must not be empty".to_string());
    }
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(format!(
            "identifier is {} bytes (maximum {MAX_IDENTIFIER_BYTES})",
            value.len()
        ));
    }
    if value.trim() != value {
        return Err("identifier must not contain leading or trailing whitespace".to_string());
    }

    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err("identifier must not be empty".to_string());
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err("identifier must start with a lowercase ASCII letter or digit".to_string());
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'-' | b'_')
    }) {
        return Err(
            "identifier may contain only lowercase ASCII letters, digits, '.', '-', and '_'"
                .to_string(),
        );
    }
    if matches!(value.as_bytes().last(), Some(b'.' | b'-' | b'_')) {
        return Err("identifier must not end with punctuation".to_string());
    }
    Ok(())
}

fn validate_sha256_hex(path: &str, value: &str, validator: &mut Validator) {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            path,
            "SHA-256 must be exactly 64 lowercase hexadecimal characters",
        );
    }
}

fn validate_tracked_limitation(
    path: &str,
    reason: &str,
    tracking_refs: &[String],
    code: RendererScenarioValidationCode,
    validator: &mut Validator,
) {
    if reason.trim().is_empty() {
        validator.error(code, format!("{path}.reason"), "reason must not be empty");
    } else if reason.len() > MAX_REASON_BYTES {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            format!("{path}.reason"),
            format!("reason is {} bytes (maximum {MAX_REASON_BYTES})", reason.len()),
        );
    }
    if tracking_refs.is_empty() {
        validator.error(
            code,
            format!("{path}.tracking_refs"),
            "at least one tracking reference is required",
        );
    }
    let mut seen = BTreeSet::new();
    for (position, tracking_ref) in tracking_refs.iter().enumerate() {
        let reference_path = format!("{path}.tracking_refs[{position}]");
        validator.require_repository_ref(&reference_path, tracking_ref);
        if !seen.insert(tracking_ref.as_str()) {
            validator.error(
                code,
                reference_path,
                format!("duplicate tracking reference `{tracking_ref}`"),
            );
        }
    }
}

fn validate_content_payload_selector(
    path: &str,
    selector: &RendererContentPayloadSelector,
    encoding: RendererContentEncoding,
    validator: &mut Validator,
) {
    match selector {
        RendererContentPayloadSelector::WholePayload => {
            if encoding == RendererContentEncoding::HexTranscriptV1 {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    path,
                    "hex_transcript_v1 requires an exact manifest-row segment",
                );
            }
        }
        RendererContentPayloadSelector::ManifestRowSegment {
            manifest_ref,
            manifest_row_id,
            decoded_byte_start,
            decoded_byte_end_exclusive,
        } => {
            validator.require_repository_ref(&format!("{path}.manifest_ref"), manifest_ref);
            validator.require_identifier(&format!("{path}.manifest_row_id"), manifest_row_id);
            if decoded_byte_end_exclusive <= decoded_byte_start {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    path,
                    "decoded byte range must be non-empty and increasing",
                );
            }
            if encoding != RendererContentEncoding::HexTranscriptV1 {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    path,
                    "manifest-row segments are reserved for hex_transcript_v1",
                );
            }
        }
    }
}

fn validate_content_decoder(
    path: &str,
    encoding: RendererContentEncoding,
    decoder: RendererContentDecoder,
    validator: &mut Validator,
) {
    let expected = match encoding {
        RendererContentEncoding::RawTerminalBytes => RendererContentDecoder::Identity,
        RendererContentEncoding::Utf8Text => RendererContentDecoder::Utf8ValidateV1,
        RendererContentEncoding::HexTranscriptV1 => RendererContentDecoder::HexDecodeV1,
        RendererContentEncoding::GpuFixtureStateV1 => {
            RendererContentDecoder::JsonFixtureStateV1
        }
        RendererContentEncoding::GeneratedTerminalBytesV1 => RendererContentDecoder::GeneratorV1,
        RendererContentEncoding::GeneratedTypedStateV1 => RendererContentDecoder::GeneratorV1,
    };
    if decoder != expected {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            path,
            format!("encoding {encoding:?} requires decoder {expected:?}"),
        );
    }
}

fn validate_content_framing(
    path: &str,
    encoding: RendererContentEncoding,
    framing: RendererContentFraming,
    validator: &mut Validator,
) {
    let valid = match encoding {
        RendererContentEncoding::RawTerminalBytes => matches!(
            framing,
            RendererContentFraming::CompleteTerminalStream
                | RendererContentFraming::KittyGraphicsApcBodyV1
                | RendererContentFraming::SixelDcsBodyV1
        ),
        RendererContentEncoding::Utf8Text => framing == RendererContentFraming::Utf8Text,
        RendererContentEncoding::HexTranscriptV1 => {
            framing == RendererContentFraming::CompleteTerminalStream
        }
        RendererContentEncoding::GpuFixtureStateV1
        | RendererContentEncoding::GeneratedTypedStateV1 => {
            framing == RendererContentFraming::TypedStateOverlay
        }
        RendererContentEncoding::GeneratedTerminalBytesV1 => matches!(
            framing,
            RendererContentFraming::CompleteTerminalStream
                | RendererContentFraming::Utf8Text
                | RendererContentFraming::KittyGraphicsApcBodyV1
                | RendererContentFraming::SixelDcsBodyV1
        ),
    };
    if !valid {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            path,
            format!("encoding {encoding:?} is incompatible with framing {framing:?}"),
        );
    }
}

/// Validate a repository-relative reference without performing file I/O.
///
/// An optional `#fragment` is accepted. Absolute paths, URI/drive prefixes,
/// backslashes, queries, empty path components, and `.`/`..` traversal are
/// rejected. Integration tests remain responsible for existence and corpus-ID
/// reconciliation against the checked-out repository.
pub fn validate_renderer_repository_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty() {
        return Err("repository reference must not be empty".to_string());
    }
    if reference.len() > MAX_REPOSITORY_REFERENCE_BYTES {
        return Err(format!(
            "repository reference is {} bytes (maximum {MAX_REPOSITORY_REFERENCE_BYTES})",
            reference.len()
        ));
    }
    if reference.trim() != reference {
        return Err(
            "repository reference must not contain leading or trailing whitespace".to_string(),
        );
    }
    if !reference.is_ascii() || reference.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("repository reference must contain printable ASCII only".to_string());
    }
    if reference.contains('\\') {
        return Err("repository reference must use '/' separators".to_string());
    }
    if reference.contains('?') || reference.contains(':') {
        return Err("repository reference must not be a query, URI, or drive path".to_string());
    }

    let mut parts = reference.split('#');
    let path = parts.next().unwrap_or_default();
    let fragment = parts.next();
    if parts.next().is_some() {
        return Err("repository reference must contain at most one '#' fragment".to_string());
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err("repository path must be relative and must name a file or entry".to_string());
    }
    if path.is_empty() {
        return Err("repository path must not be empty".to_string());
    }
    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(
                "repository path must not contain empty, '.', or '..' components".to_string(),
            );
        }
        if component.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err("repository path components must not contain whitespace".to_string());
        }
    }
    if let Some(fragment) = fragment {
        if fragment.is_empty() {
            return Err("repository fragment must not be empty".to_string());
        }
        if fragment.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err("repository fragment must not contain whitespace".to_string());
        }
    }
    Ok(())
}

#[derive(Debug)]
struct CatalogIndex<'a> {
    scenarios: BTreeMap<&'a str, &'a RendererScenarioDefinition>,
    content: BTreeMap<&'a str, &'a RendererContentCorpusReference>,
    evidence: BTreeMap<&'a str, &'a RendererEvidenceSource>,
    workloads: BTreeMap<&'a str, &'a RendererWorkloadDefinition>,
    renderer_config_profiles: BTreeMap<&'a str, &'a RendererConfigProfile>,
    layout_profiles: BTreeMap<&'a str, &'a RendererLayoutProfile>,
    surface_state_templates: BTreeMap<&'a str, &'a RendererSurfaceStateTemplate>,
    content_distribution_profiles:
        BTreeMap<&'a str, &'a RendererContentDistributionProfile>,
    phase_manifests: BTreeMap<&'a str, &'a RendererPhaseManifest>,
    coverage_overlay_profiles:
        BTreeMap<&'a str, &'a RendererCoverageOverlayProfile>,
    detector_contracts:
        BTreeMap<RendererCheckpointDetectorId, &'a RendererDetectorContract>,
    presentation_profiles:
        BTreeMap<RendererPresentationTargetProfileId, &'a RendererPresentationTargetProfile>,
    preconditioning_profiles:
        BTreeMap<RendererPreconditioningProfileId, &'a RendererPreconditioningProfile>,
    driver_canaries: BTreeMap<RendererDriverCanaryId, &'a RendererDriverCanaryDefinition>,
}

/// Auditable work performed while constructing one prepared resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResolverPreparationStats {
    /// Number of complete semantic validation passes performed.
    pub semantic_validation_passes: u32,
    /// Number of complete reusable catalog indexes constructed.
    pub index_builds: u32,
}

/// Borrowed, validated renderer catalog with reusable lookup indexes.
///
/// Construction is fail-closed and performs exactly one semantic validation
/// pass while constructing one reusable resolver index. Resolution is
/// immutable: this handle owns no mutable or global cache, so repeated pair
/// and batch results remain deterministic.
#[derive(Debug)]
pub struct RendererPreparedScenarioCatalog<'a> {
    catalog: &'a RendererScenarioCatalog,
    index: CatalogIndex<'a>,
    report: RendererScenarioValidationReport,
    preparation_stats: RendererResolverPreparationStats,
}

impl RendererPreparedScenarioCatalog<'_> {
    /// Return the fixed construction-work receipt for this prepared handle.
    #[must_use]
    pub const fn preparation_stats(&self) -> RendererResolverPreparationStats {
        self.preparation_stats
    }

    /// Return the validated pair-scoped readiness and gap report.
    #[must_use]
    pub const fn validation_report(&self) -> &RendererScenarioValidationReport {
        &self.report
    }

    /// Resolve one scenario-overlay pair without revalidating or rebuilding indexes.
    pub fn resolve(
        &self,
        scenario_id: &str,
        overlay_id: RendererCoverageOverlayId,
    ) -> Result<RendererResolvedScenarioOverlay, RendererScenarioResolveError> {
        resolve_renderer_scenario_overlay_indexed(
            self.catalog,
            &self.index,
            &self.report,
            scenario_id,
            overlay_id,
        )
    }

    /// Resolve the complete catalog in canonical scenario-major/overlay-minor order.
    ///
    /// Catalog validation guarantees that scenario order is gesture-major then
    /// fleet-minor. Overlay order is [`RendererCoverageOverlayId::ALL`].
    pub fn resolve_all_overlays(
        &self,
    ) -> Result<Vec<RendererResolvedScenarioOverlay>, RendererScenarioResolveError> {
        let mut resolved = Vec::with_capacity(
            self.catalog
                .scenarios
                .len()
                .saturating_mul(RendererCoverageOverlayId::ALL.len()),
        );
        for scenario in &self.catalog.scenarios {
            for overlay_id in RendererCoverageOverlayId::ALL {
                resolved.push(self.resolve(&scenario.scenario_id, overlay_id)?);
            }
        }
        Ok(resolved)
    }
}

/// Validate a renderer scenario catalog without file I/O or claim promotion.
#[must_use]
pub fn validate_renderer_scenario_catalog(
    catalog: &RendererScenarioCatalog,
) -> RendererScenarioValidationReport {
    validate_renderer_scenario_catalog_indexed(catalog).0
}

fn validate_renderer_scenario_catalog_indexed(
    catalog: &RendererScenarioCatalog,
) -> (RendererScenarioValidationReport, CatalogIndex<'_>) {
    let mut validator = Validator::new();
    validate_catalog_header(catalog, &mut validator);
    validate_oracle_contracts(&catalog.oracle_contracts, &mut validator);
    validate_run_observation_contracts(&catalog.run_observation_contracts, &mut validator);
    validate_accessibility_authority_boundary(
        &catalog.accessibility_authority_boundary,
        &mut validator,
    );
    let content = validate_content_corpus_references(
        &catalog.content_corpus_references,
        &mut validator,
    );
    let evidence = validate_evidence_sources(&catalog.evidence_sources, &mut validator);
    validate_feature_evidence_bindings(
        &catalog.feature_evidence_bindings,
        &evidence,
        &mut validator,
    );
    let workloads = validate_workloads(&catalog.workloads, &content, &mut validator);
    let renderer_config_profiles =
        validate_renderer_config_profiles(&catalog.renderer_config_profiles, &mut validator);
    let layout_profiles = validate_layout_profiles(&catalog.layout_profiles, &mut validator);
    let surface_state_templates = validate_surface_state_templates(
        &catalog.surface_state_templates,
        &content,
        &renderer_config_profiles,
        &mut validator,
    );
    let content_distribution_profiles = validate_content_distribution_profiles(
        &catalog.content_distribution_profiles,
        &content,
        &mut validator,
    );
    let phase_manifests = validate_phase_manifests(
        &catalog.phase_manifests,
        &layout_profiles,
        &surface_state_templates,
        &content_distribution_profiles,
        &renderer_config_profiles,
        &mut validator,
    );
    let coverage_overlay_profiles = validate_coverage_overlay_profiles(
        &catalog.coverage_overlay_profiles,
        &renderer_config_profiles,
        &mut validator,
    );
    let detector_contracts =
        validate_detector_contracts(&catalog.detector_contracts, &mut validator);
    let presentation_profiles = validate_presentation_target_profiles(
        &catalog.presentation_target_profiles,
        &mut validator,
    );
    let preconditioning_profiles = validate_preconditioning_profiles(
        &catalog.preconditioning_profiles,
        &mut validator,
    );
    let driver_canaries = validate_driver_canaries(&catalog.driver_canaries, &mut validator);
    validate_rq_s1_synthetic_substrate(
        catalog.rq_s1_synthetic_substrate.as_ref(),
        &mut validator,
    );
    let index = CatalogIndex {
        scenarios: catalog
            .scenarios
            .iter()
            .map(|scenario| (scenario.scenario_id.as_str(), scenario))
            .collect(),
        content,
        evidence,
        workloads,
        renderer_config_profiles,
        layout_profiles,
        surface_state_templates,
        content_distribution_profiles,
        phase_manifests,
        coverage_overlay_profiles,
        detector_contracts,
        presentation_profiles,
        preconditioning_profiles,
        driver_canaries,
    };
    validate_gesture_authority_map(
        &catalog.gesture_authority_map,
        &index.evidence,
        &mut validator,
    );
    validate_legacy_mappings(&catalog.legacy_mappings, &mut validator);
    validate_scenarios(catalog, &index, &mut validator);
    validate_negative_controls(catalog, &index, &mut validator);
    (validator.finish(), index)
}

/// Validate and index a renderer scenario catalog for repeated resolution.
///
/// This performs no I/O and launches no application. Invalid catalogs fail
/// before an indexed resolver can be constructed.
pub fn prepare_renderer_scenario_catalog(
    catalog: &RendererScenarioCatalog,
) -> Result<RendererPreparedScenarioCatalog<'_>, RendererScenarioResolveError> {
    let (report, index) = validate_renderer_scenario_catalog_indexed(catalog);
    if !report.valid {
        return Err(RendererScenarioResolveError::InvalidCatalog { report });
    }
    Ok(RendererPreparedScenarioCatalog {
        catalog,
        index,
        report,
        preparation_stats: RendererResolverPreparationStats {
            semantic_validation_passes: 1,
            index_builds: 1,
        },
    })
}

/// Resolve one validated scenario-overlay contract into exact ordered anchors.
///
/// This performs no I/O and launches no application. It fails closed when any
/// catalog semantic error exists; explicit tracked readiness gaps remain part
/// of the valid contract and do not prevent deterministic structural expansion.
pub fn resolve_renderer_scenario_overlay(
    catalog: &RendererScenarioCatalog,
    scenario_id: &str,
    overlay_id: RendererCoverageOverlayId,
) -> Result<RendererResolvedScenarioOverlay, RendererScenarioResolveError> {
    prepare_renderer_scenario_catalog(catalog)?.resolve(scenario_id, overlay_id)
}

fn resolve_renderer_scenario_overlay_indexed(
    catalog: &RendererScenarioCatalog,
    index: &CatalogIndex<'_>,
    report: &RendererScenarioValidationReport,
    scenario_id: &str,
    overlay_id: RendererCoverageOverlayId,
) -> Result<RendererResolvedScenarioOverlay, RendererScenarioResolveError> {
    let Some(scenario) = index.scenarios.get(scenario_id).copied() else {
        return Err(RendererScenarioResolveError::ScenarioNotFound {
            scenario_id: scenario_id.to_string(),
        });
    };
    let profile = scenario
        .coverage_overlay_profile_ids
        .iter()
        .filter_map(|profile_id| {
            index
                .coverage_overlay_profiles
                .get(profile_id.as_str())
                .copied()
        })
        .find(|profile| profile.overlay_id == overlay_id)
        .ok_or_else(|| RendererScenarioResolveError::OverlayNotFound {
            scenario_id: scenario_id.to_string(),
            overlay_id,
        })?;
    let missing = |detail: String| RendererScenarioResolveError::MissingReference {
        scenario_id: scenario_id.to_string(),
        overlay_id,
        detail,
    };
    let workload = index
        .workloads
        .get(scenario.workload_id.as_str())
        .copied()
        .ok_or_else(|| missing(format!("undefined workload `{}`", scenario.workload_id)))?;
    let config = index
        .renderer_config_profiles
        .get(profile.renderer_config_profile_id.as_str())
        .copied()
        .ok_or_else(|| {
            missing(format!(
                "undefined renderer configuration `{}`",
                profile.renderer_config_profile_id
            ))
        })?;
    let checkpoints = scenario
        .visual_checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.overlay_id == overlay_id)
        .collect::<Vec<_>>();
    if checkpoints.is_empty() {
        return Err(RendererScenarioResolveError::OverlayNotFound {
            scenario_id: scenario_id.to_string(),
            overlay_id,
        });
    }
    let mut anchors = Vec::with_capacity(checkpoints.len());
    for checkpoint in checkpoints {
        let manifest = index
            .phase_manifests
            .get(checkpoint.phase_manifest_id.as_str())
            .copied()
            .ok_or_else(|| {
                missing(format!(
                    "undefined phase manifest `{}`",
                    checkpoint.phase_manifest_id
                ))
            })?;
        let layout = index
            .layout_profiles
            .get(manifest.layout_profile_id.as_str())
            .copied()
            .ok_or_else(|| {
                missing(format!("undefined layout `{}`", manifest.layout_profile_id))
            })?;
        let distribution = index
            .content_distribution_profiles
            .get(manifest.content_distribution_profile_id.as_str())
            .copied()
            .ok_or_else(|| {
                missing(format!(
                    "undefined content distribution `{}`",
                    manifest.content_distribution_profile_id
                ))
            })?;
        let state = expand_manifest_state(manifest, index, scenario, checkpoint, workload)
            .map_err(&missing)?;
        let identities = expand_layout(layout);
        let focused_window_id = identities
            .window_ids
            .get(usize::from(manifest.focused_window_ordinal))
            .cloned()
            .ok_or_else(|| missing("focused window is outside the expanded layout".to_string()))?;
        let focused_pane_id = identities
            .pane_ids
            .get(usize::from(manifest.focused_pane_ordinal))
            .cloned()
            .ok_or_else(|| missing("focused pane is outside the expanded layout".to_string()))?;
        let mut active_global_tabs = BTreeMap::new();
        let mut windows = Vec::with_capacity(identities.window_ids.len());
        for (window_position, window_id) in identities.window_ids.iter().enumerate() {
            let window_ordinal = u16::try_from(window_position)
                .map_err(|_| missing("window ordinal exceeds u16".to_string()))?;
            let window_state = state
                .window_states
                .get(window_position)
                .ok_or_else(|| missing(format!("window {window_ordinal} has no phase state")))?;
            let tab_ordinals = identities
                .tabs_by_window
                .get(window_position)
                .ok_or_else(|| missing(format!("window {window_ordinal} has no tab order")))?;
            let active_global_tab = tab_ordinals
                .get(usize::from(window_state.active_tab_ordinal))
                .copied()
                .ok_or_else(|| {
                    missing(format!(
                        "window {window_ordinal} active tab is outside its ordered tabs"
                    ))
                })?;
            active_global_tabs.insert(window_ordinal, active_global_tab);
            let ordered_tab_ids = tab_ordinals
                .iter()
                .map(|tab| {
                    identities
                        .tab_ids
                        .get(usize::from(*tab))
                        .cloned()
                        .ok_or_else(|| missing(format!("tab {tab} is outside the layout")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let active_tab_id = identities
                .tab_ids
                .get(usize::from(active_global_tab))
                .cloned()
                .ok_or_else(|| missing("active tab identity is absent".to_string()))?;
            windows.push(RendererResolvedWindow {
                window_ordinal,
                window_id: window_id.clone(),
                coordinate_space: RendererPixelCoordinateSpace::WindowDrawable,
                drawable_rect: window_state.window_rect,
                ordered_tab_ids,
                active_tab_ordinal: window_state.active_tab_ordinal,
                active_tab_id,
                focused: window_ordinal == manifest.focused_window_ordinal,
            });
        }
        let mut tabs = Vec::with_capacity(identities.tab_ids.len());
        for (tab_position, tab_id) in identities.tab_ids.iter().enumerate() {
            let tab_ordinal = u16::try_from(tab_position)
                .map_err(|_| missing("tab ordinal exceeds u16".to_string()))?;
            let window_ordinal = *identities
                .tab_window_ordinals
                .get(tab_position)
                .ok_or_else(|| missing(format!("tab {tab_ordinal} has no window owner")))?;
            let window_id = identities
                .window_ids
                .get(usize::from(window_ordinal))
                .cloned()
                .ok_or_else(|| missing("tab window identity is absent".to_string()))?;
            let window_local_tab_ordinal = identities
                .tabs_by_window
                .get(usize::from(window_ordinal))
                .and_then(|ordered| ordered.iter().position(|tab| *tab == tab_ordinal))
                .and_then(|position| u16::try_from(position).ok())
                .ok_or_else(|| missing("tab is absent from its window order".to_string()))?;
            let ordered_pane_ids = identities
                .pane_tab_ordinals
                .iter()
                .enumerate()
                .filter_map(|(pane, owner)| (*owner == tab_ordinal).then_some(pane))
                .map(|pane| {
                    identities
                        .pane_ids
                        .get(pane)
                        .cloned()
                        .ok_or_else(|| missing(format!("pane {pane} is outside the layout")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            tabs.push(RendererResolvedTab {
                tab_ordinal,
                tab_id: tab_id.clone(),
                window_ordinal,
                window_id,
                window_local_tab_ordinal,
                ordered_pane_ids,
                active: active_global_tabs.get(&window_ordinal) == Some(&tab_ordinal),
            });
        }
        let mut panes = Vec::with_capacity(identities.pane_ids.len());
        for (pane_position, pane_id) in identities.pane_ids.iter().enumerate() {
            let pane_ordinal = u16::try_from(pane_position)
                .map_err(|_| missing("pane ordinal exceeds u16".to_string()))?;
            let window_ordinal = *identities
                .pane_window_ordinals
                .get(pane_position)
                .ok_or_else(|| missing(format!("pane {pane_ordinal} has no window owner")))?;
            let tab_ordinal = *identities
                .pane_tab_ordinals
                .get(pane_position)
                .ok_or_else(|| missing(format!("pane {pane_ordinal} has no tab owner")))?;
            let geometry = state
                .pane_geometry
                .get(pane_position)
                .ok_or_else(|| missing(format!("pane {pane_ordinal} has no split geometry")))?;
            panes.push(RendererResolvedPane {
                pane_ordinal,
                pane_id: pane_id.clone(),
                window_ordinal,
                window_id: identities.window_ids[usize::from(window_ordinal)].clone(),
                tab_ordinal,
                tab_id: identities.tab_ids[usize::from(tab_ordinal)].clone(),
                coordinate_space: RendererPixelCoordinateSpace::WindowDrawable,
                window_content_rect: geometry.rect,
                split_path: geometry.split_path.clone(),
                active_tab: active_global_tabs.get(&window_ordinal) == Some(&tab_ordinal),
                focused: pane_ordinal == manifest.focused_pane_ordinal,
                surface_state: state.surfaces[pane_position].clone(),
                output: state.outputs[pane_position].clone(),
                applied_materialization_steps: state.applied_materialization_steps
                    [pane_position]
                    .clone(),
            });
        }
        anchors.push(RendererResolvedCheckpointAnchor {
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            role: checkpoint.role,
            phase: checkpoint.phase,
            event_ordinal: checkpoint.event_ordinal,
            phase_manifest_id: manifest.phase_manifest_id.clone(),
            expected_invariant_ids: checkpoint.expected_invariant_ids.clone(),
            layout_profile_id: layout.layout_profile_id.clone(),
            layout_stable_id_revision: layout.stable_id_revision.clone(),
            content_distribution_profile_id: distribution
                .content_distribution_profile_id
                .clone(),
            content_distribution_profile_revision: distribution.profile_revision,
            focused_window_id,
            focused_pane_id,
            windows,
            tabs,
            panes,
        });
    }
    let readiness = report
        .overlay_readiness
        .iter()
        .find(|entry| entry.scenario_id == scenario.scenario_id && entry.overlay_id == overlay_id)
        .ok_or_else(|| missing("pair-scoped execution readiness is absent".to_string()))?;
    let blocking_gaps = report
        .gaps
        .iter()
        .filter(|gap| {
            readiness.blocking_gap_codes.contains(&gap.code)
                && gap
                    .scenario_id
                    .as_deref()
                    .is_none_or(|id| id == scenario.scenario_id)
                && gap.overlay_id.is_none_or(|id| id == overlay_id)
        })
        .cloned()
        .collect();
    Ok(RendererResolvedScenarioOverlay {
        contract_id: catalog.contract_id.clone(),
        schema_version: catalog.schema_version,
        catalog_revision: catalog.catalog_revision,
        scenario_id: scenario.scenario_id.clone(),
        gesture: scenario.gesture,
        fleet_point: scenario.fleet_point,
        workload_id: workload.workload_id.clone(),
        workload_revision: workload.revision,
        overlay_id,
        overlay_profile_id: profile.overlay_profile_id.clone(),
        overlay_profile_revision: profile.profile_revision,
        renderer_config_profile_id: config.renderer_config_profile_id.clone(),
        renderer_config_profile_revision: config.profile_revision,
        execution_ready: readiness.execution_ready,
        blocking_gap_codes: readiness.blocking_gap_codes.clone(),
        blocking_gaps,
        anchors,
    })
}

fn validate_catalog_header(catalog: &RendererScenarioCatalog, validator: &mut Validator) {
    if catalog.contract_id != RENDERER_SCENARIO_CONTRACT_ID {
        validator.error(
            RendererScenarioValidationCode::UnknownContract,
            "$.contract_id",
            format!(
                "expected `{RENDERER_SCENARIO_CONTRACT_ID}`, found `{}`",
                catalog.contract_id
            ),
        );
    }
    if catalog.schema_version != RENDERER_SCENARIO_SCHEMA_VERSION {
        validator.error(
            RendererScenarioValidationCode::UnknownSchemaVersion,
            "$.schema_version",
            format!(
                "expected schema version {RENDERER_SCENARIO_SCHEMA_VERSION}, found {}",
                catalog.schema_version
            ),
        );
    }
    if catalog.catalog_revision != RENDERER_SCENARIO_CATALOG_REVISION {
        validator.error(
            RendererScenarioValidationCode::UnknownCatalogRevision,
            "$.catalog_revision",
            format!(
                "expected catalog revision {RENDERER_SCENARIO_CATALOG_REVISION}, found {}",
                catalog.catalog_revision
            ),
        );
    }
    if catalog.source_bead_id != RENDERER_SCENARIO_SOURCE_BEAD_ID {
        validator.error(
            RendererScenarioValidationCode::UnknownSourceBead,
            "$.source_bead_id",
            format!(
                "expected source bead `{RENDERER_SCENARIO_SOURCE_BEAD_ID}`, found `{}`",
                catalog.source_bead_id
            ),
        );
    }
}

fn validate_oracle_contracts(oracles: &RendererOracleContracts, validator: &mut Validator) {
    validator.require_repository_ref(
        "$.oracle_contracts.state_oracle_contract_ref",
        &oracles.state_oracle_contract_ref,
    );
    validator.require_repository_ref(
        "$.oracle_contracts.visual_comparator_contract_ref",
        &oracles.visual_comparator_contract_ref,
    );
    validator.require_repository_ref(
        "$.oracle_contracts.accessibility_oracle_contract_ref",
        &oracles.accessibility_oracle_contract_ref,
    );
    validator.require_repository_ref(
        "$.oracle_contracts.checkpoint_policy_ref",
        &oracles.checkpoint_policy_ref,
    );
    validator.require_repository_ref(
        "$.oracle_contracts.rq_s11_contradiction_tracking_ref",
        &oracles.rq_s11_contradiction_tracking_ref,
    );
    if oracles.rq_s11_contradiction_tracking_ref != RQ_S11_CONTRADICTION_TRACKING_REF {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            "$.oracle_contracts.rq_s11_contradiction_tracking_ref",
            format!("expected exact tracker `{RQ_S11_CONTRADICTION_TRACKING_REF}`"),
        );
    }
}

fn validate_run_observation_contracts(
    contracts: &RendererRunObservationContracts,
    validator: &mut Validator,
) {
    let path = "$.run_observation_contracts";
    if contracts.measurement_contract_id != RENDERER_MEASUREMENT_CONTRACT_ID {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{path}.measurement_contract_id"),
            format!("expected `{RENDERER_MEASUREMENT_CONTRACT_ID}`"),
        );
    }
    for (field, repository_ref) in [
        ("observed_frame_stream_ref", &contracts.observed_frame_stream_ref),
        ("stage_metrics_contract_ref", &contracts.stage_metrics_contract_ref),
        (
            "production_path_receipt_contract_ref",
            &contracts.production_path_receipt_contract_ref,
        ),
        ("run_log_schema_ref", &contracts.run_log_schema_ref),
    ] {
        validator.require_repository_ref(&format!("{path}.{field}"), repository_ref);
    }
    if !contracts.require_overlay_and_profile_revision_identity {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{path}.require_overlay_and_profile_revision_identity"),
            "run logs and artifacts must identify overlay plus materialization/config revisions",
        );
    }
    if contracts.resize_stage_ids.as_slice() != RendererResizeTraceStage::ALL {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{path}.resize_stage_ids"),
            "resize stage inventory must equal canonical R0-R25 order",
        );
    }
    if contracts.keypress_stage_ids.as_slice() != RendererKeypressTraceStage::ALL {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{path}.keypress_stage_ids"),
            "keypress stage inventory must equal canonical K0-K13 order",
        );
    }
}

fn validate_accessibility_authority_boundary(
    boundary: &RendererAccessibilityAuthorityBoundary,
    validator: &mut Validator,
) {
    let path = "$.accessibility_authority_boundary";
    for (field, actual, expected) in [
        (
            "renderer_geometry_tracking_ref",
            boundary.renderer_geometry_tracking_ref.as_str(),
            RENDERER_ACCESSIBILITY_GEOMETRY_TRACKING_REF,
        ),
        (
            "native_accessibility_tracking_ref",
            boundary.native_accessibility_tracking_ref.as_str(),
            NATIVE_ACCESSIBILITY_AUTHORITY_TRACKING_REF,
        ),
    ] {
        validator.require_repository_ref(&format!("{path}.{field}"), actual);
        if actual != expected {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.{field}"),
                format!("expected exact authority owner `{expected}`, found `{actual}`"),
            );
        }
    }
    if boundary.machine_geometry_authorizes_native_accessibility {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureAuthority,
            format!("{path}.machine_geometry_authorizes_native_accessibility"),
            "machine renderer geometry cannot authorize NSAccessibility, VoiceOver, or human review",
        );
    }
}

const TRACK_HEADLESS_NATIVE: [&str; 3] = [
    "ft-interactive-systems-performance-4tenz.3.6",
    "ft-interactive-systems-performance-4tenz.3.3",
    "ft-interactive-systems-performance-4tenz.3.4",
];
const TRACK_STRESS_NATIVE: [&str; 3] = [
    "ft-interactive-systems-performance-4tenz.3.6.1",
    "ft-interactive-systems-performance-4tenz.3.3",
    "ft-interactive-systems-performance-4tenz.3.4",
];

struct ExpectedGestureAuthoritySource {
    evidence_source_id: &'static str,
    repository_ref: &'static str,
    coverage_status: RendererCorpusCoverageStatus,
    tracking_refs: &'static [&'static str],
}

const GESTURE_SOURCE_SAME_GRID: [ExpectedGestureAuthoritySource; 1] =
    [ExpectedGestureAuthoritySource {
        evidence_source_id: "gpu_multipane_resize_static_snapshot",
        repository_ref: "tests/golden/gpu/multipane-resize-static-snapshot/input.json",
        coverage_status: RendererCorpusCoverageStatus::Partial,
        tracking_refs: &TRACK_HEADLESS_NATIVE,
    }];
const GESTURE_SOURCE_GRID_CHANGING: [ExpectedGestureAuthoritySource; 2] = [
    ExpectedGestureAuthoritySource {
        evidence_source_id: "simulation_resize_multi_tab_storm",
        repository_ref: "fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml",
        coverage_status: RendererCorpusCoverageStatus::Partial,
        tracking_refs: &TRACK_HEADLESS_NATIVE,
    },
    ExpectedGestureAuthoritySource {
        evidence_source_id: "gpu_stress_rapid_resize_10s",
        repository_ref: "tests/golden/gpu/stress/rapid-resize-10s/input.json",
        coverage_status: RendererCorpusCoverageStatus::PresentUnqualified,
        tracking_refs: &TRACK_STRESS_NATIVE,
    },
];
const GESTURE_SOURCE_REFLOW_FORWARD: [ExpectedGestureAuthoritySource; 1] =
    [ExpectedGestureAuthoritySource {
        evidence_source_id: "policy_rq_s9_reflow_latency",
        repository_ref: "docs/perf/resize-quality-slo.json#RQ-S9.reflow_latency",
        coverage_status: RendererCorpusCoverageStatus::Gap,
        tracking_refs: &TRACK_HEADLESS_NATIVE,
    }];
const GESTURE_SOURCE_REFLOW_REVERSE: [ExpectedGestureAuthoritySource; 1] =
    [ExpectedGestureAuthoritySource {
        evidence_source_id: "simulation_resize_single_pane_scrollback",
        repository_ref:
            "fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml",
        coverage_status: RendererCorpusCoverageStatus::Partial,
        tracking_refs: &TRACK_HEADLESS_NATIVE,
    }];
const GESTURE_SOURCE_ZOOM: [ExpectedGestureAuthoritySource; 1] =
    [ExpectedGestureAuthoritySource {
        evidence_source_id: "simulation_font_churn_multi_pane",
        repository_ref: "fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml",
        coverage_status: RendererCorpusCoverageStatus::Partial,
        tracking_refs: &TRACK_HEADLESS_NATIVE,
    }];
const GESTURE_SOURCE_DPI_DISPLAY: [ExpectedGestureAuthoritySource; 2] = [
    ExpectedGestureAuthoritySource {
        evidence_source_id: "gpu_stress_dpi_1_00",
        repository_ref: "tests/golden/gpu/stress/dpi-1_00/input.json",
        coverage_status: RendererCorpusCoverageStatus::PresentUnqualified,
        tracking_refs: &TRACK_STRESS_NATIVE,
    },
    ExpectedGestureAuthoritySource {
        evidence_source_id: "gpu_stress_dpi_2_00",
        repository_ref: "tests/golden/gpu/stress/dpi-2_00/input.json",
        coverage_status: RendererCorpusCoverageStatus::PresentUnqualified,
        tracking_refs: &TRACK_STRESS_NATIVE,
    },
];
const GESTURE_SOURCE_OUTPUT_OVERLAP: [ExpectedGestureAuthoritySource; 1] =
    [ExpectedGestureAuthoritySource {
        evidence_source_id: "simulation_mixed_workload_interactive_streaming",
        repository_ref:
            "fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml",
        coverage_status: RendererCorpusCoverageStatus::Gap,
        tracking_refs: &TRACK_HEADLESS_NATIVE,
    }];

const fn expected_gesture_authority_sources(
    gesture: RendererGesture,
) -> &'static [ExpectedGestureAuthoritySource] {
    match gesture {
        RendererGesture::SameGridDrag => &GESTURE_SOURCE_SAME_GRID,
        RendererGesture::GridChangingDrag => &GESTURE_SOURCE_GRID_CHANGING,
        RendererGesture::Reflow80To200 => &GESTURE_SOURCE_REFLOW_FORWARD,
        RendererGesture::Reflow200To80 => &GESTURE_SOURCE_REFLOW_REVERSE,
        RendererGesture::ZoomIn | RendererGesture::ZoomOut => &GESTURE_SOURCE_ZOOM,
        RendererGesture::DpiDisplayMove => &GESTURE_SOURCE_DPI_DISPLAY,
        RendererGesture::OutputOverlapResize => &GESTURE_SOURCE_OUTPUT_OVERLAP,
    }
}

fn validate_gesture_authority_map(
    entries: &[RendererGestureAuthorityEntry],
    evidence: &BTreeMap<&str, &RendererEvidenceSource>,
    validator: &mut Validator,
) {
    if entries.len() != REQUIRED_RENDERER_GESTURE_COUNT {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureAuthority,
            "$.gesture_authority_map",
            format!(
                "expected exactly {REQUIRED_RENDERER_GESTURE_COUNT} gesture rows, found {}",
                entries.len()
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for (position, entry) in entries.iter().enumerate() {
        let path = format!("$.gesture_authority_map[{position}]");
        if !seen.insert(entry.gesture) {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.gesture"),
                format!("duplicate gesture row `{}`", entry.gesture.as_str()),
            );
        }
        let expected_sources = expected_gesture_authority_sources(entry.gesture);
        if entry.sources.len() != expected_sources.len() {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.sources"),
                format!(
                    "gesture `{}` requires exactly {} canonical evidence sources, found {}",
                    entry.gesture.as_str(),
                    expected_sources.len(),
                    entry.sources.len()
                ),
            );
        }
        let mut source_keys = BTreeSet::new();
        let mut derived_replay = false;
        let mut derived_compare = false;
        for (source_position, source) in entry.sources.iter().enumerate() {
            let source_path = format!("{path}.sources[{source_position}]");
            validator.require_identifier(
                &format!("{source_path}.evidence_source_id"),
                &source.evidence_source_id,
            );
            if !source_keys.insert(source.evidence_source_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::InvalidGestureAuthority,
                    format!("{source_path}.evidence_source_id"),
                    format!(
                        "duplicate gesture-authority evidence source `{}`",
                        source.evidence_source_id
                    ),
                );
            }
            if let Some(expected) = expected_sources.get(source_position) {
                if source.evidence_source_id != expected.evidence_source_id {
                    validator.error(
                        RendererScenarioValidationCode::InvalidGestureAuthority,
                        format!("{source_path}.evidence_source_id"),
                        format!(
                            "expected canonical evidence source `{}`",
                            expected.evidence_source_id
                        ),
                    );
                }
                if source.coverage_status != expected.coverage_status {
                    validator.error(
                        RendererScenarioValidationCode::InvalidGestureAuthority,
                        format!("{source_path}.coverage_status"),
                        format!(
                            "expected canonical status {:?}",
                            expected.coverage_status
                        ),
                    );
                }
                let actual_tracking = source
                    .tracking_refs
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                if actual_tracking.as_slice() != expected.tracking_refs {
                    validator.error(
                        RendererScenarioValidationCode::InvalidGestureAuthority,
                        format!("{source_path}.tracking_refs"),
                        format!(
                            "expected source-specific trackers {:?}, found {:?}",
                            expected.tracking_refs, actual_tracking
                        ),
                    );
                }
            }
            match evidence.get(source.evidence_source_id.as_str()) {
                Some(reference) => {
                    if let Some(expected) = expected_sources.get(source_position)
                        && reference.repository_ref != expected.repository_ref
                    {
                        validator.error(
                            RendererScenarioValidationCode::InvalidGestureAuthority,
                            format!("{source_path}.evidence_source_id"),
                            format!(
                                "evidence source `{}` must resolve to `{}`",
                                source.evidence_source_id, expected.repository_ref
                            ),
                        );
                    }
                    let qualified = matches!(
                        source.coverage_status,
                        RendererCorpusCoverageStatus::Direct
                            | RendererCorpusCoverageStatus::Partial
                    );
                    derived_replay |= qualified && reference.authorizes_gesture_replay;
                    derived_compare |=
                        qualified && reference.authorizes_headless_checkpoint_comparison;
                }
                None => validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    format!("{source_path}.evidence_source_id"),
                    format!(
                        "undefined gesture-authority evidence source `{}`",
                        source.evidence_source_id
                    ),
                ),
            }
            validate_non_direct_status_fields(
                &source_path,
                source.coverage_status,
                source.limitation.as_deref(),
                &source.tracking_refs,
                validator,
            );
            if let Some(limitation) = source.limitation.as_deref()
                && !limitation.trim().is_empty()
            {
                for tracking_ref in &source.tracking_refs {
                    if !tracking_ref.trim().is_empty() {
                        validator.gap(
                            RendererScenarioGapCode::GestureCoverageNotDirect,
                            &source_path,
                            limitation,
                            tracking_ref,
                        );
                    }
                }
            }
        }
        if entry.headless_gesture_replay != derived_replay {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.headless_gesture_replay"),
                format!("expected derived value {derived_replay}"),
            );
        }
        if entry.headless_checkpoint_compare != derived_compare {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.headless_checkpoint_compare"),
                format!("expected derived value {derived_compare}"),
            );
        }
        if entry.native_target_authority {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.native_target_authority"),
                "contract-only catalog cannot authorize a native target",
            );
        }
    }
    for gesture in RendererGesture::ALL {
        if !seen.contains(&gesture) {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                "$.gesture_authority_map",
                format!("missing gesture authority row `{}`", gesture.as_str()),
            );
        }
    }
}

fn validate_non_direct_status_fields(
    path: &str,
    status: RendererCorpusCoverageStatus,
    limitation: Option<&str>,
    tracking_refs: &[String],
    validator: &mut Validator,
) {
    if status == RendererCorpusCoverageStatus::Direct {
        if limitation.is_some() || !tracking_refs.is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                path,
                "direct status must not carry limitation or tracking_refs",
            );
        }
        return;
    }
    match limitation {
        Some(value) if !value.trim().is_empty() && value.len() <= MAX_REASON_BYTES => {}
        Some(value) if value.len() > MAX_REASON_BYTES => validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            format!("{path}.limitation"),
            format!(
                "limitation is {} bytes (maximum {MAX_REASON_BYTES})",
                value.len()
            ),
        ),
        Some(_) | None => validator.error(
            RendererScenarioValidationCode::InvalidGestureAuthority,
            format!("{path}.limitation"),
            "non-direct status requires a non-empty limitation",
        ),
    }
    if tracking_refs.is_empty() {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureAuthority,
            format!("{path}.tracking_refs"),
            "non-direct status requires at least one tracking reference",
        );
    }
    let mut seen = BTreeSet::new();
    for (position, value) in tracking_refs.iter().enumerate() {
        let reference_path = format!("{path}.tracking_refs[{position}]");
        if value.trim().is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                reference_path.as_str(),
                "tracking reference must not be empty",
            );
        } else {
            validator.require_repository_ref(&reference_path, value);
        }
        if !seen.insert(value.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                reference_path.as_str(),
                format!("duplicate tracking reference `{value}`"),
            );
        }
    }
}

fn validate_legacy_mappings(
    mappings: &[RendererLegacyScenarioMapping],
    validator: &mut Validator,
) {
    if mappings.len() != RendererLegacyScenarioId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidLegacyMapping,
            "$.legacy_mappings",
            format!(
                "expected exactly {} canonical legacy mappings, found {}",
                RendererLegacyScenarioId::ALL.len(),
                mappings.len()
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for (position, mapping) in mappings.iter().enumerate() {
        let path = format!("$.legacy_mappings[{position}]");
        if RendererLegacyScenarioId::ALL.get(position) != Some(&mapping.legacy_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                format!("{path}.legacy_id"),
                "legacy mappings must appear in canonical contract order",
            );
        }
        if !seen.insert(mapping.legacy_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                format!("{path}.legacy_id"),
                "duplicate canonical legacy mapping",
            );
        }
        validator.require_repository_ref(&format!("{path}.source_ref"), &mapping.source_ref);
        let (expected_source_ref, expected_disposition, expected_targets) =
            expected_legacy_mapping(mapping.legacy_id);
        if mapping.source_ref != expected_source_ref {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                format!("{path}.source_ref"),
                format!("expected canonical source `{expected_source_ref}`"),
            );
        }
        if mapping.disposition != expected_disposition {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                format!("{path}.disposition"),
                format!("expected canonical disposition {expected_disposition:?}"),
            );
        }
        if mapping.target_gestures.as_slice() != expected_targets {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                format!("{path}.target_gestures"),
                format!("expected canonical targets {expected_targets:?}"),
            );
        }
        let mut gestures = BTreeSet::new();
        for (gesture_position, gesture) in mapping.target_gestures.iter().enumerate() {
            if !gestures.insert(*gesture) {
                validator.error(
                    RendererScenarioValidationCode::InvalidLegacyMapping,
                    format!("{path}.target_gestures[{gesture_position}]"),
                    format!("duplicate target gesture `{}`", gesture.as_str()),
                );
            }
        }
        if mapping.reason.trim().is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                format!("{path}.reason"),
                "legacy mapping requires a non-empty non-equivalence reason",
            );
        } else if mapping.reason.len() > MAX_REASON_BYTES {
            validator.error(
                RendererScenarioValidationCode::LimitExceeded,
                format!("{path}.reason"),
                format!(
                    "legacy reason is {} bytes (maximum {MAX_REASON_BYTES})",
                    mapping.reason.len()
                ),
            );
        }
    }
    for legacy_id in RendererLegacyScenarioId::ALL {
        if !seen.contains(&legacy_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidLegacyMapping,
                "$.legacy_mappings",
                format!("missing canonical legacy mapping `{:?}`", legacy_id),
            );
        }
    }
}

const LEGACY_TARGET_RESIZE_STEP: [RendererGesture; 2] = [
    RendererGesture::SameGridDrag,
    RendererGesture::GridChangingDrag,
];
const LEGACY_TARGET_RESIZE_BURST: [RendererGesture; 2] = [
    RendererGesture::GridChangingDrag,
    RendererGesture::OutputOverlapResize,
];
const LEGACY_TARGET_ZOOM: [RendererGesture; 2] =
    [RendererGesture::ZoomIn, RendererGesture::ZoomOut];
const LEGACY_TARGET_DPI: [RendererGesture; 1] = [RendererGesture::DpiDisplayMove];
const LEGACY_TARGET_REFLOW: [RendererGesture; 2] = [
    RendererGesture::Reflow80To200,
    RendererGesture::Reflow200To80,
];
const LEGACY_TARGET_GRID: [RendererGesture; 1] = [RendererGesture::GridChangingDrag];
const LEGACY_TARGET_MIXED_SCALE: [RendererGesture; 3] = [
    RendererGesture::ZoomIn,
    RendererGesture::ZoomOut,
    RendererGesture::DpiDisplayMove,
];
const LEGACY_TARGET_OUTPUT: [RendererGesture; 1] = [RendererGesture::OutputOverlapResize];
const LEGACY_TARGET_NONE: [RendererGesture; 0] = [];

const fn expected_legacy_mapping(
    legacy_id: RendererLegacyScenarioId,
) -> (
    &'static str,
    RendererLegacyMappingDisposition,
    &'static [RendererGesture],
) {
    match legacy_id {
        RendererLegacyScenarioId::ResizeStep => (
            "tests/renderer_golden/SCENARIOS.md#resize-step",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_RESIZE_STEP,
        ),
        RendererLegacyScenarioId::ResizeBurst => (
            "tests/renderer_golden/SCENARIOS.md#resize-burst",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_RESIZE_BURST,
        ),
        RendererLegacyScenarioId::FontChange => (
            "tests/renderer_golden/SCENARIOS.md#font-change",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_ZOOM,
        ),
        RendererLegacyScenarioId::DpiChange => (
            "tests/renderer_golden/SCENARIOS.md#dpi-change",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_DPI,
        ),
        RendererLegacyScenarioId::ResizeSinglePaneScrollback => (
            "fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_REFLOW,
        ),
        RendererLegacyScenarioId::ResizeMultiTabStorm => (
            "fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_GRID,
        ),
        RendererLegacyScenarioId::FontChurnMultiPane => (
            "fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_ZOOM,
        ),
        RendererLegacyScenarioId::MixedScaleSoak => (
            "fixtures/simulations/resize_baseline/mixed_scale_soak.yaml",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_MIXED_SCALE,
        ),
        RendererLegacyScenarioId::MixedWorkloadInteractiveStreaming => (
            "fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml",
            RendererLegacyMappingDisposition::RelatedOnly,
            &LEGACY_TARGET_OUTPUT,
        ),
        RendererLegacyScenarioId::A11ySteadyTyping => (
            "docs/a11y/scenario-corpus.md#1-steady_typing",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11yPaneFocusChange => (
            "docs/a11y/scenario-corpus.md#2-pane_focus_change",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11yDialogOpen => (
            "docs/a11y/scenario-corpus.md#3-dialog_open",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11ySelectionChange => (
            "docs/a11y/scenario-corpus.md#4-selection_change",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11yScrollPositionChange => (
            "docs/a11y/scenario-corpus.md#5-scroll_position_change",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
    }
}

fn validate_content_corpus_references<'a>(
    references: &'a [RendererContentCorpusReference],
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererContentCorpusReference> {
    if references.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.content_corpus_references",
            "at least one terminal-content corpus is required",
        );
    }
    if references.len() != EXPECTED_CANONICAL_CONTENT.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            "$.content_corpus_references",
            format!(
                "expected exactly {} canonical v1 content inputs, found {}",
                EXPECTED_CANONICAL_CONTENT.len(),
                references.len()
            ),
        );
    }
    if references.len() > MAX_RENDERER_CORPUS_REFERENCES {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            "$.content_corpus_references",
            format!(
                "found {} content corpora (maximum {MAX_RENDERER_CORPUS_REFERENCES})",
                references.len()
            ),
        );
    }

    let mut index = BTreeMap::new();
    for (position, reference) in references.iter().enumerate() {
        let path = format!("$.content_corpus_references[{position}]");
        validator.require_identifier(
            &format!("{path}.content_corpus_id"),
            &reference.content_corpus_id,
        );
        validator.require_repository_ref(
            &format!("{path}.repository_ref"),
            &reference.repository_ref,
        );
        if index
            .insert(reference.content_corpus_id.as_str(), reference)
            .is_some()
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.content_corpus_id"),
                format!(
                    "duplicate content corpus id `{}`",
                    reference.content_corpus_id
                ),
            );
        }
        if reference.payload_revision == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                format!("{path}.payload_revision"),
                "content payload revision must be positive",
            );
        }
        if let Some(expected) = EXPECTED_CANONICAL_CONTENT.get(position) {
            validate_canonical_content_reference(&path, reference, expected, validator);
        }
        match &reference.availability {
            RendererContentInputAvailability::Available => {}
            RendererContentInputAvailability::Unavailable {
                reason,
                tracking_refs,
            } => {
                validate_tracked_limitation(
                    &format!("{path}.availability"),
                    reason,
                    tracking_refs,
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    validator,
                );
            }
        }
        match &reference.deterministic_identity {
            RendererContentDeterministicIdentity::Generator {
                generator_id,
                generator_revision,
                generator_seed,
                input_manifest_ref,
                output_encoding,
                output_decoder,
                output_framing,
            } => {
                validator.require_identifier(
                    &format!("{path}.deterministic_identity.generator_id"),
                    generator_id,
                );
                if *generator_revision == 0 || *generator_seed == 0 {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.deterministic_identity"),
                        "content generator revision and seed must be positive",
                    );
                }
                validator.require_repository_ref(
                    &format!("{path}.deterministic_identity.input_manifest_ref"),
                    input_manifest_ref,
                );
                if !matches!(
                    output_encoding,
                    RendererContentEncoding::GeneratedTerminalBytesV1
                        | RendererContentEncoding::GeneratedTypedStateV1
                ) {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.deterministic_identity.output_encoding"),
                        "a generator must declare generated terminal bytes or generated typed state",
                    );
                }
                if *output_decoder != RendererContentDecoder::GeneratorV1 {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.deterministic_identity.output_decoder"),
                        "a generator must declare generator_v1 decoding",
                    );
                }
                validate_content_framing(
                    &format!("{path}.deterministic_identity.output_framing"),
                    *output_encoding,
                    *output_framing,
                    validator,
                );
            }
            RendererContentDeterministicIdentity::Payload {
                payload_ref,
                selector,
                encoding,
                decoder,
                framing,
                encoded_payload_sha256,
                decoded_payload_sha256,
            } => {
                validator.require_repository_ref(
                    &format!("{path}.deterministic_identity.payload_ref"),
                    payload_ref,
                );
                validate_sha256_hex(
                    &format!("{path}.deterministic_identity.encoded_payload_sha256"),
                    encoded_payload_sha256,
                    validator,
                );
                validate_sha256_hex(
                    &format!("{path}.deterministic_identity.decoded_payload_sha256"),
                    decoded_payload_sha256,
                    validator,
                );
                validate_content_payload_selector(
                    &format!("{path}.deterministic_identity.selector"),
                    selector,
                    *encoding,
                    validator,
                );
                validate_content_decoder(
                    &format!("{path}.deterministic_identity.decoder"),
                    *encoding,
                    *decoder,
                    validator,
                );
                validate_content_framing(
                    &format!("{path}.deterministic_identity.framing"),
                    *encoding,
                    *framing,
                    validator,
                );
                if matches!(
                    encoding,
                    RendererContentEncoding::GeneratedTerminalBytesV1
                        | RendererContentEncoding::GeneratedTypedStateV1
                ) {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.deterministic_identity.encoding"),
                        "a checked-in payload cannot use generated-terminal encoding",
                    );
                }
                if *encoding == RendererContentEncoding::GpuFixtureStateV1
                    && !matches!(framing, RendererContentFraming::TypedStateOverlay)
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.deterministic_identity.framing"),
                        "GPU fixture state must materialize as a typed state overlay",
                    );
                }
            }
        }
        if reference.semantic_kinds.is_empty()
            && !matches!(
                content_identity_framing(reference),
                RendererContentFraming::TypedStateOverlay
            )
        {
            validator.error(
                RendererScenarioValidationCode::EmptyRequiredField,
                format!("{path}.semantic_kinds"),
                "a content corpus must declare at least one decoded semantic kind",
            );
        }
        let mut semantics = BTreeSet::new();
        let mut previous = None;
        for (semantic_position, semantic) in reference.semantic_kinds.iter().enumerate() {
            if !semantics.insert(*semantic) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{path}.semantic_kinds[{semantic_position}]"),
                    format!("duplicate semantic kind `{semantic:?}` in one content corpus"),
                );
            }
            let canonical_position = RendererContentSemanticKind::ALL
                .iter()
                .position(|candidate| candidate == semantic);
            if previous.is_some_and(|previous| canonical_position <= Some(previous)) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{path}.semantic_kinds[{semantic_position}]"),
                    "semantic kinds must appear in canonical contract order",
                );
            }
            previous = canonical_position;
        }
    }
    index
}

#[derive(Debug, Clone, Copy)]
struct ExpectedEvidenceSource {
    evidence_source_id: &'static str,
    repository_ref: &'static str,
    authority_scope: RendererCorpusAuthorityScope,
    authorizes_gesture_replay: bool,
    authorizes_headless_checkpoint_comparison: bool,
    coverage_status: RendererCorpusCoverageStatus,
}

const EXPECTED_EVIDENCE_SOURCES: [ExpectedEvidenceSource; 29] = [
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.text_basic_paragraph",
        repository_ref: "tests/golden/gpu/text-basic-paragraph/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.text_cjk_mixed",
        repository_ref: "tests/golden/gpu/text-cjk-mixed/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.text_rtl_arabic_hebrew",
        repository_ref: "tests/golden/gpu/text-rtl-arabic-hebrew/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.text_combining_marks",
        repository_ref: "tests/golden/gpu/text-combining-marks/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.text_emoji_fallback",
        repository_ref: "tests/golden/gpu/text-emoji-fallback/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_beam_blink",
        repository_ref: "tests/golden/gpu/cursor-beam-blink/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_beam_steady",
        repository_ref: "tests/golden/gpu/cursor-beam-steady/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_block_blink",
        repository_ref: "tests/golden/gpu/cursor-block-blink/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_block_steady",
        repository_ref: "tests/golden/gpu/cursor-block-steady/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_underline_blink",
        repository_ref: "tests/golden/gpu/cursor-underline-blink/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_underline_steady",
        repository_ref: "tests/golden/gpu/cursor-underline-steady/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Direct,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.selection_char",
        repository_ref: "tests/golden/gpu/selection-char/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.selection_word",
        repository_ref: "tests/golden/gpu/selection-word/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.selection_line",
        repository_ref: "tests/golden/gpu/selection-line/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_fixture.overlay_ime_composition",
        repository_ref: "tests/golden/gpu/overlay-ime-composition/meta.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: true,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "inventory.gpu_ligatures_gap",
        repository_ref: "tests/golden/gpu/README.md",
        authority_scope: RendererCorpusAuthorityScope::ContractOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "inventory.gpu_images_gap",
        repository_ref: "tests/golden/gpu/README.md",
        authority_scope: RendererCorpusAuthorityScope::ContractOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "inventory.gpu_hyperlinks_gap",
        repository_ref: "tests/golden/gpu/README.md",
        authority_scope: RendererCorpusAuthorityScope::ContractOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "inventory.renderer_alt_screen_gap",
        repository_ref: "tests/renderer_golden/SCENARIOS.md",
        authority_scope: RendererCorpusAuthorityScope::ContractOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "a11y.scenario_corpus_geometry_gap",
        repository_ref: "docs/a11y/scenario-corpus.md",
        authority_scope: RendererCorpusAuthorityScope::ContractOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_multipane_resize_static_snapshot",
        repository_ref: "tests/golden/gpu/multipane-resize-static-snapshot/input.json",
        authority_scope: RendererCorpusAuthorityScope::HeadlessVisualFixture,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "simulation_resize_multi_tab_storm",
        repository_ref: "fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml",
        authority_scope: RendererCorpusAuthorityScope::HeadlessStateReplay,
        authorizes_gesture_replay: true,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_stress_rapid_resize_10s",
        repository_ref: "tests/golden/gpu/stress/rapid-resize-10s/input.json",
        authority_scope: RendererCorpusAuthorityScope::MetamorphicSignalOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::PresentUnqualified,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "policy_rq_s9_reflow_latency",
        repository_ref: "docs/perf/resize-quality-slo.json#RQ-S9.reflow_latency",
        authority_scope: RendererCorpusAuthorityScope::ContractOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "simulation_resize_single_pane_scrollback",
        repository_ref: "fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml",
        authority_scope: RendererCorpusAuthorityScope::HeadlessStateReplay,
        authorizes_gesture_replay: true,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "simulation_font_churn_multi_pane",
        repository_ref: "fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml",
        authority_scope: RendererCorpusAuthorityScope::HeadlessStateReplay,
        authorizes_gesture_replay: true,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Partial,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_stress_dpi_1_00",
        repository_ref: "tests/golden/gpu/stress/dpi-1_00/input.json",
        authority_scope: RendererCorpusAuthorityScope::MetamorphicSignalOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::PresentUnqualified,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "gpu_stress_dpi_2_00",
        repository_ref: "tests/golden/gpu/stress/dpi-2_00/input.json",
        authority_scope: RendererCorpusAuthorityScope::MetamorphicSignalOnly,
        authorizes_gesture_replay: false,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::PresentUnqualified,
    },
    ExpectedEvidenceSource {
        evidence_source_id: "simulation_mixed_workload_interactive_streaming",
        repository_ref: "fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml",
        authority_scope: RendererCorpusAuthorityScope::HeadlessStateReplay,
        authorizes_gesture_replay: true,
        authorizes_headless_checkpoint_comparison: false,
        coverage_status: RendererCorpusCoverageStatus::Gap,
    },
];

fn validate_evidence_sources<'a>(
    sources: &'a [RendererEvidenceSource],
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererEvidenceSource> {
    if sources.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.evidence_sources",
            "at least one evidence source is required",
        );
    }
    if sources.len() != EXPECTED_EVIDENCE_SOURCES.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            "$.evidence_sources",
            format!(
                "expected exactly {} frozen evidence sources, found {}",
                EXPECTED_EVIDENCE_SOURCES.len(),
                sources.len()
            ),
        );
    }
    let expected = EXPECTED_EVIDENCE_SOURCES
        .iter()
        .map(|entry| (entry.evidence_source_id, entry))
        .collect::<BTreeMap<_, _>>();
    let mut index = BTreeMap::new();
    for (position, source) in sources.iter().enumerate() {
        let path = format!("$.evidence_sources[{position}]");
        validator.require_identifier(
            &format!("{path}.evidence_source_id"),
            &source.evidence_source_id,
        );
        validator.require_repository_ref(
            &format!("{path}.repository_ref"),
            &source.repository_ref,
        );
        if index
            .insert(source.evidence_source_id.as_str(), source)
            .is_some()
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.evidence_source_id"),
                format!("duplicate evidence source `{}`", source.evidence_source_id),
            );
        }
        match expected.get(source.evidence_source_id.as_str()) {
            Some(expected)
                if source.repository_ref == expected.repository_ref
                    && source.authority_scope == expected.authority_scope
                    && source.authorizes_gesture_replay
                        == expected.authorizes_gesture_replay
                    && source.authorizes_headless_checkpoint_comparison
                        == expected.authorizes_headless_checkpoint_comparison
                    && source.coverage_status == expected.coverage_status => {}
            Some(expected) => validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                &path,
                format!(
                    "evidence source must equal frozen tuple ref=`{}`, scope={:?}, replay={}, compare={}, status={:?}",
                    expected.repository_ref,
                    expected.authority_scope,
                    expected.authorizes_gesture_replay,
                    expected.authorizes_headless_checkpoint_comparison,
                    expected.coverage_status
                ),
            ),
            None => validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                format!("{path}.evidence_source_id"),
                format!(
                    "evidence source `{}` is outside the frozen v1 inventory",
                    source.evidence_source_id
                ),
            ),
        }
        match source.authority_scope {
            RendererCorpusAuthorityScope::HeadlessVisualFixture => {
                if source.authorizes_gesture_replay {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.authorizes_gesture_replay"),
                        "a visual fixture cannot authorize gesture replay",
                    );
                }
            }
            RendererCorpusAuthorityScope::HeadlessStateReplay => {
                if source.authorizes_headless_checkpoint_comparison {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.authorizes_headless_checkpoint_comparison"),
                        "a state replay cannot authorize visual checkpoint comparison",
                    );
                }
            }
            RendererCorpusAuthorityScope::MetamorphicSignalOnly
            | RendererCorpusAuthorityScope::ContractOnly => {
                if source.authorizes_gesture_replay
                    || source.authorizes_headless_checkpoint_comparison
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        &path,
                        "metamorphic/contract-only evidence cannot authorize replay or comparison",
                    );
                }
            }
        }
        validate_evidence_qualification(
            &path,
            source.coverage_status,
            source.limitation.as_deref(),
            &source.tracking_refs,
            validator,
        );
    }
    for expected_id in expected.keys() {
        if !index.contains_key(expected_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                "$.evidence_sources",
                format!("missing frozen evidence source `{expected_id}`"),
            );
        }
    }
    index
}

fn validate_evidence_qualification(
    path: &str,
    status: RendererCorpusCoverageStatus,
    limitation: Option<&str>,
    tracking_refs: &[String],
    validator: &mut Validator,
) {
    if status == RendererCorpusCoverageStatus::Direct {
        if limitation.is_some() || !tracking_refs.is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                path,
                "direct evidence must not carry limitation or tracking refs",
            );
        }
        return;
    }
    match limitation {
        Some(value) if !value.trim().is_empty() && value.len() <= MAX_REASON_BYTES => {}
        Some(value) if value.len() > MAX_REASON_BYTES => validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            format!("{path}.limitation"),
            format!("limitation is {} bytes (maximum {MAX_REASON_BYTES})", value.len()),
        ),
        Some(_) | None => validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            format!("{path}.limitation"),
            "non-direct evidence requires a non-empty limitation",
        ),
    }
    if tracking_refs.is_empty() {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            format!("{path}.tracking_refs"),
            "non-direct evidence requires at least one tracking reference",
        );
    }
    let mut seen = BTreeSet::new();
    for (position, tracking_ref) in tracking_refs.iter().enumerate() {
        let tracking_path = format!("{path}.tracking_refs[{position}]");
        validator.require_repository_ref(&tracking_path, tracking_ref);
        if !seen.insert(tracking_ref.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                tracking_path,
                format!("duplicate tracking reference `{tracking_ref}`"),
            );
        }
        if let Some(limitation) = limitation.filter(|value| !value.trim().is_empty()) {
            validator.gap(
                RendererScenarioGapCode::CorpusCoverageNotDirect,
                path,
                limitation,
                tracking_ref,
            );
        }
    }
}

fn validate_feature_evidence_bindings(
    bindings: &[RendererFeatureEvidenceBinding],
    evidence: &BTreeMap<&str, &RendererEvidenceSource>,
    validator: &mut Validator,
) {
    if bindings.len() != RendererTerminalFeature::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            "$.feature_evidence_bindings",
            format!(
                "expected exactly {} feature evidence rows, found {}",
                RendererTerminalFeature::ALL.len(),
                bindings.len()
            ),
        );
    }
    let mut seen_features = BTreeSet::new();
    for (position, binding) in bindings.iter().enumerate() {
        let path = format!("$.feature_evidence_bindings[{position}]");
        if RendererTerminalFeature::ALL.get(position) != Some(&binding.terminal_feature) {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                format!("{path}.terminal_feature"),
                "feature evidence rows must appear in canonical feature order",
            );
        }
        if !seen_features.insert(binding.terminal_feature) {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                format!("{path}.terminal_feature"),
                format!("duplicate feature `{}`", binding.terminal_feature.as_str()),
            );
        }
        if binding.sources.is_empty() {
            validator.error(
                RendererScenarioValidationCode::EmptyRequiredField,
                format!("{path}.sources"),
                "feature evidence row requires at least one classified source",
            );
        }
        let expected_status = expected_feature_evidence_status(binding.terminal_feature);
        let expected_sources = expected_feature_evidence_sources(binding.terminal_feature);
        if binding.sources.len() != expected_sources.len() {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                format!("{path}.sources"),
                format!(
                    "feature `{}` requires exactly {} canonical sources, found {}",
                    binding.terminal_feature.as_str(),
                    expected_sources.len(),
                    binding.sources.len()
                ),
            );
        }
        let mut source_ids = BTreeSet::new();
        for (source_position, source) in binding.sources.iter().enumerate() {
            let source_path = format!("{path}.sources[{source_position}]");
            validator.require_identifier(
                &format!("{source_path}.evidence_source_id"),
                &source.evidence_source_id,
            );
            if !source_ids.insert(source.evidence_source_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{source_path}.evidence_source_id"),
                    format!("duplicate feature evidence source `{}`", source.evidence_source_id),
                );
            }
            if !evidence.contains_key(source.evidence_source_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    format!("{source_path}.evidence_source_id"),
                    format!("undefined evidence source `{}`", source.evidence_source_id),
                );
            }
            if source.coverage_status != expected_status {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{source_path}.coverage_status"),
                    format!(
                        "feature `{}` requires status {:?}",
                        binding.terminal_feature.as_str(), expected_status
                    ),
                );
            }
            if let Some(expected) = expected_sources.get(source_position) {
                let actual_tracking = source
                    .tracking_refs
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                if source.evidence_source_id != expected.evidence_source_id
                    || actual_tracking.as_slice() != expected.tracking_refs
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        &source_path,
                        format!(
                            "expected canonical source `{}` with trackers {:?}",
                            expected.evidence_source_id, expected.tracking_refs
                        ),
                    );
                }
                if let Some(evidence_source) = evidence.get(source.evidence_source_id.as_str())
                    && evidence_source.repository_ref != expected.repository_ref
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{source_path}.evidence_source_id"),
                        format!(
                            "canonical evidence source must resolve to `{}`",
                            expected.repository_ref
                        ),
                    );
                }
            }
            validate_evidence_qualification(
                &source_path,
                source.coverage_status,
                source.limitation.as_deref(),
                &source.tracking_refs,
                validator,
            );
        }
    }
}

struct ExpectedFeatureEvidenceSource {
    evidence_source_id: &'static str,
    repository_ref: &'static str,
    tracking_refs: &'static [&'static str],
}

const TRACK_NONE: [&str; 0] = [];
const TRACK_SELECTION_ALT: [&str; 2] = [
    "ft-ruona",
    "ft-interactive-swarm-product-convergence-7xqz4.9.2",
];
const TRACK_VISUAL_CORPUS: [&str; 3] = [
    "ft-interactive-systems-performance-4tenz.3.6.2",
    "ft-interactive-systems-performance-4tenz.3.5",
    "ft-interactive-swarm-product-convergence-7xqz4.9.2",
];
const TRACK_IME_CORPUS: [&str; 4] = [
    "ft-interactive-systems-performance-4tenz.3.6.2",
    "ft-interactive-systems-performance-4tenz.3.5",
    "ft-interactive-swarm-product-convergence-7xqz4.9.2",
    "ft-interactive-swarm-product-convergence-7xqz4.9.5",
];
const TRACK_ACCESSIBILITY_GEOMETRY: [&str; 2] = [
    RENDERER_ACCESSIBILITY_GEOMETRY_TRACKING_REF,
    NATIVE_ACCESSIBILITY_AUTHORITY_TRACKING_REF,
];

const FEATURE_ASCII: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "gpu_fixture.text_basic_paragraph",
    repository_ref: "tests/golden/gpu/text-basic-paragraph/meta.json",
    tracking_refs: &TRACK_NONE,
}];
const FEATURE_CJK: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "gpu_fixture.text_cjk_mixed",
    repository_ref: "tests/golden/gpu/text-cjk-mixed/meta.json",
    tracking_refs: &TRACK_NONE,
}];
const FEATURE_RTL: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "gpu_fixture.text_rtl_arabic_hebrew",
    repository_ref: "tests/golden/gpu/text-rtl-arabic-hebrew/meta.json",
    tracking_refs: &TRACK_NONE,
}];
const FEATURE_COMBINING: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "gpu_fixture.text_combining_marks",
    repository_ref: "tests/golden/gpu/text-combining-marks/meta.json",
    tracking_refs: &TRACK_NONE,
}];
const FEATURE_EMOJI: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "gpu_fixture.text_emoji_fallback",
    repository_ref: "tests/golden/gpu/text-emoji-fallback/meta.json",
    tracking_refs: &TRACK_NONE,
}];
const FEATURE_CURSOR: [ExpectedFeatureEvidenceSource; 6] = [
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_beam_blink",
        repository_ref: "tests/golden/gpu/cursor-beam-blink/meta.json",
        tracking_refs: &TRACK_NONE,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_beam_steady",
        repository_ref: "tests/golden/gpu/cursor-beam-steady/meta.json",
        tracking_refs: &TRACK_NONE,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_block_blink",
        repository_ref: "tests/golden/gpu/cursor-block-blink/meta.json",
        tracking_refs: &TRACK_NONE,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_block_steady",
        repository_ref: "tests/golden/gpu/cursor-block-steady/meta.json",
        tracking_refs: &TRACK_NONE,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_underline_blink",
        repository_ref: "tests/golden/gpu/cursor-underline-blink/meta.json",
        tracking_refs: &TRACK_NONE,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.cursor_underline_steady",
        repository_ref: "tests/golden/gpu/cursor-underline-steady/meta.json",
        tracking_refs: &TRACK_NONE,
    },
];
const FEATURE_SELECTION: [ExpectedFeatureEvidenceSource; 3] = [
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.selection_char",
        repository_ref: "tests/golden/gpu/selection-char/meta.json",
        tracking_refs: &TRACK_SELECTION_ALT,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.selection_word",
        repository_ref: "tests/golden/gpu/selection-word/meta.json",
        tracking_refs: &TRACK_SELECTION_ALT,
    },
    ExpectedFeatureEvidenceSource {
        evidence_source_id: "gpu_fixture.selection_line",
        repository_ref: "tests/golden/gpu/selection-line/meta.json",
        tracking_refs: &TRACK_SELECTION_ALT,
    },
];
const FEATURE_IME: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "gpu_fixture.overlay_ime_composition",
    repository_ref: "tests/golden/gpu/overlay-ime-composition/meta.json",
    tracking_refs: &TRACK_IME_CORPUS,
}];
const FEATURE_LIGATURES: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "inventory.gpu_ligatures_gap",
    repository_ref: "tests/golden/gpu/README.md",
    tracking_refs: &TRACK_VISUAL_CORPUS,
}];
const FEATURE_IMAGES: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "inventory.gpu_images_gap",
    repository_ref: "tests/golden/gpu/README.md",
    tracking_refs: &TRACK_VISUAL_CORPUS,
}];
const FEATURE_HYPERLINKS: [ExpectedFeatureEvidenceSource; 1] = [ExpectedFeatureEvidenceSource {
    evidence_source_id: "inventory.gpu_hyperlinks_gap",
    repository_ref: "tests/golden/gpu/README.md",
    tracking_refs: &TRACK_VISUAL_CORPUS,
}];
const FEATURE_ALTERNATE_SCREEN: [ExpectedFeatureEvidenceSource; 1] =
    [ExpectedFeatureEvidenceSource {
        evidence_source_id: "inventory.renderer_alt_screen_gap",
        repository_ref: "tests/renderer_golden/SCENARIOS.md",
        tracking_refs: &TRACK_SELECTION_ALT,
    }];
const FEATURE_ACCESSIBILITY: [ExpectedFeatureEvidenceSource; 1] =
    [ExpectedFeatureEvidenceSource {
        evidence_source_id: "a11y.scenario_corpus_geometry_gap",
        repository_ref: "docs/a11y/scenario-corpus.md",
        tracking_refs: &TRACK_ACCESSIBILITY_GEOMETRY,
    }];

const fn expected_feature_evidence_sources(
    feature: RendererTerminalFeature,
) -> &'static [ExpectedFeatureEvidenceSource] {
    match feature {
        RendererTerminalFeature::Ascii => &FEATURE_ASCII,
        RendererTerminalFeature::Cjk => &FEATURE_CJK,
        RendererTerminalFeature::Rtl => &FEATURE_RTL,
        RendererTerminalFeature::CombiningMarks => &FEATURE_COMBINING,
        RendererTerminalFeature::Emoji => &FEATURE_EMOJI,
        RendererTerminalFeature::Ligatures => &FEATURE_LIGATURES,
        RendererTerminalFeature::Images => &FEATURE_IMAGES,
        RendererTerminalFeature::Hyperlinks => &FEATURE_HYPERLINKS,
        RendererTerminalFeature::AlternateScreen => &FEATURE_ALTERNATE_SCREEN,
        RendererTerminalFeature::Selection => &FEATURE_SELECTION,
        RendererTerminalFeature::Cursor => &FEATURE_CURSOR,
        RendererTerminalFeature::Ime => &FEATURE_IME,
        RendererTerminalFeature::AccessibilityGeometry => &FEATURE_ACCESSIBILITY,
    }
}

const fn expected_feature_evidence_status(
    feature: RendererTerminalFeature,
) -> RendererCorpusCoverageStatus {
    match feature {
        RendererTerminalFeature::Ascii
        | RendererTerminalFeature::Cjk
        | RendererTerminalFeature::Rtl
        | RendererTerminalFeature::CombiningMarks
        | RendererTerminalFeature::Emoji
        | RendererTerminalFeature::Cursor => RendererCorpusCoverageStatus::Direct,
        RendererTerminalFeature::Selection | RendererTerminalFeature::Ime => {
            RendererCorpusCoverageStatus::Partial
        }
        RendererTerminalFeature::Ligatures
        | RendererTerminalFeature::Images
        | RendererTerminalFeature::Hyperlinks
        | RendererTerminalFeature::AlternateScreen
        | RendererTerminalFeature::AccessibilityGeometry => RendererCorpusCoverageStatus::Gap,
    }
}

fn validate_workloads<'a>(
    workloads: &'a [RendererWorkloadDefinition],
    corpus: &BTreeMap<&str, &RendererContentCorpusReference>,
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererWorkloadDefinition> {
    if workloads.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.workloads",
            "at least one workload definition is required",
        );
    }
    if workloads.len() > MAX_RENDERER_WORKLOADS {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            "$.workloads",
            format!(
                "found {} workloads (maximum {MAX_RENDERER_WORKLOADS})",
                workloads.len()
            ),
        );
    }

    let mut index = BTreeMap::new();
    for (position, workload) in workloads.iter().enumerate() {
        let path = format!("$.workloads[{position}]");
        validator.require_identifier(&format!("{path}.workload_id"), &workload.workload_id);
        if index
            .insert(workload.workload_id.as_str(), workload)
            .is_some()
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.workload_id"),
                format!("duplicate workload id `{}`", workload.workload_id),
            );
        }
        if workload.revision == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{path}.revision"),
                "workload revision must be positive",
            );
        }
        for (field, count) in [
            ("pane_count", workload.pane_count),
            ("tab_count", workload.tab_count),
            ("window_count", workload.window_count),
        ] {
            if count == 0 {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    format!("{path}.{field}"),
                    format!("{field} must be positive"),
                );
            }
        }
        validator.require_identifier(
            &format!("{path}.layout_profile_id"),
            &workload.layout_profile_id,
        );
        validator.require_identifier(
            &format!("{path}.renderer_config_profile_id"),
            &workload.renderer_config_profile_id,
        );
        validator.require_repository_ref(
            &format!("{path}.font_metric_derivation_ref"),
            &workload.font_metric_derivation_ref,
        );
        if workload.gesture_duration_us == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{path}.gesture_duration_us"),
                "gesture duration must be positive",
            );
        }
        if workload.total_duration_us <= workload.gesture_duration_us {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{path}.total_duration_us"),
                "total duration must be greater than gesture duration to retain settle time",
            );
        }
        if workload.event_count == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{path}.event_count"),
                "workload event_count must be positive",
            );
        }
        if let Some(stream) = &workload.output_stream {
            validate_output_stream(
                &format!("{path}.output_stream"),
                stream,
                workload.pane_count,
                validator,
            );
        }
        validate_foreground_key_events(
            &format!("{path}.foreground_key_events"),
            &workload.foreground_key_events,
            validator,
        );
        if workload.content_corpus_ids.is_empty() {
            validator.error(
                RendererScenarioValidationCode::EmptyRequiredField,
                format!("{path}.content_corpus_ids"),
                "workload must reference at least one terminal-content corpus",
            );
        }
        let mut corpus_ids = BTreeSet::new();
        for (corpus_position, corpus_id) in workload.content_corpus_ids.iter().enumerate() {
            let corpus_path = format!("{path}.content_corpus_ids[{corpus_position}]");
            validator.require_identifier(&corpus_path, corpus_id);
            if !corpus_ids.insert(corpus_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    &corpus_path,
                    format!("duplicate content corpus id `{corpus_id}` in workload"),
                );
            }
            if !corpus.contains_key(corpus_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    corpus_path,
                    format!("undefined content corpus id `{corpus_id}`"),
                );
            }
        }
    }
    index
}

fn validate_output_stream(
    path: &str,
    stream: &RendererOutputStreamDefinition,
    pane_count: u16,
    validator: &mut Validator,
) {
    for (field, identifier) in [
        ("stream_id", &stream.stream_id),
        ("generator_id", &stream.generator_id),
        ("distribution_id", &stream.distribution_id),
        ("layout_profile_id", &stream.layout_profile_id),
    ] {
        validator.require_identifier(&format!("{path}.{field}"), identifier);
    }
    if stream.generator_revision == 0 || stream.generator_seed == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            path,
            "output generator revision and seed must both be positive",
        );
    }
    validator.require_repository_ref(
        &format!("{path}.payload_manifest_ref"),
        &stream.payload_manifest_ref,
    );
    if stream.aggregate_bytes_per_second == 0
        || stream.aggregate_bytes_per_second > MAX_RENDERER_OUTPUT_BYTES_PER_SECOND
    {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            format!("{path}.aggregate_bytes_per_second"),
            format!(
                "aggregate rate must be in 1..={MAX_RENDERER_OUTPUT_BYTES_PER_SECOND}"
            ),
        );
    }
    if pane_count == 0 {
        return;
    }
    let base = stream.aggregate_bytes_per_second / u64::from(pane_count);
    let remainder = stream.aggregate_bytes_per_second % u64::from(pane_count);
    let mut rates = (0..pane_count)
        .map(|ordinal| base + u64::from(ordinal < remainder as u16))
        .collect::<Vec<_>>();
    let mut overridden = BTreeSet::new();
    for (position, rate_override) in stream.rate_overrides.iter().enumerate() {
        let override_path = format!("{path}.rate_overrides[{position}]");
        let ordinals = expand_pane_selector(
            &format!("{override_path}.selector"),
            &rate_override.selector,
            pane_count,
            validator,
        );
        for ordinal in ordinals {
            if !overridden.insert(ordinal) {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    &override_path,
                    format!("pane ordinal {ordinal} is overridden more than once"),
                );
            }
            rates[usize::from(ordinal)] = rate_override.bytes_per_second;
        }
    }
    let sum = rates.iter().try_fold(0_u64, |total, rate| total.checked_add(*rate));
    if sum != Some(stream.aggregate_bytes_per_second) {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            format!("{path}.rate_overrides"),
            format!(
                "expanded pane rates sum to {:?}, expected aggregate {}",
                sum,
                stream.aggregate_bytes_per_second
            ),
        );
    }
}

fn validate_foreground_key_events(
    path: &str,
    events: &[RendererForegroundKeyEvent],
    validator: &mut Validator,
) {
    let mut ids = BTreeSet::new();
    for (position, event) in events.iter().enumerate() {
        let event_path = format!("{path}[{position}]");
        for (field, identifier) in [
            ("key_event_id", &event.key_event_id),
            ("logical_key", &event.logical_key),
            ("target_pane_id", &event.target_pane_id),
        ] {
            validator.require_identifier(&format!("{event_path}.{field}"), identifier);
        }
        if !ids.insert(event.key_event_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{event_path}.key_event_id"),
                format!("duplicate key-event id `{}`", event.key_event_id),
            );
        }
        let modifiers = event.modifiers.iter().copied().collect::<BTreeSet<_>>();
        if modifiers.len() != event.modifiers.len()
            || modifiers.iter().copied().collect::<Vec<_>>() != event.modifiers
        {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{event_path}.modifiers"),
                "key modifiers must be unique and in canonical enum order",
            );
        }
        if event.logical_key != "x"
            || !event.modifiers.is_empty()
            || event.encoded_bytes_hex != "78"
        {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                &event_path,
                "v1 foreground key tuple must be exactly logical_key=x, modifiers=[], encoded_bytes_hex=78",
            );
        }
        let encoded = event.encoded_bytes_hex.as_bytes();
        if encoded.is_empty()
            || encoded.len() % 2 != 0
            || !encoded
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{event_path}.encoded_bytes_hex"),
                "encoded key bytes must be non-empty, even-length lowercase hexadecimal",
            );
        }
    }
}


fn validate_scenarios(
    catalog: &RendererScenarioCatalog,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    if catalog.scenarios.len() != REQUIRED_RENDERER_SCENARIO_COUNT {
        validator.error(
            RendererScenarioValidationCode::MissingRequiredCoverage,
            "$.scenarios",
            format!(
                "expected exactly {REQUIRED_RENDERER_SCENARIO_COUNT} scenarios, found {}",
                catalog.scenarios.len()
            ),
        );
    }
    let mut scenario_ids = BTreeSet::new();
    let mut seeds = BTreeSet::new();
    let mut coverage = BTreeMap::new();
    let mut active_workloads = BTreeSet::new();
    let mut active_content = BTreeSet::new();
    let mut active_phase_manifests = BTreeMap::new();
    let mut checkpoint_manifest_order = Vec::new();
    let mut checkpoint_binding_count = 0_usize;
    for (position, scenario) in catalog.scenarios.iter().enumerate() {
        let path = format!("$.scenarios[{position}]");
        let expected_cell = RendererGesture::ALL
            .get(position / RendererFleetPoint::ALL.len())
            .zip(RendererFleetPoint::ALL.get(position % RendererFleetPoint::ALL.len()));
        if expected_cell != Some((&scenario.gesture, &scenario.fleet_point)) {
            validator.error(
                RendererScenarioValidationCode::MissingRequiredCoverage,
                &path,
                "scenarios must appear in canonical gesture-major, fleet-minor order",
            );
        }
        validator.require_identifier(&format!("{path}.scenario_id"), &scenario.scenario_id);
        if !scenario_ids.insert(scenario.scenario_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.scenario_id"),
                format!("duplicate scenario `{}`", scenario.scenario_id),
            );
        }
        if !seeds.insert(scenario.seed) {
            validator.error(
                RendererScenarioValidationCode::InvalidSeed,
                format!("{path}.seed"),
                format!("duplicate deterministic seed {}", scenario.seed),
            );
        }
        if let Some(previous) = coverage.insert((scenario.gesture, scenario.fleet_point), position)
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateCoverageCell,
                &path,
                format!("coverage cell duplicates $.scenarios[{previous}]"),
            );
        }
        let expected_id = expected_renderer_scenario_id(scenario.gesture, scenario.fleet_point);
        if scenario.scenario_id != expected_id {
            validator.error(
                RendererScenarioValidationCode::InvalidIdentifier,
                format!("{path}.scenario_id"),
                format!("expected canonical scenario id `{expected_id}`"),
            );
        }
        let expected_seed = expected_renderer_scenario_seed(scenario.gesture, scenario.fleet_point);
        if scenario.seed != expected_seed {
            validator.error(
                RendererScenarioValidationCode::InvalidSeed,
                format!("{path}.seed"),
                format!("expected deterministic seed {expected_seed}"),
            );
        }
        for (field, actual, expected) in [
            ("pane_count", scenario.pane_count, scenario.fleet_point.pane_count()),
            ("tab_count", scenario.tab_count, scenario.fleet_point.tab_count()),
            (
                "window_count",
                scenario.window_count,
                scenario.fleet_point.window_count(),
            ),
        ] {
            if actual != expected {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.{field}"),
                    format!("fleet point requires {expected}, found {actual}"),
                );
            }
        }
        if scenario.required_path_class != RendererExecutionPathClass::ProductionNative {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.required_path_class"),
                "every v1 matrix cell requires the production-native execution path",
            );
        }
        if scenario.configured_steady_quality == RendererQualityMode::Draft {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.configured_steady_quality"),
                "configured steady quality cannot be Draft",
            );
        }
        match (scenario.gesture, scenario.output_overlap_resize_mode) {
            (RendererGesture::OutputOverlapResize, None) => validator.error(
                RendererScenarioValidationCode::InvalidGestureTransition,
                format!("{path}.output_overlap_resize_mode"),
                "output-overlap resize requires an explicit resize mode",
            ),
            (RendererGesture::OutputOverlapResize, Some(_)) | (_, None) => {}
            (_, Some(_)) => validator.error(
                RendererScenarioValidationCode::InvalidGestureTransition,
                format!("{path}.output_overlap_resize_mode"),
                "output-overlap resize mode is forbidden for other gestures",
            ),
        }
        let workload = index.workloads.get(scenario.workload_id.as_str()).copied();
        if workload.is_none() {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.workload_id"),
                format!("undefined workload `{}`", scenario.workload_id),
            );
        } else {
            active_workloads.insert(scenario.workload_id.as_str());
        }
        let layout = workload.and_then(|workload| {
            if workload.pane_count != scenario.pane_count
                || workload.tab_count != scenario.tab_count
                || workload.window_count != scenario.window_count
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    format!("{path}.workload_id"),
                    "workload topology counts differ from the scenario cell",
                );
            }
            if workload.renderer_config_profile_id != PRODUCTION_DEFAULT_CONFIG_ID
                || !index
                    .renderer_config_profiles
                    .contains_key(workload.renderer_config_profile_id.as_str())
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    format!("{path}.workload_id"),
                    "workload must bind the same-catalog bundled production-default renderer configuration",
                );
            }
            if workload.output_stream.as_ref().is_some_and(|stream| {
                stream.layout_profile_id != workload.layout_profile_id
            }) {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    format!("{path}.workload_id"),
                    "output stream layout must exactly equal the workload layout",
                );
            }
            let layout = index
                .layout_profiles
                .get(workload.layout_profile_id.as_str())
                .copied();
            match layout {
                Some(layout) if layout.fleet_point != scenario.fleet_point => validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.workload_id"),
                    "workload layout fleet point differs from the scenario cell",
                ),
                None => validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    format!("{path}.workload_id"),
                    format!("undefined layout `{}`", workload.layout_profile_id),
                ),
                Some(_) => {}
            }
            layout
        });
        let facts = match (workload, layout) {
            (Some(workload), Some(layout)) => {
                let facts = validate_timeline(&path, scenario, workload, layout, validator);
                validate_output_and_key_schedule(
                    &path, scenario, workload, &facts, index, validator,
                );
                Some(facts)
            }
            _ => None,
        };
        validate_expected_invariants(&path, scenario, validator);
        let overlays = resolve_scenario_overlays(&path, scenario, index, validator);
        validate_visual_checkpoints(&path, scenario, &overlays, index, validator);
        validate_observed_frame_policies(&path, scenario, index, validator);
        validate_detector_bindings(&path, scenario, index, validator);
        validate_requirement_crosswalk(&path, scenario, workload, &overlays, index, validator);
        validate_scenario_profile_sets(&path, scenario, index, validator);
        validate_scenario_capabilities_and_readiness(
            &path, scenario, &overlays, index, validator,
        );
        if let (Some(workload), Some(facts)) = (workload, facts.as_ref()) {
            validate_measurement_bindings(
                &path, scenario, workload, facts, index, validator,
            );
            validate_scenario_materialization(
                &path,
                scenario,
                workload,
                &overlays,
                index,
                &mut active_content,
                validator,
            );
            for overlay_id in RendererCoverageOverlayId::ALL {
                if let Some(overlay) = overlays.get(&overlay_id) {
                    validate_overlay_replay(&path, scenario, workload, overlay, index, validator);
                    validate_overlay_gesture_transition(
                        &path, scenario, overlay, index, validator,
                    );
                }
            }
        }
        checkpoint_binding_count = checkpoint_binding_count
            .saturating_add(scenario.visual_checkpoints.len());
        for checkpoint in &scenario.visual_checkpoints {
            checkpoint_manifest_order.push(checkpoint.phase_manifest_id.as_str());
            if let Some(previous_scenario) = active_phase_manifests.insert(
                checkpoint.phase_manifest_id.as_str(),
                scenario.scenario_id.as_str(),
            ) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{path}.visual_checkpoints"),
                    format!(
                        "phase manifest `{}` is already bound by scenario `{previous_scenario}`",
                        checkpoint.phase_manifest_id
                    ),
                );
            }
        }
    }
    for gesture in RendererGesture::ALL {
        for fleet_point in RendererFleetPoint::ALL {
            if !coverage.contains_key(&(gesture, fleet_point)) {
                validator.error(
                    RendererScenarioValidationCode::MissingRequiredCoverage,
                    "$.scenarios",
                    format!(
                        "missing required coverage cell `{}`",
                        expected_renderer_scenario_id(gesture, fleet_point)
                    ),
                );
            }
        }
    }
    if checkpoint_binding_count != REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT {
        validator.error(
            RendererScenarioValidationCode::MissingRequiredCoverage,
            "$.scenarios[*].visual_checkpoints",
            format!(
                "the exact matrix requires {REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT} checkpoint-to-manifest bindings, found {checkpoint_binding_count}"
            ),
        );
    }
    if catalog.phase_manifests.len() != REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT {
        validator.error(
            RendererScenarioValidationCode::MissingRequiredCoverage,
            "$.phase_manifests",
            format!(
                "the exact matrix requires {REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT} unique phase manifests, found {}",
                catalog.phase_manifests.len()
            ),
        );
    }
    let manifest_order = catalog
        .phase_manifests
        .iter()
        .map(|manifest| manifest.phase_manifest_id.as_str())
        .collect::<Vec<_>>();
    if manifest_order != checkpoint_manifest_order {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            "$.phase_manifests",
            "phase manifests must appear in the exact scenario/checkpoint binding order",
        );
    }
    for (position, manifest) in catalog.phase_manifests.iter().enumerate() {
        if !active_phase_manifests.contains_key(manifest.phase_manifest_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.phase_manifests[{position}].phase_manifest_id"),
                format!(
                    "phase manifest `{}` is not bound by a visual checkpoint",
                    manifest.phase_manifest_id
                ),
            );
        }
    }
    let active_layouts = catalog
        .phase_manifests
        .iter()
        .map(|manifest| manifest.layout_profile_id.as_str())
        .chain(
            catalog
                .workloads
                .iter()
                .map(|workload| workload.layout_profile_id.as_str()),
        )
        .collect::<BTreeSet<_>>();
    let active_distributions = catalog
        .phase_manifests
        .iter()
        .map(|manifest| manifest.content_distribution_profile_id.as_str())
        .collect::<BTreeSet<_>>();
    let active_templates = catalog
        .phase_manifests
        .iter()
        .flat_map(|manifest| {
            std::iter::once(manifest.default_surface_state_template_id.as_str()).chain(
                manifest
                    .pane_overrides
                    .iter()
                    .map(|pane| pane.surface_state_template_id.as_str()),
            )
        })
        .collect::<BTreeSet<_>>();
    for (position, layout) in catalog.layout_profiles.iter().enumerate() {
        if !active_layouts.contains(layout.layout_profile_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.layout_profiles[{position}].layout_profile_id"),
                format!("layout `{}` is not selected by the matrix", layout.layout_profile_id),
            );
        }
    }
    for (position, distribution) in catalog.content_distribution_profiles.iter().enumerate() {
        if !active_distributions.contains(distribution.content_distribution_profile_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.content_distribution_profiles[{position}].content_distribution_profile_id"),
                format!(
                    "content distribution `{}` is not selected by a phase manifest",
                    distribution.content_distribution_profile_id
                ),
            );
        }
    }
    for (position, template) in catalog.surface_state_templates.iter().enumerate() {
        if !active_templates.contains(template.surface_state_template_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.surface_state_templates[{position}].surface_state_template_id"),
                format!(
                    "surface template `{}` is not selected by a phase manifest",
                    template.surface_state_template_id
                ),
            );
        }
    }
    for (position, workload) in catalog.workloads.iter().enumerate() {
        if !active_workloads.contains(workload.workload_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.workloads[{position}].workload_id"),
                format!("workload `{}` is not selected by a scenario", workload.workload_id),
            );
        }
    }
    for (position, reference) in catalog.content_corpus_references.iter().enumerate() {
        if !active_content.contains(reference.content_corpus_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.content_corpus_references[{position}].content_corpus_id"),
                format!(
                    "content `{}` is not selected by an active overlay distribution",
                    reference.content_corpus_id
                ),
            );
        }
    }
}

fn validate_scenario_profile_sets(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    if scenario.presentation_target_profile_ids.as_slice()
        != RendererPresentationTargetProfileId::ALL
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{scenario_path}.presentation_target_profile_ids"),
            "scenario must request fixed-60, fixed-120, and variable-refresh profiles in canonical order",
        );
    }
    for profile_id in &scenario.presentation_target_profile_ids {
        if !index.presentation_profiles.contains_key(profile_id) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{scenario_path}.presentation_target_profile_ids"),
                "scenario names an undefined presentation profile",
            );
        }
    }
    if scenario.preconditioning_profile_ids.as_slice()
        != RendererPreconditioningProfileId::ALL
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{scenario_path}.preconditioning_profile_ids"),
            "scenario must request cold, warm, and aged preconditions in canonical order",
        );
    }
    for profile_id in &scenario.preconditioning_profile_ids {
        if !index.preconditioning_profiles.contains_key(profile_id) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{scenario_path}.preconditioning_profile_ids"),
                "scenario names an undefined preconditioning profile",
            );
        }
    }
    if scenario.driver_canary_ids.as_slice() != RendererDriverCanaryId::ALL {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{scenario_path}.driver_canary_ids"),
            "scenario must request all five non-qualifying driver canaries in canonical order",
        );
    }
    for canary_id in &scenario.driver_canary_ids {
        if !index.driver_canaries.contains_key(canary_id) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{scenario_path}.driver_canary_ids"),
                "scenario names an undefined driver canary",
            );
        }
    }
}

fn materialization_boundary_key(
    step: &RendererContentMaterializationStep,
    scenario: &RendererScenarioDefinition,
    overlay_id: RendererCoverageOverlayId,
) -> Result<(u32, u8), String> {
    match &step.application_boundary {
        RendererContentApplicationBoundary::BeforeGesture => Ok((0, 0)),
        RendererContentApplicationBoundary::AtEvent { event_ordinal } => {
            match scenario.timeline.get(*event_ordinal as usize) {
                Some(event) if event.event_ordinal == *event_ordinal => {
                    Ok((*event_ordinal, 1))
                }
                _ => Err(format!(
                    "at-event boundary {event_ordinal} does not resolve to an exact timeline event"
                )),
            }
        }
        RendererContentApplicationBoundary::AfterCheckpoint { checkpoint_id } => scenario
            .visual_checkpoints
            .iter()
            .find(|checkpoint| {
                checkpoint.overlay_id == overlay_id
                    && checkpoint.checkpoint_id == *checkpoint_id
            })
            .map(|checkpoint| (checkpoint.event_ordinal, 2))
            .ok_or_else(|| {
                format!(
                    "after-checkpoint boundary `{checkpoint_id}` is absent from overlay `{}`",
                    overlay_id.as_str()
                )
            }),
    }
}

fn validate_scenario_materialization(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: &RendererWorkloadDefinition,
    overlays: &BTreeMap<RendererCoverageOverlayId, ResolvedScenarioOverlay<'_>>,
    index: &CatalogIndex<'_>,
    active_content: &mut BTreeSet<String>,
    validator: &mut Validator,
) {
    let mut ordered_union = Vec::new();
    let mut union_seen = BTreeSet::new();
    for overlay_id in RendererCoverageOverlayId::ALL {
        let Some(overlay) = overlays.get(&overlay_id) else {
            continue;
        };
        let Some(first_manifest) = overlay.anchors.first() else {
            continue;
        };
        let Some(distribution) = index
            .content_distribution_profiles
            .get(first_manifest.content_distribution_profile_id.as_str())
            .copied()
        else {
            continue;
        };
        let Some(final_checkpoint) = overlay.checkpoints.last().copied() else {
            continue;
        };
        for (assignment_position, assignment) in distribution.assignments.iter().enumerate() {
            let assignment_path = format!(
                "{scenario_path}.materialization.{}.assignments[{assignment_position}]",
                overlay_id.as_str()
            );
            let mut previous_boundary = None;
            let mut active_buffer = RendererTerminalBufferKind::Primary;
            for (step_position, step) in assignment.materialization_steps.iter().enumerate() {
                let step_path =
                    format!("{assignment_path}.materialization_steps[{step_position}]");
                let boundary_key = materialization_boundary_key(step, scenario, overlay_id);
                match boundary_key {
                    Ok(key) => {
                        if previous_boundary.is_some_and(|previous| key < previous) {
                            validator.error(
                                RendererScenarioValidationCode::InvalidState,
                                format!("{step_path}.application_boundary"),
                                "materialization boundaries must be in chronological canonical order",
                            );
                        }
                        previous_boundary = Some(key);
                    }
                    Err(detail) => validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{step_path}.application_boundary"),
                        detail,
                    ),
                }
                match materialization_step_applies(step, scenario, final_checkpoint) {
                    Ok(true) => {}
                    Ok(false) => validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{step_path}.application_boundary"),
                        "materialization boundary is never reached by the overlay's final checkpoint",
                    ),
                    Err(detail) => validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        &step_path,
                        detail,
                    ),
                }
                match step.operation {
                    RendererContentCompositionOperation::EnterAlternateBuffer => {
                        if active_buffer == RendererTerminalBufferKind::Alternate {
                            validator.error(
                                RendererScenarioValidationCode::InvalidState,
                                format!("{step_path}.operation"),
                                "cannot enter the alternate buffer while it is already active",
                            );
                        }
                        active_buffer = RendererTerminalBufferKind::Alternate;
                    }
                    RendererContentCompositionOperation::ExitAlternateBuffer => {
                        if active_buffer != RendererTerminalBufferKind::Alternate {
                            validator.error(
                                RendererScenarioValidationCode::InvalidState,
                                format!("{step_path}.operation"),
                                "cannot exit the alternate buffer while the primary buffer is active",
                            );
                        }
                        active_buffer = RendererTerminalBufferKind::Primary;
                    }
                    RendererContentCompositionOperation::ReplaceActiveBuffer
                    | RendererContentCompositionOperation::AppendToActiveBuffer
                    | RendererContentCompositionOperation::ApplyTypedStateOverlay => {}
                }
                if union_seen.insert(step.content_corpus_id.as_str()) {
                    ordered_union.push(step.content_corpus_id.as_str());
                }
                active_content.insert(step.content_corpus_id.clone());
            }
        }
    }
    let workload_content = workload
        .content_corpus_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if workload_content != ordered_union {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            format!("{scenario_path}.workload_id"),
            format!(
                "workload content IDs must equal the canonical ordered overlay-distribution union {ordered_union:?}"
            ),
        );
    }
}

fn expected_base_capability_requirement(
    scenario: &RendererScenarioDefinition,
    capability: RendererCapability,
) -> RendererCapabilityRequirement {
    match capability {
        RendererCapability::HeadlessStateOracle
        | RendererCapability::GpuVisualCapture
        | RendererCapability::ProductionMuxDomain
        | RendererCapability::RealPtyStream
        | RendererCapability::ProductionTermWindow
        | RendererCapability::ProductionRendererBackend
        | RendererCapability::MetalDrawableCapture
        | RendererCapability::SoftwarePresentBoundary
        | RendererCapability::DisplayPresentationBoundary
        | RendererCapability::NativeColorProfile => RendererCapabilityRequirement::Required,
        RendererCapability::NativeWindowGesture
            if scenario.gesture != RendererGesture::DpiDisplayMove =>
        {
            RendererCapabilityRequirement::Required
        }
        RendererCapability::NativeDisplayMove
            if scenario.gesture == RendererGesture::DpiDisplayMove =>
        {
            RendererCapabilityRequirement::Required
        }
        RendererCapability::NativeKeyInjection
            if scenario.gesture == RendererGesture::OutputOverlapResize =>
        {
            RendererCapabilityRequirement::Required
        }
        RendererCapability::DisplayPhotonBoundary => RendererCapabilityRequirement::Optional,
        RendererCapability::NativeWindowGesture
        | RendererCapability::NativeDisplayMove
        | RendererCapability::ImeComposition
        | RendererCapability::AccessibilityGeometry
        | RendererCapability::ImageProtocol
        | RendererCapability::NativeKeyInjection
        | RendererCapability::HdrEdrOutput
        | RendererCapability::EnabledLigatureShaping => {
            RendererCapabilityRequirement::NotApplicable
        }
    }
}

fn capability_unavailability(
    availability: &RendererCapabilityAvailability,
) -> Option<(&str, &str)> {
    match availability {
        RendererCapabilityAvailability::DeclaredAvailable => None,
        RendererCapabilityAvailability::Partial {
            reason,
            tracking_ref,
        }
        | RendererCapabilityAvailability::UnknownNotProbed {
            reason,
            tracking_ref,
        }
        | RendererCapabilityAvailability::Unsupported {
            reason,
            tracking_ref,
        }
        | RendererCapabilityAvailability::TargetDependent {
            reason,
            tracking_ref,
            ..
        } => Some((reason, tracking_ref)),
    }
}

const fn capability_blocks_native_capture(capability: RendererCapability) -> bool {
    matches!(
        capability,
        RendererCapability::GpuVisualCapture
            | RendererCapability::NativeWindowGesture
            | RendererCapability::NativeDisplayMove
            | RendererCapability::ProductionTermWindow
            | RendererCapability::ProductionRendererBackend
            | RendererCapability::MetalDrawableCapture
            | RendererCapability::SoftwarePresentBoundary
            | RendererCapability::DisplayPresentationBoundary
            | RendererCapability::NativeColorProfile
            | RendererCapability::HdrEdrOutput
    )
}

fn validate_scenario_capabilities_and_readiness(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    overlays: &BTreeMap<RendererCoverageOverlayId, ResolvedScenarioOverlay<'_>>,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.capabilities");
    if scenario.capabilities.len() != RendererCapability::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidCapabilityMatrix,
            &path,
            format!(
                "expected exactly {} capability rows, found {}",
                RendererCapability::ALL.len(),
                scenario.capabilities.len()
            ),
        );
    }
    let mut base = BTreeMap::new();
    for (position, binding) in scenario.capabilities.iter().enumerate() {
        let binding_path = format!("{path}[{position}]");
        if RendererCapability::ALL.get(position) != Some(&binding.capability) {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                format!("{binding_path}.capability"),
                "capability rows must appear in canonical inventory order",
            );
        }
        if base.insert(binding.capability, binding).is_some() {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                format!("{binding_path}.capability"),
                "duplicate capability row",
            );
        }
        let expected = expected_base_capability_requirement(scenario, binding.capability);
        if binding.requirement != expected {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                format!("{binding_path}.requirement"),
                format!(
                    "capability `{}` requires base classification {expected:?}",
                    binding.capability.as_str()
                ),
            );
        }
        validate_capability_availability_shape(
            &format!("{binding_path}.availability"),
            &binding.availability,
            validator,
        );
    }
    for overlay_id in RendererCoverageOverlayId::ALL {
        let mut blocking_codes = Vec::new();
        let profile = overlays.get(&overlay_id).map(|overlay| overlay.profile).or_else(|| {
            index
                .coverage_overlay_profiles
                .get(expected_overlay_profile_id(overlay_id).as_str())
                .copied()
        });
        let mut merged = base.clone();
        if let Some(profile) = profile {
            for delta in &profile.capability_deltas {
                merged.insert(delta.capability, delta);
            }
        }
        let overlay_requires_hdr = overlays.get(&overlay_id).is_some_and(|overlay| {
            overlay.anchors.iter().any(|manifest| {
                std::iter::once(manifest.default_surface_state_template_id.as_str())
                    .chain(
                        manifest
                            .pane_overrides
                            .iter()
                            .map(|pane| pane.surface_state_template_id.as_str()),
                    )
                    .filter_map(|template_id| index.surface_state_templates.get(template_id))
                    .any(|template| {
                        template.surface_state.display.dynamic_range_mode
                            == RendererDynamicRangeMode::Hdr
                    })
            })
        });
        for capability in RendererCapability::ALL {
            let Some(binding) = merged.get(&capability).copied() else {
                continue;
            };
            let required = binding.requirement == RendererCapabilityRequirement::Required
                || (capability == RendererCapability::HdrEdrOutput && overlay_requires_hdr);
            if !required {
                continue;
            }
            if let Some((reason, tracking_ref)) = capability_unavailability(&binding.availability)
                && !reason.trim().is_empty()
                && !tracking_ref.trim().is_empty()
            {
                validator.overlay_gap(
                    RendererScenarioGapCode::RequiredCapabilityUnavailable,
                    format!("{path}.{}", capability.as_str()),
                    format!(
                        "required capability `{}` is unavailable: {reason}",
                        capability.as_str()
                    ),
                    tracking_ref,
                    &scenario.scenario_id,
                    overlay_id,
                );
                blocking_codes.push(RendererScenarioGapCode::RequiredCapabilityUnavailable);
                if capability_blocks_native_capture(capability) {
                    validator.overlay_gap(
                        RendererScenarioGapCode::NativeCaptureUnavailable,
                        format!("{scenario_path}.visual_checkpoints"),
                        format!(
                            "native checkpoint capture is blocked by `{}`: {reason}",
                            capability.as_str()
                        ),
                        tracking_ref,
                        &scenario.scenario_id,
                        overlay_id,
                    );
                    blocking_codes.push(RendererScenarioGapCode::NativeCaptureUnavailable);
                }
            }
        }
        if let Some(profile) = profile
            && let Some(config) = index
                .renderer_config_profiles
                .get(profile.renderer_config_profile_id.as_str())
            && let RendererConfigurationAvailability::Unavailable {
                reason,
                tracking_refs,
            } = &config.availability
            && let Some(tracking_ref) = tracking_refs.first()
        {
            validator.overlay_gap(
                RendererScenarioGapCode::RendererConfigurationUnavailable,
                format!("{scenario_path}.coverage_overlay_profile_ids"),
                reason,
                tracking_ref,
                &scenario.scenario_id,
                overlay_id,
            );
            blocking_codes.push(RendererScenarioGapCode::RendererConfigurationUnavailable);
        }
        let mut unavailable_content = BTreeSet::new();
        if let Some(overlay) = overlays.get(&overlay_id)
            && let Some(manifest) = overlay.anchors.first()
            && let Some(distribution) = index
                .content_distribution_profiles
                .get(manifest.content_distribution_profile_id.as_str())
        {
            for assignment in &distribution.assignments {
                for step in &assignment.materialization_steps {
                    let unavailable = match &step.availability {
                        RendererContentInputAvailability::Unavailable {
                            reason,
                            tracking_refs,
                        } => tracking_refs.first().map(|tracking| {
                            (reason.as_str(), tracking.as_str())
                        }),
                        RendererContentInputAvailability::Available => index
                            .content
                            .get(step.content_corpus_id.as_str())
                            .and_then(|reference| match &reference.availability {
                                RendererContentInputAvailability::Unavailable {
                                    reason,
                                    tracking_refs,
                                } => tracking_refs.first().map(|tracking| {
                                    (reason.as_str(), tracking.as_str())
                                }),
                                RendererContentInputAvailability::Available => None,
                            }),
                    };
                    if let Some((reason, tracking_ref)) = unavailable
                        && unavailable_content.insert(step.content_corpus_id.as_str())
                    {
                        validator.overlay_gap(
                            RendererScenarioGapCode::ContentMaterializationUnavailable,
                            format!("{scenario_path}.materialization.{}", overlay_id.as_str()),
                            format!(
                                "content `{}` cannot be materialized: {reason}",
                                step.content_corpus_id
                            ),
                            tracking_ref,
                            &scenario.scenario_id,
                            overlay_id,
                        );
                        blocking_codes
                            .push(RendererScenarioGapCode::ContentMaterializationUnavailable);
                    }
                }
            }
        }
        if scenario.gesture == RendererGesture::OutputOverlapResize {
            validator.overlay_gap(
                RendererScenarioGapCode::DeterministicOutputStreamUnavailable,
                format!("{scenario_path}.workload_id.output_stream"),
                "renderer_output_stream_v1 has no closed implementation, manifest, and digest",
                RENDERER_OUTPUT_AUTHORITY_TRACKING_REF,
                &scenario.scenario_id,
                overlay_id,
            );
            blocking_codes.push(
                RendererScenarioGapCode::DeterministicOutputStreamUnavailable,
            );
            if overlay_id == RendererCoverageOverlayId::ProductionDefault {
                validator.overlay_gap(
                    RendererScenarioGapCode::KeyEffectOracleUnavailable,
                    format!("{scenario_path}.workload_id.foreground_key_events"),
                    "keypress-to-first-correct-present lacks a foreground fixture, PTY echo, and pre/post terminal-state oracle",
                    RENDERER_OUTPUT_AUTHORITY_TRACKING_REF,
                    &scenario.scenario_id,
                    overlay_id,
                );
                blocking_codes.push(RendererScenarioGapCode::KeyEffectOracleUnavailable);
            }
        }
        validator.record_overlay_readiness(
            &scenario.scenario_id,
            overlay_id,
            blocking_codes,
        );
    }
}

fn validate_measurement_bindings(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: &RendererWorkloadDefinition,
    facts: &TimelineFacts,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.measurement_bindings");
    let mutation_ordinals = facts
        .resize_event_ordinals
        .iter()
        .chain(&facts.font_event_ordinals)
        .chain(&facts.display_event_ordinals)
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut expected_roles = vec![
        RendererMeasurementRole::FirstCorrectViewport;
        mutation_ordinals.len()
    ];
    expected_roles.push(RendererMeasurementRole::SteadyPresentedFps);
    if matches!(
        scenario.gesture,
        RendererGesture::Reflow80To200 | RendererGesture::Reflow200To80
    ) {
        expected_roles.push(RendererMeasurementRole::ColdReflowConvergence);
    }
    if gesture_is_live_resize(scenario.gesture) {
        expected_roles.push(RendererMeasurementRole::SnapBack);
    }
    expected_roles.extend(std::iter::repeat_n(
        RendererMeasurementRole::KeypressToFirstCorrectPresent,
        facts.key_actions.len(),
    ));
    let actual_roles = scenario
        .measurement_bindings
        .iter()
        .map(RendererMeasurementBinding::role)
        .collect::<Vec<_>>();
    if actual_roles != expected_roles {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            &path,
            format!("measurement bindings require exact role order {expected_roles:?}"),
        );
    }
    let mut first_correct_ordinals = Vec::new();
    let mut key_bindings = Vec::new();
    let final_ordinal = scenario.timeline.last().map(|event| event.event_ordinal);
    for (position, binding) in scenario.measurement_bindings.iter().enumerate() {
        let binding_path = format!("{path}[{position}]");
        if binding.overlay_id() != RendererCoverageOverlayId::ProductionDefault {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{binding_path}.overlay_id"),
                "exact measurement endpoints are reserved for production_default",
            );
        }
        let (observed_boundary, presentation_ids) = match binding {
            RendererMeasurementBinding::FirstCorrectViewport {
                observed_boundary,
                presentation_target_profile_ids,
                ..
            }
            | RendererMeasurementBinding::SteadyPresentedFps {
                observed_boundary,
                presentation_target_profile_ids,
                ..
            }
            | RendererMeasurementBinding::ColdReflowConvergence {
                observed_boundary,
                presentation_target_profile_ids,
                ..
            }
            | RendererMeasurementBinding::SnapBack {
                observed_boundary,
                presentation_target_profile_ids,
                ..
            }
            | RendererMeasurementBinding::KeypressToFirstCorrectPresent {
                observed_boundary,
                presentation_target_profile_ids,
                ..
            } => (*observed_boundary, presentation_target_profile_ids),
        };
        if observed_boundary != RendererObservedBoundaryClass::DisplayPresented {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{binding_path}.observed_boundary"),
                "all v1 measurements terminate at an actual display-presented boundary",
            );
        }
        if presentation_ids != &scenario.presentation_target_profile_ids {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{binding_path}.presentation_target_profile_ids"),
                "measurement cadence profiles must exactly equal the scenario request",
            );
        }
        for profile_id in presentation_ids {
            if !index.presentation_profiles.contains_key(profile_id) {
                validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    format!("{binding_path}.presentation_target_profile_ids"),
                    "measurement names an undefined presentation profile",
                );
            }
        }
        match binding {
            RendererMeasurementBinding::FirstCorrectViewport {
                mutation_event_ordinal,
                target_selection,
                target_presented_frame_predicate_ref,
                required_stage_ids,
                ..
            } => {
                first_correct_ordinals.push(*mutation_event_ordinal);
                if *target_selection != RendererObservedFrameSelection::FirstSatisfyingObservedFrame
                    || required_stage_ids.as_slice() != RendererResizeTraceStage::ALL
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                        &binding_path,
                        "first-correct viewport requires first-satisfying selection and the complete R0-R25 trace",
                    );
                }
                validator.require_repository_ref(
                    &format!("{binding_path}.target_presented_frame_predicate_ref"),
                    target_presented_frame_predicate_ref,
                );
            }
            RendererMeasurementBinding::SteadyPresentedFps {
                interval_start_event_ordinal,
                interval_end_event_ordinal,
                minimum_presented_interval_count,
                presented_interval_contract_ref,
                required_stage_ids,
                ..
            } => {
                if interval_start_event_ordinal >= interval_end_event_ordinal
                    || Some(*interval_end_event_ordinal) != final_ordinal
                    || *minimum_presented_interval_count == 0
                    || required_stage_ids.as_slice() != RendererResizeTraceStage::ALL
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                        &binding_path,
                        "steady presented FPS requires a nonempty interval ending at final Settle and the complete R0-R25 trace",
                    );
                }
                validator.require_repository_ref(
                    &format!("{binding_path}.presented_interval_contract_ref"),
                    presented_interval_contract_ref,
                );
            }
            RendererMeasurementBinding::ColdReflowConvergence {
                trigger_event_ordinal,
                target_checkpoint_id,
                target_selection,
                target_presented_frame_predicate_ref,
                preconditioning_profile_id,
                required_stage_ids,
                ..
            } => {
                let final_checkpoint = checkpoint_for_role(
                    scenario,
                    RendererCoverageOverlayId::ProductionDefault,
                    RendererCheckpointRole::FinalSteadyState,
                );
                if !matches!(
                    scenario.gesture,
                    RendererGesture::Reflow80To200 | RendererGesture::Reflow200To80
                ) || facts.resize_event_ordinals.first() != Some(trigger_event_ordinal)
                    || final_checkpoint.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        != Some(target_checkpoint_id.as_str())
                    || *target_selection
                        != RendererObservedFrameSelection::FirstSatisfyingObservedFrame
                    || *preconditioning_profile_id != RendererPreconditioningProfileId::Cold
                    || required_stage_ids.as_slice() != RendererResizeTraceStage::ALL
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                        &binding_path,
                        "cold reflow convergence requires the first reflow mutation, final production checkpoint, cold precondition, and complete R0-R25 trace",
                    );
                }
                if !index
                    .preconditioning_profiles
                    .contains_key(preconditioning_profile_id)
                {
                    validator.error(
                        RendererScenarioValidationCode::DanglingReference,
                        format!("{binding_path}.preconditioning_profile_id"),
                        "cold reflow names an undefined precondition",
                    );
                }
                validator.require_repository_ref(
                    &format!("{binding_path}.target_presented_frame_predicate_ref"),
                    target_presented_frame_predicate_ref,
                );
            }
            RendererMeasurementBinding::SnapBack {
                last_draft_checkpoint_id,
                standard_snap_back_subject_checkpoint_id,
                independent_standard_oracle_ref,
                target_selection,
                target_presented_frame_predicate_ref,
                required_stage_ids,
                ..
            } => {
                let last_draft = checkpoint_for_role(
                    scenario,
                    RendererCoverageOverlayId::ProductionDefault,
                    RendererCheckpointRole::LastDraftProvenance,
                );
                let snap_back = checkpoint_for_role(
                    scenario,
                    RendererCoverageOverlayId::ProductionDefault,
                    RendererCheckpointRole::StandardSnapBackSubject,
                );
                if !gesture_is_live_resize(scenario.gesture)
                    || last_draft.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        != Some(last_draft_checkpoint_id.as_str())
                    || snap_back.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        != Some(standard_snap_back_subject_checkpoint_id.as_str())
                    || snap_back
                        .and_then(|checkpoint| {
                            checkpoint.independent_standard_oracle_ref.as_deref()
                        }) != Some(independent_standard_oracle_ref.as_str())
                    || *target_selection
                        != RendererObservedFrameSelection::FirstSatisfyingObservedFrame
                    || required_stage_ids.as_slice() != RendererResizeTraceStage::ALL
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                        &binding_path,
                        "snap-back measurement requires the exact Draft/Standard endpoints, independent oracle, and complete R0-R25 trace",
                    );
                }
                validator.require_repository_ref(
                    &format!("{binding_path}.independent_standard_oracle_ref"),
                    independent_standard_oracle_ref,
                );
                validator.require_repository_ref(
                    &format!("{binding_path}.target_presented_frame_predicate_ref"),
                    target_presented_frame_predicate_ref,
                );
            }
            RendererMeasurementBinding::KeypressToFirstCorrectPresent {
                key_event_id,
                key_action_event_ordinal,
                target_selection,
                target_presented_frame_predicate_ref,
                expected_terminal_effect_oracle_ref,
                stage_metrics_contract_ref,
                required_stage_ids,
                ..
            } => {
                key_bindings.push((*key_action_event_ordinal, key_event_id.as_str()));
                if *target_selection != RendererObservedFrameSelection::FirstSatisfyingObservedFrame
                    || required_stage_ids.as_slice() != RendererKeypressTraceStage::ALL
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                        &binding_path,
                        "keypress measurement requires first-satisfying selection and the complete K0-K13 trace",
                    );
                }
                for (field, repository_ref) in [
                    (
                        "target_presented_frame_predicate_ref",
                        target_presented_frame_predicate_ref,
                    ),
                    (
                        "expected_terminal_effect_oracle_ref",
                        expected_terminal_effect_oracle_ref,
                    ),
                    ("stage_metrics_contract_ref", stage_metrics_contract_ref),
                ] {
                    validator.require_repository_ref(
                        &format!("{binding_path}.{field}"),
                        repository_ref,
                    );
                }
            }
        }
    }
    if first_correct_ordinals != mutation_ordinals {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            &path,
            format!(
                "first-correct viewport requires exactly one binding per mutation bundle {mutation_ordinals:?}"
            ),
        );
    }
    let expected_keys = facts
        .key_actions
        .iter()
        .map(|(ordinal, id)| (*ordinal, id.as_str()))
        .collect::<Vec<_>>();
    if key_bindings != expected_keys
        || workload.foreground_key_events.len() != expected_keys.len()
    {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            &path,
            "keypress measurements must exactly bind every pinned workload key action in order",
        );
    }
}

const fn expected_negative_control_binding(
    control_id: RendererNegativeControlId,
) -> (RendererCheckpointDetectorId, Option<RendererTerminalFeature>) {
    match control_id {
        RendererNegativeControlId::MissingGlyph => (
            RendererCheckpointDetectorId::NoMissingGlyphs,
            Some(RendererTerminalFeature::Ascii),
        ),
        RendererNegativeControlId::MixedRendererGeneration => (
            RendererCheckpointDetectorId::CoherentRendererGeneration,
            None,
        ),
        RendererNegativeControlId::CursorDisplacement => (
            RendererCheckpointDetectorId::CursorGeometry,
            Some(RendererTerminalFeature::Cursor),
        ),
        RendererNegativeControlId::SelectionLoss => (
            RendererCheckpointDetectorId::SelectionGeometry,
            Some(RendererTerminalFeature::Selection),
        ),
        RendererNegativeControlId::StaleImage => (
            RendererCheckpointDetectorId::ImageGeometry,
            Some(RendererTerminalFeature::Images),
        ),
        RendererNegativeControlId::ImeGeometryDisplacement => (
            RendererCheckpointDetectorId::ImeGeometry,
            Some(RendererTerminalFeature::Ime),
        ),
        RendererNegativeControlId::HyperlinkRangeCorruption => (
            RendererCheckpointDetectorId::HyperlinkGeometry,
            Some(RendererTerminalFeature::Hyperlinks),
        ),
        RendererNegativeControlId::AlternateScreenFlip => (
            RendererCheckpointDetectorId::AlternateScreenState,
            Some(RendererTerminalFeature::AlternateScreen),
        ),
        RendererNegativeControlId::GridDimensionMismatch => {
            (RendererCheckpointDetectorId::ExactRowWidth, None)
        }
        RendererNegativeControlId::DuplicateStaleFrame => (
            RendererCheckpointDetectorId::NoStaleOrDuplicateFrame,
            None,
        ),
        RendererNegativeControlId::AccessibilityGeometryDisplacement => (
            RendererCheckpointDetectorId::AccessibilityGeometry,
            Some(RendererTerminalFeature::AccessibilityGeometry),
        ),
        RendererNegativeControlId::BlankFrameAfterNonblank => (
            RendererCheckpointDetectorId::NonblankAfterBaseline,
            None,
        ),
        RendererNegativeControlId::MixedGenerationTearBand => (
            RendererCheckpointDetectorId::NoMixedGenerationTearBand,
            None,
        ),
    }
}

fn validate_negative_controls(
    catalog: &RendererScenarioCatalog,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = "$.negative_controls";
    if catalog.negative_controls.len() != RendererNegativeControlId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidNegativeControl,
            path,
            format!(
                "expected exactly {} deliberate-defect controls, found {}",
                RendererNegativeControlId::ALL.len(),
                catalog.negative_controls.len()
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for (position, control) in catalog.negative_controls.iter().enumerate() {
        let control_path = format!("{path}[{position}]");
        if RendererNegativeControlId::ALL.get(position) != Some(&control.control_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidNegativeControl,
                format!("{control_path}.control_id"),
                "negative controls must appear in canonical identity order",
            );
        }
        if !seen.insert(control.control_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidNegativeControl,
                format!("{control_path}.control_id"),
                "duplicate negative-control identity",
            );
        }
        let (expected_detector, expected_feature) =
            expected_negative_control_binding(control.control_id);
        if control.bound_detector_id != expected_detector
            || control.required_feature != expected_feature
            || control.expected_failure_code != control.control_id.expected_failure_code()
        {
            validator.error(
                RendererScenarioValidationCode::InvalidNegativeControl,
                &control_path,
                format!(
                    "control requires detector `{}`, feature {expected_feature:?}, and failure code `{}`",
                    expected_detector.as_str(),
                    control.control_id.expected_failure_code()
                ),
            );
        }
        validator.require_identifier(
            &format!("{control_path}.scenario_id"),
            &control.scenario_id,
        );
        validator.require_identifier(
            &format!("{control_path}.checkpoint_id"),
            &control.checkpoint_id,
        );
        let Some(scenario) = catalog
            .scenarios
            .iter()
            .find(|scenario| scenario.scenario_id == control.scenario_id)
        else {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{control_path}.scenario_id"),
                format!("undefined scenario `{}`", control.scenario_id),
            );
            continue;
        };
        let checkpoint = scenario.visual_checkpoints.iter().find(|checkpoint| {
            checkpoint.overlay_id == control.overlay_id
                && checkpoint.checkpoint_id == control.checkpoint_id
        });
        match checkpoint {
            Some(checkpoint) if checkpoint.phase != control.injected_phase => validator.error(
                RendererScenarioValidationCode::InvalidNegativeControl,
                format!("{control_path}.injected_phase"),
                "injected phase must equal the bound checkpoint phase",
            ),
            None => validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{control_path}.checkpoint_id"),
                "control checkpoint is absent from its scenario overlay",
            ),
            Some(_) => {}
        }
        if let Some(feature) = control.required_feature
            && !expected_overlay_features(control.overlay_id).contains(&feature)
        {
            validator.error(
                RendererScenarioValidationCode::InvalidNegativeControl,
                format!("{control_path}.required_feature"),
                "control feature is absent from the selected overlay",
            );
        }
        let detector_observed = scenario
            .observed_frame_policies
            .iter()
            .find(|policy| policy.overlay_id == control.overlay_id)
            .is_some_and(|policy| {
                policy
                    .all_frame_detector_ids
                    .contains(&control.bound_detector_id)
            });
        if !detector_observed {
            validator.error(
                RendererScenarioValidationCode::InvalidNegativeControl,
                format!("{control_path}.bound_detector_id"),
                "bound detector is not active over all observed frames for this overlay",
            );
        }
        if !index
            .detector_contracts
            .contains_key(&control.bound_detector_id)
        {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{control_path}.bound_detector_id"),
                "negative control names an undefined detector contract",
            );
        }
    }
}

fn expected_scenario_invariant_ids(
    scenario: &RendererScenarioDefinition,
) -> Vec<&'static str> {
    let needs_reflow_identity = matches!(
        scenario.gesture,
        RendererGesture::GridChangingDrag
            | RendererGesture::Reflow80To200
            | RendererGesture::Reflow200To80
    ) || (scenario.gesture == RendererGesture::OutputOverlapResize
        && scenario.output_overlap_resize_mode == Some(RendererResizeMode::GridChanging));
    REQUIRED_RENDERER_INVARIANT_IDS
        .into_iter()
        .filter(|invariant_id| {
            *invariant_id != "reflow_logical_line_identity" || needs_reflow_identity
        })
        .collect()
}

fn invariant_applies_to_resolved_features(
    invariant_id: &str,
    features: &BTreeSet<RendererTerminalFeature>,
) -> bool {
    match invariant_id {
        "alternate_screen_isolation" => {
            features.contains(&RendererTerminalFeature::AlternateScreen)
        }
        "accessibility_focus_geometry" => {
            features.contains(&RendererTerminalFeature::AccessibilityGeometry)
        }
        _ => true,
    }
}

fn expected_invariant_phases(
    invariant_id: &str,
    live_resize: bool,
) -> Option<Vec<RendererTimelinePhase>> {
    let with_snap_back = |mut phases: Vec<RendererTimelinePhase>| {
        if live_resize {
            let settle_position = phases
                .iter()
                .position(|phase| *phase == RendererTimelinePhase::Settle)
                .unwrap_or(phases.len());
            phases.insert(settle_position, RendererTimelinePhase::SnapBack);
        }
        phases
    };
    match invariant_id {
        "no_blank_frame_after_nonblank" | "no_stale_full_frame_reuse" => Some(
            with_snap_back(vec![
                RendererTimelinePhase::Mutation,
                RendererTimelinePhase::Settle,
            ]),
        ),
        "coherent_grid_terminal_revision"
        | "anchors_in_bounds"
        | "alternate_screen_isolation"
        | "accessibility_focus_geometry" => Some(with_snap_back(vec![
            RendererTimelinePhase::Begin,
            RendererTimelinePhase::Mutation,
            RendererTimelinePhase::Settle,
        ])),
        "reflow_logical_line_identity" => Some(with_snap_back(vec![
            RendererTimelinePhase::Mutation,
            RendererTimelinePhase::Settle,
        ])),
        "final_state_convergence" => Some(vec![RendererTimelinePhase::Settle]),
        _ => None,
    }
}

fn validate_expected_invariants(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.expected_invariants");
    let expected_ids = expected_scenario_invariant_ids(scenario);
    if scenario.expected_invariants.len() != expected_ids.len()
        || scenario.expected_invariants.len() > MAX_RENDERER_EXPECTED_INVARIANTS
    {
        validator.error(
            RendererScenarioValidationCode::InvalidInvariant,
            &path,
            format!(
                "scenario requires exactly {} applicable invariants, found {}",
                expected_ids.len(),
                scenario.expected_invariants.len()
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for (position, invariant) in scenario.expected_invariants.iter().enumerate() {
        let invariant_path = format!("{path}[{position}]");
        validator.require_identifier(
            &format!("{invariant_path}.invariant_id"),
            &invariant.invariant_id,
        );
        validator.require_repository_ref(
            &format!("{invariant_path}.oracle_ref"),
            &invariant.oracle_ref,
        );
        if !seen.insert(invariant.invariant_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{invariant_path}.invariant_id"),
                format!("duplicate invariant `{}`", invariant.invariant_id),
            );
        }
        match expected_invariant_phases(
            &invariant.invariant_id,
            gesture_is_live_resize(scenario.gesture),
        ) {
            Some(expected_phases)
                if invariant.applicable_phases.as_slice() == expected_phases.as_slice() => {}
            Some(expected_phases) => validator.error(
                RendererScenarioValidationCode::InvalidInvariant,
                format!("{invariant_path}.applicable_phases"),
                format!(
                    "invariant `{}` requires exact phases {:?}",
                    invariant.invariant_id, expected_phases
                ),
            ),
            None => validator.error(
                RendererScenarioValidationCode::InvalidInvariant,
                format!("{invariant_path}.invariant_id"),
                format!(
                    "invariant `{}` is outside the closed inventory",
                    invariant.invariant_id
                ),
            ),
        }
    }
    let actual_ids = scenario
        .expected_invariants
        .iter()
        .map(|invariant| invariant.invariant_id.as_str())
        .collect::<Vec<_>>();
    if actual_ids != expected_ids {
        validator.error(
            RendererScenarioValidationCode::InvalidInvariant,
            &path,
            format!("invariants must equal canonical applicable order {expected_ids:?}"),
        );
    }
}

fn expected_checkpoint_invariant_ids<'a>(
    phase: RendererTimelinePhase,
    scenario_invariant_ids: impl Iterator<Item = &'a str>,
    live_resize: bool,
    resolved_features: &BTreeSet<RendererTerminalFeature>,
) -> Vec<&'a str> {
    scenario_invariant_ids
        .filter(|invariant_id| {
            invariant_applies_to_resolved_features(invariant_id, resolved_features)
                && expected_invariant_phases(invariant_id, live_resize)
                    .is_some_and(|phases| phases.contains(&phase))
        })
        .collect()
}

fn effective_quality_at_event(
    initial_quality: RendererQualityMode,
    scenario: &RendererScenarioDefinition,
    event_ordinal: u32,
) -> RendererQualityMode {
    let mut quality = initial_quality;
    for event in scenario
        .timeline
        .iter()
        .take_while(|event| event.event_ordinal <= event_ordinal)
    {
        for action in &event.actions {
            if let RendererTimelineAction::SetQualityMode { mode, .. } = action {
                quality = *mode;
            }
        }
    }
    quality
}

fn validate_visual_checkpoints(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    overlays: &BTreeMap<RendererCoverageOverlayId, ResolvedScenarioOverlay<'_>>,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.visual_checkpoints");
    let per_overlay = if gesture_is_live_resize(scenario.gesture) {
        4
    } else {
        3
    };
    let expected_count = RendererCoverageOverlayId::ALL.len() * per_overlay;
    if scenario.visual_checkpoints.len() != expected_count
        || scenario.visual_checkpoints.len() > MAX_RENDERER_CHECKPOINTS
    {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            format!(
                "scenario requires exactly {expected_count} overlay checkpoints, found {}",
                scenario.visual_checkpoints.len()
            ),
        );
    }
    let final_ordinal = scenario.timeline.last().map(|event| event.event_ordinal);
    let last_mutation_ordinal = scenario
        .timeline
        .iter()
        .rev()
        .find(|event| event.phase == RendererTimelinePhase::Mutation)
        .map(|event| event.event_ordinal);
    let snap_back_ordinal = scenario
        .timeline
        .iter()
        .find(|event| event.phase == RendererTimelinePhase::SnapBack)
        .map(|event| event.event_ordinal);
    let mut checkpoint_ids = BTreeSet::new();
    for (position, checkpoint) in scenario.visual_checkpoints.iter().enumerate() {
        let checkpoint_path = format!("{path}[{position}]");
        validator.require_identifier(
            &format!("{checkpoint_path}.checkpoint_id"),
            &checkpoint.checkpoint_id,
        );
        if !checkpoint_ids.insert(checkpoint.checkpoint_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{checkpoint_path}.checkpoint_id"),
                format!("duplicate checkpoint `{}`", checkpoint.checkpoint_id),
            );
        }
        let expected_overlay = RendererCoverageOverlayId::ALL.get(position / per_overlay);
        if expected_overlay != Some(&checkpoint.overlay_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.overlay_id"),
                "checkpoints must be grouped in canonical overlay order",
            );
        }
        if !overlays.contains_key(&checkpoint.overlay_id) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{checkpoint_path}.overlay_id"),
                "checkpoint names an unresolved overlay",
            );
        }
        match scenario.timeline.get(checkpoint.event_ordinal as usize) {
            Some(event) if event.phase == checkpoint.phase => {}
            Some(event) => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &checkpoint_path,
                format!(
                    "checkpoint phase {:?} differs from timeline phase {:?}",
                    checkpoint.phase, event.phase
                ),
            ),
            None => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.event_ordinal"),
                "checkpoint event is outside the timeline",
            ),
        }
        let expected_role_ordinal = position % per_overlay;
        let expected_role = if gesture_is_live_resize(scenario.gesture) {
            [
                RendererCheckpointRole::InitialBaseline,
                RendererCheckpointRole::LastDraftProvenance,
                RendererCheckpointRole::StandardSnapBackSubject,
                RendererCheckpointRole::FinalSteadyState,
            ][expected_role_ordinal]
        } else {
            [
                RendererCheckpointRole::InitialBaseline,
                RendererCheckpointRole::Intermediate,
                RendererCheckpointRole::FinalSteadyState,
            ][expected_role_ordinal]
        };
        let expected_phase = match expected_role {
            RendererCheckpointRole::InitialBaseline => RendererTimelinePhase::Begin,
            RendererCheckpointRole::LastDraftProvenance
            | RendererCheckpointRole::Intermediate => RendererTimelinePhase::Mutation,
            RendererCheckpointRole::StandardSnapBackSubject => {
                RendererTimelinePhase::SnapBack
            }
            RendererCheckpointRole::FinalSteadyState => RendererTimelinePhase::Settle,
        };
        let expected_event = match expected_role {
            RendererCheckpointRole::InitialBaseline => Some(0),
            RendererCheckpointRole::LastDraftProvenance
            | RendererCheckpointRole::Intermediate => last_mutation_ordinal,
            RendererCheckpointRole::StandardSnapBackSubject => snap_back_ordinal,
            RendererCheckpointRole::FinalSteadyState => final_ordinal,
        };
        if checkpoint.role != expected_role
            || checkpoint.phase != expected_phase
            || Some(checkpoint.event_ordinal) != expected_event
        {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &checkpoint_path,
                format!(
                    "expected role {expected_role:?}, phase {expected_phase:?}, event {expected_event:?}"
                ),
            );
        }
        let expected_content_class = match expected_role {
            RendererCheckpointRole::InitialBaseline => {
                RendererFrameContentClass::NonblankBaseline
            }
            RendererCheckpointRole::LastDraftProvenance
            | RendererCheckpointRole::Intermediate => {
                RendererFrameContentClass::NonblankTransient
            }
            RendererCheckpointRole::StandardSnapBackSubject => {
                RendererFrameContentClass::NonblankStandardSnapBack
            }
            RendererCheckpointRole::FinalSteadyState => {
                RendererFrameContentClass::NonblankSteadyState
            }
        };
        if checkpoint.expected_frame_content_class != expected_content_class {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.expected_frame_content_class"),
                format!("checkpoint role requires {expected_content_class:?}"),
            );
        }
        if !checkpoint.native_capture_required {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.native_capture_required"),
                "every v1 checkpoint requires native capture",
            );
        }
        for (field, repository_ref) in [
            ("state_oracle_ref", &checkpoint.state_oracle_ref),
            ("visual_oracle_ref", &checkpoint.visual_oracle_ref),
            ("accessibility_oracle_ref", &checkpoint.accessibility_oracle_ref),
        ] {
            validator.require_repository_ref(
                &format!("{checkpoint_path}.{field}"),
                repository_ref,
            );
        }
        match expected_role {
            RendererCheckpointRole::StandardSnapBackSubject => {
                let Some(independent_ref) = &checkpoint.independent_standard_oracle_ref else {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCheckpoint,
                        format!("{checkpoint_path}.independent_standard_oracle_ref"),
                        "snap-back subject requires an independent Standard oracle",
                    );
                    continue;
                };
                validator.require_repository_ref(
                    &format!("{checkpoint_path}.independent_standard_oracle_ref"),
                    independent_ref,
                );
            }
            _ if checkpoint.independent_standard_oracle_ref.is_some() => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.independent_standard_oracle_ref"),
                "only the snap-back subject may name an independent Standard oracle",
            ),
            _ => {}
        }
        let expected_policies: &[&str] = match expected_role {
            RendererCheckpointRole::LastDraftProvenance => &[],
            RendererCheckpointRole::StandardSnapBackSubject => {
                &[RQ_S11_COMPARATOR_POLICY_REF, RQ_S13_COMPARATOR_POLICY_REF]
            }
            RendererCheckpointRole::InitialBaseline
            | RendererCheckpointRole::Intermediate
            | RendererCheckpointRole::FinalSteadyState => {
                &[RQ_S13_COMPARATOR_POLICY_REF]
            }
        };
        let actual_policies = checkpoint
            .comparator_policy_refs
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if actual_policies != expected_policies {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.comparator_policy_refs"),
                format!("checkpoint requires exact comparator policies {expected_policies:?}"),
            );
        }
        for (policy_position, policy_ref) in
            checkpoint.comparator_policy_refs.iter().enumerate()
        {
            validator.require_repository_ref(
                &format!(
                    "{checkpoint_path}.comparator_policy_refs[{policy_position}]"
                ),
                policy_ref,
            );
        }
        let resolved_features = index
            .phase_manifests
            .get(checkpoint.phase_manifest_id.as_str())
            .copied()
            .zip(index.workloads.get(scenario.workload_id.as_str()).copied())
            .and_then(|(manifest, workload)| {
                expand_manifest_state(manifest, index, scenario, checkpoint, workload)
                    .map(|state| state.features)
                    .map_err(|detail| {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            format!("{checkpoint_path}.phase_manifest_id"),
                            format!("cannot derive overlay-aware invariants: {detail}"),
                        );
                    })
                    .ok()
            });
        if let Some(resolved_features) = resolved_features {
            let expected_invariants = expected_checkpoint_invariant_ids(
                checkpoint.phase,
                scenario
                    .expected_invariants
                    .iter()
                    .map(|invariant| invariant.invariant_id.as_str()),
                gesture_is_live_resize(scenario.gesture),
                &resolved_features,
            );
            let actual_invariants = checkpoint
                .expected_invariant_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            if actual_invariants != expected_invariants {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{checkpoint_path}.expected_invariant_ids"),
                    format!(
                        "checkpoint requires complete overlay-aware canonical invariant set {expected_invariants:?}"
                    ),
                );
            }
        }
        if let Some(overlay) = overlays.get(&checkpoint.overlay_id)
            && let Some(initial) = overlay.anchors.first()
            && let Some(initial_checkpoint) = overlay.checkpoints.first()
            && let Some(workload) = index.workloads.get(scenario.workload_id.as_str())
            && let Ok(initial_state) = expand_manifest_state(
                initial,
                index,
                scenario,
                initial_checkpoint,
                workload,
            )
            && let Some(initial_surface) = initial_state.surfaces.first()
        {
            let effective_quality = effective_quality_at_event(
                initial_surface.quality_mode,
                scenario,
                checkpoint.event_ordinal,
            );
            let expected_quality = match checkpoint.role {
                RendererCheckpointRole::LastDraftProvenance => {
                    RendererQualityMode::Draft
                }
                RendererCheckpointRole::StandardSnapBackSubject => {
                    RendererQualityMode::Standard
                }
                RendererCheckpointRole::FinalSteadyState => {
                    scenario.configured_steady_quality
                }
                RendererCheckpointRole::InitialBaseline
                | RendererCheckpointRole::Intermediate => initial_surface.quality_mode,
            };
            if effective_quality != expected_quality {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    &checkpoint_path,
                    format!(
                        "checkpoint role requires quality {expected_quality:?}, found {effective_quality:?}"
                    ),
                );
            }
        }
    }
}

const UNIVERSAL_ALL_FRAME_DETECTORS: [RendererCheckpointDetectorId; 9] = [
    RendererCheckpointDetectorId::NoMissingGlyphs,
    RendererCheckpointDetectorId::CoherentCellWidths,
    RendererCheckpointDetectorId::ExactRowWidth,
    RendererCheckpointDetectorId::CoherentRendererGeneration,
    RendererCheckpointDetectorId::NoMixedGenerationTearBand,
    RendererCheckpointDetectorId::NoStaleOrDuplicateFrame,
    RendererCheckpointDetectorId::NonblankAfterBaseline,
    RendererCheckpointDetectorId::ExactTerminalState,
    RendererCheckpointDetectorId::CursorGeometry,
];

fn expected_all_frame_detectors(
    overlay_id: RendererCoverageOverlayId,
) -> Vec<RendererCheckpointDetectorId> {
    let mut expected = UNIVERSAL_ALL_FRAME_DETECTORS.to_vec();
    match overlay_id {
        RendererCoverageOverlayId::AlternateScreen => {
            expected.push(RendererCheckpointDetectorId::AlternateScreenState);
        }
        RendererCoverageOverlayId::ImeComposing => {
            expected.push(RendererCheckpointDetectorId::ImeGeometry);
        }
        RendererCoverageOverlayId::ImageHyperlink => {
            expected.push(RendererCheckpointDetectorId::HyperlinkGeometry);
            expected.push(RendererCheckpointDetectorId::ImageGeometry);
        }
        RendererCoverageOverlayId::Selection => {
            expected.push(RendererCheckpointDetectorId::SelectionGeometry);
        }
        RendererCoverageOverlayId::A11yGeometry => {
            expected.push(RendererCheckpointDetectorId::AccessibilityGeometry);
        }
        RendererCoverageOverlayId::ProductionDefault
        | RendererCoverageOverlayId::UnicodeMaximal
        | RendererCoverageOverlayId::LigatureEnabled => {}
    }
    expected.sort_by_key(|detector| {
        RendererCheckpointDetectorId::ALL
            .iter()
            .position(|candidate| candidate == detector)
            .unwrap_or(RendererCheckpointDetectorId::ALL.len())
    });
    expected
}

fn validate_observed_frame_policies(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.observed_frame_policies");
    if scenario.observed_frame_policies.len() != RendererCoverageOverlayId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            format!(
                "expected exactly eight observed-frame policies, found {}",
                scenario.observed_frame_policies.len()
            ),
        );
    }
    let final_ordinal = scenario.timeline.last().map(|event| event.event_ordinal);
    let mut overlays = BTreeSet::new();
    for (position, policy) in scenario.observed_frame_policies.iter().enumerate() {
        let policy_path = format!("{path}[{position}]");
        if RendererCoverageOverlayId::ALL.get(position) != Some(&policy.overlay_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{policy_path}.overlay_id"),
                "observed-frame policies must appear in canonical overlay order",
            );
        }
        if !overlays.insert(policy.overlay_id) {
            validator.error(
                RendererScenarioValidationCode::DuplicateCoverageCell,
                format!("{policy_path}.overlay_id"),
                "duplicate observed-frame policy overlay",
            );
        }
        validator.require_repository_ref(
            &format!("{policy_path}.observation_policy_ref"),
            &policy.observation_policy_ref,
        );
        if policy.start_event_ordinal != 0
            || Some(policy.end_event_ordinal) != final_ordinal
            || policy.required_frame_classes.as_slice() != RendererObservedFrameClass::ALL
            || !policy.require_monotonic_correlation
            || !policy.detect_dropped_correlations
            || !policy.detect_duplicate_correlations
            || policy.identity_fields.as_slice() != RendererObservedFrameIdentityField::ALL
        {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &policy_path,
                "observed-frame policy must span Begin through final Settle with all boundaries, correlation checks, and exact identity fields",
            );
        }
        let expected_detectors = expected_all_frame_detectors(policy.overlay_id);
        if policy.all_frame_detector_ids != expected_detectors {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{policy_path}.all_frame_detector_ids"),
                format!(
                    "overlay `{}` requires exact all-frame detector order {:?}",
                    policy.overlay_id.as_str(),
                    expected_detectors
                ),
            );
        }
        for (detector_position, detector_id) in
            policy.all_frame_detector_ids.iter().enumerate()
        {
            match index.detector_contracts.get(detector_id) {
                Some(contract) if contract.scope == RendererDetectorScope::AllObservedFrames => {}
                Some(_) => validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!(
                        "{policy_path}.all_frame_detector_ids[{detector_position}]"
                    ),
                    "policy may contain only all-observed-frames detectors",
                ),
                None => validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    format!(
                        "{policy_path}.all_frame_detector_ids[{detector_position}]"
                    ),
                    format!("undefined detector `{}`", detector_id.as_str()),
                ),
            }
        }
    }
}

fn checkpoint_for_role(
    scenario: &RendererScenarioDefinition,
    overlay_id: RendererCoverageOverlayId,
    role: RendererCheckpointRole,
) -> Option<&RendererVisualCheckpoint> {
    scenario
        .visual_checkpoints
        .iter()
        .find(|checkpoint| checkpoint.overlay_id == overlay_id && checkpoint.role == role)
}

fn validate_detector_bindings(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.detector_bindings");
    let per_overlay = if gesture_is_live_resize(scenario.gesture) {
        5
    } else {
        4
    };
    let expected_count = RendererCoverageOverlayId::ALL.len() * per_overlay;
    if scenario.detector_bindings.len() != expected_count {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            format!(
                "scenario requires exactly {expected_count} nonlocal detector bindings, found {}",
                scenario.detector_bindings.len()
            ),
        );
    }
    let expected_detector_order = if gesture_is_live_resize(scenario.gesture) {
        vec![
            RendererCheckpointDetectorId::NoFlicker,
            RendererCheckpointDetectorId::SsimPolicy,
            RendererCheckpointDetectorId::LInfPolicy,
            RendererCheckpointDetectorId::ChangedPixelFractionPolicy,
            RendererCheckpointDetectorId::ExactlyOneStandardSnapBack,
        ]
    } else {
        vec![
            RendererCheckpointDetectorId::NoFlicker,
            RendererCheckpointDetectorId::SsimPolicy,
            RendererCheckpointDetectorId::LInfPolicy,
            RendererCheckpointDetectorId::ChangedPixelFractionPolicy,
        ]
    };
    let mut seen = BTreeSet::new();
    for (position, binding) in scenario.detector_bindings.iter().enumerate() {
        let binding_path = format!("{path}[{position}]");
        let expected_overlay = RendererCoverageOverlayId::ALL.get(position / per_overlay);
        let expected_detector = expected_detector_order.get(position % per_overlay);
        if expected_overlay != Some(&binding.overlay_id())
            || expected_detector != Some(&binding.detector_id())
        {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &binding_path,
                "detector bindings must use exact per-overlay nonlocal detector order",
            );
        }
        if !seen.insert((binding.overlay_id(), binding.detector_id())) {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &binding_path,
                "duplicate overlay/detector binding",
            );
        }
        let Some(contract) = index.detector_contracts.get(&binding.detector_id()) else {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                &binding_path,
                format!("undefined detector `{}`", binding.detector_id().as_str()),
            );
            continue;
        };
        let initial = checkpoint_for_role(
            scenario,
            binding.overlay_id(),
            RendererCheckpointRole::InitialBaseline,
        );
        let final_checkpoint = checkpoint_for_role(
            scenario,
            binding.overlay_id(),
            RendererCheckpointRole::FinalSteadyState,
        );
        let comparison_subject = if gesture_is_live_resize(scenario.gesture) {
            checkpoint_for_role(
                scenario,
                binding.overlay_id(),
                RendererCheckpointRole::StandardSnapBackSubject,
            )
        } else {
            final_checkpoint
        };
        match binding {
            RendererDetectorBinding::Interval {
                detector_id,
                start_checkpoint_id,
                end_checkpoint_id,
                ..
            } => {
                if *detector_id != RendererCheckpointDetectorId::NoFlicker
                    || contract.scope != RendererDetectorScope::Interval
                    || initial.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        != Some(start_checkpoint_id.as_str())
                    || final_checkpoint.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        != Some(end_checkpoint_id.as_str())
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCheckpoint,
                        &binding_path,
                        "no-flicker interval must span the overlay's initial and final checkpoints",
                    );
                }
            }
            RendererDetectorBinding::CheckpointOraclePair {
                detector_id,
                subject_checkpoint_id,
                independent_oracle_ref,
                comparator_policy_ref,
                ..
            } => {
                let expected_independent = comparison_subject.and_then(|checkpoint| {
                    checkpoint
                        .independent_standard_oracle_ref
                        .as_deref()
                        .or(Some(checkpoint.visual_oracle_ref.as_str()))
                });
                if !matches!(
                    detector_id,
                    RendererCheckpointDetectorId::SsimPolicy
                        | RendererCheckpointDetectorId::LInfPolicy
                        | RendererCheckpointDetectorId::ChangedPixelFractionPolicy
                ) || contract.scope != RendererDetectorScope::CheckpointOraclePair
                    || comparison_subject.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        != Some(subject_checkpoint_id.as_str())
                    || expected_independent != Some(independent_oracle_ref.as_str())
                    || comparator_policy_ref != &contract.oracle_ref
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCheckpoint,
                        &binding_path,
                        "pixel comparator must bind the exact snap-back/final subject, independent oracle, and canonical detector policy",
                    );
                }
                validator.require_repository_ref(
                    &format!("{binding_path}.independent_oracle_ref"),
                    independent_oracle_ref,
                );
                validator.require_repository_ref(
                    &format!("{binding_path}.comparator_policy_ref"),
                    comparator_policy_ref,
                );
            }
            RendererDetectorBinding::WholeTimeline { detector_id, .. } => {
                if !gesture_is_live_resize(scenario.gesture)
                    || *detector_id
                        != RendererCheckpointDetectorId::ExactlyOneStandardSnapBack
                    || contract.scope != RendererDetectorScope::WholeTimeline
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCheckpoint,
                        &binding_path,
                        "whole-timeline binding is reserved for exactly-one Standard snap-back in live gestures",
                    );
                }
            }
        }
    }
}

fn expected_requirement_ids(
    scenario: &RendererScenarioDefinition,
) -> Vec<RendererRequirementId> {
    let mut expected = Vec::new();
    if scenario.gesture == RendererGesture::OutputOverlapResize
        && scenario.fleet_point == RendererFleetPoint::P050
    {
        expected.push(RendererRequirementId::RqS6HeavyBurstInputLatency);
    }
    if scenario.gesture == RendererGesture::Reflow80To200 {
        expected.push(RendererRequirementId::RqS9ReflowLatency);
    }
    if scenario.gesture == RendererGesture::SameGridDrag {
        expected.push(RendererRequirementId::RqS10AtlasRebuildCount);
    }
    if gesture_is_live_resize(scenario.gesture) {
        expected.push(RendererRequirementId::RqS11SnapBackSsim);
    }
    expected.push(RendererRequirementId::RqS13SsimParityOracleCorpus);
    expected
}

fn rq_s10_pure_resize_predicate(
    scenario: &RendererScenarioDefinition,
    workload: &RendererWorkloadDefinition,
    production_overlay: Option<&ResolvedScenarioOverlay<'_>>,
    index: &CatalogIndex<'_>,
) -> bool {
    let window_resize_bundles = scenario
        .timeline
        .iter()
        .filter(|event| {
            event.actions.iter().any(|action| {
                matches!(action, RendererTimelineAction::SetWindowSize { .. })
            })
        })
        .count();
    let no_output_or_key = workload.output_stream.is_none()
        && workload.foreground_key_events.is_empty()
        && scenario.timeline.iter().all(|event| {
            event.actions.iter().all(|action| {
                !matches!(
                    action,
                    RendererTimelineAction::SetOutputRate { .. }
                        | RendererTimelineAction::ForegroundKey { .. }
                )
            })
        });
    let stable_font_and_scale = production_overlay
        .and_then(|overlay| {
            Some((
                overlay.anchors.first()?,
                overlay.checkpoints.first()?,
                overlay.anchors.last()?,
                overlay.checkpoints.last()?,
            ))
        })
        .and_then(|(initial, initial_checkpoint, final_manifest, final_checkpoint)| {
            Some((
                expand_manifest_state(
                    initial,
                    index,
                    scenario,
                    initial_checkpoint,
                    workload,
                )
                .ok()?,
                expand_manifest_state(
                    final_manifest,
                    index,
                    scenario,
                    final_checkpoint,
                    workload,
                )
                .ok()?,
            ))
        })
        .is_some_and(|(initial, final_state)| {
            initial.surfaces.len() == final_state.surfaces.len()
                && initial
                    .surfaces
                    .iter()
                    .zip(&final_state.surfaces)
                    .all(|(before, after)| {
                        before.font.font_id == after.font.font_id
                            && before.font.pinned_font_ref == after.font.pinned_font_ref
                            && before.font.base_size_milli_points
                                == after.font.base_size_milli_points
                            && before.font.scale_milli == after.font.scale_milli
                            && before.font.metric_derivation_revision
                                == after.font.metric_derivation_revision
                    })
        });
    scenario.gesture == RendererGesture::SameGridDrag
        && workload.resize_mutation_count == 100
        && window_resize_bundles == 100
        && workload.new_glyph_count == 0
        && no_output_or_key
        && stable_font_and_scale
}

fn snap_back_state_matches_last_draft(
    scenario: &RendererScenarioDefinition,
    index: &CatalogIndex<'_>,
) -> bool {
    let Some(workload) = index.workloads.get(scenario.workload_id.as_str()).copied() else {
        return false;
    };
    let Some(last_draft) = checkpoint_for_role(
        scenario,
        RendererCoverageOverlayId::ProductionDefault,
        RendererCheckpointRole::LastDraftProvenance,
    ) else {
        return false;
    };
    let Some(snap_back) = checkpoint_for_role(
        scenario,
        RendererCoverageOverlayId::ProductionDefault,
        RendererCheckpointRole::StandardSnapBackSubject,
    ) else {
        return false;
    };
    let (Some(last_draft_manifest), Some(snap_back_manifest)) = (
        index
            .phase_manifests
            .get(last_draft.phase_manifest_id.as_str())
            .copied(),
        index
            .phase_manifests
            .get(snap_back.phase_manifest_id.as_str())
            .copied(),
    ) else {
        return false;
    };
    let (Ok(mut last_draft_state), Ok(snap_back_state)) = (
        expand_manifest_state(
            last_draft_manifest,
            index,
            scenario,
            last_draft,
            workload,
        ),
        expand_manifest_state(
            snap_back_manifest,
            index,
            scenario,
            snap_back,
            workload,
        ),
    ) else {
        return false;
    };
    for surface in &mut last_draft_state.surfaces {
        surface.quality_mode = RendererQualityMode::Standard;
    }
    last_draft_state == snap_back_state
}

fn validate_requirement_crosswalk(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
    overlays: &BTreeMap<RendererCoverageOverlayId, ResolvedScenarioOverlay<'_>>,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.requirement_bindings");
    let expected_ids = expected_requirement_ids(scenario);
    let actual_ids = scenario
        .requirement_bindings
        .iter()
        .map(RendererRequirementBinding::requirement_id)
        .collect::<Vec<_>>();
    if actual_ids != expected_ids {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            &path,
            format!("expected exact requirement order {expected_ids:?}, found {actual_ids:?}"),
        );
    }
    let mut seen = BTreeSet::new();
    for (position, binding) in scenario.requirement_bindings.iter().enumerate() {
        let binding_path = format!("{path}[{position}]");
        if !seen.insert(binding.requirement_id()) {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                &binding_path,
                "duplicate requirement binding",
            );
        }
        if binding.requirement_id() == RendererRequirementId::RqS1ResizeFps {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                &binding_path,
                "RQ-S1 is represented only by the optional synthetic substrate and is never auto-bound to native matrix cells",
            );
        }
        if binding.overlay_id() != RendererCoverageOverlayId::ProductionDefault {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{binding_path}.overlay_id"),
                "exact requirement bindings are reserved for production_default",
            );
        }
        let expected_scope = match binding.requirement_id() {
            RendererRequirementId::RqS1ResizeFps => RendererRequirementScope::RelatedOnly,
            RendererRequirementId::RqS6HeavyBurstInputLatency => {
                RendererRequirementScope::RelatedAdversarialSuperset
            }
            RendererRequirementId::RqS9ReflowLatency => {
                if scenario.fleet_point == RendererFleetPoint::P001 {
                    RendererRequirementScope::ExactCandidateUnproven
                } else {
                    RendererRequirementScope::RelatedFleetStress
                }
            }
            RendererRequirementId::RqS10AtlasRebuildCount => {
                RendererRequirementScope::ExactScenarioPredicate
            }
            RendererRequirementId::RqS11SnapBackSsim => {
                RendererRequirementScope::CheckpointPredicate
            }
            RendererRequirementId::RqS13SsimParityOracleCorpus => {
                RendererRequirementScope::ComparatorMechanismOnly
            }
        };
        if binding.scope() != expected_scope {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{binding_path}.scope"),
                format!("requirement requires scope {expected_scope:?}"),
            );
        }
        let predicate_valid = match binding {
            RendererRequirementBinding::RqS1 { .. } => false,
            RendererRequirementBinding::RqS6 { .. } => workload.is_some_and(|workload| {
                scenario.gesture == RendererGesture::OutputOverlapResize
                    && scenario.fleet_point == RendererFleetPoint::P050
                    && workload
                        .output_stream
                        .as_ref()
                        .is_some_and(|stream| {
                            stream.aggregate_bytes_per_second
                                == OUTPUT_OVERLAP_BYTES_PER_SECOND
                        })
                    && workload.foreground_key_events.len() == 1
            }),
            RendererRequirementBinding::RqS9 { .. } => workload.is_some_and(|workload| {
                scenario.gesture == RendererGesture::Reflow80To200
                    && workload.scrollback_lines_per_pane == 1_000
            }),
            RendererRequirementBinding::RqS10 { .. } => workload.is_some_and(|workload| {
                rq_s10_pure_resize_predicate(
                    scenario,
                    workload,
                    overlays.get(&RendererCoverageOverlayId::ProductionDefault),
                    index,
                )
            }),
            RendererRequirementBinding::RqS11 {
                last_draft_checkpoint_id,
                standard_snap_back_subject_checkpoint_id,
                independent_standard_oracle_ref,
                ..
            } => {
                let last_draft = checkpoint_for_role(
                    scenario,
                    RendererCoverageOverlayId::ProductionDefault,
                    RendererCheckpointRole::LastDraftProvenance,
                );
                let snap_back = checkpoint_for_role(
                    scenario,
                    RendererCoverageOverlayId::ProductionDefault,
                    RendererCheckpointRole::StandardSnapBackSubject,
                );
                gesture_is_live_resize(scenario.gesture)
                    && last_draft.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        == Some(last_draft_checkpoint_id.as_str())
                    && snap_back.map(|checkpoint| checkpoint.checkpoint_id.as_str())
                        == Some(standard_snap_back_subject_checkpoint_id.as_str())
                    && snap_back
                        .and_then(|checkpoint| {
                            checkpoint.independent_standard_oracle_ref.as_deref()
                        })
                        == Some(independent_standard_oracle_ref.as_str())
                    && snap_back_state_matches_last_draft(scenario, index)
            }
            RendererRequirementBinding::RqS13 { .. } => true,
        };
        if !predicate_valid {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                &binding_path,
                format!(
                    "scenario does not satisfy exact `{}` binding predicate",
                    binding.requirement_id().as_str()
                ),
            );
        }
    }
}

fn content_identity_framing(
    reference: &RendererContentCorpusReference,
) -> RendererContentFraming {
    match &reference.deterministic_identity {
        RendererContentDeterministicIdentity::Generator { output_framing, .. } => *output_framing,
        RendererContentDeterministicIdentity::Payload { framing, .. } => *framing,
    }
}

fn content_input_available(reference: &RendererContentCorpusReference) -> bool {
    matches!(reference.availability, RendererContentInputAvailability::Available)
}

fn validate_content_distribution_profiles<'a>(
    profiles: &'a [RendererContentDistributionProfile],
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererContentDistributionProfile> {
    if profiles.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.content_distribution_profiles",
            "at least one content distribution profile is required",
        );
    }
    let mut index = BTreeMap::new();
    for (position, profile) in profiles.iter().enumerate() {
        let path = format!("$.content_distribution_profiles[{position}]");
        validator.require_identifier(
            &format!("{path}.content_distribution_profile_id"),
            &profile.content_distribution_profile_id,
        );
        if profile.profile_revision == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.profile_revision"),
                "content distribution revision must be positive",
            );
        }
        if index
            .insert(profile.content_distribution_profile_id.as_str(), profile)
            .is_some()
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.content_distribution_profile_id"),
                format!("duplicate content distribution `{}`", profile.content_distribution_profile_id),
            );
        }
        let pane_count = profile.fleet_point.pane_count();
        let mut covered = BTreeSet::new();
        for (assignment_position, assignment) in profile.assignments.iter().enumerate() {
            let assignment_path = format!("{path}.assignments[{assignment_position}]");
            let ordinals = expand_pane_selector(
                &format!("{assignment_path}.selector"),
                &assignment.selector,
                pane_count,
                validator,
            );
            for ordinal in ordinals {
                if !covered.insert(ordinal) {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{assignment_path}.selector"),
                        format!("pane ordinal {ordinal} is assigned more than once"),
                    );
                }
            }
            if assignment.materialization_steps.is_empty() {
                validator.error(
                    RendererScenarioValidationCode::EmptyRequiredField,
                    format!("{assignment_path}.materialization_steps"),
                    "every selected pane requires ordered materialization steps",
                );
            }
            for (step_position, step) in assignment.materialization_steps.iter().enumerate() {
                let step_path = format!("{assignment_path}.materialization_steps[{step_position}]");
                if usize::from(step.step_ordinal) != step_position {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{step_path}.step_ordinal"),
                        format!("expected contiguous step ordinal {step_position}"),
                    );
                }
                validator.require_identifier(&format!("{step_path}.content_corpus_id"), &step.content_corpus_id);
                let corpus = content.get(step.content_corpus_id.as_str()).copied();
                if corpus.is_none() {
                    validator.error(
                        RendererScenarioValidationCode::DanglingReference,
                        format!("{step_path}.content_corpus_id"),
                        format!("undefined content corpus `{}`", step.content_corpus_id),
                    );
                }
                match &step.application_boundary {
                    RendererContentApplicationBoundary::BeforeGesture
                    | RendererContentApplicationBoundary::AtEvent { .. } => {}
                    RendererContentApplicationBoundary::AfterCheckpoint { checkpoint_id } => {
                        validator.require_identifier(
                            &format!("{step_path}.application_boundary.checkpoint_id"),
                            checkpoint_id,
                        );
                    }
                }
                let mut held = BTreeSet::new();
                for (checkpoint_position, checkpoint_id) in
                    step.hold_through_checkpoint_ids.iter().enumerate()
                {
                    let checkpoint_path = format!(
                        "{step_path}.hold_through_checkpoint_ids[{checkpoint_position}]"
                    );
                    validator.require_identifier(&checkpoint_path, checkpoint_id);
                    if !held.insert(checkpoint_id.as_str()) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            checkpoint_path,
                            format!("duplicate hold-through checkpoint `{checkpoint_id}`"),
                        );
                    }
                }
                if matches!(step.operation, RendererContentCompositionOperation::EnterAlternateBuffer)
                    && step.hold_through_checkpoint_ids.is_empty()
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{step_path}.hold_through_checkpoint_ids"),
                        "alternate-screen entry must name at least one checkpoint through which it remains active",
                    );
                }
                if let Some(corpus) = corpus {
                    let is_typed_state = matches!(
                        content_identity_framing(corpus),
                        RendererContentFraming::TypedStateOverlay
                    );
                    if matches!(step.operation, RendererContentCompositionOperation::ApplyTypedStateOverlay)
                        != is_typed_state
                    {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            format!("{step_path}.operation"),
                            "typed-state framing and apply_typed_state_overlay operation must agree",
                        );
                    }
                    if matches!(
                        step.operation,
                        RendererContentCompositionOperation::ApplyTypedStateOverlay
                    ) && !matches!(
                        step.application_boundary,
                        RendererContentApplicationBoundary::BeforeGesture
                    ) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            format!("{step_path}.application_boundary"),
                            "v1 typed-state fixtures must apply before the gesture because the catalog carries no later typed-state transform payload",
                        );
                    }
                    let has_alt_semantics = corpus
                        .semantic_kinds
                        .contains(&RendererContentSemanticKind::AlternateScreenControl);
                    if matches!(
                        step.operation,
                        RendererContentCompositionOperation::EnterAlternateBuffer
                            | RendererContentCompositionOperation::ExitAlternateBuffer
                    ) && !has_alt_semantics
                    {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            format!("{step_path}.operation"),
                            "alternate-buffer operations require alternate-screen control semantics",
                        );
                    }
                    if !content_input_available(corpus)
                        && matches!(step.availability, RendererContentInputAvailability::Available)
                    {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            format!("{step_path}.availability"),
                            "a materialization step cannot make an unavailable corpus available",
                        );
                    }
                }
                if let RendererContentInputAvailability::Unavailable {
                    reason,
                    tracking_refs,
                } = &step.availability
                {
                    validate_tracked_limitation(
                        &format!("{step_path}.availability"),
                        reason,
                        tracking_refs,
                        RendererScenarioValidationCode::InvalidState,
                        validator,
                    );
                }
            }
        }
        if covered.len() != usize::from(pane_count) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.assignments"),
                format!("assignments cover {} of {pane_count} panes", covered.len()),
            );
        }
    }
    index
}

fn validate_pane_output_state(path: &str, output: &RendererPaneOutputState, validator: &mut Validator) {
    if let Some(stream_id) = &output.stream_id {
        validator.require_identifier(&format!("{path}.stream_id"), stream_id);
        if output.bytes_per_second == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                path,
                "a named output stream must have a positive rate",
            );
        }
    } else if output.bytes_per_second != 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "nonzero pane output requires a stream identity",
        );
    }
    if output.bytes_per_second > MAX_RENDERER_OUTPUT_BYTES_PER_SECOND {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            format!("{path}.bytes_per_second"),
            "pane output rate exceeds contract bound",
        );
    }
}

fn validate_phase_manifests<'a>(
    manifests: &'a [RendererPhaseManifest],
    layouts: &BTreeMap<&str, &RendererLayoutProfile>,
    templates: &BTreeMap<&str, &RendererSurfaceStateTemplate>,
    distributions: &BTreeMap<&str, &RendererContentDistributionProfile>,
    _configs: &BTreeMap<&str, &RendererConfigProfile>,
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererPhaseManifest> {
    if manifests.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.phase_manifests",
            "at least one phase manifest is required",
        );
    }
    let mut index = BTreeMap::new();
    for (position, manifest) in manifests.iter().enumerate() {
        let path = format!("$.phase_manifests[{position}]");
        validator.require_identifier(&format!("{path}.phase_manifest_id"), &manifest.phase_manifest_id);
        if index.insert(manifest.phase_manifest_id.as_str(), manifest).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.phase_manifest_id"),
                format!("duplicate phase manifest `{}`", manifest.phase_manifest_id),
            );
        }
        for (field, identifier) in [
            ("layout_profile_id", &manifest.layout_profile_id),
            (
                "default_surface_state_template_id",
                &manifest.default_surface_state_template_id,
            ),
            (
                "content_distribution_profile_id",
                &manifest.content_distribution_profile_id,
            ),
        ] {
            validator.require_identifier(&format!("{path}.{field}"), identifier);
        }
        let layout = layouts.get(manifest.layout_profile_id.as_str()).copied();
        if layout.is_none() {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.layout_profile_id"),
                format!("undefined layout `{}`", manifest.layout_profile_id),
            );
        }
        if !templates.contains_key(manifest.default_surface_state_template_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.default_surface_state_template_id"),
                format!("undefined surface template `{}`", manifest.default_surface_state_template_id),
            );
        }
        let distribution = distributions
            .get(manifest.content_distribution_profile_id.as_str())
            .copied();
        if distribution.is_none() {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.content_distribution_profile_id"),
                format!("undefined content distribution `{}`", manifest.content_distribution_profile_id),
            );
        }
        if let (Some(layout), Some(distribution)) = (layout, distribution) {
            if layout.fleet_point != distribution.fleet_point {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &path,
                    "layout and content distribution fleet points differ",
                );
            }
            let expanded = expand_layout(layout);
            if manifest.focused_window_ordinal >= layout.window_count
                || manifest.focused_pane_ordinal >= layout.pane_count
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &path,
                    "focused window/pane ordinal is out of bounds",
                );
            } else if expanded
                .pane_window_ordinals
                .get(usize::from(manifest.focused_pane_ordinal))
                .copied()
                != Some(manifest.focused_window_ordinal)
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &path,
                    "focused pane does not belong to focused window",
                );
            }
            if manifest.window_states.len() != usize::from(layout.window_count) {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.window_states"),
                    "one ordered window state is required per expanded window",
                );
            }
            for (window_position, window_state) in manifest.window_states.iter().enumerate() {
                let window_path = format!("{path}.window_states[{window_position}]");
                if usize::from(window_state.window_ordinal) != window_position {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{window_path}.window_ordinal"),
                        format!("expected contiguous window ordinal {window_position}"),
                    );
                }
                let tab_count = expanded
                    .tabs_by_window
                    .get(window_position)
                    .map_or(0, Vec::len);
                if usize::from(window_state.active_tab_ordinal) >= tab_count {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{window_path}.active_tab_ordinal"),
                        format!("active local tab ordinal is outside window tab count {tab_count}"),
                    );
                }
                validate_pixel_rect(&format!("{window_path}.window_rect"), window_state.window_rect, validator);
                if window_state.window_rect.x != 0 || window_state.window_rect.y != 0 {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{window_path}.window_rect"),
                        "v1 window regions use window_drawable coordinates with x=y=0",
                    );
                }
            }
            if let (Some(pane_tab), Some(pane_window)) = (
                expanded
                    .pane_tab_ordinals
                    .get(usize::from(manifest.focused_pane_ordinal))
                    .copied(),
                expanded
                    .pane_window_ordinals
                    .get(usize::from(manifest.focused_pane_ordinal))
                    .copied(),
            ) {
                if let Some(window_state) = manifest.window_states.get(usize::from(pane_window)) {
                    let active_global_tab = expanded
                        .tabs_by_window
                        .get(usize::from(pane_window))
                        .and_then(|tabs| tabs
                        .get(usize::from(window_state.active_tab_ordinal))
                        .copied());
                    if active_global_tab != Some(pane_tab) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            &path,
                            "focused pane must belong to the focused window's active tab",
                        );
                    }
                }
            }
            let mut overridden = BTreeSet::new();
            for (override_position, pane_override) in manifest.pane_overrides.iter().enumerate() {
                let override_path = format!("{path}.pane_overrides[{override_position}]");
                if !templates.contains_key(pane_override.surface_state_template_id.as_str()) {
                    validator.error(
                        RendererScenarioValidationCode::DanglingReference,
                        format!("{override_path}.surface_state_template_id"),
                        format!("undefined surface template `{}`", pane_override.surface_state_template_id),
                    );
                }
                for ordinal in expand_pane_selector(
                    &format!("{override_path}.selector"),
                    &pane_override.selector,
                    layout.pane_count,
                    validator,
                ) {
                    if !overridden.insert(ordinal) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            &override_path,
                            format!("pane ordinal {ordinal} is overridden more than once"),
                        );
                    }
                }
                validate_pane_output_state(&format!("{override_path}.output"), &pane_override.output, validator);
            }
        }
        validate_pane_output_state(&format!("{path}.default_output"), &manifest.default_output, validator);
    }
    index
}

fn expected_overlay_profile_id(overlay_id: RendererCoverageOverlayId) -> String {
    format!("renderer.overlay.{}", overlay_id.as_str())
}

fn validate_capability_availability_shape(
    path: &str,
    availability: &RendererCapabilityAvailability,
    validator: &mut Validator,
) {
    match availability {
        RendererCapabilityAvailability::DeclaredAvailable => {}
        RendererCapabilityAvailability::Partial {
            reason,
            tracking_ref,
        }
        | RendererCapabilityAvailability::UnknownNotProbed {
            reason,
            tracking_ref,
        }
        | RendererCapabilityAvailability::Unsupported {
            reason,
            tracking_ref,
        } => {
            validate_tracked_limitation(
                path,
                reason,
                std::slice::from_ref(tracking_ref),
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                validator,
            );
        }
        RendererCapabilityAvailability::TargetDependent {
            target_profile_ref,
            reason,
            tracking_ref,
        } => {
            validator.require_repository_ref(
                &format!("{path}.target_profile_ref"),
                target_profile_ref,
            );
            validate_tracked_limitation(
                path,
                reason,
                std::slice::from_ref(tracking_ref),
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                validator,
            );
        }
    }
}

fn validate_coverage_overlay_profiles<'a>(
    profiles: &'a [RendererCoverageOverlayProfile],
    configs: &BTreeMap<&str, &RendererConfigProfile>,
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererCoverageOverlayProfile> {
    let expected_count = RendererCoverageOverlayId::ALL.len();
    if profiles.len() != expected_count {
        validator.error(
            RendererScenarioValidationCode::MissingRequiredCoverage,
            "$.coverage_overlay_profiles",
            format!("expected exactly {expected_count} overlay profiles, found {}", profiles.len()),
        );
    }
    let mut index = BTreeMap::new();
    for (position, profile) in profiles.iter().enumerate() {
        let path = format!("$.coverage_overlay_profiles[{position}]");
        validator.require_identifier(&format!("{path}.overlay_profile_id"), &profile.overlay_profile_id);
        validator.require_identifier(
            &format!("{path}.renderer_config_profile_id"),
            &profile.renderer_config_profile_id,
        );
        if profile.profile_revision == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.profile_revision"),
                "overlay profile revision must be positive",
            );
        }
        if index.insert(profile.overlay_profile_id.as_str(), profile).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.overlay_profile_id"),
                format!("duplicate overlay profile `{}`", profile.overlay_profile_id),
            );
        }
        if RendererCoverageOverlayId::ALL.get(position) != Some(&profile.overlay_id) {
            validator.error(
                RendererScenarioValidationCode::MissingRequiredCoverage,
                format!("{path}.overlay_id"),
                "overlay profiles must appear in canonical eight-overlay order",
            );
        }
        let expected_id = expected_overlay_profile_id(profile.overlay_id);
        if profile.overlay_profile_id != expected_id {
            validator.error(
                RendererScenarioValidationCode::InvalidIdentifier,
                format!("{path}.overlay_profile_id"),
                format!("expected canonical overlay profile id `{expected_id}`"),
            );
        }
        let expected_config = if profile.overlay_id == RendererCoverageOverlayId::LigatureEnabled {
            LIGATURE_ENABLED_CONFIG_ID
        } else {
            PRODUCTION_DEFAULT_CONFIG_ID
        };
        if profile.renderer_config_profile_id != expected_config {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.renderer_config_profile_id"),
                format!("overlay requires renderer configuration `{expected_config}`"),
            );
        }
        if !configs.contains_key(profile.renderer_config_profile_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.renderer_config_profile_id"),
                format!("undefined renderer configuration `{}`", profile.renderer_config_profile_id),
            );
        }
        let expected_class = if profile.overlay_id == RendererCoverageOverlayId::ProductionDefault {
            RendererOverlayQualificationClass::ProductionDefaultSloCandidate
        } else {
            RendererOverlayQualificationClass::VisualCoverageRelatedOnly
        };
        if profile.qualification_class != expected_class {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{path}.qualification_class"),
                format!("overlay requires qualification class {expected_class:?}"),
            );
        }
        let mut capabilities = BTreeSet::new();
        for (delta_position, delta) in profile.capability_deltas.iter().enumerate() {
            let delta_path = format!("{path}.capability_deltas[{delta_position}]");
            if !capabilities.insert(delta.capability) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCapabilityMatrix,
                    format!("{delta_path}.capability"),
                    format!("duplicate overlay capability delta `{}`", delta.capability.as_str()),
                );
            }
            validate_capability_availability_shape(
                &format!("{delta_path}.availability"),
                &delta.availability,
                validator,
            );
        }
        let expected_delta = match profile.overlay_id {
            RendererCoverageOverlayId::ImeComposing => {
                Some(RendererCapability::ImeComposition)
            }
            RendererCoverageOverlayId::ImageHyperlink => {
                Some(RendererCapability::ImageProtocol)
            }
            RendererCoverageOverlayId::LigatureEnabled => {
                Some(RendererCapability::EnabledLigatureShaping)
            }
            RendererCoverageOverlayId::A11yGeometry => {
                Some(RendererCapability::AccessibilityGeometry)
            }
            RendererCoverageOverlayId::ProductionDefault
            | RendererCoverageOverlayId::UnicodeMaximal
            | RendererCoverageOverlayId::AlternateScreen
            | RendererCoverageOverlayId::Selection => None,
        };
        let exact_delta = match (expected_delta, profile.capability_deltas.as_slice()) {
            (None, []) => true,
            (Some(expected), [binding]) => {
                binding.capability == expected
                    && binding.requirement == RendererCapabilityRequirement::Required
            }
            _ => false,
        };
        if !exact_delta {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                format!("{path}.capability_deltas"),
                format!(
                    "overlay `{}` requires the exact capability delta {:?}",
                    profile.overlay_id.as_str(),
                    expected_delta
                ),
            );
        }
        let mut targets = BTreeSet::new();
        for (exclusion_position, exclusion) in profile.qualification_exclusions.iter().enumerate() {
            let exclusion_path = format!("{path}.qualification_exclusions[{exclusion_position}]");
            if !targets.insert(exclusion.target) {
                validator.error(
                    RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                    format!("{exclusion_path}.target"),
                    "duplicate overlay qualification exclusion",
                );
            }
            validate_tracked_limitation(
                &exclusion_path,
                &exclusion.detail,
                std::slice::from_ref(&exclusion.tracking_ref),
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                validator,
            );
            let valid_reason = matches!(
                (profile.overlay_id, exclusion.reason, exclusion.target),
                (
                    RendererCoverageOverlayId::ImeComposing,
                    RendererOverlayExclusionReason::ImeMayConsumeOrTransformForegroundKey,
                    RendererOverlayQualificationTarget::Measurement {
                        measurement_role: RendererMeasurementRole::KeypressToFirstCorrectPresent
                    } | RendererOverlayQualificationTarget::Requirement {
                        requirement_id: RendererRequirementId::RqS6HeavyBurstInputLatency
                    }
                ) | (
                    RendererCoverageOverlayId::AlternateScreen,
                    RendererOverlayExclusionReason::AlternateScreenConflictsWithPrimaryScrollback,
                    RendererOverlayQualificationTarget::Requirement {
                        requirement_id: RendererRequirementId::RqS9ReflowLatency
                    }
                )
            );
            if !valid_reason {
                validator.error(
                    RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                    &exclusion_path,
                    "qualification exclusion is outside the closed IME-key/alternate-primary-scrollback table",
                );
            }
        }
        let expected_exclusions = match profile.overlay_id {
            RendererCoverageOverlayId::ImeComposing => [
                Some(RendererOverlayQualificationTarget::Measurement {
                    measurement_role:
                        RendererMeasurementRole::KeypressToFirstCorrectPresent,
                }),
                Some(RendererOverlayQualificationTarget::Requirement {
                    requirement_id: RendererRequirementId::RqS6HeavyBurstInputLatency,
                }),
            ],
            RendererCoverageOverlayId::AlternateScreen => [
                Some(RendererOverlayQualificationTarget::Requirement {
                    requirement_id: RendererRequirementId::RqS9ReflowLatency,
                }),
                None,
            ],
            RendererCoverageOverlayId::ProductionDefault
            | RendererCoverageOverlayId::UnicodeMaximal
            | RendererCoverageOverlayId::ImageHyperlink
            | RendererCoverageOverlayId::LigatureEnabled
            | RendererCoverageOverlayId::Selection
            | RendererCoverageOverlayId::A11yGeometry => [None, None],
        };
        let expected_targets = expected_exclusions
            .into_iter()
            .flatten()
            .collect::<BTreeSet<_>>();
        if targets != expected_targets
            || profile.qualification_exclusions.len() != expected_targets.len()
        {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{path}.qualification_exclusions"),
                format!(
                    "overlay `{}` must carry exactly the closed exclusion set {:?}",
                    profile.overlay_id.as_str(),
                    expected_targets
                ),
            );
        }
    }
    index
}

const fn expected_detector_scope(detector: RendererCheckpointDetectorId) -> RendererDetectorScope {
    match detector {
        RendererCheckpointDetectorId::NoFlicker => RendererDetectorScope::Interval,
        RendererCheckpointDetectorId::SsimPolicy
        | RendererCheckpointDetectorId::LInfPolicy
        | RendererCheckpointDetectorId::ChangedPixelFractionPolicy => {
            RendererDetectorScope::CheckpointOraclePair
        }
        RendererCheckpointDetectorId::ExactlyOneStandardSnapBack => {
            RendererDetectorScope::WholeTimeline
        }
        RendererCheckpointDetectorId::NoMissingGlyphs
        | RendererCheckpointDetectorId::CoherentCellWidths
        | RendererCheckpointDetectorId::ExactRowWidth
        | RendererCheckpointDetectorId::CoherentRendererGeneration
        | RendererCheckpointDetectorId::NoMixedGenerationTearBand
        | RendererCheckpointDetectorId::ExactTerminalState
        | RendererCheckpointDetectorId::CursorGeometry
        | RendererCheckpointDetectorId::SelectionGeometry
        | RendererCheckpointDetectorId::ImeGeometry
        | RendererCheckpointDetectorId::HyperlinkGeometry
        | RendererCheckpointDetectorId::ImageGeometry
        | RendererCheckpointDetectorId::AlternateScreenState
        | RendererCheckpointDetectorId::AccessibilityGeometry
        | RendererCheckpointDetectorId::NoStaleOrDuplicateFrame
        | RendererCheckpointDetectorId::NonblankAfterBaseline => {
            RendererDetectorScope::AllObservedFrames
        }
    }
}

fn validate_detector_contracts<'a>(
    contracts: &'a [RendererDetectorContract],
    validator: &mut Validator,
) -> BTreeMap<RendererCheckpointDetectorId, &'a RendererDetectorContract> {
    if contracts.len() != RendererCheckpointDetectorId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            "$.detector_contracts",
            format!("expected exactly 20 detector contracts, found {}", contracts.len()),
        );
    }
    let mut index = BTreeMap::new();
    for (position, contract) in contracts.iter().enumerate() {
        let path = format!("$.detector_contracts[{position}]");
        if RendererCheckpointDetectorId::ALL.get(position) != Some(&contract.detector_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{path}.detector_id"),
                "detector contracts must appear in canonical order",
            );
        }
        if index.insert(contract.detector_id, contract).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.detector_id"),
                format!("duplicate detector `{}`", contract.detector_id.as_str()),
            );
        }
        validator.require_repository_ref(&format!("{path}.oracle_ref"), &contract.oracle_ref);
        let expected_scope = expected_detector_scope(contract.detector_id);
        if contract.scope != expected_scope {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{path}.scope"),
                format!("detector requires scope {expected_scope:?}"),
            );
        }
        match (&contract.detector_id, &contract.mechanism_status) {
            (
                RendererCheckpointDetectorId::ChangedPixelFractionPolicy,
                RendererDetectorMechanismStatus::KnownNonIndependent { reason, tracking_ref },
            ) => {
                if tracking_ref != CHANGED_PIXEL_FRACTION_TRACKING_REF {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCheckpoint,
                        format!("{path}.mechanism_status.tracking_ref"),
                        format!("expected `{CHANGED_PIXEL_FRACTION_TRACKING_REF}`"),
                    );
                }
                validate_tracked_limitation(
                    &format!("{path}.mechanism_status"),
                    reason,
                    std::slice::from_ref(tracking_ref),
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    validator,
                );
            }
            (
                RendererCheckpointDetectorId::ChangedPixelFractionPolicy,
                RendererDetectorMechanismStatus::ContractDefined,
            ) => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{path}.mechanism_status"),
                "changed-pixel detector must retain known-non-independent status",
            ),
            (_, RendererDetectorMechanismStatus::KnownNonIndependent { .. }) => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{path}.mechanism_status"),
                "only changed-pixel detector is known non-independent in v1",
            ),
            (_, RendererDetectorMechanismStatus::ContractDefined) => {}
        }
    }
    index
}

fn validate_presentation_availability(
    path: &str,
    availability: &RendererPresentationTargetAvailability,
    validator: &mut Validator,
) {
    match availability {
        RendererPresentationTargetAvailability::UnknownNotProbed {
            reason,
            tracking_ref,
        }
        | RendererPresentationTargetAvailability::Unsupported {
            reason,
            tracking_ref,
        } => validate_tracked_limitation(
            path,
            reason,
            std::slice::from_ref(tracking_ref),
            RendererScenarioValidationCode::InvalidState,
            validator,
        ),
        RendererPresentationTargetAvailability::TargetDependent {
            target_profile_ref,
            reason,
            tracking_ref,
        } => {
            validator.require_repository_ref(
                &format!("{path}.target_profile_ref"),
                target_profile_ref,
            );
            validate_tracked_limitation(
                path,
                reason,
                std::slice::from_ref(tracking_ref),
                RendererScenarioValidationCode::InvalidState,
                validator,
            );
        }
    }
}

fn validate_presentation_target_profiles<'a>(
    profiles: &'a [RendererPresentationTargetProfile],
    validator: &mut Validator,
) -> BTreeMap<RendererPresentationTargetProfileId, &'a RendererPresentationTargetProfile> {
    if profiles.len() != RendererPresentationTargetProfileId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            "$.presentation_target_profiles",
            format!("expected exactly three cadence profiles, found {}", profiles.len()),
        );
    }
    let mut index = BTreeMap::new();
    for (position, profile) in profiles.iter().enumerate() {
        let path = format!("$.presentation_target_profiles[{position}]");
        if RendererPresentationTargetProfileId::ALL.get(position) != Some(&profile.profile_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.profile_id"),
                "presentation profiles must appear in canonical order",
            );
        }
        if index.insert(profile.profile_id, profile).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.profile_id"),
                "duplicate presentation target profile",
            );
        }
        let cadence_valid = match profile.profile_id {
            RendererPresentationTargetProfileId::Fixed60Hz => {
                profile.minimum_millihz == 60_000 && profile.maximum_millihz == 60_000
            }
            RendererPresentationTargetProfileId::Fixed120Hz => {
                profile.minimum_millihz == 120_000 && profile.maximum_millihz == 120_000
            }
            RendererPresentationTargetProfileId::VariableRefreshRate => {
                profile.minimum_millihz > 0
                    && profile.minimum_millihz < profile.maximum_millihz
            }
        };
        if !cadence_valid {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &path,
                "presentation cadence does not match its closed profile identity",
            );
        }
        validate_presentation_availability(&format!("{path}.availability"), &profile.availability, validator);
    }
    index
}

fn validate_preconditioning_profiles<'a>(
    profiles: &'a [RendererPreconditioningProfile],
    validator: &mut Validator,
) -> BTreeMap<RendererPreconditioningProfileId, &'a RendererPreconditioningProfile> {
    if profiles.len() != RendererPreconditioningProfileId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            "$.preconditioning_profiles",
            format!("expected exactly cold/warm/aged profiles, found {}", profiles.len()),
        );
    }
    let mut index = BTreeMap::new();
    for (position, profile) in profiles.iter().enumerate() {
        let path = format!("$.preconditioning_profiles[{position}]");
        if RendererPreconditioningProfileId::ALL.get(position) != Some(&profile.profile_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.profile_id"),
                "preconditioning profiles must appear in canonical order",
            );
        }
        if index.insert(profile.profile_id, profile).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.profile_id"),
                "duplicate preconditioning profile",
            );
        }
        validator.require_repository_ref(
            &format!("{path}.prewarm_manifest_ref"),
            &profile.prewarm_manifest_ref,
        );
        let valid = match profile.profile_id {
            RendererPreconditioningProfileId::Cold => {
                profile.session_age_us == 0
                    && profile.scrollback_age_us == 0
                    && profile.glyph_cache == RendererCachePrecondition::Cold
                    && profile.atlas == RendererCachePrecondition::Cold
            }
            RendererPreconditioningProfileId::Warm => {
                profile.session_age_us > 0
                    && profile.glyph_cache == RendererCachePrecondition::Warm
                    && profile.atlas == RendererCachePrecondition::Warm
            }
            RendererPreconditioningProfileId::Aged => {
                profile.session_age_us > 0
                    && profile.scrollback_age_us > 0
                    && profile.scrollback_lines_per_pane > 0
                    && profile.glyph_cache == RendererCachePrecondition::Aged
                    && profile.atlas == RendererCachePrecondition::Aged
            }
        };
        if !valid {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &path,
                "preconditioning state does not match cold/warm/aged identity",
            );
        }
    }
    index
}

fn validate_driver_canaries<'a>(
    canaries: &'a [RendererDriverCanaryDefinition],
    validator: &mut Validator,
) -> BTreeMap<RendererDriverCanaryId, &'a RendererDriverCanaryDefinition> {
    if canaries.len() != RendererDriverCanaryId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            "$.driver_canaries",
            format!("expected exactly five driver canaries, found {}", canaries.len()),
        );
    }
    let mut index = BTreeMap::new();
    for (position, canary) in canaries.iter().enumerate() {
        let path = format!("$.driver_canaries[{position}]");
        if RendererDriverCanaryId::ALL.get(position) != Some(&canary.canary_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.canary_id"),
                "driver canaries must appear in canonical order",
            );
        }
        if index.insert(canary.canary_id, canary).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.canary_id"),
                "duplicate driver canary",
            );
        }
        validator.require_repository_ref(
            &format!("{path}.expected_observed_event_ref"),
            &canary.expected_observed_event_ref,
        );
        validator.require_repository_ref(
            &format!("{path}.expected_state_ref"),
            &canary.expected_state_ref,
        );
        if canary.timeout_us == 0
            || canary.minimum_window_count == 0
            || canary.minimum_tab_count == 0
            || canary.minimum_pane_count == 0
        {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &path,
                "canary timeout and applicability minima must be positive",
            );
        }
        let action_matches = matches!(
            (&canary.canary_id, &canary.action),
            (RendererDriverCanaryId::FocusWindow, RendererDriverCanaryAction::FocusWindow { .. })
                | (RendererDriverCanaryId::ActivateTab, RendererDriverCanaryAction::ActivateTab { .. })
                | (RendererDriverCanaryId::FocusPane, RendererDriverCanaryAction::FocusPane { .. })
                | (RendererDriverCanaryId::SplitGeometry, RendererDriverCanaryAction::SetSplitGeometry { .. })
                | (RendererDriverCanaryId::TopologyManifest, RendererDriverCanaryAction::SetTopologyManifest { .. })
        );
        if !action_matches {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.action"),
                "canary action does not match its closed identity",
            );
        }
        match &canary.action {
            RendererDriverCanaryAction::SetSplitGeometry { split_geometry_ref } => validator
                .require_repository_ref(&format!("{path}.action.split_geometry_ref"), split_geometry_ref),
            RendererDriverCanaryAction::SetTopologyManifest { topology_ref } => validator
                .require_repository_ref(&format!("{path}.action.topology_ref"), topology_ref),
            RendererDriverCanaryAction::FocusWindow { target_window_ordinal } => {
                if *target_window_ordinal == 0 || *target_window_ordinal >= canary.minimum_window_count {
                    validator.error(RendererScenarioValidationCode::InvalidState, format!("{path}.action"), "focus-window canary must switch to a nonzero in-bounds window");
                }
            }
            RendererDriverCanaryAction::ActivateTab {
                target_window_ordinal,
                target_tab_ordinal,
            } => {
                if *target_window_ordinal >= canary.minimum_window_count
                    || *target_tab_ordinal == 0
                    || *target_tab_ordinal >= canary.minimum_tab_count
                {
                    validator.error(RendererScenarioValidationCode::InvalidState, format!("{path}.action"), "activate-tab canary must switch to a nonzero in-bounds tab");
                }
            }
            RendererDriverCanaryAction::FocusPane { target_pane_ordinal } => {
                if *target_pane_ordinal == 0 || *target_pane_ordinal >= canary.minimum_pane_count {
                    validator.error(RendererScenarioValidationCode::InvalidState, format!("{path}.action"), "focus-pane canary must switch to a nonzero in-bounds pane");
                }
            }
        }
        let mut capabilities = BTreeSet::new();
        for (capability_position, capability) in canary.prerequisite_capabilities.iter().enumerate() {
            if !capabilities.insert(*capability) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCapabilityMatrix,
                    format!("{path}.prerequisite_capabilities[{capability_position}]"),
                    format!("duplicate canary capability `{}`", capability.as_str()),
                );
            }
        }
    }
    index
}

fn validate_rq_s1_synthetic_substrate(
    substrate: Option<&RendererRqS1SyntheticSubstrate>,
    validator: &mut Validator,
) {
    let Some(substrate) = substrate else {
        return;
    };
    validator.require_repository_ref("$.rq_s1_synthetic_substrate.benchmark_ref", &substrate.benchmark_ref);
    if substrate.frame_count != 300
        || substrate.low_columns != 80
        || substrate.high_columns != 200
        || substrate.dirty_rows_per_frame != 1
        || substrate.gesture_duration_us != 5_000_000
    {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            "$.rq_s1_synthetic_substrate",
            "optional RQ-S1 substrate must be exactly 300 frames, 80<->200 columns, one dirty row, and five seconds",
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum ExpectedContentSelector {
    Whole,
    Segment {
        manifest_ref: &'static str,
        row_id: &'static str,
        start: u64,
        end: u64,
    },
}

#[derive(Debug, Clone, Copy)]
enum ExpectedContentIdentity {
    Payload {
        payload_ref: &'static str,
        selector: ExpectedContentSelector,
        encoding: RendererContentEncoding,
        decoder: RendererContentDecoder,
        framing: RendererContentFraming,
        encoded_sha256: &'static str,
        decoded_sha256: &'static str,
    },
    UnavailableGenerator {
        generator_id: &'static str,
        input_manifest_ref: &'static str,
        seed: u64,
        encoding: RendererContentEncoding,
        framing: RendererContentFraming,
        tracking_refs: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Copy)]
struct ExpectedCanonicalContent {
    content_corpus_id: &'static str,
    repository_ref: &'static str,
    semantic_kinds: &'static [RendererContentSemanticKind],
    identity: ExpectedContentIdentity,
}

const CONTENT_SEM_ASCII: [RendererContentSemanticKind; 1] =
    [RendererContentSemanticKind::AsciiText];
const CONTENT_SEM_UNICODE: [RendererContentSemanticKind; 4] = [
    RendererContentSemanticKind::AsciiText,
    RendererContentSemanticKind::CjkText,
    RendererContentSemanticKind::CombiningMarkText,
    RendererContentSemanticKind::EmojiText,
];
const CONTENT_SEM_RTL: [RendererContentSemanticKind; 1] =
    [RendererContentSemanticKind::RtlText];
const CONTENT_SEM_HYPERLINK: [RendererContentSemanticKind; 1] =
    [RendererContentSemanticKind::HyperlinkProtocol];
const CONTENT_SEM_ALT: [RendererContentSemanticKind; 1] =
    [RendererContentSemanticKind::AlternateScreenControl];
const CONTENT_SEM_IMAGE: [RendererContentSemanticKind; 1] =
    [RendererContentSemanticKind::ImageProtocol];
const CONTENT_SEM_LIGATURE: [RendererContentSemanticKind; 1] =
    [RendererContentSemanticKind::LigatureSequence];
const CONTENT_SEM_STATE_ONLY: [RendererContentSemanticKind; 0] = [];
const CONTENT_TRACK_LIGATURE: [&str; 1] =
    ["ft-interactive-systems-performance-4tenz.3.6.2"];
const CONTENT_TRACK_IME: [&str; 3] = [
    "ft-interactive-systems-performance-4tenz.3.6.2",
    "ft-interactive-systems-performance-4tenz.3.5",
    "ft-interactive-swarm-product-convergence-7xqz4.9.5",
];
const CONTENT_TRACK_A11Y: [&str; 2] = [
    RENDERER_ACCESSIBILITY_GEOMETRY_TRACKING_REF,
    NATIVE_ACCESSIBILITY_AUTHORITY_TRACKING_REF,
];

const EXPECTED_CANONICAL_CONTENT: [ExpectedCanonicalContent; 9] = [
    ExpectedCanonicalContent {
        content_corpus_id: "content.gpu_text_basic_paragraph.v1",
        repository_ref: "tests/golden/gpu/text-basic-paragraph/input.json",
        semantic_kinds: &CONTENT_SEM_ASCII,
        identity: ExpectedContentIdentity::Payload {
            payload_ref: "tests/golden/gpu/text-basic-paragraph/input.json",
            selector: ExpectedContentSelector::Whole,
            encoding: RendererContentEncoding::GpuFixtureStateV1,
            decoder: RendererContentDecoder::JsonFixtureStateV1,
            framing: RendererContentFraming::TypedStateOverlay,
            encoded_sha256: "c4869c8fc8188fab36b5bc87367a96d80445470c9f3a11c17c317996bf11a8f7",
            decoded_sha256: "c4869c8fc8188fab36b5bc87367a96d80445470c9f3a11c17c317996bf11a8f7",
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.terminal_utf8_grapheme.v1",
        repository_ref: "tests/fixtures/terminal-conformance/transcripts/tc-utf8-grapheme-001.hex",
        semantic_kinds: &CONTENT_SEM_UNICODE,
        identity: ExpectedContentIdentity::Payload {
            payload_ref: "tests/fixtures/terminal-conformance/transcripts/tc-utf8-grapheme-001.hex",
            selector: ExpectedContentSelector::Segment {
                manifest_ref: "tests/fixtures/terminal-conformance/manifest.json",
                row_id: "tc-utf8-grapheme-001",
                start: 0,
                end: 33,
            },
            encoding: RendererContentEncoding::HexTranscriptV1,
            decoder: RendererContentDecoder::HexDecodeV1,
            framing: RendererContentFraming::CompleteTerminalStream,
            encoded_sha256: "3e8115d4d9c6d763b082e2288a8220c925c2e662d50c3fdfb7eb1492760e6247",
            decoded_sha256: "8dece071425a87156e77ba54389d107f0b04ebcba2cd78e5399eb1ffda107167",
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.gpu_text_rtl.v1",
        repository_ref: "tests/golden/gpu/text-rtl-arabic-hebrew/input.json",
        semantic_kinds: &CONTENT_SEM_RTL,
        identity: ExpectedContentIdentity::Payload {
            payload_ref: "tests/golden/gpu/text-rtl-arabic-hebrew/input.json",
            selector: ExpectedContentSelector::Whole,
            encoding: RendererContentEncoding::GpuFixtureStateV1,
            decoder: RendererContentDecoder::JsonFixtureStateV1,
            framing: RendererContentFraming::TypedStateOverlay,
            encoded_sha256: "4fafe3e034512ae98eb03cf7fbcf72825767a8f502379a2b902ad715c0d4b492",
            decoded_sha256: "4fafe3e034512ae98eb03cf7fbcf72825767a8f502379a2b902ad715c0d4b492",
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.terminal_osc8_hyperlink.v1",
        repository_ref: "tests/fixtures/terminal-conformance/transcripts/tc-osc8-hyperlink-001.hex",
        semantic_kinds: &CONTENT_SEM_HYPERLINK,
        identity: ExpectedContentIdentity::Payload {
            payload_ref: "tests/fixtures/terminal-conformance/transcripts/tc-osc8-hyperlink-001.hex",
            selector: ExpectedContentSelector::Segment {
                manifest_ref: "tests/fixtures/terminal-conformance/manifest.json",
                row_id: "tc-osc8-hyperlink-001",
                start: 0,
                end: 53,
            },
            encoding: RendererContentEncoding::HexTranscriptV1,
            decoder: RendererContentDecoder::HexDecodeV1,
            framing: RendererContentFraming::CompleteTerminalStream,
            encoded_sha256: "4338305b307daaec010e2943142cadcf68a45b513e891ba98ecdb4830757c3c8",
            decoded_sha256: "97a382742339301dbe964eb1afc060db5f47f4c15da059855d4c15dc4d1e8169",
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.terminal_alt_screen_hold.v1",
        repository_ref: "tests/fixtures/terminal-conformance/transcripts/tc-alt-screen-001.hex",
        semantic_kinds: &CONTENT_SEM_ALT,
        identity: ExpectedContentIdentity::Payload {
            payload_ref: "tests/fixtures/terminal-conformance/transcripts/tc-alt-screen-001.hex",
            selector: ExpectedContentSelector::Segment {
                manifest_ref: "tests/fixtures/terminal-conformance/manifest.json",
                row_id: "tc-alt-screen-001",
                start: 0,
                end: 43,
            },
            encoding: RendererContentEncoding::HexTranscriptV1,
            decoder: RendererContentDecoder::HexDecodeV1,
            framing: RendererContentFraming::CompleteTerminalStream,
            encoded_sha256: "9e1d3dee919739df844f359caa59e940bc3979d4522ff4fde22593af795d2df9",
            decoded_sha256: "102be893f109ad1d43e832122634e1132ac5885b94a691ea29e0601607d59996",
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.sixel_seed.v1",
        repository_ref: "fuzz/corpus/term_advance_bytes/seed_dcs_sixel_fragment.bin",
        semantic_kinds: &CONTENT_SEM_IMAGE,
        identity: ExpectedContentIdentity::Payload {
            payload_ref: "fuzz/corpus/term_advance_bytes/seed_dcs_sixel_fragment.bin",
            selector: ExpectedContentSelector::Whole,
            encoding: RendererContentEncoding::RawTerminalBytes,
            decoder: RendererContentDecoder::Identity,
            framing: RendererContentFraming::CompleteTerminalStream,
            encoded_sha256: "ba2f48b0bb4e567cd66fcd75188ec870fd0b5a23474366e5a5d3deac4ea9d162",
            decoded_sha256: "ba2f48b0bb4e567cd66fcd75188ec870fd0b5a23474366e5a5d3deac4ea9d162",
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.ligature_enabled_gap.v1",
        repository_ref: "tests/golden/gpu/README.md",
        semantic_kinds: &CONTENT_SEM_LIGATURE,
        identity: ExpectedContentIdentity::UnavailableGenerator {
            generator_id: "renderer_ligature_content_v1",
            input_manifest_ref: "tests/golden/gpu/README.md",
            seed: 1,
            encoding: RendererContentEncoding::GeneratedTerminalBytesV1,
            framing: RendererContentFraming::Utf8Text,
            tracking_refs: &CONTENT_TRACK_LIGATURE,
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.live_ime_gap.v1",
        repository_ref: "tests/golden/gpu/overlay-ime-composition/input.json",
        semantic_kinds: &CONTENT_SEM_STATE_ONLY,
        identity: ExpectedContentIdentity::UnavailableGenerator {
            generator_id: "renderer_live_ime_state_v1",
            input_manifest_ref: "tests/golden/gpu/overlay-ime-composition/input.json",
            seed: 2,
            encoding: RendererContentEncoding::GeneratedTypedStateV1,
            framing: RendererContentFraming::TypedStateOverlay,
            tracking_refs: &CONTENT_TRACK_IME,
        },
    },
    ExpectedCanonicalContent {
        content_corpus_id: "content.a11y_geometry_gap.v1",
        repository_ref: "docs/a11y/scenario-corpus.md",
        semantic_kinds: &CONTENT_SEM_STATE_ONLY,
        identity: ExpectedContentIdentity::UnavailableGenerator {
            generator_id: "renderer_a11y_geometry_state_v1",
            input_manifest_ref: "docs/a11y/scenario-corpus.md",
            seed: 3,
            encoding: RendererContentEncoding::GeneratedTypedStateV1,
            framing: RendererContentFraming::TypedStateOverlay,
            tracking_refs: &CONTENT_TRACK_A11Y,
        },
    },
];

fn validate_canonical_content_reference(
    path: &str,
    actual: &RendererContentCorpusReference,
    expected: &ExpectedCanonicalContent,
    validator: &mut Validator,
) {
    if actual.content_corpus_id != expected.content_corpus_id
        || actual.repository_ref != expected.repository_ref
        || actual.semantic_kinds.as_slice() != expected.semantic_kinds
        || actual.payload_revision != 1
    {
        validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            path,
            format!(
                "expected canonical content `{}` at `{}` with revision 1 and semantics {:?}",
                expected.content_corpus_id, expected.repository_ref, expected.semantic_kinds
            ),
        );
    }
    match (&actual.deterministic_identity, expected.identity) {
        (
            RendererContentDeterministicIdentity::Payload {
                payload_ref,
                selector,
                encoding,
                decoder,
                framing,
                encoded_payload_sha256,
                decoded_payload_sha256,
            },
            ExpectedContentIdentity::Payload {
                payload_ref: expected_ref,
                selector: expected_selector,
                encoding: expected_encoding,
                decoder: expected_decoder,
                framing: expected_framing,
                encoded_sha256,
                decoded_sha256,
            },
        ) => {
            let selector_matches = match (selector, expected_selector) {
                (RendererContentPayloadSelector::WholePayload, ExpectedContentSelector::Whole) => true,
                (
                    RendererContentPayloadSelector::ManifestRowSegment {
                        manifest_ref,
                        manifest_row_id,
                        decoded_byte_start,
                        decoded_byte_end_exclusive,
                    },
                    ExpectedContentSelector::Segment {
                        manifest_ref: expected_manifest,
                        row_id,
                        start,
                        end,
                    },
                ) => manifest_ref == expected_manifest
                    && manifest_row_id == row_id
                    && *decoded_byte_start == start
                    && *decoded_byte_end_exclusive == end,
                _ => false,
            };
            if payload_ref != expected_ref
                || *encoding != expected_encoding
                || *decoder != expected_decoder
                || *framing != expected_framing
                || encoded_payload_sha256 != encoded_sha256
                || decoded_payload_sha256 != decoded_sha256
                || !selector_matches
                || !matches!(actual.availability, RendererContentInputAvailability::Available)
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{path}.deterministic_identity"),
                    "payload identity, selector, digest layer, decoder, framing, or availability differs from the frozen v1 mapping",
                );
            }
        }
        (
            RendererContentDeterministicIdentity::Generator {
                generator_id,
                generator_revision,
                generator_seed,
                input_manifest_ref,
                output_encoding,
                output_decoder,
                output_framing,
            },
            ExpectedContentIdentity::UnavailableGenerator {
                generator_id: expected_generator,
                input_manifest_ref: expected_manifest,
                seed,
                encoding,
                framing,
                tracking_refs,
            },
        ) => {
            let actual_tracking = match &actual.availability {
                RendererContentInputAvailability::Unavailable { tracking_refs, .. } => tracking_refs
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                RendererContentInputAvailability::Available => Vec::new(),
            };
            if generator_id != expected_generator
                || *generator_revision != 1
                || *generator_seed != seed
                || input_manifest_ref != expected_manifest
                || *output_encoding != encoding
                || *output_decoder != RendererContentDecoder::GeneratorV1
                || *output_framing != framing
                || actual_tracking.as_slice() != tracking_refs
                || !matches!(actual.availability, RendererContentInputAvailability::Unavailable { .. })
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{path}.deterministic_identity"),
                    "unavailable generator identity or tracked readiness differs from the frozen v1 mapping",
                );
            }
        }
        _ => validator.error(
            RendererScenarioValidationCode::InvalidCorpusReference,
            format!("{path}.deterministic_identity"),
            "content identity variant differs from the frozen v1 mapping",
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedManifestState {
    surfaces: Vec<RendererSurfaceState>,
    outputs: Vec<RendererPaneOutputState>,
    window_states: Vec<RendererPhaseWindowState>,
    pane_geometry: Vec<ExpandedPaneGeometry>,
    applied_materialization_steps: Vec<Vec<RendererContentMaterializationStep>>,
    features: BTreeSet<RendererTerminalFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPaneMaterialization {
    applied_steps: Vec<RendererContentMaterializationStep>,
    active_buffer: RendererTerminalBufferKind,
    primary_content_corpus_ids: Vec<String>,
    alternate_content_corpus_ids: Vec<String>,
    semantics: BTreeSet<RendererContentSemanticKind>,
}

fn distribution_steps_for_pane(
    distribution: &RendererContentDistributionProfile,
    pane_ordinal: u16,
) -> Option<&[RendererContentMaterializationStep]> {
    distribution.assignments.iter().find_map(|assignment| {
        let selected = match &assignment.selector {
            RendererPaneOrdinalSelector::All => true,
            RendererPaneOrdinalSelector::OrdinalRange {
                start,
                end_exclusive,
            } => pane_ordinal >= *start && pane_ordinal < *end_exclusive,
            RendererPaneOrdinalSelector::Explicit { ordinals } => {
                ordinals.binary_search(&pane_ordinal).is_ok()
            }
        };
        selected.then_some(assignment.materialization_steps.as_slice())
    })
}

fn semantic_feature(kind: RendererContentSemanticKind) -> RendererTerminalFeature {
    match kind {
        RendererContentSemanticKind::AsciiText => RendererTerminalFeature::Ascii,
        RendererContentSemanticKind::CjkText => RendererTerminalFeature::Cjk,
        RendererContentSemanticKind::RtlText => RendererTerminalFeature::Rtl,
        RendererContentSemanticKind::CombiningMarkText => {
            RendererTerminalFeature::CombiningMarks
        }
        RendererContentSemanticKind::EmojiText => RendererTerminalFeature::Emoji,
        RendererContentSemanticKind::LigatureSequence => RendererTerminalFeature::Ligatures,
        RendererContentSemanticKind::ImageProtocol => RendererTerminalFeature::Images,
        RendererContentSemanticKind::HyperlinkProtocol => RendererTerminalFeature::Hyperlinks,
        RendererContentSemanticKind::AlternateScreenControl => {
            RendererTerminalFeature::AlternateScreen
        }
    }
}

fn overlay_checkpoint_position(
    scenario: &RendererScenarioDefinition,
    overlay_id: RendererCoverageOverlayId,
    checkpoint_id: &str,
) -> Option<usize> {
    scenario
        .visual_checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.overlay_id == overlay_id)
        .position(|checkpoint| checkpoint.checkpoint_id == checkpoint_id)
}

fn materialization_step_applies(
    step: &RendererContentMaterializationStep,
    scenario: &RendererScenarioDefinition,
    checkpoint: &RendererVisualCheckpoint,
) -> Result<bool, String> {
    let current_position = overlay_checkpoint_position(
        scenario,
        checkpoint.overlay_id,
        &checkpoint.checkpoint_id,
    )
    .ok_or_else(|| format!("checkpoint `{}` is not in its overlay", checkpoint.checkpoint_id))?;
    let (boundary_reached, minimum_hold_position, minimum_hold_event) =
        match &step.application_boundary {
        RendererContentApplicationBoundary::BeforeGesture => (true, 0, 0),
        RendererContentApplicationBoundary::AtEvent { event_ordinal } => {
            let Some(event) = scenario.timeline.get(*event_ordinal as usize) else {
                return Err(format!(
                    "at-event materialization boundary {event_ordinal} is outside the scenario timeline"
                ));
            };
            if event.event_ordinal != *event_ordinal {
                return Err(format!(
                    "at-event materialization boundary {event_ordinal} does not resolve to that exact timeline event"
                ));
            }
            (
                *event_ordinal <= checkpoint.event_ordinal,
                0,
                *event_ordinal,
            )
        }
        RendererContentApplicationBoundary::AfterCheckpoint { checkpoint_id } => {
            let boundary_position = overlay_checkpoint_position(
                scenario,
                checkpoint.overlay_id,
                checkpoint_id,
            )
            .ok_or_else(|| {
                format!(
                    "after-checkpoint boundary `{checkpoint_id}` is absent from overlay `{}`",
                    checkpoint.overlay_id.as_str()
                )
            })?;
            (
                boundary_position < current_position,
                boundary_position
                    .checked_add(1)
                    .ok_or_else(|| "checkpoint boundary position overflowed".to_string())?,
                0,
            )
        }
    };
    // Hold-through IDs are minimum-survival assertions. They never imply an
    // undo; applied operations persist until an explicit Replace or Exit.
    let mut previous_hold_position = None;
    for hold_id in &step.hold_through_checkpoint_ids {
        let hold_position = overlay_checkpoint_position(
            scenario,
            checkpoint.overlay_id,
            hold_id,
        )
        .ok_or_else(|| {
            format!(
                "hold-through checkpoint `{hold_id}` is absent from overlay `{}`",
                checkpoint.overlay_id.as_str()
            )
        })?;
        let hold_checkpoint = scenario
            .visual_checkpoints
            .iter()
            .filter(|candidate| candidate.overlay_id == checkpoint.overlay_id)
            .nth(hold_position)
            .ok_or_else(|| {
                format!(
                    "hold-through checkpoint `{hold_id}` cannot be resolved in overlay `{}`",
                    checkpoint.overlay_id.as_str()
                )
            })?;
        if hold_position < minimum_hold_position
            || hold_checkpoint.event_ordinal < minimum_hold_event
        {
            return Err(format!(
                "hold-through checkpoint `{hold_id}` precedes its materialization boundary"
            ));
        }
        if previous_hold_position.is_some_and(|previous| hold_position <= previous) {
            return Err(format!(
                "hold-through checkpoints must be unique and in canonical overlay order; `{hold_id}` is out of order"
            ));
        }
        previous_hold_position = Some(hold_position);
    }
    Ok(boundary_reached)
}

fn resolve_pane_materialization(
    distribution: &RendererContentDistributionProfile,
    pane_ordinal: u16,
    scenario: &RendererScenarioDefinition,
    checkpoint: &RendererVisualCheckpoint,
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
) -> Result<ResolvedPaneMaterialization, String> {
    let steps = distribution_steps_for_pane(distribution, pane_ordinal)
        .ok_or_else(|| format!("pane {pane_ordinal} has no content assignment"))?;
    let mut active_buffer = RendererTerminalBufferKind::Primary;
    let mut primary_content_corpus_ids = Vec::new();
    let mut alternate_content_corpus_ids = Vec::new();
    let mut applied_steps = Vec::new();
    let mut held_effects = Vec::new();
    let current_checkpoint_position = overlay_checkpoint_position(
        scenario,
        checkpoint.overlay_id,
        &checkpoint.checkpoint_id,
    )
    .ok_or_else(|| format!("checkpoint `{}` is not in its overlay", checkpoint.checkpoint_id))?;
    for step in steps {
        if !materialization_step_applies(step, scenario, checkpoint)? {
            continue;
        }
        let reference = content
            .get(step.content_corpus_id.as_str())
            .copied()
            .ok_or_else(|| {
                format!("undefined content corpus `{}`", step.content_corpus_id)
            })?;
        let target_buffer = active_buffer;
        let active_content = match target_buffer {
            RendererTerminalBufferKind::Primary => &mut primary_content_corpus_ids,
            RendererTerminalBufferKind::Alternate => &mut alternate_content_corpus_ids,
        };
        match step.operation {
            RendererContentCompositionOperation::ReplaceActiveBuffer => {
                active_content.clear();
                active_content.push(step.content_corpus_id.clone());
            }
            RendererContentCompositionOperation::AppendToActiveBuffer => {
                active_content.push(step.content_corpus_id.clone());
            }
            RendererContentCompositionOperation::EnterAlternateBuffer => {
                active_buffer = RendererTerminalBufferKind::Alternate;
                alternate_content_corpus_ids.push(step.content_corpus_id.clone());
            }
            RendererContentCompositionOperation::ExitAlternateBuffer => {
                active_buffer = RendererTerminalBufferKind::Primary;
            }
            RendererContentCompositionOperation::ApplyTypedStateOverlay => {
                if !reference.semantic_kinds.is_empty() {
                    active_content.push(step.content_corpus_id.clone());
                }
            }
        }
        let furthest_hold_position = step
            .hold_through_checkpoint_ids
            .iter()
            .filter_map(|checkpoint_id| {
                overlay_checkpoint_position(scenario, checkpoint.overlay_id, checkpoint_id)
            })
            .max();
        if furthest_hold_position
            .is_some_and(|furthest| current_checkpoint_position <= furthest)
        {
            held_effects.push((step, target_buffer));
        }
        applied_steps.push(step.clone());
    }
    for (step, target_buffer) in held_effects {
        let target_content = match target_buffer {
            RendererTerminalBufferKind::Primary => &primary_content_corpus_ids,
            RendererTerminalBufferKind::Alternate => &alternate_content_corpus_ids,
        };
        let effect_survives = match step.operation {
            RendererContentCompositionOperation::ReplaceActiveBuffer
            | RendererContentCompositionOperation::AppendToActiveBuffer => {
                target_content.contains(&step.content_corpus_id)
            }
            RendererContentCompositionOperation::EnterAlternateBuffer => {
                active_buffer == RendererTerminalBufferKind::Alternate
                    && alternate_content_corpus_ids.contains(&step.content_corpus_id)
            }
            RendererContentCompositionOperation::ExitAlternateBuffer => {
                active_buffer == RendererTerminalBufferKind::Primary
            }
            RendererContentCompositionOperation::ApplyTypedStateOverlay => {
                reference_has_visible_semantics(content, &step.content_corpus_id)
                    .is_none_or(|has_visible| {
                        !has_visible || target_content.contains(&step.content_corpus_id)
                    })
            }
        };
        if !effect_survives {
            return Err(format!(
                "materialization step {} effect does not survive continuously through promised checkpoint `{}`",
                step.step_ordinal, checkpoint.checkpoint_id
            ));
        }
    }
    let primary_unique = primary_content_corpus_ids.iter().collect::<BTreeSet<_>>();
    let alternate_unique = alternate_content_corpus_ids.iter().collect::<BTreeSet<_>>();
    if primary_unique.len() != primary_content_corpus_ids.len()
        || alternate_unique.len() != alternate_content_corpus_ids.len()
    {
        return Err(format!(
            "pane {pane_ordinal} materialization produced duplicate buffer content identities"
        ));
    }
    let visible_content = match active_buffer {
        RendererTerminalBufferKind::Primary => &primary_content_corpus_ids,
        RendererTerminalBufferKind::Alternate => &alternate_content_corpus_ids,
    };
    let semantics = visible_content
        .iter()
        .filter_map(|content_id| content.get(content_id.as_str()).copied())
        .flat_map(|reference| reference.semantic_kinds.iter().copied())
        .collect();
    Ok(ResolvedPaneMaterialization {
        applied_steps,
        active_buffer,
        primary_content_corpus_ids,
        alternate_content_corpus_ids,
        semantics,
    })
}

fn reference_has_visible_semantics(
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
    content_corpus_id: &str,
) -> Option<bool> {
    content
        .get(content_corpus_id)
        .map(|reference| !reference.semantic_kinds.is_empty())
}

fn derive_pane_features(
    state: &RendererSurfaceState,
    semantics: &BTreeSet<RendererContentSemanticKind>,
    configs: &BTreeMap<&str, &RendererConfigProfile>,
) -> BTreeSet<RendererTerminalFeature> {
    let mut features = BTreeSet::new();
    for semantic in semantics {
        match semantic {
            RendererContentSemanticKind::LigatureSequence
            | RendererContentSemanticKind::ImageProtocol
            | RendererContentSemanticKind::HyperlinkProtocol
            | RendererContentSemanticKind::AlternateScreenControl => {}
            other => {
                features.insert(semantic_feature(*other));
            }
        }
    }
    if state.terminal.cursor.visible
        && state.terminal.cursor.blink_phase != RendererCursorBlinkPhase::Off
    {
        features.insert(RendererTerminalFeature::Cursor);
    }
    if !matches!(state.terminal.selection, RendererSelectionState::Inactive) {
        features.insert(RendererTerminalFeature::Selection);
    }
    if !matches!(state.terminal.ime, RendererImeState::Inactive) {
        features.insert(RendererTerminalFeature::Ime);
    }
    if !state.terminal.inline_images.is_empty()
        && semantics.contains(&RendererContentSemanticKind::ImageProtocol)
    {
        features.insert(RendererTerminalFeature::Images);
    }
    if !state.terminal.hyperlinks.is_empty()
        && semantics.contains(&RendererContentSemanticKind::HyperlinkProtocol)
    {
        features.insert(RendererTerminalFeature::Hyperlinks);
    }
    if state.terminal.active_buffer == RendererTerminalBufferKind::Alternate
        && semantics.contains(&RendererContentSemanticKind::AlternateScreenControl)
    {
        features.insert(RendererTerminalFeature::AlternateScreen);
    }
    if matches!(
        state.terminal.accessibility_geometry,
        RendererAccessibilityGeometryState::Active { .. }
    ) {
        features.insert(RendererTerminalFeature::AccessibilityGeometry);
    }
    if semantics.contains(&RendererContentSemanticKind::LigatureSequence)
        && configs
            .get(state.renderer_config_profile_id.as_str())
            .is_some_and(|profile| {
                profile.ligature_features.calt_enabled
                    && profile.ligature_features.clig_enabled
                    && profile.ligature_features.liga_enabled
            })
    {
        features.insert(RendererTerminalFeature::Ligatures);
    }
    features
}

fn expand_manifest_state(
    manifest: &RendererPhaseManifest,
    index: &CatalogIndex<'_>,
    scenario: &RendererScenarioDefinition,
    checkpoint: &RendererVisualCheckpoint,
    workload: &RendererWorkloadDefinition,
) -> Result<ExpandedManifestState, String> {
    if checkpoint.phase_manifest_id != manifest.phase_manifest_id
        || checkpoint.event_ordinal != manifest.event_ordinal
        || checkpoint.phase != manifest.phase
        || checkpoint.overlay_id != manifest.overlay_id
    {
        return Err("checkpoint identity does not exactly bind phase manifest".to_string());
    }
    let layout = index
        .layout_profiles
        .get(manifest.layout_profile_id.as_str())
        .copied()
        .ok_or_else(|| format!("undefined layout `{}`", manifest.layout_profile_id))?;
    let distribution = index
        .content_distribution_profiles
        .get(manifest.content_distribution_profile_id.as_str())
        .copied()
        .ok_or_else(|| {
            format!(
                "undefined content distribution `{}`",
                manifest.content_distribution_profile_id
            )
        })?;
    let default_template = index
        .surface_state_templates
        .get(manifest.default_surface_state_template_id.as_str())
        .copied()
        .ok_or_else(|| {
            format!(
                "undefined surface template `{}`",
                manifest.default_surface_state_template_id
            )
        })?;
    let mut surfaces = vec![default_template.surface_state.clone(); usize::from(layout.pane_count)];
    let mut outputs = vec![manifest.default_output.clone(); usize::from(layout.pane_count)];
    for pane_override in &manifest.pane_overrides {
        let Some(template) = index
            .surface_state_templates
            .get(pane_override.surface_state_template_id.as_str())
        else {
            return Err(format!(
                "undefined surface template `{}`",
                pane_override.surface_state_template_id
            ));
        };
        let ordinals = match &pane_override.selector {
            RendererPaneOrdinalSelector::All => (0..layout.pane_count).collect::<Vec<_>>(),
            RendererPaneOrdinalSelector::OrdinalRange {
                start,
                end_exclusive,
            } => (*start..(*end_exclusive).min(layout.pane_count)).collect(),
            RendererPaneOrdinalSelector::Explicit { ordinals } => ordinals
                .iter()
                .copied()
                .filter(|ordinal| *ordinal < layout.pane_count)
                .collect(),
        };
        for ordinal in ordinals {
            surfaces[usize::from(ordinal)] = template.surface_state.clone();
            outputs[usize::from(ordinal)] = pane_override.output.clone();
        }
    }
    let pane_geometry = expand_manifest_pane_geometry(layout, manifest)?;
    let mut features = BTreeSet::new();
    let mut applied_materialization_steps = Vec::with_capacity(surfaces.len());
    for (ordinal, state) in surfaces.iter().enumerate() {
        let materialization = resolve_pane_materialization(
            distribution,
            ordinal as u16,
            scenario,
            checkpoint,
            &index.content,
        )?;
        if state.terminal.active_buffer != materialization.active_buffer
            || state.terminal.primary_buffer.content_corpus_ids
                != materialization.primary_content_corpus_ids
            || state.terminal.alternate_buffer.content_corpus_ids
                != materialization.alternate_content_corpus_ids
        {
            return Err(format!(
                "pane {ordinal} terminal buffers contradict ordered content materialization"
            ));
        }
        if state.terminal.primary_buffer.scrollback_lines
            != workload.scrollback_lines_per_pane
        {
            return Err(format!(
                "pane {ordinal} primary scrollback {} differs from workload {}",
                state.terminal.primary_buffer.scrollback_lines,
                workload.scrollback_lines_per_pane
            ));
        }
        let geometry = &pane_geometry[ordinal];
        if state.display.viewport_width_px != geometry.rect.width
            || state.display.viewport_height_px != geometry.rect.height
        {
            return Err(format!(
                "pane {ordinal} surface viewport {}x{} differs from split rect {}x{}",
                state.display.viewport_width_px,
                state.display.viewport_height_px,
                geometry.rect.width,
                geometry.rect.height
            ));
        }
        let mut recomputed = state.clone();
        recompute_surface_geometry(&mut recomputed)?;
        if &recomputed != state {
            return Err(format!(
                "pane {ordinal} carries stale or noncanonical derived pixel geometry"
            ));
        }
        features.extend(derive_pane_features(
            state,
            &materialization.semantics,
            &index.renderer_config_profiles,
        ));
        applied_materialization_steps.push(materialization.applied_steps);
    }
    Ok(ExpandedManifestState {
        surfaces,
        outputs,
        window_states: manifest.window_states.clone(),
        pane_geometry,
        applied_materialization_steps,
        features,
    })
}

const OVERLAY_FEATURES_PRODUCTION: [RendererTerminalFeature; 2] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Cursor,
];
const OVERLAY_FEATURES_UNICODE: [RendererTerminalFeature; 6] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Cjk,
    RendererTerminalFeature::Rtl,
    RendererTerminalFeature::CombiningMarks,
    RendererTerminalFeature::Emoji,
    RendererTerminalFeature::Cursor,
];
const OVERLAY_FEATURES_ALT: [RendererTerminalFeature; 3] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::AlternateScreen,
    RendererTerminalFeature::Cursor,
];
const OVERLAY_FEATURES_IME: [RendererTerminalFeature; 3] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Cursor,
    RendererTerminalFeature::Ime,
];
const OVERLAY_FEATURES_IMAGE_LINK: [RendererTerminalFeature; 4] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Images,
    RendererTerminalFeature::Hyperlinks,
    RendererTerminalFeature::Cursor,
];
const OVERLAY_FEATURES_LIGATURE: [RendererTerminalFeature; 3] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Ligatures,
    RendererTerminalFeature::Cursor,
];
const OVERLAY_FEATURES_SELECTION: [RendererTerminalFeature; 3] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Selection,
    RendererTerminalFeature::Cursor,
];
const OVERLAY_FEATURES_A11Y: [RendererTerminalFeature; 3] = [
    RendererTerminalFeature::Ascii,
    RendererTerminalFeature::Cursor,
    RendererTerminalFeature::AccessibilityGeometry,
];

const fn expected_overlay_features(
    overlay_id: RendererCoverageOverlayId,
) -> &'static [RendererTerminalFeature] {
    match overlay_id {
        RendererCoverageOverlayId::ProductionDefault => &OVERLAY_FEATURES_PRODUCTION,
        RendererCoverageOverlayId::UnicodeMaximal => &OVERLAY_FEATURES_UNICODE,
        RendererCoverageOverlayId::AlternateScreen => &OVERLAY_FEATURES_ALT,
        RendererCoverageOverlayId::ImeComposing => &OVERLAY_FEATURES_IME,
        RendererCoverageOverlayId::ImageHyperlink => &OVERLAY_FEATURES_IMAGE_LINK,
        RendererCoverageOverlayId::LigatureEnabled => &OVERLAY_FEATURES_LIGATURE,
        RendererCoverageOverlayId::Selection => &OVERLAY_FEATURES_SELECTION,
        RendererCoverageOverlayId::A11yGeometry => &OVERLAY_FEATURES_A11Y,
    }
}

fn canonical_feature_set(features: &[RendererTerminalFeature]) -> BTreeSet<RendererTerminalFeature> {
    features.iter().copied().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActiveTimelineActionKind {
    BeginGesture,
    SetWindowSize,
    SetGrid,
    SetFontScale,
    SetQualityMode,
    MoveToDisplay,
    SetOutputRate,
    ForegroundKey,
    SetRevisions,
    EndGesture,
    Settle,
}

const fn active_timeline_action_kind(action: &RendererTimelineAction) -> ActiveTimelineActionKind {
    match action {
        RendererTimelineAction::BeginGesture => ActiveTimelineActionKind::BeginGesture,
        RendererTimelineAction::SetWindowSize { .. } => ActiveTimelineActionKind::SetWindowSize,
        RendererTimelineAction::SetGrid { .. } => ActiveTimelineActionKind::SetGrid,
        RendererTimelineAction::SetFontScale { .. } => ActiveTimelineActionKind::SetFontScale,
        RendererTimelineAction::SetQualityMode { .. } => ActiveTimelineActionKind::SetQualityMode,
        RendererTimelineAction::MoveToDisplay { .. } => ActiveTimelineActionKind::MoveToDisplay,
        RendererTimelineAction::SetOutputRate { .. } => ActiveTimelineActionKind::SetOutputRate,
        RendererTimelineAction::ForegroundKey { .. } => ActiveTimelineActionKind::ForegroundKey,
        RendererTimelineAction::SetRevisions { .. } => ActiveTimelineActionKind::SetRevisions,
        RendererTimelineAction::EndGesture => ActiveTimelineActionKind::EndGesture,
        RendererTimelineAction::Settle => ActiveTimelineActionKind::Settle,
    }
}

fn timeline_action_target(action: &RendererTimelineAction) -> Option<&RendererMutationTarget> {
    match action {
        RendererTimelineAction::SetWindowSize { target, .. }
        | RendererTimelineAction::SetGrid { target, .. }
        | RendererTimelineAction::SetFontScale { target, .. }
        | RendererTimelineAction::SetQualityMode { target, .. }
        | RendererTimelineAction::MoveToDisplay { target, .. }
        | RendererTimelineAction::SetRevisions { target, .. } => Some(target),
        RendererTimelineAction::BeginGesture
        | RendererTimelineAction::SetOutputRate { .. }
        | RendererTimelineAction::ForegroundKey { .. }
        | RendererTimelineAction::EndGesture
        | RendererTimelineAction::Settle => None,
    }
}

fn validate_mutation_target(
    path: &str,
    target: &RendererMutationTarget,
    action_kind: ActiveTimelineActionKind,
    layout: &RendererLayoutProfile,
    validator: &mut Validator,
) -> Vec<u16> {
    let expanded = expand_layout(layout);
    validator.require_identifier(&format!("{path}.window_id"), &target.window_id);
    let window_ordinal = expanded
        .window_ids
        .iter()
        .position(|id| id == &target.window_id)
        .map(|value| value as u16);
    if window_ordinal.is_none() {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            format!("{path}.window_id"),
            format!("undefined target window `{}`", target.window_id),
        );
    }
    let tab_ordinal = target.tab_id.as_ref().and_then(|tab_id| {
        validator.require_identifier(&format!("{path}.tab_id"), tab_id);
        let ordinal = expanded.tab_ids.iter().position(|id| id == tab_id).map(|value| value as u16);
        if ordinal.is_none() {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{path}.tab_id"),
                format!("undefined target tab `{tab_id}`"),
            );
        }
        ordinal
    });
    if let (Some(window), Some(tab)) = (window_ordinal, tab_ordinal)
        && expanded.tab_window_ordinals[usize::from(tab)] != window
    {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            path,
            "target tab does not belong to target window",
        );
    }
    if matches!(
        action_kind,
        ActiveTimelineActionKind::SetWindowSize | ActiveTimelineActionKind::MoveToDisplay
    ) && target.tab_id.is_some()
    {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            format!("{path}.tab_id"),
            "window-size and display-move mutations must be window-scoped",
        );
    }
    let expected_ordinals = match (window_ordinal, tab_ordinal) {
        (_, Some(tab)) => expanded
            .pane_tab_ordinals
            .iter()
            .enumerate()
            .filter_map(|(ordinal, owner)| (*owner == tab).then_some(ordinal as u16))
            .collect::<Vec<_>>(),
        (Some(window), None) => expanded
            .pane_window_ordinals
            .iter()
            .enumerate()
            .filter_map(|(ordinal, owner)| (*owner == window).then_some(ordinal as u16))
            .collect::<Vec<_>>(),
        (None, None) => Vec::new(),
    };
    let expected_ids = expected_ordinals
        .iter()
        .map(|ordinal| expanded.pane_ids[usize::from(*ordinal)].as_str())
        .collect::<Vec<_>>();
    let actual_ids = target
        .affected_pane_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if actual_ids != expected_ids {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            format!("{path}.affected_pane_ids"),
            format!("affected pane IDs must equal canonical target expansion {expected_ids:?}"),
        );
    }
    expected_ordinals
}

#[derive(Debug, Default)]
struct TimelineFacts {
    resize_event_ordinals: Vec<u32>,
    grid_event_ordinals: Vec<u32>,
    font_event_ordinals: Vec<u32>,
    display_event_ordinals: Vec<u32>,
    key_actions: Vec<(u32, String)>,
    output_actions: Vec<(u32, String, u64)>,
    end_event_ordinal: Option<u32>,
    snap_back_event_ordinal: Option<u32>,
}

const fn gesture_is_live_resize(gesture: RendererGesture) -> bool {
    matches!(
        gesture,
        RendererGesture::SameGridDrag
            | RendererGesture::GridChangingDrag
            | RendererGesture::Reflow80To200
            | RendererGesture::Reflow200To80
            | RendererGesture::OutputOverlapResize
    )
}

fn validate_timeline(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: &RendererWorkloadDefinition,
    layout: &RendererLayoutProfile,
    validator: &mut Validator,
) -> TimelineFacts {
    let path = format!("{scenario_path}.timeline");
    let mut facts = TimelineFacts::default();
    if scenario.timeline.is_empty() || scenario.timeline.len() > MAX_RENDERER_TIMELINE_EVENTS {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            format!("timeline must contain 1..={MAX_RENDERER_TIMELINE_EVENTS} events"),
        );
        return facts;
    }
    if scenario.timeline.len() != workload.event_count as usize {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            &path,
            format!("timeline event count must equal workload event_count {}", workload.event_count),
        );
    }
    let mut previous_at = None;
    let mut previous_phase = None;
    let mut end_count = 0_usize;
    let mut begin_count = 0_usize;
    let mut settle_count = 0_usize;
    let mut snap_count = 0_usize;
    for (position, event) in scenario.timeline.iter().enumerate() {
        let event_path = format!("{path}[{position}]");
        if event.event_ordinal as usize != position {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.event_ordinal"),
                format!("expected contiguous event ordinal {position}"),
            );
        }
        if position == 0 && event.at_us != 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.at_us"),
                "event zero must occur at zero microseconds",
            );
        }
        if previous_at.is_some_and(|previous| event.at_us <= previous) {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.at_us"),
                "event offsets must be strictly increasing",
            );
        }
        if event.at_us > workload.total_duration_us {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.at_us"),
                "event occurs after workload total duration",
            );
        }
        previous_at = Some(event.at_us);
        if previous_phase.is_some_and(|phase: RendererTimelinePhase| event.phase.rank() < phase.rank()) {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.phase"),
                "timeline phases must be monotonic",
            );
        }
        previous_phase = Some(event.phase);
        if event.actions.is_empty() || event.actions.len() > MAX_RENDERER_ACTIONS_PER_EVENT {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.actions"),
                format!("atomic bundle must contain 1..={MAX_RENDERER_ACTIONS_PER_EVENT} actions"),
            );
        }
        let mut kinds = BTreeSet::new();
        let mut coherent_mutation_target: Option<&RendererMutationTarget> = None;
        for (action_position, action) in event.actions.iter().enumerate() {
            let action_path = format!("{event_path}.actions[{action_position}]");
            let kind = active_timeline_action_kind(action);
            if !kinds.insert(kind) {
                validator.error(
                    RendererScenarioValidationCode::InvalidTimeline,
                    &action_path,
                    format!("duplicate action kind {kind:?} in one atomic bundle"),
                );
            }
            if let Some(target) = timeline_action_target(action) {
                validate_mutation_target(&format!("{action_path}.target"), target, kind, layout, validator);
                if let Some(expected) = coherent_mutation_target {
                    if target != expected {
                        validator.error(
                            RendererScenarioValidationCode::InvalidTimeline,
                            format!("{action_path}.target"),
                            "all surface-changing actions and SetRevisions in one atomic event must use one identical mutation target",
                        );
                    }
                } else {
                    coherent_mutation_target = Some(target);
                }
            }
            match action {
                RendererTimelineAction::BeginGesture => begin_count += 1,
                RendererTimelineAction::SetWindowSize { width_px, height_px, .. } => {
                    if *width_px == 0
                        || *height_px == 0
                        || *width_px > MAX_VIEWPORT_DIMENSION_PX
                        || *height_px > MAX_VIEWPORT_DIMENSION_PX
                    {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, &action_path, "window size is outside contract bounds");
                    }
                }
                RendererTimelineAction::SetGrid { columns, rows, .. } => {
                    validate_grid_state(&action_path, RendererGridState { columns: *columns, rows: *rows }, validator);
                    facts.grid_event_ordinals.push(event.event_ordinal);
                }
                RendererTimelineAction::SetFontScale { scale_milli, .. } => {
                    if *scale_milli == 0 || *scale_milli > MAX_SCALE_FACTOR_MILLI {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, &action_path, "font scale is outside contract bounds");
                    }
                    facts.font_event_ordinals.push(event.event_ordinal);
                }
                RendererTimelineAction::SetQualityMode { mode, .. } => {
                    if event.phase == RendererTimelinePhase::Mutation && *mode != RendererQualityMode::Draft {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, &action_path, "mutation-phase quality transition may only enter Draft");
                    }
                    if event.phase == RendererTimelinePhase::SnapBack && *mode != RendererQualityMode::Standard {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, &action_path, "snap-back must transition to Standard");
                    }
                }
                RendererTimelineAction::MoveToDisplay { display, .. } => {
                    validate_display_transition(
                        &format!("{action_path}.display"),
                        display,
                        validator,
                    );
                    facts.display_event_ordinals.push(event.event_ordinal);
                }
                RendererTimelineAction::SetOutputRate {
                    stream_id,
                    bytes_per_second,
                } => {
                    validator.require_identifier(&format!("{action_path}.stream_id"), stream_id);
                    if *bytes_per_second > MAX_RENDERER_OUTPUT_BYTES_PER_SECOND {
                        validator.error(RendererScenarioValidationCode::LimitExceeded, &action_path, "output rate exceeds contract bound");
                    }
                    facts.output_actions.push((event.event_ordinal, stream_id.clone(), *bytes_per_second));
                }
                RendererTimelineAction::ForegroundKey { key_event_id } => {
                    validator.require_identifier(&format!("{action_path}.key_event_id"), key_event_id);
                    facts.key_actions.push((event.event_ordinal, key_event_id.clone()));
                }
                RendererTimelineAction::SetRevisions {
                    renderer_generation,
                    grid_revision,
                    terminal_revision,
                    ..
                } => {
                    if *renderer_generation == 0 || *grid_revision == 0 || *terminal_revision == 0 {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, &action_path, "all explicit revisions must be positive");
                    }
                }
                RendererTimelineAction::EndGesture => {
                    end_count += 1;
                    facts.end_event_ordinal = Some(event.event_ordinal);
                }
                RendererTimelineAction::Settle => settle_count += 1,
            }
        }
        if kinds.contains(&ActiveTimelineActionKind::SetWindowSize)
            || kinds.contains(&ActiveTimelineActionKind::SetGrid)
        {
            facts.resize_event_ordinals.push(event.event_ordinal);
        }
        if scenario.gesture == RendererGesture::DpiDisplayMove
            && kinds.contains(&ActiveTimelineActionKind::MoveToDisplay)
        {
            match event.actions.as_slice() {
                [
                    RendererTimelineAction::SetWindowSize {
                        target: size_target,
                        ..
                    },
                    RendererTimelineAction::MoveToDisplay {
                        target: display_target,
                        ..
                    },
                    RendererTimelineAction::SetRevisions {
                        target: revision_target,
                        ..
                    },
                ] if size_target == display_target && display_target == revision_target => {}
                _ => validator.error(
                    RendererScenarioValidationCode::InvalidTimeline,
                    format!("{event_path}.actions"),
                    "DPI display mutation requires exact SetWindowSize, MoveToDisplay, SetRevisions order with one identical window target",
                ),
            }
        }
        match event.phase {
            RendererTimelinePhase::Begin => {
                if position != 0
                    || event.actions.as_slice() != [RendererTimelineAction::BeginGesture]
                {
                    validator.error(RendererScenarioValidationCode::InvalidTimeline, &event_path, "Begin must be event zero with only gesture_begin");
                }
            }
            RendererTimelinePhase::End => {
                if event.at_us != workload.gesture_duration_us
                    || event.actions.as_slice() != [RendererTimelineAction::EndGesture]
                {
                    validator.error(RendererScenarioValidationCode::InvalidTimeline, &event_path, "End must occur exactly at gesture_duration_us with only gesture_end");
                }
            }
            RendererTimelinePhase::SnapBack => {
                snap_count += 1;
                facts.snap_back_event_ordinal = Some(event.event_ordinal);
                if !kinds.contains(&ActiveTimelineActionKind::SetQualityMode) {
                    validator.error(RendererScenarioValidationCode::InvalidTimeline, &event_path, "SnapBack requires an explicit Standard quality action");
                }
            }
            RendererTimelinePhase::Settle => {
                if position + 1 != scenario.timeline.len()
                    || event.at_us != workload.total_duration_us
                    || !kinds.contains(&ActiveTimelineActionKind::Settle)
                {
                    validator.error(RendererScenarioValidationCode::InvalidTimeline, &event_path, "final Settle must occur exactly at total_duration_us and contain settle");
                }
                let allowed_fancy = scenario.configured_steady_quality == RendererQualityMode::Fancy
                    && kinds.contains(&ActiveTimelineActionKind::SetQualityMode);
                if event.actions.len() != 1 + usize::from(allowed_fancy) {
                    validator.error(RendererScenarioValidationCode::InvalidTimeline, &event_path, "Settle bundle may contain only settle and the configured Standard-to-Fancy transition");
                }
            }
            RendererTimelinePhase::Mutation => {}
        }
    }
    if begin_count != 1 || end_count != 1 || settle_count != 1 {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            format!("timeline requires exactly one begin/end/settle, found {begin_count}/{end_count}/{settle_count}"),
        );
    }
    let live = gesture_is_live_resize(scenario.gesture);
    if (live && snap_count != 1) || (!live && snap_count != 0) {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            format!("gesture requires {} SnapBack event(s), found {snap_count}", usize::from(live)),
        );
    }
    if workload.resize_mutation_count != facts.resize_event_ordinals.len() as u32 {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            &path,
            format!("resize bundle count {} differs from workload {}", facts.resize_event_ordinals.len(), workload.resize_mutation_count),
        );
    }
    let expected_display_moves = usize::from(scenario.gesture == RendererGesture::DpiDisplayMove);
    if facts.display_event_ordinals.len() != expected_display_moves {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            format!(
                "gesture requires exactly {expected_display_moves} display-move event(s), found {}",
                facts.display_event_ordinals.len()
            ),
        );
    }
    facts
}

fn validate_output_and_key_schedule(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: &RendererWorkloadDefinition,
    facts: &TimelineFacts,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let expected_key_ids = workload
        .foreground_key_events
        .iter()
        .map(|event| event.key_event_id.as_str())
        .collect::<Vec<_>>();
    let actual_key_ids = facts
        .key_actions
        .iter()
        .map(|(_, id)| id.as_str())
        .collect::<Vec<_>>();
    if actual_key_ids != expected_key_ids {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            format!("{scenario_path}.timeline"),
            format!("foreground-key actions must equal workload key definitions {expected_key_ids:?}"),
        );
    }
    let focused_pane_id = scenario
        .visual_checkpoints
        .iter()
        .find(|checkpoint| {
            checkpoint.overlay_id == RendererCoverageOverlayId::ProductionDefault
        })
        .and_then(|checkpoint| index.phase_manifests.get(checkpoint.phase_manifest_id.as_str()))
        .and_then(|manifest| {
            index
                .layout_profiles
                .get(manifest.layout_profile_id.as_str())
                .and_then(|layout| {
                    expand_layout(layout)
                        .pane_ids
                        .get(usize::from(manifest.focused_pane_ordinal))
                        .cloned()
                })
        });
    for (position, key_event) in workload.foreground_key_events.iter().enumerate() {
        if focused_pane_id.as_deref() != Some(key_event.target_pane_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{scenario_path}.workload_id.foreground_key_events[{position}].target_pane_id"),
                format!(
                    "foreground key target must equal focused pane identity {:?}",
                    focused_pane_id
                ),
            );
        }
    }
    if scenario.gesture != RendererGesture::OutputOverlapResize {
        if workload.output_stream.is_some()
            || !workload.foreground_key_events.is_empty()
            || !facts.output_actions.is_empty()
            || !facts.key_actions.is_empty()
        {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                scenario_path,
                "non-output-overlap scenarios must have no output stream, output-rate action, or foreground key",
            );
        }
        if scenario.output_overlap_resize_mode.is_some() {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureTransition,
                format!("{scenario_path}.output_overlap_resize_mode"),
                "resize mode is reserved for output-overlap scenarios",
            );
        }
        return;
    }
    let Some(stream) = &workload.output_stream else {
        validator.error(
            RendererScenarioValidationCode::InvalidOutputOverlapRate,
            format!("{scenario_path}.workload_id"),
            "output-overlap workload requires a pinned output stream",
        );
        return;
    };
    if stream.aggregate_bytes_per_second != OUTPUT_OVERLAP_BYTES_PER_SECOND {
        validator.error(
            RendererScenarioValidationCode::InvalidOutputOverlapRate,
            format!("{scenario_path}.workload_id"),
            format!("aggregate output must equal {OUTPUT_OVERLAP_BYTES_PER_SECOND} bytes/s"),
        );
    }
    if scenario.output_overlap_resize_mode.is_none() {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            format!("{scenario_path}.output_overlap_resize_mode"),
            "output-overlap scenario requires explicit resize mode",
        );
    }
    let mut active = false;
    let mut saw_resize_while_active = false;
    let mut saw_key_while_active = false;
    let mut stopped_at = None;
    for event in &scenario.timeline {
        for action in &event.actions {
            match action {
                RendererTimelineAction::SetOutputRate {
                    stream_id,
                    bytes_per_second,
                } => {
                    if stream_id != &stream.stream_id {
                        validator.error(
                            RendererScenarioValidationCode::InvalidWorkload,
                            format!("{scenario_path}.timeline[{}]", event.event_ordinal),
                            format!("output action must name stream `{}`", stream.stream_id),
                        );
                    }
                    if *bytes_per_second == 0 {
                        if !active {
                            validator.error(RendererScenarioValidationCode::InvalidTimeline, scenario_path, "output stop occurs while stream is inactive");
                        }
                        active = false;
                        stopped_at = Some(event.event_ordinal);
                    } else {
                        if *bytes_per_second != OUTPUT_OVERLAP_BYTES_PER_SECOND || active {
                            validator.error(RendererScenarioValidationCode::InvalidOutputOverlapRate, scenario_path, "output must start once at exactly one decimal MB/s");
                        }
                        active = true;
                    }
                }
                RendererTimelineAction::SetWindowSize { .. }
                | RendererTimelineAction::SetGrid { .. } => {
                    if stopped_at.is_some() {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, scenario_path, "resize occurs after explicit output stop");
                    }
                    saw_resize_while_active |= active;
                }
                RendererTimelineAction::ForegroundKey { .. } => {
                    if stopped_at.is_some() {
                        validator.error(RendererScenarioValidationCode::InvalidTimeline, scenario_path, "foreground key occurs after explicit output stop");
                    }
                    saw_key_while_active |= active;
                }
                _ => {}
            }
        }
    }
    if active || stopped_at.is_none() || !saw_resize_while_active || !saw_key_while_active {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            scenario_path,
            "output must start nonzero, overlap resize and foreground key, stop at zero, and remain stopped",
        );
    }
    if scenario.fleet_point == RendererFleetPoint::P050
        && workload.foreground_key_events.len() != 1
    {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{scenario_path}.workload_id"),
            "p050 output-overlap RQ-S6 superset requires exactly one pinned key event",
        );
    }
    match scenario.output_overlap_resize_mode {
        Some(RendererResizeMode::SameGrid) if !facts.grid_event_ordinals.is_empty() => validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            format!("{scenario_path}.timeline"),
            "same-grid output mode forbids grid changes",
        ),
        Some(RendererResizeMode::GridChanging) => {
            let distinct_grids = scenario
                .timeline
                .iter()
                .flat_map(|event| &event.actions)
                .filter_map(|action| match action {
                    RendererTimelineAction::SetGrid { columns, rows, .. } => Some((*columns, *rows)),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            if distinct_grids.len() < 2 {
                validator.error(
                    RendererScenarioValidationCode::InvalidGestureTransition,
                    format!("{scenario_path}.timeline"),
                    "grid-changing output mode requires at least two distinct grid-boundary bundles",
                );
            }
        }
        Some(RendererResizeMode::SameGrid) | None => {}
    }
}

struct ResolvedScenarioOverlay<'a> {
    profile: &'a RendererCoverageOverlayProfile,
    checkpoints: Vec<&'a RendererVisualCheckpoint>,
    anchors: Vec<&'a RendererPhaseManifest>,
}

fn resolve_scenario_overlays<'a>(
    scenario_path: &str,
    scenario: &'a RendererScenarioDefinition,
    index: &'a CatalogIndex<'a>,
    validator: &mut Validator,
) -> BTreeMap<RendererCoverageOverlayId, ResolvedScenarioOverlay<'a>> {
    if scenario.coverage_overlay_profile_ids.len() != RendererCoverageOverlayId::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::MissingRequiredCoverage,
            format!("{scenario_path}.coverage_overlay_profile_ids"),
            format!(
                "every scenario requires exactly eight overlay profiles, found {}",
                scenario.coverage_overlay_profile_ids.len()
            ),
        );
    }
    let mut resolved = BTreeMap::new();
    let workload = index.workloads.get(scenario.workload_id.as_str()).copied();
    for (position, overlay_profile_id) in
        scenario.coverage_overlay_profile_ids.iter().enumerate()
    {
        let path = format!("{scenario_path}.coverage_overlay_profile_ids[{position}]");
        validator.require_identifier(&path, overlay_profile_id);
        let Some(profile) = index
            .coverage_overlay_profiles
            .get(overlay_profile_id.as_str())
            .copied()
        else {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                &path,
                format!("undefined overlay profile `{overlay_profile_id}`"),
            );
            continue;
        };
        let expected_overlay = RendererCoverageOverlayId::ALL.get(position).copied();
        if expected_overlay != Some(profile.overlay_id)
            || expected_overlay
                .map(expected_overlay_profile_id)
                .as_deref()
                != Some(overlay_profile_id.as_str())
        {
            validator.error(
                RendererScenarioValidationCode::MissingRequiredCoverage,
                &path,
                "scenario overlay profile IDs must equal the canonical eight IDs in order",
            );
        }
        if resolved.contains_key(&profile.overlay_id) {
            validator.error(
                RendererScenarioValidationCode::DuplicateCoverageCell,
                &path,
                format!("duplicate scenario overlay `{}`", profile.overlay_id.as_str()),
            );
            continue;
        }

        let checkpoints = scenario
            .visual_checkpoints
            .iter()
            .enumerate()
            .filter(|(_, checkpoint)| checkpoint.overlay_id == profile.overlay_id)
            .collect::<Vec<_>>();
        let expected_checkpoint_count = if gesture_is_live_resize(scenario.gesture) {
            4
        } else {
            3
        };
        if checkpoints.len() != expected_checkpoint_count {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{scenario_path}.visual_checkpoints"),
                format!(
                    "overlay `{}` requires exactly {expected_checkpoint_count} checkpoints, found {}",
                    profile.overlay_id.as_str(),
                    checkpoints.len()
                ),
            );
        }

        let mut anchors = Vec::new();
        let mut anchor_ids = BTreeSet::new();
        let mut previous_ordinal = None;
        for (checkpoint_position, checkpoint) in &checkpoints {
            let checkpoint_path =
                format!("{scenario_path}.visual_checkpoints[{checkpoint_position}]");
            let manifest_id = checkpoint.phase_manifest_id.as_str();
            validator.require_identifier(
                &format!("{checkpoint_path}.phase_manifest_id"),
                manifest_id,
            );
            if !anchor_ids.insert(manifest_id) {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{checkpoint_path}.phase_manifest_id"),
                    format!("duplicate overlay anchor `{manifest_id}`"),
                );
            }
            let Some(manifest) = index.phase_manifests.get(manifest_id).copied() else {
                validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    format!("{checkpoint_path}.phase_manifest_id"),
                    format!("undefined phase manifest `{manifest_id}`"),
                );
                continue;
            };
            if manifest.overlay_id != profile.overlay_id {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{checkpoint_path}.phase_manifest_id"),
                    "anchor manifest overlay identity differs from reusable profile",
                );
            }
            if checkpoint.phase != manifest.phase
                || checkpoint.event_ordinal != manifest.event_ordinal
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    &checkpoint_path,
                    "checkpoint phase/event must exactly equal its phase manifest",
                );
            }
            if previous_ordinal.is_some_and(|previous| manifest.event_ordinal <= previous) {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &checkpoint_path,
                    "anchor event ordinals must be strictly increasing",
                );
            }
            previous_ordinal = Some(manifest.event_ordinal);
            match scenario.timeline.get(manifest.event_ordinal as usize) {
                Some(event) if event.phase == manifest.phase => {}
                Some(_) => validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &checkpoint_path,
                    "anchor phase differs from the bound timeline event",
                ),
                None => validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &checkpoint_path,
                    "anchor event ordinal is outside the scenario timeline",
                ),
            }
            if let Some(layout) = index.layout_profiles.get(manifest.layout_profile_id.as_str())
                && layout.fleet_point != scenario.fleet_point
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &checkpoint_path,
                    "anchor layout fleet point differs from scenario",
                );
            }
            if let Some(workload) = workload {
                match expand_manifest_state(
                    manifest,
                    index,
                    scenario,
                    checkpoint,
                    workload,
                ) {
                    Ok(expanded) => {
                        let expected =
                            canonical_feature_set(expected_overlay_features(profile.overlay_id));
                        if expanded.features != expected {
                            validator.error(
                                RendererScenarioValidationCode::MissingTerminalFeatureCoverage,
                                &checkpoint_path,
                                format!(
                                    "overlay `{}` derives {:?}, expected exact {:?}",
                                    profile.overlay_id.as_str(), expanded.features, expected
                                ),
                            );
                        }
                        for state in &expanded.surfaces {
                            if state.renderer_config_profile_id
                                != profile.renderer_config_profile_id
                            {
                                validator.error(
                                    RendererScenarioValidationCode::InvalidState,
                                    &checkpoint_path,
                                    format!(
                                        "surface must select overlay config `{}`",
                                        profile.renderer_config_profile_id
                                    ),
                                );
                            }
                        }
                    }
                    Err(detail) => validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        &checkpoint_path,
                        detail,
                    ),
                }
            }
            anchors.push(manifest);
        }
        let first_valid = anchors.first().is_some_and(|manifest| {
            manifest.phase == RendererTimelinePhase::Begin && manifest.event_ordinal == 0
        });
        let last_ordinal = scenario.timeline.last().map(|event| event.event_ordinal);
        let last_valid = anchors.last().is_some_and(|manifest| {
            manifest.phase == RendererTimelinePhase::Settle
                && Some(manifest.event_ordinal) == last_ordinal
        });
        if !first_valid || !last_valid {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{scenario_path}.visual_checkpoints"),
                "overlay anchors must start at Begin/0 and end at the final Settle event",
            );
        }

        let roles = checkpoints
            .iter()
            .map(|(_, checkpoint)| (checkpoint.role, checkpoint.phase))
            .collect::<Vec<_>>();
        let exact_roles = if gesture_is_live_resize(scenario.gesture) {
            roles.as_slice()
                == [
                    (
                        RendererCheckpointRole::InitialBaseline,
                        RendererTimelinePhase::Begin,
                    ),
                    (
                        RendererCheckpointRole::LastDraftProvenance,
                        RendererTimelinePhase::Mutation,
                    ),
                    (
                        RendererCheckpointRole::StandardSnapBackSubject,
                        RendererTimelinePhase::SnapBack,
                    ),
                    (
                        RendererCheckpointRole::FinalSteadyState,
                        RendererTimelinePhase::Settle,
                    ),
                ]
        } else {
            roles.as_slice()
                == [
                    (
                        RendererCheckpointRole::InitialBaseline,
                        RendererTimelinePhase::Begin,
                    ),
                    (
                        RendererCheckpointRole::Intermediate,
                        RendererTimelinePhase::Mutation,
                    ),
                    (
                        RendererCheckpointRole::FinalSteadyState,
                        RendererTimelinePhase::Settle,
                    ),
                ]
        };
        if !exact_roles {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{scenario_path}.visual_checkpoints"),
                format!(
                    "overlay `{}` checkpoints do not match the exact role/phase sequence",
                    profile.overlay_id.as_str()
                ),
            );
        }
        if let Some(first) = anchors.first() {
            for anchor in anchors.iter().skip(1) {
                if anchor.layout_profile_id != first.layout_profile_id
                    || anchor.focused_window_ordinal != first.focused_window_ordinal
                    || anchor.focused_pane_ordinal != first.focused_pane_ordinal
                    || anchor.content_distribution_profile_id
                        != first.content_distribution_profile_id
                    || anchor
                        .window_states
                        .iter()
                        .map(|state| state.active_tab_ordinal)
                        .collect::<Vec<_>>()
                        != first
                            .window_states
                            .iter()
                            .map(|state| state.active_tab_ordinal)
                            .collect::<Vec<_>>()
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidGestureTransition,
                        &path,
                        "primary gesture cannot mutate topology, focus, active-tab order, layout, or selected content overlay",
                    );
                }
            }
        }
        resolved.insert(
            profile.overlay_id,
            ResolvedScenarioOverlay {
                profile,
                checkpoints: checkpoints
                    .iter()
                    .map(|(_, checkpoint)| *checkpoint)
                    .collect(),
                anchors,
            },
        );
    }
    resolved
}

fn expanded_output_rates(workload: &RendererWorkloadDefinition) -> Vec<u64> {
    let Some(stream) = &workload.output_stream else {
        return vec![0; usize::from(workload.pane_count)];
    };
    if workload.pane_count == 0 {
        return Vec::new();
    }
    let base = stream.aggregate_bytes_per_second / u64::from(workload.pane_count);
    let remainder = stream.aggregate_bytes_per_second % u64::from(workload.pane_count);
    let mut rates = (0..workload.pane_count)
        .map(|ordinal| base + u64::from(u64::from(ordinal) < remainder))
        .collect::<Vec<_>>();
    for rate_override in &stream.rate_overrides {
        let ordinals = match &rate_override.selector {
            RendererPaneOrdinalSelector::All => (0..workload.pane_count).collect::<Vec<_>>(),
            RendererPaneOrdinalSelector::OrdinalRange {
                start,
                end_exclusive,
            } => (*start..(*end_exclusive).min(workload.pane_count)).collect(),
            RendererPaneOrdinalSelector::Explicit { ordinals } => ordinals.clone(),
        };
        for ordinal in ordinals {
            if let Some(rate) = rates.get_mut(usize::from(ordinal)) {
                *rate = rate_override.bytes_per_second;
            }
        }
    }
    rates
}

fn apply_timeline_action(
    path: &str,
    action: &RendererTimelineAction,
    state: &mut ExpandedManifestState,
    workload: &RendererWorkloadDefinition,
    layout: &RendererLayoutProfile,
    validator: &mut Validator,
) {
    let kind = active_timeline_action_kind(action);
    let target_ordinals = timeline_action_target(action)
        .map(|target| validate_mutation_target(&format!("{path}.target"), target, kind, layout, validator))
        .unwrap_or_default();
    match action {
        RendererTimelineAction::BeginGesture
        | RendererTimelineAction::ForegroundKey { .. }
        | RendererTimelineAction::EndGesture
        | RendererTimelineAction::Settle => {}
        RendererTimelineAction::SetWindowSize {
            target,
            width_px,
            height_px,
            ..
        } => {
            let expanded = expand_layout(layout);
            if let Some(window_ordinal) = expanded
                .window_ids
                .iter()
                .position(|window_id| window_id == &target.window_id)
                && let Some(window_state) = state.window_states.get_mut(window_ordinal)
            {
                window_state.window_rect.width = *width_px;
                window_state.window_rect.height = *height_px;
            }
            refresh_expanded_geometry(path, state, layout, validator);
        }
        RendererTimelineAction::SetGrid { columns, rows, .. } => {
            for ordinal in target_ordinals {
                if let Some(surface) = state.surfaces.get_mut(usize::from(ordinal)) {
                    surface.grid = RendererGridState {
                        columns: *columns,
                        rows: *rows,
                    };
                    if let Err(detail) = refresh_surface_padding_and_geometry(surface) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            path,
                            detail,
                        );
                    }
                }
            }
        }
        RendererTimelineAction::SetFontScale { scale_milli, .. } => {
            for ordinal in target_ordinals {
                if let Some(surface) = state.surfaces.get_mut(usize::from(ordinal)) {
                    surface.font.scale_milli = *scale_milli;
                    if let Err(detail) = refresh_surface_padding_and_geometry(surface) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            path,
                            detail,
                        );
                    }
                }
            }
        }
        RendererTimelineAction::SetQualityMode { mode, .. } => {
            for ordinal in target_ordinals {
                if let Some(surface) = state.surfaces.get_mut(usize::from(ordinal)) {
                    surface.quality_mode = *mode;
                }
            }
        }
        RendererTimelineAction::MoveToDisplay {
            display,
            ..
        } => {
            for ordinal in target_ordinals {
                if let Some(surface) = state.surfaces.get_mut(usize::from(ordinal)) {
                    surface.display.display_id.clone_from(&display.display_id);
                    surface.display.dpi_milli = display.dpi_milli;
                    surface.display.scale_factor_milli = display.scale_factor_milli;
                    surface
                        .display
                        .color_space_id
                        .clone_from(&display.color_space_id);
                    surface
                        .display
                        .color_profile_ref
                        .clone_from(&display.color_profile_ref);
                    surface.display.dynamic_range_mode = display.dynamic_range_mode;
                    surface.display.edr_available = display.edr_available;
                    surface.display.edr_headroom_milli = display.edr_headroom_milli;
                    if let Err(detail) = refresh_surface_padding_and_geometry(surface) {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            path,
                            detail,
                        );
                    }
                }
            }
        }
        RendererTimelineAction::SetOutputRate {
            stream_id,
            bytes_per_second,
        } => {
            let rates = if *bytes_per_second == 0 {
                vec![0; state.outputs.len()]
            } else {
                expanded_output_rates(workload)
            };
            for (ordinal, output) in state.outputs.iter_mut().enumerate() {
                let rate = rates.get(ordinal).copied().unwrap_or_default();
                output.bytes_per_second = rate;
                output.stream_id = (rate != 0).then(|| stream_id.clone());
            }
        }
        RendererTimelineAction::SetRevisions {
            renderer_generation,
            grid_revision,
            terminal_revision,
            ..
        } => {
            for ordinal in target_ordinals {
                if let Some(surface) = state.surfaces.get_mut(usize::from(ordinal)) {
                    if *renderer_generation <= surface.renderer_generation
                        || *grid_revision <= surface.grid_revision
                        || *terminal_revision <= surface.terminal_revision
                    {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            path,
                            "explicit renderer/grid/terminal revisions must advance monotonically",
                        );
                    }
                    surface.renderer_generation = *renderer_generation;
                    surface.grid_revision = *grid_revision;
                    surface.terminal_revision = *terminal_revision;
                }
            }
        }
    }
}

fn refresh_expanded_geometry(
    path: &str,
    state: &mut ExpandedManifestState,
    layout: &RendererLayoutProfile,
    validator: &mut Validator,
) {
    let geometry = match expand_pane_geometry_from_window_states(layout, &state.window_states) {
        Ok(geometry) => geometry,
        Err(detail) => {
            validator.error(RendererScenarioValidationCode::InvalidState, path, detail);
            return;
        }
    };
    for (surface, pane_geometry) in state.surfaces.iter_mut().zip(&geometry) {
        surface.display.viewport_width_px = pane_geometry.rect.width;
        surface.display.viewport_height_px = pane_geometry.rect.height;
        if let Err(detail) = refresh_surface_padding_and_geometry(surface) {
            validator.error(RendererScenarioValidationCode::InvalidState, path, detail);
        }
    }
    state.pane_geometry = geometry;
}

fn apply_reached_materialization_to_replay(
    path: &str,
    replayed: &mut ExpandedManifestState,
    expected: &ExpandedManifestState,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    if replayed.surfaces.len() != expected.surfaces.len()
        || replayed.applied_materialization_steps.len()
            != expected.applied_materialization_steps.len()
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "materialization replay pane cardinality differs from the anchor",
        );
        return;
    }
    for pane_position in 0..replayed.surfaces.len() {
        let current_steps = &replayed.applied_materialization_steps[pane_position];
        let expected_steps = &expected.applied_materialization_steps[pane_position];
        if !expected_steps.starts_with(current_steps) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                path,
                format!(
                    "pane {pane_position} materialization history is not a persistent prefix"
                ),
            );
            continue;
        }
        let newly_reached = &expected_steps[current_steps.len()..];
        let mut changes_buffer_content = false;
        for step in newly_reached {
            match step.operation {
                RendererContentCompositionOperation::ReplaceActiveBuffer
                | RendererContentCompositionOperation::AppendToActiveBuffer
                | RendererContentCompositionOperation::EnterAlternateBuffer
                | RendererContentCompositionOperation::ExitAlternateBuffer => {
                    changes_buffer_content = true;
                }
                RendererContentCompositionOperation::ApplyTypedStateOverlay => {
                    let Some(reference) = index.content.get(step.content_corpus_id.as_str())
                    else {
                        validator.error(
                            RendererScenarioValidationCode::DanglingReference,
                            path,
                            format!(
                                "typed materialization references undefined content `{}`",
                                step.content_corpus_id
                            ),
                        );
                        continue;
                    };
                    if content_identity_framing(reference)
                        != RendererContentFraming::TypedStateOverlay
                    {
                        validator.error(
                            RendererScenarioValidationCode::InvalidState,
                            path,
                            "apply_typed_state_overlay must resolve a typed-state fixture",
                        );
                    }
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        path,
                        "typed-state materialization reached after Begin; v1 requires every typed fixture before the gesture",
                    );
                }
            }
        }
        if changes_buffer_content {
            replayed.surfaces[pane_position].terminal.active_buffer =
                expected.surfaces[pane_position].terminal.active_buffer;
            replayed.surfaces[pane_position]
                .terminal
                .primary_buffer
                .content_corpus_ids
                .clone_from(
                    &expected.surfaces[pane_position]
                        .terminal
                        .primary_buffer
                        .content_corpus_ids,
                );
            replayed.surfaces[pane_position]
                .terminal
                .alternate_buffer
                .content_corpus_ids
                .clone_from(
                    &expected.surfaces[pane_position]
                        .terminal
                        .alternate_buffer
                        .content_corpus_ids,
                );
        }
        replayed.applied_materialization_steps[pane_position].clone_from(expected_steps);
    }
    replayed.features.clone_from(&expected.features);
}

fn validate_overlay_replay(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: &RendererWorkloadDefinition,
    overlay: &ResolvedScenarioOverlay<'_>,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let Some(initial_manifest) = overlay.anchors.first() else {
        return;
    };
    let Some(initial_checkpoint) = overlay.checkpoints.first() else {
        return;
    };
    let Some(layout) = index
        .layout_profiles
        .get(initial_manifest.layout_profile_id.as_str())
        .copied()
    else {
        return;
    };
    let Ok(mut replayed) = expand_manifest_state(
        initial_manifest,
        index,
        scenario,
        initial_checkpoint,
        workload,
    ) else {
        return;
    };
    let anchors = overlay
        .anchors
        .iter()
        .zip(&overlay.checkpoints)
        .map(|(manifest, checkpoint)| (manifest.event_ordinal, (*manifest, *checkpoint)))
        .collect::<BTreeMap<_, _>>();
    for event in &scenario.timeline {
        for (action_position, action) in event.actions.iter().enumerate() {
            apply_timeline_action(
                &format!(
                    "{scenario_path}.timeline[{}].actions[{action_position}]",
                    event.event_ordinal
                ),
                action,
                &mut replayed,
                workload,
                layout,
                validator,
            );
        }
        if let Some((anchor, checkpoint)) = anchors.get(&event.event_ordinal)
            && let Ok(expected) =
                expand_manifest_state(anchor, index, scenario, checkpoint, workload)
        {
            apply_reached_materialization_to_replay(
                &format!(
                    "{scenario_path}.visual_checkpoints.{}",
                    overlay.profile.overlay_id.as_str()
                ),
                &mut replayed,
                &expected,
                index,
                validator,
            );
            if replayed.surfaces != expected.surfaces
                || replayed.outputs != expected.outputs
                || replayed.window_states != expected.window_states
                || replayed.pane_geometry != expected.pane_geometry
                || replayed.applied_materialization_steps
                    != expected.applied_materialization_steps
                || replayed.features != expected.features
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!(
                        "{scenario_path}.visual_checkpoints.{}",
                        overlay.profile.overlay_id.as_str()
                    ),
                    format!(
                        "replayed actions and reached materialization diverge from the overlay anchor at event {}",
                        event.event_ordinal
                    ),
                );
            }
        }
    }
}

fn validate_overlay_gesture_transition(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    overlay: &ResolvedScenarioOverlay<'_>,
    index: &CatalogIndex<'_>,
    validator: &mut Validator,
) {
    let (Some(initial), Some(final_manifest)) = (overlay.anchors.first(), overlay.anchors.last()) else {
        return;
    };
    let (Some(initial_checkpoint), Some(final_checkpoint)) =
        (overlay.checkpoints.first(), overlay.checkpoints.last())
    else {
        return;
    };
    let Some(workload) = index.workloads.get(scenario.workload_id.as_str()).copied() else {
        return;
    };
    let (Ok(initial), Ok(final_state)) = (
        expand_manifest_state(initial, index, scenario, initial_checkpoint, workload),
        expand_manifest_state(
            final_manifest,
            index,
            scenario,
            final_checkpoint,
            workload,
        ),
    ) else {
        return;
    };
    if initial.surfaces.len() != final_state.surfaces.len() {
        return;
    }
    let pairs = initial.surfaces.iter().zip(&final_state.surfaces).collect::<Vec<_>>();
    let font_identity_stable = pairs.iter().all(|(before, after)| {
        before.font.font_id == after.font.font_id
            && before.font.pinned_font_ref == after.font.pinned_font_ref
            && before.font.base_size_milli_points == after.font.base_size_milli_points
            && before.font.base_cell_width_milli_px == after.font.base_cell_width_milli_px
            && before.font.base_cell_height_milli_px == after.font.base_cell_height_milli_px
            && before.font.metric_reference_dpi_milli
                == after.font.metric_reference_dpi_milli
            && before.font.metric_derivation_revision == after.font.metric_derivation_revision
    });
    let terminal_stable = pairs.iter().all(|(before, after)| {
        terminal_without_derived_geometry(&before.terminal)
            == terminal_without_derived_geometry(&after.terminal)
    });
    if !font_identity_stable || !terminal_stable {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            scenario_path,
            "primary gesture cannot mutate base font identity/size or overlay terminal state",
        );
    }
    let any_viewport_change = pairs.iter().any(|(before, after)| {
        before.display.viewport_width_px != after.display.viewport_width_px
            || before.display.viewport_height_px != after.display.viewport_height_px
    });
    let any_grid_change = pairs.iter().any(|(before, after)| before.grid != after.grid);
    let valid = match scenario.gesture {
        RendererGesture::SameGridDrag => {
            pairs.iter().all(|(before, after)| before.grid == after.grid) && any_viewport_change
        }
        RendererGesture::GridChangingDrag => any_grid_change && any_viewport_change,
        RendererGesture::Reflow80To200 => pairs.iter().any(|(before, after)| {
            before.grid.columns == 80 && after.grid.columns == 200
        }),
        RendererGesture::Reflow200To80 => pairs.iter().any(|(before, after)| {
            before.grid.columns == 200 && after.grid.columns == 80
        }),
        RendererGesture::ZoomIn => {
            pairs.iter().all(|(before, after)| {
                before.display.display_id == after.display.display_id
                    && before.display.dpi_milli == after.display.dpi_milli
                    && before.display.scale_factor_milli
                        == after.display.scale_factor_milli
                    && before.display.color_space_id == after.display.color_space_id
                    && before.display.color_profile_ref == after.display.color_profile_ref
                    && before.display.dynamic_range_mode
                        == after.display.dynamic_range_mode
                    && before.display.viewport_width_px
                        == after.display.viewport_width_px
                    && before.display.viewport_height_px
                        == after.display.viewport_height_px
            })
                && pairs
                    .iter()
                    .any(|(before, after)| after.font.scale_milli > before.font.scale_milli)
        }
        RendererGesture::ZoomOut => {
            pairs.iter().all(|(before, after)| {
                before.display.display_id == after.display.display_id
                    && before.display.dpi_milli == after.display.dpi_milli
                    && before.display.scale_factor_milli
                        == after.display.scale_factor_milli
                    && before.display.color_space_id == after.display.color_space_id
                    && before.display.color_profile_ref == after.display.color_profile_ref
                    && before.display.dynamic_range_mode
                        == after.display.dynamic_range_mode
                    && before.display.viewport_width_px
                        == after.display.viewport_width_px
                    && before.display.viewport_height_px
                        == after.display.viewport_height_px
            })
                && pairs
                    .iter()
                    .any(|(before, after)| after.font.scale_milli < before.font.scale_milli)
        }
        RendererGesture::DpiDisplayMove => pairs.iter().any(|(before, after)| {
            before.display.display_id != after.display.display_id
                && (before.display.dpi_milli != after.display.dpi_milli
                    || before.display.scale_factor_milli
                        != after.display.scale_factor_milli)
        }),
        RendererGesture::OutputOverlapResize => match scenario.output_overlap_resize_mode {
            Some(RendererResizeMode::SameGrid) => {
                pairs.iter().all(|(before, after)| before.grid == after.grid)
                    && any_viewport_change
            }
            Some(RendererResizeMode::GridChanging) => any_grid_change && any_viewport_change,
            None => false,
        },
    };
    if !valid {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            scenario_path,
            format!("overlay does not implement `{}` transition semantics", scenario.gesture.as_str()),
        );
    }
}

const RENDERER_LAYOUT_STABLE_ID_REVISION: &str = "balanced_contiguous_v1.ids.v1";
const PRODUCTION_DEFAULT_CONFIG_ID: &str = "renderer.config.production_default";
const LIGATURE_ENABLED_CONFIG_ID: &str = "renderer.config.ligature_enabled";

#[derive(Debug, Clone)]
struct ExpandedLayout {
    window_ids: Vec<String>,
    tab_ids: Vec<String>,
    pane_ids: Vec<String>,
    tab_window_ordinals: Vec<u16>,
    pane_tab_ordinals: Vec<u16>,
    pane_window_ordinals: Vec<u16>,
    tabs_by_window: Vec<Vec<u16>>,
}

fn balanced_owner_map(item_count: u16, bucket_count: u16) -> Vec<u16> {
    if bucket_count == 0 {
        return Vec::new();
    }
    let base = item_count / bucket_count;
    let remainder = item_count % bucket_count;
    let mut owners = Vec::with_capacity(usize::from(item_count));
    for bucket in 0..bucket_count {
        let count = base + u16::from(bucket < remainder);
        owners.extend(std::iter::repeat_n(bucket, usize::from(count)));
    }
    owners
}

fn expand_layout(profile: &RendererLayoutProfile) -> ExpandedLayout {
    if profile.window_count == 0 || profile.tab_count == 0 || profile.pane_count == 0 {
        return ExpandedLayout {
            window_ids: Vec::new(),
            tab_ids: Vec::new(),
            pane_ids: Vec::new(),
            tab_window_ordinals: Vec::new(),
            pane_tab_ordinals: Vec::new(),
            pane_window_ordinals: Vec::new(),
            tabs_by_window: Vec::new(),
        };
    }
    let tab_window_ordinals = balanced_owner_map(profile.tab_count, profile.window_count);
    let pane_tab_ordinals = balanced_owner_map(profile.pane_count, profile.tab_count);
    let pane_window_ordinals = pane_tab_ordinals
        .iter()
        .map(|tab| tab_window_ordinals[usize::from(*tab)])
        .collect::<Vec<_>>();
    let mut tabs_by_window = vec![Vec::new(); usize::from(profile.window_count)];
    for (tab, window) in tab_window_ordinals.iter().copied().enumerate() {
        tabs_by_window[usize::from(window)].push(tab as u16);
    }
    ExpandedLayout {
        window_ids: (0..profile.window_count)
            .map(|ordinal| format!("window-{ordinal:03}"))
            .collect(),
        tab_ids: (0..profile.tab_count)
            .map(|ordinal| format!("tab-{ordinal:03}"))
            .collect(),
        pane_ids: (0..profile.pane_count)
            .map(|ordinal| format!("pane-{ordinal:03}"))
            .collect(),
        tab_window_ordinals,
        pane_tab_ordinals,
        pane_window_ordinals,
        tabs_by_window,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedPaneGeometry {
    rect: RendererPixelRect,
    split_path: Vec<RendererSplitTreeBranch>,
}

const fn alternate_split_direction(
    direction: RendererSplitDirection,
) -> RendererSplitDirection {
    match direction {
        RendererSplitDirection::Horizontal => RendererSplitDirection::Vertical,
        RendererSplitDirection::Vertical => RendererSplitDirection::Horizontal,
    }
}

fn split_rect(
    rect: RendererPixelRect,
    direction: RendererSplitDirection,
    ratio_milli: u16,
) -> Option<(RendererPixelRect, RendererPixelRect)> {
    let ratio = u64::from(ratio_milli);
    match direction {
        RendererSplitDirection::Horizontal => {
            if rect.width < 2 {
                return None;
            }
            let first_width = u32::try_from(
                (u64::from(rect.width) * ratio).div_ceil(1_000),
            )
            .ok()?;
            if first_width == 0 || first_width >= rect.width {
                return None;
            }
            let second_x = i64::from(rect.x) + i64::from(first_width);
            Some((
                RendererPixelRect {
                    width: first_width,
                    ..rect
                },
                RendererPixelRect {
                    x: i32::try_from(second_x).ok()?,
                    width: rect.width - first_width,
                    ..rect
                },
            ))
        }
        RendererSplitDirection::Vertical => {
            if rect.height < 2 {
                return None;
            }
            let first_height = u32::try_from(
                (u64::from(rect.height) * ratio).div_ceil(1_000),
            )
            .ok()?;
            if first_height == 0 || first_height >= rect.height {
                return None;
            }
            let second_y = i64::from(rect.y) + i64::from(first_height);
            Some((
                RendererPixelRect {
                    height: first_height,
                    ..rect
                },
                RendererPixelRect {
                    y: i32::try_from(second_y).ok()?,
                    height: rect.height - first_height,
                    ..rect
                },
            ))
        }
    }
}

fn expand_split_subtree(
    pane_ordinals: &[u16],
    rect: RendererPixelRect,
    direction: RendererSplitDirection,
    profile: &RendererLayoutProfile,
    path: &mut Vec<RendererSplitTreeBranch>,
    output: &mut [Option<ExpandedPaneGeometry>],
) -> Result<(), String> {
    if pane_ordinals.is_empty() {
        return Err("split subtree cannot be empty".to_string());
    }
    if pane_ordinals.len() == 1 {
        output[usize::from(pane_ordinals[0])] = Some(ExpandedPaneGeometry {
            rect,
            split_path: path.clone(),
        });
        return Ok(());
    }
    let first_count = if profile.lowest_ordinal_gets_remainder {
        pane_ordinals.len().div_ceil(2)
    } else {
        pane_ordinals.len() / 2
    };
    if first_count == 0 || first_count == pane_ordinals.len() {
        return Err("balanced split did not partition pane ordinals".to_string());
    }
    let Some((first_rect, second_rect)) =
        split_rect(rect, direction, profile.split_ratio_milli)
    else {
        return Err(format!(
            "drawable region {}x{} is too small for balanced split",
            rect.width, rect.height
        ));
    };
    let next_direction = if profile.alternate_split_direction {
        alternate_split_direction(direction)
    } else {
        direction
    };
    path.push(RendererSplitTreeBranch::First);
    expand_split_subtree(
        &pane_ordinals[..first_count],
        first_rect,
        next_direction,
        profile,
        path,
        output,
    )?;
    path.pop();
    path.push(RendererSplitTreeBranch::Second);
    expand_split_subtree(
        &pane_ordinals[first_count..],
        second_rect,
        next_direction,
        profile,
        path,
        output,
    )?;
    path.pop();
    Ok(())
}

fn expand_pane_geometry_from_window_states(
    profile: &RendererLayoutProfile,
    window_states: &[RendererPhaseWindowState],
) -> Result<Vec<ExpandedPaneGeometry>, String> {
    let expanded = expand_layout(profile);
    if window_states.len() != usize::from(profile.window_count) {
        return Err("manifest does not carry one drawable region per window".to_string());
    }
    let mut output = vec![None; usize::from(profile.pane_count)];
    for tab_ordinal in 0..profile.tab_count {
        let pane_ordinals = expanded
            .pane_tab_ordinals
            .iter()
            .enumerate()
            .filter_map(|(pane, owner)| (*owner == tab_ordinal).then_some(pane as u16))
            .collect::<Vec<_>>();
        if pane_ordinals.is_empty() {
            return Err(format!("tab {tab_ordinal} owns no panes"));
        }
        let window_ordinal = expanded.tab_window_ordinals[usize::from(tab_ordinal)];
        let window_rect = window_states[usize::from(window_ordinal)].window_rect;
        expand_split_subtree(
            &pane_ordinals,
            window_rect,
            profile.initial_split_direction,
            profile,
            &mut Vec::new(),
            &mut output,
        )?;
    }
    output
        .into_iter()
        .enumerate()
        .map(|(pane, geometry)| {
            geometry.ok_or_else(|| format!("pane {pane} was omitted from split expansion"))
        })
        .collect()
}

fn expand_manifest_pane_geometry(
    profile: &RendererLayoutProfile,
    manifest: &RendererPhaseManifest,
) -> Result<Vec<ExpandedPaneGeometry>, String> {
    expand_pane_geometry_from_window_states(profile, &manifest.window_states)
}

fn validate_renderer_config_profiles<'a>(
    profiles: &'a [RendererConfigProfile],
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererConfigProfile> {
    if profiles.len() != 2 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            "$.renderer_config_profiles",
            format!("expected exactly two renderer configuration profiles, found {}", profiles.len()),
        );
    }
    let expected = [
        (
            PRODUCTION_DEFAULT_CONFIG_ID,
            RendererConfigAuthority::BundledProductionDefault,
        ),
        (
            LIGATURE_ENABLED_CONFIG_ID,
            RendererConfigAuthority::FeatureMaximalNonDefault,
        ),
    ];
    let mut index = BTreeMap::new();
    for (position, profile) in profiles.iter().enumerate() {
        let path = format!("$.renderer_config_profiles[{position}]");
        validator.require_identifier(
            &format!("{path}.renderer_config_profile_id"),
            &profile.renderer_config_profile_id,
        );
        validator.require_repository_ref(&format!("{path}.repository_ref"), &profile.repository_ref);
        if profile.profile_revision == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.profile_revision"),
                "renderer configuration revision must be positive",
            );
        }
        if index
            .insert(profile.renderer_config_profile_id.as_str(), profile)
            .is_some()
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.renderer_config_profile_id"),
                format!("duplicate renderer configuration `{}`", profile.renderer_config_profile_id),
            );
        }
        if let Some((expected_id, expected_authority)) = expected.get(position) {
            if profile.renderer_config_profile_id != *expected_id || profile.authority != *expected_authority {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &path,
                    format!("expected `{expected_id}` with authority {expected_authority:?}"),
                );
            }
        }
        let all_ligatures_enabled = profile.ligature_features.calt_enabled
            && profile.ligature_features.clig_enabled
            && profile.ligature_features.liga_enabled;
        match profile.authority {
            RendererConfigAuthority::BundledProductionDefault if all_ligatures_enabled
                || profile.ligature_features.calt_enabled
                || profile.ligature_features.clig_enabled
                || profile.ligature_features.liga_enabled =>
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.ligature_features"),
                    "bundled production-default must pin calt/clig/liga disabled",
                );
            }
            RendererConfigAuthority::FeatureMaximalNonDefault if !all_ligatures_enabled => {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.ligature_features"),
                    "feature-maximal configuration must enable calt, clig, and liga",
                );
            }
            _ => {}
        }
        if let RendererConfigurationAvailability::Unavailable {
            reason,
            tracking_refs,
        } = &profile.availability
        {
            validate_tracked_limitation(
                &format!("{path}.availability"),
                reason,
                tracking_refs,
                RendererScenarioValidationCode::InvalidState,
                validator,
            );
        }
    }
    index
}

fn validate_layout_profiles<'a>(
    profiles: &'a [RendererLayoutProfile],
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererLayoutProfile> {
    if profiles.len() != RendererFleetPoint::ALL.len() {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            "$.layout_profiles",
            format!("expected exactly four layout profiles, found {}", profiles.len()),
        );
    }
    let mut index = BTreeMap::new();
    let mut fleet_points = BTreeSet::new();
    for (position, profile) in profiles.iter().enumerate() {
        let path = format!("$.layout_profiles[{position}]");
        validator.require_identifier(&format!("{path}.layout_profile_id"), &profile.layout_profile_id);
        validator.require_identifier(&format!("{path}.stable_id_revision"), &profile.stable_id_revision);
        if index.insert(profile.layout_profile_id.as_str(), profile).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.layout_profile_id"),
                format!("duplicate layout profile `{}`", profile.layout_profile_id),
            );
        }
        if !fleet_points.insert(profile.fleet_point) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.fleet_point"),
                "duplicate fleet-point layout",
            );
        }
        if RendererFleetPoint::ALL.get(position) != Some(&profile.fleet_point) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.fleet_point"),
                "layout profiles must appear in canonical fleet order",
            );
        }
        let expected_id = format!(
            "renderer.layout.{}.balanced_contiguous_v1",
            profile.fleet_point.as_str()
        );
        if profile.layout_profile_id != expected_id
            || profile.stable_id_revision != RENDERER_LAYOUT_STABLE_ID_REVISION
            || profile.pane_count != profile.fleet_point.pane_count()
            || profile.tab_count != profile.fleet_point.tab_count()
            || profile.window_count != profile.fleet_point.window_count()
            || profile.initial_split_direction != RendererSplitDirection::Horizontal
            || profile.split_ratio_milli != 500
            || !profile.alternate_split_direction
            || !profile.lowest_ordinal_gets_remainder
        {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &path,
                "layout must equal the closed balanced_contiguous_v1 counts, IDs, split policy, and lowest-ordinal rounding",
            );
        }
        let expanded = expand_layout(profile);
        if expanded.window_ids.len() != usize::from(profile.window_count)
            || expanded.tab_ids.len() != usize::from(profile.tab_count)
            || expanded.pane_ids.len() != usize::from(profile.pane_count)
        {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &path,
                "closed layout expansion did not reproduce declared counts",
            );
        }
    }
    index
}

fn expand_pane_selector(
    path: &str,
    selector: &RendererPaneOrdinalSelector,
    pane_count: u16,
    validator: &mut Validator,
) -> Vec<u16> {
    match selector {
        RendererPaneOrdinalSelector::All => (0..pane_count).collect(),
        RendererPaneOrdinalSelector::OrdinalRange {
            start,
            end_exclusive,
        } => {
            if start >= end_exclusive || *end_exclusive > pane_count {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    path,
                    format!("ordinal range must be non-empty within 0..{pane_count}"),
                );
                Vec::new()
            } else {
                (*start..*end_exclusive).collect()
            }
        }
        RendererPaneOrdinalSelector::Explicit { ordinals } => {
            if ordinals.is_empty() || ordinals.len() > 32 {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    path,
                    "explicit selector must contain 1..=32 ordinals",
                );
            }
            let mut previous = None;
            for (position, ordinal) in ordinals.iter().copied().enumerate() {
                if ordinal >= pane_count || previous.is_some_and(|value| ordinal <= value) {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{path}.ordinals[{position}]"),
                        format!("ordinals must be strictly increasing within 0..{pane_count}"),
                    );
                }
                previous = Some(ordinal);
            }
            ordinals
                .iter()
                .copied()
                .filter(|ordinal| *ordinal < pane_count)
                .collect()
        }
    }
}

fn validate_surface_state_templates<'a>(
    templates: &'a [RendererSurfaceStateTemplate],
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
    configs: &BTreeMap<&str, &RendererConfigProfile>,
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererSurfaceStateTemplate> {
    if templates.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.surface_state_templates",
            "at least one surface-state template is required",
        );
    }
    let mut index = BTreeMap::new();
    for (position, template) in templates.iter().enumerate() {
        let path = format!("$.surface_state_templates[{position}]");
        validator.require_identifier(
            &format!("{path}.surface_state_template_id"),
            &template.surface_state_template_id,
        );
        if index
            .insert(template.surface_state_template_id.as_str(), template)
            .is_some()
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.surface_state_template_id"),
                format!("duplicate surface template `{}`", template.surface_state_template_id),
            );
        }
        validate_surface_state(
            &format!("{path}.surface_state"),
            &template.surface_state,
            content,
            configs,
            validator,
        );
    }
    index
}

fn validate_surface_state(
    path: &str,
    state: &RendererSurfaceState,
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
    configs: &BTreeMap<&str, &RendererConfigProfile>,
    validator: &mut Validator,
) {
    if state.renderer_generation == 0 || state.grid_revision == 0 || state.terminal_revision == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "renderer, grid, and terminal revisions must be positive",
        );
    }
    validator.require_identifier(
        &format!("{path}.renderer_config_profile_id"),
        &state.renderer_config_profile_id,
    );
    if !configs.contains_key(state.renderer_config_profile_id.as_str()) {
        validator.error(
            RendererScenarioValidationCode::DanglingReference,
            format!("{path}.renderer_config_profile_id"),
            format!("undefined renderer configuration `{}`", state.renderer_config_profile_id),
        );
    }
    validate_grid_state(&format!("{path}.grid"), state.grid, validator);
    validator.require_identifier(&format!("{path}.font.font_id"), &state.font.font_id);
    validator.require_repository_ref(
        &format!("{path}.font.pinned_font_ref"),
        &state.font.pinned_font_ref,
    );
    validator.require_identifier(
        &format!("{path}.font.metric_derivation_revision"),
        &state.font.metric_derivation_revision,
    );
    if state.font.base_size_milli_points == 0
        || state.font.base_size_milli_points > MAX_FONT_SIZE_MILLI_POINTS
        || state.font.scale_milli == 0
        || state.font.scale_milli > MAX_SCALE_FACTOR_MILLI
        || state.font.base_cell_width_milli_px == 0
        || state.font.base_cell_width_milli_px
            > MAX_VIEWPORT_DIMENSION_PX.saturating_mul(1_000)
        || state.font.base_cell_height_milli_px == 0
        || state.font.base_cell_height_milli_px
            > MAX_VIEWPORT_DIMENSION_PX.saturating_mul(1_000)
        || state.font.metric_reference_dpi_milli == 0
        || state.font.metric_reference_dpi_milli > MAX_DPI_MILLI
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.font"),
            "base font size, scale, base cell extents, and reference DPI must be positive and within contract bounds",
        );
    }
    validate_display_state(&format!("{path}.display"), &state.display, validator);
    validate_surface_cell_metric_layout(path, state, validator);
    validate_terminal_mode_state(
        &format!("{path}.terminal"),
        &state.terminal,
        state.grid,
        &state.font,
        &state.display,
        content,
        validator,
    );
}

fn validate_surface_cell_metric_layout(
    path: &str,
    state: &RendererSurfaceState,
    validator: &mut Validator,
) {
    let Some(cell_width_milli) = effective_cell_extent_milli(
        state.font.base_cell_width_milli_px,
        state.font.scale_milli,
        state.display.dpi_milli,
        state.display.scale_factor_milli,
        state.font.metric_reference_dpi_milli,
    ) else {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.font"),
            "effective cell-width derivation overflowed or produced zero",
        );
        return;
    };
    let Some(cell_height_milli) = effective_cell_extent_milli(
        state.font.base_cell_height_milli_px,
        state.font.scale_milli,
        state.display.dpi_milli,
        state.display.scale_factor_milli,
        state.font.metric_reference_dpi_milli,
    ) else {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.font"),
            "effective cell-height derivation overflowed or produced zero",
        );
        return;
    };
    let derived_grid_width = u64::from(state.grid.columns)
        .checked_mul(cell_width_milli)
        .map(|value| value / 1_000);
    let derived_grid_height = u64::from(state.grid.rows)
        .checked_mul(cell_height_milli)
        .map(|value| value / 1_000);
    let exact_width = derived_grid_width
        .and_then(|grid| grid.checked_add(u64::from(state.display.content_padding_left_px)))
        .and_then(|value| {
            value.checked_add(u64::from(state.display.content_padding_right_px))
        });
    let exact_height = derived_grid_height
        .and_then(|grid| grid.checked_add(u64::from(state.display.content_padding_top_px)))
        .and_then(|value| {
            value.checked_add(u64::from(state.display.content_padding_bottom_px))
        });
    if exact_width != Some(u64::from(state.display.viewport_width_px))
        || exact_height != Some(u64::from(state.display.viewport_height_px))
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.display"),
            "left/top padding plus floor(grid*scaled-DPI cell extent) plus explicit right/bottom residual must exactly equal the surface viewport",
        );
    }
}

fn validate_grid_state(path: &str, grid: RendererGridState, validator: &mut Validator) {
    if grid.columns == 0
        || grid.rows == 0
        || grid.columns > MAX_GRID_DIMENSION
        || grid.rows > MAX_GRID_DIMENSION
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            format!("grid dimensions must be in 1..={MAX_GRID_DIMENSION}"),
        );
    }
}

fn validate_display_transition(
    path: &str,
    display: &RendererDisplayTransition,
    validator: &mut Validator,
) {
    validator.require_identifier(&format!("{path}.display_id"), &display.display_id);
    validator.require_identifier(&format!("{path}.color_space_id"), &display.color_space_id);
    validator.require_repository_ref(
        &format!("{path}.color_profile_ref"),
        &display.color_profile_ref,
    );
    if display.dpi_milli == 0
        || display.dpi_milli > MAX_DPI_MILLI
        || display.scale_factor_milli == 0
        || display.scale_factor_milli > MAX_SCALE_FACTOR_MILLI
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "DPI and scale must be positive and within bounds",
        );
    }
    validate_dynamic_range_state(
        path,
        display.dynamic_range_mode,
        display.edr_available,
        display.edr_headroom_milli,
        validator,
    );
}

fn validate_dynamic_range_state(
    path: &str,
    dynamic_range_mode: RendererDynamicRangeMode,
    edr_available: bool,
    edr_headroom_milli: u32,
    validator: &mut Validator,
) {
    match dynamic_range_mode {
        RendererDynamicRangeMode::Sdr if edr_available || edr_headroom_milli != 1_000 => {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                path,
                "SDR state must declare EDR unavailable with 1.000 headroom",
            );
        }
        RendererDynamicRangeMode::Hdr if !edr_available || edr_headroom_milli <= 1_000 => {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                path,
                "HDR state requires EDR availability and headroom above 1.000",
            );
        }
        _ => {}
    }
}

fn validate_display_state(path: &str, display: &RendererDisplayState, validator: &mut Validator) {
    validator.require_identifier(&format!("{path}.display_id"), &display.display_id);
    validator.require_identifier(&format!("{path}.color_space_id"), &display.color_space_id);
    validator.require_repository_ref(
        &format!("{path}.color_profile_ref"),
        &display.color_profile_ref,
    );
    if display.dpi_milli == 0
        || display.dpi_milli > MAX_DPI_MILLI
        || display.scale_factor_milli == 0
        || display.scale_factor_milli > MAX_SCALE_FACTOR_MILLI
        || display.viewport_width_px == 0
        || display.viewport_width_px > MAX_VIEWPORT_DIMENSION_PX
        || display.viewport_height_px == 0
        || display.viewport_height_px > MAX_VIEWPORT_DIMENSION_PX
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "DPI, scale, and viewport dimensions must be positive and within bounds",
        );
    }
    validate_dynamic_range_state(
        path,
        display.dynamic_range_mode,
        display.edr_available,
        display.edr_headroom_milli,
        validator,
    );
}

fn validate_pixel_rect(path: &str, rect: RendererPixelRect, validator: &mut Validator) {
    if rect.width == 0
        || rect.height == 0
        || rect.width > MAX_VIEWPORT_DIMENSION_PX
        || rect.height > MAX_VIEWPORT_DIMENSION_PX
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "pixel rectangle width and height must be positive and bounded",
        );
    }
}

fn validate_surface_pixel_rect(
    path: &str,
    rect: RendererPixelRect,
    display: &RendererDisplayState,
    validator: &mut Validator,
) {
    validate_pixel_rect(path, rect, validator);
    let contained = u32::try_from(rect.x)
        .ok()
        .and_then(|x| x.checked_add(rect.width))
        .is_some_and(|right| right <= display.viewport_width_px)
        && u32::try_from(rect.y)
            .ok()
            .and_then(|y| y.checked_add(rect.height))
            .is_some_and(|bottom| bottom <= display.viewport_height_px);
    if !contained {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            format!(
                "surface-local rectangle must be nonnegative and contained in {}x{} viewport",
                display.viewport_width_px, display.viewport_height_px
            ),
        );
    }
}

fn validate_virtual_display_pixel_rect(
    path: &str,
    rect: RendererPixelRect,
    validator: &mut Validator,
) {
    validate_pixel_rect(path, rect, validator);
    let right = i64::from(rect.x).checked_add(i64::from(rect.width));
    let bottom = i64::from(rect.y).checked_add(i64::from(rect.height));
    if right.is_none_or(|value| value > i64::from(i32::MAX))
        || bottom.is_none_or(|value| value > i64::from(i32::MAX))
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "virtual-display rectangle extent overflows the signed coordinate domain",
        );
    }
}

/// Derive backing-pixel cell extent in milli-pixels from logical DPI and
/// display scale, flooring one checked fixed-point expression:
/// `base * font_scale * logical_dpi * display_scale / (1000^2 * reference_dpi)`.
fn effective_cell_extent_milli(
    base_extent_milli_px: u32,
    font_scale_milli: u32,
    logical_dpi_milli: u32,
    display_scale_factor_milli: u32,
    reference_dpi_milli: u32,
) -> Option<u64> {
    if base_extent_milli_px == 0
        || font_scale_milli == 0
        || logical_dpi_milli == 0
        || display_scale_factor_milli == 0
        || reference_dpi_milli == 0
    {
        return None;
    }
    let numerator = u128::from(base_extent_milli_px)
        .checked_mul(u128::from(font_scale_milli))?
        .checked_mul(u128::from(logical_dpi_milli))?
        .checked_mul(u128::from(display_scale_factor_milli))?;
    let denominator = 1_000_u128
        .checked_mul(1_000)?
        .checked_mul(u128::from(reference_dpi_milli))?;
    u64::try_from(numerator / denominator).ok().filter(|value| *value > 0)
}

fn derive_cell_range_rect(
    grid: RendererGridState,
    font: &RendererFontState,
    display: &RendererDisplayState,
    start: RendererCellCoordinate,
    end: RendererCellCoordinate,
) -> Option<RendererPixelRect> {
    if grid.columns == 0
        || grid.rows == 0
        || start.row >= grid.rows
        || end.row >= grid.rows
        || start.column >= grid.columns
        || end.column >= grid.columns
        || end.row != start.row
        || end.column < start.column
    {
        return None;
    }
    let cell_width_milli = effective_cell_extent_milli(
        font.base_cell_width_milli_px,
        font.scale_milli,
        display.dpi_milli,
        display.scale_factor_milli,
        font.metric_reference_dpi_milli,
    )?;
    let cell_height_milli = effective_cell_extent_milli(
        font.base_cell_height_milli_px,
        font.scale_milli,
        display.dpi_milli,
        display.scale_factor_milli,
        font.metric_reference_dpi_milli,
    )?;
    let left = u64::from(display.content_padding_left_px).checked_add(
        u64::from(start.column).checked_mul(cell_width_milli)? / 1_000,
    )?;
    let right = u64::from(display.content_padding_left_px).checked_add(
        (u64::from(end.column) + 1).checked_mul(cell_width_milli)? / 1_000,
    )?;
    let top = u64::from(display.content_padding_top_px).checked_add(
        u64::from(start.row).checked_mul(cell_height_milli)? / 1_000,
    )?;
    let bottom = u64::from(display.content_padding_top_px).checked_add(
        (u64::from(end.row) + 1).checked_mul(cell_height_milli)? / 1_000,
    )?;
    let width = right.checked_sub(left)?;
    let height = bottom.checked_sub(top)?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(RendererPixelRect {
        x: i32::try_from(left).ok()?,
        y: i32::try_from(top).ok()?,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

fn recompute_surface_geometry(state: &mut RendererSurfaceState) -> Result<(), String> {
    let grid = state.grid;
    let font = &state.font;
    let display = &state.display;
    for image in &mut state.terminal.inline_images {
        image.pixel_rect = derive_cell_range_rect(
            grid,
            font,
            display,
            image.cell_start,
            image.cell_end,
        )
        .ok_or_else(|| format!("cannot derive image geometry `{}`", image.image_id))?;
    }
    for hyperlink in &mut state.terminal.hyperlinks {
        hyperlink.pixel_rect = derive_cell_range_rect(
            grid,
            font,
            display,
            hyperlink.cell_start,
            hyperlink.cell_end,
        )
        .ok_or_else(|| {
            format!("cannot derive hyperlink geometry `{}`", hyperlink.hyperlink_id)
        })?;
    }
    if let RendererImeState::Composing {
        candidate_window_geometry: Some(candidate),
        ..
    } = &state.terminal.ime
    {
        if candidate.coordinate_space != RendererPixelCoordinateSpace::VirtualDisplay {
            return Err(
                "IME candidate popup geometry requires virtual_display coordinates"
                    .to_string(),
            );
        }
    }
    if let RendererAccessibilityGeometryState::Active {
        nodes,
        caret_rect,
        ..
    } = &mut state.terminal.accessibility_geometry
    {
        for node in nodes {
            node.pixel_rect = derive_cell_range_rect(
                grid,
                font,
                display,
                node.cell_start,
                node.cell_end,
            )
            .ok_or_else(|| {
                format!("cannot derive accessibility geometry `{}`", node.node_id)
            })?;
        }
        if let Some(rect) = caret_rect {
            let cursor = RendererCellCoordinate {
                row: state.terminal.cursor.row,
                column: state.terminal.cursor.column,
            };
            *rect = derive_cell_range_rect(grid, font, display, cursor, cursor)
                .ok_or_else(|| "cannot derive accessibility caret geometry".to_string())?;
        }
    }
    Ok(())
}

fn refresh_surface_padding_and_geometry(
    state: &mut RendererSurfaceState,
) -> Result<(), String> {
    let cell_width_milli = effective_cell_extent_milli(
        state.font.base_cell_width_milli_px,
        state.font.scale_milli,
        state.display.dpi_milli,
        state.display.scale_factor_milli,
        state.font.metric_reference_dpi_milli,
    )
    .ok_or_else(|| "cannot derive effective cell width".to_string())?;
    let cell_height_milli = effective_cell_extent_milli(
        state.font.base_cell_height_milli_px,
        state.font.scale_milli,
        state.display.dpi_milli,
        state.display.scale_factor_milli,
        state.font.metric_reference_dpi_milli,
    )
    .ok_or_else(|| "cannot derive effective cell height".to_string())?;
    let grid_width = u64::from(state.grid.columns)
        .checked_mul(cell_width_milli)
        .map(|value| value / 1_000)
        .ok_or_else(|| "grid-width derivation overflowed".to_string())?;
    let grid_height = u64::from(state.grid.rows)
        .checked_mul(cell_height_milli)
        .map(|value| value / 1_000)
        .ok_or_else(|| "grid-height derivation overflowed".to_string())?;
    let used_width = u64::from(state.display.content_padding_left_px)
        .checked_add(grid_width)
        .ok_or_else(|| "horizontal content extent overflowed".to_string())?;
    let used_height = u64::from(state.display.content_padding_top_px)
        .checked_add(grid_height)
        .ok_or_else(|| "vertical content extent overflowed".to_string())?;
    let right = u64::from(state.display.viewport_width_px)
        .checked_sub(used_width)
        .ok_or_else(|| "derived grid width exceeds surface viewport".to_string())?;
    let bottom = u64::from(state.display.viewport_height_px)
        .checked_sub(used_height)
        .ok_or_else(|| "derived grid height exceeds surface viewport".to_string())?;
    state.display.content_padding_right_px =
        u32::try_from(right).map_err(|_| "right padding exceeds u32".to_string())?;
    state.display.content_padding_bottom_px =
        u32::try_from(bottom).map_err(|_| "bottom padding exceeds u32".to_string())?;
    recompute_surface_geometry(state)
}

fn terminal_without_derived_geometry(
    terminal: &RendererTerminalModeState,
) -> RendererTerminalModeState {
    let mut normalized = terminal.clone();
    let zero = RendererPixelRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
    for image in &mut normalized.inline_images {
        image.pixel_rect = zero;
    }
    for hyperlink in &mut normalized.hyperlinks {
        hyperlink.pixel_rect = zero;
    }
    if let RendererAccessibilityGeometryState::Active {
        nodes,
        caret_rect,
        ..
    } = &mut normalized.accessibility_geometry
    {
        for node in nodes {
            node.pixel_rect = zero;
        }
        if let Some(rect) = caret_rect {
            *rect = zero;
        }
    }
    normalized
}

fn validate_cell_coordinate(
    path: &str,
    coordinate: RendererCellCoordinate,
    grid: RendererGridState,
    validator: &mut Validator,
) {
    if coordinate.row >= grid.rows || coordinate.column >= grid.columns {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            format!(
                "cell ({},{}) is outside {}x{} grid",
                coordinate.row, coordinate.column, grid.rows, grid.columns
            ),
        );
    }
}

fn validate_cell_range(
    path: &str,
    start: RendererCellCoordinate,
    end: RendererCellCoordinate,
    grid: RendererGridState,
    validator: &mut Validator,
) {
    validate_cell_coordinate(&format!("{path}.start"), start, grid, validator);
    validate_cell_coordinate(&format!("{path}.end"), end, grid, validator);
    if (end.row, end.column) < (start.row, start.column) {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "cell range end must not precede start",
        );
    }
}

fn validate_terminal_buffer(
    path: &str,
    buffer: &RendererTerminalBufferState,
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
    validator: &mut Validator,
) {
    validator.require_identifier(&format!("{path}.buffer_id"), &buffer.buffer_id);
    if buffer.revision == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.revision"),
            "terminal buffer revision must be positive",
        );
    }
    let mut seen = BTreeSet::new();
    for (position, corpus_id) in buffer.content_corpus_ids.iter().enumerate() {
        let corpus_path = format!("{path}.content_corpus_ids[{position}]");
        validator.require_identifier(&corpus_path, corpus_id);
        if !seen.insert(corpus_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &corpus_path,
                format!("duplicate buffer content identity `{corpus_id}`"),
            );
        }
        if !content.contains_key(corpus_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                corpus_path,
                format!("undefined content corpus `{corpus_id}`"),
            );
        }
    }
}

fn validate_terminal_mode_state(
    path: &str,
    terminal: &RendererTerminalModeState,
    grid: RendererGridState,
    font: &RendererFontState,
    display: &RendererDisplayState,
    content: &BTreeMap<&str, &RendererContentCorpusReference>,
    validator: &mut Validator,
) {
    validate_terminal_buffer(&format!("{path}.primary_buffer"), &terminal.primary_buffer, content, validator);
    validate_terminal_buffer(&format!("{path}.alternate_buffer"), &terminal.alternate_buffer, content, validator);
    if terminal.primary_buffer.buffer_id == terminal.alternate_buffer.buffer_id {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            path,
            "primary and alternate buffers must have distinct identities",
        );
    }
    if terminal.alternate_buffer.scrollback_lines != 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.alternate_buffer.scrollback_lines"),
            "alternate-screen buffer cannot carry primary scrollback",
        );
    }
    if terminal.viewport_top < -i64::from(terminal.primary_buffer.scrollback_lines)
        || terminal.viewport_top > 0
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.viewport_top"),
            "primary viewport origin must remain within retained scrollback and live bottom",
        );
    }
    if terminal.active_buffer == RendererTerminalBufferKind::Alternate
        && terminal.viewport_top != 0
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.viewport_top"),
            "alternate-buffer viewport must remain at zero because alternate has no scrollback",
        );
    }
    validate_cell_coordinate(
        &format!("{path}.cursor"),
        RendererCellCoordinate {
            row: terminal.cursor.row,
            column: terminal.cursor.column,
        },
        grid,
        validator,
    );
    if let RendererSelectionState::Active { anchor, focus, .. } = terminal.selection {
        validate_cell_coordinate(&format!("{path}.selection.anchor"), anchor, grid, validator);
        validate_cell_coordinate(&format!("{path}.selection.focus"), focus, grid, validator);
    }
    if let RendererImeState::Composing {
        preedit_content_corpus_id,
        preedit_origin,
        caret,
        input_source_id,
        composition_segments,
        candidate_window_geometry,
    } = &terminal.ime
    {
        validator.require_identifier(
            &format!("{path}.ime.preedit_content_corpus_id"),
            preedit_content_corpus_id,
        );
        if !content.contains_key(preedit_content_corpus_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.ime.preedit_content_corpus_id"),
                format!("undefined IME content corpus `{preedit_content_corpus_id}`"),
            );
        }
        validator.require_identifier(&format!("{path}.ime.input_source_id"), input_source_id);
        validate_cell_coordinate(&format!("{path}.ime.preedit_origin"), *preedit_origin, grid, validator);
        validate_cell_coordinate(&format!("{path}.ime.caret"), *caret, grid, validator);
        if composition_segments.is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.ime.composition_segments"),
                "active IME composition requires at least one segment",
            );
        }
        let mut segment_ids = BTreeSet::new();
        for (position, segment) in composition_segments.iter().enumerate() {
            let segment_path = format!("{path}.ime.composition_segments[{position}]");
            validator.require_identifier(&format!("{segment_path}.segment_id"), &segment.segment_id);
            if !segment_ids.insert(segment.segment_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::DuplicateId,
                    format!("{segment_path}.segment_id"),
                    format!("duplicate IME segment `{}`", segment.segment_id),
                );
            }
            validate_cell_range(&segment_path, segment.start, segment.end, grid, validator);
        }
        if let Some(geometry) = candidate_window_geometry {
            if geometry.coordinate_space != RendererPixelCoordinateSpace::VirtualDisplay {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.ime.candidate_window_geometry.coordinate_space"),
                    "IME candidate popup geometry must use virtual_display coordinates",
                );
            }
            validate_virtual_display_pixel_rect(
                &format!("{path}.ime.candidate_window_geometry.rect"),
                geometry.rect,
                validator,
            );
        }
    }
    let mut image_ids = BTreeSet::new();
    for (position, image) in terminal.inline_images.iter().enumerate() {
        let image_path = format!("{path}.inline_images[{position}]");
        validator.require_identifier(&format!("{image_path}.image_id"), &image.image_id);
        if !image_ids.insert(image.image_id.as_str()) {
            validator.error(RendererScenarioValidationCode::DuplicateId, &image_path, "duplicate image identity");
        }
        validate_cell_range(&image_path, image.cell_start, image.cell_end, grid, validator);
        validate_surface_pixel_rect(
            &format!("{image_path}.pixel_rect"),
            image.pixel_rect,
            display,
            validator,
        );
        if image.cell_start.row != image.cell_end.row {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &image_path,
                "v1 one-rectangle image geometry requires a single-row cell range",
            );
        }
        if derive_cell_range_rect(grid, font, display, image.cell_start, image.cell_end)
            != Some(image.pixel_rect)
        {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{image_path}.pixel_rect"),
                "image geometry must equal the frozen cell-to-surface derivation",
            );
        }
    }
    let mut hyperlink_ids = BTreeSet::new();
    for (position, hyperlink) in terminal.hyperlinks.iter().enumerate() {
        let link_path = format!("{path}.hyperlinks[{position}]");
        validator.require_identifier(&format!("{link_path}.hyperlink_id"), &hyperlink.hyperlink_id);
        if !hyperlink_ids.insert(hyperlink.hyperlink_id.as_str()) {
            validator.error(RendererScenarioValidationCode::DuplicateId, &link_path, "duplicate hyperlink identity");
        }
        validate_cell_range(&link_path, hyperlink.cell_start, hyperlink.cell_end, grid, validator);
        if hyperlink.cell_start.row != hyperlink.cell_end.row {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                &link_path,
                "v1 one-rectangle hyperlink geometry requires a single-row cell range",
            );
        }
        validate_surface_pixel_rect(
            &format!("{link_path}.pixel_rect"),
            hyperlink.pixel_rect,
            display,
            validator,
        );
        if derive_cell_range_rect(
            grid,
            font,
            display,
            hyperlink.cell_start,
            hyperlink.cell_end,
        ) != Some(hyperlink.pixel_rect)
        {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{link_path}.pixel_rect"),
                "hyperlink geometry must equal the frozen cell-to-surface derivation",
            );
        }
    }
    if let RendererAccessibilityGeometryState::Active {
        tree_revision,
        nodes,
        caret_rect,
    } = &terminal.accessibility_geometry
    {
        if *tree_revision == 0 || nodes.is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.accessibility_geometry"),
                "active accessibility geometry requires positive revision and nodes",
            );
        }
        let mut node_ids = BTreeSet::new();
        let mut focused = 0_usize;
        for (position, node) in nodes.iter().enumerate() {
            let node_path = format!("{path}.accessibility_geometry.nodes[{position}]");
            validator.require_identifier(&format!("{node_path}.node_id"), &node.node_id);
            validator.require_identifier(&format!("{node_path}.role_id"), &node.role_id);
            if !node_ids.insert(node.node_id.as_str()) {
                validator.error(RendererScenarioValidationCode::DuplicateId, &node_path, "duplicate accessibility node identity");
            }
            focused += usize::from(node.focused);
            validate_cell_range(&node_path, node.cell_start, node.cell_end, grid, validator);
            if node.cell_start.row != node.cell_end.row {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    &node_path,
                    "v1 one-rectangle accessibility geometry requires a single-row cell range",
                );
            }
            validate_surface_pixel_rect(
                &format!("{node_path}.pixel_rect"),
                node.pixel_rect,
                display,
                validator,
            );
            if derive_cell_range_rect(grid, font, display, node.cell_start, node.cell_end)
                != Some(node.pixel_rect)
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{node_path}.pixel_rect"),
                    "accessibility node geometry must equal the frozen cell-to-surface derivation",
                );
            }
        }
        if focused != 1 {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.accessibility_geometry.nodes"),
                format!("active accessibility tree requires exactly one focused node, found {focused}"),
            );
        }
        if let Some(rect) = caret_rect {
            validate_surface_pixel_rect(
                &format!("{path}.accessibility_geometry.caret_rect"),
                *rect,
                display,
                validator,
            );
            let cursor = RendererCellCoordinate {
                row: terminal.cursor.row,
                column: terminal.cursor.column,
            };
            if derive_cell_range_rect(grid, font, display, cursor, cursor) != Some(*rect) {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.accessibility_geometry.caret_rect"),
                    "accessibility caret must equal the frozen cursor-cell derivation",
                );
            }
        }
    }
}
