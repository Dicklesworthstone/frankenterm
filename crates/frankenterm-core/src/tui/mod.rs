//! TUI module for ft
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
//! │        frankenterm-core query/model layer        │
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
//! - `rollout`: Both backends compiled; runtime selection via `FT_TUI_BACKEND`
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
pub use ftui_stub::{App, AppConfig, View, ViewState, run_tui};

// Export ftui-native model/message and alias types in both rollout and
// non-rollout builds so benchmarks and tests can target the ftui surface
// without depending on rollout-dispatched aliases.
#[cfg(feature = "ftui")]
pub use ftui_stub::{
    AppConfig as FtuiAppConfig, View as FtuiView, ViewState as FtuiViewState, WaModel, WaMsg,
};

// Rollout dispatch: runtime backend selection via FT_TUI_BACKEND env var.
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

    // -- is_exempt_line unit tests --

    #[test]
    fn exempt_line_comment() {
        assert!(is_exempt_line("// use ratatui"));
        assert!(is_exempt_line("  // some comment"));
    }

    #[test]
    fn exempt_line_doc_comment() {
        assert!(is_exempt_line("/// use ratatui::widgets"));
        assert!(is_exempt_line("  /// doc comment with crossterm::"));
    }

    #[test]
    fn exempt_line_module_doc() {
        assert!(is_exempt_line("//! module doc with ratatui::"));
        assert!(is_exempt_line("  //! inner doc comment"));
    }

    #[test]
    fn exempt_line_cfg_gated() {
        assert!(is_exempt_line("#[cfg(feature = \"tui\")] use ratatui;"));
        assert!(is_exempt_line("  #[cfg(test)] mod foo;"));
    }

    #[test]
    fn not_exempt_regular_code() {
        assert!(!is_exempt_line("use ratatui::widgets;"));
        assert!(!is_exempt_line("let x = 42;"));
        assert!(!is_exempt_line("fn main() {}"));
    }

    #[test]
    fn not_exempt_empty_string() {
        assert!(!is_exempt_line(""));
    }

    #[test]
    fn not_exempt_whitespace_only() {
        assert!(!is_exempt_line("   "));
        assert!(!is_exempt_line("\t"));
    }

    #[test]
    fn exempt_comment_with_ratatui_mention() {
        assert!(is_exempt_line("// ratatui::frame::Frame"));
        assert!(is_exempt_line("// crossterm::event::Event"));
    }

    #[test]
    fn not_exempt_code_with_cfg_in_string() {
        // is_exempt_line uses .contains("#[cfg") so string literals with #[cfg
        // are treated as exempt — a known limitation acceptable for the guardrail.
        assert!(is_exempt_line("let s = \"not #[cfg gated\";"));
    }

    // -- Constant validation tests --

    #[test]
    fn forbidden_patterns_are_non_empty() {
        assert!(!FORBIDDEN_PATTERNS.is_empty());
    }

    #[test]
    fn forbidden_patterns_all_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in FORBIDDEN_PATTERNS {
            assert!(seen.insert(p), "duplicate forbidden pattern: {}", p);
        }
    }

    #[test]
    fn forbidden_patterns_contain_ratatui_and_crossterm() {
        let has_ratatui = FORBIDDEN_PATTERNS.iter().any(|p| p.contains("ratatui"));
        let has_crossterm = FORBIDDEN_PATTERNS.iter().any(|p| p.contains("crossterm"));
        assert!(has_ratatui, "should have a ratatui pattern");
        assert!(has_crossterm, "should have a crossterm pattern");
    }

    #[test]
    fn allowed_files_all_end_with_rs() {
        for f in ALLOWED_FILES {
            assert!(
                f.ends_with(".rs"),
                "allowed file should end with .rs: {}",
                f
            );
        }
    }

    #[test]
    fn allowed_files_all_unique() {
        let mut seen = std::collections::HashSet::new();
        for f in ALLOWED_FILES {
            assert!(seen.insert(f), "duplicate allowed file: {}", f);
        }
    }

    #[test]
    fn agnostic_modules_all_unique_names() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in AGNOSTIC_MODULES {
            assert!(seen.insert(name), "duplicate agnostic module: {}", name);
        }
    }

    #[test]
    fn agnostic_modules_all_have_content() {
        for (name, content) in AGNOSTIC_MODULES {
            assert!(
                !content.is_empty(),
                "agnostic module {} has empty content",
                name
            );
        }
    }

    #[test]
    fn agnostic_modules_names_end_with_rs() {
        for (name, _) in AGNOSTIC_MODULES {
            assert!(
                name.ends_with(".rs"),
                "agnostic module name should end with .rs: {}",
                name
            );
        }
    }

    #[test]
    fn allowed_and_agnostic_are_disjoint() {
        let agnostic_names: Vec<&str> = AGNOSTIC_MODULES.iter().map(|(n, _)| *n).collect();
        for allowed in ALLOWED_FILES {
            assert!(
                !agnostic_names.contains(allowed),
                "file {} is in both ALLOWED_FILES and AGNOSTIC_MODULES",
                allowed
            );
        }
    }

    #[test]
    fn agnostic_module_count_matches_expectation() {
        assert!(
            AGNOSTIC_MODULES.len() >= 5,
            "expected at least 5 agnostic modules, got {}",
            AGNOSTIC_MODULES.len()
        );
    }

    #[test]
    fn forbidden_patterns_detect_use_import() {
        let line = "use ratatui::widgets::Table;";
        assert!(FORBIDDEN_PATTERNS.iter().any(|p| line.contains(p)));
    }

    #[test]
    fn forbidden_patterns_detect_qualified_path() {
        let line = "let frame = ratatui::Frame::default();";
        assert!(FORBIDDEN_PATTERNS.iter().any(|p| line.contains(p)));
    }

    #[test]
    fn forbidden_patterns_do_not_match_safe_code() {
        let line = "let name = \"frankenterm\";";
        assert!(!FORBIDDEN_PATTERNS.iter().any(|p| line.contains(p)));
    }

    #[test]
    fn exempt_mixed_cfg_and_use() {
        assert!(is_exempt_line(
            "#[cfg(feature = \"tui\")] use ratatui::Frame;"
        ));
    }

    #[test]
    fn allowed_files_count_is_reasonable() {
        assert!(
            ALLOWED_FILES.len() <= 20,
            "allowlist is suspiciously large ({}) — review for stale entries",
            ALLOWED_FILES.len()
        );
        assert!(
            ALLOWED_FILES.len() >= 3,
            "allowlist should have at least a few legacy files"
        );
    }

    #[test]
    fn forbidden_patterns_count() {
        assert_eq!(
            FORBIDDEN_PATTERNS.len(),
            4,
            "expected 4 forbidden patterns (use ratatui, use crossterm, ratatui::, crossterm::)"
        );
    }

    #[test]
    fn is_exempt_line_triple_slash_with_code() {
        assert!(is_exempt_line("/// ```rust\n/// use ratatui;```"));
    }

    #[test]
    fn is_exempt_line_deeply_indented_comment() {
        assert!(is_exempt_line("        // deeply indented comment"));
        assert!(is_exempt_line("\t\t// tab-indented comment"));
    }
}
