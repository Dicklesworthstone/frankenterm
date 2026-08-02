//! Strict-remote workspace formatting proof.
//!
//! RCH classifies direct `cargo fmt` as a light local command and therefore
//! rejects it when remote execution is required.  This integration-test
//! wrapper keeps the outer command compilation-bearing while running the
//! canonical workspace-wide formatter check on the remote checkout.  The
//! retained outer RCH command is the source-identity authority: RCH clean
//! mirrors intentionally omit `.git`, so this test validates the requested
//! revision label and source-mode contract but does not pretend that a
//! caller-supplied environment value proves Git identity by itself.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REQUESTED_SHA_ENV: &str = "FT_FORMAT_PROOF_SHA";
const SOURCE_MODE_ENV: &str = "FT_FORMAT_PROOF_SOURCE_MODE";
const SCOPED_PATHS_ENV: &str = "FT_FORMAT_PROOF_PATHS";
const RCH_CLEAN_BASELINE_SOURCE_MODE: &str = "rch-clean-baseline-no-overlay-v1";
const MAX_SCOPED_PATHS: usize = 64;
const MAX_SCOPED_PATH_BYTES: usize = 16 * 1024;

struct FormatProofContext {
    repo_root: PathBuf,
    requested_sha: String,
    source_mode: String,
    rustfmt: OsString,
}

#[test]
fn workspace_formatting_is_clean_under_rch_source_contract() {
    let proof = format_proof_context();
    let cargo = OsStr::new(env!("CARGO"));
    let output = Command::new(cargo)
        .current_dir(&proof.repo_root)
        .env("RUSTFMT", &proof.rustfmt)
        .args(["fmt", "--all", "--", "--check"])
        .output()
        .unwrap_or_else(|err| panic!("spawn workspace cargo fmt --check: {err}"));
    let output = assert_command_success("cargo fmt --all -- --check", output);
    assert_formatter_silent("cargo fmt --all -- --check", &output);
    println!(
        "workspace formatting check passed for requested revision {}; exact source identity remains \
         bound by the retained RCH command",
        proof.requested_sha
    );
    println!(
        "WORKSPACE_FORMAT_PROOF_SUCCESS requested_sha={} source_mode={}",
        proof.requested_sha, proof.source_mode
    );
}

/// Exact-source formatting proof for a bounded list of owned Rust files.
///
/// This is intentionally not a substitute for the workspace-wide test above.
/// It exists so one owned change can retain authoritative formatting evidence
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

fn format_proof_context() -> FormatProofContext {
    let repo_root = repo_root();
    let requested_sha = env::var(REQUESTED_SHA_ENV)
        .unwrap_or_else(|_| panic!("{REQUESTED_SHA_ENV} must label the RCH --base revision"));
    assert!(
        requested_sha.len() == 40 && requested_sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{REQUESTED_SHA_ENV} must be one full 40-hex Git object name, got {requested_sha:?}"
    );
    let source_mode = env::var(SOURCE_MODE_ENV).unwrap_or_else(|_| {
        panic!(
            "{SOURCE_MODE_ENV} must be {RCH_CLEAN_BASELINE_SOURCE_MODE:?}; the retained RCH \
             command must independently prove --base, --clean-overlay, and --no-overlay"
        )
    });
    assert_eq!(
        source_mode, RCH_CLEAN_BASELINE_SOURCE_MODE,
        "unsupported formatting source mode"
    );
    println!(
        "requested formatting revision: {requested_sha}; source authority: retained RCH \
         --base/--clean-overlay/--no-overlay command"
    );

    let cargo = OsStr::new(env!("CARGO"));
    let rustfmt = env::var_os("RUSTFMT").unwrap_or_else(|| OsString::from("rustfmt"));
    print_version(&repo_root, "cargo", cargo, ["--version"], "cargo ");
    print_version(
        &repo_root,
        "rustc",
        OsStr::new("rustc"),
        ["-Vv"],
        "rustc ",
    );
    print_version(
        &repo_root,
        "rustfmt",
        &rustfmt,
        ["--version"],
        "rustfmt ",
    );

    assert_accepted_rustfmt_stdin(
        &repo_root,
        &rustfmt,
        "fn main() {\n    println!(\"formatted canary\");\n}\n",
        "already-formatted canary",
    );
    assert_rewritten_rustfmt_stdin(
        &repo_root,
        &rustfmt,
        "fn main(){println!(\"unformatted canary\");}\n",
        "fn main() {\n    println!(\"unformatted canary\");\n}\n",
        "valid but unformatted canary",
    );
    assert_malformed_rustfmt_stdin_fails(
        &repo_root,
        &rustfmt,
        "fn main( {\n",
        "malformed Rust canary",
    );
    FormatProofContext {
        repo_root,
        requested_sha,
        source_mode,
        rustfmt,
    }
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
        assert!(!entry.is_empty(), "{SCOPED_PATHS_ENV} contains an empty path");
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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("frankenterm-core must live under crates/frankenterm-core")
        .to_path_buf()
}

fn print_version<I, S>(
    repo_root: &Path,
    label: &str,
    program: &OsStr,
    args: I,
    expected_prefix: &str,
)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .current_dir(repo_root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("spawn {label} version probe: {err}"));
    let output = assert_command_success(label, output);
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|err| panic!("{label} version output must be UTF-8: {err}"));
    let version = stdout.trim();
    assert!(!version.is_empty(), "{label} version output must not be empty");
    assert!(
        version.starts_with(expected_prefix),
        "{label} version output must start with {expected_prefix:?}, got {version:?}"
    );
    println!("{label}: {version}");
}

fn assert_accepted_rustfmt_stdin(
    repo_root: &Path,
    rustfmt: &OsStr,
    source: &str,
    label: &str,
) {
    let output = rustfmt_stdin_output(repo_root, rustfmt, source, label);
    assert!(
        output.status.success(),
        "rustfmt rejected {label}; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        source.as_bytes(),
        "rustfmt changed {label}; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_rewritten_rustfmt_stdin(
    repo_root: &Path,
    rustfmt: &OsStr,
    source: &str,
    expected: &str,
    label: &str,
) {
    assert_ne!(source, expected, "{label} must require a formatter rewrite");
    let output = rustfmt_stdin_output(repo_root, rustfmt, source, label);
    assert!(
        output.status.success(),
        "rustfmt failed to rewrite {label}; status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        expected.as_bytes(),
        "rustfmt did not produce the canonical rewrite for {label}; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_malformed_rustfmt_stdin_fails(
    repo_root: &Path,
    rustfmt: &OsStr,
    source: &str,
    label: &str,
) {
    let output = rustfmt_stdin_output(repo_root, rustfmt, source, label);
    assert_eq!(
        output.status.code(),
        Some(1),
        "rustfmt must reject {label} with exit code 1, got {}; stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr)
        .unwrap_or_else(|err| panic!("rustfmt {label} diagnostic must be UTF-8: {err}"));
    assert!(
        stderr.to_ascii_lowercase().contains("error"),
        "rustfmt rejected {label} without a stable error marker: {stderr:?}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("thread '"),
        "rustfmt crashed instead of diagnosing {label}: {stderr:?}"
    );
}

fn rustfmt_stdin_output(
    repo_root: &Path,
    rustfmt: &OsStr,
    source: &str,
    label: &str,
) -> Output {
    let mut child = Command::new(rustfmt)
        .current_dir(repo_root)
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn rustfmt for {label}: {err}"));
    child
        .stdin
        .take()
        .expect("rustfmt canary stdin must be piped")
        .write_all(source.as_bytes())
        .unwrap_or_else(|err| panic!("write {label}: {err}"));
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("wait for rustfmt {label}: {err}"))
}

fn assert_command_success(label: &str, output: Output) -> Output {
    assert!(
        output.status.success(),
        "{label} failed with {}; stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_formatter_silent(label: &str, output: &Output) {
    for (stream_name, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            bytes.is_empty(),
            "{label} emitted unexpected {stream_name} despite success: {}",
            String::from_utf8_lossy(bytes)
        );
    }
}
