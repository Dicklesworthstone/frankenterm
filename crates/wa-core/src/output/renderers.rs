//! Renderers for CLI output
//!
//! Each renderer takes typed data and produces formatted output.
//! Renderers are separate from data acquisition - they only handle display.

use super::format::{OutputFormat, Style};
use super::table::{Alignment, Column, Table};
use crate::storage::{PaneRecord, SearchResult, StoredEvent};

/// Rendering context with shared settings
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// Output format
    pub format: OutputFormat,
    /// Whether to show verbose details
    pub verbose: bool,
    /// Maximum items to display (0 = unlimited)
    pub limit: usize,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            format: OutputFormat::Auto,
            verbose: false,
            limit: 0,
        }
    }
}

impl RenderContext {
    /// Create a new render context with the given format
    #[must_use]
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            ..Default::default()
        }
    }

    /// Set verbose mode
    #[must_use]
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Set display limit
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// Trait for types that can be rendered to string
pub trait Render {
    /// Render to string with the given context
    fn render(&self, ctx: &RenderContext) -> String;

    /// Render with default context (auto format)
    fn render_default(&self) -> String {
        self.render(&RenderContext::default())
    }
}

// =============================================================================
// Pane Table Renderer
// =============================================================================

/// Renderer for pane status table
pub struct PaneTableRenderer;

impl PaneTableRenderer {
    /// Render a list of panes
    #[must_use]
    pub fn render(panes: &[PaneRecord], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(panes).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if panes.is_empty() {
            return style.dim("No panes observed\n");
        }

        let observed: Vec<_> = panes.iter().filter(|p| p.observed).collect();
        let ignored: Vec<_> = panes.iter().filter(|p| !p.observed).collect();

        let mut output = String::new();

        // Header with counts
        output.push_str(&format!(
            "{}\n",
            style.bold(&format!(
                "Panes ({} observed, {} ignored):",
                observed.len(),
                ignored.len()
            ))
        ));

        // Build table
        let mut table = Table::new(vec![
            Column::new("ID").align(Alignment::Right).min_width(4),
            Column::new("STATUS").min_width(8),
            Column::new("TITLE").max_width(24),
            Column::new("CWD").max_width(40),
            Column::new("DOMAIN").min_width(8),
        ])
        .with_format(ctx.format);

        for pane in panes {
            let status = if pane.observed {
                style.green("observed")
            } else {
                style.gray("ignored")
            };

            let title = pane.title.as_deref().unwrap_or("-");
            let cwd = pane.cwd.as_deref().unwrap_or("-");

            table.add_row(vec![
                pane.pane_id.to_string(),
                status,
                title.to_string(),
                cwd.to_string(),
                pane.domain.clone(),
            ]);
        }

        output.push_str(&table.render());
        output
    }

    /// Render a single pane detail view
    #[must_use]
    pub fn render_detail(pane: &PaneRecord, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(pane).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        output.push_str(&style.bold(&format!("Pane {}\n", pane.pane_id)));
        output.push_str(&format!(
            "  Status:  {}\n",
            if pane.observed {
                style.green("observed")
            } else {
                style.gray("ignored")
            }
        ));
        output.push_str(&format!("  Domain:  {}\n", pane.domain));

        if let Some(title) = &pane.title {
            output.push_str(&format!("  Title:   {title}\n"));
        }
        if let Some(cwd) = &pane.cwd {
            output.push_str(&format!("  CWD:     {cwd}\n"));
        }
        if let Some(tty) = &pane.tty_name {
            output.push_str(&format!("  TTY:     {tty}\n"));
        }

        if ctx.verbose {
            output.push_str(&format!(
                "  First seen: {}\n",
                format_timestamp(pane.first_seen_at)
            ));
            output.push_str(&format!(
                "  Last seen:  {}\n",
                format_timestamp(pane.last_seen_at)
            ));
            if let Some(reason) = &pane.ignore_reason {
                output.push_str(&format!("  Ignore reason: {reason}\n"));
            }
        }

        output
    }
}

// =============================================================================
// Event List Renderer
// =============================================================================

/// Renderer for event lists
pub struct EventListRenderer;

impl EventListRenderer {
    /// Render a list of events
    #[must_use]
    pub fn render(events: &[StoredEvent], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(events).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if events.is_empty() {
            return style.dim("No events found\n");
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Events ({}):\n", events.len())));

        // Build table
        let mut table = Table::new(vec![
            Column::new("ID").align(Alignment::Right).min_width(5),
            Column::new("PANE").align(Alignment::Right).min_width(4),
            Column::new("RULE").min_width(16),
            Column::new("SEV").min_width(8),
            Column::new("TIME").min_width(20),
            Column::new("STATUS").min_width(10),
        ])
        .with_format(ctx.format);

        let display_events = if ctx.limit > 0 && events.len() > ctx.limit {
            &events[..ctx.limit]
        } else {
            events
        };

        for event in display_events {
            let severity = style.severity(&event.severity, &event.severity);
            let status = if event.handled_at.is_some() {
                style.green("handled")
            } else {
                style.yellow("pending")
            };

            table.add_row(vec![
                event.id.to_string(),
                event.pane_id.to_string(),
                event.rule_id.clone(),
                severity,
                format_timestamp(event.detected_at),
                status,
            ]);
        }

        output.push_str(&table.render());

        if ctx.limit > 0 && events.len() > ctx.limit {
            output.push_str(&style.dim(&format!(
                "\n... and {} more events (use --limit to see more)\n",
                events.len() - ctx.limit
            )));
        }

        output
    }

    /// Render a single event detail view
    #[must_use]
    pub fn render_detail(event: &StoredEvent, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(event).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        output.push_str(&style.bold(&format!("Event #{}\n", event.id)));
        output.push_str(&format!("  Rule:       {}\n", event.rule_id));
        output.push_str(&format!("  Pane:       {}\n", event.pane_id));
        output.push_str(&format!("  Agent:      {}\n", event.agent_type));
        output.push_str(&format!("  Type:       {}\n", event.event_type));
        output.push_str(&format!(
            "  Severity:   {}\n",
            style.severity(&event.severity, &event.severity)
        ));
        output.push_str(&format!("  Confidence: {:.2}\n", event.confidence));
        output.push_str(&format!(
            "  Detected:   {}\n",
            format_timestamp(event.detected_at)
        ));

        if let Some(handled_at) = event.handled_at {
            output.push_str(&format!("  Handled:    {}\n", format_timestamp(handled_at)));
            if let Some(workflow) = &event.handled_by_workflow_id {
                output.push_str(&format!("  By workflow: {workflow}\n"));
            }
            if let Some(status) = &event.handled_status {
                output.push_str(&format!("  Status:     {status}\n"));
            }
        } else {
            output.push_str(&format!("  Status:     {}\n", style.yellow("unhandled")));
        }

        if let Some(matched) = &event.matched_text {
            output.push_str(&format!(
                "\n  Matched text:\n    {}\n",
                truncate(matched, 200)
            ));
        }

        if let Some(extracted) = &event.extracted {
            output.push_str("\n  Extracted data:\n");
            let json = serde_json::to_string_pretty(extracted).unwrap_or_else(|_| "{}".to_string());
            for line in json.lines() {
                output.push_str(&format!("    {line}\n"));
            }
        }

        output
    }
}

// =============================================================================
// Search Result Renderer
// =============================================================================

/// Renderer for search results
pub struct SearchResultRenderer;

impl SearchResultRenderer {
    /// Render search results
    #[must_use]
    pub fn render(results: &[SearchResult], query: &str, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(results).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if results.is_empty() {
            return format!(
                "{}\n",
                style.dim(&format!("No results for query: \"{query}\""))
            );
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!(
            "Search results for \"{}\" ({} hits):\n\n",
            query,
            results.len()
        )));

        let display_results = if ctx.limit > 0 && results.len() > ctx.limit {
            &results[..ctx.limit]
        } else {
            results
        };

        for (i, result) in display_results.iter().enumerate() {
            let segment = &result.segment;

            // Result header
            output.push_str(&format!(
                "{}. {} pane:{} seq:{} score:{:.2}\n",
                style.bold(&(i + 1).to_string()),
                style.gray(&format_timestamp(segment.captured_at)),
                segment.pane_id,
                segment.seq,
                result.score
            ));

            // Snippet or content preview
            if let Some(snippet) = &result.snippet {
                output.push_str(&format!("   {}\n", truncate(snippet, 120)));
            } else {
                output.push_str(&format!("   {}\n", truncate(&segment.content, 120)));
            }

            output.push('\n');
        }

        if ctx.limit > 0 && results.len() > ctx.limit {
            output.push_str(&style.dim(&format!(
                "... and {} more results (use --limit to see more)\n",
                results.len() - ctx.limit
            )));
        }

        output
    }
}

// =============================================================================
// Workflow Result Renderer
// =============================================================================

/// Workflow execution result for rendering
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowResult {
    /// Workflow execution ID
    pub workflow_id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Target pane ID
    pub pane_id: u64,
    /// Execution status
    pub status: String,
    /// Status reason (if failed)
    pub reason: Option<String>,
    /// Execution result data
    pub result: Option<serde_json::Value>,
    /// Step results
    pub steps: Vec<WorkflowStepResult>,
}

/// Single workflow step result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowStepResult {
    /// Step name
    pub name: String,
    /// Step outcome
    pub outcome: String,
    /// Step duration (ms)
    pub duration_ms: u64,
    /// Error message if failed
    pub error: Option<String>,
}

/// Renderer for workflow results
pub struct WorkflowResultRenderer;

impl WorkflowResultRenderer {
    /// Render a workflow result
    #[must_use]
    pub fn render(result: &WorkflowResult, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        // Header
        let status_styled = match result.status.as_str() {
            "success" | "completed" => style.green(&result.status),
            "failed" | "error" => style.red(&result.status),
            "running" | "pending" => style.yellow(&result.status),
            _ => result.status.clone(),
        };

        output.push_str(&format!(
            "{} [{}]\n",
            style.bold(&result.workflow_name),
            status_styled
        ));
        output.push_str(&format!("  ID:   {}\n", result.workflow_id));
        output.push_str(&format!("  Pane: {}\n", result.pane_id));

        if let Some(reason) = &result.reason {
            output.push_str(&format!("  Reason: {reason}\n"));
        }

        // Steps
        if !result.steps.is_empty() {
            output.push_str("\n  Steps:\n");
            for step in &result.steps {
                let outcome = match step.outcome.as_str() {
                    "success" | "completed" => style.green("✓"),
                    "failed" | "error" => style.red("✗"),
                    "skipped" => style.gray("○"),
                    _ => step.outcome.clone(),
                };

                output.push_str(&format!(
                    "    {} {} ({}ms)\n",
                    outcome, step.name, step.duration_ms
                ));

                if let Some(error) = &step.error {
                    output.push_str(&format!("      Error: {}\n", style.red(error)));
                }
            }
        }

        // Result data (verbose only)
        if ctx.verbose {
            if let Some(data) = &result.result {
                output.push_str("\n  Result data:\n");
                let json = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());
                for line in json.lines() {
                    output.push_str(&format!("    {line}\n"));
                }
            }
        }

        output
    }
}

// =============================================================================
// Summary/Stats Renderer
// =============================================================================

/// Summary statistics for display
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    /// Total panes
    pub total_panes: usize,
    /// Observed panes
    pub observed_panes: usize,
    /// Total segments captured
    pub total_segments: u64,
    /// Total events detected
    pub total_events: u64,
    /// Unhandled events
    pub unhandled_events: u64,
    /// Active workflows
    pub active_workflows: usize,
}

/// Renderer for status summary
pub struct SummaryRenderer;

impl SummaryRenderer {
    /// Render a status summary
    #[must_use]
    pub fn render(summary: &Summary, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(summary).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        output.push_str(&style.bold("Status Summary\n"));
        output.push_str(&format!(
            "  Panes:     {} observed / {} total\n",
            summary.observed_panes, summary.total_panes
        ));
        output.push_str(&format!("  Segments:  {}\n", summary.total_segments));
        output.push_str(&format!(
            "  Events:    {} total ({} unhandled)\n",
            summary.total_events, summary.unhandled_events
        ));
        output.push_str(&format!(
            "  Workflows: {} active\n",
            summary.active_workflows
        ));

        output
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Format epoch milliseconds as human-readable timestamp
#[must_use]
pub fn format_timestamp(epoch_ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};

    let secs = epoch_ms / 1000;
    let nanos = ((epoch_ms % 1000) * 1_000_000) as u32;

    let duration = Duration::new(secs as u64, nanos);
    let datetime = UNIX_EPOCH + duration;

    // Format as ISO 8601 (simplified)
    let secs_since_epoch = datetime
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Calculate components
    let days_since_epoch = secs_since_epoch / 86400;
    let secs_in_day = secs_since_epoch % 86400;
    let hours = secs_in_day / 3600;
    let minutes = (secs_in_day % 3600) / 60;
    let seconds = secs_in_day % 60;

    // Approximate year/month/day (good enough for display)
    let mut year = 1970;
    let mut remaining_days = days_since_epoch;

    while remaining_days >= days_in_year(year) {
        remaining_days -= days_in_year(year);
        year += 1;
    }

    let mut month = 1;
    while remaining_days >= days_in_month(year, month) {
        remaining_days -= days_in_month(year, month);
        month += 1;
    }

    let day = remaining_days + 1;

    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02}")
}

fn days_in_year(year: u64) -> u64 {
    if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
        366
    } else {
        365
    }
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Truncate a string with ellipsis
#[must_use]
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pane() -> PaneRecord {
        PaneRecord {
            pane_id: 1,
            domain: "local".to_string(),
            window_id: Some(0),
            tab_id: Some(0),
            title: Some("test shell".to_string()),
            cwd: Some("/home/user".to_string()),
            tty_name: Some("/dev/pts/0".to_string()),
            first_seen_at: 1_737_446_400_000,
            last_seen_at: 1_737_450_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    fn sample_event() -> StoredEvent {
        StoredEvent {
            id: 42,
            pane_id: 1,
            rule_id: "codex.usage_reached".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage_alert".to_string(),
            severity: "warning".to_string(),
            confidence: 0.95,
            extracted: Some(serde_json::json!({"usage": 95})),
            matched_text: Some("Usage at 95%".to_string()),
            segment_id: Some(100),
            detected_at: 1_737_446_400_000,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    #[test]
    fn test_pane_table_render() {
        let panes = vec![sample_pane()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = PaneTableRenderer::render(&panes, &ctx);

        assert!(output.contains("Panes"));
        assert!(output.contains("observed"));
        assert!(output.contains("test shell"));
    }

    #[test]
    fn test_event_list_render() {
        let events = vec![sample_event()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render(&events, &ctx);

        assert!(output.contains("Events"));
        assert!(output.contains("codex.usage_reached"));
        assert!(output.contains("warning"));
    }

    #[test]
    fn test_json_output() {
        let panes = vec![sample_pane()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = PaneTableRenderer::render(&panes, &ctx);

        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("hi", 2), "hi");
    }

    #[test]
    fn test_format_timestamp() {
        // Test that timestamp formatting produces valid format
        // 1737417600000 ms = 2025-01-20 12:00:00 UTC
        let ts = format_timestamp(1_737_417_600_000);
        assert!(ts.starts_with("2025-01-2"));
        assert!(ts.contains(':'));

        // Also verify the format structure
        let parts: Vec<&str> = ts.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains('-')); // date part
        assert!(parts[1].contains(':')); // time part
    }
}
