//! Replay artifact registry — structured lifecycle management for `.ftreplay` artifacts.
//!
//! Provides listing, inspection, registration, retirement, and pruning of replay
//! artifacts via a TOML-based manifest. The manifest is append-only per the
//! replay charter (principle 5.6) and lives at `tests/regression/replay/manifest.toml`.
//!
//! # Architecture
//!
//! ```text
//! ArtifactManifest (manifest.toml)
//!   ├── ArtifactEntry { path, label, sha256, event_count, ... }
//!   ├── ArtifactEntry { ... }
//!   └── ...
//! ArtifactRegistry
//!   ├── list(filter) → Vec<ArtifactEntry>
//!   ├── inspect(path) → ArtifactDetail
//!   ├── add(path, label) → Result<()>
//!   ├── retire(path, reason) → Result<()>
//!   └── prune(opts) → PruneResult
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Current manifest schema version.
pub const MANIFEST_SCHEMA_VERSION: &str = "replay.manifest.v1";

/// Default retention period for retired artifacts (days).
pub const DEFAULT_RETENTION_DAYS: u64 = 30;

// ---------------------------------------------------------------------------
// Sensitivity tier (mirrors replay_capture::CaptureSensitivityTier)
// ---------------------------------------------------------------------------

/// Sensitivity classification for replay artifacts.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSensitivityTier {
    /// Non-sensitive operational data.
    #[default]
    T1,
    /// May contain PII/secrets after redaction.
    T2,
    /// Known secrets or unredacted capture.
    T3,
}

impl ArtifactSensitivityTier {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::T1 => "T1",
            Self::T2 => "T2",
            Self::T3 => "T3",
        }
    }

    /// Parse from string (case-insensitive).
    pub fn from_str_arg(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "t1" => Some(Self::T1),
            "t2" => Some(Self::T2),
            "t3" => Some(Self::T3),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Artifact status
// ---------------------------------------------------------------------------

/// Lifecycle status of a registered artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Active in the regression suite.
    #[default]
    Active,
    /// Excluded from regression, pending pruning.
    Retired,
}

// ---------------------------------------------------------------------------
// Artifact entry (manifest record)
// ---------------------------------------------------------------------------

/// A single artifact registered in the manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// Relative path to the artifact file.
    pub path: String,
    /// Human-readable label.
    pub label: String,
    /// SHA-256 hex digest of file contents.
    pub sha256: String,
    /// Number of events in the artifact.
    pub event_count: u64,
    /// Number of distinct decision types.
    pub decision_count: u64,
    /// Unix timestamp (ms) when the artifact was created.
    pub created_at_ms: u64,
    /// Sensitivity classification.
    pub sensitivity_tier: ArtifactSensitivityTier,
    /// Current lifecycle status.
    pub status: ArtifactStatus,
    /// Byte size of the artifact file.
    pub size_bytes: u64,
    /// Retirement reason (only set when status == Retired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retire_reason: Option<String>,
    /// Unix timestamp (ms) when the artifact was retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Artifact detail (extended inspection)
// ---------------------------------------------------------------------------

/// Detailed inspection output for a single artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactDetail {
    /// The manifest entry.
    pub entry: ArtifactEntry,
    /// Whether the on-disk file matches the manifest SHA-256.
    pub integrity_ok: bool,
    /// Whether the on-disk file exists.
    pub file_exists: bool,
    /// Decision type breakdown (type → count).
    pub decision_type_counts: BTreeMap<String, u64>,
    /// Time span covered by events (ms).
    pub time_span_ms: u64,
    /// Distinct pane IDs referenced.
    pub pane_count: u64,
    /// Distinct rule IDs referenced.
    pub rule_count: u64,
}

// ---------------------------------------------------------------------------
// Manifest (TOML-backed)
// ---------------------------------------------------------------------------

/// The on-disk manifest tracking all registered artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactManifest {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// Unix timestamp (ms) of last manifest update.
    pub last_updated_ms: u64,
    /// Registered artifacts keyed by path.
    pub artifacts: Vec<ArtifactEntry>,
}

impl Default for ArtifactManifest {
    fn default() -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            last_updated_ms: 0,
            artifacts: Vec::new(),
        }
    }
}

impl ArtifactManifest {
    /// Create a new empty manifest.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a manifest from TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("manifest parse error: {e}"))
    }

    /// Serialize the manifest to TOML string.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("manifest serialize error: {e}"))
    }

    /// Find an artifact entry by path.
    #[must_use]
    pub fn find(&self, path: &str) -> Option<&ArtifactEntry> {
        self.artifacts.iter().find(|a| a.path == path)
    }

    /// Find a mutable artifact entry by path.
    pub fn find_mut(&mut self, path: &str) -> Option<&mut ArtifactEntry> {
        self.artifacts.iter_mut().find(|a| a.path == path)
    }

    /// List all active artifacts.
    #[must_use]
    pub fn active_artifacts(&self) -> Vec<&ArtifactEntry> {
        self.artifacts
            .iter()
            .filter(|a| a.status == ArtifactStatus::Active)
            .collect()
    }

    /// List all retired artifacts.
    #[must_use]
    pub fn retired_artifacts(&self) -> Vec<&ArtifactEntry> {
        self.artifacts
            .iter()
            .filter(|a| a.status == ArtifactStatus::Retired)
            .collect()
    }

    /// List artifacts filtered by sensitivity tier.
    #[must_use]
    pub fn by_tier(&self, tier: ArtifactSensitivityTier) -> Vec<&ArtifactEntry> {
        self.artifacts
            .iter()
            .filter(|a| a.sensitivity_tier == tier)
            .collect()
    }

    /// Validate manifest integrity: check for duplicate paths.
    #[must_use]
    pub fn validate(&self) -> Vec<ManifestValidationError> {
        let mut errors = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for entry in &self.artifacts {
            if !seen.insert(&entry.path) {
                errors.push(ManifestValidationError::DuplicatePath {
                    path: entry.path.clone(),
                });
            }
            if entry.sha256.len() != 64 {
                errors.push(ManifestValidationError::InvalidChecksum {
                    path: entry.path.clone(),
                    reason: format!("expected 64 hex chars, got {}", entry.sha256.len()),
                });
            }
            if entry.path.is_empty() {
                errors.push(ManifestValidationError::EmptyPath);
            }
        }

        errors
    }
}

// ---------------------------------------------------------------------------
// Manifest validation errors
// ---------------------------------------------------------------------------

/// Errors found during manifest validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManifestValidationError {
    DuplicatePath { path: String },
    InvalidChecksum { path: String, reason: String },
    EmptyPath,
    MissingFile { path: String },
    ChecksumMismatch { path: String, expected: String, actual: String },
}

// ---------------------------------------------------------------------------
// List filter
// ---------------------------------------------------------------------------

/// Filter criteria for artifact listing.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Filter by sensitivity tier.
    pub tier: Option<ArtifactSensitivityTier>,
    /// Filter by status.
    pub status: Option<ArtifactStatus>,
    /// Filter by label prefix.
    pub label_prefix: Option<String>,
}

// ---------------------------------------------------------------------------
// Prune options and result
// ---------------------------------------------------------------------------

/// Options for the prune operation.
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// Only show what would be pruned, don't actually remove.
    pub dry_run: bool,
    /// Maximum age in days for retired artifacts before pruning.
    pub max_age_days: u64,
    /// Current timestamp (ms) for age calculation.
    pub now_ms: u64,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            max_age_days: DEFAULT_RETENTION_DAYS,
            now_ms: 0,
        }
    }
}

/// Result of a prune operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PruneResult {
    /// Paths that were (or would be) pruned.
    pub pruned_paths: Vec<String>,
    /// Number of artifacts pruned.
    pub pruned_count: u64,
    /// Total bytes freed (or that would be freed).
    pub bytes_freed: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

impl PruneResult {
    /// Render a human-readable summary.
    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        if self.dry_run {
            out.push_str("DRY RUN — no files removed\n");
        }
        out.push_str(&format!(
            "Pruned: {} artifact(s), {} bytes freed\n",
            self.pruned_count, self.bytes_freed
        ));
        for p in &self.pruned_paths {
            out.push_str(&format!("  - {p}\n"));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Registry (in-memory, operates on manifest + filesystem)
// ---------------------------------------------------------------------------

/// Artifact registry providing structured lifecycle management.
///
/// The registry holds an in-memory [`ArtifactManifest`] and provides
/// operations to list, inspect, add, retire, and prune artifacts.
/// Filesystem operations (integrity checks, file deletion) use a
/// pluggable `FsBackend` trait for testability.
pub struct ArtifactRegistry {
    manifest: ArtifactManifest,
    base_dir: PathBuf,
    fs: Box<dyn FsBackend>,
}

/// Filesystem abstraction for testability.
pub trait FsBackend: Send + Sync {
    /// Read file contents as bytes.
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, String>;
    /// Check if a file exists.
    fn file_exists(&self, path: &Path) -> bool;
    /// Get file size in bytes.
    fn file_size(&self, path: &Path) -> Result<u64, String>;
    /// Remove a file.
    fn remove_file(&self, path: &Path) -> Result<(), String>;
}

/// Real filesystem backend.
pub struct RealFs;

impl FsBackend for RealFs {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
    }

    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn file_size(&self, path: &Path) -> Result<u64, String> {
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| format!("metadata {}: {e}", path.display()))
    }

    fn remove_file(&self, path: &Path) -> Result<(), String> {
        std::fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
    }
}

impl ArtifactRegistry {
    /// Create a new registry with the given manifest and base directory.
    pub fn new(manifest: ArtifactManifest, base_dir: PathBuf) -> Self {
        Self {
            manifest,
            base_dir,
            fs: Box::new(RealFs),
        }
    }

    /// Create a registry with a custom filesystem backend (for testing).
    pub fn with_fs(manifest: ArtifactManifest, base_dir: PathBuf, fs: Box<dyn FsBackend>) -> Self {
        Self {
            manifest,
            base_dir,
            fs,
        }
    }

    /// Access the current manifest.
    #[must_use]
    pub fn manifest(&self) -> &ArtifactManifest {
        &self.manifest
    }

    /// Consume the registry and return the manifest.
    #[must_use]
    pub fn into_manifest(self) -> ArtifactManifest {
        self.manifest
    }

    /// Resolve a relative path against the base directory.
    fn resolve(&self, relative: &str) -> PathBuf {
        self.base_dir.join(relative)
    }

    // ── List ─────────────────────────────────────────────────────────────

    /// List artifacts matching the given filter.
    #[must_use]
    pub fn list(&self, filter: &ListFilter) -> Vec<&ArtifactEntry> {
        self.manifest
            .artifacts
            .iter()
            .filter(|a| {
                if let Some(tier) = filter.tier {
                    if a.sensitivity_tier != tier {
                        return false;
                    }
                }
                if let Some(status) = filter.status {
                    if a.status != status {
                        return false;
                    }
                }
                if let Some(ref prefix) = filter.label_prefix {
                    if !a.label.starts_with(prefix.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    // ── Inspect ──────────────────────────────────────────────────────────

    /// Inspect a registered artifact, verifying integrity.
    pub fn inspect(&self, path: &str) -> Result<ArtifactDetail, String> {
        let entry = self
            .manifest
            .find(path)
            .ok_or_else(|| format!("artifact not found in manifest: {path}"))?
            .clone();

        let full_path = self.resolve(path);
        let file_exists = self.fs.file_exists(&full_path);

        let integrity_ok = if file_exists {
            match self.fs.read_file(&full_path) {
                Ok(bytes) => sha256_bytes(&bytes) == entry.sha256,
                Err(_) => false,
            }
        } else {
            false
        };

        Ok(ArtifactDetail {
            entry,
            integrity_ok,
            file_exists,
            decision_type_counts: BTreeMap::new(),
            time_span_ms: 0,
            pane_count: 0,
            rule_count: 0,
        })
    }

    // ── Add ──────────────────────────────────────────────────────────────

    /// Register a new artifact in the manifest.
    ///
    /// Validates that the file exists and computes its SHA-256 checksum.
    /// Rejects duplicates (same path already registered).
    pub fn add(
        &mut self,
        path: &str,
        label: &str,
        sensitivity: ArtifactSensitivityTier,
        now_ms: u64,
    ) -> Result<(), String> {
        // Reject duplicate
        if self.manifest.find(path).is_some() {
            return Err(format!("artifact already registered: {path}"));
        }

        let full_path = self.resolve(path);
        if !self.fs.file_exists(&full_path) {
            return Err(format!("file not found: {}", full_path.display()));
        }

        let bytes = self.fs.read_file(&full_path)?;
        let sha256 = sha256_bytes(&bytes);
        let size_bytes = bytes.len() as u64;

        // Try to parse as JSON array of events to get event_count / decision_count
        let (event_count, decision_count) = parse_event_counts(&bytes);

        let entry = ArtifactEntry {
            path: path.to_string(),
            label: label.to_string(),
            sha256,
            event_count,
            decision_count,
            created_at_ms: now_ms,
            sensitivity_tier: sensitivity,
            status: ArtifactStatus::Active,
            size_bytes,
            retire_reason: None,
            retired_at_ms: None,
        };

        self.manifest.artifacts.push(entry);
        self.manifest.last_updated_ms = now_ms;
        Ok(())
    }

    // ── Retire ───────────────────────────────────────────────────────────

    /// Mark an artifact as retired with a reason. Does not delete the file.
    pub fn retire(&mut self, path: &str, reason: &str, now_ms: u64) -> Result<(), String> {
        let entry = self
            .manifest
            .find_mut(path)
            .ok_or_else(|| format!("artifact not found: {path}"))?;

        if entry.status == ArtifactStatus::Retired {
            return Err(format!("artifact already retired: {path}"));
        }

        entry.status = ArtifactStatus::Retired;
        entry.retire_reason = Some(reason.to_string());
        entry.retired_at_ms = Some(now_ms);
        self.manifest.last_updated_ms = now_ms;
        Ok(())
    }

    // ── Prune ────────────────────────────────────────────────────────────

    /// Remove retired artifacts older than the retention period.
    pub fn prune(&mut self, opts: &PruneOptions) -> PruneResult {
        let max_age_ms = opts.max_age_days.saturating_mul(24 * 60 * 60 * 1000);
        let mut pruned_paths = Vec::new();
        let mut bytes_freed: u64 = 0;

        // Identify retired artifacts past retention
        let to_prune: Vec<(String, u64)> = self
            .manifest
            .artifacts
            .iter()
            .filter(|a| {
                if a.status != ArtifactStatus::Retired {
                    return false;
                }
                if let Some(retired_at) = a.retired_at_ms {
                    opts.now_ms.saturating_sub(retired_at) >= max_age_ms
                } else {
                    false
                }
            })
            .map(|a| (a.path.clone(), a.size_bytes))
            .collect();

        for (path, size) in &to_prune {
            if !opts.dry_run {
                let full_path = self.resolve(path);
                // Best-effort removal — don't fail the whole prune on one missing file
                let _ = self.fs.remove_file(&full_path);
            }
            pruned_paths.push(path.clone());
            bytes_freed += size;
        }

        if !opts.dry_run {
            // Remove pruned entries from manifest
            self.manifest
                .artifacts
                .retain(|a| !pruned_paths.contains(&a.path));
            if !pruned_paths.is_empty() {
                self.manifest.last_updated_ms = opts.now_ms;
            }
        }

        PruneResult {
            pruned_count: pruned_paths.len() as u64,
            pruned_paths,
            bytes_freed,
            dry_run: opts.dry_run,
        }
    }

    // ── Validate ─────────────────────────────────────────────────────────

    /// Validate manifest + filesystem consistency.
    #[must_use]
    pub fn validate(&self) -> Vec<ManifestValidationError> {
        let mut errors = self.manifest.validate();

        for entry in &self.manifest.artifacts {
            if entry.status == ArtifactStatus::Retired {
                continue; // retired artifacts may have been pruned
            }
            let full_path = self.resolve(&entry.path);
            if !self.fs.file_exists(&full_path) {
                errors.push(ManifestValidationError::MissingFile {
                    path: entry.path.clone(),
                });
            } else if let Ok(bytes) = self.fs.read_file(&full_path) {
                let actual = sha256_bytes(&bytes);
                if actual != entry.sha256 {
                    errors.push(ManifestValidationError::ChecksumMismatch {
                        path: entry.path.clone(),
                        expected: entry.sha256.clone(),
                        actual,
                    });
                }
            }
        }

        errors
    }

    // ── Render ───────────────────────────────────────────────────────────

    /// Render a human-readable table of artifacts.
    #[must_use]
    pub fn render_table(&self, filter: &ListFilter) -> String {
        let entries = self.list(filter);
        if entries.is_empty() {
            return "No artifacts found.\n".to_string();
        }

        let mut out = String::new();
        out.push_str(&format!(
            "{:<40} {:<15} {:<6} {:<8} {:<8}\n",
            "PATH", "LABEL", "TIER", "STATUS", "EVENTS"
        ));
        out.push_str(&"-".repeat(80));
        out.push('\n');

        for e in entries {
            let status_str = match e.status {
                ArtifactStatus::Active => "active",
                ArtifactStatus::Retired => "retired",
            };
            out.push_str(&format!(
                "{:<40} {:<15} {:<6} {:<8} {:<8}\n",
                truncate_path(&e.path, 40),
                truncate_str(&e.label, 15),
                e.sensitivity_tier.as_str(),
                status_str,
                e.event_count,
            ));
        }
        out
    }

    /// Render artifact listing as JSON.
    pub fn render_json(&self, filter: &ListFilter) -> Result<String, String> {
        let entries: Vec<&ArtifactEntry> = self.list(filter);
        serde_json::to_string_pretty(&entries)
            .map_err(|e| format!("JSON serialize error: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute SHA-256 hex digest of raw bytes.
#[must_use]
pub fn sha256_bytes(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Try to parse bytes as a JSON array of events and count event/decision types.
fn parse_event_counts(bytes: &[u8]) -> (u64, u64) {
    // Best effort: try to parse as JSON array of objects with "decision_type" field
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (0, 0);
    };
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return (0, 0);
    };
    let event_count = arr.len() as u64;
    let mut types = std::collections::HashSet::new();
    for val in &arr {
        if let Some(dt) = val.get("decision_type").and_then(|v| v.as_str()) {
            types.insert(dt.to_string());
        }
    }
    (event_count, types.len() as u64)
}

fn truncate_path(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("...{}", &s[s.len() - (max - 3)..])
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Mock filesystem for deterministic testing.
    struct MockFs {
        files: Mutex<HashMap<PathBuf, Vec<u8>>>,
    }

    impl MockFs {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
            }
        }

        fn add_file(&self, path: PathBuf, content: Vec<u8>) {
            self.files.lock().unwrap().insert(path, content);
        }
    }

    impl FsBackend for MockFs {
        fn read_file(&self, path: &Path) -> Result<Vec<u8>, String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| format!("not found: {}", path.display()))
        }

        fn file_exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }

        fn file_size(&self, path: &Path) -> Result<u64, String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .map(|b| b.len() as u64)
                .ok_or_else(|| format!("not found: {}", path.display()))
        }

        fn remove_file(&self, path: &Path) -> Result<(), String> {
            self.files
                .lock()
                .unwrap()
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| format!("not found: {}", path.display()))
        }
    }

    fn make_entry(path: &str, label: &str, content: &[u8]) -> ArtifactEntry {
        ArtifactEntry {
            path: path.to_string(),
            label: label.to_string(),
            sha256: sha256_bytes(content),
            event_count: 0,
            decision_count: 0,
            created_at_ms: 1000,
            sensitivity_tier: ArtifactSensitivityTier::T1,
            status: ArtifactStatus::Active,
            size_bytes: content.len() as u64,
            retire_reason: None,
            retired_at_ms: None,
        }
    }

    fn setup_registry(entries: Vec<ArtifactEntry>, fs: MockFs) -> ArtifactRegistry {
        let manifest = ArtifactManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            last_updated_ms: 1000,
            artifacts: entries,
        };
        ArtifactRegistry::with_fs(manifest, PathBuf::from("/base"), Box::new(fs))
    }

    // ── Manifest tests ───────────────────────────────────────────────────

    #[test]
    fn manifest_default_is_empty() {
        let m = ArtifactManifest::new();
        assert_eq!(m.schema_version, MANIFEST_SCHEMA_VERSION);
        assert!(m.artifacts.is_empty());
    }

    #[test]
    fn manifest_toml_roundtrip() {
        let mut m = ArtifactManifest::new();
        m.last_updated_ms = 5000;
        m.artifacts.push(ArtifactEntry {
            path: "test.ftreplay".into(),
            label: "test-label".into(),
            sha256: "a".repeat(64),
            event_count: 10,
            decision_count: 3,
            created_at_ms: 1000,
            sensitivity_tier: ArtifactSensitivityTier::T1,
            status: ArtifactStatus::Active,
            size_bytes: 512,
            retire_reason: None,
            retired_at_ms: None,
        });
        let toml_str = m.to_toml().unwrap();
        let restored = ArtifactManifest::from_toml(&toml_str).unwrap();
        assert_eq!(restored.artifacts.len(), 1);
        assert_eq!(restored.artifacts[0].path, "test.ftreplay");
        assert_eq!(restored.schema_version, MANIFEST_SCHEMA_VERSION);
    }

    #[test]
    fn manifest_find() {
        let mut m = ArtifactManifest::new();
        let content = b"hello";
        m.artifacts.push(make_entry("a.ftreplay", "alpha", content));
        m.artifacts.push(make_entry("b.ftreplay", "beta", content));
        assert!(m.find("a.ftreplay").is_some());
        assert!(m.find("c.ftreplay").is_none());
    }

    #[test]
    fn manifest_active_and_retired() {
        let mut m = ArtifactManifest::new();
        let content = b"data";
        let e1 = make_entry("active.ftreplay", "a", content);
        let mut e2 = make_entry("retired.ftreplay", "r", content);
        e2.status = ArtifactStatus::Retired;
        e2.retire_reason = Some("obsolete".into());
        m.artifacts.push(e1);
        m.artifacts.push(e2);
        assert_eq!(m.active_artifacts().len(), 1);
        assert_eq!(m.retired_artifacts().len(), 1);
    }

    #[test]
    fn manifest_by_tier() {
        let mut m = ArtifactManifest::new();
        let content = b"x";
        let e1 = make_entry("t1.ftreplay", "t1", content);
        let mut e2 = make_entry("t2.ftreplay", "t2", content);
        e2.sensitivity_tier = ArtifactSensitivityTier::T2;
        m.artifacts.push(e1);
        m.artifacts.push(e2);
        assert_eq!(m.by_tier(ArtifactSensitivityTier::T1).len(), 1);
        assert_eq!(m.by_tier(ArtifactSensitivityTier::T2).len(), 1);
        assert_eq!(m.by_tier(ArtifactSensitivityTier::T3).len(), 0);
    }

    #[test]
    fn manifest_validate_duplicate_path() {
        let mut m = ArtifactManifest::new();
        let content = b"dup";
        m.artifacts.push(make_entry("dup.ftreplay", "d1", content));
        m.artifacts.push(make_entry("dup.ftreplay", "d2", content));
        let errors = m.validate();
        assert!(errors.iter().any(|e| matches!(e, ManifestValidationError::DuplicatePath { .. })));
    }

    #[test]
    fn manifest_validate_bad_checksum_length() {
        let mut m = ArtifactManifest::new();
        let mut entry = make_entry("bad.ftreplay", "bad", b"x");
        entry.sha256 = "short".into();
        m.artifacts.push(entry);
        let errors = m.validate();
        assert!(errors.iter().any(|e| matches!(e, ManifestValidationError::InvalidChecksum { .. })));
    }

    #[test]
    fn manifest_validate_empty_path() {
        let mut m = ArtifactManifest::new();
        let entry = make_entry("", "empty", b"x");
        m.artifacts.push(entry);
        let errors = m.validate();
        assert!(errors.iter().any(|e| matches!(e, ManifestValidationError::EmptyPath)));
    }

    // ── Sensitivity tier tests ───────────────────────────────────────────

    #[test]
    fn tier_ordering() {
        assert!(ArtifactSensitivityTier::T1 < ArtifactSensitivityTier::T2);
        assert!(ArtifactSensitivityTier::T2 < ArtifactSensitivityTier::T3);
    }

    #[test]
    fn tier_from_str() {
        assert_eq!(ArtifactSensitivityTier::from_str_arg("T1"), Some(ArtifactSensitivityTier::T1));
        assert_eq!(ArtifactSensitivityTier::from_str_arg("t2"), Some(ArtifactSensitivityTier::T2));
        assert_eq!(ArtifactSensitivityTier::from_str_arg("T3"), Some(ArtifactSensitivityTier::T3));
        assert_eq!(ArtifactSensitivityTier::from_str_arg("T4"), None);
    }

    #[test]
    fn tier_as_str() {
        assert_eq!(ArtifactSensitivityTier::T1.as_str(), "T1");
        assert_eq!(ArtifactSensitivityTier::T2.as_str(), "T2");
        assert_eq!(ArtifactSensitivityTier::T3.as_str(), "T3");
    }

    #[test]
    fn tier_serde_roundtrip() {
        for tier in [ArtifactSensitivityTier::T1, ArtifactSensitivityTier::T2, ArtifactSensitivityTier::T3] {
            let json = serde_json::to_string(&tier).unwrap();
            let restored: ArtifactSensitivityTier = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, tier);
        }
    }

    // ── Status tests ─────────────────────────────────────────────────────

    #[test]
    fn status_serde_roundtrip() {
        for s in [ArtifactStatus::Active, ArtifactStatus::Retired] {
            let json = serde_json::to_string(&s).unwrap();
            let restored: ArtifactStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, s);
        }
    }

    // ── Registry list tests ──────────────────────────────────────────────

    #[test]
    fn list_no_filter() {
        let content = b"test";
        let fs = MockFs::new();
        let reg = setup_registry(
            vec![
                make_entry("a.ftreplay", "alpha", content),
                make_entry("b.ftreplay", "beta", content),
            ],
            fs,
        );
        assert_eq!(reg.list(&ListFilter::default()).len(), 2);
    }

    #[test]
    fn list_filter_by_tier() {
        let content = b"test";
        let e1 = make_entry("a.ftreplay", "alpha", content);
        let mut e2 = make_entry("b.ftreplay", "beta", content);
        e2.sensitivity_tier = ArtifactSensitivityTier::T2;
        let fs = MockFs::new();
        let reg = setup_registry(vec![e1, e2], fs);
        let filter = ListFilter {
            tier: Some(ArtifactSensitivityTier::T1),
            ..Default::default()
        };
        assert_eq!(reg.list(&filter).len(), 1);
    }

    #[test]
    fn list_filter_by_status() {
        let content = b"test";
        let e1 = make_entry("a.ftreplay", "active", content);
        let mut e2 = make_entry("b.ftreplay", "retired", content);
        e2.status = ArtifactStatus::Retired;
        let fs = MockFs::new();
        let reg = setup_registry(vec![e1, e2], fs);
        let filter = ListFilter {
            status: Some(ArtifactStatus::Active),
            ..Default::default()
        };
        assert_eq!(reg.list(&filter).len(), 1);
    }

    #[test]
    fn list_filter_by_label_prefix() {
        let content = b"test";
        let fs = MockFs::new();
        let reg = setup_registry(
            vec![
                make_entry("a.ftreplay", "auth_login", content),
                make_entry("b.ftreplay", "auth_logout", content),
                make_entry("c.ftreplay", "payment_flow", content),
            ],
            fs,
        );
        let filter = ListFilter {
            label_prefix: Some("auth".to_string()),
            ..Default::default()
        };
        assert_eq!(reg.list(&filter).len(), 2);
    }

    #[test]
    fn list_empty_registry() {
        let fs = MockFs::new();
        let reg = setup_registry(vec![], fs);
        assert_eq!(reg.list(&ListFilter::default()).len(), 0);
    }

    // ── Registry inspect tests ───────────────────────────────────────────

    #[test]
    fn inspect_existing_valid() {
        let content = b"valid content";
        let entry = make_entry("test.ftreplay", "test", content);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/test.ftreplay"), content.to_vec());
        let reg = setup_registry(vec![entry], fs);
        let detail = reg.inspect("test.ftreplay").unwrap();
        assert!(detail.integrity_ok);
        assert!(detail.file_exists);
        assert_eq!(detail.entry.label, "test");
    }

    #[test]
    fn inspect_checksum_mismatch() {
        let content = b"original";
        let entry = make_entry("test.ftreplay", "test", content);
        let fs = MockFs::new();
        // Put different content on disk
        fs.add_file(PathBuf::from("/base/test.ftreplay"), b"modified".to_vec());
        let reg = setup_registry(vec![entry], fs);
        let detail = reg.inspect("test.ftreplay").unwrap();
        assert!(!detail.integrity_ok);
        assert!(detail.file_exists);
    }

    #[test]
    fn inspect_missing_file() {
        let content = b"ghost";
        let entry = make_entry("ghost.ftreplay", "ghost", content);
        let fs = MockFs::new();
        // Don't add file to MockFs
        let reg = setup_registry(vec![entry], fs);
        let detail = reg.inspect("ghost.ftreplay").unwrap();
        assert!(!detail.integrity_ok);
        assert!(!detail.file_exists);
    }

    #[test]
    fn inspect_not_in_manifest() {
        let fs = MockFs::new();
        let reg = setup_registry(vec![], fs);
        assert!(reg.inspect("missing.ftreplay").is_err());
    }

    // ── Registry add tests ───────────────────────────────────────────────

    #[test]
    fn add_new_artifact() {
        let content = b"new artifact data";
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/new.ftreplay"), content.to_vec());
        let mut reg = setup_registry(vec![], fs);
        reg.add("new.ftreplay", "new-label", ArtifactSensitivityTier::T1, 2000)
            .unwrap();
        assert_eq!(reg.manifest().artifacts.len(), 1);
        assert_eq!(reg.manifest().artifacts[0].path, "new.ftreplay");
        assert_eq!(reg.manifest().artifacts[0].label, "new-label");
        assert_eq!(reg.manifest().artifacts[0].sha256, sha256_bytes(content));
        assert_eq!(reg.manifest().artifacts[0].size_bytes, content.len() as u64);
    }

    #[test]
    fn add_rejects_duplicate() {
        let content = b"existing";
        let entry = make_entry("dup.ftreplay", "dup", content);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/dup.ftreplay"), content.to_vec());
        let mut reg = setup_registry(vec![entry], fs);
        let result = reg.add("dup.ftreplay", "dup2", ArtifactSensitivityTier::T1, 2000);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn add_rejects_missing_file() {
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![], fs);
        let result = reg.add("missing.ftreplay", "m", ArtifactSensitivityTier::T1, 2000);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn add_updates_timestamp() {
        let content = b"data";
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/ts.ftreplay"), content.to_vec());
        let mut reg = setup_registry(vec![], fs);
        reg.add("ts.ftreplay", "ts", ArtifactSensitivityTier::T1, 9999)
            .unwrap();
        assert_eq!(reg.manifest().last_updated_ms, 9999);
    }

    #[test]
    fn add_parses_json_events() {
        let events = r#"[
            {"decision_type": "pattern_match", "rule_id": "r1"},
            {"decision_type": "workflow_step", "rule_id": "r2"},
            {"decision_type": "pattern_match", "rule_id": "r3"}
        ]"#;
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/events.ftreplay"), events.as_bytes().to_vec());
        let mut reg = setup_registry(vec![], fs);
        reg.add("events.ftreplay", "events", ArtifactSensitivityTier::T1, 3000)
            .unwrap();
        assert_eq!(reg.manifest().artifacts[0].event_count, 3);
        assert_eq!(reg.manifest().artifacts[0].decision_count, 2); // pattern_match + workflow_step
    }

    // ── Registry retire tests ────────────────────────────────────────────

    #[test]
    fn retire_marks_artifact() {
        let content = b"retiring";
        let entry = make_entry("old.ftreplay", "old", content);
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![entry], fs);
        reg.retire("old.ftreplay", "replaced by new version", 5000)
            .unwrap();
        let e = reg.manifest().find("old.ftreplay").unwrap();
        assert_eq!(e.status, ArtifactStatus::Retired);
        assert_eq!(e.retire_reason.as_deref(), Some("replaced by new version"));
        assert_eq!(e.retired_at_ms, Some(5000));
    }

    #[test]
    fn retire_does_not_delete_file() {
        let content = b"keep";
        let entry = make_entry("keep.ftreplay", "keep", content);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/keep.ftreplay"), content.to_vec());
        let mut reg = setup_registry(vec![entry], fs);
        reg.retire("keep.ftreplay", "reason", 5000).unwrap();
        // File should still be accessible via inspect
        let detail = reg.inspect("keep.ftreplay").unwrap();
        assert!(detail.file_exists);
    }

    #[test]
    fn retire_not_found() {
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![], fs);
        let result = reg.retire("missing.ftreplay", "reason", 5000);
        assert!(result.is_err());
    }

    #[test]
    fn retire_already_retired() {
        let content = b"already";
        let mut entry = make_entry("already.ftreplay", "a", content);
        entry.status = ArtifactStatus::Retired;
        entry.retire_reason = Some("first".into());
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![entry], fs);
        let result = reg.retire("already.ftreplay", "second", 6000);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already retired"));
    }

    // ── Registry prune tests ─────────────────────────────────────────────

    #[test]
    fn prune_removes_old_retired() {
        let content = b"old retired";
        let mut entry = make_entry("old.ftreplay", "old", content);
        entry.status = ArtifactStatus::Retired;
        entry.retired_at_ms = Some(1000);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/old.ftreplay"), content.to_vec());
        let mut reg = setup_registry(vec![entry], fs);

        let result = reg.prune(&PruneOptions {
            dry_run: false,
            max_age_days: 1,
            now_ms: 1000 + 2 * 24 * 60 * 60 * 1000, // 2 days later
        });
        assert_eq!(result.pruned_count, 1);
        assert_eq!(result.pruned_paths, vec!["old.ftreplay"]);
        assert!(reg.manifest().artifacts.is_empty());
    }

    #[test]
    fn prune_keeps_recent_retired() {
        let content = b"recent";
        let mut entry = make_entry("recent.ftreplay", "recent", content);
        entry.status = ArtifactStatus::Retired;
        entry.retired_at_ms = Some(1000);
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![entry], fs);

        let result = reg.prune(&PruneOptions {
            dry_run: false,
            max_age_days: 30,
            now_ms: 1000 + 24 * 60 * 60 * 1000, // 1 day later
        });
        assert_eq!(result.pruned_count, 0);
        assert_eq!(reg.manifest().artifacts.len(), 1);
    }

    #[test]
    fn prune_skips_active() {
        let content = b"active";
        let entry = make_entry("active.ftreplay", "active", content);
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![entry], fs);

        let result = reg.prune(&PruneOptions {
            dry_run: false,
            max_age_days: 0,
            now_ms: 999_999_999,
        });
        assert_eq!(result.pruned_count, 0);
    }

    #[test]
    fn prune_dry_run_no_delete() {
        let content = b"dry run";
        let mut entry = make_entry("dry.ftreplay", "dry", content);
        entry.status = ArtifactStatus::Retired;
        entry.retired_at_ms = Some(1000);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/dry.ftreplay"), content.to_vec());
        let mut reg = setup_registry(vec![entry], fs);

        let result = reg.prune(&PruneOptions {
            dry_run: true,
            max_age_days: 1,
            now_ms: 1000 + 2 * 24 * 60 * 60 * 1000,
        });
        assert_eq!(result.pruned_count, 1);
        assert!(result.dry_run);
        // Manifest should still have the entry
        assert_eq!(reg.manifest().artifacts.len(), 1);
    }

    #[test]
    fn prune_bytes_freed() {
        let content = b"12345678"; // 8 bytes
        let mut entry = make_entry("sized.ftreplay", "sized", content);
        entry.status = ArtifactStatus::Retired;
        entry.retired_at_ms = Some(1000);
        let fs = MockFs::new();
        let mut reg = setup_registry(vec![entry], fs);

        let result = reg.prune(&PruneOptions {
            dry_run: false,
            max_age_days: 0,
            now_ms: 1000 + 24 * 60 * 60 * 1000,
        });
        assert_eq!(result.bytes_freed, 8);
    }

    // ── Registry validate tests ──────────────────────────────────────────

    #[test]
    fn validate_detects_missing_file() {
        let content = b"exists";
        let entry = make_entry("missing_on_disk.ftreplay", "m", content);
        let fs = MockFs::new();
        let reg = setup_registry(vec![entry], fs);
        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(e, ManifestValidationError::MissingFile { .. })));
    }

    #[test]
    fn validate_detects_checksum_mismatch() {
        let content = b"original";
        let entry = make_entry("bad_hash.ftreplay", "bad", content);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/bad_hash.ftreplay"), b"tampered".to_vec());
        let reg = setup_registry(vec![entry], fs);
        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(e, ManifestValidationError::ChecksumMismatch { .. })));
    }

    #[test]
    fn validate_passes_for_valid() {
        let content = b"valid";
        let entry = make_entry("valid.ftreplay", "valid", content);
        let fs = MockFs::new();
        fs.add_file(PathBuf::from("/base/valid.ftreplay"), content.to_vec());
        let reg = setup_registry(vec![entry], fs);
        let errors = reg.validate();
        assert!(errors.is_empty());
    }

    // ── Render tests ─────────────────────────────────────────────────────

    #[test]
    fn render_table_empty() {
        let fs = MockFs::new();
        let reg = setup_registry(vec![], fs);
        let table = reg.render_table(&ListFilter::default());
        assert!(table.contains("No artifacts found"));
    }

    #[test]
    fn render_table_has_header() {
        let content = b"data";
        let entry = make_entry("a.ftreplay", "alpha", content);
        let fs = MockFs::new();
        let reg = setup_registry(vec![entry], fs);
        let table = reg.render_table(&ListFilter::default());
        assert!(table.contains("PATH"));
        assert!(table.contains("LABEL"));
        assert!(table.contains("TIER"));
        assert!(table.contains("a.ftreplay"));
    }

    #[test]
    fn render_json_valid() {
        let content = b"json test";
        let entry = make_entry("j.ftreplay", "jj", content);
        let fs = MockFs::new();
        let reg = setup_registry(vec![entry], fs);
        let json = reg.render_json(&ListFilter::default()).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    // ── SHA-256 tests ────────────────────────────────────────────────────

    #[test]
    fn sha256_deterministic() {
        let a = sha256_bytes(b"hello");
        let b = sha256_bytes(b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn sha256_different_inputs() {
        let a = sha256_bytes(b"hello");
        let b = sha256_bytes(b"world");
        assert_ne!(a, b);
    }

    // ── Prune result render ──────────────────────────────────────────────

    #[test]
    fn prune_result_render_dry_run() {
        let result = PruneResult {
            pruned_paths: vec!["a.ftreplay".into()],
            pruned_count: 1,
            bytes_freed: 100,
            dry_run: true,
        };
        let text = result.render_human();
        assert!(text.contains("DRY RUN"));
        assert!(text.contains("a.ftreplay"));
    }

    #[test]
    fn prune_result_render_real() {
        let result = PruneResult {
            pruned_paths: vec!["x.ftreplay".into()],
            pruned_count: 1,
            bytes_freed: 256,
            dry_run: false,
        };
        let text = result.render_human();
        assert!(!text.contains("DRY RUN"));
        assert!(text.contains("256 bytes"));
    }

    // ── Entry serde roundtrip ────────────────────────────────────────────

    #[test]
    fn entry_serde_roundtrip() {
        let entry = ArtifactEntry {
            path: "test.ftreplay".into(),
            label: "test".into(),
            sha256: "a".repeat(64),
            event_count: 42,
            decision_count: 5,
            created_at_ms: 1000,
            sensitivity_tier: ArtifactSensitivityTier::T2,
            status: ArtifactStatus::Active,
            size_bytes: 1024,
            retire_reason: None,
            retired_at_ms: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ArtifactEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.path, "test.ftreplay");
        assert_eq!(restored.sensitivity_tier, ArtifactSensitivityTier::T2);
    }

    // ── Parse event counts ───────────────────────────────────────────────

    #[test]
    fn parse_event_counts_valid_json() {
        let json = r#"[
            {"decision_type": "pattern_match"},
            {"decision_type": "workflow_step"},
            {"decision_type": "pattern_match"}
        ]"#;
        let (events, types) = parse_event_counts(json.as_bytes());
        assert_eq!(events, 3);
        assert_eq!(types, 2);
    }

    #[test]
    fn parse_event_counts_invalid_json() {
        let (events, types) = parse_event_counts(b"not json");
        assert_eq!(events, 0);
        assert_eq!(types, 0);
    }

    #[test]
    fn parse_event_counts_empty_array() {
        let (events, types) = parse_event_counts(b"[]");
        assert_eq!(events, 0);
        assert_eq!(types, 0);
    }
}
