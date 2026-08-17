#![allow(clippy::future_not_send)]
use crate::PKI;
use crate::dispatch::EstablishedOrderedWindowAuthority;
#[cfg(test)]
use crate::dispatch::established_ordered_window_authority_for_test;
use anyhow::{Context, anyhow};
use codec::{
    ActivatePaneDirection, AdjustPaneSize, CODEC_VERSION, CoherentPaneSnapshot, CreateFloatingPane,
    CycleStack, DecodedPdu, EraseScrollbackRequest, ErrorResponse, GetClientList,
    GetClientListResponse, GetCodecVersionResponse, GetImageCell, GetImageCellResponse, GetLines,
    GetLinesResponse, GetPaneDirection, GetPaneDirectionResponse, GetPaneRenderChanges,
    GetPaneRenderChangesResponse, GetPaneRenderableDimensions, GetPaneRenderableDimensionsResponse,
    GetPaneTieredScrollbackStatusesV1Response, GetSemanticZones, GetSemanticZonesResponse,
    GetTlsCredsResponse, InputSerial, KillPane, ListPanes, ListPanesCoherent,
    ListPanesCoherentOutcome, ListPanesCoherentResponse, ListPanesResponse, ListPanesTabStackEntry,
    ListPanesTabStacks, ListPanesTabStacksResponse, LivenessResponse, MoveFloatingPane,
    MovePaneToNewTab, MovePaneToNewTabResponse, NotifyAlert, PaneTieredScrollbackStatusEntryV1,
    PaneTieredScrollbackStatusOutcomeV1, Pdu, Ping, Pong, RemoveFloatingPane, RenameWorkspace,
    Resize, SearchScrollbackRequest, SearchScrollbackResponse, SelectStackPane, SendKeyDown,
    SendKeyDownTracedV1, SendKeyUp, SendMouseEvent, SendPaste, SetActiveWorkspace, SetClientId,
    SetFloatingPaneZ, SetFocusedPane, SetLayoutCycle, SetPalette, SetPaneZoomed,
    SetWindowWorkspace, SpawnResponse, SpawnV2, SplitPane, SwapToLayout, TabTitleChanged,
    ToggleFloatingPane, TopologyCapabilities, TopologyStreamId, UnitResponse,
    UpdatePaneConstraints, WindowTitleChanged, WriteToPane,
};
use frankenterm_core_audit_types::interaction_flight_recorder_v1::SampledTraceContextV1;
use frankenterm_core_audit_types::interaction_trace_v2::{
    InteractionTraceClockDomain, InteractionTraceCorrelation,
    InteractionTraceCounterUnavailability, InteractionTraceCounters, InteractionTraceGenerations,
    InteractionTraceObservationBoundary, InteractionTraceProducer, InteractionTraceStage,
    InteractionTraceStageOutcome, InteractionTraceTimestamp, InteractionTraceTopology,
    RendererKeypressTraceStage,
};
use frankenterm_flight_recorder::{
    ClockStamp, EventFields, FlightRecorder, ProducerHandle, RecordOutcome, RecorderError,
    TraceAdmission, TraceToken,
};
use frankenterm_sigpipe::{RecoverablePanicSite, catch_recoverable};
use mux::client::ClientId;
use mux::pane::{CachePolicy, PaneId};
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::{CurrentPane, Mux, PaneRegistrationHandle};
use promise::spawn::spawn_into_main_thread;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
#[cfg(test)]
use termwiz::surface::SEQ_ZERO;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_term::StableRowIndex;
use wezterm_term::terminal::Alert;

/// Explicit, process-owned recorder authority for mux-server connections.
///
/// This is deliberately injected through [`crate::dispatch::DispatchRuntimeConfig`]
/// rather than installed in a process-global singleton. Each binary-protocol
/// connection claims one recorder shard before it starts decoding requests;
/// request hot paths therefore never allocate or perform hidden first-use
/// producer registration.
#[derive(Debug)]
pub struct DispatchTraceAuthority {
    recorder: Arc<FlightRecorder>,
    next_shard: AtomicUsize,
    next_connection_generation: AtomicU64,
    process_identity: DispatchTraceProcessIdentity,
    clock_origin: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DispatchTraceProcessIdentity {
    host_id: u64,
    process_id: u32,
    process_generation: u64,
    clock_id: u64,
}

impl DispatchTraceAuthority {
    #[must_use]
    pub fn new(recorder: Arc<FlightRecorder>) -> Arc<Self> {
        let nonce = *uuid::Uuid::new_v4().as_bytes();
        let host_id = u64::from_le_bytes([
            nonce[0], nonce[1], nonce[2], nonce[3], nonce[4], nonce[5], nonce[6], nonce[7],
        ])
        .max(1);
        let process_generation = u64::from_le_bytes([
            nonce[8], nonce[9], nonce[10], nonce[11], nonce[12], nonce[13], nonce[14], nonce[15],
        ])
        .max(1);
        let clock_id = host_id
            .rotate_left(17)
            .wrapping_add(process_generation)
            .max(1);
        Arc::new(Self {
            recorder,
            next_shard: AtomicUsize::new(0),
            next_connection_generation: AtomicU64::new(1),
            process_identity: DispatchTraceProcessIdentity {
                host_id,
                process_id: std::process::id().max(1),
                process_generation,
                clock_id,
            },
            clock_origin: Instant::now(),
        })
    }

    pub(crate) fn claim_session(
        self: &Arc<Self>,
        topology_stream_id: TopologyStreamId,
    ) -> Option<Rc<SessionTraceProducer>> {
        let shard_count = usize::from(self.recorder.config().capacity().shard_count);
        let start = self.next_shard.fetch_add(1, Ordering::Relaxed);
        for offset in 0..shard_count {
            let shard_index = start.wrapping_add(offset) % shard_count;
            match self.recorder.register_producer(shard_index) {
                Ok(producer) => {
                    let Some(connection_generation) = self.allocate_connection_generation() else {
                        metrics::counter!(
                            "mux.server.trace_session_registration",
                            "outcome" => "connection_generation_exhausted"
                        )
                        .increment(1);
                        return None;
                    };
                    return Some(Rc::new(SessionTraceProducer {
                        authority: Arc::clone(self),
                        producer,
                        topology_stream_id,
                        connection_generation,
                        _main_thread_affinity: Cell::new(()),
                    }));
                }
                Err(RecorderError::ShardAlreadyClaimed { .. }) => {}
                Err(error) => {
                    log::warn!(
                        "mux-server trace producer registration failed; tracing this connection is disabled: {error}"
                    );
                    metrics::counter!(
                        "mux.server.trace_session_registration",
                        "outcome" => "recorder_rejected"
                    )
                    .increment(1);
                    return None;
                }
            }
        }

        metrics::counter!(
            "mux.server.trace_session_registration",
            "outcome" => "shards_exhausted"
        )
        .increment(1);
        None
    }

    fn allocate_connection_generation(&self) -> Option<u64> {
        let mut observed = self.next_connection_generation.load(Ordering::Relaxed);
        loop {
            if observed == u64::MAX {
                return None;
            }
            match self.next_connection_generation.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(observed),
                Err(actual) => observed = actual,
            }
        }
    }
}

/// One connection's explicitly registered, thread-affine recorder producer.
///
/// `ProducerHandle` and the `Rc` owner are intentionally neither `Send` nor
/// `Sync`. The binary dispatch future is consequently a main-thread-local
/// future; listener threads perform one small `Send` bridge and then spawn it
/// with `promise::spawn::spawn` on the destination thread.
#[derive(Debug)]
pub(crate) struct SessionTraceProducer {
    authority: Arc<DispatchTraceAuthority>,
    producer: ProducerHandle,
    topology_stream_id: TopologyStreamId,
    connection_generation: u64,
    _main_thread_affinity: Cell<()>,
}

impl SessionTraceProducer {
    fn connection_generation(&self) -> u64 {
        self.connection_generation
    }

    fn producer_identity(&self) -> Option<InteractionTraceProducer> {
        let thread_id = u64::try_from(self.producer.shard_index())
            .ok()?
            .checked_add(1)?;
        let process = self.authority.process_identity;
        Some(InteractionTraceProducer {
            host_id: process.host_id,
            process_id: process.process_id,
            process_generation: process.process_generation,
            thread_id,
            connection_generation: Some(self.connection_generation()),
        })
    }

    fn timestamp_at(&self, observed: Instant) -> Option<InteractionTraceTimestamp> {
        let elapsed = observed.checked_duration_since(self.authority.clock_origin)?;
        let monotonic_ns = u64::try_from(elapsed.as_nanos()).ok()?;
        let process = self.authority.process_identity;
        Some(InteractionTraceTimestamp {
            clock_domain: InteractionTraceClockDomain {
                host_id: process.host_id,
                process_generation: process.process_generation,
                clock_id: process.clock_id,
            },
            monotonic_ns,
            wall_time_unix_ns: None,
        })
    }

    fn admit_remote_trace(&self, context: SampledTraceContextV1) -> Option<TraceToken> {
        match self
            .authority
            .recorder
            .admit_remote_trace(&self.producer, context)
        {
            TraceAdmission::Admitted { token, .. } => Some(token),
            TraceAdmission::Off => None,
            TraceAdmission::Closing => {
                metrics::counter!(
                    "mux.server.trace_remote_admission",
                    "outcome" => "closing"
                )
                .increment(1);
                None
            }
            TraceAdmission::InvalidRemoteContext => {
                metrics::counter!(
                    "mux.server.trace_remote_admission",
                    "outcome" => "invalid_context"
                )
                .increment(1);
                None
            }
            TraceAdmission::WrongRecorder
            | TraceAdmission::SampledOut { .. }
            | TraceAdmission::TraceIdExhausted { .. } => {
                metrics::counter!(
                    "mux.server.trace_remote_admission",
                    "outcome" => "authority_rejected"
                )
                .increment(1);
                None
            }
        }
    }

    fn record_server_stage(
        &self,
        token: TraceToken,
        stage: RendererKeypressTraceStage,
        topology: InteractionTraceTopology,
        started_at: Instant,
        completed_at: Instant,
    ) {
        let Some(producer) = self.producer_identity() else {
            metrics::counter!(
                "mux.server.trace_event",
                "outcome" => "producer_identity_exhausted"
            )
            .increment(1);
            return;
        };
        let (Some(started_at), Some(completed_at)) = (
            self.timestamp_at(started_at),
            self.timestamp_at(completed_at),
        ) else {
            metrics::counter!("mux.server.trace_event", "outcome" => "clock_invalid").increment(1);
            return;
        };
        let stage = InteractionTraceStage::Keypress(stage);
        let fields = match EventFields::new(
            u64::from(stage.ordinal()),
            u64::from(stage.ordinal()) + 1,
            Some(u64::from(stage.ordinal())),
            stage,
            InteractionTraceStageOutcome::Performed,
            producer,
            topology,
            ClockStamp {
                started_at,
                completed_at,
            },
            InteractionTraceCorrelation::ExactProtocol {
                protocol_token: token.trace_id().sequence,
                protocol_generation: self.connection_generation(),
            },
            InteractionTraceCounters {
                work_units: 1,
                rpc_count: 1,
                ..InteractionTraceCounters::default()
            },
            InteractionTraceCounterUnavailability {
                queue_depth: true,
                oldest_queue_age_ns: true,
                work_units: false,
                bytes: true,
                rows: true,
                allocation_count: true,
                allocated_bytes: true,
                copy_count: true,
                copied_bytes: true,
                rpc_count: false,
                delta_count: true,
                dirty_rows: true,
                full_viewport_clones: true,
                cursor_row_duplicates: true,
                paint_count: true,
                frame_count: true,
            },
            InteractionTraceGenerations::default(),
            InteractionTraceObservationBoundary::InternalState,
            None,
        ) {
            Ok(fields) => fields,
            Err(error) => {
                log::warn!("refusing invalid mux-server trace event: {error}");
                metrics::counter!("mux.server.trace_event", "outcome" => "invalid_fields")
                    .increment(1);
                return;
            }
        };
        let outcome = self
            .authority
            .recorder
            .record(&self.producer, token, &fields);
        let outcome = match outcome {
            RecordOutcome::Recorded { .. } => "recorded",
            RecordOutcome::QueueFull { .. } => "queue_full",
            RecordOutcome::Closing { .. } | RecordOutcome::OutsideEpoch => "closing",
            RecordOutcome::Off => "off",
            RecordOutcome::WrongRecorder | RecordOutcome::EpochMismatch { .. } => {
                "authority_rejected"
            }
            RecordOutcome::ClockInvalid { .. } => "clock_invalid",
        };
        metrics::counter!("mux.server.trace_event", "outcome" => outcome).increment(1);
    }

    fn record_mux_dispatch_start(
        &self,
        admission: &AdmittedInputTraceV1,
        dispatch_started_at: Instant,
    ) {
        if admission.stream_id != self.topology_stream_id {
            metrics::counter!(
                "mux.server.trace_event",
                "outcome" => "connection_generation_mismatch"
            )
            .increment(1);
            return;
        }
        let (Some(token), Some(topology), Some(dispatch_queued_at)) = (
            admission.recorder_token,
            admission.topology,
            admission.dispatch_queued_at,
        ) else {
            return;
        };
        self.record_server_stage(
            token,
            RendererKeypressTraceStage::ServerDispatchMuxWait,
            topology,
            dispatch_queued_at,
            dispatch_started_at,
        );
    }
}

/// Checked, cycle-breaking conversions between the ordered-window wire schema
/// and mux authority.
///
/// The PDU handlers are source-wired, but runtime capability advertisement and
/// dispatch remain intentionally dormant. Keeping these helpers together makes
/// the eventual activation point explicit: connection-scoped stream/domain
/// authorization happens before mux mutation, and a fully converted outbound
/// value is validated before it is enqueued.
#[allow(dead_code)]
mod ordered_window_adapter {
    #[derive(Debug, Eq, PartialEq)]
    pub(super) enum OrderedWindowAdapterError {
        LimitContractMismatch {
            mux_max_tabs_per_window: usize,
            codec_max_tabs_per_window: usize,
        },
        CodecContract(codec::OrderedWindowProtocolError),
        ReservedWireId {
            field: &'static str,
            value: u64,
        },
        MuxIdDoesNotFitU64 {
            field: &'static str,
            value: usize,
        },
        RevisionExhausted {
            field: &'static str,
        },
        MuxRequestRejected(mux::WindowReorderMalformed),
        DigestAuthorityMismatch {
            wire: [u8; 32],
            mux: [u8; 32],
        },
    }

    impl std::fmt::Display for OrderedWindowAdapterError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::LimitContractMismatch {
                    mux_max_tabs_per_window,
                    codec_max_tabs_per_window,
                } => write!(
                    formatter,
                    "ordered-window tab limit mismatch: mux={mux_max_tabs_per_window}, \
                     codec={codec_max_tabs_per_window}"
                ),
                Self::CodecContract(error) => std::fmt::Display::fmt(error, formatter),
                Self::ReservedWireId { field, value } => {
                    write!(
                        formatter,
                        "ordered-window {field} uses reserved value {value}"
                    )
                }
                Self::MuxIdDoesNotFitU64 { field, value } => write!(
                    formatter,
                    "mux ordered-window {field}={value} does not fit the u64 wire width"
                ),
                Self::RevisionExhausted { field } => write!(
                    formatter,
                    "ordered-window {field} uses the terminal u64::MAX sentinel"
                ),
                Self::MuxRequestRejected(error) => {
                    write!(formatter, "mux rejected ordered-window request: {error:?}")
                }
                Self::DigestAuthorityMismatch { wire, mux } => write!(
                    formatter,
                    "ordered-window digest authority mismatch: wire={wire:02x?}, mux={mux:02x?}"
                ),
            }
        }
    }

    impl std::error::Error for OrderedWindowAdapterError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::CodecContract(error) => Some(error),
                _ => None,
            }
        }
    }

    impl From<codec::OrderedWindowProtocolError> for OrderedWindowAdapterError {
        fn from(error: codec::OrderedWindowProtocolError) -> Self {
            Self::CodecContract(error)
        }
    }

    /// Enforce the shared count boundary at the server seam before either
    /// ordered-window capability is ever advertised.
    pub(super) fn verify_limit_contract() -> Result<(), OrderedWindowAdapterError> {
        let mux_max_tabs_per_window = mux::window::MAX_TABS_PER_ORDERED_WINDOW;
        let codec_max_tabs_per_window = codec::MAX_ORDERED_TABS_PER_WINDOW;
        if mux_max_tabs_per_window != codec_max_tabs_per_window {
            return Err(OrderedWindowAdapterError::LimitContractMismatch {
                mux_max_tabs_per_window,
                codec_max_tabs_per_window,
            });
        }
        Ok(())
    }

    fn mux_id_to_wire(field: &'static str, value: usize) -> Result<u64, OrderedWindowAdapterError> {
        let wire_value = u64::try_from(value)
            .map_err(|_| OrderedWindowAdapterError::MuxIdDoesNotFitU64 { field, value })?;
        if wire_value == u64::MAX {
            return Err(OrderedWindowAdapterError::ReservedWireId {
                field,
                value: wire_value,
            });
        }
        Ok(wire_value)
    }

    fn remote_window_id_to_mux(
        window_id: codec::RemoteWindowId,
    ) -> Result<mux::window::WindowId, OrderedWindowAdapterError> {
        if window_id.get() == u64::MAX {
            return Err(OrderedWindowAdapterError::ReservedWireId {
                field: "window_id",
                value: window_id.get(),
            });
        }
        window_id.try_into_usize().map_err(Into::into)
    }

    fn remote_tab_id_to_mux(
        tab_id: codec::RemoteTabId,
    ) -> Result<mux::tab::TabId, OrderedWindowAdapterError> {
        if tab_id.get() == u64::MAX {
            return Err(OrderedWindowAdapterError::ReservedWireId {
                field: "tab_id",
                value: tab_id.get(),
            });
        }
        tab_id.try_into_usize().map_err(Into::into)
    }

    fn mux_window_id_to_remote(
        window_id: mux::window::WindowId,
    ) -> Result<codec::RemoteWindowId, OrderedWindowAdapterError> {
        Ok(codec::RemoteWindowId::new(mux_id_to_wire(
            "window_id",
            window_id,
        )?))
    }

    fn mux_tab_id_to_remote(
        tab_id: mux::tab::TabId,
    ) -> Result<codec::RemoteTabId, OrderedWindowAdapterError> {
        Ok(codec::RemoteTabId::new(mux_id_to_wire("tab_id", tab_id)?))
    }

    fn codec_revision_to_mux(
        revision: codec::WindowOrderRevision,
    ) -> Result<mux::window::WindowOrderRevision, OrderedWindowAdapterError> {
        if revision.get() == u64::MAX {
            return Err(OrderedWindowAdapterError::RevisionExhausted {
                field: "window_order_revision",
            });
        }
        Ok(mux::window::WindowOrderRevision::new(revision.get()))
    }

    fn mux_revision_to_codec(
        revision: mux::window::WindowOrderRevision,
    ) -> Result<codec::WindowOrderRevision, OrderedWindowAdapterError> {
        if revision.get() == u64::MAX {
            return Err(OrderedWindowAdapterError::RevisionExhausted {
                field: "window_order_revision",
            });
        }
        Ok(codec::WindowOrderRevision::new(revision.get()))
    }

    fn checked_topology_revision(
        revision: mux::TopologyRevision,
    ) -> Result<mux::TopologyRevision, OrderedWindowAdapterError> {
        if revision.get() == u64::MAX {
            return Err(OrderedWindowAdapterError::RevisionExhausted {
                field: "topology_revision",
            });
        }
        Ok(revision)
    }

    /// Validate the complete untrusted wire request, including its canonical
    /// digest, before narrowing any identifier or constructing mux authority.
    ///
    /// The caller must separately bind `domain_binding_id` and `stream_id` to
    /// the established connection generation. The stable domain binding is
    /// included in mux digest authority; the reconnect-rotating stream is
    /// deliberately excluded.
    pub(super) fn codec_reorder_request_to_mux(
        request: &codec::ReorderWindowTabsV1,
    ) -> Result<mux::ReorderWindowTabsRequest, OrderedWindowAdapterError> {
        verify_limit_contract()?;
        request.validate()?;

        let desired_tab_ids = request
            .desired_tab_ids
            .iter()
            .copied()
            .map(remote_tab_id_to_mux)
            .collect::<Result<Vec<_>, _>>()?;
        let desired_active_tab_id = request
            .desired_active_tab_id
            .map(remote_tab_id_to_mux)
            .transpose()?;

        let mux_request = mux::ReorderWindowTabsRequest::try_new_v1(
            request.domain_binding_id.as_bytes(),
            request.session_incarnation,
            remote_window_id_to_mux(request.window_id)?,
            codec_revision_to_mux(request.expected_order_revision)?,
            desired_tab_ids,
            desired_active_tab_id,
            mux::WindowOrderMutationId::new(
                request.mutation_id.namespace,
                request.mutation_id.sequence,
            ),
        )
        .map_err(OrderedWindowAdapterError::MuxRequestRejected)?;
        let mux_digest = mux_request.request_digest().as_bytes();
        let wire_digest = request.digest.as_bytes();
        if mux_digest != wire_digest {
            return Err(OrderedWindowAdapterError::DigestAuthorityMismatch {
                wire: wire_digest,
                mux: mux_digest,
            });
        }
        Ok(mux_request)
    }

    fn mux_order_components_to_codec(
        window_id: mux::window::WindowId,
        order_revision: mux::window::WindowOrderRevision,
        ordered_tab_ids: impl std::iter::ExactSizeIterator<Item = mux::tab::TabId>,
        active_tab_id: Option<mux::tab::TabId>,
    ) -> Result<codec::OrderedWindowStateV1, OrderedWindowAdapterError> {
        verify_limit_contract()?;
        let window_id = mux_window_id_to_remote(window_id)?;
        let tab_count = ordered_tab_ids.len();
        if tab_count > codec::MAX_ORDERED_TABS_PER_WINDOW {
            return Err(codec::OrderedWindowProtocolError::TooManyTabs {
                window_id: window_id.get(),
                count: tab_count,
                max: codec::MAX_ORDERED_TABS_PER_WINDOW,
            }
            .into());
        }
        let state = codec::OrderedWindowStateV1 {
            window_id,
            order_revision: mux_revision_to_codec(order_revision)?,
            ordered_tab_ids: ordered_tab_ids
                .map(mux_tab_id_to_remote)
                .collect::<Result<Vec<_>, _>>()?,
            active_tab_id: active_tab_id.map(mux_tab_id_to_remote).transpose()?,
        };
        state.validate()?;
        Ok(state)
    }

    pub(super) fn mux_frozen_window_order_to_codec(
        window: &mux::window::FrozenWindowOrder,
    ) -> Result<codec::OrderedWindowStateV1, OrderedWindowAdapterError> {
        mux_order_components_to_codec(
            window.window_id(),
            window.order_revision(),
            window.ordered_tab_ids(),
            window.active_tab_id(),
        )
    }

    pub(super) fn mux_window_order_state_to_codec(
        window: &mux::WindowOrderState,
    ) -> Result<codec::OrderedWindowStateV1, OrderedWindowAdapterError> {
        mux_order_components_to_codec(
            window.window_id,
            window.order_revision,
            window.ordered_tab_ids.iter().copied(),
            window.active_tab_id,
        )
    }

    fn mux_commit_to_codec(
        commit: &mux::WindowOrderCommit,
    ) -> Result<codec::WindowOrderCommitV1, OrderedWindowAdapterError> {
        Ok(codec::WindowOrderCommitV1 {
            topology_revision: checked_topology_revision(commit.topology_revision)?,
            window: mux_window_order_state_to_codec(&commit.window)?,
        })
    }

    fn mux_terminal_outcome_to_codec(
        outcome: &mux::WindowReorderTerminalOutcome,
    ) -> Result<codec::WindowReorderTerminalOutcomeV1, OrderedWindowAdapterError> {
        Ok(match outcome {
            mux::WindowReorderTerminalOutcome::Applied(commit) => {
                codec::WindowReorderTerminalOutcomeV1::Applied(mux_commit_to_codec(commit)?)
            }
            mux::WindowReorderTerminalOutcome::Conflict(commit) => {
                codec::WindowReorderTerminalOutcomeV1::Conflict(mux_commit_to_codec(commit)?)
            }
            mux::WindowReorderTerminalOutcome::StaleIncarnation
            | mux::WindowReorderTerminalOutcome::MissingWindow { .. } => {
                codec::WindowReorderTerminalOutcomeV1::StaleIncarnation
            }
            mux::WindowReorderTerminalOutcome::Malformed(_) => {
                codec::WindowReorderTerminalOutcomeV1::Malformed
            }
            mux::WindowReorderTerminalOutcome::Exhausted => {
                codec::WindowReorderTerminalOutcomeV1::Exhausted
            }
        })
    }

    /// Collapse mux-only diagnostics into the closed v1 wire vocabulary while
    /// preserving applied/conflict commits and exact replay classification.
    pub(super) fn mux_reorder_result_to_codec(
        result: &mux::ReorderWindowTabsResult,
    ) -> Result<codec::ReorderWindowTabsV1Outcome, OrderedWindowAdapterError> {
        Ok(match result {
            mux::ReorderWindowTabsResult::Decision(mux::WindowReorderTerminalOutcome::Applied(
                commit,
            )) => codec::ReorderWindowTabsV1Outcome::Applied(mux_commit_to_codec(commit)?),
            mux::ReorderWindowTabsResult::Decision(
                mux::WindowReorderTerminalOutcome::Conflict(commit),
            ) => codec::ReorderWindowTabsV1Outcome::Conflict(mux_commit_to_codec(commit)?),
            mux::ReorderWindowTabsResult::Decision(
                mux::WindowReorderTerminalOutcome::StaleIncarnation
                | mux::WindowReorderTerminalOutcome::MissingWindow { .. },
            ) => codec::ReorderWindowTabsV1Outcome::StaleIncarnation,
            mux::ReorderWindowTabsResult::Decision(
                mux::WindowReorderTerminalOutcome::Malformed(_),
            )
            | mux::ReorderWindowTabsResult::Equivocation { .. } => {
                codec::ReorderWindowTabsV1Outcome::Malformed
            }
            mux::ReorderWindowTabsResult::Decision(
                mux::WindowReorderTerminalOutcome::Exhausted,
            ) => codec::ReorderWindowTabsV1Outcome::Exhausted,
            mux::ReorderWindowTabsResult::Replay(outcome) => {
                codec::ReorderWindowTabsV1Outcome::Replay(mux_terminal_outcome_to_codec(outcome)?)
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;

        fn sample_codec_request() -> codec::ReorderWindowTabsV1 {
            codec::ReorderWindowTabsV1 {
                protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                domain_binding_id: codec::DomainBindingId::from_bytes([0x11; 16]),
                stream_id: codec::TopologyStreamId::from_bytes([0x22; 16]),
                session_incarnation: mux::MuxSessionIncarnation::from_bytes([0x33; 16]),
                window_id: codec::RemoteWindowId::new(0),
                expected_order_revision: codec::WindowOrderRevision::new(7),
                desired_tab_ids: vec![codec::RemoteTabId::new(0), codec::RemoteTabId::new(9)],
                desired_active_tab_id: Some(codec::RemoteTabId::new(9)),
                mutation_id: codec::WindowOrderMutationId::new([0x44; 16], 5),
                digest: codec::WindowReorderDigest::ZERO,
            }
            .with_computed_digest()
        }

        fn sample_mux_commit() -> mux::WindowOrderCommit {
            mux::WindowOrderCommit {
                topology_revision: mux::TopologyRevision::new(12),
                window: mux::WindowOrderState {
                    window_id: 0,
                    order_revision: mux::window::WindowOrderRevision::new(8),
                    ordered_tab_ids: Arc::from([0, 9]),
                    active_tab_id: Some(9),
                },
            }
        }

        #[test]
        fn identifier_conversions_admit_zero_and_reject_reserved_max() {
            assert_eq!(verify_limit_contract(), Ok(()));
            assert_eq!(
                remote_window_id_to_mux(codec::RemoteWindowId::new(0)),
                Ok(0)
            );
            assert_eq!(remote_tab_id_to_mux(codec::RemoteTabId::new(0)), Ok(0));
            assert_eq!(
                mux_window_id_to_remote(0),
                Ok(codec::RemoteWindowId::new(0))
            );
            assert_eq!(mux_tab_id_to_remote(0), Ok(codec::RemoteTabId::new(0)));
            assert_eq!(
                remote_window_id_to_mux(codec::RemoteWindowId::new(u64::MAX)),
                Err(OrderedWindowAdapterError::ReservedWireId {
                    field: "window_id",
                    value: u64::MAX,
                })
            );
            assert_eq!(
                remote_tab_id_to_mux(codec::RemoteTabId::new(u64::MAX)),
                Err(OrderedWindowAdapterError::ReservedWireId {
                    field: "tab_id",
                    value: u64::MAX,
                })
            );
            #[cfg(target_pointer_width = "64")]
            assert_eq!(
                mux_window_id_to_remote(usize::MAX),
                Err(OrderedWindowAdapterError::ReservedWireId {
                    field: "window_id",
                    value: u64::MAX,
                })
            );
        }

        #[cfg(target_pointer_width = "32")]
        #[test]
        fn identifier_conversion_rejects_wire_values_wider_than_mux_ids() {
            let value = u64::from(u32::MAX) + 1;
            assert_eq!(
                remote_tab_id_to_mux(codec::RemoteTabId::new(value)),
                Err(OrderedWindowAdapterError::CodecContract(
                    codec::OrderedWindowProtocolError::WireIdDoesNotFitUsize {
                        field: "RemoteTabId",
                        value,
                    }
                ))
            );
        }

        #[test]
        fn reorder_request_conversion_validates_digest_before_narrowing() {
            let request = sample_codec_request();
            let converted = codec_reorder_request_to_mux(&request)
                .expect("canonical bounded request should cross the adapter");
            assert_eq!(converted.session_incarnation(), request.session_incarnation);
            assert_eq!(converted.window_id(), 0);
            assert_eq!(converted.expected_order_revision().get(), 7);
            assert_eq!(converted.desired_tab_ids(), [0, 9]);
            assert_eq!(converted.desired_active_tab_id(), Some(9));
            assert_eq!(converted.mutation_id().namespace, [0x44; 16]);
            assert_eq!(converted.mutation_id().sequence, 5);
            assert_eq!(
                converted.request_digest().as_bytes(),
                request.digest.as_bytes()
            );

            let mut forged = request;
            forged.digest = codec::WindowReorderDigest::from_bytes([0xff; 32]);
            assert!(matches!(
                codec_reorder_request_to_mux(&forged),
                Err(OrderedWindowAdapterError::CodecContract(
                    codec::OrderedWindowProtocolError::DigestMismatch { .. }
                ))
            ));
        }

        #[test]
        fn mux_window_state_conversion_is_checked_before_wire_use() {
            let commit = sample_mux_commit();
            let converted = mux_window_order_state_to_codec(&commit.window)
                .expect("valid mux authority should have a wire representation");
            assert_eq!(converted.window_id, codec::RemoteWindowId::new(0));
            assert_eq!(converted.order_revision.get(), 8);
            assert_eq!(
                converted.ordered_tab_ids,
                vec![codec::RemoteTabId::new(0), codec::RemoteTabId::new(9)]
            );
            assert_eq!(converted.active_tab_id, Some(codec::RemoteTabId::new(9)));
            assert_eq!(converted.validate(), Ok(()));

            let duplicate = mux::WindowOrderState {
                ordered_tab_ids: Arc::from([9, 9]),
                ..commit.window.clone()
            };
            assert!(matches!(
                mux_window_order_state_to_codec(&duplicate),
                Err(OrderedWindowAdapterError::CodecContract(
                    codec::OrderedWindowProtocolError::DuplicateTabId { tab_id: 9 }
                ))
            ));

            let oversized_count = codec::MAX_ORDERED_TABS_PER_WINDOW + 1;
            let oversized = mux::WindowOrderState {
                ordered_tab_ids: Arc::from(vec![0; oversized_count]),
                ..commit.window.clone()
            };
            assert_eq!(
                mux_window_order_state_to_codec(&oversized),
                Err(OrderedWindowAdapterError::CodecContract(
                    codec::OrderedWindowProtocolError::TooManyTabs {
                        window_id: 0,
                        count: oversized_count,
                        max: codec::MAX_ORDERED_TABS_PER_WINDOW,
                    }
                ))
            );

            let exhausted = mux::WindowOrderState {
                order_revision: mux::window::WindowOrderRevision::new(u64::MAX),
                ..commit.window
            };
            assert_eq!(
                mux_window_order_state_to_codec(&exhausted),
                Err(OrderedWindowAdapterError::RevisionExhausted {
                    field: "window_order_revision",
                })
            );
        }

        #[test]
        fn mux_reorder_results_map_to_the_closed_wire_vocabulary() {
            let commit = sample_mux_commit();
            let applied = mux_reorder_result_to_codec(&mux::ReorderWindowTabsResult::Decision(
                mux::WindowReorderTerminalOutcome::Applied(commit.clone()),
            ))
            .expect("valid applied commit should convert");
            assert!(matches!(
                applied,
                codec::ReorderWindowTabsV1Outcome::Applied(codec::WindowOrderCommitV1 {
                    topology_revision,
                    ..
                }) if topology_revision == mux::TopologyRevision::new(12)
            ));

            let replay = mux_reorder_result_to_codec(&mux::ReorderWindowTabsResult::Replay(
                mux::WindowReorderTerminalOutcome::Conflict(commit),
            ))
            .expect("valid replayed conflict should convert");
            assert!(matches!(
                replay,
                codec::ReorderWindowTabsV1Outcome::Replay(
                    codec::WindowReorderTerminalOutcomeV1::Conflict(_)
                )
            ));

            let missing = mux_reorder_result_to_codec(&mux::ReorderWindowTabsResult::Decision(
                mux::WindowReorderTerminalOutcome::MissingWindow { window_id: 71 },
            ))
            .expect("missing windows collapse to stale incarnation on v1 wire");
            assert_eq!(missing, codec::ReorderWindowTabsV1Outcome::StaleIncarnation);

            let malformed =
                mux_reorder_result_to_codec(&mux::ReorderWindowTabsResult::Equivocation {
                    mutation_id: mux::WindowOrderMutationId::new([0x55; 16], 2),
                    retained_digest: mux::WindowReorderDigest::from_bytes([0x66; 32]),
                    attempted_digest: mux::WindowReorderDigest::from_bytes([0x77; 32]),
                })
                .expect("equivocation has a finite malformed wire outcome");
            assert_eq!(malformed, codec::ReorderWindowTabsV1Outcome::Malformed);
        }

        #[test]
        fn topology_revision_sentinel_never_crosses_the_adapter() {
            let mut commit = sample_mux_commit();
            commit.topology_revision = mux::TopologyRevision::new(u64::MAX);
            assert_eq!(
                mux_reorder_result_to_codec(&mux::ReorderWindowTabsResult::Decision(
                    mux::WindowReorderTerminalOutcome::Applied(commit),
                )),
                Err(OrderedWindowAdapterError::RevisionExhausted {
                    field: "topology_revision",
                })
            );
        }
    }
}

pub(crate) fn frozen_window_order_to_codec(
    window: &mux::window::FrozenWindowOrder,
) -> anyhow::Result<codec::OrderedWindowStateV1> {
    ordered_window_adapter::mux_frozen_window_order_to_codec(window)
        .context("converting frozen mux window order for ordered topology delivery")
}

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(DecodedPdu, PduDeliveryClass) -> anyhow::Result<()> + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PduDeliveryClass {
    Control,
    Bulk,
}

impl PduSender {
    pub fn send_control(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu, PduDeliveryClass::Control)
    }

    pub fn send_bulk(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu, PduDeliveryClass::Bulk)
    }

    /// Construct an enqueue boundary.
    ///
    /// `Ok(())` means the PDU was admitted. `Err` must guarantee that it was
    /// not admitted and ownership remained local; the render code may retry
    /// after an explicit error. A panic is treated as indeterminate because a
    /// callback can unwind after publication, and therefore retires any
    /// affected legacy render/notification authority instead of retrying.
    pub fn new<T>(f: T) -> Self
    where
        T: Fn(DecodedPdu, PduDeliveryClass) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
}

const SESSION_RETIRED: usize = 1usize << (usize::BITS - 1);
const SESSION_OPERATION_MASK: usize = SESSION_RETIRED - 1;

struct SessionIncarnation {
    operation_state: AtomicUsize,
}

impl SessionIncarnation {
    fn new() -> Self {
        Self {
            operation_state: AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<SessionOperationLease> {
        let mut observed = self.operation_state.load(Ordering::Acquire);
        loop {
            if observed & SESSION_RETIRED != 0 {
                return None;
            }

            let active = observed & SESSION_OPERATION_MASK;
            if active == SESSION_OPERATION_MASK {
                return None;
            }

            match self.operation_state.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(SessionOperationLease {
                        incarnation: Arc::clone(self),
                    });
                }
                Err(current) => observed = current,
            }
        }
    }

    fn retire(&self) {
        self.operation_state
            .fetch_or(SESSION_RETIRED, Ordering::AcqRel);
    }

    fn release_operation(&self) {
        let mut observed = self.operation_state.load(Ordering::Acquire);
        loop {
            let active = observed & SESSION_OPERATION_MASK;
            if active == 0 {
                debug_assert!(
                    false,
                    "session operation lease released without a matching acquisition"
                );
                return;
            }

            match self.operation_state.compare_exchange_weak(
                observed,
                observed - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }
}

struct SessionOperationLease {
    incarnation: Arc<SessionIncarnation>,
}

impl Drop for SessionOperationLease {
    fn drop(&mut self) {
        self.incarnation.release_operation();
    }
}

/// Sampled input context admitted by one exact server connection generation.
///
/// This contains only the frozen numeric trace context, the unpredictable
/// topology-stream identity allocated for this connection, and the existing
/// content-free pane/input-serial request identity. Key-event content never
/// enters the authority object. The intentionally non-`Clone` token is
/// constructed and revalidated before client-activity bookkeeping or pane
/// mutation, then consumed by the one admitted dispatch.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct AdmittedInputTraceV1 {
    context: SampledTraceContextV1,
    stream_id: TopologyStreamId,
    pane_id: PaneId,
    input_serial: InputSerial,
    recorder_token: Option<TraceToken>,
    topology: Option<InteractionTraceTopology>,
    dispatch_queued_at: Option<Instant>,
}

impl AdmittedInputTraceV1 {
    pub(crate) fn admit(
        request: &SendKeyDownTracedV1,
        stream_id: TopologyStreamId,
        local_codec_version: usize,
    ) -> anyhow::Result<Self> {
        if local_codec_version < codec::SAMPLED_INPUT_TRACE_V1_MIN_CODEC_VERSION {
            return Err(anyhow!(
                "sampled key input is unavailable in this codec dialect"
            ));
        }
        if stream_id.as_bytes() == [0; 16] {
            return Err(anyhow!(
                "sampled key input has no live connection-generation authority"
            ));
        }
        request
            .validate()
            .context("validating sampled key input context")?;
        Ok(Self {
            context: request.trace_context,
            stream_id,
            pane_id: request.request.pane_id,
            input_serial: request.request.input_serial,
            recorder_token: None,
            topology: None,
            dispatch_queued_at: None,
        })
    }

    fn validate_for_request(
        &self,
        request: &SendKeyDownTracedV1,
        current_stream_id: TopologyStreamId,
    ) -> anyhow::Result<()> {
        request
            .validate()
            .context("revalidating sampled key input context")?;
        if self.stream_id != current_stream_id {
            return Err(anyhow!(
                "sampled key input belongs to a stale connection generation"
            ));
        }
        if self.context != request.trace_context {
            return Err(anyhow!(
                "sampled key input context differs from its admission authority"
            ));
        }
        if self.pane_id != request.request.pane_id
            || self.input_serial != request.request.input_serial
        {
            return Err(anyhow!(
                "sampled key input request identity differs from its admission authority"
            ));
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn context(&self) -> SampledTraceContextV1 {
        self.context
    }

    #[must_use]
    pub(crate) const fn stream_id(&self) -> TopologyStreamId {
        self.stream_id
    }
}

/// Process-local authority for one exact mux-server connection.
///
/// Clones retain only the right to *attempt* an operation. Once the owning
/// [`SessionHandler`] retires the incarnation, deferred clones fail closed and
/// cannot redirect work through a replacement process-global mux.
#[derive(Clone)]
pub(crate) struct SessionAuthority {
    mux: Weak<Mux>,
    incarnation: Arc<SessionIncarnation>,
}

impl SessionAuthority {
    pub(crate) fn new(mux: &Arc<Mux>) -> Self {
        Self {
            mux: Arc::downgrade(mux),
            incarnation: Arc::new(SessionIncarnation::new()),
        }
    }

    fn acquire(&self) -> anyhow::Result<CurrentSession> {
        let operation = self
            .incarnation
            .try_acquire()
            .ok_or_else(|| anyhow!("mux server session is retired"))?;
        let mux = self
            .mux
            .upgrade()
            .ok_or_else(|| anyhow!("mux server session owner no longer exists"))?;
        Ok(CurrentSession {
            mux,
            _operation: operation,
        })
    }

    fn capture_current_pane(&self, pane_id: PaneId) -> anyhow::Result<PaneRegistrationHandle> {
        self.capture_current_pane_opt(pane_id)?
            .ok_or_else(|| anyhow!("no such pane {pane_id}"))
    }

    fn capture_current_pane_opt(
        &self,
        pane_id: PaneId,
    ) -> anyhow::Result<Option<PaneRegistrationHandle>> {
        let session = self.acquire()?;
        Ok(session.capture_current_pane(pane_id))
    }

    fn retire(&self) {
        self.incarnation.retire();
    }

    pub(crate) fn try_run<R>(&self, f: impl FnOnce() -> R) -> anyhow::Result<R> {
        let operation = self
            .incarnation
            .try_acquire()
            .ok_or_else(|| anyhow!("mux server session is retired"))?;
        let result = f();
        drop(operation);
        Ok(result)
    }
}

struct CurrentSession {
    mux: Arc<Mux>,
    _operation: SessionOperationLease,
}

impl Deref for CurrentSession {
    type Target = Mux;

    fn deref(&self) -> &Self::Target {
        &self.mux
    }
}

impl CurrentSession {
    fn mux(&self) -> &Arc<Mux> {
        &self.mux
    }
}

/// Unique strong owner for one mux-server connection.
///
/// Cloneable deferred work receives only [`SessionAuthority`], which contains
/// weak mux authority. This owner is therefore the sole connection-lifetime
/// strong reference and retires the incarnation before releasing the mux.
pub(crate) struct SessionOwner {
    mux: Arc<Mux>,
    authority: SessionAuthority,
}

impl SessionOwner {
    pub(crate) fn new(mux: Arc<Mux>) -> Self {
        let authority = SessionAuthority::new(&mux);
        Self { mux, authority }
    }

    pub(crate) fn mux(&self) -> &Arc<Mux> {
        &self.mux
    }

    pub(crate) fn authority(&self) -> SessionAuthority {
        self.authority.clone()
    }

    fn retire(&self) {
        self.authority.retire();
    }
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        self.retire();
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct PaneRenderBaseline {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
    mouse_grabbed: bool,
    alt_screen_active: bool,
    sent_initial_palette: bool,
    seqno: SequenceNo,
    config_generation: usize,
    committed_input_epoch: u64,
}

/// Local ownership identity for an immutable render candidate.
///
/// This is not delivery, scheduler, wire, or application-ACK authority. Live
/// integration under ft-interactive-systems-performance-4tenz.5.5.10 must
/// bind each candidate one-to-one to the exact `DeliveryClaim` and
/// coordinator claim from `ft.render-delivery-ledger.v1`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PaneRenderAttemptToken {
    pane_id: PaneId,
    render_generation: u64,
    attempt: u64,
    input_epoch: Option<u64>,
}

#[derive(Clone, Debug)]
struct PendingPaneRenderCommit {
    token: PaneRenderAttemptToken,
    baseline: PaneRenderBaseline,
    covered_notifications: Vec<Alert>,
}

/// Maximum alert backlog retained for one pane while its client is stalled.
///
/// One complete wire-sized prefix may be protected by an in-flight render
/// application while one further wire-sized suffix absorbs newer events.  The
/// suffix coalesces only replaceable state. Exact-event obligations are never
/// evicted; an unretainable event is rejected so dispatch can fail the affected
/// connection closed. The protected prefix is never mutated before its exact
/// application ACK or NACK.
const MAX_PENDING_PANE_ALERTS: usize = codec::MAX_RENDER_APPLICATION_ALERTS * 2;
const MAX_PENDING_PANE_ALERT_TEXT_BYTES: usize = codec::MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES * 2;

fn pane_alert_text_bytes(alert: &Alert) -> Option<usize> {
    match alert {
        Alert::ToastNotification { title, body, .. } => title
            .as_ref()
            .map_or(0, String::len)
            .checked_add(body.len()),
        Alert::IconTitleChanged(title) | Alert::TabTitleChanged(title) => {
            Some(title.as_ref().map_or(0, String::len))
        }
        Alert::WindowTitleChanged(title) => Some(title.len()),
        Alert::SetUserVar { name, value } => name.len().checked_add(value.len()),
        Alert::SetProfileRequested { name } => Some(name.len()),
        Alert::MouseShapeRequested { shape } => Some(shape.len()),
        Alert::ImageAltText { text, .. } => Some(text.len()),
        Alert::Bell
        | Alert::CurrentWorkingDirectoryChanged
        | Alert::PaletteChanged
        | Alert::OutputSinceFocusLost
        | Alert::Progress(_) => Some(0),
    }
}

fn pane_alerts_coalesce(existing: &Alert, incoming: &Alert) -> bool {
    matches!(
        (existing, incoming),
        (
            Alert::CurrentWorkingDirectoryChanged,
            Alert::CurrentWorkingDirectoryChanged
        ) | (Alert::PaletteChanged, Alert::PaletteChanged)
            | (Alert::OutputSinceFocusLost, Alert::OutputSinceFocusLost)
            | (Alert::Progress(_), Alert::Progress(_))
            | (Alert::IconTitleChanged(_), Alert::IconTitleChanged(_))
            | (Alert::WindowTitleChanged(_), Alert::WindowTitleChanged(_))
            | (Alert::TabTitleChanged(_), Alert::TabTitleChanged(_))
            | (
                Alert::MouseShapeRequested { .. },
                Alert::MouseShapeRequested { .. }
            )
    )
}

fn pane_alert_is_exact_event(alert: &Alert) -> bool {
    matches!(
        alert,
        Alert::Bell
            | Alert::ToastNotification { .. }
            | Alert::SetUserVar { .. }
            | Alert::SetProfileRequested { .. }
            | Alert::ImageAltText { .. }
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PaneAlertBacklogError {
    TextAccountingOverflow,
    AccountingDrift,
    CapacityInvariantExceeded,
    ProtectedPrefixInvalid,
    SingleAlertTextLimit,
    ExactEventCapacityExhausted,
    StateCapacityExhausted,
}

impl std::fmt::Display for PaneAlertBacklogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TextAccountingOverflow => "pane alert text accounting overflowed",
            Self::AccountingDrift => "pane alert retained-byte accounting drifted",
            Self::CapacityInvariantExceeded => "pane alert retained capacity invariant exceeded",
            Self::ProtectedPrefixInvalid => "pane alert protected prefix is invalid",
            Self::SingleAlertTextLimit => "pane alert exceeds the per-application text limit",
            Self::ExactEventCapacityExhausted => {
                "pane exact-event alert backlog capacity is exhausted"
            }
            Self::StateCapacityExhausted => "pane state alert backlog capacity is exhausted",
        })
    }
}

impl std::error::Error for PaneAlertBacklogError {}

#[derive(Debug, Default)]
pub(crate) struct PendingPaneAlerts {
    entries: Vec<Alert>,
    retained_text_bytes: usize,
    protected_prefix_len: usize,
}

impl PendingPaneAlerts {
    fn checked_text_bytes(entries: &[Alert]) -> Option<usize> {
        entries.iter().try_fold(0usize, |total, alert| {
            pane_alert_text_bytes(alert).and_then(|bytes| total.checked_add(bytes))
        })
    }

    fn validate_accounting(&self) -> Result<(), PaneAlertBacklogError> {
        // Check the hard retained capacities before the exact accounting scan;
        // this makes every validation O(n) with n provably bounded at 128.
        if self.entries.len() > MAX_PENDING_PANE_ALERTS
            || self.retained_text_bytes > MAX_PENDING_PANE_ALERT_TEXT_BYTES
        {
            return Err(PaneAlertBacklogError::CapacityInvariantExceeded);
        }
        if self.protected_prefix_len > self.entries.len()
            || self.protected_prefix_len > codec::MAX_RENDER_APPLICATION_ALERTS
        {
            return Err(PaneAlertBacklogError::ProtectedPrefixInvalid);
        }
        let exact = Self::checked_text_bytes(&self.entries)
            .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
        if exact != self.retained_text_bytes {
            return Err(PaneAlertBacklogError::AccountingDrift);
        }
        Ok(())
    }

    pub(crate) fn push(&mut self, alert: Alert) -> Result<(), PaneAlertBacklogError> {
        self.validate_accounting()?;
        let incoming_bytes =
            pane_alert_text_bytes(&alert).ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
        if incoming_bytes > codec::MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES {
            metrics::counter!(
                "mux.server.pane_alert_backlog_rejected",
                "reason" => "single_alert_text_limit"
            )
            .increment(1);
            return Err(PaneAlertBacklogError::SingleAlertTextLimit);
        }

        // The validation, coalescing scan, and stable retain below are O(n),
        // intentionally bounded by MAX_PENDING_PANE_ALERTS (currently 128).
        // Appending at the end preserves the new state's true temporal
        // position relative to exact-event obligations.
        let mut coalesced_count = 0usize;
        let mut replaced_bytes = 0usize;
        for existing in &self.entries[self.protected_prefix_len..] {
            if pane_alerts_coalesce(existing, &alert) {
                coalesced_count = coalesced_count
                    .checked_add(1)
                    .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
                replaced_bytes = replaced_bytes
                    .checked_add(
                        pane_alert_text_bytes(existing)
                            .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?,
                    )
                    .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
            }
        }
        if coalesced_count != 0 {
            let retained_text_bytes = self
                .retained_text_bytes
                .checked_sub(replaced_bytes)
                .and_then(|bytes| bytes.checked_add(incoming_bytes))
                .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
            if retained_text_bytes > MAX_PENDING_PANE_ALERT_TEXT_BYTES {
                metrics::counter!(
                    "mux.server.pane_alert_backlog_rejected",
                    "reason" => "state_capacity"
                )
                .increment(1);
                return Err(PaneAlertBacklogError::StateCapacityExhausted);
            }
            let protected_prefix_len = self.protected_prefix_len;
            let mut index = 0usize;
            self.entries.retain(|existing| {
                let retain =
                    index < protected_prefix_len || !pane_alerts_coalesce(existing, &alert);
                index += 1;
                retain
            });
            self.entries.push(alert);
            self.retained_text_bytes = retained_text_bytes;
            metrics::counter!("mux.server.pane_alert_backlog_coalesced").increment(1);
            debug_assert!(self.validate_accounting().is_ok());
            return Ok(());
        }

        let retained_text_bytes = self
            .retained_text_bytes
            .checked_add(incoming_bytes)
            .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
        if self.entries.len() >= MAX_PENDING_PANE_ALERTS
            || retained_text_bytes > MAX_PENDING_PANE_ALERT_TEXT_BYTES
        {
            let (error, reason) = if pane_alert_is_exact_event(&alert) {
                (
                    PaneAlertBacklogError::ExactEventCapacityExhausted,
                    "exact_event_capacity",
                )
            } else {
                (
                    PaneAlertBacklogError::StateCapacityExhausted,
                    "state_capacity",
                )
            };
            metrics::counter!(
                "mux.server.pane_alert_backlog_rejected",
                "reason" => reason
            )
            .increment(1);
            return Err(error);
        }

        self.retained_text_bytes = retained_text_bytes;
        self.entries.push(alert);
        debug_assert!(self.validate_accounting().is_ok());
        Ok(())
    }

    fn protect_prefix(&mut self, len: usize) {
        debug_assert!(len <= self.entries.len());
        debug_assert!(len <= codec::MAX_RENDER_APPLICATION_ALERTS);
        self.protected_prefix_len = len.min(self.entries.len());
    }

    fn wire_prefix_len_up_to(&self, max_len: usize) -> Result<usize, PaneAlertBacklogError> {
        self.validate_accounting()?;
        let mut retained_bytes = 0usize;
        let mut len = 0usize;
        for alert in self
            .entries
            .iter()
            .take(max_len.min(codec::MAX_RENDER_APPLICATION_ALERTS))
        {
            let alert_bytes = pane_alert_text_bytes(alert)
                .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
            let next_bytes = retained_bytes
                .checked_add(alert_bytes)
                .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
            if next_bytes > codec::MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES {
                break;
            }
            retained_bytes = next_bytes;
            len += 1;
        }
        Ok(len)
    }

    fn clear_protection(&mut self) {
        self.protected_prefix_len = 0;
    }

    fn drain_prefix(&mut self, len: usize) -> Result<(), PaneAlertBacklogError> {
        self.validate_accounting()?;
        if len > self.entries.len() || len > self.protected_prefix_len {
            return Err(PaneAlertBacklogError::ProtectedPrefixInvalid);
        }
        let removed_bytes = Self::checked_text_bytes(&self.entries[..len])
            .ok_or(PaneAlertBacklogError::TextAccountingOverflow)?;
        let retained_text_bytes = self
            .retained_text_bytes
            .checked_sub(removed_bytes)
            .ok_or(PaneAlertBacklogError::AccountingDrift)?;
        self.entries.drain(..len);
        self.retained_text_bytes = retained_text_bytes;
        self.protected_prefix_len -= len;
        debug_assert!(self.validate_accounting().is_ok());
        Ok(())
    }

    fn protected_batch_up_to(
        &mut self,
        max_len: usize,
    ) -> Result<Vec<Alert>, PaneAlertBacklogError> {
        let len = self.wire_prefix_len_up_to(max_len)?;
        if self.protected_prefix_len != 0 {
            return Err(PaneAlertBacklogError::ProtectedPrefixInvalid);
        }
        let batch = self.entries[..len].to_vec();
        self.protect_prefix(len);
        Ok(batch)
    }

    fn acknowledge_protected_front(
        &mut self,
        expected: &Alert,
    ) -> Result<(), PaneAlertBacklogError> {
        self.validate_accounting()?;
        if self.protected_prefix_len == 0 || self.entries.first() != Some(expected) {
            return Err(PaneAlertBacklogError::ProtectedPrefixInvalid);
        }
        self.drain_prefix(1)
    }

    fn release_protected_prefix(
        &mut self,
        expected: &[Alert],
    ) -> Result<(), PaneAlertBacklogError> {
        self.validate_accounting()?;
        if self.protected_prefix_len != expected.len()
            || self.entries.get(..expected.len()) != Some(expected)
        {
            return Err(PaneAlertBacklogError::ProtectedPrefixInvalid);
        }
        self.clear_protection();
        Ok(())
    }

    fn as_slice(&self) -> &[Alert] {
        &self.entries
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl PartialEq<Vec<Alert>> for PendingPaneAlerts {
    fn eq(&self, other: &Vec<Alert>) -> bool {
        self.entries == *other
    }
}

#[derive(Debug, Default)]
enum PaneRenderTransactionPhase {
    #[default]
    Idle,
    Preparing {
        token: PaneRenderAttemptToken,
        redirtied: bool,
    },
    InFlight {
        pending: Box<PendingPaneRenderCommit>,
        redirtied: bool,
    },
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LegacyRenderEnqueuePhase {
    #[default]
    Idle,
    /// A speculative surface baseline owns this exact revision until enqueue
    /// admission is acknowledged or rolled back.
    InFlight {
        installed_revision: u64,
    },
    /// A protected alert prefix owns delivery until every admitted element is
    /// acknowledged or the undelivered suffix is released.
    NotificationsInFlight,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneRenderSettlement {
    AcknowledgedClean,
    AcknowledgedRedirtied,
    SettledNoChangeClean,
    SettledNoChangeRedirtied,
    Retried,
    StaleOrDuplicate,
    Closed,
    FailedClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneRenderPreparationError {
    Busy,
    Closed,
    AttemptIdentityExhausted,
    InputIdentityExhausted,
    SourceChanged,
    TerminalSequenceExhausted,
    StableRowRangeUnrepresentable,
    LegacyBaselineSuperseded,
    LegacyAttemptIdentityExhausted,
    LegacyDeliveryAuthorityChanged,
    StaleAttempt,
    NotificationPrefixChanged,
    NotificationBacklogInvalid,
    SnapshotFailed,
    StateLockPoisoned,
}

impl std::fmt::Display for PaneRenderPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "another pane render preparation is already active",
            Self::Closed => "pane render transaction state is closed",
            Self::AttemptIdentityExhausted => {
                "pane render attempt identity exhausted before wrap or reuse"
            }
            Self::InputIdentityExhausted => {
                "pane render input identity exhausted before wrap or reuse"
            }
            Self::SourceChanged => "pane render source changed during snapshot preparation",
            Self::TerminalSequenceExhausted => {
                "terminal sequence saturated; a fresh render generation is required"
            }
            Self::StableRowRangeUnrepresentable => {
                "pane stable-row range cannot be represented without overflow"
            }
            Self::LegacyBaselineSuperseded => {
                "pane legacy render baseline changed during lock-free preparation"
            }
            Self::LegacyAttemptIdentityExhausted => {
                "pane legacy render attempt identity exhausted before wrap or reuse"
            }
            Self::LegacyDeliveryAuthorityChanged => {
                "pane legacy delivery ownership changed before settlement"
            }
            Self::StaleAttempt => "pane render attempt no longer owns the transaction",
            Self::NotificationPrefixChanged => {
                "pane notification prefix changed during transaction preparation"
            }
            Self::NotificationBacklogInvalid => {
                "pane notification backlog accounting or capacity is invalid"
            }
            Self::SnapshotFailed => "pane semantic snapshot preparation failed",
            Self::StateLockPoisoned => "pane render transaction state lock is poisoned",
        })
    }
}

impl std::error::Error for PaneRenderPreparationError {}

#[derive(Debug)]
pub(crate) struct PerPane {
    baseline: PaneRenderBaseline,
    // Candidate ownership protects the baseline and retained effects. It is
    // deliberately subordinate to the delivery ledger and must never become
    // an independent source of scheduling or convergence truth.
    transaction_phase: PaneRenderTransactionPhase,
    // Shadow observation used to prove candidate redirty behavior before live
    // ledger wiring. The authoritative durable dirty bit remains in
    // `DeliveryLedger`.
    transactional_dirty: bool,
    render_generation: u64,
    next_render_attempt: Option<u64>,
    next_input_epoch: Option<u64>,
    baseline_revision: u64,
    legacy_enqueue_phase: LegacyRenderEnqueuePhase,
    #[cfg(test)]
    panic_next_legacy_enqueue_ack: bool,
    #[cfg(test)]
    panic_next_legacy_enqueue_recovery: bool,
    pub(crate) notifications: PendingPaneAlerts,
}

impl Default for PerPane {
    fn default() -> Self {
        Self {
            baseline: PaneRenderBaseline::default(),
            transaction_phase: PaneRenderTransactionPhase::Idle,
            transactional_dirty: true,
            render_generation: 1,
            next_render_attempt: Some(1),
            next_input_epoch: Some(1),
            baseline_revision: 0,
            legacy_enqueue_phase: LegacyRenderEnqueuePhase::Idle,
            #[cfg(test)]
            panic_next_legacy_enqueue_ack: false,
            #[cfg(test)]
            panic_next_legacy_enqueue_recovery: false,
            notifications: PendingPaneAlerts::default(),
        }
    }
}

#[derive(Clone)]
struct TrackedPane {
    registration: Option<PaneRegistrationHandle>,
    state: Arc<Mutex<PerPane>>,
}

impl TrackedPane {
    fn exact(registration: PaneRegistrationHandle) -> Self {
        Self {
            registration: Some(registration),
            state: Arc::new(Mutex::new(PerPane::default())),
        }
    }
}

fn stable_row_offset(row: StableRowIndex, offset: usize) -> Option<StableRowIndex> {
    let offset = StableRowIndex::try_from(offset).ok()?;
    row.checked_add(offset)
}

fn stable_row_range_from_len(
    start: StableRowIndex,
    len: usize,
) -> Option<std::ops::Range<StableRowIndex>> {
    let end = stable_row_offset(start, len)?;
    Some(start..end)
}

#[cfg(test)]
fn stable_row_range_from_signed_len(
    start: StableRowIndex,
    len: StableRowIndex,
) -> Option<std::ops::Range<StableRowIndex>> {
    if len < 0 {
        return None;
    }

    let len = usize::try_from(len).ok()?;
    stable_row_range_from_len(start, len)
}

#[derive(Clone, Debug)]
struct PreparedSurfaceChanges {
    response: GetPaneRenderChangesResponse,
    baseline: PaneRenderBaseline,
    source_start: SequenceNo,
    source_query: SequenceNo,
    source_end: SequenceNo,
}

#[derive(Clone, Debug)]
enum SurfacePreparation {
    NoChange {
        source_start: SequenceNo,
        source_query: SequenceNo,
        source_end: SequenceNo,
    },
    Changes(Box<PreparedSurfaceChanges>),
    StableRowRangeUnrepresentable,
}

impl PaneRenderBaseline {
    fn prepare_surface_changes(
        &self,
        pane: &CurrentPane<'_>,
        force_with_input_dispatch_serial: Option<InputSerial>,
        force_for_atomic_effects: bool,
    ) -> SurfacePreparation {
        let source_start = pane.get_current_seqno();
        let mut changed = false;
        let mouse_grabbed = pane.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }
        let alt_screen_active = pane.is_alt_screen_active();
        if alt_screen_active != self.alt_screen_active {
            changed = true;
        }

        let dims = pane.get_dimensions();
        if dims != self.dimensions {
            changed = true;
        }
        let tiered_scrollback_status = pane.get_tiered_scrollback_status();
        if tiered_scrollback_status != self.tiered_scrollback_status {
            changed = true;
        }

        let cursor_position = pane.get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = pane.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        if working_dir != self.working_dir {
            changed = true;
        }

        let Some(viewport_range) = stable_row_range_from_len(dims.physical_top, dims.viewport_rows)
        else {
            return SurfacePreparation::StableRowRangeUnrepresentable;
        };
        // Capture the query fence and derive the regression/MAX-safe baseline
        // under the backend's terminal/cache lock where it has one.
        let (source_query, mut all_dirty_lines) =
            pane.get_changed_since_with_source_fence(viewport_range.clone(), self.seqno);
        if !all_dirty_lines.is_empty() {
            changed = true;
        }

        if !changed && force_with_input_dispatch_serial.is_none() && !force_for_atomic_effects {
            return SurfacePreparation::NoChange {
                source_start,
                source_query,
                source_end: pane.get_current_seqno(),
            };
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let (first_line, lines) = pane.get_lines(viewport_range);
        if stable_row_range_from_len(first_line, lines.len()).is_none() {
            return SurfacePreparation::StableRowRangeUnrepresentable;
        }
        let mut bonus_lines = Vec::new();
        for (idx, mut line) in lines.into_iter().enumerate() {
            let Some(stable_row) = stable_row_offset(first_line, idx) else {
                return SurfacePreparation::StableRowRangeUnrepresentable;
            };
            if all_dirty_lines.contains(stable_row) {
                all_dirty_lines.remove(stable_row);
                line.compress_for_scrollback();
                bonus_lines.push((stable_row, line));
            }
        }

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let Some(cursor_range) = stable_row_range_from_len(cursor_position.y, 1) else {
            return SurfacePreparation::StableRowRangeUnrepresentable;
        };
        let (cursor_line_idx, lines) = pane.get_lines(cursor_range);
        if stable_row_range_from_len(cursor_line_idx, lines.len()).is_none() {
            return SurfacePreparation::StableRowRangeUnrepresentable;
        }
        if let Some(mut cursor_line) = lines.into_iter().next() {
            cursor_line.compress_for_scrollback();
            if let Err(insertion_idx) =
                bonus_lines.binary_search_by_key(&cursor_line_idx, |(stable_row, _)| *stable_row)
            {
                // Preserve the stable-row ordering established by the viewport
                // walk even for a defensive backend whose reported cursor row
                // falls before that viewport.  Keeping this vector ordered
                // makes the binary-search dedupe itself valid on later edits
                // and avoids handing downstream consumers a shape that only
                // happens to be ordered for ordinary terminal geometry.
                bonus_lines.insert(insertion_idx, (cursor_line_idx, cursor_line));
            }
        }

        let source_end = pane.get_current_seqno();
        let mut baseline = self.clone();
        baseline.cursor_position = cursor_position;
        baseline.title.clone_from(&title);
        baseline.working_dir.clone_from(&working_dir);
        baseline.dimensions = dims;
        baseline.tiered_scrollback_status = tiered_scrollback_status;
        baseline.mouse_grabbed = mouse_grabbed;
        baseline.alt_screen_active = alt_screen_active;
        baseline.seqno = source_query;

        let bonus_lines = bonus_lines.into();
        SurfacePreparation::Changes(Box::new(PreparedSurfaceChanges {
            response: GetPaneRenderChangesResponse {
                pane_id: pane.pane_id(),
                mouse_grabbed,
                alt_screen_active,
                dirty_lines: all_dirty_lines.iter().cloned().collect(),
                dimensions: dims,
                tiered_scrollback_status,
                cursor_position,
                title,
                bonus_lines,
                working_dir: working_dir.map(Into::into),
                input_serial: force_with_input_dispatch_serial,
                seqno: source_query,
            },
            baseline,
            source_start,
            source_query,
            source_end,
        }))
    }
}

impl PerPane {
    #[cfg(test)]
    fn compute_changes(
        &mut self,
        pane: &CurrentPane<'_>,
        force_with_input_dispatch_serial: Option<InputSerial>,
    ) -> Result<Option<GetPaneRenderChangesResponse>, PaneRenderPreparationError> {
        match self
            .baseline
            .prepare_surface_changes(pane, force_with_input_dispatch_serial, false)
        {
            SurfacePreparation::StableRowRangeUnrepresentable => {
                self.mark_transactional_dirty();
                Err(PaneRenderPreparationError::StableRowRangeUnrepresentable)
            }
            SurfacePreparation::NoChange { source_query, .. } => {
                // The legacy transport has no application ACK. Preserve its
                // established behavior and avoid rescanning an ever-growing
                // no-visible-change interval while the transactional path
                // remains dormant.
                self.baseline.seqno = source_query;
                Ok(None)
            }
            SurfacePreparation::Changes(prepared) => {
                let PreparedSurfaceChanges {
                    response, baseline, ..
                } = *prepared;
                self.baseline = baseline;
                Ok(Some(response))
            }
        }
    }
}

/// Prepare a legacy render without holding the per-pane state lock across any
/// pane callback, then publish its speculative baseline only if the exact
/// baseline sampled before preparation is still current.
fn prepare_legacy_render_enqueue(
    pane: &CurrentPane<'_>,
    per_pane: &Arc<Mutex<PerPane>>,
    force_with_input_dispatch_serial: Option<InputSerial>,
) -> anyhow::Result<Option<(GetPaneRenderChangesResponse, LegacyRenderEnqueueGuard)>> {
    let pane_id = pane.pane_id();
    let (prior_baseline, prior_revision) = {
        let mut state = lock_per_pane_or_retire(per_pane, "preparing the legacy render baseline")?;
        state.ensure_legacy_transport_idle()?;
        (state.baseline.clone(), state.baseline_revision)
    };
    let surface =
        prior_baseline.prepare_surface_changes(pane, force_with_input_dispatch_serial, false);

    let mut state = lock_per_pane_or_retire(per_pane, "publishing the legacy render baseline")?;
    state.ensure_legacy_transport_idle()?;
    if state.baseline != prior_baseline || state.baseline_revision != prior_revision {
        state.mark_transactional_dirty();
        return Err(PaneRenderPreparationError::LegacyBaselineSuperseded.into());
    }

    let source_fence = match &surface {
        SurfacePreparation::NoChange {
            source_start,
            source_query,
            source_end,
        } => Some((*source_start, *source_query, *source_end)),
        SurfacePreparation::Changes(prepared) => Some((
            prepared.source_start,
            prepared.source_query,
            prepared.source_end,
        )),
        SurfacePreparation::StableRowRangeUnrepresentable => None,
    };
    if let Some((source_start, source_query, source_end)) = source_fence {
        if source_start == SequenceNo::MAX
            || source_query == SequenceNo::MAX
            || source_end == SequenceNo::MAX
        {
            state.retire_render_authority();
            return Err(PaneRenderPreparationError::TerminalSequenceExhausted.into());
        }
        if source_start != source_query || source_query != source_end {
            state.mark_transactional_dirty();
            return Err(PaneRenderPreparationError::SourceChanged.into());
        }
    }

    match surface {
        SurfacePreparation::StableRowRangeUnrepresentable => {
            state.mark_transactional_dirty();
            Err(PaneRenderPreparationError::StableRowRangeUnrepresentable.into())
        }
        SurfacePreparation::NoChange { source_query, .. } => {
            if state.baseline.seqno != source_query {
                let Some(next_revision) = prior_revision.checked_add(1) else {
                    state.retire_render_authority();
                    return Err(PaneRenderPreparationError::LegacyAttemptIdentityExhausted.into());
                };
                state.baseline.seqno = source_query;
                state.baseline_revision = next_revision;
            }
            Ok(None)
        }
        SurfacePreparation::Changes(prepared) => {
            let PreparedSurfaceChanges {
                response, baseline, ..
            } = *prepared;
            let Some(installed_revision) = prior_revision.checked_add(1) else {
                state.retire_render_authority();
                return Err(PaneRenderPreparationError::LegacyAttemptIdentityExhausted.into());
            };
            state.baseline = baseline;
            state.baseline_revision = installed_revision;
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::InFlight { installed_revision };
            drop(state);
            let rollback = LegacyRenderEnqueueGuard::new(
                Arc::clone(per_pane),
                pane_id,
                prior_baseline,
                installed_revision,
            );
            Ok(Some((response, rollback)))
        }
    }
}

#[derive(Clone, Debug)]
#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
struct PaneRenderBeginSnapshot {
    token: PaneRenderAttemptToken,
    baseline: PaneRenderBaseline,
    covered_notifications: Vec<Alert>,
    has_uncovered_notifications: bool,
}

#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
impl PerPane {
    fn ensure_legacy_transport_idle(&mut self) -> Result<(), PaneRenderPreparationError> {
        match self.legacy_enqueue_phase {
            LegacyRenderEnqueuePhase::Idle => {}
            LegacyRenderEnqueuePhase::InFlight { .. }
            | LegacyRenderEnqueuePhase::NotificationsInFlight => {
                self.mark_transactional_dirty();
                return Err(PaneRenderPreparationError::Busy);
            }
            LegacyRenderEnqueuePhase::Closed => {
                return Err(PaneRenderPreparationError::Closed);
            }
        }
        match self.transaction_phase {
            PaneRenderTransactionPhase::Idle => {
                if self.notifications.protected_prefix_len == 0 {
                    Ok(())
                } else {
                    // No transaction can own an alert prefix in Idle. Clear
                    // only that stale local marker, retain every alert, and
                    // require a fresh preparation rather than draining data
                    // under contradictory authority.
                    self.notifications.clear_protection();
                    self.transactional_dirty = true;
                    Err(PaneRenderPreparationError::NotificationPrefixChanged)
                }
            }
            PaneRenderTransactionPhase::Preparing { .. }
            | PaneRenderTransactionPhase::InFlight { .. } => {
                self.mark_transactional_dirty();
                Err(PaneRenderPreparationError::Busy)
            }
            PaneRenderTransactionPhase::Closed => Err(PaneRenderPreparationError::Closed),
        }
    }

    fn mark_transactional_dirty(&mut self) {
        self.transactional_dirty = true;
        match &mut self.transaction_phase {
            PaneRenderTransactionPhase::Preparing { redirtied, .. }
            | PaneRenderTransactionPhase::InFlight { redirtied, .. } => {
                *redirtied = true;
            }
            PaneRenderTransactionPhase::Idle | PaneRenderTransactionPhase::Closed => {}
        }
    }

    /// Permanently retire every render-settlement authority for this exact
    /// pane registration. Once delivery may have happened but local settlement
    /// is unknown, neither the speculative baseline nor retained alerts may be
    /// exposed to a successor attempt without risking omission or duplication.
    pub(crate) fn retire_render_authority(&mut self) {
        self.transaction_phase = PaneRenderTransactionPhase::Closed;
        self.legacy_enqueue_phase = LegacyRenderEnqueuePhase::Closed;
        self.notifications.clear_protection();
        self.transactional_dirty = true;
    }

    pub(crate) fn push_notification(&mut self, alert: Alert) -> Result<(), PaneAlertBacklogError> {
        self.notifications.push(alert)?;
        self.mark_transactional_dirty();
        Ok(())
    }

    fn begin_transactional_preparation(
        &mut self,
        pane_id: PaneId,
        force_with_input_dispatch_serial: Option<InputSerial>,
    ) -> Result<PaneRenderBeginSnapshot, PaneRenderPreparationError> {
        match self.legacy_enqueue_phase {
            LegacyRenderEnqueuePhase::Idle => {}
            LegacyRenderEnqueuePhase::InFlight { .. }
            | LegacyRenderEnqueuePhase::NotificationsInFlight => {
                self.mark_transactional_dirty();
                return Err(PaneRenderPreparationError::Busy);
            }
            LegacyRenderEnqueuePhase::Closed => {
                return Err(PaneRenderPreparationError::Closed);
            }
        }
        match self.transaction_phase {
            PaneRenderTransactionPhase::Idle => {}
            PaneRenderTransactionPhase::Preparing { .. }
            | PaneRenderTransactionPhase::InFlight { .. } => {
                self.mark_transactional_dirty();
                return Err(PaneRenderPreparationError::Busy);
            }
            PaneRenderTransactionPhase::Closed => {
                return Err(PaneRenderPreparationError::Closed);
            }
        }
        if self.notifications.protected_prefix_len != 0 {
            self.close_transaction();
            return Err(PaneRenderPreparationError::NotificationPrefixChanged);
        }

        let attempt = self
            .next_render_attempt
            .and_then(|attempt| attempt.checked_add(1).map(|next| (attempt, next)));
        let Some((attempt, next_attempt)) = attempt else {
            self.close_transaction();
            return Err(PaneRenderPreparationError::AttemptIdentityExhausted);
        };

        let input_epoch = if force_with_input_dispatch_serial.is_some() {
            let next = self
                .next_input_epoch
                .and_then(|epoch| epoch.checked_add(1).map(|next| (epoch, next)));
            let Some((epoch, next_epoch)) = next else {
                self.close_transaction();
                return Err(PaneRenderPreparationError::InputIdentityExhausted);
            };
            self.next_input_epoch = Some(next_epoch);
            Some(epoch)
        } else {
            None
        };

        self.next_render_attempt = Some(next_attempt);
        let token = PaneRenderAttemptToken {
            pane_id,
            render_generation: self.render_generation,
            attempt,
            input_epoch,
        };
        let covered_notifications_len = match self
            .notifications
            .wire_prefix_len_up_to(self.notifications.len())
        {
            Ok(len) => len,
            Err(_) => {
                self.close_transaction();
                return Err(PaneRenderPreparationError::NotificationBacklogInvalid);
            }
        };
        let covered_notifications = self
            .notifications
            .as_slice()
            .iter()
            .take(covered_notifications_len)
            .cloned()
            .collect::<Vec<_>>();
        let has_uncovered_notifications = covered_notifications.len() < self.notifications.len();
        self.transactional_dirty = false;
        self.transaction_phase = PaneRenderTransactionPhase::Preparing {
            token,
            redirtied: false,
        };
        self.notifications
            .protect_prefix(covered_notifications.len());
        Ok(PaneRenderBeginSnapshot {
            token,
            baseline: self.baseline.clone(),
            covered_notifications,
            has_uncovered_notifications,
        })
    }

    fn cancel_preparation(&mut self, token: PaneRenderAttemptToken) -> PaneRenderSettlement {
        let matches = matches!(
            self.transaction_phase,
            PaneRenderTransactionPhase::Preparing {
                token: active,
                ..
            } if active == token
        );
        if !matches {
            return if matches!(self.transaction_phase, PaneRenderTransactionPhase::Closed) {
                PaneRenderSettlement::Closed
            } else {
                PaneRenderSettlement::StaleOrDuplicate
            };
        }
        self.transaction_phase = PaneRenderTransactionPhase::Idle;
        self.notifications.clear_protection();
        self.transactional_dirty = true;
        PaneRenderSettlement::Retried
    }

    fn close_exhausted_preparation(
        &mut self,
        token: PaneRenderAttemptToken,
    ) -> PaneRenderSettlement {
        let matches = matches!(
            self.transaction_phase,
            PaneRenderTransactionPhase::Preparing {
                token: active,
                ..
            } if active == token
        );
        if !matches {
            return if matches!(self.transaction_phase, PaneRenderTransactionPhase::Closed) {
                PaneRenderSettlement::Closed
            } else {
                PaneRenderSettlement::StaleOrDuplicate
            };
        }
        self.close_transaction();
        PaneRenderSettlement::Closed
    }

    fn settle_no_change(&mut self, token: PaneRenderAttemptToken) -> PaneRenderSettlement {
        let phase = std::mem::replace(
            &mut self.transaction_phase,
            PaneRenderTransactionPhase::Closed,
        );
        let PaneRenderTransactionPhase::Preparing {
            token: active,
            redirtied,
        } = phase
        else {
            self.transaction_phase = phase;
            return if matches!(self.transaction_phase, PaneRenderTransactionPhase::Closed) {
                PaneRenderSettlement::Closed
            } else {
                PaneRenderSettlement::StaleOrDuplicate
            };
        };
        if active != token {
            self.transaction_phase = PaneRenderTransactionPhase::Preparing {
                token: active,
                redirtied,
            };
            return PaneRenderSettlement::StaleOrDuplicate;
        }

        self.transaction_phase = PaneRenderTransactionPhase::Idle;
        self.notifications.clear_protection();
        self.transactional_dirty = redirtied;
        if redirtied {
            PaneRenderSettlement::SettledNoChangeRedirtied
        } else {
            PaneRenderSettlement::SettledNoChangeClean
        }
    }

    fn install_prepared(
        &mut self,
        snapshot: &PaneRenderBeginSnapshot,
        baseline: PaneRenderBaseline,
        redirtied_after_snapshot: bool,
    ) -> Result<(), PaneRenderPreparationError> {
        let phase = std::mem::replace(
            &mut self.transaction_phase,
            PaneRenderTransactionPhase::Closed,
        );
        let PaneRenderTransactionPhase::Preparing { token, redirtied } = phase else {
            self.transaction_phase = phase;
            return Err(PaneRenderPreparationError::StaleAttempt);
        };
        if token != snapshot.token || self.baseline != snapshot.baseline {
            self.transaction_phase = PaneRenderTransactionPhase::Preparing { token, redirtied };
            return Err(PaneRenderPreparationError::StaleAttempt);
        }
        if self
            .notifications
            .as_slice()
            .get(..snapshot.covered_notifications.len())
            != Some(snapshot.covered_notifications.as_slice())
        {
            self.close_transaction();
            return Err(PaneRenderPreparationError::NotificationPrefixChanged);
        }

        let redirtied = redirtied
            || redirtied_after_snapshot
            || snapshot.has_uncovered_notifications
            || self.notifications.len() > snapshot.covered_notifications.len();
        self.transaction_phase = PaneRenderTransactionPhase::InFlight {
            pending: Box::new(PendingPaneRenderCommit {
                token,
                baseline,
                covered_notifications: snapshot.covered_notifications.clone(),
            }),
            redirtied,
        };
        self.transactional_dirty = redirtied;
        Ok(())
    }

    fn acknowledge_prepared(&mut self, token: PaneRenderAttemptToken) -> PaneRenderSettlement {
        let phase = std::mem::replace(
            &mut self.transaction_phase,
            PaneRenderTransactionPhase::Closed,
        );
        let PaneRenderTransactionPhase::InFlight { pending, redirtied } = phase else {
            self.transaction_phase = phase;
            return if matches!(self.transaction_phase, PaneRenderTransactionPhase::Closed) {
                PaneRenderSettlement::Closed
            } else {
                PaneRenderSettlement::StaleOrDuplicate
            };
        };
        if pending.token != token {
            self.transaction_phase = PaneRenderTransactionPhase::InFlight { pending, redirtied };
            return PaneRenderSettlement::StaleOrDuplicate;
        }
        if self
            .notifications
            .as_slice()
            .get(..pending.covered_notifications.len())
            != Some(pending.covered_notifications.as_slice())
        {
            self.close_transaction();
            return PaneRenderSettlement::FailedClosed;
        }

        let PendingPaneRenderCommit {
            baseline,
            covered_notifications,
            ..
        } = *pending;
        let Some(next_baseline_revision) = self.baseline_revision.checked_add(1) else {
            self.close_transaction();
            return PaneRenderSettlement::FailedClosed;
        };
        if self
            .notifications
            .drain_prefix(covered_notifications.len())
            .is_err()
        {
            self.close_transaction();
            return PaneRenderSettlement::FailedClosed;
        }
        self.notifications.clear_protection();
        self.baseline = baseline;
        self.baseline_revision = next_baseline_revision;
        self.transaction_phase = PaneRenderTransactionPhase::Idle;
        self.transactional_dirty = redirtied || !self.notifications.is_empty();
        if self.transactional_dirty {
            PaneRenderSettlement::AcknowledgedRedirtied
        } else {
            PaneRenderSettlement::AcknowledgedClean
        }
    }

    fn retry_prepared(&mut self, token: PaneRenderAttemptToken) -> PaneRenderSettlement {
        let matches = matches!(
            &self.transaction_phase,
            PaneRenderTransactionPhase::InFlight { pending, .. } if pending.token == token
        );
        if !matches {
            return if matches!(self.transaction_phase, PaneRenderTransactionPhase::Closed) {
                PaneRenderSettlement::Closed
            } else {
                PaneRenderSettlement::StaleOrDuplicate
            };
        }
        self.transaction_phase = PaneRenderTransactionPhase::Idle;
        self.notifications.clear_protection();
        self.transactional_dirty = true;
        PaneRenderSettlement::Retried
    }

    fn close_transaction(&mut self) {
        self.retire_render_authority();
    }
}

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
enum PaneRenderPreparationOutcome {
    NoChange(PaneRenderSettlement),
    Prepared(Box<PreparedPaneRender>),
}

#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
struct PaneRenderPreparation {
    state: Arc<Mutex<PerPane>>,
    snapshot: PaneRenderBeginSnapshot,
    force_with_input_dispatch_serial: Option<InputSerial>,
    armed: bool,
}

#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
struct PreparedPaneRender {
    state: Arc<Mutex<PerPane>>,
    token: PaneRenderAttemptToken,
    surface: GetPaneRenderChangesResponse,
    semantic_zones: GetSemanticZonesResponse,
    palette: Option<SetPalette>,
    alerts: Vec<NotifyAlert>,
    armed: bool,
}

impl std::fmt::Debug for PreparedPaneRender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedPaneRender")
            .field("token", &self.token)
            .field("surface", &self.surface)
            .field("semantic_zones", &self.semantic_zones)
            .field("palette", &self.palette)
            .field("alerts", &self.alerts)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

fn normalize_prepared_alerts(pane_id: PaneId, notifications: &[Alert]) -> (bool, Vec<NotifyAlert>) {
    let mut palette_changed = false;
    let mut unseen_output = false;
    let mut latest_progress = None;
    let mut alerts = Vec::with_capacity(notifications.len());

    for alert in notifications {
        match alert {
            Alert::PaletteChanged => palette_changed = true,
            Alert::OutputSinceFocusLost => unseen_output = true,
            Alert::Progress(progress) => latest_progress = Some(progress.clone()),
            event => alerts.push(NotifyAlert {
                pane_id,
                alert: event.clone(),
            }),
        }
    }
    if unseen_output {
        alerts.push(NotifyAlert {
            pane_id,
            alert: Alert::OutputSinceFocusLost,
        });
    }
    if let Some(progress) = latest_progress {
        alerts.push(NotifyAlert {
            pane_id,
            alert: Alert::Progress(progress),
        });
    }
    debug_assert!(alerts.len() <= codec::MAX_RENDER_APPLICATION_ALERTS);
    (palette_changed, alerts)
}

#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
fn begin_transactional_pane_render(
    state: Arc<Mutex<PerPane>>,
    pane_id: PaneId,
    force_with_input_dispatch_serial: Option<InputSerial>,
) -> Result<PaneRenderPreparation, PaneRenderPreparationError> {
    let snapshot = match state.lock() {
        Ok(mut per_pane) => {
            per_pane.begin_transactional_preparation(pane_id, force_with_input_dispatch_serial)?
        }
        Err(poison) => {
            retire_poisoned_pane_render(&state, poison);
            return Err(PaneRenderPreparationError::StateLockPoisoned);
        }
    };
    Ok(PaneRenderPreparation {
        state,
        snapshot,
        force_with_input_dispatch_serial,
        armed: true,
    })
}

#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
impl PaneRenderPreparation {
    fn token(&self) -> PaneRenderAttemptToken {
        self.snapshot.token
    }

    fn prepare(
        mut self,
        pane: &CurrentPane<'_>,
    ) -> Result<PaneRenderPreparationOutcome, PaneRenderPreparationError> {
        let pane_id = pane.pane_id();
        if pane_id != self.snapshot.token.pane_id {
            return Err(PaneRenderPreparationError::StaleAttempt);
        }

        let (palette_alerted, alerts) =
            normalize_prepared_alerts(pane_id, &self.snapshot.covered_notifications);
        let config_before = config::configuration();
        let config_generation = config_before.generation();
        let needs_palette = palette_alerted
            || !self.snapshot.baseline.sent_initial_palette
            || self.snapshot.baseline.config_generation != config_generation;
        let force_for_atomic_effects = needs_palette || !alerts.is_empty();
        let surface = self.snapshot.baseline.prepare_surface_changes(
            pane,
            self.force_with_input_dispatch_serial,
            force_for_atomic_effects,
        );

        let (source_start, source_query, source_end) = match &surface {
            SurfacePreparation::StableRowRangeUnrepresentable => {
                return Err(PaneRenderPreparationError::StableRowRangeUnrepresentable);
            }
            SurfacePreparation::NoChange {
                source_start,
                source_query,
                source_end,
            } => (*source_start, *source_query, *source_end),
            SurfacePreparation::Changes(prepared) => (
                prepared.source_start,
                prepared.source_query,
                prepared.source_end,
            ),
        };
        if source_start == SequenceNo::MAX
            || source_query == SequenceNo::MAX
            || source_end == SequenceNo::MAX
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poison) => {
                    retire_poisoned_pane_render(&self.state, poison);
                    return Err(PaneRenderPreparationError::StateLockPoisoned);
                }
            };
            let outcome = state.close_exhausted_preparation(self.snapshot.token);
            self.armed = false;
            debug_assert!(matches!(
                outcome,
                PaneRenderSettlement::Closed | PaneRenderSettlement::StaleOrDuplicate
            ));
            return Err(PaneRenderPreparationError::TerminalSequenceExhausted);
        }
        if source_start != source_query || source_query != source_end {
            return Err(PaneRenderPreparationError::SourceChanged);
        }

        let SurfacePreparation::Changes(mut surface) = surface else {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poison) => {
                    retire_poisoned_pane_render(&self.state, poison);
                    return Err(PaneRenderPreparationError::StateLockPoisoned);
                }
            };
            let outcome = state.settle_no_change(self.snapshot.token);
            self.armed = false;
            return Ok(PaneRenderPreparationOutcome::NoChange(outcome));
        };

        let (zones, zone_texts, last_exit_code) = pane.semantic_snapshot().map_err(|err| {
            log::warn!(
                "failed to prepare transactional semantic snapshot for pane {pane_id}: {err:#}"
            );
            PaneRenderPreparationError::SnapshotFailed
        })?;
        let semantic_zones = GetSemanticZonesResponse {
            pane_id,
            zones,
            zone_texts,
            last_exit_code,
        };
        let palette = needs_palette.then(|| SetPalette {
            pane_id,
            palette: pane.palette(),
        });
        let source_after_effects = pane.get_current_seqno();
        if source_after_effects != source_start {
            return Err(PaneRenderPreparationError::SourceChanged);
        }

        let config_after_generation = config::configuration().generation();
        let redirtied_after_snapshot = config_after_generation != config_generation;
        if needs_palette {
            surface.baseline.sent_initial_palette = true;
            surface.baseline.config_generation = config_generation;
        }
        if let Some(input_epoch) = self.snapshot.token.input_epoch {
            surface.baseline.committed_input_epoch = input_epoch;
        }

        let PreparedSurfaceChanges {
            response, baseline, ..
        } = *surface;
        let install_result = match self.state.lock() {
            Ok(mut state) => {
                state.install_prepared(&self.snapshot, baseline, redirtied_after_snapshot)
            }
            Err(poison) => {
                retire_poisoned_pane_render(&self.state, poison);
                return Err(PaneRenderPreparationError::StateLockPoisoned);
            }
        };
        install_result?;
        self.armed = false;
        Ok(PaneRenderPreparationOutcome::Prepared(Box::new(
            PreparedPaneRender {
                state: Arc::clone(&self.state),
                token: self.snapshot.token,
                surface: response,
                semantic_zones,
                palette,
                alerts,
                armed: true,
            },
        )))
    }
}

impl Drop for PaneRenderPreparation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match self.state.lock() {
            Ok(mut state) => {
                let _ = state.cancel_preparation(self.snapshot.token);
            }
            Err(err) => {
                if !std::thread::panicking() {
                    log::error!(
                        "failed to recover cancelled pane render preparation after lock poison: {err}"
                    );
                }
                retire_poisoned_pane_render(&self.state, err);
            }
        }
    }
}

#[allow(
    dead_code,
    reason = "live ownership transfers to the delivery coordinator under ft-interactive-systems-performance-4tenz.5.5.10"
)]
impl PreparedPaneRender {
    fn token(&self) -> PaneRenderAttemptToken {
        self.token
    }

    fn acknowledge(mut self) -> PaneRenderSettlement {
        let outcome = match self.state.lock() {
            Ok(mut state) => state.acknowledge_prepared(self.token),
            Err(err) => {
                log::error!(
                    "failed to acknowledge pane render application after lock poison: {err}"
                );
                retire_poisoned_pane_render(&self.state, err);
                PaneRenderSettlement::FailedClosed
            }
        };
        self.armed = false;
        outcome
    }

    fn nack(mut self) -> PaneRenderSettlement {
        let outcome = match self.state.lock() {
            Ok(mut state) => state.retry_prepared(self.token),
            Err(err) => {
                log::error!("failed to retry pane render application after lock poison: {err}");
                retire_poisoned_pane_render(&self.state, err);
                PaneRenderSettlement::FailedClosed
            }
        };
        self.armed = false;
        outcome
    }
}

impl Drop for PreparedPaneRender {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if std::thread::panicking() {
            // Once a prepared application is handed to a delivery
            // coordinator, an unwind may happen after queue admission but
            // before local settlement. Retrying that indeterminate candidate
            // without an application ACK could duplicate exact effects.
            retire_pane_render_after_settlement_failure(&self.state);
            return;
        }
        match self.state.lock() {
            Ok(mut state) => {
                let _ = state.retry_prepared(self.token);
            }
            Err(err) => {
                if !std::thread::panicking() {
                    log::error!(
                        "failed to recover abandoned pane render application after lock poison: {err}"
                    );
                }
                retire_poisoned_pane_render(&self.state, err);
            }
        }
    }
}

struct LegacyRenderEnqueueGuard {
    per_pane: Arc<Mutex<PerPane>>,
    pane_id: PaneId,
    prior_baseline: PaneRenderBaseline,
    installed_revision: u64,
    armed: bool,
}

impl LegacyRenderEnqueueGuard {
    fn new(
        per_pane: Arc<Mutex<PerPane>>,
        pane_id: PaneId,
        prior_baseline: PaneRenderBaseline,
        installed_revision: u64,
    ) -> Self {
        Self {
            per_pane,
            pane_id,
            prior_baseline,
            installed_revision,
            armed: true,
        }
    }

    fn acknowledge(mut self) -> anyhow::Result<()> {
        let result = match catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| {
                acknowledge_legacy_render_enqueue(
                    &self.per_pane,
                    self.pane_id,
                    self.installed_revision,
                )
            }),
        ) {
            Ok(result) => result,
            Err(err) => Err(anyhow!(
                "legacy render enqueue acknowledgement panicked for pane {}: {err}",
                self.pane_id
            )),
        };
        if result.is_err() {
            retire_pane_render_after_settlement_failure(&self.per_pane);
        }
        self.armed = false;
        result
    }

    fn recover(&self) -> anyhow::Result<()> {
        match catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| {
                retain_legacy_render_after_enqueue_failure(
                    &self.per_pane,
                    &self.prior_baseline,
                    self.installed_revision,
                )
            }),
        ) {
            Ok(result) => result,
            Err(err) => Err(anyhow!(
                "legacy render enqueue recovery panicked for pane {}: {err}",
                self.pane_id
            )),
        }
    }

    fn rollback(mut self) -> anyhow::Result<()> {
        let result = self.recover();
        if result.is_err() {
            retire_pane_render_after_settlement_failure(&self.per_pane);
        }
        self.armed = false;
        result
    }

    fn retire(mut self) {
        retire_pane_render_after_settlement_failure(&self.per_pane);
        self.armed = false;
    }
}

impl Drop for LegacyRenderEnqueueGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if std::thread::panicking() {
            // A callback may panic after admitting the PDU. Its delivery state
            // is unknowable, so rollback-and-retry could duplicate effects.
            retire_pane_render_after_settlement_failure(&self.per_pane);
            return;
        }
        if let Err(err) = self.recover() {
            retire_pane_render_after_settlement_failure(&self.per_pane);
            log::error!(
                "failed to recover abandoned legacy render enqueue for pane {}: {err:#}",
                self.pane_id
            );
        }
    }
}

struct UnsentNotificationsGuard {
    per_pane: Arc<Mutex<PerPane>>,
    notifications: Vec<Alert>,
    next_unsent: usize,
    armed: bool,
}

impl UnsentNotificationsGuard {
    fn new(per_pane: Arc<Mutex<PerPane>>, notifications: Vec<Alert>) -> Self {
        Self {
            per_pane,
            notifications,
            next_unsent: 0,
            armed: true,
        }
    }

    fn current(&self) -> Option<&Alert> {
        self.notifications.get(self.next_unsent)
    }

    fn acknowledge_current(&mut self) -> anyhow::Result<()> {
        let expected = self
            .notifications
            .get(self.next_unsent)
            .ok_or_else(|| anyhow!("no protected pane notification remains to acknowledge"))?;
        let mut state =
            lock_per_pane_or_retire(&self.per_pane, "committing a legacy pane notification")?;
        if !matches!(
            state.legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::NotificationsInFlight
        ) || !matches!(state.transaction_phase, PaneRenderTransactionPhase::Idle)
        {
            state.retire_render_authority();
            return Err(PaneRenderPreparationError::LegacyDeliveryAuthorityChanged.into());
        }
        if let Err(error) = state.notifications.acknowledge_protected_front(expected) {
            state.retire_render_authority();
            return Err(error.into());
        }
        drop(state);
        self.next_unsent = self
            .next_unsent
            .checked_add(1)
            .ok_or_else(|| anyhow!("pane notification acknowledgement index overflowed"))?;
        Ok(())
    }

    fn settle_completed(&self) -> anyhow::Result<()> {
        if self.next_unsent != self.notifications.len() {
            anyhow::bail!("pane notification batch is not completely acknowledged");
        }
        let mut state =
            lock_per_pane_or_retire(&self.per_pane, "settling a legacy pane notification batch")?;
        if !matches!(
            state.legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::NotificationsInFlight
        ) || state.notifications.protected_prefix_len != 0
            || !matches!(state.transaction_phase, PaneRenderTransactionPhase::Idle)
        {
            state.retire_render_authority();
            return Err(PaneRenderPreparationError::LegacyDeliveryAuthorityChanged.into());
        }
        state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::Idle;
        Ok(())
    }

    fn acknowledge_all(mut self) -> anyhow::Result<()> {
        let result = self.settle_completed();
        if result.is_err() {
            retire_pane_render_after_settlement_failure(&self.per_pane);
        }
        self.armed = false;
        result
    }

    fn retire(mut self) {
        retire_pane_render_after_settlement_failure(&self.per_pane);
        self.armed = false;
    }

    fn recover(&self) -> anyhow::Result<()> {
        match catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| {
                let unsent = self.notifications.get(self.next_unsent..).ok_or_else(|| {
                    anyhow!("pane notification recovery index exceeded its protected batch")
                })?;
                let mut state = lock_per_pane_or_retire(
                    &self.per_pane,
                    "recovering a legacy pane notification batch",
                )?;
                if !matches!(
                    state.legacy_enqueue_phase,
                    LegacyRenderEnqueuePhase::NotificationsInFlight
                ) || !matches!(state.transaction_phase, PaneRenderTransactionPhase::Idle)
                {
                    state.retire_render_authority();
                    return Err(PaneRenderPreparationError::LegacyDeliveryAuthorityChanged.into());
                }
                if let Err(error) = state.notifications.release_protected_prefix(unsent) {
                    state.retire_render_authority();
                    return Err(error.into());
                }
                state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::Idle;
                state.mark_transactional_dirty();
                Ok(())
            }),
        ) {
            Ok(result) => result,
            Err(err) => Err(anyhow!("pane notification recovery panicked: {err}")),
        }
    }

    fn rollback(mut self) -> anyhow::Result<()> {
        let result = self.recover();
        if result.is_err() {
            retire_pane_render_after_settlement_failure(&self.per_pane);
        }
        self.armed = false;
        result
    }
}

impl Drop for UnsentNotificationsGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if std::thread::panicking() {
            // As with surface delivery, a panicking sender may already have
            // admitted the alert. Never retry an indeterminate exact event.
            retire_pane_render_after_settlement_failure(&self.per_pane);
            return;
        }
        let result = if self.next_unsent >= self.notifications.len() {
            self.settle_completed()
        } else {
            self.recover()
        };
        if let Err(err) = result {
            retire_pane_render_after_settlement_failure(&self.per_pane);
            log::error!("failed to settle abandoned pane notifications: {err:#}");
        }
    }
}

fn retain_legacy_render_after_enqueue_failure(
    per_pane: &Arc<Mutex<PerPane>>,
    prior_baseline: &PaneRenderBaseline,
    installed_revision: u64,
) -> anyhow::Result<()> {
    let mut state = lock_per_pane_or_retire(per_pane, "recovering a legacy pane render")?;
    #[cfg(test)]
    assert!(
        !std::mem::take(&mut state.panic_next_legacy_enqueue_recovery),
        "synthetic legacy render recovery panic"
    );
    let owns_enqueue = matches!(
        state.legacy_enqueue_phase,
        LegacyRenderEnqueuePhase::InFlight {
            installed_revision: active,
        } if active == installed_revision
    );
    if !owns_enqueue || state.baseline_revision != installed_revision {
        state.retire_render_authority();
        return Err(PaneRenderPreparationError::LegacyDeliveryAuthorityChanged.into());
    }

    let current_config_generation = state.baseline.config_generation;
    let current_sent_initial_palette = state.baseline.sent_initial_palette;
    state.baseline.clone_from(prior_baseline);
    // Palette/config bookkeeping is committed independently after the
    // surface enqueue. Preserve a newer metadata update while rolling back
    // only the speculative surface baseline owned by this exact revision.
    state.baseline.config_generation = current_config_generation;
    state.baseline.sent_initial_palette = current_sent_initial_palette;
    state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::Idle;
    state.mark_transactional_dirty();
    Ok(())
}

/// Retire a pane after acknowledgement/recovery could not prove a unique
/// terminal state. This path deliberately recovers a poisoned mutex: closing
/// both render protocols and releasing any local alert-prefix marker restores
/// the fail-closed invariant before the poison marker is cleared. A baseline
/// or alert may represent delivered or undelivered bytes, so neither may be
/// exposed to another render attempt.
fn retire_pane_render_after_settlement_failure(per_pane: &Arc<Mutex<PerPane>>) {
    match per_pane.lock() {
        Ok(mut state) => state.retire_render_authority(),
        Err(poison) => retire_poisoned_pane_render(per_pane, poison),
    }
}

/// Repair a poisoned render state while retaining the original recovered
/// mutex guard. No observer can acquire the mutex between recovery and the
/// installation of the closed/dirty invariant, and poison is cleared only
/// while that repaired state is still exclusively held.
pub(crate) fn retire_poisoned_pane_render<'a>(
    per_pane: &'a Arc<Mutex<PerPane>>,
    poison: std::sync::PoisonError<std::sync::MutexGuard<'a, PerPane>>,
) {
    let mut state = poison.into_inner();
    state.retire_render_authority();
    per_pane.clear_poison();
}

/// Acquire per-pane render state or turn mutex poison into an explicit closed
/// protocol state. A panic while holding this mutex leaves every field suspect;
/// returning a bare poison error would strand the registration without proving
/// whether either delivery protocol still owned a baseline or alert prefix.
fn lock_per_pane_or_retire<'a>(
    per_pane: &'a Arc<Mutex<PerPane>>,
    operation: &'static str,
) -> anyhow::Result<std::sync::MutexGuard<'a, PerPane>> {
    match per_pane.lock() {
        Ok(state) => Ok(state),
        Err(poison) => {
            retire_poisoned_pane_render(per_pane, poison);
            Err(anyhow!("per-pane state lock poisoned while {operation}"))
        }
    }
}

fn acknowledge_legacy_render_enqueue(
    per_pane: &Arc<Mutex<PerPane>>,
    pane_id: PaneId,
    installed_revision: u64,
) -> anyhow::Result<()> {
    let mut state = lock_per_pane_or_retire(per_pane, "acknowledging a legacy pane render")?;
    #[cfg(test)]
    assert!(
        !std::mem::take(&mut state.panic_next_legacy_enqueue_ack),
        "synthetic legacy render acknowledgement panic"
    );
    let owns_enqueue = matches!(
        state.legacy_enqueue_phase,
        LegacyRenderEnqueuePhase::InFlight {
            installed_revision: active,
        } if active == installed_revision
    );
    if !owns_enqueue || state.baseline_revision != installed_revision {
        state.retire_render_authority();
        return Err(anyhow!(
            "cannot acknowledge legacy render enqueue for pane {pane_id}: {}",
            PaneRenderPreparationError::LegacyDeliveryAuthorityChanged
        ));
    }
    state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::Idle;
    Ok(())
}

fn maybe_push_pane_changes(
    pane: &CurrentPane<'_>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    if let Some((resp, rollback_guard)) = prepare_legacy_render_enqueue(pane, &per_pane, None)? {
        let send_result = sender.send_bulk(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        });
        match send_result {
            Ok(()) => rollback_guard.acknowledge()?,
            Err(send_err) => {
                if let Err(recovery_err) = rollback_guard.rollback() {
                    return Err(anyhow!(
                        "render enqueue failed: {send_err:#}; baseline recovery also failed: {recovery_err:#}"
                    ));
                }
                return Err(send_err);
            }
        }
    }

    let config_generation = config::configuration().generation();
    let (mut notifications_remaining, first_notification_batch) = {
        let mut per_pane =
            lock_per_pane_or_retire(&per_pane, "preparing legacy pane notifications")?;
        per_pane.ensure_legacy_transport_idle()?;
        if per_pane.baseline.config_generation != config_generation {
            // If the config changed, it may have changed colors
            // in the palette that we need to push down, so we
            // synthesize a palette change notification to let
            // the client know
            per_pane
                .notifications
                .push(Alert::PaletteChanged)
                .map_err(anyhow::Error::from)?;
            per_pane.baseline.config_generation = config_generation;
            per_pane.baseline.sent_initial_palette = true;
        }

        if !per_pane.baseline.sent_initial_palette {
            per_pane
                .notifications
                .push(Alert::PaletteChanged)
                .map_err(anyhow::Error::from)?;
            per_pane.baseline.sent_initial_palette = true;
        }
        let notifications_remaining = per_pane.notifications.len();
        let batch = per_pane
            .notifications
            .protected_batch_up_to(notifications_remaining)
            .map_err(anyhow::Error::from)?;
        if notifications_remaining != 0 {
            per_pane.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
        }
        (notifications_remaining, batch)
    };

    let mut next_notification_batch = Some(first_notification_batch);
    while notifications_remaining != 0 {
        let batch = next_notification_batch
            .take()
            .ok_or_else(|| anyhow!("pane notification batch authority was lost"))?;
        let batch_len = batch.len();
        let mut notifications = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);
        if batch_len == 0 || batch_len > notifications_remaining {
            notifications.rollback()?;
            anyhow::bail!("pane notification batch made no bounded forward progress");
        }
        while let Some(alert) = notifications.current() {
            let send_result = match alert {
                Alert::PaletteChanged => sender.send_bulk(DecodedPdu {
                    pdu: Pdu::SetPalette(SetPalette {
                        pane_id: pane.pane_id(),
                        palette: pane.palette(),
                    }),
                    serial: 0,
                }),
                alert => sender.send_bulk(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert: alert.clone(),
                    }),
                    serial: 0,
                }),
            };
            match send_result {
                Ok(()) => {
                    if let Err(commit_err) = notifications.acknowledge_current() {
                        // The client accepted the notification, but local
                        // state could not prove which exact event was drained.
                        // Retrying can duplicate bells and other edge-triggered
                        // effects, so this registration is terminal.
                        notifications.retire();
                        return Err(anyhow!(
                            "notification enqueue committed but local settlement failed; pane render authority retired: {commit_err:#}"
                        ));
                    }
                }
                Err(send_err) => {
                    if let Err(recovery_err) = notifications.rollback() {
                        return Err(anyhow!(
                            "notification enqueue failed: {send_err:#}; retention also failed: {recovery_err:#}"
                        ));
                    }
                    return Err(send_err);
                }
            }
        }
        notifications
            .acknowledge_all()
            .context("settling committed legacy pane-notification batch")?;
        notifications_remaining = notifications_remaining
            .checked_sub(batch_len)
            .ok_or_else(|| anyhow!("pane notification batch accounting underflowed"))?;
        if notifications_remaining != 0 {
            let batch = {
                let mut state = lock_per_pane_or_retire(
                    &per_pane,
                    "preparing the next legacy pane-notification batch",
                )?;
                state.ensure_legacy_transport_idle()?;
                let batch = state
                    .notifications
                    .protected_batch_up_to(notifications_remaining)
                    .map_err(anyhow::Error::from)?;
                state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
                batch
            };
            next_notification_batch = Some(batch);
        }
    }
    Ok(())
}

/// A pane input mutation is authoritative once its pane method succeeds.
/// Render preparation and enqueue happen afterwards and therefore must not
/// turn that committed input into an RPC error that invites a duplicate retry.
fn push_pane_changes_after_committed_input(
    pane: &CurrentPane<'_>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
    operation: &'static str,
) {
    let recovery_state = Arc::clone(&per_pane);
    match catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| maybe_push_pane_changes(pane, sender, per_pane)),
    ) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            mark_post_input_render_dirty(&recovery_state, pane.pane_id(), operation);
            log::error!(
                "render push failed after committed {operation} for pane {}; preserving the input acknowledgment: {err:#}",
                pane.pane_id()
            );
        }
        Err(err) => {
            mark_post_input_render_dirty(&recovery_state, pane.pane_id(), operation);
            log::error!(
                "render push panicked after committed {operation} for pane {}; preserving the input acknowledgment: {err}",
                pane.pane_id()
            );
        }
    }
}

fn mark_post_input_render_dirty(
    per_pane: &Arc<Mutex<PerPane>>,
    pane_id: PaneId,
    operation: &'static str,
) {
    match lock_per_pane_or_retire(per_pane, "marking a committed input render dirty") {
        Ok(mut state) => state.mark_transactional_dirty(),
        Err(err) => log::error!(
            "render state lock poisoned while recovering committed {operation} for pane {pane_id}: {err}"
        ),
    }
}

fn push_input_dispatch_changes_after_committed_input(
    pane: &CurrentPane<'_>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
    input_serial: InputSerial,
    operation: &'static str,
) {
    let pane_id = pane.pane_id();
    // Force a surface snapshot so the client can measure dispatch RTT and
    // record the exact terminal-sequence fence sampled after the pane input.
    // This acknowledges protocol dispatch only; it does not claim that the
    // PTY or application has echoed the input.
    let prepared = catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| prepare_legacy_render_enqueue(pane, &per_pane, Some(input_serial))),
    );

    let (response, rollback) = match prepared {
        Ok(Ok(Some(prepared))) => prepared,
        Ok(Ok(None)) => return,
        Ok(Err(err)) => {
            mark_post_input_render_dirty(&per_pane, pane_id, operation);
            log::error!(
                "render preparation failed after committed {operation} for pane {pane_id}; preserving the input acknowledgment: {err:#}"
            );
            return;
        }
        Err(err) => {
            mark_post_input_render_dirty(&per_pane, pane_id, operation);
            log::error!(
                "render preparation panicked after committed {operation} for pane {pane_id}; preserving the input acknowledgment: {err}"
            );
            return;
        }
    };

    match catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| {
            sender.send_control(DecodedPdu {
                pdu: Pdu::GetPaneRenderChangesResponse(response),
                serial: 0,
            })
        }),
    ) {
        Ok(Ok(())) => {
            if let Err(err) = rollback.acknowledge() {
                mark_post_input_render_dirty(&per_pane, pane_id, operation);
                log::error!(
                    "render enqueue acknowledgement failed after committed {operation} for pane {pane_id}; preserving the input acknowledgment: {err:#}"
                );
            }
        }
        Ok(Err(err)) => {
            if let Err(recovery_err) = rollback.rollback() {
                log::error!(
                    "render baseline recovery failed after committed {operation} for pane {pane_id}: {recovery_err:#}"
                );
            }
            log::error!(
                "render enqueue failed after committed {operation} for pane {pane_id}; preserving the input acknowledgment: {err:#}"
            );
        }
        Err(err) => {
            // The sender panic was caught before the guard could observe an
            // active unwind. Admission may already have occurred, so an
            // explicit terminal settlement is required here; rolling back and
            // retrying could duplicate this input-correlated surface PDU.
            rollback.retire();
            log::error!(
                "render enqueue panicked after committed {operation} for pane {pane_id}; preserving the input acknowledgment: {err}"
            );
        }
    }
}

fn session_mux(authority: &SessionAuthority) -> anyhow::Result<CurrentSession> {
    authority.acquire()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListPanesSnapshotStage {
    BeforeOrderedAuthorityRead,
    WindowsEnumerated,
    OrderedWindowsFrozen,
    TabTreeCaptured,
    TitlesCaptured,
}

fn ignore_list_panes_snapshot_stage(_: ListPanesSnapshotStage) {}

fn floating_pane_snapshot_entry(
    window_id: mux::window::WindowId,
    tab_id: mux::tab::TabId,
    workspace: &str,
    positioned: mux::tab::PositionedFloatingPane,
) -> codec::FloatingPaneSnapshotEntry {
    let pane = positioned.pane;
    let dimensions = pane.get_dimensions();
    codec::FloatingPaneSnapshotEntry {
        pane: mux::tab::PaneEntry {
            window_id,
            tab_id,
            pane_id: positioned.pane_id,
            title: pane.get_title(),
            size: wezterm_term::TerminalSize {
                cols: dimensions.cols,
                rows: dimensions.viewport_rows,
                pixel_height: dimensions.pixel_height,
                pixel_width: dimensions.pixel_width,
                dpi: dimensions.dpi,
            },
            working_dir: pane
                .get_current_working_dir(CachePolicy::AllowStale)
                .map(Into::into),
            alt_screen_active: pane.is_alt_screen_active(),
            is_active_pane: positioned.is_focused,
            is_zoomed_pane: false,
            workspace: workspace.to_string(),
            cursor_pos: pane.get_cursor_position(),
            physical_top: dimensions.physical_top,
            top_row: positioned.top,
            left_col: positioned.left,
            tty_name: pane.tty_name(),
        },
        rect: mux::tab::FloatingPaneRect {
            left: positioned.left,
            top: positioned.top,
            width: positioned.width,
            height: positioned.height,
        },
        z_order: positioned.z_order,
        visible: positioned.visible,
        pinned: positioned.pinned,
        opacity: positioned.opacity,
        focused: positioned.is_focused,
    }
}

fn append_floating_pane_snapshot(
    output: &mut Vec<codec::FloatingPaneSnapshotEntry>,
    window_id: mux::window::WindowId,
    tab: &mux::tab::Tab,
    workspace: &str,
) -> anyhow::Result<()> {
    let positioned = tab.iter_floating_panes();
    let next_len = output
        .len()
        .checked_add(positioned.len())
        .context("counting floating panes in authoritative snapshot")?;
    if next_len > codec::MAX_FLOATING_PANES_PER_SNAPSHOT {
        anyhow::bail!(
            "floating pane snapshot has {next_len} panes; maximum is {}",
            codec::MAX_FLOATING_PANES_PER_SNAPSHOT
        );
    }
    output
        .try_reserve(positioned.len())
        .context("reserving bounded floating pane snapshot")?;
    output.extend(positioned.into_iter().map(|positioned| {
        floating_pane_snapshot_entry(window_id, tab.tab_id(), workspace, positioned)
    }));
    Ok(())
}

fn collect_list_panes_snapshot_with_stage_observer(
    mux: &Mux,
    observer: &mut impl FnMut(ListPanesSnapshotStage),
) -> anyhow::Result<ListPanesResponse> {
    let mut tabs = Vec::new();
    let mut tab_titles = Vec::new();
    let mut window_titles = HashMap::new();
    let mut floating_panes = Vec::new();
    let window_ids = mux.iter_windows();
    observer(ListPanesSnapshotStage::WindowsEnumerated);
    for window_id in window_ids {
        let window_snapshot = mux.get_window(window_id).map(|window| {
            (
                window.get_title().to_string(),
                window.get_workspace().to_string(),
                window.iter().cloned().collect::<Vec<_>>(),
            )
        });
        let Some((window_title, workspace, window_tabs)) = window_snapshot else {
            log::warn!(
                "ListPanes skipped stale window id {} from iter_windows",
                window_id
            );
            continue;
        };
        window_titles.insert(window_id, window_title);
        for tab in window_tabs {
            tabs.push(tab.codec_pane_tree_in_window(window_id, &workspace)?);
            append_floating_pane_snapshot(
                &mut floating_panes,
                window_id,
                tab.as_ref(),
                &workspace,
            )?;
            observer(ListPanesSnapshotStage::TabTreeCaptured);
            tab_titles.push(tab.get_title());
        }
    }
    observer(ListPanesSnapshotStage::TitlesCaptured);
    log::trace!(
        "ListPanes snapshot has {} tab trees, {} tab titles, and {} windows",
        tabs.len(),
        tab_titles.len(),
        window_titles.len()
    );
    Ok(ListPanesResponse {
        tabs,
        tab_titles,
        window_titles,
        floating_panes,
    })
}

fn collect_list_panes_snapshot(mux: &Mux) -> anyhow::Result<ListPanesResponse> {
    let response = collect_list_panes_snapshot_with_stage_observer(
        mux,
        &mut ignore_list_panes_snapshot_stage,
    )?;
    response
        .validate_floating_panes()
        .context("validating collected floating-pane snapshot")?;
    Ok(response)
}

const COHERENT_SNAPSHOT_ATTEMPTS: u8 = 3;

fn collect_coherent_list_panes_snapshot(mux: &Mux) -> anyhow::Result<ListPanesCoherentOutcome> {
    collect_coherent_list_panes_snapshot_with_stage_observer(
        mux,
        &mut ignore_list_panes_snapshot_stage,
    )
}

fn collect_coherent_list_panes_snapshot_with_stage_observer(
    mux: &Mux,
    observer: &mut impl FnMut(ListPanesSnapshotStage),
) -> anyhow::Result<ListPanesCoherentOutcome> {
    let mut first_revision = None;
    let mut last_revision = None;

    for attempt in 1..=COHERENT_SNAPSHOT_ATTEMPTS {
        let (before_session, before_revision) = match mux.topology_snapshot_authority() {
            Ok(authority) => authority,
            Err(_) => {
                metrics::counter!(
                    "mux.server.coherent_snapshot.total",
                    "outcome" => "revision_exhausted"
                )
                .increment(1);
                return Ok(ListPanesCoherentOutcome::RevisionExhausted);
            }
        };
        if before_revision.get() == u64::MAX {
            metrics::counter!(
                "mux.server.coherent_snapshot.total",
                "outcome" => "revision_exhausted"
            )
            .increment(1);
            return Ok(ListPanesCoherentOutcome::RevisionExhausted);
        }

        let panes = collect_list_panes_snapshot_with_stage_observer(mux, observer)?;

        let (after_session, after_revision) = match mux.topology_snapshot_authority() {
            Ok(authority) => authority,
            Err(_) => {
                metrics::counter!(
                    "mux.server.coherent_snapshot.total",
                    "outcome" => "revision_exhausted"
                )
                .increment(1);
                return Ok(ListPanesCoherentOutcome::RevisionExhausted);
            }
        };
        if before_session != after_session {
            metrics::counter!(
                "mux.server.coherent_snapshot.total",
                "outcome" => "session_changed"
            )
            .increment(1);
            return Err(anyhow!(
                "mux session incarnation changed while constructing a coherent pane snapshot"
            ));
        }
        if before_revision == after_revision {
            panes
                .validate_floating_panes()
                .context("validating coherent floating-pane snapshot")?;
            metrics::histogram!("mux.server.coherent_snapshot.attempts").record(attempt as f64);
            metrics::counter!(
                "mux.server.coherent_snapshot.total",
                "outcome" => "snapshot"
            )
            .increment(1);
            return Ok(ListPanesCoherentOutcome::Snapshot(CoherentPaneSnapshot {
                session_incarnation: after_session,
                snapshot_revision: after_revision,
                panes,
            }));
        }

        first_revision.get_or_insert(before_revision);
        last_revision = Some(after_revision);
        metrics::counter!(
            "mux.server.coherent_snapshot.total",
            "outcome" => "retry"
        )
        .increment(1);
    }

    metrics::histogram!("mux.server.coherent_snapshot.attempts")
        .record(COHERENT_SNAPSHOT_ATTEMPTS as f64);
    metrics::counter!(
        "mux.server.coherent_snapshot.total",
        "outcome" => "contended"
    )
    .increment(1);
    Ok(ListPanesCoherentOutcome::Contended {
        attempts: COHERENT_SNAPSHOT_ATTEMPTS,
        first_revision: first_revision
            .expect("a contended snapshot records its first observed revision"),
        last_revision: last_revision
            .expect("a contended snapshot records its last observed revision"),
    })
}

/// One callback-free window capture used to derive both halves of PDU87.
///
/// `order` retains the exact `Arc<Tab>` identities captured while the mux
/// window guard was held. Pane trees and ordered-window records are derived
/// from those same identities only after the guard is dropped, so no pane
/// callback can re-enter the mux window map through this capture.
struct FrozenOrderedSnapshotWindow {
    title: String,
    workspace: String,
    order: mux::window::FrozenWindowOrder,
}

fn checked_ordered_snapshot_window_count(
    window_count: usize,
) -> Result<(), codec::OrderedWindowProtocolError> {
    if window_count > codec::MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
        return Err(codec::OrderedWindowProtocolError::TooManyWindows {
            count: window_count,
            max: codec::MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
        });
    }
    Ok(())
}

fn checked_ordered_snapshot_tab_total(
    prior: usize,
    additional: usize,
) -> Result<usize, codec::OrderedWindowProtocolError> {
    let total = prior
        .checked_add(additional)
        .ok_or(codec::OrderedWindowProtocolError::CountOverflow)?;
    if total > codec::MAX_ORDERED_TABS_PER_SNAPSHOT {
        return Err(codec::OrderedWindowProtocolError::TooManyTotalTabs {
            count: total,
            max: codec::MAX_ORDERED_TABS_PER_SNAPSHOT,
        });
    }
    Ok(total)
}

fn ordered_snapshot_tab_node_ceiling(
    prior_nodes: usize,
) -> Result<usize, codec::OrderedWindowProtocolError> {
    if prior_nodes > codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT {
        return Err(codec::OrderedWindowProtocolError::TooManyPaneNodes {
            count: prior_nodes,
            max: codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT,
        });
    }
    let per_tree_end = prior_nodes
        .checked_add(codec::MAX_ORDERED_PANE_NODES_PER_TREE)
        .ok_or(codec::OrderedWindowProtocolError::CountOverflow)?;
    Ok(per_tree_end.min(codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT))
}

pub(crate) fn validate_ordered_snapshot_projection(
    snapshot: &codec::OrderedPaneSnapshotV1,
) -> anyhow::Result<()> {
    let pane_window_titles = snapshot.panes.window_titles();
    if pane_window_titles.len() != snapshot.ordered_windows.len() {
        return Err(anyhow!(
            "PDU87 pane/order window cardinality mismatch: pane_titles={}, ordered_windows={}",
            pane_window_titles.len(),
            snapshot.ordered_windows.len()
        ));
    }

    let mut total_tabs = 0usize;
    let mut prior_window_id = None;
    for (window, pane_window_title) in snapshot.ordered_windows.iter().zip(pane_window_titles) {
        if prior_window_id.is_some_and(|prior| prior >= window.window_id) {
            return Err(anyhow!(
                "PDU87 ordered windows are not in strictly increasing window-id order"
            ));
        }
        prior_window_id = Some(window.window_id);
        if pane_window_title.window_id != window.window_id.get() {
            return Err(anyhow!(
                "PDU87 ordered window {} has no matching pane-list window title",
                window.window_id.get()
            ));
        }
        total_tabs = checked_ordered_snapshot_tab_total(total_tabs, window.ordered_tab_ids.len())?;
    }
    let pane_trees = snapshot.panes.trees();
    let pane_nodes = snapshot.panes.nodes();
    if pane_trees.len() != total_tabs {
        return Err(anyhow!(
            "PDU87 pane/order tab cardinality mismatch: pane_trees={}, ordered_tabs={total_tabs}",
            pane_trees.len()
        ));
    }

    let mut pane_index = 0usize;
    let mut snapshot_tab_owners = HashSet::new();
    let mut snapshot_pane_ids = HashSet::new();
    for window in &snapshot.ordered_windows {
        let window_id = window
            .window_id
            .try_into_usize()
            .context("narrowing PDU87 window identity for pane projection validation")?;
        for tab_id in &window.ordered_tab_ids {
            let expected = (
                window_id,
                tab_id
                    .try_into_usize()
                    .context("narrowing PDU87 tab identity for pane projection validation")?,
            );
            snapshot_tab_owners.insert(expected);
            let tree = pane_trees.get(pane_index).ok_or_else(|| {
                anyhow!("PDU87 pane tree vector ended before ordered tab {pane_index}")
            })?;
            let root_index = tree
                .root_index
                .ok_or_else(|| anyhow!("PDU87 pane tree {pane_index} has no root identity"))?;
            let tree_start =
                usize::try_from(root_index).context("narrowing PDU87 pane-tree root index")?;
            let node_count =
                usize::try_from(tree.node_count).context("narrowing PDU87 pane-tree node count")?;
            let tree_end = tree_start
                .checked_add(node_count)
                .ok_or_else(|| anyhow!("PDU87 pane tree {pane_index} range overflows usize"))?;
            let tree_nodes = pane_nodes.get(tree_start..tree_end).ok_or_else(|| {
                anyhow!(
                    "PDU87 pane tree {pane_index} range {tree_start}..{tree_end} exceeds its pane arena"
                )
            })?;

            let mut first_actual = None;
            let mut all_match = true;
            for node in tree_nodes {
                if let mux::tab::PaneArenaNode::Leaf(entry) = node {
                    let actual = (entry.window_id, entry.tab_id);
                    if first_actual.is_none() {
                        first_actual = Some(actual);
                    }
                    all_match &= actual == expected;
                    if !snapshot_pane_ids.insert(entry.pane_id) {
                        return Err(anyhow!(
                            "PDU87 pane {} has more than one tiled owner",
                            entry.pane_id
                        ));
                    }
                }
            }
            match (first_actual, all_match) {
                (Some(_), true) => {}
                (Some(actual), false) if actual != expected => {
                    return Err(anyhow!(
                        "PDU87 pane tree {pane_index} identifies window/tab {actual:?}, expected {expected:?}"
                    ));
                }
                (Some(_), false) => {
                    return Err(anyhow!(
                        "PDU87 pane tree {pane_index} contains a leaf outside expected window/tab {expected:?}"
                    ));
                }
                (None, _) => {
                    return Err(anyhow!(
                        "PDU87 pane tree {pane_index} has structure but no window/tab identity"
                    ));
                }
            }
            pane_index += 1;
        }
    }
    for floating in &snapshot.floating_panes {
        let owner = (floating.pane.window_id, floating.pane.tab_id);
        if !snapshot_tab_owners.contains(&owner) {
            return Err(anyhow!(
                "PDU87 floating pane {} names absent window/tab owner {owner:?}",
                floating.pane.pane_id
            ));
        }
        if !snapshot_pane_ids.insert(floating.pane.pane_id) {
            return Err(anyhow!(
                "PDU87 pane {} has more than one tiled/floating owner",
                floating.pane.pane_id
            ));
        }
    }
    Ok(())
}

fn collect_ordered_list_panes_snapshot(
    mux: &Mux,
) -> anyhow::Result<codec::ListPanesOrderedV1Outcome> {
    collect_ordered_list_panes_snapshot_with_stage_observer(
        mux,
        &mut ignore_list_panes_snapshot_stage,
    )
}

fn collect_ordered_list_panes_snapshot_with_stage_observer(
    mux: &Mux,
    observer: &mut impl FnMut(ListPanesSnapshotStage),
) -> anyhow::Result<codec::ListPanesOrderedV1Outcome> {
    let mut first_revision = None;
    let mut last_revision = None;

    for attempt in 1..=COHERENT_SNAPSHOT_ATTEMPTS {
        observer(ListPanesSnapshotStage::BeforeOrderedAuthorityRead);
        let (before_session, before_revision) = match mux.topology_snapshot_authority() {
            Ok(authority) => authority,
            Err(_) => return Ok(codec::ListPanesOrderedV1Outcome::RevisionExhausted),
        };
        if before_revision.get() == u64::MAX {
            return Ok(codec::ListPanesOrderedV1Outcome::RevisionExhausted);
        }

        let mut window_ids = mux.iter_windows();
        window_ids.sort_unstable();
        checked_ordered_snapshot_window_count(window_ids.len())?;
        observer(ListPanesSnapshotStage::WindowsEnumerated);

        let mut frozen_windows = Vec::with_capacity(window_ids.len());
        let mut total_tabs = 0usize;
        for window_id in window_ids {
            let frozen = mux.get_window(window_id).map(|window| {
                let order = window.order_snapshot()?;
                Ok::<_, mux::window::WindowOrderSnapshotError>(FrozenOrderedSnapshotWindow {
                    title: window.get_title().to_string(),
                    workspace: window.get_workspace().to_string(),
                    order,
                })
            });
            let Some(frozen) = frozen else {
                // A concurrent removal advances the topology revision. The
                // final authority read rejects this partial attempt.
                continue;
            };
            let frozen = frozen
                .with_context(|| format!("freezing ordered state for mux window {window_id}"))?;
            total_tabs =
                checked_ordered_snapshot_tab_total(total_tabs, frozen.order.ordered_tabs().len())?;
            frozen_windows.push(frozen);
        }
        observer(ListPanesSnapshotStage::OrderedWindowsFrozen);

        let mut ordered_windows = Vec::with_capacity(frozen_windows.len());
        for frozen in &frozen_windows {
            ordered_windows.push(
                ordered_window_adapter::mux_frozen_window_order_to_codec(&frozen.order)
                    .context("converting frozen mux window order for PDU87")?,
            );
        }

        let mut pane_trees = Vec::with_capacity(total_tabs);
        let mut pane_nodes = Vec::new();
        let mut pane_window_titles = Vec::with_capacity(frozen_windows.len());
        let mut floating_panes = Vec::new();
        for (frozen, ordered_window) in frozen_windows.iter().zip(&ordered_windows) {
            let window_id = frozen.order.window_id();
            pane_window_titles.push(mux::tab::PaneArenaWindowTitle {
                window_id: ordered_window.window_id.get(),
                title: frozen.title.clone(),
            });
            for tab in frozen.order.ordered_tabs() {
                // Admit no more than one tree's wire budget from the current
                // arena position. The codec validator remains authoritative,
                // but enforcing its per-tree ceiling here prevents a single
                // oversized tab from walking and allocating toward the much
                // larger whole-snapshot ceiling before it is rejected.
                let tab_node_ceiling = ordered_snapshot_tab_node_ceiling(pane_nodes.len())?;
                pane_trees.push(tab.append_codec_pane_arena_in_window(
                    window_id,
                    &frozen.workspace,
                    &mut pane_nodes,
                    codec::MAX_ORDERED_PANE_TREE_DEPTH,
                    tab_node_ceiling,
                    codec::MAX_ORDERED_PANE_CENSUS_WORK_PER_TREE,
                )?);
                append_floating_pane_snapshot(
                    &mut floating_panes,
                    window_id,
                    tab.as_ref(),
                    &frozen.workspace,
                )?;
                observer(ListPanesSnapshotStage::TabTreeCaptured);
            }
        }
        observer(ListPanesSnapshotStage::TitlesCaptured);

        let (after_session, after_revision) = match mux.topology_snapshot_authority() {
            Ok(authority) => authority,
            Err(_) => return Ok(codec::ListPanesOrderedV1Outcome::RevisionExhausted),
        };
        if after_revision.get() == u64::MAX {
            return Ok(codec::ListPanesOrderedV1Outcome::RevisionExhausted);
        }
        if before_session != after_session {
            return Err(anyhow!(
                "mux session incarnation changed while constructing an ordered pane snapshot"
            ));
        }
        if before_revision == after_revision {
            let panes = mux::tab::PaneArena::from_unvalidated_parts(
                pane_trees,
                pane_nodes,
                pane_window_titles,
            );
            let snapshot = codec::OrderedPaneSnapshotV1 {
                session_incarnation: after_session,
                topology_revision: after_revision,
                panes,
                floating_panes,
                ordered_windows,
            };
            // The dispatch coordinator is the sole PDU87 authority and runs
            // both schema and pane/order cross-projection validation exactly
            // once, outside its state lock. Everything here is constructed
            // from the same frozen tab identities, so rescanning q elements at
            // the producer would add hot-path work without adding authority.
            metrics::histogram!("mux.server.ordered_snapshot.attempts").record(attempt as f64);
            metrics::counter!(
                "mux.server.ordered_snapshot.total",
                "outcome" => "snapshot"
            )
            .increment(1);
            return Ok(codec::ListPanesOrderedV1Outcome::Snapshot(snapshot));
        }

        first_revision.get_or_insert(before_revision);
        last_revision = Some(after_revision);
        metrics::counter!(
            "mux.server.ordered_snapshot.total",
            "outcome" => "retry"
        )
        .increment(1);
    }

    metrics::histogram!("mux.server.ordered_snapshot.attempts")
        .record(COHERENT_SNAPSHOT_ATTEMPTS as f64);
    metrics::counter!(
        "mux.server.ordered_snapshot.total",
        "outcome" => "contended"
    )
    .increment(1);
    Ok(codec::ListPanesOrderedV1Outcome::Contended {
        attempts: COHERENT_SNAPSHOT_ATTEMPTS,
        first_revision: first_revision
            .expect("an ordered contended snapshot records its first observed revision"),
        last_revision: last_revision
            .expect("an ordered contended snapshot records its last observed revision"),
    })
}

const fn ordered_snapshot_foundation() -> TopologyCapabilities {
    TopologyCapabilities::from_bits(
        TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
            | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
    )
}

enum ReorderAuthorization {
    Proceed,
    Terminal(codec::ReorderWindowTabsV1Outcome),
}

fn admit_reorder_transport(
    request: &codec::ReorderWindowTabsV1,
    established: EstablishedOrderedWindowAuthority,
) -> anyhow::Result<()> {
    if !established
        .negotiated()
        .contains(TopologyCapabilities::WINDOW_REORDER_CAS_V1)
    {
        return Err(anyhow!(
            "ordered-window reorder capability is not established on this stream"
        ));
    }
    if request.stream_id != established.stream_id() {
        return Err(anyhow!(
            "ordered-window reorder targets a stale or foreign topology stream"
        ));
    }
    Ok(())
}

fn authorize_reorder_identity(
    request: &codec::ReorderWindowTabsV1,
    established: EstablishedOrderedWindowAuthority,
) -> ReorderAuthorization {
    if request.session_incarnation != established.session_incarnation() {
        return ReorderAuthorization::Terminal(codec::ReorderWindowTabsV1Outcome::StaleIncarnation);
    }
    if request.domain_binding_id != established.domain_binding_id() {
        return ReorderAuthorization::Terminal(codec::ReorderWindowTabsV1Outcome::Malformed);
    }
    ReorderAuthorization::Proceed
}

fn process_list_panes_ordered_request(
    mux: &Mux,
    stream_id: TopologyStreamId,
    runtime_supported: TopologyCapabilities,
    request: &codec::ListPanesOrderedV1,
) -> anyhow::Result<codec::ListPanesOrderedV1Response> {
    request
        .validate()
        .context("validating PDU86 ordered snapshot request")?;
    runtime_supported
        .validate()
        .context("validating runtime ordered-window capability mask")?;
    let negotiated = request.supported.intersection(runtime_supported);
    let outcome = if negotiated.contains(ordered_snapshot_foundation())
        && negotiated.contains(request.required)
    {
        collect_ordered_list_panes_snapshot(mux)?
    } else {
        codec::ListPanesOrderedV1Outcome::Unsupported {
            supported: runtime_supported,
        }
    };
    let response = codec::ListPanesOrderedV1Response {
        protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
        domain_binding_id: request.domain_binding_id,
        negotiated,
        stream_id,
        outcome,
    };
    // Request correlation plus the potentially q-sized response and
    // cross-projection validation are centralized in dispatch immediately
    // before it can mint authority or admit this PDU87 to the FIFO.
    Ok(response)
}

fn process_reorder_window_tabs_request(
    mux: &Mux,
    request: &codec::ReorderWindowTabsV1,
    established: EstablishedOrderedWindowAuthority,
    client_id: Option<&Arc<ClientId>>,
) -> anyhow::Result<codec::ReorderWindowTabsV1Response> {
    admit_reorder_transport(request, established)?;
    let mux_request = ordered_window_adapter::codec_reorder_request_to_mux(request)
        .context("validating and converting PDU88 before mux mutation")?;
    let authoritative_request_digest = mux_request.request_digest();
    let authorization = authorize_reorder_identity(request, established);
    let outcome = match authorization {
        ReorderAuthorization::Proceed => {
            let result = mux.reorder_window_tabs(mux_request);
            let counts_as_client_activity = matches!(
                &result,
                mux::ReorderWindowTabsResult::Decision(
                    mux::WindowReorderTerminalOutcome::Applied(_)
                        | mux::WindowReorderTerminalOutcome::Conflict(_)
                        | mux::WindowReorderTerminalOutcome::Exhausted
                ) | mux::ReorderWindowTabsResult::Replay(
                    mux::WindowReorderTerminalOutcome::Applied(_)
                        | mux::WindowReorderTerminalOutcome::Conflict(_)
                        | mux::WindowReorderTerminalOutcome::Exhausted
                )
            );
            if counts_as_client_activity && let Some(client_id) = client_id {
                let _ = mux.client_had_input_if_same(client_id);
            }
            ordered_window_adapter::mux_reorder_result_to_codec(&result)
                .context("converting mux reorder decision for PDU89")?
        }
        ReorderAuthorization::Terminal(outcome) => outcome,
    };
    let response = codec::ReorderWindowTabsV1Response {
        protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
        stream_id: request.stream_id,
        session_incarnation: request.session_incarnation,
        mutation_id: request.mutation_id,
        request_digest: codec::WindowReorderDigest::from_bytes(
            authoritative_request_digest.as_bytes(),
        ),
        outcome,
    };
    response
        .validate()
        .context("validating complete PDU89 before enqueue")?;
    Ok(response)
}

fn with_current_pane<R>(
    authority: &SessionAuthority,
    registration: &PaneRegistrationHandle,
    f: impl FnOnce(&CurrentPane<'_>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    authority.try_run(|| {
        registration
            .try_with_current(|current| f(&current))
            .ok_or_else(|| {
                anyhow!(
                    "pane registration {} is no longer current",
                    registration.pane_id()
                )
            })?
    })?
}

/// Sample one exact pane registration without allowing a faulty pane callback
/// to fail or unwind sibling entries in a bounded fleet-health response.
fn sample_tiered_scrollback_status(
    pane_id: PaneId,
    registration: Option<PaneRegistrationHandle>,
) -> PaneTieredScrollbackStatusOutcomeV1 {
    let sampled = catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| {
            let Some(registration) = registration else {
                return PaneTieredScrollbackStatusOutcomeV1::Missing;
            };
            match registration.try_with_current(|current| current.get_tiered_scrollback_status()) {
                Some(Some(status)) => PaneTieredScrollbackStatusOutcomeV1::Available(status.into()),
                Some(None) => PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                None => PaneTieredScrollbackStatusOutcomeV1::Closed,
            }
        }),
    );
    match sampled {
        Ok(outcome) => outcome,
        Err(_error) => {
            log::warn!("tiered scrollback health callback panicked for pane {pane_id}");
            PaneTieredScrollbackStatusOutcomeV1::CallbackPanicked
        }
    }
}

fn record_tiered_scrollback_batch_outcomes(entries: &[PaneTieredScrollbackStatusEntryV1]) {
    let mut available = 0_u64;
    let mut unavailable = 0_u64;
    let mut missing = 0_u64;
    let mut closed = 0_u64;
    let mut panicked = 0_u64;
    for entry in entries {
        match entry.outcome {
            PaneTieredScrollbackStatusOutcomeV1::Available(_) => {
                available = available.saturating_add(1);
            }
            PaneTieredScrollbackStatusOutcomeV1::Unavailable => {
                unavailable = unavailable.saturating_add(1);
            }
            PaneTieredScrollbackStatusOutcomeV1::Missing => {
                missing = missing.saturating_add(1);
            }
            PaneTieredScrollbackStatusOutcomeV1::Closed => {
                closed = closed.saturating_add(1);
            }
            PaneTieredScrollbackStatusOutcomeV1::CallbackPanicked => {
                panicked = panicked.saturating_add(1);
            }
        }
    }
    metrics::counter!("mux.server.tiered_scrollback_batch_outcomes", "outcome" => "available")
        .increment(available);
    metrics::counter!("mux.server.tiered_scrollback_batch_outcomes", "outcome" => "unavailable")
        .increment(unavailable);
    metrics::counter!("mux.server.tiered_scrollback_batch_outcomes", "outcome" => "missing")
        .increment(missing);
    metrics::counter!("mux.server.tiered_scrollback_batch_outcomes", "outcome" => "closed")
        .increment(closed);
    metrics::counter!("mux.server.tiered_scrollback_batch_outcomes", "outcome" => "panicked")
        .increment(panicked);
    if unavailable
        .saturating_add(missing)
        .saturating_add(closed)
        .saturating_add(panicked)
        > 0
    {
        metrics::counter!("mux.server.tiered_scrollback_partial_batches").increment(1);
    }
}

fn unregister_owned_client(mux: &Mux, client_id: &Arc<ClientId>) {
    let _ = mux.unregister_client_if_same(client_id);
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    owner: SessionOwner,
    topology_stream_id: TopologyStreamId,
    trace_producer: Option<Rc<SessionTraceProducer>>,
    per_pane: HashMap<PaneId, TrackedPane>,
    client_id: Option<Arc<ClientId>>,
    #[cfg(test)]
    client_input_activity_updates: usize,
    proxy_client_id: Option<ClientId>,
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        self.owner.retire();
        if let Some(client_id) = self.client_id.take() {
            unregister_owned_client(self.owner.mux(), &client_id);
        }
    }
}

impl SessionHandler {
    /// Construct a handler bound to one explicit mux incarnation.
    ///
    /// Production and fuzz callers must provide the owning mux rather than
    /// consulting the process-global singleton. Deferred work therefore cannot
    /// be redirected if another mux is installed while the session is alive.
    pub fn new_for_mux(to_write_tx: PduSender, mux: Arc<Mux>) -> Self {
        Self::new_for_session(to_write_tx, SessionOwner::new(mux))
    }

    pub(crate) fn new_for_session(to_write_tx: PduSender, owner: SessionOwner) -> Self {
        let topology_stream_id = TopologyStreamId::from_bytes(*uuid::Uuid::new_v4().as_bytes());
        Self::new_for_session_with_topology_stream(to_write_tx, owner, topology_stream_id)
    }

    pub(crate) fn new_for_session_with_topology_stream(
        to_write_tx: PduSender,
        owner: SessionOwner,
        topology_stream_id: TopologyStreamId,
    ) -> Self {
        Self::new_for_session_with_topology_stream_and_trace(
            to_write_tx,
            owner,
            topology_stream_id,
            None,
        )
    }

    pub(crate) fn new_for_session_with_topology_stream_and_trace(
        to_write_tx: PduSender,
        owner: SessionOwner,
        topology_stream_id: TopologyStreamId,
        trace_producer: Option<Rc<SessionTraceProducer>>,
    ) -> Self {
        Self {
            to_write_tx,
            owner,
            topology_stream_id,
            trace_producer,
            per_pane: HashMap::new(),
            client_id: None,
            #[cfg(test)]
            client_input_activity_updates: 0,
            proxy_client_id: None,
        }
    }

    #[cfg(test)]
    fn new(to_write_tx: PduSender) -> Self {
        let mux_owner = Mux::try_get().unwrap_or_else(|| Arc::new(Mux::new(None)));
        Self::new_for_session(to_write_tx, SessionOwner::new(mux_owner))
    }

    pub(crate) fn record_decoded_input_trace(
        &self,
        admission: &mut AdmittedInputTraceV1,
        decode_started_at: Instant,
        decode_completed_at: Instant,
    ) {
        let Some(trace_producer) = self.trace_producer.as_ref() else {
            return;
        };
        if trace_producer.topology_stream_id != admission.stream_id {
            metrics::counter!(
                "mux.server.trace_remote_admission",
                "outcome" => "connection_generation_mismatch"
            )
            .increment(1);
            return;
        }
        let Ok(session) = self.owner.authority().acquire() else {
            metrics::counter!(
                "mux.server.trace_remote_admission",
                "outcome" => "session_retired"
            )
            .increment(1);
            return;
        };
        let Some((_, window_id, tab_id)) = session.resolve_pane_id(admission.pane_id) else {
            metrics::counter!(
                "mux.server.trace_remote_admission",
                "outcome" => "pane_location_unresolved"
            )
            .increment(1);
            return;
        };
        let (Ok(window_id), Ok(tab_id), Ok(pane_id)) = (
            u64::try_from(window_id),
            u64::try_from(tab_id),
            u64::try_from(admission.pane_id),
        ) else {
            metrics::counter!(
                "mux.server.trace_remote_admission",
                "outcome" => "topology_id_exhausted"
            )
            .increment(1);
            return;
        };
        drop(session);

        let Some(token) = trace_producer.admit_remote_trace(admission.context) else {
            return;
        };
        let topology = InteractionTraceTopology {
            window_id,
            tab_id,
            pane_id,
        };
        trace_producer.record_server_stage(
            token,
            RendererKeypressTraceStage::ServerReadableDecode,
            topology,
            decode_started_at,
            decode_completed_at,
        );
        admission.recorder_token = Some(token);
        admission.topology = Some(topology);
        // K4 ends at decode completion; K5 begins at that same boundary so
        // request validation, exact-topology admission, recorder publication,
        // scheduling, and mux-main queue wait cannot disappear into an
        // unmeasured gap between the two server stages.
        admission.dispatch_queued_at = Some(decode_completed_at);
    }

    fn per_pane_for_registration(
        &mut self,
        registration: &PaneRegistrationHandle,
    ) -> Arc<Mutex<PerPane>> {
        let pane_id = registration.pane_id();
        let tracked = self
            .per_pane
            .entry(pane_id)
            .and_modify(|tracked| {
                let is_same = tracked
                    .registration
                    .as_ref()
                    .is_some_and(|current| current.same_registration(registration));
                if !is_same {
                    *tracked = TrackedPane::exact(registration.clone());
                }
            })
            .or_insert_with(|| TrackedPane::exact(registration.clone()));
        Arc::clone(&tracked.state)
    }

    #[cfg(test)]
    fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        let tracked = self.per_pane.entry(pane_id).or_insert_with(|| TrackedPane {
            registration: None,
            state: Arc::new(Mutex::new(PerPane::default())),
        });
        Arc::clone(&tracked.state)
    }

    /// Non-inserting accessor for cached per-pane state (ft-12e8l).
    ///
    /// Unlike [`per_pane`], this does NOT create an entry when the pane is
    /// untracked. Use this on push-side paths (e.g. late-arriving Alert
    /// notifications from mux) where silently re-creating an entry for a
    /// pane that was already removed via `PaneRemoved` produces a permanent
    /// map leak — no subsequent `PaneRemoved` ever fires for a dead pane.
    pub(crate) fn per_pane_if_present(&self, pane_id: PaneId) -> Option<Arc<Mutex<PerPane>>> {
        self.per_pane
            .get(&pane_id)
            .map(|tracked| Arc::clone(&tracked.state))
    }

    /// Remove cached per-pane state when a pane is destroyed.
    /// Prevents unbounded HashMap growth in long-lived sessions.
    pub(crate) fn remove_per_pane(&mut self, pane_id: PaneId) {
        let should_remove = self.per_pane.get(&pane_id).is_some_and(|tracked| {
            tracked
                .registration
                .as_ref()
                .is_none_or(|registration| registration.try_with_current(|_| ()).is_none())
        });
        if should_remove {
            self.per_pane.remove(&pane_id);
        }
    }

    fn remove_per_pane_if_same(&mut self, registration: &PaneRegistrationHandle) {
        let pane_id = registration.pane_id();
        let should_remove = self
            .per_pane
            .get(&pane_id)
            .and_then(|tracked| tracked.registration.as_ref())
            .is_some_and(|current| current.same_registration(registration));
        if should_remove {
            self.per_pane.remove(&pane_id);
        }
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let authority = self.owner.authority();
        let registration = match authority.capture_current_pane(pane_id) {
            Ok(registration) => registration,
            Err(err) => {
                log::debug!("skipping pane {pane_id} push: {err:#}");
                return;
            }
        };
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane_for_registration(&registration);
        Self::schedule_pane_push_with_state(sender, authority, registration, per_pane);
    }

    /// Push cached pane changes only for panes this session already tracks.
    ///
    /// Mux notifications can arrive after `PaneRemoved`; those notifications
    /// must not recreate `per_pane` state for a pane that will never emit a
    /// later removal notification. Client request paths still use
    /// `schedule_pane_push`, which intentionally creates first-use state.
    pub(crate) fn schedule_tracked_pane_push(&self, pane_id: PaneId) {
        if let Some(tracked) = self.per_pane.get(&pane_id) {
            let Some(registration) = tracked.registration.clone() else {
                return;
            };
            let sender = self.to_write_tx.clone();
            let per_pane = Arc::clone(&tracked.state);
            Self::schedule_pane_push_with_state(
                sender,
                self.owner.authority(),
                registration,
                per_pane,
            );
        }
    }

    fn schedule_pane_push_with_state(
        sender: PduSender,
        authority: SessionAuthority,
        registration: PaneRegistrationHandle,
        per_pane: Arc<Mutex<PerPane>>,
    ) {
        spawn_into_main_thread(async move {
            let pane_id = registration.pane_id();
            let result = authority.try_run(|| {
                registration
                    .try_with_current(|pane| maybe_push_pane_changes(&pane, sender, per_pane))
                    .ok_or_else(|| anyhow!("pane registration {} is no longer current", pane_id))?
            });
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) | Err(err) => {
                    log::error!("scheduled pane {pane_id} render push failed: {err:#}");
                }
            }
        })
        .detach();
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        self.process_one_with_dispatch_authority(decoded, None, None);
    }

    pub(crate) fn process_one_with_dispatch_authority(
        &mut self,
        decoded: DecodedPdu,
        ordered_window_authority: Option<EstablishedOrderedWindowAuthority>,
        input_trace_authority: Option<AdmittedInputTraceV1>,
    ) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;
        let authority = self.owner.authority();
        let response_authority = authority.clone();
        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            let _ = response_authority.try_run(|| sender.send_control(DecodedPdu { pdu, serial }));
        };

        let trace_validation = match (&decoded.pdu, input_trace_authority.as_ref()) {
            (Pdu::SendKeyDownTracedV1(request), Some(admission)) => {
                admission.validate_for_request(request, self.topology_stream_id)
            }
            (Pdu::SendKeyDownTracedV1(_), None) => Err(anyhow!(
                "sampled key input lacks server admission authority"
            )),
            (_, Some(_)) => Err(anyhow!(
                "sampled key input authority was attached to an unrelated request"
            )),
            (_, None) => Ok(()),
        };
        if let Err(err) = trace_validation {
            send_response(Err(err));
            return;
        }

        if let Some(client_id) = &self.client_id {
            if decoded.pdu.is_user_input() && !matches!(&decoded.pdu, Pdu::ReorderWindowTabsV1(_)) {
                // PDU88 is marked only inside its handler, after the exact
                // stream/session/capability/domain checks. An unauthorized
                // reorder must not mutate even client-activity bookkeeping.
                match self.owner.authority().acquire() {
                    Ok(mux) => {
                        #[cfg(not(test))]
                        {
                            let _ = mux.client_had_input_if_same(client_id);
                        }
                        #[cfg(test)]
                        if mux.client_had_input_if_same(client_id) {
                            self.client_input_activity_updates = self
                                .client_input_activity_updates
                                .checked_add(1)
                                .expect("test client-input activity counter overflow");
                        }
                    }
                    Err(err) => {
                        log::warn!("dropped client input activity marker: {err:#}");
                    }
                }
            }
        }

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        fn capture_pane_or_respond<SND>(
            authority: &SessionAuthority,
            pane_id: PaneId,
            send_response: &SND,
        ) -> Option<PaneRegistrationHandle>
        where
            SND: Fn(anyhow::Result<Pdu>),
        {
            match authority.capture_current_pane(pane_id) {
                Ok(registration) => Some(registration),
                Err(err) => {
                    send_response(Err(err));
                    None
                }
            }
        }

        fn capture_pane_or_respond_liveness<SND>(
            authority: &SessionAuthority,
            pane_id: PaneId,
            send_response: &SND,
        ) -> Option<PaneRegistrationHandle>
        where
            SND: Fn(anyhow::Result<Pdu>),
        {
            match authority.capture_current_pane_opt(pane_id) {
                Ok(Some(registration)) => Some(registration),
                Ok(None) => {
                    send_response(Ok(Pdu::LivenessResponse(LivenessResponse {
                        pane_id,
                        is_alive: false,
                    })));
                    None
                }
                Err(err) => {
                    send_response(Err(err));
                    None
                }
            }
        }

        let (pdu, admitted_input_trace) = match decoded.pdu {
            Pdu::SendKeyDownTracedV1(traced) => {
                let (request, trace_context) = traced.into_parts();
                let Some(admission) = input_trace_authority else {
                    send_response(Err(anyhow!(
                        "sampled key input admission authority vanished before dispatch"
                    )));
                    return;
                };
                debug_assert_eq!(admission.context(), trace_context);
                debug_assert_eq!(admission.stream_id(), self.topology_stream_id);
                (Pdu::SendKeyDown(request), Some(admission))
            }
            pdu => (pdu, None),
        };

        match pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                            window.set_workspace(&workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetActiveWorkspace(SetActiveWorkspace { workspace }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("set active workspace before SetClientId")
                            })?;
                            let mux = session_mux(&authority)?;
                            if !mux.set_active_workspace_for_client_if_same(&client_id, &workspace)
                            {
                                return Err(anyhow!("client registration is no longer current"));
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetClientId(SetClientId {
                mut client_id,
                is_proxy,
            }) => {
                if is_proxy {
                    if self.proxy_client_id.is_none() {
                        // Copy proxy identity, but don't assign it to the mux;
                        // we'll use it to annotate the actual clients own
                        // identity when they send it
                        self.proxy_client_id.replace(client_id);
                    }
                } else {
                    // If this session is a proxy, override the incoming id with
                    // the proxy information so that it is clear what is going
                    // on from the `wezterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock.clone_from(&proxy_id.ssh_auth_sock);
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }

                    let client_id = Arc::new(client_id);
                    let mux = match session_mux(&authority) {
                        Ok(mux) => mux,
                        Err(err) => {
                            send_response(Err(err));
                            return;
                        }
                    };
                    let registration_is_current = self.client_id.as_ref().is_some_and(|current| {
                        current.as_ref() == client_id.as_ref()
                            && mux.client_registration_is_current(current)
                    });
                    if !registration_is_current {
                        if let Some(prior_client_id) = self.client_id.take() {
                            unregister_owned_client(&mux, &prior_client_id);
                        }
                        mux.register_client(Arc::clone(&client_id));
                        self.client_id = Some(client_id);
                    }
                }
                send_response(Ok(Pdu::UnitResponse(UnitResponse {})));
            }
            Pdu::SetFocusedPane(SetFocusedPane { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            authority.try_run(|| {
                                registration
                                    .try_with_current(|current| {
                                        current.focus_for_client_if_same(client_id.as_ref())?;
                                        Ok(Pdu::UnitResponse(UnitResponse {}))
                                    })
                                    .ok_or_else(|| {
                                        anyhow!("pane registration {pane_id} is no longer current")
                                    })?
                            })?
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::GetClientList(GetClientList) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let clients = mux.iter_clients();
                            Ok(Pdu::GetClientListResponse(GetClientListResponse {
                                clients,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanes(ListPanes {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            Ok(Pdu::ListPanesResponse(collect_list_panes_snapshot(&mux)?))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanesCoherent(ListPanesCoherent {
                supported,
                required,
            }) => {
                let stream_id = self.topology_stream_id;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let negotiated =
                                supported.intersection(TopologyCapabilities::SERVER_SUPPORTED);
                            let outcome = if negotiated
                                .contains(TopologyCapabilities::FENCED_SNAPSHOT_V1)
                                && negotiated.contains(required)
                            {
                                let mux = session_mux(&authority)?;
                                collect_coherent_list_panes_snapshot(&mux)?
                            } else {
                                ListPanesCoherentOutcome::Unsupported {
                                    supported: TopologyCapabilities::SERVER_SUPPORTED,
                                }
                            };
                            Ok(Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                                negotiated,
                                stream_id,
                                outcome,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanesOrderedV1(request) => {
                let stream_id = self.topology_stream_id;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let response = process_list_panes_ordered_request(
                                &mux,
                                stream_id,
                                TopologyCapabilities::SERVER_SUPPORTED,
                                &request,
                            )?;
                            Ok(Pdu::ListPanesOrderedV1Response(response))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ReorderWindowTabsV1(request) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let ordered_window_authority = ordered_window_authority.ok_or_else(|| {
                                anyhow!(
                                    "ordered-window stream has not been established by a dispatched PDU87"
                                )
                            })?;
                            let mux = session_mux(&authority)?;
                            let response = process_reorder_window_tabs_request(
                                &mux,
                                &request,
                                ordered_window_authority,
                                client_id.as_ref(),
                            )?;
                            Ok(Pdu::ReorderWindowTabsV1Response(response))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanesTabStacks(ListPanesTabStacks {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let mut tab_stack_entries = vec![];
                            for window_id in mux.iter_windows() {
                                let Some(window) = mux.get_window(window_id) else {
                                    log::warn!(
                                        "ListPanesTabStacks skipped stale window id {} from iter_windows",
                                        window_id
                                    );
                                    continue;
                                };
                                tab_stack_entries.extend(window.tab_stack_entries().into_iter().map(
                                    |entry| ListPanesTabStackEntry {
                                        window_id,
                                        stack_id: entry.stack_id,
                                        tab_id: entry.tab_id,
                                        position: entry.position,
                                        is_visible: entry.is_visible,
                                    },
                                ));
                            }
                            log::trace!("ListPanesTabStacks {tab_stack_entries:#?}");
                            Ok(Pdu::ListPanesTabStacksResponse(
                                ListPanesTabStacksResponse { tab_stack_entries },
                            ))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            mux.rename_workspace(&old_workspace, &new_workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.write_all(&data)?;
                                push_pane_changes_after_committed_input(
                                    pane, sender, per_pane, "write",
                                );
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.erase_scrollback(erase_mode);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::KillPane(KillPane { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                self.remove_per_pane_if_same(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let retired = authority.try_run(|| registration.retire_if_current())?;
                            if !retired {
                                return Err(anyhow!(
                                    "pane registration {pane_id} is no longer current"
                                ));
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste {
                pane_id,
                data,
                input_serial,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.send_paste(&data)?;
                                push_input_dispatch_changes_after_committed_input(
                                    pane,
                                    sender,
                                    per_pane,
                                    input_serial,
                                    "paste",
                                );
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };

                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = async {
                            let session = authority.acquire()?;
                            let results = registration
                                .search_if_current(session.mux(), pattern, range, limit)
                                .await
                                .ok_or_else(|| {
                                    anyhow!("pane registration {pane_id} is no longer current")
                                })??;
                            Ok(Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                                results,
                            }))
                        }
                        .await;
                        send_response(result);
                    })
                    .detach();
                })
                .detach();
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.set_zoomed_in_tab(containing_tab_id, zoomed)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let pane_id = pane.pane_in_direction(direction)?;
                                Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                    pane_id,
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.activate_pane_direction(direction)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.resize_in_tab(containing_tab_id, size)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                let trace_authority = admitted_input_trace;
                let trace_producer = self.trace_producer.as_ref().map(Rc::clone);
                promise::spawn::spawn(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                if let (Some(producer), Some(admission)) =
                                    (trace_producer.as_ref(), trace_authority.as_ref())
                                {
                                    // End K5 only after the queued task has
                                    // revalidated the exact session and pane
                                    // registration. A retired session or
                                    // replaced pane therefore cannot fabricate
                                    // a performed mux-dispatch stage.
                                    producer.record_mux_dispatch_start(admission, Instant::now());
                                }
                                pane.key_down(event.key, event.modifiers)?;
                                push_input_dispatch_changes_after_committed_input(
                                    pane,
                                    sender,
                                    per_pane,
                                    input_serial,
                                    "key-down",
                                );
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendKeyUp(SendKeyUp { pane_id, event }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.key_up(event.key, event.modifiers)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.mouse_event(event)?;
                                push_pane_changes_after_committed_input(
                                    pane,
                                    sender,
                                    per_pane,
                                    "mouse event",
                                );
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SpawnV2(spawn) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_domain_spawn_v2(authority, spawn, send_response, client_id);
                })
                .detach();
            }

            Pdu::SplitPane(split) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, split.pane_id, &send_response)
                else {
                    return;
                };
                let move_registration = match split.move_pane_id {
                    Some(move_pane_id) => {
                        let Some(registration) =
                            capture_pane_or_respond(&authority, move_pane_id, &send_response)
                        else {
                            return;
                        };
                        Some(registration)
                    }
                    None => None,
                };
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_split_pane(
                        authority,
                        registration,
                        move_registration,
                        split,
                        send_response,
                        client_id,
                    );
                })
                .detach();
            }

            Pdu::MovePaneToNewTab(request) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, request.pane_id, &send_response)
                else {
                    return;
                };
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_move_pane(authority, registration, request, send_response, client_id);
                })
                .detach();
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let cursor_position = pane.get_cursor_position();
                                let dimensions = pane.get_dimensions();
                                Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                    GetPaneRenderableDimensionsResponse {
                                        pane_id,
                                        cursor_position,
                                        dimensions,
                                        tiered_scrollback_status: pane
                                            .get_tiered_scrollback_status(),
                                    },
                                ))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneTieredScrollbackStatusesV1(request) => {
                if let Err(error) = request.validate() {
                    send_response(Err(error.into()));
                    return;
                }
                let queued_at = Instant::now();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let queue_delay = queued_at.elapsed();
                            metrics::counter!("mux.server.tiered_scrollback_batch_requests")
                                .increment(1);
                            metrics::histogram!(
                                "mux.server.tiered_scrollback_batch_queue_delay_ms"
                            )
                            .record(queue_delay.as_secs_f64() * 1_000.0);
                            metrics::histogram!("mux.server.tiered_scrollback_batch_panes")
                                .record(u32::try_from(request.pane_ids.len()).unwrap_or(u32::MAX));

                            let snapshot_started_at = Instant::now();
                            let session = authority.acquire()?;
                            // Freeze registration identity for the complete request before
                            // invoking any pane callback. A callback for an earlier pane can
                            // therefore retire a later registration, but cannot change a pane
                            // that existed at turn admission from `Closed` into `Missing`.
                            // The intermediate collection is semantically required: fusing the
                            // iterators would interleave registration capture with callbacks.
                            #[allow(clippy::needless_collect)]
                            let registrations = request
                                .pane_ids
                                .into_iter()
                                .map(|pane_id| (pane_id, session.capture_current_pane(pane_id)))
                                .collect::<Vec<_>>();
                            let entries = registrations
                                .into_iter()
                                .map(
                                    |(pane_id, registration)| PaneTieredScrollbackStatusEntryV1 {
                                        pane_id,
                                        outcome: sample_tiered_scrollback_status(
                                            pane_id,
                                            registration,
                                        ),
                                    },
                                )
                                .collect::<Vec<_>>();
                            metrics::histogram!("mux.server.tiered_scrollback_batch_snapshot_ms")
                                .record(snapshot_started_at.elapsed().as_secs_f64() * 1_000.0);
                            record_tiered_scrollback_batch_outcomes(&entries);
                            let response = GetPaneTieredScrollbackStatusesV1Response { entries };
                            response.validate()?;
                            Ok(Pdu::GetPaneTieredScrollbackStatusesV1Response(response))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let Some(registration) =
                    capture_pane_or_respond_liveness(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let is_alive = authority
                                .try_run(|| {
                                    registration
                                        .try_with_current(|current| {
                                            maybe_push_pane_changes(&current, sender, per_pane)
                                        })
                                        .transpose()
                                })??
                                .is_some();
                            Ok(Pdu::LivenessResponse(LivenessResponse {
                                pane_id,
                                is_alive,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let mut lines_and_indices = vec![];

                                for range in lines {
                                    let (first_row, lines) = pane.get_lines(range);
                                    for (idx, mut line) in lines.into_iter().enumerate() {
                                        let Some(stable_row) = stable_row_offset(first_row, idx)
                                        else {
                                            break;
                                        };
                                        line.compress_for_scrollback();
                                        lines_and_indices.push((stable_row, line));
                                    }
                                }
                                Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                    pane_id,
                                    lines: lines_and_indices.into(),
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetSemanticZones(GetSemanticZones { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let (zones, zone_texts, last_exit_code) =
                                    pane.semantic_snapshot()?;
                                Ok(Pdu::GetSemanticZonesResponse(GetSemanticZonesResponse {
                                    pane_id,
                                    zones,
                                    zone_texts,
                                    last_exit_code,
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let mut data = None;

                                let lines = match stable_row_range_from_len(line_idx, 1) {
                                    Some(line_range) => pane.get_lines(line_range).1,
                                    None => Vec::new(),
                                };
                                'found_data: for line in lines {
                                    if let Some(cell) = line.get_cell(cell_idx) {
                                        if let Some(images) = cell.attrs().images() {
                                            for im in images {
                                                if im.image_data().current_content_hash()
                                                    == data_hash
                                                {
                                                    data.replace(im.image_data().clone());
                                                    break 'found_data;
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                                    pane_id,
                                    data,
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetCodecVersion(_) => {
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
                                .map(Into::into),
                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                        })));
                    }
                }
            }

            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            if mux.get_window(window_id).is_none() {
                                return Err(anyhow!("no such window {window_id}"));
                            }
                            mux.set_window_title(window_id, &title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            if mux.get_tab(tab_id).is_none() {
                                return Err(anyhow!("no such tab {tab_id}"));
                            }
                            mux.set_tab_title(tab_id, &title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.set_client_palette(palette);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.adjust_pane_size(direction, amount)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id,
                pane_id,
                rect,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.create_floating_pane(tab_id, rect)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::MoveFloatingPane(MoveFloatingPane { pane_id, rect }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.move_floating_pane(rect)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SetFloatingPaneZ(SetFloatingPaneZ { pane_id, z_order }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.set_floating_pane_z_order(z_order)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::ToggleFloatingPane(ToggleFloatingPane { pane_id, visible }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.set_floating_pane_visible(visible)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.remove_floating_pane()?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SwapToLayout(SwapToLayout {
                tab_id,
                layout_index,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                tab.swap_to_layout_index(layout_index);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SetLayoutCycle(SetLayoutCycle {
                tab_id,
                layout_names,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                // Build layout cycle from named presets
                                let mut layouts = Vec::new();
                                for name in &layout_names {
                                    let layout = match name.as_str() {
                                        "grid-4" => mux::layout::grid_4(),
                                        "main-side" => mux::layout::main_side(),
                                        "stacked" => mux::layout::stacked(),
                                        "main-bottom" => mux::layout::main_bottom(),
                                        other => {
                                            return Err(anyhow!("unknown layout preset: {other}"));
                                        }
                                    };
                                    layouts.push(layout);
                                }
                                if layouts.is_empty() {
                                    return Err(anyhow!(
                                        "layout cycle must have at least one layout"
                                    ));
                                }
                                tab.set_layout_cycle(mux::layout::LayoutCycle::new(layouts));
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::CycleStack(CycleStack {
                tab_id,
                slot_index,
                forward,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                if forward {
                                    tab.cycle_stack(slot_index);
                                } else {
                                    tab.cycle_stack_backward(slot_index);
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SelectStackPane(SelectStackPane {
                tab_id,
                slot_index,
                pane_index,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                tab.select_stack_pane(slot_index, pane_index)
                                    .ok_or_else(|| {
                                        anyhow!(
                                            "stack slot {} or pane index {} not found in tab {}",
                                            slot_index,
                                            pane_index,
                                            tab_id
                                        )
                                    })?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id,
                min_width,
                max_width,
                min_height,
                max_height,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.update_pane_constraints(
                                        min_width, max_width, min_height, max_height,
                                    )?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::RenderApplicationResult(_) | Pdu::RenderApplicationResultV1(_) => {
                send_response(Err(anyhow!(
                    "render-application settlement received before the live delivery \
                     coordinator was activated for this connection"
                )));
            }
            Pdu::GetPaneRenderDeliveryV1(_) => {
                send_response(Err(anyhow!(
                    "exact render-delivery request received before its live retention and \
                     settlement coordinator was activated for this connection"
                )));
            }
            Pdu::Invalid { ident } => {
                send_response(Err(anyhow!("invalid PDU identifier {ident}")));
            }
            unexpected @ (Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::ListPanesCoherentResponse { .. }
            | Pdu::ListPanesTabStacksResponse { .. }
            | Pdu::ListPanesOrderedV1Response { .. }
            | Pdu::ReorderWindowTabsV1Response { .. }
            | Pdu::WindowOrderEventV1 { .. }
            | Pdu::GetPaneRenderDeliveryV1Response { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::RenderApplicationUpdateV1 { .. }
            | Pdu::RenderApplicationUpdate { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetSemanticZonesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::GetPaneTieredScrollbackStatusesV1Response { .. }
            | Pdu::ErrorResponse { .. }) => {
                send_response(Err(anyhow!("expected a request, got {unexpected:?}")));
            }
            unexpected @ Pdu::TopologyEvent { .. } => {
                send_response(Err(anyhow!(
                    "expected a request, got server-unilateral topology event {unexpected:?}",
                )));
            }
            Pdu::SendKeyDownTracedV1(_) => unreachable!(
                "sampled key input is normalized only after exact trace admission validation"
            ),
        }
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn_v2<SND>(
    authority: SessionAuthority,
    spawn: SpawnV2,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        send_response(domain_spawn_v2(authority, spawn, client_id).await);
    })
    .detach();
}

fn schedule_split_pane<SND>(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    move_registration: Option<PaneRegistrationHandle>,
    split: SplitPane,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        send_response(
            split_pane(authority, registration, move_registration, split, client_id).await,
        );
    })
    .detach();
}

async fn split_pane(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    move_registration: Option<PaneRegistrationHandle>,
    split: SplitPane,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let session = authority.acquire()?;

    let receipt = if let Some(move_pane_id) = split.move_pane_id {
        let move_registration = move_registration.as_ref().ok_or_else(|| {
            anyhow!("move pane registration {move_pane_id} was not admitted for split")
        })?;
        registration
            .split_moved_if_current(
                session.mux(),
                move_registration,
                split.split_request,
                split.domain,
                client_id,
            )
            .await
    } else {
        registration
            .split_spawned_if_current(
                session.mux(),
                split.split_request,
                split.command,
                split.command_dir,
                split.domain,
                client_id,
            )
            .await
    }
    .ok_or_else(|| {
        anyhow!(
            "pane registration {} is no longer current",
            registration.pane_id()
        )
    })??;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: receipt.pane_id(),
        tab_id: receipt.tab_id(),
        window_id: receipt.window_id(),
        size: receipt.size(),
    }))
}

async fn domain_spawn_v2(
    authority: SessionAuthority,
    spawn: SpawnV2,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let mux = session_mux(&authority)?;

    let (tab, pane, window_id) = mux
        .mux()
        .spawn_tab_or_window(
            spawn.window_id,
            spawn.domain,
            spawn.command,
            spawn.command_dir,
            spawn.size,
            None, // optional current pane_id
            spawn.workspace,
            None, // optional gui window position
            client_id,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

fn schedule_move_pane<SND>(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    request: MovePaneToNewTab,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        send_response(move_pane(authority, registration, request, client_id).await);
    })
    .detach();
}

async fn move_pane(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    request: MovePaneToNewTab,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let session = authority.acquire()?;
    let receipt = registration
        .move_to_new_tab_if_current(
            session.mux(),
            request.window_id,
            request.workspace_for_new_window,
            client_id,
        )
        .await
        .ok_or_else(|| {
            anyhow!(
                "pane registration {} is no longer current",
                registration.pane_id()
            )
        })??;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id: receipt.tab_id(),
        window_id: receipt.window_id(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::keyassignment::PaneDirection;
    use frankenterm_core_audit_types::interaction_flight_recorder_v1::{
        RecorderEpochId, RecorderMode, RecorderSamplerAlgorithm, RecorderSamplerConfigV1,
        SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
    };
    use frankenterm_core_audit_types::interaction_trace_v2::{
        InteractionTraceId, InteractionTracePath, InteractionTraceRunId,
    };
    use mux::domain::DomainId;
    use mux::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, WithPaneLines};
    use parking_lot::{MappedMutexGuard, Mutex as ParkingMutex, MutexGuard as ParkingMutexGuard};
    use promise::spawn::SimpleExecutor;
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::collections::{HashMap, HashSet};
    use std::ops::Range;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use termwiz::surface::Line;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::terminal::Progress;
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    fn trace_recorder(shard_count: u16) -> Arc<FlightRecorder> {
        let config = frankenterm_flight_recorder::RecorderConfig::new(
            RecorderEpochId {
                nonce_hi: 1,
                nonce_lo: 2,
            },
            InteractionTraceRunId {
                epoch_nonce_hi: 3,
                epoch_nonce_lo: 4,
            },
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            shard_count,
            u32::from(shard_count) * 4,
            8 * 1024 * 1024,
        )
        .expect("test trace-recorder configuration must be valid");
        FlightRecorder::new(config).expect("test trace recorder must allocate")
    }

    fn topology_stream(byte: u8) -> TopologyStreamId {
        TopologyStreamId::from_bytes([byte; 16])
    }

    #[test]
    fn dispatch_trace_authority_claims_bounded_shards_off_hot_path() {
        let authority = DispatchTraceAuthority::new(trace_recorder(2));
        let first = authority
            .claim_session(topology_stream(1))
            .expect("first connection must claim a producer shard");
        let second = authority
            .claim_session(topology_stream(2))
            .expect("second connection must claim the other producer shard");

        assert_ne!(first.producer.shard_index(), second.producer.shard_index());
        assert_ne!(first.connection_generation, second.connection_generation);
        assert_eq!(first.topology_stream_id, topology_stream(1));
        assert_eq!(second.topology_stream_id, topology_stream(2));
        assert!(Arc::ptr_eq(&first.authority, &authority));
        assert!(
            authority.claim_session(topology_stream(3)).is_none(),
            "a third connection must degrade tracing instead of sharing an SPSC shard"
        );

        let released_shard = first.producer.shard_index();
        drop(first);
        let replacement = authority
            .claim_session(topology_stream(4))
            .expect("dropping a connection must release its exact producer claim");
        assert_eq!(replacement.producer.shard_index(), released_shard);
        assert_ne!(
            replacement.connection_generation,
            second.connection_generation
        );
        assert_eq!(replacement.topology_stream_id, topology_stream(4));
    }

    #[test]
    fn server_trace_producer_records_exact_k4_k5_prefix_without_content() {
        let recorder = trace_recorder(1);
        let authority = DispatchTraceAuthority::new(Arc::clone(&recorder));
        let producer = authority
            .claim_session(topology_stream(9))
            .expect("connection must claim its producer before request dispatch");
        let token = producer
            .admit_remote_trace(sampled_key_context())
            .expect("certification recorder must admit the sampled remote trace");
        let topology = InteractionTraceTopology {
            window_id: 101,
            tab_id: 202,
            pane_id: 303,
        };
        let k4_start = Instant::now();
        let k4_end = k4_start
            .checked_add(std::time::Duration::from_nanos(5))
            .expect("test instant addition must fit");
        let k5_end = k4_end
            .checked_add(std::time::Duration::from_nanos(7))
            .expect("test instant addition must fit");

        producer.record_server_stage(
            token,
            RendererKeypressTraceStage::ServerReadableDecode,
            topology,
            k4_start,
            k4_end,
        );
        let admission = AdmittedInputTraceV1 {
            context: sampled_key_context(),
            stream_id: topology_stream(9),
            pane_id: 303,
            input_serial: InputSerial::from_millis_since_epoch(11),
            recorder_token: Some(token),
            topology: Some(topology),
            dispatch_queued_at: Some(k4_end),
        };
        producer.record_mux_dispatch_start(&admission, k5_end);

        let frozen = recorder
            .try_freeze()
            .expect("quiescent two-stage recorder must freeze");
        let mut events = Vec::with_capacity(frozen.len());
        assert_eq!(
            frozen.export_into(&mut events),
            frankenterm_flight_recorder::ExportOutcome::Completed { exported_events: 2 }
        );
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].stage,
            InteractionTraceStage::Keypress(RendererKeypressTraceStage::ServerReadableDecode)
        );
        assert_eq!(
            events[1].stage,
            InteractionTraceStage::Keypress(RendererKeypressTraceStage::ServerDispatchMuxWait)
        );
        assert_eq!(events[0].topology, topology);
        assert_eq!(events[1].topology, topology);
        assert_eq!(events[0].duration_ns().expect("K4 clock is coherent"), 5);
        assert_eq!(events[1].duration_ns().expect("K5 clock is coherent"), 7);
        assert_eq!(events[0].trace_id, sampled_key_context().trace_id);
        assert_eq!(events[1].trace_id, sampled_key_context().trace_id);
    }

    fn sampled_key_context() -> SampledTraceContextV1 {
        SampledTraceContextV1 {
            schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
            trace_id: InteractionTraceId {
                run_id: InteractionTraceRunId {
                    epoch_nonce_hi: 0x4142_4344_4546_4748,
                    epoch_nonce_lo: 0x5152_5354_5556_5758,
                },
                sequence: 17,
            },
            path: InteractionTracePath::Keypress,
            origin_recorder_epoch_id: RecorderEpochId {
                nonce_hi: 0x6162_6364_6566_6768,
                nonce_lo: 0x7172_7374_7576_7778,
            },
            sampler_algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
        }
    }

    fn sampled_key_request() -> SendKeyDownTracedV1 {
        SendKeyDownTracedV1 {
            request: SendKeyDown {
                pane_id: 7_007,
                event: termwiz::input::KeyEvent {
                    key: termwiz::input::KeyCode::Char('x'),
                    modifiers: termwiz::input::Modifiers::NONE,
                },
                input_serial: InputSerial::from_millis_since_epoch(11),
            },
            trace_context: sampled_key_context(),
        }
    }

    struct ScopedMux(Option<Arc<Mux>>);

    impl ScopedMux {
        fn install(mux: &Arc<Mux>) -> Self {
            let prior = Mux::try_get();
            Mux::set_mux(mux);
            Self(prior)
        }

        fn shutdown_current() -> Self {
            let prior = Mux::try_get();
            Mux::shutdown();
            Self(prior)
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.0.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    #[derive(Clone)]
    struct FakePaneState {
        cursor_position: StableCursorPosition,
        dimensions: RenderableDimensions,
        tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
        title: String,
        working_dir: Option<Url>,
        alt_screen_active: bool,
        seqno: SequenceNo,
        lines: Vec<Line>,
    }

    struct FakePane {
        pane_id: PaneId,
        state: Mutex<FakePaneState>,
        changed_lines: Mutex<RangeSet<StableRowIndex>>,
        changed_since_seqnos: Mutex<Vec<SequenceNo>>,
        key_down_count: AtomicUsize,
        paste_count: AtomicUsize,
        callback_probe: Option<Arc<dyn Fn() + Send + Sync>>,
        tiered_scrollback_status_probe: Option<Arc<dyn Fn() + Send + Sync>>,
        seqno_on_dimensions: Option<SequenceNo>,
        cursor_line_start_override: Option<StableRowIndex>,
        // Writer sink for the Pane::writer() trait obligation. FakePane
        // has no PTY; bytes written here are discarded. Keeping a real
        // writable buffer behind a parking_lot mutex (rather than
        // panicking via unimplemented!) means tests that exercise
        // writer() — e.g., paste handlers, scripted-input drivers —
        // don't crash the test binary on a code path that's
        // semantically a no-op for a fake pane. (br-ft-35yac.3)
        writer_sink: ParkingMutex<std::io::Sink>,
        mux_registration: Arc<mux::PaneRegistrationSlot>,
    }

    impl FakePane {
        fn new(tiered_scrollback_status: Option<PaneTieredScrollbackStatus>) -> Self {
            Self::new_with_id(77, tiered_scrollback_status)
        }

        fn new_with_id(
            pane_id: PaneId,
            tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
        ) -> Self {
            Self {
                pane_id,
                callback_probe: None,
                tiered_scrollback_status_probe: None,
                seqno_on_dimensions: None,
                cursor_line_start_override: None,
                writer_sink: ParkingMutex::new(std::io::sink()),
                mux_registration: Arc::new(mux::PaneRegistrationSlot::default()),
                changed_lines: Mutex::new(RangeSet::new()),
                changed_since_seqnos: Mutex::new(Vec::new()),
                key_down_count: AtomicUsize::new(0),
                paste_count: AtomicUsize::new(0),
                state: Mutex::new(FakePaneState {
                    cursor_position: StableCursorPosition {
                        x: 4,
                        y: 0,
                        ..Default::default()
                    },
                    dimensions: RenderableDimensions {
                        cols: 80,
                        viewport_rows: 2,
                        scrollback_rows: 2,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: 96,
                        pixel_width: 640,
                        pixel_height: 480,
                        reverse_video: false,
                    },
                    tiered_scrollback_status,
                    title: "tiered-pane".to_string(),
                    working_dir: Url::parse("file:///tmp/tiered-pane").ok(),
                    alt_screen_active: false,
                    seqno: 11,
                    lines: vec![
                        Line::from_text("alpha", &Default::default(), 1, None),
                        Line::from_text("beta", &Default::default(), 1, None),
                    ],
                }),
            }
        }

        fn new_with_callback_probe(
            pane_id: PaneId,
            callback_probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Self {
            let mut pane = Self::new_with_id(pane_id, None);
            pane.callback_probe = Some(callback_probe);
            pane
        }

        fn new_with_tiered_scrollback_status_probe(
            pane_id: PaneId,
            tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
            probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Self {
            let mut pane = Self::new_with_id(pane_id, tiered_scrollback_status);
            pane.tiered_scrollback_status_probe = Some(probe);
            pane
        }

        fn set_tiered_scrollback_status(&self, status: Option<PaneTieredScrollbackStatus>) {
            self.state.lock().unwrap().tiered_scrollback_status = status;
        }

        fn set_changed_line(&self, stable_row: StableRowIndex) {
            self.changed_lines.lock().unwrap().add(stable_row);
        }

        fn clear_changed_lines(&self) {
            *self.changed_lines.lock().unwrap() = RangeSet::new();
        }

        fn take_changed_since_seqnos(&self) -> Vec<SequenceNo> {
            std::mem::take(&mut *self.changed_since_seqnos.lock().unwrap())
        }

        fn key_down_count(&self) -> usize {
            self.key_down_count.load(Ordering::Relaxed)
        }

        fn paste_count(&self) -> usize {
            self.paste_count.load(Ordering::Relaxed)
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
            &self.mux_registration
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            self.state.lock().unwrap().cursor_position
        }

        fn get_current_seqno(&self) -> SequenceNo {
            self.state.lock().unwrap().seqno
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            self.changed_since_seqnos.lock().unwrap().push(seqno);
            if self.state.lock().unwrap().seqno <= seqno {
                RangeSet::new()
            } else {
                self.changed_lines.lock().unwrap().clone()
            }
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let state = self.state.lock().unwrap();
            let first_line = if lines.start == state.cursor_position.y
                && lines.end.checked_sub(lines.start) == Some(1)
            {
                self.cursor_line_start_override.unwrap_or(lines.start)
            } else {
                lines.start
            };
            (
                first_line,
                state
                    .lines
                    .iter()
                    .skip(lines.start as usize)
                    .take((lines.end - lines.start) as usize)
                    .cloned()
                    .collect(),
            )
        }

        fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
            mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            lines: Range<StableRowIndex>,
            for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
        }

        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            if let Some(probe) = &self.callback_probe {
                probe();
            }
            let mut state = self.state.lock().unwrap();
            if let Some(seqno) = self.seqno_on_dimensions {
                state.seqno = seqno;
            }
            state.dimensions
        }

        fn get_tiered_scrollback_status(&self) -> Option<PaneTieredScrollbackStatus> {
            if let Some(probe) = &self.tiered_scrollback_status_probe {
                probe();
            }
            self.state.lock().unwrap().tiered_scrollback_status
        }

        fn get_title(&self) -> String {
            self.state.lock().unwrap().title.clone()
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            self.paste_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            // Discarding sink: FakePane has no underlying PTY, so any
            // bytes written are dropped on the floor. This replaces a
            // prior `unimplemented!()` panic-bomb (br-ft-35yac.3) so
            // test code that walks Pane::writer() without first
            // checking the pane's class doesn't crash the test binary.
            ParkingMutexGuard::map(self.writer_sink.lock(), |sink| {
                let writer: &mut dyn std::io::Write = sink;
                writer
            })
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.dimensions.cols = size.cols;
            state.dimensions.viewport_rows = size.rows;
            state.dimensions.scrollback_rows = size.rows;
            state.dimensions.dpi = size.dpi;
            state.dimensions.pixel_width = size.pixel_width;
            state.dimensions.pixel_height = size.pixel_height;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            self.key_down_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            DomainId::default()
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            self.state.lock().unwrap().alt_screen_active
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            self.state.lock().unwrap().working_dir.clone()
        }
    }

    fn sample_tiered_scrollback_status(cold_spill_lines_total: u64) -> PaneTieredScrollbackStatus {
        PaneTieredScrollbackStatus {
            tiering_enabled: true,
            configured_scrollback_rows: 10_000,
            configured_hot_lines: 512,
            configured_warm_max_bytes: 8 * 1024,
            visible_rows: 2,
            in_memory_scrollback_rows: 6,
            warm_resident_lines: 4,
            warm_resident_bytes: 512,
            warm_spill_lines_total: cold_spill_lines_total + 4,
            warm_spill_bytes_total: 4096,
            cold_spill_lines_total,
            cold_spill_bytes_total: cold_spill_lines_total * 64,
            cold_sink_retained_lines: 0,
            cold_sink_retained_bytes: 0,
            cold_worker_peak_backlog_depth: 3,
            cold_worker_completion_throughput_lines_per_sec: 256,
            cold_worker_completed_lines_total: cold_spill_lines_total,
            cold_worker_completed_batches_total: 2,
            cold_worker_cancellation_count: 0,
        }
    }

    /// Creates a PduSender that captures all sent PDUs into a shared Vec.
    fn capturing_sender() -> (PduSender, Arc<Mutex<Vec<DecodedPdu>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let sender = PduSender::new(move |pdu, _class| {
            captured_clone.lock().unwrap().push(pdu);
            Ok(())
        });
        (sender, captured)
    }

    fn register_test_pane(pane: &Arc<dyn Pane>) -> (Arc<Mux>, mux::PaneRegistrationHandle) {
        let mux = Arc::new(Mux::new(None));
        mux.add_pane(pane).expect("test pane registration");
        let registration = mux
            .capture_pane_registration(pane)
            .expect("test pane should yield an exact handle");
        (mux, registration)
    }

    fn prepare_transactional_for_registration(
        state: Arc<Mutex<PerPane>>,
        registration: &PaneRegistrationHandle,
        force_with_input_dispatch_serial: Option<InputSerial>,
    ) -> Result<PaneRenderPreparationOutcome, PaneRenderPreparationError> {
        let preparation = begin_transactional_pane_render(
            state,
            registration.pane_id(),
            force_with_input_dispatch_serial,
        )?;
        registration
            .try_with_current(move |pane| preparation.prepare(&pane))
            .expect("test pane registration remains current")
    }

    fn expect_prepared(outcome: PaneRenderPreparationOutcome) -> PreparedPaneRender {
        match outcome {
            PaneRenderPreparationOutcome::Prepared(prepared) => *prepared,
            PaneRenderPreparationOutcome::NoChange(settlement) => {
                panic!("expected prepared render application, got {settlement:?}")
            }
        }
    }

    fn poison_per_pane_lock(state: &Arc<Mutex<PerPane>>) {
        let state = Arc::clone(state);
        assert!(
            std::thread::spawn(move || {
                let _held = state.lock().expect("test state starts unpoisoned");
                panic!("synthetic per-pane lock poison");
            })
            .join()
            .is_err(),
            "synthetic poison thread must panic"
        );
    }

    /// Extract the single response PDU from the captured list.
    fn take_response(captured: &Arc<Mutex<Vec<DecodedPdu>>>) -> DecodedPdu {
        let mut v = captured.lock().unwrap();
        assert_eq!(v.len(), 1, "expected exactly one response PDU");
        v.remove(0)
    }

    fn tick_until_response(
        executor: &SimpleExecutor,
        captured: &Arc<Mutex<Vec<DecodedPdu>>>,
        expected: usize,
    ) {
        for _ in 0..16 {
            if captured.lock().unwrap().len() >= expected {
                return;
            }
            executor.tick().unwrap();
        }

        let observed = captured.lock().unwrap().len();
        panic!("timed out waiting for {expected} response PDUs; saw {observed}");
    }

    fn test_tab_size() -> TerminalSize {
        TerminalSize {
            rows: 40,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 1000,
            dpi: 96,
        }
    }

    struct NoSleepSnapshotBarrier {
        collector_arrived: Barrier,
        mutation_finished: Barrier,
        observations: AtomicUsize,
    }

    impl NoSleepSnapshotBarrier {
        fn shared() -> Arc<Self> {
            Arc::new(Self {
                collector_arrived: Barrier::new(2),
                mutation_finished: Barrier::new(2),
                observations: AtomicUsize::new(0),
            })
        }

        fn collector_arrive(&self) {
            if self.observations.fetch_add(1, Ordering::AcqRel) == 0 {
                self.collector_arrived.wait();
                self.mutation_finished.wait();
            }
        }

        fn mutate<R>(&self, mutation: impl FnOnce() -> R) -> R {
            self.collector_arrived.wait();
            let result = mutation();
            self.mutation_finished.wait();
            result
        }

        fn observations(&self) -> usize {
            self.observations.load(Ordering::Acquire)
        }
    }

    fn register_snapshot_tab(mux: &Arc<Mux>, pane: Arc<dyn Pane>) -> Arc<mux::tab::Tab> {
        let tab = Arc::new(mux::tab::Tab::new(&test_tab_size()));
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("register snapshot-stage tab and active pane");
        tab
    }

    fn attach_snapshot_tab_to_new_window(
        mux: &Arc<Mux>,
        tab: &Arc<mux::tab::Tab>,
    ) -> mux::window::WindowId {
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(tab, window_id)
            .expect("attach snapshot-stage tab to its window");
        drop(window);
        window_id
    }

    fn expect_current_coherent_snapshot(
        mux: &Mux,
        outcome: ListPanesCoherentOutcome,
    ) -> CoherentPaneSnapshot {
        let ListPanesCoherentOutcome::Snapshot(snapshot) = outcome else {
            panic!("expected one stable coherent snapshot, got {outcome:?}");
        };
        let current_authority = mux
            .topology_snapshot_authority()
            .expect("snapshot-stage mux topology authority remains live");
        assert_eq!(
            (snapshot.session_incarnation, snapshot.snapshot_revision),
            current_authority,
            "a successful retry must carry the exact post-mutation authority"
        );
        snapshot
    }

    fn ordered_window_capabilities(include_reorder: bool) -> TopologyCapabilities {
        let mut bits = ordered_snapshot_foundation().bits();
        if include_reorder {
            bits |= TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits();
        }
        TopologyCapabilities::from_bits(bits)
    }

    fn ordered_snapshot_request(include_reorder: bool) -> codec::ListPanesOrderedV1 {
        let capabilities = ordered_window_capabilities(include_reorder);
        codec::ListPanesOrderedV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: codec::DomainBindingId::from_bytes([0xd1; 16]),
            supported: capabilities,
            required: capabilities,
        }
    }

    fn expect_current_ordered_snapshot(
        mux: &Mux,
        outcome: codec::ListPanesOrderedV1Outcome,
    ) -> codec::OrderedPaneSnapshotV1 {
        let codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) = outcome else {
            panic!("expected one stable ordered snapshot, got {outcome:?}");
        };
        let current_authority = mux
            .topology_snapshot_authority()
            .expect("ordered snapshot mux topology authority remains live");
        assert_eq!(
            (snapshot.session_incarnation, snapshot.topology_revision),
            current_authority,
            "an accepted ordered snapshot must carry the exact current mux authority"
        );
        snapshot
    }

    fn ordered_pane_tree_identity(
        panes: &mux::tab::PaneArena,
        tree_index: usize,
    ) -> Option<(usize, usize)> {
        let descriptor = panes.trees().get(tree_index)?;
        let tree_start = usize::try_from(descriptor.root_index?).ok()?;
        let node_count = usize::try_from(descriptor.node_count).ok()?;
        let tree_end = tree_start.checked_add(node_count)?;
        panes
            .nodes()
            .get(tree_start..tree_end)?
            .iter()
            .find_map(|node| match node {
                mux::tab::PaneArenaNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
                mux::tab::PaneArenaNode::Empty | mux::tab::PaneArenaNode::Split { .. } => None,
            })
    }

    fn ordered_pane_tree_identities(panes: &mux::tab::PaneArena) -> Vec<(usize, usize)> {
        panes
            .trees()
            .iter()
            .enumerate()
            .map(|(tree_index, _)| {
                ordered_pane_tree_identity(panes, tree_index)
                    .expect("accepted ordered pane tree has window/tab identity")
            })
            .collect()
    }

    fn reorder_request_for_snapshot(
        snapshot: &mux::window::FrozenWindowOrder,
        session_incarnation: mux::MuxSessionIncarnation,
        stream_id: TopologyStreamId,
        domain_binding_id: codec::DomainBindingId,
        mutation_sequence: u64,
        desired_tab_ids: Vec<mux::tab::TabId>,
    ) -> codec::ReorderWindowTabsV1 {
        let window_id = u64::try_from(snapshot.window_id()).expect("test window id fits u64");
        let desired_tab_ids = desired_tab_ids
            .into_iter()
            .map(|tab_id| {
                codec::RemoteTabId::new(u64::try_from(tab_id).expect("test tab id fits u64"))
            })
            .collect();
        let desired_active_tab_id = snapshot.active_tab_id().map(|tab_id| {
            codec::RemoteTabId::new(u64::try_from(tab_id).expect("test active tab id fits u64"))
        });
        codec::ReorderWindowTabsV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id,
            stream_id,
            session_incarnation,
            window_id: codec::RemoteWindowId::new(window_id),
            expected_order_revision: codec::WindowOrderRevision::new(
                snapshot.order_revision().get(),
            ),
            desired_tab_ids,
            desired_active_tab_id,
            mutation_id: codec::WindowOrderMutationId::new([0x9d; 16], mutation_sequence),
            digest: codec::WindowReorderDigest::ZERO,
        }
        .with_computed_digest()
    }

    fn mux_reorder_request_for_snapshot_stage(
        snapshot: &mux::window::FrozenWindowOrder,
        session_incarnation: mux::MuxSessionIncarnation,
        mutation_sequence: u64,
        desired_tab_ids: Vec<mux::tab::TabId>,
    ) -> mux::ReorderWindowTabsRequest {
        let wire_request = reorder_request_for_snapshot(
            snapshot,
            session_incarnation,
            TopologyStreamId::from_bytes([0xa7; 16]),
            codec::DomainBindingId::from_bytes([0xd7; 16]),
            mutation_sequence,
            desired_tab_ids,
        );
        ordered_window_adapter::codec_reorder_request_to_mux(&wire_request)
            .expect("snapshot-stage reorder request must satisfy mux authority")
    }

    fn install_tab_with_window(
        tab: &Arc<mux::tab::Tab>,
        extra_panes: &[Arc<dyn Pane>],
    ) -> (Arc<Mux>, ScopedMux) {
        let mux = Arc::new(Mux::new(None));
        let guard = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_and_active_pane(tab).unwrap();
        for pane in extra_panes {
            mux.add_pane(pane).unwrap();
        }
        mux.add_tab_to_window(tab, window_id).unwrap();
        drop(window);
        (mux, guard)
    }

    #[test]
    fn stable_row_range_helpers_reject_unrepresentable_spans() {
        assert_eq!(
            stable_row_range_from_len(StableRowIndex::MAX - 1, 1),
            Some((StableRowIndex::MAX - 1)..StableRowIndex::MAX)
        );
        assert_eq!(stable_row_range_from_len(StableRowIndex::MAX, 1), None);
        assert_eq!(
            stable_row_range_from_signed_len(StableRowIndex::MAX - 1, 2),
            None
        );
        assert_eq!(stable_row_range_from_signed_len(10, -1), None);
    }

    #[test]
    fn session_handler_new_has_empty_state() {
        let (sender, _captured) = capturing_sender();
        let handler = SessionHandler::new(sender);
        assert!(handler.client_id.is_none());
        assert!(handler.proxy_client_id.is_none());
        assert!(handler.per_pane.is_empty());
    }

    #[test]
    fn sampled_input_admission_binds_exact_context_and_connection_generation() {
        let request = sampled_key_request();
        let stream_id = TopologyStreamId::from_bytes([0x91; 16]);
        let admission = AdmittedInputTraceV1::admit(&request, stream_id, codec::CODEC_VERSION)
            .expect("valid sampled key input should gain server admission authority");
        assert_eq!(admission.context(), request.trace_context);
        assert_eq!(admission.stream_id(), stream_id);
        admission
            .validate_for_request(&request, stream_id)
            .expect("exact request and stream should remain current");
    }

    #[test]
    fn sampled_input_admission_rejects_old_codec_zero_and_stale_generations() {
        let request = sampled_key_request();
        let current_stream = TopologyStreamId::from_bytes([0x92; 16]);
        let prior_dialect = codec::SAMPLED_INPUT_TRACE_V1_MIN_CODEC_VERSION - 1;
        let error = AdmittedInputTraceV1::admit(&request, current_stream, prior_dialect)
            .expect_err("pre-v59 dialect must reject the traced-input PDU");
        assert!(format!("{error:#}").contains("unavailable in this codec dialect"));

        let error = AdmittedInputTraceV1::admit(
            &request,
            TopologyStreamId::from_bytes([0; 16]),
            codec::CODEC_VERSION,
        )
        .expect_err("zero stream identity must fail closed");
        assert!(format!("{error:#}").contains("no live connection-generation authority"));

        let admission = AdmittedInputTraceV1::admit(&request, current_stream, codec::CODEC_VERSION)
            .expect("current stream should admit the sampled input");
        let successor_stream = TopologyStreamId::from_bytes([0x93; 16]);
        let error = admission
            .validate_for_request(&request, successor_stream)
            .expect_err("reconnect must retire the prior stream authority");
        assert!(format!("{error:#}").contains("stale connection generation"));

        let mut different_context = request.clone();
        different_context.trace_context.trace_id.sequence += 1;
        let error = admission
            .validate_for_request(&different_context, current_stream)
            .expect_err("admission cannot be transplanted to a different trace context");
        assert!(format!("{error:#}").contains("differs from its admission authority"));

        let mut different_request = request.clone();
        different_request.request.input_serial = InputSerial::from_millis_since_epoch(12);
        let error = admission
            .validate_for_request(&different_request, current_stream)
            .expect_err("admission cannot be transplanted to another input request");
        assert!(format!("{error:#}").contains("request identity differs"));
    }

    #[test]
    fn ordered_window_dispatch_token_checks_capability_before_stream() {
        let stream_id = TopologyStreamId::from_bytes([0x81; 16]);
        let session_incarnation = mux::MuxSessionIncarnation::from_bytes([0x82; 16]);
        let domain_binding_id = codec::DomainBindingId::from_bytes([0x83; 16]);
        let authority = established_ordered_window_authority_for_test(
            stream_id,
            session_incarnation,
            domain_binding_id,
            ordered_window_capabilities(false),
        );
        let request = codec::ReorderWindowTabsV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id,
            stream_id: TopologyStreamId::from_bytes([0x84; 16]),
            session_incarnation,
            window_id: codec::RemoteWindowId::new(0),
            expected_order_revision: codec::WindowOrderRevision::new(0),
            desired_tab_ids: Vec::new(),
            desired_active_tab_id: None,
            mutation_id: codec::WindowOrderMutationId::new([0x85; 16], 1),
            digest: codec::WindowReorderDigest::ZERO,
        };
        let error = admit_reorder_transport(&request, authority)
            .expect_err("missing capability must outrank a foreign stream");
        assert!(format!("{error:#}").contains("capability is not established"));
    }

    #[test]
    fn successful_pdu86_response_echoes_binding_without_handler_authority() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let stream_id = TopologyStreamId::from_bytes([0x85; 16]);
        let request = ordered_snapshot_request(true);

        let response = process_list_panes_ordered_request(
            &mux,
            stream_id,
            ordered_window_capabilities(true),
            &request,
        )
        .expect("future-enabled PDU86 must produce one request-correlated PDU87");
        response
            .validate_for_request(&request)
            .expect("future-enabled PDU87 must echo the durable binding");
        let _snapshot = expect_current_ordered_snapshot(&mux, response.outcome);
        assert_eq!(response.domain_binding_id, request.domain_binding_id);
    }

    #[test]
    fn per_pane_creates_and_caches_entry() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let pp1 = handler.per_pane(42);
        let pp2 = handler.per_pane(42);
        // Same Arc returned for same pane_id
        assert!(Arc::ptr_eq(&pp1, &pp2));

        let pp3 = handler.per_pane(99);
        // Different pane_id gets a different entry
        assert!(!Arc::ptr_eq(&pp1, &pp3));
    }

    #[test]
    fn per_pane_default_has_zero_seqno_and_empty_title() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let pp = handler.per_pane(1);
        let guard = pp.lock().unwrap();
        assert_eq!(guard.baseline.seqno, 0);
        assert_eq!(guard.baseline.title, "");
        assert!(!guard.baseline.mouse_grabbed);
        assert!(!guard.baseline.sent_initial_palette);
        assert!(guard.notifications.is_empty());
        assert!(guard.baseline.working_dir.is_none());
    }

    #[test]
    fn tracked_pane_push_does_not_recreate_removed_pane_state() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.per_pane(7);
        handler.remove_per_pane(7);
        handler.schedule_tracked_pane_push(7);

        assert!(
            handler.per_pane_if_present(7).is_none(),
            "late mux notifications must not recreate cached state for removed panes"
        );
    }

    #[test]
    fn session_owner_retirement_fails_closed_and_releases_its_mux() {
        let mux = Arc::new(Mux::new(None));
        let weak_mux = Arc::downgrade(&mux);
        let owner = SessionOwner::new(mux);
        let authority = owner.authority();
        let operation = authority
            .incarnation
            .try_acquire()
            .expect("live session should admit an operation");

        owner.retire();

        assert!(
            authority.acquire().is_err(),
            "retirement must reject new mux authority"
        );
        assert!(
            authority.try_run(|| ()).is_err(),
            "retirement must reject generic deferred work"
        );
        assert_eq!(
            authority
                .incarnation
                .operation_state
                .load(Ordering::Acquire),
            SESSION_RETIRED | 1,
            "retirement must preserve the admitted operation count until its lease drops"
        );

        drop(operation);
        assert_eq!(
            authority
                .incarnation
                .operation_state
                .load(Ordering::Acquire),
            SESSION_RETIRED,
            "operation release must preserve the retirement fence"
        );
        drop(owner);
        assert!(
            weak_mux.upgrade().is_none(),
            "cloneable deferred authority must not keep the owning mux alive"
        );
    }

    #[test]
    fn per_pane_cache_is_scoped_to_exact_registration() {
        let mux = Arc::new(Mux::new(None));
        let pane_id = 7_001;
        let original: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&original).expect("register original pane");
        let (sender, _captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));
        let original_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture original registration");
        let original_state = handler.per_pane_for_registration(&original_registration);

        assert!(original_registration.retire_if_current());
        let replacement: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&replacement)
            .expect("register replacement pane");
        let replacement_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture replacement registration");
        let replacement_state = handler.per_pane_for_registration(&replacement_registration);

        assert!(
            !Arc::ptr_eq(&original_state, &replacement_state),
            "same numeric PaneId must not reuse state across registration generations"
        );
        handler.remove_per_pane_if_same(&original_registration);
        handler.remove_per_pane(pane_id);
        let retained_state = handler
            .per_pane_if_present(pane_id)
            .expect("stale removal signals must preserve a live replacement cache");
        assert!(
            Arc::ptr_eq(&replacement_state, &retained_state),
            "stale exact and raw removals must not clear replacement state"
        );
    }

    #[test]
    fn old_registration_candidate_ack_cannot_commit_replacement_state() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let mux = Arc::new(Mux::new(None));
        let pane_id = 7_002;
        let original: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&original).expect("register original pane");
        let (sender, _captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));
        let original_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture original registration");
        let original_state = handler.per_pane_for_registration(&original_registration);
        let prepared = expect_prepared(
            prepare_transactional_for_registration(
                Arc::clone(&original_state),
                &original_registration,
                None,
            )
            .expect("prepare original registration candidate"),
        );

        assert!(original_registration.retire_if_current());
        let replacement: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&replacement)
            .expect("register replacement pane");
        let replacement_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture replacement registration");
        let replacement_state = handler.per_pane_for_registration(&replacement_registration);

        assert!(
            !Arc::ptr_eq(&original_state, &replacement_state),
            "replacement registration must own an independent transaction state"
        );
        assert_eq!(
            prepared.acknowledge(),
            PaneRenderSettlement::AcknowledgedClean
        );
        assert_eq!(
            original_state.lock().unwrap().baseline.seqno,
            11,
            "the exact original transaction may commit only its own state"
        );
        assert_eq!(
            replacement_state.lock().unwrap().baseline,
            PaneRenderBaseline::default(),
            "an old registration ACK must not mutate same-numeric-id replacement state"
        );
    }

    #[test]
    fn old_registration_legacy_enqueue_ack_cannot_mutate_replacement_state() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let mux = Arc::new(Mux::new(None));
        let pane_id = 7_003;
        let original: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&original).expect("register original pane");
        let (sender, _captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));
        let original_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture original registration");
        let original_state = handler.per_pane_for_registration(&original_registration);
        let (_, enqueue_guard) = original_registration
            .try_with_current(|current| {
                prepare_legacy_render_enqueue(&current, &original_state, None)
            })
            .expect("original registration remains current")
            .expect("prepare original legacy enqueue")
            .expect("original pane state produces a render");

        assert!(original_registration.retire_if_current());
        let replacement: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&replacement)
            .expect("register replacement pane");
        let replacement_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture replacement registration");
        let replacement_state = handler.per_pane_for_registration(&replacement_registration);

        assert!(!Arc::ptr_eq(&original_state, &replacement_state));
        enqueue_guard
            .acknowledge()
            .expect("the original enqueue settles only its exact state");
        assert_eq!(
            original_state.lock().unwrap().legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::Idle
        );
        let replacement_state = replacement_state.lock().unwrap();
        assert_eq!(replacement_state.baseline, PaneRenderBaseline::default());
        assert_eq!(
            replacement_state.legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::Idle
        );
    }

    #[test]
    fn deferred_pane_read_stays_on_origin_mux_after_global_swap() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let pane_id = 7_002;
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let originating_pane = Arc::new(FakePane::new_with_id(pane_id, None));
        let replacement_pane = Arc::new(FakePane::new_with_id(pane_id, None));
        originating_pane.state.lock().unwrap().dimensions.cols = 111;
        replacement_pane.state.lock().unwrap().dimensions.cols = 222;
        let originating_pane_dyn: Arc<dyn Pane> = originating_pane;
        let replacement_pane_dyn: Arc<dyn Pane> = replacement_pane;
        originating_mux
            .add_pane(&originating_pane_dyn)
            .expect("register originating pane");
        replacement_mux
            .add_pane(&replacement_pane_dyn)
            .expect("register replacement-mux pane");
        let _mux_guard = ScopedMux::install(&originating_mux);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session(
            sender,
            SessionOwner::new(Arc::clone(&originating_mux)),
        );

        handler.process_one(DecodedPdu {
            serial: 301,
            pdu: Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }),
        });
        Mux::set_mux(&replacement_mux);
        tick_until_response(&executor, &captured, 1);

        match take_response(&captured).pdu {
            Pdu::GetPaneRenderableDimensionsResponse(GetPaneRenderableDimensionsResponse {
                dimensions,
                ..
            }) => {
                assert_eq!(
                    dimensions.cols, 111,
                    "queued read must resolve against its originating mux"
                );
            }
            other => panic!("expected GetPaneRenderableDimensionsResponse, got {other:?}"),
        }
    }

    #[test]
    fn tiered_scrollback_bulk_status_is_ordered_lightweight_and_typed() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let render_callback_calls = Arc::new(AtomicUsize::new(0));
        let render_callback_calls_for_probe = Arc::clone(&render_callback_calls);
        let available = Arc::new(FakePane::new_with_callback_probe(
            7,
            Arc::new(move || {
                render_callback_calls_for_probe.fetch_add(1, Ordering::Relaxed);
            }),
        ));
        available.set_tiered_scrollback_status(Some(sample_tiered_scrollback_status(41)));
        let unavailable = Arc::new(FakePane::new_with_id(8, None));
        let available_dyn: Arc<dyn Pane> = available;
        let unavailable_dyn: Arc<dyn Pane> = unavailable;
        mux.add_pane(&available_dyn)
            .expect("register available pane");
        mux.add_pane(&unavailable_dyn)
            .expect("register unavailable pane");
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));
        render_callback_calls.store(0, Ordering::Relaxed);

        handler.process_one(DecodedPdu {
            serial: 401,
            pdu: Pdu::GetPaneTieredScrollbackStatusesV1(codec::GetPaneTieredScrollbackStatusesV1 {
                pane_ids: vec![7, 8, 9],
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let Pdu::GetPaneTieredScrollbackStatusesV1Response(response) = take_response(&captured).pdu
        else {
            panic!("expected bulk tiered-scrollback status response");
        };
        assert_eq!(
            response.entries,
            vec![
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 7,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Available(
                        sample_tiered_scrollback_status(41).into(),
                    ),
                },
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 8,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                },
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 9,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Missing,
                },
            ]
        );
        assert_eq!(
            render_callback_calls.load(Ordering::Relaxed),
            0,
            "health sampling must not enter the render-delta callback graph"
        );
    }

    #[test]
    fn tiered_scrollback_bulk_status_contains_one_callback_panic_to_its_entry() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let panicking: Arc<dyn Pane> = Arc::new(FakePane::new_with_tiered_scrollback_status_probe(
            17,
            Some(sample_tiered_scrollback_status(17)),
            Arc::new(|| panic!("synthetic tiered-scrollback callback failure")),
        ));
        let healthy: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(
            18,
            Some(sample_tiered_scrollback_status(18)),
        ));
        mux.add_pane(&panicking).expect("register panicking pane");
        mux.add_pane(&healthy).expect("register healthy pane");
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));

        handler.process_one(DecodedPdu {
            serial: 402,
            pdu: Pdu::GetPaneTieredScrollbackStatusesV1(codec::GetPaneTieredScrollbackStatusesV1 {
                pane_ids: vec![17, 18],
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let Pdu::GetPaneTieredScrollbackStatusesV1Response(response) = take_response(&captured).pdu
        else {
            panic!("expected panic-contained bulk status response");
        };
        assert_eq!(
            response.entries,
            vec![
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 17,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::CallbackPanicked,
                },
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 18,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Available(
                        sample_tiered_scrollback_status(18).into(),
                    ),
                },
            ],
            "one faulty pane callback must not abort or reorder sibling samples"
        );
    }

    #[test]
    fn tiered_scrollback_bulk_freezes_membership_before_callbacks() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let later_registration = Arc::new(Mutex::new(None::<PaneRegistrationHandle>));
        let later_registration_for_callback = Arc::clone(&later_registration);
        let first: Arc<dyn Pane> = Arc::new(FakePane::new_with_tiered_scrollback_status_probe(
            27,
            Some(sample_tiered_scrollback_status(27)),
            Arc::new(move || {
                if let Some(registration) = later_registration_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    let _ = registration.retire_if_current();
                }
            }),
        ));
        let later: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(
            28,
            Some(sample_tiered_scrollback_status(28)),
        ));
        mux.add_pane(&first).expect("register first pane");
        mux.add_pane(&later).expect("register later pane");
        *later_registration
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(
            mux.capture_current_pane(28)
                .expect("capture later registration"),
        );
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));

        handler.process_one(DecodedPdu {
            serial: 403,
            pdu: Pdu::GetPaneTieredScrollbackStatusesV1(codec::GetPaneTieredScrollbackStatusesV1 {
                pane_ids: vec![27, 28, 29],
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let Pdu::GetPaneTieredScrollbackStatusesV1Response(response) = take_response(&captured).pdu
        else {
            panic!("expected frozen-membership bulk status response");
        };
        assert_eq!(
            response.entries,
            vec![
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 27,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Available(
                        sample_tiered_scrollback_status(27).into(),
                    ),
                },
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 28,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Closed,
                },
                PaneTieredScrollbackStatusEntryV1 {
                    pane_id: 29,
                    outcome: PaneTieredScrollbackStatusOutcomeV1::Missing,
                },
            ],
            "membership must be captured before the first callback, preserving closed versus missing",
        );
    }

    #[test]
    fn deferred_kill_rejects_same_id_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let pane_id = 7_003;
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let original: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&original).expect("register original pane");
        let original_registration = mux
            .capture_current_pane(pane_id)
            .expect("capture original registration");
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));

        handler.process_one(DecodedPdu {
            serial: 302,
            pdu: Pdu::KillPane(KillPane { pane_id }),
        });
        assert!(original_registration.retire_if_current());
        let replacement: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&replacement)
            .expect("register replacement pane");
        tick_until_response(&executor, &captured, 1);

        match take_response(&captured).pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("no longer current"),
                    "stale kill should report exact-registration loss, got {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
        assert!(
            mux.get_pane(pane_id)
                .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement)),
            "stale queued kill must preserve the same-ID replacement"
        );
    }

    #[test]
    fn dropping_handler_retires_queued_pane_work() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let pane_id = 7_004;
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&pane).expect("register pane");
        let registration = mux
            .capture_current_pane(pane_id)
            .expect("capture live pane registration");
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));

        handler.process_one(DecodedPdu {
            serial: 303,
            pdu: Pdu::KillPane(KillPane { pane_id }),
        });
        drop(handler);
        let drained = Arc::new(AtomicUsize::new(0));
        let drained_task = Arc::clone(&drained);
        spawn_into_main_thread(async move {
            drained_task.store(1, Ordering::Release);
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        for _ in 0..16 {
            if drained.load(Ordering::Acquire) == 1 {
                break;
            }
            executor.tick().expect("drain queued session work");
        }

        assert_eq!(
            drained.load(Ordering::Acquire),
            1,
            "sentinel must run after queued session work"
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "retired session must not emit a response from queued work"
        );
        assert_eq!(
            registration.try_with_current(|current| current.pane_id()),
            Some(pane_id),
            "retired session work must not remove its formerly authorized pane"
        );
    }

    #[test]
    fn stale_equal_client_registration_cannot_focus_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let first_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7_005, None));
        let second_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7_006, None));
        tab.assign_pane(&first_pane);
        tab.split_and_insert(
            0,
            mux::tab::SplitRequest {
                direction: mux::tab::SplitDirection::Horizontal,
                ..Default::default()
            },
            Arc::clone(&second_pane),
        )
        .expect("insert second pane");
        tab.set_active_pane(&first_pane);
        let (mux, _mux_guard) = install_tab_with_window(&tab, &[Arc::clone(&second_pane)]);
        let client = test_client_id("stale-focus", 41_008);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        handler.process_one(DecodedPdu {
            serial: 304,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        let stale_client = Arc::clone(handler.client_id.as_ref().expect("registered client"));
        let replacement_client = Arc::new(client);
        mux.register_client(Arc::clone(&replacement_client));
        assert!(!mux.client_registration_is_current(&stale_client));

        handler.process_one(DecodedPdu {
            serial: 305,
            pdu: Pdu::SetFocusedPane(SetFocusedPane {
                pane_id: second_pane.pane_id(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        match take_response(&captured).pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("client registration is no longer current"),
                    "stale focus should report exact client-registration loss, got {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
        assert_eq!(
            tab.get_active_pane().map(|pane| pane.pane_id()),
            Some(first_pane.pane_id()),
            "stale equal-valued client must not focus a replacement registration's pane"
        );
        drop(handler);
        assert!(
            mux.client_registration_is_current(&replacement_client),
            "stale handler cleanup must preserve the equal-valued replacement client"
        );
        assert!(mux.unregister_client_if_same(&replacement_client));
    }

    #[test]
    fn forbidden_clipboard_does_not_record_client_input_but_valid_write_does() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let pane_id = 7_007;
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&pane).expect("register writable test pane");

        let client = test_client_id("forbidden-clipboard-activity", 41_009);
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));
        handler.process_one(DecodedPdu {
            serial: 901,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client,
                is_proxy: false,
            }),
        });
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );

        handler.process_one(DecodedPdu {
            serial: 902,
            pdu: Pdu::SetClipboard(codec::SetClipboard {
                pane_id,
                clipboard: Some("must-not-apply".to_string()),
                selection: wezterm_term::ClipboardSelection::Clipboard,
            }),
        });

        let rejected = take_response(&captured);
        assert_eq!(rejected.serial, 902);
        match rejected.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => assert!(
                reason.contains("expected a request"),
                "forbidden SetClipboard should fail request dispatch, got {reason}"
            ),
            other => panic!("expected ErrorResponse for forbidden SetClipboard, got {other:?}"),
        }
        assert_eq!(
            handler.client_input_activity_updates, 0,
            "a rejected server-unilateral PDU must not refresh client activity"
        );

        handler.process_one(DecodedPdu {
            serial: 903,
            pdu: Pdu::WriteToPane(WriteToPane {
                pane_id,
                data: b"accepted-input".to_vec(),
            }),
        });
        assert_eq!(
            handler.client_input_activity_updates, 1,
            "one accepted client-input request must record exactly one activity update"
        );

        for _ in 0..32 {
            if captured
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .iter()
                .any(|response| response.serial == 903)
            {
                break;
            }
            executor.tick().expect("drive valid pane write");
        }
        let written = {
            let mut responses = captured.lock().unwrap_or_else(|err| err.into_inner());
            let index = responses
                .iter()
                .position(|response| response.serial == 903)
                .expect("valid pane write must produce a correlated response");
            responses.remove(index)
        };
        assert_eq!(written.pdu, Pdu::UnitResponse(UnitResponse {}));
    }

    #[test]
    fn sampled_key_input_rejects_missing_and_stale_authority_before_any_mutation() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let pane = Arc::new(FakePane::new_with_id(7_007, None));
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register sampled-input test pane");

        let current_stream = TopologyStreamId::from_bytes([0xa1; 16]);
        let client = test_client_id("sampled-input-admission", 41_010);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session_with_topology_stream(
            sender,
            SessionOwner::new(Arc::clone(&mux)),
            current_stream,
        );
        handler.process_one(DecodedPdu {
            serial: 910,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client,
                is_proxy: false,
            }),
        });
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );

        let request = sampled_key_request();
        handler.process_one_with_dispatch_authority(
            DecodedPdu {
                serial: 911,
                pdu: Pdu::SendKeyDownTracedV1(request.clone()),
            },
            None,
            None,
        );
        let rejected = take_response(&captured);
        assert!(matches!(rejected.pdu, Pdu::ErrorResponse(_)));
        assert_eq!(handler.client_input_activity_updates, 0);
        assert_eq!(pane.key_down_count(), 0);
        assert!(handler.per_pane.is_empty());

        let stale_stream = TopologyStreamId::from_bytes([0xa2; 16]);
        let stale = AdmittedInputTraceV1::admit(&request, stale_stream, codec::CODEC_VERSION)
            .expect("prior connection should have admitted its own request");
        handler.process_one_with_dispatch_authority(
            DecodedPdu {
                serial: 912,
                pdu: Pdu::SendKeyDownTracedV1(request.clone()),
            },
            None,
            Some(stale),
        );
        let rejected = take_response(&captured);
        match rejected.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("stale connection generation"));
            }
            other => panic!("expected stale-generation error, got {other:?}"),
        }
        assert_eq!(handler.client_input_activity_updates, 0);
        assert_eq!(pane.key_down_count(), 0);
        assert!(handler.per_pane.is_empty());

        let expected_input_serial = request.request.input_serial;
        let admitted = AdmittedInputTraceV1::admit(&request, current_stream, codec::CODEC_VERSION)
            .expect("current connection should admit the sampled request");
        handler.process_one_with_dispatch_authority(
            DecodedPdu {
                serial: 913,
                pdu: Pdu::SendKeyDownTracedV1(request),
            },
            None,
            Some(admitted),
        );
        assert_eq!(handler.client_input_activity_updates, 1);
        for _ in 0..32 {
            if pane.key_down_count() == 1
                && captured
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .iter()
                    .any(|response| response.serial == 913)
            {
                break;
            }
            executor.tick().expect("drive sampled key input");
        }
        assert_eq!(pane.key_down_count(), 1);
        let mut responses = captured.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(
            responses.len(),
            2,
            "committed key input must emit one correlated response and one render fence"
        );
        let correlated_index = responses
            .iter()
            .position(|response| response.serial == 913)
            .expect("committed key input must retain its request serial");
        assert_eq!(
            responses.remove(correlated_index).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        let render_fence = responses
            .pop()
            .expect("committed key input must emit its render fence");
        assert_eq!(render_fence.serial, 0);
        match render_fence.pdu {
            Pdu::GetPaneRenderChangesResponse(response) => {
                assert_eq!(response.pane_id, 7_007);
                assert_eq!(response.input_serial, Some(expected_input_serial));
            }
            other => panic!("expected input-correlated render fence, got {other:?}"),
        }
    }

    #[test]
    fn ping_pdu_returns_pong() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 42,
            pdu: Pdu::Ping(Ping {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 42);
        assert_eq!(resp.pdu, Pdu::Pong(Pong {}));
    }

    #[test]
    fn select_stack_pane_pdu_selects_requested_stack_member() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane1: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        let pane2: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        let pane3: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(3, None));
        tab.assign_pane(&pane1);
        let first_request = mux::tab::SplitRequest {
            direction: mux::tab::SplitDirection::Horizontal,
            ..Default::default()
        };
        tab.split_and_insert(0, first_request, Arc::clone(&pane2))
            .unwrap();
        let second_request = mux::tab::SplitRequest {
            direction: mux::tab::SplitDirection::Horizontal,
            ..Default::default()
        };
        tab.split_and_insert(0, second_request, Arc::clone(&pane3))
            .unwrap();
        tab.set_layout_cycle(mux::layout::default_cycle());
        assert_eq!(tab.swap_to_layout_index(2).as_deref(), Some("stacked"));
        let slot_index = tab
            .first_nontrivial_stack_slot_index()
            .expect("three panes in the stacked layout must form a non-trivial stack");
        let stacked_pane_ids = tab.all_stacked_pane_ids();
        assert_eq!(
            stacked_pane_ids.len(),
            3,
            "layout redistribution must preserve every pane in the stack"
        );
        let pane_index = stacked_pane_ids
            .iter()
            .position(|pane_id| *pane_id == pane2.pane_id())
            .expect("the requested pane must remain addressable in the stack");
        let (_mux, _guard) = install_tab_with_window(
            &tab,
            &[Arc::clone(&pane1), Arc::clone(&pane2), Arc::clone(&pane3)],
        );
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 100,
            pdu: Pdu::SelectStackPane(SelectStackPane {
                tab_id: tab.tab_id(),
                slot_index,
                pane_index,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 100);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        let visible = tab.iter_panes_ignoring_zoom();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].pane.pane_id(), pane2.pane_id());
        assert_eq!(
            tab.all_stacked_pane_ids().len(),
            3,
            "queued dead-window pruning must retain every registered stack member"
        );
    }

    #[test]
    fn update_pane_constraints_pdu_updates_effective_constraints() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7, None));
        tab.assign_pane(&pane);
        let (_mux, _guard) = install_tab_with_window(&tab, &[]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 101,
            pdu: Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 7,
                min_width: Some(20),
                max_width: Some(200),
                min_height: None,
                max_height: Some(50),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 101);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        let constraints = tab.effective_pane_constraints(7).unwrap();
        assert_eq!(constraints.min_width, 20);
        assert_eq!(constraints.max_width, Some(200));
        assert_eq!(constraints.min_height, 3);
        assert_eq!(constraints.max_height, Some(50));
    }

    #[test]
    fn floating_pane_pdus_update_live_tab_state() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let tiled: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        tab.assign_pane(&tiled);
        let (mux, _guard) = install_tab_with_window(&tab, &[]);
        let floating: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        mux.add_pane(&floating).unwrap();
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let rect = mux::tab::FloatingPaneRect {
            left: 4,
            top: 5,
            width: 30,
            height: 12,
        };
        handler.process_one(DecodedPdu {
            serial: 110,
            pdu: Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id: tab.tab_id(),
                pane_id: 2,
                rect,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(tab.has_floating_pane(2));
        let floating_panes = tab.iter_floating_panes();
        assert_eq!(floating_panes.len(), 1);
        assert_eq!(floating_panes[0].left, 4);

        let moved = mux::tab::FloatingPaneRect {
            left: 10,
            top: 7,
            width: 24,
            height: 10,
        };
        handler.process_one(DecodedPdu {
            serial: 111,
            pdu: Pdu::MoveFloatingPane(MoveFloatingPane {
                pane_id: 2,
                rect: moved,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        let floating_panes = tab.iter_floating_panes();
        assert_eq!(floating_panes[0].left, 10);
        assert_eq!(floating_panes[0].top, 7);

        handler.process_one(DecodedPdu {
            serial: 112,
            pdu: Pdu::SetFloatingPaneZ(SetFloatingPaneZ {
                pane_id: 2,
                z_order: 99,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert_eq!(tab.iter_floating_panes()[0].z_order, 99);

        handler.process_one(DecodedPdu {
            serial: 113,
            pdu: Pdu::ToggleFloatingPane(ToggleFloatingPane {
                pane_id: 2,
                visible: false,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(!tab.iter_floating_panes()[0].visible);

        handler.process_one(DecodedPdu {
            serial: 114,
            pdu: Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 2 }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(!tab.has_floating_pane(2));
    }

    #[test]
    fn list_panes_releases_window_guard_before_observing_panes() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();

        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let window_id = *window;

        let armed = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(AtomicUsize::new(0));
        let weak_mux = Arc::downgrade(&mux);
        let armed_for_probe = Arc::clone(&armed);
        let observations_for_probe = Arc::clone(&observations);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if armed_for_probe.load(Ordering::Acquire) == 0 {
                return;
            }

            let mux = weak_mux.upgrade().expect("test mux remains live");
            assert!(
                mux.try_get_window_mut(window_id).is_some(),
                "ListPanes must release the enclosing mux window read guard \
                 before invoking Pane callbacks",
            );
            observations_for_probe.fetch_add(1, Ordering::AcqRel);
        });

        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(7_007, probe));
        let tab = Arc::new(mux::tab::Tab::new(&size));
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("register test tab and pane");
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to window");
        drop(window);

        armed.store(1, Ordering::Release);

        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));
        handler.process_one(DecodedPdu {
            serial: 121,
            pdu: Pdu::ListPanes(ListPanes {}),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 121);
        let pdu = response.pdu;
        let Pdu::ListPanesResponse(ListPanesResponse { tabs, .. }) = pdu else {
            panic!("expected ListPanesResponse, got {pdu:?}");
        };
        assert_eq!(tabs.len(), 1);
        assert!(
            observations.load(Ordering::Acquire) > 0,
            "the request must exercise the guarded pane-observation callback",
        );
    }

    #[test]
    fn coherent_list_panes_returns_exact_stream_session_and_snapshot_revision() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let expected_authority = mux
            .topology_snapshot_authority()
            .expect("fresh mux topology authority");
        let stream_id = TopologyStreamId::from_bytes([0x5a; 16]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session_with_topology_stream(
            sender,
            SessionOwner::new(mux),
            stream_id,
        );

        handler.process_one(DecodedPdu {
            serial: 122,
            pdu: Pdu::ListPanesCoherent(ListPanesCoherent {
                supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 122);
        let Pdu::ListPanesCoherentResponse(response) = response.pdu else {
            panic!("expected coherent ListPanes response");
        };
        assert_eq!(response.stream_id, stream_id);
        assert_eq!(
            response.negotiated,
            TopologyCapabilities::FENCED_SNAPSHOT_V1
        );
        let ListPanesCoherentOutcome::Snapshot(snapshot) = response.outcome else {
            panic!("expected an authoritative coherent pane snapshot");
        };
        assert_eq!(snapshot.session_incarnation, expected_authority.0);
        assert_eq!(snapshot.snapshot_revision, expected_authority.1);
        assert_eq!(snapshot.panes.tabs.len(), 0);
        assert_eq!(snapshot.panes.tab_titles.len(), 0);
        assert!(snapshot.panes.window_titles.is_empty());
    }

    #[test]
    fn ordered_snapshot_count_preflight_accepts_exact_limits_and_rejects_overflow() {
        assert_eq!(
            checked_ordered_snapshot_window_count(codec::MAX_ORDERED_WINDOWS_PER_SNAPSHOT),
            Ok(())
        );
        assert!(matches!(
            checked_ordered_snapshot_window_count(
                codec::MAX_ORDERED_WINDOWS_PER_SNAPSHOT + 1
            ),
            Err(codec::OrderedWindowProtocolError::TooManyWindows { count, max })
                if count == codec::MAX_ORDERED_WINDOWS_PER_SNAPSHOT + 1
                    && max == codec::MAX_ORDERED_WINDOWS_PER_SNAPSHOT
        ));
        assert_eq!(
            checked_ordered_snapshot_tab_total(codec::MAX_ORDERED_TABS_PER_SNAPSHOT - 1, 1,),
            Ok(codec::MAX_ORDERED_TABS_PER_SNAPSHOT)
        );
        assert!(matches!(
            checked_ordered_snapshot_tab_total(
                codec::MAX_ORDERED_TABS_PER_SNAPSHOT - 1,
                2,
            ),
            Err(codec::OrderedWindowProtocolError::TooManyTotalTabs { count, max })
                if count == codec::MAX_ORDERED_TABS_PER_SNAPSHOT + 1
                    && max == codec::MAX_ORDERED_TABS_PER_SNAPSHOT
        ));
        assert_eq!(
            checked_ordered_snapshot_tab_total(usize::MAX, 1),
            Err(codec::OrderedWindowProtocolError::CountOverflow)
        );
        assert_eq!(
            ordered_snapshot_tab_node_ceiling(0),
            Ok(codec::MAX_ORDERED_PANE_NODES_PER_TREE)
        );
        assert_eq!(
            ordered_snapshot_tab_node_ceiling(
                codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT - codec::MAX_ORDERED_PANE_NODES_PER_TREE
                    + 1,
            ),
            Ok(codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT)
        );
        assert!(matches!(
            ordered_snapshot_tab_node_ceiling(
                codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT + 1,
            ),
            Err(codec::OrderedWindowProtocolError::TooManyPaneNodes { count, max })
                if count == codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT + 1
                    && max == codec::MAX_ORDERED_PANE_NODES_PER_SNAPSHOT
        ));
    }

    #[test]
    fn ordered_snapshot_derives_panes_and_order_from_same_sorted_frozen_windows() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_501, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_502, None)));
        let first_window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        let second_window_id = attach_snapshot_tab_to_new_window(&mux, &second_tab);
        assert!(mux.set_window_title(first_window_id, "first-ordered-window"));
        assert!(mux.set_window_title(second_window_id, "second-ordered-window"));

        let snapshot = expect_current_ordered_snapshot(
            &mux,
            collect_ordered_list_panes_snapshot(&mux)
                .expect("stable mux must yield an ordered pane snapshot"),
        );
        snapshot
            .validate()
            .expect("complete ordered snapshot must satisfy the outbound contract");
        let expected_window_ids = {
            let mut ids = vec![first_window_id, second_window_id];
            ids.sort_unstable();
            ids.into_iter()
                .map(|window_id| {
                    codec::RemoteWindowId::new(
                        u64::try_from(window_id).expect("test window id fits u64"),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            snapshot
                .ordered_windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            expected_window_ids,
            "cross-window enumeration must be deterministic"
        );
        let pane_pairs = ordered_pane_tree_identities(&snapshot.panes);
        let ordered_pairs = snapshot
            .ordered_windows
            .iter()
            .flat_map(|window| {
                let window_id = usize::try_from(window.window_id.get())
                    .expect("test window id narrows to usize");
                window.ordered_tab_ids.iter().map(move |tab_id| {
                    (
                        window_id,
                        usize::try_from(tab_id.get()).expect("test tab id narrows to usize"),
                    )
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(pane_pairs, ordered_pairs);
        assert_eq!(snapshot.panes.trees().len(), ordered_pairs.len());
        for window in &snapshot.ordered_windows {
            assert_eq!(
                window.ordered_tab_ids.as_slice(),
                &[window.active_tab_id.unwrap()]
            );
        }

        let mut missing_tree = snapshot.clone();
        let (mut trees, mut nodes, window_titles) = missing_tree.panes.into_parts();
        let removed = trees.pop().expect("test snapshot has a second pane tree");
        let removed_start = usize::try_from(
            removed
                .root_index
                .expect("test pane-tree descriptor has a root"),
        )
        .expect("test pane-tree root narrows to usize");
        let removed_count =
            usize::try_from(removed.node_count).expect("test pane-tree node count narrows");
        assert_eq!(
            removed_start + removed_count,
            nodes.len(),
            "the removed canonical pane tree must own the trailing arena range"
        );
        nodes.truncate(removed_start);
        missing_tree.panes =
            mux::tab::PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        missing_tree
            .validate()
            .expect("the missing-tree fixture remains schema-valid before projection validation");
        assert!(
            format!(
                "{:#}",
                validate_ordered_snapshot_projection(&missing_tree)
                    .expect_err("a missing pane tree must fail complete PDU87 validation")
            )
            .contains("tab cardinality mismatch")
        );

        let mut mixed_leaf = snapshot.clone();
        let (mut trees, mut nodes, window_titles) = mixed_leaf.panes.into_parts();
        assert_eq!(trees[0].root_index, Some(0));
        assert_eq!(trees[0].node_count, 1);
        let first_entry = match &nodes[0] {
            mux::tab::PaneArenaNode::Leaf(entry) => entry.clone(),
            other => panic!("single-pane test tab must flatten as one leaf, got {other:?}"),
        };
        let mut wrong_second_entry = first_entry.clone();
        wrong_second_entry.tab_id = first_entry.tab_id.saturating_add(1_000_000);
        wrong_second_entry.is_active_pane = false;
        wrong_second_entry.is_zoomed_pane = false;
        let split = mux::tab::SplitDirectionAndSize {
            direction: mux::tab::SplitDirection::Horizontal,
            first: first_entry.size,
            second: first_entry.size,
        };
        nodes[0] = mux::tab::PaneArenaNode::Split {
            left: 1,
            right: 2,
            node: split,
        };
        nodes.insert(1, mux::tab::PaneArenaNode::Leaf(first_entry));
        nodes.insert(2, mux::tab::PaneArenaNode::Leaf(wrong_second_entry));
        trees[0].node_count = 3;
        for descriptor in trees.iter_mut().skip(1) {
            descriptor.root_index = descriptor.root_index.map(|root| {
                root.checked_add(2)
                    .expect("test pane-tree root adjustment fits u32")
            });
        }
        mixed_leaf.panes = mux::tab::PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        mixed_leaf
            .validate()
            .expect("the mixed-identity pane arena remains schema-valid");
        assert!(
            format!(
                "{:#}",
                validate_ordered_snapshot_projection(&mixed_leaf)
                    .expect_err("a mismatched second leaf must fail complete PDU87 validation")
            )
            .contains("contains a leaf outside expected")
        );

        let mut cross_wired = snapshot;
        let (trees, mut nodes, window_titles) = cross_wired.panes.into_parts();
        assert_eq!(nodes.len(), 2, "test snapshot has two single-leaf trees");
        nodes.swap(0, 1);
        cross_wired.panes =
            mux::tab::PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        cross_wired
            .validate()
            .expect("the cross-wired pane arena remains schema-valid");
        assert!(
            format!(
                "{:#}",
                validate_ordered_snapshot_projection(&cross_wired)
                    .expect_err("cross-wired pane and order vectors must fail PDU87 validation")
            )
            .contains("pane tree 0 identifies")
        );
    }

    #[test]
    fn ordered_snapshot_accepts_reorder_completed_before_initial_authority_cut() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_551, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_552, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        mux.add_tab_to_window(&second_tab, window_id)
            .expect("second authority-cut tab attaches exactly once");
        let before = mux
            .window_order_snapshot(window_id)
            .expect("authority-cut window order remains well formed")
            .expect("authority-cut window exists");
        let session_incarnation = mux
            .topology_snapshot_authority()
            .expect("authority-cut mux authority remains live")
            .0;
        let reorder = mux_reorder_request_for_snapshot_stage(
            &before,
            session_incarnation,
            1,
            vec![second_tab.tab_id(), first_tab.tab_id()],
        );

        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let mutator = thread::spawn(move || {
            barrier_for_mutator.mutate(|| mux_for_mutator.reorder_window_tabs(reorder))
        });

        let barrier_for_collector = Arc::clone(&barrier);
        let mut completed_attempts = 0usize;
        let outcome =
            collect_ordered_list_panes_snapshot_with_stage_observer(
                &mux,
                &mut |stage| match stage {
                    ListPanesSnapshotStage::BeforeOrderedAuthorityRead => {
                        barrier_for_collector.collector_arrive();
                    }
                    ListPanesSnapshotStage::TitlesCaptured => completed_attempts += 1,
                    ListPanesSnapshotStage::WindowsEnumerated
                    | ListPanesSnapshotStage::OrderedWindowsFrozen
                    | ListPanesSnapshotStage::TabTreeCaptured => {}
                },
            );
        let reorder_result = mutator
            .join()
            .expect("before-authority-cut mutator must not panic");
        assert!(matches!(
            reorder_result,
            mux::ReorderWindowTabsResult::Decision(mux::WindowReorderTerminalOutcome::Applied(_))
        ));
        let snapshot = expect_current_ordered_snapshot(
            &mux,
            outcome.expect("post-reorder authority cut must remain collectable"),
        );

        assert_eq!(
            completed_attempts, 1,
            "a mutation completed before the initial authority read belongs to the first cut"
        );
        assert_eq!(barrier.observations(), 1);
        let expected_tab_ids = vec![second_tab.tab_id(), first_tab.tab_id()];
        assert_eq!(
            snapshot.ordered_windows[0]
                .ordered_tab_ids
                .iter()
                .map(|tab_id| usize::try_from(tab_id.get()).expect("test tab id narrows"))
                .collect::<Vec<_>>(),
            expected_tab_ids
        );
        assert_eq!(
            ordered_pane_tree_identities(&snapshot.panes)
                .into_iter()
                .map(|(_, tab_id)| tab_id)
                .collect::<Vec<_>>(),
            expected_tab_ids,
            "pane projection must use the same post-reorder authority cut"
        );
    }

    #[test]
    fn ordered_snapshot_retries_reorder_after_all_window_orders_are_frozen() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_571, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_572, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        mux.add_tab_to_window(&second_tab, window_id)
            .expect("second frozen-cut tab attaches exactly once");
        let before = mux
            .window_order_snapshot(window_id)
            .expect("frozen-cut window order remains well formed")
            .expect("frozen-cut window exists");
        let session_incarnation = mux
            .topology_snapshot_authority()
            .expect("frozen-cut mux authority remains live")
            .0;
        let reorder = mux_reorder_request_for_snapshot_stage(
            &before,
            session_incarnation,
            2,
            vec![second_tab.tab_id(), first_tab.tab_id()],
        );

        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let mutator = thread::spawn(move || {
            barrier_for_mutator.mutate(|| mux_for_mutator.reorder_window_tabs(reorder))
        });

        let barrier_for_collector = Arc::clone(&barrier);
        let mut completed_attempts = 0usize;
        let outcome =
            collect_ordered_list_panes_snapshot_with_stage_observer(
                &mux,
                &mut |stage| match stage {
                    ListPanesSnapshotStage::OrderedWindowsFrozen => {
                        barrier_for_collector.collector_arrive();
                    }
                    ListPanesSnapshotStage::TitlesCaptured => completed_attempts += 1,
                    ListPanesSnapshotStage::BeforeOrderedAuthorityRead
                    | ListPanesSnapshotStage::WindowsEnumerated
                    | ListPanesSnapshotStage::TabTreeCaptured => {}
                },
            );
        let reorder_result = mutator
            .join()
            .expect("post-frozen-window-cut mutator must not panic");
        assert!(matches!(
            reorder_result,
            mux::ReorderWindowTabsResult::Decision(mux::WindowReorderTerminalOutcome::Applied(_))
        ));
        let snapshot = expect_current_ordered_snapshot(
            &mux,
            outcome.expect("post-frozen-window-cut retry must remain collectable"),
        );

        assert_eq!(
            completed_attempts, 2,
            "the authority check must reject the stale frozen order and retry once"
        );
        assert_eq!(barrier.observations(), 2);
        let expected_tab_ids = vec![second_tab.tab_id(), first_tab.tab_id()];
        assert_eq!(
            snapshot.ordered_windows[0]
                .ordered_tab_ids
                .iter()
                .map(|tab_id| usize::try_from(tab_id.get()).expect("test tab id narrows"))
                .collect::<Vec<_>>(),
            expected_tab_ids
        );
        assert_eq!(
            ordered_pane_tree_identities(&snapshot.panes)
                .into_iter()
                .map(|(_, tab_id)| tab_id)
                .collect::<Vec<_>>(),
            expected_tab_ids,
            "the accepted retry must project the post-reorder frozen identities"
        );
    }

    #[test]
    fn ordered_snapshot_retries_a_tab_tree_cut_without_mixed_order_authority() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_601, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_602, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let second_tab_for_mutator = Arc::clone(&second_tab);
        let mutator = thread::spawn(move || {
            barrier_for_mutator
                .mutate(|| mux_for_mutator.add_tab_to_window(&second_tab_for_mutator, window_id))
        });

        let barrier_for_collector = Arc::clone(&barrier);
        let mut completed_attempts = 0usize;
        let outcome =
            collect_ordered_list_panes_snapshot_with_stage_observer(
                &mux,
                &mut |stage| match stage {
                    ListPanesSnapshotStage::TabTreeCaptured => {
                        barrier_for_collector.collector_arrive();
                    }
                    ListPanesSnapshotStage::TitlesCaptured => completed_attempts += 1,
                    ListPanesSnapshotStage::BeforeOrderedAuthorityRead
                    | ListPanesSnapshotStage::WindowsEnumerated
                    | ListPanesSnapshotStage::OrderedWindowsFrozen => {}
                },
            );
        mutator
            .join()
            .expect("ordered snapshot mutator must not panic")
            .expect("successor tab must attach exactly once");
        let snapshot = expect_current_ordered_snapshot(
            &mux,
            outcome.expect("ordered snapshot retry must remain collectable"),
        );

        assert_eq!(
            completed_attempts, 2,
            "the stale first attempt must be retried"
        );
        assert_eq!(snapshot.ordered_windows.len(), 1);
        assert_eq!(snapshot.ordered_windows[0].ordered_tab_ids.len(), 2);
        assert_eq!(snapshot.panes.trees().len(), 2);
        let pane_tab_ids = ordered_pane_tree_identities(&snapshot.panes)
            .into_iter()
            .map(|(_, tab_id)| tab_id)
            .collect::<Vec<_>>();
        let ordered_tab_ids = snapshot.ordered_windows[0]
            .ordered_tab_ids
            .iter()
            .map(|tab_id| usize::try_from(tab_id.get()).expect("test tab id narrows"))
            .collect::<Vec<_>>();
        assert_eq!(pane_tab_ids, ordered_tab_ids);
    }

    #[test]
    fn pdu86_remains_dormant_under_runtime_capabilities_and_preserves_serial() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let stream_id = TopologyStreamId::from_bytes([0x86; 16]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session_with_topology_stream(
            sender,
            SessionOwner::new(mux),
            stream_id,
        );

        let request = ordered_snapshot_request(true);
        handler.process_one(DecodedPdu {
            serial: 186,
            pdu: Pdu::ListPanesOrderedV1(request.clone()),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 186);
        let Pdu::ListPanesOrderedV1Response(response) = response.pdu else {
            panic!("expected a typed PDU87 response");
        };
        response
            .validate_for_request(&request)
            .expect("dormant unsupported PDU87 must remain request-correlated");
        assert_eq!(response.stream_id, stream_id);
        assert!(matches!(
            response.outcome,
            codec::ListPanesOrderedV1Outcome::Unsupported {
                supported: TopologyCapabilities::SERVER_SUPPORTED
            }
        ));
    }

    #[test]
    fn future_enabled_reorder_maps_apply_and_replay_then_rejects_foreign_authority() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_701, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_702, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        mux.add_tab_to_window(&second_tab, window_id)
            .expect("second test tab attaches to reorder window");
        let before = mux
            .window_order_snapshot(window_id)
            .expect("test window snapshot is well formed")
            .expect("test window exists");
        let (session_incarnation, before_topology_revision) = mux
            .topology_snapshot_authority()
            .expect("test mux topology authority remains live");
        let stream_id = TopologyStreamId::from_bytes([0x88; 16]);
        let domain_binding_id = codec::DomainBindingId::from_bytes([0xd1; 16]);
        let connection_authority = established_ordered_window_authority_for_test(
            stream_id,
            session_incarnation,
            domain_binding_id,
            ordered_window_capabilities(true),
        );
        let request = reorder_request_for_snapshot(
            &before,
            session_incarnation,
            stream_id,
            domain_binding_id,
            1,
            vec![second_tab.tab_id(), first_tab.tab_id()],
        );
        let capability_only_authority = established_ordered_window_authority_for_test(
            stream_id,
            session_incarnation,
            domain_binding_id,
            ordered_window_capabilities(false),
        );
        let mut foreign_stream_without_capability = request.clone();
        foreign_stream_without_capability.stream_id = TopologyStreamId::from_bytes([0x87; 16]);
        let error = admit_reorder_transport(
            &foreign_stream_without_capability,
            capability_only_authority,
        )
        .expect_err("missing capability must outrank foreign stream identity");
        assert!(format!("{error:#}").contains("capability is not established"));

        let applied =
            process_reorder_window_tabs_request(&mux, &request, connection_authority, None)
                .expect("authorized exact permutation must yield PDU89");
        applied
            .validate()
            .expect("applied PDU89 must satisfy the complete outbound contract");
        let codec::ReorderWindowTabsV1Outcome::Applied(commit) = &applied.outcome else {
            panic!("expected applied reorder, got {:?}", applied.outcome);
        };
        assert!(commit.topology_revision > before_topology_revision);
        assert_eq!(
            commit
                .window
                .ordered_tab_ids
                .iter()
                .map(|tab_id| usize::try_from(tab_id.get()).unwrap())
                .collect::<Vec<_>>(),
            vec![second_tab.tab_id(), first_tab.tab_id()]
        );
        let replay =
            process_reorder_window_tabs_request(&mux, &request, connection_authority, None)
                .expect("exact retry must map to a replay PDU89");
        assert!(matches!(
            replay.outcome,
            codec::ReorderWindowTabsV1Outcome::Replay(
                codec::WindowReorderTerminalOutcomeV1::Applied(_)
            )
        ));

        let after_replay = mux
            .window_order_snapshot(window_id)
            .expect("post-replay window remains valid")
            .expect("post-replay window remains present");
        let topology_after_replay = mux
            .topology_snapshot_authority()
            .expect("post-replay topology remains live")
            .1;
        let foreign_domain_request = reorder_request_for_snapshot(
            &after_replay,
            session_incarnation,
            stream_id,
            codec::DomainBindingId::from_bytes([0xd2; 16]),
            2,
            after_replay.ordered_tab_ids().collect(),
        );
        let foreign_domain = process_reorder_window_tabs_request(
            &mux,
            &foreign_domain_request,
            connection_authority,
            None,
        )
        .expect("foreign domain receives a typed non-mutating decision");
        assert_eq!(
            foreign_domain.outcome,
            codec::ReorderWindowTabsV1Outcome::Malformed
        );
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("foreign-domain rejection preserves topology")
                .1,
            topology_after_replay
        );
        assert_eq!(
            mux.window_order_snapshot(window_id)
                .expect("foreign-domain rejection preserves valid order")
                .expect("foreign-domain rejection preserves window")
                .order_revision(),
            after_replay.order_revision()
        );

        let mut stale_session_request = reorder_request_for_snapshot(
            &after_replay,
            mux::MuxSessionIncarnation::from_bytes([0xee; 16]),
            stream_id,
            codec::DomainBindingId::from_bytes([0xd2; 16]),
            3,
            after_replay.ordered_tab_ids().collect(),
        );
        stale_session_request = stale_session_request.with_computed_digest();
        let stale_session = process_reorder_window_tabs_request(
            &mux,
            &stale_session_request,
            connection_authority,
            None,
        )
        .expect("stale session receives a typed non-mutating decision");
        assert_eq!(
            stale_session.outcome,
            codec::ReorderWindowTabsV1Outcome::StaleIncarnation,
            "stale session authority must outrank a foreign established binding",
        );
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("stale-session rejection preserves topology")
                .1,
            topology_after_replay
        );

        let mut foreign_stream_request = foreign_domain_request;
        foreign_stream_request.domain_binding_id = domain_binding_id;
        foreign_stream_request.stream_id = TopologyStreamId::from_bytes([0x89; 16]);
        foreign_stream_request.mutation_id = codec::WindowOrderMutationId::new([0x9d; 16], 4);
        foreign_stream_request = foreign_stream_request.with_computed_digest();
        let error = process_reorder_window_tabs_request(
            &mux,
            &foreign_stream_request,
            connection_authority,
            None,
        )
        .expect_err("foreign stream must fail before mux mutation");
        assert!(format!("{error:#}").contains("stale or foreign topology stream"));
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("foreign-stream rejection preserves topology")
                .1,
            topology_after_replay
        );

        let mut forged_digest_request = reorder_request_for_snapshot(
            &after_replay,
            session_incarnation,
            stream_id,
            domain_binding_id,
            5,
            after_replay.ordered_tab_ids().collect(),
        );
        forged_digest_request.digest = codec::WindowReorderDigest::ZERO;
        let error = process_reorder_window_tabs_request(
            &mux,
            &forged_digest_request,
            connection_authority,
            None,
        )
        .expect_err("forged digest must fail before mux mutation");
        assert!(format!("{error:#}").contains("digest"));
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("forged-digest rejection preserves topology")
                .1,
            topology_after_replay
        );

        let bound_domain = codec::DomainBindingId::from_bytes([0xd3; 16]);
        let immutable_authority = established_ordered_window_authority_for_test(
            stream_id,
            session_incarnation,
            bound_domain,
            ordered_window_capabilities(true),
        );
        let foreign_domain = codec::DomainBindingId::from_bytes([0xd4; 16]);
        let foreign_malformed_first = reorder_request_for_snapshot(
            &after_replay,
            session_incarnation,
            stream_id,
            foreign_domain,
            6,
            vec![first_tab.tab_id(), first_tab.tab_id()],
        );
        let foreign_malformed_response = process_reorder_window_tabs_request(
            &mux,
            &foreign_malformed_first,
            immutable_authority,
            None,
        )
        .expect("foreign malformed request receives a typed non-mutating response");
        assert_eq!(
            foreign_malformed_response.outcome,
            codec::ReorderWindowTabsV1Outcome::Malformed
        );
        assert_eq!(immutable_authority.domain_binding_id(), bound_domain);

        let correct_binding_reuses_unseeded_mutation = reorder_request_for_snapshot(
            &after_replay,
            session_incarnation,
            stream_id,
            bound_domain,
            6,
            vec![first_tab.tab_id(), second_tab.tab_id()],
        );
        let correctly_bound_applied = process_reorder_window_tabs_request(
            &mux,
            &correct_binding_reuses_unseeded_mutation,
            immutable_authority,
            None,
        )
        .expect("foreign binding rejection must not seed the mux receipt ledger");
        assert!(
            matches!(
                correctly_bound_applied.outcome,
                codec::ReorderWindowTabsV1Outcome::Applied(_)
            ),
            "the correct binding must remain free to use the same unseeded mutation identity",
        );

        let after_correct_binding = mux
            .window_order_snapshot(window_id)
            .expect("post-binding test window remains valid")
            .expect("post-binding test window remains present");
        let bound_malformed = reorder_request_for_snapshot(
            &after_correct_binding,
            session_incarnation,
            stream_id,
            bound_domain,
            7,
            vec![first_tab.tab_id(), first_tab.tab_id()],
        );
        let bound_malformed_response =
            process_reorder_window_tabs_request(&mux, &bound_malformed, immutable_authority, None)
                .expect("correctly bound semantic malformed request receives a typed response");
        assert_eq!(
            bound_malformed_response.outcome,
            codec::ReorderWindowTabsV1Outcome::Malformed
        );
        let replay_malformed =
            process_reorder_window_tabs_request(&mux, &bound_malformed, immutable_authority, None)
                .expect("exact semantic-malformed retry must remain replayable");
        assert!(matches!(
            replay_malformed.outcome,
            codec::ReorderWindowTabsV1Outcome::Replay(
                codec::WindowReorderTerminalOutcomeV1::Malformed
            )
        ));
    }

    #[test]
    fn coherent_snapshot_retries_window_enumeration_cut_without_partial_topology() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_101, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_102, None)));
        let first_window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let second_tab_for_mutator = Arc::clone(&second_tab);
        let mutator = thread::spawn(move || {
            barrier_for_mutator.mutate(|| {
                let window = mux_for_mutator.new_empty_window(None, None);
                let window_id = *window;
                let attach_result =
                    mux_for_mutator.add_tab_to_window(&second_tab_for_mutator, window_id);
                drop(window);
                (window_id, attach_result)
            })
        });

        let barrier_for_collector = Arc::clone(&barrier);
        let mut completed_attempts = 0usize;
        let outcome = collect_coherent_list_panes_snapshot_with_stage_observer(
            &mux,
            &mut |stage| match stage {
                ListPanesSnapshotStage::WindowsEnumerated => {
                    barrier_for_collector.collector_arrive();
                }
                ListPanesSnapshotStage::TitlesCaptured => completed_attempts += 1,
                ListPanesSnapshotStage::BeforeOrderedAuthorityRead
                | ListPanesSnapshotStage::OrderedWindowsFrozen
                | ListPanesSnapshotStage::TabTreeCaptured => {}
            },
        );
        let mutator_result = mutator
            .join()
            .expect("window-enumeration mutator must finish without panic");
        let (second_window_id, attach_result) = mutator_result;
        attach_result.expect("concurrent successor window must retain its tab");
        let snapshot = expect_current_coherent_snapshot(
            &mux,
            outcome.expect("window-enumeration retry must remain collectable"),
        );

        assert_eq!(completed_attempts, 2, "the first cut must be retried once");
        assert_eq!(barrier.observations(), 2);
        assert_eq!(snapshot.panes.tabs.len(), 2);
        assert_eq!(snapshot.panes.tab_titles.len(), 2);
        assert_eq!(snapshot.panes.window_titles.len(), 2);
        let snapshot_pairs = snapshot
            .panes
            .tabs
            .iter()
            .filter_map(mux::tab::PaneNode::window_and_tab_ids)
            .collect::<HashSet<_>>();
        assert_eq!(
            snapshot_pairs,
            HashSet::from([
                (first_window_id, first_tab.tab_id()),
                (second_window_id, second_tab.tab_id()),
            ]),
            "the accepted retry must contain both complete window/tab generations"
        );
    }

    #[test]
    fn authoritative_snapshot_families_preserve_complete_floating_pane_state() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_151, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &tab);
        let floating: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7_152, None));
        mux.add_pane(&floating)
            .expect("register floating snapshot pane");
        let rect = mux::tab::FloatingPaneRect {
            left: 9,
            top: 4,
            width: 37,
            height: 13,
        };
        tab.add_floating_pane(Arc::clone(&floating), rect)
            .expect("attach floating snapshot pane");
        assert!(tab.set_floating_pane_z_order(floating.pane_id(), 71));

        let legacy = collect_list_panes_snapshot(&mux).expect("collect legacy pane snapshot");
        let coherent = expect_current_coherent_snapshot(
            &mux,
            collect_coherent_list_panes_snapshot(&mux).expect("collect coherent pane snapshot"),
        );
        let ordered = match collect_ordered_list_panes_snapshot(&mux)
            .expect("collect ordered pane snapshot")
        {
            codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) => snapshot,
            other => panic!("expected ordered pane snapshot, got {other:?}"),
        };

        assert_eq!(legacy.floating_panes, coherent.panes.floating_panes);
        assert_eq!(legacy.floating_panes, ordered.floating_panes);
        let [snapshot] = legacy.floating_panes.as_slice() else {
            panic!("expected one floating pane in every authoritative snapshot");
        };
        assert_eq!(snapshot.pane.window_id, window_id);
        assert_eq!(snapshot.pane.tab_id, tab.tab_id());
        assert_eq!(snapshot.pane.pane_id, floating.pane_id());
        assert_eq!(snapshot.pane.left_col, rect.left);
        assert_eq!(snapshot.pane.top_row, rect.top);
        assert_eq!(snapshot.pane.size.cols, rect.width);
        assert_eq!(snapshot.pane.size.rows, rect.height);
        assert_eq!(snapshot.rect, rect);
        assert_eq!(snapshot.z_order, 71);
        assert!(snapshot.visible);
        assert!(!snapshot.pinned);
        assert_eq!(snapshot.opacity.to_bits(), 1.0_f32.to_bits());
        assert!(snapshot.focused);
        assert!(snapshot.pane.is_active_pane);
        assert!(!snapshot.pane.is_zoomed_pane);
    }

    #[test]
    fn coherent_snapshot_retries_a_floating_geometry_callback_cut_before_validation() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_161, None)));
        attach_snapshot_tab_to_new_window(&mux, &tab);
        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_probe = Arc::clone(&barrier);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            barrier_for_probe.collector_arrive();
        });
        let floating: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(7_162, probe));
        mux.add_pane(&floating)
            .expect("register floating callback-cut pane");
        let initial = mux::tab::FloatingPaneRect {
            left: 5,
            top: 3,
            width: 31,
            height: 11,
        };
        tab.add_floating_pane(Arc::clone(&floating), initial)
            .expect("attach floating callback-cut pane");

        let replacement = mux::tab::FloatingPaneRect {
            left: 13,
            top: 7,
            width: 27,
            height: 9,
        };
        let barrier_for_mutator = Arc::clone(&barrier);
        let tab_for_mutator = Arc::clone(&tab);
        let mutator = thread::spawn(move || {
            barrier_for_mutator
                .mutate(|| tab_for_mutator.set_floating_pane_rect(7_162, replacement))
        });

        let mut completed_attempts = 0usize;
        let outcome =
            collect_coherent_list_panes_snapshot_with_stage_observer(&mux, &mut |stage| {
                if stage == ListPanesSnapshotStage::TitlesCaptured {
                    completed_attempts += 1;
                }
            });
        assert!(
            mutator
                .join()
                .expect("floating callback-cut mutator must finish")
                .is_some(),
            "floating callback cut must update the pane"
        );
        let snapshot = expect_current_coherent_snapshot(
            &mux,
            outcome.expect("floating callback-cut retry must remain collectable"),
        );

        assert_eq!(completed_attempts, 2, "the mixed geometry cut must retry");
        assert!(barrier.observations() >= 2);
        let [floating] = snapshot.panes.floating_panes.as_slice() else {
            panic!("accepted retry must contain the floating pane");
        };
        assert_eq!(floating.rect, replacement);
        assert_eq!(floating.pane.left_col, replacement.left);
        assert_eq!(floating.pane.top_row, replacement.top);
        assert_eq!(floating.pane.size.cols, replacement.width);
        assert_eq!(floating.pane.size.rows, replacement.height);
    }

    #[test]
    fn coherent_snapshot_retries_tab_tree_cut_without_mismatched_titles() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let first_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_201, None)));
        let second_tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_202, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &first_tab);
        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let second_tab_for_mutator = Arc::clone(&second_tab);
        let mutator = thread::spawn(move || {
            barrier_for_mutator
                .mutate(|| mux_for_mutator.add_tab_to_window(&second_tab_for_mutator, window_id))
        });

        let barrier_for_collector = Arc::clone(&barrier);
        let mut completed_attempts = 0usize;
        let outcome = collect_coherent_list_panes_snapshot_with_stage_observer(
            &mux,
            &mut |stage| match stage {
                ListPanesSnapshotStage::TabTreeCaptured => {
                    barrier_for_collector.collector_arrive();
                }
                ListPanesSnapshotStage::TitlesCaptured => completed_attempts += 1,
                ListPanesSnapshotStage::BeforeOrderedAuthorityRead
                | ListPanesSnapshotStage::WindowsEnumerated
                | ListPanesSnapshotStage::OrderedWindowsFrozen => {}
            },
        );
        let attach_result = mutator
            .join()
            .expect("tab-tree mutator must finish without panic");
        attach_result.expect("concurrent successor tab must attach exactly once");
        let snapshot = expect_current_coherent_snapshot(
            &mux,
            outcome.expect("tab-tree retry must remain collectable"),
        );

        assert_eq!(completed_attempts, 2, "the first cut must be retried once");
        assert_eq!(barrier.observations(), 3);
        assert_eq!(snapshot.panes.tabs.len(), 2);
        assert_eq!(snapshot.panes.tab_titles.len(), snapshot.panes.tabs.len());
        let snapshot_tab_ids = snapshot
            .panes
            .tabs
            .iter()
            .filter_map(mux::tab::PaneNode::window_and_tab_ids)
            .map(|(observed_window_id, tab_id)| {
                assert_eq!(observed_window_id, window_id);
                tab_id
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            snapshot_tab_ids,
            HashSet::from([first_tab.tab_id(), second_tab.tab_id()]),
            "the accepted retry must align both pane trees with both tab titles"
        );
    }

    #[test]
    fn coherent_snapshot_retries_pane_callback_cut_without_stale_window_title() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let barrier = NoSleepSnapshotBarrier::shared();
        let armed = Arc::new(AtomicUsize::new(0));
        let barrier_for_probe = Arc::clone(&barrier);
        let armed_for_probe = Arc::clone(&armed);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if armed_for_probe.load(Ordering::Acquire) != 0 {
                barrier_for_probe.collector_arrive();
            }
        });
        let tab = register_snapshot_tab(
            &mux,
            Arc::new(FakePane::new_with_callback_probe(7_301, probe)),
        );
        let window_id = attach_snapshot_tab_to_new_window(&mux, &tab);
        assert!(mux.set_window_title(window_id, "before-pane-callback-cut"));
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let mutator = thread::spawn(move || {
            barrier_for_mutator
                .mutate(|| mux_for_mutator.set_window_title(window_id, "after-pane-callback-cut"))
        });

        armed.store(1, Ordering::Release);
        let mut completed_attempts = 0usize;
        let outcome =
            collect_coherent_list_panes_snapshot_with_stage_observer(&mux, &mut |stage| {
                if stage == ListPanesSnapshotStage::TitlesCaptured {
                    completed_attempts += 1;
                }
            });
        armed.store(0, Ordering::Release);
        let title_changed = mutator
            .join()
            .expect("pane-callback mutator must finish without panic");
        assert!(title_changed, "the pane callback cut must mutate topology");
        let snapshot = expect_current_coherent_snapshot(
            &mux,
            outcome.expect("pane-callback retry must remain collectable"),
        );

        assert_eq!(completed_attempts, 2, "the first cut must be retried once");
        assert!(barrier.observations() >= 2);
        assert_eq!(
            snapshot
                .panes
                .window_titles
                .get(&window_id)
                .map(String::as_str),
            Some("after-pane-callback-cut"),
            "the stale pre-callback title from the rejected attempt must not escape"
        );
    }

    #[test]
    fn coherent_snapshot_retries_title_capture_cut_before_final_revision_validation() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let tab = register_snapshot_tab(&mux, Arc::new(FakePane::new_with_id(7_401, None)));
        let window_id = attach_snapshot_tab_to_new_window(&mux, &tab);
        assert!(mux.set_window_title(window_id, "before-title-capture-cut"));
        let barrier = NoSleepSnapshotBarrier::shared();
        let barrier_for_mutator = Arc::clone(&barrier);
        let mux_for_mutator = Arc::clone(&mux);
        let mutator = thread::spawn(move || {
            barrier_for_mutator
                .mutate(|| mux_for_mutator.set_window_title(window_id, "after-title-capture-cut"))
        });

        let barrier_for_collector = Arc::clone(&barrier);
        let mut completed_attempts = 0usize;
        let outcome =
            collect_coherent_list_panes_snapshot_with_stage_observer(&mux, &mut |stage| {
                if stage == ListPanesSnapshotStage::TitlesCaptured {
                    completed_attempts += 1;
                    barrier_for_collector.collector_arrive();
                }
            });
        let title_changed = mutator
            .join()
            .expect("title-capture mutator must finish without panic");
        assert!(title_changed, "the title capture cut must mutate topology");
        let snapshot = expect_current_coherent_snapshot(
            &mux,
            outcome.expect("title-capture retry must remain collectable"),
        );

        assert_eq!(completed_attempts, 2, "the first cut must be retried once");
        assert_eq!(barrier.observations(), 2);
        assert_eq!(
            snapshot
                .panes
                .window_titles
                .get(&window_id)
                .map(String::as_str),
            Some("after-title-capture-cut"),
            "final revision validation must reject the already-captured stale title"
        );
    }

    #[test]
    fn coherent_list_panes_reports_typed_contention_after_three_moving_attempts() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let weak_mux = Arc::downgrade(&mux);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(mux) = weak_mux.upgrade() {
                mux.notify(mux::MuxNotification::Empty);
            }
        });
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(7_008, probe));
        let tab = Arc::new(mux::tab::Tab::new(&test_tab_size()));
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("register moving-snapshot test tab");
        let window = mux.new_empty_window(None, None);
        mux.add_tab_to_window(&tab, *window)
            .expect("attach moving-snapshot test tab");
        drop(window);

        let stream_id = TopologyStreamId::from_bytes([0x6b; 16]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session_with_topology_stream(
            sender,
            SessionOwner::new(mux),
            stream_id,
        );
        handler.process_one(DecodedPdu {
            serial: 123,
            pdu: Pdu::ListPanesCoherent(ListPanesCoherent {
                supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        let Pdu::ListPanesCoherentResponse(response) = response.pdu else {
            panic!("expected coherent ListPanes response");
        };
        let ListPanesCoherentOutcome::Contended {
            attempts,
            first_revision,
            last_revision,
        } = response.outcome
        else {
            panic!("moving topology must not mint a coherent snapshot");
        };
        assert_eq!(attempts, COHERENT_SNAPSHOT_ATTEMPTS);
        assert!(last_revision > first_revision);
    }

    #[test]
    fn coherent_list_panes_rejects_unknown_required_capability_without_authority() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let stream_id = TopologyStreamId::from_bytes([0x7c; 16]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session_with_topology_stream(
            sender,
            SessionOwner::new(mux),
            stream_id,
        );
        handler.process_one(DecodedPdu {
            serial: 124,
            pdu: Pdu::ListPanesCoherent(ListPanesCoherent {
                supported: TopologyCapabilities::from_bits(
                    TopologyCapabilities::FENCED_SNAPSHOT_V1.bits() | (1 << 63),
                ),
                required: TopologyCapabilities::from_bits(1 << 63),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        let Pdu::ListPanesCoherentResponse(response) = response.pdu else {
            panic!("expected coherent ListPanes response");
        };
        assert_eq!(response.stream_id, stream_id);
        assert!(matches!(
            response.outcome,
            ListPanesCoherentOutcome::Unsupported {
                supported: TopologyCapabilities::SERVER_SUPPORTED
            }
        ));
    }

    #[test]
    fn list_panes_tab_stacks_reports_window_stack_membership() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let first = Arc::new(mux::tab::Tab::new(&size));
        let second = Arc::new(mux::tab::Tab::new(&size));
        let first_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        let second_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        first.assign_pane(&first_pane);
        second.assign_pane(&second_pane);
        let first_id = first.tab_id();
        let second_id = second.tab_id();
        let (mux, _guard) = install_tab_with_window(&first, &[]);
        let window_id = mux.iter_windows()[0];
        mux.add_tab_and_active_pane(&second).unwrap();
        mux.add_tab_to_window(&second, window_id).unwrap();
        mux.get_window_mut(window_id)
            .unwrap()
            .create_tab_stack(mux::tab::TabStackId(7), vec![first_id, second_id])
            .unwrap();

        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        handler.process_one(DecodedPdu {
            serial: 120,
            pdu: Pdu::ListPanesTabStacks(ListPanesTabStacks {}),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 120);
        match resp.pdu {
            Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse { tab_stack_entries }) => {
                assert_eq!(
                    tab_stack_entries,
                    vec![
                        ListPanesTabStackEntry {
                            window_id,
                            stack_id: mux::tab::TabStackId(7),
                            tab_id: first_id,
                            position: 0,
                            is_visible: true,
                        },
                        ListPanesTabStackEntry {
                            window_id,
                            stack_id: mux::tab::TabStackId(7),
                            tab_id: second_id,
                            position: 1,
                            is_visible: false,
                        },
                    ]
                );
            }
            other => panic!("expected ListPanesTabStacksResponse, got {other:?}"),
        }
    }

    #[test]
    fn invalid_pdu_returns_error_response() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 200,
            pdu: Pdu::Invalid { ident: 255 },
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 200);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("invalid PDU"),
                    "error should mention invalid PDU, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn ordered_window_request_returns_typed_unsupported_until_live_capability_activation() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let required = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        );

        let request = codec::ListPanesOrderedV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: codec::DomainBindingId::from_bytes([0xd1; 16]),
            supported: required,
            required,
        };
        handler.process_one(DecodedPdu {
            serial: 201,
            pdu: Pdu::ListPanesOrderedV1(request.clone()),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 201);
        match response.pdu {
            Pdu::ListPanesOrderedV1Response(response) => {
                response
                    .validate_for_request(&request)
                    .expect("dormant ordered response must remain request-correlated");
                assert!(matches!(
                    response.outcome,
                    codec::ListPanesOrderedV1Outcome::Unsupported {
                        supported: TopologyCapabilities::SERVER_SUPPORTED
                    }
                ));
            }
            other => panic!("expected typed PDU87 Unsupported, got {other:?}"),
        }
    }

    fn exact_render_request_for_session_test() -> codec::GetPaneRenderDeliveryV1 {
        codec::GetPaneRenderDeliveryV1 {
            protocol_version: codec::EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
            identity: codec::ExactRenderDeliveryRequestIdentity {
                connection_identity: codec::RenderConnectionIdentity::new(
                    TopologyStreamId::from_bytes([0x91; 16]),
                    mux::MuxSessionIncarnation::from_bytes([0x52; 16]),
                ),
                pane_id: codec::ExactRenderPaneId::new(0),
                request_sequence: codec::ExactRenderRequestSequence::try_new(1)
                    .expect("test exact-render request sequence is nonzero"),
            },
            request_digest: codec::ExactRenderDigest::ZERO,
            applied_baseline: codec::ExactRenderAppliedBaseline::Uninitialized,
            settlement: None,
            mode: codec::ExactRenderDeliveryMode::ForceFull,
            receiver_caps: codec::ExactRenderReceiverCaps::protocol_maximum(),
            continuation: None,
        }
        .with_computed_request_digest()
        .expect("test exact-render request digest should compute")
    }

    #[test]
    fn exact_render_request_fails_closed_until_live_retention_activation() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 202,
            pdu: Pdu::GetPaneRenderDeliveryV1(exact_render_request_for_session_test()),
        });

        let response = take_response(&captured);
        assert_eq!(response.serial, 202);
        match response.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => assert!(
                reason.contains("live retention and settlement coordinator"),
                "exact-render request must retain its fail-closed activation reason, got: \
                 {reason}"
            ),
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn exact_render_response_is_treated_as_unexpected_client_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let request = exact_render_request_for_session_test();

        handler.process_one(DecodedPdu {
            serial: 203,
            pdu: Pdu::GetPaneRenderDeliveryV1Response(codec::GetPaneRenderDeliveryV1Response {
                protocol_version: codec::EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
                request_identity: request.identity,
                request_digest: request.request_digest,
                outcome: codec::ExactRenderDeliveryOutcomeV1::PaneRemoved {
                    last_pane_generation: None,
                },
            }),
        });

        let response = take_response(&captured);
        assert_eq!(response.serial, 203);
        match response.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => assert!(
                reason.contains("expected a request"),
                "exact-render server response must be rejected as client input, got: {reason}"
            ),
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_pdu_treated_as_unexpected_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        // Sending a Pong (which is a response) as a request should get rejected
        handler.process_one(DecodedPdu {
            serial: 300,
            pdu: Pdu::Pong(Pong {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 300);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("expected a request"),
                    "error should mention expected request, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn unit_response_treated_as_unexpected_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 301,
            pdu: Pdu::UnitResponse(UnitResponse {}),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn pdu_sender_propagates_errors() {
        let sender = PduSender::new(|_, _class| anyhow::bail!("send failed"));
        let result = sender.send_control(DecodedPdu {
            serial: 0,
            pdu: Pdu::Pong(Pong {}),
        });
        assert!(result.is_err());
    }

    #[test]
    fn ordinary_request_response_uses_control_delivery() {
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let sender = PduSender::new({
            let deliveries = Arc::clone(&deliveries);
            move |pdu, class| {
                deliveries.lock().unwrap().push((pdu, class));
                Ok(())
            }
        });
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 77,
            pdu: Pdu::Ping(Ping {}),
        });

        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].0.serial, 77);
        assert_eq!(deliveries[0].0.pdu, Pdu::Pong(Pong {}));
        assert_eq!(deliveries[0].1, PduDeliveryClass::Control);
    }

    #[test]
    fn serial_preserved_across_different_pdus() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        // Send multiple synchronous PDUs with distinct serials
        for serial in [1, 100, 999, u64::MAX] {
            handler.process_one(DecodedPdu {
                serial,
                pdu: Pdu::Ping(Ping {}),
            });
        }

        let pdus = captured.lock().unwrap();
        let serials: Vec<u64> = pdus.iter().map(|p| p.serial).collect();
        assert_eq!(serials, vec![1, 100, 999, u64::MAX]);
    }

    #[test]
    fn update_pane_constraints_all_none_keeps_default_constraints() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        tab.assign_pane(&pane);
        let (_mux, _guard) = install_tab_with_window(&tab, &[]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 50,
            pdu: Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 1,
                min_width: None,
                max_width: None,
                min_height: None,
                max_height: None,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert_eq!(
            tab.effective_pane_constraints(1).unwrap(),
            mux::pane::PaneConstraints::default()
        );
    }

    #[test]
    fn error_response_pdu_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 400,
            pdu: Pdu::ErrorResponse(ErrorResponse {
                reason: "test error".to_string(),
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn list_panes_response_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 401,
            pdu: Pdu::ListPanesResponse(ListPanesResponse {
                tabs: vec![],
                tab_titles: vec![],
                window_titles: HashMap::new(),
                floating_panes: Vec::new(),
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn list_panes_tab_stacks_response_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 402,
            pdu: Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse {
                tab_stack_entries: vec![],
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    fn test_client_id(name: &str, pid: u32) -> ClientId {
        ClientId {
            hostname: format!("{name}.local"),
            username: "testuser".to_string(),
            pid,
            epoch: 1000,
            id: 0,
            ssh_auth_sock: None,
        }
    }

    #[derive(Clone, Debug)]
    enum LifecycleOp {
        Claim { slot: usize, client_variant: usize },
        Release { slot: usize },
        Ping { slot: usize },
    }

    #[derive(Clone, Debug)]
    enum ReconnectOp {
        Connect { slot: usize, client_variant: usize },
        Release { slot: usize },
        Ping { slot: usize },
    }

    #[derive(Clone, Debug)]
    struct ReconnectRaceStep {
        client_variant: usize,
        drop_stale_before_reconnect: bool,
        drop_stale_after_reconnect: usize,
    }

    #[derive(Clone, Debug)]
    enum ProxyIdentityOp {
        Proxy { proxy_variant: usize },
        RealClient { client_variant: usize },
    }

    #[derive(Clone, Debug)]
    enum ActiveWorkspaceOp {
        SetClient { client_variant: usize },
        SetWorkspace { workspace_variant: usize },
    }

    #[derive(Clone, Debug)]
    enum MissingMuxPaneOp {
        WriteToPane { pane_id: PaneId, data: Vec<u8> },
        SendPaste { pane_id: PaneId, data: String },
        SendKeyDown { pane_id: PaneId },
        SendMouseEvent { pane_id: PaneId },
        GetPaneRenderChanges { pane_id: PaneId },
        KillPane { pane_id: PaneId },
    }

    #[derive(Clone, Debug)]
    enum MissingMuxSessionErrorOp {
        SetWindowWorkspace {
            window_id: usize,
            workspace: String,
        },
        RenameWorkspace {
            old_workspace: String,
            new_workspace: String,
        },
        SetFocusedPane {
            pane_id: PaneId,
        },
        GetClientList,
        ListPanes,
        ListPanesTabStacks,
        WindowTitleChanged {
            window_id: usize,
            title: String,
        },
        TabTitleChanged {
            tab_id: usize,
            title: String,
        },
        SetPaneZoomed {
            tab_id: usize,
            pane_id: PaneId,
            zoomed: bool,
        },
        GetPaneDirection {
            pane_id: PaneId,
            direction: PaneDirection,
        },
        ActivatePaneDirection {
            pane_id: PaneId,
            direction: PaneDirection,
        },
        GetPaneRenderableDimensions {
            pane_id: PaneId,
        },
        GetLines {
            pane_id: PaneId,
            start: StableRowIndex,
            len: StableRowIndex,
        },
        GetImageCell {
            pane_id: PaneId,
            line_idx: StableRowIndex,
            cell_idx: usize,
            data_hash: [u8; 32],
        },
    }

    impl MissingMuxPaneOp {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::WriteToPane { pane_id, data } => {
                    Pdu::WriteToPane(WriteToPane { pane_id, data })
                }
                Self::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste {
                    pane_id,
                    data,
                    input_serial: InputSerial::empty(),
                }),
                Self::SendKeyDown { pane_id } => Pdu::SendKeyDown(SendKeyDown {
                    pane_id,
                    event: termwiz::input::KeyEvent {
                        key: KeyCode::Char('x'),
                        modifiers: KeyModifiers::NONE,
                    },
                    input_serial: InputSerial::empty(),
                }),
                Self::SendMouseEvent { pane_id } => Pdu::SendMouseEvent(SendMouseEvent {
                    pane_id,
                    event: MouseEvent {
                        kind: wezterm_term::input::MouseEventKind::Move,
                        x: 0,
                        y: 0,
                        x_pixel_offset: 0,
                        y_pixel_offset: 0,
                        button: wezterm_term::input::MouseButton::None,
                        modifiers: KeyModifiers::NONE,
                    },
                }),
                Self::GetPaneRenderChanges { pane_id } => {
                    Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id })
                }
                Self::KillPane { pane_id } => Pdu::KillPane(KillPane { pane_id }),
            }
        }
    }

    impl MissingMuxSessionErrorOp {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::SetWindowWorkspace {
                    window_id,
                    workspace,
                } => Pdu::SetWindowWorkspace(SetWindowWorkspace {
                    window_id,
                    workspace,
                }),
                Self::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                } => Pdu::RenameWorkspace(RenameWorkspace {
                    old_workspace,
                    new_workspace,
                }),
                Self::SetFocusedPane { pane_id } => Pdu::SetFocusedPane(SetFocusedPane { pane_id }),
                Self::GetClientList => Pdu::GetClientList(GetClientList),
                Self::ListPanes => Pdu::ListPanes(ListPanes {}),
                Self::ListPanesTabStacks => Pdu::ListPanesTabStacks(ListPanesTabStacks {}),
                Self::WindowTitleChanged { window_id, title } => {
                    Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title })
                }
                Self::TabTitleChanged { tab_id, title } => {
                    Pdu::TabTitleChanged(TabTitleChanged { tab_id, title })
                }
                Self::SetPaneZoomed {
                    tab_id,
                    pane_id,
                    zoomed,
                } => Pdu::SetPaneZoomed(SetPaneZoomed {
                    containing_tab_id: tab_id,
                    pane_id,
                    zoomed,
                }),
                Self::GetPaneDirection { pane_id, direction } => {
                    Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction })
                }
                Self::ActivatePaneDirection { pane_id, direction } => {
                    Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction })
                }
                Self::GetPaneRenderableDimensions { pane_id } => {
                    Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id })
                }
                Self::GetLines {
                    pane_id,
                    start,
                    len,
                } => {
                    let range =
                        stable_row_range_from_signed_len(start, len).unwrap_or(start..start);
                    Pdu::GetLines(GetLines {
                        pane_id,
                        lines: std::iter::once(range).collect(),
                    })
                }
                Self::GetImageCell {
                    pane_id,
                    line_idx,
                    cell_idx,
                    data_hash,
                } => Pdu::GetImageCell(GetImageCell {
                    pane_id,
                    line_idx,
                    cell_idx,
                    data_hash,
                }),
            }
        }
    }

    fn arb_lifecycle_op() -> impl Strategy<Value = LifecycleOp> {
        prop_oneof![
            (0usize..4, 0usize..8).prop_map(|(slot, client_variant)| LifecycleOp::Claim {
                slot,
                client_variant,
            }),
            (0usize..4).prop_map(|slot| LifecycleOp::Release { slot }),
            (0usize..4).prop_map(|slot| LifecycleOp::Ping { slot }),
        ]
    }

    fn arb_reconnect_op() -> impl Strategy<Value = ReconnectOp> {
        prop_oneof![
            (0usize..4, 0usize..6).prop_map(|(slot, client_variant)| ReconnectOp::Connect {
                slot,
                client_variant,
            }),
            (0usize..4).prop_map(|slot| ReconnectOp::Release { slot }),
            (0usize..4).prop_map(|slot| ReconnectOp::Ping { slot }),
        ]
    }

    fn arb_reconnect_race_step() -> impl Strategy<Value = ReconnectRaceStep> {
        (0usize..6, any::<bool>(), 0usize..4).prop_map(
            |(client_variant, drop_stale_before_reconnect, drop_stale_after_reconnect)| {
                ReconnectRaceStep {
                    client_variant,
                    drop_stale_before_reconnect,
                    drop_stale_after_reconnect,
                }
            },
        )
    }

    fn arb_proxy_identity_op() -> impl Strategy<Value = ProxyIdentityOp> {
        prop_oneof![
            (0usize..6).prop_map(|proxy_variant| ProxyIdentityOp::Proxy { proxy_variant }),
            (0usize..8).prop_map(|client_variant| ProxyIdentityOp::RealClient { client_variant }),
        ]
    }

    fn arb_active_workspace_op() -> impl Strategy<Value = ActiveWorkspaceOp> {
        prop_oneof![
            (0usize..8).prop_map(|client_variant| ActiveWorkspaceOp::SetClient { client_variant }),
            (0usize..10).prop_map(|workspace_variant| ActiveWorkspaceOp::SetWorkspace {
                workspace_variant,
            }),
        ]
    }

    fn arb_missing_mux_pane_op() -> impl Strategy<Value = MissingMuxPaneOp> {
        prop_oneof![
            (0usize..4096, proptest::collection::vec(any::<u8>(), 0..64))
                .prop_map(|(pane_id, data)| MissingMuxPaneOp::WriteToPane { pane_id, data }),
            (0usize..4096, "[a-zA-Z0-9 _./-]{0,64}")
                .prop_map(|(pane_id, data)| { MissingMuxPaneOp::SendPaste { pane_id, data } }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::SendKeyDown { pane_id }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::SendMouseEvent { pane_id }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::GetPaneRenderChanges { pane_id }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::KillPane { pane_id }),
        ]
    }

    fn arb_pane_direction() -> impl Strategy<Value = PaneDirection> {
        prop_oneof![
            Just(PaneDirection::Left),
            Just(PaneDirection::Right),
            Just(PaneDirection::Up),
            Just(PaneDirection::Down),
            Just(PaneDirection::Next),
            Just(PaneDirection::Prev),
        ]
    }

    fn arb_missing_mux_session_error_op() -> impl Strategy<Value = MissingMuxSessionErrorOp> {
        let label = "[a-zA-Z0-9 _./-]{0,48}";
        let line_start = prop_oneof![
            0isize..128,
            Just(StableRowIndex::MAX - 1),
            Just(StableRowIndex::MAX),
        ];
        let line_len = prop_oneof![1isize..8, Just(-1), Just(2)];
        prop_oneof![
            (0usize..128, label).prop_map(|(window_id, workspace)| {
                MissingMuxSessionErrorOp::SetWindowWorkspace {
                    window_id,
                    workspace,
                }
            }),
            (label, label).prop_map(|(old_workspace, new_workspace)| {
                MissingMuxSessionErrorOp::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                }
            }),
            (0usize..4096).prop_map(|pane_id| MissingMuxSessionErrorOp::SetFocusedPane { pane_id }),
            Just(MissingMuxSessionErrorOp::GetClientList),
            Just(MissingMuxSessionErrorOp::ListPanes),
            Just(MissingMuxSessionErrorOp::ListPanesTabStacks),
            (0usize..128, label).prop_map(|(window_id, title)| {
                MissingMuxSessionErrorOp::WindowTitleChanged { window_id, title }
            }),
            (0usize..128, label).prop_map(|(tab_id, title)| {
                MissingMuxSessionErrorOp::TabTitleChanged { tab_id, title }
            }),
            (0usize..128, 0usize..4096, any::<bool>()).prop_map(|(tab_id, pane_id, zoomed)| {
                MissingMuxSessionErrorOp::SetPaneZoomed {
                    tab_id,
                    pane_id,
                    zoomed,
                }
            },),
            (0usize..4096, arb_pane_direction()).prop_map(|(pane_id, direction)| {
                MissingMuxSessionErrorOp::GetPaneDirection { pane_id, direction }
            }),
            (0usize..4096, arb_pane_direction()).prop_map(|(pane_id, direction)| {
                MissingMuxSessionErrorOp::ActivatePaneDirection { pane_id, direction }
            }),
            (0usize..4096).prop_map(|pane_id| {
                MissingMuxSessionErrorOp::GetPaneRenderableDimensions { pane_id }
            }),
            (0usize..4096, line_start.clone(), line_len).prop_map(|(pane_id, start, len)| {
                MissingMuxSessionErrorOp::GetLines {
                    pane_id,
                    start,
                    len,
                }
            }),
            (0usize..4096, line_start, 0usize..128, any::<[u8; 32]>()).prop_map(
                |(pane_id, line_idx, cell_idx, data_hash)| MissingMuxSessionErrorOp::GetImageCell {
                    pane_id,
                    line_idx,
                    cell_idx,
                    data_hash,
                },
            ),
        ]
    }

    fn client_for_slot(slot: usize, client_variant: usize) -> ClientId {
        test_client_id(
            &format!("prop-slot-{slot}-client-{client_variant}"),
            50_000 + (slot as u32 * 100) + client_variant as u32,
        )
    }

    fn reconnect_client(client_variant: usize) -> ClientId {
        test_client_id(
            &format!("reconnect-client-{client_variant}"),
            60_000 + client_variant as u32,
        )
    }

    fn proxy_client(proxy_variant: usize) -> ClientId {
        test_client_id(
            &format!("proxy-client-{proxy_variant}"),
            70_000 + proxy_variant as u32,
        )
    }

    fn proxied_real_client(client_variant: usize, proxy: Option<&ClientId>) -> ClientId {
        let mut client = test_client_id(
            &format!("proxied-real-client-{client_variant}"),
            80_000 + client_variant as u32,
        );
        if let Some(proxy) = proxy {
            client.ssh_auth_sock.clone_from(&proxy.ssh_auth_sock);
            client.hostname = format!("{} (via proxy pid {})", client.hostname, proxy.pid);
        }
        client
    }

    fn workspace_name(workspace_variant: usize) -> String {
        format!("prop-workspace-{workspace_variant}")
    }

    fn mux_client_set(mux: &Mux) -> HashSet<ClientId> {
        mux.iter_clients()
            .into_iter()
            .map(|info| (*info.client_id).clone())
            .collect()
    }

    fn drop_stale_reconnect_owner(
        mux: &Mux,
        stale: &mut Vec<(SessionHandler, usize, ClientId)>,
        latest_owner_by_client: &mut HashMap<ClientId, usize>,
    ) {
        if let Some((handler, owner, client)) = stale.pop() {
            drop(handler);
            if latest_owner_by_client
                .get(&client)
                .is_some_and(|latest_owner| *latest_owner == owner)
            {
                latest_owner_by_client.remove(&client);
            }
        }

        let expected: HashSet<ClientId> = latest_owner_by_client.keys().cloned().collect();
        assert_eq!(
            mux_client_set(mux),
            expected,
            "mux client set diverged after delayed stale reconnect owner drop"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        #[test]
        fn prop_session_handler_claim_release_interleavings_keep_mux_clients_consistent(
            ops in proptest::collection::vec(arb_lifecycle_op(), 1..80)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);

            struct HandlerSlot {
                handler: Option<SessionHandler>,
                captured: Option<Arc<Mutex<Vec<DecodedPdu>>>>,
            }

            let mut slots: Vec<HandlerSlot> = (0..4)
                .map(|_| HandlerSlot {
                    handler: None,
                    captured: None,
                })
                .collect();
            let mut expected_clients: Vec<Option<ClientId>> = vec![None; 4];

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                match *op {
                    LifecycleOp::Claim {
                        slot,
                        client_variant,
                    } => {
                        if slots[slot].handler.is_none() {
                            let (sender, captured) = capturing_sender();
                            slots[slot].handler = Some(SessionHandler::new(sender));
                            slots[slot].captured = Some(captured);
                        }

                        let client = client_for_slot(slot, client_variant);
                        slots[slot]
                            .handler
                            .as_mut()
                            .expect("handler exists after claim setup")
                            .process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::SetClientId(SetClientId {
                                    client_id: client.clone(),
                                    is_proxy: false,
                                }),
                            });
                        expected_clients[slot] = Some(client);

                        let captured = slots[slot]
                            .captured
                            .as_ref()
                            .expect("claim slot has captured sender")
                            .lock()
                            .expect("captured response lock");
                        let response = captured.last().expect("SetClientId should respond");
                        prop_assert_eq!(response.serial, serial);
                        prop_assert!(
                            matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                            "claim response at step {idx} should be UnitResponse, got {:?}",
                            response.pdu
                        );
                    }
                    LifecycleOp::Release { slot } => {
                        slots[slot].handler.take();
                        slots[slot].captured.take();
                        expected_clients[slot] = None;
                    }
                    LifecycleOp::Ping { slot } => {
                        if let Some(handler) = slots[slot].handler.as_mut() {
                            handler.process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::Ping(Ping {}),
                            });
                            let captured = slots[slot]
                                .captured
                                .as_ref()
                                .expect("live slot has captured sender")
                                .lock()
                                .expect("captured response lock");
                            let response = captured.last().expect("Ping should respond");
                            prop_assert_eq!(response.serial, serial);
                            prop_assert!(
                                matches!(response.pdu, Pdu::Pong(Pong {})),
                                "ping response at step {idx} should be Pong, got {:?}",
                                response.pdu
                            );
                        }
                    }
                }

                let expected: HashSet<ClientId> = expected_clients
                    .iter()
                    .filter_map(Clone::clone)
                    .collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "mux client set diverged after step {}: {:?}",
                    idx,
                    op
                );
            }

            drop(slots);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping all session handlers should unregister every claimed client"
            );
        }

        #[test]
        fn prop_session_reconnect_latest_owner_controls_mux_registration(
            ops in proptest::collection::vec(arb_reconnect_op(), 1..96)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);

            struct HandlerSlot {
                handler: Option<SessionHandler>,
                captured: Option<Arc<Mutex<Vec<DecodedPdu>>>>,
            }

            let mut slots: Vec<HandlerSlot> = (0..4)
                .map(|_| HandlerSlot {
                    handler: None,
                    captured: None,
                })
                .collect();
            let mut slot_owner: Vec<Option<(usize, ClientId)>> = vec![None; 4];
            let mut latest_owner_by_client: HashMap<ClientId, usize> = HashMap::new();
            let mut next_owner = 1usize;

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                match *op {
                    ReconnectOp::Connect {
                        slot,
                        client_variant,
                    } => {
                        if slots[slot].handler.is_none() {
                            let (sender, captured) = capturing_sender();
                            slots[slot].handler = Some(SessionHandler::new(sender));
                            slots[slot].captured = Some(captured);
                        }

                        let client = reconnect_client(client_variant);
                        let registration_is_current =
                            slot_owner[slot]
                                .as_ref()
                                .is_some_and(|(prior_owner, prior_client)| {
                                    *prior_client == client
                                        && latest_owner_by_client
                                            .get(prior_client)
                                            .is_some_and(|current_owner| {
                                                current_owner == prior_owner
                                            })
                                });
                        let owner_changes = !registration_is_current;

                        slots[slot]
                            .handler
                            .as_mut()
                            .expect("handler exists after reconnect setup")
                            .process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::SetClientId(SetClientId {
                                    client_id: client.clone(),
                                    is_proxy: false,
                                }),
                            });

                        if owner_changes {
                            if let Some((prior_owner, prior_client)) = slot_owner[slot].take() {
                                if latest_owner_by_client
                                    .get(&prior_client)
                                    .is_some_and(|owner| *owner == prior_owner)
                                {
                                    latest_owner_by_client.remove(&prior_client);
                                }
                            }
                            let owner = next_owner;
                            next_owner += 1;
                            latest_owner_by_client.insert(client.clone(), owner);
                            slot_owner[slot] = Some((owner, client));
                        }

                        let captured = slots[slot]
                            .captured
                            .as_ref()
                            .expect("reconnect slot has captured sender")
                            .lock()
                            .expect("captured response lock");
                        let response = captured.last().expect("SetClientId should respond");
                        prop_assert_eq!(response.serial, serial);
                        prop_assert!(
                            matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                            "reconnect response at step {idx} should be UnitResponse, got {:?}",
                            response.pdu
                        );
                    }
                    ReconnectOp::Release { slot } => {
                        slots[slot].handler.take();
                        slots[slot].captured.take();
                        if let Some((owner, client)) = slot_owner[slot].take() {
                            if latest_owner_by_client
                                .get(&client)
                                .is_some_and(|latest_owner| *latest_owner == owner)
                            {
                                latest_owner_by_client.remove(&client);
                            }
                        }
                    }
                    ReconnectOp::Ping { slot } => {
                        if let Some(handler) = slots[slot].handler.as_mut() {
                            handler.process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::Ping(Ping {}),
                            });
                            let captured = slots[slot]
                                .captured
                                .as_ref()
                                .expect("live slot has captured sender")
                                .lock()
                                .expect("captured response lock");
                            let response = captured.last().expect("Ping should respond");
                            prop_assert_eq!(response.serial, serial);
                            prop_assert!(
                                matches!(response.pdu, Pdu::Pong(Pong {})),
                                "ping response at step {idx} should be Pong, got {:?}",
                                response.pdu
                            );
                        }
                    }
                }

                let expected: HashSet<ClientId> = latest_owner_by_client.keys().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "mux client set diverged after reconnect step {}: {:?}",
                    idx,
                    op
                );
            }

            drop(slots);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping all live reconnect handlers should unregister the current owners"
            );
        }

        #[test]
        fn prop_session_reconnect_delayed_disconnects_preserve_newer_owner(
            steps in proptest::collection::vec(arb_reconnect_race_step(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);

            let mut active: Option<(SessionHandler, usize, ClientId)> = None;
            let mut stale: Vec<(SessionHandler, usize, ClientId)> = Vec::new();
            let mut latest_owner_by_client: HashMap<ClientId, usize> = HashMap::new();

            for (next_owner, (idx, step)) in (1usize..).zip(steps.iter().enumerate()) {
                if step.drop_stale_before_reconnect {
                    drop_stale_reconnect_owner(&mux, &mut stale, &mut latest_owner_by_client);
                }

                if let Some(prior_active) = active.take() {
                    stale.push(prior_active);
                }

                let (sender, captured) = capturing_sender();
                let mut handler = SessionHandler::new(sender);
                let client = reconnect_client(step.client_variant);
                let owner = next_owner;
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: Pdu::SetClientId(SetClientId {
                        client_id: client.clone(),
                        is_proxy: false,
                    }),
                });

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("SetClientId should respond");
                prop_assert_eq!(response.serial, idx as u64 + 1);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "reconnect race step {idx} should return UnitResponse, got {:?}",
                    response.pdu
                );
                drop(captured);

                latest_owner_by_client.insert(client.clone(), owner);
                active = Some((handler, owner, client));

                let expected: HashSet<ClientId> = latest_owner_by_client.keys().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "mux client set diverged after reconnect race step {}: {:?}",
                    idx,
                    step
                );

                for _ in 0..step.drop_stale_after_reconnect {
                    if stale.is_empty() {
                        break;
                    }
                    drop_stale_reconnect_owner(&mux, &mut stale, &mut latest_owner_by_client);
                }
            }

            while !stale.is_empty() {
                drop_stale_reconnect_owner(&mux, &mut stale, &mut latest_owner_by_client);
            }
            if let Some((handler, owner, client)) = active.take() {
                drop(handler);
                if latest_owner_by_client
                    .get(&client)
                    .is_some_and(|latest_owner| *latest_owner == owner)
                {
                    latest_owner_by_client.remove(&client);
                }
            }

            prop_assert!(
                latest_owner_by_client.is_empty(),
                "all reconnect owners should be retired at shutdown"
            );
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping active plus stale reconnect handlers should unregister every client"
            );
        }

        #[test]
        fn prop_session_concurrent_stale_release_preserves_reclaimed_client(
            client_variant in 0usize..6,
            stale_count in 1usize..8,
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let client = reconnect_client(client_variant);

            let mut stale_handlers = Vec::new();
            for idx in 0..stale_count {
                let (sender, captured) = capturing_sender();
                let mut handler = SessionHandler::new(sender);
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: Pdu::SetClientId(SetClientId {
                        client_id: client.clone(),
                        is_proxy: false,
                    }),
                });
                let response = take_response(&captured);
                prop_assert_eq!(response.serial, idx as u64 + 1);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "stale setup claim should return UnitResponse, got {:?}",
                    response.pdu
                );
                stale_handlers.push(handler);
            }

            let (sender, captured) = capturing_sender();
            let mut current_handler = SessionHandler::new(sender);
            current_handler.process_one(DecodedPdu {
                serial: 10_000,
                pdu: Pdu::SetClientId(SetClientId {
                    client_id: client.clone(),
                    is_proxy: false,
                }),
            });
            let response = take_response(&captured);
            prop_assert_eq!(response.serial, 10_000);
            prop_assert!(
                matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                "current reclaim should return UnitResponse, got {:?}",
                response.pdu
            );

            let expected_current: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_current,
                "current handler should own the reclaimed client before stale releases"
            );

            let barrier = Arc::new(Barrier::new(stale_handlers.len() + 1));
            let handles: Vec<_> = stale_handlers
                .into_iter()
                .map(|handler| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        drop(handler);
                    })
                })
                .collect();

            barrier.wait();
            for handle in handles {
                handle
                    .join()
                    .expect("stale handler release thread should not panic");
            }

            let expected_after_release: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_after_release,
                "concurrent stale releases must not clear the reclaimed client"
            );

            drop(current_handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping the current owner should unregister the reclaimed client"
            );
        }

        #[test]
        fn prop_session_concurrent_stale_release_preserves_reused_pane_state(
            client_variant in 0usize..6,
            pane_id in 1usize..4096,
            stale_count in 1usize..8,
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let size = test_tab_size();
            let tab = Arc::new(mux::tab::Tab::new(&size));
            let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
            tab.assign_pane(&pane);
            let (mux, _mux_guard) = install_tab_with_window(&tab, &[]);
            let client = reconnect_client(client_variant);

            let mut stale_handlers = Vec::new();
            for idx in 0..stale_count {
                let (sender, captured) = capturing_sender();
                let mut handler = SessionHandler::new(sender);
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: Pdu::SetClientId(SetClientId {
                        client_id: client.clone(),
                        is_proxy: false,
                    }),
                });
                let response = take_response(&captured);
                prop_assert_eq!(response.serial, idx as u64 + 1);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "stale setup claim should return UnitResponse, got {:?}",
                    response.pdu
                );

                handler.process_one(DecodedPdu {
                    serial: 1_000 + idx as u64,
                    pdu: Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id }),
                });
                tick_until_response(&executor, &captured, 2);
                prop_assert!(
                    handler.per_pane_if_present(pane_id).is_some(),
                    "stale handler should track reused pane id before release"
                );
                stale_handlers.push(handler);
            }

            let (sender, captured) = capturing_sender();
            let mut current_handler = SessionHandler::new(sender);
            current_handler.process_one(DecodedPdu {
                serial: 10_000,
                pdu: Pdu::SetClientId(SetClientId {
                    client_id: client.clone(),
                    is_proxy: false,
                }),
            });
            let response = take_response(&captured);
            prop_assert_eq!(response.serial, 10_000);
            prop_assert!(
                matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                "current reclaim should return UnitResponse, got {:?}",
                response.pdu
            );

            current_handler.process_one(DecodedPdu {
                serial: 10_001,
                pdu: Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id }),
            });
            tick_until_response(&executor, &captured, 2);
            let first_pane_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("current handler should track pane before reuse");

            let first_registration = mux
                .capture_current_pane(pane_id)
                .expect("capture pane registration before reuse");
            prop_assert!(
                first_registration.retire_if_current(),
                "the original pane registration should retire before reuse"
            );
            let replacement: Arc<dyn Pane> =
                Arc::new(FakePane::new_with_id(pane_id, None));
            mux.add_pane(&replacement)
                .expect("same-ID replacement should register");
            current_handler.process_one(DecodedPdu {
                serial: 10_002,
                pdu: Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id }),
            });
            tick_until_response(&executor, &captured, 4);
            let reused_pane_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("current handler should recreate state for reused pane id");
            prop_assert!(
                !Arc::ptr_eq(&first_pane_state, &reused_pane_state),
                "PaneId reuse should allocate fresh per-pane state after removal"
            );
            current_handler.remove_per_pane_if_same(&first_registration);
            current_handler.remove_per_pane(pane_id);
            let retained_reused_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("stale removal signals must preserve live reused pane state");
            prop_assert!(
                Arc::ptr_eq(&reused_pane_state, &retained_reused_state),
                "stale exact and raw removal signals must preserve replacement state"
            );

            let expected_current: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_current,
                "current handler should own the reclaimed client before stale releases"
            );

            let barrier = Arc::new(Barrier::new(stale_handlers.len() + 1));
            let handles: Vec<_> = stale_handlers
                .into_iter()
                .map(|handler| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        drop(handler);
                    })
                })
                .collect();

            barrier.wait();
            for handle in handles {
                handle
                    .join()
                    .expect("stale handler release thread should not panic");
            }

            let expected_after_release: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_after_release,
                "concurrent stale releases must not clear the reclaimed client with reused PaneId"
            );
            let after_release_pane_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("stale releases must not clear current reused pane state");
            prop_assert!(
                Arc::ptr_eq(&reused_pane_state, &after_release_pane_state),
                "stale releases must not replace current reused pane state"
            );

            drop(current_handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping the current owner should unregister the reclaimed client"
            );
        }

        #[test]
        fn prop_session_proxy_identity_sequences_register_only_real_clients(
            ops in proptest::collection::vec(arb_proxy_identity_op(), 1..80)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);
            let mut first_proxy: Option<ClientId> = None;
            let mut expected_registered: Option<ClientId> = None;

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                match *op {
                    ProxyIdentityOp::Proxy { proxy_variant } => {
                        let proxy = proxy_client(proxy_variant);
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetClientId(SetClientId {
                                client_id: proxy.clone(),
                                is_proxy: true,
                            }),
                        });
                        if first_proxy.is_none() {
                            first_proxy = Some(proxy);
                        }
                    }
                    ProxyIdentityOp::RealClient { client_variant } => {
                        let raw_client = proxied_real_client(client_variant, None);
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetClientId(SetClientId {
                                client_id: raw_client,
                                is_proxy: false,
                            }),
                        });
                        expected_registered =
                            Some(proxied_real_client(client_variant, first_proxy.as_ref()));
                    }
                }

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("SetClientId should respond");
                prop_assert_eq!(response.serial, serial);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "proxy identity op at step {idx} should return UnitResponse, got {:?}",
                    response.pdu
                );
                drop(captured);

                prop_assert_eq!(
                    handler.proxy_client_id.as_ref(),
                    first_proxy.as_ref(),
                    "first proxy identity must remain sticky after step {}: {:?}",
                    idx,
                    op
                );
                let expected: HashSet<ClientId> = expected_registered.iter().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "proxy identity sequence registered wrong clients after step {}: {:?}",
                    idx,
                    op
                );
            }

            drop(handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping proxy identity handler should unregister the current real client"
            );
        }

        #[test]
        fn prop_session_active_workspace_sequences_follow_current_client(
            ops in proptest::collection::vec(arb_active_workspace_op(), 1..80)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);
            let default_workspace = mux.active_workspace();
            let mut expected_client: Option<ClientId> = None;
            let mut expected_workspace: Option<String> = None;

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                let mut expect_workspace_error = false;
                match *op {
                    ActiveWorkspaceOp::SetClient { client_variant } => {
                        let client = reconnect_client(client_variant);
                        let same_client = expected_client.as_ref().is_some_and(|prior| *prior == client);
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetClientId(SetClientId {
                                client_id: client.clone(),
                                is_proxy: false,
                            }),
                        });
                        if !same_client {
                            expected_client = Some(client);
                            expected_workspace = None;
                        }
                    }
                    ActiveWorkspaceOp::SetWorkspace { workspace_variant } => {
                        let workspace = workspace_name(workspace_variant);
                        expect_workspace_error = expected_client.is_none();
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetActiveWorkspace(SetActiveWorkspace { workspace: workspace.clone() }),
                        });
                        if !expect_workspace_error {
                            expected_workspace = Some(workspace);
                        }
                    }
                }

                tick_until_response(&executor, &captured, idx + 1);
                let captured_guard = captured.lock().expect("captured response lock");
                let response = captured_guard
                    .last()
                    .expect("workspace sequence op should respond");
                prop_assert_eq!(response.serial, serial);
                if expect_workspace_error {
                    match &response.pdu {
                        Pdu::ErrorResponse(ErrorResponse { reason }) => {
                            prop_assert!(
                                reason.contains("set active workspace before SetClientId"),
                                "pre-client workspace update returned wrong error after step {}: {:?}: {reason}",
                                idx,
                                op
                            );
                        }
                        other => {
                            prop_assert!(
                                false,
                                "pre-client workspace update should return ErrorResponse after step {}: {:?}, got {other:?}",
                                idx,
                                op
                            );
                        }
                    }
                } else {
                    prop_assert!(
                        matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                        "workspace sequence op at step {} should return UnitResponse, got {:?}",
                        idx,
                        response.pdu
                    );
                }
                drop(captured_guard);

                let expected_clients: HashSet<ClientId> = expected_client.iter().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected_clients,
                    "workspace sequence registered wrong clients after step {}: {:?}",
                    idx,
                    op
                );
                if let Some(client) = expected_client.as_ref() {
                    let registered_client = handler
                        .client_id
                        .as_ref()
                        .expect("expected client must have an exact registration");
                    prop_assert_eq!(
                        registered_client.as_ref(),
                        client,
                        "handler retained the wrong exact client after step {}: {:?}",
                        idx,
                        op,
                    );
                    prop_assert!(
                        mux.client_registration_is_current(registered_client),
                        "handler retained a stale client registration after step {}: {:?}",
                        idx,
                        op,
                    );
                    let expected_workspace = expected_workspace
                        .as_deref()
                        .unwrap_or(default_workspace.as_str());
                    prop_assert_eq!(
                        mux.active_workspace_for_client(registered_client),
                        expected_workspace,
                        "active workspace drifted after step {}: {:?}",
                        idx,
                        op
                    );
                } else {
                    prop_assert!(
                        handler.client_id.is_none(),
                        "handler retained a client before SetClientId after step {}: {:?}",
                        idx,
                        op,
                    );
                }
            }

            drop(handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping active workspace handler should unregister the current client"
            );
        }

        #[test]
        fn prop_private_session_missing_pane_paths_do_not_retain_per_pane_state(
            ops in proptest::collection::vec(arb_missing_mux_pane_op(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let _mux_guard = ScopedMux::shutdown_current();
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);

            for (idx, op) in ops.into_iter().enumerate() {
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: op.clone().into_pdu(),
                });
                tick_until_response(&executor, &captured, idx + 1);

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("cancel path should respond");
                prop_assert_eq!(response.serial, idx as u64 + 1);
                match (&op, &response.pdu) {
                    (
                        MissingMuxPaneOp::GetPaneRenderChanges { pane_id },
                        Pdu::LivenessResponse(LivenessResponse {
                            pane_id: response_pane_id,
                            is_alive: false,
                        }),
                    ) => {
                        prop_assert_eq!(response_pane_id, pane_id);
                    }
                    (_, Pdu::ErrorResponse(ErrorResponse { reason })) => {
                        prop_assert!(
                            reason.contains("no such pane"),
                            "private-session missing-pane path should report absence for {:?}, got {reason}",
                            op
                        );
                    }
                    other => {
                        prop_assert!(
                            false,
                            "private-session missing-pane path for {:?} returned {other:?}",
                            op
                        );
                    }
                }
                drop(captured);

                prop_assert!(
                    handler.per_pane.is_empty(),
                    "private-session missing-pane path retained per-pane state after step {}: {:?}",
                    idx,
                    op
                );
            }
        }

        #[test]
        fn prop_present_mux_missing_pane_paths_do_not_retain_per_pane_state(
            ops in proptest::collection::vec(arb_missing_mux_pane_op(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);

            for (idx, op) in ops.into_iter().enumerate() {
                let serial = idx as u64 + 1;
                handler.process_one(DecodedPdu {
                    serial,
                    pdu: op.clone().into_pdu(),
                });
                tick_until_response(&executor, &captured, idx + 1);

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("missing-pane path should respond");
                prop_assert_eq!(response.serial, serial);
                match (&op, &response.pdu) {
                    (
                        MissingMuxPaneOp::GetPaneRenderChanges { pane_id },
                        Pdu::LivenessResponse(LivenessResponse {
                            pane_id: response_pane_id,
                            is_alive: false,
                        }),
                    ) => {
                        prop_assert_eq!(response_pane_id, pane_id);
                    }
                    (_, Pdu::ErrorResponse(ErrorResponse { reason })) => {
                        prop_assert!(
                            reason.contains("no such pane"),
                            "present-mux missing-pane path should report missing pane for {:?}, got {reason}",
                            op
                        );
                    }
                    other => {
                        prop_assert!(
                            false,
                            "present-mux missing-pane path for {:?} returned {other:?}",
                            op
                        );
                    }
                }
                drop(captured);

                prop_assert!(
                    handler.per_pane.is_empty(),
                    "present-mux missing-pane path retained per-pane state after step {}: {:?}",
                    idx,
                    op
                );
            }
        }

        #[test]
        fn prop_private_session_empty_topology_paths_preserve_serial_and_state(
            ops in proptest::collection::vec(arb_missing_mux_session_error_op(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let _mux_guard = ScopedMux::shutdown_current();
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);

            for (idx, op) in ops.into_iter().enumerate() {
                let serial = idx as u64 + 1;
                handler.process_one(DecodedPdu {
                    serial,
                    pdu: op.clone().into_pdu(),
                });
                tick_until_response(&executor, &captured, idx + 1);

                let captured = captured.lock().expect("captured response lock");
                let response = captured
                    .last()
                    .expect("empty private-session path should respond");
                prop_assert_eq!(response.serial, serial);
                match (&op, &response.pdu) {
                    (
                        MissingMuxSessionErrorOp::RenameWorkspace { .. },
                        Pdu::UnitResponse(UnitResponse {}),
                    ) => {}
                    (
                        MissingMuxSessionErrorOp::GetClientList,
                        Pdu::GetClientListResponse(GetClientListResponse { clients }),
                    ) => {
                        prop_assert!(clients.is_empty());
                    }
                    (
                        MissingMuxSessionErrorOp::ListPanes,
                        Pdu::ListPanesResponse(ListPanesResponse {
                            tabs,
                            tab_titles,
                            window_titles,
                            floating_panes,
                        }),
                    ) => {
                        prop_assert!(tabs.is_empty());
                        prop_assert!(tab_titles.is_empty());
                        prop_assert!(window_titles.is_empty());
                        prop_assert!(floating_panes.is_empty());
                    }
                    (
                        MissingMuxSessionErrorOp::ListPanesTabStacks,
                        Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse {
                            tab_stack_entries,
                        }),
                    ) => {
                        prop_assert!(tab_stack_entries.is_empty());
                    }
                    (request, Pdu::ErrorResponse(ErrorResponse { reason })) => {
                        let expected = match request {
                            MissingMuxSessionErrorOp::SetWindowWorkspace { .. } => "window",
                            MissingMuxSessionErrorOp::WindowTitleChanged { .. } => {
                                "no such window"
                            }
                            MissingMuxSessionErrorOp::TabTitleChanged { .. } => "no such tab",
                            MissingMuxSessionErrorOp::SetFocusedPane { .. }
                            | MissingMuxSessionErrorOp::SetPaneZoomed { .. }
                            | MissingMuxSessionErrorOp::GetPaneDirection { .. }
                            | MissingMuxSessionErrorOp::ActivatePaneDirection { .. }
                            | MissingMuxSessionErrorOp::GetPaneRenderableDimensions { .. }
                            | MissingMuxSessionErrorOp::GetLines { .. }
                            | MissingMuxSessionErrorOp::GetImageCell { .. } => "no such pane",
                            MissingMuxSessionErrorOp::RenameWorkspace { .. }
                            | MissingMuxSessionErrorOp::GetClientList
                            | MissingMuxSessionErrorOp::ListPanes
                            | MissingMuxSessionErrorOp::ListPanesTabStacks => {
                                prop_assert!(
                                    false,
                                    "successful empty-session request returned ErrorResponse: {:?}: {reason}",
                                    request
                                );
                                ""
                            }
                        };
                        prop_assert!(
                            reason.contains(expected),
                            "empty private-session path returned wrong error for {:?}: {reason}",
                            request
                        );
                    }
                    other => {
                        prop_assert!(
                            false,
                            "empty private-session path for {:?} returned {other:?}",
                            op
                        );
                    }
                }
                drop(captured);

                prop_assert!(
                    handler.client_id.is_none(),
                    "empty private-session path retained client identity after step {}: {:?}",
                    idx,
                    op
                );
                prop_assert!(
                    handler.per_pane.is_empty(),
                    "empty private-session path retained per-pane state after step {}: {:?}",
                    idx,
                    op
                );
            }
        }
    }

    #[test]
    fn set_client_id_proxy_stores_proxy_identity() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let proxy_id = test_client_id("proxy-host", 999);
        handler.process_one(DecodedPdu {
            serial: 500,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: proxy_id.clone(),
                is_proxy: true,
            }),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 500);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));

        // Proxy identity should be stored
        assert!(handler.proxy_client_id.is_some());
        let stored = handler.proxy_client_id.as_ref().unwrap();
        assert_eq!(stored.hostname, "proxy-host.local");
        assert_eq!(stored.pid, 999);
    }

    #[test]
    fn set_client_id_proxy_only_stores_first_proxy() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let first_proxy = test_client_id("first-proxy", 100);
        handler.process_one(DecodedPdu {
            serial: 1,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: first_proxy,
                is_proxy: true,
            }),
        });

        let second_proxy = test_client_id("second-proxy", 200);
        handler.process_one(DecodedPdu {
            serial: 2,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: second_proxy,
                is_proxy: true,
            }),
        });

        // First proxy should be kept, second ignored
        let stored = handler.proxy_client_id.as_ref().unwrap();
        assert_eq!(stored.hostname, "first-proxy.local");
        assert_eq!(stored.pid, 100);
    }

    #[test]
    fn set_client_id_replaces_prior_registered_client_without_leaking_stale_entries() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let first = test_client_id("review-first", 41_001);
        let second = test_client_id("review-second", 41_002);
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 10,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: first.clone(),
                is_proxy: false,
            }),
        });

        let clients_after_first_set = mux.iter_clients();
        assert!(
            clients_after_first_set
                .iter()
                .any(|info| *info.client_id == first)
        );

        handler.process_one(DecodedPdu {
            serial: 11,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: second.clone(),
                is_proxy: false,
            }),
        });

        let clients = mux.iter_clients();
        assert!(clients.iter().any(|info| *info.client_id == second));
        assert!(!clients.iter().any(|info| *info.client_id == first));

        drop(handler);

        let clients_after_drop = mux.iter_clients();
        assert!(
            !clients_after_drop
                .iter()
                .any(|info| *info.client_id == second)
        );
    }

    #[test]
    fn same_value_client_reclaims_after_equal_replacement_departed() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client_five = reconnect_client(5);
        let client_zero = reconnect_client(0);

        let (slot_two_sender, slot_two_responses) = capturing_sender();
        let mut slot_two = SessionHandler::new(slot_two_sender);
        slot_two.process_one(DecodedPdu {
            serial: 1,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_five.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_two_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));
        let slot_two_first = Arc::clone(
            slot_two
                .client_id
                .as_ref()
                .expect("slot two must retain its first exact registration"),
        );
        assert!(mux.client_registration_is_current(&slot_two_first));

        let (slot_three_sender, slot_three_responses) = capturing_sender();
        let mut slot_three = SessionHandler::new(slot_three_sender);
        slot_three.process_one(DecodedPdu {
            serial: 2,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_five.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_three_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));
        let slot_three_client_five = Arc::clone(
            slot_three
                .client_id
                .as_ref()
                .expect("slot three must retain the equal replacement"),
        );
        assert!(mux.client_registration_is_current(&slot_three_client_five));
        assert!(!mux.client_registration_is_current(&slot_two_first));

        slot_three.process_one(DecodedPdu {
            serial: 3,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_zero.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_three_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));
        assert_eq!(mux_client_set(&mux), HashSet::from([client_zero.clone()]));

        slot_two.process_one(DecodedPdu {
            serial: 4,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_five.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_two_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));

        let slot_two_reclaimed = Arc::clone(
            slot_two
                .client_id
                .as_ref()
                .expect("slot two must retain its reclaimed registration"),
        );
        assert!(
            !Arc::ptr_eq(&slot_two_first, &slot_two_reclaimed),
            "reclaim must retain the newly received exact token",
        );
        assert!(mux.client_registration_is_current(&slot_two_reclaimed));
        assert!(!mux.client_registration_is_current(&slot_two_first));
        assert_eq!(
            mux_client_set(&mux),
            HashSet::from([client_zero.clone(), client_five.clone()]),
        );

        drop(slot_three);
        assert_eq!(mux_client_set(&mux), HashSet::from([client_five]));
        drop(slot_two);
        assert_eq!(mux.iter_clients().len(), 0);
    }

    #[test]
    fn handler_drop_unregisters_from_owning_mux_after_global_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let client = test_client_id("exact-mux-owner", 41_006);
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&originating_mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 14,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        let replacement_client = Arc::new(client.clone());
        replacement_mux.register_client(Arc::clone(&replacement_client));
        Mux::set_mux(&replacement_mux);

        drop(handler);

        assert!(
            originating_mux.iter_clients().is_empty(),
            "handler cleanup must unregister from the mux that owns its registration",
        );
        assert!(
            replacement_mux.client_registration_is_current(&replacement_client),
            "cleanup from an old mux must not remove an equal-valued replacement registration",
        );
    }

    #[test]
    fn repeated_client_identity_stays_bound_to_connection_mux_after_global_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let client = test_client_id("mux-rebind", 41_007);
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&originating_mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        for serial in [15, 16] {
            if serial == 16 {
                Mux::set_mux(&replacement_mux);
            }
            handler.process_one(DecodedPdu {
                serial,
                pdu: Pdu::SetClientId(SetClientId {
                    client_id: client.clone(),
                    is_proxy: false,
                }),
            });
        }

        assert!(
            originating_mux
                .iter_clients()
                .iter()
                .any(|info| *info.client_id == client),
            "repeated identity must remain registered on the connection's mux",
        );
        assert!(
            replacement_mux.iter_clients().is_empty(),
            "changing the process-global mux must not retarget a live connection",
        );
        drop(handler);
        assert!(
            originating_mux.iter_clients().is_empty(),
            "drop must clean up the connection-owned registration",
        );
    }

    #[test]
    fn dropping_handler_after_global_mux_shutdown_cleans_its_connection_mux() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client = test_client_id("shutdown-safe", 41_003);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 12,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client,
                is_proxy: false,
            }),
        });

        Mux::shutdown();
        drop(handler);
        assert!(
            mux.iter_clients().is_empty(),
            "global shutdown must not prevent exact connection-owner cleanup"
        );
    }

    #[test]
    fn set_client_id_without_global_mux_uses_handler_owned_mux() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _mux_guard = ScopedMux::shutdown_current();
        let client = test_client_id("missing-mux", 41_005);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let owned_mux = Arc::clone(handler.owner.mux());

        handler.process_one(DecodedPdu {
            serial: 13,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 13);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert!(
            owned_mux
                .iter_clients()
                .iter()
                .any(|info| *info.client_id == client),
            "a handler created without a global mux must retain its private owner"
        );
        assert!(Mux::try_get().is_none());
        drop(handler);
        assert!(
            owned_mux.iter_clients().is_empty(),
            "dropping the handler must unregister from its private mux"
        );
    }

    #[test]
    fn set_active_workspace_updates_registered_client_workspace() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client = test_client_id("workspace-owner", 41_004);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 21,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        let _ = take_response(&captured);

        handler.process_one(DecodedPdu {
            serial: 22,
            pdu: Pdu::SetActiveWorkspace(SetActiveWorkspace {
                workspace: "remote-dev".to_string(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 22);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));

        let registered_client = handler
            .client_id
            .as_ref()
            .expect("SetClientId must retain the exact registration");
        assert_eq!(registered_client.as_ref(), &client);
        assert!(mux.client_registration_is_current(registered_client));
        assert_eq!(
            mux.active_workspace_for_client(registered_client),
            "remote-dev",
        );
    }

    #[test]
    fn get_codec_version_returns_version_info() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 600,
            pdu: Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 600);
        match resp.pdu {
            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                codec_vers,
                version_string,
                executable_path,
                ..
            }) => {
                assert_eq!(codec_vers, CODEC_VERSION);
                assert!(
                    !version_string.is_empty(),
                    "version string should not be empty"
                );
                assert!(
                    !executable_path.to_string_lossy().is_empty(),
                    "executable path should be non-empty"
                );
            }
            other => panic!("expected GetCodecVersionResponse, got {other:?}"),
        }
    }

    #[test]
    fn catch_helper_forwards_ok() {
        let (sender, captured) = capturing_sender();
        let serial = 700;
        let send_response = {
            let sender = sender.clone();
            move |result: anyhow::Result<Pdu>| {
                let pdu = match result {
                    Ok(pdu) => pdu,
                    Err(err) => Pdu::ErrorResponse(ErrorResponse {
                        reason: format!("{err:#}"),
                    }),
                };
                sender.send_control(DecodedPdu { pdu, serial }).ok();
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        catch(|| Ok(Pdu::UnitResponse(UnitResponse {})), send_response);

        let resp = take_response(&captured);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
    }

    #[test]
    fn catch_helper_forwards_error() {
        let (sender, captured) = capturing_sender();
        let serial = 701;
        let send_response = {
            let sender = sender.clone();
            move |result: anyhow::Result<Pdu>| {
                let pdu = match result {
                    Ok(pdu) => pdu,
                    Err(err) => Pdu::ErrorResponse(ErrorResponse {
                        reason: format!("{err:#}"),
                    }),
                };
                sender.send_control(DecodedPdu { pdu, serial }).ok();
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        catch(|| anyhow::bail!("something went wrong"), send_response);

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("something went wrong"),
                    "error should propagate message, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn multiple_per_pane_entries_are_independent() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let pp1 = handler.per_pane(10);
        let pp2 = handler.per_pane(20);

        // Modify one, verify the other is unaffected
        pp1.lock().unwrap().baseline.seqno = 42;
        assert_eq!(pp2.lock().unwrap().baseline.seqno, 0);
    }

    #[test]
    fn maybe_push_pane_changes_drops_per_pane_lock_before_sending() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let delivery_classes = Arc::new(Mutex::new(Vec::new()));

        let sender = PduSender::new({
            let per_pane = Arc::clone(&per_pane);
            let delivery_classes = Arc::clone(&delivery_classes);
            move |_, class| {
                assert!(
                    per_pane.try_lock().is_ok(),
                    "PduSender callback must not run while per-pane state is locked"
                );
                delivery_classes.lock().unwrap().push(class);
                Ok(())
            }
        });

        registration
            .try_with_current(|current| maybe_push_pane_changes(&current, sender, per_pane))
            .expect("test pane registration remains current")
            .expect("pane changes should send");

        let delivery_classes = delivery_classes.lock().unwrap();
        assert!(
            delivery_classes.len() >= 2,
            "initial render and palette PDUs should both be sent"
        );
        assert!(
            delivery_classes
                .iter()
                .all(|class| *class == PduDeliveryClass::Bulk),
            "unsolicited render, palette, and alert pushes must use bulk admission"
        );
    }

    #[test]
    fn legacy_render_preparation_drops_per_pane_lock_before_pane_callbacks() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let callback_count = Arc::new(AtomicUsize::new(0));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(7_701, {
            let per_pane = Arc::clone(&per_pane);
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                assert!(
                    per_pane.try_lock().is_ok(),
                    "legacy pane callbacks must not run under the per-pane state lock"
                );
                callback_count.fetch_add(1, Ordering::Relaxed);
            })
        }));
        let (_mux, registration) = register_test_pane(&pane);
        let (sender, _captured) = capturing_sender();

        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("lock-free legacy render preparation should send");

        assert_eq!(callback_count.load(Ordering::Relaxed), 1);
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.seqno, 11);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
    }

    #[test]
    fn legacy_render_rejects_a_source_that_changes_during_snapshot() {
        let mut fake = FakePane::new(None);
        fake.seqno_on_dimensions = Some(12);
        let pane: Arc<dyn Pane> = Arc::new(fake);
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        let result = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a mixed-source legacy snapshot must not be published"),
        };
        assert!(error.to_string().contains("source changed"));
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Idle
        ));
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_render_terminal_sequence_retires_both_protocols() {
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().seqno = SequenceNo::MAX;
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        let result = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("terminal legacy sequence identity must fail closed"),
        };
        assert!(error.to_string().contains("terminal sequence"));
        let state = per_pane.lock().unwrap();
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_render_preparation_rejects_a_superseded_baseline() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(7_702, {
            let per_pane = Arc::clone(&per_pane);
            Arc::new(move || {
                let mut state = per_pane
                    .try_lock()
                    .expect("pane callback must be able to supersede the sampled baseline");
                state.baseline.title = "newer-baseline".to_string();
            })
        }));
        let (_mux, registration) = register_test_pane(&pane);
        let send_count = Arc::new(AtomicUsize::new(0));
        let sender = PduSender::new({
            let send_count = Arc::clone(&send_count);
            move |_, _| {
                send_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        });

        let error = registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect_err("a superseded baseline must reject the stale prepared snapshot");

        assert!(error.to_string().contains("baseline changed"));
        assert_eq!(send_count.load(Ordering::Relaxed), 0);
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.title, "newer-baseline");
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_changed_render_identity_exhaustion_retires_both_protocols() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().baseline_revision = u64::MAX;

        let result = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("an exhausted legacy change identity must fail closed"),
        };
        assert!(error.to_string().contains("attempt identity exhausted"));

        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert_eq!(state.baseline_revision, u64::MAX);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_no_change_identity_exhaustion_retires_both_protocols() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, first) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial legacy render preparation succeeds")
            .expect("initial pane state produces a render");
        first
            .acknowledge()
            .expect("initial legacy baseline becomes authoritative");
        pane.clear_changed_lines();
        {
            let mut state = per_pane.lock().unwrap();
            state.baseline.seqno = 10;
            state.baseline_revision = u64::MAX;
            state.transactional_dirty = false;
        }

        let result = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("an exhausted legacy no-change identity must fail closed"),
        };
        assert!(error.to_string().contains("attempt identity exhausted"));

        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.seqno, 10);
        assert_eq!(state.baseline_revision, u64::MAX);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn post_input_pane_callback_panic_does_not_poison_render_state_or_escape() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(
            7_703,
            Arc::new(|| panic!("synthetic pane snapshot panic")),
        ));
        let (_mux, registration) = register_test_pane(&pane);
        let (sender, _captured) = capturing_sender();

        registration
            .try_with_current(|current| {
                push_pane_changes_after_committed_input(
                    &current,
                    sender,
                    Arc::clone(&per_pane),
                    "test write",
                );
            })
            .expect("test pane registration remains current");

        let state = per_pane
            .lock()
            .expect("lock-free pane callback panic must not poison render state");
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert!(state.transactional_dirty);
    }

    #[test]
    fn failed_legacy_render_enqueue_restores_baseline_and_redirties() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let sender = PduSender::new(|_, _| Err(anyhow!("synthetic enqueue failure")));

        let error = registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect_err("the synthetic render enqueue must fail");
        assert!(error.to_string().contains("synthetic enqueue failure"));

        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.baseline,
            PaneRenderBaseline::default(),
            "a render that never entered the queue cannot become authoritative"
        );
        assert!(
            state.transactional_dirty,
            "failed enqueue must retain an explicit retry obligation"
        );
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
    }

    #[test]
    fn reentrant_legacy_prepare_cannot_inherit_an_undelivered_baseline() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let nested_rejected = Arc::new(AtomicBool::new(false));
        let sender = PduSender::new({
            let pane = Arc::clone(&pane);
            let registration = registration.clone();
            let per_pane = Arc::clone(&per_pane);
            let nested_rejected = Arc::clone(&nested_rejected);
            move |_, _| {
                {
                    let mut pane_state = pane.state.lock().unwrap();
                    pane_state.title = "newer-render".to_string();
                    pane_state.seqno = 12;
                }
                let nested_result = registration
                    .try_with_current(|current| {
                        prepare_legacy_render_enqueue(&current, &per_pane, None)
                    })
                    .expect("nested registration remains exact");
                let nested_error = match nested_result {
                    Err(error) => error,
                    Ok(_) => {
                        panic!("an undelivered baseline must retain exclusive enqueue authority")
                    }
                };
                assert!(nested_error.to_string().contains("already active"));
                nested_rejected.store(true, Ordering::Relaxed);
                Err(anyhow!(
                    "outer enqueue failed after nested render was rejected"
                ))
            }
        });

        let error = registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect_err("the outer synthetic enqueue must fail");
        assert!(error.to_string().contains("outer enqueue failed"));
        assert!(nested_rejected.load(Ordering::Relaxed));

        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.baseline, PaneRenderBaseline::default());
            assert_eq!(state.baseline_revision, 1);
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
            assert!(state.transactional_dirty);
        }

        let (retry_sender, captured) = capturing_sender();
        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, retry_sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("the exact rollback must release authority for a complete retry");
        let responses = captured.lock().unwrap();
        assert!(responses.iter().any(|decoded| matches!(
            &decoded.pdu,
            Pdu::GetPaneRenderChangesResponse(response)
                if response.title == "newer-render" && response.seqno == 12
        )));
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.title, "newer-render");
        assert_eq!(state.baseline.seqno, 12);
        assert_eq!(state.baseline_revision, 2);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
    }

    #[test]
    fn legacy_render_rollback_preserves_newer_palette_metadata() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, rollback) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        {
            let mut state = per_pane.lock().unwrap();
            state.baseline.config_generation = 91;
            state.baseline.sent_initial_palette = true;
        }

        rollback
            .rollback()
            .expect("exact render revision rollback should succeed");

        let state = per_pane.lock().unwrap();
        let expected = PaneRenderBaseline {
            config_generation: 91,
            sent_initial_palette: true,
            ..PaneRenderBaseline::default()
        };
        assert_eq!(state.baseline, expected);
        assert!(state.transactional_dirty);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
    }

    #[test]
    fn abandoned_legacy_enqueue_guard_rolls_back_and_releases_authority() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        assert!(matches!(
            per_pane.lock().unwrap().legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::InFlight { .. }
        ));

        // Model a connection/task being retired after preparation but before
        // queue admission. The armed guard must restore the baseline and make
        // a later exact-registration retry possible.
        drop(guard);

        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.baseline, PaneRenderBaseline::default());
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
            assert!(state.transactional_dirty);
        }
        let (_, retry_guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("abandoned authority must permit an exact retry")
            .expect("rolled-back surface must be prepared again");
        retry_guard
            .rollback()
            .expect("retry guard cleanup restores idle authority");
    }

    #[test]
    fn legacy_preparation_retires_and_clears_a_poisoned_state_lock() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        poison_per_pane_lock(&per_pane);

        let result = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("poisoned legacy state must fail closed"),
        };
        assert!(error.to_string().contains("state lock poisoned"));

        let state = per_pane
            .lock()
            .expect("legacy terminal recovery must clear the mutex poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_enqueue_recovery_mismatch_retires_under_the_recovery_guard() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        per_pane.lock().unwrap().baseline_revision = 2;

        let error = guard
            .recover()
            .expect_err("mismatched baseline ownership must fail closed");
        assert!(error.to_string().contains("ownership changed"));
        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Closed
            ));
            assert_eq!(state.baseline.seqno, 11);
            assert!(state.transactional_dirty);
        }
        guard.retire();
    }

    #[test]
    fn legacy_enqueue_ack_mismatch_retires_under_the_ack_guard() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        per_pane.lock().unwrap().baseline_revision = 2;

        let error =
            acknowledge_legacy_render_enqueue(&per_pane, pane.pane_id(), guard.installed_revision)
                .expect_err("mismatched enqueue acknowledgement must fail closed");
        assert!(error.to_string().contains("ownership changed"));
        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Closed
            ));
            assert_eq!(state.baseline.seqno, 11);
            assert!(state.transactional_dirty);
        }
        guard.retire();
    }

    #[test]
    fn legacy_enqueue_ack_retires_poison_under_the_recovered_guard() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        poison_per_pane_lock(&per_pane);

        let error = guard
            .acknowledge()
            .expect_err("poisoned acknowledgement must fail closed");
        assert!(error.to_string().contains("state lock poisoned"));
        let state = per_pane
            .lock()
            .expect("acknowledgement poison repair must clear the mutex poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_enqueue_rollback_retires_poison_under_the_recovered_guard() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        poison_per_pane_lock(&per_pane);

        let error = guard
            .rollback()
            .expect_err("poisoned rollback must fail closed");
        assert!(error.to_string().contains("state lock poisoned"));
        let state = per_pane
            .lock()
            .expect("rollback poison repair must clear the mutex poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn panicked_legacy_enqueue_ack_retires_and_clears_lock_poison() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        per_pane.lock().unwrap().panic_next_legacy_enqueue_ack = true;

        let error = guard
            .acknowledge()
            .expect_err("a recovered acknowledgement panic must fail closed");
        assert!(error.to_string().contains("acknowledgement panicked"));
        let state = per_pane
            .lock()
            .expect("terminal retirement must clear the recovered lock poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline.seqno, 11);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn panicked_legacy_enqueue_rollback_retires_and_clears_lock_poison() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        per_pane.lock().unwrap().panic_next_legacy_enqueue_recovery = true;

        let error = guard
            .rollback()
            .expect_err("a recovered rollback panic must fail closed");
        assert!(error.to_string().contains("recovery panicked"));
        let state = per_pane
            .lock()
            .expect("terminal retirement must clear the recovered lock poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline.seqno, 11);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn panicked_abandoned_legacy_enqueue_recovery_cannot_linger_in_flight() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (_, guard) = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current")
            .expect("initial render preparation succeeds")
            .expect("initial pane state produces a render");
        per_pane.lock().unwrap().panic_next_legacy_enqueue_recovery = true;

        drop(guard);

        let state = per_pane
            .lock()
            .expect("drop-time terminal retirement must clear lock poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline.seqno, 11);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn panicked_legacy_render_enqueue_retires_indeterminate_delivery() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let sender = PduSender::new(|_, _| -> anyhow::Result<()> {
            panic!("synthetic render enqueue panic")
        });

        registration
            .try_with_current(|current| {
                push_pane_changes_after_committed_input(
                    &current,
                    sender,
                    Arc::clone(&per_pane),
                    "test write",
                );
            })
            .expect("test pane registration remains current");

        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.baseline.seqno, 11,
            "the speculative baseline is retained only inside terminal state"
        );
        assert!(
            state.transactional_dirty,
            "an enqueue panic must preserve explicit terminal dirtiness"
        );
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
    }

    #[test]
    fn committed_input_dirty_mark_retires_and_clears_a_poisoned_state_lock() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        poison_per_pane_lock(&per_pane);

        mark_post_input_render_dirty(&per_pane, 17, "test committed input");

        let state = per_pane
            .lock()
            .expect("post-input terminal recovery must clear the mutex poison");
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn failed_notification_enqueue_retains_unsent_suffix_and_redirties() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let sender = PduSender::new({
            let enqueue_count = Arc::clone(&enqueue_count);
            move |_, _| {
                if enqueue_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(())
                } else {
                    Err(anyhow!("synthetic notification enqueue failure"))
                }
            }
        });

        let error = registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect_err("the synthetic palette enqueue must fail");
        assert!(
            error
                .to_string()
                .contains("synthetic notification enqueue failure")
        );

        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.baseline.seqno, 11,
            "the successfully enqueued render baseline remains authoritative"
        );
        assert_eq!(state.notifications, vec![Alert::PaletteChanged]);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn panicked_notification_enqueue_retires_indeterminate_exact_event() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let sender = PduSender::new({
            let enqueue_count = Arc::clone(&enqueue_count);
            move |_, _| {
                if enqueue_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(())
                } else {
                    panic!("synthetic notification enqueue panic")
                }
            }
        });

        registration
            .try_with_current(|current| {
                push_pane_changes_after_committed_input(
                    &current,
                    sender,
                    Arc::clone(&per_pane),
                    "test paste",
                );
            })
            .expect("test pane registration remains current");

        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.seqno, 11);
        assert_eq!(state.notifications, vec![Alert::PaletteChanged]);
        assert!(state.transactional_dirty);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
    }

    #[test]
    fn committed_notification_with_failed_local_settlement_retires_authority() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let sender = PduSender::new({
            let enqueue_count = Arc::clone(&enqueue_count);
            let per_pane = Arc::clone(&per_pane);
            move |_, _| {
                if enqueue_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Ok(());
                }
                // Model queue admission followed by contradictory local
                // settlement authority. The accepted notification must never
                // be retried as though delivery had definitely failed.
                per_pane
                    .try_lock()
                    .expect("notification callback runs outside the pane-state lock")
                    .notifications
                    .clear_protection();
                Ok(())
            }
        });

        let error = registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect_err("committed notification without exact settlement must fail closed");
        assert!(error.to_string().contains("authority retired"));

        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.seqno, 11);
        assert_eq!(state.notifications, vec![Alert::PaletteChanged]);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
    }

    #[test]
    fn notification_ack_mismatch_retires_under_the_settlement_guard() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let batch = {
            let mut state = per_pane.lock().unwrap();
            state.push_notification(Alert::Bell).unwrap();
            let batch = state.notifications.protected_batch_up_to(1).unwrap();
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
            state.notifications.clear_protection();
            batch
        };
        let mut guard = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);

        assert!(guard.acknowledge_current().is_err());
        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Closed
            ));
            assert!(state.transactional_dirty);
        }
        guard.retire();
    }

    #[test]
    fn notification_recovery_mismatch_retires_under_the_recovery_guard() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let batch = {
            let mut state = per_pane.lock().unwrap();
            state.push_notification(Alert::Bell).unwrap();
            let batch = state.notifications.protected_batch_up_to(1).unwrap();
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::Idle;
            batch
        };
        let guard = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);

        assert!(guard.recover().is_err());
        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Closed
            ));
            assert_eq!(state.notifications.protected_prefix_len, 0);
            assert!(state.transactional_dirty);
        }
        guard.retire();
    }

    #[test]
    fn notification_completion_mismatch_retires_under_the_settlement_guard() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let batch = {
            let mut state = per_pane.lock().unwrap();
            state.push_notification(Alert::Bell).unwrap();
            let batch = state.notifications.protected_batch_up_to(1).unwrap();
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
            batch
        };
        let mut guard = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);
        guard.acknowledge_current().unwrap();
        per_pane.lock().unwrap().legacy_enqueue_phase = LegacyRenderEnqueuePhase::Idle;

        assert!(guard.settle_completed().is_err());
        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Closed
            ));
            assert!(state.transactional_dirty);
        }
        guard.retire();
    }

    #[test]
    fn notification_rollback_retires_and_clears_a_poisoned_state_lock() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let batch = {
            let mut state = per_pane.lock().unwrap();
            state
                .push_notification(Alert::Bell)
                .expect("retain exact event");
            let batch = state
                .notifications
                .protected_batch_up_to(1)
                .expect("protect exact notification batch");
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
            batch
        };
        let guard = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);
        poison_per_pane_lock(&per_pane);

        assert!(guard.rollback().is_err());
        let state = per_pane
            .lock()
            .expect("notification terminal recovery must clear mutex poison");
        assert_eq!(state.notifications, vec![Alert::Bell]);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
    }

    #[test]
    fn notification_ack_retires_poison_under_the_recovered_guard() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let batch = {
            let mut state = per_pane.lock().unwrap();
            state
                .push_notification(Alert::Bell)
                .expect("retain exact event");
            let batch = state
                .notifications
                .protected_batch_up_to(1)
                .expect("protect exact notification batch");
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
            batch
        };
        let mut guard = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);
        poison_per_pane_lock(&per_pane);

        let error = guard
            .acknowledge_current()
            .expect_err("poisoned notification acknowledgement must fail closed");
        assert!(error.to_string().contains("state lock poisoned"));
        {
            let state = per_pane
                .lock()
                .expect("notification acknowledgement must clear mutex poison before return");
            assert_eq!(state.notifications, vec![Alert::Bell]);
            assert_eq!(state.notifications.protected_prefix_len, 0);
            assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Closed
            ));
            assert!(state.transactional_dirty);
        }
        guard.retire();
    }

    #[test]
    fn notification_settlement_retires_poison_under_the_recovered_guard() {
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let batch = {
            let mut state = per_pane.lock().unwrap();
            state
                .push_notification(Alert::Bell)
                .expect("retain exact event");
            let batch = state
                .notifications
                .protected_batch_up_to(1)
                .expect("protect exact notification batch");
            state.legacy_enqueue_phase = LegacyRenderEnqueuePhase::NotificationsInFlight;
            batch
        };
        let mut guard = UnsentNotificationsGuard::new(Arc::clone(&per_pane), batch);
        guard
            .acknowledge_current()
            .expect("exact notification acknowledgement succeeds");
        poison_per_pane_lock(&per_pane);

        let error = guard
            .acknowledge_all()
            .expect_err("poisoned notification settlement must fail closed");
        assert!(error.to_string().contains("state lock poisoned"));
        let state = per_pane
            .lock()
            .expect("notification settlement poison repair must clear the mutex poison");
        assert!(state.notifications.is_empty());
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert!(state.transactional_dirty);
    }

    #[test]
    fn failed_notification_enqueue_releases_prefix_without_merging_concurrent_suffix() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let sender = PduSender::new({
            let enqueue_count = Arc::clone(&enqueue_count);
            let per_pane = Arc::clone(&per_pane);
            move |_, _| {
                if enqueue_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Ok(());
                }
                per_pane
                    .try_lock()
                    .expect("notification sender callback runs outside the pane-state lock")
                    .push_notification(Alert::Bell)
                    .expect("retain concurrent exact event behind protected prefix");
                Err(anyhow!("synthetic notification enqueue failure"))
            }
        });

        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect_err("the synthetic palette enqueue must fail");

        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.notifications,
            vec![Alert::PaletteChanged, Alert::Bell],
            "rollback must retain the untouched protected prefix before its concurrent suffix"
        );
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
    }

    #[test]
    fn legacy_notification_push_drains_every_initial_bounded_wire_batch() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        {
            let mut state = per_pane.lock().unwrap();
            state.baseline.sent_initial_palette = true;
            state.baseline.config_generation = config::configuration().generation();
            for _ in 0..=codec::MAX_RENDER_APPLICATION_ALERTS {
                state
                    .push_notification(Alert::Bell)
                    .expect("retain a two-batch exact-event backlog");
            }
        }
        let (sender, captured) = capturing_sender();

        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("bounded legacy batches should all enqueue");

        let alert_count = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|decoded| matches!(&decoded.pdu, Pdu::NotifyAlert(_)))
            .count();
        assert_eq!(alert_count, codec::MAX_RENDER_APPLICATION_ALERTS + 1);
        let state = per_pane.lock().unwrap();
        assert!(state.notifications.is_empty());
        assert_eq!(state.notifications.protected_prefix_len, 0);
    }

    #[test]
    fn legacy_notification_batches_do_not_absorb_reentrant_post_snapshot_suffix() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        {
            let mut state = per_pane.lock().unwrap();
            state.baseline.sent_initial_palette = true;
            state.baseline.config_generation = config::configuration().generation();
            for _ in 0..=codec::MAX_RENDER_APPLICATION_ALERTS {
                state
                    .push_notification(Alert::Bell)
                    .expect("retain a two-batch initial exact-event obligation");
            }
        }
        let captured = Arc::new(Mutex::new(Vec::<DecodedPdu>::new()));
        let injected = Arc::new(AtomicBool::new(false));
        let sender = PduSender::new({
            let captured = Arc::clone(&captured);
            let injected = Arc::clone(&injected);
            let per_pane = Arc::clone(&per_pane);
            move |decoded, _| {
                let is_alert = matches!(&decoded.pdu, Pdu::NotifyAlert(_));
                captured.lock().unwrap().push(decoded);
                if is_alert && !injected.swap(true, Ordering::Relaxed) {
                    per_pane
                        .try_lock()
                        .expect("notification callback runs outside pane-state authority")
                        .push_notification(Alert::Bell)
                        .expect("retain reentrant exact event behind initial obligation");
                }
                Ok(())
            }
        });

        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender.clone(), Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("the bounded initial alert obligation should enqueue");

        let first_push_alert_count = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|decoded| matches!(&decoded.pdu, Pdu::NotifyAlert(_)))
            .count();
        assert_eq!(
            first_push_alert_count,
            codec::MAX_RENDER_APPLICATION_ALERTS + 1,
            "the first push must drain exactly its initial snapshot obligation"
        );
        {
            let state = per_pane.lock().unwrap();
            assert_eq!(state.notifications, vec![Alert::Bell]);
            assert_eq!(state.notifications.protected_prefix_len, 0);
        }

        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("the separately scheduled suffix should enqueue");

        let total_alert_count = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|decoded| matches!(&decoded.pdu, Pdu::NotifyAlert(_)))
            .count();
        assert_eq!(total_alert_count, codec::MAX_RENDER_APPLICATION_ALERTS + 2);
        let state = per_pane.lock().unwrap();
        assert!(state.notifications.is_empty());
        assert_eq!(state.notifications.protected_prefix_len, 0);
    }

    #[test]
    fn reentrant_notification_push_cannot_revoke_the_active_batch_owner() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let (bootstrap_sender, _) = capturing_sender();
        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, bootstrap_sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("bootstrap render and palette settle");
        per_pane
            .lock()
            .unwrap()
            .push_notification(Alert::Bell)
            .expect("retain exact event for reentrant delivery test");

        let (nested_sender, nested_captured) = capturing_sender();
        let nested_rejected = Arc::new(AtomicBool::new(false));
        let sender = PduSender::new({
            let registration = registration.clone();
            let per_pane = Arc::clone(&per_pane);
            let nested_rejected = Arc::clone(&nested_rejected);
            move |decoded, _| {
                if matches!(&decoded.pdu, Pdu::NotifyAlert(_))
                    && !nested_rejected.swap(true, Ordering::Relaxed)
                {
                    let nested = registration
                        .try_with_current(|current| {
                            maybe_push_pane_changes(
                                &current,
                                nested_sender.clone(),
                                Arc::clone(&per_pane),
                            )
                        })
                        .expect("nested pane registration remains current");
                    let error =
                        nested.expect_err("the active notification batch must retain authority");
                    assert!(error.to_string().contains("already active"));
                }
                Ok(())
            }
        });

        registration
            .try_with_current(|current| {
                maybe_push_pane_changes(&current, sender, Arc::clone(&per_pane))
            })
            .expect("test pane registration remains current")
            .expect("the active notification owner must settle after reentrant rejection");

        assert!(nested_rejected.load(Ordering::Relaxed));
        assert!(nested_captured.lock().unwrap().is_empty());
        let state = per_pane.lock().unwrap();
        assert!(state.notifications.is_empty());
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Idle);
        assert!(!matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
    }

    #[test]
    fn panicked_key_down_render_enqueue_retires_indeterminate_delivery() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane.lock().unwrap().transactional_dirty = false;
        let sender = PduSender::new(|_, _| -> anyhow::Result<()> {
            panic!("synthetic key-down render enqueue panic")
        });

        registration
            .try_with_current(|current| {
                push_input_dispatch_changes_after_committed_input(
                    &current,
                    sender,
                    Arc::clone(&per_pane),
                    InputSerial::empty(),
                    "key-down",
                );
            })
            .expect("test pane registration remains current");

        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.seqno, 11);
        assert!(state.transactional_dirty);
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
    }

    #[test]
    fn transactional_render_prepares_without_lock_and_commits_only_on_exact_ack() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane
            .lock()
            .unwrap()
            .push_notification(Alert::Bell)
            .expect("retain initial exact event");
        let callback_count = Arc::new(AtomicUsize::new(0));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(77, {
            let per_pane = Arc::clone(&per_pane);
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                let mut state = per_pane
                    .try_lock()
                    .expect("pane callback must run without the per-pane state lock");
                assert_eq!(state.baseline.seqno, 0);
                assert_eq!(state.notifications, vec![Alert::Bell]);
                state.mark_transactional_dirty();
                callback_count.fetch_add(1, Ordering::Relaxed);
            })
        }));
        let (_mux, registration) = register_test_pane(&pane);

        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .expect("transactional render preparation"),
        );
        assert_eq!(callback_count.load(Ordering::Relaxed), 1);
        assert_eq!(prepared.surface.seqno, 11);
        assert_eq!(prepared.surface.title, "tiered-pane");
        assert_eq!(prepared.surface.cursor_position.x, 4);
        assert_eq!(prepared.surface.dimensions.cols, 80);
        assert_eq!(prepared.semantic_zones.zones.len(), 0);
        assert!(prepared.palette.is_some());
        assert_eq!(prepared.alerts.len(), 1);

        {
            let state = per_pane.lock().unwrap();
            assert_eq!(
                state.baseline,
                PaneRenderBaseline::default(),
                "preparation must not advance the authoritative baseline"
            );
            assert_eq!(
                state.notifications,
                vec![Alert::Bell],
                "preparation must not drain application effects"
            );
        }

        assert_eq!(
            prepared.acknowledge(),
            PaneRenderSettlement::AcknowledgedRedirtied
        );
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.seqno, 11);
        assert_eq!(state.baseline.title, "tiered-pane");
        assert!(state.baseline.sent_initial_palette);
        assert!(state.notifications.is_empty());
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_render_rejects_wrong_and_duplicate_ack_and_drop_retries() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        let token = prepared.token();
        let wrong = PaneRenderAttemptToken {
            pane_id: token.pane_id + 1,
            ..token
        };
        assert_eq!(
            per_pane.lock().unwrap().acknowledge_prepared(wrong),
            PaneRenderSettlement::StaleOrDuplicate
        );
        assert_eq!(per_pane.lock().unwrap().baseline.seqno, 0);
        assert_eq!(
            prepared.acknowledge(),
            PaneRenderSettlement::AcknowledgedClean
        );
        assert_eq!(
            per_pane.lock().unwrap().acknowledge_prepared(token),
            PaneRenderSettlement::StaleOrDuplicate
        );

        {
            let mut pane_state = pane.state.lock().unwrap();
            pane_state.title = "candidate-must-not-commit".to_string();
            pane_state.seqno = 12;
        }
        per_pane.lock().unwrap().mark_transactional_dirty();
        let abandoned = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        let abandoned_token = abandoned.token();
        assert_eq!(
            per_pane.lock().unwrap().baseline.title,
            "tiered-pane",
            "an in-flight candidate is not yet authoritative"
        );
        drop(abandoned);

        let mut state = per_pane.lock().unwrap();
        assert_eq!(state.baseline.title, "tiered-pane");
        assert!(state.transactional_dirty);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Idle
        ));
        assert_eq!(
            state.acknowledge_prepared(abandoned_token),
            PaneRenderSettlement::StaleOrDuplicate
        );
    }

    #[test]
    fn prepared_render_dropped_during_unwind_retires_indeterminate_delivery() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .expect("prepare transactional render"),
        );

        let result = catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(move || {
                let _prepared = prepared;
                panic!("synthetic panic after transactional render handoff");
            }),
        );
        assert!(result.is_err(), "the synthetic unwind must be recovered");

        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_ack_advances_shared_baseline_revision_and_exhaustion_fails_closed() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        let first = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .expect("prepare first transactional render"),
        );
        assert_eq!(first.acknowledge(), PaneRenderSettlement::AcknowledgedClean);
        assert_eq!(per_pane.lock().unwrap().baseline_revision, 1);

        {
            let mut state = per_pane.lock().unwrap();
            state.baseline_revision = u64::MAX;
            state.mark_transactional_dirty();
        }
        let exhausted = expect_prepared(
            prepare_transactional_for_registration(
                Arc::clone(&per_pane),
                &registration,
                Some(InputSerial::empty()),
            )
            .expect("prepare render before shared baseline identity exhaustion"),
        );
        assert_eq!(exhausted.acknowledge(), PaneRenderSettlement::FailedClosed);
        let state = per_pane.lock().unwrap();
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline_revision, u64::MAX);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn legacy_render_cannot_mutate_an_active_transaction() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        per_pane
            .lock()
            .unwrap()
            .push_notification(Alert::Bell)
            .expect("retain exact event before transaction");
        let preparation =
            begin_transactional_pane_render(Arc::clone(&per_pane), pane.pane_id(), None)
                .expect("transactional preparation owns the pane");

        let result = registration
            .try_with_current(|current| prepare_legacy_render_enqueue(&current, &per_pane, None))
            .expect("test pane registration remains current");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("legacy preparation must reject an active transaction"),
        };
        assert!(error.to_string().contains("already active"));
        {
            let state = per_pane.lock().unwrap();
            assert!(matches!(
                state.transaction_phase,
                PaneRenderTransactionPhase::Preparing { .. }
            ));
            assert_eq!(state.notifications, vec![Alert::Bell]);
            assert_eq!(state.notifications.protected_prefix_len, 1);
        }

        drop(preparation);
        let state = per_pane.lock().unwrap();
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Idle
        ));
        assert_eq!(state.notifications.protected_prefix_len, 0);
    }

    #[test]
    fn transactional_begin_retires_and_clears_a_poisoned_state_lock() {
        let state = Arc::new(Mutex::new(PerPane::default()));
        poison_per_pane_lock(&state);

        assert!(matches!(
            begin_transactional_pane_render(Arc::clone(&state), 81, None),
            Err(PaneRenderPreparationError::StateLockPoisoned)
        ));
        let state = state
            .lock()
            .expect("terminal recovery must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn poisoned_state_repair_is_visible_before_the_first_waiter_can_enter() {
        let state = Arc::new(Mutex::new(PerPane::default()));
        poison_per_pane_lock(&state);
        let poison = state
            .lock()
            .expect_err("synthetic panic must leave the state lock poisoned");
        let waiter_started = Arc::new(std::sync::Barrier::new(2));
        let waiter = {
            let state = Arc::clone(&state);
            let waiter_started = Arc::clone(&waiter_started);
            std::thread::spawn(move || {
                waiter_started.wait();
                let observed = state
                    .lock()
                    .expect("atomic poison repair must clear poison before releasing the lock");
                (
                    matches!(
                        observed.transaction_phase,
                        PaneRenderTransactionPhase::Closed
                    ),
                    observed.legacy_enqueue_phase,
                    observed.notifications.protected_prefix_len,
                    observed.transactional_dirty,
                )
            })
        };
        waiter_started.wait();

        retire_poisoned_pane_render(&state, poison);

        let (transaction_closed, legacy_phase, protected_prefix_len, dirty) = waiter
            .join()
            .expect("observer of repaired state must not panic");
        assert!(transaction_closed);
        assert_eq!(legacy_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(protected_prefix_len, 0);
        assert!(dirty);
    }

    #[test]
    fn terminal_sequence_settlement_retires_poison_under_the_recovered_guard() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().seqno = SequenceNo::MAX;
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let state = Arc::new(Mutex::new(PerPane::default()));
        let preparation =
            begin_transactional_pane_render(Arc::clone(&state), registration.pane_id(), None)
                .expect("transactional preparation owns the state");
        poison_per_pane_lock(&state);

        let result = registration
            .try_with_current(move |pane| preparation.prepare(&pane))
            .expect("test pane registration remains current");
        assert_eq!(
            result.unwrap_err(),
            PaneRenderPreparationError::StateLockPoisoned
        );

        let state = state
            .lock()
            .expect("terminal-sequence poison repair must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn no_change_settlement_retires_poison_under_the_recovered_guard() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let state = Arc::new(Mutex::new(PerPane::default()));
        let initial = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&state), &registration, None)
                .expect("prepare initial transactional render"),
        );
        assert_eq!(
            initial.acknowledge(),
            PaneRenderSettlement::AcknowledgedClean
        );
        pane.state.lock().unwrap().seqno = 12;
        state.lock().unwrap().mark_transactional_dirty();
        let preparation =
            begin_transactional_pane_render(Arc::clone(&state), registration.pane_id(), None)
                .expect("transactional preparation owns the state");
        poison_per_pane_lock(&state);

        let result = registration
            .try_with_current(move |pane| preparation.prepare(&pane))
            .expect("test pane registration remains current");
        assert_eq!(
            result.unwrap_err(),
            PaneRenderPreparationError::StateLockPoisoned
        );

        let state = state
            .lock()
            .expect("no-change poison repair must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline.seqno, 11);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn prepared_install_retires_poison_under_the_recovered_guard() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let state = Arc::new(Mutex::new(PerPane::default()));
        let preparation =
            begin_transactional_pane_render(Arc::clone(&state), registration.pane_id(), None)
                .expect("transactional preparation owns the state");
        poison_per_pane_lock(&state);

        let result = registration
            .try_with_current(move |pane| preparation.prepare(&pane))
            .expect("test pane registration remains current");
        assert_eq!(
            result.unwrap_err(),
            PaneRenderPreparationError::StateLockPoisoned
        );

        let state = state
            .lock()
            .expect("prepared-install poison repair must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn abandoned_transactional_preparation_retires_a_poisoned_state_lock() {
        let state = Arc::new(Mutex::new(PerPane::default()));
        let preparation = begin_transactional_pane_render(Arc::clone(&state), 82, None)
            .expect("transactional preparation owns the state");
        poison_per_pane_lock(&state);

        drop(preparation);

        let state = state
            .lock()
            .expect("drop-time terminal recovery must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_ack_retires_a_poisoned_inflight_state() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let state = Arc::new(Mutex::new(PerPane::default()));
        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&state), &registration, None)
                .expect("prepare transactional render"),
        );
        poison_per_pane_lock(&state);

        assert_eq!(prepared.acknowledge(), PaneRenderSettlement::FailedClosed);
        let state = state
            .lock()
            .expect("ack terminal recovery must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_nack_retires_a_poisoned_inflight_state() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let state = Arc::new(Mutex::new(PerPane::default()));
        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&state), &registration, None)
                .expect("prepare transactional render"),
        );
        poison_per_pane_lock(&state);

        assert_eq!(prepared.nack(), PaneRenderSettlement::FailedClosed);
        let state = state
            .lock()
            .expect("nack terminal recovery must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert!(state.transactional_dirty);
    }

    #[test]
    fn abandoned_prepared_render_retires_a_poisoned_inflight_state() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let state = Arc::new(Mutex::new(PerPane::default()));
        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&state), &registration, None)
                .expect("prepare transactional render"),
        );
        poison_per_pane_lock(&state);

        drop(prepared);

        let state = state
            .lock()
            .expect("drop-time terminal recovery must clear the mutex poison");
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.legacy_enqueue_phase, LegacyRenderEnqueuePhase::Closed);
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_preparation_fails_closed_on_an_unowned_protected_prefix() {
        let mut state = PerPane::default();
        state
            .push_notification(Alert::Bell)
            .expect("retain exact event before corrupting prefix authority");
        state.notifications.protect_prefix(1);

        assert_eq!(
            state
                .begin_transactional_preparation(81, None)
                .expect_err("idle state cannot silently adopt an unowned protected prefix"),
            PaneRenderPreparationError::NotificationPrefixChanged
        );
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(state.notifications, vec![Alert::Bell]);
        assert_eq!(state.notifications.protected_prefix_len, 0);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_alerts_are_exact_events_or_coalesced_state_until_ack() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        {
            let mut state = per_pane.lock().unwrap();
            for alert in [
                Alert::Bell,
                Alert::OutputSinceFocusLost,
                Alert::OutputSinceFocusLost,
                Alert::Progress(Progress::Percentage(25)),
                Alert::Progress(Progress::Percentage(50)),
                Alert::PaletteChanged,
            ] {
                state
                    .push_notification(alert)
                    .expect("retain bounded transactional alert");
            }
        }

        let first = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        assert!(first.palette.is_some());
        assert_eq!(
            first
                .alerts
                .iter()
                .filter(|alert| alert.alert == Alert::Bell)
                .count(),
            1
        );
        assert_eq!(
            first
                .alerts
                .iter()
                .filter(|alert| alert.alert == Alert::OutputSinceFocusLost)
                .count(),
            1
        );
        assert!(
            first
                .alerts
                .iter()
                .any(|alert| { alert.alert == Alert::Progress(Progress::Percentage(50)) })
        );
        assert!(
            !first
                .alerts
                .iter()
                .any(|alert| { alert.alert == Alert::Progress(Progress::Percentage(25)) })
        );
        assert_eq!(first.nack(), PaneRenderSettlement::Retried);
        assert_eq!(
            per_pane.lock().unwrap().notifications.len(),
            4,
            "state-like alerts are coalesced before they consume backlog capacity"
        );

        let retry = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        per_pane
            .lock()
            .unwrap()
            .push_notification(Alert::Bell)
            .expect("retain event arriving behind protected prefix");
        assert_eq!(
            retry.acknowledge(),
            PaneRenderSettlement::AcknowledgedRedirtied
        );
        let state = per_pane.lock().unwrap();
        assert_eq!(state.notifications, vec![Alert::Bell]);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn transactional_alert_batch_is_bounded_and_retains_unsent_suffix() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        {
            let mut state = per_pane.lock().unwrap();
            for _ in 0..=codec::MAX_RENDER_APPLICATION_ALERTS {
                state
                    .push_notification(Alert::Bell)
                    .expect("retain bounded exact event batch");
            }
        }

        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        assert_eq!(prepared.alerts.len(), codec::MAX_RENDER_APPLICATION_ALERTS);
        assert_eq!(
            prepared.acknowledge(),
            PaneRenderSettlement::AcknowledgedRedirtied
        );
        let state = per_pane.lock().unwrap();
        assert_eq!(state.notifications, vec![Alert::Bell]);
        assert!(state.transactional_dirty);
    }

    #[test]
    fn pane_alert_backlog_is_count_bounded_without_mutating_in_flight_prefix() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        {
            let mut state = per_pane.lock().unwrap();
            for _ in 0..MAX_PENDING_PANE_ALERTS {
                state
                    .push_notification(Alert::Bell)
                    .expect("fill exact-event backlog to its hard bound");
            }
        }

        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .expect("bounded backlog should prepare its exact first wire prefix"),
        );
        assert_eq!(prepared.alerts.len(), codec::MAX_RENDER_APPLICATION_ALERTS);

        {
            let mut state = per_pane.lock().unwrap();
            let error = state
                .push_notification(Alert::ToastNotification {
                    title: None,
                    body: "cannot-fit".to_string(),
                    focus: false,
                })
                .expect_err("a full exact-event backlog must reject before mutation");
            assert_eq!(error, PaneAlertBacklogError::ExactEventCapacityExhausted);
            assert_eq!(state.notifications.len(), MAX_PENDING_PANE_ALERTS);
            assert!(
                state
                    .notifications
                    .as_slice()
                    .iter()
                    .all(|alert| *alert == Alert::Bell)
            );
        }

        assert_eq!(
            prepared.acknowledge(),
            PaneRenderSettlement::AcknowledgedRedirtied
        );
        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.notifications.len(),
            codec::MAX_RENDER_APPLICATION_ALERTS
        );
        assert!(
            state
                .notifications
                .as_slice()
                .iter()
                .all(|alert| *alert == Alert::Bell)
        );
    }

    #[test]
    fn pane_alert_backlog_enforces_per_alert_and_aggregate_text_bounds() {
        let mut alerts = PendingPaneAlerts::default();
        assert_eq!(
            alerts
                .push(Alert::WindowTitleChanged(
                    "x".repeat(codec::MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES + 1,)
                ))
                .expect_err("an individually unencodable alert must be rejected"),
            PaneAlertBacklogError::SingleAlertTextLimit
        );
        assert!(
            alerts.is_empty(),
            "an individually unencodable alert must not enter retained state"
        );

        let chunk_len = codec::MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES * 3 / 4;
        for index in 0..2 {
            alerts
                .push(Alert::ToastNotification {
                    title: None,
                    body: format!("{index}{}", "x".repeat(chunk_len - 1)),
                    focus: false,
                })
                .expect("two large exact events fit the retained-byte bound");
        }
        assert_eq!(
            alerts
                .push(Alert::ToastNotification {
                    title: None,
                    body: format!("2{}", "x".repeat(chunk_len - 1)),
                    focus: false,
                })
                .expect_err("aggregate overflow must reject without eviction"),
            PaneAlertBacklogError::ExactEventCapacityExhausted
        );
        assert_eq!(alerts.len(), 2);
        assert!(alerts.retained_text_bytes <= MAX_PENDING_PANE_ALERT_TEXT_BYTES);
        assert!(matches!(
            alerts.as_slice().first(),
            Some(Alert::ToastNotification { body, .. }) if body.starts_with('0')
        ));
        assert!(matches!(
            alerts.as_slice().last(),
            Some(Alert::ToastNotification { body, .. }) if body.starts_with('1')
        ));
        assert_eq!(alerts.wire_prefix_len_up_to(alerts.len()).unwrap(), 1);
    }

    #[test]
    fn pane_alert_backlog_fails_closed_on_counter_drift_without_mutation() {
        let mut alerts = PendingPaneAlerts::default();
        alerts
            .push(Alert::WindowTitleChanged("retained".to_string()))
            .expect("retain initial state alert");
        let before = alerts.entries.clone();
        alerts.retained_text_bytes += 1;

        assert_eq!(
            alerts
                .push(Alert::Bell)
                .expect_err("counter drift must reject before queue mutation"),
            PaneAlertBacklogError::AccountingDrift
        );
        assert_eq!(alerts.entries, before);
        assert_eq!(alerts.retained_text_bytes, "retained".len() + 1);
    }

    #[test]
    fn pane_alert_backlog_validates_hard_capacity_invariants_before_mutation() {
        let mut over_count = PendingPaneAlerts {
            entries: vec![Alert::Bell; MAX_PENDING_PANE_ALERTS + 1],
            retained_text_bytes: 0,
            protected_prefix_len: 0,
        };
        assert_eq!(
            over_count
                .push(Alert::Bell)
                .expect_err("an over-count retained backlog must fail closed"),
            PaneAlertBacklogError::CapacityInvariantExceeded
        );
        assert_eq!(over_count.len(), MAX_PENDING_PANE_ALERTS + 1);

        let mut over_bytes = PendingPaneAlerts {
            entries: Vec::new(),
            retained_text_bytes: MAX_PENDING_PANE_ALERT_TEXT_BYTES + 1,
            protected_prefix_len: 0,
        };
        assert_eq!(
            over_bytes
                .push(Alert::Bell)
                .expect_err("an over-byte retained backlog must fail closed"),
            PaneAlertBacklogError::CapacityInvariantExceeded
        );
        assert!(over_bytes.is_empty());
        assert_eq!(
            over_bytes.retained_text_bytes,
            MAX_PENDING_PANE_ALERT_TEXT_BYTES + 1
        );
    }

    #[test]
    fn pane_alert_backlog_coalesces_only_replaceable_unprotected_state() {
        let mut alerts = PendingPaneAlerts::default();
        alerts
            .push(Alert::Progress(Progress::Percentage(25)))
            .expect("retain initial state");
        let protected = alerts
            .protected_batch_up_to(usize::MAX)
            .expect("protect wire prefix");
        alerts
            .push(Alert::Progress(Progress::Percentage(50)))
            .expect("retain newer state behind protected prefix");
        alerts
            .push(Alert::Progress(Progress::Percentage(75)))
            .expect("coalesce replaceable suffix state");
        alerts
            .push(Alert::SetUserVar {
                name: "mode".to_string(),
                value: "one".to_string(),
            })
            .expect("retain first exact user-var event");
        alerts
            .push(Alert::SetUserVar {
                name: "mode".to_string(),
                value: "two".to_string(),
            })
            .expect("retain repeated exact user-var event without coalescing");

        assert_eq!(alerts.len(), 4);
        assert_eq!(
            alerts.as_slice()[0],
            Alert::Progress(Progress::Percentage(25))
        );
        assert_eq!(
            alerts.as_slice()[1],
            Alert::Progress(Progress::Percentage(75))
        );
        alerts
            .release_protected_prefix(&protected)
            .expect("release exact protected prefix");
    }

    #[test]
    fn pane_alert_state_coalescing_preserves_position_after_interleaved_exact_event() {
        let mut alerts = PendingPaneAlerts::default();
        alerts
            .push(Alert::WindowTitleChanged("old".to_string()))
            .expect("retain old replaceable state");
        alerts
            .push(Alert::Bell)
            .expect("retain interleaved exact event");
        alerts
            .push(Alert::WindowTitleChanged("new".to_string()))
            .expect("coalesce state at its new temporal position");

        assert_eq!(
            alerts.as_slice(),
            &[Alert::Bell, Alert::WindowTitleChanged("new".to_string())]
        );
        assert_eq!(alerts.retained_text_bytes, "new".len());
        assert_eq!(alerts.protected_prefix_len, 0);
        assert!(alerts.validate_accounting().is_ok());
    }

    #[test]
    fn equal_input_serials_receive_distinct_internal_epochs() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let serial = InputSerial::empty();

        let first = expect_prepared(
            prepare_transactional_for_registration(
                Arc::clone(&per_pane),
                &registration,
                Some(serial),
            )
            .unwrap(),
        );
        let first_token = first.token();
        assert_eq!(first.surface.input_serial, Some(serial));
        assert_eq!(first.acknowledge(), PaneRenderSettlement::AcknowledgedClean);

        per_pane.lock().unwrap().mark_transactional_dirty();
        let second = expect_prepared(
            prepare_transactional_for_registration(
                Arc::clone(&per_pane),
                &registration,
                Some(serial),
            )
            .unwrap(),
        );
        let second_token = second.token();
        assert_eq!(second.surface.input_serial, Some(serial));
        assert_ne!(first_token, second_token);
        assert_eq!(first_token.input_epoch, Some(1));
        assert_eq!(second_token.input_epoch, Some(2));
        assert_eq!(
            second.acknowledge(),
            PaneRenderSettlement::AcknowledgedClean
        );
        assert_eq!(per_pane.lock().unwrap().baseline.committed_input_epoch, 2);
    }

    #[test]
    fn no_change_settlement_keeps_committed_sequence_baseline() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        let initial = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        assert_eq!(
            initial.acknowledge(),
            PaneRenderSettlement::AcknowledgedClean
        );
        pane.state.lock().unwrap().seqno = 12;
        per_pane.lock().unwrap().mark_transactional_dirty();

        let outcome =
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap();
        assert!(matches!(
            outcome,
            PaneRenderPreparationOutcome::NoChange(PaneRenderSettlement::SettledNoChangeClean)
        ));
        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.baseline.seqno, 11,
            "a pure no-effect settlement cannot advance the client baseline"
        );
        assert!(!state.transactional_dirty);
    }

    #[test]
    fn source_change_and_preparation_drop_restore_dirty_obligation() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let mut fake = FakePane::new(None);
        fake.seqno_on_dimensions = Some(12);
        let pane: Arc<dyn Pane> = Arc::new(fake);
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        assert_eq!(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None,)
                .unwrap_err(),
            PaneRenderPreparationError::SourceChanged
        );
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert!(state.transactional_dirty);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Idle
        ));
    }

    #[test]
    fn transactional_viewport_overflow_retries_without_committing_baseline() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().dimensions.physical_top = StableRowIndex::MAX;
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        assert_eq!(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None,)
                .unwrap_err(),
            PaneRenderPreparationError::StableRowRangeUnrepresentable
        );
        let state = per_pane.lock().unwrap();
        assert_eq!(state.baseline, PaneRenderBaseline::default());
        assert!(state.transactional_dirty);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Idle
        ));
    }

    #[test]
    fn terminal_sequence_saturation_closes_before_ambiguous_identity() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().seqno = SequenceNo::MAX;
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));

        assert_eq!(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None,)
                .unwrap_err(),
            PaneRenderPreparationError::TerminalSequenceExhausted
        );
        assert!(matches!(
            per_pane.lock().unwrap().transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert!(matches!(
            begin_transactional_pane_render(per_pane, registration.pane_id(), None),
            Err(PaneRenderPreparationError::Closed)
        ));
    }

    #[test]
    fn transactional_identity_exhaustion_and_competing_preparer_fail_closed() {
        let mut attempt_exhausted = PerPane {
            next_render_attempt: Some(u64::MAX),
            ..PerPane::default()
        };
        assert!(matches!(
            attempt_exhausted.begin_transactional_preparation(1, None),
            Err(PaneRenderPreparationError::AttemptIdentityExhausted)
        ));
        assert!(matches!(
            attempt_exhausted.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(
            attempt_exhausted.legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::Closed
        );

        let mut input_exhausted = PerPane {
            next_input_epoch: Some(u64::MAX),
            ..PerPane::default()
        };
        assert!(matches!(
            input_exhausted.begin_transactional_preparation(1, Some(InputSerial::empty())),
            Err(PaneRenderPreparationError::InputIdentityExhausted)
        ));
        assert!(matches!(
            input_exhausted.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(
            input_exhausted.legacy_enqueue_phase,
            LegacyRenderEnqueuePhase::Closed
        );

        let state = Arc::new(Mutex::new(PerPane::default()));
        let first = begin_transactional_pane_render(Arc::clone(&state), 1, None)
            .expect("first preparer owns the pane");
        assert!(matches!(
            begin_transactional_pane_render(Arc::clone(&state), 1, None),
            Err(PaneRenderPreparationError::Busy)
        ));
        drop(first);
        let state = state.lock().unwrap();
        assert!(state.transactional_dirty);
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Idle
        ));
    }

    #[test]
    fn pane_close_invalidates_in_flight_transaction_without_resurrection() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let prepared = expect_prepared(
            prepare_transactional_for_registration(Arc::clone(&per_pane), &registration, None)
                .unwrap(),
        );
        let token = prepared.token();
        per_pane.lock().unwrap().close_transaction();
        drop(prepared);

        let mut state = per_pane.lock().unwrap();
        assert!(matches!(
            state.transaction_phase,
            PaneRenderTransactionPhase::Closed
        ));
        assert_eq!(
            state.acknowledge_prepared(token),
            PaneRenderSettlement::Closed
        );
        assert_eq!(state.baseline, PaneRenderBaseline::default());
    }

    #[test]
    fn compute_changes_includes_initial_tiered_scrollback_status_snapshot() {
        let pane = Arc::new(FakePane::new(Some(sample_tiered_scrollback_status(12))));
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("initial pane snapshot should produce a response");

        assert_eq!(
            response.tiered_scrollback_status,
            Some(sample_tiered_scrollback_status(12))
        );
        assert_eq!(
            per_pane.baseline.tiered_scrollback_status,
            Some(sample_tiered_scrollback_status(12))
        );
    }

    #[test]
    fn legacy_compute_changes_rejects_overflow_without_advancing_baseline() {
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().dimensions.physical_top = StableRowIndex::MAX;
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let result = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current");
        assert!(matches!(
            result,
            Err(PaneRenderPreparationError::StableRowRangeUnrepresentable)
        ));
        assert_eq!(
            per_pane.baseline,
            PaneRenderBaseline::default(),
            "an unrepresentable viewport must not become the delivered baseline"
        );
        assert!(
            pane.take_changed_since_seqnos().is_empty(),
            "range validation must fail before querying a misleading empty span"
        );

        pane.state.lock().unwrap().dimensions.physical_top = 0;
        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("a representable retry should prepare successfully")
            .expect("the retained initial snapshot should be delivered on retry");
        assert_eq!(response.seqno, 11);
        assert_eq!(per_pane.baseline.seqno, 11);
    }

    #[test]
    fn legacy_compute_changes_rejects_unrepresentable_cursor_row() {
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().cursor_position.y = StableRowIndex::MAX;
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let result = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current");
        assert!(matches!(
            result,
            Err(PaneRenderPreparationError::StableRowRangeUnrepresentable)
        ));
        assert_eq!(
            per_pane.baseline,
            PaneRenderBaseline::default(),
            "an unrepresentable cursor row must not advance the delivered baseline"
        );

        pane.state.lock().unwrap().cursor_position.y = 0;
        registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("representable cursor retry should prepare successfully")
            .expect("retained initial snapshot should be delivered on retry");
    }

    #[test]
    fn legacy_compute_changes_rejects_unrepresentable_returned_cursor_span() {
        let mut fake = FakePane::new(None);
        fake.cursor_line_start_override = Some(StableRowIndex::MAX);
        let pane = Arc::new(fake);
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let result = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current");
        assert!(matches!(
            result,
            Err(PaneRenderPreparationError::StableRowRangeUnrepresentable)
        ));
        assert_eq!(
            per_pane.baseline,
            PaneRenderBaseline::default(),
            "an unrepresentable backend cursor span must not advance the baseline"
        );
    }

    #[test]
    fn key_down_ack_survives_post_input_render_range_failure() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().dimensions.physical_top = StableRowIndex::MAX;
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_dyn)
            .expect("register input ACK test pane");
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));

        handler.process_one(DecodedPdu {
            serial: 9_701,
            pdu: Pdu::SendKeyDown(SendKeyDown {
                pane_id: pane.pane_id(),
                event: termwiz::input::KeyEvent {
                    key: KeyCode::Char('x'),
                    modifiers: KeyModifiers::NONE,
                },
                input_serial: InputSerial::empty(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 9_701);
        assert_eq!(response.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert_eq!(
            pane.key_down_count(),
            1,
            "the key must be applied exactly once"
        );
        let per_pane = handler
            .per_pane_if_present(pane.pane_id())
            .expect("key-down should retain per-pane state");
        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.baseline,
            PaneRenderBaseline::default(),
            "failed post-input render preparation must not advance its baseline"
        );
        assert!(
            state.transactional_dirty,
            "failed post-input render preparation must retain a retry obligation"
        );
    }

    #[test]
    fn paste_propagates_exact_input_serial_in_forced_dispatch_fence() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_dyn)
            .expect("register paste dispatch-fence test pane");
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));
        let input_serial = InputSerial::now();

        handler.process_one(DecodedPdu {
            serial: 9_702,
            pdu: Pdu::SendPaste(SendPaste {
                pane_id: pane.pane_id(),
                data: "paste".to_string(),
                input_serial,
            }),
        });
        tick_until_response(&executor, &captured, 2);

        let responses = captured.lock().unwrap();
        assert_eq!(responses.len(), 2);
        match &responses[0] {
            DecodedPdu {
                serial: 0,
                pdu: Pdu::GetPaneRenderChangesResponse(response),
            } => {
                assert_eq!(response.pane_id, pane.pane_id());
                assert_eq!(response.input_serial, Some(input_serial));
                assert_eq!(response.seqno, 11);
            }
            other => panic!("expected forced paste dispatch-fence response, got {other:?}"),
        }
        assert_eq!(
            responses[1],
            DecodedPdu {
                serial: 9_702,
                pdu: Pdu::UnitResponse(UnitResponse {}),
            }
        );
        assert_eq!(
            pane.paste_count(),
            1,
            "the paste must be applied exactly once"
        );
    }

    #[test]
    fn paste_ack_survives_post_input_render_range_failure_and_redirties() {
        let _global = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().dimensions.physical_top = StableRowIndex::MAX;
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_dyn)
            .expect("register paste ACK failure-path test pane");
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));

        handler.process_one(DecodedPdu {
            serial: 9_703,
            pdu: Pdu::SendPaste(SendPaste {
                pane_id: pane.pane_id(),
                data: "paste".to_string(),
                input_serial: InputSerial::now(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 9_703);
        assert_eq!(response.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert_eq!(
            pane.paste_count(),
            1,
            "the paste must be applied exactly once"
        );
        let per_pane = handler
            .per_pane_if_present(pane.pane_id())
            .expect("paste should retain per-pane state");
        let state = per_pane.lock().unwrap();
        assert_eq!(
            state.baseline,
            PaneRenderBaseline::default(),
            "failed post-paste render preparation must not advance its baseline"
        );
        assert!(
            state.transactional_dirty,
            "failed post-paste render preparation must retain a retry obligation"
        );
    }

    #[test]
    fn legacy_compute_changes_requeries_from_zero_after_sequence_saturation() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("initial pane surface preparation should succeed")
            .expect("initial pane snapshot should produce a response");
        assert_eq!(per_pane.baseline.seqno, 11);
        assert_eq!(pane.take_changed_since_seqnos(), vec![SEQ_ZERO]);

        pane.state.lock().unwrap().seqno = SequenceNo::MAX;
        pane.set_changed_line(0);
        registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("first saturated surface preparation should succeed")
            .expect("first saturated mutation should produce a response");
        assert_eq!(per_pane.baseline.seqno, SequenceNo::MAX);
        assert_eq!(pane.take_changed_since_seqnos(), vec![SEQ_ZERO]);

        pane.clear_changed_lines();
        pane.set_changed_line(1);
        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("subsequent saturated surface preparation should succeed")
            .expect("a later MAX-stamped mutation must not be hidden by a MAX baseline");
        assert_eq!(response.seqno, SequenceNo::MAX);
        assert_eq!(pane.take_changed_since_seqnos(), vec![SEQ_ZERO]);
        let (bonus_lines, _images) = response
            .bonus_lines
            .extract_data_checked()
            .expect("saturated retry lines should remain structurally valid");
        assert!(bonus_lines.iter().any(|(stable_row, _)| *stable_row == 1));
    }

    #[test]
    fn legacy_compute_changes_requeries_from_zero_after_source_regression() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("initial pane surface preparation should succeed")
            .expect("initial pane snapshot should produce a response");
        assert_eq!(per_pane.baseline.seqno, 11);
        assert_eq!(pane.take_changed_since_seqnos(), vec![SEQ_ZERO]);

        {
            let mut state = pane.state.lock().unwrap();
            state.seqno = 3;
        }
        pane.set_changed_line(1);
        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("regressed source preparation should succeed")
            .expect("a regressed sequence domain requires a full changed-row response");
        assert_eq!(response.seqno, 3);
        assert_eq!(per_pane.baseline.seqno, 3);
        assert_eq!(pane.take_changed_since_seqnos(), vec![SEQ_ZERO]);
        let (bonus_lines, _images) = response
            .bonus_lines
            .extract_data_checked()
            .expect("source-regression retry lines should remain structurally valid");
        assert!(bonus_lines.iter().any(|(stable_row, _)| *stable_row == 1));
    }

    #[test]
    fn compute_changes_detects_cleared_tiered_scrollback_status_without_other_deltas() {
        let pane = Arc::new(FakePane::new(Some(sample_tiered_scrollback_status(12))));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let initial = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed");
        assert!(
            initial.is_some(),
            "first snapshot should populate cached pane state"
        );
        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .expect("pane surface preparation should succeed")
                .is_none(),
            "unchanged pane state should not emit a redundant render delta"
        );

        pane.set_tiered_scrollback_status(None);

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("clearing tiered scrollback status should produce a response");

        assert_eq!(response.dirty_lines.len(), 0);
        assert_eq!(response.tiered_scrollback_status, None);
        assert_eq!(per_pane.baseline.tiered_scrollback_status, None);
    }

    #[test]
    fn legacy_compute_changes_advances_no_effect_sequence_baseline() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("initial pane snapshot should produce a response");
        assert_eq!(per_pane.baseline.seqno, 11);

        pane.state.lock().unwrap().seqno = 12;
        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .expect("pane surface preparation should succeed")
                .is_none(),
            "a sequence-only change has no legacy wire effect"
        );
        assert_eq!(
            per_pane.baseline.seqno, 12,
            "legacy delivery must not repeatedly rescan a settled no-effect interval"
        );
    }

    #[test]
    fn compute_changes_detects_alt_screen_transition_without_other_deltas() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .expect("pane surface preparation should succeed")
                .is_some(),
            "first snapshot should populate cached pane state"
        );
        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .expect("pane surface preparation should succeed")
                .is_none(),
            "unchanged pane state should not emit a redundant render delta"
        );

        pane.state.lock().unwrap().alt_screen_active = true;

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("alt-screen transition should produce a response");

        assert!(response.alt_screen_active);
        assert!(per_pane.baseline.alt_screen_active);
    }

    #[test]
    fn compute_changes_skips_missing_cursor_row_in_bonus_lines() {
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().cursor_position.y = 99;
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("initial pane snapshot should still produce a response");

        let cursor_y = response.cursor_position.y;
        let (bonus_lines, _images) = response.bonus_lines.extract_data();

        assert_eq!(bonus_lines.len(), 0);
        assert_eq!(cursor_y, 99);
    }

    #[test]
    fn compute_changes_deduplicates_dirty_cursor_row_in_bonus_lines() {
        let pane = Arc::new(FakePane::new(None));
        pane.set_changed_line(0);
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("dirty cursor row should produce a response");
        let (bonus_lines, _images) = response
            .bonus_lines
            .extract_data_checked()
            .expect("cursor row must appear exactly once");

        assert_eq!(bonus_lines.len(), 1);
        assert_eq!(bonus_lines[0].0, 0);
        assert_eq!(response.dirty_lines.len(), 0);
    }

    #[test]
    fn compute_changes_keeps_defensive_cursor_row_in_stable_order() {
        let pane = Arc::new(FakePane::new(None));
        {
            let mut state = pane.state.lock().unwrap();
            state.dimensions.physical_top = 1;
            state.cursor_position.y = 0;
        }
        pane.set_changed_line(1);
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("pane surface preparation should succeed")
            .expect("initial pane snapshot should produce a response");
        let (bonus_lines, _images) = response
            .bonus_lines
            .extract_data_checked()
            .expect("defensive cursor ordering must remain structurally valid");

        assert_eq!(
            bonus_lines
                .iter()
                .map(|(stable_row, _)| *stable_row)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "a cursor row before the viewport must be inserted, not appended"
        );
    }

    /// Regression: FakePane::writer() previously panicked via
    /// `unimplemented!()`. After br-ft-35yac.3 it returns a
    /// MappedMutexGuard over an `io::Sink`; bytes are discarded but
    /// the call no longer crashes.
    #[test]
    fn fake_pane_writer_returns_sink_instead_of_panicking() {
        let pane = FakePane::new(None);
        // Write through the trait object form to exercise the same
        // code path real callers use.
        let pane_dyn: &dyn Pane = &pane;
        {
            let mut writer = pane_dyn.writer();
            writer
                .write_all(b"discarded payload")
                .expect("io::sink never fails");
            writer.flush().expect("io::sink flush never fails");
        }
        // A second call must also succeed — the prior guard was
        // dropped above so the lock is released.
        {
            let mut writer = pane_dyn.writer();
            writer
                .write_all(b"second call")
                .expect("io::sink never fails");
        }
    }
}
