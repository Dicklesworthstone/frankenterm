//! ft-xxfwy.35: the headless server honors an explicit `--config-file`.
//!
//! Runs the real binary in a hermetic environment. A TOML config naming a
//! unix domain under a temp directory must make the server bind exactly that
//! socket (not `RUNTIME_DIR/sock`) and log it; an explicit config that does not
//! load must exit 1 naming the file instead of silently running on defaults.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const STARTUP_BUDGET: Duration = Duration::from_secs(60);

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
        .env("RUST_LOG", "info")
        .stdin(Stdio::null());
    command
}

/// Every unix socket created anywhere under the hermetic root.
fn sockets_under(root: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt as _;

    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_socket() {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn explicit_toml_config_binds_its_configured_socket() {
    let root = short_temp_dir("sock");
    let socket = root.join("mux.sock");
    let config = root.join("frankenterm.toml");
    std::fs::write(
        &config,
        format!(
            "[[unix_domains]]\nname = \"isolated\"\nsocket_path = \"{}\"\nno_serve_automatically = true\n",
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
    while !socket.exists() {
        if let Some(status) = server.0.try_wait().expect("poll server") {
            panic!(
                "server exited {status} before binding {}: {}",
                socket.display(),
                std::fs::read_to_string(&stderr_path).unwrap_or_default()
            );
        }
        assert!(
            started.elapsed() < STARTUP_BUDGET,
            "server did not bind {} within {STARTUP_BUDGET:?}: {}",
            socket.display(),
            std::fs::read_to_string(&stderr_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        sockets_under(&root),
        vec![socket.clone()],
        "the explicit domain must replace, not add to, the default runtime socket"
    );

    drop(server);
    let log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        log.contains("frankenterm-mux-server-config source=explicit"),
        "config source must be logged: {log}"
    );
    assert!(
        log.contains(&format!("socket={}", socket.display())),
        "bound socket must be logged: {log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn explicit_config_that_does_not_load_exits_one_naming_the_file() {
    let root = short_temp_dir("bad");
    let config = root.join("broken.toml");
    std::fs::write(&config, "[[unix_domains]\nname = \n").expect("write broken config");

    let output = hermetic_command(&root)
        .arg("--config-file")
        .arg(&config)
        .output()
        .expect("run frankenterm-mux-server");

    assert_eq!(
        output.status.code(),
        Some(1),
        "must fail closed: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&config.display().to_string()),
        "the error must name the config file: {stderr}"
    );
    assert_eq!(
        sockets_under(&root),
        Vec::<PathBuf>::new(),
        "a failed explicit config must not fall back to the default socket"
    );
    let _ = std::fs::remove_dir_all(&root);
}
