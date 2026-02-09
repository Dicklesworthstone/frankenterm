//! Replay engine for wa session recordings.
//!
//! Reads `.war` recordings written by [`crate::recording`] and plays them back
//! with speed control, pause/resume, and seeking.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::Result;
use crate::recording::{FrameHeader, FrameType, RecordingFrame};

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
                let cols = u16::from_le_bytes(frame.payload[0..2].try_into().unwrap());
                let rows = u16::from_le_bytes(frame.payload[2..4].try_into().unwrap());
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
///
/// # One-writer rule
///
/// This sink writes directly to stdout/stderr and must NOT be used while
/// the TUI rendering pipeline is active (`GatePhase::Active`).  A
/// `debug_assert!` fires if the output gate is suppressed.
pub struct TerminalSink;

impl OutputSink for TerminalSink {
    fn write_output(&mut self, bytes: &[u8]) -> Result<()> {
        #[cfg(any(feature = "tui", feature = "ftui"))]
        debug_assert!(
            !crate::tui::output_gate::is_output_suppressed(),
            "TerminalSink::write_output called while TUI output gate is active"
        );
        use std::io::Write;
        std::io::stdout().write_all(bytes)?;
        std::io::stdout().flush()?;
        Ok(())
    }

    fn show_event(&mut self, event: &serde_json::Value) -> Result<()> {
        #[cfg(any(feature = "tui", feature = "ftui"))]
        debug_assert!(
            !crate::tui::output_gate::is_output_suppressed(),
            "TerminalSink::show_event called while TUI output gate is active"
        );
        eprintln!("[event] {event}");
        Ok(())
    }

    fn show_marker(&mut self, text: &str) -> Result<()> {
        #[cfg(any(feature = "tui", feature = "ftui"))]
        debug_assert!(
            !crate::tui::output_gate::is_output_suppressed(),
            "TerminalSink::show_marker called while TUI output gate is active"
        );
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
            return Err(crate::Error::Runtime("playback speed must be > 0".into()));
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
                    control_rx
                        .changed()
                        .await
                        .map_err(|_| crate::Error::Runtime("control channel closed".into()))?;
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
                    tokio::time::sleep(Duration::from_micros((scaled_delay * 1000.0) as u64)).await;
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
// Recording Export: Asciinema V2 cast + standalone HTML
// ---------------------------------------------------------------------------

/// Options for exporting a recording.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Terminal columns (for cast header).
    pub cols: u16,
    /// Terminal rows (for cast header).
    pub rows: u16,
    /// Apply secret redaction to output frames.
    pub redact: bool,
    /// Additional regex patterns to redact (beyond built-in secrets).
    pub extra_redact_patterns: Vec<String>,
    /// Title for the recording (used in cast header and HTML).
    pub title: Option<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            redact: true,
            extra_redact_patterns: Vec::new(),
            title: None,
        }
    }
}

/// Export format for recordings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Asciinema V2 cast format (.cast)
    Asciinema,
    /// Self-contained HTML player (.html)
    Html,
}

/// Redact the output bytes of a recording, returning a new Recording with
/// all `Output` and `Input` frame payloads redacted.
fn redact_recording(recording: &Recording, redact_extra: &[String]) -> Result<Recording> {
    use crate::policy::Redactor;

    let redactor = Redactor::new();
    let mut extra_patterns: Vec<regex::Regex> = Vec::new();
    for pat in redact_extra {
        let re = regex::Regex::new(pat).map_err(|e| {
            crate::Error::Runtime(format!("Invalid redaction pattern '{pat}': {e}"))
        })?;
        extra_patterns.push(re);
    }

    let mut new_frames = Vec::with_capacity(recording.frames.len());
    for frame in &recording.frames {
        match frame.header.frame_type {
            FrameType::Output | FrameType::Input => {
                let text = String::from_utf8_lossy(&frame.payload);
                let mut redacted = redactor.redact(&text);
                for re in &extra_patterns {
                    redacted = re.replace_all(&redacted, "[REDACTED]").to_string();
                }
                let payload = redacted.into_bytes();
                new_frames.push(RecordingFrame {
                    header: FrameHeader {
                        payload_len: payload.len() as u32,
                        ..frame.header
                    },
                    payload,
                });
            }
            _ => {
                new_frames.push(frame.clone());
            }
        }
    }

    Ok(Recording {
        duration_ms: recording.duration_ms,
        frames: new_frames,
        keyframes: Vec::new(), // keyframes not needed for export
    })
}

/// Export a recording to Asciinema V2 cast format.
///
/// The V2 format is newline-delimited JSON:
/// - Line 1: header object `{"version": 2, "width": N, "height": N, ...}`
/// - Subsequent lines: event arrays `[time, "o", "data"]`
///
/// See <https://docs.asciinema.org/manual/asciicast/v2/> for the specification.
pub fn export_asciinema<W: std::io::Write>(
    recording: &Recording,
    opts: &ExportOptions,
    writer: &mut W,
) -> Result<usize> {
    let source = if opts.redact {
        redact_recording(recording, &opts.extra_redact_patterns)?
    } else {
        recording.clone()
    };

    // Determine terminal size from resize frames or defaults
    let (cols, rows) = find_terminal_size(&source, opts.cols, opts.rows);

    // Write header
    let mut header = serde_json::Map::new();
    header.insert("version".into(), serde_json::Value::Number(2.into()));
    header.insert("width".into(), serde_json::Value::Number(cols.into()));
    header.insert("height".into(), serde_json::Value::Number(rows.into()));
    if let Some(ref title) = opts.title {
        header.insert("title".into(), serde_json::Value::String(title.clone()));
    }
    if source.duration_ms > 0 {
        let duration_secs = source.duration_ms as f64 / 1000.0;
        header.insert(
            "duration".into(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(duration_secs)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        );
    }
    let header_json = serde_json::to_string(&header)
        .map_err(|e| crate::Error::Runtime(format!("Failed to serialize cast header: {e}")))?;
    writeln!(writer, "{header_json}")
        .map_err(|e| crate::Error::Runtime(format!("Failed to write cast header: {e}")))?;

    // Write events
    let base_ts = source
        .frames
        .first()
        .map(|f| f.header.timestamp_ms)
        .unwrap_or(0);
    let mut event_count = 0usize;

    for frame in &source.frames {
        let rel_secs = (frame.header.timestamp_ms.saturating_sub(base_ts)) as f64 / 1000.0;

        match frame.header.frame_type {
            FrameType::Output => {
                let text = String::from_utf8_lossy(&frame.payload);
                let event = serde_json::json!([rel_secs, "o", text]);
                let line = serde_json::to_string(&event).map_err(|e| {
                    crate::Error::Runtime(format!("Failed to serialize cast event: {e}"))
                })?;
                writeln!(writer, "{line}").map_err(|e| {
                    crate::Error::Runtime(format!("Failed to write cast event: {e}"))
                })?;
                event_count += 1;
            }
            FrameType::Resize => {
                if frame.payload.len() >= 4 {
                    let c = u16::from_le_bytes([frame.payload[0], frame.payload[1]]);
                    let r = u16::from_le_bytes([frame.payload[2], frame.payload[3]]);
                    let event = serde_json::json!([rel_secs, "r", format!("{c}x{r}")]);
                    let line = serde_json::to_string(&event).map_err(|e| {
                        crate::Error::Runtime(format!("Failed to serialize cast event: {e}"))
                    })?;
                    writeln!(writer, "{line}").map_err(|e| {
                        crate::Error::Runtime(format!("Failed to write cast event: {e}"))
                    })?;
                    event_count += 1;
                }
            }
            FrameType::Marker => {
                // Emit markers as comments (non-standard but harmless)
                let text = String::from_utf8_lossy(&frame.payload);
                let event = serde_json::json!([rel_secs, "m", text]);
                let line = serde_json::to_string(&event).map_err(|e| {
                    crate::Error::Runtime(format!("Failed to serialize cast event: {e}"))
                })?;
                writeln!(writer, "{line}").map_err(|e| {
                    crate::Error::Runtime(format!("Failed to write cast event: {e}"))
                })?;
                event_count += 1;
            }
            _ => {} // Skip Input and Event frames in cast export
        }
    }

    Ok(event_count)
}

/// Export a recording as a self-contained HTML page with an embedded player.
///
/// The HTML includes:
/// - Inline asciinema-player CSS/JS (loaded from CDN link)
/// - The cast data embedded as a `<script>` block
/// - Speed control, pause/seek/play
pub fn export_html<W: std::io::Write>(
    recording: &Recording,
    opts: &ExportOptions,
    writer: &mut W,
) -> Result<usize> {
    // First generate the cast data into a buffer
    let mut cast_buf = Vec::new();
    let event_count = export_asciinema(recording, opts, &mut cast_buf)?;
    let cast_data = String::from_utf8_lossy(&cast_buf);

    let title = opts.title.as_deref().unwrap_or("wa Session Recording");
    let (cols, rows) = find_terminal_size(
        &if opts.redact {
            redact_recording(recording, &opts.extra_redact_patterns)?
        } else {
            recording.clone()
        },
        opts.cols,
        opts.rows,
    );

    // Emit self-contained HTML
    write!(
        writer,
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{title}</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/asciinema-player@3.8.2/dist/bundle/asciinema-player.css">
<style>
  body {{
    margin: 0;
    padding: 20px;
    background: #1a1a2e;
    color: #e0e0e0;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    display: flex;
    flex-direction: column;
    align-items: center;
    min-height: 100vh;
  }}
  h1 {{
    font-size: 1.4em;
    margin-bottom: 16px;
    color: #e0e0e0;
  }}
  .info {{
    font-size: 0.85em;
    color: #888;
    margin-bottom: 12px;
  }}
  #player-container {{
    max-width: 100%;
    overflow-x: auto;
  }}
  .footer {{
    margin-top: 16px;
    font-size: 0.75em;
    color: #666;
  }}
</style>
</head>
<body>
<h1>{title}</h1>
<div class="info">{cols}x{rows} &middot; {event_count} events</div>
<div id="player-container"></div>
<div class="footer">Exported by wa (WezTerm Automata)</div>

<script id="cast-data" type="application/x-asciicast">{cast_data}</script>

<script src="https://cdn.jsdelivr.net/npm/asciinema-player@3.8.2/dist/bundle/asciinema-player.min.js"></script>
<script>
(function() {{
  var castText = document.getElementById('cast-data').textContent;
  var blob = new Blob([castText], {{ type: 'text/plain' }});
  var url = URL.createObjectURL(blob);
  AsciinemaPlayer.create(url, document.getElementById('player-container'), {{
    cols: {cols},
    rows: {rows},
    autoPlay: false,
    speed: 1,
    idleTimeLimit: 2,
    theme: 'monokai',
    fit: 'width'
  }});
}})();
</script>
</body>
</html>
"#,
        title = html_escape(title),
        cols = cols,
        rows = rows,
        event_count = event_count,
        cast_data = html_escape(&cast_data),
    )
    .map_err(|e| crate::Error::Runtime(format!("Failed to write HTML: {e}")))?;

    Ok(event_count)
}

/// Find terminal size from resize frames, falling back to defaults.
fn find_terminal_size(recording: &Recording, default_cols: u16, default_rows: u16) -> (u16, u16) {
    for frame in &recording.frames {
        if frame.header.frame_type == FrameType::Resize && frame.payload.len() >= 4 {
            let cols = u16::from_le_bytes([frame.payload[0], frame.payload[1]]);
            let rows = u16::from_le_bytes([frame.payload[2], frame.payload[3]]);
            if cols > 0 && rows > 0 {
                return (cols, rows);
            }
        }
    }
    (default_cols, default_rows)
}

/// Summary information about a recording.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingInfo {
    pub duration_ms: u64,
    pub frame_count: usize,
    pub output_frames: usize,
    pub event_frames: usize,
    pub resize_frames: usize,
    pub marker_frames: usize,
    pub input_frames: usize,
    pub total_bytes: usize,
    pub terminal_cols: u16,
    pub terminal_rows: u16,
}

impl Recording {
    /// Compute summary information about this recording.
    #[must_use]
    pub fn info(&self) -> RecordingInfo {
        let mut output_frames = 0usize;
        let mut event_frames = 0usize;
        let mut resize_frames = 0usize;
        let mut marker_frames = 0usize;
        let mut input_frames = 0usize;
        let mut total_bytes = 0usize;

        for frame in &self.frames {
            total_bytes += frame.payload.len();
            match frame.header.frame_type {
                FrameType::Output => output_frames += 1,
                FrameType::Resize => resize_frames += 1,
                FrameType::Event => event_frames += 1,
                FrameType::Marker => marker_frames += 1,
                FrameType::Input => input_frames += 1,
            }
        }

        let (cols, rows) = find_terminal_size(self, 80, 24);

        RecordingInfo {
            duration_ms: self.duration_ms,
            frame_count: self.frames.len(),
            output_frames,
            event_frames,
            resize_frames,
            marker_frames,
            input_frames,
            total_bytes,
            terminal_cols: cols,
            terminal_rows: rows,
        }
    }
}

/// Parse a duration string like "1m30s", "90s", "2m", or raw milliseconds "5000".
///
/// Returns duration in milliseconds.
pub fn parse_duration_ms(s: &str) -> Result<u64> {
    let s = s.trim();

    // Try raw integer (milliseconds)
    if let Ok(ms) = s.parse::<u64>() {
        return Ok(ms);
    }

    let mut total_ms: u64 = 0;
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num_buf.push(ch);
        } else {
            let val: f64 = num_buf.parse().map_err(|_| {
                crate::Error::Runtime(format!("Invalid duration component: '{num_buf}'"))
            })?;
            num_buf.clear();
            match ch {
                'h' => total_ms += (val * 3_600_000.0) as u64,
                'm' => total_ms += (val * 60_000.0) as u64,
                's' => total_ms += (val * 1_000.0) as u64,
                _ => {
                    return Err(crate::Error::Runtime(format!(
                        "Unknown duration unit '{ch}' in '{s}'"
                    )));
                }
            }
        }
    }

    // Trailing number without unit → treat as seconds
    if !num_buf.is_empty() {
        let val: f64 = num_buf
            .parse()
            .map_err(|_| crate::Error::Runtime(format!("Invalid duration: '{s}'")))?;
        total_ms += (val * 1_000.0) as u64;
    }

    Ok(total_ms)
}

/// Minimal HTML escaping for embedding in HTML attributes and text content.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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
    fn build_recording(specs: &[(u64, FrameType, Vec<u8>)]) -> Vec<u8> {
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
        assert!(matches!(
            decoded,
            DecodedFrame::Resize { cols: 80, rows: 24 }
        ));
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
        assert!(!sink.output.is_empty());
    }

    #[test]
    fn seek_beyond_end() {
        let data = build_recording(&[(0, FrameType::Output, b"only".to_vec())]);
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
            tokio::time::sleep(Duration::from_secs(1)).await;
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
            (
                100,
                FrameType::Event,
                serde_json::to_vec(&json!({"id": 1})).unwrap(),
            ),
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
        assert!(matches!(
            d1,
            DecodedFrame::Resize {
                cols: 120,
                rows: 40
            }
        ));

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

    // =========================================================================
    // Export Tests
    // =========================================================================

    fn build_test_recording_with_resize() -> Recording {
        let data = build_recording(&[
            (0, FrameType::Resize, {
                let mut p = Vec::new();
                p.extend_from_slice(&120u16.to_le_bytes());
                p.extend_from_slice(&40u16.to_le_bytes());
                p
            }),
            (100, FrameType::Output, b"$ hello world\r\n".to_vec()),
            (200, FrameType::Output, b"output line 2\r\n".to_vec()),
            (300, FrameType::Marker, b"checkpoint-1".to_vec()),
            (500, FrameType::Output, b"done\r\n".to_vec()),
        ]);
        Recording::from_bytes(&data).unwrap()
    }

    #[test]
    fn export_asciinema_header_and_events() {
        let rec = build_test_recording_with_resize();
        let opts = ExportOptions {
            redact: false,
            ..Default::default()
        };
        let mut buf = Vec::new();
        let count = export_asciinema(&rec, &opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();

        // Header line
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 120);
        assert_eq!(header["height"], 40);

        // Should have: 1 resize + 3 output + 1 marker = 5 events
        assert_eq!(count, 5);
        assert_eq!(lines.len(), 6); // header + 5 events

        // First event is a resize
        let ev1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ev1[1], "r");
        assert_eq!(ev1[2], "120x40");

        // Second event is output
        let ev2: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(ev2[1], "o");
        assert!(ev2[2].as_str().unwrap().contains("hello world"));

        // Marker event
        let ev4: serde_json::Value = serde_json::from_str(lines[4]).unwrap();
        assert_eq!(ev4[1], "m");
        assert!(ev4[2].as_str().unwrap().contains("checkpoint"));
    }

    #[test]
    fn export_asciinema_with_title() {
        let rec = build_test_recording_with_resize();
        let opts = ExportOptions {
            title: Some("Test Session".to_string()),
            redact: false,
            ..Default::default()
        };
        let mut buf = Vec::new();
        export_asciinema(&rec, &opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let header: serde_json::Value =
            serde_json::from_str(output.lines().next().unwrap()).unwrap();
        assert_eq!(header["title"], "Test Session");
    }

    #[test]
    fn export_asciinema_timing_relative() {
        let data = build_recording(&[
            (1000, FrameType::Output, b"first".to_vec()),
            (2500, FrameType::Output, b"second".to_vec()),
        ]);
        let rec = Recording::from_bytes(&data).unwrap();
        let opts = ExportOptions {
            redact: false,
            ..Default::default()
        };
        let mut buf = Vec::new();
        export_asciinema(&rec, &opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();

        let ev1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        let ev2: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        // First event at 0.0s (relative), second at 1.5s
        assert!((ev1[0].as_f64().unwrap() - 0.0).abs() < 0.001);
        assert!((ev2[0].as_f64().unwrap() - 1.5).abs() < 0.001);
    }

    #[test]
    fn export_asciinema_redaction() {
        let data = build_recording(&[(
            0,
            FrameType::Output,
            b"key=sk-proj-abcdefghijklmnop1234567890 done".to_vec(),
        )]);
        let rec = Recording::from_bytes(&data).unwrap();
        let opts = ExportOptions {
            redact: true,
            ..Default::default()
        };
        let mut buf = Vec::new();
        export_asciinema(&rec, &opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("sk-proj-abcdefghijklmnop1234567890"));
    }

    #[test]
    fn export_asciinema_extra_redact_patterns() {
        let data = build_recording(&[(0, FrameType::Output, b"token=MYSECRET123 ok".to_vec())]);
        let rec = Recording::from_bytes(&data).unwrap();
        let opts = ExportOptions {
            redact: true,
            extra_redact_patterns: vec!["MYSECRET\\d+".to_string()],
            ..Default::default()
        };
        let mut buf = Vec::new();
        export_asciinema(&rec, &opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("MYSECRET123"));
    }

    #[test]
    fn export_html_self_contained() {
        let rec = build_test_recording_with_resize();
        let opts = ExportOptions {
            title: Some("Demo".to_string()),
            redact: false,
            ..Default::default()
        };
        let mut buf = Vec::new();
        let count = export_html(&rec, &opts, &mut buf).unwrap();
        let html = String::from_utf8(buf).unwrap();

        assert!(count > 0);
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("asciinema-player"));
        assert!(html.contains("Demo"));
        assert!(html.contains("120x40"));
        // Cast data should be embedded
        assert!(html.contains("cast-data"));
        assert!(html.contains("hello world"));
    }

    #[test]
    fn export_html_redacts_secrets() {
        let data = build_recording(&[(
            0,
            FrameType::Output,
            b"ANTHROPIC_API_KEY=sk-ant-test-abc123def456 ok".to_vec(),
        )]);
        let rec = Recording::from_bytes(&data).unwrap();
        let opts = ExportOptions {
            redact: true,
            ..Default::default()
        };
        let mut buf = Vec::new();
        export_html(&rec, &opts, &mut buf).unwrap();
        let html = String::from_utf8(buf).unwrap();
        assert!(!html.contains("sk-ant-test-abc123def456"));
        assert!(html.contains("[REDACTED]"));
    }

    #[test]
    fn export_empty_recording() {
        let rec = Recording::from_bytes(&[]).unwrap();
        let opts = ExportOptions::default();

        let mut cast_buf = Vec::new();
        let count = export_asciinema(&rec, &opts, &mut cast_buf).unwrap();
        assert_eq!(count, 0);
        let output = String::from_utf8(cast_buf).unwrap();
        // Should still have a header
        assert!(output.lines().count() >= 1);

        let mut html_buf = Vec::new();
        let html_count = export_html(&rec, &opts, &mut html_buf).unwrap();
        assert_eq!(html_count, 0);
    }

    #[test]
    fn export_no_redact_preserves_secrets() {
        let data = build_recording(&[(
            0,
            FrameType::Output,
            b"sk-proj-abcdefghijklmnop1234567890".to_vec(),
        )]);
        let rec = Recording::from_bytes(&data).unwrap();
        let opts = ExportOptions {
            redact: false,
            ..Default::default()
        };
        let mut buf = Vec::new();
        export_asciinema(&rec, &opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("sk-proj-abcdefghijklmnop1234567890"));
    }

    #[test]
    fn find_terminal_size_from_resize_frame() {
        let rec = build_test_recording_with_resize();
        let (cols, rows) = find_terminal_size(&rec, 80, 24);
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }

    #[test]
    fn find_terminal_size_uses_defaults() {
        let data = build_recording(&[(0, FrameType::Output, b"hi".to_vec())]);
        let rec = Recording::from_bytes(&data).unwrap();
        let (cols, rows) = find_terminal_size(&rec, 80, 24);
        assert_eq!(cols, 80);
        assert_eq!(rows, 24);
    }

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("\"hello\""), "&quot;hello&quot;");
    }

    #[test]
    fn parse_duration_ms_raw_ms() {
        assert_eq!(parse_duration_ms("5000").unwrap(), 5000);
        assert_eq!(parse_duration_ms("0").unwrap(), 0);
    }

    #[test]
    fn parse_duration_ms_seconds() {
        assert_eq!(parse_duration_ms("90s").unwrap(), 90_000);
        assert_eq!(parse_duration_ms("1.5s").unwrap(), 1_500);
    }

    #[test]
    fn parse_duration_ms_minutes() {
        assert_eq!(parse_duration_ms("2m").unwrap(), 120_000);
        assert_eq!(parse_duration_ms("1m30s").unwrap(), 90_000);
    }

    #[test]
    fn parse_duration_ms_hours() {
        assert_eq!(parse_duration_ms("1h").unwrap(), 3_600_000);
        assert_eq!(parse_duration_ms("1h30m").unwrap(), 5_400_000);
    }

    #[test]
    fn parse_duration_ms_trailing_bare_number_as_seconds() {
        // "90" without unit is raw ms (parsed first), but "1m30" treats 30 as seconds
        assert_eq!(parse_duration_ms("1m30").unwrap(), 90_000);
    }

    // -----------------------------------------------------------------------
    // wa-z0e.6: Recording tests — format, roundtrip, playback, fuzz
    // -----------------------------------------------------------------------

    #[test]
    fn recording_info_counts_frame_types() {
        let data = build_recording(&[
            (0, FrameType::Output, b"a".to_vec()),
            (10, FrameType::Output, b"b".to_vec()),
            (20, FrameType::Event, b"{}".to_vec()),
            (30, FrameType::Marker, b"m".to_vec()),
            (40, FrameType::Input, b"x".to_vec()),
            (50, FrameType::Resize, {
                let mut p = Vec::new();
                p.extend_from_slice(&80u16.to_le_bytes());
                p.extend_from_slice(&24u16.to_le_bytes());
                p
            }),
        ]);
        let rec = Recording::from_bytes(&data).unwrap();
        let info = rec.info();

        assert_eq!(info.frame_count, 6);
        assert_eq!(info.output_frames, 2);
        assert_eq!(info.event_frames, 1);
        assert_eq!(info.marker_frames, 1);
        assert_eq!(info.input_frames, 1);
        assert_eq!(info.resize_frames, 1);
        assert_eq!(info.duration_ms, 50);
        assert_eq!(info.terminal_cols, 80);
        assert_eq!(info.terminal_rows, 24);
    }

    #[test]
    fn recording_info_empty() {
        let rec = Recording::from_bytes(&[]).unwrap();
        let info = rec.info();

        assert_eq!(info.frame_count, 0);
        assert_eq!(info.duration_ms, 0);
        assert_eq!(info.total_bytes, 0);
        // Defaults when no resize frame present
        assert_eq!(info.terminal_cols, 80);
        assert_eq!(info.terminal_rows, 24);
    }

    #[test]
    fn recording_info_total_bytes() {
        let data = build_recording(&[
            (0, FrameType::Output, b"hello".to_vec()),   // 5 bytes
            (10, FrameType::Output, b"world!".to_vec()), // 6 bytes
        ]);
        let rec = Recording::from_bytes(&data).unwrap();
        assert_eq!(rec.info().total_bytes, 11);
    }

    #[test]
    fn recording_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.war");
        let data = build_recording(&[
            (0, FrameType::Output, b"file test".to_vec()),
            (100, FrameType::Marker, b"end".to_vec()),
        ]);
        std::fs::write(&path, &data).unwrap();

        let rec = Recording::load(&path).unwrap();
        assert_eq!(rec.frames.len(), 2);
        assert_eq!(rec.duration_ms, 100);
    }

    #[test]
    fn recording_load_nonexistent_file_errors() {
        let result = Recording::load(std::path::Path::new("/tmp/nonexistent_wa_test.war"));
        assert!(result.is_err());
    }

    #[test]
    fn fuzz_invalid_frame_type_byte() {
        // Valid header length but invalid frame type byte (0)
        let mut data = vec![0u8; 14];
        data[8] = 0; // invalid frame type
        let result = Recording::from_bytes(&data);
        assert!(result.is_err());
    }

    #[test]
    fn fuzz_frame_type_byte_255() {
        let mut data = vec![0u8; 14];
        data[8] = 255; // invalid frame type
        let result = Recording::from_bytes(&data);
        assert!(result.is_err());
    }

    #[test]
    fn fuzz_valid_header_huge_payload_len() {
        // Header says payload is u32::MAX bytes but only 14 bytes of header present
        let mut data = vec![0u8; 14];
        data[8] = FrameType::Output as u8;
        data[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        let result = Recording::from_bytes(&data);
        assert!(result.is_err());
    }

    #[test]
    fn fuzz_single_byte_input() {
        // Should not panic
        let result = Recording::from_bytes(&[0x42]);
        assert!(result.is_err());
    }

    #[test]
    fn fuzz_all_zeros() {
        // 14 zero bytes = valid header size but frame_type 0 is invalid
        let result = Recording::from_bytes(&[0u8; 14]);
        assert!(result.is_err());
    }

    #[test]
    fn fuzz_random_noise_does_not_panic() {
        // Various garbage inputs that must not panic
        let inputs: Vec<&[u8]> = vec![
            &[],
            &[0xFF],
            &[0xFF; 13],
            &[0xFF; 14],
            &[0xFF; 100],
            b"NOT_A_RECORDING_FORMAT",
        ];
        for input in inputs {
            let _ = Recording::from_bytes(input);
        }
    }

    #[test]
    fn fuzz_partial_second_frame() {
        // First frame valid, second frame truncated
        let mut data = build_recording(&[(0, FrameType::Output, b"ok".to_vec())]);
        // Append partial header for second frame
        data.extend_from_slice(&[0u8; 10]);
        let result = Recording::from_bytes(&data);
        assert!(result.is_err());
    }

    #[test]
    fn decode_input_frame() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Input,
                flags: 0,
                payload_len: 3,
            },
            payload: b"ls\n".to_vec(),
        };
        let decoded = decode_frame(&frame).unwrap();
        assert!(matches!(decoded, DecodedFrame::Input(ref b) if b == b"ls\n"));
    }

    #[test]
    fn decode_resize_too_short_payload() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Resize,
                flags: 0,
                payload_len: 2, // Too short — need 4 bytes for cols+rows
            },
            payload: vec![0, 0],
        };
        let result = decode_frame(&frame);
        assert!(result.is_err());
    }

    #[test]
    fn decode_event_invalid_json() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Event,
                flags: 0,
                payload_len: 3,
            },
            payload: b"???".to_vec(),
        };
        let result = decode_frame(&frame);
        assert!(result.is_err());
    }

    #[test]
    fn seek_to_exact_frame_boundary() {
        let specs: Vec<_> = (0..5)
            .map(|i| (i * 100, FrameType::Output, format!("f{i}").into_bytes()))
            .collect();
        let data = build_recording(&specs);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = CollectorSink::new();

        // Seek to exactly timestamp 200 → should land after frame at 200
        player.seek_to(200, &mut sink).unwrap();
        assert_eq!(player.position().frame_index, 3); // frames 0,1,2 replayed
    }

    #[test]
    fn seek_in_recording_with_mixed_frame_types() {
        let event_payload = serde_json::to_vec(&json!({"x": 1})).unwrap();
        let data = build_recording(&[
            (0, FrameType::Output, b"start".to_vec()),
            (50, FrameType::Event, event_payload),
            (100, FrameType::Marker, b"mid".to_vec()),
            (150, FrameType::Output, b"end".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = CollectorSink::new();

        player.seek_to(120, &mut sink).unwrap();
        // Should skip past frames 0,1,2 (timestamps 0,50,100)
        assert_eq!(player.position().frame_index, 3);
        // seek_to only replays Output frames silently — events/markers are skipped
        assert_eq!(sink.output, b"start");
        assert_eq!(sink.events.len(), 0);
        assert!(sink.markers.is_empty());
    }

    #[test]
    fn player_initial_state() {
        let data = build_recording(&[(0, FrameType::Output, b"x".to_vec())]);
        let recording = Recording::from_bytes(&data).unwrap();
        let player = Player::new(recording);

        assert_eq!(player.state(), PlayerState::Stopped);
        assert_eq!(player.position().frame_index, 0);
        assert_eq!(player.position().timestamp_ms, 0);
        assert_eq!(player.total_frames(), 1);
        assert_eq!(player.duration_ms(), 0);
    }

    #[tokio::test]
    async fn play_empty_recording_finishes_immediately() {
        let recording = Recording::from_bytes(&[]).unwrap();
        let mut player = Player::new(recording);
        let mut sink = HeadlessSink;

        player.play_simple(&mut sink).await.unwrap();
        assert_eq!(player.state(), PlayerState::Finished);
    }

    #[test]
    fn recording_large_payload() {
        let large = vec![b'Z'; 100_000];
        let data = build_recording(&[(0, FrameType::Output, large.clone())]);
        let rec = Recording::from_bytes(&data).unwrap();
        assert_eq!(rec.frames.len(), 1);
        assert_eq!(rec.frames[0].payload, large);
    }

    #[tokio::test]
    async fn play_with_pause_then_resume() {
        tokio::time::pause();

        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (5000, FrameType::Output, b"B".to_vec()),
            (10000, FrameType::Output, b"C".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);

        let (tx, rx) = watch::channel(PlayerControl::Play);
        let mut sink = CollectorSink::new();

        let tx2 = tx.clone();
        tokio::spawn(async move {
            // Pause at 1s, resume at 2s
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = tx2.send(PlayerControl::Pause);
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = tx2.send(PlayerControl::Play);
        });

        player.play(&mut sink, rx).await.unwrap();
        assert_eq!(player.state(), PlayerState::Finished);
        assert_eq!(sink.output, b"ABC");
        drop(tx);
    }

    #[test]
    fn recording_preserves_frame_ordering() {
        // Non-monotonic timestamps should still be preserved
        let data = build_recording(&[
            (100, FrameType::Output, b"late".to_vec()),
            (50, FrameType::Output, b"early".to_vec()),
            (200, FrameType::Output, b"last".to_vec()),
        ]);
        let rec = Recording::from_bytes(&data).unwrap();
        assert_eq!(rec.frames.len(), 3);
        assert_eq!(rec.frames[0].header.timestamp_ms, 100);
        assert_eq!(rec.frames[1].header.timestamp_ms, 50);
        assert_eq!(rec.frames[2].header.timestamp_ms, 200);
    }

    #[test]
    fn recording_flags_preserved() {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Output,
                flags: 0b0000_0001,
                payload_len: 3,
            },
            payload: b"gap".to_vec(),
        };
        let data = frame.encode();
        let rec = Recording::from_bytes(&data).unwrap();
        assert_eq!(rec.frames[0].header.flags, 1);
    }

    #[test]
    fn export_asciinema_skips_input_and_event_frames() {
        let data = build_recording(&[
            (0, FrameType::Output, b"$ ".to_vec()),
            (100, FrameType::Input, b"ls\n".to_vec()),
            (200, FrameType::Event, b"{}".to_vec()),
        ]);
        let rec = Recording::from_bytes(&data).unwrap();
        let opts = ExportOptions {
            redact: false,
            ..Default::default()
        };
        let mut buf = Vec::new();
        let count = export_asciinema(&rec, &opts, &mut buf).unwrap();
        // Only Output frames are exported; Input and Event are skipped
        assert_eq!(count, 1);
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output.lines().count(), 2); // header + 1 output event
    }

    #[tokio::test]
    async fn play_simple_records_correct_final_position() {
        let data = build_recording(&[
            (0, FrameType::Output, b"a".to_vec()),
            (100, FrameType::Output, b"b".to_vec()),
            (500, FrameType::Output, b"c".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::QUAD);
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(player.position().frame_index, 3);
        assert_eq!(sink.output, b"abc");
    }
}
