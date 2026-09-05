//! Subprocess bridge for `beads_rust` (`br`) CLI integration.
//!
//! Wraps the `br` binary via [`SubprocessBridge`] to query issue state
//! from within FrankenTerm.  All calls fail-open: when `br` is
//! unavailable or returns errors, the bridge degrades gracefully
//! rather than blocking callers.
//!
//! Feature-gated behind `subprocess-bridge`.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::{debug, warn};

use crate::beads_types::{
    BeadIssueDetail, BeadReadinessReport, BeadStatusCounts, BeadSummary, resolve_bead_readiness,
};
use crate::subprocess_bridge::SubprocessBridge;

/// Read one exact, bounded `br` JSONL export for advisory work selection. This
/// does not invoke `br`, flush a database, or attest that the export is current
/// database state. The caller supplies the reviewed byte revision explicitly.
#[cfg(unix)]
pub fn read_bead_work_selection(
    path: &std::path::Path,
    expected_sha256: &str,
    schema_version: u16,
    now_ms: u64,
) -> Result<crate::beads_types::BeadWorkSelection, crate::beads_types::BeadWorkSelectionError> {
    read_bead_work_selection_with_hook(path, expected_sha256, schema_version, now_ms, || {})
}

#[cfg(unix)]
fn read_bead_work_selection_with_hook(
    path: &std::path::Path,
    expected_sha256: &str,
    schema_version: u16,
    now_ms: u64,
    after_read: impl FnOnce(),
) -> Result<crate::beads_types::BeadWorkSelection, crate::beads_types::BeadWorkSelectionError> {
    use crate::beads_types::{
        BEAD_WORK_GRAPH_MAX_BYTES, BeadWorkSelectionError, select_bead_work_from_jsonl,
    };
    use std::io::Read;
    // O_NONBLOCK prevents an attacker-controlled FIFO replacement from hanging
    // before fstat can reject it. The same opened regular file is retained for
    // the complete bounded read and compared with its named entry afterward.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| BeadWorkSelectionError::FileUnavailable)?;
    let before = file
        .metadata()
        .map_err(|_| BeadWorkSelectionError::FileUnavailable)?;
    if !before.is_file() {
        return Err(BeadWorkSelectionError::FileUnavailable);
    }
    if before.len() > BEAD_WORK_GRAPH_MAX_BYTES as u64 {
        return Err(BeadWorkSelectionError::BudgetExceeded);
    }
    let modified_ms = before
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .ok_or(BeadWorkSelectionError::StaleSnapshot)?;
    let fingerprint = |metadata: &std::fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.is_file(),
        )
    };
    let mut bytes = Vec::new();
    (&mut file)
        .take(BEAD_WORK_GRAPH_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| BeadWorkSelectionError::FileUnavailable)?;
    if bytes.len() > BEAD_WORK_GRAPH_MAX_BYTES {
        return Err(BeadWorkSelectionError::BudgetExceeded);
    }
    after_read();
    let after = file
        .metadata()
        .map_err(|_| BeadWorkSelectionError::ChangedDuringRead)?;
    let named =
        std::fs::symlink_metadata(path).map_err(|_| BeadWorkSelectionError::ChangedDuringRead)?;
    if fingerprint(&before) != fingerprint(&after) || fingerprint(&after) != fingerprint(&named) {
        return Err(BeadWorkSelectionError::ChangedDuringRead);
    }
    select_bead_work_from_jsonl(&bytes, expected_sha256, schema_version, modified_ms, now_ms)
}

#[cfg(not(unix))]
pub fn read_bead_work_selection(
    _path: &std::path::Path,
    _expected_sha256: &str,
    _schema_version: u16,
    _now_ms: u64,
) -> Result<crate::beads_types::BeadWorkSelection, crate::beads_types::BeadWorkSelectionError> {
    Err(crate::beads_types::BeadWorkSelectionError::UnsupportedPlatform)
}

/// High-level beads bridge wrapping the `br` CLI.
#[derive(Debug, Clone)]
pub struct BeadsBridge {
    bridge: SubprocessBridge<Value>,
}

/// Backpressure tier derived from open-bead counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeadsBackpressure {
    /// < threshold_yellow open beads
    Green,
    /// >= threshold_yellow, < threshold_red
    Yellow,
    /// >= threshold_red
    Red,
}

/// Thresholds for mapping bead counts to backpressure tiers.
#[derive(Debug, Clone, Copy)]
pub struct BeadsBackpressureConfig {
    pub yellow_threshold: usize,
    pub red_threshold: usize,
}

impl Default for BeadsBackpressureConfig {
    fn default() -> Self {
        Self {
            yellow_threshold: 50,
            red_threshold: 100,
        }
    }
}

impl BeadsBridge {
    /// Create a new beads bridge looking for `br` in PATH and `/dp`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bridge: SubprocessBridge::new("br"),
        }
    }

    /// Check whether the `br` binary can be found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.bridge.is_available()
    }

    /// List all beads (calls `br list --json`).
    ///
    /// Returns an empty vec on any failure (fail-open).
    pub fn list_all(&self) -> Vec<BeadSummary> {
        match self.bridge.invoke(&["list", "--json"]) {
            Ok(value) => Self::parse_summary_rows(value, "beads list"),
            Err(err) => {
                warn!(bridge = "br", error = %err, "beads list failed, degrading gracefully");
                Vec::new()
            }
        }
    }

    /// List open beads only.
    pub fn list_open(&self) -> Vec<BeadSummary> {
        match self.bridge.invoke(&["list", "--status=open", "--json"]) {
            Ok(value) => Self::parse_summary_rows(value, "open beads list"),
            Err(err) => {
                warn!(bridge = "br", error = %err, unavailable = true, "open beads list failed");
                Vec::new()
            }
        }
    }

    /// Show a single bead by ID (calls `br show <id> --json`).
    ///
    /// Returns `None` on failure (fail-open).
    pub fn show(&self, id: &str) -> Option<BeadSummary> {
        match self.bridge.invoke(&["show", id, "--json"]) {
            Ok(value) => {
                let mut beads = match parse_br_rows::<BeadSummary>(value) {
                    Ok(beads) => beads,
                    Err(err) => {
                        warn!(bridge = "br", id, error = %err, "bead show parse failed");
                        return None;
                    }
                };
                if beads.is_empty() {
                    debug!(bridge = "br", id, "bead not found");
                    None
                } else {
                    Some(beads.swap_remove(0))
                }
            }
            Err(err) => {
                warn!(bridge = "br", id, error = %err, "bead show failed");
                None
            }
        }
    }

    /// List all beads including closed items.
    ///
    /// Returns an empty vec on any failure (fail-open).
    pub fn list_all_with_closed(&self) -> Vec<BeadSummary> {
        match self
            .bridge
            .invoke(&["list", "--all", "--limit", "0", "--json"])
        {
            Ok(value) => Self::parse_summary_rows(value, "beads list --all"),
            Err(err) => {
                warn!(
                    bridge = "br",
                    error = %err,
                    "beads list --all failed, degrading gracefully"
                );
                Vec::new()
            }
        }
    }

    /// Load detailed issue records (`br show`) for every known bead.
    ///
    /// Uses degraded fallback records when detail resolution fails.
    pub fn list_all_details(&self) -> Vec<BeadIssueDetail> {
        let summaries = self.list_all_with_closed();
        if summaries.is_empty() {
            return Vec::new();
        }

        let detail_bridge: SubprocessBridge<Value> =
            SubprocessBridge::new(self.bridge.binary_name());
        let mut details = Vec::with_capacity(summaries.len());

        for summary in summaries {
            let issue_id = summary.id.clone();
            match detail_bridge.invoke(&["show", &issue_id, "--json"]) {
                Ok(value) => {
                    let mut rows = match parse_br_rows::<BeadIssueDetail>(value) {
                        Ok(rows) => rows,
                        Err(err) => {
                            warn!(
                                bridge = "br",
                                issue_id,
                                error = %err,
                                "detail parse failed, using partial graph fallback"
                            );
                            details.push(BeadIssueDetail::from_summary(summary));
                            continue;
                        }
                    };
                    if let Some(mut detail) = rows.pop() {
                        detail.ingest_warning = None;
                        details.push(detail);
                    } else {
                        warn!(
                            bridge = "br",
                            issue_id, "empty show result, using partial graph fallback"
                        );
                        details.push(BeadIssueDetail::from_summary(summary));
                    }
                }
                Err(err) => {
                    warn!(
                        bridge = "br",
                        issue_id,
                        error = %err,
                        "detail fetch failed, using partial graph fallback"
                    );
                    details.push(BeadIssueDetail::from_summary(summary));
                }
            }
        }

        details
    }

    /// Resolve actionable/ready candidates from the full Beads DAG.
    pub fn readiness_report(&self) -> BeadReadinessReport {
        let details = self.list_all_details();
        resolve_bead_readiness(&details)
    }

    /// Count beads by status.
    pub fn count_by_status(&self) -> BeadStatusCounts {
        let beads = self.list_all();
        BeadStatusCounts::from_summaries(&beads)
    }

    /// Count only open beads grouped by priority.
    pub fn open_by_priority(&self) -> HashMap<u8, usize> {
        let beads = self.list_open();
        let mut counts: HashMap<u8, usize> = HashMap::new();
        for bead in &beads {
            *counts.entry(bead.priority).or_default() += 1;
        }
        counts
    }

    /// Compute backpressure tier from actionable bead count.
    pub fn backpressure(&self, config: &BeadsBackpressureConfig) -> BeadsBackpressure {
        let counts = self.count_by_status();
        let actionable = counts.actionable();
        if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        }
    }

    /// Compute backpressure with default thresholds.
    pub fn backpressure_default(&self) -> BeadsBackpressure {
        self.backpressure(&BeadsBackpressureConfig::default())
    }

    fn parse_summary_rows(value: Value, operation: &'static str) -> Vec<BeadSummary> {
        match parse_br_rows::<BeadSummary>(value) {
            Ok(beads) => {
                debug!(
                    bridge = "br",
                    operation,
                    count = beads.len(),
                    "parsed beads rows"
                );
                beads
            }
            Err(err) => {
                warn!(bridge = "br", operation, error = %err, "beads output parse failed");
                Vec::new()
            }
        }
    }
}

impl Default for BeadsBridge {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_br_rows<T: DeserializeOwned>(value: Value) -> Result<Vec<T>, serde_json::Error> {
    match value {
        Value::Object(mut object) => {
            if let Some(rows) = object.remove("issues") {
                serde_json::from_value(rows)
            } else {
                serde_json::from_value(Value::Object(object)).map(|row| vec![row])
            }
        }
        rows => serde_json::from_value(rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads_types::{
        BeadDependencyRef, BeadIssueDetail, BeadIssueType, BeadResolverReasonCode, BeadStatus,
    };
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn bead_work_selection_reads_exact_regular_snapshot_and_refuses_replaced_stale_or_oversized_input()
     {
        use crate::beads_types::{BEAD_WORK_GRAPH_MAX_BYTES, BeadWorkSelectionError};
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("issues.jsonl");
        let bytes = br#"{"id":"ready-docs","status":"open","priority":2,"issue_type":"docs","description":"private-body-do-not-log"}
"#;
        std::fs::write(&path, bytes).unwrap();
        let hash = hex::encode(Sha256::digest(bytes));
        let now = || {
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
        };
        let report = read_bead_work_selection(&path, &hash, 1, now()).unwrap();
        assert_eq!(report.selected_id(), Some("ready-docs"));
        assert_eq!(report.input_sha256(), hash);
        assert_eq!(report.source_bytes, bytes.len());
        assert_eq!(
            read_bead_work_selection(&path, &"0".repeat(64), 1, now()).unwrap_err(),
            BeadWorkSelectionError::RevisionMismatch
        );
        let backup = fixture.path().join("retained-prior-issues.jsonl");
        let result = read_bead_work_selection_with_hook(&path, &hash, 1, now(), || {
            std::fs::rename(&path, &backup).unwrap();
            std::fs::write(&path, bytes).unwrap();
        });
        assert_eq!(
            result.unwrap_err(),
            BeadWorkSelectionError::ChangedDuringRead,
            "same-byte replacement must not inherit the opened file's authority"
        );
        assert!(backup.is_file());
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            read_bead_work_selection(&path, &hash, 1, now()).unwrap_err(),
            BeadWorkSelectionError::StaleSnapshot
        );
        file.set_len(BEAD_WORK_GRAPH_MAX_BYTES as u64 + 1).unwrap();
        assert_eq!(
            read_bead_work_selection(&path, &hash, 1, now()).unwrap_err(),
            BeadWorkSelectionError::BudgetExceeded
        );
        assert_eq!(
            read_bead_work_selection(fixture.path(), &hash, 1, now()).unwrap_err(),
            BeadWorkSelectionError::FileUnavailable
        );
        let link = fixture.path().join("symlink.jsonl");
        symlink(&backup, &link).unwrap();
        assert_eq!(
            read_bead_work_selection(&link, &hash, 1, now()).unwrap_err(),
            BeadWorkSelectionError::FileUnavailable
        );
        println!(
            "BEAD_GRAPH_FILE hash={hash} selected=ready-docs replacement_stale_oversize_symlink_refused=true live_br_invocations=0"
        );
    }

    fn sample_bead(id: &str, status: BeadStatus, priority: u8) -> BeadSummary {
        BeadSummary {
            id: id.to_string(),
            title: format!("Bead {}", id),
            status,
            priority,
            issue_type: BeadIssueType::Task,
            assignee: None,
            labels: vec![],
            dependency_count: 0,
            dependent_count: 0,
            extra: HashMap::new(),
        }
    }

    fn sample_detail(id: &str, status: BeadStatus, priority: u8) -> BeadIssueDetail {
        BeadIssueDetail {
            id: id.to_string(),
            title: format!("Detail {}", id),
            status,
            priority,
            issue_type: BeadIssueType::Task,
            assignee: None,
            labels: Vec::new(),
            dependencies: Vec::new(),
            dependents: Vec::new(),
            parent: None,
            ingest_warning: None,
            extra: HashMap::new(),
        }
    }

    // -------------------------------------------------------------------------
    // BeadsBridge construction
    // -------------------------------------------------------------------------

    #[test]
    fn test_beads_bridge_new() {
        let bridge = BeadsBridge::new();
        assert_eq!(bridge.bridge.binary_name(), "br");
    }

    #[test]
    fn test_beads_bridge_default() {
        let bridge = BeadsBridge::default();
        assert_eq!(bridge.bridge.binary_name(), "br");
    }

    // -------------------------------------------------------------------------
    // BeadsBackpressure
    // -------------------------------------------------------------------------

    #[test]
    fn test_backpressure_green_below_threshold() {
        let config = BeadsBackpressureConfig {
            yellow_threshold: 10,
            red_threshold: 20,
        };
        let counts = BeadStatusCounts {
            open: 5,
            in_progress: 3,
            blocked: 0,
            deferred: 0,
            closed: 0,
        };
        let actionable = counts.actionable();
        let tier = if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Green);
    }

    #[test]
    fn test_backpressure_yellow_at_threshold() {
        let config = BeadsBackpressureConfig {
            yellow_threshold: 10,
            red_threshold: 20,
        };
        let actionable = 10;
        let tier = if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Yellow);
    }

    #[test]
    fn test_backpressure_yellow_above_threshold() {
        let config = BeadsBackpressureConfig {
            yellow_threshold: 10,
            red_threshold: 20,
        };
        let actionable = 15;
        let tier = if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Yellow);
    }

    #[test]
    fn test_backpressure_red_at_threshold() {
        let config = BeadsBackpressureConfig {
            yellow_threshold: 10,
            red_threshold: 20,
        };
        let actionable = 20;
        let tier = if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Red);
    }

    #[test]
    fn test_backpressure_red_above_critical() {
        let config = BeadsBackpressureConfig {
            yellow_threshold: 10,
            red_threshold: 20,
        };
        let actionable = 100;
        let tier = if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Red);
    }

    #[test]
    fn test_backpressure_green_with_zero() {
        let config = BeadsBackpressureConfig::default();
        let actionable = 0;
        let tier = if actionable >= config.red_threshold {
            BeadsBackpressure::Red
        } else if actionable >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Green);
    }

    // -------------------------------------------------------------------------
    // BeadsBackpressureConfig
    // -------------------------------------------------------------------------

    #[test]
    fn test_backpressure_config_default() {
        let config = BeadsBackpressureConfig::default();
        assert_eq!(config.yellow_threshold, 50);
        assert_eq!(config.red_threshold, 100);
    }

    #[test]
    fn test_backpressure_config_custom() {
        let config = BeadsBackpressureConfig {
            yellow_threshold: 5,
            red_threshold: 10,
        };
        assert_eq!(config.yellow_threshold, 5);
        assert_eq!(config.red_threshold, 10);
    }

    // -------------------------------------------------------------------------
    // Fail-open behavior (br not available)
    // -------------------------------------------------------------------------

    #[test]
    fn test_fail_open_list_all_when_br_unavailable() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        let result = bridge.list_all();
        assert!(result.is_empty());
    }

    #[test]
    fn test_fail_open_list_open_when_br_unavailable() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        let result = bridge.list_open();
        assert!(result.is_empty());
    }

    #[test]
    fn test_fail_open_show_when_br_unavailable() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        let result = bridge.show("ft-abc");
        assert!(result.is_none());
    }

    #[test]
    fn test_fail_open_count_when_br_unavailable() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        let counts = bridge.count_by_status();
        assert_eq!(counts.total(), 0);
    }

    #[test]
    fn test_fail_open_backpressure_when_br_unavailable() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        // With no data, actionable count is 0 → Green
        let tier = bridge.backpressure_default();
        assert_eq!(tier, BeadsBackpressure::Green);
    }

    // -------------------------------------------------------------------------
    // br JSON output shape parsing
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_br_rows_accepts_legacy_array() {
        let rows: Vec<BeadSummary> = parse_br_rows(json!([
            {
                "id": "ft-a",
                "title": "legacy array",
                "status": "open",
                "priority": 2,
                "issue_type": "task"
            }
        ]))
        .expect("legacy br list rows parse");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "ft-a");
        assert_eq!(rows[0].status, BeadStatus::Open);
    }

    #[test]
    fn test_parse_br_rows_accepts_current_issues_envelope() {
        let rows: Vec<BeadSummary> = parse_br_rows(json!({
            "issues": [
                {
                    "id": "ft-b",
                    "title": "current envelope",
                    "status": "in_progress",
                    "priority": 1,
                    "issue_type": "bug",
                    "assignee": "Codex"
                }
            ],
            "total": 1,
            "limit": 50,
            "offset": 0,
            "has_more": false
        }))
        .expect("current br list envelope parses");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "ft-b");
        assert_eq!(rows[0].status, BeadStatus::InProgress);
        assert_eq!(rows[0].assignee.as_deref(), Some("Codex"));
    }

    #[test]
    fn test_parse_br_rows_accepts_single_show_object() {
        let rows: Vec<BeadSummary> = parse_br_rows(json!({
            "id": "ft-c",
            "title": "single show object",
            "status": "blocked",
            "priority": 3,
            "issue_type": "task"
        }))
        .expect("single br show object parses");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "ft-c");
        assert_eq!(rows[0].status, BeadStatus::Blocked);
    }

    // -------------------------------------------------------------------------
    // StatusCounts-derived backpressure
    // -------------------------------------------------------------------------

    #[test]
    fn test_counts_to_backpressure_integration() {
        let beads: Vec<BeadSummary> = (0..60)
            .map(|i| sample_bead(&format!("ft-{}", i), BeadStatus::Open, 2))
            .collect();
        let counts = BeadStatusCounts::from_summaries(&beads);
        let config = BeadsBackpressureConfig::default();
        let tier = if counts.actionable() >= config.red_threshold {
            BeadsBackpressure::Red
        } else if counts.actionable() >= config.yellow_threshold {
            BeadsBackpressure::Yellow
        } else {
            BeadsBackpressure::Green
        };
        assert_eq!(tier, BeadsBackpressure::Yellow);
    }

    #[test]
    fn test_counts_blocked_not_actionable() {
        let beads: Vec<BeadSummary> = (0..60)
            .map(|i| sample_bead(&format!("ft-{}", i), BeadStatus::Blocked, 1))
            .collect();
        let counts = BeadStatusCounts::from_summaries(&beads);
        assert_eq!(counts.actionable(), 0);
    }

    #[test]
    fn test_counts_mixed_statuses() {
        let beads = vec![
            sample_bead("a", BeadStatus::Open, 0),
            sample_bead("b", BeadStatus::InProgress, 1),
            sample_bead("c", BeadStatus::Blocked, 2),
            sample_bead("d", BeadStatus::Deferred, 3),
            sample_bead("e", BeadStatus::Closed, 0),
        ];
        let counts = BeadStatusCounts::from_summaries(&beads);
        assert_eq!(counts.actionable(), 2);
        assert_eq!(counts.total(), 5);
    }

    // -------------------------------------------------------------------------
    // open_by_priority
    // -------------------------------------------------------------------------

    #[test]
    fn test_open_by_priority_empty_when_br_unavailable() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        let by_priority = bridge.open_by_priority();
        assert!(by_priority.is_empty());
    }

    // -------------------------------------------------------------------------
    // BeadsBackpressure equality
    // -------------------------------------------------------------------------

    #[test]
    fn test_backpressure_eq() {
        assert_eq!(BeadsBackpressure::Green, BeadsBackpressure::Green);
        assert_eq!(BeadsBackpressure::Yellow, BeadsBackpressure::Yellow);
        assert_eq!(BeadsBackpressure::Red, BeadsBackpressure::Red);
        assert_ne!(BeadsBackpressure::Green, BeadsBackpressure::Red);
    }

    #[test]
    fn test_backpressure_copy() {
        let tier = BeadsBackpressure::Yellow;
        let copy = tier;
        assert_eq!(tier, copy);
    }

    #[test]
    fn test_backpressure_debug() {
        let dbg = format!("{:?}", BeadsBackpressure::Red);
        assert!(dbg.contains("Red"));
    }

    // -------------------------------------------------------------------------
    // Bridge availability
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_available_false_for_missing() {
        let bridge = BeadsBridge {
            bridge: SubprocessBridge::new("definitely-missing-br-binary-xyz"),
        };
        assert!(!bridge.is_available());
    }

    // -------------------------------------------------------------------------
    // Readiness resolver plumbing
    // -------------------------------------------------------------------------

    #[test]
    fn test_readiness_report_from_details_produces_ready_ids() {
        let mut dep = sample_detail("dep", BeadStatus::Closed, 2);
        dep.dependents.push(BeadDependencyRef {
            id: "task".to_string(),
            title: None,
            status: Some("open".to_string()),
            priority: Some(1),
            dependency_type: Some("blocks".to_string()),
        });

        let mut task = sample_detail("task", BeadStatus::Open, 1);
        task.dependencies.push(BeadDependencyRef {
            id: "dep".to_string(),
            title: None,
            status: Some("closed".to_string()),
            priority: Some(2),
            dependency_type: Some("blocks".to_string()),
        });

        let report = resolve_bead_readiness(&[dep, task]);
        assert_eq!(report.ready_ids, vec!["task".to_string()]);
        let task_entry = report
            .candidates
            .iter()
            .find(|candidate| candidate.id == "task")
            .unwrap();
        assert_eq!(task_entry.blocker_count, 0);
        assert!(task_entry.ready);
    }

    #[test]
    fn test_readiness_report_marks_partial_graph_reason() {
        let summary = sample_bead("fallback", BeadStatus::Open, 1);
        let detail = BeadIssueDetail::from_summary(summary);
        let report = resolve_bead_readiness(&[detail]);
        assert!(
            report
                .degraded_reason_codes
                .contains(&BeadResolverReasonCode::PartialGraphData)
        );
    }

    // -------------------------------------------------------------------------
    // Integration with real br (only runs if br is on PATH)
    // -------------------------------------------------------------------------

    #[test]
    fn test_real_br_list_if_available() {
        let bridge = BeadsBridge::new();
        if !bridge.is_available() {
            return; // Skip test when br not installed
        }
        let beads = bridge.list_all();
        // If br is available, it should return at least an empty array
        // (we can't assert non-empty since it depends on repo state)
        let _ = beads.len();
    }

    #[test]
    fn test_real_br_count_if_available() {
        let bridge = BeadsBridge::new();
        if !bridge.is_available() {
            return;
        }
        let counts = bridge.count_by_status();
        // Validates successful parsing without panicking
        let _ = counts.total();
    }
}
