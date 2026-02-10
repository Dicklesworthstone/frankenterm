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

    #[tokio::test]
    async fn recording_manager_redacts_output() {
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

    #[tokio::test]
    async fn recording_manager_multi_pane() {
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
    }

    #[tokio::test]
    async fn recording_manager_duplicate_start_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");

        let manager = RecordingManager::new(RecordingOptions::default());
        manager.start_recording(1, &path, 0).await.unwrap();

        let result = manager.start_recording(1, &path, 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recording_manager_stop_nonexistent_returns_none() {
        let manager = RecordingManager::new(RecordingOptions::default());
        let result = manager.stop_recording(999).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn recording_manager_segment_for_unknown_pane_is_noop() {
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
    }
}
