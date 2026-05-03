//! Handoff capsule inspector — renders a [`HandoffCapsule`] for operator
//! triage WITHOUT applying any sections to a destination pane. [ft-yk9lp]
//!
//! This is slice 1 of the `ft handoff` CLI surface tracked under
//! ft-yk9lp. The CLI subcommand `ft handoff inspect <capsule-path>` will
//! load a capsule from disk, deserialize via serde, and render via this
//! module's [`CapsuleInspector`]. The export and import subcommands are
//! deferred to follow-up beads (filed as ft-yk9lp slice 2 / slice 3) —
//! both require live mux-server context (current-pane state for export,
//! destination passport store for import) which extends beyond the
//! pure-function scope of this slice.
//!
//! # Privacy invariants
//!
//! Inspection MUST NOT echo section payload bytes — operators inspecting
//! a capsule received over an untrusted channel should be able to see the
//! capsule's *shape* (which sections it claims to carry, what
//! capabilities they require, whether the integrity hash recomputes
//! cleanly) without leaking any secret content the source operator
//! placed inside `MissionState.payload` or similar.
//!
//! The renderer enforces this by NEVER serializing
//! [`CapsuleSection`] payloads — only the section's stable label
//! ([`CapsuleSection::label`]), its declared required capabilities
//! ([`CapsuleSection::required_capabilities`]), and a payload-size
//! summary in bytes for sizing context. The fail-closed canary test at
//! the bottom of this file plants a synthetic credential in a section
//! payload and asserts neither the text nor JSON rendering contains it.
//!
//! # Integrity reporting
//!
//! [`CapsuleInspector::inspect_text`] and
//! [`CapsuleInspector::inspect`] both run
//! [`HandoffCapsule::verify_integrity`] and surface the result as part
//! of the rendering. A capsule whose stored digest doesn't match the
//! recomputed digest gets a clear `integrity: tampered` marker so an
//! operator can see at a glance that the capsule was modified after
//! signing.

use serde::{Deserialize, Serialize};

use crate::capability_passport::{CapabilityClass, CapabilityPassport};
use crate::handoff_capsule::{
    CapsuleSection, CapsuleValidationError, HandoffCapsule, SkipReason, ValidationOutcome,
};

/// Default freshness window for the inspector's `signed_at` warning,
/// in milliseconds. Capsules signed more than this many ms ago are
/// flagged with a `stale` marker — operators receiving an old capsule
/// over an async handoff channel should think twice before applying.
///
/// 24 hours by default — long enough to handle overnight hand-offs
/// across timezones, short enough that a multi-day-old capsule shouts
/// at the operator.
pub const DEFAULT_INSPECT_FRESHNESS_MS: u64 = 24 * 60 * 60 * 1000;

/// Renders a [`HandoffCapsule`] for operator triage.
///
/// See module docs for the privacy invariant — payload bytes are
/// NEVER rendered.
pub struct CapsuleInspector {
    now_ms: u64,
    freshness_window_ms: u64,
}

impl CapsuleInspector {
    /// Create an inspector evaluating freshness relative to `now_ms`.
    #[must_use]
    pub const fn at(now_ms: u64) -> Self {
        Self {
            now_ms,
            freshness_window_ms: DEFAULT_INSPECT_FRESHNESS_MS,
        }
    }

    /// Override the freshness window. Capsules signed earlier than
    /// `now_ms - window` are reported as `stale`.
    #[must_use]
    pub const fn with_freshness_window_ms(mut self, window_ms: u64) -> Self {
        self.freshness_window_ms = window_ms;
        self
    }

    /// Render a tabular plain-text inspection of `capsule`.
    ///
    /// Format: header line with capsule version + endpoints + integrity
    /// + freshness; one line per section showing index, label,
    /// required-capability count, and payload byte size.
    #[must_use]
    pub fn inspect_text(&self, capsule: &HandoffCapsule) -> String {
        let inspection = self.inspect(capsule);
        let mut out = String::new();
        out.push_str(&format!(
            "capsule v{} signed_at={}ms ({}) integrity={}\n",
            inspection.version,
            inspection.signed_at_ms,
            inspection.freshness,
            inspection.integrity_status,
        ));
        out.push_str(&format!(
            "  source: agent={} pane={} label={}\n",
            inspection.source.agent_id,
            inspection
                .source
                .pane_id
                .map_or_else(|| "∅".to_string(), |p| p.to_string()),
            inspection.source.label.as_deref().unwrap_or("∅"),
        ));
        out.push_str(&format!(
            "  destination: agent={} pane={} label={}\n",
            inspection.destination.agent_id,
            inspection
                .destination
                .pane_id
                .map_or_else(|| "∅".to_string(), |p| p.to_string()),
            inspection.destination.label.as_deref().unwrap_or("∅"),
        ));
        if inspection.sections.is_empty() {
            out.push_str("  (no sections)\n");
        } else {
            out.push_str("  sections:\n");
            for s in &inspection.sections {
                out.push_str(&format!(
                    "    [{}] {} requires={} payload_bytes={}\n",
                    s.index,
                    s.label,
                    s.required_capabilities.len(),
                    s.payload_bytes,
                ));
            }
        }
        out
    }

    /// Render a structured JSON-serializable inspection of `capsule`.
    ///
    /// Suitable for machine consumption (filtering, alerting,
    /// dashboarding). Same privacy invariant as
    /// [`Self::inspect_text`] — no payload bytes.
    #[must_use]
    pub fn inspect(&self, capsule: &HandoffCapsule) -> CapsuleInspection {
        let integrity_status = match capsule.verify_integrity() {
            Ok(()) => IntegrityStatus::Valid,
            Err(_) => IntegrityStatus::Tampered,
        };
        let freshness = if capsule.signed_at_ms == 0 {
            FreshnessStatus::Unknown
        } else if self.now_ms < capsule.signed_at_ms {
            // Future-dated capsule — clock skew. Treat as fresh-but-suspicious.
            FreshnessStatus::FutureDated
        } else if self.now_ms.saturating_sub(capsule.signed_at_ms) > self.freshness_window_ms {
            FreshnessStatus::Stale
        } else {
            FreshnessStatus::Fresh
        };
        let sections = capsule
            .sections
            .iter()
            .enumerate()
            .map(|(idx, section)| SectionInspection::from_section(idx, section))
            .collect();
        CapsuleInspection {
            version: capsule.version,
            source: EndpointInspection::from_endpoint(&capsule.source),
            destination: EndpointInspection::from_endpoint(&capsule.destination),
            sections,
            integrity_status,
            freshness,
            signed_at_ms: capsule.signed_at_ms,
            evaluated_at_ms: self.now_ms,
            freshness_window_ms: self.freshness_window_ms,
        }
    }
}

/// Error produced when [`load_and_inspect_capsule`] cannot complete.
/// Distinguishes I/O failures from JSON deserialization failures so
/// the CLI can present operator-actionable messages without leaking
/// raw error chains.
#[derive(Debug)]
pub enum CapsuleInspectError {
    /// Reading the capsule file failed (file not found, permission
    /// denied, I/O error mid-read). The wrapped path is the file
    /// the caller asked for; the wrapped `io::Error` carries the
    /// underlying reason.
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// The bytes loaded from disk did not parse as a valid
    /// [`HandoffCapsule`] JSON document.
    Decode {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for CapsuleInspectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => write!(
                f,
                "failed to read handoff capsule from {}: {source}",
                path.display()
            ),
            Self::Decode { path, source } => write!(
                f,
                "failed to decode handoff capsule at {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CapsuleInspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
        }
    }
}

/// Pure-function helper for the eventual `ft handoff inspect <path>`
/// CLI subcommand (br-ft-difnz, slice 2 of ft-yk9lp). Loads a
/// capsule from disk, deserializes via serde, and returns the
/// rendered text suitable for stdout.
///
/// `now_ms` is the wall-clock used for freshness evaluation —
/// callers in production pass `SystemTime::now()` epoch-ms; tests
/// inject a deterministic value.
///
/// The function is intentionally thin so the CLI wiring at the
/// frankenterm crate's `main.rs` becomes a one-liner — minimizing
/// churn against the 60k-line CLI binary.
///
/// # Errors
///
/// Returns [`CapsuleInspectError::Read`] if the file cannot be
/// read, or [`CapsuleInspectError::Decode`] if the bytes loaded
/// from disk are not valid HandoffCapsule JSON. The renderer
/// itself cannot fail — text rendering is infallible by
/// construction.
pub fn load_and_inspect_capsule(
    path: &std::path::Path,
    now_ms: u64,
) -> Result<String, CapsuleInspectError> {
    let bytes = std::fs::read(path).map_err(|source| CapsuleInspectError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let capsule: HandoffCapsule =
        serde_json::from_slice(&bytes).map_err(|source| CapsuleInspectError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(CapsuleInspector::at(now_ms).inspect_text(&capsule))
}

/// JSON variant of [`load_and_inspect_capsule`]. Returns the
/// structured rendering serialized as JSON — suitable for
/// `ft handoff inspect <path> --format json` or audit-feed
/// consumption.
///
/// # Errors
///
/// Same error surface as [`load_and_inspect_capsule`], plus a
/// (currently unreachable) JSON-encoding error if the rendering
/// itself fails to serialize. Encoding failure is wrapped in
/// [`CapsuleInspectError::Decode`] with the input file path —
/// callers that need to distinguish encode-fail from decode-fail
/// should serialize the [`CapsuleInspection`] separately.
pub fn load_and_inspect_capsule_json(
    path: &std::path::Path,
    now_ms: u64,
) -> Result<String, CapsuleInspectError> {
    let bytes = std::fs::read(path).map_err(|source| CapsuleInspectError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let capsule: HandoffCapsule =
        serde_json::from_slice(&bytes).map_err(|source| CapsuleInspectError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    let inspection = CapsuleInspector::at(now_ms).inspect(&capsule);
    serde_json::to_string_pretty(&inspection).map_err(|source| CapsuleInspectError::Decode {
        path: path.to_path_buf(),
        source,
    })
}

// =============================================================================
// br-ft-r6kg2: capsule export + import helpers (slice 3 of ft-yk9lp)
// =============================================================================
//
// `ft handoff export <output-path>` calls `save_capsule_to_path` to
// pretty-print a freshly-built capsule to disk; `ft handoff import
// <capsule-path> [--target pane_id] [--dry-run]` calls
// `load_and_validate_capsule` to load + run `validate_for_destination`
// against the destination pane's passport (or `None` for `--dry-run`)
// and render the outcome.
//
// Both helpers are pure-function: they take only paths + already-built
// substrate values and return owned strings or errors. The CLI layer
// in main.rs feeds them with current-pane state (export) or
// destination passport store (import) at dispatch time.

/// Error produced when [`save_capsule_to_path`] cannot complete.
/// Distinguishes the three failure modes a CLI operator needs to
/// disambiguate: pre-existing destination, write failure (disk full,
/// permission denied, etc.), and JSON encoding failure (currently
/// unreachable for well-formed capsules but kept distinct for
/// future-proofing if non-serializable payloads ever appear).
#[derive(Debug)]
pub enum CapsuleSaveError {
    /// The destination path already exists. The save helper refuses
    /// to overwrite by default — the operator must supply a fresh
    /// path, or the caller must remove the existing file first.
    AlreadyExists { path: std::path::PathBuf },
    /// Encoding the capsule to JSON failed.
    Encode {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
    /// Writing the encoded capsule to disk failed mid-stream.
    Write {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for CapsuleSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists { path } => write!(
                f,
                "refusing to overwrite existing handoff capsule at {}",
                path.display()
            ),
            Self::Encode { path, source } => write!(
                f,
                "failed to encode handoff capsule for {}: {source}",
                path.display()
            ),
            Self::Write { path, source } => write!(
                f,
                "failed to write handoff capsule to {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CapsuleSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyExists { .. } => None,
            Self::Encode { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
        }
    }
}

/// Pretty-prints `capsule` as JSON and writes it to `path`. Refuses to
/// overwrite an existing file — operators who want to overwrite must
/// remove the prior file explicitly, so an export typo can't silently
/// clobber a previously-archived capsule.
///
/// Uses `OpenOptions::create_new` so the existence check + write are a
/// single atomic syscall; another process racing the same path will
/// see one success and one [`CapsuleSaveError::AlreadyExists`].
///
/// # Errors
///
/// - [`CapsuleSaveError::AlreadyExists`] when `path` is already present.
/// - [`CapsuleSaveError::Encode`] when serde_json fails to encode the
///   capsule (currently unreachable for well-formed capsules).
/// - [`CapsuleSaveError::Write`] when I/O fails after the file is
///   created — the partial file is left on disk for triage.
pub fn save_capsule_to_path(
    capsule: &HandoffCapsule,
    path: &std::path::Path,
) -> Result<(), CapsuleSaveError> {
    let bytes =
        serde_json::to_vec_pretty(capsule).map_err(|source| CapsuleSaveError::Encode {
            path: path.to_path_buf(),
            source,
        })?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                CapsuleSaveError::AlreadyExists {
                    path: path.to_path_buf(),
                }
            } else {
                CapsuleSaveError::Write {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
    std::io::Write::write_all(&mut file, &bytes).map_err(|source| CapsuleSaveError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    std::io::Write::flush(&mut file).map_err(|source| CapsuleSaveError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Error produced when [`load_and_validate_capsule`] cannot complete.
/// Wraps the three distinct failure modes: file I/O, JSON decode, and
/// substrate-side validation (integrity mismatch / unsupported version).
#[derive(Debug)]
pub enum CapsuleValidateError {
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    Decode {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
    Validation {
        path: std::path::PathBuf,
        source: CapsuleValidationError,
    },
}

impl std::fmt::Display for CapsuleValidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => write!(
                f,
                "failed to read handoff capsule from {}: {source}",
                path.display()
            ),
            Self::Decode { path, source } => write!(
                f,
                "failed to decode handoff capsule at {}: {source}",
                path.display()
            ),
            Self::Validation { path, source } => write!(
                f,
                "handoff capsule at {} failed validation: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CapsuleValidateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Validation { source, .. } => Some(source),
        }
    }
}

/// Structured rendering of [`ValidationOutcome`] for the JSON variant
/// of the import dispatch. Mirrors the substrate's outcome shape but
/// adds operator-readable summary fields a CLI consumer can pretty-
/// print without re-deriving them from `accepted` + `skipped` lengths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationOutcomeRendering {
    pub fully_accepted: bool,
    pub accepted_count: usize,
    pub skipped_count: usize,
    pub accepted_indices: Vec<usize>,
    pub skipped: Vec<SkipRendering>,
}

impl ValidationOutcomeRendering {
    fn from_outcome(outcome: &ValidationOutcome) -> Self {
        Self {
            fully_accepted: outcome.is_fully_accepted(),
            accepted_count: outcome.accepted.len(),
            skipped_count: outcome.skipped.len(),
            accepted_indices: outcome.accepted.clone(),
            skipped: outcome
                .skipped
                .iter()
                .map(SkipRendering::from_skip_reason)
                .collect(),
        }
    }
}

/// Public projection of [`SkipReason`]. Identical fields — re-exported
/// in the inspect crate so CLI consumers depending on this module's
/// rendering API don't need to import the substrate crate path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipRendering {
    pub section_index: usize,
    pub section_label: String,
    pub reason: String,
    pub unmet_capability_labels: Vec<String>,
}

impl SkipRendering {
    fn from_skip_reason(skip: &SkipReason) -> Self {
        Self {
            section_index: skip.section_index,
            section_label: skip.section_label.clone(),
            reason: skip.reason.clone(),
            unmet_capability_labels: skip.unmet.iter().map(CapabilityClass::label).collect(),
        }
    }
}

/// Render a [`ValidationOutcome`] as plain text for stdout. Header
/// line declares accepted/skipped counts; one indented line per
/// skipped section with `[idx] label: reason` and the unmet
/// capability labels. Acceptable sections are summarized by index list
/// rather than per-line — operators care most about WHY something was
/// skipped, not which apply candidates passed.
#[must_use]
pub fn render_validation_outcome_text(outcome: &ValidationOutcome) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "validation: accepted={} skipped={} fully_accepted={}\n",
        outcome.accepted.len(),
        outcome.skipped.len(),
        outcome.is_fully_accepted(),
    ));
    if !outcome.accepted.is_empty() {
        out.push_str("  accepted_indices: ");
        for (i, idx) in outcome.accepted.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{idx}"));
        }
        out.push('\n');
    }
    for skip in &outcome.skipped {
        out.push_str(&format!(
            "  [skip {}] {}: {}\n",
            skip.section_index, skip.section_label, skip.reason,
        ));
        if !skip.unmet.is_empty() {
            out.push_str("    unmet_capabilities:");
            for class in &skip.unmet {
                out.push_str(&format!(" {}", class.label()));
            }
            out.push('\n');
        }
    }
    if outcome.accepted.is_empty() && outcome.skipped.is_empty() {
        out.push_str("  (no sections)\n");
    }
    out
}

/// Pure-function helper for the eventual `ft handoff import
/// <capsule-path>` CLI subcommand. Loads a capsule from disk, runs
/// [`HandoffCapsule::validate_for_destination`] against
/// `destination_passport`, and returns the rendered text.
///
/// `destination_passport = None` corresponds to the `--dry-run` CLI
/// flag — the substrate treats absent passports as "no Verified
/// capabilities present", so every section requiring a capability
/// will be skipped. The output is a compact preview of which sections
/// the source capsule WOULD apply if a passport were available.
///
/// The text rendering is two-section: an `inspect` summary
/// (capsule shape, integrity, freshness) followed by a `validation`
/// summary (accepted/skipped indices, per-section reasons).
///
/// # Errors
///
/// - [`CapsuleValidateError::Read`] for I/O failures on the input.
/// - [`CapsuleValidateError::Decode`] for malformed JSON.
/// - [`CapsuleValidateError::Validation`] for integrity / version
///   mismatches surfaced by the substrate.
pub fn load_and_validate_capsule(
    path: &std::path::Path,
    destination_passport: Option<&CapabilityPassport>,
    now_ms: u64,
) -> Result<String, CapsuleValidateError> {
    let bytes = std::fs::read(path).map_err(|source| CapsuleValidateError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let capsule: HandoffCapsule =
        serde_json::from_slice(&bytes).map_err(|source| CapsuleValidateError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    let outcome = capsule.validate_for_destination(destination_passport).map_err(|source| {
        CapsuleValidateError::Validation {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let mut out = CapsuleInspector::at(now_ms).inspect_text(&capsule);
    out.push_str(&render_validation_outcome_text(&outcome));
    Ok(out)
}

/// JSON variant of [`load_and_validate_capsule`]. Returns a JSON
/// document with two keys — `inspection` (the [`CapsuleInspection`]
/// shape) and `validation` ([`ValidationOutcomeRendering`]) — so
/// audit consumers can index into the structured outcome without
/// re-parsing prose.
///
/// # Errors
///
/// Same as [`load_and_validate_capsule`].
pub fn load_and_validate_capsule_json(
    path: &std::path::Path,
    destination_passport: Option<&CapabilityPassport>,
    now_ms: u64,
) -> Result<String, CapsuleValidateError> {
    let bytes = std::fs::read(path).map_err(|source| CapsuleValidateError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let capsule: HandoffCapsule =
        serde_json::from_slice(&bytes).map_err(|source| CapsuleValidateError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    let outcome = capsule.validate_for_destination(destination_passport).map_err(|source| {
        CapsuleValidateError::Validation {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let inspection = CapsuleInspector::at(now_ms).inspect(&capsule);
    let document = ValidatedCapsuleDocument {
        inspection,
        validation: ValidationOutcomeRendering::from_outcome(&outcome),
    };
    serde_json::to_string_pretty(&document).map_err(|source| CapsuleValidateError::Decode {
        path: path.to_path_buf(),
        source,
    })
}

/// JSON envelope returned by [`load_and_validate_capsule_json`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedCapsuleDocument {
    pub inspection: CapsuleInspection,
    pub validation: ValidationOutcomeRendering,
}

/// Structured inspection of a [`HandoffCapsule`] suitable for JSON
/// rendering. Section payloads are deliberately absent — only labels +
/// required-capabilities + payload byte sizes appear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleInspection {
    pub version: u32,
    pub source: EndpointInspection,
    pub destination: EndpointInspection,
    pub sections: Vec<SectionInspection>,
    pub integrity_status: IntegrityStatus,
    pub freshness: FreshnessStatus,
    pub signed_at_ms: u64,
    pub evaluated_at_ms: u64,
    pub freshness_window_ms: u64,
}

/// Public projection of a capsule endpoint that NEVER carries
/// trust-decision bits — the renderer reproduces the human-readable
/// label and pane id but doesn't derive anything from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointInspection {
    pub agent_id: String,
    pub pane_id: Option<u64>,
    pub label: Option<String>,
}

impl EndpointInspection {
    fn from_endpoint(endpoint: &crate::handoff_capsule::CapsuleEndpoint) -> Self {
        Self {
            agent_id: endpoint.agent_id.clone(),
            pane_id: endpoint.pane_id,
            label: endpoint.label.clone(),
        }
    }
}

/// Summary of one section in a capsule. The `payload_bytes` field
/// records the section's serialized payload size in bytes for sizing
/// context but never the bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionInspection {
    pub index: usize,
    pub label: String,
    pub required_capabilities: Vec<CapabilityClass>,
    pub payload_bytes: u64,
}

impl SectionInspection {
    fn from_section(index: usize, section: &CapsuleSection) -> Self {
        // Compute payload-size summary by serializing the section to
        // JSON and reporting the byte length. We don't keep the
        // serialized bytes themselves — only the count. If
        // serialization fails (extremely rare for well-formed
        // sections), report 0 to avoid leaking error details.
        let payload_bytes = serde_json::to_vec(section)
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self {
            index,
            label: section.label().to_string(),
            required_capabilities: section.required_capabilities(),
            payload_bytes,
        }
    }
}

/// Result of running [`HandoffCapsule::verify_integrity`] during
/// inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityStatus {
    Valid,
    Tampered,
}

impl std::fmt::Display for IntegrityStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valid => f.write_str("valid"),
            Self::Tampered => f.write_str("tampered"),
        }
    }
}

/// Freshness verdict derived from `signed_at_ms` vs the inspector's
/// `now_ms` and `freshness_window_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessStatus {
    Fresh,
    Stale,
    FutureDated,
    Unknown,
}

impl std::fmt::Display for FreshnessStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh => f.write_str("fresh"),
            Self::Stale => f.write_str("stale"),
            Self::FutureDated => f.write_str("future_dated"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff_capsule::CapsuleEndpoint;
    use std::io::Write;

    fn endpoint(agent: &str, pane: Option<u64>) -> CapsuleEndpoint {
        CapsuleEndpoint {
            agent_id: agent.to_string(),
            pane_id: pane,
            label: None,
        }
    }

    fn capsule_with(sections: Vec<CapsuleSection>, signed_at_ms: u64) -> HandoffCapsule {
        HandoffCapsule::build(
            endpoint("source-agent", Some(1)),
            endpoint("dest-agent", Some(2)),
            sections,
            signed_at_ms,
        )
    }

    #[test]
    fn inspect_text_one_line_per_section_no_payload_bytes() {
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: "secret context here".to_string(),
                },
                CapsuleSection::VerificationChecklist {
                    items: vec!["check 1".to_string(), "check 2".to_string()],
                },
            ],
            1_000,
        );
        let inspector = CapsuleInspector::at(2_000);
        let text = inspector.inspect_text(&capsule);

        // Header line with version + integrity status.
        assert!(text.contains("capsule v1"));
        assert!(text.contains("integrity=valid"));
        // One line per section with index + label.
        assert!(text.contains("[0] context_summary"));
        assert!(text.contains("[1] verification_checklist"));
        // Privacy invariant: payload contents must NOT appear.
        assert!(!text.contains("secret context here"));
        assert!(!text.contains("check 1"));
    }

    #[test]
    fn inspect_marks_each_section_label_correctly() {
        let capsule = capsule_with(
            vec![
                CapsuleSection::MissionState {
                    payload: serde_json::json!({"placeholder": "ignored"}),
                },
                CapsuleSection::ContextSummary { text: "x".into() },
                CapsuleSection::CausalSummary {
                    claim_ids: vec!["c1".to_string()],
                },
                CapsuleSection::PendingApprovals {
                    count: 3,
                    summaries: vec!["a".to_string()],
                },
                CapsuleSection::VerificationChecklist {
                    items: vec!["v".to_string()],
                },
            ],
            1_000,
        );
        let inspector = CapsuleInspector::at(2_000);
        let inspection = inspector.inspect(&capsule);
        let labels: Vec<&str> = inspection
            .sections
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(
            labels,
            vec![
                "mission_state",
                "context_summary",
                "causal_summary",
                "pending_approvals",
                "verification_checklist",
            ]
        );
    }

    #[test]
    fn inspect_reports_required_capabilities_per_section() {
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary { text: "x".into() },
                CapsuleSection::MissionState {
                    payload: serde_json::Value::Null,
                },
            ],
            1_000,
        );
        let inspector = CapsuleInspector::at(2_000);
        let inspection = inspector.inspect(&capsule);
        // ContextSummary requires no capabilities.
        assert!(inspection.sections[0].required_capabilities.is_empty());
        // MissionState requires SafetyConstraint("inherit_mission_state").
        assert_eq!(inspection.sections[1].required_capabilities.len(), 1);
    }

    #[test]
    fn inspect_freshness_fresh_when_inside_window() {
        let capsule = capsule_with(Vec::new(), 1_000);
        let inspector = CapsuleInspector::at(2_000).with_freshness_window_ms(5_000);
        assert_eq!(
            inspector.inspect(&capsule).freshness,
            FreshnessStatus::Fresh
        );
    }

    #[test]
    fn inspect_freshness_stale_when_outside_window() {
        let capsule = capsule_with(Vec::new(), 1_000);
        let inspector = CapsuleInspector::at(10_000).with_freshness_window_ms(5_000);
        assert_eq!(
            inspector.inspect(&capsule).freshness,
            FreshnessStatus::Stale
        );
    }

    #[test]
    fn inspect_freshness_future_dated_when_clock_skew() {
        let capsule = capsule_with(Vec::new(), 10_000);
        let inspector = CapsuleInspector::at(5_000);
        assert_eq!(
            inspector.inspect(&capsule).freshness,
            FreshnessStatus::FutureDated
        );
    }

    #[test]
    fn inspect_freshness_unknown_when_signed_at_zero() {
        let capsule = capsule_with(Vec::new(), 0);
        let inspector = CapsuleInspector::at(1_000_000);
        assert_eq!(
            inspector.inspect(&capsule).freshness,
            FreshnessStatus::Unknown
        );
    }

    #[test]
    fn inspect_integrity_marks_tampered_after_section_mutation() {
        let mut capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: "original".into(),
            }],
            1_000,
        );
        // Tamper with the section AFTER build — the stored integrity
        // hash now disagrees with the recomputed hash.
        if let Some(CapsuleSection::ContextSummary { text }) = capsule.sections.get_mut(0) {
            *text = "modified".to_string();
        }
        let inspector = CapsuleInspector::at(2_000);
        let inspection = inspector.inspect(&capsule);
        assert_eq!(inspection.integrity_status, IntegrityStatus::Tampered);
        // Text rendering surfaces the tampering.
        assert!(
            inspector
                .inspect_text(&capsule)
                .contains("integrity=tampered")
        );
    }

    #[test]
    fn inspect_integrity_valid_for_pristine_capsule() {
        let capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: "x".to_string(),
            }],
            1_000,
        );
        let inspector = CapsuleInspector::at(2_000);
        let inspection = inspector.inspect(&capsule);
        assert_eq!(inspection.integrity_status, IntegrityStatus::Valid);
    }

    #[test]
    fn inspect_serde_roundtrips_inspection_struct() {
        let capsule = capsule_with(
            vec![CapsuleSection::ContextSummary { text: "x".into() }],
            1_000,
        );
        let inspector = CapsuleInspector::at(2_000);
        let inspection = inspector.inspect(&capsule);
        let json = serde_json::to_string(&inspection).unwrap();
        let parsed: CapsuleInspection = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, inspection);
    }

    #[test]
    fn inspect_empty_capsule_marker() {
        let capsule = capsule_with(Vec::new(), 1_000);
        let inspector = CapsuleInspector::at(2_000);
        assert!(inspector.inspect_text(&capsule).contains("(no sections)"));
        assert!(inspector.inspect(&capsule).sections.is_empty());
    }

    /// SEC fail-closed canary [ft-yk9lp]: a planted credential in a
    /// section payload MUST NOT appear in any rendered form. Same
    /// shape as `doctor_render_does_not_leak_planted_credential_ft_ykp2y`
    /// from capability_passport_doctor.
    #[test]
    fn inspect_does_not_leak_planted_credential_ft_yk9lp() {
        const PLANTED: &str = "ANTHROPIC_API_KEY=sk-fake-inspect-canary-1234567890ABCDEFGHIJ";
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: PLANTED.to_string(),
                },
                CapsuleSection::MissionState {
                    payload: serde_json::json!({"secret": PLANTED}),
                },
                CapsuleSection::PendingApprovals {
                    count: 1,
                    summaries: vec![PLANTED.to_string()],
                },
            ],
            1_000,
        );
        let inspector = CapsuleInspector::at(2_000);
        let text = inspector.inspect_text(&capsule);
        assert!(
            !text.contains(PLANTED),
            "br-ft-yk9lp: text rendering must not leak section payloads; got {text}"
        );
        let json = serde_json::to_string(&inspector.inspect(&capsule)).unwrap();
        assert!(
            !json.contains(PLANTED),
            "br-ft-yk9lp: JSON rendering must not leak section payloads; got {json}"
        );
        // Verify the renderer DID record byte sizes (so we know the
        // sections aren't skipped, just their contents redacted).
        let inspection = inspector.inspect(&capsule);
        for section in &inspection.sections {
            assert!(section.payload_bytes > 0);
        }
    }

    // ─── br-ft-difnz: load_and_inspect_capsule helper ───────────────
    //
    // Slice 2 of ft-yk9lp wires the `ft handoff inspect` CLI
    // subcommand. This helper is the load-from-disk + render
    // pipeline the CLI dispatch will invoke. Tests exercise the
    // file-loading path so the eventual main.rs wiring becomes a
    // one-liner.

    fn write_capsule_to_temp(capsule: &HandoffCapsule) -> tempfile::NamedTempFile {
        let json = serde_json::to_string_pretty(capsule).expect("serialize capsule");
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut tmp, json.as_bytes()).expect("write tempfile");
        tmp.flush().expect("flush tempfile");
        tmp
    }

    #[test]
    fn load_and_inspect_capsule_round_trips_text() {
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: "operator note".to_string(),
                },
                CapsuleSection::VerificationChecklist {
                    items: vec!["check 1".to_string()],
                },
            ],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let text = load_and_inspect_capsule(tmp.path(), 2_000).expect("inspect succeeds");

        // Same shape as inspect_text would produce directly.
        assert!(text.contains("capsule v1"));
        assert!(text.contains("integrity=valid"));
        assert!(text.contains("[0] context_summary"));
        assert!(text.contains("[1] verification_checklist"));
        // Privacy: payload contents must NOT appear (same canary as
        // the renderer's own test).
        assert!(!text.contains("operator note"));
        assert!(!text.contains("check 1"));
    }

    #[test]
    fn load_and_inspect_capsule_json_round_trips_inspection() {
        let capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: "x".to_string(),
            }],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let json = load_and_inspect_capsule_json(tmp.path(), 2_000).expect("inspect json succeeds");

        // The JSON re-deserializes as a CapsuleInspection.
        let parsed: CapsuleInspection = serde_json::from_str(&json).expect("parse inspection");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.evaluated_at_ms, 2_000);
        assert_eq!(parsed.sections.len(), 1);
    }

    #[test]
    fn load_and_inspect_capsule_returns_read_error_for_missing_file() {
        let path = std::path::Path::new("/tmp/ft-yk9lp-does-not-exist-canary-zzzzzzzz");
        let err = load_and_inspect_capsule(path, 1_000).expect_err("missing file should error");
        match err {
            CapsuleInspectError::Read { path: p, .. } => {
                assert_eq!(p, path);
            }
            other => panic!("expected Read error, got {other:?}"),
        }
    }

    #[test]
    fn load_and_inspect_capsule_returns_decode_error_for_garbage_json() {
        // Write non-JSON garbage to a temp file and assert the
        // helper distinguishes Decode from Read.
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut tmp, b"not a capsule, just some bytes").expect("write");
        tmp.flush().expect("flush");

        let err = load_and_inspect_capsule(tmp.path(), 1_000).expect_err("garbage should error");
        match err {
            CapsuleInspectError::Decode { path: p, .. } => {
                assert_eq!(p, tmp.path());
            }
            other => panic!("expected Decode error, got {other:?}"),
        }
    }

    #[test]
    fn load_and_inspect_capsule_error_display_includes_path() {
        // Operators reading the error message need the file path
        // so they can correct the typo. Pin that the Display impl
        // includes it.
        let path = std::path::PathBuf::from("/tmp/ft-yk9lp-display-canary");
        let read_err = CapsuleInspectError::Read {
            path: path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        let msg = format!("{read_err}");
        assert!(msg.contains("/tmp/ft-yk9lp-display-canary"));
        assert!(msg.contains("failed to read"));
    }

    /// br-ft-difnz canary: the load-and-render path must not
    /// amplify leakage versus the direct
    /// `inspect_text(&capsule)` path. Plant a credential in a
    /// section payload, write capsule to disk, load + inspect via
    /// the helper, and assert the helper's output equals the
    /// in-memory rendering byte-for-byte.
    #[test]
    fn load_and_inspect_capsule_matches_in_memory_rendering_ft_difnz() {
        const PLANTED: &str = "ANTHROPIC_API_KEY=sk-fake-difnz-canary-9876543210ZYXWVUTSRQPO";
        let capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: PLANTED.to_string(),
            }],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);

        let from_disk = load_and_inspect_capsule(tmp.path(), 2_000).unwrap();
        let in_memory = CapsuleInspector::at(2_000).inspect_text(&capsule);

        // Bit-exact equality — load path adds no observable bytes
        // beyond what the renderer would produce in-memory.
        assert_eq!(from_disk, in_memory);
        // And both paths preserve the renderer's privacy invariant.
        assert!(!from_disk.contains(PLANTED));
        assert!(!in_memory.contains(PLANTED));
    }

    // ─── br-ft-r6kg2: save_capsule_to_path + load_and_validate_capsule ─

    use crate::capability_passport::{
        CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
    };

    fn passport_with(verified_classes: Vec<CapabilityClass>) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: "destination-agent".to_string(),
            pane_id: Some(2),
            capabilities: verified_classes
                .into_iter()
                .map(|class| CapabilityEntry {
                    class,
                    verification: CapabilityVerification::Verified,
                    last_observed_at_ms: Some(1_000),
                    proof: RedactedProof::empty(),
                })
                .collect(),
            generation: 1,
            signed_at_ms: 1_000,
        }
    }

    #[test]
    fn save_capsule_to_path_round_trips_via_serde() {
        let capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: "hello".to_string(),
            }],
            1_000,
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capsule.json");
        save_capsule_to_path(&capsule, &path).expect("save succeeds");

        let bytes = std::fs::read(&path).expect("read back");
        let loaded: HandoffCapsule = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(loaded.signed_at_ms, capsule.signed_at_ms);
        assert_eq!(loaded.sections.len(), 1);
        loaded.verify_integrity().expect("integrity preserved");
    }

    #[test]
    fn save_capsule_to_path_refuses_to_overwrite() {
        let capsule = capsule_with(Vec::new(), 1_000);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capsule.json");
        save_capsule_to_path(&capsule, &path).expect("first save succeeds");

        let err = save_capsule_to_path(&capsule, &path)
            .expect_err("second save must refuse");
        match err {
            CapsuleSaveError::AlreadyExists { path: p } => assert_eq!(p, path),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn save_capsule_to_path_returns_write_error_for_unwritable_path() {
        let capsule = capsule_with(Vec::new(), 1_000);
        // A path inside a non-existent parent directory triggers a
        // Write error (NotFound on the parent).
        let path = std::path::PathBuf::from(
            "/tmp/ft-r6kg2-canary-no-such-dir/handoff/capsule.json",
        );
        let err = save_capsule_to_path(&capsule, &path).expect_err("must error");
        match err {
            CapsuleSaveError::Write { path: p, .. } => assert_eq!(p, path),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn save_capsule_error_display_includes_path() {
        let path = std::path::PathBuf::from("/tmp/ft-r6kg2-display-canary");
        let err = CapsuleSaveError::AlreadyExists { path: path.clone() };
        let msg = format!("{err}");
        assert!(msg.contains("/tmp/ft-r6kg2-display-canary"));
        assert!(msg.contains("refusing to overwrite"));
    }

    #[test]
    fn load_and_validate_capsule_fully_accepts_safe_only_capsule() {
        // ContextSummary + VerificationChecklist require no
        // capabilities — destination passport is irrelevant. Pass
        // None to mimic --dry-run.
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: "x".to_string(),
                },
                CapsuleSection::VerificationChecklist {
                    items: vec!["c".to_string()],
                },
            ],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let text = load_and_validate_capsule(tmp.path(), None, 2_000)
            .expect("validation succeeds");
        assert!(text.contains("validation: accepted=2 skipped=0 fully_accepted=true"));
        assert!(text.contains("accepted_indices: 0, 1"));
        // Capsule shape header still present from inspect_text prefix.
        assert!(text.contains("capsule v1"));
        assert!(text.contains("integrity=valid"));
    }

    #[test]
    fn load_and_validate_capsule_skips_sections_when_passport_missing() {
        // MissionState requires SafetyConstraint("inherit_mission_state").
        // Passport=None should skip it with a clear reason.
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: "x".to_string(),
                },
                CapsuleSection::MissionState {
                    payload: serde_json::Value::Null,
                },
            ],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let text = load_and_validate_capsule(tmp.path(), None, 2_000).expect("validates");
        assert!(text.contains("validation: accepted=1 skipped=1 fully_accepted=false"));
        assert!(text.contains("[skip 1] mission_state"));
        assert!(text.contains("destination has no capability passport"));
        assert!(text.contains("safety:inherit_mission_state"));
    }

    #[test]
    fn load_and_validate_capsule_accepts_when_passport_carries_required_capability() {
        let capsule = capsule_with(
            vec![CapsuleSection::MissionState {
                payload: serde_json::Value::Null,
            }],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let passport = passport_with(vec![CapabilityClass::SafetyConstraint(
            "inherit_mission_state".to_string(),
        )]);
        let text = load_and_validate_capsule(tmp.path(), Some(&passport), 2_000)
            .expect("validates");
        assert!(text.contains("validation: accepted=1 skipped=0 fully_accepted=true"));
    }

    #[test]
    fn load_and_validate_capsule_partial_skip_lists_per_section_unmet() {
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: "ok".to_string(),
                },
                CapsuleSection::MissionState {
                    payload: serde_json::Value::Null,
                },
                CapsuleSection::CausalSummary {
                    claim_ids: vec!["c1".to_string()],
                },
            ],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        // Passport carries only `inherit_mission_state` — the
        // CausalSummary section's `inherit_causal_summary` capability
        // is missing.
        let passport = passport_with(vec![CapabilityClass::SafetyConstraint(
            "inherit_mission_state".to_string(),
        )]);
        let text = load_and_validate_capsule(tmp.path(), Some(&passport), 2_000)
            .expect("validates");
        assert!(text.contains("validation: accepted=2 skipped=1 fully_accepted=false"));
        assert!(text.contains("[skip 2] causal_summary"));
        assert!(text.contains("safety:inherit_causal_summary"));
        // The accepted-indices line is sorted by input order.
        assert!(text.contains("accepted_indices: 0, 1"));
    }

    #[test]
    fn load_and_validate_capsule_returns_validation_error_on_integrity_mismatch() {
        let mut capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: "original".to_string(),
            }],
            1_000,
        );
        // Tamper: store the capsule with an in-memory mutation that
        // breaks the stored digest.
        if let Some(CapsuleSection::ContextSummary { text }) = capsule.sections.get_mut(0) {
            *text = "modified".to_string();
        }
        let tmp = write_capsule_to_temp(&capsule);
        let err = load_and_validate_capsule(tmp.path(), None, 2_000)
            .expect_err("integrity mismatch must error");
        match err {
            CapsuleValidateError::Validation {
                source: CapsuleValidationError::IntegrityMismatch { .. },
                ..
            } => {}
            other => panic!("expected IntegrityMismatch, got {other:?}"),
        }
    }

    #[test]
    fn load_and_validate_capsule_returns_read_error_for_missing_path() {
        let path =
            std::path::Path::new("/tmp/ft-r6kg2-no-such-file-canary-zzzzzzz");
        let err = load_and_validate_capsule(path, None, 1_000)
            .expect_err("missing path must error");
        match err {
            CapsuleValidateError::Read { path: p, .. } => assert_eq!(p, path),
            other => panic!("expected Read, got {other:?}"),
        }
    }

    #[test]
    fn load_and_validate_capsule_json_round_trips_envelope() {
        let capsule = capsule_with(
            vec![CapsuleSection::ContextSummary {
                text: "x".to_string(),
            }],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let json = load_and_validate_capsule_json(tmp.path(), None, 2_000)
            .expect("json validates");
        let parsed: ValidatedCapsuleDocument =
            serde_json::from_str(&json).expect("parse envelope");
        assert_eq!(parsed.inspection.version, 1);
        assert_eq!(parsed.validation.accepted_count, 1);
        assert_eq!(parsed.validation.skipped_count, 0);
        assert!(parsed.validation.fully_accepted);
    }

    #[test]
    fn load_and_validate_capsule_json_records_skip_capability_labels() {
        let capsule = capsule_with(
            vec![CapsuleSection::MissionState {
                payload: serde_json::Value::Null,
            }],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);
        let json = load_and_validate_capsule_json(tmp.path(), None, 2_000)
            .expect("json validates");
        let parsed: ValidatedCapsuleDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.validation.skipped.len(), 1);
        let skip = &parsed.validation.skipped[0];
        assert_eq!(skip.section_label, "mission_state");
        assert_eq!(
            skip.unmet_capability_labels,
            vec!["safety:inherit_mission_state".to_string()]
        );
    }

    /// br-ft-r6kg2 fail-closed canary: a planted credential in a
    /// section payload MUST NOT appear in either the text or JSON
    /// validation rendering, regardless of whether the section
    /// landed in `accepted` or `skipped`.
    #[test]
    fn load_and_validate_capsule_does_not_leak_planted_credential_ft_r6kg2() {
        const PLANTED: &str =
            "ANTHROPIC_API_KEY=sk-fake-r6kg2-canary-1122334455MNOPQRSTUVWX";
        let capsule = capsule_with(
            vec![
                CapsuleSection::ContextSummary {
                    text: PLANTED.to_string(),
                },
                CapsuleSection::MissionState {
                    payload: serde_json::json!({"secret": PLANTED}),
                },
            ],
            1_000,
        );
        let tmp = write_capsule_to_temp(&capsule);

        let text = load_and_validate_capsule(tmp.path(), None, 2_000).unwrap();
        assert!(
            !text.contains(PLANTED),
            "br-ft-r6kg2: text validation must not leak section payloads; got {text}"
        );

        let json = load_and_validate_capsule_json(tmp.path(), None, 2_000).unwrap();
        assert!(
            !json.contains(PLANTED),
            "br-ft-r6kg2: JSON validation must not leak section payloads; got {json}"
        );
    }

    #[test]
    fn render_validation_outcome_text_handles_empty_outcome() {
        let outcome = ValidationOutcome {
            accepted: Vec::new(),
            skipped: Vec::new(),
        };
        let rendered = render_validation_outcome_text(&outcome);
        assert!(rendered.contains("validation: accepted=0 skipped=0 fully_accepted=true"));
        assert!(rendered.contains("(no sections)"));
    }

    #[test]
    fn validation_outcome_rendering_serde_roundtrips() {
        let rendering = ValidationOutcomeRendering {
            fully_accepted: false,
            accepted_count: 1,
            skipped_count: 1,
            accepted_indices: vec![0],
            skipped: vec![SkipRendering {
                section_index: 1,
                section_label: "mission_state".to_string(),
                reason: "destination has no capability passport".to_string(),
                unmet_capability_labels: vec!["safety:inherit_mission_state".to_string()],
            }],
        };
        let json = serde_json::to_string(&rendering).unwrap();
        let parsed: ValidationOutcomeRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }
}
