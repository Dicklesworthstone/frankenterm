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
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

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
#[derive(Debug, Clone, Copy)]
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

        let mut payload = segment.content.as_bytes().to_vec();
        if self.options.redact_output {
            let redacted = self.redactor.redact(&segment.content);
            payload = redacted.into_bytes();
        }

        let is_gap = matches!(segment.kind, CapturedSegmentKind::Gap { .. });
        recorder.bytes_raw += segment.content.as_bytes().len() as u64;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Severity};
    use serde_json::json;
    use tempfile::tempdir;

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
        let detection = Detection {
            rule_id: "test.rule".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({ "token": "sk-secret-value" }),
            matched_text: "sk-secret-value".to_string(),
            span: (0, 5),
        };

        let redactor = Redactor::new();
        let redacted = super::redact_detection(&detection, &redactor);
        assert!(!redacted.matched_text.contains("sk-secret-value"));
        let serialized = serde_json::to_string(&redacted.extracted).unwrap();
        assert!(!serialized.contains("sk-secret-value"));
    }

    #[tokio::test]
    async fn recording_manager_redacts_output() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");

        let manager = RecordingManager::new(RecordingOptions {
            flush_threshold: 1,
            redact_output: true,
            redact_events: false,
        });

        manager.start_recording(1, &path, 0).await.unwrap();
        let segment = CapturedSegment {
            pane_id: 1,
            seq: 0,
            content: "token sk-secret-value".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 10,
        };
        manager.record_segment(&segment).await.unwrap();
        manager.stop_recording(1).await.unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("sk-secret-value"));
        assert!(text.contains("[REDACTED]"));
    }
}
