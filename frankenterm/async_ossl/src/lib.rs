use openssl::ssl::{ErrorCode, ShutdownResult, SslStream};
#[cfg(feature = "async-asupersync")]
use std::future::poll_fn;
#[cfg(feature = "async-asupersync")]
use std::io::IoSlice;
use std::io::Write;
use std::net::TcpStream;
#[cfg(feature = "async-asupersync")]
use std::pin::Pin;
#[cfg(feature = "async-asupersync")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "async-asupersync")]
use std::sync::{Mutex, MutexGuard};
#[cfg(feature = "async-asupersync")]
use std::task::{Context, Poll};
#[cfg(feature = "async-asupersync")]
use std::time::Duration;

#[cfg(feature = "async-asupersync")]
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
#[cfg(feature = "async-asupersync")]
use asupersync::runtime::{Interest, IoRegistration};
#[cfg(feature = "async-asupersync")]
use asupersync::Cx;
#[cfg(feature = "async-asupersync")]
use futures::io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};

// async-asupersync is the default runtime surface. Legacy smol consumers still
// opt into async-io explicitly, and Cargo feature unification can enable both
// at once while the async-io IoSafe impl remains harmless.

#[cfg(feature = "async-asupersync")]
#[allow(dead_code)]
struct _AsupersyncDep(asupersync::io::IoNotAvailable);

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket {}

#[derive(Debug)]
pub struct AsyncSslStream {
    s: SslStream<TcpStream>,
    #[cfg(feature = "async-asupersync")]
    registration: Mutex<Option<IoRegistration>>,
    #[cfg(feature = "async-asupersync")]
    pending_outbound_retry: AtomicBool,
}

#[cfg(feature = "async-io")]
unsafe impl async_io::IoSafe for AsyncSslStream {}

#[cfg(feature = "async-asupersync")]
const FALLBACK_IO_BACKOFF: Duration = Duration::from_millis(1);

impl AsyncSslStream {
    pub fn new(s: SslStream<TcpStream>) -> Self {
        Self {
            s,
            #[cfg(feature = "async-asupersync")]
            registration: Mutex::new(None),
            #[cfg(feature = "async-asupersync")]
            pending_outbound_retry: AtomicBool::new(false),
        }
    }

    /// Probe for decrypted TLS bytes, encrypted socket bytes, or EOF without
    /// consuming input. This is intentionally synchronous and nonblocking so
    /// a dispatcher can check inbound work before servicing a hot internal
    /// queue without repeatedly allocating and discarding readiness futures.
    pub fn try_readable_without_consuming(&self) -> std::io::Result<bool> {
        // Callers can enter dispatch through the direct TLS path without the
        // protocol-detection prelude that normally makes the socket
        // nonblocking. Never let a readiness probe block an executor thread.
        self.s.get_ref().set_nonblocking(true)?;
        if self.s.ssl().pending() > 0 {
            return Ok(true);
        }

        let mut probe = [0_u8; 1];
        match self.s.get_ref().peek(&mut probe) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(false),
            Err(err) => Err(err),
        }
    }

    #[cfg(feature = "async-asupersync")]
    pub async fn wait_for_readable(&self) -> std::io::Result<()> {
        poll_fn(|cx| {
            match self.try_readable_without_consuming() {
                Ok(true) => return Poll::Ready(Ok(())),
                Ok(false) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }
            self.register_interest_for_read(cx)?;

            // Close the probe/register race: bytes can arrive between the
            // first peek and arming the reactor interest.
            match self.try_readable_without_consuming() {
                Ok(true) => Poll::Ready(Ok(())),
                Ok(false) => Poll::Pending,
                Err(err) => Poll::Ready(Err(err)),
            }
        })
        .await
    }

    /// Wait until either inbound data or outbound capacity may be available.
    ///
    /// A single combined registration is required because this wrapper owns
    /// one reactor registration slot. Racing separate read/write waiters would
    /// let the second waiter overwrite the first waiter's interest.
    #[cfg(feature = "async-asupersync")]
    pub async fn wait_for_readable_or_writable(&self) -> std::io::Result<()> {
        let mut armed = false;
        poll_fn(|cx| {
            match self.try_readable_without_consuming() {
                Ok(true) => return Poll::Ready(Ok(())),
                Ok(false) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }

            if armed {
                // The combined registration woke this task. The caller
                // re-probes readability before treating the wake as writable,
                // so a spurious wake cannot manufacture inbound work.
                return Poll::Ready(Ok(()));
            }

            self.register_interest_for_read_write(cx)?;
            armed = true;

            // Close the probe/register race for the read side.
            match self.try_readable_without_consuming() {
                Ok(true) => Poll::Ready(Ok(())),
                Ok(false) => Poll::Pending,
                Err(err) => Poll::Ready(Err(err)),
            }
        })
        .await
    }

    /// Whether the last TLS write/flush poll returned `WANT_READ` or
    /// `WANT_WRITE` and therefore must be retried before any other SSL I/O.
    #[cfg(feature = "async-asupersync")]
    pub fn pending_outbound_requires_retry(&self) -> bool {
        self.pending_outbound_retry.load(Ordering::Acquire)
    }

    /// Suspend on the exact reactor interest armed by the preceding SSL
    /// write/flush poll without overwriting it with a combined interest.
    #[cfg(feature = "async-asupersync")]
    pub async fn wait_for_pending_outbound_retry(&self) -> std::io::Result<()> {
        let mut suspended = false;
        poll_fn(|_| {
            if suspended {
                Poll::Ready(Ok(()))
            } else {
                suspended = true;
                Poll::Pending
            }
        })
        .await
    }

    #[cfg(feature = "async-asupersync")]
    pub async fn wait_for_writable(&self) -> std::io::Result<()> {
        let mut armed = false;
        poll_fn(|cx| {
            if armed {
                return Poll::Ready(Ok(()));
            }
            self.register_interest_for_write(cx)?;
            armed = true;
            Poll::Pending
        })
        .await
    }
}

#[cfg(feature = "async-asupersync")]
fn fallback_rewake(cx: &Context<'_>) {
    if let Some(timer) = Cx::current().and_then(|current| current.timer_driver()) {
        let deadline = timer.now() + FALLBACK_IO_BACKOFF;
        let _ = timer.register(deadline, cx.waker().clone());
    } else {
        cx.waker().wake_by_ref();
    }
}

#[cfg(feature = "async-asupersync")]
fn ssl_error_to_io(err: openssl::ssl::Error) -> std::io::Error {
    match err.into_io_error() {
        Ok(ioerr) => ioerr,
        Err(err) => std::io::Error::other(err),
    }
}

#[cfg(feature = "async-asupersync")]
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

#[cfg(unix)]
impl std::os::fd::AsFd for AsyncSslStream {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.s.get_ref().as_fd()
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for AsyncSslStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.s.get_ref().as_raw_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for AsyncSslStream {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.s.get_ref().as_raw_socket()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsSocket for AsyncSslStream {
    fn as_socket(&self) -> std::os::windows::io::BorrowedSocket<'_> {
        self.s.get_ref().as_socket()
    }
}

impl AsRawDesc for AsyncSslStream {}

impl std::io::Read for AsyncSslStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.s.read(buf)
    }
}

impl std::io::Write for AsyncSslStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.s.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.s.flush()
    }
}

#[cfg(feature = "async-asupersync")]
impl AsyncSslStream {
    fn ensure_nonblocking(&self) -> std::io::Result<()> {
        self.s.get_ref().set_nonblocking(true)
    }

    fn lock_registration(&self) -> MutexGuard<'_, Option<IoRegistration>> {
        lock_registration_mutex(&self.registration)
    }

    fn register_interest_for_read(&self, cx: &Context<'_>) -> std::io::Result<()> {
        self.register_interest(cx, Interest::READABLE)
    }

    fn register_interest_for_write(&self, cx: &Context<'_>) -> std::io::Result<()> {
        self.register_interest(cx, Interest::WRITABLE)
    }

    fn register_interest_for_read_write(&self, cx: &Context<'_>) -> std::io::Result<()> {
        self.register_interest(cx, Interest::READABLE | Interest::WRITABLE)
    }

    fn register_interest(&self, cx: &Context<'_>, interest: Interest) -> std::io::Result<()> {
        self.ensure_nonblocking()?;

        let mut registration = self.lock_registration();
        if let Some(existing) = registration.as_mut() {
            match existing.rearm(interest, cx.waker()) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    *registration = None;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotConnected => {
                    *registration = None;
                    drop(registration);
                    fallback_rewake(cx);
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }

        let Some(current) = Cx::current() else {
            drop(registration);
            fallback_rewake(cx);
            return Ok(());
        };

        // asupersync's `Cx::register_io` is gated `#[cfg(unix)]` — the
        // mio-style I/O driver only exists on Unix. On Windows we fall
        // through to the same `fallback_rewake` polling path the function
        // already takes when no driver is available; matches the
        // frankenterm-uds shape (see frankenterm/uds/src/lib.rs).
        #[cfg(unix)]
        {
            match current.register_io(self, interest) {
                Ok(new_registration) => {
                    let _ = new_registration.update_waker(cx.waker().clone());
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
                    fallback_rewake(cx);
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
        #[cfg(windows)]
        {
            let _ = (current, interest);
            drop(registration);
            fallback_rewake(cx);
            Ok(())
        }
    }
}

#[cfg(all(test, feature = "async-asupersync"))]
mod tests {
    use super::*;

    #[test]
    fn registration_lock_recovers_after_poison() {
        let registration = Mutex::new(None::<IoRegistration>);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registration.lock().unwrap();
            panic!("simulate async SSL registration lock poison");
        }));

        assert!(poisoned.is_err());
        assert!(registration.is_poisoned());

        {
            let guard = lock_registration_mutex(&registration);
            assert!(guard.is_none());
        }

        assert!(!registration.is_poisoned());
    }
}

#[cfg(feature = "async-asupersync")]
impl AsyncRead for AsyncSslStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.ssl_read(buf.unfilled()) {
            Ok(read) => {
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Err(err) if err.code() == ErrorCode::ZERO_RETURN => Poll::Ready(Ok(())),
            Err(err) if err.code() == ErrorCode::WANT_READ => {
                if let Err(register_err) = this.register_interest_for_read(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::WANT_WRITE => {
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(ssl_error_to_io(err))),
        }
    }
}

#[cfg(feature = "async-asupersync")]
impl AsyncWrite for AsyncSslStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.ssl_write(buf) {
            Ok(written) => {
                this.pending_outbound_retry.store(false, Ordering::Release);
                Poll::Ready(Ok(written))
            }
            Err(err) if err.code() == ErrorCode::WANT_WRITE => {
                this.pending_outbound_retry.store(true, Ordering::Release);
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    this.pending_outbound_retry.store(false, Ordering::Release);
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::WANT_READ => {
                this.pending_outbound_retry.store(true, Ordering::Release);
                if let Err(register_err) = this.register_interest_for_read(cx) {
                    this.pending_outbound_retry.store(false, Ordering::Release);
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::ZERO_RETURN => {
                this.pending_outbound_retry.store(false, Ordering::Release);
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "TLS session closed",
                )))
            }
            Err(err) => {
                this.pending_outbound_retry.store(false, Ordering::Release);
                Poll::Ready(Err(ssl_error_to_io(err)))
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(buf) = bufs.iter().find(|buf| !buf.is_empty()) {
            <Self as AsyncWrite>::poll_write(self, cx, buf)
        } else {
            Poll::Ready(Ok(0))
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.flush() {
            Ok(()) => {
                this.pending_outbound_retry.store(false, Ordering::Release);
                Poll::Ready(Ok(()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                this.pending_outbound_retry.store(true, Ordering::Release);
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    this.pending_outbound_retry.store(false, Ordering::Release);
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => {
                this.pending_outbound_retry.store(false, Ordering::Release);
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.shutdown() {
            Ok(ShutdownResult::Received | ShutdownResult::Sent) => Poll::Ready(Ok(())),
            Err(err) if err.code() == ErrorCode::WANT_WRITE => {
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::WANT_READ => {
                if let Err(register_err) = this.register_interest_for_read(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::ZERO_RETURN => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(Err(ssl_error_to_io(err))),
        }
    }
}

#[cfg(feature = "async-asupersync")]
impl FuturesAsyncRead for AsyncSslStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.ssl_read(buf) {
            Ok(read) => Poll::Ready(Ok(read)),
            Err(err) if err.code() == ErrorCode::ZERO_RETURN => Poll::Ready(Ok(0)),
            Err(err) if err.code() == ErrorCode::WANT_READ => {
                if let Err(register_err) = this.register_interest_for_read(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::WANT_WRITE => {
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(ssl_error_to_io(err))),
        }
    }
}

#[cfg(feature = "async-asupersync")]
impl FuturesAsyncWrite for AsyncSslStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.ssl_write(buf) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(err) if err.code() == ErrorCode::WANT_WRITE => {
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::WANT_READ => {
                if let Err(register_err) = this.register_interest_for_read(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::ZERO_RETURN => Poll::Ready(Err(
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "TLS session closed"),
            )),
            Err(err) => Poll::Ready(Err(ssl_error_to_io(err))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.flush() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(err) = this.ensure_nonblocking() {
            return Poll::Ready(Err(err));
        }
        match this.s.shutdown() {
            Ok(ShutdownResult::Received | ShutdownResult::Sent) => Poll::Ready(Ok(())),
            Err(err) if err.code() == ErrorCode::WANT_WRITE => {
                if let Err(register_err) = this.register_interest_for_write(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::WANT_READ => {
                if let Err(register_err) = this.register_interest_for_read(cx) {
                    return Poll::Ready(Err(register_err));
                }
                Poll::Pending
            }
            Err(err) if err.code() == ErrorCode::ZERO_RETURN => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(Err(ssl_error_to_io(err))),
        }
    }
}
