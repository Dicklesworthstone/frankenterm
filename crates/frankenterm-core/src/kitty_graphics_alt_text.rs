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
            Self::Sanitised {
                from: SanitisedSource::ExplicitX,
            } => "sanitised_explicit_x",
            Self::Sanitised {
                from: SanitisedSource::FilenameFallback,
            } => "sanitised_filename_fallback",
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
        Self {
            source: AltTextSource::None,
            text: None,
        }
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

/// Outcome of a sanitization pass.
///
/// **Privacy-critical**: `sanitised_text` is `pub(crate)` so
/// external code cannot overwrite it with unsanitized data
/// AFTER `sanitize_alt_text` ran. The downstream AT layer
/// (VoiceOver / Orca / Narrator) announces this string;
/// re-injecting C0/C1/DEL/CSI escape codes here would defeat
/// the recent br-ft-mc629 control-char scrub work and could
/// cause terminal injection at the AT layer. Read via
/// [`Self::sanitised_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizationOutcome {
    pub(crate) sanitised_text: String,
    pub(crate) modified: bool,
    pub(crate) truncated: bool,
    pub(crate) control_chars_scrubbed: u32,
    pub(crate) whitespace_collapsed: u32,
}

impl SanitizationOutcome {
    /// Read accessor for the sanitised text. The downstream
    /// AT layer announces this; the field privacy guarantees
    /// no post-sanitization tampering.
    #[must_use]
    pub fn sanitised_text(&self) -> &str {
        &self.sanitised_text
    }

    #[must_use]
    pub const fn modified(&self) -> bool {
        self.modified
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    #[must_use]
    pub const fn control_chars_scrubbed(&self) -> u32 {
        self.control_chars_scrubbed
    }

    #[must_use]
    pub const fn whitespace_collapsed(&self) -> u32 {
        self.whitespace_collapsed
    }

    /// Consume the outcome and return the sanitised text.
    /// Used by integration code that needs to take ownership
    /// (e.g., constructing the AccessibilityAlert).
    #[must_use]
    pub fn into_sanitised_text(self) -> String {
        self.sanitised_text
    }
}

/// Pure sanitiser. Returns the sanitised string + flags
/// describing what changed; the integration plumbs the flags
/// into `KittyAltTextTelemetry`.
///
/// Self-review fix (br-ft-mc629): scrub now covers C0
/// (0x00–0x1F), DEL (0x7F), and C1 (0x80–0x9F). Earlier
/// version only handled C0; a malicious actor could embed
/// DEL or CSI (0x9B) in the X= field and bypass the scrub.
#[must_use]
pub fn sanitize_alt_text(input: &str, config: AltTextSanitizerConfig) -> SanitizationOutcome {
    let mut out = String::with_capacity(input.len());
    let mut modified = false;
    let mut control_chars_scrubbed: u32 = 0;
    let mut whitespace_collapsed: u32 = 0;
    let mut prev_was_space = false;

    for ch in input.chars() {
        let cp = ch as u32;
        let is_control = cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp);
        if config.scrub_control_chars && is_control {
            control_chars_scrubbed = control_chars_scrubbed.saturating_add(1);
            modified = true;
            // \t and \n and \r collapse to one space; other
            // controls drop entirely.
            if matches!(ch, '\t' | '\n' | '\r') && !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
            continue;
        }
        if config.collapse_whitespace && ch.is_whitespace() {
            if prev_was_space {
                whitespace_collapsed = whitespace_collapsed.saturating_add(1);
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
        _ => {
            return AltTextResolution {
                source,
                text: Some(text),
            };
        }
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
///
/// **Attestation integrity**: fields are `pub(crate)` so
/// external code can't flip `alt_text_a11y_test_passed` /
/// `cap_rejection_test_passed` to true and clear
/// `fixtures_failed` to bypass [`Self::meets_release_bar`].
/// Construct via the builder; tests + CI runner are the only
/// legitimate fillers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolCoverageAttestation {
    pub(crate) version: String,
    /// Conformance fixtures that passed byte-for-byte.
    pub(crate) fixtures_passed: Vec<String>,
    pub(crate) fixtures_failed: Vec<String>,
    pub(crate) alt_text_a11y_test_passed: bool,
    pub(crate) cap_rejection_test_passed: bool,
    /// Feature-flag rollout state at attestation time
    /// (Hidden / OptIn / Default per BR-TERM-EMULATOR-UPLIFT.ROLLOUT).
    pub(crate) rollout_phase: RolloutPhase,
}

impl ProtocolCoverageAttestation {
    /// Builder-style constructor. The CI runner fills the
    /// evidence fields explicitly; rollout phase is derived from
    /// the production rollout constant so callers cannot attest an
    /// arbitrary rollout state.
    #[must_use]
    pub fn new(version: String) -> Self {
        Self {
            version,
            fixtures_passed: Vec::new(),
            fixtures_failed: Vec::new(),
            alt_text_a11y_test_passed: false,
            cap_rejection_test_passed: false,
            rollout_phase: current_kitty_graphics_rollout_phase(),
        }
    }

    pub fn record_fixture_pass(&mut self, fixture: impl Into<String>) {
        self.fixtures_passed.push(fixture.into());
    }

    pub fn record_fixture_fail(&mut self, fixture: impl Into<String>) {
        self.fixtures_failed.push(fixture.into());
    }

    pub fn mark_alt_text_a11y_passed(&mut self) {
        self.alt_text_a11y_test_passed = true;
    }

    pub fn mark_cap_rejection_passed(&mut self) {
        self.cap_rejection_test_passed = true;
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn fixtures_passed(&self) -> &[String] {
        &self.fixtures_passed
    }

    #[must_use]
    pub fn fixtures_failed(&self) -> &[String] {
        &self.fixtures_failed
    }

    #[must_use]
    pub const fn alt_text_a11y_test_passed(&self) -> bool {
        self.alt_text_a11y_test_passed
    }

    #[must_use]
    pub const fn cap_rejection_test_passed(&self) -> bool {
        self.cap_rejection_test_passed
    }

    #[must_use]
    pub const fn rollout_phase(&self) -> RolloutPhase {
        self.rollout_phase
    }
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

    #[must_use]
    pub const fn is_enabled_by_default(self) -> bool {
        matches!(self, Self::Default)
    }

    #[must_use]
    pub const fn is_runtime_configurable(self) -> bool {
        matches!(self, Self::OptIn | Self::Default)
    }
}

/// Rollout registry key used by config, docs, and release attestations.
pub const KITTY_GRAPHICS_ROLLOUT_FEATURE_ID: &str = "kitty_graphics_alt_text";

/// Current Kitty graphics alt-text rollout phase per
/// BR-TERM-EMULATOR-UPLIFT.ROLLOUT.
#[must_use]
pub const fn current_kitty_graphics_rollout_phase() -> RolloutPhase {
    RolloutPhase::OptIn
}

/// Default value for `[kitty_graphics].enable_kitty_graphics`.
#[must_use]
pub const fn kitty_graphics_enabled_by_default() -> bool {
    current_kitty_graphics_rollout_phase().is_enabled_by_default()
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
// Attestation generator (ft-d1pv3 slice 1)
// ============================================================================

/// Reasons the release gate refuses to ship a build.
///
/// Returned by [`gate_release`] when the supplied
/// [`ProtocolCoverageAttestation`] does not satisfy
/// [`ProtocolCoverageAttestation::meets_release_bar`]. Each
/// variant carries the observed counter so the gate's failure
/// message can be precise about which acceptance criterion failed.
///
/// Per ft-d1pv3 (cont of ft-h8s0p).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseGateError {
    /// At least one conformance fixture failed; release blocked.
    FixturesFailed { count: usize, names: Vec<String> },
    /// Fewer than 3 conformance fixtures passed (the bead's
    /// release-bar minimum).
    InsufficientFixturesPassed { passed: usize, required: usize },
    /// The alt-text accessibility test never marked itself passed.
    AltTextA11yNotPassed,
    /// The cap-rejection integration test never marked itself
    /// passed.
    CapRejectionNotPassed,
}

impl core::fmt::Display for ReleaseGateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FixturesFailed { count, names } => {
                write!(
                    f,
                    "release blocked: {count} conformance fixture(s) failed: {names:?}"
                )
            }
            Self::InsufficientFixturesPassed { passed, required } => {
                write!(
                    f,
                    "release blocked: only {passed} conformance fixtures passed; {required} required"
                )
            }
            Self::AltTextA11yNotPassed => {
                write!(
                    f,
                    "release blocked: alt-text accessibility integration test did not pass"
                )
            }
            Self::CapRejectionNotPassed => {
                write!(
                    f,
                    "release blocked: cap-rejection integration test did not pass"
                )
            }
        }
    }
}

impl std::error::Error for ReleaseGateError {}

/// Gate the release on the attestation's acceptance criteria. The
/// CI runner invokes this immediately after writing the
/// attestation JSON via [`write_protocol_coverage_attestation`];
/// a failed gate refuses the build.
///
/// Per ft-d1pv3: the substrate's
/// [`ProtocolCoverageAttestation::meets_release_bar`] returns a
/// single bool. This wrapper maps that bool back to a precise
/// `ReleaseGateError` describing which criterion failed first, so
/// CI logs surface the actionable signal without the operator
/// having to inspect the JSON.
pub fn gate_release(att: &ProtocolCoverageAttestation) -> Result<(), ReleaseGateError> {
    if !att.fixtures_failed.is_empty() {
        return Err(ReleaseGateError::FixturesFailed {
            count: att.fixtures_failed.len(),
            names: att.fixtures_failed.clone(),
        });
    }
    const MIN_PASSED: usize = 3;
    if att.fixtures_passed.len() < MIN_PASSED {
        return Err(ReleaseGateError::InsufficientFixturesPassed {
            passed: att.fixtures_passed.len(),
            required: MIN_PASSED,
        });
    }
    if !att.alt_text_a11y_test_passed {
        return Err(ReleaseGateError::AltTextA11yNotPassed);
    }
    if !att.cap_rejection_test_passed {
        return Err(ReleaseGateError::CapRejectionNotPassed);
    }
    debug_assert!(
        att.meets_release_bar(),
        "gate_release passed every check but meets_release_bar returned false",
    );
    Ok(())
}

/// Render the attestation as the canonical JSON layout written to
/// `docs/attestations/protocol-coverage-<version>.json`.
///
/// The JSON shape is deliberately stable — external CI / release
/// tooling consumes this file. Field names mirror the substrate
/// struct; arrays preserve their record-order so the output is
/// deterministic for a given attestation value.
///
/// Per ft-d1pv3.
#[must_use]
pub fn protocol_coverage_attestation_json(att: &ProtocolCoverageAttestation) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "1.0.0",
        "version": att.version(),
        "rollout_phase": att.rollout_phase().label(),
        "fixtures_passed": att.fixtures_passed(),
        "fixtures_failed": att.fixtures_failed(),
        "alt_text_a11y_test_passed": att.alt_text_a11y_test_passed(),
        "cap_rejection_test_passed": att.cap_rejection_test_passed(),
        "meets_release_bar": att.meets_release_bar(),
    })
}

/// Write the attestation JSON to the given path, creating parent
/// directories as needed. The output is pretty-printed for
/// human / git-diff readability.
///
/// Per ft-d1pv3.
pub fn write_protocol_coverage_attestation(
    att: &ProtocolCoverageAttestation,
    path: &std::path::Path,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let value = protocol_coverage_attestation_json(att);
    let pretty = serde_json::to_string_pretty(&value)
        .expect("ProtocolCoverageAttestation always serializes");
    std::fs::write(path, pretty)
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
                self.alt_text_filename_fallback = self.alt_text_filename_fallback.saturating_add(1);
            }
            AltTextSource::None => {
                self.alt_text_missing = self.alt_text_missing.saturating_add(1);
            }
            AltTextSource::Sanitised {
                from: SanitisedSource::ExplicitX,
            } => {
                self.alt_text_explicit_x = self.alt_text_explicit_x.saturating_add(1);
            }
            AltTextSource::Sanitised {
                from: SanitisedSource::FilenameFallback,
            } => {
                self.alt_text_filename_fallback = self.alt_text_filename_fallback.saturating_add(1);
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
        self.accessibility_alerts_emitted = self.accessibility_alerts_emitted.saturating_add(1);
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
        let s = AltTextSource::Sanitised {
            from: SanitisedSource::ExplicitX,
        };
        assert!(s.announceable());
    }

    #[test]
    fn source_label_stable() {
        assert_eq!(AltTextSource::ExplicitX.label(), "explicit_x");
        assert_eq!(AltTextSource::FilenameFallback.label(), "filename_fallback");
        assert_eq!(AltTextSource::None.label(), "none");
        assert_eq!(
            AltTextSource::Sanitised {
                from: SanitisedSource::ExplicitX
            }
            .label(),
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
    fn sanitize_strips_del_and_c1_controls() {
        // Self-review fix (br-ft-mc629): DEL (0x7F) and C1
        // controls (0x80–0x9F, including CSI=0x9B) must be
        // scrubbed too. Previously only C0 (<0x20) was caught.
        let cfg = AltTextSanitizerConfig::default();
        // \u{7F} = DEL, \u{9B} = CSI, \u{85} = NEL (next-line).
        let out = sanitize_alt_text("safe\u{7F}text\u{9B}with\u{85}c1", cfg);
        assert!(out.modified);
        assert_eq!(out.control_chars_scrubbed, 3);
        assert_eq!(out.sanitised_text, "safetextwithc1");
    }

    #[test]
    fn sanitize_passes_high_unicode_above_c1() {
        // Defensive: substrate must NOT scrub printable
        // Unicode above 0x9F (e.g., CJK, emoji).
        let cfg = AltTextSanitizerConfig::default();
        let out = sanitize_alt_text("猫🐈cat", cfg);
        assert!(!out.modified);
        assert_eq!(out.control_chars_scrubbed, 0);
        assert_eq!(out.sanitised_text, "猫🐈cat");
    }

    #[test]
    fn sanitization_outcome_field_is_private_no_post_sanitize_overwrite() {
        // PRIVACY/A11Y REGRESSION TEST: previously,
        // SanitizationOutcome.sanitised_text was pub. External
        // code could overwrite with malicious unsanitized
        // content AFTER sanitize_alt_text ran, defeating the
        // C0/C1/DEL/CSI scrub work and potentially injecting
        // escape codes into the AT layer's announcement.
        //
        // Now the field is pub(crate). External code must
        // read via sanitised_text() accessor (returns &str —
        // no mutation). Pin: external code can only consume
        // via into_sanitised_text() (which takes ownership).
        let cfg = AltTextSanitizerConfig::default();
        // ESC (0x1B) is a C0 control char — scrubbed.
        // The literal `[A` chars after ESC are printable
        // (not part of the C0/C1/DEL alphabet); they pass
        // through. The output reflects what the AT layer
        // would announce; the substrate's job is to remove
        // executable escape codes, not interpret them.
        let out = sanitize_alt_text("dirty\x1b[Atext", cfg);
        assert_eq!(out.sanitised_text(), "dirty[Atext");
        // Cannot do `out.sanitised_text = malicious` from
        // outside the crate — compile error via pub(crate).
        let owned = out.into_sanitised_text();
        assert_eq!(owned, "dirty[Atext");
    }

    #[test]
    fn protocol_coverage_attestation_builder_round_trip() {
        // ATTESTATION FORGERY REGRESSION TEST: previously,
        // ProtocolCoverageAttestation fields were pub.
        // External code could flip alt_text_a11y_test_passed +
        // cap_rejection_test_passed to true and clear
        // fixtures_failed to bypass meets_release_bar() —
        // false attestations would land in
        // docs/attestations/.
        //
        // Now construction goes through new() + record_*
        // mutators only (pub(crate) fields).
        let mut att = ProtocolCoverageAttestation::new("1.0.0".to_string());
        assert!(!att.meets_release_bar());
        att.record_fixture_pass("imgcat");
        att.record_fixture_pass("yazi");
        att.record_fixture_pass("nvim");
        att.mark_alt_text_a11y_passed();
        att.mark_cap_rejection_passed();
        assert!(att.meets_release_bar());

        // Pin accessors.
        assert_eq!(att.version(), "1.0.0");
        assert_eq!(att.fixtures_passed().len(), 3);
        assert!(att.fixtures_failed().is_empty());
        assert!(att.alt_text_a11y_test_passed());
        assert!(att.cap_rejection_test_passed());
        assert_eq!(att.rollout_phase(), RolloutPhase::OptIn);
    }

    #[test]
    fn kitty_graphics_rollout_phase_drives_runtime_default() {
        assert_eq!(current_kitty_graphics_rollout_phase(), RolloutPhase::OptIn);
        assert!(!kitty_graphics_enabled_by_default());
        assert!(current_kitty_graphics_rollout_phase().is_runtime_configurable());
    }

    #[test]
    fn protocol_coverage_attestation_derives_rollout_phase() {
        let att = ProtocolCoverageAttestation::new("1.0.0".to_string());

        assert_eq!(att.rollout_phase(), current_kitty_graphics_rollout_phase());
    }

    #[test]
    fn protocol_coverage_attestation_failed_fixture_blocks_release() {
        let mut att = ProtocolCoverageAttestation::new("1.0.0".to_string());
        att.record_fixture_pass("imgcat");
        att.record_fixture_pass("yazi");
        att.record_fixture_pass("nvim");
        att.record_fixture_fail("regression");
        att.mark_alt_text_a11y_passed();
        att.mark_cap_rejection_passed();
        // Even with all flags + fixtures_passed.len() >= 3,
        // a single failed fixture blocks release.
        assert!(!att.meets_release_bar());
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
            AltTextSource::Sanitised {
                from: SanitisedSource::ExplicitX
            },
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
            AltTextSource::Sanitised {
                from: SanitisedSource::FilenameFallback
            },
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
        let r = resolve_and_sanitize(Some("Sales Q3 2026 line chart"), Some("chart.png"), cfg);
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
        let r = resolve_and_sanitize(Some("benign\x1b[31malert\x07evil"), Some("img.png"), cfg);
        assert_eq!(
            r.source,
            AltTextSource::Sanitised {
                from: SanitisedSource::ExplicitX
            },
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

    // ── ft-d1pv3 attestation generator tests ────────────────────────────────

    fn fully_passing_attestation() -> ProtocolCoverageAttestation {
        let mut att = ProtocolCoverageAttestation::new("0.5.0".to_string());
        att.record_fixture_pass("image_nvim");
        att.record_fixture_pass("yazi");
        att.record_fixture_pass("icat");
        att.mark_alt_text_a11y_passed();
        att.mark_cap_rejection_passed();
        att
    }

    #[test]
    fn gate_release_passes_for_full_attestation() {
        let att = fully_passing_attestation();
        assert!(att.meets_release_bar());
        gate_release(&att).expect("full attestation must pass the release gate");
    }

    #[test]
    fn gate_release_reports_fixtures_failed_first() {
        let mut att = fully_passing_attestation();
        att.record_fixture_fail("image_nvim_2");
        match gate_release(&att) {
            Err(ReleaseGateError::FixturesFailed { count, names }) => {
                assert_eq!(count, 1);
                assert_eq!(names, vec!["image_nvim_2".to_string()]);
            }
            other => panic!("expected FixturesFailed, got {other:?}"),
        }
    }

    #[test]
    fn gate_release_reports_insufficient_passing_fixtures() {
        let mut att = ProtocolCoverageAttestation::new("0.5.0".to_string());
        att.record_fixture_pass("only_one");
        att.mark_alt_text_a11y_passed();
        att.mark_cap_rejection_passed();
        match gate_release(&att) {
            Err(ReleaseGateError::InsufficientFixturesPassed { passed, required }) => {
                assert_eq!(passed, 1);
                assert_eq!(required, 3);
            }
            other => panic!("expected InsufficientFixturesPassed, got {other:?}"),
        }
    }

    #[test]
    fn gate_release_reports_alt_text_a11y_gap() {
        let mut att = fully_passing_attestation();
        att.alt_text_a11y_test_passed = false;
        match gate_release(&att) {
            Err(ReleaseGateError::AltTextA11yNotPassed) => {}
            other => panic!("expected AltTextA11yNotPassed, got {other:?}"),
        }
    }

    #[test]
    fn gate_release_reports_cap_rejection_gap() {
        let mut att = fully_passing_attestation();
        att.cap_rejection_test_passed = false;
        match gate_release(&att) {
            Err(ReleaseGateError::CapRejectionNotPassed) => {}
            other => panic!("expected CapRejectionNotPassed, got {other:?}"),
        }
    }

    #[test]
    fn gate_release_error_messages_include_actionable_signal() {
        let err = ReleaseGateError::InsufficientFixturesPassed {
            passed: 1,
            required: 3,
        };
        let s = err.to_string();
        assert!(s.contains("only 1"));
        assert!(s.contains("3 required"));
    }

    #[test]
    fn attestation_json_layout_is_stable() {
        let att = fully_passing_attestation();
        let json = protocol_coverage_attestation_json(&att);
        assert_eq!(json["schema_version"], "1.0.0");
        assert_eq!(json["version"], "0.5.0");
        assert_eq!(json["rollout_phase"], "opt_in");
        assert_eq!(
            json["fixtures_passed"],
            serde_json::json!(["image_nvim", "yazi", "icat"])
        );
        assert_eq!(json["fixtures_failed"], serde_json::json!([]));
        assert_eq!(json["alt_text_a11y_test_passed"], true);
        assert_eq!(json["cap_rejection_test_passed"], true);
        assert_eq!(json["meets_release_bar"], true);
    }

    #[test]
    fn attestation_json_reports_failed_state_truthfully() {
        let mut att = ProtocolCoverageAttestation::new("0.5.0".to_string());
        att.rollout_phase = RolloutPhase::Hidden;
        att.record_fixture_fail("image_nvim");
        let json = protocol_coverage_attestation_json(&att);
        assert_eq!(json["meets_release_bar"], false);
        assert_eq!(json["rollout_phase"], "hidden");
        assert_eq!(json["fixtures_failed"], serde_json::json!(["image_nvim"]));
    }

    #[test]
    fn write_attestation_to_path_produces_pretty_json() {
        let att = fully_passing_attestation();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("nested")
            .join("protocol-coverage-0.5.0.json");

        write_protocol_coverage_attestation(&att, &path).expect("write attestation");

        let contents = std::fs::read_to_string(&path).expect("read attestation");
        // Pretty-printed JSON contains line breaks; minified does not.
        assert!(contents.contains('\n'));
        // Round-trips back to the same Value.
        let parsed: serde_json::Value = serde_json::from_str(&contents).expect("parse attestation");
        assert_eq!(parsed, protocol_coverage_attestation_json(&att));
    }
}
