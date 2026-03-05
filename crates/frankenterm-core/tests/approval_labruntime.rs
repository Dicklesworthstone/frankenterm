//! LabRuntime-ported approval tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `approval.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for ApprovalStore
//! issue/consume operations against StorageHandle.
//!
//! StorageHandle internally uses tokio channels, which work correctly
//! under RuntimeFixture's current-thread runtime.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::approval::{ApprovalAuditContext, ApprovalStore};
use frankenterm_core::config::ApprovalConfig;
use frankenterm_core::error::Error;
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyInput, PolicySurface,
};
use frankenterm_core::storage::{AuditQuery, PaneRecord, StorageHandle, now_ms};
use std::sync::atomic::{AtomicU64, Ordering};

// ===========================================================================
// Helpers
// ===========================================================================

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path(suffix: &str) -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!(
        "wa_approval_labrt_{suffix}_{counter}_{}.db",
        std::process::id()
    ))
    .to_str()
    .unwrap()
    .to_string()
}

fn base_input() -> PolicyInput {
    PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(1)
        .with_domain("local")
        .with_text_summary("echo hi")
        .with_capabilities(PaneCapabilities::prompt())
}

fn test_pane() -> PaneRecord {
    PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

async fn setup_storage(suffix: &str) -> (StorageHandle, String) {
    let db_path = temp_db_path(suffix);
    let storage = StorageHandle::new(&db_path).await.unwrap();
    storage.upsert_pane(test_pane()).await.unwrap();
    (storage, db_path)
}

async fn cleanup(storage: StorageHandle, db_path: &str) {
    storage.shutdown().await.unwrap();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

// ===========================================================================
// Section 1: Issue and consume lifecycle
// ===========================================================================

#[test]
fn approval_issue_and_consume_allow_once() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("issue_consume").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();
        assert!(request.allow_once_full_hash.starts_with("sha256:"));
        assert_eq!(
            request.command,
            format!("ft approve {}", request.allow_once_code)
        );

        // First consume succeeds
        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_some());

        // Second consume fails (token already used)
        let second = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(second.is_none());

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_scope_mismatch_does_not_consume() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("scope_mismatch").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let request = store.issue(&input, None).await.unwrap();

        let wrong_pane = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(2)
            .with_domain("local")
            .with_text_summary("echo hi");
        let consumed = store
            .consume(&request.allow_once_code, &wrong_pane)
            .await
            .unwrap();
        assert!(consumed.is_none());

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_max_active_tokens_enforced() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("max_active").await;
        let config = ApprovalConfig {
            max_active_tokens: 1,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        store.issue(&input, None).await.unwrap();
        let second = store.issue(&input, None).await;
        assert!(matches!(second, Err(Error::Policy(_))));

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_expired_token_cannot_be_consumed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("expired").await;
        let config = ApprovalConfig {
            token_expiry_secs: 0,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        // Wait a tiny bit to ensure time has passed
        frankenterm_core::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_none(), "Expired token should not be consumable");

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_consume_with_context_records_correlation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("correlation").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        let audit_context = ApprovalAuditContext {
            correlation_id: Some("sha256:testcorr".to_string()),
            decision_context: Some("{\"stage\":\"approval\"}".to_string()),
        };
        let consumed = store
            .consume_with_context(&request.allow_once_code, &input, Some(audit_context))
            .await
            .unwrap();
        assert!(consumed.is_some());

        let query = AuditQuery {
            correlation_id: Some("sha256:testcorr".to_string()),
            ..Default::default()
        };
        let audits = storage.get_audit_actions(query).await.unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].correlation_id.as_deref(), Some("sha256:testcorr"));
        assert_eq!(
            audits[0].decision_context.as_deref(),
            Some("{\"stage\":\"approval\"}")
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_different_action_fingerprint_prevents_consumption() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("fingerprint").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let request = store.issue(&input, None).await.unwrap();

        let different_text = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_domain("local")
            .with_text_summary("echo different")
            .with_capabilities(PaneCapabilities::prompt());

        let consumed = store
            .consume(&request.allow_once_code, &different_text)
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Token should only work with matching fingerprint"
        );

        // Original input should still work
        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_some(), "Token should work with matching input");

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_different_policy_surface_prevents_consumption() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("surface_scope").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input().with_surface(PolicySurface::Robot);
        let request = store.issue(&input, None).await.unwrap();

        let different_surface = input.clone().with_surface(PolicySurface::Mcp);
        let consumed = store
            .consume(&request.allow_once_code, &different_surface)
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Token should be scoped to the same policy surface"
        );

        // Original input should still work
        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(
            consumed.is_some(),
            "Token should work with matching surface"
        );

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 2: Plan-bound approval tests
// ===========================================================================

#[test]
fn approval_issue_and_consume_plan_bound() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_bound").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let plan_hash = "sha256:plan123abc";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), Some("Low risk".to_string()))
            .await
            .unwrap();
        assert!(request.allow_once_full_hash.starts_with("sha256:"));

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(consumed.is_some(), "Matching plan_hash should succeed");

        let token = consumed.unwrap();
        assert_eq!(token.plan_hash.as_deref(), Some(plan_hash));
        assert_eq!(token.plan_version, Some(1));
        assert_eq!(token.risk_summary.as_deref(), Some("Low risk"));

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_plan_hash_mismatch_rejects_consumption() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_mismatch").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(&input, "sha256:originalplan", Some(1), None)
            .await
            .unwrap();

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, "sha256:differentplan")
            .await
            .unwrap();
        assert!(consumed.is_none(), "Mismatched plan_hash must be rejected");

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_plan_bound_token_expired_cannot_consume() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_expired").await;
        let config = ApprovalConfig {
            token_expiry_secs: 0,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(&input, "sha256:expiredplan", Some(1), None)
            .await
            .unwrap();

        frankenterm_core::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, "sha256:expiredplan")
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Expired plan-bound token should not be consumable"
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_plan_bound_scope_violation_rejected() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_scope").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(&input, "sha256:scopedplan", Some(1), None)
            .await
            .unwrap();

        let wrong_pane = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(99)
            .with_domain("local")
            .with_text_summary("echo hi")
            .with_capabilities(PaneCapabilities::prompt());

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &wrong_pane, "sha256:scopedplan")
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Wrong pane scope should reject even with correct plan_hash"
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_non_plan_bound_token_works_with_consume_for_plan() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("no_plan_bound").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, "sha256:anyplan")
            .await
            .unwrap();
        assert!(
            consumed.is_some(),
            "Non-plan-bound token should not reject based on plan_hash"
        );

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 3: Issue metadata and code format tests
// ===========================================================================

#[test]
fn approval_issue_with_custom_summary() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("custom_summary").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue(&input, Some("Custom approval summary".to_string()))
            .await
            .unwrap();
        assert_eq!(request.summary, "Custom approval summary");
        assert_eq!(request.allow_once_code.len(), 8); // DEFAULT_CODE_LEN

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_issue_generates_default_summary() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("default_summary").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        // Default summary should contain the action kind
        assert!(
            request.summary.contains("send_text"),
            "Default summary should contain action kind, got: {}",
            request.summary
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_issue_code_format_is_correct() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("code_format").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        // Code should be uppercase alphanumeric, length 8
        assert_eq!(request.allow_once_code.len(), 8);
        assert!(
            request
                .allow_once_code
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );

        // Hash should be sha256
        assert!(request.allow_once_full_hash.starts_with("sha256:"));

        // Command format
        assert_eq!(
            request.command,
            format!("ft approve {}", request.allow_once_code)
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_issue_expires_at_in_future() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("expires_future").await;
        let config = ApprovalConfig {
            token_expiry_secs: 3600,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        let before = now_ms();
        let request = store.issue(&input, None).await.unwrap();
        let after = now_ms();

        let expected_min = before + 3_600_000;
        let expected_max = after + 3_600_000;
        assert!(
            request.expires_at >= expected_min,
            "expires_at should be at least now + 1h"
        );
        assert!(
            request.expires_at <= expected_max,
            "expires_at should be at most now + 1h"
        );

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 4: Consume edge cases
// ===========================================================================

#[test]
fn approval_consume_wrong_code_returns_none() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("wrong_code").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let _request = store.issue(&input, None).await.unwrap();

        let consumed = store.consume("ZZZZZZZZ", &input).await.unwrap();
        assert!(
            consumed.is_none(),
            "Wrong code should not consume any token"
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_consume_empty_code_returns_none() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("empty_code").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let _request = store.issue(&input, None).await.unwrap();

        let consumed = store.consume("", &input).await.unwrap();
        assert!(
            consumed.is_none(),
            "Empty code should not consume any token"
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_consume_without_context_has_no_correlation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("no_ctx").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();
        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_some());

        let query = AuditQuery {
            action_kind: Some("approve_allow_once".to_string()),
            ..Default::default()
        };
        let audits = storage.get_audit_actions(query).await.unwrap();
        assert!(!audits.is_empty());
        let last = audits.last().unwrap();
        assert!(last.correlation_id.is_none());

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_consume_with_none_context_same_as_without() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("none_ctx").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();
        let consumed = store
            .consume_with_context(&request.allow_once_code, &input, None)
            .await
            .unwrap();
        assert!(consumed.is_some());

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 5: Token limit edge cases
// ===========================================================================

#[test]
fn approval_max_active_tokens_zero_blocks_all() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("zero_limit").await;
        let config = ApprovalConfig {
            max_active_tokens: 0,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        let result = store.issue(&input, None).await;
        assert!(
            matches!(result, Err(Error::Policy(_))),
            "max_active_tokens=0 should block all issuance"
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_max_active_tokens_for_plan_also_enforced() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_limit").await;
        let config = ApprovalConfig {
            max_active_tokens: 1,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        store
            .issue_for_plan(&input, "sha256:plan1", Some(1), None)
            .await
            .unwrap();

        let result = store
            .issue_for_plan(&input, "sha256:plan2", Some(2), None)
            .await;
        assert!(
            matches!(result, Err(Error::Policy(_))),
            "Plan-bound issue should also respect max_active_tokens"
        );

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 6: Plan metadata tests
// ===========================================================================

#[test]
fn approval_issue_for_plan_without_risk_summary_uses_default() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_no_risk").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(&input, "sha256:plan", None, None)
            .await
            .unwrap();

        // Summary should contain the action kind since no custom summary provided
        assert!(
            request.summary.contains("send_text"),
            "Default plan summary should contain action kind, got: {}",
            request.summary
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_issue_for_plan_with_risk_summary() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_risk").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(
                &input,
                "sha256:plan",
                Some(5),
                Some("HIGH RISK: deletes data".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(request.summary, "HIGH RISK: deletes data");

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_issue_for_plan_no_version() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("plan_no_version").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(&input, "sha256:abc", None, None)
            .await
            .unwrap();

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, "sha256:abc")
            .await
            .unwrap();
        assert!(consumed.is_some());

        let token = consumed.unwrap();
        assert_eq!(token.plan_version, None);
        assert_eq!(token.risk_summary, None);

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 7: attach_to_decision tests
// ===========================================================================

#[test]
fn approval_attach_to_decision_require_approval() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("attach_require").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let decision = PolicyDecision::require_approval("needs human review");
        let result = store
            .attach_to_decision(decision, &input, None)
            .await
            .unwrap();

        assert!(result.requires_approval());
        if let PolicyDecision::RequireApproval { approval, .. } = &result {
            assert!(approval.is_some(), "Approval payload should be attached");
            let ap = approval.as_ref().unwrap();
            assert!(ap.allow_once_full_hash.starts_with("sha256:"));
            assert_eq!(ap.allow_once_code.len(), 8);
        } else {
            panic!("Expected RequireApproval decision");
        }

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_attach_to_decision_allow_is_passthrough() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("attach_allow").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let decision = PolicyDecision::allow();
        let result = store
            .attach_to_decision(decision, &input, None)
            .await
            .unwrap();
        assert!(result.is_allowed());

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_attach_to_decision_deny_is_passthrough() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("attach_deny").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let decision = PolicyDecision::deny("not allowed");
        let result = store
            .attach_to_decision(decision, &input, None)
            .await
            .unwrap();
        assert!(result.is_denied());

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_attach_to_decision_with_custom_summary() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("attach_summary").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let decision = PolicyDecision::require_approval("risky");
        let result = store
            .attach_to_decision(
                decision,
                &input,
                Some("Please review this action".to_string()),
            )
            .await
            .unwrap();

        if let PolicyDecision::RequireApproval { approval, .. } = &result {
            let ap = approval.as_ref().unwrap();
            assert_eq!(ap.summary, "Please review this action");
        } else {
            panic!("Expected RequireApproval decision");
        }

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 8: Double-consume and independence tests
// ===========================================================================

#[test]
fn approval_consume_for_plan_already_consumed_returns_none() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("double_plan").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let plan_hash = "sha256:planX";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), None)
            .await
            .unwrap();

        let first = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(first.is_some());

        let second = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(
            second.is_none(),
            "Already-consumed token should not be consumable again"
        );

        cleanup(storage, &db_path).await;
    });
}

#[test]
fn approval_multiple_tokens_independent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("multi_token").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request1 = store.issue(&input, None).await.unwrap();
        let request2 = store.issue(&input, None).await.unwrap();

        assert_ne!(request1.allow_once_code, request2.allow_once_code);

        let consumed1 = store
            .consume(&request1.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed1.is_some());

        let consumed2 = store
            .consume(&request2.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed2.is_some());

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 9: Audit record verification
// ===========================================================================

#[test]
fn approval_audit_record_fields_populated() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("audit_fields").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();
        store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();

        let query = AuditQuery {
            action_kind: Some("approve_allow_once".to_string()),
            ..Default::default()
        };
        let audits = storage.get_audit_actions(query).await.unwrap();
        assert_eq!(audits.len(), 1);

        let audit = &audits[0];
        assert_eq!(audit.actor_kind, "human");
        assert_eq!(audit.action_kind, "approve_allow_once");
        assert_eq!(audit.policy_decision, "allow");
        assert_eq!(audit.result, "success");
        assert!(
            audit
                .decision_reason
                .as_deref()
                .unwrap()
                .contains("allow_once")
        );
        assert!(
            audit
                .input_summary
                .as_deref()
                .unwrap()
                .contains("send_text")
        );
        assert!(
            audit
                .verification_summary
                .as_deref()
                .unwrap()
                .contains("workspace=ws")
        );
        assert!(
            audit
                .verification_summary
                .as_deref()
                .unwrap()
                .contains("fingerprint=sha256:")
        );
        assert!(
            audit
                .verification_summary
                .as_deref()
                .unwrap()
                .contains("hash=sha256:")
        );
        assert_eq!(audit.pane_id, Some(1));
        assert_eq!(audit.domain.as_deref(), Some("local"));

        cleanup(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 10: Action kind mismatch
// ===========================================================================

#[test]
fn approval_wrong_action_kind_prevents_consumption() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("wrong_action").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input(); // SendText

        let request = store.issue(&input, None).await.unwrap();

        let wrong_action = PolicyInput::new(ActionKind::Close, ActorKind::Robot)
            .with_pane(1)
            .with_domain("local")
            .with_text_summary("echo hi")
            .with_capabilities(PaneCapabilities::prompt());

        let consumed = store
            .consume(&request.allow_once_code, &wrong_action)
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Wrong action kind should prevent consumption"
        );

        cleanup(storage, &db_path).await;
    });
}
