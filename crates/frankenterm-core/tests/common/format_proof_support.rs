use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REQUESTED_SHA_ENV: &str = "FT_FORMAT_PROOF_SHA";
const SOURCE_MODE_ENV: &str = "FT_FORMAT_PROOF_SOURCE_MODE";
const RCH_CLEAN_BASELINE_SOURCE_MODE: &str = "rch-clean-baseline-no-overlay-v1";

pub struct FormatProofContext {
    pub repo_root: PathBuf,
    pub requested_sha: String,
    pub source_mode: String,
    pub rustfmt: OsString,
}

pub fn format_proof_context() -> FormatProofContext {
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
    print_version(&repo_root, "rustc", OsStr::new("rustc"), ["-Vv"], "rustc ");
    print_version(&repo_root, "rustfmt", &rustfmt, ["--version"], "rustfmt ");

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
) where
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
    assert!(
        !version.is_empty(),
        "{label} version output must not be empty"
    );
    assert!(
        version.starts_with(expected_prefix),
        "{label} version output must start with {expected_prefix:?}, got {version:?}"
    );
    println!("{label}: {version}");
}

fn assert_accepted_rustfmt_stdin(repo_root: &Path, rustfmt: &OsStr, source: &str, label: &str) {
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

fn rustfmt_stdin_output(repo_root: &Path, rustfmt: &OsStr, source: &str, label: &str) -> Output {
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

pub fn assert_command_success(label: &str, output: Output) -> Output {
    assert!(
        output.status.success(),
        "{label} failed with {}; stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub fn assert_formatter_silent(label: &str, output: &Output) {
    for (stream_name, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            bytes.is_empty(),
            "{label} emitted unexpected {stream_name} despite success: {}",
            String::from_utf8_lossy(bytes)
        );
    }
}
