//! Fail-closed orphan-cleanup surface.
//!
//! FrankenTerm historically searched the global process table for old
//! `wezterm cli` command lines and sent `KILL` to the resulting numeric PIDs.
//! A command-line match is not proof that FrankenTerm created the process, and
//! a PID can be recycled between discovery and signalling.  The mechanism is
//! therefore intentionally inert until subprocesses are registered by owned
//! child handle plus immutable process identity.
//!
//! Per-command timeout supervisors remain responsible for children they own.
//! This module never enumerates or signals processes.  The retained report and
//! entry points let existing callers fail closed while the handle-owned child
//! registry tracked by `ft-interactive-systems-performance-4tenz.50.2` is
//! implemented.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::CliConfig;

/// Historical short-lived `wezterm cli` classifier retained only in tests.
///
/// It demonstrates why command-line classification is insufficient authority:
/// even a positive match must never result in a process signal.
#[cfg(test)]
const REAPABLE_SUBCOMMANDS: &[&str] = &[
    "list",
    "get-text",
    "send-text",
    "spawn",
    "split-pane",
    "activate-pane",
    "kill-pane",
    "zoom-pane",
    "list-clients",
    "get-pane-direction",
];

/// Historical value-taking flags used by the test-only classifier.
#[cfg(test)]
const PRE_SUBCOMMAND_VALUE_FLAGS: &[&str] = &["--config-file", "--config", "--class"];

/// Summary of a single orphan-reaper scan cycle.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReapReport {
    /// Number of processes scanned. Always zero while global scanning is disabled.
    pub scanned: usize,
    /// Number of processes killed. Always zero without handle-owned authority.
    pub killed: usize,
    /// PIDs killed through owned handles. Empty until that registry exists.
    pub killed_pids: Vec<u32>,
    /// Stable reasons that cleanup did not run, including cancellation.
    pub errors: Vec<String>,
}

/// A test-only process entry parsed from synthetic historical `ps` output.
#[cfg(test)]
#[derive(Debug)]
struct ProcessEntry {
    pid: u32,
    /// Elapsed time in seconds since the process started.
    age_seconds: u64,
    /// The full command line.
    command: String,
}

/// Enter the fail-closed orphan-cleanup surface.
///
/// The function always returns without inspecting or signalling any process.
pub async fn run_orphan_reaper(config: CliConfig, shutdown_flag: Arc<AtomicBool>) {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    run_orphan_reaper_with_cx(&cx, config, shutdown_flag).await;
}

/// Cx-aware fail-closed entry point.
///
/// A configured non-zero interval no longer opts into global process-table
/// scanning.  It emits a content-free warning and returns immediately.
pub async fn run_orphan_reaper_with_cx(
    cx: &crate::cx::Cx,
    config: CliConfig,
    _shutdown_flag: Arc<AtomicBool>,
) {
    if cx.is_cancel_requested() {
        debug!("orphan reaper aborted before first cycle: capability context already cancelled");
        return;
    }

    let interval = config.orphan_reap_interval_seconds;
    if interval == 0 {
        info!("orphan cleanup disabled");
        return;
    }

    warn!(
        configured_interval_seconds = interval,
        "orphan cleanup refused: no handle-owned child identity registry"
    );
}

/// Return a fail-closed cleanup report without inspecting any process.
pub async fn reap_orphans(_max_age_seconds: u64) -> ReapReport {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    reap_orphans_with_cx(&cx, _max_age_seconds).await
}

/// Cx-aware sibling of [`reap_orphans`].
pub async fn reap_orphans_with_cx(cx: &crate::cx::Cx, _max_age_seconds: u64) -> ReapReport {
    let mut report = ReapReport::default();

    if cx.checkpoint().is_err() {
        report.errors.push("reap cancelled before scan".to_string());
        return report;
    }

    report
        .errors
        .push("reap disabled: no handle-owned child identity".to_string());
    report
}

/// Parse a synthetic historical `ps -eo pid,etimes,args` line.
///
/// This is test-only. A positive result conveys no ownership or signal
/// authority and is never consumed by production code.
///
/// Returns `None` for:
/// - Non-wezterm processes
/// - Lines where `wezterm cli` appears only as an argument to a wrapper (grep,
///   shell -c, etc.)
/// - `wezterm cli proxy` and any other non-allowlisted subcommand
#[cfg(test)]
fn parse_ps_line_if_reapable(line: &str) -> Option<ProcessEntry> {
    // Expected format: "  PID  ELAPSED  ARGS..."
    // Fields are whitespace-separated, with ARGS potentially containing spaces.
    let mut parts = line.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let age_seconds: u64 = parts.next()?.parse().ok()?;

    // Collect the remaining tokens as the argument vector.
    let tokens: Vec<&str> = parts.collect();
    if tokens.is_empty() {
        return None;
    }

    // The first token must be a wezterm binary (the last path component must
    // start with "wezterm").  This filters out wrapper processes like:
    //   grep "wezterm cli"
    //   zsh -c "wezterm cli list"
    //   bash /some/script.sh  (that happens to mention wezterm in later args)
    let binary = tokens[0];
    let binary_basename = binary.rsplit('/').next().unwrap_or(binary);
    if !binary_basename.starts_with("wezterm") {
        return None;
    }

    // Expect at least: wezterm cli <subcommand>
    // tokens[0] = "wezterm" (or "/path/to/wezterm")
    // tokens[1] = "cli"     (possibly after flags like --config-file)
    // tokens[N] = the subcommand

    // Find the "cli" token.  WezTerm allows global flags before "cli"
    // (e.g. `wezterm --config-file foo.toml cli list`), so we scan forward.
    let cli_pos = tokens.iter().position(|&t| t == "cli")?;

    // Find the subcommand by walking tokens after `cli` and skipping:
    // - boolean flags like `--prefer-mux`
    // - the value token for known value-taking flags like `--config-file foo`
    let mut idx = cli_pos + 1;
    let subcommand = loop {
        let token = *tokens.get(idx)?;
        if !token.starts_with('-') {
            break token;
        }

        if PRE_SUBCOMMAND_VALUE_FLAGS.contains(&token) {
            idx += 2;
            continue;
        }

        idx += 1;
    };

    // Only reap subcommands on the explicit allowlist.
    if !REAPABLE_SUBCOMMANDS.contains(&subcommand) {
        return None;
    }

    let command = tokens.join(" ");

    Some(ProcessEntry {
        pid,
        age_seconds,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn default_configuration_disables_global_reaping() {
        assert_eq!(CliConfig::default().orphan_reap_interval_seconds, 0);
    }

    #[test]
    fn production_source_has_no_process_scan_or_pid_signal_primitive() {
        let source = include_str!("orphan_reaper.rs");
        let process_scan = ["Command::new(", "\"ps\")"].concat();
        let pid_signal = ["send_unix_signal", "_to_pid"].concat();
        assert!(!source.contains(&process_scan));
        assert!(!source.contains(&pid_signal));
    }

    // Historical classifier cases. Even positive classifications never reach
    // a production scan or signalling path.

    #[test]
    fn accepts_simple_list() {
        let line = "  1234   45 wezterm cli list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert_eq!(entry.pid, 1234);
        assert_eq!(entry.age_seconds, 45);
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_absolute_path() {
        let line = "  5678   90 /usr/bin/wezterm cli get-text --pane-id 3";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert_eq!(entry.pid, 5678);
        assert!(entry.command.contains("get-text"));
    }

    #[test]
    fn accepts_send_text() {
        let line = "  100   120 wezterm cli send-text --pane-id 1 hello world";
        assert!(parse_ps_line_if_reapable(line).is_some());
    }

    #[test]
    fn accepts_spawn() {
        let line = "  200   35 wezterm cli spawn --new-window";
        assert!(parse_ps_line_if_reapable(line).is_some());
    }

    #[test]
    fn accepts_split_pane() {
        let line = "  300   50 /opt/wezterm cli split-pane --right";
        assert!(parse_ps_line_if_reapable(line).is_some());
    }

    #[test]
    fn accepts_with_global_flags() {
        // Global flags before "cli"
        let line = "  400   60 wezterm --config-file /tmp/wez.toml cli list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_with_prefer_mux_flag_before_list() {
        // --prefer-mux is a flag to "cli", subcommand is "list" => reapable
        let line = "  500   70 wezterm cli --prefer-mux list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_with_config_file_flag_after_cli_before_subcommand() {
        let line = "  501   70 wezterm cli --config-file /tmp/wez.toml list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_with_config_override_flag_after_cli_before_subcommand() {
        let line = "  502   70 wezterm cli --config mux.enable_kitty_graphics=true list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_with_class_flag_after_cli_before_subcommand() {
        let line = "  503   70 wezterm cli --class agent-fleet list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_with_multiple_value_taking_flags_after_cli_before_subcommand() {
        let line =
            "  504   70 wezterm cli --config-file /tmp/wez.toml --config mux.enabled=true list";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("list"));
    }

    #[test]
    fn accepts_when_value_taking_flag_value_matches_reapable_subcommand_name() {
        let line = "  505   70 wezterm cli --config-file list get-text --pane-id 1";
        let entry = parse_ps_line_if_reapable(line).expect("should match");
        assert!(entry.command.contains("get-text"));
    }

    #[test]
    fn accepts_all_allowlisted_subcommands() {
        for sub in REAPABLE_SUBCOMMANDS {
            let line = format!("  999   40 wezterm cli {sub}");
            assert!(
                parse_ps_line_if_reapable(&line).is_some(),
                "subcommand '{sub}' should be accepted"
            );
        }
    }

    // -- Negative cases: must NOT be accepted --

    #[test]
    fn rejects_proxy() {
        let line = "  1000   500 wezterm cli proxy";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_prefer_mux_proxy() {
        let line = "  1001   600 wezterm cli --prefer-mux proxy";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_grep_containing_wezterm_cli() {
        let line = r"  2000   10 grep wezterm cli list";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_shell_wrapper() {
        let line = r"  2001   10 zsh -c wezterm cli list";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_bash_wrapper() {
        let line = r"  2002   10 bash -c wezterm cli list";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let line = "  3000   40 wezterm cli some-future-cmd";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_non_wezterm_binary() {
        let line = "  4000   40 notwezterm cli list";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_header_line() {
        let line = "  PID ELAPSED COMMAND";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_empty_line() {
        assert!(parse_ps_line_if_reapable("").is_none());
    }

    #[test]
    fn rejects_wezterm_without_cli() {
        let line = "  5000   40 wezterm start --always-new-process";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_wezterm_cli_without_subcommand() {
        let line = "  6000   40 wezterm cli";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_wezterm_cli_with_only_flags() {
        let line = "  6001   40 wezterm cli --prefer-mux";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    #[test]
    fn rejects_value_taking_flag_without_subcommand() {
        let line = "  6002   40 wezterm cli --config-file /tmp/wez.toml";
        assert!(parse_ps_line_if_reapable(line).is_none());
    }

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for the Cx-first fail-closed surface.
    // -------------------------------------------------------------------------

    mod labruntime_orphan_reaper {
        use super::*;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime got stuck; termination: {:?}",
                report.termination,
            );
        }

        /// Pre-flight cancellation returns before reading configuration.
        #[test]
        fn run_orphan_reaper_with_cx_pre_cancelled_exits_immediately() {
            run_lab(0x0_1FEA_BED5_0505, || async move {
                let config = CliConfig::default();
                let shutdown = Arc::new(AtomicBool::new(false));

                let budget = crate::cx::Budget::new().with_poll_quota(0);
                let cx = crate::cx::Cx::for_testing_with_budget(budget);
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("ft-xbnl0.2.x orphan_reaper precancel"),
                );

                run_orphan_reaper_with_cx(&cx, config, Arc::clone(&shutdown)).await;

                assert!(
                    !shutdown.load(Ordering::SeqCst),
                    "run_with_cx must return via the Cx path, not the shutdown flag"
                );
            });
        }

        /// Interval=0 disables the surface and returns immediately.
        #[test]
        fn run_orphan_reaper_with_cx_interval_zero_disables() {
            run_lab(0x0_1FEA_BED5_0606, || async move {
                let config = CliConfig {
                    orphan_reap_interval_seconds: 0,
                    ..Default::default()
                };
                let shutdown = Arc::new(AtomicBool::new(false));
                let cx = crate::cx::for_request();

                run_orphan_reaper_with_cx(&cx, config, Arc::clone(&shutdown)).await;

                assert!(
                    !shutdown.load(Ordering::SeqCst),
                    "interval=0 must not touch the shutdown flag"
                );
            });
        }

        #[test]
        fn configured_interval_still_cannot_scan_or_signal() {
            run_lab(0x0_1FEA_BED5_0666, || async move {
                let config = CliConfig {
                    orphan_reap_interval_seconds: 1,
                    ..Default::default()
                };
                let shutdown = Arc::new(AtomicBool::new(false));
                let cx = crate::cx::for_request();

                run_orphan_reaper_with_cx(&cx, config, Arc::clone(&shutdown)).await;

                let report = reap_orphans_with_cx(&cx, 0).await;
                assert_eq!(report.scanned, 0);
                assert_eq!(report.killed, 0);
                assert!(report.killed_pids.is_empty());
                assert_eq!(
                    report.errors,
                    vec!["reap disabled: no handle-owned child identity".to_string()]
                );
            });
        }

        /// ft-xbnl0.2.3 Cx-first: `reap_orphans_with_cx` must return a
        /// report with "cancelled" in the errors when given a
        /// pre-cancelled cx, without scanning any processes.
        #[test]
        fn reap_orphans_with_precancelled_cx_returns_empty_report_with_cancel_error() {
            run_lab(0x0_1FEA_BED5_0707, || async move {
                let budget = crate::cx::Budget::new().with_poll_quota(0);
                let cx = crate::cx::Cx::for_testing_with_budget(budget);
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("ft-xbnl0.2.x reap_orphans precancel"),
                );

                let report = reap_orphans_with_cx(&cx, 60).await;

                assert_eq!(report.scanned, 0, "pre-cancelled cx must skip scan");
                assert_eq!(report.killed, 0);
                assert!(
                    report.errors.iter().any(|e| e.contains("cancelled")),
                    "errors must include cancellation reason: {:?}",
                    report.errors
                );
            });
        }
    }
}
