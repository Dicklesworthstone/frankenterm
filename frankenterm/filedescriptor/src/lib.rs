//! The purpose of this crate is to make it a bit more ergonomic for portable
//! applications that need to work with the platform level `RawFd` and
//! `RawHandle` types.
//!
//! Rather than conditionally using `RawFd` and `RawHandle`, the `FileDescriptor`
//! type can be used to manage ownership, duplicate, read and write.
//!
//! ## FileDescriptor
//!
//! This is a bit of a contrived example, but demonstrates how to avoid
//! the conditional code that would otherwise be required to deal with
//! calling `as_raw_fd` and `as_raw_handle`:
//!
//! ```
//! use filedescriptor::{FileDescriptor, FromRawFileDescriptor, Result};
//! use std::io::Write;
//!
//! fn get_stdout() -> Result<FileDescriptor> {
//!   let stdout = std::io::stdout();
//!   let handle = stdout.lock();
//!   FileDescriptor::dup(&handle)
//! }
//!
//! fn print_something() -> Result<()> {
//!    get_stdout()?.write(b"hello")?;
//!    Ok(())
//! }
//! ```
//!
//! ## Pipe
//! The `Pipe` type makes it more convenient to create a pipe and manage
//! the lifetime of both the read and write ends of that pipe.
//!
//! ```
//! use filedescriptor::{Pipe, Error};
//! use std::io::{Read, Write};
//!
//! let mut pipe = Pipe::new()?;
//! pipe.write.write(b"hello")?;
//! drop(pipe.write);
//!
//! let mut s = String::new();
//! pipe.read.read_to_string(&mut s)?;
//! assert_eq!(s, "hello");
//! # Ok::<(), Error>(())
//! ```
//!
//! ## Socketpair
//! The `socketpair` function returns a pair of connected `SOCK_STREAM`
//! sockets and functions both on posix and windows systems.
//!
//! ```
//! use std::io::{Read, Write};
//! use filedescriptor::Error;
//!
//! let (mut a, mut b) = filedescriptor::socketpair()?;
//! a.write(b"hello")?;
//! drop(a);
//!
//! let mut s = String::new();
//! b.read_to_string(&mut s)?;
//! assert_eq!(s, "hello");
//! # Ok::<(), Error>(())
//! ```
//!
//! ## Polling
//! The `mio` crate offers powerful and scalable IO multiplexing, but there
//! are some situations where `mio` doesn't fit.  The `filedescriptor` crate
//! offers a `poll(2)` compatible interface suitable for testing the readiness
//! of a set of file descriptors.  On unix systems this is a very thin wrapper
//! around `poll(2)`, except on macOS where it is actually a wrapper around
//! the `select(2)` interface.  On Windows systems the winsock `WSAPoll`
//! function is used instead.
//!
//! ```
//! use filedescriptor::*;
//! use std::time::Duration;
//! use std::io::{Read, Write};
//!
//! let (mut a, mut b) = filedescriptor::socketpair()?;
//! let mut poll_array = [pollfd {
//!    fd: a.as_socket_descriptor(),
//!    events: POLLIN,
//!    revents: 0
//! }];
//! // sleeps for 20 milliseconds because `a` is not yet ready
//! assert_eq!(poll(&mut poll_array, Some(Duration::from_millis(20)))?, 0);
//!
//! b.write(b"hello")?;
//!
//! // Now a is ready for read
//! assert_eq!(poll(&mut poll_array, Some(Duration::from_millis(20)))?, 1);
//!
//! # Ok::<(), Error>(())
//! ```

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use crate::unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use crate::windows::*;

use thiserror::Error;
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("failed to create a pipe")]
    Pipe(#[source] std::io::Error),
    #[error("failed to create a socketpair")]
    Socketpair(#[source] std::io::Error),
    #[error("failed to create a socket")]
    Socket(#[source] std::io::Error),
    #[error("failed to bind a socket")]
    Bind(#[source] std::io::Error),
    #[error("failed to fetch socket name")]
    Getsockname(#[source] std::io::Error),
    #[error("failed to set socket to listen mode")]
    Listen(#[source] std::io::Error),
    #[error("failed to connect socket")]
    Connect(#[source] std::io::Error),
    #[error("failed to accept socket")]
    Accept(#[source] std::io::Error),
    #[error("fcntl read failed")]
    Fcntl(#[source] std::io::Error),
    #[error("failed to set cloexec")]
    Cloexec(#[source] std::io::Error),
    #[error("failed to change non-blocking mode")]
    FionBio(#[source] std::io::Error),
    #[error("poll failed")]
    Poll(#[source] std::io::Error),
    #[error("dup of fd {fd} failed")]
    Dup { fd: i64, source: std::io::Error },
    #[error("dup of fd {src_fd} to fd {dest_fd} failed")]
    Dup2 {
        src_fd: i64,
        dest_fd: i64,
        source: std::io::Error,
    },
    #[error("Illegal fd value {0}")]
    IllegalFdValue(i64),
    #[error("fd value {0} too large to use with select(2)")]
    FdValueOutsideFdSetSize(i64),
    #[error("Only socket descriptors can change their non-blocking mode on Windows")]
    OnlySocketsNonBlocking,
    #[error("SetStdHandle failed")]
    SetStdHandle(#[source] std::io::Error),

    #[error("IoError")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// `AsRawFileDescriptor` is a platform independent trait for returning
/// a non-owning reference to the underlying platform file descriptor
/// type.
pub trait AsRawFileDescriptor {
    fn as_raw_file_descriptor(&self) -> RawFileDescriptor;
}

/// `IntoRawFileDescriptor` is a platform independent trait for converting
/// an instance into the underlying platform file descriptor type.
pub trait IntoRawFileDescriptor {
    fn into_raw_file_descriptor(self) -> RawFileDescriptor;
}

/// `FromRawFileDescriptor` is a platform independent trait for creating
/// an instance from the underlying platform file descriptor type.
/// Because the platform file descriptor type has no inherent ownership
/// management, the `from_raw_file_descriptor` function is marked as unsafe
/// to indicate that care must be taken by the caller to ensure that it
/// is used appropriately.
pub trait FromRawFileDescriptor {
    /// Construct `Self` from a raw file descriptor, taking ownership.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `fd` is a valid, open file descriptor and
    /// that ownership is transferred to the returned value. After calling
    /// this function, the caller must not close or otherwise use `fd`
    /// independently of the returned value.
    unsafe fn from_raw_file_descriptor(fd: RawFileDescriptor) -> Self;
}

pub trait AsRawSocketDescriptor {
    fn as_socket_descriptor(&self) -> SocketDescriptor;
}
pub trait IntoRawSocketDescriptor {
    fn into_socket_descriptor(self) -> SocketDescriptor;
}
pub trait FromRawSocketDescriptor {
    /// Construct `Self` from a raw socket descriptor, taking ownership.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `fd` is a valid, open socket descriptor and
    /// that ownership is transferred to the returned value. After calling
    /// this function, the caller must not close or otherwise use `fd`
    /// independently of the returned value.
    unsafe fn from_socket_descriptor(fd: SocketDescriptor) -> Self;
}

/// `OwnedHandle` allows managing the lifetime of the platform `RawFileDescriptor`
/// type.  It is exposed in the interface of this crate primarily for convenience
/// on Windows where the system handle type is used for a variety of objects
/// that don't support reading and writing.
#[derive(Debug)]
pub struct OwnedHandle {
    handle: RawFileDescriptor,
    handle_type: HandleType,
}

impl OwnedHandle {
    /// Create a new handle from some object that is convertible into
    /// the system `RawFileDescriptor` type.  This consumes the parameter
    /// and replaces it with an `OwnedHandle` instance.
    pub fn new<F: IntoRawFileDescriptor>(f: F) -> Self {
        let handle = f.into_raw_file_descriptor();
        Self {
            handle,
            handle_type: Self::probe_handle_type(handle),
        }
    }

    /// Attempt to duplicate the underlying handle and return an
    /// `OwnedHandle` wrapped around the duplicate.  Since the duplication
    /// requires kernel resources that may not be available, this is a
    /// potentially fallible operation.
    /// The returned handle has a separate lifetime from the source, but
    /// references the same object at the kernel level.
    pub fn try_clone(&self) -> Result<Self> {
        Self::dup_impl(self, self.handle_type)
    }

    /// Attempt to duplicate the underlying handle from an object that is
    /// representable as the system `RawFileDescriptor` type and return an
    /// `OwnedHandle` wrapped around the duplicate.  Since the duplication
    /// requires kernel resources that may not be available, this is a
    /// potentially fallible operation.
    /// The returned handle has a separate lifetime from the source, but
    /// references the same object at the kernel level.
    pub fn dup<F: AsRawFileDescriptor>(f: &F) -> Result<Self> {
        Self::dup_impl(f, Default::default())
    }
}

/// `FileDescriptor` is a thin wrapper on top of the `OwnedHandle` type that
/// exposes the ability to Read and Write to the platform `RawFileDescriptor`.
///
/// This is a bit of a contrived example, but demonstrates how to avoid
/// the conditional code that would otherwise be required to deal with
/// calling `as_raw_fd` and `as_raw_handle`:
///
/// ```
/// use filedescriptor::{FileDescriptor, FromRawFileDescriptor, Result};
/// use std::io::Write;
///
/// fn get_stdout() -> Result<FileDescriptor> {
///   let stdout = std::io::stdout();
///   let handle = stdout.lock();
///   FileDescriptor::dup(&handle)
/// }
///
/// fn print_something() -> Result<()> {
///    get_stdout()?.write(b"hello")?;
///    Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct FileDescriptor {
    handle: OwnedHandle,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StdioDescriptor {
    Stdin,
    Stdout,
    Stderr,
}

impl FileDescriptor {
    /// Create a new descriptor from some object that is convertible into
    /// the system `RawFileDescriptor` type.  This consumes the parameter
    /// and replaces it with a `FileDescriptor` instance.
    pub fn new<F: IntoRawFileDescriptor>(f: F) -> Self {
        let handle = OwnedHandle::new(f);
        Self { handle }
    }

    /// Attempt to duplicate the underlying handle from an object that is
    /// representable as the system `RawFileDescriptor` type and return a
    /// `FileDescriptor` wrapped around the duplicate.  Since the duplication
    /// requires kernel resources that may not be available, this is a
    /// potentially fallible operation.
    /// The returned handle has a separate lifetime from the source, but
    /// references the same object at the kernel level.
    pub fn dup<F: AsRawFileDescriptor>(f: &F) -> Result<Self> {
        OwnedHandle::dup(f).map(|handle| Self { handle })
    }

    /// Attempt to duplicate the underlying handle and return a
    /// `FileDescriptor` wrapped around the duplicate.  Since the duplication
    /// requires kernel resources that may not be available, this is a
    /// potentially fallible operation.
    /// The returned handle has a separate lifetime from the source, but
    /// references the same object at the kernel level.
    pub fn try_clone(&self) -> Result<Self> {
        self.handle.try_clone().map(|handle| Self { handle })
    }

    /// A convenience method for creating a `std::process::Stdio` object
    /// to be used for eg: redirecting the stdio streams of a child
    /// process.  The `Stdio` is created using a duplicated handle so
    /// that the source handle remains alive.
    pub fn as_stdio(&self) -> Result<std::process::Stdio> {
        self.as_stdio_impl()
    }

    /// A convenience method for creating a `std::fs::File` object.
    /// The `File` is created using a duplicated handle so
    /// that the source handle remains alive.
    pub fn as_file(&self) -> Result<std::fs::File> {
        self.as_file_impl()
    }

    /// Attempt to change the non-blocking IO mode of the file descriptor.
    /// Not all kinds of file descriptor can be placed in non-blocking mode
    /// on all systems, and some file descriptors will claim to be in
    /// non-blocking mode but it will have no effect.
    /// File descriptors based on sockets are the most portable type
    /// that can be successfully made non-blocking.
    pub fn set_non_blocking(&mut self, non_blocking: bool) -> Result<()> {
        self.set_non_blocking_impl(non_blocking)
    }

    /// Attempt to redirect stdio to the underlying handle and return
    /// a `FileDescriptor` wrapped around the original stdio source.
    /// Since the redirection requires kernel resources that may not be
    /// available, this is a potentially fallible operation.
    /// Supports stdin, stdout, and stderr redirections.
    pub fn redirect_stdio<F: AsRawFileDescriptor>(f: &F, stdio: StdioDescriptor) -> Result<Self> {
        Self::redirect_stdio_impl(f, stdio)
    }
}

/// Represents the readable and writable ends of a pair of descriptors
/// connected via a kernel pipe.
///
/// ```
/// use filedescriptor::{Pipe, Error};
/// use std::io::{Read,Write};
///
/// let mut pipe = Pipe::new()?;
/// pipe.write.write(b"hello")?;
/// drop(pipe.write);
///
/// let mut s = String::new();
/// pipe.read.read_to_string(&mut s)?;
/// assert_eq!(s, "hello");
/// # Ok::<(), Error>(())
/// ```
pub struct Pipe {
    /// The readable end of the pipe
    pub read: FileDescriptor,
    /// The writable end of the pipe
    pub write: FileDescriptor,
}

use std::time::Duration;

/// Examines a set of FileDescriptors to see if some of them are ready for I/O,
/// or if certain events have occurred on them.
///
/// This uses the system native readiness checking mechanism, which on Windows
/// means that it does NOT use IOCP and that this only works with sockets on
/// Windows.  If you need IOCP then the `mio` crate is recommended for a much
/// more scalable solution.
///
/// On macOS, the `poll(2)` implementation has problems when used with eg: pty
/// descriptors, so this implementation of poll uses the `select(2)` interface
/// under the covers.  That places a limit on the maximum file descriptor value
/// that can be passed to poll.  If a file descriptor is out of range then an
/// error will returned.  This limitation could potentially be lifted in the
/// future.
///
/// On Windows, `WSAPoll` is used to implement readiness checking, which has
/// the consequence that it can only be used with sockets.
///
/// If `duration` is `None`, then `poll` will block until any of the requested
/// events are ready.  Otherwise, `duration` specifies how long to wait for
/// readiness before giving up.
///
/// The return value is the number of entries that were satisfied; `0` means
/// that none were ready after waiting for the specified duration.
///
/// The `pfd` array is mutated and the `revents` field is updated to indicate
/// which of the events were received.
pub fn poll(pfd: &mut [pollfd], duration: Option<Duration>) -> Result<usize> {
    poll_impl(pfd, duration)
}

/// Create a pair of connected sockets
///
/// This implementation creates a pair of SOCK_STREAM sockets.
pub fn socketpair() -> Result<(FileDescriptor, FileDescriptor)> {
    socketpair_impl()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    // ── Error Display ─────────────────────────────────────────

    #[test]
    fn error_pipe_display() {
        let err = Error::Pipe(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("pipe"));
    }

    #[test]
    fn error_socketpair_display() {
        let err = Error::Socketpair(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("socketpair"));
    }

    #[test]
    fn error_dup_display() {
        let err = Error::Dup {
            fd: 42,
            source: std::io::Error::from_raw_os_error(0),
        };
        let s = err.to_string();
        assert!(s.contains("42"));
    }

    #[test]
    fn error_dup2_display() {
        let err = Error::Dup2 {
            src_fd: 1,
            dest_fd: 2,
            source: std::io::Error::from_raw_os_error(0),
        };
        let s = err.to_string();
        assert!(s.contains("1"));
        assert!(s.contains("2"));
    }

    #[test]
    fn error_illegal_fd_display() {
        let err = Error::IllegalFdValue(-1);
        assert!(err.to_string().contains("-1"));
    }

    #[test]
    fn error_is_debug() {
        let err = Error::OnlySocketsNonBlocking;
        let debug = format!("{err:?}");
        assert!(!debug.is_empty());
    }

    // ── StdioDescriptor ───────────────────────────────────────

    #[test]
    fn stdio_descriptor_debug() {
        assert_eq!(format!("{:?}", StdioDescriptor::Stdin), "Stdin");
        assert_eq!(format!("{:?}", StdioDescriptor::Stdout), "Stdout");
        assert_eq!(format!("{:?}", StdioDescriptor::Stderr), "Stderr");
    }

    #[test]
    fn stdio_descriptor_clone_eq() {
        let a = StdioDescriptor::Stdin;
        let b = a;
        assert_eq!(a, b);
    }

    // ── Pipe ──────────────────────────────────────────────────

    #[test]
    fn pipe_new_succeeds() {
        let _pipe = Pipe::new().unwrap();
    }

    #[test]
    fn pipe_write_then_read() {
        let mut pipe = Pipe::new().unwrap();
        pipe.write.write_all(b"hello pipe").unwrap();
        drop(pipe.write);

        let mut buf = String::new();
        pipe.read.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello pipe");
    }

    #[test]
    fn pipe_empty_write() {
        let mut pipe = Pipe::new().unwrap();
        let n = pipe.write.write(b"").unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn pipe_large_write() {
        let mut pipe = Pipe::new().unwrap();
        let data = vec![0xABu8; 4096];
        pipe.write.write_all(&data).unwrap();
        drop(pipe.write);

        let mut buf = Vec::new();
        pipe.read.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 4096);
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    // ── Socketpair ────────────────────────────────────────────

    #[test]
    fn socketpair_succeeds() {
        let (_a, _b) = socketpair().unwrap();
    }

    #[test]
    fn socketpair_bidirectional() {
        let (mut a, mut b) = socketpair().unwrap();
        a.write_all(b"ping").unwrap();

        let mut buf = [0u8; 16];
        let n = b.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");

        b.write_all(b"pong").unwrap();
        let n = a.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"pong");
    }

    #[test]
    fn socketpair_close_one_end() {
        let (mut a, b) = socketpair().unwrap();
        drop(b);
        // Reading from a when b is closed should return 0 (EOF)
        let mut buf = [0u8; 16];
        let n = a.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    // ── FileDescriptor dup / try_clone ────────────────────────

    #[test]
    fn file_descriptor_dup_stdout() {
        let stdout = std::io::stdout();
        let handle = stdout.lock();
        let fd = FileDescriptor::dup(&handle).unwrap();
        assert!(fd.as_raw_file_descriptor() >= 0);
    }

    #[test]
    fn file_descriptor_try_clone() {
        let pipe = Pipe::new().unwrap();
        let clone = pipe.read.try_clone().unwrap();
        assert_ne!(
            pipe.read.as_raw_file_descriptor(),
            clone.as_raw_file_descriptor()
        );
    }

    #[test]
    fn file_descriptor_is_debug() {
        let pipe = Pipe::new().unwrap();
        let debug = format!("{:?}", pipe.read);
        assert!(!debug.is_empty());
    }

    // ── FileDescriptor set_non_blocking ───────────────────────

    #[test]
    fn socketpair_set_non_blocking() {
        let (mut a, _b) = socketpair().unwrap();
        a.set_non_blocking(true).unwrap();
        // Reading should fail with WouldBlock
        let mut buf = [0u8; 16];
        let result = a.read(&mut buf);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn set_non_blocking_then_blocking() {
        let (mut a, mut b) = socketpair().unwrap();
        a.set_non_blocking(true).unwrap();
        a.set_non_blocking(false).unwrap();
        // After setting back to blocking, a write/read should work
        b.write_all(b"test").unwrap();
        let mut buf = [0u8; 16];
        let n = a.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"test");
    }

    // ── FileDescriptor as_stdio / as_file ─────────────────────

    #[test]
    fn as_stdio_succeeds() {
        let pipe = Pipe::new().unwrap();
        let _stdio = pipe.read.as_stdio().unwrap();
    }

    #[test]
    fn as_file_succeeds() {
        let pipe = Pipe::new().unwrap();
        let _file = pipe.read.as_file().unwrap();
    }

    #[test]
    fn as_file_read_through() {
        let mut pipe = Pipe::new().unwrap();
        pipe.write.write_all(b"via file").unwrap();
        drop(pipe.write);

        let mut file = pipe.read.as_file().unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "via file");
    }

    // ── OwnedHandle ──────────────────────────────────────────

    #[test]
    fn owned_handle_new_and_debug() {
        let pipe = Pipe::new().unwrap();
        let fd = pipe.read.into_raw_file_descriptor();
        let handle = OwnedHandle::new(unsafe { FileDescriptor::from_raw_file_descriptor(fd) });
        let debug = format!("{handle:?}");
        assert!(debug.contains("OwnedHandle"));
    }

    #[test]
    fn owned_handle_try_clone() {
        let pipe = Pipe::new().unwrap();
        let fd = pipe.read.as_raw_file_descriptor();
        let handle = OwnedHandle::dup(&pipe.read).unwrap();
        let cloned = handle.try_clone().unwrap();
        assert_ne!(
            handle.as_raw_file_descriptor(),
            cloned.as_raw_file_descriptor()
        );
        // Both should be valid (not the original fd necessarily)
        assert!(handle.as_raw_file_descriptor() >= 0);
        assert!(cloned.as_raw_file_descriptor() >= 0);
        let _ = fd;
    }

    // ── Poll ──────────────────────────────────────────────────

    #[test]
    fn poll_timeout_with_no_ready_fds() {
        let (a, _b) = socketpair().unwrap();
        let mut pfd = [pollfd {
            fd: a.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        }];
        let n = poll(&mut pfd, Some(Duration::from_millis(10))).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn poll_detects_readable_fd() {
        let (a, mut b) = socketpair().unwrap();
        b.write_all(b"data").unwrap();

        let mut pfd = [pollfd {
            fd: a.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        }];
        let n = poll(&mut pfd, Some(Duration::from_millis(100))).unwrap();
        assert_eq!(n, 1);
        assert!(pfd[0].revents & POLLIN != 0);
    }

    #[test]
    fn poll_detects_writable_fd() {
        let (a, _b) = socketpair().unwrap();
        let mut pfd = [pollfd {
            fd: a.as_socket_descriptor(),
            events: POLLOUT,
            revents: 0,
        }];
        let n = poll(&mut pfd, Some(Duration::from_millis(100))).unwrap();
        assert!(n >= 1);
        assert!(pfd[0].revents & POLLOUT != 0);
    }

    #[test]
    fn poll_empty_array() {
        let n = poll(&mut [], Some(Duration::from_millis(10))).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn poll_multiple_fds() {
        let (a1, mut b1) = socketpair().unwrap();
        let (a2, _b2) = socketpair().unwrap();

        b1.write_all(b"ready").unwrap();

        let mut pfd = [
            pollfd {
                fd: a1.as_socket_descriptor(),
                events: POLLIN,
                revents: 0,
            },
            pollfd {
                fd: a2.as_socket_descriptor(),
                events: POLLIN,
                revents: 0,
            },
        ];
        let n = poll(&mut pfd, Some(Duration::from_millis(100))).unwrap();
        assert_eq!(n, 1);
        assert!(pfd[0].revents & POLLIN != 0);
        assert_eq!(pfd[1].revents & POLLIN, 0);
    }

    // ── IntoRawFileDescriptor / FromRawFileDescriptor ─────────

    #[test]
    fn into_and_from_raw_fd_roundtrip() {
        let pipe = Pipe::new().unwrap();
        let raw = pipe.read.into_raw_file_descriptor();
        assert!(raw >= 0);
        // Re-wrap to ensure cleanup
        let _fd = unsafe { FileDescriptor::from_raw_file_descriptor(raw) };
    }

    // ── Trait implementations ─────────────────────────────────

    #[test]
    fn as_raw_file_descriptor_trait() {
        let pipe = Pipe::new().unwrap();
        let raw = pipe.read.as_raw_file_descriptor();
        assert!(raw >= 0);
    }

    #[test]
    fn as_socket_descriptor_trait() {
        let (a, _b) = socketpair().unwrap();
        let sd = a.as_socket_descriptor();
        assert!(sd >= 0);
    }

    // ── Additional Error variant tests ───────────────────────

    #[test]
    fn error_socket_display() {
        let err = Error::Socket(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("socket"));
    }

    #[test]
    fn error_bind_display() {
        let err = Error::Bind(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("bind"));
    }

    #[test]
    fn error_getsockname_display() {
        let err = Error::Getsockname(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("socket name"));
    }

    #[test]
    fn error_listen_display() {
        let err = Error::Listen(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("listen"));
    }

    #[test]
    fn error_connect_display() {
        let err = Error::Connect(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("connect"));
    }

    #[test]
    fn error_accept_display() {
        let err = Error::Accept(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("accept"));
    }

    #[test]
    fn error_fcntl_display() {
        let err = Error::Fcntl(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("fcntl"));
    }

    #[test]
    fn error_cloexec_display() {
        let err = Error::Cloexec(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("cloexec"));
    }

    #[test]
    fn error_fionbio_display() {
        let err = Error::FionBio(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("non-blocking"));
    }

    #[test]
    fn error_poll_display() {
        let err = Error::Poll(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("poll"));
    }

    #[test]
    fn error_fd_outside_fdset_display() {
        let err = Error::FdValueOutsideFdSetSize(99999);
        let s = err.to_string();
        assert!(s.contains("99999"));
    }

    #[test]
    fn error_set_std_handle_display() {
        let err = Error::SetStdHandle(std::io::Error::from_raw_os_error(0));
        assert!(err.to_string().contains("SetStdHandle"));
    }

    #[test]
    fn error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: Error = io_err.into();
        assert!(err.to_string().contains("IoError"));
    }

    // ── Additional Pipe tests ────────────────────────────────

    #[test]
    fn pipe_multiple_writes() {
        let mut pipe = Pipe::new().unwrap();
        pipe.write.write_all(b"hello ").unwrap();
        pipe.write.write_all(b"world").unwrap();
        drop(pipe.write);

        let mut buf = String::new();
        pipe.read.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello world");
    }

    #[test]
    fn pipe_binary_data() {
        let mut pipe = Pipe::new().unwrap();
        let data: Vec<u8> = (0..=255).collect();
        pipe.write.write_all(&data).unwrap();
        drop(pipe.write);

        let mut buf = Vec::new();
        pipe.read.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn pipe_read_returns_eof_after_writer_closed() {
        let pipe = Pipe::new().unwrap();
        drop(pipe.write);
        let mut buf = [0u8; 16];
        let mut reader = pipe.read;
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn pipe_read_fd_differs_from_write_fd() {
        let pipe = Pipe::new().unwrap();
        assert_ne!(
            pipe.read.as_raw_file_descriptor(),
            pipe.write.as_raw_file_descriptor()
        );
    }

    // ── Additional Socketpair tests ──────────────────────────

    #[test]
    fn socketpair_large_transfer() {
        let (mut a, mut b) = socketpair().unwrap();
        let data = vec![0x42u8; 8192];
        a.write_all(&data).unwrap();
        drop(a);

        let mut buf = Vec::new();
        b.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 8192);
        assert!(buf.iter().all(|&b| b == 0x42));
    }

    #[test]
    fn socketpair_multiple_messages() {
        let (mut a, mut b) = socketpair().unwrap();
        for i in 0..10u8 {
            a.write_all(&[i]).unwrap();
        }
        for i in 0..10u8 {
            let mut buf = [0u8; 1];
            b.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], i);
        }
    }

    #[test]
    fn socketpair_fds_are_distinct() {
        let (a, b) = socketpair().unwrap();
        assert_ne!(
            a.as_raw_file_descriptor(),
            b.as_raw_file_descriptor()
        );
    }

    // ── Additional FileDescriptor tests ──────────────────────

    #[test]
    fn file_descriptor_dup_stderr() {
        let stderr = std::io::stderr();
        let handle = stderr.lock();
        let fd = FileDescriptor::dup(&handle).unwrap();
        assert!(fd.as_raw_file_descriptor() >= 0);
    }

    #[test]
    fn file_descriptor_multiple_try_clones() {
        let pipe = Pipe::new().unwrap();
        let clone1 = pipe.read.try_clone().unwrap();
        let clone2 = pipe.read.try_clone().unwrap();
        let clone3 = clone1.try_clone().unwrap();
        // All should have distinct fds
        let arr = [
            pipe.read.as_raw_file_descriptor(),
            clone1.as_raw_file_descriptor(),
            clone2.as_raw_file_descriptor(),
            clone3.as_raw_file_descriptor(),
        ];
        let fds: std::collections::HashSet<_> = arr.iter().collect();
        assert_eq!(fds.len(), 4);
    }

    #[test]
    fn file_descriptor_write_debug() {
        let pipe = Pipe::new().unwrap();
        let debug = format!("{:?}", pipe.write);
        assert!(debug.contains("FileDescriptor"));
    }

    // ── Poll edge cases ──────────────────────────────────────

    #[test]
    fn poll_detects_hangup() {
        let (a, b) = socketpair().unwrap();
        drop(b);
        let mut pfd = [pollfd {
            fd: a.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        }];
        let n = poll(&mut pfd, Some(Duration::from_millis(50))).unwrap();
        assert!(n >= 1);
        // On hangup, POLLIN or POLLHUP should be set
        assert!(pfd[0].revents & (POLLIN | POLLHUP) != 0);
    }

    #[test]
    fn poll_zero_timeout_returns_immediately() {
        let (a, _b) = socketpair().unwrap();
        let mut pfd = [pollfd {
            fd: a.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        }];
        let start = std::time::Instant::now();
        let n = poll(&mut pfd, Some(Duration::from_millis(0))).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(n, 0);
        // Should return almost immediately
        assert!(elapsed < Duration::from_millis(50));
    }

    #[test]
    fn poll_both_read_and_write_events() {
        let (a, mut b) = socketpair().unwrap();
        b.write_all(b"data").unwrap();

        let mut pfd = [pollfd {
            fd: a.as_socket_descriptor(),
            events: POLLIN | POLLOUT,
            revents: 0,
        }];
        let n = poll(&mut pfd, Some(Duration::from_millis(100))).unwrap();
        assert!(n >= 1);
        // Should be both readable and writable
        assert!(pfd[0].revents & POLLIN != 0);
        assert!(pfd[0].revents & POLLOUT != 0);
    }

    // ── StdioDescriptor additional tests ─────────────────────

    #[test]
    fn stdio_descriptor_all_variants_distinct() {
        assert_ne!(StdioDescriptor::Stdin, StdioDescriptor::Stdout);
        assert_ne!(StdioDescriptor::Stdout, StdioDescriptor::Stderr);
        assert_ne!(StdioDescriptor::Stdin, StdioDescriptor::Stderr);
    }

    #[test]
    fn stdio_descriptor_copy() {
        let a = StdioDescriptor::Stdout;
        let b = a;
        let c = a; // Copy
        assert_eq!(b, c);
    }

    // ── OwnedHandle additional tests ─────────────────────────

    #[test]
    fn owned_handle_dup_from_pipe() {
        let pipe = Pipe::new().unwrap();
        let handle = OwnedHandle::dup(&pipe.read).unwrap();
        assert!(handle.as_raw_file_descriptor() >= 0);
        assert_ne!(
            handle.as_raw_file_descriptor(),
            pipe.read.as_raw_file_descriptor()
        );
    }

    // ── Flush behavior ───────────────────────────────────────

    #[test]
    fn file_descriptor_flush_succeeds() {
        let mut pipe = Pipe::new().unwrap();
        pipe.write.write_all(b"data").unwrap();
        // flush is a no-op on unix but should succeed
        pipe.write.flush().unwrap();
    }
}
