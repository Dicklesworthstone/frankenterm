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

use anyhow::Context;
use clap::{Parser, ValueHint};
use std::ffi::OsString;
use std::path::PathBuf;

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
}

fn parse_config_override(s: &str) -> Result<(String, String), String> {
    let eq_pos = s.find('=').ok_or_else(|| {
        format!("expected 'name=value', got '{s}'")
    })?;
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

    // Validate vendored crate integration by exercising key types.
    // This proves the vendored mux/term/config crates are linked correctly.
    validate_vendored_integration()
        .context("vendored crate integration check failed")?;

    if opts.skip_config {
        tracing::info!("configuration loading skipped (--skip-config)");
    }

    // The full GUI startup sequence will be wired in ft-1memj.3 once the
    // window and font crates are vendored (ft-1memj.2). For now, print
    // a status message and exit cleanly.
    tracing::info!(
        "FrankenTerm GUI bootstrap complete. \
         Window/font crates pending (ft-1memj.2). \
         Full GUI pending (ft-1memj.3)."
    );

    eprintln!(
        "frankenterm-gui v{}: bootstrap mode\n\
         Vendored crate integration: OK\n\
         GUI rendering: not yet available (pending ft-1memj.2 + ft-1memj.3)\n\
         \n\
         This binary proves that the frankenterm-gui crate compiles and links \
         against the vendored mux/term/config crates. The next steps are:\n\
         1. Vendor window/font/client crates (ft-1memj.2)\n\
         2. Wire minimal GUI window (ft-1memj.3)",
        env!("CARGO_PKG_VERSION"),
    );

    Ok(())
}

/// Validate that vendored crates are linked and functional by exercising
/// key types from mux, term, config, and codec.
fn validate_vendored_integration() -> anyhow::Result<()> {
    // Exercise config types
    let _config_key_assignment = config::keyassignment::KeyAssignment::Nop;
    tracing::debug!("config crate: key assignment types accessible");

    // Exercise termwiz types
    let _cell_attrs = termwiz::cell::CellAttributes::default();
    tracing::debug!("termwiz crate: cell attributes accessible");

    // Exercise codec types
    let _pdu = codec::Pdu::Ping(codec::Ping {});
    tracing::debug!("codec crate: PDU types accessible");

    // Exercise mux domain types
    let _domain_type = mux::domain::DomainState::Detached;
    tracing::debug!("mux crate: domain types accessible");

    // Exercise rangeset
    let mut rs = rangeset::RangeSet::new();
    rs.add_range(0..10);
    assert!(rs.contains(5));
    tracing::debug!("rangeset crate: range operations functional");

    // Exercise escape parser — verify the crate is linked
    let _esc = frankenterm_escape_parser::Esc::Unspecified { intermediate: None, control: b'c' };
    tracing::debug!("escape-parser crate: Esc types accessible");

    tracing::info!("all vendored crate integration checks passed");
    Ok(())
}
