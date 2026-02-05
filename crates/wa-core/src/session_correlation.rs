//! Cass session correlation utilities.
//!
//! Provides a deterministic correlation algorithm for matching wa-observed
//! agent sessions to cass sessions using project path and start-time windows.

use crate::cass::{
    CassAgent, CassClient, CassSession, CassSessionSummary, parse_cass_timestamp_ms,
};
use crate::storage::{AgentSessionRecord, StorageHandle};
use crate::wezterm::CwdInfo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Correlation algorithm version (for metadata/auditing).
pub const CASS_CORRELATION_VERSION: &str = "v1";

const DEFAULT_WINDOW_MS: i64 = 10 * 60 * 1_000; // 10 minutes

/// Options controlling cass correlation behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CassCorrelationOptions {
    /// Time window before session start to consider (ms).
    pub window_before_ms: i64,
    /// Time window after session start to consider (ms).
    pub window_after_ms: i64,
    /// Manual override for cass session id (skips cass lookup).
    pub override_session_id: Option<String>,
}

impl Default for CassCorrelationOptions {
    fn default() -> Self {
        Self {
            window_before_ms: DEFAULT_WINDOW_MS,
            window_after_ms: DEFAULT_WINDOW_MS,
            override_session_id: None,
        }
    }
}

/// Correlation outcome status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrelationStatus {
    Linked,
    Unlinked,
    Error,
}

/// Correlation result for a cass session lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCorrelation {
    pub status: CorrelationStatus,
    pub external_id: Option<String>,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub candidates_considered: usize,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub selected_started_at_ms: Option<i64>,
    pub algorithm_version: String,
    pub error: Option<String>,
}

impl SessionCorrelation {
    fn linked(
        external_id: String,
        confidence: f64,
        reasons: Vec<String>,
        candidates_considered: usize,
        window_start_ms: i64,
        window_end_ms: i64,
        selected_started_at_ms: Option<i64>,
    ) -> Self {
        Self {
            status: CorrelationStatus::Linked,
            external_id: Some(external_id),
            confidence,
            reasons,
            candidates_considered,
            window_start_ms,
            window_end_ms,
            selected_started_at_ms,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        }
    }

    fn unlinked(
        reasons: Vec<String>,
        candidates_considered: usize,
        window_start_ms: i64,
        window_end_ms: i64,
    ) -> Self {
        Self {
            status: CorrelationStatus::Unlinked,
            external_id: None,
            confidence: 0.0,
            reasons,
            candidates_considered,
            window_start_ms,
            window_end_ms,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        }
    }

    fn error(
        message: String,
        reasons: Vec<String>,
        window_start_ms: i64,
        window_end_ms: i64,
    ) -> Self {
        Self {
            status: CorrelationStatus::Error,
            external_id: None,
            confidence: 0.0,
            reasons,
            candidates_considered: 0,
            window_start_ms,
            window_end_ms,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: Some(message),
        }
    }

    #[must_use]
    pub fn to_external_meta(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            serde_json::json!({
                "status": "error",
                "error": "correlation_meta_serialize_failed",
                "algorithm_version": CASS_CORRELATION_VERSION,
            })
        })
    }
}

/// Correlate a cass session list using a start-time window.
#[must_use]
pub fn correlate_from_sessions(
    sessions: &[CassSession],
    session_started_at_ms: i64,
    options: &CassCorrelationOptions,
) -> SessionCorrelation {
    if let Some(override_id) = options.override_session_id.as_ref() {
        return SessionCorrelation::linked(
            override_id.clone(),
            1.0,
            vec!["manual_override".to_string()],
            sessions.len(),
            window_start(session_started_at_ms, options),
            window_end(session_started_at_ms, options),
            None,
        );
    }

    let window_start_ms = window_start(session_started_at_ms, options);
    let window_end_ms = window_end(session_started_at_ms, options);

    let mut skipped_missing_id = 0usize;
    let mut skipped_missing_time = 0usize;
    let mut skipped_outside_window = 0usize;
    let mut candidates = Vec::new();

    for session in sessions {
        let Some(session_id) = session.session_id.clone() else {
            skipped_missing_id += 1;
            continue;
        };
        let Some(started_at_raw) = session.started_at.as_deref() else {
            skipped_missing_time += 1;
            continue;
        };
        let Some(started_at_ms) = parse_cass_timestamp_ms(started_at_raw) else {
            skipped_missing_time += 1;
            continue;
        };

        if started_at_ms < window_start_ms || started_at_ms > window_end_ms {
            skipped_outside_window += 1;
            continue;
        }

        let diff_ms = if session_started_at_ms >= started_at_ms {
            session_started_at_ms - started_at_ms
        } else {
            started_at_ms - session_started_at_ms
        };
        candidates.push(Candidate {
            session_id,
            started_at_ms,
            diff_ms,
        });
    }

    let mut reasons = vec![format!(
        "sessions_total={} in_window={} skipped_missing_id={} skipped_missing_time={} skipped_outside_window={}",
        sessions.len(),
        candidates.len(),
        skipped_missing_id,
        skipped_missing_time,
        skipped_outside_window
    )];

    if candidates.is_empty() {
        reasons.push("no_candidates_in_window".to_string());
        return SessionCorrelation::unlinked(reasons, 0, window_start_ms, window_end_ms);
    }

    candidates.sort_by(|a, b| {
        a.diff_ms
            .cmp(&b.diff_ms)
            .then_with(|| b.started_at_ms.cmp(&a.started_at_ms))
    });

    if candidates.len() > 1 {
        reasons.push("ambiguous_candidates".to_string());
    }

    let selected = &candidates[0];
    let tie_breaker = if candidates.len() > 1 && candidates[0].diff_ms == candidates[1].diff_ms {
        "latest_started_at"
    } else {
        "closest_start_time"
    };

    let gap_ms = candidates
        .get(1)
        .map(|second| second.diff_ms.saturating_sub(selected.diff_ms).max(0));

    let confidence = compute_confidence(
        candidates.len(),
        selected.diff_ms,
        gap_ms,
        options.window_before_ms + options.window_after_ms,
    );

    reasons.push(format!(
        "selected session_id={} diff_ms={} tie_breaker={}",
        selected.session_id, selected.diff_ms, tie_breaker
    ));
    if let Some(gap) = gap_ms {
        reasons.push(format!("runner_up_gap_ms={gap}"));
    }

    SessionCorrelation::linked(
        selected.session_id.clone(),
        confidence,
        reasons,
        candidates.len(),
        window_start_ms,
        window_end_ms,
        Some(selected.started_at_ms),
    )
}

/// Correlate a session by querying cass for a project path.
pub async fn correlate_with_cass(
    cass: &CassClient,
    project_path: &Path,
    agent: CassAgent,
    session_started_at_ms: i64,
    options: &CassCorrelationOptions,
) -> SessionCorrelation {
    if options.override_session_id.is_some() {
        return correlate_from_sessions(&[], session_started_at_ms, options);
    }

    match cass.search_sessions(project_path, Some(agent)).await {
        Ok(sessions) => correlate_from_sessions(&sessions, session_started_at_ms, options),
        Err(err) => SessionCorrelation::error(
            err.to_string(),
            vec!["cass_search_failed".to_string()],
            window_start(session_started_at_ms, options),
            window_end(session_started_at_ms, options),
        ),
    }
}

/// Correlate and persist the cass session for a pane/session.
pub async fn correlate_and_persist_for_pane(
    storage: &StorageHandle,
    cass: &CassClient,
    pane_id: u64,
    agent: CassAgent,
    session_started_at_ms: i64,
    options: &CassCorrelationOptions,
) -> Result<SessionCorrelation, String> {
    let window_start_ms = window_start(session_started_at_ms, options);
    let window_end_ms = window_end(session_started_at_ms, options);

    let correlation = if options.override_session_id.is_some() {
        correlate_from_sessions(&[], session_started_at_ms, options)
    } else {
        let pane = storage.get_pane(pane_id).await.map_err(|e| e.to_string())?;

        let project_path = pane
            .as_ref()
            .and_then(|record| record.cwd.as_ref())
            .and_then(|cwd| resolve_project_path(cwd));

        if let Some(path) = project_path {
            correlate_with_cass(cass, &path, agent, session_started_at_ms, options).await
        } else {
            SessionCorrelation::unlinked(
                vec!["missing_or_remote_cwd".to_string()],
                0,
                window_start_ms,
                window_end_ms,
            )
        }
    };

    let mut session_record = select_session_record(storage, pane_id, agent, session_started_at_ms)
        .await
        .map_err(|e| e.to_string())?;
    session_record.external_id = correlation.external_id.clone();
    session_record.external_meta = Some(correlation.to_external_meta());

    storage
        .upsert_agent_session(session_record)
        .await
        .map_err(|e| e.to_string())?;

    Ok(correlation)
}

/// Options controlling cass summary refresh behavior.
///
/// Typical trigger points:
/// - manual: invoked by a status/diagnostic command that asks for a refresh
/// - workflow: invoked after usage-limit handling to capture final totals
/// - periodic: optional background refresh with a guard interval
#[derive(Debug, Clone)]
pub struct CassSummaryRefreshOptions {
    /// Minimum interval between cass refreshes (ms).
    pub min_refresh_interval_ms: i64,
    /// Force refresh even if the cached summary is recent.
    pub force: bool,
}

impl Default for CassSummaryRefreshOptions {
    fn default() -> Self {
        Self {
            min_refresh_interval_ms: 5 * 60 * 1_000,
            force: false,
        }
    }
}

/// Refresh token/message accounting from cass and persist to agent_sessions.
///
/// This is designed to be called by higher-level surfaces (CLI, workflow steps,
/// or periodic health checks) rather than automatically polling cass on ingest.
pub async fn refresh_cass_summary_for_session(
    storage: &StorageHandle,
    cass: &CassClient,
    session_id: i64,
    options: &CassSummaryRefreshOptions,
) -> Result<CassSessionSummary, String> {
    let mut record = storage
        .get_agent_session(session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("agent session not found: {session_id}"))?;

    let Some(external_id) = record.external_id.clone() else {
        return Err("cass external_id missing on agent session".to_string());
    };

    let now = now_ms();
    if !options.force {
        if let Some(last_refresh) = record.external_meta.as_ref().and_then(extract_refresh_ms) {
            let elapsed = now.saturating_sub(last_refresh);
            if elapsed < options.min_refresh_interval_ms {
                if let Some(summary) = record
                    .external_meta
                    .as_ref()
                    .and_then(extract_summary_from_meta)
                {
                    return Ok(summary);
                }
            }
        }
    }

    let session = cass
        .query_session(&external_id)
        .await
        .map_err(|err| err.to_string())?;

    let summary = CassSessionSummary::from_session(&session);
    record.total_tokens = summary.total_tokens;
    record.input_tokens = summary.input_tokens;
    record.output_tokens = summary.output_tokens;

    if record.ended_at.is_none() {
        record.ended_at = summary.session_ended_at_ms;
    }

    record.external_meta = Some(merge_external_meta(
        record.external_meta.take(),
        &summary,
        now,
        &external_id,
    ));

    storage
        .upsert_agent_session(record)
        .await
        .map_err(|e| e.to_string())?;

    Ok(summary)
}

fn window_start(session_started_at_ms: i64, options: &CassCorrelationOptions) -> i64 {
    session_started_at_ms.saturating_sub(options.window_before_ms.max(0))
}

fn window_end(session_started_at_ms: i64, options: &CassCorrelationOptions) -> i64 {
    session_started_at_ms.saturating_add(options.window_after_ms.max(0))
}

async fn select_session_record(
    storage: &StorageHandle,
    pane_id: u64,
    agent: CassAgent,
    session_started_at_ms: i64,
) -> Result<AgentSessionRecord, crate::Error> {
    let agent_type = agent.as_str();
    let sessions = storage.get_sessions_for_pane(pane_id).await?;
    if let Some(existing) = sessions
        .iter()
        .find(|record| record.agent_type == agent_type && record.ended_at.is_none())
    {
        return Ok(existing.clone());
    }

    let mut record = AgentSessionRecord::new_start(pane_id, agent_type);
    record.started_at = session_started_at_ms;
    Ok(record)
}

fn resolve_project_path(cwd: &str) -> Option<PathBuf> {
    let parsed = CwdInfo::parse(cwd);
    if parsed.is_remote || parsed.path.is_empty() {
        return None;
    }
    let path = PathBuf::from(parsed.path);
    find_repo_root(&path).or(Some(path))
}

fn find_repo_root(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        let git_path = ancestor.join(".git");
        if let Ok(meta) = std::fs::metadata(&git_path) {
            if meta.is_dir() || meta.is_file() {
                return Some(ancestor.to_path_buf());
            }
        }
    }
    None
}

fn merge_external_meta(
    existing: Option<Value>,
    summary: &CassSessionSummary,
    refreshed_at_ms: i64,
    external_id: &str,
) -> Value {
    let mut map = match existing {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };

    map.insert(
        "cass_summary".to_string(),
        serde_json::to_value(summary).unwrap_or(Value::Null),
    );
    map.insert(
        "cass_refreshed_at_ms".to_string(),
        Value::Number(refreshed_at_ms.into()),
    );
    map.insert(
        "cass_session_id".to_string(),
        Value::String(external_id.to_string()),
    );

    Value::Object(map)
}

fn extract_refresh_ms(meta: &Value) -> Option<i64> {
    meta.get("cass_refreshed_at_ms").and_then(Value::as_i64)
}

fn extract_summary_from_meta(meta: &Value) -> Option<CassSessionSummary> {
    meta.get("cass_summary")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn compute_confidence(
    candidate_count: usize,
    selected_diff_ms: i64,
    runner_up_gap_ms: Option<i64>,
    window_span_ms: i64,
) -> f64 {
    let span = window_span_ms.max(1) as f64;
    let closeness = 1.0 - (selected_diff_ms as f64 / span).clamp(0.0, 1.0);

    let mut confidence = if candidate_count <= 1 {
        0.25_f64.mul_add(closeness, 0.7)
    } else {
        0.2_f64.mul_add(closeness, 0.5)
    };

    if let Some(gap) = runner_up_gap_ms {
        if gap >= 120_000 {
            confidence += 0.1;
        } else {
            confidence -= 0.05;
        }
    }

    confidence.clamp(0.0, 0.95)
}

#[derive(Debug, Clone)]
struct Candidate {
    session_id: String,
    started_at_ms: i64,
    diff_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::PaneRecord;

    fn make_session(id: &str, started_at: &str) -> CassSession {
        CassSession {
            session_id: Some(id.to_string()),
            started_at: Some(started_at.to_string()),
            ..CassSession::default()
        }
    }

    #[test]
    fn correlate_single_candidate() {
        let start_ms = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();
        let sessions = vec![make_session("cass-1", "2026-01-29T17:01:00Z")];
        let result =
            correlate_from_sessions(&sessions, start_ms, &CassCorrelationOptions::default());

        assert_eq!(result.status, CorrelationStatus::Linked);
        assert_eq!(result.external_id.as_deref(), Some("cass-1"));
        assert!(result.confidence > 0.5);
    }

    #[test]
    fn correlate_tie_breaks_latest_start() {
        let start_ms = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();
        let sessions = vec![
            make_session("cass-old", "2026-01-29T16:58:00Z"),
            make_session("cass-new", "2026-01-29T17:02:00Z"),
        ];

        let result =
            correlate_from_sessions(&sessions, start_ms, &CassCorrelationOptions::default());
        assert_eq!(result.external_id.as_deref(), Some("cass-new"));
    }

    #[test]
    fn correlate_no_candidates_in_window() {
        let start_ms = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();
        let sessions = vec![make_session("cass-1", "2026-01-29T14:00:00Z")];
        let result =
            correlate_from_sessions(&sessions, start_ms, &CassCorrelationOptions::default());

        assert_eq!(result.status, CorrelationStatus::Unlinked);
        assert!(result.external_id.is_none());
        assert!(result.confidence == 0.0);
    }

    #[test]
    fn correlate_manual_override() {
        let start_ms = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();
        let mut options = CassCorrelationOptions::default();
        options.override_session_id = Some("cass-override".to_string());

        let result = correlate_from_sessions(&[], start_ms, &options);
        assert_eq!(result.external_id.as_deref(), Some("cass-override"));
        assert!((result.confidence - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn correlate_and_persist_override_updates_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let db_path_str = db_path.to_string_lossy().to_string();
        let handle = StorageHandle::new(&db_path_str).await.unwrap();

        let now = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();

        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: Some(dir.path().to_string_lossy().to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        handle.upsert_pane(pane).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "claude_code");
        session.started_at = now;
        let session_id = handle.upsert_agent_session(session).await.unwrap();

        let mut options = CassCorrelationOptions::default();
        options.override_session_id = Some("cass-override".to_string());

        let cass = CassClient::new();
        let correlation =
            correlate_and_persist_for_pane(&handle, &cass, 1, CassAgent::ClaudeCode, now, &options)
                .await
                .unwrap();

        let updated = handle.get_agent_session(session_id).await.unwrap().unwrap();
        assert_eq!(updated.external_id.as_deref(), Some("cass-override"));
        assert!(updated.external_meta.is_some());
        assert_eq!(correlation.status, CorrelationStatus::Linked);

        handle.shutdown().await.unwrap();
    }
}
