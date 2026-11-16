//! ft-4gdy5 — Regression guard: no `[] as [<this_module>::Type; 0]` casts.
//!
//! Hundreds of test assertions once spelled empty-array comparisons as
//! `[] as [module_name::Type; 0]` inside `module_name.rs` itself. Inside that
//! module the name `module_name` is not in scope, so Rust resolves it as an
//! extern crate and the whole frankenterm-core lib test harness failed with
//! E0433 before any test ran. Paths through real crates (`std::`, `mux::`,
//! sibling crates) are valid and stay allowed; only a path whose first segment
//! is the enclosing file's own module name is rejected.

use std::path::{Path, PathBuf};

fn core_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The module name a file's own items are declared under: the file stem, or
/// the directory name for `mod.rs`.
fn own_module_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem == "mod" {
        return Some(path.parent()?.file_name()?.to_str()?.to_string());
    }
    Some(stem.to_string())
}

/// First path segment of every `[] as [<segment>::...; 0]` cast on `line`.
fn empty_array_cast_roots(line: &str) -> Vec<&str> {
    let mut roots = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("[] as [") {
        let after = &rest[start + "[] as [".len()..];
        let segment_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        if after[segment_len..].starts_with("::") {
            roots.push(&after[..segment_len]);
        }
        rest = after;
    }
    roots
}

#[test]
fn empty_array_casts_never_name_their_own_module() {
    let mut files = Vec::new();
    rust_files(&core_src(), &mut files);
    assert!(files.len() > 100, "scanned only {} files", files.len());

    let mut casts_seen = 0usize;
    let mut violations = Vec::new();
    for file in &files {
        let Some(module) = own_module_name(file) else {
            continue;
        };
        let source = std::fs::read_to_string(file).expect("read source file");
        for (index, line) in source.lines().enumerate() {
            for root in empty_array_cast_roots(line) {
                casts_seen += 1;
                if root == module {
                    violations.push(format!(
                        "{}:{}: `[] as [{root}::…; 0]` names its own module",
                        file.strip_prefix(core_src()).unwrap_or(file).display(),
                        index + 1
                    ));
                }
            }
        }
    }
    assert!(casts_seen > 0, "the guard must see the casts it polices");
    assert!(
        violations.is_empty(),
        "same-module paths resolve as extern crates (E0433); use an in-scope type:\n{}",
        violations.join("\n")
    );
}

#[test]
fn guard_detects_the_same_module_shape_and_allows_crate_paths() {
    assert_eq!(
        empty_array_cast_roots("assert_eq!(x, [] as [wal_engine::Entry; 0]);"),
        vec!["wal_engine"]
    );
    assert_eq!(
        empty_array_cast_roots("a == [] as [std::vec::Vec<u8>; 0] && b == [] as [mux::Pane; 0]"),
        vec!["std", "mux"]
    );
    assert!(empty_array_cast_roots("let v: [u8; 0] = [];").is_empty());
    assert!(empty_array_cast_roots("[] as [u8; 0]").is_empty());
    assert_eq!(
        own_module_name(Path::new("src/tui/state.rs")).as_deref(),
        Some("state")
    );
    assert_eq!(
        own_module_name(Path::new("src/workflows/mod.rs")).as_deref(),
        Some("workflows")
    );
}
