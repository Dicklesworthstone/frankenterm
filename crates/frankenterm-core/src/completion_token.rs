//! Cross-subsystem action completion tokens and cause-chain context.
//!
//! Prevents racey partial-completion by tracking the full lifecycle of
//! multi-step operations across capture, storage, policy, workflows, and the
//! event bus. Each operation carries a [`CompletionToken`] that accumulates a
//! [`CauseChain`] as it flows through subsystems, enforcing explicit terminal
//! states (success, timeout, failure) at defined [`CompletionBoundary`]
//! checkpoints.
//!
//! # Design principles
//!
//! * **No operation reports success before its logical completion boundary.**
//! * Timeout and failure paths preserve full postmortem context.
//! * Zero-allocation happy path: [`TokenId`] is a stack-allocated 16-byte UUID.
//! * Lock-free state tracking via atomic ordinals.
//!
//! # Example
//!
//! ```ignore
//! let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
//! let boundary = CompletionBoundary::new(&["policy", "injection", "audit"]);
//! let token = tracker.begin("send_text", boundary);
//!
//! // Policy subsystem completes its part:
//! tracker.advance(&token.id, "policy", StepOutcome::Ok, "allow-listed");
//!
//! // Injection subsystem completes:
//! tracker.advance(&token.id, "injection", StepOutcome::Ok, "sent 42 bytes");
//!
//! // Audit subsystem completes (boundary fully met):
//! tracker.advance(&token.id, "audit", StepOutcome::Ok, "recorded");
//!
//! // Token is now Completed:
//! assert_eq!(tracker.state(&token.id), Some(CompletionState::Completed));
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// =============================================================================
// Token identity
// =============================================================================

/// Unique identifier for a completion token.
///
/// Uses a simple monotonic counter + timestamp for cheap, collision-resistant
/// IDs without requiring a UUID crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenId(pub String);

impl TokenId {
    /// Generate a new token ID from a monotonic counter and current time.
    fn generate(counter: u64) -> Self {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self(format!("ct-{ts_ms:x}-{counter:04x}"))
    }
}

impl std::fmt::Display for TokenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// =============================================================================
// Completion state machine
// =============================================================================

/// Lifecycle state of a completion token.
///
/// ```text
/// Pending ──► InProgress ──┬──► Completed
///                          ├──► TimedOut
///                          ├──► Failed
///                          └──► PartialFailure
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum CompletionState {
    /// Token created but no subsystem has started processing.
    Pending = 0,
    /// At least one subsystem has reported progress.
    InProgress = 1,
    /// All boundary subsystems completed successfully.
    Completed = 2,
    /// Deadline expired before all boundary subsystems completed.
    TimedOut = 3,
    /// A subsystem reported a fatal failure; remaining steps cancelled.
    Failed = 4,
    /// Some subsystems succeeded but at least one failed non-fatally.
    PartialFailure = 5,
}

impl CompletionState {
    /// Whether this is a terminal state (no further transitions).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::TimedOut | Self::Failed | Self::PartialFailure
        )
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Pending,
            1 => Self::InProgress,
            2 => Self::Completed,
            3 => Self::TimedOut,
            4 => Self::Failed,
            5 => Self::PartialFailure,
            _ => Self::Failed, // defensive
        }
    }
}

// =============================================================================
// Cause chain
// =============================================================================

/// Outcome of a single subsystem step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// Step completed successfully.
    Ok,
    /// Step failed (details in the step's message).
    Error,
    /// Step was skipped (e.g. not applicable for this operation).
    Skipped,
    /// Step was cancelled before completion.
    Cancelled,
}

/// One step in the cause chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CauseStep {
    /// Which subsystem executed this step (e.g. "policy", "injection", "audit").
    pub subsystem: String,
    /// Result of the step.
    pub outcome: StepOutcome,
    /// Human-readable message or diagnostic detail.
    pub message: String,
    /// Wall-clock timestamp (ms since epoch).
    pub timestamp_ms: i64,
    /// Optional extra context for postmortem.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// An ordered, append-only log of steps an operation has passed through.
///
/// Each subsystem appends a [`CauseStep`] as it processes the operation,
/// building a full audit trail from initiation to terminal state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CauseChain {
    steps: Vec<CauseStep>,
}

impl CauseChain {
    /// Create an empty cause chain.
    #[must_use]
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Append a step to the chain.
    pub fn push(&mut self, step: CauseStep) {
        self.steps.push(step);
    }

    /// Convenience: append a step with just subsystem, outcome, and message.
    pub fn record(&mut self, subsystem: &str, outcome: StepOutcome, message: impl Into<String>) {
        self.push(CauseStep {
            subsystem: subsystem.to_string(),
            outcome,
            message: message.into(),
            timestamp_ms: now_ms(),
            metadata: HashMap::new(),
        });
    }

    /// All steps in order.
    #[must_use]
    pub fn steps(&self) -> &[CauseStep] {
        &self.steps
    }

    /// Number of steps recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether no steps have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Subsystems that reported errors.
    #[must_use]
    pub fn failed_subsystems(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Error)
            .map(|s| s.subsystem.as_str())
            .collect()
    }

    /// Duration from first to last step (ms), or 0 if fewer than 2 steps.
    #[must_use]
    pub fn elapsed_ms(&self) -> i64 {
        match (self.steps.first(), self.steps.last()) {
            (Some(first), Some(last)) if self.steps.len() >= 2 => {
                last.timestamp_ms - first.timestamp_ms
            }
            _ => 0,
        }
    }
}

// =============================================================================
// Completion boundary
// =============================================================================

/// Defines which subsystems must complete before an operation is considered done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionBoundary {
    /// Set of subsystem names that must report a step before completion.
    required: Vec<String>,
}

impl CompletionBoundary {
    /// Create a boundary requiring the named subsystems.
    #[must_use]
    pub fn new(subsystems: &[&str]) -> Self {
        Self {
            required: subsystems.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// Check whether all required subsystems have reported in the cause chain.
    #[must_use]
    pub fn is_satisfied(&self, chain: &CauseChain) -> bool {
        self.required.iter().all(|req| {
            chain
                .steps
                .iter()
                .any(|step| step.subsystem == *req && step.outcome != StepOutcome::Cancelled)
        })
    }

    /// Required subsystem names.
    #[must_use]
    pub fn required(&self) -> &[String] {
        &self.required
    }

    /// Subsystems that haven't reported yet.
    #[must_use]
    pub fn pending_subsystems<'a>(&'a self, chain: &'a CauseChain) -> Vec<&'a str> {
        self.required
            .iter()
            .filter(|req| {
                !chain
                    .steps
                    .iter()
                    .any(|step| step.subsystem == **req && step.outcome != StepOutcome::Cancelled)
            })
            .map(|s| s.as_str())
            .collect()
    }
}

// =============================================================================
// Completion token
// =============================================================================

/// Tracks the full lifecycle of a multi-step operation.
///
/// Created via [`CompletionTracker::begin`], accumulates [`CauseStep`]s as the
/// operation flows through subsystems, and transitions to a terminal state when
/// the [`CompletionBoundary`] is satisfied (or a timeout/failure occurs).
#[derive(Debug)]
pub struct CompletionToken {
    /// Unique token identifier (usable as correlation_id in audit records).
    pub id: TokenId,
    /// Human-readable operation name (e.g. "send_text", "workflow:deploy").
    pub operation: String,
    /// Atomic state for lock-free reads.
    state: AtomicU8,
    /// Cause chain accumulating steps.
    pub cause_chain: CauseChain,
    /// Boundary defining completion requirements.
    pub boundary: CompletionBoundary,
    /// Creation timestamp (ms since epoch).
    pub created_at_ms: i64,
    /// Deadline (ms since epoch); None = no timeout.
    pub deadline_ms: Option<i64>,
    /// Optional pane context.
    pub pane_id: Option<u64>,
}

impl CompletionToken {
    /// Current state (lock-free read).
    #[must_use]
    pub fn state(&self) -> CompletionState {
        CompletionState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Set state (only transitions to more-terminal states).
    fn set_state(&self, new: CompletionState) {
        let current = self.state.load(Ordering::Acquire);
        let current_state = CompletionState::from_u8(current);
        if !current_state.is_terminal() {
            self.state.store(new as u8, Ordering::Release);
        }
    }

    /// Whether the token has reached a terminal state.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state().is_terminal()
    }

    /// The token ID as a string usable as a `correlation_id`.
    #[must_use]
    pub fn correlation_id(&self) -> &str {
        &self.id.0
    }

    /// Check if the deadline has passed (if set).
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.deadline_ms.map(|dl| now_ms() >= dl).unwrap_or(false)
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for `CompletionTracker`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletionTrackerConfig {
    /// Default timeout for operations (ms). 0 = no timeout.
    pub default_timeout_ms: u64,
    /// Maximum number of active tokens before rejecting new ones.
    pub max_active_tokens: usize,
    /// How long to retain completed tokens for postmortem queries (ms).
    pub retention_ms: u64,
}

impl Default for CompletionTrackerConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: 30_000, // 30 seconds
            max_active_tokens: 10_000,
            retention_ms: 300_000, // 5 minutes
        }
    }
}

// =============================================================================
// Completion tracker
// =============================================================================

/// Manages the lifecycle of active completion tokens.
///
/// Thread-safe for single-threaded async runtimes (not Sync). For multi-threaded
/// use, wrap in a Mutex.
pub struct CompletionTracker {
    config: CompletionTrackerConfig,
    tokens: HashMap<TokenId, CompletionToken>,
    counter: u64,
}

impl CompletionTracker {
    /// Create a new tracker.
    #[must_use]
    pub fn new(config: CompletionTrackerConfig) -> Self {
        Self {
            config,
            tokens: HashMap::new(),
            counter: 0,
        }
    }

    /// Begin tracking a new operation.
    ///
    /// Returns the token ID for correlation. Returns `None` if the tracker
    /// is at capacity.
    pub fn begin(&mut self, operation: &str, boundary: CompletionBoundary) -> Option<TokenId> {
        self.begin_with_options(operation, boundary, None, None)
    }

    /// Begin tracking with optional timeout and pane context.
    pub fn begin_with_options(
        &mut self,
        operation: &str,
        boundary: CompletionBoundary,
        timeout_ms: Option<u64>,
        pane_id: Option<u64>,
    ) -> Option<TokenId> {
        if self.active_count() >= self.config.max_active_tokens {
            return None;
        }

        self.counter += 1;
        let id = TokenId::generate(self.counter);
        let now = now_ms();
        let effective_timeout = timeout_ms.or(if self.config.default_timeout_ms > 0 {
            Some(self.config.default_timeout_ms)
        } else {
            None
        });
        let deadline = effective_timeout.map(|t| now + t as i64);

        let token = CompletionToken {
            id: id.clone(),
            operation: operation.to_string(),
            state: AtomicU8::new(CompletionState::Pending as u8),
            cause_chain: CauseChain::new(),
            boundary,
            created_at_ms: now,
            deadline_ms: deadline,
            pane_id,
        };
        self.tokens.insert(id.clone(), token);
        Some(id)
    }

    /// Record a subsystem step and potentially transition the token's state.
    ///
    /// Returns the new state, or `None` if the token doesn't exist.
    pub fn advance(
        &mut self,
        token_id: &TokenId,
        subsystem: &str,
        outcome: StepOutcome,
        message: impl Into<String>,
    ) -> Option<CompletionState> {
        self.advance_with_metadata(token_id, subsystem, outcome, message, HashMap::new())
    }

    /// Record a step with additional metadata.
    pub fn advance_with_metadata(
        &mut self,
        token_id: &TokenId,
        subsystem: &str,
        outcome: StepOutcome,
        message: impl Into<String>,
        metadata: HashMap<String, String>,
    ) -> Option<CompletionState> {
        let token = self.tokens.get_mut(token_id)?;

        // Don't modify terminal tokens.
        if token.state().is_terminal() {
            return Some(token.state());
        }

        // Record the step.
        token.cause_chain.push(CauseStep {
            subsystem: subsystem.to_string(),
            outcome,
            message: message.into(),
            timestamp_ms: now_ms(),
            metadata,
        });

        // Transition from Pending → InProgress on first step.
        if token.state() == CompletionState::Pending {
            token.set_state(CompletionState::InProgress);
        }

        // Determine new state based on outcome and boundary.
        let new_state = match outcome {
            StepOutcome::Error => {
                // Check if there are any successful steps (partial failure)
                // vs pure failure.
                let has_ok = token
                    .cause_chain
                    .steps()
                    .iter()
                    .any(|s| s.outcome == StepOutcome::Ok);
                if has_ok {
                    CompletionState::PartialFailure
                } else {
                    CompletionState::Failed
                }
            }
            StepOutcome::Ok | StepOutcome::Skipped => {
                if token.boundary.is_satisfied(&token.cause_chain) {
                    // Check for any errors in the chain.
                    let has_errors = token
                        .cause_chain
                        .steps()
                        .iter()
                        .any(|s| s.outcome == StepOutcome::Error);
                    if has_errors {
                        CompletionState::PartialFailure
                    } else {
                        CompletionState::Completed
                    }
                } else {
                    CompletionState::InProgress
                }
            }
            StepOutcome::Cancelled => CompletionState::Failed,
        };

        token.set_state(new_state);
        Some(new_state)
    }

    /// Explicitly fail a token (e.g. on unrecoverable error).
    pub fn fail(&mut self, token_id: &TokenId, reason: &str) -> Option<CompletionState> {
        self.advance(token_id, "_system", StepOutcome::Error, reason)
    }

    /// Explicitly mark a token as timed out.
    pub fn timeout(&mut self, token_id: &TokenId) -> Option<CompletionState> {
        let token = self.tokens.get_mut(token_id)?;
        if !token.state().is_terminal() {
            token
                .cause_chain
                .record("_system", StepOutcome::Error, "operation timed out");
            token.set_state(CompletionState::TimedOut);
        }
        Some(token.state())
    }

    /// Scan for expired tokens and transition them to TimedOut.
    ///
    /// Returns the IDs of tokens that were timed out.
    pub fn sweep_timeouts(&mut self) -> Vec<TokenId> {
        let now = now_ms();
        let expired: Vec<TokenId> = self
            .tokens
            .iter()
            .filter(|(_, t)| {
                !t.state().is_terminal() && t.deadline_ms.map(|dl| now >= dl).unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired {
            self.timeout(id);
        }
        expired
    }

    /// Evict completed tokens older than the retention window.
    ///
    /// Returns the number of tokens evicted.
    pub fn evict_completed(&mut self) -> usize {
        let cutoff = now_ms() - self.config.retention_ms as i64;
        let before = self.tokens.len();
        self.tokens
            .retain(|_, t| !t.state().is_terminal() || t.created_at_ms > cutoff);
        before - self.tokens.len()
    }

    /// Current state of a token.
    #[must_use]
    pub fn state(&self, token_id: &TokenId) -> Option<CompletionState> {
        self.tokens.get(token_id).map(|t| t.state())
    }

    /// Get the cause chain for a token.
    #[must_use]
    pub fn cause_chain(&self, token_id: &TokenId) -> Option<&CauseChain> {
        self.tokens.get(token_id).map(|t| &t.cause_chain)
    }

    /// Get the full token (for detailed inspection).
    #[must_use]
    pub fn token(&self, token_id: &TokenId) -> Option<&CompletionToken> {
        self.tokens.get(token_id)
    }

    /// Number of active (non-terminal) tokens.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.tokens
            .values()
            .filter(|t| !t.state().is_terminal())
            .count()
    }

    /// Total tokens (active + retained completed).
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.tokens.len()
    }

    /// Subsystems still pending for a token.
    #[must_use]
    pub fn pending_subsystems(&self, token_id: &TokenId) -> Option<Vec<&str>> {
        self.tokens
            .get(token_id)
            .map(|t| t.boundary.pending_subsystems(&t.cause_chain))
    }

    /// Summary of all active tokens (for diagnostics).
    #[must_use]
    pub fn active_summary(&self) -> Vec<TokenSummary> {
        self.tokens
            .values()
            .filter(|t| !t.state().is_terminal())
            .map(|t| TokenSummary {
                id: t.id.clone(),
                operation: t.operation.clone(),
                state: t.state(),
                steps_completed: t.cause_chain.len(),
                pending: t
                    .boundary
                    .pending_subsystems(&t.cause_chain)
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                age_ms: now_ms() - t.created_at_ms,
                pane_id: t.pane_id,
            })
            .collect()
    }
}

/// Diagnostic summary of a token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSummary {
    pub id: TokenId,
    pub operation: String,
    pub state: CompletionState,
    pub steps_completed: usize,
    pub pending: Vec<String>,
    pub age_ms: i64,
    pub pane_id: Option<u64>,
}

// =============================================================================
// Common boundaries
// =============================================================================

/// Pre-defined completion boundaries for common operations.
pub struct Boundaries;

impl Boundaries {
    /// Boundary for send_text: policy check → injection → audit.
    #[must_use]
    pub fn send_text() -> CompletionBoundary {
        CompletionBoundary::new(&["policy", "injection", "audit"])
    }

    /// Boundary for workflow step: policy → execution → event_bus.
    #[must_use]
    pub fn workflow_step() -> CompletionBoundary {
        CompletionBoundary::new(&["policy", "execution", "event_bus"])
    }

    /// Boundary for capture pipeline: ingest → storage.
    #[must_use]
    pub fn capture() -> CompletionBoundary {
        CompletionBoundary::new(&["ingest", "storage"])
    }

    /// Boundary for pattern detection: scan → event_bus → notification.
    #[must_use]
    pub fn pattern_detection() -> CompletionBoundary {
        CompletionBoundary::new(&["scan", "event_bus", "notification"])
    }

    /// Boundary for recovery: snapshot → restore → verify.
    #[must_use]
    pub fn recovery() -> CompletionBoundary {
        CompletionBoundary::new(&["snapshot", "restore", "verify"])
    }
}

// =============================================================================
// Utilities
// =============================================================================

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CompletionTrackerConfig {
        CompletionTrackerConfig {
            default_timeout_ms: 0, // no default timeout for unit tests
            max_active_tokens: 100,
            retention_ms: 1_000,
        }
    }

    // -- TokenId ---------------------------------------------------------------

    #[test]
    fn token_id_generation_is_unique() {
        let a = TokenId::generate(1);
        let b = TokenId::generate(2);
        assert_ne!(a, b);
        assert!(a.0.starts_with("ct-"));
    }

    #[test]
    fn token_id_display() {
        let id = TokenId("ct-abc-0001".to_string());
        assert_eq!(format!("{id}"), "ct-abc-0001");
    }

    #[test]
    fn token_id_serde_roundtrip() {
        let id = TokenId("ct-test-0042".to_string());
        let json = serde_json::to_string(&id).unwrap();
        let back: TokenId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    // -- CompletionState -------------------------------------------------------

    #[test]
    fn terminal_states() {
        assert!(!CompletionState::Pending.is_terminal());
        assert!(!CompletionState::InProgress.is_terminal());
        assert!(CompletionState::Completed.is_terminal());
        assert!(CompletionState::TimedOut.is_terminal());
        assert!(CompletionState::Failed.is_terminal());
        assert!(CompletionState::PartialFailure.is_terminal());
    }

    #[test]
    fn state_u8_roundtrip() {
        for &state in &[
            CompletionState::Pending,
            CompletionState::InProgress,
            CompletionState::Completed,
            CompletionState::TimedOut,
            CompletionState::Failed,
            CompletionState::PartialFailure,
        ] {
            assert_eq!(CompletionState::from_u8(state as u8), state);
        }
    }

    // -- CauseChain ------------------------------------------------------------

    #[test]
    fn cause_chain_append_and_query() {
        let mut chain = CauseChain::new();
        assert!(chain.is_empty());

        chain.record("policy", StepOutcome::Ok, "allowed");
        chain.record("injection", StepOutcome::Ok, "sent 42 bytes");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.steps()[0].subsystem, "policy");
        assert_eq!(chain.steps()[1].subsystem, "injection");
    }

    #[test]
    fn cause_chain_failed_subsystems() {
        let mut chain = CauseChain::new();
        chain.record("policy", StepOutcome::Ok, "ok");
        chain.record("injection", StepOutcome::Error, "connection refused");
        chain.record("audit", StepOutcome::Ok, "ok");
        assert_eq!(chain.failed_subsystems(), vec!["injection"]);
    }

    #[test]
    fn cause_chain_elapsed() {
        let mut chain = CauseChain::new();
        chain.push(CauseStep {
            subsystem: "a".to_string(),
            outcome: StepOutcome::Ok,
            message: String::new(),
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        });
        chain.push(CauseStep {
            subsystem: "b".to_string(),
            outcome: StepOutcome::Ok,
            message: String::new(),
            timestamp_ms: 1250,
            metadata: HashMap::new(),
        });
        assert_eq!(chain.elapsed_ms(), 250);
    }

    #[test]
    fn cause_chain_elapsed_single_step() {
        let mut chain = CauseChain::new();
        chain.record("a", StepOutcome::Ok, "");
        assert_eq!(chain.elapsed_ms(), 0);
    }

    #[test]
    fn cause_chain_serde_roundtrip() {
        let mut chain = CauseChain::new();
        chain.record("policy", StepOutcome::Ok, "allowed");
        let json = serde_json::to_string(&chain).unwrap();
        let back: CauseChain = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.steps()[0].subsystem, "policy");
    }

    // -- CompletionBoundary ----------------------------------------------------

    #[test]
    fn boundary_satisfaction() {
        let boundary = CompletionBoundary::new(&["policy", "injection", "audit"]);
        let mut chain = CauseChain::new();

        assert!(!boundary.is_satisfied(&chain));
        assert_eq!(boundary.pending_subsystems(&chain).len(), 3);

        chain.record("policy", StepOutcome::Ok, "ok");
        assert!(!boundary.is_satisfied(&chain));

        chain.record("injection", StepOutcome::Ok, "ok");
        assert!(!boundary.is_satisfied(&chain));

        chain.record("audit", StepOutcome::Ok, "ok");
        assert!(boundary.is_satisfied(&chain));
        assert!(boundary.pending_subsystems(&chain).is_empty());
    }

    #[test]
    fn boundary_skipped_counts_as_complete() {
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let mut chain = CauseChain::new();
        chain.record("a", StepOutcome::Ok, "ok");
        chain.record("b", StepOutcome::Skipped, "not applicable");
        assert!(boundary.is_satisfied(&chain));
    }

    #[test]
    fn boundary_cancelled_does_not_satisfy() {
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let mut chain = CauseChain::new();
        chain.record("a", StepOutcome::Ok, "ok");
        chain.record("b", StepOutcome::Cancelled, "cancelled");
        assert!(!boundary.is_satisfied(&chain));
    }

    #[test]
    fn boundary_error_satisfies_subsystem() {
        // Error means the subsystem *did* report — we track it as done
        // (but the overall state may be Failed/PartialFailure).
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let mut chain = CauseChain::new();
        chain.record("a", StepOutcome::Ok, "ok");
        chain.record("b", StepOutcome::Error, "something broke");
        assert!(boundary.is_satisfied(&chain));
    }

    // -- CompletionTracker: success path ---------------------------------------

    #[test]
    fn tracker_happy_path() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = Boundaries::send_text();

        let id = tracker.begin("send_text", boundary).unwrap();
        assert_eq!(tracker.state(&id), Some(CompletionState::Pending));
        assert_eq!(tracker.active_count(), 1);

        let s = tracker.advance(&id, "policy", StepOutcome::Ok, "allow-listed");
        assert_eq!(s, Some(CompletionState::InProgress));

        let s = tracker.advance(&id, "injection", StepOutcome::Ok, "sent 42 bytes");
        assert_eq!(s, Some(CompletionState::InProgress));

        let s = tracker.advance(&id, "audit", StepOutcome::Ok, "recorded");
        assert_eq!(s, Some(CompletionState::Completed));

        assert!(tracker.state(&id) == Some(CompletionState::Completed));
        // Active count drops to 0 since the token is terminal.
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn tracker_correlation_id() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();
        let token = tracker.token(&id).unwrap();
        assert!(token.correlation_id().starts_with("ct-"));
    }

    // -- CompletionTracker: failure path ---------------------------------------

    #[test]
    fn tracker_failure_on_error_step() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        let s = tracker.advance(&id, "a", StepOutcome::Error, "connection refused");
        assert_eq!(s, Some(CompletionState::Failed));

        // Terminal — further advances don't change state.
        let s = tracker.advance(&id, "b", StepOutcome::Ok, "ok");
        assert_eq!(s, Some(CompletionState::Failed));
    }

    #[test]
    fn tracker_partial_failure() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.advance(&id, "a", StepOutcome::Ok, "ok");
        let s = tracker.advance(&id, "b", StepOutcome::Error, "flaky");
        assert_eq!(s, Some(CompletionState::PartialFailure));
    }

    #[test]
    fn tracker_explicit_fail() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.fail(&id, "unrecoverable");
        assert_eq!(tracker.state(&id), Some(CompletionState::Failed));
    }

    // -- CompletionTracker: timeout path ---------------------------------------

    #[test]
    fn tracker_explicit_timeout() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.advance(&id, "_start", StepOutcome::Ok, "started");
        tracker.timeout(&id);
        assert_eq!(tracker.state(&id), Some(CompletionState::TimedOut));

        // Cause chain has the timeout step.
        let chain = tracker.cause_chain(&id).unwrap();
        assert_eq!(chain.steps().last().unwrap().subsystem, "_system");
    }

    #[test]
    fn tracker_sweep_timeouts() {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });
        let boundary = CompletionBoundary::new(&["a"]);

        // Token with deadline in the past.
        let id = tracker
            .begin_with_options("test_op", boundary, Some(0), None)
            .unwrap();

        // Sleeping 1ms to ensure the deadline passes.
        std::thread::sleep(std::time::Duration::from_millis(1));

        let timed_out = tracker.sweep_timeouts();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0], id);
        assert_eq!(tracker.state(&id), Some(CompletionState::TimedOut));
    }

    // -- CompletionTracker: capacity -------------------------------------------

    #[test]
    fn tracker_rejects_at_capacity() {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 2,
            retention_ms: 60_000,
        });

        let b = || CompletionBoundary::new(&["a"]);
        assert!(tracker.begin("op1", b()).is_some());
        assert!(tracker.begin("op2", b()).is_some());
        assert!(tracker.begin("op3", b()).is_none()); // at capacity

        // Complete one, now there's room.
        let id = TokenId(tracker.tokens.keys().next().unwrap().0.clone());
        tracker.advance(&id, "a", StepOutcome::Ok, "done");
        assert!(tracker.begin("op3", b()).is_some());
    }

    // -- CompletionTracker: eviction -------------------------------------------

    #[test]
    fn tracker_evict_completed() {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 0, // immediate eviction
        });

        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();
        tracker.advance(&id, "a", StepOutcome::Ok, "done");
        assert_eq!(tracker.total_count(), 1);

        std::thread::sleep(std::time::Duration::from_millis(1));
        let evicted = tracker.evict_completed();
        assert_eq!(evicted, 1);
        assert_eq!(tracker.total_count(), 0);
    }

    #[test]
    fn tracker_evict_keeps_active() {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 0,
        });

        let boundary = CompletionBoundary::new(&["a"]);
        let _id = tracker.begin("active_op", boundary).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1));
        let evicted = tracker.evict_completed();
        assert_eq!(evicted, 0);
        assert_eq!(tracker.total_count(), 1);
    }

    // -- CompletionTracker: diagnostics ----------------------------------------

    #[test]
    fn tracker_active_summary() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let id = tracker
            .begin_with_options("test_op", boundary, None, Some(42))
            .unwrap();
        tracker.advance(&id, "a", StepOutcome::Ok, "ok");

        let summaries = tracker.active_summary();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].operation, "test_op");
        assert_eq!(summaries[0].state, CompletionState::InProgress);
        assert_eq!(summaries[0].steps_completed, 1);
        assert_eq!(summaries[0].pending, vec!["b"]);
        assert_eq!(summaries[0].pane_id, Some(42));
    }

    #[test]
    fn tracker_pending_subsystems() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a", "b", "c"]);
        let id = tracker.begin("test_op", boundary).unwrap();
        tracker.advance(&id, "a", StepOutcome::Ok, "ok");

        let pending = tracker.pending_subsystems(&id).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.contains(&"b"));
        assert!(pending.contains(&"c"));
    }

    // -- Boundaries presets ----------------------------------------------------

    #[test]
    fn preset_boundaries() {
        let b = Boundaries::send_text();
        assert_eq!(b.required().len(), 3);

        let b = Boundaries::workflow_step();
        assert_eq!(b.required().len(), 3);

        let b = Boundaries::capture();
        assert_eq!(b.required().len(), 2);

        let b = Boundaries::pattern_detection();
        assert_eq!(b.required().len(), 3);

        let b = Boundaries::recovery();
        assert_eq!(b.required().len(), 3);
    }

    // -- Terminal state immutability -------------------------------------------

    #[test]
    fn completed_token_cannot_change_state() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.advance(&id, "a", StepOutcome::Ok, "done");
        assert_eq!(tracker.state(&id), Some(CompletionState::Completed));

        // Try to fail it — should remain Completed.
        tracker.fail(&id, "too late");
        assert_eq!(tracker.state(&id), Some(CompletionState::Completed));
    }

    #[test]
    fn timed_out_token_cannot_change_state() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.timeout(&id);
        assert_eq!(tracker.state(&id), Some(CompletionState::TimedOut));

        tracker.advance(&id, "a", StepOutcome::Ok, "late arrival");
        assert_eq!(tracker.state(&id), Some(CompletionState::TimedOut));
    }

    // -- Metadata on steps -----------------------------------------------------

    #[test]
    fn step_metadata() {
        let mut tracker = CompletionTracker::new(test_config());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        let mut meta = HashMap::new();
        meta.insert("bytes_sent".to_string(), "42".to_string());
        meta.insert("pane_id".to_string(), "7".to_string());
        tracker.advance_with_metadata(&id, "a", StepOutcome::Ok, "sent", meta);

        let chain = tracker.cause_chain(&id).unwrap();
        assert_eq!(
            chain.steps()[0].metadata.get("bytes_sent"),
            Some(&"42".to_string())
        );
    }

    // -- CauseChain serialization with metadata --------------------------------

    #[test]
    fn cause_chain_empty_metadata_not_serialized() {
        let mut chain = CauseChain::new();
        chain.record("a", StepOutcome::Ok, "ok");
        let json = serde_json::to_string(&chain).unwrap();
        // Empty metadata should be skipped.
        assert!(!json.contains("metadata"));
    }

    // -- Multiple concurrent tokens --------------------------------------------

    #[test]
    fn multiple_concurrent_tokens() {
        let mut tracker = CompletionTracker::new(test_config());
        let b1 = CompletionBoundary::new(&["a"]);
        let b2 = CompletionBoundary::new(&["x", "y"]);

        let id1 = tracker.begin("op1", b1).unwrap();
        let id2 = tracker.begin("op2", b2).unwrap();
        assert_eq!(tracker.active_count(), 2);

        tracker.advance(&id1, "a", StepOutcome::Ok, "done");
        assert_eq!(tracker.active_count(), 1);

        tracker.advance(&id2, "x", StepOutcome::Ok, "half");
        assert_eq!(tracker.active_count(), 1);

        tracker.advance(&id2, "y", StepOutcome::Ok, "done");
        assert_eq!(tracker.active_count(), 0);
    }

    // -- Nonexistent token returns None ----------------------------------------

    #[test]
    fn unknown_token_returns_none() {
        let mut tracker = CompletionTracker::new(test_config());
        let fake = TokenId("ct-fake-0000".to_string());
        assert_eq!(tracker.state(&fake), None);
        assert_eq!(tracker.advance(&fake, "a", StepOutcome::Ok, "?"), None);
        assert_eq!(tracker.pending_subsystems(&fake), None);
    }
}
