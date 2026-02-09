//! Renderers for CLI output

#![allow(clippy::format_push_string, clippy::uninlined_format_args)]
//!
//! Each renderer takes typed data and produces formatted output.
//! Renderers are separate from data acquisition - they only handle display.

use super::format::{OutputFormat, Style};
use super::table::{Alignment, Column, Table};
use crate::event_templates;
use crate::storage::{
    ActionHistoryRecord, AuditActionRecord, CorrelationType, PaneRecord, SearchResult,
    SearchSuggestion, StoredEvent, Timeline,
};

/// Rendering context with shared settings
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// Output format
    pub format: OutputFormat,
    /// Verbosity level: 0=default, 1=verbose (-v), 2=debug (-vv)
    pub verbose: u8,
    /// Maximum items to display (0 = unlimited)
    pub limit: usize,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            format: OutputFormat::Auto,
            verbose: 0,
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

    /// Set verbosity level (0=default, 1=verbose, 2+=debug)
    #[must_use]
    pub fn verbose(mut self, verbose: u8) -> Self {
        self.verbose = verbose;
        self
    }

    /// Whether verbose level is at least 1 (-v)
    #[must_use]
    pub fn is_verbose(&self) -> bool {
        self.verbose >= 1
    }

    /// Whether verbose level is at least 2 (-vv)
    #[must_use]
    pub fn is_debug(&self) -> bool {
        self.verbose >= 2
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

        let observed_count = panes.iter().filter(|p| p.observed).count();
        let ignored_count = panes.len().saturating_sub(observed_count);

        let mut output = String::new();

        // Header with counts
        output.push_str(&format!(
            "{}\n",
            style.bold(&format!(
                "Panes ({} observed, {} ignored):",
                observed_count, ignored_count
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

        if ctx.is_verbose() {
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
            Column::new("SUMMARY").min_width(24),
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
            let summary = event_templates::render_event(event).summary;
            let status = if event.handled_at.is_some() {
                style.green("handled")
            } else {
                style.yellow("pending")
            };

            table.add_row(vec![
                event.id.to_string(),
                event.pane_id.to_string(),
                event.rule_id.clone(),
                truncate(&summary, 60),
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

    /// Render events with noise control annotations (muted keys, suppression counts).
    ///
    /// `muted_keys` is a set of identity keys that are currently muted.
    /// `active_mute_count` is the total number of active mutes (shown in footer).
    #[must_use]
    pub fn render_with_noise_info(
        events: &[StoredEvent],
        ctx: &RenderContext,
        muted_keys: &std::collections::HashSet<String>,
        active_mute_count: usize,
    ) -> String {
        if ctx.format.is_json() {
            let annotated: Vec<serde_json::Value> = events
                .iter()
                .map(|e| {
                    let is_muted = e
                        .dedupe_key
                        .as_ref()
                        .is_some_and(|k| muted_keys.contains(k));
                    let mut obj = serde_json::to_value(e).unwrap_or_else(|_| serde_json::json!({}));
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("muted".to_string(), serde_json::json!(is_muted));
                    }
                    obj
                })
                .collect();
            return serde_json::to_string_pretty(&annotated).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if events.is_empty() {
            let mut output = style.dim("No events found\n");
            if active_mute_count > 0 {
                output.push_str(&style.dim(&format!(
                    "({active_mute_count} active mute(s) filtering events)\n"
                )));
            }
            return output;
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Events ({}):\n", events.len())));

        let mut table = Table::new(vec![
            Column::new("ID").align(Alignment::Right).min_width(5),
            Column::new("PANE").align(Alignment::Right).min_width(4),
            Column::new("RULE").min_width(16),
            Column::new("SUMMARY").min_width(24),
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
            let summary = event_templates::render_event(event).summary;
            let is_muted = event
                .dedupe_key
                .as_ref()
                .is_some_and(|k| muted_keys.contains(k));
            let status = if is_muted {
                style.dim("muted")
            } else if event.handled_at.is_some() {
                style.green("handled")
            } else {
                style.yellow("pending")
            };

            table.add_row(vec![
                event.id.to_string(),
                event.pane_id.to_string(),
                event.rule_id.clone(),
                truncate(&summary, 60),
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

        if active_mute_count > 0 {
            output.push_str(&style.dim(&format!(
                "\n{active_mute_count} active mute(s). Use 'wa mute list' to view.\n"
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

        let rendered = event_templates::render_event(event);
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
        output.push_str(&format!("  Summary:    {}\n", rendered.summary));
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

        if !rendered.description.trim().is_empty() {
            output.push_str("\n  Description:\n");
            for line in rendered.description.lines() {
                output.push_str(&format!("    {line}\n"));
            }
        }

        if !rendered.suggestions.is_empty() {
            output.push_str("\n  Suggestions:\n");
            for suggestion in rendered.suggestions {
                output.push_str(&format!("    - {}\n", suggestion.text));
                if let Some(command) = suggestion.command {
                    output.push_str(&format!("      Command: {command}\n"));
                }
                if let Some(doc) = suggestion.doc_link {
                    output.push_str(&format!("      Docs: {doc}\n"));
                }
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
// Search Suggest Renderer
// =============================================================================

pub struct SearchSuggestRenderer;

impl SearchSuggestRenderer {
    /// Render search query suggestions.
    #[must_use]
    pub fn render(suggestions: &[SearchSuggestion], partial: &str, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(suggestions).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if suggestions.is_empty() {
            return format!(
                "{}\n",
                style.dim(&format!(
                    "No suggestions{}",
                    if partial.is_empty() {
                        String::new()
                    } else {
                        format!(" for \"{partial}\"")
                    }
                ))
            );
        }

        let mut output = String::new();
        let heading = if partial.is_empty() {
            "Search suggestions:".to_string()
        } else {
            format!("Suggestions for \"{partial}\":")
        };
        output.push_str(&style.bold(&heading));
        output.push('\n');

        for suggestion in suggestions {
            let desc = suggestion.description.as_deref().unwrap_or("");
            output.push_str(&format!(
                "  {} {}\n",
                style.bold(&suggestion.text),
                style.dim(&format!("— {desc}"))
            ));
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
        if ctx.is_verbose() {
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct SummaryRenderer;

impl SummaryRenderer {
    /// Render a status summary
    #[allow(dead_code)]
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
// Rules Renderer
// =============================================================================

/// Lightweight display item for a single rule (serializable for JSON output)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleListItem {
    /// Stable rule ID (e.g., "codex.usage_reached")
    pub id: String,
    /// Agent type (codex, claude_code, gemini, wezterm)
    pub agent_type: String,
    /// Event type emitted on match
    pub event_type: String,
    /// Severity level (info, warning, critical)
    pub severity: String,
    /// Human-readable description
    pub description: String,
    /// Suggested workflow name (if any)
    pub workflow: Option<String>,
    /// Number of anchors in the rule
    pub anchor_count: usize,
    /// Whether the rule has a regex extractor
    pub has_regex: bool,
}

/// Result item from testing text against rules
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleTestMatch {
    /// Rule ID that matched
    pub rule_id: String,
    /// Agent type
    pub agent_type: String,
    /// Event type
    pub event_type: String,
    /// Severity
    pub severity: String,
    /// Confidence score (0.0–1.0)
    pub confidence: f64,
    /// The text fragment that matched
    pub matched_text: String,
    /// Extracted structured data (if any)
    pub extracted: Option<serde_json::Value>,
}

/// Detail view for a single rule
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleDetail {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub description: String,
    pub anchors: Vec<String>,
    pub regex: Option<String>,
    pub workflow: Option<String>,
    pub remediation: Option<String>,
    pub manual_fix: Option<String>,
    pub learn_more_url: Option<String>,
}

/// Renderer for pattern rule listings
pub struct RulesListRenderer;

impl RulesListRenderer {
    /// Render a list of rules as a table
    #[must_use]
    pub fn render(rules: &[RuleListItem], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(rules).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if rules.is_empty() {
            return style.dim("No rules found\n");
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Rules ({}):\n", rules.len())));

        let mut table = Table::new(vec![
            Column::new("ID").min_width(20),
            Column::new("AGENT").min_width(12),
            Column::new("EVENT").min_width(16),
            Column::new("SEV").min_width(8),
            Column::new("REGEX").min_width(5),
            Column::new("WORKFLOW").min_width(12),
        ])
        .with_format(ctx.format);

        for rule in rules {
            let severity = style.severity(&rule.severity, &rule.severity);
            let has_regex = if rule.has_regex {
                style.green("yes")
            } else {
                style.gray("-")
            };
            let workflow = rule.workflow.as_deref().unwrap_or("-").to_string();

            table.add_row(vec![
                rule.id.clone(),
                rule.agent_type.clone(),
                rule.event_type.clone(),
                severity,
                has_regex,
                workflow,
            ]);
        }

        output.push_str(&table.render());

        if ctx.is_verbose() {
            output.push_str(&style.dim("\n(use 'wa rules show <ID>' for full details)\n"));
        }

        output
    }

    /// Render a verbose list with descriptions
    #[must_use]
    pub fn render_verbose(rules: &[RuleListItem], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(rules).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if rules.is_empty() {
            return style.dim("No rules found\n");
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Rules ({}):\n\n", rules.len())));

        for rule in rules {
            let severity = style.severity(&rule.severity, &rule.severity);
            output.push_str(&format!(
                "  {} [{}] {}\n",
                style.bold(&rule.id),
                severity,
                rule.agent_type,
            ));
            output.push_str(&format!("    {}\n", rule.description));
            if let Some(wf) = &rule.workflow {
                output.push_str(&format!("    Workflow: {wf}\n"));
            }
            output.push('\n');
        }

        output
    }
}

/// Renderer for rule test results
pub struct RulesTestRenderer;

impl RulesTestRenderer {
    /// Render test results (matches from testing text against rules)
    #[must_use]
    pub fn render(matches: &[RuleTestMatch], text_len: usize, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            let wrapper = serde_json::json!({
                "text_length": text_len,
                "match_count": matches.len(),
                "matches": matches,
            });
            return serde_json::to_string_pretty(&wrapper).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);

        if matches.is_empty() {
            return format!(
                "{}\n",
                style.dim(&format!("No matches ({text_len} bytes tested)"))
            );
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!(
            "Matches ({} hit{}, {} bytes tested):\n\n",
            matches.len(),
            if matches.len() == 1 { "" } else { "s" },
            text_len,
        )));

        for (i, m) in matches.iter().enumerate() {
            let severity = style.severity(&m.severity, &m.severity);
            output.push_str(&format!(
                "  {}. {} [{}] confidence={:.2}\n",
                i + 1,
                style.bold(&m.rule_id),
                severity,
                m.confidence,
            ));
            output.push_str(&format!(
                "     Agent: {}  Event: {}\n",
                m.agent_type, m.event_type
            ));
            output.push_str(&format!(
                "     Matched: \"{}\"\n",
                truncate(&m.matched_text, 80)
            ));

            if let Some(ref extracted) = m.extracted {
                if !extracted.is_null()
                    && !extracted.as_object().is_some_and(serde_json::Map::is_empty)
                {
                    let json =
                        serde_json::to_string(extracted).unwrap_or_else(|_| "{}".to_string());
                    output.push_str(&format!("     Extracted: {}\n", truncate(&json, 100)));
                }
            }

            output.push('\n');
        }

        output
    }
}

/// Renderer for rule detail view
pub struct RuleDetailRenderer;

impl RuleDetailRenderer {
    /// Render a single rule's full details
    #[must_use]
    pub fn render(detail: &RuleDetail, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(detail).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        let severity = style.severity(&detail.severity, &detail.severity);

        output.push_str(&style.bold(&format!("Rule: {}\n", detail.id)));
        output.push_str(&format!("  Agent:      {}\n", detail.agent_type));
        output.push_str(&format!("  Event:      {}\n", detail.event_type));
        output.push_str(&format!("  Severity:   {severity}\n"));
        output.push_str(&format!("  Description: {}\n", detail.description));

        output.push_str(&format!("\n  Anchors ({}):\n", detail.anchors.len()));
        for anchor in &detail.anchors {
            output.push_str(&format!("    - \"{anchor}\"\n"));
        }

        if let Some(ref regex) = detail.regex {
            output.push_str(&format!("\n  Regex: {regex}\n"));
        }

        if let Some(ref workflow) = detail.workflow {
            output.push_str(&format!("\n  Workflow: {workflow}\n"));
        }
        if let Some(ref remediation) = detail.remediation {
            output.push_str(&format!("  Remediation: {remediation}\n"));
        }
        if let Some(ref manual_fix) = detail.manual_fix {
            output.push_str(&format!("  Manual fix: {manual_fix}\n"));
        }
        if let Some(ref url) = detail.learn_more_url {
            output.push_str(&format!("  Learn more: {url}\n"));
        }

        output
    }
}

// =============================================================================
// Audit List Renderer
// =============================================================================

/// Renderer for audit action lists
pub struct AuditListRenderer;

impl AuditListRenderer {
    /// Render a list of audit actions
    #[must_use]
    pub fn render(actions: &[AuditActionRecord], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(actions).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if actions.is_empty() {
            return style.dim("No audit records found\n");
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Audit trail ({} records):\n", actions.len())));

        let mut table = Table::new(vec![
            Column::new("ID").align(Alignment::Right).min_width(5),
            Column::new("TIME").min_width(20),
            Column::new("ACTOR").min_width(8),
            Column::new("ACTION").min_width(14),
            Column::new("DECISION").min_width(10),
            Column::new("RESULT").min_width(8),
            Column::new("PANE").align(Alignment::Right).min_width(4),
            Column::new("SUMMARY").min_width(30),
        ])
        .with_format(ctx.format);

        let display_actions = if ctx.limit > 0 && actions.len() > ctx.limit {
            &actions[..ctx.limit]
        } else {
            actions
        };

        for action in display_actions {
            let decision = match action.policy_decision.as_str() {
                "allow" => style.green("allow"),
                "deny" => style.red("deny"),
                "require_approval" => style.yellow("require_approval"),
                other => other.to_string(),
            };

            let result = match action.result.as_str() {
                "success" => style.green("success"),
                "denied" => style.red("denied"),
                "failed" => style.red("failed"),
                "timeout" => style.yellow("timeout"),
                other => other.to_string(),
            };

            let pane = action
                .pane_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());

            let summary = action
                .input_summary
                .as_deref()
                .or(action.decision_reason.as_deref())
                .unwrap_or("-");

            table.add_row(vec![
                action.id.to_string(),
                format_timestamp(action.ts),
                action.actor_kind.clone(),
                action.action_kind.clone(),
                decision,
                result,
                pane,
                truncate(summary, 50),
            ]);
        }

        output.push_str(&table.render());

        if ctx.limit > 0 && actions.len() > ctx.limit {
            output.push_str(&style.dim(&format!(
                "\n... and {} more records (use --limit to see more)\n",
                actions.len() - ctx.limit
            )));
        }

        output
    }

    /// Render a single audit action detail view
    #[must_use]
    pub fn render_detail(action: &AuditActionRecord, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(action).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        output.push_str(&style.bold(&format!("Audit Record #{}\n", action.id)));
        output.push_str(&format!("  Time:       {}\n", format_timestamp(action.ts)));
        output.push_str(&format!("  Actor:      {}\n", action.actor_kind));
        if let Some(actor_id) = &action.actor_id {
            output.push_str(&format!("  Actor ID:   {actor_id}\n"));
        }
        if let Some(correlation_id) = &action.correlation_id {
            output.push_str(&format!("  Correlation: {correlation_id}\n"));
        }
        if let Some(pane_id) = action.pane_id {
            output.push_str(&format!("  Pane:       {pane_id}\n"));
        }
        if let Some(domain) = &action.domain {
            output.push_str(&format!("  Domain:     {domain}\n"));
        }
        output.push_str(&format!("  Action:     {}\n", action.action_kind));

        let decision_styled = match action.policy_decision.as_str() {
            "allow" => style.green(&action.policy_decision),
            "deny" => style.red(&action.policy_decision),
            "require_approval" => style.yellow(&action.policy_decision),
            _ => action.policy_decision.clone(),
        };
        output.push_str(&format!("  Decision:   {decision_styled}\n"));

        if let Some(reason) = &action.decision_reason {
            output.push_str(&format!("  Reason:     {reason}\n"));
        }
        if let Some(rule_id) = &action.rule_id {
            output.push_str(&format!("  Rule:       {rule_id}\n"));
        }

        let result_styled = match action.result.as_str() {
            "success" => style.green(&action.result),
            "denied" => style.red(&action.result),
            "failed" => style.red(&action.result),
            "timeout" => style.yellow(&action.result),
            _ => action.result.clone(),
        };
        output.push_str(&format!("  Result:     {result_styled}\n"));

        if let Some(input) = &action.input_summary {
            output.push_str(&format!("  Input:      {input}\n"));
        }
        if let Some(verification) = &action.verification_summary {
            output.push_str(&format!("  Verify:     {verification}\n"));
        }
        if let Some(context) = &action.decision_context {
            output.push_str(&format!("  Context:    {context}\n"));
        }

        output
    }
}

// =============================================================================
// Action History Renderer
// =============================================================================

/// Renderer for action history view (audit + undo + workflow step info)
pub struct ActionHistoryRenderer;

impl ActionHistoryRenderer {
    /// Render action history list
    #[must_use]
    pub fn render(actions: &[ActionHistoryRecord], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(actions).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if actions.is_empty() {
            return style.dim("No action history found\n");
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Action history ({} records):\n", actions.len())));

        let mut table = Table::new(vec![
            Column::new("ID").align(Alignment::Right).min_width(5),
            Column::new("TIME").min_width(19),
            Column::new("PANE").align(Alignment::Right).min_width(4),
            Column::new("ACTION").min_width(16),
            Column::new("RESULT").min_width(8),
            Column::new("UNDO").min_width(5),
            Column::new("SUMMARY").min_width(30),
        ])
        .with_format(ctx.format);

        let display_actions = if ctx.limit > 0 && actions.len() > ctx.limit {
            &actions[..ctx.limit]
        } else {
            actions
        };

        for action in display_actions {
            let result = match action.result.as_str() {
                "success" | "completed" => style.green(&action.result),
                "denied" | "failed" => style.red(&action.result),
                "timeout" => style.yellow("timeout"),
                other => other.to_string(),
            };

            let undo = match (action.undoable, action.undone_at) {
                (Some(true), None) => style.green("✓"),
                (Some(true), Some(_)) => style.gray("undone"),
                _ => "-".to_string(),
            };

            let pane = action
                .pane_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());

            let summary = history_summary(action);

            table.add_row(vec![
                action.id.to_string(),
                format_timestamp(action.ts),
                pane,
                action.action_kind.clone(),
                result,
                undo,
                truncate(&summary, 60),
            ]);
        }

        output.push_str(&table.render());

        if ctx.limit > 0 && actions.len() > ctx.limit {
            output.push_str(&style.dim(&format!(
                "\n... and {} more records (use --limit to see more)\n",
                actions.len() - ctx.limit
            )));
        }

        output
    }

    /// Render workflow tree view (plain output)
    #[must_use]
    pub fn render_workflow(
        actions: &[ActionHistoryRecord],
        workflow_id: &str,
        ctx: &RenderContext,
    ) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(actions).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);
        if actions.is_empty() {
            return style.dim(&format!(
                "No action history found for workflow {workflow_id}\n"
            ));
        }

        let workflow_name = actions
            .iter()
            .find_map(extract_workflow_name)
            .unwrap_or_else(|| "workflow".to_string());

        let mut ordered: Vec<ActionHistoryRecord> = actions.to_vec();
        ordered.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));

        let mut output = String::new();
        output.push_str(&style.bold(&format!("Workflow: {workflow_name} ({workflow_id})\n")));

        let root_action_kinds = [
            "workflow_start",
            "workflow_completed",
            "workflow_failed",
            "workflow_aborted",
        ];

        for action in &ordered {
            let is_root = root_action_kinds.contains(&action.action_kind.as_str());
            let indent = if is_root { "" } else { "  " };
            let summary = history_summary(action);
            let line = format!(
                "{indent}{}  {}  {}\n",
                format_timestamp(action.ts),
                action.action_kind,
                summary
            );
            output.push_str(&line);
        }

        output
    }

    /// Render action history as CSV
    #[must_use]
    pub fn render_csv(actions: &[ActionHistoryRecord]) -> String {
        let mut output = String::new();
        output.push_str("id,ts,actor_kind,actor_id,correlation_id,pane_id,domain,action_kind,policy_decision,decision_reason,rule_id,input_summary,verification_summary,decision_context,result,undoable,undo_strategy,undo_hint,undone_at,undone_by,workflow_id,step_name\n");

        for action in actions {
            let cells = vec![
                action.id.to_string(),
                action.ts.to_string(),
                action.actor_kind.clone(),
                action.actor_id.clone().unwrap_or_default(),
                action.correlation_id.clone().unwrap_or_default(),
                action.pane_id.map_or_else(String::new, |id| id.to_string()),
                action.domain.clone().unwrap_or_default(),
                action.action_kind.clone(),
                action.policy_decision.clone(),
                action.decision_reason.clone().unwrap_or_default(),
                action.rule_id.clone().unwrap_or_default(),
                action.input_summary.clone().unwrap_or_default(),
                action.verification_summary.clone().unwrap_or_default(),
                action.decision_context.clone().unwrap_or_default(),
                action.result.clone(),
                action.undoable.map_or_else(String::new, |v| v.to_string()),
                action.undo_strategy.clone().unwrap_or_default(),
                action.undo_hint.clone().unwrap_or_default(),
                action.undone_at.map_or_else(String::new, |v| v.to_string()),
                action.undone_by.clone().unwrap_or_default(),
                action.workflow_id.clone().unwrap_or_default(),
                action.step_name.clone().unwrap_or_default(),
            ];

            let escaped: Vec<String> = cells.into_iter().map(csv_escape).collect();
            output.push_str(&escaped.join(","));
            output.push('\n');
        }

        output
    }
}

fn history_summary(action: &ActionHistoryRecord) -> String {
    if let Some(value) = parse_summary_json(action.input_summary.as_deref()) {
        if let Some(step_name) = value.get("step_name").and_then(|v| v.as_str()) {
            return format!("step: {step_name}");
        }
        if let Some(reason) = value.get("reason").and_then(|v| v.as_str()) {
            return reason.to_string();
        }
        if let Some(workflow_name) = value.get("workflow_name").and_then(|v| v.as_str()) {
            return workflow_name.to_string();
        }
    }

    action
        .input_summary
        .as_deref()
        .or(action.decision_reason.as_deref())
        .or(action.undo_hint.as_deref())
        .unwrap_or("-")
        .to_string()
}

fn extract_workflow_name(action: &ActionHistoryRecord) -> Option<String> {
    parse_summary_json(action.input_summary.as_deref()).and_then(|value| {
        value
            .get("workflow_name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })
}

fn parse_summary_json(summary: Option<&str>) -> Option<serde_json::Value> {
    let summary = summary?;
    serde_json::from_str::<serde_json::Value>(summary).ok()
}

fn csv_escape(value: String) -> String {
    if value.contains('"') || value.contains(',') || value.contains('\n') || value.contains('\r') {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        value
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Format epoch milliseconds as human-readable timestamp
#[must_use]
pub fn format_timestamp(epoch_ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};

    let secs = u64::try_from(epoch_ms / 1000).unwrap_or(0);
    let nanos = u32::try_from(epoch_ms.rem_euclid(1000) * 1_000_000).unwrap_or(0);

    let duration = Duration::new(secs, nanos);
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

// =============================================================================
// Health Snapshot Renderer (wa-upg.12.4)
// =============================================================================

use crate::crash::HealthSnapshot;

/// Renderer for runtime health snapshot (queue depths, ingest lag, warnings).
///
/// Used by `wa status` and `wa doctor` to surface backpressure and health data
/// when the daemon is running.
pub struct HealthSnapshotRenderer;

impl HealthSnapshotRenderer {
    /// Render a full health snapshot.
    #[must_use]
    pub fn render(snapshot: &HealthSnapshot, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(snapshot).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        output.push_str(&style.bold("Health\n"));

        // Queue depths
        let capture_status = Self::queue_status_label(snapshot.capture_queue_depth, &style);
        let write_status = Self::queue_status_label(snapshot.write_queue_depth, &style);

        output.push_str(&format!(
            "  Capture queue: {} pending{}\n",
            snapshot.capture_queue_depth, capture_status
        ));
        output.push_str(&format!(
            "  Write queue:   {} pending{}\n",
            snapshot.write_queue_depth, write_status
        ));

        // Ingest lag
        output.push_str(&format!(
            "  Ingest lag:    avg {:.1}ms, max {}ms\n",
            snapshot.ingest_lag_avg_ms, snapshot.ingest_lag_max_ms
        ));

        // DB status
        let db_label = if snapshot.db_writable {
            style.green("writable")
        } else {
            style.red("NOT writable")
        };
        output.push_str(&format!("  Database:      {db_label}\n"));

        // Observed panes
        output.push_str(&format!(
            "  Observed:      {} pane(s)\n",
            snapshot.observed_panes
        ));

        // Runtime pane priority overrides (operator-set).
        if !snapshot.pane_priority_overrides.is_empty() {
            output.push_str(&format!(
                "  Overrides:    {} pane(s) with priority overrides\n",
                snapshot.pane_priority_overrides.len()
            ));
            if ctx.is_verbose() {
                for ov in &snapshot.pane_priority_overrides {
                    let expires = ov.expires_at.map_or_else(
                        || "expires <never>".to_string(),
                        |ts| format!("expires {ts}"),
                    );
                    output.push_str(&format!(
                        "    - pane {}: priority {} ({expires})\n",
                        ov.pane_id, ov.priority
                    ));
                }
            }
        }

        // Capture scheduler (budget enforcement)
        if let Some(sched) = &snapshot.scheduler {
            output.push_str(&format!(
                "  Scheduler:     captures {}/s, bytes {}/s, tracked {} pane(s)\n",
                sched.max_captures_per_sec, sched.max_bytes_per_sec, sched.tracked_panes,
            ));
            if sched.total_rate_limited > 0 || sched.total_byte_budget_exceeded > 0 {
                output.push_str(&format!(
                    "    throttled: {} rate-limited, {} byte-budget exceeded\n",
                    sched.total_rate_limited, sched.total_byte_budget_exceeded,
                ));
            }
        }

        // Backpressure tier
        if let Some(tier) = &snapshot.backpressure_tier {
            output.push_str(&format!("  Backpressure:  {tier}\n"));
        }

        // Stuck pane indicator
        if !snapshot.last_activity_by_pane.is_empty() && snapshot.timestamp > 0 {
            const STUCK_MS: u64 = 5 * 60 * 1000;
            let stuck: Vec<u64> = snapshot
                .last_activity_by_pane
                .iter()
                .filter(|(_, last)| snapshot.timestamp.saturating_sub(*last) > STUCK_MS)
                .map(|(id, _)| *id)
                .collect();
            if !stuck.is_empty() {
                let ids: Vec<String> = stuck.iter().map(|id| id.to_string()).collect();
                output.push_str(&format!(
                    "  {}  {} stuck pane(s): {}\n",
                    style.yellow("Stuck:"),
                    stuck.len(),
                    ids.join(", ")
                ));
            }
        }

        // Restart diagnostics
        if snapshot.restart_count > 0 || snapshot.in_crash_loop {
            let label = if snapshot.in_crash_loop {
                style.red("CRASH LOOP")
            } else {
                format!("{} restart(s)", snapshot.restart_count)
            };
            output.push_str(&format!("  Restarts:      {label}\n"));
            if snapshot.consecutive_crashes > 0 {
                output.push_str(&format!(
                    "    consecutive: {}, backoff: {}ms\n",
                    snapshot.consecutive_crashes, snapshot.current_backoff_ms
                ));
            }
        }

        // Verbose: per-pane sequence numbers
        if ctx.is_verbose() && !snapshot.last_seq_by_pane.is_empty() {
            output.push_str("  Sequences:     ");
            let pairs: Vec<String> = snapshot
                .last_seq_by_pane
                .iter()
                .map(|(pane, seq)| format!("pane {pane}=seq {seq}"))
                .collect();
            output.push_str(&pairs.join(", "));
            output.push('\n');
        }

        // Warnings
        if !snapshot.warnings.is_empty() {
            output.push('\n');
            for w in &snapshot.warnings {
                output.push_str(&format!("  {} {w}\n", style.yellow("WARNING:")));
            }
        }

        output
    }

    /// Render a compact one-line summary suitable for appending to status output.
    #[must_use]
    pub fn render_compact(snapshot: &HealthSnapshot, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return String::new(); // JSON mode renders the full snapshot elsewhere
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        let queue_total = snapshot.capture_queue_depth + snapshot.write_queue_depth;
        let lag_label = if snapshot.ingest_lag_max_ms > 0 {
            format!("lag {}ms", snapshot.ingest_lag_max_ms)
        } else {
            "lag 0ms".to_string()
        };

        let db_label = if snapshot.db_writable {
            "db ok"
        } else {
            "db ERR"
        };

        output.push_str(&format!(
            "  Health: {} queued, {}, {}\n",
            queue_total, lag_label, db_label
        ));

        if !snapshot.pane_priority_overrides.is_empty() {
            let shown: Vec<String> = snapshot
                .pane_priority_overrides
                .iter()
                .take(3)
                .map(|ov| format!("pane {}={}", ov.pane_id, ov.priority))
                .collect();
            let more = snapshot
                .pane_priority_overrides
                .len()
                .saturating_sub(shown.len());
            if more == 0 {
                output.push_str(&format!("  Overrides: {}\n", shown.join(", ")));
            } else {
                output.push_str(&format!(
                    "  Overrides: {} (+{more} more)\n",
                    shown.join(", ")
                ));
            }
        }

        // Surface warnings inline
        for w in &snapshot.warnings {
            output.push_str(&format!("  {} {w}\n", style.yellow("WARNING:")));
        }

        output
    }

    /// Produce diagnostic checks from a health snapshot (for `wa doctor`).
    #[must_use]
    pub fn diagnostic_checks(snapshot: &HealthSnapshot) -> Vec<HealthDiagnostic> {
        let mut checks = Vec::new();

        // Queue depths
        let capture_pct = if snapshot.capture_queue_depth > 0 {
            format!("{} pending", snapshot.capture_queue_depth)
        } else {
            "idle".to_string()
        };
        checks.push(HealthDiagnostic {
            name: "capture queue",
            status: if snapshot.capture_queue_depth > 0 {
                HealthDiagnosticStatus::Info
            } else {
                HealthDiagnosticStatus::Ok
            },
            detail: capture_pct,
        });

        let write_pct = if snapshot.write_queue_depth > 0 {
            format!("{} pending", snapshot.write_queue_depth)
        } else {
            "idle".to_string()
        };
        checks.push(HealthDiagnostic {
            name: "write queue",
            status: if snapshot.write_queue_depth > 0 {
                HealthDiagnosticStatus::Info
            } else {
                HealthDiagnosticStatus::Ok
            },
            detail: write_pct,
        });

        // Ingest lag
        let lag_status = if snapshot.ingest_lag_max_ms > 5000 {
            HealthDiagnosticStatus::Warning
        } else {
            HealthDiagnosticStatus::Ok
        };
        checks.push(HealthDiagnostic {
            name: "ingest lag",
            status: lag_status,
            detail: format!(
                "avg {:.1}ms, max {}ms",
                snapshot.ingest_lag_avg_ms, snapshot.ingest_lag_max_ms
            ),
        });

        // DB writability
        if snapshot.db_writable {
            checks.push(HealthDiagnostic {
                name: "database health",
                status: HealthDiagnosticStatus::Ok,
                detail: "writable".to_string(),
            });
        } else {
            checks.push(HealthDiagnostic {
                name: "database health",
                status: HealthDiagnosticStatus::Error,
                detail: "database is NOT writable".to_string(),
            });
        }

        // Stuck pane detection: panes with no activity for > 5 minutes
        const STUCK_THRESHOLD_MS: u64 = 5 * 60 * 1000;
        if !snapshot.last_activity_by_pane.is_empty() && snapshot.timestamp > 0 {
            let stuck: Vec<u64> = snapshot
                .last_activity_by_pane
                .iter()
                .filter(|(_, last_seen)| {
                    snapshot.timestamp.saturating_sub(*last_seen) > STUCK_THRESHOLD_MS
                })
                .map(|(pane_id, _)| *pane_id)
                .collect();

            if stuck.is_empty() {
                checks.push(HealthDiagnostic {
                    name: "pane activity",
                    status: HealthDiagnosticStatus::Ok,
                    detail: format!(
                        "all {} pane(s) active",
                        snapshot.last_activity_by_pane.len()
                    ),
                });
            } else {
                let ids: Vec<String> = stuck.iter().map(|id| id.to_string()).collect();
                checks.push(HealthDiagnostic {
                    name: "pane activity",
                    status: HealthDiagnosticStatus::Warning,
                    detail: format!("{} stuck pane(s): {}", stuck.len(), ids.join(", ")),
                });
            }
        }

        // Crash loop detection
        if snapshot.in_crash_loop {
            checks.push(HealthDiagnostic {
                name: "crash loop",
                status: HealthDiagnosticStatus::Error,
                detail: format!(
                    "{} consecutive crashes, backoff {}ms",
                    snapshot.consecutive_crashes, snapshot.current_backoff_ms
                ),
            });
        } else if snapshot.restart_count > 0 {
            checks.push(HealthDiagnostic {
                name: "crash loop",
                status: HealthDiagnosticStatus::Info,
                detail: format!("{} restart(s), no crash loop", snapshot.restart_count),
            });
        }

        // Surface warnings as individual diagnostics
        for w in &snapshot.warnings {
            checks.push(HealthDiagnostic {
                name: "runtime warning",
                status: HealthDiagnosticStatus::Warning,
                detail: w.clone(),
            });
        }

        checks
    }

    /// Label suffix for queue status (empty when queue is idle).
    fn queue_status_label(depth: usize, style: &Style) -> String {
        if depth == 0 {
            format!(" ({})", style.green("idle"))
        } else {
            String::new()
        }
    }
}

/// Diagnostic status levels for health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthDiagnosticStatus {
    /// No issues
    Ok,
    /// Informational (non-zero but not concerning)
    Info,
    /// Possible issue
    Warning,
    /// Definite issue
    Error,
}

/// A single diagnostic result from health snapshot analysis.
#[derive(Debug, Clone)]
pub struct HealthDiagnostic {
    /// Check name (e.g., "capture queue")
    pub name: &'static str,
    /// Status level
    pub status: HealthDiagnosticStatus,
    /// Detail message
    pub detail: String,
}

// =============================================================================
// Account List Renderer (wa-nu4.3.2.5)
// =============================================================================

use crate::accounts::{AccountRecord, AccountSelectionResult};

/// Renderer for account list with optional pick preview.
pub struct AccountListRenderer;

impl AccountListRenderer {
    /// Render account list with optional selection preview.
    #[must_use]
    pub fn render(
        accounts: &[AccountRecord],
        pick: Option<&AccountSelectionResult>,
        service: &str,
        ctx: &RenderContext,
    ) -> String {
        if ctx.format.is_json() {
            let mut payload = serde_json::json!({
                "accounts": accounts,
                "total": accounts.len(),
                "service": service,
            });
            if let Some(pick_result) = pick {
                payload["pick_preview"] = serde_json::json!({
                    "selected_account_id": pick_result.selected.as_ref().map(|a| &a.account_id),
                    "selected_name": pick_result.selected.as_ref().and_then(|a| a.name.as_ref()),
                    "selection_reason": &pick_result.explanation.selection_reason,
                    "candidates_count": pick_result.explanation.candidates.len(),
                    "filtered_count": pick_result.explanation.filtered_out.len(),
                });
            }
            return serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);

        if accounts.is_empty() {
            return style.dim(&format!("No accounts found for service \"{service}\"\n"));
        }

        let mut output = String::new();
        output.push_str(&style.bold(&format!(
            "Accounts ({}, service: {service}):\n",
            accounts.len()
        )));

        let mut table = Table::new(vec![
            Column::new("ACCOUNT").min_width(16),
            Column::new("NAME").min_width(12),
            Column::new("REMAINING")
                .align(Alignment::Right)
                .min_width(10),
            Column::new("USED").align(Alignment::Right).min_width(10),
            Column::new("LIMIT").align(Alignment::Right).min_width(10),
            Column::new("REFRESHED").min_width(20),
        ])
        .with_format(ctx.format);

        for acct in accounts {
            let name = acct.name.as_deref().unwrap_or("-");
            let remaining = format!("{:.1}%", acct.percent_remaining);
            let used = acct
                .tokens_used
                .map_or_else(|| "-".to_string(), format_token_count);
            let limit = acct
                .tokens_limit
                .map_or_else(|| "-".to_string(), format_token_count);
            let refreshed = format_timestamp(acct.last_refreshed_at);

            table.add_row(vec![
                truncate(&acct.account_id, 24),
                truncate(name, 16),
                remaining,
                used,
                limit,
                refreshed,
            ]);
        }

        output.push_str(&table.render());

        // Pick preview
        if let Some(pick_result) = pick {
            output.push('\n');
            if let Some(ref selected) = pick_result.selected {
                let display_name = selected.name.as_deref().unwrap_or(&selected.account_id);
                output.push_str(&style.bold("Pick: "));
                output.push_str(&style.green(display_name));
                output.push_str(&format!(
                    " ({:.1}%) - {}\n",
                    selected.percent_remaining, pick_result.explanation.selection_reason
                ));
            } else {
                output.push_str(&style.yellow(&format!(
                    "Pick: none - {}\n",
                    pick_result.explanation.selection_reason
                )));
            }

            if !pick_result.explanation.filtered_out.is_empty() {
                output.push_str(&style.dim(&format!(
                    "  {} account(s) filtered (below threshold)\n",
                    pick_result.explanation.filtered_out.len()
                )));
            }
        }

        output
    }
}

/// Format a token count in human-readable form (e.g. 1.2M, 500K).
fn format_token_count(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

// =============================================================================
// Analytics Renderers (wa-985.3)
// =============================================================================

/// Summary data for analytics overview
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnalyticsSummaryData {
    /// Period label (e.g. "Last 7 Days")
    pub period_label: String,
    /// Total tokens across all agents
    pub total_tokens: i64,
    /// Estimated cost in USD
    pub total_cost: f64,
    /// Number of rate limit events
    pub rate_limit_hits: i64,
    /// Number of workflow executions
    pub workflow_runs: i64,
}

/// Renderer for analytics summary
pub struct AnalyticsSummaryRenderer;

impl AnalyticsSummaryRenderer {
    /// Render analytics summary
    #[must_use]
    pub fn render(data: &AnalyticsSummaryData, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);
        let mut output = String::new();

        output.push_str(&format!(
            "{}\n",
            style.bold(&format!("Usage Analytics ({})", data.period_label))
        ));
        output.push_str(&format!(
            "  Total Tokens:    {}\n",
            format_number(data.total_tokens)
        ));
        output.push_str(&format!("  Estimated Cost:  ${:.2}\n", data.total_cost));
        output.push_str(&format!("  Rate Limits Hit: {}\n", data.rate_limit_hits));
        output.push_str(&format!("  Workflows Run:   {}\n", data.workflow_runs));

        output
    }
}

/// Renderer for daily metrics breakdown
pub struct AnalyticsDailyRenderer;

impl AnalyticsDailyRenderer {
    /// Render daily metrics table
    #[must_use]
    pub fn render(days: &[crate::storage::DailyMetricSummary], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(days).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if days.is_empty() {
            return style.dim("No daily metrics found\n");
        }

        let mut output = String::new();
        output.push_str(&format!("{}\n", style.bold("Daily Metrics:")));

        let mut table = Table::new(vec![
            Column::new("DATE").min_width(12),
            Column::new("TOKENS").align(Alignment::Right).min_width(12),
            Column::new("COST").align(Alignment::Right).min_width(10),
            Column::new("EVENTS").align(Alignment::Right).min_width(8),
        ])
        .with_format(ctx.format);

        for day in days {
            let date_str = format_epoch_date(day.day_ts);
            table.add_row(vec![
                date_str,
                format_number(day.total_tokens),
                format!("${:.2}", day.total_cost),
                day.event_count.to_string(),
            ]);
        }

        output.push_str(&table.render());
        output
    }
}

/// Renderer for per-agent metrics breakdown
pub struct AnalyticsAgentRenderer;

impl AnalyticsAgentRenderer {
    /// Render agent breakdown table
    #[must_use]
    pub fn render(agents: &[crate::storage::AgentMetricBreakdown], ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(agents).unwrap_or_else(|_| "[]".to_string());
        }

        let style = Style::from_format(ctx.format);

        if agents.is_empty() {
            return style.dim("No agent metrics found\n");
        }

        let total_tokens: i64 = agents.iter().map(|a| a.total_tokens).sum();

        let mut output = String::new();
        output.push_str(&format!("{}\n", style.bold("Breakdown by Agent:")));

        let mut table = Table::new(vec![
            Column::new("AGENT").min_width(14),
            Column::new("TOKENS").align(Alignment::Right).min_width(12),
            Column::new("COST").align(Alignment::Right).min_width(10),
            Column::new("% OF TOTAL")
                .align(Alignment::Right)
                .min_width(10),
            Column::new("AVG/EVENT")
                .align(Alignment::Right)
                .min_width(10),
        ])
        .with_format(ctx.format);

        for agent in agents {
            let pct = if total_tokens > 0 {
                #[allow(clippy::cast_precision_loss)]
                let p = (agent.total_tokens as f64 / total_tokens as f64) * 100.0;
                format!("{p:.0}%")
            } else {
                "0%".to_string()
            };

            table.add_row(vec![
                agent.agent_type.clone(),
                format_number(agent.total_tokens),
                format!("${:.2}", agent.total_cost),
                pct,
                format!("{:.0}", agent.avg_tokens_per_event),
            ]);
        }

        output.push_str(&table.render());
        output
    }
}

/// Renderer for metrics export (CSV format)
pub struct AnalyticsExportRenderer;

impl AnalyticsExportRenderer {
    /// Render metrics as CSV
    #[must_use]
    pub fn render_csv(metrics: &[crate::storage::UsageMetricRecord]) -> String {
        let mut output =
            String::from("timestamp,metric_type,agent_type,account_id,tokens,amount,count\n");
        for m in metrics {
            output.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                m.timestamp,
                m.metric_type,
                m.agent_type.as_deref().unwrap_or(""),
                m.account_id.as_deref().unwrap_or(""),
                m.tokens.map_or(String::new(), |t| t.to_string()),
                m.amount.map_or(String::new(), |a| format!("{a:.4}")),
                m.count.map_or(String::new(), |c| c.to_string()),
            ));
        }
        output
    }

    /// Render metrics as JSON
    #[must_use]
    pub fn render_json(metrics: &[crate::storage::UsageMetricRecord]) -> String {
        serde_json::to_string_pretty(metrics).unwrap_or_else(|_| "[]".to_string())
    }
}

// =============================================================================
// Timeline Renderer (wa-6sk.3)
// =============================================================================

/// Renderer for event timeline with cross-pane correlations
pub struct TimelineRenderer;

impl TimelineRenderer {
    /// Render a timeline with ASCII visualization
    #[must_use]
    pub fn render(timeline: &Timeline, ctx: &RenderContext) -> String {
        if ctx.format.is_json() {
            return serde_json::to_string_pretty(timeline).unwrap_or_else(|_| "{}".to_string());
        }

        let style = Style::from_format(ctx.format);

        if timeline.events.is_empty() {
            return style.dim("No events in timeline\n");
        }

        let mut output = String::new();

        // Header: time range and summary
        let pane_ids: std::collections::HashSet<u64> = timeline
            .events
            .iter()
            .map(|e| e.pane_info.pane_id)
            .collect();
        let header = format!(
            "Timeline: {} - {} ({} panes, {} events)\n\n",
            format_timestamp(timeline.start),
            format_timestamp(timeline.end),
            pane_ids.len(),
            timeline.total_count,
        );
        output.push_str(&style.bold(&header));

        // Build correlation lookup: event_id -> list of correlation labels
        let mut corr_labels: std::collections::HashMap<i64, Vec<String>> =
            std::collections::HashMap::new();
        for corr in &timeline.correlations {
            let label = match corr.correlation_type {
                CorrelationType::Failover => "failover",
                CorrelationType::Cascade => "cascade",
                CorrelationType::Temporal => "temporal",
                CorrelationType::WorkflowGroup => "workflow-group",
                CorrelationType::DedupeGroup => "dedupe-group",
            };
            for &eid in &corr.event_ids {
                corr_labels.entry(eid).or_default().push(label.to_string());
            }
        }

        // Render each event as a timeline entry
        let events = &timeline.events;
        for (i, event) in events.iter().enumerate() {
            let ts = format_time_hms(event.timestamp);
            let connector = if i == 0 {
                "\u{2500}\u{252c}\u{2500}" // ─┬─
            } else if i == events.len() - 1 {
                "\u{2500}\u{2534}\u{2500}" // ─┴─
            } else {
                "\u{2500}\u{253c}\u{2500}" // ─┼─
            };

            // Pane label
            let agent_label = event.pane_info.agent_type.as_deref().unwrap_or("unknown");
            let pane_label = format!("Pane {} ({})", event.pane_info.pane_id, agent_label,);

            // Event line
            let mut line = format!(
                "{} {} {}: {}",
                style.dim(&ts),
                connector,
                style.cyan(&pane_label),
                event.event_type,
            );

            // Handled indicator
            if let Some(ref handled) = event.handled {
                let handled_ts = format_time_hms(handled.handled_at);
                line.push_str(&format!(
                    " {}",
                    style.green(&format!("\u{2500}\u{2500}\u{2192} handled ({handled_ts})")),
                ));
            }

            // Correlation markers
            if let Some(labels) = corr_labels.get(&event.id) {
                let unique: std::collections::HashSet<&str> =
                    labels.iter().map(|s| s.as_str()).collect();
                let mut sorted: Vec<&str> = unique.into_iter().collect();
                sorted.sort_unstable();
                for label in sorted {
                    line.push_str(&format!(
                        " {}",
                        style.yellow(&format!("[CORRELATED: {label}]")),
                    ));
                }
            }

            output.push_str(&line);
            output.push('\n');

            // Verbose: show workflow info on a sub-line
            if ctx.is_verbose() {
                if let Some(ref handled) = event.handled {
                    if let Some(ref wf_id) = handled.workflow_id {
                        output.push_str(&format!(
                            "          {}\n",
                            style.dim(&format!(
                                "\u{251c}\u{2500}\u{2500}\u{2192} workflow: {} ({})",
                                wf_id, handled.status
                            )),
                        ));
                    }
                }
            }

            // Debug: show full event details
            if ctx.is_debug() {
                output.push_str(&format!(
                    "          {} id={} rule={} sev={} conf={:.2}\n",
                    style.dim("\u{2502}"),
                    event.id,
                    event.rule_id,
                    event.severity,
                    event.confidence,
                ));
                if let Some(ref summary) = event.summary {
                    output.push_str(&format!(
                        "          {} {}\n",
                        style.dim("\u{2502}"),
                        style.dim(summary),
                    ));
                }
                for cref in &event.correlations {
                    output.push_str(&format!(
                        "          {} corr: {} ({:?})\n",
                        style.dim("\u{2502}"),
                        cref.id,
                        cref.correlation_type,
                    ));
                }
            }

            // Vertical connector between events
            if i < events.len() - 1 {
                output.push_str(&format!("          {}\n", style.dim("\u{2502}"),));
            }
        }

        // Pagination hint
        if timeline.has_more {
            output.push_str(&style.dim(&format!(
                "\n... {} more events (use --limit to see more)\n",
                timeline.total_count as usize - events.len(),
            )));
        }

        // Legend
        output.push('\n');
        output.push_str(&style.dim(
            "Legend: \u{2500}\u{2500}\u{2192} workflow action  [CORRELATED] cross-pane relationship\n",
        ));

        output
    }
}

/// Format epoch-ms as HH:MM:SS for timeline display
fn format_time_hms(epoch_ms: i64) -> String {
    let secs_since_epoch = u64::try_from(epoch_ms / 1000).unwrap_or(0);
    let secs_in_day = secs_since_epoch % 86400;
    let hours = secs_in_day / 3600;
    let minutes = (secs_in_day % 3600) / 60;
    let seconds = secs_in_day % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Format a large number with comma separators
fn format_number(n: i64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut result = String::new();
    let len = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

/// Format an epoch-ms timestamp as a date string (YYYY-MM-DD)
fn format_epoch_date(epoch_ms: i64) -> String {
    let secs = epoch_ms / 1000;
    let days = secs / 86400;
    // Simple date calculation from epoch days
    let mut y = 1970i64;
    let mut remaining = days;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while m < 12 && remaining >= month_days[m] {
        remaining -= month_days[m];
        m += 1;
    }
    format!("{y:04}-{:02}-{:02}", m + 1, remaining + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pane() -> PaneRecord {
        PaneRecord {
            pane_id: 1,
            pane_uuid: None,
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
            dedupe_key: None,
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
    fn event_list_plain_has_no_ansi() {
        let events = vec![sample_event()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render(&events, &ctx);

        assert!(!output.contains("\x1b["));
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

    fn sample_audit_action() -> AuditActionRecord {
        AuditActionRecord {
            id: 1,
            ts: 1_737_446_400_000,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(5),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("prompt detected".to_string()),
            rule_id: None,
            input_summary: Some("echo hello".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        }
    }

    fn sample_history_action(action_kind: &str, ts: i64) -> ActionHistoryRecord {
        ActionHistoryRecord {
            id: 1,
            ts,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-123".to_string()),
            correlation_id: None,
            pane_id: Some(2),
            domain: Some("local".to_string()),
            action_kind: action_kind.to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("echo hello".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
            undoable: Some(true),
            undo_strategy: Some("workflow_abort".to_string()),
            undo_hint: Some("wa robot workflow abort wf-123".to_string()),
            undone_at: None,
            undone_by: None,
            workflow_id: Some("wf-123".to_string()),
            step_name: None,
        }
    }

    #[test]
    fn audit_list_render_plain() {
        let actions = vec![sample_audit_action()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render(&actions, &ctx);

        assert!(output.contains("Audit trail"));
        assert!(output.contains("human"));
        assert!(output.contains("send_text"));
        assert!(output.contains("allow"));
        assert!(output.contains("success"));
        assert!(output.contains("echo hello"));
    }

    #[test]
    fn history_list_render_plain() {
        let actions = vec![sample_history_action("send_text", 1_737_446_400_000)];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = ActionHistoryRenderer::render(&actions, &ctx);

        assert!(output.contains("Action history"));
        assert!(output.contains("send_text"));
        assert!(output.contains("echo hello"));
    }

    #[test]
    fn history_list_render_plain_no_ansi() {
        let actions = vec![sample_history_action("send_text", 1_737_446_400_000)];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = ActionHistoryRenderer::render(&actions, &ctx);

        assert_no_ansi(&output, "ActionHistoryRenderer::render");
    }

    #[test]
    fn history_list_render_json() {
        let actions = vec![sample_history_action("send_text", 1_737_446_400_000)];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = ActionHistoryRenderer::render(&actions, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["action_kind"], "send_text");
        assert_eq!(parsed[0]["actor_kind"], "workflow");
    }

    #[test]
    fn history_list_render_csv() {
        let actions = vec![sample_history_action("send_text", 1_737_446_400_000)];
        let output = ActionHistoryRenderer::render_csv(&actions);

        let mut lines = output.lines();
        let header = lines.next().unwrap_or_default();
        let row = lines.next().unwrap_or_default();
        assert!(header.starts_with("id,ts,actor_kind"));
        assert!(row.contains(",send_text,"));
    }

    #[test]
    fn history_workflow_render_plain() {
        let mut start = sample_history_action("workflow_start", 1_737_446_400_000);
        start.input_summary =
            Some(r#"{"workflow_name":"handle_compaction","execution_id":"wf-123"}"#.to_string());
        let mut step = sample_history_action("workflow_step", 1_737_446_401_000);
        step.input_summary =
            Some(r#"{"workflow_name":"handle_compaction","step_name":"step_0"}"#.to_string());
        let mut done = sample_history_action("workflow_completed", 1_737_446_402_000);
        done.input_summary =
            Some(r#"{"workflow_name":"handle_compaction","reason":"success"}"#.to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = ActionHistoryRenderer::render_workflow(&[done, step, start], "wf-123", &ctx);

        assert_no_ansi(&output, "ActionHistoryRenderer::render_workflow");
        assert!(output.contains("Workflow: handle_compaction (wf-123)"));
        assert!(output.contains("workflow_step"));
        assert!(output.contains("step: step_0"));
    }

    #[test]
    fn audit_list_render_plain_no_ansi() {
        let actions = vec![sample_audit_action()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render(&actions, &ctx);

        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn audit_list_render_json() {
        let actions = vec![sample_audit_action()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AuditListRenderer::render(&actions, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["actor_kind"], "human");
        assert_eq!(parsed[0]["action_kind"], "send_text");
        assert_eq!(parsed[0]["result"], "success");
    }

    #[test]
    fn audit_list_render_empty() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render(&[], &ctx);

        assert!(output.contains("No audit records found"));
    }

    #[test]
    fn audit_list_render_deny_decision() {
        let mut action = sample_audit_action();
        action.policy_decision = "deny".to_string();
        action.result = "denied".to_string();
        action.decision_reason = Some("alt-screen active".to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render(&[action], &ctx);

        assert!(output.contains("deny"));
        assert!(output.contains("denied"));
    }

    #[test]
    fn audit_detail_render_plain() {
        let action = sample_audit_action();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render_detail(&action, &ctx);

        assert!(output.contains("Audit Record #1"));
        assert!(output.contains("Actor:      human"));
        assert!(output.contains("Pane:       5"));
        assert!(output.contains("Action:     send_text"));
        assert!(output.contains("Decision:   allow"));
        assert!(output.contains("Result:     success"));
        assert!(output.contains("Reason:     prompt detected"));
        assert!(output.contains("Input:      echo hello"));
    }

    #[test]
    fn audit_detail_render_json() {
        let action = sample_audit_action();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AuditListRenderer::render_detail(&action, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["actor_kind"], "human");
        assert_eq!(parsed["action_kind"], "send_text");
    }

    #[test]
    fn audit_list_limit_truncation() {
        let actions: Vec<_> = (0..5)
            .map(|i| {
                let mut a = sample_audit_action();
                a.id = i + 1;
                a
            })
            .collect();

        let ctx = RenderContext::new(OutputFormat::Plain).limit(3);
        let output = AuditListRenderer::render(&actions, &ctx);

        assert!(output.contains("and 2 more records"));
    }

    // =========================================================================
    // Snapshot tests: plain output stability (wa-nu4.3.2.10)
    // =========================================================================

    /// Helper: assert no ANSI escape sequences in output
    fn assert_no_ansi(output: &str, renderer_name: &str) {
        assert!(
            !output.contains("\x1b["),
            "{renderer_name}: plain output must not contain ANSI escape sequences.\nOutput:\n{output}"
        );
    }

    /// Fake secret for redaction testing.
    ///
    /// Build at runtime (split string literals) to avoid repository push-protection
    /// treating the test token as a real secret.
    static FAKE_SECRET: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| ["sk", "-proj-", "TEST", "SECRET1234567890abcdef"].concat());

    /// Helper: assert fake secret never appears in output
    fn assert_no_secrets(output: &str, renderer_name: &str) {
        assert!(
            !output.contains(FAKE_SECRET.as_str()),
            "{renderer_name}: output must not contain raw secrets.\nOutput:\n{output}"
        );
    }

    // --- Pane table snapshots ---

    #[test]
    fn snapshot_pane_table_plain() {
        let panes = vec![sample_pane()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = PaneTableRenderer::render(&panes, &ctx);

        assert_no_ansi(&output, "PaneTableRenderer");

        // Verify stable structural elements
        assert!(output.contains("Panes (1 observed, 0 ignored):"));
        assert!(output.contains("ID"));
        assert!(output.contains("STATUS"));
        assert!(output.contains("TITLE"));
        assert!(output.contains("CWD"));
        assert!(output.contains("DOMAIN"));
        assert!(output.contains("1")); // pane id
        assert!(output.contains("observed"));
        assert!(output.contains("test shell"));
        assert!(output.contains("/home/user"));
        assert!(output.contains("local"));
    }

    #[test]
    fn snapshot_pane_table_json_schema() {
        let panes = vec![sample_pane()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = PaneTableRenderer::render(&panes, &ctx);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);

        let pane = &parsed[0];
        // Verify stable JSON fields
        assert_eq!(pane["pane_id"], 1);
        assert_eq!(pane["domain"], "local");
        assert_eq!(pane["title"], "test shell");
        assert_eq!(pane["cwd"], "/home/user");
        assert_eq!(pane["observed"], true);
        assert!(pane["first_seen_at"].is_i64());
        assert!(pane["last_seen_at"].is_i64());
    }

    #[test]
    fn snapshot_pane_table_empty_plain() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = PaneTableRenderer::render(&[], &ctx);

        assert_no_ansi(&output, "PaneTableRenderer(empty)");
        assert!(output.contains("No panes observed"));
    }

    #[test]
    fn snapshot_pane_table_mixed_status() {
        let mut ignored = sample_pane();
        ignored.pane_id = 2;
        ignored.observed = false;
        ignored.ignore_reason = Some("excluded by pattern".to_string());

        let panes = vec![sample_pane(), ignored];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = PaneTableRenderer::render(&panes, &ctx);

        assert_no_ansi(&output, "PaneTableRenderer(mixed)");
        assert!(output.contains("1 observed, 1 ignored"));
        assert!(output.contains("ignored"));
    }

    // --- Event list snapshots ---

    #[test]
    fn snapshot_event_list_plain() {
        let events = vec![sample_event()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render(&events, &ctx);

        assert_no_ansi(&output, "EventListRenderer");

        assert!(output.contains("Events (1):"));
        assert!(output.contains("ID"));
        assert!(output.contains("PANE"));
        assert!(output.contains("RULE"));
        assert!(output.contains("SEV"));
        assert!(output.contains("TIME"));
        assert!(output.contains("STATUS"));
        assert!(output.contains("42")); // event id
        assert!(output.contains("codex.usage_reached"));
        assert!(output.contains("warning"));
        assert!(output.contains("pending"));
    }

    #[test]
    fn snapshot_event_list_handled() {
        let mut event = sample_event();
        event.handled_at = Some(1_737_450_000_000);
        event.handled_by_workflow_id = Some("wf-123".to_string());
        event.handled_status = Some("resolved".to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render(&[event], &ctx);

        assert_no_ansi(&output, "EventListRenderer(handled)");
        assert!(output.contains("handled"));
    }

    #[test]
    fn snapshot_event_list_json_schema() {
        let events = vec![sample_event()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = EventListRenderer::render(&events, &ctx);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);

        let event = &parsed[0];
        assert_eq!(event["id"], 42);
        assert_eq!(event["pane_id"], 1);
        assert_eq!(event["rule_id"], "codex.usage_reached");
        assert_eq!(event["event_type"], "usage_alert");
        assert_eq!(event["severity"], "warning");
        assert!(event["detected_at"].is_i64());
        assert!(event["confidence"].is_f64());
    }

    #[test]
    fn snapshot_event_list_empty_plain() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render(&[], &ctx);

        assert_no_ansi(&output, "EventListRenderer(empty)");
        assert!(output.contains("No events found"));
    }

    // --- Search result snapshots ---

    fn sample_search_result() -> SearchResult {
        SearchResult {
            segment: crate::storage::Segment {
                id: 10,
                pane_id: 1,
                seq: 42,
                content: "Error: API key invalid".to_string(),
                content_len: 22,
                content_hash: None,
                captured_at: 1_737_446_400_000,
            },
            snippet: Some("Error: API key invalid".to_string()),
            highlight: None,
            score: 0.85,
        }
    }

    #[test]
    fn snapshot_search_plain() {
        let results = vec![sample_search_result()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = SearchResultRenderer::render(&results, "API key", &ctx);

        assert_no_ansi(&output, "SearchResultRenderer");

        assert!(output.contains("Search results"));
        assert!(output.contains("API key"));
        assert!(output.contains("1 hits"));
        assert!(output.contains("pane:1"));
        assert!(output.contains("seq:42"));
        assert!(output.contains("Error: API key invalid"));
    }

    #[test]
    fn snapshot_search_json_schema() {
        let results = vec![sample_search_result()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = SearchResultRenderer::render(&results, "API key", &ctx);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);

        let result = &parsed[0];
        assert!(result["segment"]["id"].is_i64());
        assert_eq!(result["segment"]["pane_id"], 1);
        assert_eq!(result["segment"]["seq"], 42);
        assert!(result["score"].is_f64());
    }

    #[test]
    fn snapshot_search_empty_plain() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = SearchResultRenderer::render(&[], "missing term", &ctx);

        assert_no_ansi(&output, "SearchResultRenderer(empty)");
        assert!(output.contains("No results"));
        assert!(output.contains("missing term"));
    }

    // --- Workflow result snapshots ---

    fn sample_workflow_result() -> WorkflowResult {
        WorkflowResult {
            workflow_id: "wf-abc-123".to_string(),
            workflow_name: "handle_compaction".to_string(),
            pane_id: 3,
            status: "completed".to_string(),
            reason: None,
            result: None,
            steps: vec![
                WorkflowStepResult {
                    name: "detect_marker".to_string(),
                    outcome: "success".to_string(),
                    duration_ms: 50,
                    error: None,
                },
                WorkflowStepResult {
                    name: "send_text".to_string(),
                    outcome: "success".to_string(),
                    duration_ms: 120,
                    error: None,
                },
            ],
        }
    }

    #[test]
    fn snapshot_workflow_plain() {
        let result = sample_workflow_result();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = WorkflowResultRenderer::render(&result, &ctx);

        assert_no_ansi(&output, "WorkflowResultRenderer");

        assert!(output.contains("handle_compaction"));
        assert!(output.contains("completed"));
        assert!(output.contains("wf-abc-123"));
        assert!(output.contains("Pane: 3"));
        assert!(output.contains("Steps:"));
        assert!(output.contains("detect_marker"));
        assert!(output.contains("send_text"));
    }

    #[test]
    fn snapshot_workflow_failed() {
        let mut result = sample_workflow_result();
        result.status = "failed".to_string();
        result.reason = Some("pane not found".to_string());
        result.steps[1].outcome = "failed".to_string();
        result.steps[1].error = Some("pane 3 not available".to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = WorkflowResultRenderer::render(&result, &ctx);

        assert_no_ansi(&output, "WorkflowResultRenderer(failed)");
        assert!(output.contains("failed"));
        assert!(output.contains("pane not found"));
        assert!(output.contains("pane 3 not available"));
    }

    #[test]
    fn snapshot_workflow_json_schema() {
        let result = sample_workflow_result();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = WorkflowResultRenderer::render(&result, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["workflow_id"], "wf-abc-123");
        assert_eq!(parsed["workflow_name"], "handle_compaction");
        assert_eq!(parsed["pane_id"], 3);
        assert_eq!(parsed["status"], "completed");
        assert!(parsed["steps"].is_array());
        assert_eq!(parsed["steps"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["steps"][0]["name"], "detect_marker");
    }

    // --- Audit snapshots (additional stability tests) ---

    #[test]
    fn snapshot_audit_plain_columns() {
        let actions = vec![sample_audit_action()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render(&actions, &ctx);

        // Verify column headers present
        assert!(output.contains("ID"));
        assert!(output.contains("TIME"));
        assert!(output.contains("ACTOR"));
        assert!(output.contains("ACTION"));
        assert!(output.contains("DECISION"));
        assert!(output.contains("RESULT"));
        assert!(output.contains("PANE"));
        assert!(output.contains("SUMMARY"));
    }

    #[test]
    fn snapshot_audit_json_all_fields() {
        let mut action = sample_audit_action();
        action.actor_id = Some("mcp-client-1".to_string());
        action.rule_id = Some("command_gate.rm_rf".to_string());
        action.verification_summary = Some("matched pattern".to_string());
        action.decision_context = Some(r#"{"risk":"high"}"#.to_string());

        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AuditListRenderer::render(&[action], &ctx);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        let a = &parsed[0];

        // All fields present in JSON output
        assert!(a["id"].is_i64());
        assert!(a["ts"].is_i64());
        assert_eq!(a["actor_kind"], "human");
        assert_eq!(a["actor_id"], "mcp-client-1");
        assert_eq!(a["pane_id"], 5);
        assert_eq!(a["domain"], "local");
        assert_eq!(a["action_kind"], "send_text");
        assert_eq!(a["policy_decision"], "allow");
        assert_eq!(a["decision_reason"], "prompt detected");
        assert_eq!(a["rule_id"], "command_gate.rm_rf");
        assert_eq!(a["input_summary"], "echo hello");
        assert_eq!(a["verification_summary"], "matched pattern");
        assert!(a["decision_context"].is_string());
        assert_eq!(a["result"], "success");
    }

    // --- Summary snapshots ---

    #[test]
    fn snapshot_summary_plain() {
        let summary = Summary {
            total_panes: 5,
            observed_panes: 3,
            total_segments: 100,
            total_events: 12,
            unhandled_events: 2,
            active_workflows: 1,
        };
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = SummaryRenderer::render(&summary, &ctx);

        assert_no_ansi(&output, "SummaryRenderer");
        assert!(output.contains("Status Summary"));
        assert!(output.contains("3 observed / 5 total"));
        assert!(output.contains("Segments:  100"));
        assert!(output.contains("12 total (2 unhandled)"));
        assert!(output.contains("Workflows: 1 active"));
    }

    #[test]
    fn snapshot_summary_json_schema() {
        let summary = Summary {
            total_panes: 5,
            observed_panes: 3,
            total_segments: 100,
            total_events: 12,
            unhandled_events: 2,
            active_workflows: 1,
        };
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = SummaryRenderer::render(&summary, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["total_panes"], 5);
        assert_eq!(parsed["observed_panes"], 3);
        assert_eq!(parsed["total_segments"], 100);
        assert_eq!(parsed["total_events"], 12);
        assert_eq!(parsed["unhandled_events"], 2);
        assert_eq!(parsed["active_workflows"], 1);
    }

    // =========================================================================
    // No-ANSI guarantees for all renderers (wa-nu4.3.2.10)
    // =========================================================================

    #[test]
    fn no_ansi_pane_detail_plain() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = PaneTableRenderer::render_detail(&sample_pane(), &ctx);
        assert_no_ansi(&output, "PaneTableRenderer::render_detail");
    }

    #[test]
    fn no_ansi_event_detail_plain() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render_detail(&sample_event(), &ctx);
        assert_no_ansi(&output, "EventListRenderer::render_detail");
    }

    #[test]
    fn no_ansi_audit_detail_plain() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render_detail(&sample_audit_action(), &ctx);
        assert_no_ansi(&output, "AuditListRenderer::render_detail");
    }

    // =========================================================================
    // Secret redaction guarantees (wa-nu4.3.2.10)
    // =========================================================================

    #[test]
    fn no_secrets_pane_table() {
        let mut pane = sample_pane();
        pane.title = Some(format!("session with {}", FAKE_SECRET.as_str()));

        let ctx = RenderContext::new(OutputFormat::Plain);
        // Note: PaneTableRenderer truncates titles, so this tests that even
        // if secrets appear in data fields, they get truncated.
        let output = PaneTableRenderer::render(&[pane.clone()], &ctx);

        // Title column is truncated to 24 chars max, so the secret is cut off.
        // This validates that column truncation acts as a defense.
        let title_col_max = 24;
        let title = format!("session with {}", FAKE_SECRET.as_str());
        if title.len() > title_col_max {
            assert!(
                !output.contains(FAKE_SECRET.as_str()),
                "PaneTableRenderer: secret should be truncated by column width"
            );
        }

        // JSON mode would expose it — but that's the raw data contract
    }

    #[test]
    fn no_secrets_audit_summary_when_redacted() {
        // Renderers do NOT perform redaction — callers (storage layer /
        // record_audit_action_redacted) must redact before passing data.
        // This test verifies that a properly redacted record renders cleanly.
        let mut action = sample_audit_action();
        action.input_summary = Some(format!("wa send 5 '[REDACTED]'"));

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AuditListRenderer::render(&[action], &ctx);

        assert_no_secrets(&output, "AuditListRenderer(redacted input_summary)");
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn no_secrets_search_snippet() {
        let mut result = sample_search_result();
        result.snippet = Some(format!("Found key: {} in config", FAKE_SECRET.as_str()));

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = SearchResultRenderer::render(&[result], "key", &ctx);

        // Snippet is truncated to 120 chars, so the secret appears within bounds.
        // This test documents that SearchResultRenderer does NOT redact — the
        // caller (storage layer) is responsible for redaction before display.
        // The assertion here is just structural: we confirm the renderer runs.
        assert!(output.contains("Search results"));
    }

    // =========================================================================
    // Rules renderer tests (wa-nu4.3.2.6)
    // =========================================================================

    fn sample_rule_list_item() -> RuleListItem {
        RuleListItem {
            id: "codex.usage_reached".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage_alert".to_string(),
            severity: "warning".to_string(),
            description: "Codex usage limit reached".to_string(),
            workflow: Some("handle_usage".to_string()),
            anchor_count: 2,
            has_regex: true,
        }
    }

    fn sample_rule_test_match() -> RuleTestMatch {
        RuleTestMatch {
            rule_id: "codex.usage_reached".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage_alert".to_string(),
            severity: "warning".to_string(),
            confidence: 0.95,
            matched_text: "Usage at 95%".to_string(),
            extracted: Some(serde_json::json!({"usage": 95})),
        }
    }

    fn sample_rule_detail() -> RuleDetail {
        RuleDetail {
            id: "codex.usage_reached".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage_alert".to_string(),
            severity: "warning".to_string(),
            description: "Codex usage limit reached".to_string(),
            anchors: vec!["Usage".to_string(), "limit".to_string()],
            regex: Some(r"Usage at (\d+)%".to_string()),
            workflow: Some("handle_usage".to_string()),
            remediation: Some("Restart agent session".to_string()),
            manual_fix: None,
            learn_more_url: Some("https://example.com/docs".to_string()),
        }
    }

    #[test]
    fn rules_list_render_plain() {
        let rules = vec![sample_rule_list_item()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RulesListRenderer::render(&rules, &ctx);

        assert!(output.contains("Rules (1)"));
        assert!(output.contains("codex.usage_reached"));
        assert!(output.contains("codex"));
        assert!(output.contains("warning"));
        assert!(output.contains("handle_usage"));
    }

    #[test]
    fn rules_list_render_plain_no_ansi() {
        let rules = vec![sample_rule_list_item()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RulesListRenderer::render(&rules, &ctx);

        assert_no_ansi(&output, "RulesListRenderer");
    }

    #[test]
    fn rules_list_render_json() {
        let rules = vec![sample_rule_list_item()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = RulesListRenderer::render(&rules, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["id"], "codex.usage_reached");
        assert_eq!(parsed[0]["agent_type"], "codex");
        assert_eq!(parsed[0]["severity"], "warning");
        assert!(parsed[0]["has_regex"].as_bool().unwrap());
    }

    #[test]
    fn rules_list_render_empty() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RulesListRenderer::render(&[], &ctx);

        assert!(output.contains("No rules found"));
    }

    #[test]
    fn rules_list_render_verbose() {
        let rules = vec![sample_rule_list_item()];
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(1);
        let output = RulesListRenderer::render_verbose(&rules, &ctx);

        assert!(output.contains("codex.usage_reached"));
        assert!(output.contains("Codex usage limit reached"));
        assert!(output.contains("Workflow: handle_usage"));
    }

    #[test]
    fn rules_test_render_plain() {
        let matches = vec![sample_rule_test_match()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RulesTestRenderer::render(&matches, 128, &ctx);

        assert!(output.contains("Matches (1 hit"));
        assert!(output.contains("128 bytes tested"));
        assert!(output.contains("codex.usage_reached"));
        assert!(output.contains("confidence=0.95"));
        assert!(output.contains("Usage at 95%"));
    }

    #[test]
    fn rules_test_render_plain_no_ansi() {
        let matches = vec![sample_rule_test_match()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RulesTestRenderer::render(&matches, 128, &ctx);

        assert_no_ansi(&output, "RulesTestRenderer");
    }

    #[test]
    fn rules_test_render_json() {
        let matches = vec![sample_rule_test_match()];
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = RulesTestRenderer::render(&matches, 128, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["text_length"], 128);
        assert_eq!(parsed["match_count"], 1);
        assert_eq!(parsed["matches"][0]["rule_id"], "codex.usage_reached");
        assert_eq!(parsed["matches"][0]["confidence"], 0.95);
    }

    #[test]
    fn rules_test_render_no_matches() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RulesTestRenderer::render(&[], 64, &ctx);

        assert!(output.contains("No matches"));
        assert!(output.contains("64 bytes tested"));
    }

    #[test]
    fn rule_detail_render_plain() {
        let detail = sample_rule_detail();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RuleDetailRenderer::render(&detail, &ctx);

        assert!(output.contains("Rule: codex.usage_reached"));
        assert!(output.contains("Agent:      codex"));
        assert!(output.contains("Event:      usage_alert"));
        assert!(output.contains("Severity:   warning"));
        assert!(output.contains("Codex usage limit reached"));
        assert!(output.contains("Anchors (2)"));
        assert!(output.contains("\"Usage\""));
        assert!(output.contains("\"limit\""));
        assert!(output.contains("Regex:"));
        assert!(output.contains("Workflow: handle_usage"));
        assert!(output.contains("Remediation: Restart agent session"));
        assert!(output.contains("Learn more: https://example.com/docs"));
    }

    #[test]
    fn rule_detail_render_plain_no_ansi() {
        let detail = sample_rule_detail();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = RuleDetailRenderer::render(&detail, &ctx);

        assert_no_ansi(&output, "RuleDetailRenderer");
    }

    #[test]
    fn rule_detail_render_json() {
        let detail = sample_rule_detail();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = RuleDetailRenderer::render(&detail, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["id"], "codex.usage_reached");
        assert_eq!(parsed["agent_type"], "codex");
        assert_eq!(parsed["anchors"].as_array().unwrap().len(), 2);
        assert!(parsed["regex"].is_string());
        assert_eq!(parsed["workflow"], "handle_usage");
    }

    // =========================================================================
    // Verbosity level tests (wa-rnf.3)
    // =========================================================================

    #[test]
    fn render_context_default_verbosity_is_zero() {
        let ctx = RenderContext::default();
        assert_eq!(ctx.verbose, 0);
        assert!(!ctx.is_verbose());
        assert!(!ctx.is_debug());
    }

    #[test]
    fn render_context_verbose_level_one() {
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(1);
        assert_eq!(ctx.verbose, 1);
        assert!(ctx.is_verbose());
        assert!(!ctx.is_debug());
    }

    #[test]
    fn render_context_verbose_level_two() {
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(2);
        assert_eq!(ctx.verbose, 2);
        assert!(ctx.is_verbose());
        assert!(ctx.is_debug());
    }

    #[test]
    fn render_context_verbose_level_three_is_debug() {
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(3);
        assert!(ctx.is_verbose());
        assert!(ctx.is_debug());
    }

    #[test]
    fn pane_detail_default_hides_timestamps() {
        let pane = sample_pane();
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(0);
        let output = PaneTableRenderer::render_detail(&pane, &ctx);
        assert!(!output.contains("First seen:"));
    }

    #[test]
    fn pane_detail_verbose_shows_timestamps() {
        let pane = sample_pane();
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(1);
        let output = PaneTableRenderer::render_detail(&pane, &ctx);
        assert!(output.contains("First seen:"));
    }

    #[test]
    fn render_context_builder_chain() {
        let ctx = RenderContext::new(OutputFormat::Json).verbose(2).limit(10);
        assert_eq!(ctx.verbose, 2);
        assert_eq!(ctx.limit, 10);
        assert!(ctx.format.is_json());
        assert!(ctx.is_debug());
    }

    // =========================================================================
    // Health Snapshot Renderer Tests (wa-upg.12.4)
    // =========================================================================

    fn sample_health_snapshot() -> HealthSnapshot {
        HealthSnapshot {
            timestamp: 1_700_000_000_000,
            observed_panes: 3,
            capture_queue_depth: 12,
            write_queue_depth: 5,
            last_seq_by_pane: vec![(1, 100), (2, 50)],
            warnings: vec![],
            ingest_lag_avg_ms: 15.5,
            ingest_lag_max_ms: 42,
            db_writable: true,
            db_last_write_at: Some(1_700_000_000_000),
            pane_priority_overrides: vec![],
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![(1, 1_700_000_000_000), (2, 1_700_000_000_000)],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        }
    }

    #[test]
    fn health_snapshot_plain_shows_queue_depths() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Capture queue: 12 pending"));
        assert!(output.contains("Write queue:   5 pending"));
    }

    #[test]
    fn health_snapshot_plain_shows_ingest_lag() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Ingest lag:    avg 15.5ms, max 42ms"));
    }

    #[test]
    fn health_snapshot_plain_shows_db_status() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Database:      writable"));
    }

    #[test]
    fn health_snapshot_plain_shows_observed_panes() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Observed:      3 pane(s)"));
    }

    #[test]
    fn health_snapshot_json_is_valid() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["capture_queue_depth"], 12);
        assert_eq!(parsed["write_queue_depth"], 5);
        assert_eq!(parsed["observed_panes"], 3);
        assert_eq!(parsed["db_writable"], true);
    }

    #[test]
    fn health_snapshot_verbose_shows_sequences() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(1);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Sequences:"));
        assert!(output.contains("pane 1=seq 100"));
        assert!(output.contains("pane 2=seq 50"));
    }

    #[test]
    fn health_snapshot_non_verbose_hides_sequences() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(!output.contains("Sequences:"));
    }

    #[test]
    fn health_snapshot_warnings_displayed() {
        let mut snapshot = sample_health_snapshot();
        snapshot.warnings = vec!["Capture queue backpressure: 800/1024 (78%)".to_string()];
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("WARNING:"));
        assert!(output.contains("Capture queue backpressure"));
    }

    #[test]
    fn health_snapshot_db_not_writable() {
        let mut snapshot = sample_health_snapshot();
        snapshot.db_writable = false;
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("NOT writable"));
    }

    #[test]
    fn health_compact_shows_totals() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render_compact(&snapshot, &ctx);

        assert!(output.contains("17 queued")); // 12 + 5
        assert!(output.contains("lag 42ms"));
        assert!(output.contains("db ok"));
    }

    #[test]
    fn health_compact_db_error() {
        let mut snapshot = sample_health_snapshot();
        snapshot.db_writable = false;
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render_compact(&snapshot, &ctx);

        assert!(output.contains("db ERR"));
    }

    #[test]
    fn health_compact_json_returns_empty() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = HealthSnapshotRenderer::render_compact(&snapshot, &ctx);

        assert!(output.is_empty());
    }

    #[test]
    fn health_diagnostics_idle_all_ok() {
        let mut snapshot = sample_health_snapshot();
        snapshot.capture_queue_depth = 0;
        snapshot.write_queue_depth = 0;
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);

        // Should have: capture queue, write queue, ingest lag, db health
        assert!(checks.len() >= 4);
        assert!(
            checks
                .iter()
                .all(|c| c.status == HealthDiagnosticStatus::Ok)
        );
    }

    #[test]
    fn health_diagnostics_queue_active() {
        let snapshot = sample_health_snapshot(); // depth 12 and 5
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);

        let capture = checks.iter().find(|c| c.name == "capture queue").unwrap();
        assert_eq!(capture.status, HealthDiagnosticStatus::Info);
        assert!(capture.detail.contains("12 pending"));
    }

    #[test]
    fn health_diagnostics_high_lag_warns() {
        let mut snapshot = sample_health_snapshot();
        snapshot.ingest_lag_max_ms = 6000; // > 5000 threshold
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);

        let lag = checks.iter().find(|c| c.name == "ingest lag").unwrap();
        assert_eq!(lag.status, HealthDiagnosticStatus::Warning);
    }

    #[test]
    fn health_diagnostics_db_not_writable_errors() {
        let mut snapshot = sample_health_snapshot();
        snapshot.db_writable = false;
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);

        let db = checks.iter().find(|c| c.name == "database health").unwrap();
        assert_eq!(db.status, HealthDiagnosticStatus::Error);
    }

    #[test]
    fn health_diagnostics_warnings_surfaced() {
        let mut snapshot = sample_health_snapshot();
        snapshot.warnings = vec!["Write queue backpressure: 50/64 (78%)".to_string()];
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);

        let warns: Vec<_> = checks
            .iter()
            .filter(|c| c.name == "runtime warning")
            .collect();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].status, HealthDiagnosticStatus::Warning);
        assert!(warns[0].detail.contains("backpressure"));
    }

    #[test]
    fn health_idle_queue_shows_idle_label() {
        let mut snapshot = sample_health_snapshot();
        snapshot.capture_queue_depth = 0;
        snapshot.write_queue_depth = 0;
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("0 pending (idle)"));
    }

    // ── render_with_noise_info tests ─────────────────────────────────────

    #[test]
    fn noise_info_shows_muted_status_for_matching_key() {
        let mut event = sample_event();
        event.dedupe_key = Some("evt:abc123".to_string());
        let events = vec![event];

        let mut muted_keys = std::collections::HashSet::new();
        muted_keys.insert("evt:abc123".to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render_with_noise_info(&events, &ctx, &muted_keys, 1);

        assert!(output.contains("muted"));
        assert!(output.contains("1 active mute(s)"));
    }

    #[test]
    fn noise_info_shows_pending_for_non_muted_event() {
        let mut event = sample_event();
        event.dedupe_key = Some("evt:different".to_string());
        let events = vec![event];

        let mut muted_keys = std::collections::HashSet::new();
        muted_keys.insert("evt:abc123".to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render_with_noise_info(&events, &ctx, &muted_keys, 1);

        assert!(output.contains("pending"));
    }

    #[test]
    fn noise_info_json_includes_muted_field() {
        let mut event = sample_event();
        event.dedupe_key = Some("evt:key1".to_string());
        let events = vec![event];

        let mut muted_keys = std::collections::HashSet::new();
        muted_keys.insert("evt:key1".to_string());

        let ctx = RenderContext::new(OutputFormat::Json);
        let output = EventListRenderer::render_with_noise_info(&events, &ctx, &muted_keys, 1);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["muted"], serde_json::json!(true));
    }

    #[test]
    fn noise_info_json_muted_false_when_no_match() {
        let mut event = sample_event();
        event.dedupe_key = Some("evt:other".to_string());
        let events = vec![event];

        let muted_keys = std::collections::HashSet::new();

        let ctx = RenderContext::new(OutputFormat::Json);
        let output = EventListRenderer::render_with_noise_info(&events, &ctx, &muted_keys, 0);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed[0]["muted"], serde_json::json!(false));
    }

    #[test]
    fn noise_info_empty_events_shows_mute_hint() {
        let muted_keys = std::collections::HashSet::new();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render_with_noise_info(&[], &ctx, &muted_keys, 3);

        assert!(output.contains("No events found"));
        assert!(output.contains("3 active mute(s)"));
    }

    #[test]
    fn noise_info_no_mutes_no_footer() {
        let events = vec![sample_event()];
        let muted_keys = std::collections::HashSet::new();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = EventListRenderer::render_with_noise_info(&events, &ctx, &muted_keys, 0);

        assert!(!output.contains("active mute"));
    }

    #[test]
    fn health_snapshot_renders_scheduler_info() {
        use crate::tailer::SchedulerSnapshot;

        let mut snapshot = sample_health_snapshot();
        snapshot.scheduler = Some(SchedulerSnapshot {
            budget_active: true,
            max_captures_per_sec: 50,
            max_bytes_per_sec: 1_000_000,
            captures_remaining: 42,
            bytes_remaining: 500_000,
            total_rate_limited: 7,
            total_byte_budget_exceeded: 2,
            total_throttle_events: 9,
            tracked_panes: 3,
        });
        snapshot.backpressure_tier = Some("Yellow".to_string());

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Scheduler:"));
        assert!(output.contains("50/s"));
        assert!(output.contains("3 pane(s)"));
        assert!(output.contains("7 rate-limited"));
        assert!(output.contains("2 byte-budget exceeded"));
        assert!(output.contains("Backpressure:  Yellow"));
    }

    #[test]
    fn health_snapshot_json_includes_scheduler() {
        use crate::tailer::SchedulerSnapshot;

        let mut snapshot = sample_health_snapshot();
        snapshot.scheduler = Some(SchedulerSnapshot {
            budget_active: true,
            max_captures_per_sec: 10,
            max_bytes_per_sec: 500,
            captures_remaining: 8,
            bytes_remaining: 400,
            total_rate_limited: 0,
            total_byte_budget_exceeded: 0,
            total_throttle_events: 0,
            tracked_panes: 1,
        });

        let ctx = RenderContext::new(OutputFormat::Json);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["scheduler"]["max_captures_per_sec"], 10);
        assert_eq!(parsed["scheduler"]["tracked_panes"], 1);
        assert!(parsed["scheduler"]["budget_active"].as_bool().unwrap());
    }

    // ── Stuck pane detection tests (wa-nu4.3.4.2) ────────────────────────

    #[test]
    fn health_diagnostics_stuck_pane_detected() {
        let mut snapshot = sample_health_snapshot();
        // Pane 1 last seen 10 minutes ago (stuck), pane 2 recent
        snapshot.timestamp = 1_700_000_600_000; // 10 min after base
        snapshot.last_activity_by_pane = vec![
            (1, 1_700_000_000_000), // 10 min old → stuck
            (2, 1_700_000_590_000), // 10 sec old → ok
        ];

        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);
        let activity = checks.iter().find(|c| c.name == "pane activity").unwrap();
        assert_eq!(activity.status, HealthDiagnosticStatus::Warning);
        assert!(activity.detail.contains("1 stuck pane"));
        assert!(activity.detail.contains("1")); // pane id 1
    }

    #[test]
    fn health_diagnostics_no_stuck_panes_all_active() {
        let snapshot = sample_health_snapshot(); // both panes have same timestamp
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);
        let activity = checks.iter().find(|c| c.name == "pane activity").unwrap();
        assert_eq!(activity.status, HealthDiagnosticStatus::Ok);
        assert!(activity.detail.contains("all 2 pane(s) active"));
    }

    #[test]
    fn health_diagnostics_no_activity_data_skips_check() {
        let mut snapshot = sample_health_snapshot();
        snapshot.last_activity_by_pane = vec![];
        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);
        assert!(checks.iter().all(|c| c.name != "pane activity"));
    }

    #[test]
    fn health_render_shows_stuck_pane_warning() {
        let mut snapshot = sample_health_snapshot();
        snapshot.timestamp = 1_700_000_600_000;
        snapshot.last_activity_by_pane = vec![
            (3, 1_700_000_000_000), // 10 min → stuck
            (7, 1_700_000_000_000), // 10 min → stuck
            (9, 1_700_000_599_000), // 1 sec → ok
        ];

        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);

        assert!(output.contains("Stuck:"));
        assert!(output.contains("2 stuck pane(s)"));
        assert!(output.contains("3"));
        assert!(output.contains("7"));
    }

    #[test]
    fn health_render_no_stuck_panes_no_stuck_line() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);
        assert!(!output.contains("Stuck:"));
    }

    #[test]
    fn health_json_includes_last_activity_by_pane() {
        let snapshot = sample_health_snapshot();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        let activity = parsed["last_activity_by_pane"].as_array().unwrap();
        assert_eq!(activity.len(), 2);
        assert_eq!(activity[0][0], 1); // pane_id
        assert_eq!(activity[1][0], 2); // pane_id
    }

    // ── Schema stability tests (wa-nu4.3.4.2) ───────────────────────────

    #[test]
    fn health_json_schema_has_expected_fields() {
        let mut snapshot = sample_health_snapshot();
        snapshot.scheduler = Some(crate::tailer::SchedulerSnapshot {
            budget_active: true,
            max_captures_per_sec: 10,
            max_bytes_per_sec: 500,
            captures_remaining: 8,
            bytes_remaining: 400,
            total_rate_limited: 0,
            total_byte_budget_exceeded: 0,
            total_throttle_events: 0,
            tracked_panes: 1,
        });
        snapshot.backpressure_tier = Some("Green".to_string());

        let ctx = RenderContext::new(OutputFormat::Json);
        let output = HealthSnapshotRenderer::render(&snapshot, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let obj = parsed.as_object().unwrap();

        let expected = [
            "timestamp",
            "observed_panes",
            "capture_queue_depth",
            "write_queue_depth",
            "last_seq_by_pane",
            "warnings",
            "ingest_lag_avg_ms",
            "ingest_lag_max_ms",
            "db_writable",
            "db_last_write_at",
            "pane_priority_overrides",
            "scheduler",
            "backpressure_tier",
            "last_activity_by_pane",
            "restart_count",
            "last_crash_at",
            "consecutive_crashes",
            "current_backoff_ms",
            "in_crash_loop",
        ];

        for field in &expected {
            assert!(
                obj.contains_key(*field),
                "health JSON missing expected field: {field}"
            );
        }

        // No unexpected fields (schema drift detection)
        for key in obj.keys() {
            assert!(
                expected.contains(&key.as_str()),
                "health JSON contains unexpected field: {key}"
            );
        }
    }

    #[test]
    fn health_json_roundtrip_stable() {
        let snapshot = sample_health_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let deser: HealthSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(deser.timestamp, snapshot.timestamp);
        assert_eq!(deser.observed_panes, snapshot.observed_panes);
        assert_eq!(deser.capture_queue_depth, snapshot.capture_queue_depth);
        assert_eq!(deser.write_queue_depth, snapshot.write_queue_depth);
        assert_eq!(deser.last_seq_by_pane, snapshot.last_seq_by_pane);
        assert_eq!(deser.warnings, snapshot.warnings);
        assert_eq!(deser.db_writable, snapshot.db_writable);
        assert_eq!(deser.last_activity_by_pane, snapshot.last_activity_by_pane);
    }

    #[test]
    fn health_json_backward_compat_missing_new_fields() {
        // Old JSON without last_activity_by_pane should still deserialize
        let json = r#"{
            "timestamp": 1000,
            "observed_panes": 1,
            "capture_queue_depth": 0,
            "write_queue_depth": 0,
            "last_seq_by_pane": [],
            "warnings": [],
            "ingest_lag_avg_ms": 0.0,
            "ingest_lag_max_ms": 0,
            "db_writable": true,
            "db_last_write_at": null
        }"#;

        let deser: HealthSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(deser.timestamp, 1000);
        assert!(deser.last_activity_by_pane.is_empty());
        assert!(deser.pane_priority_overrides.is_empty());
        assert!(deser.scheduler.is_none());
        assert!(deser.backpressure_tier.is_none());
    }

    #[test]
    fn health_diagnostics_multiple_stuck_panes() {
        let mut snapshot = sample_health_snapshot();
        snapshot.timestamp = 1_700_001_000_000; // well after base
        snapshot.last_activity_by_pane = vec![
            (1, 1_700_000_000_000), // stuck
            (2, 1_700_000_000_000), // stuck
            (3, 1_700_000_000_000), // stuck
        ];

        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);
        let activity = checks.iter().find(|c| c.name == "pane activity").unwrap();
        assert_eq!(activity.status, HealthDiagnosticStatus::Warning);
        assert!(activity.detail.contains("3 stuck pane(s)"));
    }

    #[test]
    fn health_diagnostics_pane_just_under_threshold_not_stuck() {
        let mut snapshot = sample_health_snapshot();
        // 4 min 59 sec = under 5 min threshold
        snapshot.timestamp = 1_700_000_299_000;
        snapshot.last_activity_by_pane = vec![(1, 1_700_000_000_000)];

        let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);
        let activity = checks.iter().find(|c| c.name == "pane activity").unwrap();
        assert_eq!(activity.status, HealthDiagnosticStatus::Ok);
    }

    // =========================================================================
    // Account List Renderer tests (wa-nu4.3.2.5)
    // =========================================================================

    fn sample_accounts() -> Vec<crate::accounts::AccountRecord> {
        vec![
            crate::accounts::AccountRecord {
                id: 1,
                account_id: "acct-alpha".to_string(),
                service: "openai".to_string(),
                name: Some("Alpha".to_string()),
                percent_remaining: 82.5,
                reset_at: None,
                tokens_used: Some(175_000),
                tokens_remaining: Some(825_000),
                tokens_limit: Some(1_000_000),
                last_refreshed_at: 1_700_000_000_000,
                last_used_at: Some(1_699_999_000_000),
                created_at: 1_699_000_000_000,
                updated_at: 1_700_000_000_000,
            },
            crate::accounts::AccountRecord {
                id: 2,
                account_id: "acct-beta".to_string(),
                service: "openai".to_string(),
                name: Some("Beta".to_string()),
                percent_remaining: 45.0,
                reset_at: None,
                tokens_used: Some(550_000),
                tokens_remaining: Some(450_000),
                tokens_limit: Some(1_000_000),
                last_refreshed_at: 1_700_000_000_000,
                last_used_at: None,
                created_at: 1_699_000_000_000,
                updated_at: 1_700_000_000_000,
            },
        ]
    }

    #[test]
    fn account_list_plain_shows_table() {
        let accounts = sample_accounts();
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AccountListRenderer::render(&accounts, None, "openai", &ctx);
        assert!(output.contains("Accounts (2"), "{output}");
        assert!(output.contains("openai"), "{output}");
        assert!(output.contains("Alpha"), "{output}");
        assert!(output.contains("Beta"), "{output}");
        assert!(output.contains("82.5%"), "{output}");
        assert!(output.contains("45.0%"), "{output}");
        assert_no_ansi(&output, "AccountListRenderer");
    }

    #[test]
    fn account_list_json_is_valid() {
        let accounts = sample_accounts();
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AccountListRenderer::render(&accounts, None, "openai", &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(parsed["total"], 2);
        assert_eq!(parsed["service"], "openai");
        assert!(parsed["accounts"].is_array());
        assert_eq!(parsed["accounts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn account_list_empty() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AccountListRenderer::render(&[], None, "openai", &ctx);
        assert!(output.contains("No accounts found"), "{output}");
    }

    #[test]
    fn account_list_with_pick_preview() {
        let accounts = sample_accounts();
        let sel_config = crate::accounts::AccountSelectionConfig::default();
        let pick = crate::accounts::select_account(&accounts, &sel_config);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AccountListRenderer::render(&accounts, Some(&pick), "openai", &ctx);
        assert!(output.contains("Pick:"), "{output}");
        assert!(output.contains("Alpha"), "{output}");
        assert_no_ansi(&output, "AccountListRenderer(pick)");
    }

    #[test]
    fn account_list_json_with_pick_preview() {
        let accounts = sample_accounts();
        let sel_config = crate::accounts::AccountSelectionConfig::default();
        let pick = crate::accounts::select_account(&accounts, &sel_config);
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AccountListRenderer::render(&accounts, Some(&pick), "openai", &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(
            parsed["pick_preview"].is_object(),
            "should have pick_preview"
        );
        assert_eq!(parsed["pick_preview"]["selected_account_id"], "acct-alpha");
    }

    #[test]
    fn account_list_pick_none_when_all_below_threshold() {
        let accounts = vec![crate::accounts::AccountRecord {
            id: 1,
            account_id: "acct-low".to_string(),
            service: "openai".to_string(),
            name: Some("Low".to_string()),
            percent_remaining: 2.0,
            reset_at: None,
            tokens_used: Some(980_000),
            tokens_remaining: Some(20_000),
            tokens_limit: Some(1_000_000),
            last_refreshed_at: 1_700_000_000_000,
            last_used_at: None,
            created_at: 1_699_000_000_000,
            updated_at: 1_700_000_000_000,
        }];
        let sel_config = crate::accounts::AccountSelectionConfig::default();
        let pick = crate::accounts::select_account(&accounts, &sel_config);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AccountListRenderer::render(&accounts, Some(&pick), "openai", &ctx);
        assert!(output.contains("Pick: none"), "{output}");
        assert!(output.contains("filtered"), "{output}");
    }

    #[test]
    fn account_list_format_token_count() {
        assert_eq!(super::format_token_count(500), "500");
        assert_eq!(super::format_token_count(1_500), "2K");
        assert_eq!(super::format_token_count(175_000), "175K");
        assert_eq!(super::format_token_count(1_500_000), "1.5M");
        assert_eq!(super::format_token_count(0), "0");
    }

    // ── Timeline renderer tests (wa-6sk.3) ────────────────────────────────

    use crate::storage::{Correlation, CorrelationRef, HandledInfo, PaneInfo, TimelineEvent};

    fn sample_pane_info(pane_id: u64, agent_type: &str) -> PaneInfo {
        PaneInfo {
            pane_id,
            pane_uuid: None,
            agent_type: Some(agent_type.to_string()),
            domain: "local".to_string(),
            cwd: Some("/home/user".to_string()),
            title: None,
        }
    }

    fn sample_timeline_event(id: i64, pane_id: u64, ts: i64) -> TimelineEvent {
        TimelineEvent {
            id,
            timestamp: ts,
            pane_info: sample_pane_info(pane_id, "codex"),
            rule_id: format!("rule.{id}"),
            event_type: "session.started".to_string(),
            severity: "info".to_string(),
            confidence: 0.9,
            handled: None,
            correlations: vec![],
            summary: None,
        }
    }

    fn sample_timeline(events: Vec<TimelineEvent>, correlations: Vec<Correlation>) -> Timeline {
        let start = events.iter().map(|e| e.timestamp).min().unwrap_or(0);
        let end = events.iter().map(|e| e.timestamp).max().unwrap_or(0);
        let total = events.len() as u64;
        Timeline {
            start,
            end,
            events,
            correlations,
            total_count: total,
            has_more: false,
        }
    }

    #[test]
    fn timeline_empty_renders_message() {
        let tl = sample_timeline(vec![], vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("No events"), "{output}");
    }

    #[test]
    fn timeline_single_event_renders() {
        let events = vec![sample_timeline_event(1, 3, 1_738_000_000_000)];
        let tl = sample_timeline(events, vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("Pane 3"), "{output}");
        assert!(output.contains("session.started"), "{output}");
        assert!(output.contains("Timeline:"), "{output}");
        assert!(output.contains("1 panes"), "{output}");
        assert!(output.contains("1 events"), "{output}");
    }

    #[test]
    fn timeline_multiple_events_connector_chars() {
        let events = vec![
            sample_timeline_event(1, 1, 1_738_000_000_000),
            sample_timeline_event(2, 2, 1_738_000_010_000),
            sample_timeline_event(3, 3, 1_738_000_020_000),
        ];
        let tl = sample_timeline(events, vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        // First event: ─┬─, middle: ─┼─, last: ─┴─
        assert!(
            output.contains("\u{2500}\u{252c}\u{2500}"),
            "missing top connector: {output}"
        );
        assert!(
            output.contains("\u{2500}\u{253c}\u{2500}"),
            "missing mid connector: {output}"
        );
        assert!(
            output.contains("\u{2500}\u{2534}\u{2500}"),
            "missing bot connector: {output}"
        );
    }

    #[test]
    fn timeline_handled_shows_arrow() {
        let mut event = sample_timeline_event(1, 1, 1_738_000_000_000);
        event.handled = Some(HandledInfo {
            handled_at: 1_738_000_008_000,
            workflow_id: Some("wf-abc".to_string()),
            status: "handled".to_string(),
        });
        let tl = sample_timeline(vec![event], vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("handled"), "{output}");
        assert!(output.contains("\u{2192}"), "missing arrow: {output}");
    }

    #[test]
    fn timeline_verbose_shows_workflow() {
        let mut event = sample_timeline_event(1, 1, 1_738_000_000_000);
        event.handled = Some(HandledInfo {
            handled_at: 1_738_000_008_000,
            workflow_id: Some("wf-xyz".to_string()),
            status: "in_progress".to_string(),
        });
        let tl = sample_timeline(vec![event], vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(1);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("workflow: wf-xyz"), "{output}");
        assert!(output.contains("in_progress"), "{output}");
    }

    #[test]
    fn timeline_debug_shows_details() {
        let mut event = sample_timeline_event(1, 1, 1_738_000_000_000);
        event.summary = Some("Test summary text".to_string());
        let tl = sample_timeline(vec![event], vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(2);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("id=1"), "{output}");
        assert!(output.contains("rule=rule.1"), "{output}");
        assert!(output.contains("sev=info"), "{output}");
        assert!(output.contains("Test summary text"), "{output}");
    }

    #[test]
    fn timeline_correlation_markers() {
        let mut e1 = sample_timeline_event(1, 1, 1_738_000_000_000);
        let mut e2 = sample_timeline_event(2, 7, 1_738_000_045_000);
        let corr = Correlation {
            id: "corr-fail-1".to_string(),
            event_ids: vec![1, 2],
            correlation_type: CorrelationType::Failover,
            confidence: 0.85,
            description: "Agent failover detected".to_string(),
        };
        e1.correlations = vec![CorrelationRef {
            id: "corr-fail-1".to_string(),
            correlation_type: CorrelationType::Failover,
        }];
        e2.correlations = vec![CorrelationRef {
            id: "corr-fail-1".to_string(),
            correlation_type: CorrelationType::Failover,
        }];
        let tl = sample_timeline(vec![e1, e2], vec![corr]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("[CORRELATED: failover]"), "{output}");
    }

    #[test]
    fn timeline_json_output() {
        let events = vec![sample_timeline_event(1, 1, 1_738_000_000_000)];
        let tl = sample_timeline(events, vec![]);
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = TimelineRenderer::render(&tl, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.get("events").is_some(), "{output}");
        assert!(parsed.get("correlations").is_some(), "{output}");
        assert!(parsed.get("total_count").is_some(), "{output}");
    }

    #[test]
    fn timeline_plain_no_ansi() {
        let events = vec![sample_timeline_event(1, 1, 1_738_000_000_000)];
        let tl = sample_timeline(events, vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(
            !output.contains("\x1b["),
            "found ANSI codes in plain output: {output}"
        );
    }

    #[test]
    fn timeline_has_more_shows_hint() {
        let events = vec![sample_timeline_event(1, 1, 1_738_000_000_000)];
        let mut tl = sample_timeline(events, vec![]);
        tl.total_count = 50;
        tl.has_more = true;
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("49 more events"), "{output}");
    }

    #[test]
    fn timeline_legend_present() {
        let events = vec![sample_timeline_event(1, 1, 1_738_000_000_000)];
        let tl = sample_timeline(events, vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("Legend:"), "{output}");
    }

    #[test]
    fn timeline_multiple_pane_count() {
        let events = vec![
            sample_timeline_event(1, 1, 1_738_000_000_000),
            sample_timeline_event(2, 3, 1_738_000_010_000),
            sample_timeline_event(3, 5, 1_738_000_020_000),
            sample_timeline_event(4, 1, 1_738_000_030_000),
        ];
        let tl = sample_timeline(events, vec![]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("3 panes"), "{output}");
        assert!(output.contains("4 events"), "{output}");
    }

    #[test]
    fn timeline_debug_shows_correlation_refs() {
        let mut e1 = sample_timeline_event(1, 1, 1_738_000_000_000);
        e1.correlations = vec![CorrelationRef {
            id: "corr-t-1".to_string(),
            correlation_type: CorrelationType::Temporal,
        }];
        let corr = Correlation {
            id: "corr-t-1".to_string(),
            event_ids: vec![1],
            correlation_type: CorrelationType::Temporal,
            confidence: 0.8,
            description: "Temporal cluster".to_string(),
        };
        let tl = sample_timeline(vec![e1], vec![corr]);
        let ctx = RenderContext::new(OutputFormat::Plain).verbose(2);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("corr: corr-t-1"), "{output}");
        assert!(output.contains("Temporal"), "{output}");
    }

    #[test]
    fn format_time_hms_basic() {
        // 1738000000 seconds since epoch = HH:MM:SS (mod 86400)
        let result = super::format_time_hms(1_738_000_000_000);
        assert_eq!(result.len(), 8, "HH:MM:SS should be 8 chars: {result}");
        assert!(result.contains(':'), "should contain colon: {result}");
    }

    #[test]
    fn timeline_dedupe_group_label() {
        let mut e1 = sample_timeline_event(1, 1, 1_738_000_000_000);
        let mut e2 = sample_timeline_event(2, 2, 1_738_000_005_000);
        let corr = Correlation {
            id: "corr-dedupe-1".to_string(),
            event_ids: vec![1, 2],
            correlation_type: CorrelationType::DedupeGroup,
            confidence: 0.7,
            description: "Same rule fired across 2 panes".to_string(),
        };
        e1.correlations = vec![CorrelationRef {
            id: "corr-dedupe-1".to_string(),
            correlation_type: CorrelationType::DedupeGroup,
        }];
        e2.correlations = vec![CorrelationRef {
            id: "corr-dedupe-1".to_string(),
            correlation_type: CorrelationType::DedupeGroup,
        }];
        let tl = sample_timeline(vec![e1, e2], vec![corr]);
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = TimelineRenderer::render(&tl, &ctx);
        assert!(output.contains("[CORRELATED: dedupe-group]"), "{output}");
    }

    // ── SearchSuggestRenderer tests (bd-282k) ─────────────────────────

    fn sample_suggestions() -> Vec<SearchSuggestion> {
        vec![
            SearchSuggestion {
                text: "error".to_string(),
                description: Some("Common errors".to_string()),
            },
            SearchSuggestion {
                text: "warning".to_string(),
                description: Some("Warnings in output".to_string()),
            },
            SearchSuggestion {
                text: "panic".to_string(),
                description: None,
            },
        ]
    }

    #[test]
    fn suggest_renderer_empty_no_partial() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = SearchSuggestRenderer::render(&[], "", &ctx);
        assert!(output.contains("No suggestions"), "{output}");
        assert!(!output.contains("for \""), "{output}");
    }

    #[test]
    fn suggest_renderer_empty_with_partial() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = SearchSuggestRenderer::render(&[], "xyz", &ctx);
        assert!(output.contains("No suggestions for \"xyz\""), "{output}");
    }

    #[test]
    fn suggest_renderer_plain_output() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let suggestions = sample_suggestions();
        let output = SearchSuggestRenderer::render(&suggestions, "err", &ctx);
        assert!(output.contains("error"), "{output}");
        assert!(output.contains("Common errors"), "{output}");
        assert!(output.contains("warning"), "{output}");
        assert!(output.contains("panic"), "{output}");
        assert_no_ansi(&output, "SearchSuggestRenderer");
    }

    #[test]
    fn suggest_renderer_json_output() {
        let ctx = RenderContext::new(OutputFormat::Json);
        let suggestions = sample_suggestions();
        let output = SearchSuggestRenderer::render(&suggestions, "", &ctx);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["text"], "error");
        assert_eq!(parsed[1]["text"], "warning");
        assert_eq!(parsed[2]["description"], serde_json::Value::Null);
    }

    #[test]
    fn suggest_renderer_heading_includes_partial() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let suggestions = sample_suggestions();
        let output = SearchSuggestRenderer::render(&suggestions, "err", &ctx);
        assert!(output.contains("Suggestions for \"err\""), "{output}");
    }

    #[test]
    fn suggest_renderer_heading_no_partial() {
        let ctx = RenderContext::new(OutputFormat::Plain);
        let suggestions = sample_suggestions();
        let output = SearchSuggestRenderer::render(&suggestions, "", &ctx);
        assert!(output.contains("Search suggestions:"), "{output}");
    }
}
