//! TUI module for wa
//!
//! Provides an optional interactive terminal UI for WezTerm Automata.
//! Behind the `tui` feature flag.
//!
//! # Architecture
//!
//! The TUI is designed with a strict separation between UI and data access:
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │                   App (event loop)              │
//! │  ┌────────────┐   ┌────────────┐   ┌─────────┐ │
//! │  │   Views    │ ← │   State    │ ← │ Events  │ │
//! │  └────────────┘   └────────────┘   └─────────┘ │
//! └─────────────────────────────────────────────────┘
//!              │
//!              ▼
//! ┌─────────────────────────────────────────────────┐
//! │               QueryClient (trait)               │
//! │    list_panes() | list_events() | search()     │
//! └─────────────────────────────────────────────────┘
//!              │
//!              ▼
//! ┌─────────────────────────────────────────────────┐
//! │            wa-core query/model layer            │
//! │       (same APIs used by robot commands)        │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! This separation ensures:
//! - The TUI is testable (mock QueryClient for unit tests)
//! - No direct DB calls from UI widgets
//! - Consistent data access with robot mode

mod app;
mod query;
mod views;

pub use app::{App, AppConfig, run_tui};
pub use query::{QueryClient, QueryError, ProductionQueryClient};
pub use views::{View, ViewState};
