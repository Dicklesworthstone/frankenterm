//! E2E coverage for `ft steer plan` (ft-7h5da.6.2) — drives the real binary
//! against a temp workspace and asserts the deterministic SteeringReceipt.

use assert_cmd::Command;
use frankenterm_core::config::Config;
use std::path::Path;
use tempfile::TempDir;

fn ft(ws: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ft").expect("ft binary");
    cmd.env("FT_WORKSPACE", ws)
        .env("FT_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

fn workspace() -> TempDir {
    let ws = TempDir::new().expect("temp workspace");
    let db = Config::default().effective_db_path(ws.path());
    std::fs::create_dir_all(db.parent().expect("db parent")).expect("create .ft dir");
    ws
}

fn stdout(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8")
}

#[test]
fn steer_plan_clean_ready_json_receipt() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer", "plan", "--objective", "ship the W3 family", "--scenario",
                "clean-ready", "--format", "json",
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("\"receipt_id\""), "no receipt_id: {out}");
    assert!(out.contains("steer:"), "receipt id not content-addressed: {out}");
    assert!(out.contains("envelope.admit"), "wrong verdict: {out}");
    assert!(out.contains("950"), "missing rehearsal score: {out}");
}

#[test]
fn steer_plan_rch_blocked_plain() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args(["steer", "plan", "--objective", "x", "--scenario", "rch-blocked"])
            .assert()
            .success(),
    );
    assert!(out.contains("rch_substrate_blocked"), "wrong status: {out}");
    assert!(
        out.contains("envelope.blocked.rch_substrate"),
        "wrong verdict: {out}"
    );
}

#[test]
fn steer_plan_approval_required_lists_approval() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer", "plan", "--objective", "x", "--scenario", "approval-required",
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("requires_approval"), "{out}");
    assert!(out.contains("owner_handoff"), "approval not listed: {out}");
}

#[test]
fn steer_plan_deterministic_receipt_id() {
    let w = workspace();
    let run = || {
        stdout(
            ft(w.path())
                .args([
                    "steer", "plan", "--objective", "same", "--scenario", "clean-ready",
                    "--format", "json",
                ])
                .assert()
                .success(),
        )
    };
    let a: serde_json::Value = serde_json::from_str(&run()).expect("json a");
    let b: serde_json::Value = serde_json::from_str(&run()).expect("json b");
    assert_eq!(
        a["receipt_id"], b["receipt_id"],
        "receipt_id must be deterministic (content-addressed)"
    );
}

#[test]
fn steer_plan_persists_receipt_artifact() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer", "plan", "--objective", "persist me", "--scenario", "clean-ready",
                "--format", "json",
            ])
            .assert()
            .success(),
    );
    let receipt: serde_json::Value = serde_json::from_str(&out).expect("json receipt");
    let id = receipt["receipt_id"].as_str().expect("receipt_id");
    let safe = id.replace(':', "_");
    let path = w
        .path()
        .join(".ft/steer_receipts")
        .join(format!("{safe}.json"));
    assert!(
        path.exists(),
        "receipt artifact must be persisted at {}",
        path.display()
    );
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read stored")).expect("json");
    assert_eq!(
        stored["receipt_id"], receipt["receipt_id"],
        "persisted receipt must match the printed one"
    );
}

#[test]
fn steer_plan_rejects_unknown_scenario() {
    let w = workspace();
    ft(w.path())
        .args(["steer", "plan", "--objective", "x", "--scenario", "bogus"])
        .assert()
        .failure();
}
