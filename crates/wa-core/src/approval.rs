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
            command: format!("wa approve {code}"),
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
            command: format!("wa approve {code}"),
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
            format!("wa approve {}", request.allow_once_code)
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
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

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
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

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
}
