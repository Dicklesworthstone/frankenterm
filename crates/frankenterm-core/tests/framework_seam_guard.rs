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

fn collect_named_files(root: &Path, file_name: &str) -> Vec<PathBuf> {
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
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| matches!(name, ".git" | "target"))
                {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) == Some(file_name) {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

fn non_comment_lines<'a>(content: &'a str) -> impl Iterator<Item = (usize, &'a str)> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#'))
}

fn manifest_has_optional_workspace_dependency(manifest: &str, dependency: &str) -> bool {
    non_comment_lines(manifest).any(|(_, line)| {
        let trimmed = line.trim();
        trimmed.starts_with(&format!("{dependency} "))
            && trimmed.contains("workspace = true")
            && trimmed.contains("optional = true")
    })
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
        for (line_index, line) in non_comment_lines(&content) {
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

#[test]
fn framework_manifest_references_stay_centralized_to_workspace_root_and_core_manifest() {
    let core_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = core_root
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("expected core crate to live under <workspace>/crates/"));
    let allowed: BTreeSet<PathBuf> = BTreeSet::from([
        workspace_root.join("Cargo.toml"),
        workspace_root.join("crates/frankenterm-core/Cargo.toml"),
    ]);
    let mut violations = Vec::new();

    for manifest in collect_named_files(workspace_root, "Cargo.toml") {
        if allowed.contains(&manifest) {
            continue;
        }

        let content = fs::read_to_string(&manifest).unwrap_or_else(|err| {
            panic!("failed to read manifest {}: {err}", manifest.display());
        });
        for (line_index, line) in non_comment_lines(&content) {
            if line.contains("fastmcp") || line.contains("fastapi") {
                let rel_path = manifest
                    .strip_prefix(workspace_root)
                    .unwrap_or(&manifest)
                    .display()
                    .to_string();
                violations.push(format!("{}:{}: {}", rel_path, line_index + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "framework manifest references must stay centralized to Cargo.toml and \
         crates/frankenterm-core/Cargo.toml:\n{}",
        violations.join("\n")
    );
}

#[test]
fn core_manifest_keeps_framework_dependencies_optional_and_cli_uses_feature_forwarding() {
    let core_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = core_root
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("expected core crate to live under <workspace>/crates/"));
    let core_manifest =
        fs::read_to_string(workspace_root.join("crates/frankenterm-core/Cargo.toml"))
            .expect("failed to read crates/frankenterm-core/Cargo.toml");
    let cli_manifest = fs::read_to_string(workspace_root.join("crates/frankenterm/Cargo.toml"))
        .expect("failed to read crates/frankenterm/Cargo.toml");

    assert!(
        manifest_has_optional_workspace_dependency(&core_manifest, "fastmcp"),
        "frankenterm-core must keep fastmcp as an optional workspace dependency"
    );
    assert!(
        manifest_has_optional_workspace_dependency(&core_manifest, "fastapi"),
        "frankenterm-core must keep fastapi as an optional workspace dependency"
    );
    assert!(
        !non_comment_lines(&cli_manifest)
            .any(|(_, line)| line.contains("fastmcp") || line.contains("fastapi")),
        "CLI manifest must consume framework support only through frankenterm-core features"
    );
}
