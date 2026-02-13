use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream as StreamImpl;
#[cfg(windows)]
use std::os::windows::io::{
    AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, IntoRawSocket, RawSocket,
};
use std::path::Path;
#[cfg(windows)]
use uds_windows::UnixStream as StreamImpl;

// Both async-io and async-asupersync may be enabled simultaneously due to Cargo
// workspace feature unification. When both are active, asupersync takes priority
// but the async-io IoSafe impl is kept since it's harmless.

#[cfg(feature = "async-asupersync")]
#[allow(dead_code)]
struct _AsupersyncDep(asupersync::io::IoNotAvailable);

#[cfg(unix)]
use std::os::unix::net::UnixListener as ListenerImpl;
#[cfg(windows)]
use uds_windows::UnixListener as ListenerImpl;

#[cfg(unix)]
use std::os::unix::net::SocketAddr;
#[cfg(windows)]
use uds_windows::SocketAddr;

/// This wrapper makes UnixStream IoSafe on all platforms.
/// This isn't strictly needed on unix, because async-io
/// includes an impl for the std UnixStream, but on Windows
/// the uds_windows crate doesn't have an impl.
/// Here we define it for all platforms in the interest of
/// minimizing platform differences.
#[derive(Debug)]
pub struct UnixStream(StreamImpl);

#[cfg(unix)]
impl AsFd for UnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
#[cfg(unix)]
impl IntoRawFd for UnixStream {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}
#[cfg(unix)]
impl FromRawFd for UnixStream {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixStream {
        UnixStream(StreamImpl::from_raw_fd(fd))
    }
}
#[cfg(unix)]
impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(windows)]
impl IntoRawSocket for UnixStream {
    fn into_raw_socket(self) -> RawSocket {
        self.0.into_raw_socket()
    }
}
#[cfg(windows)]
impl AsRawSocket for UnixStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.0.as_raw_socket()
    }
}
#[cfg(windows)]
impl AsSocket for UnixStream {
    fn as_socket(&self) -> BorrowedSocket {
        self.0.as_socket()
    }
}
#[cfg(windows)]
impl FromRawSocket for UnixStream {
    unsafe fn from_raw_socket(socket: RawSocket) -> UnixStream {
        UnixStream(StreamImpl::from_raw_socket(socket))
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.0.read(buf)
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.0.flush()
    }
}

#[cfg(feature = "async-io")]
unsafe impl async_io::IoSafe for UnixStream {}

impl UnixStream {
    pub fn connect<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(StreamImpl::connect(path)?))
    }
}

impl std::ops::Deref for UnixStream {
    type Target = StreamImpl;
    fn deref(&self) -> &StreamImpl {
        &self.0
    }
}

impl std::ops::DerefMut for UnixStream {
    fn deref_mut(&mut self) -> &mut StreamImpl {
        &mut self.0
    }
}

pub struct UnixListener(ListenerImpl);

impl UnixListener {
    pub fn bind<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(ListenerImpl::bind(path)?))
    }

    pub fn accept(&self) -> std::io::Result<(UnixStream, SocketAddr)> {
        let (stream, addr) = self.0.accept()?;
        Ok((UnixStream(stream), addr))
    }

    pub fn incoming(&self) -> impl Iterator<Item = std::io::Result<UnixStream>> + '_ {
        self.0.incoming().map(|r| r.map(UnixStream))
    }
}

impl std::ops::Deref for UnixListener {
    type Target = ListenerImpl;
    fn deref(&self) -> &ListenerImpl {
        &self.0
    }
}

impl std::ops::DerefMut for UnixListener {
    fn deref_mut(&mut self) -> &mut ListenerImpl {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_socket_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!(
            "frankenterm_uds_test_{}_{}",
            name,
            std::process::id()
        ))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    // ── UnixListener ───────────────────────────────────────────

    #[test]
    fn listener_bind_creates_socket() {
        let path = temp_socket_path("bind");
        cleanup(&path);
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(path.exists());
        cleanup(&path);
    }

    #[test]
    fn listener_bind_to_invalid_path_fails() {
        let result = UnixListener::bind("/nonexistent/dir/socket.sock");
        assert!(result.is_err());
    }

    // ── Connect + Accept ───────────────────────────────────────

    #[test]
    fn stream_connect_and_accept() {
        let path = temp_socket_path("connect");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server_stream, _addr) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        drop(server_stream);
        cleanup(&path);
    }

    #[test]
    fn connect_to_nonexistent_socket_fails() {
        let path = temp_socket_path("noexist");
        cleanup(&path);
        let result = UnixStream::connect(&path);
        assert!(result.is_err());
    }

    // ── Read / Write ───────────────────────────────────────────

    #[test]
    fn read_write_roundtrip() {
        let path = temp_socket_path("rw");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"hello uds").unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut server_stream, _addr) = listener.accept().unwrap();
        client.join().unwrap();

        let mut buf = [0u8; 64];
        let n = server_stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello uds");
        cleanup(&path);
    }

    #[test]
    fn bidirectional_communication() {
        let path = temp_socket_path("bidir");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"ping").unwrap();
                stream.flush().unwrap();
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = server_stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        server_stream.write_all(b"pong").unwrap();
        server_stream.flush().unwrap();

        let response = client.join().unwrap();
        assert_eq!(response, "pong");
        cleanup(&path);
    }

    #[test]
    fn write_empty_data() {
        let path = temp_socket_path("empty");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                let n = stream.write(b"").unwrap();
                assert_eq!(n, 0);
            }
        });

        let (_server_stream, _) = listener.accept().unwrap();
        client.join().unwrap();
        cleanup(&path);
    }

    // ── Deref ──────────────────────────────────────────────────

    #[test]
    fn unix_stream_deref_exposes_inner() {
        let path = temp_socket_path("deref_s");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server_stream, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        // Deref gives access to inner StreamImpl methods
        let _ = server_stream.set_nonblocking(true);
        cleanup(&path);
    }

    #[test]
    fn unix_listener_deref_exposes_inner() {
        let path = temp_socket_path("deref_l");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        // Deref gives access to ListenerImpl; set_nonblocking is on the inner type
        listener.set_nonblocking(true).unwrap();
        cleanup(&path);
    }

    // ── Fd operations (unix only) ──────────────────────────────

    #[cfg(unix)]
    #[test]
    fn as_raw_fd_returns_valid_fd() {
        let path = temp_socket_path("rawfd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server_stream, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        let fd = server_stream.as_raw_fd();
        assert!(fd >= 0);
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn as_fd_returns_borrowed_fd() {
        let path = temp_socket_path("borrowfd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server_stream, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        let _borrowed: BorrowedFd<'_> = server_stream.as_fd();
        cleanup(&path);
    }

    // ── incoming iterator ──────────────────────────────────────

    #[test]
    fn incoming_yields_connections() {
        let path = temp_socket_path("incoming");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        // Connect a client
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        // Give client time to connect
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut count = 0;
        for result in listener.incoming() {
            match result {
                Ok(_stream) => {
                    count += 1;
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{}", format!("unexpected error: {e}")),
            }
        }
        assert_eq!(count, 1);
        client.join().unwrap();
        cleanup(&path);
    }

    // ── Debug ──────────────────────────────────────────────────

    #[test]
    fn unix_stream_is_debug() {
        let path = temp_socket_path("debug");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server_stream, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        let debug = format!("{server_stream:?}");
        assert!(!debug.is_empty());
        cleanup(&path);
    }

    // ── Multiple clients ───────────────────────────────────────

    #[test]
    fn multiple_clients_can_connect() {
        let path = temp_socket_path("multi");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let path2 = path.clone();
        let c1 = std::thread::spawn({
            let p = path2.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let c2 = std::thread::spawn({
            let p = path2;
            move || UnixStream::connect(&p).unwrap()
        });

        let (mut s1, _) = listener.accept().unwrap();
        let (mut s2, _) = listener.accept().unwrap();
        let _c1 = c1.join().unwrap();
        let _c2 = c2.join().unwrap();

        s1.write_all(b"one").unwrap();
        s2.write_all(b"two").unwrap();
        cleanup(&path);
    }
}
