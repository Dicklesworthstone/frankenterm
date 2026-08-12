use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::runtime_async_surface_guard::{
    allowed_raw_runtime_files, allowed_raw_tokio_runtime_builder_apis,
};

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

fn imports_runtime_async_item(contents: &str, item: &str) -> bool {
    let compact: String = contents
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.contains(&format!("usecrate::runtime_async::{item};")) {
        return true;
    }

    let group_prefix = "usecrate::runtime_async::{";
    compact
        .split(group_prefix)
        .skip(1)
        .filter_map(|suffix| suffix.split_once("};").map(|(items, _)| items))
        .any(|items| items.split(',').any(|candidate| candidate == item))
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
fn tokio_async_runtime_primitives_stay_confined_to_runtime_async_module() {
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
            "tokio::sync::broadcast",
            "tokio::sync::oneshot",
            "tokio::sync::Notify",
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
fn runtime_async_helper_shims_do_not_reappear_in_production_surfaces() {
    let workspace_root = workspace_root();
    let files = production_surface_files(&workspace_root);
    let allowed =
        BTreeSet::from([workspace_root.join("crates/frankenterm-core/src/runtime_async.rs")]);
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
        "runtime_async helper shims must not be reintroduced into production call sites:\n{}",
        violations.join("\n")
    );
}

#[test]
fn raw_tokio_runtime_builder_quarantine_stays_empty() {
    let workspace_root = workspace_root();
    let runtime_async =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/runtime_async.rs"))
            .expect("failed to read runtime_async.rs");

    let allowed_apis = allowed_raw_tokio_runtime_builder_apis();
    assert!(
        allowed_apis.is_empty(),
        "the raw tokio runtime-builder quarantine must remain empty after the asupersync-only cutover"
    );

    assert_eq!(
        runtime_async.matches("tokio::runtime::Builder::").count(),
        0,
        "runtime_async.rs must not reintroduce raw tokio runtime builder constructors"
    );
}

#[test]
fn web_and_cli_async_surfaces_route_through_runtime_async() {
    let workspace_root = workspace_root();
    let runtime_async =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/runtime_async.rs"))
            .expect("failed to read runtime_async.rs");
    let web_server =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/web/server.rs"))
            .expect("failed to read web/server.rs");
    let web_sse = fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/web/sse.rs"))
        .expect("failed to read web/sse.rs");
    let main = fs::read_to_string(workspace_root.join("crates/frankenterm/src/main.rs"))
        .expect("failed to read main.rs");

    assert!(
        runtime_async.contains("macro_rules! select")
            && runtime_async.contains("pub use crate::select;"),
        "runtime_async.rs must expose the project-owned, non-tokio select macro"
    );
    assert!(
        !runtime_async.contains("pub use tokio::select;"),
        "runtime_async.rs must not regress to the retired tokio select re-export"
    );
    assert!(
        runtime_async.contains("pub mod broadcast"),
        "runtime_async.rs must expose the project-owned broadcast surface"
    );
    assert!(
        runtime_async.contains("pub mod oneshot"),
        "runtime_async.rs must expose the project-owned oneshot surface"
    );
    assert!(
        runtime_async.contains("pub mod notify"),
        "runtime_async.rs must expose the project-owned notify surface"
    );
    assert!(
        runtime_async.contains("pub mod signal"),
        "runtime_async.rs must expose the project-owned signal surface"
    );
    assert!(
        runtime_async.contains("pub mod process"),
        "runtime_async.rs must expose the project-owned process surface"
    );
    assert!(
        imports_runtime_async_item(&web_server, "signal")
            && web_server.contains("crate::runtime_async::select!"),
        "web/server.rs must route server lifecycle selection and signals through runtime_async"
    );
    assert!(
        ["mpsc", "select", "sleep", "task"]
            .iter()
            .all(|item| imports_runtime_async_item(&web_sse, item)),
        "web/sse.rs must import the runtime_async stream primitives it uses"
    );
    assert!(
        !web_server.contains("tokio::select!") && !web_server.contains("tokio::signal::"),
        "web/server.rs must not bypass runtime_async for select/signal operations"
    );
    assert!(
        !web_sse.contains("tokio::select!") && !web_sse.contains("tokio::signal::"),
        "web/sse.rs must not bypass runtime_async for select/signal operations"
    );
    assert!(
        main.contains("frankenterm_core::runtime_async::select!"),
        "main.rs must use runtime_async::select! at CLI/runtime coordination sites"
    );
    assert!(
        main.contains("frankenterm_core::runtime_async::signal::ctrl_c()"),
        "main.rs must use runtime_async signal handling"
    );
    assert!(
        !main.contains("tokio::select!") && !main.contains("tokio::signal::"),
        "main.rs must not bypass runtime_async for select/signal operations"
    );
}

#[test]
fn distributed_http_cx_surfaces_route_through_crate_cx() {
    let workspace_root = workspace_root();
    let distributed =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/distributed.rs"))
            .expect("failed to read distributed.rs");

    assert!(
        distributed.contains("use crate::cx::Cx;"),
        "distributed.rs must import the project-owned Cx alias"
    );
    assert!(
        !distributed.contains("&asupersync::cx::Cx"),
        "distributed.rs production HTTP APIs must accept crate::cx::Cx, not asupersync::cx::Cx"
    );
}

#[test]
fn production_channel_surfaces_route_through_runtime_async() {
    let workspace_root = workspace_root();
    let events = fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/events.rs"))
        .expect("failed to read events.rs");
    let storage = fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/storage.rs"))
        .expect("failed to read storage.rs");
    let search_bridge =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/search_bridge.rs"))
            .expect("failed to read search_bridge.rs");
    let cancellation_safe_channel = fs::read_to_string(
        workspace_root.join("crates/frankenterm-core/src/cancellation_safe_channel.rs"),
    )
    .expect("failed to read cancellation_safe_channel.rs");
    let spsc_ring_buffer =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/src/spsc_ring_buffer.rs"))
            .expect("failed to read spsc_ring_buffer.rs");

    assert!(
        events.contains("use crate::runtime_async::broadcast;"),
        "events.rs must route broadcast fan-out through runtime_async"
    );
    assert!(
        imports_runtime_async_item(&storage, "oneshot"),
        "storage.rs must route request/response oneshot channels through runtime_async"
    );
    for (path, contents) in [
        ("search_bridge.rs", &search_bridge),
        ("cancellation_safe_channel.rs", &cancellation_safe_channel),
        ("spsc_ring_buffer.rs", &spsc_ring_buffer),
    ] {
        assert!(
            imports_runtime_async_item(contents, "notify::Notify"),
            "{path} must route async notifications through runtime_async::notify"
        );
    }

    for (path, contents) in [
        ("events.rs", &events),
        ("storage.rs", &storage),
        ("search_bridge.rs", &search_bridge),
        ("cancellation_safe_channel.rs", &cancellation_safe_channel),
        ("spsc_ring_buffer.rs", &spsc_ring_buffer),
    ] {
        assert!(
            !contents.contains("tokio::sync::broadcast")
                && !contents.contains("tokio::sync::oneshot")
                && !contents.contains("tokio::sync::Notify"),
            "{path} must not bypass runtime_async for channel/notify primitives"
        );
    }
}
