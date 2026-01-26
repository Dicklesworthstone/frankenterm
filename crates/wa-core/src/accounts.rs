//! Account management and selection policy.
//!
//! This module provides:
//! - Account records mirroring caut usage data
//! - Selection policy (percent_remaining primary, LRU tie-break)
//! - Explainability for selection decisions
//!
//! # Selection Policy
//!
//! 1. Primary: highest `percent_remaining`
//! 2. Filter: exclude accounts below threshold
//! 3. Tie-breaker: least-recently-used (`last_used_at` oldest wins)
//!
//! # Design
//!
//! The accounts table mirrors caut usage data. Selection is deterministic
//! and explainable: every selection includes a log of which accounts were
//! considered, filtered, and why.

use crate::caut::{CautAccountUsage, CautService};
use serde::{Deserialize, Serialize};

/// Account record for the accounts table.
///
/// This mirrors caut usage data and adds wa-specific tracking fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    /// Database record ID (auto-assigned, 0 for new records)
    pub id: i64,
    /// Stable account identifier (from caut, or hash of email)
    pub account_id: String,
    /// Service (e.g., "openai")
    pub service: String,
    /// Display name for the account
    pub name: Option<String>,
    /// Percentage of usage remaining (0.0-100.0)
    pub percent_remaining: f64,
    /// When the usage quota resets (ISO8601 or epoch ms as string)
    pub reset_at: Option<String>,
    /// Tokens used in current period
    pub tokens_used: Option<i64>,
    /// Tokens remaining in current period
    pub tokens_remaining: Option<i64>,
    /// Total token limit for the period
    pub tokens_limit: Option<i64>,
    /// When this record was last refreshed from caut (epoch ms)
    pub last_refreshed_at: i64,
    /// When this account was last used for failover (epoch ms)
    pub last_used_at: Option<i64>,
    /// Created timestamp (epoch ms)
    pub created_at: i64,
    /// Updated timestamp (epoch ms)
    pub updated_at: i64,
}

impl AccountRecord {
    /// Create a new account record from caut usage data.
    #[must_use]
    pub fn from_caut(usage: &CautAccountUsage, service: CautService, now_ms: i64) -> Self {
        let account_id = usage
            .id
            .clone()
            .or_else(|| usage.name.clone())
            .unwrap_or_else(|| format!("unknown-{now_ms}"));

        Self {
            id: 0,
            account_id,
            service: service.as_str().to_string(),
            name: usage.name.clone(),
            percent_remaining: usage.percent_remaining.unwrap_or(0.0),
            reset_at: usage.reset_at.clone(),
            tokens_used: usage.tokens_used.map(|v| v as i64),
            tokens_remaining: usage.tokens_remaining.map(|v| v as i64),
            tokens_limit: usage.tokens_limit.map(|v| v as i64),
            last_refreshed_at: now_ms,
            last_used_at: None,
            created_at: now_ms,
            updated_at: now_ms,
        }
    }

    /// Returns true if the account is above the given threshold.
    #[must_use]
    pub fn is_above_threshold(&self, threshold: f64) -> bool {
        self.percent_remaining >= threshold
    }
}

/// Configuration for account selection policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSelectionConfig {
    /// Minimum percent_remaining to consider an account (default: 5.0)
    pub threshold_percent: f64,
}

impl Default for AccountSelectionConfig {
    fn default() -> Self {
        Self {
            threshold_percent: 5.0,
        }
    }
}

/// Result of account selection, including explainability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSelectionResult {
    /// The selected account (None if no eligible accounts)
    pub selected: Option<AccountRecord>,
    /// Explanation of the selection decision
    pub explanation: SelectionExplanation,
}

/// Detailed explanation of the selection decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionExplanation {
    /// Total accounts considered
    pub total_considered: usize,
    /// Accounts filtered out (below threshold)
    pub filtered_out: Vec<FilteredAccount>,
    /// Candidates remaining after filtering
    pub candidates: Vec<CandidateAccount>,
    /// Why the selected account was chosen
    pub selection_reason: String,
}

/// An account that was filtered out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredAccount {
    /// Account identifier
    pub account_id: String,
    /// Display name
    pub name: Option<String>,
    /// Percent remaining when filtered
    pub percent_remaining: f64,
    /// Why it was filtered
    pub reason: String,
}

/// A candidate account considered for selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateAccount {
    /// Account identifier
    pub account_id: String,
    /// Display name
    pub name: Option<String>,
    /// Percent remaining
    pub percent_remaining: f64,
    /// Last used timestamp (epoch ms, None = never used)
    pub last_used_at: Option<i64>,
}

/// Select the best account from a list according to policy.
///
/// # Policy
///
/// 1. Filter: exclude accounts below `config.threshold_percent`
/// 2. Primary: highest `percent_remaining`
/// 3. Tie-breaker: least-recently-used (`last_used_at` oldest wins, None = never used = oldest)
///
/// # Returns
///
/// An `AccountSelectionResult` with the selected account and full explanation.
#[must_use]
pub fn select_account(
    accounts: &[AccountRecord],
    config: &AccountSelectionConfig,
) -> AccountSelectionResult {
    let total_considered = accounts.len();

    // Filter out accounts below threshold
    let mut filtered_out = Vec::new();
    let mut candidates: Vec<&AccountRecord> = Vec::new();

    for account in accounts {
        if account.percent_remaining < config.threshold_percent {
            filtered_out.push(FilteredAccount {
                account_id: account.account_id.clone(),
                name: account.name.clone(),
                percent_remaining: account.percent_remaining,
                reason: format!(
                    "Below threshold ({:.1}% < {:.1}%)",
                    account.percent_remaining, config.threshold_percent
                ),
            });
        } else {
            candidates.push(account);
        }
    }

    // Build candidate list for explanation
    let candidate_explanations: Vec<CandidateAccount> = candidates
        .iter()
        .map(|a| CandidateAccount {
            account_id: a.account_id.clone(),
            name: a.name.clone(),
            percent_remaining: a.percent_remaining,
            last_used_at: a.last_used_at,
        })
        .collect();

    // No eligible candidates
    if candidates.is_empty() {
        return AccountSelectionResult {
            selected: None,
            explanation: SelectionExplanation {
                total_considered,
                filtered_out,
                candidates: candidate_explanations,
                selection_reason: if total_considered == 0 {
                    "No accounts available".to_string()
                } else {
                    format!(
                        "All {} accounts below threshold ({:.1}%)",
                        total_considered, config.threshold_percent
                    )
                },
            },
        };
    }

    // Sort candidates: highest percent_remaining first, then oldest last_used_at
    // For last_used_at: None (never used) is treated as epoch 0 (oldest)
    candidates.sort_by(|a, b| {
        // Primary: highest percent_remaining first
        let pct_cmp = b
            .percent_remaining
            .partial_cmp(&a.percent_remaining)
            .unwrap_or(std::cmp::Ordering::Equal);

        if pct_cmp != std::cmp::Ordering::Equal {
            return pct_cmp;
        }

        // Tie-breaker: oldest last_used_at (None = 0 = never used = oldest)
        let a_used = a.last_used_at.unwrap_or(0);
        let b_used = b.last_used_at.unwrap_or(0);
        a_used.cmp(&b_used)
    });

    let selected = candidates[0].clone();

    // Determine selection reason
    let selection_reason = if candidates.len() == 1 {
        "Only eligible account".to_string()
    } else {
        let runner_up = candidates.get(1);
        if let Some(runner) = runner_up {
            if (selected.percent_remaining - runner.percent_remaining).abs() < 0.001 {
                // Tie-break applied
                let selected_used = selected.last_used_at.unwrap_or(0);
                let runner_used = runner.last_used_at.unwrap_or(0);
                format!(
                    "Tie-break: least recently used (last_used: {} vs {})",
                    if selected_used == 0 {
                        "never".to_string()
                    } else {
                        selected_used.to_string()
                    },
                    if runner_used == 0 {
                        "never".to_string()
                    } else {
                        runner_used.to_string()
                    }
                )
            } else {
                format!(
                    "Highest percent_remaining ({:.1}% vs {:.1}%)",
                    selected.percent_remaining, runner.percent_remaining
                )
            }
        } else {
            format!(
                "Highest percent_remaining ({:.1}%)",
                selected.percent_remaining
            )
        }
    };

    AccountSelectionResult {
        selected: Some(selected),
        explanation: SelectionExplanation {
            total_considered,
            filtered_out,
            candidates: candidate_explanations,
            selection_reason,
        },
    }
}

/// Convenience function to get the current time in milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_account(id: &str, pct: f64, last_used: Option<i64>) -> AccountRecord {
        AccountRecord {
            id: 0,
            account_id: id.to_string(),
            service: "openai".to_string(),
            name: Some(id.to_string()),
            percent_remaining: pct,
            reset_at: None,
            tokens_used: None,
            tokens_remaining: None,
            tokens_limit: None,
            last_refreshed_at: 1000,
            last_used_at: last_used,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn select_highest_percent_remaining() {
        let accounts = vec![
            make_account("alpha", 30.0, None),
            make_account("beta", 50.0, None),
            make_account("gamma", 20.0, None),
        ];
        let config = AccountSelectionConfig::default();

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_some());
        assert_eq!(result.selected.unwrap().account_id, "beta");
        assert!(result
            .explanation
            .selection_reason
            .contains("Highest percent_remaining"));
    }

    #[test]
    fn filter_below_threshold() {
        let accounts = vec![
            make_account("alpha", 3.0, None),  // below 5%
            make_account("beta", 50.0, None),  // above
            make_account("gamma", 4.5, None),  // below 5%
        ];
        let config = AccountSelectionConfig {
            threshold_percent: 5.0,
        };

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_some());
        assert_eq!(result.selected.unwrap().account_id, "beta");
        assert_eq!(result.explanation.filtered_out.len(), 2);
        assert_eq!(result.explanation.candidates.len(), 1);
    }

    #[test]
    fn all_below_threshold_returns_none() {
        let accounts = vec![
            make_account("alpha", 3.0, None),
            make_account("beta", 4.0, None),
        ];
        let config = AccountSelectionConfig {
            threshold_percent: 5.0,
        };

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_none());
        assert!(result
            .explanation
            .selection_reason
            .contains("below threshold"));
    }

    #[test]
    fn empty_accounts_returns_none() {
        let accounts: Vec<AccountRecord> = vec![];
        let config = AccountSelectionConfig::default();

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_none());
        assert_eq!(result.explanation.selection_reason, "No accounts available");
    }

    #[test]
    fn tie_break_by_lru() {
        let accounts = vec![
            make_account("alpha", 50.0, Some(2000)), // used more recently
            make_account("beta", 50.0, Some(1000)),  // used less recently
            make_account("gamma", 50.0, None),       // never used (oldest)
        ];
        let config = AccountSelectionConfig::default();

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_some());
        // gamma should win: never used = oldest
        assert_eq!(result.selected.unwrap().account_id, "gamma");
        assert!(result.explanation.selection_reason.contains("Tie-break"));
    }

    #[test]
    fn tie_break_lru_among_used_accounts() {
        let accounts = vec![
            make_account("alpha", 50.0, Some(3000)),
            make_account("beta", 50.0, Some(1000)), // oldest used
            make_account("gamma", 50.0, Some(2000)),
        ];
        let config = AccountSelectionConfig::default();

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_some());
        // beta should win: oldest last_used
        assert_eq!(result.selected.unwrap().account_id, "beta");
    }

    #[test]
    fn only_eligible_account() {
        let accounts = vec![
            make_account("alpha", 3.0, None), // below threshold
            make_account("beta", 50.0, None), // only eligible
        ];
        let config = AccountSelectionConfig {
            threshold_percent: 5.0,
        };

        let result = select_account(&accounts, &config);

        assert!(result.selected.is_some());
        assert_eq!(result.selected.unwrap().account_id, "beta");
        assert_eq!(result.explanation.selection_reason, "Only eligible account");
    }

    #[test]
    fn deterministic_selection() {
        // Same inputs should always produce same output
        let accounts = vec![
            make_account("alpha", 30.0, Some(1000)),
            make_account("beta", 50.0, Some(2000)),
            make_account("gamma", 50.0, Some(1500)),
        ];
        let config = AccountSelectionConfig::default();

        let result1 = select_account(&accounts, &config);
        let result2 = select_account(&accounts, &config);

        assert_eq!(
            result1.selected.as_ref().map(|a| &a.account_id),
            result2.selected.as_ref().map(|a| &a.account_id)
        );
    }

    #[test]
    fn from_caut_usage() {
        let caut_usage = CautAccountUsage {
            id: Some("acc-123".to_string()),
            name: Some("Test Account".to_string()),
            percent_remaining: Some(75.5),
            limit_hours: Some(24),
            reset_at: Some("2026-01-26T00:00:00Z".to_string()),
            tokens_used: Some(1000),
            tokens_remaining: Some(3000),
            tokens_limit: Some(4000),
            extra: Default::default(),
        };

        let record = AccountRecord::from_caut(&caut_usage, CautService::OpenAI, 1234567890);

        assert_eq!(record.account_id, "acc-123");
        assert_eq!(record.service, "openai");
        assert_eq!(record.name.as_deref(), Some("Test Account"));
        assert!((record.percent_remaining - 75.5).abs() < 0.001);
        assert_eq!(record.tokens_used, Some(1000));
    }
}
