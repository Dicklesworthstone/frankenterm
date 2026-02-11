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
/// This is intended to be spawned as a background `tokio::spawn` task
/// inside the watcher process.
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
        tokio::time::sleep(interval).await;

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
    // Run the blocking process scan on the blocking threadpool
    tokio::task::spawn_blocking(move || reap_orphans_sync(max_age_secs))
        .await
        .unwrap_or_else(|e| {
            let mut report = ReapReport::default();
            report.errors.push(format!("spawn_blocking failed: {e}"));
            report
        })
}

/// Synchronous implementation of the orphan reaper.
fn reap_orphans_sync(max_age_secs: u64) -> ReapReport {
    let mut report = ReapReport::default();

    let processes = match list_wezterm_cli_processes() {
        Ok(processes) => processes,
        Err(e) => {
            report.errors.push(format!("process scan failed: {e}"));
            return report;
        }
    };

    report.scanned = processes.len();

    for proc in processes {
        if proc.age_secs < max_age_secs {
            continue;
        }

        if let Err(e) = kill_process(proc.pid) {
            report.errors.push(format!("failed to kill pid {}: {e}", proc.pid));
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

#[cfg(target_os = "linux")]
fn list_wezterm_cli_processes() -> Result<Vec<ScannedProcess>, String> {
    list_wezterm_cli_processes_via_ps(["-eo", "pid=,etimes=,args="], parse_ps_age_secs_linux)
}

#[cfg(target_os = "macos")]
fn list_wezterm_cli_processes() -> Result<Vec<ScannedProcess>, String> {
    list_wezterm_cli_processes_via_ps(
        ["-axo", "pid=,etime=,command="],
        parse_ps_age_secs_macos,
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn list_wezterm_cli_processes() -> Result<Vec<ScannedProcess>, String> {
    Err("orphan reaper not supported on this platform".to_string())
}

fn list_wezterm_cli_processes_via_ps<const N: usize>(
    ps_args: [&str; N],
    parse_age_secs: fn(&str) -> Result<u64, String>,
) -> Result<Vec<ScannedProcess>, String> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(ps_args)
        .output()
        .map_err(|e| format!("ps failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ps returned non-zero: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut processes = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }

        let (pid_str, rest) = split_first_token(line)
            .ok_or_else(|| format!("unexpected ps line (pid missing): {line}"))?;
        let pid: u32 = pid_str
            .parse()
            .map_err(|e| format!("parse pid {pid_str:?}: {e}"))?;

        let rest = rest.trim_start();
        let (age_str, rest) = split_first_token(rest)
            .ok_or_else(|| format!("unexpected ps line (age missing): {line}"))?;
        let age_secs = parse_age_secs(age_str)?;

        let command = rest.trim_start();
        if !command.contains("wezterm cli") {
            continue;
        }

        processes.push(ScannedProcess { pid, age_secs });
    }

    Ok(processes)
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
}
