//! Allow-once approval tokens for RequireApproval policy decisions.

use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ApprovalConfig;
use crate::error::{Error, Result};
use crate::policy::{ApprovalRequest, PolicyDecision, PolicyInput};
use crate::storage::{ApprovalTokenRecord, AuditActionRecord, StorageHandle};

const DEFAULT_CODE_LEN: usize = 8;

/// Workspace- and action-scoped approval context
#[derive(Debug, Clone)]
pub struct ApprovalScope {
    /// Workspace identifier
    pub workspace_id: String,
    /// Action kind (send_text, workflow_run, etc.)
    pub action_kind: String,
    /// Target pane ID (if applicable)
    pub pane_id: Option<u64>,
    /// Normalized action fingerprint
    pub action_fingerprint: String,
}

/// Optional audit context for approval consumption
#[derive(Debug, Clone, Default)]
pub struct ApprovalAuditContext {
    /// Correlation identifier to attach to the audit record
    pub correlation_id: Option<String>,
    /// Decision context JSON to attach to the audit record
    pub decision_context: Option<String>,
}

impl ApprovalScope {
    /// Build a scope from policy input
    #[must_use]
    pub fn from_input(workspace_id: impl Into<String>, input: &PolicyInput) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            action_kind: input.action.as_str().to_string(),
            pane_id: input.pane_id,
            action_fingerprint: fingerprint_for_input(input),
        }
    }
}

/// Store and validate allow-once approvals
pub struct ApprovalStore<'a> {
    storage: &'a StorageHandle,
    config: ApprovalConfig,
    workspace_id: String,
}

impl<'a> ApprovalStore<'a> {
    /// Create a new approval store for a workspace
    #[must_use]
    pub fn new(
        storage: &'a StorageHandle,
        config: ApprovalConfig,
        workspace_id: impl Into<String>,
    ) -> Self {
        Self {
            storage,
            config,
            workspace_id: workspace_id.into(),
        }
    }

    /// Issue a new allow-once approval for the given policy input
    pub async fn issue(
        &self,
        input: &PolicyInput,
        summary: Option<String>,
    ) -> Result<ApprovalRequest> {
        let now = now_ms();
        let active = self
            .storage
            .count_active_approvals(&self.workspace_id, now)
            .await?;
        if active >= self.config.max_active_tokens {
            return Err(Error::Policy(format!(
                "Approval token limit reached ({active}/{})",
                self.config.max_active_tokens
            )));
        }

        let code = generate_allow_once_code(DEFAULT_CODE_LEN);
        let code_hash = hash_allow_once_code(&code);
        let fingerprint = fingerprint_for_input(input);
        let expires_at = now.saturating_add(expiry_ms(self.config.token_expiry_secs));

        let token = ApprovalTokenRecord {
            id: 0,
            code_hash: code_hash.clone(),
            created_at: now,
            expires_at,
            used_at: None,
            workspace_id: self.workspace_id.clone(),
            action_kind: input.action.as_str().to_string(),
            pane_id: input.pane_id,
            action_fingerprint: fingerprint,
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        self.storage.insert_approval_token(token).await?;

        let summary = summary.unwrap_or_else(|| summary_for_input(input));
        Ok(ApprovalRequest {
            allow_once_code: code.clone(),
            allow_once_full_hash: code_hash,
            expires_at,
            summary,
            command: format!("ft approve {code}"),
        })
    }

    /// Issue a plan-bound allow-once approval for a specific ActionPlan.
    ///
    /// The token will only be consumable when the caller presents the same
    /// `plan_hash`. This prevents TOCTOU attacks where the plan changes
    /// between approval and execution.
    pub async fn issue_for_plan(
        &self,
        input: &PolicyInput,
        plan_hash: &str,
        plan_version: Option<i32>,
        risk_summary: Option<String>,
    ) -> Result<ApprovalRequest> {
        let now = now_ms();
        let active = self
            .storage
            .count_active_approvals(&self.workspace_id, now)
            .await?;
        if active >= self.config.max_active_tokens {
            return Err(Error::Policy(format!(
                "Approval token limit reached ({active}/{})",
                self.config.max_active_tokens
            )));
        }

        let code = generate_allow_once_code(DEFAULT_CODE_LEN);
        let code_hash = hash_allow_once_code(&code);
        let fingerprint = fingerprint_for_input(input);
        let expires_at = now.saturating_add(expiry_ms(self.config.token_expiry_secs));

        let summary_text = risk_summary
            .clone()
            .unwrap_or_else(|| summary_for_input(input));

        let token = ApprovalTokenRecord {
            id: 0,
            code_hash: code_hash.clone(),
            created_at: now,
            expires_at,
            used_at: None,
            workspace_id: self.workspace_id.clone(),
            action_kind: input.action.as_str().to_string(),
            pane_id: input.pane_id,
            action_fingerprint: fingerprint,
            plan_hash: Some(plan_hash.to_string()),
            plan_version,
            risk_summary: risk_summary.clone(),
        };
        self.storage.insert_approval_token(token).await?;

        Ok(ApprovalRequest {
            allow_once_code: code.clone(),
            allow_once_full_hash: code_hash,
            expires_at,
            summary: summary_text,
            command: format!("ft approve {code}"),
        })
    }

    /// Consume a plan-bound approval, validating that the plan_hash matches.
    ///
    /// Returns `None` if the token doesn't exist, has expired, was already
    /// consumed, or the plan_hash doesn't match.
    pub async fn consume_for_plan(
        &self,
        allow_once_code: &str,
        input: &PolicyInput,
        plan_hash: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let record = self.consume(allow_once_code, input).await?;
        match record {
            Some(ref token) => {
                // If the token was issued with a plan_hash, validate it matches
                if let Some(ref stored_hash) = token.plan_hash {
                    if stored_hash != plan_hash {
                        // Plan changed since approval — reject.
                        // The token is already consumed, which is intentional:
                        // a mismatched plan_hash is a potential TOCTOU attack
                        // and the token should be invalidated.
                        return Ok(None);
                    }
                }
                Ok(record)
            }
            None => Ok(None),
        }
    }

    /// Attach an allow-once approval payload to a RequireApproval decision
    pub async fn attach_to_decision(
        &self,
        decision: PolicyDecision,
        input: &PolicyInput,
        summary: Option<String>,
    ) -> Result<PolicyDecision> {
        if decision.requires_approval() {
            let approval = self.issue(input, summary).await?;
            Ok(decision.with_approval(approval))
        } else {
            Ok(decision)
        }
    }

    /// Consume a previously issued allow-once approval
    pub async fn consume(
        &self,
        allow_once_code: &str,
        input: &PolicyInput,
    ) -> Result<Option<ApprovalTokenRecord>> {
        self.consume_with_context(allow_once_code, input, None)
            .await
    }

    /// Consume a previously issued allow-once approval with optional audit context
    pub async fn consume_with_context(
        &self,
        allow_once_code: &str,
        input: &PolicyInput,
        audit_context: Option<ApprovalAuditContext>,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let code_hash = hash_allow_once_code(allow_once_code);
        let fingerprint = fingerprint_for_input(input);
        let record = self
            .storage
            .consume_approval_token(
                &code_hash,
                &self.workspace_id,
                input.action.as_str(),
                input.pane_id,
                &fingerprint,
            )
            .await?;

        if record.is_some() {
            self.audit_approval_grant(input, &code_hash, &fingerprint, audit_context.as_ref())
                .await?;
        }

        Ok(record)
    }

    async fn audit_approval_grant(
        &self,
        input: &PolicyInput,
        code_hash: &str,
        fingerprint: &str,
        audit_context: Option<&ApprovalAuditContext>,
    ) -> Result<()> {
        let verification = format!(
            "workspace={}, fingerprint={}, hash={}",
            self.workspace_id, fingerprint, code_hash
        );

        let audit = AuditActionRecord {
            id: 0,
            ts: now_ms(),
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: audit_context.and_then(|ctx| ctx.correlation_id.clone()),
            pane_id: input.pane_id,
            domain: input.domain.clone(),
            action_kind: "approve_allow_once".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("allow_once approval granted".to_string()),
            rule_id: None,
            input_summary: Some(format!("allow_once approval for {}", input.action.as_str())),
            verification_summary: Some(verification),
            decision_context: audit_context.and_then(|ctx| ctx.decision_context.clone()),
            result: "success".to_string(),
        };

        self.storage.record_audit_action_redacted(audit).await?;
        Ok(())
    }
}

/// Compute a stable fingerprint for a policy input
#[must_use]
pub fn fingerprint_for_input(input: &PolicyInput) -> String {
    let mut canonical = String::new();
    canonical.push_str("action_kind=");
    canonical.push_str(input.action.as_str());
    canonical.push('|');
    canonical.push_str("pane_id=");
    if let Some(pane_id) = input.pane_id {
        canonical.push_str(&pane_id.to_string());
    }
    canonical.push('|');
    canonical.push_str("domain=");
    if let Some(domain) = &input.domain {
        canonical.push_str(domain);
    }
    canonical.push('|');
    canonical.push_str("text_summary=");
    if let Some(summary) = &input.text_summary {
        canonical.push_str(summary);
    }
    canonical.push('|');
    canonical.push_str("workflow_id=");
    if let Some(workflow_id) = &input.workflow_id {
        canonical.push_str(workflow_id);
    }
    canonical.push('|');
    canonical.push_str("command_text=");
    if let Some(cmd) = &input.command_text {
        canonical.push_str(cmd);
    }
    canonical.push('|');
    canonical.push_str("agent_type=");
    if let Some(agent) = &input.agent_type {
        canonical.push_str(agent);
    }
    canonical.push('|');
    canonical.push_str("pane_title=");
    if let Some(title) = &input.pane_title {
        canonical.push_str(title);
    }
    canonical.push('|');
    canonical.push_str("pane_cwd=");
    if let Some(cwd) = &input.pane_cwd {
        canonical.push_str(cwd);
    }

    format!("sha256:{}", sha256_hex(&canonical))
}

/// Hash an allow-once code using sha256
#[must_use]
pub fn hash_allow_once_code(code: &str) -> String {
    format!("sha256:{}", sha256_hex(code))
}

fn summary_for_input(input: &PolicyInput) -> String {
    use std::fmt::Write;

    let mut summary = input.action.as_str().to_string();
    if let Some(pane_id) = input.pane_id {
        let _ = write!(summary, " pane {pane_id}");
    }
    if let Some(domain) = &input.domain {
        let _ = write!(summary, " ({domain})");
    }
    if let Some(summary_text) = &input.text_summary {
        summary.push_str(": ");
        summary.push_str(summary_text);
    }
    summary
}

fn generate_allow_once_code(len: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(|b: u8| char::from(b).to_ascii_uppercase())
        .collect()
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn expiry_ms(expiry_secs: u64) -> i64 {
    let expiry_ms = expiry_secs.saturating_mul(1000);
    i64::try_from(expiry_ms).unwrap_or(i64::MAX)
}

fn now_ms() -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ActionKind, ActorKind, PaneCapabilities, PolicyInput};
    use crate::storage::{AuditQuery, PaneRecord, StorageHandle};

    fn base_input() -> PolicyInput {
        PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_domain("local")
            .with_text_summary("echo hi")
            .with_capabilities(PaneCapabilities::prompt())
    }

    // -----------------------------------------------------------------------
    // Pure helper function tests
    // -----------------------------------------------------------------------

    #[test]
    fn hash_allow_once_code_is_deterministic() {
        let hash1 = hash_allow_once_code("ABC123");
        let hash2 = hash_allow_once_code("ABC123");
        assert_eq!(hash1, hash2);
        assert!(hash1.starts_with("sha256:"));
    }

    #[test]
    fn hash_allow_once_code_different_inputs_differ() {
        let hash1 = hash_allow_once_code("ABC123");
        let hash2 = hash_allow_once_code("XYZ789");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn sha256_hex_known_value() {
        // SHA256 of empty string is well-known.
        let hash = sha256_hex("");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_is_64_hex_chars() {
        let hash = sha256_hex("hello");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_allow_once_code_correct_length() {
        let code = generate_allow_once_code(8);
        assert_eq!(code.len(), 8);
        let code16 = generate_allow_once_code(16);
        assert_eq!(code16.len(), 16);
    }

    #[test]
    fn generate_allow_once_code_all_uppercase_alphanumeric() {
        let code = generate_allow_once_code(100);
        assert!(
            code.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn generate_allow_once_code_different_each_time() {
        let a = generate_allow_once_code(16);
        let b = generate_allow_once_code(16);
        // Extremely unlikely to be equal.
        assert_ne!(a, b);
    }

    #[test]
    fn summary_for_input_basic() {
        let input = base_input();
        let s = summary_for_input(&input);
        assert!(s.contains("send_text"));
        assert!(s.contains("pane 1"));
        assert!(s.contains("(local)"));
        assert!(s.contains("echo hi"));
    }

    #[test]
    fn summary_for_input_minimal() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let s = summary_for_input(&input);
        assert!(s.contains("send_text"));
        // No pane, no domain, no summary.
        assert!(!s.contains("pane"));
        assert!(!s.contains('('));
    }

    #[test]
    fn expiry_ms_normal_value() {
        assert_eq!(expiry_ms(300), 300_000);
    }

    #[test]
    fn expiry_ms_zero() {
        assert_eq!(expiry_ms(0), 0);
    }

    #[test]
    fn expiry_ms_large_value_saturates() {
        // u64::MAX seconds → should saturate to i64::MAX.
        let result = expiry_ms(u64::MAX);
        assert_eq!(result, i64::MAX);
    }

    #[test]
    fn now_ms_returns_positive() {
        let ms = now_ms();
        assert!(ms > 0);
    }

    #[test]
    fn approval_scope_from_input_sets_fields() {
        let input = base_input();
        let scope = ApprovalScope::from_input("my-ws", &input);
        assert_eq!(scope.workspace_id, "my-ws");
        assert_eq!(scope.action_kind, "send_text");
        assert_eq!(scope.pane_id, Some(1));
        assert!(scope.action_fingerprint.starts_with("sha256:"));
    }

    #[test]
    fn approval_audit_context_default_is_none() {
        let ctx = ApprovalAuditContext::default();
        assert!(ctx.correlation_id.is_none());
        assert!(ctx.decision_context.is_none());
    }

    #[test]
    fn fingerprint_for_input_includes_all_fields() {
        let input = base_input()
            .with_command_text("rm -rf /")
            .with_workflow("wf-123");
        let fp = fingerprint_for_input(&input);
        assert!(fp.starts_with("sha256:"));

        // Changing any field should change fingerprint.
        let input2 = base_input()
            .with_command_text("rm -rf /")
            .with_workflow("wf-456");
        let fp2 = fingerprint_for_input(&input2);
        assert_ne!(fp, fp2);
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let input = base_input();
        let first = fingerprint_for_input(&input);
        let second = fingerprint_for_input(&input);
        assert_eq!(first, second);

        let different = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_domain("local")
            .with_text_summary("echo bye");
        assert_ne!(first, fingerprint_for_input(&different));
    }

    #[test]
    fn command_text_changes_fingerprint() {
        let input1 = base_input().with_command_text("echo A");
        let input2 = base_input().with_command_text("echo B");

        let fp1 = fingerprint_for_input(&input1);
        let fp2 = fingerprint_for_input(&input2);

        assert_ne!(
            fp1, fp2,
            "Fingerprint should differ when command_text changes"
        );
    }

    // -----------------------------------------------------------------------
    // NEW: Additional pure helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn sha256_hex_known_value_hello() {
        // Known SHA-256 of "hello"
        let hash = sha256_hex("hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_hex_all_lowercase() {
        let hash = sha256_hex("test");
        // All hex chars should be lowercase
        assert!(hash.chars().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn hash_allow_once_code_empty_string() {
        let hash = hash_allow_once_code("");
        assert!(hash.starts_with("sha256:"));
        // SHA-256 of empty string is well-known
        assert_eq!(
            hash,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hash_allow_once_code_length_is_consistent() {
        // "sha256:" (7 chars) + 64 hex chars = 71 chars total
        let hash = hash_allow_once_code("ABCDEFGH");
        assert_eq!(hash.len(), 71);
    }

    #[test]
    fn hash_allow_once_code_case_sensitive() {
        let lower = hash_allow_once_code("abc");
        let upper = hash_allow_once_code("ABC");
        assert_ne!(lower, upper, "Hash should be case-sensitive");
    }

    #[test]
    fn generate_allow_once_code_zero_length() {
        let code = generate_allow_once_code(0);
        assert!(code.is_empty());
    }

    #[test]
    fn generate_allow_once_code_length_one() {
        let code = generate_allow_once_code(1);
        assert_eq!(code.len(), 1);
        assert!(
            code.chars().next().unwrap().is_ascii_uppercase()
                || code.chars().next().unwrap().is_ascii_digit()
        );
    }

    #[test]
    fn generate_allow_once_code_large_length() {
        let code = generate_allow_once_code(1000);
        assert_eq!(code.len(), 1000);
        assert!(
            code.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn expiry_ms_one_second() {
        assert_eq!(expiry_ms(1), 1000);
    }

    #[test]
    fn expiry_ms_one_day() {
        assert_eq!(expiry_ms(86400), 86_400_000);
    }

    #[test]
    fn expiry_ms_boundary_near_i64_max() {
        // i64::MAX / 1000 should still fit in i64 after multiply
        let secs = (i64::MAX / 1000) as u64;
        let result = expiry_ms(secs);
        assert!(result > 0);
        // result is i64, so result <= i64::MAX is always true — just verify it's positive
    }

    #[test]
    fn expiry_ms_just_over_i64_max_saturates() {
        // Just barely overflowing u64 multiply should saturate to i64::MAX
        let secs = (i64::MAX as u64) / 1000 + 2;
        let result = expiry_ms(secs);
        // The result should still be representable
        assert!(result > 0);
    }

    #[test]
    fn now_ms_reasonable_range() {
        // Should be after 2020-01-01 and before 2100-01-01
        let ms = now_ms();
        let year_2020_ms: i64 = 1_577_836_800_000;
        let year_2100_ms: i64 = 4_102_444_800_000;
        assert!(ms > year_2020_ms, "now_ms should be after 2020");
        assert!(ms < year_2100_ms, "now_ms should be before 2100");
    }

    #[test]
    fn now_ms_monotonic() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a, "now_ms should be non-decreasing");
    }

    // -----------------------------------------------------------------------
    // NEW: ApprovalScope tests
    // -----------------------------------------------------------------------

    #[test]
    fn approval_scope_from_input_no_pane() {
        let input = PolicyInput::new(ActionKind::Spawn, ActorKind::Human);
        let scope = ApprovalScope::from_input("ws-1", &input);
        assert_eq!(scope.workspace_id, "ws-1");
        assert_eq!(scope.action_kind, "spawn");
        assert_eq!(scope.pane_id, None);
        assert!(scope.action_fingerprint.starts_with("sha256:"));
    }

    #[test]
    fn approval_scope_from_input_workflow_action() {
        let input = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Workflow)
            .with_workflow("deploy-v2")
            .with_pane(42);
        let scope = ApprovalScope::from_input("production", &input);
        assert_eq!(scope.action_kind, "workflow_run");
        assert_eq!(scope.pane_id, Some(42));
    }

    #[test]
    fn approval_scope_clone_is_independent() {
        let input = base_input();
        let scope = ApprovalScope::from_input("ws", &input);
        let cloned = scope.clone();
        assert_eq!(scope.workspace_id, cloned.workspace_id);
        assert_eq!(scope.action_kind, cloned.action_kind);
        assert_eq!(scope.pane_id, cloned.pane_id);
        assert_eq!(scope.action_fingerprint, cloned.action_fingerprint);
    }

    #[test]
    fn approval_scope_debug_impl() {
        let input = base_input();
        let scope = ApprovalScope::from_input("ws", &input);
        let debug_str = format!("{:?}", scope);
        assert!(debug_str.contains("ApprovalScope"));
        assert!(debug_str.contains("ws"));
        assert!(debug_str.contains("send_text"));
    }

    #[test]
    fn approval_scope_empty_workspace_id() {
        let input = base_input();
        let scope = ApprovalScope::from_input("", &input);
        assert_eq!(scope.workspace_id, "");
    }

    #[test]
    fn approval_scope_string_workspace_id() {
        // Test that Into<String> works with owned String
        let ws = String::from("my-workspace");
        let input = base_input();
        let scope = ApprovalScope::from_input(ws, &input);
        assert_eq!(scope.workspace_id, "my-workspace");
    }

    // -----------------------------------------------------------------------
    // NEW: ApprovalAuditContext tests
    // -----------------------------------------------------------------------

    #[test]
    fn approval_audit_context_with_both_fields() {
        let ctx = ApprovalAuditContext {
            correlation_id: Some("corr-123".to_string()),
            decision_context: Some("{\"key\":\"value\"}".to_string()),
        };
        assert_eq!(ctx.correlation_id.as_deref(), Some("corr-123"));
        assert_eq!(ctx.decision_context.as_deref(), Some("{\"key\":\"value\"}"));
    }

    #[test]
    fn approval_audit_context_clone_is_independent() {
        let ctx = ApprovalAuditContext {
            correlation_id: Some("abc".to_string()),
            decision_context: Some("def".to_string()),
        };
        let cloned = ctx.clone();
        assert_eq!(ctx.correlation_id, cloned.correlation_id);
        assert_eq!(ctx.decision_context, cloned.decision_context);
    }

    #[test]
    fn approval_audit_context_debug_impl() {
        let ctx = ApprovalAuditContext {
            correlation_id: Some("test-id".to_string()),
            decision_context: None,
        };
        let debug_str = format!("{:?}", ctx);
        assert!(debug_str.contains("ApprovalAuditContext"));
        assert!(debug_str.contains("test-id"));
    }

    #[test]
    fn approval_audit_context_only_correlation() {
        let ctx = ApprovalAuditContext {
            correlation_id: Some("only-corr".to_string()),
            decision_context: None,
        };
        assert!(ctx.correlation_id.is_some());
        assert!(ctx.decision_context.is_none());
    }

    #[test]
    fn approval_audit_context_only_decision_context() {
        let ctx = ApprovalAuditContext {
            correlation_id: None,
            decision_context: Some("{}".to_string()),
        };
        assert!(ctx.correlation_id.is_none());
        assert!(ctx.decision_context.is_some());
    }

    // -----------------------------------------------------------------------
    // NEW: Fingerprint edge case tests
    // -----------------------------------------------------------------------

    #[test]
    fn fingerprint_no_optional_fields() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let fp = fingerprint_for_input(&input);
        assert!(fp.starts_with("sha256:"));
        assert_eq!(fp.len(), 71); // "sha256:" + 64 hex
    }

    #[test]
    fn fingerprint_all_optional_fields_set() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(42)
            .with_domain("remote")
            .with_text_summary("ls -la")
            .with_workflow("wf-abc")
            .with_command_text("ls -la")
            .with_agent_type("claude")
            .with_pane_title("bash")
            .with_pane_cwd("/home/user");
        let fp = fingerprint_for_input(&input);
        assert!(fp.starts_with("sha256:"));
        assert_eq!(fp.len(), 71);
    }

    #[test]
    fn fingerprint_different_action_kinds() {
        let send = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let spawn = PolicyInput::new(ActionKind::Spawn, ActorKind::Robot);
        let close = PolicyInput::new(ActionKind::Close, ActorKind::Robot);

        let fp_send = fingerprint_for_input(&send);
        let fp_spawn = fingerprint_for_input(&spawn);
        let fp_close = fingerprint_for_input(&close);

        assert_ne!(fp_send, fp_spawn);
        assert_ne!(fp_send, fp_close);
        assert_ne!(fp_spawn, fp_close);
    }

    #[test]
    fn fingerprint_pane_id_changes_hash() {
        let input1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(1);
        let input2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(2);
        let input_none = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);

        assert_ne!(
            fingerprint_for_input(&input1),
            fingerprint_for_input(&input2)
        );
        assert_ne!(
            fingerprint_for_input(&input1),
            fingerprint_for_input(&input_none)
        );
    }

    #[test]
    fn fingerprint_domain_changes_hash() {
        let a = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_domain("local");
        let b = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_domain("remote");

        assert_ne!(fingerprint_for_input(&a), fingerprint_for_input(&b));
    }

    #[test]
    fn fingerprint_agent_type_changes_hash() {
        let a = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_agent_type("claude");
        let b = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_agent_type("cursor");

        assert_ne!(fingerprint_for_input(&a), fingerprint_for_input(&b));
    }

    #[test]
    fn fingerprint_pane_title_changes_hash() {
        let a = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_title("bash");
        let b = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_title("zsh");

        assert_ne!(fingerprint_for_input(&a), fingerprint_for_input(&b));
    }

    #[test]
    fn fingerprint_pane_cwd_changes_hash() {
        let a = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_cwd("/home");
        let b = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_cwd("/tmp");

        assert_ne!(fingerprint_for_input(&a), fingerprint_for_input(&b));
    }

    #[test]
    fn fingerprint_actor_kind_does_not_change_hash() {
        // Actor kind is NOT part of the fingerprint canonical string
        let robot = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(1);
        let human = PolicyInput::new(ActionKind::SendText, ActorKind::Human).with_pane(1);

        assert_eq!(
            fingerprint_for_input(&robot),
            fingerprint_for_input(&human),
            "Actor kind should not affect fingerprint"
        );
    }

    // -----------------------------------------------------------------------
    // NEW: summary_for_input edge case tests
    // -----------------------------------------------------------------------

    #[test]
    fn summary_for_input_with_domain_only() {
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot).with_domain("remote");
        let s = summary_for_input(&input);
        assert_eq!(s, "close (remote)");
    }

    #[test]
    fn summary_for_input_with_pane_only() {
        let input = PolicyInput::new(ActionKind::Spawn, ActorKind::Robot).with_pane(99);
        let s = summary_for_input(&input);
        assert_eq!(s, "spawn pane 99");
    }

    #[test]
    fn summary_for_input_with_text_summary_only() {
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_text_summary("git push");
        let s = summary_for_input(&input);
        assert_eq!(s, "send_text: git push");
    }

    #[test]
    fn summary_for_input_all_action_kinds() {
        // Exercise summary generation for many different action kinds
        let actions = [
            (ActionKind::SendText, "send_text"),
            (ActionKind::SendCtrlC, "send_ctrl_c"),
            (ActionKind::Spawn, "spawn"),
            (ActionKind::Close, "close"),
            (ActionKind::WorkflowRun, "workflow_run"),
            (ActionKind::BrowserAuth, "browser_auth"),
        ];
        for (kind, expected_str) in &actions {
            let input = PolicyInput::new(*kind, ActorKind::Robot);
            let s = summary_for_input(&input);
            assert!(
                s.starts_with(expected_str),
                "Expected summary to start with '{}', got '{}'",
                expected_str,
                s
            );
        }
    }

    #[test]
    fn summary_for_input_full_fields_ordering() {
        // pane appears before domain, domain before text_summary
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(5)
            .with_domain("staging")
            .with_text_summary("deploy");
        let s = summary_for_input(&input);
        let pane_pos = s.find("pane 5").unwrap();
        let domain_pos = s.find("(staging)").unwrap();
        let text_pos = s.find("deploy").unwrap();
        assert!(pane_pos < domain_pos, "pane should appear before domain");
        assert!(
            domain_pos < text_pos,
            "domain should appear before text_summary"
        );
    }

    // -----------------------------------------------------------------------
    // NEW: DEFAULT_CODE_LEN constant test
    // -----------------------------------------------------------------------

    #[test]
    fn default_code_len_is_eight() {
        assert_eq!(DEFAULT_CODE_LEN, 8);
    }

    // -----------------------------------------------------------------------
    // Async integration tests (existing)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn issue_and_consume_allow_once() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_approval_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();

        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let request = store.issue(&input, None).await.unwrap();

        assert!(request.allow_once_full_hash.starts_with("sha256:"));
        assert_eq!(
            request.command,
            format!("ft approve {}", request.allow_once_code)
        );

        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_some());

        let second = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(second.is_none());

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    #[tokio::test]
    async fn scope_mismatch_does_not_consume() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_approval_scope_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();

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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    #[tokio::test]
    async fn max_active_tokens_enforced() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_approval_limit_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();

        let config = ApprovalConfig {
            max_active_tokens: 1,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();
        store.issue(&input, None).await.unwrap();

        let second = store.issue(&input, None).await;
        assert!(matches!(second, Err(Error::Policy(_))));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    #[tokio::test]
    async fn expired_token_cannot_be_consumed() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_approval_expiry_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();

        // Create store with 0 second expiry (tokens expire immediately)
        let config = ApprovalConfig {
            token_expiry_secs: 0,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        // Issue a token (will have expires_at = now)
        let request = store.issue(&input, None).await.unwrap();

        // Wait a tiny bit to ensure time has passed
        crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

        // Try to consume - should fail because token has expired
        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_none(), "Expired token should not be consumable");

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    #[tokio::test]
    async fn consume_with_context_records_correlation() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!(
            "wa_test_approval_context_{}.db",
            std::process::id()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();

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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    #[tokio::test]
    async fn different_action_fingerprint_prevents_consumption() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!(
            "wa_test_approval_fingerprint_{}.db",
            std::process::id()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();

        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let request = store.issue(&input, None).await.unwrap();

        // Try to consume with same pane but different text summary (different fingerprint)
        let different_text = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_domain("local")
            .with_text_summary("echo different") // Different text
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    /// Helper to create a test storage handle with a pane registered
    async fn setup_test_storage(suffix: &str) -> (StorageHandle, std::path::PathBuf) {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!(
            "wa_test_plan_hash_{suffix}_{}.db",
            std::process::id()
        ));
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();
        let pane = PaneRecord {
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
        };
        storage.upsert_pane(pane).await.unwrap();
        (storage, db_path)
    }

    async fn cleanup_storage(storage: StorageHandle, db_path: &std::path::Path) {
        storage.shutdown().await.unwrap();
        let db_path_str = db_path.to_string_lossy();
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    }

    #[tokio::test]
    async fn issue_and_consume_plan_bound_approval() {
        let (storage, db_path) = setup_test_storage("issue_consume").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let plan_hash = "sha256:plan123abc";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), Some("Low risk".to_string()))
            .await
            .unwrap();

        assert!(request.allow_once_full_hash.starts_with("sha256:"));

        // Consume with matching plan_hash succeeds
        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(consumed.is_some(), "Matching plan_hash should succeed");

        let token = consumed.unwrap();
        assert_eq!(token.plan_hash.as_deref(), Some(plan_hash));
        assert_eq!(token.plan_version, Some(1));
        assert_eq!(token.risk_summary.as_deref(), Some("Low risk"));

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn plan_hash_mismatch_rejects_consumption() {
        let (storage, db_path) = setup_test_storage("mismatch").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let plan_hash = "sha256:originalplan";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), None)
            .await
            .unwrap();

        // Consume with different plan_hash is rejected
        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, "sha256:differentplan")
            .await
            .unwrap();
        assert!(consumed.is_none(), "Mismatched plan_hash must be rejected");

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn plan_bound_token_expired_cannot_consume() {
        let (storage, db_path) = setup_test_storage("expired").await;
        let config = ApprovalConfig {
            token_expiry_secs: 0, // Expire immediately
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();
        let plan_hash = "sha256:expiredplan";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), None)
            .await
            .unwrap();

        // Wait for expiry
        crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Expired plan-bound token should not be consumable"
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn plan_bound_scope_violation_rejected() {
        let (storage, db_path) = setup_test_storage("scope").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let plan_hash = "sha256:scopedplan";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), None)
            .await
            .unwrap();

        // Wrong pane = scope violation
        let wrong_pane = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(99)
            .with_domain("local")
            .with_text_summary("echo hi")
            .with_capabilities(PaneCapabilities::prompt());

        let consumed = store
            .consume_for_plan(&request.allow_once_code, &wrong_pane, plan_hash)
            .await
            .unwrap();
        assert!(
            consumed.is_none(),
            "Wrong pane scope should reject even with correct plan_hash"
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn non_plan_bound_token_works_with_consume_for_plan() {
        let (storage, db_path) = setup_test_storage("noplan").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        // Issue without plan binding
        let request = store.issue(&input, None).await.unwrap();

        // consume_for_plan should still work (token has no plan_hash to validate)
        let consumed = store
            .consume_for_plan(&request.allow_once_code, &input, "sha256:anyplan")
            .await
            .unwrap();
        assert!(
            consumed.is_some(),
            "Non-plan-bound token should not reject based on plan_hash"
        );

        cleanup_storage(storage, &db_path).await;
    }

    // -----------------------------------------------------------------------
    // NEW: Async integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn issue_with_custom_summary() {
        let (storage, db_path) = setup_test_storage("custom_summary").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue(&input, Some("Custom approval summary".to_string()))
            .await
            .unwrap();

        assert_eq!(request.summary, "Custom approval summary");
        assert!(request.allow_once_code.len() == DEFAULT_CODE_LEN);

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn issue_generates_default_summary() {
        let (storage, db_path) = setup_test_storage("default_summary").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        // Default summary should match summary_for_input
        let expected = summary_for_input(&input);
        assert_eq!(request.summary, expected);

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn issue_code_format_is_correct() {
        let (storage, db_path) = setup_test_storage("code_format").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();

        // Code should be uppercase alphanumeric, length DEFAULT_CODE_LEN
        assert_eq!(request.allow_once_code.len(), DEFAULT_CODE_LEN);
        assert!(
            request
                .allow_once_code
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );

        // Hash should be sha256 of the code
        let expected_hash = hash_allow_once_code(&request.allow_once_code);
        assert_eq!(request.allow_once_full_hash, expected_hash);

        // Command format
        assert_eq!(
            request.command,
            format!("ft approve {}", request.allow_once_code)
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn issue_expires_at_in_future() {
        let (storage, db_path) = setup_test_storage("expires_future").await;
        let config = ApprovalConfig {
            token_expiry_secs: 3600, // 1 hour
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        let before = now_ms();
        let request = store.issue(&input, None).await.unwrap();
        let after = now_ms();

        // expires_at should be approximately now + 3600*1000
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn consume_wrong_code_returns_none() {
        let (storage, db_path) = setup_test_storage("wrong_code").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let _request = store.issue(&input, None).await.unwrap();

        // Try a completely wrong code
        let consumed = store.consume("ZZZZZZZZ", &input).await.unwrap();
        assert!(
            consumed.is_none(),
            "Wrong code should not consume any token"
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn consume_empty_code_returns_none() {
        let (storage, db_path) = setup_test_storage("empty_code").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let _request = store.issue(&input, None).await.unwrap();

        let consumed = store.consume("", &input).await.unwrap();
        assert!(
            consumed.is_none(),
            "Empty code should not consume any token"
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn consume_without_context_has_no_correlation() {
        let (storage, db_path) = setup_test_storage("no_ctx").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();
        let consumed = store
            .consume(&request.allow_once_code, &input)
            .await
            .unwrap();
        assert!(consumed.is_some());

        // Audit should exist but without correlation_id
        let query = AuditQuery {
            action_kind: Some("approve_allow_once".to_string()),
            ..Default::default()
        };
        let audits = storage.get_audit_actions(query).await.unwrap();
        assert!(!audits.is_empty());
        // The audit from consume (no context) should have no correlation_id
        let last = audits.last().unwrap();
        assert!(last.correlation_id.is_none());

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn consume_with_none_context_same_as_without() {
        let (storage, db_path) = setup_test_storage("none_ctx").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store.issue(&input, None).await.unwrap();
        let consumed = store
            .consume_with_context(&request.allow_once_code, &input, None)
            .await
            .unwrap();
        assert!(consumed.is_some());

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn max_active_tokens_zero_blocks_all() {
        let (storage, db_path) = setup_test_storage("zero_limit").await;
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn max_active_tokens_for_plan_also_enforced() {
        let (storage, db_path) = setup_test_storage("plan_limit").await;
        let config = ApprovalConfig {
            max_active_tokens: 1,
            ..ApprovalConfig::default()
        };
        let store = ApprovalStore::new(&storage, config, "ws");
        let input = base_input();

        // First plan-bound issue succeeds
        store
            .issue_for_plan(&input, "sha256:plan1", Some(1), None)
            .await
            .unwrap();

        // Second should fail due to limit
        let result = store
            .issue_for_plan(&input, "sha256:plan2", Some(2), None)
            .await;
        assert!(
            matches!(result, Err(Error::Policy(_))),
            "Plan-bound issue should also respect max_active_tokens"
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn issue_for_plan_without_risk_summary_uses_default() {
        let (storage, db_path) = setup_test_storage("plan_no_risk").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request = store
            .issue_for_plan(&input, "sha256:plan", None, None)
            .await
            .unwrap();

        // Summary should be the default from summary_for_input
        let expected = summary_for_input(&input);
        assert_eq!(request.summary, expected);

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn issue_for_plan_with_risk_summary() {
        let (storage, db_path) = setup_test_storage("plan_risk").await;
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn attach_to_decision_require_approval() {
        let (storage, db_path) = setup_test_storage("attach_require").await;
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
            assert_eq!(ap.allow_once_code.len(), DEFAULT_CODE_LEN);
        } else {
            panic!("Expected RequireApproval decision");
        }

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn attach_to_decision_allow_is_passthrough() {
        let (storage, db_path) = setup_test_storage("attach_allow").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let decision = PolicyDecision::allow();
        let result = store
            .attach_to_decision(decision, &input, None)
            .await
            .unwrap();

        assert!(result.is_allowed());

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn attach_to_decision_deny_is_passthrough() {
        let (storage, db_path) = setup_test_storage("attach_deny").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let decision = PolicyDecision::deny("not allowed");
        let result = store
            .attach_to_decision(decision, &input, None)
            .await
            .unwrap();

        assert!(result.is_denied());

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn attach_to_decision_with_custom_summary() {
        let (storage, db_path) = setup_test_storage("attach_summary").await;
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn consume_for_plan_already_consumed_returns_none() {
        let (storage, db_path) = setup_test_storage("double_consume_plan").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();
        let plan_hash = "sha256:planX";

        let request = store
            .issue_for_plan(&input, plan_hash, Some(1), None)
            .await
            .unwrap();

        // First consumption succeeds
        let first = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(first.is_some());

        // Second consumption fails (already consumed)
        let second = store
            .consume_for_plan(&request.allow_once_code, &input, plan_hash)
            .await
            .unwrap();
        assert!(
            second.is_none(),
            "Already-consumed token should not be consumable again"
        );

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn multiple_tokens_independent() {
        let (storage, db_path) = setup_test_storage("multi_token").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input();

        let request1 = store.issue(&input, None).await.unwrap();
        let request2 = store.issue(&input, None).await.unwrap();

        // Codes should be different
        assert_ne!(request1.allow_once_code, request2.allow_once_code);

        // Consuming one should not affect the other
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn issue_for_plan_no_version() {
        let (storage, db_path) = setup_test_storage("plan_no_version").await;
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn audit_record_fields_populated() {
        let (storage, db_path) = setup_test_storage("audit_fields").await;
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

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn wrong_action_kind_prevents_consumption() {
        let (storage, db_path) = setup_test_storage("wrong_action").await;
        let store = ApprovalStore::new(&storage, ApprovalConfig::default(), "ws");
        let input = base_input(); // SendText

        let request = store.issue(&input, None).await.unwrap();

        // Try consuming with a different action kind
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

        cleanup_storage(storage, &db_path).await;
    }
}
