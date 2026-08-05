//! ft-tf6g3.29 — model/code consistency for the RuntimeProof soundness proof.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn lean_model_runtime_proof_impls_match_rust_impls() {
    let repo_root = repo_root();
    let lean_path = repo_root.join("docs/proofs/runtime-proof-soundness.lean");
    let rust_path = repo_root.join("crates/frankenterm-core/src/runtime_proof.rs");

    let lean = fs::read_to_string(&lean_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", lean_path.display()));
    let rust = fs::read_to_string(&rust_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", rust_path.display()));

    let model_impls = parse_lean_impl_names(&lean);
    let runtime_impls = parse_rust_impls(&rust, "RuntimeProof for");
    let sealed_impls = parse_rust_impls(&rust, "sealed::Sealed for");

    assert!(
        !model_impls.is_empty(),
        "Lean model declared implementation list is empty"
    );

    assert_eq!(
        model_impls, runtime_impls,
        "Lean model's declared RuntimeProof implementation list drifted from runtime_proof.rs"
    );

    assert_eq!(
        runtime_impls, sealed_impls,
        "Every RuntimeProof impl must have exactly one matching sealed::Sealed impl"
    );
}

/// Cargo-addressable form of the Lean/e2e gate for strict remote workers.
///
/// The ordinary model-consistency test above needs no external toolchain.
/// This test is ignored by default because it intentionally requires the
/// repository-pinned Lean executable; proof lanes invoke it explicitly with
/// `--ignored --exact` so a missing Lean installation fails closed.
#[test]
#[ignore = "requires the repository-pinned Lean toolchain"]
fn lean_runtime_proof_soundness_e2e() {
    let root = repo_root();
    let script = root.join("tests/e2e/test_runtime_proof_check.sh");
    let output = Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|err| panic!("run {}: {err}", script.display()));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "runtime-proof Lean/e2e gate failed (status={}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr,
    );
    print!("{stdout}");
    eprint!("{stderr}");
}

fn repo_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn parse_lean_impl_names(text: &str) -> BTreeSet<String> {
    let mut in_list = false;
    let mut names = BTreeSet::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("def rustRuntimeProofImplNames") {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        if trimmed == "]" {
            break;
        }
        if let Some(name) = string_literal(trimmed) {
            names.insert(normalize_type_name(name));
        }
    }

    names
}

fn parse_rust_impls(text: &str, marker: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("impl") {
            continue;
        }
        let Some(after_marker) = trimmed.split_once(marker).map(|(_, tail)| tail) else {
            continue;
        };
        let Some((type_name, _)) = after_marker.split_once('{') else {
            continue;
        };
        names.insert(normalize_type_name(type_name.trim()));
    }

    names
}

fn string_literal(line: &str) -> Option<&str> {
    let line = line.trim_end_matches(',').trim();
    let tail = line.strip_prefix('"')?;
    let end = tail.find('"')?;
    Some(&tail[..end])
}

fn normalize_type_name(input: &str) -> String {
    input.split_whitespace().collect::<String>()
}
