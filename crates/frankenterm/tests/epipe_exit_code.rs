//! GH#75 regression test: `ft <anything> | head` must never SIGABRT.
//!
//! Rust's runtime sets `SIGPIPE` to `SIG_IGN`, so an early-closing
//! downstream reader surfaces as an `EPIPE` write error, `println!` panics
//! on it, and an unhandled write panic can turn an ordinary pipeline close
//! into an operator-visible crash. The fix
//! (`frankenterm_sigpipe::exit_quietly_on_broken_pipe`,
//! installed at the top of every binary's `main`) recognizes broken-pipe
//! write panics in a panic hook — which still runs before the abort — and
//! exits with the conventional `128 + SIGPIPE = 141` instead.
//!
//! The test closes the pipe's read end *before* spawning the child, so the
//! child's very first stdout write hits `EPIPE` deterministically — no race
//! against pipe-buffer capacity or reader timing.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

/// Run `ft <args>` with stdout connected to a pipe whose read end is
/// already closed, and return the resulting `ExitStatus`.
fn run_ft_with_closed_stdout(args: &[&str]) -> std::process::ExitStatus {
    let pipe = filedescriptor::Pipe::new().expect("create pipe");
    let stdout = pipe
        .write
        .as_stdio()
        .expect("convert pipe write end to Stdio");
    // Close the read end before the child exists: every write the child
    // makes to stdout must fail with EPIPE.
    drop(pipe.read);

    let home = tempfile::tempdir().expect("create scratch HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_ft"))
        .args(args)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .stdout(stdout)
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .expect("spawn ft");
    // Parent's copy of the write end closes when `pipe` drops (the child got
    // a dup); EPIPE depends only on the read end, which is already gone.
    drop(pipe.write);
    output.status
}

#[test]
fn ft_version_exits_141_when_stdout_reader_is_gone() {
    let status = run_ft_with_closed_stdout(&["version"]);
    assert_eq!(
        status.signal(),
        None,
        "ft must not die from a signal when stdout closes early (SIGABRT \
         regression, GH#75); got {status:?}"
    );
    assert_eq!(
        status.code(),
        Some(141),
        "ft must exit 128+SIGPIPE=141 on a broken stdout pipe; got {status:?}"
    );
}

#[test]
fn ft_help_exits_cleanly_or_141_but_never_signals() {
    // `--help` goes through clap's own printer; whatever exit path it takes,
    // a closed stdout must never surface as SIGABRT/SIGPIPE termination.
    let status = run_ft_with_closed_stdout(&["--help"]);
    assert_eq!(
        status.signal(),
        None,
        "ft --help must not die from a signal when stdout closes early; got {status:?}"
    );
}

#[test]
fn ft_version_still_succeeds_with_open_stdout() {
    let home = tempfile::tempdir().expect("create scratch HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_ft"))
        .arg("version")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn ft");
    assert!(
        output.status.success(),
        "ft version must still succeed normally: {:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ft "),
        "ft version output missing version banner"
    );
}
