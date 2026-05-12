//! Real wezterm-mux-server subprocess fixture for no-mocks integration tests.
//!
//! Spawns a hermetic `wezterm-mux-server` instance with:
//! - A fresh `HOME` (TempDir) so the global pid lock at
//!   `~/.local/share/wezterm/pid` from the user's interactive session
//!   does NOT block test invocations.
//! - A unix domain socket inside that TempDir.
//! - A generated `frankenterm.toml` inside that TempDir so the subprocess has
//!   a single, explicit config source.
//! - A persistent `default_prog` loop so the default pane and spawned
//!   default-program panes stay alive across follow-up `list_panes` calls.
//! - A mux-server binary selected from the current ft build when available
//!   (`FT_WEZTERM_MUX_SERVER`, Cargo's bin env, or the workspace target dir),
//!   or built from this workspace before falling back to a system
//!   `wezterm-mux-server`.
//!
//! Returns a real `WeztermClient` configured `with_socket(...)` against the
//! hermetic socket. When the vendored mux backend is available, the client is
//! also given a direct mux pool so tests do not depend on a system `wezterm`
//! CLI binary.
//!
//! ## Usage
//! ```ignore
//! use common::wezterm_subprocess::WeztermSubprocessFixture;
//!
//! let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux-server");
//! let client = fixture.client();
//! let panes = client.list_panes().await?;  // hits the real subprocess
//! // fixture drops -> SIGTERM mux-server, remove tempdir
//! ```
//!
//! ## Skip semantics
//! Tests using this fixture should gate on `FT_REAL_WEZTERM_TESTS=1` so CI
//! lanes without a compatible wezterm-mux-server binary (Linux-only
//! sandboxes, containerized runners, or system binaries with codec skew)
//! skip cleanly. Use `should_run()` for the gate.
//!
//! Beads: ft-dvgzi, ft-2funa.

#![allow(dead_code)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use frankenterm_core::wezterm::WeztermClient;
use tempfile::TempDir;

/// Returns true iff the live test should attempt to spawn a real
/// wezterm-mux-server. Gates on the `FT_REAL_WEZTERM_TESTS` env var so
/// regular `cargo test` runs do not require the binary on PATH.
pub fn should_run() -> bool {
    std::env::var("FT_REAL_WEZTERM_TESTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Errors from fixture spawn / lifecycle.
#[derive(Debug)]
pub enum FixtureError {
    BinaryNotFound(String),
    ConfigSerialize(toml::ser::Error),
    SpawnFailed(std::io::Error),
    SocketTimeout {
        binary: PathBuf,
        path: PathBuf,
        stdout: String,
        stderr: String,
    },
    EarlyExit {
        binary: PathBuf,
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
    },
    TempDir(std::io::Error),
}

impl std::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BinaryNotFound(p) => write!(f, "wezterm-mux-server binary not found at {p}"),
            Self::ConfigSerialize(e) => write!(f, "failed to serialize mux-server config: {e}"),
            Self::SpawnFailed(e) => write!(f, "failed to spawn wezterm-mux-server: {e}"),
            Self::SocketTimeout {
                binary,
                path,
                stdout,
                stderr,
            } => write!(
                f,
                "timed out waiting for socket at {} from {}; stdout={}; stderr={}",
                path.display(),
                binary.display(),
                format_child_output(stdout),
                format_child_output(stderr)
            ),
            Self::EarlyExit {
                binary,
                status,
                stdout,
                stderr,
            } => write!(
                f,
                "wezterm-mux-server {} exited early: {status:?}; stdout={}; stderr={}",
                binary.display(),
                format_child_output(stdout),
                format_child_output(stderr)
            ),
            Self::TempDir(e) => write!(f, "failed to create test tempdir: {e}"),
        }
    }
}

impl std::error::Error for FixtureError {}

/// Hermetic wezterm-mux-server subprocess.
pub struct WeztermSubprocessFixture {
    home_dir: TempDir,
    socket_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    child: Option<Child>,
}

impl WeztermSubprocessFixture {
    /// Spawn a new wezterm-mux-server with a hermetic HOME and socket. Blocks
    /// until the socket file appears (≤ 30s) or returns an error.
    pub fn spawn() -> Result<Self, FixtureError> {
        Self::spawn_with_default_prog(&["/bin/sh", "-c", "while :; do sleep 3600; done"])
    }

    /// Spawn a new wezterm-mux-server with a caller-provided default program.
    /// This keeps no-mock tests on a real mux/PTY boundary while allowing
    /// scenarios to choose an interactive echo program such as `/bin/cat`.
    pub fn spawn_with_default_prog(default_prog: &[&str]) -> Result<Self, FixtureError> {
        let bin = locate_explicit_mux_binary()
            .or_else(build_mux_binary)
            .or_else(locate_current_mux_binary)
            .or_else(locate_system_mux_binary)
            .ok_or_else(|| {
                FixtureError::BinaryNotFound(
                "wezterm-mux-server (FT_WEZTERM_MUX_SERVER, current target dir, PATH, or Homebrew)"
                    .into(),
            )
            })?;

        let home = TempDir::new().map_err(FixtureError::TempDir)?;
        let socket_path = home.path().join("mux.sock");
        let stdout_path = home.path().join("mux-server.stdout.log");
        let stderr_path = home.path().join("mux-server.stderr.log");
        let config_path = home.path().join("frankenterm.toml");
        let config_toml = mux_server_config_toml(&socket_path, default_prog)?;
        fs::write(&config_path, config_toml).map_err(FixtureError::SpawnFailed)?;

        let mut cmd = Command::new(&bin);
        cmd.env("HOME", home.path())
            .env("XDG_RUNTIME_DIR", home.path())
            // Strip env vars that would inadvertently target the user's
            // interactive session.
            .env_remove("WEZTERM_UNIX_SOCKET")
            .env_remove("WEZTERM_PANE")
            .arg("--config-file")
            .arg(&config_path)
            .stdout(Stdio::from(
                File::create(&stdout_path).map_err(FixtureError::SpawnFailed)?,
            ))
            .stderr(Stdio::from(
                File::create(&stderr_path).map_err(FixtureError::SpawnFailed)?,
            ));

        let mut child = cmd.spawn().map_err(FixtureError::SpawnFailed)?;

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if socket_path.exists() {
                // mux-server creates the socket on bind, but we briefly wait
                // for it to start accepting connections.
                std::thread::sleep(Duration::from_millis(75));
                return Ok(Self {
                    home_dir: home,
                    socket_path,
                    stdout_path,
                    stderr_path,
                    child: Some(child),
                });
            }
            if let Ok(Some(status)) = child.try_wait() {
                let (stdout, stderr) = read_child_output(&stdout_path, &stderr_path);
                return Err(FixtureError::EarlyExit {
                    binary: bin.clone(),
                    status,
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let (stdout, stderr) = read_child_output(&stdout_path, &stderr_path);
                return Err(FixtureError::SocketTimeout {
                    binary: bin.clone(),
                    path: socket_path,
                    stdout,
                    stderr,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Path to the hermetic mux socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Path to the hermetic HOME directory (for tests that need to plant
    /// additional state there, e.g. wezterm config overrides).
    pub fn home_dir(&self) -> &Path {
        self.home_dir.path()
    }

    /// Construct a real `WeztermClient` pointed at the hermetic socket.
    pub fn client(&self) -> WeztermClient {
        let client = WeztermClient::with_socket(self.socket_path.display().to_string());
        #[cfg(all(feature = "vendored", unix))]
        {
            let mut mux = frankenterm_core::vendored::DirectMuxClientConfig::default()
                .with_socket_path(self.socket_path.clone());
            mux.read_timeout = Duration::from_secs(30);
            mux.write_timeout = Duration::from_secs(30);
            let pool = frankenterm_core::vendored::MuxPoolConfig {
                mux,
                ..frankenterm_core::vendored::MuxPoolConfig::default()
            };
            client.with_mux_pool(std::sync::Arc::new(
                frankenterm_core::vendored::MuxPool::new(pool),
            ))
        }
        #[cfg(not(all(feature = "vendored", unix)))]
        {
            client
        }
    }

    /// Construct a `WeztermHandle` for tests that need to plug into
    /// watchdog/restorer/snapshot APIs.
    pub fn handle(&self) -> frankenterm_core::wezterm::WeztermHandle {
        std::sync::Arc::new(self.client())
    }

    /// Process id of the running mux-server (for fault-injection tests in
    /// ft-2funa: kill -SIGSTOP, etc.).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// **Fault-injection helper (ft-2funa).** Kills the mux subprocess
    /// without dropping the fixture, leaving the socket file present but
    /// dead. Subsequent `WeztermClient` calls hit the strict-socket guard
    /// (ft-dvgzi.1.1) and return `Wezterm(NotRunning)` /
    /// `Wezterm(CommandFailed("failed to connect"))`. The TempDir + socket
    /// path are still cleaned up on Drop.
    pub fn kill_mux(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for WeztermSubprocessFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::thread::panicking() {
            let (stdout, stderr) = read_child_output(&self.stdout_path, &self.stderr_path);
            eprintln!(
                "wezterm-mux-server fixture logs on panic: stdout={}; stderr={}",
                format_child_output(&stdout),
                format_child_output(&stderr)
            );
        }
        // home_dir TempDir's Drop removes the temp directory automatically.
    }
}

#[derive(serde::Serialize)]
struct FixtureMuxServerConfig<'a> {
    default_prog: Vec<&'a str>,
    unix_domains: Vec<FixtureUnixDomain>,
}

#[derive(serde::Serialize)]
struct FixtureUnixDomain {
    name: &'static str,
    socket_path: String,
    skip_permissions_check: bool,
}

fn mux_server_config_toml(
    socket_path: &Path,
    default_prog: &[&str],
) -> Result<String, FixtureError> {
    toml::to_string(&FixtureMuxServerConfig {
        default_prog: default_prog.to_vec(),
        unix_domains: vec![FixtureUnixDomain {
            name: "ft-test",
            socket_path: socket_path.display().to_string(),
            skip_permissions_check: true,
        }],
    })
    .map_err(FixtureError::ConfigSerialize)
}

fn read_child_output(stdout_path: &Path, stderr_path: &Path) -> (String, String) {
    (read_log_file(stdout_path), read_log_file(stderr_path))
}

fn read_log_file(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|err| format!("<failed to read {}: {err}>", path.display()))
}

fn format_child_output(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        "<empty>".to_string()
    } else {
        trimmed.to_string()
    }
}

fn locate_current_mux_binary() -> Option<PathBuf> {
    let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    mux_binary_candidates_from(None, None, cargo_target_dir, &manifest_dir)
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn locate_explicit_mux_binary() -> Option<PathBuf> {
    let explicit = std::env::var_os("FT_WEZTERM_MUX_SERVER").map(PathBuf::from);
    let cargo_bin = std::env::var_os("CARGO_BIN_EXE_frankenterm-mux-server").map(PathBuf::from);
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    mux_binary_candidates_from(explicit, cargo_bin, None, &manifest_dir)
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn locate_system_mux_binary() -> Option<PathBuf> {
    // Try $PATH via `which`-style probe after ft-built candidates. A system
    // wezterm-mux-server may speak a different binary codec than this checkout.
    if let Ok(output) = Command::new("/usr/bin/which")
        .arg("wezterm-mux-server")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    let homebrew = PathBuf::from("/opt/homebrew/bin/wezterm-mux-server");
    if homebrew.exists() {
        return Some(homebrew);
    }
    let usr_local = PathBuf::from("/usr/local/bin/wezterm-mux-server");
    if usr_local.exists() {
        return Some(usr_local);
    }
    None
}

fn build_mux_binary() -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().and_then(Path::parent)?;
    let cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"));

    eprintln!("building frankenterm-mux-server for real mux fixture");
    let status = Command::new(cargo)
        .current_dir(workspace_root)
        .arg("build")
        .arg("-p")
        .arg("frankenterm-mux-server")
        .status()
        .ok()?;

    if !status.success() {
        return None;
    }

    mux_binary_candidates()
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn mux_binary_candidates() -> Vec<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let explicit = std::env::var_os("FT_WEZTERM_MUX_SERVER").map(PathBuf::from);
    let cargo_bin = std::env::var_os("CARGO_BIN_EXE_frankenterm-mux-server").map(PathBuf::from);
    let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    mux_binary_candidates_from(explicit, cargo_bin, cargo_target_dir, manifest_dir)
}

fn mux_binary_candidates_from(
    explicit: Option<PathBuf>,
    cargo_bin: Option<PathBuf>,
    cargo_target_dir: Option<PathBuf>,
    manifest_dir: &Path,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let workspace_root = manifest_dir.parent().and_then(Path::parent);

    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        candidates.push(path);
    }
    if let Some(path) = cargo_bin.filter(|path| !path.as_os_str().is_empty()) {
        candidates.push(path);
    }
    if let Some(target_dir) = cargo_target_dir.filter(|path| !path.as_os_str().is_empty()) {
        let target_dir = if target_dir.is_absolute() {
            target_dir
        } else if let Some(workspace_root) = workspace_root {
            workspace_root.join(target_dir)
        } else {
            target_dir
        };
        candidates.push(target_dir.join("debug").join("frankenterm-mux-server"));
    }
    if let Some(workspace_root) = workspace_root {
        candidates.push(
            workspace_root
                .join("target")
                .join("debug")
                .join("frankenterm-mux-server"),
        );
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_binary_candidates_prefer_ft_built_binary_before_workspace_target() {
        let manifest_dir = Path::new("/repo/crates/frankenterm-core");
        let candidates = mux_binary_candidates_from(
            Some(PathBuf::from("/tmp/explicit/frankenterm-mux-server")),
            Some(PathBuf::from("/tmp/cargo-bin/frankenterm-mux-server")),
            Some(PathBuf::from("/tmp/target")),
            manifest_dir,
        );

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/tmp/explicit/frankenterm-mux-server"),
                PathBuf::from("/tmp/cargo-bin/frankenterm-mux-server"),
                PathBuf::from("/tmp/target/debug/frankenterm-mux-server"),
                PathBuf::from("/repo/target/debug/frankenterm-mux-server"),
            ]
        );
    }

    #[test]
    fn mux_binary_candidates_resolve_relative_cargo_target_dir_from_workspace_root() {
        let manifest_dir = Path::new("/repo/crates/frankenterm-core");
        let candidates = mux_binary_candidates_from(
            None,
            None,
            Some(PathBuf::from("target/local-mux-proof")),
            manifest_dir,
        );

        assert_eq!(
            candidates[0],
            PathBuf::from("/repo/target/local-mux-proof/debug/frankenterm-mux-server"),
        );
    }
}
