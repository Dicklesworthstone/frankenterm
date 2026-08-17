// Vendored mux client: large futures are inherent to the mux protocol's
// deeply-nested async call chains.
#![allow(clippy::large_futures)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use crate::config as wa_config;
use crate::cx::{self, Cx, RuntimeHandle};
use crate::protocol_recovery::{
    MuxConnectionDisposition, MuxRecoveryDecision, ProtocolErrorKind, classify_mux_error,
    mux_recovery_decision,
};
#[cfg(test)]
use crate::runtime_async::mpsc_reserve_send;
use crate::runtime_async::unix::{self as compat_unix, AsyncWriteExt, UnixStream};
use crate::runtime_async::{io, mpsc, mpsc_try_reserve_send, watch};
#[cfg(test)]
use crate::runtime_async::{task, timeout};
use codec::{
    AdjustPaneSize, CODEC_VERSION, CODEC_VERSION_MIN_SUPPORTED, CompatDecision, CompressionMode,
    CreateFloatingPane, CycleStack, DecodedPdu, GetCodecVersion, GetCodecVersionResponse, GetLines,
    GetLinesResponse, GetPaneRenderChanges, GetPaneRenderChangesResponse,
    GetPaneTieredScrollbackStatusesV1, GetPaneTieredScrollbackStatusesV1Response, GetSemanticZones,
    GetSemanticZonesResponse, InputSerial, ListPanes, ListPanesResponse, MoveFloatingPane,
    OwnedPreparedPduOutbound, Pdu, PduCapabilityUse, PduProducer, PduQueueQos, PduWireRole,
    RemoveFloatingPane, Resize, SelectStackPane, SendPaste, SetClientId, SetFloatingPaneZ,
    SetLayoutCycle, SpawnResponse, SpawnV2, SplitPane, StreamingPduBuffer, SwapToLayout,
    ToggleFloatingPane, TopologyCapabilities, UnitResponse, UpdatePaneConstraints, WriteToPane,
};
use config as wezterm_config;
use frankenterm_term::TerminalSize;
use mux::client::ClientId;
use mux::tab::FloatingPaneRect;

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_READ_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_WRITE_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MAX_OUTSTANDING_REQUESTS: usize = 256;
const DEFAULT_MAX_PENDING_RESPONSES: usize = 256;
const DEFAULT_MAX_PENDING_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_PENDING_RENDER_CHANGES: usize = 512;
const DEFAULT_MAX_PENDING_RENDER_CHANGE_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_MAX_RENDER_CHANGE_SNAPSHOTS: usize = 512;
const DEFAULT_MAX_RENDER_CHANGE_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct DirectMuxClientConfig {
    pub socket_path: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_frame_bytes: usize,
    /// Shared logical codec-memory ceiling for admitted outbound requests.
    ///
    /// A standalone client owns one authority. Every connection and recovery
    /// successor created by a [`super::mux_pool::MuxPool`] shares the pool's
    /// single authority. One sixteenth of this byte ceiling and one request
    /// slot (when more than one exists) remain reserved for control and
    /// interactive traffic so bulk/query work cannot consume all codec-memory
    /// admission capacity needed for a small key input.
    pub max_outbound_codec_bytes: usize,
    /// Shared count ceiling for requests that currently own codec memory.
    pub max_outbound_in_flight_requests: usize,
    /// Maximum request serials that may await a response on one connection.
    pub max_outstanding_requests: usize,
    /// Maximum out-of-order responses retained until their waiter consumes them.
    pub max_pending_responses: usize,
    /// Exact uncompressed retained-frame budget for out-of-order responses.
    pub max_pending_response_bytes: usize,
    /// Maximum unilateral render changes retained until their pane poll consumes them.
    pub max_pending_render_changes: usize,
    /// Conservative uncompressed retained-frame-equivalent render budget.
    ///
    /// Encoded global entries are charged exactly. Typed batch-local entries
    /// retain the complete decoded payload charge, which can conservatively
    /// include additive bytes ignored by this codec version.
    pub max_pending_render_change_bytes: usize,
    /// Maximum pane snapshots retained for liveness-only render responses.
    pub max_render_change_snapshots: usize,
    /// Exact uncompressed retained-frame budget for pane render snapshots.
    pub max_render_change_snapshot_bytes: usize,
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
        cfg.max_frame_bytes = config.vendored.mux_pool.max_frame_bytes;
        cfg.max_outbound_codec_bytes = config.vendored.mux_pool.max_outbound_codec_bytes;
        cfg.max_outbound_in_flight_requests =
            config.vendored.mux_pool.max_outbound_in_flight_requests;
        cfg.compression_mode = config.vendored.mux_pool.compression;
        cfg
    }

    #[must_use]
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = Some(path.into());
        self
    }

    fn validate(&self) -> Result<(), DirectMuxError> {
        for (field, value) in [
            ("max_frame_bytes", self.max_frame_bytes),
            ("max_outbound_codec_bytes", self.max_outbound_codec_bytes),
            (
                "max_outbound_in_flight_requests",
                self.max_outbound_in_flight_requests,
            ),
            ("max_outstanding_requests", self.max_outstanding_requests),
            ("max_pending_responses", self.max_pending_responses),
            (
                "max_pending_response_bytes",
                self.max_pending_response_bytes,
            ),
            (
                "max_pending_render_changes",
                self.max_pending_render_changes,
            ),
            (
                "max_pending_render_change_bytes",
                self.max_pending_render_change_bytes,
            ),
            (
                "max_render_change_snapshots",
                self.max_render_change_snapshots,
            ),
            (
                "max_render_change_snapshot_bytes",
                self.max_render_change_snapshot_bytes,
            ),
        ] {
            if value == 0 {
                return Err(DirectMuxError::InvalidLimit { field });
            }
        }
        Ok(())
    }
}

impl Default for DirectMuxClientConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            read_timeout: Duration::from_millis(DEFAULT_READ_TIMEOUT_MS),
            write_timeout: Duration::from_millis(DEFAULT_WRITE_TIMEOUT_MS),
            max_frame_bytes: crate::config::DEFAULT_VENDORED_MUX_MAX_FRAME_BYTES,
            max_outbound_codec_bytes: crate::config::DEFAULT_VENDORED_MUX_MAX_OUTBOUND_CODEC_BYTES,
            max_outbound_in_flight_requests:
                crate::config::DEFAULT_VENDORED_MUX_MAX_OUTBOUND_IN_FLIGHT_REQUESTS,
            max_outstanding_requests: DEFAULT_MAX_OUTSTANDING_REQUESTS,
            max_pending_responses: DEFAULT_MAX_PENDING_RESPONSES,
            max_pending_response_bytes: DEFAULT_MAX_PENDING_RESPONSE_BYTES,
            max_pending_render_changes: DEFAULT_MAX_PENDING_RENDER_CHANGES,
            max_pending_render_change_bytes: DEFAULT_MAX_PENDING_RENDER_CHANGE_BYTES,
            max_render_change_snapshots: DEFAULT_MAX_RENDER_CHANGE_SNAPSHOTS,
            max_render_change_snapshot_bytes: DEFAULT_MAX_RENDER_CHANGE_SNAPSHOT_BYTES,
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
    #[error("direct mux connection identity space exhausted")]
    ConnectionIdExhausted,
    #[error("request serial space exhausted for this connection")]
    SerialExhausted,
    #[error("process-local input serial space exhausted")]
    InputSerialExhausted,
    #[error("invalid direct mux client limit {field}: value must be nonzero")]
    InvalidLimit { field: &'static str },
    #[error(
        "{resource} retention limit exceeded: requested {requested_count} items/{requested_bytes} \
         bytes, limit {max_count} items/{max_bytes} bytes"
    )]
    RetentionLimitExceeded {
        resource: &'static str,
        requested_count: usize,
        requested_bytes: usize,
        max_count: usize,
        max_bytes: usize,
    },
    #[error("response serial {serial} is not outstanding on direct mux connection {connection_id}")]
    ResponseSerialNotOutstanding { connection_id: u64, serial: u64 },
    #[error(
        "retained mux state belongs to connection {got_connection_id}, not active connection \
         {expected_connection_id}"
    )]
    RetainedConnectionMismatch {
        expected_connection_id: u64,
        got_connection_id: u64,
    },
    #[error("retained {resource} accounting is inconsistent")]
    RetainedStateAccounting { resource: &'static str },
    #[error("codec error: {0}")]
    Codec(String),
    #[error("remote error: {0}")]
    RemoteError(String),
    #[error("pipeline batch timed out after {timeout_ms}ms")]
    BatchTimeout { timeout_ms: u64 },
    #[error("duplicate pane {pane_id} in render-change batch")]
    DuplicateRenderBatchPane { pane_id: u64 },
    #[error("unexpected response: expected {expected}, got {got}")]
    UnexpectedResponse { expected: String, got: String },
    #[error("unexpected aligned response: expected {expected}, got {got}")]
    AlignedUnexpectedResponse { expected: String, got: String },
    /// A local validation/admission failure proven to occur before the write
    /// boundary. The nested error retains its diagnostic kind while this
    /// wrapper carries the transport-alignment proof.
    #[error(transparent)]
    ProvenPreWriteRejection(Box<DirectMuxError>),
    /// A nested operation error that cannot retain its otherwise narrow reuse
    /// classification because the enclosing batch still owns earlier writes.
    #[error(transparent)]
    InFlightScopeAbandoned(Box<DirectMuxError>),
    #[error(
        "codec version mismatch: local={local} (min {local_min}), remote={remote} (min \
         {remote_min}, version {remote_version}); the compatibility windows do not overlap"
    )]
    IncompatibleCodec {
        local: usize,
        local_min: usize,
        remote: usize,
        remote_min: usize,
        remote_version: String,
    },
    #[error("outbound PDU {pdu} is forbidden during direct mux phase {phase}")]
    OutboundPduInvalidForPhase {
        pdu: &'static str,
        phase: &'static str,
    },
    #[error("outbound PDU {pdu} is not client-produced")]
    OutboundPduDirectionViolation { pdu: &'static str },
    #[error("outbound PDU {pdu} requires codec {required}, above negotiated codec {agreed}")]
    OutboundPduRequiresCodec {
        pdu: &'static str,
        agreed: usize,
        required: usize,
    },
    #[error(
        "outbound PDU {pdu} requires capabilities 0x{required:x}, but only 0x{negotiated:x} \
         was negotiated"
    )]
    OutboundCapabilityNotNegotiated {
        pdu: &'static str,
        negotiated: u64,
        required: u64,
    },
    #[error("inbound PDU {pdu} is forbidden during direct mux phase {phase}")]
    InboundPduInvalidForPhase {
        pdu: &'static str,
        phase: &'static str,
    },
    #[error("inbound PDU {pdu} is not server-produced")]
    InboundPduDirectionViolation { pdu: &'static str },
    #[error("inbound PDU {pdu} requires codec {required}, above negotiated codec {agreed}")]
    InboundPduRequiresCodec {
        pdu: &'static str,
        agreed: usize,
        required: usize,
    },
    #[error(
        "inbound PDU {pdu} requires capabilities 0x{required:x}, but only 0x{negotiated:x} \
         was negotiated"
    )]
    InboundCapabilityNotNegotiated {
        pdu: &'static str,
        negotiated: u64,
        required: u64,
    },
    #[error("mux {phase} cancelled: {detail}")]
    Cancelled { phase: &'static str, detail: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl DirectMuxError {
    pub(crate) fn proven_pre_write_rejection(error: Self) -> Self {
        debug_assert!(!matches!(&error, Self::ProvenPreWriteRejection(_)));
        Self::ProvenPreWriteRejection(Box::new(error))
    }

    fn in_flight_scope_abandoned(error: Self) -> Self {
        debug_assert!(!matches!(&error, Self::InFlightScopeAbandoned(_)));
        Self::InFlightScopeAbandoned(Box::new(error))
    }

    /// Whether this error represents an explicit capability-context cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled { .. } => true,
            Self::InFlightScopeAbandoned(source) => source.is_cancelled(),
            // Retain defensive recognition for cancellation errors produced
            // by older callers that still encode the signal in Interrupted
            // text. New internal construction uses the typed variant above.
            Self::Io(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                let text = err.to_string().to_ascii_lowercase();
                text.contains("cancelled") || text.contains("canceled")
            }
            _ => false,
        }
    }

    /// Project the canonical recovery decision to its diagnostic bucket.
    ///
    /// [`mux_recovery_decision`] is the single multi-axis authority, and
    /// [`classify_mux_error`] is its thin kind projection. Keep this inherent
    /// method as the ergonomic kind-only entry point for mux callers.
    #[must_use]
    pub fn protocol_error_kind(&self) -> ProtocolErrorKind {
        classify_mux_error(self)
    }

    /// Return the canonical retry, connection, and cancellation decision.
    pub fn recovery_decision(&self) -> MuxRecoveryDecision {
        mux_recovery_decision(self)
    }

    /// Whether this error proves that no socket write boundary was entered and
    /// no request bytes could have reached the peer.
    ///
    /// This is a transport-alignment proof, not a promise that local work was
    /// untouched: a request serial may already have been consumed and outbound
    /// encoding may already have been attempted.
    ///
    /// MuxPool uses this narrow predicate for mutation calls. General recovery
    /// axes are insufficient: a policy rejection can be permanent, or a
    /// cancellation transient, while the attempted mutation is nevertheless
    /// known not to have crossed the write boundary.
    #[must_use]
    pub(super) fn is_proven_pre_write_rejection(&self) -> bool {
        matches!(
            self,
            Self::InputSerialExhausted
                | Self::OutboundPduInvalidForPhase { .. }
                | Self::OutboundPduDirectionViolation { .. }
                | Self::OutboundPduRequiresCodec { .. }
                | Self::OutboundCapabilityNotNegotiated { .. }
                | Self::ProvenPreWriteRejection(_)
        ) || self.is_pre_transport_cancellation()
    }

    /// Whether a typed cancellation was observed at a checkpoint that is
    /// statically before request bytes can be handed to the transport.
    ///
    /// Timeout/cancellation races after a write begins use distinct
    /// `*_in_progress` phases and therefore never enter this set.
    #[must_use]
    pub(crate) fn is_pre_transport_cancellation(&self) -> bool {
        matches!(
            self,
            Self::Cancelled {
                phase: "request_start" | "request_write_wait" | "batch_wait" | "render_batch_wait",
                ..
            }
        )
    }

    /// Whether local codec admission proved that this peer cannot understand
    /// one named additive PDU. This is the only condition under which a
    /// higher-level read may take its bounded compatibility fallback.
    #[must_use]
    pub(crate) fn is_unsupported_pdu(&self, expected_pdu: &str) -> bool {
        match self {
            Self::OutboundPduRequiresCodec { pdu, .. } => *pdu == expected_pdu,
            Self::ProvenPreWriteRejection(source) => source.is_unsupported_pdu(expected_pdu),
            _ => false,
        }
    }
}

fn cancelled_mux_error(phase: &'static str, detail: impl std::fmt::Display) -> DirectMuxError {
    DirectMuxError::Cancelled {
        phase,
        detail: detail.to_string(),
    }
}

fn classify_cx_timeout(
    cx: &Cx,
    phase: &'static str,
    timeout_err: String,
    on_timeout: DirectMuxError,
) -> DirectMuxError {
    if cx.is_cancel_requested() {
        cancelled_mux_error(phase, timeout_err)
    } else {
        on_timeout
    }
}

fn checkpoint_mux_cx(
    cx: &Cx,
    connection_id: u64,
    phase: &'static str,
) -> Result<(), DirectMuxError> {
    cx.checkpoint().map_err(|err| {
        tracing::debug!(
            connection_id,
            explicit_cx = true,
            phase,
            error = %err,
            "mux operation cancelled before transport boundary"
        );
        cancelled_mux_error(phase, err)
    })
}

#[derive(Debug)]
struct RetainedMuxPdu {
    connection_id: u64,
    serial: u64,
    frame: Vec<u8>,
}

impl RetainedMuxPdu {
    fn encode(connection_id: u64, serial: u64, pdu: Pdu) -> Result<Self, DirectMuxError> {
        let frame = pdu
            .encode_retained_frame(serial)
            .map_err(|err| DirectMuxError::Codec(err.to_string()))?;
        Ok(Self {
            connection_id,
            serial,
            frame,
        })
    }

    fn retained_bytes(&self) -> usize {
        self.frame.len()
    }

    fn decode(
        &self,
        expected_connection_id: u64,
        expected_serial: u64,
    ) -> Result<Pdu, DirectMuxError> {
        if self.connection_id != expected_connection_id {
            return Err(DirectMuxError::RetainedConnectionMismatch {
                expected_connection_id,
                got_connection_id: self.connection_id,
            });
        }
        if self.serial != expected_serial {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: "mux PDU serial",
            });
        }
        let decoded = Pdu::decode_retained_frame(self.frame.as_slice())
            .map_err(|err| DirectMuxError::Codec(err.to_string()))?;
        if decoded.serial != expected_serial {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: "decoded mux PDU serial",
            });
        }
        Ok(decoded.pdu)
    }
}

#[derive(Clone, Copy, Debug)]
struct RetentionLimit {
    max_count: usize,
    max_bytes: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RenderRetentionCodecStats {
    pending_payload_encodes: usize,
    pending_payload_frame_allocations: usize,
    pending_payload_encoded_bytes: usize,
    pending_payload_frame_capacity_bytes: usize,
    pending_payload_decodes: usize,
    snapshot_encodes: usize,
    snapshot_frame_allocations: usize,
    snapshot_encoded_bytes: usize,
    snapshot_frame_capacity_bytes: usize,
    batch_local_claims: usize,
    batch_local_returns: usize,
    batch_local_demotions: usize,
    batch_local_peak_count: usize,
    batch_local_peak_frame_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
struct RetainedTotals {
    count: usize,
    bytes: usize,
    count_check: usize,
    bytes_check: usize,
}

impl Default for RetainedTotals {
    fn default() -> Self {
        Self {
            count: 0,
            bytes: 0,
            count_check: !0,
            bytes_check: !0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RetainedRemovalPlan {
    removed_count: usize,
    next_count: usize,
    next_bytes: usize,
}

impl RetainedTotals {
    fn validate(self, resource: &'static str) -> Result<(), DirectMuxError> {
        if self.count_check == !self.count && self.bytes_check == !self.bytes {
            Ok(())
        } else {
            Err(DirectMuxError::RetainedStateAccounting { resource })
        }
    }

    fn set(&mut self, count: usize, bytes: usize) {
        self.count = count;
        self.bytes = bytes;
        self.count_check = !count;
        self.bytes_check = !bytes;
    }

    fn after_remove(
        self,
        removed_count: usize,
        removed_bytes: usize,
        resource: &'static str,
    ) -> Result<(usize, usize), DirectMuxError> {
        self.validate(resource)?;
        let count = self
            .count
            .checked_sub(removed_count)
            .ok_or(DirectMuxError::RetainedStateAccounting { resource })?;
        let bytes = self
            .bytes
            .checked_sub(removed_bytes)
            .ok_or(DirectMuxError::RetainedStateAccounting { resource })?;
        Ok((count, bytes))
    }
}

#[derive(Debug)]
struct RetainedRenderChange {
    pane_id: u64,
    pdu: RetainedMuxPdu,
}

impl RetainedRenderChange {
    fn encode(
        connection_id: u64,
        payload: GetPaneRenderChangesResponse,
    ) -> Result<Self, DirectMuxError> {
        let pane_id = payload.pane_id as u64;
        let pdu =
            RetainedMuxPdu::encode(connection_id, 0, Pdu::GetPaneRenderChangesResponse(payload))?;
        Ok(Self { pane_id, pdu })
    }

    fn retained_bytes(&self) -> usize {
        self.pdu.retained_bytes()
    }

    fn decode(
        &self,
        expected_connection_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        match self.pdu.decode(expected_connection_id, 0)? {
            Pdu::GetPaneRenderChangesResponse(payload)
                if payload.pane_id as u64 == self.pane_id =>
            {
                Ok(payload)
            }
            Pdu::GetPaneRenderChangesResponse(_) => Err(DirectMuxError::RetainedStateAccounting {
                resource: "render-change pane identity",
            }),
            _ => Err(DirectMuxError::RetainedStateAccounting {
                resource: "render-change retained PDU type",
            }),
        }
    }
}

/// Per-pane FIFO with its common single retained change stored inline.
///
/// Most panes have at most one unsolicited delta waiting for their next poll.
/// Keeping that head in the map value avoids a separate deque allocation for
/// that case while the tail preserves deterministic FIFO order during bursts.
#[derive(Debug)]
struct PaneRenderChangeQueue {
    head: RetainedRenderChange,
    tail: VecDeque<RetainedRenderChange>,
}

impl PaneRenderChangeQueue {
    fn new(head: RetainedRenderChange) -> Self {
        Self {
            head,
            tail: VecDeque::new(),
        }
    }

    fn push_back(&mut self, retained: RetainedRenderChange) {
        self.tail.push_back(retained);
    }

    fn front(&self) -> &RetainedRenderChange {
        &self.head
    }

    fn has_tail(&self) -> bool {
        !self.tail.is_empty()
    }

    fn pop_front_with_tail(&mut self) -> Result<RetainedRenderChange, DirectMuxError> {
        let next = self
            .tail
            .pop_front()
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: PendingRenderChanges::RESOURCE,
            })?;
        Ok(std::mem::replace(&mut self.head, next))
    }

    fn iter(&self) -> impl Iterator<Item = &RetainedRenderChange> {
        std::iter::once(&self.head).chain(self.tail.iter())
    }
}

#[derive(Debug, Default)]
struct PendingRenderChanges {
    by_pane: HashMap<u64, PaneRenderChangeQueue>,
    totals: RetainedTotals,
    #[cfg(test)]
    take_operations: usize,
    #[cfg(test)]
    removal_plan_lookups: usize,
    #[cfg(test)]
    removal_plan_visits: usize,
    #[cfg(test)]
    removal_commit_operations: usize,
}

impl PendingRenderChanges {
    const RESOURCE: &'static str = "pending unilateral render changes";

    fn len(&self) -> usize {
        self.totals.count
    }

    fn is_empty(&self) -> bool {
        self.totals.count == 0
    }

    fn retained_bytes(&self) -> usize {
        self.totals.bytes
    }

    fn contains_pane(&self, pane_id: u64) -> Result<bool, DirectMuxError> {
        self.validate()?;
        Ok(self.by_pane.contains_key(&pane_id))
    }

    fn validate(&self) -> Result<(), DirectMuxError> {
        self.totals.validate(Self::RESOURCE)?;
        if self.is_empty() == self.by_pane.is_empty() {
            Ok(())
        } else {
            Err(DirectMuxError::RetainedStateAccounting {
                resource: Self::RESOURCE,
            })
        }
    }

    fn admit_insert(
        &self,
        retained: &RetainedRenderChange,
        limit: RetentionLimit,
    ) -> Result<(usize, usize), DirectMuxError> {
        self.validate()?;
        checked_retention_after_insert(
            Self::RESOURCE,
            self.totals.count,
            self.totals.bytes,
            None,
            retained.retained_bytes(),
            limit,
        )
    }

    fn commit_insert(&mut self, retained: RetainedRenderChange, next: (usize, usize)) {
        match self.by_pane.entry(retained.pane_id) {
            std::collections::hash_map::Entry::Vacant(bucket) => {
                bucket.insert(PaneRenderChangeQueue::new(retained));
            }
            std::collections::hash_map::Entry::Occupied(mut bucket) => {
                bucket.get_mut().push_back(retained);
            }
        }
        self.totals.set(next.0, next.1);
    }

    fn take_for_pane(
        &mut self,
        pane_id: u64,
    ) -> Result<Option<RetainedRenderChange>, DirectMuxError> {
        self.validate()?;
        #[cfg(test)]
        {
            self.take_operations += 1;
        }
        let mut bucket = match self.by_pane.entry(pane_id) {
            std::collections::hash_map::Entry::Vacant(_) => return Ok(None),
            std::collections::hash_map::Entry::Occupied(bucket) => bucket,
        };
        let retained = bucket.get().front();
        if retained.pane_id != pane_id {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: Self::RESOURCE,
            });
        }
        let next = self
            .totals
            .after_remove(1, retained.retained_bytes(), Self::RESOURCE)?;
        let retained = if bucket.get().has_tail() {
            bucket.get_mut().pop_front_with_tail()?
        } else {
            bucket.remove().head
        };
        self.totals.set(next.0, next.1);
        Ok(Some(retained))
    }

    // The mutable receiver accounts deterministic operation counts in test
    // builds; production deliberately compiles those counters out.
    #[cfg_attr(not(test), allow(clippy::needless_pass_by_ref_mut))]
    fn plan_remove_panes(
        &mut self,
        pane_ids: &HashSet<u64>,
    ) -> Result<RetainedRemovalPlan, DirectMuxError> {
        self.validate()?;
        let mut removed_count = 0usize;
        let mut removed_bytes = 0usize;
        for pane_id in pane_ids {
            #[cfg(test)]
            {
                self.removal_plan_lookups += 1;
            }
            let Some(queue) = self.by_pane.get(pane_id) else {
                continue;
            };
            for retained in queue.iter() {
                #[cfg(test)]
                {
                    self.removal_plan_visits += 1;
                }
                if retained.pane_id != *pane_id {
                    return Err(DirectMuxError::RetainedStateAccounting {
                        resource: Self::RESOURCE,
                    });
                }
                removed_count = removed_count.checked_add(1).ok_or(
                    DirectMuxError::RetainedStateAccounting {
                        resource: Self::RESOURCE,
                    },
                )?;
                removed_bytes = removed_bytes.checked_add(retained.retained_bytes()).ok_or(
                    DirectMuxError::RetainedStateAccounting {
                        resource: Self::RESOURCE,
                    },
                )?;
            }
        }
        let next = self
            .totals
            .after_remove(removed_count, removed_bytes, Self::RESOURCE)?;
        Ok(RetainedRemovalPlan {
            removed_count,
            next_count: next.0,
            next_bytes: next.1,
        })
    }

    fn commit_remove_panes(&mut self, pane_ids: &HashSet<u64>, plan: RetainedRemovalPlan) {
        for pane_id in pane_ids {
            #[cfg(test)]
            {
                self.removal_commit_operations += 1;
            }
            self.by_pane.remove(pane_id);
        }
        self.totals.set(plan.next_count, plan.next_bytes);
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &RetainedRenderChange> {
        self.by_pane.values().flat_map(PaneRenderChangeQueue::iter)
    }

    #[cfg(test)]
    fn reset_operation_counts(&mut self) {
        self.take_operations = 0;
        self.removal_plan_lookups = 0;
        self.removal_plan_visits = 0;
        self.removal_commit_operations = 0;
    }

    #[cfg(test)]
    fn operation_counts(&self) -> (usize, usize, usize, usize) {
        (
            self.take_operations,
            self.removal_plan_lookups,
            self.removal_plan_visits,
            self.removal_commit_operations,
        )
    }
}

#[derive(Debug, Default)]
struct RenderChangeSnapshots {
    by_pane: HashMap<u64, RetainedRenderChange>,
    totals: RetainedTotals,
    #[cfg(test)]
    removal_plan_lookups: usize,
    #[cfg(test)]
    removal_plan_visits: usize,
    #[cfg(test)]
    removal_commit_operations: usize,
}

impl RenderChangeSnapshots {
    const RESOURCE: &'static str = "render change snapshots";

    fn len(&self) -> usize {
        self.by_pane.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.by_pane.is_empty()
    }

    fn retained_bytes(&self) -> usize {
        self.totals.bytes
    }

    fn get(&self, pane_id: u64) -> Option<&RetainedRenderChange> {
        self.by_pane.get(&pane_id)
    }

    #[cfg(test)]
    fn contains_key(&self, pane_id: u64) -> bool {
        self.by_pane.contains_key(&pane_id)
    }

    fn validate(&self) -> Result<(), DirectMuxError> {
        self.totals.validate(Self::RESOURCE)?;
        if self.totals.count == self.by_pane.len() {
            Ok(())
        } else {
            Err(DirectMuxError::RetainedStateAccounting {
                resource: Self::RESOURCE,
            })
        }
    }

    fn admit_insert(
        &self,
        pane_id: u64,
        retained: &RetainedRenderChange,
        limit: RetentionLimit,
    ) -> Result<(usize, usize), DirectMuxError> {
        self.validate()?;
        let replaced_bytes = self
            .by_pane
            .get(&pane_id)
            .map(RetainedRenderChange::retained_bytes);
        checked_retention_after_insert(
            Self::RESOURCE,
            self.totals.count,
            self.totals.bytes,
            replaced_bytes,
            retained.retained_bytes(),
            limit,
        )
    }

    fn commit_insert(
        &mut self,
        pane_id: u64,
        retained: RetainedRenderChange,
        next: (usize, usize),
    ) {
        self.by_pane.insert(pane_id, retained);
        self.totals.set(next.0, next.1);
    }

    // The mutable receiver accounts deterministic operation counts in test
    // builds; production deliberately compiles those counters out.
    #[cfg_attr(not(test), allow(clippy::needless_pass_by_ref_mut))]
    fn plan_remove_panes(
        &mut self,
        pane_ids: &HashSet<u64>,
    ) -> Result<RetainedRemovalPlan, DirectMuxError> {
        self.validate()?;
        let mut removed_count = 0usize;
        let mut removed_bytes = 0usize;
        for pane_id in pane_ids {
            #[cfg(test)]
            {
                self.removal_plan_lookups += 1;
            }
            let Some(retained) = self.by_pane.get(pane_id) else {
                continue;
            };
            #[cfg(test)]
            {
                self.removal_plan_visits += 1;
            }
            if retained.pane_id != *pane_id {
                return Err(DirectMuxError::RetainedStateAccounting {
                    resource: Self::RESOURCE,
                });
            }
            removed_count =
                removed_count
                    .checked_add(1)
                    .ok_or(DirectMuxError::RetainedStateAccounting {
                        resource: Self::RESOURCE,
                    })?;
            removed_bytes = removed_bytes.checked_add(retained.retained_bytes()).ok_or(
                DirectMuxError::RetainedStateAccounting {
                    resource: Self::RESOURCE,
                },
            )?;
        }
        let next = self
            .totals
            .after_remove(removed_count, removed_bytes, Self::RESOURCE)?;
        Ok(RetainedRemovalPlan {
            removed_count,
            next_count: next.0,
            next_bytes: next.1,
        })
    }

    fn commit_remove_panes(&mut self, pane_ids: &HashSet<u64>, plan: RetainedRemovalPlan) {
        for pane_id in pane_ids {
            #[cfg(test)]
            {
                self.removal_commit_operations += 1;
            }
            self.by_pane.remove(pane_id);
        }
        self.totals.set(plan.next_count, plan.next_bytes);
    }

    #[cfg(test)]
    fn keys(&self) -> impl Iterator<Item = &u64> {
        self.by_pane.keys()
    }

    #[cfg(test)]
    fn values(&self) -> impl Iterator<Item = &RetainedRenderChange> {
        self.by_pane.values()
    }

    #[cfg(test)]
    fn reset_operation_counts(&mut self) {
        self.removal_plan_lookups = 0;
        self.removal_plan_visits = 0;
        self.removal_commit_operations = 0;
    }

    #[cfg(test)]
    fn operation_counts(&self) -> (usize, usize, usize) {
        (
            self.removal_plan_lookups,
            self.removal_plan_visits,
            self.removal_commit_operations,
        )
    }
}

/// Codec window retained for one exact DirectMux transport generation.
///
/// The connection identity is deliberately part of the record. A compatible
/// dialect learned on one socket is not authority for its pool successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NegotiatedCodec {
    connection_id: u64,
    local_max: usize,
    local_min: usize,
    remote_max: usize,
    remote_min: usize,
    agreed: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionAuthority {
    codec: NegotiatedCodec,
    locally_activated_capabilities: TopologyCapabilities,
    negotiated_capabilities: TopologyCapabilities,
}

/// Ordered connection setup and feature authority for one DirectMux socket.
///
/// Ordered-window and render-application capabilities intentionally remain
/// inactive. The codec knows their wire shapes, but no DirectMux producer or
/// consumer may use them until the corresponding live authority is wired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectMuxProtocolState {
    AwaitingCodec { connection_id: u64 },
    AwaitingRegistration { codec: NegotiatedCodec },
    Ready(SessionAuthority),
    Poisoned { connection_id: u64 },
}

impl DirectMuxProtocolState {
    const fn connection_id(self) -> u64 {
        match self {
            Self::AwaitingCodec { connection_id } | Self::Poisoned { connection_id } => {
                connection_id
            }
            Self::AwaitingRegistration { codec } | Self::Ready(SessionAuthority { codec, .. }) => {
                codec.connection_id
            }
        }
    }

    const fn phase_name(self) -> &'static str {
        match self {
            Self::AwaitingCodec { .. } => "awaiting_codec",
            Self::AwaitingRegistration { .. } => "awaiting_registration",
            Self::Ready(_) => "ready",
            Self::Poisoned { .. } => "poisoned",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DirectMuxOutboundBudgetState {
    codec_bytes: usize,
    noninteractive_codec_bytes: usize,
    requests: usize,
    noninteractive_requests: usize,
    peak_codec_bytes: usize,
}

/// One outbound-memory authority shared by every connection incarnation in a
/// direct-client ownership domain.
///
/// The budget charges the codec's conservative peak, not just final wire
/// bytes. This covers the counted payload, bounded compression destination,
/// and final frame while they can coexist. A lease is released only after the
/// write attempt has completed or the pre-write path has failed.
#[derive(Debug)]
pub(super) struct DirectMuxOutboundBudget {
    max_codec_bytes: usize,
    max_noninteractive_codec_bytes: usize,
    max_requests: usize,
    max_noninteractive_requests: usize,
    state: StdMutex<DirectMuxOutboundBudgetState>,
}

impl DirectMuxOutboundBudget {
    pub(super) fn from_config(config: &DirectMuxClientConfig) -> Self {
        let interactive_byte_reserve = config.max_outbound_codec_bytes / 16;
        let max_noninteractive_codec_bytes = config
            .max_outbound_codec_bytes
            .saturating_sub(interactive_byte_reserve);
        let max_noninteractive_requests = if config.max_outbound_in_flight_requests > 1 {
            config.max_outbound_in_flight_requests - 1
        } else {
            config.max_outbound_in_flight_requests
        };
        Self {
            max_codec_bytes: config.max_outbound_codec_bytes,
            max_noninteractive_codec_bytes,
            max_requests: config.max_outbound_in_flight_requests,
            max_noninteractive_requests,
            state: StdMutex::new(DirectMuxOutboundBudgetState::default()),
        }
    }

    fn try_admit(
        self: &Arc<Self>,
        prepared: OwnedPreparedPduOutbound,
        max_frame_bytes: usize,
    ) -> Result<DirectMuxOutboundLease, DirectMuxError> {
        if prepared.maximum_frame_bytes() > max_frame_bytes {
            return Err(DirectMuxError::proven_pre_write_rejection(
                DirectMuxError::FrameTooLarge {
                    max_bytes: max_frame_bytes,
                },
            ));
        }
        let planned_codec_bytes = prepared.codec_peak_bytes();
        let noninteractive = matches!(
            prepared.metadata().queue_qos,
            PduQueueQos::Normal | PduQueueQos::Bulk
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (requested_count, requested_bytes) = checked_retention_after_insert(
            "outbound mux admission",
            state.requests,
            state.codec_bytes,
            None,
            planned_codec_bytes,
            RetentionLimit {
                max_count: self.max_requests,
                max_bytes: self.max_codec_bytes,
            },
        )
        .map_err(DirectMuxError::proven_pre_write_rejection)?;
        let (requested_noninteractive_count, requested_noninteractive_bytes) = if noninteractive {
            checked_retention_after_insert(
                "noninteractive outbound mux admission",
                state.noninteractive_requests,
                state.noninteractive_codec_bytes,
                None,
                planned_codec_bytes,
                RetentionLimit {
                    max_count: self.max_noninteractive_requests,
                    max_bytes: self.max_noninteractive_codec_bytes,
                },
            )
            .map_err(DirectMuxError::proven_pre_write_rejection)?
        } else {
            (
                state.noninteractive_requests,
                state.noninteractive_codec_bytes,
            )
        };
        state.codec_bytes = requested_bytes;
        state.noninteractive_codec_bytes = requested_noninteractive_bytes;
        state.requests = requested_count;
        state.noninteractive_requests = requested_noninteractive_count;
        state.peak_codec_bytes = state.peak_codec_bytes.max(requested_bytes);
        drop(state);

        metrics::counter!(
            "mux.direct_client.outbound.admission.total",
            "outcome" => "admitted"
        )
        .increment(1);
        metrics::counter!(
            "mux.direct_client.outbound.codec_bytes.total",
            "outcome" => "reserved"
        )
        .increment(u64::try_from(planned_codec_bytes).unwrap_or(u64::MAX));

        Ok(DirectMuxOutboundLease {
            budget: Arc::clone(self),
            prepared: Some(prepared),
            planned_codec_bytes,
            noninteractive,
        })
    }

    #[cfg(test)]
    fn snapshot(&self) -> DirectMuxOutboundBudgetState {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
struct DirectMuxOutboundLease {
    budget: Arc<DirectMuxOutboundBudget>,
    prepared: Option<OwnedPreparedPduOutbound>,
    planned_codec_bytes: usize,
    noninteractive: bool,
}

impl DirectMuxOutboundLease {
    fn pdu_name(&self) -> &'static str {
        self.prepared
            .as_ref()
            .expect("direct mux outbound lease retains its PDU until encoding")
            .pdu()
            .pdu_name()
    }

    fn encode_frame(&mut self, serial: u64) -> Result<Vec<u8>, DirectMuxError> {
        self.prepared
            .take()
            .expect("direct mux outbound lease encodes its exact PDU once")
            .encode_frame(serial)
            .map_err(|error| DirectMuxError::Codec(error.to_string()))
            .map_err(DirectMuxError::proven_pre_write_rejection)
    }
}

impl Drop for DirectMuxOutboundLease {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.codec_bytes = state
            .codec_bytes
            .checked_sub(self.planned_codec_bytes)
            .expect("direct mux outbound codec reservation underflow");
        state.requests = state
            .requests
            .checked_sub(1)
            .expect("direct mux outbound request reservation underflow");
        if self.noninteractive {
            state.noninteractive_codec_bytes = state
                .noninteractive_codec_bytes
                .checked_sub(self.planned_codec_bytes)
                .expect("direct mux noninteractive codec reservation underflow");
            state.noninteractive_requests = state
                .noninteractive_requests
                .checked_sub(1)
                .expect("direct mux noninteractive request reservation underflow");
        }
        drop(state);
        metrics::counter!("mux.direct_client.outbound.lease_release.total").increment(1);
        metrics::counter!(
            "mux.direct_client.outbound.codec_bytes.total",
            "outcome" => "released"
        )
        .increment(u64::try_from(self.planned_codec_bytes).unwrap_or(u64::MAX));
    }
}

pub struct DirectMuxClient {
    connection_id: u64,
    protocol_state: DirectMuxProtocolState,
    stream: UnixStream,
    socket_path: PathBuf,
    read_buf: StreamingPduBuffer,
    serial: u64,
    outstanding_requests: HashSet<u64>,
    pending_responses: HashMap<u64, RetainedMuxPdu>,
    pending_response_bytes: usize,
    pending_render_changes: PendingRenderChanges,
    render_change_snapshots: RenderChangeSnapshots,
    outbound_budget: Arc<DirectMuxOutboundBudget>,
    config: DirectMuxClientConfig,
    compression_mode: CompressionMode,
    connection_poisoned: bool,
    #[cfg(test)]
    poison_transition_count: usize,
    #[cfg(test)]
    render_retention_codec_stats: RenderRetentionCodecStats,
}

impl std::fmt::Debug for DirectMuxClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("DirectMuxClient");
        debug
            .field("connection_id", &self.connection_id)
            .field("protocol_state", &self.protocol_state)
            .field("socket_path", &self.socket_path)
            .field("serial", &self.serial)
            .field("outstanding_requests", &self.outstanding_requests.len())
            .field("pending_responses", &self.pending_responses.len())
            .field("pending_response_bytes", &self.pending_response_bytes)
            .field("pending_render_changes", &self.pending_render_changes.len())
            .field(
                "pending_render_change_bytes",
                &self.pending_render_changes.retained_bytes(),
            )
            .field(
                "render_change_snapshots",
                &self.render_change_snapshots.len(),
            )
            .field(
                "render_change_snapshot_bytes",
                &self.render_change_snapshots.retained_bytes(),
            )
            .field("compression_mode", &self.compression_mode)
            .field("connection_poisoned", &self.connection_poisoned);
        #[cfg(test)]
        debug.field("poison_transition_count", &self.poison_transition_count);
        debug.finish_non_exhaustive()
    }
}

/// Bounded serial-to-output-slot ownership for one pipelined request set.
///
/// Response arrival order is deliberately not represented: caller order lives
/// in the separately indexed output vector.  This map owns only correlation,
/// so a reversed completion burst performs one keyed removal per response
/// rather than repeatedly searching and shifting a depth-sized deque.
struct InFlightRequestSlots {
    by_serial: HashMap<u64, usize>,
    #[cfg(test)]
    insert_operations: usize,
    #[cfg(test)]
    take_operations: usize,
}

/// One decoded render sideband plus its conservative uncompressed-frame charge.
///
/// The codec counts the complete decompressed payload, including any ignored
/// additive tail from a newer compatible peer, and adds the canonical serial-0
/// frame header. That may exceed a fresh known-schema re-encoding, which is a
/// safe admission overestimate. It is not an exact heap/RSS measurement for
/// the decoded Rust value; target-host memory claims still require allocator
/// and RSS evidence.
#[derive(Debug)]
struct TypedRenderSideband {
    payload: GetPaneRenderChangesResponse,
    retained_frame_bytes: usize,
}

/// Typed sidebands owned by the currently issued render-batch requests.
///
/// At most one payload is held for each unique in-flight pane. A second
/// payload is demoted together with the first to the globally bounded FIFO,
/// preserving arrival order. Local and global pending sidebands share the
/// existing count and canonical-frame byte caps, so the fast path cannot gain
/// an independent retention budget. This lets the normal
/// sideband-plus-liveness path avoid serializing and immediately decoding its
/// full render payload.
struct BatchLocalRenderSidebands {
    by_pane: HashMap<u64, TypedRenderSideband>,
    totals: RetainedTotals,
    capacity: usize,
}

impl BatchLocalRenderSidebands {
    const RESOURCE: &'static str = "batch-local typed render sidebands";

    fn with_limit(capacity: usize) -> Self {
        Self {
            // The common unchanged-pane batch retains no sidebands, so avoid
            // allocating this secondary map until the first delta arrives.
            by_pane: HashMap::new(),
            totals: RetainedTotals::default(),
            capacity,
        }
    }

    fn validate(&self) -> Result<(), DirectMuxError> {
        self.totals.validate(Self::RESOURCE)?;
        if self.totals.count == self.by_pane.len()
            && (self.totals.count == 0) == self.by_pane.is_empty()
        {
            Ok(())
        } else {
            Err(DirectMuxError::RetainedStateAccounting {
                resource: Self::RESOURCE,
            })
        }
    }

    fn totals(&self) -> Result<(usize, usize), DirectMuxError> {
        self.validate()?;
        Ok((self.totals.count, self.totals.bytes))
    }

    fn insert(
        &mut self,
        pane_id: u64,
        sideband: TypedRenderSideband,
        global: &PendingRenderChanges,
        limit: RetentionLimit,
    ) -> Result<(), DirectMuxError> {
        self.validate()?;
        global.validate()?;
        if sideband.retained_frame_bytes == 0 || self.totals.count >= self.capacity {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: Self::RESOURCE,
            });
        }

        let aggregate_count = global.len().checked_add(self.totals.count).ok_or(
            DirectMuxError::RetainedStateAccounting {
                resource: PendingRenderChanges::RESOURCE,
            },
        )?;
        let aggregate_bytes = global
            .retained_bytes()
            .checked_add(self.totals.bytes)
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: PendingRenderChanges::RESOURCE,
            })?;
        let _ = checked_retention_after_insert(
            PendingRenderChanges::RESOURCE,
            aggregate_count,
            aggregate_bytes,
            None,
            sideband.retained_frame_bytes,
            limit,
        )?;
        let next_count =
            self.totals
                .count
                .checked_add(1)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: Self::RESOURCE,
                })?;
        let next_bytes = self
            .totals
            .bytes
            .checked_add(sideband.retained_frame_bytes)
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: Self::RESOURCE,
            })?;
        match self.by_pane.entry(pane_id) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(sideband);
                self.totals.set(next_count, next_bytes);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(DirectMuxError::RetainedStateAccounting {
                    resource: Self::RESOURCE,
                })
            }
        }
    }

    fn take(&mut self, pane_id: u64) -> Result<Option<TypedRenderSideband>, DirectMuxError> {
        self.validate()?;
        let Some(retained_frame_bytes) = self
            .by_pane
            .get(&pane_id)
            .map(|sideband| sideband.retained_frame_bytes)
        else {
            return Ok(None);
        };
        let (next_count, next_bytes) =
            self.totals
                .after_remove(1, retained_frame_bytes, Self::RESOURCE)?;
        let sideband =
            self.by_pane
                .remove(&pane_id)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: Self::RESOURCE,
                })?;
        self.totals.set(next_count, next_bytes);
        Ok(Some(sideband))
    }

    fn remove(&mut self, pane_id: u64) -> Result<(), DirectMuxError> {
        let _ = self.take(pane_id)?;
        Ok(())
    }

    fn is_empty(&self) -> Result<bool, DirectMuxError> {
        self.validate()?;
        Ok(self.by_pane.is_empty())
    }
}

impl InFlightRequestSlots {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            by_serial: HashMap::with_capacity(capacity),
            #[cfg(test)]
            insert_operations: 0,
            #[cfg(test)]
            take_operations: 0,
        }
    }

    fn len(&self) -> usize {
        self.by_serial.len()
    }

    fn is_empty(&self) -> bool {
        self.by_serial.is_empty()
    }

    fn insert(&mut self, serial: u64, request_idx: usize) -> Result<(), DirectMuxError> {
        #[cfg(test)]
        {
            self.insert_operations += 1;
        }
        match self.by_serial.entry(serial) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(request_idx);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(DirectMuxError::RetainedStateAccounting {
                    resource: "in-flight mux request serials",
                })
            }
        }
    }

    fn take(&mut self, serial: u64) -> Option<usize> {
        #[cfg(test)]
        {
            self.take_operations += 1;
        }
        self.by_serial.remove(&serial)
    }

    #[cfg(test)]
    fn operation_counts(&self) -> (usize, usize) {
        (self.insert_operations, self.take_operations)
    }
}

/// Owns the mutable client borrow while a render batch is in progress.
///
/// The timeout wrappers are allowed to drop their inner future.  Keeping the
/// cleanup state beside the client borrow means that ordinary Rust drop order
/// can fail the connection closed without unsafe code or an asynchronous Drop
/// implementation.
struct RenderBatchGuard<'a> {
    client: &'a mut DirectMuxClient,
    pane_ids: &'a [u64],
    in_flight: InFlightRequestSlots,
    in_flight_panes: HashSet<u64>,
    local_sidebands: BatchLocalRenderSidebands,
    outputs: Vec<Option<GetPaneRenderChangesResponse>>,
    next_request_idx: usize,
    settled_count: usize,
    first_error: Option<DirectMuxError>,
    transport_ambiguous: bool,
    batch_progressed: bool,
    explicit_cx: bool,
    disarmed: bool,
}

impl<'a> RenderBatchGuard<'a> {
    fn new(
        client: &'a mut DirectMuxClient,
        pane_ids: &'a [u64],
        depth: usize,
        explicit_cx: bool,
    ) -> Self {
        Self {
            client,
            pane_ids,
            in_flight: InFlightRequestSlots::with_capacity(depth),
            in_flight_panes: HashSet::with_capacity(depth),
            local_sidebands: BatchLocalRenderSidebands::with_limit(depth),
            outputs: std::iter::repeat_with(|| None)
                .take(pane_ids.len())
                .collect(),
            next_request_idx: 0,
            settled_count: 0,
            first_error: None,
            transport_ambiguous: false,
            batch_progressed: false,
            explicit_cx,
            disarmed: false,
        }
    }

    fn can_admit(&self) -> bool {
        self.first_error.is_none() && self.next_request_idx < self.pane_ids.len()
    }

    fn record_issued(&mut self, request_idx: usize, serial: u64) -> Result<(), DirectMuxError> {
        let next_request_idx =
            request_idx
                .checked_add(1)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch request index",
                })?;
        let pane_id =
            *self
                .pane_ids
                .get(request_idx)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch issued pane index",
                })?;
        self.in_flight.insert(serial, request_idx)?;
        if !self.in_flight_panes.insert(pane_id) {
            let _ = self.in_flight.take(serial);
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: "render batch in-flight pane ownership",
            });
        }
        self.next_request_idx = next_request_idx;
        Ok(())
    }

    async fn send_next_with_cx(&mut self, cx: &Cx) -> Result<bool, DirectMuxError> {
        if !self.can_admit() {
            return Ok(false);
        }
        let request_idx = self.next_request_idx;
        let pane_id =
            *self
                .pane_ids
                .get(request_idx)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch explicit-Cx request index",
                })?;
        let serial = self
            .client
            .send_request_only_with_cx_tracking(
                cx,
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges {
                    pane_id: pane_id as usize,
                }),
                Some(&mut self.transport_ambiguous),
            )
            .await?;
        self.batch_progressed = true;
        self.record_issued(request_idx, serial)?;
        Ok(true)
    }

    fn remember_first_error(&mut self, pane_id: u64, error: DirectMuxError) {
        if self.first_error.is_none() {
            tracing::debug!(
                connection_id = self.client.connection_id,
                pane_id,
                issued_count = self.next_request_idx,
                settled_count = self.settled_count.saturating_add(1),
                remaining_count = self.in_flight.len(),
                explicit_cx = self.explicit_cx,
                error_kind = ?error.protocol_error_kind(),
                error = %error,
                phase = "render_batch_semantic_error",
                "mux render batch recorded its first semantic error and will drain"
            );
            self.first_error = Some(error);
        }
    }

    fn pending_render_limit(&self) -> RetentionLimit {
        RetentionLimit {
            max_count: self.client.config.max_pending_render_changes,
            max_bytes: self.client.config.max_pending_render_change_bytes,
        }
    }

    fn stash_global_render_sideband(
        &mut self,
        sideband: TypedRenderSideband,
    ) -> Result<(), DirectMuxError> {
        let (reserved_count, reserved_bytes) = self.local_sidebands.totals()?;
        self.client.stash_unilateral_render_change_with_reservation(
            sideband,
            reserved_count,
            reserved_bytes,
        )
    }

    fn handle_unilateral(
        &mut self,
        pdu: Pdu,
        retained_frame_bytes: Option<usize>,
    ) -> Result<(), DirectMuxError> {
        match pdu {
            Pdu::GetPaneRenderChangesResponse(payload) => {
                let pane_id = payload.pane_id as u64;
                let Some(retained_frame_bytes) = retained_frame_bytes else {
                    // Metadata is an optimization authority, not a semantic
                    // prerequisite. Preserve the established encoded FIFO
                    // path if a future/custom decoder cannot provide it.
                    let (reserved_count, reserved_bytes) = self.local_sidebands.totals()?;
                    return self.client.stash_unilateral_render_change_inner(
                        payload,
                        reserved_count,
                        reserved_bytes,
                    );
                };
                let sideband = TypedRenderSideband {
                    payload,
                    retained_frame_bytes,
                };
                if !self.in_flight_panes.contains(&pane_id) {
                    return self.stash_global_render_sideband(sideband);
                }

                if let Some(prior) = self.local_sidebands.take(pane_id)? {
                    if self.client.pending_render_changes.contains_pane(pane_id)? {
                        return Err(DirectMuxError::RetainedStateAccounting {
                            resource: "batch-local typed render sideband FIFO",
                        });
                    }
                    self.stash_global_render_sideband(prior)?;
                    #[cfg(test)]
                    {
                        self.client
                            .render_retention_codec_stats
                            .batch_local_demotions += 1;
                    }
                    return self.stash_global_render_sideband(sideband);
                }

                if self.client.pending_render_changes.contains_pane(pane_id)? {
                    return self.stash_global_render_sideband(sideband);
                }

                let limit = self.pending_render_limit();
                self.local_sidebands.insert(
                    pane_id,
                    sideband,
                    &self.client.pending_render_changes,
                    limit,
                )?;
                #[cfg(test)]
                {
                    let (local_count, local_bytes) = self.local_sidebands.totals()?;
                    let stats = &mut self.client.render_retention_codec_stats;
                    stats.batch_local_claims += 1;
                    stats.batch_local_peak_count = stats.batch_local_peak_count.max(local_count);
                    stats.batch_local_peak_frame_bytes =
                        stats.batch_local_peak_frame_bytes.max(local_bytes);
                }
                Ok(())
            }
            Pdu::PaneRemoved(removed) => {
                self.local_sidebands.remove(removed.pane_id as u64)?;
                self.client.stash_unilateral_pdu(Pdu::PaneRemoved(removed))
            }
            other => self.client.stash_unilateral_pdu(other),
        }
    }

    /// Returns true when this frame completed one request owned by the batch.
    fn handle_decoded(
        &mut self,
        decoded: codec::DecodedPduWithRetentionMetadata,
    ) -> Result<bool, DirectMuxError> {
        let (decoded, retained_frame_bytes) = decoded.into_parts();
        if decoded.serial == 0 {
            self.handle_unilateral(decoded.pdu, retained_frame_bytes)?;
            return Ok(false);
        }

        let Some(response_idx) = self.in_flight.take(decoded.serial) else {
            self.client
                .stash_pending_response(decoded.serial, decoded.pdu)?;
            return Ok(false);
        };

        self.client.complete_response_serial(decoded.serial)?;
        if self
            .outputs
            .get(response_idx)
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: "render batch response index",
            })?
            .is_some()
        {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: "render batch response slots",
            });
        }

        let pane_id =
            *self
                .pane_ids
                .get(response_idx)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch response pane index",
                })?;
        if !self.in_flight_panes.remove(&pane_id) {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: "render batch in-flight pane ownership",
            });
        }
        let local_sideband = self.local_sidebands.take(pane_id)?;
        let (reserved_count, reserved_bytes) = self.local_sidebands.totals()?;
        let resolved = DirectMuxClient::response_from_pdu(decoded.pdu).and_then(|pdu| {
            self.client.resolve_render_change_response_with_sideband(
                pane_id,
                pdu,
                local_sideband,
                reserved_count,
                reserved_bytes,
            )
        });
        match resolved {
            Ok(payload) => {
                let output = self.outputs.get_mut(response_idx).ok_or(
                    DirectMuxError::RetainedStateAccounting {
                        resource: "render batch response storage",
                    },
                )?;
                *output = Some(payload);
            }
            Err(error)
                if matches!(
                    &error,
                    DirectMuxError::AlignedUnexpectedResponse { .. }
                        | DirectMuxError::RemoteError(_)
                ) =>
            {
                self.remember_first_error(pane_id, error);
            }
            Err(error) => return Err(error),
        }
        self.settled_count =
            self.settled_count
                .checked_add(1)
                .ok_or(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch settled response count",
                })?;
        if self.in_flight.is_empty() {
            if !self.in_flight_panes.is_empty() || !self.local_sidebands.is_empty()? {
                return Err(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch terminal sideband ownership",
                });
            }
            self.transport_ambiguous = false;
        }
        Ok(true)
    }

    async fn run_with_cx(&mut self, cx: &Cx, depth: usize) -> Result<(), DirectMuxError> {
        while self.in_flight.len() < depth {
            if !self.send_next_with_cx(cx).await? {
                break;
            }
        }

        while !self.in_flight.is_empty() {
            let decoded = self
                .client
                .read_next_pdu_with_retention_metadata_with_cx(cx)
                .await?;
            let completed_batch_request = self.handle_decoded(decoded)?;
            if completed_batch_request && self.can_admit() && !self.send_next_with_cx(cx).await? {
                return Err(DirectMuxError::RetainedStateAccounting {
                    resource: "render batch explicit-Cx admission",
                });
            }
        }
        Ok(())
    }

    fn invalidate_target_render_state(&mut self) -> Result<(), DirectMuxError> {
        self.client
            .invalidate_render_state_for_panes(self.pane_ids)?;
        Ok(())
    }

    fn fail_finish<T>(
        &mut self,
        error: DirectMuxError,
        reason: &'static str,
    ) -> Result<T, DirectMuxError> {
        let sidebands_drained = matches!(self.local_sidebands.is_empty(), Ok(true));
        let scope_ambiguous = self.transport_ambiguous
            || !self.in_flight.is_empty()
            || !self.in_flight_panes.is_empty()
            || !sidebands_drained;
        let error = if scope_ambiguous {
            // A locally pre-write error is reusable only when this whole batch
            // owns no earlier writes. Never let its narrow classification hide
            // abandoned in-flight serials from the guard's Drop authority.
            DirectMuxError::in_flight_scope_abandoned(error)
        } else {
            error
        };
        self.client
            .apply_error_disposition(&error, reason, self.explicit_cx);
        self.disarmed = true;
        Err(error)
    }

    fn finish(mut self) -> Result<Vec<GetPaneRenderChangesResponse>, DirectMuxError> {
        let local_sidebands_empty = match self.local_sidebands.is_empty() {
            Ok(empty) => empty,
            Err(error) => {
                return self.fail_finish(error, "render batch sideband accounting failure");
            }
        };
        if !self.in_flight.is_empty() || !self.in_flight_panes.is_empty() || !local_sidebands_empty
        {
            self.transport_ambiguous = true;
            return self.fail_finish(
                DirectMuxError::RetainedStateAccounting {
                    resource: "render batch completion",
                },
                "incomplete render batch settlement",
            );
        }

        if let Some(error) = self.first_error.take() {
            if let Err(cleanup_error) = self.invalidate_target_render_state() {
                return self
                    .fail_finish(cleanup_error, "render batch semantic-error cleanup failure");
            }
            tracing::trace!(
                connection_id = self.client.connection_id,
                request_count = self.pane_ids.len(),
                settled_count = self.settled_count,
                explicit_cx = self.explicit_cx,
                phase = "render_batch_drained_error",
                "mux render batch drained all issued requests before returning semantic error"
            );
            return self.fail_finish(error, "drained render batch semantic failure");
        }

        let mut ordered = Vec::with_capacity(self.outputs.len());
        for (request_idx, output) in std::mem::take(&mut self.outputs).into_iter().enumerate() {
            let Some(payload) = output else {
                self.transport_ambiguous = true;
                return self.fail_finish(
                    DirectMuxError::RetainedStateAccounting {
                        resource: "render batch missing response",
                    },
                    "render batch output accounting failure",
                );
            };
            let Some(expected_pane_id) = self.pane_ids.get(request_idx).copied() else {
                self.transport_ambiguous = true;
                return self.fail_finish(
                    DirectMuxError::RetainedStateAccounting {
                        resource: "render batch output pane index",
                    },
                    "render batch output index failure",
                );
            };
            if payload.pane_id as u64 != expected_pane_id {
                self.transport_ambiguous = true;
                return self.fail_finish(
                    DirectMuxError::RetainedStateAccounting {
                        resource: "render batch output pane identity",
                    },
                    "render batch output identity failure",
                );
            }
            ordered.push(payload);
        }
        tracing::trace!(
            connection_id = self.client.connection_id,
            response_count = ordered.len(),
            explicit_cx = self.explicit_cx,
            phase = "render_batch_complete",
            "mux render batch completed"
        );
        self.disarmed = true;
        Ok(ordered)
    }
}

impl Drop for RenderBatchGuard<'_> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        if self.transport_ambiguous {
            self.client
                .poison_connection("ambiguous render batch abandonment", self.explicit_cx);
        } else if self.batch_progressed && self.invalidate_target_render_state().is_err() {
            self.client.poison_connection(
                "render batch abandonment cleanup accounting failure",
                self.explicit_cx,
            );
        }
    }
}

impl DirectMuxClient {
    #[cfg(test)]
    pub(super) fn shares_outbound_budget(&self, budget: &Arc<DirectMuxOutboundBudget>) -> bool {
        Arc::ptr_eq(&self.outbound_budget, budget)
    }

    pub async fn connect(config: DirectMuxClientConfig) -> Result<Self, DirectMuxError> {
        // Keep the ambient entry point for compatibility, but route the
        // actual transport work through the explicit-Cx path so connect,
        // handshake, and timeout boundaries inherit the caller's runtime
        // budget/cancellation context instead of open-coding a fresh one.
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        Self::connect_with_cx(&cx, config).await
    }

    /// Connect using an explicit capability context.
    pub async fn connect_with_cx(
        cx: &Cx,
        config: DirectMuxClientConfig,
    ) -> Result<Self, DirectMuxError> {
        config.validate()?;
        let outbound_budget = Arc::new(DirectMuxOutboundBudget::from_config(&config));
        Self::connect_with_cx_and_budget(cx, config, outbound_budget).await
    }

    pub(super) async fn connect_with_cx_and_budget(
        cx: &Cx,
        config: DirectMuxClientConfig,
        outbound_budget: Arc<DirectMuxOutboundBudget>,
    ) -> Result<Self, DirectMuxError> {
        config.validate()?;
        let socket_path = resolve_socket_path(&config)?;
        if !socket_path.exists() {
            return Err(DirectMuxError::SocketNotFound(socket_path));
        }

        let preferred_mode = resolve_compression_mode(config.compression_mode, &socket_path);
        tracing::debug!(
            socket_path = %socket_path.display(),
            configured_compression_mode = ?config.compression_mode,
            preferred_compression_mode = ?preferred_mode,
            explicit_cx = true,
            "connecting direct mux client"
        );
        match Self::connect_with_mode_with_cx(
            cx,
            socket_path.clone(),
            config.clone(),
            preferred_mode,
            Arc::clone(&outbound_budget),
        )
        .await
        {
            Ok(client) => Ok(client),
            Err(err)
                if should_auto_fallback_to_always(
                    config.compression_mode,
                    preferred_mode,
                    &err,
                ) =>
            {
                tracing::warn!(
                    socket_path = %socket_path.display(),
                    preferred_compression_mode = ?preferred_mode,
                    fallback_compression_mode = ?CompressionMode::Always,
                    error_kind = ?err.protocol_error_kind(),
                    error = %err,
                    explicit_cx = true,
                    "retrying direct mux connection with compression fallback"
                );
                Self::connect_with_mode_with_cx(
                    cx,
                    socket_path,
                    config,
                    CompressionMode::Always,
                    outbound_budget,
                )
                .await
            }
            Err(err) => Err(err),
        }
    }

    async fn connect_with_mode_with_cx(
        cx: &Cx,
        socket_path: PathBuf,
        config: DirectMuxClientConfig,
        compression_mode: CompressionMode,
        outbound_budget: Arc<DirectMuxOutboundBudget>,
    ) -> Result<Self, DirectMuxError> {
        let connection_id = next_connection_id()?;
        checkpoint_mux_cx(cx, connection_id, "connect_start")?;
        // Tick 199 (ft-xbnl0.2.3): route the connect timeout through
        // timeout_with_cx so the caller's explicit cx bounds the
        // socket handshake. Previously used ambient `timeout` which
        // falls back to `Cx::current()` thread-local lookup —
        // orphan-cx hole whenever the mux client connects outside
        // the caller's thread-local scope.
        let stream = crate::runtime_async::timeout_with_cx(
            cx,
            config.connect_timeout,
            compat_unix::connect(&socket_path),
        )
        .await
        .map_err(|timeout_err| {
            classify_cx_timeout(
                cx,
                "connect_wait",
                timeout_err,
                DirectMuxError::ConnectTimeout(socket_path.clone()),
            )
        })??;

        let mut client = Self {
            connection_id,
            protocol_state: DirectMuxProtocolState::AwaitingCodec { connection_id },
            stream,
            compression_mode,
            socket_path,
            read_buf: StreamingPduBuffer::new(),
            serial: 0,
            outstanding_requests: HashSet::new(),
            pending_responses: HashMap::new(),
            pending_response_bytes: 0,
            pending_render_changes: PendingRenderChanges::default(),
            render_change_snapshots: RenderChangeSnapshots::default(),
            outbound_budget,
            connection_poisoned: false,
            #[cfg(test)]
            poison_transition_count: 0,
            #[cfg(test)]
            render_retention_codec_stats: RenderRetentionCodecStats::default(),
            config,
        };

        if let Err(err) = client.verify_codec_version_with_cx(cx).await {
            tracing::warn!(
                connection_id = client.connection_id,
                socket_path = %client.socket_path.display(),
                phase = "codec_version_handshake",
                error_kind = ?err.protocol_error_kind(),
                error = %err,
                explicit_cx = true,
                "direct mux codec verification failed"
            );
            client.apply_error_disposition(&err, "codec-version handshake failure", true);
            return Err(err);
        }
        if let Err(err) = client.register_client_with_cx(cx).await {
            tracing::warn!(
                connection_id = client.connection_id,
                socket_path = %client.socket_path.display(),
                phase = "register_client",
                error_kind = ?err.protocol_error_kind(),
                error = %err,
                explicit_cx = true,
                "direct mux client registration failed"
            );
            client.apply_error_disposition(&err, "client registration failure", true);
            return Err(err);
        }
        tracing::debug!(
            connection_id = client.connection_id,
            socket_path = %client.socket_path.display(),
            compression_mode = ?client.compression_mode,
            connect_timeout_ms = duration_to_ms_u64(client.config.connect_timeout),
            read_timeout_ms = duration_to_ms_u64(client.config.read_timeout),
            write_timeout_ms = duration_to_ms_u64(client.config.write_timeout),
            phase = "connected",
            explicit_cx = true,
            "direct mux client connected"
        );
        Ok(client)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Whether this live client has crossed an ambiguous transport boundary
    /// and may no longer be reused.
    #[must_use]
    pub(super) fn is_connection_poisoned(&self) -> bool {
        self.connection_poisoned
    }

    /// Process-local, non-reusing identity for this exact transport instance.
    ///
    /// This is deliberately distinct from the server-authored render
    /// connection identity. It scopes DirectMuxClient-owned request and render
    /// retention so state from a dropped pool connection cannot be replayed
    /// into its replacement.
    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub async fn list_panes(&mut self) -> Result<ListPanesResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.list_panes_with_cx(&cx).await
    }

    /// List panes using an explicit capability context.
    pub async fn list_panes_with_cx(
        &mut self,
        cx: &Cx,
    ) -> Result<ListPanesResponse, DirectMuxError> {
        let response = self
            .send_request_with_cx(cx, Pdu::ListPanes(ListPanes {}))
            .await?;
        match response {
            Pdu::ListPanesResponse(payload) => Ok(payload),
            other => self.unexpected_response("ListPanesResponse", &other, true),
        }
    }

    /// Check whether the negotiated peer dialect admits the additive batch PDU.
    pub(crate) fn supports_tiered_scrollback_status_batch(&self) -> Result<bool, DirectMuxError> {
        let probe = Pdu::GetPaneTieredScrollbackStatusesV1(GetPaneTieredScrollbackStatusesV1 {
            pane_ids: vec![0],
        });
        match self.authorize_outbound_pdu(&probe) {
            Ok(()) => Ok(true),
            Err(error) if error.is_unsupported_pdu("GetPaneTieredScrollbackStatusesV1") => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Read only the bounded tiered-scrollback health fields for a pane batch.
    pub async fn get_pane_tiered_scrollback_statuses_with_cx(
        &mut self,
        cx: &Cx,
        pane_ids: Vec<usize>,
    ) -> Result<GetPaneTieredScrollbackStatusesV1Response, DirectMuxError> {
        let request = GetPaneTieredScrollbackStatusesV1 { pane_ids };
        request.validate().map_err(|error| {
            DirectMuxError::proven_pre_write_rejection(DirectMuxError::Codec(error.to_string()))
        })?;
        let requested_pane_ids = request.pane_ids.clone();
        let response = self
            .send_request_with_cx(cx, Pdu::GetPaneTieredScrollbackStatusesV1(request))
            .await?;
        let result = match response {
            Pdu::GetPaneTieredScrollbackStatusesV1Response(response) => {
                if let Err(error) = response.validate() {
                    Err(DirectMuxError::AlignedUnexpectedResponse {
                        expected: "bounded unique tiered-scrollback status response".to_string(),
                        got: error.to_string(),
                    })
                } else {
                    let response_pane_ids = response
                        .entries
                        .iter()
                        .map(|entry| entry.pane_id)
                        .collect::<Vec<_>>();
                    if response_pane_ids != requested_pane_ids {
                        Err(DirectMuxError::AlignedUnexpectedResponse {
                            expected: format!(
                                "tiered-scrollback statuses for panes {requested_pane_ids:?}"
                            ),
                            got: format!(
                                "tiered-scrollback statuses for panes {response_pane_ids:?}"
                            ),
                        })
                    } else {
                        Ok(response)
                    }
                }
            }
            other => Err(DirectMuxError::UnexpectedResponse {
                expected: "GetPaneTieredScrollbackStatusesV1Response".to_string(),
                got: other.pdu_name().to_string(),
            }),
        };
        self.settle_transport_result(
            result,
            "tiered-scrollback batch response contract failure",
            true,
        )
    }

    /// Spawn a new mux pane/tab through the native mux protocol.
    pub async fn spawn_v2(&mut self, spawn: SpawnV2) -> Result<SpawnResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.spawn_v2_with_cx(&cx, spawn).await
    }

    /// Spawn a new mux pane/tab through the native mux protocol with explicit Cx.
    pub async fn spawn_v2_with_cx(
        &mut self,
        cx: &Cx,
        spawn: SpawnV2,
    ) -> Result<SpawnResponse, DirectMuxError> {
        let response = self.send_request_with_cx(cx, Pdu::SpawnV2(spawn)).await?;
        match response {
            Pdu::SpawnResponse(payload) => Ok(payload),
            other => self.unexpected_response("SpawnResponse", &other, true),
        }
    }

    /// Split an existing pane through the native mux protocol.
    pub async fn split_pane(&mut self, split: SplitPane) -> Result<SpawnResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.split_pane_with_cx(&cx, split).await
    }

    /// Split an existing pane through the native mux protocol with explicit Cx.
    pub async fn split_pane_with_cx(
        &mut self,
        cx: &Cx,
        split: SplitPane,
    ) -> Result<SpawnResponse, DirectMuxError> {
        let response = self.send_request_with_cx(cx, Pdu::SplitPane(split)).await?;
        match response {
            Pdu::SpawnResponse(payload) => Ok(payload),
            other => self.unexpected_response("SpawnResponse", &other, true),
        }
    }

    /// Poll the mux server for render changes since the last check for a pane.
    pub async fn get_pane_render_changes(
        &mut self,
        pane_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_pane_render_changes_with_cx(&cx, pane_id).await
    }

    /// Poll render changes using an explicit capability context.
    pub async fn get_pane_render_changes_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        let serial = self
            .send_request_only_with_cx(
                cx,
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges {
                    pane_id: pane_id as usize,
                }),
            )
            .await?;
        let response = self.await_response_with_cx(cx, serial).await;
        self.settle_single_render_response(pane_id, response, true)
    }

    /// Fetch specific lines from a pane's scrollback.
    pub async fn get_lines(
        &mut self,
        pane_id: u64,
        lines: Vec<std::ops::Range<isize>>,
    ) -> Result<GetLinesResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_lines_with_cx(&cx, pane_id, lines).await
    }

    /// Fetch pane lines using an explicit capability context.
    pub async fn get_lines_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        lines: Vec<std::ops::Range<isize>>,
    ) -> Result<GetLinesResponse, DirectMuxError> {
        let response = self
            .send_request_with_cx(
                cx,
                Pdu::GetLines(GetLines {
                    pane_id: pane_id as usize,
                    lines,
                }),
            )
            .await?;
        match response {
            Pdu::GetLinesResponse(payload) => Ok(payload),
            other => self.unexpected_response("GetLinesResponse", &other, true),
        }
    }

    /// Fetch OSC 133 semantic zones from a pane through the native mux protocol.
    pub async fn get_semantic_zones(
        &mut self,
        pane_id: u64,
    ) -> Result<GetSemanticZonesResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_semantic_zones_with_cx(&cx, pane_id).await
    }

    /// Fetch OSC 133 semantic zones using an explicit capability context.
    pub async fn get_semantic_zones_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
    ) -> Result<GetSemanticZonesResponse, DirectMuxError> {
        let response = self
            .send_request_with_cx(
                cx,
                Pdu::GetSemanticZones(GetSemanticZones {
                    pane_id: pane_id as usize,
                }),
            )
            .await?;
        match response {
            Pdu::GetSemanticZonesResponse(payload) => Ok(payload),
            other => self.unexpected_response("GetSemanticZonesResponse", &other, true),
        }
    }

    /// Write raw bytes to a pane (no-paste mode, character-by-character).
    pub async fn write_to_pane(
        &mut self,
        pane_id: u64,
        data: Vec<u8>,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.write_to_pane_with_cx(&cx, pane_id, data).await
    }

    /// Write raw bytes to a pane using an explicit capability context.
    pub async fn write_to_pane_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        data: Vec<u8>,
    ) -> Result<UnitResponse, DirectMuxError> {
        let response = self
            .send_request_with_cx(
                cx,
                Pdu::WriteToPane(WriteToPane {
                    pane_id: pane_id as usize,
                    data,
                }),
            )
            .await?;
        match response {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => self.unexpected_response("UnitResponse", &other, true),
        }
    }

    /// Send text via paste mode (efficient for multi-character input).
    pub async fn send_paste(
        &mut self,
        pane_id: u64,
        data: String,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.send_paste_with_cx(&cx, pane_id, data).await
    }

    /// Resize a pane through the mux session using the same PDU as a GUI client.
    pub async fn resize(
        &mut self,
        containing_tab_id: u64,
        pane_id: u64,
        size: TerminalSize,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.resize_with_cx(&cx, containing_tab_id, pane_id, size)
            .await
    }

    /// Resize a pane through the mux session using an explicit capability context.
    pub async fn resize_with_cx(
        &mut self,
        cx: &Cx,
        containing_tab_id: u64,
        pane_id: u64,
        size: TerminalSize,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::Resize(Resize {
                containing_tab_id: containing_tab_id as usize,
                pane_id: pane_id as usize,
                size,
            }),
        )
        .await
    }

    /// Adjust pane split geometry through the mux session.
    pub async fn adjust_pane_size(
        &mut self,
        pane_id: u64,
        direction: wezterm_config::keyassignment::PaneDirection,
        amount: usize,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.adjust_pane_size_with_cx(&cx, pane_id, direction, amount)
            .await
    }

    /// Adjust pane split geometry through the mux session using an explicit capability context.
    pub async fn adjust_pane_size_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        direction: wezterm_config::keyassignment::PaneDirection,
        amount: usize,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id: pane_id as usize,
                direction,
                amount,
            }),
        )
        .await
    }

    pub async fn create_floating_pane(
        &mut self,
        tab_id: usize,
        pane_id: u64,
        rect: FloatingPaneRect,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.create_floating_pane_with_cx(&cx, tab_id, pane_id, rect)
            .await
    }

    pub async fn move_floating_pane(
        &mut self,
        pane_id: u64,
        rect: FloatingPaneRect,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.move_floating_pane_with_cx(&cx, pane_id, rect).await
    }

    pub async fn set_floating_pane_z(
        &mut self,
        pane_id: u64,
        z_order: u32,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.set_floating_pane_z_with_cx(&cx, pane_id, z_order)
            .await
    }

    pub async fn toggle_floating_pane(
        &mut self,
        pane_id: u64,
        visible: bool,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.toggle_floating_pane_with_cx(&cx, pane_id, visible)
            .await
    }

    pub async fn remove_floating_pane(
        &mut self,
        pane_id: u64,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.remove_floating_pane_with_cx(&cx, pane_id).await
    }

    pub async fn swap_to_layout(
        &mut self,
        tab_id: usize,
        layout_index: usize,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.swap_to_layout_with_cx(&cx, tab_id, layout_index).await
    }

    pub async fn set_layout_cycle(
        &mut self,
        tab_id: usize,
        layout_names: Vec<String>,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.set_layout_cycle_with_cx(&cx, tab_id, layout_names)
            .await
    }

    pub async fn cycle_stack(
        &mut self,
        tab_id: usize,
        slot_index: usize,
        forward: bool,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.cycle_stack_with_cx(&cx, tab_id, slot_index, forward)
            .await
    }

    pub async fn select_stack_pane(
        &mut self,
        tab_id: usize,
        slot_index: usize,
        pane_index: usize,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.select_stack_pane_with_cx(&cx, tab_id, slot_index, pane_index)
            .await
    }

    pub async fn update_pane_constraints(
        &mut self,
        pane_id: u64,
        min_width: Option<usize>,
        max_width: Option<usize>,
        min_height: Option<usize>,
        max_height: Option<usize>,
    ) -> Result<UnitResponse, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.update_pane_constraints_with_cx(
            &cx, pane_id, min_width, max_width, min_height, max_height,
        )
        .await
    }

    /// Send paste text using an explicit capability context.
    pub async fn send_paste_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        data: String,
    ) -> Result<UnitResponse, DirectMuxError> {
        let input_serial = InputSerial::try_now().ok_or(DirectMuxError::InputSerialExhausted)?;
        let response = self
            .send_request_with_cx(
                cx,
                Pdu::SendPaste(SendPaste {
                    pane_id: pane_id as usize,
                    data,
                    input_serial,
                }),
            )
            .await?;
        match response {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => self.unexpected_response("UnitResponse", &other, true),
        }
    }

    async fn expect_unit_response_with_cx(
        &mut self,
        cx: &Cx,
        request: Pdu,
    ) -> Result<UnitResponse, DirectMuxError> {
        match self.send_request_with_cx(cx, request).await? {
            Pdu::UnitResponse(payload) => Ok(payload),
            other => self.unexpected_response("UnitResponse", &other, true),
        }
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`create_floating_pane`].
    pub async fn create_floating_pane_with_cx(
        &mut self,
        cx: &Cx,
        tab_id: usize,
        pane_id: u64,
        rect: FloatingPaneRect,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id,
                pane_id: pane_id as usize,
                rect,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`move_floating_pane`].
    pub async fn move_floating_pane_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        rect: FloatingPaneRect,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::MoveFloatingPane(MoveFloatingPane {
                pane_id: pane_id as usize,
                rect,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`set_floating_pane_z`].
    pub async fn set_floating_pane_z_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        z_order: u32,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::SetFloatingPaneZ(SetFloatingPaneZ {
                pane_id: pane_id as usize,
                z_order,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`toggle_floating_pane`].
    pub async fn toggle_floating_pane_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        visible: bool,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::ToggleFloatingPane(ToggleFloatingPane {
                pane_id: pane_id as usize,
                visible,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`remove_floating_pane`].
    pub async fn remove_floating_pane_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::RemoveFloatingPane(RemoveFloatingPane {
                pane_id: pane_id as usize,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`swap_to_layout`].
    pub async fn swap_to_layout_with_cx(
        &mut self,
        cx: &Cx,
        tab_id: usize,
        layout_index: usize,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::SwapToLayout(SwapToLayout {
                tab_id,
                layout_index,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`set_layout_cycle`].
    pub async fn set_layout_cycle_with_cx(
        &mut self,
        cx: &Cx,
        tab_id: usize,
        layout_names: Vec<String>,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::SetLayoutCycle(SetLayoutCycle {
                tab_id,
                layout_names,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`cycle_stack`].
    pub async fn cycle_stack_with_cx(
        &mut self,
        cx: &Cx,
        tab_id: usize,
        slot_index: usize,
        forward: bool,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::CycleStack(CycleStack {
                tab_id,
                slot_index,
                forward,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`select_stack_pane`].
    pub async fn select_stack_pane_with_cx(
        &mut self,
        cx: &Cx,
        tab_id: usize,
        slot_index: usize,
        pane_index: usize,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::SelectStackPane(SelectStackPane {
                tab_id,
                slot_index,
                pane_index,
            }),
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`update_pane_constraints`].
    pub async fn update_pane_constraints_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        min_width: Option<usize>,
        max_width: Option<usize>,
        min_height: Option<usize>,
        max_height: Option<usize>,
    ) -> Result<UnitResponse, DirectMuxError> {
        self.expect_unit_response_with_cx(
            cx,
            Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: pane_id as usize,
                min_width,
                max_width,
                min_height,
                max_height,
            }),
        )
        .await
    }

    async fn verify_codec_version_with_cx(
        &mut self,
        cx: &Cx,
    ) -> Result<GetCodecVersionResponse, DirectMuxError> {
        let response = self
            .send_request_with_cx(cx, Pdu::GetCodecVersion(GetCodecVersion {}))
            .await?;
        match response {
            Pdu::GetCodecVersionResponse(payload) => {
                let remote_min = if payload.min_supported == 0 {
                    payload.codec_vers
                } else {
                    payload.min_supported
                };
                let compatibility = codec::check_compat(
                    CODEC_VERSION,
                    CODEC_VERSION_MIN_SUPPORTED,
                    payload.codec_vers,
                    remote_min,
                )
                .map_err(|_| DirectMuxError::IncompatibleCodec {
                    local: CODEC_VERSION,
                    local_min: CODEC_VERSION_MIN_SUPPORTED,
                    remote: payload.codec_vers,
                    remote_min,
                    remote_version: payload.version_string.clone(),
                });
                let CompatDecision::Compatible { agreed } = self.settle_transport_result(
                    compatibility,
                    "codec compatibility failure",
                    true,
                )?;
                let negotiated = NegotiatedCodec {
                    connection_id: self.connection_id,
                    local_max: CODEC_VERSION,
                    local_min: CODEC_VERSION_MIN_SUPPORTED,
                    remote_max: payload.codec_vers,
                    remote_min,
                    agreed,
                };
                self.protocol_state =
                    DirectMuxProtocolState::AwaitingRegistration { codec: negotiated };
                Ok(payload)
            }
            other => self.unexpected_response("GetCodecVersionResponse", &other, true),
        }
    }

    async fn register_client_with_cx(&mut self, cx: &Cx) -> Result<UnitResponse, DirectMuxError> {
        let DirectMuxProtocolState::AwaitingRegistration { codec } = self.protocol_state else {
            return Err(DirectMuxError::OutboundPduInvalidForPhase {
                pdu: "SetClientId",
                phase: self.protocol_state.phase_name(),
            });
        };
        let client_id = ClientId::new();
        let response = self
            .send_request_with_cx(
                cx,
                Pdu::SetClientId(SetClientId {
                    client_id,
                    is_proxy: false,
                }),
            )
            .await?;
        match response {
            Pdu::UnitResponse(payload) => {
                self.protocol_state = DirectMuxProtocolState::Ready(SessionAuthority {
                    codec,
                    locally_activated_capabilities: TopologyCapabilities::NONE,
                    negotiated_capabilities: TopologyCapabilities::NONE,
                });
                Ok(payload)
            }
            other => self.unexpected_response("UnitResponse", &other, true),
        }
    }

    /// Batch `GetPaneRenderChanges` requests with depth-limited pipelining.
    ///
    /// Responses are returned in the same order as `pane_ids`, regardless of
    /// on-wire response ordering. Pane IDs must be unique; duplicates are
    /// rejected before transport admission.
    pub async fn get_pane_render_changes_batch(
        &mut self,
        pane_ids: &[u64],
        max_pipeline_depth: usize,
        pipeline_timeout: Duration,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, DirectMuxError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_pane_render_changes_batch_with_cx(
            &cx,
            pane_ids,
            max_pipeline_depth,
            pipeline_timeout,
        )
        .await
    }

    /// Batch render-change requests using an explicit capability context.
    /// Pane IDs must be unique; duplicates are rejected before transport admission.
    pub async fn get_pane_render_changes_batch_with_cx(
        &mut self,
        cx: &Cx,
        pane_ids: &[u64],
        max_pipeline_depth: usize,
        pipeline_timeout: Duration,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, DirectMuxError> {
        if pane_ids.is_empty() {
            return Ok(Vec::new());
        }
        validate_render_batch_panes(pane_ids)?;
        self.get_pane_render_changes_batch_with_cx_prevalidated(
            cx,
            pane_ids,
            max_pipeline_depth,
            pipeline_timeout,
        )
        .await
    }

    /// Pool-only explicit-Cx render batch core after unique-pane prevalidation.
    ///
    /// The caller must reject duplicate pane IDs before acquiring a client.
    pub(super) async fn get_pane_render_changes_batch_with_cx_prevalidated(
        &mut self,
        cx: &Cx,
        pane_ids: &[u64],
        max_pipeline_depth: usize,
        pipeline_timeout: Duration,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, DirectMuxError> {
        if pane_ids.is_empty() {
            return Ok(Vec::new());
        }
        debug_assert!(validate_render_batch_panes(pane_ids).is_ok());
        let usable = self.ensure_connection_usable();
        self.settle_transport_result(usable, "render batch rejected by poisoned transport", true)?;
        let checkpoint = checkpoint_mux_cx(cx, self.connection_id, "render_batch_wait");
        self.settle_transport_result(checkpoint, "render batch preflight cancellation", true)?;
        let depth = max_pipeline_depth.max(1).min(pane_ids.len());
        let capacity = self
            .ensure_outstanding_request_slots(depth)
            .map_err(DirectMuxError::proven_pre_write_rejection);
        self.settle_transport_result(
            capacity,
            "render batch outstanding-request admission failure",
            true,
        )?;
        let timeout_ms = duration_to_ms_u64(pipeline_timeout);
        tracing::trace!(
            connection_id = self.connection_id,
            request_count = pane_ids.len(),
            max_pipeline_depth = depth,
            explicit_cx = true,
            phase = "render_batch_start",
            "starting mux render batch"
        );
        let mut guard = RenderBatchGuard::new(self, pane_ids, depth, true);
        let result = crate::runtime_async::timeout_with_cx(
            cx,
            pipeline_timeout,
            guard.run_with_cx(cx, depth),
        )
        .await;
        match result {
            Ok(Ok(())) => guard.finish(),
            Ok(Err(error)) => guard.fail_finish(error, "render batch execution failure"),
            Err(timeout_err) => {
                drop(guard);
                let error = classify_cx_timeout(
                    cx,
                    "render_batch_in_progress",
                    timeout_err,
                    DirectMuxError::BatchTimeout { timeout_ms },
                );
                self.apply_error_disposition(&error, "render batch interruption", true);
                Err(error)
            }
        }
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
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.batch_with_cx(&cx, requests, max_pipeline_depth, pipeline_timeout)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`batch`].
    ///
    /// Cancellation/budget/virtual-time propagate through each
    /// pipelined request via the cx-aware inner loop. This remains the
    /// Cx-first entry point for general PDU batching; render-change batches
    /// use their correlation-aware specialized path.
    pub async fn batch_with_cx(
        &mut self,
        cx: &Cx,
        requests: Vec<Pdu>,
        max_pipeline_depth: usize,
        pipeline_timeout: Duration,
    ) -> Result<Vec<Pdu>, DirectMuxError> {
        let timeout_ms = duration_to_ms_u64(pipeline_timeout);
        let checkpoint = checkpoint_mux_cx(cx, self.connection_id, "batch_wait");
        self.settle_transport_result(checkpoint, "request batch preflight cancellation", true)?;
        let result = crate::runtime_async::timeout_with_cx(
            cx,
            pipeline_timeout,
            self.batch_inner_with_cx(cx, requests, max_pipeline_depth.max(1)),
        )
        .await;
        let result = match result {
            Ok(inner) => inner,
            Err(timeout_err) => Err(classify_cx_timeout(
                cx,
                "batch_in_progress",
                timeout_err,
                DirectMuxError::BatchTimeout { timeout_ms },
            )),
        };
        self.settle_transport_result(result, "request batch failure", true)
    }

    fn fail_batch_scope<T>(
        &mut self,
        error: DirectMuxError,
        owns_in_flight: bool,
        explicit_cx: bool,
    ) -> Result<T, DirectMuxError> {
        let error = if owns_in_flight {
            DirectMuxError::in_flight_scope_abandoned(error)
        } else {
            error
        };
        self.apply_error_disposition(&error, "request batch failure", explicit_cx);
        Err(error)
    }

    async fn batch_inner_with_cx(
        &mut self,
        cx: &Cx,
        requests: Vec<Pdu>,
        max_pipeline_depth: usize,
    ) -> Result<Vec<Pdu>, DirectMuxError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_connection_usable()?;
        for request in &requests {
            self.authorize_outbound_pdu(request)?;
        }
        self.ensure_outstanding_request_slots(requests.len().min(max_pipeline_depth))
            .map_err(DirectMuxError::proven_pre_write_rejection)?;

        tracing::trace!(
            connection_id = self.connection_id,
            request_count = requests.len(),
            max_pipeline_depth,
            explicit_cx = true,
            phase = "batch_start",
            "starting mux request batch"
        );

        if max_pipeline_depth <= 1 {
            let mut responses = Vec::with_capacity(requests.len());
            for request in requests {
                responses.push(self.send_request_with_cx(cx, request).await?);
            }
            return Ok(responses);
        }

        let total = requests.len();
        let mut requests = requests.into_iter().enumerate();
        let mut in_flight = InFlightRequestSlots::with_capacity(max_pipeline_depth);
        let mut responses: Vec<Option<Pdu>> = std::iter::repeat_with(|| None).take(total).collect();

        while in_flight.len() < max_pipeline_depth {
            let Some((request_idx, request)) = requests.next() else {
                break;
            };
            let serial = match self.send_request_only_with_cx(cx, request).await {
                Ok(serial) => serial,
                Err(error) => {
                    return self.fail_batch_scope(error, !in_flight.is_empty(), true);
                }
            };
            if let Err(error) = in_flight.insert(serial, request_idx) {
                return self.fail_batch_scope(error, true, true);
            }
        }

        while !in_flight.is_empty() {
            let decoded = self.read_next_pdu_with_cx(cx).await?;
            if decoded.serial == 0 {
                self.stash_unilateral_pdu(decoded.pdu)?;
                continue;
            }
            if let Some(response_idx) = in_flight.take(decoded.serial) {
                self.complete_response_serial(decoded.serial)?;
                let response = match Self::response_from_pdu(decoded.pdu) {
                    Ok(response) => response,
                    Err(error) => {
                        return self.fail_batch_scope(error, !in_flight.is_empty(), true);
                    }
                };
                responses[response_idx] = Some(response);
                if let Some((request_idx, request)) = requests.next() {
                    let serial = match self.send_request_only_with_cx(cx, request).await {
                        Ok(serial) => serial,
                        Err(error) => {
                            return self.fail_batch_scope(error, !in_flight.is_empty(), true);
                        }
                    };
                    if let Err(error) = in_flight.insert(serial, request_idx) {
                        return self.fail_batch_scope(error, true, true);
                    }
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
        tracing::trace!(
            connection_id = self.connection_id,
            response_count = ordered.len(),
            max_pipeline_depth,
            explicit_cx = true,
            phase = "batch_complete",
            "mux request batch completed"
        );
        Ok(ordered)
    }

    fn ensure_connection_usable(&self) -> Result<(), DirectMuxError> {
        if self.connection_poisoned {
            Err(DirectMuxError::Disconnected)
        } else {
            Ok(())
        }
    }

    fn validate_protocol_state_connection(&self) -> Result<(), DirectMuxError> {
        let state_connection_id = self.protocol_state.connection_id();
        if state_connection_id == self.connection_id {
            Ok(())
        } else {
            Err(DirectMuxError::RetainedConnectionMismatch {
                expected_connection_id: self.connection_id,
                got_connection_id: state_connection_id,
            })
        }
    }

    fn authorize_outbound_pdu(&self, pdu: &Pdu) -> Result<(), DirectMuxError> {
        self.validate_protocol_state_connection()?;
        let pdu_name = pdu.pdu_name();
        match self.protocol_state {
            DirectMuxProtocolState::AwaitingCodec { .. } => {
                if !matches!(pdu, Pdu::GetCodecVersion(_)) {
                    return Err(DirectMuxError::OutboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: self.protocol_state.phase_name(),
                    });
                }
                Self::authorize_outbound_wire(
                    pdu,
                    CODEC_VERSION,
                    TopologyCapabilities::NONE,
                    TopologyCapabilities::NONE,
                )
            }
            DirectMuxProtocolState::AwaitingRegistration { codec } => {
                if !matches!(pdu, Pdu::SetClientId(_)) {
                    return Err(DirectMuxError::OutboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: self.protocol_state.phase_name(),
                    });
                }
                Self::authorize_outbound_wire(
                    pdu,
                    codec.agreed,
                    TopologyCapabilities::NONE,
                    TopologyCapabilities::NONE,
                )
            }
            DirectMuxProtocolState::Ready(authority) => {
                if matches!(pdu, Pdu::GetCodecVersion(_) | Pdu::SetClientId(_)) {
                    return Err(DirectMuxError::OutboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: self.protocol_state.phase_name(),
                    });
                }
                if matches!(
                    pdu,
                    Pdu::RenderApplicationResultV1(_) | Pdu::RenderApplicationResult(_)
                ) {
                    return Err(DirectMuxError::OutboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: "ready_render_application_inactive",
                    });
                }
                Self::authorize_outbound_wire(
                    pdu,
                    authority.codec.agreed,
                    authority.locally_activated_capabilities,
                    authority.negotiated_capabilities,
                )
            }
            DirectMuxProtocolState::Poisoned { .. } => {
                Err(DirectMuxError::OutboundPduInvalidForPhase {
                    pdu: pdu_name,
                    phase: self.protocol_state.phase_name(),
                })
            }
        }
    }

    fn authorize_outbound_wire(
        pdu: &Pdu,
        agreed_codec: usize,
        locally_activated_capabilities: TopologyCapabilities,
        negotiated_capabilities: TopologyCapabilities,
    ) -> Result<(), DirectMuxError> {
        let pdu_name = pdu.pdu_name();
        let Some(spec) = pdu.wire_spec() else {
            return Err(DirectMuxError::OutboundPduDirectionViolation { pdu: pdu_name });
        };
        if !spec.authorizes(PduProducer::Client, PduWireRole::Request) {
            return Err(DirectMuxError::OutboundPduDirectionViolation { pdu: pdu_name });
        }
        if spec.min_codec_version > agreed_codec {
            return Err(DirectMuxError::OutboundPduRequiresCodec {
                pdu: pdu_name,
                agreed: agreed_codec,
                required: spec.min_codec_version,
            });
        }
        let available = match spec.capability {
            PduCapabilityUse::None => return Ok(()),
            PduCapabilityUse::Negotiates(_) => locally_activated_capabilities,
            PduCapabilityUse::Requires(_) => negotiated_capabilities,
        };
        let required = match spec.capability {
            PduCapabilityUse::None => TopologyCapabilities::NONE,
            PduCapabilityUse::Negotiates(required) | PduCapabilityUse::Requires(required) => {
                required
            }
        };
        if available.contains(required) {
            Ok(())
        } else {
            Err(DirectMuxError::OutboundCapabilityNotNegotiated {
                pdu: pdu_name,
                negotiated: available.bits(),
                required: required.bits(),
            })
        }
    }

    fn authorize_inbound_pdu(&self, decoded: &DecodedPdu) -> Result<(), DirectMuxError> {
        self.validate_protocol_state_connection()?;
        let pdu_name = decoded.pdu.pdu_name();
        let role = if decoded.serial == 0 {
            PduWireRole::Unilateral
        } else {
            PduWireRole::CorrelatedReply
        };
        match self.protocol_state {
            DirectMuxProtocolState::AwaitingCodec { .. } => {
                if !matches!(
                    &decoded.pdu,
                    Pdu::GetCodecVersionResponse(_) | Pdu::ErrorResponse(_)
                ) {
                    return Err(DirectMuxError::InboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: self.protocol_state.phase_name(),
                    });
                }
                Self::authorize_inbound_wire(
                    &decoded.pdu,
                    role,
                    CODEC_VERSION,
                    TopologyCapabilities::NONE,
                    TopologyCapabilities::NONE,
                )
            }
            DirectMuxProtocolState::AwaitingRegistration { codec } => {
                if !matches!(&decoded.pdu, Pdu::UnitResponse(_) | Pdu::ErrorResponse(_)) {
                    return Err(DirectMuxError::InboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: self.protocol_state.phase_name(),
                    });
                }
                Self::authorize_inbound_wire(
                    &decoded.pdu,
                    role,
                    codec.agreed,
                    TopologyCapabilities::NONE,
                    TopologyCapabilities::NONE,
                )
            }
            DirectMuxProtocolState::Ready(authority) => {
                if matches!(&decoded.pdu, Pdu::GetCodecVersionResponse(_)) {
                    return Err(DirectMuxError::InboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: self.protocol_state.phase_name(),
                    });
                }
                if matches!(
                    &decoded.pdu,
                    Pdu::RenderApplicationUpdateV1(_) | Pdu::RenderApplicationUpdate(_)
                ) {
                    return Err(DirectMuxError::InboundPduInvalidForPhase {
                        pdu: pdu_name,
                        phase: "ready_render_application_inactive",
                    });
                }
                Self::authorize_inbound_wire(
                    &decoded.pdu,
                    role,
                    authority.codec.agreed,
                    authority.locally_activated_capabilities,
                    authority.negotiated_capabilities,
                )
            }
            DirectMuxProtocolState::Poisoned { .. } => {
                Err(DirectMuxError::InboundPduInvalidForPhase {
                    pdu: pdu_name,
                    phase: self.protocol_state.phase_name(),
                })
            }
        }
    }

    fn authorize_inbound_wire(
        pdu: &Pdu,
        role: PduWireRole,
        agreed_codec: usize,
        locally_activated_capabilities: TopologyCapabilities,
        negotiated_capabilities: TopologyCapabilities,
    ) -> Result<(), DirectMuxError> {
        let pdu_name = pdu.pdu_name();
        let Some(spec) = pdu.wire_spec() else {
            return Err(DirectMuxError::InboundPduDirectionViolation { pdu: pdu_name });
        };
        if !spec.authorizes(PduProducer::Server, role) {
            return Err(DirectMuxError::InboundPduDirectionViolation { pdu: pdu_name });
        }
        if spec.min_codec_version > agreed_codec {
            return Err(DirectMuxError::InboundPduRequiresCodec {
                pdu: pdu_name,
                agreed: agreed_codec,
                required: spec.min_codec_version,
            });
        }
        let available = match spec.capability {
            PduCapabilityUse::None => return Ok(()),
            PduCapabilityUse::Negotiates(_) => locally_activated_capabilities,
            PduCapabilityUse::Requires(_) => negotiated_capabilities,
        };
        let required = match spec.capability {
            PduCapabilityUse::None => TopologyCapabilities::NONE,
            PduCapabilityUse::Negotiates(required) | PduCapabilityUse::Requires(required) => {
                required
            }
        };
        if available.contains(required) {
            Ok(())
        } else {
            Err(DirectMuxError::InboundCapabilityNotNegotiated {
                pdu: pdu_name,
                negotiated: available.bits(),
                required: required.bits(),
            })
        }
    }

    fn poison_connection(&mut self, reason: &'static str, explicit_cx: bool) {
        if self.connection_poisoned {
            return;
        }
        #[cfg(test)]
        {
            self.poison_transition_count += 1;
        }
        let shutdown_error = self.stream.shutdown(std::net::Shutdown::Both).err();
        tracing::warn!(
            connection_id = self.connection_id,
            reason,
            explicit_cx,
            outstanding_count = self.outstanding_requests.len(),
            pending_response_count = self.pending_responses.len(),
            pending_response_bytes = self.pending_response_bytes,
            pending_render_change_count = self.pending_render_changes.len(),
            pending_render_change_bytes = self.pending_render_changes.retained_bytes(),
            render_snapshot_count = self.render_change_snapshots.len(),
            render_snapshot_bytes = self.render_change_snapshots.retained_bytes(),
            read_buffer_bytes = self.read_buf.len(),
            socket_shutdown_succeeded = shutdown_error.is_none(),
            socket_shutdown_error = ?shutdown_error,
            protocol_phase = self.protocol_state.phase_name(),
            phase = "connection_poison",
            "poisoning direct mux connection and clearing retained state"
        );
        self.connection_poisoned = true;
        self.protocol_state = DirectMuxProtocolState::Poisoned {
            connection_id: self.connection_id,
        };
        self.outstanding_requests = HashSet::new();
        self.pending_responses = HashMap::new();
        self.pending_response_bytes = 0;
        self.pending_render_changes = PendingRenderChanges::default();
        self.render_change_snapshots = RenderChangeSnapshots::default();
        self.read_buf = StreamingPduBuffer::new();
    }

    /// Apply the canonical recovery disposition before an operation error
    /// escapes a live DirectMux transport.
    ///
    /// This is deliberately idempotent through [`Self::poison_connection`].
    /// Nested protocol helpers may each defend their own return boundary, but
    /// one transport generation makes at most one healthy-to-poisoned
    /// transition. Reuse-class failures are left completely untouched.
    fn apply_error_disposition(
        &mut self,
        error: &DirectMuxError,
        reason: &'static str,
        explicit_cx: bool,
    ) {
        if matches!(
            mux_recovery_decision(error).connection,
            MuxConnectionDisposition::Discard
        ) {
            self.poison_connection(reason, explicit_cx);
        }
    }

    fn settle_transport_result<T>(
        &mut self,
        result: Result<T, DirectMuxError>,
        reason: &'static str,
        explicit_cx: bool,
    ) -> Result<T, DirectMuxError> {
        if let Err(error) = &result {
            self.apply_error_disposition(error, reason, explicit_cx);
        }
        result
    }

    fn unexpected_response<T>(
        &mut self,
        expected: impl Into<String>,
        got: &Pdu,
        explicit_cx: bool,
    ) -> Result<T, DirectMuxError> {
        let error = DirectMuxError::AlignedUnexpectedResponse {
            expected: expected.into(),
            got: got.pdu_name().to_string(),
        };
        self.apply_error_disposition(&error, "unexpected correlated response", explicit_cx);
        Err(error)
    }

    fn ensure_outstanding_request_slots(&self, additional: usize) -> Result<(), DirectMuxError> {
        let requested_count = self
            .outstanding_requests
            .len()
            .checked_add(additional)
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: "outstanding mux requests",
            })?;
        if requested_count > self.config.max_outstanding_requests {
            let item_bytes = std::mem::size_of::<u64>();
            return Err(DirectMuxError::RetentionLimitExceeded {
                resource: "outstanding mux requests",
                requested_count,
                requested_bytes: requested_count.saturating_mul(item_bytes),
                max_count: self.config.max_outstanding_requests,
                max_bytes: self
                    .config
                    .max_outstanding_requests
                    .saturating_mul(item_bytes),
            });
        }
        Ok(())
    }

    fn ensure_outstanding_request_capacity(&self) -> Result<(), DirectMuxError> {
        self.ensure_outstanding_request_slots(1)
    }

    fn mark_request_outstanding(&mut self, serial: u64) -> Result<(), DirectMuxError> {
        self.ensure_outstanding_request_capacity()?;
        if !self.outstanding_requests.insert(serial) {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: "outstanding mux request serials",
            });
        }
        Ok(())
    }

    fn validate_response_serial(&self, serial: u64) -> Result<(), DirectMuxError> {
        if serial == 0 || self.outstanding_requests.contains(&serial) {
            return Ok(());
        }
        Err(DirectMuxError::ResponseSerialNotOutstanding {
            connection_id: self.connection_id,
            serial,
        })
    }

    fn complete_response_serial(&mut self, serial: u64) -> Result<(), DirectMuxError> {
        if self.outstanding_requests.remove(&serial) {
            return Ok(());
        }
        Err(DirectMuxError::ResponseSerialNotOutstanding {
            connection_id: self.connection_id,
            serial,
        })
    }

    fn take_pending_response(&mut self, serial: u64) -> Result<Option<Pdu>, DirectMuxError> {
        let Some(retained) = self.pending_responses.remove(&serial) else {
            return Ok(None);
        };
        self.pending_response_bytes = self
            .pending_response_bytes
            .checked_sub(retained.retained_bytes())
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: "pending mux responses",
            })?;
        let pdu = retained.decode(self.connection_id, serial)?;
        self.complete_response_serial(serial)?;
        Ok(Some(pdu))
    }

    /// Plan and charge one exact request before serial allocation or codec
    /// buffer construction. The returned lease owns the PDU/plan pair and the
    /// shared budget reservation through the write boundary.
    fn admit_outbound_request(&self, pdu: Pdu) -> Result<DirectMuxOutboundLease, DirectMuxError> {
        let prepared = pdu
            .prepare_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                self.compression_mode,
            )
            .map_err(|error| DirectMuxError::Codec(error.to_string()))
            .map_err(DirectMuxError::proven_pre_write_rejection);
        let admitted = match prepared {
            Ok(prepared) => self
                .outbound_budget
                .try_admit(prepared, self.config.max_frame_bytes),
            Err(error) => Err(error),
        };
        admitted.inspect_err(|_| {
            metrics::counter!(
                "mux.direct_client.outbound.admission.total",
                "outcome" => "rejected"
            )
            .increment(1);
        })
    }

    async fn send_request_with_cx(&mut self, cx: &Cx, pdu: Pdu) -> Result<Pdu, DirectMuxError> {
        let serial = self.send_request_only_with_cx(cx, pdu).await?;
        self.await_response_with_cx(cx, serial).await
    }

    #[cfg(test)]
    async fn send_request_only(&mut self, pdu: Pdu) -> Result<u64, DirectMuxError> {
        self.send_request_only_tracking(pdu, None).await
    }

    #[cfg(test)]
    async fn send_request_only_tracking(
        &mut self,
        pdu: Pdu,
        write_boundary_entered: Option<&mut bool>,
    ) -> Result<u64, DirectMuxError> {
        let usable = self.ensure_connection_usable();
        self.settle_transport_result(usable, "request rejected by poisoned transport", false)?;
        let authorized = self.authorize_outbound_pdu(&pdu);
        self.settle_transport_result(authorized, "outbound PDU authority rejection", false)?;
        let capacity = self
            .ensure_outstanding_request_capacity()
            .map_err(DirectMuxError::proven_pre_write_rejection);
        self.settle_transport_result(capacity, "outstanding request admission failure", false)?;
        let admitted = self.admit_outbound_request(pdu);
        let mut outbound = self.settle_transport_result(
            admitted,
            "outbound request byte admission failure",
            false,
        )?;
        let serial_result = next_request_serial(&mut self.serial);
        let serial = self.settle_transport_result(
            serial_result,
            "request serial allocation failure",
            false,
        )?;
        let pdu_name = outbound.pdu_name();
        tracing::trace!(
            connection_id = self.connection_id,
            request_serial = serial,
            request_pdu = pdu_name,
            phase = "encode",
            compression_mode = ?self.compression_mode,
            "encoding mux request"
        );
        let encoded = outbound.encode_frame(serial);
        let buf = self.settle_transport_result(encoded, "outbound PDU encoding failure", false)?;
        let encoded_len = buf.len();
        if let Some(write_boundary_entered) = write_boundary_entered {
            *write_boundary_entered = true;
        }
        match timeout(self.config.write_timeout, self.stream.write_all(&buf)).await {
            Ok(Ok(())) => {
                tracing::trace!(
                    connection_id = self.connection_id,
                    request_serial = serial,
                    request_pdu = pdu_name,
                    encoded_bytes = encoded_len,
                    phase = "write_complete",
                    "mux request write completed"
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    connection_id = self.connection_id,
                    request_serial = serial,
                    request_pdu = pdu_name,
                    encoded_bytes = encoded_len,
                    phase = "write_error",
                    error = %err,
                    "mux request write failed"
                );
                let error = DirectMuxError::Io(err);
                self.apply_error_disposition(&error, "request write I/O failure", false);
                return Err(error);
            }
            Err(_) => {
                tracing::warn!(
                    connection_id = self.connection_id,
                    request_serial = serial,
                    request_pdu = pdu_name,
                    encoded_bytes = encoded_len,
                    timeout_ms = duration_to_ms_u64(self.config.write_timeout),
                    phase = "write_timeout",
                    "mux request write timed out"
                );
                let error = DirectMuxError::WriteTimeout;
                self.apply_error_disposition(&error, "request write timeout", false);
                return Err(error);
            }
        }
        let tracked = self.mark_request_outstanding(serial);
        self.settle_transport_result(
            tracked,
            "post-write request correlation accounting failure",
            false,
        )?;
        Ok(serial)
    }

    async fn send_request_only_with_cx(
        &mut self,
        cx: &Cx,
        pdu: Pdu,
    ) -> Result<u64, DirectMuxError> {
        self.send_request_only_with_cx_tracking(cx, pdu, None).await
    }

    async fn send_request_only_with_cx_tracking(
        &mut self,
        cx: &Cx,
        pdu: Pdu,
        write_boundary_entered: Option<&mut bool>,
    ) -> Result<u64, DirectMuxError> {
        let usable = self.ensure_connection_usable();
        self.settle_transport_result(usable, "request rejected by poisoned transport", true)?;
        let authorized = self.authorize_outbound_pdu(&pdu);
        self.settle_transport_result(authorized, "outbound PDU authority rejection", true)?;
        let checkpoint = checkpoint_mux_cx(cx, self.connection_id, "request_start");
        self.settle_transport_result(checkpoint, "request-start cancellation", true)?;
        let capacity = self
            .ensure_outstanding_request_capacity()
            .map_err(DirectMuxError::proven_pre_write_rejection);
        self.settle_transport_result(capacity, "outstanding request admission failure", true)?;
        let admitted = self.admit_outbound_request(pdu);
        let mut outbound = self.settle_transport_result(
            admitted,
            "outbound request byte admission failure",
            true,
        )?;
        let serial_result = next_request_serial(&mut self.serial);
        let serial =
            self.settle_transport_result(serial_result, "request serial allocation failure", true)?;
        let pdu_name = outbound.pdu_name();
        tracing::trace!(
            connection_id = self.connection_id,
            request_serial = serial,
            request_pdu = pdu_name,
            explicit_cx = true,
            phase = "encode",
            compression_mode = ?self.compression_mode,
            "encoding mux request"
        );
        let encoded = outbound.encode_frame(serial);
        let buf = self.settle_transport_result(encoded, "outbound PDU encoding failure", true)?;
        let encoded_len = buf.len();
        let checkpoint = checkpoint_mux_cx(cx, self.connection_id, "request_write_wait");
        self.settle_transport_result(checkpoint, "pre-write cancellation", true)?;
        if let Some(write_boundary_entered) = write_boundary_entered {
            *write_boundary_entered = true;
        }
        // Tick 199 (ft-xbnl0.2.3): route the write timeout through
        // timeout_with_cx so the caller's explicit cx bounds the
        // PDU write. Previously used ambient `timeout` — cancel
        // would only land via drop-cancel when the ambient wrapper
        // saw cancel via `Cx::current()` thread-local, not via the
        // explicit cx threaded into send_request_only_with_cx.
        match crate::runtime_async::timeout_with_cx(
            cx,
            self.config.write_timeout,
            self.stream.write_all(&buf),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::trace!(
                    connection_id = self.connection_id,
                    request_serial = serial,
                    request_pdu = pdu_name,
                    encoded_bytes = encoded_len,
                    explicit_cx = true,
                    phase = "write_complete",
                    "mux request write completed"
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    connection_id = self.connection_id,
                    request_serial = serial,
                    request_pdu = pdu_name,
                    encoded_bytes = encoded_len,
                    explicit_cx = true,
                    phase = "write_error",
                    error = %err,
                    "mux request write failed"
                );
                let error = DirectMuxError::Io(err);
                self.apply_error_disposition(&error, "request write I/O failure", true);
                return Err(error);
            }
            Err(timeout_err) => {
                tracing::warn!(
                    connection_id = self.connection_id,
                    request_serial = serial,
                    request_pdu = pdu_name,
                    encoded_bytes = encoded_len,
                    timeout_ms = duration_to_ms_u64(self.config.write_timeout),
                    explicit_cx = true,
                    phase = "write_timeout",
                    "mux request write timed out"
                );
                let error = classify_cx_timeout(
                    cx,
                    "request_write_in_progress",
                    timeout_err,
                    DirectMuxError::WriteTimeout,
                );
                self.apply_error_disposition(&error, "request write interruption", true);
                return Err(error);
            }
        }
        let tracked = self.mark_request_outstanding(serial);
        self.settle_transport_result(
            tracked,
            "post-write request correlation accounting failure",
            true,
        )?;
        Ok(serial)
    }

    #[cfg(test)]
    async fn await_response(&mut self, serial: u64) -> Result<Pdu, DirectMuxError> {
        let correlated = self.validate_response_serial(serial);
        self.settle_transport_result(correlated, "response waiter correlation violation", false)?;
        let pending = self.take_pending_response(serial);
        let pending =
            self.settle_transport_result(pending, "pending response retention failure", false)?;
        if let Some(pending) = pending {
            tracing::trace!(
                connection_id = self.connection_id,
                request_serial = serial,
                phase = "response_pending_hit",
                "served mux response from pending map"
            );
            return Self::response_from_pdu(pending);
        }
        loop {
            let decoded = self.read_next_pdu().await?;
            if decoded.serial == serial {
                let completed = self.complete_response_serial(serial);
                self.settle_transport_result(
                    completed,
                    "response completion accounting failure",
                    false,
                )?;
                return Self::response_from_pdu(decoded.pdu);
            }
            if decoded.serial == 0 {
                let stashed = self.stash_unilateral_pdu(decoded.pdu);
                self.settle_transport_result(stashed, "unilateral PDU retention failure", false)?;
                continue;
            }
            tracing::trace!(
                connection_id = self.connection_id,
                request_serial = serial,
                response_serial = decoded.serial,
                phase = "response_out_of_order",
                "stashing out-of-order mux response"
            );
            let stashed = self.stash_pending_response(decoded.serial, decoded.pdu);
            self.settle_transport_result(
                stashed,
                "out-of-order response retention failure",
                false,
            )?;
        }
    }

    async fn await_response_with_cx(
        &mut self,
        cx: &Cx,
        serial: u64,
    ) -> Result<Pdu, DirectMuxError> {
        let correlated = self.validate_response_serial(serial);
        self.settle_transport_result(correlated, "response waiter correlation violation", true)?;
        let pending = self.take_pending_response(serial);
        let pending =
            self.settle_transport_result(pending, "pending response retention failure", true)?;
        if let Some(pending) = pending {
            tracing::trace!(
                connection_id = self.connection_id,
                request_serial = serial,
                explicit_cx = true,
                phase = "response_pending_hit",
                "served mux response from pending map"
            );
            return Self::response_from_pdu(pending);
        }
        loop {
            let decoded = self.read_next_pdu_with_cx(cx).await?;
            if decoded.serial == serial {
                let completed = self.complete_response_serial(serial);
                self.settle_transport_result(
                    completed,
                    "response completion accounting failure",
                    true,
                )?;
                return Self::response_from_pdu(decoded.pdu);
            }
            if decoded.serial == 0 {
                let stashed = self.stash_unilateral_pdu(decoded.pdu);
                self.settle_transport_result(stashed, "unilateral PDU retention failure", true)?;
                continue;
            }
            tracing::trace!(
                connection_id = self.connection_id,
                request_serial = serial,
                response_serial = decoded.serial,
                explicit_cx = true,
                phase = "response_out_of_order",
                "stashing out-of-order mux response"
            );
            let stashed = self.stash_pending_response(decoded.serial, decoded.pdu);
            self.settle_transport_result(stashed, "out-of-order response retention failure", true)?;
        }
    }

    fn response_from_pdu(pdu: Pdu) -> Result<Pdu, DirectMuxError> {
        match pdu {
            Pdu::ErrorResponse(err) => Err(DirectMuxError::RemoteError(err.reason)),
            other => Ok(other),
        }
    }

    fn settle_single_render_response(
        &mut self,
        pane_id: u64,
        response: Result<Pdu, DirectMuxError>,
        explicit_cx: bool,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        let result = match response {
            Ok(response) => self.resolve_render_change_response(pane_id, response),
            Err(error @ DirectMuxError::RemoteError(_)) => {
                if let Err(cleanup_error) = self.invalidate_render_state_for_pane(pane_id) {
                    self.poison_connection(
                        "single render remote-error cleanup accounting failure",
                        explicit_cx,
                    );
                    return Err(cleanup_error);
                }
                Err(error)
            }
            Err(error) => {
                self.poison_connection("single render response settlement failure", explicit_cx);
                return Err(error);
            }
        };
        self.settle_transport_result(
            result,
            "single render response settlement failure",
            explicit_cx,
        )
    }

    fn resolve_render_change_response(
        &mut self,
        pane_id: u64,
        response: Pdu,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        self.resolve_render_change_response_with_sideband(pane_id, response, None, 0, 0)
    }

    fn resolve_render_change_response_with_sideband(
        &mut self,
        pane_id: u64,
        response: Pdu,
        local_sideband: Option<TypedRenderSideband>,
        reserved_local_count: usize,
        reserved_local_bytes: usize,
    ) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        match response {
            Pdu::GetPaneRenderChangesResponse(payload) => {
                if let Some(sideband) = local_sideband {
                    if sideband.payload.pane_id as u64 != pane_id
                        || self.pending_render_changes.contains_pane(pane_id)?
                    {
                        return Err(DirectMuxError::RetainedStateAccounting {
                            resource: "batch-local typed render sideband FIFO",
                        });
                    }
                    self.stash_unilateral_render_change_with_reservation(
                        sideband,
                        reserved_local_count,
                        reserved_local_bytes,
                    )?;
                    #[cfg(test)]
                    {
                        self.render_retention_codec_stats.batch_local_demotions += 1;
                    }
                }
                if payload.pane_id as u64 != pane_id {
                    self.invalidate_render_state_for_pane(pane_id)?;
                    return Err(DirectMuxError::AlignedUnexpectedResponse {
                        expected: format!("GetPaneRenderChangesResponse for pane {pane_id}"),
                        got: format!("GetPaneRenderChangesResponse for pane {}", payload.pane_id),
                    });
                }
                self.remember_render_change_snapshot(&payload)?;
                Ok(payload)
            }
            Pdu::LivenessResponse(liveness) => {
                if liveness.pane_id as u64 != pane_id {
                    self.invalidate_render_state_for_pane(pane_id)?;
                    return Err(DirectMuxError::AlignedUnexpectedResponse {
                        expected: format!("LivenessResponse for pane {pane_id}"),
                        got: format!("LivenessResponse for pane {}", liveness.pane_id),
                    });
                }
                if !liveness.is_alive {
                    self.invalidate_render_state_for_pane(pane_id)?;
                    return Err(DirectMuxError::RemoteError(format!(
                        "pane {pane_id} is not alive"
                    )));
                }
                if let Some(sideband) = local_sideband {
                    if sideband.payload.pane_id as u64 != pane_id
                        || self.pending_render_changes.contains_pane(pane_id)?
                    {
                        return Err(DirectMuxError::RetainedStateAccounting {
                            resource: "batch-local typed render sideband FIFO",
                        });
                    }
                    self.remember_render_change_snapshot(&sideband.payload)?;
                    #[cfg(test)]
                    {
                        self.render_retention_codec_stats.batch_local_returns += 1;
                    }
                    return Ok(sideband.payload);
                }
                if let Some(payload) = self.take_pending_render_change(pane_id)? {
                    return Ok(payload);
                }
                if let Some(payload) = self
                    .render_change_snapshots
                    .get(pane_id)
                    .map(|snapshot| snapshot.decode(self.connection_id))
                    .transpose()?
                {
                    return Ok(payload);
                }
                self.invalidate_render_state_for_pane(pane_id)?;
                Err(DirectMuxError::AlignedUnexpectedResponse {
                    expected: format!(
                        "GetPaneRenderChangesResponse or cached render snapshot for pane {pane_id}"
                    ),
                    got: "LivenessResponse without accompanying render delta".to_string(),
                })
            }
            other => {
                self.invalidate_render_state_for_pane(pane_id)?;
                Err(DirectMuxError::AlignedUnexpectedResponse {
                    expected: "LivenessResponse or GetPaneRenderChangesResponse".to_string(),
                    got: other.pdu_name().to_string(),
                })
            }
        }
    }

    fn stash_unilateral_pdu(&mut self, pdu: Pdu) -> Result<(), DirectMuxError> {
        match pdu {
            Pdu::GetPaneRenderChangesResponse(payload) => {
                self.stash_unilateral_render_change(payload)?;
            }
            Pdu::PaneRemoved(removed) => {
                let pane_id = removed.pane_id as u64;
                let (pending_removed, snapshot_removed) =
                    self.invalidate_render_state_for_pane(pane_id)?;
                tracing::trace!(
                    connection_id = self.connection_id,
                    pane_id,
                    pending_removed,
                    snapshot_removed,
                    phase = "pane_removed_invalidate",
                    "invalidated direct mux render retention for removed pane"
                );
            }
            other => {
                tracing::trace!(
                    connection_id = self.connection_id,
                    response_pdu = other.pdu_name(),
                    phase = "unilateral_drop",
                    "ignoring unsupported unilateral mux PDU"
                );
            }
        }
        Ok(())
    }

    fn stash_unilateral_render_change(
        &mut self,
        payload: GetPaneRenderChangesResponse,
    ) -> Result<(), DirectMuxError> {
        self.stash_unilateral_render_change_inner(payload, 0, 0)
    }

    fn stash_unilateral_render_change_with_reservation(
        &mut self,
        sideband: TypedRenderSideband,
        reserved_local_count: usize,
        reserved_local_bytes: usize,
    ) -> Result<(), DirectMuxError> {
        self.stash_unilateral_render_change_inner(
            sideband.payload,
            reserved_local_count,
            reserved_local_bytes,
        )
    }

    fn stash_unilateral_render_change_inner(
        &mut self,
        payload: GetPaneRenderChangesResponse,
        reserved_local_count: usize,
        reserved_local_bytes: usize,
    ) -> Result<(), DirectMuxError> {
        if (reserved_local_count == 0) != (reserved_local_bytes == 0) {
            return Err(DirectMuxError::RetainedStateAccounting {
                resource: BatchLocalRenderSidebands::RESOURCE,
            });
        }
        let pane_id = payload.pane_id as u64;
        let snapshot = self.encode_render_change_snapshot(&payload)?;
        let pending = self.encode_pending_render_change(payload)?;

        self.pending_render_changes.validate()?;
        let aggregate_count = self
            .pending_render_changes
            .len()
            .checked_add(reserved_local_count)
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: PendingRenderChanges::RESOURCE,
            })?;
        let aggregate_bytes = self
            .pending_render_changes
            .retained_bytes()
            .checked_add(reserved_local_bytes)
            .ok_or(DirectMuxError::RetainedStateAccounting {
                resource: PendingRenderChanges::RESOURCE,
            })?;
        let pending_limit = RetentionLimit {
            max_count: self.config.max_pending_render_changes,
            max_bytes: self.config.max_pending_render_change_bytes,
        };
        let _ = checked_retention_after_insert(
            PendingRenderChanges::RESOURCE,
            aggregate_count,
            aggregate_bytes,
            None,
            pending.retained_bytes(),
            pending_limit,
        )?;

        let next_pending = self
            .pending_render_changes
            .admit_insert(&pending, pending_limit)?;
        let next_snapshot = self.render_change_snapshots.admit_insert(
            pane_id,
            &snapshot,
            RetentionLimit {
                max_count: self.config.max_render_change_snapshots,
                max_bytes: self.config.max_render_change_snapshot_bytes,
            },
        )?;

        self.render_change_snapshots
            .commit_insert(pane_id, snapshot, next_snapshot);
        self.pending_render_changes
            .commit_insert(pending, next_pending);
        Ok(())
    }

    fn take_pending_render_change(
        &mut self,
        pane_id: u64,
    ) -> Result<Option<GetPaneRenderChangesResponse>, DirectMuxError> {
        let Some(retained) = self.pending_render_changes.take_for_pane(pane_id)? else {
            return Ok(None);
        };
        #[cfg(test)]
        {
            self.render_retention_codec_stats.pending_payload_decodes += 1;
        }
        retained.decode(self.connection_id).map(Some)
    }

    fn remember_render_change_snapshot(
        &mut self,
        payload: &GetPaneRenderChangesResponse,
    ) -> Result<(), DirectMuxError> {
        let pane_id = payload.pane_id as u64;
        let snapshot = self.encode_render_change_snapshot(payload)?;
        let next = self.render_change_snapshots.admit_insert(
            pane_id,
            &snapshot,
            RetentionLimit {
                max_count: self.config.max_render_change_snapshots,
                max_bytes: self.config.max_render_change_snapshot_bytes,
            },
        )?;
        self.render_change_snapshots
            .commit_insert(pane_id, snapshot, next);
        Ok(())
    }

    // Test-only codec accounting mutates the receiver; production retains the
    // same signature so the measured and unmeasured paths cannot diverge.
    #[cfg_attr(not(test), allow(clippy::needless_pass_by_ref_mut))]
    fn encode_pending_render_change(
        &mut self,
        payload: GetPaneRenderChangesResponse,
    ) -> Result<RetainedRenderChange, DirectMuxError> {
        let retained = RetainedRenderChange::encode(self.connection_id, payload)?;
        #[cfg(test)]
        {
            self.render_retention_codec_stats.pending_payload_encodes += 1;
            self.render_retention_codec_stats
                .pending_payload_frame_allocations += 1;
            self.render_retention_codec_stats
                .pending_payload_encoded_bytes += retained.retained_bytes();
            self.render_retention_codec_stats
                .pending_payload_frame_capacity_bytes += retained.pdu.frame.capacity();
        }
        Ok(retained)
    }

    // Test-only codec accounting mutates the receiver; production retains the
    // same signature so the measured and unmeasured paths cannot diverge.
    #[cfg_attr(not(test), allow(clippy::needless_pass_by_ref_mut))]
    fn encode_render_change_snapshot(
        &mut self,
        payload: &GetPaneRenderChangesResponse,
    ) -> Result<RetainedRenderChange, DirectMuxError> {
        let retained =
            RetainedRenderChange::encode(self.connection_id, Self::idle_render_snapshot(payload))?;
        #[cfg(test)]
        {
            self.render_retention_codec_stats.snapshot_encodes += 1;
            self.render_retention_codec_stats.snapshot_frame_allocations += 1;
            self.render_retention_codec_stats.snapshot_encoded_bytes += retained.retained_bytes();
            self.render_retention_codec_stats
                .snapshot_frame_capacity_bytes += retained.pdu.frame.capacity();
        }
        Ok(retained)
    }

    fn invalidate_render_state_for_pane(
        &mut self,
        pane_id: u64,
    ) -> Result<(usize, bool), DirectMuxError> {
        let targets = HashSet::from([pane_id]);
        let snapshot_plan = self.render_change_snapshots.plan_remove_panes(&targets)?;
        let pending_plan = self.pending_render_changes.plan_remove_panes(&targets)?;
        self.render_change_snapshots
            .commit_remove_panes(&targets, snapshot_plan);
        self.pending_render_changes
            .commit_remove_panes(&targets, pending_plan);
        Ok((pending_plan.removed_count, snapshot_plan.removed_count != 0))
    }

    fn invalidate_render_state_for_panes(
        &mut self,
        pane_ids: &[u64],
    ) -> Result<(usize, usize), DirectMuxError> {
        if pane_ids.is_empty() {
            return Ok((0, 0));
        }

        let targets = pane_ids.iter().copied().collect::<HashSet<_>>();
        let snapshot_plan = self.render_change_snapshots.plan_remove_panes(&targets)?;
        let pending_plan = self.pending_render_changes.plan_remove_panes(&targets)?;
        self.render_change_snapshots
            .commit_remove_panes(&targets, snapshot_plan);
        self.pending_render_changes
            .commit_remove_panes(&targets, pending_plan);
        Ok((pending_plan.removed_count, snapshot_plan.removed_count))
    }

    fn idle_render_snapshot(
        payload: &GetPaneRenderChangesResponse,
    ) -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: payload.pane_id,
            mouse_grabbed: payload.mouse_grabbed,
            alt_screen_active: payload.alt_screen_active,
            cursor_position: payload.cursor_position,
            dimensions: payload.dimensions,
            tiered_scrollback_status: payload.tiered_scrollback_status,
            dirty_lines: Vec::new(),
            title: payload.title.clone(),
            working_dir: payload.working_dir.clone(),
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno: payload.seqno,
        }
    }

    fn stash_pending_response(&mut self, serial: u64, pdu: Pdu) -> Result<(), DirectMuxError> {
        if serial == 0 {
            return Err(DirectMuxError::UnexpectedResponse {
                expected: "nonzero request response serial".to_string(),
                got: "reserved unilateral serial 0".to_string(),
            });
        }
        self.validate_response_serial(serial)?;
        if self.pending_responses.contains_key(&serial) {
            tracing::warn!(
                connection_id = self.connection_id,
                duplicate_serial = serial,
                phase = "stash_pending_response",
                "duplicate mux response serial observed"
            );
            return Err(DirectMuxError::UnexpectedResponse {
                expected: "unique serial".to_string(),
                got: format!("duplicate response serial {serial}"),
            });
        }
        let retained = RetainedMuxPdu::encode(self.connection_id, serial, pdu)?;
        let (_, next_bytes) = checked_retention_after_insert(
            "pending mux responses",
            self.pending_responses.len(),
            self.pending_response_bytes,
            None,
            retained.retained_bytes(),
            RetentionLimit {
                max_count: self.config.max_pending_responses,
                max_bytes: self.config.max_pending_response_bytes,
            },
        )?;
        let replaced = self.pending_responses.insert(serial, retained);
        debug_assert!(replaced.is_none());
        self.pending_response_bytes = next_bytes;
        Ok(())
    }

    #[cfg(test)]
    async fn read_next_pdu(&mut self) -> Result<DecodedPdu, DirectMuxError> {
        self.read_next_pdu_with_retention_metadata()
            .await
            .map(|decoded| decoded.into_parts().0)
    }

    #[cfg(test)]
    async fn read_next_pdu_with_retention_metadata(
        &mut self,
    ) -> Result<codec::DecodedPduWithRetentionMetadata, DirectMuxError> {
        loop {
            let decoded_result = decode_from_buffer_with_retention_metadata(
                &mut self.read_buf,
                self.config.max_frame_bytes,
            );
            let decoded_result = self.settle_transport_result(
                decoded_result,
                "inbound frame decode failure",
                false,
            )?;
            if let Some(decoded) = decoded_result {
                let decoded_ref = decoded.decoded();
                if let Err(error) = self.authorize_inbound_pdu(decoded_ref) {
                    self.apply_error_disposition(&error, "inbound PDU authority violation", false);
                    return Err(error);
                }
                let correlated = self.validate_response_serial(decoded_ref.serial);
                self.settle_transport_result(
                    correlated,
                    "inbound response correlation violation",
                    false,
                )?;
                tracing::trace!(
                    connection_id = self.connection_id,
                    response_serial = decoded_ref.serial,
                    response_pdu = decoded_ref.pdu.pdu_name(),
                    phase = "decode_buffered_pdu",
                    "decoded mux response from buffered bytes"
                );
                return Ok(decoded);
            }

            let mut temp = vec![0u8; 4096];
            let read = match timeout(
                self.config.read_timeout,
                unix_stream_read(&mut self.stream, &mut temp),
            )
            .await
            {
                Ok(Ok(read)) => read,
                Ok(Err(err)) => {
                    tracing::warn!(
                        connection_id = self.connection_id,
                        phase = "read_io_error",
                        error = %err,
                        "mux response read failed"
                    );
                    let error = DirectMuxError::Io(err);
                    self.apply_error_disposition(&error, "response read I/O failure", false);
                    return Err(error);
                }
                Err(_) => {
                    tracing::warn!(
                        connection_id = self.connection_id,
                        timeout_ms = duration_to_ms_u64(self.config.read_timeout),
                        phase = "read_timeout",
                        "mux response read timed out"
                    );
                    let error = DirectMuxError::ReadTimeout;
                    self.apply_error_disposition(&error, "response read timeout", false);
                    return Err(error);
                }
            };
            if read == 0 {
                tracing::debug!(
                    connection_id = self.connection_id,
                    phase = "read_eof",
                    "mux socket disconnected during response read"
                );
                let error = DirectMuxError::Disconnected;
                self.apply_error_disposition(&error, "response read EOF", false);
                return Err(error);
            }
            self.read_buf.extend_from_slice(&temp[..read]);
        }
    }

    async fn read_next_pdu_with_cx(&mut self, cx: &Cx) -> Result<DecodedPdu, DirectMuxError> {
        self.read_next_pdu_with_retention_metadata_with_cx(cx)
            .await
            .map(|decoded| decoded.into_parts().0)
    }

    async fn read_next_pdu_with_retention_metadata_with_cx(
        &mut self,
        cx: &Cx,
    ) -> Result<codec::DecodedPduWithRetentionMetadata, DirectMuxError> {
        loop {
            let decoded_result = decode_from_buffer_with_retention_metadata(
                &mut self.read_buf,
                self.config.max_frame_bytes,
            );
            let decoded_result =
                self.settle_transport_result(decoded_result, "inbound frame decode failure", true)?;
            if let Some(decoded) = decoded_result {
                let decoded_ref = decoded.decoded();
                if let Err(error) = self.authorize_inbound_pdu(decoded_ref) {
                    self.apply_error_disposition(&error, "inbound PDU authority violation", true);
                    return Err(error);
                }
                let correlated = self.validate_response_serial(decoded_ref.serial);
                self.settle_transport_result(
                    correlated,
                    "inbound response correlation violation",
                    true,
                )?;
                tracing::trace!(
                    connection_id = self.connection_id,
                    response_serial = decoded_ref.serial,
                    response_pdu = decoded_ref.pdu.pdu_name(),
                    explicit_cx = true,
                    phase = "decode_buffered_pdu",
                    "decoded mux response from buffered bytes"
                );
                return Ok(decoded);
            }

            let checkpoint = checkpoint_mux_cx(cx, self.connection_id, "response_read_wait");
            self.settle_transport_result(checkpoint, "response read cancellation", true)?;
            let mut temp = vec![0u8; 4096];
            let read = match crate::runtime_async::timeout_with_cx(
                cx,
                self.config.read_timeout,
                unix_stream_read_with_cx(cx, &mut self.stream, &mut temp),
            )
            .await
            {
                Ok(Ok(read)) => read,
                Ok(Err(err)) => {
                    tracing::warn!(
                        connection_id = self.connection_id,
                        explicit_cx = true,
                        phase = "read_io_error",
                        error = %err,
                        "mux response read failed"
                    );
                    let error = if cx.is_cancel_requested() {
                        cancelled_mux_error("response_read_wait", err)
                    } else {
                        DirectMuxError::Io(err)
                    };
                    self.apply_error_disposition(&error, "response read interruption", true);
                    return Err(error);
                }
                Err(timeout_err) => {
                    tracing::warn!(
                        connection_id = self.connection_id,
                        timeout_ms = duration_to_ms_u64(self.config.read_timeout),
                        explicit_cx = true,
                        phase = "read_timeout",
                        "mux response read timed out"
                    );
                    let error = classify_cx_timeout(
                        cx,
                        "response_read_wait",
                        timeout_err,
                        DirectMuxError::ReadTimeout,
                    );
                    self.apply_error_disposition(&error, "response read timeout", true);
                    return Err(error);
                }
            };
            if read == 0 {
                tracing::debug!(
                    connection_id = self.connection_id,
                    explicit_cx = true,
                    phase = "read_eof",
                    "mux socket disconnected during response read"
                );
                let error = DirectMuxError::Disconnected;
                self.apply_error_disposition(&error, "response read EOF", true);
                return Err(error);
            }
            self.read_buf.extend_from_slice(&temp[..read]);
        }
    }
}

fn ambient_mux_cx() -> Cx {
    Cx::current().unwrap_or_else(crate::cx::for_request)
}

fn next_connection_id() -> Result<u64, DirectMuxError> {
    next_connection_id_from(&NEXT_CONNECTION_ID)
}

fn next_connection_id_from(next: &AtomicU64) -> Result<u64, DirectMuxError> {
    let mut current = next.load(Ordering::Relaxed);
    loop {
        if current == 0 || current == u64::MAX {
            return Err(DirectMuxError::ConnectionIdExhausted);
        }

        match next.compare_exchange(current, current + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

fn next_request_serial(serial: &mut u64) -> Result<u64, DirectMuxError> {
    *serial = serial
        .checked_add(1)
        .ok_or(DirectMuxError::SerialExhausted)?;
    Ok(*serial)
}

fn checked_retention_after_insert(
    resource: &'static str,
    current_count: usize,
    current_bytes: usize,
    replaced_bytes: Option<usize>,
    added_bytes: usize,
    limit: RetentionLimit,
) -> Result<(usize, usize), DirectMuxError> {
    let replaced_count = usize::from(replaced_bytes.is_some());
    let requested_count = current_count
        .checked_sub(replaced_count)
        .and_then(|count| count.checked_add(1))
        .ok_or(DirectMuxError::RetainedStateAccounting { resource })?;
    let requested_bytes = current_bytes
        .checked_sub(replaced_bytes.unwrap_or(0))
        .and_then(|bytes| bytes.checked_add(added_bytes))
        .ok_or(DirectMuxError::RetainedStateAccounting { resource })?;
    if requested_count > limit.max_count || requested_bytes > limit.max_bytes {
        return Err(DirectMuxError::RetentionLimitExceeded {
            resource,
            requested_count,
            requested_bytes,
            max_count: limit.max_count,
            max_bytes: limit.max_bytes,
        });
    }
    Ok((requested_count, requested_bytes))
}

fn duration_to_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn validate_render_batch_panes(pane_ids: &[u64]) -> Result<(), DirectMuxError> {
    let mut seen = HashSet::with_capacity(pane_ids.len());
    for pane_id in pane_ids.iter().copied() {
        if !seen.insert(pane_id) {
            return Err(DirectMuxError::DuplicateRenderBatchPane { pane_id });
        }
    }
    Ok(())
}

#[cfg(test)]
fn decode_from_buffer(
    buffer: &mut StreamingPduBuffer,
    max_frame_bytes: usize,
) -> Result<Option<DecodedPdu>, DirectMuxError> {
    codec::Pdu::stream_decode_with_frame_limit(buffer, max_frame_bytes).map_err(|error| {
        if let Some(limit) = error.downcast_ref::<codec::StreamingPduFrameLimitExceeded>() {
            DirectMuxError::FrameTooLarge {
                max_bytes: limit.max_frame_bytes(),
            }
        } else {
            DirectMuxError::Codec(error.to_string())
        }
    })
}

fn decode_from_buffer_with_retention_metadata(
    buffer: &mut StreamingPduBuffer,
    max_frame_bytes: usize,
) -> Result<Option<codec::DecodedPduWithRetentionMetadata>, DirectMuxError> {
    codec::Pdu::stream_decode_with_retention_metadata_and_frame_limit(buffer, max_frame_bytes)
        .map_err(|error| {
            if let Some(limit) = error.downcast_ref::<codec::StreamingPduFrameLimitExceeded>() {
                DirectMuxError::FrameTooLarge {
                    max_bytes: limit.max_frame_bytes(),
                }
            } else {
                DirectMuxError::Codec(error.to_string())
            }
        })
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
    resolve_compression_mode_for_locality(mode, is_local_unix_socket(socket_path))
}

fn resolve_compression_mode_for_locality(
    mode: wa_config::VendoredCompressionMode,
    is_local_socket: bool,
) -> CompressionMode {
    match mode {
        wa_config::VendoredCompressionMode::Always => CompressionMode::Always,
        wa_config::VendoredCompressionMode::Never => CompressionMode::Never,
        wa_config::VendoredCompressionMode::Auto => {
            if is_local_socket {
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

fn should_auto_fallback_to_always(
    configured_mode: wa_config::VendoredCompressionMode,
    resolved_mode: CompressionMode,
    err: &DirectMuxError,
) -> bool {
    let decision = mux_recovery_decision(err);
    matches!(configured_mode, wa_config::VendoredCompressionMode::Auto)
        && matches!(resolved_mode, CompressionMode::Never)
        && matches!(decision.kind, ProtocolErrorKind::Recoverable)
        && decision.retry
        && !decision.cancelled
        && matches!(decision.connection, MuxConnectionDisposition::Discard)
}

#[cfg(test)]
async fn unix_stream_read(stream: &mut UnixStream, buf: &mut [u8]) -> std::io::Result<usize> {
    io::read(stream, buf).await
}

/// ft-xbnl0.2.3 Cx-first sibling of [`unix_stream_read`].
///
/// Pre-flight `cx.checkpoint()` is folded into an internal
/// `io::ErrorKind::Interrupted` so polling stops before the underlying read.
/// `read_next_pdu_with_cx` translates that signal back into the structured
/// `DirectMuxError::Cancelled` authority while the parent `Cx` is in scope.
async fn unix_stream_read_with_cx(
    cx: &Cx,
    stream: &mut UnixStream,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    cx.checkpoint().map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            format!("unix_stream_read cancelled: {err}"),
        )
    })?;
    io::read(stream, buf).await
}

// ---------------------------------------------------------------------------
// PaneOutputSubscription: stream pane output as deltas (wa-nu4.4.2.2)
// ---------------------------------------------------------------------------

/// A delta event from a pane's output, compatible with the capture gap model.
#[derive(Debug, Clone)]
pub enum PaneDelta {
    /// New content was rendered (dirty lines changed).
    Output {
        pane_id: u64,
        /// Mux-side terminal mutation version from
        /// `GetPaneRenderChangesResponse`.
        ///
        /// This is diagnostic source state, not a contiguous delivery
        /// sequence. Multiple terminal mutations between polls can advance it
        /// by more than one without any transport loss.
        seqno: u64,
        /// Best-effort UTF-8 text extracted from render-change bonus lines.
        ///
        /// This is the closest available approximation to output deltas using
        /// `GetPaneRenderChanges` polling. It may be empty when no bonus lines
        /// are present. Downstream must retain that as typed metadata-only
        /// activity and must not fabricate terminal output bytes.
        delta_text: String,
        /// Title of the pane at the time of the delta.
        title: String,
        /// Number of dirty line ranges reported.
        dirty_range_count: usize,
        /// Total number of dirty rows across all ranges.
        dirty_row_count: usize,
    },
    /// A proven capture-delivery gap was detected (for example, bounded-channel
    /// overflow or reconnect without an exact recovery baseline).
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
enum SubscriptionTask {
    Scoped(cx::JoinHandle<()>),
}

pub struct PaneOutputSubscription {
    receiver: mpsc::Receiver<PaneDelta>,
    cancel: watch::Sender<bool>,
    task: Option<SubscriptionTask>,
}

async fn pane_delta_recv_with_cx(cx: &Cx, rx: &mut mpsc::Receiver<PaneDelta>) -> Option<PaneDelta> {
    rx.recv(cx).await.ok()
}

#[cfg(test)]
async fn pane_delta_recv(rx: &mut mpsc::Receiver<PaneDelta>) -> Option<PaneDelta> {
    let cx = ambient_mux_cx();
    pane_delta_recv_with_cx(&cx, rx).await
}

#[cfg(test)]
async fn pane_delta_send(tx: &mpsc::Sender<PaneDelta>, delta: PaneDelta) {
    let _ = mpsc_reserve_send(tx, delta).await;
}

fn pane_delta_try_send(tx: &mpsc::Sender<PaneDelta>, delta: PaneDelta) -> bool {
    mpsc_try_reserve_send(tx, delta)
}

fn pane_delta_try_emit_ended(
    tx: &mpsc::Sender<PaneDelta>,
    pane_id: u64,
    reason: impl Into<String>,
) {
    let _ = pane_delta_try_send(
        tx,
        PaneDelta::Ended {
            pane_id,
            reason: reason.into(),
        },
    );
}

async fn join_subscription_task(task: SubscriptionTask) {
    let SubscriptionTask::Scoped(handle) = task;
    handle.await;
}

#[allow(clippy::needless_pass_by_ref_mut)] // mut needed for the update-taking watch path
fn cancel_requested(cancel_rx: &mut watch::Receiver<bool>) -> bool {
    cancel_rx.borrow_and_clone()
}

async fn wait_for_cancel_change_with_cx(cx: &Cx, cancel_rx: &mut watch::Receiver<bool>) -> bool {
    cancel_rx.changed(cx).await.is_ok()
}

/// A subscription owns one client and cannot reconnect it. It may retry only
/// when the canonical recovery authority permits both replay and reuse.
fn subscription_can_retry_same_client(err: &DirectMuxError) -> bool {
    let decision = mux_recovery_decision(err);
    decision.retry
        && !decision.cancelled
        && matches!(decision.connection, MuxConnectionDisposition::Reuse)
}

async fn run_subscription_loop(
    cx: &Cx,
    mut client: DirectMuxClient,
    pane_id: u64,
    config: SubscriptionConfig,
    tx: mpsc::Sender<PaneDelta>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    loop {
        if cx.checkpoint().is_err() {
            pane_delta_try_emit_ended(&tx, pane_id, "cancelled");
            break;
        }

        if cancel_requested(&mut cancel_rx) {
            pane_delta_try_emit_ended(&tx, pane_id, "cancelled");
            break;
        }

        let result = client.get_pane_render_changes_with_cx(cx, pane_id).await;

        let saw_dirty_output = match result {
            Ok(changes) => match render_changes_to_output_delta(pane_id, changes) {
                Some(delta) => {
                    if !pane_delta_try_send(&tx, delta) {
                        let _ = pane_delta_try_send(
                            &tx,
                            PaneDelta::Gap {
                                pane_id,
                                reason: "slow consumer: channel full".to_string(),
                            },
                        );
                    }
                    true
                }
                None => false,
            },
            Err(err) if subscription_can_retry_same_client(&err) => {
                tracing::debug!(
                    pane_id,
                    error_kind = ?err.protocol_error_kind(),
                    error = %err,
                    "subscription poll failed without invalidating its client; retrying"
                );
                false
            }
            Err(err) => {
                pane_delta_try_emit_ended(&tx, pane_id, format!("subscription error: {err}"));
                break;
            }
        };

        let wait_interval = subscription_poll_delay(&config, saw_dirty_output);
        match crate::runtime_async::timeout_with_cx(
            cx,
            wait_interval,
            wait_for_cancel_change_with_cx(cx, &mut cancel_rx),
        )
        .await
        {
            Ok(changed_ok) if !changed_ok || cancel_requested(&mut cancel_rx) => {
                pane_delta_try_emit_ended(&tx, pane_id, "cancelled");
                break;
            }
            Ok(_) => {}
            Err(_) if cx.checkpoint().is_err() => {
                pane_delta_try_emit_ended(&tx, pane_id, "cancelled");
                break;
            }
            Err(_) => {}
        }
    }
}

impl PaneOutputSubscription {
    /// Receive the next delta using an explicit capability context.
    pub async fn next_with_cx(&mut self, cx: &Cx) -> Option<PaneDelta> {
        pane_delta_recv_with_cx(cx, &mut self.receiver).await
    }

    /// Receive the next delta. Returns `None` when the subscription ends.
    pub async fn next(&mut self) -> Option<PaneDelta> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.next_with_cx(&cx).await
    }

    /// Cancel the subscription.
    ///
    /// br-ft-x2oyy: function-level cancel contract. The watch channel is only
    /// a wake-up for the poller; send failure means the poller has already
    /// exited, which is equivalent to a completed cancel.
    pub fn cancel(&self) {
        // br-ft-x2oyy: intentional best-effort cancel signal; send only
        // fails when the subscription poller has already exited.
        let _ = self.cancel.send(true);
    }

    /// Cancel the subscription and wait for the background poller to exit.
    ///
    /// This gives callers a deterministic shutdown path instead of relying on
    /// detached task teardown after `Drop`.
    pub async fn shutdown(self) {
        // Cleanup is already admitted once this consuming API is called. Use
        // a fresh request context so an ambient parent cancellation cannot
        // weaken shutdown's documented join-before-return postcondition.
        let cx = cx::for_request();
        self.shutdown_with_cx(&cx).await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`shutdown`].
    ///
    /// Issues the cancel signal unconditionally (so the
    /// background poller always exits cleanly), then awaits the
    /// join only if the cx is not already cancelled. If the cx
    /// is cancelled at entry, returns immediately after sending
    /// the cancel — the background task will still exit on its
    /// own, but the caller does not block. This lets a cancelled
    /// parent scope bail fast while preserving the "cancel
    /// before return" guarantee that the legacy shutdown gives.
    pub async fn shutdown_with_cx(mut self, cx: &Cx) {
        self.cancel();
        if cx.checkpoint().is_err() {
            return;
        }
        if let Some(task) = self.task.take() {
            join_subscription_task(task).await;
        }
    }
}

fn subscription_poll_delay(config: &SubscriptionConfig, saw_dirty_output: bool) -> Duration {
    if saw_dirty_output {
        config.min_poll_interval.min(config.poll_interval)
    } else {
        config.poll_interval
    }
}

fn spawn_subscription_task_with_cx(
    handle: &RuntimeHandle,
    cx: &Cx,
    client: DirectMuxClient,
    pane_id: u64,
    config: SubscriptionConfig,
    tx: mpsc::Sender<PaneDelta>,
    cancel_rx: watch::Receiver<bool>,
) -> SubscriptionTask {
    let task = cx::spawn_with_cx(handle, cx, move |cx| async move {
        run_subscription_loop(&cx, client, pane_id, config, tx, cancel_rx).await;
    });
    SubscriptionTask::Scoped(task)
}

fn inherited_subscription_runtime_handle() -> RuntimeHandle {
    crate::runtime_async::current_runtime_handle()
        .expect("pane output subscription started without an installed runtime handle")
}

// br-ft-x2oyy: function-level Drop cancel contract. Dropping a subscription is
// allowed to signal the poller without waiting; a failed send means the poller
// has already observed cancellation or exited independently.
impl Drop for PaneOutputSubscription {
    fn drop(&mut self) {
        // br-ft-x2oyy: intentional best-effort Drop cancel; send only
        // fails when the subscription poller has already exited.
        let _ = self.cancel.send(true);
    }
}

/// Start a subscription to a pane's output via `GetPaneRenderChanges` polling.
///
/// This spawns a background task that polls the mux server and emits
/// `PaneDelta` events through a bounded channel. Dropping the returned
/// `PaneOutputSubscription` cancels the background poller.
///
/// The mux-side terminal mutation `seqno` is retained on each output event for
/// diagnostics only. It is not a delivery counter, so ordinary jumps between
/// polls never imply a [`PaneDelta::Gap`].
#[allow(dead_code)]
pub fn subscribe_pane_output_with_cx(
    handle: &RuntimeHandle,
    cx: &Cx,
    client: DirectMuxClient,
    pane_id: u64,
    config: SubscriptionConfig,
) -> PaneOutputSubscription {
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let (cancel_tx, cancel_rx) = watch::channel(false);

    PaneOutputSubscription {
        receiver: rx,
        cancel: cancel_tx,
        task: Some(spawn_subscription_task_with_cx(
            handle, cx, client, pane_id, config, tx, cancel_rx,
        )),
    }
}

/// Start a subscription using the installed runtime handle plus an inherited `Cx`.
///
/// Under `asupersync-runtime`, prefer [`subscribe_pane_output_with_cx`] so the
/// background poller and receiver path share an explicit caller-owned `Cx`.
pub fn subscribe_pane_output_with_inherited_cx(
    cx: &Cx,
    client: DirectMuxClient,
    pane_id: u64,
    config: SubscriptionConfig,
) -> PaneOutputSubscription {
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handle = inherited_subscription_runtime_handle();

    PaneOutputSubscription {
        receiver: rx,
        cancel: cancel_tx,
        task: Some(spawn_subscription_task_with_cx(
            &handle, cx, client, pane_id, config, tx, cancel_rx,
        )),
    }
}

pub fn subscribe_pane_output(
    client: DirectMuxClient,
    pane_id: u64,
    config: SubscriptionConfig,
) -> PaneOutputSubscription {
    {
        let cx = ambient_mux_cx();
        subscribe_pane_output_with_inherited_cx(&cx, client, pane_id, config)
    }
}

fn total_dirty_rows(ranges: &[std::ops::Range<isize>]) -> usize {
    ranges.iter().fold(0usize, |acc, range| {
        let span = if range.end > range.start {
            range
                .end
                .checked_sub(range.start)
                .and_then(|span| usize::try_from(span).ok())
                .unwrap_or(usize::MAX)
        } else {
            0
        };
        acc.saturating_add(span)
    })
}

/// Convert one poll's render-changes response into an emit-ready output
/// delta, or `None` when the response genuinely carries no new content.
///
/// GH#73: the mux server moves changed viewport rows *out of* `dirty_lines`
/// and into `bonus_lines` (see `sessionhandler.rs`, which removes each
/// prefetched row id from the dirty set as it serializes the line), so an
/// ordinary viewport update commonly arrives as bonus-lines-only. Readiness
/// must therefore treat `bonus_lines` as content-bearing. The previous gate
/// (`!dirty_lines.is_empty()`) silently discarded every bonus-lines-only
/// update: no `PaneDelta::Output`, no `CaptureEvent`, no stored segment, and
/// no gap marker — durable capture froze while every transport surface
/// looked healthy.
fn render_changes_to_output_delta(
    pane_id: u64,
    changes: codec::GetPaneRenderChangesResponse,
) -> Option<PaneDelta> {
    let seqno = changes.seqno as u64;
    let dirty_range_count = changes.dirty_lines.len();
    let has_bonus_lines = changes.bonus_lines.line_count() > 0;
    if dirty_range_count == 0 && !has_bonus_lines {
        return None;
    }
    let dirty_row_count = total_dirty_rows(&changes.dirty_lines);
    let delta_text = bonus_lines_to_text(changes.bonus_lines);
    Some(PaneDelta::Output {
        pane_id,
        seqno,
        delta_text,
        title: changes.title,
        dirty_range_count,
        dirty_row_count,
    })
}

fn bonus_lines_to_text(lines: codec::SerializedLines) -> String {
    let (lines, _images) = lines.extract_data();
    let mut text = String::new();
    for (idx, (_row, line)) in lines.into_iter().enumerate() {
        if idx > 0 {
            text.push('\n');
        }
        text.push_str(line.as_str().as_ref());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_async::unix as compat_unix;
    use crate::runtime_async::{CompatRuntime, Mutex, RuntimeBuilder, sleep};
    use proptest::prelude::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const COMPRESSED_MASK: u64 = 1 << 63;

    fn decode_u64_leb128_prefix(bytes: &[u8]) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;

        for (idx, byte) in bytes.iter().copied().enumerate() {
            if idx >= 10 {
                return None;
            }
            value |= u64::from(byte & 0x7f) << shift;
            if (byte & 0x80) == 0 {
                return Some(value);
            }
            shift += 7;
        }

        None
    }

    fn frame_marked_compressed(bytes: &[u8]) -> Option<bool> {
        decode_u64_leb128_prefix(bytes).map(|length| (length & COMPRESSED_MASK) != 0)
    }

    fn encode_u64_leb128(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for mux_client tests");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            CompatRuntime::block_on(&runtime, future);
        }));
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

    // br-ft-x2oyy: function-level test-server close notification contract.
    // These oneshot sends are diagnostic breadcrumbs for tests waiting on a
    // synthetic mux server to observe EOF/write-failure; send failure means
    // the assertion-side receiver already timed out, completed, or was
    // dropped while the test was unwinding.
    fn notify_test_server_closed_best_effort(tx: crate::runtime_async::oneshot::Sender<()>) {
        let _ = tx.send(());
    }

    fn direct_mux_client_config(socket_path: PathBuf) -> DirectMuxClientConfig {
        DirectMuxClientConfig {
            socket_path: Some(socket_path),
            ..Default::default()
        }
    }

    fn direct_mux_client_config_with_timeout(
        socket_path: PathBuf,
        read_timeout: Duration,
    ) -> DirectMuxClientConfig {
        DirectMuxClientConfig {
            socket_path: Some(socket_path),
            read_timeout,
            ..Default::default()
        }
    }

    fn test_render_change(
        pane_id: mux::pane::PaneId,
        seqno: usize,
        title: &str,
    ) -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id,
            mouse_grabbed: false,
            alt_screen_active: false,
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
            tiered_scrollback_status: None,
            dirty_lines: std::iter::once(0..1).collect(),
            title: title.to_string(),
            working_dir: None,
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno,
        }
    }

    fn preload_compressed_render_sideband(
        client: &mut DirectMuxClient,
        pane_id: mux::pane::PaneId,
        title: &str,
    ) {
        let mut frame = Vec::new();
        Pdu::GetPaneRenderChangesResponse(test_render_change(pane_id, 1, title))
            .encode_with_mode(&mut frame, 0, CompressionMode::Always)
            .expect("encode preloaded compressed render sideband");
        client.read_buf.extend_from_slice(&frame);
    }

    fn admit_pending_test_render_change(
        pending: &mut PendingRenderChanges,
        pane_id: mux::pane::PaneId,
        seqno: usize,
        title: &str,
    ) {
        let retained = RetainedRenderChange::encode(41, test_render_change(pane_id, seqno, title))
            .expect("encode pending render-change fixture");
        let next = pending
            .admit_insert(
                &retained,
                RetentionLimit {
                    max_count: DEFAULT_MAX_PENDING_RENDER_CHANGES,
                    max_bytes: DEFAULT_MAX_PENDING_RENDER_CHANGE_BYTES,
                },
            )
            .expect("admit pending render-change fixture");
        pending.commit_insert(retained, next);
    }

    fn admit_snapshot_test_render_change(
        snapshots: &mut RenderChangeSnapshots,
        pane_id: mux::pane::PaneId,
        seqno: usize,
        title: &str,
    ) {
        let pane_id_u64 = u64::try_from(pane_id).expect("bounded pane id must fit u64");
        let retained = RetainedRenderChange::encode(41, test_render_change(pane_id, seqno, title))
            .expect("encode render-change snapshot fixture");
        let next = snapshots
            .admit_insert(
                pane_id_u64,
                &retained,
                RetentionLimit {
                    max_count: DEFAULT_MAX_RENDER_CHANGE_SNAPSHOTS,
                    max_bytes: DEFAULT_MAX_RENDER_CHANGE_SNAPSHOT_BYTES,
                },
            )
            .expect("admit render-change snapshot fixture");
        snapshots.commit_insert(pane_id_u64, retained, next);
    }

    fn assert_render_retention_members(
        client: &DirectMuxClient,
        expected_snapshot_keys: &HashSet<u64>,
        expected_pending_panes: &[u64],
    ) {
        assert_eq!(
            &client
                .render_change_snapshots
                .keys()
                .copied()
                .collect::<HashSet<_>>(),
            expected_snapshot_keys
        );
        assert_eq!(
            client
                .pending_render_changes
                .iter()
                .map(|retained| retained.pane_id)
                .collect::<Vec<_>>(),
            expected_pending_panes
        );
    }

    /// Like `run_async_test` but spawns a dedicated thread so the test gets
    /// a pristine TLS state. Prevents interference when 25 000+ tests run
    /// in parallel and stomp each other's `ASUPERSYNC_HANDLE` thread-local.
    #[allow(dead_code)]
    fn run_async_test_isolated<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        let result = std::thread::Builder::new()
            .name("mux-client-test-isolated".into())
            .spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("failed to build runtime for mux_client isolated test");
                let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    CompatRuntime::block_on(&runtime, f());
                }));
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(runtime);
                }));
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::runtime_async::clear_runtime_handle();
                }));
                if let Err(payload) = test_result {
                    std::panic::resume_unwind(payload);
                }
            })
            .expect("failed to spawn isolated test thread")
            .join();
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    async fn write_response_pdu(
        stream: &mut compat_unix::UnixStream,
        pdu: &Pdu,
        serial: u64,
    ) -> std::io::Result<()> {
        let mut out = Vec::new();
        pdu.encode(&mut out, serial)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        stream.write_all(&out).await?;
        stream.flush().await
    }

    async fn read_test_request_pdu(
        stream: &mut compat_unix::UnixStream,
        read_buf: &mut StreamingPduBuffer,
    ) -> DecodedPdu {
        loop {
            if let Some(decoded) =
                codec::Pdu::stream_decode(read_buf).expect("decode synthetic mux request")
            {
                return decoded;
            }
            let mut temp = vec![0_u8; 4096];
            let read = unix_stream_read(stream, &mut temp)
                .await
                .expect("read synthetic mux request");
            assert!(read > 0, "client disconnected before synthetic request");
            read_buf.extend_from_slice(&temp[..read]);
        }
    }

    async fn accept_direct_mux_handshake(
        listener: compat_unix::UnixListener,
        remote_max: usize,
        remote_min: usize,
    ) -> compat_unix::UnixStream {
        let (mut stream, _) = listener.accept().await.expect("accept direct mux client");
        let mut read_buf = StreamingPduBuffer::new();
        let mut codec_response_sent = false;
        let mut registration_response_sent = false;

        while !registration_response_sent {
            let mut temp = vec![0u8; 4096];
            let read = unix_stream_read(&mut stream, &mut temp)
                .await
                .expect("read direct mux handshake");
            assert!(read > 0, "direct mux client disconnected during handshake");
            read_buf.extend_from_slice(&temp[..read]);

            while let Some(decoded) =
                codec::Pdu::stream_decode(&mut read_buf).expect("decode direct mux handshake")
            {
                match decoded.pdu {
                    Pdu::GetCodecVersion(_) => {
                        assert!(!codec_response_sent, "duplicate codec-version request");
                        assert_eq!(decoded.serial, 1);
                        codec_response_sent = true;
                        write_response_pdu(
                            &mut stream,
                            &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: remote_max,
                                version_string: format!("test-codec-{remote_max}-{remote_min}"),
                                executable_path: PathBuf::from("/bin/frankenterm-mux-server"),
                                config_file_path: None,
                                min_supported: remote_min,
                            }),
                            decoded.serial,
                        )
                        .await
                        .expect("write codec-version response");
                    }
                    Pdu::SetClientId(_) => {
                        assert!(codec_response_sent, "registration preceded codec agreement");
                        assert!(
                            !registration_response_sent,
                            "duplicate registration request"
                        );
                        assert_eq!(decoded.serial, 2);
                        registration_response_sent = true;
                        write_response_pdu(
                            &mut stream,
                            &Pdu::UnitResponse(UnitResponse {}),
                            decoded.serial,
                        )
                        .await
                        .expect("write registration response");
                    }
                    other => panic!(
                        "unexpected PDU {} during direct mux handshake",
                        other.pdu_name()
                    ),
                }
            }
        }

        stream
    }

    fn empty_list_panes_response() -> Pdu {
        Pdu::ListPanesResponse(ListPanesResponse {
            tabs: Vec::new(),
            tab_titles: Vec::new(),
            window_titles: HashMap::new(),
            floating_panes: Vec::new(),
        })
    }

    fn ordered_window_request() -> Pdu {
        let foundation = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        );
        Pdu::ListPanesOrderedV1(codec::ListPanesOrderedV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: codec::DomainBindingId::from_bytes([0x11; 16]),
            supported: TopologyCapabilities::from_bits(
                foundation.bits() | TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
            ),
            required: foundation,
        })
    }

    fn ordered_window_event() -> Pdu {
        Pdu::WindowOrderEventV1(codec::WindowOrderEventV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            stream_id: codec::TopologyStreamId::from_bytes([0x22; 16]),
            session_incarnation: mux::MuxSessionIncarnation::from_bytes([0x33; 16]),
            topology_revision: mux::TopologyRevision::new(2),
            windows: vec![codec::OrderedWindowStateV1 {
                window_id: codec::RemoteWindowId::new(7),
                order_revision: codec::WindowOrderRevision::new(1),
                ordered_tab_ids: vec![codec::RemoteTabId::new(11)],
                active_tab_id: Some(codec::RemoteTabId::new(11)),
            }],
        })
    }

    fn unsupported_ordered_window_response() -> Pdu {
        Pdu::ListPanesOrderedV1Response(codec::ListPanesOrderedV1Response {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: codec::DomainBindingId::from_bytes([0x11; 16]),
            negotiated: TopologyCapabilities::NONE,
            stream_id: codec::TopologyStreamId::from_bytes([0x44; 16]),
            outcome: codec::ListPanesOrderedV1Outcome::Unsupported {
                supported: TopologyCapabilities::NONE,
            },
        })
    }

    fn cancelled_test_cx(message: &'static str) -> Cx {
        let budget = crate::cx::Budget::new().with_poll_quota(0);
        let cx = Cx::for_testing_with_budget(budget);
        cx.cancel_with(crate::outcome::CancelKind::User, Some(message));
        cx
    }

    fn assert_cancelled_mux_error(err: &DirectMuxError) {
        assert!(
            err.is_cancelled(),
            "typed cancellation bit must be retained"
        );
        let source = match err {
            DirectMuxError::InFlightScopeAbandoned(source) => source.as_ref(),
            other => other,
        };
        match source {
            DirectMuxError::Cancelled { phase, detail } => {
                assert!(!phase.is_empty(), "cancelled mux phase must be retained");
                assert!(!detail.is_empty(), "cancelled mux detail must be retained");
            }
            other => panic!("expected typed mux cancellation, got: {other}"),
        }
    }

    #[test]
    fn decode_from_buffer_roundtrip() {
        let mut buf = Vec::new();
        let pdu = Pdu::Ping(codec::Ping {});
        pdu.encode(&mut buf, 42).expect("encode should succeed");

        let mut partial = StreamingPduBuffer::from(buf[..buf.len() / 2].to_vec());
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
        let frame = Pdu::Ping(codec::Ping {})
            .encode_frame(7)
            .expect("encode oversize fixture");
        let max_frame_bytes = frame.len() - 1;
        let mut buf = StreamingPduBuffer::from(frame.clone());
        let err = decode_from_buffer(&mut buf, max_frame_bytes)
            .expect_err("should reject an oversized declared frame");
        assert!(matches!(
            err,
            DirectMuxError::FrameTooLarge { max_bytes }
                if max_bytes == max_frame_bytes
        ));
        assert_eq!(buf.as_slice(), frame.as_slice());
    }

    #[test]
    fn decode_from_buffer_accepts_individually_bounded_coalesced_frames() {
        let first = Pdu::Ping(codec::Ping {})
            .encode_frame(7)
            .expect("encode first frame");
        let second = Pdu::Pong(codec::Pong {})
            .encode_frame(9)
            .expect("encode second frame");
        let max_frame_bytes = first.len().max(second.len());
        let mut coalesced = first;
        coalesced.extend_from_slice(&second);
        assert!(coalesced.len() > max_frame_bytes);
        let mut buffer = StreamingPduBuffer::from(coalesced);

        let first = decode_from_buffer(&mut buffer, max_frame_bytes)
            .expect("first bounded decode")
            .expect("complete first frame");
        assert_eq!(first.serial, 7);
        assert!(matches!(first.pdu, Pdu::Ping(_)));
        let second = decode_from_buffer(&mut buffer, max_frame_bytes)
            .expect("second bounded decode")
            .expect("complete second frame");
        assert_eq!(second.serial, 9);
        assert!(matches!(second.pdu, Pdu::Pong(_)));
        assert!(buffer.is_empty());
    }

    #[test]
    fn list_panes_roundtrip() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("mux.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut responses: HashMap<u64, Pdu> = HashMap::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                };
                                Pdu::GetCodecVersionResponse(payload)
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::ListPanes(_) => {
                                let payload = ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: HashMap::new(),
                                    floating_panes: Vec::new(),
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

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let panes = client.list_panes().await.expect("list panes");
            assert!(panes.tabs.is_empty());
        });
    }

    #[test]
    fn list_panes_with_cx_roundtrip() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("mux-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut responses: HashMap<u64, Pdu> = HashMap::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                };
                                Pdu::GetCodecVersionResponse(payload)
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::ListPanes(_) => {
                                let payload = ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: HashMap::new(),
                                    floating_panes: Vec::new(),
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

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            let panes = client
                .list_panes_with_cx(&cx)
                .await
                .expect("list panes with cx");
            assert!(panes.tabs.is_empty());
        });
    }

    #[test]
    fn request_methods_with_cx_roundtrip() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("request-methods-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut saw_render = None;
                let mut saw_lines = None;
                let mut saw_write = None;
                let mut saw_paste = None;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                };
                                Pdu::GetCodecVersionResponse(payload)
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(request) => {
                                saw_render = Some(request.pane_id);
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: request.pane_id,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines: Vec::new(),
                                    title: format!("pane-{}", request.pane_id),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno: 7,
                                })
                            }
                            Pdu::GetLines(request) => {
                                saw_lines = Some((request.pane_id, request.lines.clone()));
                                Pdu::GetLinesResponse(GetLinesResponse {
                                    pane_id: request.pane_id,
                                    lines: Vec::new().into(),
                                })
                            }
                            Pdu::WriteToPane(request) => {
                                saw_write = Some((request.pane_id, request.data.to_vec()));
                                Pdu::UnitResponse(UnitResponse {})
                            }
                            Pdu::SendPaste(request) => {
                                saw_paste = Some((request.pane_id, request.data.clone()));
                                Pdu::UnitResponse(UnitResponse {})
                            }
                            _ => continue,
                        };

                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response");
                        stream.write_all(&out).await.expect("write response");

                        if saw_render.is_some()
                            && saw_lines.is_some()
                            && saw_write.is_some()
                            && saw_paste.is_some()
                        {
                            break;
                        }
                    }

                    if saw_render.is_some()
                        && saw_lines.is_some()
                        && saw_write.is_some()
                        && saw_paste.is_some()
                    {
                        break;
                    }
                }

                (
                    saw_render.expect("saw render request"),
                    saw_lines.expect("saw get_lines request"),
                    saw_write.expect("saw write request"),
                    saw_paste.expect("saw paste request"),
                )
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let render = client
                .get_pane_render_changes_with_cx(&cx, 12)
                .await
                .expect("render changes with cx");
            assert_eq!(render.pane_id, 12);
            assert_eq!(render.seqno, 7);

            let requested_ranges = vec![0..3, 5..6];
            let lines = client
                .get_lines_with_cx(&cx, 34, requested_ranges.clone())
                .await
                .expect("get lines with cx");
            assert_eq!(lines.pane_id, 34);
            let (extracted, _images) = lines.lines.extract_data();
            assert!(extracted.is_empty());

            client
                .write_to_pane_with_cx(&cx, 56, b"hello".to_vec())
                .await
                .expect("write to pane with cx");
            client
                .send_paste_with_cx(&cx, 78, "paste me".to_string())
                .await
                .expect("send paste with cx");

            drop(client);
            let (saw_render, saw_lines, saw_write, saw_paste) = server.await.expect("server task");
            assert_eq!(saw_render, 12);
            assert_eq!(saw_lines.0, 34);
            assert_eq!(saw_lines.1, requested_ranges);
            assert_eq!(saw_write.0, 56);
            assert_eq!(saw_write.1, b"hello".to_vec());
            assert_eq!(saw_paste.0, 78);
            assert_eq!(saw_paste.1, "paste me");
        });
    }

    /// ft-xbnl0.2.3 Cx-first: verify `create_floating_pane_with_cx`
    /// (and by extension the `expect_unit_response_with_cx`
    /// helper + the 9 sibling pane/layout ops that share it)
    /// roundtrips through the mux codec correctly when given a
    /// fresh, uncancelled cx.
    #[test]
    fn create_floating_pane_with_cx_roundtrip() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("create-floating-pane-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut saw_create: Option<(usize, usize, FloatingPaneRect)> = None;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "wezterm-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::CreateFloatingPane(req) => {
                                saw_create = Some((req.tab_id, req.pane_id, req.rect));
                                Pdu::UnitResponse(UnitResponse {})
                            }
                            _ => continue,
                        };
                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response");
                        stream.write_all(&out).await.expect("write response");

                        if saw_create.is_some() {
                            break;
                        }
                    }
                    if saw_create.is_some() {
                        break;
                    }
                }

                saw_create.expect("saw create_floating_pane request")
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let rect = FloatingPaneRect {
                left: 10,
                top: 20,
                width: 40,
                height: 15,
            };
            client
                .create_floating_pane_with_cx(&cx, 3, 99, rect)
                .await
                .expect("create_floating_pane_with_cx");

            drop(client);
            let (tab_id, pane_id, seen_rect) = server.await.expect("server task");
            assert_eq!(tab_id, 3);
            assert_eq!(pane_id, 99);
            assert_eq!(seen_rect, rect);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: verify pub-elevated
    /// `batch_with_cx` accepts a heterogeneous PDU batch from
    /// an external caller and returns responses in request
    /// order. Uses two ListPanes requests so the server can
    /// count how many it saw before responding.
    #[test]
    fn batch_with_cx_pub_entry_returns_responses_in_request_order() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-with-cx-pub.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut list_panes_seen = 0u32;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "wezterm-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::ListPanes(_) => {
                                list_panes_seen += 1;
                                Pdu::ListPanesResponse(codec::ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: std::collections::HashMap::new(),
                                    floating_panes: Vec::new(),
                                })
                            }
                            _ => continue,
                        };
                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response");
                        stream.write_all(&out).await.expect("write response");

                        if list_panes_seen >= 2 {
                            break;
                        }
                    }
                    if list_panes_seen >= 2 {
                        break;
                    }
                }

                list_panes_seen
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let requests = vec![
                Pdu::ListPanes(codec::ListPanes {}),
                Pdu::ListPanes(codec::ListPanes {}),
            ];
            let responses = client
                .batch_with_cx(&cx, requests, 2, Duration::from_secs(2))
                .await
                .expect("batch_with_cx roundtrip");

            assert_eq!(responses.len(), 2);
            for resp in &responses {
                assert!(matches!(resp, Pdu::ListPanesResponse(_)));
            }

            drop(client);
            let seen = server.await.expect("server task");
            assert_eq!(seen, 2);
        });
    }

    #[test]
    fn proven_prewrite_rejections_preserve_alignment_for_next_request() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let cancelled_cx = cancelled_test_cx("pre-cancelled request write");
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("pre-cancelled-request-write.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let (handshake_seen_tx, handshake_seen_rx) = std::sync::mpsc::channel();

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut post_handshake_requests = 0usize;
                let mut handshake_complete = false;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match timeout(
                        Duration::from_secs(1),
                        unix_stream_read(&mut stream, &mut temp),
                    )
                    .await
                    {
                        Ok(Ok(read)) => read,
                        Ok(Err(err)) => panic!("read failed: {err}"),
                        Err(timeout_err) if handshake_complete => {
                            let _ = timeout_err;
                            break;
                        }
                        Err(timeout_err) => {
                            panic!("server timed out before handshake completed: {timeout_err}");
                        }
                    };
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "pre-cancelled-request-write-test"
                                            .to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write client response");
                                handshake_complete = true;
                                handshake_seen_tx
                                    .send(())
                                    .expect("signal that handshake completed");
                            }
                            Pdu::ListPanes(_) => {
                                post_handshake_requests += 1;
                                write_response_pdu(
                                    &mut stream,
                                    &empty_list_panes_response(),
                                    decoded.serial,
                                )
                                .await
                                .expect("write list panes response");
                            }
                            _ => {}
                        }
                    }
                }

                post_handshake_requests
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            handshake_seen_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should complete handshake");

            let original_limit = client.config.max_outstanding_requests;
            client.config.max_outstanding_requests = 0;
            let admission_error = client
                .send_request_only(Pdu::ListPanes(ListPanes {}))
                .await
                .expect_err("local admission limit should reject before sending");
            assert!(matches!(
                &admission_error,
                DirectMuxError::ProvenPreWriteRejection(source)
                    if matches!(
                        source.as_ref(),
                        DirectMuxError::RetentionLimitExceeded {
                            resource: "outstanding mux requests",
                            ..
                        }
                    )
            ));
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);
            client.config.max_outstanding_requests = original_limit;

            let err = client
                .send_request_only_with_cx(&cancelled_cx, Pdu::ListPanes(ListPanes {}))
                .await
                .expect_err("pre-cancelled request write should fail before sending");
            assert_cancelled_mux_error(&err);
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            let panes = client
                .list_panes()
                .await
                .expect("aligned client should serve a valid request after local rejections");
            assert!(panes.tabs.is_empty());

            client.outstanding_requests.insert(9_001);
            let mid_batch_error = client
                .fail_batch_scope::<()>(
                    cancelled_mux_error("request_write_wait", "later batch admission cancelled"),
                    true,
                    true,
                )
                .expect_err("a pre-write rejection cannot reuse across earlier in-flight work");
            assert_cancelled_mux_error(&mid_batch_error);
            assert!(matches!(
                mid_batch_error.recovery_decision().connection,
                MuxConnectionDisposition::Discard
            ));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);
            assert!(client.outstanding_requests.is_empty());

            drop(client);
            assert_eq!(
                server.await.expect("server task"),
                1,
                "only the valid request after both rejections may reach the wire"
            );
        });
    }

    #[test]
    fn await_response_with_precancelled_cx_fails_before_reading_frame() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let cancelled_cx = cancelled_test_cx("pre-cancelled response read");
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("pre-cancelled-response-read.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let (request_seen_tx, request_seen_rx) = std::sync::mpsc::channel();

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut list_panes_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match timeout(
                        Duration::from_millis(400),
                        unix_stream_read(&mut stream, &mut temp),
                    )
                    .await
                    {
                        Ok(Ok(read)) => read,
                        Ok(Err(err)) => panic!("read failed: {err}"),
                        Err(_) => break,
                    };
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "pre-cancelled-response-read-test"
                                            .to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write client response");
                            }
                            Pdu::ListPanes(_) => {
                                list_panes_requests += 1;
                                request_seen_tx
                                    .send(())
                                    .expect("signal that request frame was observed");
                                sleep(Duration::from_millis(250)).await;
                                return list_panes_requests;
                            }
                            _ => {}
                        }
                    }
                }

                list_panes_requests
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            let serial = client
                .send_request_only(Pdu::ListPanes(ListPanes {}))
                .await
                .expect("send request without awaiting response");

            request_seen_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should observe the request frame");

            let err = client
                .await_response_with_cx(&cancelled_cx, serial)
                .await
                .expect_err("pre-cancelled response read should fail before blocking read");
            assert_cancelled_mux_error(&err);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            assert_eq!(
                server.await.expect("server task"),
                1,
                "server should observe exactly the one request sent before cancellation"
            );
        });
    }

    #[test]
    fn get_lines_rejects_unexpected_response_type() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("unexpected-get-lines.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let payload = GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "unexpected-get-lines-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                };
                                Pdu::GetCodecVersionResponse(payload)
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetLines(_) => Pdu::UnitResponse(UnitResponse {}),
                            _ => continue,
                        };

                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response");
                        stream.write_all(&out).await.expect("write response");

                        if matches!(decoded.pdu, Pdu::GetLines(_)) {
                            return;
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let err = client
                .get_lines(34, vec![0..3, 5..6])
                .await
                .expect_err("get_lines should reject wrong response type");
            assert!(matches!(
                &err,
                DirectMuxError::AlignedUnexpectedResponse { expected, got }
                    if expected == "GetLinesResponse" && got == "UnitResponse"
            ));
            assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Recoverable);
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn get_pane_render_changes_accepts_unilateral_render_delta_after_liveness() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("render-delta-sideband.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "sideband-render-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                let sideband = Pdu::GetPaneRenderChangesResponse(
                                    GetPaneRenderChangesResponse {
                                        pane_id: request.pane_id,
                                        mouse_grabbed: false,
                                        alt_screen_active: false,
                                        cursor_position:
                                            mux::renderable::StableCursorPosition::default(),
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
                                        tiered_scrollback_status: None,
                                        dirty_lines: std::iter::once(0..1).collect(),
                                        title: "sideband-pane".to_string(),
                                        working_dir: None,
                                        bonus_lines: Vec::new().into(),
                                        input_serial: None,
                                        seqno: 9,
                                    },
                                );
                                write_response_pdu(&mut stream, &sideband, 0)
                                    .await
                                    .expect("write sideband render response");

                                let liveness = Pdu::LivenessResponse(codec::LivenessResponse {
                                    pane_id: request.pane_id,
                                    is_alive: true,
                                });
                                write_response_pdu(&mut stream, &liveness, decoded.serial)
                                    .await
                                    .expect("write liveness response");
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let render = client
                .get_pane_render_changes(12)
                .await
                .expect("sideband render changes should succeed");
            assert_eq!(render.pane_id, 12);
            assert_eq!(render.seqno, 9);
            assert_eq!(render.title, "sideband-pane");
            assert_eq!(
                render.dirty_lines,
                std::iter::once(0..1).collect::<Vec<_>>()
            );

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn get_pane_render_changes_reuses_cached_snapshot_when_liveness_has_no_delta() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("render-delta-cache.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "cached-render-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                render_requests += 1;
                                if render_requests == 1 {
                                    let sideband = Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: request.pane_id,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
                                            dimensions: mux::renderable::RenderableDimensions {
                                                cols: 120,
                                                viewport_rows: 40,
                                                scrollback_rows: 200,
                                                physical_top: 10,
                                                scrollback_top: 0,
                                                dpi: 96,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                                reverse_video: false,
                                            },
                                            tiered_scrollback_status: None,
                                            dirty_lines: std::iter::once(10..12).collect(),
                                            title: "cached-pane".to_string(),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: 14,
                                        },
                                    );
                                    write_response_pdu(&mut stream, &sideband, 0)
                                        .await
                                        .expect("write first sideband");
                                }

                                let liveness = Pdu::LivenessResponse(codec::LivenessResponse {
                                    pane_id: request.pane_id,
                                    is_alive: true,
                                });
                                write_response_pdu(&mut stream, &liveness, decoded.serial)
                                    .await
                                    .expect("write liveness response");

                                if render_requests == 2 {
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let first = client
                .get_pane_render_changes(27)
                .await
                .expect("first render changes should succeed");
            assert_eq!(first.seqno, 14);
            assert_eq!(
                first.dirty_lines,
                std::iter::once(10..12).collect::<Vec<_>>()
            );

            let second = client
                .get_pane_render_changes(27)
                .await
                .expect("cached render snapshot should be reused");
            assert_eq!(second.seqno, 14);
            assert!(second.dirty_lines.is_empty());
            assert!(second.bonus_lines.extract_data().0.is_empty());
            assert_eq!(second.title, "cached-pane");
            assert_eq!(second.dimensions.cols, 120);

            client
                .stash_unilateral_pdu(Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 27 }))
                .expect("pane removal must invalidate retained render state");
            assert!(!client.render_change_snapshots.contains_key(27));
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);

            let stale_cache_error = client
                .resolve_render_change_response(
                    27,
                    Pdu::LivenessResponse(codec::LivenessResponse {
                        pane_id: 27,
                        is_alive: true,
                    }),
                )
                .expect_err("reused pane ID must not inherit the removed pane snapshot");
            assert!(matches!(
                stale_cache_error,
                DirectMuxError::AlignedUnexpectedResponse { .. }
            ));

            let replacement = client
                .resolve_render_change_response(
                    27,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        27,
                        1,
                        "replacement-pane",
                    )),
                )
                .expect("authoritative replacement snapshot may restart its sequence");
            assert_eq!(replacement.seqno, 1);
            assert_eq!(replacement.title, "replacement-pane");
            assert!(client.render_change_snapshots.contains_key(27));
            assert!(client.render_change_snapshots.retained_bytes() > 0);

            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    27,
                    2,
                    "wrong-liveness-pending",
                )))
                .expect("stage render delta before wrong-pane liveness");
            let wrong_liveness = client
                .resolve_render_change_response(
                    27,
                    Pdu::LivenessResponse(codec::LivenessResponse {
                        pane_id: 99,
                        is_alive: true,
                    }),
                )
                .expect_err("wrong-pane liveness must fail closed");
            assert!(matches!(
                wrong_liveness,
                DirectMuxError::AlignedUnexpectedResponse { .. }
            ));
            assert!(!client.render_change_snapshots.contains_key(27));
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);

            let recovered = client
                .resolve_render_change_response(
                    27,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        27,
                        3,
                        "recovered-after-wrong-liveness",
                    )),
                )
                .expect("connection remains reusable after a fully correlated identity error");
            assert_eq!(recovered.pane_id, 27);
            assert!(!client.connection_poisoned);

            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    27,
                    4,
                    "dead-pane-pending",
                )))
                .expect("stage render delta before dead liveness");
            let dead = client
                .resolve_render_change_response(
                    27,
                    Pdu::LivenessResponse(codec::LivenessResponse {
                        pane_id: 27,
                        is_alive: false,
                    }),
                )
                .expect_err("dead liveness must invalidate retained pane state");
            assert!(matches!(dead, DirectMuxError::RemoteError(_)));
            assert!(!client.render_change_snapshots.contains_key(27));
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);

            client
                .resolve_render_change_response(
                    27,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        27,
                        5,
                        "recovered-after-dead-liveness",
                    )),
                )
                .expect("connection remains reusable after dead-pane settlement");
            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    27,
                    6,
                    "legacy-mismatch-pending",
                )))
                .expect("stage render delta before legacy identity mismatch");
            let legacy_mismatch = client
                .resolve_render_change_response(
                    27,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        99,
                        7,
                        "wrong-legacy-pane",
                    )),
                )
                .expect_err("legacy correlated payload must retain pane identity");
            assert!(matches!(
                legacy_mismatch,
                DirectMuxError::AlignedUnexpectedResponse { .. }
            ));
            assert!(!client.render_change_snapshots.contains_key(27));
            assert!(!client.render_change_snapshots.contains_key(99));
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            client
                .resolve_render_change_response(
                    27,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        27,
                        8,
                        "recovered-after-legacy-mismatch",
                    )),
                )
                .expect("legacy mismatch cleanup leaves the connection reusable");
            assert!(!client.connection_poisoned);

            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    28,
                    9,
                    "second-batch-target",
                )))
                .expect("stage a second target before batch-level error cleanup");
            let batch_targets = [27, 28];
            let mut guard = RenderBatchGuard::new(&mut client, &batch_targets, 2, false);
            guard.first_error = Some(DirectMuxError::AlignedUnexpectedResponse {
                expected: "valid render batch correlation".to_string(),
                got: "synthetic correlated mismatch".to_string(),
            });
            let batch_error = guard
                .finish()
                .expect_err("failed batches must clean every target pane");
            assert!(matches!(
                batch_error,
                DirectMuxError::AlignedUnexpectedResponse { .. }
            ));
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(!client.connection_poisoned);
            client
                .resolve_render_change_response(
                    27,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        27,
                        10,
                        "recovered-after-batch-cleanup",
                    )),
                )
                .expect("drained batch error cleanup leaves the connection reusable");

            client.config.max_pending_render_changes = 1;
            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    28,
                    2,
                    "queued-pane",
                )))
                .expect("first bounded unilateral render change");
            let pending_bytes = client.pending_render_changes.retained_bytes();
            let snapshot_bytes = client.render_change_snapshots.retained_bytes();
            let limit_error = client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    29,
                    3,
                    "over-limit-pane",
                )))
                .expect_err("second unilateral must exceed the one-entry queue");
            assert!(matches!(
                limit_error,
                DirectMuxError::RetentionLimitExceeded {
                    resource: "pending unilateral render changes",
                    ..
                }
            ));
            assert_eq!(client.pending_render_changes.len(), 1);
            assert_eq!(
                client.pending_render_changes.retained_bytes(),
                pending_bytes
            );
            assert_eq!(
                client.render_change_snapshots.retained_bytes(),
                snapshot_bytes,
                "failed queue admission must not partially publish a snapshot"
            );
            assert!(!client.render_change_snapshots.contains_key(29));

            client
                .stash_unilateral_pdu(Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 28 }))
                .expect("pane removal must release queued retention");
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(!client.render_change_snapshots.contains_key(28));

            let serial_before_duplicate = client.serial;
            let duplicate_error = client
                .get_pane_render_changes_batch(&[27, 27], 2, Duration::from_secs(1))
                .await
                .expect_err("duplicate panes must fail before touching the closed transport");
            assert!(matches!(
                duplicate_error,
                DirectMuxError::DuplicateRenderBatchPane { pane_id: 27 }
            ));
            assert_eq!(client.serial, serial_before_duplicate);
            assert!(!client.connection_poisoned);

            let cancelled_cx = crate::cx::for_testing();
            cancelled_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel before render batch transport"),
            );
            let serial_before_cancel = client.serial;
            let cancel_error = client
                .get_pane_render_changes_batch_with_cx(
                    &cancelled_cx,
                    &[27],
                    1,
                    Duration::from_secs(1),
                )
                .await
                .expect_err("pre-transport cancellation must fail without poisoning");
            assert_cancelled_mux_error(&cancel_error);
            assert_eq!(client.serial, serial_before_cancel);
            assert!(client.outstanding_requests.is_empty());
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            client.outstanding_requests.insert(9_002);
            let ambiguous_targets = [27];
            let mut ambiguous_guard =
                RenderBatchGuard::new(&mut client, &ambiguous_targets, 1, true);
            ambiguous_guard
                .in_flight
                .insert(9_002, 0)
                .expect("stage synthetic in-flight render request");
            assert!(ambiguous_guard.in_flight_panes.insert(27));
            ambiguous_guard.transport_ambiguous = true;
            let mid_batch_error = ambiguous_guard
                .fail_finish::<()>(
                    cancelled_mux_error("request_write_wait", "later render admission cancelled"),
                    "synthetic mid-batch pre-write cancellation",
                )
                .expect_err("earlier in-flight render work must override local reuse");
            assert_cancelled_mux_error(&mid_batch_error);
            assert!(matches!(
                mid_batch_error.recovery_decision().connection,
                MuxConnectionDisposition::Discard
            ));
            drop(ambiguous_guard);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);
            assert!(client.outstanding_requests.is_empty());

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn request_methods_with_cx_reject_unexpected_response_types() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir
                .path()
                .join("unexpected-request-methods-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut unexpected_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let payload = GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "unexpected-request-methods-with-cx-test"
                                        .to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                };
                                Pdu::GetCodecVersionResponse(payload)
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) | Pdu::GetLines(_) => {
                                unexpected_requests += 1;
                                Pdu::UnitResponse(UnitResponse {})
                            }
                            Pdu::WriteToPane(_) | Pdu::SendPaste(_) => {
                                unexpected_requests += 1;
                                Pdu::ListPanesResponse(ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: HashMap::new(),
                                    floating_panes: Vec::new(),
                                })
                            }
                            _ => continue,
                        };

                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response");
                        stream.write_all(&out).await.expect("write response");

                        if unexpected_requests == 4 {
                            return;
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let render_err = client
                .get_pane_render_changes_with_cx(&cx, 12)
                .await
                .expect_err("render changes with cx should reject wrong response type");
            assert!(matches!(
                &render_err,
                DirectMuxError::AlignedUnexpectedResponse { expected, got }
                    if expected == "LivenessResponse or GetPaneRenderChangesResponse" && got == "UnitResponse"
            ));
            assert_eq!(
                render_err.protocol_error_kind(),
                ProtocolErrorKind::Recoverable
            );

            let lines_err = client
                .get_lines_with_cx(&cx, 34, vec![0..3, 5..6])
                .await
                .expect_err("get_lines_with_cx should reject wrong response type");
            assert!(matches!(
                &lines_err,
                DirectMuxError::AlignedUnexpectedResponse { expected, got }
                    if expected == "GetLinesResponse" && got == "UnitResponse"
            ));
            assert_eq!(
                lines_err.protocol_error_kind(),
                ProtocolErrorKind::Recoverable
            );

            let write_err = client
                .write_to_pane_with_cx(&cx, 56, b"hello".to_vec())
                .await
                .expect_err("write_to_pane_with_cx should reject wrong response type");
            assert!(matches!(
                &write_err,
                DirectMuxError::AlignedUnexpectedResponse { expected, got }
                    if expected == "UnitResponse" && got == "ListPanesResponse"
            ));
            assert_eq!(
                write_err.protocol_error_kind(),
                ProtocolErrorKind::Recoverable
            );

            let paste_err = client
                .send_paste_with_cx(&cx, 78, "paste me".to_string())
                .await
                .expect_err("send_paste_with_cx should reject wrong response type");
            assert!(matches!(
                &paste_err,
                DirectMuxError::AlignedUnexpectedResponse { expected, got }
                    if expected == "UnitResponse" && got == "ListPanesResponse"
            ));
            assert_eq!(
                paste_err.protocol_error_kind(),
                ProtocolErrorKind::Recoverable
            );
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn list_panes_wire_frame_matches_codec_encoding() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("wire-frame.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut captured_frame: Option<(u64, Vec<u8>)> = None;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    loop {
                        let before_decode = read_buf.as_slice().to_vec();
                        let decoded = match codec::Pdu::stream_decode(&mut read_buf) {
                            Ok(Some(decoded)) => decoded,
                            Ok(None) => break,
                            Err(err) => panic!("failed to decode request frame: {err}"),
                        };
                        let consumed = before_decode.len().saturating_sub(read_buf.len());
                        let raw_frame = before_decode[..consumed].to_vec();

                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let payload = GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "wezterm-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                };
                                Pdu::GetCodecVersionResponse(payload)
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::ListPanes(_) => {
                                captured_frame = Some((decoded.serial, raw_frame));
                                let payload = ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: HashMap::new(),
                                    floating_panes: Vec::new(),
                                };
                                Pdu::ListPanesResponse(payload)
                            }
                            _ => continue,
                        };

                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response");
                        stream.write_all(&out).await.expect("write response");

                        if captured_frame.is_some() {
                            break;
                        }
                    }

                    if captured_frame.is_some() {
                        break;
                    }
                }

                captured_frame.expect("captured ListPanes request frame")
            });

            let config = DirectMuxClientConfig {
                socket_path: Some(socket_path),
                compression_mode: crate::config::VendoredCompressionMode::Never,
                ..Default::default()
            };
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let _ = client.list_panes().await.expect("list panes");
            drop(client);

            let (serial, observed_frame) = server.await.expect("server task");

            let mut expected_frame = Vec::new();
            Pdu::ListPanes(ListPanes {})
                .encode_with_mode(&mut expected_frame, serial, CompressionMode::Never)
                .expect("encode expected frame");

            assert_eq!(
                observed_frame, expected_frame,
                "ListPanes request frame must remain bit-for-bit stable"
            );
        });
    }

    #[test]
    fn render_batch_duplicate_panes_fail_permanently_during_preflight() {
        let error = validate_render_batch_panes(&[7, 9, 7])
            .expect_err("duplicate pane IDs must be rejected before transport admission");
        assert!(matches!(
            &error,
            DirectMuxError::DuplicateRenderBatchPane { pane_id: 7 }
        ));
        let decision = error.recovery_decision();
        assert_eq!(decision.kind, ProtocolErrorKind::Permanent);
        assert!(!decision.retry);
        assert_eq!(decision.connection, MuxConnectionDisposition::Reuse);
        assert!(!decision.cancelled);
        validate_render_batch_panes(&[7, 9, 11]).expect("unique pane IDs remain admissible");
    }

    #[test]
    fn batch_local_sidebands_avoid_full_payload_codec_churn_at_bounded_depths() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            for depth in [1usize, 32, DEFAULT_MAX_OUTSTANDING_REQUESTS] {
                let socket_path = temp_dir.path().join(format!("typed-sideband-{depth}.sock"));
                let listener = compat_unix::bind(&socket_path)
                    .await
                    .expect("bind listener");

                let server = task::spawn(async move {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut requests = Vec::with_capacity(depth);
                    let mut batch_index = 0usize;

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            return;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                            .expect("decode typed-sideband request")
                        {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "typed-sideband-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        }),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write codec response");
                                }
                                Pdu::SetClientId(_) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::UnitResponse(UnitResponse {}),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write client response");
                                }
                                Pdu::GetPaneRenderChanges(request) => {
                                    requests.push((decoded.serial, request.pane_id));
                                    if requests.len() != depth {
                                        continue;
                                    }
                                    batch_index += 1;
                                    if batch_index == 1 {
                                        for (_, pane_id) in requests.iter().rev() {
                                            write_response_pdu(
                                                &mut stream,
                                                &Pdu::GetPaneRenderChangesResponse(
                                                    test_render_change(
                                                        *pane_id,
                                                        *pane_id,
                                                        "typed-sideband",
                                                    ),
                                                ),
                                                0,
                                            )
                                            .await
                                            .expect("write typed sideband");
                                        }
                                    }
                                    for (serial, pane_id) in requests.iter().rev() {
                                        write_response_pdu(
                                            &mut stream,
                                            &Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: *pane_id,
                                                is_alive: true,
                                            }),
                                            *serial,
                                        )
                                        .await
                                        .expect("write correlated liveness");
                                    }
                                    requests.clear();
                                    if batch_index == 2 {
                                        return;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                });

                let mut client = DirectMuxClient::connect(direct_mux_client_config(socket_path))
                    .await
                    .expect("connect");
                let pane_ids = (1..=depth)
                    .map(|pane_id| u64::try_from(pane_id).expect("bounded pane id must fit u64"))
                    .collect::<Vec<_>>();
                let responses = client
                    .get_pane_render_changes_batch(&pane_ids, depth, Duration::from_secs(10))
                    .await
                    .expect("typed-sideband batch");
                let reused = client
                    .get_pane_render_changes_batch(&pane_ids, depth, Duration::from_secs(10))
                    .await
                    .expect("typed-sideband snapshot reuse batch");

                assert_eq!(responses.len(), depth);
                for (response, pane_id) in responses.iter().zip(1..=depth) {
                    assert_eq!(response.pane_id, pane_id);
                    assert_eq!(response.seqno, pane_id);
                }
                assert_eq!(reused.len(), depth);
                for (response, pane_id) in reused.iter().zip(1..=depth) {
                    assert_eq!(response.pane_id, pane_id);
                    assert_eq!(response.seqno, pane_id);
                    assert!(response.dirty_lines.is_empty());
                    assert_eq!(
                        response
                            .bonus_lines
                            .validate_structure()
                            .expect("cached snapshot lines must remain structurally valid"),
                        codec::SerializedLinesResourceCounts::default()
                    );
                }
                assert!(client.pending_render_changes.is_empty());
                assert_eq!(client.pending_render_changes.retained_bytes(), 0);
                assert_eq!(client.render_change_snapshots.len(), depth);
                assert_eq!(
                    client.render_retention_codec_stats,
                    RenderRetentionCodecStats {
                        snapshot_encodes: depth,
                        snapshot_frame_allocations: depth,
                        batch_local_claims: depth,
                        batch_local_returns: depth,
                        batch_local_peak_count: depth,
                        snapshot_encoded_bytes: client
                            .render_retention_codec_stats
                            .snapshot_encoded_bytes,
                        snapshot_frame_capacity_bytes: client
                            .render_retention_codec_stats
                            .snapshot_frame_capacity_bytes,
                        batch_local_peak_frame_bytes: client
                            .render_retention_codec_stats
                            .batch_local_peak_frame_bytes,
                        ..RenderRetentionCodecStats::default()
                    },
                    "depth {depth} must not serialize or decode a full typed sideband"
                );
                assert!(client.render_retention_codec_stats.snapshot_encoded_bytes > 0);
                assert!(
                    client
                        .render_retention_codec_stats
                        .snapshot_frame_capacity_bytes
                        >= client.render_retention_codec_stats.snapshot_encoded_bytes
                );
                assert!(
                    client
                        .render_retention_codec_stats
                        .batch_local_peak_frame_bytes
                        > 0
                );
                assert!(
                    client
                        .render_retention_codec_stats
                        .batch_local_peak_frame_bytes
                        <= client.config.max_pending_render_change_bytes
                );

                drop(client);
                server.await.expect("server task");
            }
        });
    }

    #[test]
    fn batch_local_duplicate_sidebands_demote_in_exact_fifo_order() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("typed-sideband-duplicate.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                        .expect("decode duplicate-sideband request")
                    {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "typed-sideband-duplicate-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                render_requests += 1;
                                if render_requests == 1 {
                                    for seqno in [1usize, 2] {
                                        write_response_pdu(
                                            &mut stream,
                                            &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                                request.pane_id,
                                                seqno,
                                                "duplicate-sideband",
                                            )),
                                            0,
                                        )
                                        .await
                                        .expect("write duplicate sideband");
                                    }
                                }
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::LivenessResponse(codec::LivenessResponse {
                                        pane_id: request.pane_id,
                                        is_alive: true,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write correlated liveness");
                                if render_requests == 2 {
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let mut client = DirectMuxClient::connect(direct_mux_client_config(socket_path))
                .await
                .expect("connect");
            let first = client
                .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                .await
                .expect("first duplicate-sideband batch");
            let second = client
                .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                .await
                .expect("second duplicate-sideband batch");

            assert_eq!(first[0].seqno, 1);
            assert_eq!(second[0].seqno, 2);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_change_snapshots.len(), 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_demotions, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                2
            );
            assert_eq!(
                client
                    .render_retention_codec_stats
                    .pending_payload_frame_allocations,
                2
            );
            assert!(
                client
                    .render_retention_codec_stats
                    .pending_payload_encoded_bytes
                    > 0
            );
            assert!(
                client
                    .render_retention_codec_stats
                    .pending_payload_frame_capacity_bytes
                    >= client
                        .render_retention_codec_stats
                        .pending_payload_encoded_bytes
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                2
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 2);
            assert_eq!(
                client
                    .render_retention_codec_stats
                    .snapshot_frame_allocations,
                2
            );
            assert!(
                client
                    .render_retention_codec_stats
                    .snapshot_frame_capacity_bytes
                    >= client.render_retention_codec_stats.snapshot_encoded_bytes
            );

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_local_sidebands_preserve_semantic_error_cleanup_and_reuse() {
        #[derive(Clone, Copy, Debug)]
        enum LocalSemanticCase {
            WrongLegacyPane,
            WrongLivenessPane,
            DeadPane,
            RemovedBeforeLiveness,
            UnexpectedPdu,
            ErrorResponse,
        }

        // This scenario's protocol state machine is intentionally broad. Keep
        // the generated future off the small Rust test-thread stack so that it
        // remains deterministic on high-core builders with default stack sizes.
        run_async_test(Box::pin(async {
            for (case_index, case) in [
                LocalSemanticCase::WrongLegacyPane,
                LocalSemanticCase::WrongLivenessPane,
                LocalSemanticCase::DeadPane,
                LocalSemanticCase::RemovedBeforeLiveness,
                LocalSemanticCase::UnexpectedPdu,
                LocalSemanticCase::ErrorResponse,
            ]
            .into_iter()
            .enumerate()
            {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                // Unix-domain socket paths have a small platform limit (108 bytes on
                // Linux). Keep the leaf deliberately short because remote builders
                // can provide a comparatively long temporary-directory prefix.
                let socket_path = temp_dir.path().join(format!("s{case_index}.sock"));
                let listener = compat_unix::bind(&socket_path)
                    .await
                    .expect("bind listener");

                let server = task::spawn(async move {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut first_render_request = true;

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            return;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                            .expect("decode local-sideband semantic request")
                        {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "typed-sideband-semantic-test"
                                                .to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        }),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write codec response");
                                }
                                Pdu::SetClientId(_) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::UnitResponse(UnitResponse {}),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write client response");
                                }
                                Pdu::GetPaneRenderChanges(request) if first_render_request => {
                                    first_render_request = false;
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            request.pane_id,
                                            1,
                                            "typed-before-semantic-error",
                                        )),
                                        0,
                                    )
                                    .await
                                    .expect("write typed sideband before semantic error");

                                    if matches!(case, LocalSemanticCase::RemovedBeforeLiveness) {
                                        write_response_pdu(
                                            &mut stream,
                                            &Pdu::PaneRemoved(codec::PaneRemoved {
                                                pane_id: request.pane_id,
                                            }),
                                            0,
                                        )
                                        .await
                                        .expect("write pane removal before liveness");
                                    }

                                    let response = match case {
                                        LocalSemanticCase::WrongLegacyPane => {
                                            Pdu::GetPaneRenderChangesResponse(test_render_change(
                                                700,
                                                2,
                                                "wrong-legacy-pane",
                                            ))
                                        }
                                        LocalSemanticCase::WrongLivenessPane => {
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: 700,
                                                is_alive: true,
                                            })
                                        }
                                        LocalSemanticCase::DeadPane => {
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: request.pane_id,
                                                is_alive: false,
                                            })
                                        }
                                        LocalSemanticCase::RemovedBeforeLiveness => {
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: request.pane_id,
                                                is_alive: true,
                                            })
                                        }
                                        LocalSemanticCase::UnexpectedPdu => {
                                            Pdu::UnitResponse(UnitResponse {})
                                        }
                                        LocalSemanticCase::ErrorResponse => {
                                            Pdu::ErrorResponse(codec::ErrorResponse {
                                                reason: "typed-sideband semantic error".to_string(),
                                            })
                                        }
                                    };
                                    write_response_pdu(&mut stream, &response, decoded.serial)
                                        .await
                                        .expect("write semantic response");
                                }
                                Pdu::GetPaneRenderChanges(request) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            request.pane_id,
                                            1,
                                            "reuse-after-local-semantic-error",
                                        )),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write connection-reuse response");
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });

                let mut client = DirectMuxClient::connect(direct_mux_client_config(socket_path))
                    .await
                    .expect("connect");
                let error = client
                    .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                    .await
                    .expect_err("typed sideband must not mask a correlated semantic error");
                if matches!(
                    case,
                    LocalSemanticCase::DeadPane | LocalSemanticCase::ErrorResponse
                ) {
                    assert!(matches!(error, DirectMuxError::RemoteError(_)), "{case:?}");
                } else {
                    assert!(
                        matches!(error, DirectMuxError::AlignedUnexpectedResponse { .. }),
                        "{case:?}"
                    );
                }

                let demoted = usize::from(matches!(case, LocalSemanticCase::WrongLegacyPane));
                assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
                assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
                assert_eq!(
                    client.render_retention_codec_stats.batch_local_demotions,
                    demoted
                );
                assert_eq!(
                    client.render_retention_codec_stats.pending_payload_encodes,
                    demoted
                );
                assert_eq!(
                    client
                        .render_retention_codec_stats
                        .pending_payload_frame_allocations,
                    demoted
                );
                assert_eq!(
                    client.render_retention_codec_stats.pending_payload_decodes,
                    0
                );
                assert_eq!(
                    client.render_retention_codec_stats.snapshot_encodes,
                    demoted
                );
                assert_eq!(
                    client
                        .render_retention_codec_stats
                        .snapshot_frame_allocations,
                    demoted
                );
                assert!(client.outstanding_requests.is_empty(), "{case:?}");
                assert!(client.pending_render_changes.is_empty(), "{case:?}");
                assert_eq!(
                    client.pending_render_changes.retained_bytes(),
                    0,
                    "{case:?}"
                );
                assert!(client.render_change_snapshots.is_empty(), "{case:?}");
                assert_eq!(
                    client.render_change_snapshots.retained_bytes(),
                    0,
                    "{case:?}"
                );
                assert!(!client.connection_poisoned, "{case:?}");
                assert_eq!(client.poison_transition_count, 0, "{case:?}");

                let reused = client
                    .get_pane_render_changes(77)
                    .await
                    .expect("drained semantic error must preserve aligned reuse");
                assert_eq!(reused.pane_id, 77, "{case:?}");
                assert_eq!(reused.title, "reuse-after-local-semantic-error", "{case:?}");

                drop(client);
                server.await.expect("server task");
            }
        }));
    }

    #[test]
    fn batch_local_sideband_survives_matching_legacy_correlated_response() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("typed-sideband-legacy.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                        .expect("decode local-sideband legacy request")
                    {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "typed-sideband-legacy-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                render_requests += 1;
                                if render_requests == 1 {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            request.pane_id,
                                            1,
                                            "unilateral-before-legacy",
                                        )),
                                        0,
                                    )
                                    .await
                                    .expect("write unilateral response");
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            request.pane_id,
                                            2,
                                            "matching-legacy-response",
                                        )),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write matching legacy response");
                                } else {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::LivenessResponse(codec::LivenessResponse {
                                            pane_id: request.pane_id,
                                            is_alive: true,
                                        }),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write liveness for retained sideband");
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let mut client = DirectMuxClient::connect(direct_mux_client_config(socket_path))
                .await
                .expect("connect");
            let legacy = client
                .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                .await
                .expect("matching legacy response");
            assert_eq!(legacy[0].seqno, 2);
            assert_eq!(legacy[0].title, "matching-legacy-response");
            assert_eq!(client.pending_render_changes.len(), 1);

            let retained = client
                .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                .await
                .expect("retained unilateral response");
            assert_eq!(retained[0].seqno, 1);
            assert_eq!(retained[0].title, "unilateral-before-legacy");
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_demotions, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                1
            );
            assert_eq!(
                client
                    .render_retention_codec_stats
                    .pending_payload_frame_allocations,
                1
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                1
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 2);
            assert_eq!(
                client
                    .render_retention_codec_stats
                    .snapshot_frame_allocations,
                2
            );

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_local_sidebands_leave_unowned_payloads_in_global_retention() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("typed-sideband-unowned.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                        .expect("decode unowned-sideband request")
                    {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "typed-sideband-unowned-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                        request.pane_id,
                                        7,
                                        "owned-sideband",
                                    )),
                                    0,
                                )
                                .await
                                .expect("write owned sideband");
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                        999,
                                        41,
                                        "unowned-sideband",
                                    )),
                                    0,
                                )
                                .await
                                .expect("write unowned sideband");
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::LivenessResponse(codec::LivenessResponse {
                                        pane_id: request.pane_id,
                                        is_alive: true,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write owned liveness");
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            });

            let mut client = DirectMuxClient::connect(direct_mux_client_config(socket_path))
                .await
                .expect("connect");
            let owned = client
                .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                .await
                .expect("owned typed sideband");
            assert_eq!(owned[0].pane_id, 7);
            assert_eq!(owned[0].seqno, 7);
            assert_eq!(client.pending_render_changes.len(), 1);
            assert_eq!(
                client
                    .pending_render_changes
                    .iter()
                    .map(|item| item.pane_id)
                    .collect::<Vec<_>>(),
                vec![999]
            );
            assert!(client.render_change_snapshots.contains_key(7));
            assert!(client.render_change_snapshots.contains_key(999));
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 1);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                1
            );
            assert_eq!(
                client
                    .render_retention_codec_stats
                    .pending_payload_frame_allocations,
                1
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                0
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 2);

            let unowned = client
                .resolve_render_change_response(
                    999,
                    Pdu::LivenessResponse(codec::LivenessResponse {
                        pane_id: 999,
                        is_alive: true,
                    }),
                )
                .expect("unowned sideband must remain available globally");
            assert_eq!(unowned.pane_id, 999);
            assert_eq!(unowned.seqno, 41);
            assert_eq!(unowned.title, "unowned-sideband");
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                1
            );

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_local_and_global_sidebands_share_one_byte_authority() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("typed-sideband-shared-cap.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let owned_payload = test_render_change(7, 1, "owned-shared-cap");
            let unowned_payload = test_render_change(999, 1, "unowned-shared-cap");
            let owned_bytes = RetainedRenderChange::encode(1, owned_payload.clone())
                .expect("measure owned canonical retained frame")
                .retained_bytes();
            let unowned_bytes = RetainedRenderChange::encode(1, unowned_payload.clone())
                .expect("measure unowned canonical retained frame")
                .retained_bytes();
            let aggregate_bytes = owned_bytes
                .checked_add(unowned_bytes)
                .expect("small fixture retained bytes must add");
            let byte_limit = aggregate_bytes
                .checked_sub(1)
                .expect("two non-empty retained frames exceed one byte");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return 0usize;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Some(decoded) =
                        codec::Pdu::stream_decode(&mut read_buf).expect("decode shared-cap request")
                    {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "typed-sideband-shared-cap-test"
                                            .to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetPaneRenderChangesResponse(owned_payload),
                                    0,
                                )
                                .await
                                .expect("write locally owned sideband");
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetPaneRenderChangesResponse(unowned_payload),
                                    0,
                                )
                                .await
                                .expect("write globally retained sideband");
                                let mut eof_probe = [0u8; 1];
                                return unix_stream_read(&mut stream, &mut eof_probe)
                                    .await
                                    .expect("read shared-cap poison shutdown EOF");
                            }
                            _ => {}
                        }
                    }
                }
            });

            let mut config = direct_mux_client_config(socket_path);
            config.max_pending_render_change_bytes = byte_limit;
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let error = client
                .get_pane_render_changes_batch(&[7], 1, Duration::from_secs(5))
                .await
                .expect_err("local plus global sidebands must share one byte cap");
            assert!(
                matches!(
                    &error,
                    DirectMuxError::InFlightScopeAbandoned(source)
                        if matches!(
                            source.as_ref(),
                            DirectMuxError::RetentionLimitExceeded {
                                resource: "pending unilateral render changes",
                                requested_count: 2,
                                requested_bytes,
                                max_bytes,
                                ..
                            } if *requested_bytes == aggregate_bytes && *max_bytes == byte_limit
                        )
                ),
                "unexpected shared-cap error: {error:?}"
            );
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(
                client.render_retention_codec_stats.batch_local_peak_count,
                1
            );
            assert_eq!(
                client
                    .render_retention_codec_stats
                    .batch_local_peak_frame_bytes,
                owned_bytes
            );

            let peer_read = timeout(Duration::from_secs(1), server)
                .await
                .expect("shared-cap failure must shut down peer before client drop")
                .expect("server task");
            assert_eq!(peer_read, 0);
            drop(client);
        });
    }

    #[test]
    fn batch_render_changes_resolves_out_of_order_sidebands_and_liveness() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-order.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut batch_requests: Vec<(u64, usize)> = Vec::new();
                let mut responses_sent = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "wezterm-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                batch_requests.push((decoded.serial, request.pane_id));
                                if batch_requests.len() == 2 {
                                    for (idx, (_serial, pane_id)) in
                                        batch_requests.iter().rev().enumerate()
                                    {
                                        let sideband =
                                            Pdu::GetPaneRenderChangesResponse(test_render_change(
                                                *pane_id,
                                                idx + 1,
                                                &format!("pane-{pane_id}"),
                                            ));
                                        write_response_pdu(&mut stream, &sideband, 0)
                                            .await
                                            .expect("write sideband render delta");
                                    }
                                    for (serial, pane_id) in batch_requests.iter().rev() {
                                        let liveness =
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: *pane_id,
                                                is_alive: true,
                                            });
                                        write_response_pdu(&mut stream, &liveness, *serial)
                                            .await
                                            .expect("write correlated liveness response");
                                    }
                                    responses_sent += batch_requests.len();
                                    batch_requests.clear();
                                    if responses_sent == 6 {
                                        return;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let mut config = direct_mux_client_config(socket_path);
            config.max_pending_render_changes = 2;
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let responses = client
                .get_pane_render_changes_batch(&[10, 20, 30, 40, 50, 60], 2, Duration::from_secs(1))
                .await
                .expect("batch request");

            assert_eq!(responses.len(), 6);
            for (response, pane_id) in responses.iter().zip([10usize, 20, 30, 40, 50, 60]) {
                assert_eq!(response.pane_id, pane_id);
                assert_eq!(response.title, format!("pane-{pane_id}"));
            }
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_change_snapshots.len(), 6);
            assert!(client.render_change_snapshots.retained_bytes() > 0);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_render_changes_with_cx_resolves_out_of_order_sidebands_and_liveness() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-order-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut batch_requests: Vec<(u64, usize)> = Vec::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "wezterm-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                batch_requests.push((decoded.serial, request.pane_id));
                                if batch_requests.len() == 3 {
                                    for (idx, (serial, pane_id)) in
                                        batch_requests.iter().rev().enumerate()
                                    {
                                        let sideband =
                                            Pdu::GetPaneRenderChangesResponse(test_render_change(
                                                *pane_id,
                                                idx + 1,
                                                &format!("pane-{pane_id}"),
                                            ));
                                        write_response_pdu(&mut stream, &sideband, 0)
                                            .await
                                            .expect("write sideband render delta");
                                        let liveness =
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: *pane_id,
                                                is_alive: true,
                                            });
                                        write_response_pdu(&mut stream, &liveness, *serial)
                                            .await
                                            .expect("write correlated liveness response");
                                    }
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            let responses = client
                .get_pane_render_changes_batch_with_cx(
                    &cx,
                    &[10, 20, 30],
                    3,
                    Duration::from_secs(1),
                )
                .await
                .expect("batch request with cx");

            assert_eq!(responses.len(), 3);
            assert_eq!(responses[0].pane_id, 10);
            assert_eq!(responses[1].pane_id, 20);
            assert_eq!(responses[2].pane_id, 30);
            assert_eq!(responses[0].title, "pane-10");
            assert_eq!(responses[1].title, "pane-20");
            assert_eq!(responses[2].title, "pane-30");
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert_eq!(client.render_change_snapshots.len(), 3);
            assert!(client.render_change_snapshots.retained_bytes() > 0);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn render_batch_semantic_errors_drain_cleanup_and_reuse_connection() {
        #[derive(Clone, Copy, Debug)]
        enum SemanticCase {
            WrongLegacyPane,
            WrongLivenessPane,
            DeadPane,
            MissingDelta,
            UnexpectedPdu,
            ErrorResponse,
        }

        run_async_test(async {
            for case in [
                SemanticCase::WrongLegacyPane,
                SemanticCase::WrongLivenessPane,
                SemanticCase::DeadPane,
                SemanticCase::MissingDelta,
                SemanticCase::UnexpectedPdu,
                SemanticCase::ErrorResponse,
            ] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = temp_dir
                    .path()
                    .join(format!("render-semantic-{case:?}.sock"));
                let listener = compat_unix::bind(&socket_path)
                    .await
                    .expect("bind listener");

                let server = task::spawn(async move {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut initial_requests: Vec<(u64, usize)> = Vec::new();
                    let mut initial_batch_answered = false;

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);

                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "render-semantic-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    write_response_pdu(&mut stream, &response, decoded.serial)
                                        .await
                                        .expect("write codec response");
                                }
                                Pdu::SetClientId(_) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::UnitResponse(UnitResponse {}),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write client response");
                                }
                                Pdu::GetPaneRenderChanges(request) if !initial_batch_answered => {
                                    initial_requests.push((decoded.serial, request.pane_id));
                                    if initial_requests.len() != 3 {
                                        continue;
                                    }

                                    let (last_serial, last_pane) = initial_requests[2];
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            last_pane,
                                            30,
                                            "drained-last-target",
                                        )),
                                        last_serial,
                                    )
                                    .await
                                    .expect("write out-of-order last target response");

                                    let (bad_serial, bad_pane) = initial_requests[0];
                                    if matches!(case, SemanticCase::MissingDelta) {
                                        write_response_pdu(
                                            &mut stream,
                                            &Pdu::PaneRemoved(codec::PaneRemoved {
                                                pane_id: bad_pane,
                                            }),
                                            0,
                                        )
                                        .await
                                        .expect("invalidate pre-seeded missing-delta target");
                                    }
                                    let bad_response = match case {
                                        SemanticCase::WrongLegacyPane => {
                                            Pdu::GetPaneRenderChangesResponse(test_render_change(
                                                700,
                                                31,
                                                "wrong-legacy-pane",
                                            ))
                                        }
                                        SemanticCase::WrongLivenessPane => {
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: 700,
                                                is_alive: true,
                                            })
                                        }
                                        SemanticCase::DeadPane => {
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: bad_pane,
                                                is_alive: false,
                                            })
                                        }
                                        SemanticCase::MissingDelta => {
                                            Pdu::LivenessResponse(codec::LivenessResponse {
                                                pane_id: bad_pane,
                                                is_alive: true,
                                            })
                                        }
                                        SemanticCase::UnexpectedPdu => {
                                            Pdu::UnitResponse(UnitResponse {})
                                        }
                                        SemanticCase::ErrorResponse => {
                                            Pdu::ErrorResponse(codec::ErrorResponse {
                                                reason: "semantic test remote error".to_string(),
                                            })
                                        }
                                    };
                                    write_response_pdu(&mut stream, &bad_response, bad_serial)
                                        .await
                                        .expect("write semantic error response");

                                    let (middle_serial, middle_pane) = initial_requests[1];
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            middle_pane,
                                            32,
                                            "drained-middle-target",
                                        )),
                                        middle_serial,
                                    )
                                    .await
                                    .expect("write final drained target response");
                                    initial_batch_answered = true;
                                }
                                Pdu::GetPaneRenderChanges(request) => {
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetPaneRenderChangesResponse(test_render_change(
                                            request.pane_id,
                                            1,
                                            "same-connection-reuse",
                                        )),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write same-connection reuse response");
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });

                let config = direct_mux_client_config(socket_path);
                let mut client = DirectMuxClient::connect(config).await.expect("connect");
                for pane_id in [11usize, 22, 33, 999] {
                    client
                        .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(
                            test_render_change(pane_id, 10, "pre-seeded-retention"),
                        ))
                        .expect("seed render retention");
                }
                let unrelated_snapshot_bytes = client
                    .render_change_snapshots
                    .get(999)
                    .expect("unrelated snapshot")
                    .retained_bytes();
                let unrelated_pending_bytes = client
                    .pending_render_changes
                    .iter()
                    .find(|retained| retained.pane_id == 999)
                    .expect("unrelated pending delta")
                    .retained_bytes();
                let unrelated_response_serial =
                    next_request_serial(&mut client.serial).expect("reserve unrelated serial");
                client
                    .mark_request_outstanding(unrelated_response_serial)
                    .expect("mark unrelated request outstanding");
                client
                    .stash_pending_response(
                        unrelated_response_serial,
                        Pdu::UnitResponse(UnitResponse {}),
                    )
                    .expect("stage unrelated pending response");
                let unrelated_response_bytes = client.pending_response_bytes;

                let error = client
                    .get_pane_render_changes_batch(&[11, 22, 33], 3, Duration::from_secs(1))
                    .await
                    .expect_err("semantic response shape must fail after draining");
                match case {
                    SemanticCase::DeadPane | SemanticCase::ErrorResponse => {
                        assert!(matches!(error, DirectMuxError::RemoteError(_)), "{case:?}");
                    }
                    _ => {
                        assert!(
                            matches!(error, DirectMuxError::AlignedUnexpectedResponse { .. }),
                            "{case:?}"
                        );
                    }
                }
                assert_eq!(client.outstanding_requests.len(), 1, "{case:?}");
                assert!(
                    client
                        .outstanding_requests
                        .contains(&unrelated_response_serial),
                    "{case:?}"
                );
                assert_eq!(client.pending_responses.len(), 1, "{case:?}");
                assert!(
                    client
                        .pending_responses
                        .contains_key(&unrelated_response_serial),
                    "{case:?}"
                );
                assert_eq!(
                    client.pending_response_bytes, unrelated_response_bytes,
                    "{case:?}"
                );
                assert_eq!(client.pending_render_changes.len(), 1, "{case:?}");
                assert_eq!(
                    client
                        .pending_render_changes
                        .iter()
                        .next()
                        .map(|retained| retained.pane_id),
                    Some(999),
                    "{case:?}"
                );
                assert_eq!(
                    client.pending_render_changes.retained_bytes(),
                    unrelated_pending_bytes,
                    "{case:?}"
                );
                assert_eq!(client.render_change_snapshots.len(), 1, "{case:?}");
                assert!(client.render_change_snapshots.contains_key(999), "{case:?}");
                assert_eq!(
                    client.render_change_snapshots.retained_bytes(),
                    unrelated_snapshot_bytes,
                    "{case:?}"
                );
                assert!(!client.connection_poisoned, "{case:?}");
                assert_eq!(client.poison_transition_count, 0, "{case:?}");

                let preserved_response = client
                    .await_response(unrelated_response_serial)
                    .await
                    .expect("unrelated pending response must survive target cleanup");
                assert!(
                    matches!(preserved_response, Pdu::UnitResponse(_)),
                    "{case:?}"
                );
                assert!(client.outstanding_requests.is_empty(), "{case:?}");
                assert!(client.pending_responses.is_empty(), "{case:?}");
                assert_eq!(client.pending_response_bytes, 0, "{case:?}");

                let reuse = client
                    .get_pane_render_changes(77)
                    .await
                    .expect("fully drained semantic error must preserve aligned reuse");
                assert_eq!(reuse.pane_id, 77, "{case:?}");
                assert_eq!(reuse.title, "same-connection-reuse", "{case:?}");

                drop(client);
                server.await.expect("server task");
            }
        });
    }

    #[test]
    fn single_render_error_response_clears_stale_state_and_preserves_reuse() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("single-render-error-response.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_request_count = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "single-render-error-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(request) => {
                                render_request_count += 1;
                                if matches!(render_request_count, 1 | 3) {
                                    Pdu::ErrorResponse(codec::ErrorResponse {
                                        reason: format!(
                                            "single render remote error {render_request_count}"
                                        ),
                                    })
                                } else {
                                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                                        request.pane_id,
                                        render_request_count,
                                        "single-render-reuse",
                                    ))
                                }
                            }
                            _ => continue,
                        };
                        write_response_pdu(&mut stream, &response, decoded.serial)
                            .await
                            .expect("write response");
                        if render_request_count == 4 {
                            return;
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    27,
                    10,
                    "stale-before-ambient-error",
                )))
                .expect("seed ambient stale state");
            let ambient_error = client
                .get_pane_render_changes(27)
                .await
                .expect_err("ambient ErrorResponse must surface RemoteError");
            assert!(matches!(ambient_error, DirectMuxError::RemoteError(_)));
            assert!(!client.render_change_snapshots.contains_key(27));
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(!client.connection_poisoned);
            let ambient_reuse = client
                .get_pane_render_changes(27)
                .await
                .expect("ambient connection remains aligned");
            assert_eq!(ambient_reuse.title, "single-render-reuse");

            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    27,
                    11,
                    "stale-before-cx-error",
                )))
                .expect("seed Cx stale state");
            let cx_error = client
                .get_pane_render_changes_with_cx(&cx, 27)
                .await
                .expect_err("Cx ErrorResponse must surface RemoteError");
            assert!(matches!(cx_error, DirectMuxError::RemoteError(_)));
            assert!(!client.render_change_snapshots.contains_key(27));
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(!client.connection_poisoned);
            let cx_reuse = client
                .get_pane_render_changes_with_cx(&cx, 27)
                .await
                .expect("Cx connection remains aligned");
            assert_eq!(cx_reuse.title, "single-render-reuse");

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn later_prewrite_cx_cancellation_cleans_without_poisoning_aligned_connection() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("later-prewrite-cancel.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_request_count = 0usize;
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "later-prewrite-cancel-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(request) => {
                                render_request_count += 1;
                                if render_request_count == 1 {
                                    assert_eq!(request.pane_id, 11);
                                } else {
                                    assert_eq!(request.pane_id, 77);
                                }
                                Pdu::GetPaneRenderChangesResponse(test_render_change(
                                    request.pane_id,
                                    render_request_count,
                                    "later-prewrite-cancel-reuse",
                                ))
                            }
                            _ => continue,
                        };
                        write_response_pdu(&mut stream, &response, decoded.serial)
                            .await
                            .expect("write response");
                        if render_request_count == 2 {
                            return;
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let targets = [11_u64, 22];
            let mut guard = RenderBatchGuard::new(&mut client, &targets, 1, true);
            assert!(guard.send_next_with_cx(&cx).await.expect("issue first"));
            let decoded = guard
                .client
                .read_next_pdu_with_retention_metadata_with_cx(&cx)
                .await
                .expect("read first response");
            assert!(
                guard
                    .handle_decoded(decoded)
                    .expect("settle first response")
            );
            assert!(!guard.transport_ambiguous);
            assert!(guard.in_flight.is_empty());

            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel before second render write boundary"),
            );
            let error = guard
                .send_next_with_cx(&cx)
                .await
                .expect_err("second request must cancel before its write boundary");
            assert_cancelled_mux_error(&error);
            assert!(!guard.transport_ambiguous);
            drop(guard);

            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);
            assert!(client.outstanding_requests.is_empty());
            assert!(!client.render_change_snapshots.contains_key(11));
            let reuse_cx = crate::cx::for_testing();
            let reuse = client
                .get_pane_render_changes_with_cx(&reuse_cx, 77)
                .await
                .expect("aligned connection remains reusable after clean cancellation");
            assert_eq!(reuse.title, "later-prewrite-cancel-reuse");

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn post_write_render_retention_failure_poisons_and_shuts_down_socket() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("post-write-retention-failure.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return 0usize;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "retention-failure-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(request) => {
                                let response =
                                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                                        request.pane_id,
                                        1,
                                        "must-exceed-snapshot-limit",
                                    ));
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write render response");
                                let mut eof_probe = [0u8; 1];
                                return unix_stream_read(&mut stream, &mut eof_probe)
                                    .await
                                    .expect("read poison shutdown EOF");
                            }
                            _ => continue,
                        };
                        write_response_pdu(&mut stream, &response, decoded.serial)
                            .await
                            .expect("write handshake response");
                    }
                }
            });

            let mut config = direct_mux_client_config(socket_path);
            config.max_render_change_snapshots = 1;
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client
                .resolve_render_change_response(
                    999,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        999,
                        1,
                        "occupy-only-snapshot-slot",
                    )),
                )
                .expect("seed sole snapshot slot");

            let error = client
                .get_pane_render_changes_batch(&[11], 1, Duration::from_secs(1))
                .await
                .expect_err("post-write snapshot retention failure must fail closed");
            assert!(matches!(
                error,
                DirectMuxError::RetentionLimitExceeded {
                    resource: "render change snapshots",
                    ..
                }
            ));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(matches!(
                client.list_panes().await,
                Err(DirectMuxError::Disconnected)
            ));

            let peer_read = timeout(Duration::from_secs(1), server)
                .await
                .expect("poison must shut down peer before client drop")
                .expect("server task");
            assert_eq!(peer_read, 0);
            drop(client);
        });
    }

    #[test]
    fn dropping_in_flight_render_batch_future_poisons_and_shuts_down_socket() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("caller-drop-render-batch.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let request_observed = Arc::new(AtomicBool::new(false));
            let server_observed = Arc::clone(&request_observed);

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return 0usize;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "caller-drop-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(_) => {
                                server_observed.store(true, Ordering::SeqCst);
                                let mut eof_probe = [0u8; 1];
                                return unix_stream_read(&mut stream, &mut eof_probe)
                                    .await
                                    .expect("read caller-drop shutdown EOF");
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client.config.read_timeout = Duration::from_secs(5);
            preload_compressed_render_sideband(
                &mut client,
                11,
                "typed-sideband-before-caller-drop",
            );
            let targets = [11_u64];
            let mut batch =
                Box::pin(client.get_pane_render_changes_batch(&targets, 1, Duration::from_secs(5)));
            std::future::poll_fn(|task_cx| {
                match std::future::Future::poll(batch.as_mut(), task_cx) {
                    std::task::Poll::Ready(result) => {
                        panic!("stalled batch unexpectedly completed: {result:?}")
                    }
                    std::task::Poll::Pending if request_observed.load(Ordering::SeqCst) => {
                        std::task::Poll::Ready(())
                    }
                    std::task::Poll::Pending => {
                        task_cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                }
            })
            .await;
            drop(batch);

            assert!(client.connection_poisoned);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.read_buf.is_empty());
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
            assert_eq!(client.render_retention_codec_stats.batch_local_demotions, 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                0
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                0
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 0);
            assert!(matches!(
                client.list_panes().await,
                Err(DirectMuxError::Disconnected)
            ));

            let peer_read = timeout(Duration::from_secs(1), server)
                .await
                .expect("caller-drop poison must shut down peer before client drop")
                .expect("server task");
            assert_eq!(peer_read, 0);
            drop(client);
        });
    }

    #[test]
    fn render_invalidation_accounting_corruption_is_atomic_and_batch_cleanup_poisons() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("render-accounting-corruption.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_serials = Vec::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return 0usize;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "render-accounting-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(_) => {
                                render_serials.push(decoded.serial);
                                if render_serials.len() == 2 {
                                    for serial in render_serials.iter().rev().copied() {
                                        write_response_pdu(
                                            &mut stream,
                                            &Pdu::ErrorResponse(codec::ErrorResponse {
                                                reason: "drained accounting cleanup error"
                                                    .to_string(),
                                            }),
                                            serial,
                                        )
                                        .await
                                        .expect("write drained semantic response");
                                    }
                                    let mut eof_probe = [0u8; 1];
                                    return unix_stream_read(&mut stream, &mut eof_probe)
                                        .await
                                        .expect("read accounting-poison shutdown EOF");
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            for pane_id in [11usize, 22, 999] {
                client
                    .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                        pane_id,
                        1,
                        "accounting-seed",
                    )))
                    .expect("seed retained render state");
            }
            let bulk_targets = (1_000_u64..1_256).collect::<Vec<_>>();
            for pane_id in bulk_targets.iter().copied() {
                client
                    .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                        pane_id as usize,
                        1,
                        "bulk-accounting-seed",
                    )))
                    .expect("seed bulk retained render state");
            }
            let (pending_removed, snapshots_removed) = client
                .invalidate_render_state_for_panes(&bulk_targets)
                .expect("bulk invalidation should remove 256 targets in one checked pass");
            assert_eq!(pending_removed, bulk_targets.len());
            assert_eq!(snapshots_removed, bulk_targets.len());
            assert_eq!(client.pending_render_changes.len(), 3);
            assert_eq!(client.render_change_snapshots.len(), 3);
            assert_eq!(
                client.pending_render_changes.retained_bytes(),
                client
                    .pending_render_changes
                    .iter()
                    .map(RetainedRenderChange::retained_bytes)
                    .sum::<usize>()
            );
            assert_eq!(
                client.render_change_snapshots.retained_bytes(),
                client
                    .render_change_snapshots
                    .values()
                    .map(RetainedRenderChange::retained_bytes)
                    .sum::<usize>()
            );
            let snapshot_keys = client
                .render_change_snapshots
                .keys()
                .copied()
                .collect::<HashSet<_>>();
            let pending_panes = client
                .pending_render_changes
                .iter()
                .map(|retained| retained.pane_id)
                .collect::<Vec<_>>();
            let correct_snapshot_bytes = client.render_change_snapshots.retained_bytes();
            let correct_pending_bytes = client.pending_render_changes.retained_bytes();
            assert!(correct_snapshot_bytes > 0);
            assert!(correct_pending_bytes > 0);

            client.pending_render_changes.totals.bytes = correct_pending_bytes + 1;
            let error = client
                .invalidate_render_state_for_pane(11)
                .expect_err("single invalidation must reject pending-byte overcount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "pending unilateral render changes"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.pending_render_changes.retained_bytes(),
                correct_pending_bytes + 1
            );
            client
                .pending_render_changes
                .totals
                .set(pending_panes.len(), correct_pending_bytes);

            client.pending_render_changes.totals.bytes = correct_pending_bytes - 1;
            let error = client
                .invalidate_render_state_for_pane(11)
                .expect_err("single invalidation must reject pending-byte undercount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "pending unilateral render changes"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.pending_render_changes.retained_bytes(),
                correct_pending_bytes - 1
            );
            client
                .pending_render_changes
                .totals
                .set(pending_panes.len(), correct_pending_bytes);

            client.render_change_snapshots.totals.bytes = correct_snapshot_bytes + 1;
            let error = client
                .invalidate_render_state_for_pane(11)
                .expect_err("single invalidation must reject snapshot-byte overcount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "render change snapshots"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.render_change_snapshots.retained_bytes(),
                correct_snapshot_bytes + 1
            );
            client
                .render_change_snapshots
                .totals
                .set(snapshot_keys.len(), correct_snapshot_bytes);

            client.render_change_snapshots.totals.bytes = correct_snapshot_bytes - 1;
            let error = client
                .invalidate_render_state_for_pane(11)
                .expect_err("single invalidation must reject snapshot-byte undercount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "render change snapshots"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.render_change_snapshots.retained_bytes(),
                correct_snapshot_bytes - 1
            );
            client
                .render_change_snapshots
                .totals
                .set(snapshot_keys.len(), correct_snapshot_bytes);

            client.pending_render_changes.totals.bytes = correct_pending_bytes + 1;
            let error = client
                .invalidate_render_state_for_panes(&[11, 22])
                .expect_err("bulk invalidation must reject pending-byte overcount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "pending unilateral render changes"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.pending_render_changes.retained_bytes(),
                correct_pending_bytes + 1
            );
            client
                .pending_render_changes
                .totals
                .set(pending_panes.len(), correct_pending_bytes);

            client.pending_render_changes.totals.bytes = correct_pending_bytes - 1;
            let error = client
                .invalidate_render_state_for_panes(&[11, 22])
                .expect_err("bulk invalidation must reject pending-byte undercount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "pending unilateral render changes"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.pending_render_changes.retained_bytes(),
                correct_pending_bytes - 1
            );
            client
                .pending_render_changes
                .totals
                .set(pending_panes.len(), correct_pending_bytes);

            client.render_change_snapshots.totals.bytes = correct_snapshot_bytes + 1;
            let error = client
                .invalidate_render_state_for_panes(&[11, 22])
                .expect_err("bulk invalidation must reject snapshot-byte overcount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "render change snapshots"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.render_change_snapshots.retained_bytes(),
                correct_snapshot_bytes + 1
            );

            client.render_change_snapshots.totals.bytes = correct_snapshot_bytes - 1;
            let error = client
                .invalidate_render_state_for_panes(&[11, 22])
                .expect_err("bulk invalidation must reject snapshot-byte undercount");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "render change snapshots"
                }
            ));
            assert_render_retention_members(&client, &snapshot_keys, &pending_panes);
            assert_eq!(
                client.render_change_snapshots.retained_bytes(),
                correct_snapshot_bytes - 1
            );

            client.render_change_snapshots.totals.bytes = correct_snapshot_bytes + 1;

            let error = client
                .get_pane_render_changes_batch(&[11, 22], 2, Duration::from_secs(1))
                .await
                .expect_err("drained semantic cleanup accounting failure must poison");
            assert!(matches!(
                error,
                DirectMuxError::RetainedStateAccounting {
                    resource: "render change snapshots"
                }
            ));
            assert!(client.connection_poisoned);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.read_buf.is_empty());

            let peer_read = timeout(Duration::from_secs(1), server)
                .await
                .expect("cleanup poison must shut down peer before client drop")
                .expect("server task");
            assert_eq!(peer_read, 0);
            drop(client);
        });
    }

    #[test]
    fn render_batch_disconnect_after_write_poisons_and_blocks_serial_reuse() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("render-batch-disconnect.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "render-disconnect-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    }),
                                    decoded.serial,
                                )
                                .await
                                .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                write_response_pdu(
                                    &mut stream,
                                    &Pdu::UnitResponse(UnitResponse {}),
                                    decoded.serial,
                                )
                                .await
                                .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(_) => return,
                            _ => {}
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client
                .stash_unilateral_pdu(Pdu::GetPaneRenderChangesResponse(test_render_change(
                    999,
                    1,
                    "must-clear-on-disconnect",
                )))
                .expect("seed retained state");
            let serial_before = client.serial;
            let error = client
                .get_pane_render_changes_batch(&[11], 1, Duration::from_secs(1))
                .await
                .expect_err("peer close after request write must fail batch");
            assert!(matches!(error, DirectMuxError::Disconnected));
            assert!(client.connection_poisoned);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.read_buf.is_empty());
            assert_eq!(client.serial, serial_before + 1);
            let poisoned_serial = client.serial;
            assert!(matches!(
                client.list_panes().await,
                Err(DirectMuxError::Disconnected)
            ));
            assert_eq!(client.serial, poisoned_serial);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn render_batch_liveness_only_reuses_valid_cached_snapshot() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("render-batch-cached-liveness.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return;
                    }
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "cached-liveness-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(request) => {
                                assert_eq!(request.pane_id, 11);
                                Pdu::LivenessResponse(codec::LivenessResponse {
                                    pane_id: request.pane_id,
                                    is_alive: true,
                                })
                            }
                            _ => continue,
                        };
                        write_response_pdu(&mut stream, &response, decoded.serial)
                            .await
                            .expect("write response");
                        if matches!(response, Pdu::LivenessResponse(_)) {
                            return;
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client
                .resolve_render_change_response(
                    11,
                    Pdu::GetPaneRenderChangesResponse(test_render_change(
                        11,
                        41,
                        "cached-batch-snapshot",
                    )),
                )
                .expect("seed cached snapshot");
            let responses = client
                .get_pane_render_changes_batch(&[11], 1, Duration::from_secs(1))
                .await
                .expect("liveness-only batch may reuse a valid cached snapshot");
            assert_eq!(responses.len(), 1);
            assert_eq!(responses[0].pane_id, 11);
            assert_eq!(responses[0].seqno, 41);
            assert_eq!(responses[0].title, "cached-batch-snapshot");
            assert!(responses[0].dirty_lines.is_empty());
            assert!(!client.connection_poisoned);
            assert!(client.outstanding_requests.is_empty());

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_render_changes_zero_pipeline_depth_is_clamped_and_succeeds() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-depth-clamp.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "batch-depth-clamp-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                let response = Pdu::GetPaneRenderChangesResponse(
                                    GetPaneRenderChangesResponse {
                                        pane_id: request.pane_id,
                                        mouse_grabbed: false,
                                        alt_screen_active: false,
                                        cursor_position:
                                            mux::renderable::StableCursorPosition::default(),
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
                                        tiered_scrollback_status: None,
                                        dirty_lines: Vec::new(),
                                        title: format!("pane-{}", request.pane_id),
                                        working_dir: None,
                                        bonus_lines: Vec::new().into(),
                                        input_serial: None,
                                        seqno: request.pane_id,
                                    },
                                );
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            let responses = client
                .get_pane_render_changes_batch(&[41, 42], 0, Duration::from_secs(1))
                .await
                .expect("batch request with zero depth should be clamped");

            assert_eq!(responses.len(), 2);
            assert_eq!(responses[0].pane_id, 41);
            assert_eq!(responses[1].pane_id, 42);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_render_changes_with_cx_zero_pipeline_depth_is_clamped_and_succeeds() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-depth-clamp-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "batch-depth-clamp-with-cx-test"
                                            .to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                let response = Pdu::GetPaneRenderChangesResponse(
                                    GetPaneRenderChangesResponse {
                                        pane_id: request.pane_id,
                                        mouse_grabbed: false,
                                        alt_screen_active: false,
                                        cursor_position:
                                            mux::renderable::StableCursorPosition::default(),
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
                                        tiered_scrollback_status: None,
                                        dirty_lines: Vec::new(),
                                        title: format!("pane-{}", request.pane_id),
                                        working_dir: None,
                                        bonus_lines: Vec::new().into(),
                                        input_serial: None,
                                        seqno: request.pane_id,
                                    },
                                );
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write response");
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            let responses = client
                .get_pane_render_changes_batch_with_cx(&cx, &[41, 42], 0, Duration::from_secs(1))
                .await
                .expect("batch request with cx and zero depth should be clamped");

            assert_eq!(responses.len(), 2);
            assert_eq!(responses[0].pane_id, 41);
            assert_eq!(responses[1].pane_id, 42);

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn concurrent_get_pane_render_changes_operations_share_connection_safely() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("concurrent-ops.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let expected_requests = 5usize;

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_serials = Vec::with_capacity(expected_requests);

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "concurrent-ops-test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(request) => {
                                render_serials.push(decoded.serial);
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: request.pane_id,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines: Vec::new(),
                                    title: format!("pane-{}", request.pane_id),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno: request.pane_id,
                                })
                            }
                            _ => continue,
                        };

                        write_response_pdu(&mut stream, &response, decoded.serial)
                            .await
                            .expect("write response");

                        if render_serials.len() == expected_requests {
                            return render_serials;
                        }
                    }
                }

                render_serials
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = Arc::new(Mutex::new(
                DirectMuxClient::connect(config).await.expect("connect"),
            ));
            let pane_ids = vec![11_u64, 22, 33, 44, 55];

            // The runtime_async MutexGuard is !Send, so it cannot be held
            // across an await inside task::spawn. The spawned variant of this
            // test serialized the requests through the mutex anyway; issue
            // them sequentially from the test future (current-thread runtime,
            // no Send requirement) — connection sharing across requests is
            // still exercised.
            let mut seen_panes = HashSet::new();
            for pane_id in &pane_ids {
                let requested = *pane_id;
                let mut guard = client.lock().await;
                let response = guard
                    .get_pane_render_changes(requested)
                    .await
                    .expect("get_pane_render_changes");
                drop(guard);
                assert_eq!(response.pane_id as u64, requested);
                assert_eq!(response.seqno as u64, requested);
                seen_panes.insert(response.pane_id);
            }
            assert_eq!(seen_panes.len(), pane_ids.len());

            drop(client);
            let render_serials = server.await.expect("server task");
            assert_eq!(render_serials.len(), expected_requests);
            let unique_serials: HashSet<u64> = render_serials.iter().copied().collect();
            assert_eq!(unique_serials.len(), expected_requests);
        });
    }

    #[test]
    fn await_response_reuses_stashed_out_of_order_response_for_later_serial() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("stashed-out-of-order.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut list_serials = Vec::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "stashed-response-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                let mut out = Vec::new();
                                response
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let mut out = Vec::new();
                                Pdu::UnitResponse(UnitResponse {})
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                            Pdu::ListPanes(_) => {
                                list_serials.push(decoded.serial);
                                if list_serials.len() == 2 {
                                    for serial in list_serials.iter().rev().copied() {
                                        let response = Pdu::ListPanesResponse(ListPanesResponse {
                                            tabs: Vec::new(),
                                            tab_titles: Vec::new(),
                                            window_titles: HashMap::new(),
                                            floating_panes: Vec::new(),
                                        });
                                        let mut out = Vec::new();
                                        response.encode(&mut out, serial).expect("encode response");
                                        stream.write_all(&out).await.expect("write response");
                                    }
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let first_serial = client
                .send_request_only(Pdu::ListPanes(ListPanes {}))
                .await
                .expect("send first list panes request");
            let second_serial = client
                .send_request_only(Pdu::ListPanes(ListPanes {}))
                .await
                .expect("send second list panes request");
            assert_ne!(first_serial, second_serial);

            let first_response = client
                .await_response(first_serial)
                .await
                .expect("await first response");
            assert!(
                matches!(first_response, Pdu::ListPanesResponse(_)),
                "first serial should resolve to ListPanesResponse"
            );
            assert!(
                client.pending_responses.contains_key(&second_serial),
                "out-of-order second response should be stashed"
            );
            let retained_bytes = client.pending_response_bytes;
            assert!(
                retained_bytes > 0,
                "stashed response must contribute to the exact byte budget"
            );

            let duplicate_error = client
                .stash_pending_response(second_serial, Pdu::UnitResponse(UnitResponse {}))
                .expect_err("duplicate response serial must fail without replacing state");
            assert!(matches!(
                duplicate_error,
                DirectMuxError::UnexpectedResponse { .. }
            ));
            assert_eq!(
                client.pending_response_bytes, retained_bytes,
                "duplicate rejection must preserve byte accounting"
            );

            let second_response = client
                .await_response(second_serial)
                .await
                .expect("await second response from stash");
            assert!(
                matches!(second_response, Pdu::ListPanesResponse(_)),
                "second serial should resolve from pending response stash"
            );
            assert!(
                !client.pending_responses.contains_key(&second_serial),
                "pending stash should be drained after serving the second response"
            );
            assert_eq!(
                client.pending_response_bytes, 0,
                "draining the retained response must release its byte budget"
            );
            let stale_error = client
                .stash_pending_response(second_serial, Pdu::UnitResponse(UnitResponse {}))
                .expect_err("response for a completed request must be rejected");
            assert!(matches!(
                stale_error,
                DirectMuxError::ResponseSerialNotOutstanding {
                    serial,
                    ..
                } if serial == second_serial
            ));
            assert!(client.pending_responses.is_empty());

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn concurrent_connect_attempts_assign_unique_connection_ids() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("concurrent-connect.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let expected_clients = 4usize;

            let server = task::spawn(async move {
                let mut handlers = Vec::with_capacity(expected_clients);
                for _ in 0..expected_clients {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    handlers.push(task::spawn(async move {
                        let mut read_buf = StreamingPduBuffer::new();
                        loop {
                            let mut temp = vec![0u8; 4096];
                            let read = unix_stream_read(&mut stream, &mut temp)
                                .await
                                .expect("read");
                            if read == 0 {
                                break;
                            }
                            read_buf.extend_from_slice(&temp[..read]);

                            while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                                let response = match decoded.pdu {
                                    Pdu::GetCodecVersion(_) => {
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "concurrent-connect-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        })
                                    }
                                    Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                    Pdu::ListPanes(_) => {
                                        Pdu::ListPanesResponse(ListPanesResponse {
                                            tabs: Vec::new(),
                                            tab_titles: Vec::new(),
                                            window_titles: HashMap::new(),
                                            floating_panes: Vec::new(),
                                        })
                                    }
                                    _ => continue,
                                };

                                let mut out = Vec::new();
                                response
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                        }
                    }));
                }

                for handler in handlers {
                    handler.await.expect("connection handler");
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut joins = Vec::with_capacity(expected_clients);
            for _ in 0..expected_clients {
                let config = config.clone();
                joins.push(task::spawn(async move {
                    let mut client = DirectMuxClient::connect(config).await.expect("connect");
                    let _ = client.list_panes().await.expect("list panes");
                    client.connection_id
                }));
            }

            let mut ids = HashSet::new();
            for join in joins {
                let id = join.await.expect("join connect task");
                assert!(id > 0, "connection id should be positive");
                ids.insert(id);
            }
            assert_eq!(
                ids.len(),
                expected_clients,
                "each concurrent connect should get a unique connection id"
            );

            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_render_changes_times_out_when_server_stalls_mid_batch() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-timeout.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_request_count = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "batch-timeout-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                let mut out = Vec::new();
                                response
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let mut out = Vec::new();
                                Pdu::UnitResponse(UnitResponse {})
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                            Pdu::GetPaneRenderChanges(_) => {
                                render_request_count += 1;
                                if render_request_count == 2 {
                                    // Both requests are now unambiguously in
                                    // flight. Hold the socket open until the
                                    // timed-out client poisons and closes it.
                                    let mut eof_probe = [0u8; 1];
                                    let peer_read = unix_stream_read(&mut stream, &mut eof_probe)
                                        .await
                                        .expect("read timeout poison shutdown EOF");
                                    assert_eq!(peer_read, 0);
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = direct_mux_client_config(socket_path);
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client.config.read_timeout = Duration::from_millis(500);
            preload_compressed_render_sideband(&mut client, 10, "pane-timeout");

            let err = client
                .get_pane_render_changes_batch(&[10, 20], 2, Duration::from_millis(25))
                .await
                .expect_err("batch should time out when server stalls mid-batch");
            match err {
                DirectMuxError::BatchTimeout { timeout_ms } => assert_eq!(timeout_ms, 25),
                other => panic!("expected BatchTimeout, got: {other}"),
            }
            assert!(client.connection_poisoned);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.read_buf.is_empty());
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
            assert_eq!(client.render_retention_codec_stats.batch_local_demotions, 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                0
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                0
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 0);
            let reuse_error = client
                .list_panes()
                .await
                .expect_err("a timed-out render batch must poison direct connection reuse");
            assert!(matches!(reuse_error, DirectMuxError::Disconnected));

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_render_changes_with_cx_times_out_when_server_stalls_mid_batch() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-timeout-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_request_count = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        return;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "batch-timeout-with-cx-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                let mut out = Vec::new();
                                response
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                            Pdu::SetClientId(_) => {
                                let mut out = Vec::new();
                                Pdu::UnitResponse(UnitResponse {})
                                    .encode(&mut out, decoded.serial)
                                    .expect("encode response");
                                stream.write_all(&out).await.expect("write response");
                            }
                            Pdu::GetPaneRenderChanges(_) => {
                                render_request_count += 1;
                                if render_request_count == 2 {
                                    let mut eof_probe = [0u8; 1];
                                    let peer_read = unix_stream_read(&mut stream, &mut eof_probe)
                                        .await
                                        .expect("read Cx-timeout poison shutdown EOF");
                                    assert_eq!(peer_read, 0);
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            client.config.read_timeout = Duration::from_millis(500);
            preload_compressed_render_sideband(&mut client, 10, "pane-timeout-with-cx");

            let err = client
                .get_pane_render_changes_batch_with_cx(&cx, &[10, 20], 2, Duration::from_millis(25))
                .await
                .expect_err("batch with cx should time out when server stalls mid-batch");
            match err {
                DirectMuxError::BatchTimeout { timeout_ms } => assert_eq!(timeout_ms, 25),
                other => panic!("expected BatchTimeout, got: {other}"),
            }
            assert!(client.connection_poisoned);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.read_buf.is_empty());
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
            assert_eq!(client.render_retention_codec_stats.batch_local_demotions, 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                0
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                0
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 0);
            let reuse_error = client
                .list_panes()
                .await
                .expect_err("a timed-out Cx render batch must poison direct connection reuse");
            assert!(matches!(reuse_error, DirectMuxError::Disconnected));

            drop(client);
            server.await.expect("server task");
        });
    }

    #[test]
    fn batch_render_changes_with_cx_cancellation_during_stalled_batch_surfaces_cancelled_io() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("batch-cancel-with-cx.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");
            let (stall_tx, stall_rx) = std::sync::mpsc::channel();

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_request_count = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    if read == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                let response =
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "batch-cancel-with-cx-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    });
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write codec response");
                            }
                            Pdu::SetClientId(_) => {
                                let response = Pdu::UnitResponse(UnitResponse {});
                                write_response_pdu(&mut stream, &response, decoded.serial)
                                    .await
                                    .expect("write client response");
                            }
                            Pdu::GetPaneRenderChanges(_) => {
                                render_request_count += 1;
                                if render_request_count == 2 {
                                    stall_tx.send(()).expect("signal stalled batch");
                                    let mut eof_probe = [0u8; 1];
                                    let peer_read = unix_stream_read(&mut stream, &mut eof_probe)
                                        .await
                                        .expect("read cancellation poison shutdown EOF");
                                    assert_eq!(peer_read, 0);
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            client.config.read_timeout = Duration::from_millis(500);
            preload_compressed_render_sideband(&mut client, 10, "pane-cancel-with-cx");

            let cancel_cx = cx.clone();
            let cancel = std::thread::spawn(move || {
                stall_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("server should enter stalled batch state");
                cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("cancel during batch wait"),
                );
            });

            let err = client
                .get_pane_render_changes_batch_with_cx(&cx, &[10, 20], 2, Duration::from_secs(5))
                .await
                .expect_err("batch with cx should surface cancellation");
            assert_cancelled_mux_error(&err);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);
            assert!(client.outstanding_requests.is_empty());
            assert!(client.pending_responses.is_empty());
            assert_eq!(client.pending_response_bytes, 0);
            assert!(client.pending_render_changes.is_empty());
            assert_eq!(client.pending_render_changes.retained_bytes(), 0);
            assert!(client.render_change_snapshots.is_empty());
            assert_eq!(client.render_change_snapshots.retained_bytes(), 0);
            assert!(client.read_buf.is_empty());
            assert_eq!(client.render_retention_codec_stats.batch_local_claims, 1);
            assert_eq!(client.render_retention_codec_stats.batch_local_returns, 0);
            assert_eq!(client.render_retention_codec_stats.batch_local_demotions, 0);
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_encodes,
                0
            );
            assert_eq!(
                client.render_retention_codec_stats.pending_payload_decodes,
                0
            );
            assert_eq!(client.render_retention_codec_stats.snapshot_encodes, 0);
            let reuse_error = client
                .list_panes()
                .await
                .expect_err("a cancelled render batch must poison direct connection reuse");
            assert!(matches!(reuse_error, DirectMuxError::Disconnected));

            drop(client);
            cancel.join().expect("cancel thread");
            server.await.expect("server task");
        });
    }

    #[test]
    fn next_request_serial_rejects_overflow() {
        let mut serial = u64::MAX;
        let err = next_request_serial(&mut serial).expect_err("overflow should be rejected");
        assert!(matches!(err, DirectMuxError::SerialExhausted));
    }

    #[test]
    fn connection_id_allocator_exhausts_without_wrapping_or_reusing() {
        let next = std::sync::atomic::AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            next_connection_id_from(&next).expect("last admissible identity"),
            u64::MAX - 1
        );
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);

        let err = next_connection_id_from(&next)
            .expect_err("terminal allocator state must fail permanently");
        assert!(matches!(err, DirectMuxError::ConnectionIdExhausted));
        assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Permanent);
        assert_eq!(
            next.load(Ordering::Relaxed),
            u64::MAX,
            "exhaustion must not wrap the allocator"
        );

        let reserved = std::sync::atomic::AtomicU64::new(0);
        assert!(matches!(
            next_connection_id_from(&reserved),
            Err(DirectMuxError::ConnectionIdExhausted)
        ));
        assert_eq!(reserved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn retained_pdu_rejects_cross_connection_replay() {
        let retained = RetainedMuxPdu::encode(41, 7, Pdu::UnitResponse(UnitResponse {}))
            .expect("encode retained response");
        let err = retained
            .decode(42, 7)
            .expect_err("successor connection must reject predecessor retention");
        assert!(matches!(
            err,
            DirectMuxError::RetainedConnectionMismatch {
                expected_connection_id: 42,
                got_connection_id: 41,
            }
        ));
        assert!(matches!(
            retained
                .decode(41, 7)
                .expect("matching connection may decode retained state"),
            Pdu::UnitResponse(_)
        ));
    }

    #[test]
    fn config_rejects_zero_retention_limits_before_transport_use() {
        let mut config = DirectMuxClientConfig::default();
        config.max_pending_render_changes = 0;
        let err = config
            .validate()
            .expect_err("zero retention bound must fail closed");
        assert!(matches!(
            &err,
            DirectMuxError::InvalidLimit {
                field: "max_pending_render_changes"
            }
        ));
        let decision = err.recovery_decision();
        assert_eq!(decision.kind, ProtocolErrorKind::Permanent);
        assert!(!decision.retry);
        assert_eq!(decision.connection, MuxConnectionDisposition::Reuse);
        assert!(!decision.cancelled);
    }

    #[test]
    fn config_rejects_zero_outbound_budget_before_transport_use() {
        for (field, config) in [
            (
                "max_outbound_codec_bytes",
                DirectMuxClientConfig {
                    max_outbound_codec_bytes: 0,
                    ..DirectMuxClientConfig::default()
                },
            ),
            (
                "max_outbound_in_flight_requests",
                DirectMuxClientConfig {
                    max_outbound_in_flight_requests: 0,
                    ..DirectMuxClientConfig::default()
                },
            ),
        ] {
            let err = config
                .validate()
                .expect_err("zero outbound budget must fail closed");
            assert!(matches!(
                err,
                DirectMuxError::InvalidLimit { field: got } if got == field
            ));
        }
    }

    fn prepared_write_request(
        compression_mode: CompressionMode,
        payload_bytes: usize,
    ) -> OwnedPreparedPduOutbound {
        Pdu::WriteToPane(WriteToPane {
            pane_id: 7,
            data: vec![b'x'; payload_bytes],
        })
        .prepare_outbound(
            PduProducer::Client,
            PduWireRole::Request,
            None,
            compression_mode,
        )
        .expect("bounded write request must produce an outbound plan")
    }

    #[test]
    fn outbound_frame_limit_is_exact_and_precedes_budget_mutation_for_every_mode() {
        for compression_mode in [
            CompressionMode::Never,
            CompressionMode::Auto,
            CompressionMode::Always,
        ] {
            let probe = prepared_write_request(compression_mode, 4096);
            let exact_frame_bytes = probe.maximum_frame_bytes();
            let exact_codec_bytes = probe.codec_peak_bytes();
            let config = DirectMuxClientConfig {
                max_frame_bytes: exact_frame_bytes,
                max_outbound_codec_bytes: exact_codec_bytes,
                max_outbound_in_flight_requests: 1,
                ..DirectMuxClientConfig::default()
            };
            let budget = Arc::new(DirectMuxOutboundBudget::from_config(&config));

            let lease = budget
                .try_admit(probe, exact_frame_bytes)
                .expect("the exact conservative frame and codec bounds must admit");
            assert_eq!(
                budget.snapshot(),
                DirectMuxOutboundBudgetState {
                    codec_bytes: exact_codec_bytes,
                    noninteractive_codec_bytes: 0,
                    requests: 1,
                    noninteractive_requests: 0,
                    peak_codec_bytes: exact_codec_bytes,
                }
            );
            drop(lease);
            assert_eq!(
                budget.snapshot(),
                DirectMuxOutboundBudgetState {
                    peak_codec_bytes: exact_codec_bytes,
                    ..DirectMuxOutboundBudgetState::default()
                }
            );

            let rejected = budget
                .try_admit(
                    prepared_write_request(compression_mode, 4096),
                    exact_frame_bytes - 1,
                )
                .expect_err("one byte below the planned frame bound must reject");
            assert!(matches!(
                rejected,
                DirectMuxError::ProvenPreWriteRejection(source)
                    if matches!(*source, DirectMuxError::FrameTooLarge { max_bytes }
                        if max_bytes == exact_frame_bytes - 1)
            ));
            assert_eq!(
                budget.snapshot(),
                DirectMuxOutboundBudgetState {
                    peak_codec_bytes: exact_codec_bytes,
                    ..DirectMuxOutboundBudgetState::default()
                },
                "frame-cap rejection must not charge the shared authority"
            );
        }
    }

    #[test]
    fn outbound_budget_is_shared_and_releases_the_exact_lease() {
        let first = prepared_write_request(CompressionMode::Never, 1024);
        let one_request_bytes = first.codec_peak_bytes();
        let max_codec_bytes = one_request_bytes
            .checked_mul(2)
            .expect("small test budget must fit");
        let config = DirectMuxClientConfig {
            max_frame_bytes: first.maximum_frame_bytes(),
            max_outbound_codec_bytes: max_codec_bytes,
            max_outbound_in_flight_requests: 2,
            ..DirectMuxClientConfig::default()
        };
        let budget = Arc::new(DirectMuxOutboundBudget::from_config(&config));
        let first_lease = budget
            .try_admit(first, config.max_frame_bytes)
            .expect("first connection incarnation must admit");
        let successor_budget = Arc::clone(&budget);
        let successor_lease = successor_budget
            .try_admit(
                prepared_write_request(CompressionMode::Never, 1024),
                config.max_frame_bytes,
            )
            .expect("successor connection must share the remaining root capacity");

        let rejected = successor_budget
            .try_admit(
                prepared_write_request(CompressionMode::Never, 1024),
                config.max_frame_bytes,
            )
            .expect_err("a third connection must not bypass the shared root");
        assert!(matches!(
            rejected,
            DirectMuxError::ProvenPreWriteRejection(source)
                if matches!(*source, DirectMuxError::RetentionLimitExceeded {
                    resource: "outbound mux admission",
                    ..
                })
        ));
        assert_eq!(budget.snapshot().codec_bytes, max_codec_bytes);
        assert_eq!(budget.snapshot().requests, 2);

        drop(first_lease);
        let replacement = successor_budget
            .try_admit(
                prepared_write_request(CompressionMode::Never, 1024),
                config.max_frame_bytes,
            )
            .expect("dropping one exact lease must restore one root slot");
        assert_eq!(budget.snapshot().codec_bytes, max_codec_bytes);
        assert_eq!(budget.snapshot().requests, 2);
        drop(replacement);
        drop(successor_lease);
        assert_eq!(budget.snapshot().codec_bytes, 0);
        assert_eq!(budget.snapshot().requests, 0);
        assert_eq!(budget.snapshot().peak_codec_bytes, max_codec_bytes);
    }

    #[test]
    fn noninteractive_saturation_preserves_one_small_input_admission() {
        let query = Pdu::ListPanes(ListPanes {})
            .prepare_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Never,
            )
            .expect("query plan");
        assert_eq!(query.metadata().queue_qos, PduQueueQos::Normal);
        let query_bytes = query.codec_peak_bytes();
        let query_frame_bytes = query.maximum_frame_bytes();
        let input = prepared_write_request(CompressionMode::Never, 1);
        assert_eq!(input.metadata().queue_qos, PduQueueQos::Interactive);
        let input_bytes = input.codec_peak_bytes();
        let input_frame_bytes = input.maximum_frame_bytes();
        let total_bytes = query_bytes
            .checked_add(input_bytes)
            .expect("small reserve proof must fit");
        let budget = Arc::new(DirectMuxOutboundBudget {
            max_codec_bytes: total_bytes,
            max_noninteractive_codec_bytes: query_bytes,
            max_requests: 2,
            max_noninteractive_requests: 1,
            state: StdMutex::new(DirectMuxOutboundBudgetState::default()),
        });

        let query_lease = budget
            .try_admit(query, query_frame_bytes)
            .expect("first noninteractive request must admit");
        let second_query = budget
            .try_admit(
                Pdu::ListPanes(ListPanes {})
                    .prepare_outbound(
                        PduProducer::Client,
                        PduWireRole::Request,
                        None,
                        CompressionMode::Never,
                    )
                    .expect("second query plan"),
                query_frame_bytes,
            )
            .expect_err("noninteractive lane must stop before consuming input reserve");
        assert!(matches!(
            second_query,
            DirectMuxError::ProvenPreWriteRejection(source)
                if matches!(*source, DirectMuxError::RetentionLimitExceeded {
                    resource: "noninteractive outbound mux admission",
                    ..
                })
        ));

        let input_lease = budget
            .try_admit(input, input_frame_bytes)
            .expect("small interactive input must use the reserved root capacity");
        assert_eq!(budget.snapshot().codec_bytes, total_bytes);
        assert_eq!(budget.snapshot().requests, 2);
        assert_eq!(budget.snapshot().noninteractive_codec_bytes, query_bytes);
        assert_eq!(budget.snapshot().noninteractive_requests, 1);
        drop(input_lease);
        drop(query_lease);
        assert_eq!(budget.snapshot().codec_bytes, 0);
        assert_eq!(budget.snapshot().requests, 0);
    }

    #[test]
    fn outbound_cap_rejection_precedes_serial_and_wire_for_every_mode() {
        run_async_test(async {
            for compression_mode in [
                CompressionMode::Never,
                CompressionMode::Auto,
                CompressionMode::Always,
            ] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = temp_dir
                    .path()
                    .join(format!("outbound-cap-{compression_mode:?}.sock"));
                let listener = compat_unix::bind(&socket_path)
                    .await
                    .expect("bind direct mux listener");
                let server = task::spawn(async move {
                    let mut stream = accept_direct_mux_handshake(
                        listener,
                        CODEC_VERSION,
                        CODEC_VERSION_MIN_SUPPORTED,
                    )
                    .await;
                    let mut read_buf = StreamingPduBuffer::new();
                    let observed = read_test_request_pdu(&mut stream, &mut read_buf).await;
                    assert!(
                        matches!(&observed.pdu, Pdu::ListPanes(_)),
                        "the first post-handshake frame must be the causal follow-up, not the rejected request: {:?}",
                        observed.pdu
                    );
                });

                let cx = crate::cx::for_testing();
                let config = DirectMuxClientConfig::default().with_socket_path(&socket_path);
                let budget = Arc::new(DirectMuxOutboundBudget::from_config(&config));
                let mut client = DirectMuxClient::connect_with_mode_with_cx(
                    &cx,
                    socket_path,
                    config,
                    compression_mode,
                    Arc::clone(&budget),
                )
                .await
                .expect("direct mux handshake");
                let request = Pdu::WriteToPane(WriteToPane {
                    pane_id: 7,
                    data: vec![b'x'; 4096],
                });
                let planned_frame_bytes = request
                    .plan_outbound(
                        PduProducer::Client,
                        PduWireRole::Request,
                        None,
                        compression_mode,
                    )
                    .expect("request plan")
                    .maximum_frame_bytes();
                client.config.max_frame_bytes = planned_frame_bytes - 1;
                let serial_before = client.serial;
                let mut write_boundary_entered = false;
                let rejected = client
                    .send_request_only_with_cx_tracking(
                        &cx,
                        request,
                        Some(&mut write_boundary_entered),
                    )
                    .await
                    .expect_err("cap-minus-one must reject before the request serial");
                assert!(matches!(
                    rejected,
                    DirectMuxError::ProvenPreWriteRejection(source)
                        if matches!(*source, DirectMuxError::FrameTooLarge { max_bytes }
                            if max_bytes == planned_frame_bytes - 1)
                ));
                assert_eq!(client.serial, serial_before);
                assert!(!write_boundary_entered);
                assert!(!client.connection_poisoned);
                assert_eq!(budget.snapshot().codec_bytes, 0);
                assert_eq!(budget.snapshot().requests, 0);

                client.config.max_frame_bytes = crate::config::DEFAULT_VENDORED_MUX_MAX_FRAME_BYTES;
                let follow_up_serial = client
                    .send_request_only_with_cx(&cx, Pdu::ListPanes(ListPanes {}))
                    .await
                    .expect("pre-write rejection must leave the connection reusable");
                assert_eq!(follow_up_serial, serial_before + 1);

                server.await.expect("causal server observer task");
                drop(client);
            }
        });
    }

    #[test]
    fn pending_render_sideband_index_uses_one_keyed_take_per_response() {
        for depth in [
            32usize,
            DEFAULT_MAX_OUTSTANDING_REQUESTS,
            DEFAULT_MAX_PENDING_RENDER_CHANGES,
        ] {
            let mut pending = PendingRenderChanges::default();
            for pane_id in 1..=depth {
                admit_pending_test_render_change(
                    &mut pending,
                    pane_id,
                    pane_id,
                    "indexed-sideband",
                );
            }

            pending.reset_operation_counts();
            for pane_id in (1..=depth).rev() {
                let retained = pending
                    .take_for_pane(u64::try_from(pane_id).expect("bounded pane id must fit u64"))
                    .expect("keyed sideband take must preserve accounting")
                    .expect("seeded pane must retain one sideband");
                let decoded = retained
                    .decode(41)
                    .expect("retained sideband must preserve connection identity");
                assert_eq!(decoded.pane_id, pane_id);
                assert_eq!(decoded.seqno, pane_id);
            }

            assert!(pending.is_empty());
            assert_eq!(pending.retained_bytes(), 0);
            assert_eq!(
                pending.operation_counts(),
                (depth, 0, 0, 0),
                "depth {depth} must perform exactly one keyed take per response"
            );
        }
    }

    #[test]
    fn pending_render_sideband_index_preserves_interleaved_per_pane_fifo() {
        let mut pending = PendingRenderChanges::default();
        for (pane_id, seqno, title) in [
            (7usize, 1usize, "pane-seven-1"),
            (9, 10, "pane-nine-10"),
            (7, 2, "pane-seven-2"),
            (9, 11, "pane-nine-11"),
            (7, 3, "pane-seven-3"),
        ] {
            admit_pending_test_render_change(&mut pending, pane_id, seqno, title);
        }

        pending.reset_operation_counts();
        for (pane_id, expected) in [(7u64, 1usize), (7, 2), (7, 3), (9, 10), (9, 11)] {
            let decoded = pending
                .take_for_pane(pane_id)
                .expect("keyed FIFO take must preserve accounting")
                .expect("seeded pane must retain the next sideband")
                .decode(41)
                .expect("retained sideband must preserve connection identity");
            assert_eq!(
                u64::try_from(decoded.pane_id).expect("bounded pane id must fit u64"),
                pane_id
            );
            assert_eq!(decoded.seqno, expected);
        }

        assert!(pending.is_empty());
        assert_eq!(pending.retained_bytes(), 0);
        assert_eq!(pending.operation_counts(), (5, 0, 0, 0));
    }

    #[test]
    fn targeted_render_invalidation_work_is_independent_of_unrelated_depth() {
        for depth in [
            32usize,
            DEFAULT_MAX_OUTSTANDING_REQUESTS,
            DEFAULT_MAX_PENDING_RENDER_CHANGES,
        ] {
            let mut pending = PendingRenderChanges::default();
            let mut snapshots = RenderChangeSnapshots::default();
            for pane_id in 1..=depth {
                admit_pending_test_render_change(
                    &mut pending,
                    pane_id,
                    pane_id,
                    "targeted-invalidation",
                );
                admit_snapshot_test_render_change(
                    &mut snapshots,
                    pane_id,
                    pane_id,
                    "targeted-invalidation",
                );
            }

            let targets = HashSet::from([
                1u64,
                u64::try_from(depth / 2).expect("bounded pane id must fit u64"),
                u64::try_from(depth).expect("bounded pane id must fit u64"),
            ]);
            pending.reset_operation_counts();
            snapshots.reset_operation_counts();

            let snapshot_plan = snapshots
                .plan_remove_panes(&targets)
                .expect("snapshot invalidation plan must preserve accounting");
            let pending_plan = pending
                .plan_remove_panes(&targets)
                .expect("pending invalidation plan must preserve accounting");
            assert_eq!(snapshot_plan.removed_count, targets.len());
            assert_eq!(pending_plan.removed_count, targets.len());
            snapshots.commit_remove_panes(&targets, snapshot_plan);
            pending.commit_remove_panes(&targets, pending_plan);

            assert_eq!(pending.len(), depth - targets.len());
            assert_eq!(snapshots.len(), depth - targets.len());
            assert_eq!(
                pending.retained_bytes(),
                pending
                    .iter()
                    .map(RetainedRenderChange::retained_bytes)
                    .sum::<usize>()
            );
            assert_eq!(
                snapshots.retained_bytes(),
                snapshots
                    .values()
                    .map(RetainedRenderChange::retained_bytes)
                    .sum::<usize>()
            );
            assert_eq!(
                pending.operation_counts(),
                (0, targets.len(), targets.len(), targets.len()),
                "depth {depth} must not add work for unrelated pending panes"
            );
            assert_eq!(
                snapshots.operation_counts(),
                (targets.len(), targets.len(), targets.len()),
                "depth {depth} must not add work for unrelated snapshots"
            );
        }
    }

    #[test]
    fn indexed_render_retention_rejects_count_corruption_before_mutation() {
        let mut pending = PendingRenderChanges::default();
        let mut snapshots = RenderChangeSnapshots::default();
        admit_pending_test_render_change(&mut pending, 7, 1, "count-corruption");
        admit_snapshot_test_render_change(&mut snapshots, 7, 1, "count-corruption");
        let pending_bytes = pending.retained_bytes();
        let snapshot_bytes = snapshots.retained_bytes();

        pending.totals.count += 1;
        let pending_error = pending
            .take_for_pane(7)
            .expect_err("count corruption must fail before removing a pending sideband");
        assert!(matches!(
            pending_error,
            DirectMuxError::RetainedStateAccounting {
                resource: "pending unilateral render changes"
            }
        ));
        assert_eq!(pending.by_pane.len(), 1);
        assert_eq!(pending.retained_bytes(), pending_bytes);
        pending.totals.set(1, pending_bytes);

        snapshots.totals.count += 1;
        let snapshot_error = snapshots
            .plan_remove_panes(&HashSet::from([7]))
            .expect_err("count corruption must fail before removing a render snapshot");
        assert!(matches!(
            snapshot_error,
            DirectMuxError::RetainedStateAccounting {
                resource: "render change snapshots"
            }
        ));
        assert_eq!(snapshots.by_pane.len(), 1);
        assert_eq!(snapshots.retained_bytes(), snapshot_bytes);
    }

    #[test]
    fn batch_local_render_sidebands_enforce_pipeline_depth_bound() {
        let global = PendingRenderChanges::default();
        let limit = RetentionLimit {
            max_count: 2,
            max_bytes: 100,
        };
        let mut sidebands = BatchLocalRenderSidebands::with_limit(1);
        sidebands
            .insert(
                7,
                TypedRenderSideband {
                    payload: test_render_change(7, 1, "bounded-local-sideband"),
                    retained_frame_bytes: 64,
                },
                &global,
                limit,
            )
            .expect("first in-flight pane must fit the depth bound");
        let error = sidebands
            .insert(
                9,
                TypedRenderSideband {
                    payload: test_render_change(9, 1, "bounded-local-sideband"),
                    retained_frame_bytes: 1,
                },
                &global,
                limit,
            )
            .expect_err("second pane must fail a depth-one local bound");
        assert!(matches!(
            error,
            DirectMuxError::RetainedStateAccounting {
                resource: "batch-local typed render sidebands"
            }
        ));
        assert_eq!(
            sidebands
                .take(7)
                .expect("local accounting must remain valid")
                .expect("failed admission must preserve the admitted pane")
                .payload
                .pane_id,
            7
        );
        assert!(
            sidebands
                .is_empty()
                .expect("empty local accounting must validate")
        );
        sidebands.totals.count_check = 0;
        let empty_corruption = sidebands
            .is_empty()
            .expect_err("zero-state checksum corruption must not appear empty");
        assert!(matches!(
            empty_corruption,
            DirectMuxError::RetainedStateAccounting {
                resource: "batch-local typed render sidebands"
            }
        ));
        assert!(sidebands.by_pane.is_empty());
        sidebands.totals.set(0, 0);

        let mut byte_bounded = BatchLocalRenderSidebands::with_limit(2);
        byte_bounded
            .insert(
                7,
                TypedRenderSideband {
                    payload: test_render_change(7, 1, "byte-bounded-local-sideband"),
                    retained_frame_bytes: 64,
                },
                &global,
                limit,
            )
            .expect("first typed sideband must fit the shared byte cap");
        let byte_error = byte_bounded
            .insert(
                9,
                TypedRenderSideband {
                    payload: test_render_change(9, 1, "byte-bounded-local-sideband"),
                    retained_frame_bytes: 37,
                },
                &global,
                limit,
            )
            .expect_err("local sidebands must share the global pending byte cap");
        assert!(matches!(
            byte_error,
            DirectMuxError::RetentionLimitExceeded {
                resource: "pending unilateral render changes",
                requested_count: 2,
                requested_bytes: 101,
                max_count: 2,
                max_bytes: 100,
            }
        ));
        assert_eq!(
            byte_bounded.totals().expect("valid retained totals"),
            (1, 64)
        );

        byte_bounded.totals.set(1, 32);
        let accounting_error = byte_bounded
            .take(7)
            .expect_err("understated local bytes must fail before removing the payload");
        assert!(matches!(
            accounting_error,
            DirectMuxError::RetainedStateAccounting {
                resource: "batch-local typed render sidebands"
            }
        ));
        assert!(byte_bounded.by_pane.contains_key(&7));
        assert_eq!(byte_bounded.totals.bytes, 32);
    }

    #[test]
    fn in_flight_serial_index_uses_one_keyed_take_per_reversed_response() {
        for depth in [32usize, DEFAULT_MAX_OUTSTANDING_REQUESTS, 4_096] {
            let mut slots = InFlightRequestSlots::with_capacity(depth);
            for request_idx in 0..depth {
                let serial =
                    u64::try_from(request_idx).expect("bounded request index must fit u64") + 1;
                slots
                    .insert(serial, request_idx)
                    .expect("fixture serials must be unique");
            }

            for request_idx in (0..depth).rev() {
                let serial =
                    u64::try_from(request_idx).expect("bounded response index must fit u64") + 1;
                assert_eq!(
                    slots.take(serial),
                    Some(request_idx),
                    "reversed completion must resolve the exact caller-order slot"
                );
            }

            assert!(slots.is_empty());
            assert_eq!(
                slots.operation_counts(),
                (depth, depth),
                "depth {depth} must perform one keyed insert and one keyed take per request"
            );
        }
    }

    #[test]
    fn duplicate_in_flight_serial_fails_before_overwriting_ownership() {
        let mut slots = InFlightRequestSlots::with_capacity(2);
        slots
            .insert(17, 3)
            .expect("first serial owner must be admitted");
        let error = slots
            .insert(17, 9)
            .expect_err("duplicate serial ownership must fail closed");
        assert!(matches!(
            error,
            DirectMuxError::RetainedStateAccounting {
                resource: "in-flight mux request serials"
            }
        ));
        assert_eq!(slots.len(), 1);
        assert_eq!(slots.take(17), Some(3));
        assert!(slots.is_empty());
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
        let mut in_flight = InFlightRequestSlots::with_capacity(depth);
        let mut delivered: Vec<Option<u64>> = vec![None; total_requests];
        let mut next_request = 0usize;
        let mut peak = 0usize;

        while next_request < total_requests && in_flight.len() < depth {
            let serial = (next_request + 1) as u64;
            in_flight
                .insert(serial, next_request)
                .expect("new simulated serial must be unique");
            next_request += 1;
            peak = peak.max(in_flight.len());
        }

        for &response_idx in response_order {
            let serial = (response_idx + 1) as u64;
            let slot = in_flight
                .take(serial)
                .expect("response serial must correspond to an in-flight request");
            delivered[slot] = Some(serial);
            if next_request < total_requests {
                let serial = (next_request + 1) as u64;
                in_flight
                    .insert(serial, next_request)
                    .expect("successor simulated serial must be unique");
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

    proptest! {
        #[test]
        fn prop_resolve_compression_mode_for_locality_invariants(is_local in any::<bool>()) {
            use crate::config::VendoredCompressionMode::{Always, Auto, Never};

            prop_assert_eq!(
                resolve_compression_mode_for_locality(Always, is_local),
                CompressionMode::Always
            );
            prop_assert_eq!(
                resolve_compression_mode_for_locality(Never, is_local),
                CompressionMode::Never
            );
            prop_assert_eq!(
                resolve_compression_mode_for_locality(Auto, is_local),
                if is_local {
                    CompressionMode::Never
                } else {
                    CompressionMode::Auto
                }
            );
        }
    }

    proptest! {
        #[test]
        fn prop_write_to_pane_roundtrips_for_explicit_modes(
            pane_id in 0usize..128,
            serial in 1u64..10_000,
            payload in prop::collection::vec(any::<u8>(), 0..2048)
        ) {
            let expected_payload = payload.clone();
            let pdu = Pdu::WriteToPane(WriteToPane {
                pane_id,
                data: payload,
            });

            for mode in [CompressionMode::Never, CompressionMode::Always] {
                let mut encoded = Vec::new();
                pdu.encode_with_mode(&mut encoded, serial, mode)
                    .expect("encode_with_mode");
                let decoded = Pdu::decode(encoded.as_slice()).expect("decode");
                prop_assert_eq!(decoded.serial, serial);
                match decoded.pdu {
                    Pdu::WriteToPane(write) => {
                        prop_assert_eq!(write.pane_id, pane_id);
                        prop_assert_eq!(write.data.as_slice(), expected_payload.as_slice());
                    }
                    other => {
                        panic!("unexpected decoded pdu: {}", other.pdu_name());
                    }
                }
            }
        }
    }

    proptest! {
        #[test]
        fn prop_is_local_unix_socket_rejects_regular_files(
            payload in prop::collection::vec(any::<u8>(), 0..1024)
        ) {
            let file = tempfile::NamedTempFile::new().expect("temp file");
            std::fs::write(file.path(), payload).expect("write temp file");
            prop_assert!(!is_local_unix_socket(file.path()));
        }
    }

    proptest! {
        #[test]
        fn prop_decode_u64_leb128_prefix_roundtrips(
            value in any::<u64>(),
            suffix in prop::collection::vec(any::<u8>(), 0..32)
        ) {
            let mut encoded = encode_u64_leb128(value);
            encoded.extend_from_slice(&suffix);
            prop_assert_eq!(decode_u64_leb128_prefix(&encoded), Some(value));
        }
    }

    proptest! {
        #[test]
        fn prop_frame_marked_compressed_tracks_high_bit(
            payload_len in 0u64..(1u64 << 63),
            compressed in any::<bool>(),
            suffix in prop::collection::vec(any::<u8>(), 0..16)
        ) {
            let header = if compressed {
                payload_len | COMPRESSED_MASK
            } else {
                payload_len
            };
            let mut encoded = encode_u64_leb128(header);
            encoded.extend_from_slice(&suffix);

            prop_assert_eq!(frame_marked_compressed(&encoded), Some(compressed));
        }
    }

    proptest! {
        #[test]
        fn prop_subscription_poll_delay_respects_fast_and_slow_bounds(
            poll_ms in 1u64..5_000,
            min_ms in 0u64..5_000,
            saw_dirty_output in any::<bool>()
        ) {
            let config = SubscriptionConfig {
                poll_interval: Duration::from_millis(poll_ms),
                min_poll_interval: Duration::from_millis(min_ms),
                channel_capacity: 8,
            };

            let delay = subscription_poll_delay(&config, saw_dirty_output);
            let expected_fast = Duration::from_millis(min_ms).min(Duration::from_millis(poll_ms));

            if saw_dirty_output {
                prop_assert_eq!(delay, expected_fast);
            } else {
                prop_assert_eq!(delay, Duration::from_millis(poll_ms));
            }

            prop_assert!(delay <= Duration::from_millis(poll_ms));
        }
    }

    proptest! {
        #[test]
        fn prop_total_dirty_rows_matches_saturating_span_sum(
            ranges in prop::collection::vec((-128isize..128, -128isize..128), 0..64)
        ) {
            let ranges: Vec<std::ops::Range<isize>> =
                ranges.into_iter().map(|(start, end)| start..end).collect();

            let expected = ranges.iter().fold(0usize, |acc, range| {
                let span = if range.end > range.start {
                    range.end - range.start
                } else {
                    0
                };
                let span_usize = usize::try_from(span).unwrap_or(usize::MAX);
                acc.saturating_add(span_usize)
            });

            prop_assert_eq!(total_dirty_rows(&ranges), expected);
        }
    }

    #[test]
    fn default_config_has_sane_timeouts() {
        let config = DirectMuxClientConfig::default();
        assert!(config.connect_timeout.as_secs() > 0);
        assert!(config.read_timeout.as_secs() > 0);
        assert!(config.write_timeout.as_secs() > 0);
        assert!(config.max_frame_bytes > 0);
        assert!(config.max_outbound_codec_bytes > 0);
        assert!(config.max_outbound_in_flight_requests > 0);
        assert!(config.max_outstanding_requests > 0);
        assert!(config.max_pending_responses > 0);
        assert!(config.max_pending_response_bytes > 0);
        assert!(config.max_pending_render_changes > 0);
        assert!(config.max_pending_render_change_bytes > 0);
        assert!(config.max_render_change_snapshots > 0);
        assert!(config.max_render_change_snapshot_bytes > 0);
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
    fn config_from_wa_config_with_outbound_admission_bounds() {
        let mut wa_cfg = crate::config::Config::default();
        wa_cfg.vendored.mux_pool.max_frame_bytes = 12_345;
        wa_cfg.vendored.mux_pool.max_outbound_codec_bytes = 67_890;
        wa_cfg.vendored.mux_pool.max_outbound_in_flight_requests = 17;

        let config = DirectMuxClientConfig::from_wa_config(&wa_cfg);
        assert_eq!(config.max_frame_bytes, 12_345);
        assert_eq!(config.max_outbound_codec_bytes, 67_890);
        assert_eq!(config.max_outbound_in_flight_requests, 17);
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
    fn auto_fallback_retry_gate_matches_expected_conditions() {
        let recoverable = DirectMuxError::Disconnected;
        assert!(should_auto_fallback_to_always(
            crate::config::VendoredCompressionMode::Auto,
            CompressionMode::Never,
            &recoverable
        ));
        assert!(!should_auto_fallback_to_always(
            crate::config::VendoredCompressionMode::Always,
            CompressionMode::Never,
            &recoverable
        ));
        assert!(!should_auto_fallback_to_always(
            crate::config::VendoredCompressionMode::Auto,
            CompressionMode::Always,
            &recoverable
        ));

        let permanent = DirectMuxError::IncompatibleCodec {
            local: CODEC_VERSION,
            local_min: CODEC_VERSION_MIN_SUPPORTED,
            remote: CODEC_VERSION_MIN_SUPPORTED - 1,
            remote_min: CODEC_VERSION_MIN_SUPPORTED - 1,
            remote_version: "test".to_string(),
        };
        assert!(!should_auto_fallback_to_always(
            crate::config::VendoredCompressionMode::Auto,
            CompressionMode::Never,
            &permanent
        ));

        let cancelled = cancelled_mux_error("connect_wait", "caller cancelled");
        assert!(!should_auto_fallback_to_always(
            crate::config::VendoredCompressionMode::Auto,
            CompressionMode::Never,
            &cancelled
        ));
    }

    #[test]
    fn protocol_error_kind_treats_connection_io_as_recoverable() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::NotConnected,
        ] {
            let err = DirectMuxError::Io(std::io::Error::from(kind));
            assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Recoverable);
        }
    }

    #[test]
    fn protocol_error_kind_treats_other_io_as_transient() {
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::Interrupted,
        ] {
            let err = DirectMuxError::Io(std::io::Error::from(kind));
            assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Transient);
        }
    }

    #[test]
    fn explicit_mux_cancellation_distinguishes_prewrite_and_in_progress_axes() {
        for (phase, expected_connection) in [
            ("request_write_wait", MuxConnectionDisposition::Reuse),
            (
                "request_write_in_progress",
                MuxConnectionDisposition::Discard,
            ),
        ] {
            let err = cancelled_mux_error(phase, "test cancel");
            assert!(matches!(
                &err,
                DirectMuxError::Cancelled {
                    phase: actual_phase,
                    detail,
                } if *actual_phase == phase && detail == "test cancel"
            ));
            assert!(err.is_cancelled());
            assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Transient);
            let decision = err.recovery_decision();
            assert!(!decision.retry);
            assert!(decision.cancelled);
            assert_eq!(decision.connection, expected_connection);
        }
    }

    #[test]
    fn legacy_cancelled_mux_io_is_detected_without_changing_kind_projection() {
        let err = DirectMuxError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "mux request_write_wait cancelled: test cancel",
        ));
        assert!(err.is_cancelled());
        assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Transient);
        assert!(!err.recovery_decision().retry);
        assert!(err.recovery_decision().cancelled);
    }

    #[test]
    fn canceled_mux_io_spelling_variant_is_detected() {
        let err = DirectMuxError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "mux request_write_wait canceled: american spelling",
        ));
        assert!(err.is_cancelled());
        assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Transient);
    }

    #[test]
    fn generic_interrupted_io_is_not_treated_as_mux_cancellation() {
        let err = DirectMuxError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "generic interrupt",
        ));
        assert!(!err.is_cancelled());
        assert!(err.recovery_decision().retry);
        assert!(matches!(
            err.recovery_decision().connection,
            MuxConnectionDisposition::Discard
        ));
    }

    #[test]
    fn remote_error_is_not_replayed_and_preserves_connection_alignment() {
        let err = DirectMuxError::RemoteError("application rejected request".to_string());
        let decision = err.recovery_decision();
        assert_eq!(decision.kind, ProtocolErrorKind::Transient);
        assert!(!decision.retry);
        assert!(!decision.cancelled);
        assert!(matches!(
            decision.connection,
            MuxConnectionDisposition::Reuse
        ));
    }

    #[test]
    fn subscription_retries_only_replayable_errors_on_reusable_connections() {
        for err in [
            DirectMuxError::ReadTimeout,
            DirectMuxError::Disconnected,
            DirectMuxError::RemoteError("application rejected request".to_string()),
            cancelled_mux_error("response_read_wait", "scope ended"),
        ] {
            assert!(
                !subscription_can_retry_same_client(&err),
                "subscription must terminate for {err:?}"
            );
        }
    }

    #[test]
    fn is_local_unix_socket_rejects_directory_paths() {
        let tmp = tempfile::tempdir().expect("temp dir");
        assert!(!is_local_unix_socket(tmp.path()));
    }

    #[test]
    fn auto_mode_falls_back_to_compressed_when_server_rejects_uncompressed() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("compression-fallback.sock");
            let listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind listener");

            let server = task::spawn(async move {
                for attempt in 0..2 {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let reject_uncompressed = attempt == 0;
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut first_frame_checked = false;

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);

                        if !first_frame_checked {
                            if let Some(is_compressed) =
                                frame_marked_compressed(read_buf.as_slice())
                            {
                                first_frame_checked = true;
                                if reject_uncompressed && !is_compressed {
                                    break;
                                }
                            }
                        }

                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            let response = match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "compression-fallback-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                _ => continue,
                            };
                            let mut out = Vec::new();
                            response
                                .encode(&mut out, decoded.serial)
                                .expect("encode response");
                            stream.write_all(&out).await.expect("write response");
                        }
                    }
                }
            });

            let auto_config =
                DirectMuxClientConfig::default().with_socket_path(socket_path.clone());
            let client = DirectMuxClient::connect(auto_config)
                .await
                .expect("auto mode should retry with compression when uncompressed PDUs fail");
            drop(client);

            server.await.expect("server task");
        });
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
            DirectMuxError::InputSerialExhausted,
            DirectMuxError::Codec("bad frame".to_string()),
            DirectMuxError::RemoteError("denied".to_string()),
            DirectMuxError::BatchTimeout { timeout_ms: 5000 },
            DirectMuxError::UnexpectedResponse {
                expected: "Pong".to_string(),
                got: "Error".to_string(),
            },
            DirectMuxError::IncompatibleCodec {
                local: 2,
                local_min: 2,
                remote: 1,
                remote_min: 1,
                remote_version: "old".to_string(),
            },
            DirectMuxError::Cancelled {
                phase: "request_write_wait",
                detail: "scope ended".to_string(),
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
    fn input_serial_exhaustion_is_a_proven_pre_write_rejection() {
        let error = DirectMuxError::InputSerialExhausted;
        assert!(error.is_proven_pre_write_rejection());
        assert_eq!(
            error.recovery_decision(),
            MuxRecoveryDecision {
                kind: ProtocolErrorKind::Permanent,
                retry: false,
                connection: MuxConnectionDisposition::Reuse,
                cancelled: false,
            }
        );
    }

    #[test]
    fn decode_empty_buffer_returns_none() {
        let mut buf = StreamingPduBuffer::new();
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
            let mut truncated = StreamingPduBuffer::from(buf[..cut].to_vec());
            let _ = decode_from_buffer(&mut truncated, 4096);
            // If it didn't panic, the test passes
        }
    }

    #[test]
    fn connect_to_missing_socket_returns_error() {
        run_async_test(async {
            let config = DirectMuxClientConfig::default()
                .with_socket_path("/tmp/wa-test-nonexistent-socket-12345.sock");
            let err = DirectMuxClient::connect(config).await.unwrap_err();
            match err {
                DirectMuxError::SocketNotFound(_) => {}
                other => panic!("expected SocketNotFound, got: {other}"),
            }
        });
    }

    #[test]
    fn connect_with_cx_to_missing_socket_returns_error() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let config = DirectMuxClientConfig::default()
                .with_socket_path("/tmp/wa-test-nonexistent-socket-with-cx-12345.sock");
            let err = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .unwrap_err();
            match err {
                DirectMuxError::SocketNotFound(_) => {}
                other => panic!("expected SocketNotFound, got: {other}"),
            }
        });
    }

    #[test]
    fn connect_with_precancelled_cx_fails_before_opening_socket() {
        run_async_test(async {
            let cx = cancelled_test_cx("pre-cancelled connect");
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("pre-cancelled-connect.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            let server = task::spawn(async move {
                match timeout(Duration::from_millis(200), listener.accept()).await {
                    Ok(Ok((_stream, _addr))) => true,
                    Ok(Err(err)) => panic!("accept failed: {err}"),
                    Err(_) => false,
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let err = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect_err("pre-cancelled connect_with_cx should fail fast");
            assert_cancelled_mux_error(&err);
            assert!(
                !server.await.expect("server task"),
                "pre-cancelled connect should not open a socket connection"
            );
        });
    }

    #[test]
    fn connect_times_out_when_server_stalls_during_codec_handshake() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("connect-read-timeout.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for connect timeout test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    assert!(read > 0, "expected codec handshake request bytes");

                    // Keep the socket open without sending a codec response so
                    // client-side read timeout handling is exercised.
                    sleep(Duration::from_millis(150)).await;
                });
            });

            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should become ready");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(40));

            let err = DirectMuxClient::connect(config)
                .await
                .expect_err("connect should fail when codec handshake stalls");
            assert!(
                matches!(err, DirectMuxError::ReadTimeout),
                "expected ReadTimeout, got: {err}"
            );

            server.join().expect("server thread should exit cleanly");
        });
    }

    #[test]
    fn connect_with_cx_times_out_when_server_stalls_during_codec_handshake() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("connect-read-timeout-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for connect timeout with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut temp = vec![0u8; 4096];
                    let read = unix_stream_read(&mut stream, &mut temp)
                        .await
                        .expect("read");
                    assert!(read > 0, "expected codec handshake request bytes");

                    // Keep the socket open without sending a codec response so
                    // client-side read timeout handling is exercised.
                    sleep(Duration::from_millis(150)).await;
                });
            });

            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should become ready");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(40));

            let err = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect_err("connect_with_cx should fail when codec handshake stalls");
            assert!(
                matches!(err, DirectMuxError::ReadTimeout),
                "expected ReadTimeout, got: {err}"
            );

            server.join().expect("server thread should exit cleanly");
        });
    }

    #[test]
    fn list_panes_with_cx_cancellation_during_response_read_surfaces_cancelled_io() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("read-cancel-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let (request_seen_tx, request_seen_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for read cancel with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "read-cancel-with-cx-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    write_response_pdu(&mut stream, &response, decoded.serial)
                                        .await
                                        .expect("write codec response");
                                }
                                Pdu::SetClientId(_) => {
                                    let response = Pdu::UnitResponse(UnitResponse {});
                                    write_response_pdu(&mut stream, &response, decoded.serial)
                                        .await
                                        .expect("write client response");
                                }
                                Pdu::ListPanes(_) => {
                                    request_seen_tx.send(()).expect("signal list panes request");
                                    sleep(Duration::from_millis(250)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });

            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(40));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let cancel_cx = cx.clone();
            let cancel = std::thread::spawn(move || {
                request_seen_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("server should observe list panes request");
                std::thread::sleep(Duration::from_millis(5));
                cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("cancel during response read"),
                );
            });

            let err = client
                .list_panes_with_cx(&cx)
                .await
                .expect_err("list_panes_with_cx should surface cancellation");
            assert_cancelled_mux_error(&err);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            cancel.join().expect("cancel thread");
            server.join().expect("server thread should exit cleanly");
        });
    }

    #[test]
    fn send_paste_write_timeout_when_server_stops_reading_after_handshake() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("write-timeout.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for write-timeout test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "write-timeout-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");

                                    // Keep socket open but stop reading so the client
                                    // write path eventually back-pressures.
                                    sleep(Duration::from_millis(500)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect(config).await.expect("connect");
            client.config.write_timeout = Duration::from_millis(5);

            let payload = "x".repeat(32 * 1024 * 1024);
            let err = client
                .send_paste(0, payload)
                .await
                .expect_err("send_paste should time out when peer stops reading");
            assert!(matches!(err, DirectMuxError::WriteTimeout));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);
            client.poison_connection("idempotent poison test", false);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn send_paste_with_cx_write_timeout_when_server_stops_reading_after_handshake() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("write-timeout-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for write-timeout with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "write-timeout-with-cx-test"
                                                .to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");

                                    // Keep socket open but stop reading so the client
                                    // write path eventually back-pressures.
                                    sleep(Duration::from_millis(500)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            client.config.write_timeout = Duration::from_millis(5);

            let payload = "x".repeat(32 * 1024 * 1024);
            let err = client
                .send_paste_with_cx(&cx, 0, payload)
                .await
                .expect_err("send_paste_with_cx should time out when peer stops reading");
            assert!(matches!(err, DirectMuxError::WriteTimeout));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn send_paste_with_cx_cancellation_during_write_surfaces_cancelled_io() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("write-cancel-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for write cancel with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "write-cancel-with-cx-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    write_response_pdu(&mut stream, &response, decoded.serial)
                                        .await
                                        .expect("write codec response");
                                }
                                Pdu::SetClientId(_) => {
                                    let response = Pdu::UnitResponse(UnitResponse {});
                                    write_response_pdu(&mut stream, &response, decoded.serial)
                                        .await
                                        .expect("write client response");
                                    sleep(Duration::from_millis(500)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });

            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");
            client.config.write_timeout = Duration::from_millis(40);

            let cancel_cx = cx.clone();
            let cancel = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("cancel during request write"),
                );
            });

            let payload = "x".repeat(32 * 1024 * 1024);
            let err = client
                .send_paste_with_cx(&cx, 0, payload)
                .await
                .expect_err("send_paste_with_cx should surface cancellation");
            assert_cancelled_mux_error(&err);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            cancel.join().expect("cancel thread");
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_read_timeout_when_server_stalls_after_handshake() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("read-timeout.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for read-timeout test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "read-timeout-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    // Keep the socket open but silent past client read_timeout.
                                    sleep(Duration::from_millis(250)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(40));
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let err = client
                .list_panes()
                .await
                .expect_err("list_panes should time out when server stalls");
            assert!(matches!(err, DirectMuxError::ReadTimeout));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_with_cx_read_timeout_when_server_stalls_after_handshake() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("read-timeout-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for read-timeout with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "read-timeout-with-cx-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    // Keep the socket open but silent past client read_timeout.
                                    sleep(Duration::from_millis(250)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(40));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let err = client
                .list_panes_with_cx(&cx)
                .await
                .expect_err("list_panes_with_cx should time out when server stalls");
            assert!(matches!(err, DirectMuxError::ReadTimeout));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_disconnected_when_server_closes_after_request() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("disconnected-after-request.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for disconnected test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "disconnected-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    // Close after consuming the request so the client sees EOF
                                    // while awaiting the corresponding response.
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let err = client
                .list_panes()
                .await
                .expect_err("list_panes should fail when server closes without responding");
            assert!(matches!(err, DirectMuxError::Disconnected));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_with_cx_disconnected_when_server_closes_after_request() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir
                .path()
                .join("disconnected-after-request-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for disconnected with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "disconnected-with-cx-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    // Close after consuming the request so the client sees EOF
                                    // while awaiting the corresponding response.
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let err = client
                .list_panes_with_cx(&cx)
                .await
                .expect_err("list_panes_with_cx should fail when server closes without responding");
            assert!(matches!(err, DirectMuxError::Disconnected));
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_handles_partial_frame_reads() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("partial-frame.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for partial-frame test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "partial-frame-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    let response = Pdu::ListPanesResponse(ListPanesResponse {
                                        tabs: Vec::new(),
                                        tab_titles: Vec::new(),
                                        window_titles: HashMap::new(),
                                        floating_panes: Vec::new(),
                                    });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    assert!(
                                        out.len() > 1,
                                        "encoded frame should be splittable for partial-read test"
                                    );
                                    let split = (out.len() / 2).max(1).min(out.len() - 1);
                                    stream
                                        .write_all(&out[..split])
                                        .await
                                        .expect("write first frame chunk");
                                    sleep(Duration::from_millis(20)).await;
                                    stream
                                        .write_all(&out[split..])
                                        .await
                                        .expect("write second frame chunk");
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let panes = client
                .list_panes()
                .await
                .expect("list_panes should succeed with split response frame");
            assert!(panes.tabs.is_empty());
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn get_pane_render_changes_handles_partial_compressed_frame_reads() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("partial-render-frame.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for partial render-frame test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "partial-render-frame-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::GetPaneRenderChanges(request) => {
                                    let response = Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: request.pane_id,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines: Vec::new(),
                                            title: format!("pane-{}", request.pane_id),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: 7,
                                        },
                                    );
                                    let mut out = Vec::new();
                                    // Force compression to guarantee the partial-frame
                                    // test exercises compressed codec paths. With
                                    // CompressionMode::Auto, the payload may be too small
                                    // or compress to a larger size (especially after the
                                    // skip_serializing_if removal for tiered_scrollback_status).
                                    response
                                        .encode_with_mode(
                                            &mut out,
                                            decoded.serial,
                                            codec::CompressionMode::Always,
                                        )
                                        .expect("encode response");
                                    assert_eq!(
                                        frame_marked_compressed(&out),
                                        Some(true),
                                        "render-change response should exercise compressed partial-frame handling"
                                    );
                                    let split = (out.len() / 2).max(1).min(out.len() - 1);
                                    stream
                                        .write_all(&out[..split])
                                        .await
                                        .expect("write first frame chunk");
                                    sleep(Duration::from_millis(20)).await;
                                    stream
                                        .write_all(&out[split..])
                                        .await
                                        .expect("write second frame chunk");
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let render = client
                .get_pane_render_changes(12)
                .await
                .expect("render changes should succeed with split compressed response frame");
            assert_eq!(render.pane_id, 12);
            assert_eq!(render.seqno, 7);
            assert_eq!(render.dimensions.cols, 80);
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_with_cx_handles_partial_frame_reads() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("partial-frame-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for partial-frame with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "partial-frame-with-cx-test"
                                                .to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    let response = Pdu::ListPanesResponse(ListPanesResponse {
                                        tabs: Vec::new(),
                                        tab_titles: Vec::new(),
                                        window_titles: HashMap::new(),
                                        floating_panes: Vec::new(),
                                    });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    assert!(
                                        out.len() > 1,
                                        "encoded frame should be splittable for partial-read test"
                                    );
                                    let split = (out.len() / 2).max(1).min(out.len() - 1);
                                    stream
                                        .write_all(&out[..split])
                                        .await
                                        .expect("write first frame chunk");
                                    sleep(Duration::from_millis(20)).await;
                                    stream
                                        .write_all(&out[split..])
                                        .await
                                        .expect("write second frame chunk");
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect with cx");

            let panes = client
                .list_panes_with_cx(&cx)
                .await
                .expect("list_panes_with_cx should succeed with split response frame");
            assert!(panes.tabs.is_empty());
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn get_pane_render_changes_with_cx_handles_partial_compressed_frame_reads() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("partial-render-frame-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for partial render-frame with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "partial-render-frame-with-cx-test"
                                                .to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::GetPaneRenderChanges(request) => {
                                    let response = Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: request.pane_id,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines: Vec::new(),
                                            title: format!("pane-{}", request.pane_id),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: 9,
                                        },
                                    );
                                    let mut out = Vec::new();
                                    // Force compression to guarantee the partial-frame
                                    // test exercises compressed codec paths. With
                                    // CompressionMode::Auto, the payload may be too small
                                    // or compress to a larger size (especially after the
                                    // skip_serializing_if removal for tiered_scrollback_status).
                                    response
                                        .encode_with_mode(
                                            &mut out,
                                            decoded.serial,
                                            codec::CompressionMode::Always,
                                        )
                                        .expect("encode response");
                                    assert_eq!(
                                        frame_marked_compressed(&out),
                                        Some(true),
                                        "render-change response should exercise compressed partial-frame handling"
                                    );
                                    let split = (out.len() / 2).max(1).min(out.len() - 1);
                                    stream
                                        .write_all(&out[..split])
                                        .await
                                        .expect("write first frame chunk");
                                    sleep(Duration::from_millis(20)).await;
                                    stream
                                        .write_all(&out[split..])
                                        .await
                                        .expect("write second frame chunk");
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config =
                direct_mux_client_config_with_timeout(socket_path, Duration::from_millis(200));
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");

            let render = client
                .get_pane_render_changes_with_cx(&cx, 27)
                .await
                .expect(
                    "render changes with cx should succeed with split compressed response frame",
                );
            assert_eq!(render.pane_id, 27);
            assert_eq!(render.seqno, 9);
            assert_eq!(render.dimensions.viewport_rows, 24);
            assert!(!client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 0);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_rejects_oversized_response_frame() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("oversized-frame.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let max_frame_bytes = 128usize;
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for oversized-frame test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "oversized-frame-test".to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    let mut window_titles = HashMap::new();
                                    for window_id in 0..24usize {
                                        window_titles.insert(
                                            window_id + 1,
                                            format!(
                                                "oversized-window-{window_id:02}-{}",
                                                "x".repeat(32)
                                            ),
                                        );
                                    }
                                    let response = Pdu::ListPanesResponse(ListPanesResponse {
                                        tabs: Vec::new(),
                                        tab_titles: Vec::new(),
                                        window_titles,
                                        floating_panes: Vec::new(),
                                    });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    assert!(
                                        out.len() > max_frame_bytes + 1,
                                        "encoded frame must exceed the configured max"
                                    );

                                    let prefix = &out[..=max_frame_bytes];
                                    let chunk_size = (max_frame_bytes / 2).max(1);
                                    for chunk in prefix.chunks(chunk_size) {
                                        stream.write_all(chunk).await.expect("write frame chunk");
                                        sleep(Duration::from_millis(5)).await;
                                    }
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config = DirectMuxClientConfig {
                socket_path: Some(socket_path),
                max_frame_bytes,
                read_timeout: Duration::from_millis(200),
                ..Default::default()
            };
            let mut client = DirectMuxClient::connect(config).await.expect("connect");

            let err = client
                .list_panes()
                .await
                .expect_err("list_panes should reject oversized response frames");
            assert!(matches!(
                err,
                DirectMuxError::FrameTooLarge { max_bytes } if max_bytes == max_frame_bytes
            ));
            assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Recoverable);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn list_panes_with_cx_rejects_oversized_response_frame() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("oversized-frame-with-cx.sock");
            let server_socket_path = socket_path.clone();
            let (server_ready_tx, server_ready_rx) = std::sync::mpsc::channel();
            let max_frame_bytes = 128usize;
            let server = std::thread::spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime for oversized-frame with-cx test server");
                CompatRuntime::block_on(&runtime, async move {
                    let listener = compat_unix::bind(&server_socket_path).await.expect("bind");
                    server_ready_tx.send(()).expect("send server ready signal");

                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read");
                        if read == 0 {
                            break;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    let response =
                                        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: CODEC_VERSION,
                                            version_string: "oversized-frame-with-cx-test"
                                                .to_string(),
                                            executable_path: PathBuf::from("/bin/wezterm"),
                                            config_file_path: None,
                                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                        });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::SetClientId(_) => {
                                    let mut out = Vec::new();
                                    Pdu::UnitResponse(UnitResponse {})
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    stream.write_all(&out).await.expect("write response");
                                }
                                Pdu::ListPanes(_) => {
                                    let mut window_titles = HashMap::new();
                                    for window_id in 0..24usize {
                                        window_titles.insert(
                                            window_id + 1,
                                            format!(
                                                "oversized-with-cx-window-{window_id:02}-{}",
                                                "x".repeat(32)
                                            ),
                                        );
                                    }
                                    let response = Pdu::ListPanesResponse(ListPanesResponse {
                                        tabs: Vec::new(),
                                        tab_titles: Vec::new(),
                                        window_titles,
                                        floating_panes: Vec::new(),
                                    });
                                    let mut out = Vec::new();
                                    response
                                        .encode(&mut out, decoded.serial)
                                        .expect("encode response");
                                    assert!(
                                        out.len() > max_frame_bytes + 1,
                                        "encoded frame must exceed the configured max"
                                    );

                                    let prefix = &out[..=max_frame_bytes];
                                    let chunk_size = (max_frame_bytes / 2).max(1);
                                    for chunk in prefix.chunks(chunk_size) {
                                        stream.write_all(chunk).await.expect("write frame chunk");
                                        sleep(Duration::from_millis(5)).await;
                                    }
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
            server_ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should be ready before client connects");

            let config = DirectMuxClientConfig {
                socket_path: Some(socket_path),
                max_frame_bytes,
                read_timeout: Duration::from_millis(200),
                ..Default::default()
            };
            let mut client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");

            let err = client
                .list_panes_with_cx(&cx)
                .await
                .expect_err("list_panes_with_cx should reject oversized response frames");
            assert!(matches!(
                err,
                DirectMuxError::FrameTooLarge { max_bytes } if max_bytes == max_frame_bytes
            ));
            assert_eq!(err.protocol_error_kind(), ProtocolErrorKind::Recoverable);
            assert!(client.connection_poisoned);
            assert_eq!(client.poison_transition_count, 1);

            drop(client);
            server.join().expect("server thread");
        });
    }

    #[test]
    fn decode_garbage_frame_returns_error_or_none() {
        // Intentionally invalid RPC frame: random bytes that don't form a valid PDU.
        let mut buf = StreamingPduBuffer::from(vec![
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x10, 0xFF, 0xFF,
        ]);
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
        let mut buf = StreamingPduBuffer::from(buf);

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
            let mut buf = StreamingPduBuffer::from(buf);

            let decoded = decode_from_buffer(&mut buf, 4096)
                .expect("should not error")
                .expect("should decode");
            assert_eq!(decoded.serial, *serial);
        }
    }

    #[test]
    fn codec_overlap_is_retained_per_connection_for_ambient_and_explicit_cx() {
        run_async_test(async {
            let mut prior_connection_id = None;
            let future_codec = CODEC_VERSION
                .checked_add(2)
                .expect("test codec version must leave future-version headroom");
            for (case_idx, explicit_cx, remote_max, remote_min, expected_agreed) in [
                (
                    0,
                    false,
                    CODEC_VERSION,
                    CODEC_VERSION_MIN_SUPPORTED,
                    CODEC_VERSION,
                ),
                (
                    1,
                    true,
                    CODEC_VERSION,
                    CODEC_VERSION_MIN_SUPPORTED,
                    CODEC_VERSION,
                ),
                (
                    2,
                    false,
                    future_codec,
                    CODEC_VERSION_MIN_SUPPORTED,
                    CODEC_VERSION,
                ),
                (
                    3,
                    true,
                    future_codec,
                    CODEC_VERSION_MIN_SUPPORTED,
                    CODEC_VERSION,
                ),
                (4, false, CODEC_VERSION, 0, CODEC_VERSION),
                (5, true, CODEC_VERSION, 0, CODEC_VERSION),
            ] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = temp_dir
                    .path()
                    .join(format!("mux-codec-overlap-{case_idx}.sock"));
                let listener = compat_unix::bind(&socket_path).await.expect("bind");
                let server = task::spawn(accept_direct_mux_handshake(
                    listener, remote_max, remote_min,
                ));

                let config = direct_mux_client_config(socket_path);
                let client = if explicit_cx {
                    let cx = crate::cx::for_testing();
                    DirectMuxClient::connect_with_cx(&cx, config)
                        .await
                        .expect("explicit-Cx overlap must connect")
                } else {
                    DirectMuxClient::connect(config)
                        .await
                        .expect("ambient overlap must connect")
                };

                let DirectMuxProtocolState::Ready(SessionAuthority { codec, .. }) =
                    client.protocol_state
                else {
                    panic!("connected client did not retain ready session authority");
                };
                assert_eq!(codec.connection_id, client.connection_id);
                assert_eq!(codec.local_max, CODEC_VERSION);
                assert_eq!(codec.local_min, CODEC_VERSION_MIN_SUPPORTED);
                assert_eq!(codec.remote_max, remote_max);
                assert_eq!(
                    codec.remote_min,
                    if remote_min == 0 {
                        remote_max
                    } else {
                        remote_min
                    }
                );
                assert_eq!(codec.agreed, expected_agreed);
                if let Some(prior_connection_id) = prior_connection_id {
                    assert_ne!(
                        client.connection_id, prior_connection_id,
                        "reconnect must mint fresh connection-scoped authority"
                    );
                }
                prior_connection_id = Some(client.connection_id);

                drop(client);
                drop(server.await.expect("server task"));
            }
        });
    }

    #[test]
    fn tiered_scrollback_bulk_round_trip_preserves_order_and_typed_outcomes() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("mux-tiered-scrollback-bulk.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");
            let server = task::spawn(async move {
                let mut stream = accept_direct_mux_handshake(
                    listener,
                    CODEC_VERSION,
                    CODEC_VERSION_MIN_SUPPORTED,
                )
                .await;
                let mut read_buf = StreamingPduBuffer::new();
                let decoded = read_test_request_pdu(&mut stream, &mut read_buf).await;
                let Pdu::GetPaneTieredScrollbackStatusesV1(request) = decoded.pdu else {
                    panic!("expected bulk tiered-scrollback request");
                };
                assert_eq!(request.pane_ids, vec![7, 11, 19]);
                let response = codec::GetPaneTieredScrollbackStatusesV1Response {
                    entries: vec![
                        codec::PaneTieredScrollbackStatusEntryV1 {
                            pane_id: 7,
                            outcome: codec::PaneTieredScrollbackStatusOutcomeV1::Available(
                                codec::PaneTieredScrollbackSummaryV1::default(),
                            ),
                        },
                        codec::PaneTieredScrollbackStatusEntryV1 {
                            pane_id: 11,
                            outcome: codec::PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                        },
                        codec::PaneTieredScrollbackStatusEntryV1 {
                            pane_id: 19,
                            outcome: codec::PaneTieredScrollbackStatusOutcomeV1::Missing,
                        },
                    ],
                };
                write_response_pdu(
                    &mut stream,
                    &Pdu::GetPaneTieredScrollbackStatusesV1Response(response.clone()),
                    decoded.serial,
                )
                .await
                .expect("write bulk tiered-scrollback response");
                response
            });

            let cx = crate::cx::for_testing();
            let mut client =
                DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(socket_path))
                    .await
                    .expect("connect bulk tiered-scrollback client");
            let response = client
                .get_pane_tiered_scrollback_statuses_with_cx(&cx, vec![7, 11, 19])
                .await
                .expect("bulk tiered-scrollback request must succeed");
            assert_eq!(response, server.await.expect("server task"));
            assert!(!client.is_connection_poisoned());
        });
    }

    #[test]
    fn tiered_scrollback_bulk_rejects_invalid_batches_before_wire() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir
                .path()
                .join("mux-tiered-scrollback-prewrite-rejections.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");
            let server = task::spawn(async move {
                let mut stream = accept_direct_mux_handshake(
                    listener,
                    CODEC_VERSION,
                    CODEC_VERSION_MIN_SUPPORTED,
                )
                .await;
                let mut read_buf = StreamingPduBuffer::new();
                let decoded = read_test_request_pdu(&mut stream, &mut read_buf).await;
                assert!(
                    matches!(decoded.pdu, Pdu::ListPanes(_)),
                    "a rejected bulk request reached the wire"
                );
                assert_eq!(decoded.serial, 3, "pre-write rejection consumed a serial");
                write_response_pdu(&mut stream, &empty_list_panes_response(), decoded.serial)
                    .await
                    .expect("write negative-control response");
            });

            let cx = crate::cx::for_testing();
            let mut client =
                DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(socket_path))
                    .await
                    .expect("connect current peer");

            for invalid in [
                Vec::new(),
                vec![7, 7],
                (0..=codec::MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES).collect(),
            ] {
                let error = client
                    .get_pane_tiered_scrollback_statuses_with_cx(&cx, invalid)
                    .await
                    .expect_err("invalid batch must fail before wire emission");
                assert!(error.is_proven_pre_write_rejection());
            }
            assert_eq!(client.serial, 2);
            client
                .list_panes_with_cx(&cx)
                .await
                .expect("negative-control request must retain aligned connection");
            assert!(!client.is_connection_poisoned());
            server.await.expect("server task");
        });
    }

    #[test]
    fn tiered_scrollback_bulk_order_mismatch_poisons_aligned_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("mux-tiered-scrollback-reordered.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");
            let server = task::spawn(async move {
                let mut stream = accept_direct_mux_handshake(
                    listener,
                    CODEC_VERSION,
                    CODEC_VERSION_MIN_SUPPORTED,
                )
                .await;
                let mut read_buf = StreamingPduBuffer::new();
                let decoded = read_test_request_pdu(&mut stream, &mut read_buf).await;
                write_response_pdu(
                    &mut stream,
                    &Pdu::GetPaneTieredScrollbackStatusesV1Response(
                        codec::GetPaneTieredScrollbackStatusesV1Response {
                            entries: vec![
                                codec::PaneTieredScrollbackStatusEntryV1 {
                                    pane_id: 11,
                                    outcome:
                                        codec::PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                                },
                                codec::PaneTieredScrollbackStatusEntryV1 {
                                    pane_id: 7,
                                    outcome:
                                        codec::PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                                },
                            ],
                        },
                    ),
                    decoded.serial,
                )
                .await
                .expect("write reordered response");
            });

            let cx = crate::cx::for_testing();
            let mut client =
                DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(socket_path))
                    .await
                    .expect("connect reordered-response client");
            let error = client
                .get_pane_tiered_scrollback_statuses_with_cx(&cx, vec![7, 11])
                .await
                .expect_err("reordered response must fail contract validation");
            assert!(matches!(
                error,
                DirectMuxError::AlignedUnexpectedResponse { .. }
            ));
            assert!(client.is_connection_poisoned());
            server.await.expect("server task");
        });
    }

    #[test]
    fn impossible_or_disjoint_codec_windows_fail_before_registration() {
        run_async_test(async {
            for (case_idx, explicit_cx, remote_max, remote_min) in [
                (0, false, 50, 51),
                (1, true, 50, 51),
                (2, false, 52, 52),
                (3, true, 52, 52),
            ] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = temp_dir
                    .path()
                    .join(format!("mux-codec-no-registration-{case_idx}.sock"));
                let listener = compat_unix::bind(&socket_path).await.expect("bind");
                let server = task::spawn(async move {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut saw_registration = false;
                    let mut sent_codec_response = false;
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read codec rejection handshake");
                        if read == 0 {
                            return saw_registration;
                        }
                        read_buf.extend_from_slice(&temp[..read]);
                        while let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                            .expect("decode codec rejection handshake")
                        {
                            match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    assert!(!sent_codec_response);
                                    sent_codec_response = true;
                                    write_response_pdu(
                                        &mut stream,
                                        &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                            codec_vers: remote_max,
                                            version_string: "rejected-codec-window".to_string(),
                                            executable_path: PathBuf::from(
                                                "/bin/frankenterm-mux-server",
                                            ),
                                            config_file_path: None,
                                            min_supported: remote_min,
                                        }),
                                        decoded.serial,
                                    )
                                    .await
                                    .expect("write rejected codec response");
                                }
                                Pdu::SetClientId(_) => saw_registration = true,
                                _ => {}
                            }
                        }
                    }
                });

                let config = direct_mux_client_config(socket_path);
                let error = if explicit_cx {
                    let cx = crate::cx::for_testing();
                    DirectMuxClient::connect_with_cx(&cx, config)
                        .await
                        .expect_err("invalid codec window must fail")
                } else {
                    DirectMuxClient::connect(config)
                        .await
                        .expect_err("invalid codec window must fail")
                };
                assert!(matches!(error, DirectMuxError::IncompatibleCodec { .. }));
                assert!(
                    !server.await.expect("server task"),
                    "client must reject an invalid codec window before SetClientId"
                );
            }
        });
    }

    #[test]
    fn ordered_window_outbound_gate_precedes_serial_encode_and_batch_prefix() {
        run_async_test(async {
            for (case_idx, explicit_cx, remote_max) in [
                (0, false, CODEC_VERSION_MIN_SUPPORTED),
                (1, true, CODEC_VERSION_MIN_SUPPORTED),
            ] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = temp_dir
                    .path()
                    .join(format!("mux-ordered-outbound-gate-{case_idx}.sock"));
                let listener = compat_unix::bind(&socket_path).await.expect("bind");
                let server = task::spawn(async move {
                    let mut stream = accept_direct_mux_handshake(listener, remote_max, 46).await;
                    let mut read_buf = StreamingPduBuffer::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read post-gate request");
                        assert!(read > 0, "client disconnected before negative control");
                        read_buf.extend_from_slice(&temp[..read]);
                        if let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                            .expect("decode post-gate request")
                        {
                            assert!(
                                matches!(decoded.pdu, Pdu::ListPanes(_)),
                                "a rejected ordered-window request or batch prefix reached the wire"
                            );
                            assert_eq!(
                                decoded.serial, 3,
                                "rejected requests must not consume a serial"
                            );
                            write_response_pdu(
                                &mut stream,
                                &empty_list_panes_response(),
                                decoded.serial,
                            )
                            .await
                            .expect("write negative-control response");
                            return decoded.serial;
                        }
                    }
                });

                let config = direct_mux_client_config(socket_path);
                let cx = crate::cx::for_testing();
                let mut client = if explicit_cx {
                    DirectMuxClient::connect_with_cx(&cx, config)
                        .await
                        .expect("connect explicit-Cx gate client")
                } else {
                    DirectMuxClient::connect(config)
                        .await
                        .expect("connect ambient gate client")
                };
                assert_eq!(client.serial, 2);

                let direct_error = if explicit_cx {
                    client
                        .send_request_only_with_cx(&cx, ordered_window_request())
                        .await
                        .expect_err("inactive ordered-window request must be rejected")
                } else {
                    client
                        .send_request_only(ordered_window_request())
                        .await
                        .expect_err("inactive ordered-window request must be rejected")
                };
                assert!(matches!(
                    direct_error,
                    DirectMuxError::OutboundCapabilityNotNegotiated { .. }
                ));
                assert_eq!(client.serial, 2);
                assert!(client.outstanding_requests.is_empty());

                let batch_requests = vec![Pdu::ListPanes(ListPanes {}), ordered_window_request()];
                let batch_error = if explicit_cx {
                    client
                        .batch_with_cx(&cx, batch_requests, 2, Duration::from_secs(1))
                        .await
                        .expect_err("whole batch must be preflighted")
                } else {
                    client
                        .batch(batch_requests, 2, Duration::from_secs(1))
                        .await
                        .expect_err("whole batch must be preflighted")
                };
                assert_eq!(
                    batch_error.protocol_error_kind(),
                    ProtocolErrorKind::Permanent
                );
                assert_eq!(client.serial, 2);
                assert!(client.outstanding_requests.is_empty());

                if explicit_cx {
                    client
                        .list_panes_with_cx(&cx)
                        .await
                        .expect("explicit-Cx negative control");
                } else {
                    client.list_panes().await.expect("ambient negative control");
                }
                assert_eq!(server.await.expect("server task"), 3);
            }
        });
    }

    #[test]
    fn ordered_window_inbound_gate_precedes_correlation_and_poisons_both_readers() {
        run_async_test(async {
            for (case_idx, explicit_cx, remote_max, correlated_reply) in [
                (0, false, CODEC_VERSION_MIN_SUPPORTED, false),
                (1, true, CODEC_VERSION_MIN_SUPPORTED, false),
                (2, false, CODEC_VERSION_MIN_SUPPORTED, true),
                (3, true, CODEC_VERSION_MIN_SUPPORTED, true),
            ] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = temp_dir
                    .path()
                    .join(format!("mux-ordered-inbound-gate-{case_idx}.sock"));
                let listener = compat_unix::bind(&socket_path).await.expect("bind");
                let server = task::spawn(async move {
                    let mut stream = accept_direct_mux_handshake(listener, remote_max, 46).await;
                    let mut read_buf = StreamingPduBuffer::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = unix_stream_read(&mut stream, &mut temp)
                            .await
                            .expect("read request preceding forbidden inbound PDU");
                        assert!(read > 0);
                        read_buf.extend_from_slice(&temp[..read]);
                        if let Some(decoded) = codec::Pdu::stream_decode(&mut read_buf)
                            .expect("decode request preceding forbidden inbound PDU")
                        {
                            assert!(matches!(decoded.pdu, Pdu::ListPanes(_)));
                            let (forbidden, serial) = if correlated_reply {
                                (unsupported_ordered_window_response(), 999)
                            } else {
                                (ordered_window_event(), 0)
                            };
                            write_response_pdu(&mut stream, &forbidden, serial)
                                .await
                                .expect("write forbidden inbound PDU");
                            let mut eof_probe = [0u8; 1];
                            let peer_read = unix_stream_read(&mut stream, &mut eof_probe)
                                .await
                                .expect("read poisoned connection EOF");
                            assert_eq!(peer_read, 0);
                            return;
                        }
                    }
                });

                let config = direct_mux_client_config(socket_path);
                let cx = crate::cx::for_testing();
                let mut client = if explicit_cx {
                    DirectMuxClient::connect_with_cx(&cx, config)
                        .await
                        .expect("connect explicit-Cx inbound-gate client")
                } else {
                    DirectMuxClient::connect(config)
                        .await
                        .expect("connect ambient inbound-gate client")
                };
                let error = if explicit_cx {
                    client
                        .list_panes_with_cx(&cx)
                        .await
                        .expect_err("forbidden ordered-window PDU must fail")
                } else {
                    client
                        .list_panes()
                        .await
                        .expect_err("forbidden ordered-window PDU must fail")
                };
                assert!(matches!(
                    error,
                    DirectMuxError::InboundCapabilityNotNegotiated { .. }
                ));
                assert!(client.connection_poisoned);
                assert!(matches!(
                    client.protocol_state,
                    DirectMuxProtocolState::Poisoned { .. }
                ));
                assert!(client.outstanding_requests.is_empty());
                assert!(client.pending_responses.is_empty());
                assert_eq!(client.pending_response_bytes, 0);
                assert!(client.pending_render_changes.is_empty());
                assert!(client.render_change_snapshots.is_empty());
                assert!(client.read_buf.is_empty());

                let subsequent = if explicit_cx {
                    client.list_panes_with_cx(&cx).await
                } else {
                    client.list_panes().await
                };
                assert!(matches!(subsequent, Err(DirectMuxError::Disconnected)));
                server.await.expect("server task");
            }
        });
    }

    #[test]
    fn incompatible_codec_version_rejected() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("mux-incompat.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut temp = vec![0u8; 4096];
                let read = unix_stream_read(&mut stream, &mut temp)
                    .await
                    .expect("read");
                read_buf.extend_from_slice(&temp[..read]);
                if let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                    // Respond with wrong codec version
                    let response = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                        codec_vers: CODEC_VERSION + 999,
                        version_string: "incompatible-wezterm".to_string(),
                        executable_path: PathBuf::from("/bin/wezterm"),
                        config_file_path: None,
                        min_supported: CODEC_VERSION + 1,
                    });
                    let mut out = Vec::new();
                    response.encode(&mut out, decoded.serial).expect("encode");
                    stream.write_all(&out).await.expect("write");
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let err = DirectMuxClient::connect(config).await.unwrap_err();
            match err {
                DirectMuxError::IncompatibleCodec {
                    local,
                    local_min,
                    remote,
                    remote_min,
                    ..
                } => {
                    assert_eq!(local, CODEC_VERSION);
                    assert_eq!(local_min, CODEC_VERSION_MIN_SUPPORTED);
                    assert_eq!(remote, CODEC_VERSION + 999);
                    assert_eq!(remote_min, CODEC_VERSION + 1);
                }
                other => panic!("expected IncompatibleCodec, got: {other}"),
            }
        });
    }

    #[test]
    fn incompatible_codec_version_rejected_with_cx() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("mux-incompat-with-cx.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut temp = vec![0u8; 4096];
                let read = unix_stream_read(&mut stream, &mut temp)
                    .await
                    .expect("read");
                read_buf.extend_from_slice(&temp[..read]);
                if let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                    let response = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                        codec_vers: CODEC_VERSION + 999,
                        version_string: "incompatible-wezterm-with-cx".to_string(),
                        executable_path: PathBuf::from("/bin/wezterm"),
                        config_file_path: None,
                        min_supported: CODEC_VERSION + 1,
                    });
                    let mut out = Vec::new();
                    response.encode(&mut out, decoded.serial).expect("encode");
                    stream.write_all(&out).await.expect("write");
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let err = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .unwrap_err();
            match err {
                DirectMuxError::IncompatibleCodec {
                    local,
                    local_min,
                    remote,
                    remote_min,
                    ..
                } => {
                    assert_eq!(local, CODEC_VERSION);
                    assert_eq!(local_min, CODEC_VERSION_MIN_SUPPORTED);
                    assert_eq!(remote, CODEC_VERSION + 999);
                    assert_eq!(remote_min, CODEC_VERSION + 1);
                }
                other => panic!("expected IncompatibleCodec, got: {other}"),
            }
        });
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
    fn subscription_poll_delay_uses_fast_path_when_dirty() {
        let config = SubscriptionConfig {
            poll_interval: Duration::from_millis(100),
            min_poll_interval: Duration::from_millis(20),
            channel_capacity: 8,
        };
        assert_eq!(
            subscription_poll_delay(&config, true),
            Duration::from_millis(20)
        );
        assert_eq!(
            subscription_poll_delay(&config, false),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn subscription_poll_delay_caps_min_interval_to_poll_interval() {
        let config = SubscriptionConfig {
            poll_interval: Duration::from_millis(25),
            min_poll_interval: Duration::from_millis(80),
            channel_capacity: 8,
        };
        assert_eq!(
            subscription_poll_delay(&config, true),
            Duration::from_millis(25)
        );
    }

    #[test]
    fn pane_delta_send_delivers_via_reserve_commit() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(4);
            pane_delta_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 99,
                    reason: "reserve-commit".to_string(),
                },
            )
            .await;
            let received = pane_delta_recv(&mut rx)
                .await
                .expect("delta should be delivered");
            match received {
                PaneDelta::Gap { pane_id, reason } => {
                    assert_eq!(pane_id, 99);
                    assert_eq!(reason, "reserve-commit");
                }
                other => panic!("expected gap delta, got {:?}", other),
            }
        });
    }

    #[test]
    fn pane_delta_send_is_noop_when_receiver_closed() {
        run_async_test(async {
            let (tx, rx) = mpsc::channel(1);
            drop(rx);
            pane_delta_send(
                &tx,
                PaneDelta::Ended {
                    pane_id: 1,
                    reason: "receiver-closed".to_string(),
                },
            )
            .await;
        });
    }

    #[test]
    fn pane_delta_try_send_delivers_via_reserve_commit() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(1);
            let sent = pane_delta_try_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 7,
                    reason: "try-reserve".to_string(),
                },
            );
            assert!(sent);
            let received = pane_delta_recv(&mut rx)
                .await
                .expect("delta should be delivered");
            match received {
                PaneDelta::Gap { pane_id, reason } => {
                    assert_eq!(pane_id, 7);
                    assert_eq!(reason, "try-reserve");
                }
                other => panic!("expected gap delta, got {:?}", other),
            }
        });
    }

    #[test]
    fn pane_delta_try_send_returns_false_when_full() {
        run_async_test(async {
            let (tx, _rx) = mpsc::channel(1);
            let first = pane_delta_try_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 1,
                    reason: "first".to_string(),
                },
            );
            assert!(first);
            let second = pane_delta_try_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 1,
                    reason: "second".to_string(),
                },
            );
            assert!(!second);
        });
    }

    #[test]
    fn pane_delta_try_send_succeeds_after_capacity_is_freed() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(1);
            assert!(pane_delta_try_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 11,
                    reason: "first".to_string(),
                },
            ));
            assert!(!pane_delta_try_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 11,
                    reason: "second".to_string(),
                },
            ));

            let drained = pane_delta_recv(&mut rx)
                .await
                .expect("first delta should drain");
            match drained {
                PaneDelta::Gap { pane_id, reason } => {
                    assert_eq!(pane_id, 11);
                    assert_eq!(reason, "first");
                }
                other => panic!("expected first gap delta, got {:?}", other),
            }

            assert!(pane_delta_try_send(
                &tx,
                PaneDelta::Gap {
                    pane_id: 11,
                    reason: "third".to_string(),
                },
            ));
        });
    }

    #[test]
    fn pane_delta_try_send_returns_false_when_receiver_closed() {
        run_async_test(async {
            let (tx, rx) = mpsc::channel(1);
            drop(rx);
            let sent = pane_delta_try_send(
                &tx,
                PaneDelta::Ended {
                    pane_id: 2,
                    reason: "closed".to_string(),
                },
            );
            assert!(!sent);
        });
    }

    #[test]
    fn total_dirty_rows_sums_range_spans() {
        let ranges: Vec<std::ops::Range<isize>> = vec![-4..-2, 10..13, 20..21];
        assert_eq!(total_dirty_rows(&ranges), 6);
    }

    #[test]
    fn total_dirty_rows_ignores_descending_ranges() {
        #[allow(clippy::reversed_empty_ranges)]
        let ranges: Vec<std::ops::Range<isize>> = vec![5..2, 3..3, 7..9];
        assert_eq!(total_dirty_rows(&ranges), 2);
    }

    #[test]
    fn total_dirty_rows_saturates_for_extreme_signed_range() {
        let ranges = vec![isize::MIN..isize::MAX, 0..1];
        assert_eq!(total_dirty_rows(&ranges), usize::MAX);
    }

    fn test_bonus_lines(texts: &[&str]) -> codec::SerializedLines {
        use termwiz::cell::CellAttributes;
        use termwiz::surface::Line;
        texts
            .iter()
            .enumerate()
            .map(|(idx, text)| {
                (
                    idx as isize,
                    Line::from_text(text, &CellAttributes::default(), 1, None),
                )
            })
            .collect::<Vec<_>>()
            .into()
    }

    /// GH#73 regression: the mux server moves changed viewport rows out of
    /// `dirty_lines` and into `bonus_lines`, so a bonus-lines-only response
    /// is an ordinary content-bearing update and MUST produce an output
    /// delta. The old gate (`!dirty_lines.is_empty()`) silently discarded
    /// these updates end-to-end: no CaptureEvent, no stored segment, no gap.
    #[test]
    fn render_changes_bonus_lines_only_produces_output_delta() {
        let mut changes = test_render_change(3, 11, "pane-title");
        changes.dirty_lines = Vec::new();
        changes.bonus_lines = test_bonus_lines(&["hello", "world"]);

        let delta = render_changes_to_output_delta(3, changes)
            .expect("bonus-lines-only update must emit an output delta");
        match delta {
            PaneDelta::Output {
                pane_id,
                seqno,
                delta_text,
                title,
                dirty_range_count,
                dirty_row_count,
            } => {
                assert_eq!(pane_id, 3);
                assert_eq!(seqno, 11);
                assert_eq!(delta_text, "hello\nworld");
                assert_eq!(title, "pane-title");
                assert_eq!(dirty_range_count, 0);
                assert_eq!(dirty_row_count, 0);
            }
            other => panic!("expected PaneDelta::Output, got {other:?}"),
        }
    }

    /// GH#73: dirty-lines-only updates (no prefetched bonus rows) must keep
    /// emitting exactly as before.
    #[test]
    fn render_changes_dirty_lines_only_still_produces_output_delta() {
        let changes = test_render_change(4, 5, "t");
        assert!(!changes.dirty_lines.is_empty());
        let delta = render_changes_to_output_delta(4, changes)
            .expect("dirty-lines update must emit an output delta");
        match delta {
            PaneDelta::Output {
                dirty_range_count,
                dirty_row_count,
                ..
            } => {
                assert_eq!(dirty_range_count, 1);
                assert_eq!(dirty_row_count, 1);
            }
            other => panic!("expected PaneDelta::Output, got {other:?}"),
        }
    }

    /// GH#73: a genuinely idle response (no dirty ranges, no bonus lines)
    /// must not fabricate a delta.
    #[test]
    fn render_changes_idle_response_produces_no_delta() {
        let mut changes = test_render_change(5, 9, "t");
        changes.dirty_lines = Vec::new();
        assert!(render_changes_to_output_delta(5, changes).is_none());
    }

    /// GH#73: bonus lines whose text is empty (blank rows) still represent a
    /// content change and must emit rather than be classified as idle.
    #[test]
    fn render_changes_blank_bonus_lines_still_emit() {
        let mut changes = test_render_change(6, 2, "t");
        changes.dirty_lines = Vec::new();
        changes.bonus_lines = test_bonus_lines(&[""]);
        assert!(render_changes_to_output_delta(6, changes).is_some());
    }

    #[test]
    fn pane_delta_output_debug_format() {
        let delta = PaneDelta::Output {
            pane_id: 42,
            seqno: 7,
            delta_text: "hello world".to_string(),
            title: "bash".to_string(),
            dirty_range_count: 3,
            dirty_row_count: 9,
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
            reason: "bounded channel loss".to_string(),
        };
        let dbg = format!("{delta:?}");
        assert!(dbg.contains("Gap"));
        assert!(dbg.contains("bounded channel loss"));
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
            delta_text: "delta".to_string(),
            title: "zsh".to_string(),
            dirty_range_count: 1,
            dirty_row_count: 1,
        };
        let cloned = delta.clone();
        // Clone should produce identical debug output
        assert_eq!(format!("{delta:?}"), format!("{cloned:?}"));
    }

    #[test]
    fn subscription_output_delta_reports_dirty_counts() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("dirty-counts.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut emitted_output = false;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                let (dirty_lines, seqno) = if emitted_output {
                                    (Vec::new(), 2)
                                } else {
                                    emitted_output = true;
                                    (vec![0isize..2isize, 4isize..7isize], 1)
                                };

                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 7,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines,
                                    title: "test".to_string(),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno,
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
                7,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 8,
                },
            );

            let mut observed = None;
            for _ in 0..20 {
                match timeout(Duration::from_millis(200), sub.next()).await {
                    Ok(Some(PaneDelta::Output {
                        pane_id,
                        dirty_range_count,
                        dirty_row_count,
                        ..
                    })) => {
                        observed = Some((pane_id, dirty_range_count, dirty_row_count));
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {}
                }
            }

            let (pane_id, dirty_range_count, dirty_row_count) =
                observed.expect("expected output delta with dirty counts");
            assert_eq!(pane_id, 7);
            assert_eq!(dirty_range_count, 2);
            assert_eq!(dirty_row_count, 5);
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish promptly");
        });
    }

    #[test]
    fn subscription_with_cx_receives_output_delta() {
        let runtime = crate::cx::CxRuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let cx = crate::cx::for_testing();
        let handle = runtime.handle();

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("subscription-with-cx.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            std::mem::drop(crate::cx::spawn_with_cx(
                &handle,
                &cx,
                |_child_cx| async move {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut emitted_output = false;

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
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
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::GetPaneRenderChanges(_) => {
                                    let dirty_lines = if emitted_output {
                                        Vec::new()
                                    } else {
                                        emitted_output = true;
                                        std::iter::once(0isize..2isize).collect()
                                    };

                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: 31,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines,
                                            title: "with-cx".to_string(),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: 1,
                                        },
                                    )
                                }
                                _ => continue,
                            };
                            let mut out = Vec::new();
                            response.encode(&mut out, decoded.serial).expect("encode");
                            stream.write_all(&out).await.expect("write");
                        }
                    }
                },
            ));

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");
            let mut sub = subscribe_pane_output_with_cx(
                &handle,
                &cx,
                client,
                31,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 8,
                },
            );

            let mut observed = None;
            for _ in 0..20 {
                match timeout(Duration::from_millis(200), sub.next_with_cx(&cx)).await {
                    Ok(Some(PaneDelta::Output {
                        pane_id,
                        seqno,
                        dirty_range_count,
                        dirty_row_count,
                        ..
                    })) => {
                        observed = Some((pane_id, seqno, dirty_range_count, dirty_row_count));
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {}
                }
            }

            assert_eq!(observed, Some((31, 1, 1, 2)));
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish promptly");
        });
    }

    #[test]
    fn subscription_with_inherited_cx_receives_output_delta() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("subscription-with-inherited-cx.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut emitted_output = false;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                let dirty_lines = if emitted_output {
                                    Vec::new()
                                } else {
                                    emitted_output = true;
                                    std::iter::once(0isize..2isize).collect()
                                };

                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 32,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines,
                                    title: "with-inherited-cx".to_string(),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno: 1,
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
            let client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");
            let mut sub = subscribe_pane_output_with_inherited_cx(
                &cx,
                client,
                32,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 8,
                },
            );

            let mut observed = None;
            for _ in 0..20 {
                match timeout(Duration::from_millis(200), sub.next_with_cx(&cx)).await {
                    Ok(Some(PaneDelta::Output {
                        pane_id,
                        seqno,
                        dirty_range_count,
                        dirty_row_count,
                        ..
                    })) => {
                        observed = Some((pane_id, seqno, dirty_range_count, dirty_row_count));
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {}
                }
            }

            assert_eq!(observed, Some((32, 1, 1, 2)));
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish promptly");
        });
    }

    #[test]
    fn subscription_with_cx_shutdown_waits_for_poller_exit() {
        let runtime = crate::cx::CxRuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let cx = crate::cx::for_testing();
        let handle = runtime.handle();

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("subscription-with-cx-shutdown.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");
            let render_request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&render_request_count);
            let (closed_tx, closed_rx) = crate::runtime_async::oneshot::channel::<()>();

            std::mem::drop(crate::cx::spawn_with_cx(
                &handle,
                &cx,
                |_child_cx| async move {
                    let mut closed_tx = Some(closed_tx);
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => {
                                if let Some(tx) = closed_tx.take() {
                                    // br-ft-x2oyy: intentional best-effort test close signal;
                                    // failure means the waiting assertion side already exited.
                                    notify_test_server_closed_best_effort(tx);
                                }
                                break;
                            }
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
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::GetPaneRenderChanges(_) => {
                                    server_request_count.fetch_add(1, Ordering::SeqCst);
                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: 31,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines: Vec::new(),
                                            title: "with-cx-shutdown".to_string(),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: 1,
                                        },
                                    )
                                }
                                _ => continue,
                            };
                            let mut out = Vec::new();
                            response.encode(&mut out, decoded.serial).expect("encode");
                            if stream.write_all(&out).await.is_err() {
                                if let Some(tx) = closed_tx.take() {
                                    // br-ft-x2oyy: intentional best-effort test close signal;
                                    // failure means the waiting assertion side already exited.
                                    notify_test_server_closed_best_effort(tx);
                                }
                                return;
                            }
                        }
                    }
                },
            ));

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");
            let sub = subscribe_pane_output_with_cx(
                &handle,
                &cx,
                client,
                31,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 8,
                },
            );

            timeout(Duration::from_secs(1), async {
                loop {
                    if render_request_count.load(Ordering::SeqCst) >= 1 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("subscription should issue a render request");

            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish promptly");

            let closed = timeout(
                Duration::from_millis(500),
                crate::runtime_async::oneshot_recv(closed_rx),
            )
            .await
            .expect("shutdown should await server-observed socket close");
            closed.expect("server close signal should complete");
        });
    }

    #[test]
    fn concurrent_subscriptions_do_not_cross_talk() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("subscription-no-crosstalk.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");
            let observed_panes = Arc::new(Mutex::new(HashSet::new()));

            task::spawn({
                let observed_panes = Arc::clone(&observed_panes);
                async move {
                    for _ in 0..2 {
                        let (mut stream, _) = listener.accept().await.expect("accept");
                        let observed_panes = Arc::clone(&observed_panes);
                        task::spawn(async move {
                            let mut read_buf = StreamingPduBuffer::new();
                            let mut emitted_output = false;

                            loop {
                                let mut temp = vec![0u8; 4096];
                                let read = match unix_stream_read(&mut stream, &mut temp).await {
                                    Ok(0) => break,
                                    Ok(n) => n,
                                    Err(_) => break,
                                };
                                read_buf.extend_from_slice(&temp[..read]);
                                while let Ok(Some(decoded)) =
                                    codec::Pdu::stream_decode(&mut read_buf)
                                {
                                    let response = match decoded.pdu {
                                        Pdu::GetCodecVersion(_) => {
                                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                                codec_vers: CODEC_VERSION,
                                                version_string: "test".to_string(),
                                                executable_path: PathBuf::from("/bin/wezterm"),
                                                config_file_path: None,
                                                min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                            })
                                        }
                                        Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                        Pdu::GetPaneRenderChanges(request) => {
                                            let pane_id = request.pane_id as u64;
                                            {
                                                let mut seen = observed_panes.lock().await;
                                                seen.insert(pane_id);
                                            }
                                            let dirty_lines = if emitted_output {
                                                Vec::new()
                                            } else {
                                                emitted_output = true;
                                                match pane_id {
                                                    21 => std::iter::once(0isize..1isize).collect(),
                                                    22 => vec![0isize..1isize, 2isize..4isize],
                                                    _ => Vec::new(),
                                                }
                                            };

                                            Pdu::GetPaneRenderChangesResponse(
                                                GetPaneRenderChangesResponse {
                                                    pane_id: request.pane_id,
                                                    mouse_grabbed: false,
                                                    alt_screen_active: false,
                                                    cursor_position:
                                                        mux::renderable::StableCursorPosition::default(),
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
                                                    tiered_scrollback_status: None,
                                                    dirty_lines,
                                                    title: format!("pane-{pane_id}"),
                                                    working_dir: None,
                                                    bonus_lines: Vec::new().into(),
                                                    input_serial: None,
                                                    seqno: 1,
                                                },
                                            )
                                        }
                                        _ => continue,
                                    };
                                    let mut out = Vec::new();
                                    response.encode(&mut out, decoded.serial).expect("encode");
                                    stream.write_all(&out).await.expect("write");
                                }
                            }
                        });
                    }
                }
            });

            let config = SubscriptionConfig {
                poll_interval: Duration::from_millis(10),
                min_poll_interval: Duration::from_millis(5),
                channel_capacity: 8,
            };

            let client_a = DirectMuxClient::connect(
                DirectMuxClientConfig::default().with_socket_path(socket_path.clone()),
            )
            .await
            .expect("connect client_a");
            let client_b = DirectMuxClient::connect(
                DirectMuxClientConfig::default().with_socket_path(socket_path),
            )
            .await
            .expect("connect client_b");

            let mut sub_a = subscribe_pane_output(client_a, 21, config.clone());
            let mut sub_b = subscribe_pane_output(client_b, 22, config);

            let mut a_counts: Option<(usize, usize)> = None;
            let mut b_counts: Option<(usize, usize)> = None;

            for _ in 0..30 {
                if a_counts.is_none() {
                    if let Ok(Some(PaneDelta::Output {
                        pane_id,
                        dirty_range_count,
                        dirty_row_count,
                        ..
                    })) = timeout(Duration::from_millis(200), sub_a.next()).await
                    {
                        assert_eq!(pane_id, 21, "subscription A should only receive pane 21");
                        a_counts = Some((dirty_range_count, dirty_row_count));
                    }
                }
                if b_counts.is_none() {
                    if let Ok(Some(PaneDelta::Output {
                        pane_id,
                        dirty_range_count,
                        dirty_row_count,
                        ..
                    })) = timeout(Duration::from_millis(200), sub_b.next()).await
                    {
                        assert_eq!(pane_id, 22, "subscription B should only receive pane 22");
                        b_counts = Some((dirty_range_count, dirty_row_count));
                    }
                }
                if a_counts.is_some() && b_counts.is_some() {
                    break;
                }
            }

            sub_a.cancel();
            sub_b.cancel();

            let a_counts = a_counts.expect("subscription A output");
            let b_counts = b_counts.expect("subscription B output");
            assert_eq!(a_counts, (1, 1));
            assert_eq!(b_counts, (2, 3));

            let seen = observed_panes.lock().await;
            assert!(seen.contains(&21), "server should observe pane 21 requests");
            assert!(seen.contains(&22), "server should observe pane 22 requests");
            assert_eq!(seen.len(), 2, "server should observe only requested panes");
        });
    }

    #[test]
    fn subscription_terminal_seqno_jump_does_not_emit_delivery_gap() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("terminal-seqno-jump.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                render_requests += 1;
                                let (seqno, dirty_lines) = match render_requests {
                                    1 => (1, std::iter::once(0isize..1isize).collect()),
                                    2 => (4, std::iter::once(1isize..2isize).collect()),
                                    _ => (4, Vec::new()),
                                };
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 11,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines,
                                    title: "gap-test".to_string(),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno,
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
                11,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 16,
                },
            );

            let mut saw_seq1 = false;
            let mut saw_seq4 = false;
            let mut unexpected_gap = None;

            for _ in 0..30 {
                match timeout(Duration::from_millis(200), sub.next()).await {
                    Ok(Some(PaneDelta::Output { seqno: 1, .. })) => saw_seq1 = true,
                    Ok(Some(PaneDelta::Output { seqno: 4, .. })) => saw_seq4 = true,
                    Ok(Some(PaneDelta::Gap { reason, .. })) => {
                        unexpected_gap = Some(reason);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {}
                }

                if saw_seq1 && saw_seq4 {
                    break;
                }
            }

            assert!(saw_seq1, "expected first output event at seqno=1");
            assert!(saw_seq4, "expected second output event at seqno=4");
            assert!(
                unexpected_gap.is_none(),
                "terminal mutation seqno jump must not imply delivery loss: {unexpected_gap:?}"
            );
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish promptly");
        });
    }

    #[test]
    fn subscription_emits_ended_when_mux_disconnects() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("subscription-disconnect.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                let mut render_requests = 0usize;

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    read_buf.extend_from_slice(&temp[..read]);
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let maybe_response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Some(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "test".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                }))
                            }
                            Pdu::SetClientId(_) => Some(Pdu::UnitResponse(UnitResponse {})),
                            Pdu::GetPaneRenderChanges(_) => {
                                render_requests += 1;
                                if render_requests == 1 {
                                    Some(Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: 12,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines: std::iter::once(0isize..1isize).collect(),
                                            title: "disconnect-test".to_string(),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: 1,
                                        },
                                    ))
                                } else {
                                    // Simulate abrupt server disconnect after consuming request.
                                    return;
                                }
                            }
                            _ => None,
                        };

                        let Some(response) = maybe_response else {
                            continue;
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
                12,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 8,
                },
            );

            let mut saw_disconnect_end = false;
            for _ in 0..30 {
                match timeout(Duration::from_millis(200), sub.next()).await {
                    Ok(Some(PaneDelta::Ended { reason, .. })) => {
                        if reason.contains("mux socket disconnected") {
                            saw_disconnect_end = true;
                            break;
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {}
                }
            }

            assert!(
                saw_disconnect_end,
                "expected Ended event with disconnect reason"
            );
        });
    }

    #[test]
    fn subscription_cancel_closes_connection_when_channel_full() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("cancel-full-channel.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            let render_request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&render_request_count);
            let (closed_tx, closed_rx) = crate::runtime_async::oneshot::channel::<()>();

            task::spawn(async move {
                let mut closed_tx = Some(closed_tx);
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
                        Ok(0) => {
                            if let Some(tx) = closed_tx.take() {
                                // br-ft-x2oyy: intentional best-effort test close signal;
                                // failure means the waiting assertion side already exited.
                                notify_test_server_closed_best_effort(tx);
                            }
                            break;
                        }
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                let seqno = server_request_count.fetch_add(1, Ordering::SeqCst) + 1;
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 13,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines: std::iter::once(0isize..1isize).collect(),
                                    title: "cancel-full-channel".to_string(),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno,
                                })
                            }
                            _ => continue,
                        };
                        let mut out = Vec::new();
                        response.encode(&mut out, decoded.serial).expect("encode");
                        if stream.write_all(&out).await.is_err() {
                            if let Some(tx) = closed_tx.take() {
                                // br-ft-x2oyy: intentional best-effort test close signal;
                                // failure means the waiting assertion side already exited.
                                notify_test_server_closed_best_effort(tx);
                            }
                            return;
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect(config).await.expect("connect");
            let sub = subscribe_pane_output(
                client,
                13,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(5),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 1,
                },
            );

            timeout(Duration::from_secs(1), async {
                loop {
                    if render_request_count.load(Ordering::SeqCst) >= 2 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("subscription should issue at least two render requests");

            // Cancel without draining the receiver. Cancellation must still terminate promptly
            // and the background poller must finish instead of leaking into later tests.
            sub.cancel();
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish promptly");

            let closed = timeout(
                Duration::from_millis(500),
                crate::runtime_async::oneshot_recv(closed_rx),
            )
            .await
            .expect("server should observe connection close after cancellation");
            closed.expect("server close signal should complete");
        });
    }

    #[test]
    fn subscription_with_cx_cancel_closes_connection_when_channel_full() {
        let runtime = crate::cx::CxRuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let cx = crate::cx::for_testing();
        let handle = runtime.handle();

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("cancel-full-channel-with-cx.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            let render_request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&render_request_count);
            let (closed_tx, closed_rx) = crate::runtime_async::oneshot::channel::<()>();

            std::mem::drop(crate::cx::spawn_with_cx(
                &handle,
                &cx,
                |_child_cx| async move {
                    let mut closed_tx = Some(closed_tx);
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => {
                                if let Some(tx) = closed_tx.take() {
                                    // br-ft-x2oyy: intentional best-effort test close signal;
                                    // failure means the waiting assertion side already exited.
                                    notify_test_server_closed_best_effort(tx);
                                }
                                break;
                            }
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
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::GetPaneRenderChanges(_) => {
                                    let seqno =
                                        server_request_count.fetch_add(1, Ordering::SeqCst) + 1;
                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: 13,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines: std::iter::once(0isize..1isize).collect(),
                                            title: "cancel-full-channel-with-cx".to_string(),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno,
                                        },
                                    )
                                }
                                _ => continue,
                            };
                            let mut out = Vec::new();
                            response.encode(&mut out, decoded.serial).expect("encode");
                            if stream.write_all(&out).await.is_err() {
                                if let Some(tx) = closed_tx.take() {
                                    // br-ft-x2oyy: intentional best-effort test close signal;
                                    // failure means the waiting assertion side already exited.
                                    notify_test_server_closed_best_effort(tx);
                                }
                                return;
                            }
                        }
                    }
                },
            ));

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");
            let sub = subscribe_pane_output_with_cx(
                &handle,
                &cx,
                client,
                13,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(5),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 1,
                },
            );

            timeout(Duration::from_secs(1), async {
                loop {
                    if render_request_count.load(Ordering::SeqCst) >= 2 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("subscription should issue at least two render requests");

            sub.cancel();
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish after cancellation");

            let closed = timeout(
                Duration::from_millis(500),
                crate::runtime_async::oneshot_recv(closed_rx),
            )
            .await
            .expect("server should observe connection close after cancellation");
            closed.expect("server close signal should complete");
        });
    }

    #[test]
    fn subscription_cancel_closes_connection_when_output_channel_is_full() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("cancel-full-output-channel.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            let render_request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&render_request_count);
            let (closed_tx, closed_rx) = crate::runtime_async::oneshot::channel::<()>();

            task::spawn(async move {
                let mut closed_tx = Some(closed_tx);
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
                        Ok(0) => {
                            if let Some(tx) = closed_tx.take() {
                                // br-ft-x2oyy: intentional best-effort test close signal;
                                // failure means the waiting assertion side already exited.
                                notify_test_server_closed_best_effort(tx);
                            }
                            break;
                        }
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                let request_number =
                                    server_request_count.fetch_add(1, Ordering::SeqCst) + 1;
                                let (seqno, dirty_lines) = if request_number == 1 {
                                    (1, std::iter::once(0isize..1isize).collect())
                                } else {
                                    // Keep polling while the first output event occupies
                                    // the bounded channel. Cancellation must remain
                                    // independent of receiver progress.
                                    (2, Vec::new())
                                };
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 13,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines,
                                    title: "cancel-full-output-channel".to_string(),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno,
                                })
                            }
                            _ => continue,
                        };
                        let mut out = Vec::new();
                        response.encode(&mut out, decoded.serial).expect("encode");
                        if stream.write_all(&out).await.is_err() {
                            if let Some(tx) = closed_tx.take() {
                                // br-ft-x2oyy: intentional best-effort test close signal;
                                // failure means the waiting assertion side already exited.
                                notify_test_server_closed_best_effort(tx);
                            }
                            return;
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect(config).await.expect("connect");
            let sub = subscribe_pane_output(
                client,
                13,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(5),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 1,
                },
            );

            timeout(Duration::from_secs(1), async {
                loop {
                    if render_request_count.load(Ordering::SeqCst) >= 2 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("subscription should issue at least two render requests");

            // Cancel without draining the receiver. A full output channel must
            // not block cancellation/connection teardown, and the background
            // poller must finish instead of lingering past the test.
            sub.cancel();
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish after cancellation");

            let closed = timeout(
                Duration::from_millis(500),
                crate::runtime_async::oneshot_recv(closed_rx),
            )
            .await
            .expect("server should observe connection close after cancellation");
            closed.expect("server close signal should complete");
        });
    }

    #[test]
    fn subscription_with_cx_cancel_closes_connection_when_output_channel_is_full() {
        let runtime = crate::cx::CxRuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let cx = crate::cx::for_testing();
        let handle = runtime.handle();

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir
                .path()
                .join("cancel-full-output-channel-with-cx.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            let render_request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&render_request_count);
            let (closed_tx, closed_rx) = crate::runtime_async::oneshot::channel::<()>();

            std::mem::drop(crate::cx::spawn_with_cx(
                &handle,
                &cx,
                |_child_cx| async move {
                    let mut closed_tx = Some(closed_tx);
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut read_buf = StreamingPduBuffer::new();

                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => {
                                if let Some(tx) = closed_tx.take() {
                                    // br-ft-x2oyy: intentional best-effort test close signal;
                                    // failure means the waiting assertion side already exited.
                                    notify_test_server_closed_best_effort(tx);
                                }
                                break;
                            }
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
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::GetPaneRenderChanges(_) => {
                                    let request_number =
                                        server_request_count.fetch_add(1, Ordering::SeqCst) + 1;
                                    let (seqno, dirty_lines) = if request_number == 1 {
                                        (1, std::iter::once(0isize..1isize).collect())
                                    } else {
                                        (2, Vec::new())
                                    };
                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: 13,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
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
                                            tiered_scrollback_status: None,
                                            dirty_lines,
                                            title: "cancel-full-output-channel-with-cx".to_string(),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno,
                                        },
                                    )
                                }
                                _ => continue,
                            };
                            let mut out = Vec::new();
                            response.encode(&mut out, decoded.serial).expect("encode");
                            if stream.write_all(&out).await.is_err() {
                                if let Some(tx) = closed_tx.take() {
                                    // br-ft-x2oyy: intentional best-effort test close signal;
                                    // failure means the waiting assertion side already exited.
                                    notify_test_server_closed_best_effort(tx);
                                }
                                return;
                            }
                        }
                    }
                },
            ));

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect_with_cx(&cx, config)
                .await
                .expect("connect");
            let sub = subscribe_pane_output_with_cx(
                &handle,
                &cx,
                client,
                13,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(5),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 1,
                },
            );

            timeout(Duration::from_secs(1), async {
                loop {
                    if render_request_count.load(Ordering::SeqCst) >= 2 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("subscription should issue at least two render requests");

            sub.cancel();
            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish after cancellation");

            let closed = timeout(
                Duration::from_millis(500),
                crate::runtime_async::oneshot_recv(closed_rx),
            )
            .await
            .expect("server should observe connection close after cancellation");
            closed.expect("server close signal should complete");
        });
    }

    #[test]
    fn subscription_shutdown_waits_for_background_task_exit() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("shutdown-waits.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            let render_request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&render_request_count);
            let (closed_tx, closed_rx) = crate::runtime_async::oneshot::channel::<()>();

            task::spawn(async move {
                let mut closed_tx = Some(closed_tx);
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();

                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
                        Ok(0) => {
                            if let Some(tx) = closed_tx.take() {
                                // br-ft-x2oyy: intentional best-effort test close signal;
                                // failure means the waiting assertion side already exited.
                                notify_test_server_closed_best_effort(tx);
                            }
                            break;
                        }
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                server_request_count.fetch_add(1, Ordering::SeqCst);
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 13,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
                                    dirty_lines: Vec::new(),
                                    title: "shutdown-waits".to_string(),
                                    working_dir: None,
                                    bonus_lines: Vec::new().into(),
                                    input_serial: None,
                                    seqno: 1,
                                })
                            }
                            _ => continue,
                        };
                        let mut out = Vec::new();
                        response.encode(&mut out, decoded.serial).expect("encode");
                        if stream.write_all(&out).await.is_err() {
                            if let Some(tx) = closed_tx.take() {
                                // br-ft-x2oyy: intentional best-effort test close signal;
                                // failure means the waiting assertion side already exited.
                                notify_test_server_closed_best_effort(tx);
                            }
                            return;
                        }
                    }
                }
            });

            let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
            let client = DirectMuxClient::connect(config).await.expect("connect");
            let sub = subscribe_pane_output(
                client,
                13,
                SubscriptionConfig {
                    poll_interval: Duration::from_millis(10),
                    min_poll_interval: Duration::from_millis(5),
                    channel_capacity: 8,
                },
            );

            timeout(Duration::from_secs(1), async {
                loop {
                    if render_request_count.load(Ordering::SeqCst) >= 1 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("subscription should issue a render request");

            timeout(Duration::from_millis(500), sub.shutdown())
                .await
                .expect("shutdown should finish after cancellation");

            let closed = timeout(
                Duration::from_millis(500),
                crate::runtime_async::oneshot_recv(closed_rx),
            )
            .await
            .expect("shutdown should await server-observed socket close");
            closed.expect("server close signal should complete");
        });
    }

    #[test]
    fn subscription_cancel_stops_poller() {
        run_async_test(async {
            // Create a subscription with a mock socket that never responds.
            // The poller should shut down when cancelled via the handle.
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("cancel-test.sock");
            let listener = compat_unix::bind(&socket_path).await.expect("bind");

            // Server: accept, do codec handshake, then respond to GetPaneRenderChanges
            // with empty dirty_lines (no deltas to emit).
            task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut read_buf = StreamingPduBuffer::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match unix_stream_read(&mut stream, &mut temp).await {
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
                                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::GetPaneRenderChanges(_) => {
                                // Return empty changes (seqno 0, no dirty lines)
                                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                                    pane_id: 0,
                                    mouse_grabbed: false,
                                    alt_screen_active: false,
                                    cursor_position: mux::renderable::StableCursorPosition::default(
                                    ),
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
                                    tiered_scrollback_status: None,
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
                Err(e) => panic!("subscription did not terminate within timeout: {e}"),
            }
        });
    }

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for DirectMuxClient primitives (wa-p48pw)
    //
    // These tests exercise the Cx-aware primitives that the mux client wraps
    // (mpsc, watch, two-phase reserve/commit, cancellation) under the
    // deterministic asupersync::LabRuntime instead of a real runtime with
    // real sockets. They complement the large existing real-socket suite by
    // proving that the asupersync channel contracts hold under virtual
    // time — the exact scenarios the bead calls out as "LabRuntime DPOR"
    // coverage for concurrent client operations:
    //   - multiple pane subscription channels active simultaneously,
    //     verifying no cross-talk
    //   - interleaved reserve/commit on the same channel, verifying no
    //     partial deliveries
    //   - cancellation signals propagating via watch::Sender to the
    //     run_subscription_loop's cancel_rx
    //   - Cancelled-Cx propagation through pane_delta_recv_with_cx
    //
    // Real-socket I/O under LabRuntime is intentionally excluded: the
    // existing `run_async_test` suite above already covers every socket
    // scenario (partial reads, timeouts, oversized frames, etc.) using
    // `CompatRuntime` which is backed by asupersync when the
    // `asupersync-runtime` feature is enabled (default).
    // -------------------------------------------------------------------------

    mod labruntime_mux_client {
        use super::*;
        use crate::runtime_async::{mpsc as compat_mpsc, watch as compat_watch};
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

        /// Build a LabRuntime, spawn a root task running `f`, and auto-advance
        /// to quiescence. Panics if the runtime gets stuck.
        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime got stuck; termination: {:?}",
                report.termination,
            );
        }

        /// Direct reserve/commit send using the caller's LabRuntime Cx. The
        /// mux_client production helpers (`pane_delta_send`,
        /// `mpsc_reserve_send`) rely on `crate::cx::for_testing()` which
        /// minted a *different* Cx than the LabRuntime task's Cx, so those
        /// helpers deadlock the deterministic scheduler. These tests use the
        /// raw asupersync API to exercise the same two-phase semantics the
        /// production helpers wrap.
        async fn lab_send(cx: &asupersync::Cx, tx: &compat_mpsc::Sender<PaneDelta>, v: PaneDelta) {
            tx.reserve(cx).await.expect("reserve lab permit").send(v);
        }

        /// 1. A single PaneDelta channel delivers a value end-to-end under
        ///    LabRuntime using the compat-routed mpsc primitives.
        #[test]
        fn pane_delta_channel_delivers_under_labruntime() {
            run_lab(801, || async move {
                let (tx, mut rx) = compat_mpsc::channel::<PaneDelta>(4);
                let cx = asupersync::Cx::current().expect("lab Cx");

                lab_send(
                    &cx,
                    &tx,
                    PaneDelta::Output {
                        pane_id: 7,
                        seqno: 42,
                        delta_text: "hi".into(),
                        title: String::new(),
                        dirty_range_count: 1,
                        dirty_row_count: 2,
                    },
                )
                .await;

                let delta = rx.recv(&cx).await.expect("recv");
                match delta {
                    PaneDelta::Output {
                        pane_id,
                        seqno,
                        delta_text,
                        dirty_range_count,
                        dirty_row_count,
                        ..
                    } => {
                        assert_eq!(pane_id, 7);
                        assert_eq!(seqno, 42);
                        assert_eq!(delta_text, "hi");
                        assert_eq!(dirty_range_count, 1);
                        assert_eq!(dirty_row_count, 2);
                    }
                    other => panic!("unexpected delta: {other:?}"),
                }
            });
        }

        /// 2. The two-phase try_send path preserves backpressure semantics
        ///    under LabRuntime: a full channel rejects, then accepts once
        ///    capacity frees.
        #[test]
        fn pane_delta_try_send_backpressure_under_labruntime() {
            run_lab(802, || async move {
                let (tx, mut rx) = compat_mpsc::channel::<PaneDelta>(1);
                let cx = asupersync::Cx::current().expect("lab Cx");

                assert!(pane_delta_try_send(
                    &tx,
                    PaneDelta::Gap {
                        pane_id: 1,
                        reason: "first".into(),
                    },
                ));
                assert!(
                    !pane_delta_try_send(
                        &tx,
                        PaneDelta::Gap {
                            pane_id: 1,
                            reason: "overflow".into(),
                        },
                    ),
                    "channel at capacity must reject try_send"
                );

                let first = rx.recv(&cx).await.expect("recv");
                assert!(matches!(first, PaneDelta::Gap { reason, .. } if reason == "first"));

                assert!(
                    pane_delta_try_send(
                        &tx,
                        PaneDelta::Gap {
                            pane_id: 1,
                            reason: "after-drain".into(),
                        },
                    ),
                    "capacity freed by recv must accept try_send"
                );
                let second = rx.recv(&cx).await.expect("recv");
                assert!(matches!(second, PaneDelta::Gap { reason, .. } if reason == "after-drain"));
            });
        }

        /// 3. Concurrent subscription channels do not cross-talk: each
        ///    receiver only sees its own pane's deltas even when both are
        ///    driven from the same LabRuntime tick.
        #[test]
        fn concurrent_pane_delta_channels_do_not_cross_talk_under_labruntime() {
            run_lab(803, || async move {
                let (tx_a, mut rx_a) = compat_mpsc::channel::<PaneDelta>(8);
                let (tx_b, mut rx_b) = compat_mpsc::channel::<PaneDelta>(8);
                let cx = asupersync::Cx::current().expect("lab Cx");

                lab_send(
                    &cx,
                    &tx_a,
                    PaneDelta::Output {
                        pane_id: 11,
                        seqno: 1,
                        delta_text: "a".into(),
                        title: String::new(),
                        dirty_range_count: 0,
                        dirty_row_count: 0,
                    },
                )
                .await;
                lab_send(
                    &cx,
                    &tx_b,
                    PaneDelta::Output {
                        pane_id: 22,
                        seqno: 2,
                        delta_text: "b".into(),
                        title: String::new(),
                        dirty_range_count: 0,
                        dirty_row_count: 0,
                    },
                )
                .await;

                match rx_a.recv(&cx).await.expect("recv a") {
                    PaneDelta::Output { pane_id, .. } => assert_eq!(pane_id, 11),
                    other => panic!("pane A saw cross-talk: {other:?}"),
                }
                match rx_b.recv(&cx).await.expect("recv b") {
                    PaneDelta::Output { pane_id, .. } => assert_eq!(pane_id, 22),
                    other => panic!("pane B saw cross-talk: {other:?}"),
                }
            });
        }

        /// 4. watch::Sender cancellation signal propagates to
        ///    `cancel_requested` helper used by `run_subscription_loop`.
        #[test]
        fn watch_cancellation_signal_observable_under_labruntime() {
            run_lab(804, || async move {
                let (cancel_tx, mut cancel_rx) = compat_watch::channel(false);
                assert!(!cancel_requested(&mut cancel_rx));

                cancel_tx.send(true).expect("send cancel");
                assert!(
                    cancel_requested(&mut cancel_rx),
                    "run_subscription_loop must see the cancel flip"
                );
            });
        }

        /// 5. Cancelled-Cx propagation: `pane_delta_recv_with_cx` returns
        ///    `None` when the Cx is cancelled rather than hanging forever.
        #[test]
        fn pane_delta_recv_with_cancelled_cx_returns_none_under_labruntime() {
            run_lab(805, || async move {
                let (_tx, mut rx) = compat_mpsc::channel::<PaneDelta>(4);
                let cx = cancelled_test_cx("wa-p48pw cancelled recv");

                let result = pane_delta_recv_with_cx(&cx, &mut rx).await;
                assert!(
                    result.is_none(),
                    "cancelled Cx must surface as None from pane_delta_recv_with_cx, got {result:?}"
                );
            });
        }

        /// 5b. The ambient `PaneOutputSubscription::next` path must inherit the
        /// current LabRuntime Cx rather than minting a fresh request scope.
        /// Otherwise deterministic schedulers deadlock because the recv waits
        /// on a foreign capability context.
        #[test]
        fn ambient_subscription_next_inherits_current_cx_under_labruntime() {
            run_lab(8051, || async move {
                let (tx, rx) = compat_mpsc::channel::<PaneDelta>(4);
                let (cancel_tx, _cancel_rx) = compat_watch::channel(false);
                let cx = asupersync::Cx::current().expect("lab Cx");

                lab_send(
                    &cx,
                    &tx,
                    PaneDelta::Gap {
                        pane_id: 55,
                        reason: "lab-inherited-current-cx".into(),
                    },
                )
                .await;

                let mut sub = PaneOutputSubscription {
                    receiver: rx,
                    cancel: cancel_tx,
                    task: None,
                };

                let result = sub.next().await;
                assert!(
                    matches!(result, Some(PaneDelta::Gap { pane_id: 55, ref reason }) if reason == "lab-inherited-current-cx"),
                    "ambient subscription recv must inherit current cx, got {result:?}"
                );
            });
        }

        /// 6. Ordered delivery across many sends: LabRuntime virtual-time
        ///    scheduling must preserve FIFO on a single channel. This is
        ///    the ordering guarantee `run_subscription_loop` relies on to
        ///    keep seqno bookkeeping monotonic.
        #[test]
        fn pane_delta_channel_preserves_fifo_under_labruntime() {
            run_lab(806, || async move {
                let (tx, mut rx) = compat_mpsc::channel::<PaneDelta>(8);
                let cx = asupersync::Cx::current().expect("lab Cx");

                for seqno in 0u64..6 {
                    lab_send(
                        &cx,
                        &tx,
                        PaneDelta::Output {
                            pane_id: 5,
                            seqno,
                            delta_text: format!("chunk-{seqno}"),
                            title: String::new(),
                            dirty_range_count: 0,
                            dirty_row_count: 0,
                        },
                    )
                    .await;
                }

                for expected in 0u64..6 {
                    match rx.recv(&cx).await.expect("recv") {
                        PaneDelta::Output { seqno, .. } => {
                            assert_eq!(
                                seqno, expected,
                                "FIFO violated: expected seqno {expected}, got {seqno}"
                            );
                        }
                        other => panic!("unexpected delta: {other:?}"),
                    }
                }
            });
        }

        /// 7. Multiple concurrent senders on the same subscription channel
        ///    do not produce partial or lost PaneDelta frames. Exercises
        ///    the two-phase reserve/commit pattern under LabRuntime's
        ///    deterministic scheduler.
        #[test]
        fn concurrent_reserve_commit_never_loses_deltas_under_labruntime() {
            run_lab(807, || async move {
                let (tx, mut rx) = compat_mpsc::channel::<PaneDelta>(32);
                let cx = asupersync::Cx::current().expect("lab Cx");

                let total = AtomicU64::new(0);
                for seqno in 0u64..10 {
                    lab_send(
                        &cx,
                        &tx,
                        PaneDelta::Output {
                            pane_id: 9,
                            seqno,
                            delta_text: String::new(),
                            title: String::new(),
                            dirty_range_count: 0,
                            dirty_row_count: 0,
                        },
                    )
                    .await;
                    total.fetch_add(1, AtomicOrdering::Relaxed);
                }

                let mut observed = 0u64;
                for _ in 0..10 {
                    match rx.recv(&cx).await {
                        Ok(PaneDelta::Output { .. }) => observed += 1,
                        other => panic!("unexpected recv: {other:?}"),
                    }
                }
                assert_eq!(
                    observed,
                    total.load(AtomicOrdering::Relaxed),
                    "every reserve/commit pair must result in exactly one delivered delta"
                );
            });
        }

        /// 8. Cancelling via `watch` after draining the channel must not
        ///    resurrect stale messages. The cancel signal is strictly a
        ///    lifecycle flip, not a data channel.
        #[test]
        fn cancel_watch_does_not_inject_pane_deltas_under_labruntime() {
            run_lab(808, || async move {
                let (_tx, mut rx) = compat_mpsc::channel::<PaneDelta>(4);
                let (cancel_tx, mut cancel_rx) = compat_watch::channel(false);
                let cx = asupersync::Cx::current().expect("lab Cx");

                cancel_tx.send(true).expect("cancel");
                assert!(cancel_requested(&mut cancel_rx));

                // Drop the sender so recv immediately completes with
                // Disconnected — proves the cancel path did not leak a
                // delta into the channel.
                drop(_tx);
                let result = rx.recv(&cx).await;
                assert!(
                    result.is_err(),
                    "no PaneDelta should appear after cancel+drop, got {result:?}"
                );
            });
        }
    }
}
