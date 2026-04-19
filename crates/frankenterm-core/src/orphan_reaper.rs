//! Orphan reaper for stuck `wezterm cli` helper processes.
//!
//! FrankenTerm spawns short-lived `wezterm cli <subcommand>` processes to query
//! and control the WezTerm mux backend.  These can hang due to lock contention,
//! socket timeouts, or notification feedback loops.  The orphan reaper
//! periodically scans for such processes and kills any that exceed a
//! configurable age threshold.
//!
//! # Proxy safety
//!
//! `wezterm cli --prefer-mux proxy` (and other `proxy` invocations) are
//! **long-lived SSH session transport processes**.  Killing them severs active
//! SSH sessions.  The reaper therefore maintains an explicit allowlist of
//! short-lived helper subcommands and **never** touches `proxy` or any
//! unrecognized subcommand.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::CliConfig;

/// Short-lived `wezterm cli` subcommands that FrankenTerm spawns and that are
/// safe to reap when they exceed the age threshold.
///
/// This allowlist exists because `wezterm cli` also has long-lived subcommands
/// (notably `proxy`, used as SSH session transport via `--prefer-mux proxy`)
/// that must NEVER be killed.  Rather than trying to enumerate dangerous
/// subcommands (a fragile denylist), we enumerate only the ones we spawn.
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

/// Summary of a single orphan-reaper scan cycle.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReapReport {
    /// Number of candidate processes scanned.
    pub scanned: usize,
    /// Number of orphan processes successfully killed.
    pub killed: usize,
    /// PIDs that were successfully killed.
    pub killed_pids: Vec<u32>,
    /// Errors encountered while scanning or killing processes.
    pub errors: Vec<String>,
}

/// A process entry parsed from `ps` output.
#[derive(Debug)]
struct ProcessEntry {
    pid: u32,
    /// Elapsed time in seconds since the process started.
    age_seconds: u64,
    /// The full command line.
    command: String,
}

/// Run the orphan reaper loop.  Returns when `shutdown_flag` is set or the
/// reap interval is configured to zero (disabled).
pub async fn run_orphan_reaper(config: CliConfig, shutdown_flag: Arc<AtomicBool>) {
    let interval = config.orphan_reap_interval_seconds;
    if interval == 0 {
        info!("orphan reaper disabled (orphan_reap_interval_seconds = 0)");
        return;
    }

    let max_age = config.orphan_max_age_seconds;
    info!(
        interval_s = interval,
        max_age_s = max_age,
        "orphan reaper started"
    );

    // ft-xbnl0.2.3 tick 292: cx-first orphan reaper loop sleep.
    let reaper_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    loop {
        if crate::runtime_compat::sleep_with_cx(&reaper_cx, Duration::from_secs(interval))
            .await
            .is_err()
        {
            debug!("orphan reaper cancelled via cx");
            return;
        }

        if shutdown_flag.load(Ordering::Relaxed) {
            debug!("orphan reaper shutting down");
            return;
        }

        let report = reap_orphans(max_age).await;
        if report.killed > 0 {
            info!(
                scanned = report.scanned,
                killed = report.killed,
                pids = ?report.killed_pids,
                "orphan reaper cycle complete"
            );
        } else {
            debug!(scanned = report.scanned, "orphan reaper cycle — no orphans");
        }

        for err in &report.errors {
            warn!(error = %err, "orphan reaper error during scan");
        }
    }
}

/// Run the orphan reaper loop against the caller's asupersync capability
/// context (ft-xbnl0.2.x Cx-first entry point).
///
/// Short-circuits before the first reap if `cx` is already cancelled
/// — an operator who has abandoned the watch daemon should not trigger
/// a final orphan scan. Otherwise each inter-cycle sleep is bound via
/// [`crate::runtime_compat::sleep_with_cx`], so budget-driven
/// cancellation from the outer scope cuts the sleep deterministically
/// under `LabRuntime` virtual time. Both the `shutdown_flag` and
/// `cx.is_cancel_requested()` are checked each iteration so either
/// cancellation path terminates the loop promptly without waiting on
/// the full reap interval.
///
/// The legacy [`run_orphan_reaper`] entry point is preserved for
/// non-migrated callers; this is strictly additive.
#[cfg(feature = "asupersync-runtime")]
pub async fn run_orphan_reaper_with_cx(
    cx: &crate::cx::Cx,
    config: CliConfig,
    shutdown_flag: Arc<AtomicBool>,
) {
    if cx.is_cancel_requested() {
        debug!("orphan reaper aborted before first cycle: capability context already cancelled");
        return;
    }

    let interval = config.orphan_reap_interval_seconds;
    if interval == 0 {
        info!("orphan reaper disabled (orphan_reap_interval_seconds = 0)");
        return;
    }

    let max_age = config.orphan_max_age_seconds;
    info!(
        interval_s = interval,
        max_age_s = max_age,
        "orphan reaper started (Cx-aware)"
    );

    loop {
        // `sleep_with_cx` returns Err on cancellation; treat as
        // "time to exit" so the loop terminates cleanly without a
        // spurious extra reap cycle after cancellation.
        if crate::runtime_compat::sleep_with_cx(cx, Duration::from_secs(interval))
            .await
            .is_err()
        {
            debug!("orphan reaper shutting down: Cx cancelled during sleep");
            return;
        }

        if shutdown_flag.load(Ordering::Relaxed) || cx.is_cancel_requested() {
            debug!("orphan reaper shutting down");
            return;
        }

        // ft-xbnl0.2.3 tick 107: route through the Cx-first reap so
        // mid-cycle cancellation bails before killing every stale pid.
        let report = reap_orphans_with_cx(cx, max_age).await;
        if report.killed > 0 {
            info!(
                scanned = report.scanned,
                killed = report.killed,
                pids = ?report.killed_pids,
                "orphan reaper cycle complete"
            );
        } else {
            debug!(scanned = report.scanned, "orphan reaper cycle — no orphans");
        }

        for err in &report.errors {
            warn!(error = %err, "orphan reaper error during scan");
        }
    }
}

/// Scan for orphaned `wezterm cli` processes and kill those exceeding
/// `max_age_seconds`, returning a serializable cycle report.
pub async fn reap_orphans(max_age_seconds: u64) -> ReapReport {
    scan_and_reap(max_age_seconds).await
}

/// ft-xbnl0.2.3 Cx-first sibling of [`reap_orphans`].
///
/// Threads the caller's cx through the reap cycle:
/// - Pre-flight checkpoint gates entry before `ps` listing.
/// - Per-kill checkpoint between iterations lets a cancelled
///   caller stop partway through killing a long list of stale
///   processes, returning a partial report.
///   Scan errors and individual kill errors are captured in the
///   report (not short-circuited) so the caller always gets a
///   coherent snapshot of what did get reaped.
#[cfg(feature = "asupersync-runtime")]
pub async fn reap_orphans_with_cx(cx: &crate::cx::Cx, max_age_seconds: u64) -> ReapReport {
    scan_and_reap_with_cx(cx, max_age_seconds).await
}

/// Implementation for a single orphan-reaper cycle.
async fn scan_and_reap(max_age_seconds: u64) -> ReapReport {
    let mut report = ReapReport::default();
    let entries = match list_wezterm_cli_processes_via_ps().await {
        Ok(entries) => entries,
        Err(error) => {
            report.errors.push(error);
            return report;
        }
    };
    report.scanned = entries.len();

    for entry in entries {
        if entry.age_seconds >= max_age_seconds {
            debug!(
                pid = entry.pid,
                age_s = entry.age_seconds,
                cmd = %entry.command,
                "killing orphaned wezterm cli process"
            );
            // Use runtime_compat::spawn_blocking + std::process::Command to
            // avoid requiring a Tokio reactor (panics under asupersync).
            let pid = entry.pid;
            let pid_str = pid.to_string();
            let kill_result = crate::runtime_compat::spawn_blocking(move || {
                std::process::Command::new("kill")
                    .args(["-s", "KILL", &pid_str])
                    .status()
            })
            .await;

            match kill_result {
                Ok(Ok(status)) if status.success() => {
                    report.killed += 1;
                    report.killed_pids.push(pid);
                }
                Ok(Ok(status)) => {
                    report
                        .errors
                        .push(format!("kill -s KILL {pid} exited with {status}"));
                }
                Ok(Err(error)) => {
                    report
                        .errors
                        .push(format!("failed to run kill for pid {pid}: {error}"));
                }
                Err(error) => {
                    report.errors.push(format!(
                        "spawn_blocking failed while killing pid {pid}: {error}"
                    ));
                }
            }
        }
    }

    report
}

/// Cx-first implementation of a single orphan-reaper cycle.
#[cfg(feature = "asupersync-runtime")]
async fn scan_and_reap_with_cx(cx: &crate::cx::Cx, max_age_seconds: u64) -> ReapReport {
    let mut report = ReapReport::default();

    if cx.checkpoint().is_err() {
        report.errors.push("reap cancelled before scan".to_string());
        return report;
    }

    let entries = match list_wezterm_cli_processes_via_ps_with_cx(cx).await {
        Ok(entries) => entries,
        Err(error) => {
            report.errors.push(error);
            return report;
        }
    };
    report.scanned = entries.len();

    for entry in entries {
        if cx.checkpoint().is_err() {
            report
                .errors
                .push("reap cancelled between kills".to_string());
            break;
        }

        if entry.age_seconds >= max_age_seconds {
            debug!(
                pid = entry.pid,
                age_s = entry.age_seconds,
                cmd = %entry.command,
                "killing orphaned wezterm cli process (cx-first)"
            );
            let pid = entry.pid;
            let pid_str = pid.to_string();
            let kill_result = crate::runtime_compat::spawn_blocking(move || {
                std::process::Command::new("kill")
                    .args(["-s", "KILL", &pid_str])
                    .status()
            })
            .await;

            match kill_result {
                Ok(Ok(status)) if status.success() => {
                    report.killed += 1;
                    report.killed_pids.push(pid);
                }
                Ok(Ok(status)) => {
                    report
                        .errors
                        .push(format!("kill -s KILL {pid} exited with {status}"));
                }
                Ok(Err(error)) => {
                    report
                        .errors
                        .push(format!("failed to run kill for pid {pid}: {error}"));
                }
                Err(error) => {
                    report.errors.push(format!(
                        "spawn_blocking failed while killing pid {pid}: {error}"
                    ));
                }
            }
        }
    }

    report
}

/// List `wezterm cli` processes that are candidates for reaping.
///
/// Uses `ps -eo pid,etimes,args` to get PID, elapsed time in seconds, and the
/// full command line.  Only processes whose command is a direct `wezterm cli
/// <subcommand>` invocation with a subcommand on the [`REAPABLE_SUBCOMMANDS`]
/// allowlist are returned.
///
/// Specifically excluded:
/// - `proxy` subcommand (long-lived SSH session transport — killing it severs
///   active sessions)
/// - Lines where `wezterm cli` appears only as an argument to another process
///   (e.g. `grep "wezterm cli"`, `zsh -c "wezterm cli list"`)
/// - Any unrecognized subcommand (defense in depth — only reap what we know)
async fn list_wezterm_cli_processes_via_ps() -> Result<Vec<ProcessEntry>, String> {
    // `etimes` gives elapsed time in seconds (POSIX, works on Linux and macOS).
    // Use runtime_compat::spawn_blocking + std::process::Command to avoid
    // requiring a Tokio reactor (panics under asupersync runtime).
    let output = crate::runtime_compat::spawn_blocking(|| {
        std::process::Command::new("ps")
            .args(["-eo", "pid,etimes,args"])
            .output()
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
    .map_err(|e| format!("failed to run ps: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "ps exited with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        // Skip the header line.
        if line.starts_with("PID") || line.is_empty() {
            continue;
        }

        if let Some(entry) = parse_ps_line_if_reapable(line) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

/// ft-xbnl0.2.3 Cx-first sibling of [`list_wezterm_cli_processes_via_ps`].
///
/// Pre-flight `cx.checkpoint()` before spawning the blocking `ps`
/// command and parsing its output. The spawn_blocking body is not
/// itself cx-aware (asupersync doesn't yet offer a cx-threaded
/// spawn_blocking), but this sibling at least lets callers bail
/// from the reaper scan loop without kicking off another full
/// `ps` fan-out when parent cancellation is already requested.
#[cfg(feature = "asupersync-runtime")]
async fn list_wezterm_cli_processes_via_ps_with_cx(
    cx: &crate::cx::Cx,
) -> Result<Vec<ProcessEntry>, String> {
    cx.checkpoint()
        .map_err(|err| format!("list_wezterm_cli_processes_via_ps cancelled: {err}"))?;
    list_wezterm_cli_processes_via_ps().await
}

/// Parse a single `ps -eo pid,etimes,args` line and return a [`ProcessEntry`]
/// only if the command is a directly-invoked `wezterm cli <allowed-subcommand>`.
///
/// Returns `None` for:
/// - Non-wezterm processes
/// - Lines where `wezterm cli` appears only as an argument to a wrapper (grep,
///   shell -c, etc.)
/// - `wezterm cli proxy` and any other non-allowlisted subcommand
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

    // Find the subcommand: the first token after "cli" that does not start
    // with "-" (skip flags like `--prefer-mux` that appear between "cli" and
    // the subcommand).
    let subcommand = tokens[(cli_pos + 1)..]
        .iter()
        .find(|&&t| !t.starts_with('-'))?;

    // Only reap subcommands on the explicit allowlist.
    if !REAPABLE_SUBCOMMANDS.contains(subcommand) {
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

    // -- Positive cases: should be accepted --

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

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for the Cx-first reaper loop
    // (ft-xbnl0.2.x slice). Exercise only the short-circuit paths that do
    // NOT spawn `ps`; the actual reap cycle requires a real
    // OS process table which is outside the scope of a deterministic test.
    // -------------------------------------------------------------------------

    #[cfg(feature = "asupersync-runtime")]
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

        /// Pre-flight cancellation: if the Cx is already cancelled on
        /// entry, the reaper returns before the `interval == 0` check
        /// and before any `sleep_with_cx` await. A Cx-unaware loop
        /// would either never start or block on the first sleep; the
        /// Cx-first entry point must exit immediately.
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

        /// Interval=0 disables the reaper: it must return immediately
        /// even under a live Cx. This matches the legacy `run_orphan_reaper`
        /// contract and avoids a permanent sleep loop when the operator
        /// has explicitly opted out.
        #[test]
        fn run_orphan_reaper_with_cx_interval_zero_disables() {
            run_lab(0x0_1FEA_BED5_0606, || async move {
                let mut config = CliConfig::default();
                config.orphan_reap_interval_seconds = 0;
                let shutdown = Arc::new(AtomicBool::new(false));
                let cx = crate::cx::for_request();

                run_orphan_reaper_with_cx(&cx, config, Arc::clone(&shutdown)).await;

                assert!(
                    !shutdown.load(Ordering::SeqCst),
                    "interval=0 must not touch the shutdown flag"
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
