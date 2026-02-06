//! Incident bundle format specification.
//!
//! Defines the canonical bundle layout, privacy budget, versioning, and
//! replay-mode contracts for wa incident bundles.  All types are
//! serializable so the spec lives alongside the code that enforces it.
//!
//! # Bundle directory layout
//!
//! ```text
//! wa_incident_{kind}_{YYYYMMDD_HHMMSS}/
//! ├── incident_manifest.json   # always present — versioned metadata
//! ├── README.md                # human-readable instructions
//! ├── redaction_report.json    # what was redacted (counts only)
//! ├── crash_report.json        # panic info (crash kind only)
//! ├── crash_manifest.json      # crash-time metadata (crash kind only)
//! ├── health_snapshot.json     # last HealthSnapshot (if available)
//! ├── config_summary.toml      # redacted config (if provided)
//! ├── db_metadata.json         # schema + storage stats (if db available)
//! └── recent_events.json       # bounded event summaries (if db + events)
//! ```
//!
//! # Privacy budget
//!
//! Default behaviour is **safe-to-share**.  Hard limits prevent accidental
//! exposure of raw secrets or unbounded data.  Callers may opt-in to
//! `verbose` mode for more data when the bundle is for internal use only.
//!
//! # Replay modes
//!
//! Three modes are defined with explicit input/output contracts:
//! - **Policy** — validates crash/incident consistency and redaction
//! - **Rules** — validates event structure and bounded text
//! - **WorkflowTrace** — validates workflow step logs and timing

use serde::{Deserialize, Serialize};

use crate::crash::IncidentKind;

// ───────────────────────────────────────────────────────────────────────────
// Format version
// ───────────────────────────────────────────────────────────────────────────

/// Current bundle format version.
///
/// Replay tooling **refuses** bundles with a different `major` version
/// and **warns** when `minor` is newer than the reader's version.
pub const CURRENT_FORMAT_VERSION: BundleFormatVersion = BundleFormatVersion { major: 1, minor: 0 };

/// Semantic version of the incident bundle format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFormatVersion {
    /// Incremented on breaking layout/schema changes.
    pub major: u16,
    /// Incremented on backwards-compatible additions.
    pub minor: u16,
}

impl std::fmt::Display for BundleFormatVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Error returned when a bundle's format version is incompatible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleVersionError {
    /// Major version mismatch — the bundle cannot be read.
    IncompatibleMajor {
        reader_major: u16,
        bundle_major: u16,
    },
    /// Bundle has a newer minor version — reads may miss fields.
    NewerMinor {
        reader_minor: u16,
        bundle_minor: u16,
    },
}

impl std::fmt::Display for BundleVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleMajor {
                reader_major,
                bundle_major,
            } => write!(
                f,
                "incompatible bundle format: reader supports major version {reader_major}, \
                 bundle is major version {bundle_major}"
            ),
            Self::NewerMinor {
                reader_minor,
                bundle_minor,
            } => write!(
                f,
                "bundle minor version {bundle_minor} is newer than reader version {reader_minor} \
                 — some fields may be missing"
            ),
        }
    }
}

impl std::error::Error for BundleVersionError {}

impl BundleFormatVersion {
    /// Check whether this reader version can handle a bundle with `bundle_version`.
    ///
    /// - Same major → Ok (newer minor → warning via `NewerMinor`)
    /// - Different major → Err
    pub fn check_compatibility(&self, bundle_version: &Self) -> Result<(), BundleVersionError> {
        if self.major != bundle_version.major {
            return Err(BundleVersionError::IncompatibleMajor {
                reader_major: self.major,
                bundle_major: bundle_version.major,
            });
        }
        if bundle_version.minor > self.minor {
            return Err(BundleVersionError::NewerMinor {
                reader_minor: self.minor,
                bundle_minor: bundle_version.minor,
            });
        }
        Ok(())
    }

    /// True when major versions match (backwards-compatible reads).
    #[must_use]
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        self.major == other.major
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Privacy budget
// ───────────────────────────────────────────────────────────────────────────

/// Configurable limits that bound the data included in a bundle.
///
/// Defaults are conservative ("safe-to-share with a vendor").  Use
/// [`PrivacyBudget::verbose`] when the bundle is for internal debugging
/// and more data is acceptable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyBudget {
    /// Maximum bytes written per individual file.
    pub max_bytes_per_file: usize,
    /// Maximum total bytes for the entire bundle (all files combined).
    pub max_total_bytes: usize,
    /// Maximum lines included per log/text file.
    pub max_lines_per_log: usize,
    /// Maximum characters for any single output excerpt (e.g. `matched_text_preview`).
    pub max_output_excerpt_len: usize,
    /// Maximum backtrace string length (bytes).
    pub max_backtrace_len: usize,
    /// Whether to include DB metadata (schema version, row counts).
    pub include_db_metadata: bool,
    /// Whether to include recent event summaries.
    pub include_recent_events: bool,
    /// Maximum number of recent events to include (when enabled).
    pub max_recent_events: usize,
}

/// Error returned when a [`PrivacyBudget`] contains invalid values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyBudgetError {
    /// Per-file limit exceeds total limit.
    FileExceedsTotal {
        max_bytes_per_file: usize,
        max_total_bytes: usize,
    },
    /// A zero limit that would make the bundle empty.
    ZeroLimit { field: &'static str },
}

impl std::fmt::Display for PrivacyBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileExceedsTotal {
                max_bytes_per_file,
                max_total_bytes,
            } => write!(
                f,
                "max_bytes_per_file ({max_bytes_per_file}) exceeds max_total_bytes ({max_total_bytes})"
            ),
            Self::ZeroLimit { field } => write!(f, "{field} must be > 0"),
        }
    }
}

impl std::error::Error for PrivacyBudgetError {}

impl Default for PrivacyBudget {
    /// Conservative defaults safe for sharing externally.
    fn default() -> Self {
        Self {
            max_bytes_per_file: 256 * 1024, // 256 KiB
            max_total_bytes: 1024 * 1024,   // 1 MiB
            max_lines_per_log: 1_000,
            max_output_excerpt_len: 200,
            max_backtrace_len: 64 * 1024, // 64 KiB
            include_db_metadata: true,
            include_recent_events: true,
            max_recent_events: 50,
        }
    }
}

impl PrivacyBudget {
    /// Strict budget — minimal data, suitable for public/vendor sharing.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            max_bytes_per_file: 64 * 1024, // 64 KiB
            max_total_bytes: 256 * 1024,   // 256 KiB
            max_lines_per_log: 200,
            max_output_excerpt_len: 100,
            max_backtrace_len: 16 * 1024, // 16 KiB
            include_db_metadata: true,
            include_recent_events: false, // no events
            max_recent_events: 0,
        }
    }

    /// Verbose budget — more data for internal debugging.
    #[must_use]
    pub fn verbose() -> Self {
        Self {
            max_bytes_per_file: 1024 * 1024,  // 1 MiB
            max_total_bytes: 4 * 1024 * 1024, // 4 MiB
            max_lines_per_log: 10_000,
            max_output_excerpt_len: 500,
            max_backtrace_len: 128 * 1024, // 128 KiB
            include_db_metadata: true,
            include_recent_events: true,
            max_recent_events: 200,
        }
    }

    /// Validate that budget values are internally consistent.
    pub fn validate(&self) -> Result<(), PrivacyBudgetError> {
        if self.max_bytes_per_file > self.max_total_bytes {
            return Err(PrivacyBudgetError::FileExceedsTotal {
                max_bytes_per_file: self.max_bytes_per_file,
                max_total_bytes: self.max_total_bytes,
            });
        }
        if self.max_total_bytes == 0 {
            return Err(PrivacyBudgetError::ZeroLimit {
                field: "max_total_bytes",
            });
        }
        if self.max_bytes_per_file == 0 {
            return Err(PrivacyBudgetError::ZeroLimit {
                field: "max_bytes_per_file",
            });
        }
        Ok(())
    }

    /// Check whether writing `additional_bytes` would exceed the total budget
    /// given `current_bytes` already written.
    #[must_use]
    pub fn would_exceed_total(&self, current_bytes: usize, additional_bytes: usize) -> bool {
        current_bytes.saturating_add(additional_bytes) > self.max_total_bytes
    }

    /// Truncate `content` to fit within `max_bytes_per_file`, appending a
    /// truncation marker if shortened.
    #[must_use]
    pub fn truncate_file_content(&self, content: &str) -> String {
        if content.len() <= self.max_bytes_per_file {
            return content.to_string();
        }
        // Find a safe UTF-8 boundary
        let mut end = self.max_bytes_per_file.saturating_sub(40);
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        let marker = format!(
            "\n... [truncated at {} bytes, limit {}]",
            content.len(),
            self.max_bytes_per_file
        );
        format!("{}{marker}", &content[..end])
    }

    /// Truncate a text excerpt to `max_output_excerpt_len` characters.
    #[must_use]
    pub fn truncate_excerpt(&self, text: &str) -> String {
        if text.chars().count() <= self.max_output_excerpt_len {
            return text.to_string();
        }
        let truncated: String = text.chars().take(self.max_output_excerpt_len).collect();
        format!("{truncated}...")
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Bundle file layout
// ───────────────────────────────────────────────────────────────────────────

/// Known files that may appear in a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleFile {
    /// `incident_manifest.json` — versioned metadata, always present.
    Manifest,
    /// `README.md` — human-readable instructions, always present.
    Readme,
    /// `redaction_report.json` — what was redacted (counts, no secrets).
    RedactionReport,
    /// `crash_report.json` — panic details (crash kind only).
    CrashReport,
    /// `crash_manifest.json` — crash-time metadata (crash kind only).
    CrashManifest,
    /// `health_snapshot.json` — last known runtime health.
    HealthSnapshot,
    /// `config_summary.toml` — redacted configuration.
    ConfigSummary,
    /// `db_metadata.json` — schema + storage statistics.
    DbMetadata,
    /// `recent_events.json` — bounded event summaries.
    RecentEvents,
}

impl BundleFile {
    /// The on-disk filename for this bundle file.
    #[must_use]
    pub fn filename(&self) -> &'static str {
        match self {
            Self::Manifest => "incident_manifest.json",
            Self::Readme => "README.md",
            Self::RedactionReport => "redaction_report.json",
            Self::CrashReport => "crash_report.json",
            Self::CrashManifest => "crash_manifest.json",
            Self::HealthSnapshot => "health_snapshot.json",
            Self::ConfigSummary => "config_summary.toml",
            Self::DbMetadata => "db_metadata.json",
            Self::RecentEvents => "recent_events.json",
        }
    }

    /// Whether this file is mandatory (must be present in every valid bundle).
    #[must_use]
    pub fn is_required(&self) -> bool {
        matches!(self, Self::Manifest | Self::Readme | Self::RedactionReport)
    }

    /// Whether this file type is relevant for the given incident kind.
    #[must_use]
    pub fn applies_to_kind(&self, kind: IncidentKind) -> bool {
        match self {
            Self::Manifest | Self::Readme | Self::RedactionReport => true,
            Self::CrashReport | Self::CrashManifest => kind == IncidentKind::Crash,
            Self::HealthSnapshot | Self::ConfigSummary | Self::DbMetadata | Self::RecentEvents => {
                true
            }
        }
    }

    /// All known bundle file variants.
    pub fn all() -> &'static [BundleFile] {
        &[
            Self::Manifest,
            Self::Readme,
            Self::RedactionReport,
            Self::CrashReport,
            Self::CrashManifest,
            Self::HealthSnapshot,
            Self::ConfigSummary,
            Self::DbMetadata,
            Self::RecentEvents,
        ]
    }

    /// Files expected for a given incident kind (required + optional).
    #[must_use]
    pub fn expected_for_kind(kind: IncidentKind) -> Vec<BundleFile> {
        Self::all()
            .iter()
            .copied()
            .filter(|f| f.applies_to_kind(kind))
            .collect()
    }
}

/// Bundle directory naming convention.
///
/// Format: `wa_incident_{kind}_{YYYYMMDD_HHMMSS}`
///
/// The timestamp uses UTC, formatted as `YYYYMMDD_HHMMSS`.  If a directory
/// with the same name already exists, callers should append `_2`, `_3`, etc.
#[must_use]
pub fn bundle_dirname(kind: IncidentKind, timestamp_str: &str) -> String {
    format!("wa_incident_{kind}_{timestamp_str}")
}

// ───────────────────────────────────────────────────────────────────────────
// Enhanced manifest (the canonical schema)
// ───────────────────────────────────────────────────────────────────────────

/// Incident manifest — the root metadata document written to every bundle.
///
/// This replaces the ad-hoc `IncidentBundleResult` as the canonical
/// schema definition.  The `format_version` field enables replay tooling
/// to reject incompatible bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentManifest {
    /// Bundle format version for compatibility checking.
    pub format_version: BundleFormatVersion,
    /// wa binary version that produced the bundle.
    pub wa_version: String,
    /// Kind of incident.
    pub kind: IncidentKind,
    /// ISO-8601 UTC timestamp of bundle creation.
    pub created_at: String,
    /// Files included in the bundle with per-file metadata.
    pub files: Vec<BundleFileEntry>,
    /// Privacy budget summary describing applied limits.
    pub privacy_budget: PrivacyBudgetSummary,
    /// Total bundle size in bytes (all files combined).
    pub total_size_bytes: u64,
    /// Redaction summary (counts only, no secrets).
    pub redaction_summary: Option<RedactionSummary>,
}

/// Per-file entry in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFileEntry {
    /// Filename within the bundle directory.
    pub name: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Whether the file contents were passed through the redactor.
    pub redacted: bool,
}

/// Summary of the privacy budget applied to this bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyBudgetSummary {
    /// Which budget tier was used.
    pub tier: String,
    /// Maximum total bytes allowed.
    pub max_total_bytes: usize,
    /// Maximum bytes per file.
    pub max_bytes_per_file: usize,
    /// Whether recent events were included.
    pub includes_events: bool,
    /// Maximum events included (0 if events excluded).
    pub max_events: usize,
}

impl From<&PrivacyBudget> for PrivacyBudgetSummary {
    fn from(budget: &PrivacyBudget) -> Self {
        let tier = if *budget == PrivacyBudget::strict() {
            "strict"
        } else if *budget == PrivacyBudget::verbose() {
            "verbose"
        } else if *budget == PrivacyBudget::default() {
            "default"
        } else {
            "custom"
        };
        Self {
            tier: tier.to_string(),
            max_total_bytes: budget.max_total_bytes,
            max_bytes_per_file: budget.max_bytes_per_file,
            includes_events: budget.include_recent_events,
            max_events: budget.max_recent_events,
        }
    }
}

/// Summary of redactions applied across the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionSummary {
    /// Total number of redaction replacements.
    pub total_redactions: usize,
    /// Number of files that had at least one redaction.
    pub files_with_redactions: usize,
}

// ───────────────────────────────────────────────────────────────────────────
// Replay modes + contracts
// ───────────────────────────────────────────────────────────────────────────

/// Extended replay mode with explicit contracts.
///
/// Each mode defines which files are required and which checks are run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleReplayMode {
    /// Validate crash/incident consistency and redaction correctness.
    ///
    /// **Required files:** `incident_manifest.json`
    /// **Optional files:** `crash_report.json`, `db_metadata.json`,
    ///   `redaction_report.json`
    ///
    /// **Checks:**
    /// 1. `manifest_valid` — manifest parses correctly
    /// 2. `redaction_report_valid` — redaction report is well-formed
    /// 3. `no_secrets_leaked` — no patterns detected in any file
    /// 4. `crash_report_valid` — crash report parses (if present)
    /// 5. `db_metadata_valid` — DB metadata parses (if present)
    /// 6. `files_complete` — all manifest-listed files exist on disk
    /// 7. `version_compatible` — format version is readable
    Policy,

    /// Validate event data structure and bounded text.
    ///
    /// **Required files:** `incident_manifest.json`
    /// **Optional files:** `recent_events.json`, `redaction_report.json`
    ///
    /// **Checks:**
    /// 1. `manifest_valid`
    /// 2. `redaction_report_valid`
    /// 3. `no_secrets_leaked`
    /// 4. `events_structure_valid` — events have required fields
    /// 5. `events_text_bounded` — all text excerpts within budget
    /// 6. `files_complete`
    /// 7. `version_compatible`
    Rules,

    /// Validate workflow step logs and execution traces.
    ///
    /// **Required files:** `incident_manifest.json`
    /// **Optional files:** `recent_events.json` (with workflow-type events)
    ///
    /// **Checks:**
    /// 1. `manifest_valid`
    /// 2. `redaction_report_valid`
    /// 3. `no_secrets_leaked`
    /// 4. `workflow_steps_valid` — step logs have required fields
    /// 5. `workflow_timing_valid` — step timestamps are monotonic
    /// 6. `workflow_no_raw_output` — step output is bounded
    /// 7. `files_complete`
    /// 8. `version_compatible`
    WorkflowTrace,
}

impl std::fmt::Display for BundleReplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy => write!(f, "policy"),
            Self::Rules => write!(f, "rules"),
            Self::WorkflowTrace => write!(f, "workflow_trace"),
        }
    }
}

/// Contract definition for a replay mode.
///
/// Describes what each mode requires and what it validates, enabling
/// tooling to report clear errors when prerequisites are missing.
#[derive(Debug, Clone)]
pub struct ReplayContract {
    /// The replay mode this contract describes.
    pub mode: BundleReplayMode,
    /// Files that must be present for this mode to run.
    pub required_files: Vec<BundleFile>,
    /// Files that are used if present but not mandatory.
    pub optional_files: Vec<BundleFile>,
    /// Names of checks that this mode runs.
    pub checks: Vec<&'static str>,
    /// Human-readable description of what this mode validates.
    pub description: &'static str,
}

impl BundleReplayMode {
    /// Return the formal contract for this replay mode.
    #[must_use]
    pub fn contract(&self) -> ReplayContract {
        match self {
            Self::Policy => ReplayContract {
                mode: *self,
                required_files: vec![BundleFile::Manifest],
                optional_files: vec![
                    BundleFile::CrashReport,
                    BundleFile::DbMetadata,
                    BundleFile::RedactionReport,
                    BundleFile::HealthSnapshot,
                ],
                checks: vec![
                    "manifest_valid",
                    "version_compatible",
                    "redaction_report_valid",
                    "no_secrets_leaked",
                    "crash_report_valid",
                    "db_metadata_valid",
                    "files_complete",
                ],
                description: "Validates crash/incident consistency and redaction correctness",
            },
            Self::Rules => ReplayContract {
                mode: *self,
                required_files: vec![BundleFile::Manifest],
                optional_files: vec![BundleFile::RecentEvents, BundleFile::RedactionReport],
                checks: vec![
                    "manifest_valid",
                    "version_compatible",
                    "redaction_report_valid",
                    "no_secrets_leaked",
                    "events_structure_valid",
                    "events_text_bounded",
                    "files_complete",
                ],
                description: "Validates event data structure and bounded text excerpts",
            },
            Self::WorkflowTrace => ReplayContract {
                mode: *self,
                required_files: vec![BundleFile::Manifest],
                optional_files: vec![BundleFile::RecentEvents, BundleFile::RedactionReport],
                checks: vec![
                    "manifest_valid",
                    "version_compatible",
                    "redaction_report_valid",
                    "no_secrets_leaked",
                    "workflow_steps_valid",
                    "workflow_timing_valid",
                    "workflow_no_raw_output",
                    "files_complete",
                ],
                description: "Validates workflow step logs, timing, and output boundaries",
            },
        }
    }

    /// All defined replay modes.
    pub fn all() -> &'static [BundleReplayMode] {
        &[Self::Policy, Self::Rules, Self::WorkflowTrace]
    }
}

// ───────────────────────────────────────────────────────────────────────────
// README generation
// ───────────────────────────────────────────────────────────────────────────

/// Generate a human-readable README.md for an incident bundle.
///
/// The README explains what the bundle contains, how to replay it,
/// and confirms that secrets have been redacted.
#[must_use]
pub fn generate_bundle_readme(manifest: &IncidentManifest) -> String {
    let mut out = String::with_capacity(1024);

    out.push_str("# wa Incident Bundle\n\n");
    out.push_str(&format!(
        "**Kind:** {}  \n**Created:** {}  \n**wa version:** {}  \n**Format version:** {}  \n\n",
        manifest.kind, manifest.created_at, manifest.wa_version, manifest.format_version,
    ));

    out.push_str("## Contents\n\n");
    out.push_str("| File | Size | Redacted |\n");
    out.push_str("|------|------|----------|\n");
    for entry in &manifest.files {
        let size_display = if entry.size_bytes < 1024 {
            format!("{} B", entry.size_bytes)
        } else {
            format!("{:.1} KiB", entry.size_bytes as f64 / 1024.0)
        };
        let redacted = if entry.redacted { "yes" } else { "no" };
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            entry.name, size_display, redacted
        ));
    }
    out.push('\n');

    if let Some(ref summary) = manifest.redaction_summary {
        out.push_str(&format!(
            "**Redaction:** {} secret(s) redacted across {} file(s).  \n",
            summary.total_redactions, summary.files_with_redactions,
        ));
        out.push_str("All secrets have been replaced with `[REDACTED]` markers.\n\n");
    } else {
        out.push_str("**Redaction:** No secrets detected.\n\n");
    }

    out.push_str(&format!(
        "**Privacy budget:** {} (max {} total, {} per file)\n\n",
        manifest.privacy_budget.tier,
        format_bytes(manifest.privacy_budget.max_total_bytes),
        format_bytes(manifest.privacy_budget.max_bytes_per_file),
    ));

    out.push_str("## Replay\n\n");
    out.push_str("Validate this bundle using:\n\n");
    out.push_str("```bash\n");
    out.push_str("wa reproduce --mode policy  <bundle-dir>  # check consistency + redaction\n");
    out.push_str("wa reproduce --mode rules   <bundle-dir>  # check event structure\n");
    out.push_str("wa reproduce --mode workflow <bundle-dir>  # check workflow traces\n");
    out.push_str("```\n\n");

    out.push_str("## Safety\n\n");
    out.push_str(
        "This bundle was produced with automatic secret detection and redaction.\n\
         Review the `redaction_report.json` for details on what was removed.\n\
         If you find sensitive data that should have been redacted, please report\n\
         it so the detection patterns can be improved.\n",
    );

    out
}

/// Format a byte count for human display.
fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{} MiB", bytes / (1024 * 1024))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- Format version ---

    #[test]
    fn format_version_display() {
        assert_eq!(CURRENT_FORMAT_VERSION.to_string(), "1.0");
        let v = BundleFormatVersion { major: 2, minor: 3 };
        assert_eq!(v.to_string(), "2.3");
    }

    #[test]
    fn format_version_same_version_is_compatible() {
        let v = BundleFormatVersion { major: 1, minor: 0 };
        assert!(v.check_compatibility(&v).is_ok());
    }

    #[test]
    fn format_version_older_minor_is_compatible() {
        let reader = BundleFormatVersion { major: 1, minor: 2 };
        let bundle = BundleFormatVersion { major: 1, minor: 1 };
        assert!(reader.check_compatibility(&bundle).is_ok());
    }

    #[test]
    fn format_version_newer_minor_returns_warning() {
        let reader = BundleFormatVersion { major: 1, minor: 0 };
        let bundle = BundleFormatVersion { major: 1, minor: 3 };
        let err = reader.check_compatibility(&bundle).unwrap_err();
        assert!(matches!(err, BundleVersionError::NewerMinor { .. }));
    }

    #[test]
    fn format_version_different_major_is_incompatible() {
        let reader = BundleFormatVersion { major: 1, minor: 0 };
        let bundle = BundleFormatVersion { major: 2, minor: 0 };
        let err = reader.check_compatibility(&bundle).unwrap_err();
        assert!(matches!(err, BundleVersionError::IncompatibleMajor { .. }));
    }

    #[test]
    fn format_version_is_compatible_with() {
        let a = BundleFormatVersion { major: 1, minor: 0 };
        let b = BundleFormatVersion { major: 1, minor: 5 };
        let c = BundleFormatVersion { major: 2, minor: 0 };
        assert!(a.is_compatible_with(&b));
        assert!(!a.is_compatible_with(&c));
    }

    #[test]
    fn format_version_roundtrip_serde() {
        let v = BundleFormatVersion { major: 1, minor: 2 };
        let json = serde_json::to_string(&v).unwrap();
        let parsed: BundleFormatVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(v, parsed);
    }

    // --- Privacy budget ---

    #[test]
    fn default_budget_is_valid() {
        assert!(PrivacyBudget::default().validate().is_ok());
    }

    #[test]
    fn strict_budget_is_valid() {
        assert!(PrivacyBudget::strict().validate().is_ok());
    }

    #[test]
    fn verbose_budget_is_valid() {
        assert!(PrivacyBudget::verbose().validate().is_ok());
    }

    #[test]
    fn budget_file_exceeds_total_is_invalid() {
        let mut budget = PrivacyBudget::default();
        budget.max_bytes_per_file = budget.max_total_bytes + 1;
        let err = budget.validate().unwrap_err();
        assert!(matches!(err, PrivacyBudgetError::FileExceedsTotal { .. }));
    }

    #[test]
    fn budget_zero_total_is_invalid() {
        let mut budget = PrivacyBudget::default();
        budget.max_total_bytes = 0;
        budget.max_bytes_per_file = 0;
        let err = budget.validate().unwrap_err();
        assert!(matches!(err, PrivacyBudgetError::ZeroLimit { .. }));
    }

    #[test]
    fn budget_would_exceed_total() {
        let budget = PrivacyBudget::default();
        assert!(!budget.would_exceed_total(0, 100));
        assert!(budget.would_exceed_total(budget.max_total_bytes, 1));
        assert!(budget.would_exceed_total(budget.max_total_bytes - 10, 20));
    }

    #[test]
    fn budget_truncate_file_content() {
        let budget = PrivacyBudget {
            max_bytes_per_file: 50,
            ..PrivacyBudget::default()
        };
        let short = "hello";
        assert_eq!(budget.truncate_file_content(short), short);

        let long = "a".repeat(100);
        let truncated = budget.truncate_file_content(&long);
        assert!(truncated.len() <= 60); // 50 + marker
        assert!(truncated.contains("truncated"));
    }

    #[test]
    fn budget_truncate_excerpt() {
        let budget = PrivacyBudget {
            max_output_excerpt_len: 10,
            ..PrivacyBudget::default()
        };
        assert_eq!(budget.truncate_excerpt("short"), "short");
        assert_eq!(
            budget.truncate_excerpt("this is a longer text"),
            "this is a ..."
        );
    }

    #[test]
    fn budget_roundtrip_serde() {
        let budget = PrivacyBudget::strict();
        let json = serde_json::to_string(&budget).unwrap();
        let parsed: PrivacyBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget, parsed);
    }

    #[test]
    fn strict_budget_is_more_restrictive_than_default() {
        let strict = PrivacyBudget::strict();
        let default = PrivacyBudget::default();
        assert!(strict.max_total_bytes < default.max_total_bytes);
        assert!(strict.max_bytes_per_file < default.max_bytes_per_file);
        assert!(strict.max_lines_per_log < default.max_lines_per_log);
        assert!(!strict.include_recent_events);
    }

    #[test]
    fn verbose_budget_is_more_permissive_than_default() {
        let verbose = PrivacyBudget::verbose();
        let default = PrivacyBudget::default();
        assert!(verbose.max_total_bytes > default.max_total_bytes);
        assert!(verbose.max_bytes_per_file > default.max_bytes_per_file);
        assert!(verbose.max_recent_events > default.max_recent_events);
    }

    // --- Bundle file layout ---

    #[test]
    fn bundle_file_filenames_are_unique() {
        let names: Vec<&str> = BundleFile::all().iter().map(|f| f.filename()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn required_files_are_always_applicable() {
        for file in BundleFile::all() {
            if file.is_required() {
                assert!(file.applies_to_kind(IncidentKind::Crash));
                assert!(file.applies_to_kind(IncidentKind::Manual));
            }
        }
    }

    #[test]
    fn crash_files_only_apply_to_crash_kind() {
        assert!(BundleFile::CrashReport.applies_to_kind(IncidentKind::Crash));
        assert!(!BundleFile::CrashReport.applies_to_kind(IncidentKind::Manual));
        assert!(BundleFile::CrashManifest.applies_to_kind(IncidentKind::Crash));
        assert!(!BundleFile::CrashManifest.applies_to_kind(IncidentKind::Manual));
    }

    #[test]
    fn expected_for_crash_includes_crash_files() {
        let expected = BundleFile::expected_for_kind(IncidentKind::Crash);
        assert!(expected.contains(&BundleFile::CrashReport));
        assert!(expected.contains(&BundleFile::CrashManifest));
        assert!(expected.contains(&BundleFile::Manifest));
    }

    #[test]
    fn expected_for_manual_excludes_crash_files() {
        let expected = BundleFile::expected_for_kind(IncidentKind::Manual);
        assert!(!expected.contains(&BundleFile::CrashReport));
        assert!(!expected.contains(&BundleFile::CrashManifest));
        assert!(expected.contains(&BundleFile::Manifest));
    }

    #[test]
    fn bundle_dirname_format() {
        let name = bundle_dirname(IncidentKind::Crash, "20260206_183000");
        assert_eq!(name, "wa_incident_crash_20260206_183000");

        let name = bundle_dirname(IncidentKind::Manual, "20260101_120000");
        assert_eq!(name, "wa_incident_manual_20260101_120000");
    }

    // --- Manifest ---

    #[test]
    fn manifest_roundtrip_serde() {
        let manifest = sample_manifest();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: IncidentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.format_version, manifest.format_version);
        assert_eq!(parsed.kind, manifest.kind);
        assert_eq!(parsed.files.len(), manifest.files.len());
    }

    #[test]
    fn manifest_includes_format_version() {
        let manifest = sample_manifest();
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(json.contains("\"format_version\""));
        assert!(json.contains("\"major\":1"));
    }

    #[test]
    fn privacy_budget_summary_from_default() {
        let summary = PrivacyBudgetSummary::from(&PrivacyBudget::default());
        assert_eq!(summary.tier, "default");
        assert!(summary.includes_events);
    }

    #[test]
    fn privacy_budget_summary_from_strict() {
        let summary = PrivacyBudgetSummary::from(&PrivacyBudget::strict());
        assert_eq!(summary.tier, "strict");
        assert!(!summary.includes_events);
    }

    #[test]
    fn privacy_budget_summary_from_verbose() {
        let summary = PrivacyBudgetSummary::from(&PrivacyBudget::verbose());
        assert_eq!(summary.tier, "verbose");
        assert!(summary.includes_events);
    }

    #[test]
    fn privacy_budget_summary_custom() {
        let mut budget = PrivacyBudget::default();
        budget.max_total_bytes = 42;
        budget.max_bytes_per_file = 10;
        let summary = PrivacyBudgetSummary::from(&budget);
        assert_eq!(summary.tier, "custom");
    }

    // --- Replay modes ---

    #[test]
    fn replay_mode_display() {
        assert_eq!(BundleReplayMode::Policy.to_string(), "policy");
        assert_eq!(BundleReplayMode::Rules.to_string(), "rules");
        assert_eq!(
            BundleReplayMode::WorkflowTrace.to_string(),
            "workflow_trace"
        );
    }

    #[test]
    fn replay_mode_roundtrip_serde() {
        for mode in BundleReplayMode::all() {
            let json = serde_json::to_string(mode).unwrap();
            let parsed: BundleReplayMode = serde_json::from_str(&json).unwrap();
            assert_eq!(*mode, parsed);
        }
    }

    #[test]
    fn policy_contract_requires_manifest() {
        let contract = BundleReplayMode::Policy.contract();
        assert!(contract.required_files.contains(&BundleFile::Manifest));
        assert!(contract.checks.contains(&"no_secrets_leaked"));
        assert!(contract.checks.contains(&"version_compatible"));
    }

    #[test]
    fn rules_contract_checks_events() {
        let contract = BundleReplayMode::Rules.contract();
        assert!(contract.checks.contains(&"events_structure_valid"));
        assert!(contract.checks.contains(&"events_text_bounded"));
    }

    #[test]
    fn workflow_contract_checks_steps_and_timing() {
        let contract = BundleReplayMode::WorkflowTrace.contract();
        assert!(contract.checks.contains(&"workflow_steps_valid"));
        assert!(contract.checks.contains(&"workflow_timing_valid"));
        assert!(contract.checks.contains(&"workflow_no_raw_output"));
    }

    #[test]
    fn all_replay_modes_have_manifest_check() {
        for mode in BundleReplayMode::all() {
            let contract = mode.contract();
            assert!(
                contract.checks.contains(&"manifest_valid"),
                "{mode} missing manifest_valid check"
            );
        }
    }

    #[test]
    fn all_replay_modes_have_secret_leak_check() {
        for mode in BundleReplayMode::all() {
            let contract = mode.contract();
            assert!(
                contract.checks.contains(&"no_secrets_leaked"),
                "{mode} missing no_secrets_leaked check"
            );
        }
    }

    #[test]
    fn all_replay_modes_have_version_check() {
        for mode in BundleReplayMode::all() {
            let contract = mode.contract();
            assert!(
                contract.checks.contains(&"version_compatible"),
                "{mode} missing version_compatible check"
            );
        }
    }

    // --- README generation ---

    #[test]
    fn readme_contains_bundle_metadata() {
        let manifest = sample_manifest();
        let readme = generate_bundle_readme(&manifest);
        assert!(readme.contains("wa Incident Bundle"));
        assert!(readme.contains("crash"));
        assert!(readme.contains("0.1.0-test"));
        assert!(readme.contains("1.0"));
    }

    #[test]
    fn readme_contains_file_table() {
        let manifest = sample_manifest();
        let readme = generate_bundle_readme(&manifest);
        assert!(readme.contains("incident_manifest.json"));
        assert!(readme.contains("crash_report.json"));
        assert!(readme.contains("| File |"));
    }

    #[test]
    fn readme_contains_replay_instructions() {
        let manifest = sample_manifest();
        let readme = generate_bundle_readme(&manifest);
        assert!(readme.contains("wa reproduce"));
        assert!(readme.contains("--mode policy"));
        assert!(readme.contains("--mode rules"));
        assert!(readme.contains("--mode workflow"));
    }

    #[test]
    fn readme_mentions_redaction() {
        let manifest = sample_manifest();
        let readme = generate_bundle_readme(&manifest);
        assert!(readme.contains("redact"));
    }

    #[test]
    fn readme_shows_privacy_budget() {
        let manifest = sample_manifest();
        let readme = generate_bundle_readme(&manifest);
        assert!(readme.contains("Privacy budget"));
        assert!(readme.contains("default"));
    }

    #[test]
    fn readme_with_no_redactions() {
        let mut manifest = sample_manifest();
        manifest.redaction_summary = None;
        let readme = generate_bundle_readme(&manifest);
        assert!(readme.contains("No secrets detected"));
    }

    // --- Helpers ---

    fn sample_manifest() -> IncidentManifest {
        IncidentManifest {
            format_version: CURRENT_FORMAT_VERSION,
            wa_version: "0.1.0-test".to_string(),
            kind: IncidentKind::Crash,
            created_at: "2026-02-06T18:30:00Z".to_string(),
            files: vec![
                BundleFileEntry {
                    name: "incident_manifest.json".to_string(),
                    size_bytes: 512,
                    redacted: false,
                },
                BundleFileEntry {
                    name: "crash_report.json".to_string(),
                    size_bytes: 2048,
                    redacted: true,
                },
            ],
            privacy_budget: PrivacyBudgetSummary::from(&PrivacyBudget::default()),
            total_size_bytes: 2560,
            redaction_summary: Some(RedactionSummary {
                total_redactions: 3,
                files_with_redactions: 1,
            }),
        }
    }
}
