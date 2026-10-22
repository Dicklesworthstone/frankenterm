//! No-mocks end-to-end test: a real `frankenterm-mux-server` built from this
//! workspace, in an isolated HOME/XDG tree, driven through the helper's own
//! session loop with in-memory stdin/stdout.
//!
//! The server binary comes from `FT_MOBILE_ATTACH_MUX_SERVER` (an absolute
//! path), else this test builds `frankenterm-mux-server` from the same
//! workspace into the same target directory (Cargo releases its build lock
//! before running tests).

#![cfg(unix)]

use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use frankenterm_core::runtime_async::{CompatRuntime, Runtime, RuntimeBuilder};
use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig, DirectMuxError};
use frankenterm_mobile_attach::panes::{PaneSummary, summarize};
use frankenterm_mobile_attach::session::{
    Event, PtySize, SessionConfig, SessionEnd, SessionError, run_session,
};
use frankenterm_term::TerminalSize;

const READY_BUDGET: Duration = Duration::from_secs(120);
const STEP_BUDGET: Duration = Duration::from_secs(60);
const DCS_PREFIX: &[u8] = b"\x1bPft-mobile;panes;";

fn mux_server_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("FT_MOBILE_ATTACH_MUX_SERVER") {
        let path = PathBuf::from(path);
        assert!(
            path.is_absolute(),
            "FT_MOBILE_ATTACH_MUX_SERVER must be absolute"
        );
        return path;
    }
    // <target>/<profile>/deps/<this test> -> <target>/<profile>
    let exe = std::env::current_exe().expect("test executable path");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("test executable lives in <target>/<profile>/deps");
    let target_dir = profile_dir.parent().expect("profile dir has a target dir");
    let profile = match profile_dir.file_name().and_then(|name| name.to_str()) {
        Some("debug") | None => "dev",
        Some(name) => name,
    };
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into());
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    eprintln!(
        "building frankenterm-mux-server (profile {profile}) into {}",
        target_dir.display()
    );
    let status = Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .args([
            "-p",
            "frankenterm-mux-server",
            "--bin",
            "frankenterm-mux-server",
        ])
        .args(["--profile", profile])
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .expect("run cargo build for frankenterm-mux-server");
    assert!(
        status.success(),
        "building frankenterm-mux-server failed: {status}"
    );
    let binary = profile_dir.join("frankenterm-mux-server");
    assert!(binary.is_file(), "missing {}", binary.display());
    binary
}

/// A real mux server in a private HOME/XDG tree, killed on drop.
struct MuxServer {
    child: Child,
    root: tempfile::TempDir,
    socket: PathBuf,
}

impl MuxServer {
    fn start() -> Self {
        let binary = mux_server_binary();
        // Unix socket paths are short (~104 bytes): stay directly under /tmp.
        let root = tempfile::Builder::new()
            .prefix("ftma")
            .tempdir_in("/tmp")
            .expect("temp dir");
        for sub in ["home", "config", "cache", "data", "state", "runtime"] {
            std::fs::create_dir_all(root.path().join(sub)).expect("create isolated dir");
        }
        let socket = root.path().join("s.sock");
        let config = root.path().join("frankenterm.toml");
        std::fs::write(
            &config,
            format!(
                "default_prog = [\"/bin/sh\"]\n\n[[unix_domains]]\nname = \"mobile-attach-test\"\nsocket_path = \"{}\"\nskip_permissions_check = true\n",
                socket.display()
            ),
        )
        .expect("write mux config");
        let stdout = std::fs::File::create(root.path().join("mux.stdout.log")).expect("log");
        let stderr = std::fs::File::create(root.path().join("mux.stderr.log")).expect("log");
        let child = Command::new(&binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .env("HOME", root.path().join("home"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
            .env("TMPDIR", "/tmp")
            .current_dir(root.path())
            .arg("--config-file")
            .arg(&config)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn frankenterm-mux-server");
        Self {
            child,
            root,
            socket,
        }
    }

    fn logs(&self) -> String {
        let read =
            |name: &str| std::fs::read_to_string(self.root.path().join(name)).unwrap_or_default();
        format!(
            "--- mux stdout ---\n{}\n--- mux stderr ---\n{}",
            read("mux.stdout.log"),
            read("mux.stderr.log")
        )
    }
}

impl Drop for MuxServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if thread::panicking() {
            eprintln!("{}", self.logs());
        }
    }
}

/// The test's own mux connection, used to observe and set up the server.
struct Probe {
    client: DirectMuxClient,
    runtime: Runtime,
}

impl Probe {
    fn connect(server: &mut MuxServer) -> Self {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("probe runtime");
        let deadline = Instant::now() + READY_BUDGET;
        loop {
            if let Some(status) = server.child.try_wait().expect("poll mux server") {
                panic!("mux server exited early ({status}):\n{}", server.logs());
            }
            if server.socket.exists() {
                let config = DirectMuxClientConfig::default().with_socket_path(&server.socket);
                if let Ok(client) = runtime.block_on(Box::pin(DirectMuxClient::connect(config))) {
                    return Self { client, runtime };
                }
            }
            assert!(
                Instant::now() < deadline,
                "mux server never accepted a connection:\n{}",
                server.logs()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn panes(&mut self) -> Vec<PaneSummary> {
        let list = self
            .runtime
            .block_on(Box::pin(self.client.list_panes()))
            .expect("list panes");
        summarize(&list)
    }

    fn pane(&mut self, id: u64) -> Option<PaneSummary> {
        self.panes().into_iter().find(|pane| pane.id == id)
    }

    fn write(&mut self, pane: u64, bytes: &[u8]) {
        self.runtime
            .block_on(Box::pin(self.client.write_to_pane(pane, bytes.to_vec())))
            .expect("write to the test's own pane");
    }

    /// Drop the client before its runtime, then the runtime the way the
    /// project's harnesses do, so thread teardown cannot abort the test.
    fn close(self) {
        let Self { client, runtime } = self;
        drop(client);
        let _ = std::panic::catch_unwind(AssertUnwindSafe(move || drop(runtime)));
        let _ = std::panic::catch_unwind(AssertUnwindSafe(
            frankenterm_core::runtime_async::clear_runtime_handle,
        ));
    }

    fn spawn_window(&mut self) -> u64 {
        let spawn = codec::SpawnV2 {
            domain: config::keyassignment::SpawnTabDomain::DefaultDomain,
            window_id: None,
            command: None,
            command_dir: None,
            size: TerminalSize::default(),
            workspace: mux::DEFAULT_WORKSPACE.to_string(),
        };
        let spawned = self
            .runtime
            .block_on(Box::pin(self.client.spawn_v2(spawn)))
            .expect("spawn a second pane");
        spawned.pane_id as u64
    }
}

/// The session's stdout, shared with the test.
#[derive(Clone, Default)]
struct SharedOutput(Arc<Mutex<Vec<u8>>>);

impl SharedOutput {
    fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl Write for SharedOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Every pane-list DCS in `output`, decoded, after checking the payload is
/// compact printable ASCII.
fn pane_lists(output: &[u8]) -> Vec<serde_json::Value> {
    let mut lists = Vec::new();
    let mut rest = output;
    while let Some(start) = find(rest, DCS_PREFIX) {
        let body = &rest[start + DCS_PREFIX.len()..];
        let Some(end) = find(body, b"\x1b\\") else {
            break;
        };
        let json = &body[..end];
        assert!(
            json.iter().all(|b| (0x20..0x7f).contains(b)),
            "DCS payload must be printable ASCII"
        );
        lists.push(serde_json::from_slice(json).expect("DCS payload is JSON"));
        rest = &body[end..];
    }
    lists
}

fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + STEP_BUDGET;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn size_of(probe: &mut Probe, pane: u64) -> Option<(usize, usize)> {
    probe.pane(pane).map(|pane| (pane.cols, pane.rows))
}

/// An independent raw-PTY byte oracle, unaffected by rendering or shell echo.
struct ByteCapture {
    bytes: PathBuf,
    done: PathBuf,
}

impl ByteCapture {
    fn start(probe: &mut Probe, pane: u64, root: &Path, name: &str) -> Self {
        let bytes = root.join(format!("{name}.byte"));
        let ready = root.join(format!("{name}.ready"));
        let done = root.join(format!("{name}.done"));
        // The paths are under the test's /tmp/ftma<random> directory; names
        // are fixed below, never remote or user-provided command material.
        let command = format!(
            "stty raw -echo; : > '{}'; dd bs=1 count=1 of='{}' 2>/dev/null; stty sane; : > '{}'\r",
            ready.display(), bytes.display(), done.display()
        );
        probe.write(pane, command.as_bytes());
        wait_until("the raw byte reader", || {
            (ready.is_file() && bytes.is_file()).then_some(())
        });
        Self { bytes, done }
    }

    fn expect(&self, expected: u8) {
        wait_until("the captured byte and restored terminal", || {
            self.done.is_file().then_some(())
        });
        assert_eq!(
            std::fs::read(&self.bytes).expect("read captured byte"),
            [expected]
        );
    }
}

#[test]
fn attaches_types_resizes_switches_and_detaches_against_a_real_mux_server() {
    let mut server = MuxServer::start();
    let mut probe = Probe::connect(&mut server);

    let first = wait_until("the mux's default pane", || {
        probe.panes().into_iter().next()
    });
    let first_original = (first.cols, first.rows);
    let second = probe.spawn_window();
    let second_original = wait_until("the spawned pane", || size_of(&mut probe, second));
    assert_ne!(first.id, second);

    let output = SharedOutput::default();
    let (events, inbox) = mpsc::channel();
    let mut config = SessionConfig::new(
        server.socket.clone(),
        Some(first.id),
        PtySize {
            rows: 20,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        },
    );
    config.topology_interval = Duration::from_millis(500);
    let mut session_out = output.clone();
    let session = thread::spawn(move || -> Result<SessionEnd, SessionError> {
        run_session(&config, &inbox, &mut session_out)
    });

    // 1. The pane list arrives as a private DCS naming the attached pane.
    let list = wait_until("the pane-list DCS", || {
        pane_lists(&output.snapshot()).into_iter().next()
    });
    assert_eq!(list["attached"], first.id);
    let ids: Vec<u64> = list["panes"]
        .as_array()
        .expect("panes array")
        .iter()
        .map(|pane| pane["id"].as_u64().expect("pane id"))
        .collect();
    assert!(ids.contains(&first.id) && ids.contains(&second), "{ids:?}");
    assert!(
        find(&output.snapshot(), b"\x1b[?2004h").is_some(),
        "bracketed paste is enabled on the phone"
    );

    // 2. The attached pane takes the phone's size.
    wait_until("the pane to take the phone's 60x20", || {
        (size_of(&mut probe, first.id) == Some((60, 20))).then_some(())
    });

    // 3. Keystrokes reach the shell and its output comes back rendered.
    events
        .send(Event::Input(b"echo HELLO-$((40+2))\r".to_vec()))
        .unwrap();
    wait_until("HELLO-42 in the rendered output", || {
        find(&output.snapshot(), b"HELLO-42").map(|_| ())
    });

    // 4. A window-size change resizes the pane again.
    events
        .send(Event::Resize(PtySize {
            rows: 18,
            cols: 70,
            pixel_width: 0,
            pixel_height: 0,
        }))
        .unwrap();
    wait_until("the pane to follow the phone to 70x18", || {
        (size_of(&mut probe, first.id) == Some((70, 18))).then_some(())
    });

    // 5. The app switches panes with a control DCS, split across two reads.
    let mark = output.snapshot().len();
    let control = format!("\x1bPft-mobile;attach;{second}\x1b\\").into_bytes();
    events.send(Event::Input(control[..7].to_vec())).unwrap();
    events.send(Event::Input(control[7..].to_vec())).unwrap();
    wait_until("a pane-list DCS naming the second pane", || {
        pane_lists(&output.snapshot()[mark..])
            .iter()
            .any(|list| list["attached"] == second)
            .then_some(())
    });
    wait_until("the first pane's original size to come back", || {
        (size_of(&mut probe, first.id) == Some(first_original)).then_some(())
    });
    wait_until("the second pane to take the phone's 70x18", || {
        (size_of(&mut probe, second) == Some((70, 18))).then_some(())
    });
    let mark = output.snapshot().len();
    events
        .send(Event::Input(b"echo SECOND-$((1+1))\r".to_vec()))
        .unwrap();
    wait_until("SECOND-2 from the second pane", || {
        find(&output.snapshot()[mark..], b"SECOND-2").map(|_| ())
    });
    assert!(
        find(&output.snapshot(), b"\x1bPft-mobile;attach").is_none(),
        "control messages are consumed, never echoed"
    );

    // 6. `panes` re-announces on request.
    let before = pane_lists(&output.snapshot()).len();
    events
        .send(Event::Input(b"\x1bPft-mobile;panes\x1b\\".to_vec()))
        .unwrap();
    wait_until("a re-announced pane list", || {
        (pane_lists(&output.snapshot()).len() > before).then_some(())
    });

    // 7. An ambiguous key before a control belongs to the old pane. Both
    // reads use real raw PTYs so escape rendering cannot hide misdelivery.
    let old_capture = ByteCapture::start(&mut probe, second, server.root.path(), "old-escape");
    let new_capture = ByteCapture::start(&mut probe, first.id, server.root.path(), "new-key");
    let mut switch = b"\x1b".to_vec();
    switch.extend_from_slice(format!("\x1bPft-mobile;attach;{}\x1b\\X", first.id).as_bytes());
    events.send(Event::Input(switch)).unwrap();
    new_capture.expect(b'X');
    old_capture.expect(0x1b);

    // A flush alone does not retire InputParser's incomplete UTF-8. Bytes
    // on opposite sides of an attach must not combine into a key for the
    // successor. The next control retires the orphan continuation byte.
    let utf8_capture = ByteCapture::start(&mut probe, second, server.root.path(), "after-utf8");
    let mut switch = vec![0xc3];
    switch.extend_from_slice(format!("\x1bPft-mobile;attach;{second}\x1b\\").as_bytes());
    switch.extend_from_slice(b"\xa9\x1bPft-mobile;panes\x1b\\Y");
    events.send(Event::Input(switch)).unwrap();
    utf8_capture.expect(b'Y');

    // 8. Termination detaches: the second pane gets its size back and the
    // phone's modes are reset.
    events.send(Event::Terminate).unwrap();
    let end = session
        .join()
        .expect("session thread")
        .expect("session ends cleanly");
    assert_eq!(end, SessionEnd::Terminated);
    wait_until("the second pane's original size to come back", || {
        (size_of(&mut probe, second) == Some(second_original)).then_some(())
    });
    let output = output.snapshot();
    assert!(
        output.ends_with(b"\x1b[0m\x1b[?25h\x1b[0 q\x1b[?2004l\r\n"),
        "the phone's terminal modes are reset on exit"
    );
    probe.close();
}

#[test]
fn lost_or_missing_selected_panes_end_without_typing_into_a_survivor() {
    let mut server = MuxServer::start();
    let mut probe = Probe::connect(&mut server);
    let first = wait_until("the mux's default pane", || {
        probe.panes().into_iter().next()
    });
    let second = probe.spawn_window();
    let original = wait_until("the surviving pane", || size_of(&mut probe, second));
    let survivor = ByteCapture::start(&mut probe, second, server.root.path(), "after-loss");
    let output = SharedOutput::default();
    let (events, inbox) = mpsc::channel();
    let mut config = SessionConfig::new(
        server.socket.clone(),
        Some(first.id),
        PtySize {
            rows: 20,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        },
    );
    config.topology_interval = Duration::from_millis(100);
    let mut session_out = output.clone();
    let session = thread::spawn(move || run_session(&config, &inbox, &mut session_out));
    wait_until("the selected pane announcement", || {
        pane_lists(&output.snapshot())
            .iter()
            .any(|list| list["attached"] == first.id)
            .then_some(())
    });

    events
        .send(Event::Input(b"\x1b[200~NEVER-REDIRECT".to_vec()))
        .unwrap();
    events
        .send(Event::Resize(PtySize {
            rows: 20,
            cols: 61,
            pixel_width: 0,
            pixel_height: 0,
        }))
        .unwrap();
    // Resize is serviced after the earlier input, proving that the helper
    // has consumed the unfinished paste before the source disappears.
    wait_until("the input-ordering resize", || {
        (size_of(&mut probe, first.id) == Some((61, 20))).then_some(())
    });
    probe.write(first.id, b"exit\r");
    wait_until("the selected pane to close", || {
        probe.pane(first.id).is_none().then_some(())
    });
    // It is also valid for source loss to have already closed the receiver.
    let _ = events.send(Event::Input(b"-TAIL\x1b[201~Q".to_vec()));
    wait_until("the helper to stop after source loss", || {
        session.is_finished().then_some(())
    });
    match session.join().expect("session thread") {
        Err(SessionError::PaneNotFound(id)) => assert_eq!(id, first.id),
        Err(SessionError::Mux(DirectMuxError::RemoteRejection(rejection))) => {
            assert_eq!(rejection.code, codec::MuxErrorCode::PANE_NOT_FOUND);
        }
        other => panic!("expected selected-pane loss, got {other:?}"),
    }
    assert!(
        pane_lists(&output.snapshot())
            .iter()
            .all(|list| list["attached"] == first.id),
        "the helper must never announce a fallback destination"
    );
    assert_eq!(size_of(&mut probe, second), Some(original));
    // A positive byte through a separate mux client proves the survivor is
    // still usable and that it did not receive the abandoned paste or key.
    probe.write(second, b"Z");
    survivor.expect(b'Z');

    // A deliberate new invocation can still choose the remaining pane.
    // An explicit request for the closed pane must then fail before the
    // following key, instead of continuing on that initial selection.
    let missing = ByteCapture::start(&mut probe, second, server.root.path(), "missing-selection");
    let output = SharedOutput::default();
    let (events, inbox) = mpsc::channel();
    let config = SessionConfig::new(
        server.socket.clone(),
        None,
        PtySize {
            rows: 20,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        },
    );
    let mut session_out = output.clone();
    let session = thread::spawn(move || run_session(&config, &inbox, &mut session_out));
    wait_until("the new invocation to select the surviving pane", || {
        pane_lists(&output.snapshot())
            .iter()
            .any(|list| list["attached"] == second)
            .then_some(())
    });
    events
        .send(Event::Input(
            format!("\x1bPft-mobile;attach;{}\x1b\\R", first.id).into_bytes(),
        ))
        .unwrap();
    wait_until("the helper to reject the missing explicit target", || {
        session.is_finished().then_some(())
    });
    assert!(matches!(
        session.join().expect("session thread"),
        Err(SessionError::PaneNotFound(id)) if id == first.id
    ));
    wait_until("the still-current pane's original size after a missing target", || {
        (size_of(&mut probe, second) == Some(original)).then_some(())
    });
    probe.write(second, b"Z");
    missing.expect(b'Z');
    probe.close();
}
