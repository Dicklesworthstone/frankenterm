//! Token bucket rate limiter for fine-grained flow control.
//!
//! The token bucket algorithm allows bursts of activity up to the bucket
//! capacity while enforcing an average rate over time. Tokens are added at
//! a constant rate and consumed by operations.
//!
//! # Algorithm
//!
//! - Bucket holds up to `capacity` tokens.
//! - Tokens refill at `refill_rate` tokens per second.
//! - Each operation costs 1 or more tokens.
//! - If insufficient tokens, the operation is denied (non-blocking) or
//!   the caller can compute the wait time.
//!
//! # Use cases in FrankenTerm
//!
//! - **API throttling**: Limit WezTerm CLI calls per second.
//! - **Output rate**: Cap how fast pane output is processed.
//! - **Agent actions**: Limit robot-mode actions per interval.
//! - **Hierarchical**: Per-pane bucket nested under global bucket.

use serde::{Deserialize, Serialize};

// =============================================================================
// TokenBucket
// =============================================================================

/// A token bucket rate limiter.
///
/// Uses a timestamp-based lazy refill: tokens accumulate between calls
/// without background threads.
///
/// # Example
///
/// ```ignore
/// let mut bucket = TokenBucket::new(10.0, 5.0); // 10 capacity, 5 tokens/sec
/// let now_ms = current_time_ms();
/// assert!(bucket.try_acquire(1, now_ms));  // succeeds
/// ```
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Maximum tokens the bucket can hold.
    capacity: f64,
    /// Tokens added per second.
    refill_rate: f64,
    /// Current available tokens.
    tokens: f64,
    /// Last refill timestamp (milliseconds).
    last_refill_ms: u64,
    /// Total tokens consumed.
    total_consumed: u64,
    /// Total requests denied.
    total_denied: u64,
}

impl TokenBucket {
    /// Create a new token bucket.
    ///
    /// Starts full (tokens = capacity).
    ///
    /// # Panics
    ///
    /// Panics if `capacity` or `refill_rate` is not positive.
    #[must_use]
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        assert!(capacity > 0.0, "capacity must be positive");
        assert!(refill_rate > 0.0, "refill_rate must be positive");
        Self {
            capacity,
            refill_rate,
            tokens: capacity,
            last_refill_ms: 0,
            total_consumed: 0,
            total_denied: 0,
        }
    }

    /// Create a token bucket starting empty.
    #[must_use]
    pub fn new_empty(capacity: f64, refill_rate: f64) -> Self {
        let mut bucket = Self::new(capacity, refill_rate);
        bucket.tokens = 0.0;
        bucket
    }

    /// Create a token bucket with an initial timestamp.
    #[must_use]
    pub fn with_time(capacity: f64, refill_rate: f64, now_ms: u64) -> Self {
        let mut bucket = Self::new(capacity, refill_rate);
        bucket.last_refill_ms = now_ms;
        bucket
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self, now_ms: u64) {
        if now_ms <= self.last_refill_ms {
            return;
        }
        let elapsed_secs = (now_ms - self.last_refill_ms) as f64 / 1000.0;
        let new_tokens = elapsed_secs * self.refill_rate;
        self.tokens = (self.tokens + new_tokens).min(self.capacity);
        self.last_refill_ms = now_ms;
    }

    /// Try to consume `cost` tokens. Returns `true` if successful.
    ///
    /// Non-blocking: if insufficient tokens, returns `false` without waiting.
    pub fn try_acquire(&mut self, cost: u32, now_ms: u64) -> bool {
        self.refill(now_ms);
        let cost_f = cost as f64;
        if self.tokens >= cost_f {
            self.tokens -= cost_f;
            self.total_consumed += cost as u64;
            true
        } else {
            self.total_denied += 1;
            false
        }
    }

    /// Try to consume 1 token.
    pub fn try_acquire_one(&mut self, now_ms: u64) -> bool {
        self.try_acquire(1, now_ms)
    }

    /// How long (in ms) until `cost` tokens are available.
    ///
    /// Returns 0 if tokens are already available.
    #[must_use]
    pub fn wait_time_ms(&mut self, cost: u32, now_ms: u64) -> u64 {
        self.refill(now_ms);
        let deficit = cost as f64 - self.tokens;
        if deficit <= 0.0 {
            0
        } else {
            (deficit / self.refill_rate * 1000.0).ceil() as u64
        }
    }

    /// Current number of available tokens.
    #[must_use]
    pub fn available(&mut self, now_ms: u64) -> f64 {
        self.refill(now_ms);
        self.tokens
    }

    /// Maximum capacity.
    #[must_use]
    pub fn capacity(&self) -> f64 {
        self.capacity
    }

    /// Refill rate (tokens per second).
    #[must_use]
    pub fn refill_rate(&self) -> f64 {
        self.refill_rate
    }

    /// Total tokens consumed since creation.
    #[must_use]
    pub fn total_consumed(&self) -> u64 {
        self.total_consumed
    }

    /// Total requests denied since creation.
    #[must_use]
    pub fn total_denied(&self) -> u64 {
        self.total_denied
    }

    /// Reset the bucket to full capacity.
    pub fn reset(&mut self, now_ms: u64) {
        self.tokens = self.capacity;
        self.last_refill_ms = now_ms;
    }

    /// Update the refill rate dynamically.
    pub fn set_refill_rate(&mut self, rate: f64) {
        assert!(rate > 0.0, "refill_rate must be positive");
        self.refill_rate = rate;
    }

    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> BucketStats {
        BucketStats {
            capacity: self.capacity,
            refill_rate: self.refill_rate,
            current_tokens: self.tokens,
            total_consumed: self.total_consumed,
            total_denied: self.total_denied,
            fill_ratio: self.tokens / self.capacity,
        }
    }
}

// =============================================================================
// HierarchicalBucket
// =============================================================================

/// A two-level hierarchical token bucket.
///
/// Operations must pass both a local (per-resource) and global bucket.
/// This enforces both per-pane and system-wide rate limits.
///
/// # Example
///
/// ```ignore
/// let mut hb = HierarchicalBucket::new(
///     TokenBucket::new(5.0, 2.0),   // local: 5 capacity, 2/sec
///     TokenBucket::new(50.0, 20.0),  // global: 50 capacity, 20/sec
/// );
/// assert!(hb.try_acquire(1, now_ms)); // checks both buckets
/// ```
#[derive(Debug, Clone)]
pub struct HierarchicalBucket {
    local: TokenBucket,
    global: TokenBucket,
}

impl HierarchicalBucket {
    /// Create a new hierarchical bucket with local and global limits.
    #[must_use]
    pub fn new(local: TokenBucket, global: TokenBucket) -> Self {
        Self { local, global }
    }

    /// Try to acquire tokens from both buckets.
    ///
    /// The operation succeeds only if BOTH buckets have sufficient tokens.
    /// If the local bucket has tokens but the global doesn't, neither is
    /// consumed (atomic check).
    pub fn try_acquire(&mut self, cost: u32, now_ms: u64) -> HierarchicalResult {
        // Check both first (non-consuming).
        let local_avail = self.local.available(now_ms) >= cost as f64;
        let global_avail = self.global.available(now_ms) >= cost as f64;

        if local_avail && global_avail {
            self.local.try_acquire(cost, now_ms);
            self.global.try_acquire(cost, now_ms);
            HierarchicalResult::Allowed
        } else if !local_avail {
            self.local.total_denied += 1;
            HierarchicalResult::DeniedLocal {
                wait_ms: self.local.wait_time_ms(cost, now_ms),
            }
        } else {
            self.global.total_denied += 1;
            HierarchicalResult::DeniedGlobal {
                wait_ms: self.global.wait_time_ms(cost, now_ms),
            }
        }
    }

    /// Get the local bucket reference.
    #[must_use]
    pub fn local(&self) -> &TokenBucket {
        &self.local
    }

    /// Get the global bucket reference.
    #[must_use]
    pub fn global(&self) -> &TokenBucket {
        &self.global
    }
}

/// Result of a hierarchical bucket acquisition.
#[derive(Debug, Clone, PartialEq)]
pub enum HierarchicalResult {
    /// Both local and global buckets allowed the operation.
    Allowed,
    /// Local bucket denied — wait the given ms.
    DeniedLocal { wait_ms: u64 },
    /// Global bucket denied — wait the given ms.
    DeniedGlobal { wait_ms: u64 },
}

impl HierarchicalResult {
    /// Whether the operation was allowed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, HierarchicalResult::Allowed)
    }
}

// =============================================================================
// BucketStats (serializable)
// =============================================================================

/// Serializable statistics about a token bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketStats {
    /// Maximum capacity.
    pub capacity: f64,
    /// Refill rate (tokens/sec).
    pub refill_rate: f64,
    /// Current available tokens.
    pub current_tokens: f64,
    /// Total tokens consumed.
    pub total_consumed: u64,
    /// Total requests denied.
    pub total_denied: u64,
    /// Fill ratio (current / capacity).
    pub fill_ratio: f64,
}

// =============================================================================
// BucketConfig (serializable configuration)
// =============================================================================

/// Serializable configuration for creating a token bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    /// Maximum tokens.
    pub capacity: f64,
    /// Tokens per second.
    pub refill_rate: f64,
    /// Whether to start empty (default: start full).
    #[serde(default)]
    pub start_empty: bool,
}

impl BucketConfig {
    /// Create a token bucket from this configuration.
    #[must_use]
    pub fn build(&self, now_ms: u64) -> TokenBucket {
        if self.start_empty {
            let mut b = TokenBucket::new_empty(self.capacity, self.refill_rate);
            b.last_refill_ms = now_ms;
            b
        } else {
            TokenBucket::with_time(self.capacity, self.refill_rate, now_ms)
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- TokenBucket basic ------------------------------------------------------

    #[test]
    fn new_bucket_is_full() {
        let b = TokenBucket::new(10.0, 5.0);
        assert!((b.capacity() - 10.0).abs() < f64::EPSILON);
        assert!((b.refill_rate() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn acquire_when_full() {
        let mut b = TokenBucket::with_time(10.0, 5.0, 0);
        assert!(b.try_acquire_one(0));
        assert_eq!(b.total_consumed(), 1);
    }

    #[test]
    fn acquire_depletes_tokens() {
        let mut b = TokenBucket::with_time(3.0, 1.0, 0);
        assert!(b.try_acquire_one(0));
        assert!(b.try_acquire_one(0));
        assert!(b.try_acquire_one(0));
        assert!(!b.try_acquire_one(0)); // depleted
        assert_eq!(b.total_consumed(), 3);
        assert_eq!(b.total_denied(), 1);
    }

    #[test]
    fn refill_over_time() {
        let mut b = TokenBucket::with_time(10.0, 10.0, 0); // 10 tokens/sec
        // Consume all tokens.
        for _ in 0..10 {
            b.try_acquire_one(0);
        }
        assert!(!b.try_acquire_one(0)); // empty

        // Wait 500ms → should get 5 tokens.
        assert!(b.try_acquire(5, 500));
        assert!(!b.try_acquire_one(500)); // used them all
    }

    #[test]
    fn refill_caps_at_capacity() {
        let mut b = TokenBucket::with_time(5.0, 100.0, 0); // fast refill
        b.try_acquire(5, 0); // empty it
        // Wait 10 seconds → would generate 1000 tokens, but capped at 5.
        let avail = b.available(10_000);
        assert!((avail - 5.0).abs() < 0.01);
    }

    #[test]
    fn multi_token_acquire() {
        let mut b = TokenBucket::with_time(10.0, 5.0, 0);
        assert!(b.try_acquire(5, 0));
        assert!(b.try_acquire(5, 0));
        assert!(!b.try_acquire(1, 0)); // empty
    }

    #[test]
    fn empty_bucket() {
        let mut b = TokenBucket::new_empty(10.0, 5.0);
        assert!(!b.try_acquire_one(0));
    }

    #[test]
    fn empty_bucket_refills() {
        let mut b = TokenBucket::new_empty(10.0, 10.0);
        b.last_refill_ms = 0;
        // After 1 second, should have 10 tokens.
        assert!(b.try_acquire(10, 1000));
    }

    // -- Wait time --------------------------------------------------------------

    #[test]
    fn wait_time_when_available() {
        let mut b = TokenBucket::with_time(10.0, 5.0, 0);
        assert_eq!(b.wait_time_ms(1, 0), 0);
    }

    #[test]
    fn wait_time_when_empty() {
        let mut b = TokenBucket::with_time(1.0, 1.0, 0); // 1 token/sec
        b.try_acquire_one(0); // empty
        let wait = b.wait_time_ms(1, 0);
        assert_eq!(wait, 1000); // need 1 token, refill 1/sec = 1000ms
    }

    #[test]
    fn wait_time_partial() {
        let mut b = TokenBucket::with_time(10.0, 2.0, 0); // 2 tokens/sec
        b.try_acquire(10, 0); // empty
        let wait = b.wait_time_ms(5, 0);
        assert_eq!(wait, 2500); // 5 tokens / 2 per sec = 2.5 sec
    }

    // -- Reset ------------------------------------------------------------------

    #[test]
    fn reset_refills() {
        let mut b = TokenBucket::with_time(10.0, 1.0, 0);
        b.try_acquire(10, 0); // empty
        b.reset(0);
        assert!(b.try_acquire(10, 0)); // full again
    }

    // -- Dynamic rate -----------------------------------------------------------

    #[test]
    fn set_refill_rate() {
        let mut b = TokenBucket::with_time(10.0, 1.0, 0);
        b.try_acquire(10, 0); // empty
        b.set_refill_rate(10.0); // speed up
        // After 500ms at 10/sec → 5 tokens.
        assert!(b.try_acquire(5, 500));
    }

    #[test]
    #[should_panic(expected = "refill_rate must be positive")]
    fn set_zero_rate_panics() {
        let mut b = TokenBucket::new(10.0, 1.0);
        b.set_refill_rate(0.0);
    }

    // -- Stats ------------------------------------------------------------------

    #[test]
    fn stats_full() {
        let b = TokenBucket::new(10.0, 5.0);
        let s = b.stats();
        assert!((s.fill_ratio - 1.0).abs() < f64::EPSILON);
        assert_eq!(s.total_consumed, 0);
        assert_eq!(s.total_denied, 0);
    }

    #[test]
    fn stats_after_usage() {
        let mut b = TokenBucket::with_time(10.0, 5.0, 0);
        b.try_acquire(7, 0);
        b.try_acquire(5, 0); // denied
        let s = b.stats();
        assert_eq!(s.total_consumed, 7);
        assert_eq!(s.total_denied, 1);
        assert!((s.current_tokens - 3.0).abs() < 0.01);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let b = TokenBucket::new(10.0, 5.0);
        let s = b.stats();
        let json = serde_json::to_string(&s).unwrap();
        let back: BucketStats = serde_json::from_str(&json).unwrap();
        assert!((s.capacity - back.capacity).abs() < f64::EPSILON);
    }

    // -- BucketConfig -----------------------------------------------------------

    #[test]
    fn config_build_full() {
        let config = BucketConfig {
            capacity: 10.0,
            refill_rate: 5.0,
            start_empty: false,
        };
        let mut b = config.build(1000);
        assert!(b.try_acquire(10, 1000));
    }

    #[test]
    fn config_build_empty() {
        let config = BucketConfig {
            capacity: 10.0,
            refill_rate: 5.0,
            start_empty: true,
        };
        let mut b = config.build(0);
        assert!(!b.try_acquire_one(0));
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = BucketConfig {
            capacity: 10.0,
            refill_rate: 5.0,
            start_empty: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: BucketConfig = serde_json::from_str(&json).unwrap();
        assert!((config.capacity - back.capacity).abs() < f64::EPSILON);
        assert_eq!(config.start_empty, back.start_empty);
    }

    // -- HierarchicalBucket -----------------------------------------------------

    #[test]
    fn hierarchical_both_allow() {
        let local = TokenBucket::with_time(5.0, 2.0, 0);
        let global = TokenBucket::with_time(50.0, 20.0, 0);
        let mut hb = HierarchicalBucket::new(local, global);
        let result = hb.try_acquire(1, 0);
        assert!(result.is_allowed());
    }

    #[test]
    fn hierarchical_local_denied() {
        let local = TokenBucket::new_empty(5.0, 2.0);
        let global = TokenBucket::with_time(50.0, 20.0, 0);
        let mut hb = HierarchicalBucket::new(local, global);
        let result = hb.try_acquire(1, 0);
        assert!(matches!(result, HierarchicalResult::DeniedLocal { .. }));
    }

    #[test]
    fn hierarchical_global_denied() {
        let local = TokenBucket::with_time(5.0, 2.0, 0);
        let global = TokenBucket::new_empty(50.0, 20.0);
        let mut hb = HierarchicalBucket::new(local, global);
        let result = hb.try_acquire(1, 0);
        assert!(matches!(result, HierarchicalResult::DeniedGlobal { .. }));
    }

    #[test]
    fn hierarchical_atomic_no_consume_on_deny() {
        let local = TokenBucket::with_time(5.0, 2.0, 0);
        let global = TokenBucket::new_empty(50.0, 20.0); // global empty
        let mut hb = HierarchicalBucket::new(local, global);
        hb.try_acquire(1, 0); // denied by global
        // Local should NOT have been consumed.
        assert_eq!(hb.local().total_consumed(), 0);
    }

    #[test]
    fn hierarchical_wait_ms() {
        let local = TokenBucket::new_empty(5.0, 1.0);
        let global = TokenBucket::with_time(50.0, 20.0, 0);
        let mut hb = HierarchicalBucket::new(local, global);
        if let HierarchicalResult::DeniedLocal { wait_ms } = hb.try_acquire(1, 0) {
            assert_eq!(wait_ms, 1000); // 1 token / 1 per sec
        } else {
            panic!("expected DeniedLocal");
        }
    }

    // -- Panics -----------------------------------------------------------------

    #[test]
    #[should_panic(expected = "capacity must be positive")]
    fn zero_capacity_panics() {
        let _ = TokenBucket::new(0.0, 1.0);
    }

    #[test]
    #[should_panic(expected = "refill_rate must be positive")]
    fn zero_rate_panics() {
        let _ = TokenBucket::new(10.0, 0.0);
    }
}
