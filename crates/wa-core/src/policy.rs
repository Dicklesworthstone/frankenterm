//! Safety and policy engine
//!
//! Provides capability gates, rate limiting, and secret redaction.

use std::collections::HashMap;
use std::time::Instant;

/// Rate limiter per pane
pub struct RateLimiter {
    /// Maximum operations per minute
    limit: u32,
    /// Tracking per pane
    pane_counts: HashMap<u64, Vec<Instant>>,
}

impl RateLimiter {
    /// Create a new rate limiter
    #[must_use]
    pub fn new(limit_per_minute: u32) -> Self {
        Self {
            limit: limit_per_minute,
            pane_counts: HashMap::new(),
        }
    }

    /// Check if operation is allowed for pane
    #[must_use]
    pub fn check(&mut self, pane_id: u64) -> bool {
        let now = Instant::now();
        let minute_ago = now
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap_or(now);

        let timestamps = self.pane_counts.entry(pane_id).or_default();

        // Remove old timestamps
        timestamps.retain(|t| *t > minute_ago);

        // Check if under limit
        if timestamps.len() < self.limit as usize {
            timestamps.push(now);
            true
        } else {
            false
        }
    }
}

/// Policy checks for safe operations
pub struct PolicyEngine {
    /// Rate limiter
    rate_limiter: RateLimiter,
    /// Whether to require prompt active before send
    require_prompt_active: bool,
}

impl PolicyEngine {
    /// Create a new policy engine
    #[must_use]
    pub fn new(rate_limit: u32, require_prompt_active: bool) -> Self {
        Self {
            rate_limiter: RateLimiter::new(rate_limit),
            require_prompt_active,
        }
    }

    /// Check if send operation is allowed
    #[must_use]
    pub fn check_send(&mut self, pane_id: u64, is_prompt_active: bool) -> PolicyResult {
        // Check rate limit
        if !self.rate_limiter.check(pane_id) {
            return PolicyResult::Denied {
                reason: "Rate limit exceeded".to_string(),
            };
        }

        // Check prompt state if required
        if self.require_prompt_active && !is_prompt_active {
            return PolicyResult::Denied {
                reason: "Prompt not active - refusing to send to running command".to_string(),
            };
        }

        PolicyResult::Allowed
    }

    /// Redact secrets from text
    #[must_use]
    pub fn redact_secrets(&self, text: &str) -> String {
        // TODO: Implement secret redaction patterns
        // For now, just pass through
        text.to_string()
    }
}

/// Policy check result
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyResult {
    /// Operation allowed
    Allowed,
    /// Operation denied
    Denied { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_under_limit() {
        let mut limiter = RateLimiter::new(10);
        assert!(limiter.check(1));
        assert!(limiter.check(1));
    }

    #[test]
    fn policy_allows_with_active_prompt() {
        let mut policy = PolicyEngine::new(30, true);
        let result = policy.check_send(1, true);
        assert_eq!(result, PolicyResult::Allowed);
    }

    #[test]
    fn policy_denies_without_active_prompt() {
        let mut policy = PolicyEngine::new(30, true);
        let result = policy.check_send(1, false);
        assert!(matches!(result, PolicyResult::Denied { .. }));
    }
}
