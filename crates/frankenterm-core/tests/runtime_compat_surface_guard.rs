use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::runtime_compat_surface_guard::allowed_raw_runtime_files;

fn collect_rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).unwrap_or_else(|err| {
            panic!("failed to read directory {}: {err}", dir.display());
        });
        for entry in entries {
            let entry = entry.unwrap_or_else(|err| {
                panic!(
                    "failed to read directory entry under {}: {err}",
                    dir.display()
                );
            });
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn is_comment_only(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("*/")
}

fn scan_for_patterns(
    workspace_root: &Path,
    files: &[PathBuf],
    allowed: &BTreeSet<PathBuf>,
    patterns: &[&str],
) -> Vec<String> {
    let mut violations = Vec::new();
    for file in files {
        if allowed.contains(file) {
            continue;
        }

        let content = fs::read_to_string(file).unwrap_or_else(|err| {
            panic!("failed to read source file {}: {err}", file.display());
        });
        for (line_index, line) in content.lines().enumerate() {
            if is_comment_only(line) {
                continue;
            }
            if patterns.iter().any(|pattern| line.contains(pattern)) {
                let rel_path = file
                    .strip_prefix(workspace_root)
                    .unwrap_or(file)
                    .display()
                    .to_string();
                violations.push(format!("{}:{}: {}", rel_path, line_index + 1, line.trim()));
            }
        }
    }
    violations
}

fn workspace_root() -> PathBuf {
    let core_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    core_root
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("expected core crate to live under <workspace>/crates/"))
        .to_path_buf()
}

fn production_surface_files(workspace_root: &Path) -> Vec<PathBuf> {
    let mut files = collect_rust_files(&workspace_root.join("crates/frankenterm-core/src"));
    files.extend(collect_rust_files(
        &workspace_root.join("crates/frankenterm/src"),
    ));
    files
}

fn allowed_core_runtime_files(workspace_root: &Path) -> BTreeSet<PathBuf> {
    allowed_raw_runtime_files()
        .into_iter()
        .map(|file_name| {
            workspace_root
                .join("crates/frankenterm-core/src")
                .join(file_name)
        })
        .collect()
}

#[test]
fn tokio_async_runtime_primitives_stay_confined_to_runtime_compat_module() {
    let workspace_root = workspace_root();
    let files = production_surface_files(&workspace_root);
    let allowed = allowed_core_runtime_files(&workspace_root);
    let violations = scan_for_patterns(
        &workspace_root,
        &files,
        &allowed,
        &[
            "tokio::select!",
            "tokio::process::",
            "tokio::runtime::Builder",
            "tokio::signal::",
            "tokio::time::sleep",
            "tokio::time::timeout",
            "tokio::sync::mpsc",
            "tokio::sync::watch",
        ],
    );

    assert!(
        violations.is_empty(),
        "direct tokio async runtime primitives must stay confined to the allowed runtime surface files:\n{}",
        violations.join("\n")
    );
}

#[test]
fn runtime_compat_helper_shims_do_not_reappear_in_production_surfaces() {
    let workspace_root = workspace_root();
    let files = production_surface_files(&workspace_root);
    let allowed =
        BTreeSet::from([workspace_root.join("crates/frankenterm-core/src/runtime_compat.rs")]);
    let violations = scan_for_patterns(
        &workspace_root,
        &files,
        &allowed,
        &[
            "mpsc_send(",
            "mpsc_recv_option(",
            "watch_has_changed(",
            "watch_borrow_and_update_clone(",
            "watch_changed(",
        ],
    );

    assert!(
        violations.is_empty(),
        "runtime_compat helper shims must not be reintroduced into production call sites:\n{}",
        violations.join("\n")
    );
}

#[test]
fn web_and_cli_async_surfaces_route_through_runtime_compat() {
    let workspace_root = workspace_root();
    let runtime_compat =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/runtime_compat.rs"))
            .expect("failed to read runtime_compat.rs");
    let web = fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/web.rs"))
        .expect("failed to read web.rs");
    let main = fs::read_to_string(workspace_root.join("crates/frankenterm/src/main.rs"))
        .expect("failed to read main.rs");

    assert!(
        runtime_compat.contains("pub use tokio::select;"),
        "runtime_compat.rs must continue to expose a select bridge while this migration contract is active"
    );
    assert!(
        runtime_compat.contains("pub mod signal"),
        "runtime_compat.rs must continue to expose a signal bridge while this migration contract is active"
    );
    assert!(
        runtime_compat.contains("pub mod process"),
        "runtime_compat.rs must continue to expose a process bridge while this migration contract is active"
    );
    assert!(
        runtime_compat.contains("pub fn start_paused"),
        "runtime_compat.rs must continue to expose a paused-test runtime builder bridge while this migration contract is active"
    );
    assert!(
        web.contains("use crate::runtime_compat::{mpsc, select, signal, sleep, task, timeout};"),
        "web.rs must import runtime_compat bridges for async runtime operations"
    );
    assert!(
        !web.contains("tokio::select!") && !web.contains("tokio::signal::"),
        "web.rs must not bypass runtime_compat for select/signal operations"
    );
    assert!(
        main.contains("frankenterm_core::runtime_compat::select!"),
        "main.rs must use runtime_compat::select! at CLI/runtime coordination sites"
    );
    assert!(
        main.contains("frankenterm_core::runtime_compat::signal::ctrl_c()"),
        "main.rs must use runtime_compat signal handling"
    );
    assert!(
        !main.contains("tokio::select!") && !main.contains("tokio::signal::"),
        "main.rs must not bypass runtime_compat for select/signal operations"
    );
}
