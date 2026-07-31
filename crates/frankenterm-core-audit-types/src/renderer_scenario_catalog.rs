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

/// Contract identifier accepted by schema version 1.
pub const RENDERER_SCENARIO_CONTRACT_ID: &str = "ft.renderer_scenario_catalog.v1";

/// Schema version implemented by this module.
pub const RENDERER_SCENARIO_SCHEMA_VERSION: u32 = 1;

/// Bead that owns the version-1 contract.
pub const RENDERER_SCENARIO_SOURCE_BEAD_ID: &str =
    "ft-interactive-systems-performance-4tenz.3.1";

/// Maximum raw JSON document accepted by the bounded decoder.
pub const MAX_RENDERER_SCENARIO_CATALOG_BYTES: usize = 1024 * 1024;

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

/// Number of required closed-domain gestures.
pub const REQUIRED_RENDERER_GESTURE_COUNT: usize = 8;

/// Number of exact fleet qualification points.
pub const REQUIRED_RENDERER_FLEET_POINT_COUNT: usize = 4;

/// Number of gesture-by-fleet coverage cells.
pub const REQUIRED_RENDERER_SCENARIO_COUNT: usize =
    REQUIRED_RENDERER_GESTURE_COUNT * REQUIRED_RENDERER_FLEET_POINT_COUNT;

/// Number of terminal-content features required across active corpus entries.
pub const REQUIRED_RENDERER_TERMINAL_FEATURE_COUNT: usize = 13;

/// Exact invariant count required in every scenario.
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
    Reflow80To200,
    /// Reflow from exactly 200 columns to exactly 80 columns.
    Reflow200To80,
    /// Increase the configured font size.
    ZoomIn,
    /// Decrease the configured font size.
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
}

impl RendererCapability {
    /// Exact capability matrix required on every scenario.
    pub const ALL: [Self; 18] = [
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
    /// Named revision of the effective-size/cell-metric derivation.
    pub metric_derivation_revision: String,
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
        /// Exact composition segment/range manifest.
        composition_manifest_ref: String,
        /// Optional candidate-window geometry when the input source exposes it.
        candidate_window_rect: Option<RendererPixelRect>,
    },
}

/// Accessibility tree and geometry state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererAccessibilityGeometryState {
    /// Deterministic tree revision.
    pub tree_revision: u64,
    /// Number of nodes in the expected tree.
    pub node_count: u32,
    /// Stable focused-node identity when a node is focused.
    pub focused_node_id: Option<String>,
    /// Expected caret geometry when present.
    pub caret_rect: Option<RendererPixelRect>,
}

/// Complete terminal-mode state visible during a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererTerminalModeState {
    /// Stable-row viewport origin; zero is the live bottom and history is negative.
    pub viewport_top: i64,
    /// Whether the alternate screen is active.
    pub alternate_screen: bool,
    /// Explicit selection state.
    pub selection: RendererSelectionState,
    /// Explicit cursor state.
    pub cursor: RendererCursorState,
    /// Explicit IME state.
    pub ime: RendererImeState,
    /// Number of inline images represented by canonical corpus content.
    pub inline_image_count: u16,
    /// Repository reference to complete image geometry when images are present.
    pub inline_image_geometry_ref: Option<String>,
    /// Number of active hyperlink spans represented by corpus content.
    pub active_hyperlink_count: u16,
    /// Repository reference to complete hyperlink ranges when links are present.
    pub hyperlink_geometry_ref: Option<String>,
    /// Accessibility tree and geometry state.
    pub accessibility_geometry: RendererAccessibilityGeometryState,
}

/// Fully explicit focused-pane renderer state.
///
/// The scenario's topology and pane-state manifest references carry complete
/// per-window/tab/pane state; this focused surface must not be interpreted as
/// an enumeration of an entire fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererSurfaceState {
    /// Monotonic renderer generation for the focused pane.
    pub renderer_generation: u64,
    /// Monotonic grid generation for the focused pane.
    pub grid_revision: u64,
    /// Monotonic terminal-state generation for the focused pane.
    pub terminal_revision: u64,
    /// Relative reference to the complete renderer configuration.
    pub renderer_config_ref: String,
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
        /// New Draft or Standard mode.
        mode: RendererQualityMode,
    },
    /// Replace the complete display/DPI/viewport state.
    MoveToDisplay {
        /// Explicit target window and affected panes.
        target: RendererMutationTarget,
        /// New display state.
        display: RendererDisplayState,
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
    /// Replace terminal-mode/accessibility state without embedding corpus data.
    SetTerminalMode {
        /// Explicit target tab/panes.
        target: RendererMutationTarget,
        /// New mode state.
        terminal: RendererTerminalModeState,
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

/// Ordered pane identity and its exact terminal-content inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPaneContentBinding {
    /// Stable pane identity.
    pub pane_id: String,
    /// Non-empty exact terminal-content inputs for this pane.
    pub content_corpus_ids: Vec<String>,
}

/// Ordered tab identity within one window manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOrderedTabManifest {
    /// Stable tab identity.
    pub tab_id: String,
    /// Zero-based contiguous ordinal within this window.
    pub tab_ordinal: u16,
    /// Pane identities/content owned by this tab, in deterministic traversal order.
    pub panes: Vec<RendererPaneContentBinding>,
}

/// Ordered window and tab inventory at one exact phase/event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOrderedWindowManifest {
    /// Stable window identity.
    pub window_id: String,
    /// Zero-based contiguous ordinal within the fleet point.
    pub window_ordinal: u16,
    /// Stable active-tab identity within `tabs`.
    pub active_tab_id: String,
    /// Ordered per-window tab sequence; order is part of the contract.
    pub tabs: Vec<RendererOrderedTabManifest>,
}

/// Complete phase-specific fleet manifest binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererPhaseManifestBinding {
    /// Phase of the bound timeline event.
    pub phase: RendererTimelinePhase,
    /// Exact timeline event ordinal.
    pub event_ordinal: u32,
    /// Exact fleet-wide pane count.
    pub pane_count: u16,
    /// Exact fleet-wide tab count.
    pub tab_count: u16,
    /// Exact fleet-wide window count.
    pub window_count: u16,
    /// Stable focused window identity.
    pub focused_window_id: String,
    /// Stable focused pane identity.
    pub focused_pane_id: String,
    /// Ordered window/tab/pane identity inventory.
    pub windows: Vec<RendererOrderedWindowManifest>,
    /// Exact canonical feature union derived from every pane content binding.
    pub terminal_feature_union: Vec<RendererTerminalFeature>,
    /// Complete deterministic topology manifest.
    pub topology_ref: String,
    /// Complete deterministic split-tree and geometry manifest.
    pub split_geometry_ref: String,
    /// Complete per-pane terminal-state/output manifest.
    pub pane_state_manifest_ref: String,
    /// Exact focused-surface state manifest for this phase.
    pub focused_surface_state_ref: String,
}

/// Visual checkpoint bound to state, bitmap, and accessibility oracles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererVisualCheckpoint {
    /// Stable checkpoint identity.
    pub checkpoint_id: String,
    /// Initial, Draft source, intermediate, or final role.
    pub role: RendererCheckpointRole,
    /// Phase copied from the bound timeline event.
    pub phase: RendererTimelinePhase,
    /// Exact event ordinal at which all three oracles are evaluated.
    pub event_ordinal: u32,
    /// Non-empty invariant identities evaluated at this exact event.
    pub expected_invariant_ids: Vec<String>,
    /// Exact closed detector inventory applicable at this checkpoint.
    pub expected_detector_ids: Vec<RendererCheckpointDetectorId>,
    /// Explicitly nonblank baseline/transient/snap-back/steady class.
    pub expected_frame_content_class: RendererFrameContentClass,
    /// Complete phase/event-specific fleet and surface manifest binding.
    pub phase_manifest: RendererPhaseManifestBinding,
    /// Repository reference to the state-oracle definition.
    pub state_oracle_ref: String,
    /// Repository reference to the visual oracle/comparator input.
    pub visual_oracle_ref: String,
    /// Independent Standard oracle used only by the snap-back subject.
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

/// Terminal-content input referenced by deterministic workloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererContentCorpusReference {
    /// Stable terminal-content identity.
    pub content_corpus_id: String,
    /// Relative, non-traversing repository reference to its canonical source.
    pub repository_ref: String,
    /// Terminal features supplied by that source.
    pub terminal_features: Vec<RendererTerminalFeature>,
    /// Positive payload or generator revision.
    pub payload_revision: u32,
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
}

/// Exact per-pane output rate in a deterministic stream distribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOutputPaneRate {
    /// Stable pane identity from the scenario manifests.
    pub pane_id: String,
    /// Exact decimal bytes per second assigned to this pane.
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
    /// Exact distribution manifest.
    pub distribution_manifest_ref: String,
    /// Exact aggregate decimal byte rate.
    pub aggregate_bytes_per_second: u64,
    /// Non-empty per-pane allocation whose sum equals the aggregate rate.
    pub pane_rates: Vec<RendererOutputPaneRate>,
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
    /// Complete workload layout/topology identity.
    pub layout_manifest_ref: String,
    /// Exact renderer configuration identity.
    pub renderer_config_ref: String,
    /// Exact effective font/cell-metric derivation identity.
    pub font_metric_derivation_ref: String,
    /// Exact native gesture-input duration; gesture_end occurs here.
    pub gesture_duration_us: u64,
    /// Positive end-to-end duration; final settle occurs here.
    pub total_duration_us: u64,
    /// Exact number of timeline events in every bound scenario.
    pub event_count: u32,
    /// Optional fully pinned PTY stream; absent means zero background output.
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
    /// Deliberately harder concurrent workload related to, not equal to, an SLO.
    RelatedAdversarialSuperset,
    /// Larger-fleet stress variant related to a singular source predicate.
    RelatedFleetStress,
    /// One exact final/settled checkpoint carries the binding.
    CheckpointPredicate,
    /// The row references comparator mechanics only, never an SLO verdict.
    ComparatorMechanismOnly,
}

/// Typed requirement crosswalk row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererRequirementBinding {
    /// Stable full renderer requirement ID.
    pub requirement_id: RendererRequirementId,
    /// Exact scenario, checkpoint, or comparator-only scope.
    pub scope: RendererRequirementScope,
    /// Required only for checkpoint-predicate bindings.
    pub checkpoint_id: Option<String>,
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
    /// Exact checkpoint expected to detect the defect.
    pub checkpoint_id: String,
    /// Timeline phase in which the defect is injected.
    pub injected_phase: RendererTimelinePhase,
    /// Closed checkpoint detector expected to reject the defect.
    pub bound_detector_id: RendererCheckpointDetectorId,
    /// Required corpus feature for feature-specific controls.
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

/// Exact K0-K13 keypress trace-v2 stage inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RendererKeypressTraceStage {
    #[serde(rename = "K0")]
    K0,
    #[serde(rename = "K1")]
    K1,
    #[serde(rename = "K2")]
    K2,
    #[serde(rename = "K3")]
    K3,
    #[serde(rename = "K4")]
    K4,
    #[serde(rename = "K5")]
    K5,
    #[serde(rename = "K6")]
    K6,
    #[serde(rename = "K7")]
    K7,
    #[serde(rename = "K8")]
    K8,
    #[serde(rename = "K9")]
    K9,
    #[serde(rename = "K10")]
    K10,
    #[serde(rename = "K11")]
    K11,
    #[serde(rename = "K12")]
    K12,
    #[serde(rename = "K13")]
    K13,
}

impl RendererKeypressTraceStage {
    pub const ALL: [Self; 14] = [
        Self::K0,
        Self::K1,
        Self::K2,
        Self::K3,
        Self::K4,
        Self::K5,
        Self::K6,
        Self::K7,
        Self::K8,
        Self::K9,
        Self::K10,
        Self::K11,
        Self::K12,
        Self::K13,
    ];
}

/// Scenario-local measurement endpoints without a measurement verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererMeasurementBinding {
    FirstCorrectViewport {
        mutation_event_ordinal: u32,
        target_presented_frame_predicate_ref: String,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    SteadyPresentedFps {
        interval_start_event_ordinal: u32,
        interval_end_event_ordinal: u32,
        minimum_presented_interval_count: u32,
        presented_interval_contract_ref: String,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    ColdReflowConvergence {
        trigger_event_ordinal: u32,
        target_checkpoint_id: String,
        preconditioning_profile_id: RendererPreconditioningProfileId,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    SnapBack {
        last_draft_checkpoint_id: String,
        standard_snap_back_subject_checkpoint_id: String,
        independent_standard_oracle_ref: String,
        presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    },
    KeypressToFirstCorrectPresent {
        key_event_id: String,
        key_action_event_ordinal: u32,
        target_presented_frame_predicate_ref: String,
        stage_metrics_contract_ref: String,
        required_stage_ids: Vec<RendererKeypressTraceStage>,
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
    SingleCheckpoint,
    Pair,
    Interval,
    WholeTimeline,
    AllObservedFrames,
}

/// Explicit non-local detector endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererDetectorBinding {
    Pair {
        detector_id: RendererCheckpointDetectorId,
        source_checkpoint_id: String,
        target_checkpoint_id: String,
    },
    Interval {
        detector_id: RendererCheckpointDetectorId,
        start_checkpoint_id: String,
        end_checkpoint_id: String,
    },
    WholeTimeline {
        detector_id: RendererCheckpointDetectorId,
    },
    AllObservedFrames {
        detector_id: RendererCheckpointDetectorId,
        observation_policy_ref: String,
    },
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

/// Whole-stream transient observation requirement; checkpoints are anchors only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererObservedFramePolicy {
    pub observation_policy_ref: String,
    pub start_event_ordinal: u32,
    pub end_event_ordinal: u32,
    pub capture_every_observed_presented_frame: bool,
    pub identity_fields: Vec<RendererObservedFrameIdentityField>,
    pub all_frame_detector_ids: Vec<RendererCheckpointDetectorId>,
}

/// Strict downstream run-log, stage-metric, and production-path contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererRunObservationContracts {
    pub observed_frame_stream_ref: String,
    pub stage_metrics_contract_ref: String,
    pub production_path_receipt_contract_ref: String,
    pub run_log_schema_ref: String,
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
    /// Exact pane count; validated against `fleet_point`.
    pub pane_count: u16,
    /// Exact tab count; validated against `fleet_point`.
    pub tab_count: u16,
    /// Exact window count; validated against `fleet_point`.
    pub window_count: u16,
    /// Complete initial Begin-phase manifest with ordered tab identities.
    pub initial_manifest: RendererPhaseManifestBinding,
    /// Complete final Settle-phase manifest with ordered tab identities.
    pub final_manifest: RendererPhaseManifestBinding,
    /// Must equal [`expected_renderer_scenario_seed`] for this coverage cell.
    pub seed: u64,
    /// Reference to a deterministic workload definition.
    pub workload_id: String,
    /// Exact, typed requirement scope crosswalk for this scenario.
    pub requirement_bindings: Vec<RendererRequirementBinding>,
    /// Required only for output-overlap resize; forbidden otherwise.
    pub output_overlap_resize_mode: Option<RendererResizeMode>,
    /// Configured post-snap steady mode; Draft is forbidden here.
    pub configured_steady_quality: RendererQualityMode,
    /// Fully explicit initial renderer and terminal state.
    pub initial_state: RendererSurfaceState,
    /// Fully explicit final renderer and terminal state.
    pub final_state: RendererSurfaceState,
    /// Replayable, zero-based, strictly ordered gesture timeline.
    pub timeline: Vec<RendererTimelineEvent>,
    /// Expected intermediate invariants.
    pub expected_invariants: Vec<RendererExpectedInvariant>,
    /// State, visual, and accessibility checkpoints.
    pub visual_checkpoints: Vec<RendererVisualCheckpoint>,
    /// Continuous frame observation from gesture_begin through final settle.
    pub observed_frame_policy: RendererObservedFramePolicy,
    /// Pair/interval/whole-timeline detector assertions.
    pub detector_bindings: Vec<RendererDetectorBinding>,
    /// Downstream measurement endpoint vocabulary.
    pub measurement_bindings: Vec<RendererMeasurementBinding>,
    /// Exact cadence profiles requested without multiplying matrix cells.
    pub presentation_target_profile_ids: Vec<RendererPresentationTargetProfileId>,
    /// Exact fresh/aged precondition profiles requested for this cell.
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
    /// Deterministic workload identities.
    pub workloads: Vec<RendererWorkloadDefinition>,
    /// Exact detector mechanism inventory.
    pub detector_contracts: Vec<RendererDetectorContract>,
    /// Fixed 60/120 and variable-refresh request profiles.
    pub presentation_target_profiles: Vec<RendererPresentationTargetProfile>,
    /// Fresh and aged session/cache precondition profiles.
    pub preconditioning_profiles: Vec<RendererPreconditioningProfile>,
    /// Exact five non-qualifying driver-canary definitions.
    pub driver_canaries: Vec<RendererDriverCanaryDefinition>,
    /// Optional synthetic RQ-S1 substrate, never inferred from native cells.
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
}

/// Deterministically ordered semantic validation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererScenarioValidationReport {
    /// True only when no semantic errors were found.
    pub valid: bool,
    /// True only when the valid contract has no declared execution gaps.
    pub execution_ready: bool,
    /// Errors sorted by path, stable code, and detail.
    pub errors: Vec<RendererScenarioValidationError>,
    /// Explicit non-green gaps sorted by path, stable code, and detail.
    pub gaps: Vec<RendererScenarioValidationGap>,
}

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
}

impl Validator {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            gaps: Vec::new(),
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
        let valid = self.errors.is_empty();
        let execution_ready = valid && self.gaps.is_empty();
        RendererScenarioValidationReport {
            valid,
            execution_ready,
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

struct CatalogIndex<'a> {
    content: BTreeMap<&'a str, &'a RendererContentCorpusReference>,
    evidence: BTreeMap<&'a str, &'a RendererEvidenceSource>,
    workloads: BTreeMap<&'a str, &'a RendererWorkloadDefinition>,
    detector_contracts:
        BTreeMap<RendererCheckpointDetectorId, &'a RendererDetectorContract>,
    presentation_profiles:
        BTreeMap<RendererPresentationTargetProfileId, &'a RendererPresentationTargetProfile>,
    preconditioning_profiles:
        BTreeMap<RendererPreconditioningProfileId, &'a RendererPreconditioningProfile>,
    driver_canaries: BTreeMap<RendererDriverCanaryId, &'a RendererDriverCanaryDefinition>,
}

/// Validate a renderer scenario catalog without file I/O or claim promotion.
#[must_use]
pub fn validate_renderer_scenario_catalog(
    catalog: &RendererScenarioCatalog,
) -> RendererScenarioValidationReport {
    let mut validator = Validator::new();
    validate_catalog_header(catalog, &mut validator);
    validate_oracle_contracts(&catalog.oracle_contracts, &mut validator);
    validate_accessibility_authority_boundary(
        &catalog.accessibility_authority_boundary,
        &mut validator,
    );
    let content = validate_content_corpus_references(
        &catalog.content_corpus_references,
        &mut validator,
    );
    let evidence = validate_evidence_sources(&catalog.evidence_sources, &mut validator);
    let workloads = validate_workloads(&catalog.workloads, &content, &mut validator);
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
        content,
        evidence,
        workloads,
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
    validator.finish()
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
    if catalog.catalog_revision == 0 {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.catalog_revision",
            "catalog revision must be positive",
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

const GESTURE_STATUS_PARTIAL: [RendererCorpusCoverageStatus; 1] =
    [RendererCorpusCoverageStatus::Partial];
const GESTURE_STATUS_PARTIAL_AND_PRESENT: [RendererCorpusCoverageStatus; 2] = [
    RendererCorpusCoverageStatus::Partial,
    RendererCorpusCoverageStatus::PresentUnqualified,
];
const GESTURE_STATUS_GAP: [RendererCorpusCoverageStatus; 1] =
    [RendererCorpusCoverageStatus::Gap];
const GESTURE_STATUS_PRESENT_TWICE: [RendererCorpusCoverageStatus; 2] = [
    RendererCorpusCoverageStatus::PresentUnqualified,
    RendererCorpusCoverageStatus::PresentUnqualified,
];

const TRACK_3_6: [&str; 1] = ["ft-interactive-systems-performance-4tenz.3.6"];
const TRACK_3_6_1_TWICE: [&str; 2] = [
    "ft-interactive-systems-performance-4tenz.3.6.1",
    "ft-interactive-systems-performance-4tenz.3.6.1",
];
const TRACK_OUTPUT_OVERLAP: [&str; 3] = [
    "ft-interactive-systems-performance-4tenz.3.6",
    "ft-interactive-systems-performance-4tenz.3.3",
    "ft-interactive-systems-performance-4tenz.3.4",
];

const fn expected_gesture_coverage_statuses(
    gesture: RendererGesture,
) -> &'static [RendererCorpusCoverageStatus] {
    match gesture {
        RendererGesture::SameGridDrag
        | RendererGesture::Reflow200To80
        | RendererGesture::ZoomIn
        | RendererGesture::ZoomOut => &GESTURE_STATUS_PARTIAL,
        RendererGesture::GridChangingDrag => &GESTURE_STATUS_PARTIAL_AND_PRESENT,
        RendererGesture::Reflow80To200 | RendererGesture::OutputOverlapResize => {
            &GESTURE_STATUS_GAP
        }
        RendererGesture::DpiDisplayMove => &GESTURE_STATUS_PRESENT_TWICE,
    }
}

const fn expected_gesture_tracking_refs(gesture: RendererGesture) -> &'static [&'static str] {
    match gesture {
        RendererGesture::SameGridDrag
        | RendererGesture::Reflow80To200
        | RendererGesture::Reflow200To80
        | RendererGesture::ZoomIn
        | RendererGesture::ZoomOut => &TRACK_3_6,
        RendererGesture::GridChangingDrag | RendererGesture::DpiDisplayMove => {
            &TRACK_3_6_1_TWICE
        }
        RendererGesture::OutputOverlapResize => &TRACK_OUTPUT_OVERLAP,
    }
}

const SOURCE_SAME_GRID: [&str; 1] =
    ["tests/golden/gpu/multipane-resize-static-snapshot/input.json"];
const SOURCE_GRID_CHANGING: [&str; 2] = [
    "fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml",
    "tests/golden/gpu/stress/rapid-resize-10s/input.json",
];
const SOURCE_REFLOW_FORWARD: [&str; 1] =
    ["docs/perf/resize-quality-slo.json#RQ-S9.reflow_latency"];
const SOURCE_REFLOW_REVERSE: [&str; 1] =
    ["fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml"];
const SOURCE_ZOOM: [&str; 1] =
    ["fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml"];
const SOURCE_DPI_DISPLAY: [&str; 2] = [
    "tests/golden/gpu/stress/dpi-1_00/input.json",
    "tests/golden/gpu/stress/dpi-2_00/input.json",
];
const SOURCE_OUTPUT_OVERLAP: [&str; 1] =
    ["fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml"];

const fn expected_gesture_source_refs(gesture: RendererGesture) -> &'static [&'static str] {
    match gesture {
        RendererGesture::SameGridDrag => &SOURCE_SAME_GRID,
        RendererGesture::GridChangingDrag => &SOURCE_GRID_CHANGING,
        RendererGesture::Reflow80To200 => &SOURCE_REFLOW_FORWARD,
        RendererGesture::Reflow200To80 => &SOURCE_REFLOW_REVERSE,
        RendererGesture::ZoomIn | RendererGesture::ZoomOut => &SOURCE_ZOOM,
        RendererGesture::DpiDisplayMove => &SOURCE_DPI_DISPLAY,
        RendererGesture::OutputOverlapResize => &SOURCE_OUTPUT_OVERLAP,
    }
}

fn validate_gesture_authority_map(
    entries: &[RendererGestureAuthorityEntry],
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
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
        if entry.sources.is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.sources"),
                "gesture authority row requires at least one classified source",
            );
        }
        let mut source_keys = BTreeSet::new();
        let mut statuses = Vec::new();
        let mut source_refs = Vec::new();
        let mut tracking_refs = Vec::new();
        let mut derived_replay = false;
        let mut derived_compare = false;
        for (source_position, source) in entry.sources.iter().enumerate() {
            let source_path = format!("{path}.sources[{source_position}]");
            validator.require_identifier(
                &format!("{source_path}.corpus_id"),
                &source.corpus_id,
            );
            validator.require_repository_ref(
                &format!("{source_path}.source_ref"),
                &source.source_ref,
            );
            if !source_keys.insert(source.corpus_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::InvalidGestureAuthority,
                    format!("{source_path}.corpus_id"),
                    format!(
                        "duplicate gesture-authority corpus `{}`",
                        source.corpus_id
                    ),
                );
            }
            statuses.push(source.coverage_status);
            source_refs.push(source.source_ref.as_str());
            match corpus.get(source.corpus_id.as_str()) {
                Some(reference) => {
                    if source.source_ref != reference.repository_ref {
                        validator.error(
                            RendererScenarioValidationCode::InvalidGestureAuthority,
                            format!("{source_path}.source_ref"),
                            format!(
                                "source_ref must equal corpus `{}` repository_ref `{}`",
                                source.corpus_id, reference.repository_ref
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
                    format!("{source_path}.corpus_id"),
                    format!("undefined gesture-authority corpus `{}`", source.corpus_id),
                ),
            }
            validate_non_direct_status_fields(
                &source_path,
                source.coverage_status,
                source.limitation.as_deref(),
                &source.tracking_refs,
                validator,
            );
            tracking_refs.extend(source.tracking_refs.iter().map(String::as_str));
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
        let expected_statuses = expected_gesture_coverage_statuses(entry.gesture);
        if statuses.as_slice() != expected_statuses {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.sources"),
                format!(
                    "gesture `{}` requires canonical source statuses {:?}, found {:?}",
                    entry.gesture.as_str(), expected_statuses, statuses
                ),
            );
        }
        let expected_source_refs = expected_gesture_source_refs(entry.gesture);
        if source_refs.as_slice() != expected_source_refs {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.sources"),
                format!(
                    "gesture `{}` requires canonical source refs {:?}, found {:?}",
                    entry.gesture.as_str(), expected_source_refs, source_refs
                ),
            );
        }
        let expected_tracking_refs = expected_gesture_tracking_refs(entry.gesture);
        if tracking_refs.as_slice() != expected_tracking_refs {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                format!("{path}.sources"),
                format!(
                    "gesture `{}` requires canonical tracking refs {:?}, found {:?}",
                    entry.gesture.as_str(), expected_tracking_refs, tracking_refs
                ),
            );
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
                reference_path,
                "tracking reference must not be empty",
            );
        } else {
            validator.require_repository_ref(&reference_path, value);
        }
        if !seen.insert(value.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidGestureAuthority,
                reference_path,
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
            "docs/a11y/scenario-corpus.md#steady_typing",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11yPaneFocusChange => (
            "docs/a11y/scenario-corpus.md#pane_focus_change",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11yDialogOpen => (
            "docs/a11y/scenario-corpus.md#dialog_open",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11ySelectionChange => (
            "docs/a11y/scenario-corpus.md#selection_change",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
        RendererLegacyScenarioId::A11yScrollPositionChange => (
            "docs/a11y/scenario-corpus.md#scroll_position_change",
            RendererLegacyMappingDisposition::Rejected,
            &LEGACY_TARGET_NONE,
        ),
    }
}

fn validate_corpus_references<'a>(
    references: &'a [RendererCorpusReference],
    validator: &mut Validator,
) -> BTreeMap<&'a str, &'a RendererCorpusReference> {
    if references.is_empty() {
        validator.error(
            RendererScenarioValidationCode::EmptyRequiredField,
            "$.corpus_references",
            "at least one canonical corpus reference is required",
        );
    }
    if references.len() > MAX_RENDERER_CORPUS_REFERENCES {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            "$.corpus_references",
            format!(
                "found {} corpus references (maximum {MAX_RENDERER_CORPUS_REFERENCES})",
                references.len()
            ),
        );
    }

    let mut index = BTreeMap::new();
    let mut authority_scopes = BTreeSet::new();
    for (position, reference) in references.iter().enumerate() {
        let path = format!("$.corpus_references[{position}]");
        validator.require_identifier(&format!("{path}.corpus_id"), &reference.corpus_id);
        validator.require_repository_ref(
            &format!("{path}.repository_ref"),
            &reference.repository_ref,
        );
        if index.insert(reference.corpus_id.as_str(), reference).is_some() {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.corpus_id"),
                format!("duplicate corpus id `{}`", reference.corpus_id),
            );
        }
        authority_scopes.insert(reference.authority_scope);
        match reference.authority_scope {
            RendererCorpusAuthorityScope::HeadlessVisualFixture => {
                if reference.authorizes_gesture_replay {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.authorizes_gesture_replay"),
                        "a visual fixture cannot authorize gesture replay",
                    );
                }
            }
            RendererCorpusAuthorityScope::HeadlessStateReplay => {
                if reference.authorizes_headless_checkpoint_comparison {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.authorizes_headless_checkpoint_comparison"),
                        "a state-replay source cannot authorize visual checkpoint comparison",
                    );
                }
            }
            RendererCorpusAuthorityScope::MetamorphicSignalOnly => {
                if reference.authorizes_gesture_replay
                    || reference.authorizes_headless_checkpoint_comparison
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        &path,
                        "a metamorphic-signal-only source cannot authorize replay or comparison",
                    );
                }
            }
            RendererCorpusAuthorityScope::ContractOnly => {
                if reference.authorizes_gesture_replay
                    || reference.authorizes_headless_checkpoint_comparison
                {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        &path,
                        "a contract-only source cannot authorize replay or comparison",
                    );
                }
            }
        }

        if reference.terminal_features.is_empty() {
            validator.error(
                RendererScenarioValidationCode::EmptyRequiredField,
                format!("{path}.terminal_features"),
                "a corpus reference must declare at least one terminal feature",
            );
        }
        let mut features = BTreeSet::new();
        for (feature_position, feature) in reference.terminal_features.iter().enumerate() {
            if !features.insert(*feature) {
                validator.error(
                    RendererScenarioValidationCode::InvalidCorpusReference,
                    format!("{path}.terminal_features[{feature_position}]"),
                    format!(
                        "duplicate terminal feature `{}` in one corpus reference",
                        feature.as_str()
                    ),
                );
            }
        }

        match reference.coverage_status {
            RendererCorpusCoverageStatus::Direct => {
                if reference.limitation.is_some() || reference.tracking_ref.is_some() {
                    validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        &path,
                        "direct corpus mapping must not carry limitation or tracking_ref",
                    );
                }
            }
            RendererCorpusCoverageStatus::Partial
            | RendererCorpusCoverageStatus::Gap
            | RendererCorpusCoverageStatus::PresentUnqualified => {
                match reference.limitation.as_deref() {
                    Some(limitation)
                        if !limitation.trim().is_empty()
                            && limitation.len() <= MAX_REASON_BYTES => {}
                    Some(limitation) if limitation.len() > MAX_REASON_BYTES => validator.error(
                        RendererScenarioValidationCode::LimitExceeded,
                        format!("{path}.limitation"),
                        format!(
                            "limitation is {} bytes (maximum {MAX_REASON_BYTES})",
                            limitation.len()
                        ),
                    ),
                    Some(_) | None => validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.limitation"),
                        "non-direct corpus mapping requires a non-empty limitation",
                    ),
                }
                match reference.tracking_ref.as_deref() {
                    Some(tracking_ref) if !tracking_ref.trim().is_empty() => {
                        validator.require_repository_ref(
                            &format!("{path}.tracking_ref"),
                            tracking_ref,
                        );
                    }
                    Some(_) | None => validator.error(
                        RendererScenarioValidationCode::InvalidCorpusReference,
                        format!("{path}.tracking_ref"),
                        "non-direct corpus mapping requires a non-empty tracking_ref",
                    ),
                }
                if let (Some(limitation), Some(tracking_ref)) =
                    (reference.limitation.as_deref(), reference.tracking_ref.as_deref())
                    && !limitation.trim().is_empty()
                    && !tracking_ref.trim().is_empty()
                {
                    validator.gap(
                        RendererScenarioGapCode::CorpusCoverageNotDirect,
                        &path,
                        limitation,
                        tracking_ref,
                    );
                }
            }
        }
    }
    for authority_scope in RendererCorpusAuthorityScope::ALL {
        if !authority_scopes.contains(&authority_scope) {
            validator.error(
                RendererScenarioValidationCode::InvalidCorpusReference,
                "$.corpus_references",
                format!(
                    "missing corpus authority scope `{:?}` required to keep source classes distinct",
                    authority_scope
                ),
            );
        }
    }
    index
}

fn validate_workloads<'a>(
    workloads: &'a [RendererWorkloadDefinition],
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
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
        if workload.duration_us == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{path}.duration_us"),
                "workload duration must be positive",
            );
        }
        if workload.event_count == 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{path}.event_count"),
                "workload event_count must be positive",
            );
        }
        if workload.output_bytes_per_second > MAX_RENDERER_OUTPUT_BYTES_PER_SECOND {
            validator.error(
                RendererScenarioValidationCode::LimitExceeded,
                format!("{path}.output_bytes_per_second"),
                format!(
                    "output rate {} exceeds maximum {MAX_RENDERER_OUTPUT_BYTES_PER_SECOND}",
                    workload.output_bytes_per_second
                ),
            );
        }
        if workload.corpus_ids.is_empty() {
            validator.error(
                RendererScenarioValidationCode::EmptyRequiredField,
                format!("{path}.corpus_ids"),
                "workload must reference at least one canonical corpus entry",
            );
        }
        let mut corpus_ids = BTreeSet::new();
        for (corpus_position, corpus_id) in workload.corpus_ids.iter().enumerate() {
            let corpus_path = format!("{path}.corpus_ids[{corpus_position}]");
            validator.require_identifier(&corpus_path, corpus_id);
            if !corpus_ids.insert(corpus_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    &corpus_path,
                    format!("duplicate corpus id `{corpus_id}` in workload"),
                );
            }
            if !corpus.contains_key(corpus_id.as_str()) {
                validator.error(
                    RendererScenarioValidationCode::DanglingReference,
                    corpus_path,
                    format!("undefined corpus id `{corpus_id}`"),
                );
            }
        }
    }
    index
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

    for (position, scenario) in catalog.scenarios.iter().enumerate() {
        let path = format!("$.scenarios[{position}]");
        validator.require_identifier(&format!("{path}.scenario_id"), &scenario.scenario_id);
        if !scenario_ids.insert(scenario.scenario_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{path}.scenario_id"),
                format!("duplicate scenario id `{}`", scenario.scenario_id),
            );
        }
        if !seeds.insert(scenario.seed) {
            validator.error(
                RendererScenarioValidationCode::InvalidSeed,
                format!("{path}.seed"),
                format!("duplicate scenario seed {}", scenario.seed),
            );
        }
        if let Some(previous_position) =
            coverage.insert((scenario.gesture, scenario.fleet_point), position)
        {
            validator.error(
                RendererScenarioValidationCode::DuplicateCoverageCell,
                &path,
                format!(
                    "coverage cell {}:{} duplicates $.scenarios[{previous_position}]",
                    scenario.gesture.as_str(),
                    scenario.fleet_point.as_str()
                ),
            );
        }

        let expected_id =
            expected_renderer_scenario_id(scenario.gesture, scenario.fleet_point);
        if scenario.scenario_id != expected_id {
            validator.error(
                RendererScenarioValidationCode::InvalidIdentifier,
                format!("{path}.scenario_id"),
                format!("expected canonical scenario id `{expected_id}`"),
            );
        }
        if scenario.pane_count != scenario.fleet_point.pane_count() {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.pane_count"),
                format!(
                    "fleet point {} requires exactly {} panes, found {}",
                    scenario.fleet_point.as_str(),
                    scenario.fleet_point.pane_count(),
                    scenario.pane_count
                ),
            );
        }
        validate_scenario_layout_and_refs(&path, scenario, validator);
        let expected_seed =
            expected_renderer_scenario_seed(scenario.gesture, scenario.fleet_point);
        if scenario.seed != expected_seed {
            validator.error(
                RendererScenarioValidationCode::InvalidSeed,
                format!("{path}.seed"),
                format!(
                    "coverage cell {}:{} requires seed {expected_seed}, found {}",
                    scenario.gesture.as_str(),
                    scenario.fleet_point.as_str(),
                    scenario.seed
                ),
            );
        }

        validator.require_identifier(&format!("{path}.workload_id"), &scenario.workload_id);
        let workload = index.workloads.get(scenario.workload_id.as_str()).copied();
        if workload.is_none() {
            validator.error(
                RendererScenarioValidationCode::DanglingReference,
                format!("{path}.workload_id"),
                format!("undefined workload id `{}`", scenario.workload_id),
            );
        } else {
            active_workloads.insert(scenario.workload_id.as_str());
        }

        validate_requirement_crosswalk(&path, scenario, workload, validator);

        validate_surface_state(
            &format!("{path}.initial_state"),
            &scenario.initial_state,
            &index.corpus,
            validator,
        );
        validate_surface_state(
            &format!("{path}.final_state"),
            &scenario.final_state,
            &index.corpus,
            validator,
        );
        validate_timeline(&path, scenario, workload, &index.corpus, validator);
        validate_expected_invariants(&path, scenario, validator);
        validate_visual_checkpoints(&path, scenario, validator);
        validate_capability_matrix(&path, scenario, validator);
        validate_gesture_transition(&path, scenario, workload, validator);
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

    validate_active_definitions_and_features(catalog, index, &active_workloads, validator);
}

fn validate_requirement_crosswalk(
    path: &str,
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
    validator: &mut Validator,
) {
    let expected = expected_requirement_ids(scenario);
    let mut seen = BTreeSet::new();
    for (position, binding) in scenario.requirement_bindings.iter().enumerate() {
        let binding_path = format!("{path}.requirement_bindings[{position}]");
        if !seen.insert(binding.requirement_id) {
            validator.error(
                RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                format!("{binding_path}.requirement_id"),
                format!(
                    "duplicate renderer requirement `{}`",
                    binding.requirement_id.as_str()
                ),
            );
        }
        validate_requirement_binding_shape(&binding_path, binding, scenario, validator);
        validate_requirement_predicate(
            &binding_path,
            binding.requirement_id,
            scenario,
            workload,
            validator,
        );
    }
    let actual = scenario
        .requirement_bindings
        .iter()
        .map(|binding| binding.requirement_id)
        .collect::<Vec<_>>();
    if actual != expected {
        let expected_labels = expected
            .iter()
            .map(|requirement| requirement.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let actual_labels = scenario
            .requirement_bindings
            .iter()
            .map(|binding| binding.requirement_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{path}.requirement_bindings"),
            format!(
                "expected canonical requirement order [{expected_labels}], found [{actual_labels}]"
            ),
        );
    }
}

fn validate_scenario_layout_and_refs(
    path: &str,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    for (field, actual, expected) in [
        (
            "tab_count",
            scenario.tab_count,
            scenario.fleet_point.tab_count(),
        ),
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
                format!(
                    "fleet point {} requires exact {field}={expected}, found {actual}",
                    scenario.fleet_point.as_str()
                ),
            );
        }
    }
    for (field, ordinal, count) in [
        (
            "focused_window_ordinal",
            scenario.focused_window_ordinal,
            scenario.window_count,
        ),
        (
            "active_tab_ordinal",
            scenario.active_tab_ordinal,
            scenario.tab_count,
        ),
        (
            "focused_pane_ordinal",
            scenario.focused_pane_ordinal,
            scenario.pane_count,
        ),
    ] {
        if ordinal >= count {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.{field}"),
                format!("ordinal {ordinal} is outside count {count}"),
            );
        }
    }
    for (field, repository_ref) in [
        ("topology_ref", &scenario.topology_ref),
        ("split_geometry_ref", &scenario.split_geometry_ref),
        ("pane_state_manifest_ref", &scenario.pane_state_manifest_ref),
    ] {
        validator.require_repository_ref(&format!("{path}.{field}"), repository_ref);
    }
    match (scenario.gesture, scenario.output_overlap_resize_mode) {
        (RendererGesture::OutputOverlapResize, None) => validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            format!("{path}.output_overlap_resize_mode"),
            "output-overlap resize requires explicit same_grid or grid_changing mode",
        ),
        (RendererGesture::OutputOverlapResize, Some(_)) | (_, None) => {}
        (_, Some(_)) => validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            format!("{path}.output_overlap_resize_mode"),
            "output_overlap_resize_mode is forbidden for other gestures",
        ),
    }
}

fn expected_requirement_ids(
    scenario: &RendererScenarioDefinition,
) -> Vec<RendererRequirementId> {
    let mut expected = Vec::new();
    if scenario.fleet_point == RendererFleetPoint::P200
        && matches!(
            scenario.gesture,
            RendererGesture::SameGridDrag
                | RendererGesture::GridChangingDrag
                | RendererGesture::OutputOverlapResize
        )
    {
        expected.push(RendererRequirementId::RqS1ResizeFps);
    }
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

fn validate_requirement_binding_shape(
    path: &str,
    binding: &RendererRequirementBinding,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    let expected_scope = match binding.requirement_id {
        RendererRequirementId::RqS11SnapBackSsim => {
            RendererRequirementScope::CheckpointPredicate
        }
        RendererRequirementId::RqS13SsimParityOracleCorpus => {
            RendererRequirementScope::ComparatorMechanismOnly
        }
        RendererRequirementId::RqS1ResizeFps
        | RendererRequirementId::RqS6HeavyBurstInputLatency
        | RendererRequirementId::RqS9ReflowLatency
        | RendererRequirementId::RqS10AtlasRebuildCount => {
            RendererRequirementScope::ExactScenarioPredicate
        }
    };
    if binding.scope != expected_scope {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            format!("{path}.scope"),
            format!(
                "requirement `{}` requires scope {:?}",
                binding.requirement_id.as_str(), expected_scope
            ),
        );
    }

    match binding.requirement_id {
        RendererRequirementId::RqS11SnapBackSsim => {
            let final_checkpoint = final_settle_checkpoint(scenario);
            if binding.checkpoint_id.as_deref()
                != final_checkpoint.map(|checkpoint| checkpoint.checkpoint_id.as_str())
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                    format!("{path}.checkpoint_id"),
                    "RQ-S11 must bind the final settle checkpoint",
                );
            }
        }
        RendererRequirementId::RqS1ResizeFps
        | RendererRequirementId::RqS6HeavyBurstInputLatency
        | RendererRequirementId::RqS9ReflowLatency
        | RendererRequirementId::RqS10AtlasRebuildCount
        | RendererRequirementId::RqS13SsimParityOracleCorpus => {
            if binding.checkpoint_id.is_some() {
                validator.error(
                    RendererScenarioValidationCode::InvalidRequirementCrosswalk,
                    format!("{path}.checkpoint_id"),
                    "only checkpoint-predicate RQ-S11 may carry checkpoint_id",
                );
            }
        }
    }
}

fn final_settle_checkpoint(
    scenario: &RendererScenarioDefinition,
) -> Option<&RendererVisualCheckpoint> {
    let final_ordinal = scenario.timeline.last()?.event_ordinal;
    scenario.visual_checkpoints.iter().find(|checkpoint| {
        checkpoint.phase == RendererTimelinePhase::Settle
            && checkpoint.event_ordinal == final_ordinal
    })
}

fn validate_requirement_predicate(
    path: &str,
    requirement_id: RendererRequirementId,
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
    validator: &mut Validator,
) {
    let predicate_valid = match requirement_id {
        RendererRequirementId::RqS1ResizeFps => {
            scenario.fleet_point == RendererFleetPoint::P200
                && matches!(
                    scenario.gesture,
                    RendererGesture::SameGridDrag
                        | RendererGesture::GridChangingDrag
                        | RendererGesture::OutputOverlapResize
                )
                && workload.map(|value| value.duration_us) == Some(5_000_000)
        }
        RendererRequirementId::RqS6HeavyBurstInputLatency => {
            scenario.gesture == RendererGesture::OutputOverlapResize
                && scenario.fleet_point == RendererFleetPoint::P050
                && workload.map(|value| value.output_bytes_per_second)
                    == Some(OUTPUT_OVERLAP_BYTES_PER_SECOND)
                && workload.map(|value| value.foreground_key_event_count) == Some(1)
        }
        RendererRequirementId::RqS9ReflowLatency => {
            scenario.gesture == RendererGesture::Reflow80To200
                && workload.map(|value| value.scrollback_lines_per_pane) == Some(1_000)
        }
        RendererRequirementId::RqS10AtlasRebuildCount => {
            rq_s10_pure_resize_predicate(scenario, workload)
        }
        RendererRequirementId::RqS11SnapBackSsim => final_settle_checkpoint(scenario)
            .is_some_and(|checkpoint| {
                checkpoint.native_capture_required
                    && checkpoint
                        .comparator_policy_refs
                        .iter()
                        .any(|reference| reference == RQ_S11_COMPARATOR_POLICY_REF)
            }),
        RendererRequirementId::RqS13SsimParityOracleCorpus => scenario
            .visual_checkpoints
            .iter()
            .all(|checkpoint| {
                checkpoint
                    .comparator_policy_refs
                    .iter()
                    .any(|reference| reference == RQ_S13_COMPARATOR_POLICY_REF)
            }),
    };
    if !predicate_valid {
        validator.error(
            RendererScenarioValidationCode::InvalidRequirementCrosswalk,
            path,
            format!(
                "scenario/workload/checkpoint does not satisfy exact predicate for `{}`",
                requirement_id.as_str()
            ),
        );
    }
}

fn rq_s10_pure_resize_predicate(
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
) -> bool {
    let Some(workload) = workload else {
        return false;
    };
    let resize_mutations = scenario
        .timeline
        .iter()
        .filter(|event| {
            matches!(event.action, RendererTimelineAction::SetWindowSize { .. })
        })
        .count();
    let mutations_are_pure_resize = scenario.timeline.iter().all(|event| {
        matches!(
            event.action,
            RendererTimelineAction::BeginGesture
                | RendererTimelineAction::SetWindowSize { .. }
                | RendererTimelineAction::SetRevisions { .. }
                | RendererTimelineAction::EndGesture
                | RendererTimelineAction::Settle
        )
    });
    scenario.gesture == RendererGesture::SameGridDrag
        && workload.resize_mutation_count == 100
        && resize_mutations == 100
        && workload.new_glyph_count == 0
        && scenario.initial_state.grid == scenario.final_state.grid
        && scenario.initial_state.font == scenario.final_state.font
        && scenario.initial_state.display.display_id == scenario.final_state.display.display_id
        && scenario.initial_state.display.dpi_milli == scenario.final_state.display.dpi_milli
        && scenario.initial_state.display.scale_factor_milli
            == scenario.final_state.display.scale_factor_milli
        && scenario.initial_state.terminal == scenario.final_state.terminal
        && mutations_are_pure_resize
}

fn validate_active_definitions_and_features(
    catalog: &RendererScenarioCatalog,
    index: &CatalogIndex<'_>,
    active_workloads: &BTreeSet<&str>,
    validator: &mut Validator,
) {
    for (position, workload) in catalog.workloads.iter().enumerate() {
        if !active_workloads.contains(workload.workload_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.workloads[{position}].workload_id"),
                format!(
                    "workload `{}` is not referenced by any scenario",
                    workload.workload_id
                ),
            );
        }
    }

    let mut active_corpus = BTreeSet::new();
    for workload_id in active_workloads {
        if let Some(workload) = index.workloads.get(workload_id) {
            for corpus_id in &workload.corpus_ids {
                if index.corpus.contains_key(corpus_id.as_str()) {
                    active_corpus.insert(corpus_id.as_str());
                }
            }
        }
    }
    for (position, reference) in catalog.corpus_references.iter().enumerate() {
        if !active_corpus.contains(reference.corpus_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::UnreferencedDefinition,
                format!("$.corpus_references[{position}].corpus_id"),
                format!(
                    "corpus `{}` is not referenced by an active workload",
                    reference.corpus_id
                ),
            );
        }
    }

    let mut active_features = BTreeSet::new();
    for corpus_id in active_corpus {
        if let Some(reference) = index.corpus.get(corpus_id) {
            active_features.extend(reference.terminal_features.iter().copied());
        }
    }
    let missing = RendererTerminalFeature::ALL
        .into_iter()
        .filter(|feature| !active_features.contains(feature))
        .map(RendererTerminalFeature::as_str)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        validator.error(
            RendererScenarioValidationCode::MissingTerminalFeatureCoverage,
            "$.corpus_references",
            format!(
                "active corpus union is missing terminal features: {}",
                missing.join(", ")
            ),
        );
    }
}

fn validate_surface_state(
    path: &str,
    state: &RendererSurfaceState,
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
    validator: &mut Validator,
) {
    if state.grid_revision == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.grid_revision"),
            "grid revision must be positive",
        );
    }
    if state.terminal_revision == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.terminal_revision"),
            "terminal revision must be positive",
        );
    }
    validator.require_repository_ref(
        &format!("{path}.renderer_config_ref"),
        &state.renderer_config_ref,
    );
    validate_grid_state(&format!("{path}.grid"), state.grid, validator);
    validator.require_identifier(&format!("{path}.font.font_id"), &state.font.font_id);
    validator.require_repository_ref(
        &format!("{path}.font.pinned_font_ref"),
        &state.font.pinned_font_ref,
    );
    if state.font.size_milli_points == 0
        || state.font.size_milli_points > MAX_FONT_SIZE_MILLI_POINTS
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.font.size_milli_points"),
            format!(
                "font size must be in 1..={MAX_FONT_SIZE_MILLI_POINTS}, found {}",
                state.font.size_milli_points
            ),
        );
    }
    if state.font.scale_milli == 0 || state.font.scale_milli > MAX_SCALE_FACTOR_MILLI {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.font.scale_milli"),
            format!(
                "font scale must be in 1..={MAX_SCALE_FACTOR_MILLI}, found {}",
                state.font.scale_milli
            ),
        );
    }
    validate_display_state(&format!("{path}.display"), &state.display, validator);
    validate_terminal_mode_state(
        &format!("{path}.terminal"),
        &state.terminal,
        state.grid,
        corpus,
        validator,
    );
}

fn validate_grid_state(path: &str, grid: RendererGridState, validator: &mut Validator) {
    if grid.columns == 0 || grid.columns > MAX_GRID_DIMENSION {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.columns"),
            format!(
                "columns must be in 1..={MAX_GRID_DIMENSION}, found {}",
                grid.columns
            ),
        );
    }
    if grid.rows == 0 || grid.rows > MAX_GRID_DIMENSION {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.rows"),
            format!(
                "rows must be in 1..={MAX_GRID_DIMENSION}, found {}",
                grid.rows
            ),
        );
    }
}

fn validate_display_state(path: &str, display: &RendererDisplayState, validator: &mut Validator) {
    validator.require_identifier(&format!("{path}.display_id"), &display.display_id);
    if display.dpi_milli == 0 || display.dpi_milli > MAX_DPI_MILLI {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.dpi_milli"),
            format!(
                "DPI must be in 1..={MAX_DPI_MILLI} milli-DPI, found {}",
                display.dpi_milli
            ),
        );
    }
    if display.scale_factor_milli == 0
        || display.scale_factor_milli > MAX_SCALE_FACTOR_MILLI
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.scale_factor_milli"),
            format!(
                "scale factor must be in 1..={MAX_SCALE_FACTOR_MILLI}, found {}",
                display.scale_factor_milli
            ),
        );
    }
    for (name, dimension) in [
        ("viewport_width_px", display.viewport_width_px),
        ("viewport_height_px", display.viewport_height_px),
    ] {
        if dimension == 0 || dimension > MAX_VIEWPORT_DIMENSION_PX {
            validator.error(
                RendererScenarioValidationCode::InvalidState,
                format!("{path}.{name}"),
                format!(
                    "viewport dimension must be in 1..={MAX_VIEWPORT_DIMENSION_PX}, found {dimension}"
                ),
            );
        }
    }
}

fn validate_terminal_mode_state(
    path: &str,
    terminal: &RendererTerminalModeState,
    grid: RendererGridState,
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
    validator: &mut Validator,
) {
    if terminal.viewport_top > 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.viewport_top"),
            "stable-row viewport_top must be non-positive",
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
        validate_cell_coordinate(
            &format!("{path}.selection.anchor"),
            anchor,
            grid,
            validator,
        );
        validate_cell_coordinate(
            &format!("{path}.selection.focus"),
            focus,
            grid,
            validator,
        );
    }
    if let RendererImeState::Composing {
        preedit_corpus_id,
        preedit_origin,
        caret,
    } = &terminal.ime
    {
        let corpus_path = format!("{path}.ime.preedit_corpus_id");
        validator.require_identifier(&corpus_path, preedit_corpus_id);
        match corpus.get(preedit_corpus_id.as_str()) {
            Some(reference)
                if !reference
                    .terminal_features
                    .contains(&RendererTerminalFeature::Ime) =>
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    corpus_path,
                    format!(
                        "corpus `{preedit_corpus_id}` does not declare the `ime` feature"
                    ),
                );
            }
            Some(_) => {}
            None => validator.error(
                RendererScenarioValidationCode::DanglingReference,
                corpus_path,
                format!("undefined IME corpus id `{preedit_corpus_id}`"),
            ),
        }
        validate_cell_coordinate(
            &format!("{path}.ime.preedit_origin"),
            *preedit_origin,
            grid,
            validator,
        );
        validate_cell_coordinate(
            &format!("{path}.ime.caret"),
            *caret,
            grid,
            validator,
        );
    }

    validate_geometry_reference_pair(
        path,
        "inline_image_count",
        terminal.inline_image_count,
        "inline_image_geometry_ref",
        terminal.inline_image_geometry_ref.as_deref(),
        validator,
    );
    validate_geometry_reference_pair(
        path,
        "active_hyperlink_count",
        terminal.active_hyperlink_count,
        "hyperlink_geometry_ref",
        terminal.hyperlink_geometry_ref.as_deref(),
        validator,
    );

    let accessibility = &terminal.accessibility_geometry;
    if accessibility.tree_revision == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.accessibility_geometry.tree_revision"),
            "accessibility tree revision must be positive",
        );
    }
    if accessibility.node_count == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.accessibility_geometry.node_count"),
            "accessibility geometry must declare at least one node",
        );
    }
    if let Some(focused_node_id) = &accessibility.focused_node_id {
        validator.require_identifier(
            &format!("{path}.accessibility_geometry.focused_node_id"),
            focused_node_id,
        );
    }
    if let Some(caret) = accessibility.caret_rect
        && (caret.width == 0 || caret.height == 0)
    {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.accessibility_geometry.caret_rect"),
            "caret rectangle width and height must be positive",
        );
    }
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
                "cell ({}, {}) is outside {}x{} grid",
                coordinate.row, coordinate.column, grid.rows, grid.columns
            ),
        );
    }
}

fn validate_geometry_reference_pair(
    path: &str,
    count_field: &str,
    count: u16,
    reference_field: &str,
    repository_ref: Option<&str>,
    validator: &mut Validator,
) {
    match (count, repository_ref) {
        (0, None) => {}
        (0, Some(_)) => validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.{reference_field}"),
            format!("{reference_field} must be absent when {count_field} is zero"),
        ),
        (_, Some(reference)) => {
            validator.require_repository_ref(&format!("{path}.{reference_field}"), reference);
        }
        (_, None) => validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{path}.{reference_field}"),
            format!("{reference_field} is required when {count_field} is positive"),
        ),
    }
}

fn validate_timeline(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.timeline");
    if scenario.timeline.len() < 4 {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            "timeline requires begin, at least one mutation, end, and settle events",
        );
    }
    if scenario.timeline.len() > MAX_RENDERER_TIMELINE_EVENTS {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            &path,
            format!(
                "found {} timeline events (maximum {MAX_RENDERER_TIMELINE_EVENTS})",
                scenario.timeline.len()
            ),
        );
    }

    let mut previous_time = None;
    let mut previous_phase = None;
    let mut begin_count = 0_usize;
    let mut mutation_count = 0_usize;
    let mut end_count = 0_usize;
    let mut settle_count = 0_usize;
    let mut replayed = scenario.initial_state.clone();

    for (position, event) in scenario.timeline.iter().enumerate() {
        let event_path = format!("{path}[{position}]");
        let expected_ordinal = u32::try_from(position).unwrap_or(u32::MAX);
        if event.event_ordinal != expected_ordinal {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.event_ordinal"),
                format!(
                    "expected contiguous ordinal {expected_ordinal}, found {}",
                    event.event_ordinal
                ),
            );
        }
        if position == 0 && event.at_us != 0 {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.at_us"),
                format!("event zero must start at 0 us, found {}", event.at_us),
            );
        }
        if let Some(previous_time) = previous_time
            && event.at_us <= previous_time
        {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.at_us"),
                format!(
                    "timestamp {} must be strictly greater than previous timestamp {previous_time}",
                    event.at_us
                ),
            );
        }
        previous_time = Some(event.at_us);

        if let Some(previous_phase) = previous_phase
            && event.phase.rank() < previous_phase
        {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.phase"),
                "timeline phase must not move backward",
            );
        }
        previous_phase = Some(event.phase.rank());

        let required_phase = validate_atomic_action_bundle(
            &format!("{event_path}.actions"),
            &event.actions,
            &mut replayed,
            corpus,
            validator,
        );
        match required_phase {
            Some(RendererTimelinePhase::Begin) => begin_count += 1,
            Some(RendererTimelinePhase::Mutation) => mutation_count += 1,
            Some(RendererTimelinePhase::End) => end_count += 1,
            Some(RendererTimelinePhase::Settle) => settle_count += 1,
            None => {}
        }
        if required_phase.is_some_and(|phase| phase != event.phase) {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                format!("{event_path}.phase"),
                format!(
                    "atomic action bundle requires phase {:?}, found {:?}",
                    required_phase.unwrap_or(RendererTimelinePhase::Mutation),
                    event.phase
                ),
            );
        }
    }

    if begin_count != 1 || end_count != 1 || settle_count != 1 || mutation_count == 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            format!(
                "timeline requires exactly one begin/end/settle and at least one mutation; found begin={begin_count}, mutation={mutation_count}, end={end_count}, settle={settle_count}"
            ),
        );
    }
    if !scenario.timeline.first().is_some_and(|event| {
        matches!(event.actions.as_slice(), [RendererTimelineAction::BeginGesture])
    }) {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            "first timeline action must be `begin_gesture`",
        );
    }
    if !scenario.timeline.last().is_some_and(|event| {
        matches!(event.actions.as_slice(), [RendererTimelineAction::Settle])
    }) {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            &path,
            "last timeline action must be `settle`",
        );
    }
    if replayed != scenario.final_state {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{scenario_path}.final_state"),
            "replaying the complete timeline from initial_state does not produce final_state",
        );
    }

    if let (Some(workload), Some(last_event)) = (workload, scenario.timeline.last())
        && workload.duration_us != last_event.at_us
    {
        validator.error(
            RendererScenarioValidationCode::InvalidWorkload,
            format!("{scenario_path}.workload_id"),
            format!(
                "workload duration {} us does not match settle timestamp {} us",
                workload.duration_us, last_event.at_us
            ),
        );
    }
    if let Some(workload) = workload {
        let timeline_event_count = u32::try_from(scenario.timeline.len()).ok();
        if timeline_event_count != Some(workload.event_count) {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{scenario_path}.workload_id"),
                format!(
                    "workload event_count {} does not match timeline length {}",
                    workload.event_count,
                    scenario.timeline.len()
                ),
            );
        }
        let resize_event_count = scenario
            .timeline
            .iter()
            .filter(|event| {
                event.actions.iter().any(|action| {
                    matches!(
                        action,
                        RendererTimelineAction::SetWindowSize { .. }
                            | RendererTimelineAction::SetGrid { .. }
                    )
                })
            })
            .count();
        if u32::try_from(resize_event_count).ok() != Some(workload.resize_mutation_count) {
            validator.error(
                RendererScenarioValidationCode::InvalidWorkload,
                format!("{scenario_path}.workload_id"),
                format!(
                    "workload resize_mutation_count {} does not match {resize_event_count} resize events",
                    workload.resize_mutation_count
                ),
            );
        }
        validate_viewport_top_bound(
            scenario_path,
            &scenario.initial_state,
            workload,
            "initial_state",
            validator,
        );
        validate_viewport_top_bound(
            scenario_path,
            &scenario.final_state,
            workload,
            "final_state",
            validator,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RendererTimelineActionKind {
    BeginGesture,
    SetOutputRate,
    SetWindowSize,
    SetGrid,
    SetFontScale,
    SetQualityMode,
    MoveToDisplay,
    ForegroundKey,
    SetTerminalMode,
    SetRevisions,
    EndGesture,
    Settle,
}

const fn timeline_action_kind(action: &RendererTimelineAction) -> RendererTimelineActionKind {
    match action {
        RendererTimelineAction::BeginGesture => RendererTimelineActionKind::BeginGesture,
        RendererTimelineAction::SetOutputRate { .. } => {
            RendererTimelineActionKind::SetOutputRate
        }
        RendererTimelineAction::SetWindowSize { .. } => {
            RendererTimelineActionKind::SetWindowSize
        }
        RendererTimelineAction::SetGrid { .. } => RendererTimelineActionKind::SetGrid,
        RendererTimelineAction::SetFontScale { .. } => {
            RendererTimelineActionKind::SetFontScale
        }
        RendererTimelineAction::SetQualityMode { .. } => {
            RendererTimelineActionKind::SetQualityMode
        }
        RendererTimelineAction::MoveToDisplay { .. } => {
            RendererTimelineActionKind::MoveToDisplay
        }
        RendererTimelineAction::ForegroundKey { .. } => {
            RendererTimelineActionKind::ForegroundKey
        }
        RendererTimelineAction::SetTerminalMode { .. } => {
            RendererTimelineActionKind::SetTerminalMode
        }
        RendererTimelineAction::SetRevisions { .. } => {
            RendererTimelineActionKind::SetRevisions
        }
        RendererTimelineAction::EndGesture => RendererTimelineActionKind::EndGesture,
        RendererTimelineAction::Settle => RendererTimelineActionKind::Settle,
    }
}

fn validate_atomic_action_bundle(
    path: &str,
    actions: &[RendererTimelineAction],
    state: &mut RendererSurfaceState,
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
    validator: &mut Validator,
) -> Option<RendererTimelinePhase> {
    let errors_before_bundle = validator.errors.len();
    if actions.is_empty() {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            path,
            "atomic action bundle must not be empty",
        );
        return None;
    }
    if actions.len() > MAX_RENDERER_ACTIONS_PER_EVENT {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            path,
            format!(
                "atomic action bundle contains {} actions (maximum {MAX_RENDERER_ACTIONS_PER_EVENT})",
                actions.len()
            ),
        );
    }

    let boundary_action = actions.iter().find_map(|action| match action {
        RendererTimelineAction::BeginGesture => Some(RendererTimelinePhase::Begin),
        RendererTimelineAction::EndGesture => Some(RendererTimelinePhase::End),
        RendererTimelineAction::Settle => Some(RendererTimelinePhase::Settle),
        _ => None,
    });
    if boundary_action.is_some() && actions.len() != 1 {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            path,
            "gesture_begin, gesture_end, and settle are exclusive action bundles",
        );
    }

    let mut kinds = BTreeSet::new();
    let mut previous_kind = None;
    for (position, action) in actions.iter().enumerate() {
        let action_path = format!("{path}[{position}]");
        let kind = timeline_action_kind(action);
        if !kinds.insert(kind) {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                action_path,
                format!("duplicate atomic action kind {kind:?}"),
            );
        }
        if previous_kind.is_some_and(|previous| kind <= previous) {
            validator.error(
                RendererScenarioValidationCode::InvalidTimeline,
                action_path,
                "atomic actions must appear once in canonical state-application order",
            );
        }
        previous_kind = Some(kind);
    }
    if kinds.contains(&RendererTimelineActionKind::SetWindowSize)
        && kinds.contains(&RendererTimelineActionKind::MoveToDisplay)
    {
        validator.error(
            RendererScenarioValidationCode::InvalidTimeline,
            path,
            "SetWindowSize and MoveToDisplay both replace viewport dimensions and cannot share a bundle",
        );
    }

    let mut candidate = state.clone();
    for (position, action) in actions.iter().enumerate() {
        validate_and_replay_action(
            &format!("{path}[{position}]"),
            action,
            &mut candidate,
            corpus,
            validator,
        );
    }
    if validator.errors.len() == errors_before_bundle {
        *state = candidate;
    }

    boundary_action.or(Some(RendererTimelinePhase::Mutation))
}

fn validate_viewport_top_bound(
    scenario_path: &str,
    state: &RendererSurfaceState,
    workload: &RendererWorkloadDefinition,
    state_field: &str,
    validator: &mut Validator,
) {
    let oldest_available_row = -i64::from(workload.scrollback_lines_per_pane);
    if state.terminal.viewport_top < oldest_available_row {
        validator.error(
            RendererScenarioValidationCode::InvalidState,
            format!("{scenario_path}.{state_field}.terminal.viewport_top"),
            format!(
                "viewport_top {} precedes oldest available scrollback row {oldest_available_row}",
                state.terminal.viewport_top
            ),
        );
    }
}

fn validate_expected_invariants(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.expected_invariants");
    if scenario.expected_invariants.len() != REQUIRED_RENDERER_INVARIANT_COUNT {
        validator.error(
            RendererScenarioValidationCode::InvalidInvariant,
            &path,
            format!(
                "expected exactly {REQUIRED_RENDERER_INVARIANT_COUNT} invariants, found {}",
                scenario.expected_invariants.len()
            ),
        );
    }
    if scenario.expected_invariants.len() > MAX_RENDERER_EXPECTED_INVARIANTS {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            &path,
            format!(
                "found {} invariants (maximum {MAX_RENDERER_EXPECTED_INVARIANTS})",
                scenario.expected_invariants.len()
            ),
        );
    }

    let mut ids = BTreeSet::new();
    for (position, invariant) in scenario.expected_invariants.iter().enumerate() {
        let invariant_path = format!("{path}[{position}]");
        validator.require_identifier(
            &format!("{invariant_path}.invariant_id"),
            &invariant.invariant_id,
        );
        if !ids.insert(invariant.invariant_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::DuplicateId,
                format!("{invariant_path}.invariant_id"),
                format!("duplicate invariant id `{}`", invariant.invariant_id),
            );
        }
        validator.require_repository_ref(
            &format!("{invariant_path}.oracle_ref"),
            &invariant.oracle_ref,
        );
        let expected_phases = expected_invariant_phases(&invariant.invariant_id);
        if expected_phases.is_empty() {
            validator.error(
                RendererScenarioValidationCode::InvalidInvariant,
                format!("{invariant_path}.invariant_id"),
                format!(
                    "invariant `{}` is outside the closed version-1 inventory",
                    invariant.invariant_id
                ),
            );
        } else if invariant.applicable_phases.as_slice() != expected_phases {
            validator.error(
                RendererScenarioValidationCode::InvalidInvariant,
                format!("{invariant_path}.applicable_phases"),
                format!(
                    "invariant `{}` requires canonical applicable phases {:?}, found {:?}",
                    invariant.invariant_id, expected_phases, invariant.applicable_phases
                ),
            );
        }
    }
    let actual_ids = scenario
        .expected_invariants
        .iter()
        .map(|invariant| invariant.invariant_id.as_str())
        .collect::<Vec<_>>();
    if actual_ids.as_slice() != REQUIRED_RENDERER_INVARIANT_IDS {
        validator.error(
            RendererScenarioValidationCode::InvalidInvariant,
            &path,
            format!(
                "invariants must appear exactly once in canonical order: {}",
                REQUIRED_RENDERER_INVARIANT_IDS.join(", ")
            ),
        );
    }
}

const INVARIANT_SAFETY_PHASES: [RendererTimelinePhase; 3] = [
    RendererTimelinePhase::Begin,
    RendererTimelinePhase::Mutation,
    RendererTimelinePhase::Settle,
];
const INVARIANT_SETTLE_PHASE: [RendererTimelinePhase; 1] =
    [RendererTimelinePhase::Settle];
const INVARIANT_NO_PHASES: [RendererTimelinePhase; 0] = [];

fn expected_invariant_phases(invariant_id: &str) -> &'static [RendererTimelinePhase] {
    match invariant_id {
        "no_blank_frame_after_nonblank"
        | "no_stale_full_frame_reuse"
        | "coherent_grid_terminal_revision"
        | "anchors_in_bounds"
        | "reflow_logical_line_identity"
        | "alternate_screen_isolation"
        | "accessibility_focus_geometry" => &INVARIANT_SAFETY_PHASES,
        "final_state_convergence" => &INVARIANT_SETTLE_PHASE,
        _ => &INVARIANT_NO_PHASES,
    }
}

fn validate_visual_checkpoints(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    let path = format!("{scenario_path}.visual_checkpoints");
    if scenario.visual_checkpoints.len() < 3 {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "at least begin, mutation, and settle checkpoints are required",
        );
    }
    if scenario.visual_checkpoints.len() > MAX_RENDERER_CHECKPOINTS {
        validator.error(
            RendererScenarioValidationCode::LimitExceeded,
            &path,
            format!(
                "found {} checkpoints (maximum {MAX_RENDERER_CHECKPOINTS})",
                scenario.visual_checkpoints.len()
            ),
        );
    }

    let invariants = scenario
        .expected_invariants
        .iter()
        .map(|invariant| (invariant.invariant_id.as_str(), invariant))
        .collect::<BTreeMap<_, _>>();
    let mut checkpoint_ids = BTreeSet::new();
    let mut previous_ordinal = None;
    let mut has_begin = false;
    let mut has_intermediate = false;
    let mut has_settle = false;
    let mut draft_source_count = 0_usize;
    let mut standard_ground_truth_count = 0_usize;
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
                format!("duplicate checkpoint id `{}`", checkpoint.checkpoint_id),
            );
        }
        if let Some(previous_ordinal) = previous_ordinal
            && checkpoint.event_ordinal <= previous_ordinal
        {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.event_ordinal"),
                "checkpoint ordinals must be strictly increasing",
            );
        }
        previous_ordinal = Some(checkpoint.event_ordinal);

        match scenario.timeline.get(checkpoint.event_ordinal as usize) {
            Some(event) if event.phase != checkpoint.phase => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &checkpoint_path,
                format!(
                    "checkpoint phase {:?} does not match timeline phase {:?} at ordinal {}",
                    checkpoint.phase, event.phase, checkpoint.event_ordinal
                ),
            ),
            Some(_) => {
                has_begin |= checkpoint.phase == RendererTimelinePhase::Begin;
                has_intermediate |= checkpoint.phase == RendererTimelinePhase::Mutation;
                has_settle |= checkpoint.phase == RendererTimelinePhase::Settle;
            }
            None => validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.event_ordinal"),
                format!(
                    "event ordinal {} is outside timeline length {}",
                    checkpoint.event_ordinal,
                    scenario.timeline.len()
                ),
            ),
        }

        if !checkpoint.native_capture_required {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                format!("{checkpoint_path}.native_capture_required"),
                "every version-1 gesture checkpoint requires native capture",
            );
        }
        validate_checkpoint_role(
            &checkpoint_path,
            checkpoint,
            scenario,
            &mut draft_source_count,
            &mut standard_ground_truth_count,
            validator,
        );

        validate_checkpoint_invariants(
            &checkpoint_path,
            checkpoint,
            &invariants,
            validator,
        );
        for (field, repository_ref) in [
            ("state_oracle_ref", &checkpoint.state_oracle_ref),
            ("visual_oracle_ref", &checkpoint.visual_oracle_ref),
            (
                "accessibility_oracle_ref",
                &checkpoint.accessibility_oracle_ref,
            ),
        ] {
            validator.require_repository_ref(
                &format!("{checkpoint_path}.{field}"),
                repository_ref,
            );
        }
        validate_checkpoint_comparator_policies(
            &checkpoint_path,
            checkpoint,
            scenario,
            validator,
        );
        validate_native_checkpoint_availability(
            &checkpoint_path,
            checkpoint,
            scenario,
            validator,
        );
    }

    if !scenario.visual_checkpoints.first().is_some_and(|checkpoint| {
        checkpoint.phase == RendererTimelinePhase::Begin
            && checkpoint.event_ordinal == 0
            && checkpoint.role == RendererCheckpointRole::InitialBaseline
    }) {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "first checkpoint must be the initial baseline at begin ordinal zero",
        );
    }
    if let (Some(last), Some(last_event)) =
        (scenario.visual_checkpoints.last(), scenario.timeline.last())
        && (last.event_ordinal != last_event.event_ordinal
            || last.phase != RendererTimelinePhase::Settle)
    {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            format!("{path}[{}].event_ordinal", scenario.visual_checkpoints.len() - 1),
            format!(
                "last checkpoint must bind settle event ordinal {}",
                last_event.event_ordinal
            ),
        );
    }
    if !has_begin || !has_settle {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "checkpoint sequence requires exact begin and settle placement",
        );
    }
    if !has_intermediate {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "at least one checkpoint must bind an intermediate mutation event",
        );
    }
    if gesture_is_live_resize(scenario.gesture) {
        if draft_source_count != 1 || standard_ground_truth_count != 1 {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &path,
                format!(
                    "RQ-S11 scenario requires exactly one draft source and one standard ground truth; found draft={draft_source_count}, standard={standard_ground_truth_count}"
                ),
            );
        }
    } else if draft_source_count != 0 || standard_ground_truth_count != 0 {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "non-RQ-S11 scenario cannot carry snap-back checkpoint roles",
        );
    }
}

fn validate_checkpoint_role(
    path: &str,
    checkpoint: &RendererVisualCheckpoint,
    scenario: &RendererScenarioDefinition,
    draft_source_count: &mut usize,
    standard_ground_truth_count: &mut usize,
    validator: &mut Validator,
) {
    let final_ordinal = scenario.timeline.last().map(|event| event.event_ordinal);
    match checkpoint.role {
        RendererCheckpointRole::InitialBaseline => {
            if checkpoint.phase != RendererTimelinePhase::Begin || checkpoint.event_ordinal != 0 {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{path}.role"),
                    "initial_baseline role is valid only at begin ordinal zero",
                );
            }
        }
        RendererCheckpointRole::LiveResizeDraft => {
            *draft_source_count += 1;
            if !gesture_is_live_resize(scenario.gesture)
                || checkpoint.phase != RendererTimelinePhase::Mutation
                || quality_mode_at_event(scenario, checkpoint.event_ordinal)
                    != Some(RendererQualityMode::Draft)
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{path}.role"),
                    "draft_live_resize_source requires an RQ-S11 mutation checkpoint in Draft mode",
                );
            }
        }
        RendererCheckpointRole::Intermediate => {
            if checkpoint.phase != RendererTimelinePhase::Mutation {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{path}.role"),
                    "intermediate role requires a mutation checkpoint",
                );
            }
        }
        RendererCheckpointRole::FinalStandardGroundTruth => {
            *standard_ground_truth_count += 1;
            if !gesture_is_live_resize(scenario.gesture)
                || checkpoint.phase != RendererTimelinePhase::Settle
                || Some(checkpoint.event_ordinal) != final_ordinal
                || quality_mode_at_event(scenario, checkpoint.event_ordinal)
                    != Some(RendererQualityMode::Standard)
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{path}.role"),
                    "standard_ground_truth requires the final RQ-S11 settle checkpoint in Standard mode",
                );
            }
        }
        RendererCheckpointRole::FinalState => {
            if gesture_is_live_resize(scenario.gesture)
                || checkpoint.phase != RendererTimelinePhase::Settle
                || Some(checkpoint.event_ordinal) != final_ordinal
            {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    format!("{path}.role"),
                    "final_state role is reserved for the final settle checkpoint of a non-RQ-S11 scenario",
                );
            }
        }
    }
}

fn quality_mode_at_event(
    scenario: &RendererScenarioDefinition,
    event_ordinal: u32,
) -> Option<RendererQualityMode> {
    let mut mode = scenario.initial_state.quality_mode;
    for event in scenario
        .timeline
        .iter()
        .filter(|event| event.event_ordinal <= event_ordinal)
    {
        for action in &event.actions {
            if let RendererTimelineAction::SetQualityMode { mode: next } = action {
                mode = *next;
            }
        }
    }
    usize::try_from(event_ordinal)
        .ok()
        .and_then(|position| scenario.timeline.get(position))
        .map(|_| mode)
}

fn validate_checkpoint_comparator_policies(
    checkpoint_path: &str,
    checkpoint: &RendererVisualCheckpoint,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    let path = format!("{checkpoint_path}.comparator_policy_refs");
    if checkpoint.comparator_policy_refs.is_empty() {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "checkpoint must bind at least one external comparator policy",
        );
    }
    let mut refs = BTreeSet::new();
    for (position, policy_ref) in checkpoint.comparator_policy_refs.iter().enumerate() {
        let policy_path = format!("{path}[{position}]");
        validator.require_repository_ref(&policy_path, policy_ref);
        if !refs.insert(policy_ref.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                policy_path,
                format!("duplicate comparator policy reference `{policy_ref}`"),
            );
        }
    }

    let has_s11 = scenario.requirement_bindings.iter().any(|binding| {
        binding.requirement_id == RendererRequirementId::RqS11SnapBackSsim
    });
    let is_s11_ground_truth = checkpoint.role == RendererCheckpointRole::FinalStandardGroundTruth;
    if has_s11 && is_s11_ground_truth && !refs.contains(RQ_S11_COMPARATOR_POLICY_REF) {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            format!(
                "RQ-S11 standard ground truth requires comparator policy `{RQ_S11_COMPARATOR_POLICY_REF}`"
            ),
        );
    }
    if (!has_s11 || !is_s11_ground_truth) && refs.contains(RQ_S11_COMPARATOR_POLICY_REF) {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "RQ-S11 comparator policy is reserved for its final standard ground-truth checkpoint",
        );
    }

    let has_s13 = scenario.requirement_bindings.iter().any(|binding| {
        binding.requirement_id == RendererRequirementId::RqS13SsimParityOracleCorpus
    });
    if has_s13 && !refs.contains(RQ_S13_COMPARATOR_POLICY_REF) {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            format!(
                "RQ-S13 comparator mechanism requires policy `{RQ_S13_COMPARATOR_POLICY_REF}`"
            ),
        );
    }
}

fn validate_checkpoint_invariants(
    checkpoint_path: &str,
    checkpoint: &RendererVisualCheckpoint,
    invariants: &BTreeMap<&str, &RendererExpectedInvariant>,
    validator: &mut Validator,
) {
    let path = format!("{checkpoint_path}.expected_invariant_ids");
    if checkpoint.expected_invariant_ids.is_empty() {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            "checkpoint must bind at least one expected invariant",
        );
    }
    let mut ids = BTreeSet::new();
    for (position, invariant_id) in checkpoint.expected_invariant_ids.iter().enumerate() {
        let invariant_path = format!("{path}[{position}]");
        validator.require_identifier(&invariant_path, invariant_id);
        if !ids.insert(invariant_id.as_str()) {
            validator.error(
                RendererScenarioValidationCode::InvalidCheckpoint,
                &invariant_path,
                format!("duplicate checkpoint invariant `{invariant_id}`"),
            );
        }
        match invariants.get(invariant_id.as_str()) {
            Some(invariant) if !invariant.applicable_phases.contains(&checkpoint.phase) => {
                validator.error(
                    RendererScenarioValidationCode::InvalidCheckpoint,
                    invariant_path,
                    format!(
                        "invariant `{invariant_id}` is not applicable to checkpoint phase {:?}",
                        checkpoint.phase
                    ),
                );
            }
            Some(_) => {}
            None => validator.error(
                RendererScenarioValidationCode::DanglingReference,
                invariant_path,
                format!("undefined expected invariant `{invariant_id}`"),
            ),
        }
    }
    let expected_ids = invariants
        .values()
        .filter(|invariant| invariant.applicable_phases.contains(&checkpoint.phase))
        .map(|invariant| invariant.invariant_id.as_str())
        .collect::<Vec<_>>();
    let canonical_expected_ids = REQUIRED_RENDERER_INVARIANT_IDS
        .iter()
        .copied()
        .filter(|invariant_id| expected_ids.contains(invariant_id))
        .collect::<Vec<_>>();
    let actual_ids = checkpoint
        .expected_invariant_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if actual_ids != canonical_expected_ids {
        validator.error(
            RendererScenarioValidationCode::InvalidCheckpoint,
            &path,
            format!(
                "checkpoint must bind the complete canonical invariant set for phase {:?}: {:?}",
                checkpoint.phase, canonical_expected_ids
            ),
        );
    }
}

fn validate_capability_matrix(
    scenario_path: &str,
    scenario: &RendererScenarioDefinition,
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

    let mut seen = BTreeSet::new();
    for (position, binding) in scenario.capabilities.iter().enumerate() {
        let binding_path = format!("{path}[{position}]");
        if !seen.insert(binding.capability) {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                format!("{binding_path}.capability"),
                format!(
                    "duplicate capability row `{}`",
                    binding.capability.as_str()
                ),
            );
        }
        if capability_is_inherently_required(scenario, binding.capability)
            && binding.requirement != RendererCapabilityRequirement::Required
        {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                format!("{binding_path}.requirement"),
                format!(
                    "capability `{}` is inherently required by this scenario",
                    binding.capability.as_str()
                ),
            );
        }
        match &binding.availability {
            RendererCapabilityAvailability::DeclaredAvailable => {}
            RendererCapabilityAvailability::Unsupported {
                reason,
                tracking_ref,
            } => {
                if reason.trim().is_empty() {
                    validator.error(
                        RendererScenarioValidationCode::MissingUnsupportedCapabilityReason,
                        format!("{binding_path}.availability.reason"),
                        "unsupported capability requires a non-empty reason",
                    );
                } else if reason.len() > MAX_REASON_BYTES {
                    validator.error(
                        RendererScenarioValidationCode::LimitExceeded,
                        format!("{binding_path}.availability.reason"),
                        format!(
                            "unsupported reason is {} bytes (maximum {MAX_REASON_BYTES})",
                            reason.len()
                        ),
                    );
                }
                if tracking_ref.trim().is_empty() {
                    validator.error(
                        RendererScenarioValidationCode::MissingUnsupportedCapabilityReason,
                        format!("{binding_path}.availability.tracking_ref"),
                        "unsupported capability requires a non-empty tracking_ref",
                    );
                } else {
                    validator.require_repository_ref(
                        &format!("{binding_path}.availability.tracking_ref"),
                        tracking_ref,
                    );
                }
                if binding.requirement == RendererCapabilityRequirement::Required
                    && !reason.trim().is_empty()
                    && !tracking_ref.trim().is_empty()
                {
                    validator.gap(
                        RendererScenarioGapCode::RequiredCapabilityUnavailable,
                        &binding_path,
                        reason,
                        tracking_ref,
                    );
                }
            }
        }
    }
    for capability in RendererCapability::ALL {
        if !seen.contains(&capability) {
            validator.error(
                RendererScenarioValidationCode::InvalidCapabilityMatrix,
                &path,
                format!("missing capability row `{}`", capability.as_str()),
            );
        }
    }
}

fn capability_is_inherently_required(
    scenario: &RendererScenarioDefinition,
    capability: RendererCapability,
) -> bool {
    match capability {
        RendererCapability::HeadlessStateOracle
        | RendererCapability::GpuVisualCapture
        | RendererCapability::AccessibilityGeometry => true,
        RendererCapability::NativeWindowGesture => matches!(
            scenario.gesture,
            RendererGesture::SameGridDrag
                | RendererGesture::GridChangingDrag
                | RendererGesture::Reflow80To200
                | RendererGesture::Reflow200To80
                | RendererGesture::OutputOverlapResize
        ),
        RendererCapability::NativeDisplayMove => {
            scenario.gesture == RendererGesture::DpiDisplayMove
        }
        RendererCapability::ImeComposition => scenario_uses_ime(scenario),
        RendererCapability::ImageProtocol => scenario_uses_images(scenario),
    }
}

fn scenario_uses_ime(scenario: &RendererScenarioDefinition) -> bool {
    !matches!(scenario.initial_state.terminal.ime, RendererImeState::Inactive)
        || !matches!(scenario.final_state.terminal.ime, RendererImeState::Inactive)
        || scenario.timeline.iter().any(|event| {
            matches!(
                &event.action,
                RendererTimelineAction::SetTerminalMode { terminal }
                    if !matches!(terminal.ime, RendererImeState::Inactive)
            )
        })
}

fn scenario_uses_images(scenario: &RendererScenarioDefinition) -> bool {
    scenario.initial_state.terminal.inline_image_count > 0
        || scenario.final_state.terminal.inline_image_count > 0
        || scenario.timeline.iter().any(|event| {
            matches!(
                &event.action,
                RendererTimelineAction::SetTerminalMode { terminal }
                    if terminal.inline_image_count > 0
            )
        })
}

fn validate_native_checkpoint_availability(
    checkpoint_path: &str,
    checkpoint: &RendererVisualCheckpoint,
    scenario: &RendererScenarioDefinition,
    validator: &mut Validator,
) {
    if !checkpoint.native_capture_required {
        return;
    }
    let mut relevant = vec![RendererCapability::GpuVisualCapture];
    if capability_is_inherently_required(scenario, RendererCapability::NativeWindowGesture) {
        relevant.push(RendererCapability::NativeWindowGesture);
    }
    if capability_is_inherently_required(scenario, RendererCapability::NativeDisplayMove) {
        relevant.push(RendererCapability::NativeDisplayMove);
    }
    for capability in relevant {
        if let Some(RendererCapabilityBinding {
            availability:
                RendererCapabilityAvailability::Unsupported {
                    reason,
                    tracking_ref,
                },
            ..
        }) = scenario
            .capabilities
            .iter()
            .find(|binding| binding.capability == capability)
            && !reason.trim().is_empty()
            && !tracking_ref.trim().is_empty()
        {
            validator.gap(
                RendererScenarioGapCode::NativeCaptureUnavailable,
                checkpoint_path,
                format!(
                    "native checkpoint requires unavailable capability `{}`: {reason}",
                    capability.as_str()
                ),
                tracking_ref,
            );
        }
    }
}

fn validate_and_replay_action(
    path: &str,
    action: &RendererTimelineAction,
    state: &mut RendererSurfaceState,
    corpus: &BTreeMap<&str, &RendererCorpusReference>,
    validator: &mut Validator,
) {
    match action {
        RendererTimelineAction::BeginGesture
        | RendererTimelineAction::EndGesture
        | RendererTimelineAction::Settle => {}
        RendererTimelineAction::SetWindowSize {
            width_px,
            height_px,
        } => {
            for (name, dimension) in [("width_px", *width_px), ("height_px", *height_px)] {
                if dimension == 0 || dimension > MAX_VIEWPORT_DIMENSION_PX {
                    validator.error(
                        RendererScenarioValidationCode::InvalidState,
                        format!("{path}.{name}"),
                        format!(
                            "window dimension must be in 1..={MAX_VIEWPORT_DIMENSION_PX}, found {dimension}"
                        ),
                    );
                }
            }
            state.display.viewport_width_px = *width_px;
            state.display.viewport_height_px = *height_px;
        }
        RendererTimelineAction::SetGrid { columns, rows } => {
            let grid = RendererGridState {
                columns: *columns,
                rows: *rows,
            };
            validate_grid_state(path, grid, validator);
            state.grid = grid;
        }
        RendererTimelineAction::SetFontScale { scale_milli } => {
            if *scale_milli == 0 || *scale_milli > MAX_SCALE_FACTOR_MILLI {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.scale_milli"),
                    format!(
                        "font scale must be in 1..={MAX_SCALE_FACTOR_MILLI}, found {scale_milli}"
                    ),
                );
            }
            state.font.scale_milli = *scale_milli;
        }
        RendererTimelineAction::SetQualityMode { mode } => {
            state.quality_mode = *mode;
        }
        RendererTimelineAction::MoveToDisplay { display } => {
            validate_display_state(&format!("{path}.display"), display, validator);
            state.display.clone_from(display);
        }
        RendererTimelineAction::SetOutputRate { bytes_per_second } => {
            if *bytes_per_second > MAX_RENDERER_OUTPUT_BYTES_PER_SECOND {
                validator.error(
                    RendererScenarioValidationCode::LimitExceeded,
                    format!("{path}.bytes_per_second"),
                    format!(
                        "output rate {bytes_per_second} exceeds maximum {MAX_RENDERER_OUTPUT_BYTES_PER_SECOND}"
                    ),
                );
            }
        }
        RendererTimelineAction::ForegroundKey { count } => {
            if *count == 0 {
                validator.error(
                    RendererScenarioValidationCode::InvalidTimeline,
                    format!("{path}.count"),
                    "foreground key action count must be positive",
                );
            }
        }
        RendererTimelineAction::SetTerminalMode { terminal } => {
            validate_terminal_mode_state(path, terminal, state.grid, corpus, validator);
            state.terminal.clone_from(terminal);
        }
        RendererTimelineAction::SetRevisions {
            grid_revision,
            terminal_revision,
        } => {
            if *grid_revision <= state.grid_revision {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.grid_revision"),
                    format!(
                        "grid revision must advance beyond {}, found {grid_revision}",
                        state.grid_revision
                    ),
                );
            }
            if *terminal_revision <= state.terminal_revision {
                validator.error(
                    RendererScenarioValidationCode::InvalidState,
                    format!("{path}.terminal_revision"),
                    format!(
                        "terminal revision must advance beyond {}, found {terminal_revision}",
                        state.terminal_revision
                    ),
                );
            }
            state.grid_revision = *grid_revision;
            state.terminal_revision = *terminal_revision;
        }
    }
}

fn validate_gesture_transition(
    path: &str,
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
    validator: &mut Validator,
) {
    let initial = &scenario.initial_state;
    let final_state = &scenario.final_state;
    let has_window_resize = scenario.timeline.iter().any(|event| {
        matches!(event.action, RendererTimelineAction::SetWindowSize { .. })
    });
    let has_grid_change = scenario
        .timeline
        .iter()
        .any(|event| matches!(event.action, RendererTimelineAction::SetGrid { .. }));
    let has_font_change = scenario.timeline.iter().any(|event| {
        matches!(event.action, RendererTimelineAction::SetFontSize { .. })
    });
    let has_display_move = scenario.timeline.iter().any(|event| {
        matches!(event.action, RendererTimelineAction::MoveToDisplay { .. })
    });

    let transition_valid = match scenario.gesture {
        RendererGesture::SameGridDrag => {
            initial.grid == final_state.grid
                && initial.display.viewport_width_px != final_state.display.viewport_width_px
                    || initial.grid == final_state.grid
                        && initial.display.viewport_height_px
                            != final_state.display.viewport_height_px
        }
        RendererGesture::GridChangingDrag => {
            initial.grid != final_state.grid
                && (initial.display.viewport_width_px != final_state.display.viewport_width_px
                    || initial.display.viewport_height_px
                        != final_state.display.viewport_height_px)
        }
        RendererGesture::Reflow80To200 => {
            initial.grid.columns == 80 && final_state.grid.columns == 200
        }
        RendererGesture::Reflow200To80 => {
            initial.grid.columns == 200 && final_state.grid.columns == 80
        }
        RendererGesture::ZoomIn => {
            final_state.font.size_milli_points > initial.font.size_milli_points
        }
        RendererGesture::ZoomOut => {
            final_state.font.size_milli_points < initial.font.size_milli_points
        }
        RendererGesture::DpiDisplayMove => {
            initial.display.display_id != final_state.display.display_id
                && (initial.display.dpi_milli != final_state.display.dpi_milli
                    || initial.display.scale_factor_milli
                        != final_state.display.scale_factor_milli)
        }
        RendererGesture::OutputOverlapResize => {
            initial.grid != final_state.grid
                || initial.display.viewport_width_px != final_state.display.viewport_width_px
                || initial.display.viewport_height_px != final_state.display.viewport_height_px
        }
    };
    if !transition_valid {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            path,
            format!(
                "initial/final state does not satisfy `{}` transition semantics",
                scenario.gesture.as_str()
            ),
        );
    }

    let required_action_present = match scenario.gesture {
        RendererGesture::SameGridDrag => has_window_resize && !has_grid_change,
        RendererGesture::GridChangingDrag => has_window_resize && has_grid_change,
        RendererGesture::Reflow80To200 | RendererGesture::Reflow200To80 => has_grid_change,
        RendererGesture::ZoomIn | RendererGesture::ZoomOut => has_font_change,
        RendererGesture::DpiDisplayMove => has_display_move,
        RendererGesture::OutputOverlapResize => has_window_resize || has_grid_change,
    };
    if !required_action_present {
        validator.error(
            RendererScenarioValidationCode::InvalidGestureTransition,
            format!("{path}.timeline"),
            format!(
                "timeline lacks the required mutation action for `{}`",
                scenario.gesture.as_str()
            ),
        );
    }

    validate_output_rate_binding(path, scenario, workload, validator);
}

fn validate_output_rate_binding(
    path: &str,
    scenario: &RendererScenarioDefinition,
    workload: Option<&RendererWorkloadDefinition>,
    validator: &mut Validator,
) {
    let event_rates = scenario
        .timeline
        .iter()
        .filter_map(|event| match event.action {
            RendererTimelineAction::SetOutputRate { bytes_per_second } => {
                Some(bytes_per_second)
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    if let Some(workload) = workload {
        for rate in &event_rates {
            if *rate != 0 && *rate != workload.output_bytes_per_second {
                validator.error(
                    RendererScenarioValidationCode::InvalidWorkload,
                    format!("{path}.timeline"),
                    format!(
                        "timeline output rate {rate} does not match workload rate {}",
                        workload.output_bytes_per_second
                    ),
                );
            }
        }
    }

    if scenario.gesture != RendererGesture::OutputOverlapResize {
        return;
    }
    let workload_rate = workload.map(|value| value.output_bytes_per_second);
    let has_exact_event = event_rates.contains(&OUTPUT_OVERLAP_BYTES_PER_SECOND);
    let has_wrong_nonzero_event = event_rates
        .iter()
        .any(|rate| *rate != 0 && *rate != OUTPUT_OVERLAP_BYTES_PER_SECOND);
    if workload_rate != Some(OUTPUT_OVERLAP_BYTES_PER_SECOND)
        || !has_exact_event
        || has_wrong_nonzero_event
    {
        validator.error(
            RendererScenarioValidationCode::InvalidOutputOverlapRate,
            path,
            format!(
                "output-overlap resize requires workload and nonzero timeline output at exactly {OUTPUT_OVERLAP_BYTES_PER_SECOND} bytes/s"
            ),
        );
    }
}
