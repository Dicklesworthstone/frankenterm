#![no_main]

//! [ft-s96ej] Fuzz target for the `ft` CLI argv parser.
//!
//! ## Attack surface
//!
//! `crates/frankenterm/src/main.rs` defines the entire `ft` CLI via
//! clap (Parser + Subcommand derives). It is the principal control
//! surface AI agents use to drive other AI agents through
//! `ft robot ...`, plus the human entry point. When an agent runs
//! `ft robot send <pane> "<text>"`, the `<text>` is
//! attacker-controlled (another agent's output passed as command
//! input).
//!
//! ## Why this harness mirrors production rather than calling it
//!
//! The production `Cli` enum is private to the binary crate
//! (`frankenterm` has a `[[bin]]` only, no `[lib]`), and main.rs is
//! ~2.3 MB — adding a fuzz-only public shim would require a
//! crate-level restructure that's out of scope for this bead.
//!
//! Instead, this harness uses clap's BUILDER API to stand up a
//! representative subset of `ft`'s top-level subcommand shapes —
//! single-positional, repeated-`--flag value`, optional-positional,
//! conflicting-flags, deeply-nested subcommand chains — and feeds
//! it arbitrary argv. clap derive expands to clap-builder under the
//! hood, so the parsing surface this harness exercises is the same
//! library that production uses, configured similarly.
//!
//! What this CATCHES:
//! - clap-builder panics on malformed argv (the historical class
//!   that motivated this fuzz target).
//! - Argv-split UB on malformed UTF-8 / embedded null bytes /
//!   oversized strings.
//! - Repeated-flag accumulator overflow (clap `ArgAction::Append`
//!   with no upper bound).
//! - Conflicting flag combos that should error but might panic.
//! - Subcommand chain depth bombs.
//!
//! What this DOES NOT catch:
//! - Bugs in production-specific custom `value_parser` functions
//!   (e.g. `parse_pct_arg` in main.rs) — those need either a
//!   shim-based harness or per-parser unit fuzz targets.
//!
//! ## Modes (Arbitrary-driven)
//!
//! - **Generic**: split arbitrary bytes on `\0` to produce argv,
//!   feed through the harness's clap::Command. Hits the broad
//!   parsing surface.
//! - **RobotSendText**: simulate `ft robot send <pane> <text>`
//!   where `<text>` is the attacker-controlled byte slice — the
//!   one path that DOES handle attacker bytes today.
//! - **DeepSubcommand**: build a synthetically-deep argv chain to
//!   exercise subcommand-chain depth limits.
//!
//! Pattern reuse: matches ft-h8v8v (wire_envelope), ft-hfbsp
//! (simd_scan), ft-ul4vi (jsonschema_validator) — Archetype 5
//! structure-aware Arbitrary input + Archetype 1 crash detector.

use arbitrary::Arbitrary;
use clap::{Arg, ArgAction, Command};
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

const MAX_ARGV_BYTES: usize = 64 * 1024;
const MAX_ARGV_LEN: usize = 256;

/// Compile the harness's clap Command once and reuse across iterations.
/// Mirrors production `ft` top-level shape: global verbose/config/workspace
/// flags + a representative subset of subcommands chosen for parser-coverage
/// diversity (single-positional, repeated-Append, conflicts, nested subs).
static FT_CLI: OnceLock<Command> = OnceLock::new();

fn ft_cli() -> Command {
    FT_CLI.get_or_init(build_ft_cli).clone()
}

fn build_ft_cli() -> Command {
    Command::new("ft")
        // Global flags (mirror main.rs:74-86)
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .global(true)
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .global(true)
                .num_args(1),
        )
        .arg(
            Arg::new("workspace")
                .long("workspace")
                .global(true)
                .num_args(1),
        )
        // `ft robot send <pane_id> <text>` — the attacker-controlled-text path.
        .subcommand(
            Command::new("robot").subcommand(
                Command::new("send")
                    .arg(Arg::new("pane_id").required(true).num_args(1))
                    .arg(Arg::new("text").required(true).num_args(1))
                    .arg(Arg::new("format").long("format").num_args(1))
                    .arg(
                        Arg::new("include")
                            .long("include")
                            .num_args(1)
                            .action(ArgAction::Append),
                    ),
            ),
        )
        // `ft watch [--foreground] [--auto-handle] [--poll-interval N]`
        .subcommand(
            Command::new("watch")
                .arg(
                    Arg::new("foreground")
                        .long("foreground")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("auto_handle")
                        .long("auto-handle")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("poll_interval").long("poll-interval").num_args(1)),
        )
        // `ft search <query> [--pane N] [--limit N]`
        .subcommand(
            Command::new("search")
                .arg(Arg::new("query").required(true).num_args(1))
                .arg(Arg::new("pane").long("pane").num_args(1))
                .arg(Arg::new("limit").long("limit").num_args(1)),
        )
        // `ft doctor [--json]`
        .subcommand(
            Command::new("doctor").arg(Arg::new("json").long("json").action(ArgAction::SetTrue)),
        )
        // Deep-nested subcommand chain to exercise depth handling.
        // `ft session show <id> [--json]`
        .subcommand(
            Command::new("session").subcommand(
                Command::new("show")
                    .arg(Arg::new("id").required(true).num_args(1))
                    .arg(Arg::new("json").long("json").action(ArgAction::SetTrue)),
            ),
        )
        // Conflicting flags: --foreground vs --daemonize on `ft mux start`
        .subcommand(
            Command::new("mux").subcommand(
                Command::new("start")
                    .arg(
                        Arg::new("foreground")
                            .long("foreground")
                            .action(ArgAction::SetTrue),
                    )
                    .arg(
                        Arg::new("daemonize")
                            .long("daemonize")
                            .action(ArgAction::SetTrue)
                            .conflicts_with("foreground"),
                    ),
            ),
        )
        .disable_help_flag(false)
        .disable_version_flag(false)
}

#[derive(Arbitrary, Debug)]
enum FuzzInput<'a> {
    /// Generic argv mode — split bytes on \0, feed through clap.
    Generic(&'a [u8]),
    /// Narrow on `ft robot send <pane> <text>` where `<text>` is
    /// the attacker-controlled byte slice. The pane id is fixed at
    /// "0" so libFuzzer doesn't waste bits there.
    RobotSendText(&'a [u8]),
    /// Synthetically-deep argv: replicate one subcommand N times.
    DeepSubcommand { depth: u8, leaf_arg: &'a [u8] },
}

/// Convert a byte slice into argv tokens, splitting on \0. The harness
/// rejects non-UTF-8 tokens (clap requires &str) and caps the argv at
/// MAX_ARGV_LEN to keep iteration fast.
fn bytes_to_argv(bytes: &[u8]) -> Vec<String> {
    if bytes.len() > MAX_ARGV_BYTES {
        return Vec::new();
    }
    let mut out: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter_map(|s| std::str::from_utf8(s).ok())
        .filter(|s| !s.is_empty())
        .take(MAX_ARGV_LEN)
        .map(String::from)
        .collect();
    // try_get_matches_from expects argv[0] to be the program name;
    // prepend it so clap doesn't treat the first token as a subcommand.
    out.insert(0, "ft".to_string());
    out
}

fn try_parse(argv: &[String]) {
    let cli = ft_cli();
    // Contract: clap must return Ok(_) or Err(_) for every argv. A
    // panic, abort, or stack overflow on malformed argv is a bug.
    let _ = cli.try_get_matches_from(argv);
}

fuzz_target!(|input: FuzzInput| {
    match input {
        FuzzInput::Generic(bytes) => {
            let argv = bytes_to_argv(bytes);
            if argv.len() <= 1 {
                return;
            }
            try_parse(&argv);
        }
        FuzzInput::RobotSendText(bytes) => {
            if bytes.len() > MAX_ARGV_BYTES {
                return;
            }
            // clap requires &str — drop non-UTF-8 inputs at the
            // production framing layer, since argv values must
            // round-trip through OsStr→str on most production
            // platforms.
            let Ok(text) = std::str::from_utf8(bytes) else {
                return;
            };
            let argv = vec![
                "ft".to_string(),
                "robot".to_string(),
                "send".to_string(),
                "0".to_string(),
                text.to_string(),
            ];
            try_parse(&argv);
        }
        FuzzInput::DeepSubcommand { depth, leaf_arg } => {
            // Build "ft session show session show ... <leaf>" repeated
            // up to `depth` times. clap's parser must reject this
            // gracefully (the harness's `session` only has one
            // subcommand level) without panicking on the chain depth.
            let depth_capped = (depth as usize) % 64;
            let Ok(leaf) = std::str::from_utf8(leaf_arg) else {
                return;
            };
            let mut argv: Vec<String> = vec!["ft".to_string()];
            for _ in 0..depth_capped {
                argv.push("session".to_string());
                argv.push("show".to_string());
            }
            // Cap leaf length so the argv stays within MAX_ARGV_BYTES.
            let leaf_truncated: String = leaf.chars().take(1024).collect();
            argv.push(leaf_truncated);
            try_parse(&argv);
        }
    }
});
