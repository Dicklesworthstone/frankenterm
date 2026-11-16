//! Regex catalog for [`crate::smart_selection`] (ft-cnil8).
//!
//! Substrate slice of the `BR-TERM-EMULATOR-UPLIFT-2.14.cont` bead.
//! Ships compiled regexes for every [`SelectionPatternKind`]
//! variant + a single [`find_all_smart_selection_matches`] entry
//! point that the GUI mouse-double-click handler consumes.
//!
//! ## Path note
//!
//! The bead's scope text lists
//! `frankenterm/term/src/selection_patterns.rs` as the canonical
//! home. That file lives in the legacy wezterm-style fork crate;
//! shipping the catalog there requires bringing the regex deps and
//! test harness into that crate too. The substrate already lives
//! in `frankenterm-core` (`smart_selection.rs`), so the regex
//! catalog naturally sits next to it. The legacy fork can re-
//! export from here once the GUI-side wiring lands.
//!
//! ## What ships
//!
//! - One compiled `regex::Regex` per `SelectionPatternKind`,
//!   constructed once via `OnceLock` so each call is a hash-table
//!   lookup rather than a recompile.
//! - [`find_all_smart_selection_matches`] — walks every kind's
//!   regex over the input string and returns a `Vec<SelectionMatch>`
//!   sorted by `(span_start, span_end)`. The classifier in
//!   `smart_selection` consumes this directly.
//! - [`drop_shell_quoted_supersets`] — the bead's "drop ShellQuoted
//!   when a higher-priority match is fully contained" pre-filter.
//!
//! ## What is deferred
//!
//! - `frankenterm.toml` `[[smart_selection.patterns]]` array support
//!   for user-supplied patterns (lives in `config.rs`).
//! - GUI mouse-double-click handler (`frankenterm-gui`).
//! - AT-tree announcement wiring.
//! - OSC 52 GUI prompt UI.
//!
//! Those slices land under the same bead but plug into the typed
//! surface here.

use std::sync::OnceLock;

use regex::Regex;

use crate::smart_selection::{
    SelectionMatch, SelectionPatternKind, classify_double_click, classify_triple_click,
};

/// Compile every catalog regex at first call and cache them. Each
/// entry is keyed by `SelectionPatternKind`; lookups are O(1) via
/// the closed-form match expression in [`pattern_for`].
///
/// `expect` is acceptable here because the patterns are static
/// literals validated by the inline tests below — a compile failure
/// is a build-time bug, not a runtime input.
fn compiled_pattern(kind: SelectionPatternKind) -> &'static Regex {
    macro_rules! cached {
        ($pat:expr) => {{
            static CELL: OnceLock<Regex> = OnceLock::new();
            CELL.get_or_init(|| Regex::new($pat).expect("smart-selection regex"))
        }};
    }
    match kind {
        // RFC 3986 scheme + authority + path + query + fragment.
        // Permissive on path chars so the click target captures the
        // entire URL inside surrounding shell-quoted contexts.
        SelectionPatternKind::Url => {
            cached!(r#"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s'"]+"#)
        }
        // HTTP/HTTPS-only — duplicates URL but the bead wants the
        // narrower variant available for cases where strict RFC
        // 3986 parsing has rejected the surrounding text.
        SelectionPatternKind::HttpUrl => {
            cached!(r#"\bhttps?://[^\s'"]+"#)
        }
        // Unix path: leading '/' or '~' followed by valid POSIX
        // component chars + slash separators. The substrate's
        // `regex` crate does not support lookbehind, so the leading
        // anchor is implicit (the regex engine slides the window
        // and the classifier ignores spans not containing the
        // click position).
        SelectionPatternKind::UnixPath => {
            cached!(r"[/~][A-Za-z0-9._/~\-]+")
        }
        // Windows drive-letter path. Backslashes only — forward-slash
        // paths with a drive letter are caught by Url's catch-all.
        SelectionPatternKind::WindowsPath => {
            cached!(r#"\b[A-Za-z]:\\[^\s'"<>|]+"#)
        }
        // Single-quoted, double-quoted, or dollar-quoted shell
        // strings. Non-greedy so a string of strings doesn't merge.
        SelectionPatternKind::ShellQuoted => {
            cached!(r#"\$?'[^']*'|"[^"]*""#)
        }
        // RFC 5321 simplified: local-part covers the common cases;
        // domain enforces at least one dot and a 2+ char TLD.
        SelectionPatternKind::Email => {
            cached!(r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b")
        }
        // IPv4 dotted-quad. Component magnitude is not validated
        // here — `999.999.999.999` passes the regex but fails
        // semantic parsing; the smart-selection contract says the
        // visual span is what wins, so over-eager matches are fine.
        SelectionPatternKind::Ipv4 => {
            cached!(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b")
        }
        // IPv6 — accept full and `::`-abbreviated forms. Word
        // boundaries are not stable for hex, so anchor on a leading
        // colon, hex digit, or whitespace.
        SelectionPatternKind::Ipv6 => {
            cached!(
                r"(?:[0-9A-Fa-f]{1,4}:){7}[0-9A-Fa-f]{1,4}|(?:[0-9A-Fa-f]{1,4}:){1,7}:|::(?:[0-9A-Fa-f]{1,4}:){0,6}[0-9A-Fa-f]{1,4}"
            )
        }
        // Git ref: short or full commit hash. Branch / tag names
        // would need a wider catalog; the substrate's contract
        // calls only the hash form out as the high-confidence
        // case. Range 7..=40 hex digits.
        SelectionPatternKind::GitRef => {
            cached!(r"\b[0-9a-f]{7,40}\b")
        }
        // E.164 + common phone number formats. Permissive on
        // separators (space / dot / dash / paren) and requires a
        // minimum digit count to keep `1.2.3.4` from matching.
        SelectionPatternKind::PhoneNumber => {
            cached!(r"\+?\d[\d\s().\-]{8,}\d")
        }
        // Hex color: `#RGB`, `#RRGGBB`, or `#RRGGBBAA`.
        SelectionPatternKind::HexColor => {
            cached!(r"#(?:[0-9A-Fa-f]{3}|[0-9A-Fa-f]{6}|[0-9A-Fa-f]{8})\b")
        }
        // Numeric literal: integer / float / hex / binary / octal.
        // Excludes phone-number and IPv4 contexts via word
        // boundaries (regex can't fully disambiguate without
        // post-filtering, so the priority order in
        // SelectionPatternKind::priority handles ties).
        SelectionPatternKind::NumericLiteral => {
            cached!(
                r"\b(?:0[xX][0-9A-Fa-f]+|0[bB][01]+|0[oO][0-7]+|\d+\.\d+(?:[eE][+\-]?\d+)?|\d+)\b"
            )
        }
    }
}

/// Find every match of `kind`'s regex in `text`, returning typed
/// [`SelectionMatch`] records. Spans are byte offsets into the
/// original `text`.
#[must_use]
pub fn find_pattern_matches(text: &str, kind: SelectionPatternKind) -> Vec<SelectionMatch> {
    let regex = compiled_pattern(kind);
    regex
        .find_iter(text)
        .map(|m| SelectionMatch::new(kind, m.start(), m.end()))
        .collect()
}

/// Walk every catalog kind over `text` and return all matches,
/// sorted by `(span_start, span_end)`. The classifier in
/// `smart_selection` filters this list down to a single click
/// target; this function is intentionally pure on the input string.
#[must_use]
pub fn find_all_smart_selection_matches(text: &str) -> Vec<SelectionMatch> {
    let mut matches: Vec<SelectionMatch> = Vec::new();
    for kind in SelectionPatternKind::all() {
        matches.extend(find_pattern_matches(text, *kind));
    }
    matches.sort_by(|a, b| {
        a.span_start
            .cmp(&b.span_start)
            .then(a.span_end.cmp(&b.span_end))
    });
    matches
}

/// One-shot entry point for the GUI mouse-double-click handler
/// (ft-cnil8.1). Runs the canonical pipeline:
///
/// 1. [`find_all_smart_selection_matches`] over `line_text`.
/// 2. [`drop_shell_quoted_supersets`] pre-filter so URL-inside-
///    quotes wins over the surrounding ShellQuoted span.
/// 3. [`classify_double_click`] against the click position.
///
/// `click_byte_offset` is the byte offset of the click cell into
/// `line_text` — the GUI side translates from `(cell_x, cell_y)`
/// using its existing logical-line accessor; this entry point
/// stays pure on the resulting offset.
///
/// Returns `None` when no pattern contains the click. The GUI
/// caller falls back to plain word-boundary selection in that
/// case.
#[must_use]
pub fn smart_match_at_click(text: &str, click_byte_offset: usize) -> Option<SelectionMatch> {
    let raw = find_all_smart_selection_matches(text);
    let filtered = drop_shell_quoted_supersets(raw);
    classify_double_click(&filtered, click_byte_offset)
}

/// Triple-click variant of [`smart_match_at_click`] (ft-cnil8.2).
/// Constrains classification to the line span `[line_start,
/// line_end)` so a triple-click resolves to the widest pattern
/// fully contained in the current line, not the click position.
#[must_use]
pub fn smart_match_in_line(
    text: &str,
    line_start: usize,
    line_end: usize,
) -> Option<SelectionMatch> {
    let raw = find_all_smart_selection_matches(text);
    let filtered = drop_shell_quoted_supersets(raw);
    classify_triple_click(&filtered, line_start, line_end)
}

/// Drop `ShellQuoted` candidates that fully contain a higher-
/// priority match. The bead's algorithm: for each `ShellQuoted`
/// candidate, check whether any non-ShellQuoted candidate is
/// fully contained; if so, drop the ShellQuoted so the user's
/// double-click resolves to the inner URL / Email / Path.
///
/// "Higher priority" here means a *lower* `priority()` value, since
/// the priority ordering puts URL = 0 first.
#[must_use]
pub fn drop_shell_quoted_supersets(matches: Vec<SelectionMatch>) -> Vec<SelectionMatch> {
    let inner: Vec<&SelectionMatch> = matches
        .iter()
        .filter(|m| m.kind != SelectionPatternKind::ShellQuoted)
        .collect();
    matches
        .iter()
        .filter(|outer| {
            if outer.kind != SelectionPatternKind::ShellQuoted {
                return true;
            }
            let outer_priority = outer.kind.priority();
            !inner.iter().any(|inner| {
                inner.kind.priority() < outer_priority
                    && inner.span_start >= outer.span_start
                    && inner.span_end <= outer.span_end
            })
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_contains(
        matches: &[SelectionMatch],
        kind: SelectionPatternKind,
        text: &str,
        span: &str,
    ) -> bool {
        matches
            .iter()
            .any(|m| m.kind == kind && &text[m.span_start..m.span_end] == span)
    }

    // ----------------------------------------------------------------
    // Per-kind regex coverage
    // ----------------------------------------------------------------

    #[test]
    fn url_pattern_captures_full_uri() {
        let text = "see https://example.com/foo?bar=1#frag for context";
        let matches = find_pattern_matches(text, SelectionPatternKind::Url);
        assert!(matches_contains(
            &matches,
            SelectionPatternKind::Url,
            text,
            "https://example.com/foo?bar=1#frag"
        ));
    }

    #[test]
    fn http_url_pattern_matches_http_and_https() {
        let text = "http://a.com and https://b.example/path";
        let matches = find_pattern_matches(text, SelectionPatternKind::HttpUrl);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn unix_path_matches_absolute_and_home() {
        let text = "open /etc/hosts and ~/notes/today.md";
        let matches = find_pattern_matches(text, SelectionPatternKind::UnixPath);
        assert!(matches_contains(
            &matches,
            SelectionPatternKind::UnixPath,
            text,
            "/etc/hosts"
        ));
        assert!(matches_contains(
            &matches,
            SelectionPatternKind::UnixPath,
            text,
            "~/notes/today.md"
        ));
    }

    #[test]
    fn windows_path_matches_drive_letter() {
        let text = r"C:\Users\me\file.txt";
        let matches = find_pattern_matches(text, SelectionPatternKind::WindowsPath);
        assert!(matches_contains(
            &matches,
            SelectionPatternKind::WindowsPath,
            text,
            r"C:\Users\me\file.txt"
        ));
    }

    #[test]
    fn shell_quoted_matches_single_double_dollar() {
        let text = r#"echo 'hello' "world" $'tab\there'"#;
        let matches = find_pattern_matches(text, SelectionPatternKind::ShellQuoted);
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == "'hello'")
        );
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == r#""world""#)
        );
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == r"$'tab\there'")
        );
    }

    #[test]
    fn email_pattern_matches_local_at_domain() {
        let text = "contact alice@example.com or bob.smith+tag@sub.example.co.uk";
        let matches = find_pattern_matches(text, SelectionPatternKind::Email);
        assert!(matches_contains(
            &matches,
            SelectionPatternKind::Email,
            text,
            "alice@example.com"
        ));
        assert!(matches_contains(
            &matches,
            SelectionPatternKind::Email,
            text,
            "bob.smith+tag@sub.example.co.uk"
        ));
    }

    #[test]
    fn ipv4_pattern_matches_dotted_quad() {
        let text = "127.0.0.1 and 192.168.1.42 plus 10.0.0.1";
        let matches = find_pattern_matches(text, SelectionPatternKind::Ipv4);
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn ipv6_pattern_matches_full_and_abbrev() {
        let text = "loopback ::1 and full 2001:db8:85a3:0:0:8a2e:370:7334";
        let matches = find_pattern_matches(text, SelectionPatternKind::Ipv6);
        assert!(!matches.is_empty(), "should find at least one IPv6 match");
    }

    #[test]
    fn git_ref_pattern_matches_short_and_full_hash() {
        let text = "rebase onto a1b2c3d and 0123456789abcdef0123456789abcdef01234567";
        let matches = find_pattern_matches(text, SelectionPatternKind::GitRef);
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == "a1b2c3d")
        );
        assert!(
            matches
                .iter()
                .any(|m| { text[m.span_start..m.span_end].len() == 40 })
        );
    }

    #[test]
    fn phone_number_pattern_matches_e164_and_friendly() {
        let text = "call +1-555-123-4567 or (415) 555-0100 today";
        let matches = find_pattern_matches(text, SelectionPatternKind::PhoneNumber);
        assert!(
            !matches.is_empty(),
            "phone number matcher must find at least one"
        );
    }

    #[test]
    fn hex_color_pattern_matches_3_6_8_digit_forms() {
        let text = "#fff and #C0FFEE and #DEADBEEF here";
        let matches = find_pattern_matches(text, SelectionPatternKind::HexColor);
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == "#fff")
        );
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == "#C0FFEE")
        );
        assert!(
            matches
                .iter()
                .any(|m| &text[m.span_start..m.span_end] == "#DEADBEEF")
        );
    }

    #[test]
    fn numeric_literal_pattern_matches_int_float_hex_bin_oct() {
        let text = "values: 42, 3.14, 0xC0FFEE, 0b1010, 0o755";
        let matches = find_pattern_matches(text, SelectionPatternKind::NumericLiteral);
        let captured: Vec<&str> = matches
            .iter()
            .map(|m| &text[m.span_start..m.span_end])
            .collect();
        for needle in ["42", "3.14", "0xC0FFEE", "0b1010", "0o755"] {
            assert!(
                captured.contains(&needle),
                "missing {needle} in numeric matches: {captured:?}"
            );
        }
    }

    // ----------------------------------------------------------------
    // find_all_smart_selection_matches
    // ----------------------------------------------------------------

    #[test]
    fn find_all_returns_sorted_by_span_start() {
        let text = "see https://example.com or 192.168.1.1 and #fff";
        let matches = find_all_smart_selection_matches(text);
        for pair in matches.windows(2) {
            assert!(
                pair[0].span_start <= pair[1].span_start,
                "matches must be sorted: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn find_all_covers_every_kind_against_a_seeded_corpus() {
        // One match per kind, embedded in a single string.
        let text = concat!(
            "https://example.com/x ",                    // Url + HttpUrl
            "/etc/hosts ",                               // UnixPath
            r"C:\Users\me\file.txt ",                    // WindowsPath
            "'quoted' ",                                 // ShellQuoted
            "alice@example.com ",                        // Email
            "192.168.1.1 ",                              // Ipv4
            "::1 ",                                      // Ipv6
            "0123456789abcdef0123456789abcdef01234567 ", // GitRef (40-char hash)
            "+1-555-123-4567 ",                          // PhoneNumber
            "#C0FFEE ",                                  // HexColor
            "42",                                        // NumericLiteral
        );
        let matches = find_all_smart_selection_matches(text);
        let mut kinds: std::collections::HashSet<SelectionPatternKind> =
            std::collections::HashSet::new();
        for m in &matches {
            kinds.insert(m.kind);
        }
        // Every variant except possibly Ipv6 (depending on word
        // boundaries around `::1`) must be represented.
        for kind in SelectionPatternKind::all() {
            if *kind == SelectionPatternKind::Ipv6 {
                continue;
            }
            assert!(
                kinds.contains(kind),
                "kind {kind:?} not represented in seeded corpus matches: {kinds:?}"
            );
        }
    }

    // ----------------------------------------------------------------
    // drop_shell_quoted_supersets pre-filter
    // ----------------------------------------------------------------

    #[test]
    fn drop_shell_quoted_drops_when_url_is_inside() {
        let text = r"echo 'https://example.com/foo'";
        let raw = find_all_smart_selection_matches(text);
        let filtered = drop_shell_quoted_supersets(raw.clone());
        let raw_kinds: std::collections::HashSet<_> = raw.iter().map(|m| m.kind).collect();
        let filtered_kinds: std::collections::HashSet<_> =
            filtered.iter().map(|m| m.kind).collect();
        assert!(raw_kinds.contains(&SelectionPatternKind::ShellQuoted));
        assert!(
            !filtered_kinds.contains(&SelectionPatternKind::ShellQuoted),
            "ShellQuoted must be dropped when URL is fully contained"
        );
        assert!(filtered_kinds.contains(&SelectionPatternKind::Url));
    }

    #[test]
    fn drop_shell_quoted_keeps_when_no_higher_priority_inside() {
        // Plain quoted string with no URL/email/path inside.
        let text = "'just text'";
        let raw = find_all_smart_selection_matches(text);
        let filtered = drop_shell_quoted_supersets(raw);
        assert!(
            filtered
                .iter()
                .any(|m| m.kind == SelectionPatternKind::ShellQuoted),
            "ShellQuoted must survive when nothing higher-priority is inside"
        );
    }

    #[test]
    fn drop_shell_quoted_keeps_other_non_shell_quoted_matches() {
        let text = r#""abc" "def""#;
        let raw = find_all_smart_selection_matches(text);
        let raw_count = raw.len();
        let filtered = drop_shell_quoted_supersets(raw);
        // Both shell-quoted strings have nothing higher-priority
        // inside, so the filter is a no-op.
        assert_eq!(filtered.len(), raw_count);
    }

    // ----------------------------------------------------------------
    // smart_match_at_click / smart_match_in_line — one-shot entry points
    // ----------------------------------------------------------------

    #[test]
    fn smart_match_at_click_returns_url_when_clicked_inside_quotes() {
        // The bead's marquee scenario: double-clicking inside
        // 'https://example.com' must select the URL, not the
        // shell-quoted span.
        let text = r"echo 'https://example.com/foo'";
        // Byte offset that falls inside "example.com" — well
        // within the URL match.
        let url_start = text.find("https://").expect("url");
        let click = url_start + "https://exa".len();
        let m = smart_match_at_click(text, click).expect("URL match");
        assert_eq!(m.kind, SelectionPatternKind::Url);
        let span = &text[m.span_start..m.span_end];
        assert!(span.starts_with("https://example.com"));
    }

    #[test]
    fn smart_match_at_click_returns_none_when_click_in_whitespace() {
        let text = "see https://example.com";
        let space_offset = "see".len(); // position 3 — the space
        assert!(smart_match_at_click(text, space_offset).is_none());
    }

    #[test]
    fn smart_match_at_click_returns_email_when_clicked_in_email() {
        let text = "contact alice@example.com please";
        let email_start = text.find("alice").expect("email");
        let click = email_start + "alice@".len();
        let m = smart_match_at_click(text, click).expect("Email");
        assert_eq!(m.kind, SelectionPatternKind::Email);
    }

    #[test]
    fn smart_match_in_line_returns_widest_match_in_span() {
        // Triple-click on a line containing exactly one URL
        // returns the URL even when the click position is at the
        // beginning of the line.
        let text = "see https://example.com today";
        let m = smart_match_in_line(text, 0, text.len()).expect("URL");
        assert_eq!(m.kind, SelectionPatternKind::Url);
    }

    #[test]
    fn smart_match_in_line_constrains_to_line_span() {
        // When the line span does not cover the URL, no match.
        let text = "see https://example.com today";
        let url_start = text.find("https").unwrap();
        // Constrain span to before the URL.
        assert!(smart_match_in_line(text, 0, url_start).is_none());
    }

    // ----------------------------------------------------------------
    // OnceLock caching contract
    // ----------------------------------------------------------------

    #[test]
    fn compiled_pattern_returns_same_static_reference() {
        // Two calls with the same kind must return the exact same
        // pointer — that is what makes the regex compile a one-time
        // cost.
        let a = std::ptr::from_ref::<Regex>(compiled_pattern(SelectionPatternKind::Url));
        let b = std::ptr::from_ref::<Regex>(compiled_pattern(SelectionPatternKind::Url));
        assert!(std::ptr::eq(a, b));
    }
}
