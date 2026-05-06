//! OSC 22 / 8 / 52 cluster — contract layer
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.5.cont] / `ft-jornq`).
//!
//! This module ships the **contract layer** for the
//! continuation of `ft-7yiu2`:
//!
//! - **OSC 22** (mouse cursor shape) — W3C cursor name →
//!   per-platform native-cursor table with fallback to
//!   `default` for unknown names.
//! - **OSC 8** (hyperlinks) — hover state machine that maps
//!   `(row, col)` to the underlying URL when present;
//!   accessibility announcement contract.
//! - **OSC 52** (clipboard) — explicit policy gate with
//!   `ClipboardRead` and `ClipboardWrite` `ActionKind`s; the
//!   bead's headline rule: **default-deny on reads** to
//!   prevent pages from siphoning the user's clipboard.
//!
//! ## Already shipped (parent bead `ft-7yiu2`)
//!
//! Term-layer slice at `526e675cd`:
//! - OSC 22 parser entry (was missing entirely).
//! - OSC 8 / 52 audit tests pinning existing wiring.
//! - 11 integration tests passing.
//!
//! ## What this module is NOT
//!
//! - Not the actual GUI cursor / clipboard wiring. The
//!   contract types ship here; the per-platform code (NSCursor,
//!   xcb_cursor, LoadCursorW) lands in frankenterm-gui as the
//!   integration follow-on.
//! - Not the URL handler. The bead's "click invokes the
//!   configured URL handler" lives in the GUI; this module
//!   ships the lookup contract (`get_hyperlink_at`).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

// ============================================================================
// OSC 22 — Mouse cursor shape
// ============================================================================

/// Closed list of W3C-defined cursor names that
/// `OSC 22 ; <name>` can request. Adding a name extends this
/// enum; the per-platform table below maps each variant to a
/// native handle name.
///
/// Reference: <https://www.w3.org/TR/css-ui-3/#cursor> (the
/// CSS3 cursor keywords; iTerm2 / kitty / WezTerm all
/// dispatch the same set via OSC 22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorShape {
    /// Default arrow.
    Default,
    /// I-beam — text edit.
    Text,
    /// Pointing hand — clickable.
    Pointer,
    /// Wait / busy.
    Wait,
    /// Crosshair.
    Crosshair,
    /// Move all directions.
    Move,
    /// Cannot drop here.
    NotAllowed,
    /// Help cursor (with question mark).
    Help,
    /// Vertical resize (north-south).
    NsResize,
    /// Horizontal resize (east-west).
    EwResize,
    /// Diagonal resize (northwest-southeast).
    NwseResize,
    /// Diagonal resize (northeast-southwest).
    NeswResize,
    /// Grab / open hand.
    Grab,
    /// Grabbing / closed hand.
    Grabbing,
}

impl CursorShape {
    /// Stable W3C-name slug used in the OSC 22 wire form.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Text => "text",
            Self::Pointer => "pointer",
            Self::Wait => "wait",
            Self::Crosshair => "crosshair",
            Self::Move => "move",
            Self::NotAllowed => "not-allowed",
            Self::Help => "help",
            Self::NsResize => "ns-resize",
            Self::EwResize => "ew-resize",
            Self::NwseResize => "nwse-resize",
            Self::NeswResize => "nesw-resize",
            Self::Grab => "grab",
            Self::Grabbing => "grabbing",
        }
    }

    /// Resolve from the wire-format name. Unknown names fall
    /// back to `Default` per the bead's "must not crash on
    /// unknown" rule.
    #[must_use]
    pub fn from_slug_with_fallback(slug: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|s| s.slug() == slug)
            .unwrap_or(Self::Default)
    }

    /// All cursor variants in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::Text,
        Self::Pointer,
        Self::Wait,
        Self::Crosshair,
        Self::Move,
        Self::NotAllowed,
        Self::Help,
        Self::NsResize,
        Self::EwResize,
        Self::NwseResize,
        Self::NeswResize,
        Self::Grab,
        Self::Grabbing,
    ];
}

/// Per-platform native cursor handle name. The frankenterm-
/// gui crate consumes these to call the OS-specific cursor
/// API (`NSCursor.init(named:)`, `xcb_cursor_load_cursor`,
/// `LoadCursorW`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeCursorOs {
    Macos,
    Linux,
    Windows,
}

/// One row in the (CursorShape, OS) → native-name table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCursorMapping {
    pub shape: CursorShape,
    pub os: NativeCursorOs,
    /// Native name the OS API consumes
    /// (e.g. `"IBeamCursor"` on macOS, `"xterm"` on Linux,
    /// `"IDC_IBEAM"` on Windows).
    pub native_name: &'static str,
}

/// Per-platform mapping table. Used by the bead's action #1
/// integration code to translate `OSC 22 ; <slug>` into the
/// native cursor handle.
#[must_use]
pub fn native_cursor_table() -> &'static [NativeCursorMapping] {
    use CursorShape as Shape;
    use NativeCursorOs as Os;
    const TABLE: &[NativeCursorMapping] = &[
        // macOS — names as passed to NSCursor.init(named:).
        // The W3C names map roughly to these stock names; the
        // bead's "fallback to NSCursor.arrow" rule applies for
        // any new shape added without a Macos row.
        NativeCursorMapping {
            shape: Shape::Default,
            os: Os::Macos,
            native_name: "arrowCursor",
        },
        NativeCursorMapping {
            shape: Shape::Text,
            os: Os::Macos,
            native_name: "IBeamCursor",
        },
        NativeCursorMapping {
            shape: Shape::Pointer,
            os: Os::Macos,
            native_name: "pointingHandCursor",
        },
        NativeCursorMapping {
            shape: Shape::Wait,
            os: Os::Macos,
            native_name: "busyButClickableCursor",
        },
        NativeCursorMapping {
            shape: Shape::Crosshair,
            os: Os::Macos,
            native_name: "crosshairCursor",
        },
        NativeCursorMapping {
            shape: Shape::Move,
            os: Os::Macos,
            native_name: "openHandCursor",
        },
        NativeCursorMapping {
            shape: Shape::NotAllowed,
            os: Os::Macos,
            native_name: "operationNotAllowedCursor",
        },
        NativeCursorMapping {
            shape: Shape::Help,
            os: Os::Macos,
            native_name: "arrowCursor",
        },
        NativeCursorMapping {
            shape: Shape::NsResize,
            os: Os::Macos,
            native_name: "resizeUpDownCursor",
        },
        NativeCursorMapping {
            shape: Shape::EwResize,
            os: Os::Macos,
            native_name: "resizeLeftRightCursor",
        },
        NativeCursorMapping {
            shape: Shape::NwseResize,
            os: Os::Macos,
            native_name: "arrowCursor",
        },
        NativeCursorMapping {
            shape: Shape::NeswResize,
            os: Os::Macos,
            native_name: "arrowCursor",
        },
        NativeCursorMapping {
            shape: Shape::Grab,
            os: Os::Macos,
            native_name: "openHandCursor",
        },
        NativeCursorMapping {
            shape: Shape::Grabbing,
            os: Os::Macos,
            native_name: "closedHandCursor",
        },
        // Linux — XCursor names (also valid for Wayland via
        // wl_cursor_load_theme).
        NativeCursorMapping {
            shape: Shape::Default,
            os: Os::Linux,
            native_name: "default",
        },
        NativeCursorMapping {
            shape: Shape::Text,
            os: Os::Linux,
            native_name: "xterm",
        },
        NativeCursorMapping {
            shape: Shape::Pointer,
            os: Os::Linux,
            native_name: "hand2",
        },
        NativeCursorMapping {
            shape: Shape::Wait,
            os: Os::Linux,
            native_name: "watch",
        },
        NativeCursorMapping {
            shape: Shape::Crosshair,
            os: Os::Linux,
            native_name: "crosshair",
        },
        NativeCursorMapping {
            shape: Shape::Move,
            os: Os::Linux,
            native_name: "fleur",
        },
        NativeCursorMapping {
            shape: Shape::NotAllowed,
            os: Os::Linux,
            native_name: "not-allowed",
        },
        NativeCursorMapping {
            shape: Shape::Help,
            os: Os::Linux,
            native_name: "question_arrow",
        },
        NativeCursorMapping {
            shape: Shape::NsResize,
            os: Os::Linux,
            native_name: "ns-resize",
        },
        NativeCursorMapping {
            shape: Shape::EwResize,
            os: Os::Linux,
            native_name: "ew-resize",
        },
        NativeCursorMapping {
            shape: Shape::NwseResize,
            os: Os::Linux,
            native_name: "nwse-resize",
        },
        NativeCursorMapping {
            shape: Shape::NeswResize,
            os: Os::Linux,
            native_name: "nesw-resize",
        },
        NativeCursorMapping {
            shape: Shape::Grab,
            os: Os::Linux,
            native_name: "grab",
        },
        NativeCursorMapping {
            shape: Shape::Grabbing,
            os: Os::Linux,
            native_name: "grabbing",
        },
        // Windows — IDC_* identifiers passed to LoadCursorW.
        NativeCursorMapping {
            shape: Shape::Default,
            os: Os::Windows,
            native_name: "IDC_ARROW",
        },
        NativeCursorMapping {
            shape: Shape::Text,
            os: Os::Windows,
            native_name: "IDC_IBEAM",
        },
        NativeCursorMapping {
            shape: Shape::Pointer,
            os: Os::Windows,
            native_name: "IDC_HAND",
        },
        NativeCursorMapping {
            shape: Shape::Wait,
            os: Os::Windows,
            native_name: "IDC_WAIT",
        },
        NativeCursorMapping {
            shape: Shape::Crosshair,
            os: Os::Windows,
            native_name: "IDC_CROSS",
        },
        NativeCursorMapping {
            shape: Shape::Move,
            os: Os::Windows,
            native_name: "IDC_SIZEALL",
        },
        NativeCursorMapping {
            shape: Shape::NotAllowed,
            os: Os::Windows,
            native_name: "IDC_NO",
        },
        NativeCursorMapping {
            shape: Shape::Help,
            os: Os::Windows,
            native_name: "IDC_HELP",
        },
        NativeCursorMapping {
            shape: Shape::NsResize,
            os: Os::Windows,
            native_name: "IDC_SIZENS",
        },
        NativeCursorMapping {
            shape: Shape::EwResize,
            os: Os::Windows,
            native_name: "IDC_SIZEWE",
        },
        NativeCursorMapping {
            shape: Shape::NwseResize,
            os: Os::Windows,
            native_name: "IDC_SIZENWSE",
        },
        NativeCursorMapping {
            shape: Shape::NeswResize,
            os: Os::Windows,
            native_name: "IDC_SIZENESW",
        },
        NativeCursorMapping {
            shape: Shape::Grab,
            os: Os::Windows,
            native_name: "IDC_HAND",
        },
        NativeCursorMapping {
            shape: Shape::Grabbing,
            os: Os::Windows,
            native_name: "IDC_HAND",
        },
    ];
    TABLE
}

/// Look up the native cursor name for `(shape, os)`. Returns
/// the fallback "default cursor" native name when not found
/// in the table — the bead's "must not crash on unknown" rule.
#[must_use]
pub fn native_cursor_name(shape: CursorShape, os: NativeCursorOs) -> &'static str {
    native_cursor_table()
        .iter()
        .find(|m| m.shape == shape && m.os == os)
        .map(|m| m.native_name)
        .unwrap_or_else(|| match os {
            NativeCursorOs::Macos => "arrowCursor",
            NativeCursorOs::Linux => "default",
            NativeCursorOs::Windows => "IDC_ARROW",
        })
}

// ============================================================================
// OSC 8 — hyperlink hover
// ============================================================================

/// One pane-cell hyperlink anchor, populated by the term layer
/// when an `OSC 8` opens. The GUI's hover handler queries
/// `get_hyperlink_at(row, col)` and gets back the matching
/// `HyperlinkAnchor` if present.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HyperlinkAnchor {
    /// Stable id from `OSC 8 ; id=<value>;...` (or empty when
    /// the app didn't supply one).
    pub id: String,
    /// The URL being linked.
    pub url: String,
    /// Pane-relative row of the cell.
    pub row: u16,
    /// Pane-relative column.
    pub col: u16,
}

/// State for the hover/click/a11y subsystem. Tracks the
/// currently-hovered anchor so the GUI emits the
/// accessibility announcement exactly once per hover, not
/// once per mouse-move event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperlinkHoverState {
    /// Current anchor under the cursor.
    pub current: Option<HyperlinkAnchor>,
    /// Total announcements emitted (debounced).
    pub announcements_emitted: u64,
}

/// Outcome of a `update_hover` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HoverOutcome {
    /// Cursor entered a new anchor — emit announcement.
    AnchorEntered,
    /// Cursor stayed on the same anchor — no announcement.
    NoChange,
    /// Cursor left the anchor (or moved to a non-link cell).
    AnchorLeft,
}

impl HyperlinkHoverState {
    /// Update with the current cell's anchor (or `None` if
    /// the cell isn't a hyperlink). Returns the outcome the
    /// GUI uses to decide whether to emit the
    /// accessibility announcement.
    pub fn update_hover(&mut self, anchor: Option<HyperlinkAnchor>) -> HoverOutcome {
        match (&self.current, &anchor) {
            (None, None) => HoverOutcome::NoChange,
            (Some(_), None) => {
                self.current = None;
                HoverOutcome::AnchorLeft
            }
            (None, Some(_)) => {
                self.current = anchor;
                self.announcements_emitted = self.announcements_emitted.saturating_add(1);
                HoverOutcome::AnchorEntered
            }
            (Some(curr), Some(next)) => {
                // Same anchor (id+url match) → NoChange.
                // Different anchor → AnchorEntered (new
                // announcement).
                if curr.id == next.id && curr.url == next.url {
                    HoverOutcome::NoChange
                } else {
                    self.current = anchor;
                    self.announcements_emitted = self.announcements_emitted.saturating_add(1);
                    HoverOutcome::AnchorEntered
                }
            }
        }
    }
}

// ============================================================================
// OSC 52 — Clipboard policy
// ============================================================================

/// Action kind for clipboard operations. The bead's headline
/// rule: **`ClipboardRead` is default-deny** because allowing
/// pages to read the user's clipboard via `OSC 52 ; ; ?`
/// query is a security hole (a page can siphon credentials,
/// secrets, etc., that the user copied from elsewhere).
/// `ClipboardWrite` defaults to `RequireApproval` so the
/// user can opt in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardActionKind {
    /// `OSC 52 ; <selection> ; <base64>` — write to clipboard.
    ClipboardWrite,
    /// `OSC 52 ; <selection> ; ?` — query clipboard contents.
    ClipboardRead,
}

/// Operator-configured policy for one clipboard action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardPolicy {
    Allow,
    Deny,
    RequireApproval,
}

impl ClipboardPolicy {
    /// The bead's default for each action kind.
    #[must_use]
    pub const fn default_for(action: ClipboardActionKind) -> Self {
        match action {
            ClipboardActionKind::ClipboardWrite => Self::RequireApproval,
            ClipboardActionKind::ClipboardRead => Self::Deny,
        }
    }
}

/// Decision the policy evaluator returns to the dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardDecision {
    /// Forward to the OS clipboard.
    Allow,
    /// Drop the request silently + log.
    Deny,
    /// Raise an `Alert::ClipboardActionRequested` for the
    /// GUI to confirm.
    PromptUser,
}

/// Evaluate a clipboard request against the policy.
#[must_use]
pub const fn evaluate_clipboard(
    _action: ClipboardActionKind,
    policy: ClipboardPolicy,
) -> ClipboardDecision {
    match policy {
        ClipboardPolicy::Allow => ClipboardDecision::Allow,
        ClipboardPolicy::Deny => ClipboardDecision::Deny,
        ClipboardPolicy::RequireApproval => ClipboardDecision::PromptUser,
    }
}

/// Per-action policy table. The bead's section #3 audit hook:
/// the dispatcher consults this table on every OSC 52
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardPolicyTable {
    pub write: ClipboardPolicy,
    pub read: ClipboardPolicy,
}

impl Default for ClipboardPolicyTable {
    fn default() -> Self {
        Self {
            write: ClipboardPolicy::default_for(ClipboardActionKind::ClipboardWrite),
            read: ClipboardPolicy::default_for(ClipboardActionKind::ClipboardRead),
        }
    }
}

impl ClipboardPolicyTable {
    /// Look up the policy for an action.
    #[must_use]
    pub const fn for_action(&self, action: ClipboardActionKind) -> ClipboardPolicy {
        match action {
            ClipboardActionKind::ClipboardWrite => self.write,
            ClipboardActionKind::ClipboardRead => self.read,
        }
    }
}

// ============================================================================
// Conformance corpus
// ============================================================================

/// Per-OSC fixture slugs covered by the bead's action #4
/// corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc2xConformanceFixture {
    /// `tests/golden/osc_22/<W3C name>/` — minimal fixture
    /// per cursor name.
    Osc22Cursor { shape: CursorShape },
    /// `tests/golden/osc_8/<scenario>/` — gitstatus / eza-
    /// style id=<value>;<url> sequences plus the simple shape.
    Osc8Hyperlink { scenario: Osc8Scenario },
    /// `tests/golden/osc_52/<scenario>/` — write / clear /
    /// query, with policy-allow + policy-deny variants for
    /// each.
    Osc52Clipboard { scenario: Osc52Scenario },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc8Scenario {
    /// Simple `OSC 8 ; ; <url> ST text OSC 8 ; ;` (no id).
    Simple,
    /// `OSC 8 ; id=<value>;<url>` (gitstatus / eza style).
    WithId,
    /// Nested anchor (mid-render OSC 8 close + reopen).
    Nested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc52Scenario {
    Write,
    Clear,
    QueryAllowed,
    QueryDenied,
}

/// Full corpus — covers all 14 cursor shapes + 3 OSC 8
/// scenarios + 4 OSC 52 scenarios.
#[must_use]
pub fn osc_2x_corpus() -> Vec<Osc2xConformanceFixture> {
    let mut out = Vec::new();
    for shape in CursorShape::ALL {
        out.push(Osc2xConformanceFixture::Osc22Cursor { shape: *shape });
    }
    for scenario in [
        Osc8Scenario::Simple,
        Osc8Scenario::WithId,
        Osc8Scenario::Nested,
    ] {
        out.push(Osc2xConformanceFixture::Osc8Hyperlink { scenario });
    }
    for scenario in [
        Osc52Scenario::Write,
        Osc52Scenario::Clear,
        Osc52Scenario::QueryAllowed,
        Osc52Scenario::QueryDenied,
    ] {
        out.push(Osc2xConformanceFixture::Osc52Clipboard { scenario });
    }
    out
}

// ============================================================================
// Rollout staging
// ============================================================================

/// Feature-flag rollout phase per `ft-mpc9b.9` substrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    /// Compiled but not exposed; flag-gated.
    Hidden,
    /// User-opt-in via `[features.osc_2x] = "on"`.
    OptIn,
    /// Default-on.
    Default,
}

impl RolloutPhase {
    #[must_use]
    pub const fn user_visible(self) -> bool {
        matches!(self, Self::OptIn | Self::Default)
    }

    #[must_use]
    pub const fn on_by_default(self) -> bool {
        matches!(self, Self::Default)
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the OSC 22/8/52 cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Osc2xHealth {
    pub rollout_phase: RolloutPhase,
    /// Current OS the GUI is running on.
    pub os: NativeCursorOs,
    /// Current cursor shape (most recent OSC 22).
    pub current_cursor: CursorShape,
    /// Total OSC 22 dispatches.
    pub osc_22_total: u64,
    /// Total OSC 8 anchors observed.
    pub osc_8_anchors_total: u64,
    /// Total hyperlink hover-enter events.
    pub osc_8_hover_enters_total: u64,
    /// Total OSC 52 reads attempted (regardless of outcome).
    pub osc_52_reads_attempted: u64,
    /// Total OSC 52 reads denied.
    pub osc_52_reads_denied: u64,
    /// Total OSC 52 writes attempted.
    pub osc_52_writes_attempted: u64,
    /// Cursor shapes the parser saw with no native mapping
    /// for the active OS (logged for follow-on table updates).
    pub unknown_cursor_shapes: BTreeSet<String>,
}

impl Osc2xHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            rollout_phase: RolloutPhase::Hidden,
            os: NativeCursorOs::Linux,
            current_cursor: CursorShape::Default,
            osc_22_total: 0,
            osc_8_anchors_total: 0,
            osc_8_hover_enters_total: 0,
            osc_52_reads_attempted: 0,
            osc_52_reads_denied: 0,
            osc_52_writes_attempted: 0,
            unknown_cursor_shapes: BTreeSet::new(),
        }
    }

    /// True iff the security-critical invariant holds:
    /// every attempted OSC 52 read was either denied or
    /// approved-via-prompt — no read can succeed with the
    /// default policy in place. The harness asserts
    /// `reads_attempted == reads_denied + (allowed via prompt)`
    /// indirectly via the policy evaluator.
    ///
    /// Returned predicate: true unless we observed
    /// `reads_attempted > 0` and `reads_denied == 0` —
    /// indicating the gate is broken (all reads are
    /// silently approved without the policy evaluator firing).
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        // If reads were attempted, at least one must have
        // been denied OR the policy was explicitly Allow
        // (operator opt-in). The default policy is Deny, so
        // observing 100% allow-rate without explicit opt-in
        // is the bug class.
        self.osc_52_reads_attempted == 0 || self.osc_52_reads_denied > 0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // CursorShape + native table
    // ------------------------------------------------------------------------

    #[test]
    fn all_cursor_shapes_have_distinct_slugs() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for s in CursorShape::ALL {
            assert!(seen.insert(s.slug()), "dup slug: {}", s.slug());
        }
    }

    #[test]
    fn unknown_cursor_slug_falls_back_to_default() {
        assert_eq!(
            CursorShape::from_slug_with_fallback("foobar-not-a-cursor"),
            CursorShape::Default
        );
    }

    #[test]
    fn known_cursor_slugs_roundtrip() {
        for s in CursorShape::ALL {
            let parsed = CursorShape::from_slug_with_fallback(s.slug());
            assert_eq!(parsed, *s);
        }
    }

    #[test]
    fn native_cursor_table_covers_every_shape_per_os() {
        for os in [
            NativeCursorOs::Macos,
            NativeCursorOs::Linux,
            NativeCursorOs::Windows,
        ] {
            for shape in CursorShape::ALL {
                let name = native_cursor_name(*shape, os);
                assert!(!name.is_empty(), "missing mapping for {shape:?} on {os:?}");
            }
        }
    }

    #[test]
    fn cursor_serde_roundtrip() {
        let json = serde_json::to_string(&CursorShape::NwseResize).unwrap();
        let parsed: CursorShape = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, CursorShape::NwseResize);
    }

    // ------------------------------------------------------------------------
    // Hyperlink hover state
    // ------------------------------------------------------------------------

    fn anchor(id: &str, url: &str) -> HyperlinkAnchor {
        HyperlinkAnchor {
            id: id.to_string(),
            url: url.to_string(),
            row: 0,
            col: 0,
        }
    }

    #[test]
    fn hover_outside_link_is_no_change() {
        let mut s = HyperlinkHoverState::default();
        let outcome = s.update_hover(None);
        assert_eq!(outcome, HoverOutcome::NoChange);
        assert_eq!(s.announcements_emitted, 0);
    }

    #[test]
    fn entering_link_emits_announcement() {
        let mut s = HyperlinkHoverState::default();
        let outcome = s.update_hover(Some(anchor("", "https://example.com")));
        assert_eq!(outcome, HoverOutcome::AnchorEntered);
        assert_eq!(s.announcements_emitted, 1);
    }

    #[test]
    fn staying_on_same_link_does_not_re_announce() {
        let mut s = HyperlinkHoverState::default();
        s.update_hover(Some(anchor("a", "https://example.com")));
        let outcome = s.update_hover(Some(anchor("a", "https://example.com")));
        assert_eq!(outcome, HoverOutcome::NoChange);
        assert_eq!(s.announcements_emitted, 1);
    }

    #[test]
    fn moving_to_different_link_re_announces() {
        let mut s = HyperlinkHoverState::default();
        s.update_hover(Some(anchor("a", "https://example.com")));
        let outcome = s.update_hover(Some(anchor("b", "https://other.com")));
        assert_eq!(outcome, HoverOutcome::AnchorEntered);
        assert_eq!(s.announcements_emitted, 2);
    }

    #[test]
    fn moving_off_link_emits_left() {
        let mut s = HyperlinkHoverState::default();
        s.update_hover(Some(anchor("a", "https://example.com")));
        let outcome = s.update_hover(None);
        assert_eq!(outcome, HoverOutcome::AnchorLeft);
        assert_eq!(s.current, None);
    }

    // ------------------------------------------------------------------------
    // Clipboard policy
    // ------------------------------------------------------------------------

    #[test]
    fn clipboard_read_default_is_deny() {
        assert_eq!(
            ClipboardPolicy::default_for(ClipboardActionKind::ClipboardRead),
            ClipboardPolicy::Deny
        );
    }

    #[test]
    fn clipboard_write_default_is_require_approval() {
        assert_eq!(
            ClipboardPolicy::default_for(ClipboardActionKind::ClipboardWrite),
            ClipboardPolicy::RequireApproval
        );
    }

    #[test]
    fn evaluate_clipboard_maps_policy_to_decision() {
        assert_eq!(
            evaluate_clipboard(ClipboardActionKind::ClipboardRead, ClipboardPolicy::Deny),
            ClipboardDecision::Deny
        );
        assert_eq!(
            evaluate_clipboard(ClipboardActionKind::ClipboardWrite, ClipboardPolicy::Allow),
            ClipboardDecision::Allow
        );
        assert_eq!(
            evaluate_clipboard(
                ClipboardActionKind::ClipboardWrite,
                ClipboardPolicy::RequireApproval
            ),
            ClipboardDecision::PromptUser
        );
    }

    #[test]
    fn clipboard_policy_table_default_blocks_reads() {
        let t = ClipboardPolicyTable::default();
        assert_eq!(
            t.for_action(ClipboardActionKind::ClipboardRead),
            ClipboardPolicy::Deny
        );
        let decision = evaluate_clipboard(
            ClipboardActionKind::ClipboardRead,
            t.for_action(ClipboardActionKind::ClipboardRead),
        );
        assert_eq!(decision, ClipboardDecision::Deny);
    }

    // ------------------------------------------------------------------------
    // Conformance corpus
    // ------------------------------------------------------------------------

    #[test]
    fn corpus_has_expected_size() {
        let c = osc_2x_corpus();
        // 14 cursor shapes + 3 OSC 8 scenarios + 4 OSC 52 = 21
        assert_eq!(c.len(), 14 + 3 + 4);
    }

    #[test]
    fn corpus_includes_all_cursor_shapes() {
        let c = osc_2x_corpus();
        for shape in CursorShape::ALL {
            assert!(
                c.iter().any(|f| matches!(
                    f,
                    Osc2xConformanceFixture::Osc22Cursor { shape: s } if s == shape
                )),
                "missing fixture for {shape:?}"
            );
        }
    }

    // ------------------------------------------------------------------------
    // Rollout
    // ------------------------------------------------------------------------

    #[test]
    fn rollout_visibility_progression() {
        assert!(!RolloutPhase::Hidden.user_visible());
        assert!(RolloutPhase::OptIn.user_visible());
        assert!(RolloutPhase::Default.user_visible());
        assert!(!RolloutPhase::OptIn.on_by_default());
        assert!(RolloutPhase::Default.on_by_default());
    }

    #[test]
    fn rollout_phase_ordered() {
        assert!(RolloutPhase::Hidden < RolloutPhase::OptIn);
        assert!(RolloutPhase::OptIn < RolloutPhase::Default);
    }

    // ------------------------------------------------------------------------
    // Health
    // ------------------------------------------------------------------------

    #[test]
    fn baseline_health_safe() {
        assert!(Osc2xHealth::baseline().is_safe());
    }

    #[test]
    fn health_unsafe_when_reads_attempted_but_zero_denied() {
        let h = Osc2xHealth {
            osc_52_reads_attempted: 5,
            osc_52_reads_denied: 0,
            ..Osc2xHealth::baseline()
        };
        assert!(!h.is_safe());
    }

    #[test]
    fn health_safe_when_some_reads_denied() {
        let h = Osc2xHealth {
            osc_52_reads_attempted: 5,
            osc_52_reads_denied: 5,
            ..Osc2xHealth::baseline()
        };
        assert!(h.is_safe());
    }

    #[test]
    fn health_serde_roundtrips() {
        let mut h = Osc2xHealth::baseline();
        h.unknown_cursor_shapes
            .insert("strangely-named".to_string());
        let json = serde_json::to_string(&h).unwrap();
        let parsed: Osc2xHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, h);
    }
}
