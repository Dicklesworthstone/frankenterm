//! Platform reduce-motion probe (br-ft-d6nrd step 6 / br-ft-mpc9b.10.5
//! integration slice).
//!
//! Pure-substrate platform-preference probe that reads the
//! OS-level `prefers-reduced-motion` signal and maps it to the
//! [`MotionPreference`] enum the renderer's reduce-motion gate
//! consumes. Companion to the
//! [`accessibility_preferences`] substrate, which deferred
//! per-OS probing to the integration bead.
//!
//! ## Platforms
//!
//! - **macOS** — `defaults read com.apple.universalaccess
//!   reduceMotion`. Returns `1` when the user enabled
//!   "Reduce motion" in System Settings → Accessibility →
//!   Display.
//! - **Linux (GNOME)** — `gsettings get
//!   org.gnome.desktop.interface enable-animations`. Returns
//!   `false` when animations are disabled.
//! - **Windows** — `powershell -Command "(Get-ItemProperty
//!   'HKCU:\\Control Panel\\Desktop\\WindowMetrics'
//!   -Name MinAnimate).MinAnimate"`. Returns `0` when
//!   animations are disabled. (`SystemParametersInfo`
//!   SPI_GETCLIENTAREAANIMATION is not callable from
//!   `#![forbid(unsafe_code)]` Rust.)
//! - **Other / fallback** — returns
//!   [`MotionPreference::NoPreference`]; renderers preserve
//!   animations.
//!
//! ## Why `Command` instead of FFI
//!
//! The crate enforces `#![forbid(unsafe_code)]`, so the
//! probe drives platform CLIs via `std::process::Command`
//! and parses stdout. The probe is run rarely (session start
//! and on `SIGUSR1` reload), so the process-spawn cost is
//! immaterial against a 60-Hz paint loop.
//!
//! ## Failure handling
//!
//! Every probe is fallible. When the platform CLI is missing,
//! returns nonzero, or stdout doesn't parse, the probe falls
//! back to [`MotionPreference::NoPreference`] — the
//! "preserve animations" default per the substrate's docs
//! (the Unknown safety default). Operators can override via
//! `frankenterm.toml` regardless of probe outcome.
//!
//! ## What this module is NOT
//!
//! - Not a live notification subscription. The substrate's
//!   integration plan calls for per-OS notification
//!   listeners (NSWorkspace.didChangeAccessibilityDisplay,
//!   org.freedesktop.portal.Settings, etc.); those live in
//!   the GUI integration. This module ships the one-shot
//!   probe primitive those listeners call when the
//!   notification fires.
//! - Not a contrast / color-scheme probe. Those are deferred
//!   to a sibling slice; this slice covers only the motion
//!   axis (the largest accessibility regression risk per the
//!   parent bead's A11Y.5 ask).

use std::process::Command;
use std::time::Duration;

use crate::accessibility_preferences::MotionPreference;

/// Maximum wall-clock per-probe budget. The probe shells out
/// to a platform CLI; cap the wait so a hung shell never
/// blocks a session start. 500 ms is generous —
/// `defaults read` / `gsettings get` / `Get-ItemProperty`
/// all return in tens of milliseconds on healthy systems.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// One-shot OS preference probe.
///
/// Returns the user's current `prefers-reduced-motion` state
/// per the active platform. On any error path (missing CLI,
/// nonzero exit, unparseable output, or unsupported OS) the
/// probe returns [`MotionPreference::NoPreference`] — the
/// substrate's "preserve animations on Unknown" doctrine.
///
/// The probe is synchronous; callers run it once at session
/// startup and again on a change-notification signal.
#[must_use]
pub fn probe_reduce_motion() -> MotionPreference {
    // Each platform's helper returns Some on a confident
    // signal; None on any error path.
    let detected: Option<MotionPreference> = {
        #[cfg(target_os = "macos")]
        {
            macos::probe()
        }
        #[cfg(target_os = "linux")]
        {
            linux::probe()
        }
        #[cfg(target_os = "windows")]
        {
            windows::probe()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None::<MotionPreference>
        }
    };
    detected.unwrap_or(MotionPreference::NoPreference)
}

// ============================================================================
// Output parsers (platform-independent so the test suite can
// exercise them on any host)
// ============================================================================

/// Parse the stdout of `defaults read com.apple.universalaccess
/// reduceMotion`. Returns `Reduce` only when the value is
/// strictly `1`; any other recognized value is `NoPreference`.
/// Unrecognized output → `None` (caller falls back to default).
#[must_use]
pub fn parse_macos_reduce_motion(stdout: &str) -> Option<MotionPreference> {
    match stdout.trim() {
        "1" | "true" | "YES" => Some(MotionPreference::Reduce),
        "0" | "false" | "NO" => Some(MotionPreference::NoPreference),
        _ => None,
    }
}

/// Parse the stdout of
/// `gsettings get org.gnome.desktop.interface enable-animations`.
/// `false` → `Reduce` (animations disabled). `true` → `NoPreference`.
/// Unrecognized output → `None`.
#[must_use]
pub fn parse_linux_enable_animations(stdout: &str) -> Option<MotionPreference> {
    match stdout.trim() {
        "false" => Some(MotionPreference::Reduce),
        "true" => Some(MotionPreference::NoPreference),
        _ => None,
    }
}

/// Parse the stdout of the Windows MinAnimate registry probe.
/// `0` → `Reduce`. Any nonzero integer → `NoPreference`.
/// Unrecognized output → `None`.
#[must_use]
pub fn parse_windows_min_animate(stdout: &str) -> Option<MotionPreference> {
    let trimmed = stdout.trim();
    match trimmed.parse::<i64>() {
        Ok(0) => Some(MotionPreference::Reduce),
        Ok(_) => Some(MotionPreference::NoPreference),
        Err(_) => None,
    }
}

// ============================================================================
// Command runner — shared by all platforms
// ============================================================================

/// Run a `Command` with the [`PROBE_TIMEOUT`] cap. Returns
/// stdout as a `String` on a clean exit, `None` on any
/// failure (spawn error, nonzero exit, non-UTF-8 stdout,
/// timeout).
///
/// `std::process::Child::wait` doesn't natively support a
/// timeout, but the probe doesn't need true async-cancel
/// semantics — a polling loop with a short sleep is good
/// enough for a 500 ms budget. The probe runs on the GUI
/// thread at session startup; a 1-second worst-case stall
/// is acceptable.
fn run_with_timeout(mut cmd: Command) -> Option<String> {
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                if !status.success() {
                    return None;
                }
                let output = child.wait_with_output().ok()?;
                return String::from_utf8(output.stdout).ok();
            }
            None => {
                if start.elapsed() >= PROBE_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

// ============================================================================
// Platform-specific probe drivers
// ============================================================================

#[cfg(target_os = "macos")]
mod macos {
    use std::process::Command;

    use crate::accessibility_preferences::MotionPreference;

    use super::{parse_macos_reduce_motion, run_with_timeout};

    pub(super) fn probe() -> Option<MotionPreference> {
        let mut cmd = Command::new("defaults");
        cmd.args(["read", "com.apple.universalaccess", "reduceMotion"]);
        let stdout = run_with_timeout(cmd)?;
        parse_macos_reduce_motion(&stdout)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::Command;

    use crate::accessibility_preferences::MotionPreference;

    use super::{parse_linux_enable_animations, run_with_timeout};

    pub(super) fn probe() -> Option<MotionPreference> {
        let mut cmd = Command::new("gsettings");
        cmd.args(["get", "org.gnome.desktop.interface", "enable-animations"]);
        let stdout = run_with_timeout(cmd)?;
        parse_linux_enable_animations(&stdout)
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::process::Command;

    use crate::accessibility_preferences::MotionPreference;

    use super::{parse_windows_min_animate, run_with_timeout};

    pub(super) fn probe() -> Option<MotionPreference> {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-Command",
            "(Get-ItemProperty -Path 'HKCU:\\Control Panel\\Desktop\\WindowMetrics' -Name MinAnimate).MinAnimate",
        ]);
        let stdout = run_with_timeout(cmd)?;
        parse_windows_min_animate(&stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // macOS parser
    // ----------------------------------------------------------------

    #[test]
    fn macos_parser_recognises_one_as_reduce() {
        assert_eq!(
            parse_macos_reduce_motion("1\n"),
            Some(MotionPreference::Reduce)
        );
        assert_eq!(
            parse_macos_reduce_motion("1"),
            Some(MotionPreference::Reduce)
        );
    }

    #[test]
    fn macos_parser_recognises_zero_as_no_preference() {
        assert_eq!(
            parse_macos_reduce_motion("0\n"),
            Some(MotionPreference::NoPreference)
        );
    }

    #[test]
    fn macos_parser_recognises_true_false_words() {
        assert_eq!(
            parse_macos_reduce_motion("true"),
            Some(MotionPreference::Reduce)
        );
        assert_eq!(
            parse_macos_reduce_motion("false"),
            Some(MotionPreference::NoPreference)
        );
        // macOS sometimes writes capitalised YES/NO via plist.
        assert_eq!(
            parse_macos_reduce_motion("YES"),
            Some(MotionPreference::Reduce)
        );
        assert_eq!(
            parse_macos_reduce_motion("NO"),
            Some(MotionPreference::NoPreference)
        );
    }

    #[test]
    fn macos_parser_unknown_returns_none() {
        assert_eq!(parse_macos_reduce_motion(""), None);
        assert_eq!(parse_macos_reduce_motion("garbage"), None);
        // `defaults read` for a missing key prints an error to
        // stderr (which we discard) and an empty stdout — the
        // empty case routes to None and the caller falls back
        // to NoPreference.
    }

    // ----------------------------------------------------------------
    // Linux parser
    // ----------------------------------------------------------------

    #[test]
    fn linux_parser_recognises_false_as_reduce() {
        assert_eq!(
            parse_linux_enable_animations("false\n"),
            Some(MotionPreference::Reduce)
        );
    }

    #[test]
    fn linux_parser_recognises_true_as_no_preference() {
        assert_eq!(
            parse_linux_enable_animations("true\n"),
            Some(MotionPreference::NoPreference)
        );
    }

    #[test]
    fn linux_parser_unknown_returns_none() {
        // gsettings prints `'something'` for string keys; for the
        // boolean key it prints unquoted `true`/`false`. Anything
        // else (including the quoted form) routes to None.
        assert_eq!(parse_linux_enable_animations("'true'"), None);
        assert_eq!(parse_linux_enable_animations(""), None);
        assert_eq!(parse_linux_enable_animations("yes"), None);
    }

    // ----------------------------------------------------------------
    // Windows parser
    // ----------------------------------------------------------------

    #[test]
    fn windows_parser_recognises_zero_as_reduce() {
        assert_eq!(
            parse_windows_min_animate("0\r\n"),
            Some(MotionPreference::Reduce)
        );
        assert_eq!(
            parse_windows_min_animate("0"),
            Some(MotionPreference::Reduce)
        );
    }

    #[test]
    fn windows_parser_recognises_nonzero_as_no_preference() {
        assert_eq!(
            parse_windows_min_animate("1"),
            Some(MotionPreference::NoPreference)
        );
        assert_eq!(
            parse_windows_min_animate("42"),
            Some(MotionPreference::NoPreference)
        );
    }

    #[test]
    fn windows_parser_unknown_returns_none() {
        assert_eq!(parse_windows_min_animate(""), None);
        assert_eq!(parse_windows_min_animate("not-a-number"), None);
    }

    // ----------------------------------------------------------------
    // Probe end-to-end (no platform-specific assertion — the
    // probe runs against the test host's actual settings).
    // ----------------------------------------------------------------

    #[test]
    fn probe_returns_some_well_defined_preference() {
        // The probe MUST return one of the two MotionPreference
        // variants; it must not panic on any platform regardless
        // of OS configuration. This is the safety invariant.
        let pref = probe_reduce_motion();
        assert!(matches!(
            pref,
            MotionPreference::NoPreference | MotionPreference::Reduce
        ));
    }

    #[test]
    fn probe_runs_within_one_second_even_if_platform_cli_missing() {
        // 500 ms hard budget per probe. Run twice (so the
        // second iteration sees a hot dyld cache and we
        // capture worst-case spawn). Total wall-clock cap of 2s
        // — generous for the inner timeout.
        let start = std::time::Instant::now();
        let _ = probe_reduce_motion();
        let _ = probe_reduce_motion();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "two probes ran in {elapsed:?}; expected < 2s",
        );
    }

    // ----------------------------------------------------------------
    // run_with_timeout: spawn-failure path
    // ----------------------------------------------------------------

    #[test]
    fn run_with_timeout_returns_none_when_command_does_not_exist() {
        // A binary that's certainly not on PATH on any
        // supported platform.
        let cmd = Command::new("ft-this-probe-cli-does-not-exist-9bc7");
        let out = run_with_timeout(cmd);
        assert!(out.is_none(), "missing binary must yield None, got {out:?}");
    }

    // ----------------------------------------------------------------
    // run_with_timeout: nonzero-exit path
    // ----------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_returns_none_on_nonzero_exit() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 7"]);
        let out = run_with_timeout(cmd);
        assert!(out.is_none(), "nonzero exit must yield None");
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_returns_stdout_on_clean_exit() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf hello"]);
        let out = run_with_timeout(cmd);
        assert_eq!(out.as_deref(), Some("hello"));
    }
}
