//! Real wezterm-mux-server subprocess fixture for no-mocks integration tests.
//!
//! Spawns a hermetic `wezterm-mux-server` instance with:
//! - A fresh retained `HOME` so the global pid lock at
//!   `~/.local/share/wezterm/pid` from the user's interactive session
//!   does NOT block test invocations.
//! - A unix domain socket inside that TempDir.
//! - A generated `frankenterm.toml` inside that TempDir so the subprocess has
//!   a single, explicit config source.
//! - A persistent `default_prog` so the default pane and spawned
//!   default-program panes stay alive across follow-up `list_panes` calls.
//! - An explicit mux-server binary supplied by `FT_WEZTERM_MUX_SERVER` or
//!   Cargo's binary environment. The invoking RCH/DSR lane binds that artifact
//!   to its source; this fixture never builds or discovers a system binary.
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
//! // fixture drops -> stop owned mux-server; retain workspace and logs
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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    CapacityExhausted,
    PollFailed(std::io::Error),
    ShutdownIndeterminate {
        pid: u32,
    },
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
            Self::CapacityExhausted => write!(f, "owned mux fixture capacity exhausted"),
            Self::PollFailed(error) => write!(f, "failed to observe owned mux process: {error}"),
            Self::ShutdownIndeterminate { pid } => {
                write!(
                    f,
                    "owned mux shutdown unconfirmed; retained child pid={pid}"
                )
            }
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

// Includes unconfirmed shutdowns. Never silently drop a Child after a failed
// reap, and never admit an unbounded number of unresolved fixture owners.
static MUX_FIXTURE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static RETAINED_MUX_CHILDREN: Mutex<Vec<OwnedMuxChild>> = Mutex::new(Vec::new());

struct MuxFixtureSlot;

impl MuxFixtureSlot {
    fn acquire() -> Result<Self, FixtureError> {
        RETAINED_MUX_CHILDREN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain_mut(|owner| !matches!(owner.child.try_wait(), Ok(Some(_))));
        MUX_FIXTURE_SLOTS
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                (active < 16).then_some(active + 1)
            })
            .map_err(|_| FixtureError::CapacityExhausted)?;
        Ok(Self)
    }
}

impl Drop for MuxFixtureSlot {
    fn drop(&mut self) {
        MUX_FIXTURE_SLOTS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct OwnedMuxChild {
    child: Child,
    _slot: MuxFixtureSlot,
}

fn settle_mux_child(mut owner: OwnedMuxChild) -> Result<(), FixtureError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    match owner.child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {
            // Signal only a leader whose unreaped ownership was just observed.
            // A signal error is not an exit receipt; continue bounded polling.
            let _ = owner.child.kill();
            while Instant::now() < deadline {
                match owner.child.try_wait() {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        }
        Err(_) => {}
    }
    let pid = owner.child.id();
    RETAINED_MUX_CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(owner);
    Err(FixtureError::ShutdownIndeterminate { pid })
}

/// Hermetic wezterm-mux-server subprocess.
pub struct WeztermSubprocessFixture {
    home_dir: PathBuf,
    socket_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    child: Option<OwnedMuxChild>,
}

impl WeztermSubprocessFixture {
    /// Spawn a new wezterm-mux-server with a hermetic HOME and socket. Blocks
    /// until the socket file appears (≤ 30s) or returns an error.
    pub fn spawn() -> Result<Self, FixtureError> {
        Self::spawn_with_default_prog(&["/bin/cat"])
    }

    /// Spawn a new wezterm-mux-server with a caller-provided default program.
    /// This keeps no-mock tests on a real mux/PTY boundary while allowing
    /// scenarios to choose an interactive echo program such as `/bin/cat`.
    pub fn spawn_with_default_prog(default_prog: &[&str]) -> Result<Self, FixtureError> {
        let bin = select_mux_binary(
            std::env::var_os("FT_WEZTERM_MUX_SERVER").map(PathBuf::from),
            std::env::var_os("CARGO_BIN_EXE_frankenterm-mux-server").map(PathBuf::from),
        )?;
        let metadata = fs::symlink_metadata(&bin)
            .map_err(|_| FixtureError::BinaryNotFound(bin.display().to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(FixtureError::BinaryNotFound(bin.display().to_string()));
        }

        let slot = MuxFixtureSlot::acquire()?;
        let home = TempDir::new().map_err(FixtureError::TempDir)?.keep();
        let socket_path = home.join("mux.sock");
        let stdout_path = home.join("mux-server.stdout.log");
        let stderr_path = home.join("mux-server.stderr.log");
        let config_path = home.join("frankenterm.toml");
        let config_toml = mux_server_config_toml(&socket_path, default_prog)?;
        fs::write(&config_path, config_toml).map_err(FixtureError::SpawnFailed)?;

        let mut cmd = Command::new(&bin);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &home)
            .env("XDG_CONFIG_HOME", &home)
            .current_dir(&home)
            .arg("--config-file")
            .arg(&config_path)
            .stdout(Stdio::from(
                File::create(&stdout_path).map_err(FixtureError::SpawnFailed)?,
            ))
            .stderr(Stdio::from(
                File::create(&stderr_path).map_err(FixtureError::SpawnFailed)?,
            ));
        if !default_prog.is_empty() {
            cmd.arg("--").args(default_prog);
        }

        let child = cmd.spawn().map_err(FixtureError::SpawnFailed)?;
        let mut fixture = Self {
            home_dir: home,
            socket_path: socket_path.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            child: Some(OwnedMuxChild { child, _slot: slot }),
        };

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = fixture
                .child
                .as_mut()
                .unwrap()
                .child
                .try_wait()
                .map_err(FixtureError::PollFailed)?;
            if let Some(status) = status {
                let (stdout, stderr) = read_child_output(&stdout_path, &stderr_path);
                return Err(FixtureError::EarlyExit {
                    binary: bin.clone(),
                    status,
                    stdout,
                    stderr,
                });
            }
            if socket_path.exists() {
                // The caller must still perform a real protocol request;
                // socket publication alone is not mux readiness evidence.
                return Ok(fixture);
            }
            if Instant::now() >= deadline {
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

    /// Path where the fixture captures mux-server stdout.
    pub fn stdout_path(&self) -> &Path {
        &self.stdout_path
    }

    /// Path where the fixture captures mux-server stderr.
    pub fn stderr_path(&self) -> &Path {
        &self.stderr_path
    }

    /// Snapshot the current mux-server stdout/stderr logs.
    pub fn child_output_snapshot(&self) -> (String, String) {
        read_child_output(&self.stdout_path, &self.stderr_path)
    }

    /// Path to the hermetic HOME directory (for tests that need to plant
    /// additional state there, e.g. wezterm config overrides).
    pub fn home_dir(&self) -> &Path {
        &self.home_dir
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
        self.child.as_ref().map(|owner| owner.child.id())
    }

    /// **Fault-injection helper (ft-2funa).** Kills the mux subprocess
    /// without dropping the fixture, leaving the socket file present but
    /// dead. Subsequent `WeztermClient` calls hit the strict-socket guard
    /// (ft-dvgzi.1.1) and return `Wezterm(NotRunning)` /
    /// `Wezterm(CommandFailed("failed to connect"))`. The workspace, socket
    /// and logs remain available for inspection after Drop.
    pub fn kill_mux(&mut self) {
        if let Some(owner) = self.child.take() {
            settle_mux_child(owner).expect("owned mux must settle before fixture proof succeeds");
        }
    }
}

impl Drop for WeztermSubprocessFixture {
    fn drop(&mut self) {
        if let Some(owner) = self.child.take() {
            if let Err(error) = settle_mux_child(owner) {
                eprintln!("MUX_FIXTURE_SETTLEMENT status=indeterminate error={error}");
            }
        }
        if std::thread::panicking() {
            let (stdout, stderr) = read_child_output(&self.stdout_path, &self.stderr_path);
            eprintln!(
                "wezterm-mux-server fixture logs on panic: stdout={}; stderr={}",
                format_child_output(&stdout),
                format_child_output(&stderr)
            );
        }
        // Retain the owned directory and logs even on success.
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
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let result =
        File::open(path).and_then(|file| file.take((LIMIT + 1) as u64).read_to_end(&mut bytes));
    match result {
        Ok(_) => {
            let truncated = bytes.len() > LIMIT;
            bytes.truncate(LIMIT);
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if truncated {
                text.push_str("\n<remaining log bytes retained on disk>");
            }
            text
        }
        Err(err) => format!("<failed to read {}: {err}>", path.display()),
    }
}

fn format_child_output(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        "<empty>".to_string()
    } else {
        trimmed.to_string()
    }
}

fn select_mux_binary(
    explicit: Option<PathBuf>,
    cargo_bin: Option<PathBuf>,
) -> Result<PathBuf, FixtureError> {
    explicit.or(cargo_bin).filter(|path| path.is_absolute()).ok_or_else(|| {
        FixtureError::BinaryNotFound(
            "supply an absolute FT_WEZTERM_MUX_SERVER or Cargo binary path from the qualified RCH/DSR build".to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_binary_selection_uses_explicit_artifact_without_discovery() {
        let selected = select_mux_binary(
            Some(PathBuf::from("/tmp/explicit/frankenterm-mux-server")),
            Some(PathBuf::from("/tmp/cargo-bin/frankenterm-mux-server")),
        )
        .unwrap();

        assert_eq!(
            selected,
            PathBuf::from("/tmp/explicit/frankenterm-mux-server")
        );
    }

    #[test]
    fn mux_binary_selection_refuses_missing_relative_and_empty_authority() {
        assert!(select_mux_binary(None, None).is_err());
        for path in [
            PathBuf::new(),
            PathBuf::from("target/debug/frankenterm-mux-server"),
        ] {
            assert!(
                select_mux_binary(
                    Some(path),
                    Some(PathBuf::from("/tmp/alternate/frankenterm-mux-server")),
                )
                .is_err()
            );
        }
        let selected = select_mux_binary(
            None,
            Some(PathBuf::from("/tmp/cargo-bin/frankenterm-mux-server")),
        )
        .unwrap();
        assert_eq!(
            selected,
            PathBuf::from("/tmp/cargo-bin/frankenterm-mux-server"),
        );
    }

    #[test]
    fn owned_mux_child_settlement_reaps_a_real_blocked_process() {
        let slot = MuxFixtureSlot::acquire().unwrap();
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let pid = child.id();
        let started = Instant::now();
        settle_mux_child(OwnedMuxChild { child, _slot: slot }).unwrap();
        assert!(started.elapsed() < Duration::from_secs(6));
        println!("MUX_FIXTURE_SETTLEMENT pid={pid} blocked_stdin=true leader_reaped=true");
    }

    #[test]
    fn mux_log_snapshot_is_bounded_and_retains_the_complete_file() {
        let root = TempDir::new().unwrap().keep();
        let path = root.join("owned-log");
        let bytes = vec![b'x'; 128 * 1024];
        fs::write(&path, &bytes).unwrap();
        let snapshot = read_log_file(&path);
        assert!(snapshot.len() < 65 * 1024);
        assert!(snapshot.ends_with("<remaining log bytes retained on disk>"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
