//! FrankenTerm GUI — custom terminal emulator binary.
//!
//! This binary will become the primary FrankenTerm terminal emulator, using:
//! - Vendored mux/term/config crates (modified from WezTerm) for terminal backend
//! - frankenterm-core for pattern detection, storage, policy engine
//! - Native event emission to `ft watch` via WaEventSink socket
//! - TOML-native config (Lua optional)
//!
//! Current status: bootstrap skeleton (ft-1memj.1).
//! Next steps:
//! - ft-1memj.2: Vendor window/font/client crates from legacy_wezterm
//! - ft-1memj.3: Wire up minimal GUI that opens a terminal window

use anyhow::{Context, anyhow};
use clap::{Parser, ValueEnum, ValueHint};
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// FrankenTerm GUI — swarm-native terminal emulator
#[derive(Debug, Parser)]
#[command(
    name = "frankenterm-gui",
    about = "FrankenTerm — Swarm-Native Terminal Emulator",
    version = env!("CARGO_PKG_VERSION"),
)]
struct Opt {
    /// Skip loading configuration
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use
    #[arg(
        long = "config-file",
        value_parser,
        conflicts_with = "skip_config",
        value_hint = ValueHint::FilePath,
    )]
    config_file: Option<OsString>,

    /// Override specific configuration values (key=value)
    #[arg(
        long = "config",
        name = "name=value",
        value_parser = parse_config_override,
        number_of_values = 1,
    )]
    config_override: Vec<(String, String)>,

    /// Working directory for the initial pane
    #[arg(long, value_hint = ValueHint::DirPath)]
    cwd: Option<PathBuf>,

    /// Program and arguments to run in the initial pane
    #[arg(trailing_var_arg = true)]
    prog: Vec<OsString>,

    /// Emit bootstrap report in text (default) or JSON
    #[arg(long, value_enum, default_value_t = BootstrapReportFormat::Text)]
    bootstrap_report: BootstrapReportFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BootstrapReportFormat {
    Text,
    Json,
}

#[derive(Debug, Serialize)]
struct BootstrapReport {
    mode: &'static str,
    version: &'static str,
    config: ConfigResolutionReport,
    cwd: Option<String>,
    program: Vec<String>,
    overrides: Vec<String>,
    vendored_checks_passed: bool,
    pending_beads: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ConfigResolutionReport {
    loading: &'static str,
    source: &'static str,
    selected_path: Option<String>,
    file_exists: bool,
    searched_paths: Vec<String>,
}

fn parse_config_override(s: &str) -> Result<(String, String), String> {
    let eq_pos = s
        .find('=')
        .ok_or_else(|| format!("expected 'name=value', got '{s}'"))?;
    Ok((s[..eq_pos].to_string(), s[eq_pos + 1..].to_string()))
}

fn main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let opts = Opt::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("FrankenTerm GUI starting");

    let config_report = resolve_config_report(&opts);
    validate_bootstrap_inputs(&opts, &config_report)?;

    // Validate vendored crate integration by exercising key types.
    // This proves the vendored mux/term/config crates are linked correctly.
    validate_vendored_integration().context("vendored crate integration check failed")?;

    let report = build_bootstrap_report(&opts, config_report);
    emit_bootstrap_report(&report, opts.bootstrap_report)?;

    tracing::info!(
        mode = report.mode,
        config_source = report.config.source,
        config_selected = report.config.selected_path.as_deref().unwrap_or("none"),
        "FrankenTerm GUI bootstrap complete",
    );

    Ok(())
}

fn resolve_config_report(opts: &Opt) -> ConfigResolutionReport {
    if opts.skip_config {
        return ConfigResolutionReport {
            loading: "skipped",
            source: "none",
            selected_path: None,
            file_exists: false,
            searched_paths: Vec::new(),
        };
    }

    if let Some(config_file) = opts.config_file.as_ref() {
        let path = PathBuf::from(config_file);
        let exists = path.is_file();
        let path_str = path_to_string(&path);
        return ConfigResolutionReport {
            loading: "enabled",
            source: "explicit",
            selected_path: Some(path_str.clone()),
            file_exists: exists,
            searched_paths: vec![path_str],
        };
    }

    let candidates = default_config_candidates();
    let selected = candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned();

    ConfigResolutionReport {
        loading: "enabled",
        source: if selected.is_some() { "auto" } else { "none" },
        selected_path: selected.as_ref().map(|path| path_to_string(path)),
        file_exists: selected.is_some(),
        searched_paths: candidates.iter().map(|path| path_to_string(path)).collect(),
    }
}

fn default_config_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let root = PathBuf::from(xdg);
        candidates.push(root.join("frankenterm").join("frankenterm.toml"));
        candidates.push(root.join("wezterm").join("wezterm.lua"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let root = PathBuf::from(home).join(".config");
        candidates.push(root.join("frankenterm").join("frankenterm.toml"));
        candidates.push(root.join("wezterm").join("wezterm.lua"));
    }
    dedupe_paths(candidates)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::with_capacity(paths.len());
    for path in paths {
        if !deduped.contains(&path) {
            deduped.push(path);
        }
    }
    deduped
}

fn validate_bootstrap_inputs(opts: &Opt, config: &ConfigResolutionReport) -> anyhow::Result<()> {
    if let Some(cwd) = opts.cwd.as_ref() {
        if !cwd.exists() {
            return Err(anyhow!("--cwd path does not exist: {}", cwd.display()));
        }
        if !cwd.is_dir() {
            return Err(anyhow!("--cwd path is not a directory: {}", cwd.display()));
        }
    }

    if config.loading == "enabled" && config.source == "explicit" && !config.file_exists {
        return Err(anyhow!(
            "explicit config path does not exist or is not a file: {}",
            config.selected_path.as_deref().unwrap_or("<unknown>")
        ));
    }

    Ok(())
}

fn build_bootstrap_report(opts: &Opt, config: ConfigResolutionReport) -> BootstrapReport {
    BootstrapReport {
        mode: "bootstrap",
        version: env!("CARGO_PKG_VERSION"),
        config,
        cwd: opts.cwd.as_deref().map(path_to_string),
        program: opts.prog.iter().map(os_string_to_utf8).collect(),
        overrides: opts
            .config_override
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect(),
        vendored_checks_passed: true,
        pending_beads: vec!["ft-1memj.2", "ft-1memj.3"],
    }
}

fn emit_bootstrap_report(
    report: &BootstrapReport,
    format: BootstrapReportFormat,
) -> anyhow::Result<()> {
    match format {
        BootstrapReportFormat::Json => {
            let json = serde_json::to_string_pretty(report)
                .context("failed to serialize bootstrap report as JSON")?;
            println!("{json}");
        }
        BootstrapReportFormat::Text => {
            let config_selected = report.config.selected_path.as_deref().unwrap_or("none");
            eprintln!(
                "frankenterm-gui v{}: bootstrap mode\n\
                 Vendored crate integration: OK\n\
                 Config loading: {} ({})\n\
                 Config path: {}\n\
                 GUI rendering: not yet available (pending ft-1memj.2 + ft-1memj.3)\n\
                 \n\
                 This binary proves that the frankenterm-gui crate compiles and links \
                 against the vendored mux/term/config crates. The next steps are:\n\
                 1. Vendor window/font/client crates (ft-1memj.2)\n\
                 2. Wire minimal GUI window (ft-1memj.3)",
                report.version, report.config.loading, report.config.source, config_selected
            );
        }
    }
    Ok(())
}

fn path_to_string(path: &Path) -> String {
    path.display().to_string()
}

fn os_string_to_utf8(value: &OsString) -> String {
    value.to_string_lossy().into_owned()
}

/// Validate that vendored crates are linked and functional by exercising
/// key types from mux, term, config, and codec.
fn validate_vendored_integration() -> anyhow::Result<()> {
    // Exercise config types
    assert!(
        matches!(
            config::keyassignment::KeyAssignment::Nop,
            config::keyassignment::KeyAssignment::Nop
        ),
        "config crate: KeyAssignment enum must be accessible"
    );
    tracing::debug!("config crate: key assignment types accessible");

    // Exercise termwiz types
    let _ = termwiz::cell::CellAttributes::default();
    tracing::debug!("termwiz crate: cell attributes accessible");

    // Exercise codec types
    let _ = codec::Pdu::Ping(codec::Ping {});
    tracing::debug!("codec crate: PDU types accessible");

    // Exercise mux domain types
    assert!(
        matches!(
            mux::domain::DomainState::Detached,
            mux::domain::DomainState::Detached
        ),
        "mux crate: DomainState enum must be accessible"
    );
    tracing::debug!("mux crate: domain types accessible");

    // Exercise rangeset
    let mut rs = rangeset::RangeSet::new();
    rs.add_range(0..10);
    assert!(rs.contains(5), "rangeset: range operations must work");
    tracing::debug!("rangeset crate: range operations functional");

    // Exercise escape parser — verify the crate is linked
    let _ = frankenterm_escape_parser::Esc::Unspecified {
        intermediate: None,
        control: b'c',
    };
    tracing::debug!("escape-parser crate: Esc types accessible");

    tracing::info!("all vendored crate integration checks passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_override_accepts_key_value() {
        let parsed = parse_config_override("foo=bar").expect("key=value should parse");
        assert_eq!(parsed.0, "foo");
        assert_eq!(parsed.1, "bar");
    }

    #[test]
    fn parse_config_override_rejects_missing_separator() {
        let err = parse_config_override("foobar").expect_err("missing '=' must fail");
        assert!(err.contains("expected 'name=value'"));
    }

    #[test]
    fn dedupe_paths_preserves_first_occurrence() {
        let paths = vec![
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/b"),
            PathBuf::from("/tmp/a"),
        ];
        let deduped = dedupe_paths(paths);
        assert_eq!(
            deduped,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
    }
}
