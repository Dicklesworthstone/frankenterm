//! Generic subprocess bridge for /dp CLI integrations.
//!
//! Bridges standard CLI patterns used across integration modules:
//! - binary discovery (PATH first, then project release dirs)
//! - timeout-bounded process execution
//! - structured JSON parsing into typed outputs
//! - typed, content-free error surfacing via `BridgeError`

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use thiserror::Error;
use tracing::{debug, warn};

use crate::runtime_async::process::{
    Command, CommandCancellation, CommandCancelled, CommandCleanupTrigger,
    CommandOutputCaptureIncomplete, CommandOutputLimitExceeded, CommandOutputStream,
    CommandProcessCleanupIncomplete, CommandTimedOut,
    DEFAULT_COMMAND_STDERR_LIMIT_BYTES, DEFAULT_COMMAND_STDOUT_LIMIT_BYTES,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DP_ROOT: &str = "/dp";
const EXEC_BUSY_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(10), Duration::from_millis(50)];

/// Reusable subprocess bridge for typed JSON CLI integrations.
#[derive(Debug, Clone)]
pub struct SubprocessBridge<T> {
    binary_name: String,
    search_paths: Vec<PathBuf>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    _phantom: PhantomData<T>,
}

impl<T: DeserializeOwned> SubprocessBridge<T> {
    /// The name of the binary this bridge wraps.
    #[must_use]
    pub fn binary_name(&self) -> &str {
        &self.binary_name
    }

    /// Create a new bridge with default timeout and `/dp` project search root.
    #[must_use]
    pub fn new(binary: &str) -> Self {
        Self {
            binary_name: binary.to_string(),
            search_paths: vec![PathBuf::from(DP_ROOT)],
            timeout: DEFAULT_TIMEOUT,
            stdout_limit: DEFAULT_COMMAND_STDOUT_LIMIT_BYTES,
            stderr_limit: DEFAULT_COMMAND_STDERR_LIMIT_BYTES,
            _phantom: PhantomData,
        }
    }

    /// Override timeout for subprocess execution.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override project search roots used after PATH lookup.
    #[must_use]
    pub fn with_search_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.search_paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Override the maximum retained stdout bytes. Zero permits empty output
    /// only.
    #[must_use]
    pub fn with_stdout_limit(mut self, limit: usize) -> Self {
        self.stdout_limit = limit;
        self
    }

    /// Override the maximum retained stderr bytes. Zero permits empty output
    /// only.
    #[must_use]
    pub fn with_stderr_limit(mut self, limit: usize) -> Self {
        self.stderr_limit = limit;
        self
    }

    /// Check whether the target binary can be resolved.
    #[must_use]
    pub fn is_available(&self) -> bool {
        let available = self.resolve_binary().is_ok();
        debug!(available, "subprocess bridge availability checked");
        available
    }

    /// Invoke the CLI with args and parse JSON output.
    pub fn invoke(&self, args: &[&str]) -> Result<T, BridgeError> {
        self.invoke_with_env(args, &[])
    }

    /// Invoke with cooperative cancellation while retaining the same timeout
    /// and output bounds as [`Self::invoke`].
    pub fn invoke_with_cancellation(
        &self,
        args: &[&str],
        cancellation: &CommandCancellation,
    ) -> Result<T, BridgeError> {
        self.invoke_with_env_and_cancellation(args, &[], Some(cancellation))
    }

    /// Invoke the CLI with args + temporary environment overrides and parse JSON output.
    pub fn invoke_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<T, BridgeError> {
        self.invoke_with_env_and_cancellation(args, env, None)
    }

    fn invoke_with_env_and_cancellation(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        cancellation: Option<&CommandCancellation>,
    ) -> Result<T, BridgeError> {
        let binary = self.resolve_binary()?;
        debug!(
            timeout_ms = self.timeout.as_millis(),
            stdout_limit = self.stdout_limit,
            stderr_limit = self.stderr_limit,
            "invoking subprocess bridge"
        );

        let mut cmd = Command::new(&binary);
        cmd.args(args);
        cmd.kill_on_drop(true);
        cmd.stdout_limit(self.stdout_limit);
        cmd.stderr_limit(self.stderr_limit);
        cmd.exec_busy_retry_delays(&EXEC_BUSY_RETRY_DELAYS);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let output = match cancellation {
            Some(cancellation) => cmd
                .output_blocking_with_cancellation(self.timeout, cancellation)
                .map_err(|error| self.map_command_error(&error))?,
            None => cmd
                .output_blocking(self.timeout)
                .map_err(|error| self.map_command_error(&error))?,
        };

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            let error = BridgeError::ExitCode(code);
            warn!(error = %error, "subprocess bridge command failed");
            return Err(error);
        }

        serde_json::from_slice(&output.stdout).map_err(|_| {
            let parse_error = BridgeError::ParseError;
            warn!(error = %parse_error, "subprocess bridge parse failure");
            parse_error
        })
    }

    fn map_command_error(&self, err: &std::io::Error) -> BridgeError {
        if err.kind() == std::io::ErrorKind::NotFound {
            return BridgeError::BinaryNotFound;
        }
        if CommandTimedOut::from_io_error(err).is_some() {
            return BridgeError::Timeout(self.timeout);
        }
        if CommandCancelled::from_io_error(err).is_some() {
            return BridgeError::Cancelled;
        }
        if let Some(incomplete) = CommandOutputCaptureIncomplete::from_io_error(err) {
            return BridgeError::CaptureIncomplete {
                stdout_open: incomplete.stdout_open(),
                stderr_open: incomplete.stderr_open(),
                drain_timeout_ms: incomplete.drain_timeout_ms(),
            };
        }
        if let Some(incomplete) = CommandProcessCleanupIncomplete::from_io_error(err) {
            return BridgeError::CleanupIncomplete {
                trigger: incomplete.trigger(),
                leader_reaped: incomplete.leader_reaped(),
                signal_helper_settled: incomplete.signal_helper_settled(),
                process_tree_signalled: incomplete.process_tree_signalled(),
                stdout_open: incomplete.stdout_open(),
                stderr_open: incomplete.stderr_open(),
                settle_timeout_ms: incomplete.settle_timeout_ms(),
            };
        }
        if let Some(exceeded) = CommandOutputLimitExceeded::from_io_error(err) {
            return BridgeError::OutputTooLarge {
                stream: exceeded.stream(),
                observed: exceeded.observed(),
                limit: exceeded.limit(),
            };
        }
        BridgeError::Io(err.kind())
    }

    fn resolve_binary(&self) -> Result<PathBuf, BridgeError> {
        if self.binary_name.contains(std::path::MAIN_SEPARATOR) {
            let direct = PathBuf::from(&self.binary_name);
            if is_executable_file(&direct) {
                return Ok(direct);
            }
            return Err(BridgeError::BinaryNotFound);
        }

        if let Some(path_hit) = self.find_in_path() {
            return Ok(path_hit);
        }

        if let Some(search_hit) = self.find_in_search_paths() {
            return Ok(search_hit);
        }

        Err(BridgeError::BinaryNotFound)
    }

    fn find_in_path(&self) -> Option<PathBuf> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(&self.binary_name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn find_in_search_paths(&self) -> Option<PathBuf> {
        for root in &self.search_paths {
            let direct = root.join(&self.binary_name);
            if is_executable_file(&direct) {
                return Some(direct);
            }

            let root_release = root.join("target").join("release").join(&self.binary_name);
            if is_executable_file(&root_release) {
                return Some(root_release);
            }

            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let candidate = entry
                        .path()
                        .join("target")
                        .join("release")
                        .join(&self.binary_name);
                    if is_executable_file(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
}

fn is_executable_file(path: &Path) -> bool {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };

    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

/// Structured subprocess bridge failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BridgeError {
    #[error("subprocess binary not found")]
    BinaryNotFound,

    #[error("timed out after {0:?}")]
    Timeout(Duration),

    #[error("subprocess output was not valid JSON")]
    ParseError,

    #[error("subprocess exited with code {0}")]
    ExitCode(i32),

    #[error("subprocess command cancelled")]
    Cancelled,

    #[error(
        "subprocess output capture incomplete after {drain_timeout_ms} ms (stdout_open={stdout_open}, stderr_open={stderr_open})"
    )]
    CaptureIncomplete {
        stdout_open: bool,
        stderr_open: bool,
        drain_timeout_ms: u64,
    },

    #[error(
        "subprocess cleanup incomplete after {settle_timeout_ms} ms (trigger={trigger}, leader_reaped={leader_reaped}, signal_helper_settled={signal_helper_settled}, process_tree_signalled={process_tree_signalled}, stdout_open={stdout_open}, stderr_open={stderr_open})"
    )]
    CleanupIncomplete {
        trigger: CommandCleanupTrigger,
        leader_reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_open: bool,
        stderr_open: bool,
        settle_timeout_ms: u64,
    },

    #[error(
        "subprocess {stream} capture limit exceeded: observed at least {observed} bytes, limit {limit}"
    )]
    OutputTooLarge {
        stream: CommandOutputStream,
        observed: usize,
        limit: usize,
    },

    #[error("subprocess I/O failure: {0:?}")]
    Io(std::io::ErrorKind),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use serde::Deserialize;
    #[cfg(unix)]
    use serde_json::json;
    use serde_json::Value;
    #[cfg(unix)]
    use tempfile::tempdir;

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn write_non_executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn bridge(binary: &str) -> SubprocessBridge<Value> {
        SubprocessBridge::new(binary)
    }

    #[cfg(unix)]
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct SamplePayload {
        ok: bool,
        value: i32,
    }

    #[test]
    fn bridge_new_defaults() {
        let b = bridge("demo");
        assert_eq!(b.binary_name, "demo");
        assert_eq!(b.timeout, Duration::from_secs(10));
        assert_eq!(b.search_paths, vec![PathBuf::from("/dp")]);
        assert_eq!(b.stdout_limit, DEFAULT_COMMAND_STDOUT_LIMIT_BYTES);
        assert_eq!(b.stderr_limit, DEFAULT_COMMAND_STDERR_LIMIT_BYTES);
    }

    #[test]
    fn bridge_with_timeout_overrides_default() {
        let b = bridge("demo").with_timeout(Duration::from_millis(250));
        assert_eq!(b.timeout, Duration::from_millis(250));
    }

    #[test]
    fn bridge_with_search_paths_overrides_default() {
        let b = bridge("demo").with_search_paths(["/tmp/a", "/tmp/b"]);
        assert_eq!(
            b.search_paths,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
    }

    #[test]
    fn is_available_false_for_missing_binary() {
        let b = bridge("definitely-missing-binary-xyz");
        assert!(!b.is_available());
    }

    #[cfg(unix)]
    #[test]
    fn is_available_true_for_path_binary_sh() {
        let b = bridge("sh");
        assert!(b.is_available());
    }

    #[test]
    fn invoke_binary_not_found() {
        let b = bridge("definitely-missing-binary-xyz");
        let err = b.invoke(&[]).unwrap_err();
        assert_eq!(err, BridgeError::BinaryNotFound);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_parses_json_output_with_shell() {
        let b = bridge("sh");
        let out = b
            .invoke(&["-c", "printf '{\"ok\":true,\"value\":3}'"])
            .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["value"], 3);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_typed_payload_deserializes() {
        let b: SubprocessBridge<SamplePayload> = SubprocessBridge::new("sh");
        let out = b
            .invoke(&["-c", "printf '{\"ok\":true,\"value\":42}'"])
            .unwrap();
        assert_eq!(
            out,
            SamplePayload {
                ok: true,
                value: 42
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn invoke_with_env_passes_variables() {
        let b = bridge("sh");
        let out = b
            .invoke_with_env(
                &["-c", "printf '{\"v\":\"%s\"}' \"$BRIDGE_TEST_ENV\""],
                &[("BRIDGE_TEST_ENV", "expected")],
            )
            .unwrap();
        assert_eq!(out["v"], "expected");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_with_empty_env_still_works() {
        let b = bridge("sh");
        let out = b
            .invoke_with_env(&["-c", "printf '{\"ok\":true}'"], &[])
            .unwrap();
        assert_eq!(out["ok"], true);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_nonzero_exit_discards_stderr_content() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "echo 'boom' 1>&2; exit 23"]).unwrap_err();
        assert_eq!(err, BridgeError::ExitCode(23));
        assert!(!err.to_string().contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_nonzero_exit_discards_stdout_content() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "echo 'stdout-only'; exit 7"]).unwrap_err();
        assert_eq!(err, BridgeError::ExitCode(7));
        assert!(!err.to_string().contains("stdout-only"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_timeout_returns_error() {
        let b = bridge("sh").with_timeout(Duration::from_millis(25));
        let err = b
            .invoke(&["-c", "sleep 1; printf '{\"ok\":true}'"])
            .unwrap_err();
        assert_eq!(err, BridgeError::Timeout(Duration::from_millis(25)));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_invalid_json_returns_parse_error() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "printf 'not-json'"]).unwrap_err();
        assert_eq!(err, BridgeError::ParseError);
        assert!(!err.to_string().contains("not-json"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_parse_error_does_not_retain_stdout_content() {
        let b = bridge("sh");
        let err = b
            .invoke(&["-c", "printf 'token=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'"])
            .unwrap_err();
        assert_eq!(err, BridgeError::ParseError);
        assert!(!err.to_string().contains("AAAAAAAAAAAAAAAA"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_exit_error_does_not_retain_child_output() {
        let b = bridge("sh");
        let err = b
            .invoke(&[
                "-c",
                "printf 'secret=BBBBBBBBBBBBBBBBBBBBBBBB' 1>&2; exit 17",
            ])
            .unwrap_err();
        assert_eq!(err, BridgeError::ExitCode(17));
        assert!(!err.to_string().contains("BBBBBBBBBBBBBBBB"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_empty_output_returns_parse_error() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "printf ''"]).unwrap_err();
        assert_eq!(err, BridgeError::ParseError);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_unicode_json_roundtrip() {
        let b = bridge("sh");
        let out = b
            .invoke(&["-c", "printf '{\"msg\":\"h\\u00e9llo\"}'"])
            .unwrap();
        assert_eq!(out["msg"], "héllo");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_large_json_payload() {
        let b = bridge("sh");
        let out = b
            .invoke(&[
                "-c",
                "python3 - <<'PY'\nimport json\nprint(json.dumps({'n': 123, 'data': 'x'*2048}))\nPY",
            ])
            .unwrap();
        assert_eq!(out["n"], 123);
        assert_eq!(out["data"].as_str().unwrap().len(), 2048);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_path_with_slash_uses_direct_binary() {
        let b: SubprocessBridge<Value> = SubprocessBridge::new("/bin/sh");
        let out = b.invoke(&["-c", "printf '{\"ok\":true}'"]).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_binary_prefers_path_hit() {
        let b = bridge("sh").with_search_paths(["/definitely/not/used"]);
        let resolved = b.resolve_binary().unwrap();
        assert!(resolved.ends_with("sh"));
    }

    #[test]
    fn bridge_error_variants_are_structural_and_content_free() {
        let secret = "raw-child-output-canary";
        for error in [
            BridgeError::BinaryNotFound,
            BridgeError::ParseError,
            BridgeError::ExitCode(17),
            BridgeError::Cancelled,
            BridgeError::CaptureIncomplete {
                stdout_open: true,
                stderr_open: false,
                drain_timeout_ms: 100,
            },
            BridgeError::CleanupIncomplete {
                trigger: CommandCleanupTrigger::Cancelled,
                leader_reaped: false,
                signal_helper_settled: false,
                process_tree_signalled: false,
                stdout_open: true,
                stderr_open: false,
                settle_timeout_ms: 250,
            },
            BridgeError::Io(std::io::ErrorKind::Other),
        ] {
            assert!(!error.to_string().contains(secret));
        }
    }

    #[test]
    fn bridge_error_display_binary_not_found() {
        let err = BridgeError::BinaryNotFound;
        assert!(err.to_string().contains("binary not found"));
    }

    #[test]
    fn bridge_error_display_timeout() {
        let err = BridgeError::Timeout(Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn bridge_error_display_parse_error() {
        let err = BridgeError::ParseError;
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn bridge_error_display_exit_code() {
        let err = BridgeError::ExitCode(9);
        assert!(err.to_string().contains("code 9"));
    }

    #[test]
    fn bridge_error_equality_timeout() {
        assert_eq!(
            BridgeError::Timeout(Duration::from_secs(3)),
            BridgeError::Timeout(Duration::from_secs(3))
        );
    }

    #[test]
    fn bridge_error_equality_binary_not_found() {
        assert_eq!(BridgeError::BinaryNotFound, BridgeError::BinaryNotFound);
    }

    #[test]
    fn bridge_error_equality_parse_error() {
        assert_eq!(BridgeError::ParseError, BridgeError::ParseError);
    }

    #[test]
    fn bridge_error_equality_exit_code() {
        assert_eq!(BridgeError::ExitCode(1), BridgeError::ExitCode(1));
    }

    #[test]
    fn fail_open_pattern_example() {
        let b = bridge("definitely-missing-binary-xyz");
        let degraded = b.invoke(&[]).is_err();
        assert!(degraded);
    }

    #[cfg(unix)]
    #[test]
    fn find_binary_in_search_path_root_file() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("custom-bridge-bin");
        write_executable(&bin, "#!/bin/sh\nprintf '{\"ok\":true}'\n");

        let b = bridge("custom-bridge-bin").with_search_paths([dir.path().to_path_buf()]);
        let resolved = b.resolve_binary().unwrap();
        assert_eq!(resolved, bin);
    }

    #[cfg(unix)]
    #[test]
    fn find_binary_in_search_path_project_release_dir() {
        let dir = tempdir().unwrap();
        let release = dir.path().join("proj-a").join("target").join("release");
        std::fs::create_dir_all(&release).unwrap();
        let bin = release.join("custom-bridge-bin");
        write_executable(&bin, "#!/bin/sh\nprintf '{\"ok\":true}'\n");

        let b = bridge("custom-bridge-bin").with_search_paths([dir.path().to_path_buf()]);
        let resolved = b.resolve_binary().unwrap();
        assert_eq!(resolved, bin);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_from_search_path_root_file() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("root-bin");
        write_executable(&bin, "#!/bin/sh\nprintf '{\"source\":\"root\"}'\n");

        let b = bridge("root-bin").with_search_paths([dir.path().to_path_buf()]);
        let out = b.invoke(&[]).unwrap();
        assert_eq!(out["source"], "root");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_from_project_release_dir() {
        let dir = tempdir().unwrap();
        let release = dir.path().join("proj-b").join("target").join("release");
        std::fs::create_dir_all(&release).unwrap();
        let bin = release.join("proj-bin");
        write_executable(&bin, "#!/bin/sh\nprintf '{\"source\":\"release\"}'\n");

        let b = bridge("proj-bin").with_search_paths([dir.path().to_path_buf()]);
        let out = b.invoke(&[]).unwrap();
        assert_eq!(out["source"], "release");
    }

    #[cfg(unix)]
    #[test]
    fn search_path_order_first_match_wins() {
        let d1 = tempdir().unwrap();
        let d2 = tempdir().unwrap();
        let b1 = d1.path().join("same-bin");
        let b2 = d2.path().join("same-bin");

        write_executable(&b1, "#!/bin/sh\nprintf '{\"id\":1}'\n");
        write_executable(&b2, "#!/bin/sh\nprintf '{\"id\":2}'\n");

        let b = bridge("same-bin")
            .with_search_paths([d1.path().to_path_buf(), d2.path().to_path_buf()]);
        let out = b.invoke(&[]).unwrap();
        assert_eq!(out["id"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_not_available() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("noexec-bin");
        write_non_executable(&bin, "#!/bin/sh\nprintf '{\"ok\":true}'\n");

        let b = bridge("noexec-bin").with_search_paths([dir.path().to_path_buf()]);
        assert!(!b.is_available());
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_invocation_returns_binary_not_found() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("noexec-bin");
        write_non_executable(&bin, "#!/bin/sh\nprintf '{\"ok\":true}'\n");

        let b = bridge("noexec-bin").with_search_paths([dir.path().to_path_buf()]);
        let err = b.invoke(&[]).unwrap_err();
        assert_eq!(err, BridgeError::BinaryNotFound);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_permission_denied_direct_path_is_rejected_during_resolution() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("deny-bin");
        write_non_executable(&bin, "#!/bin/sh\nprintf '{\"ok\":true}'\n");

        let b: SubprocessBridge<Value> = SubprocessBridge::new(bin.to_string_lossy().as_ref());
        let err = b.invoke(&[]).unwrap_err();
        assert_eq!(err, BridgeError::BinaryNotFound);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_with_multiple_args_roundtrip() {
        let b = bridge("sh");
        let out = b
            .invoke(&[
                "-c",
                "printf '{\"argc\":%s,\"arg1\":\"%s\",\"arg2\":\"%s\"}' $# \"$1\" \"$2\"",
                "_",
                "one",
                "two",
            ])
            .unwrap();
        assert_eq!(out["argc"], 2);
        assert_eq!(out["arg1"], "one");
        assert_eq!(out["arg2"], "two");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_no_args_with_sh_c() {
        let b = bridge("sh");
        let out = b.invoke(&["-c", "printf '{\"ok\":true}'"]).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[cfg(unix)]
    #[test]
    fn parse_error_does_not_preserve_original_output_text() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "printf 'oops'"]).unwrap_err();
        assert_eq!(err, BridgeError::ParseError);
        assert!(!err.to_string().contains("oops"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_with_env_override_last_wins() {
        let b = bridge("sh");
        let out = b
            .invoke_with_env(
                &["-c", "printf '{\"v\":\"%s\"}' \"$BRIDGE_TEST_ENV\""],
                &[("BRIDGE_TEST_ENV", "first"), ("BRIDGE_TEST_ENV", "second")],
            )
            .unwrap();
        assert_eq!(out["v"], "second");
    }

    #[test]
    fn invoke_exit_code_preserves_negative_one_for_signal_or_unknown() {
        let err = BridgeError::ExitCode(-1);
        assert_eq!(err, BridgeError::ExitCode(-1));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_success_with_whitespace_json() {
        let b = bridge("sh");
        let out = b.invoke(&["-c", "printf '  {\"ok\":true}  '"]).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_parse_array_json() {
        let b = bridge("sh");
        let out = b.invoke(&["-c", "printf '[1,2,3]'"]).unwrap();
        assert_eq!(out, json!([1, 2, 3]));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_parse_nested_json_object() {
        let b = bridge("sh");
        let out = b
            .invoke(&["-c", "printf '{\"a\":{\"b\":{\"c\":1}}}'"])
            .unwrap();
        assert_eq!(out["a"]["b"]["c"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn invoke_daemon_holds_stdout_reports_incomplete_capture() {
        let b = bridge("sh").with_timeout(Duration::from_millis(500));
        let start = std::time::Instant::now();
        // The main 'sh' process exits immediately, but the background 'sleep' process
        // inherits stdout and holds it open.
        let err = b
            .invoke(&["-c", "sleep 1 >&1 2>/dev/null &"])
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            err,
            BridgeError::CaptureIncomplete {
                stdout_open: true,
                drain_timeout_ms: 100,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_accepts_exact_stdout_cap_and_rejects_one_byte_over() {
        let payload = r#"{"ok":true}"#;
        let exact = bridge("sh").with_stdout_limit(payload.len());
        assert_eq!(
            exact
                .invoke(&["-c", "printf '{\"ok\":true}'"])
                .expect("exact stdout boundary must be accepted"),
            json!({"ok": true})
        );

        let over = bridge("sh").with_stdout_limit(payload.len() - 1);
        let error = over
            .invoke(&[
                "-c",
                "printf '{\"ok\":true}'; while :; do sleep 1; done",
            ])
            .expect_err("first byte beyond stdout cap must fail closed");
        assert!(matches!(
            error,
            BridgeError::OutputTooLarge {
                stream: CommandOutputStream::Stdout,
                observed,
                limit
            } if observed >= payload.len() && limit == payload.len() - 1
        ));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_stderr_overflow_is_typed_and_content_free() {
        let error = bridge("sh")
            .with_stderr_limit(1)
            .invoke(&[
                "-c",
                "printf 'raw-child-output-canary' >&2; while :; do sleep 1; done",
            ])
            .expect_err("stderr flood must hit the configured cap");
        assert!(matches!(
            &error,
            BridgeError::OutputTooLarge {
                stream: CommandOutputStream::Stderr,
                limit: 1,
                ..
            }
        ));
        assert!(!error.to_string().contains("raw-child-output-canary"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_accepts_exact_stderr_cap_and_rejects_one_byte_over() {
        let exact = bridge("sh").with_stderr_limit(3);
        let output = exact
            .invoke(&[
                "-c",
                "printf 'abc' >&2; printf '{\"ok\":true}'",
            ])
            .expect("exact stderr boundary must be accepted");
        assert_eq!(output, json!({"ok": true}));

        let over = exact.with_stderr_limit(2);
        let error = over
            .invoke(&[
                "-c",
                "printf 'abc' >&2; while :; do sleep 1; done",
            ])
            .expect_err("first byte beyond stderr cap must fail closed");
        assert!(matches!(
            error,
            BridgeError::OutputTooLarge {
                stream: CommandOutputStream::Stderr,
                observed,
                limit: 2,
            } if observed >= 3
        ));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_high_volume_stdout_is_stopped_at_finite_cap() {
        const LIMIT: usize = 64 * 1024;
        let error = bridge("sh")
            .with_timeout(Duration::from_secs(2))
            .with_stdout_limit(LIMIT)
            .invoke(&["-c", "yes x"])
            .expect_err("unbounded producer must be stopped at the capture cap");
        assert!(matches!(
            error,
            BridgeError::OutputTooLarge {
                stream: CommandOutputStream::Stdout,
                observed,
                limit: LIMIT,
            } if observed > LIMIT
        ));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_cooperative_cancellation_is_typed_and_bounded() {
        let cancellation = CommandCancellation::new();
        let cancel_from_thread = cancellation.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            cancel_from_thread.cancel();
        });
        let started = std::time::Instant::now();
        let error = bridge("sh")
            .with_timeout(Duration::from_secs(5))
            .invoke_with_cancellation(&["-c", "sleep 10"], &cancellation)
            .expect_err("explicit cancellation must stop the subprocess");
        trigger.join().expect("cancellation trigger must finish");

        assert_eq!(error, BridgeError::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn production_bridge_has_no_unbounded_reader_or_child_content_error_path() {
        let source = include_str!("subprocess_bridge.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        for forbidden in [
            "read_to_end",
            "std::thread::Builder",
            "std::thread::spawn",
            ".join()",
            "child.wait()",
            "from_utf8_lossy",
            "redact_for_error",
            "binary = %",
            "args = ?",
        ] {
            assert!(
                !production.contains(forbidden),
                "production bridge must not contain {forbidden}"
            );
        }
    }
}
