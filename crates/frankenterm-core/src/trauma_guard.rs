//! Trauma guard core state tracking for per-pane failure loops.
//!
//! This module tracks recent command executions and error signatures so the
//! control plane can deterministically flag recurring failure loops.
//!
//! Design goals:
//! - Low-overhead per-pane state
//! - Fast "have we seen this signature recently?" checks via Bloom filter
//! - Time-windowed recurrence accounting via [`SlidingWindow`]
//! - Bounded in-memory history for explainability and debugging

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::bloom_filter::BloomFilter;
use crate::patterns::Detection;
use crate::sliding_window::{SlidingWindow, SlidingWindowConfig};

const REASON_RECURRING_FAILURE_LOOP: &str = "recurring_failure_loop";

/// Configuration for [`TraumaState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TraumaConfig {
    /// Maximum number of events retained in history per pane.
    pub history_limit: usize,
    /// Sliding window used for per-signature recurrence counting.
    pub signature_window: SlidingWindowConfig,
    /// Number of repeated failures required to trigger intervention.
    pub loop_threshold: u64,
    /// Capacity target for the recent-signature Bloom filter.
    pub bloom_capacity: usize,
    /// Target Bloom filter false-positive rate.
    pub bloom_fp_rate: f64,
    /// Maximum signatures retained for Bloom filter rebuilds.
    pub signature_retention: usize,
}

impl Default for TraumaConfig {
    fn default() -> Self {
        Self {
            history_limit: 128,
            signature_window: SlidingWindowConfig {
                window_duration_ms: 60_000,
                n_buckets: 60,
            },
            loop_threshold: 3,
            bloom_capacity: 512,
            bloom_fp_rate: 0.01,
            signature_retention: 512,
        }
    }
}

/// A single command execution outcome tracked by [`TraumaState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraumaEvent {
    /// Event timestamp (epoch ms).
    pub timestamp_ms: u64,
    /// Deterministic hash of the executed command.
    pub command_hash: u64,
    /// Canonicalized error signatures observed for this command.
    pub error_signatures: Vec<String>,
    /// Signatures that were considered recurring at this event.
    pub recurring_signatures: Vec<String>,
}

/// Decision produced after recording a command outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraumaDecision {
    /// Whether guard intervention should occur.
    pub should_intervene: bool,
    /// Stable reason code (when intervention is required).
    pub reason_code: Option<String>,
    /// Command hash associated with the decision.
    pub command_hash: u64,
    /// Number of trailing repeated failures for this command/signature set.
    pub repeat_count: u64,
    /// Recurring signatures that contributed to this decision.
    pub recurring_signatures: Vec<String>,
}

impl TraumaDecision {
    #[must_use]
    fn allow(command_hash: u64, repeat_count: u64, recurring_signatures: Vec<String>) -> Self {
        Self {
            should_intervene: false,
            reason_code: None,
            command_hash,
            repeat_count,
            recurring_signatures,
        }
    }

    #[must_use]
    fn intervene(command_hash: u64, repeat_count: u64, recurring_signatures: Vec<String>) -> Self {
        Self {
            should_intervene: true,
            reason_code: Some(REASON_RECURRING_FAILURE_LOOP.to_string()),
            command_hash,
            repeat_count,
            recurring_signatures,
        }
    }
}

/// Per-pane trauma tracking state.
///
/// The state is intentionally bounded and lightweight:
/// - `history` is capped by `history_limit`
/// - signature windows are retained only while active in the configured window
/// - Bloom filter membership is periodically rebuilt from `recent_signatures`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraumaState {
    config: TraumaConfig,
    history: VecDeque<TraumaEvent>,
    signature_windows: HashMap<String, SlidingWindow>,
    recent_signatures: VecDeque<String>,
    signature_bloom: BloomFilter,
}

impl Default for TraumaState {
    fn default() -> Self {
        Self::new()
    }
}

impl TraumaState {
    /// Create a state tracker using default config.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(TraumaConfig::default())
    }

    /// Create a state tracker with explicit config.
    #[must_use]
    pub fn with_config(config: TraumaConfig) -> Self {
        let sanitized = sanitize_config(config);
        Self {
            history: VecDeque::with_capacity(sanitized.history_limit),
            signature_windows: HashMap::new(),
            recent_signatures: VecDeque::with_capacity(sanitized.signature_retention),
            signature_bloom: BloomFilter::with_capacity(
                sanitized.bloom_capacity,
                sanitized.bloom_fp_rate,
            ),
            config: sanitized,
        }
    }

    /// Return the active config.
    #[must_use]
    pub fn config(&self) -> &TraumaConfig {
        &self.config
    }

    /// Number of events currently retained in history.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Immutable access to recent event history.
    #[must_use]
    pub fn recent_events(&self) -> &VecDeque<TraumaEvent> {
        &self.history
    }

    /// Check whether a signature appears in recent-memory membership state.
    #[must_use]
    pub fn was_signature_seen_recently(&self, signature: &str) -> bool {
        self.signature_bloom.contains(signature.as_bytes())
    }

    /// Count occurrences for a signature within the configured sliding window.
    #[must_use]
    pub fn signature_count(&self, signature: &str, now_ms: u64) -> u64 {
        self.signature_windows
            .get(signature)
            .map_or(0, |window| window.count(now_ms))
    }

    /// Record a command result using raw signatures.
    ///
    /// `error_signatures` should come from pattern detections (typically
    /// `Detection.rule_id` values).
    pub fn record_command_result(
        &mut self,
        timestamp_ms: u64,
        command: &str,
        error_signatures: &[String],
    ) -> TraumaDecision {
        let command_hash = hash_command(command);
        let signatures = normalize_signatures(error_signatures);
        let recurring_signatures = self.record_signatures(timestamp_ms, &signatures);

        let event = TraumaEvent {
            timestamp_ms,
            command_hash,
            error_signatures: signatures.clone(),
            recurring_signatures: recurring_signatures.clone(),
        };
        self.history.push_back(event);
        self.trim_history();

        let repeat_count = self.trailing_repeat_count(command_hash, &signatures);
        let should_intervene =
            !recurring_signatures.is_empty() && repeat_count >= self.config.loop_threshold;

        if should_intervene {
            TraumaDecision::intervene(command_hash, repeat_count, recurring_signatures)
        } else {
            TraumaDecision::allow(command_hash, repeat_count, recurring_signatures)
        }
    }

    /// Record a command result from pattern detections.
    pub fn record_detections(
        &mut self,
        timestamp_ms: u64,
        command: &str,
        detections: &[Detection],
    ) -> TraumaDecision {
        let signatures: Vec<String> = detections.iter().map(|d| d.rule_id.clone()).collect();
        self.record_command_result(timestamp_ms, command, &signatures)
    }

    fn record_signatures(&mut self, timestamp_ms: u64, signatures: &[String]) -> Vec<String> {
        if signatures.is_empty() {
            self.prune_signature_windows(timestamp_ms);
            return Vec::new();
        }

        let mut recurring = Vec::new();
        for signature in signatures {
            let was_seen = self.signature_bloom.contains(signature.as_bytes());
            let window = self
                .signature_windows
                .entry(signature.clone())
                .or_insert_with(|| SlidingWindow::from_config(self.config.signature_window));

            window.record(timestamp_ms);
            let count = window.count(timestamp_ms);

            if was_seen && count >= self.config.loop_threshold {
                recurring.push(signature.clone());
            }

            self.signature_bloom.insert(signature.as_bytes());
            self.recent_signatures.push_back(signature.clone());
        }

        recurring.sort();
        recurring.dedup();
        self.trim_signature_retention();
        self.prune_signature_windows(timestamp_ms);
        recurring
    }

    fn trailing_repeat_count(&self, command_hash: u64, signatures: &[String]) -> u64 {
        if signatures.is_empty() {
            return 0;
        }

        let signature_set: HashSet<&str> = signatures.iter().map(String::as_str).collect();
        let mut count = 0_u64;

        for event in self.history.iter().rev() {
            if event.command_hash != command_hash || event.error_signatures.is_empty() {
                break;
            }

            let overlap = event
                .error_signatures
                .iter()
                .any(|signature| signature_set.contains(signature.as_str()));
            if !overlap {
                break;
            }

            count = count.saturating_add(1);
        }

        count
    }

    fn trim_history(&mut self) {
        while self.history.len() > self.config.history_limit {
            let _ = self.history.pop_front();
        }
    }

    fn trim_signature_retention(&mut self) {
        while self.recent_signatures.len() > self.config.signature_retention {
            let _ = self.recent_signatures.pop_front();
        }

        self.signature_bloom.clear();
        for signature in &self.recent_signatures {
            self.signature_bloom.insert(signature.as_bytes());
        }
    }

    fn prune_signature_windows(&mut self, now_ms: u64) {
        self.signature_windows
            .retain(|_, window| window.count(now_ms) > 0);
    }
}

/// Deterministically hash a command string for telemetry-safe identification.
#[must_use]
pub fn hash_command(command: &str) -> u64 {
    fnv1a64(command.as_bytes())
}

fn sanitize_config(mut config: TraumaConfig) -> TraumaConfig {
    config.history_limit = config.history_limit.max(1);
    config.loop_threshold = config.loop_threshold.max(1);
    config.bloom_capacity = config.bloom_capacity.max(1);
    config.signature_retention = config.signature_retention.max(1);
    config.signature_window.window_duration_ms = config.signature_window.window_duration_ms.max(1);
    config.signature_window.n_buckets = config.signature_window.n_buckets.max(1);

    if !(0.0..1.0).contains(&config.bloom_fp_rate) {
        config.bloom_fp_rate = TraumaConfig::default().bloom_fp_rate;
    }

    config
}

fn normalize_signatures(signatures: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = signatures
        .iter()
        .map(|signature| signature.trim().to_string())
        .filter(|signature| !signature.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn test_config() -> TraumaConfig {
        TraumaConfig {
            history_limit: 8,
            signature_window: SlidingWindowConfig {
                window_duration_ms: 1_000,
                n_buckets: 10,
            },
            loop_threshold: 3,
            bloom_capacity: 128,
            bloom_fp_rate: 0.01,
            signature_retention: 128,
        }
    }

    #[test]
    fn records_without_signatures_do_not_intervene() {
        let mut state = TraumaState::with_config(test_config());
        let decision = state.record_command_result(1_000, "cargo test", &[]);

        assert!(!decision.should_intervene);
        assert_eq!(decision.reason_code, None);
        assert_eq!(decision.repeat_count, 0);
        assert_eq!(state.history_len(), 1);
    }

    #[test]
    fn recurring_signature_intervenes_after_threshold() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let first = state.record_command_result(1_000, "cargo test", &signatures);
        let second = state.record_command_result(1_100, "cargo test", &signatures);
        let third = state.record_command_result(1_200, "cargo test", &signatures);

        assert!(!first.should_intervene);
        assert!(!second.should_intervene);
        assert!(third.should_intervene);
        assert_eq!(
            third.reason_code.as_deref(),
            Some(REASON_RECURRING_FAILURE_LOOP)
        );
        assert_eq!(third.repeat_count, 3);
    }

    #[test]
    fn history_rollover_respects_limit() {
        let mut config = test_config();
        config.history_limit = 3;
        let mut state = TraumaState::with_config(config);
        let signatures = vec!["core.error".to_string()];

        for i in 0_u64..6 {
            let _ = state.record_command_result(1_000 + i * 100, "cmd", &signatures);
        }

        assert_eq!(state.history_len(), 3);
        assert_eq!(
            state
                .recent_events()
                .front()
                .map(|event| event.timestamp_ms),
            Some(1_300)
        );
        assert_eq!(
            state.recent_events().back().map(|event| event.timestamp_ms),
            Some(1_500)
        );
    }

    #[test]
    fn e2e_repeated_failure_loop_decision_is_deterministic() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let decisions: Vec<TraumaDecision> = (0_u64..4)
            .map(|i| state.record_command_result(10_000 + i * 100, "cargo test", &signatures))
            .collect();

        assert_eq!(
            decisions
                .iter()
                .map(|decision| decision.should_intervene)
                .collect::<Vec<bool>>(),
            vec![false, false, true, true]
        );
        assert_eq!(
            decisions
                .iter()
                .map(|decision| decision.repeat_count)
                .collect::<Vec<u64>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            decisions[2].reason_code.as_deref(),
            Some(REASON_RECURRING_FAILURE_LOOP)
        );
        assert_eq!(
            decisions[3].reason_code.as_deref(),
            Some(REASON_RECURRING_FAILURE_LOOP)
        );
    }

    proptest! {
        #[test]
        fn proptest_signature_window_count_matches_recent_buckets(
            failures in prop::collection::vec(any::<bool>(), 1..80)
        ) {
            let mut state = TraumaState::with_config(test_config());
            let signature = vec!["loop.sig".to_string()];

            for (idx, failed) in failures.iter().enumerate() {
                let ts = (idx as u64) * 100;
                if *failed {
                    let _ = state.record_command_result(ts, "cmd", &signature);
                } else {
                    let _ = state.record_command_result(ts, "cmd", &[]);
                }

                let expected = failures
                    .iter()
                    .take(idx + 1)
                    .enumerate()
                    .filter(|(j, seen)| **seen && idx.saturating_sub(*j) < 10)
                    .count() as u64;
                let observed = state.signature_count("loop.sig", ts);
                prop_assert_eq!(observed, expected);
            }
        }

        #[test]
        fn proptest_bloom_false_positive_rate_stays_bounded(inserted_len in 16usize..96usize) {
            let mut config = test_config();
            config.bloom_capacity = inserted_len * 4;
            config.signature_retention = inserted_len * 8;
            let mut state = TraumaState::with_config(config);

            for i in 0..inserted_len {
                let signatures = vec![format!("known-{i}")];
                let _ = state.record_command_result(i as u64, "cmd", &signatures);
            }

            let queries = 256usize;
            let false_positives = (0..queries)
                .filter(|i| state.was_signature_seen_recently(&format!("unknown-{i}")))
                .count();

            prop_assert!(
                false_positives <= 64,
                "false positives too high: {false_positives} / {queries}"
            );
        }
    }
}
