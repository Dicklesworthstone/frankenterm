//! OSC 8/22/52 integration substrate
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.5.cont] / `ft-io922`).
//!
//! The OSC protocol substrate already lives at
//! `osc_protocol_omnibus.rs` (`c46aa28b8`, 39 tests).
//! This module ships the **integration substrate** the
//! bead's continuation work consumes — particularly the
//! typed-state OSC 52 read-path that structurally enforces
//! the "respond with empty payload when denied" privacy
//! rule.
//!
//! ## What this module ships
//!
//! - [`HyperlinkSpan`] — pure-data view of a contiguous
//!   range of cells under a single OSC 8 hyperlink id;
//!   integration's per-cell `HyperlinkId` storage projects
//!   onto this for hover/click dispatch.
//! - [`Osc22PerPaneCursorMap`] — per-pane cursor-shape
//!   state with focus-change preservation (sub-task OSC 22).
//! - **[`Osc52ReadResponse`] typed-state pipeline** —
//!   `Decoded → PolicyGated → Allowed | Denied → Emitted`.
//!   Denied path *cannot* construct a non-empty response;
//!   privacy rule "Read path: when denied, respond with
//!   empty payload" is enforced at compile time.
//! - [`A11yAnnouncementShape`] — cross-protocol
//!   announcement contract (cross-link `a11y_tree.rs`).
//! - [`OscIntegrationHealth`] — `ft doctor` snapshot.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use crate::osc_protocol_omnibus::{
    Osc52AuditDecision, Osc52AuditEvent, Osc52DenyReason, Osc52Direction, parse_osc52_targets,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

// ============================================================================
// OSC 8: Hyperlink span tracking
// ============================================================================

/// Stable hyperlink id (per-pane). The integration's
/// per-cell `Cell::hyperlink_id` field carries one of
/// these (or `None`).
pub type HyperlinkId = u32;

/// One cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct CellCoord {
    pub line: u32,
    pub col: u32,
}

/// Contiguous span of cells under a single hyperlink id.
/// The integration projects per-cell `HyperlinkId` storage
/// onto this for hover/click dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HyperlinkSpan {
    pub id: HyperlinkId,
    pub start: CellCoord,
    pub end_exclusive: CellCoord,
    pub uri: String,
}

impl HyperlinkSpan {
    /// True iff `coord` falls inside the span (inclusive
    /// of start, exclusive of end).
    #[must_use]
    pub fn contains(&self, coord: CellCoord) -> bool {
        coord >= self.start && coord < self.end_exclusive
    }

    /// Cell count for an intra-line span. Returns `None`
    /// for multi-line spans (the integration computes the
    /// total via the grid width — this module doesn't
    /// have it).
    #[must_use]
    pub fn intra_line_cell_count(&self) -> Option<u32> {
        if self.start.line == self.end_exclusive.line {
            Some(self.end_exclusive.col.saturating_sub(self.start.col))
        } else {
            None
        }
    }

    /// True iff the span crosses a line boundary.
    #[must_use]
    pub fn is_multi_line(&self) -> bool {
        self.start.line != self.end_exclusive.line
    }
}

/// Hover/click dispatch result. Bead sub-task OSC 8 #4:
/// "Click handler: dispatch into frankenterm/open-url."
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HyperlinkInteraction {
    /// Cursor is over a hyperlink — show URI in status
    /// bar, change cursor.
    Hovering { id: HyperlinkId, uri: String },
    /// Click without modifier — open URI.
    OpenUrl { id: HyperlinkId, uri: String },
    /// Click with selection modifier (per bead sub-task
    /// OSC 8 #4) — fall through to smart_selection.
    SelectInstead { id: HyperlinkId },
    /// Cursor not over a hyperlink — no-op.
    NotOverHyperlink,
}

/// Decision tree per the bead's sub-task #4:
///
/// > Click handler: dispatch into frankenterm/open-url;
/// > tied into smart_selection (cross-link 2.14) so
/// > clicking the hyperlink span selects it instead of
/// > triggering open when modifier held.
#[must_use]
pub fn dispatch_click(
    span: Option<&HyperlinkSpan>,
    selection_modifier_held: bool,
) -> HyperlinkInteraction {
    match span {
        None => HyperlinkInteraction::NotOverHyperlink,
        Some(s) if selection_modifier_held => HyperlinkInteraction::SelectInstead { id: s.id },
        Some(s) => HyperlinkInteraction::OpenUrl {
            id: s.id,
            uri: s.uri.clone(),
        },
    }
}

// ============================================================================
// OSC 22: Per-pane cursor-shape map (sub-task OSC 22 #3)
// ============================================================================

/// Cursor shape per the OSC 22 protocol. Mirrors the
/// substrate's `Osc22CursorShape` to keep this module
/// decoupled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShapeSlug {
    Default,
    BlockBlinking,
    BlockSteady,
    UnderlineBlinking,
    UnderlineSteady,
    BarBlinking,
    BarSteady,
}

impl CursorShapeSlug {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::BlockBlinking => "block_blinking",
            Self::BlockSteady => "block_steady",
            Self::UnderlineBlinking => "underline_blinking",
            Self::UnderlineSteady => "underline_steady",
            Self::BarBlinking => "bar_blinking",
            Self::BarSteady => "bar_steady",
        }
    }
}

/// Per-pane cursor-shape map. Bead sub-task OSC 22 #3:
///
/// > Persist across pane focus change (per-pane state,
/// > not global).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Osc22PerPaneCursorMap {
    pub by_pane: BTreeMap<u64, CursorShapeSlug>,
    /// Lifetime count of cursor-shape changes — telemetry.
    pub changes_total: u64,
}

impl Osc22PerPaneCursorMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the cursor shape for a pane. Returns the prior
    /// shape if it changed.
    pub fn set(&mut self, pane_id: u64, shape: CursorShapeSlug) -> Option<CursorShapeSlug> {
        let prior = self.by_pane.insert(pane_id, shape);
        if prior != Some(shape) {
            self.changes_total = self.changes_total.saturating_add(1);
        }
        prior
    }

    /// Read the cursor shape for a pane. Returns `Default`
    /// if no prior shape was set.
    #[must_use]
    pub fn get(&self, pane_id: u64) -> CursorShapeSlug {
        self.by_pane
            .get(&pane_id)
            .copied()
            .unwrap_or(CursorShapeSlug::Default)
    }

    /// Drop a pane's cursor-shape state on pane close.
    /// Returns the shape that was removed, if any.
    pub fn forget(&mut self, pane_id: u64) -> Option<CursorShapeSlug> {
        self.by_pane.remove(&pane_id)
    }
}

// ============================================================================
// OSC 52: Typed-state read-response pipeline
// ============================================================================

/// Phantom marker types for the OSC 52 read-response
/// typed-state pipeline. Each stage's wrapper has methods
/// that only produce the next stage. The privacy rule
/// "Read path: when denied, respond with empty payload"
/// is enforced at compile time:
///
/// - The `Denied` typed-state has no method that exposes
///   the underlying clipboard bytes — its only emit-method
///   produces an empty response.
/// - The `Prompted` typed-state has no `emit_*` method at
///   all. To emit, the integration MUST first resolve the
///   prompt via `confirmed_by_operator()` →
///   `Osc52ReadResponse<Allowed>` or `denied_by_operator()`
///   → `Osc52ReadResponse<Denied>`. This prevents a
///   maintainer from accidentally emitting clipboard
///   content while a prompt is still pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decoded;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Allowed;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Denied;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Prompted;

/// OSC 52 read-response typed-state. Pipeline:
/// `Decoded → policy_gate(policy) → Allowed | Denied`.
#[derive(Debug, Clone)]
pub struct Osc52ReadResponse<Stage> {
    /// Clipboard bytes — present only in `Allowed` state.
    /// Foundation slice carries `Option<Vec<u8>>`; the
    /// stage marker enforces the structural rule. The
    /// `Denied` stage has no method that exposes this.
    bytes: Option<Vec<u8>>,
    _stage: PhantomData<Stage>,
}

/// Operator policy decision. Mirrors the existing
/// `Osc52Policy` in `smart_selection.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc52PolicySlug {
    Allow,
    Prompt,
    Deny,
}

/// Write-side policy outcome distilled for audit-chain emission.
///
/// The actual write gate lives in `frankenterm/term`, so this core
/// integration substrate carries only the policy outcome slug needed for
/// `Osc52AuditEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc52WriteAuditOutcome {
    Allowed,
    Prompted,
    DeniedByPolicy,
    DeniedOversized,
    DeniedInvalidBase64,
}

impl Osc52WriteAuditOutcome {
    #[must_use]
    const fn audit_decision(self) -> Osc52AuditDecision {
        match self {
            Self::Allowed => Osc52AuditDecision::Allowed,
            Self::Prompted => Osc52AuditDecision::Prompted,
            Self::DeniedByPolicy => Osc52AuditDecision::Denied {
                reason: Osc52DenyReason::OperatorPolicy,
            },
            Self::DeniedOversized => Osc52AuditDecision::Denied {
                reason: Osc52DenyReason::Oversized,
            },
            Self::DeniedInvalidBase64 => Osc52AuditDecision::Denied {
                reason: Osc52DenyReason::InvalidBase64,
            },
        }
    }
}

/// Append a write-side OSC 52 policy decision to `policy_audit_chain`.
///
/// Call this immediately after the write gate decides whether decoded
/// clipboard bytes are allowed, prompted, or denied. `decoded_bytes_len` is
/// the post-decode payload length, not raw OSC text, and the audit event never
/// receives clipboard bytes.
pub fn append_osc52_write_audit<'a>(
    chain: &'a mut crate::policy_audit_chain::AuditChain,
    targets: &str,
    decoded_bytes_len: u64,
    outcome: Osc52WriteAuditOutcome,
    source_pane: u64,
    timestamp_ms: u64,
) -> &'a crate::policy_audit_chain::AuditChainEntry {
    let event = Osc52AuditEvent {
        direction: Osc52Direction::Write,
        targets: parse_osc52_targets(&sanitize_osc52_targets(targets)),
        decoded_bytes: decoded_bytes_len,
        decision: outcome.audit_decision(),
        source_pane,
        timestamp_ms,
    };
    event.append_to_chain(chain)
}

impl Osc52ReadResponse<Decoded> {
    /// Construct from raw clipboard bytes. The integration
    /// reads the OS clipboard and wraps the bytes here.
    #[must_use]
    pub fn from_clipboard(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Some(bytes),
            _stage: PhantomData,
        }
    }

    /// Apply the operator's policy gate.
    #[must_use]
    pub fn policy_gate(self, policy: Osc52PolicySlug) -> Osc52PolicyGated {
        match policy {
            Osc52PolicySlug::Allow => Osc52PolicyGated::Allowed(Osc52ReadResponse {
                bytes: self.bytes,
                _stage: PhantomData,
            }),
            Osc52PolicySlug::Deny => {
                // Privacy rule: when denied, the
                // clipboard bytes are dropped here. The
                // Denied state cannot reconstruct them.
                Osc52PolicyGated::Denied(Osc52ReadResponse {
                    bytes: None,
                    _stage: PhantomData,
                })
            }
            Osc52PolicySlug::Prompt => {
                // Pending operator decision. Returns a
                // distinct `Prompted` state with NO emit
                // method — the integration MUST resolve
                // via confirmed_by_operator() or
                // denied_by_operator() before emitting.
                Osc52PolicyGated::Prompted(Osc52ReadResponse {
                    bytes: self.bytes,
                    _stage: PhantomData,
                })
            }
        }
    }
}

/// Outcome of the policy gate.
#[derive(Debug, Clone)]
pub enum Osc52PolicyGated {
    Allowed(Osc52ReadResponse<Allowed>),
    Denied(Osc52ReadResponse<Denied>),
    /// Pending operator decision via prompt UI. Has NO
    /// emit method — must resolve to Allowed/Denied first.
    Prompted(Osc52ReadResponse<Prompted>),
}

impl Osc52PolicyGated {
    /// Append the read-side policy decision to `policy_audit_chain`.
    ///
    /// Call this at the OSC 52 read decision site immediately after
    /// [`Osc52ReadResponse<Decoded>::policy_gate`]. The audit event records the
    /// target selection set, post-decode byte length, and policy outcome slug,
    /// but never clipboard bytes.
    pub fn append_read_audit<'a>(
        &self,
        chain: &'a mut crate::policy_audit_chain::AuditChain,
        targets: &str,
        source_pane: u64,
        timestamp_ms: u64,
    ) -> &'a crate::policy_audit_chain::AuditChainEntry {
        let (decoded_bytes, decision) = match self {
            Self::Allowed(response) => (response.decoded_len(), Osc52AuditDecision::Allowed),
            Self::Denied(_) => (
                Osc52ReadResponse::<Denied>::decoded_len(),
                Osc52AuditDecision::Denied {
                    reason: Osc52DenyReason::ReadDefaultDeny,
                },
            ),
            Self::Prompted(response) => (response.decoded_len(), Osc52AuditDecision::Prompted),
        };
        let event = Osc52AuditEvent {
            direction: Osc52Direction::Read,
            targets: parse_osc52_targets(&sanitize_osc52_targets(targets)),
            decoded_bytes,
            decision,
            source_pane,
            timestamp_ms,
        };
        event.append_to_chain(chain)
    }
}

impl Osc52ReadResponse<Prompted> {
    #[must_use]
    fn decoded_len(&self) -> u64 {
        self.bytes
            .as_ref()
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    /// Operator clicked "allow" in the prompt UI. Resolves
    /// to `Allowed` carrying the clipboard bytes.
    #[must_use]
    pub fn confirmed_by_operator(self) -> Osc52ReadResponse<Allowed> {
        Osc52ReadResponse {
            bytes: self.bytes,
            _stage: PhantomData,
        }
    }

    /// Operator clicked "deny" in the prompt UI. Resolves
    /// to `Denied` and drops the clipboard bytes (privacy
    /// invariant matches the direct-Deny policy path).
    #[must_use]
    pub fn denied_by_operator(self) -> Osc52ReadResponse<Denied> {
        Osc52ReadResponse {
            bytes: None,
            _stage: PhantomData,
        }
    }

    /// Operator-allow with a "remember-this-session"
    /// box checked — same as confirmed_by_operator() but
    /// the integration's session-policy cache records the
    /// allow so subsequent reads skip the prompt.
    /// Foundation slice ships the resolution shape; the
    /// integration owns the cache.
    #[must_use]
    pub fn confirmed_for_session(self) -> Osc52ReadResponse<Allowed> {
        self.confirmed_by_operator()
    }
}

impl Osc52ReadResponse<Allowed> {
    #[must_use]
    fn decoded_len(&self) -> u64 {
        self.bytes
            .as_ref()
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    /// Emit the OSC 52 response with base64-encoded clipboard bytes.
    ///
    /// Format: `\x1b]52;<targets>;<base64>\x1b\\`
    ///
    /// `targets` is sanitized via [`sanitize_osc52_targets`]
    /// to prevent **OSC envelope injection** — a malicious
    /// caller could otherwise pass `targets = "c\x1b\\"`
    /// causing the embedded ST terminator to close the
    /// OSC envelope early and inject the rest as terminal
    /// output. Invalid characters are dropped; if no
    /// valid targets remain, falls back to `c`
    /// (clipboard).
    #[must_use]
    pub fn emit_base64(&self, targets: &str) -> Vec<u8> {
        let safe_targets = sanitize_osc52_targets(targets);
        let mut out = Vec::from(&b"\x1b]52;"[..]);
        out.extend_from_slice(safe_targets.as_bytes());
        out.push(b';');
        if let Some(ref bytes) = self.bytes {
            out.extend(
                base64::engine::general_purpose::STANDARD
                    .encode(bytes)
                    .bytes(),
            );
        }
        out.extend_from_slice(&b"\x1b\\"[..]);
        out
    }
}

/// Sanitize an OSC 52 `targets` string to prevent envelope
/// injection. The OSC 52 protocol defines targets as
/// single-character codes: `c` (clipboard), `p` (primary
/// selection), `s` (selection), `0`-`7` (cut buffers).
///
/// Anything outside this alphabet is dropped — including
/// `\x1b` (ESC), `\x07` (BEL), `\\`, `;`, and other chars
/// that could break out of the OSC envelope.
///
/// If sanitization leaves no targets, falls back to
/// `c` (clipboard). This prevents both injection AND a
/// downstream parser confusion with empty target string.
#[must_use]
pub fn sanitize_osc52_targets(targets: &str) -> String {
    let mut out: String = targets
        .chars()
        .filter(|c| matches!(c, 'c' | 'p' | 's' | '0'..='7'))
        .collect();
    if out.is_empty() {
        out.push('c');
    }
    out
}

impl Osc52ReadResponse<Denied> {
    #[must_use]
    const fn decoded_len() -> u64 {
        0
    }

    /// Emit the OSC 52 response with empty payload.
    /// Privacy rule: "When denied, respond with empty
    /// payload (don't reveal the clipboard exists)."
    ///
    /// Note: the `Denied` typed-state has *no* method
    /// that takes a base64 closure or clipboard bytes —
    /// the privacy rule is structural. Even if a future
    /// developer mistakenly tries to emit clipboard
    /// content here, the type system rejects them.
    #[must_use]
    pub fn emit_empty(&self, targets: &str) -> Vec<u8> {
        let safe_targets = sanitize_osc52_targets(targets);
        let mut out = Vec::from(&b"\x1b]52;"[..]);
        out.extend_from_slice(safe_targets.as_bytes());
        out.extend_from_slice(&b";\x1b\\"[..]);
        out
    }
}

// ============================================================================
// A11Y announcement shape (DO NOT BREAK rule)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum A11yAnnouncementShape {
    /// OSC 8: "Link to <uri>" announced on focus/hover.
    HyperlinkFocus { id: HyperlinkId, uri: String },
    /// OSC 22: "Cursor shape changed to <shape>".
    CursorShapeChange { pane_id: u64, shape_slug: String },
    /// OSC 52: "Clipboard write blocked / allowed".
    /// (Bead's "DO NOT BREAK" rule: existing manual Cmd-C
    /// path unaffected — this announcement is for OSC 52
    /// programmatic ops only.)
    ClipboardPolicyDecision {
        decision_slug: String,
        bytes_in: u32,
    },
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OscIntegrationHealth {
    pub osc8_hyperlinks_admitted_total: u64,
    pub osc8_clicks_dispatched_total: u64,
    pub osc8_select_instead_total: u64,
    pub osc22_cursor_changes_total: u64,
    pub osc22_panes_with_state: u32,
    pub osc52_writes_allowed_total: u64,
    pub osc52_writes_denied_total: u64,
    pub osc52_reads_allowed_total: u64,
    pub osc52_reads_denied_total: u64,
    pub a11y_announcements_total: u64,
    pub interactions_by_kind: BTreeMap<String, u64>,
}

impl OscIntegrationHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    pub fn record_hyperlink_interaction(&mut self, interaction: &HyperlinkInteraction) {
        let slug = match interaction {
            HyperlinkInteraction::Hovering { .. } => "hover",
            HyperlinkInteraction::OpenUrl { .. } => "open_url",
            HyperlinkInteraction::SelectInstead { .. } => "select_instead",
            HyperlinkInteraction::NotOverHyperlink => "not_over",
        };
        *self
            .interactions_by_kind
            .entry(slug.to_string())
            .or_insert(0) += 1;
        match interaction {
            HyperlinkInteraction::OpenUrl { .. } => {
                self.osc8_clicks_dispatched_total =
                    self.osc8_clicks_dispatched_total.saturating_add(1);
            }
            HyperlinkInteraction::SelectInstead { .. } => {
                self.osc8_select_instead_total = self.osc8_select_instead_total.saturating_add(1);
            }
            _ => {}
        }
    }

    pub fn record_cursor_change(&mut self, panes_with_state: u32) {
        self.osc22_cursor_changes_total = self.osc22_cursor_changes_total.saturating_add(1);
        self.osc22_panes_with_state = panes_with_state;
    }

    pub fn record_a11y_announcement(&mut self) {
        self.a11y_announcements_total = self.a11y_announcements_total.saturating_add(1);
    }

    /// True iff the integration is in a healthy state.
    ///
    /// Currently checks:
    /// - **A11Y announcement coverage**: every interaction
    ///   (open_url + select_instead) produced an A11Y
    ///   announcement (the bead's "hyperlink target announced
    ///   on focus/hover" rule).
    ///
    /// **Privacy invariant for OSC 52** (reads on Deny policy
    /// must produce empty payloads) is enforced **structurally**
    /// by the [`Osc52ReadResponse<Denied>`] typed-state — the
    /// Denied stage has no method that takes payload bytes, so
    /// `emit_empty` is the only way to produce a wire-format
    /// response from that stage. There is no runtime
    /// double-check via this function; the type system is the
    /// load-bearing enforcement. (Per ft-uea9o: the previous
    /// docstring claimed a "doctor surface double-checks" that
    /// wasn't actually implemented. If a future refactor relaxes
    /// the typed-state guarantee, a runtime counter on the
    /// Denied emit path should be added here.)
    #[must_use]
    pub fn is_safe(&self) -> bool {
        let total_interactions = self.osc8_clicks_dispatched_total + self.osc8_select_instead_total;
        if total_interactions > 0 && self.a11y_announcements_total < total_interactions {
            return false;
        }
        true
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: u32, line_a: u32, col_a: u32, line_b: u32, col_b: u32) -> HyperlinkSpan {
        HyperlinkSpan {
            id,
            start: CellCoord {
                line: line_a,
                col: col_a,
            },
            end_exclusive: CellCoord {
                line: line_b,
                col: col_b,
            },
            uri: "https://example.com".to_string(),
        }
    }

    // ------------------------------------------------------------------------
    // OSC 8 spans + dispatch
    // ------------------------------------------------------------------------

    #[test]
    fn span_contains_in_range() {
        let s = span(1, 0, 0, 0, 5);
        assert!(s.contains(CellCoord { line: 0, col: 3 }));
    }

    #[test]
    fn span_does_not_contain_end_exclusive() {
        let s = span(1, 0, 0, 0, 5);
        assert!(!s.contains(CellCoord { line: 0, col: 5 }));
    }

    #[test]
    fn span_does_not_contain_before_start() {
        let s = span(1, 0, 5, 0, 10);
        assert!(!s.contains(CellCoord { line: 0, col: 4 }));
    }

    #[test]
    fn intra_line_cell_count() {
        let s = span(1, 0, 5, 0, 10);
        assert_eq!(s.intra_line_cell_count(), Some(5));
        assert!(!s.is_multi_line());
    }

    #[test]
    fn multi_line_cell_count_returns_none() {
        // Previous version silently returned 0 — now
        // returns None so callers cannot mistake "0 cells"
        // for "no cells covered".
        let s = span(1, 0, 5, 2, 3);
        assert_eq!(s.intra_line_cell_count(), None);
        assert!(s.is_multi_line());
    }

    #[test]
    fn click_without_span_is_not_over_hyperlink() {
        let result = dispatch_click(None, false);
        assert_eq!(result, HyperlinkInteraction::NotOverHyperlink);
    }

    #[test]
    fn click_with_span_no_modifier_opens_url() {
        let s = span(42, 0, 0, 0, 5);
        let result = dispatch_click(Some(&s), false);
        assert_eq!(
            result,
            HyperlinkInteraction::OpenUrl {
                id: 42,
                uri: "https://example.com".to_string()
            }
        );
    }

    #[test]
    fn click_with_modifier_falls_through_to_select() {
        let s = span(42, 0, 0, 0, 5);
        let result = dispatch_click(Some(&s), true);
        assert_eq!(result, HyperlinkInteraction::SelectInstead { id: 42 });
    }

    // ------------------------------------------------------------------------
    // OSC 22 per-pane cursor map
    // ------------------------------------------------------------------------

    #[test]
    fn cursor_map_default_for_unknown_pane() {
        let m = Osc22PerPaneCursorMap::new();
        assert_eq!(m.get(7), CursorShapeSlug::Default);
    }

    #[test]
    fn cursor_map_set_returns_prior() {
        let mut m = Osc22PerPaneCursorMap::new();
        let prior = m.set(7, CursorShapeSlug::BlockSteady);
        assert!(prior.is_none());
        let prior = m.set(7, CursorShapeSlug::BarBlinking);
        assert_eq!(prior, Some(CursorShapeSlug::BlockSteady));
    }

    #[test]
    fn cursor_map_changes_counter_only_increments_on_real_change() {
        let mut m = Osc22PerPaneCursorMap::new();
        m.set(7, CursorShapeSlug::BlockSteady);
        m.set(7, CursorShapeSlug::BlockSteady); // same — no count
        m.set(7, CursorShapeSlug::BarBlinking);
        assert_eq!(m.changes_total, 2);
    }

    #[test]
    fn cursor_map_persists_across_pane_focus() {
        let mut m = Osc22PerPaneCursorMap::new();
        m.set(7, CursorShapeSlug::BarSteady);
        m.set(8, CursorShapeSlug::BlockBlinking);
        // Per-pane state preserved.
        assert_eq!(m.get(7), CursorShapeSlug::BarSteady);
        assert_eq!(m.get(8), CursorShapeSlug::BlockBlinking);
    }

    #[test]
    fn cursor_map_forget_drops_state_on_pane_close() {
        let mut m = Osc22PerPaneCursorMap::new();
        m.set(7, CursorShapeSlug::BarSteady);
        let dropped = m.forget(7);
        assert_eq!(dropped, Some(CursorShapeSlug::BarSteady));
        assert_eq!(m.get(7), CursorShapeSlug::Default);
    }

    #[test]
    fn each_cursor_shape_has_distinct_slug() {
        let shapes = [
            CursorShapeSlug::Default,
            CursorShapeSlug::BlockBlinking,
            CursorShapeSlug::BlockSteady,
            CursorShapeSlug::UnderlineBlinking,
            CursorShapeSlug::UnderlineSteady,
            CursorShapeSlug::BarBlinking,
            CursorShapeSlug::BarSteady,
        ];
        let slugs: Vec<&str> = shapes.iter().map(|s| s.slug()).collect();
        let unique: std::collections::HashSet<_> = slugs.iter().collect();
        assert_eq!(unique.len(), 7);
    }

    // ------------------------------------------------------------------------
    // OSC 52 typed-state pipeline
    // ------------------------------------------------------------------------

    #[test]
    fn osc52_allowed_path_emits_base64() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Allow);
        let emitted = match gated {
            Osc52PolicyGated::Allowed(allowed) => allowed.emit_base64("c"),
            Osc52PolicyGated::Denied(_) => panic!("expected Allowed"),
            Osc52PolicyGated::Prompted(_) => panic!("expected Allowed"),
        };
        assert_eq!(emitted, b"\x1b]52;c;c2VjcmV0\x1b\\");
    }

    #[test]
    fn osc52_denied_path_emits_empty_payload() {
        // Privacy rule: "Read path: when denied, respond
        // with empty payload (don't reveal the clipboard
        // exists)."
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Deny);
        let emitted = match gated {
            Osc52PolicyGated::Denied(denied) => denied.emit_empty("c"),
            Osc52PolicyGated::Allowed(_) => panic!("expected Denied"),
            Osc52PolicyGated::Prompted(_) => panic!("expected Denied"),
        };
        assert_eq!(emitted, b"\x1b]52;c;\x1b\\");
        // Note: response is `\x1b]52;c;\x1b\\` — same
        // form as a clipboard miss, so a malicious app
        // cannot tell whether the clipboard was empty,
        // contained data, or was denied.
    }

    #[test]
    fn sanitize_targets_keeps_valid_chars() {
        assert_eq!(sanitize_osc52_targets("c"), "c");
        assert_eq!(sanitize_osc52_targets("cps"), "cps");
        assert_eq!(sanitize_osc52_targets("01234567"), "01234567");
    }

    #[test]
    fn sanitize_targets_drops_injection_chars() {
        // SECURITY REGRESSION TEST: previously, targets
        // was passed unvalidated, allowing an embedded
        // \x1b\\ to close the OSC envelope early and
        // inject the rest as terminal output.
        assert_eq!(sanitize_osc52_targets("c\x1b\\"), "c");
        assert_eq!(sanitize_osc52_targets("c\x07"), "c");
        // Drop ';' (target separator within OSC envelope)
        // + non-target chars. Note: 'c' inside "injection"
        // would survive, so use a string with no other
        // valid chars.
        assert_eq!(sanitize_osc52_targets("c;XYZ"), "c");
        assert_eq!(sanitize_osc52_targets("c\\x"), "c");
        // Multi-target preservation:
        assert_eq!(sanitize_osc52_targets("cp\x1b\\s"), "cps");
    }

    #[test]
    fn sanitize_targets_falls_back_to_clipboard_when_empty() {
        // Empty targets after sanitization defaults to "c"
        // — preserves a valid OSC envelope shape and
        // avoids downstream parser confusion.
        assert_eq!(sanitize_osc52_targets(""), "c");
        assert_eq!(sanitize_osc52_targets("\x1b\\"), "c");
        assert_eq!(sanitize_osc52_targets("XYZ"), "c");
    }

    #[test]
    fn osc52_emit_base64_sanitizes_injection_attempt() {
        // SECURITY REGRESSION TEST: a malicious caller
        // passes targets containing \x1b\\ (ST). Before
        // the fix this would inject "INJECTED" into the
        // terminal output stream; after the fix the
        // injection chars are stripped and the OSC
        // envelope stays well-formed.
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"data".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Allow);
        let emitted = match gated {
            Osc52PolicyGated::Allowed(allowed) => allowed.emit_base64("c\x1b\\INJECTED"),
            _ => panic!("expected Allowed"),
        };
        let s = String::from_utf8_lossy(&emitted);
        assert!(!s.contains("INJECTED"));
        // Envelope should be exactly one OSC sequence.
        assert_eq!(emitted.iter().filter(|&&b| b == 0x1b).count(), 2);
    }

    #[test]
    fn osc52_allowed_with_empty_clipboard_emits_envelope_with_no_payload() {
        // Coverage gap: when the operator's clipboard is
        // genuinely empty (not denied), the Allowed-path
        // emit produces the same envelope shape as Denied
        // (\x1b]52;c;\x1b\\). Pin this so a future
        // maintainer doesn't "fix" by emitting a distinct
        // shape — that would break the privacy invariant
        // (responses to Deny + responses to genuine-empty
        // must look identical so a malicious app cannot
        // distinguish "operator denied" from "empty
        // clipboard").
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(Vec::new());
        let gated = response.policy_gate(Osc52PolicySlug::Allow);
        let emitted = match gated {
            Osc52PolicyGated::Allowed(allowed) => allowed.emit_base64("c"),
            _ => panic!("expected Allowed"),
        };
        assert_eq!(emitted, b"\x1b]52;c;\x1b\\");
    }

    #[test]
    fn osc52_emit_empty_sanitizes_injection_attempt() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Deny);
        let emitted = match gated {
            Osc52PolicyGated::Denied(denied) => denied.emit_empty("c\x1b\\;BAD\x07"),
            _ => panic!("expected Denied"),
        };
        let s = String::from_utf8_lossy(&emitted);
        assert!(!s.contains("BAD"));
        // Envelope ends cleanly — exactly two ESC bytes
        // (one for OSC start, one for ST end).
        assert_eq!(emitted.iter().filter(|&&b| b == 0x1b).count(), 2);
    }

    #[test]
    fn osc52_prompt_returns_distinct_prompted_state() {
        // Prompt now returns a Prompted state — NOT
        // Allowed. Previously this returned Allowed which
        // was a privacy hole: a maintainer could call
        // emit_base64 immediately, bypassing the
        // prompt UI.
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Prompt);
        assert!(matches!(gated, Osc52PolicyGated::Prompted(_)));
    }

    #[test]
    fn osc52_prompted_confirmed_by_operator_yields_allowed() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Prompt);
        let allowed = match gated {
            Osc52PolicyGated::Prompted(p) => p.confirmed_by_operator(),
            _ => panic!("expected Prompted"),
        };
        let emitted = allowed.emit_base64("c");
        assert_eq!(emitted, b"\x1b]52;c;c2VjcmV0\x1b\\");
    }

    #[test]
    fn osc52_prompted_denied_by_operator_yields_empty_response() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Prompt);
        let denied = match gated {
            Osc52PolicyGated::Prompted(p) => p.denied_by_operator(),
            _ => panic!("expected Prompted"),
        };
        let emitted = denied.emit_empty("c");
        assert_eq!(emitted, b"\x1b]52;c;\x1b\\");
        let s = String::from_utf8_lossy(&emitted);
        assert!(!s.contains("secret"));
    }

    #[test]
    fn osc52_confirmed_for_session_also_yields_allowed() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Prompt);
        let allowed = match gated {
            Osc52PolicyGated::Prompted(p) => p.confirmed_for_session(),
            _ => panic!("expected Prompted"),
        };
        let emitted = allowed.emit_base64("c");
        assert_eq!(emitted, b"\x1b]52;c;c2VjcmV0\x1b\\");
    }

    #[test]
    fn osc52_write_audit_appends_allowed_decision_without_payload() {
        let mut chain = crate::policy_audit_chain::AuditChain::new(4);
        let entry = append_osc52_write_audit(
            &mut chain,
            "c\x1b\\INJECTED",
            6,
            Osc52WriteAuditOutcome::Allowed,
            17,
            1_700_000_000_000,
        )
        .clone();

        assert_eq!(
            entry.kind,
            crate::policy_audit_chain::AuditEntryKind::Osc52Action
        );
        assert_eq!(entry.actor, "pane-17");
        assert_eq!(
            entry.description,
            "osc52 write targets=1 bytes=6 decision=allowed"
        );
        assert_eq!(entry.entity_ref, "osc52:pane-17");
        assert!(!entry.description.contains("secret"));
        assert!(!entry.description.contains("INJECTED"));
    }

    #[test]
    fn osc52_write_audit_maps_denial_reasons_to_slugs() {
        let mut chain = crate::policy_audit_chain::AuditChain::new(4);
        let oversized = append_osc52_write_audit(
            &mut chain,
            "cp",
            2_000_000,
            Osc52WriteAuditOutcome::DeniedOversized,
            3,
            10,
        )
        .clone();
        let invalid_base64 = append_osc52_write_audit(
            &mut chain,
            "c",
            0,
            Osc52WriteAuditOutcome::DeniedInvalidBase64,
            3,
            11,
        )
        .clone();

        assert_eq!(
            oversized.description,
            "osc52 write targets=2 bytes=2000000 decision=denied:oversized"
        );
        assert_eq!(
            invalid_base64.description,
            "osc52 write targets=1 bytes=0 decision=denied:invalid_base64"
        );
        assert_eq!(invalid_base64.previous_hash, oversized.chain_hash);
    }

    #[test]
    fn osc52_read_audit_appends_policy_gate_decision() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Allow);
        let mut chain = crate::policy_audit_chain::AuditChain::new(4);
        let entry = gated
            .append_read_audit(&mut chain, "p;BAD", 42, 1_000_001)
            .clone();

        assert_eq!(entry.actor, "pane-42");
        assert_eq!(
            entry.description,
            "osc52 read targets=1 bytes=6 decision=allowed"
        );
        assert_eq!(entry.entity_ref, "osc52:pane-42");
        assert!(!entry.description.contains("secret"));
        assert!(!entry.description.contains("BAD"));
    }

    #[test]
    fn osc52_read_audit_denied_records_empty_payload_shape() {
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"secret".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Deny);
        let mut chain = crate::policy_audit_chain::AuditChain::new(4);
        let entry = gated
            .append_read_audit(&mut chain, "", 5, 1_000_002)
            .clone();

        assert_eq!(
            entry.description,
            "osc52 read targets=1 bytes=0 decision=denied:read_default_deny"
        );
    }

    // The privacy rule for Prompted is structural — these
    // patterns do not compile:
    //
    // // Cannot emit_base64 from Prompted state:
    // let prompted: Osc52ReadResponse<Prompted> = ...;
    // prompted.emit_base64("c");
    // // ^^^ compile error: emit_base64 only on Allowed
    //
    // // Cannot emit_empty from Prompted state:
    // let prompted: Osc52ReadResponse<Prompted> = ...;
    // prompted.emit_empty("c");
    // // ^^^ compile error: emit_empty only on Denied

    // The privacy invariant is structural — these patterns
    // do not compile:
    //
    // // Cannot read clipboard bytes from Denied state:
    // let denied: Osc52ReadResponse<Denied> = ...;
    // let bytes = denied.bytes; // private field, no public reader
    //
    // // Cannot emit_base64 from Denied state:
    // let denied: Osc52ReadResponse<Denied> = ...;
    // denied.emit_base64("c");
    // // ^^^ compile error: emit_base64 only on Allowed

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_safe() {
        assert!(OscIntegrationHealth::baseline().is_safe());
    }

    #[test]
    fn health_records_per_kind_interactions() {
        let mut h = OscIntegrationHealth::baseline();
        let span = span(1, 0, 0, 0, 5);
        let open = HyperlinkInteraction::OpenUrl {
            id: 1,
            uri: "https://example.com".to_string(),
        };
        let select = HyperlinkInteraction::SelectInstead { id: 1 };
        let _ = span;
        h.record_hyperlink_interaction(&open);
        h.record_hyperlink_interaction(&open);
        h.record_hyperlink_interaction(&select);
        assert_eq!(h.interactions_by_kind.get("open_url"), Some(&2));
        assert_eq!(h.interactions_by_kind.get("select_instead"), Some(&1));
        assert_eq!(h.osc8_clicks_dispatched_total, 2);
        assert_eq!(h.osc8_select_instead_total, 1);
    }

    #[test]
    fn health_records_cursor_changes() {
        let mut h = OscIntegrationHealth::baseline();
        h.record_cursor_change(5);
        h.record_cursor_change(7);
        assert_eq!(h.osc22_cursor_changes_total, 2);
        assert_eq!(h.osc22_panes_with_state, 7);
    }

    #[test]
    fn health_unsafe_when_interactions_exceed_a11y_announcements() {
        // Bead's "hyperlink target announced on focus/
        // hover" rule: every interaction must produce an
        // announcement.
        let mut h = OscIntegrationHealth::baseline();
        let open = HyperlinkInteraction::OpenUrl {
            id: 1,
            uri: "https://example.com".to_string(),
        };
        h.record_hyperlink_interaction(&open);
        // No record_a11y_announcement call — should be unsafe.
        assert!(!h.is_safe());
    }

    #[test]
    fn health_safe_when_a11y_announcements_cover_interactions() {
        let mut h = OscIntegrationHealth::baseline();
        let open = HyperlinkInteraction::OpenUrl {
            id: 1,
            uri: "https://example.com".to_string(),
        };
        h.record_hyperlink_interaction(&open);
        h.record_a11y_announcement();
        assert!(h.is_safe());
    }

    // ------------------------------------------------------------------------
    // Headline scenarios
    // ------------------------------------------------------------------------

    #[test]
    fn click_hyperlink_open_url_scenario() {
        // imgcat's emitted OSC 8 link clicked without
        // modifier → opens URL.
        let s = span(42, 5, 10, 5, 30);
        let interaction = dispatch_click(Some(&s), false);
        match interaction {
            HyperlinkInteraction::OpenUrl { id, uri } => {
                assert_eq!(id, 42);
                assert_eq!(uri, "https://example.com");
            }
            _ => panic!("expected OpenUrl"),
        }
    }

    #[test]
    fn osc52_read_with_deny_policy_scenario() {
        // Bead headline: a malicious app emits OSC 52
        // read; operator policy is Deny; integration
        // responds with empty payload (cannot leak).
        let response = Osc52ReadResponse::<Decoded>::from_clipboard(b"PRIVATE".to_vec());
        let gated = response.policy_gate(Osc52PolicySlug::Deny);
        match gated {
            Osc52PolicyGated::Denied(denied) => {
                let emitted = denied.emit_empty("c");
                // Crucially: emitted bytes contain no
                // trace of "PRIVATE".
                let s = String::from_utf8_lossy(&emitted);
                assert!(!s.contains("PRIVATE"));
                assert!(!s.contains("UFJJVkFURQ")); // base64("PRIVATE")
            }
            _ => panic!("expected Denied"),
        }
    }

    #[test]
    fn cursor_shape_persists_across_focus_change_scenario() {
        // Bead headline: pane 7 sets BarSteady cursor;
        // focus shifts to pane 8 (which sets
        // BlockBlinking); focus returns to pane 7 →
        // BarSteady still applied.
        let mut m = Osc22PerPaneCursorMap::new();
        m.set(7, CursorShapeSlug::BarSteady);
        m.set(8, CursorShapeSlug::BlockBlinking);
        // Focus returns to pane 7:
        assert_eq!(m.get(7), CursorShapeSlug::BarSteady);
    }

    #[test]
    fn cursor_map_serde_roundtrip() {
        let mut m = Osc22PerPaneCursorMap::new();
        m.set(7, CursorShapeSlug::BarSteady);
        m.set(8, CursorShapeSlug::BlockBlinking);
        let json = serde_json::to_string(&m).unwrap();
        let parsed: Osc22PerPaneCursorMap = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, m);
    }
}
