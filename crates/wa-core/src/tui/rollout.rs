//! Runtime TUI backend selection for phased rollout (FTUI-09.2).
//!
//! During Stages 1-2 of the ftui migration, this module enables both
//! backends to coexist in a single binary.  The operator selects the
//! active backend via the `WA_TUI_BACKEND` environment variable.
//!
//! | Stage | Default   | Override                     |
//! |-------|-----------|------------------------------|
//! | 0 Dev | compile-time only (`--features tui` or `ftui`) |
//! | 1 Canary | ratatui | `WA_TUI_BACKEND=ftui`      |
//! | 2 Beta   | ftui    | `WA_TUI_BACKEND=ratatui`   |
//! | 3 GA     | ftui only (this module deleted)        |
//!
//! See `docs/ftui-rollout-strategy.md` for full rollout details.
//!
//! DELETION: Remove this module at Stage 3 (FTUI-09.5).

use super::query::QueryClient;

// Re-export AppConfig from the legacy backend (struct is identical in both).
pub use super::app::AppConfig;

// Re-export View and ViewState from the ftui backend (the migration target).
pub use super::ftui_stub::{View, ViewState};

/// Active TUI rendering backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiBackend {
    /// Legacy ratatui/crossterm backend.
    Ratatui,
    /// FrankenTUI backend (migration target).
    Ftui,
}

impl TuiBackend {
    /// Default backend for the current rollout stage.
    ///
    /// Update this constant when advancing stages:
    ///   Stage 1 (Canary) → `Ratatui`  (current)
    ///   Stage 2 (Beta)   → `Ftui`
    const STAGE_DEFAULT: Self = Self::Ratatui;
}

impl std::fmt::Display for TuiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ratatui => f.write_str("ratatui"),
            Self::Ftui => f.write_str("ftui"),
        }
    }
}

/// Select the TUI backend based on the `WA_TUI_BACKEND` environment variable.
///
/// Returns the stage default if the variable is unset or has an unrecognized value.
pub fn select_backend() -> TuiBackend {
    parse_backend(std::env::var("WA_TUI_BACKEND").ok().as_deref())
}

/// Parse a backend name string into a `TuiBackend` variant.
fn parse_backend(value: Option<&str>) -> TuiBackend {
    match value {
        Some("ftui" | "frankentui") => TuiBackend::Ftui,
        Some("ratatui" | "legacy") => TuiBackend::Ratatui,
        _ => TuiBackend::STAGE_DEFAULT,
    }
}

/// Launch the TUI with the runtime-selected backend.
///
/// Reads `WA_TUI_BACKEND` to pick ratatui or ftui, then delegates to the
/// appropriate `run_tui` implementation.
pub fn run_tui<Q: QueryClient + Send + Sync + 'static>(
    query_client: Q,
    config: AppConfig,
) -> Result<(), crate::Error> {
    let backend = select_backend();
    tracing::info!(%backend, "TUI backend selected (rollout mode)");

    match backend {
        TuiBackend::Ratatui => super::app::run_tui(query_client, config)
            .map_err(|e| crate::Error::Runtime(format!("TUI (ratatui) error: {e}"))),
        TuiBackend::Ftui => {
            // AppConfig is structurally identical in both backends but they are
            // distinct types.  Convert field-by-field for the ftui path.
            let ftui_config = super::ftui_stub::AppConfig {
                refresh_interval: config.refresh_interval,
                debug: config.debug,
            };
            super::ftui_stub::run_tui(query_client, ftui_config)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_default_is_ratatui() {
        assert_eq!(parse_backend(None), TuiBackend::Ratatui);
    }

    #[test]
    fn parse_backend_ftui_explicit() {
        assert_eq!(parse_backend(Some("ftui")), TuiBackend::Ftui);
    }

    #[test]
    fn parse_backend_ratatui_explicit() {
        assert_eq!(parse_backend(Some("ratatui")), TuiBackend::Ratatui);
    }

    #[test]
    fn parse_backend_frankentui_alias() {
        assert_eq!(parse_backend(Some("frankentui")), TuiBackend::Ftui);
    }

    #[test]
    fn parse_backend_legacy_alias() {
        assert_eq!(parse_backend(Some("legacy")), TuiBackend::Ratatui);
    }

    #[test]
    fn parse_backend_unknown_falls_to_default() {
        assert_eq!(parse_backend(Some("unknown")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn parse_backend_empty_string_falls_to_default() {
        assert_eq!(parse_backend(Some("")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn backend_display() {
        assert_eq!(TuiBackend::Ratatui.to_string(), "ratatui");
        assert_eq!(TuiBackend::Ftui.to_string(), "ftui");
    }

    #[test]
    fn stage_default_is_ratatui_for_canary() {
        // Stage 1 (Canary): default should be Ratatui.
        // Update this test when advancing to Stage 2.
        assert_eq!(
            TuiBackend::STAGE_DEFAULT,
            TuiBackend::Ratatui,
            "Stage 1 default should be Ratatui (legacy)"
        );
    }
}
