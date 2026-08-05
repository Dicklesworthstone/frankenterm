//! Output layer for CLI commands
//!
//! This module provides consistent output formatting across all CLI commands,
//! with support for multiple output modes (auto/rich, plain, json).
//!
//! # Architecture
//!
//! ```text
//! Command Handler → Data → Renderer → String
//!                          ↓
//!               OutputFormat (auto/plain/json)
//! ```
//!
//! # Output Modes
//!
//! - `auto`: Rich formatting if TTY, plain if not (default)
//! - `plain`: No ANSI codes, stable for piping
//! - `json`: Machine-readable JSON output
//!
//! # Usage
//!
//! ```no_run
//! use frankenterm_core::output::{detect_format, Style};
//!
//! let format = detect_format();
//! let style = Style::from_format(format);
//! println!("{}", style.bold("FrankenTerm"));
//! ```

mod error_renderer;
mod format;
mod renderers;
mod table;

pub use error_renderer::{ErrorRenderer, get_code_for_error, render_error};
pub use format::{EffectiveFormat, OutputFormat, Style, colors, detect_format};
pub use renderers::{
    AccountListRenderer, ActionHistoryRenderer, AnalyticsAgentRenderer, AnalyticsDailyRenderer,
    AnalyticsExportRenderer, AnalyticsSummaryData, AnalyticsSummaryRenderer, AuditListRenderer,
    EventListRenderer, HealthDiagnostic, HealthDiagnosticStatus, HealthSnapshotRenderer,
    PaneTableRenderer, Render, RenderContext, ResizeDashboardSnapshot, RuleDetail,
    RuleDetailRenderer, RuleListItem, RuleTestMatch, RulesListRenderer, RulesTestRenderer,
    SearchResultRenderer, SearchSuggestRenderer, Summary, TimelineRenderer, WorkflowResult,
    WorkflowResultRenderer, WorkflowStepResult, sanitize_redact_truncate_bounded, truncate,
    truncate_bounded,
};
pub use table::{
    Alignment, Column, Table, normalize_terminal_text_for_redaction, sanitize_terminal_text,
    strip_ansi,
};
