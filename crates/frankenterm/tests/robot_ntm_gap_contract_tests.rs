//! Contract harness for current `ft robot` NTM-gap dispatch.
//!
//! The schema/state-machine suite in `frankenterm-core` describes target
//! native semantics. This integration harness guards the live CLI boundary:
//! fallback actions must keep returning the structured `robot.not_implemented`
//! envelope, and implementation beads have a single manifest entry to flip
//! once an action is natively wired.

#![allow(deprecated)]

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum ExpectedBackend {
    NtmFallback,
    Native,
}

#[derive(Debug)]
struct RobotActionContract {
    family: &'static str,
    action: &'static str,
    args: &'static [&'static str],
    expected_backend: ExpectedBackend,
    is_mutation: bool,
}

const NTM_GAP_ACTIONS: &[RobotActionContract] = &[
    RobotActionContract {
        family: "checkpoint",
        action: "save",
        args: &["checkpoint", "save", "--label", "contract-smoke"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "checkpoint",
        action: "list",
        args: &["checkpoint", "list", "--limit", "1"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "checkpoint",
        action: "show",
        args: &["checkpoint", "show", "checkpoint-smoke"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "checkpoint",
        action: "delete",
        args: &["checkpoint", "delete", "checkpoint-smoke"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "checkpoint",
        action: "rollback",
        args: &["checkpoint", "rollback", "checkpoint-smoke", "--dry-run"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "context",
        action: "status",
        args: &["context", "status", "--pane-id", "1"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "context",
        action: "rotate",
        args: &["context", "rotate", "1", "--strategy", "gentle"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "context",
        action: "history",
        args: &["context", "history", "1", "--limit", "1"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "work",
        action: "claim",
        args: &["work", "claim", "ft-smoke", "--agent-id", "agent-a"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "work",
        action: "release",
        args: &["work", "release", "ft-smoke", "--reason", "contract-smoke"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "work",
        action: "complete",
        args: &[
            "work",
            "complete",
            "ft-smoke",
            "--summary",
            "done",
            "--evidence",
            "abc123",
        ],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "work",
        action: "list",
        args: &["work", "list", "--limit", "1"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "work",
        action: "ready",
        args: &["work", "ready", "--agent-id", "agent-a", "--limit", "1"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "work",
        action: "assign",
        args: &[
            "work",
            "assign",
            "ft-smoke",
            "--agent-id",
            "agent-a",
            "--strategy",
            "manual",
        ],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "fleet",
        action: "status",
        args: &["fleet", "status", "--detailed"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
    RobotActionContract {
        family: "fleet",
        action: "scale",
        args: &["fleet", "scale", "codex", "1", "--dry-run"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "fleet",
        action: "rebalance",
        args: &[
            "fleet",
            "rebalance",
            "--strategy",
            "round_robin",
            "--dry-run",
        ],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: true,
    },
    RobotActionContract {
        family: "fleet",
        action: "agents",
        args: &["fleet", "agents", "--program", "codex", "--state", "idle"],
        expected_backend: ExpectedBackend::NtmFallback,
        is_mutation: false,
    },
];

fn setup_workspace() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let ft_dir = dir.path().join(".ft");
    std::fs::create_dir_all(&ft_dir).expect("create .ft dir");
    std::fs::write(
        ft_dir.join("config.toml"),
        "[general]\nlog_level = \"error\"\n",
    )
    .expect("write quiet config");

    let db_path = ft_dir.join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");
    frankenterm_core::storage::initialize_schema(&conn).expect("init schema");
    drop(conn);

    let workspace = dir.path().to_string_lossy().to_string();
    (dir, workspace)
}

fn run_robot_json(workspace: &str, contract: &RobotActionContract) -> Value {
    let output = Command::cargo_bin("ft")
        .expect("locate ft binary")
        .env("FT_WORKSPACE", workspace)
        .args(["robot", "--format", "json"])
        .args(contract.args)
        .output()
        .unwrap_or_else(|err| panic!("ft robot {:?} should execute: {err}", contract.args));

    assert!(
        !output.stdout.is_empty(),
        "ft robot {:?} emitted no stdout; status: {:?}; stderr: {}",
        contract.args,
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "ft robot {:?} stdout should be JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            contract.args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_structured_ntm_fallback(contract: &RobotActionContract, payload: &Value) {
    assert_eq!(
        payload["ok"], false,
        "{} {} should currently be an explicit fallback",
        contract.family, contract.action
    );
    assert_eq!(
        payload["error_code"].as_str(),
        Some("robot.not_implemented")
    );
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!("ft robot {} {}", contract.family, contract.action)),
        "fallback error should name parsed command: {payload}"
    );
    assert!(
        payload["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("NTM equivalent:"),
        "fallback hint should name ntm equivalent: {payload}"
    );

    let data = &payload["data"];
    assert_eq!(data["family"].as_str(), Some(contract.family));
    assert_eq!(data["action"].as_str(), Some(contract.action));
    assert_eq!(data["is_mutation"].as_bool(), Some(contract.is_mutation));
    assert!(
        data["ntm_equivalence"]["ntm_commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty()),
        "fallback must expose at least one ntm equivalent: {payload}"
    );
    assert!(
        data["parsed_request"].is_object(),
        "fallback should include typed parsed_request metadata: {payload}"
    );
}

fn assert_native_not_ntm_fallback(contract: &RobotActionContract, payload: &Value) {
    assert_ne!(
        payload["error_code"].as_str(),
        Some("robot.not_implemented"),
        "{} {} is marked native but still returns the NTM-gap fallback",
        contract.family,
        contract.action
    );
}

#[test]
fn robot_ntm_gap_dispatch_matches_manifest() {
    let (_dir, workspace) = setup_workspace();

    for contract in NTM_GAP_ACTIONS {
        let payload = run_robot_json(&workspace, contract);
        match contract.expected_backend {
            ExpectedBackend::NtmFallback => assert_structured_ntm_fallback(contract, &payload),
            ExpectedBackend::Native => assert_native_not_ntm_fallback(contract, &payload),
        }
    }
}
