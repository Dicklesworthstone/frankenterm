use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::config as wa_config;
use crate::runtime_compat::{sleep, timeout};
use codec::{
    CODEC_VERSION, CompressionMode, DecodedPdu, GetCodecVersion, GetCodecVersionResponse, GetLines,
    GetLinesResponse, GetPaneRenderChanges, GetPaneRenderChangesResponse, ListPanes,
    ListPanesResponse, Pdu, SendPaste, SetClientId, UnitResponse, WriteToPane,
};
use config as wezterm_config;
use mux::client::ClientId;

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_READ_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_WRITE_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DirectMuxClientConfig {
    pub socket_path: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_frame_bytes: usize,
    pub compression_mode: wa_config::VendoredCompressionMode,
}

impl DirectMuxClientConfig {
    pub fn from_wa_config(config: &wa_config::Config) -> Self {
        let mut cfg = Self::default();
        if let Some(path) = &config.vendored.mux_socket_path {
            if !path.trim().is_empty() {
                cfg.socket_path = Some(PathBuf::from(path));
            }
        }
        cfg.compression_mode = config.vendored.mux_pool.compression;
        cfg
    }

    #[must_use]
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = Some(path.into());
        self
    }
}

impl Default for DirectMuxClientConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            read_timeout: Duration::from_millis(DEFAULT_READ_TIMEOUT_MS),
            write_timeout: Duration::from_millis(DEFAULT_WRITE_TIMEOUT_MS),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            compression_mode: wa_config::VendoredCompressionMode::Auto,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DirectMuxError {
    #[error("mux socket path not found; set WEZTERM_UNIX_SOCKET or wa vendored.mux_socket_path")]
    SocketPathMissing,
    #[error("mux socket not found at {0}")]
    SocketNotFound(PathBuf),
    #[error("mux proxy command not supported for direct client")]
    ProxyUnsupported,
    #[error("connect to mux socket timed out: {0}")]
    ConnectTimeout(PathBuf),
    #[error("read from mux socket timed out")]
    ReadTimeout,
    #[error("write to mux socket timed out")]
    WriteTimeout,
    #[error("mux socket disconnected")]
    Disconnected,
    #[error("frame exceeded max size ({max_bytes} bytes)")]
    FrameTooLarge { max_bytes: usize },
    #[error("request serial space exhausted for this connection")]
    SerialExhausted,
    #[error("codec error: {0}")]
    Codec(String),
    #[error("remote error: {0}")]
    RemoteError(String),
    #[error("pipeline batch timed out after {timeout_ms}ms")]
    BatchTimeout { timeout_ms: u64 },
    #[error("unexpected response: expected {expected}, got {got}")]
    UnexpectedResponse { expected: String, got: String },
    #[error("codec version mismatch: local {local} != remote {remote} (version {remote_version})")]
    IncompatibleCodec {
        local: usize,
        remote: usize,
        remote_version: String,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Coarse classification of mux protocol errors for retry/reconnect decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    /// Connection is likely corrupted/out-of-sync; reconnect and retry.
    Recoverable,
    /// Configuration/version problems; don't retry.
    Permanent,
    /// Temporary condition; retry may succeed without requiring reconnection.
    Transient,
}

impl DirectMuxError {
    /// Classify an error into a retry/reconnect decision bucket.
    #[must_use]
    pub fn protocol_error_kind(&self) -> ProtocolErrorKind {
        match self {
            Self::UnexpectedResponse { .. }
            | Self::Disconnected
            | Self::ReadTimeout
            | Self::WriteTimeout
            | Self::ConnectTimeout(_)
            | Self::FrameTooLarge { .. }
            | Self::Codec(_)
            | Self::BatchTimeout { .. }
            | Self::SerialExhausted => ProtocolErrorKind::Recoverable,
            Self::IncompatibleCodec { .. }
            | Self::SocketPathMissing
            | Self::SocketNotFound(_)
            | Self::ProxyUnsupported => ProtocolErrorKind::Permanent,
            Self::RemoteError(_) => ProtocolErrorKind::Transient,
            Self::Io(err) => match err.kind() {
                std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected => ProtocolErrorKind::Recoverable,
                _ => ProtocolErrorKind::Transient,
            },
        }
    }
}

pub struct DirectMuxClient {
    stream: UnixStream,
    socket_path: PathBuf,
    read_buf: Vec<u8>,
    serial: u64,
    pending_responses: HashMap<u64, Pdu>,
    config: DirectMuxClientConfig,
    compression_mode: CompressionMode,
}

impl std::fmt::Debug for DirectMuxClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectMuxClient")
            .field("socket_path", &self.socket_path)
            .field("serial", &self.serial)
            .field("pending_responses", &self.pending_responses.len())
            .field("compression_mode", &self.compression_mode)
            .finish_non_exhaustive()
    }
}

impl DirectMuxClient {
    pub async fn connect(config: DirectMuxClientConfig) -> Result<Self, DirectMuxError> {
        let socket_path = resolve_socket_path(&config)?;
        if !socket_path.exists() {
            return Err(DirectMuxError::SocketNotFound(socket_path));
        }

        let stream = timeout(config.connect_timeout, UnixStream::connect(&socket_path))
            .await
            .map_err(|_| DirectMuxError::ConnectTimeout(socket_path.clone()))??;

        let mut client = Self {
            stream,
            compression_mode: resolve_compression_mode(config.compression_mode, &socket_path),
            socket_path,
            read_buf: Vec::new(),
            serial: 0,
            pending_responses: HashMap::new(),
            config,
        };

        client.verify_codec_version().await?;
        client.register_client().await?;

        Ok(client)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn list_panes(&mut self) -> Result<ListPanesResponse, DirectMuxError> {
        let response = self.send_request(Pdu::ListPanes(ListPanes {})).await?;
        match response {
            Pdu::ListPanesResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "ListPanesResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    /// Poll the mux server for render changes since the last check for a pane.
    pub async fn get_pane_render_changes(
        &mut self,
        pane_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        let response = self
            .send_request(Pdu::GetPaneRenderChanges(GetPaneRenderChanges {
                pane_id: pane_id as usize,
            }))
            .await?;
        match response {
            Pdu::GetPaneRenderChangesResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "GetPaneRenderChangesResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    /// Fetch specific lines from a pane's scrollback.
    pub async fn get_lines(
        &mut self,
        pane_id: u64,
        lines: Vec<std::ops::Range<isize>>,
    ) -> Result<GetLinesResponse, DirectMuxError> {
        let response = self
            .send_request(Pdu::GetLines(GetLines {
                pane_id: pane_id as usize,
                lines,
            }))
            .await?;
        match response {
            Pdu::GetLinesResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "GetLinesResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    /// Write raw bytes to a pane (no-paste mode, character-by-character).
    pub async fn write_to_pane(
        &mut self,
        pane_id: u64,
        data: Vec<u8>,
    ) -> Result<UnitResponse, DirectMuxError> {
        let response = self
            .send_request(Pdu::WriteToPane(WriteToPane {
                pane_id: pane_id as usize,
                data,
            }))
            .await?;
        match response {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "UnitResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    /// Send text via paste mode (efficient for multi-character input).
    pub async fn send_paste(
        &mut self,
        pane_id: u64,
        data: String,
    ) -> Result<UnitResponse, DirectMuxError> {
        let response = self
            .send_request(Pdu::SendPaste(SendPaste {
                pane_id: pane_id as usize,
                data,
            }))
            .await?;
        match response {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "UnitResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    async fn verify_codec_version(&mut self) -> Result<GetCodecVersionResponse, DirectMuxError> {
        let response = self
            .send_request(Pdu::GetCodecVersion(GetCodecVersion {}))
            .await?;
        match response {
            Pdu::GetCodecVersionResponse(payload) => {
                if payload.codec_vers != CODEC_VERSION {
                    return Err(DirectMuxError::IncompatibleCodec {
                        local: CODEC_VERSION,
                        remote: payload.codec_vers,
                        remote_version: payload.version_string.clone(),
                    });
                }
                Ok(payload)
            }
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "GetCodecVersionResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    async fn register_client(&mut self) -> Result<UnitResponse, DirectMuxError> {
        let client_id = ClientId::new();
        let response = self
            .send_request(Pdu::SetClientId(SetClientId {
                client_id,
                is_proxy: false,
            }))
            .await?;
        match response {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "UnitResponse".to_string(),
                got: other.pdu_name().to_string(),
            }),
        }
    }

    /// Batch `GetPaneRenderChanges` requests with depth-limited pipelining.
    ///
    /// Responses are returned in the same order as `pane_ids`, regardless of
    /// on-wire response ordering.
    pub async fn get_pane_render_changes_batch(
        &mut self,
        pane_ids: &[u64],
        max_pipeline_depth: usize,
        pipeline_timeout: Duration,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, DirectMuxError> {
        if pane_ids.is_empty() {
            return Ok(Vec::new());
        }

        let requests = pane_ids
            .iter()
            .map(|pane_id| {
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges {
                    pane_id: *pane_id as usize,
                })
            })
            .collect::<Vec<_>>();

        let responses = self
            .batch(requests, max_pipeline_depth, pipeline_timeout)
            .await?;
        let mut out = Vec::with_capacity(responses.len());
        for response in responses {
            match response {
                Pdu::GetPaneRenderChangesResponse(payload) => out.push(payload),
                other => {
                    return Err(DirectMuxError::UnexpectedResponse {
                        expected: "GetPaneRenderChangesResponse".to_string(),
                        got: other.pdu_name().to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Send a batch of requests using depth-limited pipelining.
    ///
    /// The method issues up to `max_pipeline_depth` requests before waiting
    /// for a response, then keeps the pipeline full until all requests are
    /// completed. Responses are returned in request order.
    pub async fn batch(
        &mut self,
        requests: Vec<Pdu>,
        max_pipeline_depth: usize,
        pipeline_timeout: Duration,
    ) -> Result<Vec<Pdu>, DirectMuxError> {
        let timeout_ms = duration_to_ms_u64(pipeline_timeout);
        timeout(
            pipeline_timeout,
            self.batch_inner(requests, max_pipeline_depth.max(1)),
        )
        .await
        .map_err(|_| DirectMuxError::BatchTimeout { timeout_ms })?
    }

    async fn batch_inner(
        &mut self,
        requests: Vec<Pdu>,
        max_pipeline_depth: usize,
    ) -> Result<Vec<Pdu>, DirectMuxError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        if max_pipeline_depth <= 1 {
            let mut responses = Vec::with_capacity(requests.len());
            for request in requests {
                responses.push(self.send_request(request).await?);
            }
            return Ok(responses);
        }

        let total = requests.len();
        let mut requests = requests.into_iter().enumerate();
        let mut in_flight: VecDeque<(usize, u64)> = VecDeque::with_capacity(max_pipeline_depth);
        let mut responses: Vec<Option<Pdu>> = std::iter::repeat_with(|| None).take(total).collect();

        while in_flight.len() < max_pipeline_depth {
            let Some((request_idx, request)) = requests.next() else {
                break;
            };
            let serial = self.send_request_only(request).await?;
            in_flight.push_back((request_idx, serial));
        }

        while !in_flight.is_empty() {
            let decoded = self.read_next_pdu().await?;
            if let Some(response_idx) = take_in_flight_slot(&mut in_flight, decoded.serial) {
                responses[response_idx] = Some(Self::response_from_pdu(decoded.pdu)?);
                if let Some((request_idx, request)) = requests.next() {
                    let serial = self.send_request_only(request).await?;
                    in_flight.push_back((request_idx, serial));
                }
            } else {
                self.stash_pending_response(decoded.serial, decoded.pdu)?;
            }
        }

        let mut ordered = Vec::with_capacity(total);
        for response in responses {
            ordered.push(response.ok_or_else(|| {
                DirectMuxError::Codec("pipeline batch completed with missing response".to_string())
            })?);
        }
        Ok(ordered)
    }

    async fn send_request(&mut self, pdu: Pdu) -> Result<Pdu, DirectMuxError> {
        let serial = self.send_request_only(pdu).await?;
        self.await_response(serial).await
    }

    async fn send_request_only(&mut self, pdu: Pdu) -> Result<u64, DirectMuxError> {
        let serial = next_request_serial(&mut self.serial)?;
        let mut buf = Vec::new();
        pdu.encode_with_mode(&mut buf, serial, self.compression_mode)
            .map_err(|err| DirectMuxError::Codec(err.to_string()))?;
        timeout(self.config.write_timeout, self.stream.write_all(&buf))
            .await
            .map_err(|_| DirectMuxError::WriteTimeout)??;
        Ok(serial)
    }

    async fn await_response(&mut self, serial: u64) -> Result<Pdu, DirectMuxError> {
        if let Some(pending) = self.pending_responses.remove(&serial) {
            return Self::response_from_pdu(pending);
        }
        loop {
            let decoded = self.read_next_pdu().await?;
            if decoded.serial == serial {
                return Self::response_from_pdu(decoded.pdu);
            }
            self.stash_pending_response(decoded.serial, decoded.pdu)?;
        }
    }

    fn response_from_pdu(pdu: Pdu) -> Result<Pdu, DirectMuxError> {
        match pdu {
            Pdu::ErrorResponse(err) => Err(DirectMuxError::RemoteError(err.reason)),
            other => Ok(other),
        }
    }

    fn stash_pending_response(&mut self, serial: u64, pdu: Pdu) -> Result<(), DirectMuxError> {
        if self.pending_responses.insert(serial, pdu).is_some() {
            return Err(DirectMuxError::UnexpectedResponse {
                expected: "unique serial".to_string(),
                got: format!("duplicate response serial {serial}"),
            });
        }
        Ok(())
    }

    async fn read_next_pdu(&mut self) -> Result<DecodedPdu, DirectMuxError> {
        loop {
            if let Some(decoded) =
                decode_from_buffer(&mut self.read_buf, self.config.max_frame_bytes)?
            {
                return Ok(decoded);
            }

            let mut temp = vec![0u8; 4096];
            let read = timeout(self.config.read_timeout, self.stream.read(&mut temp))
                .await
                .map_err(|_| DirectMuxError::ReadTimeout)??;
            if read == 0 {
                return Err(DirectMuxError::Disconnected);
            }
            self.read_buf.extend_from_slice(&temp[..read]);
            if self.read_buf.len() > self.config.max_frame_bytes {
                return Err(DirectMuxError::FrameTooLarge {
                    max_bytes: self.config.max_frame_bytes,
                });
            }
        }
    }
}

fn next_request_serial(serial: &mut u64) -> Result<u64, DirectMuxError> {
    *serial = serial
        .checked_add(1)
        .ok_or(DirectMuxError::SerialExhausted)?;
    Ok(*serial)
}

fn duration_to_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn take_in_flight_slot(in_flight: &mut VecDeque<(usize, u64)>, serial: u64) -> Option<usize> {
    let pos = in_flight
        .iter()
        .position(|(_, expected)| *expected == serial)?;
    in_flight.remove(pos).map(|(idx, _)| idx)
}

fn decode_from_buffer(
    buffer: &mut Vec<u8>,
    max_frame_bytes: usize,
) -> Result<Option<DecodedPdu>, DirectMuxError> {
    if buffer.len() > max_frame_bytes {
        return Err(DirectMuxError::FrameTooLarge {
            max_bytes: max_frame_bytes,
        });
    }
    codec::Pdu::stream_decode(buffer).map_err(|err| DirectMuxError::Codec(err.to_string()))
}

fn resolve_socket_path(config: &DirectMuxClientConfig) -> Result<PathBuf, DirectMuxError> {
    if let Some(path) = &config.socket_path {
        return Ok(path.clone());
    }

    if let Some(path) = std::env::var_os("WEZTERM_UNIX_SOCKET") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    let handle = wezterm_config::configuration_result()
        .unwrap_or_else(|_| wezterm_config::ConfigHandle::default_config());
    if let Some(domain) = handle.unix_domains.first() {
        if domain.proxy_command.is_some() {
            return Err(DirectMuxError::ProxyUnsupported);
        }
        return Ok(domain.socket_path());
    }

    let mut default_domains = wezterm_config::UnixDomain::default_unix_domains();
    if let Some(domain) = default_domains.pop() {
        return Ok(domain.socket_path());
    }

    Err(DirectMuxError::SocketPathMissing)
}

fn resolve_compression_mode(
    mode: wa_config::VendoredCompressionMode,
    socket_path: &Path,
) -> CompressionMode {
    match mode {
        wa_config::VendoredCompressionMode::Always => CompressionMode::Always,
        wa_config::VendoredCompressionMode::Never => CompressionMode::Never,
        wa_config::VendoredCompressionMode::Auto => {
            if is_local_unix_socket(socket_path) {
                CompressionMode::Never
            } else {
                CompressionMode::Auto
            }
        }
    }
}

fn is_local_unix_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    std::fs::metadata(path)
        .map(|meta| meta.file_type().is_socket())
        // If metadata is unavailable, keep `auto` in the safe local-fast path.
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// PaneOutputSubscription: stream pane output as deltas (wa-nu4.4.2.2)
// ---------------------------------------------------------------------------

/// A delta event from a pane's output, compatible with the seq/gap model.
#[derive(Debug, Clone)]
pub enum PaneDelta {
    /// New content was rendered (dirty lines changed).
    Output {
        pane_id: u64,
        /// Mux-side sequence number from `GetPaneRenderChangesResponse`.
        seqno: u64,
        /// Title of the pane at the time of the delta.
        title: String,
        /// Number of dirty line ranges reported.
        dirty_range_count: usize,
    },
    /// A gap was detected (polling too slow or reconnect).
    Gap { pane_id: u64, reason: String },
    /// Subscription ended (pane closed, shutdown, or error).
    Ended { pane_id: u64, reason: String },
}

/// Configuration for a pane output subscription.
#[derive(Debug, Clone)]
pub struct SubscriptionConfig {
    /// How often to poll `GetPaneRenderChanges` when idle.
    pub poll_interval: Duration,
    /// Minimum interval between polls when active.
    pub min_poll_interval: Duration,
    /// Channel capacity for the delta stream.
    pub channel_capacity: usize,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            min_poll_interval: Duration::from_millis(20),
            channel_capacity: 256,
        }
    }
}

/// A handle to a running pane output subscription.
///
/// Dropping this handle cancels the subscription.
pub struct PaneOutputSubscription {
    receiver: tokio::sync::mpsc::Receiver<PaneDelta>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl PaneOutputSubscription {
    /// Receive the next delta. Returns `None` when the subscription ends.
    pub async fn next(&mut self) -> Option<PaneDelta> {
        self.receiver.recv().await
    }

    /// Cancel the subscription.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

impl Drop for PaneOutputSubscription {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

/// Start a subscription to a pane's output via `GetPaneRenderChanges` polling.
///
/// This spawns a background task that polls the mux server and emits
/// `PaneDelta` events through a bounded channel. Dropping the returned
/// `PaneOutputSubscription` cancels the background poller.
///
/// The poller tracks the last seen `seqno` and emits a `PaneDelta::Gap`
/// if the mux-side seqno jumps by more than 1.
pub fn subscribe_pane_output(
    mut client: DirectMuxClient,
    pane_id: u64,
    config: SubscriptionConfig,
) -> PaneOutputSubscription {
    let (tx, rx) = tokio::sync::mpsc::channel(config.channel_capacity);
    let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let mut last_seqno: Option<u64> = None;

        loop {
            // Check cancellation
            if *cancel_rx.borrow() {
                let _ = tx
                    .send(PaneDelta::Ended {
                        pane_id,
                        reason: "cancelled".to_string(),
                    })
                    .await;
                break;
            }

            // Poll for render changes
            let result = client.get_pane_render_changes(pane_id).await;

            match result {
                Ok(changes) => {
                    let seqno = changes.seqno as u64;
                    let has_dirty = !changes.dirty_lines.is_empty();

                    // Detect gaps in seqno
                    if let Some(prev) = last_seqno {
                        if seqno > prev + 1 {
                            let _ = tx
                                .send(PaneDelta::Gap {
                                    pane_id,
                                    reason: format!(
                                        "seqno jump: {} -> {} (missed {})",
                                        prev,
                                        seqno,
                                        seqno - prev - 1
                                    ),
                                })
                                .await;
                        }
                    }
                    last_seqno = Some(seqno);

                    // Only emit Output delta if there are dirty lines
                    if has_dirty {
                        let delta = PaneDelta::Output {
                            pane_id,
                            seqno,
                            title: changes.title,
                            dirty_range_count: changes.dirty_lines.len(),
                        };

                        // Bounded send — if the channel is full, emit a gap
                        if tx.try_send(delta).is_err() {
                            let _ = tx
                                .send(PaneDelta::Gap {
                                    pane_id,
                                    reason: "slow consumer: channel full".to_string(),
                                })
                                .await;
                        }
                    }
                }
                Err(DirectMuxError::Disconnected) => {
                    let _ = tx
                        .send(PaneDelta::Ended {
                            pane_id,
                            reason: "mux socket disconnected".to_string(),
                        })
                        .await;
                    break;
                }
                Err(DirectMuxError::ReadTimeout) => {
                    // Transient — continue polling
                    tracing::debug!(pane_id, "subscription poll timeout, retrying");
                }
                Err(err) => {
                    let _ = tx
                        .send(PaneDelta::Ended {
                            pane_id,
                            reason: format!("subscription error: {err}"),
                        })
                        .await;
                    break;
                }
            }

            // Wait for the next poll interval or cancellation
            tokio::select! {
                () = sleep(config.poll_interval) => {}
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        let _ = tx.send(PaneDelta::Ended {
                            pane_id,
                            reason: "cancelled".to_string(),
                        }).await;
                        break;
                    }
                }
            }
        }
    });

    PaneOutputSubscription {
        receiver: rx,
        cancel: cancel_tx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn decode_from_buffer_roundtrip() {
        let mut buf = Vec::new();
        let pdu = Pdu::Ping(codec::Ping {});
        pdu.encode(&mut buf, 42).expect("encode should succeed");

        let mut partial = buf[..buf.len() / 2].to_vec();
        let result = decode_from_buffer(&mut partial, 1024).expect("decode should not error");
        assert!(result.is_none());

        partial.extend_from_slice(&buf[buf.len() / 2..]);
        let decoded = decode_from_buffer(&mut partial, 1024)
            .expect("decode should succeed")
            .expect("should decode");
        assert_eq!(decoded.serial, 42);
    }

    #[test]
    fn decode_from_buffer_rejects_oversize() {
        let mut buf = vec![0u8; 10];
        let err = decode_from_buffer(&mut buf, 4).expect_err("should reject oversize buffer");
        match err {
            DirectMuxError::FrameTooLarge { .. } => {}
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn list_panes_roundtrip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("mux.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind listener");

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut read_buf = Vec::new();
            let mut responses: HashMap<u64, Pdu> = HashMap::new();
            loop {
                let mut temp = vec![0u8; 4096];
                let read = stream.read(&mut temp).await.expect("read");
                if read == 0 {
                    break;
                }
                read_buf.extend_from_slice(&temp[..read]);
                while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            let payload = GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "wezterm-test".to_string(),
                                executable_path: PathBuf::from("/bin/wezterm"),
                                config_file_path: None,
                            };
                            Pdu::GetCodecVersionResponse(payload)
                        }
                        Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                        Pdu::ListPanes(_) => {
                            let payload = ListPanesResponse {
                                tabs: Vec::new(),
                                tab_titles: Vec::new(),
                                window_titles: HashMap::new(),
                            };
                            Pdu::ListPanesResponse(payload)
                        }
                        _ => continue,
                    };
                    responses.insert(decoded.serial, response);
                }

                for (serial, pdu) in responses.drain() {
                    let mut out = Vec::new();
                    pdu.encode(&mut out, serial).expect("encode response");
                    stream.write_all(&out).await.expect("write response");
                }
            }
        });

        let mut config = DirectMuxClientConfig::default();
        config.socket_path = Some(socket_path);
        let mut client = DirectMuxClient::connect(config).await.expect("connect");
        let panes = client.list_panes().await.expect("list panes");
        assert!(panes.tabs.is_empty());
    }

    #[test]
    fn next_request_serial_rejects_overflow() {
        let mut serial = u64::MAX;
        let err = next_request_serial(&mut serial).expect_err("overflow should be rejected");
        assert!(matches!(err, DirectMuxError::SerialExhausted));
    }

    fn permutation_from_keys(keys: &[u32]) -> Vec<usize> {
        let mut with_index = keys
            .iter()
            .copied()
            .enumerate()
            .map(|(idx, key)| (key, idx))
            .collect::<Vec<_>>();
        with_index.sort_unstable();
        with_index.into_iter().map(|(_, idx)| idx).collect()
    }

    fn causal_response_order(
        total_requests: usize,
        max_pipeline_depth: usize,
        keys: &[u32],
    ) -> Vec<usize> {
        let mut in_flight: VecDeque<usize> = VecDeque::new();
        let mut order = Vec::with_capacity(total_requests);
        let depth = max_pipeline_depth.max(1);
        let mut next_request = 0usize;
        let mut key_cursor = 0usize;

        while next_request < total_requests && in_flight.len() < depth {
            in_flight.push_back(next_request);
            next_request += 1;
        }

        while !in_flight.is_empty() {
            let key = keys[key_cursor % keys.len()];
            let pick = (key as usize) % in_flight.len();
            let response_idx = in_flight
                .remove(pick)
                .expect("picked index must refer to in-flight request");
            order.push(response_idx);

            if next_request < total_requests {
                in_flight.push_back(next_request);
                next_request += 1;
            }
            key_cursor += 1;
        }

        order
    }

    fn simulate_pipeline_dispatch(
        total_requests: usize,
        max_pipeline_depth: usize,
        response_order: &[usize],
    ) -> (Vec<Option<u64>>, usize) {
        let depth = max_pipeline_depth.max(1);
        let mut in_flight: VecDeque<(usize, u64)> = VecDeque::new();
        let mut delivered: Vec<Option<u64>> = vec![None; total_requests];
        let mut next_request = 0usize;
        let mut peak = 0usize;

        while next_request < total_requests && in_flight.len() < depth {
            let serial = (next_request + 1) as u64;
            in_flight.push_back((next_request, serial));
            next_request += 1;
            peak = peak.max(in_flight.len());
        }

        for &response_idx in response_order {
            let serial = (response_idx + 1) as u64;
            let slot = take_in_flight_slot(&mut in_flight, serial)
                .expect("response serial must correspond to an in-flight request");
            delivered[slot] = Some(serial);
            if next_request < total_requests {
                let serial = (next_request + 1) as u64;
                in_flight.push_back((next_request, serial));
                next_request += 1;
                peak = peak.max(in_flight.len());
            }
        }

        (delivered, peak)
    }

    proptest! {
        #[test]
        fn prop_message_ordering_invariant(keys in prop::collection::vec(any::<u32>(), 1..64)) {
            let total = keys.len();
            let order = permutation_from_keys(&keys);
            let (delivered, _) = simulate_pipeline_dispatch(total, total, &order);

            for (idx, serial) in delivered.into_iter().enumerate() {
                prop_assert_eq!(serial, Some((idx + 1) as u64));
            }
        }
    }

    proptest! {
        #[test]
        fn prop_pipeline_completeness(
            (total, depth, keys) in (1usize..96, 1usize..32).prop_flat_map(|(total, depth)| {
                (
                    Just(total),
                    Just(depth),
                    prop::collection::vec(any::<u32>(), total),
                )
            })
        ) {
            let order = causal_response_order(total, depth, &keys);
            let (delivered, _) = simulate_pipeline_dispatch(total, depth, &order);

            prop_assert_eq!(delivered.iter().filter(|v| v.is_some()).count(), total);
            let unique = delivered
                .into_iter()
                .flatten()
                .collect::<HashSet<_>>();
            prop_assert_eq!(unique.len(), total);
        }
    }

    proptest! {
        #[test]
        fn prop_sequence_numbers_monotonic_and_unique(
            start in 0u64..1_000_000,
            count in 1usize..10_000
        ) {
            let mut serial = start;
            let mut previous = serial;
            let mut seen = HashSet::new();

            for _ in 0..count {
                let next = next_request_serial(&mut serial).expect("serial should advance");
                prop_assert!(next > previous);
                prop_assert!(seen.insert(next));
                previous = next;
            }
        }
    }

    proptest! {
        #[test]
        fn prop_depth_limiting_enforced(
            (total, depth, keys) in (1usize..96, 1usize..64).prop_flat_map(|(total, depth)| {
                (
                    Just(total),
                    Just(depth),
                    prop::collection::vec(any::<u32>(), total),
                )
            })
        ) {
            let order = causal_response_order(total, depth, &keys);
            let (_delivered, peak) = simulate_pipeline_dispatch(total, depth, &order);

            prop_assert!(peak <= depth.max(1));
            prop_assert_eq!(peak, total.min(depth.max(1)));
        }
    }

    #[test]
    fn default_config_has_sane_timeouts() {
        let config = DirectMuxClientConfig::default();
        assert!(config.connect_timeout.as_secs() > 0);
        assert!(config.read_timeout.as_secs() > 0);
        assert!(config.write_timeout.as_secs() > 0);
        assert!(config.max_frame_bytes > 0);
        assert!(config.socket_path.is_none());
        assert_eq!(
            config.compression_mode,
            crate::config::VendoredCompressionMode::Auto
        );
    }

    #[test]
    fn config_from_wa_config_with_socket_path() {
        let mut wa_cfg = crate::config::Config::default();
        wa_cfg.vendored.mux_socket_path = Some("/tmp/test.sock".to_string());
        let config = DirectMuxClientConfig::from_wa_config(&wa_cfg);
        assert_eq!(
            config.socket_path.as_ref().map(|p| p.to_str().unwrap()),
            Some("/tmp/test.sock")
        );
        assert_eq!(
            config.compression_mode,
            crate::config::VendoredCompressionMode::Auto
        );
    }

    #[test]
    fn config_from_wa_config_without_socket_path() {
        let wa_cfg = crate::config::Config::default();
        let config = DirectMuxClientConfig::from_wa_config(&wa_cfg);
        assert!(config.socket_path.is_none());
        assert_eq!(
            config.compression_mode,
            crate::config::VendoredCompressionMode::Auto
        );
    }

    #[test]
    fn config_from_wa_config_empty_path_is_none() {
        let mut wa_cfg = crate::config::Config::default();
        wa_cfg.vendored.mux_socket_path = Some("  ".to_string());
        let config = DirectMuxClientConfig::from_wa_config(&wa_cfg);
        assert!(config.socket_path.is_none());
    }

    #[test]
    fn config_from_wa_config_with_compression_mode() {
        let mut wa_cfg = crate::config::Config::default();
        wa_cfg.vendored.mux_pool.compression = crate::config::VendoredCompressionMode::Never;
        let config = DirectMuxClientConfig::from_wa_config(&wa_cfg);
        assert_eq!(
            config.compression_mode,
            crate::config::VendoredCompressionMode::Never
        );
    }

    #[test]
    fn config_with_socket_path_builder() {
        let config = DirectMuxClientConfig::default().with_socket_path("/tmp/mux.sock");
        assert_eq!(
            config.socket_path.unwrap().to_str().unwrap(),
            "/tmp/mux.sock"
        );
    }

    #[test]
    fn resolve_compression_mode_respects_explicit_overrides() {
        let missing = Path::new("/tmp/ft-nonexistent-socket-for-test.sock");
        assert_eq!(
            resolve_compression_mode(crate::config::VendoredCompressionMode::Always, missing),
            CompressionMode::Always
        );
        assert_eq!(
            resolve_compression_mode(crate::config::VendoredCompressionMode::Never, missing),
            CompressionMode::Never
        );
    }

    #[test]
    fn resolve_compression_mode_auto_local_socket_bypasses_compression() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let socket_path = tmp.path().join("mux.sock");
        let _listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind unix socket");

        assert_eq!(
            resolve_compression_mode(crate::config::VendoredCompressionMode::Auto, &socket_path),
            CompressionMode::Never
        );
    }

    #[test]
    fn resolve_socket_path_uses_explicit() {
        let config = DirectMuxClientConfig::default().with_socket_path("/tmp/explicit.sock");
        let path = resolve_socket_path(&config).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/explicit.sock"));
    }

    #[test]
    fn error_display_messages_are_descriptive() {
        let errors = [
            DirectMuxError::SocketPathMissing,
            DirectMuxError::SocketNotFound(PathBuf::from("/tmp/missing.sock")),
            DirectMuxError::ProxyUnsupported,
            DirectMuxError::ConnectTimeout(PathBuf::from("/tmp/sock")),
            DirectMuxError::ReadTimeout,
            DirectMuxError::WriteTimeout,
            DirectMuxError::Disconnected,
            DirectMuxError::FrameTooLarge { max_bytes: 1024 },
            DirectMuxError::SerialExhausted,
            DirectMuxError::Codec("bad frame".to_string()),
            DirectMuxError::RemoteError("denied".to_string()),
            DirectMuxError::BatchTimeout { timeout_ms: 5000 },
            DirectMuxError::UnexpectedResponse {
                expected: "Pong".to_string(),
                got: "Error".to_string(),
            },
            DirectMuxError::IncompatibleCodec {
                local: 2,
                remote: 1,
                remote_version: "old".to_string(),
            },
        ];
        for err in &errors {
            let msg = err.to_string();
            assert!(
                !msg.is_empty(),
                "Error message should not be empty: {err:?}"
            );
        }
    }

    #[test]
    fn decode_empty_buffer_returns_none() {
        let mut buf = Vec::new();
        let result = decode_from_buffer(&mut buf, 4096).expect("should not error");
        assert!(result.is_none());
    }

    #[test]
    fn decode_truncated_frame_does_not_panic() {
        let mut buf = Vec::new();
        let pdu = Pdu::Ping(codec::Ping {});
        pdu.encode(&mut buf, 1).expect("encode");
        // Feed truncated data — should either return None or a codec error, never panic
        for cut in [1, 2, 3, buf.len() / 2, buf.len() - 1] {
            if cut >= buf.len() {
                continue;
            }
            let mut truncated = buf[..cut].to_vec();
            let _ = decode_from_buffer(&mut truncated, 4096);
            // If it didn't panic, the test passes
        }
    }

    #[tokio::test]
    async fn connect_to_missing_socket_returns_error() {
        let config = DirectMuxClientConfig::default()
            .with_socket_path("/tmp/wa-test-nonexistent-socket-12345.sock");
        let err = DirectMuxClient::connect(config).await.unwrap_err();
        match err {
            DirectMuxError::SocketNotFound(_) => {}
            other => panic!("expected SocketNotFound, got: {other}"),
        }
    }

    #[test]
    fn decode_garbage_frame_returns_error_or_none() {
        // Intentionally invalid RPC frame: random bytes that don't form a valid PDU.
        let mut buf = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x10, 0xFF, 0xFF];
        let result = decode_from_buffer(&mut buf, 4096);
        // Should either error (codec parse failure) or return None (incomplete).
        // Must NOT panic.
        match result {
            Ok(None) => {} // incomplete frame
            Err(_) => {}   // codec error — expected for garbage
            Ok(Some(_)) => panic!("garbage bytes should never decode into a valid PDU"),
        }
    }

    #[test]
    fn decode_valid_then_garbage_tail() {
        // Encode a valid frame, then append garbage.
        let mut buf = Vec::new();
        let pdu = Pdu::Ping(codec::Ping {});
        pdu.encode(&mut buf, 7).expect("encode");
        buf.extend_from_slice(&[0xFF, 0xFE, 0xFD]);

        // First decode should succeed and consume the valid portion.
        let decoded = decode_from_buffer(&mut buf, 4096)
            .expect("should not error on valid prefix")
            .expect("should decode");
        assert_eq!(decoded.serial, 7);

        // Remaining buffer should be just the garbage tail.
        assert_eq!(buf.len(), 3, "buffer should contain only garbage tail");
        // Decoding the leftover garbage should not panic.
        let tail_result = decode_from_buffer(&mut buf, 4096);
        match tail_result {
            Ok(None) | Err(_) => {} // either is acceptable
            Ok(Some(_)) => panic!("garbage tail should not decode"),
        }
    }

    #[test]
    fn encode_decode_multiple_pdu_types() {
        // Round-trip test for various PDU types to exercise different code paths.
        let pdus: Vec<(Pdu, u64)> = vec![
            (Pdu::Ping(codec::Ping {}), 1),
            (Pdu::Pong(codec::Pong {}), 2),
            (Pdu::UnitResponse(UnitResponse {}), 3),
            (
                Pdu::ErrorResponse(codec::ErrorResponse {
                    reason: "test error".to_string(),
                }),
                4,
            ),
        ];

        for (pdu, serial) in &pdus {
            let mut buf = Vec::new();
            pdu.encode(&mut buf, *serial).expect("encode");

            let decoded = decode_from_buffer(&mut buf, 4096)
                .expect("should not error")
                .expect("should decode");
            assert_eq!(decoded.serial, *serial);
        }
    }

    #[tokio::test]
    async fn incompatible_codec_version_rejected() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("mux-incompat.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut read_buf = Vec::new();
            let mut temp = vec![0u8; 4096];
            let read = stream.read(&mut temp).await.expect("read");
            read_buf.extend_from_slice(&temp[..read]);
            if let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                // Respond with wrong codec version
                let response = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: CODEC_VERSION + 999,
                    version_string: "incompatible-wezterm".to_string(),
                    executable_path: PathBuf::from("/bin/wezterm"),
                    config_file_path: None,
                });
                let mut out = Vec::new();
                response.encode(&mut out, decoded.serial).expect("encode");
                stream.write_all(&out).await.expect("write");
            }
        });

        let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
        let err = DirectMuxClient::connect(config).await.unwrap_err();
        match err {
            DirectMuxError::IncompatibleCodec { local, remote, .. } => {
                assert_eq!(local, CODEC_VERSION);
                assert_eq!(remote, CODEC_VERSION + 999);
            }
            other => panic!("expected IncompatibleCodec, got: {other}"),
        }
    }

    // --- subscribe_pane_output / PaneDelta / SubscriptionConfig tests ---

    #[test]
    fn subscription_config_defaults_are_sane() {
        let cfg = SubscriptionConfig::default();
        assert_eq!(cfg.poll_interval, Duration::from_millis(100));
        assert_eq!(cfg.min_poll_interval, Duration::from_millis(20));
        assert_eq!(cfg.channel_capacity, 256);
        assert!(cfg.poll_interval >= cfg.min_poll_interval);
    }

    #[test]
    fn pane_delta_output_debug_format() {
        let delta = PaneDelta::Output {
            pane_id: 42,
            seqno: 7,
            title: "bash".to_string(),
            dirty_range_count: 3,
        };
        let dbg = format!("{delta:?}");
        assert!(dbg.contains("Output"));
        assert!(dbg.contains("42"));
        assert!(dbg.contains("bash"));
    }

    #[test]
    fn pane_delta_gap_debug_format() {
        let delta = PaneDelta::Gap {
            pane_id: 1,
            reason: "seqno jump".to_string(),
        };
        let dbg = format!("{delta:?}");
        assert!(dbg.contains("Gap"));
        assert!(dbg.contains("seqno jump"));
    }

    #[test]
    fn pane_delta_ended_debug_format() {
        let delta = PaneDelta::Ended {
            pane_id: 5,
            reason: "cancelled".to_string(),
        };
        let dbg = format!("{delta:?}");
        assert!(dbg.contains("Ended"));
        assert!(dbg.contains("cancelled"));
    }

    #[test]
    fn pane_delta_clone_eq() {
        let delta = PaneDelta::Output {
            pane_id: 10,
            seqno: 99,
            title: "zsh".to_string(),
            dirty_range_count: 1,
        };
        let cloned = delta.clone();
        // Clone should produce identical debug output
        assert_eq!(format!("{delta:?}"), format!("{cloned:?}"));
    }

    #[tokio::test]
    async fn subscription_cancel_stops_poller() {
        // Create a subscription with a mock socket that never responds.
        // The poller should shut down when cancelled via the handle.
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("cancel-test.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");

        // Server: accept, do codec handshake, then respond to GetPaneRenderChanges
        // with empty dirty_lines (no deltas to emit).
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut read_buf = Vec::new();
            loop {
                let mut temp = vec![0u8; 4096];
                let read = match stream.read(&mut temp).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                read_buf.extend_from_slice(&temp[..read]);
                while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "test".to_string(),
                                executable_path: PathBuf::from("/bin/wezterm"),
                                config_file_path: None,
                            })
                        }
                        Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                        Pdu::GetPaneRenderChanges(_) => {
                            // Return empty changes (seqno 0, no dirty lines)
                            Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                pane_id: 0,
                                mouse_grabbed: false,
                                cursor_position: mux::renderable::StableCursorPosition::default(),
                                dimensions: mux::renderable::RenderableDimensions {
                                    cols: 80,
                                    viewport_rows: 24,
                                    scrollback_rows: 0,
                                    physical_top: 0,
                                    scrollback_top: 0,
                                    dpi: 96,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                    reverse_video: false,
                                },
                                dirty_lines: Vec::new(),
                                title: "test".to_string(),
                                working_dir: None,
                                bonus_lines: Vec::new().into(),
                                input_serial: None,
                                seqno: 0,
                            })
                        }
                        _ => continue,
                    };
                    let mut out = Vec::new();
                    response.encode(&mut out, decoded.serial).expect("encode");
                    stream.write_all(&out).await.expect("write");
                }
            }
        });

        let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
        let client = DirectMuxClient::connect(config).await.expect("connect");

        let mut sub = subscribe_pane_output(
            client,
            0,
            SubscriptionConfig {
                poll_interval: Duration::from_millis(10),
                min_poll_interval: Duration::from_millis(5),
                channel_capacity: 8,
            },
        );

        // Give the poller time to start
        sleep(Duration::from_millis(50)).await;

        // Cancel and verify it terminates
        sub.cancel();

        // next() should return an Ended delta or None eventually
        let timeout = timeout(Duration::from_secs(2), sub.next()).await;
        match timeout {
            Ok(Some(PaneDelta::Ended { reason, .. })) => {
                assert!(reason.contains("cancelled"));
            }
            Ok(None) => {} // channel closed — also fine
            Ok(Some(other)) => {
                // Could get a stale delta before Ended; drain until Ended or None
                let mut found_end = false;
                let _ = other; // consume
                for _ in 0..10 {
                    match sub.next().await {
                        Some(PaneDelta::Ended { .. }) | None => {
                            found_end = true;
                            break;
                        }
                        _ => {}
                    }
                }
                assert!(found_end, "should eventually see Ended or channel close");
            }
            Err(_) => panic!("subscription did not terminate within timeout"),
        }
    }
}
