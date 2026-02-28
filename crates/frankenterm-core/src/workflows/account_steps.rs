//! Account selection and device auth workflow step helpers.
//!
//! Provides functions for refreshing account usage from caut, selecting
//! the best account for failover, and parsing device authentication codes.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Account Selection Step (wa-nu4.1.3.4)
// ============================================================================

/// Result of the account selection workflow step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountSelectionStepResult {
    /// The selected account (if any eligible accounts exist)
    pub selected: Option<crate::accounts::AccountRecord>,
    /// Full explanation of the selection decision
    pub explanation: crate::accounts::SelectionExplanation,
    /// Quota availability advisory for downstream scheduling/launch logic
    pub quota_advisory: crate::accounts::AccountQuotaAdvisory,
    /// Number of accounts refreshed from caut
    pub accounts_refreshed: usize,
}

/// Errors that can occur during account selection step.
#[derive(Debug)]
pub enum AccountSelectionStepError {
    /// caut command failed
    Caut(crate::caut::CautError),
    /// Storage operation failed
    Storage(String),
}

impl std::fmt::Display for AccountSelectionStepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Caut(e) => write!(f, "caut error: {e}"),
            Self::Storage(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for AccountSelectionStepError {}

/// Refresh account usage from caut and select the best account for failover.
///
/// This function:
/// 1. Calls `caut refresh --service openai --format json` to get latest usage
/// 2. Updates the accounts mirror in the database
/// 3. Selects the best account according to the configured policy
///
/// # Arguments
/// * `caut_client` - The caut CLI wrapper client
/// * `storage` - Storage handle for persisting accounts
/// * `config` - Account selection configuration (threshold, etc.)
///
/// # Returns
/// An `AccountSelectionStepResult` with the selected account and explanation.
///
/// # Note
/// This function does NOT update `last_used_at` - that should only happen
/// after the failover is actually successful.
#[allow(dead_code)]
pub(crate) async fn refresh_and_select_account(
    caut_client: &crate::caut::CautClient,
    storage: &StorageHandle,
    config: &crate::accounts::AccountSelectionConfig,
) -> Result<AccountSelectionStepResult, AccountSelectionStepError> {
    // Step 1: Refresh usage from caut
    let refresh_result = caut_client
        .refresh(crate::caut::CautService::OpenAI)
        .await
        .map_err(AccountSelectionStepError::Caut)?;

    // Step 2: Update accounts mirror in DB
    let accounts_refreshed = refresh_result.accounts.len();
    let now_ms = crate::accounts::now_ms();
    persist_caut_refresh_accounts(
        storage,
        crate::caut::CautService::OpenAI,
        &refresh_result,
        now_ms,
    )
    .await?;

    // Step 3: Select best account
    let selection = storage
        .select_account("openai", config)
        .await
        .map_err(|e| AccountSelectionStepError::Storage(e.to_string()))?;
    let quota_advisory = crate::accounts::build_quota_advisory(
        &selection,
        crate::accounts::DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
    );

    Ok(AccountSelectionStepResult {
        selected: selection.selected,
        explanation: selection.explanation,
        quota_advisory,
        accounts_refreshed,
    })
}

pub(crate) async fn persist_caut_refresh_accounts(
    storage: &StorageHandle,
    service: crate::caut::CautService,
    refresh: &crate::caut::CautRefresh,
    now_ms: i64,
) -> Result<usize, AccountSelectionStepError> {
    fn extra_f64(
        extra: &std::collections::HashMap<String, serde_json::Value>,
        key: &str,
    ) -> Option<f64> {
        extra.get(key).and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
    }

    fn estimated_cost_usd(usage: &crate::caut::CautAccountUsage) -> Option<f64> {
        // Best-effort: caut schemas drift; accept a few common spellings.
        for key in [
            "estimated_cost_usd",
            "estimatedCostUsd",
            "estimated_cost",
            "estimatedCost",
            "cost_usd",
            "costUsd",
        ] {
            if let Some(v) = extra_f64(&usage.extra, key) {
                return Some(v);
            }
        }
        None
    }

    let mut metrics: Vec<crate::storage::UsageMetricRecord> = Vec::new();

    for usage in &refresh.accounts {
        let record = crate::accounts::AccountRecord::from_caut(usage, service, now_ms);
        let account_id = record.account_id.clone();

        storage
            .upsert_account(record)
            .await
            .map_err(|e| AccountSelectionStepError::Storage(e.to_string()))?;

        // Usage metric is intentionally "agent-agnostic" here: it's account pool state,
        // not a single agent session. The service is kept in metadata.
        metrics.push(crate::storage::UsageMetricRecord {
            id: 0,
            timestamp: now_ms,
            metric_type: crate::storage::MetricType::TokenUsage,
            pane_id: None,
            agent_type: None,
            account_id: Some(account_id),
            workflow_id: None,
            count: None,
            amount: estimated_cost_usd(usage),
            tokens: usage.tokens_used.and_then(|v| i64::try_from(v).ok()),
            metadata: Some(
                serde_json::json!({
                    "source": "caut.refresh",
                    "service": service.as_str(),
                    "percent_remaining": usage.percent_remaining,
                    "reset_at": usage.reset_at,
                    "tokens_used": usage.tokens_used,
                    "tokens_remaining": usage.tokens_remaining,
                    "tokens_limit": usage.tokens_limit,
                })
                .to_string(),
            ),
            created_at: now_ms,
        });
    }

    // Best-effort: avoid breaking account selection due to metrics storage failure.
    if !metrics.is_empty() {
        if let Err(err) = storage.record_usage_metrics_batch(metrics).await {
            tracing::warn!(error = %err, "Failed to record caut refresh usage metrics");
        }
    }

    Ok(refresh.accounts.len())
}

/// Mark an account as used (update `last_used_at`) after successful failover.
///
/// This should only be called after the failover workflow completes successfully.
#[allow(dead_code)]
pub(crate) async fn mark_account_used(
    storage: &StorageHandle,
    service: &str,
    account_id: &str,
) -> Result<(), String> {
    // Get current account record
    let account = storage
        .get_account(service, account_id)
        .await
        .map_err(|e| format!("Failed to get account: {e}"))?
        .ok_or_else(|| format!("Account not found: {service}/{account_id}"))?;

    // Update last_used_at
    let now_ms = crate::accounts::now_ms();
    let updated = crate::accounts::AccountRecord {
        last_used_at: Some(now_ms),
        updated_at: now_ms,
        ..account
    };

    storage
        .upsert_account(updated)
        .await
        .map_err(|e| format!("Failed to update account: {e}"))?;

    Ok(())
}

// ============================================================================
// Device Auth Step (wa-nu4.1.3.5)
// ============================================================================

/// Device code extracted from Codex device-auth login prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    /// The device code (e.g., "ABCD-1234" or "ABCD-12345")
    pub code: String,
    /// The URL to visit for authentication (if present)
    pub url: Option<String>,
}

/// Structured error for device code parsing.
#[derive(Debug, Clone)]
pub struct DeviceCodeParseError {
    /// What was expected
    pub expected: &'static str,
    /// Hash of the tail (for safe diagnostics)
    pub tail_hash: u64,
    /// Length of the tail
    pub tail_len: usize,
}

impl std::fmt::Display for DeviceCodeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Device code not found (expected: {}, tail_hash={:016x}, tail_len={})",
            self.expected, self.tail_hash, self.tail_len
        )
    }
}

impl std::error::Error for DeviceCodeParseError {}

/// Regex for device codes: 4+ alphanumeric, dash, 4+ alphanumeric
#[allow(dead_code)]
pub(super) static DEVICE_CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:code|enter)[\s:]+([A-Z0-9]{4,}-[A-Z0-9]{4,})").expect("device code regex")
});

/// Regex for authentication URL
#[allow(dead_code)]
pub(super) static DEVICE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://[^\s]+(?:device|auth|activate)[^\s]*)").expect("device url regex")
});

/// Parse device code from pane tail text.
///
/// Looks for patterns like:
/// - "Enter code: ABCD-1234"
/// - "Your code is ABCD-12345"
/// - "code: WXYZ-5678"
#[allow(dead_code)]
pub(crate) fn parse_device_code(tail: &str) -> Result<DeviceCode, DeviceCodeParseError> {
    let tail_hash = stable_hash(tail.as_bytes());
    let tail_len = tail.len();

    // Try to find the device code
    let code = DEVICE_CODE_RE
        .captures(tail)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_uppercase());

    // Try to find the URL (optional)
    let url = DEVICE_URL_RE.find(tail).map(|m| m.as_str().to_string());

    match code {
        Some(code) => Ok(DeviceCode { code, url }),
        None => Err(DeviceCodeParseError {
            expected: "device code pattern like 'code: XXXX-YYYY'",
            tail_hash,
            tail_len,
        }),
    }
}

/// Validate a device code format.
///
/// Returns true if the code matches the expected pattern (4+ chars, dash, 4+ chars).
#[allow(dead_code)]
pub(crate) fn validate_device_code(code: &str) -> bool {
    let parts: Vec<&str> = code.split('-').collect();
    if parts.len() != 2 {
        return false;
    }
    let first_valid = parts[0].len() >= 4 && parts[0].chars().all(|c| c.is_ascii_alphanumeric());
    let second_valid = parts[1].len() >= 4 && parts[1].chars().all(|c| c.is_ascii_alphanumeric());
    first_valid && second_valid
}

/// The command to send to initiate device auth login.
pub const DEVICE_AUTH_LOGIN_COMMAND: &str = "cod login --device-auth\n";
