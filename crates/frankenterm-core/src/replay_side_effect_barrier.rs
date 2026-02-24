//! Side-effect isolation for deterministic replay (ft-og6q6.3.3).
//!
//! Provides a [`SideEffectBarrier`] trait that intercepts all side-effect-producing
//! calls during replay, preventing real actions while capturing a deterministic
//! [`SideEffectLog`] for decision-diff analysis.
//!
//! # Barrier Variants
//!
//! - [`LiveBarrier`] — Production: passes through to real implementations.
//! - [`ReplayBarrier`] — Replay: captures all calls, executes nothing.
//! - [`CounterfactualBarrier`] — Extends replay with override injection points.
//!
//! # Design Principle
//!
//! Replay is **analysis-only** unless an explicit future mode intentionally opts
//! into controlled execution. The barrier layer enforces this invariant.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::policy::ActionKind;

// ============================================================================
// Effect Types
// ============================================================================

/// Classification of side-effect operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectType {
    /// Send text/keystrokes to a pane.
    SendKeys,
    /// Spawn a new process or pane.
    SpawnProcess,
    /// Make an external API call.
    ApiCall,
    /// Write to the filesystem.
    FileWrite,
    /// Emit a notification (desktop, email, webhook).
    EmitNotification,
    /// Send a control character (Ctrl-C, Ctrl-D, etc.).
    SendControl,
    /// Execute an arbitrary command.
    ExecCommand,
    /// Close a pane.
    ClosePane,
}

impl EffectType {
    /// Map from [`ActionKind`] to the corresponding [`EffectType`].
    #[must_use]
    pub fn from_action_kind(kind: ActionKind) -> Self {
        match kind {
            ActionKind::SendText => Self::SendKeys,
            ActionKind::SendCtrlC
            | ActionKind::SendCtrlD
            | ActionKind::SendCtrlZ
            | ActionKind::SendControl => Self::SendControl,
            ActionKind::Spawn | ActionKind::Split => Self::SpawnProcess,
            ActionKind::Close => Self::ClosePane,
            ActionKind::BrowserAuth | ActionKind::WorkflowRun => Self::ApiCall,
            ActionKind::WriteFile | ActionKind::DeleteFile => Self::FileWrite,
            ActionKind::ExecCommand => Self::ExecCommand,
            ActionKind::ReservePane
            | ActionKind::ReleasePane
            | ActionKind::Activate
            | ActionKind::ReadOutput
            | ActionKind::SearchOutput => Self::EmitNotification,
        }
    }
}

// ============================================================================
// Side-Effect Entry
// ============================================================================

/// A single intercepted side-effect call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideEffectEntry {
    /// Monotonic index within the log.
    pub index: usize,
    /// Virtual timestamp (milliseconds) when the effect was intercepted.
    pub timestamp_ms: u64,
    /// Classification of the effect.
    pub effect_type: EffectType,
    /// Target pane (if applicable).
    pub pane_id: Option<u64>,
    /// Human-readable summary of the payload.
    pub payload_summary: String,
    /// Caller hint (module::function or similar).
    pub caller_hint: String,
    /// The original ActionKind from the policy layer.
    pub action_kind: ActionKind,
    /// Additional metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

// ============================================================================
// Side-Effect Log
// ============================================================================

/// Append-only log of intercepted side-effects.
///
/// Thread-safe via internal `Mutex`. Queryable by effect type or pane ID.
#[derive(Debug, Clone)]
pub struct SideEffectLog {
    inner: Arc<Mutex<SideEffectLogInner>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SideEffectLogInner {
    entries: Vec<SideEffectEntry>,
}

impl Default for SideEffectLog {
    fn default() -> Self {
        Self::new()
    }
}

impl SideEffectLog {
    /// Create a new, empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SideEffectLogInner::default())),
        }
    }

    /// Record a side-effect entry.
    pub fn record(&self, mut entry: SideEffectEntry) {
        let mut inner = self.inner.lock().unwrap();
        entry.index = inner.entries.len();
        inner.entries.push(entry);
    }

    /// Number of recorded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// True if no entries have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }

    /// Return all entries (snapshot).
    #[must_use]
    pub fn entries(&self) -> Vec<SideEffectEntry> {
        self.inner.lock().unwrap().entries.clone()
    }

    /// Return entries of a specific effect type.
    #[must_use]
    pub fn effects_of_type(&self, effect_type: EffectType) -> Vec<SideEffectEntry> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.effect_type == effect_type)
            .cloned()
            .collect()
    }

    /// Return entries targeting a specific pane.
    #[must_use]
    pub fn effects_for_pane(&self, pane_id: u64) -> Vec<SideEffectEntry> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.pane_id == Some(pane_id))
            .cloned()
            .collect()
    }

    /// Serialize the log to JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        let inner = self.inner.lock().unwrap();
        serde_json::to_string(&inner.entries).unwrap_or_else(|_| "[]".to_string())
    }

    /// Deserialize a log from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let entries: Vec<SideEffectEntry> = serde_json::from_str(json)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(SideEffectLogInner { entries })),
        })
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.inner.lock().unwrap().entries.clear();
    }
}

// ============================================================================
// Barrier Request
// ============================================================================

/// A request to execute a side effect, passed through the barrier.
#[derive(Debug, Clone)]
pub struct EffectRequest {
    /// Virtual timestamp of the request.
    pub timestamp_ms: u64,
    /// What kind of effect.
    pub effect_type: EffectType,
    /// Target pane (if applicable).
    pub pane_id: Option<u64>,
    /// The payload (text to send, command to run, etc.).
    pub payload: String,
    /// Caller hint for provenance.
    pub caller: String,
    /// Original ActionKind from policy layer.
    pub action_kind: ActionKind,
    /// Additional context.
    pub metadata: HashMap<String, String>,
}

/// The result of a barrier decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectOutcome {
    /// Whether the effect was executed (live) or captured (replay).
    pub executed: bool,
    /// Whether an override was applied (counterfactual).
    pub overridden: bool,
    /// Summary of the outcome.
    pub summary: String,
}

// ============================================================================
// SideEffectBarrier Trait
// ============================================================================

/// Trait for intercepting side-effect-producing operations.
///
/// Implementors decide whether to execute, capture, or override effects.
/// This trait is `Send + Sync` for async compatibility.
pub trait SideEffectBarrier: Send + Sync {
    /// Process a side-effect request.
    ///
    /// - [`LiveBarrier`]: Executes the effect and returns success/failure.
    /// - [`ReplayBarrier`]: Captures the effect in the log, returns success.
    /// - [`CounterfactualBarrier`]: Checks overrides, then delegates to base.
    fn process(&self, request: &EffectRequest) -> EffectOutcome;

    /// Return the accumulated side-effect log (if any).
    fn log(&self) -> Option<&SideEffectLog>;

    /// Return the barrier mode name for diagnostics.
    fn mode_name(&self) -> &'static str;
}

// ============================================================================
// LiveBarrier
// ============================================================================

/// Production barrier: marks all effects as executed.
///
/// In production, the actual execution happens in the caller (PolicyGatedInjector).
/// The LiveBarrier simply records that the effect was permitted and "executed"
/// from the barrier's perspective.
#[derive(Debug)]
pub struct LiveBarrier {
    log: SideEffectLog,
}

impl Default for LiveBarrier {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveBarrier {
    /// Create a new live barrier.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: SideEffectLog::new(),
        }
    }
}

impl SideEffectBarrier for LiveBarrier {
    fn process(&self, request: &EffectRequest) -> EffectOutcome {
        self.log.record(SideEffectEntry {
            index: 0, // Will be set by log.record()
            timestamp_ms: request.timestamp_ms,
            effect_type: request.effect_type,
            pane_id: request.pane_id,
            payload_summary: truncate_payload(&request.payload, 200),
            caller_hint: request.caller.clone(),
            action_kind: request.action_kind,
            metadata: request.metadata.clone(),
        });
        EffectOutcome {
            executed: true,
            overridden: false,
            summary: format!(
                "live: {} executed for pane {:?}",
                request.effect_type.as_str(),
                request.pane_id
            ),
        }
    }

    fn log(&self) -> Option<&SideEffectLog> {
        Some(&self.log)
    }

    fn mode_name(&self) -> &'static str {
        "live"
    }
}

// ============================================================================
// ReplayBarrier
// ============================================================================

/// Replay barrier: captures all effects without execution.
///
/// Every intercepted call is recorded in the [`SideEffectLog`] with full
/// provenance. No real side effects escape.
#[derive(Debug)]
pub struct ReplayBarrier {
    log: SideEffectLog,
}

impl Default for ReplayBarrier {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayBarrier {
    /// Create a new replay barrier.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: SideEffectLog::new(),
        }
    }

    /// Create a replay barrier with a shared log.
    #[must_use]
    pub fn with_log(log: SideEffectLog) -> Self {
        Self { log }
    }
}

impl SideEffectBarrier for ReplayBarrier {
    fn process(&self, request: &EffectRequest) -> EffectOutcome {
        self.log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: request.timestamp_ms,
            effect_type: request.effect_type,
            pane_id: request.pane_id,
            payload_summary: truncate_payload(&request.payload, 200),
            caller_hint: request.caller.clone(),
            action_kind: request.action_kind,
            metadata: request.metadata.clone(),
        });
        EffectOutcome {
            executed: false,
            overridden: false,
            summary: format!(
                "replay: {} captured for pane {:?}",
                request.effect_type.as_str(),
                request.pane_id
            ),
        }
    }

    fn log(&self) -> Option<&SideEffectLog> {
        Some(&self.log)
    }

    fn mode_name(&self) -> &'static str {
        "replay"
    }
}

// ============================================================================
// CounterfactualBarrier
// ============================================================================

/// An override rule for counterfactual what-if analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideRule {
    /// Which effect type this rule applies to.
    pub effect_type: EffectType,
    /// Optional pane filter (None = all panes).
    pub pane_id: Option<u64>,
    /// Optional payload pattern match (substring).
    pub payload_contains: Option<String>,
    /// The replacement payload to inject.
    pub replacement_payload: String,
    /// Human-readable description of why this override exists.
    pub description: String,
}

impl OverrideRule {
    /// Check if this rule matches a given request.
    fn matches(&self, request: &EffectRequest) -> bool {
        if self.effect_type != request.effect_type {
            return false;
        }
        if let Some(pid) = self.pane_id {
            if request.pane_id != Some(pid) {
                return false;
            }
        }
        if let Some(ref pattern) = self.payload_contains {
            if !request.payload.contains(pattern.as_str()) {
                return false;
            }
        }
        true
    }
}

/// Counterfactual barrier: extends replay with override injection.
///
/// When an effect matches an [`OverrideRule`], the override's replacement
/// payload is substituted. The original and override are both logged.
#[derive(Debug)]
pub struct CounterfactualBarrier {
    base: ReplayBarrier,
    overrides: Vec<OverrideRule>,
    override_count: Mutex<usize>,
}

impl CounterfactualBarrier {
    /// Create a new counterfactual barrier with override rules.
    #[must_use]
    pub fn new(overrides: Vec<OverrideRule>) -> Self {
        Self {
            base: ReplayBarrier::new(),
            overrides,
            override_count: Mutex::new(0),
        }
    }

    /// Create with a shared log.
    #[must_use]
    pub fn with_log(log: SideEffectLog, overrides: Vec<OverrideRule>) -> Self {
        Self {
            base: ReplayBarrier::with_log(log),
            overrides,
            override_count: Mutex::new(0),
        }
    }

    /// Number of overrides that have been applied.
    #[must_use]
    pub fn overrides_applied(&self) -> usize {
        *self.override_count.lock().unwrap()
    }
}

impl SideEffectBarrier for CounterfactualBarrier {
    fn process(&self, request: &EffectRequest) -> EffectOutcome {
        // Check if any override matches.
        let matched_override = self.overrides.iter().find(|r| r.matches(request));

        if let Some(rule) = matched_override {
            // Record the original effect with override annotation.
            let mut metadata = request.metadata.clone();
            metadata.insert(
                "override_applied".to_string(),
                rule.description.clone(),
            );
            metadata.insert(
                "original_payload".to_string(),
                truncate_payload(&request.payload, 200),
            );
            metadata.insert(
                "replacement_payload".to_string(),
                truncate_payload(&rule.replacement_payload, 200),
            );

            self.base.log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: request.timestamp_ms,
                effect_type: request.effect_type,
                pane_id: request.pane_id,
                payload_summary: truncate_payload(&rule.replacement_payload, 200),
                caller_hint: request.caller.clone(),
                action_kind: request.action_kind,
                metadata,
            });

            *self.override_count.lock().unwrap() += 1;

            EffectOutcome {
                executed: false,
                overridden: true,
                summary: format!(
                    "counterfactual: {} overridden for pane {:?} — {}",
                    request.effect_type.as_str(),
                    request.pane_id,
                    rule.description
                ),
            }
        } else {
            // No override — delegate to base replay barrier.
            self.base.process(request)
        }
    }

    fn log(&self) -> Option<&SideEffectLog> {
        Some(&self.base.log)
    }

    fn mode_name(&self) -> &'static str {
        "counterfactual"
    }
}

// ============================================================================
// Helpers
// ============================================================================

impl EffectType {
    /// Stable string representation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SendKeys => "send_keys",
            Self::SpawnProcess => "spawn_process",
            Self::ApiCall => "api_call",
            Self::FileWrite => "file_write",
            Self::EmitNotification => "emit_notification",
            Self::SendControl => "send_control",
            Self::ExecCommand => "exec_command",
            Self::ClosePane => "close_pane",
        }
    }
}

/// Truncate a payload string for summary logging.
fn truncate_payload(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(
        effect_type: EffectType,
        pane_id: Option<u64>,
        payload: &str,
    ) -> EffectRequest {
        EffectRequest {
            timestamp_ms: 1000,
            effect_type,
            pane_id,
            payload: payload.to_string(),
            caller: "test::caller".to_string(),
            action_kind: ActionKind::SendText,
            metadata: HashMap::new(),
        }
    }

    // ── SideEffectLog Tests ─────────────────────────────────────────────

    #[test]
    fn log_starts_empty() {
        let log = SideEffectLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn log_records_entry() {
        let log = SideEffectLog::new();
        log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: 100,
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_summary: "hello".to_string(),
            caller_hint: "test".to_string(),
            action_kind: ActionKind::SendText,
            metadata: HashMap::new(),
        });
        assert_eq!(log.len(), 1);
        assert!(!log.is_empty());
    }

    #[test]
    fn log_preserves_order() {
        let log = SideEffectLog::new();
        for i in 0..5 {
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: i * 100,
                effect_type: EffectType::SendKeys,
                pane_id: Some(1),
                payload_summary: format!("msg_{i}"),
                caller_hint: "test".to_string(),
                action_kind: ActionKind::SendText,
                metadata: HashMap::new(),
            });
        }
        let entries = log.entries();
        assert_eq!(entries.len(), 5);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.index, i);
            assert_eq!(entry.timestamp_ms, i as u64 * 100);
        }
    }

    #[test]
    fn log_filter_by_effect_type() {
        let log = SideEffectLog::new();
        log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: 100,
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_summary: "keys".to_string(),
            caller_hint: "test".to_string(),
            action_kind: ActionKind::SendText,
            metadata: HashMap::new(),
        });
        log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: 200,
            effect_type: EffectType::SpawnProcess,
            pane_id: Some(2),
            payload_summary: "spawn".to_string(),
            caller_hint: "test".to_string(),
            action_kind: ActionKind::Spawn,
            metadata: HashMap::new(),
        });
        log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: 300,
            effect_type: EffectType::SendKeys,
            pane_id: Some(3),
            payload_summary: "more_keys".to_string(),
            caller_hint: "test".to_string(),
            action_kind: ActionKind::SendText,
            metadata: HashMap::new(),
        });

        let keys = log.effects_of_type(EffectType::SendKeys);
        assert_eq!(keys.len(), 2);
        let spawns = log.effects_of_type(EffectType::SpawnProcess);
        assert_eq!(spawns.len(), 1);
        let api = log.effects_of_type(EffectType::ApiCall);
        assert!(api.is_empty());
    }

    #[test]
    fn log_filter_by_pane() {
        let log = SideEffectLog::new();
        for pane in [1, 2, 1, 3, 1] {
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: 100,
                effect_type: EffectType::SendKeys,
                pane_id: Some(pane),
                payload_summary: "x".to_string(),
                caller_hint: "test".to_string(),
                action_kind: ActionKind::SendText,
                metadata: HashMap::new(),
            });
        }
        assert_eq!(log.effects_for_pane(1).len(), 3);
        assert_eq!(log.effects_for_pane(2).len(), 1);
        assert_eq!(log.effects_for_pane(3).len(), 1);
        assert_eq!(log.effects_for_pane(99).len(), 0);
    }

    #[test]
    fn log_json_roundtrip() {
        let log = SideEffectLog::new();
        log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: 42,
            effect_type: EffectType::FileWrite,
            pane_id: None,
            payload_summary: "write /tmp/x".to_string(),
            caller_hint: "workflow::step3".to_string(),
            action_kind: ActionKind::WriteFile,
            metadata: HashMap::new(),
        });
        let json = log.to_json();
        let restored = SideEffectLog::from_json(&json).unwrap();
        assert_eq!(restored.len(), 1);
        let entry = &restored.entries()[0];
        assert_eq!(entry.timestamp_ms, 42);
        assert_eq!(entry.effect_type, EffectType::FileWrite);
    }

    #[test]
    fn log_empty_serializes_to_empty_array() {
        let log = SideEffectLog::new();
        assert_eq!(log.to_json(), "[]");
    }

    #[test]
    fn log_clear() {
        let log = SideEffectLog::new();
        log.record(SideEffectEntry {
            index: 0,
            timestamp_ms: 1,
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_summary: "x".to_string(),
            caller_hint: "t".to_string(),
            action_kind: ActionKind::SendText,
            metadata: HashMap::new(),
        });
        assert_eq!(log.len(), 1);
        log.clear();
        assert!(log.is_empty());
    }

    // ── EffectType Tests ────────────────────────────────────────────────

    #[test]
    fn effect_type_from_action_kind_mapping() {
        assert_eq!(
            EffectType::from_action_kind(ActionKind::SendText),
            EffectType::SendKeys
        );
        assert_eq!(
            EffectType::from_action_kind(ActionKind::SendCtrlC),
            EffectType::SendControl
        );
        assert_eq!(
            EffectType::from_action_kind(ActionKind::Spawn),
            EffectType::SpawnProcess
        );
        assert_eq!(
            EffectType::from_action_kind(ActionKind::Close),
            EffectType::ClosePane
        );
        assert_eq!(
            EffectType::from_action_kind(ActionKind::WriteFile),
            EffectType::FileWrite
        );
        assert_eq!(
            EffectType::from_action_kind(ActionKind::ExecCommand),
            EffectType::ExecCommand
        );
        assert_eq!(
            EffectType::from_action_kind(ActionKind::WorkflowRun),
            EffectType::ApiCall
        );
    }

    #[test]
    fn effect_type_as_str_unique() {
        let types = [
            EffectType::SendKeys,
            EffectType::SpawnProcess,
            EffectType::ApiCall,
            EffectType::FileWrite,
            EffectType::EmitNotification,
            EffectType::SendControl,
            EffectType::ExecCommand,
            EffectType::ClosePane,
        ];
        let strs: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
        let unique: std::collections::HashSet<&str> = strs.iter().copied().collect();
        assert_eq!(strs.len(), unique.len(), "as_str must be unique");
    }

    // ── LiveBarrier Tests ───────────────────────────────────────────────

    #[test]
    fn live_barrier_marks_executed() {
        let barrier = LiveBarrier::new();
        let req = make_request(EffectType::SendKeys, Some(1), "hello");
        let outcome = barrier.process(&req);
        assert!(outcome.executed);
        assert!(!outcome.overridden);
    }

    #[test]
    fn live_barrier_records_in_log() {
        let barrier = LiveBarrier::new();
        let req = make_request(EffectType::SpawnProcess, Some(2), "bash");
        barrier.process(&req);
        let log = barrier.log().unwrap();
        assert_eq!(log.len(), 1);
        let entry = &log.entries()[0];
        assert_eq!(entry.effect_type, EffectType::SpawnProcess);
        assert_eq!(entry.pane_id, Some(2));
    }

    #[test]
    fn live_barrier_mode_name() {
        let barrier = LiveBarrier::new();
        assert_eq!(barrier.mode_name(), "live");
    }

    // ── ReplayBarrier Tests ─────────────────────────────────────────────

    #[test]
    fn replay_barrier_does_not_execute() {
        let barrier = ReplayBarrier::new();
        let req = make_request(EffectType::SendKeys, Some(1), "hello");
        let outcome = barrier.process(&req);
        assert!(!outcome.executed);
        assert!(!outcome.overridden);
    }

    #[test]
    fn replay_barrier_captures_send_keys() {
        let barrier = ReplayBarrier::new();
        let req = make_request(EffectType::SendKeys, Some(1), "ls -la");
        barrier.process(&req);
        let log = barrier.log().unwrap();
        assert_eq!(log.len(), 1);
        let entry = &log.entries()[0];
        assert_eq!(entry.effect_type, EffectType::SendKeys);
        assert_eq!(entry.payload_summary, "ls -la");
    }

    #[test]
    fn replay_barrier_captures_spawn_process() {
        let barrier = ReplayBarrier::new();
        let req = make_request(EffectType::SpawnProcess, Some(2), "bash --login");
        barrier.process(&req);
        let entries = barrier.log().unwrap().effects_of_type(EffectType::SpawnProcess);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].payload_summary, "bash --login");
    }

    #[test]
    fn replay_barrier_captures_api_call() {
        let barrier = ReplayBarrier::new();
        let req = make_request(EffectType::ApiCall, None, "POST /api/notify");
        barrier.process(&req);
        let entries = barrier.log().unwrap().effects_of_type(EffectType::ApiCall);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn replay_barrier_captures_file_write() {
        let barrier = ReplayBarrier::new();
        let req = make_request(EffectType::FileWrite, None, "/tmp/output.json");
        barrier.process(&req);
        let entries = barrier.log().unwrap().effects_of_type(EffectType::FileWrite);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn replay_barrier_captures_notification() {
        let barrier = ReplayBarrier::new();
        let req = make_request(EffectType::EmitNotification, Some(5), "pane stuck");
        barrier.process(&req);
        let entries = barrier
            .log()
            .unwrap()
            .effects_of_type(EffectType::EmitNotification);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn replay_barrier_chronological_order() {
        let barrier = ReplayBarrier::new();
        for ts in [100, 200, 300, 150] {
            let mut req = make_request(EffectType::SendKeys, Some(1), "x");
            req.timestamp_ms = ts;
            barrier.process(&req);
        }
        let entries = barrier.log().unwrap().entries();
        // Index is monotonically increasing regardless of timestamp ordering.
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.index, i);
        }
    }

    #[test]
    fn replay_barrier_mode_name() {
        let barrier = ReplayBarrier::new();
        assert_eq!(barrier.mode_name(), "replay");
    }

    #[test]
    fn replay_barrier_shared_log() {
        let log = SideEffectLog::new();
        let barrier = ReplayBarrier::with_log(log.clone());
        let req = make_request(EffectType::SendKeys, Some(1), "test");
        barrier.process(&req);
        // The shared log should see the entry.
        assert_eq!(log.len(), 1);
    }

    // ── CounterfactualBarrier Tests ─────────────────────────────────────

    #[test]
    fn counterfactual_no_override_delegates_to_replay() {
        let barrier = CounterfactualBarrier::new(vec![]);
        let req = make_request(EffectType::SendKeys, Some(1), "hello");
        let outcome = barrier.process(&req);
        assert!(!outcome.executed);
        assert!(!outcome.overridden);
        assert_eq!(barrier.overrides_applied(), 0);
    }

    #[test]
    fn counterfactual_applies_matching_override() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_contains: None,
            replacement_payload: "replaced".to_string(),
            description: "test override".to_string(),
        };
        let barrier = CounterfactualBarrier::new(vec![rule]);
        let req = make_request(EffectType::SendKeys, Some(1), "original");
        let outcome = barrier.process(&req);
        assert!(!outcome.executed);
        assert!(outcome.overridden);
        assert_eq!(barrier.overrides_applied(), 1);

        // Log should have the replacement payload.
        let entries = barrier.log().unwrap().entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].payload_summary, "replaced");
        assert_eq!(
            entries[0].metadata.get("original_payload").unwrap(),
            "original"
        );
    }

    #[test]
    fn counterfactual_pane_filter() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_contains: None,
            replacement_payload: "override".to_string(),
            description: "pane 1 only".to_string(),
        };
        let barrier = CounterfactualBarrier::new(vec![rule]);

        // Pane 1 — should override.
        let req1 = make_request(EffectType::SendKeys, Some(1), "x");
        let out1 = barrier.process(&req1);
        assert!(out1.overridden);

        // Pane 2 — should NOT override.
        let req2 = make_request(EffectType::SendKeys, Some(2), "x");
        let out2 = barrier.process(&req2);
        assert!(!out2.overridden);

        assert_eq!(barrier.overrides_applied(), 1);
    }

    #[test]
    fn counterfactual_payload_pattern_match() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: None,
            payload_contains: Some("cargo test".to_string()),
            replacement_payload: "cargo test --release".to_string(),
            description: "add release flag".to_string(),
        };
        let barrier = CounterfactualBarrier::new(vec![rule]);

        // Matches.
        let req1 = make_request(EffectType::SendKeys, Some(1), "cargo test -p foo");
        let out1 = barrier.process(&req1);
        assert!(out1.overridden);

        // Does not match.
        let req2 = make_request(EffectType::SendKeys, Some(1), "cargo build");
        let out2 = barrier.process(&req2);
        assert!(!out2.overridden);
    }

    #[test]
    fn counterfactual_multiple_overrides_first_wins() {
        let rules = vec![
            OverrideRule {
                effect_type: EffectType::SendKeys,
                pane_id: None,
                payload_contains: None,
                replacement_payload: "first".to_string(),
                description: "first rule".to_string(),
            },
            OverrideRule {
                effect_type: EffectType::SendKeys,
                pane_id: None,
                payload_contains: None,
                replacement_payload: "second".to_string(),
                description: "second rule".to_string(),
            },
        ];
        let barrier = CounterfactualBarrier::new(rules);
        let req = make_request(EffectType::SendKeys, Some(1), "anything");
        barrier.process(&req);
        let entries = barrier.log().unwrap().entries();
        assert_eq!(entries[0].payload_summary, "first");
    }

    #[test]
    fn counterfactual_records_override_provenance() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: None,
            payload_contains: None,
            replacement_payload: "new_cmd".to_string(),
            description: "test provenance".to_string(),
        };
        let barrier = CounterfactualBarrier::new(vec![rule]);
        let req = make_request(EffectType::SendKeys, Some(1), "old_cmd");
        barrier.process(&req);
        let entry = &barrier.log().unwrap().entries()[0];
        assert!(entry.metadata.contains_key("override_applied"));
        assert!(entry.metadata.contains_key("original_payload"));
        assert!(entry.metadata.contains_key("replacement_payload"));
        assert_eq!(
            entry.metadata.get("override_applied").unwrap(),
            "test provenance"
        );
    }

    #[test]
    fn counterfactual_mode_name() {
        let barrier = CounterfactualBarrier::new(vec![]);
        assert_eq!(barrier.mode_name(), "counterfactual");
    }

    #[test]
    fn counterfactual_non_matching_effect_type() {
        let rule = OverrideRule {
            effect_type: EffectType::SpawnProcess,
            pane_id: None,
            payload_contains: None,
            replacement_payload: "override".to_string(),
            description: "spawn only".to_string(),
        };
        let barrier = CounterfactualBarrier::new(vec![rule]);
        let req = make_request(EffectType::SendKeys, Some(1), "hello");
        let outcome = barrier.process(&req);
        assert!(!outcome.overridden);
    }

    // ── OverrideRule Tests ──────────────────────────────────────────────

    #[test]
    fn override_rule_matches_exact() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_contains: Some("test".to_string()),
            replacement_payload: "r".to_string(),
            description: "d".to_string(),
        };
        let req = make_request(EffectType::SendKeys, Some(1), "run test suite");
        assert!(rule.matches(&req));
    }

    #[test]
    fn override_rule_no_match_wrong_type() {
        let rule = OverrideRule {
            effect_type: EffectType::SpawnProcess,
            pane_id: None,
            payload_contains: None,
            replacement_payload: "r".to_string(),
            description: "d".to_string(),
        };
        let req = make_request(EffectType::SendKeys, Some(1), "x");
        assert!(!rule.matches(&req));
    }

    #[test]
    fn override_rule_no_match_wrong_pane() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: Some(1),
            payload_contains: None,
            replacement_payload: "r".to_string(),
            description: "d".to_string(),
        };
        let req = make_request(EffectType::SendKeys, Some(2), "x");
        assert!(!rule.matches(&req));
    }

    #[test]
    fn override_rule_no_match_wrong_payload() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: None,
            payload_contains: Some("foo".to_string()),
            replacement_payload: "r".to_string(),
            description: "d".to_string(),
        };
        let req = make_request(EffectType::SendKeys, Some(1), "bar");
        assert!(!rule.matches(&req));
    }

    #[test]
    fn override_rule_wildcard_pane() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: None,
            payload_contains: None,
            replacement_payload: "r".to_string(),
            description: "d".to_string(),
        };
        let req = make_request(EffectType::SendKeys, Some(99), "any");
        assert!(rule.matches(&req));
    }

    // ── Barrier Send+Sync ───────────────────────────────────────────────

    #[test]
    fn barrier_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LiveBarrier>();
        assert_send_sync::<ReplayBarrier>();
        assert_send_sync::<CounterfactualBarrier>();
    }

    // ── Integration: Multiple barriers compose ──────────────────────────

    #[test]
    fn barriers_compose_via_trait_object() {
        let barriers: Vec<Box<dyn SideEffectBarrier>> = vec![
            Box::new(LiveBarrier::new()),
            Box::new(ReplayBarrier::new()),
            Box::new(CounterfactualBarrier::new(vec![])),
        ];
        let req = make_request(EffectType::SendKeys, Some(1), "test");
        let outcomes: Vec<EffectOutcome> = barriers.iter().map(|b| b.process(&req)).collect();
        assert!(outcomes[0].executed);  // LiveBarrier
        assert!(!outcomes[1].executed); // ReplayBarrier
        assert!(!outcomes[2].executed); // CounterfactualBarrier
    }

    // ── Truncation ──────────────────────────────────────────────────────

    #[test]
    fn truncate_payload_short() {
        assert_eq!(truncate_payload("hello", 10), "hello");
    }

    #[test]
    fn truncate_payload_exact() {
        assert_eq!(truncate_payload("1234567890", 10), "1234567890");
    }

    #[test]
    fn truncate_payload_long() {
        let result = truncate_payload("this is a very long string", 10);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 10);
    }

    // ── EffectOutcome / OverrideRule serde ───────────────────────────────

    #[test]
    fn effect_outcome_serde_roundtrip() {
        let outcome = EffectOutcome {
            executed: false,
            overridden: true,
            summary: "test".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: EffectOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome.executed, back.executed);
        assert_eq!(outcome.overridden, back.overridden);
        assert_eq!(outcome.summary, back.summary);
    }

    #[test]
    fn override_rule_serde_roundtrip() {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: Some(42),
            payload_contains: Some("cargo".to_string()),
            replacement_payload: "cargo check".to_string(),
            description: "test".to_string(),
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: OverrideRule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule.effect_type, back.effect_type);
        assert_eq!(rule.pane_id, back.pane_id);
        assert_eq!(rule.payload_contains, back.payload_contains);
    }

    #[test]
    fn side_effect_entry_serde_roundtrip() {
        let entry = SideEffectEntry {
            index: 0,
            timestamp_ms: 1234,
            effect_type: EffectType::SpawnProcess,
            pane_id: Some(7),
            payload_summary: "bash".to_string(),
            caller_hint: "wf::step1".to_string(),
            action_kind: ActionKind::Spawn,
            metadata: HashMap::from([("key".to_string(), "val".to_string())]),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: SideEffectEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.index, back.index);
        assert_eq!(entry.effect_type, back.effect_type);
        assert_eq!(entry.pane_id, back.pane_id);
        assert_eq!(entry.action_kind, back.action_kind);
    }

    // ── Completeness: no real effects escape ReplayBarrier ──────────────

    #[test]
    fn replay_barrier_blocks_all_effect_types() {
        let barrier = ReplayBarrier::new();
        let types = [
            EffectType::SendKeys,
            EffectType::SpawnProcess,
            EffectType::ApiCall,
            EffectType::FileWrite,
            EffectType::EmitNotification,
            EffectType::SendControl,
            EffectType::ExecCommand,
            EffectType::ClosePane,
        ];
        for et in types {
            let req = make_request(et, Some(1), "payload");
            let outcome = barrier.process(&req);
            assert!(
                !outcome.executed,
                "ReplayBarrier must not execute {:?}",
                et
            );
        }
        assert_eq!(barrier.log().unwrap().len(), types.len());
    }

    // ── Metadata preservation ───────────────────────────────────────────

    #[test]
    fn metadata_preserved_through_barrier() {
        let barrier = ReplayBarrier::new();
        let mut req = make_request(EffectType::SendKeys, Some(1), "test");
        req.metadata
            .insert("workflow_id".to_string(), "wf-123".to_string());
        req.metadata
            .insert("step".to_string(), "3".to_string());
        barrier.process(&req);
        let entry = &barrier.log().unwrap().entries()[0];
        assert_eq!(entry.metadata.get("workflow_id").unwrap(), "wf-123");
        assert_eq!(entry.metadata.get("step").unwrap(), "3");
    }

    #[test]
    fn caller_hint_preserved() {
        let barrier = ReplayBarrier::new();
        let mut req = make_request(EffectType::SendKeys, Some(1), "test");
        req.caller = "workflows::codex_exit::step2".to_string();
        barrier.process(&req);
        let entry = &barrier.log().unwrap().entries()[0];
        assert_eq!(entry.caller_hint, "workflows::codex_exit::step2");
    }
}
