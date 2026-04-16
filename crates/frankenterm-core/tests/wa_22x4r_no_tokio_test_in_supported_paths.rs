//! wa-22x4r — Regression guard: no active `#[tokio::test]` attributes in
//! supported-path code.
//!
//! The bead's core acceptance criterion is "No #[tokio::test] remaining in
//! codebase." This test walks the supported-path source trees
//! (`crates/` and `frankenterm/`) and asserts that no Rust file contains a
//! line whose trimmed content begins with `#[tokio::test`.
//!
//! This test intentionally does NOT flag:
//!   - references to the string inside doc comments or string literals
//!     (they legitimately appear in eradication governance modules such as
//!     `crates/frankenterm-core/src/dependency_eradication.rs` and in the
//!     port-source comments at the top of `tests/*_labruntime.rs` files);
//!   - `legacy_zellij/` which is not a workspace member and retains
//!     `#[tokio::test]` usages as historical reference code;
//!   - `target/` and `.beads/` bookkeeping directories.
//!
//! The check is that active attribute use — the only thing that would
//! regress the port — is gone. If this test ever fails, the failure
//! message points at the exact file:line pair that reintroduced
//! `#[tokio::test]`.

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

/// A file is considered part of the "supported-path" audit surface iff it
/// has a `.rs` extension and none of its ancestors is `target/`.
fn is_supported_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if name == "target" {
                return false;
            }
        }
    }
    true
}

/// Recursively collect every `.rs` file beneath `dir` that passes
/// `is_supported_rust_file`.
fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip target/ at any depth.
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

/// Scan a file for lines whose trimmed content begins with
/// `#[tokio::test`. Returns (line_number, raw_line) for each hit.
fn scan_file_for_active_tokio_test(path: &Path) -> Vec<(usize, String)> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[tokio::test") {
                Some((idx + 1, line.to_string()))
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn wa_22x4r_no_active_tokio_test_attributes_in_supported_paths() {
    let root = workspace_root();
    let mut rust_files = Vec::new();
    for src_root in supported_path_roots(&root) {
        if src_root.exists() {
            collect_rust_files(&src_root, &mut rust_files);
        }
    }

    assert!(
        !rust_files.is_empty(),
        "scan must find at least one Rust file under crates/ or frankenterm/ \
         (checked {})",
        root.display()
    );

    let mut violations: Vec<(PathBuf, Vec<(usize, String)>)> = Vec::new();
    for path in &rust_files {
        let hits = scan_file_for_active_tokio_test(path);
        if !hits.is_empty() {
            violations.push((path.clone(), hits));
        }
    }

    assert!(
        violations.is_empty(),
        "wa-22x4r regression: {} file(s) reintroduced `#[tokio::test]` \
         attributes in supported-path code. Port each offending test to \
         LabRuntime (run_lab_test / lab_test helpers) before re-running:\n{}",
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

/// Companion positive-signal test: the port landed many `*_labruntime.rs`
/// files. If every one of them disappeared, this suite would silently
/// pass — which would be a regression of the port work itself, not of
/// `#[tokio::test]` usage. Assert that a nontrivial number of port files
/// still exist so accidental deletion is caught.
#[test]
fn wa_22x4r_labruntime_port_files_still_exist() {
    let tests_dir = workspace_root()
        .join("crates")
        .join("frankenterm-core")
        .join("tests");
    let entries = fs::read_dir(&tests_dir).expect("read tests dir");
    let port_files: Vec<_> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with("_labruntime.rs"))
        })
        .collect();
    assert!(
        port_files.len() >= 20,
        "expected ≥20 `*_labruntime.rs` port files under {}, found {}; \
         the wa-22x4r port work may have regressed",
        tests_dir.display(),
        port_files.len()
    );
}
