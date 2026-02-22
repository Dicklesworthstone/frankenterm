//! Generic subprocess bridge for /dp CLI integrations.
//!
//! Bridges standard CLI patterns used across integration modules:
//! - binary discovery (PATH first, then project release dirs)
//! - timeout-bounded process execution
//! - structured JSON parsing into typed outputs
//! - fail-open error surfacing via `BridgeError`

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use thiserror::Error;
use tracing::{debug, warn};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DP_ROOT: &str = "/dp";
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Reusable subprocess bridge for typed JSON CLI integrations.
#[derive(Debug, Clone)]
pub struct SubprocessBridge<T> {
    binary_name: String,
    search_paths: Vec<PathBuf>,
    timeout: Duration,
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

    /// Check whether the target binary can be resolved.
    #[must_use]
    pub fn is_available(&self) -> bool {
        let available = self.resolve_binary().is_ok();
        debug!(bridge = %self.binary_name, available, "subprocess bridge availability checked");
        available
    }

    /// Invoke the CLI with args and parse JSON output.
    pub fn invoke(&self, args: &[&str]) -> Result<T, BridgeError> {
        self.invoke_with_env(args, &[])
    }

    /// Invoke the CLI with args + temporary environment overrides and parse JSON output.
    pub fn invoke_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<T, BridgeError> {
        let binary = self.resolve_binary()?;
        debug!(
            bridge = %self.binary_name,
            binary = %binary.display(),
            args = ?args,
            timeout_ms = self.timeout.as_millis(),
            "invoking subprocess bridge"
        );

        let mut cmd = Command::new(&binary);
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|err| self.map_spawn_error(err))?;
        let started = Instant::now();

        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    let output = child
                        .wait_with_output()
                        .map_err(|err| BridgeError::ExitCode(-1, err.to_string()))?;

                    if !output.status.success() {
                        let code = output.status.code().unwrap_or(-1);
                        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                        let detail = if stderr.trim().is_empty() {
                            stdout
                        } else {
                            stderr
                        };
                        let err = BridgeError::ExitCode(code, truncate_for_error(&detail));
                        warn!(bridge = %self.binary_name, error = %err, "subprocess bridge command failed");
                        return Err(err);
                    }

                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    return serde_json::from_str(&stdout).map_err(|err| {
                        let parse_err = BridgeError::ParseError(format!(
                            "{} (stdout preview: {})",
                            err,
                            truncate_for_error(&stdout)
                        ));
                        warn!(bridge = %self.binary_name, error = %parse_err, "subprocess bridge parse failure");
                        parse_err
                    });
                }
                Ok(None) => {
                    if started.elapsed() >= self.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        let err = BridgeError::Timeout(self.timeout);
                        warn!(bridge = %self.binary_name, error = %err, "subprocess bridge timeout");
                        return Err(err);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(err) => {
                    let bridge_err = BridgeError::ExitCode(-1, err.to_string());
                    warn!(bridge = %self.binary_name, error = %bridge_err, "subprocess bridge wait failure");
                    return Err(bridge_err);
                }
            }
        }
    }

    fn map_spawn_error(&self, err: std::io::Error) -> BridgeError {
        if err.kind() == std::io::ErrorKind::NotFound {
            return BridgeError::BinaryNotFound(self.binary_name.clone());
        }
        BridgeError::ExitCode(-1, err.to_string())
    }

    fn resolve_binary(&self) -> Result<PathBuf, BridgeError> {
        if self.binary_name.contains(std::path::MAIN_SEPARATOR) {
            let direct = PathBuf::from(&self.binary_name);
            if is_executable_file(&direct) {
                return Ok(direct);
            }
            return Err(BridgeError::BinaryNotFound(self.binary_name.clone()));
        }

        if let Some(path_hit) = self.find_in_path() {
            return Ok(path_hit);
        }

        if let Some(search_hit) = self.find_in_search_paths() {
            return Ok(search_hit);
        }

        Err(BridgeError::BinaryNotFound(self.binary_name.clone()))
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

fn truncate_for_error(input: &str) -> String {
    const MAX_LEN: usize = 240;
    if input.len() <= MAX_LEN {
        return input.to_string();
    }

    let mut end = MAX_LEN;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }

    let mut out = input[..end].to_string();
    out.push_str("...");
    out
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
    #[error("binary not found: {0}")]
    BinaryNotFound(String),

    #[error("timed out after {0:?}")]
    Timeout(Duration),

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("exit code {0}: {1}")]
    ExitCode(i32, String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::{Value, json};
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

    #[test]
    fn is_available_true_for_path_binary_sh() {
        let b = bridge("sh");
        assert!(b.is_available());
    }

    #[test]
    fn invoke_binary_not_found() {
        let b = bridge("definitely-missing-binary-xyz");
        let err = b.invoke(&[]).unwrap_err();
        assert!(matches!(err, BridgeError::BinaryNotFound(_)));
    }

    #[test]
    fn invoke_parses_json_output_with_shell() {
        let b = bridge("sh");
        let out = b
            .invoke(&["-c", "printf '{\"ok\":true,\"value\":3}'"])
            .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["value"], 3);
    }

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

    #[test]
    fn invoke_with_empty_env_still_works() {
        let b = bridge("sh");
        let out = b
            .invoke_with_env(&["-c", "printf '{\"ok\":true}'"], &[])
            .unwrap();
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn invoke_nonzero_exit_reports_stderr() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "echo 'boom' 1>&2; exit 23"]).unwrap_err();
        match err {
            BridgeError::ExitCode(code, message) => {
                assert_eq!(code, 23);
                assert!(message.contains("boom"));
            }
            _ => panic!("expected exit-code error"),
        }
    }

    #[test]
    fn invoke_nonzero_exit_falls_back_to_stdout_when_stderr_empty() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "echo 'stdout-only'; exit 7"]).unwrap_err();
        match err {
            BridgeError::ExitCode(code, message) => {
                assert_eq!(code, 7);
                assert!(message.contains("stdout-only"));
            }
            _ => panic!("expected exit-code error"),
        }
    }

    #[test]
    fn invoke_timeout_returns_error() {
        let b = bridge("sh").with_timeout(Duration::from_millis(25));
        let err = b
            .invoke(&["-c", "sleep 1; printf '{\"ok\":true}'"])
            .unwrap_err();
        assert_eq!(err, BridgeError::Timeout(Duration::from_millis(25)));
    }

    #[test]
    fn invoke_invalid_json_returns_parse_error() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "printf 'not-json'"]).unwrap_err();
        match err {
            BridgeError::ParseError(msg) => assert!(msg.contains("stdout preview")),
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn invoke_empty_output_returns_parse_error() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "printf ''"]).unwrap_err();
        assert!(matches!(err, BridgeError::ParseError(_)));
    }

    #[test]
    fn invoke_unicode_json_roundtrip() {
        let b = bridge("sh");
        let out = b
            .invoke(&["-c", "printf '{\"msg\":\"h\\u00e9llo\"}'"])
            .unwrap();
        assert_eq!(out["msg"], "héllo");
    }

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

    #[test]
    fn invoke_path_with_slash_uses_direct_binary() {
        let b: SubprocessBridge<Value> = SubprocessBridge::new("/bin/sh");
        let out = b.invoke(&["-c", "printf '{\"ok\":true}'"]).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn resolve_binary_prefers_path_hit() {
        let b = bridge("sh").with_search_paths(["/definitely/not/used"]);
        let resolved = b.resolve_binary().unwrap();
        assert!(resolved.ends_with("sh"));
    }

    #[test]
    fn truncate_for_error_short_passthrough() {
        let msg = "short";
        assert_eq!(truncate_for_error(msg), msg);
    }

    #[test]
    fn truncate_for_error_long_adds_ellipsis() {
        let msg = "x".repeat(500);
        let truncated = truncate_for_error(&msg);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() < msg.len());
    }

    #[test]
    fn bridge_error_display_binary_not_found() {
        let err = BridgeError::BinaryNotFound("demo".to_string());
        assert!(err.to_string().contains("binary not found"));
    }

    #[test]
    fn bridge_error_display_timeout() {
        let err = BridgeError::Timeout(Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn bridge_error_display_parse_error() {
        let err = BridgeError::ParseError("bad json".to_string());
        assert!(err.to_string().contains("parse error"));
    }

    #[test]
    fn bridge_error_display_exit_code() {
        let err = BridgeError::ExitCode(9, "boom".to_string());
        assert!(err.to_string().contains("exit code 9"));
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
        assert_eq!(
            BridgeError::BinaryNotFound("x".to_string()),
            BridgeError::BinaryNotFound("x".to_string())
        );
    }

    #[test]
    fn bridge_error_equality_parse_error() {
        assert_eq!(
            BridgeError::ParseError("a".to_string()),
            BridgeError::ParseError("a".to_string())
        );
    }

    #[test]
    fn bridge_error_equality_exit_code() {
        assert_eq!(
            BridgeError::ExitCode(1, "a".to_string()),
            BridgeError::ExitCode(1, "a".to_string())
        );
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
        assert!(matches!(err, BridgeError::BinaryNotFound(_)));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_permission_denied_direct_path_maps_exit_code() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("deny-bin");
        write_non_executable(&bin, "#!/bin/sh\nprintf '{\"ok\":true}'\n");

        let b: SubprocessBridge<Value> = SubprocessBridge::new(bin.to_string_lossy().as_ref());
        let err = b.invoke(&[]).unwrap_err();
        assert!(matches!(err, BridgeError::BinaryNotFound(_)));
    }

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

    #[test]
    fn invoke_no_args_with_sh_c() {
        let b = bridge("sh");
        let out = b.invoke(&["-c", "printf '{\"ok\":true}'"]).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn parse_error_preserves_original_message_text() {
        let b = bridge("sh");
        let err = b.invoke(&["-c", "printf 'oops'"]).unwrap_err();
        if let BridgeError::ParseError(msg) = err {
            assert!(msg.contains("expected value") || msg.contains("stdout preview"));
        } else {
            panic!("expected parse error");
        }
    }

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
        let err = BridgeError::ExitCode(-1, "x".to_string());
        assert_eq!(err, BridgeError::ExitCode(-1, "x".to_string()));
    }

    #[test]
    fn invoke_success_with_whitespace_json() {
        let b = bridge("sh");
        let out = b.invoke(&["-c", "printf '  {\"ok\":true}  '"]).unwrap();
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn invoke_parse_array_json() {
        let b = bridge("sh");
        let out = b.invoke(&["-c", "printf '[1,2,3]'"]).unwrap();
        assert_eq!(out, json!([1, 2, 3]));
    }

    #[test]
    fn invoke_parse_nested_json_object() {
        let b = bridge("sh");
        let out = b
            .invoke(&["-c", "printf '{\"a\":{\"b\":{\"c\":1}}}'"])
            .unwrap();
        assert_eq!(out["a"]["b"]["c"], 1);
    }
}
