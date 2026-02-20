//! Runtime TUI backend selection for phased rollout (FTUI-09.2).
//!
//! During Stages 1-2 of the ftui migration, this module enables both
//! backends to coexist in a single binary.  The operator selects the
//! active backend via the `FT_TUI_BACKEND` environment variable.
//!
//! | Stage | Default   | Override                     |
//! |-------|-----------|------------------------------|
//! | 0 Dev | compile-time only (`--features tui` or `ftui`) |
//! | 1 Canary | ratatui | `FT_TUI_BACKEND=ftui`      |
//! | 2 Beta   | ftui    | `FT_TUI_BACKEND=ratatui`   |
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

/// Select the TUI backend based on the `FT_TUI_BACKEND` environment variable.
///
/// Returns the stage default if the variable is unset or has an unrecognized value.
pub fn select_backend() -> TuiBackend {
    parse_backend(std::env::var("FT_TUI_BACKEND").ok().as_deref())
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
/// Reads `FT_TUI_BACKEND` to pick ratatui or ftui, then delegates to the
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
        assert_eq!(
            TuiBackend::STAGE_DEFAULT,
            TuiBackend::Ratatui,
            "Stage 1 default should be Ratatui (legacy)"
        );
    }

    #[test]
    fn backend_debug_format() {
        let dbg = format!("{:?}", TuiBackend::Ratatui);
        assert!(dbg.contains("Ratatui"));
        let dbg2 = format!("{:?}", TuiBackend::Ftui);
        assert!(dbg2.contains("Ftui"));
    }

    #[test]
    fn backend_clone() {
        let a = TuiBackend::Ratatui;
        let b = a;
        assert_eq!(a, b);
        let c = TuiBackend::Ftui;
        let d = c;
        assert_eq!(c, d);
    }

    #[test]
    fn backend_partial_eq_symmetry() {
        assert_eq!(TuiBackend::Ratatui, TuiBackend::Ratatui);
        assert_eq!(TuiBackend::Ftui, TuiBackend::Ftui);
        assert_ne!(TuiBackend::Ratatui, TuiBackend::Ftui);
        assert_ne!(TuiBackend::Ftui, TuiBackend::Ratatui);
    }

    #[test]
    fn backend_eq_reflexive() {
        let a = TuiBackend::Ratatui;
        assert!(a == a);
        let b = TuiBackend::Ftui;
        assert!(b == b);
    }

    #[test]
    fn parse_backend_case_sensitive() {
        assert_eq!(parse_backend(Some("FTUI")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("Ftui")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("RATATUI")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("Ratatui")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("LEGACY")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("FRANKENTUI")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn parse_backend_whitespace_not_trimmed() {
        assert_eq!(parse_backend(Some(" ftui")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("ftui ")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some(" ratatui ")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn parse_backend_numeric_input() {
        assert_eq!(parse_backend(Some("0")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("1")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("42")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn parse_backend_special_characters() {
        assert_eq!(parse_backend(Some("ftui\n")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("ftui\0")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("ratatui\t")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn display_matches_parse_roundtrip() {
        let display = TuiBackend::Ratatui.to_string();
        assert_eq!(parse_backend(Some(&display)), TuiBackend::Ratatui);
        let display = TuiBackend::Ftui.to_string();
        assert_eq!(parse_backend(Some(&display)), TuiBackend::Ftui);
    }

    #[test]
    fn all_recognized_values() {
        let ftui_inputs = ["ftui", "frankentui"];
        let ratatui_inputs = ["ratatui", "legacy"];
        for input in ftui_inputs {
            assert_eq!(
                parse_backend(Some(input)),
                TuiBackend::Ftui,
                "expected Ftui for input: {input}"
            );
        }
        for input in ratatui_inputs {
            assert_eq!(
                parse_backend(Some(input)),
                TuiBackend::Ratatui,
                "expected Ratatui for input: {input}"
            );
        }
    }

    #[test]
    fn stage_default_is_a_valid_variant() {
        assert!(
            TuiBackend::STAGE_DEFAULT == TuiBackend::Ratatui
                || TuiBackend::STAGE_DEFAULT == TuiBackend::Ftui
        );
    }

    // -- Additional coverage: rollout expanded tests --

    #[test]
    fn backend_ne_is_symmetric() {
        assert_ne!(TuiBackend::Ratatui, TuiBackend::Ftui);
        assert_ne!(TuiBackend::Ftui, TuiBackend::Ratatui);
    }

    #[test]
    fn all_variants_have_distinct_display() {
        let ratatui_str = TuiBackend::Ratatui.to_string();
        let ftui_str = TuiBackend::Ftui.to_string();
        assert_ne!(ratatui_str, ftui_str);
    }

    #[test]
    fn copy_semantics_preserve_value() {
        let a = TuiBackend::Ftui;
        let b = a;
        let c = b;
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(a, c);
    }

    #[test]
    fn parse_backend_deterministic_across_calls() {
        for _ in 0..10 {
            assert_eq!(parse_backend(Some("ftui")), TuiBackend::Ftui);
            assert_eq!(parse_backend(Some("ratatui")), TuiBackend::Ratatui);
            assert_eq!(parse_backend(None), TuiBackend::STAGE_DEFAULT);
        }
    }

    #[test]
    fn parse_backend_with_hyphen() {
        assert_eq!(
            parse_backend(Some("franken-tui")),
            TuiBackend::STAGE_DEFAULT
        );
        assert_eq!(parse_backend(Some("rata-tui")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn parse_backend_with_underscore() {
        assert_eq!(
            parse_backend(Some("franken_tui")),
            TuiBackend::STAGE_DEFAULT
        );
        assert_eq!(parse_backend(Some("rata_tui")), TuiBackend::STAGE_DEFAULT);
    }

    #[test]
    fn parse_backend_similar_but_wrong() {
        assert_eq!(parse_backend(Some("ftui2")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("ratatuis")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(parse_backend(Some("legacymode")), TuiBackend::STAGE_DEFAULT);
        assert_eq!(
            parse_backend(Some("frankentui1")),
            TuiBackend::STAGE_DEFAULT
        );
    }

    #[test]
    fn debug_format_contains_variant_name() {
        assert!(format!("{:?}", TuiBackend::Ratatui).contains("Ratatui"));
        assert!(format!("{:?}", TuiBackend::Ftui).contains("Ftui"));
    }

    #[test]
    fn display_is_lowercase() {
        let ratatui_display = TuiBackend::Ratatui.to_string();
        assert_eq!(ratatui_display, ratatui_display.to_lowercase());
        let ftui_display = TuiBackend::Ftui.to_string();
        assert_eq!(ftui_display, ftui_display.to_lowercase());
    }

    #[test]
    fn parse_backend_none_vs_empty_equivalent() {
        assert_eq!(parse_backend(None), parse_backend(Some("")));
    }
}
