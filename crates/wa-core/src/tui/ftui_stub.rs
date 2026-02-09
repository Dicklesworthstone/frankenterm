//! FrankenTUI backend stub for wa TUI migration.
//!
//! This module provides the same public interface as the ratatui backend
//! (`app.rs` + `views.rs`) but backed by FrankenTUI. During the migration,
//! most of this is placeholder code that will be replaced by real
//! implementations in FTUI-03 (runtime), FTUI-05 (views), and FTUI-06 (input).
//!
//! The QueryClient abstraction layer is shared with the legacy backend
//! and is not duplicated here — see `query.rs`.

use std::time::Duration;

use super::query::QueryClient;

/// Available views in the TUI.
///
/// Duplicated from `views.rs` during migration. Will be unified into a shared
/// `view_types` module in FTUI-04.2 (adapter layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Home,
    Panes,
    Events,
    Triage,
    History,
    Search,
    Help,
}

impl View {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Panes => "Panes",
            Self::Events => "Events",
            Self::Triage => "Triage",
            Self::History => "History",
            Self::Search => "Search",
            Self::Help => "Help",
        }
    }

    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Home,
            Self::Panes,
            Self::Events,
            Self::Triage,
            Self::History,
            Self::Search,
            Self::Help,
        ]
    }
}

/// View state stub. Full state management will be implemented in FTUI-04.3
/// (deterministic UI state reducer).
#[derive(Debug, Default)]
pub struct ViewState {
    pub current_view: View,
    pub error_message: Option<String>,
}

/// TUI application configuration.
pub struct AppConfig {
    pub refresh_interval: Duration,
    pub debug: bool,
}

/// FrankenTUI application shell.
///
/// Placeholder struct — the real implementation lands in FTUI-03.1 (terminal
/// session ownership) and FTUI-05.1 (app shell with tabs/layout/view router).
pub struct App<Q: QueryClient> {
    _query: Q,
    _config: AppConfig,
}

/// Run the TUI using the FrankenTUI backend.
///
/// This is the ftui equivalent of `app::run_tui`. Currently a stub that
/// prints a message and exits. The real implementation will be built in
/// FTUI-03 (runtime ownership) and wired up in FTUI-05.1 (app shell).
pub fn run_tui<Q: QueryClient>(_query: Q, _config: AppConfig) -> Result<(), crate::Error> {
    eprintln!("wa: FrankenTUI backend is not yet implemented.");
    eprintln!("wa: Use `--features tui` for the current ratatui backend.");
    eprintln!("wa: See docs/adr/0004-phased-rollout-and-rollback.md for migration status.");
    Ok(())
}
