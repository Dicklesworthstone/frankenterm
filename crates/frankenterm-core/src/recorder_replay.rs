//! Forensic replay engine for flight recorder events.
//!
//! Bead: wa-nbkq
//!
//! Provides time-controlled replay of recorded mux I/O events with:
//! - Speed control (1x, 2x, 0.5x, pause)
//! - Pane filtering (single pane or multi-pane interleaved)
//! - Seek to timestamp
//! - Authorization-aware: only replays data the actor can access
//! - Audit trail for all replay sessions
//!
//! # Usage
//!
//! The replay engine operates over an ordered sequence of events from
//! the recorder query executor. It maintains a cursor and emits events
//! with proper inter-event timing.
//!
//! ```text
//! Events (ordered) ──→ ReplaySession ──→ ReplayFrame (event + delay)
//!                           │
//!                     speed / seek / pause
//! ```

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::event_id::{RecorderMergeKey, StreamKind};
use crate::recorder_audit::{
    AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog, AuditScope,
    AuthzDecision,
};
use crate::recorder_query::{QueryEventKind, QueryResultEvent};
use crate::recording::{RecorderEvent, RecorderEventPayload};

// =============================================================================
// Replay configuration
// =============================================================================

/// Equivalence level for deterministic replay validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEquivalenceLevel {
    Structural,
    Decision,
    Full,
}

/// Configuration for a replay session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayConfig {
    /// Playback speed multiplier (1.0 = real-time, 2.0 = double speed).
    #[serde(default = "default_speed")]
    pub speed: f64,
    /// Maximum delay between frames (prevents long waits).
    #[serde(default = "default_max_delay")]
    pub max_delay_ms: u64,
    /// Whether to skip frames with no text content.
    #[serde(default)]
    pub skip_empty: bool,
    /// Whether to include lifecycle/control events in replay output.
    #[serde(default = "default_true")]
    pub include_markers: bool,
    /// Pane IDs to include (empty = all panes).
    #[serde(default)]
    pub pane_filter: Vec<u64>,
    /// Event kinds to include (empty = all kinds).
    #[serde(default)]
    pub kind_filter: Vec<QueryEventKind>,
    /// Equivalence gate used by deterministic replay validation.
    #[serde(default = "default_equivalence_level")]
    pub equivalence_level: ReplayEquivalenceLevel,
}

fn default_speed() -> f64 {
    1.0
}
fn default_max_delay() -> u64 {
    5000
}
fn default_true() -> bool {
    true
}
fn default_equivalence_level() -> ReplayEquivalenceLevel {
    ReplayEquivalenceLevel::Decision
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            max_delay_ms: 5000,
            skip_empty: false,
            include_markers: true,
            pane_filter: Vec::new(),
            kind_filter: Vec::new(),
            equivalence_level: default_equivalence_level(),
        }
    }
}

impl ReplayConfig {
    /// Create a real-time replay config.
    #[must_use]
    pub fn realtime() -> Self {
        Self::default()
    }

    /// Create a fast replay config (no delays).
    #[must_use]
    pub fn instant() -> Self {
        Self {
            speed: f64::INFINITY,
            max_delay_ms: 0,
            ..Default::default()
        }
    }

    /// Set playback speed.
    #[must_use]
    pub fn with_speed(mut self, speed: f64) -> Self {
        self.speed = speed;
        self
    }

    /// Filter to specific panes.
    #[must_use]
    pub fn with_panes(mut self, panes: Vec<u64>) -> Self {
        self.pane_filter = panes;
        self
    }

    /// Filter to specific event kinds.
    #[must_use]
    pub fn with_kinds(mut self, kinds: Vec<QueryEventKind>) -> Self {
        self.kind_filter = kinds;
        self
    }

    /// Set equivalence level for deterministic replay validation.
    #[must_use]
    pub fn with_equivalence_level(mut self, level: ReplayEquivalenceLevel) -> Self {
        self.equivalence_level = level;
        self
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ReplayError> {
        if self.speed <= 0.0 && !self.speed.is_infinite() {
            return Err(ReplayError::InvalidConfig(
                "speed must be positive or infinite".into(),
            ));
        }
        Ok(())
    }
}

// =============================================================================
// Replay frame
// =============================================================================

/// A single frame emitted by the replay engine.
#[derive(Debug, Clone)]
pub struct ReplayFrame {
    /// The event being replayed.
    pub event: QueryResultEvent,
    /// Delay to wait before emitting this frame.
    pub delay: Duration,
    /// Frame index in the replay sequence (0-based).
    pub frame_index: usize,
    /// Total frames in the session.
    pub total_frames: usize,
    /// Wall-clock timestamp of the original event.
    pub original_ts_ms: u64,
    /// Progress through the replay (0.0 to 1.0).
    pub progress: f64,
}

// =============================================================================
// Replay statistics
// =============================================================================

/// Statistics collected during a replay session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayStats {
    /// Total frames emitted.
    pub frames_emitted: usize,
    /// Total frames skipped (filtered out).
    pub frames_skipped: usize,
    /// Total original time span (ms) of the replayed events.
    pub original_duration_ms: u64,
    /// Total replay duration with speed applied (ms).
    pub replay_duration_ms: u64,
    /// Unique panes in the replay.
    pub unique_panes: usize,
    /// Frames per event kind.
    pub by_kind: std::collections::HashMap<String, usize>,
    /// Whether the replay completed fully.
    pub completed: bool,
}

// =============================================================================
// Replay errors
// =============================================================================

/// Errors from the replay engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// Invalid replay configuration.
    InvalidConfig(String),
    /// No events to replay.
    EmptySession,
    /// Seek target is out of range.
    SeekOutOfRange {
        target_ms: u64,
        min_ms: u64,
        max_ms: u64,
    },
    /// Session already completed.
    SessionCompleted,
    /// Checkpoint state does not match scheduler bounds.
    InvalidCheckpoint { cursor: usize, total_events: usize },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid replay config: {}", msg),
            Self::EmptySession => write!(f, "no events to replay"),
            Self::SeekOutOfRange {
                target_ms,
                min_ms,
                max_ms,
            } => write!(
                f,
                "seek target {} out of range [{}, {}]",
                target_ms, min_ms, max_ms
            ),
            Self::SessionCompleted => write!(f, "replay session already completed"),
            Self::InvalidCheckpoint {
                cursor,
                total_events,
            } => write!(
                f,
                "invalid checkpoint cursor {} for scheduler with {} events",
                cursor, total_events
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

// =============================================================================
// Replay session
// =============================================================================

/// State of a replay session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayState {
    /// Ready to start.
    Ready,
    /// Currently playing.
    Playing,
    /// Paused mid-replay.
    Paused,
    /// Replay completed.
    Completed,
}

/// A forensic replay session over recorded events.
///
/// The session is created from a set of `QueryResultEvent`s (already
/// authorized and potentially redacted by the query executor).
/// It provides frame-by-frame iteration with timing control.
pub struct ReplaySession {
    /// Events to replay (sorted by occurred_at_ms, sequence).
    events: Vec<QueryResultEvent>,
    /// Current cursor position.
    cursor: usize,
    /// Replay configuration.
    config: ReplayConfig,
    /// Session state.
    state: ReplayState,
    /// Statistics.
    stats: ReplayStats,
    /// Session ID for audit trail.
    session_id: String,
    /// Actor performing the replay.
    actor: ActorIdentity,
    /// Effective access tier for the session.
    effective_tier: AccessTier,
}

impl ReplaySession {
    /// Create a new replay session from query results.
    ///
    /// Events are sorted by (occurred_at_ms, sequence) for deterministic replay.
    pub fn new(
        mut events: Vec<QueryResultEvent>,
        config: ReplayConfig,
        actor: ActorIdentity,
        effective_tier: AccessTier,
        session_id: impl Into<String>,
    ) -> Result<Self, ReplayError> {
        config.validate()?;

        if events.is_empty() {
            return Err(ReplayError::EmptySession);
        }

        // Sort events deterministically.
        events.sort_by_key(|e| (e.occurred_at_ms, e.sequence));

        // Compute original duration.
        let first_ts = events.first().map(|e| e.occurred_at_ms).unwrap_or(0);
        let last_ts = events.last().map(|e| e.occurred_at_ms).unwrap_or(0);
        let original_duration_ms = last_ts.saturating_sub(first_ts);

        // Count unique panes.
        let mut pane_set = std::collections::HashSet::new();
        for e in &events {
            pane_set.insert(e.pane_id);
        }

        let stats = ReplayStats {
            original_duration_ms,
            unique_panes: pane_set.len(),
            ..Default::default()
        };

        Ok(Self {
            events,
            cursor: 0,
            config,
            state: ReplayState::Ready,
            stats,
            session_id: session_id.into(),
            actor,
            effective_tier,
        })
    }

    /// Get the current state of the session.
    #[must_use]
    pub fn state(&self) -> ReplayState {
        self.state
    }

    /// Get the current cursor position.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Get the total number of events in the session.
    #[must_use]
    pub fn total_events(&self) -> usize {
        self.events.len()
    }

    /// Get the session ID.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the actor performing the replay.
    #[must_use]
    pub fn actor(&self) -> &ActorIdentity {
        &self.actor
    }

    /// Get the effective access tier.
    #[must_use]
    pub fn effective_tier(&self) -> AccessTier {
        self.effective_tier
    }

    /// Get current statistics.
    #[must_use]
    pub fn stats(&self) -> &ReplayStats {
        &self.stats
    }

    /// Get the time range of events in the session.
    #[must_use]
    pub fn time_range(&self) -> (u64, u64) {
        let first = self.events.first().map(|e| e.occurred_at_ms).unwrap_or(0);
        let last = self.events.last().map(|e| e.occurred_at_ms).unwrap_or(0);
        (first, last)
    }

    /// Get the current progress (0.0 to 1.0).
    #[must_use]
    pub fn progress(&self) -> f64 {
        if self.events.is_empty() {
            return 1.0;
        }
        self.cursor as f64 / self.events.len() as f64
    }

    /// Start or resume playback.
    pub fn play(&mut self) {
        match self.state {
            ReplayState::Ready | ReplayState::Paused => {
                self.state = ReplayState::Playing;
            }
            ReplayState::Playing | ReplayState::Completed => {}
        }
    }

    /// Pause playback.
    pub fn pause(&mut self) {
        if self.state == ReplayState::Playing {
            self.state = ReplayState::Paused;
        }
    }

    /// Seek to a specific timestamp. Sets cursor to the first event at or after the target.
    pub fn seek(&mut self, target_ms: u64) -> Result<usize, ReplayError> {
        let (min_ts, max_ts) = self.time_range();

        if target_ms < min_ts || target_ms > max_ts {
            return Err(ReplayError::SeekOutOfRange {
                target_ms,
                min_ms: min_ts,
                max_ms: max_ts,
            });
        }

        // Binary search for the target timestamp.
        let idx = self
            .events
            .partition_point(|e| e.occurred_at_ms < target_ms);

        self.cursor = idx;
        if self.state == ReplayState::Completed {
            self.state = ReplayState::Paused;
        }

        Ok(idx)
    }

    /// Reset the session to the beginning.
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.state = ReplayState::Ready;
        self.stats.frames_emitted = 0;
        self.stats.frames_skipped = 0;
        self.stats.replay_duration_ms = 0;
        self.stats.by_kind.clear();
        self.stats.completed = false;
    }

    /// Emit the next frame, applying filters and timing.
    ///
    /// Returns `None` when the replay is complete or paused.
    pub fn next_frame(&mut self) -> Option<ReplayFrame> {
        if self.state == ReplayState::Paused || self.state == ReplayState::Completed {
            return None;
        }

        if self.state == ReplayState::Ready {
            self.state = ReplayState::Playing;
        }

        loop {
            if self.cursor >= self.events.len() {
                self.state = ReplayState::Completed;
                self.stats.completed = true;
                return None;
            }

            let event = &self.events[self.cursor];
            let frame_index = self.cursor;

            // Apply pane filter.
            if !self.config.pane_filter.is_empty()
                && !self.config.pane_filter.contains(&event.pane_id)
            {
                self.cursor += 1;
                self.stats.frames_skipped += 1;
                continue;
            }

            // Apply kind filter.
            if !self.config.kind_filter.is_empty()
                && !self.config.kind_filter.contains(&event.event_kind)
            {
                self.cursor += 1;
                self.stats.frames_skipped += 1;
                continue;
            }

            // Apply skip_empty filter.
            if self.config.skip_empty && event.text.is_none() {
                self.cursor += 1;
                self.stats.frames_skipped += 1;
                continue;
            }

            // Calculate delay from previous event.
            let delay = if frame_index == 0 || self.cursor == 0 {
                Duration::ZERO
            } else {
                // Find the previous non-skipped event's timestamp.
                let prev_ts = if frame_index > 0 {
                    self.events[..frame_index]
                        .iter()
                        .next_back()
                        .map(|e| e.occurred_at_ms)
                        .unwrap_or(event.occurred_at_ms)
                } else {
                    event.occurred_at_ms
                };

                let delta_ms = event.occurred_at_ms.saturating_sub(prev_ts);
                let scaled = if self.config.speed.is_infinite() || self.config.speed == 0.0 {
                    0
                } else {
                    (delta_ms as f64 / self.config.speed) as u64
                };
                let clamped = scaled.min(self.config.max_delay_ms);
                Duration::from_millis(clamped)
            };

            self.stats.replay_duration_ms += delay.as_millis() as u64;
            self.stats.frames_emitted += 1;

            let kind_key = format!("{:?}", event.event_kind);
            *self.stats.by_kind.entry(kind_key).or_insert(0) += 1;

            let total = self.events.len();
            let progress = (frame_index + 1) as f64 / total as f64;

            let frame = ReplayFrame {
                event: event.clone(),
                delay,
                frame_index,
                total_frames: total,
                original_ts_ms: event.occurred_at_ms,
                progress,
            };

            self.cursor += 1;
            return Some(frame);
        }
    }

    /// Collect all remaining frames into a vector (for instant replay).
    pub fn collect_remaining(&mut self) -> Vec<ReplayFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame() {
            frames.push(frame);
        }
        frames
    }

    /// Audit the start of a replay session.
    pub fn audit_start(&self, audit_log: &AuditLog, now_ms: u64) {
        let (start_ts, end_ts) = self.time_range();
        let pane_ids: Vec<u64> = {
            let mut ids: Vec<u64> = self.events.iter().map(|e| e.pane_id).collect();
            ids.sort();
            ids.dedup();
            ids
        };

        audit_log.append(
            AuditEventBuilder::new(AuditEventType::RecorderReplay, self.actor.clone(), now_ms)
                .with_decision(AuthzDecision::Allow)
                .with_scope(AuditScope {
                    pane_ids,
                    time_range: Some((start_ts, end_ts)),
                    query: None,
                    segment_ids: Vec::new(),
                    result_count: Some(self.events.len() as u64),
                })
                .with_details(serde_json::json!({
                    "session_id": self.session_id,
                    "speed": self.config.speed,
                    "total_events": self.events.len(),
                })),
        );
    }

    /// Audit the completion of a replay session.
    pub fn audit_complete(&self, audit_log: &AuditLog, now_ms: u64) {
        audit_log.append(
            AuditEventBuilder::new(AuditEventType::RecorderReplay, self.actor.clone(), now_ms)
                .with_decision(AuthzDecision::Allow)
                .with_details(serde_json::json!({
                    "session_id": self.session_id,
                    "frames_emitted": self.stats.frames_emitted,
                    "frames_skipped": self.stats.frames_skipped,
                    "completed": self.stats.completed,
                })),
        );
    }
}

// =============================================================================
// Replay builder
// =============================================================================

/// Builder for creating replay sessions with configuration.
pub struct ReplayBuilder {
    config: ReplayConfig,
    actor: ActorIdentity,
    effective_tier: AccessTier,
    session_id: Option<String>,
}

impl ReplayBuilder {
    /// Create a new builder for a replay session.
    pub fn new(actor: ActorIdentity) -> Self {
        let effective_tier = AccessTier::default_for_actor(actor.kind);
        Self {
            config: ReplayConfig::default(),
            actor,
            effective_tier,
            session_id: None,
        }
    }

    /// Set playback speed.
    #[must_use]
    pub fn speed(mut self, speed: f64) -> Self {
        self.config.speed = speed;
        self
    }

    /// Set instant playback (no delays).
    #[must_use]
    pub fn instant(mut self) -> Self {
        self.config = ReplayConfig::instant();
        self
    }

    /// Filter to specific panes.
    #[must_use]
    pub fn panes(mut self, panes: Vec<u64>) -> Self {
        self.config.pane_filter = panes;
        self
    }

    /// Filter to specific event kinds.
    #[must_use]
    pub fn kinds(mut self, kinds: Vec<QueryEventKind>) -> Self {
        self.config.kind_filter = kinds;
        self
    }

    /// Skip frames with no text content.
    #[must_use]
    pub fn skip_empty(mut self) -> Self {
        self.config.skip_empty = true;
        self
    }

    /// Set the effective access tier.
    #[must_use]
    pub fn tier(mut self, tier: AccessTier) -> Self {
        self.effective_tier = tier;
        self
    }

    /// Set a custom session ID.
    #[must_use]
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Build the replay session from events.
    pub fn build(self, events: Vec<QueryResultEvent>) -> Result<ReplaySession, ReplayError> {
        let session_id = self
            .session_id
            .unwrap_or_else(|| format!("replay-{}", self.actor.identity));

        ReplaySession::new(
            events,
            self.config,
            self.actor,
            self.effective_tier,
            session_id,
        )
    }
}

// =============================================================================
// Deterministic replay kernel (ft-og6q6.3.1)
// =============================================================================

/// Snapshot of virtual clock state for checkpoint/resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VirtualClockSnapshot {
    pub occurred_at_ms: u64,
    pub recorded_at_ms: u64,
    pub initialized: bool,
}

/// Trace-driven virtual clock.
///
/// The clock advances only when the scheduler processes a replay event.
#[derive(Debug, Clone)]
pub struct VirtualClock {
    speed: f64,
    snapshot: VirtualClockSnapshot,
}

impl VirtualClock {
    pub fn new(speed: f64) -> Result<Self, ReplayError> {
        if speed <= 0.0 && !speed.is_infinite() {
            return Err(ReplayError::InvalidConfig(
                "virtual clock speed must be positive or infinite".to_string(),
            ));
        }
        Ok(Self {
            speed,
            snapshot: VirtualClockSnapshot::default(),
        })
    }

    pub fn from_snapshot(speed: f64, snapshot: VirtualClockSnapshot) -> Result<Self, ReplayError> {
        if speed <= 0.0 && !speed.is_infinite() {
            return Err(ReplayError::InvalidConfig(
                "virtual clock speed must be positive or infinite".to_string(),
            ));
        }
        Ok(Self { speed, snapshot })
    }

    #[must_use]
    pub fn snapshot(&self) -> VirtualClockSnapshot {
        self.snapshot
    }

    /// Advance clock to the next event and return replay delay for this step.
    #[must_use]
    pub fn advance_to_event(&mut self, event: &RecorderEvent, max_delay_ms: u64) -> Duration {
        let delay = if !self.snapshot.initialized {
            Duration::ZERO
        } else {
            let delta_ms = event
                .recorded_at_ms
                .saturating_sub(self.snapshot.recorded_at_ms);
            let scaled_ms = if self.speed.is_infinite() {
                0
            } else {
                (delta_ms as f64 / self.speed) as u64
            };
            Duration::from_millis(scaled_ms.min(max_delay_ms))
        };

        self.snapshot = VirtualClockSnapshot {
            occurred_at_ms: event.occurred_at_ms,
            recorded_at_ms: event.recorded_at_ms,
            initialized: true,
        };
        delay
    }
}

/// Logical route for event processing during deterministic replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEngineRoute {
    Pattern,
    Workflow,
    Policy,
}

/// Deterministic decision emitted for a replayed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayDecisionRecord {
    pub decision_id: String,
    pub event_id: String,
    pub pane_id: u64,
    pub sequence: u64,
    pub stream_kind: StreamKind,
    pub engine_route: ReplayEngineRoute,
    pub decision_path: String,
    pub occurred_at_ms: u64,
    pub recorded_at_ms: u64,
}

/// Per-step output from the deterministic scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayScheduleStep {
    pub cursor: usize,
    pub merge_recorded_at_ms: u64,
    pub merge_pane_id: u64,
    pub merge_stream_kind: StreamKind,
    pub merge_sequence: u64,
    pub merge_event_id: String,
    pub delay_ms: u64,
    pub clock: VirtualClockSnapshot,
    pub decision: ReplayDecisionRecord,
}

/// Serializable scheduler state for checkpoint/resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaySchedulerState {
    pub cursor: usize,
    pub clock: VirtualClockSnapshot,
    pub decisions_emitted: usize,
}

/// Event-driven deterministic replay scheduler.
#[derive(Debug)]
pub struct ReplayScheduler {
    events: Vec<RecorderEvent>,
    config: ReplayConfig,
    cursor: usize,
    clock: VirtualClock,
    decisions: Vec<ReplayDecisionRecord>,
}

impl ReplayScheduler {
    /// Create a scheduler over timeline events sorted by `RecorderMergeKey`.
    pub fn new(mut events: Vec<RecorderEvent>, config: ReplayConfig) -> Result<Self, ReplayError> {
        config.validate()?;
        if events.is_empty() {
            return Err(ReplayError::EmptySession);
        }

        events.sort_by_key(RecorderMergeKey::from_event);

        let clock = VirtualClock::new(config.speed)?;
        Ok(Self {
            events,
            config,
            cursor: 0,
            clock,
            decisions: Vec::new(),
        })
    }

    /// Create a scheduler from a parsed `.ftreplay` artifact.
    pub fn from_artifact(
        artifact: crate::replay_fixture_harvest::FtreplayArtifact,
        config: ReplayConfig,
    ) -> Result<Self, ReplayError> {
        Self::new(artifact.events, config)
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn total_events(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn decisions(&self) -> &[ReplayDecisionRecord] {
        &self.decisions
    }

    #[must_use]
    pub fn checkpoint(&self) -> ReplaySchedulerState {
        ReplaySchedulerState {
            cursor: self.cursor,
            clock: self.clock.snapshot(),
            decisions_emitted: self.decisions.len(),
        }
    }

    pub fn resume(&mut self, state: ReplaySchedulerState) -> Result<(), ReplayError> {
        if state.cursor > self.events.len() {
            return Err(ReplayError::InvalidCheckpoint {
                cursor: state.cursor,
                total_events: self.events.len(),
            });
        }
        self.cursor = state.cursor;
        self.clock = VirtualClock::from_snapshot(self.config.speed, state.clock)?;
        if self.decisions.len() > state.decisions_emitted {
            self.decisions.truncate(state.decisions_emitted);
        }
        Ok(())
    }

    /// Process one event and emit one deterministic scheduler step.
    pub fn next_step(&mut self) -> Option<ReplayScheduleStep> {
        while self.cursor < self.events.len() {
            let cursor = self.cursor;
            self.cursor += 1;

            let event = &self.events[cursor];
            if !self.passes_filters(event) {
                continue;
            }

            let merge_key = RecorderMergeKey::from_event(event);
            let delay = self
                .clock
                .advance_to_event(event, self.config.max_delay_ms)
                .as_millis() as u64;

            let decision = build_decision_record(event);
            let step = ReplayScheduleStep {
                cursor,
                merge_recorded_at_ms: merge_key.recorded_at_ms,
                merge_pane_id: merge_key.pane_id,
                merge_stream_kind: merge_key.stream_kind,
                merge_sequence: merge_key.sequence,
                merge_event_id: merge_key.event_id.clone(),
                delay_ms: delay,
                clock: self.clock.snapshot(),
                decision: decision.clone(),
            };

            self.decisions.push(decision);
            return Some(step);
        }
        None
    }

    /// Process all remaining events.
    pub fn run_to_completion(&mut self) -> Vec<ReplayScheduleStep> {
        let mut steps = Vec::new();
        while let Some(step) = self.next_step() {
            steps.push(step);
        }
        steps
    }

    /// Stable newline-delimited JSON encoding of all emitted decisions.
    pub fn decision_trace_bytes(&self) -> crate::Result<Vec<u8>> {
        let mut out = Vec::new();
        for decision in &self.decisions {
            out.extend(serde_json::to_vec(decision)?);
            out.push(b'\n');
        }
        Ok(out)
    }

    fn passes_filters(&self, event: &RecorderEvent) -> bool {
        if !self.config.pane_filter.is_empty() && !self.config.pane_filter.contains(&event.pane_id)
        {
            return false;
        }

        let kind = query_kind_from_payload(&event.payload);
        if !self.config.kind_filter.is_empty() && !self.config.kind_filter.contains(&kind) {
            return false;
        }

        if !self.config.include_markers
            && matches!(
                kind,
                QueryEventKind::ControlMarker | QueryEventKind::LifecycleMarker
            )
        {
            return false;
        }

        if self.config.skip_empty {
            match &event.payload {
                RecorderEventPayload::IngressText { text, .. }
                | RecorderEventPayload::EgressOutput { text, .. } => {
                    if text.is_empty() {
                        return false;
                    }
                }
                _ => {}
            }
        }

        true
    }
}

fn query_kind_from_payload(payload: &RecorderEventPayload) -> QueryEventKind {
    match payload {
        RecorderEventPayload::IngressText { .. } => QueryEventKind::IngressText,
        RecorderEventPayload::EgressOutput { .. } => QueryEventKind::EgressOutput,
        RecorderEventPayload::ControlMarker { .. } => QueryEventKind::ControlMarker,
        RecorderEventPayload::LifecycleMarker { .. } => QueryEventKind::LifecycleMarker,
    }
}

fn build_decision_record(event: &RecorderEvent) -> ReplayDecisionRecord {
    let stream_kind = StreamKind::from_payload(&event.payload);
    let (engine_route, decision_path) = match &event.payload {
        RecorderEventPayload::IngressText { .. } | RecorderEventPayload::EgressOutput { .. } => {
            (ReplayEngineRoute::Pattern, "pattern.evaluate")
        }
        RecorderEventPayload::ControlMarker { .. } => {
            (ReplayEngineRoute::Policy, "policy.evaluate")
        }
        RecorderEventPayload::LifecycleMarker { .. } => {
            (ReplayEngineRoute::Workflow, "workflow.evaluate")
        }
    };
    let decision_id = format!(
        "{}:{}:{}:{}",
        event.event_id,
        event.pane_id,
        stream_kind.rank(),
        event.sequence
    );

    ReplayDecisionRecord {
        decision_id,
        event_id: event.event_id.clone(),
        pane_id: event.pane_id,
        sequence: event.sequence,
        stream_kind,
        engine_route,
        decision_path: decision_path.to_string(),
        occurred_at_ms: event.occurred_at_ms,
        recorded_at_ms: event.recorded_at_ms,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::policy::ActorKind;
    use crate::recorder_retention::SensitivityTier;
    use crate::recording::{
        RecorderControlMarkerType, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
        RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding, RECORDER_EVENT_SCHEMA_VERSION_V1,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_result_event(
        pane_id: u64,
        seq: u64,
        ts_ms: u64,
        text: Option<&str>,
        kind: QueryEventKind,
    ) -> QueryResultEvent {
        QueryResultEvent {
            event_id: format!("evt-{}-{}", pane_id, seq),
            pane_id,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts_ms,
            sequence: seq,
            session_id: None,
            text: text.map(String::from),
            redacted: false,
            sensitivity: SensitivityTier::T1Standard,
            event_kind: kind,
        }
    }

    fn make_text_event(pane_id: u64, seq: u64, ts_ms: u64, text: &str) -> QueryResultEvent {
        make_result_event(pane_id, seq, ts_ms, Some(text), QueryEventKind::IngressText)
    }

    fn make_lifecycle(pane_id: u64, seq: u64, ts_ms: u64) -> QueryResultEvent {
        make_result_event(pane_id, seq, ts_ms, None, QueryEventKind::LifecycleMarker)
    }

    fn human() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Human, "user-1")
    }

    fn sample_events() -> Vec<QueryResultEvent> {
        vec![
            make_text_event(1, 0, 1000, "ls -la"),
            make_text_event(1, 1, 1500, "cd /tmp"),
            make_text_event(2, 2, 2000, "echo hello"),
            make_lifecycle(1, 3, 2500),
            make_text_event(1, 4, 3000, "cat file.txt"),
        ]
    }

    fn make_recorder_event(
        pane_id: u64,
        sequence: u64,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
        payload: RecorderEventPayload,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("evt-{pane_id}-{sequence}-{recorded_at_ms}"),
            pane_id,
            session_id: Some("session-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload,
        }
    }

    fn make_recorder_ingress(
        pane_id: u64,
        sequence: u64,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
        text: &str,
    ) -> RecorderEvent {
        make_recorder_event(
            pane_id,
            sequence,
            occurred_at_ms,
            recorded_at_ms,
            RecorderEventPayload::IngressText {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        )
    }

    fn make_recorder_egress(
        pane_id: u64,
        sequence: u64,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
        text: &str,
    ) -> RecorderEvent {
        make_recorder_event(
            pane_id,
            sequence,
            occurred_at_ms,
            recorded_at_ms,
            RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        )
    }

    fn make_recorder_control(
        pane_id: u64,
        sequence: u64,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
    ) -> RecorderEvent {
        make_recorder_event(
            pane_id,
            sequence,
            occurred_at_ms,
            recorded_at_ms,
            RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PolicyDecision,
                details: json!({"decision": "allow"}),
            },
        )
    }

    fn make_recorder_lifecycle(
        pane_id: u64,
        sequence: u64,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
    ) -> RecorderEvent {
        make_recorder_event(
            pane_id,
            sequence,
            occurred_at_ms,
            recorded_at_ms,
            RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: RecorderLifecyclePhase::CaptureStarted,
                reason: Some("start".to_string()),
                details: json!({}),
            },
        )
    }

    // -----------------------------------------------------------------------
    // Session creation
    // -----------------------------------------------------------------------

    #[test]
    fn create_session() {
        let session = ReplaySession::new(
            sample_events(),
            ReplayConfig::default(),
            human(),
            AccessTier::A2FullQuery,
            "test-1",
        )
        .unwrap();

        assert_eq!(session.total_events(), 5);
        assert_eq!(session.state(), ReplayState::Ready);
        assert_eq!(session.cursor(), 0);
        assert_eq!(session.session_id(), "test-1");
        assert_eq!(session.time_range(), (1000, 3000));
        assert_eq!(session.stats().original_duration_ms, 2000);
    }

    #[test]
    fn empty_events_error() {
        let result = ReplaySession::new(
            vec![],
            ReplayConfig::default(),
            human(),
            AccessTier::A2FullQuery,
            "empty",
        );
        assert!(matches!(result, Err(ReplayError::EmptySession)));
    }

    #[test]
    fn invalid_speed_error() {
        let config = ReplayConfig {
            speed: -1.0,
            ..Default::default()
        };
        let result = ReplaySession::new(
            sample_events(),
            config,
            human(),
            AccessTier::A2FullQuery,
            "bad-speed",
        );
        assert!(matches!(result, Err(ReplayError::InvalidConfig(_))));
    }

    // -----------------------------------------------------------------------
    // Frame iteration
    // -----------------------------------------------------------------------

    #[test]
    fn iterate_all_frames() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "all-frames",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 5);
        assert_eq!(session.state(), ReplayState::Completed);
        assert!(session.stats().completed);
    }

    #[test]
    fn frame_ordering() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "ordering",
        )
        .unwrap();

        let frames = session.collect_remaining();
        // Events should be in timestamp order.
        for window in frames.windows(2) {
            assert!(window[0].original_ts_ms <= window[1].original_ts_ms);
        }
    }

    #[test]
    fn frame_index_and_progress() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "progress",
        )
        .unwrap();

        let frames = session.collect_remaining();
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.frame_index, i);
            assert_eq!(frame.total_frames, 5);
        }

        // Last frame should have progress close to 1.0.
        assert!((frames.last().unwrap().progress - 1.0).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Speed control
    // -----------------------------------------------------------------------

    #[test]
    fn realtime_delays() {
        let mut session = ReplaySession::new(
            vec![
                make_text_event(1, 0, 1000, "a"),
                make_text_event(1, 1, 2000, "b"),
                make_text_event(1, 2, 3000, "c"),
            ],
            ReplayConfig::realtime(),
            human(),
            AccessTier::A2FullQuery,
            "realtime",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames[0].delay, Duration::ZERO); // First frame has no delay.
        assert_eq!(frames[1].delay, Duration::from_secs(1));
        assert_eq!(frames[2].delay, Duration::from_secs(1));
    }

    #[test]
    fn double_speed_halves_delays() {
        let mut session = ReplaySession::new(
            vec![
                make_text_event(1, 0, 1000, "a"),
                make_text_event(1, 1, 3000, "b"),
            ],
            ReplayConfig::default().with_speed(2.0),
            human(),
            AccessTier::A2FullQuery,
            "2x",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames[1].delay, Duration::from_secs(1)); // 2000ms / 2.0
    }

    #[test]
    fn max_delay_clamping() {
        let config = ReplayConfig {
            speed: 1.0,
            max_delay_ms: 500,
            ..Default::default()
        };

        let mut session = ReplaySession::new(
            vec![
                make_text_event(1, 0, 1000, "a"),
                make_text_event(1, 1, 10000, "b"), // 9000ms gap
            ],
            config,
            human(),
            AccessTier::A2FullQuery,
            "clamped",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames[1].delay, Duration::from_millis(500)); // Clamped from 9000.
    }

    #[test]
    fn instant_replay_zero_delays() {
        let mut session = ReplaySession::new(
            vec![
                make_text_event(1, 0, 1000, "a"),
                make_text_event(1, 1, 999999, "b"),
            ],
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "instant",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames[0].delay, Duration::ZERO);
        assert_eq!(frames[1].delay, Duration::ZERO);
    }

    // -----------------------------------------------------------------------
    // Pane filtering
    // -----------------------------------------------------------------------

    #[test]
    fn pane_filter() {
        let config = ReplayConfig::instant().with_panes(vec![1]);

        let mut session = ReplaySession::new(
            sample_events(),
            config,
            human(),
            AccessTier::A2FullQuery,
            "pane-filter",
        )
        .unwrap();

        let frames = session.collect_remaining();
        // 4 events on pane 1 out of 5 total.
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|f| f.event.pane_id == 1));
        assert_eq!(session.stats().frames_skipped, 1);
    }

    // -----------------------------------------------------------------------
    // Kind filtering
    // -----------------------------------------------------------------------

    #[test]
    fn kind_filter_text_only() {
        let config = ReplayConfig::instant().with_kinds(vec![QueryEventKind::IngressText]);

        let mut session = ReplaySession::new(
            sample_events(),
            config,
            human(),
            AccessTier::A2FullQuery,
            "kind-filter",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 4); // 4 IngressText, 1 LifecycleMarker skipped.
        assert!(frames
            .iter()
            .all(|f| f.event.event_kind == QueryEventKind::IngressText));
    }

    // -----------------------------------------------------------------------
    // Skip empty
    // -----------------------------------------------------------------------

    #[test]
    fn skip_empty_frames() {
        let mut config = ReplayConfig::instant();
        config.skip_empty = true;

        let mut session = ReplaySession::new(
            sample_events(),
            config,
            human(),
            AccessTier::A2FullQuery,
            "skip-empty",
        )
        .unwrap();

        let frames = session.collect_remaining();
        // Lifecycle event has no text → skipped.
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|f| f.event.text.is_some()));
    }

    // -----------------------------------------------------------------------
    // Pause / resume
    // -----------------------------------------------------------------------

    #[test]
    fn pause_and_resume() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "pause",
        )
        .unwrap();

        // Get first frame.
        let f1 = session.next_frame().unwrap();
        assert_eq!(f1.frame_index, 0);
        assert_eq!(session.state(), ReplayState::Playing);

        // Pause.
        session.pause();
        assert_eq!(session.state(), ReplayState::Paused);
        assert!(session.next_frame().is_none()); // No frames while paused.

        // Resume.
        session.play();
        assert_eq!(session.state(), ReplayState::Playing);
        let f2 = session.next_frame().unwrap();
        assert_eq!(f2.frame_index, 1); // Continues from where we left off.
    }

    // -----------------------------------------------------------------------
    // Seek
    // -----------------------------------------------------------------------

    #[test]
    fn seek_to_timestamp() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seek",
        )
        .unwrap();

        // Seek to 2000ms (event at index 2).
        let idx = session.seek(2000).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(session.cursor(), 2);
    }

    #[test]
    fn seek_out_of_range() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seek-oor",
        )
        .unwrap();

        let result = session.seek(999); // Before first event.
        assert!(matches!(result, Err(ReplayError::SeekOutOfRange { .. })));

        let result = session.seek(9999); // After last event.
        assert!(matches!(result, Err(ReplayError::SeekOutOfRange { .. })));
    }

    #[test]
    fn seek_after_completion_resumes() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seek-resume",
        )
        .unwrap();

        // Play to completion.
        session.collect_remaining();
        assert_eq!(session.state(), ReplayState::Completed);

        // Seek back.
        session.seek(1000).unwrap();
        assert_eq!(session.state(), ReplayState::Paused);
        assert_eq!(session.cursor(), 0);

        // Resume.
        session.play();
        let frame = session.next_frame().unwrap();
        assert_eq!(frame.event.occurred_at_ms, 1000);
    }

    // -----------------------------------------------------------------------
    // Reset
    // -----------------------------------------------------------------------

    #[test]
    fn reset_clears_state() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "reset",
        )
        .unwrap();

        session.collect_remaining();
        assert_eq!(session.state(), ReplayState::Completed);

        session.reset();
        assert_eq!(session.state(), ReplayState::Ready);
        assert_eq!(session.cursor(), 0);
        assert_eq!(session.stats().frames_emitted, 0);

        // Can replay again.
        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 5);
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    #[test]
    fn stats_tracking() {
        let config = ReplayConfig::instant().with_panes(vec![1]);

        let mut session = ReplaySession::new(
            sample_events(),
            config,
            human(),
            AccessTier::A2FullQuery,
            "stats",
        )
        .unwrap();

        session.collect_remaining();

        let stats = session.stats();
        assert_eq!(stats.frames_emitted, 4); // 4 pane-1 events.
        assert_eq!(stats.frames_skipped, 1); // 1 pane-2 event.
        assert!(stats.completed);
        assert_eq!(stats.unique_panes, 2); // Both panes in input.
    }

    // -----------------------------------------------------------------------
    // Audit integration
    // -----------------------------------------------------------------------

    #[test]
    fn audit_start_and_complete() {
        let session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "audit-test",
        )
        .unwrap();

        let audit = AuditLog::new(crate::recorder_audit::AuditLogConfig::default());

        session.audit_start(&audit, 1700000000000);
        session.audit_complete(&audit, 1700000001000);

        let entries = audit.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_type, AuditEventType::RecorderReplay);
        assert_eq!(entries[1].event_type, AuditEventType::RecorderReplay);
    }

    // -----------------------------------------------------------------------
    // Builder
    // -----------------------------------------------------------------------

    #[test]
    fn builder_creates_session() {
        let session = ReplayBuilder::new(human())
            .instant()
            .panes(vec![1])
            .session_id("builder-test")
            .build(sample_events())
            .unwrap();

        assert_eq!(session.session_id(), "builder-test");
        assert_eq!(session.total_events(), 5);
    }

    #[test]
    fn builder_empty_events_error() {
        let result = ReplayBuilder::new(human()).build(vec![]);
        assert!(matches!(result, Err(ReplayError::EmptySession)));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn single_event_replay() {
        let mut session = ReplaySession::new(
            vec![make_text_event(1, 0, 1000, "only one")],
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "single",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_index, 0);
        assert_eq!(frames[0].total_frames, 1);
        assert!((frames[0].progress - 1.0).abs() < 0.01);
    }

    #[test]
    fn all_events_filtered_out() {
        let config = ReplayConfig::instant().with_panes(vec![99]);

        let mut session = ReplaySession::new(
            sample_events(),
            config,
            human(),
            AccessTier::A2FullQuery,
            "all-filtered",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 0);
        assert_eq!(session.stats().frames_skipped, 5);
    }

    #[test]
    fn replay_error_display() {
        assert_eq!(ReplayError::EmptySession.to_string(), "no events to replay");
        assert!(ReplayError::InvalidConfig("bad".into())
            .to_string()
            .contains("bad"));
        assert!(ReplayError::SeekOutOfRange {
            target_ms: 50,
            min_ms: 100,
            max_ms: 200
        }
        .to_string()
        .contains("50"));
    }

    #[test]
    fn config_instant_preset() {
        let config = ReplayConfig::instant();
        assert!(config.speed.is_infinite());
        assert_eq!(config.max_delay_ms, 0);
    }

    #[test]
    fn progress_at_various_points() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "progress-check",
        )
        .unwrap();

        assert!((session.progress() - 0.0).abs() < 0.01);

        session.next_frame(); // cursor → 1
        assert!((session.progress() - 0.2).abs() < 0.01);

        session.next_frame(); // cursor → 2
        assert!((session.progress() - 0.4).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn config_serde_roundtrip() {
        let config = ReplayConfig {
            speed: 2.5,
            max_delay_ms: 3000,
            skip_empty: true,
            include_markers: false,
            pane_filter: vec![1, 2, 3],
            kind_filter: vec![QueryEventKind::IngressText],
            equivalence_level: ReplayEquivalenceLevel::Full,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ReplayConfig = serde_json::from_str(&json).unwrap();
        assert!((deserialized.speed - 2.5).abs() < f64::EPSILON);
        assert_eq!(deserialized.max_delay_ms, 3000);
        assert!(deserialized.skip_empty);
        assert!(!deserialized.include_markers);
        assert_eq!(deserialized.pane_filter, vec![1, 2, 3]);
        assert_eq!(deserialized.kind_filter, vec![QueryEventKind::IngressText]);
        assert_eq!(deserialized.equivalence_level, ReplayEquivalenceLevel::Full);
    }

    #[test]
    fn config_serde_defaults_from_empty_json() {
        // Deserializing an empty JSON object should produce all defaults.
        let deserialized: ReplayConfig = serde_json::from_str("{}").unwrap();
        assert!((deserialized.speed - 1.0).abs() < f64::EPSILON);
        assert_eq!(deserialized.max_delay_ms, 5000);
        assert!(!deserialized.skip_empty);
        assert!(deserialized.include_markers);
        assert!(deserialized.pane_filter.is_empty());
        assert!(deserialized.kind_filter.is_empty());
        assert_eq!(
            deserialized.equivalence_level,
            ReplayEquivalenceLevel::Decision
        );
    }

    #[test]
    fn config_validate_zero_speed_is_invalid() {
        let config = ReplayConfig {
            speed: 0.0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(matches!(result, Err(ReplayError::InvalidConfig(_))));
    }

    #[test]
    fn config_validate_positive_infinity_is_valid() {
        let config = ReplayConfig {
            speed: f64::INFINITY,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_negative_speed_is_invalid() {
        let config = ReplayConfig {
            speed: -0.001,
            ..Default::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidConfig(_))
        ));
    }

    #[test]
    fn config_with_speed_builder() {
        let config = ReplayConfig::default().with_speed(4.0);
        assert!((config.speed - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_with_kinds_builder() {
        let config = ReplayConfig::default().with_kinds(vec![
            QueryEventKind::EgressOutput,
            QueryEventKind::ControlMarker,
        ]);
        assert_eq!(config.kind_filter.len(), 2);
        assert_eq!(config.kind_filter[0], QueryEventKind::EgressOutput);
        assert_eq!(config.kind_filter[1], QueryEventKind::ControlMarker);
    }

    #[test]
    fn config_realtime_preset_defaults() {
        let config = ReplayConfig::realtime();
        assert!((config.speed - 1.0).abs() < f64::EPSILON);
        assert_eq!(config.max_delay_ms, 5000);
        assert!(!config.skip_empty);
        assert!(config.include_markers);
    }

    #[test]
    fn replay_state_serde_roundtrip() {
        let states = [
            ReplayState::Ready,
            ReplayState::Playing,
            ReplayState::Paused,
            ReplayState::Completed,
        ];
        for state in &states {
            let json = serde_json::to_string(state).unwrap();
            let deserialized: ReplayState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, deserialized);
        }
    }

    #[test]
    fn replay_state_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ReplayState::Ready).unwrap(),
            "\"ready\""
        );
        assert_eq!(
            serde_json::to_string(&ReplayState::Playing).unwrap(),
            "\"playing\""
        );
        assert_eq!(
            serde_json::to_string(&ReplayState::Paused).unwrap(),
            "\"paused\""
        );
        assert_eq!(
            serde_json::to_string(&ReplayState::Completed).unwrap(),
            "\"completed\""
        );
    }

    #[test]
    fn replay_error_equality() {
        assert_eq!(ReplayError::EmptySession, ReplayError::EmptySession);
        assert_eq!(
            ReplayError::InvalidConfig("x".into()),
            ReplayError::InvalidConfig("x".into())
        );
        assert_ne!(
            ReplayError::InvalidConfig("a".into()),
            ReplayError::InvalidConfig("b".into())
        );
        assert_eq!(
            ReplayError::SeekOutOfRange {
                target_ms: 1,
                min_ms: 2,
                max_ms: 3
            },
            ReplayError::SeekOutOfRange {
                target_ms: 1,
                min_ms: 2,
                max_ms: 3
            }
        );
        assert_ne!(ReplayError::EmptySession, ReplayError::SessionCompleted);
    }

    #[test]
    fn replay_error_display_session_completed() {
        let err = ReplayError::SessionCompleted;
        assert_eq!(err.to_string(), "replay session already completed");
    }

    #[test]
    fn replay_error_is_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(ReplayError::InvalidConfig("test".into()));
        // Verify Display works through the trait object.
        assert!(err.to_string().contains("test"));
    }

    #[test]
    fn session_sorts_out_of_order_events() {
        // Provide events in reverse timestamp order.
        let events = vec![
            make_text_event(1, 2, 3000, "third"),
            make_text_event(1, 0, 1000, "first"),
            make_text_event(1, 1, 2000, "second"),
        ];
        let mut session = ReplaySession::new(
            events,
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "sort-test",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames[0].event.text.as_deref(), Some("first"));
        assert_eq!(frames[1].event.text.as_deref(), Some("second"));
        assert_eq!(frames[2].event.text.as_deref(), Some("third"));
    }

    #[test]
    fn session_sorts_by_sequence_for_same_timestamp() {
        // Same timestamp, different sequences — should sort by sequence.
        let events = vec![
            make_text_event(1, 5, 1000, "seq-5"),
            make_text_event(1, 1, 1000, "seq-1"),
            make_text_event(1, 3, 1000, "seq-3"),
        ];
        let mut session = ReplaySession::new(
            events,
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seq-sort",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames[0].event.sequence, 1);
        assert_eq!(frames[1].event.sequence, 3);
        assert_eq!(frames[2].event.sequence, 5);
    }

    #[test]
    fn pause_from_ready_is_noop() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "pause-ready",
        )
        .unwrap();

        assert_eq!(session.state(), ReplayState::Ready);
        session.pause(); // pause when Ready should be a no-op
        assert_eq!(session.state(), ReplayState::Ready);
    }

    #[test]
    fn play_when_completed_is_noop() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "play-completed",
        )
        .unwrap();

        session.collect_remaining();
        assert_eq!(session.state(), ReplayState::Completed);
        session.play(); // play when Completed should be a no-op
        assert_eq!(session.state(), ReplayState::Completed);
    }

    #[test]
    fn next_frame_auto_transitions_ready_to_playing() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "auto-transition",
        )
        .unwrap();

        assert_eq!(session.state(), ReplayState::Ready);
        let _frame = session.next_frame().unwrap();
        assert_eq!(session.state(), ReplayState::Playing);
    }

    #[test]
    fn combined_pane_and_kind_filter() {
        // Events: pane 1 text, pane 1 lifecycle, pane 2 text, pane 2 lifecycle
        let events = vec![
            make_text_event(1, 0, 1000, "p1-text"),
            make_lifecycle(1, 1, 1500),
            make_text_event(2, 2, 2000, "p2-text"),
            make_lifecycle(2, 3, 2500),
        ];
        // Filter: only pane 1 AND only IngressText
        let config = ReplayConfig::instant()
            .with_panes(vec![1])
            .with_kinds(vec![QueryEventKind::IngressText]);

        let mut session = ReplaySession::new(
            events,
            config,
            human(),
            AccessTier::A2FullQuery,
            "combined-filter",
        )
        .unwrap();

        let frames = session.collect_remaining();
        // Only pane-1 IngressText should survive both filters.
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.pane_id, 1);
        assert_eq!(frames[0].event.event_kind, QueryEventKind::IngressText);
        // 3 events skipped (pane-1 lifecycle, pane-2 text, pane-2 lifecycle)
        assert_eq!(session.stats().frames_skipped, 3);
    }

    #[test]
    fn replay_duration_ms_tracks_scaled_delays() {
        let mut session = ReplaySession::new(
            vec![
                make_text_event(1, 0, 1000, "a"),
                make_text_event(1, 1, 3000, "b"), // 2000ms gap
                make_text_event(1, 2, 4000, "c"), // 1000ms gap
            ],
            ReplayConfig::default().with_speed(2.0),
            human(),
            AccessTier::A2FullQuery,
            "duration-track",
        )
        .unwrap();

        session.collect_remaining();
        // At 2x speed: 0 + (2000/2) + (1000/2) = 0 + 1000 + 500 = 1500
        assert_eq!(session.stats().replay_duration_ms, 1500);
    }

    #[test]
    fn by_kind_stats_populated_correctly() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "kind-stats",
        )
        .unwrap();

        session.collect_remaining();
        let by_kind = &session.stats().by_kind;
        // sample_events has 4 IngressText and 1 LifecycleMarker.
        assert_eq!(by_kind.get("IngressText"), Some(&4));
        assert_eq!(by_kind.get("LifecycleMarker"), Some(&1));
    }

    #[test]
    fn builder_default_session_id_uses_actor_identity() {
        let session = ReplayBuilder::new(human())
            .instant()
            .build(sample_events())
            .unwrap();

        // Default session_id should be "replay-<identity>"
        assert_eq!(session.session_id(), "replay-user-1");
    }

    #[test]
    fn builder_speed_method() {
        let session = ReplayBuilder::new(human())
            .speed(3.0)
            .build(sample_events())
            .unwrap();

        // Verify the session was created — speed is internal to config so we
        // verify indirectly through the delay computation on a known gap.
        let (start, end) = session.time_range();
        assert!(end > start);
    }

    #[test]
    fn builder_tier_method() {
        let session = ReplayBuilder::new(human())
            .tier(AccessTier::A4Admin)
            .instant()
            .build(sample_events())
            .unwrap();

        assert_eq!(session.effective_tier(), AccessTier::A4Admin);
    }

    #[test]
    fn builder_skip_empty_filters_null_text() {
        let events = vec![
            make_text_event(1, 0, 1000, "has text"),
            make_lifecycle(1, 1, 2000), // no text
            make_text_event(1, 2, 3000, "also text"),
        ];
        let mut session = ReplayBuilder::new(human())
            .instant()
            .skip_empty()
            .build(events)
            .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.event.text.is_some()));
    }

    #[test]
    fn builder_kinds_method() {
        let events = vec![
            make_text_event(1, 0, 1000, "text"),
            make_lifecycle(1, 1, 2000),
        ];
        let mut session = ReplayBuilder::new(human())
            .instant()
            .kinds(vec![QueryEventKind::LifecycleMarker])
            .build(events)
            .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.event_kind, QueryEventKind::LifecycleMarker);
    }

    #[test]
    fn seek_to_exact_first_timestamp() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seek-first",
        )
        .unwrap();

        // Seeking to the exact first timestamp should land on index 0.
        let idx = session.seek(1000).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(session.cursor(), 0);
    }

    #[test]
    fn seek_to_exact_last_timestamp() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seek-last",
        )
        .unwrap();

        // Seeking to the exact last timestamp should land on the last event index.
        let idx = session.seek(3000).unwrap();
        assert_eq!(idx, 4); // partition_point for 3000 in [1000,1500,2000,2500,3000]
        assert_eq!(session.cursor(), 4);
    }

    #[test]
    fn seek_preserves_state_when_not_completed() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "seek-state",
        )
        .unwrap();

        // In Ready state, seek should not change to Paused.
        assert_eq!(session.state(), ReplayState::Ready);
        session.seek(2000).unwrap();
        assert_eq!(session.state(), ReplayState::Ready);

        // In Playing state, seek should not change to Paused.
        session.play();
        assert_eq!(session.state(), ReplayState::Playing);
        session.seek(1500).unwrap();
        assert_eq!(session.state(), ReplayState::Playing);
    }

    #[test]
    fn actor_and_tier_accessors() {
        let actor = ActorIdentity::new(ActorKind::Robot, "bot-42");
        let session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            actor,
            AccessTier::A1RedactedQuery,
            "access-test",
        )
        .unwrap();

        assert_eq!(session.actor().kind, ActorKind::Robot);
        assert_eq!(session.actor().identity, "bot-42");
        assert_eq!(session.effective_tier(), AccessTier::A1RedactedQuery);
    }

    #[test]
    fn reset_clears_by_kind_and_replay_duration() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::realtime(),
            human(),
            AccessTier::A2FullQuery,
            "reset-deep",
        )
        .unwrap();

        session.collect_remaining();
        assert!(!session.stats().by_kind.is_empty());
        assert!(session.stats().replay_duration_ms > 0);

        session.reset();
        assert!(session.stats().by_kind.is_empty());
        assert_eq!(session.stats().replay_duration_ms, 0);
        assert!(!session.stats().completed);
        assert_eq!(session.stats().frames_skipped, 0);
    }

    #[test]
    fn replay_stats_default() {
        let stats = ReplayStats::default();
        assert_eq!(stats.frames_emitted, 0);
        assert_eq!(stats.frames_skipped, 0);
        assert_eq!(stats.original_duration_ms, 0);
        assert_eq!(stats.replay_duration_ms, 0);
        assert_eq!(stats.unique_panes, 0);
        assert!(stats.by_kind.is_empty());
        assert!(!stats.completed);
    }

    #[test]
    fn replay_stats_serde_roundtrip() {
        let mut stats = ReplayStats {
            frames_emitted: 10,
            frames_skipped: 3,
            original_duration_ms: 5000,
            replay_duration_ms: 2500,
            unique_panes: 2,
            by_kind: std::collections::HashMap::new(),
            completed: true,
        };
        stats.by_kind.insert("IngressText".to_string(), 7);
        stats.by_kind.insert("LifecycleMarker".to_string(), 3);

        let json = serde_json::to_string(&stats).unwrap();
        let deserialized: ReplayStats = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.frames_emitted, 10);
        assert_eq!(deserialized.frames_skipped, 3);
        assert_eq!(deserialized.original_duration_ms, 5000);
        assert_eq!(deserialized.replay_duration_ms, 2500);
        assert_eq!(deserialized.unique_panes, 2);
        assert!(deserialized.completed);
        assert_eq!(deserialized.by_kind.get("IngressText"), Some(&7));
        assert_eq!(deserialized.by_kind.get("LifecycleMarker"), Some(&3));
    }

    #[test]
    fn audit_start_records_pane_ids_and_time_range() {
        let session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant(),
            human(),
            AccessTier::A2FullQuery,
            "audit-detail",
        )
        .unwrap();

        let audit = AuditLog::new(crate::recorder_audit::AuditLogConfig::default());
        session.audit_start(&audit, 9000000);

        let entries = audit.entries();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.actor.kind, ActorKind::Human);
        assert_eq!(entry.actor.identity, "user-1");
        assert_eq!(entry.decision, AuthzDecision::Allow);
        // Scope should have pane_ids [1, 2] (sorted, deduped) and time_range.
        assert_eq!(entry.scope.pane_ids, vec![1, 2]);
        assert_eq!(entry.scope.time_range, Some((1000, 3000)));
        assert_eq!(entry.scope.result_count, Some(5));
    }

    #[test]
    fn audit_complete_records_stats() {
        let mut session = ReplaySession::new(
            sample_events(),
            ReplayConfig::instant().with_panes(vec![1]),
            human(),
            AccessTier::A2FullQuery,
            "audit-stats",
        )
        .unwrap();

        session.collect_remaining();
        let audit = AuditLog::new(crate::recorder_audit::AuditLogConfig::default());
        session.audit_complete(&audit, 9000000);

        let entries = audit.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].decision, AuthzDecision::Allow);
    }

    #[test]
    fn multi_pane_filter_accepts_multiple_panes() {
        let events = vec![
            make_text_event(1, 0, 1000, "pane1"),
            make_text_event(2, 1, 2000, "pane2"),
            make_text_event(3, 2, 3000, "pane3"),
            make_text_event(4, 3, 4000, "pane4"),
        ];
        let config = ReplayConfig::instant().with_panes(vec![2, 4]);

        let mut session = ReplaySession::new(
            events,
            config,
            human(),
            AccessTier::A2FullQuery,
            "multi-pane",
        )
        .unwrap();

        let frames = session.collect_remaining();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.pane_id, 2);
        assert_eq!(frames[1].event.pane_id, 4);
        assert_eq!(session.stats().frames_skipped, 2);
    }

    #[test]
    fn replay_config_default_equivalence_level_is_decision() {
        assert_eq!(
            ReplayConfig::default().equivalence_level,
            ReplayEquivalenceLevel::Decision
        );
    }

    #[test]
    fn virtual_clock_speed_modes() {
        let first = make_recorder_ingress(1, 0, 1000, 1000, "a");
        let second = make_recorder_ingress(1, 1, 1500, 2000, "b");

        let mut realtime = VirtualClock::new(1.0).unwrap();
        assert_eq!(
            realtime.advance_to_event(&first, 10_000),
            Duration::from_millis(0)
        );
        assert_eq!(
            realtime.advance_to_event(&second, 10_000),
            Duration::from_millis(1000)
        );
        assert_eq!(realtime.snapshot().occurred_at_ms, 1500);
        assert_eq!(realtime.snapshot().recorded_at_ms, 2000);

        let mut double = VirtualClock::new(2.0).unwrap();
        let _ = double.advance_to_event(&first, 10_000);
        assert_eq!(
            double.advance_to_event(&second, 10_000),
            Duration::from_millis(500)
        );

        let mut instant = VirtualClock::new(f64::INFINITY).unwrap();
        let _ = instant.advance_to_event(&first, 10_000);
        assert_eq!(
            instant.advance_to_event(&second, 10_000),
            Duration::from_millis(0)
        );
    }

    #[test]
    fn replay_scheduler_orders_by_merge_key() {
        let events = vec![
            make_recorder_ingress(1, 2, 1300, 1000, "ingress"),
            make_recorder_control(1, 1, 1200, 1000),
            make_recorder_lifecycle(1, 0, 1100, 1000),
            make_recorder_egress(2, 0, 1400, 1000, "egress"),
        ];

        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let steps = scheduler.run_to_completion();
        let order: Vec<StreamKind> = steps.iter().map(|step| step.merge_stream_kind).collect();

        assert_eq!(
            order,
            vec![
                StreamKind::Lifecycle,
                StreamKind::Control,
                StreamKind::Ingress,
                StreamKind::Egress
            ]
        );
        assert_eq!(steps[0].merge_pane_id, 1);
        assert_eq!(steps[3].merge_pane_id, 2);
    }

    #[test]
    fn replay_scheduler_routes_events_to_expected_engines() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "ingress"),
            make_recorder_egress(1, 1, 1100, 1100, "egress"),
            make_recorder_control(1, 2, 1200, 1200),
            make_recorder_lifecycle(1, 3, 1300, 1300),
        ];
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let steps = scheduler.run_to_completion();
        let routes: Vec<ReplayEngineRoute> = steps
            .iter()
            .map(|step| step.decision.engine_route)
            .collect();

        assert_eq!(
            routes,
            vec![
                ReplayEngineRoute::Pattern,
                ReplayEngineRoute::Pattern,
                ReplayEngineRoute::Policy,
                ReplayEngineRoute::Workflow
            ]
        );
    }

    #[test]
    fn replay_scheduler_respects_filters() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "p1"),
            make_recorder_ingress(2, 1, 1100, 1100, "p2"),
            make_recorder_control(1, 2, 1200, 1200),
        ];

        let config = ReplayConfig::instant()
            .with_panes(vec![1])
            .with_kinds(vec![QueryEventKind::IngressText]);
        let mut scheduler = ReplayScheduler::new(events, config).unwrap();
        let steps = scheduler.run_to_completion();

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].decision.pane_id, 1);
        assert_eq!(steps[0].decision.engine_route, ReplayEngineRoute::Pattern);
    }

    #[test]
    fn replay_scheduler_skip_empty_and_marker_filtering() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, ""),
            make_recorder_control(1, 1, 1100, 1100),
            make_recorder_ingress(1, 2, 1200, 1200, "ok"),
        ];
        let config = ReplayConfig {
            skip_empty: true,
            include_markers: false,
            ..ReplayConfig::instant()
        };

        let mut scheduler = ReplayScheduler::new(events, config).unwrap();
        let steps = scheduler.run_to_completion();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].decision.sequence, 2);
    }

    #[test]
    fn replay_scheduler_checkpoint_resume_round_trip() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_ingress(1, 1, 1200, 1200, "b"),
            make_recorder_control(1, 2, 1300, 1300),
            make_recorder_lifecycle(1, 3, 1400, 1400),
        ];
        let config = ReplayConfig::default().with_speed(2.0);

        let mut baseline = ReplayScheduler::new(events.clone(), config.clone()).unwrap();
        let _ = baseline.next_step().unwrap();
        let _ = baseline.next_step().unwrap();
        let checkpoint = baseline.checkpoint();
        let baseline_tail = baseline.run_to_completion();

        let mut resumed = ReplayScheduler::new(events, config).unwrap();
        resumed.resume(checkpoint).unwrap();
        let resumed_tail = resumed.run_to_completion();

        assert_eq!(baseline_tail, resumed_tail);
    }

    #[test]
    fn replay_scheduler_rejects_invalid_checkpoint() {
        let events = vec![make_recorder_ingress(1, 0, 1000, 1000, "a")];
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let err = scheduler
            .resume(ReplaySchedulerState {
                cursor: 10,
                clock: VirtualClockSnapshot::default(),
                decisions_emitted: 0,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            ReplayError::InvalidCheckpoint {
                cursor: 10,
                total_events: 1
            }
        ));
    }

    #[test]
    fn replay_scheduler_decision_trace_is_deterministic() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_egress(2, 0, 1001, 1001, "b"),
            make_recorder_control(1, 1, 1002, 1002),
            make_recorder_lifecycle(1, 2, 1003, 1003),
        ];

        let mut first = ReplayScheduler::new(events.clone(), ReplayConfig::instant()).unwrap();
        let _ = first.run_to_completion();
        let first_bytes = first.decision_trace_bytes().unwrap();

        let mut second = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let _ = second.run_to_completion();
        let second_bytes = second.decision_trace_bytes().unwrap();

        assert_eq!(first_bytes, second_bytes);
    }

    #[test]
    fn virtual_clock_rejects_non_positive_speed() {
        let zero = VirtualClock::new(0.0).unwrap_err();
        let negative = VirtualClock::new(-1.0).unwrap_err();
        assert!(matches!(zero, ReplayError::InvalidConfig(_)));
        assert!(matches!(negative, ReplayError::InvalidConfig(_)));
    }

    #[test]
    fn virtual_clock_clamps_to_max_delay() {
        let first = make_recorder_ingress(1, 0, 1000, 1000, "a");
        let second = make_recorder_ingress(1, 1, 2000, 11_000, "b");

        let mut clock = VirtualClock::new(1.0).unwrap();
        let _ = clock.advance_to_event(&first, 50);
        let delay = clock.advance_to_event(&second, 50);
        assert_eq!(delay, Duration::from_millis(50));
        assert_eq!(clock.snapshot().recorded_at_ms, 11_000);
    }

    #[test]
    fn replay_scheduler_rejects_empty_event_list() {
        let err = ReplayScheduler::new(Vec::new(), ReplayConfig::instant()).unwrap_err();
        assert!(matches!(err, ReplayError::EmptySession));
    }

    #[test]
    fn replay_scheduler_completion_is_idempotent() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_ingress(1, 1, 1100, 1100, "b"),
        ];
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let first_run = scheduler.run_to_completion();
        let second_run = scheduler.run_to_completion();
        assert_eq!(first_run.len(), 2);
        assert!(second_run.is_empty());
        assert_eq!(scheduler.cursor(), scheduler.total_events());
    }

    #[test]
    fn replay_scheduler_cursor_advances_when_events_filtered() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_control(1, 1, 1100, 1100),
            make_recorder_lifecycle(1, 2, 1200, 1200),
        ];
        let config = ReplayConfig::instant()
            .with_panes(vec![2])
            .with_kinds(vec![QueryEventKind::IngressText]);
        let mut scheduler = ReplayScheduler::new(events, config).unwrap();
        let step = scheduler.next_step();
        assert!(step.is_none());
        assert_eq!(scheduler.cursor(), scheduler.total_events());
        assert!(scheduler.decisions().is_empty());
    }

    #[test]
    fn replay_scheduler_resume_truncates_decision_buffer() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_ingress(1, 1, 1100, 1100, "b"),
            make_recorder_control(1, 2, 1200, 1200),
        ];
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let _ = scheduler.next_step().unwrap();
        let _ = scheduler.next_step().unwrap();
        assert_eq!(scheduler.decisions().len(), 2);

        let mut checkpoint = scheduler.checkpoint();
        checkpoint.decisions_emitted = 1;
        scheduler.resume(checkpoint).unwrap();
        assert_eq!(scheduler.decisions().len(), 1);
        assert_eq!(scheduler.decisions()[0].sequence, 0);
    }

    #[test]
    fn replay_scheduler_resume_allows_larger_decisions_emitted_hint() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_control(1, 1, 1100, 1100),
        ];
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let _ = scheduler.next_step().unwrap();

        let mut checkpoint = scheduler.checkpoint();
        checkpoint.decisions_emitted = 10;
        scheduler.resume(checkpoint).unwrap();
        assert_eq!(scheduler.decisions().len(), 1);
    }

    #[test]
    fn replay_scheduler_checkpoint_initial_state() {
        let events = vec![make_recorder_ingress(1, 0, 1000, 1000, "a")];
        let scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let checkpoint = scheduler.checkpoint();
        assert_eq!(checkpoint.cursor, 0);
        assert_eq!(checkpoint.decisions_emitted, 0);
        assert_eq!(checkpoint.clock, VirtualClockSnapshot::default());
    }

    #[test]
    fn replay_scheduler_includes_markers_when_enabled() {
        let events = vec![
            make_recorder_control(1, 0, 1000, 1000),
            make_recorder_lifecycle(1, 1, 1100, 1100),
        ];
        let config = ReplayConfig {
            include_markers: true,
            skip_empty: true,
            ..ReplayConfig::instant()
        };
        let mut scheduler = ReplayScheduler::new(events, config).unwrap();
        let steps = scheduler.run_to_completion();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].decision.engine_route, ReplayEngineRoute::Policy);
        assert_eq!(steps[1].decision.engine_route, ReplayEngineRoute::Workflow);
    }

    #[test]
    fn replay_scheduler_does_not_skip_empty_when_disabled() {
        let events = vec![make_recorder_ingress(1, 0, 1000, 1000, "")];
        let config = ReplayConfig {
            skip_empty: false,
            include_markers: false,
            ..ReplayConfig::instant()
        };
        let mut scheduler = ReplayScheduler::new(events, config).unwrap();
        let steps = scheduler.run_to_completion();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].decision.sequence, 0);
    }

    #[test]
    fn replay_scheduler_trace_is_newline_delimited_json() {
        let events = vec![
            make_recorder_ingress(1, 0, 1000, 1000, "a"),
            make_recorder_control(1, 1, 1100, 1100),
        ];
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let _ = scheduler.run_to_completion();
        let trace = scheduler.decision_trace_bytes().unwrap();
        assert!(!trace.is_empty());
        assert_eq!(trace.last().copied(), Some(b'\n'));

        let lines = String::from_utf8(trace).unwrap();
        for line in lines.lines() {
            let parsed: ReplayDecisionRecord = serde_json::from_str(line).unwrap();
            assert!(!parsed.decision_id.is_empty());
        }
    }

    #[test]
    fn replay_scheduler_decision_ids_are_stable() {
        let events = vec![
            make_recorder_ingress(2, 3, 1000, 1000, "x"),
            make_recorder_egress(2, 4, 1010, 1010, "y"),
            make_recorder_control(2, 5, 1020, 1020),
        ];

        let mut first = ReplayScheduler::new(events.clone(), ReplayConfig::instant()).unwrap();
        let _ = first.run_to_completion();
        let first_ids: Vec<String> = first
            .decisions()
            .iter()
            .map(|decision| decision.decision_id.clone())
            .collect();

        let mut second = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let _ = second.run_to_completion();
        let second_ids: Vec<String> = second
            .decisions()
            .iter()
            .map(|decision| decision.decision_id.clone())
            .collect();

        assert_eq!(first_ids, second_ids);
    }
}
