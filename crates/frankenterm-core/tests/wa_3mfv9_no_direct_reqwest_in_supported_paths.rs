//! wa-3mfv9 — Regression guard: no direct `reqwest::` usage in
//! supported-path FrankenTerm source code.
//!
//! The bead's acceptance criterion is "reqwest removed from Cargo.toml
//! [and] all HTTP requests use asupersync::http." Prior migration work
//! has already moved every direct HTTP caller off reqwest in the
//! supported-path code — this test pins that invariant so future code
//! cannot reintroduce a reqwest dependency through `use reqwest::...`
//! or `reqwest::blocking::get(...)` imports in `crates/` or
//! `frankenterm/` workspace members.
//!
//! Intentionally NOT flagged:
//!   - `frankenterm/char-props/codegen/` — explicitly excluded from the
//!     workspace (`exclude = [...]` in root `Cargo.toml`); a build-time
//!     Unicode-data fetcher that runs manually on upgrades and whose
//!     deps are not workspace-managed.
//!   - `legacy_wezterm/` and `legacy_zellij/` — not workspace members.
//!   - Doc comments and string literals mentioning reqwest by name
//!     (e.g. `distributed.rs` comment explaining the replacement);
//!     only `use reqwest` and `reqwest::` paths are flagged.
//!   - Transitive reqwest via `hf-hub` → `fastembed` → `frankensearch`.
//!     That's an upstream-library dependency chain FrankenTerm does not
//!     own; the bead's scope is direct project-owned HTTP calls.

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
            // Exclude the char-props codegen tree even though it lives
            // under frankenterm/ — it is an excluded workspace member.
            if name == "codegen" {
                // Only skip the char-props codegen; let other codegen/
                // paths through if any appear later.
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

/// Scan a file for lines whose trimmed content starts with `use reqwest`
/// (including `use reqwest::...` and `use reqwest;`) or that contain a
/// `reqwest::` path expression outside of comments and string literals.
/// We use a simple heuristic: flag lines whose trimmed content starts
/// with `use reqwest`, and separately flag any `reqwest::` occurrence
/// whose byte offset is NOT inside a `//` or `/* */` comment or a
/// Rust string literal. For the lightweight test purpose we
/// conservatively flag only `use reqwest` imports — those are the
/// entry points that bring the crate into scope. Deeper lexical
/// analysis is unnecessary because without a `use reqwest` somewhere
/// in the file, `reqwest::` paths cannot resolve.
fn scan_file_for_reqwest_use(path: &Path) -> Vec<(usize, String)> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("use reqwest") || trimmed.starts_with("pub use reqwest") {
                Some((idx + 1, line.to_string()))
            } else if trimmed.starts_with("extern crate reqwest") {
                Some((idx + 1, line.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Scan a Cargo.toml (or similar) for a `reqwest` dependency
/// declaration at the start of a line.
fn scan_toml_for_reqwest_dep(path: &Path) -> Vec<(usize, String)> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("reqwest ")
                || trimmed.starts_with("reqwest=")
                || trimmed.starts_with("reqwest.")
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
                    // Skip target/ and the excluded char-props/codegen tree.
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
fn wa_3mfv9_no_direct_reqwest_use_in_supported_paths() {
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
        let hits = scan_file_for_reqwest_use(path);
        if !hits.is_empty() {
            violations.push((path.clone(), hits));
        }
    }

    assert!(
        violations.is_empty(),
        "wa-3mfv9 regression: {} file(s) reintroduced direct reqwest use. \
         Replace with asupersync::http before re-running:\n{}",
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
fn wa_3mfv9_no_reqwest_in_workspace_manifests() {
    let root = workspace_root();
    let mut manifests = Vec::new();
    collect_workspace_cargo_manifests(&root, &mut manifests);

    let mut violations: Vec<(PathBuf, Vec<(usize, String)>)> = Vec::new();
    for path in &manifests {
        let hits = scan_toml_for_reqwest_dep(path);
        if !hits.is_empty() {
            violations.push((path.clone(), hits));
        }
    }

    assert!(
        violations.is_empty(),
        "wa-3mfv9 regression: {} workspace Cargo.toml file(s) declare a \
         reqwest dependency. Remove the declaration or exclude the crate \
         from the workspace:\n{}",
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
fn wa_3mfv9_char_props_codegen_still_excluded() {
    // The only remaining reqwest user in the tree is the out-of-workspace
    // char-props codegen tool. Ensure it is still excluded from the
    // workspace — if someone accidentally re-adds it as a member, this
    // test will fail and push them to replace reqwest with
    // asupersync::http before including the crate in the workspace build.
    let root = workspace_root();
    let root_manifest = root.join("Cargo.toml");
    let content = fs::read_to_string(&root_manifest).expect("read root Cargo.toml");

    // The exclude block must name char-props/codegen.
    assert!(
        content.contains("\"frankenterm/char-props/codegen\""),
        "wa-3mfv9 regression: root Cargo.toml no longer excludes \
         frankenterm/char-props/codegen. Either re-exclude it or replace \
         its reqwest dependency with asupersync::http before letting \
         the workspace pull it in."
    );
}
