//! Replay engine for wa session recordings.
//!
//! Reads `.war` recordings written by [`crate::recording`] and plays them back
//! with speed control, pause/resume, and seeking.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::recording::{FrameHeader, FrameType, RecordingFrame};
use crate::Result;

// ---------------------------------------------------------------------------
// Frame parsing
// ---------------------------------------------------------------------------

/// Size of the binary frame header (timestamp_ms + type + flags + payload_len).
const FRAME_HEADER_LEN: usize = 14;

/// Default keyframe interval (one keyframe every N output frames).
const KEYFRAME_INTERVAL: usize = 50;

/// Parse a single [`RecordingFrame`] from a byte slice starting at `offset`.
///
/// Returns the parsed frame and the offset immediately after it.
fn parse_frame(data: &[u8], offset: usize) -> crate::Result<(RecordingFrame, usize)> {
    if data.len() < offset + FRAME_HEADER_LEN {
        return Err(crate::Error::Runtime(
            "unexpected EOF reading frame header".into(),
        ));
    }

    let ts = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    let ft_byte = data[offset + 8];
    let flags = data[offset + 9];
    let payload_len =
        u32::from_le_bytes(data[offset + 10..offset + 14].try_into().unwrap()) as usize;

    let frame_type = match ft_byte {
        1 => FrameType::Output,
        2 => FrameType::Resize,
        3 => FrameType::Event,
        4 => FrameType::Marker,
        5 => FrameType::Input,
        other => {
            return Err(crate::Error::Runtime(format!(
                "unknown frame type byte {other}"
            )));
        }
    };

    let payload_start = offset + FRAME_HEADER_LEN;
    let payload_end = payload_start + payload_len;
    if data.len() < payload_end {
        return Err(crate::Error::Runtime(
            "unexpected EOF reading frame payload".into(),
        ));
    }

    let frame = RecordingFrame {
        header: FrameHeader {
            timestamp_ms: ts,
            frame_type,
            flags,
            payload_len: payload_len as u32,
        },
        payload: data[payload_start..payload_end].to_vec(),
    };

    Ok((frame, payload_end))
}

// ---------------------------------------------------------------------------
// Recording container
// ---------------------------------------------------------------------------

/// Keyframe entry for fast seeking.
#[derive(Debug, Clone, Copy)]
struct KeyframeEntry {
    /// Index into `Recording::frames`.
    frame_index: usize,
    /// Timestamp of this keyframe (ms since recording start).
    timestamp_ms: u64,
}

/// A loaded recording ready for playback.
#[derive(Debug, Clone)]
pub struct Recording {
    /// All parsed frames, in order.
    pub frames: Vec<RecordingFrame>,
    /// Keyframe index for seeking (built on load).
    keyframes: Vec<KeyframeEntry>,
    /// Total duration in milliseconds (timestamp of last frame).
    pub duration_ms: u64,
}

impl Recording {
    /// Load a recording from the given `.war` file path.
    pub fn load(path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Self::from_bytes(&data)
    }

    /// Parse a recording from raw bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut frames = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let (frame, next_offset) = parse_frame(data, offset)?;
            frames.push(frame);
            offset = next_offset;
        }

        let keyframes = build_keyframe_index(&frames);
        let duration_ms = frames.last().map_or(0, |f| f.header.timestamp_ms);

        Ok(Self {
            frames,
            keyframes,
            duration_ms,
        })
    }
}

/// Build a keyframe index: every `KEYFRAME_INTERVAL`-th output frame.
fn build_keyframe_index(frames: &[RecordingFrame]) -> Vec<KeyframeEntry> {
    let mut keyframes = Vec::new();
    let mut output_count = 0usize;

    for (i, frame) in frames.iter().enumerate() {
        if frame.header.frame_type == FrameType::Output {
            if output_count % KEYFRAME_INTERVAL == 0 {
                keyframes.push(KeyframeEntry {
                    frame_index: i,
                    timestamp_ms: frame.header.timestamp_ms,
                });
            }
            output_count += 1;
        }
    }

    // Always include the very first frame if not already present.
    if keyframes.is_empty() && !frames.is_empty() {
        keyframes.push(KeyframeEntry {
            frame_index: 0,
            timestamp_ms: frames[0].header.timestamp_ms,
        });
    }

    keyframes
}

// ---------------------------------------------------------------------------
// Decoded frame output
// ---------------------------------------------------------------------------

/// A decoded frame payload, ready for output.
#[derive(Debug, Clone)]
pub enum DecodedFrame {
    /// Terminal output bytes.
    Output(Vec<u8>),
    /// Terminal resize (cols, rows).
    Resize { cols: u16, rows: u16 },
    /// Detection event (JSON payload).
    Event(serde_json::Value),
    /// User marker/annotation.
    Marker(String),
    /// Captured input (redacted).
    Input(Vec<u8>),
}

/// Decode a [`RecordingFrame`] into its semantic representation.
pub fn decode_frame(frame: &RecordingFrame) -> Result<DecodedFrame> {
    match frame.header.frame_type {
        FrameType::Output => Ok(DecodedFrame::Output(frame.payload.clone())),
        FrameType::Resize => {
            if frame.payload.len() >= 4 {
                let cols = u16::from_le_bytes(
                    frame.payload[0..2].try_into().unwrap(),
                );
                let rows = u16::from_le_bytes(
                    frame.payload[2..4].try_into().unwrap(),
                );
                Ok(DecodedFrame::Resize { cols, rows })
            } else {
                Err(crate::Error::Runtime(
                    "resize frame payload too short".into(),
                ))
            }
        }
        FrameType::Event => {
            let value: serde_json::Value = serde_json::from_slice(&frame.payload)?;
            Ok(DecodedFrame::Event(value))
        }
        FrameType::Marker => {
            let text = String::from_utf8_lossy(&frame.payload).into_owned();
            Ok(DecodedFrame::Marker(text))
        }
        FrameType::Input => Ok(DecodedFrame::Input(frame.payload.clone())),
    }
}

// ---------------------------------------------------------------------------
// Output sink trait
// ---------------------------------------------------------------------------

/// Destination for decoded playback frames.
pub trait OutputSink: Send {
    /// Write terminal output bytes.
    fn write_output(&mut self, bytes: &[u8]) -> Result<()>;

    /// Show a detection event annotation.
    fn show_event(&mut self, event: &serde_json::Value) -> Result<()>;

    /// Show a user marker/annotation.
    fn show_marker(&mut self, text: &str) -> Result<()>;
}

/// A no-op sink that discards output (useful for testing or seeking).
pub struct HeadlessSink;

impl OutputSink for HeadlessSink {
    fn write_output(&mut self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
    fn show_event(&mut self, _event: &serde_json::Value) -> Result<()> {
        Ok(())
    }
    fn show_marker(&mut self, _text: &str) -> Result<()> {
        Ok(())
    }
}

/// Sink that writes terminal output to stdout.
pub struct TerminalSink;

impl OutputSink for TerminalSink {
    fn write_output(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        std::io::stdout().write_all(bytes)?;
        std::io::stdout().flush()?;
        Ok(())
    }

    fn show_event(&mut self, event: &serde_json::Value) -> Result<()> {
        eprintln!("[event] {event}");
        Ok(())
    }

    fn show_marker(&mut self, text: &str) -> Result<()> {
        eprintln!("[marker] {text}");
        Ok(())
    }
}

/// Sink that collects output bytes in memory (for testing).
pub struct CollectorSink {
    pub output: Vec<u8>,
    pub events: Vec<serde_json::Value>,
    pub markers: Vec<String>,
}

impl CollectorSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            events: Vec::new(),
            markers: Vec::new(),
        }
    }
}

impl Default for CollectorSink {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputSink for CollectorSink {
    fn write_output(&mut self, bytes: &[u8]) -> Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn show_event(&mut self, event: &serde_json::Value) -> Result<()> {
        self.events.push(event.clone());
        Ok(())
    }

    fn show_marker(&mut self, text: &str) -> Result<()> {
        self.markers.push(text.to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------------

/// Playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
    Finished,
}

/// Current playback position.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackPosition {
    pub frame_index: usize,
    pub timestamp_ms: u64,
}

/// Playback speed multiplier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackSpeed(f32);

impl PlaybackSpeed {
    pub const HALF: Self = Self(0.5);
    pub const NORMAL: Self = Self(1.0);
    pub const DOUBLE: Self = Self(2.0);
    pub const QUAD: Self = Self(4.0);

    /// Create a custom speed multiplier. Must be > 0.
    pub fn new(speed: f32) -> Result<Self> {
        if speed <= 0.0 {
            return Err(crate::Error::Runtime(
                "playback speed must be > 0".into(),
            ));
        }
        Ok(Self(speed))
    }

    #[must_use]
    pub fn as_f32(self) -> f32 {
        self.0
    }
}

impl Default for PlaybackSpeed {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// Control signal sent to the player via `watch` channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerControl {
    /// Continue playing.
    Play,
    /// Pause playback.
    Pause,
    /// Stop (terminate) playback.
    Stop,
    /// Change speed.
    SetSpeed(PlaybackSpeed),
}

/// Session replay player.
pub struct Player {
    recording: Recording,
    position: PlaybackPosition,
    speed: PlaybackSpeed,
    state: PlayerState,
}

impl Player {
    /// Create a new player for the given recording.
    #[must_use]
    pub fn new(recording: Recording) -> Self {
        Self {
            recording,
            position: PlaybackPosition {
                frame_index: 0,
                timestamp_ms: 0,
            },
            speed: PlaybackSpeed::NORMAL,
            state: PlayerState::Stopped,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> PlayerState {
        self.state
    }

    /// Current position.
    #[must_use]
    pub fn position(&self) -> PlaybackPosition {
        self.position
    }

    /// Total frames in the recording.
    #[must_use]
    pub fn total_frames(&self) -> usize {
        self.recording.frames.len()
    }

    /// Recording duration in ms.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.recording.duration_ms
    }

    /// Set playback speed.
    pub fn set_speed(&mut self, speed: PlaybackSpeed) {
        self.speed = speed;
    }

    /// Seek to the given timestamp (ms from recording start).
    ///
    /// Replays output frames silently from the nearest keyframe to the target
    /// timestamp so that the output state is correct.
    pub fn seek_to(&mut self, timestamp_ms: u64, sink: &mut dyn OutputSink) -> Result<()> {
        // Find nearest keyframe at or before the target timestamp.
        let keyframe = self
            .recording
            .keyframes
            .iter()
            .rev()
            .find(|kf| kf.timestamp_ms <= timestamp_ms)
            .copied()
            .unwrap_or(KeyframeEntry {
                frame_index: 0,
                timestamp_ms: 0,
            });

        // Replay from keyframe to target (output only, no delays).
        for i in keyframe.frame_index..self.recording.frames.len() {
            let frame = &self.recording.frames[i];
            if frame.header.timestamp_ms > timestamp_ms {
                self.position = PlaybackPosition {
                    frame_index: i,
                    timestamp_ms,
                };
                return Ok(());
            }

            // Silently apply output frames to rebuild terminal state.
            if frame.header.frame_type == FrameType::Output {
                let decoded = decode_frame(frame)?;
                if let DecodedFrame::Output(bytes) = decoded {
                    sink.write_output(&bytes)?;
                }
            }
        }

        // Target is at or beyond the end.
        self.position = PlaybackPosition {
            frame_index: self.recording.frames.len(),
            timestamp_ms: self.recording.duration_ms,
        };
        self.state = PlayerState::Finished;
        Ok(())
    }

    /// Handle a control signal. Returns `true` if playback should stop.
    async fn handle_control(
        &mut self,
        ctrl: PlayerControl,
        control_rx: &mut watch::Receiver<PlayerControl>,
    ) -> Result<bool> {
        match ctrl {
            PlayerControl::Stop => {
                self.state = PlayerState::Stopped;
                Ok(true)
            }
            PlayerControl::Pause => {
                self.state = PlayerState::Paused;
                loop {
                    control_rx.changed().await.map_err(|_| {
                        crate::Error::Runtime("control channel closed".into())
                    })?;
                    let sig = *control_rx.borrow();
                    match sig {
                        PlayerControl::Play => {
                            self.state = PlayerState::Playing;
                            return Ok(false);
                        }
                        PlayerControl::Stop => {
                            self.state = PlayerState::Stopped;
                            return Ok(true);
                        }
                        PlayerControl::SetSpeed(s) => self.speed = s,
                        PlayerControl::Pause => {}
                    }
                }
            }
            PlayerControl::SetSpeed(s) => {
                self.speed = s;
                Ok(false)
            }
            PlayerControl::Play => Ok(false),
        }
    }

    /// Play the recording from the current position with timing delays.
    ///
    /// A `watch::Receiver<PlayerControl>` is used for external control
    /// (pause, stop, speed change) without polling overhead.
    pub async fn play(
        &mut self,
        sink: &mut dyn OutputSink,
        mut control_rx: watch::Receiver<PlayerControl>,
    ) -> Result<()> {
        self.state = PlayerState::Playing;

        while self.position.frame_index < self.recording.frames.len() {
            // Check for control signals.
            if let Some(ctrl) = check_control(&mut control_rx) {
                if self.handle_control(ctrl, &mut control_rx).await? {
                    return Ok(());
                }
            }

            // Read frame timestamp and decode before any &mut self calls.
            let frame_ts = self.recording.frames[self.position.frame_index]
                .header
                .timestamp_ms;

            // Compute delay based on speed.
            if frame_ts > self.position.timestamp_ms {
                let raw_delay_ms = frame_ts - self.position.timestamp_ms;
                let scaled_delay = (raw_delay_ms as f64) / (self.speed.as_f32() as f64);
                if scaled_delay > 0.5 {
                    tokio::time::sleep(Duration::from_micros((scaled_delay * 1000.0) as u64))
                        .await;
                }

                // Re-check controls after sleep (signal may have arrived during delay).
                if let Some(ctrl) = check_control(&mut control_rx) {
                    if self.handle_control(ctrl, &mut control_rx).await? {
                        return Ok(());
                    }
                }
            }

            // Decode and output (re-borrow after potential &mut self above).
            let decoded = decode_frame(&self.recording.frames[self.position.frame_index])?;
            output_decoded(sink, &decoded)?;

            self.position = PlaybackPosition {
                frame_index: self.position.frame_index + 1,
                timestamp_ms: frame_ts,
            };
        }

        self.state = PlayerState::Finished;
        Ok(())
    }

    /// Play the recording without external controls (convenience wrapper).
    pub async fn play_simple(&mut self, sink: &mut dyn OutputSink) -> Result<()> {
        let (_tx, rx) = watch::channel(PlayerControl::Play);
        self.play(sink, rx).await
    }
}

/// Poll the control channel for the latest signal (non-blocking).
fn check_control(rx: &mut watch::Receiver<PlayerControl>) -> Option<PlayerControl> {
    // has_changed() returns Err if sender is dropped; treat as no change.
    if rx.has_changed().unwrap_or(false) {
        Some(*rx.borrow_and_update())
    } else {
        None
    }
}

/// Route a decoded frame to the appropriate sink method.
fn output_decoded(sink: &mut dyn OutputSink, decoded: &DecodedFrame) -> Result<()> {
    match decoded {
        DecodedFrame::Output(bytes) => sink.write_output(bytes),
        DecodedFrame::Resize { .. } => Ok(()), // resize handled by caller
        DecodedFrame::Event(event) => sink.show_event(event),
        DecodedFrame::Marker(text) => sink.show_marker(text),
        DecodedFrame::Input(_) => Ok(()), // input frames are informational
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{FrameHeader, FrameType, RecordingFrame};
    use serde_json::json;

    /// Build a test recording from frame specs: (timestamp_ms, frame_type, payload).
    fn build_recording(
        specs: &[(u64, FrameType, Vec<u8>)],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        for (ts, ft, payload) in specs {
            let frame = RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: *ts,
                    frame_type: *ft,
                    flags: 0,
                    payload_len: payload.len() as u32,
                },
                payload: payload.clone(),
            };
            data.extend(frame.encode());
        }
        data
    }

    #[test]
    fn parse_single_frame() {
        let payload = b"hello".to_vec();
        let data = build_recording(&[(100, FrameType::Output, payload.clone())]);

        let (frame, next) = parse_frame(&data, 0).unwrap();
        assert_eq!(frame.header.timestamp_ms, 100);
        assert_eq!(frame.header.frame_type, FrameType::Output);
        assert_eq!(frame.payload, payload);
        assert_eq!(next, data.len());
    }

    #[test]
    fn parse_multiple_frames() {
        let data = build_recording(&[
            (0, FrameType::Output, b"first".to_vec()),
            (50, FrameType::Output, b"second".to_vec()),
            (100, FrameType::Event, b"{}".to_vec()),
        ]);

        let recording = Recording::from_bytes(&data).unwrap();
        assert_eq!(recording.frames.len(), 3);
        assert_eq!(recording.duration_ms, 100);
    }

    #[test]
    fn parse_empty_recording() {
        let recording = Recording::from_bytes(&[]).unwrap();
        assert!(recording.frames.is_empty());
        assert_eq!(recording.duration_ms, 0);
    }

    #[test]
    fn parse_frame_truncated_header() {
        let result = parse_frame(&[0u8; 10], 0);
        assert!(result.is_err());
    }

    #[test]
    fn parse_frame_truncated_payload() {
        // Header says payload is 100 bytes but data ends after header.
        let mut data = vec![0u8; FRAME_HEADER_LEN];
        data[8] = FrameType::Output as u8; // frame_type
        data[10..14].copy_from_slice(&100u32.to_le_bytes()); // payload_len = 100
        let result = parse_frame(&data, 0);
        assert!(result.is_err());
    }

    #[test]
    fn decode_output_frame() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 5,
            },
            payload: b"hello".to_vec(),
        };
        let decoded = decode_frame(&frame).unwrap();
        assert!(matches!(decoded, DecodedFrame::Output(ref b) if b == b"hello"));
    }

    #[test]
    fn decode_resize_frame() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&80u16.to_le_bytes());
        payload.extend_from_slice(&24u16.to_le_bytes());
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Resize,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        };
        let decoded = decode_frame(&frame).unwrap();
        assert!(matches!(decoded, DecodedFrame::Resize { cols: 80, rows: 24 }));
    }

    #[test]
    fn decode_event_frame() {
        let event = json!({"rule_id": "test.rule"});
        let payload = serde_json::to_vec(&event).unwrap();
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Event,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        };
        let decoded = decode_frame(&frame).unwrap();
        if let DecodedFrame::Event(v) = decoded {
            assert_eq!(v["rule_id"], "test.rule");
        } else {
            panic!("expected Event");
        }
    }

    #[test]
    fn decode_marker_frame() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Marker,
                flags: 0,
                payload_len: 4,
            },
            payload: b"note".to_vec(),
        };
        let decoded = decode_frame(&frame).unwrap();
        assert!(matches!(decoded, DecodedFrame::Marker(ref s) if s == "note"));
    }

    #[test]
    fn collector_sink_collects_output() {
        let mut sink = CollectorSink::new();
        sink.write_output(b"abc").unwrap();
        sink.write_output(b"def").unwrap();
        sink.show_event(&json!({"x": 1})).unwrap();
        sink.show_marker("mark").unwrap();

        assert_eq!(sink.output, b"abcdef");
        assert_eq!(sink.events.len(), 1);
        assert_eq!(sink.markers, vec!["mark"]);
    }

    #[test]
    fn keyframe_index_built() {
        // Build 120 output frames; expect keyframes at 0, 50, 100.
        let specs: Vec<_> = (0..120)
            .map(|i| (i as u64 * 10, FrameType::Output, vec![b'x']))
            .collect();
        let data = build_recording(&specs);
        let recording = Recording::from_bytes(&data).unwrap();

        assert_eq!(recording.keyframes.len(), 3);
        assert_eq!(recording.keyframes[0].frame_index, 0);
        assert_eq!(recording.keyframes[1].frame_index, 50);
        assert_eq!(recording.keyframes[2].frame_index, 100);
    }

    #[test]
    fn keyframe_index_single_frame() {
        let data = build_recording(&[(0, FrameType::Output, b"x".to_vec())]);
        let recording = Recording::from_bytes(&data).unwrap();
        assert_eq!(recording.keyframes.len(), 1);
        assert_eq!(recording.keyframes[0].frame_index, 0);
    }

    #[test]
    fn seek_to_beginning() {
        let data = build_recording(&[
            (0, FrameType::Output, b"first".to_vec()),
            (100, FrameType::Output, b"second".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = HeadlessSink;

        player.seek_to(0, &mut sink).unwrap();
        // Position should be at the first frame whose timestamp > 0,
        // after having replayed the frame at timestamp 0.
        assert_eq!(player.position().frame_index, 1);
    }

    #[test]
    fn seek_to_middle() {
        let specs: Vec<_> = (0..10)
            .map(|i| (i * 100, FrameType::Output, format!("frame{i}").into_bytes()))
            .collect();
        let data = build_recording(&specs);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = CollectorSink::new();

        player.seek_to(450, &mut sink).unwrap();
        assert_eq!(player.position().frame_index, 5);
        // Frames 0-4 (timestamps 0, 100, 200, 300, 400) should have been replayed.
        assert!(sink.output.len() > 0);
    }

    #[test]
    fn seek_beyond_end() {
        let data = build_recording(&[
            (0, FrameType::Output, b"only".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = HeadlessSink;

        player.seek_to(99999, &mut sink).unwrap();
        assert_eq!(player.state(), PlayerState::Finished);
    }

    #[test]
    fn playback_speed_validation() {
        assert!(PlaybackSpeed::new(0.0).is_err());
        assert!(PlaybackSpeed::new(-1.0).is_err());
        assert!(PlaybackSpeed::new(0.1).is_ok());
    }

    #[tokio::test]
    async fn play_simple_all_frames() {
        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (10, FrameType::Output, b"B".to_vec()),
            (20, FrameType::Marker, b"done".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::QUAD); // fast
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(player.state(), PlayerState::Finished);
        assert_eq!(sink.output, b"AB");
        assert_eq!(sink.markers, vec!["done"]);
    }

    #[tokio::test]
    async fn play_with_stop_control() {
        tokio::time::pause();

        // Large delays between frames so stop arrives before frame C.
        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (5000, FrameType::Output, b"B".to_vec()),
            (10000, FrameType::Output, b"C".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);

        let (tx, rx) = watch::channel(PlayerControl::Play);
        let mut sink = CollectorSink::new();

        // Keep tx alive in main task; clone for spawned task.
        let tx2 = tx.clone();
        tokio::spawn(async move {
            // Stop fires at t=1s, before frame B at t=5s.
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let _ = tx2.send(PlayerControl::Stop);
        });

        player.play(&mut sink, rx).await.unwrap();
        assert_eq!(player.state(), PlayerState::Stopped);
        // Only frame A (at t=0) should have been output.
        assert_eq!(sink.output, b"A");
        drop(tx); // explicit drop after assertions
    }

    #[tokio::test]
    async fn play_deterministic_timing() {
        // Use tokio::time::pause for deterministic timing tests.
        tokio::time::pause();

        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (100, FrameType::Output, b"B".to_vec()),
            (200, FrameType::Output, b"C".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        // Normal speed (1x).
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(player.state(), PlayerState::Finished);
        assert_eq!(sink.output, b"ABC");
        assert_eq!(player.position().frame_index, 3);
    }

    #[tokio::test]
    async fn play_double_speed() {
        tokio::time::pause();

        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (1000, FrameType::Output, b"B".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::DOUBLE);
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(sink.output, b"AB");
        assert_eq!(player.state(), PlayerState::Finished);
    }

    #[tokio::test]
    async fn play_with_events_and_markers() {
        let event_payload = serde_json::to_vec(&json!({"rule": "test"})).unwrap();
        let data = build_recording(&[
            (0, FrameType::Output, b"text".to_vec()),
            (50, FrameType::Event, event_payload),
            (100, FrameType::Marker, b"annotation".to_vec()),
            (150, FrameType::Output, b"more".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::QUAD);
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(sink.output, b"textmore");
        assert_eq!(sink.events.len(), 1);
        assert_eq!(sink.events[0]["rule"], "test");
        assert_eq!(sink.markers, vec!["annotation"]);
    }

    #[test]
    fn recording_roundtrip() {
        // Frames written by Recorder can be parsed by Recording.
        let specs = vec![
            (0u64, FrameType::Output, b"hello world".to_vec()),
            (42, FrameType::Resize, {
                let mut p = Vec::new();
                p.extend_from_slice(&120u16.to_le_bytes());
                p.extend_from_slice(&40u16.to_le_bytes());
                p
            }),
            (100, FrameType::Event, serde_json::to_vec(&json!({"id": 1})).unwrap()),
            (200, FrameType::Marker, b"checkpoint".to_vec()),
            (300, FrameType::Input, b"ls -la\n".to_vec()),
        ];
        let data = build_recording(&specs);
        let recording = Recording::from_bytes(&data).unwrap();

        assert_eq!(recording.frames.len(), 5);
        assert_eq!(recording.duration_ms, 300);

        // Verify each frame type decodes correctly.
        let d0 = decode_frame(&recording.frames[0]).unwrap();
        assert!(matches!(d0, DecodedFrame::Output(ref b) if b == b"hello world"));

        let d1 = decode_frame(&recording.frames[1]).unwrap();
        assert!(matches!(d1, DecodedFrame::Resize { cols: 120, rows: 40 }));

        let d2 = decode_frame(&recording.frames[2]).unwrap();
        if let DecodedFrame::Event(v) = d2 {
            assert_eq!(v["id"], 1);
        } else {
            panic!("expected event");
        }

        let d3 = decode_frame(&recording.frames[3]).unwrap();
        assert!(matches!(d3, DecodedFrame::Marker(ref s) if s == "checkpoint"));

        let d4 = decode_frame(&recording.frames[4]).unwrap();
        assert!(matches!(d4, DecodedFrame::Input(ref b) if b == b"ls -la\n"));
    }
}
