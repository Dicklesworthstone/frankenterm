//! Type-state machine for compile-time pane lifecycle safety.
//!
//! Uses Rust's type system to make illegal pane state transitions
//! unrepresentable. A `TypedPane<State>` carries a zero-sized type marker
//! that gates which operations are available. State transitions consume the
//! old value and return a new one, preventing use-after-transition.
//!
//! # States
//!
//! ```text
//!  Creating ──→ Active ──→ Closed
//!                 │  ↑
//!                 ↓  │
//!           Snapshotting
//!                 │
//!                 ↓
//!             Restoring ──→ Active
//! ```
//!
//! - `Creating` — pane is being configured, cannot read or write terminal data
//! - `Active` — pane is live; can read, write, begin snapshot, or close
//! - `Snapshotting` — read-only capture in progress; cannot mutate or close
//! - `Restoring` — write-only restore in progress; cannot read or close
//! - `Closed` — terminal state; no operations allowed
//!
//! # Zero Runtime Cost
//!
//! All state markers are ZSTs (`PhantomData`). The generated code is
//! identical to non-generic functions — state enforcement is purely
//! compile-time.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::marker::PhantomData;

// ── State markers (zero-sized types) ────────────────────────────────

/// Pane is being created/configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Creating;

/// Pane is fully active — can read, write, snapshot, close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Active;

/// Snapshot capture is in progress — read-only access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshotting;

/// State restoration is in progress — write-only access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restoring;

/// Pane has been closed — no operations allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

// ── Sealed trait for state markers ──────────────────────────────────

mod sealed {
    pub trait PaneState {}
    impl PaneState for super::Creating {}
    impl PaneState for super::Active {}
    impl PaneState for super::Snapshotting {}
    impl PaneState for super::Restoring {}
    impl PaneState for super::Closed {}
}

/// Trait bound for valid pane states.
pub trait PaneState: sealed::PaneState + std::fmt::Debug {}
impl PaneState for Creating {}
impl PaneState for Active {}
impl PaneState for Snapshotting {}
impl PaneState for Restoring {}
impl PaneState for Closed {}

// ── Runtime state label (for serialization and logging) ─────────────

/// Runtime-visible label corresponding to type-state markers.
///
/// This enum mirrors the compile-time states for use in serialization,
/// logging, and diagnostics. It does NOT replace the compile-time
/// guarantees — it exists solely for runtime introspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateLabel {
    Creating,
    Active,
    Snapshotting,
    Restoring,
    Closed,
}

impl std::fmt::Display for StateLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateLabel::Creating => write!(f, "creating"),
            StateLabel::Active => write!(f, "active"),
            StateLabel::Snapshotting => write!(f, "snapshotting"),
            StateLabel::Restoring => write!(f, "restoring"),
            StateLabel::Closed => write!(f, "closed"),
        }
    }
}

/// Map from compile-time state marker to runtime label.
pub trait HasLabel: PaneState {
    fn label() -> StateLabel;
}

impl HasLabel for Creating {
    fn label() -> StateLabel {
        StateLabel::Creating
    }
}
impl HasLabel for Active {
    fn label() -> StateLabel {
        StateLabel::Active
    }
}
impl HasLabel for Snapshotting {
    fn label() -> StateLabel {
        StateLabel::Snapshotting
    }
}
impl HasLabel for Restoring {
    fn label() -> StateLabel {
        StateLabel::Restoring
    }
}
impl HasLabel for Closed {
    fn label() -> StateLabel {
        StateLabel::Closed
    }
}

// ── Pane inner data ─────────────────────────────────────────────────

/// Configuration for creating a pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneConfig {
    /// WezTerm pane ID.
    pub pane_id: u64,
    /// Shell command to run.
    pub shell: Option<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Human-readable title.
    pub title: Option<String>,
}

impl PaneConfig {
    /// Create a minimal config with just a pane ID.
    pub fn new(pane_id: u64) -> Self {
        Self {
            pane_id,
            shell: None,
            cwd: None,
            env: HashMap::new(),
            title: None,
        }
    }
}

/// Shared inner data for a pane (state-independent).
#[derive(Debug, Clone)]
pub struct PaneInner {
    /// Pane identifier.
    pub pane_id: u64,
    /// Shell command.
    pub shell: Option<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Human-readable title.
    pub title: Option<String>,
    /// Accumulated terminal output (for snapshot/restore).
    pub output_buffer: Vec<u8>,
    /// Number of state transitions this pane has undergone.
    pub transition_count: u32,
}

impl PaneInner {
    fn from_config(config: PaneConfig) -> Self {
        Self {
            pane_id: config.pane_id,
            shell: config.shell,
            cwd: config.cwd,
            env: config.env,
            title: config.title,
            output_buffer: Vec::new(),
            transition_count: 0,
        }
    }
}

// ── Snapshot data ───────────────────────────────────────────────────

/// Data captured during a snapshot operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotData {
    /// Pane ID at time of snapshot.
    pub pane_id: u64,
    /// Terminal output captured.
    pub output: Vec<u8>,
    /// Title at time of snapshot.
    pub title: Option<String>,
    /// CWD at time of snapshot.
    pub cwd: Option<String>,
    /// Shell at time of snapshot.
    pub shell: Option<String>,
    /// Environment at time of snapshot.
    pub env: HashMap<String, String>,
}

// ── TypedPane<S> ────────────────────────────────────────────────────

/// A pane with compile-time lifecycle enforcement.
///
/// The `S` type parameter is a zero-sized state marker. Only methods
/// appropriate for the current state are available.
pub struct TypedPane<S: PaneState> {
    inner: PaneInner,
    _state: PhantomData<S>,
}

impl<S: PaneState + HasLabel> std::fmt::Debug for TypedPane<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedPane")
            .field("pane_id", &self.inner.pane_id)
            .field("state", &S::label())
            .field("transitions", &self.inner.transition_count)
            .finish()
    }
}

// Helper: state transition (consumes self, returns new state)
impl<S: PaneState> TypedPane<S> {
    fn transition<S2: PaneState>(self) -> TypedPane<S2> {
        let mut inner = self.inner;
        inner.transition_count += 1;
        TypedPane {
            inner,
            _state: PhantomData,
        }
    }
}

// ── Shared accessors (available in all states) ──────────────────────

impl<S: PaneState + HasLabel> TypedPane<S> {
    /// Get the pane ID.
    pub fn pane_id(&self) -> u64 {
        self.inner.pane_id
    }

    /// Get the current state label (runtime reflection).
    pub fn state_label(&self) -> StateLabel {
        S::label()
    }

    /// Get the number of state transitions this pane has undergone.
    pub fn transition_count(&self) -> u32 {
        self.inner.transition_count
    }

    /// Get the pane title.
    pub fn title(&self) -> Option<&str> {
        self.inner.title.as_deref()
    }

    /// Get the current working directory.
    pub fn cwd(&self) -> Option<&str> {
        self.inner.cwd.as_deref()
    }
}

// ── Creating state ──────────────────────────────────────────────────

impl TypedPane<Creating> {
    /// Create a new pane in the `Creating` state.
    pub fn new(config: PaneConfig) -> Self {
        Self {
            inner: PaneInner::from_config(config),
            _state: PhantomData,
        }
    }

    /// Set the shell command.
    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.inner.shell = Some(shell.into());
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.inner.cwd = Some(cwd.into());
        self
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.inner.title = Some(title.into());
        self
    }

    /// Add an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner.env.insert(key.into(), value.into());
        self
    }

    /// Activate the pane. Consumes `Creating`, returns `Active`.
    pub fn activate(self) -> TypedPane<Active> {
        self.transition()
    }
}

// ── Active state ────────────────────────────────────────────────────

impl TypedPane<Active> {
    /// Read terminal output.
    pub fn get_text(&self) -> &[u8] {
        &self.inner.output_buffer
    }

    /// Get the shell command.
    pub fn shell(&self) -> Option<&str> {
        self.inner.shell.as_deref()
    }

    /// Get environment variables.
    pub fn env(&self) -> &HashMap<String, String> {
        &self.inner.env
    }

    /// Write data to the pane's output buffer.
    pub fn write_output(&mut self, data: &[u8]) {
        self.inner.output_buffer.extend_from_slice(data);
    }

    /// Update the pane title.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.inner.title = Some(title.into());
    }

    /// Update the working directory.
    pub fn set_cwd(&mut self, cwd: impl Into<String>) {
        self.inner.cwd = Some(cwd.into());
    }

    /// Begin a snapshot capture. Consumes `Active`, returns `Snapshotting`.
    pub fn begin_snapshot(self) -> TypedPane<Snapshotting> {
        self.transition()
    }

    /// Close the pane. Consumes `Active`, returns `Closed`.
    pub fn close(self) -> TypedPane<Closed> {
        self.transition()
    }
}

// ── Snapshotting state ──────────────────────────────────────────────

impl TypedPane<Snapshotting> {
    /// Read the snapshot data (read-only access).
    pub fn snapshot_data(&self) -> SnapshotData {
        SnapshotData {
            pane_id: self.inner.pane_id,
            output: self.inner.output_buffer.clone(),
            title: self.inner.title.clone(),
            cwd: self.inner.cwd.clone(),
            shell: self.inner.shell.clone(),
            env: self.inner.env.clone(),
        }
    }

    /// Finish the snapshot. Consumes `Snapshotting`, returns `Active`.
    pub fn finish_snapshot(self) -> TypedPane<Active> {
        self.transition()
    }

    /// Abort the snapshot and return to active.
    pub fn abort_snapshot(self) -> TypedPane<Active> {
        self.transition()
    }
}

// ── Restoring state ─────────────────────────────────────────────────

impl TypedPane<Restoring> {
    /// Create a restoring pane from snapshot data.
    pub fn from_snapshot(data: &SnapshotData) -> Self {
        let inner = PaneInner {
            pane_id: data.pane_id,
            shell: data.shell.clone(),
            cwd: data.cwd.clone(),
            env: data.env.clone(),
            title: data.title.clone(),
            output_buffer: data.output.clone(),
            transition_count: 0,
        };
        TypedPane {
            inner,
            _state: PhantomData,
        }
    }

    /// Write restore data to the pane.
    pub fn restore_output(&mut self, data: &[u8]) {
        self.inner.output_buffer.extend_from_slice(data);
    }

    /// Replace the entire output buffer during restore.
    pub fn set_output(&mut self, data: Vec<u8>) {
        self.inner.output_buffer = data;
    }

    /// Update environment during restore.
    pub fn restore_env(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.inner.env.insert(key.into(), value.into());
    }

    /// Finish restoration. Consumes `Restoring`, returns `Active`.
    pub fn finish_restore(self) -> TypedPane<Active> {
        self.transition()
    }

    /// Abort restoration and close the pane.
    pub fn abort_restore(self) -> TypedPane<Closed> {
        self.transition()
    }
}

// ── Closed state ────────────────────────────────────────────────────

impl TypedPane<Closed> {
    /// Check if the pane is closed (always true, for completeness).
    pub fn is_closed(&self) -> bool {
        true
    }

    /// Get the final transition count.
    pub fn final_transition_count(&self) -> u32 {
        self.inner.transition_count
    }
}

// ── Transition record (for audit/logging) ───────────────────────────

/// A record of a state transition for audit/logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRecord {
    /// Pane ID.
    pub pane_id: u64,
    /// State before transition.
    pub from: StateLabel,
    /// State after transition.
    pub to: StateLabel,
    /// Timestamp in ms.
    pub timestamp_ms: u64,
}

/// Tracks pane state transitions for audit.
#[derive(Debug, Clone, Default)]
pub struct TransitionLog {
    records: Vec<TransitionRecord>,
}

impl TransitionLog {
    /// Create an empty transition log.
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Record a transition.
    pub fn record(&mut self, pane_id: u64, from: StateLabel, to: StateLabel, timestamp_ms: u64) {
        self.records.push(TransitionRecord {
            pane_id,
            from,
            to,
            timestamp_ms,
        });
    }

    /// Get all records.
    pub fn records(&self) -> &[TransitionRecord] {
        &self.records
    }

    /// Get records for a specific pane.
    pub fn records_for_pane(&self, pane_id: u64) -> Vec<&TransitionRecord> {
        self.records
            .iter()
            .filter(|r| r.pane_id == pane_id)
            .collect()
    }

    /// Count transitions for a pane.
    pub fn count_for_pane(&self, pane_id: u64) -> usize {
        self.records.iter().filter(|r| r.pane_id == pane_id).count()
    }

    /// Total number of recorded transitions.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Clear all records.
    pub fn clear(&mut self) {
        self.records.clear();
    }
}

// ── Valid transition matrix ─────────────────────────────────────────

/// Check if a transition between two states is valid.
///
/// This encodes the same rules as the type system, but at runtime.
/// Useful for validation and testing.
pub fn is_valid_transition(from: StateLabel, to: StateLabel) -> bool {
    matches!(
        (from, to),
        (
            StateLabel::Creating | StateLabel::Snapshotting,
            StateLabel::Active
        ) | (
            StateLabel::Active,
            StateLabel::Snapshotting | StateLabel::Closed
        ) | (
            StateLabel::Restoring,
            StateLabel::Active | StateLabel::Closed
        )
    )
}

/// Get all valid transitions from a given state.
pub fn valid_transitions_from(state: StateLabel) -> Vec<StateLabel> {
    match state {
        StateLabel::Creating => vec![StateLabel::Active],
        StateLabel::Active => vec![StateLabel::Snapshotting, StateLabel::Closed],
        StateLabel::Snapshotting => vec![StateLabel::Active],
        StateLabel::Restoring => vec![StateLabel::Active, StateLabel::Closed],
        StateLabel::Closed => vec![],
    }
}

/// Get all states that can transition to a given state.
pub fn valid_transitions_to(state: StateLabel) -> Vec<StateLabel> {
    match state {
        StateLabel::Creating => vec![],
        StateLabel::Active => vec![
            StateLabel::Creating,
            StateLabel::Snapshotting,
            StateLabel::Restoring,
        ],
        StateLabel::Snapshotting => vec![StateLabel::Active],
        StateLabel::Restoring => vec![],
        StateLabel::Closed => vec![StateLabel::Active, StateLabel::Restoring],
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PaneConfig {
        PaneConfig::new(42)
    }

    #[test]
    fn create_and_activate() {
        let pane = TypedPane::new(test_config());
        assert_eq!(pane.pane_id(), 42);
        assert_eq!(pane.state_label(), StateLabel::Creating);
        assert_eq!(pane.transition_count(), 0);

        let active = pane.activate();
        assert_eq!(active.pane_id(), 42);
        assert_eq!(active.state_label(), StateLabel::Active);
        assert_eq!(active.transition_count(), 1);
    }

    #[test]
    fn builder_pattern() {
        let pane = TypedPane::new(PaneConfig::new(1))
            .with_shell("/bin/zsh")
            .with_cwd("/tmp")
            .with_title("test")
            .with_env("FOO", "bar")
            .activate();

        assert_eq!(pane.shell(), Some("/bin/zsh"));
        assert_eq!(pane.cwd(), Some("/tmp"));
        assert_eq!(pane.title(), Some("test"));
        assert_eq!(pane.env().get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn active_write_read() {
        let mut pane = TypedPane::new(test_config()).activate();
        pane.write_output(b"hello ");
        pane.write_output(b"world");
        assert_eq!(pane.get_text(), b"hello world");
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut pane = TypedPane::new(PaneConfig::new(7))
            .with_shell("/bin/bash")
            .with_title("test-pane")
            .activate();
        pane.write_output(b"data");

        let snapshotting = pane.begin_snapshot();
        assert_eq!(snapshotting.state_label(), StateLabel::Snapshotting);
        let data = snapshotting.snapshot_data();
        assert_eq!(data.pane_id, 7);
        assert_eq!(data.output, b"data");
        assert_eq!(data.shell.as_deref(), Some("/bin/bash"));

        let active = snapshotting.finish_snapshot();
        assert_eq!(active.state_label(), StateLabel::Active);
        assert_eq!(active.get_text(), b"data");
    }

    #[test]
    fn restore_from_snapshot() {
        // Create and snapshot
        let mut pane = TypedPane::new(PaneConfig::new(1))
            .with_title("original")
            .activate();
        pane.write_output(b"content");

        let snap = pane.begin_snapshot();
        let data = snap.snapshot_data();
        let _active = snap.finish_snapshot();

        // Restore
        let mut restoring = TypedPane::<Restoring>::from_snapshot(&data);
        assert_eq!(restoring.state_label(), StateLabel::Restoring);
        restoring.restore_output(b" extra");

        let restored = restoring.finish_restore();
        assert_eq!(restored.state_label(), StateLabel::Active);
        assert_eq!(restored.get_text(), b"content extra");
        assert_eq!(restored.title(), Some("original"));
    }

    #[test]
    fn close_from_active() {
        let pane = TypedPane::new(test_config()).activate();
        let closed = pane.close();
        assert!(closed.is_closed());
        assert_eq!(closed.state_label(), StateLabel::Closed);
        assert_eq!(closed.final_transition_count(), 2); // Creating→Active→Closed
    }

    #[test]
    fn abort_snapshot() {
        let pane = TypedPane::new(test_config()).activate();
        let snapshotting = pane.begin_snapshot();
        let active = snapshotting.abort_snapshot();
        assert_eq!(active.state_label(), StateLabel::Active);
    }

    #[test]
    fn abort_restore() {
        let data = SnapshotData {
            pane_id: 1,
            output: vec![],
            title: None,
            cwd: None,
            shell: None,
            env: HashMap::new(),
        };
        let restoring = TypedPane::<Restoring>::from_snapshot(&data);
        let closed = restoring.abort_restore();
        assert!(closed.is_closed());
    }

    #[test]
    fn transition_count_tracks() {
        let pane = TypedPane::new(test_config());
        assert_eq!(pane.transition_count(), 0);

        let active = pane.activate(); // 1
        assert_eq!(active.transition_count(), 1);

        let snap = active.begin_snapshot(); // 2
        assert_eq!(snap.transition_count(), 2);

        let active = snap.finish_snapshot(); // 3
        assert_eq!(active.transition_count(), 3);

        let closed = active.close(); // 4
        assert_eq!(closed.final_transition_count(), 4);
    }

    #[test]
    fn state_label_display() {
        assert_eq!(StateLabel::Creating.to_string(), "creating");
        assert_eq!(StateLabel::Active.to_string(), "active");
        assert_eq!(StateLabel::Snapshotting.to_string(), "snapshotting");
        assert_eq!(StateLabel::Restoring.to_string(), "restoring");
        assert_eq!(StateLabel::Closed.to_string(), "closed");
    }

    #[test]
    fn state_label_serde() {
        for label in &[
            StateLabel::Creating,
            StateLabel::Active,
            StateLabel::Snapshotting,
            StateLabel::Restoring,
            StateLabel::Closed,
        ] {
            let json = serde_json::to_string(label).unwrap();
            let back: StateLabel = serde_json::from_str(&json).unwrap();
            assert_eq!(*label, back);
        }
    }

    #[test]
    fn valid_transitions() {
        assert!(is_valid_transition(
            StateLabel::Creating,
            StateLabel::Active
        ));
        assert!(is_valid_transition(
            StateLabel::Active,
            StateLabel::Snapshotting
        ));
        assert!(is_valid_transition(StateLabel::Active, StateLabel::Closed));
        assert!(is_valid_transition(
            StateLabel::Snapshotting,
            StateLabel::Active
        ));
        assert!(is_valid_transition(
            StateLabel::Restoring,
            StateLabel::Active
        ));
        assert!(is_valid_transition(
            StateLabel::Restoring,
            StateLabel::Closed
        ));

        // Invalid
        assert!(!is_valid_transition(
            StateLabel::Creating,
            StateLabel::Closed
        ));
        assert!(!is_valid_transition(
            StateLabel::Snapshotting,
            StateLabel::Closed
        ));
        assert!(!is_valid_transition(StateLabel::Closed, StateLabel::Active));
        assert!(!is_valid_transition(
            StateLabel::Creating,
            StateLabel::Snapshotting
        ));
    }

    #[test]
    fn valid_transitions_from_fn() {
        assert_eq!(
            valid_transitions_from(StateLabel::Creating),
            vec![StateLabel::Active]
        );
        assert_eq!(
            valid_transitions_from(StateLabel::Active),
            vec![StateLabel::Snapshotting, StateLabel::Closed]
        );
        assert_eq!(
            valid_transitions_from(StateLabel::Snapshotting),
            vec![StateLabel::Active]
        );
        assert_eq!(
            valid_transitions_from(StateLabel::Restoring),
            vec![StateLabel::Active, StateLabel::Closed]
        );
        assert_eq!(valid_transitions_from(StateLabel::Closed), vec![]);
    }

    #[test]
    fn valid_transitions_to_fn() {
        assert_eq!(valid_transitions_to(StateLabel::Creating), vec![]);
        assert_eq!(
            valid_transitions_to(StateLabel::Active),
            vec![
                StateLabel::Creating,
                StateLabel::Snapshotting,
                StateLabel::Restoring
            ]
        );
        assert_eq!(
            valid_transitions_to(StateLabel::Closed),
            vec![StateLabel::Active, StateLabel::Restoring]
        );
    }

    #[test]
    fn transition_log() {
        let mut log = TransitionLog::new();
        assert!(log.is_empty());

        log.record(1, StateLabel::Creating, StateLabel::Active, 1000);
        log.record(1, StateLabel::Active, StateLabel::Closed, 2000);
        log.record(2, StateLabel::Creating, StateLabel::Active, 1500);

        assert_eq!(log.len(), 3);
        assert_eq!(log.count_for_pane(1), 2);
        assert_eq!(log.count_for_pane(2), 1);
        assert_eq!(log.records_for_pane(1).len(), 2);
    }

    #[test]
    fn transition_record_serde() {
        let record = TransitionRecord {
            pane_id: 42,
            from: StateLabel::Active,
            to: StateLabel::Closed,
            timestamp_ms: 1234,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: TransitionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn snapshot_data_serde() {
        let data = SnapshotData {
            pane_id: 1,
            output: vec![1, 2, 3],
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            shell: Some("/bin/bash".to_string()),
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SnapshotData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, back);
    }

    #[test]
    fn pane_config_serde() {
        let config = PaneConfig {
            pane_id: 5,
            shell: Some("/bin/zsh".to_string()),
            cwd: Some("/home".to_string()),
            env: HashMap::from([("A".to_string(), "B".to_string())]),
            title: Some("my pane".to_string()),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: PaneConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.pane_id, back.pane_id);
        assert_eq!(config.shell, back.shell);
    }

    #[test]
    fn typed_pane_debug() {
        let pane = TypedPane::new(test_config());
        let dbg = format!("{:?}", pane);
        assert!(dbg.contains("TypedPane"), "got: {}", dbg);
        assert!(dbg.contains("42"), "got: {}", dbg);
    }

    #[test]
    fn set_title_and_cwd() {
        let mut pane = TypedPane::new(test_config()).activate();
        pane.set_title("new title");
        pane.set_cwd("/new/cwd");
        assert_eq!(pane.title(), Some("new title"));
        assert_eq!(pane.cwd(), Some("/new/cwd"));
    }

    #[test]
    fn restoring_set_output() {
        let data = SnapshotData {
            pane_id: 1,
            output: vec![1, 2, 3],
            title: None,
            cwd: None,
            shell: None,
            env: HashMap::new(),
        };
        let mut restoring = TypedPane::<Restoring>::from_snapshot(&data);
        restoring.set_output(vec![4, 5, 6]);
        let active = restoring.finish_restore();
        assert_eq!(active.get_text(), &[4, 5, 6]);
    }

    #[test]
    fn restoring_env() {
        let data = SnapshotData {
            pane_id: 1,
            output: vec![],
            title: None,
            cwd: None,
            shell: None,
            env: HashMap::new(),
        };
        let mut restoring = TypedPane::<Restoring>::from_snapshot(&data);
        restoring.restore_env("KEY", "VALUE");
        let active = restoring.finish_restore();
        assert_eq!(active.env().get("KEY").map(String::as_str), Some("VALUE"));
    }

    #[test]
    fn transition_log_clear() {
        let mut log = TransitionLog::new();
        log.record(1, StateLabel::Creating, StateLabel::Active, 0);
        assert_eq!(log.len(), 1);
        log.clear();
        assert!(log.is_empty());
    }

    #[test]
    fn full_lifecycle() {
        // Creating → Active → Snapshotting → Active → Closed
        let pane = TypedPane::new(PaneConfig::new(99));
        let mut active = pane.activate();
        active.write_output(b"hello");
        let snap = active.begin_snapshot();
        let _data = snap.snapshot_data();
        let active = snap.finish_snapshot();
        let closed = active.close();
        assert_eq!(closed.final_transition_count(), 4);
    }

    #[test]
    fn restore_lifecycle() {
        // Restoring → Active → Closed
        let data = SnapshotData {
            pane_id: 1,
            output: b"saved".to_vec(),
            title: Some("restored".to_string()),
            cwd: None,
            shell: None,
            env: HashMap::new(),
        };
        let restoring = TypedPane::<Restoring>::from_snapshot(&data);
        let active = restoring.finish_restore();
        assert_eq!(active.get_text(), b"saved");
        assert_eq!(active.title(), Some("restored"));
        let closed = active.close();
        assert!(closed.is_closed());
    }

    // Batch: DarkBadger wa-1u90p.7.1

    #[test]
    fn state_label_hash_in_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(StateLabel::Creating);
        set.insert(StateLabel::Active);
        set.insert(StateLabel::Snapshotting);
        set.insert(StateLabel::Restoring);
        set.insert(StateLabel::Closed);
        set.insert(StateLabel::Creating); // dup
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn state_label_all_five_distinct() {
        let labels = [
            StateLabel::Creating,
            StateLabel::Active,
            StateLabel::Snapshotting,
            StateLabel::Restoring,
            StateLabel::Closed,
        ];
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn state_label_copy_semantics() {
        let a = StateLabel::Active;
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a;
        assert_eq!(a, c);
    }

    #[test]
    fn state_label_serde_snake_case_all() {
        let expected = [
            (StateLabel::Creating, "\"creating\""),
            (StateLabel::Active, "\"active\""),
            (StateLabel::Snapshotting, "\"snapshotting\""),
            (StateLabel::Restoring, "\"restoring\""),
            (StateLabel::Closed, "\"closed\""),
        ];
        for (label, json_str) in expected {
            let json = serde_json::to_string(&label).unwrap();
            assert_eq!(json, json_str);
            let back: StateLabel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, label);
        }
    }

    #[test]
    fn state_label_display_matches_serde() {
        let labels = [
            StateLabel::Creating,
            StateLabel::Active,
            StateLabel::Snapshotting,
            StateLabel::Restoring,
            StateLabel::Closed,
        ];
        for label in labels {
            let display = label.to_string();
            let serde_str = serde_json::to_string(&label).unwrap();
            // serde has quotes; display doesn't
            assert_eq!(format!("\"{}\"", display), serde_str);
        }
    }

    #[test]
    fn pane_config_debug_clone_serde() {
        let mut config = PaneConfig::new(42);
        config.env.insert("KEY".into(), "VAL".into());
        let c = config.clone();
        assert_eq!(c.pane_id, 42);
        assert_eq!(c.env.get("KEY").map(String::as_str), Some("VAL"));
        let _ = format!("{:?}", config);

        let json = serde_json::to_string(&config).unwrap();
        let back: PaneConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, 42);
        assert_eq!(back.env.get("KEY").map(String::as_str), Some("VAL"));
    }

    #[test]
    fn snapshot_data_eq_and_serde() {
        let a = SnapshotData {
            pane_id: 1,
            output: b"data".to_vec(),
            title: Some("t".into()),
            cwd: Some("/tmp".into()),
            shell: Some("/bin/sh".into()),
            env: HashMap::new(),
        };
        let b = a.clone();
        assert_eq!(a, b);

        let json = serde_json::to_string(&a).unwrap();
        let back: SnapshotData = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn snapshot_data_debug_format() {
        let s = SnapshotData {
            pane_id: 7,
            output: vec![],
            title: None,
            cwd: None,
            shell: None,
            env: HashMap::new(),
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("SnapshotData"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn transition_record_serde_roundtrip() {
        let record = TransitionRecord {
            pane_id: 5,
            from: StateLabel::Creating,
            to: StateLabel::Active,
            timestamp_ms: 12345,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: TransitionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn transition_record_debug_clone_eq() {
        let a = TransitionRecord {
            pane_id: 1,
            from: StateLabel::Active,
            to: StateLabel::Closed,
            timestamp_ms: 999,
        };
        let b = a.clone();
        assert_eq!(a, b);
        let _ = format!("{:?}", a);
    }

    #[test]
    fn transition_log_default_vs_new() {
        let a = TransitionLog::default();
        let b = TransitionLog::new();
        assert_eq!(a.len(), b.len());
        assert!(a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn transition_log_records_accessor() {
        let mut log = TransitionLog::new();
        log.record(1, StateLabel::Creating, StateLabel::Active, 100);
        log.record(2, StateLabel::Creating, StateLabel::Active, 200);
        let records = log.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].pane_id, 1);
        assert_eq!(records[1].pane_id, 2);
    }

    #[test]
    fn transition_log_records_for_nonexistent_pane() {
        let mut log = TransitionLog::new();
        log.record(1, StateLabel::Creating, StateLabel::Active, 100);
        let filtered = log.records_for_pane(999);
        assert!(filtered.is_empty());
        assert_eq!(log.count_for_pane(999), 0);
    }

    #[test]
    fn transition_log_clone_independence() {
        let mut log = TransitionLog::new();
        log.record(1, StateLabel::Creating, StateLabel::Active, 100);
        let mut cloned = log.clone();
        cloned.record(2, StateLabel::Active, StateLabel::Closed, 200);
        assert_eq!(log.len(), 1);
        assert_eq!(cloned.len(), 2);
    }

    #[test]
    fn with_env_overwrite_key() {
        let pane = TypedPane::new(PaneConfig::new(1))
            .with_env("FOO", "bar")
            .with_env("FOO", "baz")
            .activate();
        assert_eq!(pane.env().get("FOO").map(String::as_str), Some("baz"));
    }

    #[test]
    fn empty_write_output() {
        let mut pane = TypedPane::new(test_config()).activate();
        pane.write_output(b"");
        assert_eq!(pane.get_text(), b"");
        pane.write_output(b"data");
        pane.write_output(b"");
        assert_eq!(pane.get_text(), b"data");
    }

    #[test]
    fn typed_pane_debug_contains_state() {
        let pane = TypedPane::new(PaneConfig::new(77)).activate();
        let dbg = format!("{:?}", pane);
        assert!(dbg.contains("TypedPane"));
        assert!(dbg.contains("77"));
        assert!(dbg.contains("Active"));
    }

    #[test]
    fn has_label_all_five_states() {
        assert_eq!(Creating::label(), StateLabel::Creating);
        assert_eq!(Active::label(), StateLabel::Active);
        assert_eq!(Snapshotting::label(), StateLabel::Snapshotting);
        assert_eq!(Restoring::label(), StateLabel::Restoring);
        assert_eq!(Closed::label(), StateLabel::Closed);
    }
}
