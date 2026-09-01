//! Canonical naming for the mux sockets a running FrankenTerm GUI publishes.
//!
//! Three parties must agree on these paths: the GUI (publisher), the vendored
//! client library (`frankenterm-client::discovery`), and `frankenterm-core`
//! (which discovers a live mux for the `ft` CLI and watcher). Keeping the
//! naming here, in the one crate all three already depend on, is what lets
//! `ft` find the app without an external `wezterm` binary.
//!
//! Layout inside `RUNTIME_DIR` (`~/.local/share/frankenterm` on macOS,
//! `$XDG_RUNTIME_DIR/frankenterm` elsewhere):
//!
//! - `frankenterm-gui-sock-<pid>`: one listening socket per GUI process.
//! - `default-<class>` (macOS) / `wayland-<display>-<class>` /
//!   `x11-<display>-<class>` (other unix): a symlink to the socket of the
//!   most recently published GUI instance for that window class.
//!
//! Liveness is deliberately *not* decided here: each consumer probes the
//! socket itself, because the GUI's quarantine policy and the CLI's
//! read-only discovery have different side-effect budgets.

use std::path::{Path, PathBuf};

/// Filename prefix for per-process FrankenTerm GUI mux sockets.
pub const GUI_SOCKET_PREFIX: &str = "frankenterm-gui-sock-";

/// Window class the GUI publishes under unless overridden with `--class`.
/// Doubles as the macOS bundle identifier.
pub const DEFAULT_WINDOW_CLASS: &str = "com.dicklesworthstone.frankenterm";

/// Runtime path of the mux socket owned by the GUI process `pid`.
#[must_use]
pub fn gui_socket_path_for_pid(pid: u32) -> PathBuf {
    gui_socket_path_for_pid_in(crate::RUNTIME_DIR.as_path(), pid)
}

/// [`gui_socket_path_for_pid`] against an explicit runtime directory.
#[must_use]
pub fn gui_socket_path_for_pid_in(runtime_dir: &Path, pid: u32) -> PathBuf {
    runtime_dir.join(format!("{GUI_SOCKET_PREFIX}{pid}"))
}

/// Parse the owning pid out of a canonical `frankenterm-gui-sock-<pid>` name.
///
/// Rejects empty, zero, zero-padded, non-numeric, and out-of-range pids so a
/// hand-crafted directory entry can never be mistaken for a GUI socket.
#[must_use]
pub fn parse_gui_socket_pid(name: &str) -> Option<u32> {
    let pid = name.strip_prefix(GUI_SOCKET_PREFIX)?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let parsed = pid.parse::<u32>().ok()?;
    (parsed != 0 && parsed.to_string() == pid).then_some(parsed)
}

/// Filename of the "current instance" symlink for `class_name`.
///
/// On macOS there is one display, so the name is `default-<class>`. On other
/// unix platforms the display identity is part of the name so a Wayland and
/// an X11 session on the same host do not clobber each other's link.
#[must_use]
pub fn published_gui_sock_name(class_name: &str) -> String {
    #[cfg(not(target_os = "macos"))]
    {
        let config = crate::configuration();
        if config.enable_wayland {
            if let Ok(wayland) = std::env::var("WAYLAND_DISPLAY") {
                return format!("wayland-{wayland}-{class_name}");
            }
            // No default WAYLAND_DISPLAY is assumed: we cannot tell here
            // whether the session would fall back to X11.
        }
        let x11 = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        format!("x11-{x11}-{class_name}")
    }
    #[cfg(target_os = "macos")]
    {
        format!("default-{class_name}")
    }
}

/// Runtime path of the "current instance" symlink for `class_name`.
#[must_use]
pub fn published_gui_sock_path(class_name: &str) -> PathBuf {
    published_gui_sock_path_in(crate::RUNTIME_DIR.as_path(), class_name)
}

/// [`published_gui_sock_path`] against an explicit runtime directory.
#[must_use]
pub fn published_gui_sock_path_in(runtime_dir: &Path, class_name: &str) -> PathBuf {
    runtime_dir.join(published_gui_sock_name(class_name))
}

/// Read the socket path the GUI last published for `class_name`.
///
/// This only follows the symlink; it says nothing about whether the target
/// still exists or accepts connections.
pub fn resolve_published_gui_sock(class_name: &str) -> std::io::Result<PathBuf> {
    resolve_published_gui_sock_in(crate::RUNTIME_DIR.as_path(), class_name)
}

/// [`resolve_published_gui_sock`] against an explicit runtime directory.
pub fn resolve_published_gui_sock_in(
    runtime_dir: &Path,
    class_name: &str,
) -> std::io::Result<PathBuf> {
    std::fs::read_link(published_gui_sock_path_in(runtime_dir, class_name))
}

/// Enumerate `frankenterm-gui-sock-<pid>` entries in `runtime_dir`.
///
/// Returns `(pid, path)` pairs for entries whose name parses and whose file
/// type is a socket (on unix). No liveness probe is performed; callers decide
/// how to treat sockets whose owner has exited.
#[must_use]
pub fn list_gui_socket_entries(runtime_dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(dir) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in dir.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(pid) = parse_gui_socket_pid(&name) else {
            continue;
        };
        if !entry_is_socket(&entry) {
            continue;
        }
        entries.push((pid, entry.path()));
    }
    entries.sort_by_key(|(pid, _)| *pid);
    entries
}

#[cfg(unix)]
fn entry_is_socket(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::FileTypeExt;

    entry
        .file_type()
        .is_ok_and(|file_type| file_type.is_socket())
}

#[cfg(not(unix))]
fn entry_is_socket(_entry: &std::fs::DirEntry) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;

    #[test]
    fn socket_path_uses_canonical_prefix() {
        let path = gui_socket_path_for_pid_in(Path::new("/rt"), 42);
        assert_eq!(path, PathBuf::from("/rt/frankenterm-gui-sock-42"));
    }

    #[test]
    fn parse_requires_canonical_numeric_pid() {
        assert_eq!(parse_gui_socket_pid("frankenterm-gui-sock-42"), Some(42));
        assert_eq!(parse_gui_socket_pid("gui-sock-42"), None);
        assert_eq!(parse_gui_socket_pid("frankenterm-gui-sock-"), None);
        assert_eq!(parse_gui_socket_pid("frankenterm-gui-sock-not-a-pid"), None);
        assert_eq!(parse_gui_socket_pid("frankenterm-gui-sock-0"), None);
        assert_eq!(parse_gui_socket_pid("frankenterm-gui-sock-00042"), None);
        assert_eq!(
            parse_gui_socket_pid("frankenterm-gui-sock-4294967296"),
            None
        );
        assert_eq!(parse_gui_socket_pid("frankenterm-gui-sock-42.lock"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn published_name_on_macos_is_default_class() {
        assert_eq!(
            published_gui_sock_name(DEFAULT_WINDOW_CLASS),
            "default-com.dicklesworthstone.frankenterm"
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_entries_returns_only_sockets_with_canonical_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let live = gui_socket_path_for_pid_in(dir.path(), 4242);
        let _listener = UnixListener::bind(&live).expect("bind");
        std::fs::write(dir.path().join("frankenterm-gui-sock-7"), b"not a socket").unwrap();
        std::fs::write(dir.path().join("frankenterm-gui-sock-4242.lock"), b"").unwrap();
        let other = dir.path().join("sock");
        let _other = UnixListener::bind(&other).expect("bind");

        let entries = list_gui_socket_entries(dir.path());
        assert_eq!(entries, vec![(4242, live)]);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_published_follows_symlink_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = gui_socket_path_for_pid_in(dir.path(), 99);
        symlink(&target, published_gui_sock_path_in(dir.path(), "x.y.z")).expect("symlink");
        // The target does not exist; resolution still reports it, because
        // liveness is the caller's decision.
        assert_eq!(
            resolve_published_gui_sock_in(dir.path(), "x.y.z").unwrap(),
            target
        );
        assert!(resolve_published_gui_sock_in(dir.path(), "missing").is_err());
    }
}
