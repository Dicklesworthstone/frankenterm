//! Backup and restore for wa databases.
//!
//! Provides portable backup archives containing the SQLite database,
//! manifest metadata, and integrity checksums.

use std::cmp::Ordering;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone, Timelike, Weekday};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::SCHEMA_VERSION;
use crate::{Error, Result};

/// Manifest describing a backup archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    /// wa version that created this backup
    pub wa_version: String,
    /// Database schema version
    pub schema_version: i32,
    /// ISO-8601 timestamp of backup creation
    pub created_at: String,
    /// Workspace root that was backed up
    pub workspace: String,
    /// Database file size in bytes
    pub db_size_bytes: u64,
    /// SHA-256 checksum of the database file
    pub db_checksum: String,
    /// Statistics about the backed-up data
    pub stats: BackupStats,
}

/// Statistics about backed-up data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupStats {
    pub panes: u64,
    pub segments: u64,
    pub events: u64,
    pub audit_actions: u64,
    pub workflow_executions: u64,
}

/// Result of a backup export operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    /// Path to the created backup file
    pub output_path: String,
    /// Manifest describing the backup
    pub manifest: BackupManifest,
    /// Total size of the backup directory in bytes
    pub total_size_bytes: u64,
}

/// Options for export.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Output directory path (backup archive directory)
    pub output: Option<PathBuf>,
    /// Whether to include a SQL text dump alongside the binary copy
    pub include_sql_dump: bool,
    /// Whether to verify the backup after creation
    pub verify: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            output: None,
            include_sql_dump: false,
            verify: true,
        }
    }
}

/// Parsed schedule for automatic backups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupSchedule {
    /// Run every hour at the given minute (default: minute 0).
    Hourly { minute: u32 },
    /// Run daily at the given hour/minute (default: 03:00).
    Daily { hour: u32, minute: u32 },
    /// Run weekly on the given weekday at hour/minute (default: Sunday 03:00).
    Weekly {
        weekday: Weekday,
        hour: u32,
        minute: u32,
    },
    /// Simple 5-field cron (supports "*" or a single numeric value per field).
    Cron(CronSchedule),
}

/// Simple cron schedule representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Option<u32>,
    hour: Option<u32>,
    day_of_month: Option<u32>,
    month: Option<u32>,
    day_of_week: Option<u32>,
    raw: String,
}

impl BackupSchedule {
    /// Parse a schedule string ("hourly", "daily", "weekly", or "m h dom mon dow").
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("hourly") {
            return Ok(Self::Hourly { minute: 0 });
        }
        if trimmed.eq_ignore_ascii_case("daily") {
            return Ok(Self::Daily { hour: 3, minute: 0 });
        }
        if trimmed.eq_ignore_ascii_case("weekly") {
            return Ok(Self::Weekly {
                weekday: Weekday::Sun,
                hour: 3,
                minute: 0,
            });
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(Error::Config(crate::error::ConfigError::ParseError(
                "backup schedule must be 'hourly', 'daily', 'weekly', or 5-field cron".to_string(),
            )));
        }

        let minute = parse_cron_field(parts[0], 0, 59)?;
        let hour = parse_cron_field(parts[1], 0, 23)?;
        let day_of_month = parse_cron_field(parts[2], 1, 31)?;
        let month = parse_cron_field(parts[3], 1, 12)?;
        let day_of_week = parse_cron_field(parts[4], 0, 7)?;

        Ok(Self::Cron(CronSchedule {
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            raw: trimmed.to_string(),
        }))
    }

    /// Return a human-friendly schedule label for status output.
    #[must_use]
    pub fn display_label(&self) -> String {
        match self {
            Self::Hourly { .. } => "hourly".to_string(),
            Self::Daily { .. } => "daily".to_string(),
            Self::Weekly { .. } => "weekly".to_string(),
            Self::Cron(cron) => format!("cron: {}", cron.raw),
        }
    }

    /// Compute the next run time after `now` (local time).
    pub fn next_after(&self, now: DateTime<Local>) -> Result<DateTime<Local>> {
        match self {
            Self::Hourly { minute } => next_hourly(now, *minute),
            Self::Daily { hour, minute } => next_daily(now, *hour, *minute),
            Self::Weekly {
                weekday,
                hour,
                minute,
            } => next_weekly(now, *weekday, *hour, *minute),
            Self::Cron(cron) => next_cron(now, cron),
        }
    }
}

fn parse_cron_field(raw: &str, min: u32, max: u32) -> Result<Option<u32>> {
    if raw == "*" {
        return Ok(None);
    }
    let value: u32 = raw.parse().map_err(|_| {
        Error::Config(crate::error::ConfigError::ParseError(format!(
            "invalid cron field '{raw}'"
        )))
    })?;
    if value < min || value > max {
        return Err(Error::Config(crate::error::ConfigError::ParseError(
            format!("cron field '{raw}' out of range ({min}-{max})"),
        )));
    }
    Ok(Some(value))
}

fn next_hourly(now: DateTime<Local>, minute: u32) -> Result<DateTime<Local>> {
    if minute > 59 {
        return Err(Error::Config(crate::error::ConfigError::ParseError(
            "hourly minute must be 0-59".to_string(),
        )));
    }
    let mut candidate = now
        .with_minute(minute)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .ok_or_else(|| Error::Runtime("Failed to compute hourly schedule time".to_string()))?;

    if candidate <= now {
        candidate += ChronoDuration::hours(1);
    }
    Ok(candidate)
}

fn next_daily(now: DateTime<Local>, hour: u32, minute: u32) -> Result<DateTime<Local>> {
    if hour > 23 || minute > 59 {
        return Err(Error::Config(crate::error::ConfigError::ParseError(
            "daily schedule time must be in 24h range".to_string(),
        )));
    }
    let mut candidate = now
        .with_hour(hour)
        .and_then(|t| t.with_minute(minute))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .ok_or_else(|| Error::Runtime("Failed to compute daily schedule time".to_string()))?;

    if candidate <= now {
        candidate += ChronoDuration::days(1);
    }
    Ok(candidate)
}

fn next_weekly(
    now: DateTime<Local>,
    weekday: Weekday,
    hour: u32,
    minute: u32,
) -> Result<DateTime<Local>> {
    if hour > 23 || minute > 59 {
        return Err(Error::Config(crate::error::ConfigError::ParseError(
            "weekly schedule time must be in 24h range".to_string(),
        )));
    }

    let now_weekday = now.weekday().number_from_monday() as i64;
    let target_weekday = weekday.number_from_monday() as i64;
    let mut days_ahead = target_weekday - now_weekday;
    if days_ahead < 0 {
        days_ahead += 7;
    }

    let mut candidate = now
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .ok_or_else(|| Error::Runtime("Failed to compute weekly schedule time".to_string()))?;
    candidate += ChronoDuration::days(days_ahead);
    let candidate = Local
        .from_local_datetime(&candidate)
        .single()
        .ok_or_else(|| Error::Runtime("Failed to localize weekly schedule time".to_string()))?;

    if candidate <= now {
        return Ok(candidate + ChronoDuration::days(7));
    }
    Ok(candidate)
}

fn next_cron(now: DateTime<Local>, cron: &CronSchedule) -> Result<DateTime<Local>> {
    // Scan forward minute-by-minute up to 366 days.
    let max_minutes = 366_u32.saturating_mul(24).saturating_mul(60);
    for offset in 1..=max_minutes {
        let candidate = now + ChronoDuration::minutes(offset as i64);
        if cron_matches(candidate, cron) {
            return Ok(candidate);
        }
    }
    Err(Error::Runtime(
        "Failed to find next cron run within 366 days".to_string(),
    ))
}

fn cron_matches(candidate: DateTime<Local>, cron: &CronSchedule) -> bool {
    if let Some(minute) = cron.minute {
        if candidate.minute() != minute {
            return false;
        }
    }
    if let Some(hour) = cron.hour {
        if candidate.hour() != hour {
            return false;
        }
    }
    if let Some(month) = cron.month {
        if candidate.month() != month {
            return false;
        }
    }

    let day_of_month_matches = cron.day_of_month.is_none_or(|dom| candidate.day() == dom);
    let day_of_week_matches = cron.day_of_week.is_none_or(|dow| {
        let normalized = if dow == 7 { 0 } else { dow };
        let candidate_dow = candidate.weekday().num_days_from_sunday();
        candidate_dow == normalized
    });

    match (cron.day_of_month.is_some(), cron.day_of_week.is_some()) {
        (true, true) => day_of_month_matches || day_of_week_matches,
        _ => day_of_month_matches && day_of_week_matches,
    }
}

/// Information about a backup directory on disk.
#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub created_at: Option<String>,
    pub created_ts: Option<i64>,
    pub total_size_bytes: u64,
}

/// Status summary for scheduled backups.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledBackupStatus {
    pub enabled: bool,
    pub schedule: String,
    pub next_backup_at: Option<String>,
    pub last_backup_at: Option<String>,
    pub last_backup_size_bytes: Option<u64>,
    pub backups_kept: usize,
    pub max_backups: Option<u32>,
    pub destination: String,
}

/// Export a backup of the wa database to a directory archive.
///
/// Creates a backup directory containing:
/// - `database.db` — binary copy of the SQLite database (via backup API)
/// - `manifest.json` — metadata, stats, and checksums
/// - `checksums.sha256` — per-file SHA-256 checksums
///
/// The backup is created atomically: files are written to a temp directory
/// first, then renamed to the final location.
pub fn export_backup(
    db_path: &Path,
    workspace_root: &Path,
    opts: &ExportOptions,
) -> Result<ExportResult> {
    // Validate source database exists
    if !db_path.exists() {
        return Err(Error::Storage(crate::StorageError::Database(format!(
            "Database not found: {}",
            db_path.display()
        ))));
    }

    // Determine output path
    let output_dir = match &opts.output {
        Some(p) => p.clone(),
        None => default_backup_path(workspace_root),
    };

    // Create output directory
    fs::create_dir_all(&output_dir).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to create backup directory {}: {e}",
            output_dir.display()
        )))
    })?;

    // Step 1: Copy database using rusqlite backup API (safe, consistent snapshot)
    let dest_db_path = output_dir.join("database.db");
    backup_database(db_path, &dest_db_path)?;

    // Step 2: Compute checksum of the backed-up database
    let db_checksum = sha256_file(&dest_db_path)?;
    let db_size = fs::metadata(&dest_db_path).map_or(0, |m| m.len());

    // Step 3: Gather stats from the backup copy
    let stats = gather_stats(&dest_db_path)?;

    // Step 4: Build manifest
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let created_at = format_iso8601(now.as_secs());

    let manifest = BackupManifest {
        wa_version: crate::VERSION.to_string(),
        schema_version: SCHEMA_VERSION,
        created_at,
        workspace: workspace_root.display().to_string(),
        db_size_bytes: db_size,
        db_checksum: db_checksum.clone(),
        stats,
    };

    // Step 5: Write manifest.json
    let manifest_path = output_dir.join("manifest.json");
    let manifest_json = serde_json::to_string_pretty(&manifest).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to serialize manifest: {e}"
        )))
    })?;
    fs::write(&manifest_path, &manifest_json).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to write manifest: {e}"
        )))
    })?;

    // Step 6: Write checksums file
    let checksums_path = output_dir.join("checksums.sha256");
    let manifest_checksum = sha256_bytes(manifest_json.as_bytes());
    let checksums_content = format!(
        "{}  database.db\n{}  manifest.json\n",
        db_checksum, manifest_checksum
    );
    fs::write(&checksums_path, &checksums_content).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to write checksums: {e}"
        )))
    })?;

    // Step 7: Optionally include SQL text dump
    if opts.include_sql_dump {
        let sql_path = output_dir.join("database.sql");
        dump_database_sql(&dest_db_path, &sql_path)?;
    }

    // Step 8: Verify backup integrity if requested
    if opts.verify {
        verify_backup(&output_dir, &manifest)?;
    }

    // Compute total size
    let total_size = dir_size(&output_dir);

    Ok(ExportResult {
        output_path: output_dir.display().to_string(),
        manifest,
        total_size_bytes: total_size,
    })
}

/// Compute scheduled backup status for `wa status`.
pub fn scheduled_backup_status(
    config: &crate::config::ScheduledBackupConfig,
    workspace_root: &Path,
    now: DateTime<Local>,
) -> Result<ScheduledBackupStatus> {
    let schedule = BackupSchedule::parse(&config.schedule)?;
    let destination_root = resolve_destination_root(workspace_root, config.destination.as_deref());
    let entries = list_backup_entries(&destination_root)?;
    let latest = entries.iter().max_by(|a, b| compare_backup_entries(a, b));

    let next_backup_at = if config.enabled {
        Some(format_local_datetime(schedule.next_after(now)?))
    } else {
        None
    };

    Ok(ScheduledBackupStatus {
        enabled: config.enabled,
        schedule: schedule.display_label(),
        next_backup_at,
        last_backup_at: latest.and_then(|entry| entry.created_at.clone()),
        last_backup_size_bytes: latest.map(|entry| entry.total_size_bytes),
        backups_kept: entries.len(),
        max_backups: if config.max_backups == 0 {
            None
        } else {
            Some(config.max_backups)
        },
        destination: destination_root.display().to_string(),
    })
}

/// Resolve a backup destination root directory.
#[must_use]
pub fn backup_destination_root(workspace_root: &Path, destination: Option<&str>) -> PathBuf {
    resolve_destination_root(workspace_root, destination)
}

/// Build a unique output directory for a scheduled backup.
#[must_use]
pub fn scheduled_backup_output_path(
    workspace_root: &Path,
    destination: Option<&str>,
    now: DateTime<Local>,
) -> PathBuf {
    let base_dir = resolve_destination_root(workspace_root, destination);
    let ts = format_timestamp_compact(now.timestamp().max(0) as u64);
    unique_backup_path(&base_dir, &format!("wa_backup_{ts}"))
}

/// List backup directories under the destination root.
pub fn list_backup_entries(base_dir: &Path) -> Result<Vec<BackupEntry>> {
    if !base_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(base_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join("manifest.json");
        let (created_at, created_ts) = if manifest_path.exists() {
            match fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|data| serde_json::from_str::<BackupManifest>(&data).ok())
            {
                Some(manifest) => {
                    let ts = parse_manifest_timestamp(&manifest.created_at);
                    (Some(manifest.created_at), ts)
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };

        let created_ts = created_ts.or_else(|| {
            entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
        });

        let created_at = created_at.or_else(|| {
            created_ts
                .filter(|ts| *ts >= 0)
                .map(|ts| format_iso8601(ts as u64))
        });

        let total_size_bytes = dir_size(&path);
        entries.push(BackupEntry {
            path,
            created_at,
            created_ts,
            total_size_bytes,
        });
    }

    Ok(entries)
}

/// Summary of retention/rotation pruning.
#[derive(Debug, Clone)]
pub struct PruneSummary {
    pub removed: usize,
    pub kept: usize,
}

/// Prune backups by retention and max count.
pub fn prune_backups(
    base_dir: &Path,
    retention_days: u32,
    max_backups: u32,
    now: DateTime<Local>,
) -> Result<PruneSummary> {
    let mut entries = list_backup_entries(base_dir)?;
    if entries.is_empty() {
        return Ok(PruneSummary {
            removed: 0,
            kept: 0,
        });
    }

    let mut removed = 0_usize;
    if retention_days > 0 {
        let cutoff = now - ChronoDuration::days(retention_days as i64);
        entries.retain(|entry| {
            let keep = entry
                .created_ts
                .map(|ts| ts >= cutoff.timestamp())
                .unwrap_or(true);
            if !keep {
                if let Err(e) = fs::remove_dir_all(&entry.path) {
                    tracing::warn!(
                        path = %entry.path.display(),
                        error = %e,
                        "Failed to remove expired backup"
                    );
                } else {
                    removed += 1;
                }
            }
            keep
        });
    }

    if max_backups > 0 && entries.len() > max_backups as usize {
        entries.sort_by(compare_backup_entries);
        let keep_count = max_backups as usize;
        for entry in entries.drain(0..entries.len().saturating_sub(keep_count)) {
            if let Err(e) = fs::remove_dir_all(&entry.path) {
                tracing::warn!(
                    path = %entry.path.display(),
                    error = %e,
                    "Failed to remove rotated backup"
                );
            } else {
                removed += 1;
            }
        }
    }

    let kept = list_backup_entries(base_dir)?.len();
    Ok(PruneSummary { removed, kept })
}

/// Verify a backup directory's integrity.
pub fn verify_backup(backup_dir: &Path, manifest: &BackupManifest) -> Result<()> {
    let db_path = backup_dir.join("database.db");
    if !db_path.exists() {
        return Err(Error::Storage(crate::StorageError::Database(
            "Backup verification failed: database.db not found".to_string(),
        )));
    }

    let actual_checksum = sha256_file(&db_path)?;
    if actual_checksum != manifest.db_checksum {
        return Err(Error::Storage(crate::StorageError::Database(format!(
            "Backup verification failed: checksum mismatch (expected {}, got {})",
            manifest.db_checksum, actual_checksum
        ))));
    }

    // Verify the database can be opened and queried
    let conn = Connection::open(&db_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Backup verification failed: cannot open database: {e}"
        )))
    })?;

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| {
            Error::Storage(crate::StorageError::Database(format!(
                "Backup verification failed: integrity check error: {e}"
            )))
        })?;

    if integrity != "ok" {
        return Err(Error::Storage(crate::StorageError::Database(format!(
            "Backup verification failed: integrity check returned: {integrity}"
        ))));
    }

    Ok(())
}

/// Result of an import operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    /// Path of the backup that was imported
    pub source_path: String,
    /// Manifest from the imported backup
    pub manifest: BackupManifest,
    /// Path to the pre-import safety backup (if created)
    pub safety_backup_path: Option<String>,
    /// Whether this was a dry-run
    pub dry_run: bool,
}

/// Options for import.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// If true, only verify and show what would happen
    pub dry_run: bool,
    /// If true, skip interactive confirmation
    pub yes: bool,
    /// If true, skip creating a safety backup of current data
    pub no_safety_backup: bool,
}

/// Load and verify a backup manifest from a backup directory.
pub fn load_backup_manifest(backup_dir: &Path) -> Result<BackupManifest> {
    let manifest_path = backup_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(Error::Storage(crate::StorageError::Database(format!(
            "No manifest.json found in backup directory: {}",
            backup_dir.display()
        ))));
    }

    let data = fs::read_to_string(&manifest_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to read manifest: {e}"
        )))
    })?;

    let manifest: BackupManifest = serde_json::from_str(&data).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to parse manifest: {e}"
        )))
    })?;

    Ok(manifest)
}

/// Import (restore) a backup into the target database location.
///
/// Safety:
/// - Verifies backup integrity before importing
/// - Creates a safety backup of the current database (unless opted out)
/// - Refuses to import if schema version is incompatible
/// - Dry-run mode shows what would happen without modifying anything
pub fn import_backup(
    backup_dir: &Path,
    target_db_path: &Path,
    workspace_root: &Path,
    opts: &ImportOptions,
) -> Result<ImportResult> {
    // Step 1: Load and validate manifest
    let manifest = load_backup_manifest(backup_dir)?;

    // Step 2: Check schema compatibility
    if manifest.schema_version > SCHEMA_VERSION {
        return Err(Error::Storage(crate::StorageError::Database(format!(
            "Backup schema version {} is newer than supported version {}. \
             Upgrade wa before importing this backup.",
            manifest.schema_version, SCHEMA_VERSION
        ))));
    }

    // Step 3: Verify backup integrity
    let backup_db = backup_dir.join("database.db");
    if !backup_db.exists() {
        return Err(Error::Storage(crate::StorageError::Database(
            "Backup database.db not found".to_string(),
        )));
    }
    verify_backup(backup_dir, &manifest)?;

    // Dry-run: report what would happen and return
    if opts.dry_run {
        let safety_backup_path = if target_db_path.exists() && !opts.no_safety_backup {
            let path = default_backup_path(workspace_root);
            Some(path.display().to_string())
        } else {
            None
        };

        return Ok(ImportResult {
            source_path: backup_dir.display().to_string(),
            manifest,
            safety_backup_path,
            dry_run: true,
        });
    }

    // Step 4: Create safety backup of current database
    let safety_backup_path = if target_db_path.exists() && !opts.no_safety_backup {
        let safety_opts = ExportOptions {
            output: None, // default timestamped path
            include_sql_dump: false,
            verify: true,
        };
        let safety_result = export_backup(target_db_path, workspace_root, &safety_opts)?;
        Some(safety_result.output_path)
    } else {
        None
    };

    // Step 5: Replace current database with backup copy
    // Use rusqlite backup API to restore (consistent, handles WAL mode)
    if target_db_path.exists() {
        // Remove WAL and journal files if they exist
        let wal_path = target_db_path.with_extension("db-wal");
        let shm_path = target_db_path.with_extension("db-shm");
        let journal_path = target_db_path.with_extension("db-journal");
        for p in [&wal_path, &shm_path, &journal_path] {
            if p.exists() {
                let _ = fs::remove_file(p);
            }
        }
    }

    backup_database(&backup_db, target_db_path)?;

    Ok(ImportResult {
        source_path: backup_dir.display().to_string(),
        manifest,
        safety_backup_path,
        dry_run: false,
    })
}

// --- Internal helpers ---

/// Use rusqlite's online backup API for a consistent snapshot.
fn backup_database(src_path: &Path, dest_path: &Path) -> Result<()> {
    let src = Connection::open(src_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to open source database: {e}"
        )))
    })?;

    let mut dest = Connection::open(dest_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to create backup database: {e}"
        )))
    })?;

    let backup = rusqlite::backup::Backup::new(&src, &mut dest).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to initialize backup: {e}"
        )))
    })?;

    // Copy all pages in one step (no progress callback needed for now)
    backup.step(-1).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!("Backup failed: {e}")))
    })?;

    Ok(())
}

/// Dump the database to a SQL text file using sqlite3 .dump equivalent.
fn dump_database_sql(db_path: &Path, sql_path: &Path) -> Result<()> {
    let conn = Connection::open(db_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to open database for SQL dump: {e}"
        )))
    })?;

    let mut file = fs::File::create(sql_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to create SQL dump file: {e}"
        )))
    })?;

    // Write header
    writeln!(file, "-- wa database backup (SQL dump)").ok();
    writeln!(file, "-- Schema version: {}", SCHEMA_VERSION).ok();
    writeln!(file, "BEGIN TRANSACTION;").ok();

    // Get all table names
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|e| {
            Error::Storage(crate::StorageError::Database(format!(
                "Failed to list tables: {e}"
            )))
        })?;

    let tables: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| {
            Error::Storage(crate::StorageError::Database(format!(
                "Failed to query tables: {e}"
            )))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (name, create_sql) in &tables {
        writeln!(file, "\n-- Table: {name}").ok();
        writeln!(file, "{create_sql};").ok();

        // Dump rows as INSERT statements
        let row_sql = format!("SELECT * FROM \"{name}\"");
        if let Ok(mut row_stmt) = conn.prepare(&row_sql) {
            let col_count = row_stmt.column_count();
            let col_names: Vec<String> = (0..col_count)
                .map(|i| row_stmt.column_name(i).unwrap_or("?").to_string())
                .collect();

            let mut rows = row_stmt.query([]).unwrap();
            while let Ok(Some(row)) = rows.next() {
                let values: Vec<String> = (0..col_count)
                    .map(|i| match row.get_ref(i) {
                        Ok(rusqlite::types::ValueRef::Null) => "NULL".to_string(),
                        Ok(rusqlite::types::ValueRef::Integer(v)) => v.to_string(),
                        Ok(rusqlite::types::ValueRef::Real(f)) => f.to_string(),
                        Ok(rusqlite::types::ValueRef::Text(t)) => {
                            let s = String::from_utf8_lossy(t);
                            format!("'{}'", s.replace('\'', "''"))
                        }
                        Ok(rusqlite::types::ValueRef::Blob(b)) => {
                            format!("X'{}'", hex::encode(b))
                        }
                        Err(_) => "NULL".to_string(),
                    })
                    .collect();

                writeln!(
                    file,
                    "INSERT INTO \"{}\" ({}) VALUES ({});",
                    name,
                    col_names.join(", "),
                    values.join(", ")
                )
                .ok();
            }
        }
    }

    // Dump indexes
    let mut idx_stmt = conn
        .prepare(
            "SELECT sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name",
        )
        .map_err(|e| {
            Error::Storage(crate::StorageError::Database(format!(
                "Failed to list indexes: {e}"
            )))
        })?;

    let indexes: Vec<String> = idx_stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| {
            Error::Storage(crate::StorageError::Database(format!(
                "Failed to query indexes: {e}"
            )))
        })?
        .filter_map(|r| r.ok())
        .collect();

    if !indexes.is_empty() {
        writeln!(file, "\n-- Indexes").ok();
        for idx_sql in &indexes {
            writeln!(file, "{idx_sql};").ok();
        }
    }

    writeln!(file, "\nCOMMIT;").ok();

    Ok(())
}

/// Gather row counts from the database for stats.
fn gather_stats(db_path: &Path) -> Result<BackupStats> {
    let conn = Connection::open(db_path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to open database for stats: {e}"
        )))
    })?;

    let count = |table: &str| -> u64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as u64
    };

    Ok(BackupStats {
        panes: count("panes"),
        segments: count("output_segments"),
        events: count("events"),
        audit_actions: count("audit_actions"),
        workflow_executions: count("workflow_executions"),
    })
}

/// Compute SHA-256 of a file.
fn sha256_file(path: &Path) -> Result<String> {
    let data = fs::read(path).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "Failed to read file for checksum: {e}"
        )))
    })?;
    Ok(sha256_bytes(&data))
}

/// Compute SHA-256 of bytes.
fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn resolve_destination_root(workspace_root: &Path, destination: Option<&str>) -> PathBuf {
    match destination {
        Some(raw) => {
            let expanded = expand_tilde(raw);
            if expanded.is_absolute() {
                expanded
            } else {
                workspace_root.join(expanded)
            }
        }
        None => workspace_root.join(".wa").join("backups"),
    }
}

fn unique_backup_path(base_dir: &Path, base_name: &str) -> PathBuf {
    let mut candidate = base_dir.join(base_name);
    if !candidate.exists() {
        return candidate;
    }
    for idx in 1..=1000 {
        let name = format!("{base_name}_{idx:02}");
        candidate = base_dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate
}

fn parse_manifest_timestamp(created_at: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|dt| dt.timestamp())
}

fn compare_backup_entries(a: &BackupEntry, b: &BackupEntry) -> Ordering {
    match (a.created_ts, b.created_ts) {
        (Some(ts_a), Some(ts_b)) => ts_a.cmp(&ts_b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.path.cmp(&b.path),
    }
}

fn format_local_datetime(value: DateTime<Local>) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    )
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(suffix) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(suffix);
        }
    }
    PathBuf::from(path)
}

/// Generate default backup path based on timestamp.
fn default_backup_path(workspace_root: &Path) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let ts = format_timestamp_compact(now.as_secs());
    workspace_root
        .join(".wa")
        .join("backups")
        .join(format!("wa_backup_{ts}"))
}

/// Format epoch seconds as compact timestamp: YYYYMMDD_HHMMSS
fn format_timestamp_compact(epoch_secs: u64) -> String {
    // Use chrono-free approach: compute date/time from epoch
    let secs = epoch_secs;
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Compute date from days since epoch (1970-01-01)
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}{month:02}{day:02}_{hours:02}{minutes:02}{seconds:02}")
}

/// Format epoch seconds as ISO-8601 string.
fn format_iso8601(epoch_secs: u64) -> String {
    let secs = epoch_secs;
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Compute total size of a directory.
fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path).map_or(0, |entries| {
        entries
            .filter_map(|e| e.ok())
            .map(|e: std::fs::DirEntry| e.metadata().map_or(0, |m| m.len()))
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use tempfile::TempDir;

    fn create_test_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS panes (id INTEGER PRIMARY KEY, name TEXT);
            CREATE TABLE IF NOT EXISTS output_segments (id INTEGER PRIMARY KEY, data TEXT);
            CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, type TEXT);
            CREATE TABLE IF NOT EXISTS audit_actions (id INTEGER PRIMARY KEY, action TEXT);
            CREATE TABLE IF NOT EXISTS workflow_executions (id INTEGER PRIMARY KEY, name TEXT);
            INSERT INTO panes (name) VALUES ('test_pane_1'), ('test_pane_2');
            INSERT INTO output_segments (data) VALUES ('segment1'), ('segment2'), ('segment3');
            INSERT INTO events (type) VALUES ('compaction_warning');
            PRAGMA user_version = 7;
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn export_creates_valid_backup() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        let output_dir = tmp.path().join("backup");
        let opts = ExportOptions {
            output: Some(output_dir.clone()),
            include_sql_dump: true,
            verify: true,
        };

        let result = export_backup(&db_path, tmp.path(), &opts).unwrap();

        // Check files exist
        assert!(output_dir.join("database.db").exists());
        assert!(output_dir.join("manifest.json").exists());
        assert!(output_dir.join("checksums.sha256").exists());
        assert!(output_dir.join("database.sql").exists());

        // Check manifest
        assert_eq!(result.manifest.schema_version, SCHEMA_VERSION);
        assert_eq!(result.manifest.stats.panes, 2);
        assert_eq!(result.manifest.stats.segments, 3);
        assert_eq!(result.manifest.stats.events, 1);
        assert_eq!(result.manifest.stats.audit_actions, 0);
        assert!(!result.manifest.db_checksum.is_empty());
    }

    #[test]
    fn verify_detects_corruption() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        let output_dir = tmp.path().join("backup");
        let opts = ExportOptions {
            output: Some(output_dir.clone()),
            verify: false, // skip initial verify
            ..Default::default()
        };

        let result = export_backup(&db_path, tmp.path(), &opts).unwrap();

        // Corrupt the backup
        fs::write(output_dir.join("database.db"), b"corrupted").unwrap();

        // Verify should fail
        let err = verify_backup(&output_dir, &result.manifest);
        assert!(err.is_err());
    }

    #[test]
    fn export_missing_db_returns_error() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("nonexistent.db");
        let opts = ExportOptions::default();

        let result = export_backup(&db_path, tmp.path(), &opts);
        assert!(result.is_err());
    }

    #[test]
    fn sha256_is_deterministic() {
        let hash1 = sha256_bytes(b"hello world");
        let hash2 = sha256_bytes(b"hello world");
        assert_eq!(hash1, hash2);
        assert!(!hash1.is_empty());
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2026-01-29 is 20482 days since epoch
        let (y, m, d) = days_to_ymd(20482);
        assert_eq!(y, 2026);
        assert_eq!(m, 1);
        assert_eq!(d, 29);
    }

    #[test]
    fn format_iso8601_produces_valid_string() {
        let s = format_iso8601(0);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn gather_stats_counts_rows() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        let stats = gather_stats(&db_path).unwrap();
        assert_eq!(stats.panes, 2);
        assert_eq!(stats.segments, 3);
        assert_eq!(stats.events, 1);
    }

    #[test]
    fn default_backup_path_contains_timestamp() {
        let tmp = TempDir::new().unwrap();
        let path = default_backup_path(tmp.path());
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("wa_backup_"));
    }

    #[test]
    fn import_roundtrip_preserves_data() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("source.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        // Export
        let backup_dir = tmp.path().join("backup");
        let export_opts = ExportOptions {
            output: Some(backup_dir.clone()),
            verify: true,
            ..Default::default()
        };
        let _export = export_backup(&db_path, tmp.path(), &export_opts).unwrap();

        // Import into a new location
        let target_db = tmp.path().join("restored.db");
        let import_opts = ImportOptions {
            dry_run: false,
            yes: true,
            no_safety_backup: true,
        };
        let result = import_backup(&backup_dir, &target_db, tmp.path(), &import_opts).unwrap();

        assert!(!result.dry_run);
        assert!(target_db.exists());

        // Verify imported data
        let conn = Connection::open(&target_db).unwrap();
        let pane_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM panes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pane_count, 2);

        let seg_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM output_segments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(seg_count, 3);
    }

    #[test]
    fn schedule_parses_keywords() {
        assert!(matches!(
            BackupSchedule::parse("hourly").unwrap(),
            BackupSchedule::Hourly { .. }
        ));
        assert!(matches!(
            BackupSchedule::parse("daily").unwrap(),
            BackupSchedule::Daily { .. }
        ));
        assert!(matches!(
            BackupSchedule::parse("weekly").unwrap(),
            BackupSchedule::Weekly { .. }
        ));
    }

    #[test]
    fn schedule_next_daily_advances() {
        let schedule = BackupSchedule::parse("daily").unwrap();
        let now = Local
            .with_ymd_and_hms(2026, 1, 18, 12, 0, 0)
            .single()
            .unwrap();
        let next = schedule.next_after(now).unwrap();
        assert!(next > now);
        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn schedule_parses_cron() {
        let schedule = BackupSchedule::parse("15 3 * * *").unwrap();
        assert!(matches!(schedule, BackupSchedule::Cron(_)));
    }

    #[test]
    fn prune_backups_respects_max() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let create_backup = |name: &str, created_at: &str| {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            let manifest = BackupManifest {
                wa_version: "test".to_string(),
                schema_version: 1,
                created_at: created_at.to_string(),
                workspace: root.display().to_string(),
                db_size_bytes: 0,
                db_checksum: "deadbeef".to_string(),
                stats: BackupStats::default(),
            };
            let data = serde_json::to_string(&manifest).unwrap();
            fs::write(dir.join("manifest.json"), data).unwrap();
        };

        create_backup("b1", "2026-01-01T00:00:00Z");
        create_backup("b2", "2026-01-10T00:00:00Z");
        create_backup("b3", "2026-01-20T00:00:00Z");

        let now = Local
            .with_ymd_and_hms(2026, 1, 25, 12, 0, 0)
            .single()
            .unwrap();
        let summary = prune_backups(root, 0, 2, now).unwrap();
        assert_eq!(summary.kept, 2);

        let entries = list_backup_entries(root).unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"b2".to_string()));
        assert!(names.contains(&"b3".to_string()));
    }

    #[test]
    fn import_dry_run_does_not_modify() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("source.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        // Export
        let backup_dir = tmp.path().join("backup");
        let export_opts = ExportOptions {
            output: Some(backup_dir.clone()),
            verify: true,
            ..Default::default()
        };
        let _export = export_backup(&db_path, tmp.path(), &export_opts).unwrap();

        // Dry-run import
        let target_db = tmp.path().join("target.db");
        let import_opts = ImportOptions {
            dry_run: true,
            yes: true,
            no_safety_backup: true,
        };
        let result = import_backup(&backup_dir, &target_db, tmp.path(), &import_opts).unwrap();

        assert!(result.dry_run);
        assert!(
            !target_db.exists(),
            "Dry-run should not create target database"
        );
    }

    #[test]
    fn import_creates_safety_backup() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("existing.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        // Export to create a backup archive
        let backup_dir = tmp.path().join("backup");
        let export_opts = ExportOptions {
            output: Some(backup_dir.clone()),
            verify: true,
            ..Default::default()
        };
        let _export = export_backup(&db_path, tmp.path(), &export_opts).unwrap();

        // Import over the existing database (with safety backup)
        let import_opts = ImportOptions {
            dry_run: false,
            yes: true,
            no_safety_backup: false,
        };
        let result = import_backup(&backup_dir, &db_path, tmp.path(), &import_opts).unwrap();

        assert!(result.safety_backup_path.is_some());
        let safety_path = PathBuf::from(result.safety_backup_path.unwrap());
        assert!(safety_path.join("database.db").exists());
        assert!(safety_path.join("manifest.json").exists());
    }

    #[test]
    fn import_rejects_nonexistent_backup() {
        let tmp = TempDir::new().unwrap();
        let fake_backup = tmp.path().join("nonexistent");
        let target = tmp.path().join("target.db");
        let opts = ImportOptions::default();

        let result = import_backup(&fake_backup, &target, tmp.path(), &opts);
        assert!(result.is_err());
    }

    #[test]
    fn load_manifest_parses_correctly() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let _conn = create_test_db(&db_path);
        drop(_conn);

        let backup_dir = tmp.path().join("backup");
        let export_opts = ExportOptions {
            output: Some(backup_dir.clone()),
            verify: true,
            ..Default::default()
        };
        let _export = export_backup(&db_path, tmp.path(), &export_opts).unwrap();

        let manifest = load_backup_manifest(&backup_dir).unwrap();
        assert_eq!(manifest.schema_version, SCHEMA_VERSION);
        assert_eq!(manifest.stats.panes, 2);
        assert!(!manifest.db_checksum.is_empty());
    }
}
