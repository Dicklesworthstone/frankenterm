use crate::color::ColorPalette;
use downcast_rs::{Downcast, impl_downcast};
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::UnicodeVersion;
use frankenterm_surface::line::MonospaceKpCostModel;
use frankenterm_surface::{Line, SequenceNo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewlineCanon {
    None,
    LineFeed,
    CarriageReturn,
    CarriageReturnAndLineFeed,
}

/// Operator policy for OSC 52 clipboard write requests. Per
/// ft-io922 (cont of ft-2okh0.1.5).
///
/// Mirrors `frankenterm_core::osc_protocol_integration::Osc52PolicySlug`
/// without taking the cross-crate dependency. The terminal-state
/// crate sees only the local enum; the GUI integration plumbs the
/// substrate's typed-state pipeline above this layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Osc52WritePolicy {
    /// Permit OSC 52 clipboard writes (subject to the size cap from
    /// [`TerminalConfiguration::osc52_write_max_bytes`]).
    Allow,
    /// Surface the request to the operator via the GUI prompt UI.
    /// At the term-state layer (no UI surface) this is treated as
    /// `Deny`; the GUI integration intercepts before this layer is
    /// consulted.
    Prompt,
    /// Refuse the write. The operator-facing error path is silent
    /// — per the bead's privacy rule, denied requests do not log
    /// the clipboard contents. Telemetry counts the deny.
    Deny,
}

/// Outcome of running an OSC 52 SetSelection through
/// [`route_osc52_write`]. Per ft-io922.
///
/// The `Deny*` variants carry a slug for telemetry; the integration
/// emits a counter increment but never the clipboard bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Osc52WriteOutcome<'a> {
    /// Policy gate passed and size cap satisfied — write the bytes
    /// to the OS clipboard.
    Allow { bytes: &'a [u8] },
    /// Policy gate said `Prompt` but the term-state layer cannot
    /// surface UI — defer to the integration above this layer.
    /// Carries the bytes so the integration can resolve the prompt
    /// without re-decoding.
    Prompt { bytes: &'a [u8] },
    /// Operator policy denied the write.
    DenyByPolicy,
    /// Decoded payload exceeded
    /// [`TerminalConfiguration::osc52_write_max_bytes`]. Bytes
    /// dropped before the OS clipboard is touched.
    DenyOversized {
        decoded_len: usize,
        max_bytes: usize,
    },
}

impl<'a> Osc52WriteOutcome<'a> {
    /// True iff the OS clipboard write should proceed at this
    /// layer. `Prompt` is treated as a deferred-write — the
    /// term-state layer must NOT touch the clipboard, even for
    /// `Prompt`, because the operator hasn't approved yet.
    #[must_use]
    pub fn should_write_clipboard(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    /// Slug for telemetry / structured-log emission. Stable across
    /// versions.
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Allow { .. } => "allow",
            Self::Prompt { .. } => "prompt",
            Self::DenyByPolicy => "deny_policy",
            Self::DenyOversized { .. } => "deny_oversized",
        }
    }
}

/// Route an OSC 52 write request through the size cap + policy
/// gate. Returns the outcome the term-state consumer should act
/// on. Per ft-io922.
///
/// `decoded` is the post-base64 clipboard payload as bytes. The
/// escape parser performs the base64 decode upstream and hands the
/// raw bytes here.
#[must_use]
pub fn route_osc52_write<'a>(
    decoded: &'a [u8],
    policy: Osc52WritePolicy,
    max_bytes: usize,
) -> Osc52WriteOutcome<'a> {
    if decoded.len() > max_bytes {
        return Osc52WriteOutcome::DenyOversized {
            decoded_len: decoded.len(),
            max_bytes,
        };
    }
    match policy {
        Osc52WritePolicy::Allow => Osc52WriteOutcome::Allow { bytes: decoded },
        Osc52WritePolicy::Prompt => Osc52WriteOutcome::Prompt { bytes: decoded },
        Osc52WritePolicy::Deny => Osc52WriteOutcome::DenyByPolicy,
    }
}

impl NewlineCanon {
    fn target(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::LineFeed => Some("\n"),
            Self::CarriageReturn => Some("\r"),
            Self::CarriageReturnAndLineFeed => Some("\r\n"),
        }
    }

    pub fn canonicalize(self, text: &str) -> String {
        let target = self.target();
        let mut buf = String::new();
        let mut iter = text.chars().peekable();
        while let Some(c) = iter.next() {
            match target {
                None => buf.push(c),
                Some(canon) => {
                    if c == '\n' {
                        buf.push_str(canon);
                    } else if c == '\r' {
                        buf.push_str(canon);
                        if let Some('\n') = iter.peek() {
                            // Paired with the \r, so consume this one
                            iter.next();
                        }
                    } else {
                        buf.push(c);
                    }
                }
            }
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ft-io922 OSC 52 write-policy gate tests ────────────────────────────

    /// Default `Allow` preserves the prior behavior so existing
    /// OSC 52 yank-via-shell workflows keep working.
    #[test]
    fn route_osc52_write_allow_default_passes_bytes_through() {
        let payload = b"hello, clipboard";
        let outcome = route_osc52_write(payload, Osc52WritePolicy::Allow, 1024);
        assert_eq!(outcome, Osc52WriteOutcome::Allow { bytes: payload });
        assert!(outcome.should_write_clipboard());
        assert_eq!(outcome.slug(), "allow");
    }

    /// Operator override to `Deny` refuses every request — the
    /// outcome carries no bytes so the deny path cannot leak.
    #[test]
    fn route_osc52_write_deny_drops_bytes() {
        let payload = b"hello, clipboard";
        let outcome = route_osc52_write(payload, Osc52WritePolicy::Deny, 1024);
        assert_eq!(outcome, Osc52WriteOutcome::DenyByPolicy);
        assert!(!outcome.should_write_clipboard());
        assert_eq!(outcome.slug(), "deny_policy");
    }

    /// `Prompt` is treated as deferred at the term-state layer:
    /// the outcome carries the bytes (so the GUI integration can
    /// resolve without re-decoding) but `should_write_clipboard`
    /// returns false. The clipboard MUST NOT be touched at this
    /// layer.
    #[test]
    fn route_osc52_write_prompt_defers_to_higher_layer() {
        let payload = b"hello, clipboard";
        let outcome = route_osc52_write(payload, Osc52WritePolicy::Prompt, 1024);
        assert_eq!(outcome, Osc52WriteOutcome::Prompt { bytes: payload });
        assert!(
            !outcome.should_write_clipboard(),
            "Prompt MUST NOT trigger an immediate clipboard write — \
             integration above this layer resolves the prompt first",
        );
        assert_eq!(outcome.slug(), "prompt");
    }

    /// Size cap is enforced before the policy gate: an oversized
    /// payload is denied even with `Allow`.
    #[test]
    fn route_osc52_write_oversized_denied_even_under_allow() {
        let payload = b"x".repeat(2048);
        let outcome = route_osc52_write(&payload, Osc52WritePolicy::Allow, 1024);
        match outcome {
            Osc52WriteOutcome::DenyOversized {
                decoded_len,
                max_bytes,
            } => {
                assert_eq!(decoded_len, 2048);
                assert_eq!(max_bytes, 1024);
            }
            other => panic!("expected DenyOversized, got {:?}", other),
        }
        assert!(!outcome.should_write_clipboard());
    }

    /// Boundary: payload exactly at the cap is allowed (cap is
    /// inclusive — the byte count must EXCEED the cap to deny).
    #[test]
    fn route_osc52_write_payload_at_cap_is_allowed() {
        let payload = b"x".repeat(1024);
        let outcome = route_osc52_write(&payload, Osc52WritePolicy::Allow, 1024);
        assert!(matches!(outcome, Osc52WriteOutcome::Allow { .. }));
    }

    /// Empty payload (clear-clipboard intent) is structurally
    /// distinct from oversized and is allowed under default policy.
    #[test]
    fn route_osc52_write_empty_payload_passes() {
        let outcome = route_osc52_write(&[], Osc52WritePolicy::Allow, 1024);
        assert_eq!(outcome, Osc52WriteOutcome::Allow { bytes: &[] });
    }

    /// Slugs are stable across all variants — they're the
    /// telemetry key.
    #[test]
    fn route_osc52_write_slugs_are_distinct() {
        let payload = b"x";
        assert_eq!(
            route_osc52_write(payload, Osc52WritePolicy::Allow, 1024).slug(),
            "allow"
        );
        assert_eq!(
            route_osc52_write(payload, Osc52WritePolicy::Prompt, 1024).slug(),
            "prompt"
        );
        assert_eq!(
            route_osc52_write(payload, Osc52WritePolicy::Deny, 1024).slug(),
            "deny_policy"
        );
        assert_eq!(
            route_osc52_write(payload, Osc52WritePolicy::Allow, 0).slug(),
            "deny_oversized"
        );
    }

    #[test]
    fn newline_canon_eq() {
        assert_eq!(NewlineCanon::None, NewlineCanon::None);
        assert_ne!(NewlineCanon::None, NewlineCanon::LineFeed);
        assert_ne!(NewlineCanon::LineFeed, NewlineCanon::CarriageReturn);
    }

    #[test]
    fn newline_canon_clone_copy() {
        let a = NewlineCanon::LineFeed;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn newline_canon_debug() {
        let debug = format!("{:?}", NewlineCanon::CarriageReturnAndLineFeed);
        assert!(debug.contains("CarriageReturnAndLineFeed"));
    }

    #[test]
    fn newline_canon_default_on_unix() {
        if !cfg!(windows) {
            assert_eq!(NewlineCanon::default(), NewlineCanon::CarriageReturn);
        }
    }

    #[test]
    fn canon_none_preserves_all() {
        assert_eq!(
            NewlineCanon::None.canonicalize("a\nb\rc\r\nd"),
            "a\nb\rc\r\nd"
        );
    }

    #[test]
    fn canon_none_empty_string() {
        assert_eq!(NewlineCanon::None.canonicalize(""), "");
    }

    #[test]
    fn canon_lf_converts_cr_to_lf() {
        assert_eq!(NewlineCanon::LineFeed.canonicalize("a\rb"), "a\nb");
    }

    #[test]
    fn canon_lf_converts_crlf_to_lf() {
        assert_eq!(NewlineCanon::LineFeed.canonicalize("a\r\nb"), "a\nb");
    }

    #[test]
    fn canon_cr_converts_lf_to_cr() {
        assert_eq!(NewlineCanon::CarriageReturn.canonicalize("a\nb"), "a\rb");
    }

    #[test]
    fn canon_cr_converts_crlf_to_cr() {
        assert_eq!(NewlineCanon::CarriageReturn.canonicalize("a\r\nb"), "a\rb");
    }

    #[test]
    fn canon_crlf_converts_lf_to_crlf() {
        assert_eq!(
            NewlineCanon::CarriageReturnAndLineFeed.canonicalize("a\nb"),
            "a\r\nb"
        );
    }

    #[test]
    fn canon_crlf_converts_cr_to_crlf() {
        assert_eq!(
            NewlineCanon::CarriageReturnAndLineFeed.canonicalize("a\rb"),
            "a\r\nb"
        );
    }

    #[test]
    fn canon_no_newlines_unchanged() {
        assert_eq!(
            NewlineCanon::LineFeed.canonicalize("hello world"),
            "hello world"
        );
    }

    // Original comprehensive test
    #[test]
    fn test_canon() {
        assert_eq!(
            "hello\nthere",
            NewlineCanon::None.canonicalize("hello\nthere")
        );
        assert_eq!(
            "hello\r\nthere",
            NewlineCanon::CarriageReturnAndLineFeed.canonicalize("hello\nthere")
        );
        assert_eq!(
            "hello\rthere",
            NewlineCanon::CarriageReturn.canonicalize("hello\nthere")
        );
        assert_eq!(
            "hello\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\nthere")
        );
        assert_eq!(
            "hello\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\r\nthere")
        );
        assert_eq!(
            "hello\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\rthere")
        );
        assert_eq!(
            "hello\n\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\r\rthere")
        );
        assert_eq!(
            "hello\n\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\r\n\rthere")
        );
        assert_eq!(
            "hello\n\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\r\n\nthere")
        );
        assert_eq!(
            "hello\n\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\r\n\r\nthere")
        );
        assert_eq!(
            "hello\n\n\nthere",
            NewlineCanon::LineFeed.canonicalize("hello\r\r\n\nthere")
        );
    }

    // ── BidiMode ────────────────────────────────────────────

    #[test]
    fn bidi_mode_eq() {
        let a = BidiMode {
            enabled: true,
            hint: ParagraphDirectionHint::LeftToRight,
        };
        let b = BidiMode {
            enabled: true,
            hint: ParagraphDirectionHint::LeftToRight,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn bidi_mode_ne() {
        let a = BidiMode {
            enabled: true,
            hint: ParagraphDirectionHint::LeftToRight,
        };
        let b = BidiMode {
            enabled: false,
            hint: ParagraphDirectionHint::LeftToRight,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn bidi_mode_clone_copy() {
        let a = BidiMode {
            enabled: true,
            hint: ParagraphDirectionHint::LeftToRight,
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn bidi_mode_debug() {
        let mode = BidiMode {
            enabled: true,
            hint: ParagraphDirectionHint::LeftToRight,
        };
        let debug = format!("{mode:?}");
        assert!(debug.contains("enabled"));
    }
}

impl Default for NewlineCanon {
    fn default() -> Self {
        // This is a bit horrible; in general we try to stick with unix line
        // endings as the one-true representation because using canonical
        // CRLF can result in excess blank lines during a paste operation.
        // On Windows we're in a bit of a frustrating situation: pasting into
        // Windows console programs requires CRLF otherwise there is no newline
        // at all, but when in WSL, pasting with CRLF gives excess blank lines.
        //
        // To come to a compromise, if wezterm is running on Windows then we'll
        // use canonical CRLF unless the embedded application has enabled
        // bracketed paste: we can use bracketed paste mode as a signal that
        // the application will prefer newlines.
        //
        // In practice this means that unix shells and vim will get the
        // unix newlines in their pastes (which is the UX I want) and
        // cmd.exe will get CRLF.
        if cfg!(windows) {
            Self::CarriageReturnAndLineFeed
        } else {
            // For compatibility with the `nano` editor, which unfortunately
            // treats \n as a shortcut that justifies text
            // <https://savannah.gnu.org/bugs/?49176>, we default to
            // \r which is typically fine.
            // <https://github.com/wezterm/wezterm/issues/1575>
            Self::CarriageReturn
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackTierConfig {
    /// Enables tier-aware scrollback budgeting in the terminal model.
    pub enabled: bool,
    /// Maximum number of scrollback lines kept in the in-memory hot tier.
    pub hot_lines: usize,
    /// Approximate byte budget for warm-tier accounting.
    pub warm_max_bytes: usize,
}

impl Default for ScrollbackTierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hot_lines: 1000,
            warm_max_bytes: 0,
        }
    }
}

/// TerminalConfiguration allows for the embedding application to pass configuration
/// information to the Terminal.
/// The configuration can be changed at runtime; provided that the implementation
/// increments the generation counter appropriately, the changes will be detected
/// and applied at the next appropriate opportunity.
pub trait TerminalConfiguration: Downcast + std::fmt::Debug + Send + Sync {
    /// Returns a generation counter for the active
    /// configuration.  If the implementation may be
    /// changed at runtime, it must increment the generation
    /// number with each change so that any caches maintained
    /// by the terminal can be flushed.
    fn generation(&self) -> usize {
        0
    }

    /// Returns the size of the scrollback in terms of the number of rows.
    fn scrollback_size(&self) -> usize {
        3500
    }

    /// Tiered scrollback budgeting controls.
    ///
    /// By default this is disabled and the terminal retains all configured
    /// scrollback rows in memory.
    fn scrollback_tier_config(&self) -> ScrollbackTierConfig {
        ScrollbackTierConfig {
            enabled: false,
            hot_lines: self.scrollback_size().max(1),
            warm_max_bytes: 0,
        }
    }

    /// Cost model used by resize-time bounded KP wrapping.
    fn resize_wrap_kp_cost_model(&self) -> MonospaceKpCostModel {
        MonospaceKpCostModel::terminal_default()
    }

    /// Enables per-line wrap scorecard emission during resize.
    fn resize_wrap_scorecard_enabled(&self) -> bool {
        false
    }

    /// Enables readability gate evaluation over aggregate resize scorecard metrics.
    fn resize_wrap_readability_gate_enabled(&self) -> bool {
        false
    }

    /// Maximum allowed per-line badness delta versus greedy baseline.
    fn resize_wrap_readability_max_line_badness_delta(&self) -> i64 {
        0
    }

    /// Maximum allowed total badness delta versus greedy baseline.
    fn resize_wrap_readability_max_total_badness_delta(&self) -> i64 {
        0
    }

    /// Maximum allowed fallback ratio (% of wrapped lines using fallback mode).
    fn resize_wrap_readability_max_fallback_ratio_percent(&self) -> u8 {
        100
    }

    /// Return true if the embedding application wants to use CSI-u encoding
    /// for keys that would otherwise be ambiguous.
    /// <http://www.leonerd.org.uk/hacks/fixterms/>
    fn enable_csi_u_key_encoding(&self) -> bool {
        false
    }

    /// Returns the default color palette for the application.
    /// Various escape sequences can dynamically modify the effective
    /// color palette for a terminal instance at runtime, but this method
    /// defines the initial palette.
    fn color_palette(&self) -> ColorPalette;

    fn canonicalize_pasted_newlines(&self) -> NewlineCanon {
        NewlineCanon::default()
    }

    fn alternate_buffer_wheel_scroll_speed(&self) -> u8 {
        3
    }

    fn enq_answerback(&self) -> String {
        "".to_string()
    }

    fn enable_kitty_graphics(&self) -> bool {
        false
    }

    fn enable_kitty_keyboard(&self) -> bool {
        false
    }

    /// Memory budget (bytes) for Kitty image protocol data.
    /// Default: 320 MiB.
    fn kitty_image_budget_bytes(&self) -> usize {
        320 * 1024 * 1024
    }

    /// Per-image transmission-size cap for Kitty graphics (ft-heic8).
    /// Applied to the post-decompression payload to defend against
    /// memory-bomb APC sequences (zlib-compressed PNG that
    /// decompresses to GBs of RGBA, or oversized direct payloads).
    /// Default: 16 MiB — fits a 2048×2048 RGBA frame and
    /// accommodates typical PNGs from image.nvim, yazi, and Kitty's
    /// `icat`. Operators on hosts with large-image workflows
    /// (4K AI-generated previews, scientific imaging) may raise it.
    fn kitty_image_max_transmission_bytes(&self) -> usize {
        16 * 1024 * 1024
    }

    /// Operator policy for OSC 52 clipboard write requests
    /// (`\x1b]52;<sel>;<base64>\x1b\\`). Per ft-io922 (cont of
    /// ft-2okh0.1.5): until this trait method gained the gate, every
    /// OSC 52 SetSelection unconditionally rewrote the OS clipboard
    /// — a trust gap when shell output is attacker-influenced.
    ///
    /// Default `Allow` preserves the prior behavior so existing
    /// operator workflows (yank-via-osc52 in vim/tmux) keep working;
    /// privacy-conservative deployments can override to `Prompt` or
    /// `Deny`.
    ///
    /// `Prompt` is treated as `Deny` at this layer because the
    /// terminal-state crate has no UI surface to ask the operator;
    /// the GUI-layer integration (cont-bead) intercepts before this
    /// method is consulted to display the prompt.
    fn osc52_write_policy(&self) -> Osc52WritePolicy {
        Osc52WritePolicy::Allow
    }

    /// Maximum bytes of decoded clipboard payload an OSC 52 write
    /// request is allowed to deliver before the policy gate
    /// auto-denies. Per ft-io922.
    ///
    /// Default 1 MiB — comfortably above any human-typed clipboard
    /// and any single-screen yank, well below the OS clipboard
    /// stress-thresholds. The cap is enforced before the OS
    /// clipboard is touched so a malicious source cannot use OSC 52
    /// as a memory-amplification primitive.
    fn osc52_write_max_bytes(&self) -> usize {
        1024 * 1024
    }

    /// Maximum number of user variables (iTerm2 SetUserVar).
    /// Prevents unbounded growth. Default: 512.
    fn max_user_vars(&self) -> usize {
        512
    }

    /// Maximum depth of the unicode version stack.
    /// Prevents unbounded growth from unbalanced Push operations.
    /// Default: 64.
    fn max_unicode_version_stack_depth(&self) -> usize {
        64
    }

    /// Maximum length (bytes) for the accumulating OSC title string.
    /// Prevents unbounded growth from malformed escape sequences.
    /// Default: 8192.
    fn max_accumulating_title_len(&self) -> usize {
        8192
    }

    /// Maximum entries in the sixel color register map.
    /// Default: 4096.
    fn max_color_map_entries(&self) -> usize {
        4096
    }

    /// The default unicode version to assume.
    /// This affects how the width of certain sequences is interpreted.
    /// At the time of writing, we default to 9 even though the current
    /// version of unicode is 14.  14 introduced emoji presentation selectors
    /// that also alter the width of certain sequences, and that is too
    /// new for most deployed applications.
    // Coupled with config/src/lib.rs:default_unicode_version
    fn unicode_version(&self) -> UnicodeVersion {
        UnicodeVersion {
            version: 9,
            ambiguous_are_wide: false,
            cell_widths: None,
        }
    }

    /// Whether to normalize incoming text runs to
    /// canonical NFC unicode representation
    fn normalize_output_to_unicode_nfc(&self) -> bool {
        false
    }

    fn debug_key_events(&self) -> bool {
        false
    }

    /// Returns (bidi_enabled, direction hint) that should be used
    /// unless an escape sequence has changed the default mode
    fn bidi_mode(&self) -> BidiMode {
        BidiMode {
            enabled: false,
            hint: ParagraphDirectionHint::LeftToRight,
        }
    }

    /// Disabled by default per:
    /// <https://marc.info/?l=bugtraq&m=104612710031920&w=2>
    fn enable_title_reporting(&self) -> bool {
        false
    }

    fn log_unknown_escape_sequences(&self) -> bool {
        false
    }
}
impl_downcast!(TerminalConfiguration);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BidiMode {
    pub enabled: bool,
    pub hint: ParagraphDirectionHint,
}

impl BidiMode {
    pub fn apply_to_line(&self, line: &mut Line, seqno: SequenceNo) {
        line.set_bidi_info(self.enabled, self.hint, seqno);
    }
}
