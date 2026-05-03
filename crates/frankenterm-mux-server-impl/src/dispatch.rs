#![allow(clippy::future_not_send)]
#![allow(clippy::type_repetition_in_bounds)]
use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use asupersync::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use asupersync::runtime::IoDriverHandle;
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use asupersync::runtime::reactor::{Interest, IoUringReactor};
use async_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::future::{Either, select};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use futures::task::{ArcWake, waker};
use futures::{FutureExt, pin_mut};
use mux::{Mux, MuxNotification};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use parking_lot::Mutex as ParkingMutex;
use std::future::Future;
use std::io::ErrorKind;
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use std::task::{Context as TaskContext, Poll, Waker};
use wezterm_uds::UnixStream;

pub const DISPATCH_ITEM_QUEUE_CAPACITY: usize = 4096;
const TRANSIENT_WRITE_RETRY_LIMIT: usize = 8;

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
            Self::Epoll
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
            Self::Kqueue
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        {
            Self::Poll
        }
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
            (DispatchIoPreference::Epoll | DispatchIoPreference::Kqueue, _) => Some(
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

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    fn io_uring_fd(&self) -> Option<RawFd> {
        None
    }
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

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    fn io_uring_fd(&self) -> Option<RawFd> {
        Some(self.as_raw_fd())
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
    match item_tx.try_send(Item::WritePdu(Box::new(DecodedPdu { pdu, serial }))) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(anyhow::anyhow!(
            "mux dispatch item queue is full (capacity {DISPATCH_ITEM_QUEUE_CAPACITY}); applying client backpressure"
        )),
        Err(TrySendError::Closed(_)) => Err(anyhow::anyhow!("mux dispatch item queue is closed")),
    }
}

fn queue_notification(item_tx: &Sender<Item>, notification: MuxNotification) -> bool {
    match item_tx.try_send(Item::Notif(notification)) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            log::warn!(
                "dropped mux notification because dispatch item queue is full (capacity {DISPATCH_ITEM_QUEUE_CAPACITY})"
            );
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
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

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Clone, Copy)]
struct DispatchRawFdSource {
    raw_fd: RawFd,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl std::os::fd::AsRawFd for DispatchRawFdSource {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Default)]
struct DispatchIoUringWaiter {
    outcome: ParkingMutex<Option<std::io::Result<()>>>,
    task_waker: ParkingMutex<Option<Waker>>,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl DispatchIoUringWaiter {
    fn set_task_waker(&self, waker: &Waker) {
        let mut slot = self.task_waker.lock();
        if slot
            .as_ref()
            .is_none_or(|existing| !existing.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
    }

    fn finish(&self, result: std::io::Result<()>) {
        {
            let mut outcome = self.outcome.lock();
            if outcome.is_none() {
                *outcome = Some(result);
            }
        }

        if let Some(waker) = self.task_waker.lock().as_ref().cloned() {
            waker.wake();
        }
    }

    fn take_outcome(&self) -> Option<std::io::Result<()>> {
        self.outcome.lock().take()
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl ArcWake for DispatchIoUringWaiter {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.finish(Ok(()));
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Default)]
struct DispatchIoUringRuntimeState {
    waiters: ParkingMutex<Vec<std::sync::Weak<DispatchIoUringWaiter>>>,
    poll_error: ParkingMutex<Option<(std::io::ErrorKind, String)>>,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl DispatchIoUringRuntimeState {
    fn track_waiter(&self, waiter: &Arc<DispatchIoUringWaiter>) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|existing| existing.strong_count() > 0);
        waiters.push(Arc::downgrade(waiter));
    }

    fn poll_error(&self) -> Option<std::io::Error> {
        self.poll_error
            .lock()
            .as_ref()
            .map(|(kind, message)| std::io::Error::new(*kind, message.clone()))
    }

    fn fail_waiters(&self, kind: std::io::ErrorKind, message: String) {
        *self.poll_error.lock() = Some((kind, message.clone()));
        let mut waiters = self.waiters.lock();
        waiters.retain(|waiter| {
            if let Some(waiter) = waiter.upgrade() {
                waiter.finish(Err(std::io::Error::new(kind, message.clone())));
                true
            } else {
                false
            }
        });
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
struct DispatchIoUringRuntime {
    driver: IoDriverHandle,
    state: Arc<DispatchIoUringRuntimeState>,
    shutdown: Arc<AtomicBool>,
    poller: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(all(feature = "io-uring", target_os = "linux")))]
struct DispatchIoUringRuntime;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl DispatchIoUringRuntime {
    fn maybe_new(reactor: DispatchReactor, raw_fd: Option<RawFd>) -> Option<Self> {
        if reactor.backend() != DispatchIoBackend::IoUring || raw_fd.is_none() {
            return None;
        }

        match Self::new() {
            Ok(runtime) => Some(runtime),
            Err(err) => {
                log::warn!(
                    "io_uring mux dispatch backend selected but runtime init failed; falling back to readiness path: {err}"
                );
                None
            }
        }
    }

    fn new() -> std::io::Result<Self> {
        let reactor = Arc::new(IoUringReactor::new()?);
        let driver = IoDriverHandle::new(reactor);
        let state = Arc::new(DispatchIoUringRuntimeState::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let poll_driver = driver.clone();
        let poll_state = Arc::clone(&state);
        let poll_shutdown = Arc::clone(&shutdown);
        let poller = std::thread::Builder::new()
            .name("ft-mux-io-uring".to_string())
            .spawn(move || {
                while !poll_shutdown.load(Ordering::Acquire) {
                    match poll_driver.turn(None) {
                        Ok(_) => {}
                        Err(err) => {
                            poll_state.fail_waiters(
                                err.kind(),
                                format!("io_uring mux dispatch poll failed: {err}"),
                            );
                            break;
                        }
                    }
                }
            })
            .map_err(std::io::Error::other)?;

        Ok(Self {
            driver,
            state,
            shutdown,
            poller: Some(poller),
        })
    }

    fn wait_for_fd(&self, raw_fd: RawFd, interest: Interest) -> DispatchIoUringWaitFuture<'_> {
        DispatchIoUringWaitFuture {
            runtime: self,
            raw_fd,
            interest,
            waiter: Arc::new(DispatchIoUringWaiter::default()),
            registration: None,
        }
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl Drop for DispatchIoUringRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.driver.wake();
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
struct DispatchIoUringWaitFuture<'a> {
    runtime: &'a DispatchIoUringRuntime,
    raw_fd: RawFd,
    interest: Interest,
    waiter: Arc<DispatchIoUringWaiter>,
    registration: Option<asupersync::runtime::IoRegistration>,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl Future for DispatchIoUringWaitFuture<'_> {
    type Output = std::io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.waiter.take_outcome() {
            self.registration.take();
            return Poll::Ready(result);
        }

        if let Some(err) = self.runtime.state.poll_error() {
            return Poll::Ready(Err(err));
        }

        self.waiter.set_task_waker(cx.waker());

        if self.registration.is_none() {
            self.runtime.state.track_waiter(&self.waiter);
            let source = DispatchRawFdSource {
                raw_fd: self.raw_fd,
            };
            self.registration = Some(self.runtime.driver.register(
                &source,
                self.interest,
                waker(Arc::clone(&self.waiter)),
            )?);
        }

        if let Some(result) = self.waiter.take_outcome() {
            self.registration.take();
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}

async fn wait_for_dispatch_readable<T>(
    stream: &T,
    _io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> std::io::Result<()>
where
    T: DispatchStream,
{
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    if let (Some(runtime), Some(raw_fd)) = (_io_uring_runtime, stream.io_uring_fd()) {
        return runtime.wait_for_fd(raw_fd, Interest::READABLE).await;
    }

    stream.wait_for_readable().await
}

async fn wait_for_dispatch_writable<T>(
    stream: &T,
    _io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> std::io::Result<()>
where
    T: DispatchStream,
{
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    if let (Some(runtime), Some(raw_fd)) = (_io_uring_runtime, stream.io_uring_fd()) {
        return runtime.wait_for_fd(raw_fd, Interest::WRITABLE).await;
    }

    stream.wait_for_writable().await
}

async fn write_pending_pdus<T>(
    stream: &mut T,
    first: Box<DecodedPdu>,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    write_pending_pdus_with_compression_mode(
        stream,
        first,
        item_rx,
        deferred_item,
        io_uring_runtime,
        codec::CompressionMode::Auto,
    )
    .await
}

async fn write_pending_pdus_with_compression_mode<T>(
    stream: &mut T,
    first: Box<DecodedPdu>,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
    compression_mode: codec::CompressionMode,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    wait_for_dispatch_writable(stream, io_uring_runtime)
        .await
        .context("waiting for mux stream to become writable")?;

    let mut current = Some(first);
    loop {
        let Some(decoded) = current.take() else {
            break;
        };
        let mut frame = Vec::new();
        decoded
            .pdu
            .encode_with_mode(&mut frame, decoded.serial, compression_mode)
            .context("encoding PDU frame")?;
        write_frame_with_transient_retries(stream, &frame, io_uring_runtime)
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

fn is_transient_write_error(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock)
}

async fn write_frame_with_transient_retries<T>(
    stream: &mut T,
    frame: &[u8],
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    let mut offset = 0;
    let mut transient_retries = 0;
    while offset < frame.len() {
        match stream.write(&frame[offset..]).await {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write complete mux PDU frame",
                )
                .into());
            }
            Ok(written) => {
                offset += written;
                transient_retries = 0;
            }
            Err(err)
                if is_transient_write_error(&err)
                    && transient_retries < TRANSIENT_WRITE_RETRY_LIMIT =>
            {
                transient_retries += 1;
                wait_for_dispatch_writable(stream, io_uring_runtime)
                    .await
                    .context("waiting to retry transient mux stream write failure")?;
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(())
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

pub async fn process_async<T>(stream: T) -> anyhow::Result<()>
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

    let (item_tx, item_rx) = bounded::<Item>(DISPATCH_ITEM_QUEUE_CAPACITY);
    let mut deferred_item = None;
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    let io_uring_runtime = DispatchIoUringRuntime::maybe_new(reactor, stream.io_uring_fd());
    #[cfg(not(all(feature = "io-uring", target_os = "linux")))]
    let io_uring_runtime: Option<DispatchIoUringRuntime> = None;

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| queue_pdu(&item_tx, pdu.pdu, pdu.serial)
    });
    let mut handler = SessionHandler::new(pdu_sender);

    {
        let mux = Mux::try_get().context("mux singleton is not available")?;
        let tx = item_tx.clone();
        let sub_id = mux.subscribe(move |n| queue_notification(&tx, n));
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
                        let wait_for_read = wait_for_dispatch_readable(
                            &stream,
                            io_uring_runtime.as_ref(),
                        )
                        .map(|result| result.map(|()| Item::Readable).map_err(anyhow::Error::from));

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
                            if is_clean_disconnect(&err) {
                                // Client disconnected: no need to make a noise.
                                return Ok(());
                            }
                            return Err(err).context("reading Pdu from client");
                        }
                    };
                    handler.process_one(decoded);
                }
                Ok(Item::WritePdu(decoded)) => {
                    if let Err(err) = write_pending_pdus(
                        &mut stream,
                        decoded,
                        &item_rx,
                        &mut deferred_item,
                        io_uring_runtime.as_ref(),
                    )
                    .await
                    {
                        if is_clean_disconnect(&err) {
                            return Ok(());
                        }
                        return Err(err);
                    }
                }
                Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                    handler.schedule_tracked_pane_push(pane_id);
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
                        handler.schedule_tracked_pane_push(pane_id);
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
                    let workspace = if let Some(mux) = Mux::try_get() {
                        mux.get_window(window_id)
                            .map(|w| w.get_workspace().to_string())
                    } else {
                        None
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
                    if is_clean_disconnect(&err) {
                        return Ok(());
                    }
                    return Err(err).context("waiting for mux stream readiness or dispatch item");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
    use async_channel::unbounded;
    use codec::{CompressionMode, Ping, Pong, SendPaste, WriteToPane};
    use proptest::prelude::*;
    use std::io;
    use std::io::Cursor;
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    use std::io::Write;
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use wezterm_term::terminal::{Alert, ClipboardSelection};

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

    #[derive(Debug)]
    struct PartialFrameDisconnectStream {
        frame_prefix: Vec<u8>,
        cursor: usize,
        chunk_size: usize,
    }

    impl PartialFrameDisconnectStream {
        fn new(frame_prefix: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                frame_prefix,
                cursor: 0,
                chunk_size: chunk_size.max(1),
            }
        }
    }

    impl DispatchStream for PartialFrameDisconnectStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for PartialFrameDisconnectStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let available = this.frame_prefix.len().saturating_sub(this.cursor);
            if available == 0 {
                return Poll::Ready(Ok(()));
            }
            let want = buf.remaining().min(available).min(this.chunk_size);
            if want == 0 {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&this.frame_prefix[this.cursor..this.cursor + want]);
            this.cursor += want;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for PartialFrameDisconnectStream {
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
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .expect("global test lock");
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(EofDispatchStream));
        assert!(
            result.is_ok(),
            "EOF should be treated as a normal client disconnect"
        );
    }

    #[derive(Debug)]
    struct ReadErrorDispatchStream {
        kind: io::ErrorKind,
    }

    impl DispatchStream for ReadErrorDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for ReadErrorDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(self.kind, "client disconnected")))
        }
    }

    impl AsyncWrite for ReadErrorDispatchStream {
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
    fn process_async_treats_read_side_connection_reset_as_clean_disconnect() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .expect("global test lock");
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(ReadErrorDispatchStream {
            kind: io::ErrorKind::ConnectionReset,
        }));
        assert!(
            result.is_ok(),
            "read-side connection reset should be treated as a normal client disconnect"
        );
    }

    #[derive(Debug, Default)]
    struct FailingReadableDispatchStream;

    impl DispatchStream for FailingReadableDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Err(io::Error::other("readable wait failed")) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for FailingReadableDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other(
                "poll_read unexpectedly ran before the readiness error surfaced",
            )))
        }
    }

    impl AsyncWrite for FailingReadableDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other(
                "poll_write unexpectedly ran in the readability failure test",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn process_async_propagates_readable_wait_failures() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .expect("global test lock");
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(FailingReadableDispatchStream));
        let err = result.expect_err("readable wait failures must not be swallowed");
        let message = format!("{err:#}");
        assert!(
            message.contains("readable wait failed"),
            "error should preserve the readiness failure context: {message}"
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

    #[derive(Debug, Default)]
    struct RecordingDispatchStream {
        bytes: Vec<u8>,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl DispatchStream for RecordingDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for RecordingDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for RecordingDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes.extend_from_slice(buf);
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

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GeneratedOutboundPdu {
        Ping,
        Pong,
        WriteToPane { pane_id: usize, data: Vec<u8> },
        SendPaste { pane_id: usize, data: String },
    }

    impl GeneratedOutboundPdu {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::Ping => Pdu::Ping(Ping {}),
                Self::Pong => Pdu::Pong(Pong {}),
                Self::WriteToPane { pane_id, data } => {
                    Pdu::WriteToPane(WriteToPane { pane_id, data })
                }
                Self::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste { pane_id, data }),
            }
        }

        fn matches_decoded(&self, decoded: &Pdu) -> bool {
            match (self, decoded) {
                (Self::Ping, Pdu::Ping(Ping {})) | (Self::Pong, Pdu::Pong(Pong {})) => true,
                (
                    Self::WriteToPane { pane_id, data },
                    Pdu::WriteToPane(WriteToPane {
                        pane_id: decoded_pane_id,
                        data: decoded_data,
                    }),
                ) => pane_id == decoded_pane_id && data == decoded_data,
                (
                    Self::SendPaste { pane_id, data },
                    Pdu::SendPaste(SendPaste {
                        pane_id: decoded_pane_id,
                        data: decoded_data,
                    }),
                ) => pane_id == decoded_pane_id && data == decoded_data,
                _ => false,
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GeneratedMuxNotification {
        Empty,
        PaneRemoved {
            pane_id: usize,
        },
        AlertPaletteChanged {
            pane_id: usize,
        },
        AlertBell {
            pane_id: usize,
        },
        PaneFocused {
            pane_id: usize,
        },
        TabResized {
            tab_id: usize,
        },
        TabTitleChanged {
            tab_id: usize,
            title: String,
        },
        WindowTitleChanged {
            window_id: usize,
            title: String,
        },
        WorkspaceRenamed {
            old_workspace: String,
            new_workspace: String,
        },
        AssignClipboard {
            pane_id: usize,
            selection: ClipboardSelection,
            clipboard: Option<String>,
        },
    }

    impl GeneratedMuxNotification {
        fn into_notification(self) -> MuxNotification {
            match self {
                Self::Empty => MuxNotification::Empty,
                Self::PaneRemoved { pane_id } => MuxNotification::PaneRemoved(pane_id),
                Self::AlertPaletteChanged { pane_id } => MuxNotification::Alert {
                    pane_id,
                    alert: Alert::PaletteChanged,
                },
                Self::AlertBell { pane_id } => MuxNotification::Alert {
                    pane_id,
                    alert: Alert::Bell,
                },
                Self::PaneFocused { pane_id } => MuxNotification::PaneFocused(pane_id),
                Self::TabResized { tab_id } => MuxNotification::TabResized(tab_id),
                Self::TabTitleChanged { tab_id, title } => {
                    MuxNotification::TabTitleChanged { tab_id, title }
                }
                Self::WindowTitleChanged { window_id, title } => {
                    MuxNotification::WindowTitleChanged { window_id, title }
                }
                Self::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                } => MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                },
                Self::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                } => MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                },
            }
        }
    }

    fn queued_generated_pdu(serial: u64, pdu: GeneratedOutboundPdu) -> Box<DecodedPdu> {
        Box::new(DecodedPdu {
            pdu: pdu.into_pdu(),
            serial,
        })
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum NetworkWriteFailure {
        BrokenPipe,
        ConnectionReset,
        NotConnected,
        UnexpectedEof,
        TimedOut,
        WouldBlock,
        Other,
    }

    impl NetworkWriteFailure {
        fn kind(self) -> io::ErrorKind {
            match self {
                Self::BrokenPipe => io::ErrorKind::BrokenPipe,
                Self::ConnectionReset => io::ErrorKind::ConnectionReset,
                Self::NotConnected => io::ErrorKind::NotConnected,
                Self::UnexpectedEof => io::ErrorKind::UnexpectedEof,
                Self::TimedOut => io::ErrorKind::TimedOut,
                Self::WouldBlock => io::ErrorKind::WouldBlock,
                Self::Other => io::ErrorKind::Other,
            }
        }

        const fn is_transient(self) -> bool {
            matches!(self, Self::WouldBlock)
        }

        const fn is_clean_disconnect(self) -> bool {
            matches!(
                self,
                Self::BrokenPipe | Self::ConnectionReset | Self::NotConnected | Self::UnexpectedEof
            )
        }
    }

    fn network_write_failures() -> impl Strategy<Value = NetworkWriteFailure> {
        prop_oneof![
            Just(NetworkWriteFailure::BrokenPipe),
            Just(NetworkWriteFailure::ConnectionReset),
            Just(NetworkWriteFailure::NotConnected),
            Just(NetworkWriteFailure::UnexpectedEof),
            Just(NetworkWriteFailure::TimedOut),
            Just(NetworkWriteFailure::WouldBlock),
            Just(NetworkWriteFailure::Other),
        ]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TransientWriteFailure {
        Interrupted,
        WouldBlock,
    }

    impl TransientWriteFailure {
        const fn kind(self) -> io::ErrorKind {
            match self {
                Self::Interrupted => io::ErrorKind::Interrupted,
                Self::WouldBlock => io::ErrorKind::WouldBlock,
            }
        }
    }

    fn transient_write_failures() -> impl Strategy<Value = TransientWriteFailure> {
        prop_oneof![
            Just(TransientWriteFailure::Interrupted),
            Just(TransientWriteFailure::WouldBlock),
        ]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PartialFrameFailure {
        WriteZero,
        UnexpectedEof,
        BrokenPipe,
        ConnectionReset,
        Other,
    }

    impl PartialFrameFailure {
        fn result(self) -> io::Result<usize> {
            match self {
                Self::WriteZero => Ok(0),
                Self::UnexpectedEof => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "simulated EOF after partial frame write",
                )),
                Self::BrokenPipe => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "simulated broken pipe after partial frame write",
                )),
                Self::ConnectionReset => Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "simulated connection reset after partial frame write",
                )),
                Self::Other => Err(io::Error::other(
                    "simulated non-disconnect error after partial frame write",
                )),
            }
        }

        const fn is_clean_disconnect(self) -> bool {
            matches!(
                self,
                Self::UnexpectedEof | Self::BrokenPipe | Self::ConnectionReset
            )
        }
    }

    fn partial_frame_failures() -> impl Strategy<Value = PartialFrameFailure> {
        prop_oneof![
            Just(PartialFrameFailure::WriteZero),
            Just(PartialFrameFailure::UnexpectedEof),
            Just(PartialFrameFailure::BrokenPipe),
            Just(PartialFrameFailure::ConnectionReset),
            Just(PartialFrameFailure::Other),
        ]
    }

    #[derive(Debug)]
    struct FailingWriteDispatchStream {
        failure: NetworkWriteFailure,
        write_attempts: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl FailingWriteDispatchStream {
        fn new(failure: NetworkWriteFailure) -> Self {
            Self {
                failure,
                write_attempts: AtomicUsize::new(0),
                flush_calls: AtomicUsize::new(0),
                writable_waits: AtomicUsize::new(0),
            }
        }
    }

    impl DispatchStream for FailingWriteDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for FailingWriteDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FailingWriteDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_attempts.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Err(io::Error::new(
                this.failure.kind(),
                "simulated network write failure",
            )))
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

    #[derive(Debug)]
    struct TransientThenSuccessfulWriteDispatchStream {
        transient: TransientWriteFailure,
        failures_remaining: usize,
        bytes: Vec<u8>,
        write_attempts: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl TransientThenSuccessfulWriteDispatchStream {
        fn new(transient: TransientWriteFailure, failures_before_success: usize) -> Self {
            Self {
                transient,
                failures_remaining: failures_before_success,
                bytes: Vec::new(),
                write_attempts: AtomicUsize::new(0),
                flush_calls: AtomicUsize::new(0),
                writable_waits: AtomicUsize::new(0),
            }
        }
    }

    impl DispatchStream for TransientThenSuccessfulWriteDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for TransientThenSuccessfulWriteDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for TransientThenSuccessfulWriteDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_attempts.fetch_add(1, Ordering::Relaxed);
            if this.failures_remaining > 0 {
                this.failures_remaining -= 1;
                return Poll::Ready(Err(io::Error::new(
                    this.transient.kind(),
                    "simulated transient network write failure",
                )));
            }

            this.bytes.extend_from_slice(buf);
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

    #[derive(Debug)]
    struct PartialFrameFailingDispatchStream {
        fail_after_bytes: usize,
        failure: PartialFrameFailure,
        bytes: Vec<u8>,
        write_attempts: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl PartialFrameFailingDispatchStream {
        fn new(fail_after_bytes: usize, failure: PartialFrameFailure) -> Self {
            Self {
                fail_after_bytes,
                failure,
                bytes: Vec::new(),
                write_attempts: AtomicUsize::new(0),
                flush_calls: AtomicUsize::new(0),
                writable_waits: AtomicUsize::new(0),
            }
        }
    }

    impl DispatchStream for PartialFrameFailingDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for PartialFrameFailingDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for PartialFrameFailingDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_attempts.fetch_add(1, Ordering::Relaxed);

            if this.bytes.len() < this.fail_after_bytes {
                let remaining_prefix = this.fail_after_bytes - this.bytes.len();
                let accepted = remaining_prefix.min(buf.len());
                this.bytes.extend_from_slice(&buf[..accepted]);
                return Poll::Ready(Ok(accepted));
            }

            Poll::Ready(this.failure.result())
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum QueuedDispatchItem {
        WritePdu,
        Notif,
        Readable,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum QueuedGeneratedDispatchItem {
        WritePdu(GeneratedOutboundPdu),
        Notif,
        Readable,
    }

    fn queued_dispatch_items() -> impl Strategy<Value = Vec<QueuedDispatchItem>> {
        proptest::collection::vec(
            prop_oneof![
                Just(QueuedDispatchItem::WritePdu),
                Just(QueuedDispatchItem::Notif),
                Just(QueuedDispatchItem::Readable),
            ],
            0..=16,
        )
    }

    fn generated_outbound_pdu() -> impl Strategy<Value = GeneratedOutboundPdu> {
        prop_oneof![
            Just(GeneratedOutboundPdu::Ping),
            Just(GeneratedOutboundPdu::Pong),
            (0usize..4096, proptest::collection::vec(any::<u8>(), 0..512))
                .prop_map(|(pane_id, data)| GeneratedOutboundPdu::WriteToPane { pane_id, data },),
            (0usize..4096, "[a-zA-Z0-9 _./-]{0,512}")
                .prop_map(|(pane_id, data)| { GeneratedOutboundPdu::SendPaste { pane_id, data } }),
        ]
    }

    fn compression_modes() -> impl Strategy<Value = CompressionMode> {
        prop_oneof![
            Just(CompressionMode::Auto),
            Just(CompressionMode::Always),
            Just(CompressionMode::Never),
        ]
    }

    fn queued_generated_dispatch_items() -> impl Strategy<Value = Vec<QueuedGeneratedDispatchItem>>
    {
        proptest::collection::vec(
            prop_oneof![
                generated_outbound_pdu().prop_map(QueuedGeneratedDispatchItem::WritePdu),
                Just(QueuedGeneratedDispatchItem::Notif),
                Just(QueuedGeneratedDispatchItem::Readable),
            ],
            0..=16,
        )
    }

    fn clipboard_selections() -> impl Strategy<Value = ClipboardSelection> {
        prop_oneof![
            Just(ClipboardSelection::Clipboard),
            Just(ClipboardSelection::PrimarySelection),
        ]
    }

    fn generated_mux_notification() -> impl Strategy<Value = GeneratedMuxNotification> {
        let label = "[a-zA-Z0-9 _./-]{0,48}";
        prop_oneof![
            Just(GeneratedMuxNotification::Empty),
            (0usize..4096).prop_map(|pane_id| GeneratedMuxNotification::PaneRemoved { pane_id }),
            (0usize..4096)
                .prop_map(|pane_id| GeneratedMuxNotification::AlertPaletteChanged { pane_id }),
            (0usize..4096).prop_map(|pane_id| GeneratedMuxNotification::AlertBell { pane_id }),
            (0usize..4096).prop_map(|pane_id| GeneratedMuxNotification::PaneFocused { pane_id }),
            (0usize..256).prop_map(|tab_id| GeneratedMuxNotification::TabResized { tab_id }),
            (0usize..256, label).prop_map(|(tab_id, title)| {
                GeneratedMuxNotification::TabTitleChanged { tab_id, title }
            }),
            (0usize..256, label).prop_map(|(window_id, title)| {
                GeneratedMuxNotification::WindowTitleChanged { window_id, title }
            }),
            (label, label).prop_map(|(old_workspace, new_workspace)| {
                GeneratedMuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                }
            }),
            (
                0usize..4096,
                clipboard_selections(),
                prop::option::of(label)
            )
                .prop_map(|(pane_id, selection, clipboard)| {
                    GeneratedMuxNotification::AssignClipboard {
                        pane_id,
                        selection,
                        clipboard,
                    }
                },),
        ]
    }

    fn classify_item(item: &Item) -> QueuedDispatchItem {
        match item {
            Item::WritePdu(_) => QueuedDispatchItem::WritePdu,
            Item::Notif(_) => QueuedDispatchItem::Notif,
            Item::Readable => QueuedDispatchItem::Readable,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn proptest_write_pending_pdus_preserves_serial_order_and_deferred_boundary(
            queued_items in queued_dispatch_items()
        ) {
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_written_serials = vec![1_u64];
            let mut expected_deferred = None;
            let mut expected_remaining = Vec::new();
            let mut still_in_write_prefix = true;

            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(Item::WritePdu(queued_ping(next_serial)))
                            .expect("queue generated ping");
                        if still_in_write_prefix {
                            expected_written_serials.push(next_serial);
                        } else {
                            expected_remaining.push(QueuedDispatchItem::WritePdu);
                        }
                        next_serial += 1;
                    }
                    QueuedDispatchItem::Notif => {
                        item_tx
                            .try_send(Item::Notif(MuxNotification::Empty))
                            .expect("queue generated notification");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Notif);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Notif);
                        }
                    }
                    QueuedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Readable);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Readable);
                        }
                    }
                }
            }

            let mut deferred_item = None;
            let mut stream = RecordingDispatchStream::default();
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_ping(1),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            prop_assert!(result.is_ok(), "batched write helper should succeed: {result:?}");
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                1,
                "batched writes should flush exactly once"
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                1,
                "batched writes should wait for writability exactly once"
            );

            let mut decoded_serials = Vec::new();
            let mut cursor = Cursor::new(stream.bytes.as_slice());
            while (cursor.position() as usize) < stream.bytes.len() {
                let decoded = Pdu::decode(&mut cursor).expect("decode recorded outbound PDU");
                prop_assert!(
                    matches!(decoded.pdu, Pdu::Ping(_)),
                    "generated dispatch test should only encode Ping PDUs"
                );
                decoded_serials.push(decoded.serial);
            }

            prop_assert_eq!(
                decoded_serials,
                expected_written_serials,
                "write batch must preserve the contiguous outbound PDU serial order"
            );
            prop_assert_eq!(deferred_item.as_ref().map(classify_item), expected_deferred);

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "items after the first deferred non-write must stay queued in original order"
            );
        }

        #[test]
        fn proptest_write_pending_pdus_preserves_dispatch_order_across_compression_modes(
            compression_mode in compression_modes(),
            first_pdu in generated_outbound_pdu(),
            queued_items in queued_generated_dispatch_items(),
            first_serial in any::<u16>(),
        ) {
            let first_serial = u64::from(first_serial);
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = first_serial.saturating_add(1);
            let mut expected_written = vec![(first_serial, first_pdu.clone())];
            let mut expected_deferred = None;
            let mut expected_remaining = Vec::new();
            let mut still_in_write_prefix = true;

            for queued_item in queued_items {
                match queued_item {
                    QueuedGeneratedDispatchItem::WritePdu(pdu) => {
                        item_tx
                            .try_send(Item::WritePdu(queued_generated_pdu(next_serial, pdu.clone())))
                            .expect("queue generated PDU");
                        if still_in_write_prefix {
                            expected_written.push((next_serial, pdu));
                        } else {
                            expected_remaining.push(QueuedDispatchItem::WritePdu);
                        }
                        next_serial = next_serial.saturating_add(1);
                    }
                    QueuedGeneratedDispatchItem::Notif => {
                        item_tx
                            .try_send(Item::Notif(MuxNotification::Empty))
                            .expect("queue generated notification");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Notif);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Notif);
                        }
                    }
                    QueuedGeneratedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Readable);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Readable);
                        }
                    }
                }
            }

            let mut deferred_item = None;
            let mut stream = RecordingDispatchStream::default();
            let result = promise::spawn::block_on(write_pending_pdus_with_compression_mode(
                &mut stream,
                queued_generated_pdu(first_serial, first_pdu),
                &item_rx,
                &mut deferred_item,
                None,
                compression_mode,
            ));

            prop_assert!(
                result.is_ok(),
                "batched write helper should succeed under {:?}: {:?}",
                compression_mode,
                result
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                1,
                "batched writes should flush exactly once under {:?}",
                compression_mode
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                1,
                "batched writes should wait for writability exactly once under {:?}",
                compression_mode
            );

            let mut decoded = Vec::new();
            let mut cursor = Cursor::new(stream.bytes.as_slice());
            while (cursor.position() as usize) < stream.bytes.len() {
                decoded.push(Pdu::decode(&mut cursor).expect("decode recorded outbound PDU"));
            }

            prop_assert_eq!(
                decoded.len(),
                expected_written.len(),
                "compression mode {:?} must not change dispatch frame count",
                compression_mode
            );
            for (decoded, (expected_serial, expected_pdu)) in
                decoded.iter().zip(expected_written.iter())
            {
                prop_assert_eq!(
                    decoded.serial,
                    *expected_serial,
                    "compression mode {:?} changed outbound serial order",
                    compression_mode
                );
                prop_assert!(
                    expected_pdu.matches_decoded(&decoded.pdu),
                    "compression mode {:?} changed outbound PDU payload: expected {:?}, got {:?}",
                    compression_mode,
                    expected_pdu,
                    decoded.pdu
                );
            }
            prop_assert_eq!(deferred_item.as_ref().map(classify_item), expected_deferred);

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "compression mode {:?} must preserve queued items after the first deferred non-write",
                compression_mode
            );
        }

        #[test]
        fn proptest_process_async_treats_partial_inbound_frame_disconnect_as_clean(
            compression_mode in compression_modes(),
            inbound_pdu in generated_outbound_pdu(),
            serial in any::<u16>(),
            cut_seed in any::<usize>(),
            chunk_size in 1usize..64,
        ) {
            let mut encoded = Vec::new();
            inbound_pdu
                .clone()
                .into_pdu()
                .encode_with_mode(&mut encoded, u64::from(serial), compression_mode)
                .expect("generated inbound PDU should encode");
            prop_assume!(encoded.len() > 1);
            let cut = 1 + (cut_seed % (encoded.len() - 1));
            let frame_prefix = encoded[..cut].to_vec();

            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .expect("global test lock");
            let mux = Arc::new(Mux::new(None));
            let _scoped_mux = ScopedMux::install(&mux);
            let result = promise::spawn::block_on(process_async(PartialFrameDisconnectStream::new(
                frame_prefix,
                chunk_size,
            )));

            prop_assert!(
                result.is_ok(),
                "partial inbound frame disconnect should be clean for {:?} {:?} cut {}/{} chunk {}: {:?}",
                compression_mode,
                inbound_pdu,
                cut,
                encoded.len(),
                chunk_size,
                result
            );
        }

        #[test]
        fn proptest_notification_error_bus_subscription_liveness_contract(
            notification in generated_mux_notification()
        ) {
            let notification = notification.into_notification();

            let (full_tx, full_rx) = bounded(1);
            full_tx
                .try_send(Item::Readable)
                .expect("fill dispatch notification queue");
            prop_assert!(
                queue_notification(&full_tx, notification.clone()),
                "full notification queue should report a live subscription"
            );
            prop_assert!(
                matches!(full_rx.try_recv(), Ok(Item::Readable)),
                "dropped notification must not displace the older queued item"
            );
            prop_assert!(
                matches!(full_rx.try_recv(), Err(TryRecvError::Empty)),
                "full queue path should drop only the new notification"
            );

            let mux = Mux::new(None);
            let full_calls = Arc::new(AtomicUsize::new(0));
            let full_calls_for_subscriber = Arc::clone(&full_calls);
            full_tx
                .try_send(Item::Readable)
                .expect("refill dispatch notification queue");
            mux.subscribe(move |notification| {
                full_calls_for_subscriber.fetch_add(1, Ordering::Relaxed);
                queue_notification(&full_tx, notification)
            });
            mux.notify(notification.clone());
            mux.notify(notification.clone());
            prop_assert_eq!(
                full_calls.load(Ordering::Relaxed),
                2,
                "backpressure-only notification failures must not unsubscribe the mux subscriber"
            );

            let (closed_tx, closed_rx) = bounded(1);
            drop(closed_rx);
            prop_assert!(
                !queue_notification(&closed_tx, notification.clone()),
                "closed notification queue should report a dead subscription"
            );

            let mux = Mux::new(None);
            let closed_calls = Arc::new(AtomicUsize::new(0));
            let closed_calls_for_subscriber = Arc::clone(&closed_calls);
            mux.subscribe(move |notification| {
                closed_calls_for_subscriber.fetch_add(1, Ordering::Relaxed);
                queue_notification(&closed_tx, notification)
            });
            mux.notify(notification.clone());
            mux.notify(notification);
            prop_assert_eq!(
                closed_calls.load(Ordering::Relaxed),
                1,
                "closed notification queue should remove the mux subscriber after the first failed delivery"
            );
        }

        #[test]
        fn proptest_write_pending_pdus_retries_transient_network_errors_without_reordering(
            transient in transient_write_failures(),
            failures_before_success in 1usize..=TRANSIENT_WRITE_RETRY_LIMIT,
            first_pdu in generated_outbound_pdu(),
            queued_items in queued_generated_dispatch_items(),
            first_serial in any::<u16>(),
        ) {
            let first_serial = u64::from(first_serial);
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = first_serial.saturating_add(1);
            let mut expected_written = vec![(first_serial, first_pdu.clone())];
            let mut expected_deferred = None;
            let mut expected_remaining = Vec::new();
            let mut still_in_write_prefix = true;

            for queued_item in queued_items {
                match queued_item {
                    QueuedGeneratedDispatchItem::WritePdu(pdu) => {
                        item_tx
                            .try_send(Item::WritePdu(queued_generated_pdu(next_serial, pdu.clone())))
                            .expect("queue generated PDU");
                        if still_in_write_prefix {
                            expected_written.push((next_serial, pdu));
                        } else {
                            expected_remaining.push(QueuedDispatchItem::WritePdu);
                        }
                        next_serial = next_serial.saturating_add(1);
                    }
                    QueuedGeneratedDispatchItem::Notif => {
                        item_tx
                            .try_send(Item::Notif(MuxNotification::Empty))
                            .expect("queue generated notification");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Notif);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Notif);
                        }
                    }
                    QueuedGeneratedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Readable);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Readable);
                        }
                    }
                }
            }

            let mut deferred_item = None;
            let mut stream = TransientThenSuccessfulWriteDispatchStream::new(
                transient,
                failures_before_success,
            );
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_generated_pdu(first_serial, first_pdu),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            prop_assert!(
                result.is_ok(),
                "transient {:?} write failures should retry and preserve dispatch: {:?}",
                transient,
                result
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                1,
                "successful retry path should flush exactly once"
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                failures_before_success + 1,
                "dispatch should wait once initially and once for each transient retry"
            );
            prop_assert_eq!(
                stream.write_attempts.load(Ordering::Relaxed),
                failures_before_success + expected_written.len(),
                "dispatch should retry only the failed frame before continuing in order"
            );

            let mut decoded = Vec::new();
            let mut cursor = Cursor::new(stream.bytes.as_slice());
            while (cursor.position() as usize) < stream.bytes.len() {
                decoded.push(Pdu::decode(&mut cursor).expect("decode recorded outbound PDU"));
            }

            prop_assert_eq!(
                decoded.len(),
                expected_written.len(),
                "transient retry path must not drop or duplicate dispatch frames"
            );
            for (decoded, (expected_serial, expected_pdu)) in
                decoded.iter().zip(expected_written.iter())
            {
                prop_assert_eq!(
                    decoded.serial,
                    *expected_serial,
                    "transient retry path changed outbound serial order"
                );
                prop_assert!(
                    expected_pdu.matches_decoded(&decoded.pdu),
                    "transient retry path changed outbound PDU payload: expected {:?}, got {:?}",
                    expected_pdu,
                    decoded.pdu
                );
            }
            prop_assert_eq!(deferred_item.as_ref().map(classify_item), expected_deferred);

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "transient retry path must preserve queued items after the first deferred non-write",
            );
        }

        #[test]
        fn proptest_write_pending_pdus_classifies_network_write_failures_without_reordering(
            failure in network_write_failures(),
            queued_items in queued_dispatch_items(),
            first_serial in any::<u16>(),
        ) {
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_remaining = Vec::new();

            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(Item::WritePdu(queued_ping(next_serial)))
                            .expect("queue generated ping");
                        next_serial += 1;
                    }
                    QueuedDispatchItem::Notif => {
                        item_tx
                            .try_send(Item::Notif(MuxNotification::Empty))
                            .expect("queue generated notification");
                    }
                    QueuedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                    }
                }
                expected_remaining.push(queued_item);
            }

            let mut deferred_item = None;
            let mut stream = FailingWriteDispatchStream::new(failure);
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_ping(u64::from(first_serial)),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            let err = result.expect_err("network write failure should stop the PDU batch");
            let message = format!("{err:#}");
            prop_assert!(
                message.contains("encoding PDU to client"),
                "write-side network errors should retain dispatch context: {message}"
            );
            prop_assert_eq!(
                is_clean_disconnect(&err),
                failure.is_clean_disconnect(),
                "clean-disconnect classification should match network error kind"
            );
            let expected_write_attempts = if failure.is_transient() {
                TRANSIENT_WRITE_RETRY_LIMIT + 1
            } else {
                1
            };
            prop_assert_eq!(
                stream.write_attempts.load(Ordering::Relaxed),
                expected_write_attempts,
                "dispatch should bound retries for transient failures and stop immediately for hard failures"
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                0,
                "failed writes must not be followed by a flush"
            );
            let expected_writable_waits = if failure.is_transient() {
                TRANSIENT_WRITE_RETRY_LIMIT + 1
            } else {
                1
            };
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                expected_writable_waits,
                "dispatch should wait initially and before each transient retry"
            );
            prop_assert!(
                deferred_item.is_none(),
                "a write failure before queue draining must not manufacture a deferred item"
            );

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "queued dispatch items must remain ordered after a network write failure"
            );
        }

        #[test]
        fn proptest_write_pending_pdus_stops_on_partial_frame_eof_without_dropping_queue(
            failure in partial_frame_failures(),
            queued_items in queued_dispatch_items(),
            first_serial in any::<u16>(),
            cut_seed in any::<usize>(),
        ) {
            let serial = u64::from(first_serial);
            let mut full_frame = Vec::new();
            Pdu::Ping(Ping {})
                .encode(&mut full_frame, serial)
                .expect("generated Ping frame should encode");
            prop_assume!(full_frame.len() > 1);
            let fail_after_bytes = 1 + (cut_seed % (full_frame.len() - 1));

            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_remaining = Vec::new();
            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(Item::WritePdu(queued_ping(next_serial)))
                            .expect("queue generated ping");
                        next_serial += 1;
                    }
                    QueuedDispatchItem::Notif => {
                        item_tx
                            .try_send(Item::Notif(MuxNotification::Empty))
                            .expect("queue generated notification");
                    }
                    QueuedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                    }
                }
                expected_remaining.push(queued_item);
            }

            let mut deferred_item = None;
            let mut stream = PartialFrameFailingDispatchStream::new(fail_after_bytes, failure);
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_ping(serial),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            let err = result.expect_err("partial-frame EOF/write failure should stop dispatch");
            let message = format!("{err:#}");
            prop_assert!(
                message.contains("encoding PDU to client"),
                "partial-frame errors should retain dispatch context: {message}"
            );
            prop_assert_eq!(
                is_clean_disconnect(&err),
                failure.is_clean_disconnect(),
                "partial-frame EOF classification should match the write failure kind"
            );
            prop_assert_eq!(
                stream.bytes.as_slice(),
                &full_frame[..fail_after_bytes],
                "partial write must leave only the strict frame prefix on the stream"
            );
            prop_assert!(
                Pdu::decode(stream.bytes.as_slice()).is_err(),
                "strict partial frame prefix must not decode as a complete PDU"
            );
            prop_assert!(
                stream.write_attempts.load(Ordering::Relaxed) >= 2,
                "write_all should retry after the first short frame write"
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                0,
                "partial-frame failures must not be followed by a flush"
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                1,
                "dispatch should wait once for writability before attempting the frame"
            );
            prop_assert!(
                deferred_item.is_none(),
                "failure before queue draining must not manufacture a deferred item"
            );

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "queued dispatch items must remain ordered after partial-frame EOF"
            );
        }
    }

    #[test]
    fn queue_pdu_reports_full_dispatch_queue() {
        let (item_tx, item_rx) = bounded(1);
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        let err = queue_pdu(&item_tx, Pdu::Ping(Ping {}), 7)
            .expect_err("full queue should produce explicit backpressure error");
        let message = format!("{err:#}");
        assert!(
            message.contains("mux dispatch item queue is full"),
            "queue-full error should be specific: {message}"
        );
        assert!(
            message.contains(&DISPATCH_ITEM_QUEUE_CAPACITY.to_string()),
            "queue-full error should include the configured capacity: {message}"
        );
        assert!(
            matches!(item_rx.try_recv(), Ok(Item::Readable)),
            "failed enqueue must not disturb the queued item"
        );
    }

    #[test]
    fn full_notification_queue_keeps_mux_subscriber_alive() {
        let (item_tx, item_rx) = bounded(1);
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        assert!(
            queue_notification(&item_tx, MuxNotification::Empty),
            "full notification queues should drop the notification without unsubscribing"
        );
        assert!(
            matches!(item_rx.try_recv(), Ok(Item::Readable)),
            "dropped notification must not displace an older queued item"
        );
    }

    #[test]
    fn write_pending_pdus_batches_flush_and_preserves_first_non_write_item() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .expect("global test lock");
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
            None,
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

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    #[test]
    fn io_uring_runtime_waits_for_unix_fd_readability() {
        let runtime = match DispatchIoUringRuntime::new() {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("skipping io_uring readability test: {err}");
                return;
            }
        };
        let (left, mut right) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        left.set_nonblocking(true).expect("left nonblocking");
        right.set_nonblocking(true).expect("right nonblocking");

        let writer = std::thread::spawn(move || {
            right.write_all(b"ping").expect("write ping");
        });

        let result =
            promise::spawn::block_on(runtime.wait_for_fd(left.as_raw_fd(), Interest::READABLE));
        writer.join().expect("writer thread");
        assert!(
            result.is_ok(),
            "readability wait should succeed: {result:?}"
        );
    }
}
