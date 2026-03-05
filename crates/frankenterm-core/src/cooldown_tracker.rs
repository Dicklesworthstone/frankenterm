//! Time-based cooldown tracker for deduplication with expiry.
//!
//! Provides a generic key→cooldown map where entries automatically expire after
//! a configurable duration.  Unlike LRU-based deduplication, this ensures that
//! suppressed items resurface after their cooldown period ends.
//!
//! Use cases:
//! - Pattern detection deduplication (emit same pattern again after cooldown)
//! - Alert throttling (re-fire alert after quiet period)
//! - Event deduplication across panes (suppress repeated events temporarily)

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Configuration for a [`CooldownTracker`].
#[derive(Debug, Clone)]
pub struct CooldownConfig {
    /// Default cooldown duration for new entries.
    pub default_cooldown: Duration,
    /// Maximum number of tracked entries before forced eviction of oldest.
    pub max_entries: usize,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            default_cooldown: Duration::from_secs(300), // 5 minutes
            max_entries: 10_000,
        }
    }
}

/// Entry in the cooldown tracker.
#[derive(Debug, Clone)]
struct CooldownEntry {
    /// When this entry was last triggered/refreshed.
    last_seen: Instant,
    /// How long the cooldown lasts from `last_seen`.
    cooldown: Duration,
    /// Number of times this key was suppressed during cooldown.
    suppressed_count: u64,
    /// Total number of times this key has been seen.
    total_count: u64,
}

impl CooldownEntry {
    fn new(now: Instant, cooldown: Duration) -> Self {
        Self {
            last_seen: now,
            cooldown,
            suppressed_count: 0,
            total_count: 1,
        }
    }

    fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_seen) >= self.cooldown
    }

    fn remaining(&self, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last_seen);
        self.cooldown.saturating_sub(elapsed)
    }
}

/// Outcome of a [`CooldownTracker::check`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CooldownOutcome {
    /// Key is not in cooldown — this is the first occurrence or cooldown expired.
    /// The tracker has started a new cooldown period.
    Allowed,
    /// Key is still in cooldown. Contains remaining time and suppression count.
    Suppressed {
        /// Time remaining until cooldown expires.
        remaining: Duration,
        /// Number of times suppressed during this cooldown period.
        suppressed_count: u64,
    },
}

impl CooldownOutcome {
    /// Whether this check was allowed (not suppressed).
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Whether this check was suppressed.
    #[must_use]
    pub fn is_suppressed(&self) -> bool {
        matches!(self, Self::Suppressed { .. })
    }
}

/// Statistics snapshot from a [`CooldownTracker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownStats {
    /// Number of entries currently tracked (including expired).
    pub tracked_entries: usize,
    /// Number of entries currently in active cooldown.
    pub active_cooldowns: usize,
    /// Total number of suppressed checks across all keys.
    pub total_suppressed: u64,
    /// Total number of allowed checks across all keys.
    pub total_allowed: u64,
}

/// Time-based cooldown tracker with automatic expiry.
///
/// Each key independently tracks its cooldown period.  When a key is checked:
/// - If not tracked or expired: the check is **allowed** and a new cooldown starts.
/// - If still in cooldown: the check is **suppressed**.
///
/// Unlike LRU deduplication, cooldowns have a finite lifetime, so keys
/// always resurface after their period ends.
pub struct CooldownTracker<K> {
    entries: HashMap<K, CooldownEntry>,
    config: CooldownConfig,
    total_allowed: u64,
    total_suppressed: u64,
}

impl<K: Eq + Hash + Clone> CooldownTracker<K> {
    /// Create a new tracker with default config.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(CooldownConfig::default())
    }

    /// Create a new tracker with custom config.
    #[must_use]
    pub fn with_config(config: CooldownConfig) -> Self {
        Self {
            entries: HashMap::new(),
            config,
            total_allowed: 0,
            total_suppressed: 0,
        }
    }

    /// Check a key using the default cooldown duration.
    ///
    /// Returns whether the key was allowed or suppressed.
    pub fn check(&mut self, key: &K) -> CooldownOutcome {
        self.check_with_cooldown(key, self.config.default_cooldown)
    }

    /// Check a key with a custom cooldown duration for this specific key.
    pub fn check_with_cooldown(&mut self, key: &K, cooldown: Duration) -> CooldownOutcome {
        let now = Instant::now();
        self.check_at(key, cooldown, now)
    }

    /// Internal: check a key at a specific time (for testability).
    fn check_at(&mut self, key: &K, cooldown: Duration, now: Instant) -> CooldownOutcome {
        if let Some(entry) = self.entries.get_mut(key) {
            if entry.is_expired(now) {
                // Cooldown expired: reset and allow
                entry.last_seen = now;
                entry.cooldown = cooldown;
                entry.suppressed_count = 0;
                entry.total_count += 1;
                self.total_allowed += 1;
                CooldownOutcome::Allowed
            } else {
                // Still in cooldown: suppress
                entry.suppressed_count += 1;
                entry.total_count += 1;
                self.total_suppressed += 1;
                CooldownOutcome::Suppressed {
                    remaining: entry.remaining(now),
                    suppressed_count: entry.suppressed_count,
                }
            }
        } else {
            // New key: allow and start tracking
            self.entries
                .insert(key.clone(), CooldownEntry::new(now, cooldown));
            self.total_allowed += 1;
            self.maybe_evict(now);
            CooldownOutcome::Allowed
        }
    }

    /// Remove all expired entries.
    ///
    /// Returns the number of entries removed.
    pub fn purge_expired(&mut self) -> usize {
        let now = Instant::now();
        self.purge_expired_at(now)
    }

    fn purge_expired_at(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| !entry.is_expired(now));
        before - self.entries.len()
    }

    /// Remove a specific key from tracking.
    ///
    /// Returns whether the key was being tracked.
    pub fn clear_key(&mut self, key: &K) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Remove all tracked entries.
    pub fn clear_all(&mut self) {
        self.entries.clear();
    }

    /// Number of currently tracked entries (including expired ones not yet purged).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tracker has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Check if a key is currently in active cooldown (not expired).
    #[must_use]
    pub fn is_in_cooldown(&self, key: &K) -> bool {
        let now = Instant::now();
        self.entries
            .get(key)
            .is_some_and(|entry| !entry.is_expired(now))
    }

    /// Get the remaining cooldown time for a key. Returns `None` if not tracked or expired.
    #[must_use]
    pub fn remaining(&self, key: &K) -> Option<Duration> {
        let now = Instant::now();
        self.entries.get(key).and_then(|entry| {
            if entry.is_expired(now) {
                None
            } else {
                Some(entry.remaining(now))
            }
        })
    }

    /// Get suppression count for a key during current cooldown period.
    #[must_use]
    pub fn suppressed_count(&self, key: &K) -> u64 {
        self.entries
            .get(key)
            .map_or(0, |entry| entry.suppressed_count)
    }

    /// Get total count for a key across all cooldown periods.
    #[must_use]
    pub fn total_count(&self, key: &K) -> u64 {
        self.entries.get(key).map_or(0, |entry| entry.total_count)
    }

    /// Snapshot of tracker statistics.
    #[must_use]
    pub fn stats(&self) -> CooldownStats {
        let now = Instant::now();
        let active = self.entries.values().filter(|e| !e.is_expired(now)).count();
        CooldownStats {
            tracked_entries: self.entries.len(),
            active_cooldowns: active,
            total_suppressed: self.total_suppressed,
            total_allowed: self.total_allowed,
        }
    }

    /// Access the config.
    #[must_use]
    pub fn config(&self) -> &CooldownConfig {
        &self.config
    }

    /// Evict oldest entries when we exceed max_entries.
    fn maybe_evict(&mut self, now: Instant) {
        if self.entries.len() <= self.config.max_entries {
            return;
        }

        // First, try purging expired
        self.purge_expired_at(now);

        // If still over limit, evict oldest entries
        while self.entries.len() > self.config.max_entries {
            let oldest_key = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                self.entries.remove(&key);
            } else {
                break;
            }
        }
    }
}

impl<K: Eq + Hash + Clone> Default for CooldownTracker<K> {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_check_is_allowed() {
        let mut tracker = CooldownTracker::<String>::new();
        let outcome = tracker.check(&"key1".to_string());
        assert!(outcome.is_allowed());
    }

    #[test]
    fn test_second_check_is_suppressed() {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check(&"key1".to_string());
        let outcome = tracker.check(&"key1".to_string());
        assert!(outcome.is_suppressed());
    }

    #[test]
    fn test_different_keys_independent() {
        let mut tracker = CooldownTracker::<String>::new();
        let o1 = tracker.check(&"key1".to_string());
        let o2 = tracker.check(&"key2".to_string());
        assert!(o1.is_allowed());
        assert!(o2.is_allowed());
    }

    #[test]
    fn test_suppressed_count_increments() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        tracker.check(&key);
        tracker.check(&key);
        tracker.check(&key);
        assert_eq!(tracker.suppressed_count(&key), 2);
    }

    #[test]
    fn test_total_count_tracks_all() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        tracker.check(&key);
        tracker.check(&key);
        tracker.check(&key);
        assert_eq!(tracker.total_count(&key), 3);
    }

    #[test]
    fn test_len_tracks_entries() {
        let mut tracker = CooldownTracker::<u32>::new();
        assert!(tracker.is_empty());
        tracker.check(&1);
        tracker.check(&2);
        tracker.check(&3);
        assert_eq!(tracker.len(), 3);
    }

    #[test]
    fn test_clear_key_removes_entry() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        tracker.check(&key);
        assert!(!tracker.is_empty());
        assert!(tracker.clear_key(&key));
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_clear_key_nonexistent() {
        let mut tracker = CooldownTracker::<String>::new();
        assert!(!tracker.clear_key(&"missing".to_string()));
    }

    #[test]
    fn test_clear_all() {
        let mut tracker = CooldownTracker::<u32>::new();
        tracker.check(&1);
        tracker.check(&2);
        tracker.clear_all();
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_config_default() {
        let config = CooldownConfig::default();
        assert_eq!(config.default_cooldown, Duration::from_secs(300));
        assert_eq!(config.max_entries, 10_000);
    }

    #[test]
    fn test_custom_config() {
        let config = CooldownConfig {
            default_cooldown: Duration::from_secs(60),
            max_entries: 100,
        };
        let tracker = CooldownTracker::<String>::with_config(config);
        assert_eq!(tracker.config().default_cooldown, Duration::from_secs(60));
        assert_eq!(tracker.config().max_entries, 100);
    }

    #[test]
    fn test_expired_key_allows_again() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        let cooldown = Duration::from_millis(0);

        // Use zero-duration cooldown so it expires immediately
        let outcome = tracker.check_with_cooldown(&key, cooldown);
        assert!(outcome.is_allowed());

        // Wait a tiny bit and check again
        std::thread::sleep(Duration::from_millis(1));

        let outcome2 = tracker.check_with_cooldown(&key, cooldown);
        assert!(outcome2.is_allowed());
    }

    #[test]
    fn test_suppressed_has_remaining_time() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        let cooldown = Duration::from_secs(60);
        tracker.check_with_cooldown(&key, cooldown);
        let outcome = tracker.check_with_cooldown(&key, cooldown);
        if let CooldownOutcome::Suppressed {
            remaining,
            suppressed_count,
        } = outcome
        {
            assert!(remaining > Duration::ZERO);
            assert!(remaining <= cooldown);
            assert_eq!(suppressed_count, 1);
        } else {
            panic!("Expected Suppressed outcome");
        }
    }

    #[test]
    fn test_stats_initial() {
        let tracker = CooldownTracker::<String>::new();
        let stats = tracker.stats();
        assert_eq!(stats.tracked_entries, 0);
        assert_eq!(stats.active_cooldowns, 0);
        assert_eq!(stats.total_suppressed, 0);
        assert_eq!(stats.total_allowed, 0);
    }

    #[test]
    fn test_stats_after_checks() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        tracker.check(&key);
        tracker.check(&key);
        tracker.check(&key);

        let stats = tracker.stats();
        assert_eq!(stats.tracked_entries, 1);
        assert_eq!(stats.total_allowed, 1);
        assert_eq!(stats.total_suppressed, 2);
    }

    #[test]
    fn test_is_in_cooldown() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        assert!(!tracker.is_in_cooldown(&key));
        tracker.check(&key);
        assert!(tracker.is_in_cooldown(&key));
    }

    #[test]
    fn test_remaining_none_for_untracked() {
        let tracker = CooldownTracker::<String>::new();
        assert!(tracker.remaining(&"x".to_string()).is_none());
    }

    #[test]
    fn test_remaining_some_for_active() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        tracker.check_with_cooldown(&key, Duration::from_secs(60));
        let rem = tracker.remaining(&key);
        assert!(rem.is_some());
        assert!(rem.unwrap() <= Duration::from_secs(60));
    }

    #[test]
    fn test_purge_expired_zero_cooldown() {
        let mut tracker = CooldownTracker::<String>::new();
        let cooldown = Duration::from_millis(0);
        tracker.check_with_cooldown(&"a".to_string(), cooldown);
        tracker.check_with_cooldown(&"b".to_string(), cooldown);

        std::thread::sleep(Duration::from_millis(1));
        let purged = tracker.purge_expired();
        assert_eq!(purged, 2);
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_purge_keeps_active() {
        let mut tracker = CooldownTracker::<String>::new();
        tracker.check_with_cooldown(&"active".to_string(), Duration::from_secs(300));
        tracker.check_with_cooldown(&"expired".to_string(), Duration::from_millis(0));

        std::thread::sleep(Duration::from_millis(1));
        let purged = tracker.purge_expired();
        assert_eq!(purged, 1);
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_max_entries_eviction() {
        let config = CooldownConfig {
            default_cooldown: Duration::from_secs(300),
            max_entries: 3,
        };
        let mut tracker = CooldownTracker::<u32>::with_config(config);
        tracker.check(&1);
        tracker.check(&2);
        tracker.check(&3);
        tracker.check(&4); // Should evict oldest
        assert!(tracker.len() <= 3);
    }

    #[test]
    fn test_outcome_is_allowed() {
        assert!(CooldownOutcome::Allowed.is_allowed());
        assert!(!CooldownOutcome::Allowed.is_suppressed());
    }

    #[test]
    fn test_outcome_is_suppressed() {
        let s = CooldownOutcome::Suppressed {
            remaining: Duration::from_secs(1),
            suppressed_count: 1,
        };
        assert!(s.is_suppressed());
        assert!(!s.is_allowed());
    }

    #[test]
    fn test_integer_key() {
        let mut tracker = CooldownTracker::<u64>::new();
        assert!(tracker.check(&42).is_allowed());
        assert!(tracker.check(&42).is_suppressed());
        assert!(tracker.check(&43).is_allowed());
    }

    #[test]
    fn test_tuple_key() {
        let mut tracker = CooldownTracker::<(u32, u32)>::new();
        assert!(tracker.check(&(1, 2)).is_allowed());
        assert!(tracker.check(&(1, 2)).is_suppressed());
        assert!(tracker.check(&(1, 3)).is_allowed());
    }

    #[test]
    fn test_suppressed_count_for_untracked() {
        let tracker = CooldownTracker::<String>::new();
        assert_eq!(tracker.suppressed_count(&"x".to_string()), 0);
    }

    #[test]
    fn test_total_count_for_untracked() {
        let tracker = CooldownTracker::<String>::new();
        assert_eq!(tracker.total_count(&"x".to_string()), 0);
    }

    #[test]
    fn test_clear_key_after_suppression_resets() {
        let mut tracker = CooldownTracker::<String>::new();
        let key = "k".to_string();
        tracker.check(&key);
        tracker.check(&key);
        tracker.clear_key(&key);
        // After clearing, next check is allowed
        assert!(tracker.check(&key).is_allowed());
    }

    #[test]
    fn test_default_impl() {
        let tracker = CooldownTracker::<String>::default();
        assert!(tracker.is_empty());
        assert_eq!(tracker.config().default_cooldown, Duration::from_secs(300));
    }

    #[test]
    fn test_stats_equality() {
        let s1 = CooldownStats {
            tracked_entries: 1,
            active_cooldowns: 1,
            total_suppressed: 5,
            total_allowed: 3,
        };
        let s2 = s1.clone();
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_config_clone() {
        let c1 = CooldownConfig::default();
        let c2 = c1.clone();
        assert_eq!(c1.default_cooldown, c2.default_cooldown);
        assert_eq!(c1.max_entries, c2.max_entries);
    }
}
