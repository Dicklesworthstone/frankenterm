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

#[path = "common/format_proof_support.rs"]
mod format_proof_support;

use format_proof_support::{assert_command_success, assert_formatter_silent, format_proof_context};
use std::ffi::OsStr;
use std::process::Command;

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
