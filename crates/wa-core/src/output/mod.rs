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
//! ```ignore
//! use wa_core::output::{OutputFormat, Renderer, PaneRenderer};
//!
//! let format = OutputFormat::detect();
//! let renderer = PaneRenderer::new(format);
//! println!("{}", renderer.render(&panes));
//! ```

mod error_renderer;
mod format;
mod renderers;
mod table;

pub use error_renderer::{ErrorRenderer, get_code_for_error, render_error};
pub use format::{OutputFormat, detect_format};
pub use renderers::{
    AccountListRenderer, ActionHistoryRenderer, AnalyticsAgentRenderer, AnalyticsDailyRenderer,
    AnalyticsExportRenderer, AnalyticsSummaryData, AnalyticsSummaryRenderer, AuditListRenderer,
    EventListRenderer, HealthDiagnostic, HealthDiagnosticStatus, HealthSnapshotRenderer,
    PaneTableRenderer, Render, RenderContext, RuleDetail, RuleDetailRenderer, RuleListItem,
    RuleTestMatch, RulesListRenderer, RulesTestRenderer, SearchResultRenderer,
    SearchSuggestRenderer, TimelineRenderer, WorkflowResultRenderer,
};
pub use table::{Alignment, Column, Table};
