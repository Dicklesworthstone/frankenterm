//! Every `.rs` file under `crates/frankenterm-core/src` must be reachable from
//! `lib.rs` through `mod` declarations.
//!
//! Why this exists: `mcp_helpers.rs` sat orphaned (not declared anywhere) from
//! 2026-03 to 2026-09 while agents kept editing it as if it were live code
//! (ft-nfk94, ft-xxfwy.25). An orphan compiles nothing, tests nothing, and
//! silently diverges from the module it duplicates. This test turns that
//! failure mode into a red build.
//!
//! The check is deliberately syntactic and conservative: a file `dir/name.rs`
//! is reachable when `mod name` (any visibility, inline or file-backed) is
//! declared in `dir.rs`, `dir/mod.rs`, or when some file names it through a
//! `#[path = "..."]` attribute. `cfg`-gated declarations count as reachable:
//! the point is ownership, not the active feature set.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("read source directory");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Names declared through `mod <name>` (any visibility, `;` or `{`) and any
/// relative paths named by `#[path = "..."]` in `contents`.
fn declared_modules(contents: &str) -> (HashSet<String>, HashSet<String>) {
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim_start();
        if let Some(rest) = line.strip_prefix("#[path") {
            if let Some(start) = rest.find('"') {
                if let Some(end) = rest[start + 1..].find('"') {
                    paths.insert(rest[start + 1..start + 1 + end].to_string());
                }
            }
            continue;
        }
        // Strip a leading visibility qualifier: `pub`, `pub(crate)`, `pub(super)`, ...
        let line = if let Some(rest) = line.strip_prefix("pub") {
            let rest = rest.trim_start();
            if let Some(after_paren) = rest.strip_prefix('(') {
                match after_paren.find(')') {
                    Some(close) => after_paren[close + 1..].trim_start(),
                    None => continue,
                }
            } else {
                rest
            }
        } else {
            line
        };
        let Some(rest) = line.strip_prefix("mod ") else {
            continue;
        };
        let name: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            names.insert(name);
        }
    }
    (names, paths)
}

#[test]
fn every_source_file_is_declared_as_a_module() {
    let root = source_root();
    let mut files = Vec::new();
    collect_rust_files(&root, &mut files);
    files.sort();
    assert!(
        files.len() > 100,
        "expected the core source tree, found {} files",
        files.len()
    );

    // Pass 1: gather every declaration site.
    let mut declared_in: Vec<(PathBuf, HashSet<String>)> = Vec::new();
    let mut path_attr_targets: HashSet<PathBuf> = HashSet::new();
    for file in &files {
        let contents = std::fs::read_to_string(file).expect("read source file");
        let (names, paths) = declared_modules(&contents);
        let dir = file.parent().expect("source file has a parent");
        for rel in paths {
            path_attr_targets.insert(dir.join(rel));
        }
        declared_in.push((file.clone(), names));
    }

    // Pass 2: every file must be owned by a parent declaration.
    let mut orphans = BTreeSet::new();
    for file in &files {
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf-8 file stem")
            .to_string();
        let dir = file.parent().expect("parent");
        if file == &root.join("lib.rs") || path_attr_targets.contains(file) {
            continue;
        }
        let (module_name, owner_candidates): (String, Vec<PathBuf>) = if stem == "mod" {
            let module_dir_name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .expect("module dir name")
                .to_string();
            let grandparent = dir.parent().expect("module dir parent");
            (
                module_dir_name.clone(),
                vec![
                    grandparent.join(format!("{module_dir_name}.rs")),
                    grandparent.join("mod.rs"),
                    grandparent.join("lib.rs"),
                ],
            )
        } else {
            let dir_name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let parent_of_dir = dir.parent().map(Path::to_path_buf);
            let mut candidates = vec![dir.join("mod.rs"), dir.join("lib.rs")];
            if let Some(parent) = parent_of_dir {
                candidates.push(parent.join(format!("{dir_name}.rs")));
            }
            (stem.clone(), candidates)
        };
        let owned = declared_in.iter().any(|(owner, names)| {
            owner_candidates.iter().any(|c| c == owner) && names.contains(&module_name)
        });
        if !owned {
            orphans.insert(file.strip_prefix(&root).unwrap_or(file).to_path_buf());
        }
    }

    // Ratchet: entries that no longer show up as orphans must be removed from
    // the baseline, so the list can only shrink.
    let stale_baseline: Vec<&str> = KNOWN_ORPHANS
        .iter()
        .copied()
        .filter(|known| !orphans.contains(Path::new(known)))
        .collect();
    assert!(
        stale_baseline.is_empty(),
        "KNOWN_ORPHANS entries are no longer orphaned; delete them from the baseline: {stale_baseline:?}"
    );
    orphans.retain(|path| {
        !KNOWN_ORPHANS
            .iter()
            .any(|known| Path::new(known) == path.as_path())
    });

    assert!(
        orphans.is_empty(),
        "orphaned source files (not declared by any `mod` or `#[path]`): {orphans:#?}\n\
         Either declare the module or remove the file (removal requires owner authorization per AGENTS.md Rule 1)."
    );
}

/// Orphans that pre-date this guard. Each is tracked dead code that the guard
/// found on its first remote run (2026-09-02); deleting them needs owner
/// authorization (AGENTS.md Rule 1), tracked on ft-xxfwy.31. New orphans are
/// never added here; fix them instead.
const KNOWN_ORPHANS: &[&str] = &[
    "cx_stub.rs",
    "search/model2vec_embedder.rs",
    "storage/handle/mod.rs",
    "test_subprocess_deadlock.rs",
];
