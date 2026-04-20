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
use std::sync::Mutex;
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
        }
    }

    #[cfg(feature = "async-asupersync")]
    pub async fn wait_for_readable(&self) -> std::io::Result<()> {
        let mut armed = false;
        poll_fn(|cx| {
            if armed {
                return Poll::Ready(Ok(()));
            }
            self.register_interest_for_read(cx)?;
            armed = true;
            Poll::Pending
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
    fn as_socket(&self) -> std::os::windows::io::BorrowedSocket {
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

    fn lock_registration(
        &self,
    ) -> std::io::Result<std::sync::MutexGuard<'_, Option<IoRegistration>>> {
        self.registration
            .lock()
            .map_err(|_| std::io::Error::other("async SSL registration lock poisoned"))
    }

    fn register_interest_for_read(&self, cx: &Context<'_>) -> std::io::Result<()> {
        self.register_interest(cx, Interest::READABLE)
    }

    fn register_interest_for_write(&self, cx: &Context<'_>) -> std::io::Result<()> {
        self.register_interest(cx, Interest::WRITABLE)
    }

    fn register_interest(&self, cx: &Context<'_>, interest: Interest) -> std::io::Result<()> {
        self.ensure_nonblocking()?;

        let mut registration = self.lock_registration()?;
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
