//! Kitty graphics alt-text resolution + screen-reader sanitiser
//! substrate (ft-h8s0p / BR-TERM-EMULATOR-UPLIFT-2.1.cont-graphics-2).
//!
//! Pure-logic substrate covering the substrate-shaped pieces of
//! the bead: alt-text source resolution (Kitty `X=<base64>`
//! field vs filename fallback), screen-reader sanitiser
//! (length cap, control-char scrub), `AccessibilityAlert`
//! event payload, and per-release conformance/attestation
//! schema.
//!
//! ## What this module ships
//!
//! - `AltTextSource` 4-variant — `ExplicitX / FilenameFallback /
//!   None / Sanitised{from}`. Audit-trail-friendly: substrate
//!   keeps the original source even after sanitisation.
//! - `AltTextResolution` decision tree: prefer the explicit
//!   `X=` base64 field; fall back to filename for file-source
//!   transmissions; return `None` otherwise.
//! - `AltTextSanitizer` — strips control chars, normalises
//!   whitespace, caps length at 256 chars (configurable). The
//!   bead's "screen-reader announcement" rule: an alt-text
//!   string that won't crash a screen reader.
//! - `AccessibilityAlert` event type the integration emits at
//!   image admission. Bead: "emit Alert::ImageAltText {
//!   image_id, text } so the GUI accessibility tree can
//!   announce it".
//! - `ConformanceFixture` schema — `{ name, input_bytes_hash,
//!   expected_image_id, expected_alt_text, expected_placement }`
//!   for the bead's `tests/golden/kitty_graphics/` corpus.
//! - `ProtocolCoverageAttestation` schema for
//!   `docs/attestations/protocol-coverage-<version>.json`.
//! - `KittyAltTextTelemetry` per-session counters.
//!
//! ## What is deferred to ft-h8s0p follow-up
//!
//! - Extending `frankenterm/escape-parser/src/apc.rs` to
//!   capture the `X=` field per the Kitty extension.
//! - Plumbing into `TerminalState::ingest_kitty_image` to
//!   emit `Alert::ImageAltText` on admission.
//! - Conformance corpus byte-stream fixtures (image.nvim /
//!   yazi / icat).
//! - Feature-flag rollout staging.
//! - Attestation generation hooked into the release script.

#![allow(dead_code)]

// ============================================================================
// AltTextSource — provenance
// ============================================================================

/// Where the resolved alt-text came from. Substrate keeps the
/// provenance even after sanitisation so the integration can
/// log "ExplicitX (truncated)" vs "FilenameFallback" for the
/// audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AltTextSource {
    /// Explicit `X=<base64>` field per the Kitty extension.
    ExplicitX,
    /// Fallback to the file's basename for file-source
    /// transmissions (`t=f` / `t=t` per Kitty).
    FilenameFallback,
    /// No alt-text available.
    #[default]
    None,
    /// Source was one of the above but the sanitiser
    /// modified the contents (truncation or control-char
    /// scrub). The integration's audit trail uses this to
    /// note "alt text was modified".
    Sanitised { from: SanitisedSource },
}

/// Inner source for the `Sanitised { from }` variant. Kept
/// separate so `AltTextSource` is `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SanitisedSource {
    ExplicitX,
    FilenameFallback,
}

impl AltTextSource {
    /// Whether the alt-text is suitable for screen-reader
    /// announcement.
    #[must_use]
    pub const fn announceable(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Stable string label for telemetry / attestation.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ExplicitX => "explicit_x",
            Self::FilenameFallback => "filename_fallback",
            Self::None => "none",
            Self::Sanitised { from: SanitisedSource::ExplicitX } => "sanitised_explicit_x",
            Self::Sanitised { from: SanitisedSource::FilenameFallback } => {
                "sanitised_filename_fallback"
            }
        }
    }
}

// ============================================================================
// AltTextResolution — decision
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltTextResolution {
    pub source: AltTextSource,
    pub text: Option<String>,
}

impl AltTextResolution {
    pub const fn missing() -> Self {
        Self { source: AltTextSource::None, text: None }
    }
}

/// Pure-logic resolution: prefer the explicit `X=` field over
/// the filename fallback. Caller has already base64-decoded the
/// `X=` field where applicable.
///
/// `explicit_x_decoded` is the post-base64 `X=` value (None if
/// the field was absent or decode failed).
/// `filename` is the basename of the source file for `t=f` /
/// `t=t` transmissions (None for inline `t=d` / `t=s`
/// transmissions).
#[must_use]
pub fn resolve_alt_text(
    explicit_x_decoded: Option<&str>,
    filename: Option<&str>,
) -> AltTextResolution {
    if let Some(x) = explicit_x_decoded {
        if !x.is_empty() {
            return AltTextResolution {
                source: AltTextSource::ExplicitX,
                text: Some(x.to_string()),
            };
        }
    }
    if let Some(f) = filename {
        if !f.is_empty() {
            return AltTextResolution {
                source: AltTextSource::FilenameFallback,
                text: Some(f.to_string()),
            };
        }
    }
    AltTextResolution::missing()
}

// ============================================================================
// AltTextSanitizer
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltTextSanitizerConfig {
    /// Maximum length in characters. Bead default 256 — long
    /// enough to be useful, short enough that VoiceOver/Orca
    /// don't choke.
    pub max_chars: usize,
    /// Strip C0 control chars (`\x00..\x1f`) other than
    /// `\t` (which collapses to space).
    pub scrub_control_chars: bool,
    /// Collapse runs of whitespace to a single space.
    pub collapse_whitespace: bool,
}

pub const DEFAULT_MAX_ALT_TEXT_CHARS: usize = 256;

impl Default for AltTextSanitizerConfig {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_ALT_TEXT_CHARS,
            scrub_control_chars: true,
            collapse_whitespace: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizationOutcome {
    pub sanitised_text: String,
    pub modified: bool,
    pub truncated: bool,
    pub control_chars_scrubbed: u32,
    pub whitespace_collapsed: u32,
}

/// Pure sanitiser. Returns the sanitised string + flags
/// describing what changed; the integration plumbs the flags
/// into `KittyAltTextTelemetry`.
#[must_use]
pub fn sanitize_alt_text(input: &str, config: AltTextSanitizerConfig) -> SanitizationOutcome {
    let mut out = String::with_capacity(input.len());
    let mut modified = false;
    let mut control_chars_scrubbed = 0u32;
    let mut whitespace_collapsed = 0u32;
    let mut prev_was_space = false;

    for ch in input.chars() {
        if config.scrub_control_chars && (ch as u32) < 0x20 && ch != ' ' {
            control_chars_scrubbed += 1;
            modified = true;
            // \t and \n collapse to one space; other controls drop.
            if matches!(ch, '\t' | '\n' | '\r') {
                if !prev_was_space {
                    out.push(' ');
                    prev_was_space = true;
                }
            }
            continue;
        }
        if config.collapse_whitespace && ch.is_whitespace() {
            if prev_was_space {
                whitespace_collapsed += 1;
                modified = true;
                continue;
            }
            out.push(' ');
            prev_was_space = true;
            continue;
        }
        out.push(ch);
        prev_was_space = false;
    }

    // Trim trailing whitespace from collapse.
    let trimmed_len = out.trim_end().len();
    if trimmed_len < out.len() {
        out.truncate(trimmed_len);
        modified = true;
    }

    let mut truncated = false;
    if out.chars().count() > config.max_chars {
        let mut iter = out.char_indices();
        let split = iter
            .nth(config.max_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(out.len());
        out.truncate(split);
        truncated = true;
        modified = true;
    }

    SanitizationOutcome {
        sanitised_text: out,
        modified,
        truncated,
        control_chars_scrubbed,
        whitespace_collapsed,
    }
}

/// Compose `resolve_alt_text` + `sanitize_alt_text` and
/// upgrade the source provenance to `Sanitised { from }` when
/// the sanitiser modified the text.
#[must_use]
pub fn resolve_and_sanitize(
    explicit_x_decoded: Option<&str>,
    filename: Option<&str>,
    config: AltTextSanitizerConfig,
) -> AltTextResolution {
    let resolution = resolve_alt_text(explicit_x_decoded, filename);
    let source = resolution.source;
    let Some(text) = resolution.text else {
        return AltTextResolution { source, text: None };
    };
    let outcome = sanitize_alt_text(&text, config);
    if !outcome.modified {
        return AltTextResolution {
            source,
            text: Some(outcome.sanitised_text),
        };
    }
    let from = match source {
        AltTextSource::ExplicitX => SanitisedSource::ExplicitX,
        AltTextSource::FilenameFallback => SanitisedSource::FilenameFallback,
        // Unreachable: text=Some implies source was ExplicitX or
        // FilenameFallback. Belt-and-braces: keep the original.
        _ => return AltTextResolution { source, text: Some(text) },
    };
    AltTextResolution {
        source: AltTextSource::Sanitised { from },
        text: Some(outcome.sanitised_text),
    }
}

// ============================================================================
// AccessibilityAlert
// ============================================================================

/// The bead's `Alert::ImageAltText { image_id, text }` event.
/// The GUI accessibility tree subscribes to these and forwards
/// to the platform AT (VoiceOver / Orca / Narrator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityAlert {
    pub image_id: u32,
    pub text: String,
    pub source: AltTextSource,
    /// Pane that received the image (so multi-pane sessions
    /// announce against the right surface).
    pub pane_id: u64,
}

// ============================================================================
// ConformanceFixture
// ============================================================================

/// Schema for `tests/golden/kitty_graphics/<scenario>/` per the
/// bead. Substrate carries the schema; the integration's
/// fixture loader populates from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceFixture {
    /// Human-readable name (`image_nvim`, `yazi`, `icat`).
    pub name: String,
    /// SHA-256 of the input APC bytes for tamper detection.
    pub input_bytes_sha256: [u8; 32],
    pub expected_image_id: u32,
    pub expected_alt_text: AltTextResolution,
    pub expected_placement: ExpectedPlacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedPlacement {
    Virtual { z: i32, cell_x: u32, cell_y: u32 },
    Classical { px_x: i32, px_y: i32 },
    None,
}

// ============================================================================
// ProtocolCoverageAttestation
// ============================================================================

/// Schema for `docs/attestations/protocol-coverage-<version>.json`
/// (cross-link BR-RC-FOUNDATION.G3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolCoverageAttestation {
    pub version: String,
    /// Conformance fixtures that passed byte-for-byte.
    pub fixtures_passed: Vec<String>,
    pub fixtures_failed: Vec<String>,
    pub alt_text_a11y_test_passed: bool,
    pub cap_rejection_test_passed: bool,
    /// Feature-flag rollout state at attestation time
    /// (Hidden / OptIn / Default per BR-TERM-EMULATOR-UPLIFT.ROLLOUT).
    pub rollout_phase: RolloutPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RolloutPhase {
    /// Feature exists but isn't documented; only operators
    /// who set the flag manually use it.
    #[default]
    Hidden,
    /// Feature documented; opt-in via config.
    OptIn,
    /// On by default.
    Default,
}

impl RolloutPhase {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::OptIn => "opt_in",
            Self::Default => "default",
        }
    }
}

impl ProtocolCoverageAttestation {
    /// Whether the attestation passes the bead's release-bar
    /// ("3 conformance fixtures pass byte-for-byte" + alt-text
    /// a11y test + cap-rejection test).
    #[must_use]
    pub fn meets_release_bar(&self) -> bool {
        self.fixtures_failed.is_empty()
            && self.fixtures_passed.len() >= 3
            && self.alt_text_a11y_test_passed
            && self.cap_rejection_test_passed
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KittyAltTextTelemetry {
    pub alt_text_explicit_x: u64,
    pub alt_text_filename_fallback: u64,
    pub alt_text_missing: u64,
    pub alt_text_truncated: u64,
    pub alt_text_control_chars_scrubbed: u64,
    pub alt_text_whitespace_collapsed: u64,
    pub accessibility_alerts_emitted: u64,
}

impl KittyAltTextTelemetry {
    pub fn record_resolution(&mut self, source: AltTextSource) {
        match source {
            AltTextSource::ExplicitX => {
                self.alt_text_explicit_x = self.alt_text_explicit_x.saturating_add(1);
            }
            AltTextSource::FilenameFallback => {
                self.alt_text_filename_fallback =
                    self.alt_text_filename_fallback.saturating_add(1);
            }
            AltTextSource::None => {
                self.alt_text_missing = self.alt_text_missing.saturating_add(1);
            }
            AltTextSource::Sanitised { from: SanitisedSource::ExplicitX } => {
                self.alt_text_explicit_x = self.alt_text_explicit_x.saturating_add(1);
            }
            AltTextSource::Sanitised { from: SanitisedSource::FilenameFallback } => {
                self.alt_text_filename_fallback =
                    self.alt_text_filename_fallback.saturating_add(1);
            }
        }
    }

    pub fn record_sanitization(&mut self, outcome: &SanitizationOutcome) {
        if outcome.truncated {
            self.alt_text_truncated = self.alt_text_truncated.saturating_add(1);
        }
        self.alt_text_control_chars_scrubbed = self
            .alt_text_control_chars_scrubbed
            .saturating_add(outcome.control_chars_scrubbed as u64);
        self.alt_text_whitespace_collapsed = self
            .alt_text_whitespace_collapsed
            .saturating_add(outcome.whitespace_collapsed as u64);
    }

    pub fn record_alert_emitted(&mut self) {
        self.accessibility_alerts_emitted =
            self.accessibility_alerts_emitted.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // AltTextSource
    // ----------------------------------------------------------------

    #[test]
    fn source_announceable() {
        assert!(AltTextSource::ExplicitX.announceable());
        assert!(AltTextSource::FilenameFallback.announceable());
        assert!(!AltTextSource::None.announceable());
        let s = AltTextSource::Sanitised { from: SanitisedSource::ExplicitX };
        assert!(s.announceable());
    }

    #[test]
    fn source_label_stable() {
        assert_eq!(AltTextSource::ExplicitX.label(), "explicit_x");
        assert_eq!(AltTextSource::FilenameFallback.label(), "filename_fallback");
        assert_eq!(AltTextSource::None.label(), "none");
        assert_eq!(
            AltTextSource::Sanitised { from: SanitisedSource::ExplicitX }.label(),
            "sanitised_explicit_x",
        );
    }

    // ----------------------------------------------------------------
    // resolve_alt_text — priority
    // ----------------------------------------------------------------

    #[test]
    fn resolve_explicit_x_wins() {
        let r = resolve_alt_text(Some("a chart"), Some("chart.png"));
        assert_eq!(r.source, AltTextSource::ExplicitX);
        assert_eq!(r.text.as_deref(), Some("a chart"));
    }

    #[test]
    fn resolve_filename_fallback_when_no_x() {
        let r = resolve_alt_text(None, Some("chart.png"));
        assert_eq!(r.source, AltTextSource::FilenameFallback);
        assert_eq!(r.text.as_deref(), Some("chart.png"));
    }

    #[test]
    fn resolve_empty_x_falls_back_to_filename() {
        // X= present but empty (decode of empty base64) — treat
        // as missing per the bead's intent.
        let r = resolve_alt_text(Some(""), Some("chart.png"));
        assert_eq!(r.source, AltTextSource::FilenameFallback);
    }

    #[test]
    fn resolve_no_sources_missing() {
        let r = resolve_alt_text(None, None);
        assert_eq!(r.source, AltTextSource::None);
        assert!(r.text.is_none());
    }

    #[test]
    fn resolve_empty_filename_missing() {
        let r = resolve_alt_text(None, Some(""));
        assert_eq!(r.source, AltTextSource::None);
    }

    // ----------------------------------------------------------------
    // sanitize_alt_text
    // ----------------------------------------------------------------

    #[test]
    fn sanitize_clean_input_no_change() {
        let cfg = AltTextSanitizerConfig::default();
        let out = sanitize_alt_text("a chart of bananas", cfg);
        assert!(!out.modified);
        assert!(!out.truncated);
        assert_eq!(out.sanitised_text, "a chart of bananas");
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let cfg = AltTextSanitizerConfig::default();
        let out = sanitize_alt_text("hello\x07world\x1b", cfg);
        assert!(out.modified);
        assert_eq!(out.control_chars_scrubbed, 2);
        assert_eq!(out.sanitised_text, "helloworld");
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        let cfg = AltTextSanitizerConfig::default();
        let out = sanitize_alt_text("a   chart   of   bananas", cfg);
        assert!(out.modified);
        assert!(out.whitespace_collapsed > 0);
        assert_eq!(out.sanitised_text, "a chart of bananas");
    }

    #[test]
    fn sanitize_truncates_long_input() {
        let cfg = AltTextSanitizerConfig {
            max_chars: 10,
            ..AltTextSanitizerConfig::default()
        };
        let out = sanitize_alt_text("0123456789ABCDEF", cfg);
        assert!(out.truncated);
        assert_eq!(out.sanitised_text.chars().count(), 10);
        assert_eq!(out.sanitised_text, "0123456789");
    }

    #[test]
    fn sanitize_handles_unicode_safely_at_boundary() {
        // 5 multi-byte CJK chars + 1 ASCII; cap at 4 chars.
        let cfg = AltTextSanitizerConfig {
            max_chars: 4,
            ..AltTextSanitizerConfig::default()
        };
        let out = sanitize_alt_text("猫犬鳥魚A", cfg);
        assert!(out.truncated);
        assert_eq!(out.sanitised_text.chars().count(), 4);
        assert_eq!(out.sanitised_text, "猫犬鳥魚");
    }

    #[test]
    fn sanitize_tab_becomes_space() {
        let cfg = AltTextSanitizerConfig::default();
        let out = sanitize_alt_text("hello\tworld", cfg);
        assert_eq!(out.sanitised_text, "hello world");
    }

    #[test]
    fn sanitize_leading_trailing_whitespace_trimmed() {
        let cfg = AltTextSanitizerConfig::default();
        let out = sanitize_alt_text("  spacious  ", cfg);
        // Leading collapsed to single space; trailing trimmed.
        assert_eq!(out.sanitised_text, " spacious");
    }

    #[test]
    fn sanitize_at_exact_max_chars_no_truncation() {
        let cfg = AltTextSanitizerConfig {
            max_chars: 5,
            ..AltTextSanitizerConfig::default()
        };
        let out = sanitize_alt_text("hello", cfg);
        assert!(!out.truncated);
    }

    // ----------------------------------------------------------------
    // resolve_and_sanitize
    // ----------------------------------------------------------------

    #[test]
    fn resolve_sanitize_clean_keeps_provenance() {
        let cfg = AltTextSanitizerConfig::default();
        let r = resolve_and_sanitize(Some("clean alt text"), None, cfg);
        assert_eq!(r.source, AltTextSource::ExplicitX);
        assert_eq!(r.text.as_deref(), Some("clean alt text"));
    }

    #[test]
    fn resolve_sanitize_modified_upgrades_to_sanitised() {
        let cfg = AltTextSanitizerConfig::default();
        let r = resolve_and_sanitize(Some("a\x07chart"), None, cfg);
        assert_eq!(
            r.source,
            AltTextSource::Sanitised { from: SanitisedSource::ExplicitX },
        );
        assert_eq!(r.text.as_deref(), Some("achart"));
    }

    #[test]
    fn resolve_sanitize_filename_fallback_modified() {
        let cfg = AltTextSanitizerConfig {
            max_chars: 5,
            ..AltTextSanitizerConfig::default()
        };
        let r = resolve_and_sanitize(None, Some("verylongfilename.png"), cfg);
        assert_eq!(
            r.source,
            AltTextSource::Sanitised { from: SanitisedSource::FilenameFallback },
        );
    }

    #[test]
    fn resolve_sanitize_missing_stays_missing() {
        let cfg = AltTextSanitizerConfig::default();
        let r = resolve_and_sanitize(None, None, cfg);
        assert_eq!(r.source, AltTextSource::None);
        assert!(r.text.is_none());
    }

    // ----------------------------------------------------------------
    // ProtocolCoverageAttestation — release-bar predicate
    // ----------------------------------------------------------------

    #[test]
    fn attestation_passes_with_3_fixtures() {
        let a = ProtocolCoverageAttestation {
            version: "0.5.0".to_string(),
            fixtures_passed: vec![
                "image_nvim".to_string(),
                "yazi".to_string(),
                "icat".to_string(),
            ],
            fixtures_failed: vec![],
            alt_text_a11y_test_passed: true,
            cap_rejection_test_passed: true,
            rollout_phase: RolloutPhase::OptIn,
        };
        assert!(a.meets_release_bar());
    }

    #[test]
    fn attestation_fails_with_under_3_fixtures() {
        let a = ProtocolCoverageAttestation {
            version: "0.5.0".to_string(),
            fixtures_passed: vec!["image_nvim".to_string(), "yazi".to_string()],
            fixtures_failed: vec![],
            alt_text_a11y_test_passed: true,
            cap_rejection_test_passed: true,
            rollout_phase: RolloutPhase::OptIn,
        };
        assert!(!a.meets_release_bar());
    }

    #[test]
    fn attestation_fails_when_any_fixture_failed() {
        let a = ProtocolCoverageAttestation {
            version: "0.5.0".to_string(),
            fixtures_passed: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            fixtures_failed: vec!["d".to_string()],
            alt_text_a11y_test_passed: true,
            cap_rejection_test_passed: true,
            rollout_phase: RolloutPhase::OptIn,
        };
        assert!(!a.meets_release_bar());
    }

    #[test]
    fn attestation_fails_without_a11y_test() {
        let a = ProtocolCoverageAttestation {
            version: "0.5.0".to_string(),
            fixtures_passed: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            fixtures_failed: vec![],
            alt_text_a11y_test_passed: false,
            cap_rejection_test_passed: true,
            rollout_phase: RolloutPhase::OptIn,
        };
        assert!(!a.meets_release_bar());
    }

    #[test]
    fn attestation_fails_without_cap_rejection_test() {
        let a = ProtocolCoverageAttestation {
            version: "0.5.0".to_string(),
            fixtures_passed: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            fixtures_failed: vec![],
            alt_text_a11y_test_passed: true,
            cap_rejection_test_passed: false,
            rollout_phase: RolloutPhase::OptIn,
        };
        assert!(!a.meets_release_bar());
    }

    #[test]
    fn rollout_phase_label_stable() {
        assert_eq!(RolloutPhase::Hidden.label(), "hidden");
        assert_eq!(RolloutPhase::OptIn.label(), "opt_in");
        assert_eq!(RolloutPhase::Default.label(), "default");
    }

    // ----------------------------------------------------------------
    // KittyAltTextTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = KittyAltTextTelemetry::default();
        assert_eq!(t.alt_text_explicit_x, 0);
        assert_eq!(t.accessibility_alerts_emitted, 0);
    }

    #[test]
    fn telemetry_record_resolution_routes() {
        let mut t = KittyAltTextTelemetry::default();
        t.record_resolution(AltTextSource::ExplicitX);
        t.record_resolution(AltTextSource::FilenameFallback);
        t.record_resolution(AltTextSource::None);
        t.record_resolution(AltTextSource::Sanitised {
            from: SanitisedSource::ExplicitX,
        });
        assert_eq!(t.alt_text_explicit_x, 2); // explicit + sanitised-explicit
        assert_eq!(t.alt_text_filename_fallback, 1);
        assert_eq!(t.alt_text_missing, 1);
    }

    #[test]
    fn telemetry_record_sanitization_routes() {
        let mut t = KittyAltTextTelemetry::default();
        let outcome = SanitizationOutcome {
            sanitised_text: "x".to_string(),
            modified: true,
            truncated: true,
            control_chars_scrubbed: 5,
            whitespace_collapsed: 3,
        };
        t.record_sanitization(&outcome);
        assert_eq!(t.alt_text_truncated, 1);
        assert_eq!(t.alt_text_control_chars_scrubbed, 5);
        assert_eq!(t.alt_text_whitespace_collapsed, 3);
    }

    #[test]
    fn telemetry_record_alert_emitted_increments() {
        let mut t = KittyAltTextTelemetry::default();
        t.record_alert_emitted();
        t.record_alert_emitted();
        assert_eq!(t.accessibility_alerts_emitted, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_image_nvim_chart_with_alt_text() {
        // image.nvim emits a chart with X=base64-decoded
        // "Sales Q3 2026 line chart"; ft sanitises
        // (clean, no change) and emits an Alert.
        let cfg = AltTextSanitizerConfig::default();
        let r = resolve_and_sanitize(
            Some("Sales Q3 2026 line chart"),
            Some("chart.png"),
            cfg,
        );
        assert_eq!(r.source, AltTextSource::ExplicitX);
        assert_eq!(r.text.as_deref(), Some("Sales Q3 2026 line chart"));

        let alert = AccessibilityAlert {
            image_id: 42,
            text: r.text.unwrap(),
            source: r.source,
            pane_id: 1,
        };
        let mut telem = KittyAltTextTelemetry::default();
        telem.record_resolution(alert.source);
        telem.record_alert_emitted();
        assert_eq!(telem.accessibility_alerts_emitted, 1);
    }

    #[test]
    fn scenario_yazi_thumbnail_filename_fallback() {
        // yazi previews `cute_cat.png` without an X= field.
        // Substrate falls back to the filename.
        let cfg = AltTextSanitizerConfig::default();
        let r = resolve_and_sanitize(None, Some("cute_cat.png"), cfg);
        assert_eq!(r.source, AltTextSource::FilenameFallback);
        assert_eq!(r.text.as_deref(), Some("cute_cat.png"));
    }

    #[test]
    fn scenario_adversarial_alt_text_sanitised() {
        // Malicious actor embeds escape codes in alt-text to
        // trip a screen reader. Substrate scrubs.
        let cfg = AltTextSanitizerConfig::default();
        let r = resolve_and_sanitize(
            Some("benign\x1b[31malert\x07evil"),
            Some("img.png"),
            cfg,
        );
        assert_eq!(
            r.source,
            AltTextSource::Sanitised { from: SanitisedSource::ExplicitX },
        );
        let text = r.text.unwrap();
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(text.contains("benign"));
    }

    #[test]
    fn scenario_release_attestation_full_pass() {
        let a = ProtocolCoverageAttestation {
            version: "0.5.0".to_string(),
            fixtures_passed: vec![
                "image_nvim".to_string(),
                "yazi".to_string(),
                "icat".to_string(),
            ],
            fixtures_failed: vec![],
            alt_text_a11y_test_passed: true,
            cap_rejection_test_passed: true,
            rollout_phase: RolloutPhase::OptIn,
        };
        assert!(a.meets_release_bar());
    }
}
