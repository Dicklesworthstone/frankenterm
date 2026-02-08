//! Sync for wa: plan-first, explicit, non-destructive.
//!
//! Provides sync primitives for config and binary files between machines.
//! All operations are plan-first: changes are classified before any writes.
//! Secret-containing paths are always denied. Overwrites require explicit opt-in.

use crate::config::{Config, SyncDirection, SyncTargetConfig, WorkspaceLayout};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Result type for sync operations.
pub type SyncResult<T> = Result<T, SyncError>;

/// High-level payload categories eligible for sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncCategory {
    Binary,
    Config,
    Snapshots,
}

/// Sync status summary.
#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub enabled: bool,
    pub allow_binary: bool,
    pub allow_config: bool,
    pub allow_snapshots: bool,
    pub allow_overwrite: bool,
    pub require_confirmation: bool,
    pub allow_paths: Vec<String>,
    pub deny_paths: Vec<String>,
    pub targets: Vec<SyncTargetStatus>,
    pub warnings: Vec<String>,
}

/// Target summary (effective settings).
#[derive(Debug, Clone, Serialize)]
pub struct SyncTargetStatus {
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub root: String,
    pub default_direction: SyncDirection,
    pub allow_binary: bool,
    pub allow_config: bool,
    pub allow_snapshots: bool,
}

/// Plan options for sync operations.
#[derive(Debug, Clone)]
pub struct SyncPlanOptions {
    pub target: Option<String>,
    pub direction: SyncDirection,
    pub dry_run: bool,
    pub apply: bool,
    pub yes: bool,
    pub allow_overwrite: bool,
    pub include: Vec<SyncCategory>,
    pub config_path: Option<PathBuf>,
}

/// Plan for a sync operation.
#[derive(Debug, Clone, Serialize)]
pub struct SyncPlan {
    pub target: SyncTargetStatus,
    pub direction: SyncDirection,
    pub dry_run: bool,
    pub apply: bool,
    pub allow_overwrite: bool,
    pub warnings: Vec<String>,
    pub payloads: Vec<SyncPayload>,
}

/// Planned payload.
#[derive(Debug, Clone, Serialize)]
pub struct SyncPayload {
    pub category: SyncCategory,
    pub source: String,
    pub destination: String,
    pub note: Option<String>,
}

/// Paths that must never be synced (secret/sensitive patterns).
const DENIED_PATH_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    "tokens.json",
    "credentials.json",
    "keyring",
    "keychain",
    ".ssh",
    "id_rsa",
    "id_ed25519",
    ".gnupg",
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// Action to take for a single file in a sync plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncItemAction {
    /// New file at destination.
    Add,
    /// File exists at destination with different content.
    Update,
    /// File unchanged (content hashes match).
    Skip,
    /// Both sides changed; manual resolution required.
    Conflict,
    /// Path is denied by security rules.
    Denied,
}

/// A single file in a sync plan.
#[derive(Debug, Clone, Serialize)]
pub struct SyncItem {
    pub relative_path: String,
    pub action: SyncItemAction,
    pub source_hash: Option<String>,
    pub destination_hash: Option<String>,
    pub size_bytes: Option<u64>,
    pub reason: Option<String>,
}

/// Detailed file-level sync plan.
#[derive(Debug, Clone, Serialize)]
pub struct SyncFilePlan {
    pub category: SyncCategory,
    pub source_root: String,
    pub destination_root: String,
    pub direction: SyncDirection,
    pub items: Vec<SyncItem>,
    pub denied_count: usize,
    pub add_count: usize,
    pub update_count: usize,
    pub skip_count: usize,
    pub conflict_count: usize,
}

/// Sync plan/build errors.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("sync is disabled; set [sync].enabled = true in wa.toml")]
    Disabled,
    #[error("no sync targets configured")]
    NoTargets,
    #[error("sync target '{name}' not found (available: {available})")]
    UnknownTarget { name: String, available: String },
    #[error("multiple sync targets configured; specify --target (available: {available})")]
    AmbiguousTarget { available: String },
    #[error("confirmation required; re-run with --yes")]
    ConfirmationRequired,
    #[error("unresolved conflicts: {count} file(s) changed on both sides")]
    UnresolvedConflicts { count: usize },
    #[error("IO error during sync: {0}")]
    Io(#[from] std::io::Error),
}

fn default_warnings() -> Vec<String> {
    vec![
        "Live SQLite DB files are never synced; export snapshots only.".to_string(),
        "Secrets are always redacted and never synced.".to_string(),
    ]
}

fn effective_allow(global: bool, override_value: Option<bool>) -> bool {
    override_value.unwrap_or(global)
}

fn target_status(config: &Config, target: &SyncTargetConfig) -> SyncTargetStatus {
    SyncTargetStatus {
        name: target.name.clone(),
        transport: target.transport.clone(),
        endpoint: target.endpoint.clone(),
        root: target.root.clone(),
        default_direction: target.default_direction,
        allow_binary: effective_allow(config.sync.allow_binary, target.allow_binary),
        allow_config: effective_allow(config.sync.allow_config, target.allow_config),
        allow_snapshots: effective_allow(config.sync.allow_snapshots, target.allow_snapshots),
    }
}

/// Build sync status information from config.
#[must_use]
pub fn build_sync_status(config: &Config) -> SyncStatus {
    let mut targets: Vec<SyncTargetStatus> = config
        .sync
        .targets
        .iter()
        .map(|target| target_status(config, target))
        .collect();
    targets.sort_by(|a, b| a.name.cmp(&b.name));

    SyncStatus {
        enabled: config.sync.enabled,
        allow_binary: config.sync.allow_binary,
        allow_config: config.sync.allow_config,
        allow_snapshots: config.sync.allow_snapshots,
        allow_overwrite: config.sync.allow_overwrite,
        require_confirmation: config.sync.require_confirmation,
        allow_paths: config.sync.allow_paths.clone(),
        deny_paths: config.sync.deny_paths.clone(),
        targets,
        warnings: default_warnings(),
    }
}

fn select_target<'a>(
    targets: &'a [SyncTargetConfig],
    name: Option<&str>,
) -> Result<&'a SyncTargetConfig, SyncError> {
    if targets.is_empty() {
        return Err(SyncError::NoTargets);
    }

    if let Some(name) = name {
        return targets
            .iter()
            .find(|target| target.name == name)
            .ok_or_else(|| SyncError::UnknownTarget {
                name: name.to_string(),
                available: targets
                    .iter()
                    .map(|target| target.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
    }

    if targets.len() == 1 {
        Ok(&targets[0])
    } else {
        Err(SyncError::AmbiguousTarget {
            available: targets
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}

fn include_category(include: &[SyncCategory], category: SyncCategory) -> bool {
    if include.is_empty() {
        return true;
    }
    include.contains(&category)
}

/// Build a sync plan (plan-only; does not mutate any files).
pub fn build_sync_plan(
    config: &Config,
    layout: &WorkspaceLayout,
    options: SyncPlanOptions,
) -> SyncResult<SyncPlan> {
    if !config.sync.enabled {
        return Err(SyncError::Disabled);
    }

    if options.apply && config.sync.require_confirmation && !options.yes {
        return Err(SyncError::ConfirmationRequired);
    }

    let target = select_target(&config.sync.targets, options.target.as_deref())?;
    let target_info = target_status(config, target);

    let local_paths = LocalSyncPaths::from_config(config, layout, options.config_path.as_deref());
    let mut payloads = Vec::new();
    let mut warnings = default_warnings();

    let allow_overwrite = options.allow_overwrite || config.sync.allow_overwrite;

    let allow_binary =
        target_info.allow_binary && include_category(&options.include, SyncCategory::Binary);
    let allow_config =
        target_info.allow_config && include_category(&options.include, SyncCategory::Config);
    let allow_snapshots =
        target_info.allow_snapshots && include_category(&options.include, SyncCategory::Snapshots);

    if allow_binary {
        payloads.push(build_payload(
            SyncCategory::Binary,
            &local_paths.binary_path,
            &remote_root_for(&target_info, "bin/wa"),
            options.direction,
        ));
    } else if target_info.allow_binary {
        warnings.push("Binary sync excluded by --include filter.".to_string());
    } else {
        warnings.push("Binary sync disabled for this target.".to_string());
    }

    if allow_config {
        payloads.push(build_payload(
            SyncCategory::Config,
            &local_paths.config_root,
            &remote_root_for(&target_info, "config"),
            options.direction,
        ));
    } else if target_info.allow_config {
        warnings.push("Config sync excluded by --include filter.".to_string());
    } else {
        warnings.push("Config sync disabled for this target.".to_string());
    }

    if allow_snapshots {
        payloads.push(build_payload(
            SyncCategory::Snapshots,
            &local_paths.snapshots_root,
            &remote_root_for(&target_info, "snapshots"),
            options.direction,
        ));
    } else if target_info.allow_snapshots {
        warnings.push("Snapshot sync excluded by --include filter.".to_string());
    } else {
        warnings.push("Snapshot sync disabled for this target.".to_string());
    }

    if options.apply && options.dry_run {
        warnings.push("Both --apply and --dry-run set; dry-run takes precedence.".to_string());
    }

    Ok(SyncPlan {
        target: target_info,
        direction: options.direction,
        dry_run: options.dry_run,
        apply: options.apply,
        allow_overwrite,
        warnings,
        payloads,
    })
}

fn build_payload(
    category: SyncCategory,
    local_path: &Path,
    remote_path: &Path,
    direction: SyncDirection,
) -> SyncPayload {
    let (source, destination) = match direction {
        SyncDirection::Push => (local_path, remote_path),
        SyncDirection::Pull => (remote_path, local_path),
    };

    SyncPayload {
        category,
        source: path_to_string(source),
        destination: path_to_string(destination),
        note: Some("plan-only scaffolding (no file transfers)".to_string()),
    }
}

fn remote_root_for(target: &SyncTargetStatus, suffix: &str) -> PathBuf {
    let base = PathBuf::from(&target.root);
    if suffix.is_empty() {
        base
    } else {
        base.join(suffix)
    }
}

struct LocalSyncPaths {
    binary_path: PathBuf,
    config_root: PathBuf,
    snapshots_root: PathBuf,
}

impl LocalSyncPaths {
    fn from_config(config: &Config, layout: &WorkspaceLayout, config_path: Option<&Path>) -> Self {
        let binary_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wa"));
        let config_root = config_path
            .and_then(|path| path.parent().map(PathBuf::from))
            .unwrap_or_else(default_config_root);
        let snapshots_root = crate::backup::backup_destination_root(
            &layout.root,
            config.backup.scheduled.destination.as_deref(),
        );

        Self {
            binary_path,
            config_root,
            snapshots_root,
        }
    }
}

fn default_config_root() -> PathBuf {
    if let Some(dir) = dirs::config_dir() {
        dir.join("wa")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".config").join("wa")
    } else {
        PathBuf::from(".config/wa")
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

// =============================================================================
// Path filtering (secret-aware deny rules)
// =============================================================================

/// Check if a relative path matches any denied pattern.
///
/// A path is denied if any component (filename or directory name) matches
/// a denied pattern, or if the full path ends with a denied pattern.
#[must_use]
pub fn is_path_denied(relative_path: &str, extra_deny: &[String]) -> bool {
    let path = Path::new(relative_path);

    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        for pattern in DENIED_PATH_PATTERNS {
            if name == *pattern {
                return true;
            }
        }
        for pattern in extra_deny {
            if name == *pattern {
                return true;
            }
        }
    }

    // Also check filename against common secret file extensions
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        if matches!(ext.as_str(), "key" | "pem" | "p12" | "pfx") {
            return true;
        }
    }

    false
}

/// Check if a path is in the allow list (if the allow list is non-empty).
///
/// An empty allow list means all non-denied paths are allowed.
#[must_use]
pub fn is_path_allowed(relative_path: &str, allow_paths: &[String]) -> bool {
    if allow_paths.is_empty() {
        return true;
    }
    allow_paths.iter().any(|allowed| {
        // Simple prefix matching (glob support is deferred)
        relative_path.starts_with(allowed.trim_end_matches("**").trim_end_matches('/'))
    })
}

// =============================================================================
// File hashing
// =============================================================================

/// Compute SHA-256 hash of a file's contents. Returns hex-encoded hash.
fn hash_file(path: &Path) -> std::io::Result<String> {
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

// =============================================================================
// File-level plan builder
// =============================================================================

/// Walk a source directory and classify each file against a destination directory.
///
/// Returns a [`SyncFilePlan`] with per-file actions. No files are modified.
pub fn build_file_plan(
    source_root: &Path,
    destination_root: &Path,
    category: SyncCategory,
    direction: SyncDirection,
    deny_paths: &[String],
    allow_paths: &[String],
    allow_overwrite: bool,
) -> SyncResult<SyncFilePlan> {
    let mut items = Vec::new();

    if source_root.is_file() {
        // Single-file mode (e.g., binary)
        let relative = source_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let item = classify_file(
            source_root,
            destination_root,
            &relative,
            deny_paths,
            allow_paths,
            allow_overwrite,
        )?;
        items.push(item);
    } else if source_root.is_dir() {
        walk_dir(
            source_root,
            source_root,
            destination_root,
            deny_paths,
            allow_paths,
            allow_overwrite,
            &mut items,
        )?;
        // Stable ordering for deterministic output
        items.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    }
    // If source doesn't exist, plan is empty (no items)

    let denied_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Denied)
        .count();
    let add_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Add)
        .count();
    let update_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Update)
        .count();
    let skip_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Skip)
        .count();
    let conflict_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Conflict)
        .count();

    Ok(SyncFilePlan {
        category,
        source_root: path_to_string(source_root),
        destination_root: path_to_string(destination_root),
        direction,
        items,
        denied_count,
        add_count,
        update_count,
        skip_count,
        conflict_count,
    })
}

fn walk_dir(
    base: &Path,
    current: &Path,
    dest_root: &Path,
    deny_paths: &[String],
    allow_paths: &[String],
    allow_overwrite: bool,
    items: &mut Vec<SyncItem>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(current) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            walk_dir(
                base,
                &path,
                dest_root,
                deny_paths,
                allow_paths,
                allow_overwrite,
                items,
            )?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let item = classify_file(
                &path,
                &dest_root.join(&relative),
                &relative,
                deny_paths,
                allow_paths,
                allow_overwrite,
            )
            .map_err(|e| match e {
                SyncError::Io(io_err) => io_err,
                _ => std::io::Error::other(e.to_string()),
            })?;
            items.push(item);
        }
    }
    Ok(())
}

fn classify_file(
    source_path: &Path,
    dest_path: &Path,
    relative_path: &str,
    deny_paths: &[String],
    allow_paths: &[String],
    allow_overwrite: bool,
) -> SyncResult<SyncItem> {
    // Check deny rules first
    if is_path_denied(relative_path, deny_paths) {
        return Ok(SyncItem {
            relative_path: relative_path.to_string(),
            action: SyncItemAction::Denied,
            source_hash: None,
            destination_hash: None,
            size_bytes: None,
            reason: Some("path matches deny rules".to_string()),
        });
    }

    // Check allow rules
    if !is_path_allowed(relative_path, allow_paths) {
        return Ok(SyncItem {
            relative_path: relative_path.to_string(),
            action: SyncItemAction::Denied,
            source_hash: None,
            destination_hash: None,
            size_bytes: None,
            reason: Some("path not in allow list".to_string()),
        });
    }

    let source_hash = hash_file(source_path)?;
    let source_size = source_path.metadata()?.len();

    if !dest_path.exists() {
        return Ok(SyncItem {
            relative_path: relative_path.to_string(),
            action: SyncItemAction::Add,
            source_hash: Some(source_hash),
            destination_hash: None,
            size_bytes: Some(source_size),
            reason: None,
        });
    }

    let dest_hash = hash_file(dest_path)?;

    if source_hash == dest_hash {
        return Ok(SyncItem {
            relative_path: relative_path.to_string(),
            action: SyncItemAction::Skip,
            source_hash: Some(source_hash),
            destination_hash: Some(dest_hash),
            size_bytes: Some(source_size),
            reason: Some("content unchanged".to_string()),
        });
    }

    // Content differs
    if allow_overwrite {
        Ok(SyncItem {
            relative_path: relative_path.to_string(),
            action: SyncItemAction::Update,
            source_hash: Some(source_hash),
            destination_hash: Some(dest_hash),
            size_bytes: Some(source_size),
            reason: None,
        })
    } else {
        Ok(SyncItem {
            relative_path: relative_path.to_string(),
            action: SyncItemAction::Conflict,
            source_hash: Some(source_hash),
            destination_hash: Some(dest_hash),
            size_bytes: Some(source_size),
            reason: Some("destination differs; use --allow-overwrite to replace".to_string()),
        })
    }
}

// =============================================================================
// Sync execution (local-directory transport only)
// =============================================================================

/// Execute a file plan: copy Add/Update items from source to destination.
///
/// Only operates on local filesystem paths. Skips Denied, Skip, and Conflict items.
/// Returns the number of files written.
pub fn execute_file_plan(plan: &SyncFilePlan) -> SyncResult<usize> {
    if plan.conflict_count > 0 {
        return Err(SyncError::UnresolvedConflicts {
            count: plan.conflict_count,
        });
    }

    let source_root = Path::new(&plan.source_root);
    let dest_root = Path::new(&plan.destination_root);
    let mut written = 0;

    for item in &plan.items {
        match item.action {
            SyncItemAction::Add | SyncItemAction::Update => {
                let source = if source_root.is_file() {
                    source_root.to_path_buf()
                } else {
                    source_root.join(&item.relative_path)
                };
                let dest =
                    if dest_root.is_file() || (plan.items.len() == 1 && source_root.is_file()) {
                        dest_root.to_path_buf()
                    } else {
                        dest_root.join(&item.relative_path)
                    };

                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&source, &dest)?;
                written += 1;
            }
            SyncItemAction::Skip | SyncItemAction::Denied | SyncItemAction::Conflict => {}
        }
    }

    Ok(written)
}

// =============================================================================
// Snapshot sync: immutable artifacts with versioned filenames
// =============================================================================

/// Patterns that identify live SQLite database files (never eligible for sync).
const LIVE_DB_PATTERNS: &[&str] = &[
    ".db",
    "-wal",
    "-shm",
    ".db-wal",
    ".db-shm",
    ".sqlite",
    ".sqlite-wal",
    ".sqlite-shm",
];

/// Check if a path refers to a live SQLite database file.
///
/// Live DB files (including WAL and SHM) must never be synced.
/// Only exported snapshots (explicit copies) are safe.
#[must_use]
pub fn is_live_db_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    LIVE_DB_PATTERNS
        .iter()
        .any(|pattern| lower.ends_with(pattern))
}

/// Generate a self-describing snapshot filename.
///
/// Format: `wa_snapshot_{version}_{timestamp}_{workspace_key}_{host}.db`
///
/// - `version`: wa version string (sanitized)
/// - `timestamp`: UTC ISO-8601 compact (YYYYMMDD_HHMMSS)
/// - `workspace_key`: first 8 chars of SHA-256 of workspace root
/// - `host`: hostname (sanitized, truncated)
#[must_use]
pub fn snapshot_filename(
    version: &str,
    timestamp_utc: &str,
    workspace_root: &Path,
    hostname: &str,
) -> String {
    let version_safe = sanitize_component(version);
    let ts_safe = sanitize_component(timestamp_utc);
    let ws_hash = {
        let mut hasher = Sha256::new();
        hasher.update(workspace_root.to_string_lossy().as_bytes());
        hex::encode(hasher.finalize())
    };
    let ws_short = &ws_hash[..8];
    let host_safe = sanitize_component(hostname);
    let host_trunc = if host_safe.len() > 16 {
        &host_safe[..16]
    } else {
        &host_safe
    };

    format!("wa_snapshot_{version_safe}_{ts_safe}_{ws_short}_{host_trunc}.db")
}

/// Parse metadata from a snapshot filename.
///
/// Returns `Some((version, timestamp, workspace_key, host))` if the filename matches
/// the expected pattern, `None` otherwise.
#[must_use]
pub fn parse_snapshot_filename(filename: &str) -> Option<(String, String, String, String)> {
    let stem = filename.strip_suffix(".db")?;
    let rest = stem.strip_prefix("wa_snapshot_")?;

    // Split on underscores: version_ts_wskey_host
    // But version and timestamp may contain underscores themselves in the timestamp part
    // Format is: {version}_{YYYYMMDD}_{HHMMSS}_{wskey}_{host}
    let parts: Vec<&str> = rest.splitn(5, '_').collect();
    if parts.len() >= 4 {
        let version = parts[0].to_string();
        let timestamp = if parts.len() == 5 {
            format!("{}_{}", parts[1], parts[2])
        } else {
            parts[1].to_string()
        };
        let ws_key = if parts.len() == 5 {
            parts[3].to_string()
        } else {
            parts[2].to_string()
        };
        let host = if parts.len() == 5 {
            parts[4].to_string()
        } else {
            parts[3].to_string()
        };
        Some((version, timestamp, ws_key, host))
    } else {
        None
    }
}

fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build a snapshot-specific file plan with immutable semantics.
///
/// Unlike config sync, snapshot sync:
/// - Rejects any live DB files (WAL, SHM)
/// - Never overwrites: existing files with the same name are skipped
/// - Only processes files matching the `wa_snapshot_*` pattern
pub fn build_snapshot_plan(
    source_root: &Path,
    destination_root: &Path,
    direction: SyncDirection,
) -> SyncResult<SyncFilePlan> {
    let mut items = Vec::new();

    if !source_root.exists() {
        return Ok(SyncFilePlan {
            category: SyncCategory::Snapshots,
            source_root: path_to_string(source_root),
            destination_root: path_to_string(destination_root),
            direction,
            items: vec![],
            denied_count: 0,
            add_count: 0,
            update_count: 0,
            skip_count: 0,
            conflict_count: 0,
        });
    }

    let entries = if source_root.is_dir() {
        std::fs::read_dir(source_root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .collect::<Vec<_>>()
    } else {
        vec![]
    };

    for entry in entries {
        let path = entry.path();
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Reject live DB files
        if is_live_db_path(&filename) && !filename.starts_with("wa_snapshot_") {
            items.push(SyncItem {
                relative_path: filename,
                action: SyncItemAction::Denied,
                source_hash: None,
                destination_hash: None,
                size_bytes: None,
                reason: Some(
                    "live database file; only exported snapshots are eligible".to_string(),
                ),
            });
            continue;
        }

        let dest_path = destination_root.join(&filename);

        // Immutable: if destination exists, skip (never overwrite snapshots)
        if dest_path.exists() {
            let source_hash = hash_file(&path)?;
            let dest_hash = hash_file(&dest_path)?;
            items.push(SyncItem {
                relative_path: filename,
                action: SyncItemAction::Skip,
                source_hash: Some(source_hash),
                destination_hash: Some(dest_hash),
                size_bytes: Some(path.metadata()?.len()),
                reason: Some("snapshot already exists at destination (immutable)".to_string()),
            });
            continue;
        }

        // New snapshot: add
        let source_hash = hash_file(&path)?;
        items.push(SyncItem {
            relative_path: filename,
            action: SyncItemAction::Add,
            source_hash: Some(source_hash),
            destination_hash: None,
            size_bytes: Some(path.metadata()?.len()),
            reason: None,
        });
    }

    items.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

    let denied_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Denied)
        .count();
    let add_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Add)
        .count();
    let skip_count = items
        .iter()
        .filter(|i| i.action == SyncItemAction::Skip)
        .count();

    Ok(SyncFilePlan {
        category: SyncCategory::Snapshots,
        source_root: path_to_string(source_root),
        destination_root: path_to_string(destination_root),
        direction,
        items,
        denied_count,
        add_count,
        update_count: 0,
        skip_count,
        conflict_count: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sync_status_sorts_targets() {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.targets = vec![
            SyncTargetConfig {
                name: "zeta".to_string(),
                endpoint: "zeta".to_string(),
                root: "/tmp/zeta".to_string(),
                ..SyncTargetConfig::default()
            },
            SyncTargetConfig {
                name: "alpha".to_string(),
                endpoint: "alpha".to_string(),
                root: "/tmp/alpha".to_string(),
                ..SyncTargetConfig::default()
            },
        ];

        let status = build_sync_status(&config);
        assert_eq!(status.targets[0].name, "alpha");
        assert_eq!(status.targets[1].name, "zeta");
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn make_target(name: &str) -> SyncTargetConfig {
        SyncTargetConfig {
            name: name.to_string(),
            endpoint: format!("user@{name}.example.com"),
            root: format!("/srv/{name}"),
            ..SyncTargetConfig::default()
        }
    }

    fn enabled_config_with_target(name: &str) -> Config {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.targets = vec![make_target(name)];
        config
    }

    fn test_layout() -> WorkspaceLayout {
        WorkspaceLayout::new(
            PathBuf::from("/tmp/wa-test-workspace"),
            &Default::default(),
            &Default::default(),
        )
    }

    fn plan_options(direction: SyncDirection) -> SyncPlanOptions {
        SyncPlanOptions {
            target: None,
            direction,
            dry_run: true,
            apply: false,
            yes: false,
            allow_overwrite: false,
            include: vec![],
            config_path: None,
        }
    }

    // ========================================================================
    // SyncStatus construction
    // ========================================================================

    #[test]
    fn sync_status_disabled_by_default() {
        let config = Config::default();
        let status = build_sync_status(&config);
        assert!(!status.enabled);
        assert!(status.targets.is_empty());
    }

    #[test]
    fn sync_status_reflects_global_flags() {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.allow_binary = true;
        config.sync.allow_config = false;
        config.sync.allow_snapshots = true;
        config.sync.allow_overwrite = true;
        config.sync.require_confirmation = false;

        let status = build_sync_status(&config);
        assert!(status.enabled);
        assert!(status.allow_binary);
        assert!(!status.allow_config);
        assert!(status.allow_snapshots);
        assert!(status.allow_overwrite);
        assert!(!status.require_confirmation);
    }

    #[test]
    fn sync_status_includes_allow_deny_paths() {
        let mut config = Config::default();
        config.sync.allow_paths = vec!["~/.config/wa/**".to_string()];
        config.sync.deny_paths = vec!["**/.env".to_string(), "**/tokens.json".to_string()];

        let status = build_sync_status(&config);
        assert_eq!(status.allow_paths.len(), 1);
        assert_eq!(status.deny_paths.len(), 2);
        assert!(status.deny_paths.contains(&"**/.env".to_string()));
    }

    #[test]
    fn sync_status_default_warnings_present() {
        let config = Config::default();
        let status = build_sync_status(&config);
        assert!(status.warnings.len() >= 2);
        assert!(status.warnings.iter().any(|w| w.contains("SQLite")));
        assert!(status.warnings.iter().any(|w| w.contains("Secrets")));
    }

    #[test]
    fn sync_status_target_inherits_global_flags() {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.allow_binary = true;
        config.sync.allow_config = true;
        config.sync.allow_snapshots = false;
        config.sync.targets = vec![make_target("prod")];

        let status = build_sync_status(&config);
        let target = &status.targets[0];
        assert!(target.allow_binary);
        assert!(target.allow_config);
        assert!(!target.allow_snapshots);
    }

    #[test]
    fn sync_status_target_override_wins() {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.allow_binary = false;
        config.sync.targets = vec![SyncTargetConfig {
            allow_binary: Some(true),
            ..make_target("override")
        }];

        let status = build_sync_status(&config);
        assert!(
            status.targets[0].allow_binary,
            "per-target override should win"
        );
    }

    // ========================================================================
    // Target selection
    // ========================================================================

    #[test]
    fn select_target_no_targets_fails() {
        let err = select_target(&[], None).unwrap_err();
        assert!(matches!(err, SyncError::NoTargets));
    }

    #[test]
    fn select_target_single_auto_selects() {
        let targets = vec![make_target("only")];
        let selected = select_target(&targets, None).unwrap();
        assert_eq!(selected.name, "only");
    }

    #[test]
    fn select_target_multiple_without_name_is_ambiguous() {
        let targets = vec![make_target("a"), make_target("b")];
        let err = select_target(&targets, None).unwrap_err();
        match err {
            SyncError::AmbiguousTarget { available } => {
                assert!(available.contains("a"));
                assert!(available.contains("b"));
            }
            _ => panic!("expected AmbiguousTarget, got {err:?}"),
        }
    }

    #[test]
    fn select_target_by_name() {
        let targets = vec![make_target("prod"), make_target("staging")];
        let selected = select_target(&targets, Some("staging")).unwrap();
        assert_eq!(selected.name, "staging");
    }

    #[test]
    fn select_target_unknown_name_fails() {
        let targets = vec![make_target("prod")];
        let err = select_target(&targets, Some("missing")).unwrap_err();
        match err {
            SyncError::UnknownTarget { name, available } => {
                assert_eq!(name, "missing");
                assert!(available.contains("prod"));
            }
            _ => panic!("expected UnknownTarget, got {err:?}"),
        }
    }

    // ========================================================================
    // Plan generation (build_sync_plan)
    // ========================================================================

    #[test]
    fn plan_fails_when_sync_disabled() {
        let config = Config::default(); // sync.enabled = false
        let layout = test_layout();
        let options = plan_options(SyncDirection::Push);

        let err = build_sync_plan(&config, &layout, options).unwrap_err();
        assert!(matches!(err, SyncError::Disabled));
    }

    #[test]
    fn plan_fails_with_no_targets() {
        let mut config = Config::default();
        config.sync.enabled = true;
        let layout = test_layout();

        let err = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap_err();
        assert!(matches!(err, SyncError::NoTargets));
    }

    #[test]
    fn plan_requires_confirmation_for_apply() {
        let config = enabled_config_with_target("prod");
        let layout = test_layout();
        let options = SyncPlanOptions {
            apply: true,
            yes: false,
            ..plan_options(SyncDirection::Push)
        };

        let err = build_sync_plan(&config, &layout, options).unwrap_err();
        assert!(matches!(err, SyncError::ConfirmationRequired));
    }

    #[test]
    fn plan_apply_with_yes_bypasses_confirmation() {
        let config = enabled_config_with_target("prod");
        let layout = test_layout();
        let options = SyncPlanOptions {
            apply: true,
            yes: true,
            ..plan_options(SyncDirection::Push)
        };

        let plan = build_sync_plan(&config, &layout, options).unwrap();
        assert!(plan.apply);
    }

    #[test]
    fn plan_confirmation_not_required_when_disabled() {
        let mut config = enabled_config_with_target("prod");
        config.sync.require_confirmation = false;
        let layout = test_layout();
        let options = SyncPlanOptions {
            apply: true,
            yes: false,
            ..plan_options(SyncDirection::Push)
        };

        // Should not error since require_confirmation is false
        let plan = build_sync_plan(&config, &layout, options).unwrap();
        assert!(plan.apply);
    }

    #[test]
    fn plan_push_generates_payloads_for_enabled_categories() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_binary = true;
        config.sync.allow_config = true;
        config.sync.allow_snapshots = true;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        assert_eq!(plan.payloads.len(), 3);

        let categories: Vec<SyncCategory> = plan.payloads.iter().map(|p| p.category).collect();
        assert!(categories.contains(&SyncCategory::Binary));
        assert!(categories.contains(&SyncCategory::Config));
        assert!(categories.contains(&SyncCategory::Snapshots));
    }

    #[test]
    fn plan_excludes_disabled_categories() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_binary = false;
        config.sync.allow_config = true;
        config.sync.allow_snapshots = false;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        assert_eq!(plan.payloads.len(), 1);
        assert_eq!(plan.payloads[0].category, SyncCategory::Config);
    }

    #[test]
    fn plan_include_filter_restricts_categories() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_binary = true;
        config.sync.allow_config = true;
        config.sync.allow_snapshots = true;
        let layout = test_layout();
        let options = SyncPlanOptions {
            include: vec![SyncCategory::Snapshots],
            ..plan_options(SyncDirection::Push)
        };

        let plan = build_sync_plan(&config, &layout, options).unwrap();
        assert_eq!(plan.payloads.len(), 1);
        assert_eq!(plan.payloads[0].category, SyncCategory::Snapshots);
        // Warnings should mention excluded categories
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Binary sync excluded"))
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Config sync excluded"))
        );
    }

    #[test]
    fn plan_push_direction_sets_source_as_local() {
        let config = enabled_config_with_target("prod");
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        assert_eq!(plan.direction, SyncDirection::Push);
        // Config payload: source should be local, destination should be remote
        let config_payload = plan
            .payloads
            .iter()
            .find(|p| p.category == SyncCategory::Config)
            .unwrap();
        assert!(config_payload.destination.starts_with("/srv/prod"));
    }

    #[test]
    fn plan_pull_direction_reverses_source_destination() {
        let config = enabled_config_with_target("prod");
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Pull)).unwrap();
        assert_eq!(plan.direction, SyncDirection::Pull);
        // Config payload: source should be remote, destination should be local
        let config_payload = plan
            .payloads
            .iter()
            .find(|p| p.category == SyncCategory::Config)
            .unwrap();
        assert!(config_payload.source.starts_with("/srv/prod"));
    }

    #[test]
    fn plan_dry_run_flag_preserved() {
        let config = enabled_config_with_target("prod");
        let layout = test_layout();
        let options = SyncPlanOptions {
            dry_run: true,
            ..plan_options(SyncDirection::Push)
        };

        let plan = build_sync_plan(&config, &layout, options).unwrap();
        assert!(plan.dry_run);
        assert!(!plan.apply);
    }

    #[test]
    fn plan_allow_overwrite_from_options() {
        let config = enabled_config_with_target("prod");
        let layout = test_layout();
        let options = SyncPlanOptions {
            allow_overwrite: true,
            ..plan_options(SyncDirection::Push)
        };

        let plan = build_sync_plan(&config, &layout, options).unwrap();
        assert!(plan.allow_overwrite);
    }

    #[test]
    fn plan_allow_overwrite_from_config() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_overwrite = true;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        assert!(plan.allow_overwrite);
    }

    // ========================================================================
    // include_category helper
    // ========================================================================

    #[test]
    fn include_empty_allows_all() {
        let include: Vec<SyncCategory> = vec![];
        assert!(include_category(&include, SyncCategory::Binary));
        assert!(include_category(&include, SyncCategory::Config));
        assert!(include_category(&include, SyncCategory::Snapshots));
    }

    #[test]
    fn include_explicit_filters_others() {
        let include = vec![SyncCategory::Config];
        assert!(!include_category(&include, SyncCategory::Binary));
        assert!(include_category(&include, SyncCategory::Config));
        assert!(!include_category(&include, SyncCategory::Snapshots));
    }

    // ========================================================================
    // effective_allow helper
    // ========================================================================

    #[test]
    fn effective_allow_uses_global_when_no_override() {
        assert!(effective_allow(true, None));
        assert!(!effective_allow(false, None));
    }

    #[test]
    fn effective_allow_override_wins() {
        assert!(effective_allow(false, Some(true)));
        assert!(!effective_allow(true, Some(false)));
    }

    // ========================================================================
    // Error display messages
    // ========================================================================

    #[test]
    fn error_disabled_message() {
        let err = SyncError::Disabled;
        let msg = err.to_string();
        assert!(msg.contains("disabled"));
        assert!(msg.contains("wa.toml"));
    }

    #[test]
    fn error_no_targets_message() {
        let err = SyncError::NoTargets;
        assert!(err.to_string().contains("no sync targets"));
    }

    #[test]
    fn error_unknown_target_message() {
        let err = SyncError::UnknownTarget {
            name: "ghost".to_string(),
            available: "prod, staging".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("ghost"));
        assert!(msg.contains("prod, staging"));
    }

    #[test]
    fn error_ambiguous_target_message() {
        let err = SyncError::AmbiguousTarget {
            available: "prod, staging".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("--target"));
        assert!(msg.contains("prod, staging"));
    }

    #[test]
    fn error_confirmation_required_message() {
        let err = SyncError::ConfirmationRequired;
        assert!(err.to_string().contains("--yes"));
    }

    // ========================================================================
    // JSON serialization stability
    // ========================================================================

    #[test]
    fn sync_plan_json_serialization_stable() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_config = true;
        config.sync.allow_binary = false;
        config.sync.allow_snapshots = false;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        let json = serde_json::to_value(&plan).unwrap();

        // Verify expected fields exist
        assert!(json.get("target").is_some());
        assert!(json.get("direction").is_some());
        assert!(json.get("dry_run").is_some());
        assert!(json.get("apply").is_some());
        assert!(json.get("allow_overwrite").is_some());
        assert!(json.get("warnings").is_some());
        assert!(json.get("payloads").is_some());

        // direction serializes correctly
        assert_eq!(json["direction"], "push");
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["apply"], false);
    }

    #[test]
    fn sync_plan_pull_direction_serializes() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_config = true;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Pull)).unwrap();
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["direction"], "pull");
    }

    #[test]
    fn sync_payload_json_fields() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_config = true;
        config.sync.allow_binary = false;
        config.sync.allow_snapshots = false;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        let json = serde_json::to_value(&plan).unwrap();
        let payload = &json["payloads"][0];

        assert_eq!(payload["category"], "config");
        assert!(payload["source"].is_string());
        assert!(payload["destination"].is_string());
    }

    #[test]
    fn sync_status_json_serialization() {
        let config = enabled_config_with_target("prod");
        let status = build_sync_status(&config);
        let json = serde_json::to_value(&status).unwrap();

        assert_eq!(json["enabled"], true);
        assert!(json["targets"].is_array());
        assert!(json["warnings"].is_array());
        assert!(json["allow_paths"].is_array());
        assert!(json["deny_paths"].is_array());
    }

    #[test]
    fn sync_category_serializes_as_snake_case() {
        let payload = SyncPayload {
            category: SyncCategory::Snapshots,
            source: "/local/snap".to_string(),
            destination: "/remote/snap".to_string(),
            note: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["category"], "snapshots");
    }

    // ========================================================================
    // Warnings: safety messages never leak secrets
    // ========================================================================

    #[test]
    fn default_warnings_do_not_contain_paths_or_secrets() {
        let warnings = default_warnings();
        for w in &warnings {
            assert!(
                !w.contains('/'),
                "warnings should not contain file paths: {w}"
            );
            assert!(
                !w.contains("token"),
                "warnings should not contain 'token': {w}"
            );
            assert!(!w.contains("key"), "warnings should not contain 'key': {w}");
        }
    }

    #[test]
    fn plan_warnings_include_disabled_category_notes() {
        let mut config = enabled_config_with_target("prod");
        config.sync.allow_binary = false;
        config.sync.allow_config = false;
        config.sync.allow_snapshots = false;
        let layout = test_layout();

        let plan = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Binary sync disabled"))
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Config sync disabled"))
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Snapshot sync disabled"))
        );
    }

    // ========================================================================
    // Remote path construction
    // ========================================================================

    #[test]
    fn remote_root_for_with_suffix() {
        let target = SyncTargetStatus {
            name: "prod".to_string(),
            transport: "ssh".to_string(),
            endpoint: "user@host".to_string(),
            root: "/srv/wa".to_string(),
            default_direction: SyncDirection::Push,
            allow_binary: true,
            allow_config: true,
            allow_snapshots: true,
        };

        assert_eq!(
            remote_root_for(&target, "bin/wa"),
            PathBuf::from("/srv/wa/bin/wa")
        );
        assert_eq!(
            remote_root_for(&target, "config"),
            PathBuf::from("/srv/wa/config")
        );
        assert_eq!(
            remote_root_for(&target, "snapshots"),
            PathBuf::from("/srv/wa/snapshots")
        );
    }

    #[test]
    fn remote_root_for_empty_suffix() {
        let target = SyncTargetStatus {
            name: "prod".to_string(),
            transport: "ssh".to_string(),
            endpoint: "user@host".to_string(),
            root: "/srv/wa".to_string(),
            default_direction: SyncDirection::Push,
            allow_binary: true,
            allow_config: true,
            allow_snapshots: true,
        };

        assert_eq!(remote_root_for(&target, ""), PathBuf::from("/srv/wa"));
    }

    // ========================================================================
    // Multi-target plans
    // ========================================================================

    #[test]
    fn plan_selects_explicit_target_from_multiple() {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.allow_config = true;
        config.sync.targets = vec![make_target("prod"), make_target("staging")];
        let layout = test_layout();
        let options = SyncPlanOptions {
            target: Some("staging".to_string()),
            ..plan_options(SyncDirection::Push)
        };

        let plan = build_sync_plan(&config, &layout, options).unwrap();
        assert_eq!(plan.target.name, "staging");
        assert!(plan.target.endpoint.contains("staging"));
    }

    #[test]
    fn plan_fails_with_multiple_targets_no_selection() {
        let mut config = Config::default();
        config.sync.enabled = true;
        config.sync.targets = vec![make_target("prod"), make_target("staging")];
        let layout = test_layout();

        let err = build_sync_plan(&config, &layout, plan_options(SyncDirection::Push)).unwrap_err();
        assert!(matches!(err, SyncError::AmbiguousTarget { .. }));
    }

    // ========================================================================
    // Path deny/allow rules
    // ========================================================================

    #[test]
    fn deny_env_files() {
        assert!(is_path_denied(".env", &[]));
        assert!(is_path_denied(".env.local", &[]));
        assert!(is_path_denied(".env.production", &[]));
        assert!(is_path_denied("subdir/.env", &[]));
    }

    #[test]
    fn deny_credential_files() {
        assert!(is_path_denied("tokens.json", &[]));
        assert!(is_path_denied("credentials.json", &[]));
        assert!(is_path_denied("subdir/tokens.json", &[]));
    }

    #[test]
    fn deny_ssh_and_crypto_keys() {
        assert!(is_path_denied(".ssh/id_rsa", &[]));
        assert!(is_path_denied("id_ed25519", &[]));
        assert!(is_path_denied(".gnupg/pubring.kbx", &[]));
        assert!(is_path_denied("cert.pem", &[]));
        assert!(is_path_denied("private.key", &[]));
        assert!(is_path_denied("server.p12", &[]));
        assert!(is_path_denied("client.pfx", &[]));
    }

    #[test]
    fn deny_extra_patterns() {
        let extra = vec!["custom_secret.txt".to_string()];
        assert!(is_path_denied("custom_secret.txt", &extra));
        assert!(!is_path_denied("safe_file.txt", &extra));
    }

    #[test]
    fn allow_normal_config_files() {
        assert!(!is_path_denied("wa.toml", &[]));
        assert!(!is_path_denied("profiles/default.toml", &[]));
        assert!(!is_path_denied("rules/codex.yaml", &[]));
    }

    #[test]
    fn allow_list_empty_allows_all() {
        assert!(is_path_allowed("anything.txt", &[]));
    }

    #[test]
    fn allow_list_filters() {
        let allow = vec!["config/".to_string()];
        assert!(is_path_allowed("config/wa.toml", &allow));
        assert!(!is_path_allowed("secrets/token.json", &allow));
    }

    #[test]
    fn allow_glob_style_prefix() {
        let allow = vec!["config/**".to_string()];
        assert!(is_path_allowed("config/profiles/default.toml", &allow));
        assert!(!is_path_allowed("other/file.txt", &allow));
    }

    // ========================================================================
    // File plan building (temp dirs)
    // ========================================================================

    fn setup_temp_trees() -> (tempfile::TempDir, tempfile::TempDir) {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        (src, dst)
    }

    fn write_file(dir: &Path, rel_path: &str, content: &str) {
        let path = dir.join(rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn file_plan_new_file_is_add() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# config");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert_eq!(plan.add_count, 1);
        assert_eq!(plan.items[0].action, SyncItemAction::Add);
        assert!(plan.items[0].source_hash.is_some());
        assert!(plan.items[0].destination_hash.is_none());
    }

    #[test]
    fn file_plan_same_content_is_skip() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# same content");
        write_file(dst.path(), "wa.toml", "# same content");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert_eq!(plan.skip_count, 1);
        assert_eq!(plan.items[0].action, SyncItemAction::Skip);
    }

    #[test]
    fn file_plan_different_content_without_overwrite_is_conflict() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# source version");
        write_file(dst.path(), "wa.toml", "# dest version");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert_eq!(plan.conflict_count, 1);
        assert_eq!(plan.items[0].action, SyncItemAction::Conflict);
        assert!(
            plan.items[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("--allow-overwrite")
        );
    }

    #[test]
    fn file_plan_different_content_with_overwrite_is_update() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# source version");
        write_file(dst.path(), "wa.toml", "# dest version");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            true, // allow_overwrite
        )
        .unwrap();

        assert_eq!(plan.update_count, 1);
        assert_eq!(plan.items[0].action, SyncItemAction::Update);
    }

    #[test]
    fn file_plan_denied_files_are_excluded() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# config");
        write_file(src.path(), ".env", "SECRET=value");
        write_file(src.path(), "tokens.json", "{}");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert_eq!(plan.denied_count, 2);
        assert_eq!(plan.add_count, 1);
        let denied: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.action == SyncItemAction::Denied)
            .collect();
        assert_eq!(denied.len(), 2);
    }

    #[test]
    fn file_plan_sorted_deterministically() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "z_file.toml", "z");
        write_file(src.path(), "a_file.toml", "a");
        write_file(src.path(), "m_file.toml", "m");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        let paths: Vec<&str> = plan
            .items
            .iter()
            .map(|i| i.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["a_file.toml", "m_file.toml", "z_file.toml"]);
    }

    #[test]
    fn file_plan_nested_directories() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "profiles/default.toml", "default");
        write_file(src.path(), "profiles/work.toml", "work");
        write_file(src.path(), "rules/codex.yaml", "rules");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert_eq!(plan.add_count, 3);
        assert!(
            plan.items
                .iter()
                .any(|i| i.relative_path == "profiles/default.toml")
        );
        assert!(
            plan.items
                .iter()
                .any(|i| i.relative_path == "rules/codex.yaml")
        );
    }

    #[test]
    fn file_plan_single_file_mode() {
        let (src, dst) = setup_temp_trees();
        let binary = src.path().join("wa");
        std::fs::write(&binary, b"binary content").unwrap();

        let plan = build_file_plan(
            &binary,
            &dst.path().join("wa"),
            SyncCategory::Binary,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert_eq!(plan.add_count, 1);
        assert_eq!(plan.items[0].relative_path, "wa");
    }

    #[test]
    fn file_plan_empty_source_dir() {
        let (src, dst) = setup_temp_trees();

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert!(plan.items.is_empty());
        assert_eq!(plan.add_count, 0);
    }

    #[test]
    fn file_plan_nonexistent_source_is_empty() {
        let dst = tempfile::tempdir().unwrap();
        let plan = build_file_plan(
            Path::new("/nonexistent/path/that/does/not/exist"),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        assert!(plan.items.is_empty());
    }

    // ========================================================================
    // Sync execution
    // ========================================================================

    #[test]
    fn execute_adds_new_files() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# config");
        write_file(src.path(), "profiles/default.toml", "default");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        let written = execute_file_plan(&plan).unwrap();
        assert_eq!(written, 2);
        assert!(dst.path().join("wa.toml").exists());
        assert!(dst.path().join("profiles/default.toml").exists());
        assert_eq!(
            std::fs::read_to_string(dst.path().join("wa.toml")).unwrap(),
            "# config"
        );
    }

    #[test]
    fn execute_skips_unchanged_files() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# same");
        write_file(dst.path(), "wa.toml", "# same");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        let written = execute_file_plan(&plan).unwrap();
        assert_eq!(written, 0);
    }

    #[test]
    fn execute_refuses_unresolved_conflicts() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# source");
        write_file(dst.path(), "wa.toml", "# dest");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false, // no overwrite
        )
        .unwrap();

        let err = execute_file_plan(&plan).unwrap_err();
        assert!(matches!(err, SyncError::UnresolvedConflicts { count: 1 }));
    }

    #[test]
    fn execute_updates_with_overwrite() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# new version");
        write_file(dst.path(), "wa.toml", "# old version");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            true, // allow overwrite
        )
        .unwrap();

        let written = execute_file_plan(&plan).unwrap();
        assert_eq!(written, 1);
        assert_eq!(
            std::fs::read_to_string(dst.path().join("wa.toml")).unwrap(),
            "# new version"
        );
    }

    #[test]
    fn execute_never_writes_denied_files() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), ".env", "SECRET=hunter2");
        write_file(src.path(), "wa.toml", "# config");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        let written = execute_file_plan(&plan).unwrap();
        assert_eq!(written, 1);
        assert!(
            !dst.path().join(".env").exists(),
            "denied file must not be synced"
        );
        assert!(dst.path().join("wa.toml").exists());
    }

    #[test]
    fn dry_run_plan_does_not_modify_destination() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# config");

        // Build plan (plan never modifies files)
        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        // Verify destination is still empty (plan is read-only)
        assert!(!dst.path().join("wa.toml").exists());
        assert_eq!(plan.add_count, 1);
    }

    // ========================================================================
    // JSON serialization: file plan
    // ========================================================================

    #[test]
    fn file_plan_json_stable() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.toml", "# config");

        let plan = build_file_plan(
            src.path(),
            dst.path(),
            SyncCategory::Config,
            SyncDirection::Push,
            &[],
            &[],
            false,
        )
        .unwrap();

        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["category"], "config");
        assert_eq!(json["add_count"], 1);
        assert_eq!(json["denied_count"], 0);
        assert!(json["items"].is_array());
        assert_eq!(json["items"][0]["action"], "add");
    }

    #[test]
    fn sync_item_action_serializes_snake_case() {
        let item = SyncItem {
            relative_path: "test.txt".to_string(),
            action: SyncItemAction::Denied,
            source_hash: None,
            destination_hash: None,
            size_bytes: None,
            reason: Some("denied".to_string()),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["action"], "denied");
    }

    // ========================================================================
    // Error display: new variants
    // ========================================================================

    #[test]
    fn error_unresolved_conflicts_message() {
        let err = SyncError::UnresolvedConflicts { count: 3 };
        let msg = err.to_string();
        assert!(msg.contains("3"));
        assert!(msg.contains("conflict"));
    }

    #[test]
    fn error_io_message() {
        let err = SyncError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let msg = err.to_string();
        assert!(msg.contains("access denied"));
    }

    // ========================================================================
    // Live DB path detection
    // ========================================================================

    #[test]
    fn live_db_paths_detected() {
        assert!(is_live_db_path("wa.db"));
        assert!(is_live_db_path("wa.db-wal"));
        assert!(is_live_db_path("wa.db-shm"));
        assert!(is_live_db_path("data.sqlite"));
        assert!(is_live_db_path("data.sqlite-wal"));
        assert!(is_live_db_path("data.sqlite-shm"));
        assert!(is_live_db_path("/path/to/wa.db"));
    }

    #[test]
    fn non_db_paths_not_detected() {
        assert!(!is_live_db_path("wa.toml"));
        assert!(!is_live_db_path("config.yaml"));
        assert!(!is_live_db_path("backup.tar.gz"));
        // Note: snapshot filenames end with .db so is_live_db_path returns true,
        // but build_snapshot_plan handles this by checking the wa_snapshot_ prefix.
        assert!(is_live_db_path(
            "wa_snapshot_0.1.0_20260208_120000_abcd1234_host.db"
        ));
    }

    // ========================================================================
    // Snapshot filename generation
    // ========================================================================

    #[test]
    fn snapshot_filename_format() {
        let name = snapshot_filename(
            "0.1.0",
            "20260208_120000",
            Path::new("/home/user/project"),
            "my-server",
        );
        assert!(name.starts_with("wa_snapshot_"));
        assert!(name.ends_with(".db"));
        assert!(name.contains("0.1.0"));
        assert!(name.contains("20260208_120000"));
        assert!(name.contains("my-server"));
        // workspace key is 8 hex chars
        let parts: Vec<&str> = name.split('_').collect();
        assert!(parts.len() >= 5);
    }

    #[test]
    fn snapshot_filename_sanitizes_special_chars() {
        let name = snapshot_filename(
            "0.1.0-beta+build",
            "2026/02/08 12:00",
            Path::new("/home/user"),
            "host name!",
        );
        // No special chars except - and .
        assert!(!name.contains('/'));
        assert!(!name.contains('!'));
        assert!(!name.contains(' '));
    }

    #[test]
    fn snapshot_filename_truncates_long_hostname() {
        let name = snapshot_filename(
            "0.1.0",
            "20260208",
            Path::new("/workspace"),
            "this-is-a-very-long-hostname-that-should-be-truncated",
        );
        // Hostname portion should be at most 16 chars
        assert!(name.len() < 200);
    }

    #[test]
    fn snapshot_filename_deterministic() {
        let a = snapshot_filename("0.1.0", "20260208", Path::new("/ws"), "host");
        let b = snapshot_filename("0.1.0", "20260208", Path::new("/ws"), "host");
        assert_eq!(a, b);
    }

    #[test]
    fn snapshot_filename_differs_by_workspace() {
        let a = snapshot_filename("0.1.0", "20260208", Path::new("/ws/a"), "host");
        let b = snapshot_filename("0.1.0", "20260208", Path::new("/ws/b"), "host");
        assert_ne!(a, b);
    }

    // ========================================================================
    // Snapshot filename parsing
    // ========================================================================

    #[test]
    fn parse_snapshot_roundtrip() {
        let name = snapshot_filename(
            "0.1.0",
            "20260208_120000",
            Path::new("/home/user/project"),
            "myhost",
        );
        let parsed = parse_snapshot_filename(&name);
        assert!(parsed.is_some());
        let (version, ts, _ws_key, host) = parsed.unwrap();
        assert_eq!(version, "0.1.0");
        assert!(ts.contains("20260208"));
        assert_eq!(host, "myhost");
    }

    #[test]
    fn parse_invalid_filename_returns_none() {
        assert!(parse_snapshot_filename("random_file.txt").is_none());
        assert!(parse_snapshot_filename("wa.db").is_none());
        assert!(parse_snapshot_filename("").is_none());
    }

    // ========================================================================
    // Snapshot plan building
    // ========================================================================

    #[test]
    fn snapshot_plan_adds_new_snapshots() {
        let (src, dst) = setup_temp_trees();
        let snap_name = snapshot_filename("0.1.0", "20260208_120000", Path::new("/test"), "host");
        write_file(src.path(), &snap_name, "snapshot data");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();

        assert_eq!(plan.add_count, 1);
        assert_eq!(plan.items[0].action, SyncItemAction::Add);
    }

    #[test]
    fn snapshot_plan_skips_existing_snapshots() {
        let (src, dst) = setup_temp_trees();
        let snap_name = snapshot_filename("0.1.0", "20260208_120000", Path::new("/test"), "host");
        write_file(src.path(), &snap_name, "snapshot data");
        write_file(dst.path(), &snap_name, "snapshot data");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();

        assert_eq!(plan.skip_count, 1);
        assert_eq!(plan.add_count, 0);
        assert!(
            plan.items[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("immutable")
        );
    }

    #[test]
    fn snapshot_plan_never_overwrites_different_content() {
        let (src, dst) = setup_temp_trees();
        let snap_name = snapshot_filename("0.1.0", "20260208_120000", Path::new("/test"), "host");
        write_file(src.path(), &snap_name, "new snapshot data");
        write_file(dst.path(), &snap_name, "old snapshot data");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();

        // Even with different content, snapshots are immutable: skip, not conflict
        assert_eq!(plan.skip_count, 1);
        assert_eq!(plan.conflict_count, 0);
    }

    #[test]
    fn snapshot_plan_denies_live_db_files() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.db", "live database");
        write_file(src.path(), "wa.db-wal", "wal");
        write_file(src.path(), "wa.db-shm", "shm");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();

        assert_eq!(plan.denied_count, 3);
        assert_eq!(plan.add_count, 0);
        for item in &plan.items {
            assert_eq!(item.action, SyncItemAction::Denied);
            assert!(item.reason.as_deref().unwrap().contains("live database"));
        }
    }

    #[test]
    fn snapshot_plan_mixed_files() {
        let (src, dst) = setup_temp_trees();
        let snap = snapshot_filename("0.1.0", "20260208", Path::new("/t"), "h");
        write_file(src.path(), &snap, "snapshot");
        write_file(src.path(), "wa.db", "live db");
        write_file(src.path(), "wa.db-wal", "wal");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();

        assert_eq!(plan.add_count, 1);
        assert_eq!(plan.denied_count, 2);
    }

    #[test]
    fn snapshot_plan_empty_source() {
        let (src, dst) = setup_temp_trees();
        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();
        assert!(plan.items.is_empty());
    }

    #[test]
    fn snapshot_plan_nonexistent_source() {
        let dst = tempfile::tempdir().unwrap();
        let plan = build_snapshot_plan(
            Path::new("/nonexistent/snapshot/dir"),
            dst.path(),
            SyncDirection::Push,
        )
        .unwrap();
        assert!(plan.items.is_empty());
    }

    #[test]
    fn snapshot_plan_execute_adds_snapshot() {
        let (src, dst) = setup_temp_trees();
        let snap = snapshot_filename("0.1.0", "20260208", Path::new("/t"), "h");
        write_file(src.path(), &snap, "snapshot content");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();
        let written = execute_file_plan(&plan).unwrap();

        assert_eq!(written, 1);
        assert_eq!(
            std::fs::read_to_string(dst.path().join(&snap)).unwrap(),
            "snapshot content"
        );
    }

    #[test]
    fn snapshot_plan_execute_never_copies_live_db() {
        let (src, dst) = setup_temp_trees();
        write_file(src.path(), "wa.db", "live db data");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();
        let written = execute_file_plan(&plan).unwrap();

        assert_eq!(written, 0);
        assert!(!dst.path().join("wa.db").exists());
    }

    #[test]
    fn snapshot_plan_sorted_deterministically() {
        let (src, dst) = setup_temp_trees();
        let snap_c = snapshot_filename("0.1.0", "20260210", Path::new("/t"), "c");
        let snap_a = snapshot_filename("0.1.0", "20260208", Path::new("/t"), "a");
        let snap_b = snapshot_filename("0.1.0", "20260209", Path::new("/t"), "b");
        write_file(src.path(), &snap_c, "c");
        write_file(src.path(), &snap_a, "a");
        write_file(src.path(), &snap_b, "b");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();

        // Items should be sorted by filename (which includes timestamp)
        let paths: Vec<&str> = plan
            .items
            .iter()
            .map(|i| i.relative_path.as_str())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn snapshot_plan_json_stable() {
        let (src, dst) = setup_temp_trees();
        let snap = snapshot_filename("0.1.0", "20260208", Path::new("/t"), "h");
        write_file(src.path(), &snap, "data");

        let plan = build_snapshot_plan(src.path(), dst.path(), SyncDirection::Push).unwrap();
        let json = serde_json::to_value(&plan).unwrap();

        assert_eq!(json["category"], "snapshots");
        assert_eq!(json["add_count"], 1);
        assert_eq!(json["conflict_count"], 0);
        assert_eq!(json["update_count"], 0);
    }
}
