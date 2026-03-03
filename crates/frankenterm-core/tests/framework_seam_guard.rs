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
    files
}

#[test]
fn framework_imports_are_centralized_to_seam_modules() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = crate_root.join("src");
    let allowed: BTreeSet<&str> = BTreeSet::from(["mcp_framework.rs", "web_framework.rs"]);
    let mut violations = Vec::new();

    for file in collect_rust_files(&src_dir) {
        let file_name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if allowed.contains(file_name) {
            continue;
        }

        let content = fs::read_to_string(&file).unwrap_or_else(|err| {
            panic!("failed to read source file {}: {err}", file.display());
        });
        for (line_index, line) in content.lines().enumerate() {
            if line.contains("fastmcp::") || line.contains("fastapi::") {
                let rel_path = file
                    .strip_prefix(&crate_root)
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                violations.push(format!("{}:{}: {}", rel_path, line_index + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "direct fastmcp/fastapi imports must stay in mcp_framework.rs/web_framework.rs:\n{}",
        violations.join("\n")
    );
}
