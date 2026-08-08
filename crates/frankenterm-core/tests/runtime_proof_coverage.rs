//! ft-3kv6e — RuntimeProof coverage ratchet (regression test).
//!
//! Asserts that every source-declared `pub async fn`, plus the exact allowlist
//! of intentionally eager settled-future APIs in
//! `crates/frankenterm-core/src/`, is classified by the RuntimeProof census
//! recorded at `tests/runtime_proof_coverage_baseline.json`.
//!
//! Each adoption commit is expected to lower the baseline. New
//! New async sites without `&Cx` / `RuntimeProof`, or eager-settled sites that
//! deviate from their direct-proof + exact-ready grammar, FAIL this test.
//!
//! The audit logic lives in `scripts/check_runtime_proof_coverage.py`
//! so the same source of truth runs in CI (advisory + enforce paths)
//! and in the developer's local `cargo test` loop. This test is the
//! Rust-side completion gate the bead's acceptance criteria asks for:
//! "A regression test in `tests/runtime_proof_coverage.rs` walks the
//! AST (or a syn-based macro) and asserts the invariant."
//!
//! Cross-references:
//! - ft-i2eni.1 — sealed RuntimeProof trait foundation
//! - ft-3kv6e   — adoption sweep (this test ratchets it)
//! - docs/runtime/runtime-proof-trait.md — doctrine

use std::path::PathBuf;
use std::process::Command;

#[test]
fn runtime_proof_coverage_does_not_regress() {
    // Walk up from CARGO_MANIFEST_DIR to find the repo root that owns
    // the audit script. The crate is `crates/frankenterm-core`, so the
    // workspace root is two levels up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    let script = repo_root.join("scripts/check_runtime_proof_coverage.py");
    assert!(
        script.is_file(),
        "ft-3kv6e: audit script missing at {}",
        script.display()
    );

    // The Python checker is the source of truth for this Cargo-addressable
    // gate. Missing tooling must fail closed: returning success here would let
    // an exact-SHA proof lane report a zero-work false positive.
    let python = which_python3().unwrap_or_else(|| {
        panic!(
            "ft-3kv6e: python3/python is required to execute the RuntimeProof coverage gate"
        )
    });

    let output = Command::new(python)
        .arg(&script)
        .arg("--summary")
        .current_dir(repo_root)
        .output()
        .expect("invoke audit script");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Always print the summary so a passing run still surfaces the
    // current numbers — useful when an adopt-and-lower commit needs to
    // know how far it has dropped the baseline.
    eprintln!("--- ft-3kv6e audit ---");
    eprintln!("{stdout}");
    if !stderr.trim().is_empty() {
        eprintln!("--- audit stderr ---");
        eprintln!("{stderr}");
    }

    assert!(
        output.status.success(),
        "ft-3kv6e: RuntimeProof coverage regressed.\n\
         Run `python3 scripts/check_runtime_proof_coverage.py` to see the offending sites,\n\
         or `--update-baseline` after lowering the count via Cx threading."
    );
}

fn which_python3() -> Option<PathBuf> {
    for cand in ["python3", "python"] {
        let out = Command::new(cand).arg("--version").output();
        if let Ok(o) = out
            && o.status.success()
        {
            return Some(PathBuf::from(cand));
        }
    }
    None
}
