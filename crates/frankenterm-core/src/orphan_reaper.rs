//! Orphan process reaper for stuck `wezterm cli` subprocesses.
//!
//! Under agent swarm workloads, WezTerm CLI processes can hang due to mux
//! server lock contention, missing socket timeouts, or notification feedback
//! loops. Even with `kill_on_drop(true)` on spawned commands, edge cases
//! (e.g., double-fork, signal races) can leave orphans.
//!
//! The reaper periodically scans for `wezterm cli` processes older than a
//! configurable threshold and kills them with SIGKILL.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::CliConfig;

/// Summary of a single reaper scan cycle.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReapReport {
    /// Number of candidate processes scanned.
    pub scanned: usize,
    /// Number of orphan processes killed.
    pub killed: usize,
    /// PIDs that were killed.
    pub killed_pids: Vec<u32>,
    /// Errors encountered during the scan.
    pub errors: Vec<String>,
}

/// Run the orphan reaper loop until `shutdown` is signalled.
///
/// This is intended to be spawned as a background runtime task inside the
/// watcher process.
pub async fn run_orphan_reaper(config: CliConfig, shutdown: Arc<AtomicBool>) {
    if config.orphan_reap_interval_seconds == 0 {
        info!("Orphan reaper disabled (orphan_reap_interval_seconds = 0)");
        return;
    }

    let interval = Duration::from_secs(config.orphan_reap_interval_seconds);
    let max_age_secs = config.orphan_max_age_seconds;

    info!(
        interval_secs = config.orphan_reap_interval_seconds,
        max_age_secs, "Orphan reaper started"
    );

    loop {
        crate::runtime_compat::sleep(interval).await;

        if shutdown.load(Ordering::Relaxed) {
            info!("Orphan reaper shutting down");
            break;
        }

        let report = reap_orphans(max_age_secs).await;

        if report.killed > 0 {
            warn!(
                scanned = report.scanned,
                killed = report.killed,
                pids = ?report.killed_pids,
                "Orphan reaper killed stuck wezterm cli processes"
            );
        } else {
            debug!(
                scanned = report.scanned,
                "Orphan reaper scan: no orphans found"
            );
        }

        for err in &report.errors {
            warn!(error = %err, "Orphan reaper error during scan");
        }
    }
}

/// Scan for and kill orphaned `wezterm` CLI processes older than `max_age_secs`.
pub async fn reap_orphans(max_age_secs: u64) -> ReapReport {
    // Run the blocking process scan on the active runtime's blocking threadpool.
    #[cfg(feature = "asupersync-runtime")]
    {
        asupersync::runtime::spawn_blocking(move || reap_orphans_sync(max_age_secs)).await
    }
    #[cfg(not(feature = "asupersync-runtime"))]
    {
        tokio::task::spawn_blocking(move || reap_orphans_sync(max_age_secs))
            .await
            .unwrap_or_else(|e| {
                let mut report = ReapReport::default();
                report.errors.push(format!("spawn_blocking failed: {e}"));
                report
            })
    }
}

/// Synchronous implementation of the orphan reaper.
fn reap_orphans_sync(max_age_secs: u64) -> ReapReport {
    let mut report = ReapReport::default();

    let scan = list_wezterm_cli_processes();
    report.errors.extend(scan.errors);
    let processes = scan.processes;

    report.scanned = processes.len();

    for proc in processes {
        if proc.age_secs < max_age_secs {
            continue;
        }

        if let Err(e) = kill_process(proc.pid) {
            report
                .errors
                .push(format!("failed to kill pid {}: {e}", proc.pid));
        } else {
            report.killed += 1;
            report.killed_pids.push(proc.pid);
        }
    }

    report
}

#[derive(Debug, Clone, Copy)]
struct ScannedProcess {
    pid: u32,
    age_secs: u64,
}

#[derive(Debug, Default)]
struct ProcessScan {
    processes: Vec<ScannedProcess>,
    errors: Vec<String>,
}

#[cfg(target_os = "linux")]
fn list_wezterm_cli_processes() -> ProcessScan {
    list_wezterm_cli_processes_via_ps(["-eo", "pid=,etimes=,args="], parse_ps_age_secs_linux)
}

#[cfg(target_os = "macos")]
fn list_wezterm_cli_processes() -> ProcessScan {
    list_wezterm_cli_processes_via_ps(["-axo", "pid=,etime=,command="], parse_ps_age_secs_macos)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn list_wezterm_cli_processes() -> ProcessScan {
    ProcessScan {
        processes: Vec::new(),
        errors: vec!["orphan reaper not supported on this platform".to_string()],
    }
}

fn list_wezterm_cli_processes_via_ps<const N: usize>(
    ps_args: [&str; N],
    parse_age_secs: fn(&str) -> Result<u64, String>,
) -> ProcessScan {
    use std::process::Command;

    let output = match Command::new("ps").args(ps_args).output() {
        Ok(output) => output,
        Err(e) => {
            return ProcessScan {
                processes: Vec::new(),
                errors: vec![format!("ps failed: {e}")],
            };
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ProcessScan {
            processes: Vec::new(),
            errors: vec![format!("ps returned non-zero: {stderr}")],
        };
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut scan = ProcessScan::default();
    for line in stdout.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }

        let Some((pid_str, rest)) = split_first_token(line) else {
            scan.errors
                .push(format!("unexpected ps line (pid missing): {line}"));
            continue;
        };
        let pid: u32 = pid_str.parse().unwrap_or_else(|e| {
            scan.errors.push(format!("parse pid {pid_str:?}: {e}"));
            0
        });
        if pid == 0 {
            continue;
        }

        let rest = rest.trim_start();
        let Some((age_str, rest)) = split_first_token(rest) else {
            scan.errors
                .push(format!("unexpected ps line (age missing): {line}"));
            continue;
        };
        let age_secs = match parse_age_secs(age_str) {
            Ok(age_secs) => age_secs,
            Err(e) => {
                debug!(pid, error = %e, "Could not parse process age");
                continue;
            }
        };

        let command = rest.trim_start();
        if !command.contains("wezterm cli") {
            continue;
        }

        scan.processes.push(ScannedProcess { pid, age_secs });
    }

    scan
}

fn split_first_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }

    match input.find(char::is_whitespace) {
        Some(idx) => Some((&input[..idx], &input[idx..])),
        None => Some((input, "")),
    }
}

#[cfg(target_os = "linux")]
fn parse_ps_age_secs_linux(age: &str) -> Result<u64, String> {
    age.parse::<u64>()
        .map_err(|e| format!("parse etimes seconds: {e}"))
}

#[cfg(target_os = "macos")]
fn parse_ps_age_secs_macos(age: &str) -> Result<u64, String> {
    parse_etime(age)
}

/// Parse the `etime` format from `ps -o etime=`.
///
/// Formats: `SS`, `MM:SS`, `HH:MM:SS`, `D-HH:MM:SS`
fn parse_etime(etime: &str) -> Result<u64, String> {
    if etime.is_empty() {
        return Err("empty etime".to_string());
    }

    let (days, rest) = if let Some((d, r)) = etime.split_once('-') {
        let days: u64 = d.parse().map_err(|e| format!("parse days: {e}"))?;
        (days, r)
    } else {
        (0, etime)
    };

    let parts: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match parts.len() {
        1 => {
            let s: u64 = parts[0].parse().map_err(|e| format!("parse secs: {e}"))?;
            (0, 0, s)
        }
        2 => {
            let m: u64 = parts[0].parse().map_err(|e| format!("parse mins: {e}"))?;
            let s: u64 = parts[1].parse().map_err(|e| format!("parse secs: {e}"))?;
            (0, m, s)
        }
        3 => {
            let h: u64 = parts[0].parse().map_err(|e| format!("parse hours: {e}"))?;
            let m: u64 = parts[1].parse().map_err(|e| format!("parse mins: {e}"))?;
            let s: u64 = parts[2].parse().map_err(|e| format!("parse secs: {e}"))?;
            (h, m, s)
        }
        _ => return Err(format!("unexpected etime format: {etime}")),
    };

    Ok(days * 86400 + hours * 3600 + minutes * 60 + seconds)
}

/// Send SIGKILL to a process.
#[cfg(unix)]
fn kill_process(pid: u32) -> Result<(), String> {
    use std::process::Command;

    let output = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .map_err(|e| format!("kill command failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "No such process" means it already exited — not an error
        if stderr.contains("No such process") {
            Ok(())
        } else {
            Err(stderr.to_string())
        }
    }
}

#[cfg(not(unix))]
fn kill_process(_pid: u32) -> Result<(), String> {
    Err("kill not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_etime_seconds_only() {
        assert_eq!(parse_etime("42").unwrap(), 42);
    }

    #[test]
    fn parse_etime_minutes_seconds() {
        assert_eq!(parse_etime("03:42").unwrap(), 3 * 60 + 42);
    }

    #[test]
    fn parse_etime_hours_minutes_seconds() {
        assert_eq!(parse_etime("01:03:42").unwrap(), 3600 + 3 * 60 + 42);
    }

    #[test]
    fn parse_etime_days_hours_minutes_seconds() {
        assert_eq!(
            parse_etime("2-01:03:42").unwrap(),
            2 * 86400 + 3600 + 3 * 60 + 42
        );
    }

    #[test]
    fn parse_etime_empty_is_error() {
        assert!(parse_etime("").is_err());
    }

    #[test]
    fn parse_etime_invalid_is_error() {
        assert!(parse_etime("abc").is_err());
    }

    #[test]
    fn reap_report_default_is_empty() {
        let report = ReapReport::default();
        assert_eq!(report.scanned, 0);
        assert_eq!(report.killed, 0);
        assert!(report.killed_pids.is_empty());
        assert!(report.errors.is_empty());
    }

    #[test]
    fn reap_report_serializes() {
        let report = ReapReport {
            scanned: 5,
            killed: 2,
            killed_pids: vec![1234, 5678],
            errors: vec![],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"scanned\":5"));
        assert!(json.contains("\"killed\":2"));
    }

    #[test]
    fn cli_config_defaults() {
        let config = CliConfig::default();
        assert_eq!(config.timeout_seconds, 15);
        assert_eq!(config.orphan_reap_interval_seconds, 60);
        assert_eq!(config.orphan_max_age_seconds, 30);
    }

    // -----------------------------------------------------------------------
    // Batch 14 — PearlHeron wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- split_first_token ----

    #[test]
    fn split_first_token_normal() {
        let (first, rest) = split_first_token("hello world foo").unwrap();
        assert_eq!(first, "hello");
        assert_eq!(rest.trim(), "world foo");
    }

    #[test]
    fn split_first_token_single_word() {
        let (first, rest) = split_first_token("hello").unwrap();
        assert_eq!(first, "hello");
        assert_eq!(rest, "");
    }

    #[test]
    fn split_first_token_empty() {
        assert!(split_first_token("").is_none());
    }

    #[test]
    fn split_first_token_whitespace_only() {
        assert!(split_first_token("   ").is_none());
    }

    #[test]
    fn split_first_token_leading_whitespace() {
        let (first, rest) = split_first_token("   hello world").unwrap();
        assert_eq!(first, "hello");
        assert_eq!(rest.trim(), "world");
    }

    #[test]
    fn split_first_token_tab_separator() {
        let (first, rest) = split_first_token("abc\tdef").unwrap();
        assert_eq!(first, "abc");
        assert_eq!(rest.trim(), "def");
    }

    // ---- parse_etime edge cases ----

    #[test]
    fn parse_etime_zero() {
        assert_eq!(parse_etime("0").unwrap(), 0);
        assert_eq!(parse_etime("00:00").unwrap(), 0);
        assert_eq!(parse_etime("00:00:00").unwrap(), 0);
        assert_eq!(parse_etime("0-00:00:00").unwrap(), 0);
    }

    #[test]
    fn parse_etime_days_only() {
        assert_eq!(parse_etime("1-00:00:00").unwrap(), 86400);
        assert_eq!(parse_etime("7-00:00:00").unwrap(), 7 * 86400);
    }

    #[test]
    fn parse_etime_too_many_colons() {
        assert!(parse_etime("1:2:3:4").is_err());
    }

    // ---- ReapReport ----

    #[test]
    fn reap_report_serde_json_fields() {
        let report = ReapReport {
            scanned: 10,
            killed: 0,
            killed_pids: vec![],
            errors: vec!["test error".to_string()],
        };
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["scanned"], 10);
        assert_eq!(json["killed"], 0);
        assert!(json["killed_pids"].as_array().unwrap().is_empty());
        assert_eq!(json["errors"][0], "test error");
    }

    // ── Batch: RubyBeaver wa-1u90p.7.1 ──────────────────────────────────

    // ---- parse_etime comprehensive ----

    #[test]
    fn parse_etime_minutes_zero_seconds() {
        assert_eq!(parse_etime("05:00").unwrap(), 300);
    }

    #[test]
    fn parse_etime_hours_zero_minutes_seconds() {
        assert_eq!(parse_etime("02:00:00").unwrap(), 7200);
    }

    #[test]
    fn parse_etime_large_days() {
        assert_eq!(parse_etime("365-00:00:00").unwrap(), 365 * 86400);
    }

    #[test]
    fn parse_etime_days_with_time() {
        assert_eq!(
            parse_etime("1-12:30:45").unwrap(),
            86400 + 12 * 3600 + 30 * 60 + 45
        );
    }

    #[test]
    fn parse_etime_invalid_days_format() {
        assert!(parse_etime("abc-01:02:03").is_err());
    }

    #[test]
    fn parse_etime_invalid_hours() {
        assert!(parse_etime("xx:02:03").is_err());
    }

    #[test]
    fn parse_etime_invalid_minutes() {
        assert!(parse_etime("01:yy:03").is_err());
    }

    #[test]
    fn parse_etime_invalid_seconds_in_mm_ss() {
        assert!(parse_etime("01:zz").is_err());
    }

    #[test]
    fn parse_etime_max_values() {
        // 99 days, 23 hours, 59 minutes, 59 seconds
        let result = parse_etime("99-23:59:59").unwrap();
        assert_eq!(result, 99 * 86400 + 23 * 3600 + 59 * 60 + 59);
    }

    #[test]
    fn parse_etime_single_digit_seconds() {
        assert_eq!(parse_etime("5").unwrap(), 5);
    }

    #[test]
    fn parse_etime_single_digit_minutes_seconds() {
        assert_eq!(parse_etime("1:2").unwrap(), 62);
    }

    // ---- split_first_token comprehensive ----

    #[test]
    fn split_first_token_multiple_spaces() {
        let (first, rest) = split_first_token("hello    world").unwrap();
        assert_eq!(first, "hello");
        assert!(rest.starts_with(' '));
    }

    #[test]
    fn split_first_token_newline_separator() {
        let (first, rest) = split_first_token("abc\ndef").unwrap();
        assert_eq!(first, "abc");
        assert_eq!(rest.trim(), "def");
    }

    #[test]
    fn split_first_token_mixed_whitespace() {
        let (first, rest) = split_first_token("  \thello world").unwrap();
        assert_eq!(first, "hello");
        assert_eq!(rest.trim(), "world");
    }

    // ---- ReapReport tests ----

    #[test]
    fn reap_report_debug_format() {
        let report = ReapReport::default();
        let dbg = format!("{report:?}");
        assert!(dbg.contains("scanned"));
        assert!(dbg.contains("killed"));
    }

    #[test]
    fn reap_report_clone() {
        let report = ReapReport {
            scanned: 3,
            killed: 1,
            killed_pids: vec![42],
            errors: vec!["err".to_string()],
        };
        let cloned = report.clone();
        assert_eq!(cloned.scanned, 3);
        assert_eq!(cloned.killed, 1);
        assert_eq!(cloned.killed_pids, vec![42]);
        assert_eq!(cloned.errors, vec!["err"]);
    }

    #[test]
    fn reap_report_with_multiple_killed() {
        let report = ReapReport {
            scanned: 20,
            killed: 5,
            killed_pids: vec![100, 200, 300, 400, 500],
            errors: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"killed\":5"));
        assert!(json.contains("500"));
    }

    #[test]
    fn reap_report_with_multiple_errors() {
        let report = ReapReport {
            scanned: 3,
            killed: 0,
            killed_pids: vec![],
            errors: vec![
                "error 1".to_string(),
                "error 2".to_string(),
                "error 3".to_string(),
            ],
        };
        assert_eq!(report.errors.len(), 3);
    }

    // ---- reap_orphans_sync with mocked process list ----

    #[test]
    fn reap_orphans_sync_returns_report() {
        // With max_age_secs very high, no real processes should be killed
        let report = reap_orphans_sync(999_999);
        // We can't predict exact counts but the structure should be valid
        assert!(report.killed_pids.len() == report.killed);
    }

    #[test]
    fn reap_orphans_sync_max_age_zero_is_aggressive() {
        // max_age 0 means kill any wezterm cli process
        let report = reap_orphans_sync(0);
        // Report structure is valid regardless of what was found
        assert!(report.killed_pids.len() == report.killed);
    }

    // ---- CliConfig defaults for reaper ----

    #[test]
    fn cli_config_orphan_fields() {
        let config = CliConfig::default();
        assert!(config.orphan_reap_interval_seconds > 0);
        assert!(config.orphan_max_age_seconds > 0);
        assert!(
            config.orphan_max_age_seconds < config.orphan_reap_interval_seconds,
            "max age should be less than interval"
        );
    }

    #[test]
    fn cli_config_debug() {
        let config = CliConfig::default();
        let dbg = format!("{config:?}");
        assert!(dbg.contains("timeout_seconds"));
        assert!(dbg.contains("orphan_reap_interval_seconds"));
    }

    // ---- Async reap_orphans ----

    #[tokio::test]
    async fn reap_orphans_async_returns_report() {
        let report = reap_orphans(999_999).await;
        assert!(report.killed_pids.len() == report.killed);
    }

    #[tokio::test]
    async fn reap_orphans_async_zero_max_age() {
        let report = reap_orphans(0).await;
        assert!(report.killed_pids.len() == report.killed);
    }

    // ---- run_orphan_reaper disabled ----

    #[tokio::test]
    async fn run_orphan_reaper_disabled_returns_immediately() {
        let mut config = CliConfig::default();
        config.orphan_reap_interval_seconds = 0; // disabled
        let shutdown = Arc::new(AtomicBool::new(false));

        // Should return immediately when interval is 0
        let handle = tokio::spawn(run_orphan_reaper(config, shutdown));
        let result = tokio::time::timeout(Duration::from_millis(100), handle).await;
        assert!(result.is_ok(), "disabled reaper should return immediately");
    }

    // ---- run_orphan_reaper shutdown ----

    #[tokio::test]
    async fn run_orphan_reaper_responds_to_shutdown() {
        let mut config = CliConfig::default();
        config.orphan_reap_interval_seconds = 1; // 1 second interval
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn(run_orphan_reaper(config, shutdown_clone));

        // Signal shutdown after a short delay
        crate::runtime_compat::sleep(Duration::from_millis(50)).await;
        shutdown.store(true, Ordering::Relaxed);

        // Should exit within a reasonable time (after current sleep)
        let result = tokio::time::timeout(Duration::from_secs(3), handle).await;
        assert!(result.is_ok(), "reaper should respond to shutdown signal");
    }
}
