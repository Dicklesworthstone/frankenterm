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

/// Maximum length of a unix domain socket path (`sun_path`) in bytes, excluding
/// the trailing NUL terminator.
///
/// On Darwin/macOS and BSDs, `sizeof(sockaddr_un.sun_path)` is 104 bytes, leaving
/// 103 usable bytes. On Linux and other platforms, it is 108 bytes (107 usable).
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub const MAX_SUN_PATH_LEN: usize = 103;

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
pub const MAX_SUN_PATH_LEN: usize = 107;

/// Extract raw byte representation of a path without lossy UTF-8 conversion,
/// normalizing trailing slashes.
#[must_use]
fn path_raw_bytes(p: &Path) -> &[u8] {
    let mut b = p.as_os_str().as_encoded_bytes();
    while b.len() > 1 && b.ends_with(b"/") {
        b = &b[..b.len() - 1];
    }
    b
}

/// Compute a deterministic 64-bit FNV-1a hash over raw path bytes.
#[must_use]
fn stable_path_hash(data: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Check whether `dir` can host the worst-case GUI socket path within `MAX_SUN_PATH_LEN`.
///
/// Joins `dir` with the longest possible GUI socket filename (`frankenterm-gui-sock-4294967295`)
/// and validates the complete resulting path against the platform's `SUN_LEN` limit.
#[must_use]
pub fn can_hold_gui_socket(dir: &Path) -> bool {
    let max_socket_name = format!("{GUI_SOCKET_PREFIX}{}", u32::MAX);
    let max_socket_path = dir.join(max_socket_name);
    max_socket_path.as_os_str().len() <= MAX_SUN_PATH_LEN
}

#[cfg(unix)]
fn is_secure_sticky_dir(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(parent_meta) = parent.symlink_metadata() else {
        return false;
    };
    if !parent_meta.is_dir()
        || parent_meta.uid() != 0
        || parent_meta.permissions().mode() & 0o022 != 0
    {
        return false;
    }
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    meta.is_dir() && meta.uid() == 0 && meta.permissions().mode() & 0o1000 != 0
}

fn bounded_gui_socket_dir_for(runtime_dir: &Path) -> Option<PathBuf> {
    let raw_bytes = path_raw_bytes(runtime_dir);
    let hash = stable_path_hash(raw_bytes);
    let hex = format!("{hash:016x}");

    // Use one fixed OS base so login state and differing TMPDIR/XDG settings
    // cannot make the publisher and discoverer choose different directories.
    // The listener creates the child with verified current-user ownership,
    // mode 0700 and O_NOFOLLOW through create_user_owned_dirs.
    #[cfg(unix)]
    {
        let uid = nix::unistd::geteuid().as_raw();
        #[cfg(target_os = "macos")]
        let tmp_base = Path::new("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let tmp_base = Path::new("/tmp");
        if is_secure_sticky_dir(tmp_base) {
            let candidate = tmp_base.join(format!("ft-u{uid}-{hex}"));
            if can_hold_gui_socket(&candidate) {
                return Some(candidate);
            }
        }
    }

    #[cfg(not(unix))]
    let _ = hex;
    None
}

/// Resolve the canonical directory to store GUI control sockets and published symlinks.
///
/// If `runtime_dir` is short enough that any valid GUI socket name
/// fits within `SUN_LEN` (`can_hold_gui_socket(runtime_dir)`), `runtime_dir` is
/// returned directly. Standard installed `$HOME` directories are completely unaffected.
///
/// If `runtime_dir` exceeds `SUN_LEN`, this attempts to resolve a bounded, per-user
/// directory on a validated secure OS base isolated by the 64-bit hash of raw
/// `runtime_dir` bytes. If no secure bounded base is available, it returns
/// `runtime_dir` without inventing an unchecked, unsafe fallback.
///
/// Idempotent: `gui_socket_dir_for(&gui_socket_dir_for(p)) == gui_socket_dir_for(p)`.
#[must_use]
pub fn gui_socket_dir_for(runtime_dir: &Path) -> PathBuf {
    if can_hold_gui_socket(runtime_dir) {
        return runtime_dir.to_path_buf();
    }
    bounded_gui_socket_dir_for(runtime_dir).unwrap_or_else(|| runtime_dir.to_path_buf())
}

/// Discovery cannot trust a directory another user planted in the shared OS
/// base. Unlike listener creation, this check never changes filesystem state.
fn private_gui_socket_dir_is_safe(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        dir.symlink_metadata().is_ok_and(|meta| {
            meta.is_dir()
                && meta.uid() == nix::unistd::geteuid().as_raw()
                && meta.permissions().mode() & 0o7777 == 0o700
        })
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

/// Runtime path of the mux socket owned by the GUI process `pid`.
#[must_use]
pub fn gui_socket_path_for_pid(pid: u32) -> PathBuf {
    gui_socket_path_for_pid_in(crate::RUNTIME_DIR.as_path(), pid)
}

/// [`gui_socket_path_for_pid`] against an explicit runtime directory.
#[must_use]
pub fn gui_socket_path_for_pid_in(runtime_dir: &Path, pid: u32) -> PathBuf {
    let dir = gui_socket_dir_for(runtime_dir);
    dir.join(format!("{GUI_SOCKET_PREFIX}{pid}"))
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
    let dir = gui_socket_dir_for(runtime_dir);
    dir.join(published_gui_sock_name(class_name))
}

/// Read the socket path the GUI last published for `class_name`.
///
/// Resolves through canonical absolute path resolution.
pub fn resolve_published_gui_sock(class_name: &str) -> std::io::Result<PathBuf> {
    resolve_published_gui_sock_in(crate::RUNTIME_DIR.as_path(), class_name)
}

/// Read and resolve a symlink to an absolute `PathBuf` anchored at the symlink file's actual parent directory.
fn read_link_anchored_absolute(link_path: &Path) -> std::io::Result<PathBuf> {
    let target = std::fs::read_link(link_path)?;
    if target.is_absolute() {
        Ok(target)
    } else {
        let parent = link_path.parent().unwrap_or_else(|| Path::new("."));
        Ok(parent.join(target))
    }
}

/// [`resolve_published_gui_sock`] against an explicit runtime directory.
///
/// Follows symlinks and resolves relative targets against the symlink's actual parent directory.
pub fn resolve_published_gui_sock_in(
    runtime_dir: &Path,
    class_name: &str,
) -> std::io::Result<PathBuf> {
    let resolved_dir = gui_socket_dir_for(runtime_dir);
    let primary = resolved_dir.join(published_gui_sock_name(class_name));
    let resolved = if resolved_dir != runtime_dir && !private_gui_socket_dir_is_safe(&resolved_dir)
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "bounded GUI socket directory is not private and user-owned",
        ))
    } else {
        read_link_anchored_absolute(&primary)
    };
    match resolved {
        Ok(target) => Ok(target),
        Err(err) if resolved_dir != runtime_dir => {
            let fallback = runtime_dir.join(published_gui_sock_name(class_name));
            read_link_anchored_absolute(&fallback).map_err(|_| err)
        }
        Err(err) => Err(err),
    }
}

/// Enumerate `frankenterm-gui-sock-<pid>` entries in `runtime_dir`.
///
/// Returns `(pid, path)` pairs for entries whose name parses and whose file
/// type is a socket (on unix). No liveness probe is performed; callers decide
/// how to treat sockets whose owner has exited.
#[must_use]
pub fn list_gui_socket_entries(runtime_dir: &Path) -> Vec<(u32, PathBuf)> {
    let resolved_dir = gui_socket_dir_for(runtime_dir);
    let mut entries =
        if resolved_dir == runtime_dir || private_gui_socket_dir_is_safe(&resolved_dir) {
            list_gui_socket_entries_direct(&resolved_dir)
        } else {
            Vec::new()
        };
    if resolved_dir != runtime_dir {
        for entry in list_gui_socket_entries_direct(runtime_dir) {
            if !entries.iter().any(|(pid, _)| *pid == entry.0) {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|(pid, _)| *pid);
    }
    entries
}

fn list_gui_socket_entries_direct(dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
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

    #[test]
    fn stable_path_hash_is_deterministic_and_distinct() {
        let h1 = stable_path_hash(b"/path/to/alpha");
        let h2 = stable_path_hash(b"/path/to/alpha");
        let h3 = stable_path_hash(b"/path/to/beta");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[cfg(unix)]
    #[test]
    fn path_raw_bytes_preserves_non_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let raw = b"/tmp/non_utf8_\xff\xfe_path//";
        let path = Path::new(OsStr::from_bytes(raw));
        let normalized = path_raw_bytes(path);
        assert_eq!(normalized, b"/tmp/non_utf8_\xff\xfe_path");
    }

    #[test]
    fn short_runtime_dir_is_unaffected() {
        let short = Path::new("/short/path");
        assert!(can_hold_gui_socket(short));
        assert_eq!(gui_socket_dir_for(short), short);
    }

    #[cfg(unix)]
    #[test]
    fn long_runtime_dir_diverts_and_is_idempotent() {
        let fixture = tempfile::tempdir().expect("temp dir");
        let long_parent = fixture
            .path()
            .join("a".repeat(80))
            .join(".local/share/frankenterm");
        assert!(!can_hold_gui_socket(&long_parent));

        let bounded = gui_socket_dir_for(&long_parent);
        assert_ne!(bounded, long_parent);
        assert!(can_hold_gui_socket(&bounded));

        // Verify idempotence: calling gui_socket_dir_for on bounded must return bounded without re-hashing
        let second = gui_socket_dir_for(&bounded);
        assert_eq!(bounded, second);

        let sock_path = gui_socket_path_for_pid_in(&long_parent, 4294967295);
        assert!(
            sock_path.as_os_str().len() <= MAX_SUN_PATH_LEN,
            "bounded socket path {} has len {}, exceeding MAX_SUN_PATH_LEN {}",
            sock_path.display(),
            sock_path.as_os_str().len(),
            MAX_SUN_PATH_LEN
        );
    }

    #[test]
    fn distinct_long_runtime_dirs_produce_distinct_bounded_dirs() {
        let fixture = tempfile::tempdir().expect("temp dir");
        let long_a = fixture.path().join("root_alpha").join("a".repeat(80));
        let long_b = fixture.path().join("root_beta").join("a".repeat(80));
        let bounded_a = gui_socket_dir_for(&long_a);
        let bounded_b = gui_socket_dir_for(&long_b);
        assert_ne!(bounded_a, bounded_b);
    }

    #[test]
    fn trailing_slash_normalization_yields_same_bounded_dir() {
        let fixture = tempfile::tempdir().expect("temp dir");
        let long = fixture.path().join("root_gamma").join("a".repeat(80));
        let mut long_slash = long.clone().into_os_string();
        long_slash.push("/");
        let long_with = PathBuf::from(long_slash);

        assert_eq!(gui_socket_dir_for(&long), gui_socket_dir_for(&long_with));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_dir_bind_publish_and_resolve_lifecycle() {
        let fixture = tempfile::tempdir().expect("temp dir");
        let long = fixture
            .path()
            .join("a".repeat(80))
            .join(".local/share/frankenterm");

        let sock_path = gui_socket_path_for_pid_in(&long, 98228);
        assert!(
            sock_path.as_os_str().len() <= MAX_SUN_PATH_LEN,
            "socket path must be shorter than SUN_LEN"
        );

        let parent = sock_path.parent().expect("socket has parent directory");
        crate::create_user_owned_dirs(parent).expect("create bounded directory");

        let listener = UnixListener::bind(&sock_path).expect("bind must succeed under bounded dir");

        let class = "com.test.lifecycle";
        let pub_path = published_gui_sock_path_in(&long, class);
        symlink(&sock_path, &pub_path).expect("publish symlink");

        let resolved = resolve_published_gui_sock_in(&long, class).expect("resolve symlink");
        assert_eq!(resolved, sock_path);

        let entries = list_gui_socket_entries(&long);
        assert!(
            entries
                .iter()
                .any(|(pid, p)| *pid == 98228 && *p == sock_path),
            "entries must contain the bounded live socket: {entries:?}"
        );

        drop(listener);
        std::fs::remove_file(&sock_path).ok();
        std::fs::remove_file(&pub_path).ok();
        std::fs::remove_dir(parent).ok();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_discovery_rejects_symlink_and_nonprivate_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = tempfile::tempdir().expect("isolated fixture");
        let long = fixture.path().join("a".repeat(100));
        let bounded = gui_socket_dir_for(&long);
        assert_ne!(bounded, long);
        let target = fixture.path().join("target");
        crate::create_user_owned_dirs(&target).unwrap();
        symlink(&target, &bounded).unwrap();
        assert!(!private_gui_socket_dir_is_safe(&bounded));
        assert!(list_gui_socket_entries(&long).is_empty());
        assert_eq!(
            resolve_published_gui_sock_in(&long, "isolated.test")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        // Remove only the symlink created by this isolated fixture, then test
        // an actual directory at the same canonical name.
        std::fs::remove_file(&bounded).unwrap();
        crate::create_user_owned_dirs(&bounded).unwrap();
        assert!(private_gui_socket_dir_is_safe(&bounded));
        std::fs::set_permissions(&bounded, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(!private_gui_socket_dir_is_safe(&bounded));
        assert!(resolve_published_gui_sock_in(&long, "isolated.test").is_err());
        std::fs::remove_dir(&bounded).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_published_relative_symlink_anchors_at_actual_parent() {
        let fixture = tempfile::tempdir_in("/tmp").expect("short isolated fixture");
        // A two-digit PID still fits here, but the conservative ten-digit
        // bound relocates new publishers. Discover an older raw-path link
        // relative to its actual parent when the bounded directory is absent.
        let raw_dir_len = MAX_SUN_PATH_LEN - 1 - GUI_SOCKET_PREFIX.len() - 2;
        let component_len = raw_dir_len - fixture.path().as_os_str().len() - 1;
        let raw_rt = fixture.path().join("r".repeat(component_len));
        assert!(!can_hold_gui_socket(&raw_rt));
        std::fs::create_dir_all(&raw_rt).expect("create raw_rt");

        let target_sock = raw_rt.join("frankenterm-gui-sock-77");
        let listener = UnixListener::bind(&target_sock).expect("bind");

        let class = "relative.test";
        let link_path = raw_rt.join(published_gui_sock_name(class));
        symlink(Path::new("frankenterm-gui-sock-77"), &link_path).expect("create relative symlink");

        let resolved = resolve_published_gui_sock_in(&raw_rt, class).expect("resolve");
        assert_eq!(resolved, target_sock);

        drop(listener);
        std::fs::remove_file(&link_path).ok();
        std::fs::remove_file(&target_sock).ok();
    }
}
