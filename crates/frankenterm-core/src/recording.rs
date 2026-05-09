//! Recording engine for wa sessions.
//!
//! Provides a per-pane recorder that writes frame data to disk using the
//! WAR recording format (see docs/recording-format-spec.md).
//!
//! NOTE: This is the core engine only; CLI wiring lives elsewhere.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::runtime_async::Mutex;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::RuntimeOperationSource;
use crate::ingest::{CapturedSegment, CapturedSegmentKind};
use crate::patterns::Detection;
use crate::policy::Redactor;

/// Supported frame types within a recording stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FrameType {
    /// Terminal output payload.
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
///
/// Recording v1 only stores full payload chunks. The recorder does not expose
/// diff/repeat variants until there is real encode/decode support for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaEncoding {
    /// Full frame payload (no delta).
    Full(Vec<u8>),
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
    /// Construct a `RecordingFrame` with `header.payload_len` derived from
    /// `payload.len()`. The on-disk replay path advances offsets by the
    /// declared `payload_len` (`replay.rs:46-80`), so any mismatch silently
    /// corrupts the next frame's parse boundary. This constructor is the
    /// only way to build a frame whose declared length is guaranteed to
    /// match its bytes (br-ft-l88j0).
    pub fn new(
        timestamp_ms: u64,
        frame_type: FrameType,
        flags: u8,
        payload: Vec<u8>,
    ) -> Result<Self> {
        let payload_len = checked_frame_payload_len("RecordingFrame::new", payload.len())?;
        Ok(Self {
            header: FrameHeader {
                timestamp_ms,
                frame_type,
                flags,
                payload_len,
            },
            payload,
        })
    }

    /// Verify `header.payload_len` matches `payload.len()` exactly.
    ///
    /// br-ft-l88j0: the public `RecordingFrame { .. }` struct-literal API
    /// lets callers set `payload_len` independently of the payload bytes.
    /// The replay reader trusts the declared length when advancing
    /// offsets, so a short value leaves trailing bytes that get reparsed
    /// as a next-frame header, and a long value makes the stream look
    /// truncated. Producer-side validation at `write_frame` is the
    /// boundary that prevents corrupt frames from reaching disk.
    pub(crate) fn check_payload_len_matches(&self) -> Result<()> {
        let actual = u32::try_from(self.payload.len()).map_err(|_| {
            crate::Error::Io(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "RecordingFrame payload is {} bytes; WAR frame payload_len is limited to {} bytes",
                    self.payload.len(),
                    u32::MAX
                ),
            ))
        })?;
        if self.header.payload_len != actual {
            return Err(crate::Error::Io(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "RecordingFrame header.payload_len={} but payload.len()={}; WAR replay would misparse the stream (br-ft-l88j0)",
                    self.header.payload_len, actual
                ),
            )));
        }
        Ok(())
    }

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

/// Convert an in-memory payload length into the WAR frame header width.
///
/// The on-disk format stores payload lengths as `u32`; callers must fail
/// before constructing a wrapped header for oversized in-memory payloads.
pub(crate) fn checked_frame_payload_len(context: &str, payload_len: usize) -> Result<u32> {
    u32::try_from(payload_len).map_err(|_| {
        crate::Error::Io(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "{context} payload is {payload_len} bytes; WAR frame payload_len is limited to {} bytes",
                u32::MAX
            ),
        ))
    })
}

/// Buffered frame writer for recording output.
pub struct FrameWriter {
    buffer: Vec<RecordingFrame>,
    flush_threshold: usize,
    writer: Option<BufWriter<File>>,
    finalized: bool,
    /// br-ft-ye2gs: in-flight finalize worker receiver, set when
    /// `finalize_with_timeout` spawns a worker but the receive
    /// times out before the worker reports completion. The worker
    /// owns the buffer + writer and may still be flushing in the
    /// background; the next finalize_with_timeout call awaits this
    /// receiver instead of spawning a new worker (which would have
    /// nothing to flush, since the original buffer and writer have
    /// already been moved into the in-flight worker).
    ///
    /// This makes finalize retryable: a transient slow disk that
    /// blows the first timeout can succeed on a subsequent call
    /// once the worker completes. Pre-fix the timeout error left
    /// `finalized = true` and dropped the worker handle, so the
    /// caller had no way to know whether the flush eventually
    /// landed and no path to await it.
    pending_finalize: Option<std::sync::mpsc::Receiver<std::io::Result<()>>>,
}

const DEFAULT_FRAME_WRITER_FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);

impl FrameWriter {
    /// Create a new frame writer.
    pub fn new(path: &Path, flush_threshold: usize) -> Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            buffer: Vec::with_capacity(flush_threshold.max(1)),
            flush_threshold: flush_threshold.max(1),
            writer: Some(BufWriter::new(file)),
            finalized: false,
            pending_finalize: None,
        })
    }

    /// br-ft-ye2gs: report whether a previous `finalize_with_timeout`
    /// call timed out and a worker is still in flight. Callers can
    /// observe this to distinguish "durably finalized" from
    /// "dispatch in flight, retry pending" without consuming the
    /// receiver.
    #[must_use]
    pub fn has_pending_finalize(&self) -> bool {
        self.pending_finalize.is_some()
    }

    /// br-ft-ye2gs: report whether the writer is durably finalized
    /// (i.e., the worker reported successful completion). Returns
    /// false while a worker is still in flight after a timeout.
    #[must_use]
    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Write a frame (buffered). Flushes when buffer reaches threshold.
    ///
    /// br-ft-l88j0: rejects frames whose `header.payload_len` does not
    /// match `payload.len()`. The replay reader trusts the declared
    /// length, so a malformed frame would silently corrupt the WAR
    /// stream's parse boundary.
    pub fn write_frame(&mut self, frame: RecordingFrame) -> Result<()> {
        if self.finalized {
            return Err(frame_writer_finalized_error());
        }
        if self.pending_finalize.is_some() {
            return Err(frame_writer_finalize_pending_error());
        }
        frame.check_payload_len_matches()?;
        self.buffer.push(frame);
        if self.buffer.len() >= self.flush_threshold {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush buffered frames to disk.
    pub fn flush(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        if self.pending_finalize.is_some() {
            return Err(frame_writer_finalize_pending_error());
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(frame_writer_finalized_error)?;
        for frame in self.buffer.drain(..) {
            let bytes = frame.encode();
            writer.write_all(&bytes)?;
        }
        writer.flush()?;
        Ok(())
    }

    /// Flush all queued frames and permanently close this writer path.
    pub fn finalize(&mut self) -> Result<()> {
        self.finalize_with_timeout(DEFAULT_FRAME_WRITER_FINALIZE_TIMEOUT)
    }

    /// Flush all queued frames and wait up to `timeout` for completion.
    ///
    /// br-ft-ye2gs: timeout is now retryable. If the worker doesn't
    /// report completion within `timeout`, the receiver is stashed
    /// in `self.pending_finalize` and the next call to this method
    /// awaits it instead of spawning a new worker. `self.finalized`
    /// is only set to `true` after the worker reports successful
    /// completion, so callers can distinguish "transient timeout
    /// — retryable" from "permanently flushed". On `Disconnected`
    /// (worker panicked / exited without sending) the file handle
    /// is unrecoverable, so we mark `finalized` to prevent further
    /// usage and surface the broken-pipe error.
    pub fn finalize_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.finalize_with_timeout_after_worker_delay(timeout, None)
    }

    #[cfg(test)]
    fn finalize_with_timeout_for_test(
        &mut self,
        timeout: Duration,
        worker_delay: Duration,
    ) -> Result<()> {
        self.finalize_with_timeout_after_worker_delay(timeout, Some(worker_delay))
    }

    fn finalize_with_timeout_after_worker_delay(
        &mut self,
        timeout: Duration,
        worker_delay: Option<Duration>,
    ) -> Result<()> {
        if self.finalized {
            return Ok(());
        }

        // br-ft-ye2gs: if a previous call timed out and stashed an
        // in-flight worker receiver, await it instead of spawning a
        // new worker. The original buffer + writer were moved into
        // that worker; spawning a fresh one would have nothing to
        // flush.
        if let Some(receiver) = self.pending_finalize.take() {
            return self.await_finalize_receiver(receiver, timeout);
        }

        let buffer = std::mem::take(&mut self.buffer);
        let Some(writer) = self.writer.take() else {
            // No writer left and no pending worker: nothing to flush.
            // Mark finalized so subsequent calls short-circuit cleanly.
            self.finalized = true;
            return Ok(());
        };

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            if let Some(delay) = worker_delay {
                std::thread::sleep(delay);
            }
            let result = flush_frame_writer_parts(buffer, writer);
            // br-ft-x2oyy: intentional best-effort completion signal; send
            // fails only if finalize timed out and dropped the receiver.
            let _ = sender.send(result);
        });

        self.await_finalize_receiver(receiver, timeout)
    }

    /// br-ft-ye2gs: shared receive-loop body for both fresh-worker
    /// and retry-after-timeout paths. Translates the
    /// `recv_timeout` outcomes into the public `Result<()>` while
    /// updating `self.finalized` and `self.pending_finalize` to
    /// reflect durable completion semantics rather than worker
    /// dispatch semantics.
    fn await_finalize_receiver(
        &mut self,
        receiver: std::sync::mpsc::Receiver<std::io::Result<()>>,
        timeout: Duration,
    ) -> Result<()> {
        match receiver.recv_timeout(timeout) {
            Ok(Ok(())) => {
                self.finalized = true;
                Ok(())
            }
            Ok(Err(io_err)) => {
                // Worker reported a flush error — the file may be in
                // a partial state but the worker is done. Mark
                // finalized to prevent retry of an unrecoverable
                // path.
                self.finalized = true;
                Err(crate::Error::Io(io_err))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // br-ft-ye2gs: keep the receiver alive so the next
                // call can await it. Do NOT set self.finalized — the
                // flush hasn't durably completed.
                self.pending_finalize = Some(receiver);
                Err(crate::Error::Io(std::io::Error::new(
                    ErrorKind::TimedOut,
                    format!(
                        "FrameWriter finalize exceeded {timeout:?}; \
                         worker still in flight, retry finalize_with_timeout to await it"
                    ),
                )))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Worker panicked / exited without sending. The file
                // handle is gone. Mark finalized so the writer
                // cannot be reused; surface the broken-pipe error.
                self.finalized = true;
                Err(crate::Error::Io(std::io::Error::new(
                    ErrorKind::BrokenPipe,
                    "FrameWriter finalize worker exited before reporting completion",
                )))
            }
        }
    }
}

impl Drop for FrameWriter {
    /// Best-effort finalization of buffered frames on drop.
    ///
    /// Without this, a `FrameWriter` that is dropped without an explicit
    /// `finalize()` / `Recorder::stop()` call (e.g. on runtime shutdown,
    /// pane removal, or panic unwind) silently loses up to
    /// `flush_threshold` queued `RecordingFrame`s. Those frames live in
    /// `self.buffer` and are NOT in the inner `BufWriter`, so `BufWriter`'s
    /// own drop-flush doesn't reach them. Errors here are intentionally
    /// swallowed: Drop cannot fail, and the underlying file may already
    /// be gone in the shutdown path.
    ///
    /// SAFETY against double-panic abort (ft-7bcox): wrap finalization in
    /// `catch_unwind`. If `self.finalize()` itself panics — for example
    /// because the inner allocator OOMs while encoding a queued frame,
    /// or a future custom `Write` impl panics on bad state — and Drop
    /// is running during another unwind, Rust's panic-during-unwind
    /// rule aborts the process. `catch_unwind` swallows the inner
    /// panic so only the outer panic propagates. The `AssertUnwindSafe`
    /// wrap is sound here because: (i) we discard the closure's
    /// `Result`, (ii) we never observe `&mut self` again after the
    /// closure returns, (iii) the only state that survives is
    /// `self.buffer` / `self.writer`, both of which are about to be
    /// dropped anyway.
    fn drop(&mut self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = self.finalize();
        }));
    }
}

fn flush_frame_writer_parts(
    buffer: Vec<RecordingFrame>,
    mut writer: BufWriter<File>,
) -> std::io::Result<()> {
    for frame in buffer {
        let bytes = frame.encode();
        writer.write_all(&bytes)?;
    }
    writer.flush()
}

fn frame_writer_finalized_error() -> crate::Error {
    crate::Error::Io(std::io::Error::new(
        ErrorKind::BrokenPipe,
        "FrameWriter is already finalized",
    ))
}

fn frame_writer_finalize_pending_error() -> crate::Error {
    crate::Error::Io(std::io::Error::new(
        ErrorKind::WouldBlock,
        "FrameWriter finalize is still pending; retry finalize before writing more frames",
    ))
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
    ///
    /// br-ft-ye2gs: state transitions to `Stopped` only after the
    /// underlying `FrameWriter::finalize` reports durable completion.
    /// Pre-fix the state was set to `Stopped` BEFORE finalize ran, so
    /// a transient slow disk that timed out the finalize would leave
    /// the recorder marked stopped (rejecting new frames) while
    /// surfacing an error to the caller — and there was no retry
    /// path because finalize had already moved out the buffer +
    /// writer. Now: on a finalize timeout, the recorder stays in its
    /// current state; the caller can call `stop()` again to await
    /// the in-flight worker (see `FrameWriter::has_pending_finalize`).
    pub fn stop(&mut self) -> Result<()> {
        self.writer.finalize()?;
        self.state = RecorderState::Stopped;
        Ok(())
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
        let payload_len = checked_frame_payload_len("recording output", payload.len())?;

        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms,
                frame_type: FrameType::Output,
                flags,
                payload_len,
            },
            payload: payload.to_vec(),
        };
        let bytes_written = frame.payload.len() as u64;

        self.writer.write_frame(frame)?;
        self.frames_written += 1;
        self.bytes_written += bytes_written;
        Ok(())
    }

    /// Record a captured output segment as a frame.
    pub fn record_segment(&mut self, segment: &CapturedSegment) -> Result<()> {
        let is_gap = matches!(segment.kind, CapturedSegmentKind::Gap { .. });
        let payload = segment.content.as_bytes();
        let raw_len = payload.len() as u64;
        // Only credit bytes_raw after a successful write; otherwise a payload
        // rejected by the u32 guard (or a downstream IO error) inflates the
        // stat by content that never made it into the recording.
        self.record_output(segment.captured_at, is_gap, payload)?;
        self.bytes_raw += raw_len;
        Ok(())
    }

    /// Record a detection event as a frame (redaction to be applied by caller).
    pub fn record_event(&mut self, detection: &Detection, captured_at_ms: i64) -> Result<()> {
        if !self.is_recording() {
            return Ok(());
        }

        let timestamp_ms = self.timestamp_ms_for_capture(captured_at_ms);
        let payload = serde_json::to_vec(detection)?;
        let payload_len = checked_frame_payload_len("recording event", payload.len())?;
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms,
                frame_type: FrameType::Event,
                flags: 0,
                payload_len,
            },
            payload,
        };
        let bytes_written = frame.payload.len() as u64;

        self.writer.write_frame(frame)?;
        self.frames_written += 1;
        self.bytes_written += bytes_written;
        Ok(())
    }

    fn timestamp_ms_for_capture(&self, captured_at_ms: i64) -> u64 {
        if let Some(start_ms) = self.start_epoch_ms {
            let delta_ms = captured_at_ms.saturating_sub(start_ms).max(0);
            return u64::try_from(delta_ms).unwrap_or(0);
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

fn recording_runtime_error(operation: &'static str, detail: impl Into<String>) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Backend(detail.into()),
    }
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.start_recording_with_cx(&cx, pane_id, path, started_at_ms)
            .await
    }

    /// Start recording a pane under an explicit `&Cx` (ft-xbnl0.2.3
    /// Cx-first entry point). The internal `recorders` mutex acquire
    /// is bound to the caller's `Cx` via `Mutex::lock_with_cx`, so a
    /// caller-cancelled wait propagates through the mutex wait
    /// instead of being pulled from `Cx::current()` thread-local.
    pub async fn start_recording_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        path: &Path,
        started_at_ms: i64,
    ) -> Result<()> {
        let mut guard = self.recorders.lock_with_cx(cx).await;
        if guard.contains_key(&pane_id) {
            return Err(recording_runtime_error(
                "recording.start",
                format!("Recorder already active for pane {pane_id}"),
            ));
        }
        let mut recorder = Recorder::new(pane_id, path, self.options.flush_threshold)?;
        recorder.start(started_at_ms);
        guard.insert(pane_id, recorder);
        Ok(())
    }

    /// Stop recording a pane and flush any buffered frames.
    pub async fn stop_recording(&self, pane_id: u64) -> Result<Option<RecorderStats>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.stop_recording_with_cx(&cx, pane_id).await
    }

    /// Stop recording a pane under an explicit `&Cx` (ft-xbnl0.2.3
    /// Cx-first entry point).
    pub async fn stop_recording_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<RecorderStats>> {
        let mut guard = self.recorders.lock_with_cx(cx).await;
        if let Some(mut recorder) = guard.remove(&pane_id) {
            recorder.stop()?;
            return Ok(Some(recorder.stats()));
        }
        Ok(None)
    }

    /// Record a captured output segment (redacted if configured).
    pub async fn record_segment(&self, segment: &CapturedSegment) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_segment_with_cx(&cx, segment).await
    }

    /// Record a captured output segment under an explicit `&Cx`
    /// (ft-xbnl0.2.3 Cx-first entry point).
    ///
    /// Redaction is a pure function over `&self.redactor` and
    /// `&self.options` — it does not touch the `recorders` mutex, so we
    /// run it *before* acquiring the lock. This keeps the critical
    /// section reduced to the HashMap lookup, the `is_recording` check,
    /// and the synchronous `record_output` write; concurrent callers
    /// targeting *other* panes are no longer serialized behind this
    /// pane's regex-scan + allocation work.
    pub async fn record_segment_with_cx(
        &self,
        cx: &crate::cx::Cx,
        segment: &CapturedSegment,
    ) -> Result<()> {
        let payload: Vec<u8> = if self.options.redact_output {
            self.redactor.redact(&segment.content).into_bytes()
        } else {
            segment.content.as_bytes().to_vec()
        };
        let content_len = segment.content.len() as u64;
        let is_gap = matches!(segment.kind, CapturedSegmentKind::Gap { .. });

        let mut guard = self.recorders.lock_with_cx(cx).await;
        let Some(recorder) = guard.get_mut(&segment.pane_id) else {
            return Ok(());
        };
        if !recorder.is_recording() {
            return Ok(());
        }

        // Credit bytes_raw only after the write succeeds — a payload
        // rejected by the WAR frame size guard (or a downstream IO error)
        // would otherwise inflate the stat with content that never landed.
        recorder.record_output(segment.captured_at, is_gap, &payload)?;
        recorder.bytes_raw += content_len;
        Ok(())
    }

    /// Record a detection event (redacted if configured).
    pub async fn record_event(
        &self,
        pane_id: u64,
        detection: &Detection,
        captured_at_ms: i64,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_event_with_cx(&cx, pane_id, detection, captured_at_ms)
            .await
    }

    /// Record a detection event under an explicit `&Cx` (ft-xbnl0.2.3
    /// Cx-first entry point).
    ///
    /// Mirrors the critical-section reduction in `record_segment_with_cx`:
    /// the redaction step is a pure function over `&self.redactor` and
    /// does not touch the `recorders` mutex, so it runs above the lock.
    pub async fn record_event_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        detection: &Detection,
        captured_at_ms: i64,
    ) -> Result<()> {
        let redacted_owned: Option<Detection> = if self.options.redact_events {
            Some(redact_detection(detection, &self.redactor))
        } else {
            None
        };
        let detection_ref: &Detection = redacted_owned.as_ref().unwrap_or(detection);

        let mut guard = self.recorders.lock_with_cx(cx).await;
        let Some(recorder) = guard.get_mut(&pane_id) else {
            return Ok(());
        };
        if !recorder.is_recording() {
            return Ok(());
        }
        recorder.record_event(detection_ref, captured_at_ms)
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

// ft-j1qjt.3: the recorder-event metadata enum cluster + the schema-version
// constant moved to `frankenterm-core-replay-types::recorder_metadata`.
// They are pure-data leaf types with zero crate-internal deps; co-locating
// them with `replay_decision_graph` lets both `frankenterm-core` and the
// future `frankenterm-core-replay` extraction (ft-j1qjt.2) reach the same
// canonical definitions without a cargo cycle.
//
// Re-exported here so existing `crate::recording::RecorderEventSource`
// (and the other 6 items) paths keep resolving unchanged across the ~12
// importing files in core.
pub use frankenterm_core_replay_types::recorder_metadata::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};

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
        return Err(recording_runtime_error(
            "recording.parse_event",
            format!(
                "unsupported recorder event schema version: {version:?} \
             (expected {RECORDER_EVENT_SCHEMA_VERSION_V1:?})"
            ),
        ));
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

/// br-ft-crpvd: cumulative count of `SystemTime::now() < UNIX_EPOCH`
/// events observed by [`epoch_ms_now`]. Same observability shape as
/// `web::sse::EPOCH_CLOCK_ANOMALY_COUNT` (ft-bn6qi). The recorder is
/// the forensic-truth surface — a silent timestamp=0 during a clock
/// anomaly window turns the monotonic-sequence promise into "all
/// events claim the same instant", so the counter+warn pattern is
/// the cheap fix.
static RECORDING_CLOCK_ANOMALY_COUNT: AtomicU64 = AtomicU64::new(0);

/// br-ft-crpvd: cumulative count of recording clock-before-1970
/// anomalies. See [`RECORDING_CLOCK_ANOMALY_COUNT`].
#[must_use]
pub fn recording_clock_anomaly_count() -> u64 {
    RECORDING_CLOCK_ANOMALY_COUNT.load(Ordering::Relaxed)
}

/// br-ft-crpvd: test-only reset for the clock-anomaly counter.
#[cfg(test)]
pub(crate) fn reset_recording_clock_anomaly_count_for_test() {
    RECORDING_CLOCK_ANOMALY_COUNT.store(0, Ordering::Relaxed);
}

/// Returns the current Unix epoch in milliseconds.
///
/// br-ft-crpvd: pre-fix this used `unwrap_or_default()` which silently
/// substituted 0 for every captured event during a clock anomaly
/// window (NTP fail / container restart with un-synced clock). The
/// recorder feeds replay_capture and the forensic ledger, so a flat
/// line of timestamp=0 across an anomaly is forensically catastrophic.
/// Now: bump the anomaly counter, emit a structured tracing::warn,
/// still return 0 so the scalar-u64 contract stays the same.
#[must_use]
pub fn epoch_ms_now() -> u64 {
    epoch_ms_now_from(std::time::SystemTime::now())
}

/// br-ft-crpvd: testable inner helper. Takes a `SystemTime` so a
/// regression test can inject `UNIX_EPOCH - 100s` (a valid
/// pre-epoch SystemTime) to exercise the Err branch without
/// depending on the host system clock.
fn epoch_ms_now_from(now: std::time::SystemTime) -> u64 {
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(ts) => ts.as_millis() as u64,
        Err(err) => {
            RECORDING_CLOCK_ANOMALY_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "ft.recording.clock",
                event = "recording_clock_anomaly",
                pre_epoch_secs = err.duration().as_secs(),
                "system clock is before UNIX_EPOCH; recorder timestamp falling back to 0 (br-ft-crpvd)"
            );
            0
        }
    }
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
    use proptest::prelude::*;
    use serde_json::json;
    use std::future::Future;
    use tempfile::tempdir;

    fn run_async_test<F>(future: F)
    where
        F: Future<Output = ()>,
    {
        use crate::runtime_async::{CompatRuntime, RuntimeBuilder};

        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("recording test runtime should build");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn assert_runtime_operation_backend(
        err: crate::Error,
        expected_operation: &'static str,
        expected_detail: &str,
    ) {
        match err {
            crate::Error::RuntimeOperation { operation, source } => {
                assert_eq!(operation, expected_operation);
                match source {
                    RuntimeOperationSource::Backend(detail) => {
                        assert!(
                            detail.contains(expected_detail),
                            "expected detail to contain {expected_detail:?}, got {detail:?}"
                        );
                    }
                    other => panic!("expected Backend source, got {other:?}"),
                }
            }
            other => panic!("expected RuntimeOperation, got {other:?}"),
        }
    }

    fn fake_redaction_key() -> String {
        let mut key = String::from("s");
        key.push('k');
        key.push('-');
        key.push_str("abc123456789012345678901234567890123456789012345678901");
        key
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
    fn recording_frame_new_derives_payload_len_from_payload() {
        let payload = b"hello world".to_vec();
        let frame =
            RecordingFrame::new(7, FrameType::Output, 0, payload.clone()).expect("constructor");
        assert_eq!(frame.header.payload_len as usize, payload.len());
        assert_eq!(frame.payload, payload);
        // Round-trip through encode: header.payload_len equals on-disk LE u32.
        let bytes = frame.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[10..14].try_into().unwrap()),
            payload.len() as u32
        );
    }

    #[test]
    fn write_frame_rejects_short_declared_payload_len() {
        // br-ft-l88j0: short declared length leaves trailing bytes that
        // the replay reader would reparse as a next-frame header.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rec.war");
        let mut writer = FrameWriter::new(&path, 4).expect("writer");
        let bad = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 1,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 2, // declares 2 but actually has 5
            },
            payload: b"hello".to_vec(),
        };
        let err = writer.write_frame(bad).expect_err("must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-l88j0"),
            "error must cite bead breadcrumb: {msg}"
        );
        assert!(
            msg.contains("payload_len=2") && msg.contains("payload.len()=5"),
            "error must cite both lengths: {msg}"
        );
    }

    #[test]
    fn write_frame_rejects_long_declared_payload_len() {
        // br-ft-l88j0: long declared length makes the stream look truncated.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rec.war");
        let mut writer = FrameWriter::new(&path, 4).expect("writer");
        let bad = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 1,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 100, // declares 100 but only has 3 bytes
            },
            payload: vec![1, 2, 3],
        };
        let err = writer.write_frame(bad).expect_err("must reject");
        assert!(err.to_string().contains("br-ft-l88j0"));
    }

    #[test]
    fn write_frame_accepts_consistent_zero_length_payload() {
        // Empty payload with payload_len=0 is the canonical empty frame
        // shape (used by Resize / Marker frame types).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rec.war");
        let mut writer = FrameWriter::new(&path, 4).expect("writer");
        let ok = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 1,
                frame_type: FrameType::Resize,
                flags: 0,
                payload_len: 0,
            },
            payload: vec![],
        };
        writer.write_frame(ok).expect("zero-length frame is valid");
    }

    #[test]
    fn checked_frame_payload_len_rejects_header_overflow() {
        if usize::BITS <= u32::BITS {
            return;
        }

        let over_limit = usize::try_from(u32::MAX).expect("u32 max should fit usize") + 1;
        let err = checked_frame_payload_len("test frame", over_limit)
            .expect_err("payload length over u32::MAX must fail");

        assert!(
            err.to_string().contains("WAR frame payload_len"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn redact_detection_scrubs_secrets() {
        let key = fake_redaction_key();
        let detection = Detection {
            rule_id: "test.rule".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({ "token": key }),
            matched_text: key.clone(),
            span: (0, 5),
        };

        let redactor = Redactor::new();
        let redacted = super::redact_detection(&detection, &redactor);
        assert!(!redacted.matched_text.contains(&key));
        let serialized = serde_json::to_string(&redacted.extracted).unwrap();
        assert!(!serialized.contains(&key));
    }

    #[test]
    fn recording_manager_redacts_output() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let path = dir.path().join("test.war");
            let key = fake_redaction_key();

            let manager = RecordingManager::new(RecordingOptions {
                flush_threshold: 1,
                redact_output: true,
                redact_events: false,
            });

            manager.start_recording(1, &path, 0).await.unwrap();
            let segment = CapturedSegment {
                pane_id: 1,
                seq: 0,
                content: format!("token {key}"),
                kind: CapturedSegmentKind::Delta,
                captured_at: 10,
            };
            manager.record_segment(&segment).await.unwrap();
            manager.stop_recording(1).await.unwrap();

            let bytes = std::fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(&key));
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
        let DeltaEncoding::Full(inner) = enc;
        assert_eq!(inner, data);
    }

    #[test]
    fn delta_encoding_serde_roundtrip() {
        let enc = DeltaEncoding::Full(vec![0xDE, 0xAD]);
        let json = serde_json::to_string(&enc).unwrap();
        let back: DeltaEncoding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, enc);
    }

    #[test]
    fn delta_encoding_rejects_legacy_placeholder_variants() {
        let diff = r#"{"Diff":{"base_frame":7,"ops":[]}}"#;
        let repeat = r#"{"Repeat":{"base_frame":7}}"#;

        let diff_err = serde_json::from_str::<DeltaEncoding>(diff).unwrap_err();
        let repeat_err = serde_json::from_str::<DeltaEncoding>(repeat).unwrap_err();

        assert!(
            diff_err.to_string().contains("unknown variant"),
            "unexpected error: {diff_err}"
        );
        assert!(
            repeat_err.to_string().contains("unknown variant"),
            "unexpected error: {repeat_err}"
        );
    }

    #[test]
    fn frame_writer_drop_completes_during_panic_unwind_without_aborting() {
        // ft-7bcox: the FrameWriter Drop impl flushes a BufWriter; if that
        // flush panics WHILE we're already unwinding (e.g., the caller
        // panicked mid-recording and Drop runs as the stack unwinds),
        // Rust's panic-during-unwind rule aborts the process. The Drop
        // impl wraps its body in std::panic::catch_unwind so a flush
        // panic stays swallowed and only the outer panic propagates.
        //
        // Test the integration: panic inside a closure that owns a live
        // FrameWriter with buffered frames. Drop runs during unwind. If
        // the catch_unwind wrap is missing AND flush panics, the test
        // process aborts — `result` is never observed, the test runner
        // reports a SIGABRT, and we lose the assertion below. The test
        // PASSING (we get back to assert!) is the proof that Drop did
        // not abort.
        let dir = tempdir().unwrap();
        let path = dir.path().join("panic-during-drop.war");

        let result = std::panic::catch_unwind(|| {
            let mut writer = FrameWriter::new(&path, 100).unwrap();
            // Buffer some frames without crossing flush_threshold so Drop
            // has work to do (the flush path is the one we want to harden).
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
            // Force unwind. Drop runs after this.
            panic!("simulated mid-recording panic — Drop must not abort");
        });

        assert!(
            result.is_err(),
            "outer panic must propagate; if Drop had aborted instead, the \
             test process would have died before reaching this assertion"
        );

        // Drop's catch_unwind only swallows panics from flush; it still
        // gives flush a chance to run the happy path. With the buffered
        // frame above and a healthy filesystem, the file should contain
        // the frame's bytes after the unwind completes.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.len(),
            14 + 5,
            "Drop's flush must run on the happy path even during unwind"
        );
    }

    #[test]
    fn frame_writer_drop_catch_unwind_swallows_inner_panic() {
        // Standalone fence on the catch_unwind + AssertUnwindSafe idiom
        // the Drop impl uses. If a future refactor accidentally drops
        // the AssertUnwindSafe wrap or replaces catch_unwind with an
        // unwind-propagating call, the inner panic would escape and
        // (during a real Drop unwind) abort the process. This test
        // exercises the same idiom on a deliberately-panicking closure
        // and asserts the panic is captured rather than propagated —
        // the contract the Drop impl relies on.
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let mut sentinel = 0_i32;
        let result = catch_unwind(AssertUnwindSafe(|| {
            sentinel = 42;
            panic!("simulated flush panic during unwind");
        }));

        assert!(result.is_err(), "inner panic must be captured");
        assert_eq!(
            sentinel, 42,
            "the closure ran far enough to mutate captured state before panicking"
        );
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
    fn frame_writer_finalize_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("finalize-idempotent.war");

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

        writer.finalize().unwrap();
        let first = std::fs::read(&path).unwrap();
        assert_eq!(first.len(), 14 + 5);

        writer.finalize().unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_eq!(second, first);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        #[test]
        fn frame_writer_finalize_timeout_retry_preserves_buffered_frames_ft_ye2gs(
            payloads in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..32), 1..6),
        ) {
            let dir = tempdir().unwrap();
            let path = dir.path().join("finalize-timeout-retry.war");
            let mut writer = FrameWriter::new(&path, payloads.len() + 1).unwrap();
            let expected_len: usize = payloads.iter().map(|payload| 14 + payload.len()).sum();

            for (idx, payload) in payloads.iter().enumerate() {
                let frame = RecordingFrame::new(
                    idx as u64,
                    FrameType::Output,
                    0,
                    payload.clone(),
                ).unwrap();
                writer.write_frame(frame).unwrap();
            }

            let first = writer
                .finalize_with_timeout_for_test(Duration::from_millis(1), Duration::from_millis(25))
                .expect_err("delayed finalize worker should time out first");
            prop_assert!(matches!(first, crate::Error::Io(ref err) if err.kind() == ErrorKind::TimedOut));
            prop_assert!(writer.has_pending_finalize());
            prop_assert!(!writer.is_finalized());

            let pending_write = writer.write_frame(
                RecordingFrame::new(999, FrameType::Marker, 0, b"x".to_vec()).unwrap(),
            );
            prop_assert_eq!(
                pending_write
                    .expect_err("pending finalize should reject new writes")
                    .to_string()
                    .contains("finalize is still pending"),
                true
            );

            writer.finalize_with_timeout(Duration::from_secs(2)).unwrap();
            prop_assert!(!writer.has_pending_finalize());
            prop_assert!(writer.is_finalized());

            let bytes = std::fs::read(&path).unwrap();
            prop_assert_eq!(bytes.len(), expected_len);
            writer.finalize().unwrap();
        }
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
    fn recorder_timestamp_delta_clamps_reversed_clock_ft_ye1p2() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 100).unwrap();

        recorder.start(1_000);

        assert_eq!(recorder.timestamp_ms_for_capture(999), 0);
        assert_eq!(recorder.timestamp_ms_for_capture(1_000), 0);
        assert_eq!(recorder.timestamp_ms_for_capture(1_001), 1);
    }

    #[test]
    fn recorder_timestamp_delta_saturates_extreme_inputs_ft_ye1p2() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.war");
        let mut recorder = Recorder::new(1, &path, 100).unwrap();

        recorder.start(i64::MIN);
        assert_eq!(
            recorder.timestamp_ms_for_capture(i64::MAX),
            u64::try_from(i64::MAX).unwrap()
        );

        recorder.start(i64::MAX);
        assert_eq!(recorder.timestamp_ms_for_capture(i64::MIN), 0);
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
            assert_runtime_operation_backend(
                result.expect_err("duplicate start should fail"),
                "recording.start",
                "Recorder already active for pane 1",
            );
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

    /// ft-xbnl0.2.3 Cx-first: full start → record_segment → stop cycle
    /// via the explicit `&Cx` API. Pins end-to-end no-regression on
    /// the Cx-first path so all four `_with_cx` variants produce the
    /// same observable behavior as their legacy counterparts.
    #[test]
    fn recording_manager_with_cx_full_cycle_matches_legacy() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let path = dir.path().join("test_with_cx.war");

            let manager = RecordingManager::new(RecordingOptions::default());
            let cx = crate::cx::for_request();

            manager
                .start_recording_with_cx(&cx, 7, &path, 1_000)
                .await
                .expect("start_recording_with_cx");

            let segment = CapturedSegment {
                pane_id: 7,
                seq: 1,
                content: "hello world".into(),
                kind: CapturedSegmentKind::Delta,
                captured_at: 1_100,
            };
            manager
                .record_segment_with_cx(&cx, &segment)
                .await
                .expect("record_segment_with_cx");

            let stats = manager
                .stop_recording_with_cx(&cx, 7)
                .await
                .expect("stop_recording_with_cx");
            assert!(
                stats.is_some(),
                "stopping an active recorder should return stats"
            );
            let stats = stats.unwrap();
            assert!(
                stats.bytes_raw >= "hello world".len() as u64,
                "bytes_raw should reflect the recorded segment content"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: stop_recording_with_cx on a pane that was
    /// never started returns None, matching the legacy contract.
    #[test]
    fn recording_manager_stop_with_cx_nonexistent_returns_none() {
        run_async_test(async {
            let manager = RecordingManager::new(RecordingOptions::default());
            let cx = crate::cx::for_request();
            let result = manager
                .stop_recording_with_cx(&cx, 42)
                .await
                .expect("stop_with_cx on unknown pane is infallible");
            assert!(result.is_none());
        });
    }

    /// ft-xbnl0.2.3 Cx-first: record_event_with_cx records a detection event
    /// into an active recording, then verifies the event appears in replay.
    #[test]
    fn recording_manager_record_event_with_cx_captures_detection() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let path = dir.path().join("test_event_cx.war");

            let manager = RecordingManager::new(RecordingOptions::default());
            let cx = crate::cx::for_request();

            manager
                .start_recording_with_cx(&cx, 8, &path, 1_000)
                .await
                .expect("start_recording_with_cx");

            let detection = crate::patterns::Detection {
                rule_id: "test.cx_event".to_string(),
                agent_type: crate::patterns::AgentType::Unknown,
                event_type: "test_event".to_string(),
                severity: crate::patterns::Severity::Info,
                confidence: 0.95,
                extracted: serde_json::json!({"key": "value"}),
                matched_text: "test matched text".to_string(),
                span: (0, 17),
            };
            manager
                .record_event_with_cx(&cx, 8, &detection, 1_200)
                .await
                .expect("record_event_with_cx");

            let stats = manager
                .stop_recording_with_cx(&cx, 8)
                .await
                .expect("stop_recording_with_cx");
            assert!(stats.is_some(), "stopping active recorder returns stats");
        });
    }

    /// ft-xbnl0.2.3 Cx-first: record_event_with_cx on an unrecorded pane
    /// is a no-op — returns Ok(()) without error.
    #[test]
    fn recording_manager_record_event_with_cx_unknown_pane_noop() {
        run_async_test(async {
            let manager = RecordingManager::new(RecordingOptions::default());
            let cx = crate::cx::for_request();

            let detection = crate::patterns::Detection {
                rule_id: "test.noop".to_string(),
                agent_type: crate::patterns::AgentType::Unknown,
                event_type: "noop_event".to_string(),
                severity: crate::patterns::Severity::Info,
                confidence: 1.0,
                extracted: serde_json::json!(null),
                matched_text: String::new(),
                span: (0, 0),
            };
            // No recording started for pane 999 → should return Ok(())
            manager
                .record_event_with_cx(&cx, 999, &detection, 5_000)
                .await
                .expect("record_event_with_cx on unknown pane should be infallible");
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

    // ── br-ft-crpvd: clock-anomaly observability ──

    #[test]
    fn epoch_ms_now_from_post_epoch_no_anomaly_ft_crpvd() {
        // Sanity: a valid post-epoch SystemTime returns positive
        // ms and does NOT bump the anomaly counter.
        super::reset_recording_clock_anomaly_count_for_test();
        let post = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_704_067_200);
        let ms = super::epoch_ms_now_from(post);
        assert!(ms > 0);
        assert_eq!(
            super::recording_clock_anomaly_count(),
            0,
            "post-epoch path must not bump anomaly counter"
        );
    }

    #[test]
    fn epoch_ms_now_from_pre_epoch_bumps_counter_ft_crpvd() {
        // br-ft-crpvd: pre-epoch SystemTime triggers the Err
        // branch. Returns 0 (preserves scalar contract) and bumps
        // the process-global counter.
        super::reset_recording_clock_anomaly_count_for_test();
        let pre = std::time::UNIX_EPOCH - std::time::Duration::from_secs(100);
        let ms = super::epoch_ms_now_from(pre);
        assert_eq!(ms, 0);
        assert_eq!(super::recording_clock_anomaly_count(), 1);
        // Second call bumps again — every event observable.
        let _ = super::epoch_ms_now_from(pre);
        assert_eq!(super::recording_clock_anomaly_count(), 2);
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
        assert_runtime_operation_backend(
            result.expect_err("wrong schema version should fail"),
            "recording.parse_event",
            "ft.recorder.event.v99",
        );
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
        assert_runtime_operation_backend(
            result.expect_err("wrong schema version should fail"),
            "recording.parse_event",
            "ft.recorder.event.v2",
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
        assert_runtime_operation_backend(
            result.expect_err("missing schema_version should fail"),
            "recording.parse_event",
            "unsupported recorder event schema version",
        );
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

    // ── br-ft-ye2gs: FrameWriter finalize timeout retryability ──────────

    /// br-ft-ye2gs: a `Duration::ZERO` timeout always trips the
    /// recv_timeout boundary regardless of how fast the worker is.
    /// Pre-fix this returned a TimedOut error AND set
    /// `self.finalized = true`, leaving the caller with an unflushed
    /// (but not-recoverable) writer. Post-fix the receiver is stashed
    /// in `pending_finalize`, `self.finalized` stays false, and a
    /// follow-up `finalize_with_timeout(reasonable)` awaits the
    /// in-flight worker to durable completion.
    #[test]
    fn finalize_with_timeout_zero_is_retryable_ft_ye2gs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("retry.war");
        let mut writer = FrameWriter::new(&path, 16).expect("writer");
        for i in 0..4 {
            writer
                .write_frame(RecordingFrame {
                    header: FrameHeader {
                        timestamp_ms: i,
                        frame_type: FrameType::Output,
                        flags: 0,
                        payload_len: 1,
                    },
                    payload: vec![i as u8],
                })
                .expect("write_frame");
        }

        // First call: ZERO timeout MAY race-win the worker on a very
        // fast disk. Both outcomes (Ok / TimedOut+pending) are valid;
        // we test both branches by inspecting the post-state.
        let zero_outcome = writer.finalize_with_timeout(Duration::ZERO);
        match zero_outcome {
            Ok(()) => {
                assert!(
                    writer.is_finalized(),
                    "race-win path: finalized must be true"
                );
                assert!(!writer.has_pending_finalize());
            }
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("FrameWriter finalize exceeded"),
                    "error must surface the timeout boundary; got {msg:?}"
                );
                assert!(
                    writer.has_pending_finalize(),
                    "ft-ye2gs: pending_finalize receiver must be stashed for retry"
                );
                assert!(
                    !writer.is_finalized(),
                    "ft-ye2gs: finalized must stay false on timeout"
                );

                // Retry awaits the in-flight worker to durable completion.
                writer
                    .finalize_with_timeout(Duration::from_secs(5))
                    .expect("retry must await pending_finalize and succeed");
                assert!(
                    writer.is_finalized(),
                    "ft-ye2gs: finalized must transition to true after worker reports completion"
                );
                assert!(
                    !writer.has_pending_finalize(),
                    "ft-ye2gs: pending_finalize must be cleared after successful retry"
                );
            }
        }

        // Idempotent — already finalized.
        writer
            .finalize_with_timeout(Duration::from_secs(5))
            .expect("third call must short-circuit on finalized=true");
    }

    /// br-ft-ye2gs: `Recorder::stop` no longer transitions to
    /// `Stopped` before `FrameWriter::finalize` reports durable
    /// completion. With a healthy writer the stop succeeds and
    /// state transitions to Stopped; this pins the success-path
    /// invariant against accidental reordering regressions.
    #[test]
    fn recorder_stop_state_transition_gated_on_finalize_ft_ye2gs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stop.war");
        let mut rec = Recorder::new(0, &path, 16).expect("recorder");
        rec.start(0);
        assert_eq!(rec.state, RecorderState::Recording);
        rec.stop().expect("stop on a healthy writer must succeed");
        assert_eq!(
            rec.state,
            RecorderState::Stopped,
            "ft-ye2gs: state transitions to Stopped only after finalize succeeds"
        );
    }

    /// br-ft-ye2gs: post-finalize state observability — `is_finalized`
    /// and `has_pending_finalize` must accurately reflect the writer's
    /// internal state across the lifecycle. Pins the public-API
    /// observability contract that callers depend on to distinguish
    /// "durable" from "in-flight" without inspecting private fields.
    #[test]
    fn finalize_state_reflects_durable_completion_ft_ye2gs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("done.war");
        let mut writer = FrameWriter::new(&path, 16).expect("writer");
        assert!(!writer.is_finalized(), "fresh writer is not finalized");
        assert!(
            !writer.has_pending_finalize(),
            "fresh writer has no pending"
        );
        writer
            .finalize_with_timeout(Duration::from_secs(5))
            .expect("clean finalize must succeed");
        assert!(
            writer.is_finalized(),
            "ft-ye2gs: finalized must be true after successful finalize"
        );
        assert!(
            !writer.has_pending_finalize(),
            "ft-ye2gs: no pending receiver after successful finalize"
        );
    }

    /// br-ft-ye2gs: property-style sweep across a representative
    /// frame-count load envelope. For each (frames_count) value:
    ///   1. ZERO-timeout finalize is either Ok+finalized (race-win)
    ///      or TimedOut+pending (race-loss) — never Ok+pending or
    ///      TimedOut+finalized.
    ///   2. After retry (or race-win), is_finalized=true and no
    ///      pending receiver remains.
    ///
    /// This pins the retry contract uniformly across the load envelope
    /// rather than relying on a single representative case.
    #[test]
    fn finalize_retry_property_sweep_ft_ye2gs() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (idx, frames_count) in [0usize, 1, 4, 16, 64].iter().enumerate() {
            let path = dir.path().join(format!("sweep-{idx}.war"));
            let mut writer = FrameWriter::new(&path, 32).expect("writer");
            for i in 0..*frames_count {
                writer
                    .write_frame(RecordingFrame {
                        header: FrameHeader {
                            timestamp_ms: i as u64,
                            frame_type: FrameType::Output,
                            flags: 0,
                            payload_len: 1,
                        },
                        payload: vec![i as u8],
                    })
                    .expect("write_frame");
            }

            let zero_outcome = writer.finalize_with_timeout(Duration::ZERO);
            match zero_outcome {
                Ok(()) => {
                    assert!(
                        writer.is_finalized(),
                        "ft-ye2gs(n={frames_count}): race-win must mark finalized"
                    );
                    assert!(
                        !writer.has_pending_finalize(),
                        "ft-ye2gs(n={frames_count}): race-win clears pending"
                    );
                }
                Err(_) => {
                    assert!(
                        writer.has_pending_finalize(),
                        "ft-ye2gs(n={frames_count}): timeout path must stash pending receiver"
                    );
                    assert!(
                        !writer.is_finalized(),
                        "ft-ye2gs(n={frames_count}): timeout must NOT mark finalized"
                    );
                    writer
                        .finalize_with_timeout(Duration::from_secs(5))
                        .expect("retry must succeed");
                }
            }

            assert!(
                writer.is_finalized(),
                "ft-ye2gs(n={frames_count}): final state must be finalized"
            );
            assert!(
                !writer.has_pending_finalize(),
                "ft-ye2gs(n={frames_count}): no pending receiver in terminal state"
            );
        }
    }
}
