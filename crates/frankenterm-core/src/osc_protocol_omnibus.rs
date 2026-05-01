//! OSC 8 / OSC 22 / OSC 52 substrate (ft-2okh0.1.5).
//!
//! Pure-logic substrate for the bead's three-OSC omnibus.
//! `Osc52Policy` already lives in `smart_selection.rs`; this
//! module adds the missing pieces:
//!
//! - OSC 8: hyperlink-id allocator + per-cell tag + URI
//!   classification + hover-state policy.
//! - OSC 22: cursor shape parser + persistence policy.
//! - OSC 52: clipboard target enum + size-cap gate + audit
//!   event payload.
//!
//! ## What this module ships
//!
//! - `HyperlinkId(u32)` opaque tag stored on cells.
//! - `HyperlinkUri` validated URI wrapper with scheme
//!   classification (`Http / Https / Ftp / Mailto / File /
//!   Other`).
//! - `Osc8Open` / `Osc8Close` parser-output enums.
//! - `HyperlinkRegistry` policy substrate with allocate /
//!   resolve / drop / capacity cap.
//! - `Osc8HoverDecision` — `ShowStatus / Suppress` for
//!   non-Http(s) URIs the operator may want to hide.
//! - `Osc22CursorShape` — `Block / Underline / Bar / Default`
//!   per the bead.
//! - `parse_osc22_payload` — pure-logic parser returning
//!   `Option<Osc22CursorShape>`.
//! - `Osc52Target` — `Clipboard / Primary / Selection /
//!   BufferCut` per the bead's `c / p / s / b` field.
//! - `parse_osc52_targets` over the comma-list field.
//! - `Osc52SizeCapDecision` — `Approved / RejectedOversized`.
//! - `Osc52AuditEvent` — payload the integration writes to
//!   `policy_audit_chain` (cross-link BR-RC-DOCTRINE).
//! - `OmnibusOscTelemetry` — bead's structured-logging
//!   counters (per-OSC + per-decision).
//!
//! ## What is deferred to ft-2okh0.1.5.cont
//!
//! - Wiring the OSC 8 parser into `escape-parser/src/osc.rs`.
//! - Per-cell hyperlink_id storage on `Cell`.
//! - Hover-state UI: status-bar URI display + cursor change.
//! - Click: dispatch into `frankenterm/open-url`.
//! - OSC 22 application to the per-pane cursor renderer.
//! - OSC 52 prompt UX (allow-once / allow-session).
//! - `policy_audit_chain` writes for every OSC 52 decision.
//! - Smart-selection integration for hyperlink-span clicks
//!   (cross-link 2.14).

#![allow(dead_code)]

// ============================================================================
// OSC 8 — Hyperlinks
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct HyperlinkId(pub u32);

impl HyperlinkId {
    /// `0` is reserved as "no hyperlink"; substrate hands out
    /// ids starting at `1`.
    pub const NONE: Self = Self(0);

    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// Validated URI. The integration's parser does the heavy
/// lifting; substrate keeps the post-parse string + scheme
/// classification for hover-policy decisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HyperlinkUri {
    pub raw: String,
    pub scheme: HyperlinkScheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HyperlinkScheme {
    Http,
    Https,
    Ftp,
    Mailto,
    File,
    /// Anything else — operator may want to suppress hover
    /// hints for non-standard schemes.
    #[default]
    Other,
}

impl HyperlinkScheme {
    #[must_use]
    pub fn classify(uri: &str) -> Self {
        let lower_prefix: String = uri.chars().take(8).collect::<String>().to_lowercase();
        if lower_prefix.starts_with("https://") {
            Self::Https
        } else if lower_prefix.starts_with("http://") {
            Self::Http
        } else if lower_prefix.starts_with("ftp://") {
            Self::Ftp
        } else if lower_prefix.starts_with("mailto:") {
            Self::Mailto
        } else if lower_prefix.starts_with("file://") {
            Self::File
        } else {
            Self::Other
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Ftp => "ftp",
            Self::Mailto => "mailto",
            Self::File => "file",
            Self::Other => "other",
        }
    }

    /// Whether this scheme is "trusted enough to surface
    /// without operator opt-in" (https/http/mailto). Other
    /// schemes follow `Osc8HoverPolicy`.
    #[must_use]
    pub const fn is_well_known(self) -> bool {
        matches!(self, Self::Http | Self::Https | Self::Mailto)
    }
}

impl HyperlinkUri {
    #[must_use]
    pub fn new(raw: String) -> Self {
        let scheme = HyperlinkScheme::classify(&raw);
        Self { raw, scheme }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc8Output {
    /// `\x1b]8;<params>;<URI>\x1b\\` — open a new hyperlink
    /// span. Subsequent cells get tagged with the resulting
    /// `HyperlinkId`.
    Open { uri: HyperlinkUri, params: String },
    /// `\x1b]8;;\x1b\\` — close the active hyperlink span.
    Close,
    /// Malformed payload.
    Malformed,
}

/// Operator policy for hover-state UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Osc8HoverPolicy {
    /// Show hover status-bar text for every URI.
    AllSchemes,
    /// Show only well-known schemes (http/https/mailto).
    /// Other schemes: still clickable but no hover hint.
    #[default]
    WellKnownOnly,
    /// Never show hover hint (operator may have a custom
    /// mechanism).
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc8HoverDecision {
    ShowStatus,
    Suppress,
}

/// Pure decision: should the GUI show hover-state for this
/// URI?
#[must_use]
pub fn osc8_hover_decision(
    uri: &HyperlinkUri,
    policy: Osc8HoverPolicy,
) -> Osc8HoverDecision {
    match policy {
        Osc8HoverPolicy::AllSchemes => Osc8HoverDecision::ShowStatus,
        Osc8HoverPolicy::Suppressed => Osc8HoverDecision::Suppress,
        Osc8HoverPolicy::WellKnownOnly => {
            if uri.scheme.is_well_known() {
                Osc8HoverDecision::ShowStatus
            } else {
                Osc8HoverDecision::Suppress
            }
        }
    }
}

/// Hyperlink registry — assigns unique ids and resolves them
/// back to URIs at hover/click time. Capped to prevent a
/// hostile stream from burning unbounded memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkRegistry {
    entries: Vec<HyperlinkUri>,
    cap: usize,
}

pub const DEFAULT_HYPERLINK_REGISTRY_CAP: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HyperlinkAllocOutcome {
    Allocated(HyperlinkId),
    /// Registry full — substrate refuses; integration drops
    /// the link tag and renders the cells without a hyperlink.
    DeniedFull,
}

impl HyperlinkRegistry {
    #[must_use]
    pub const fn new(cap: usize) -> Self {
        Self { entries: Vec::new(), cap }
    }

    pub fn allocate(&mut self, uri: HyperlinkUri) -> HyperlinkAllocOutcome {
        if self.entries.len() >= self.cap {
            return HyperlinkAllocOutcome::DeniedFull;
        }
        self.entries.push(uri);
        // Index 0 reserved as NONE; first allocation gets id=1.
        let id = HyperlinkId(self.entries.len() as u32);
        HyperlinkAllocOutcome::Allocated(id)
    }

    #[must_use]
    pub fn resolve(&self, id: HyperlinkId) -> Option<&HyperlinkUri> {
        if id.is_none() {
            return None;
        }
        self.entries.get((id.0 as usize).saturating_sub(1))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// OSC 22 — Cursor shape
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Osc22CursorShape {
    Block,
    Underline,
    Bar,
    /// Restore the operator-configured default.
    #[default]
    Default,
}

impl Osc22CursorShape {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Underline => "underline",
            Self::Bar => "bar",
            Self::Default => "default",
        }
    }
}

/// Parse the OSC 22 payload (`<shape>` from `\x1b]22;<shape>\x1b\\`).
/// Case-insensitive per common terminal practice.
#[must_use]
pub fn parse_osc22_payload(payload: &str) -> Option<Osc22CursorShape> {
    match payload.trim().to_ascii_lowercase().as_str() {
        "block" => Some(Osc22CursorShape::Block),
        "underline" | "underscore" => Some(Osc22CursorShape::Underline),
        "bar" | "vbar" | "vertical-bar" | "ibeam" => Some(Osc22CursorShape::Bar),
        "default" | "" => Some(Osc22CursorShape::Default),
        _ => None,
    }
}

// ============================================================================
// OSC 52 — Clipboard
// ============================================================================

/// Per the bead's `c / p / s / b` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52Target {
    /// `c` — system clipboard.
    Clipboard,
    /// `p` — primary selection (X11).
    Primary,
    /// `s` — selection (variant of primary).
    Selection,
    /// `b` — cut buffer.
    BufferCut,
}

impl Osc52Target {
    #[must_use]
    pub const fn from_letter(letter: char) -> Option<Self> {
        match letter {
            'c' => Some(Self::Clipboard),
            'p' => Some(Self::Primary),
            's' => Some(Self::Selection),
            'b' => Some(Self::BufferCut),
            _ => None,
        }
    }

    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Clipboard => 'c',
            Self::Primary => 'p',
            Self::Selection => 's',
            Self::BufferCut => 'b',
        }
    }
}

/// Parse the comma-list `<clipboards>` field (`c,p`, `cs`, etc.
/// — common variants accept either separator or no separator).
/// Returns the targets in stable (Clipboard < Primary <
/// Selection < BufferCut) order with duplicates removed.
#[must_use]
pub fn parse_osc52_targets(field: &str) -> Vec<Osc52Target> {
    let mut seen = [false; 4];
    for ch in field.chars() {
        if let Some(target) = Osc52Target::from_letter(ch) {
            let idx = match target {
                Osc52Target::Clipboard => 0,
                Osc52Target::Primary => 1,
                Osc52Target::Selection => 2,
                Osc52Target::BufferCut => 3,
            };
            seen[idx] = true;
        }
    }
    let mut out = Vec::new();
    if seen[0] { out.push(Osc52Target::Clipboard); }
    if seen[1] { out.push(Osc52Target::Primary); }
    if seen[2] { out.push(Osc52Target::Selection); }
    if seen[3] { out.push(Osc52Target::BufferCut); }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Osc52Config {
    /// Per the bead: 5 MiB default.
    pub max_payload_bytes: u64,
    /// Whether the substrate refuses non-base64-clean
    /// payloads. Default true.
    pub refuse_invalid_base64: bool,
}

pub const DEFAULT_OSC52_MAX_PAYLOAD_BYTES: u64 = 5 * 1024 * 1024;

impl Default for Osc52Config {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_OSC52_MAX_PAYLOAD_BYTES,
            refuse_invalid_base64: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52SizeCapDecision {
    /// Payload size within the cap.
    Approved,
    /// Decoded byte size exceeded the cap; integration
    /// rejects the write.
    RejectedOversized,
}

/// Pure cap gate. `decoded_bytes` is the post-base64 length.
#[must_use]
pub fn osc52_size_cap_decision(
    decoded_bytes: u64,
    config: Osc52Config,
) -> Osc52SizeCapDecision {
    if decoded_bytes > config.max_payload_bytes {
        Osc52SizeCapDecision::RejectedOversized
    } else {
        Osc52SizeCapDecision::Approved
    }
}

/// Audit-trail payload for `policy_audit_chain` (cross-link
/// BR-RC-DOCTRINE policy.rs). Substrate carries the schema;
/// the integration writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Osc52AuditEvent {
    pub direction: Osc52Direction,
    pub targets: Vec<Osc52Target>,
    pub decoded_bytes: u64,
    pub decision: Osc52AuditDecision,
    pub source_pane: u64,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52Direction {
    /// `\x1b]52;<targets>;<base64>\x1b\\` — write to clipboard.
    Write,
    /// `\x1b]52;<targets>;?\x1b\\` — read from clipboard.
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52AuditDecision {
    Allowed,
    Denied { reason: Osc52DenyReason },
    /// Operator was prompted; decision deferred to UX layer.
    Prompted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52DenyReason {
    /// Operator config: `osc52_clipboard_write="deny"` etc.
    OperatorPolicy,
    /// Payload exceeded the cap.
    Oversized,
    /// Read attempted with default-deny policy.
    ReadDefaultDeny,
    /// Base64 decode failed.
    InvalidBase64,
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OmnibusOscTelemetry {
    // OSC 8
    pub hyperlinks_opened: u64,
    pub hyperlinks_closed: u64,
    pub hyperlink_registry_full_denials: u64,
    pub hyperlink_hovers_shown: u64,
    pub hyperlink_hovers_suppressed: u64,
    // OSC 22
    pub cursor_shape_changes: u64,
    pub cursor_shape_parse_errors: u64,
    // OSC 52
    pub osc52_writes_allowed: u64,
    pub osc52_writes_denied: u64,
    pub osc52_writes_prompted: u64,
    pub osc52_reads_attempted: u64,
    pub osc52_reads_allowed: u64,
    pub osc52_reads_denied: u64,
    pub osc52_oversized_rejections: u64,
}

impl OmnibusOscTelemetry {
    pub fn record_osc8_open(&mut self, allocated: bool) {
        if allocated {
            self.hyperlinks_opened = self.hyperlinks_opened.saturating_add(1);
        } else {
            self.hyperlink_registry_full_denials = self
                .hyperlink_registry_full_denials
                .saturating_add(1);
        }
    }

    pub fn record_osc8_close(&mut self) {
        self.hyperlinks_closed = self.hyperlinks_closed.saturating_add(1);
    }

    pub fn record_osc8_hover(&mut self, decision: Osc8HoverDecision) {
        match decision {
            Osc8HoverDecision::ShowStatus => {
                self.hyperlink_hovers_shown = self.hyperlink_hovers_shown.saturating_add(1);
            }
            Osc8HoverDecision::Suppress => {
                self.hyperlink_hovers_suppressed =
                    self.hyperlink_hovers_suppressed.saturating_add(1);
            }
        }
    }

    pub fn record_osc22(&mut self, parsed: bool) {
        if parsed {
            self.cursor_shape_changes = self.cursor_shape_changes.saturating_add(1);
        } else {
            self.cursor_shape_parse_errors =
                self.cursor_shape_parse_errors.saturating_add(1);
        }
    }

    pub fn record_osc52_audit(&mut self, event: &Osc52AuditEvent) {
        match (event.direction, event.decision) {
            (Osc52Direction::Write, Osc52AuditDecision::Allowed) => {
                self.osc52_writes_allowed = self.osc52_writes_allowed.saturating_add(1);
            }
            (Osc52Direction::Write, Osc52AuditDecision::Denied { reason }) => {
                self.osc52_writes_denied = self.osc52_writes_denied.saturating_add(1);
                if matches!(reason, Osc52DenyReason::Oversized) {
                    self.osc52_oversized_rejections =
                        self.osc52_oversized_rejections.saturating_add(1);
                }
            }
            (Osc52Direction::Write, Osc52AuditDecision::Prompted) => {
                self.osc52_writes_prompted = self.osc52_writes_prompted.saturating_add(1);
            }
            (Osc52Direction::Read, Osc52AuditDecision::Allowed) => {
                self.osc52_reads_attempted =
                    self.osc52_reads_attempted.saturating_add(1);
                self.osc52_reads_allowed = self.osc52_reads_allowed.saturating_add(1);
            }
            (Osc52Direction::Read, Osc52AuditDecision::Denied { .. }) => {
                self.osc52_reads_attempted =
                    self.osc52_reads_attempted.saturating_add(1);
                self.osc52_reads_denied = self.osc52_reads_denied.saturating_add(1);
            }
            (Osc52Direction::Read, Osc52AuditDecision::Prompted) => {
                self.osc52_reads_attempted =
                    self.osc52_reads_attempted.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // HyperlinkScheme
    // ----------------------------------------------------------------

    #[test]
    fn scheme_classify_https() {
        assert_eq!(
            HyperlinkScheme::classify("https://example.com"),
            HyperlinkScheme::Https,
        );
    }

    #[test]
    fn scheme_classify_http_lowercase() {
        assert_eq!(
            HyperlinkScheme::classify("HTTP://Example.COM"),
            HyperlinkScheme::Http,
        );
    }

    #[test]
    fn scheme_classify_mailto_file_ftp() {
        assert_eq!(HyperlinkScheme::classify("mailto:foo@bar.com"), HyperlinkScheme::Mailto);
        assert_eq!(HyperlinkScheme::classify("file:///path"), HyperlinkScheme::File);
        assert_eq!(HyperlinkScheme::classify("ftp://server"), HyperlinkScheme::Ftp);
    }

    #[test]
    fn scheme_classify_other() {
        assert_eq!(HyperlinkScheme::classify("javascript:alert"), HyperlinkScheme::Other);
        assert_eq!(HyperlinkScheme::classify("data:text/plain"), HyperlinkScheme::Other);
        assert_eq!(HyperlinkScheme::classify(""), HyperlinkScheme::Other);
    }

    #[test]
    fn scheme_is_well_known() {
        assert!(HyperlinkScheme::Http.is_well_known());
        assert!(HyperlinkScheme::Https.is_well_known());
        assert!(HyperlinkScheme::Mailto.is_well_known());
        assert!(!HyperlinkScheme::Ftp.is_well_known());
        assert!(!HyperlinkScheme::File.is_well_known());
        assert!(!HyperlinkScheme::Other.is_well_known());
    }

    // ----------------------------------------------------------------
    // HyperlinkRegistry
    // ----------------------------------------------------------------

    #[test]
    fn registry_allocate_and_resolve() {
        let mut r = HyperlinkRegistry::new(8);
        let outcome = r.allocate(HyperlinkUri::new("https://a.com".to_string()));
        let id = match outcome {
            HyperlinkAllocOutcome::Allocated(i) => i,
            HyperlinkAllocOutcome::DeniedFull => panic!("expected allocation"),
        };
        assert_eq!(id, HyperlinkId(1));
        assert_eq!(r.resolve(id).unwrap().raw, "https://a.com");
    }

    #[test]
    fn registry_resolve_none_id_returns_none() {
        let r = HyperlinkRegistry::new(8);
        assert!(r.resolve(HyperlinkId::NONE).is_none());
    }

    #[test]
    fn registry_resolve_oob_returns_none() {
        let r = HyperlinkRegistry::new(8);
        assert!(r.resolve(HyperlinkId(99)).is_none());
    }

    #[test]
    fn registry_at_cap_denies() {
        let mut r = HyperlinkRegistry::new(2);
        r.allocate(HyperlinkUri::new("a".to_string()));
        r.allocate(HyperlinkUri::new("b".to_string()));
        let outcome = r.allocate(HyperlinkUri::new("c".to_string()));
        assert_eq!(outcome, HyperlinkAllocOutcome::DeniedFull);
    }

    #[test]
    fn registry_ids_monotonic() {
        let mut r = HyperlinkRegistry::new(8);
        let mut ids = Vec::new();
        for i in 0..5 {
            if let HyperlinkAllocOutcome::Allocated(id) =
                r.allocate(HyperlinkUri::new(format!("uri{i}")))
            {
                ids.push(id);
            }
        }
        for w in ids.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    // ----------------------------------------------------------------
    // osc8_hover_decision
    // ----------------------------------------------------------------

    #[test]
    fn hover_all_schemes_always_shows() {
        let uri = HyperlinkUri::new("javascript:bad".to_string());
        let d = osc8_hover_decision(&uri, Osc8HoverPolicy::AllSchemes);
        assert_eq!(d, Osc8HoverDecision::ShowStatus);
    }

    #[test]
    fn hover_suppressed_always_suppresses() {
        let uri = HyperlinkUri::new("https://safe.com".to_string());
        let d = osc8_hover_decision(&uri, Osc8HoverPolicy::Suppressed);
        assert_eq!(d, Osc8HoverDecision::Suppress);
    }

    #[test]
    fn hover_well_known_only_filters() {
        let https = HyperlinkUri::new("https://safe.com".to_string());
        let weird = HyperlinkUri::new("data:text/plain".to_string());
        assert_eq!(
            osc8_hover_decision(&https, Osc8HoverPolicy::WellKnownOnly),
            Osc8HoverDecision::ShowStatus,
        );
        assert_eq!(
            osc8_hover_decision(&weird, Osc8HoverPolicy::WellKnownOnly),
            Osc8HoverDecision::Suppress,
        );
    }

    // ----------------------------------------------------------------
    // OSC 22 cursor shape
    // ----------------------------------------------------------------

    #[test]
    fn parse_osc22_block() {
        assert_eq!(parse_osc22_payload("block"), Some(Osc22CursorShape::Block));
        assert_eq!(parse_osc22_payload("BLOCK"), Some(Osc22CursorShape::Block));
        assert_eq!(parse_osc22_payload("  block  "), Some(Osc22CursorShape::Block));
    }

    #[test]
    fn parse_osc22_aliases() {
        assert_eq!(parse_osc22_payload("underscore"), Some(Osc22CursorShape::Underline));
        assert_eq!(parse_osc22_payload("ibeam"), Some(Osc22CursorShape::Bar));
        assert_eq!(parse_osc22_payload("vbar"), Some(Osc22CursorShape::Bar));
    }

    #[test]
    fn parse_osc22_default_and_empty() {
        assert_eq!(parse_osc22_payload("default"), Some(Osc22CursorShape::Default));
        assert_eq!(parse_osc22_payload(""), Some(Osc22CursorShape::Default));
    }

    #[test]
    fn parse_osc22_unknown_returns_none() {
        assert_eq!(parse_osc22_payload("hourglass"), None);
        assert_eq!(parse_osc22_payload("\x1b"), None);
    }

    #[test]
    fn cursor_shape_label_stable() {
        assert_eq!(Osc22CursorShape::Block.label(), "block");
        assert_eq!(Osc22CursorShape::Underline.label(), "underline");
        assert_eq!(Osc22CursorShape::Bar.label(), "bar");
        assert_eq!(Osc22CursorShape::Default.label(), "default");
    }

    // ----------------------------------------------------------------
    // OSC 52 — targets
    // ----------------------------------------------------------------

    #[test]
    fn osc52_target_letter_roundtrip() {
        for target in [
            Osc52Target::Clipboard,
            Osc52Target::Primary,
            Osc52Target::Selection,
            Osc52Target::BufferCut,
        ] {
            let l = target.letter();
            assert_eq!(Osc52Target::from_letter(l), Some(target));
        }
    }

    #[test]
    fn osc52_target_unknown_letter_none() {
        assert_eq!(Osc52Target::from_letter('x'), None);
        assert_eq!(Osc52Target::from_letter('C'), None); // case matters
    }

    #[test]
    fn parse_osc52_targets_single() {
        assert_eq!(parse_osc52_targets("c"), vec![Osc52Target::Clipboard]);
    }

    #[test]
    fn parse_osc52_targets_multiple() {
        // Order preserved: Clipboard < Primary < Selection < BufferCut
        let r = parse_osc52_targets("psbc");
        assert_eq!(r, vec![
            Osc52Target::Clipboard,
            Osc52Target::Primary,
            Osc52Target::Selection,
            Osc52Target::BufferCut,
        ]);
    }

    #[test]
    fn parse_osc52_targets_dedup() {
        let r = parse_osc52_targets("ccc");
        assert_eq!(r, vec![Osc52Target::Clipboard]);
    }

    #[test]
    fn parse_osc52_targets_skips_garbage() {
        let r = parse_osc52_targets("c,p,XYZ");
        assert_eq!(r, vec![Osc52Target::Clipboard, Osc52Target::Primary]);
    }

    // ----------------------------------------------------------------
    // OSC 52 — size cap
    // ----------------------------------------------------------------

    #[test]
    fn osc52_under_cap_approved() {
        let config = Osc52Config::default();
        let d = osc52_size_cap_decision(1024, config);
        assert_eq!(d, Osc52SizeCapDecision::Approved);
    }

    #[test]
    fn osc52_at_cap_approved() {
        let config = Osc52Config::default();
        let d = osc52_size_cap_decision(5 * 1024 * 1024, config);
        assert_eq!(d, Osc52SizeCapDecision::Approved);
    }

    #[test]
    fn osc52_over_cap_rejected() {
        let config = Osc52Config::default();
        let d = osc52_size_cap_decision(6 * 1024 * 1024, config);
        assert_eq!(d, Osc52SizeCapDecision::RejectedOversized);
    }

    // ----------------------------------------------------------------
    // OmnibusOscTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = OmnibusOscTelemetry::default();
        assert_eq!(t.hyperlinks_opened, 0);
        assert_eq!(t.osc52_writes_allowed, 0);
    }

    #[test]
    fn telemetry_record_osc8_open_close() {
        let mut t = OmnibusOscTelemetry::default();
        t.record_osc8_open(true);
        t.record_osc8_open(true);
        t.record_osc8_open(false);
        t.record_osc8_close();
        assert_eq!(t.hyperlinks_opened, 2);
        assert_eq!(t.hyperlink_registry_full_denials, 1);
        assert_eq!(t.hyperlinks_closed, 1);
    }

    #[test]
    fn telemetry_record_osc8_hover() {
        let mut t = OmnibusOscTelemetry::default();
        t.record_osc8_hover(Osc8HoverDecision::ShowStatus);
        t.record_osc8_hover(Osc8HoverDecision::Suppress);
        assert_eq!(t.hyperlink_hovers_shown, 1);
        assert_eq!(t.hyperlink_hovers_suppressed, 1);
    }

    #[test]
    fn telemetry_record_osc22() {
        let mut t = OmnibusOscTelemetry::default();
        t.record_osc22(true);
        t.record_osc22(false);
        assert_eq!(t.cursor_shape_changes, 1);
        assert_eq!(t.cursor_shape_parse_errors, 1);
    }

    #[test]
    fn telemetry_record_osc52_audit_routes() {
        let mut t = OmnibusOscTelemetry::default();
        let allowed_write = Osc52AuditEvent {
            direction: Osc52Direction::Write,
            targets: vec![Osc52Target::Clipboard],
            decoded_bytes: 100,
            decision: Osc52AuditDecision::Allowed,
            source_pane: 1,
            timestamp_ms: 1_000_000,
        };
        let denied_write = Osc52AuditEvent {
            direction: Osc52Direction::Write,
            targets: vec![Osc52Target::Clipboard],
            decoded_bytes: 100_000_000,
            decision: Osc52AuditDecision::Denied {
                reason: Osc52DenyReason::Oversized,
            },
            source_pane: 1,
            timestamp_ms: 1_000_001,
        };
        let denied_read = Osc52AuditEvent {
            direction: Osc52Direction::Read,
            targets: vec![Osc52Target::Clipboard],
            decoded_bytes: 0,
            decision: Osc52AuditDecision::Denied {
                reason: Osc52DenyReason::ReadDefaultDeny,
            },
            source_pane: 1,
            timestamp_ms: 1_000_002,
        };
        t.record_osc52_audit(&allowed_write);
        t.record_osc52_audit(&denied_write);
        t.record_osc52_audit(&denied_read);
        assert_eq!(t.osc52_writes_allowed, 1);
        assert_eq!(t.osc52_writes_denied, 1);
        assert_eq!(t.osc52_oversized_rejections, 1);
        assert_eq!(t.osc52_reads_attempted, 1);
        assert_eq!(t.osc52_reads_denied, 1);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_lazygit_clickable_hyperlink() {
        // lazygit emits OSC 8 with an https URI.
        let mut registry = HyperlinkRegistry::new(DEFAULT_HYPERLINK_REGISTRY_CAP);
        let mut telem = OmnibusOscTelemetry::default();
        let uri = HyperlinkUri::new("https://github.com/owner/repo/pull/123".to_string());
        match registry.allocate(uri.clone()) {
            HyperlinkAllocOutcome::Allocated(_id) => telem.record_osc8_open(true),
            _ => panic!("expected alloc"),
        }
        let hover = osc8_hover_decision(&uri, Osc8HoverPolicy::WellKnownOnly);
        telem.record_osc8_hover(hover);
        assert_eq!(telem.hyperlinks_opened, 1);
        assert_eq!(telem.hyperlink_hovers_shown, 1);
    }

    #[test]
    fn scenario_vim_cursor_shape_per_mode() {
        // vim sets bar in insert mode, block in normal.
        let mut telem = OmnibusOscTelemetry::default();
        let insert = parse_osc22_payload("bar").unwrap();
        let normal = parse_osc22_payload("block").unwrap();
        assert_eq!(insert, Osc22CursorShape::Bar);
        assert_eq!(normal, Osc22CursorShape::Block);
        telem.record_osc22(true);
        telem.record_osc22(true);
        assert_eq!(telem.cursor_shape_changes, 2);
    }

    #[test]
    fn scenario_ssh_remote_clipboard_write_with_cap() {
        // SSH'd remote app writes a 1 KiB clipboard payload.
        // Default Prompt policy defers to UX; substrate
        // approves cap.
        let config = Osc52Config::default();
        assert_eq!(
            osc52_size_cap_decision(1024, config),
            Osc52SizeCapDecision::Approved,
        );
    }

    #[test]
    fn scenario_malicious_oversized_clipboard_rejected() {
        // Bead's threat model: malicious app ships a 100 MiB
        // OSC 52 payload to wedge the terminal. Substrate caps.
        let config = Osc52Config::default();
        assert_eq!(
            osc52_size_cap_decision(100 * 1024 * 1024, config),
            Osc52SizeCapDecision::RejectedOversized,
        );
    }

    #[test]
    fn scenario_remote_clipboard_read_default_deny() {
        // Bead's threat model: malicious app on remote host
        // tries to read local clipboard. Substrate's audit
        // event records Denied with ReadDefaultDeny.
        let event = Osc52AuditEvent {
            direction: Osc52Direction::Read,
            targets: vec![Osc52Target::Clipboard],
            decoded_bytes: 0,
            decision: Osc52AuditDecision::Denied {
                reason: Osc52DenyReason::ReadDefaultDeny,
            },
            source_pane: 1,
            timestamp_ms: 1_000_000,
        };
        let mut telem = OmnibusOscTelemetry::default();
        telem.record_osc52_audit(&event);
        assert_eq!(telem.osc52_reads_denied, 1);
        assert_eq!(telem.osc52_reads_allowed, 0);
    }

    #[test]
    fn scenario_javascript_uri_suppressed_by_default() {
        // Bead implies hostile schemes shouldn't show hover
        // hint by default. Substrate's WellKnownOnly policy
        // suppresses.
        let uri = HyperlinkUri::new("javascript:steal_session()".to_string());
        let d = osc8_hover_decision(&uri, Osc8HoverPolicy::WellKnownOnly);
        assert_eq!(d, Osc8HoverDecision::Suppress);
    }

    #[test]
    fn scenario_registry_cap_burns_dont_grow_unbounded() {
        // Hostile stream emits 10_000 OSC 8 opens; substrate
        // caps at the configured limit.
        let mut r = HyperlinkRegistry::new(100);
        let mut allocated = 0;
        let mut denied = 0;
        for i in 0..10_000 {
            match r.allocate(HyperlinkUri::new(format!("uri{i}"))) {
                HyperlinkAllocOutcome::Allocated(_) => allocated += 1,
                HyperlinkAllocOutcome::DeniedFull => denied += 1,
            }
        }
        assert_eq!(allocated, 100);
        assert_eq!(denied, 9_900);
    }
}
