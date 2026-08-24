use crate::color::ColorPalette;
use downcast_rs::{impl_downcast, Downcast};
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::UnicodeVersion;
use frankenterm_surface::line::MonospaceKpCostModel;
use frankenterm_surface::{Line, SequenceNo};
use std::sync::Arc;

use crate::StableRowIndex;

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

/// Identity of one immutable cold-scrollback snapshot generation.
///
/// The epoch changes after a committed logical clear. The revision changes
/// whenever rows in that epoch may have changed. The epoch bytes are kept
/// private and omitted from `Debug` so diagnostics cannot accidentally turn a
/// durable storage identity into an operator-visible capability.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackSnapshotGeneration {
    content_epoch: [u8; 16],
    revision: u64,
}

impl ScrollbackSnapshotGeneration {
    pub fn new(content_epoch: [u8; 16], revision: u64) -> Self {
        Self {
            content_epoch,
            revision,
        }
    }

    /// Opaque durable-content lineage used by authenticated recovery data.
    ///
    /// Callers must keep this value inside the same confidentiality boundary as
    /// terminal checkpoint content; `Debug` intentionally never exposes it.
    pub const fn content_epoch(self) -> [u8; 16] {
        self.content_epoch
    }

    pub const fn revision(self) -> u64 {
        self.revision
    }
}

impl std::fmt::Debug for ScrollbackSnapshotGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollbackSnapshotGeneration")
            .field("content_epoch", &"[REDACTED]")
            .field("revision", &self.revision)
            .finish()
    }
}

/// Hard limits enforced by a cold sink while it holds its snapshot boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbackSnapshotLimits {
    pub max_rows: usize,
    pub max_stored_bytes: u64,
    pub max_decoded_bytes: usize,
    pub max_physical_bytes: u64,
}

/// Whether the cold rows can participate in a semantic terminal checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbackSnapshotFidelity {
    /// Full `Line` semantics, including authoritative cell widths and
    /// attributes, are retained without content mutation.
    ExactSemantic,
    /// The current operator-transcript store redacts content and uses a legacy
    /// row codec. It is coherent for transcript reads but is not a lossless
    /// terminal-recovery source.
    LegacyRedacted,
}

/// Content-free failure reported by a cold-scrollback durability operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbackSpillError {
    ResourceLimit {
        resource: &'static str,
        observed: u64,
        maximum: u64,
    },
    ArithmeticOverflow(&'static str),
    SnapshotRangeMismatch,
    SnapshotGenerationMismatch,
    SnapshotRowMissing,
    StorageUnavailable,
    RevisionExhausted,
    /// A manifest rename may have published, but its parent-directory sync
    /// failed. The sink is quarantined until reopen verifies disk state.
    CommitOutcomeIndeterminate,
}

impl std::fmt::Display for ScrollbackSpillError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResourceLimit {
                resource,
                observed,
                maximum,
            } => write!(
                formatter,
                "cold scrollback {resource} exceeds limit ({observed} > {maximum})"
            ),
            Self::ArithmeticOverflow(resource) => {
                write!(formatter, "cold scrollback {resource} arithmetic overflow")
            }
            Self::SnapshotRangeMismatch => {
                formatter.write_str("cold scrollback snapshot range mismatch")
            }
            Self::SnapshotGenerationMismatch => {
                formatter.write_str("cold scrollback snapshot generation mismatch")
            }
            Self::SnapshotRowMissing => {
                formatter.write_str("cold scrollback snapshot row missing")
            }
            Self::StorageUnavailable => {
                formatter.write_str("cold scrollback storage unavailable")
            }
            Self::RevisionExhausted => {
                formatter.write_str("cold scrollback snapshot revision exhausted")
            }
            Self::CommitOutcomeIndeterminate => {
                formatter.write_str("cold scrollback commit outcome indeterminate")
            }
        }
    }
}

impl std::error::Error for ScrollbackSpillError {}

/// Content-free failure of the recovery-to-live scrollback retiering
/// transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbackActivationError {
    MissingRecoveryBoundary,
    InvalidRecoveryBoundary,
    ConfiguredRetentionInsufficient,
    MissingStorageCapability,
    Spill(ScrollbackSpillError),
    /// The sink reported success, but its receipt did not prove the exact
    /// publication requested by the terminal. Publication may already be
    /// durable, so this outcome must never be retried against the stale
    /// checkpoint generation.
    CommitOutcomeIndeterminate,
}

impl ScrollbackActivationError {
    /// Whether retrying this activation against the checkpoint generation
    /// could overwrite or conflict with a publication that may already be
    /// durable.
    pub const fn outcome_is_indeterminate(self) -> bool {
        matches!(
            self,
            Self::CommitOutcomeIndeterminate
                | Self::Spill(ScrollbackSpillError::CommitOutcomeIndeterminate)
        )
    }
}

impl std::fmt::Display for ScrollbackActivationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRecoveryBoundary => {
                formatter.write_str("recovery cold-prefix boundary is missing")
            }
            Self::InvalidRecoveryBoundary => {
                formatter.write_str("recovery cold-prefix boundary is inconsistent")
            }
            Self::ConfiguredRetentionInsufficient => formatter
                .write_str("live scrollback retention cannot preserve every recovered row"),
            Self::MissingStorageCapability => {
                formatter.write_str("live configuration has no required cold-store capability")
            }
            Self::Spill(error) => std::fmt::Display::fmt(error, formatter),
            Self::CommitOutcomeIndeterminate => {
                formatter.write_str("cold-store replacement outcome is indeterminate")
            }
        }
    }
}

impl std::error::Error for ScrollbackActivationError {}

/// Immutable, contiguous rows captured under one sink-owned mutation boundary.
///
/// `Debug` deliberately reports only structural metadata; it never prints row
/// content. Construction validates the exact half-open range represented by
/// the row vector.
pub struct ScrollbackSnapshot {
    generation: ScrollbackSnapshotGeneration,
    fidelity: ScrollbackSnapshotFidelity,
    oldest_stable_row: Option<StableRowIndex>,
    newest_stable_row_exclusive: StableRowIndex,
    stored_bytes: u64,
    decoded_bytes: usize,
    rows: Vec<Line>,
}

impl ScrollbackSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn from_contiguous_rows(
        generation: ScrollbackSnapshotGeneration,
        fidelity: ScrollbackSnapshotFidelity,
        oldest_stable_row: Option<StableRowIndex>,
        newest_stable_row_exclusive: StableRowIndex,
        stored_bytes: u64,
        decoded_bytes: usize,
        rows: Vec<Line>,
    ) -> Result<Self, ScrollbackSpillError> {
        match (oldest_stable_row, rows.is_empty()) {
            (None, true) => {}
            (Some(oldest), false) => {
                let row_count = StableRowIndex::try_from(rows.len())
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("row_count"))?;
                let observed_newest = oldest
                    .checked_add(row_count)
                    .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
                if observed_newest != newest_stable_row_exclusive {
                    return Err(ScrollbackSpillError::SnapshotRangeMismatch);
                }
            }
            _ => return Err(ScrollbackSpillError::SnapshotRangeMismatch),
        }
        Ok(Self {
            generation,
            fidelity,
            oldest_stable_row,
            newest_stable_row_exclusive,
            stored_bytes,
            decoded_bytes,
            rows,
        })
    }

    pub fn generation(&self) -> ScrollbackSnapshotGeneration {
        self.generation
    }

    pub fn fidelity(&self) -> ScrollbackSnapshotFidelity {
        self.fidelity
    }

    pub fn oldest_stable_row(&self) -> Option<StableRowIndex> {
        self.oldest_stable_row
    }

    pub fn newest_stable_row_exclusive(&self) -> StableRowIndex {
        self.newest_stable_row_exclusive
    }

    pub fn stored_bytes(&self) -> u64 {
        self.stored_bytes
    }

    pub fn decoded_bytes(&self) -> usize {
        self.decoded_bytes
    }

    pub fn rows(&self) -> &[Line] {
        &self.rows
    }

    pub fn into_rows(self) -> Vec<Line> {
        self.rows
    }
}

impl std::fmt::Debug for ScrollbackSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollbackSnapshot")
            .field("generation", &self.generation)
            .field("fidelity", &self.fidelity)
            .field("oldest_stable_row", &self.oldest_stable_row)
            .field(
                "newest_stable_row_exclusive",
                &self.newest_stable_row_exclusive,
            )
            .field("stored_bytes", &self.stored_bytes)
            .field("decoded_bytes", &self.decoded_bytes)
            .field("row_count", &self.rows.len())
            .finish()
    }
}

/// Borrowed exact-semantic prefix offered to an atomic cold-store replacement.
///
/// A `VecDeque` may wrap, so the prefix is represented as at most two slices.
/// Construction proves that those slices exactly cover the declared stable-row
/// range. `Debug` reports only bounded structural metadata and never row
/// content.
pub struct ScrollbackPrefix<'rows> {
    oldest_stable_row: Option<StableRowIndex>,
    newest_stable_row_exclusive: StableRowIndex,
    first: &'rows [Line],
    second: &'rows [Line],
}

impl<'rows> ScrollbackPrefix<'rows> {
    pub fn from_slices(
        oldest_stable_row: Option<StableRowIndex>,
        newest_stable_row_exclusive: StableRowIndex,
        first: &'rows [Line],
        second: &'rows [Line],
    ) -> Result<Self, ScrollbackSpillError> {
        let row_count = first
            .len()
            .checked_add(second.len())
            .ok_or(ScrollbackSpillError::ArithmeticOverflow("row_count"))?;
        match (oldest_stable_row, row_count) {
            (None, 0) => {}
            (Some(oldest), count) if count != 0 => {
                let count = StableRowIndex::try_from(count)
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("row_count"))?;
                let observed_newest = oldest.checked_add(count).ok_or(
                    ScrollbackSpillError::ArithmeticOverflow("stable_row_range"),
                )?;
                if observed_newest != newest_stable_row_exclusive {
                    return Err(ScrollbackSpillError::SnapshotRangeMismatch);
                }
            }
            _ => return Err(ScrollbackSpillError::SnapshotRangeMismatch),
        }
        Ok(Self {
            oldest_stable_row,
            newest_stable_row_exclusive,
            first,
            second,
        })
    }

    pub const fn oldest_stable_row(&self) -> Option<StableRowIndex> {
        self.oldest_stable_row
    }

    pub const fn newest_stable_row_exclusive(&self) -> StableRowIndex {
        self.newest_stable_row_exclusive
    }

    pub fn row_count(&self) -> usize {
        self.first.len() + self.second.len()
    }

    pub fn rows(&self) -> impl Iterator<Item = &'rows Line> + '_ {
        self.first.iter().chain(self.second.iter())
    }
}

impl std::fmt::Debug for ScrollbackPrefix<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollbackPrefix")
            .field("oldest_stable_row", &self.oldest_stable_row)
            .field(
                "newest_stable_row_exclusive",
                &self.newest_stable_row_exclusive,
            )
            .field("row_count", &self.row_count())
            .finish()
    }
}

/// Receipt proving that a complete cold-prefix replacement reached its durable
/// logical commit point.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackReplaceCommit {
    generation: ScrollbackSnapshotGeneration,
    oldest_stable_row: Option<StableRowIndex>,
    newest_stable_row_exclusive: StableRowIndex,
}

impl ScrollbackReplaceCommit {
    pub const fn new(
        generation: ScrollbackSnapshotGeneration,
        oldest_stable_row: Option<StableRowIndex>,
        newest_stable_row_exclusive: StableRowIndex,
    ) -> Self {
        Self {
            generation,
            oldest_stable_row,
            newest_stable_row_exclusive,
        }
    }

    pub const fn generation(self) -> ScrollbackSnapshotGeneration {
        self.generation
    }

    pub const fn oldest_stable_row(self) -> Option<StableRowIndex> {
        self.oldest_stable_row
    }

    pub const fn newest_stable_row_exclusive(self) -> StableRowIndex {
        self.newest_stable_row_exclusive
    }
}

impl std::fmt::Debug for ScrollbackReplaceCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollbackReplaceCommit")
            .field("generation", &self.generation)
            .field("oldest_stable_row", &self.oldest_stable_row)
            .field(
                "newest_stable_row_exclusive",
                &self.newest_stable_row_exclusive,
            )
            .finish()
    }
}

/// Receipt proving that a logical clear reached its durable commit point.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackClearCommit {
    generation: ScrollbackSnapshotGeneration,
}

impl ScrollbackClearCommit {
    pub fn new(generation: ScrollbackSnapshotGeneration) -> Self {
        Self { generation }
    }

    pub fn generation(&self) -> ScrollbackSnapshotGeneration {
        self.generation
    }
}

impl std::fmt::Debug for ScrollbackClearCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrollbackClearCommit")
            .field("generation", &self.generation)
            .finish()
    }
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

/// External cold-scrollback sink used by tiered scrollback integrations.
///
/// The terminal model owns full-fidelity [`Line`] values and knows exactly when
/// a stable row leaves the in-memory hot tier. The persistence layer owns disk
/// policy, redaction, encryption, retention, and crash safety. This trait is the
/// boundary between those responsibilities: terminal code offers evicted rows,
/// and a higher layer may persist and hydrate them without forcing this crate to
/// depend on storage/redaction crates.
pub trait ScrollbackSpillSink: std::fmt::Debug + Send + Sync {
    /// Persist a row that just left the in-memory hot tier.
    ///
    /// `max_retained_rows` is the configured total scrollback budget minus the
    /// hot rows still resident in [`Screen`](crate::screen::Screen). Sinks should
    /// use it to bound their metadata and backing store.
    fn store_scrollback_line(
        &self,
        stable_row: StableRowIndex,
        line: &Line,
        max_retained_rows: usize,
    ) -> bool;

    /// Hydrate a previously stored stable row.
    fn load_scrollback_line(&self, stable_row: StableRowIndex) -> Option<Line>;

    /// Oldest stable row still reachable from this sink.
    fn oldest_scrollback_row(&self) -> Option<StableRowIndex>;

    /// Count of rows retained outside the hot in-memory screen buffer.
    fn retained_scrollback_rows(&self) -> usize;

    /// Approximate bytes retained outside the hot in-memory screen buffer.
    fn retained_scrollback_bytes(&self) -> usize;

    /// Capture one exact, contiguous logical generation ending immediately
    /// before the current in-memory hot tier.
    ///
    /// Implementations must hold their mutation boundary from metadata capture
    /// through the final row decode and enforce every supplied limit before
    /// returning. A successful result must represent exactly
    /// `[oldest_stable_row, expected_newest_exclusive)`.
    fn snapshot_scrollback(
        &self,
        expected_newest_exclusive: StableRowIndex,
        limits: ScrollbackSnapshotLimits,
    ) -> Result<ScrollbackSnapshot, ScrollbackSpillError>;

    /// Compare-and-swap the complete logical cold store to one exact prefix.
    ///
    /// The implementation must hold a single mutation boundary while it:
    ///
    /// 1. verifies `expected_generation` equals the currently published
    ///    generation (or, for `None`, verifies a pristine revision-zero sink
    ///    with no logical rows),
    /// 2. validates and stages every exact-semantic row without truncation,
    /// 3. durably publishes a manifest that names exactly `prefix`, and
    /// 4. removes every stale logical prefix or suffix from reachability.
    ///
    /// A successful empty prefix is an atomic logical replacement with an
    /// empty set, not a clear-then-store sequence. If `prefix.row_count()`
    /// exceeds `max_retained_rows`, the method must reject it rather than drop
    /// rows. The returned receipt must name the exact requested half-open
    /// range. Its generation must preserve the predecessor content epoch and
    /// advance its revision by exactly one when `expected_generation` is
    /// `Some`; after `None`, the returned revision must be exactly one. This
    /// prevents a checkpoint with no cold generation from overwriting an
    /// empty-but-advanced lineage. Ordinary
    /// errors prove that no logical publication occurred; indeterminate
    /// publication must quarantine the sink and return
    /// [`ScrollbackSpillError::CommitOutcomeIndeterminate`]. A caller that
    /// observes a malformed success receipt must likewise treat publication as
    /// indeterminate and must not retry until reopening the sink reconciles its
    /// authenticated manifest. Physical reclamation of superseded ledgers may
    /// happen after the durable commit.
    fn replace_scrollback_prefix(
        &self,
        expected_generation: Option<ScrollbackSnapshotGeneration>,
        prefix: ScrollbackPrefix<'_>,
        max_retained_rows: usize,
    ) -> Result<ScrollbackReplaceCommit, ScrollbackSpillError>;

    /// Commit a logical clear before returning success.
    ///
    /// Physical reclamation may be retried after this method returns, but
    /// Callers must retain their in-memory rows on every `Err`. Ordinary errors
    /// mean no logical clear was committed. The explicit
    /// [`ScrollbackSpillError::CommitOutcomeIndeterminate`] variant means the
    /// sink has quarantined itself until reopen verifies whether publication
    /// reached durable storage.
    fn clear_scrollback(&self) -> Result<ScrollbackClearCommit, ScrollbackSpillError>;
}

/// Bounded synchronous exclusion held across recovery activation.
///
/// The default lease is appropriate only for immutable configuration
/// implementations. Any implementation with interior mutation must override
/// [`TerminalConfiguration::acquire_recovery_activation_lease`] and make every
/// semantic setter acquire the same exclusion gate before mutation.
pub trait RecoveryActivationLease: std::fmt::Debug {}

#[derive(Debug)]
struct ImmutableRecoveryActivationLease;

impl RecoveryActivationLease for ImmutableRecoveryActivationLease {}

/// Complete identity of the terminal configuration visible to recovery code.
///
/// `base_generation` retains the long-standing configuration generation,
/// while `overlay_generation` fences independently mutable runtime overlays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalConfigurationRevision {
    pub base_generation: usize,
    pub overlay_generation: usize,
}

impl TerminalConfigurationRevision {
    pub const fn new(base_generation: usize, overlay_generation: usize) -> Self {
        Self {
            base_generation,
            overlay_generation,
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

    /// Returns the complete configuration revision used to fence checkpoint
    /// capture and recovery. Implementations with no independently mutable
    /// overlay inherit the legacy generation as their base revision.
    fn revision(&self) -> TerminalConfigurationRevision {
        TerminalConfigurationRevision::new(self.generation(), 0)
    }

    /// Exclude semantic configuration mutation across the final verified
    /// restore-to-activation transaction.
    ///
    /// This lease is deliberately synchronous and bounded: activation performs
    /// no async work while holding it. Immutable implementations may inherit
    /// the no-op lease. Interior-mutable implementations must override this
    /// method and route every setter through the same gate.
    fn acquire_recovery_activation_lease(&self) -> Box<dyn RecoveryActivationLease + '_> {
        Box::new(ImmutableRecoveryActivationLease)
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

    /// Optional spill/hydration hook for rows evicted from tiered scrollback.
    fn scrollback_spill_sink(&self) -> Option<Arc<dyn ScrollbackSpillSink>> {
        None
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

    fn enable_checksum_rectangular_area(&self) -> bool {
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
