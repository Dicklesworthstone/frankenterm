//! Recording engine for wa sessions.
//!
//! Provides a per-pane recorder that writes frame data to disk using the
//! WAR recording format (see docs/recording-format-spec.md).
//!
//! NOTE: This is the core engine only; CLI wiring lives elsewhere.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::runtime_compat::Mutex;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ingest::{CapturedSegment, CapturedSegmentKind};
use crate::patterns::Detection;
use crate::policy::Redactor;

/// Supported frame types within a recording stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FrameType {
    /// Terminal output delta.
    Output = 1,
    /// Terminal resize event.
    Resize = 2,
    /// wa detection event.
    Event = 3,
    /// User marker/annotation.
    Marker = 4,
    /// Optional captured input (redacted).
    Input = 5,
}

/// Output encoding used for output frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaEncoding {
    /// Full frame payload (no delta).
    Full(Vec<u8>),
    /// Placeholder for diff encoding (to be implemented).
    #[allow(dead_code)]
    Diff { base_frame: u32, ops: Vec<DiffOp> },
    /// Placeholder for repeat encoding (to be implemented).
    #[allow(dead_code)]
    Repeat { base_frame: u32 },
}

/// Diff operation placeholder for future delta encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffOp {
    Copy { offset: u32, len: u32 },
    Insert { data: Vec<u8> },
}

/// Frame header written to disk before payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub timestamp_ms: u64,
    pub frame_type: FrameType,
    pub flags: u8,
    pub payload_len: u32,
}

/// A single recording frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFrame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl RecordingFrame {
    /// Serialize frame into bytes (header + payload).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(14 + self.payload.len());
        out.extend_from_slice(&self.header.timestamp_ms.to_le_bytes());
        out.push(self.header.frame_type as u8);
        out.push(self.header.flags);
        out.extend_from_slice(&self.header.payload_len.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Buffered frame writer for recording output.
pub struct FrameWriter {
    buffer: Vec<RecordingFrame>,
    flush_threshold: usize,
    writer: BufWriter<File>,
}

impl FrameWriter {
    /// Create a new frame writer.
    pub fn new(path: &Path, flush_threshold: usize) -> Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            buffer: Vec::with_capacity(flush_threshold.max(1)),
            flush_threshold: flush_threshold.max(1),
            writer: BufWriter::new(file),
        })
    }

    /// Write a frame (buffered). Flushes when buffer reaches threshold.
    pub fn write_frame(&mut self, frame: RecordingFrame) -> Result<()> {
        self.buffer.push(frame);
        if self.buffer.len() >= self.flush_threshold {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush buffered frames to disk.
    pub fn flush(&mut self) -> Result<()> {
        for frame in self.buffer.drain(..) {
            let bytes = frame.encode();
            self.writer.write_all(&bytes)?;
        }
        self.writer.flush()?;
        Ok(())
    }
}

/// Recorder runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderState {
    Idle,
    Recording,
    Paused,
    Stopped,
}

/// Recording behavior options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingOptions {
    /// Flush threshold for buffered frames.
    pub flush_threshold: usize,
    /// Redact output content before writing.
    pub redact_output: bool,
    /// Redact detection events before writing.
    pub redact_events: bool,
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            flush_threshold: 64,
            redact_output: true,
            redact_events: true,
        }
    }
}

/// Per-pane recording engine.
pub struct Recorder {
    pane_id: u64,
    writer: FrameWriter,
    state: RecorderState,
    start_instant: Option<Instant>,
    start_epoch_ms: Option<i64>,
    frames_written: u64,
    bytes_raw: u64,
    bytes_written: u64,
}

impl Recorder {
    /// Create a new recorder for a pane and output path.
    pub fn new(pane_id: u64, path: &Path, flush_threshold: usize) -> Result<Self> {
        Ok(Self {
            pane_id,
            writer: FrameWriter::new(path, flush_threshold)?,
            state: RecorderState::Idle,
            start_instant: None,
            start_epoch_ms: None,
            frames_written: 0,
            bytes_raw: 0,
            bytes_written: 0,
        })
    }

    /// Begin recording. The start timestamp anchors relative frame times.
    pub fn start(&mut self, started_at_ms: i64) {
        self.state = RecorderState::Recording;
        self.start_instant = Some(Instant::now());
        self.start_epoch_ms = Some(started_at_ms);
    }

    /// Stop recording and flush any buffered frames.
    pub fn stop(&mut self) -> Result<()> {
        self.state = RecorderState::Stopped;
        self.writer.flush()
    }

    /// Check whether the recorder is actively recording.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.state == RecorderState::Recording
    }

    /// Record a raw output payload as a frame.
    pub fn record_output(
        &mut self,
        captured_at_ms: i64,
        is_gap: bool,
        payload: &[u8],
    ) -> Result<()> {
        if !self.is_recording() {
            return Ok(());
        }

        let timestamp_ms = self.timestamp_ms_for_capture(captured_at_ms);
        let mut flags = 0u8;
        if is_gap {
            flags |= 0b0000_0001;
        }

        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms,
                frame_type: FrameType::Output,
                flags,
                payload_len: payload.len() as u32,
            },
            payload: payload.to_vec(),
        };

        self.frames_written += 1;
        self.bytes_written += frame.payload.len() as u64;
        self.writer.write_frame(frame)
    }

    /// Record a captured output segment as a frame.
    pub fn record_segment(&mut self, segment: &CapturedSegment) -> Result<()> {
        let is_gap = matches!(segment.kind, CapturedSegmentKind::Gap { .. });
        let payload = segment.content.as_bytes();
        self.bytes_raw += payload.len() as u64;
        self.record_output(segment.captured_at, is_gap, payload)
    }

    /// Record a detection event as a frame (redaction to be applied by caller).
    pub fn record_event(&mut self, detection: &Detection, captured_at_ms: i64) -> Result<()> {
        if !self.is_recording() {
            return Ok(());
        }

        let timestamp_ms = self.timestamp_ms_for_capture(captured_at_ms);
        let payload = serde_json::to_vec(detection)?;
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms,
                frame_type: FrameType::Event,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        };

        self.frames_written += 1;
        self.bytes_written += frame.payload.len() as u64;
        self.writer.write_frame(frame)
    }

    fn timestamp_ms_for_capture(&self, captured_at_ms: i64) -> u64 {
        if let Some(start_ms) = self.start_epoch_ms {
            return u64::try_from((captured_at_ms - start_ms).max(0)).unwrap_or(0);
        }
        if let Some(start) = self.start_instant {
            return start.elapsed().as_millis() as u64;
        }
        0
    }

    /// Summary stats for debugging/telemetry.
    #[must_use]
    pub fn stats(&self) -> RecorderStats {
        RecorderStats {
            pane_id: self.pane_id,
            frames_written: self.frames_written,
            bytes_raw: self.bytes_raw,
            bytes_written: self.bytes_written,
            state: self.state,
        }
    }
}

/// Snapshot of recorder stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderStats {
    pub pane_id: u64,
    pub frames_written: u64,
    pub bytes_raw: u64,
    pub bytes_written: u64,
    pub state: RecorderState,
}

/// Manages per-pane recorders and redaction behavior.
pub struct RecordingManager {
    options: RecordingOptions,
    redactor: Redactor,
    recorders: Mutex<HashMap<u64, Recorder>>,
}

impl RecordingManager {
    /// Create a new recording manager with the given options.
    #[must_use]
    pub fn new(options: RecordingOptions) -> Self {
        Self {
            options,
            redactor: Redactor::new(),
            recorders: Mutex::new(HashMap::new()),
        }
    }

    /// Start recording a pane to the given path.
    pub async fn start_recording(
        &self,
        pane_id: u64,
        path: &Path,
        started_at_ms: i64,
    ) -> Result<()> {
        let mut guard = self.recorders.lock().await;
        if guard.contains_key(&pane_id) {
            return Err(crate::Error::Runtime(format!(
                "Recorder already active for pane {pane_id}"
            )));
        }
        let mut recorder = Recorder::new(pane_id, path, self.options.flush_threshold)?;
        recorder.start(started_at_ms);
        guard.insert(pane_id, recorder);
        Ok(())
    }

    /// Stop recording a pane and flush any buffered frames.
    pub async fn stop_recording(&self, pane_id: u64) -> Result<Option<RecorderStats>> {
        let mut guard = self.recorders.lock().await;
        if let Some(mut recorder) = guard.remove(&pane_id) {
            recorder.stop()?;
            return Ok(Some(recorder.stats()));
        }
        Ok(None)
    }

    /// Record a captured output segment (redacted if configured).
    pub async fn record_segment(&self, segment: &CapturedSegment) -> Result<()> {
        let mut guard = self.recorders.lock().await;
        let Some(recorder) = guard.get_mut(&segment.pane_id) else {
            return Ok(());
        };
        if !recorder.is_recording() {
            return Ok(());
        }

        let payload = if self.options.redact_output {
            let redacted = self.redactor.redact(&segment.content);
            redacted.into_bytes()
        } else {
            segment.content.as_bytes().to_vec()
        };

        let is_gap = matches!(segment.kind, CapturedSegmentKind::Gap { .. });
        recorder.bytes_raw += segment.content.len() as u64;
        recorder.record_output(segment.captured_at, is_gap, &payload)
    }

    /// Record a detection event (redacted if configured).
    pub async fn record_event(
        &self,
        pane_id: u64,
        detection: &Detection,
        captured_at_ms: i64,
    ) -> Result<()> {
        let mut guard = self.recorders.lock().await;
        let Some(recorder) = guard.get_mut(&pane_id) else {
            return Ok(());
        };
        if !recorder.is_recording() {
            return Ok(());
        }

        let mut detection = detection.clone();
        if self.options.redact_events {
            detection = redact_detection(&detection, &self.redactor);
        }
        recorder.record_event(&detection, captured_at_ms)
    }
}

fn redact_detection(detection: &Detection, redactor: &Redactor) -> Detection {
    let mut redacted = detection.clone();
    redacted.matched_text = redactor.redact(&redacted.matched_text);
    if let Ok(serialized) = serde_json::to_string(&redacted.extracted) {
        let scrubbed = redactor.redact(&serialized);
        if let Ok(value) = serde_json::from_str(&scrubbed) {
            redacted.extracted = value;
        }
    }
    redacted
}

// ---------------------------------------------------------------------------
// Recorder event schema v1 — versioned canonical events for flight recorder
// ---------------------------------------------------------------------------

/// Schema version string for the v1 recorder event contract.
pub const RECORDER_EVENT_SCHEMA_VERSION_V1: &str = "ft.recorder.event.v1";

/// Source subsystem that produced the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderEventSource {
    WeztermMux,
    RobotMode,
    WorkflowEngine,
    OperatorAction,
    RecoveryFlow,
}

/// Text encoding used for ingress/egress payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderTextEncoding {
    Utf8,
}

/// Redaction level applied to captured text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderRedactionLevel {
    None,
    Partial,
    Full,
}

/// How ingress text was injected into the mux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderIngressKind {
    SendText,
    Paste,
    WorkflowAction,
}

/// Kind of egress output segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderSegmentKind {
    Delta,
    Gap,
    Snapshot,
}

/// Type of control marker event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderControlMarkerType {
    PromptBoundary,
    Resize,
    PolicyDecision,
    ApprovalCheckpoint,
}

/// Lifecycle phase for capture state transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderLifecyclePhase {
    CaptureStarted,
    CaptureStopped,
    PaneOpened,
    PaneClosed,
    ReplayStarted,
    ReplayFinished,
}

/// Causal linkage between recorder events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderEventCausality {
    pub parent_event_id: Option<String>,
    pub trigger_event_id: Option<String>,
    pub root_event_id: Option<String>,
}

/// Variant-specific payload for a recorder event.
///
/// Serializes with an internally tagged `event_type` discriminant so all
/// fields appear at the top level when flattened into [`RecorderEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum RecorderEventPayload {
    IngressText {
        text: String,
        encoding: RecorderTextEncoding,
        redaction: RecorderRedactionLevel,
        ingress_kind: RecorderIngressKind,
    },
    EgressOutput {
        text: String,
        encoding: RecorderTextEncoding,
        redaction: RecorderRedactionLevel,
        segment_kind: RecorderSegmentKind,
        is_gap: bool,
    },
    ControlMarker {
        control_marker_type: RecorderControlMarkerType,
        details: serde_json::Value,
    },
    LifecycleMarker {
        lifecycle_phase: RecorderLifecyclePhase,
        reason: Option<String>,
        details: serde_json::Value,
    },
}

/// A versioned recorder event for the flight recorder.
///
/// The payload is flattened so all fields appear at the top level in JSON,
/// matching the `ft-recorder-event-v1.json` schema contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderEvent {
    pub schema_version: String,
    pub event_id: String,
    pub pane_id: u64,
    pub session_id: Option<String>,
    pub workflow_id: Option<String>,
    pub correlation_id: Option<String>,
    pub source: RecorderEventSource,
    pub occurred_at_ms: u64,
    pub recorded_at_ms: u64,
    pub sequence: u64,
    pub causality: RecorderEventCausality,
    #[serde(flatten)]
    pub payload: RecorderEventPayload,
}

/// Parse a JSON string into a [`RecorderEvent`], validating the schema version.
///
/// Returns an error if the schema version is not `ft.recorder.event.v1`.
/// Tolerates unknown additive fields for forward compatibility.
pub fn parse_recorder_event_json(json: &str) -> crate::Result<RecorderEvent> {
    // First pass: check schema version before full deserialization.
    let raw: serde_json::Value = serde_json::from_str(json).map_err(crate::Error::Json)?;

    let version = raw
        .get("schema_version")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if version != RECORDER_EVENT_SCHEMA_VERSION_V1 {
        return Err(crate::Error::Runtime(format!(
            "unsupported recorder event schema version: {version:?} \
             (expected {RECORDER_EVENT_SCHEMA_VERSION_V1:?})"
        )));
    }

    // Second pass: deserialize with serde, tolerating unknown fields.
    let event: RecorderEvent = serde_json::from_value(raw).map_err(crate::Error::Json)?;
    Ok(event)
}

// ---------------------------------------------------------------------------
// Ingress tap — observer interface for mux ingress capture
// ---------------------------------------------------------------------------

/// Outcome of an ingress injection attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressOutcome {
    /// Injection was allowed and executed successfully.
    Allowed,
    /// Injection was denied by policy.
    Denied { reason: String },
    /// Injection requires human approval before execution.
    RequiresApproval,
    /// Injection was allowed but the send failed.
    Error { error: String },
}

/// An ingress event captured at a tap point.
///
/// Contains all metadata needed to produce a [`RecorderEvent`] with
/// an [`RecorderEventPayload::IngressText`] payload.
#[derive(Debug, Clone)]
pub struct IngressEvent {
    /// Target pane for the injection.
    pub pane_id: u64,
    /// The injected text (may be redacted).
    pub text: String,
    /// Source subsystem that produced this injection.
    pub source: RecorderEventSource,
    /// How the text was injected.
    pub ingress_kind: RecorderIngressKind,
    /// Redaction level applied to the captured text.
    pub redaction: RecorderRedactionLevel,
    /// Unix epoch milliseconds when the injection occurred.
    pub occurred_at_ms: u64,
    /// Outcome of the injection attempt.
    pub outcome: IngressOutcome,
    /// Optional workflow correlation ID.
    pub workflow_id: Option<String>,
}

/// Observer interface for ingress event capture.
///
/// Implementations must be fast and non-blocking. The tap is called
/// synchronously on the injection hot path — expensive work (persistence,
/// network I/O) should be offloaded to a background task.
pub trait IngressTap: Send + Sync {
    /// Called when an ingress injection is attempted.
    ///
    /// This fires for ALL outcomes (allowed, denied, requires_approval, error)
    /// to provide complete forensic visibility.
    fn on_ingress(&self, event: IngressEvent);
}

/// Maps a [`crate::policy::ActorKind`] to a [`RecorderEventSource`].
#[must_use]
pub fn actor_to_source(actor: crate::policy::ActorKind) -> RecorderEventSource {
    use crate::policy::ActorKind;
    match actor {
        ActorKind::Human => RecorderEventSource::OperatorAction,
        ActorKind::Robot => RecorderEventSource::RobotMode,
        ActorKind::Mcp => RecorderEventSource::RobotMode,
        ActorKind::Workflow => RecorderEventSource::WorkflowEngine,
    }
}

/// Maps a [`crate::policy::ActionKind`] and [`crate::policy::ActorKind`]
/// to a [`RecorderIngressKind`].
#[must_use]
pub fn action_to_ingress_kind(
    action: crate::policy::ActionKind,
    actor: crate::policy::ActorKind,
) -> RecorderIngressKind {
    use crate::policy::{ActionKind, ActorKind};
    match action {
        ActionKind::SendText if actor == ActorKind::Workflow => RecorderIngressKind::WorkflowAction,
        ActionKind::SendText => RecorderIngressKind::SendText,
        // Control key sends are still "send_text" in the recorder schema
        ActionKind::SendCtrlC
        | ActionKind::SendCtrlD
        | ActionKind::SendCtrlZ
        | ActionKind::SendControl => {
            if actor == ActorKind::Workflow {
                RecorderIngressKind::WorkflowAction
            } else {
                RecorderIngressKind::SendText
            }
        }
        // Non-injection actions shouldn't reach the tap, but map defensively
        _ => RecorderIngressKind::SendText,
    }
}

/// Returns the current Unix epoch in milliseconds.
#[must_use]
pub fn epoch_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Thread-safe monotonic sequence counter for recorder events.
///
/// Each pane should have its own counter. The counter is lock-free
/// using `AtomicU64` with relaxed ordering (sufficient for monotonicity
/// within a single process).
#[derive(Debug)]
pub struct IngressSequence {
    next: AtomicU64,
}

impl IngressSequence {
    /// Create a new sequence counter starting at 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
        }
    }

    /// Advance and return the next sequence number.
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for IngressSequence {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide monotonic sequence counter for cross-pane merge ordering.
#[derive(Debug)]
pub struct GlobalSequence {
    next: AtomicU64,
}

impl GlobalSequence {
    /// Create a new global sequence counter starting at 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
        }
    }

    /// Advance and return the next global sequence number.
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for GlobalSequence {
    fn default() -> Self {
        Self::new()
    }
}

/// A collecting tap that stores all received events for testing.
///
/// Uses `std::sync::Mutex` (not async) because these methods are synchronous.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct CollectingTap {
    events: std::sync::Mutex<Vec<IngressEvent>>,
}

#[cfg(test)]
impl CollectingTap {
    /// Create a new empty collecting tap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of all collected events.
    pub fn events(&self) -> Vec<IngressEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Return the number of collected events.
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Return true if no events have been collected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
impl IngressTap for CollectingTap {
    fn on_ingress(&self, event: IngressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// No-op tap that discards all events (zero overhead).
pub struct NoopTap;

impl IngressTap for NoopTap {
    #[inline]
    fn on_ingress(&self, _event: IngressEvent) {}
}

/// Convenience alias for a shared ingress tap.
pub type SharedIngressTap = Arc<dyn IngressTap>;

// ---------------------------------------------------------------------------
// Egress tap — observer interface for mux egress capture (ft-oegrb.2.3)
// ---------------------------------------------------------------------------

/// An egress event captured at a tap point.
///
/// Contains all metadata needed to produce a [`RecorderEvent`] with
/// an [`RecorderEventPayload::EgressOutput`] payload.
#[derive(Debug, Clone)]
pub struct EgressEvent {
    /// Pane that produced this output.
    pub pane_id: u64,
    /// The captured output text (delta or full snapshot).
    pub text: String,
    /// Kind of egress segment (delta, gap, or snapshot).
    pub segment_kind: RecorderSegmentKind,
    /// True if this segment represents a capture discontinuity.
    pub is_gap: bool,
    /// Reason for the gap (only set when `is_gap` is true).
    pub gap_reason: Option<String>,
    /// Text encoding (always UTF-8 for now).
    pub encoding: RecorderTextEncoding,
    /// Redaction level applied to the captured text.
    pub redaction: RecorderRedactionLevel,
    /// Unix epoch milliseconds when the capture occurred.
    pub occurred_at_ms: u64,
    /// Per-pane monotonic sequence number.
    pub sequence: u64,
    /// Process-wide monotonic sequence used to merge inter-pane streams.
    pub global_sequence: u64,
}

/// Observer interface for egress event capture.
///
/// Implementations must be fast and non-blocking. The tap is called
/// synchronously on the capture hot path — expensive work (persistence,
/// network I/O) should be offloaded to a background task.
pub trait EgressTap: Send + Sync {
    /// Called when a pane output segment is captured.
    ///
    /// This fires for ALL segment kinds (delta, gap, snapshot) to provide
    /// complete capture visibility including discontinuity markers.
    fn on_egress(&self, event: EgressEvent);
}

/// No-op egress tap that discards all events (zero overhead).
pub struct EgressNoopTap;

impl EgressTap for EgressNoopTap {
    #[inline]
    fn on_egress(&self, _event: EgressEvent) {}
}

/// Convenience alias for a shared egress tap.
pub type SharedEgressTap = Arc<dyn EgressTap>;

/// Maps a [`crate::ingest::CapturedSegmentKind`] to a [`RecorderSegmentKind`]
/// and derives the `is_gap` flag.
#[must_use]
pub fn captured_kind_to_segment(
    kind: &crate::ingest::CapturedSegmentKind,
) -> (RecorderSegmentKind, bool) {
    use crate::ingest::CapturedSegmentKind;
    match kind {
        CapturedSegmentKind::Delta => (RecorderSegmentKind::Delta, false),
        CapturedSegmentKind::Gap { .. } => (RecorderSegmentKind::Gap, true),
    }
}

/// A collecting egress tap that stores all received events for testing.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct CollectingEgressTap {
    events: std::sync::Mutex<Vec<EgressEvent>>,
}

#[cfg(test)]
impl CollectingEgressTap {
    /// Create a new empty collecting egress tap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of all collected events.
    pub fn events(&self) -> Vec<EgressEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Return the number of collected events.
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Return true if no events have been collected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
impl EgressTap for CollectingEgressTap {
    fn on_egress(&self, event: EgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Severity};
    use serde_json::json;
    use std::future::Future;
    use tempfile::tempdir;

    fn run_async_test<F>(future: F)
    where
        F: Future<Output = ()>,
    {
        use crate::runtime_compat::{CompatRuntime, RuntimeBuilder};

        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("recording test runtime should build");
        runtime.block_on(future);
    }

    #[test]
    fn recording_frame_encodes_header() {
        let payload = vec![1u8, 2, 3];
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 42,
                frame_type: FrameType::Output,
                flags: 7,
                payload_len: payload.len() as u32,
            },
            payload: payload.clone(),
        };

        let bytes = frame.encode();
        assert_eq!(bytes.len(), 14 + payload.len());
        assert_eq!(u64::from_le_bytes(bytes[0..8].try_into().unwrap()), 42);
        assert_eq!(bytes[8], FrameType::Output as u8);
        assert_eq!(bytes[9], 7);
        assert_eq!(u32::from_le_bytes(bytes[10..14].try_into().unwrap()), 3);
        assert_eq!(&bytes[14..], payload.as_slice());
    }

    #[test]
    fn redact_detection_scrubs_secrets() {
        let secret = "sk-abc123456789012345678901234567890123456789012345678901";
        let detection = Detection {
            rule_id: "test.rule".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({ "token": secret }),
            matched_text: secret.to_string(),
            span: (0, 5),
        };

        let redactor = Redactor::new();
        let redacted = super::redact_detection(&detection, &redactor);
        assert!(!redacted.matched_text.contains(secret));
        let serialized = serde_json::to_string(&redacted.extracted).unwrap();
        assert!(!serialized.contains(secret));
    }

    #[test]
    fn recording_manager_redacts_output() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let path = dir.path().join("test.war");
            let secret = "sk-abc123456789012345678901234567890123456789012345678901";

            let manager = RecordingManager::new(RecordingOptions {
                flush_threshold: 1,
                redact_output: true,
                redact_events: false,
            });

            manager.start_recording(1, &path, 0).await.unwrap();
            let segment = CapturedSegment {
                pane_id: 1,
                seq: 0,
                content: format!("token {secret}"),
                kind: CapturedSegmentKind::Delta,
                captured_at: 10,
            };
            manager.record_segment(&segment).await.unwrap();
            manager.stop_recording(1).await.unwrap();

            let bytes = std::fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(secret));
            assert!(text.contains("[REDACTED]"));
        });
    }

    // -----------------------------------------------------------------------
    // wa-z0e.6: Recording tests — format, roundtrip, fuzz
    // -----------------------------------------------------------------------

    #[test]
    fn frame_encodes_all_frame_types() {
        let types = [
            (FrameType::Output, 1u8),
            (FrameType::Resize, 2),
            (FrameType::Event, 3),
            (FrameType::Marker, 4),
            (FrameType::Input, 5),
        ];
        for (ft, expected_byte) in types {
            let frame = RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: 0,
                    frame_type: ft,
                    flags: 0,
                    payload_len: 0,
                },
                payload: vec![],
            };
            let bytes = frame.encode();
            assert_eq!(bytes.len(), 14);
            assert_eq!(bytes[8], expected_byte, "wrong byte for {ft:?}");
        }
    }

    #[test]
    fn frame_encodes_empty_payload() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 0,
            },
            payload: vec![],
        };
        let bytes = frame.encode();
        assert_eq!(bytes.len(), 14);
        assert_eq!(u32::from_le_bytes(bytes[10..14].try_into().unwrap()), 0);
    }

    #[test]
    fn frame_encodes_gap_flag() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Output,
                flags: 0b0000_0001, // gap flag
                payload_len: 0,
            },
            payload: vec![],
        };
        let bytes = frame.encode();
        assert_eq!(bytes[9], 1);
    }

    #[test]
    fn frame_encodes_max_timestamp() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: u64::MAX,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 0,
            },
            payload: vec![],
        };
        let bytes = frame.encode();
        assert_eq!(
            u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            u64::MAX
        );
    }

    #[test]
    fn delta_encoding_full_variant() {
        let data = vec![1u8, 2, 3, 4, 5];
        let enc = DeltaEncoding::Full(data.clone());
        if let DeltaEncoding::Full(inner) = enc {
            assert_eq!(inner, data);
        } else {
            panic!("expected Full variant");
        }
    }

    #[test]
    fn delta_encoding_serde_roundtrip() {
        let enc = DeltaEncoding::Full(vec![0xDE, 0xAD]);
        let json = serde_json::to_string(&enc).unwrap();
        let back: DeltaEncoding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, enc);
    }

    #[test]
    fn diff_op_serde_roundtrip() {
        let ops = vec![
            DiffOp::Copy { offset: 0, len: 10 },
            DiffOp::Insert {
                data: vec![1, 2, 3],
            },
        ];
        let json = serde_json::to_string(&ops).unwrap();
        let back: Vec<DiffOp> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ops);
    }

    #[test]
    fn frame_writer_writes_to_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");

        {
            let mut writer = FrameWriter::new(&path, 10).unwrap();
            writer
                .write_frame(RecordingFrame {
                    header: FrameHeader {
                        timestamp_ms: 0,
                        frame_type: FrameType::Output,
                        flags: 0,
                        payload_len: 5,
                    },
                    payload: b"hello".to_vec(),
                })
                .unwrap();
            writer.flush().unwrap();
        }

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 14 + 5); // header + payload
    }

    #[test]
    fn frame_writer_auto_flushes_at_threshold() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");

        {
            let mut writer = FrameWriter::new(&path, 2).unwrap();
            // Write 2 frames (equals threshold) — should auto-flush
            for _ in 0..2 {
                writer
                    .write_frame(RecordingFrame {
                        header: FrameHeader {
                            timestamp_ms: 0,
                            frame_type: FrameType::Marker,
                            flags: 0,
                            payload_len: 1,
                        },
                        payload: vec![b'x'],
                    })
                    .unwrap();
            }
            // Don't call flush() — it should have happened automatically
        }

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), (14 + 1) * 2);
    }

    #[test]
    fn frame_writer_multiple_flushes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");

        {
            let mut writer = FrameWriter::new(&path, 1).unwrap();
            for i in 0..5u8 {
                writer
                    .write_frame(RecordingFrame {
                        header: FrameHeader {
                            timestamp_ms: i as u64 * 100,
                            frame_type: FrameType::Output,
                            flags: 0,
                            payload_len: 1,
                        },
                        payload: vec![b'A' + i],
                    })
                    .unwrap();
            }
        }

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), (14 + 1) * 5);
    }

    #[test]
    fn recorder_state_transitions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(42, &path, 10).unwrap();

        assert_eq!(recorder.state, RecorderState::Idle);
        assert!(!recorder.is_recording());

        recorder.start(1000);
        assert_eq!(recorder.state, RecorderState::Recording);
        assert!(recorder.is_recording());

        recorder.stop().unwrap();
        assert_eq!(recorder.state, RecorderState::Stopped);
        assert!(!recorder.is_recording());
    }

    #[test]
    fn recorder_ignores_output_when_not_recording() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 10).unwrap();

        // Don't start — output should be silently dropped
        recorder.record_output(0, false, b"ignored").unwrap();
        assert_eq!(recorder.stats().frames_written, 0);
    }

    #[test]
    fn recorder_records_output_frames() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 100).unwrap();

        recorder.start(0);
        recorder.record_output(10, false, b"hello").unwrap();
        recorder.record_output(20, true, b"world").unwrap();
        recorder.stop().unwrap();

        let stats = recorder.stats();
        assert_eq!(stats.frames_written, 2);
        assert_eq!(stats.bytes_written, 10); // "hello" + "world"

        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn recorder_records_segments() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 100).unwrap();

        recorder.start(0);
        let segment = CapturedSegment {
            pane_id: 1,
            seq: 0,
            content: "output data".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 50,
        };
        recorder.record_segment(&segment).unwrap();
        recorder.stop().unwrap();

        let stats = recorder.stats();
        assert_eq!(stats.frames_written, 1);
        assert_eq!(stats.bytes_raw, 11); // "output data"
    }

    #[test]
    fn recorder_stats_snapshot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let recorder = Recorder::new(42, &path, 10).unwrap();

        let stats = recorder.stats();
        assert_eq!(stats.pane_id, 42);
        assert_eq!(stats.frames_written, 0);
        assert_eq!(stats.bytes_raw, 0);
        assert_eq!(stats.bytes_written, 0);
        assert_eq!(stats.state, RecorderState::Idle);
    }

    #[test]
    fn recorder_file_io_roundtrip() {
        use crate::replay::Recording;

        let dir = tempdir().unwrap();
        let path = dir.path().join("roundtrip.war");

        // Record some frames
        {
            let mut recorder = Recorder::new(1, &path, 1).unwrap();
            recorder.start(0);
            recorder.record_output(10, false, b"first line\n").unwrap();
            recorder.record_output(20, false, b"second line\n").unwrap();
            recorder.record_output(30, true, b"gap output\n").unwrap();
            recorder.stop().unwrap();
        }

        // Load and verify via replay module
        let bytes = std::fs::read(&path).unwrap();
        let recording = Recording::from_bytes(&bytes).unwrap();
        assert_eq!(recording.frames.len(), 3);
        assert_eq!(recording.duration_ms, 30);
        assert_eq!(recording.frames[0].payload, b"first line\n");
        assert_eq!(recording.frames[2].header.flags & 1, 1); // gap flag
    }

    #[test]
    fn recording_manager_multi_pane() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let path1 = dir.path().join("pane1.war");
            let path2 = dir.path().join("pane2.war");

            let manager = RecordingManager::new(RecordingOptions {
                flush_threshold: 1,
                redact_output: false,
                redact_events: false,
            });

            manager.start_recording(1, &path1, 0).await.unwrap();
            manager.start_recording(2, &path2, 0).await.unwrap();

            let seg1 = CapturedSegment {
                pane_id: 1,
                seq: 0,
                content: "pane1_data".into(),
                kind: CapturedSegmentKind::Delta,
                captured_at: 10,
            };
            let seg2 = CapturedSegment {
                pane_id: 2,
                seq: 0,
                content: "pane2_data".into(),
                kind: CapturedSegmentKind::Delta,
                captured_at: 10,
            };

            manager.record_segment(&seg1).await.unwrap();
            manager.record_segment(&seg2).await.unwrap();

            let stats1 = manager.stop_recording(1).await.unwrap().unwrap();
            let stats2 = manager.stop_recording(2).await.unwrap().unwrap();

            assert_eq!(stats1.pane_id, 1);
            assert_eq!(stats1.frames_written, 1);
            assert_eq!(stats2.pane_id, 2);
            assert_eq!(stats2.frames_written, 1);

            // Verify file isolation
            let bytes1 = std::fs::read(&path1).unwrap();
            let bytes2 = std::fs::read(&path2).unwrap();
            assert!(String::from_utf8_lossy(&bytes1).contains("pane1_data"));
            assert!(String::from_utf8_lossy(&bytes2).contains("pane2_data"));
            assert!(!String::from_utf8_lossy(&bytes1).contains("pane2_data"));
        });
    }

    #[test]
    fn recording_manager_duplicate_start_fails() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let path = dir.path().join("test.war");

            let manager = RecordingManager::new(RecordingOptions::default());
            manager.start_recording(1, &path, 0).await.unwrap();

            let result = manager.start_recording(1, &path, 0).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn recording_manager_stop_nonexistent_returns_none() {
        run_async_test(async {
            let manager = RecordingManager::new(RecordingOptions::default());
            let result = manager.stop_recording(999).await.unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn recording_manager_segment_for_unknown_pane_is_noop() {
        run_async_test(async {
            let manager = RecordingManager::new(RecordingOptions::default());
            let segment = CapturedSegment {
                pane_id: 999,
                seq: 0,
                content: "ghost".into(),
                kind: CapturedSegmentKind::Delta,
                captured_at: 0,
            };
            // Should not error
            manager.record_segment(&segment).await.unwrap();
        });
    }

    // Ingress tap unit tests (ft-oegrb.2.2)

    #[test]
    fn actor_to_source_maps_all_variants() {
        use crate::policy::ActorKind;
        assert_eq!(
            actor_to_source(ActorKind::Human),
            RecorderEventSource::OperatorAction
        );
        assert_eq!(
            actor_to_source(ActorKind::Robot),
            RecorderEventSource::RobotMode
        );
        assert_eq!(
            actor_to_source(ActorKind::Mcp),
            RecorderEventSource::RobotMode
        );
        assert_eq!(
            actor_to_source(ActorKind::Workflow),
            RecorderEventSource::WorkflowEngine
        );
    }

    #[test]
    fn action_to_ingress_kind_maps_correctly() {
        use crate::policy::{ActionKind, ActorKind};
        assert_eq!(
            action_to_ingress_kind(ActionKind::SendText, ActorKind::Robot),
            RecorderIngressKind::SendText
        );
        assert_eq!(
            action_to_ingress_kind(ActionKind::SendText, ActorKind::Workflow),
            RecorderIngressKind::WorkflowAction
        );
        assert_eq!(
            action_to_ingress_kind(ActionKind::SendCtrlC, ActorKind::Robot),
            RecorderIngressKind::SendText
        );
        assert_eq!(
            action_to_ingress_kind(ActionKind::SendCtrlD, ActorKind::Workflow),
            RecorderIngressKind::WorkflowAction
        );
    }

    #[test]
    fn ingress_sequence_monotonic() {
        let seq = IngressSequence::new();
        assert_eq!(seq.next(), 0);
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
    }

    #[test]
    fn epoch_ms_now_reasonable() {
        let ms = epoch_ms_now();
        assert!(ms > 1_735_689_600_000);
        assert!(ms < 4_102_444_800_000);
    }

    #[test]
    fn noop_tap_is_zero_cost() {
        let tap = NoopTap;
        tap.on_ingress(IngressEvent {
            pane_id: 0,
            text: String::new(),
            source: RecorderEventSource::RobotMode,
            ingress_kind: RecorderIngressKind::SendText,
            redaction: RecorderRedactionLevel::None,
            occurred_at_ms: 0,
            outcome: IngressOutcome::Allowed,
            workflow_id: None,
        });
    }

    #[test]
    fn collecting_tap_accumulates() {
        let tap = CollectingTap::new();
        assert!(tap.is_empty());
        for i in 0..3 {
            tap.on_ingress(IngressEvent {
                pane_id: i,
                text: format!("cmd-{i}"),
                source: RecorderEventSource::RobotMode,
                ingress_kind: RecorderIngressKind::SendText,
                redaction: RecorderRedactionLevel::None,
                occurred_at_ms: 0,
                outcome: IngressOutcome::Allowed,
                workflow_id: None,
            });
        }
        assert_eq!(tap.len(), 3);
        assert_eq!(tap.events()[1].pane_id, 1);
    }

    #[test]
    fn shared_tap_via_arc() {
        let tap = Arc::new(CollectingTap::new());
        let shared: SharedIngressTap = tap.clone();
        shared.on_ingress(IngressEvent {
            pane_id: 42,
            text: "test".into(),
            source: RecorderEventSource::WorkflowEngine,
            ingress_kind: RecorderIngressKind::WorkflowAction,
            redaction: RecorderRedactionLevel::None,
            occurred_at_ms: 3000,
            outcome: IngressOutcome::Allowed,
            workflow_id: Some("wf-123".into()),
        });
        assert_eq!(tap.len(), 1);
        assert_eq!(tap.events()[0].workflow_id, Some("wf-123".into()));
    }

    // ========================================================================
    // Batch 2: RubyBeaver wa-1u90p.7.1 — expanded coverage
    // ========================================================================

    // --- Serde round-trips for enum types ---

    #[test]
    fn frame_type_serde_round_trip() {
        for ft in [
            FrameType::Output,
            FrameType::Resize,
            FrameType::Event,
            FrameType::Marker,
            FrameType::Input,
        ] {
            let json = serde_json::to_string(&ft).unwrap();
            let back: FrameType = serde_json::from_str(&json).unwrap();
            assert_eq!(ft, back);
        }
    }

    #[test]
    fn recorder_event_source_serde_round_trip() {
        for src in [
            RecorderEventSource::WeztermMux,
            RecorderEventSource::RobotMode,
            RecorderEventSource::WorkflowEngine,
            RecorderEventSource::OperatorAction,
            RecorderEventSource::RecoveryFlow,
        ] {
            let json = serde_json::to_string(&src).unwrap();
            let back: RecorderEventSource = serde_json::from_str(&json).unwrap();
            assert_eq!(src, back);
        }
    }

    #[test]
    fn recorder_text_encoding_serde() {
        let enc = RecorderTextEncoding::Utf8;
        let json = serde_json::to_string(&enc).unwrap();
        let back: RecorderTextEncoding = serde_json::from_str(&json).unwrap();
        assert_eq!(enc, back);
    }

    #[test]
    fn recorder_redaction_level_serde_round_trip() {
        for level in [
            RecorderRedactionLevel::None,
            RecorderRedactionLevel::Partial,
            RecorderRedactionLevel::Full,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: RecorderRedactionLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    #[test]
    fn recorder_ingress_kind_serde_round_trip() {
        for kind in [
            RecorderIngressKind::SendText,
            RecorderIngressKind::Paste,
            RecorderIngressKind::WorkflowAction,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: RecorderIngressKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn recorder_segment_kind_serde_round_trip() {
        for kind in [
            RecorderSegmentKind::Delta,
            RecorderSegmentKind::Gap,
            RecorderSegmentKind::Snapshot,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: RecorderSegmentKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn recorder_control_marker_type_serde_round_trip() {
        for mt in [
            RecorderControlMarkerType::PromptBoundary,
            RecorderControlMarkerType::Resize,
            RecorderControlMarkerType::PolicyDecision,
            RecorderControlMarkerType::ApprovalCheckpoint,
        ] {
            let json = serde_json::to_string(&mt).unwrap();
            let back: RecorderControlMarkerType = serde_json::from_str(&json).unwrap();
            assert_eq!(mt, back);
        }
    }

    #[test]
    fn recorder_lifecycle_phase_serde_round_trip() {
        for phase in [
            RecorderLifecyclePhase::CaptureStarted,
            RecorderLifecyclePhase::CaptureStopped,
            RecorderLifecyclePhase::PaneOpened,
            RecorderLifecyclePhase::PaneClosed,
            RecorderLifecyclePhase::ReplayStarted,
            RecorderLifecyclePhase::ReplayFinished,
        ] {
            let json = serde_json::to_string(&phase).unwrap();
            let back: RecorderLifecyclePhase = serde_json::from_str(&json).unwrap();
            assert_eq!(phase, back);
        }
    }

    // --- RecorderEventCausality serde ---

    #[test]
    fn recorder_event_causality_serde_round_trip() {
        let causality = RecorderEventCausality {
            parent_event_id: Some("parent-1".into()),
            trigger_event_id: None,
            root_event_id: Some("root-0".into()),
        };
        let json = serde_json::to_string(&causality).unwrap();
        let back: RecorderEventCausality = serde_json::from_str(&json).unwrap();
        assert_eq!(causality, back);
    }

    // --- RecorderEventPayload serde ---

    #[test]
    fn recorder_event_payload_ingress_serde() {
        let payload = RecorderEventPayload::IngressText {
            text: "hello".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
    }

    #[test]
    fn recorder_event_payload_egress_serde() {
        let payload = RecorderEventPayload::EgressOutput {
            text: "output".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Partial,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
    }

    #[test]
    fn recorder_event_payload_control_marker_serde() {
        let payload = RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::Resize,
            details: json!({"cols": 80, "rows": 24}),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
    }

    #[test]
    fn recorder_event_payload_lifecycle_serde() {
        let payload = RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: RecorderLifecyclePhase::CaptureStarted,
            reason: Some("user initiated".into()),
            details: json!({}),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
    }

    // --- RecorderEvent full serde ---

    #[test]
    fn recorder_event_full_serde_round_trip() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: "evt-001".into(),
            pane_id: 42,
            session_id: Some("sess-1".into()),
            workflow_id: None,
            correlation_id: Some("corr-1".into()),
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: "ls -la".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: RecorderEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.event_id, back.event_id);
        assert_eq!(event.pane_id, back.pane_id);
        assert_eq!(event.source, back.source);
    }

    // --- parse_recorder_event_json ---

    #[test]
    fn parse_recorder_event_json_valid() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: "test-1".into(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::OperatorAction,
            occurred_at_ms: 500,
            recorded_at_ms: 501,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: RecorderLifecyclePhase::PaneOpened,
                reason: None,
                details: json!({}),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed = parse_recorder_event_json(&json).unwrap();
        assert_eq!(parsed.event_id, "test-1");
    }

    #[test]
    fn parse_recorder_event_json_wrong_version() {
        let json = json!({
            "schema_version": "ft.recorder.event.v99",
            "event_id": "bad",
            "pane_id": 1,
        });
        let result = parse_recorder_event_json(&json.to_string());
        assert!(result.is_err());
    }

    #[test]
    fn parse_recorder_event_json_invalid_json() {
        let result = parse_recorder_event_json("not json {{{");
        assert!(result.is_err());
    }

    // --- captured_kind_to_segment ---

    #[test]
    fn captured_kind_to_segment_delta() {
        let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Delta);
        assert_eq!(kind, RecorderSegmentKind::Delta);
        assert!(!is_gap);
    }

    #[test]
    fn captured_kind_to_segment_gap() {
        let gap = CapturedSegmentKind::Gap {
            reason: "test".into(),
        };
        let (kind, is_gap) = captured_kind_to_segment(&gap);
        assert_eq!(kind, RecorderSegmentKind::Gap);
        assert!(is_gap);
    }

    // --- GlobalSequence ---

    #[test]
    fn global_sequence_monotonic() {
        let seq = GlobalSequence::new();
        assert_eq!(seq.next(), 0);
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
    }

    #[test]
    fn global_sequence_default() {
        let seq = GlobalSequence::default();
        assert_eq!(seq.next(), 0);
    }

    #[test]
    fn ingress_sequence_default() {
        let seq = IngressSequence::default();
        assert_eq!(seq.next(), 0);
    }

    // --- RecordingOptions ---

    #[test]
    fn recording_options_default_values() {
        let opts = RecordingOptions::default();
        assert_eq!(opts.flush_threshold, 64);
        assert!(opts.redact_output);
        assert!(opts.redact_events);
    }

    // --- EgressTap ---

    #[test]
    fn egress_noop_tap_does_not_panic() {
        let tap = EgressNoopTap;
        tap.on_egress(EgressEvent {
            pane_id: 1,
            text: "hello".into(),
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
            gap_reason: None,
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            occurred_at_ms: 0,
            sequence: 0,
            global_sequence: 0,
        });
    }

    #[test]
    fn collecting_egress_tap_accumulates() {
        let tap = CollectingEgressTap::new();
        assert!(tap.is_empty());
        tap.on_egress(EgressEvent {
            pane_id: 1,
            text: "output1".into(),
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
            gap_reason: None,
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            occurred_at_ms: 100,
            sequence: 0,
            global_sequence: 0,
        });
        tap.on_egress(EgressEvent {
            pane_id: 2,
            text: "output2".into(),
            segment_kind: RecorderSegmentKind::Gap,
            is_gap: true,
            gap_reason: Some("missed".into()),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            occurred_at_ms: 200,
            sequence: 1,
            global_sequence: 1,
        });
        assert_eq!(tap.len(), 2);
        let events = tap.events();
        assert_eq!(events[0].pane_id, 1);
        assert_eq!(events[1].segment_kind, RecorderSegmentKind::Gap);
        assert!(events[1].is_gap);
        assert_eq!(events[1].gap_reason, Some("missed".into()));
    }

    // --- IngressOutcome ---

    #[test]
    fn ingress_outcome_variants() {
        let allowed = IngressOutcome::Allowed;
        let denied = IngressOutcome::Denied {
            reason: "policy".into(),
        };
        let approval = IngressOutcome::RequiresApproval;
        let error = IngressOutcome::Error {
            error: "fail".into(),
        };
        assert_eq!(allowed, IngressOutcome::Allowed);
        assert_ne!(allowed, denied);
        assert_ne!(approval, error);
    }

    // --- action_to_ingress_kind additional mappings ---

    #[test]
    fn action_to_ingress_kind_ctrl_z_workflow() {
        use crate::policy::{ActionKind, ActorKind};
        assert_eq!(
            action_to_ingress_kind(ActionKind::SendCtrlZ, ActorKind::Workflow),
            RecorderIngressKind::WorkflowAction
        );
    }

    #[test]
    fn action_to_ingress_kind_send_control_robot() {
        use crate::policy::{ActionKind, ActorKind};
        assert_eq!(
            action_to_ingress_kind(ActionKind::SendControl, ActorKind::Robot),
            RecorderIngressKind::SendText
        );
    }

    // --- Recorder event recording ---

    #[test]
    fn recorder_records_event_frame() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 100).unwrap();
        recorder.start(0);

        let detection = Detection {
            rule_id: "test.rule".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: json!({}),
            matched_text: "test".into(),
            span: (0, 4),
        };
        recorder.record_event(&detection, 50).unwrap();
        recorder.stop().unwrap();

        let stats = recorder.stats();
        assert_eq!(stats.frames_written, 1);
        assert!(stats.bytes_written > 0);
    }

    #[test]
    fn recorder_ignores_event_when_not_recording() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 10).unwrap();

        let detection = Detection {
            rule_id: "test.rule".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: json!({}),
            matched_text: "test".into(),
            span: (0, 4),
        };
        recorder.record_event(&detection, 50).unwrap();
        assert_eq!(recorder.stats().frames_written, 0);
    }

    // --- redact_detection with no secrets ---

    #[test]
    fn redact_detection_preserves_clean_text() {
        let detection = Detection {
            rule_id: "test.rule".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: json!({"key": "normal_value"}),
            matched_text: "normal text".into(),
            span: (0, 11),
        };
        let redactor = Redactor::new();
        let redacted = super::redact_detection(&detection, &redactor);
        assert_eq!(redacted.matched_text, "normal text");
        assert_eq!(
            redacted.extracted.get("key").and_then(|v| v.as_str()),
            Some("normal_value")
        );
    }

    // --- RECORDER_EVENT_SCHEMA_VERSION_V1 constant ---

    #[test]
    fn schema_version_constant() {
        assert_eq!(RECORDER_EVENT_SCHEMA_VERSION_V1, "ft.recorder.event.v1");
    }

    // ========================================================================
    // Batch 3: RubyBeaver — expanded inline unit tests
    // ========================================================================

    #[test]
    fn frame_type_serde_roundtrip() {
        let variants = [
            FrameType::Output,
            FrameType::Resize,
            FrameType::Event,
            FrameType::Marker,
            FrameType::Input,
        ];
        for v in variants {
            let serialized = serde_json::to_value(v).unwrap();
            let deserialized: FrameType = serde_json::from_value(serialized.clone()).unwrap();
            assert_eq!(v, deserialized, "roundtrip failed for {:?}", v);
        }
    }

    #[test]
    fn frame_type_copy_semantics() {
        let a = FrameType::Marker;
        let b = a; // Copy
        assert_eq!(a, b);
        // Verify all variants can be copied and remain equal
        let c = FrameType::Input;
        let d = c;
        assert_eq!(c, d);
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn recorder_state_copy_semantics() {
        let a = RecorderState::Idle;
        let b = a; // Copy
        assert_eq!(a, b);

        let c = RecorderState::Recording;
        let d = c;
        assert_eq!(c, d);

        let e = RecorderState::Paused;
        let f = e;
        assert_eq!(e, f);

        let g = RecorderState::Stopped;
        let h = g;
        assert_eq!(g, h);
    }

    #[test]
    fn recorder_state_eq_all_variants() {
        let variants = [
            RecorderState::Idle,
            RecorderState::Recording,
            RecorderState::Paused,
            RecorderState::Stopped,
        ];
        // Each variant is equal to itself and distinct from all others
        for (i, a) in variants.iter().enumerate() {
            assert_eq!(a, a);
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b, "variant {} should equal itself", i);
                } else {
                    assert_ne!(a, b, "variant {} should differ from variant {}", i, j);
                }
            }
        }
    }

    #[test]
    fn recording_options_clone() {
        let original = RecordingOptions {
            flush_threshold: 128,
            redact_output: false,
            redact_events: true,
        };
        let cloned = original;
        assert_eq!(original, cloned);
        assert_eq!(cloned.flush_threshold, 128);
        assert!(!cloned.redact_output);
        assert!(cloned.redact_events);
    }

    #[test]
    fn recording_options_debug() {
        let opts = RecordingOptions::default();
        let debug_str = format!("{:?}", opts);
        assert!(
            debug_str.contains("RecordingOptions"),
            "Debug output should contain type name, got: {}",
            debug_str
        );
    }

    #[test]
    fn recorder_event_source_serde_roundtrip() {
        let variants = [
            RecorderEventSource::WeztermMux,
            RecorderEventSource::RobotMode,
            RecorderEventSource::WorkflowEngine,
            RecorderEventSource::OperatorAction,
            RecorderEventSource::RecoveryFlow,
        ];
        for v in variants {
            let json_val = serde_json::to_value(v).unwrap();
            let back: RecorderEventSource = serde_json::from_value(json_val).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn recorder_text_encoding_serde_roundtrip() {
        let enc = RecorderTextEncoding::Utf8;
        let json_val = serde_json::to_value(enc).unwrap();
        assert_eq!(json_val, json!("utf8"));
        let back: RecorderTextEncoding = serde_json::from_value(json_val).unwrap();
        assert_eq!(enc, back);
    }

    #[test]
    fn recorder_redaction_level_serde_roundtrip() {
        let variants = [
            RecorderRedactionLevel::None,
            RecorderRedactionLevel::Partial,
            RecorderRedactionLevel::Full,
        ];
        for v in variants {
            let json_val = serde_json::to_value(v).unwrap();
            let back: RecorderRedactionLevel = serde_json::from_value(json_val).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn recorder_ingress_kind_serde_roundtrip() {
        let variants = [
            RecorderIngressKind::SendText,
            RecorderIngressKind::Paste,
            RecorderIngressKind::WorkflowAction,
        ];
        for v in variants {
            let json_val = serde_json::to_value(v).unwrap();
            let back: RecorderIngressKind = serde_json::from_value(json_val).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn recorder_segment_kind_serde_roundtrip() {
        let variants = [
            RecorderSegmentKind::Delta,
            RecorderSegmentKind::Gap,
            RecorderSegmentKind::Snapshot,
        ];
        for v in variants {
            let json_val = serde_json::to_value(v).unwrap();
            let back: RecorderSegmentKind = serde_json::from_value(json_val).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn recorder_control_marker_type_serde_roundtrip() {
        let variants = [
            RecorderControlMarkerType::PromptBoundary,
            RecorderControlMarkerType::Resize,
            RecorderControlMarkerType::PolicyDecision,
            RecorderControlMarkerType::ApprovalCheckpoint,
        ];
        for v in variants {
            let json_val = serde_json::to_value(v).unwrap();
            let back: RecorderControlMarkerType = serde_json::from_value(json_val).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn recorder_lifecycle_phase_serde_roundtrip() {
        let variants = [
            RecorderLifecyclePhase::CaptureStarted,
            RecorderLifecyclePhase::CaptureStopped,
            RecorderLifecyclePhase::PaneOpened,
            RecorderLifecyclePhase::PaneClosed,
            RecorderLifecyclePhase::ReplayStarted,
            RecorderLifecyclePhase::ReplayFinished,
        ];
        for v in variants {
            let json_val = serde_json::to_value(v).unwrap();
            let back: RecorderLifecyclePhase = serde_json::from_value(json_val).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn recorder_event_causality_serde_roundtrip() {
        let causality = RecorderEventCausality {
            parent_event_id: Some("parent-abc".into()),
            trigger_event_id: Some("trigger-def".into()),
            root_event_id: Some("root-ghi".into()),
        };
        let json_str = serde_json::to_string(&causality).unwrap();
        let back: RecorderEventCausality = serde_json::from_str(&json_str).unwrap();
        assert_eq!(causality, back);

        // Also verify all-None case
        let empty = RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        };
        let json_str2 = serde_json::to_string(&empty).unwrap();
        let back2: RecorderEventCausality = serde_json::from_str(&json_str2).unwrap();
        assert_eq!(empty, back2);
    }

    #[test]
    fn recorder_event_payload_ingress_serde_roundtrip() {
        let payload = RecorderEventPayload::IngressText {
            text: "echo hello".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Partial,
            ingress_kind: RecorderIngressKind::Paste,
        };
        let json_str = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json_str).unwrap();
        assert_eq!(payload, back);
    }

    #[test]
    fn recorder_event_payload_egress_serde_roundtrip() {
        let payload = RecorderEventPayload::EgressOutput {
            text: "drwxr-xr-x  2 user user".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Full,
            segment_kind: RecorderSegmentKind::Snapshot,
            is_gap: true,
        };
        let json_str = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json_str).unwrap();
        assert_eq!(payload, back);
    }

    #[test]
    fn recorder_event_serde_roundtrip() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: "evt-roundtrip".into(),
            pane_id: 7,
            session_id: Some("sess-rt".into()),
            workflow_id: Some("wf-rt".into()),
            correlation_id: Some("corr-rt".into()),
            source: RecorderEventSource::WorkflowEngine,
            occurred_at_ms: 5000,
            recorded_at_ms: 5001,
            sequence: 42,
            causality: RecorderEventCausality {
                parent_event_id: Some("parent-rt".into()),
                trigger_event_id: None,
                root_event_id: Some("root-rt".into()),
            },
            payload: RecorderEventPayload::EgressOutput {
                text: "output data".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        };
        let json_str = serde_json::to_string(&event).unwrap();
        let back: RecorderEvent = serde_json::from_str(&json_str).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn parse_recorder_event_json_valid_roundtrip() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: "parse-valid-rt".into(),
            pane_id: 10,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RecoveryFlow,
            occurred_at_ms: 999,
            recorded_at_ms: 1000,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: json!({"prompt": "$ "}),
            },
        };
        let json_str = serde_json::to_string(&event).unwrap();
        let parsed = parse_recorder_event_json(&json_str).unwrap();
        assert_eq!(parsed.event_id, "parse-valid-rt");
        assert_eq!(parsed.source, RecorderEventSource::RecoveryFlow);
        assert_eq!(parsed.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
    }

    #[test]
    fn parse_recorder_event_json_wrong_version_roundtrip() {
        let bad_json = json!({
            "schema_version": "ft.recorder.event.v2",
            "event_id": "bad-version",
            "pane_id": 1,
            "session_id": null,
            "workflow_id": null,
            "correlation_id": null,
            "source": "robot_mode",
            "occurred_at_ms": 0,
            "recorded_at_ms": 0,
            "sequence": 0,
            "causality": {
                "parent_event_id": null,
                "trigger_event_id": null,
                "root_event_id": null
            },
            "event_type": "ingress_text",
            "text": "x",
            "encoding": "utf8",
            "redaction": "none",
            "ingress_kind": "send_text"
        });
        let result = parse_recorder_event_json(&bad_json.to_string());
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("v2"),
            "error should mention wrong version, got: {}",
            err_msg
        );
    }

    #[test]
    fn parse_recorder_event_json_missing_version() {
        let no_version = json!({
            "event_id": "no-ver",
            "pane_id": 1,
            "source": "robot_mode",
            "occurred_at_ms": 0,
            "recorded_at_ms": 0,
            "sequence": 0,
            "causality": {
                "parent_event_id": null,
                "trigger_event_id": null,
                "root_event_id": null
            },
            "event_type": "ingress_text",
            "text": "x",
            "encoding": "utf8",
            "redaction": "none",
            "ingress_kind": "send_text"
        });
        let result = parse_recorder_event_json(&no_version.to_string());
        assert!(result.is_err(), "missing schema_version should be rejected");
    }

    #[test]
    fn ingress_outcome_clone_debug() {
        let variants: Vec<IngressOutcome> = vec![
            IngressOutcome::Allowed,
            IngressOutcome::Denied {
                reason: "blocked by policy".into(),
            },
            IngressOutcome::RequiresApproval,
            IngressOutcome::Error {
                error: "connection lost".into(),
            },
        ];
        for v in &variants {
            let cloned = v.clone();
            assert_eq!(v, &cloned);
            let debug_str = format!("{:?}", v);
            assert!(!debug_str.is_empty());
        }
        // Verify Debug output contains variant-specific info
        let denied_debug = format!("{:?}", variants[1]);
        assert!(
            denied_debug.contains("Denied"),
            "Debug should contain variant name, got: {}",
            denied_debug
        );
        let error_debug = format!("{:?}", variants[3]);
        assert!(
            error_debug.contains("connection lost"),
            "Debug should contain error message, got: {}",
            error_debug
        );
    }

    #[test]
    fn ingress_event_clone() {
        let event = IngressEvent {
            pane_id: 99,
            text: "test-clone".into(),
            source: RecorderEventSource::OperatorAction,
            ingress_kind: RecorderIngressKind::Paste,
            redaction: RecorderRedactionLevel::Full,
            occurred_at_ms: 12345,
            outcome: IngressOutcome::RequiresApproval,
            workflow_id: Some("wf-clone".into()),
        };
        let cloned = event.clone();
        assert_eq!(cloned.pane_id, 99);
        assert_eq!(cloned.text, "test-clone");
        assert_eq!(cloned.source, RecorderEventSource::OperatorAction);
        assert_eq!(cloned.ingress_kind, RecorderIngressKind::Paste);
        assert_eq!(cloned.redaction, RecorderRedactionLevel::Full);
        assert_eq!(cloned.occurred_at_ms, 12345);
        assert_eq!(cloned.outcome, IngressOutcome::RequiresApproval);
        assert_eq!(cloned.workflow_id, Some("wf-clone".into()));
    }

    #[test]
    fn global_sequence_monotonic_batch3() {
        let seq = GlobalSequence::new();
        let mut prev = seq.next();
        for _ in 0..100 {
            let current = seq.next();
            assert!(
                current > prev,
                "sequence should be strictly monotonic: {} > {}",
                current,
                prev
            );
            prev = current;
        }
    }

    #[test]
    fn global_sequence_default_batch3() {
        let seq = GlobalSequence::default();
        let first = seq.next();
        assert_eq!(first, 0, "default sequence should start at 0");
        let second = seq.next();
        assert_eq!(second, 1);
    }

    #[test]
    fn captured_kind_to_segment_all_variants() {
        // Delta variant
        let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Delta);
        assert_eq!(kind, RecorderSegmentKind::Delta);
        assert!(!is_gap);

        // Gap variant
        let gap = CapturedSegmentKind::Gap {
            reason: "poll timeout".into(),
        };
        let (kind2, is_gap2) = captured_kind_to_segment(&gap);
        assert_eq!(kind2, RecorderSegmentKind::Gap);
        assert!(is_gap2);
    }

    #[test]
    fn collecting_egress_tap_accumulates_batch3() {
        let tap = CollectingEgressTap::new();
        assert!(tap.is_empty());
        assert_eq!(tap.len(), 0);

        for i in 0..5u64 {
            tap.on_egress(EgressEvent {
                pane_id: i,
                text: format!("output-{}", i),
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
                gap_reason: None,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                occurred_at_ms: i * 100,
                sequence: i,
                global_sequence: i,
            });
        }

        assert_eq!(tap.len(), 5);
        assert!(!tap.is_empty());

        let events = tap.events();
        assert_eq!(events.len(), 5);
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.pane_id, i as u64);
            assert_eq!(ev.text, format!("output-{}", i));
        }
    }

    #[test]
    fn egress_noop_tap_is_zero_cost() {
        let tap = EgressNoopTap;
        // Call with various segment kinds; should never panic
        for kind in [
            RecorderSegmentKind::Delta,
            RecorderSegmentKind::Gap,
            RecorderSegmentKind::Snapshot,
        ] {
            tap.on_egress(EgressEvent {
                pane_id: 0,
                text: String::new(),
                segment_kind: kind,
                is_gap: kind == RecorderSegmentKind::Gap,
                gap_reason: if kind == RecorderSegmentKind::Gap {
                    Some("test".into())
                } else {
                    None
                },
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                occurred_at_ms: 0,
                sequence: 0,
                global_sequence: 0,
            });
        }
    }
}
