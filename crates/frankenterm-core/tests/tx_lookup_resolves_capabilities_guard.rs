//! ft-xxfwy.13 / ft-zhwa6 — every production tx target lookup resolves pane
//! capabilities.
//!
//! A bare `StorageBackedPrepareTargetLookup::new(..)` reads
//! `capabilities.prompt_active` as unknown, so PromptActive preconditions can
//! never pass from that surface. Production code must chain
//! `.with_resolved_capabilities(..)` onto every construction; test modules are
//! exempt.

use std::path::{Path, PathBuf};

const CONSTRUCTOR: &str = "StorageBackedPrepareTargetLookup::new(";
const RESOLVER: &str = ".with_resolved_capabilities(";
/// How far after the constructor the resolver call must appear.
const WINDOW_BYTES: usize = 400;

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

/// Source before the file's first `mod tests {` block (test code is exempt).
fn production_part(source: &str) -> &str {
    source
        .find("\nmod tests {")
        .map_or(source, |index| &source[..index])
}

/// Line numbers of constructions not followed by the resolver.
fn bare_constructions(source: &str) -> Vec<usize> {
    let production = production_part(source);
    production
        .match_indices(CONSTRUCTOR)
        .filter(|(index, _)| {
            let end = (index + CONSTRUCTOR.len() + WINDOW_BYTES).min(production.len());
            !production[*index..end].contains(RESOLVER)
        })
        .map(|(index, _)| production[..index].matches('\n').count() + 1)
        .collect()
}

#[test]
fn production_tx_lookups_always_resolve_capabilities() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&manifest.join("src"), &mut files);
    rust_files(&manifest.join("../frankenterm/src"), &mut files);

    let mut constructions = 0;
    let mut violations = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read source");
        constructions += production_part(&source).matches(CONSTRUCTOR).count();
        for line in bare_constructions(&source) {
            violations.push(format!("{}:{line}", file.display()));
        }
    }
    assert!(
        constructions >= 5,
        "guard must see the CLI/MCP tx lookups it polices (saw {constructions})"
    );
    assert!(
        violations.is_empty(),
        "tx target lookups without resolved capabilities (PromptActive can never pass):\n{}",
        violations.join("\n")
    );
}

#[test]
fn guard_flags_bare_constructions_and_ignores_tests() {
    let resolved = "let t = StorageBackedPrepareTargetLookup::new(None, Some(&s))\n    .with_resolved_capabilities(caps);\n";
    assert!(bare_constructions(resolved).is_empty());
    let bare = "fn f() {\n    let t = StorageBackedPrepareTargetLookup::new(None, Some(&s));\n}\n";
    assert_eq!(bare_constructions(bare), vec![2]);
    let in_tests = format!("fn f() {{}}\nmod tests {{\n{bare}}}\n");
    assert!(bare_constructions(&in_tests).is_empty());
}
