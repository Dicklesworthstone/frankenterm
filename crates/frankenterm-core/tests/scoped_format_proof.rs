//! Strict-remote formatting proof for a bounded set of owned Rust files.
//!
//! This target is deliberately separate from `workspace_format_proof`: the
//! workspace authority contract requires its exact invocation to execute one
//! test with zero filtered tests. Keeping the scoped fallback in this binary
//! preserves both proof surfaces without making either transcript ambiguous.

#[path = "common/format_proof_support.rs"]
mod format_proof_support;

use format_proof_support::{assert_command_success, assert_formatter_silent, format_proof_context};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const SCOPED_PATHS_ENV: &str = "FT_FORMAT_PROOF_PATHS";
const MAX_SCOPED_PATHS: usize = 64;
const MAX_SCOPED_PATH_BYTES: usize = 16 * 1024;

/// Exact-source formatting proof for a bounded list of owned Rust files.
///
/// This is intentionally not a substitute for the workspace-wide test. It
/// exists so one owned change can retain authoritative formatting evidence
/// while unrelated committed formatter drift keeps the broader gate red.
#[test]
fn scoped_formatting_is_clean_under_rch_source_contract() {
    let proof = format_proof_context();
    let paths = scoped_format_paths(&proof.repo_root);
    let mut command = Command::new(&proof.rustfmt);
    command
        .current_dir(&proof.repo_root)
        .args(["--edition", "2024", "--check"]);
    for path in &paths {
        command.arg(path);
    }
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("spawn scoped rustfmt --check: {err}"));
    let output = assert_command_success("scoped rustfmt --check", output);
    assert_formatter_silent("scoped rustfmt --check", &output);
    let rendered_paths = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "SCOPED_FORMAT_PROOF_SUCCESS requested_sha={} source_mode={} paths={rendered_paths}",
        proof.requested_sha, proof.source_mode
    );
}

#[test]
fn workspace_format_proof_target_declares_exactly_one_test() {
    let source = include_str!("workspace_format_proof.rs");
    let declared_tests = source
        .lines()
        .filter(|line| line.trim() == "#[test]")
        .count();
    assert_eq!(
        declared_tests, 1,
        "workspace format authority must expose exactly one test so its exact command reports zero filtered tests"
    );
    assert!(source.contains("fn workspace_formatting_is_clean_under_rch_source_contract()"));
}

fn scoped_format_paths(repo_root: &Path) -> Vec<PathBuf> {
    let raw = env::var(SCOPED_PATHS_ENV)
        .unwrap_or_else(|_| panic!("{SCOPED_PATHS_ENV} must list comma-separated Rust files"));
    assert!(
        !raw.is_empty() && raw.len() <= MAX_SCOPED_PATH_BYTES,
        "{SCOPED_PATHS_ENV} must contain 1..={MAX_SCOPED_PATH_BYTES} bytes"
    );
    let mut unique = BTreeSet::new();
    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|err| panic!("canonicalize repository root: {err}"));
    for entry in raw.split(',') {
        assert!(
            !entry.is_empty(),
            "{SCOPED_PATHS_ENV} contains an empty path"
        );
        let path = Path::new(entry);
        assert!(
            !path.is_absolute()
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "{SCOPED_PATHS_ENV} path must be a normalized repository-relative path: {entry:?}"
        );
        assert_eq!(
            path.extension(),
            Some(OsStr::new("rs")),
            "{SCOPED_PATHS_ENV} path must name a Rust source file: {entry:?}"
        );
        let candidate = repo_root.join(path);
        let metadata = candidate
            .symlink_metadata()
            .unwrap_or_else(|err| panic!("stat scoped format path {entry:?}: {err}"));
        assert!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "{SCOPED_PATHS_ENV} path must name a non-symlink regular file: {entry:?}"
        );
        let canonical_candidate = candidate
            .canonicalize()
            .unwrap_or_else(|err| panic!("canonicalize scoped format path {entry:?}: {err}"));
        assert!(
            canonical_candidate.starts_with(&canonical_root),
            "{SCOPED_PATHS_ENV} path escapes the repository through a symlink: {entry:?}"
        );
        assert!(
            unique.insert(path.to_path_buf()),
            "{SCOPED_PATHS_ENV} contains duplicate path {entry:?}"
        );
        assert!(
            unique.len() <= MAX_SCOPED_PATHS,
            "{SCOPED_PATHS_ENV} exceeds the {MAX_SCOPED_PATHS}-file proof bound"
        );
    }
    unique.into_iter().collect()
}
