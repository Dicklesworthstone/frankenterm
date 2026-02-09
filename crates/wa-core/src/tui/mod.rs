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
//! The rendering backend is selected via feature flags:
//! - `tui`: Legacy ratatui/crossterm backend (current production)
//! - `ftui`: FrankenTUI backend (migration target, see docs/adr/)
//! - `rollout`: Both backends compiled; runtime selection via `WA_TUI_BACKEND`
//!
//! `tui` and `ftui` are mutually exclusive unless `rollout` is active.
//! The QueryClient trait and data types are shared between both backends.

// QueryClient trait and data types — framework-agnostic, always compiled.
mod query;
pub use query::{
    EventFilters, EventView, HealthStatus, PaneView, ProductionQueryClient, QueryClient,
    QueryError, SearchResultView, TriageAction, TriageItemView, WorkflowProgressView,
};

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

// Deterministic UI state reducer — pure function mapping (state, action) → (state, effects).
// Framework-agnostic, shared between ratatui and ftui backends.
// Replaces the ad-hoc state mutation in app.rs during migration.
pub mod state;

// Legacy ratatui backend
#[cfg(feature = "tui")]
mod app;
#[cfg(feature = "tui")]
mod views;

// Single-backend re-exports (suppressed when rollout is active to avoid
// name collisions — rollout.rs provides the dispatch layer instead).
#[cfg(all(feature = "tui", not(feature = "rollout")))]
pub use app::{App, AppConfig, run_tui};
#[cfg(all(feature = "tui", not(feature = "rollout")))]
pub use views::{View, ViewState};

// FrankenTUI backend (migration target — FTUI-03 through FTUI-06)
#[cfg(feature = "ftui")]
mod ftui_stub;

#[cfg(all(feature = "ftui", not(feature = "rollout")))]
pub use ftui_stub::{App, AppConfig, View, ViewState, WaModel, WaMsg, run_tui};

// Rollout dispatch: runtime backend selection via WA_TUI_BACKEND env var.
// Compiles both backends and delegates at runtime based on operator preference.
// DELETION: Remove when the `tui` feature is dropped (FTUI-09.3).
#[cfg(feature = "rollout")]
mod rollout;
#[cfg(feature = "rollout")]
pub use rollout::{AppConfig, TuiBackend, View, ViewState, run_tui, select_backend};

// -------------------------------------------------------------------------
// FTUI-09.3.a: Compile-time guardrails against ratatui reintroduction
// -------------------------------------------------------------------------
//
// These tests read source files at test time and verify that migration-
// complete modules do not contain bare (non-cfg-gated) ratatui/crossterm
// references. This catches accidental re-imports during development without
// requiring a separate CI script.
//
// Developer guidance for violations:
//   1. Replace `ratatui::` types with equivalents from `tui::ftui_compat`
//   2. Replace `crossterm::` types with `tui::ftui_compat::InputEvent` etc.
//   3. Use `ftui::` directly for FrankenTUI-native code
//   4. If a conversion is genuinely needed, add it to ftui_compat.rs with
//      `#[cfg(feature = "tui")]`
//
// Allowlist: To exempt a file, add it to ALLOWED_FILES below with a comment
// explaining why and when the exception expires.

#[cfg(test)]
mod import_guardrail_tests {
    /// Files that are part of the compatibility/legacy layer and ARE allowed
    /// to contain ratatui/crossterm references.
    const ALLOWED_FILES: &[&str] = &[
        "ftui_compat.rs",      // Compatibility adapter with cfg-gated conversions
        "terminal_session.rs", // CrosstermSession impl is cfg-gated under `tui`
        "mod.rs",              // Conditional module imports
        "app.rs",              // Legacy ratatui backend (only compiled under `tui`)
        "views.rs",            // Legacy ratatui backend (only compiled under `tui`)
        "rollout.rs",          // Runtime dispatch — references both backends (FTUI-09.2)
    ];

    /// Migration-complete modules that MUST NOT contain bare ratatui/crossterm.
    const AGNOSTIC_MODULES: &[(&str, &str)] = &[
        ("query.rs", include_str!("query.rs")),
        ("view_adapters.rs", include_str!("view_adapters.rs")),
        ("keymap.rs", include_str!("keymap.rs")),
        ("state.rs", include_str!("state.rs")),
        ("command_handoff.rs", include_str!("command_handoff.rs")),
        ("output_gate.rs", include_str!("output_gate.rs")),
    ];

    /// Patterns that indicate a bare (non-cfg-gated) ratatui/crossterm reference.
    const FORBIDDEN_PATTERNS: &[&str] =
        &["use ratatui", "use crossterm", "ratatui::", "crossterm::"];

    /// Check if a line is exempt from the import check.
    fn is_exempt_line(line: &str) -> bool {
        let trimmed = line.trim();
        // Skip comments
        if trimmed.starts_with("//") || trimmed.starts_with("///") {
            return true;
        }
        // Skip cfg-gated lines
        if trimmed.contains("#[cfg") {
            return true;
        }
        // Skip lines inside doc strings that reference the types
        if trimmed.starts_with("//!") {
            return true;
        }
        false
    }

    #[test]
    fn agnostic_modules_have_no_bare_ratatui_imports() {
        let mut violations = Vec::new();

        for &(filename, source) in AGNOSTIC_MODULES {
            for (line_num, line) in source.lines().enumerate() {
                if is_exempt_line(line) {
                    continue;
                }
                for &pattern in FORBIDDEN_PATTERNS {
                    if line.contains(pattern) {
                        violations.push(format!(
                            "  {}:{}: {}",
                            filename,
                            line_num + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "\n\nFTUI-09.3.a VIOLATION: Migration-complete modules contain bare \
             ratatui/crossterm references.\n\
             \n\
             The following lines must be updated to use ftui_compat types or ftui:: \
             directly:\n\
             {}\n\
             \n\
             See tui/mod.rs FTUI-09.3.a section for developer guidance.\n",
            violations.join("\n")
        );
    }

    #[test]
    fn allowed_files_list_is_consistent() {
        // Verify the allowlist files actually exist (catches stale entries after deletion)
        let tui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");

        for &allowed in ALLOWED_FILES {
            let path = tui_dir.join(allowed);
            assert!(
                path.exists(),
                "Allowlisted file tui/{allowed} does not exist — \
                 remove it from ALLOWED_FILES in tui/mod.rs"
            );
        }
    }

    #[test]
    fn no_new_ratatui_modules_without_allowlist() {
        // Scan all .rs files in the tui directory and flag any that contain
        // ratatui/crossterm but aren't in the allowlist.
        let tui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");

        let agnostic_names: Vec<&str> = AGNOSTIC_MODULES.iter().map(|(n, _)| *n).collect();
        let mut unlisted = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&tui_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let filename = path.file_name().unwrap().to_str().unwrap();

                // Skip allowed and already-checked files
                if ALLOWED_FILES.contains(&filename) || agnostic_names.contains(&filename) {
                    continue;
                }

                // Read file and check for forbidden patterns
                if let Ok(source) = std::fs::read_to_string(&path) {
                    let has_forbidden = source.lines().any(|line| {
                        if is_exempt_line(line) {
                            return false;
                        }
                        FORBIDDEN_PATTERNS.iter().any(|p| line.contains(p))
                    });

                    if has_forbidden {
                        unlisted.push(filename.to_string());
                    }
                }
            }
        }

        assert!(
            unlisted.is_empty(),
            "\n\nFTUI-09.3.a WARNING: New TUI modules contain ratatui/crossterm references \
             but are not in the allowlist or agnostic-modules list:\n  {}\n\n\
             Add each file to either ALLOWED_FILES (if it needs ratatui/crossterm) or \
             AGNOSTIC_MODULES (if it should be framework-agnostic) in tui/mod.rs.\n",
            unlisted.join("\n  ")
        );
    }
}
