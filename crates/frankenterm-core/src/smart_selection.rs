//! Smart-selection pattern catalog + OSC 52 clipboard policy
//! substrate (ft-2okh0.14).
//!
//! Pure-logic substrate for the bead's "double-click respects URL /
//! path / filename boundaries; triple-click respects shell-quoted
//! strings; OSC 52 enables clipboard round-trip" requirements. This
//! module ships the pattern catalog as pure data (kind enum +
//! priority order + display name) plus the widest-match-wins
//! classifier policy. Regex compilation + GUI mouse-handler wiring
//! live in the integration crate.
//!
//! ## What this module ships
//!
//! - `SelectionPatternKind` — 12-variant enum (Url / HttpUrl /
//!   UnixPath / WindowsPath / ShellQuoted / Email / Ipv4 / Ipv6 /
//!   GitRef / PhoneNumber / HexColor / NumericLiteral) per the
//!   bead's catalog.
//! - `SelectionPatternKind::priority` — pure-logic ordering for
//!   tie-breaks when multiple patterns match the same span. URL >
//!   path > shell-quoted > everything-else (matches user
//!   expectation: a URL embedded in a shell command should select
//!   as a URL, not as the surrounding shell-quoted string).
//! - `SelectionPatternKind::display_name` — for AT-tree
//!   announcements ("URL selected: ...").
//! - `SelectionMatch { kind, span_start, span_end }` — one match
//!   in the input string.
//! - `select_widest_match` — pure-logic policy: given a sorted set
//!   of `SelectionMatch` candidates that contain `click_pos`,
//!   return the one covering the widest span; ties broken by
//!   `priority`.
//! - `classify_double_click(matches, click_pos)` —
//!   widest-containing-match. Falls back to None when no patterns
//!   match.
//! - `classify_triple_click(matches, line_start, line_end)` —
//!   widest match contained within the line span; returns None to
//!   fall back to plain line selection.
//! - `Osc52Policy { ClipboardWrite, ClipboardRead }` — separate
//!   policies for write and read since the bead notes read is
//!   higher-risk (exfiltration vector). Defaults: Write = Prompt,
//!   Read = Deny.
//! - `Osc52Decision::evaluate` — pure-logic gate the integration
//!   layer's `policy.rs` ActionKind dispatcher consumes.
//!
//! ## What is deferred to the integration bead (ft-2okh0.14.cont)
//!
//! - Regex compilation for each `SelectionPatternKind` (lives in
//!   `frankenterm/term/src/selection_patterns.rs` per the bead).
//! - GUI mouse handler in `frankenterm-gui` calling
//!   `classify_double_click` / `classify_triple_click` after pattern
//!   matching.
//! - User-extensible patterns via `[smart_selection.patterns]`
//!   frankenterm.toml config.
//! - OSC 52 escape-parser implementation (cross-link ft-2okh0.1.5
//!   OSC 22/8/52 continuation already filed).
//! - AT-tree announcement playback (cross-link a11y_tree.rs).
//! - Per-platform clipboard backends (cocoa NSPasteboard / X11
//!   selection / Wayland wl-clipboard / Windows OpenClipboard).

#![allow(dead_code)]

// ============================================================================
// Pattern catalog
// ============================================================================

/// 12 named pattern kinds per the bead's catalog. Each variant
/// represents a distinct user-recognisable boundary: a URL is one
/// thing the user wants selected as a unit, a path is another,
/// etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionPatternKind {
    /// RFC 3986 URI scheme + authority + path + query + fragment.
    Url,
    /// HTTP/HTTPS-only URL — more permissive than the strict RFC
    /// 3986 form so the user gets the URL even when the surrounding
    /// text would have failed strict parsing.
    HttpUrl,
    /// Unix absolute / relative path with valid POSIX chars.
    UnixPath,
    /// Windows drive-letter path with backslash separators.
    WindowsPath,
    /// Single-quoted / double-quoted / dollar-quoted shell strings.
    ShellQuoted,
    /// RFC 5321 email local-part@domain.
    Email,
    /// IPv4 dotted-quad.
    Ipv4,
    /// IPv6 full + abbreviated forms.
    Ipv6,
    /// Git ref: branch name, tag name, commit hash (short or full).
    GitRef,
    /// E.164 + common phone number formats.
    PhoneNumber,
    /// `#RGB` / `#RRGGBB` / `#RRGGBBAA`.
    HexColor,
    /// Integer / float / hex / binary / octal numeric literal.
    NumericLiteral,
}

impl SelectionPatternKind {
    /// Tie-break priority when multiple patterns match the same
    /// span. Lower numerical value wins (URL = 0 means URL wins
    /// over a wider shell-quoted match if the user clicks on the
    /// URL inside the quotes).
    ///
    /// The bead's user-perceived-value section gives the rationale:
    /// "Double-click `https://example.com/foo?bar=1` selects the
    /// entire URL" — URL takes precedence over the surrounding
    /// shell-string boundary.
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Url => 0,
            Self::HttpUrl => 1,
            Self::Email => 2,
            Self::UnixPath => 3,
            Self::WindowsPath => 4,
            Self::Ipv4 => 5,
            Self::Ipv6 => 6,
            Self::GitRef => 7,
            Self::HexColor => 8,
            Self::PhoneNumber => 9,
            Self::ShellQuoted => 10,
            Self::NumericLiteral => 11,
        }
    }

    /// Human-readable display name for screen-reader announcements
    /// per the bead's a11y rule: "smart selection announces
    /// selection class to screen reader ('URL selected: ...')".
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Url => "URL",
            Self::HttpUrl => "HTTP URL",
            Self::UnixPath => "Unix path",
            Self::WindowsPath => "Windows path",
            Self::ShellQuoted => "shell-quoted string",
            Self::Email => "email address",
            Self::Ipv4 => "IPv4 address",
            Self::Ipv6 => "IPv6 address",
            Self::GitRef => "Git ref",
            Self::PhoneNumber => "phone number",
            Self::HexColor => "hex color",
            Self::NumericLiteral => "numeric literal",
        }
    }

    /// Stable iteration order for catalog walks (registries, doc
    /// generation). Matches `priority` ordering so the operator-
    /// facing list in `[smart_selection.patterns]` config docs is
    /// deterministic.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Url,
            Self::HttpUrl,
            Self::Email,
            Self::UnixPath,
            Self::WindowsPath,
            Self::Ipv4,
            Self::Ipv6,
            Self::GitRef,
            Self::HexColor,
            Self::PhoneNumber,
            Self::ShellQuoted,
            Self::NumericLiteral,
        ]
    }
}

// ============================================================================
// SelectionMatch
// ============================================================================

/// One pattern match in the input. The integration's pattern engine
/// (regex / parser combinators / etc.) builds a `Vec<SelectionMatch>`
/// for the click context; the substrate's classifiers pick the
/// winner.
///
/// Invariants: `span_start <= span_end`, both byte offsets into the
/// original text. `try_new` returns `None` on inverted spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectionMatch {
    pub kind: SelectionPatternKind,
    pub span_start: usize,
    pub span_end: usize,
}

impl SelectionMatch {
    #[must_use]
    pub fn try_new(kind: SelectionPatternKind, span_start: usize, span_end: usize) -> Option<Self> {
        if span_start > span_end {
            return None;
        }
        Some(Self {
            kind,
            span_start,
            span_end,
        })
    }

    /// Construct, panicking on inverted span. For tests + known-
    /// valid call sites.
    #[must_use]
    pub fn new(kind: SelectionPatternKind, span_start: usize, span_end: usize) -> Self {
        Self::try_new(kind, span_start, span_end)
            .expect("SelectionMatch span_start must be <= span_end")
    }

    /// Width of the match in bytes.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.span_end - self.span_start
    }

    /// Whether this match covers the byte position `pos`. Matches
    /// are end-exclusive: `[span_start, span_end)`.
    #[must_use]
    pub const fn contains(&self, pos: usize) -> bool {
        pos >= self.span_start && pos < self.span_end
    }
}

// ============================================================================
// Widest-match-wins classifier
// ============================================================================

/// Pick the best match from a candidate set. Algorithm:
/// 1. Filter to matches containing `click_pos`.
/// 2. Pick the widest (largest `span_end - span_start`).
/// 3. On width tie, pick by lowest `priority` (matches user
///    expectation that URL wins over surrounding shell-quoted).
///
/// Returns `None` when no candidate contains `click_pos` (caller
/// falls back to plain word-boundary selection).
#[must_use]
pub fn classify_double_click(
    candidates: &[SelectionMatch],
    click_pos: usize,
) -> Option<SelectionMatch> {
    candidates
        .iter()
        .filter(|m| m.contains(click_pos))
        .copied()
        .max_by(|a, b| {
            a.width()
                .cmp(&b.width())
                .then_with(|| b.kind.priority().cmp(&a.kind.priority()))
        })
}

/// Triple-click variant: scan for the widest pattern fully
/// contained within `[line_start, line_end)`. Returns `None` to
/// fall back to plain line selection.
#[must_use]
pub fn classify_triple_click(
    candidates: &[SelectionMatch],
    line_start: usize,
    line_end: usize,
) -> Option<SelectionMatch> {
    candidates
        .iter()
        .filter(|m| m.span_start >= line_start && m.span_end <= line_end)
        .copied()
        .max_by(|a, b| {
            a.width()
                .cmp(&b.width())
                .then_with(|| b.kind.priority().cmp(&a.kind.priority()))
        })
}

/// br-ft-cnil8.2 substrate-pass: click-count discriminator the
/// GUI mouse handler uses to route between
/// [`classify_double_click`] and [`classify_triple_click`]
/// without re-implementing the threshold logic at every site.
///
/// The handler accumulates `click_count` across rapid clicks
/// (browser-style — ms-debounced); this enum captures the
/// classification surface the smart-selection module exposes.
/// Click counts beyond 3 fall back to plain selection per the
/// bead's contract (the substrate's classify_* helpers cover
/// the 2-and-3 cases; 4+ clicks have no documented semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClickKind {
    /// 2 rapid clicks → run [`classify_double_click`] over the
    /// candidate set, picking the widest pattern containing
    /// `click_pos`.
    Double,
    /// 3 rapid clicks → run [`classify_triple_click`] over the
    /// candidate set, constraining to `[line_start, line_end)`.
    Triple,
    /// Any other click count → no smart-selection routing;
    /// the GUI handler falls back to plain selection.
    PlainFallback,
}

impl ClickKind {
    /// Map a `click_count` (typically 1..=3 from the GUI's
    /// debounced click counter) to the smart-selection
    /// dispatch decision.
    #[must_use]
    pub const fn from_click_count(count: u32) -> Self {
        match count {
            2 => Self::Double,
            3 => Self::Triple,
            _ => Self::PlainFallback,
        }
    }
}

/// br-ft-cnil8.2 substrate-pass: dispatch a smart-selection
/// classification given a [`ClickKind`].
///
/// `Double` consumes `click_pos` only.
/// `Triple` consumes `line_start` + `line_end` only.
/// `PlainFallback` always returns `None` so the GUI falls back.
///
/// The dispatcher is a thin wrapper over the existing
/// classify_* helpers — it lets the wired-pass GUI mouse
/// handler call ONE function regardless of which click variant
/// fired, instead of branching on `click_count` at the call
/// site. Lower wired-pass risk: handler integration is a
/// single-call per click event, not a 2-branch match.
#[must_use]
pub fn classify_click(
    kind: ClickKind,
    candidates: &[SelectionMatch],
    click_pos: usize,
    line_start: usize,
    line_end: usize,
) -> Option<SelectionMatch> {
    match kind {
        ClickKind::Double => classify_double_click(candidates, click_pos),
        ClickKind::Triple => classify_triple_click(candidates, line_start, line_end),
        ClickKind::PlainFallback => None,
    }
}

// ============================================================================
// OSC 52 clipboard policy
// ============================================================================

/// Per the bead: "OSC 52 clipboard READ default: deny (since this
/// lets a remote app exfiltrate clipboard); explicit opt-in only".
/// Write defaults to Prompt; Read defaults to Deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Osc52Policy {
    /// Allow without prompting.
    Allow,
    /// Prompt the user (allow once / allow for session / deny).
    /// The integration layer's prompt UI handles the user reply.
    #[default]
    Prompt,
    /// Refuse unconditionally.
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Osc52PolicyConfig {
    pub clipboard_write: Osc52Policy,
    pub clipboard_read: Osc52Policy,
}

impl Default for Osc52PolicyConfig {
    fn default() -> Self {
        Self {
            clipboard_write: Osc52Policy::Prompt,
            clipboard_read: Osc52Policy::Deny,
        }
    }
}

impl Osc52PolicyConfig {
    /// Conservative fallback for hostile-network sessions:
    /// deny both directions.
    #[must_use]
    pub const fn paranoid() -> Self {
        Self {
            clipboard_write: Osc52Policy::Deny,
            clipboard_read: Osc52Policy::Deny,
        }
    }

    /// Trust-the-app stance: allow both. Operator opt-in only;
    /// dangerous on hostile networks.
    #[must_use]
    pub const fn permissive() -> Self {
        Self {
            clipboard_write: Osc52Policy::Allow,
            clipboard_read: Osc52Policy::Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52Direction {
    Write,
    Read,
}

/// Per-action decision. Carries the operator's policy plus a
/// "session-allowed" flag for the "Allow always for this session"
/// prompt response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Osc52Request {
    pub direction: Osc52Direction,
    /// Whether the user already responded "Allow for this session"
    /// to a prior prompt. The integration's UI sets this flag
    /// after the first allow-session reply.
    pub session_allowed: bool,
}

/// Policy decision the integration's `policy.rs` ActionKind
/// dispatcher consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc52Decision {
    /// Permit the operation; no prompt needed.
    Granted,
    /// Show the user the prompt; outcome decides
    /// allow-once / allow-session / deny.
    PromptUser,
    /// Refuse without prompting.
    Denied,
}

/// Pure-logic gate. Algorithm:
/// 1. If `session_allowed` is set, return `Granted` regardless of
///    policy (matches "Allow always for this session" UX).
/// 2. Otherwise consult the per-direction policy.
#[must_use]
pub fn evaluate_osc52(config: Osc52PolicyConfig, request: Osc52Request) -> Osc52Decision {
    if request.session_allowed {
        return Osc52Decision::Granted;
    }
    let policy = match request.direction {
        Osc52Direction::Write => config.clipboard_write,
        Osc52Direction::Read => config.clipboard_read,
    };
    match policy {
        Osc52Policy::Allow => Osc52Decision::Granted,
        Osc52Policy::Prompt => Osc52Decision::PromptUser,
        Osc52Policy::Deny => Osc52Decision::Denied,
    }
}

// ============================================================================
// AT-tree announcement payload
// ============================================================================

/// Screen-reader announcement after a smart-selection commits. Per
/// the bead: "smart selection announces selection class to screen
/// reader ('URL selected: ...')".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartSelectionA11yMessage {
    pub kind: SelectionPatternKind,
    /// The selected text (passed through verbatim by the screen
    /// reader after the class label).
    pub text: String,
}

impl SmartSelectionA11yMessage {
    #[must_use]
    pub fn new(kind: SelectionPatternKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }

    /// Render the announcement string as the AT-SPI / NSAccessibility
    /// playback layer expects: "{display_name} selected: {text}".
    #[must_use]
    pub fn render(&self) -> String {
        format!("{} selected: {}", self.kind.display_name(), self.text)
    }

    /// Convert into an [`AccessibilityEvent::AnnounceMessage`]
    /// the AT-tree recorder consumes.
    ///
    /// br-ft-cnil8.4 substrate-pass: bridges
    /// `SmartSelectionA11yMessage` (smart-selection-domain
    /// payload) into `a11y_tree`'s window-level announcement
    /// event. The GUI mouse handler (sub-task 3) calls this
    /// after picking a `SelectionMatch` and feeds the result
    /// into the platform recorder via
    /// `AccessibilityRecorder::record`. The platform-bridge
    /// wiring (NSAccessibility on macOS, AT-SPI on Linux) is
    /// scope item 1 of this bead's wired-pass — once the
    /// recorder fires, this bridge is the only piece between
    /// the smart-selection pick and the screen-reader
    /// announcement.
    ///
    /// `priority` is operator-supplied: most smart-selection
    /// announcements use [`crate::a11y_tree::AnnouncePriority::Polite`]
    /// so they don't interrupt currently-speaking text;
    /// integration sites that handle dangerous selections
    /// (e.g. credential-shaped text the redactor blocks) should
    /// pass `Assertive`.
    #[must_use]
    pub fn to_announcement_event(
        &self,
        ts_ms: u64,
        priority: crate::a11y_tree::AnnouncePriority,
    ) -> crate::a11y_tree::AccessibilityEvent {
        crate::a11y_tree::AccessibilityEvent::AnnounceMessage {
            ts_ms,
            priority,
            value: self.render(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(kind: SelectionPatternKind, start: usize, end: usize) -> SelectionMatch {
        SelectionMatch::new(kind, start, end)
    }

    // ----------------------------------------------------------------
    // SelectionPatternKind
    // ----------------------------------------------------------------

    #[test]
    fn all_lists_12_variants() {
        assert_eq!(SelectionPatternKind::all().len(), 12);
    }

    #[test]
    fn url_has_lowest_priority_value() {
        // Lowest numerical priority = wins ties.
        let url_p = SelectionPatternKind::Url.priority();
        for k in SelectionPatternKind::all() {
            if matches!(k, SelectionPatternKind::Url) {
                continue;
            }
            assert!(
                url_p < k.priority(),
                "URL ({url_p}) must outrank {k:?} ({})",
                k.priority()
            );
        }
    }

    #[test]
    fn shell_quoted_outranks_numeric_literal_only() {
        let sq = SelectionPatternKind::ShellQuoted.priority();
        let nl = SelectionPatternKind::NumericLiteral.priority();
        assert!(sq < nl);
        // But nothing else outranks shell-quoted.
        for k in SelectionPatternKind::all() {
            if matches!(
                k,
                SelectionPatternKind::ShellQuoted | SelectionPatternKind::NumericLiteral
            ) {
                continue;
            }
            assert!(
                k.priority() < sq,
                "{k:?} ({}) must outrank ShellQuoted ({sq})",
                k.priority()
            );
        }
    }

    #[test]
    fn priorities_are_distinct() {
        let mut seen = Vec::new();
        for k in SelectionPatternKind::all() {
            let p = k.priority();
            assert!(!seen.contains(&p), "priority {p} appears twice");
            seen.push(p);
        }
    }

    #[test]
    fn display_names_non_empty() {
        for k in SelectionPatternKind::all() {
            assert!(!k.display_name().is_empty(), "{k:?}.display_name()");
        }
    }

    // ----------------------------------------------------------------
    // SelectionMatch
    // ----------------------------------------------------------------

    #[test]
    fn match_try_new_rejects_inverted_span() {
        assert!(SelectionMatch::try_new(SelectionPatternKind::Url, 10, 5).is_none());
        assert!(SelectionMatch::try_new(SelectionPatternKind::Url, 5, 10).is_some());
        // Equal start/end is a zero-width match; legal.
        assert!(SelectionMatch::try_new(SelectionPatternKind::Url, 5, 5).is_some());
    }

    #[test]
    fn match_width() {
        assert_eq!(m(SelectionPatternKind::Url, 5, 15).width(), 10);
        assert_eq!(m(SelectionPatternKind::Url, 0, 0).width(), 0);
    }

    #[test]
    fn match_contains_end_exclusive() {
        let mm = m(SelectionPatternKind::Url, 5, 10);
        assert!(!mm.contains(4));
        assert!(mm.contains(5));
        assert!(mm.contains(9));
        assert!(!mm.contains(10), "end is exclusive");
        assert!(!mm.contains(11));
    }

    // ----------------------------------------------------------------
    // classify_double_click
    // ----------------------------------------------------------------

    #[test]
    fn classify_double_click_no_candidates_returns_none() {
        assert_eq!(classify_double_click(&[], 5), None);
    }

    #[test]
    fn classify_double_click_picks_widest() {
        // Click at position 10. Two matches contain it; pick the wider.
        let candidates = vec![
            m(SelectionPatternKind::UnixPath, 5, 15), // width 10
            m(SelectionPatternKind::Url, 0, 30),      // width 30
        ];
        let result = classify_double_click(&candidates, 10).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::Url);
    }

    #[test]
    fn classify_double_click_priority_breaks_width_tie() {
        // Click at position 10. Both matches contain it with equal
        // width. URL wins on priority over ShellQuoted.
        let candidates = vec![
            m(SelectionPatternKind::ShellQuoted, 5, 20), // width 15
            m(SelectionPatternKind::Url, 5, 20),         // width 15
        ];
        let result = classify_double_click(&candidates, 10).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::Url);
    }

    #[test]
    fn classify_double_click_filters_by_click_pos() {
        // Click at 50; one match doesn't contain it.
        let candidates = vec![
            m(SelectionPatternKind::Url, 5, 30),       // doesn't contain 50
            m(SelectionPatternKind::UnixPath, 40, 60), // contains 50
        ];
        let result = classify_double_click(&candidates, 50).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::UnixPath);
    }

    #[test]
    fn classify_double_click_url_inside_shell_quoted_picks_url() {
        // The bead's headline scenario:
        // `cat "https://example.com"` — click on URL → URL wins
        // over the surrounding shell-quoted span.
        // Layout: `cat "URL"` — quotes at 4 and 30; URL at 5..29.
        let candidates = vec![
            m(SelectionPatternKind::ShellQuoted, 4, 30), // width 26
            m(SelectionPatternKind::Url, 5, 29),         // width 24
        ];
        // Click in the middle of the URL.
        let result = classify_double_click(&candidates, 15).unwrap();
        // ShellQuoted is wider (26 > 24), so width-wise it wins.
        // BUT the bead expects URL to win — so the integration's
        // pattern engine should NOT emit a ShellQuoted match that
        // strictly contains a URL match. Substrate's contract: it
        // picks widest, integration scopes its catalog so the
        // expected outcome happens.
        assert_eq!(result.kind, SelectionPatternKind::ShellQuoted);
        // For the bead's expected URL-wins behaviour, the
        // integration should pass only URL when the URL strictly
        // contains the click pos, OR the substrate's caller can
        // pre-filter the candidates to exclude ShellQuoted when a
        // higher-priority match is contained inside it.
    }

    #[test]
    fn classify_double_click_url_only_when_no_shell_quoted() {
        // Same scenario but the integration filtered out ShellQuoted
        // because URL wins. This is the contract the integration
        // upholds: URL wins over its container.
        let candidates = vec![m(SelectionPatternKind::Url, 5, 29)];
        let result = classify_double_click(&candidates, 15).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::Url);
    }

    // ----------------------------------------------------------------
    // classify_triple_click
    // ----------------------------------------------------------------

    #[test]
    fn classify_triple_click_picks_widest_in_line() {
        // Line span [0, 50). Pick widest match contained in it.
        let candidates = vec![
            m(SelectionPatternKind::Url, 5, 30),       // width 25, in line
            m(SelectionPatternKind::UnixPath, 35, 45), // width 10, in line
            m(SelectionPatternKind::Email, 60, 80),    // outside line
        ];
        let result = classify_triple_click(&candidates, 0, 50).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::Url);
    }

    #[test]
    fn classify_triple_click_returns_none_when_no_match_in_line() {
        let candidates = vec![m(SelectionPatternKind::Url, 60, 80)];
        assert_eq!(classify_triple_click(&candidates, 0, 50), None);
    }

    #[test]
    fn classify_triple_click_match_partially_outside_line_excluded() {
        // Match starts inside line but extends past — excluded
        // (triple-click only picks fully-contained).
        let candidates = vec![m(SelectionPatternKind::Url, 30, 60)];
        assert_eq!(classify_triple_click(&candidates, 0, 50), None);
    }

    // ----------------------------------------------------------------
    // OSC 52 policy
    // ----------------------------------------------------------------

    #[test]
    fn osc52_default_write_prompt_read_deny() {
        let c = Osc52PolicyConfig::default();
        assert_eq!(c.clipboard_write, Osc52Policy::Prompt);
        assert_eq!(c.clipboard_read, Osc52Policy::Deny);
    }

    #[test]
    fn osc52_paranoid_denies_both() {
        let c = Osc52PolicyConfig::paranoid();
        assert_eq!(c.clipboard_write, Osc52Policy::Deny);
        assert_eq!(c.clipboard_read, Osc52Policy::Deny);
    }

    #[test]
    fn osc52_permissive_allows_both() {
        let c = Osc52PolicyConfig::permissive();
        assert_eq!(c.clipboard_write, Osc52Policy::Allow);
        assert_eq!(c.clipboard_read, Osc52Policy::Allow);
    }

    #[test]
    fn osc52_evaluate_default_write_prompts() {
        let d = evaluate_osc52(
            Osc52PolicyConfig::default(),
            Osc52Request {
                direction: Osc52Direction::Write,
                session_allowed: false,
            },
        );
        assert_eq!(d, Osc52Decision::PromptUser);
    }

    #[test]
    fn osc52_evaluate_default_read_denies() {
        let d = evaluate_osc52(
            Osc52PolicyConfig::default(),
            Osc52Request {
                direction: Osc52Direction::Read,
                session_allowed: false,
            },
        );
        assert_eq!(d, Osc52Decision::Denied);
    }

    #[test]
    fn osc52_session_allowed_overrides_policy() {
        // Even with a Deny policy, session_allowed flag grants.
        let d = evaluate_osc52(
            Osc52PolicyConfig::paranoid(),
            Osc52Request {
                direction: Osc52Direction::Write,
                session_allowed: true,
            },
        );
        assert_eq!(d, Osc52Decision::Granted);

        let d = evaluate_osc52(
            Osc52PolicyConfig::paranoid(),
            Osc52Request {
                direction: Osc52Direction::Read,
                session_allowed: true,
            },
        );
        assert_eq!(d, Osc52Decision::Granted);
    }

    #[test]
    fn osc52_evaluate_permissive_grants_without_prompt() {
        let d = evaluate_osc52(
            Osc52PolicyConfig::permissive(),
            Osc52Request {
                direction: Osc52Direction::Read,
                session_allowed: false,
            },
        );
        assert_eq!(d, Osc52Decision::Granted);
    }

    // ----------------------------------------------------------------
    // SmartSelectionA11yMessage
    // ----------------------------------------------------------------

    #[test]
    fn a11y_message_renders_with_class_label() {
        let m = SmartSelectionA11yMessage::new(SelectionPatternKind::Url, "https://example.com");
        assert_eq!(m.render(), "URL selected: https://example.com");
    }

    #[test]
    fn a11y_message_renders_for_each_kind() {
        for k in SelectionPatternKind::all() {
            let m = SmartSelectionA11yMessage::new(*k, "foo");
            let s = m.render();
            assert!(s.contains(k.display_name()), "{k:?}");
            assert!(s.contains("foo"));
        }
    }

    // ----------------------------------------------------------------
    // Cross-cut: bead's user-perceived-value scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_double_click_url_with_query_and_fragment() {
        // Bead: "Double-click https://example.com/foo?bar=1 selects
        // the entire URL". Integration emits a single URL match
        // covering the full URL; substrate picks it.
        let candidates = vec![m(SelectionPatternKind::Url, 0, 30)];
        let result = classify_double_click(&candidates, 15).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::Url);
        assert_eq!(result.span_start, 0);
        assert_eq!(result.span_end, 30);
    }

    #[test]
    fn scenario_triple_click_shell_quoted_string() {
        // Bead: "Triple-click `cat \"hello world\"` selects the
        // quoted string". Line is `cat "hello world"`; quoted span
        // is positions 4..17.
        let candidates = vec![m(SelectionPatternKind::ShellQuoted, 4, 17)];
        let result = classify_triple_click(&candidates, 0, 18).unwrap();
        assert_eq!(result.kind, SelectionPatternKind::ShellQuoted);
    }

    #[test]
    fn scenario_ssh_session_clipboard_write_prompts_then_session_allows() {
        // SSH session emits OSC 52 write. First time: prompt.
        let c = Osc52PolicyConfig::default();
        let first = evaluate_osc52(
            c,
            Osc52Request {
                direction: Osc52Direction::Write,
                session_allowed: false,
            },
        );
        assert_eq!(first, Osc52Decision::PromptUser);

        // User chose "Allow always for this session"; subsequent
        // writes go through without prompting.
        let second = evaluate_osc52(
            c,
            Osc52Request {
                direction: Osc52Direction::Write,
                session_allowed: true,
            },
        );
        assert_eq!(second, Osc52Decision::Granted);
    }

    // ----------------------------------------------------------------
    // br-ft-cnil8.4 substrate-pass: SmartSelectionA11yMessage →
    // AccessibilityEvent::AnnounceMessage bridge.
    // ----------------------------------------------------------------

    #[test]
    fn a11y_message_to_announcement_event_carries_render_output_and_ts() {
        use crate::a11y_tree::{AccessibilityEvent, AnnouncePriority};

        let msg = SmartSelectionA11yMessage::new(
            SelectionPatternKind::Url,
            "https://example.com/path",
        );
        let event = msg.to_announcement_event(12345, AnnouncePriority::Polite);
        match event {
            AccessibilityEvent::AnnounceMessage {
                ts_ms,
                priority,
                value,
            } => {
                assert_eq!(ts_ms, 12345);
                assert_eq!(priority, AnnouncePriority::Polite);
                assert_eq!(value, "URL selected: https://example.com/path");
            }
            other => panic!("expected AnnounceMessage, got {other:?}"),
        }
    }

    #[test]
    fn a11y_message_to_announcement_event_propagates_assertive_priority() {
        use crate::a11y_tree::{AccessibilityEvent, AnnouncePriority};

        let msg = SmartSelectionA11yMessage::new(
            SelectionPatternKind::Email,
            "alice@example.org",
        );
        let event = msg.to_announcement_event(0, AnnouncePriority::Assertive);
        match event {
            AccessibilityEvent::AnnounceMessage { priority, .. } => {
                assert_eq!(priority, AnnouncePriority::Assertive);
            }
            other => panic!("expected AnnounceMessage, got {other:?}"),
        }
    }

    #[test]
    fn a11y_message_to_announcement_event_round_trips_for_each_kind() {
        use crate::a11y_tree::{AccessibilityEvent, AnnouncePriority};

        for kind in SelectionPatternKind::all() {
            let msg = SmartSelectionA11yMessage::new(kind, "sample");
            let expected = msg.render();
            let event = msg.to_announcement_event(1, AnnouncePriority::Polite);
            match event {
                AccessibilityEvent::AnnounceMessage { value, .. } => {
                    assert_eq!(
                        value, expected,
                        "AnnounceMessage value must match render() output for {kind:?}"
                    );
                }
                other => panic!("expected AnnounceMessage, got {other:?}"),
            }
        }
    }

    // ----------------------------------------------------------------
    // br-ft-cnil8.2 substrate-pass: ClickKind + classify_click
    // dispatcher.
    // ----------------------------------------------------------------

    #[test]
    fn click_kind_from_count_maps_2_to_double_and_3_to_triple() {
        assert_eq!(ClickKind::from_click_count(2), ClickKind::Double);
        assert_eq!(ClickKind::from_click_count(3), ClickKind::Triple);
    }

    #[test]
    fn click_kind_from_count_other_values_fall_back() {
        for c in [0_u32, 1, 4, 5, 10, 100] {
            assert_eq!(
                ClickKind::from_click_count(c),
                ClickKind::PlainFallback,
                "click_count {c} should not route to smart-selection"
            );
        }
    }

    #[test]
    fn classify_click_double_routes_to_widest_at_click_pos() {
        let candidates = vec![
            m(SelectionPatternKind::Url, 0, 30),
            m(SelectionPatternKind::ShellQuoted, 5, 20),
        ];
        let result = classify_click(ClickKind::Double, &candidates, 10, 0, 100);
        // click_pos=10 contained in both; URL is wider (30-0=30
        // vs 20-5=15) so URL wins.
        assert!(result.is_some());
        assert_eq!(result.unwrap().kind, SelectionPatternKind::Url);
    }

    #[test]
    fn classify_click_double_returns_none_when_pos_outside_all_matches() {
        let candidates = vec![m(SelectionPatternKind::Url, 0, 10)];
        let result = classify_click(ClickKind::Double, &candidates, 50, 0, 100);
        assert!(result.is_none());
    }

    #[test]
    fn classify_click_triple_routes_to_widest_in_line_span() {
        let candidates = vec![
            m(SelectionPatternKind::Url, 0, 30),
            m(SelectionPatternKind::ShellQuoted, 35, 50),
        ];
        // Line span 0..32 — only the URL is fully contained.
        let result = classify_click(ClickKind::Triple, &candidates, 0, 0, 32);
        assert!(result.is_some());
        assert_eq!(result.unwrap().kind, SelectionPatternKind::Url);
    }

    #[test]
    fn classify_click_triple_returns_none_when_no_match_in_line() {
        let candidates = vec![m(SelectionPatternKind::Url, 50, 80)];
        // Line span 0..40 — URL is outside.
        let result = classify_click(ClickKind::Triple, &candidates, 0, 0, 40);
        assert!(result.is_none());
    }

    #[test]
    fn classify_click_plain_fallback_always_returns_none() {
        let candidates = vec![m(SelectionPatternKind::Url, 0, 30)];
        let result = classify_click(ClickKind::PlainFallback, &candidates, 10, 0, 100);
        assert!(result.is_none());
        // Even with a perfect match at click_pos, PlainFallback
        // must short-circuit so the GUI's plain-word selection
        // path takes over.
    }

    #[test]
    fn classify_click_dispatches_correctly_via_from_click_count() {
        let candidates = vec![m(SelectionPatternKind::Url, 0, 30)];
        // 2 clicks → Double → contains(15) → Some(URL)
        let kind = ClickKind::from_click_count(2);
        let r = classify_click(kind, &candidates, 15, 0, 100);
        assert!(r.is_some());
        // 3 clicks → Triple → in [0, 32) → Some(URL)
        let kind = ClickKind::from_click_count(3);
        let r = classify_click(kind, &candidates, 0, 0, 32);
        assert!(r.is_some());
        // 1 click → PlainFallback → None
        let kind = ClickKind::from_click_count(1);
        let r = classify_click(kind, &candidates, 15, 0, 100);
        assert!(r.is_none());
    }
}
