//! Adapter layer: QueryClient data types → render-ready view models.
//!
//! This module sits between the `QueryClient` trait (data source) and the
//! rendering code (ratatui or ftui). Its job is to normalize, format, and
//! transform raw data into render-ready row models that rendering code can
//! consume without further logic.
//!
//! ## Design rules
//!
//! 1. **Framework-agnostic.** No ratatui or ftui imports. Uses `ftui_compat`
//!    types (StyleSpec, ColorSpec) for styling hints.
//! 2. **Deterministic.** Given the same input, always produces the same output.
//!    No system clock, no network, no randomness.
//! 3. **Testable.** Every adapter function has unit tests on representative data.
//! 4. **Redaction-safe.** Secret-like patterns are never surfaced in view models.
//!
//! ## Architecture
//!
//! ```text
//! QueryClient.list_panes()    → adapt_pane()      → PaneRow
//! QueryClient.list_events()   → adapt_event()     → EventRow
//! QueryClient.list_triage()   → adapt_triage()    → TriageRow
//! QueryClient.list_history()  → adapt_history()   → HistoryRow
//! QueryClient.search()        → adapt_search()    → SearchRow
//! QueryClient.health()        → adapt_health()    → HealthModel
//! QueryClient.workflows()     → adapt_workflow()  → WorkflowRow
//! ```

use super::ftui_compat::{ColorSpec, StyleSpec};
use super::query::{
    EventView, HealthStatus, HistoryEntryView, PaneView, SearchResultView, TriageItemView,
    WorkflowProgressView,
};
use crate::circuit_breaker::CircuitStateKind;
use crate::storage::{CorrelationRef, TimelineEvent};

// ---------------------------------------------------------------------------
// Pane adapter
// ---------------------------------------------------------------------------

/// Render-ready pane row for the Panes view.
#[derive(Debug, Clone)]
pub struct PaneRow {
    pub pane_id: String,
    pub title: String,
    pub domain: String,
    pub cwd: String,
    pub agent_label: String,
    pub state_label: String,
    pub unhandled_badge: String,
    pub state_style: StyleSpec,
    pub agent_style: StyleSpec,
    pub unhandled_style: StyleSpec,
}

/// Adapt a PaneView from QueryClient into a render-ready PaneRow.
#[must_use]
pub fn adapt_pane(pane: &PaneView) -> PaneRow {
    let agent_label = pane.agent_type.as_deref().unwrap_or("unknown").to_string();

    let agent_style = match pane.agent_type.as_deref() {
        Some("codex") => StyleSpec::new().fg(ColorSpec::Green),
        Some("claude") => StyleSpec::new().fg(ColorSpec::Magenta),
        Some("gemini") => StyleSpec::new().fg(ColorSpec::Blue),
        _ => StyleSpec::new().fg(ColorSpec::DarkGray),
    };

    let state_label = pane.pane_state.clone();
    let state_style = match pane.pane_state.as_str() {
        "AltScreen" => StyleSpec::new().fg(ColorSpec::Yellow),
        "CommandRunning" => StyleSpec::new().fg(ColorSpec::Cyan),
        "PromptActive" => StyleSpec::new().fg(ColorSpec::Green),
        _ => StyleSpec::new().fg(ColorSpec::DarkGray),
    };

    let unhandled_badge = if pane.unhandled_event_count > 0 {
        format!("{}", pane.unhandled_event_count)
    } else {
        String::new()
    };

    let unhandled_style = if pane.unhandled_event_count > 0 {
        StyleSpec::new().fg(ColorSpec::Red).bold()
    } else {
        StyleSpec::new()
    };

    PaneRow {
        pane_id: pane.pane_id.to_string(),
        title: truncate(&pane.title, 40),
        domain: pane.domain.clone(),
        cwd: pane.cwd.clone().unwrap_or_default(),
        agent_label,
        state_label,
        unhandled_badge,
        state_style,
        agent_style,
        unhandled_style,
    }
}

// ---------------------------------------------------------------------------
// Event adapter
// ---------------------------------------------------------------------------

/// Render-ready event row for the Events view.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub id: String,
    pub rule_id: String,
    pub pane_id: String,
    pub severity_label: String,
    pub message: String,
    pub timestamp: String,
    pub handled_label: String,
    pub triage_label: String,
    pub labels_label: String,
    pub note_preview: String,
    pub severity_style: StyleSpec,
    pub handled_style: StyleSpec,
    pub triage_style: StyleSpec,
}

/// Adapt an EventView from QueryClient into a render-ready EventRow.
#[must_use]
pub fn adapt_event(event: &EventView) -> EventRow {
    let severity_style = severity_to_style(&event.severity);
    let handled_label = if event.handled {
        "handled".to_string()
    } else {
        "UNHANDLED".to_string()
    };
    let handled_style = if event.handled {
        StyleSpec::new().fg(ColorSpec::DarkGray)
    } else {
        StyleSpec::new().fg(ColorSpec::Yellow).bold()
    };

    let triage_label = event.triage_state.as_deref().unwrap_or("").to_string();
    let triage_style = match event.triage_state.as_deref() {
        Some("escalated") => StyleSpec::new().fg(ColorSpec::Red).bold(),
        Some("deferred") => StyleSpec::new().fg(ColorSpec::Yellow),
        Some("acknowledged") => StyleSpec::new().fg(ColorSpec::Green),
        Some(_) => StyleSpec::new().fg(ColorSpec::DarkGray),
        None => StyleSpec::new(),
    };

    let labels_label = if event.labels.is_empty() {
        String::new()
    } else {
        event.labels.join(", ")
    };

    let note_preview = event
        .note
        .as_deref()
        .map(|n| truncate(n, 40))
        .unwrap_or_default();

    EventRow {
        id: event.id.to_string(),
        rule_id: event.rule_id.clone(),
        pane_id: event.pane_id.to_string(),
        severity_label: event.severity.clone(),
        message: redact_secrets(&truncate(&event.message, 80)),
        timestamp: format_epoch_ms(event.timestamp),
        handled_label,
        triage_label,
        labels_label,
        note_preview,
        severity_style,
        handled_style,
        triage_style,
    }
}

// ---------------------------------------------------------------------------
// Triage adapter
// ---------------------------------------------------------------------------

/// Render-ready triage row for the Triage view.
#[derive(Debug, Clone)]
pub struct TriageRow {
    pub section: String,
    pub severity_label: String,
    pub title: String,
    pub detail: String,
    pub action_labels: Vec<String>,
    pub action_commands: Vec<String>,
    pub severity_style: StyleSpec,
    // Cross-reference IDs for provenance tracing.
    pub event_id: String,
    pub pane_id: String,
    pub workflow_id: String,
}

/// Adapt a TriageItemView from QueryClient into a render-ready TriageRow.
#[must_use]
pub fn adapt_triage(item: &TriageItemView) -> TriageRow {
    let severity_style = severity_to_style(&item.severity);

    TriageRow {
        section: item.section.clone(),
        severity_label: item.severity.clone(),
        title: truncate(&item.title, 60),
        detail: redact_secrets(&truncate(&item.detail, 120)),
        action_labels: item.actions.iter().map(|a| a.label.clone()).collect(),
        action_commands: item.actions.iter().map(|a| a.command.clone()).collect(),
        severity_style,
        event_id: item.event_id.map(|id| id.to_string()).unwrap_or_default(),
        pane_id: item.pane_id.map(|id| id.to_string()).unwrap_or_default(),
        workflow_id: item.workflow_id.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// History adapter
// ---------------------------------------------------------------------------

/// Render-ready history row for the History view.
#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub audit_id: String,
    pub timestamp: String,
    pub action_kind: String,
    pub result_label: String,
    pub actor_kind: String,
    pub summary: String,
    pub undo_label: String,
    pub result_style: StyleSpec,
    pub undo_style: StyleSpec,
    // Provenance fields for cross-referencing and operator interpretation.
    pub pane_id: String,
    pub workflow_id: String,
    pub step_name: String,
    pub rule_id: String,
    pub undo_strategy: String,
    pub undo_hint: String,
}

/// Adapt a HistoryEntryView from QueryClient into a render-ready HistoryRow.
#[must_use]
pub fn adapt_history(entry: &HistoryEntryView) -> HistoryRow {
    let result_style = match entry.result.as_str() {
        "success" => StyleSpec::new().fg(ColorSpec::Green),
        "denied" => StyleSpec::new().fg(ColorSpec::Yellow),
        "failed" => StyleSpec::new().fg(ColorSpec::Red),
        _ => StyleSpec::new().fg(ColorSpec::DarkGray),
    };

    let undo_label = if entry.undone {
        "UNDONE".to_string()
    } else if entry.undoable {
        "undoable".to_string()
    } else {
        String::new()
    };

    let undo_style = if entry.undone {
        StyleSpec::new().fg(ColorSpec::DarkGray).dim()
    } else if entry.undoable {
        StyleSpec::new().fg(ColorSpec::Cyan)
    } else {
        StyleSpec::new()
    };

    HistoryRow {
        audit_id: entry.audit_id.to_string(),
        timestamp: format_epoch_ms(entry.timestamp),
        action_kind: entry.action_kind.clone(),
        result_label: entry.result.clone(),
        actor_kind: entry.actor_kind.clone(),
        summary: redact_secrets(&truncate(&entry.summary, 60)),
        undo_label,
        result_style,
        undo_style,
        pane_id: entry.pane_id.map(|id| id.to_string()).unwrap_or_default(),
        workflow_id: entry.workflow_id.clone().unwrap_or_default(),
        step_name: entry.step_name.clone().unwrap_or_default(),
        rule_id: entry.rule_id.clone().unwrap_or_default(),
        undo_strategy: entry.undo_strategy.clone().unwrap_or_default(),
        undo_hint: entry.undo_hint.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Search adapter
// ---------------------------------------------------------------------------

/// Render-ready search result row for the Search view.
#[derive(Debug, Clone)]
pub struct SearchRow {
    pub pane_id: String,
    pub timestamp: String,
    pub snippet: String,
    pub rank_label: String,
}

/// Adapt a SearchResultView from QueryClient into a render-ready SearchRow.
#[must_use]
pub fn adapt_search(result: &SearchResultView) -> SearchRow {
    SearchRow {
        pane_id: result.pane_id.to_string(),
        timestamp: format_epoch_ms(result.timestamp),
        snippet: redact_secrets(&result.snippet),
        rank_label: format!("{:.2}", result.rank),
    }
}

// ---------------------------------------------------------------------------
// Workflow adapter
// ---------------------------------------------------------------------------

/// Render-ready workflow progress row.
#[derive(Debug, Clone)]
pub struct WorkflowRow {
    pub id: String,
    pub name: String,
    pub pane_id: String,
    pub progress_label: String,
    pub status_label: String,
    pub error: Option<String>,
    pub status_style: StyleSpec,
    pub started_at: String,
    pub updated_at: String,
}

/// Adapt a WorkflowProgressView from QueryClient into a render-ready WorkflowRow.
#[must_use]
pub fn adapt_workflow(wf: &WorkflowProgressView) -> WorkflowRow {
    let status_style = match wf.status.as_str() {
        "running" | "pending" => StyleSpec::new().fg(ColorSpec::Cyan),
        "completed" => StyleSpec::new().fg(ColorSpec::Green),
        "failed" | "error" => StyleSpec::new().fg(ColorSpec::Red),
        _ => StyleSpec::new().fg(ColorSpec::DarkGray),
    };

    WorkflowRow {
        id: wf.id.clone(),
        name: wf.workflow_name.clone(),
        pane_id: wf.pane_id.to_string(),
        progress_label: format!("{}/{}", wf.current_step, wf.total_steps),
        status_label: wf.status.clone(),
        error: wf.error.clone(),
        status_style,
        started_at: format_epoch_ms(wf.started_at),
        updated_at: format_epoch_ms(wf.updated_at),
    }
}

// ---------------------------------------------------------------------------
// Timeline adapter
// ---------------------------------------------------------------------------

/// Render-ready timeline event row for the Timeline view.
#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub id: String,
    pub timestamp: String,
    pub pane_label: String,
    pub agent_label: String,
    pub event_type: String,
    pub severity_label: String,
    pub handled_label: String,
    pub correlation_label: String,
    pub summary: String,
    pub severity_style: StyleSpec,
    pub agent_style: StyleSpec,
    pub handled_style: StyleSpec,
    pub correlation_style: StyleSpec,
}

/// Adapt a TimelineEvent from storage into a render-ready TimelineRow.
#[must_use]
pub fn adapt_timeline_event(event: &TimelineEvent) -> TimelineRow {
    let severity_style = severity_to_style(&event.severity);
    let agent_label = event
        .pane_info
        .agent_type
        .as_deref()
        .unwrap_or("unknown")
        .to_string();

    let agent_style = match event.pane_info.agent_type.as_deref() {
        Some("codex") => StyleSpec::new().fg(ColorSpec::Green),
        Some("claude") => StyleSpec::new().fg(ColorSpec::Magenta),
        Some("gemini") => StyleSpec::new().fg(ColorSpec::Blue),
        _ => StyleSpec::new().fg(ColorSpec::DarkGray),
    };

    let handled_label = if event.handled.is_some() {
        "handled".to_string()
    } else {
        "OPEN".to_string()
    };
    let handled_style = if event.handled.is_some() {
        StyleSpec::new().fg(ColorSpec::DarkGray)
    } else {
        StyleSpec::new().fg(ColorSpec::Yellow).bold()
    };

    let correlation_label = format_correlations(&event.correlations);
    let correlation_style = if event.correlations.is_empty() {
        StyleSpec::new()
    } else {
        StyleSpec::new().fg(ColorSpec::Cyan)
    };

    let summary = event
        .summary
        .as_deref()
        .map(|s| truncate(s, 60))
        .unwrap_or_default();

    let pane_label = format!("P{}", event.pane_info.pane_id);

    TimelineRow {
        id: event.id.to_string(),
        timestamp: format_epoch_ms(event.timestamp),
        pane_label,
        agent_label,
        event_type: event.event_type.clone(),
        severity_label: event.severity.clone(),
        handled_label,
        correlation_label,
        summary,
        severity_style,
        agent_style,
        handled_style,
        correlation_style,
    }
}

/// Format correlation references into a compact label.
fn format_correlations(correlations: &[CorrelationRef]) -> String {
    if correlations.is_empty() {
        return String::new();
    }
    correlations
        .iter()
        .map(|c| c.correlation_type.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Health adapter
// ---------------------------------------------------------------------------

/// Render-ready health model for the Home view.
#[derive(Debug, Clone)]
pub struct HealthModel {
    pub watcher_label: String,
    pub watcher_style: StyleSpec,
    pub db_label: String,
    pub db_style: StyleSpec,
    pub wezterm_label: String,
    pub wezterm_style: StyleSpec,
    pub circuit_label: String,
    pub circuit_style: StyleSpec,
    pub pane_count: String,
    pub event_count: String,
}

/// Adapt a HealthStatus from QueryClient into a render-ready HealthModel.
#[must_use]
pub fn adapt_health(health: &HealthStatus) -> HealthModel {
    let (watcher_label, watcher_style) = if health.watcher_running {
        ("running".to_string(), StyleSpec::new().fg(ColorSpec::Green))
    } else {
        (
            "stopped".to_string(),
            StyleSpec::new().fg(ColorSpec::Red).bold(),
        )
    };

    let (db_label, db_style) = if health.db_accessible {
        ("ok".to_string(), StyleSpec::new().fg(ColorSpec::Green))
    } else {
        (
            "unavailable".to_string(),
            StyleSpec::new().fg(ColorSpec::Red),
        )
    };

    let (wezterm_label, wezterm_style) = if health.wezterm_accessible {
        ("ok".to_string(), StyleSpec::new().fg(ColorSpec::Green))
    } else {
        (
            "unavailable".to_string(),
            StyleSpec::new().fg(ColorSpec::Red),
        )
    };

    let (circuit_label, circuit_style) = match health.wezterm_circuit.state {
        CircuitStateKind::Closed => ("closed".to_string(), StyleSpec::new().fg(ColorSpec::Green)),
        CircuitStateKind::Open => (
            "OPEN".to_string(),
            StyleSpec::new().fg(ColorSpec::Red).bold(),
        ),
        CircuitStateKind::HalfOpen => (
            "half-open".to_string(),
            StyleSpec::new().fg(ColorSpec::Yellow),
        ),
    };

    HealthModel {
        watcher_label,
        watcher_style,
        db_label,
        db_style,
        wezterm_label,
        wezterm_style,
        circuit_label,
        circuit_style,
        pane_count: health.pane_count.to_string(),
        event_count: health.event_count.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Map severity string to a color style.
fn severity_to_style(severity: &str) -> StyleSpec {
    match severity {
        "error" => StyleSpec::new().fg(ColorSpec::Red).bold(),
        "warning" => StyleSpec::new().fg(ColorSpec::Yellow),
        "info" => StyleSpec::new().fg(ColorSpec::Cyan),
        _ => StyleSpec::new().fg(ColorSpec::DarkGray),
    }
}

/// Truncate a string to `max_len` characters, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

/// Redact secret-like patterns from display strings.
///
/// Masks patterns that commonly appear as secrets in terminal output:
/// - API keys / tokens (Bearer, sk-, key-, ghp_, AKIA, etc.)
/// - Password-like assignments (`password=...`, `secret=...`, `token=...`)
/// - AWS-style access keys
///
/// This is a best-effort defense-in-depth measure. The primary redaction
/// boundary is at the storage/capture layer; this provides a second pass
/// before values reach the render surface.
fn redact_secrets(s: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
        // Order: longer patterns first to avoid partial matches.
        Regex::new(concat!(
            r"(?i)",
            r"(?:",
            // Bearer token: "Bearer <token>"
            r"Bearer\s+\S{8,}",
            r"|",
            // Common key prefixes followed by 8+ non-whitespace chars
            r"(?:sk-|sk_live_|sk_test_|ghp_|gho_|github_pat_|AKIA)[A-Za-z0-9_\-]{8,}",
            r"|",
            // password=/secret=/token=/api_key= assignments (value after =)
            r"(?:password|secret|token|api_key|apikey|access_key|private_key)\s*=\s*\S{4,}",
            r")",
        ))
        .expect("redact_secrets regex is valid")
    });

    SECRET_RE.replace_all(s, "[REDACTED]").into_owned()
}

/// Format epoch milliseconds as a human-readable relative or absolute timestamp.
///
/// Returns relative format for recent times (e.g., "2m ago"), absolute for older.
/// This is deterministic for a given input (no clock dependency in the format itself;
/// the "ago" suffix is purely presentation).
fn format_epoch_ms(ts: i64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    // Format as ISO-like compact: YYYY-MM-DD HH:MM
    let secs = ts / 1000;
    let dt = chrono::DateTime::from_timestamp(secs, 0);
    match dt {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => ts.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pane() -> PaneView {
        PaneView {
            pane_id: 42,
            title: "Claude Code - /data/projects/foo".to_string(),
            domain: "local".to_string(),
            cwd: Some("/data/projects/foo".to_string()),
            is_excluded: false,
            agent_type: Some("claude".to_string()),
            pane_state: "PromptActive".to_string(),
            last_activity_ts: Some(1_700_000_000_000),
            unhandled_event_count: 3,
        }
    }

    fn sample_event() -> EventView {
        EventView {
            id: 100,
            rule_id: "rate_limit_detected".to_string(),
            pane_id: 42,
            severity: "warning".to_string(),
            message: "Rate limit exceeded for API calls".to_string(),
            timestamp: 1_700_000_000_000,
            handled: false,
            triage_state: Some("escalated".to_string()),
            labels: vec!["api".to_string(), "rate-limit".to_string()],
            note: Some("Investigate throttling config".to_string()),
        }
    }

    fn sample_history() -> HistoryEntryView {
        HistoryEntryView {
            audit_id: 1,
            timestamp: 1_700_000_000_000,
            pane_id: Some(42),
            workflow_id: None,
            action_kind: "send_text".to_string(),
            result: "success".to_string(),
            actor_kind: "robot".to_string(),
            step_name: None,
            undoable: true,
            undone: false,
            undo_strategy: Some("workflow_abort".to_string()),
            undo_hint: None,
            rule_id: None,
            summary: "Sent retry command to pane 42".to_string(),
        }
    }

    fn sample_search() -> SearchResultView {
        SearchResultView {
            pane_id: 42,
            timestamp: 1_700_000_000_000,
            snippet: ">>error<< in authentication module".to_string(),
            rank: 3.15,
        }
    }

    fn sample_triage() -> TriageItemView {
        TriageItemView {
            section: "events".to_string(),
            severity: "error".to_string(),
            title: "[pane 42] rate_limit: api_error".to_string(),
            detail: "API returned 429 Too Many Requests".to_string(),
            actions: vec![super::super::query::TriageAction {
                label: "Explain".to_string(),
                command: "wa why --recent --pane 42".to_string(),
            }],
            event_id: Some(100),
            pane_id: Some(42),
            workflow_id: None,
        }
    }

    fn sample_workflow() -> WorkflowProgressView {
        WorkflowProgressView {
            id: "wf-001".to_string(),
            workflow_name: "rate_limit_backoff".to_string(),
            pane_id: 42,
            current_step: 2,
            total_steps: 5,
            status: "running".to_string(),
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_060_000,
        }
    }

    #[test]
    fn adapt_pane_formats_correctly() {
        let row = adapt_pane(&sample_pane());
        assert_eq!(row.pane_id, "42");
        assert_eq!(row.agent_label, "claude");
        assert_eq!(row.state_label, "PromptActive");
        assert_eq!(row.unhandled_badge, "3");
        assert!(row.unhandled_style.bold);
        assert_eq!(row.unhandled_style.fg, Some(ColorSpec::Red));
    }

    #[test]
    fn adapt_pane_empty_unhandled() {
        let mut pane = sample_pane();
        pane.unhandled_event_count = 0;
        let row = adapt_pane(&pane);
        assert_eq!(row.unhandled_badge, "");
    }

    #[test]
    fn adapt_pane_unknown_agent() {
        let mut pane = sample_pane();
        pane.agent_type = None;
        let row = adapt_pane(&pane);
        assert_eq!(row.agent_label, "unknown");
        assert_eq!(row.agent_style.fg, Some(ColorSpec::DarkGray));
    }

    #[test]
    fn adapt_event_unhandled() {
        let row = adapt_event(&sample_event());
        assert_eq!(row.id, "100");
        assert_eq!(row.severity_label, "warning");
        assert_eq!(row.handled_label, "UNHANDLED");
        assert!(row.handled_style.bold);
    }

    #[test]
    fn adapt_event_handled() {
        let mut event = sample_event();
        event.handled = true;
        let row = adapt_event(&event);
        assert_eq!(row.handled_label, "handled");
        assert!(!row.handled_style.bold);
    }

    #[test]
    fn adapt_event_triage_state_escalated() {
        let row = adapt_event(&sample_event());
        assert_eq!(row.triage_label, "escalated");
        assert_eq!(row.triage_style.fg, Some(ColorSpec::Red));
        assert!(row.triage_style.bold);
    }

    #[test]
    fn adapt_event_triage_state_acknowledged() {
        let mut event = sample_event();
        event.triage_state = Some("acknowledged".to_string());
        let row = adapt_event(&event);
        assert_eq!(row.triage_label, "acknowledged");
        assert_eq!(row.triage_style.fg, Some(ColorSpec::Green));
    }

    #[test]
    fn adapt_event_triage_state_none() {
        let mut event = sample_event();
        event.triage_state = None;
        let row = adapt_event(&event);
        assert_eq!(row.triage_label, "");
        assert_eq!(row.triage_style.fg, None);
    }

    #[test]
    fn adapt_event_labels_mapped() {
        let row = adapt_event(&sample_event());
        assert_eq!(row.labels_label, "api, rate-limit");
    }

    #[test]
    fn adapt_event_empty_labels() {
        let mut event = sample_event();
        event.labels = vec![];
        let row = adapt_event(&event);
        assert_eq!(row.labels_label, "");
    }

    #[test]
    fn adapt_event_note_preview() {
        let row = adapt_event(&sample_event());
        assert_eq!(row.note_preview, "Investigate throttling config");
    }

    #[test]
    fn adapt_event_no_note() {
        let mut event = sample_event();
        event.note = None;
        let row = adapt_event(&event);
        assert_eq!(row.note_preview, "");
    }

    #[test]
    fn adapt_history_undoable() {
        let row = adapt_history(&sample_history());
        assert_eq!(row.audit_id, "1");
        assert_eq!(row.result_label, "success");
        assert_eq!(row.undo_label, "undoable");
        assert_eq!(row.undo_style.fg, Some(ColorSpec::Cyan));
    }

    #[test]
    fn adapt_history_undone() {
        let mut entry = sample_history();
        entry.undone = true;
        let row = adapt_history(&entry);
        assert_eq!(row.undo_label, "UNDONE");
        assert!(row.undo_style.dim);
    }

    #[test]
    fn adapt_history_no_undo() {
        let mut entry = sample_history();
        entry.undoable = false;
        let row = adapt_history(&entry);
        assert_eq!(row.undo_label, "");
    }

    #[test]
    fn adapt_history_provenance_fields() {
        let row = adapt_history(&sample_history());
        assert_eq!(row.pane_id, "42");
        assert_eq!(row.workflow_id, "");
        assert_eq!(row.step_name, "");
        assert_eq!(row.rule_id, "");
        assert_eq!(row.undo_strategy, "workflow_abort");
        assert_eq!(row.undo_hint, "");
    }

    #[test]
    fn adapt_history_full_provenance() {
        let mut entry = sample_history();
        entry.workflow_id = Some("wf-010".to_string());
        entry.step_name = Some("verify_rollback".to_string());
        entry.rule_id = Some("rate_limit_detected".to_string());
        entry.undo_hint = Some("Abort workflow wf-010".to_string());
        let row = adapt_history(&entry);
        assert_eq!(row.pane_id, "42");
        assert_eq!(row.workflow_id, "wf-010");
        assert_eq!(row.step_name, "verify_rollback");
        assert_eq!(row.rule_id, "rate_limit_detected");
        assert_eq!(row.undo_strategy, "workflow_abort");
        assert_eq!(row.undo_hint, "Abort workflow wf-010");
    }

    #[test]
    fn adapt_history_no_pane() {
        let mut entry = sample_history();
        entry.pane_id = None;
        let row = adapt_history(&entry);
        assert_eq!(row.pane_id, "");
    }

    #[test]
    fn adapt_search_formats_rank() {
        let row = adapt_search(&sample_search());
        assert_eq!(row.pane_id, "42");
        assert_eq!(row.rank_label, "3.14");
        assert!(row.snippet.contains(">>error<<"));
    }

    #[test]
    fn adapt_triage_formats_actions() {
        let row = adapt_triage(&sample_triage());
        assert_eq!(row.severity_label, "error");
        assert!(row.severity_style.bold);
        assert_eq!(row.action_labels.len(), 1);
        assert_eq!(row.action_labels[0], "Explain");
        assert_eq!(row.action_commands[0], "wa why --recent --pane 42");
    }

    #[test]
    fn adapt_triage_cross_references() {
        let row = adapt_triage(&sample_triage());
        assert_eq!(row.event_id, "100");
        assert_eq!(row.pane_id, "42");
        assert_eq!(row.workflow_id, "");
    }

    #[test]
    fn adapt_triage_workflow_cross_reference() {
        let mut item = sample_triage();
        item.event_id = None;
        item.pane_id = Some(5);
        item.workflow_id = Some("wf-007".to_string());
        let row = adapt_triage(&item);
        assert_eq!(row.event_id, "");
        assert_eq!(row.pane_id, "5");
        assert_eq!(row.workflow_id, "wf-007");
    }

    #[test]
    fn adapt_workflow_running() {
        let row = adapt_workflow(&sample_workflow());
        assert_eq!(row.progress_label, "2/5");
        assert_eq!(row.status_label, "running");
        assert_eq!(row.status_style.fg, Some(ColorSpec::Cyan));
        assert!(row.error.is_none());
    }

    #[test]
    fn adapt_workflow_failed() {
        let mut wf = sample_workflow();
        wf.status = "failed".to_string();
        wf.error = Some("connection timeout".to_string());
        let row = adapt_workflow(&wf);
        assert_eq!(row.status_style.fg, Some(ColorSpec::Red));
        assert_eq!(row.error.as_deref(), Some("connection timeout"));
    }

    #[test]
    fn adapt_workflow_timing_fields() {
        let row = adapt_workflow(&sample_workflow());
        assert!(row.started_at.contains("2023"));
        assert!(row.updated_at.contains("2023"));
        // updated_at should differ from started_at (60s apart in sample)
        assert_ne!(row.started_at, row.updated_at);
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world!", 8), "hello...");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn severity_styles_are_distinct() {
        let error_style = severity_to_style("error");
        let warning_style = severity_to_style("warning");
        let info_style = severity_to_style("info");
        assert_ne!(error_style.fg, warning_style.fg);
        assert_ne!(warning_style.fg, info_style.fg);
        assert!(error_style.bold);
        assert!(!warning_style.bold);
    }

    #[test]
    fn format_epoch_ms_zero() {
        assert_eq!(format_epoch_ms(0), "-");
    }

    #[test]
    fn format_epoch_ms_valid() {
        let formatted = format_epoch_ms(1_700_000_000_000);
        assert!(formatted.contains("2023"));
        assert!(formatted.contains("-"));
    }

    #[test]
    fn adapt_health_all_healthy() {
        let health = HealthStatus {
            watcher_running: true,
            db_accessible: true,
            wezterm_accessible: true,
            wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
            pane_count: 5,
            event_count: 42,
            last_capture_ts: Some(1_700_000_000_000),
        };
        let model = adapt_health(&health);
        assert_eq!(model.watcher_label, "running");
        assert_eq!(model.watcher_style.fg, Some(ColorSpec::Green));
        assert_eq!(model.pane_count, "5");
        assert_eq!(model.event_count, "42");
    }

    #[test]
    fn adapt_health_degraded() {
        let health = HealthStatus {
            watcher_running: false,
            db_accessible: false,
            wezterm_accessible: false,
            wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
            pane_count: 0,
            event_count: 0,
            last_capture_ts: None,
        };
        let model = adapt_health(&health);
        assert_eq!(model.watcher_label, "stopped");
        assert!(model.watcher_style.bold);
        assert_eq!(model.db_label, "unavailable");
    }

    // ----- Redaction tests -----

    #[test]
    fn redact_bearer_token() {
        let input = "Auth: Bearer sk-abc123456789def";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123456789def"));
    }

    #[test]
    fn redact_github_pat() {
        let input = "Using token ghp_aBcDeFgHiJkLmNoPqRsT";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ghp_aBcDeFgHiJkLmNoPqRsT"));
    }

    #[test]
    fn redact_password_assignment() {
        let input = "Config: password=MyS3cretP@ss";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("MyS3cretP@ss"));
    }

    #[test]
    fn redact_api_key_assignment() {
        let input = "export api_key=abcd1234efgh5678";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("abcd1234efgh5678"));
    }

    #[test]
    fn redact_aws_access_key() {
        let input = "AWS key: AKIAIOSFODNN7EXAMPLE";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redact_preserves_normal_text() {
        let input = "Rate limit exceeded for API calls on pane 42";
        let result = redact_secrets(input);
        assert_eq!(result, input);
    }

    #[test]
    fn redact_event_message_integration() {
        let mut event = sample_event();
        event.message = "Error: Bearer sk-live_abcdef123456 expired".to_string();
        let row = adapt_event(&event);
        assert!(row.message.contains("[REDACTED]"));
        assert!(!row.message.contains("sk-live_abcdef123456"));
    }

    #[test]
    fn redact_search_snippet_integration() {
        let mut result = sample_search();
        result.snippet = "Found token=xyzSecret12345 in output".to_string();
        let row = adapt_search(&result);
        assert!(row.snippet.contains("[REDACTED]"));
        assert!(!row.snippet.contains("xyzSecret12345"));
    }

    #[test]
    fn redact_history_summary_integration() {
        let mut entry = sample_history();
        entry.summary = "Sent password=hunter2 to pane".to_string();
        let row = adapt_history(&entry);
        assert!(row.summary.contains("[REDACTED]"));
        assert!(!row.summary.contains("hunter2"));
    }

    #[test]
    fn redact_triage_detail_integration() {
        let mut item = sample_triage();
        item.detail = "Leaked secret=topSecretValue99 in logs".to_string();
        let row = adapt_triage(&item);
        assert!(row.detail.contains("[REDACTED]"));
        assert!(!row.detail.contains("topSecretValue99"));
    }

    // =====================================================================
    // Adapter Fixture Pack (wa-2i6m / FTUI-04.2.a)
    //
    // Systematic corpus covering normal, missing, redacted, and malformed
    // input variants for all adapter domains. Each test validates every
    // output field for deterministic, actionable diagnostics on failure.
    // =====================================================================

    /// Macro for field-level adapter validation with actionable diffs.
    ///
    /// On failure, reports: domain, fixture variant, field name, expected, actual.
    macro_rules! assert_field {
        ($domain:expr, $variant:expr, $field:expr, $actual:expr, $expected:expr) => {
            assert_eq!(
                $actual, $expected,
                "\n  Adapter fixture mismatch!\n  domain:   {}\n  variant:  {}\n  field:    {}\n  expected: {:?}\n  actual:   {:?}",
                $domain, $variant, $field, $expected, $actual,
            );
        };
    }

    // --- Pane fixtures ---

    /// Missing optional fields: no cwd, no agent, zero unhandled.
    fn pane_missing() -> PaneView {
        PaneView {
            pane_id: 0,
            title: String::new(),
            domain: String::new(),
            cwd: None,
            is_excluded: false,
            agent_type: None,
            pane_state: String::new(),
            last_activity_ts: None,
            unhandled_event_count: 0,
        }
    }

    /// Malformed: overlong title, unknown state, max pane_id, unicode domain.
    fn pane_malformed() -> PaneView {
        PaneView {
            pane_id: u64::MAX,
            title: "x".repeat(200),
            domain: "\u{1F4BB} remote\u{0000}".to_string(),
            cwd: Some("/a/".repeat(100)),
            is_excluded: true,
            agent_type: Some("unknown_agent_v99".to_string()),
            pane_state: "BogusState".to_string(),
            last_activity_ts: Some(-1),
            unhandled_event_count: u32::MAX,
        }
    }

    #[test]
    fn fixture_pane_normal_all_fields() {
        let row = adapt_pane(&sample_pane());
        assert_field!("pane", "normal", "pane_id", row.pane_id, "42");
        assert_field!(
            "pane",
            "normal",
            "title",
            row.title,
            "Claude Code - /data/projects/foo"
        );
        assert_field!("pane", "normal", "domain", row.domain, "local");
        assert_field!("pane", "normal", "cwd", row.cwd, "/data/projects/foo");
        assert_field!("pane", "normal", "agent_label", row.agent_label, "claude");
        assert_field!(
            "pane",
            "normal",
            "state_label",
            row.state_label,
            "PromptActive"
        );
        assert_field!(
            "pane",
            "normal",
            "unhandled_badge",
            row.unhandled_badge,
            "3"
        );
        assert_field!(
            "pane",
            "normal",
            "state_style.fg",
            row.state_style.fg,
            Some(ColorSpec::Green)
        );
        assert_field!(
            "pane",
            "normal",
            "agent_style.fg",
            row.agent_style.fg,
            Some(ColorSpec::Magenta)
        );
        assert_field!(
            "pane",
            "normal",
            "unhandled_style.fg",
            row.unhandled_style.fg,
            Some(ColorSpec::Red)
        );
        assert_field!(
            "pane",
            "normal",
            "unhandled_style.bold",
            row.unhandled_style.bold,
            true
        );
    }

    #[test]
    fn fixture_pane_missing_all_fields() {
        let row = adapt_pane(&pane_missing());
        assert_field!("pane", "missing", "pane_id", row.pane_id, "0");
        assert_field!("pane", "missing", "title", row.title, "");
        assert_field!("pane", "missing", "domain", row.domain, "");
        assert_field!("pane", "missing", "cwd", row.cwd, "");
        assert_field!("pane", "missing", "agent_label", row.agent_label, "unknown");
        assert_field!("pane", "missing", "state_label", row.state_label, "");
        assert_field!(
            "pane",
            "missing",
            "unhandled_badge",
            row.unhandled_badge,
            ""
        );
        assert_field!(
            "pane",
            "missing",
            "agent_style.fg",
            row.agent_style.fg,
            Some(ColorSpec::DarkGray)
        );
        assert_field!(
            "pane",
            "missing",
            "state_style.fg",
            row.state_style.fg,
            Some(ColorSpec::DarkGray)
        );
        assert_field!(
            "pane",
            "missing",
            "unhandled_style.bold",
            row.unhandled_style.bold,
            false
        );
    }

    #[test]
    fn fixture_pane_malformed_all_fields() {
        let row = adapt_pane(&pane_malformed());
        assert_field!(
            "pane",
            "malformed",
            "pane_id",
            row.pane_id,
            u64::MAX.to_string()
        );
        // Title is truncated to 40 chars
        assert!(
            row.title.len() <= 40,
            "title should be truncated to 40, got {}",
            row.title.len()
        );
        assert!(
            row.title.ends_with("..."),
            "truncated title should end with ..."
        );
        // Unknown agent type → label is the raw string, style is DarkGray
        assert_field!(
            "pane",
            "malformed",
            "agent_label",
            row.agent_label,
            "unknown_agent_v99"
        );
        assert_field!(
            "pane",
            "malformed",
            "agent_style.fg",
            row.agent_style.fg,
            Some(ColorSpec::DarkGray)
        );
        // Unknown state → DarkGray
        assert_field!(
            "pane",
            "malformed",
            "state_label",
            row.state_label,
            "BogusState"
        );
        assert_field!(
            "pane",
            "malformed",
            "state_style.fg",
            row.state_style.fg,
            Some(ColorSpec::DarkGray)
        );
        // Max unhandled count → formatted as string
        assert_field!(
            "pane",
            "malformed",
            "unhandled_badge",
            row.unhandled_badge,
            u32::MAX.to_string()
        );
    }

    #[test]
    fn fixture_pane_agent_variants() {
        for (agent, expected_fg) in [
            (Some("codex"), Some(ColorSpec::Green)),
            (Some("claude"), Some(ColorSpec::Magenta)),
            (Some("gemini"), Some(ColorSpec::Blue)),
            (Some("grok"), Some(ColorSpec::DarkGray)),
            (None, Some(ColorSpec::DarkGray)),
        ] {
            let mut pane = sample_pane();
            pane.agent_type = agent.map(String::from);
            let row = adapt_pane(&pane);
            assert_field!(
                "pane",
                format!("agent={:?}", agent),
                "agent_style.fg",
                row.agent_style.fg,
                expected_fg
            );
        }
    }

    #[test]
    fn fixture_pane_state_variants() {
        for (state, expected_fg) in [
            ("AltScreen", Some(ColorSpec::Yellow)),
            ("CommandRunning", Some(ColorSpec::Cyan)),
            ("PromptActive", Some(ColorSpec::Green)),
            ("SomethingElse", Some(ColorSpec::DarkGray)),
            ("", Some(ColorSpec::DarkGray)),
        ] {
            let mut pane = sample_pane();
            pane.pane_state = state.to_string();
            let row = adapt_pane(&pane);
            assert_field!(
                "pane",
                format!("state={}", state),
                "state_style.fg",
                row.state_style.fg,
                expected_fg
            );
        }
    }

    // --- Event fixtures ---

    fn event_missing() -> EventView {
        EventView {
            id: 0,
            rule_id: String::new(),
            pane_id: 0,
            severity: String::new(),
            message: String::new(),
            timestamp: 0,
            handled: true,
            triage_state: None,
            labels: vec![],
            note: None,
        }
    }

    fn event_malformed() -> EventView {
        EventView {
            id: i64::MAX,
            rule_id: "x".repeat(200),
            pane_id: u64::MAX,
            severity: "BOGUS_SEVERITY".to_string(),
            message: "m".repeat(200),
            timestamp: -999,
            handled: false,
            triage_state: Some("unknown_triage_state".to_string()),
            labels: vec!["a".repeat(100), "\u{0000}null".to_string()],
            note: Some("n".repeat(200)),
        }
    }

    fn event_redacted() -> EventView {
        EventView {
            id: 50,
            rule_id: "secret_leak".to_string(),
            pane_id: 7,
            severity: "error".to_string(),
            message: "Leaked Bearer sk-live_abc123456789 in output".to_string(),
            timestamp: 1_700_000_000_000,
            handled: false,
            triage_state: Some("escalated".to_string()),
            labels: vec!["security".to_string()],
            note: Some("Contains password=hunter2 in details".to_string()),
        }
    }

    #[test]
    fn fixture_event_normal_all_fields() {
        let row = adapt_event(&sample_event());
        assert_field!("event", "normal", "id", row.id, "100");
        assert_field!(
            "event",
            "normal",
            "rule_id",
            row.rule_id,
            "rate_limit_detected"
        );
        assert_field!("event", "normal", "pane_id", row.pane_id, "42");
        assert_field!(
            "event",
            "normal",
            "severity_label",
            row.severity_label,
            "warning"
        );
        assert_field!(
            "event",
            "normal",
            "handled_label",
            row.handled_label,
            "UNHANDLED"
        );
        assert_field!(
            "event",
            "normal",
            "triage_label",
            row.triage_label,
            "escalated"
        );
        assert_field!(
            "event",
            "normal",
            "labels_label",
            row.labels_label,
            "api, rate-limit"
        );
        assert_field!(
            "event",
            "normal",
            "note_preview",
            row.note_preview,
            "Investigate throttling config"
        );
        assert_field!(
            "event",
            "normal",
            "severity_style.fg",
            row.severity_style.fg,
            Some(ColorSpec::Yellow)
        );
        assert_field!(
            "event",
            "normal",
            "handled_style.bold",
            row.handled_style.bold,
            true
        );
        assert_field!(
            "event",
            "normal",
            "triage_style.fg",
            row.triage_style.fg,
            Some(ColorSpec::Red)
        );
    }

    #[test]
    fn fixture_event_missing_all_fields() {
        let row = adapt_event(&event_missing());
        assert_field!("event", "missing", "id", row.id, "0");
        assert_field!("event", "missing", "rule_id", row.rule_id, "");
        assert_field!("event", "missing", "pane_id", row.pane_id, "0");
        assert_field!("event", "missing", "severity_label", row.severity_label, "");
        assert_field!("event", "missing", "message", row.message, "");
        assert_field!("event", "missing", "timestamp", row.timestamp, "-");
        assert_field!(
            "event",
            "missing",
            "handled_label",
            row.handled_label,
            "handled"
        );
        assert_field!("event", "missing", "triage_label", row.triage_label, "");
        assert_field!("event", "missing", "labels_label", row.labels_label, "");
        assert_field!("event", "missing", "note_preview", row.note_preview, "");
        assert_field!(
            "event",
            "missing",
            "severity_style.fg",
            row.severity_style.fg,
            Some(ColorSpec::DarkGray)
        );
        assert_field!(
            "event",
            "missing",
            "triage_style.fg",
            row.triage_style.fg,
            None::<ColorSpec>
        );
    }

    #[test]
    fn fixture_event_malformed_all_fields() {
        let row = adapt_event(&event_malformed());
        assert_field!("event", "malformed", "id", row.id, i64::MAX.to_string());
        assert_field!(
            "event",
            "malformed",
            "pane_id",
            row.pane_id,
            u64::MAX.to_string()
        );
        // Message truncated to 80 chars
        assert!(
            row.message.len() <= 80,
            "message should be truncated to 80, got {}",
            row.message.len()
        );
        // Negative timestamp → falls through to raw number display
        assert!(!row.timestamp.is_empty());
        // Unknown severity → DarkGray
        assert_field!(
            "event",
            "malformed",
            "severity_style.fg",
            row.severity_style.fg,
            Some(ColorSpec::DarkGray)
        );
        // Unknown triage state → DarkGray
        assert_field!(
            "event",
            "malformed",
            "triage_style.fg",
            row.triage_style.fg,
            Some(ColorSpec::DarkGray)
        );
        // Note truncated to 40 chars
        assert!(
            row.note_preview.len() <= 40,
            "note should be truncated to 40, got {}",
            row.note_preview.len()
        );
    }

    #[test]
    fn fixture_event_redacted_secrets_stripped() {
        let row = adapt_event(&event_redacted());
        assert!(
            row.message.contains("[REDACTED]"),
            "message should have redacted token"
        );
        assert!(
            !row.message.contains("sk-live_abc123456789"),
            "raw token must not appear"
        );
        // Note is truncated but would contain redacted if long enough
        // The event ID and non-secret fields are preserved
        assert_field!("event", "redacted", "id", row.id, "50");
        assert_field!("event", "redacted", "rule_id", row.rule_id, "secret_leak");
    }

    #[test]
    fn fixture_event_severity_variants() {
        for (severity, expected_fg, expected_bold) in [
            ("error", Some(ColorSpec::Red), true),
            ("warning", Some(ColorSpec::Yellow), false),
            ("info", Some(ColorSpec::Cyan), false),
            ("debug", Some(ColorSpec::DarkGray), false),
            ("", Some(ColorSpec::DarkGray), false),
        ] {
            let mut event = sample_event();
            event.severity = severity.to_string();
            let row = adapt_event(&event);
            assert_field!(
                "event",
                format!("severity={}", severity),
                "severity_style.fg",
                row.severity_style.fg,
                expected_fg
            );
            assert_field!(
                "event",
                format!("severity={}", severity),
                "severity_style.bold",
                row.severity_style.bold,
                expected_bold
            );
        }
    }

    #[test]
    fn fixture_event_triage_state_variants() {
        for (triage, expected_fg, expected_bold) in [
            (Some("escalated"), Some(ColorSpec::Red), true),
            (Some("deferred"), Some(ColorSpec::Yellow), false),
            (Some("acknowledged"), Some(ColorSpec::Green), false),
            (Some("custom"), Some(ColorSpec::DarkGray), false),
            (None, None, false),
        ] {
            let mut event = sample_event();
            event.triage_state = triage.map(String::from);
            let row = adapt_event(&event);
            assert_field!(
                "event",
                format!("triage={:?}", triage),
                "triage_style.fg",
                row.triage_style.fg,
                expected_fg
            );
            assert_field!(
                "event",
                format!("triage={:?}", triage),
                "triage_style.bold",
                row.triage_style.bold,
                expected_bold
            );
        }
    }

    // --- Triage fixtures ---

    fn triage_missing() -> TriageItemView {
        TriageItemView {
            section: String::new(),
            severity: String::new(),
            title: String::new(),
            detail: String::new(),
            actions: vec![],
            event_id: None,
            pane_id: None,
            workflow_id: None,
        }
    }

    fn triage_malformed() -> TriageItemView {
        TriageItemView {
            section: "\u{0000}".to_string(),
            severity: "BOGUS".to_string(),
            title: "t".repeat(200),
            detail: "d".repeat(300),
            actions: vec![
                super::super::query::TriageAction {
                    label: String::new(),
                    command: String::new(),
                },
                super::super::query::TriageAction {
                    label: "x".repeat(100),
                    command: "y".repeat(100),
                },
            ],
            event_id: Some(i64::MAX),
            pane_id: Some(u64::MAX),
            workflow_id: Some(String::new()),
        }
    }

    fn triage_redacted() -> TriageItemView {
        TriageItemView {
            section: "events".to_string(),
            severity: "error".to_string(),
            title: "Secret leak detected".to_string(),
            detail: "Found token=xyzAbcDef12345 in pane output".to_string(),
            actions: vec![super::super::query::TriageAction {
                label: "Investigate".to_string(),
                command: "wa why --pane 42".to_string(),
            }],
            event_id: Some(200),
            pane_id: Some(42),
            workflow_id: None,
        }
    }

    #[test]
    fn fixture_triage_normal_all_fields() {
        let row = adapt_triage(&sample_triage());
        assert_field!("triage", "normal", "section", row.section, "events");
        assert_field!(
            "triage",
            "normal",
            "severity_label",
            row.severity_label,
            "error"
        );
        assert_field!(
            "triage",
            "normal",
            "title",
            row.title,
            "[pane 42] rate_limit: api_error"
        );
        assert_field!(
            "triage",
            "normal",
            "detail",
            row.detail,
            "API returned 429 Too Many Requests"
        );
        assert_field!(
            "triage",
            "normal",
            "action_labels.len",
            row.action_labels.len(),
            1
        );
        assert_field!(
            "triage",
            "normal",
            "action_labels[0]",
            row.action_labels[0],
            "Explain"
        );
        assert_field!(
            "triage",
            "normal",
            "action_commands[0]",
            row.action_commands[0],
            "wa why --recent --pane 42"
        );
        assert_field!("triage", "normal", "event_id", row.event_id, "100");
        assert_field!("triage", "normal", "pane_id", row.pane_id, "42");
        assert_field!("triage", "normal", "workflow_id", row.workflow_id, "");
        assert_field!(
            "triage",
            "normal",
            "severity_style.bold",
            row.severity_style.bold,
            true
        );
    }

    #[test]
    fn fixture_triage_missing_all_fields() {
        let row = adapt_triage(&triage_missing());
        assert_field!("triage", "missing", "section", row.section, "");
        assert_field!(
            "triage",
            "missing",
            "severity_label",
            row.severity_label,
            ""
        );
        assert_field!("triage", "missing", "title", row.title, "");
        assert_field!("triage", "missing", "detail", row.detail, "");
        assert_field!(
            "triage",
            "missing",
            "action_labels.len",
            row.action_labels.len(),
            0
        );
        assert_field!("triage", "missing", "event_id", row.event_id, "");
        assert_field!("triage", "missing", "pane_id", row.pane_id, "");
        assert_field!("triage", "missing", "workflow_id", row.workflow_id, "");
    }

    #[test]
    fn fixture_triage_malformed_all_fields() {
        let row = adapt_triage(&triage_malformed());
        // Title truncated to 60
        assert!(
            row.title.len() <= 60,
            "title should be truncated to 60, got {}",
            row.title.len()
        );
        // Detail truncated to 120
        assert!(
            row.detail.len() <= 120,
            "detail should be truncated to 120, got {}",
            row.detail.len()
        );
        // Actions preserved including empty ones
        assert_field!(
            "triage",
            "malformed",
            "action_labels.len",
            row.action_labels.len(),
            2
        );
        assert_field!(
            "triage",
            "malformed",
            "action_labels[0]",
            row.action_labels[0],
            ""
        );
        // Cross-ref IDs formatted from max values
        assert_field!(
            "triage",
            "malformed",
            "event_id",
            row.event_id,
            i64::MAX.to_string()
        );
        assert_field!(
            "triage",
            "malformed",
            "pane_id",
            row.pane_id,
            u64::MAX.to_string()
        );
        assert_field!("triage", "malformed", "workflow_id", row.workflow_id, "");
    }

    #[test]
    fn fixture_triage_redacted_secrets_stripped() {
        let row = adapt_triage(&triage_redacted());
        assert!(
            row.detail.contains("[REDACTED]"),
            "detail should have redacted token"
        );
        assert!(
            !row.detail.contains("xyzAbcDef12345"),
            "raw token must not appear"
        );
        // Non-secret fields preserved
        assert_field!(
            "triage",
            "redacted",
            "title",
            row.title,
            "Secret leak detected"
        );
    }

    // --- History fixtures ---

    fn history_missing() -> HistoryEntryView {
        HistoryEntryView {
            audit_id: 0,
            timestamp: 0,
            pane_id: None,
            workflow_id: None,
            action_kind: String::new(),
            result: String::new(),
            actor_kind: String::new(),
            step_name: None,
            undoable: false,
            undone: false,
            undo_strategy: None,
            undo_hint: None,
            rule_id: None,
            summary: String::new(),
        }
    }

    fn history_malformed() -> HistoryEntryView {
        HistoryEntryView {
            audit_id: i64::MAX,
            timestamp: -1,
            pane_id: Some(u64::MAX),
            workflow_id: Some("w".repeat(200)),
            action_kind: "a".repeat(100),
            result: "BOGUS_RESULT".to_string(),
            actor_kind: "\u{0000}null".to_string(),
            step_name: Some("s".repeat(200)),
            undoable: true,
            undone: true, // Both undoable and undone — undone wins
            undo_strategy: Some("u".repeat(200)),
            undo_hint: Some("h".repeat(200)),
            rule_id: Some("r".repeat(200)),
            summary: "s".repeat(200),
        }
    }

    fn history_redacted() -> HistoryEntryView {
        HistoryEntryView {
            audit_id: 99,
            timestamp: 1_700_000_000_000,
            pane_id: Some(7),
            workflow_id: None,
            action_kind: "send_text".to_string(),
            result: "success".to_string(),
            actor_kind: "robot".to_string(),
            step_name: None,
            undoable: true,
            undone: false,
            undo_strategy: Some("ctrl_c".to_string()),
            undo_hint: None,
            rule_id: None,
            summary: "Sent api_key=secretABC12345 to pane 7".to_string(),
        }
    }

    #[test]
    fn fixture_history_normal_all_fields() {
        let row = adapt_history(&sample_history());
        assert_field!("history", "normal", "audit_id", row.audit_id, "1");
        assert_field!(
            "history",
            "normal",
            "action_kind",
            row.action_kind,
            "send_text"
        );
        assert_field!(
            "history",
            "normal",
            "result_label",
            row.result_label,
            "success"
        );
        assert_field!("history", "normal", "actor_kind", row.actor_kind, "robot");
        assert_field!(
            "history",
            "normal",
            "undo_label",
            row.undo_label,
            "undoable"
        );
        assert_field!("history", "normal", "pane_id", row.pane_id, "42");
        assert_field!("history", "normal", "workflow_id", row.workflow_id, "");
        assert_field!("history", "normal", "step_name", row.step_name, "");
        assert_field!("history", "normal", "rule_id", row.rule_id, "");
        assert_field!(
            "history",
            "normal",
            "undo_strategy",
            row.undo_strategy,
            "workflow_abort"
        );
        assert_field!("history", "normal", "undo_hint", row.undo_hint, "");
        assert_field!(
            "history",
            "normal",
            "result_style.fg",
            row.result_style.fg,
            Some(ColorSpec::Green)
        );
        assert_field!(
            "history",
            "normal",
            "undo_style.fg",
            row.undo_style.fg,
            Some(ColorSpec::Cyan)
        );
    }

    #[test]
    fn fixture_history_missing_all_fields() {
        let row = adapt_history(&history_missing());
        assert_field!("history", "missing", "audit_id", row.audit_id, "0");
        assert_field!("history", "missing", "timestamp", row.timestamp, "-");
        assert_field!("history", "missing", "action_kind", row.action_kind, "");
        assert_field!("history", "missing", "result_label", row.result_label, "");
        assert_field!("history", "missing", "actor_kind", row.actor_kind, "");
        assert_field!("history", "missing", "summary", row.summary, "");
        assert_field!("history", "missing", "undo_label", row.undo_label, "");
        assert_field!("history", "missing", "pane_id", row.pane_id, "");
        assert_field!("history", "missing", "workflow_id", row.workflow_id, "");
        assert_field!("history", "missing", "step_name", row.step_name, "");
        assert_field!("history", "missing", "rule_id", row.rule_id, "");
        assert_field!("history", "missing", "undo_strategy", row.undo_strategy, "");
        assert_field!("history", "missing", "undo_hint", row.undo_hint, "");
        // Unknown result → DarkGray
        assert_field!(
            "history",
            "missing",
            "result_style.fg",
            row.result_style.fg,
            Some(ColorSpec::DarkGray)
        );
    }

    #[test]
    fn fixture_history_malformed_all_fields() {
        let row = adapt_history(&history_malformed());
        assert_field!(
            "history",
            "malformed",
            "audit_id",
            row.audit_id,
            i64::MAX.to_string()
        );
        // Summary truncated to 60
        assert!(
            row.summary.len() <= 60,
            "summary should be truncated to 60, got {}",
            row.summary.len()
        );
        // undone=true takes priority over undoable=true
        assert_field!(
            "history",
            "malformed",
            "undo_label",
            row.undo_label,
            "UNDONE"
        );
        assert_field!(
            "history",
            "malformed",
            "undo_style.dim",
            row.undo_style.dim,
            true
        );
        // Unknown result → DarkGray
        assert_field!(
            "history",
            "malformed",
            "result_style.fg",
            row.result_style.fg,
            Some(ColorSpec::DarkGray)
        );
    }

    #[test]
    fn fixture_history_redacted_secrets_stripped() {
        let row = adapt_history(&history_redacted());
        assert!(
            row.summary.contains("[REDACTED]"),
            "summary should have redacted token"
        );
        assert!(
            !row.summary.contains("secretABC12345"),
            "raw token must not appear"
        );
        assert_field!("history", "redacted", "audit_id", row.audit_id, "99");
    }

    #[test]
    fn fixture_history_result_style_variants() {
        for (result, expected_fg) in [
            ("success", Some(ColorSpec::Green)),
            ("denied", Some(ColorSpec::Yellow)),
            ("failed", Some(ColorSpec::Red)),
            ("timeout", Some(ColorSpec::DarkGray)),
            ("", Some(ColorSpec::DarkGray)),
        ] {
            let mut entry = sample_history();
            entry.result = result.to_string();
            let row = adapt_history(&entry);
            assert_field!(
                "history",
                format!("result={}", result),
                "result_style.fg",
                row.result_style.fg,
                expected_fg
            );
        }
    }

    #[test]
    fn fixture_history_undo_state_matrix() {
        // (undoable, undone) → expected undo_label
        for (undoable, undone, expected_label) in [
            (false, false, ""),
            (true, false, "undoable"),
            (false, true, "UNDONE"),
            (true, true, "UNDONE"), // undone takes priority
        ] {
            let mut entry = sample_history();
            entry.undoable = undoable;
            entry.undone = undone;
            let row = adapt_history(&entry);
            assert_field!(
                "history",
                format!("undoable={},undone={}", undoable, undone),
                "undo_label",
                row.undo_label,
                expected_label
            );
        }
    }

    // --- Search fixtures ---

    fn search_missing() -> SearchResultView {
        SearchResultView {
            pane_id: 0,
            timestamp: 0,
            snippet: String::new(),
            rank: 0.0,
        }
    }

    fn search_malformed() -> SearchResultView {
        SearchResultView {
            pane_id: u64::MAX,
            timestamp: -1,
            snippet: "x".repeat(1000),
            rank: f64::NEG_INFINITY,
        }
    }

    fn search_redacted() -> SearchResultView {
        SearchResultView {
            pane_id: 42,
            timestamp: 1_700_000_000_000,
            snippet: "Output: password=superSecret99 detected".to_string(),
            rank: 2.5,
        }
    }

    #[test]
    fn fixture_search_normal_all_fields() {
        let row = adapt_search(&sample_search());
        assert_field!("search", "normal", "pane_id", row.pane_id, "42");
        assert!(row.timestamp.contains("2023"));
        assert!(row.snippet.contains(">>error<<"));
        assert_field!("search", "normal", "rank_label", row.rank_label, "3.14");
    }

    #[test]
    fn fixture_search_missing_all_fields() {
        let row = adapt_search(&search_missing());
        assert_field!("search", "missing", "pane_id", row.pane_id, "0");
        assert_field!("search", "missing", "timestamp", row.timestamp, "-");
        assert_field!("search", "missing", "snippet", row.snippet, "");
        assert_field!("search", "missing", "rank_label", row.rank_label, "0.00");
    }

    #[test]
    fn fixture_search_malformed_all_fields() {
        let row = adapt_search(&search_malformed());
        assert_field!(
            "search",
            "malformed",
            "pane_id",
            row.pane_id,
            u64::MAX.to_string()
        );
        // Very long snippet is NOT truncated by adapt_search (only redacted)
        assert!(row.snippet.len() == 1000);
        // NEG_INFINITY formats as "-inf"
        assert_field!("search", "malformed", "rank_label", row.rank_label, "-inf");
    }

    #[test]
    fn fixture_search_redacted_secrets_stripped() {
        let row = adapt_search(&search_redacted());
        assert!(
            row.snippet.contains("[REDACTED]"),
            "snippet should have redacted token"
        );
        assert!(
            !row.snippet.contains("superSecret99"),
            "raw token must not appear"
        );
    }

    // --- Workflow fixtures ---

    fn workflow_missing() -> WorkflowProgressView {
        WorkflowProgressView {
            id: String::new(),
            workflow_name: String::new(),
            pane_id: 0,
            current_step: 0,
            total_steps: 0,
            status: String::new(),
            error: None,
            started_at: 0,
            updated_at: 0,
        }
    }

    fn workflow_malformed() -> WorkflowProgressView {
        WorkflowProgressView {
            id: "w".repeat(200),
            workflow_name: "\u{0000}".to_string(),
            pane_id: u64::MAX,
            current_step: usize::MAX,
            total_steps: 0,
            status: "BOGUS_STATUS".to_string(),
            error: Some("e".repeat(500)),
            started_at: -1,
            updated_at: i64::MAX,
        }
    }

    #[test]
    fn fixture_workflow_normal_all_fields() {
        let row = adapt_workflow(&sample_workflow());
        assert_field!("workflow", "normal", "id", row.id, "wf-001");
        assert_field!("workflow", "normal", "name", row.name, "rate_limit_backoff");
        assert_field!("workflow", "normal", "pane_id", row.pane_id, "42");
        assert_field!(
            "workflow",
            "normal",
            "progress_label",
            row.progress_label,
            "2/5"
        );
        assert_field!(
            "workflow",
            "normal",
            "status_label",
            row.status_label,
            "running"
        );
        assert!(row.error.is_none());
        assert_field!(
            "workflow",
            "normal",
            "status_style.fg",
            row.status_style.fg,
            Some(ColorSpec::Cyan)
        );
    }

    #[test]
    fn fixture_workflow_missing_all_fields() {
        let row = adapt_workflow(&workflow_missing());
        assert_field!("workflow", "missing", "id", row.id, "");
        assert_field!("workflow", "missing", "name", row.name, "");
        assert_field!("workflow", "missing", "pane_id", row.pane_id, "0");
        assert_field!(
            "workflow",
            "missing",
            "progress_label",
            row.progress_label,
            "0/0"
        );
        assert_field!("workflow", "missing", "status_label", row.status_label, "");
        assert!(row.error.is_none());
        assert_field!("workflow", "missing", "started_at", row.started_at, "-");
        assert_field!("workflow", "missing", "updated_at", row.updated_at, "-");
        assert_field!(
            "workflow",
            "missing",
            "status_style.fg",
            row.status_style.fg,
            Some(ColorSpec::DarkGray)
        );
    }

    #[test]
    fn fixture_workflow_malformed_all_fields() {
        let row = adapt_workflow(&workflow_malformed());
        assert_field!(
            "workflow",
            "malformed",
            "progress_label",
            row.progress_label,
            format!("{}/0", usize::MAX)
        );
        assert!(row.error.is_some());
        assert_field!(
            "workflow",
            "malformed",
            "status_style.fg",
            row.status_style.fg,
            Some(ColorSpec::DarkGray)
        );
    }

    #[test]
    fn fixture_workflow_status_variants() {
        for (status, expected_fg) in [
            ("running", Some(ColorSpec::Cyan)),
            ("pending", Some(ColorSpec::Cyan)),
            ("completed", Some(ColorSpec::Green)),
            ("failed", Some(ColorSpec::Red)),
            ("error", Some(ColorSpec::Red)),
            ("unknown", Some(ColorSpec::DarkGray)),
            ("", Some(ColorSpec::DarkGray)),
        ] {
            let mut wf = sample_workflow();
            wf.status = status.to_string();
            let row = adapt_workflow(&wf);
            assert_field!(
                "workflow",
                format!("status={}", status),
                "status_style.fg",
                row.status_style.fg,
                expected_fg
            );
        }
    }

    // --- Health fixtures ---

    fn health_all_down() -> HealthStatus {
        HealthStatus {
            watcher_running: false,
            db_accessible: false,
            wezterm_accessible: false,
            wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus {
                state: CircuitStateKind::Open,
                consecutive_failures: 10,
                ..Default::default()
            },
            pane_count: 0,
            event_count: 0,
            last_capture_ts: None,
        }
    }

    fn health_half_open() -> HealthStatus {
        HealthStatus {
            watcher_running: true,
            db_accessible: true,
            wezterm_accessible: false,
            wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus {
                state: CircuitStateKind::HalfOpen,
                consecutive_failures: 3,
                ..Default::default()
            },
            pane_count: 2,
            event_count: 10,
            last_capture_ts: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn fixture_health_all_down_fields() {
        let model = adapt_health(&health_all_down());
        assert_field!(
            "health",
            "all_down",
            "watcher_label",
            model.watcher_label,
            "stopped"
        );
        assert_field!(
            "health",
            "all_down",
            "watcher_style.bold",
            model.watcher_style.bold,
            true
        );
        assert_field!(
            "health",
            "all_down",
            "watcher_style.fg",
            model.watcher_style.fg,
            Some(ColorSpec::Red)
        );
        assert_field!(
            "health",
            "all_down",
            "db_label",
            model.db_label,
            "unavailable"
        );
        assert_field!(
            "health",
            "all_down",
            "db_style.fg",
            model.db_style.fg,
            Some(ColorSpec::Red)
        );
        assert_field!(
            "health",
            "all_down",
            "wezterm_label",
            model.wezterm_label,
            "unavailable"
        );
        assert_field!(
            "health",
            "all_down",
            "circuit_label",
            model.circuit_label,
            "OPEN"
        );
        assert_field!(
            "health",
            "all_down",
            "circuit_style.bold",
            model.circuit_style.bold,
            true
        );
        assert_field!(
            "health",
            "all_down",
            "circuit_style.fg",
            model.circuit_style.fg,
            Some(ColorSpec::Red)
        );
        assert_field!("health", "all_down", "pane_count", model.pane_count, "0");
        assert_field!("health", "all_down", "event_count", model.event_count, "0");
    }

    #[test]
    fn fixture_health_half_open_fields() {
        let model = adapt_health(&health_half_open());
        assert_field!(
            "health",
            "half_open",
            "watcher_label",
            model.watcher_label,
            "running"
        );
        assert_field!(
            "health",
            "half_open",
            "watcher_style.fg",
            model.watcher_style.fg,
            Some(ColorSpec::Green)
        );
        assert_field!("health", "half_open", "db_label", model.db_label, "ok");
        assert_field!(
            "health",
            "half_open",
            "wezterm_label",
            model.wezterm_label,
            "unavailable"
        );
        assert_field!(
            "health",
            "half_open",
            "circuit_label",
            model.circuit_label,
            "half-open"
        );
        assert_field!(
            "health",
            "half_open",
            "circuit_style.fg",
            model.circuit_style.fg,
            Some(ColorSpec::Yellow)
        );
        assert_field!("health", "half_open", "pane_count", model.pane_count, "2");
        assert_field!(
            "health",
            "half_open",
            "event_count",
            model.event_count,
            "10"
        );
    }

    // --- Cross-domain batch validation ---

    /// Verify all adapters handle the full fixture corpus without panic.
    #[test]
    fn fixture_all_adapters_no_panic() {
        // Pane variants
        let _ = adapt_pane(&sample_pane());
        let _ = adapt_pane(&pane_missing());
        let _ = adapt_pane(&pane_malformed());

        // Event variants
        let _ = adapt_event(&sample_event());
        let _ = adapt_event(&event_missing());
        let _ = adapt_event(&event_malformed());
        let _ = adapt_event(&event_redacted());

        // Triage variants
        let _ = adapt_triage(&sample_triage());
        let _ = adapt_triage(&triage_missing());
        let _ = adapt_triage(&triage_malformed());
        let _ = adapt_triage(&triage_redacted());

        // History variants
        let _ = adapt_history(&sample_history());
        let _ = adapt_history(&history_missing());
        let _ = adapt_history(&history_malformed());
        let _ = adapt_history(&history_redacted());

        // Search variants
        let _ = adapt_search(&sample_search());
        let _ = adapt_search(&search_missing());
        let _ = adapt_search(&search_malformed());
        let _ = adapt_search(&search_redacted());

        // Workflow variants
        let _ = adapt_workflow(&sample_workflow());
        let _ = adapt_workflow(&workflow_missing());
        let _ = adapt_workflow(&workflow_malformed());

        // Health variants
        let health_ok = HealthStatus {
            watcher_running: true,
            db_accessible: true,
            wezterm_accessible: true,
            wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
            pane_count: 5,
            event_count: 42,
            last_capture_ts: Some(1_700_000_000_000),
        };
        let _ = adapt_health(&health_ok);
        let _ = adapt_health(&health_all_down());
        let _ = adapt_health(&health_half_open());
    }

    /// Verify stable ordering: adapting the same input twice yields identical output.
    #[test]
    fn fixture_deterministic_output() {
        let pane1 = adapt_pane(&sample_pane());
        let pane2 = adapt_pane(&sample_pane());
        assert_eq!(pane1.pane_id, pane2.pane_id);
        assert_eq!(pane1.title, pane2.title);
        assert_eq!(pane1.agent_label, pane2.agent_label);

        let event1 = adapt_event(&sample_event());
        let event2 = adapt_event(&sample_event());
        assert_eq!(event1.id, event2.id);
        assert_eq!(event1.message, event2.message);
        assert_eq!(event1.timestamp, event2.timestamp);

        let history1 = adapt_history(&sample_history());
        let history2 = adapt_history(&sample_history());
        assert_eq!(history1.audit_id, history2.audit_id);
        assert_eq!(history1.summary, history2.summary);
        assert_eq!(history1.undo_label, history2.undo_label);

        let triage1 = adapt_triage(&sample_triage());
        let triage2 = adapt_triage(&sample_triage());
        assert_eq!(triage1.title, triage2.title);
        assert_eq!(triage1.action_labels, triage2.action_labels);
    }

    // -- Timeline adapter tests (wa-6sk.4) --

    fn sample_timeline_event() -> TimelineEvent {
        use crate::storage::{CorrelationType, HandledInfo, PaneInfo};
        TimelineEvent {
            id: 42,
            timestamp: 1_700_000_060_000,
            pane_info: PaneInfo {
                pane_id: 3,
                pane_uuid: None,
                agent_type: Some("codex".to_string()),
                domain: "local".to_string(),
                cwd: Some("/data/projects/foo".to_string()),
                title: Some("codex session".to_string()),
            },
            rule_id: "error_burst_detected".to_string(),
            event_type: "error_burst".to_string(),
            severity: "error".to_string(),
            confidence: 0.95,
            handled: Some(HandledInfo {
                handled_at: 1_700_000_062_000,
                workflow_id: Some("wf-1".to_string()),
                status: "resolved".to_string(),
            }),
            correlations: vec![
                CorrelationRef {
                    id: "corr-1".to_string(),
                    correlation_type: CorrelationType::Failover,
                },
                CorrelationRef {
                    id: "corr-2".to_string(),
                    correlation_type: CorrelationType::Cascade,
                },
            ],
            summary: Some("Error burst detected in pane 3".to_string()),
        }
    }

    #[test]
    fn adapt_timeline_event_formats_id() {
        let row = adapt_timeline_event(&sample_timeline_event());
        assert_eq!(row.id, "42");
    }

    #[test]
    fn adapt_timeline_event_formats_pane_label() {
        let row = adapt_timeline_event(&sample_timeline_event());
        assert_eq!(row.pane_label, "P3");
    }

    #[test]
    fn adapt_timeline_event_formats_agent() {
        let row = adapt_timeline_event(&sample_timeline_event());
        assert_eq!(row.agent_label, "codex");
    }

    #[test]
    fn adapt_timeline_event_handled_label() {
        let row = adapt_timeline_event(&sample_timeline_event());
        assert_eq!(row.handled_label, "handled");
    }

    #[test]
    fn adapt_timeline_event_unhandled_label() {
        let mut event = sample_timeline_event();
        event.handled = None;
        let row = adapt_timeline_event(&event);
        assert_eq!(row.handled_label, "OPEN");
    }

    #[test]
    fn adapt_timeline_event_correlation_label() {
        let row = adapt_timeline_event(&sample_timeline_event());
        assert!(row.correlation_label.contains("failover"));
        assert!(row.correlation_label.contains("cascade"));
    }

    #[test]
    fn adapt_timeline_event_no_correlations() {
        let mut event = sample_timeline_event();
        event.correlations.clear();
        let row = adapt_timeline_event(&event);
        assert!(row.correlation_label.is_empty());
    }

    #[test]
    fn adapt_timeline_event_severity_error_bold() {
        let row = adapt_timeline_event(&sample_timeline_event());
        assert!(row.severity_style.bold);
    }

    #[test]
    fn adapt_timeline_event_unknown_agent() {
        let mut event = sample_timeline_event();
        event.pane_info.agent_type = None;
        let row = adapt_timeline_event(&event);
        assert_eq!(row.agent_label, "unknown");
    }

    #[test]
    fn adapt_timeline_event_deterministic() {
        let row1 = adapt_timeline_event(&sample_timeline_event());
        let row2 = adapt_timeline_event(&sample_timeline_event());
        assert_eq!(row1.id, row2.id);
        assert_eq!(row1.pane_label, row2.pane_label);
        assert_eq!(row1.severity_label, row2.severity_label);
        assert_eq!(row1.correlation_label, row2.correlation_label);
    }

    #[test]
    fn format_correlations_empty() {
        assert!(format_correlations(&[]).is_empty());
    }

    #[test]
    fn format_correlations_multiple() {
        use crate::storage::CorrelationType;
        let refs = vec![
            CorrelationRef {
                id: "a".to_string(),
                correlation_type: CorrelationType::Failover,
            },
            CorrelationRef {
                id: "b".to_string(),
                correlation_type: CorrelationType::Temporal,
            },
        ];
        let label = format_correlations(&refs);
        assert!(label.contains("failover"));
        assert!(label.contains("temporal"));
    }
}
