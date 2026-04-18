//! ft-xbnl0.2.4 — Regression guard: no direct `tokio::net::` or
//! `tokio_rustls::` imports in supported-path FrankenTerm source code.
//!
//! The bead's acceptance criterion 1 is: "Covered TCP, TLS, and HTTP
//! client or service surfaces no longer require direct Tokio-era
//! networking crates." Prior migration work replaced every production
//! TCP surface with `asupersync::net::{TcpStream, TcpListener}` and
//! every production TLS surface with `asupersync::tls::{TlsAcceptor,
//! TlsConnector}`. This test pins that invariant so future code cannot
//! reintroduce `use tokio::net::` (TcpStream/TcpListener) or
//! `use tokio_rustls::` imports in production paths.
//!
//! Mirrors the structure of `wa_3mfv9_no_direct_reqwest_in_supported_paths.rs`
//! (the parallel reqwest guard for acceptance criterion 1's HTTP half).
//!
//! Intentionally NOT flagged:
//!   - `tokio::net::{UnixListener, UnixStream}` — Unix-domain IPC sockets
//!     are still via tokio under the legacy non-asupersync-runtime cfg
//!     branch. Those are gated behind `#[cfg(not(feature =
//!     "asupersync-runtime"))]` and flagging them would also flag the
//!     intentional legacy fallback. This guard targets ONLY the TCP/TLS
//!     surfaces that the bead scopes.
//!   - `use tokio::` imports that do NOT start with `tokio::net::Tcp`
//!     or `tokio_rustls::` — e.g. `tokio::select!`, `tokio::sync::*`,
//!     `tokio::io::*` are part of the broader runtime migration (ft-
//!     xbnl0.2.5) and not in this bead's scope.
//!   - Comments and string literals mentioning these paths by name.
//!   - `frankenterm/char-props/codegen/` and other excluded crates.
//!   - Doc-comment examples inside `//!` or `///` comments.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    let core_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    core_manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("expected frankenterm-core to live under <workspace>/crates/")
}

fn supported_path_roots(root: &Path) -> Vec<PathBuf> {
    vec![root.join("crates"), root.join("frankenterm")]
}

fn is_supported_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if name == "target" {
                return false;
            }
            if name == "codegen" {
                if path
                    .ancestors()
                    .any(|a| a.file_name().and_then(|s| s.to_str()) == Some("char-props"))
                {
                    return false;
                }
            }
        }
    }
    true
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == "target")
            {
                continue;
            }
            collect_rust_files(&path, out);
        } else if is_supported_rust_file(&path) {
            out.push(path);
        }
    }
}

/// Flag lines whose trimmed content starts with:
///   - `use tokio::net::Tcp`
///   - `use tokio_rustls`
///   - `pub use tokio::net::Tcp`
///   - `pub use tokio_rustls`
///   - `extern crate tokio_rustls`
///
/// The `::Tcp` prefix targets `TcpListener` / `TcpStream` re-exports.
/// Unix-domain sockets (`UnixListener`/`UnixStream`) are intentionally
/// permitted under the legacy `#[cfg(not(feature = "asupersync-runtime"))]`
/// branch — see module-level doc.
fn scan_file_for_banned_imports(path: &Path) -> Vec<(usize, String)> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            // Skip doc comments (/// or //!) and line comments (//).
            if trimmed.starts_with("//") {
                return None;
            }
            let is_tokio_tcp_net = trimmed.starts_with("use tokio::net::Tcp")
                || trimmed.starts_with("pub use tokio::net::Tcp");
            let is_tokio_rustls = trimmed.starts_with("use tokio_rustls")
                || trimmed.starts_with("pub use tokio_rustls")
                || trimmed.starts_with("extern crate tokio_rustls");
            if is_tokio_tcp_net || is_tokio_rustls {
                Some((idx + 1, line.to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn scan_toml_for_tokio_rustls_dep(path: &Path) -> Vec<(usize, String)> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("tokio-rustls ")
                || trimmed.starts_with("tokio-rustls=")
                || trimmed.starts_with("tokio-rustls.")
                || trimmed.starts_with("tokio_rustls ")
                || trimmed.starts_with("tokio_rustls=")
            {
                Some((idx + 1, line.to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn collect_workspace_cargo_manifests(root: &Path, out: &mut Vec<PathBuf>) {
    for src_root in supported_path_roots(root) {
        let mut stack = vec![src_root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    if name == "target" {
                        continue;
                    }
                    if name == "codegen"
                        && path
                            .ancestors()
                            .any(|a| a.file_name().and_then(|s| s.to_str()) == Some("char-props"))
                    {
                        continue;
                    }
                    stack.push(path);
                } else if path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
                    out.push(path);
                }
            }
        }
    }
}

#[test]
fn ft_xbnl0_2_4_no_direct_tokio_tcp_or_rustls_imports() {
    let root = workspace_root();
    let mut rust_files = Vec::new();
    for src_root in supported_path_roots(&root) {
        if src_root.exists() {
            collect_rust_files(&src_root, &mut rust_files);
        }
    }
    assert!(
        !rust_files.is_empty(),
        "scan must find Rust source under {} or {}",
        root.join("crates").display(),
        root.join("frankenterm").display()
    );

    let mut violations: Vec<(PathBuf, Vec<(usize, String)>)> = Vec::new();
    for path in &rust_files {
        let hits = scan_file_for_banned_imports(path);
        if !hits.is_empty() {
            violations.push((path.clone(), hits));
        }
    }

    assert!(
        violations.is_empty(),
        "ft-xbnl0.2.4 regression: {} file(s) reintroduced direct tokio TCP \
         or tokio_rustls usage. Replace with asupersync::net::{{TcpStream, \
         TcpListener}} or asupersync::tls::{{TlsAcceptor, TlsConnector}} \
         before re-running:\n{}",
        violations.len(),
        violations
            .iter()
            .map(|(path, hits)| {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                hits.iter()
                    .map(|(line, text)| format!("  {}:{} — {}", rel.display(), line, text.trim()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn ft_xbnl0_2_4_no_tokio_rustls_in_workspace_manifests() {
    let root = workspace_root();
    let mut manifests = Vec::new();
    collect_workspace_cargo_manifests(&root, &mut manifests);

    let mut violations: Vec<(PathBuf, Vec<(usize, String)>)> = Vec::new();
    for path in &manifests {
        let hits = scan_toml_for_tokio_rustls_dep(path);
        if !hits.is_empty() {
            violations.push((path.clone(), hits));
        }
    }

    assert!(
        violations.is_empty(),
        "ft-xbnl0.2.4 regression: {} workspace Cargo.toml file(s) declare \
         a tokio-rustls dependency. Remove the declaration or replace with \
         asupersync::tls before re-running:\n{}",
        violations.len(),
        violations
            .iter()
            .map(|(path, hits)| {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                hits.iter()
                    .map(|(line, text)| format!("  {}:{} — {}", rel.display(), line, text.trim()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
}
