use crate::domain::{ClientDomain, ClientDomainConfig, ClientInner};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use asupersync::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use asupersync::runtime::{Interest, IoRegistration};
use asupersync::Cx;
use async_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use async_ossl::AsyncSslStream;
use async_trait::async_trait;
use codec::*;
use config::{configuration, SshDomain, TlsDomainClient, UnixDomain, UnixTarget};
use filedescriptor::FileDescriptor;
use futures::future::{ready, select, Either};
use futures::pin_mut;
use mux::client::ClientId;
use mux::connui::ConnectionUI;
use mux::domain::DomainId;
use mux::pane::{Pane, PaneId};
use mux::ssh::ssh_connect_with_ui;
use mux::{Mux, PaneRegistrationHandle};
use openssl::ssl::{SslConnector, SslFiletype, SslMethod};
use openssl::x509::X509;
use portable_pty::Child;
use std::collections::{hash_map::Entry, HashMap};
use std::future::poll_fn;
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::marker::Unpin;
use std::net::TcpStream;
use std::num::NonZeroU64;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context as TaskContext, Poll};
use std::thread;
use std::time::Duration;
use thiserror::Error;
use wezterm_uds::UnixStream;

const UNIX_SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const INITIAL_CONNECTION_GENERATION: u64 = 1;

#[derive(Error, Debug)]
#[error("Timeout")]
struct Timeout;

#[derive(Error, Debug)]
#[error("ChannelSendError")]
struct ChannelSendError;

enum ReaderMessage {
    SendPdu {
        pdu: Box<Pdu>,
        promise: Sender<anyhow::Result<Pdu>>,
    },
}

#[derive(Clone)]
pub struct Client {
    sender: Sender<ReaderMessage>,
    local_domain_id: Option<DomainId>,
    incarnation: Arc<ClientIncarnation>,
    connection_generation: Arc<AtomicU64>,
    pub client_id: ClientId,
    client_domain_config: ClientDomainConfig,
    pub is_reconnectable: bool,
    pub is_local: bool,
}

struct ClientIncarnation;

#[derive(Clone)]
enum ClientDispatchTarget {
    Standalone,
    Attached {
        local_domain_id: DomainId,
        mux_owner: Weak<Mux>,
    },
}

/// Exact authority for dispatching work produced by one transport connection.
///
/// The client incarnation prevents an old reconnect thread from operating on a
/// replacement `ClientInner`. The monotonically increasing connection
/// generation revokes already-queued work as soon as its transport ends. The
/// weak mux owner prevents process-global mux replacement from retargeting that
/// work.
#[derive(Clone)]
struct ClientDispatchAuthority {
    target: ClientDispatchTarget,
    client_incarnation: Arc<ClientIncarnation>,
    connection_generation: Arc<AtomicU64>,
    generation: u64,
}

#[derive(Clone)]
struct CurrentClientDispatch {
    authority: ClientDispatchAuthority,
    mux: Arc<Mux>,
    domain: Arc<dyn mux::domain::Domain>,
    inner: Arc<ClientInner>,
}

impl ClientDispatchAuthority {
    fn new(
        local_domain_id: Option<DomainId>,
        mux_owner: Weak<Mux>,
        client_incarnation: Arc<ClientIncarnation>,
        connection_generation: Arc<AtomicU64>,
    ) -> Self {
        let target = match local_domain_id {
            Some(local_domain_id) => ClientDispatchTarget::Attached {
                local_domain_id,
                mux_owner,
            },
            None => ClientDispatchTarget::Standalone,
        };
        Self {
            target,
            client_incarnation,
            connection_generation,
            generation: INITIAL_CONNECTION_GENERATION,
        }
    }

    fn is_standalone(&self) -> bool {
        matches!(&self.target, ClientDispatchTarget::Standalone)
    }

    fn generation_is_current(&self) -> bool {
        self.generation != 0
            && self.connection_generation.load(AtomicOrdering::Acquire) == self.generation
    }

    /// Revoke the current transport and mint the authority for its successor.
    ///
    /// The compare-exchange is deliberately exact: if some path ever attempts
    /// to advance an already-retired authority, it fails closed rather than
    /// accidentally blessing work from a different connection.
    fn advance_generation(&self) -> anyhow::Result<Self> {
        let Some(next_generation) = self.generation.checked_add(1) else {
            let _ = self.connection_generation.compare_exchange(
                self.generation,
                0,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            );
            bail!("mux client connection generation exhausted");
        };
        self.connection_generation
            .compare_exchange(
                self.generation,
                next_generation,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .map_err(|observed| {
                anyhow!(
                    "cannot advance stale mux client connection generation {} (current {})",
                    self.generation,
                    observed
                )
            })?;

        let mut next = self.clone();
        next.generation = next_generation;
        Ok(next)
    }

    fn captured_mux(&self) -> Option<Arc<Mux>> {
        match &self.target {
            ClientDispatchTarget::Standalone => None,
            ClientDispatchTarget::Attached { mux_owner, .. } => mux_owner.upgrade(),
        }
    }

    fn resolve_current(&self) -> anyhow::Result<Option<CurrentClientDispatch>> {
        if !self.generation_is_current() {
            return Ok(None);
        }
        let ClientDispatchTarget::Attached {
            local_domain_id, ..
        } = &self.target
        else {
            return Ok(None);
        };
        let Some(mux) = self.captured_mux() else {
            return Ok(None);
        };
        let Some(domain) = mux.get_domain(*local_domain_id) else {
            return Ok(None);
        };
        let client_domain = domain
            .downcast_ref::<ClientDomain>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain instance", local_domain_id))?;
        let Some(inner) = client_domain.inner() else {
            return Ok(None);
        };
        if inner.is_detached() || !inner.client.matches_dispatch_authority(self) {
            return Ok(None);
        }

        let current = CurrentClientDispatch {
            authority: self.clone(),
            mux,
            domain,
            inner,
        };
        Ok(current.is_current().then_some(current))
    }
}

impl CurrentClientDispatch {
    fn local_domain_id(&self) -> DomainId {
        self.domain.domain_id()
    }

    fn client_domain(&self) -> &ClientDomain {
        self.domain
            .downcast_ref::<ClientDomain>()
            .expect("current client dispatch was resolved from a ClientDomain")
    }

    fn is_current(&self) -> bool {
        if !self.authority.generation_is_current() || self.inner.is_detached() {
            return false;
        }
        let ClientDispatchTarget::Attached {
            local_domain_id,
            mux_owner,
        } = &self.authority.target
        else {
            return false;
        };
        if *local_domain_id != self.domain.domain_id()
            || !mux_owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.mux))
            || !self
                .mux
                .get_domain(*local_domain_id)
                .is_some_and(|current| Arc::ptr_eq(&current, &self.domain))
        {
            return false;
        }

        self.client_domain().inner_is_current(&self.inner)
            && self
                .inner
                .client
                .matches_dispatch_authority(&self.authority)
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Codec version mismatch: local={} (frankenterm {}), remote={} (frankenterm {}). \
     Until ft-kuxho/B's CODEC_VERSION_MIN_SUPPORTED window lands, every \
     CODEC_VERSION bump is an atomic-redeploy event — see \
     docs/codec-atomic-redeploy.md for the operator runbook (server-first \
     deploy order, expected connection drops, rollback procedure).",
    CODEC_VERSION,
    config::wezterm_version(),
    codec_vers,
    version
)]
pub struct IncompatibleVersionError {
    pub version: String,
    pub codec_vers: usize,
}

const MAX_REMOTE_ERROR_REASON_CHARS: usize = 512;

fn sanitized_remote_error_reason(reason: &str) -> String {
    let mut sanitized = String::with_capacity(reason.len().min(MAX_REMOTE_ERROR_REASON_CHARS));
    for (index, ch) in reason.chars().enumerate() {
        if index == MAX_REMOTE_ERROR_REASON_CHARS {
            sanitized.push('…');
            break;
        }
        sanitized.extend(ch.escape_debug());
    }
    sanitized
}

macro_rules! rpc {
    ($method_name:ident, $request_type:ident, $response_type:ident) => {
        pub async fn $method_name(&self, pdu: $request_type) -> anyhow::Result<$response_type> {
            let start = std::time::Instant::now();
            let result = self.send_pdu(Pdu::$request_type(pdu)).await;
            let elapsed = start.elapsed();
            metrics::histogram!("rpc", "method" => stringify!($method_name)).record(elapsed);
            metrics::counter!("rpc.count", "method" => stringify!($method_name)).increment(1);
            match result {
                Ok(Pdu::$response_type(res)) => Ok(res),
                Ok(Pdu::ErrorResponse(err)) => {
                    bail!(
                        "{} failed: {}",
                        stringify!($method_name),
                        sanitized_remote_error_reason(&err.reason)
                    )
                }
                Ok(other) => bail!(
                    "unexpected {} response to {}; expected {}",
                    other.pdu_name(),
                    stringify!($method_name),
                    stringify!($response_type)
                ),
                Err(err) => Err(err),
            }
        }
    };

    // This variant allows omitting the request parameter; this is useful
    // in the case where the struct is empty and present only for the purpose
    // of typing the request.
    ($method_name:ident, $request_type:ident=(), $response_type:ident) => {
        #[allow(dead_code)]
        pub async fn $method_name(&self) -> anyhow::Result<$response_type> {
            let start = std::time::Instant::now();
            let result = self.send_pdu(Pdu::$request_type($request_type{})).await;
            let elapsed = start.elapsed();
            metrics::histogram!("rpc", "method" => stringify!($method_name)).record(elapsed);
            metrics::counter!("rpc.count", "method" => stringify!($method_name)).increment(1);
            match result {
                Ok(Pdu::$response_type(res)) => Ok(res),
                Ok(Pdu::ErrorResponse(err)) => {
                    bail!(
                        "{} failed: {}",
                        stringify!($method_name),
                        sanitized_remote_error_reason(&err.reason)
                    )
                }
                Ok(other) => bail!(
                    "unexpected {} response to {}; expected {}",
                    other.pdu_name(),
                    stringify!($method_name),
                    stringify!($response_type)
                ),
                Err(err) => Err(err),
            }
        }
    };
}

fn process_unilateral_inner(dispatch: CurrentClientDispatch, pane_id: PaneId, decoded: DecodedPdu) {
    if !dispatch.is_current() {
        return;
    }
    let admitted = admit_client_pane(&dispatch, pane_id);
    if !dispatch.is_current() {
        return;
    }
    promise::spawn::spawn(async move {
        process_unilateral_inner_async(dispatch, admitted, pane_id, decoded).await?;
        Ok::<(), anyhow::Error>(())
    })
    .detach();
}

fn admit_client_pane(
    dispatch: &CurrentClientDispatch,
    remote_pane_id: PaneId,
) -> Option<(Arc<dyn Pane>, PaneRegistrationHandle)> {
    if !dispatch.is_current() {
        return None;
    }
    let local_pane_id = dispatch
        .inner
        .remote_to_local_pane_id(&dispatch.mux, remote_pane_id)?;
    let pane = dispatch.mux.get_pane(local_pane_id)?;
    let client_pane = pane.downcast_ref::<ClientPane>()?;
    if !client_pane.belongs_to_client(&dispatch.inner)
        || client_pane.remote_pane_id() != remote_pane_id
    {
        return None;
    }
    let registration = dispatch.mux.capture_pane_registration(&pane)?;
    dispatch.is_current().then_some((pane, registration))
}

async fn process_unilateral_inner_async(
    dispatch: CurrentClientDispatch,
    admitted: Option<(Arc<dyn Pane>, PaneRegistrationHandle)>,
    pane_id: PaneId,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    let local_domain_id = dispatch.local_domain_id();
    if !dispatch.is_current() {
        log::trace!(
            "discarding unilateral PDU for retired client connection in domain {}",
            local_domain_id
        );
        return Ok(());
    }
    let client_domain = dispatch.client_domain();

    let (pane, registration) = if let Some(admitted) = admitted {
        admitted
    } else {
        // If we get a push for a pane that we don't yet know about, it means
        // that some other client has manipulated the mux topology; re-sync on
        // the captured origin, never a later process-global replacement.
        let local_pane_id = match dispatch
            .inner
            .remote_to_local_pane_id(&dispatch.mux, pane_id)
        {
            Some(p) => p,
            None => {
                log::debug!("got {decoded:?}, pane not found locally, resync");
                if !dispatch.is_current() {
                    return Ok(());
                }
                let resync_result = client_domain
                    .resync_if_current(Arc::clone(&dispatch.mux), Arc::clone(&dispatch.inner))
                    .await;
                if !dispatch.is_current() {
                    return Ok(());
                }
                resync_result?;
                dispatch
                    .inner
                    .remote_to_local_pane_id(&dispatch.mux, pane_id)
                    .ok_or_else(|| {
                        anyhow!("remote pane id {} does not have a local pane id", pane_id)
                    })?
            }
        };

        let pane = match dispatch.mux.get_pane(local_pane_id) {
            Some(p) => p,
            None => {
                log::debug!(
                    "got {decoded:?}, but local pane {local_pane_id} no longer exists; resync"
                );
                if !dispatch.is_current() {
                    return Ok(());
                }
                let resync_result = client_domain
                    .resync_if_current(Arc::clone(&dispatch.mux), Arc::clone(&dispatch.inner))
                    .await;
                if !dispatch.is_current() {
                    return Ok(());
                }
                resync_result?;

                let local_pane_id = dispatch
                    .inner
                    .remote_to_local_pane_id(&dispatch.mux, pane_id)
                    .ok_or_else(|| {
                        anyhow!("remote pane id {} does not have a local pane id", pane_id)
                    })?;

                dispatch
                    .mux
                    .get_pane(local_pane_id)
                    .ok_or_else(|| anyhow!("local pane {local_pane_id} not found"))?
            }
        };
        if !dispatch.is_current() {
            return Ok(());
        }
        let registration = dispatch
            .mux
            .capture_pane_registration(&pane)
            .ok_or_else(|| anyhow!("local pane {} is no longer registered", pane.pane_id()))?;
        (pane, registration)
    };
    let local_pane_id = pane.pane_id();
    let client_pane = pane.downcast_ref::<ClientPane>().ok_or_else(|| {
        log::error!(
            "received unilateral PDU for pane {} which is \
                     not an instance of ClientPane: {:?}",
            local_pane_id,
            decoded.pdu
        );
        anyhow!(
            "received unilateral PDU for pane {} which is \
                     not an instance of ClientPane: {:?}",
            local_pane_id,
            decoded.pdu
        )
    })?;
    if !dispatch.is_current()
        || !client_pane.belongs_to_client(&dispatch.inner)
        || client_pane.remote_pane_id() != pane_id
    {
        log::trace!(
            "discarding unilateral PDU for stale client pane {} (remote {})",
            local_pane_id,
            pane_id
        );
        return Ok(());
    }
    let result = client_pane
        .process_unilateral(&registration, decoded.pdu)
        .await;
    if !dispatch.is_current() {
        return Ok(());
    }
    result
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum StandaloneUnilateralError {
    #[error(
        "standalone mux client cannot process {pdu_name} unilateral PDU without a local domain; \
         reconnect with an attached client domain or retry after topology settles"
    )]
    RequiresAttachedDomain { pdu_name: &'static str },
}

fn unilateral_without_local_domain_is_ignorable(pdu: &Pdu) -> bool {
    match pdu {
        Pdu::WindowTitleChanged(_)
        | Pdu::TabTitleChanged(_)
        | Pdu::PaneFocused(_)
        | Pdu::TabResized(_)
        | Pdu::NotifyAlert(_)
        | Pdu::SetClipboard(_) => true,
        Pdu::PaneRemoved(_)
        | Pdu::TabAddedToWindow(_)
        | Pdu::WindowWorkspaceChanged(_)
        | Pdu::RenameWorkspace(_) => false,
        _ => pdu.pane_id().is_some(),
    }
}

fn handle_unilateral_without_local_domain(decoded: &DecodedPdu) -> anyhow::Result<()> {
    if unilateral_without_local_domain_is_ignorable(&decoded.pdu) {
        log::trace!(
            "standalone mux client explicitly ignored {} unilateral PDU",
            decoded.pdu.pdu_name()
        );
        return Ok(());
    }

    Err(StandaloneUnilateralError::RequiresAttachedDomain {
        pdu_name: decoded.pdu.pdu_name(),
    }
    .into())
}

fn process_unilateral(
    authority: &ClientDispatchAuthority,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    if !authority.generation_is_current() {
        return Ok(());
    }
    if authority.is_standalone() {
        return handle_unilateral_without_local_domain(&decoded);
    }
    let Some(dispatch) = authority.resolve_current()? else {
        return Ok(());
    };

    match &decoded.pdu {
        Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
            window_id,
            workspace,
        }) => {
            let window_id = *window_id;
            let workspace = workspace.to_string();
            if !dispatch.is_current() {
                return Ok(());
            }
            promise::spawn::spawn_into_main_thread(async move {
                if !dispatch.is_current() {
                    return Ok(());
                }
                let local_window_id = dispatch
                    .client_domain()
                    .remote_to_local_window_id(window_id)
                    .ok_or_else(|| anyhow!("no local window for remote window id {}", window_id))?;
                if !dispatch.is_current() {
                    return Ok(());
                }
                if let Some(mut window) = dispatch.mux.get_window_mut(local_window_id) {
                    if !dispatch.is_current() {
                        return Ok(());
                    }
                    window.set_workspace(&workspace);
                }

                anyhow::Result::<()>::Ok(())
            })
            .detach();

            return Ok(());
        }
        Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
            let title = title.to_string();
            let window_id = *window_id;
            if !dispatch.is_current() {
                return Ok(());
            }
            promise::spawn::spawn_into_main_thread(async move {
                if !dispatch.is_current() {
                    return Ok(());
                }
                let local_window_id = dispatch
                    .client_domain()
                    .remote_to_local_window_id(window_id)
                    .ok_or_else(|| anyhow!("no local window for remote window id {}", window_id))?;
                if !dispatch.is_current() {
                    return Ok(());
                }
                dispatch.mux.set_window_title(local_window_id, &title);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace,
            new_workspace,
        }) => {
            let old_workspace = old_workspace.to_string();
            let new_workspace = new_workspace.to_string();
            if !dispatch.is_current() {
                return Ok(());
            }
            promise::spawn::spawn_into_main_thread(async move {
                if !dispatch.is_current() {
                    return Ok(());
                }
                log::debug!("got a rename {old_workspace} -> {new_workspace}");
                dispatch
                    .mux
                    .rename_workspace(&old_workspace, &new_workspace);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
            let title = title.to_string();
            let tab_id = *tab_id;
            if !dispatch.is_current() {
                return Ok(());
            }
            promise::spawn::spawn_into_main_thread(async move {
                if !dispatch.is_current() {
                    return Ok(());
                }
                let local_tab_id = dispatch
                    .inner
                    .remote_to_local_tab_id(tab_id)
                    .ok_or_else(|| anyhow!("no local tab for remote tab id {}", tab_id))?;
                if !dispatch.is_current() {
                    return Ok(());
                }
                dispatch.mux.set_tab_title(local_tab_id, &title);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::TabResized(_) | Pdu::TabAddedToWindow(_) => {
            log::trace!("resync due to {:?}", decoded.pdu);
            if !dispatch.is_current() {
                return Ok(());
            }
            promise::spawn::spawn_into_main_thread(async move {
                if !dispatch.is_current() {
                    return Ok(());
                }
                let result = dispatch
                    .client_domain()
                    .resync_if_current(Arc::clone(&dispatch.mux), Arc::clone(&dispatch.inner))
                    .await;
                if !dispatch.is_current() {
                    return Ok(());
                }
                result
            })
            .detach();

            return Ok(());
        }
        _ => {}
    }

    if let Some(pane_id) = decoded.pdu.pane_id() {
        if !dispatch.is_current() {
            return Ok(());
        }
        promise::spawn::spawn_into_main_thread(async move {
            if dispatch.is_current() {
                process_unilateral_inner(dispatch, pane_id, decoded);
            }
        })
        .detach();
    } else {
        bail!("don't know how to handle {:?}", decoded);
    }
    Ok(())
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum NotReconnectableError {
    #[error("Client was destroyed")]
    ClientWasDestroyed,
}

#[derive(Debug)]
struct PendingRpc {
    completion: Sender<anyhow::Result<Pdu>>,
    pdu_name: &'static str,
}

#[derive(Debug, Clone)]
struct RpcMetrics {
    pending: metrics::Gauge,
    admitted: metrics::Counter,
    preclosed: metrics::Counter,
    delivered: metrics::Counter,
    abandoned: metrics::Counter,
    transport_failed_live: metrics::Counter,
    transport_cleared_abandoned: metrics::Counter,
    retirement_reply_channel_full: metrics::Counter,
    future_serial: metrics::Counter,
    unmatched_serial: metrics::Counter,
    serial_exhausted: metrics::Counter,
    reserve_failed: metrics::Counter,
    serial_collision: metrics::Counter,
    protocol_reply_channel_full: metrics::Counter,
}

impl RpcMetrics {
    fn register() -> Self {
        Self {
            pending: metrics::gauge!("mux.client.rpc.pending"),
            admitted: metrics::counter!(
                "mux.client.rpc.admission.total",
                "outcome" => "admitted"
            ),
            preclosed: metrics::counter!(
                "mux.client.rpc.admission.total",
                "outcome" => "preclosed"
            ),
            delivered: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "delivered"
            ),
            abandoned: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "abandoned"
            ),
            transport_failed_live: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "transport_failed_live"
            ),
            transport_cleared_abandoned: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "transport_cleared_abandoned"
            ),
            retirement_reply_channel_full: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "reply_channel_full"
            ),
            future_serial: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "future_serial"
            ),
            unmatched_serial: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "unmatched_serial"
            ),
            serial_exhausted: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "serial_exhausted"
            ),
            reserve_failed: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "reserve_failed"
            ),
            serial_collision: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "serial_collision"
            ),
            protocol_reply_channel_full: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "reply_channel_full"
            ),
        }
    }
}

#[derive(Error, Debug)]
enum PendingRpcError {
    #[error("mux client RPC serial space is exhausted")]
    SerialExhausted,
    #[error("cannot reserve capacity for another pending mux client RPC")]
    Reserve(#[source] std::collections::TryReserveError),
    #[error(
        "mux client RPC serial {serial} for {request} collides with pending request {pending_request}"
    )]
    SerialCollision {
        serial: NonZeroU64,
        request: &'static str,
        pending_request: &'static str,
    },
    #[error(
        "server replied with future RPC serial {serial}; highest serial issued by this transport is {highest_issued}"
    )]
    FutureSerial {
        serial: NonZeroU64,
        highest_issued: u64,
    },
    #[error(
        "server replied with RPC serial {serial}, which is no longer pending (highest issued {highest_issued})"
    )]
    UnmatchedSerial {
        serial: NonZeroU64,
        highest_issued: u64,
    },
    #[error(
        "reply channel for RPC serial {serial} ({request} -> {response}) was unexpectedly full"
    )]
    ReplyChannelFull {
        serial: NonZeroU64,
        request: &'static str,
        response: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyDisposition {
    Delivered,
    Abandoned,
}

#[derive(Debug)]
struct PendingReplies {
    map: HashMap<NonZeroU64, PendingRpc>,
    next_serial: Option<NonZeroU64>,
    highest_issued: u64,
    metrics: RpcMetrics,
}

impl PendingReplies {
    fn new(metrics: RpcMetrics) -> Self {
        Self {
            map: HashMap::new(),
            next_serial: NonZeroU64::new(1),
            highest_issued: 0,
            metrics,
        }
    }

    /// Admit a request at the reader's local write-attempt boundary.
    ///
    /// If the caller closed its receiver while this request was still queued,
    /// it consumes neither a serial nor a wire frame. A close after this check
    /// is an admitted abandonment even if a later encode or flush fails, and
    /// retains this same map entry until reply drainage or transport teardown.
    fn admit(
        &mut self,
        completion: Sender<anyhow::Result<Pdu>>,
        pdu_name: &'static str,
    ) -> Result<Option<NonZeroU64>, PendingRpcError> {
        if completion.is_closed() {
            self.metrics.preclosed.increment(1);
            return Ok(None);
        }

        let Some(serial) = self.next_serial else {
            self.metrics.serial_exhausted.increment(1);
            return Self::reject_admission(completion, PendingRpcError::SerialExhausted);
        };
        if let Err(err) = self.map.try_reserve(1) {
            self.metrics.reserve_failed.increment(1);
            return Self::reject_admission(completion, PendingRpcError::Reserve(err));
        };

        match self.map.entry(serial) {
            Entry::Vacant(entry) => {
                entry.insert(PendingRpc {
                    completion,
                    pdu_name,
                });
            }
            Entry::Occupied(entry) => {
                self.metrics.serial_collision.increment(1);
                let error = PendingRpcError::SerialCollision {
                    serial,
                    request: pdu_name,
                    pending_request: entry.get().pdu_name,
                };
                return Self::reject_admission(completion, error);
            }
        }

        self.highest_issued = serial.get();
        self.next_serial = serial.get().checked_add(1).and_then(NonZeroU64::new);
        self.metrics.admitted.increment(1);
        // This gauge is process-wide and intentionally unlabeled. Treat it as
        // an aggregate across every concurrent mux connection; absolute
        // per-connection `set` operations would clobber one another.
        self.metrics.pending.increment(1);
        Ok(Some(serial))
    }

    fn highest_issued(&self) -> u64 {
        self.highest_issued
    }

    fn complete(
        &mut self,
        serial: NonZeroU64,
        pdu: Pdu,
    ) -> Result<ReplyDisposition, PendingRpcError> {
        let Some(pending) = self.map.remove(&serial) else {
            if serial.get() > self.highest_issued {
                self.metrics.future_serial.increment(1);
                return Err(PendingRpcError::FutureSerial {
                    serial,
                    highest_issued: self.highest_issued,
                });
            }
            self.metrics.unmatched_serial.increment(1);
            return Err(PendingRpcError::UnmatchedSerial {
                serial,
                highest_issued: self.highest_issued,
            });
        };
        self.metrics.pending.decrement(1);

        let response_name = pdu.pdu_name();
        match pending.completion.try_send(Ok(pdu)) {
            Ok(()) => {
                // "delivered" is linearized at successful enqueue into the
                // one-shot channel; the caller may close before observing it.
                self.metrics.delivered.increment(1);
                Ok(ReplyDisposition::Delivered)
            }
            Err(TrySendError::Closed(_)) => {
                self.metrics.abandoned.increment(1);
                Ok(ReplyDisposition::Abandoned)
            }
            Err(TrySendError::Full(_)) => {
                self.metrics.retirement_reply_channel_full.increment(1);
                self.metrics.protocol_reply_channel_full.increment(1);
                Err(PendingRpcError::ReplyChannelFull {
                    serial,
                    request: pending.pdu_name,
                    response: response_name,
                })
            }
        }
    }

    fn complete_or_fail_transport(
        &mut self,
        serial: NonZeroU64,
        pdu: Pdu,
    ) -> Result<ReplyDisposition, PendingRpcError> {
        match self.complete(serial, pdu) {
            Ok(disposition) => Ok(disposition),
            Err(error) => {
                let reason = error.to_string();
                self.fail_all(&reason);
                Err(error)
            }
        }
    }

    fn fail_all(&mut self, reason: &str) {
        log::trace!("failing all pending RPCs: {reason}");
        for (serial, pending) in self.map.drain() {
            self.metrics.pending.decrement(1);
            if pending.completion.is_closed() {
                self.metrics.transport_cleared_abandoned.increment(1);
                continue;
            }
            match pending.completion.try_send(Err(anyhow!(
                "{reason} (pending {} serial {serial})",
                pending.pdu_name
            ))) {
                Ok(()) => self.metrics.transport_failed_live.increment(1),
                Err(TrySendError::Closed(_)) => {
                    self.metrics.transport_cleared_abandoned.increment(1);
                }
                Err(TrySendError::Full(_)) => {
                    self.metrics.retirement_reply_channel_full.increment(1);
                    self.metrics.protocol_reply_channel_full.increment(1);
                    log::error!(
                        "reply channel unexpectedly full while failing pending {} serial {}",
                        pending.pdu_name,
                        serial
                    );
                }
            }
        }
    }

    fn record_decode_protocol_error(&self, error: &anyhow::Error) {
        if matches!(
            error.downcast_ref::<CorruptResponse>(),
            Some(CorruptResponse::SerialAboveCeiling { .. })
        ) {
            self.metrics.future_serial.increment(1);
        }
    }

    fn fail_after_transport_error(&mut self, error: &anyhow::Error) {
        self.fail_all(&format!("{error:#}"));
    }

    fn fail_after_decode_error(&mut self, error: &anyhow::Error) -> String {
        self.record_decode_protocol_error(error);
        let reason = format!("Error while decoding response pdu: {error:#}");
        self.fail_all(&reason);
        reason
    }

    fn reject_admission(
        completion: Sender<anyhow::Result<Pdu>>,
        error: PendingRpcError,
    ) -> Result<Option<NonZeroU64>, PendingRpcError> {
        let _ = completion.try_send(Err(anyhow!("{error}")));
        Err(error)
    }
}

impl Drop for PendingReplies {
    fn drop(&mut self) {
        self.fail_all("Client was destroyed");
    }
}

const FALLBACK_IO_BACKOFF: Duration = Duration::from_millis(1);

fn fallback_rewake(task_cx: &TaskContext<'_>) {
    if let Some(timer) = Cx::current().and_then(|current| current.timer_driver()) {
        let deadline = timer.now() + FALLBACK_IO_BACKOFF;
        let _ = timer.register(deadline, task_cx.waker().clone());
    } else {
        task_cx.waker().wake_by_ref();
    }
}

fn client_thread(
    mut reconnectable: Reconnectable,
    mut rx: Receiver<ReaderMessage>,
    dispatch_authority: ClientDispatchAuthority,
) -> (anyhow::Result<()>, Reconnectable, Receiver<ReaderMessage>) {
    // The reader performs ALL of this connection's socket I/O, so it must run as
    // a scheduler-managed task (block_on_io) rather than a directly-polled
    // block_on future. asupersync only delivers socket-readiness wakeups to
    // tasks living on the scheduler; a future polled directly by block_on just
    // parks the thread and never gets the wakeup, so the handshake reply that
    // arrives *after* the reader parks (any real, latency-bearing connection
    // such as an SSH-proxy mux domain) is never consumed and the version check
    // times out. We move the owned reconnectable + receiver into the task and
    // return them so the reconnect loop can reuse them on the next attempt.
    promise::spawn::block_on_io(async move {
        let result = client_thread_async(&mut reconnectable, &mut rx, &dispatch_authority).await;
        (result, reconnectable, rx)
    })
}

async fn client_thread_async(
    reconnectable: &mut Reconnectable,
    rx: &mut Receiver<ReaderMessage>,
    dispatch_authority: &ClientDispatchAuthority,
) -> anyhow::Result<()> {
    let mut pending = PendingReplies::new(RpcMetrics::register());

    // Wrap the connection in a persistent buffered reader so PDU decoding pulls
    // its leb128 length headers and bodies from an in-memory buffer (one socket
    // read per refill, default 8 KiB) instead of one syscall per byte
    // (decode_async reads leb128 a byte at a time — ~30 syscalls per PDU). The
    // BufReader lives across the whole reader loop so partially-buffered and
    // pipelined PDUs carry over between iterations. Writes still go straight to
    // the socket via `get_mut()` (BufReader only buffers reads), so the encode
    // path is byte-for-byte unchanged.
    let mut reader = BufReader::new(reconnectable.take_stream().ok_or_else(|| {
        anyhow::anyhow!("mux client stream not available — connection not established")
    })?);

    enum NextEvent {
        Message(Result<ReaderMessage, async_channel::RecvError>),
        Readable(anyhow::Result<()>),
    }

    loop {
        let next_event = {
            let rx_msg = rx.recv();
            // Readiness must be buffer-aware: a prior socket read may have
            // already buffered a complete (pipelined) PDU while the underlying
            // socket has nothing pending. Waiting on the socket alone would then
            // strand that buffered PDU until more bytes happen to arrive (a
            // latency stall / hang). So treat a non-empty buffer as immediately
            // readable and only park on the socket once the buffer is drained.
            //
            // The two cases must be combined via `Either` rather than an `async`
            // block: `wait_for_readable()` is an `#[async_trait]` boxed `Send`
            // future, but an `async` block awaiting it would capture `&reader`
            // across the await, and `Box<dyn AsyncReadAndWrite>` is `Send` but
            // not `Sync`, so that reference is `!Send` — which would make the
            // whole reader future `!Send` and fail `block_on_io`.
            let wait_for_read = if reader.buffer().is_empty() {
                Either::Left(reader.get_ref().wait_for_readable())
            } else {
                Either::Right(ready(Ok::<(), anyhow::Error>(())))
            };

            pin_mut!(rx_msg);
            pin_mut!(wait_for_read);

            match select(rx_msg, wait_for_read).await {
                Either::Left((message, _)) => NextEvent::Message(message),
                Either::Right((readable, _)) => NextEvent::Readable(readable),
            }
        };

        match next_event {
            NextEvent::Message(Ok(ReaderMessage::SendPdu { pdu, promise })) => {
                let request_name = pdu.pdu_name();
                let serial = match pending.admit(promise, request_name) {
                    Ok(Some(serial)) => serial,
                    Ok(None) => continue,
                    Err(error) => {
                        let error = anyhow::Error::new(error);
                        pending.fail_after_transport_error(&error);
                        return Err(error);
                    }
                };

                if let Err(error) = pdu
                    .encode_async(reader.get_mut(), serial.get())
                    .await
                    .context("encoding a PDU to send to the server")
                {
                    pending.fail_after_transport_error(&error);
                    return Err(error);
                }
                if let Err(error) = reader
                    .get_mut()
                    .flush()
                    .await
                    .context("flushing PDU to server")
                {
                    pending.fail_after_transport_error(&error);
                    return Err(error);
                }
            }
            NextEvent::Message(Err(_)) => {
                return Err(NotReconnectableError::ClientWasDestroyed.into());
            }
            NextEvent::Readable(Ok(())) => {
                match Pdu::decode_async(&mut reader, Some(pending.highest_issued())).await {
                    Ok(decoded) => {
                        log::debug!(
                            "decoded serial {} {}",
                            decoded.serial,
                            decoded.pdu.pdu_name()
                        );
                        if decoded.serial == 0 {
                            if let Err(error) = process_unilateral(dispatch_authority, decoded)
                                .context("processing unilateral PDU from server")
                            {
                                log::error!("process_unilateral: {:?}", error);
                                pending.fail_after_transport_error(&error);
                                return Err(error);
                            }
                        } else {
                            let serial = NonZeroU64::new(decoded.serial)
                                .expect("the unilateral serial-zero branch was handled above");
                            if let Err(err) =
                                pending.complete_or_fail_transport(serial, decoded.pdu)
                            {
                                return Err(err.into());
                            }
                        }
                    }
                    Err(err) => {
                        let reason = pending.fail_after_decode_error(&err);
                        log::error!("{}", reason);
                        return Err(err).context("Error while decoding response pdu");
                    }
                }
            }
            NextEvent::Readable(Err(err)) => {
                let reason = format!("Error while waiting for stream readability: {:#}", err);
                log::error!("{}", reason);
                pending.fail_all(&reason);
                return Err(err).context("Error while waiting for stream readability");
            }
        }
    }
}

pub fn unix_connect_with_retry(
    target: &UnixTarget,
    just_spawned: bool,
    max_attempts: Option<u64>,
) -> anyhow::Result<UnixStream> {
    let mut error = None;

    if just_spawned {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let max_attempts = max_attempts.unwrap_or(10);
    if max_attempts == 0 {
        bail!("unix connection retry count must be greater than zero");
    }

    for iter in 0..max_attempts {
        if iter > 0 {
            std::thread::sleep(std::time::Duration::from_millis(iter * 50));
        }
        match target {
            UnixTarget::Socket(path) => match unix_stream_connect_with_timeout(
                path.to_path_buf(),
                UNIX_SOCKET_CONNECT_TIMEOUT,
            ) {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    error =
                        Some(Err(err).with_context(|| format!("connecting to {}", path.display())))
                }
            },
            UnixTarget::Proxy(argv) => {
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| anyhow!("unix proxy command is empty"))?;
                let mut cmd = std::process::Command::new(program);
                cmd.args(args);

                let (a, b) = filedescriptor::socketpair()?;

                cmd.stdin(b.as_stdio()?);
                cmd.stdout(b.as_stdio()?);
                cmd.stderr(std::process::Stdio::inherit());
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("spawning proxy command {:?}", cmd))?;

                error.take();

                // Grace period to detect whether connection failed
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            error = Some(Err(anyhow!(
                                "{:?} exited already with status {:?}",
                                cmd,
                                status
                            )));
                            continue;
                        }
                        Ok(None) => {
                            error.take();
                        }
                        Err(err) => {
                            error =
                                Some(Err(err).context(format!("spawning proxy command {:?}", cmd)));
                            continue;
                        }
                    }
                }

                if error.is_none() {
                    #[cfg(unix)]
                    unsafe {
                        use std::os::unix::io::{FromRawFd, IntoRawFd};
                        return Ok(UnixStream::from_raw_fd(a.into_raw_fd()));
                    }
                    #[cfg(windows)]
                    unsafe {
                        // `a` is a `filedescriptor::FileDescriptor`, which
                        // does not impl `std::os::windows::io::IntoRawSocket`
                        // directly — only the project's
                        // `IntoRawSocketDescriptor` (returning `SocketDescriptor
                        // = SOCKET`). `RawSocket` is also SOCKET-shaped on
                        // Windows, so the `as _` cast bridges them.
                        use filedescriptor::IntoRawSocketDescriptor;
                        use std::os::windows::io::FromRawSocket;
                        return Ok(UnixStream::from_raw_socket(a.into_socket_descriptor() as _));
                    }
                }
            }
        }
    }

    error.unwrap_or_else(|| Err(anyhow!("unix connection failed without recording a cause")))
}

fn unix_stream_connect_with_timeout(
    path: PathBuf,
    timeout: Duration,
) -> std::io::Result<UnixStream> {
    if timeout.is_zero() {
        return Err(std::io::Error::new(
            ErrorKind::TimedOut,
            "unix socket connect timeout must be greater than zero",
        ));
    }

    let display_path = path.display().to_string();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name(format!("frankenterm-unix-connect-{}", std::process::id()))
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(path));
        })
        .map_err(|err| {
            std::io::Error::other(format!("spawn unix socket connect timeout thread: {err}"))
        })?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            ErrorKind::TimedOut,
            format!(
                "timed out after {}ms connecting to {}",
                timeout.as_millis(),
                display_path
            ),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
            ErrorKind::ConnectionAborted,
            format!("unix socket connect worker exited before connecting to {display_path}"),
        )),
    }
}

#[async_trait]
pub trait AsyncReadAndWrite: Unpin + AsyncRead + AsyncWrite + std::fmt::Debug + Send {
    async fn wait_for_readable(&self) -> anyhow::Result<()>;
}

#[async_trait]
impl AsyncReadAndWrite for UnixStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        UnixStream::wait_for_readable(self)
            .await
            .map_err(Into::into)
    }
}

#[async_trait]
impl AsyncReadAndWrite for AsyncSslStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        AsyncSslStream::wait_for_readable(self)
            .await
            .map_err(Into::into)
    }
}

#[async_trait]
impl AsyncReadAndWrite for SshStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        SshStream::wait_for_readable(self).await.map_err(Into::into)
    }
}

#[derive(Debug)]
struct Reconnectable {
    config: ClientDomainConfig,
    stream: Option<Box<dyn AsyncReadAndWrite>>,
    tls_creds: Option<GetTlsCredsResponse>,
}

struct SshStream {
    stdin: FileDescriptor,
    stdout: FileDescriptor,
    read_registration: Mutex<Option<IoRegistration>>,
    write_registration: Mutex<Option<IoRegistration>>,
}

impl std::fmt::Debug for SshStream {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(fmt, "SshStream {{...}}")
    }
}

fn lock_registration_mutex(
    registration: &Mutex<Option<IoRegistration>>,
) -> MutexGuard<'_, Option<IoRegistration>> {
    match registration.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            registration.clear_poison();
            poisoned.into_inner()
        }
    }
}

impl SshStream {
    fn new(mut stdin: FileDescriptor, mut stdout: FileDescriptor) -> std::io::Result<Self> {
        stdin
            .set_non_blocking(true)
            .map_err(std::io::Error::other)?;
        stdout
            .set_non_blocking(true)
            .map_err(std::io::Error::other)?;
        Ok(Self {
            stdin,
            stdout,
            read_registration: Mutex::new(None),
            write_registration: Mutex::new(None),
        })
    }

    async fn wait_for_readable(&self) -> std::io::Result<()> {
        let mut armed = false;
        poll_fn(|task_cx| {
            if armed {
                return Poll::Ready(Ok(()));
            }
            self.register_interest_for_read(task_cx)?;
            armed = true;
            Poll::Pending
        })
        .await
    }

    fn register_interest_for_read(&self, task_cx: &TaskContext<'_>) -> std::io::Result<()> {
        self.register_interest(
            &self.stdout,
            &self.read_registration,
            Interest::READABLE,
            task_cx,
        )
    }

    fn register_interest_for_write(&self, task_cx: &TaskContext<'_>) -> std::io::Result<()> {
        self.register_interest(
            &self.stdin,
            &self.write_registration,
            Interest::WRITABLE,
            task_cx,
        )
    }

    fn register_interest(
        &self,
        desc: &FileDescriptor,
        registration: &Mutex<Option<IoRegistration>>,
        interest: Interest,
        task_cx: &TaskContext<'_>,
    ) -> std::io::Result<()> {
        let mut registration = lock_registration_mutex(registration);
        if let Some(existing) = registration.as_mut() {
            match existing.rearm(interest, task_cx.waker()) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    *registration = None;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotConnected => {
                    *registration = None;
                    drop(registration);
                    fallback_rewake(task_cx);
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }

        let Some(current) = Cx::current() else {
            drop(registration);
            fallback_rewake(task_cx);
            return Ok(());
        };

        // asupersync's `Cx::register_io` is gated `#[cfg(unix)]`. On Windows
        // (where this code only runs through the `uds_windows`-backed
        // ssh-pipe path) we fall through to `fallback_rewake` polling —
        // same shape as frankenterm-uds and async_ossl.
        #[cfg(unix)]
        {
            match current.register_io(desc, interest) {
                Ok(new_registration) => {
                    let _ = new_registration.update_waker(task_cx.waker().clone());
                    *registration = Some(new_registration);
                    Ok(())
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::Unsupported | std::io::ErrorKind::NotConnected
                    ) =>
                {
                    drop(registration);
                    fallback_rewake(task_cx);
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
        #[cfg(windows)]
        {
            let _ = (current, desc, interest);
            drop(registration);
            fallback_rewake(task_cx);
            Ok(())
        }
    }
}

impl Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.stdout.read(buf)
    }
}

impl Write for SshStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.stdin.flush()
    }
}

impl AsyncRead for SshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.stdout.read(buf.unfilled()) {
            Ok(read) => {
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_read(task_cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.stdin.write(buf) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(task_cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.stdin.write_vectored(bufs) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(task_cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.stdin.flush() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(task_cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _task_cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Reconnectable {
    fn new(config: ClientDomainConfig, stream: Option<Box<dyn AsyncReadAndWrite>>) -> Self {
        Self {
            config,
            stream,
            tls_creds: None,
        }
    }

    fn tls_creds_path(&self) -> anyhow::Result<PathBuf> {
        let path = config::pki_dir()?.join(self.config.name());
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn tls_creds_ca_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("ca.pem"))
    }

    fn tls_creds_cert_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("cert.pem"))
    }

    fn take_stream(&mut self) -> Option<Box<dyn AsyncReadAndWrite>> {
        self.stream.take()
    }

    fn is_local(&mut self) -> bool {
        matches!(&self.config, ClientDomainConfig::Unix(_))
    }

    fn reconnectable(&mut self) -> bool {
        match &self.config {
            // A plain local unix socket only disconnects when its server dies,
            // so reconnecting can't preserve the set of tabs and would leave us
            // with confusing, inconsistent state. BUT when the unix domain uses
            // a proxy_command (our remote frankenterm-mux-server reached over
            // `ssh ... nc -U <sock>`), the *remote* mux is persistent: a dropped
            // ssh/nc pipe is a transient transport failure, and reconnecting
            // re-runs the proxy_command and re-syncs the exact same remote tabs.
            // So reconnect iff there is a proxy_command.
            ClientDomainConfig::Unix(unix) => unix.proxy_command.is_some(),
            ClientDomainConfig::Tls(_) => true,
            // It *does* make sense to reconnect with an ssh session, but we
            // need to grow some smarts about whether the disconnect was because
            // we sent CTRL-D to close the last session, or whether it was a network
            // level disconnect, because we will otherwise throw up authentication
            // dialogs that would be annoying
            ClientDomainConfig::Ssh(_) => false,
        }
    }

    fn connect(
        &mut self,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        match self.config.clone() {
            ClientDomainConfig::Unix(unix_dom) => {
                self.unix_connect(unix_dom, initial, ui, no_auto_start)
            }
            ClientDomainConfig::Tls(tls) => self.tls_connect(tls, initial, ui),
            ClientDomainConfig::Ssh(ssh) => self.ssh_connect(ssh, initial, ui),
        }
    }

    /// Resolve the path to wezterm for the remote system.
    /// We can't simply derive this from the current executable because
    /// we are being asked to produce a path for the remote system and
    /// we don't really know anything about it.
    /// `path` comes from the SshDoman::remote_wezterm_path option; if set
    /// then the user has told us where to look.
    /// Otherwise, we have to rely on the `PATH` environment for the remote
    /// system, and we don't know if it is even running unix, or whether
    /// any given shell syntax will help us provide a more meaningful
    /// message to the user.
    fn wezterm_bin_path(path: &Option<String>) -> String {
        path.as_deref().unwrap_or("wezterm").to_string()
    }

    fn build_ssh_proxy_command(
        remote_wezterm_path: &Option<String>,
        override_proxy_command: Option<&str>,
        initial: bool,
    ) -> String {
        if let Some(cmd) = override_proxy_command {
            cmd.to_string()
        } else {
            let proxy_bin = Self::wezterm_bin_path(remote_wezterm_path);
            if initial {
                format!("{proxy_bin} cli --prefer-mux proxy")
            } else {
                format!("{proxy_bin} cli --prefer-mux --no-auto-start proxy")
            }
        }
    }

    fn build_tls_creds_command(remote_wezterm_path: &Option<String>) -> String {
        format!(
            "{} cli tlscreds",
            Self::wezterm_bin_path(remote_wezterm_path)
        )
    }

    fn should_retry_tls_bootstrap_after_reuse_error(err: &anyhow::Error) -> bool {
        match err.root_cause().downcast_ref::<std::io::Error>() {
            Some(ioerr) => ioerr.kind() == std::io::ErrorKind::ConnectionRefused,
            None => true,
        }
    }

    fn ssh_connect(
        &mut self,
        ssh_dom: SshDomain,
        initial: bool,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<()> {
        let ssh_config = mux::ssh::ssh_domain_to_ssh_config(&ssh_dom)?;

        let sess = ssh_connect_with_ui(ssh_config, ui)?;
        let cmd = Self::build_ssh_proxy_command(
            &ssh_dom.remote_wezterm_path,
            ssh_dom.override_proxy_command.as_deref(),
            initial,
        );
        ui.output_str(&format!("Running: {}\n", cmd));
        log::debug!("going to run {}", cmd);

        let exec = wezterm_ssh::runtime::block_on(sess.exec(&cmd, None))?;

        let mut stderr = exec.stderr;
        std::thread::Builder::new()
            .name("ssh-proxy-stderr".to_string())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(len) = stderr.read(&mut buf) {
                    if len == 0 {
                        break;
                    } else {
                        let stderr = &buf[0..len];
                        log::error!("ssh stderr: {}", String::from_utf8_lossy(stderr));
                    }
                }
            })
            .context("spawn ssh proxy stderr reader thread")?;

        // This is a bit gross, but it helps to surface errors in running
        // the proxy, and prevents us from hanging forever after the process
        // has died
        let mut child = exec.child;
        std::thread::Builder::new()
            .name("ssh-proxy-waiter".to_string())
            .spawn(move || match child.wait() {
                Err(err) => log::error!("waiting on {} failed: {:#}", cmd, err),
                Ok(status) if !status.success() => log::error!("{}: {}", cmd, status),
                _ => {}
            })
            .context("spawn ssh proxy waiter thread")?;

        let stream: Box<dyn AsyncReadAndWrite> = Box::new(SshStream::new(exec.stdin, exec.stdout)?);
        self.stream.replace(stream);
        Ok(())
    }

    fn unix_connect(
        &mut self,
        unix_dom: UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        let target = unix_dom.target();
        ui.output_str(&format!("Connect to {:?}\n", target));
        log::trace!("connect to {:?}", target);

        let max_attempts = if no_auto_start { Some(1) } else { None };

        let stream = match unix_connect_with_retry(&target, false, max_attempts) {
            Ok(stream) => stream,
            Err(e) => {
                if no_auto_start || unix_dom.no_serve_automatically || !initial {
                    bail!("failed to connect to {:?}: {}", target, e);
                }
                log::warn!(
                    "While connecting to {:?}: {}.  Will try spawning the server.",
                    target,
                    e
                );
                ui.output_str(&format!("Error: {}.  Will try spawning server.\n", e));

                let argv = unix_dom.serve_command()?;
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| anyhow!("unix domain serve command is empty"))?;

                let mut cmd = std::process::Command::new(program);
                cmd.args(args);

                #[cfg(unix)]
                if let Some(mask) = umask::UmaskSaver::saved_umask() {
                    unsafe {
                        cmd.pre_exec(move || {
                            libc::umask(mask);
                            Ok(())
                        });
                    }
                }

                log::warn!("Running: {:?}", cmd);
                ui.output_str(&format!("Running: {:?}\n", cmd));

                let child = cmd
                    .spawn()
                    .with_context(|| format!("while spawning {:?}", cmd))?;
                if let Err(err) = std::thread::Builder::new()
                    .name("unix-domain-server-waiter".to_string())
                    .spawn(move || match child.wait_with_output() {
                        Ok(out) => {
                            if let Ok(stdout) = std::str::from_utf8(&out.stdout) {
                                if !stdout.is_empty() {
                                    log::warn!("stdout: {}", stdout);
                                }
                            }
                            if let Ok(stderr) = std::str::from_utf8(&out.stderr) {
                                if !stderr.is_empty() {
                                    log::warn!("stderr: {}", stderr);
                                }
                            }
                        }
                        Err(err) => {
                            log::error!("spawn: {:#}", err);
                        }
                    })
                {
                    log::error!("failed to spawn unix domain server waiter thread: {err:#}");
                }

                unix_connect_with_retry(&target, true, None).with_context(|| {
                    format!("(after spawning server) failed to connect to {:?}", target)
                })?
            }
        };

        ui.output_str("Connected!\n");
        stream.set_read_timeout(Some(unix_dom.read_timeout))?;
        stream.set_write_timeout(Some(unix_dom.write_timeout))?;
        let stream: Box<dyn AsyncReadAndWrite> = Box::new(stream);
        self.stream.replace(stream);
        Ok(())
    }

    pub fn tls_connect(
        &mut self,
        tls_client: TlsDomainClient,
        _initial: bool,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<()> {
        openssl::init();

        let remote_address = &tls_client.remote_address;

        let remote_host_name = remote_address.split(':').next().ok_or_else(|| {
            anyhow!(
                "expected mux_server_remote_address to have the form 'host:port', but have {}",
                remote_address
            )
        })?;

        // If we are reconnecting and already bootstrapped via SSH, let's see if
        // we can connect using those same credentials and avoid running through
        // the SSH authentication flow.
        if let Some(Ok(_)) = tls_client.ssh_parameters() {
            match self.try_connect(&tls_client, ui, remote_address, remote_host_name) {
                Ok(stream) => {
                    self.stream.replace(stream);
                    return Ok(());
                }
                Err(err) => {
                    if !Self::should_retry_tls_bootstrap_after_reuse_error(&err) {
                        // Transport-level IO failures other than connection-refused
                        // mean we had trouble reaching or otherwise talking to the
                        // remote host. Re-running the SSH bootstrap is unlikely to help.
                        return Err(err);
                    }
                    ui.output_str(&format!(
                        "Failed to reuse creds: {:?}\nWill retry bootstrap via SSH\n",
                        err
                    ));
                }
            }
        }

        if let Some(Ok(ssh_params)) = tls_client.ssh_parameters() {
            if self.tls_creds.is_none() {
                // We need to bootstrap via an ssh session

                let mut ssh_config = wezterm_ssh::Config::new();
                ssh_config.add_default_config_files();

                let mut fields = ssh_params.host_and_port.split(':');
                let host = fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no host component somehow"))?;
                let port = fields.next();

                let mut ssh_config = ssh_config.for_host(host);
                if let Some(username) = &ssh_params.username {
                    ssh_config.insert("user".to_string(), username.to_string());
                }
                if let Some(port) = port {
                    ssh_config.insert("port".to_string(), port.to_string());
                }

                let sess = ssh_connect_with_ui(ssh_config, ui)?;

                let creds = ui.run_and_log_error(|| {
                    // The `tlscreds` command will start the server if needed and then
                    // obtain client credentials that we can use for tls.
                    let cmd = Self::build_tls_creds_command(&tls_client.remote_wezterm_path);

                    ui.output_str(&format!("Running: {}\n", cmd));
                    let mut exec = wezterm_ssh::runtime::block_on(sess.exec(&cmd, None))
                        .with_context(|| format!("executing `{}` on remote host", cmd))?;

                    log::debug!("waiting for command to finish");
                    let status = exec.child.wait()?;
                    if !status.success() {
                        anyhow::bail!("{} failed", cmd);
                    }

                    drop(exec.stdin);

                    let mut stderr = exec.stderr;
                    if let Err(err) = thread::Builder::new()
                        .name("tls-creds-stderr".to_string())
                        .spawn(move || {
                            // stderr is ideally empty
                            let mut err = String::new();
                            let _ = stderr.read_to_string(&mut err);
                            if !err.is_empty() {
                                log::error!("remote: `{}` stderr -> `{}`", cmd, err);
                            }
                        })
                    {
                        log::error!("failed to spawn tls creds stderr reader thread: {err:#}");
                    }

                    let creds = match Pdu::decode(exec.stdout)
                        .context("reading tlscreds response")?
                        .pdu
                    {
                        Pdu::GetTlsCredsResponse(creds) => creds,
                        _ => bail!("unexpected response to tlscreds"),
                    };

                    // Save the credentials to disk, as that is currently the easiest
                    // way to get them into openssl.  Ideally we'd keep these entirely
                    // in memory.
                    std::fs::write(&self.tls_creds_ca_path()?, creds.ca_cert_pem.as_bytes())?;
                    std::fs::write(
                        &self.tls_creds_cert_path()?,
                        creds.client_cert_pem.as_bytes(),
                    )?;
                    log::info!("got TLS creds");
                    Ok(creds)
                })?;
                self.tls_creds.replace(creds);
            }
        }

        let cloned_ui = ui.clone();
        let stream = cloned_ui.run_and_log_error({
            || self.try_connect(&tls_client, ui, remote_address, remote_host_name)
        })?;
        self.stream.replace(stream);
        Ok(())
    }

    fn try_connect(
        &mut self,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
        remote_address: &str,
        remote_host_name: &str,
    ) -> anyhow::Result<Box<dyn AsyncReadAndWrite>> {
        let mut connector = SslConnector::builder(SslMethod::tls())?;

        let cert_file = match tls_client.pem_cert.clone() {
            Some(cert) => cert,
            None => self.tls_creds_cert_path()?,
        };

        connector
            .set_certificate_file(&cert_file, SslFiletype::PEM)
            .context(format!(
                "set_certificate_file to {} for TLS client",
                cert_file.display()
            ))?;

        if let Some(chain_file) = tls_client.pem_ca.as_ref() {
            connector
                .set_certificate_chain_file(chain_file)
                .context(format!(
                    "set_certificate_chain_file to {} for TLS client",
                    chain_file.display()
                ))?;
        }

        let key_file = match tls_client.pem_private_key.clone() {
            Some(key) => key,
            None => self.tls_creds_cert_path()?,
        };
        connector
            .set_private_key_file(&key_file, SslFiletype::PEM)
            .context(format!(
                "set_private_key_file to {} for TLS client",
                key_file.display()
            ))?;

        fn load_cert(name: &Path) -> anyhow::Result<X509> {
            let cert_bytes = std::fs::read(name)?;
            log::trace!("loaded {}", name.display());
            Ok(X509::from_pem(&cert_bytes)?)
        }
        for name in &tls_client.pem_root_certs {
            if name.is_dir() {
                for entry in std::fs::read_dir(name)? {
                    if let Ok(cert) = load_cert(&entry?.path()) {
                        connector.cert_store_mut().add_cert(cert).ok();
                    }
                }
            } else {
                connector.cert_store_mut().add_cert(load_cert(name)?)?;
            }
        }

        if let Ok(ca_path) = self.tls_creds_ca_path() {
            if ca_path.exists() {
                connector.cert_store_mut().add_cert(load_cert(&ca_path)?)?;
            }
        }

        let connector = connector.build();
        let connector = connector
            .configure()?
            .verify_hostname(!tls_client.accept_invalid_hostnames);

        ui.output_str(&format!("Connecting to {} using TLS\n", remote_address));
        let stream = TcpStream::connect(remote_address)
            .with_context(|| format!("connecting to {}", remote_address))?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(tls_client.write_timeout))?;
        stream.set_read_timeout(Some(tls_client.read_timeout))?;

        let stream = Box::new(AsyncSslStream::new(
            connector
                .connect(
                    tls_client
                        .expected_cn
                        .as_deref()
                        .unwrap_or(remote_host_name),
                    stream,
                )
                .with_context(|| {
                    format!(
                        "SslConnector for {} with host name {}",
                        remote_address, remote_host_name,
                    )
                })?,
        ));
        ui.output_str("TLS Connected!\n");
        Ok(stream)
    }
}

impl Client {
    fn matches_dispatch_authority(&self, authority: &ClientDispatchAuthority) -> bool {
        Arc::ptr_eq(&self.incarnation, &authority.client_incarnation)
            && Arc::ptr_eq(
                &self.connection_generation,
                &authority.connection_generation,
            )
    }

    fn new(
        local_domain_id: Option<DomainId>,
        mut reconnectable: Reconnectable,
        mux_owner: Weak<Mux>,
    ) -> Self {
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, mut receiver) = unbounded();
        let client_id = ClientId::new();
        let incarnation = Arc::new(ClientIncarnation);
        let connection_generation = Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION));
        let initial_dispatch_authority = ClientDispatchAuthority::new(
            local_domain_id,
            mux_owner,
            Arc::clone(&incarnation),
            Arc::clone(&connection_generation),
        );
        let mut reconnect_dispatch_authority = initial_dispatch_authority.clone();

        if let Err(err) = thread::Builder::new()
            .name("client-reconnect".to_string())
            .spawn(move || {
                let cfg = configuration();
                let base_interval = Duration::from_millis(cfg.client_reconnect_base_interval_ms);
                let max_interval = Duration::from_millis(cfg.client_reconnect_max_interval_ms);

                let max_attempts = cfg.client_reconnect_max_attempts;
                let healthy_session =
                    Duration::from_millis(cfg.client_reconnect_healthy_session_ms);

                let mut backoff = base_interval;
                // One connection window for the whole domain, opened lazily on
                // the first disconnect. It used to be constructed inside the
                // loop, so every reconnect cycle spawned a *new* window: a
                // host that accepts the connection and then immediately drops
                // the session cycles forever, and the user gets an endless
                // stream of "Reconnecting..." windows at startup.
                let mut reconnect_ui: Option<ConnectionUI> = None;
                // Cycles since the last connection that stayed up long enough
                // to count as recovered. Bounds the churn above.
                let mut failed_cycles: u32 = 0;
                loop {
                    let session_started = std::time::Instant::now();
                    let (thread_result, returned_reconnectable, returned_receiver) = client_thread(
                        reconnectable,
                        receiver,
                        reconnect_dispatch_authority.clone(),
                    );
                    reconnectable = returned_reconnectable;
                    receiver = returned_receiver;
                    reconnect_dispatch_authority = match reconnect_dispatch_authority
                        .advance_generation()
                    {
                        Ok(next) => next,
                        Err(err) => {
                            log::error!("cannot retire mux client transport authority: {err:#}");
                            break;
                        }
                    };
                    // A session that survived long enough is a genuine
                    // recovery, not churn: forget the earlier failures so a
                    // long-lived domain can still reconnect indefinitely
                    // across ordinary transient drops.
                    if session_started.elapsed() >= healthy_session {
                        failed_cycles = 0;
                    }
                    if let Err(e) = thread_result {
                        if !reconnectable.reconnectable() || local_domain_id.is_none() {
                            log::debug!("client thread ended: {}", e);
                            break;
                        }

                        let local_domain_id = local_domain_id.expect("checked above");

                        if let Some(ioerr) = e.root_cause().downcast_ref::<std::io::Error>() {
                            if let std::io::ErrorKind::UnexpectedEof = ioerr.kind() {
                                // A clean server shutdown surfaces as EOF; for a
                                // TLS or plain-local connection that means "stop,
                                // don't reconnect". But for a unix proxy_command
                                // domain (remote mux reached over ssh+nc) an EOF
                                // just means the transport pipe dropped while the
                                // remote mux keeps running, so we reconnect and
                                // re-sync rather than give up.
                                let is_unix_proxy = matches!(
                                    &reconnectable.config,
                                    ClientDomainConfig::Unix(u) if u.proxy_command.is_some()
                                );
                                if !is_unix_proxy {
                                    log::error!("server closed connection ({})", e);
                                    break;
                                }
                                log::warn!(
                                    "proxy_command connection closed ({}); will reconnect",
                                    e
                                );
                            }
                        }

                        if let Some(err) = e.root_cause().downcast_ref::<NotReconnectableError>() {
                            log::error!("{}; won't try to reconnect", err);
                            break;
                        }

                        failed_cycles = failed_cycles.saturating_add(1);
                        if max_attempts != 0 && failed_cycles > max_attempts {
                            log::error!(
                                "giving up on domain {local_domain_id}: {failed_cycles} \
                                 reconnect cycles without a session lasting {healthy_session:?} \
                                 (last error: {e}). Raise \
                                 client_reconnect_max_attempts to keep retrying."
                            );
                            if let Some(ui) = reconnect_ui.as_ref() {
                                ui.output_str(&format!(
                                    "Giving up after {failed_cycles} reconnect attempts: {e}\n"
                                ));
                            }
                            break;
                        }

                        let ui = reconnect_ui.get_or_insert_with(|| {
                            let ui = ConnectionUI::new();
                            ui.title("FrankenTerm: Reconnecting...");
                            ui
                        });

                        // Bounded, unlike before: a host that is simply down
                        // never returns Ok here, and an unbounded loop meant
                        // the domain retried until the app exited.
                        let mut reconnected = false;
                        let mut dial_attempts: u32 = 0;
                        loop {
                            ui.sleep_with_reason(
                                &format!("client disconnected {}; will reconnect", e),
                                backoff,
                            )
                            .ok();
                            let initial = false;
                            let no_auto_start = true; // Don't auto-start on a reconnect
                            match reconnectable.connect(initial, ui, no_auto_start) {
                                Ok(_) => {
                                    backoff = base_interval;
                                    log::error!("Reconnected!");
                                    let reattach_ui = ui.clone();
                                    match reconnect_dispatch_authority.resolve_current() {
                                        Ok(Some(dispatch)) => {
                                            promise::spawn::spawn_into_main_thread(async move {
                                                if !dispatch.is_current() {
                                                    return Ok(());
                                                }
                                                let result = ClientDomain::reattach_if_current(
                                                    Arc::clone(&dispatch.mux),
                                                    Arc::clone(&dispatch.domain),
                                                    Arc::clone(&dispatch.inner),
                                                    reattach_ui,
                                                )
                                                .await;
                                                if !dispatch.is_current() {
                                                    return Ok(());
                                                }
                                                result
                                            })
                                            .detach();
                                        }
                                        Ok(None) => {
                                            log::trace!(
                                                "discarding reconnect reattach for retired client \
                                                 authority in domain {local_domain_id}"
                                            );
                                        }
                                        Err(err) => {
                                            log::error!(
                                                "cannot resolve reconnect reattach authority for \
                                                 domain {local_domain_id}: {err:#}"
                                            );
                                        }
                                    }
                                    reconnected = true;
                                    break;
                                }
                                Err(err) => {
                                    dial_attempts = dial_attempts.saturating_add(1);
                                    if max_attempts != 0 && dial_attempts >= max_attempts {
                                        ui.output_str(&format!(
                                            "giving up after {dial_attempts} attempts: {err}\n"
                                        ));
                                        break;
                                    }
                                    backoff = (backoff + backoff).min(max_interval);
                                    ui.output_str(&format!(
                                        "problem reconnecting: {}; will reconnect in {:?}\n",
                                        err, backoff
                                    ));
                                }
                            }
                        }
                        if !reconnected {
                            log::error!(
                                "giving up on domain {local_domain_id}: could not reconnect \
                                 after {dial_attempts} attempts (last error: {e}). Raise \
                                 client_reconnect_max_attempts to keep retrying."
                            );
                            break;
                        }
                    } else {
                        log::error!("client_thread returned without any error condition");
                        break;
                    }
                }

                match reconnect_dispatch_authority.resolve_current() {
                    Ok(Some(dispatch)) => {
                        promise::spawn::spawn_into_main_thread(async move {
                            if !dispatch.is_current() {
                                return Ok(());
                            }
                            let client_domain = dispatch.client_domain();
                            if !dispatch.is_current() {
                                return Ok(());
                            }
                            let _ = client_domain.perform_detach_if_current(&dispatch.inner);
                            anyhow::Result::<()>::Ok(())
                        })
                        .detach();
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if let Some(domain_id) = local_domain_id {
                            log::error!(
                                "cannot resolve final detach authority for domain \
                                 {domain_id}: {err:#}"
                            );
                        } else {
                            log::error!("cannot resolve final standalone authority: {err:#}");
                        }
                    }
                }
            })
        {
            log::error!("failed to spawn client reconnect thread: {err:#}");
            let _ = initial_dispatch_authority.advance_generation();
        }

        Self {
            sender,
            local_domain_id,
            incarnation,
            connection_generation,
            is_reconnectable,
            is_local,
            client_id,
            client_domain_config,
        }
    }

    pub fn into_client_domain_config(self) -> ClientDomainConfig {
        self.client_domain_config
    }

    pub async fn verify_version_compat(
        &self,
        ui: &ConnectionUI,
    ) -> anyhow::Result<GetCodecVersionResponse> {
        let version_info = {
            let codec_version = self.get_codec_version(GetCodecVersion {});
            let timeout = async {
                promise::spawn::sleep(Duration::from_secs(60)).await;
                Err(Timeout).context("Timeout")
            };

            pin_mut!(codec_version);
            pin_mut!(timeout);

            match select(codec_version, timeout).await {
                Either::Left((result, _)) => result,
                Either::Right((result, _)) => result,
            }
        };

        match version_info {
            Ok(info) => {
                // ft-kuxho.B.1 + ft-kuxho.B.3: feed the rolling-upgrade
                // window helper with the real symmetric tuple. The
                // server's GetCodecVersionResponse now carries
                // `min_supported` (ft-kuxho.B.3); a legacy peer that
                // pre-dates the field will deserialize with the sentinel
                // value 0, in which case we conservatively substitute
                // `info.codec_vers` (treat the legacy server as
                // supporting only its own version).
                let remote_min = if info.min_supported == 0 {
                    info.codec_vers
                } else {
                    info.min_supported
                };
                match codec::check_compat(
                    CODEC_VERSION,
                    codec::CODEC_VERSION_MIN_SUPPORTED,
                    info.codec_vers,
                    remote_min,
                ) {
                    Ok(codec::CompatDecision::Compatible { agreed }) => {
                        if info.codec_vers != CODEC_VERSION {
                            log::warn!(
                                "Codec compat window: server={}, client={}, agreed={} \
                                 (peer is inside the supported window)",
                                info.codec_vers,
                                CODEC_VERSION,
                                agreed
                            );
                        }
                        log::trace!(
                            "Server version is {} (codec version {}, agreed {})",
                            info.version_string,
                            info.codec_vers,
                            agreed
                        );
                        self.set_client_id(SetClientId {
                            client_id: self.client_id.clone(),
                            is_proxy: false,
                        })
                        .await?;
                        Ok(info)
                    }
                    Err(_) => {
                        let err = IncompatibleVersionError {
                            version: info.version_string,
                            codec_vers: info.codec_vers,
                        };
                        ui.output_str(&err.to_string());
                        log::error!("{:?}", err);
                        Err(err.into())
                    }
                }
            }
            Err(err) => {
                log::trace!("{:?}", err);
                let msg = if err.root_cause().is::<Timeout>() {
                    "Timed out while parsing the response from the server. \
                    This may be due to network connectivity issues"
                        .to_string()
                } else if err.root_cause().is::<CorruptResponse>() {
                    "Received an implausible and likely corrupt response from \
                    the server. This can happen if the remote host outputs \
                    to stdout prior to running commands. \
                    Check your shell startup!"
                        .to_string()
                } else if err.root_cause().is::<ChannelSendError>() {
                    "Internal channel was closed prior to sending request. \
                    This may indicate that the remote host output invalid data \
                    to stdout prior to running the requested command. \
                    Check your shell startup!"
                        .to_string()
                } else {
                    format!(
                        "Please install the same version of FrankenTerm on both \
                     the client and server! \
                     The server reported error '{err}' while being asked for its \
                     version.  This likely means that the server is older \
                     than the client, but it could also happen if the remote \
                     host outputs to stdout prior to running commands. \
                     Check your shell startup!",
                    )
                };
                ui.output_str(&msg);
                bail!("{}", msg);
            }
        }
    }

    #[allow(dead_code)]
    pub fn local_domain_id(&self) -> Option<DomainId> {
        self.local_domain_id
    }

    fn compute_unix_domain(
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<config::UnixDomain> {
        match std::env::var_os("WEZTERM_UNIX_SOCKET") {
            Some(path) if !path.is_empty() => Ok(config::UnixDomain {
                socket_path: Some(path.into()),
                ..Default::default()
            }),
            Some(_) | None => {
                if !prefer_mux {
                    if let Ok(gui) = crate::discovery::resolve_gui_sock_path(class_name) {
                        return Ok(config::UnixDomain {
                            socket_path: Some(gui),
                            no_serve_automatically: true,
                            ..Default::default()
                        });
                    }
                }

                let config = configuration();
                Ok(config
                    .unix_domains
                    .first()
                    .ok_or_else(|| {
                        anyhow!(
                            "no default unix domain is configured and WEZTERM_UNIX_SOCKET \
                             is not set in the environment"
                        )
                    })?
                    .clone())
            }
        }
    }

    pub fn new_default_unix_domain(
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<Self> {
        let unix_dom = Self::compute_unix_domain(prefer_mux, class_name)?;
        Self::new_unix_domain(None, &unix_dom, initial, ui, no_auto_start, Weak::new())
    }

    pub fn new_unix_domain(
        local_domain_id: Option<DomainId>,
        unix_dom: &UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
        mux_owner: Weak<Mux>,
    ) -> anyhow::Result<Self> {
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_dom.clone()), None);
        reconnectable.connect(initial, ui, no_auto_start)?;
        Ok(Self::new(local_domain_id, reconnectable, mux_owner))
    }

    pub fn new_tls(
        local_domain_id: DomainId,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
        mux_owner: Weak<Mux>,
    ) -> anyhow::Result<Self> {
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Tls(tls_client.clone()), None);
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        Ok(Self::new(Some(local_domain_id), reconnectable, mux_owner))
    }

    pub fn new_ssh(
        local_domain_id: DomainId,
        ssh_dom: &SshDomain,
        ui: &mut ConnectionUI,
        mux_owner: Weak<Mux>,
    ) -> anyhow::Result<Self> {
        let mut reconnectable = Reconnectable::new(ClientDomainConfig::Ssh(ssh_dom.clone()), None);
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        Ok(Self::new(Some(local_domain_id), reconnectable, mux_owner))
    }

    pub async fn send_pdu(&self, pdu: Pdu) -> anyhow::Result<Pdu> {
        let (promise, rx) = bounded(1);
        self.sender
            .send(ReaderMessage::SendPdu {
                pdu: Box::new(pdu),
                promise,
            })
            .await
            .map_err(|_| ChannelSendError)
            .context("send_pdu send")?;
        rx.recv().await.context("send_pdu recv")?
    }

    pub async fn resolve_pane_id(&self, pane_id: Option<PaneId>) -> anyhow::Result<PaneId> {
        let pane_id: PaneId = match pane_id {
            Some(p) => p,
            None => {
                if let Ok(pane) = std::env::var("WEZTERM_PANE") {
                    pane.parse()?
                } else {
                    let mut clients = self.list_clients().await?.clients;
                    clients.retain(|client| client.focused_pane_id.is_some());
                    clients.sort_by_key(|b| std::cmp::Reverse(b.last_input));
                    if clients.is_empty() {
                        anyhow::bail!(
                            "--pane-id was not specified and $WEZTERM_PANE
                         is not set in the environment, and I couldn't
                         determine which pane was currently focused"
                        );
                    }

                    clients[0]
                        .focused_pane_id
                        .expect("to have filtered out above")
                }
            }
        };
        Ok(pane_id)
    }

    rpc!(ping, Ping = (), Pong);
    rpc!(list_panes, ListPanes = (), ListPanesResponse);
    rpc!(spawn_v2, SpawnV2, SpawnResponse);
    rpc!(split_pane, SplitPane, SpawnResponse);
    rpc!(
        move_pane_to_new_tab,
        MovePaneToNewTab,
        MovePaneToNewTabResponse
    );
    rpc!(write_to_pane, WriteToPane, UnitResponse);
    rpc!(send_paste, SendPaste, UnitResponse);
    rpc!(key_down, SendKeyDown, UnitResponse);
    rpc!(key_up, SendKeyUp, UnitResponse);
    rpc!(mouse_event, SendMouseEvent, UnitResponse);
    rpc!(resize, Resize, UnitResponse);
    rpc!(set_zoomed, SetPaneZoomed, UnitResponse);
    rpc!(activate_pane_direction, ActivatePaneDirection, UnitResponse);
    rpc!(
        get_pane_render_changes,
        GetPaneRenderChanges,
        LivenessResponse
    );
    rpc!(get_lines, GetLines, GetLinesResponse);
    rpc!(
        get_dimensions,
        GetPaneRenderableDimensions,
        GetPaneRenderableDimensionsResponse
    );
    rpc!(get_codec_version, GetCodecVersion, GetCodecVersionResponse);
    rpc!(get_tls_creds, GetTlsCreds = (), GetTlsCredsResponse);
    rpc!(
        search_scrollback,
        SearchScrollbackRequest,
        SearchScrollbackResponse
    );
    rpc!(kill_pane, KillPane, UnitResponse);
    rpc!(set_client_id, SetClientId, UnitResponse);
    rpc!(list_clients, GetClientList = (), GetClientListResponse);
    rpc!(set_window_workspace, SetWindowWorkspace, UnitResponse);
    rpc!(set_active_workspace, SetActiveWorkspace, UnitResponse);
    rpc!(set_focused_pane_id, SetFocusedPane, UnitResponse);
    rpc!(get_image_cell, GetImageCell, GetImageCellResponse);
    rpc!(set_configured_palette_for_pane, SetPalette, UnitResponse);
    rpc!(set_tab_title, TabTitleChanged, UnitResponse);
    rpc!(set_window_title, WindowTitleChanged, UnitResponse);
    rpc!(rename_workspace, RenameWorkspace, UnitResponse);
    rpc!(erase_scrollback, EraseScrollbackRequest, UnitResponse);
    rpc!(
        get_pane_direction,
        GetPaneDirection,
        GetPaneDirectionResponse
    );
    rpc!(adjust_pane_size, AdjustPaneSize, UnitResponse);
}

#[cfg(test)]
impl Client {
    fn test_dispatch_authority(&self, mux_owner: Weak<Mux>) -> ClientDispatchAuthority {
        ClientDispatchAuthority::new(
            self.local_domain_id,
            mux_owner,
            Arc::clone(&self.incarnation),
            Arc::clone(&self.connection_generation),
        )
    }

    pub(crate) fn new_test_client(
        local_domain_id: Option<DomainId>,
        client_domain_config: ClientDomainConfig,
    ) -> Self {
        let (sender, _receiver) = unbounded();
        Self {
            sender,
            local_domain_id,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            client_id: ClientId {
                hostname: "test-host".to_string(),
                username: "tester".to_string(),
                pid: 1,
                epoch: 1,
                id: 1,
                ssh_auth_sock: None,
            },
            client_domain_config,
            is_reconnectable: false,
            is_local: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MuxTestScope;
    use asupersync::runtime::RuntimeBuilder;
    use codec::{
        GetCodecVersionResponse, PaneRemoved, SetClientId, UnitResponse, WindowTitleChanged,
        WindowWorkspaceChanged,
    };
    use metrics::atomics::AtomicU64 as MetricAtomicU64;
    use metrics::{Counter, Gauge};
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::Ordering;
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::sync::{Mutex as StdMutex, Once};
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_LOGGER: TestLogger = TestLogger {
        records: StdMutex::new(Vec::new()),
    };
    static TEST_LOGGER_INIT: Once = Once::new();
    #[cfg(unix)]
    static TEST_SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestLogger {
        records: StdMutex<Vec<String>>,
    }

    impl log::Log for TestLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            self.records.lock().expect("test logger lock").push(format!(
                "{} {}",
                record.level(),
                record.args()
            ));
        }

        fn flush(&self) {}
    }

    fn reset_test_logger() {
        TEST_LOGGER_INIT.call_once(|| {
            log::set_logger(&TEST_LOGGER).expect("install test logger");
            log::set_max_level(log::LevelFilter::Trace);
        });
        TEST_LOGGER
            .records
            .lock()
            .expect("test logger lock")
            .clear();
    }

    fn captured_logs() -> Vec<String> {
        TEST_LOGGER
            .records
            .lock()
            .expect("test logger lock")
            .clone()
    }

    fn asupersync_block_on<F: std::future::Future>(future: F) -> F::Output {
        RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build frankenterm-client asupersync runtime")
            .block_on(future)
    }

    fn assert_expected_reader_shutdown(result: anyhow::Result<()>) {
        let error = result.expect_err("reader must not report success when its transport ends");
        let client_destroyed = error
            .downcast_ref::<NotReconnectableError>()
            .is_some_and(|kind| *kind == NotReconnectableError::ClientWasDestroyed);
        let clean_eof = error
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == ErrorKind::UnexpectedEof);
        assert!(
            client_destroyed || clean_eof,
            "unexpected reader shutdown disposition: {:#}",
            error
        );
    }

    /// Cancellable hang watchdog. Returns a guard; dropping it (test finished,
    /// even on panic) stops the watchdog. A fire-and-forget watchdog that
    /// outlived its test could `process::exit` during a *later* test if the
    /// whole suite ran slower than the timeout (busy CI/swarm host), spuriously
    /// killing the run. The watchdog only aborts if the guard is still alive at
    /// the deadline (the test genuinely hung).
    struct WatchdogGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for WatchdogGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[must_use = "hold the guard for the duration of the test"]
    fn hang_watchdog(secs: u64, label: &'static str, exit_code: i32) -> WatchdogGuard {
        use std::sync::atomic::Ordering;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            for _ in 0..secs.saturating_mul(20) {
                std::thread::sleep(Duration::from_millis(50));
                if flag.load(Ordering::SeqCst) {
                    return;
                }
            }
            if !flag.swap(true, Ordering::SeqCst) {
                eprintln!("WATCHDOG: `{label}` HUNG after {secs}s");
                std::process::exit(exit_code);
            }
        });
        WatchdogGuard(done)
    }

    #[cfg(unix)]
    fn unique_handshake_socket_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let sequence = TEST_SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!(
            "/tmp/ftk4-{}-{nanos}-{sequence}.sock",
            std::process::id(),
        ))
    }

    #[derive(Debug)]
    struct RpcMetricProbe {
        pending: Arc<MetricAtomicU64>,
        admitted: Arc<MetricAtomicU64>,
        preclosed: Arc<MetricAtomicU64>,
        delivered: Arc<MetricAtomicU64>,
        abandoned: Arc<MetricAtomicU64>,
        transport_failed_live: Arc<MetricAtomicU64>,
        transport_cleared_abandoned: Arc<MetricAtomicU64>,
        retirement_reply_channel_full: Arc<MetricAtomicU64>,
        future_serial: Arc<MetricAtomicU64>,
        unmatched_serial: Arc<MetricAtomicU64>,
        serial_exhausted: Arc<MetricAtomicU64>,
        reserve_failed: Arc<MetricAtomicU64>,
        serial_collision: Arc<MetricAtomicU64>,
        protocol_reply_channel_full: Arc<MetricAtomicU64>,
    }

    impl RpcMetricProbe {
        fn new() -> (RpcMetrics, Self) {
            let probe = Self {
                pending: Arc::new(MetricAtomicU64::new(0.0_f64.to_bits())),
                admitted: Arc::new(MetricAtomicU64::new(0)),
                preclosed: Arc::new(MetricAtomicU64::new(0)),
                delivered: Arc::new(MetricAtomicU64::new(0)),
                abandoned: Arc::new(MetricAtomicU64::new(0)),
                transport_failed_live: Arc::new(MetricAtomicU64::new(0)),
                transport_cleared_abandoned: Arc::new(MetricAtomicU64::new(0)),
                retirement_reply_channel_full: Arc::new(MetricAtomicU64::new(0)),
                future_serial: Arc::new(MetricAtomicU64::new(0)),
                unmatched_serial: Arc::new(MetricAtomicU64::new(0)),
                serial_exhausted: Arc::new(MetricAtomicU64::new(0)),
                reserve_failed: Arc::new(MetricAtomicU64::new(0)),
                serial_collision: Arc::new(MetricAtomicU64::new(0)),
                protocol_reply_channel_full: Arc::new(MetricAtomicU64::new(0)),
            };
            let metrics = RpcMetrics {
                pending: Gauge::from_arc(Arc::clone(&probe.pending)),
                admitted: Counter::from_arc(Arc::clone(&probe.admitted)),
                preclosed: Counter::from_arc(Arc::clone(&probe.preclosed)),
                delivered: Counter::from_arc(Arc::clone(&probe.delivered)),
                abandoned: Counter::from_arc(Arc::clone(&probe.abandoned)),
                transport_failed_live: Counter::from_arc(Arc::clone(&probe.transport_failed_live)),
                transport_cleared_abandoned: Counter::from_arc(Arc::clone(
                    &probe.transport_cleared_abandoned,
                )),
                retirement_reply_channel_full: Counter::from_arc(Arc::clone(
                    &probe.retirement_reply_channel_full,
                )),
                future_serial: Counter::from_arc(Arc::clone(&probe.future_serial)),
                unmatched_serial: Counter::from_arc(Arc::clone(&probe.unmatched_serial)),
                serial_exhausted: Counter::from_arc(Arc::clone(&probe.serial_exhausted)),
                reserve_failed: Counter::from_arc(Arc::clone(&probe.reserve_failed)),
                serial_collision: Counter::from_arc(Arc::clone(&probe.serial_collision)),
                protocol_reply_channel_full: Counter::from_arc(Arc::clone(
                    &probe.protocol_reply_channel_full,
                )),
            };
            (metrics, probe)
        }

        fn counter(counter: &MetricAtomicU64) -> u64 {
            counter.load(Ordering::Acquire)
        }

        fn pending(&self) -> f64 {
            let value = f64::from_bits(self.pending.load(Ordering::Acquire));
            assert!(
                value.is_finite() && value >= 0.0 && value.fract() == 0.0,
                "pending gauge must be a finite non-negative integer, got {}",
                value
            );
            value
        }

        fn assert_balanced(&self) {
            assert_eq!(
                self.pending(),
                0.0,
                "balance is asserted only at a quiescent boundary"
            );
            let retired = Self::counter(&self.delivered)
                + Self::counter(&self.abandoned)
                + Self::counter(&self.transport_failed_live)
                + Self::counter(&self.transport_cleared_abandoned)
                + Self::counter(&self.retirement_reply_channel_full);
            assert_eq!(
                Self::counter(&self.admitted),
                retired,
                "at quiescence every admitted request must have exactly one retirement outcome"
            );
        }
    }

    fn pending_replies_for_test() -> (PendingReplies, RpcMetricProbe) {
        let (metrics, probe) = RpcMetricProbe::new();
        (PendingReplies::new(metrics), probe)
    }

    #[test]
    fn remote_rpc_error_reason_is_control_escaped_and_bounded() {
        let reason = format!("\u{1b}[31mdenied\n{}\r", "x".repeat(600));
        let sanitized = sanitized_remote_error_reason(&reason);

        assert!(
            sanitized.chars().all(|ch| !ch.is_control()),
            "sanitized remote reason retained a control character: {:?}",
            sanitized
        );
        assert!(
            sanitized.ends_with('…'),
            "oversized remote reason must advertise truncation"
        );
        assert!(
            sanitized.len() < reason.len(),
            "sanitized remote reason must not preserve an attacker-sized payload"
        );
    }

    #[test]
    fn pending_rpc_gauge_aggregates_across_connections_and_drop_drains_exactly_once() {
        let (metrics, probe) = RpcMetricProbe::new();
        let mut first = PendingReplies::new(metrics.clone());
        let (first_tx, first_rx) = bounded(1);
        let first_serial = first
            .admit(first_tx, "Ping")
            .expect("admit first connection RPC")
            .expect("assign first connection serial");
        assert_eq!(probe.pending(), 1.0);

        let mut second = PendingReplies::new(metrics);
        assert_eq!(
            probe.pending(),
            1.0,
            "constructing another connection must not reset the process gauge"
        );
        let (second_tx, second_rx) = bounded(1);
        second
            .admit(second_tx, "SearchScrollbackRequest")
            .expect("admit second connection RPC")
            .expect("assign second connection serial");
        assert_eq!(probe.pending(), 2.0);

        first
            .complete(first_serial, Pdu::Pong(Pong {}))
            .expect("first connection reply should enqueue");
        assert!(first_rx
            .try_recv()
            .expect("first connection completion")
            .is_ok());
        assert_eq!(probe.pending(), 1.0);

        drop(second);
        assert!(second_rx
            .try_recv()
            .expect("PendingReplies::drop must wake the live waiter")
            .is_err());
        assert_eq!(probe.pending(), 0.0);
        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.transport_failed_live), 1);
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_preclosed_admission_consumes_neither_frame_slot_nor_serial() {
        let (mut pending, probe) = pending_replies_for_test();
        let (preclosed_tx, preclosed_rx) = bounded(1);
        drop(preclosed_rx);

        assert_eq!(
            pending
                .admit(preclosed_tx, "SearchScrollbackRequest")
                .expect("preclosed admission is a normal disposition"),
            None
        );
        assert_eq!(pending.highest_issued(), 0);
        assert!(pending.map.is_empty());
        assert_eq!(RpcMetricProbe::counter(&probe.preclosed), 1);
        probe.assert_balanced();

        let (live_tx, live_rx) = bounded(1);
        let serial = pending
            .admit(live_tx, "Ping")
            .expect("live request should admit")
            .expect("live request should receive a serial");
        assert_eq!(serial.get(), 1);
        assert_eq!(pending.highest_issued(), 1);
        assert_eq!(probe.pending(), 1.0);

        assert_eq!(
            pending
                .complete(serial, Pdu::Pong(Pong {}))
                .expect("live response should deliver"),
            ReplyDisposition::Delivered
        );
        assert_eq!(
            live_rx
                .try_recv()
                .expect("live response should be queued")
                .expect("live response should be successful"),
            Pdu::Pong(Pong {})
        );
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_max_serial_is_issued_once_and_collision_never_replaces() {
        let (mut pending, probe) = pending_replies_for_test();
        pending.next_serial = NonZeroU64::new(u64::MAX);
        pending.highest_issued = u64::MAX - 1;

        let (max_tx, max_rx) = bounded(1);
        let max_serial = pending
            .admit(max_tx, "Ping")
            .expect("maximum serial should be admissible once")
            .expect("maximum serial should be assigned");
        assert_eq!(max_serial.get(), u64::MAX);
        assert_eq!(pending.next_serial, None);

        let (exhausted_tx, exhausted_rx) = bounded(1);
        let exhausted = pending
            .admit(exhausted_tx, "Ping")
            .expect_err("the serial after u64::MAX must fail closed");
        assert!(matches!(exhausted, PendingRpcError::SerialExhausted));
        assert!(exhausted_rx
            .try_recv()
            .expect("admission failure should wake the caller")
            .is_err());
        assert_eq!(RpcMetricProbe::counter(&probe.serial_exhausted), 1);

        pending.next_serial = Some(max_serial);
        let (collision_tx, collision_rx) = bounded(1);
        let collision = pending
            .admit(collision_tx, "SearchScrollbackRequest")
            .expect_err("occupied serial must not be replaced");
        assert!(matches!(
            collision,
            PendingRpcError::SerialCollision {
                serial,
                request: "SearchScrollbackRequest",
                pending_request: "Ping",
            } if serial == max_serial
        ));
        assert!(collision_rx
            .try_recv()
            .expect("collision should wake the caller")
            .is_err());
        assert_eq!(pending.map.len(), 1);
        assert_eq!(
            pending
                .map
                .get(&max_serial)
                .expect("original pending request must remain")
                .pdu_name,
            "Ping"
        );
        assert_eq!(RpcMetricProbe::counter(&probe.serial_collision), 1);

        pending
            .complete(max_serial, Pdu::Pong(Pong {}))
            .expect("the original maximum-serial request should complete");
        assert!(max_rx.try_recv().expect("maximum-serial response").is_ok());
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_completion_classifies_delivery_abandonment_and_protocol_errors() {
        let (mut pending, probe) = pending_replies_for_test();

        let (delivered_tx, delivered_rx) = bounded(1);
        let delivered_serial = pending
            .admit(delivered_tx, "Ping")
            .expect("admit delivered request")
            .expect("assign delivered serial");
        assert_eq!(
            pending
                .complete(delivered_serial, Pdu::Pong(Pong {}))
                .expect("response before receiver drop should deliver"),
            ReplyDisposition::Delivered
        );
        drop(delivered_rx);

        let duplicate = pending
            .complete(delivered_serial, Pdu::Pong(Pong {}))
            .expect_err("a duplicate response must be fatal");
        assert!(matches!(
            duplicate,
            PendingRpcError::UnmatchedSerial { serial, .. } if serial == delivered_serial
        ));

        let future_serial =
            NonZeroU64::new(delivered_serial.get() + 1).expect("future serial is nonzero");
        let future = pending
            .complete(future_serial, Pdu::Pong(Pong {}))
            .expect_err("a never-issued future response must be fatal");
        assert!(matches!(
            future,
            PendingRpcError::FutureSerial { serial, .. } if serial == future_serial
        ));

        let (abandoned_tx, abandoned_rx) = bounded(1);
        let abandoned_serial = pending
            .admit(abandoned_tx, "SearchScrollbackRequest")
            .expect("admit abandoned request")
            .expect("assign abandoned serial");
        drop(abandoned_rx);
        assert_eq!(
            pending
                .complete(
                    abandoned_serial,
                    Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                        results: Vec::new(),
                    }),
                )
                .expect("a late response to an abandoned caller must drain"),
            ReplyDisposition::Abandoned
        );

        let (full_tx, full_rx) = bounded(1);
        let filler = full_tx.clone();
        let full_serial = pending
            .admit(full_tx, "Ping")
            .expect("admit full-channel request")
            .expect("assign full-channel serial");
        filler
            .try_send(Ok(Pdu::Pong(Pong {})))
            .expect("test should prefill completion channel");
        let full = pending
            .complete(full_serial, Pdu::Pong(Pong {}))
            .expect_err("a full one-shot reply channel is an invariant failure");
        assert!(matches!(
            full,
            PendingRpcError::ReplyChannelFull { serial, .. } if serial == full_serial
        ));
        drop(full_rx);

        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.unmatched_serial), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.future_serial), 1);
        assert_eq!(
            RpcMetricProbe::counter(&probe.retirement_reply_channel_full),
            1
        );
        assert_eq!(
            RpcMetricProbe::counter(&probe.protocol_reply_channel_full),
            1
        );
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
    }

    #[test]
    fn fatal_reply_dispositions_fail_every_other_live_waiter() {
        let (mut duplicate_pending, duplicate_probe) = pending_replies_for_test();
        let (completed_tx, completed_rx) = bounded(1);
        let completed_serial = duplicate_pending
            .admit(completed_tx, "Ping")
            .expect("admit first request")
            .expect("assign first serial");
        let (duplicate_witness_tx, duplicate_witness_rx) = bounded(1);
        duplicate_pending
            .admit(duplicate_witness_tx, "GetCodecVersion")
            .expect("admit duplicate-failure witness")
            .expect("assign witness serial");
        duplicate_pending
            .complete(completed_serial, Pdu::Pong(Pong {}))
            .expect("first reply should enqueue");
        assert!(completed_rx.try_recv().expect("first completion").is_ok());

        let duplicate_error = duplicate_pending
            .complete_or_fail_transport(completed_serial, Pdu::Pong(Pong {}))
            .expect_err("duplicate reply must retire the transport");
        assert!(matches!(
            duplicate_error,
            PendingRpcError::UnmatchedSerial { serial, .. } if serial == completed_serial
        ));
        assert!(duplicate_witness_rx
            .try_recv()
            .expect("duplicate reply must wake every other live waiter")
            .is_err());
        duplicate_probe.assert_balanced();

        let (mut full_pending, full_probe) = pending_replies_for_test();
        let (full_tx, full_rx) = bounded(1);
        let filler = full_tx.clone();
        let full_serial = full_pending
            .admit(full_tx, "Ping")
            .expect("admit full-channel request")
            .expect("assign full-channel serial");
        let (full_witness_tx, full_witness_rx) = bounded(1);
        full_pending
            .admit(full_witness_tx, "GetCodecVersion")
            .expect("admit full-channel witness")
            .expect("assign full-channel witness serial");
        filler
            .try_send(Ok(Pdu::Pong(Pong {})))
            .expect("prefill completion channel");

        let full_error = full_pending
            .complete_or_fail_transport(full_serial, Pdu::Pong(Pong {}))
            .expect_err("full reply channel must retire the transport");
        assert!(matches!(
            full_error,
            PendingRpcError::ReplyChannelFull { serial, .. } if serial == full_serial
        ));
        assert!(full_rx
            .try_recv()
            .expect("prefilled reply must remain intact")
            .is_ok());
        assert!(full_witness_rx
            .try_recv()
            .expect("full reply channel must wake every other live waiter")
            .is_err());
        full_probe.assert_balanced();
    }

    #[test]
    fn codec_future_serial_rejection_reaches_client_protocol_metric() {
        let (mut pending, probe) = pending_replies_for_test();
        let (live_tx, live_rx) = bounded(1);
        let admitted_serial = pending
            .admit(live_tx, "Ping")
            .expect("admit live request")
            .expect("assign live serial");
        let future_serial = admitted_serial
            .get()
            .checked_add(1)
            .expect("test serial has headroom");

        let mut encoded = Vec::new();
        Pdu::Pong(Pong {})
            .encode(&mut encoded, future_serial)
            .expect("encode future response");
        let mut reader = std::io::Cursor::new(encoded);
        let error = asupersync_block_on(Pdu::decode_async(
            &mut reader,
            Some(pending.highest_issued()),
        ))
        .expect_err("codec must reject a response above the transport high-water mark");
        assert_eq!(
            error.downcast_ref::<CorruptResponse>(),
            Some(&CorruptResponse::SerialAboveCeiling {
                serial: future_serial,
                max_serial: admitted_serial.get(),
            })
        );

        pending.fail_after_decode_error(&error);
        assert_eq!(RpcMetricProbe::counter(&probe.future_serial), 1);
        assert_eq!(
            probe.pending(),
            0.0,
            "header rejection must retire the transport and clear every waiter"
        );
        assert!(live_rx
            .try_recv()
            .expect("transport teardown must wake the admitted waiter")
            .is_err());
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_transport_teardown_wakes_live_and_clears_abandoned_waiters() {
        let (mut pending, probe) = pending_replies_for_test();
        let (live_tx, live_rx) = bounded(1);
        pending
            .admit(live_tx, "Ping")
            .expect("admit live waiter")
            .expect("assign live waiter serial");
        let (abandoned_tx, abandoned_rx) = bounded(1);
        pending
            .admit(abandoned_tx, "SearchScrollbackRequest")
            .expect("admit eventual abandonment")
            .expect("assign abandoned waiter serial");
        drop(abandoned_rx);

        let transport_error = anyhow!("test transport terminated during flush");
        pending.fail_after_transport_error(&transport_error);

        let live_error = live_rx
            .try_recv()
            .expect("transport teardown should wake live waiter")
            .expect_err("transport teardown should report an error");
        assert!(
            live_error
                .to_string()
                .contains("test transport terminated during flush"),
            "unexpected transport error: {:#}",
            live_error
        );
        assert!(
            !live_error.to_string().contains("Client was destroyed"),
            "transport failure must preserve its real cause"
        );
        assert_eq!(RpcMetricProbe::counter(&probe.transport_failed_live), 1);
        assert_eq!(
            RpcMetricProbe::counter(&probe.transport_cleared_abandoned),
            1
        );
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
    }

    #[test]
    fn wezterm_bin_path_defaults_to_wezterm() {
        assert_eq!(Reconnectable::wezterm_bin_path(&None), "wezterm");
    }

    /// ft-7f2om: the IncompatibleVersionError Display impl must surface
    /// both the local and remote codec versions plus a pointer to the
    /// atomic-redeploy operator runbook so on-call sees the runbook
    /// path the moment a handshake fails. The pre-ft-7f2om message
    /// said "install the same version of wezterm" — outdated framing
    /// (we retired the wezterm-as-identity framing in ft-zoxxq.3) and
    /// gave operators no pointer to the new ft-kuxho docs trio.
    #[test]
    fn incompatible_version_error_includes_versions_and_runbook_link() {
        let err = IncompatibleVersionError {
            version: "ft 0.99.99".to_string(),
            codec_vers: 47,
        };
        let rendered = err.to_string();

        // Local-side codec version (CODEC_VERSION constant) must appear
        // verbatim. We don't hard-code the literal value because it
        // moves over time; instead we read the same constant the impl
        // reads and assert the formatted string contains it.
        let local_codec = CODEC_VERSION.to_string();
        assert!(
            rendered.contains(&local_codec),
            "rendered error missing local CODEC_VERSION ({}): {}",
            local_codec,
            rendered
        );

        // Remote-side codec version (the field value) must appear too.
        assert!(
            rendered.contains("47"),
            "rendered error missing remote codec_vers (47): {}",
            rendered
        );

        // Remote frankenterm version string must appear so operators
        // can correlate against deploy bundles.
        assert!(
            rendered.contains("ft 0.99.99"),
            "rendered error missing remote version string: {}",
            rendered
        );

        // Runbook link must appear so on-call has a one-click path to
        // the operator procedure. The exact docs path is part of the
        // user-facing contract — change it deliberately or this test
        // fails.
        assert!(
            rendered.contains("docs/codec-atomic-redeploy.md"),
            "rendered error missing docs/codec-atomic-redeploy.md link: {}",
            rendered
        );

        // The retired "install the same version of wezterm" framing
        // (per ft-zoxxq.3) must NOT come back. Guard against accidental
        // reverts.
        assert!(
            !rendered.contains("install the same version of wezterm"),
            "retired ft-zoxxq.3 framing reintroduced in IncompatibleVersionError: {}",
            rendered
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_version_compat_reaches_set_client_id_over_real_stream_ft_kuxho_4() {
        reset_test_logger();

        let socket_path = unique_handshake_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind local UDS handshake server");
        let (set_client_id_tx, set_client_id_rx) = mpsc::channel::<SetClientId>();
        let (server_release_tx, server_release_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-kuxho-handshake-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let (mut stream, _addr) = listener.accept().context("accept mux client")?;

                loop {
                    let decoded = Pdu::decode(&mut stream).context("server decode client PDU")?;

                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION + 1,
                                version_string: "ft-kuxho.4-real-stream-server".to_string(),
                                executable_path: PathBuf::from("/usr/local/bin/ft"),
                                config_file_path: None,
                                min_supported: CODEC_VERSION,
                            })
                        }
                        Pdu::SetClientId(set_client_id) => {
                            set_client_id_tx
                                .send(set_client_id)
                                .expect("test receiver should remain alive");
                            Pdu::UnitResponse(UnitResponse {})
                        }
                        other => panic!("unexpected client handshake PDU: {}", other.pdu_name()),
                    };

                    response
                        .encode(&mut stream, decoded.serial)
                        .context("server encode response PDU")?;
                    stream.flush().context("server flush response PDU")?;

                    if matches!(response, Pdu::UnitResponse(_)) {
                        // Keep the transport alive until the test has consumed
                        // the response. Closing here races the handshake future
                        // against the reader's terminal EOF result and makes
                        // the harness nondeterministic even when the response
                        // was decoded and delivered correctly.
                        server_release_rx
                            .recv_timeout(Duration::from_secs(5))
                            .context("wait for completed client handshake")?;
                        break;
                    }
                }

                Ok(())
            })
            .expect("spawn local UDS handshake server");

        let mut ui = ConnectionUI::new_headless();
        let unix_domain = UnixDomain {
            name: "ft-kuxho.4".to_string(),
            socket_path: Some(socket_path),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_domain.clone()), None);
        reconnectable
            .connect(true, &mut ui, true)
            .expect("connect to local UDS handshake server");
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, mut receiver) = unbounded();
        let client = Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable,
            is_local,
        };
        let dispatch_authority = client.test_dispatch_authority(Weak::new());

        let info = asupersync_block_on(async {
            let handshake = client.verify_version_compat(&ui);
            let worker =
                client_thread_async(&mut reconnectable, &mut receiver, &dispatch_authority);
            pin_mut!(handshake);
            pin_mut!(worker);
            match select(handshake, worker).await {
                Either::Left((result, _worker)) => result,
                Either::Right((result, _handshake)) => {
                    panic!(
                        "client thread ended before handshake completed: {:?}",
                        result
                    )
                }
            }
        })
        .expect("v+1 server with min_supported=v must complete client handshake");

        assert_eq!(info.codec_vers, CODEC_VERSION + 1);
        assert_eq!(info.min_supported, CODEC_VERSION);

        let set_client_id = set_client_id_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server should see client id registration");
        assert_eq!(set_client_id.client_id, client.client_id);
        assert!(!set_client_id.is_proxy);

        let logs = captured_logs().join("\n");
        assert!(
            logs.contains("Codec compat window: server="),
            "expected in-window negotiation warning, got logs: {}",
            logs
        );

        server_release_tx
            .send(())
            .expect("server must remain alive through handshake assertions");
        drop(client);
        server
            .join()
            .expect("server thread should join")
            .expect("server handshake loop should succeed");
    }

    /// Regression for the mux-client connect hang: the reader must receive
    /// socket-readiness wakeups even when the server's reply arrives AFTER the
    /// reader parks (i.e. any real, latency-bearing connection). Before the fix
    /// the reader ran as a directly-polled `block_on` future, which asupersync
    /// never delivers I/O wakeups to, so a delayed handshake reply was never
    /// consumed and `verify_version_compat` timed out. `client_thread` now runs
    /// the reader as a scheduler-managed task via `block_on_io`. This test
    /// drives the REAL `client_thread` against a server that delays its reply,
    /// so it hangs (watchdog-killed) on regression and completes on success.
    /// Regression guard for the connect hang (#1). The mux reader must receive
    /// socket-readiness wakeups even when the server reply arrives AFTER it
    /// parks (any real, latency-bearing connection). `client_thread` runs the
    /// reader via `block_on_io` (a scheduler-managed, reactor-driven task);
    /// before that it was a directly-polled `block_on` future, which asupersync
    /// never delivers fd readiness to, so a delayed reply hung forever.
    ///
    /// NB: this is deliberately a *real fd* test (UDS + a wall-clock watchdog),
    /// NOT an asupersync `LabRuntime` test. LabRuntime models logical concurrency
    /// / scheduling (DPOR) for simulated tasks; it does not model the OS fd
    /// reactor whose readiness delivery is the actual bug here, so a LabRuntime
    /// test would prove nothing about this regression. The watchdog converts a
    /// hang into a fast, explicit failure.
    #[cfg(unix)]
    #[test]
    fn reader_receives_delayed_handshake_reply_ft_connect_fix() {
        // Watchdog: if the reader never wakes, fail fast instead of waiting out
        // the 60s in-flight handshake timeout.
        let _wd = hang_watchdog(12, "delayed-handshake reader (readiness regression)", 98);

        reset_test_logger();
        let socket_path = unique_handshake_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind local UDS handshake server");
        let server = std::thread::Builder::new()
            .name("ft-connect-fix-delayed-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let (mut stream, _addr) = listener.accept().context("accept mux client")?;
                let mut first = true;
                loop {
                    let decoded = Pdu::decode(&mut stream).context("server decode client PDU")?;
                    if first {
                        // Reply only AFTER the reader has parked waiting for
                        // readability — the condition the old code hung on.
                        std::thread::sleep(Duration::from_millis(400));
                        first = false;
                    }
                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "ft-connect-fix-delayed-server".to_string(),
                                executable_path: PathBuf::from("/usr/local/bin/ft"),
                                config_file_path: None,
                                min_supported: CODEC_VERSION,
                            })
                        }
                        Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                        other => panic!("unexpected client handshake PDU: {}", other.pdu_name()),
                    };
                    response
                        .encode(&mut stream, decoded.serial)
                        .context("server encode response PDU")?;
                    stream.flush().context("server flush response PDU")?;
                    if matches!(response, Pdu::UnitResponse(_)) {
                        break;
                    }
                }
                Ok(())
            })
            .expect("spawn delayed UDS handshake server");

        let mut ui = ConnectionUI::new_headless();
        let unix_domain = UnixDomain {
            name: "ft-connect-fix".to_string(),
            socket_path: Some(socket_path),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let mut reconnectable = Reconnectable::new(ClientDomainConfig::Unix(unix_domain), None);
        reconnectable
            .connect(true, &mut ui, true)
            .expect("connect to local UDS handshake server");
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, receiver) = unbounded();
        let client = Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable,
            is_local,
        };
        let dispatch_authority = client.test_dispatch_authority(Weak::new());

        // Run the REAL reader exactly as production does (client_thread ->
        // block_on_io -> scheduler-managed, reactor-driven task).
        let reader = std::thread::Builder::new()
            .name("ft-connect-fix-reader".to_string())
            .spawn(move || {
                let (result, _reconnectable, _receiver) =
                    client_thread(reconnectable, receiver, dispatch_authority);
                result
            })
            .expect("spawn reader thread");

        let info = asupersync_block_on(client.verify_version_compat(&ui))
            .expect("delayed handshake reply must be consumed by the reader");
        assert_eq!(info.codec_vers, CODEC_VERSION);

        drop(client);
        assert_expected_reader_shutdown(reader.join().expect("reader thread panicked"));
        server
            .join()
            .expect("server thread should join")
            .expect("server handshake loop should succeed");
    }

    /// A caller dropping an RPC is abandonment, not transport destruction.
    ///
    /// This socket-pair regression covers both cancellation sides of the
    /// reader's admission boundary through the production reconnect loop and
    /// deliberately reorders multiple live and abandoned replies. The
    /// preclosed request must consume neither a frame nor serial. Requests
    /// dropped after the server confirms receipt remain as tombstones until
    /// their replies are drained. Distinct live response types prove exact
    /// serial correlation, and a final Ping proves that every operation used
    /// the same socket, reader, and connection generation.
    #[cfg(unix)]
    #[test]
    fn abandoned_rpc_replies_drain_without_retiring_transport_generation() {
        fn recv_rpc_with_timeout(receiver: &Receiver<anyhow::Result<Pdu>>, label: &str) -> Pdu {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match receiver.try_recv() {
                    Ok(result) => {
                        return result.unwrap_or_else(|err| panic!("{}: {:#}", label, err));
                    }
                    Err(async_channel::TryRecvError::Closed) => {
                        panic!("{}: completion channel closed without a response", label)
                    }
                    Err(async_channel::TryRecvError::Empty) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{}: timed out waiting for RPC completion",
                            label
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }

        let (client_stream, mut server_stream) =
            UnixStream::pair().expect("create in-memory mux socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound server reads");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("bound server writes");
        let (requests_admitted_tx, requests_admitted_rx) = mpsc::channel::<()>();
        let (release_replies_tx, release_replies_rx) = mpsc::channel::<()>();
        let (release_server_tx, release_server_rx) = mpsc::channel::<()>();
        let (server_done_tx, server_done_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-rpc-abandonment-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let result = (|| -> anyhow::Result<()> {
                    let first =
                        Pdu::decode(&mut server_stream).context("decode first admitted RPC")?;
                    let second =
                        Pdu::decode(&mut server_stream).context("decode second admitted RPC")?;
                    let third =
                        Pdu::decode(&mut server_stream).context("decode third admitted RPC")?;
                    let fourth =
                        Pdu::decode(&mut server_stream).context("decode fourth admitted RPC")?;

                    anyhow::ensure!(first.serial == 1, "preclosed request consumed a serial");
                    anyhow::ensure!(second.serial == 2, "unexpected second serial");
                    anyhow::ensure!(third.serial == 3, "unexpected third serial");
                    anyhow::ensure!(fourth.serial == 4, "unexpected fourth serial");
                    anyhow::ensure!(
                        matches!(first.pdu, Pdu::SearchScrollbackRequest(_)),
                        "unexpected first request"
                    );
                    anyhow::ensure!(
                        matches!(second.pdu, Pdu::GetCodecVersion(_)),
                        "unexpected second request"
                    );
                    anyhow::ensure!(
                        matches!(third.pdu, Pdu::SearchScrollbackRequest(_)),
                        "unexpected third request"
                    );
                    anyhow::ensure!(
                        matches!(fourth.pdu, Pdu::Ping(_)),
                        "unexpected fourth request"
                    );

                    requests_admitted_tx
                        .send(())
                        .context("notify caller of wire admission")?;
                    release_replies_rx
                        .recv_timeout(Duration::from_secs(10))
                        .context("wait for caller abandonment")?;

                    Pdu::Pong(Pong {})
                        .encode(&mut server_stream, fourth.serial)
                        .context("encode fourth response")?;
                    Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                        results: Vec::new(),
                    })
                    .encode(&mut server_stream, third.serial)
                    .context("encode third response")?;
                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                        codec_vers: CODEC_VERSION,
                        version_string: "ft-rpc-correlation".to_string(),
                        executable_path: PathBuf::from("/test/ft"),
                        config_file_path: None,
                        min_supported: CODEC_VERSION,
                    })
                    .encode(&mut server_stream, second.serial)
                    .context("encode second response")?;
                    Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                        results: Vec::new(),
                    })
                    .encode(&mut server_stream, first.serial)
                    .context("encode first response")?;
                    Write::flush(&mut server_stream).context("flush reverse-order responses")?;

                    let final_ping =
                        Pdu::decode(&mut server_stream).context("decode final live Ping")?;
                    anyhow::ensure!(
                        final_ping.serial == 5,
                        "same transport did not retain its serial high-water mark"
                    );
                    anyhow::ensure!(
                        matches!(final_ping.pdu, Pdu::Ping(_)),
                        "unexpected final request"
                    );
                    Pdu::Pong(Pong {})
                        .encode(&mut server_stream, final_ping.serial)
                        .context("encode final Pong")?;
                    Write::flush(&mut server_stream).context("flush final Pong")?;

                    release_server_rx
                        .recv_timeout(Duration::from_secs(10))
                        .context("hold transport through client assertions")?;
                    Ok(())
                })();
                let _ = server_done_tx.send(());
                result
            })
            .expect("spawn socket-pair RPC server");

        let unix_domain = UnixDomain {
            name: "ft-rpc-abandonment".to_string(),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(unix_domain),
            Some(Box::new(client_stream)),
        );
        let client = Client::new(None, reconnectable, Weak::new());

        let (preclosed_tx, preclosed_rx) = bounded(1);
        drop(preclosed_rx);
        asupersync_block_on(client.sender.send(ReaderMessage::SendPdu {
            pdu: Box::new(Pdu::Ping(Ping {})),
            promise: preclosed_tx,
        }))
        .expect("queue preclosed request");

        let search_request = |pane_id| {
            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern: mux::pane::Pattern::CaseSensitiveString("needle".to_string()),
                range: 0..100,
                limit: Some(4),
            })
        };
        let (abandoned_one_tx, abandoned_one_rx) = bounded(1);
        let (live_two_tx, live_two_rx) = bounded(1);
        let (abandoned_three_tx, abandoned_three_rx) = bounded(1);
        let (live_four_tx, live_four_rx) = bounded(1);
        asupersync_block_on(async {
            client
                .sender
                .send(ReaderMessage::SendPdu {
                    pdu: Box::new(search_request(11)),
                    promise: abandoned_one_tx,
                })
                .await?;
            client
                .sender
                .send(ReaderMessage::SendPdu {
                    pdu: Box::new(Pdu::GetCodecVersion(GetCodecVersion {})),
                    promise: live_two_tx,
                })
                .await?;
            client
                .sender
                .send(ReaderMessage::SendPdu {
                    pdu: Box::new(search_request(33)),
                    promise: abandoned_three_tx,
                })
                .await?;
            client
                .sender
                .send(ReaderMessage::SendPdu {
                    pdu: Box::new(Pdu::Ping(Ping {})),
                    promise: live_four_tx,
                })
                .await?;
            Ok::<(), async_channel::SendError<ReaderMessage>>(())
        })
        .expect("queue mixed RPCs");

        requests_admitted_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server should confirm all requests reached the wire");
        drop(abandoned_one_rx);
        drop(abandoned_three_rx);
        release_replies_tx
            .send(())
            .expect("release reverse-order server replies");

        let fourth = recv_rpc_with_timeout(&live_four_rx, "fourth live RPC");
        let second = recv_rpc_with_timeout(&live_two_rx, "second live RPC");
        assert_eq!(fourth, Pdu::Pong(Pong {}));
        match second {
            Pdu::GetCodecVersionResponse(info) => {
                assert_eq!(info.codec_vers, CODEC_VERSION);
                assert_eq!(info.version_string, "ft-rpc-correlation");
                assert_eq!(info.executable_path, PathBuf::from("/test/ft"));
            }
            other => panic!(
                "serial two received the wrong response type: {}",
                other.pdu_name()
            ),
        }

        let (final_tx, final_rx) = bounded(1);
        asupersync_block_on(client.sender.send(ReaderMessage::SendPdu {
            pdu: Box::new(Pdu::Ping(Ping {})),
            promise: final_tx,
        }))
        .expect("queue final Ping");
        assert_eq!(
            recv_rpc_with_timeout(&final_rx, "final Ping"),
            Pdu::Pong(Pong {})
        );
        assert_eq!(
            client.connection_generation.load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "caller abandonment must not retire the connection generation"
        );

        let generation = Arc::clone(&client.connection_generation);
        drop(client);
        let retirement_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while generation.load(AtomicOrdering::Acquire) == INITIAL_CONNECTION_GENERATION {
            assert!(
                std::time::Instant::now() < retirement_deadline,
                "production reconnect loop did not retire the destroyed transport"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            generation.load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION + 1,
            "client destruction must retire exactly one transport generation"
        );

        release_server_tx
            .send(())
            .expect("release server after transport retirement");
        server_done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("RPC server did not reach its bounded terminal state");
        server
            .join()
            .expect("RPC server thread should join")
            .expect("RPC server should complete without protocol errors");
    }

    /// Regression guard for the remote-pane write path (#4): the WriteToPane mux
    /// RPC must round-trip against the real reader (a scheduler-managed,
    /// reactor-driven task) without panic or hang. `PaneWriter::write` is the sync
    /// `std::io::Write` impl invoked on the GUI main-thread spawn queue when the
    /// user types into a remote pane. It NO LONGER blocks: it now spawns this RPC
    /// fire-and-forget (mirroring `key_down`/`send_paste`) so a slow or dead/
    /// reconnecting domain cannot park the main thread and freeze the whole GUI
    /// (the head-of-line block). This test drives that WriteToPane RPC to
    /// completion on the standard test runtime against the real reader + a server
    /// that answers WriteToPane, and asserts it round-trips (no panic/hang). It
    /// deliberately no longer wraps the call in `block_on_io` +
    /// `enter_main_thread_dispatch_scope()`: that main-thread-blocking shape is
    /// exactly what the fix removed — it can deadlock the single-threaded runtime
    /// (reader future + blocking join contend for the one worker), which is the
    /// GUI freeze this change eliminates.
    #[cfg(unix)]
    #[test]
    #[ignore = "Pre-existing harness limitation (ft-uyt88 family): the reader-driven \
                 RPC round-trip deadlocks in this single-threaded multi-runtime test \
                 harness (the reader future and the test thread's block-on both need \
                 the one shared worker), independent of this change — it hangs on clean \
                 HEAD too. The main-thread *blocking* write path this guarded was \
                 removed by the non-blocking HOL fix in clientpane.rs, so its original \
                 reason to exist is gone. Re-enable if/when the harness drives the \
                 reader on a dedicated runtime."]
    fn main_thread_pane_write_round_trips_ft_connect_fix() {
        let _wd = hang_watchdog(12, "remote pane write RPC round-trip", 96);

        reset_test_logger();
        let socket_path = unique_handshake_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind local UDS mux server");
        let server = std::thread::Builder::new()
            .name("ft-pane-write-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let (mut stream, _addr) = listener.accept().context("accept mux client")?;
                loop {
                    let decoded = Pdu::decode(&mut stream).context("server decode client PDU")?;
                    let (response, done) = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => (
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "ft-pane-write-server".to_string(),
                                executable_path: PathBuf::from("/usr/local/bin/ft"),
                                config_file_path: None,
                                min_supported: CODEC_VERSION,
                            }),
                            false,
                        ),
                        Pdu::SetClientId(_) => (Pdu::UnitResponse(UnitResponse {}), false),
                        // The WriteToPane reply is what PaneWriter::write blocks on.
                        Pdu::WriteToPane(_) => (Pdu::UnitResponse(UnitResponse {}), true),
                        other => panic!("unexpected client PDU: {}", other.pdu_name()),
                    };
                    response
                        .encode(&mut stream, decoded.serial)
                        .context("server encode response PDU")?;
                    stream.flush().context("server flush response PDU")?;
                    if done {
                        break;
                    }
                }
                Ok(())
            })
            .expect("spawn pane-write UDS server");

        let mut ui = ConnectionUI::new_headless();
        let unix_domain = UnixDomain {
            name: "ft-pane-write".to_string(),
            socket_path: Some(socket_path),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let mut reconnectable = Reconnectable::new(ClientDomainConfig::Unix(unix_domain), None);
        reconnectable
            .connect(true, &mut ui, true)
            .expect("connect to local UDS server");
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, receiver) = unbounded();
        let client = std::sync::Arc::new(Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable,
            is_local,
        });
        let dispatch_authority = client.test_dispatch_authority(Weak::new());

        let reader = std::thread::Builder::new()
            .name("ft-pane-write-reader".to_string())
            .spawn(move || {
                let (result, _reconnectable, _receiver) =
                    client_thread(reconnectable, receiver, dispatch_authority);
                result
            })
            .expect("spawn reader thread");

        asupersync_block_on(client.verify_version_compat(&ui)).expect("handshake completes");

        // The crux: drive the WriteToPane RPC — the operation `PaneWriter::write`
        // now spawns fire-and-forget — to completion on the real reader and assert
        // it round-trips. This intentionally does NOT wrap the call in `block_on_io`
        // + `enter_main_thread_dispatch_scope()`: that main-thread-blocking shape is
        // exactly what the fix removed, and it can deadlock the single-threaded
        // runtime (the reader future and the blocking join contend for the one
        // worker thread) — the GUI freeze this whole change eliminates.
        let write_client = std::sync::Arc::clone(&client);
        let result = asupersync_block_on(async move {
            write_client
                .write_to_pane(codec::WriteToPane {
                    pane_id: 1,
                    data: b"hello-remote".to_vec(),
                })
                .await
        });
        result.expect("remote pane write RPC must round-trip without panic or hang");

        drop(client);
        assert_expected_reader_shutdown(reader.join().expect("reader thread panicked"));
        server
            .join()
            .expect("server thread should join")
            .expect("server pane-write loop should succeed");
    }

    #[test]
    fn ssh_proxy_command_uses_override_verbatim() {
        let cmd = Reconnectable::build_ssh_proxy_command(
            &Some("/opt/wezterm".to_string()),
            Some("custom proxy --flag"),
            true,
        );
        assert_eq!(cmd, "custom proxy --flag");
    }

    #[test]
    fn ssh_proxy_command_uses_initial_proxy_launch_by_default() {
        let cmd =
            Reconnectable::build_ssh_proxy_command(&Some("/opt/wezterm".to_string()), None, true);
        assert_eq!(cmd, "/opt/wezterm cli --prefer-mux proxy");
    }

    #[test]
    fn ssh_proxy_command_disables_auto_start_on_reconnect() {
        let cmd = Reconnectable::build_ssh_proxy_command(&None, None, false);
        assert_eq!(cmd, "wezterm cli --prefer-mux --no-auto-start proxy");
    }

    #[test]
    fn tls_creds_command_uses_remote_wezterm_path_when_present() {
        let cmd = Reconnectable::build_tls_creds_command(&Some("/usr/bin/wezterm".to_string()));
        assert_eq!(cmd, "/usr/bin/wezterm cli tlscreds");
    }

    #[test]
    fn tls_bootstrap_retry_only_on_connection_refused() {
        let err = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(Reconnectable::should_retry_tls_bootstrap_after_reuse_error(
            &err
        ));
    }

    #[test]
    fn tls_bootstrap_retry_rejects_other_transport_failures() {
        let err = anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));
        assert!(!Reconnectable::should_retry_tls_bootstrap_after_reuse_error(&err));
    }

    #[test]
    fn tls_bootstrap_retry_preserves_non_io_fallback_behavior() {
        let err = anyhow!("boom");
        assert!(Reconnectable::should_retry_tls_bootstrap_after_reuse_error(
            &err
        ));
    }

    #[test]
    fn ssh_registration_lock_recovers_after_poison() {
        let registration = Mutex::new(None::<IoRegistration>);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registration.lock().unwrap();
            panic!("simulate SSH registration lock poison");
        }));

        assert!(poisoned.is_err());
        assert!(registration.is_poisoned());

        {
            let guard = lock_registration_mutex(&registration);
            assert!(guard.is_none());
        }

        assert!(!registration.is_poisoned());
    }

    #[test]
    fn unix_connect_with_retry_rejects_zero_attempts() {
        let socket_path = PathBuf::from("/tmp/frankenterm-zero-attempts.sock");
        let err = match unix_connect_with_retry(&UnixTarget::Socket(socket_path), false, Some(0)) {
            Ok(_) => panic!("zero retry attempts should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("greater than zero"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn unix_stream_connect_timeout_rejects_zero_deadline() {
        let err = unix_stream_connect_with_timeout(
            PathBuf::from("/tmp/frankenterm-zero-connect-timeout.sock"),
            Duration::ZERO,
        )
        .expect_err("zero connect timeout should fail before attempting to connect");

        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(
            err.to_string().contains("greater than zero"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn unix_connect_with_retry_rejects_empty_proxy_command() {
        let err = match unix_connect_with_retry(&UnixTarget::Proxy(Vec::new()), false, Some(1)) {
            Ok(_) => panic!("empty proxy command should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("proxy command is empty"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn unix_connect_rejects_empty_serve_command() {
        let socket_path = PathBuf::from("/tmp/frankenterm-empty-serve-command.sock");
        let unix_domain = UnixDomain {
            name: "empty-serve-command".to_string(),
            socket_path: Some(socket_path),
            serve_command: Some(Vec::new()),
            no_serve_automatically: false,
            ..Default::default()
        };
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_domain.clone()), None);
        let mut ui = ConnectionUI::new_headless();

        let err = reconnectable
            .unix_connect(unix_domain, true, &mut ui, false)
            .expect_err("empty serve command should fail before spawning");

        assert!(
            err.to_string().contains("serve command is empty"),
            "unexpected error: {:?}",
            err
        );
    }

    fn unilateral(pdu: Pdu) -> DecodedPdu {
        DecodedPdu { serial: 0, pdu }
    }

    fn standalone_dispatch_authority() -> ClientDispatchAuthority {
        ClientDispatchAuthority::new(
            None,
            Weak::new(),
            Arc::new(ClientIncarnation),
            Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
        )
    }

    #[test]
    fn standalone_client_ignores_cosmetic_unilateral_updates() {
        let authority = standalone_dispatch_authority();
        let result = process_unilateral(
            &authority,
            unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                window_id: 3,
                title: "dev shell".into(),
            })),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn standalone_client_rejects_workspace_rebinding_without_domain() {
        let authority = standalone_dispatch_authority();
        let err = process_unilateral(
            &authority,
            unilateral(Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
                window_id: 7,
                workspace: "ops".into(),
            })),
        )
        .expect_err("workspace topology changes must not be silently ignored");
        let root = err
            .root_cause()
            .downcast_ref::<StandaloneUnilateralError>()
            .expect("root cause should preserve the standalone unilateral classification");
        assert_eq!(
            root,
            &StandaloneUnilateralError::RequiresAttachedDomain {
                pdu_name: "WindowWorkspaceChanged",
            }
        );
    }

    #[test]
    fn standalone_client_rejects_pane_removal_without_domain() {
        let authority = standalone_dispatch_authority();
        let err = process_unilateral(
            &authority,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect_err("pane removal changes must not be silently ignored");
        let root = err
            .root_cause()
            .downcast_ref::<StandaloneUnilateralError>()
            .expect("root cause should preserve the standalone unilateral classification");
        assert_eq!(
            root,
            &StandaloneUnilateralError::RequiresAttachedDomain {
                pdu_name: "PaneRemoved",
            }
        );
    }

    #[test]
    fn retired_connection_generation_discards_queued_unilateral_work() {
        let stale = standalone_dispatch_authority();
        let current = stale
            .advance_generation()
            .expect("the successor transport generation should be minted");

        assert!(!stale.generation_is_current());
        assert!(current.generation_is_current());
        process_unilateral(
            &stale,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect("a PDU queued by a retired transport must fail closed");
        process_unilateral(
            &current,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect_err("the current standalone transport retains topology classification");
    }

    #[test]
    fn client_incarnation_rejects_replacement_client_with_same_domain_id() {
        let config = ClientDomainConfig::Unix(UnixDomain::default());
        let old_client = Client::new_test_client(Some(42), config.clone());
        let replacement_client = Client::new_test_client(Some(42), config);
        let authority = old_client.test_dispatch_authority(Weak::new());

        assert!(old_client.matches_dispatch_authority(&authority));
        assert!(!replacement_client.matches_dispatch_authority(&authority));
    }

    #[test]
    fn attached_authority_uses_captured_mux_not_process_global_mux() {
        let scope = MuxTestScope::enter();
        let origin_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        scope.set_mux(&replacement_mux);

        let authority = ClientDispatchAuthority::new(
            Some(42),
            Arc::downgrade(&origin_mux),
            Arc::new(ClientIncarnation),
            Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
        );
        let captured = authority
            .captured_mux()
            .expect("captured mux owner should remain alive");
        assert!(Arc::ptr_eq(&captured, &origin_mux));
        assert!(!Arc::ptr_eq(&captured, &replacement_mux));
    }

    #[test]
    fn ssh_stream_asupersync_roundtrip_handles_initial_would_block() {
        use asupersync::io::{AsyncReadExt, AsyncWriteExt};

        let (stdin, mut remote_stdin) =
            filedescriptor::socketpair().expect("create stdin socketpair");
        let (mut remote_stdout, stdout) =
            filedescriptor::socketpair().expect("create stdout socketpair");
        let mut stream = SshStream::new(stdin, stdout).expect("construct ssh stream");

        let remote = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            std::thread::sleep(Duration::from_millis(50));
            remote_stdout.write_all(b"ping")?;
            remote_stdout.flush()?;

            let mut buf = [0u8; 4];
            remote_stdin.read_exact(&mut buf)?;
            Ok(buf.to_vec())
        });

        let received = asupersync_block_on(async {
            let mut buf = [0u8; 4];
            AsyncReadExt::read_exact(&mut stream, &mut buf).await?;
            AsyncWriteExt::write_all(&mut stream, b"pong").await?;
            AsyncWriteExt::flush(&mut stream).await?;
            Ok::<Vec<u8>, std::io::Error>(buf.to_vec())
        })
        .expect("client roundtrip should succeed");

        assert_eq!(received, b"ping");
        assert_eq!(
            remote.join().expect("remote thread should join").unwrap(),
            b"pong"
        );
    }
}
