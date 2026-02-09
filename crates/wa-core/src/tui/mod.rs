//! TUI module for wa
//!
//! Provides an optional interactive terminal UI for WezTerm Automata.
//! Behind the `tui` (ratatui) or `ftui` (FrankenTUI) feature flag.
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
//!
//! # Backend Selection
//!
//! The rendering backend is selected at compile time via feature flags:
//! - `tui`: Legacy ratatui/crossterm backend (current production)
//! - `ftui`: FrankenTUI backend (migration target, see docs/adr/)
//!
//! These features are mutually exclusive (enforced by compile_error! in lib.rs).
//! The QueryClient trait and data types are shared between both backends.

// QueryClient trait and data types — framework-agnostic, always compiled.
mod query;
pub use query::{ProductionQueryClient, QueryClient, QueryError};

// Compatibility adapter for incremental migration between backends.
// Framework-agnostic types with cfg-gated conversions for each backend.
// See docs/adr/0001-adopt-frankentui-for-tui-migration.md for context.
// DELETION: Remove this module when the `tui` feature is dropped (FTUI-09.3).
pub mod ftui_compat;

// View adapters: QueryClient data types → render-ready view models.
// Framework-agnostic, usable by both ratatui and ftui rendering code.
// See docs/adr/0008-query-facade-contract.md for the data boundary.
pub mod view_adapters;

// One-writer output gate — tracks whether the TUI owns the terminal.
// Thread-safe atomic gate consulted by logging, crash handlers, debug output.
// DELETION: Remove when ftui TerminalWriter owns output routing (FTUI-09.3).
pub mod output_gate;

// Canonical keybinding table and input dispatcher.
// Single source of truth for key→action mapping, shared between backends.
// DELETION: Remove legacy parity tests when `tui` feature is dropped (FTUI-09.3).
pub mod keymap;

// Terminal session ownership abstraction — lifecycle, command handoff, teardown.
// DELETION: Remove when ftui Program runtime fully owns the lifecycle (FTUI-09.3).
pub mod terminal_session;

// Command execution handoff — suspend TUI, run shell command, resume.
// Deterministic state machine with output gate integration.
// DELETION: Remove when ftui's native subprocess model replaces this (FTUI-09.3).
pub mod command_handoff;

// Legacy ratatui backend
#[cfg(feature = "tui")]
mod app;
#[cfg(feature = "tui")]
mod views;

#[cfg(feature = "tui")]
pub use app::{App, AppConfig, run_tui};
#[cfg(feature = "tui")]
pub use views::{View, ViewState};

// FrankenTUI backend (migration target — FTUI-03 through FTUI-06)
#[cfg(feature = "ftui")]
mod ftui_stub;

#[cfg(feature = "ftui")]
pub use ftui_stub::{App, AppConfig, View, ViewState, run_tui};
