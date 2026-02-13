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

use crate::recorder_audit::{
    AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog, AuditScope,
    AuthzDecision,
};
use crate::recorder_query::{QueryEventKind, QueryResultEvent};

// =============================================================================
// Replay configuration
// =============================================================================

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

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            max_delay_ms: 5000,
            skip_empty: false,
            include_markers: true,
            pane_filter: Vec::new(),
            kind_filter: Vec::new(),
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
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ActorKind;
    use crate::recorder_retention::SensitivityTier;
    use crate::recording::RecorderEventSource;

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
        assert_eq!(frames[1].delay, Duration::from_millis(1000));
        assert_eq!(frames[2].delay, Duration::from_millis(1000));
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
        assert_eq!(frames[1].delay, Duration::from_millis(1000)); // 2000ms / 2.0
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
        assert!(
            frames
                .iter()
                .all(|f| f.event.event_kind == QueryEventKind::IngressText)
        );
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
        assert!(
            ReplayError::InvalidConfig("bad".into())
                .to_string()
                .contains("bad")
        );
        assert!(
            ReplayError::SeekOutOfRange {
                target_ms: 50,
                min_ms: 100,
                max_ms: 200
            }
            .to_string()
            .contains("50")
        );
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
}
