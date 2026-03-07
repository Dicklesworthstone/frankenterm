use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

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

fn collect_surface_files(crate_root: &Path, surfaces: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for surface in surfaces {
        let path = crate_root.join(surface);
        if path.exists() {
            files.extend(collect_rust_files(&path));
        }
    }
    files
}

#[test]
fn framework_imports_are_centralized_to_seam_modules_across_core_and_cli_surfaces() {
    let core_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = core_root
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("expected core crate to live under <workspace>/crates/"));
    let cli_root = workspace_root.join("crates/frankenterm");
    let allowed: BTreeSet<PathBuf> = BTreeSet::from([
        workspace_root.join("crates/frankenterm-core/src/mcp_framework.rs"),
        workspace_root.join("crates/frankenterm-core/src/web_framework.rs"),
        workspace_root.join("crates/frankenterm-core/tests/framework_seam_guard.rs"),
    ]);
    let mut violations = Vec::new();
    let mut files = collect_surface_files(&core_root, &["src", "tests", "benches"]);
    files.extend(collect_surface_files(
        &cli_root,
        &["src", "tests", "benches"],
    ));

    for file in files {
        if allowed.contains(&file) {
            continue;
        }

        let content = fs::read_to_string(&file).unwrap_or_else(|err| {
            panic!("failed to read source file {}: {err}", file.display());
        });
        for (line_index, line) in content.lines().enumerate() {
            if line.contains("fastmcp::") || line.contains("fastapi::") {
                let rel_path = file
                    .strip_prefix(workspace_root)
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                violations.push(format!("{}:{}: {}", rel_path, line_index + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "direct fastmcp/fastapi imports must stay in frankenterm-core seam modules:\n{}",
        violations.join("\n")
    );
}
