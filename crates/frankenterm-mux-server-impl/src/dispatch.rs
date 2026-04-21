#![allow(clippy::future_not_send)]
#![allow(clippy::type_repetition_in_bounds)]
use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use asupersync::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use async_channel::{Receiver, Sender, TryRecvError, unbounded};
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::future::{Either, select};
use futures::{FutureExt, pin_mut};
use mux::{Mux, MuxNotification};
use std::future::Future;
use std::io::ErrorKind;
use std::pin::Pin;
use std::sync::Arc;
use wezterm_uds::UnixStream;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchIoBackend {
    IoUring,
    Epoll,
    Kqueue,
    Poll,
}

impl DispatchIoBackend {
    pub const fn readiness_default() -> Self {
        #[cfg(target_os = "linux")]
        {
            return Self::Epoll;
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            return Self::Kqueue;
        }
        Self::Poll
    }

    pub const fn current_default() -> Self {
        #[cfg(all(feature = "io-uring", target_os = "linux"))]
        {
            return Self::IoUring;
        }
        Self::readiness_default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchIoPreference {
    Auto,
    IoUring,
    Epoll,
    Kqueue,
    Poll,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchRuntimeConfig {
    preference: DispatchIoPreference,
}

impl DispatchRuntimeConfig {
    #[must_use]
    pub const fn new(preference: DispatchIoPreference) -> Self {
        Self { preference }
    }

    #[must_use]
    pub const fn preference(self) -> DispatchIoPreference {
        self.preference
    }
}

impl Default for DispatchRuntimeConfig {
    fn default() -> Self {
        Self::new(DispatchIoPreference::Auto)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchStreamKind {
    Unix,
    Tls,
    Generic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchIoRuntimeAvailability {
    io_uring_compiled: bool,
    io_uring_runtime_available: bool,
}

impl DispatchIoRuntimeAvailability {
    fn detect() -> Self {
        Self {
            io_uring_compiled: cfg!(all(feature = "io-uring", target_os = "linux")),
            io_uring_runtime_available: io_uring_runtime_available(),
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn for_conformance(
        io_uring_compiled: bool,
        io_uring_runtime_available: bool,
    ) -> Self {
        Self {
            io_uring_compiled,
            io_uring_runtime_available,
        }
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoUringReactor {
    _marker: std::marker::PhantomData<io_uring::IoUring>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchReactor {
    requested: DispatchIoPreference,
    backend: DispatchIoBackend,
    fallback_reason: Option<&'static str>,
}

impl DispatchReactor {
    #[must_use]
    pub fn resolve(config: DispatchRuntimeConfig, stream_kind: DispatchStreamKind) -> Self {
        Self::resolve_with_availability(
            config,
            stream_kind,
            DispatchIoRuntimeAvailability::detect(),
        )
    }

    #[doc(hidden)]
    #[must_use]
    pub fn resolve_with_availability(
        config: DispatchRuntimeConfig,
        stream_kind: DispatchStreamKind,
        availability: DispatchIoRuntimeAvailability,
    ) -> Self {
        let fallback_backend = DispatchIoBackend::readiness_default();
        let wants_io_uring = matches!(
            config.preference,
            DispatchIoPreference::Auto | DispatchIoPreference::IoUring
        );

        if wants_io_uring {
            if stream_kind != DispatchStreamKind::Unix {
                return Self {
                    requested: config.preference,
                    backend: fallback_backend,
                    fallback_reason: Some(
                        "io_uring mux dispatch currently supports UnixStream only; using readiness backend",
                    ),
                };
            }

            if !availability.io_uring_compiled {
                return Self {
                    requested: config.preference,
                    backend: fallback_backend,
                    fallback_reason: Some(
                        "io_uring support was not compiled into this binary; using readiness backend",
                    ),
                };
            }

            if !availability.io_uring_runtime_available {
                return Self {
                    requested: config.preference,
                    backend: fallback_backend,
                    fallback_reason: Some(
                        "io_uring unavailable on this kernel/runtime; using readiness backend",
                    ),
                };
            }

            return Self {
                requested: config.preference,
                backend: DispatchIoBackend::IoUring,
                fallback_reason: None,
            };
        }

        let selected = match config.preference {
            DispatchIoPreference::Auto | DispatchIoPreference::IoUring => unreachable!(),
            DispatchIoPreference::Epoll => {
                #[cfg(target_os = "linux")]
                {
                    DispatchIoBackend::Epoll
                }
                #[cfg(not(target_os = "linux"))]
                {
                    fallback_backend
                }
            }
            DispatchIoPreference::Kqueue => {
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                ))]
                {
                    DispatchIoBackend::Kqueue
                }
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                )))]
                {
                    fallback_backend
                }
            }
            DispatchIoPreference::Poll => DispatchIoBackend::Poll,
        };

        let fallback_reason = match (config.preference, selected) {
            (DispatchIoPreference::Epoll, DispatchIoBackend::Epoll)
            | (DispatchIoPreference::Kqueue, DispatchIoBackend::Kqueue)
            | (DispatchIoPreference::Poll, DispatchIoBackend::Poll) => None,
            (DispatchIoPreference::Epoll, _) | (DispatchIoPreference::Kqueue, _) => Some(
                "requested dispatch backend is unavailable on this platform; using readiness backend",
            ),
            _ => None,
        };

        Self {
            requested: config.preference,
            backend: selected,
            fallback_reason,
        }
    }

    #[must_use]
    pub const fn backend(self) -> DispatchIoBackend {
        self.backend
    }

    #[must_use]
    pub const fn fallback_reason(self) -> Option<&'static str> {
        self.fallback_reason
    }
}

#[cfg(target_os = "linux")]
fn io_uring_runtime_available() -> bool {
    if !cfg!(feature = "io-uring") {
        return false;
    }

    let Ok(osrelease) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else {
        return false;
    };

    let mut components = osrelease.trim().split(['.', '-']);
    let major = components
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = components
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);

    if (major, minor) < (5, 6) {
        return false;
    }

    match std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled") {
        Ok(flag) => flag.trim() == "0",
        Err(_) => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn io_uring_runtime_available() -> bool {
    false
}

pub trait DispatchStream: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Generic
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>>;
    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>>;
}

impl DispatchStream for UnixStream {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Unix
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(UnixStream::wait_for_readable(self))
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(UnixStream::wait_for_writable(self))
    }
}

impl DispatchStream for AsyncSslStream {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Tls
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(AsyncSslStream::wait_for_readable(self))
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(AsyncSslStream::wait_for_writable(self))
    }
}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu(Box<DecodedPdu>),
    Readable,
}

struct MuxSubscriptionGuard {
    mux: Arc<Mux>,
    sub_id: usize,
}

impl MuxSubscriptionGuard {
    fn new(mux: Arc<Mux>, sub_id: usize) -> Self {
        Self { mux, sub_id }
    }
}

impl Drop for MuxSubscriptionGuard {
    fn drop(&mut self) {
        let _ = self.mux.unsubscribe(self.sub_id);
    }
}

fn queue_pdu(item_tx: &Sender<Item>, pdu: Pdu, serial: u64) -> anyhow::Result<()> {
    item_tx
        .try_send(Item::WritePdu(Box::new(DecodedPdu { pdu, serial })))
        .map_err(|err| anyhow::anyhow!("{err:?}"))
}

fn is_clean_disconnect(err: &anyhow::Error) -> bool {
    err.root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io_err| {
            matches!(
                io_err.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionReset
                    | ErrorKind::NotConnected
            )
        })
}

async fn write_pending_pdus<T>(
    stream: &mut T,
    first: Box<DecodedPdu>,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    stream
        .wait_for_writable()
        .await
        .context("waiting for mux stream to become writable")?;

    let mut current = Some(first);
    loop {
        let Some(decoded) = current.take() else {
            break;
        };
        decoded
            .pdu
            .encode_async(stream, decoded.serial)
            .await
            .context("encoding PDU to client")?;
        match item_rx.try_recv() {
            Ok(Item::WritePdu(next)) => current = Some(next),
            Ok(other) => {
                *deferred_item = Some(other);
                break;
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }

    stream.flush().await.context("flushing PDU to client")
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    process_with_config(stream, DispatchRuntimeConfig::default()).await
}

pub async fn process_with_config<T>(stream: T, config: DispatchRuntimeConfig) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    process_async_with_config(stream, config).await
}

pub async fn process_async<T>(mut stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    process_async_with_config(stream, DispatchRuntimeConfig::default()).await
}

async fn process_async_with_config<T>(
    mut stream: T,
    config: DispatchRuntimeConfig,
) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    let reactor = DispatchReactor::resolve(config, stream.dispatch_stream_kind());
    if let Some(reason) = reactor.fallback_reason() {
        log::trace!(
            "process_async configured backend {:?} resolved to {:?}: {}",
            config.preference(),
            reactor.backend(),
            reason
        );
    } else {
        log::trace!(
            "process_async configured backend {:?} resolved to {:?}",
            config.preference(),
            reactor.backend()
        );
    }

    let (item_tx, item_rx) = unbounded::<Item>();
    let mut deferred_item = None;

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| queue_pdu(&item_tx, pdu.pdu, pdu.serial)
    });
    let mut handler = SessionHandler::new(pdu_sender);

    {
        let mux = Mux::get();
        let tx = item_tx.clone();
        let sub_id = mux.subscribe(move |n| tx.try_send(Item::Notif(n)).is_ok());
        let _subscription_guard = MuxSubscriptionGuard::new(mux, sub_id);

        loop {
            let next_item = if let Some(item) = deferred_item.take() {
                Ok(item)
            } else {
                match item_rx.try_recv() {
                    Ok(item) => Ok(item),
                    Err(TryRecvError::Closed) => {
                        Err(anyhow::anyhow!("mux dispatch item queue closed"))
                    }
                    Err(TryRecvError::Empty) => {
                        let rx_msg = item_rx
                            .recv()
                            .map(|result| result.map_err(|err| anyhow::anyhow!("{err:?}")));
                        let wait_for_read = stream.wait_for_readable().map(|result| {
                            result.map(|()| Item::Readable).map_err(anyhow::Error::from)
                        });

                        pin_mut!(rx_msg);
                        pin_mut!(wait_for_read);
                        match select(rx_msg, wait_for_read).await {
                            Either::Left((result, _)) | Either::Right((result, _)) => result,
                        }
                    }
                }
            };

            match next_item {
                Ok(Item::Readable) => {
                    let decoded = match Pdu::decode_async(&mut stream, None).await {
                        Ok(data) => data,
                        Err(err) => {
                            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                                if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                    // Client disconnected: no need to make a noise
                                    return Ok(());
                                }
                            }
                            return Err(err).context("reading Pdu from client");
                        }
                    };
                    handler.process_one(decoded);
                }
                Ok(Item::WritePdu(decoded)) => {
                    if let Err(err) =
                        write_pending_pdus(&mut stream, decoded, &item_rx, &mut deferred_item).await
                    {
                        if is_clean_disconnect(&err) {
                            return Ok(());
                        }
                        return Err(err);
                    }
                }
                Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                    handler.schedule_pane_push(pane_id);
                }
                Ok(Item::Notif(MuxNotification::PaneAdded(_pane_id))) => {}
                Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                    handler.remove_per_pane(pane_id);
                    queue_pdu(
                        &item_tx,
                        Pdu::PaneRemoved(codec::PaneRemoved { pane_id }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                    // ft-12e8l: use the non-inserting accessor. If the pane
                    // was already removed (PaneRemoved arm above clears the
                    // entry), re-inserting a fresh PerPane here would leak —
                    // no subsequent PaneRemoved ever fires for a dead pane.
                    // Silently drop the stale alert; the client can't render
                    // a dead pane anyway. For client-initiated PDU arms that
                    // are the first reference to a pane, per_pane (not this
                    // helper) is still correct.
                    if let Some(per_pane) = handler.per_pane_if_present(pane_id) {
                        {
                            let mut per_pane = per_pane
                                .lock()
                                .map_err(|err| anyhow::anyhow!("per-pane lock poisoned: {err}"))?;
                            per_pane.notifications.push(alert);
                        }
                        handler.schedule_pane_push(pane_id);
                    }
                }
                Ok(Item::Notif(MuxNotification::SaveToDownloads { .. })) => {}
                Ok(Item::Notif(MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                })) => {
                    queue_pdu(
                        &item_tx,
                        Pdu::SetClipboard(codec::SetClipboard {
                            pane_id,
                            clipboard,
                            selection,
                        }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id })) => {
                    queue_pdu(
                        &item_tx,
                        Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::WindowRemoved(_window_id))) => {}
                Ok(Item::Notif(MuxNotification::WindowCreated(_window_id))) => {}
                Ok(Item::Notif(MuxNotification::WindowInvalidated(_window_id))) => {}
                Ok(Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id))) => {
                    let workspace = {
                        let mux = Mux::get();
                        mux.get_window(window_id)
                            .map(|w| w.get_workspace().to_string())
                    };
                    if let Some(workspace) = workspace {
                        queue_pdu(
                            &item_tx,
                            Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                                window_id,
                                workspace,
                            }),
                            0,
                        )?;
                    }
                }
                Ok(Item::Notif(MuxNotification::PaneFocused(pane_id))) => {
                    queue_pdu(
                        &item_tx,
                        Pdu::PaneFocused(codec::PaneFocused { pane_id }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::TabResized(tab_id))) => {
                    queue_pdu(&item_tx, Pdu::TabResized(codec::TabResized { tab_id }), 0)?;
                }
                Ok(Item::Notif(MuxNotification::TabTitleChanged { tab_id, title })) => {
                    queue_pdu(
                        &item_tx,
                        Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::WindowTitleChanged { window_id, title })) => {
                    queue_pdu(
                        &item_tx,
                        Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                })) => {
                    queue_pdu(
                        &item_tx,
                        Pdu::RenameWorkspace(codec::RenameWorkspace {
                            old_workspace,
                            new_workspace,
                        }),
                        0,
                    )?;
                }
                Ok(Item::Notif(MuxNotification::ActiveWorkspaceChanged(_))) => {}
                Ok(Item::Notif(MuxNotification::Empty)) => {}
                Err(err) => {
                    log::error!("process_async Err {}", err);
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
    use codec::Ping;
    use std::io;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    static GLOBAL_STATE_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    struct ScopedMux(Option<Arc<Mux>>);

    impl ScopedMux {
        fn install(mux: &Arc<Mux>) -> Self {
            let prior = Mux::try_get();
            Mux::set_mux(mux);
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

    #[derive(Debug, Default)]
    struct EofDispatchStream;

    impl DispatchStream for EofDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for EofDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for EofDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn subscription_guard_eagerly_unsubscribes_on_drop() {
        let mux = Arc::new(Mux::new(None));
        let observed = Arc::new(AtomicUsize::new(0));
        let notifications = Arc::clone(&observed);
        let sub_id = mux.subscribe(move |_| {
            notifications.fetch_add(1, Ordering::Relaxed);
            true
        });

        {
            let _guard = MuxSubscriptionGuard::new(Arc::clone(&mux), sub_id);
            mux.notify(MuxNotification::Empty);
            assert_eq!(observed.load(Ordering::Relaxed), 1);
        }

        assert!(
            !mux.unsubscribe(sub_id),
            "subscription guard should remove the subscriber eagerly"
        );
        mux.notify(MuxNotification::Empty);
        assert_eq!(observed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn process_async_treats_unexpected_eof_as_clean_disconnect() {
        let _lock = GLOBAL_STATE_TEST_LOCK.lock().expect("global test lock");
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(EofDispatchStream));
        assert!(
            result.is_ok(),
            "EOF should be treated as a normal client disconnect"
        );
    }

    #[derive(Debug, Default)]
    struct CountingDispatchStream {
        bytes_written: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl DispatchStream for CountingDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for CountingDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for CountingDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes_written.fetch_add(buf.len(), Ordering::Relaxed);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn queued_ping(serial: u64) -> Box<DecodedPdu> {
        Box::new(DecodedPdu {
            pdu: Pdu::Ping(Ping {}),
            serial,
        })
    }

    #[test]
    fn write_pending_pdus_batches_flush_and_preserves_first_non_write_item() {
        let _lock = GLOBAL_STATE_TEST_LOCK.lock().expect("global test lock");
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(Item::WritePdu(queued_ping(2)))
            .expect("queue second ping");
        item_tx
            .try_send(Item::Notif(MuxNotification::Empty))
            .expect("queue notification");

        let mut deferred_item = None;
        let mut stream = CountingDispatchStream::default();
        let result = promise::spawn::block_on(write_pending_pdus(
            &mut stream,
            queued_ping(1),
            &item_rx,
            &mut deferred_item,
        ));

        assert!(result.is_ok(), "batched write helper should succeed");
        assert!(
            stream.bytes_written.load(Ordering::Relaxed) > 0,
            "encoded PDUs should write bytes"
        );
        assert_eq!(
            stream.flush_calls.load(Ordering::Relaxed),
            1,
            "batched writes should flush once"
        );
        assert_eq!(
            stream.writable_waits.load(Ordering::Relaxed),
            1,
            "batched writes should wait once"
        );
        assert!(
            matches!(deferred_item, Some(Item::Notif(MuxNotification::Empty))),
            "first non-write item should be preserved for the main loop"
        );
        assert!(
            matches!(item_rx.try_recv(), Err(TryRecvError::Empty)),
            "write batch should drain only queued write items"
        );
    }

    #[test]
    fn dispatch_backend_reports_platform_default() {
        let backend = DispatchIoBackend::current_default();

        #[cfg(all(feature = "io-uring", target_os = "linux"))]
        assert_eq!(backend, DispatchIoBackend::IoUring);
        #[cfg(all(not(feature = "io-uring"), target_os = "linux"))]
        assert_eq!(backend, DispatchIoBackend::Epoll);
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        assert_eq!(backend, DispatchIoBackend::Kqueue);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        assert_eq!(backend, DispatchIoBackend::Poll);
    }

    #[test]
    fn auto_prefers_io_uring_for_unix_when_available() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::Auto),
            DispatchStreamKind::Unix,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: true,
                io_uring_runtime_available: true,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::IoUring);
        assert_eq!(reactor.fallback_reason(), None);
    }

    #[test]
    fn auto_falls_back_when_io_uring_unavailable() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::Auto),
            DispatchStreamKind::Unix,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: true,
                io_uring_runtime_available: false,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::readiness_default());
        assert!(reactor.fallback_reason().is_some());
    }

    #[test]
    fn tls_forces_readiness_even_when_io_uring_is_available() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::IoUring),
            DispatchStreamKind::Tls,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: true,
                io_uring_runtime_available: true,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::readiness_default());
        assert!(
            reactor
                .fallback_reason()
                .is_some_and(|reason| reason.contains("UnixStream"))
        );
    }

    #[test]
    fn explicit_poll_stays_poll() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::Poll),
            DispatchStreamKind::Generic,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: false,
                io_uring_runtime_available: false,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::Poll);
        assert_eq!(reactor.fallback_reason(), None);
    }
}
