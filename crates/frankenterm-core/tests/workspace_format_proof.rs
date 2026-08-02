//! Strict-remote workspace formatting proof.
//!
//! RCH classifies direct `cargo fmt` as a light local command and therefore
//! rejects it when remote execution is required.  This integration-test
//! wrapper keeps the outer command compilation-bearing while running the
//! canonical workspace-wide formatter check on the exact remote checkout.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const EXPECTED_SHA_ENV: &str = "FT_FORMAT_PROOF_SHA";

#[test]
fn exact_sha_workspace_formatting_is_clean() {
    let repo_root = repo_root();
    let expected_sha = env::var(EXPECTED_SHA_ENV)
        .unwrap_or_else(|_| panic!("{EXPECTED_SHA_ENV} must bind the RCH --base revision"));
    assert!(
        expected_sha.len() == 40 && expected_sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{EXPECTED_SHA_ENV} must be one full hexadecimal Git SHA, got {expected_sha:?}"
    );

    let head = checked_output(&repo_root, "git", ["rev-parse", "HEAD"]);
    let actual_sha = String::from_utf8(head.stdout)
        .expect("git rev-parse HEAD must emit UTF-8")
        .trim()
        .to_owned();
    assert_eq!(
        actual_sha, expected_sha,
        "remote formatting checkout is not the requested exact revision"
    );
    assert_clean_checkout(&repo_root);

    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let rustfmt = env::var_os("RUSTFMT").unwrap_or_else(|| OsString::from("rustfmt"));
    print_version(&repo_root, "cargo", &cargo, ["--version"]);
    print_version(&repo_root, "rustc", OsStr::new("rustc"), ["-Vv"]);
    print_version(&repo_root, "rustfmt", &rustfmt, ["--version"]);

    assert_rejected_rustfmt_stdin(
        &repo_root,
        &rustfmt,
        "fn main(){println!(\"unformatted canary\");}\n",
        "valid but unformatted canary",
    );
    assert_rejected_rustfmt_stdin(
        &repo_root,
        &rustfmt,
        "fn main( {\n",
        "malformed Rust canary",
    );

    let output = Command::new(&cargo)
        .current_dir(&repo_root)
        .args(["fmt", "--all", "--", "--check"])
        .output()
        .unwrap_or_else(|err| panic!("spawn workspace cargo fmt --check: {err}"));
    assert_command_success("cargo fmt --all -- --check", output);
    println!("workspace formatting proof passed at exact SHA {actual_sha}");
}

fn assert_clean_checkout(repo_root: &Path) {
    let status = checked_output(
        repo_root,
        "git",
        ["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(
        status.stdout.is_empty(),
        "remote formatting checkout contains an index, worktree, or untracked overlay: {}",
        String::from_utf8_lossy(&status.stdout)
    );
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("frankenterm-core must live under crates/frankenterm-core")
        .to_path_buf()
}

fn checked_output<I, S>(repo_root: &Path, program: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .current_dir(repo_root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("spawn {program}: {err}"));
    assert_command_success(program, output)
}

fn print_version<I, S>(repo_root: &Path, label: &str, program: &OsStr, args: I)
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
    println!("{label}: {}", String::from_utf8_lossy(&output.stdout).trim());
}

fn assert_rejected_rustfmt_stdin(
    repo_root: &Path,
    rustfmt: &OsStr,
    source: &str,
    label: &str,
) {
    let mut child = Command::new(rustfmt)
        .current_dir(repo_root)
        .args(["--edition", "2024", "--check"])
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
    let output = child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("wait for rustfmt {label}: {err}"));
    assert!(
        !output.status.success(),
        "rustfmt incorrectly accepted {label}; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
