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
use crate::edit_distance::jaro_winkler_str;
use crate::patterns::Detection;
use crate::sliding_window::{SlidingWindow, SlidingWindowConfig};

const REASON_RECURRING_FAILURE_LOOP: &str = "recurring_failure_loop";
const DEFAULT_COMMAND_SIMILARITY_THRESHOLD: f64 = 0.88;
const DEFAULT_TOKEN_JACCARD_THRESHOLD: f64 = 0.60;
const DEFAULT_EXECUTION_PREFIXES: &[&str] =
    &["cargo", "npm run", "npm", "python", "python3", "node", "go"];
const DEFAULT_CRITICAL_FLAGS: &[&str] = &[
    "--all",
    "--all-targets",
    "--workspace",
    "--lib",
    "--bins",
    "--tests",
    "--benches",
    "--release",
    "-p",
    "--package",
];

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
    /// Minimum character-level similarity to treat commands as trivial variations.
    pub command_similarity_threshold: f64,
    /// Minimum token-level Jaccard similarity for trivial-variation matching.
    pub token_jaccard_threshold: f64,
    /// Strip common execution prefixes (e.g. `cargo`, `npm run`) before matching.
    pub strip_execution_prefixes: bool,
    /// Common execution prefixes stripped when `strip_execution_prefixes = true`.
    pub execution_prefixes: Vec<String>,
    /// Flags considered semantic intent pivots. Mismatches break the repeat chain.
    pub critical_flags: Vec<String>,
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
            command_similarity_threshold: DEFAULT_COMMAND_SIMILARITY_THRESHOLD,
            token_jaccard_threshold: DEFAULT_TOKEN_JACCARD_THRESHOLD,
            strip_execution_prefixes: true,
            execution_prefixes: DEFAULT_EXECUTION_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_string())
                .collect(),
            critical_flags: DEFAULT_CRITICAL_FLAGS
                .iter()
                .map(|flag| (*flag).to_string())
                .collect(),
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
    /// Token-aware normalized command fingerprint for fuzzy matching.
    #[serde(default)]
    pub command_fingerprint: String,
    /// Sorted unique command tokens used for Jaccard similarity.
    #[serde(default)]
    pub command_tokens: Vec<String>,
    /// Sorted unique critical flags extracted from `command_tokens`.
    #[serde(default)]
    pub critical_flags: Vec<String>,
    /// Mutation epoch snapshot used to reset loop counting after functional edits.
    #[serde(default)]
    pub mutation_epoch: u64,
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
    mutation_epoch: u64,
    last_mutation_timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct CommandFeatures {
    command_hash: u64,
    command_fingerprint: String,
    command_tokens: Vec<String>,
    critical_flags: Vec<String>,
    mutation_epoch: u64,
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
            mutation_epoch: 0,
            last_mutation_timestamp_ms: None,
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

    /// Current mutation epoch.
    ///
    /// Each functional mutation increments the epoch and resets trailing-loop
    /// matching for subsequent commands.
    #[must_use]
    pub const fn mutation_epoch(&self) -> u64 {
        self.mutation_epoch
    }

    /// Timestamp of the most recent functional mutation, if recorded.
    #[must_use]
    pub const fn last_mutation_timestamp_ms(&self) -> Option<u64> {
        self.last_mutation_timestamp_ms
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
        let command_features = self.command_features(command);
        let command_hash = command_features.command_hash;
        let signatures = normalize_signatures(error_signatures);
        let recurring_signatures = self.record_signatures(timestamp_ms, &signatures);

        let event = TraumaEvent {
            timestamp_ms,
            command_hash,
            command_fingerprint: command_features.command_fingerprint.clone(),
            command_tokens: command_features.command_tokens.clone(),
            critical_flags: command_features.critical_flags.clone(),
            mutation_epoch: command_features.mutation_epoch,
            error_signatures: signatures.clone(),
            recurring_signatures: recurring_signatures.clone(),
        };
        self.history.push_back(event);
        self.trim_history();

        let repeat_count = self.trailing_repeat_count(&command_features, &signatures);
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

    /// Record a filesystem mutation event and update reset epoch when functional.
    ///
    /// Returns `true` when the mutation should reset loop counting (source/runtime
    /// edits). Returns `false` for scratchpad/docs mutations (`.beads/`, `*.md`,
    /// `*.txt`).
    pub fn record_mutation(&mut self, timestamp_ms: u64, path: &str) -> bool {
        if !is_functional_mutation_path(path) {
            return false;
        }
        self.mutation_epoch = self.mutation_epoch.saturating_add(1);
        self.last_mutation_timestamp_ms = Some(timestamp_ms);
        true
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

    fn trailing_repeat_count(&self, command: &CommandFeatures, signatures: &[String]) -> u64 {
        if signatures.is_empty() {
            return 0;
        }

        let signature_set: HashSet<&str> = signatures.iter().map(String::as_str).collect();
        let mut count = 0_u64;

        for event in self.history.iter().rev() {
            if event.error_signatures.is_empty() {
                break;
            }

            let overlap = event
                .error_signatures
                .iter()
                .any(|signature| signature_set.contains(signature.as_str()));
            if !overlap {
                break;
            }

            if !self.is_trivial_variation(command, event) {
                break;
            }

            count = count.saturating_add(1);
        }

        count
    }

    fn command_features(&self, command: &str) -> CommandFeatures {
        let command_hash = hash_command(command);
        let normalized = normalize_command_text(command);

        let stripped = if self.config.strip_execution_prefixes {
            strip_execution_prefix(&normalized, &self.config.execution_prefixes)
        } else {
            normalized.clone()
        };

        let command_fingerprint = if stripped.is_empty() {
            normalized
        } else {
            stripped
        };

        let command_tokens = tokenize_command(&command_fingerprint);
        let critical_flags = extract_critical_flags(&command_tokens, &self.config.critical_flags);

        CommandFeatures {
            command_hash,
            command_fingerprint,
            command_tokens,
            critical_flags,
            mutation_epoch: self.mutation_epoch,
        }
    }

    fn is_trivial_variation(&self, command: &CommandFeatures, event: &TraumaEvent) -> bool {
        if command.mutation_epoch != event.mutation_epoch {
            return false;
        }

        if command.command_hash == event.command_hash {
            return true;
        }

        if command.command_fingerprint.is_empty() || event.command_fingerprint.is_empty() {
            return false;
        }

        if has_critical_flag_conflict(&command.critical_flags, &event.critical_flags) {
            return false;
        }

        let edit_similarity =
            jaro_winkler_str(&command.command_fingerprint, &event.command_fingerprint);
        if edit_similarity < self.config.command_similarity_threshold {
            return false;
        }

        let token_similarity =
            token_jaccard_similarity(&command.command_tokens, &event.command_tokens);
        token_similarity >= self.config.token_jaccard_threshold
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

    if !(0.0..=1.0).contains(&config.command_similarity_threshold) {
        config.command_similarity_threshold = DEFAULT_COMMAND_SIMILARITY_THRESHOLD;
    }
    if !(0.0..=1.0).contains(&config.token_jaccard_threshold) {
        config.token_jaccard_threshold = DEFAULT_TOKEN_JACCARD_THRESHOLD;
    }

    if config.execution_prefixes.is_empty() {
        config.execution_prefixes = DEFAULT_EXECUTION_PREFIXES
            .iter()
            .map(|prefix| (*prefix).to_string())
            .collect();
    }
    config.execution_prefixes = sanitize_list(config.execution_prefixes);
    config
        .execution_prefixes
        .sort_by_key(|s| std::cmp::Reverse(s.len()));

    if config.critical_flags.is_empty() {
        config.critical_flags = DEFAULT_CRITICAL_FLAGS
            .iter()
            .map(|flag| (*flag).to_string())
            .collect();
    }
    config.critical_flags = sanitize_list(config.critical_flags);

    config
}

fn sanitize_list(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = values
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
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

fn normalize_command_text(command: &str) -> String {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn strip_execution_prefix(command: &str, execution_prefixes: &[String]) -> String {
    for prefix in execution_prefixes {
        if command == prefix {
            return String::new();
        }

        if let Some(rest) = command.strip_prefix(prefix) {
            if let Some(stripped) = rest.strip_prefix(' ') {
                return stripped.to_string();
            }
        }
    }

    command.to_string()
}

fn tokenize_command(command: &str) -> Vec<String> {
    let mut tokens: Vec<String> = command
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|ch: char| {
                    matches!(
                        ch,
                        ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
                    )
                })
                .to_string()
        })
        .filter(|token| !token.is_empty())
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn extract_critical_flags(tokens: &[String], critical_flags: &[String]) -> Vec<String> {
    let mut flags: Vec<String> = tokens
        .iter()
        .filter(|token| critical_flags.binary_search(token).is_ok())
        .cloned()
        .collect();
    flags.sort();
    flags.dedup();
    flags
}

fn has_critical_flag_conflict(left: &[String], right: &[String]) -> bool {
    (!left.is_empty() || !right.is_empty()) && left != right
}

fn token_jaccard_similarity(left: &[String], right: &[String]) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }

    let left_set: HashSet<&str> = left.iter().map(String::as_str).collect();
    let right_set: HashSet<&str> = right.iter().map(String::as_str).collect();

    let intersection = left_set.intersection(&right_set).count();
    let union = left_set.union(&right_set).count();
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

fn is_functional_mutation_path(path: &str) -> bool {
    let normalized = path.trim().replace('\\', "/").to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }

    if normalized == ".beads"
        || normalized.starts_with(".beads/")
        || normalized.contains("/.beads/")
    {
        return false;
    }

    if let Some(ext) = std::path::Path::new(&normalized).extension() {
        if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("txt") {
            return false;
        }
    }

    true
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
            command_similarity_threshold: 0.82,
            token_jaccard_threshold: 0.50,
            strip_execution_prefixes: true,
            execution_prefixes: vec![
                "cargo".to_string(),
                "npm run".to_string(),
                "npm".to_string(),
                "python".to_string(),
            ],
            critical_flags: vec![
                "--all".to_string(),
                "--workspace".to_string(),
                "--lib".to_string(),
                "--tests".to_string(),
                "--benches".to_string(),
                "-p".to_string(),
            ],
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
    fn fuzzy_variations_intervene_after_threshold() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let first = state.record_command_result(1_000, "cargo test -p foo", &signatures);
        let second = state.record_command_result(1_100, "cargo test -p foo -v", &signatures);
        let third = state.record_command_result(1_200, "cargo test -p foo --verbose", &signatures);

        assert!(!first.should_intervene);
        assert!(!second.should_intervene);
        assert!(third.should_intervene);
        assert_eq!(first.repeat_count, 1);
        assert_eq!(second.repeat_count, 2);
        assert_eq!(third.repeat_count, 3);
    }

    #[test]
    fn semantic_flag_change_breaks_repeat_chain() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let first = state.record_command_result(1_000, "cargo test --lib", &signatures);
        let second = state.record_command_result(1_100, "cargo test --lib -v", &signatures);
        let third = state.record_command_result(1_200, "cargo test --all", &signatures);

        assert_eq!(first.repeat_count, 1);
        assert_eq!(second.repeat_count, 2);
        assert_eq!(third.repeat_count, 1);
        assert!(!third.should_intervene);
    }

    #[test]
    fn source_mutation_resets_repeat_chain() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let first = state.record_command_result(1_000, "cargo test -p foo", &signatures);
        let second = state.record_command_result(1_100, "cargo test -p foo -v", &signatures);
        let reset = state.record_mutation(1_150, "crates/frankenterm-core/src/lib.rs");
        let third = state.record_command_result(1_200, "cargo test -p foo --verbose", &signatures);

        assert!(reset);
        assert_eq!(first.repeat_count, 1);
        assert_eq!(second.repeat_count, 2);
        assert_eq!(third.repeat_count, 1);
        assert!(!third.should_intervene);
        assert_eq!(state.mutation_epoch(), 1);
        assert_eq!(state.last_mutation_timestamp_ms(), Some(1_150));
    }

    #[test]
    fn scratchpad_mutation_does_not_reset_repeat_chain() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let _ = state.record_command_result(2_000, "cargo test -p foo", &signatures);
        let _ = state.record_command_result(2_100, "cargo test -p foo -v", &signatures);
        let reset = state.record_mutation(2_150, ".beads/issues.jsonl");
        let decision =
            state.record_command_result(2_200, "cargo test -p foo --verbose", &signatures);

        assert!(!reset);
        assert_eq!(decision.repeat_count, 3);
        assert!(decision.should_intervene);
        assert_eq!(state.mutation_epoch(), 0);
        assert_eq!(state.last_mutation_timestamp_ms(), None);
    }

    #[test]
    fn docs_mutation_does_not_reset_repeat_chain() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let _ = state.record_command_result(3_000, "cargo test -p foo", &signatures);
        let _ = state.record_command_result(3_100, "cargo test -p foo -v", &signatures);
        assert!(!state.record_mutation(3_150, "PLAN.md"));
        assert!(!state.record_mutation(3_160, "notes/todo.txt"));
        let decision =
            state.record_command_result(3_200, "cargo test -p foo --verbose", &signatures);

        assert_eq!(decision.repeat_count, 3);
        assert!(decision.should_intervene);
    }

    #[test]
    fn mutation_filter_is_case_insensitive_and_cross_platform() {
        assert!(!is_functional_mutation_path("   "));
        assert!(!is_functional_mutation_path(r".beads\issues.jsonl"));
        assert!(!is_functional_mutation_path(r"Docs\PLAN.MD"));
        assert!(!is_functional_mutation_path(r"notes\todo.TXT"));
        assert!(is_functional_mutation_path(r"src\main.RS"));
    }

    #[test]
    fn prefix_stripping_links_equivalent_commands() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let first = state.record_command_result(1_000, "cargo test -p foo", &signatures);
        let second = state.record_command_result(1_100, "test -p foo", &signatures);

        assert_eq!(first.repeat_count, 1);
        assert_eq!(second.repeat_count, 2);
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

    #[test]
    fn e2e_fuzzy_variation_loop_decision_is_deterministic() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];
        let commands = [
            "cargo test -p foo",
            "cargo test -p foo -v",
            "cargo test -p foo --verbose",
            "cargo test -p foo --verbose --color=always",
        ];

        let decisions: Vec<TraumaDecision> = commands
            .iter()
            .enumerate()
            .map(|(idx, command)| {
                state.record_command_result(20_000 + (idx as u64 * 100), command, &signatures)
            })
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
    }

    #[test]
    fn e2e_semantic_change_resets_loop_and_recovers() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let _ = state.record_command_result(30_000, "cargo test --lib", &signatures);
        let _ = state.record_command_result(30_100, "cargo test --lib -v", &signatures);
        let decision = state.record_command_result(30_200, "cargo test --all", &signatures);

        assert_eq!(decision.repeat_count, 1);
        assert!(!decision.should_intervene);
    }

    #[test]
    fn e2e_source_mutation_resets_loop_counter() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let _ = state.record_command_result(40_000, "cargo test -p foo", &signatures);
        let _ = state.record_command_result(40_100, "cargo test -p foo -v", &signatures);
        assert!(state.record_mutation(40_150, "src/main.rs"));
        let decision =
            state.record_command_result(40_200, "cargo test -p foo --verbose", &signatures);

        assert_eq!(decision.repeat_count, 1);
        assert!(!decision.should_intervene);
    }

    #[test]
    fn e2e_scratchpad_mutation_does_not_reset_loop_counter() {
        let mut state = TraumaState::with_config(test_config());
        let signatures = vec!["core.codex:error_loop".to_string()];

        let _ = state.record_command_result(50_000, "cargo test -p foo", &signatures);
        let _ = state.record_command_result(50_100, "cargo test -p foo -v", &signatures);
        assert!(!state.record_mutation(50_150, "AGENT_TODO.md"));
        let decision =
            state.record_command_result(50_200, "cargo test -p foo --verbose", &signatures);

        assert_eq!(decision.repeat_count, 3);
        assert!(decision.should_intervene);
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

        #[test]
        fn proptest_interleaved_command_error_stream_keeps_state_bounded(
            events in prop::collection::vec(
                (
                    prop_oneof![
                        Just("cargo test -p foo".to_string()),
                        Just("cargo test -p foo --verbose".to_string()),
                        Just("cargo clippy --workspace".to_string()),
                        Just("npm run lint".to_string()),
                        Just("python -m pytest tests/unit".to_string()),
                    ],
                    prop_oneof![
                        Just(Vec::<String>::new()),
                        Just(vec!["core.codex:error_loop".to_string()]),
                        Just(vec!["core.codex:error_loop".to_string(), "core.codex:retry".to_string()]),
                        Just(vec!["core.gemini:error".to_string()]),
                    ],
                    any::<bool>(),
                ),
                64..220
            )
        ) {
            let mut config = test_config();
            config.history_limit = 16;
            config.signature_retention = 64;
            config.bloom_capacity = 64;
            let mut state = TraumaState::with_config(config);

            let base_ts = 80_000_u64;
            let bucket_count = state.config().signature_window.n_buckets.max(1);
            let step_ms =
                (state.config().signature_window.window_duration_ms / bucket_count as u64).max(1);

            for (idx, (command, signatures, trigger_mutation)) in events.iter().enumerate() {
                let ts = base_ts + (idx as u64 * step_ms);

                if *trigger_mutation {
                    let mutation_path = if idx % 2 == 0 {
                        "src/lib.rs"
                    } else {
                        "NOTES.md"
                    };
                    let _ = state.record_mutation(ts.saturating_sub(1), mutation_path);
                }

                let decision = state.record_command_result(ts, command, signatures);

                prop_assert!(state.history_len() <= state.config().history_limit);
                prop_assert!(decision.repeat_count <= state.history_len() as u64);
                prop_assert!(
                    decision
                        .recurring_signatures
                        .iter()
                        .all(|sig| signatures.contains(sig))
                );

                if signatures.is_empty() {
                    prop_assert_eq!(decision.repeat_count, 0);
                    prop_assert!(!decision.should_intervene);
                }
            }

            let now_idx = events.len().saturating_sub(1);
            let now_ms = base_ts + (now_idx as u64 * step_ms);
            let buckets_in_window = state.config().signature_window.n_buckets;
            for signature in ["core.codex:error_loop", "core.codex:retry", "core.gemini:error"] {
                let expected = events
                    .iter()
                    .enumerate()
                    .filter(|(idx, (_, signatures, _))| {
                        now_idx.saturating_sub(*idx) < buckets_in_window
                            && signatures.iter().any(|sig| sig == signature)
                    })
                    .count() as u64;
                let observed = state.signature_count(signature, now_ms);
                prop_assert_eq!(observed, expected);
            }
        }

        #[test]
        fn proptest_token_jaccard_similarity_is_symmetric(
            left in prop::collection::vec("[a-z\\-]{1,8}", 0..16),
            right in prop::collection::vec("[a-z\\-]{1,8}", 0..16),
        ) {
            let left_tokens = tokenize_command(&left.join(" "));
            let right_tokens = tokenize_command(&right.join(" "));
            let forward = token_jaccard_similarity(&left_tokens, &right_tokens);
            let backward = token_jaccard_similarity(&right_tokens, &left_tokens);
            prop_assert!((forward - backward).abs() < f64::EPSILON);
            prop_assert!((0.0..=1.0).contains(&forward));
        }

        #[test]
        fn proptest_functional_mutation_filter(
            stem in "[a-z_]{1,12}",
            ext in prop_oneof![Just("rs"), Just("toml"), Just("json"), Just("yaml")]
        ) {
            let path = format!("src/{stem}.{ext}");
            prop_assert!(is_functional_mutation_path(&path));
        }
    }
}
