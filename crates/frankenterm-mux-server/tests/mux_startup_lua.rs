//! A `mux-startup` handler in a Lua config can use `wezterm.mux` to open
//! windows on the headless server, as it can in the GUI.
//!
//! Runs the real binary in a hermetic environment with a Lua config whose
//! handler calls `wezterm.mux.spawn_window` and logs the new window id.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const STARTUP_BUDGET: Duration = Duration::from_secs(60);
const SPAWNED_MARKER: &str = "mux-startup spawned window";

/// A short private directory: unix socket paths are limited to ~104 bytes.
fn short_temp_dir(label: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!("ftmx-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn hermetic_command(root: &Path) -> Command {
    for sub in ["home", "config", "cache", "data", "state", "runtime", "tmp"] {
        std::fs::create_dir_all(root.join(sub)).expect("create hermetic dir");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_frankenterm-mux-server"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root.join("tmp"))
        .env("FRANKENTERM_LUA_CONFIG", "1")
        .env("RUST_LOG", "info")
        .stdin(Stdio::null());
    command
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn mux_startup_handler_can_spawn_a_window() {
    let root = short_temp_dir("lua");
    let socket = root.join("mux.sock");
    let config = root.join("frankenterm.lua");
    std::fs::write(
        &config,
        format!(
            r#"local wezterm = require 'wezterm'
wezterm.on('mux-startup', function()
  local _tab, _pane, window = wezterm.mux.spawn_window {{ args = {{ '/bin/cat' }} }}
  wezterm.log_info('{SPAWNED_MARKER} ' .. tostring(window:window_id()))
end)
return {{
  unix_domains = {{
    {{ name = 'isolated', socket_path = '{}', no_serve_automatically = true }},
  }},
}}
"#,
            socket.display()
        ),
    )
    .expect("write config");
    let stderr_path = root.join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");

    let mut server = KillOnDrop(
        hermetic_command(&root)
            .arg("--config-file")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn frankenterm-mux-server"),
    );

    let started = Instant::now();
    loop {
        let log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        if log.contains(SPAWNED_MARKER) {
            break;
        }
        assert!(
            !log.contains("while processing mux-startup event"),
            "the mux-startup handler failed: {log}"
        );
        if let Some(status) = server.0.try_wait().expect("poll server") {
            panic!("server exited {status} before mux-startup ran: {log}");
        }
        assert!(
            started.elapsed() < STARTUP_BUDGET,
            "mux-startup did not log {SPAWNED_MARKER:?} within {STARTUP_BUDGET:?}: {log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}
